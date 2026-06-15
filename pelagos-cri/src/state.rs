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
    /// PID namespace mode from namespace_options.pid:
    ///   0 = POD (isolated, default), 1 = CONTAINER (isolated), 2 = NODE (host PID namespace).
    #[serde(default)]
    pub pid_namespace_mode: i32,
    /// IPC namespace mode from namespace_options.ipc:
    ///   0 = POD (isolated, default), 1 = CONTAINER (isolated), 2 = NODE (host IPC namespace).
    #[serde(default)]
    pub ipc_namespace_mode: i32,
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
    /// Capabilities to add on top of the default set (from CRI LinuxContainerSecurityContext).
    #[serde(default)]
    pub cap_add: Vec<String>,
    /// Capabilities to drop from the default set (from CRI LinuxContainerSecurityContext).
    #[serde(default)]
    pub cap_drop: Vec<String>,
    /// Run in privileged mode: all capabilities, no seccomp, /sys RW.
    #[serde(default)]
    pub privileged: bool,
    /// Memory limit in bytes (0 = no limit).
    #[serde(default)]
    pub memory_limit: i64,
    /// CPU CFS period in microseconds (0 = not specified).
    #[serde(default)]
    pub cpu_period: i64,
    /// CPU CFS quota in microseconds (0 = not specified).
    #[serde(default)]
    pub cpu_quota: i64,
    /// CPU shares (relative weight; 0 = not specified).
    #[serde(default)]
    pub cpu_shares: i64,
    /// Mount the container rootfs read-only (securityContext.readOnlyRootFilesystem).
    #[serde(default)]
    pub read_only_rootfs: bool,
    /// Seccomp profile type: 0=RuntimeDefault, 1=Unconfined, 2=Localhost.
    #[serde(default)]
    pub seccomp_profile_type: i32,
    /// Localhost seccomp profile path (only used when seccomp_profile_type == 2).
    #[serde(default)]
    pub seccomp_profile_path: String,
    /// Set PR_SET_NO_NEW_PRIVS on the container process.
    #[serde(default)]
    pub no_new_privs: bool,
    /// Paths to mask inside the container (e.g. /proc/kcore, /sys/firmware).
    #[serde(default)]
    pub masked_paths: Vec<String>,
    /// Paths to bind-mount read-only inside the container.
    #[serde(default)]
    pub readonly_paths: Vec<String>,
    /// AppArmor profile type: 0=RuntimeDefault, 1=Unconfined, 2=Localhost.
    #[serde(default)]
    pub apparmor_profile_type: i32,
    /// AppArmor localhost profile name (only used when apparmor_profile_type == 2).
    #[serde(default)]
    pub apparmor_profile_path: String,
    /// OOM score adjustment written to /proc/<pid>/oom_score_adj (-1000 to 1000).
    /// i32::MIN means "not set" (field absent from proto).
    #[serde(default = "default_oom_score_unset")]
    pub oom_score_adj: i32,
    /// Combined memory+swap cgroup limit in bytes (0 = not set, -1 = unlimited swap).
    #[serde(default)]
    pub memory_swap_limit: i64,
    /// CPUs this container may use (cpuset string, e.g. "0-3,6").
    #[serde(default)]
    pub cpuset_cpus: String,
    /// Memory nodes this container may use (cpuset string, e.g. "0-1").
    #[serde(default)]
    pub cpuset_mems: String,
    /// Signal to send when stopping the container (empty = SIGTERM default).
    #[serde(default)]
    pub stop_signal: String,
    /// HugePage limits: (page_size_string, limit_in_bytes).
    #[serde(default)]
    pub hugepage_limits: Vec<(String, u64)>,
    /// SELinux label "user:role:type:level" (empty = not set).
    #[serde(default)]
    pub selinux_label: String,
}

fn default_oom_score_unset() -> i32 {
    i32::MIN
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

        // Re-adopt running pods across a `pelagos-cri` restart (#336).
        //
        // CRI metadata under /run/pelagos-cri/ persists across restarts, and —
        // because each pod's pause process now runs in its own transient systemd
        // unit under `pelagos.slice` (see `scope`) — the pause survives the
        // runtime restart too. So a sandbox whose pause is still alive is simply
        // re-adopted: its metadata is kept and the kubelet's view is unchanged.
        //
        // Only sandboxes whose pause is genuinely gone (crash, reboot before the
        // unit came back, or a legacy non-scoped pause killed with the old
        // runtime) are purged, taking their containers with them, so the kubelet
        // recreates just those pods rather than every pod on the node.
        let stale = stale_sandbox_ids(&inner.sandboxes, |pid| {
            std::path::Path::new(&format!("/proc/{}", pid)).exists()
        });

        let adopted = inner.sandboxes.len() - stale.len();
        if adopted > 0 {
            log::info!("startup: re-adopting {adopted} running sandbox(es) across restart");
        }

        for sid in &stale {
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

/// Identify sandboxes whose supervisor (pause process) is gone and must be purged
/// on startup. `is_alive(pid)` reports whether a given pause PID is still running.
///
/// Sandboxes created on the native (non-CNI) path have `pause_pid <= 0` and no
/// pause process to check, so they are never treated as stale here. Pulled out as
/// a pure function so the re-adoption policy can be unit-tested without touching
/// `/proc` (#336).
pub(crate) fn stale_sandbox_ids<F: Fn(i32) -> bool>(
    sandboxes: &HashMap<String, CriSandbox>,
    is_alive: F,
) -> Vec<String> {
    sandboxes
        .values()
        .filter(|s| s.pause_pid > 0 && !is_alive(s.pause_pid))
        .map(|s| s.id.clone())
        .collect()
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

/// A CRI id must be a non-empty, single path component (our ids are 64-char
/// hex). Reject anything else so a corrupted/empty id from an inconsistent
/// ("phantom") sandbox can never make a removal escape its directory or wipe a
/// whole parent dir (#347).
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id != "." && id != ".." && !id.contains('/') && !id.contains('\0')
}

pub fn remove_sandbox_file(id: &str) {
    if !valid_id(id) {
        log::error!("remove_sandbox_file: refusing invalid id {id:?} (#347)");
        return;
    }
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
    if !valid_id(id) {
        log::error!("remove_pelagos_sandbox_state: refusing invalid id {id:?} (#347)");
        return;
    }
    let dir = format!("{}/{}", PELAGOS_SANDBOXES_DIR, id);
    let _ = std::fs::remove_dir_all(dir);
}

pub fn remove_container_file(id: &str) {
    if !valid_id(id) {
        log::error!("remove_container_file: refusing invalid id {id:?} (#347)");
        return;
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a CriSandbox with the given id and pause_pid via JSON so we don't
    /// have to spell out every (mostly `#[serde(default)]`) field.
    fn sandbox(id: &str, pause_pid: i32) -> CriSandbox {
        let json = format!(
            r#"{{"id":"{id}","name":"n","namespace":"ns","uid":"u","attempt":0,
                 "labels":{{}},"annotations":{{}},"created_at_ns":0,"state":"Running",
                 "pause_pid":{pause_pid}}}"#
        );
        serde_json::from_str(&json).expect("valid sandbox json")
    }

    fn map(items: Vec<CriSandbox>) -> HashMap<String, CriSandbox> {
        items.into_iter().map(|s| (s.id.clone(), s)).collect()
    }

    #[test]
    fn live_pause_sandboxes_are_re_adopted_not_purged() {
        // Two CNI sandboxes whose pause processes are still alive must survive a
        // restart untouched (#336): stale set is empty.
        let sandboxes = map(vec![sandbox("alive1", 1001), sandbox("alive2", 1002)]);
        let stale = stale_sandbox_ids(&sandboxes, |_pid| true);
        assert!(
            stale.is_empty(),
            "live sandboxes must be re-adopted: {stale:?}"
        );
    }

    #[test]
    fn dead_pause_sandboxes_are_purged() {
        // Only the sandbox whose pause is gone is purged; the live one is kept.
        let sandboxes = map(vec![sandbox("alive", 1001), sandbox("dead", 1002)]);
        let stale = stale_sandbox_ids(&sandboxes, |pid| pid == 1001);
        assert_eq!(stale, vec!["dead".to_string()]);
    }

    #[test]
    fn native_sandboxes_without_pause_are_never_stale() {
        // pause_pid <= 0 (native bridge path) has no pause to check; such a
        // sandbox must not be purged even though `is_alive` would say "dead".
        let sandboxes = map(vec![sandbox("native", 0), sandbox("native_neg", -1)]);
        let stale = stale_sandbox_ids(&sandboxes, |_pid| false);
        assert!(
            stale.is_empty(),
            "native sandboxes must never be stale: {stale:?}"
        );
    }
}
