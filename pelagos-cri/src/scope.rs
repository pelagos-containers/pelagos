//! Transient systemd-unit supervision for container/pause processes.
//!
//! Issue #336: per-container supervisors (the detached `pelagos run` watcher and
//! the sandbox pause process) were launched as plain children of pelagos-cri and
//! therefore lived in the `pelagos-cri.service` cgroup. systemd's default
//! `KillMode=control-group` means a `systemctl restart pelagos-cri` SIGTERMs (then
//! SIGKILLs) every process in that cgroup — taking the supervisors, and with them
//! the running workloads, down with the runtime. Compliant CRIs (containerd,
//! CRI-O) keep each container's supervisor in its own transient scope/service so a
//! runtime restart is transparent.
//!
//! This module launches supervisors via `systemd-run`, which asks systemd (PID 1)
//! to create the unit. The resulting processes are children of systemd under
//! `pelagos.slice`, fully outside `pelagos-cri.service`, so they survive a runtime
//! restart and are re-adopted on startup.
//!
//! Two unit kinds are used because the two supervisors have different lifecycles:
//!
//! * **scope** for `pelagos run --detach`: the foreground process forks the
//!   watcher and returns quickly, so `systemd-run --scope` returns promptly while
//!   the watcher persists in the (non-empty) scope.
//! * **service** for the pause process: it blocks forever, so it is run as a
//!   backgrounded transient service whose `MainPID` is the real pause PID.
//!
//! When systemd is unavailable (unit tests, non-systemd hosts) callers fall back to
//! launching the process directly, preserving the previous behavior.

use std::sync::OnceLock;

/// Slice that groups all pelagos supervisor units. systemd auto-creates it when
/// first referenced; an optional shipped `pelagos.slice` may set properties.
pub const SLICE: &str = "pelagos.slice";

/// Whether a given systemd handles `$VAR` / `${VAR}` references inside a transient
/// unit's (pre-split, D-Bus-supplied) ExecStart argv is **version-dependent, not a
/// constant** — confirmed live on two real hosts: systemd 259 expands them against
/// the scope's own environment (undeclared vars become an empty string); systemd 255
/// does not touch the argv at all. #483 originally "fixed" this by escaping every `$`
/// as `$$` (systemd's own literal-dollar escape), on the assumption that expansion
/// always happens. On a systemd that does *not* expand, that escape is never undone:
/// the container process (bash) receives a literal `$$` in its argv, which bash
/// itself then interprets as *its own PID* (a container's init process is PID 1 in
/// its own PID namespace) — corrupting `${BIN_PATH}` into `1{BIN_PATH}` (#543).
///
/// The version-independent fix is to never rely on which behavior a given systemd
/// has: pass the container's declared env vars to `systemd-run` itself via
/// `--setenv=KEY=VALUE`, mirroring the `--env` values pelagos-cri already passes to
/// `pelagos run`. If systemd *does* expand `$VAR` references in the argv, they now
/// resolve to the same value the container's own process would produce (a harmless,
/// idempotent no-op substitution). If it does not, the raw text passes through
/// unmodified and bash performs its own correct expansion, exactly as it always did
/// outside the systemd-run wrapper. Either way the result is correct, and the
/// container's actual command text is never rewritten.
fn setenv_args(envs: &[(String, String)]) -> Vec<String> {
    envs.iter()
        .map(|(k, v)| format!("--setenv={}={}", k, v))
        .collect()
}

/// Whether this host is running under systemd and `systemd-run` is usable.
///
/// Requires both a live systemd (`/run/systemd/system`) and `systemd-run` on PATH.
/// Cached after the first probe.
pub fn systemd_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        std::path::Path::new("/run/systemd/system").is_dir() && which_systemd_run().is_some()
    })
}

fn which_systemd_run() -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join("systemd-run");
        if cand.is_file() {
            return Some(cand.to_string_lossy().into_owned());
        }
    }
    None
}

/// Sanitize an arbitrary id into a valid systemd unit-name component.
///
/// systemd unit names allow `[A-Za-z0-9:_.\\-]`; anything else is replaced with
/// `-`. The id is truncated so the final unit name stays well within systemd's
/// 256-byte limit while remaining collision-free for our hex/`pcri-` ids.
fn sanitize(id: &str) -> String {
    let mut s: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '.' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if s.len() > 64 {
        s.truncate(64);
    }
    s
}

/// Unit name for a container watcher scope, e.g. `pelagos-ctr-pcri-abc123.scope`.
pub fn container_unit(pelagos_name: &str) -> String {
    format!("pelagos-ctr-{}.scope", sanitize(pelagos_name))
}

/// Unit name for a sandbox pause service, e.g. `pelagos-sbx-<id>.service`.
pub fn sandbox_unit(sandbox_id: &str) -> String {
    format!("pelagos-sbx-{}.service", sanitize(sandbox_id))
}

/// Build the `systemd-run --scope` argv that wraps a quick-returning command
/// whose forked descendants must outlive the runtime (the `pelagos run --detach`
/// watcher). Returns the full argv beginning with `systemd-run`.
///
/// `envs` are the container's declared environment variables (the same pairs
/// passed to `pelagos run` as `--env`), mirrored onto the wrapper via
/// `--setenv=` — see `setenv_args` for why. Order is `--setenv=...` flags first,
/// then `--`, then the wrapped command, matching `systemd-run`'s own option
/// convention of flags-before-`--`.
pub fn build_scope_argv(unit: &str, bin: &str, args: &[&str], envs: &[(String, String)]) -> Vec<String> {
    let mut v = vec![
        "systemd-run".to_string(),
        "--scope".to_string(),
        "--collect".to_string(),
        format!("--slice={}", SLICE),
        format!("--unit={}", unit),
        "--quiet".to_string(),
    ];
    v.extend(setenv_args(envs));
    v.push("--".to_string());
    v.push(bin.to_string());
    v.extend(args.iter().map(|s| s.to_string()));
    v
}

/// Build the `systemd-run` transient-service argv for a long-running, blocking
/// command (the pause process). The service is backgrounded; `systemd-run`
/// returns immediately and the real process is the unit's `MainPID`.
///
/// `KillMode=mixed` so that stopping the unit signals the main process directly,
/// and `--collect` so a failed unit is garbage-collected rather than lingering.
/// The pause process's argv is entirely pelagos-internal (no user-supplied
/// command or env references), so unlike `build_scope_argv` it needs no
/// `--setenv` mirroring.
pub fn build_service_argv(unit: &str, bin: &str, args: &[&str]) -> Vec<String> {
    let mut v = vec![
        "systemd-run".to_string(),
        "--collect".to_string(),
        format!("--slice={}", SLICE),
        format!("--unit={}", unit),
        "--property=KillMode=mixed".to_string(),
        "--quiet".to_string(),
        "--".to_string(),
        bin.to_string(),
    ];
    v.extend(args.iter().map(|s| s.to_string()));
    v
}

/// Query the `MainPID` of a transient service unit. Returns `None` if the unit is
/// unknown, inactive, or systemd reports PID 0 (not yet started / already gone).
pub async fn service_main_pid(unit: &str) -> Option<i32> {
    let out = tokio::process::Command::new("systemctl")
        .args(["show", "-p", "MainPID", "--value", unit])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let pid: i32 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    if pid > 0 {
        Some(pid)
    } else {
        None
    }
}

/// Best-effort stop of a transient unit (scope or service). Errors are ignored:
/// scopes auto-collect when empty, and a missing unit is already gone.
pub async fn stop_unit(unit: &str) {
    let _ = tokio::process::Command::new("systemctl")
        .args(["stop", unit])
        .output()
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_argv_wraps_command_under_slice_and_unit() {
        let argv = build_scope_argv(
            "pelagos-ctr-pcri-abc123.scope",
            "/usr/local/bin/pelagos",
            &["run", "--detach", "--name", "pcri-abc123"],
            &[],
        );
        assert_eq!(argv[0], "systemd-run");
        assert!(argv.contains(&"--scope".to_string()));
        assert!(argv.contains(&"--collect".to_string()));
        assert!(argv.contains(&"--slice=pelagos.slice".to_string()));
        assert!(argv.contains(&"--unit=pelagos-ctr-pcri-abc123.scope".to_string()));
        // The `--` separator must precede the wrapped binary so its flags are not
        // parsed by systemd-run.
        let sep = argv.iter().position(|s| s == "--").unwrap();
        assert_eq!(argv[sep + 1], "/usr/local/bin/pelagos");
        assert_eq!(argv[sep + 2], "run");
        assert_eq!(argv.last().unwrap(), "pcri-abc123");
    }

    #[test]
    fn service_argv_is_backgrounded_with_killmode() {
        let argv = build_service_argv(
            "pelagos-sbx-deadbeef.service",
            "/usr/local/bin/pelagos",
            &["sandbox", "__pause__", "pcri-deadbeef"],
        );
        assert_eq!(argv[0], "systemd-run");
        // A transient *service* must NOT carry --scope (which would block).
        assert!(!argv.contains(&"--scope".to_string()));
        assert!(argv.contains(&"--property=KillMode=mixed".to_string()));
        assert!(argv.contains(&"--unit=pelagos-sbx-deadbeef.service".to_string()));
        let sep = argv.iter().position(|s| s == "--").unwrap();
        assert_eq!(argv[sep + 1], "/usr/local/bin/pelagos");
        assert_eq!(argv[sep + 2], "sandbox");
    }

    #[test]
    fn dollar_refs_in_args_are_passed_through_unmodified() {
        // #543: whether systemd expands $VAR / ${VAR} references inside a transient
        // unit's argv is version-dependent (confirmed live: systemd 259 does, 255
        // does not) — a prior fix (#483) escaped every `$` as `$$` assuming
        // expansion always happens. On a systemd that does NOT expand, that escape
        // is never undone, so the container's bash process receives a literal `$$`
        // and interprets it as *its own PID* (1, as a container's PID-1 init),
        // corrupting `${BIN_PATH}` into `1{BIN_PATH}` — exactly the reported bug.
        // The fix must never rewrite the command text; the env mirroring below is
        // what makes systemd's expansion (when it happens) harmless instead.
        let script = "bash -ec 'nsenter \"${BIN_PATH}/tool\"'";
        let argv = build_scope_argv(
            "pelagos-ctr-pcri-abc.scope",
            "/usr/local/bin/pelagos",
            &[
                "run",
                "--env",
                "BIN_PATH=/k3s/bin",
                "--",
                "img",
                "bash",
                "-ec",
                script,
            ],
            &[("BIN_PATH".to_string(), "/k3s/bin".to_string())],
        );
        assert_eq!(argv.last().unwrap(), script);
        assert!(argv.contains(&"--env".to_string()));
        assert!(argv.contains(&"BIN_PATH=/k3s/bin".to_string()));
    }

    #[test]
    fn scope_argv_mirrors_container_envs_via_setenv() {
        // The container's declared env vars are mirrored onto systemd-run itself
        // via --setenv, so that IF the local systemd expands $VAR references in
        // the wrapped argv, they resolve to the same value the container process
        // would produce anyway — an idempotent no-op rather than data loss.
        let argv = build_scope_argv(
            "pelagos-ctr-pcri-abc.scope",
            "/usr/local/bin/pelagos",
            &["run", "--detach"],
            &[
                ("BIN_PATH".to_string(), "/k3s/bin".to_string()),
                ("FOO".to_string(), "bar".to_string()),
            ],
        );
        assert!(argv.contains(&"--setenv=BIN_PATH=/k3s/bin".to_string()));
        assert!(argv.contains(&"--setenv=FOO=bar".to_string()));
        // --setenv flags must precede the `--` separator (they configure
        // systemd-run itself, not the wrapped command).
        let sep = argv.iter().position(|s| s == "--").unwrap();
        let setenv_pos = argv
            .iter()
            .position(|s| s == "--setenv=BIN_PATH=/k3s/bin")
            .unwrap();
        assert!(setenv_pos < sep);
    }

    #[test]
    fn unit_names_are_sanitized_and_bounded() {
        // Hex/`pcri-` ids pass through unchanged.
        assert_eq!(
            container_unit("pcri-abc123def456"),
            "pelagos-ctr-pcri-abc123def456.scope"
        );
        assert_eq!(
            sandbox_unit("0123456789abcdef"),
            "pelagos-sbx-0123456789abcdef.service"
        );
        // Disallowed characters are replaced; length is bounded.
        let dirty = sanitize("a/b c*d\u{00e9}");
        assert!(dirty
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '.' | '-')));
        let long = sanitize(&"x".repeat(200));
        assert!(long.len() <= 64);
    }
}
