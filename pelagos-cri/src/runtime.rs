//! CRI RuntimeService implementation.

use crate::cni;
use crate::cri::runtime_service_server::RuntimeService;
use crate::cri::{
    AttachRequest, AttachResponse, CheckpointContainerRequest, CheckpointContainerResponse,
    ContainerAttributes, ContainerEventResponse, ContainerMetadata,
    ContainerState as CriContainerStateEnum, ContainerStats, ContainerStatsRequest,
    ContainerStatsResponse, ContainerStatus as CriContainerStatus, ContainerStatusRequest,
    ContainerStatusResponse, CpuUsage, CreateContainerRequest, CreateContainerResponse,
    ExecRequest, ExecResponse, ExecSyncRequest, ExecSyncResponse, FilesystemIdentifier,
    FilesystemUsage, GetEventsRequest, ImageSpec, LinuxPodSandboxStats, LinuxPodSandboxStatus,
    ListContainerStatsRequest, ListContainerStatsResponse, ListContainersRequest,
    ListContainersResponse, ListMetricDescriptorsRequest, ListMetricDescriptorsResponse,
    ListPodSandboxMetricsRequest, ListPodSandboxMetricsResponse, ListPodSandboxRequest,
    ListPodSandboxResponse, ListPodSandboxStatsRequest, ListPodSandboxStatsResponse, MemoryUsage,
    Namespace, NamespaceOption, PodSandbox, PodSandboxAttributes, PodSandboxMetadata,
    PodSandboxNetworkStatus, PodSandboxState, PodSandboxStats, PodSandboxStatsRequest,
    PodSandboxStatsResponse, PodSandboxStatus as CriPodSandboxStatus, PodSandboxStatusRequest,
    PodSandboxStatusResponse, PortForwardRequest, PortForwardResponse, RemoveContainerRequest,
    RemoveContainerResponse, RemovePodSandboxRequest, RemovePodSandboxResponse,
    ReopenContainerLogRequest, ReopenContainerLogResponse, RuntimeCondition, RuntimeConfigRequest,
    RuntimeConfigResponse, RuntimeStatus, StartContainerRequest, StartContainerResponse,
    StatusRequest, StatusResponse, StopContainerRequest, StopContainerResponse,
    StopPodSandboxRequest, StopPodSandboxResponse, StreamContainerStatsRequest,
    StreamContainerStatsResponse, StreamContainersRequest, StreamContainersResponse,
    StreamPodSandboxMetricsRequest, StreamPodSandboxMetricsResponse, StreamPodSandboxStatsRequest,
    StreamPodSandboxStatsResponse, StreamPodSandboxesRequest, StreamPodSandboxesResponse,
    UInt64Value, UpdateContainerResourcesRequest, UpdateContainerResourcesResponse,
    UpdatePodSandboxResourcesRequest, UpdatePodSandboxResourcesResponse,
    UpdateRuntimeConfigRequest, UpdateRuntimeConfigResponse, VersionRequest, VersionResponse,
};
use crate::invoke::run_pelagos;
use crate::state::{
    self, AppState, ContainerState as MyContainerState, CriContainer, CriMount, CriSandbox,
    SandboxState,
};
use crate::streaming::{PendingExec, PendingPortForward, Registry};
use serde::Deserialize;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use tonic::{Request, Response, Status};

const PELAGOS_SANDBOXES_DIR: &str = "/run/pelagos/sandboxes";
const PELAGOS_CONTAINERS_DIR: &str = "/run/pelagos/containers";

// ── Pelagos on-disk state structs ────────────────────────────────────────────

#[derive(Deserialize)]
struct PelagosSandboxState {
    #[allow(dead_code)]
    id: String,
    #[allow(dead_code)]
    #[serde(default)]
    name: Option<String>,
    #[allow(dead_code)]
    pause_pid: i32,
    #[allow(dead_code)]
    ns_name: String,
    #[allow(dead_code)]
    veth_host: String,
    container_ip: String,
}

#[derive(Deserialize)]
struct PelagosContainerState {
    #[allow(dead_code)]
    name: String,
    status: String,
    pid: i32,
    #[allow(dead_code)]
    started_at: String,
    #[serde(default)]
    exit_code: Option<i32>,
    /// Cgroup path stored by `pelagos run` for CPU/memory accounting.
    /// Relative to the cgroup root (no leading `/sys/fs/cgroup`).
    #[serde(default)]
    cgroup_name: Option<String>,
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Generate a 64-character lowercase hex container/sandbox ID.
///
/// 32 bytes from the OS CSPRNG encoded as hex — identical to the format used
/// by containerd and CRI-O.  The 64-char length is a de facto standard
/// hardcoded in SPIRE, Fluentd, Fluent Bit, OTel, Datadog, Falco, and cAdvisor.
fn generate_id() -> String {
    use std::io::Read;
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .expect("read /dev/urandom for container ID generation");
    bytes.map(|b| format!("{:02x}", b)).concat()
}

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as i64
}

// Linux CLK_TCK is 100 on virtually all architectures (jiffies).
const CLK_TCK: u64 = 100;

/// Read total CPU time in nanoseconds for a container, preferring cgroup
/// accounting over single-PID /proc/stat.
///
/// /proc/{pid}/stat only accounts for one process. Containers with an init
/// wrapper (tini, dumb-init) would show near-zero CPU for the init PID while
/// the real workload runs as a child. Cgroup CPU accounting sums all processes
/// in the container's cgroup subtree, matching what cAdvisor reports.
///
/// Priority:
///  1. cgroup v2: /sys/fs/cgroup/{cgroup_name}/cpu.stat → usage_usec
///  2. cgroup v1: /sys/fs/cgroup/cpuacct/{cgroup_name}/cpuacct.usage
///  3. /proc/{pid}/stat utime+stime (fallback when no cgroup path is known)
fn read_container_cpu_nanos(pid: i32, cgroup_name: Option<&str>) -> u64 {
    if let Some(cg) = cgroup_name {
        // cgroup v2: cpu.stat contains "usage_usec <N>"
        let v2_path = format!("/sys/fs/cgroup/{}/cpu.stat", cg);
        if let Ok(data) = std::fs::read_to_string(&v2_path) {
            for line in data.lines() {
                if let Some(rest) = line.strip_prefix("usage_usec ") {
                    if let Ok(usec) = rest.trim().parse::<u64>() {
                        return usec * 1_000; // microseconds → nanoseconds
                    }
                }
            }
        }
        // cgroup v1: cpuacct.usage is already in nanoseconds
        let v1_path = format!("/sys/fs/cgroup/cpuacct/{}/cpuacct.usage", cg);
        if let Ok(data) = std::fs::read_to_string(&v1_path) {
            if let Ok(nanos) = data.trim().parse::<u64>() {
                return nanos;
            }
        }
    }
    // Fallback: single-process /proc stat (may under-count multi-process containers)
    read_proc_cpu_nanos(pid)
}

fn read_proc_cpu_nanos(pid: i32) -> u64 {
    let path = format!("/proc/{}/stat", pid);
    let Ok(data) = std::fs::read_to_string(path) else {
        return 0;
    };
    // Skip past comm "(name)" — it can contain spaces and parens.
    let after_comm = data.rfind(')').map(|i| &data[i + 2..]).unwrap_or("");
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // After state: ppid(0) … utime(11) stime(12) (0-indexed from state field).
    let utime: u64 = fields.get(11).and_then(|s| s.parse().ok()).unwrap_or(0);
    let stime: u64 = fields.get(12).and_then(|s| s.parse().ok()).unwrap_or(0);
    (utime + stime) * (1_000_000_000 / CLK_TCK)
}

fn read_proc_mem_bytes(pid: i32) -> u64 {
    let path = format!("/proc/{}/status", pid);
    let Ok(data) = std::fs::read_to_string(path) else {
        return 0;
    };
    for line in data.lines() {
        if line.starts_with("VmRSS:") {
            let kb: u64 = line
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            return kb * 1024;
        }
    }
    0
}

/// Read cgroup memory stats for a container.
///
/// Returns `(usage_bytes, working_set_bytes)` where:
/// - `usage_bytes`       = total cgroup memory (all processes, including page cache)
/// - `working_set_bytes` = usage minus reclaimable page cache (inactive_file),
///                         the value metrics-server uses for HPA
///
/// Priority:
///  1. cgroup v2: memory.current minus memory.stat[inactive_file]
///  2. cgroup v1: memory.usage_in_bytes minus memory.stat[total_inactive_file]
///  3. /proc/{pid}/status VmRSS fallback (single process only)
fn read_container_mem_bytes(pid: i32, cgroup_name: Option<&str>) -> (u64, u64) {
    if let Some(cg) = cgroup_name {
        // cgroup v2
        let v2_current = format!("/sys/fs/cgroup/{}/memory.current", cg);
        let v2_stat = format!("/sys/fs/cgroup/{}/memory.stat", cg);
        if let Ok(current_str) = std::fs::read_to_string(&v2_current) {
            if let Ok(usage) = current_str.trim().parse::<u64>() {
                let inactive_file = std::fs::read_to_string(&v2_stat)
                    .unwrap_or_default()
                    .lines()
                    .find(|l| l.starts_with("inactive_file "))
                    .and_then(|l| l["inactive_file ".len()..].trim().parse::<u64>().ok())
                    .unwrap_or(0);
                let working_set = usage.saturating_sub(inactive_file);
                return (usage, working_set);
            }
        }
        // cgroup v1
        let v1_usage = format!("/sys/fs/cgroup/memory/{}/memory.usage_in_bytes", cg);
        let v1_stat = format!("/sys/fs/cgroup/memory/{}/memory.stat", cg);
        if let Ok(usage_str) = std::fs::read_to_string(&v1_usage) {
            if let Ok(usage) = usage_str.trim().parse::<u64>() {
                let inactive_file = std::fs::read_to_string(&v1_stat)
                    .unwrap_or_default()
                    .lines()
                    .find(|l| l.starts_with("total_inactive_file "))
                    .and_then(|l| l["total_inactive_file ".len()..].trim().parse::<u64>().ok())
                    .unwrap_or(0);
                let working_set = usage.saturating_sub(inactive_file);
                return (usage, working_set);
            }
        }
    }
    // Fallback: single-process VmRSS (under-counts multi-process containers)
    let rss = read_proc_mem_bytes(pid);
    (rss, rss)
}

fn build_container_stats(c: &CriContainer) -> ContainerStats {
    let ts = now_ns();
    let state = read_pelagos_container_state(&c.pelagos_name);
    let pid = state.as_ref().map(|s| s.pid).unwrap_or(0);
    let cgroup_name = state.as_ref().and_then(|s| s.cgroup_name.as_deref());
    let (cpu_nanos, mem_usage, mem_working_set) = if pid > 0 || cgroup_name.is_some() {
        let cpu = read_container_cpu_nanos(pid, cgroup_name);
        let (usage, working_set) = read_container_mem_bytes(pid, cgroup_name);
        (cpu, usage, working_set)
    } else {
        (0, 0, 0)
    };
    ContainerStats {
        attributes: Some(ContainerAttributes {
            id: c.id.clone(),
            metadata: Some(ContainerMetadata {
                name: c.name.clone(),
                attempt: 0,
            }),
            labels: c.labels.clone(),
            annotations: c.annotations.clone(),
        }),
        cpu: Some(CpuUsage {
            timestamp: ts,
            usage_core_nano_seconds: Some(UInt64Value { value: cpu_nanos }),
            usage_nano_cores: Some(UInt64Value { value: 0 }),
            psi: None,
        }),
        memory: Some(MemoryUsage {
            timestamp: ts,
            working_set_bytes: Some(UInt64Value {
                value: mem_working_set,
            }),
            available_bytes: None,
            usage_bytes: Some(UInt64Value { value: mem_usage }),
            rss_bytes: None,
            page_faults: None,
            major_page_faults: None,
            psi: None,
        }),
        writable_layer: Some(FilesystemUsage {
            timestamp: ts,
            fs_id: Some(FilesystemIdentifier {
                mountpoint: "/var/lib/pelagos".to_string(),
            }),
            used_bytes: Some(UInt64Value { value: 0 }),
            inodes_used: Some(UInt64Value { value: 0 }),
        }),
        swap: None,
        io: None,
    }
}

fn build_sandbox_stats(sb: &CriSandbox, containers: &[CriContainer]) -> PodSandboxStats {
    let ts = now_ns();
    // Aggregate CPU and memory across all app containers in this pod.
    // The pause process is a namespace holder that is essentially idle;
    // reading its stats would report near-zero for the entire pod.
    let pod_containers: Vec<&CriContainer> = containers
        .iter()
        .filter(|c| c.sandbox_id == sb.id)
        .collect();
    let (cpu_nanos, mem_usage, mem_working_set) =
        pod_containers
            .iter()
            .fold((0u64, 0u64, 0u64), |(cpu, usage, ws), c| {
                let state = read_pelagos_container_state(&c.pelagos_name);
                let pid = state.as_ref().map(|s| s.pid).unwrap_or(0);
                let cgroup_name = state.as_ref().and_then(|s| s.cgroup_name.as_deref());
                if pid > 0 || cgroup_name.is_some() {
                    let (mu, mws) = read_container_mem_bytes(pid, cgroup_name);
                    (
                        cpu.saturating_add(read_container_cpu_nanos(pid, cgroup_name)),
                        usage.saturating_add(mu),
                        ws.saturating_add(mws),
                    )
                } else {
                    (cpu, usage, ws)
                }
            });
    PodSandboxStats {
        attributes: Some(PodSandboxAttributes {
            id: sb.id.clone(),
            metadata: Some(PodSandboxMetadata {
                name: sb.name.clone(),
                namespace: sb.namespace.clone(),
                attempt: 0,
                uid: sb.uid.clone(),
            }),
            labels: sb.labels.clone(),
            annotations: sb.annotations.clone(),
        }),
        linux: Some(LinuxPodSandboxStats {
            cpu: Some(CpuUsage {
                timestamp: ts,
                usage_core_nano_seconds: Some(UInt64Value { value: cpu_nanos }),
                usage_nano_cores: Some(UInt64Value { value: 0 }),
                psi: None,
            }),
            memory: Some(MemoryUsage {
                timestamp: ts,
                working_set_bytes: Some(UInt64Value {
                    value: mem_working_set,
                }),
                available_bytes: None,
                usage_bytes: Some(UInt64Value { value: mem_usage }),
                rss_bytes: None,
                page_faults: None,
                major_page_faults: None,
                psi: None,
            }),
            network: None,
            process: None,
            containers: pod_containers
                .iter()
                .map(|c| build_container_stats(c))
                .collect(),
            io: None,
        }),
        windows: None,
    }
}

fn read_pelagos_sandbox_ip(sandbox_id: &str) -> Option<String> {
    let path = format!("{}/{}/state.json", PELAGOS_SANDBOXES_DIR, sandbox_id);
    let data = std::fs::read_to_string(&path).ok()?;
    let st: PelagosSandboxState = serde_json::from_str(&data).ok()?;
    Some(st.container_ip)
}

fn read_pelagos_container_state(pelagos_name: &str) -> Option<PelagosContainerState> {
    let path = format!("{}/{}/state.json", PELAGOS_CONTAINERS_DIR, pelagos_name);
    let data = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&data).ok()
}

fn cri_sandbox_state_val(s: &SandboxState) -> i32 {
    match s {
        SandboxState::Running => PodSandboxState::SandboxReady as i32,
        SandboxState::NotReady => PodSandboxState::SandboxNotready as i32,
    }
}

fn cri_container_state_val(s: &MyContainerState) -> i32 {
    match s {
        MyContainerState::Created => CriContainerStateEnum::ContainerCreated as i32,
        MyContainerState::Running => CriContainerStateEnum::ContainerRunning as i32,
        MyContainerState::Exited => CriContainerStateEnum::ContainerExited as i32,
        MyContainerState::Unknown => CriContainerStateEnum::ContainerUnknown as i32,
    }
}

/// Convert a CRI Signal proto enum value to a signal name string for `pelagos --stop-signal`.
/// Returns empty string for RUNTIME_DEFAULT (0) meaning "use runtime default (SIGTERM)".
fn cri_signal_to_name(sig: i32) -> String {
    match sig {
        0 => String::new(), // RUNTIME_DEFAULT — no override
        1 => "SIGABRT".into(),
        2 => "SIGALRM".into(),
        3 => "SIGBUS".into(),
        4 => "SIGCHLD".into(),
        5 => "SIGCLD".into(),
        6 => "SIGCONT".into(),
        7 => "SIGFPE".into(),
        8 => "SIGHUP".into(),
        9 => "SIGILL".into(),
        10 => "SIGINT".into(),
        11 => "SIGIO".into(),
        12 => "SIGIOT".into(),
        13 => "SIGKILL".into(),
        14 => "SIGPIPE".into(),
        15 => "SIGPOLL".into(),
        16 => "SIGPROF".into(),
        17 => "SIGPWR".into(),
        18 => "SIGQUIT".into(),
        19 => "SIGSEGV".into(),
        20 => "SIGSTKFLT".into(),
        21 => "SIGSTOP".into(),
        22 => "SIGSYS".into(),
        23 => "SIGTERM".into(),
        24 => "SIGTRAP".into(),
        25 => "SIGTSTP".into(),
        26 => "SIGTTIN".into(),
        27 => "SIGTTOU".into(),
        28 => "SIGURG".into(),
        29 => "SIGUSR1".into(),
        30 => "SIGUSR2".into(),
        31 => "SIGVTALRM".into(),
        32 => "SIGWINCH".into(),
        33 => "SIGXCPU".into(),
        34 => "SIGXFSZ".into(),
        n => format!("{}", n), // numeric fallback; parse_signal handles numeric strings
    }
}

fn labels_match(
    container_labels: &HashMap<String, String>,
    selector: &HashMap<String, String>,
) -> bool {
    selector
        .iter()
        .all(|(k, v)| container_labels.get(k).map(|val| val == v).unwrap_or(false))
}

fn sandbox_to_proto(s: &CriSandbox) -> PodSandbox {
    PodSandbox {
        id: s.id.clone(),
        metadata: Some(PodSandboxMetadata {
            name: s.name.clone(),
            uid: s.uid.clone(),
            namespace: s.namespace.clone(),
            attempt: s.attempt,
        }),
        state: cri_sandbox_state_val(&s.state),
        created_at: s.created_at_ns,
        labels: s.labels.clone(),
        annotations: s.annotations.clone(),
        runtime_handler: String::new(),
    }
}

fn container_to_proto(c: &CriContainer) -> crate::cri::Container {
    crate::cri::Container {
        id: c.id.clone(),
        pod_sandbox_id: c.sandbox_id.clone(),
        metadata: Some(ContainerMetadata {
            name: c.name.clone(),
            attempt: 0,
        }),
        image: Some(ImageSpec {
            image: c.image.clone(),
            annotations: HashMap::new(),
            ..Default::default()
        }),
        image_ref: c.image.clone(),
        state: cri_container_state_val(&c.state),
        created_at: c.created_at_ns,
        labels: c.labels.clone(),
        annotations: c.annotations.clone(),
        image_id: c.image.clone(),
    }
}

// ── RuntimeSvc ───────────────────────────────────────────────────────────────

pub struct RuntimeSvc {
    pub state: AppState,
    pub streaming_base_url: String,
    pub registry: Registry,
}

impl RuntimeSvc {
    async fn bin(&self) -> String {
        self.state.inner.lock().await.pelagos_bin.clone()
    }

    /// Resolve a digest-form image ref (sha256:...) to a repo tag by scanning the image store.
    /// Kubelet may pass the digest it received from ImageStatus rather than the original tag.
    async fn resolve_image_ref(image_ref: &str) -> String {
        if !image_ref.starts_with("sha256:") {
            return image_ref.to_string();
        }
        let Ok(mut rd) = tokio::fs::read_dir("/var/lib/pelagos/images").await else {
            return image_ref.to_string();
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            let manifest_path = entry.path().join("manifest.json");
            if let Ok(data) = tokio::fs::read_to_string(&manifest_path).await {
                if let Ok(m) = serde_json::from_str::<serde_json::Value>(&data) {
                    if m["digest"].as_str() == Some(image_ref) {
                        if let Some(tag) = m["reference"].as_str() {
                            return tag.to_string();
                        }
                    }
                }
            }
        }
        image_ref.to_string()
    }

    /// Load the default ENTRYPOINT and CMD from the stored image manifest.
    /// Returns `(entrypoint, cmd)` as string vecs; empty if the manifest can't be read.
    async fn load_image_defaults(image_ref: &str) -> (Vec<String>, Vec<String>) {
        let Ok(mut rd) = tokio::fs::read_dir("/var/lib/pelagos/images").await else {
            return (vec![], vec![]);
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            let manifest_path = entry.path().join("manifest.json");
            let Ok(data) = tokio::fs::read_to_string(&manifest_path).await else {
                continue;
            };
            let Ok(m) = serde_json::from_str::<serde_json::Value>(&data) else {
                continue;
            };
            if m["reference"].as_str() == Some(image_ref) || m["digest"].as_str() == Some(image_ref)
            {
                let ep = m["config"]["entrypoint"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let cmd = m["config"]["cmd"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                return (ep, cmd);
            }
        }
        (vec![], vec![])
    }
}

// ── Trait impl ───────────────────────────────────────────────────────────────

#[tonic::async_trait]
impl RuntimeService for RuntimeSvc {
    async fn version(
        &self,
        _request: Request<VersionRequest>,
    ) -> Result<Response<VersionResponse>, Status> {
        Ok(Response::new(VersionResponse {
            version: "0.1.0".into(),
            runtime_name: "pelagos".into(),
            runtime_version: env!("CARGO_PKG_VERSION").to_string(),
            runtime_api_version: "v1".into(),
        }))
    }

    async fn status(
        &self,
        _request: Request<StatusRequest>,
    ) -> Result<Response<StatusResponse>, Status> {
        Ok(Response::new(StatusResponse {
            status: Some(RuntimeStatus {
                conditions: vec![
                    RuntimeCondition {
                        r#type: "RuntimeReady".into(),
                        status: true,
                        reason: String::new(),
                        message: String::new(),
                    },
                    RuntimeCondition {
                        r#type: "NetworkReady".into(),
                        status: true,
                        reason: String::new(),
                        message: String::new(),
                    },
                ],
            }),
            info: HashMap::new(),
            runtime_handlers: vec![],
            features: None,
        }))
    }

    async fn run_pod_sandbox(
        &self,
        request: Request<crate::cri::RunPodSandboxRequest>,
    ) -> Result<Response<crate::cri::RunPodSandboxResponse>, Status> {
        let req = request.into_inner();
        let config = req
            .config
            .ok_or_else(|| Status::invalid_argument("missing config"))?;
        let meta = config
            .metadata
            .ok_or_else(|| Status::invalid_argument("missing metadata"))?;

        let uid = meta.uid.clone();
        let bin = self.bin().await;

        // Try CNI networking first; fall back to pelagos native if no config is present.
        let (sandbox_id, netns, ip, cni_conf, pause_pid) = if let Some(conf_path) =
            cni::find_cni_conf()
        {
            // ── CNI path ───────────────────────────────────────────────────
            let id = generate_id();
            let ns_name = format!("pcri-{}", &id[..12]);

            let netns_path = cni::create_netns(&ns_name)
                .map_err(|e| Status::internal(format!("create netns for CNI sandbox: {}", e)))?;

            let ip = match cni::cni_add(&id, &netns_path, &conf_path) {
                Ok(ip) => ip,
                Err(e) => {
                    cni::delete_netns(&ns_name);
                    return Err(Status::internal(format!("CNI ADD: {}", e)));
                }
            };

            let net_arg = format!("--net={}", netns_path);

            // Bring the loopback interface up in the sandbox netns.  The CNI
            // spec requires a loopback plugin in the chain, but k3s/Flannel
            // omit it.  Without this, lo stays DOWN and all intra-pod 127.0.0.1
            // traffic fails — breaking the sidecar pattern (issue #331).
            match tokio::process::Command::new("nsenter")
                .args([net_arg.as_str(), "--", "ip", "link", "set", "lo", "up"])
                .output()
                .await
            {
                Ok(out) if !out.status.success() => {
                    log::warn!(
                        "bring lo up in {}: {}",
                        ns_name,
                        String::from_utf8_lossy(&out.stderr).trim()
                    );
                }
                Err(e) => log::warn!("nsenter lo up in {}: {}", ns_name, e),
                _ => {}
            }

            // Allow non-root container processes (e.g. coredns running as "nonroot") to
            // bind privileged ports inside this netns.  This matches containerd's default.
            match tokio::process::Command::new("nsenter")
                .args([
                    net_arg.as_str(),
                    "--",
                    "sysctl",
                    "-w",
                    "net.ipv4.ip_unprivileged_port_start=0",
                ])
                .output()
                .await
            {
                Ok(out) if !out.status.success() => {
                    log::warn!(
                        "sysctl ip_unprivileged_port_start in {}: {}",
                        ns_name,
                        String::from_utf8_lossy(&out.stderr).trim()
                    );
                }
                Err(e) => log::warn!("nsenter sysctl in {}: {}", ns_name, e),
                _ => {}
            }

            // Spawn pause process: joins the CNI-configured netns, unshares IPC+UTS.
            // We re-use pelagos's own `sandbox __pause__ <ns_name>` subcommand.
            let pause = std::process::Command::new(&bin)
                .args(["sandbox", "__pause__", &ns_name])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .map_err(|e| Status::internal(format!("spawn pause process: {}", e)))?;
            let pause_pid = pause.id() as i32;
            // Intentionally leaked — killed explicitly on StopPodSandbox.
            std::mem::forget(pause);

            // Brief pause for the process to enter namespaces before containers join.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;

            // Write pelagos-format sandbox state so `pelagos run --sandbox` works.
            state::write_pelagos_sandbox_state(&id, Some(&meta.name), pause_pid, &ns_name, &ip)
                .map_err(|e| Status::internal(format!("write sandbox state: {}", e)))?;

            log::info!(
                "CNI sandbox {} created: netns={} ip={} conf={}",
                id,
                ns_name,
                ip,
                conf_path.display()
            );

            (
                id,
                ns_name,
                ip,
                conf_path.to_string_lossy().to_string(),
                pause_pid,
            )
        } else {
            // ── Pelagos native path ────────────────────────────────────────
            log::info!("no CNI config found — using pelagos native bridge networking");
            let raw = run_pelagos(&bin, &["sandbox", "create", "--name", &uid])
                .await
                .map_err(|e| Status::internal(format!("exec error: {}", e)))?;
            if !raw.success {
                return Err(Status::internal(format!(
                    "sandbox create failed: {}",
                    raw.stderr
                )));
            }
            let sandbox_id = raw.stdout.trim().to_string();
            let ip = read_pelagos_sandbox_ip(&sandbox_id).unwrap_or_default();
            (sandbox_id, String::new(), ip, String::new(), 0)
        };

        let sandbox = CriSandbox {
            id: sandbox_id.clone(),
            name: meta.name.clone(),
            namespace: meta.namespace.clone(),
            uid,
            attempt: meta.attempt,
            labels: config.labels.clone(),
            annotations: config.annotations.clone(),
            created_at_ns: now_ns(),
            state: SandboxState::Running,
            netns,
            ip,
            cni_conf,
            pause_pid,
            log_directory: config.log_directory.clone(),
            cgroup_parent: config
                .linux
                .as_ref()
                .map(|l| l.cgroup_parent.trim_start_matches('/').to_owned())
                .unwrap_or_default(),
            supplemental_groups: config
                .linux
                .as_ref()
                .and_then(|l| l.security_context.as_ref())
                .map(|sc| sc.supplemental_groups.clone())
                .unwrap_or_default(),
            dns_servers: config
                .dns_config
                .as_ref()
                .map(|d| d.servers.clone())
                .unwrap_or_default(),
            dns_searches: config
                .dns_config
                .as_ref()
                .map(|d| d.searches.clone())
                .unwrap_or_default(),
            dns_options: config
                .dns_config
                .as_ref()
                .map(|d| d.options.clone())
                .unwrap_or_default(),
            pid_namespace_mode: config
                .linux
                .as_ref()
                .and_then(|l| l.security_context.as_ref())
                .and_then(|sc| sc.namespace_options.as_ref())
                .map(|no| no.pid)
                .unwrap_or(0),
            ipc_namespace_mode: config
                .linux
                .as_ref()
                .and_then(|l| l.security_context.as_ref())
                .and_then(|sc| sc.namespace_options.as_ref())
                .map(|no| no.ipc)
                .unwrap_or(0),
        };

        {
            let mut st = self.state.inner.lock().await;
            state::save_sandbox(&sandbox)
                .map_err(|e| Status::internal(format!("save sandbox: {}", e)))?;
            st.sandboxes.insert(sandbox_id.clone(), sandbox);
        }

        Ok(Response::new(crate::cri::RunPodSandboxResponse {
            pod_sandbox_id: sandbox_id,
        }))
    }

    async fn stop_pod_sandbox(
        &self,
        request: Request<StopPodSandboxRequest>,
    ) -> Result<Response<StopPodSandboxResponse>, Status> {
        let sandbox_id = request.into_inner().pod_sandbox_id;
        let bin = self.bin().await;

        let containers_to_stop: Vec<(String, MyContainerState)> = {
            let st = self.state.inner.lock().await;
            st.containers
                .values()
                .filter(|c| c.sandbox_id == sandbox_id)
                .map(|c| (c.pelagos_name.clone(), c.state.clone()))
                .collect()
        };

        for (pelagos_name, cstate) in &containers_to_stop {
            if *cstate == MyContainerState::Running {
                let _ = run_pelagos(&bin, &["stop", pelagos_name]).await;
            }
        }

        let sandbox = {
            let st = self.state.inner.lock().await;
            st.sandboxes.get(&sandbox_id).cloned()
        };

        if let Some(ref sb) = sandbox {
            if !sb.netns.is_empty() {
                // ── CNI teardown ───────────────────────────────────────────────
                let netns_path = format!("/run/netns/{}", sb.netns);
                if !sb.cni_conf.is_empty() {
                    cni::cni_del(&sandbox_id, &netns_path, std::path::Path::new(&sb.cni_conf));
                }
                if sb.pause_pid > 0 {
                    let _ = std::process::Command::new("kill")
                        .args(["-TERM", &sb.pause_pid.to_string()])
                        .output();
                }
                cni::delete_netns(&sb.netns);
                state::remove_pelagos_sandbox_state(&sandbox_id);
            } else {
                // ── Pelagos native teardown ────────────────────────────────────
                let _ = run_pelagos(&bin, &["sandbox", "rm", &sandbox_id]).await;
            }
        }

        let mut st = self.state.inner.lock().await;
        if let Some(s) = st.sandboxes.get_mut(&sandbox_id) {
            s.state = SandboxState::NotReady;
            let _ = state::save_sandbox(s);
        }

        Ok(Response::new(StopPodSandboxResponse {}))
    }

    async fn remove_pod_sandbox(
        &self,
        request: Request<RemovePodSandboxRequest>,
    ) -> Result<Response<RemovePodSandboxResponse>, Status> {
        let sandbox_id = request.into_inner().pod_sandbox_id;

        let mut st = self.state.inner.lock().await;
        let container_ids: Vec<String> = st
            .containers
            .values()
            .filter(|c| c.sandbox_id == sandbox_id)
            .map(|c| c.id.clone())
            .collect();

        for cid in container_ids {
            st.containers.remove(&cid);
            state::remove_container_file(&cid);
        }

        st.sandboxes.remove(&sandbox_id);
        state::remove_sandbox_file(&sandbox_id);

        Ok(Response::new(RemovePodSandboxResponse {}))
    }

    async fn pod_sandbox_status(
        &self,
        request: Request<PodSandboxStatusRequest>,
    ) -> Result<Response<PodSandboxStatusResponse>, Status> {
        let sandbox_id = request.into_inner().pod_sandbox_id;

        let st = self.state.inner.lock().await;
        let sandbox = st
            .sandboxes
            .get(&sandbox_id)
            .ok_or_else(|| Status::not_found("sandbox not found"))?
            .clone();
        drop(st);

        // For CNI sandboxes, ip is stored directly in CriSandbox.
        // For native sandboxes, fall back to reading the pelagos state file.
        let ip = if sandbox.ip.is_empty() {
            read_pelagos_sandbox_ip(&sandbox_id).unwrap_or_default()
        } else {
            sandbox.ip.clone()
        };

        let status = CriPodSandboxStatus {
            id: sandbox.id.clone(),
            metadata: Some(PodSandboxMetadata {
                name: sandbox.name.clone(),
                uid: sandbox.uid.clone(),
                namespace: sandbox.namespace.clone(),
                attempt: sandbox.attempt,
            }),
            state: cri_sandbox_state_val(&sandbox.state),
            created_at: sandbox.created_at_ns,
            network: Some(PodSandboxNetworkStatus {
                ip: ip.clone(),
                additional_ips: vec![],
            }),
            linux: Some(LinuxPodSandboxStatus {
                namespaces: Some(Namespace {
                    options: Some(NamespaceOption {
                        network: 0,
                        pid: 0,
                        ipc: 0,
                        target_id: String::new(),
                        userns_options: None,
                    }),
                }),
            }),
            labels: sandbox.labels.clone(),
            annotations: sandbox.annotations.clone(),
            runtime_handler: String::new(),
        };

        Ok(Response::new(PodSandboxStatusResponse {
            status: Some(status),
            info: HashMap::new(),
            containers_statuses: vec![],
            timestamp: now_ns(),
        }))
    }

    async fn list_pod_sandbox(
        &self,
        request: Request<ListPodSandboxRequest>,
    ) -> Result<Response<ListPodSandboxResponse>, Status> {
        let filter = request.into_inner().filter;
        let st = self.state.inner.lock().await;

        let items: Vec<PodSandbox> = st
            .sandboxes
            .values()
            .filter(|s| {
                if let Some(ref f) = filter {
                    if !f.id.is_empty() && s.id != f.id {
                        return false;
                    }
                    if let Some(ref sv) = f.state {
                        let want = sv.state;
                        let have = cri_sandbox_state_val(&s.state);
                        if want != have {
                            return false;
                        }
                    }
                    if !labels_match(&s.labels, &f.label_selector) {
                        return false;
                    }
                }
                true
            })
            .map(sandbox_to_proto)
            .collect();

        Ok(Response::new(ListPodSandboxResponse { items }))
    }

    async fn create_container(
        &self,
        request: Request<CreateContainerRequest>,
    ) -> Result<Response<CreateContainerResponse>, Status> {
        let req = request.into_inner();
        let sandbox_id = req.pod_sandbox_id.clone();
        let config = req
            .config
            .ok_or_else(|| Status::invalid_argument("missing container config"))?;

        let meta = config
            .metadata
            .ok_or_else(|| Status::invalid_argument("missing container metadata"))?;

        let image_ref = config.image.map(|s| s.image).unwrap_or_default();

        let id = generate_id();
        let pelagos_name = format!("pcri-{}", &id[..12]);

        let envs: Vec<(String, String)> = config
            .envs
            .iter()
            .map(|kv| {
                (
                    kv.key.clone(),
                    String::from_utf8_lossy(&kv.value).into_owned(),
                )
            })
            .collect();

        let mounts: Vec<CriMount> = config
            .mounts
            .iter()
            .map(|m| CriMount {
                host_path: m.host_path.clone(),
                container_path: m.container_path.clone(),
                readonly: m.readonly,
            })
            .collect();

        // Extract runAsUser/runAsGroup from the Linux security context.
        let (run_as_user, run_as_group) = config
            .linux
            .as_ref()
            .and_then(|l| l.security_context.as_ref())
            .map(|sc| {
                (
                    sc.run_as_user.as_ref().map(|v| v.value),
                    sc.run_as_group.as_ref().map(|v| v.value),
                )
            })
            .unwrap_or((None, None));

        // B′ — UID overflow/validity guard.
        // Valid Linux UIDs are 0..=4294967294. The value 4294967295 (u32::MAX) equals
        // (uid_t)-1; setuid(-1) is a no-op on some paths, silently leaving the process
        // as root. This is the vector behind CVE-2024-40635 / CVE-2026-46680.
        // Negative proto values are also nonsensical — reject them too.
        for (label, maybe_id) in [("run_as_user", run_as_user), ("run_as_group", run_as_group)] {
            if let Some(id) = maybe_id {
                if !(0..=4_294_967_294).contains(&id) {
                    return Err(Status::invalid_argument(format!(
                        "{label} value {id} is out of the valid Linux UID/GID range (0..=4294967294)"
                    )));
                }
            }
        }

        // Extract security context fields from LinuxContainerSecurityContext.
        let (
            cap_add,
            cap_drop,
            privileged,
            read_only_rootfs,
            no_new_privs,
            masked_paths,
            readonly_paths,
            seccomp_profile_type,
            seccomp_profile_path,
        ) = config
            .linux
            .as_ref()
            .and_then(|l| l.security_context.as_ref())
            .map(|sc| {
                let (add, drop) = sc
                    .capabilities
                    .as_ref()
                    .map(|c| (c.add_capabilities.clone(), c.drop_capabilities.clone()))
                    .unwrap_or_default();
                let (sec_type, sec_path) = sc
                    .seccomp
                    .as_ref()
                    .map(|s| (s.profile_type, s.localhost_ref.clone()))
                    .unwrap_or((0, String::new()));
                (
                    add,
                    drop,
                    sc.privileged,
                    sc.readonly_rootfs,
                    sc.no_new_privs,
                    sc.masked_paths.clone(),
                    sc.readonly_paths.clone(),
                    sec_type,
                    sec_path,
                )
            })
            .unwrap_or_default();

        // Extract resource limits from LinuxContainerConfig.
        // oom_score_adj is int64 in the proto but the kernel range is -1000..1000 (fits i32).
        // Treat proto value 0 as "not set" (kernel default); use i32::MIN as our sentinel.
        let (memory_limit, cpu_period, cpu_quota, cpu_shares, oom_score_adj, memory_swap_limit) =
            config
                .linux
                .as_ref()
                .and_then(|l| l.resources.as_ref())
                .map(|r| {
                    let oom: i32 = if r.oom_score_adj == 0 {
                        i32::MIN // not set; keep kernel default
                    } else {
                        r.oom_score_adj.clamp(-1000, 1000) as i32
                    };
                    (
                        r.memory_limit_in_bytes,
                        r.cpu_period,
                        r.cpu_quota,
                        r.cpu_shares,
                        oom,
                        r.memory_swap_limit_in_bytes,
                    )
                })
                .unwrap_or((0, 0, 0, 0, i32::MIN, 0));

        // Extract AppArmor profile from LinuxContainerSecurityContext.
        let (apparmor_profile_type, apparmor_profile_path) = config
            .linux
            .as_ref()
            .and_then(|l| l.security_context.as_ref())
            .and_then(|sc| sc.apparmor.as_ref())
            .map(|a| (a.profile_type, a.localhost_ref.clone()))
            .unwrap_or((0, String::new()));

        // Extract SELinux options from LinuxContainerSecurityContext.
        let selinux_label = config
            .linux
            .as_ref()
            .and_then(|l| l.security_context.as_ref())
            .and_then(|sc| sc.selinux_options.as_ref())
            .map(|s| {
                // Combine user:role:type:level into a single label string.
                format!("{}:{}:{}:{}", s.user, s.role, s.r#type, s.level)
            })
            .filter(|s| s != ":::")
            .unwrap_or_default();

        // Extract cpuset and hugepage limits from LinuxContainerResources.
        let (cpuset_cpus, cpuset_mems, hugepage_limits) = config
            .linux
            .as_ref()
            .and_then(|l| l.resources.as_ref())
            .map(|r| {
                let hugepages = r
                    .hugepage_limits
                    .iter()
                    .map(|h| (h.page_size.clone(), h.limit))
                    .collect::<Vec<_>>();
                (r.cpuset_cpus.clone(), r.cpuset_mems.clone(), hugepages)
            })
            .unwrap_or_default();

        // Extract stop signal from ContainerConfig.
        // config.stop_signal is the Signal proto enum (i32); 0 = RUNTIME_DEFAULT (no override).
        let stop_signal = cri_signal_to_name(config.stop_signal);

        // Identify the termination log mount.  Kubelet passes terminationMessagePath
        // (default /dev/termination-log) as a regular bind mount; after the container
        // exits we read that host-side file and return it as ContainerStatus.message.
        let termination_msg_container_path = config
            .annotations
            .get("io.kubernetes.container.terminationMessagePath")
            .map(|s| s.as_str())
            .unwrap_or("/dev/termination-log");
        let termination_log_host_path = config
            .mounts
            .iter()
            .find(|m| m.container_path == termination_msg_container_path)
            .map(|m| m.host_path.clone())
            .unwrap_or_default();

        let container = CriContainer {
            id: id.clone(),
            sandbox_id,
            pelagos_name,
            name: meta.name.clone(),
            image: image_ref,
            entrypoint: config.command.clone(),
            args: config.args.clone(),
            envs,
            working_dir: config.working_dir.clone(),
            mounts,
            labels: config.labels.clone(),
            annotations: config.annotations.clone(),
            created_at_ns: now_ns(),
            started_at_ns: 0,
            finished_at_ns: 0,
            state: MyContainerState::Created,
            exit_code: 0,
            run_as_user,
            run_as_group,
            termination_log_host_path,
            log_path: config.log_path.clone(),
            supplemental_groups: config
                .linux
                .as_ref()
                .and_then(|l| l.security_context.as_ref())
                .map(|sc| sc.supplemental_groups.clone())
                .unwrap_or_default(),
            cap_add,
            cap_drop,
            privileged,
            memory_limit,
            cpu_period,
            cpu_quota,
            cpu_shares,
            read_only_rootfs,
            seccomp_profile_type,
            seccomp_profile_path,
            no_new_privs,
            masked_paths,
            readonly_paths,
            apparmor_profile_type,
            apparmor_profile_path,
            oom_score_adj,
            memory_swap_limit,
            cpuset_cpus,
            cpuset_mems,
            stop_signal,
            hugepage_limits,
            selinux_label,
        };

        {
            let mut st = self.state.inner.lock().await;
            state::save_container(&container)
                .map_err(|e| Status::internal(format!("save container: {}", e)))?;
            st.containers.insert(id.clone(), container);
        }

        Ok(Response::new(CreateContainerResponse { container_id: id }))
    }

    async fn start_container(
        &self,
        request: Request<StartContainerRequest>,
    ) -> Result<Response<StartContainerResponse>, Status> {
        let container_id = request.into_inner().container_id;
        let bin = self.bin().await;

        let container = {
            let st = self.state.inner.lock().await;
            st.containers
                .get(&container_id)
                .cloned()
                .ok_or_else(|| Status::not_found("container not found"))?
        };

        let mut args: Vec<String> = vec![
            "run".into(),
            "--name".into(),
            container.pelagos_name.clone(),
            "--detach".into(),
            "--sandbox".into(),
            container.sandbox_id.clone(),
        ];

        for (k, v) in &container.envs {
            args.push("--env".into());
            args.push(format!("{}={}", k, v));
        }

        if !container.working_dir.is_empty() {
            args.push("--workdir".into());
            args.push(container.working_dir.clone());
        }

        for m in &container.mounts {
            args.push("-v".into());
            if m.readonly {
                args.push(format!("{}:{}:ro", m.host_path, m.container_path));
            } else {
                args.push(format!("{}:{}", m.host_path, m.container_path));
            }
        }

        // Kubelet may pass the sha256 digest form rather than the tag; resolve to a known tag.
        let image = Self::resolve_image_ref(&container.image).await;

        // CRI entrypoint semantics:
        //   container.entrypoint (CRI "command") overrides the image ENTRYPOINT when non-empty.
        //   container.args (CRI "args") overrides the image CMD when non-empty.
        //   When entrypoint is empty the image's default ENTRYPOINT must be used — omitting it
        //   would cause `pelagos run` to exec the first arg as the binary, which is wrong.
        let (image_entrypoint, image_cmd) = Self::load_image_defaults(&image).await;
        let effective_entrypoint: Vec<String> = if !container.entrypoint.is_empty() {
            container.entrypoint.clone()
        } else {
            image_entrypoint
        };
        let effective_cmd: Vec<String> = if !container.args.is_empty() {
            container.args.clone()
        } else {
            image_cmd
        };

        // B″ — Effective-UID-is-zero audit log.
        // If the container will run as root and is not explicitly privileged, warn.
        // We cannot know the pod's runAsNonRoot intent (the CRI proto doesn't carry it),
        // but an unexpected effective UID 0 is worth surfacing for misconfiguration diagnosis.
        if !container.privileged {
            let effective_uid = container.run_as_user.unwrap_or(0);
            if effective_uid == 0 {
                log::warn!(
                    "container {} ({}) will run as UID 0 (root) without privileged flag; \
                     verify this is intentional",
                    container.id,
                    container.name
                );
            }
        }

        // If the pod securityContext specifies runAsUser, override the image default user.
        // This is required for projected volume permissions (e.g. serviceaccount tokens
        // are written with the fsGroup/runAsUser UID and readable only by that UID).
        if let Some(uid) = container.run_as_user {
            match container.run_as_group {
                Some(gid) => {
                    args.push("--user".into());
                    args.push(format!("{}:{}", uid, gid));
                }
                None => {
                    args.push("--user".into());
                    args.push(uid.to_string());
                }
            }
        }

        // Supplemental groups: merge sandbox-level (fsGroup) and container-level groups.
        let sandbox_supplemental_groups: Vec<i64> = {
            let st = self.state.inner.lock().await;
            st.sandboxes
                .get(&container.sandbox_id)
                .map(|s| s.supplemental_groups.clone())
                .unwrap_or_default()
        };
        let mut all_supp_groups: Vec<i64> = sandbox_supplemental_groups;
        for g in &container.supplemental_groups {
            if !all_supp_groups.contains(g) {
                all_supp_groups.push(*g);
            }
        }
        for g in all_supp_groups {
            args.push("--group-add".into());
            args.push(g.to_string());
        }

        // DNS config, cgroup placement, and PID namespace mode from the pod sandbox.
        let (
            sandbox_dns_servers,
            sandbox_dns_searches,
            sandbox_dns_options,
            sandbox_cgroup_parent,
            sandbox_pid_ns_mode,
        ) = {
            let st = self.state.inner.lock().await;
            st.sandboxes
                .get(&container.sandbox_id)
                .map(|s| {
                    (
                        s.dns_servers.clone(),
                        s.dns_searches.clone(),
                        s.dns_options.clone(),
                        s.cgroup_parent.clone(),
                        s.pid_namespace_mode,
                    )
                })
                .unwrap_or_default()
        };
        for server in &sandbox_dns_servers {
            args.push("--dns".into());
            args.push(server.clone());
        }
        for domain in &sandbox_dns_searches {
            args.push("--dns-search".into());
            args.push(domain.clone());
        }
        for opt in &sandbox_dns_options {
            args.push("--dns-option".into());
            args.push(opt.clone());
        }
        // NODE PID namespace mode (hostPID: true): run in the host PID namespace so
        // the SPIRE agent can attest workloads via SO_PEERCRED (PID 0 is never returned).
        if sandbox_pid_ns_mode == 2 {
            args.push("--no-pid-ns".into());
        }

        // Place the container in the kubelet-assigned cgroup under the pod-level
        // parent so that /proc/<pid>/cgroup encodes the container ID, enabling
        // SPIRE workload attestation and per-container resource accounting.
        if !sandbox_cgroup_parent.is_empty() {
            let cgroup_path = format!("{}/{}", sandbox_cgroup_parent, container_id);
            args.push("--cgroup-path".into());
            args.push(cgroup_path);
        }

        // Privileged mode (securityContext.privileged = true).
        if container.privileged {
            args.push("--privileged".into());
        }

        // Capabilities from CRI LinuxContainerSecurityContext.capabilities.
        for cap in &container.cap_add {
            args.push("--cap-add".into());
            args.push(cap.clone());
        }
        for cap in &container.cap_drop {
            args.push("--cap-drop".into());
            args.push(cap.clone());
        }

        // Resource limits from CRI LinuxContainerResources.
        if container.memory_limit > 0 {
            args.push("--memory".into());
            args.push(container.memory_limit.to_string());
        }
        if container.cpu_quota > 0 && container.cpu_period > 0 {
            // pelagos --cpus accepts a float: quota_us / period_us.
            let cpus = container.cpu_quota as f64 / container.cpu_period as f64;
            args.push("--cpus".into());
            args.push(format!("{:.6}", cpus));
        }
        if container.cpu_shares > 0 {
            args.push("--cpu-shares".into());
            args.push(container.cpu_shares.to_string());
        }

        // Read-only rootfs (securityContext.readOnlyRootFilesystem).
        if container.read_only_rootfs {
            args.push("--read-only".into());
        }

        // No-new-privileges (securityContext.noNewPrivs).
        if container.no_new_privs {
            args.push("--security-opt".into());
            args.push("no-new-privileges".into());
        }

        // Seccomp profile (securityContext.seccomp).
        // Profile type: 0=RuntimeDefault, 1=Unconfined, 2=Localhost.
        match container.seccomp_profile_type {
            0 => {
                // RuntimeDefault — use pelagos's built-in Docker-compatible seccomp profile.
                args.push("--security-opt".into());
                args.push("seccomp=default".into());
            }
            1 => {
                // Unconfined — disable seccomp entirely.
                args.push("--security-opt".into());
                args.push("seccomp=none".into());
            }
            2 => {
                // Localhost — use the profile file at localhost_ref.
                if !container.seccomp_profile_path.is_empty() {
                    args.push("--security-opt".into());
                    args.push(format!("seccomp={}", container.seccomp_profile_path));
                }
            }
            _ => {}
        }

        // Masked paths (securityContext.maskedPaths).
        for path in &container.masked_paths {
            args.push("--masked-path".into());
            args.push(path.clone());
        }

        // Readonly paths (securityContext.readonlyPaths) — bind-mount RO inside the container.
        for path in &container.readonly_paths {
            args.push("--bind-ro".into());
            args.push(format!("{}:{}", path, path));
        }

        // AppArmor profile (securityContext.apparmor).
        // Type: 0=RuntimeDefault (pelagos applies its default automatically, no flag needed),
        //       1=Unconfined, 2=Localhost.
        match container.apparmor_profile_type {
            1 => {
                args.push("--apparmor-profile".into());
                args.push("unconfined".into());
            }
            2 => {
                if !container.apparmor_profile_path.is_empty() {
                    args.push("--apparmor-profile".into());
                    args.push(container.apparmor_profile_path.clone());
                }
            }
            _ => {}
        }

        // OOM score adjustment (resources.oom_score_adj).
        // i32::MIN sentinel means the field was absent from the proto.
        // Use `=` form so clap does not misparse negative values as flags.
        if container.oom_score_adj != i32::MIN {
            args.push(format!("--oom-score-adj={}", container.oom_score_adj));
        }

        // Memory+swap combined limit (resources.memory_swap_limit_in_bytes).
        // -1 means "unlimited swap"; use `=` form for the same reason as above.
        if container.memory_swap_limit != 0 {
            args.push(format!("--memory-swap={}", container.memory_swap_limit));
        }

        // Host IPC namespace mode (sandbox namespace_options.ipc == 2).
        // Pass --no-ipc-ns to skip IPC namespace unsharing so the container
        // shares the host IPC namespace (hostIPC: true in the pod spec).
        let sandbox_ipc_ns_mode = {
            let st = self.state.inner.lock().await;
            st.sandboxes
                .get(&container.sandbox_id)
                .map(|s| s.ipc_namespace_mode)
                .unwrap_or(0)
        };
        if sandbox_ipc_ns_mode == 2 {
            args.push("--no-ipc-ns".into());
        }

        // cpuset affinity (resources.cpuset_cpus / cpuset_mems).
        if !container.cpuset_cpus.is_empty() {
            args.push("--cpuset-cpus".into());
            args.push(container.cpuset_cpus.clone());
        }
        if !container.cpuset_mems.is_empty() {
            args.push("--cpuset-mems".into());
            args.push(container.cpuset_mems.clone());
        }

        // Custom stop signal (config.stop_signal).
        if !container.stop_signal.is_empty() {
            args.push("--stop-signal".into());
            args.push(container.stop_signal.clone());
        }

        // HugePage limits (resources.hugepage_limits).
        for (page_size, limit) in &container.hugepage_limits {
            args.push("--hugepage-limit".into());
            args.push(format!("{}={}", page_size, limit));
        }

        // SELinux label (securityContext.selinux_options).
        if !container.selinux_label.is_empty() {
            args.push("--selinux-label".into());
            args.push(container.selinux_label.clone());
        }

        // `--` stops clap flag parsing so that container args beginning with `-`
        // (signal numbers, negative values, etc.) are passed through verbatim
        // instead of being interpreted as pelagos flags (issue #322).
        args.push("--".into());
        args.push(image);
        args.extend(effective_entrypoint);
        args.extend(effective_cmd);

        let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let out = run_pelagos(&bin, &args_ref)
            .await
            .map_err(|e| Status::internal(format!("exec error: {}", e)))?;

        if !out.success {
            log::error!(
                "start_container {}: pelagos run failed\nstdout: {}\nstderr: {}",
                container_id,
                out.stdout.trim(),
                out.stderr.trim()
            );
            return Err(Status::internal(format!(
                "pelagos run failed: {}",
                out.stderr.trim()
            )));
        }

        let (log_directory, log_path_rel) = {
            let st = self.state.inner.lock().await;
            let log_dir = st
                .sandboxes
                .get(&container.sandbox_id)
                .map(|s| s.log_directory.clone())
                .unwrap_or_default();
            let log_rel = st
                .containers
                .get(&container_id)
                .map(|c| c.log_path.clone())
                .unwrap_or_default();
            (log_dir, log_rel)
        };

        let mut st = self.state.inner.lock().await;
        if let Some(c) = st.containers.get_mut(&container_id) {
            c.state = MyContainerState::Running;
            c.started_at_ns = now_ns();
            let _ = state::save_container(c);
        }
        drop(st);

        if !log_directory.is_empty() && !log_path_rel.is_empty() {
            let cri_log_path = format!("{}/{}", log_directory, log_path_rel);
            let pelagos_name = container.pelagos_name.clone();
            tokio::spawn(relay_container_logs(pelagos_name, cri_log_path));
        }

        Ok(Response::new(StartContainerResponse {}))
    }

    async fn stop_container(
        &self,
        request: Request<StopContainerRequest>,
    ) -> Result<Response<StopContainerResponse>, Status> {
        let container_id = request.into_inner().container_id;
        let bin = self.bin().await;

        let pelagos_name = {
            let st = self.state.inner.lock().await;
            st.containers
                .get(&container_id)
                .map(|c| c.pelagos_name.clone())
                .ok_or_else(|| Status::not_found("container not found"))?
        };

        let _ = run_pelagos(&bin, &["stop", &pelagos_name]).await;

        let mut st = self.state.inner.lock().await;
        if let Some(c) = st.containers.get_mut(&container_id) {
            c.state = MyContainerState::Exited;
            c.finished_at_ns = now_ns();
            let _ = state::save_container(c);
        }

        Ok(Response::new(StopContainerResponse {}))
    }

    async fn remove_container(
        &self,
        request: Request<RemoveContainerRequest>,
    ) -> Result<Response<RemoveContainerResponse>, Status> {
        let container_id = request.into_inner().container_id;
        let bin = self.bin().await;

        let pelagos_name = {
            let st = self.state.inner.lock().await;
            st.containers
                .get(&container_id)
                .map(|c| c.pelagos_name.clone())
        };

        if let Some(name) = pelagos_name {
            let _ = run_pelagos(&bin, &["rm", &name]).await;
        }

        let mut st = self.state.inner.lock().await;
        st.containers.remove(&container_id);
        state::remove_container_file(&container_id);

        Ok(Response::new(RemoveContainerResponse {}))
    }

    async fn list_containers(
        &self,
        request: Request<ListContainersRequest>,
    ) -> Result<Response<ListContainersResponse>, Status> {
        let filter = request.into_inner().filter;
        let mut st = self.state.inner.lock().await;

        // Refresh running containers from disk
        let ids: Vec<String> = st.containers.keys().cloned().collect();
        for id in ids {
            if let Some(c) = st.containers.get(&id) {
                if c.state == MyContainerState::Running {
                    let pelagos_name = c.pelagos_name.clone();
                    if let Some(live) = read_pelagos_container_state(&pelagos_name) {
                        if live.status == "exited" {
                            if let Some(c) = st.containers.get_mut(&id) {
                                c.state = MyContainerState::Exited;
                                c.exit_code = live.exit_code.unwrap_or(0);
                                c.finished_at_ns = now_ns();
                                let _ = state::save_container(c);
                            }
                        }
                    }
                }
            }
        }

        let containers: Vec<crate::cri::Container> = st
            .containers
            .values()
            .filter(|c| {
                if let Some(ref f) = filter {
                    if !f.id.is_empty() && c.id != f.id {
                        return false;
                    }
                    if !f.pod_sandbox_id.is_empty() && c.sandbox_id != f.pod_sandbox_id {
                        return false;
                    }
                    if let Some(ref sv) = f.state {
                        let want = sv.state;
                        let have = cri_container_state_val(&c.state);
                        if want != have {
                            return false;
                        }
                    }
                    if !labels_match(&c.labels, &f.label_selector) {
                        return false;
                    }
                }
                true
            })
            .map(container_to_proto)
            .collect();

        Ok(Response::new(ListContainersResponse { containers }))
    }

    async fn container_status(
        &self,
        request: Request<ContainerStatusRequest>,
    ) -> Result<Response<ContainerStatusResponse>, Status> {
        let container_id = request.into_inner().container_id;

        let mut st = self.state.inner.lock().await;
        let container = st
            .containers
            .get(&container_id)
            .cloned()
            .ok_or_else(|| Status::not_found("container not found"))?;

        // Refresh from disk if running
        let container = if container.state == MyContainerState::Running {
            if let Some(live) = read_pelagos_container_state(&container.pelagos_name) {
                if live.status == "exited" {
                    if let Some(c) = st.containers.get_mut(&container_id) {
                        c.state = MyContainerState::Exited;
                        c.exit_code = live.exit_code.unwrap_or(0);
                        c.finished_at_ns = now_ns();
                        let _ = state::save_container(c);
                    }
                    st.containers.get(&container_id).cloned().unwrap()
                } else {
                    container
                }
            } else {
                container
            }
        } else {
            container
        };

        let cstate = cri_container_state_val(&container.state);

        let full_log_path = if !container.log_path.is_empty() {
            st.sandboxes
                .get(&container.sandbox_id)
                .filter(|s| !s.log_directory.is_empty())
                .map(|s| format!("{}/{}", s.log_directory, container.log_path))
                .unwrap_or_default()
        } else {
            String::new()
        };

        // Read termination message (up to 4096 bytes per CRI convention).
        let message = if !container.termination_log_host_path.is_empty() {
            std::fs::read_to_string(&container.termination_log_host_path)
                .unwrap_or_default()
                .chars()
                .take(4096)
                .collect()
        } else {
            String::new()
        };

        let status = CriContainerStatus {
            id: container.id.clone(),
            metadata: Some(ContainerMetadata {
                name: container.name.clone(),
                attempt: 0,
            }),
            state: cstate,
            created_at: container.created_at_ns,
            started_at: container.started_at_ns,
            finished_at: container.finished_at_ns,
            exit_code: container.exit_code,
            image: Some(ImageSpec {
                image: container.image.clone(),
                annotations: HashMap::new(),
                ..Default::default()
            }),
            image_ref: container.image.clone(),
            reason: String::new(),
            message,
            labels: container.labels.clone(),
            annotations: container.annotations.clone(),
            mounts: vec![],
            log_path: full_log_path,
            resources: None,
            image_id: container.image.clone(),
            user: None,
            stop_signal: 0,
        };

        Ok(Response::new(ContainerStatusResponse {
            status: Some(status),
            info: HashMap::new(),
        }))
    }

    async fn exec_sync(
        &self,
        request: Request<ExecSyncRequest>,
    ) -> Result<Response<ExecSyncResponse>, Status> {
        let req = request.into_inner();
        let container_id = req.container_id;
        let cmd = req.cmd;
        let timeout_secs = req.timeout;

        let (pelagos_name, bin) = {
            let st = self.state.inner.lock().await;
            let name = st
                .containers
                .get(&container_id)
                .map(|c| c.pelagos_name.clone())
                .ok_or_else(|| Status::not_found("container not found"))?;
            let bin = st.pelagos_bin.clone();
            (name, bin)
        };

        use std::process::Stdio;
        use tokio::process::Command;

        let mut proc_cmd = Command::new(&bin);
        proc_cmd.arg("exec");
        proc_cmd.arg(&pelagos_name);
        for c in &cmd {
            proc_cmd.arg(c);
        }
        proc_cmd.stdout(Stdio::piped());
        proc_cmd.stderr(Stdio::piped());

        let child = proc_cmd
            .spawn()
            .map_err(|e| Status::internal(format!("spawn error: {}", e)))?;

        let output = if timeout_secs > 0 {
            match tokio::time::timeout(
                std::time::Duration::from_secs(timeout_secs as u64),
                child.wait_with_output(),
            )
            .await
            {
                Ok(Ok(out)) => out,
                Ok(Err(e)) => return Err(Status::internal(format!("wait error: {}", e))),
                Err(_) => return Err(Status::deadline_exceeded("exec timed out")),
            }
        } else {
            child
                .wait_with_output()
                .await
                .map_err(|e| Status::internal(format!("wait error: {}", e)))?
        };

        let exit_code = output.status.code().unwrap_or(1);

        Ok(Response::new(ExecSyncResponse {
            stdout: output.stdout,
            stderr: output.stderr,
            exit_code,
        }))
    }

    // ── Unimplemented RPCs ───────────────────────────────────────────────────

    async fn update_container_resources(
        &self,
        _request: Request<UpdateContainerResourcesRequest>,
    ) -> Result<Response<UpdateContainerResourcesResponse>, Status> {
        Ok(Response::new(UpdateContainerResourcesResponse {}))
    }

    async fn reopen_container_log(
        &self,
        _request: Request<ReopenContainerLogRequest>,
    ) -> Result<Response<ReopenContainerLogResponse>, Status> {
        Ok(Response::new(ReopenContainerLogResponse {}))
    }

    async fn exec(&self, request: Request<ExecRequest>) -> Result<Response<ExecResponse>, Status> {
        let req = request.into_inner();
        let container_id = &req.container_id;

        let (pelagos_name, _sandbox_id) = {
            let st = self.state.inner.lock().await;
            let c = st
                .containers
                .get(container_id)
                .ok_or_else(|| Status::not_found(format!("container {container_id} not found")))?;
            (c.pelagos_name.clone(), c.sandbox_id.clone())
        };

        let token = uuid::Uuid::new_v4().to_string();
        crate::streaming::register_exec(
            &self.registry,
            token.clone(),
            PendingExec {
                container_name: pelagos_name,
                cmd: req.cmd,
                stdin: req.stdin,
                stdout: req.stdout,
                stderr: req.stderr,
                tty: req.tty,
            },
        )
        .await;

        let url = format!("{}/exec/{}", self.streaming_base_url, token);
        log::debug!("exec: token={token} url={url}");
        Ok(Response::new(ExecResponse { url }))
    }

    async fn attach(
        &self,
        request: Request<AttachRequest>,
    ) -> Result<Response<AttachResponse>, Status> {
        let req = request.into_inner();
        let container_id = &req.container_id;

        let pelagos_name = {
            let st = self.state.inner.lock().await;
            st.containers
                .get(container_id)
                .ok_or_else(|| Status::not_found(format!("container {container_id} not found")))?
                .pelagos_name
                .clone()
        };

        // Attach is exec with the container's default command (empty cmd).
        let token = uuid::Uuid::new_v4().to_string();
        crate::streaming::register_exec(
            &self.registry,
            token.clone(),
            PendingExec {
                container_name: pelagos_name,
                cmd: vec![],
                stdin: req.stdin,
                stdout: req.stdout,
                stderr: req.stderr,
                tty: req.tty,
            },
        )
        .await;

        let url = format!("{}/attach/{}", self.streaming_base_url, token);
        log::debug!("attach: token={token} url={url}");
        Ok(Response::new(AttachResponse { url }))
    }

    async fn port_forward(
        &self,
        request: Request<PortForwardRequest>,
    ) -> Result<Response<PortForwardResponse>, Status> {
        let req = request.into_inner();
        let sandbox_id = &req.pod_sandbox_id;

        let pod_ip = {
            let st = self.state.inner.lock().await;
            st.sandboxes
                .get(sandbox_id)
                .ok_or_else(|| Status::not_found(format!("sandbox {sandbox_id} not found")))?
                .ip
                .clone()
        };

        if pod_ip.is_empty() {
            return Err(Status::failed_precondition(
                "sandbox has no IP assigned (native networking not supported for port-forward)",
            ));
        }

        let token = uuid::Uuid::new_v4().to_string();
        crate::streaming::register_port_forward(
            &self.registry,
            token.clone(),
            PendingPortForward {
                pod_ip,
                ports: req.port.iter().map(|p| *p as u32).collect(),
            },
        )
        .await;

        let url = format!("{}/portforward/{}", self.streaming_base_url, token);
        log::debug!("portforward: token={token} url={url}");
        Ok(Response::new(PortForwardResponse { url }))
    }

    async fn container_stats(
        &self,
        request: Request<ContainerStatsRequest>,
    ) -> Result<Response<ContainerStatsResponse>, Status> {
        let id = request.into_inner().container_id;
        let container = {
            let st = self.state.inner.lock().await;
            st.containers.get(&id).cloned()
        };
        let container = container.ok_or_else(|| Status::not_found("container not found"))?;
        Ok(Response::new(ContainerStatsResponse {
            stats: Some(build_container_stats(&container)),
        }))
    }

    async fn list_container_stats(
        &self,
        request: Request<ListContainerStatsRequest>,
    ) -> Result<Response<ListContainerStatsResponse>, Status> {
        let filter = request.into_inner().filter;
        let containers: Vec<CriContainer> = {
            let st = self.state.inner.lock().await;
            st.containers
                .values()
                .filter(|c| {
                    if let Some(ref f) = filter {
                        if !f.id.is_empty() && c.id != f.id {
                            return false;
                        }
                        if !f.pod_sandbox_id.is_empty() && c.sandbox_id != f.pod_sandbox_id {
                            return false;
                        }
                        if !f.label_selector.is_empty()
                            && !labels_match(&c.labels, &f.label_selector)
                        {
                            return false;
                        }
                    }
                    true
                })
                .cloned()
                .collect()
        };
        let stats = containers.iter().map(build_container_stats).collect();
        Ok(Response::new(ListContainerStatsResponse { stats }))
    }

    async fn pod_sandbox_stats(
        &self,
        request: Request<PodSandboxStatsRequest>,
    ) -> Result<Response<PodSandboxStatsResponse>, Status> {
        let sb_id = request.into_inner().pod_sandbox_id;
        let st = self.state.inner.lock().await;
        let sb = st
            .sandboxes
            .get(&sb_id)
            .ok_or_else(|| Status::not_found(format!("sandbox not found: {}", sb_id)))?
            .clone();
        let containers: Vec<CriContainer> = st.containers.values().cloned().collect();
        Ok(Response::new(PodSandboxStatsResponse {
            stats: Some(build_sandbox_stats(&sb, &containers)),
        }))
    }

    async fn list_pod_sandbox_stats(
        &self,
        request: Request<ListPodSandboxStatsRequest>,
    ) -> Result<Response<ListPodSandboxStatsResponse>, Status> {
        let filter = request.into_inner().filter;
        let st = self.state.inner.lock().await;
        let containers: Vec<CriContainer> = st.containers.values().cloned().collect();
        let stats = st
            .sandboxes
            .values()
            .filter(|sb| {
                if let Some(ref f) = filter {
                    if !f.id.is_empty() && sb.id != f.id {
                        return false;
                    }
                    if !f.label_selector.is_empty() && !labels_match(&sb.labels, &f.label_selector)
                    {
                        return false;
                    }
                }
                true
            })
            .map(|sb| build_sandbox_stats(sb, &containers))
            .collect();
        Ok(Response::new(ListPodSandboxStatsResponse { stats }))
    }

    async fn update_runtime_config(
        &self,
        request: Request<UpdateRuntimeConfigRequest>,
    ) -> Result<Response<UpdateRuntimeConfigResponse>, Status> {
        let req = request.into_inner();
        if let Some(cfg) = &req.runtime_config {
            if let Some(net) = &cfg.network_config {
                log::info!("update_runtime_config: pod_cidr={}", net.pod_cidr);
            }
        }
        Ok(Response::new(UpdateRuntimeConfigResponse {}))
    }

    async fn checkpoint_container(
        &self,
        _request: Request<CheckpointContainerRequest>,
    ) -> Result<Response<CheckpointContainerResponse>, Status> {
        Err(Status::unimplemented("not implemented"))
    }

    async fn list_metric_descriptors(
        &self,
        _request: Request<ListMetricDescriptorsRequest>,
    ) -> Result<Response<ListMetricDescriptorsResponse>, Status> {
        Err(Status::unimplemented("not implemented"))
    }

    async fn list_pod_sandbox_metrics(
        &self,
        _request: Request<ListPodSandboxMetricsRequest>,
    ) -> Result<Response<ListPodSandboxMetricsResponse>, Status> {
        Err(Status::unimplemented("not implemented"))
    }

    async fn runtime_config(
        &self,
        _request: Request<RuntimeConfigRequest>,
    ) -> Result<Response<RuntimeConfigResponse>, Status> {
        Ok(Response::new(RuntimeConfigResponse { linux: None }))
    }

    async fn update_pod_sandbox_resources(
        &self,
        _request: Request<UpdatePodSandboxResourcesRequest>,
    ) -> Result<Response<UpdatePodSandboxResourcesResponse>, Status> {
        Ok(Response::new(UpdatePodSandboxResourcesResponse {}))
    }

    // ── Streaming RPCs ───────────────────────────────────────────────────────

    type StreamPodSandboxesStream = std::pin::Pin<
        Box<dyn futures_core::Stream<Item = Result<StreamPodSandboxesResponse, Status>> + Send>,
    >;

    async fn stream_pod_sandboxes(
        &self,
        _request: Request<StreamPodSandboxesRequest>,
    ) -> Result<Response<Self::StreamPodSandboxesStream>, Status> {
        Err(Status::unimplemented("not implemented"))
    }

    type StreamContainersStream = std::pin::Pin<
        Box<dyn futures_core::Stream<Item = Result<StreamContainersResponse, Status>> + Send>,
    >;

    async fn stream_containers(
        &self,
        _request: Request<StreamContainersRequest>,
    ) -> Result<Response<Self::StreamContainersStream>, Status> {
        Err(Status::unimplemented("not implemented"))
    }

    type StreamContainerStatsStream = std::pin::Pin<
        Box<dyn futures_core::Stream<Item = Result<StreamContainerStatsResponse, Status>> + Send>,
    >;

    async fn stream_container_stats(
        &self,
        _request: Request<StreamContainerStatsRequest>,
    ) -> Result<Response<Self::StreamContainerStatsStream>, Status> {
        Err(Status::unimplemented("not implemented"))
    }

    type StreamPodSandboxStatsStream = std::pin::Pin<
        Box<dyn futures_core::Stream<Item = Result<StreamPodSandboxStatsResponse, Status>> + Send>,
    >;

    async fn stream_pod_sandbox_stats(
        &self,
        _request: Request<StreamPodSandboxStatsRequest>,
    ) -> Result<Response<Self::StreamPodSandboxStatsStream>, Status> {
        Err(Status::unimplemented("not implemented"))
    }

    type GetContainerEventsStream = std::pin::Pin<
        Box<dyn futures_core::Stream<Item = Result<ContainerEventResponse, Status>> + Send>,
    >;

    async fn get_container_events(
        &self,
        _request: Request<GetEventsRequest>,
    ) -> Result<Response<Self::GetContainerEventsStream>, Status> {
        Err(Status::unimplemented("not implemented"))
    }

    type StreamPodSandboxMetricsStream = std::pin::Pin<
        Box<
            dyn futures_core::Stream<Item = Result<StreamPodSandboxMetricsResponse, Status>> + Send,
        >,
    >;

    async fn stream_pod_sandbox_metrics(
        &self,
        _request: Request<StreamPodSandboxMetricsRequest>,
    ) -> Result<Response<Self::StreamPodSandboxMetricsStream>, Status> {
        Err(Status::unimplemented("not implemented"))
    }
}

// ── CRI log relay ─────────────────────────────────────────────────────────────

/// Format current time as RFC3339 with nanosecond precision for the CRI log format.
fn cri_now() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.9fZ")
        .to_string()
}

/// Read bytes from `path` starting at `offset`. Returns `(new_bytes, new_offset)` or `None`.
async fn read_from_offset(path: &str, offset: u64) -> Option<(Vec<u8>, u64)> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut f = tokio::fs::File::open(path).await.ok()?;
    let size = f.seek(tokio::io::SeekFrom::End(0)).await.ok()?;
    if size <= offset {
        return None;
    }
    f.seek(tokio::io::SeekFrom::Start(offset)).await.ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).await.ok()?;
    let new_offset = offset + buf.len() as u64;
    Some((buf, new_offset))
}

/// Write complete lines from `buf` to `dest` in CRI log format; keeps any trailing partial line.
async fn flush_lines(buf: &mut String, dest: &str, stream: &str) {
    let flush_len = match buf.rfind('\n') {
        Some(pos) => pos + 1,
        None => return,
    };
    let to_write = buf[..flush_len].to_string();
    buf.drain(..flush_len);

    // Build the full output string before the blocking write.
    let stream = stream.to_string();
    let mut output = String::new();
    for line in to_write.lines() {
        output.push_str(&format!("{} {} F {}\n", cri_now(), stream, line));
    }
    if output.is_empty() {
        return;
    }

    let dest = dest.to_string();
    let _ = tokio::task::spawn_blocking(move || {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&dest)
            .and_then(|mut f| f.write_all(output.as_bytes()))
    })
    .await;
}

/// Background task: tail pelagos container log files and write to the CRI log file.
async fn relay_container_logs(pelagos_name: String, cri_log_path: String) {
    // Wait for log files to appear (container may take a moment to start writing).
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let container_dir = format!("/run/pelagos/containers/{}", pelagos_name);
    relay_logs_from_dir(container_dir, cri_log_path).await;
}

/// Core relay loop: polls stdout/stderr log files in `container_dir` and writes
/// CRI-formatted entries to `cri_log_path` until the container exits.
///
/// Separated from `relay_container_logs` so tests can inject a temp directory
/// instead of the live `/run/pelagos/containers/<name>/` path.
async fn relay_logs_from_dir(container_dir: String, cri_log_path: String) {
    let stdout_src = format!("{}/stdout.log", container_dir);
    let stderr_src = format!("{}/stderr.log", container_dir);
    let state_file = format!("{}/state.json", container_dir);

    // Ensure CRI log parent directory exists.
    if let Some(parent) = std::path::Path::new(&cri_log_path).parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }

    let mut stdout_off: u64 = 0;
    let mut stderr_off: u64 = 0;
    let mut stdout_buf = String::new();
    let mut stderr_buf = String::new();

    loop {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        if let Some((data, new_off)) = read_from_offset(&stdout_src, stdout_off).await {
            stdout_off = new_off;
            stdout_buf.push_str(&String::from_utf8_lossy(&data));
            flush_lines(&mut stdout_buf, &cri_log_path, "stdout").await;
        }

        if let Some((data, new_off)) = read_from_offset(&stderr_src, stderr_off).await {
            stderr_off = new_off;
            stderr_buf.push_str(&String::from_utf8_lossy(&data));
            flush_lines(&mut stderr_buf, &cri_log_path, "stderr").await;
        }

        // Stop when the container exits.
        match tokio::fs::read_to_string(&state_file).await {
            Ok(data) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&data) {
                    if v["status"].as_str() == Some("exited") {
                        // Final drain including any partial line.
                        if !stdout_buf.is_empty() {
                            stdout_buf.push('\n');
                            flush_lines(&mut stdout_buf, &cri_log_path, "stdout").await;
                        }
                        if !stderr_buf.is_empty() {
                            stderr_buf.push('\n');
                            flush_lines(&mut stderr_buf, &cri_log_path, "stderr").await;
                        }
                        break;
                    }
                }
            }
            Err(_) => break, // Container removed.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_cri_now_format() {
        let ts = cri_now();
        // Must match RFC3339Nano: 2006-01-02T15:04:05.000000000Z
        assert!(ts.ends_with('Z'), "timestamp must end with Z: {ts}");
        assert!(ts.len() >= 20, "timestamp too short: {ts}");
        // Basic date structure: YYYY-MM-DDTHH:MM:SS
        assert_eq!(&ts[4..5], "-", "year-month separator: {ts}");
        assert_eq!(&ts[7..8], "-", "month-day separator: {ts}");
        assert_eq!(&ts[10..11], "T", "date-time separator: {ts}");
    }

    #[tokio::test]
    async fn test_flush_lines_complete_lines() {
        let dest = NamedTempFile::new().unwrap();
        let dest_path = dest.path().to_str().unwrap().to_string();

        let mut buf = "hello world\nfoo bar\n".to_string();
        flush_lines(&mut buf, &dest_path, "stdout").await;

        assert!(buf.is_empty(), "complete lines should be flushed: {buf:?}");
        let content = std::fs::read_to_string(&dest_path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "two log entries expected");
        assert!(
            lines[0].contains(" stdout F hello world"),
            "line: {}",
            lines[0]
        );
        assert!(lines[1].contains(" stdout F foo bar"), "line: {}", lines[1]);
    }

    #[tokio::test]
    async fn test_flush_lines_partial_line_retained() {
        let dest = NamedTempFile::new().unwrap();
        let dest_path = dest.path().to_str().unwrap().to_string();

        let mut buf = "complete line\npartial".to_string();
        flush_lines(&mut buf, &dest_path, "stdout").await;

        // "partial" has no trailing newline — must stay in buffer.
        assert_eq!(buf, "partial", "partial line must be retained: {buf:?}");
        let content = std::fs::read_to_string(&dest_path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 1, "only the complete line should be written");
        assert!(lines[0].contains("complete line"));
    }

    #[tokio::test]
    async fn test_flush_lines_stderr_tag() {
        let dest = NamedTempFile::new().unwrap();
        let dest_path = dest.path().to_str().unwrap().to_string();

        let mut buf = "error output\n".to_string();
        flush_lines(&mut buf, &dest_path, "stderr").await;

        let content = std::fs::read_to_string(&dest_path).unwrap();
        assert!(
            content.contains(" stderr F error output"),
            "stderr tag expected: {content}"
        );
    }

    #[tokio::test]
    async fn test_flush_lines_empty_buf_noop() {
        let dest = NamedTempFile::new().unwrap();
        let dest_path = dest.path().to_str().unwrap().to_string();

        let mut buf = String::new();
        flush_lines(&mut buf, &dest_path, "stdout").await;

        let content = std::fs::read_to_string(&dest_path).unwrap();
        assert!(
            content.is_empty(),
            "nothing should be written for empty buf"
        );
    }

    /// End-to-end test for the relay loop.
    ///
    /// Sets up a fake container directory with stdout/stderr log files and a
    /// state.json, then runs relay_logs_from_dir and verifies:
    ///   - Lines written before the relay started are captured (catch-up).
    ///   - Lines appended while the relay is running are captured (streaming).
    ///   - Each entry has the CRI log format: `{RFC3339Nano} {stream} F {line}`.
    ///   - The relay exits promptly once state.json shows "exited".
    ///   - Both stdout and stderr entries appear with the correct stream tag.
    #[tokio::test]
    async fn test_relay_logs_from_dir_e2e() {
        use std::io::Write;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let dir_path = dir.path().to_str().unwrap().to_string();

        // Write the initial state: container is running.
        let state_path = dir.path().join("state.json");
        std::fs::write(&state_path, r#"{"status":"running"}"#).unwrap();

        // Pre-write some stdout before the relay starts (catch-up scenario).
        let stdout_path = dir.path().join("stdout.log");
        let stderr_path = dir.path().join("stderr.log");
        std::fs::write(&stdout_path, "line before relay\n").unwrap();

        // CRI log destination.
        let cri_log = NamedTempFile::new().unwrap();
        let cri_log_path = cri_log.path().to_str().unwrap().to_string();

        // Spawn the relay; it sleeps 0 ms here because we use relay_logs_from_dir
        // directly (skipping the 300 ms startup wait in relay_container_logs).
        let dir_clone = dir_path.clone();
        let cri_clone = cri_log_path.clone();
        let relay = tokio::spawn(async move {
            relay_logs_from_dir(dir_clone, cri_clone).await;
        });

        // Give the relay time to start and pick up the pre-written line.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;

        // Append more output while the relay is running.
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&stdout_path)
                .unwrap();
            writeln!(f, "line during relay").unwrap();
        }
        {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&stderr_path)
                .unwrap();
            writeln!(f, "stderr line").unwrap();
        }

        // Let the relay pick up the new lines.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;

        // Signal container exit; relay should drain and stop.
        std::fs::write(&state_path, r#"{"status":"exited"}"#).unwrap();

        // Wait for the relay task with a generous timeout.
        tokio::time::timeout(std::time::Duration::from_secs(3), relay)
            .await
            .expect("relay did not stop within 3 s after container exited")
            .expect("relay task panicked");

        let content = std::fs::read_to_string(cri_log.path()).unwrap();

        // Every non-empty line must be a valid CRI log entry.
        let entries: Vec<&str> = content.lines().collect();
        assert!(!entries.is_empty(), "no CRI log entries written");
        for entry in &entries {
            assert!(
                entry.contains(" stdout F ") || entry.contains(" stderr F "),
                "entry missing CRI stream tag: {entry}"
            );
            // Timestamp must look like RFC3339: starts with a digit and contains 'T'.
            assert!(
                entry
                    .chars()
                    .next()
                    .map(|c| c.is_ascii_digit())
                    .unwrap_or(false)
                    && entry.contains('T'),
                "entry missing RFC3339 timestamp: {entry}"
            );
        }

        // All three expected payload lines must appear.
        let payload = |needle: &str| entries.iter().any(|e| e.ends_with(needle));
        assert!(payload("line before relay"), "catch-up line missing");
        assert!(payload("line during relay"), "streaming line missing");
        assert!(payload("stderr line"), "stderr line missing");

        // Stream tags must be correct.
        assert!(
            entries.iter().any(|e| e.contains(" stdout F ")),
            "no stdout entries"
        );
        assert!(
            entries.iter().any(|e| e.contains(" stderr F ")),
            "no stderr entries"
        );
    }

    /// Verify that DNS fields from CriSandbox round-trip through serde correctly,
    /// matching what run_pod_sandbox stores from a CRI DnsConfig proto.
    #[test]
    fn test_cri_sandbox_dns_fields_roundtrip() {
        let sandbox = state::CriSandbox {
            id: "abc".into(),
            name: "test-pod".into(),
            namespace: "default".into(),
            uid: "uid-1".into(),
            attempt: 0,
            labels: Default::default(),
            annotations: Default::default(),
            created_at_ns: 0,
            state: state::SandboxState::Running,
            netns: String::new(),
            ip: String::new(),
            cni_conf: String::new(),
            pause_pid: 0,
            log_directory: String::new(),
            cgroup_parent: String::new(),
            supplemental_groups: vec![],
            dns_servers: vec!["10.96.0.10".into()],
            dns_searches: vec!["default.svc.cluster.local".into(), "cluster.local".into()],
            dns_options: vec!["ndots:5".into()],
            pid_namespace_mode: 0,
            ipc_namespace_mode: 0,
        };

        let json = serde_json::to_string(&sandbox).expect("serialize");
        let decoded: state::CriSandbox = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(decoded.dns_servers, vec!["10.96.0.10"]);
        assert_eq!(
            decoded.dns_searches,
            vec!["default.svc.cluster.local", "cluster.local"]
        );
        assert_eq!(decoded.dns_options, vec!["ndots:5"]);
    }

    /// Verify that CriSandbox.cgroup_parent round-trips through serde, and that
    /// start_container constructs the correct cgroup path as <parent>/<container-id>.
    #[test]
    fn test_cri_sandbox_cgroup_parent_roundtrip() {
        let sandbox = state::CriSandbox {
            id: "sb1".into(),
            name: "test-pod".into(),
            namespace: "default".into(),
            uid: "uid-1".into(),
            attempt: 0,
            labels: Default::default(),
            annotations: Default::default(),
            created_at_ns: 0,
            state: state::SandboxState::Running,
            netns: String::new(),
            ip: String::new(),
            cni_conf: String::new(),
            pause_pid: 0,
            log_directory: String::new(),
            cgroup_parent: "kubepods/besteffort/pod084f1637-fccf-40a9-8048-7aea569fe82b".into(),
            supplemental_groups: vec![],
            dns_servers: vec![],
            dns_searches: vec![],
            dns_options: vec![],
            pid_namespace_mode: 0,
            ipc_namespace_mode: 0,
        };

        let json = serde_json::to_string(&sandbox).expect("serialize");
        let decoded: state::CriSandbox = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            decoded.cgroup_parent,
            "kubepods/besteffort/pod084f1637-fccf-40a9-8048-7aea569fe82b"
        );

        // Verify the path construction logic used in start_container.
        let container_id = "abc123def456";
        let cgroup_path = format!("{}/{}", decoded.cgroup_parent, container_id);
        assert_eq!(
            cgroup_path,
            "kubepods/besteffort/pod084f1637-fccf-40a9-8048-7aea569fe82b/abc123def456"
        );
    }

    /// Verify that leading slashes in cgroup_parent from kubelet are stripped so
    /// the path is always relative to /sys/fs/cgroup.
    #[test]
    fn test_cgroup_parent_leading_slash_stripped() {
        // Simulate what run_pod_sandbox does when kubelet sends "/kubepods/besteffort/pod<uid>".
        let raw = "/kubepods/besteffort/pod-uid-123";
        let stored = raw.trim_start_matches('/').to_owned();
        assert_eq!(stored, "kubepods/besteffort/pod-uid-123");

        let container_id = "ctr-xyz";
        let full = format!("{}/{}", stored, container_id);
        assert_eq!(full, "kubepods/besteffort/pod-uid-123/ctr-xyz");
    }

    /// Verify generate_id() produces a 64-character lowercase hex string.
    ///
    /// This is the de facto standard used by containerd and CRI-O.  Tools like SPIRE,
    /// Fluentd, Fluent Bit, OTel, Datadog, and Falco hardcode `[a-f0-9]{64}` regexes
    /// to extract container IDs from cgroup paths and log file names.  A 32-char UUID
    /// simple string would produce zero matches — no workload identity, no log correlation.
    #[test]
    fn test_generate_id_is_64_char_hex() {
        let id = generate_id();
        assert_eq!(
            id.len(),
            64,
            "container ID must be 64 chars (containerd/CRI-O compat); got {} chars: {}",
            id.len(),
            id
        );
        assert!(
            id.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
            "container ID must be lowercase hex; got: {}",
            id
        );
    }

    /// Verify generate_id() produces unique IDs on repeated calls.
    ///
    /// Collision probability at 256 bits is negligible, but an obvious bug
    /// (e.g. seeded PRNG, zero bytes) would produce duplicates.
    #[test]
    fn test_generate_id_is_unique() {
        let ids: std::collections::HashSet<String> = (0..20).map(|_| generate_id()).collect();
        assert_eq!(ids.len(), 20, "generate_id() produced duplicate IDs");
    }
}
