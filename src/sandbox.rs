//! Pod sandbox support — shared network/IPC/UTS namespaces for groups of containers.
//!
//! A sandbox owns a bridge-attached network namespace (same as NetworkMode::Bridge)
//! and a long-lived "pause" process that holds the namespaces open.  Containers
//! can join the sandbox's namespaces via [`SandboxInfo::namespace_paths`] and
//! `Command::with_sandbox()`.
//!
//! ## State layout
//!
//! ```text
//! <runtime>/sandboxes/<id>/
//!   pause.pid   — PID of the pause process
//!   ns_name     — named network namespace name (e.g. "psb-abc12345")
//!   name        — optional human-readable name
//! ```
//!
//! The named netns lives at `/run/netns/<ns_name>` and is created by
//! [`create_sandbox`] via `setup_bridge_network`, identical to a normal
//! bridge container.  When the sandbox is removed, teardown_network removes
//! the veth and netns.

use serde::{Deserialize, Serialize};
use std::io;
use std::path::PathBuf;

// ── SandboxError ──────────────────────────────────────────────────────────────

/// Error type for sandbox operations.
#[derive(Debug)]
pub enum SandboxError {
    /// Sandbox not found (load failed).
    NotFound(String),
    /// Sandbox exists but pause process is not running.
    NotRunning(String),
    /// I/O error.
    Io(io::Error),
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SandboxError::NotFound(msg) => write!(f, "sandbox not found: {}", msg),
            SandboxError::NotRunning(id) => {
                write!(f, "sandbox '{}' is not running (pause process dead)", id)
            }
            SandboxError::Io(e) => write!(f, "sandbox I/O error: {}", e),
        }
    }
}

impl std::error::Error for SandboxError {}

impl From<io::Error> for SandboxError {
    fn from(e: io::Error) -> Self {
        SandboxError::Io(e)
    }
}

// ── Namespace modes ──────────────────────────────────────────────────────────

/// How a single Linux namespace is shared for a sandbox, mirroring the CRI
/// `NamespaceMode` enum (runtime.v1): `Pod` (0), `Container` (1), `Node` (2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum NsMode {
    /// Isolated namespace shared among the pod's containers (CRI `POD`).
    #[default]
    Pod,
    /// Per-container isolated namespace (CRI `CONTAINER`).
    Container,
    /// The host namespace — no isolation (CRI `NODE`, e.g. `hostNetwork`).
    Node,
}

impl NsMode {
    /// Convert a CRI `NamespaceMode` i32 (0=`POD`, 1=`CONTAINER`, 2=`NODE`).
    pub fn from_cri(mode: i32) -> Self {
        match mode {
            2 => NsMode::Node,
            1 => NsMode::Container,
            _ => NsMode::Pod,
        }
    }

    /// True if this is the host namespace (CRI `NODE`).
    pub fn is_host(self) -> bool {
        matches!(self, NsMode::Node)
    }
}

/// Default PID namespace mode: `Container` (per-container isolation), **not**
/// `Pod`. This matches Kubernetes/CRI semantics — network and IPC are pod-shared
/// by default, but the PID namespace is per-container unless a pod explicitly
/// sets `shareProcessNamespace` (CRI `pid == POD`). Using `Container` as the
/// default means an absent `NamespaceOption` (and legacy sandbox state) never
/// accidentally enables PID sharing (#398).
fn default_pid_mode() -> NsMode {
    NsMode::Container
}

/// The network / PID / IPC namespace sharing modes for a sandbox.
///
/// Read from the CRI `NamespaceOption` exactly once (in `RunPodSandbox`) and
/// carried through sandbox state, so the pause, container join, and teardown all
/// consult one source of truth rather than re-deriving from scattered fields.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct NamespaceModes {
    /// Network namespace mode (`hostNetwork: true` ⇒ `Node`).
    #[serde(default)]
    pub network: NsMode,
    /// PID namespace mode: `Pod` (shared, `shareProcessNamespace`), `Container`
    /// (per-container, the default), or `Node` (`hostPID: true`).
    #[serde(default = "default_pid_mode")]
    pub pid: NsMode,
    /// IPC namespace mode (`hostIPC: true` ⇒ `Node`).
    #[serde(default)]
    pub ipc: NsMode,
}

impl Default for NamespaceModes {
    fn default() -> Self {
        NamespaceModes {
            network: NsMode::Pod,
            pid: default_pid_mode(),
            ipc: NsMode::Pod,
        }
    }
}

impl NamespaceModes {
    /// True when the pod shares the host network namespace.
    pub fn host_network(&self) -> bool {
        self.network.is_host()
    }
    /// True when the pod shares the host IPC namespace.
    pub fn host_ipc(&self) -> bool {
        self.ipc.is_host()
    }
    /// True when the pod shares the host PID namespace.
    pub fn host_pid(&self) -> bool {
        self.pid.is_host()
    }
    /// True when the pod containers share a single pod PID namespace
    /// (`shareProcessNamespace: true`, explicit CRI `pid == POD`). The pause is
    /// PID 1 of that namespace and containers join it (#398).
    pub fn shared_pid(&self) -> bool {
        matches!(self.pid, NsMode::Pod)
    }
}

// ── SandboxState ─────────────────────────────────────────────────────────────

/// Persistent state for a running sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxState {
    /// Short random hex ID (16 chars).
    pub id: String,
    /// Optional human-readable name.
    pub name: Option<String>,
    /// PID of the pause process that holds the namespaces open.
    pub pause_pid: i32,
    /// Named netns name (e.g. `"psb-abc12345"`), present as `/run/netns/<ns_name>`.
    pub ns_name: String,
    /// Host-side veth interface name (e.g. `"vh-a1b2c3d4"`).
    pub veth_host: String,
    /// Container IP assigned to the sandbox's eth0.
    pub container_ip: String,
    /// Network / PID / IPC namespace sharing modes for the sandbox.
    /// `#[serde(default)]` keeps state written before this field existed
    /// loadable (defaults to all-`Pod`, i.e. the previous behaviour).
    #[serde(default)]
    pub namespaces: NamespaceModes,
}

impl SandboxState {
    /// Load from `<runtime>/sandboxes/<id>/state.json`.
    pub fn load(id: &str) -> io::Result<Self> {
        let path = crate::paths::sandbox_dir(id).join("state.json");
        let data = std::fs::read_to_string(&path)
            .map_err(|e| io::Error::other(format!("sandbox '{}' not found: {}", id, e)))?;
        serde_json::from_str(&data).map_err(|e| io::Error::other(e.to_string()))
    }

    /// Save to `<runtime>/sandboxes/<id>/state.json`.
    pub fn save(&self) -> io::Result<()> {
        let dir = crate::paths::sandbox_dir(&self.id);
        std::fs::create_dir_all(&dir)?;
        let json =
            serde_json::to_string_pretty(self).map_err(|e| io::Error::other(e.to_string()))?;
        std::fs::write(dir.join("state.json"), json)
    }

    /// Returns the network namespace path for joining.
    ///
    /// Uses `/proc/<pause_pid>/ns/net` (always valid while the sandbox is
    /// alive) rather than `/run/netns/<ns_name>` (bind-mount that can
    /// disappear after a CRI restart, causing `setns` EINVAL — #461).
    pub fn net_ns_path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/{}/ns/net", self.pause_pid))
    }

    /// Returns `/proc/<pause_pid>/ns/ipc` path.
    pub fn ipc_ns_path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/{}/ns/ipc", self.pause_pid))
    }

    /// Returns `/proc/<pause_pid>/ns/uts` path.
    pub fn uts_ns_path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/{}/ns/uts", self.pause_pid))
    }

    /// Returns `/proc/<pause_pid>/ns/pid` path. For a shared-PID sandbox the
    /// `pause_pid` is the PID-1 child of the pod PID namespace, so containers
    /// joining this path land in the pod's PID namespace (#398).
    pub fn pid_ns_path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/{}/ns/pid", self.pause_pid))
    }

    /// Returns true if the pause process is still alive and not a zombie.
    ///
    /// `kill(pid, 0)` returns 0 for zombie processes (they remain in the process
    /// table until reaped), causing false positives: a zombie pause would pass
    /// the is_alive check but its /proc/<pid>/ns/{ipc,uts} may belong to a
    /// different process if the PID was recycled.  Read /proc/status instead.
    pub fn is_alive(&self) -> bool {
        if self.pause_pid <= 0 {
            return false;
        }
        match std::fs::read_to_string(format!("/proc/{}/status", self.pause_pid)) {
            Err(_) => false,
            Ok(s) => s
                .lines()
                .find(|l| l.starts_with("State:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .map(|c| !c.starts_with('Z'))
                .unwrap_or(false),
        }
    }
}

// ── List sandboxes ────────────────────────────────────────────────────────────

/// Return all sandbox states from the sandboxes directory.
pub fn list_sandboxes() -> Vec<SandboxState> {
    let dir = crate::paths::sandboxes_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut sandboxes = Vec::new();
    for entry in entries.flatten() {
        let state_file = entry.path().join("state.json");
        if state_file.exists() {
            if let Ok(data) = std::fs::read_to_string(&state_file) {
                if let Ok(s) = serde_json::from_str::<SandboxState>(&data) {
                    sandboxes.push(s);
                }
            }
        }
    }
    sandboxes.sort_by(|a, b| a.id.cmp(&b.id));
    sandboxes
}

// ── Generate sandbox ID ───────────────────────────────────────────────────────

/// Generate a 16-char random hex sandbox ID.
pub fn generate_sandbox_id() -> String {
    // Use /dev/urandom for 8 bytes → 16 hex chars.
    let mut buf = [0u8; 8];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut buf);
    } else {
        // Fallback: use PID + time
        let pid = unsafe { libc::getpid() } as u64;
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos() as u64;
        let v = pid ^ (t << 32) ^ t;
        buf.copy_from_slice(&v.to_le_bytes());
    }
    buf.iter().map(|b| format!("{:02x}", b)).collect()
}

// ── Create sandbox ────────────────────────────────────────────────────────────

/// Create a new sandbox: allocate an ID, set up bridge networking, spawn the
/// pause process, and persist state.
///
/// Returns the `SandboxState` of the created sandbox.
///
/// # Errors
///
/// Returns an error if bridge networking setup fails, the state directory
/// cannot be created, or the pause process cannot be spawned.
pub fn create_sandbox(name: Option<&str>) -> io::Result<SandboxState> {
    if unsafe { libc::getuid() } != 0 {
        return Err(io::Error::other(
            "sandbox create requires root (bridge networking needs CAP_NET_ADMIN)",
        ));
    }

    let id = generate_sandbox_id();
    let ns_name = format!("psb-{}", &id[..8]);

    // Create state directory.
    let dir = crate::paths::sandbox_dir(&id);
    std::fs::create_dir_all(&dir)?;

    // Set up bridge networking (same as a normal bridge container).
    // nat=false: the sandbox itself doesn't need NAT; containers joining it
    // will inherit the same IP but NAT is handled per-container at run time.
    let net_setup = crate::network::setup_bridge_network(&ns_name, "pelagos0", false, vec![])
        .map_err(|e| io::Error::other(format!("sandbox bridge network setup failed: {}", e)))?;

    let container_ip = net_setup.container_ip.to_string();
    let veth_host = net_setup.veth_host.clone();

    // Spawn the pause process.  We re-exec ourselves with the internal
    // `sandbox pause <ns_name>` subcommand so no external binary is needed.
    let exe = std::env::current_exe()
        .map_err(|e| io::Error::other(format!("cannot find current executable: {}", e)))?;

    let child = std::process::Command::new(&exe)
        .args(["sandbox", "__pause__", &ns_name])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| io::Error::other(format!("failed to spawn pause process: {}", e)))?;

    let pause_pid = child.id() as i32;

    // Leak the child handle — we don't wait on it; it runs until sandbox rm.
    std::mem::forget(child);

    // Wait briefly for the pause process to enter the namespaces.
    std::thread::sleep(std::time::Duration::from_millis(50));

    let state = SandboxState {
        id: id.clone(),
        name: name.map(|s| s.to_string()),
        pause_pid,
        ns_name: ns_name.clone(),
        veth_host,
        container_ip,
        // `pelagos sandbox create` (non-CRI) always uses isolated pod namespaces.
        namespaces: NamespaceModes::default(),
    };
    state.save()?;

    // Write pause.pid file for quick access.
    std::fs::write(
        crate::paths::sandbox_pid_file(&id),
        format!("{}", pause_pid),
    )?;

    // Persist the ns_name for teardown.
    std::fs::write(crate::paths::sandbox_ns_name_file(&id), &ns_name)?;

    if let Some(n) = name {
        std::fs::write(crate::paths::sandbox_name_file(&id), n)?;
    }

    // Store the network setup in a drop guard — but since we're creating the
    // sandbox (not tearing down), we need to NOT tear down.  Forget the setup.
    std::mem::forget(net_setup);

    Ok(state)
}

// ── Remove sandbox ────────────────────────────────────────────────────────────

/// Remove a sandbox: SIGTERM the pause process, tear down the network namespace,
/// and clean up the state directory.
pub fn remove_sandbox(id: &str) -> io::Result<()> {
    // Reject an empty/separator-laden id before any path is built from it: an
    // empty id makes `sandbox_dir("")` resolve to the sandboxes parent dir, and
    // a `/`-leading id escapes the runtime dir entirely (#347).
    if id.is_empty() || id.contains('/') || id == "." || id == ".." {
        return Err(io::Error::other(format!("invalid sandbox id '{}'", id)));
    }
    let state = SandboxState::load(id)?;

    // Send SIGTERM to the pause process.
    if state.is_alive() {
        unsafe { libc::kill(state.pause_pid, libc::SIGTERM) };
        // Wait up to 2 s for it to exit, then SIGKILL.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while state.is_alive() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if state.is_alive() {
            unsafe { libc::kill(state.pause_pid, libc::SIGKILL) };
        }
    }

    // Tear down the network namespace.
    let veth_host = state.veth_host.clone();
    // Best-effort: ignore errors (veth may already be gone if container died).
    let _ = crate::netlink::link_del(&veth_host);
    let _ = crate::netlink::netns_del(&state.ns_name);

    // Remove the state directory (guarded against any path outside the runtime dir).
    let dir = crate::paths::sandbox_dir(id);
    if dir.exists() {
        crate::paths::guarded_remove_dir_all(&dir)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nsmode_from_cri() {
        assert_eq!(NsMode::from_cri(0), NsMode::Pod);
        assert_eq!(NsMode::from_cri(1), NsMode::Container);
        assert_eq!(NsMode::from_cri(2), NsMode::Node);
        // Unknown values default to the safe isolated Pod mode.
        assert_eq!(NsMode::from_cri(99), NsMode::Pod);
        assert!(NsMode::Node.is_host());
        assert!(!NsMode::Pod.is_host());
        assert!(!NsMode::Container.is_host());
    }

    #[test]
    fn test_namespace_modes_host_helpers() {
        let m = NamespaceModes {
            network: NsMode::Node,
            pid: NsMode::Pod,
            ipc: NsMode::Node,
        };
        assert!(m.host_network());
        assert!(!m.host_pid());
        assert!(m.host_ipc());
        assert!(m.shared_pid()); // pid == Pod ⇒ shared
                                 // Default: net/ipc are pod-shared, but PID defaults to Container
                                 // (per-container isolation) — NOT shared — so an absent NamespaceOption
                                 // never accidentally enables shareProcessNamespace (#398).
        let d = NamespaceModes::default();
        assert!(!d.host_network() && !d.host_pid() && !d.host_ipc());
        assert!(!d.shared_pid());
        assert!(matches!(d.pid, NsMode::Container));
        // shared_pid only for an EXPLICIT pid == POD.
        let shared = NamespaceModes {
            pid: NsMode::Pod,
            ..Default::default()
        };
        assert!(shared.shared_pid() && !shared.host_pid());
    }

    #[test]
    fn test_namespace_modes_serde_contract() {
        // pelagos-cri serialises its mirror NamespaceModes into the sandbox-state
        // JSON; the library must deserialise it back. The wire form is the unit
        // variant names "Pod"/"Container"/"Node" — pin that so the two crates
        // can't silently drift apart.
        let m = NamespaceModes {
            network: NsMode::Node,
            pid: NsMode::Container,
            ipc: NsMode::Pod,
        };
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(json, r#"{"network":"Node","pid":"Container","ipc":"Pod"}"#);
        let back: NamespaceModes = serde_json::from_str(&json).unwrap();
        assert!(back.host_network() && !back.host_pid() && !back.host_ipc());
        // A state blob written before this field existed still loads (serde
        // default) and — critically — does NOT enable PID sharing: the `pid`
        // field defaults to Container, so legacy sandboxes stay isolated (#398).
        let legacy: SandboxState = serde_json::from_str(
            r#"{"id":"x","name":null,"pause_pid":1,"ns_name":"n","veth_host":"","container_ip":"1.2.3.4"}"#,
        )
        .unwrap();
        assert!(!legacy.namespaces.host_network());
        assert!(!legacy.namespaces.shared_pid());
        assert!(matches!(legacy.namespaces.pid, NsMode::Container));
        // Explicit pid==POD round-trips and is detected as shared.
        let shared = NamespaceModes {
            pid: NsMode::Pod,
            ..Default::default()
        };
        let back: NamespaceModes =
            serde_json::from_str(&serde_json::to_string(&shared).unwrap()).unwrap();
        assert!(back.shared_pid());
    }
}
