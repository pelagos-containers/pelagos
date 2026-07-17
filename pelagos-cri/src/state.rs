//! In-memory CRI state backed by disk at `/run/pelagos-cri/`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

const SANDBOXES_DIR: &str = "/run/pelagos-cri/sandboxes";
const CONTAINERS_DIR: &str = "/run/pelagos-cri/containers";
const PELAGOS_CONTAINERS_DIR: &str = "/run/pelagos/containers";

/// SIGKILL every process in a named cgroup at startup (before tokio runtime
/// is spun up, so we use the sync std API).  No-op if the cgroup is absent.
fn kill_cgroup_on_startup(cgroup_name: &str) {
    let dir = std::path::Path::new("/sys/fs/cgroup").join(cgroup_name.trim_start_matches('/'));
    if !dir.is_dir() {
        return;
    }
    let kill_file = dir.join("cgroup.kill");
    if kill_file.exists() {
        let _ = std::fs::write(&kill_file, "1");
        return;
    }
    if let Ok(procs) = std::fs::read_to_string(dir.join("cgroup.procs")) {
        for line in procs.lines() {
            if let Ok(pid) = line.trim().parse::<libc::pid_t>() {
                if pid > 1 {
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                }
            }
        }
    }
}

// ── Namespace modes ──────────────────────────────────────────────────────────
//
// Mirror of `pelagos::sandbox::{NsMode, NamespaceModes}`. pelagos-cri is a lean
// CRI shim that shells out to the `pelagos` binary rather than linking the
// runtime library (which would pull in cgroups-rs/seccompiler/oci-client/...), so
// the type is duplicated here. The two stay compatible through the sandbox-state
// JSON contract: serde serialises the `NsMode` unit variants as the strings
// "Pod"/"Container"/"Node", which the library deserialises back into its own enum.

/// How a single Linux namespace is shared for a sandbox, mirroring the CRI
/// `NamespaceMode` enum: `Pod` (0), `Container` (1), `Node` (2).
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

    /// Convert back to the CRI `NamespaceMode` i32 (`POD`=0, `CONTAINER`=1, `NODE`=2).
    /// Inverse of [`NsMode::from_cri`]; used by `PodSandboxStatus` to report the
    /// sandbox's namespace modes back to the kubelet. Reporting the wrong value
    /// (e.g. `POD` for a `hostNetwork` sandbox) makes the kubelet's
    /// `podSandboxChanged` check see a mismatch and recreate the sandbox on every
    /// sync — an endless crash-loop for host-namespace pods (#410).
    pub fn to_cri(self) -> i32 {
        match self {
            NsMode::Pod => 0,
            NsMode::Container => 1,
            NsMode::Node => 2,
        }
    }

    /// True if this is the host namespace (CRI `NODE`).
    pub fn is_host(self) -> bool {
        matches!(self, NsMode::Node)
    }
}

/// Default PID namespace mode: `Container`, not `Pod` — PID is per-container
/// unless a pod sets `shareProcessNamespace` (mirrors `pelagos::sandbox`; #398).
fn default_pid_mode() -> NsMode {
    NsMode::Container
}

/// Network / PID / IPC namespace sharing modes for a sandbox, read from the pod's
/// CRI `NamespaceOption` exactly once and carried through sandbox state.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct NamespaceModes {
    /// Network namespace mode (`hostNetwork: true` ⇒ `Node`).
    #[serde(default)]
    pub network: NsMode,
    /// PID namespace mode: `Pod` (shared), `Container` (per-container, default),
    /// or `Node` (`hostPID`).
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
    /// Build from a CRI `NamespaceOption`'s (network, pid, ipc) i32 modes.
    pub fn from_cri(network: i32, pid: i32, ipc: i32) -> Self {
        NamespaceModes {
            network: NsMode::from_cri(network),
            pid: NsMode::from_cri(pid),
            ipc: NsMode::from_cri(ipc),
        }
    }
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
    /// (`shareProcessNamespace`, explicit CRI `pid == POD`; #398).
    pub fn shared_pid(&self) -> bool {
        matches!(self.pid, NsMode::Pod)
    }
}

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
    /// Network / PID / IPC namespace sharing modes, read once from the pod's
    /// `NamespaceOption` (POD / CONTAINER / NODE). Replaces the former separate
    /// `pid_namespace_mode` / `ipc_namespace_mode` i32 fields and adds `network`.
    #[serde(default)]
    pub namespaces: NamespaceModes,
    /// Host port mappings (PodSandboxConfig.port_mappings) — passed to the CNI
    /// portmap plugin as capability args at ADD and (for cleanup) at DEL.
    #[serde(default)]
    pub port_mappings: Vec<CriPortMapping>,
}

/// A CRI `PortMapping` (host_port → container_port) persisted with the sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CriPortMapping {
    /// Protocol: 0=TCP, 1=UDP, 2=SCTP (matches the CRI proto enum).
    pub protocol: i32,
    pub container_port: i32,
    pub host_port: i32,
    #[serde(default)]
    pub host_ip: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SandboxState {
    Running,
    /// The sandbox has been explicitly stopped via `StopPodSandbox`.
    ///
    /// **Invariant**: this variant is set in exactly ONE place —
    /// `stop_pod_sandbox` in `runtime.rs`.  Nothing else must write it.
    /// Phantom-detection (`stale_sandbox_ids`) relies on this: a sandbox
    /// in this state has a dead pause *by design*, so the missing pause is
    /// not evidence of an unexpected crash.  If you add another code path
    /// that sets `NotReady`, update `CriSandbox::is_explicitly_stopped` and
    /// `stale_sandbox_ids` to match.
    NotReady,
}

impl CriSandbox {
    /// Returns `true` iff `stop_pod_sandbox` has been called for this sandbox.
    ///
    /// The phantom-reaping logic uses this to distinguish between two cases
    /// that look identical at the pause-liveness level:
    ///
    /// * `false` (state == Running, pause dead) → unexpected crash → phantom → reap
    /// * `true`  (state == NotReady, pause dead) → explicit stop → waiting for
    ///   `remove_pod_sandbox` → do NOT reap (#438)
    ///
    /// The single source of truth for the mapping is `SandboxState::NotReady`;
    /// see the invariant doc on that variant before adding new callers.
    pub fn is_explicitly_stopped(&self) -> bool {
        self.state == SandboxState::NotReady
    }
}

// ── CRI container metadata ───────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CriMount {
    pub host_path: String,
    pub container_path: String,
    pub readonly: bool,
    /// CRI Mount.recursive_read_only (#356). Reported back in ContainerStatus so
    /// the kubelet/critest can confirm the runtime honored the request. `false`
    /// = non-recursive readonly (top mount only); `true` = recursive readonly.
    #[serde(default)]
    pub recursive_read_only: bool,
    /// CRI Mount.propagation enum value (0=PRIVATE, 1=HOST_TO_CONTAINER, 2=BIDIRECTIONAL).
    #[serde(default)]
    pub propagation: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CriContainer {
    pub id: String,
    pub sandbox_id: String,
    pub pelagos_name: String,
    pub name: String,
    /// CRI metadata.attempt — the container's restart attempt counter; must be
    /// preserved and reported back in ContainerStatus/ListContainers (#357).
    #[serde(default)]
    pub attempt: u32,
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
    /// True if the kernel OOM killer terminated the container (#343). Reported as
    /// `reason: OOMKilled` in ContainerStatus.
    #[serde(default)]
    pub oom_killed: bool,
    /// Override UID from pod securityContext.runAsUser (None = use image default).
    #[serde(default)]
    pub run_as_user: Option<i64>,
    /// Override GID from pod securityContext.runAsGroup (None = use image default).
    #[serde(default)]
    pub run_as_group: Option<i64>,
    /// Username from securityContext.runAsUsername, resolved to a uid against the
    /// image's /etc/passwd at run time (used when run_as_user is None). Empty = unset.
    #[serde(default)]
    pub run_as_username: String,
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
    /// Container was created with `stdin: true` — keep its stdin open on a FIFO
    /// (`pelagos run --stdin`) so a later `attach` can deliver input (#403).
    #[serde(default)]
    pub stdin: bool,
    /// Container was created with `tty: true` (CRI ContainerConfig.tty).
    #[serde(default)]
    pub tty: bool,
    /// True when the kubelet sets `io.kubernetes.cri.container-type=sidecar_container`
    /// (KEP-753 native sidecars: init containers with restartPolicy: Always). Stored so
    /// log relay and lifecycle handlers can distinguish sidecars from regular init
    /// containers without re-parsing the labels map on every call (#437).
    #[serde(default)]
    pub is_sidecar: bool,
    /// Device plugin device allocations from ContainerConfig.devices.
    /// Each entry is "host_path:container_path" (container_path may equal host_path).
    #[serde(default)]
    pub devices: Vec<CriDevice>,
}

/// A host device to expose inside the container, derived from the CRI Device proto.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct CriDevice {
    pub host_path: String,
    pub container_path: String,
    /// Cgroup permissions string ("mrw", "r", etc.).
    pub permissions: String,
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

    /// Reap sandboxes whose pause process is gone, taking their containers with
    /// them; returns the reaped sandbox ids. `is_alive(pid)` reports whether a
    /// pause pid is still live.
    ///
    /// A sandbox with a dead pause is unusable — its network namespace and
    /// processes are gone. Pelagos has always reaped these at startup; the same
    /// pass runs periodically (see [`AppState::reconcile_stale_sandboxes`]) so a
    /// dead "phantom" sandbox is removed within one interval instead of lingering
    /// in the live listing until the next restart. A lingering phantom is exactly
    /// what the kubelet discovers as an orphan and garbage-collects — the path
    /// that deleted the host `/bin` (#347). Keys on pause-liveness AND sandbox
    /// state: only Running sandboxes with a dead pause are phantoms; a NotReady
    /// sandbox whose pause is dead was explicitly stopped via StopPodSandbox and
    /// must survive until RemovePodSandbox is called (#438).
    pub(crate) fn reap_stale_sandboxes<F: Fn(i32) -> bool>(&mut self, is_alive: F) -> Vec<String> {
        let stale = stale_sandbox_ids(&self.sandboxes, is_alive);
        for sid in &stale {
            // Remove all containers that belonged to this sandbox.
            let stale_ctrs: Vec<String> = self
                .containers
                .values()
                .filter(|c| &c.sandbox_id == sid)
                .map(|c| c.id.clone())
                .collect();
            for cid in stale_ctrs {
                self.containers.remove(&cid);
                remove_container_file(&cid);
            }
            self.sandboxes.remove(sid);
            remove_sandbox_file(sid);
            remove_pelagos_sandbox_state(sid);
        }
        stale
    }
}

// ── AppState ─────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct AppState {
    pub inner: Arc<Mutex<StateInner>>,
    /// Per-container "CRI log finalized" flags, set by the log relay once it has
    /// drained and stopped after the container exits. `container_status` waits on
    /// this at the Running→Exited transition so a client (e.g. critest) that reads
    /// the log right after seeing the container exited gets the COMPLETE log, not a
    /// partially-flushed one (#344). Keyed by pelagos_name; not persisted.
    pub log_done: Arc<Mutex<HashMap<String, Arc<std::sync::atomic::AtomicBool>>>>,
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
        let total_before = inner.sandboxes.len();
        let stale = inner
            .reap_stale_sandboxes(|pid| std::path::Path::new(&format!("/proc/{}", pid)).exists());

        let adopted = total_before - stale.len();
        if adopted > 0 {
            log::info!("startup: re-adopting {adopted} running sandbox(es) across restart");
        }
        for sid in &stale {
            log::info!("startup: removing stale sandbox {sid} (pause process gone)");
        }

        // #457 — hostNetwork container processes hold ports in the HOST network
        // namespace.  A CRI restart re-adopts the sandbox (pause still alive,
        // network namespace preserved) but leaves the container process running
        // with its host-side port binding intact.  When kubelet later calls
        // StopPodSandbox + RunPodSandbox for a replacement pod, the new
        // container tries to bind the same port and fails with EADDRINUSE.
        //
        // Fix: on startup, SIGKILL the container process for every re-adopted
        // hostNetwork container and mark it Exited.  Kubelet sees the container
        // as dead and immediately calls StartContainer (or a full recreate cycle)
        // — the replacement starts clean with no port conflict.
        //
        // Non-hostNetwork containers are NOT touched: their processes are in the
        // pod's private network namespace and hold no host ports, so they survive
        // the CRI restart harmlessly under #336 (zero churn).
        //
        // This is surgical: only the class of container that can cause EADDRINUSE
        // gets restarted; the vast majority of workloads are unaffected.
        let finished_at_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        let host_network_ctrs: Vec<String> = inner
            .containers
            .values()
            .filter(|c| c.state == ContainerState::Running)
            .filter(|c| {
                inner
                    .sandboxes
                    .get(&c.sandbox_id)
                    .map(|s| s.namespaces.host_network())
                    .unwrap_or(false)
            })
            .map(|c| c.id.clone())
            .collect();

        let mut host_net_killed: u32 = 0;
        for cid in &host_network_ctrs {
            let Some(c) = inner.containers.get_mut(cid) else {
                continue;
            };
            let pelagos_name = c.pelagos_name.clone();

            // Read the live PID from the pelagos state file and SIGKILL it.
            let state_path = format!("{}/{}/state.json", PELAGOS_CONTAINERS_DIR, pelagos_name);
            if let Ok(data) = std::fs::read_to_string(&state_path) {
                if let Ok(cs) = serde_json::from_str::<serde_json::Value>(&data) {
                    let pid = cs.get("pid").and_then(|v| v.as_i64()).unwrap_or(0) as libc::pid_t;
                    if pid > 1 && unsafe { libc::kill(pid, 0) } == 0 {
                        unsafe { libc::kill(pid, libc::SIGKILL) };
                        log::info!(
                            "startup: killed hostNetwork container process \
                             pid={pid} ({pelagos_name}) — port binding released (#457)"
                        );
                        host_net_killed += 1;
                    }
                    // Also kill via cgroup to catch forked/setsid'd descendants.
                    if let Some(cg) = cs.get("cgroup_name").and_then(|v| v.as_str()) {
                        kill_cgroup_on_startup(cg);
                    }
                }
            }

            // Mark Exited so kubelet immediately schedules a container restart.
            c.state = ContainerState::Exited;
            c.finished_at_ns = finished_at_ns;
            let _ = save_container(c);
        }
        if host_net_killed > 0 {
            log::info!(
                "startup: released {host_net_killed} hostNetwork port binding(s); \
                 kubelet will restart those container(s)"
            );
        }

        AppState {
            inner: Arc::new(Mutex::new(inner)),
            log_done: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Reap dead-pause ("phantom") sandboxes from the live state. Intended to run
    /// on a timer (see `main`) so a phantom is removed within one interval rather
    /// than lingering in `list_pod_sandbox` until the next restart — otherwise the
    /// kubelet discovers it as an orphan and garbage-collects it, the path that
    /// deleted the host `/bin` (#347). Mirrors the startup reconciliation.
    pub async fn reconcile_stale_sandboxes(&self) {
        let reaped = {
            let mut st = self.inner.lock().await;
            st.reap_stale_sandboxes(|pid| std::path::Path::new(&format!("/proc/{}", pid)).exists())
        };
        for sid in &reaped {
            log::info!("reconcile: removed stale sandbox {sid} (pause process gone)");
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
        // Only reap sandboxes that were NOT explicitly stopped.  A sandbox
        // where `is_explicitly_stopped()` returns true had its pause killed
        // intentionally by `stop_pod_sandbox`; it must survive in state until
        // `remove_pod_sandbox` is called, so that `ContainerStatus` can still
        // return container records and `kubectl logs` works on terminated pods
        // (#438).  See `CriSandbox::is_explicitly_stopped` for the invariant.
        .filter(|s| !s.is_explicitly_stopped() && s.pause_pid > 0 && !is_alive(s.pause_pid))
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
    namespaces: NamespaceModes,
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
        "namespaces": namespaces,
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

    /// Build a minimal CriContainer attached to a sandbox via JSON.
    fn container(id: &str, sandbox_id: &str) -> CriContainer {
        let json = format!(
            r#"{{"id":"{id}","sandbox_id":"{sandbox_id}","pelagos_name":"pcri-{id}",
                 "name":"c","image":"img","entrypoint":[],"args":[],"envs":[],
                 "working_dir":"","mounts":[],"labels":{{}},"annotations":{{}},
                 "created_at_ns":0,"started_at_ns":0,"finished_at_ns":0,
                 "state":"Running","exit_code":0}}"#
        );
        serde_json::from_str(&json).expect("valid container json")
    }

    /// Build a CriSandbox that has been explicitly stopped via StopPodSandbox
    /// (state=NotReady, pause already dead) — as opposed to a phantom whose pause
    /// died unexpectedly.
    fn stopped_sandbox(id: &str, pause_pid: i32) -> CriSandbox {
        let json = format!(
            r#"{{"id":"{id}","name":"n","namespace":"ns","uid":"u","attempt":0,
                 "labels":{{}},"annotations":{{}},"created_at_ns":0,"state":"NotReady",
                 "pause_pid":{pause_pid}}}"#
        );
        serde_json::from_str(&json).expect("valid stopped sandbox json")
    }

    /// #438 regression: a NotReady sandbox whose pause is dead (because
    /// StopPodSandbox was already called) must NOT be reaped as a phantom.
    ///
    /// The normal kubelet sequence is: StopPodSandbox → [kubectl logs works here]
    /// → RemovePodSandbox.  If stale_sandbox_ids includes the NotReady sandbox,
    /// list_pod_sandbox reaps it and its containers — ContainerStatus then returns
    /// not_found and `kubectl logs` on a terminated pod fails.
    #[test]
    fn stopped_sandbox_with_dead_pause_is_not_reaped() {
        // Sandbox explicitly stopped (state=NotReady); pause is gone.
        let sandboxes = map(vec![stopped_sandbox("stopped", 1234)]);
        let stale = stale_sandbox_ids(&sandboxes, |_pid| false); // all pids "dead"
        assert!(
            stale.is_empty(),
            "a NotReady sandbox must not be reaped as a phantom (#438): {stale:?}"
        );
    }

    /// #438: only Running sandboxes with a dead pause are phantoms.  A mix of
    /// Running/NotReady sandboxes must reap exactly the Running-and-dead ones.
    #[test]
    fn only_running_sandboxes_with_dead_pause_are_reaped() {
        let sandboxes = map(vec![
            sandbox("running-live", 1001),         // Running, pause alive → keep
            sandbox("running-dead", 1002),         // Running, pause dead → reap
            stopped_sandbox("stopped-dead", 1003), // NotReady, pause dead → keep
        ]);
        // pid 1001 alive; 1002 and 1003 dead.
        let mut stale = stale_sandbox_ids(&sandboxes, |pid| pid == 1001);
        stale.sort();
        assert_eq!(
            stale,
            vec!["running-dead".to_string()],
            "only the Running sandbox with a dead pause must be reaped"
        );
    }

    /// #347 follow-up: reaping a dead-pause "phantom" sandbox must drop it from the
    /// live state AND take its containers with it, while leaving live sandboxes (and
    /// their containers) untouched. This is what stops us from continuing to present
    /// the kubelet an orphan to GC between restarts.
    #[test]
    fn reap_removes_dead_phantom_sandbox_and_its_containers() {
        let mut inner = StateInner {
            sandboxes: map(vec![sandbox("alive", 1001), sandbox("dead", 1002)]),
            containers: vec![
                container("c-alive", "alive"),
                container("c-dead1", "dead"),
                container("c-dead2", "dead"),
            ]
            .into_iter()
            .map(|c| (c.id.clone(), c))
            .collect(),
            pelagos_bin: String::new(),
        };

        // pid 1001 (alive) is live; 1002 (dead) is gone.
        let reaped = inner.reap_stale_sandboxes(|pid| pid == 1001);

        assert_eq!(
            reaped,
            vec!["dead".to_string()],
            "only the dead sandbox reaped"
        );
        assert!(inner.sandboxes.contains_key("alive"), "live sandbox kept");
        assert!(
            !inner.sandboxes.contains_key("dead"),
            "dead sandbox removed"
        );
        assert!(
            inner.containers.contains_key("c-alive"),
            "live container kept"
        );
        assert!(
            !inner.containers.contains_key("c-dead1") && !inner.containers.contains_key("c-dead2"),
            "the phantom's containers must be removed with it"
        );
    }
}
