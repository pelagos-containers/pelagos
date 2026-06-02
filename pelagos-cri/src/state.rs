//! In-memory CRI state backed by disk at `/run/pelagos-cri/`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

const SANDBOXES_DIR: &str = "/run/pelagos-cri/sandboxes";
const CONTAINERS_DIR: &str = "/run/pelagos-cri/containers";

// ── CRI sandbox metadata ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CriSandbox {
    pub id: String,
    pub name: String,
    pub namespace: String,
    pub uid: String,
    pub attempt: u32,
    pub labels: HashMap<String, String>,
    pub annotations: HashMap<String, String>,
    pub created_at_ns: i64,
    pub state: SandboxState,
    /// Named netns for CNI sandboxes (e.g. "pcri-a1b2c3d4").
    /// Empty string means pelagos native networking was used.
    #[serde(default)]
    pub netns: String,
    /// IP assigned to this sandbox (by CNI or pelagos native IPAM).
    #[serde(default)]
    pub ip: String,
    /// Path to the CNI config file used for ADD; needed for DEL on teardown.
    #[serde(default)]
    pub cni_conf: String,
    /// PID of the pause process holding IPC/UTS namespaces open (CNI path only).
    #[serde(default)]
    pub pause_pid: i32,
    /// Sandbox log directory (passed by kubelet for kubectl logs).
    #[serde(default)]
    pub log_directory: String,
    /// Supplemental GIDs from the pod security context (fsGroup etc.).
    #[serde(default)]
    pub supplemental_groups: Vec<i64>,
    /// Cgroup parent path assigned by kubelet (e.g. "kubepods/besteffort/pod<uid>").
    /// Empty means no explicit cgroup placement (container inherits daemon cgroup).
    #[serde(default)]
    pub cgroup_parent: String,
    /// DNS nameservers from the pod DNS config.
    #[serde(default)]
    pub dns_servers: Vec<String>,
    /// DNS search domains from the pod DNS config.
    #[serde(default)]
    pub dns_searches: Vec<String>,
    /// DNS resolver options from the pod DNS config.
    #[serde(default)]
    pub dns_options: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SandboxState {
    Running,
    NotReady,
}

// ── CRI container metadata ───────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CriMount {
    pub host_path: String,
    pub container_path: String,
    pub readonly: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CriContainer {
    pub id: String,
    pub sandbox_id: String,
    pub pelagos_name: String,
    pub name: String,
    pub image: String,
    /// CRI `command` field — overrides image ENTRYPOINT.
    pub entrypoint: Vec<String>,
    /// CRI `args` field — overrides image CMD.
    pub args: Vec<String>,
    pub envs: Vec<(String, String)>,
    pub working_dir: String,
    pub mounts: Vec<CriMount>,
    pub labels: HashMap<String, String>,
    pub annotations: HashMap<String, String>,
    pub created_at_ns: i64,
    pub started_at_ns: i64,
    pub finished_at_ns: i64,
    pub state: ContainerState,
    pub exit_code: i32,
    /// Override UID from pod securityContext.runAsUser (None = use image default).
    #[serde(default)]
    pub run_as_user: Option<i64>,
    /// Override GID from pod securityContext.runAsGroup (None = use image default).
    #[serde(default)]
    pub run_as_group: Option<i64>,
    /// Host-side path of the termination log file (empty when not set).
    /// Populated from the mount whose container_path matches terminationMessagePath.
    #[serde(default)]
    pub termination_log_host_path: String,
    /// Log path relative to sandbox log_directory (kubelet-assigned).
    #[serde(default)]
    pub log_path: String,
    /// Supplemental GIDs from the container security context.
    #[serde(default)]
    pub supplemental_groups: Vec<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ContainerState {
    Created,
    Running,
    Exited,
    Unknown,
}

// ── StateInner ───────────────────────────────────────────────────────────────

pub struct StateInner {
    pub sandboxes: HashMap<String, CriSandbox>,
    pub containers: HashMap<String, CriContainer>,
    pub pelagos_bin: String,
}

impl StateInner {
    fn load() -> Self {
        let sandboxes = load_all_sandboxes();
        let containers = load_all_containers();
        StateInner {
            sandboxes,
            containers,
            pelagos_bin: String::new(),
        }
    }
}

// ── AppState ─────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct AppState {
    pub inner: Arc<Mutex<StateInner>>,
}

impl AppState {
    pub fn new(pelagos_bin: String) -> Self {
        let _ = std::fs::create_dir_all(SANDBOXES_DIR);
        let _ = std::fs::create_dir_all(CONTAINERS_DIR);
        let mut inner = StateInner::load();
        inner.pelagos_bin = pelagos_bin;

        // Purge stale sandboxes whose pause process no longer exists.
        // This happens when pelagos-cri restarts (e.g. after a system reboot or
        // RuntimeDirectory wipe) and the in-memory /run/pelagos/ state is gone.
        // k3s persists container IDs across its own restarts and will call
        // StartContainer on those old IDs, hitting "sandbox not found".
        // Removing the records here gives k3s a clean slate and causes it to
        // recreate the pods from scratch.
        let stale_sandbox_ids: Vec<String> = inner
            .sandboxes
            .values()
            .filter(|s| {
                if s.pause_pid <= 0 {
                    return false;
                }
                // Process is alive iff /proc/<pid> exists.
                !std::path::Path::new(&format!("/proc/{}", s.pause_pid)).exists()
            })
            .map(|s| s.id.clone())
            .collect();

        for sid in &stale_sandbox_ids {
            log::info!("startup: removing stale sandbox {sid} (pause process gone)");
            // Remove all containers that belonged to this sandbox.
            let stale_ctrs: Vec<String> = inner
                .containers
                .values()
                .filter(|c| &c.sandbox_id == sid)
                .map(|c| c.id.clone())
                .collect();
            for cid in stale_ctrs {
                inner.containers.remove(&cid);
                remove_container_file(&cid);
            }
            inner.sandboxes.remove(sid);
            remove_sandbox_file(sid);
            remove_pelagos_sandbox_state(sid);
        }

        AppState {
            inner: Arc::new(Mutex::new(inner)),
        }
    }
}

// ── Disk helpers ─────────────────────────────────────────────────────────────

pub fn save_sandbox(s: &CriSandbox) -> std::io::Result<()> {
    let _ = std::fs::create_dir_all(SANDBOXES_DIR);
    let path = format!("{}/{}.json", SANDBOXES_DIR, s.id);
    let json = serde_json::to_string(s)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(path, json)
}

pub fn save_container(c: &CriContainer) -> std::io::Result<()> {
    let _ = std::fs::create_dir_all(CONTAINERS_DIR);
    let path = format!("{}/{}.json", CONTAINERS_DIR, c.id);
    let json = serde_json::to_string(c)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(path, json)
}

pub fn remove_sandbox_file(id: &str) {
    let _ = std::fs::remove_file(format!("{}/{}.json", SANDBOXES_DIR, id));
}

// ── Pelagos runtime sandbox state (for `pelagos run --sandbox`) ──────────────

const PELAGOS_SANDBOXES_DIR: &str = "/run/pelagos/sandboxes";

/// Write the pelagos-format sandbox state so that `pelagos run --sandbox <id>`
/// can join the CNI-configured network namespace.
///
/// The JSON schema mirrors `pelagos::sandbox::SandboxState`.
pub fn write_pelagos_sandbox_state(
    id: &str,
    name: Option<&str>,
    pause_pid: i32,
    ns_name: &str,
    container_ip: &str,
) -> std::io::Result<()> {
    let dir = format!("{}/{}", PELAGOS_SANDBOXES_DIR, id);
    std::fs::create_dir_all(&dir)?;

    let json = serde_json::json!({
        "id": id,
        "name": name,
        "pause_pid": pause_pid,
        "ns_name": ns_name,
        "veth_host": "",        // CNI owns its own veth — pelagos must not delete it
        "container_ip": container_ip,
    });
    std::fs::write(
        format!("{}/state.json", dir),
        serde_json::to_string(&json).unwrap(),
    )?;
    // pause.pid and ns_name files are read by some pelagos internals.
    std::fs::write(format!("{}/pause.pid", dir), format!("{}", pause_pid))?;
    std::fs::write(format!("{}/ns_name", dir), ns_name)?;
    Ok(())
}

/// Remove the pelagos-format sandbox state directory.
pub fn remove_pelagos_sandbox_state(id: &str) {
    let dir = format!("{}/{}", PELAGOS_SANDBOXES_DIR, id);
    let _ = std::fs::remove_dir_all(dir);
}

pub fn remove_container_file(id: &str) {
    let _ = std::fs::remove_file(format!("{}/{}.json", CONTAINERS_DIR, id));
}

fn load_all_sandboxes() -> HashMap<String, CriSandbox> {
    let Ok(entries) = std::fs::read_dir(SANDBOXES_DIR) else {
        return HashMap::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "json").unwrap_or(false))
        .filter_map(|e| {
            std::fs::read_to_string(e.path())
                .ok()
                .and_then(|d| serde_json::from_str::<CriSandbox>(&d).ok())
        })
        .map(|s| (s.id.clone(), s))
        .collect()
}

fn load_all_containers() -> HashMap<String, CriContainer> {
    let Ok(entries) = std::fs::read_dir(CONTAINERS_DIR) else {
        return HashMap::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "json").unwrap_or(false))
        .filter_map(|e| {
            std::fs::read_to_string(e.path())
                .ok()
                .and_then(|d| serde_json::from_str::<CriContainer>(&d).ok())
        })
        .map(|c| (c.id.clone(), c))
        .collect()
}
