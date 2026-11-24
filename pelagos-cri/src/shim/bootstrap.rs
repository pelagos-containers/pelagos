//! Legacy shim v2 bootstrap: `containerd-shim-<name>-v2 start|delete`.
//!
//! Contract (containerd `core/runtime/v2/README.md`): the shim is executed
//! with `-namespace -id -address -publish-binary` flags and the bundle
//! directory as cwd. `start` prints the shim's ttrpc address on stdout (plain
//! text, or JSON `{"version":2,"address":...,"protocol":"ttrpc"}`). `delete`
//! cleans up a shim that can no longer be reached.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use tokio::process::Command;

use crate::scope;

/// Task API version a shim speaks when it does not say (legacy plain reply).
pub const DEFAULT_SHIM_VERSION: u32 = 2;

#[derive(Debug)]
pub enum BootstrapError {
    Spawn(std::io::Error),
    Timeout,
    /// The shim exited non-zero. Carries the exit code and stderr.
    Failed(Option<i32>, String),
    /// `start` succeeded but printed no usable address.
    BadAddress(String),
}

impl std::fmt::Display for BootstrapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(e) => write!(f, "failed to execute shim: {e}"),
            Self::Timeout => write!(f, "shim bootstrap timed out"),
            Self::Failed(code, err) => write!(f, "shim exited with {code:?}: {}", err.trim()),
            Self::BadAddress(s) => write!(f, "shim printed no usable address: {s:?}"),
        }
    }
}

impl std::error::Error for BootstrapError {}

/// Address and protocol reported by `start`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShimAddress {
    /// e.g. `unix:///run/containerd/s/abc` or a bare socket path.
    pub address: String,
    pub version: u32,
    pub protocol: String,
}

#[derive(Debug, Clone)]
pub struct StartParams {
    /// Shim executable (`containerd-shim-kata-v2`, absolute path or on PATH).
    pub binary: String,
    pub namespace: String,
    pub id: String,
    /// Value of the `-address` flag: the containerd-style endpoint address.
    pub address: String,
    /// Events sink socket, exported to the shim as `TTRPC_ADDRESS`. Empty means
    /// "do not set it" (the shim then only logs events). Distinct from
    /// `address`: containerd serves its gRPC API at one path and the ttrpc
    /// events service at another.
    pub ttrpc_address: String,
    /// Value of `-publish-binary`; must be non-empty (Kata validates it).
    pub publish_binary: String,
    /// Bundle directory; becomes the shim's cwd.
    pub bundle: PathBuf,
}

/// Runtime type (`io.containerd.kata.v2`) -> shim binary name
/// (`containerd-shim-kata-v2`), per containerd's naming rule.
pub fn binary_name_for_runtime_type(runtime_type: &str) -> Option<String> {
    let parts: Vec<&str> = runtime_type.split('.').collect();
    // io.containerd.<name>.<version>
    if parts.len() < 4 || parts[0] != "io" || parts[1] != "containerd" {
        return None;
    }
    let name = parts[2..parts.len() - 1].join(".");
    let ver = parts[parts.len() - 1];
    if name.is_empty() || ver.is_empty() {
        return None;
    }
    Some(format!("containerd-shim-{name}-{ver}"))
}

fn common_args(p: &StartParams) -> Vec<String> {
    vec![
        "-namespace".into(),
        p.namespace.clone(),
        "-id".into(),
        p.id.clone(),
        "-address".into(),
        p.address.clone(),
        "-publish-binary".into(),
        p.publish_binary.clone(),
    ]
}

/// Argument vector for `start`.
pub fn start_args(p: &StartParams) -> Vec<String> {
    let mut a = common_args(p);
    a.push("start".into());
    a
}

/// Argument vector for `delete`: the same flags plus `-bundle`.
pub fn delete_args(p: &StartParams) -> Vec<String> {
    let mut a = common_args(p);
    a.push("-bundle".into());
    a.push(p.bundle.to_string_lossy().into_owned());
    a.push("delete".into());
    a
}

#[derive(Deserialize)]
struct JsonReply {
    version: Option<u32>,
    address: String,
    protocol: Option<String>,
}

/// Parse the stdout of `start`: plain address or the JSON form.
pub fn parse_start_output(out: &str) -> Result<ShimAddress, BootstrapError> {
    let t = out.trim();
    if t.is_empty() {
        return Err(BootstrapError::BadAddress(out.to_string()));
    }
    if t.starts_with('{') {
        let j: JsonReply =
            serde_json::from_str(t).map_err(|_| BootstrapError::BadAddress(out.to_string()))?;
        if j.address.is_empty() {
            return Err(BootstrapError::BadAddress(out.to_string()));
        }
        return Ok(ShimAddress {
            address: j.address,
            version: j.version.unwrap_or(DEFAULT_SHIM_VERSION),
            protocol: j.protocol.unwrap_or_else(|| "ttrpc".into()),
        });
    }
    // Plain form: the address is the whole (single-line) output.
    let line = t.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        return Err(BootstrapError::BadAddress(out.to_string()));
    }
    Ok(ShimAddress {
        address: line.to_string(),
        version: DEFAULT_SHIM_VERSION,
        protocol: "ttrpc".into(),
    })
}

/// Program and argv for running a shim subcommand. With `scope_unit` the shim
/// (and the daemon it forks) is placed in its own transient systemd scope under
/// `pelagos.slice`, so it lives outside `pelagos-cri.service`: systemd's default
/// `KillMode=control-group` would otherwise kill every shim (and its VM or
/// sandbox) when the CRI restarts. Kata's shim escapes by moving itself to
/// another cgroup; Go shims such as runsc do not (#618). Same approach as the
/// native supervisors (#336, `scope.rs`).
pub(crate) fn shim_invocation(
    binary: &str,
    args: &[String],
    scope_unit: Option<&str>,
) -> (String, Vec<String>) {
    match scope_unit {
        Some(unit) => {
            let a: Vec<&str> = args.iter().map(String::as_str).collect();
            let mut argv = scope::build_scope_argv(unit, binary, &a, &[]);
            let program = argv.remove(0);
            (program, argv)
        }
        None => (binary.to_string(), args.to_vec()),
    }
}

/// Whether `start` should be wrapped in a scope: needs a live systemd and
/// root (a transient scope under the system manager is privileged).
fn use_scope() -> bool {
    // SAFETY: geteuid has no preconditions.
    scope::systemd_available() && unsafe { libc::geteuid() } == 0
}

async fn run(
    binary: &str,
    args: &[String],
    cwd: &Path,
    ttrpc_address: &str,
    timeout: Duration,
    scope_unit: Option<&str>,
) -> Result<String, BootstrapError> {
    let (program, argv) = shim_invocation(binary, args, scope_unit);
    let mut cmd = Command::new(program);
    cmd.args(argv).current_dir(cwd).env("MAX_SHIM_VERSION", "2");
    if ttrpc_address.is_empty() {
        // Never leak an inherited value that would point the shim at some
        // other client's sink.
        cmd.env_remove("TTRPC_ADDRESS");
    } else {
        cmd.env("TTRPC_ADDRESS", ttrpc_address);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    // A shim binary that was just written (installers, and our own tests) can
    // be briefly "text file busy" while another thread's fork still holds the
    // write fd; that clears within milliseconds, so retry instead of failing
    // the pod.
    let mut attempts = 0;
    let child = loop {
        match cmd.spawn() {
            Ok(c) => break c,
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && attempts < 10 => {
                attempts += 1;
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Err(e) => return Err(BootstrapError::Spawn(e)),
        }
    };
    let out = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| BootstrapError::Timeout)?
        .map_err(BootstrapError::Spawn)?;
    if !out.status.success() {
        return Err(BootstrapError::Failed(
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run `start` and return the shim's address.
pub async fn start(p: &StartParams, timeout: Duration) -> Result<ShimAddress, BootstrapError> {
    let unit = use_scope().then(|| scope::shim_unit(&p.id));
    let out = run(
        &p.binary,
        &start_args(p),
        &p.bundle,
        &p.ttrpc_address,
        timeout,
        unit.as_deref(),
    )
    .await?;
    parse_start_output(&out)
}

/// Run `delete` for an unreachable shim; returns its stdout (a `DeleteResponse`
/// in some shims, ignored by callers).
pub async fn delete(p: &StartParams, timeout: Duration) -> Result<String, BootstrapError> {
    run(
        &p.binary,
        &delete_args(p),
        &p.bundle,
        &p.ttrpc_address,
        timeout,
        None,
    )
    .await
}

#[cfg(test)]
mod tests {
    /// #618: with a scope unit the shim command is wrapped in
    /// `systemd-run --scope --slice=pelagos.slice`; without one it is run as-is.
    #[test]
    fn shim_invocation_wraps_in_a_scope_only_when_asked() {
        let args: Vec<String> = ["-namespace", "k8s.io", "-id", "p1", "start"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (prog, argv) = shim_invocation("/opt/shim", &args, None);
        assert_eq!((prog.as_str(), argv.as_slice()), ("/opt/shim", &args[..]));

        let unit = crate::scope::shim_unit("p1");
        assert_eq!(unit, "pelagos-shim-p1.scope");
        let (prog, argv) = shim_invocation("/opt/shim", &args, Some(&unit));
        assert_eq!(prog, "systemd-run");
        assert!(argv.contains(&"--scope".to_string()));
        assert!(argv.contains(&"--slice=pelagos.slice".to_string()));
        assert!(argv.contains(&format!("--unit={unit}")));
        let dd = argv.iter().position(|a| a == "--").unwrap();
        assert_eq!(&argv[dd + 1..dd + 2], ["/opt/shim"]);
        assert_eq!(&argv[dd + 2..], &args[..]);
    }

    use super::*;

    fn params() -> StartParams {
        StartParams {
            binary: "containerd-shim-kata-v2".into(),
            namespace: "k8s.io".into(),
            id: "abc".into(),
            address: "/run/pelagos-cri/containerd.sock".into(),
            ttrpc_address: "/run/pelagos-cri/events.sock".into(),
            publish_binary: "/usr/local/bin/pelagos-cri-publish".into(),
            bundle: PathBuf::from("/run/pelagos-cri/shim/k8s.io/abc"),
        }
    }

    #[test]
    fn runtime_type_to_binary() {
        assert_eq!(
            binary_name_for_runtime_type("io.containerd.kata.v2").unwrap(),
            "containerd-shim-kata-v2"
        );
        assert_eq!(
            binary_name_for_runtime_type("io.containerd.pelagos.wasm.v1").unwrap(),
            "containerd-shim-pelagos.wasm-v1"
        );
        assert!(binary_name_for_runtime_type("kata").is_none());
        assert!(binary_name_for_runtime_type("io.containerd.v2").is_none());
    }

    #[test]
    fn start_and_delete_args() {
        let a = start_args(&params());
        assert_eq!(a.last().unwrap(), "start");
        assert!(a.windows(2).any(|w| w == ["-id", "abc"]));
        assert!(a.windows(2).any(|w| w == ["-namespace", "k8s.io"]));
        let d = delete_args(&params());
        assert_eq!(d.last().unwrap(), "delete");
        assert!(d
            .windows(2)
            .any(|w| w == ["-bundle", "/run/pelagos-cri/shim/k8s.io/abc"]));
    }

    #[test]
    fn parse_plain_address() {
        let a = parse_start_output("unix:///run/containerd/s/x\n").unwrap();
        assert_eq!(a.address, "unix:///run/containerd/s/x");
        assert_eq!(a.version, 2);
        assert_eq!(a.protocol, "ttrpc");
    }

    #[test]
    fn parse_json_address() {
        let a = parse_start_output(r#"{"version":3,"address":"unix:///s","protocol":"ttrpc"}"#)
            .unwrap();
        assert_eq!(a.version, 3);
        assert_eq!(a.address, "unix:///s");
    }

    #[test]
    fn parse_rejects_empty() {
        assert!(parse_start_output("  \n").is_err());
        assert!(parse_start_output(r#"{"address":""}"#).is_err());
        assert!(parse_start_output("{garbage").is_err());
    }

    /// #618 (root + systemd only): a process the shim's `start` leaves behind
    /// lives in a `pelagos.slice` scope, not in the CRI's own cgroup, so a CRI
    /// restart (KillMode=control-group) cannot take it down.
    #[tokio::test]
    async fn shim_daemon_lives_outside_the_cri_cgroup() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0 || !crate::scope::systemd_available() {
            eprintln!("skipped: needs root and systemd");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("containerd-shim-scopetest-v2");
        std::fs::write(
            &script,
            "#!/bin/sh\nsleep 120 >/dev/null 2>&1 </dev/null &\necho $! > ./daemon.pid\necho unix:///x\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let id = format!("scopetest{}", std::process::id());
        let p = StartParams {
            binary: script.to_string_lossy().into_owned(),
            namespace: "k8s.io".into(),
            id: id.clone(),
            address: "/run/x.sock".into(),
            ttrpc_address: String::new(),
            publish_binary: "/bin/true".into(),
            bundle: tmp.path().to_path_buf(),
        };
        let addr = start(&p, Duration::from_secs(20)).await.unwrap();
        assert_eq!(addr.address, "unix:///x");
        let pid: i32 = std::fs::read_to_string(tmp.path().join("daemon.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let theirs = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
        let ours = std::fs::read_to_string("/proc/self/cgroup").unwrap();
        let unit = crate::scope::shim_unit(&id);
        crate::scope::stop_unit(&unit).await;
        // SAFETY: plain signal to a process we spawned.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        assert!(
            theirs.contains("pelagos.slice") && theirs.contains(&unit),
            "shim daemon cgroup: {theirs}"
        );
        assert_ne!(
            theirs.trim(),
            ours.trim(),
            "must not share the CRI's cgroup"
        );
    }
}
