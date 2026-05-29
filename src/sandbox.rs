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

    /// Returns `/proc/<pause_pid>/ns/net` path.
    pub fn net_ns_path(&self) -> PathBuf {
        PathBuf::from(format!("/run/netns/{}", self.ns_name))
    }

    /// Returns `/proc/<pause_pid>/ns/ipc` path.
    pub fn ipc_ns_path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/{}/ns/ipc", self.pause_pid))
    }

    /// Returns `/proc/<pause_pid>/ns/uts` path.
    pub fn uts_ns_path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/{}/ns/uts", self.pause_pid))
    }

    /// Returns true if the pause process is still alive.
    pub fn is_alive(&self) -> bool {
        if self.pause_pid <= 0 {
            return false;
        }
        // kill(pid, 0) — check existence without sending a signal.
        unsafe { libc::kill(self.pause_pid, 0) == 0 }
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

    // Remove the state directory.
    let dir = crate::paths::sandbox_dir(id);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }

    Ok(())
}
