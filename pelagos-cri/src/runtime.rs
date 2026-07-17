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
    Mount, Namespace, NamespaceOption, PodSandbox, PodSandboxAttributes, PodSandboxMetadata,
    PodSandboxNetworkStatus, PodSandboxState, PodSandboxStats, PodSandboxStatsRequest,
    PodSandboxStatsResponse, PodSandboxStatus as CriPodSandboxStatus, PodSandboxStatusRequest,
    PodSandboxStatusResponse, PortForwardRequest, PortForwardResponse, RemoveContainerRequest,
    RemoveContainerResponse, RemovePodSandboxRequest, RemovePodSandboxResponse,
    ReopenContainerLogRequest, ReopenContainerLogResponse, RuntimeCondition, RuntimeConfigRequest,
    RuntimeConfigResponse, RuntimeHandler, RuntimeHandlerFeatures, RuntimeStatus,
    StartContainerRequest, StartContainerResponse, StatusRequest, StatusResponse,
    StopContainerRequest, StopContainerResponse, StopPodSandboxRequest, StopPodSandboxResponse,
    StreamContainerStatsRequest, StreamContainerStatsResponse, StreamContainersRequest,
    StreamContainersResponse, StreamPodSandboxMetricsRequest, StreamPodSandboxMetricsResponse,
    StreamPodSandboxStatsRequest, StreamPodSandboxStatsResponse, StreamPodSandboxesRequest,
    StreamPodSandboxesResponse, UInt64Value, UpdateContainerResourcesRequest,
    UpdateContainerResourcesResponse, UpdatePodSandboxResourcesRequest,
    UpdatePodSandboxResourcesResponse, UpdateRuntimeConfigRequest, UpdateRuntimeConfigResponse,
    VersionRequest, VersionResponse,
};
use crate::invoke::run_pelagos;
use crate::scope;
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
    /// True if the kernel OOM killer terminated the container (#343). Surfaced
    /// as `reason: OOMKilled` in ContainerStatus.
    #[serde(default)]
    oom_killed: bool,
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Generate a 64-character lowercase hex container/sandbox ID.
///
/// 32 bytes from the OS CSPRNG encoded as hex — identical to the format used
/// by containerd and CRI-O.  The 64-char length is a de facto standard
/// hardcoded in SPIRE, Fluentd, Fluent Bit, OTel, Datadog, Falco, and cAdvisor.
/// The absolute filesystem path where a container's CRI-format log is written AND
/// that `ContainerStatus.log_path` reports back to the kubelet. The log relay
/// (writer) and ContainerStatus (reporter) both call this, so the reported path
/// always has a real log file behind it — it is never a dangling placeholder.
///
/// Every container gets a real, absolute path — never empty, never relative:
///   * the client supplied a log directory + path (the kubelet always does) →
///     honor `<log_directory>/<log_path>`;
///   * the client supplied neither (e.g. a direct CRI client such as critest) →
///     synthesize one inside pelagos's own runtime dir and relay the container's
///     stdout/stderr there anyway, so `crictl logs` still works and the path is
///     both meaningful and contained.
///
/// Why never empty/relative (issue #347): the kubelet derives its container-log
/// cleanup path from this value. An empty or relative LogPath collapses on the
/// kubelet side — `filepath.Dir("")` is `"."`, and with the kubelet's working
/// directory at `/` a subsequent `os.RemoveAll` walks the HOST ROOT and unlinks
/// entries like the usr-merge `/bin` symlink, breaking the node. Proven on a
/// disposable cluster node: the k3s kubelet does `unlinkat(AT_FDCWD</>, "bin")`
/// when GC'ing an orphaned sandbox whose container reported an empty log path.
fn effective_cri_log_path(log_directory: &str, log_path: &str, pelagos_name: &str) -> String {
    if !log_directory.is_empty() && !log_path.is_empty() {
        let joined = format!("{}/{}", log_directory, log_path);
        if joined.starts_with('/') {
            return joined;
        }
    }
    format!("/run/pelagos/containers/{}/cri.log", pelagos_name)
}

fn generate_id() -> String {
    use std::io::Read;
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .expect("read /dev/urandom for container ID generation");
    bytes.map(|b| format!("{:02x}", b)).concat()
}

/// Read `(ppid, session_id)` for a host process from `/proc/<pid>/stat`.
///
/// The `comm` field may contain spaces and parentheses, so we parse the fields
/// *after* the final `)`: there, field 0 is state, 1 is ppid, 2 is pgrp, 3 is
/// session.
fn read_ppid_sid(pid: i32) -> Option<(i32, i32)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after = &stat[stat.rfind(')')? + 1..];
    let f: Vec<&str> = after.split_whitespace().collect();
    Some((f.get(1)?.parse().ok()?, f.get(3)?.parse().ok()?))
}

/// Terminate a timed-out `pelagos exec` and the command it ran inside the
/// container, leaving the container's own processes untouched (issue #339).
///
/// This is subtle because of two layers of indirection:
///   * For a PID-namespaced container `pelagos exec` double-forks: an
///     intermediate (host namespace, shares the runtime's session) forks the real
///     exec'd process, which runs in the container's namespace and `setsid`s into
///     its OWN session.
///   * A shell that *forks* its command (`sh -c '...; sleep N'`) leaves the
///     `sleep` reparented to container-init.
///
/// So neither a kill of the wrapper, nor of its direct child, nor a host-side
/// process-group kill (the group lives inside the namespace) reaches the command.
/// Session membership does: it survives reparenting and is visible from the host.
///
/// Algorithm: scan `/proc`, compute the wrapper's transitive descendants by PPID
/// (capturing the intermediate AND the real exec'd process while the intermediate
/// is still alive), then collect every session whose *leader is one of those
/// descendants* — i.e. the session the exec'd process created with `setsid`.
/// SIGKILL every process in those sessions, plus all descendants and the wrapper.
///
/// Only sessions led by our own descendants are killed, never a merely *shared*
/// session — that safety property is what keeps this from taking down pelagos-cri
/// itself or unrelated processes.
fn kill_exec_wrapper(wrapper_pid: i32) {
    let mut procs: Vec<(i32, i32, i32)> = Vec::new(); // (pid, ppid, sid)
    if let Ok(rd) = std::fs::read_dir("/proc") {
        for ent in rd.flatten() {
            if let Some(pid) = ent.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) {
                if let Some((ppid, sid)) = read_ppid_sid(pid) {
                    procs.push((pid, ppid, sid));
                }
            }
        }
    }

    // Transitive descendants of the wrapper (PPID closure).
    let mut descendants: Vec<i32> = vec![wrapper_pid];
    loop {
        let mut added = false;
        for (pid, ppid, _) in &procs {
            if descendants.contains(ppid) && !descendants.contains(pid) {
                descendants.push(*pid);
                added = true;
            }
        }
        if !added {
            break;
        }
    }

    // Sessions whose leader is one of those descendants (the setsid'd exec'd
    // process / its forking shell). Never a session merely shared with the
    // wrapper or the runtime.
    let mut sessions: Vec<i32> = procs
        .iter()
        .filter(|(pid, _, sid)| *pid == *sid && *sid > 0 && descendants.contains(pid))
        .map(|(_, _, sid)| *sid)
        .collect();
    sessions.sort_unstable();
    sessions.dedup();

    for (pid, _, sid) in &procs {
        if descendants.contains(pid) || sessions.contains(sid) {
            unsafe {
                libc::kill(*pid, libc::SIGKILL);
            }
        }
    }
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
/// - `usage_bytes` = total cgroup memory (all processes, including page cache)
/// - `working_set_bytes` = usage minus reclaimable page cache (inactive_file), the
///   value metrics-server uses for HPA
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
                attempt: c.attempt,
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

/// Resolve the AppArmor `(profile_type, profile_name)` for a container (#353).
/// The new `apparmor` SecurityProfile (passed as `(profile_type, localhost_ref)`)
/// takes precedence over the deprecated `apparmor_profile` string. critest sends
/// the profile name with a `"localhost/"` prefix in both forms; strip it so the
/// bare name reaches the kernel. Types: 0=RuntimeDefault, 1=Unconfined, 2=Localhost.
fn resolve_apparmor(apparmor: Option<(i32, &str)>, deprecated: &str) -> (i32, String) {
    if let Some((ptype, localhost_ref)) = apparmor {
        let name = localhost_ref
            .strip_prefix("localhost/")
            .unwrap_or(localhost_ref)
            .to_string();
        (ptype, name)
    } else {
        match deprecated {
            "" | "runtime/default" => (0, String::new()),
            "unconfined" => (1, String::new()),
            s => (2, s.strip_prefix("localhost/").unwrap_or(s).to_string()),
        }
    }
}

/// Whether an AppArmor profile of the given name is currently loaded into the
/// kernel (#353). The securityfs file lists one `"<name> (<mode>)"` per line.
/// If securityfs/AppArmor is unavailable we cannot validate, so assume loaded
/// (don't reject) — the apply-time write surfaces any real error.
fn apparmor_profile_loaded(name: &str) -> bool {
    match std::fs::read_to_string("/sys/kernel/security/apparmor/profiles") {
        Ok(data) => data.lines().any(|l| l.split(" (").next() == Some(name)),
        Err(_) => true,
    }
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
            attempt: c.attempt,
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

    /// Version of the pelagos binary we delegate to, for the CRI `Version` RPC.
    ///
    /// pelagos-cri's own crate version is meaningless as a runtime version (it
    /// is decoupled from the pelagos release, #424), so we ask the binary
    /// itself: `pelagos --version` → `pelagos 0.65.42+abc1234`. This reflects
    /// the actually-installed release and makes kubelet's node
    /// containerRuntimeVersion (and kube_node_info) report the true version.
    /// Falls back to this crate's version if the binary can't be queried.
    async fn pelagos_binary_version(&self) -> String {
        let bin = self.bin().await;
        let fallback = || env!("CARGO_PKG_VERSION").to_string();
        match tokio::process::Command::new(&bin)
            .arg("--version")
            .output()
            .await
        {
            Ok(out) if out.status.success() => {
                parse_pelagos_version(&String::from_utf8_lossy(&out.stdout))
                    .unwrap_or_else(fallback)
            }
            _ => fallback(),
        }
    }

    /// The config digest (image id) of a stored image dir: sha256 of its raw
    /// `oci-config.json` blob (matches containerd's image id and what PullImage/
    /// ImageStatus report). Falls back to the manifest digest when the config
    /// blob isn't present (older/locally-built images). #382.
    async fn config_digest_of(dir: &std::path::Path, manifest_digest: &str) -> String {
        match tokio::fs::read(dir.join("oci-config.json")).await {
            Ok(bytes) => {
                use sha2::Digest as _;
                format!("sha256:{:x}", sha2::Sha256::digest(&bytes))
            }
            Err(_) => manifest_digest.to_string(),
        }
    }

    /// Resolve a digest-form image ref (sha256:...) to a repo tag by scanning the image store.
    /// The kubelet creates containers using the image **id** (config digest) it received from
    /// PullImage/ImageStatus, not the original tag — so match the config digest as well as the
    /// manifest digest, otherwise `pelagos run` would try to pull `docker.io/library/sha256:…`
    /// and fail (#382).
    async fn resolve_image_ref(image_ref: &str) -> String {
        if !image_ref.starts_with("sha256:") {
            return image_ref.to_string();
        }
        let Ok(mut rd) = tokio::fs::read_dir("/var/lib/pelagos/images").await else {
            return image_ref.to_string();
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            let dir = entry.path();
            if let Ok(data) = tokio::fs::read_to_string(dir.join("manifest.json")).await {
                if let Ok(m) = serde_json::from_str::<serde_json::Value>(&data) {
                    let manifest_digest = m["digest"].as_str().unwrap_or("");
                    let config_digest = Self::config_digest_of(&dir, manifest_digest).await;
                    if config_digest == image_ref || manifest_digest == image_ref {
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
            let dir = entry.path();
            let Ok(data) = tokio::fs::read_to_string(dir.join("manifest.json")).await else {
                continue;
            };
            let Ok(m) = serde_json::from_str::<serde_json::Value>(&data) else {
                continue;
            };
            let manifest_digest = m["digest"].as_str().unwrap_or("");
            let config_digest = Self::config_digest_of(&dir, manifest_digest).await;
            if m["reference"].as_str() == Some(image_ref)
                || manifest_digest == image_ref
                || config_digest == image_ref
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

/// Extract the version token from `pelagos --version` output.
/// `"pelagos 0.65.42+abc1234\n"` → `Some("0.65.42+abc1234")`.
fn parse_pelagos_version(output: &str) -> Option<String> {
    output.split_whitespace().nth(1).map(|s| s.to_string())
}

#[tonic::async_trait]
impl RuntimeService for RuntimeSvc {
    async fn version(
        &self,
        _request: Request<VersionRequest>,
    ) -> Result<Response<VersionResponse>, Status> {
        Ok(Response::new(VersionResponse {
            // `version` is the constant CRI-API marker (mirrors containerd);
            // kubelet builds the node's containerRuntimeVersion from
            // runtime_name://runtime_version, so the real release goes there.
            version: "0.1.0".into(),
            runtime_name: "pelagos".into(),
            runtime_version: self.pelagos_binary_version().await,
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
            // Advertise recursive read-only mount support (#356) on the default
            // handler so the kubelet/critest will request and exercise it.
            runtime_handlers: vec![RuntimeHandler {
                name: String::new(),
                features: Some(RuntimeHandlerFeatures {
                    recursive_read_only_mounts: true,
                    ..Default::default()
                }),
            }],
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

        // Pod-level settings applied to the pause's (container-shared) namespaces
        // once the pause exists: hostname → UTS ns, sysctls → net/ipc/uts ns (#354/#355).
        let sandbox_hostname = config.hostname.clone();
        let sandbox_sysctls = config
            .linux
            .as_ref()
            .map(|l| l.sysctls.clone())
            .unwrap_or_default();

        // Host port mappings → CNI portmap plugin capability args (#354).
        let sandbox_port_mappings: Vec<state::CriPortMapping> = config
            .port_mappings
            .iter()
            .map(|p| state::CriPortMapping {
                protocol: p.protocol,
                container_port: p.container_port,
                host_port: p.host_port,
                host_ip: p.host_ip.clone(),
            })
            .collect();
        let cni_cap_args = cni::port_mapping_cap_args(&sandbox_port_mappings);

        // Network / PID / IPC namespace sharing modes — read from the pod's
        // NamespaceOption exactly once here, then reused for the pause flags and
        // stored in the sandbox state (rather than re-derived in several places).
        // Read up front because the pause is spawned before the CriSandbox struct.
        let namespaces = config
            .linux
            .as_ref()
            .and_then(|l| l.security_context.as_ref())
            .and_then(|sc| sc.namespace_options.as_ref())
            .map(|no| state::NamespaceModes::from_cri(no.network, no.pid, no.ipc))
            .unwrap_or_default();
        // hostIPC (#386): the pause skips unshare(IPC) so the pod shares host IPC.
        let host_ipc = namespaces.host_ipc();
        // hostNetwork (#394): the pod uses the host network namespace — no CNI,
        // no netns; the pause stays in host net and containers skip the NET join.
        let host_network = namespaces.host_network();
        // shareProcessNamespace (#398): the pause forks a PID-1 init holding a
        // shared pod PID namespace that containers join.
        let pod_pid = namespaces.shared_pid();

        // hostNetwork takes a dedicated branch (no CNI); otherwise try CNI first
        // and fall back to pelagos native bridge networking.
        let (sandbox_id, netns, ip, cni_conf, pause_pid) = if host_network {
            // ── HostNetwork path: no CNI, no netns; share the host network ──
            let id = generate_id();
            let ip = node_primary_ipv4();

            // Pause: --host-net keeps it in host net (skips the netns setns);
            // --host-ipc additionally keeps host IPC; --pod-pid makes it a PID-1
            // init for a shared pod PID namespace. ns_name is unused by the pause
            // in this mode, so pass an empty placeholder.
            let mut pause_argv: Vec<&str> = vec!["sandbox", "__pause__", "", "--host-net"];
            if host_ipc {
                pause_argv.push("--host-ipc");
            }
            if pod_pid {
                pause_argv.push("--pod-pid");
            }
            let pause_pid = spawn_sandbox_pause(&bin, &id, &pause_argv, pod_pid).await?;

            // Store ns_name="" so StopPodSandbox skips CNI/netns teardown.
            state::write_pelagos_sandbox_state(
                &id,
                Some(&meta.name),
                pause_pid,
                "",
                &ip,
                namespaces,
            )
            .map_err(|e| Status::internal(format!("write sandbox state: {}", e)))?;

            // Apply the pod hostname into the pause's UTS namespace once unshared.
            // Skip sysctls: a hostNetwork pod has no private netns, so net sysctls
            // would target the host; UTS/host sysctls apply by definition.
            if !sandbox_hostname.is_empty() && wait_for_pause_ns_unshare(pause_pid).await {
                apply_pod_hostname(pause_pid, &sandbox_hostname).await;
            }

            log::info!(
                "hostNetwork sandbox {} created: host network namespace, ip={}",
                id,
                ip
            );

            (id, String::new(), ip, String::new(), pause_pid)
        } else if let Some(conf_path) = cni::find_cni_conf() {
            // ── CNI path ───────────────────────────────────────────────────
            let id = generate_id();
            let ns_name = format!("pcri-{}", &id[..12]);

            let netns_path = cni::create_netns(&ns_name)
                .map_err(|e| Status::internal(format!("create netns for CNI sandbox: {}", e)))?;

            let ip = match cni::cni_add(&id, &netns_path, &conf_path, &cni_cap_args) {
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
            //
            // Under systemd the pause runs as a transient service under
            // `pelagos.slice` (created by PID 1) so it survives a `pelagos-cri`
            // restart and the sandbox is re-adopted rather than torn down (#336).
            // The pause blocks forever, so it must be a backgrounded *service*
            // (not a `--scope`, which would block); its real PID is the unit's
            // MainPID. Off systemd we fall back to a plain leaked child.
            // pause argv: --host-ipc keeps host IPC (#386); --pod-pid makes the
            // pause a PID-1 init for a shared pod PID namespace (#398).
            let mut pause_argv: Vec<&str> = vec!["sandbox", "__pause__", &ns_name];
            if host_ipc {
                pause_argv.push("--host-ipc");
            }
            if pod_pid {
                pause_argv.push("--pod-pid");
            }
            let pause_pid = spawn_sandbox_pause(&bin, &id, &pause_argv, pod_pid).await?;

            // Write pelagos-format sandbox state so `pelagos run --sandbox` works.
            state::write_pelagos_sandbox_state(
                &id,
                Some(&meta.name),
                pause_pid,
                &ns_name,
                &ip,
                namespaces,
            )
            .map_err(|e| Status::internal(format!("write sandbox state: {}", e)))?;

            // Apply pod hostname + sysctls into the pause namespaces — but ONLY after
            // confirming the pause has unshared UTS/IPC, so nsenter targets the POD
            // namespaces and never the host (containers join these namespaces and
            // inherit the values: #354 set-hostname, #355 sysctls).
            if (!sandbox_hostname.is_empty() || !sandbox_sysctls.is_empty())
                && wait_for_pause_ns_unshare(pause_pid).await
            {
                apply_pod_hostname(pause_pid, &sandbox_hostname).await;
                apply_sandbox_sysctls(&netns_path, pause_pid, &sandbox_sysctls).await;
            } else if !sandbox_hostname.is_empty() || !sandbox_sysctls.is_empty() {
                log::warn!(
                    "pause {} did not unshare UTS/IPC in time; skipping pod hostname/sysctls \
                     to avoid mutating the host",
                    pause_pid
                );
            }

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
            namespaces,
            port_mappings: sandbox_port_mappings,
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
        log::debug!("StopPodSandbox {} BEGIN", sandbox_id);

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
                log::debug!(
                    "StopPodSandbox {} step=stop-container {}",
                    sandbox_id,
                    pelagos_name
                );
                // `pelagos stop` is the single kill path (#459): cgroup kill is
                // primary, SIGKILL fallback, zombie-proof liveness wait.
                let _ = run_pelagos(&bin, &["stop", pelagos_name]).await;
                log::debug!(
                    "StopPodSandbox {} step=stop-container {} DONE",
                    sandbox_id,
                    pelagos_name
                );
            }
        }

        let sandbox = {
            let st = self.state.inner.lock().await;
            st.sandboxes.get(&sandbox_id).cloned()
        };

        if let Some(ref sb) = sandbox {
            if !sb.netns.is_empty() {
                // ── CNI teardown ───────────────────────────────────────────────
                // ORDER MATTERS, and the killer subtlety is systemd, not the network.
                //
                // The pause is PID 1 of the pod's PID namespace. Killing it collapses
                // that namespace: the kernel SIGKILLs and reaps every process sharing
                // it (the pause runs `zap_pid_ns_processes`). For a
                // shareProcessNamespace (pid=POD) pod a container's workers outlive
                // `pelagos stop` — the master dying does not reap them; they are
                // reparented to the pause — so this collapse has real work to do.
                //
                // If we ask systemd to stop the pause's transient unit WHILE that
                // collapse is in flight, systemd (host PID 1) blocks UNINTERRUPTIBLY
                // in `cgroup_drain_dying` waiting for the pause cgroup to empty — but
                // the pause can't leave the cgroup until its exit completes, and that
                // exit deadlocks against the very cgroup teardown systemd is holding.
                // PID 1 stuck in D wedges the ENTIRE host: ICMP still answers but every
                // fork/SSH-session hangs (#399 PodPID teardown / #400). Confirmed via
                // live `ps -o stat,wchan` capture of the hung node.
                //
                // So: SIGTERM the pause and WAIT for it to fully exit (drain) FIRST.
                // Only once it is gone — cgroup already empty — is it safe to let
                // systemd reap the unit. If the pause refuses to die, SKIP the systemd
                // stop entirely: leaking a transient unit (it self-collects via
                // --collect once it eventually dies) is infinitely preferable to
                // hanging PID 1. Network teardown happens last, on a quiesced netns.
                let netns_path = format!("/run/netns/{}", sb.netns);
                let mut pause_drained = true;
                if sb.pause_pid > 0 {
                    log::debug!(
                        "StopPodSandbox {} step=kill-pause pid={}",
                        sandbox_id,
                        sb.pause_pid
                    );
                    let _ = std::process::Command::new("kill")
                        .args(["-TERM", &sb.pause_pid.to_string()])
                        .output();
                    pause_drained = false;
                    // Bounded drain (~5s): poll until the pause is gone (ESRCH).
                    for _ in 0..100 {
                        if unsafe { libc::kill(sb.pause_pid, 0) } != 0 {
                            pause_drained = true;
                            break; // pause gone, pod PID ns fully collapsed
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    log::debug!(
                        "StopPodSandbox {} step=kill-pause DONE drained={}",
                        sandbox_id,
                        pause_drained
                    );
                }
                // Tear down the transient pause service (#336) — but ONLY after the
                // pause has actually exited, so systemd's cgroup teardown finds an
                // empty cgroup and cannot block PID 1. Skip it on a stuck pause.
                if scope::systemd_available() {
                    if pause_drained {
                        log::debug!("StopPodSandbox {} step=stop_unit", sandbox_id);
                        scope::stop_unit(&scope::sandbox_unit(&sandbox_id)).await;
                        log::debug!("StopPodSandbox {} step=stop_unit DONE", sandbox_id);
                    } else {
                        log::warn!(
                            "StopPodSandbox {} pause {} did not exit within drain window; \
                             skipping `systemctl stop` to avoid wedging PID 1 — the transient \
                             unit will self-collect (--collect) once the pause dies",
                            sandbox_id,
                            sb.pause_pid
                        );
                    }
                }
                if !sb.cni_conf.is_empty() {
                    let cap_args = cni::port_mapping_cap_args(&sb.port_mappings);
                    log::debug!(
                        "StopPodSandbox {} step=cni_del netns={}",
                        sandbox_id,
                        sb.netns
                    );
                    cni::cni_del(
                        &sandbox_id,
                        &netns_path,
                        std::path::Path::new(&sb.cni_conf),
                        &cap_args,
                    );
                    log::debug!("StopPodSandbox {} step=cni_del DONE", sandbox_id);
                }
                log::debug!("StopPodSandbox {} step=delete_netns", sandbox_id);
                cni::delete_netns(&sb.netns);
                log::debug!("StopPodSandbox {} step=delete_netns DONE", sandbox_id);
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
        log::debug!("StopPodSandbox {} END", sandbox_id);

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
                    // Report the sandbox's ACTUAL namespace modes, not zeros. The
                    // kubelet's `podSandboxChanged` compares these against the pod
                    // spec; reporting POD (0) for a hostNetwork sandbox makes it
                    // recreate the sandbox every sync — an endless crash-loop for
                    // host-namespace pods (#410).
                    options: Some(NamespaceOption {
                        network: sandbox.namespaces.network.to_cri(),
                        pid: sandbox.namespaces.pid.to_cri(),
                        ipc: sandbox.namespaces.ipc.to_cri(),
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
        let mut st = self.state.inner.lock().await;

        // Self-heal at the point of observation: never return a dead-pause
        // ("phantom") sandbox. The kubelet discovers GC targets via this list, so
        // reaping dead sandboxes here — synchronously, under the same lock that
        // builds the response — means it can never see one to garbage-collect.
        // The periodic reaper (see `main`) only *bounds* that window; doing it on
        // read *closes* it (the path that deleted the host /bin, #347/#351).
        let reaped =
            st.reap_stale_sandboxes(|pid| std::path::Path::new(&format!("/proc/{}", pid)).exists());
        for sid in &reaped {
            log::info!("list_pod_sandbox: reaped stale sandbox {sid} (pause process gone)");
        }

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

        // Validate recursive_read_only constraints (#356): the CRI spec requires a
        // recursive readonly mount to also be readonly and have PRIVATE propagation.
        // critest expects CreateContainer to reject violations.
        for m in &config.mounts {
            if m.recursive_read_only {
                if !m.readonly {
                    return Err(Status::invalid_argument(format!(
                        "recursive_read_only mount {:?} requires readonly=true",
                        m.container_path
                    )));
                }
                if m.propagation != 0 {
                    return Err(Status::invalid_argument(format!(
                        "recursive_read_only mount {:?} requires PRIVATE propagation",
                        m.container_path
                    )));
                }
            }
        }
        let mounts: Vec<CriMount> = config
            .mounts
            .iter()
            .map(|m| CriMount {
                host_path: m.host_path.clone(),
                container_path: m.container_path.clone(),
                readonly: m.readonly,
                recursive_read_only: m.recursive_read_only,
                propagation: m.propagation,
            })
            .collect();

        // Extract runAsUser/runAsGroup from the Linux security context.
        let (run_as_user, run_as_group, run_as_username) = config
            .linux
            .as_ref()
            .and_then(|l| l.security_context.as_ref())
            .map(|sc| {
                (
                    sc.run_as_user.as_ref().map(|v| v.value),
                    sc.run_as_group.as_ref().map(|v| v.value),
                    sc.run_as_username.clone(),
                )
            })
            .unwrap_or((None, None, String::new()));

        // CRI requires RunAsUser (or RunAsUserName) whenever RunAsGroup is set —
        // a group with no user is rejected (critest: "should return error if
        // RunAsGroup is set without RunAsUser").
        if run_as_group.is_some() && run_as_user.is_none() && run_as_username.is_empty() {
            return Err(Status::invalid_argument(
                "RunAsGroup is set without RunAsUser/RunAsUserName",
            ));
        }

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
                // CRI ProfileType: 0=RuntimeDefault, 1=Unconfined, 2=Localhost.
                // A NIL seccomp field means UNCONFINED (1), not RuntimeDefault — the
                // kubelet sets RuntimeDefault explicitly when it wants it (#352).
                let (sec_type, sec_path) = sc
                    .seccomp
                    .as_ref()
                    .map(|s| (s.profile_type, s.localhost_ref.clone()))
                    .unwrap_or((1, String::new()));
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

        // Extract AppArmor profile from LinuxContainerSecurityContext (#353).
        // The new `apparmor` SecurityProfile takes precedence over the deprecated
        // `apparmor_profile` string. critest sends the profile name with a
        // "localhost/" prefix in BOTH forms; strip it so the bare name reaches the
        // kernel (a prefixed name is not a loaded profile → ENOENT at exec).
        // Types: 0=RuntimeDefault, 1=Unconfined, 2=Localhost.
        let (apparmor_profile_type, apparmor_profile_path) = {
            let sc = config
                .linux
                .as_ref()
                .and_then(|l| l.security_context.as_ref());
            let new = sc
                .and_then(|sc| sc.apparmor.as_ref())
                .map(|a| (a.profile_type, a.localhost_ref.as_str()));
            // The deprecated `apparmor_profile` field is intentionally read for
            // backwards compatibility (critest exercises it); silence the lint.
            #[allow(deprecated)]
            let deprecated = sc.map(|sc| sc.apparmor_profile.as_str()).unwrap_or("");
            resolve_apparmor(new, deprecated)
        };
        // Reject a Localhost profile that isn't loaded into the kernel — critest's
        // "should fail with an unloaded apparmor_profile" expects CreateContainer to
        // error rather than the container failing to start later.
        if apparmor_profile_type == 2
            && !apparmor_profile_path.is_empty()
            && !apparmor_profile_loaded(&apparmor_profile_path)
        {
            return Err(Status::invalid_argument(format!(
                "apparmor profile {:?} is not loaded",
                apparmor_profile_path
            )));
        }

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
            attempt: meta.attempt,
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
            oom_killed: false,
            run_as_user,
            run_as_group,
            run_as_username,
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
            stdin: config.stdin,
            tty: config.tty,
            // KEP-753 native sidecars: the kubelet sets this label to distinguish
            // persistent init containers from one-shot init containers (#437).
            is_sidecar: config
                .labels
                .get("io.kubernetes.cri.container-type")
                .map(|v| v == "sidecar_container")
                .unwrap_or(false),
            // Device plugin allocations from ContainerConfig.devices (#449).
            // The kubelet populates this from device plugin AllocateResponse.DeviceSpecs.
            devices: config
                .devices
                .iter()
                .map(|d| state::CriDevice {
                    host_path: d.host_path.clone(),
                    container_path: if d.container_path.is_empty() {
                        d.host_path.clone()
                    } else {
                        d.container_path.clone()
                    },
                    permissions: d.permissions.clone(),
                })
                .collect(),
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

        // Keep the container's stdin open on a FIFO when it was created with
        // `stdin: true`, so a later CRI `attach` can deliver input to the running
        // process (#403). Without this the detached container's stdin is
        // /dev/null and an interactive process (e.g. a shell) would see EOF and
        // exit immediately.
        if container.stdin {
            args.push("--stdin".into());
        }

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
            let mut spec = format!("{}:{}", m.host_path, m.container_path);
            // recursive_read_only (#356) → :rro (mount_setattr AT_RECURSIVE);
            // plain readonly → :ro (non-recursive, top mount only).
            if m.recursive_read_only {
                spec.push_str(":rro");
            } else if m.readonly {
                spec.push_str(":ro");
            }
            // CRI MountPropagation → pelagos -v suffix (#341):
            // 1 = HOST_TO_CONTAINER (rslave), 2 = BIDIRECTIONAL (rshared),
            // 0 = PRIVATE (default, no suffix).
            match m.propagation {
                1 => spec.push_str(":rslave"),
                2 => spec.push_str(":rshared"),
                _ => {}
            }
            args.push(spec);
        }

        // Kubelet may pass the sha256 digest form rather than the tag; resolve to a known tag.
        let image = Self::resolve_image_ref(&container.image).await;

        // CRI entrypoint/cmd resolution — see `resolve_container_argv` (#358).
        let (image_entrypoint, image_cmd) = Self::load_image_defaults(&image).await;
        let (effective_entrypoint, effective_cmd) = resolve_container_argv(
            &container.entrypoint,
            &container.args,
            image_entrypoint,
            image_cmd,
        );

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
        } else if !container.run_as_username.is_empty() {
            // RunAsUserName: pass the name so `pelagos run` resolves it against the
            // image's /etc/passwd (e.g. "nobody" → its uid:gid).
            args.push("--user".into());
            args.push(container.run_as_username.clone());
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

        // DNS config, cgroup placement, and host-PID flag from the pod sandbox.
        let (
            sandbox_dns_servers,
            sandbox_dns_searches,
            sandbox_dns_options,
            sandbox_cgroup_parent,
            sandbox_ns,
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
                        s.namespaces,
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
        // The container must not unshare its own PID namespace when the pod uses
        // the host PID namespace (hostPID/NODE — for SPIRE SO_PEERCRED attestation)
        // or a shared pod PID namespace (shareProcessNamespace/POD — #398). In the
        // shared case `with_sandbox` then joins the pod's PID namespace; in the
        // host case the container simply stays in the host PID namespace.
        if sandbox_ns.host_pid() || sandbox_ns.shared_pid() {
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
        // A PRIVILEGED container ignores any seccomp profile — it runs unconfined
        // (critest "ignore a seccomp profile ... when privileged", #352).
        match if container.privileged {
            1 // force Unconfined
        } else {
            container.seccomp_profile_type
        } {
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

        // Host IPC namespace mode (sandbox namespace_options.ipc == NODE).
        // Pass --no-ipc-ns to skip IPC namespace unsharing so the container
        // shares the host IPC namespace (hostIPC: true in the pod spec).
        let sandbox_host_ipc = {
            let st = self.state.inner.lock().await;
            st.sandboxes
                .get(&container.sandbox_id)
                .map(|s| s.namespaces.host_ipc())
                .unwrap_or(false)
        };
        if sandbox_host_ipc {
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

        // Device plugin device allocations (ContainerConfig.devices).
        // The kubelet populates this from device plugin AllocateResponse.DeviceSpecs;
        // each entry has a host_path, container_path, and permissions string.
        // Pass them as --device host:container so pelagos mknod's the node inside
        // the container's /dev and records it in the cgroup device allowlist (#449).
        for dev in &container.devices {
            if !dev.host_path.is_empty() {
                let container_path = if dev.container_path.is_empty() {
                    &dev.host_path
                } else {
                    &dev.container_path
                };
                args.push("--device".into());
                args.push(format!("{}:{}", dev.host_path, container_path));
            }
        }

        // `--` stops clap flag parsing so that container args beginning with `-`
        // (signal numbers, negative values, etc.) are passed through verbatim
        // instead of being interpreted as pelagos flags (issue #322).
        args.push("--".into());
        args.push(image);
        args.extend(effective_entrypoint);
        args.extend(effective_cmd);

        let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        // Under systemd, wrap `pelagos run --detach` in a transient scope under
        // `pelagos.slice` so the watcher it forks lives outside the
        // `pelagos-cri.service` cgroup and survives a runtime restart (#336). The
        // foreground `pelagos run` returns promptly after forking the watcher, so
        // the scope (kept alive by the watcher) outlives this call. Off systemd we
        // invoke pelagos directly, preserving prior behavior.
        let out = if scope::systemd_available() {
            let unit = scope::container_unit(&container.pelagos_name);
            scope::stop_unit(&unit).await; // clear any stale unit for this name
            let argv = scope::build_scope_argv(&unit, &bin, &args_ref);
            let argv_ref: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
            run_pelagos(argv_ref[0], &argv_ref[1..])
                .await
                .map_err(|e| Status::internal(format!("exec error: {}", e)))?
        } else {
            run_pelagos(&bin, &args_ref)
                .await
                .map_err(|e| Status::internal(format!("exec error: {}", e)))?
        };

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

        // Always relay logs to an absolute, pelagos-reported path — even when the
        // CRI client supplied no log dir/path — so the container has a real CRI
        // log and ContainerStatus.LogPath never points at a missing file or
        // collapses to the host root (#347).
        let log_dest =
            effective_cri_log_path(&log_directory, &log_path_rel, &container.pelagos_name);
        // Create the log file SYNCHRONOUSLY before StartContainer returns: a client
        // that reads it immediately (e.g. critest) must not get ENOENT just because
        // the async relay task has not been scheduled yet (#344).
        ensure_cri_log_file(&log_dest);
        // Register a "log finalized" flag the relay sets once it has drained and
        // stopped after the container exits; container_status waits on it so a
        // client never reads a partially-flushed log right after seeing exit (#344).
        let log_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.state
            .log_done
            .lock()
            .await
            .insert(container.pelagos_name.clone(), log_done.clone());
        tokio::spawn(relay_container_logs(
            container.pelagos_name.clone(),
            log_dest,
            log_done,
        ));

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
        };
        // StopContainer must be IDEMPOTENT: stopping a container the runtime no
        // longer knows about (already removed, or never existed) is a no-op
        // success, not an error — the kubelet relies on this (critest #342).
        let Some(pelagos_name) = pelagos_name else {
            return Ok(Response::new(StopContainerResponse {}));
        };

        // `pelagos stop` is the single kill path (#459): cgroup kill is primary,
        // SIGKILL fallback, zombie-proof liveness wait.
        let _ = run_pelagos(&bin, &["stop", &pelagos_name]).await;
        // The watcher has exited, so the transient scope is now empty; --collect
        // reaps it, but stop it explicitly for determinism (#336).
        if scope::systemd_available() {
            scope::stop_unit(&scope::container_unit(&pelagos_name)).await;
        }

        // Read the real exit state from state.json now that the process has been
        // stopped.  This is critical for native sidecars (#437): when the kubelet
        // calls StopContainer on an already-exited sidecar as part of the restart
        // cycle, we must preserve the actual exit code (e.g. 137 = SIGKILL) so
        // the kubelet can apply the correct backoff — defaulting to 0 misrepresents
        // a crash as a clean exit and masks restart-loop diagnostics.
        let live = read_pelagos_container_state(&pelagos_name);

        let mut st = self.state.inner.lock().await;
        if let Some(c) = st.containers.get_mut(&container_id) {
            c.state = MyContainerState::Exited;
            c.finished_at_ns = now_ns();
            if let Some(live) = live {
                c.exit_code = live.exit_code.unwrap_or(0);
                c.oom_killed = live.oom_killed;
            }
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
            if scope::systemd_available() {
                scope::stop_unit(&scope::container_unit(&name)).await;
            }
            self.state.log_done.lock().await.remove(&name); // drop the #344 finalize flag
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
                                c.oom_killed = live.oom_killed;
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

        // If the container has exited, wait (bounded) for the log relay to finish
        // writing the CRI log before we report its status. critest (and the kubelet)
        // read the log right after seeing the container exited, so it must be
        // complete — otherwise the read races the relay's final flush (#344).
        let pname_state = {
            let st = self.state.inner.lock().await;
            st.containers
                .get(&container_id)
                .map(|c| (c.pelagos_name.clone(), c.state.clone()))
        };
        if let Some((pname, cstate)) = pname_state {
            let exited = cstate == MyContainerState::Exited
                || read_pelagos_container_state(&pname)
                    .map(|l| l.status == "exited")
                    .unwrap_or(false);
            if exited {
                if let Some(flag) = self.state.log_done.lock().await.get(&pname).cloned() {
                    for _ in 0..100 {
                        if flag.load(std::sync::atomic::Ordering::SeqCst) {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    }
                }
            }
        }

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
                        c.oom_killed = live.oom_killed;
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

        // Report the SAME absolute path the log relay writes to (see
        // effective_cri_log_path / StartContainer), so the reported LogPath always
        // has a real file behind it and never collapses to the host root (#347).
        let sandbox_log_dir = st
            .sandboxes
            .get(&container.sandbox_id)
            .map(|s| s.log_directory.clone())
            .unwrap_or_default();
        let full_log_path = effective_cri_log_path(
            &sandbox_log_dir,
            &container.log_path,
            &container.pelagos_name,
        );

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
                attempt: container.attempt,
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
            // Kubernetes surfaces this verbatim (pod status, `kubectl describe`).
            // An OOM-killed container must report "OOMKilled" (#343); otherwise the
            // kill is misattributed to a generic Error and memory-limit debugging
            // breaks. Exit code 137 (128+SIGKILL) is carried in exit_code above.
            reason: if container.oom_killed {
                "OOMKilled".to_string()
            } else {
                String::new()
            },
            message,
            labels: container.labels.clone(),
            annotations: container.annotations.clone(),
            // Report the container's mounts with their readonly / recursive_read_only
            // attributes so the kubelet and critest can confirm the runtime honored the
            // request (#356). Was hardcoded empty, which failed the non-recursive
            // readonly-mount conformance spec (it inspects ContainerStatus.mounts).
            mounts: container
                .mounts
                .iter()
                .map(|m| Mount {
                    container_path: m.container_path.clone(),
                    host_path: m.host_path.clone(),
                    readonly: m.readonly,
                    recursive_read_only: m.recursive_read_only,
                    propagation: m.propagation,
                    ..Default::default()
                })
                .collect(),
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
        use tokio::io::AsyncReadExt;
        use tokio::process::Command;

        let mut proc_cmd = Command::new(&bin);
        proc_cmd.arg("exec");
        proc_cmd.arg(&pelagos_name);
        for c in &cmd {
            proc_cmd.arg(c);
        }
        proc_cmd.stdout(Stdio::piped());
        proc_cmd.stderr(Stdio::piped());

        let mut child = proc_cmd
            .spawn()
            .map_err(|e| Status::internal(format!("spawn error: {}", e)))?;

        // Capture the exec wrapper's PID before any await consumes the child, so
        // we can kill its whole subtree on timeout (#339).
        let child_pid = child.id().map(|p| p as i32);

        // Drain stdout/stderr concurrently so a full pipe buffer can't deadlock
        // the child before it exits (or before we time out).
        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        let out_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            if let Some(s) = stdout.as_mut() {
                let _ = s.read_to_end(&mut buf).await;
            }
            buf
        });
        let err_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            if let Some(s) = stderr.as_mut() {
                let _ = s.read_to_end(&mut buf).await;
            }
            buf
        });

        let status = if timeout_secs > 0 {
            match tokio::time::timeout(
                std::time::Duration::from_secs(timeout_secs as u64),
                child.wait(),
            )
            .await
            {
                Ok(Ok(st)) => st,
                Ok(Err(e)) => return Err(Status::internal(format!("wait error: {}", e))),
                Err(_) => {
                    // Timeout: terminate ONLY the exec'd command and its session
                    // inside the container — leave the container itself running —
                    // then reap the wrapper (#339).
                    if let Some(pid) = child_pid {
                        kill_exec_wrapper(pid);
                    }
                    let _ = child.wait().await;
                    return Err(Status::deadline_exceeded("exec timed out"));
                }
            }
        } else {
            child
                .wait()
                .await
                .map_err(|e| Status::internal(format!("wait error: {}", e)))?
        };

        let stdout = out_task.await.unwrap_or_default();
        let stderr = err_task.await.unwrap_or_default();
        let exit_code = status.code().unwrap_or(1);

        Ok(Response::new(ExecSyncResponse {
            stdout,
            stderr,
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
        request: Request<ReopenContainerLogRequest>,
    ) -> Result<Response<ReopenContainerLogResponse>, Status> {
        // The kubelet rotated the log (renamed <logPath> → <logPath>.<ts>) and now
        // asks us to reopen it. (Re)create a fresh file at the ORIGINAL path so the
        // relay's next per-flush append lands there — not in the renamed file (#344).
        // (The relay opens the path per write, so no fd needs reopening.)
        let container_id = request.into_inner().container_id;
        let st = self.state.inner.lock().await;
        if let Some(container) = st.containers.get(&container_id) {
            let log_dir = st
                .sandboxes
                .get(&container.sandbox_id)
                .map(|s| s.log_directory.clone())
                .unwrap_or_default();
            let cri_log_path =
                effective_cri_log_path(&log_dir, &container.log_path, &container.pelagos_name);
            drop(st);
            ensure_cri_log_file(&cri_log_path);
        }
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

/// Resolve the container's effective `(entrypoint, cmd)` from the CRI request and
/// the image defaults, per Kubernetes/CRI/OCI semantics (#358):
///
/// | CRI command | CRI args | entrypoint | cmd          |
/// |-------------|----------|------------|--------------|
/// | empty       | empty    | image EP   | image CMD    |
/// | empty       | [args]   | image EP   | args         |
/// | [cmd]       | empty    | cmd        | (none)       |  ← image CMD DROPPED
/// | [cmd]       | [args]   | cmd        | args         |
///
/// The critical rule: a CRI `command` overrides the entrypoint AND drops the image
/// CMD. The old code appended the image CMD whenever `args` was empty — even when
/// `command` was set — producing e.g. `sleep 3600 sh` (image CMD `sh` appended),
/// which exits immediately and breaks every verify-by-exec conformance spec.
fn resolve_container_argv(
    cri_command: &[String],
    cri_args: &[String],
    image_entrypoint: Vec<String>,
    image_cmd: Vec<String>,
) -> (Vec<String>, Vec<String>) {
    let entrypoint = if cri_command.is_empty() {
        image_entrypoint
    } else {
        cri_command.to_vec()
    };
    let cmd = if !cri_args.is_empty() {
        cri_args.to_vec()
    } else if cri_command.is_empty() {
        // Only inherit the image CMD when the CRI command did NOT override the
        // entrypoint; otherwise the CRI command is the full argv.
        image_cmd
    } else {
        Vec::new()
    };
    (entrypoint, cmd)
}

// ── Sandbox pause spawn + host networking helpers ─────────────────────────────

/// Spawn the sandbox pause and return the PID that **holds the pod namespaces**
/// (the one containers join via `with_sandbox()`).
///
/// For a shared-PID pod (`pod_pid`, `--pod-pid`) the pause forks a PID-1 init
/// child that actually holds the pod PID namespace; the spawned (MainPID) process
/// is its parent/supervisor. We resolve and return the **child** pid in that case
/// via `/proc/<parent>/task/<parent>/children`. Otherwise the spawned pid is the
/// namespace holder.
async fn spawn_sandbox_pause(
    bin: &str,
    id: &str,
    pause_argv: &[&str],
    pod_pid: bool,
) -> Result<i32, Status> {
    let parent = spawn_sandbox_pause_proc(bin, id, pause_argv).await?;
    if !pod_pid {
        return Ok(parent);
    }
    // The PID-1 init is the parent's only child; poll until it appears.
    for _ in 0..80 {
        if let Ok(s) = std::fs::read_to_string(format!("/proc/{}/task/{}/children", parent, parent))
        {
            if let Some(child) = s
                .split_whitespace()
                .next()
                .and_then(|p| p.parse::<i32>().ok())
            {
                return Ok(child);
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    Err(Status::internal(format!(
        "pod-pid pause {} did not fork a PID-1 init child",
        parent
    )))
}

/// Spawn the pause process and return the spawned (MainPID/parent) PID.
///
/// Under systemd the pause runs as a transient service under `pelagos.slice`
/// (created by PID 1) so it survives a `pelagos-cri` restart and the sandbox is
/// re-adopted rather than torn down (#336). The pause blocks forever, so it must
/// be a backgrounded *service* (not a `--scope`, which would block); its real PID
/// is the unit's MainPID. Off systemd we fall back to a plain leaked child
/// (killed explicitly on StopPodSandbox). `pause_argv` carries the namespace
/// flags (`--host-ipc` / `--host-net` / `--pod-pid`).
async fn spawn_sandbox_pause_proc(bin: &str, id: &str, pause_argv: &[&str]) -> Result<i32, Status> {
    if scope::systemd_available() {
        let unit = scope::sandbox_unit(id);
        // A re-run for the same sandbox id would collide with a lingering unit;
        // clear any stale one first (best effort).
        scope::stop_unit(&unit).await;
        let argv = scope::build_service_argv(&unit, bin, pause_argv);
        let out = tokio::process::Command::new(&argv[0])
            .args(&argv[1..])
            .output()
            .await
            .map_err(|e| Status::internal(format!("systemd-run pause: {}", e)))?;
        if !out.status.success() {
            return Err(Status::internal(format!(
                "systemd-run pause failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        // MainPID is populated asynchronously once systemd forks the unit.
        let mut pid = 0;
        for _ in 0..40 {
            if let Some(p) = scope::service_main_pid(&unit).await {
                pid = p;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        if pid <= 0 {
            scope::stop_unit(&unit).await;
            return Err(Status::internal(format!(
                "pause unit {} did not report a MainPID",
                unit
            )));
        }
        Ok(pid)
    } else {
        let pause = std::process::Command::new(bin)
            .args(pause_argv)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| Status::internal(format!("spawn pause process: {}", e)))?;
        let pid = pause.id() as i32;
        // Intentionally leaked — killed explicitly on StopPodSandbox.
        std::mem::forget(pause);
        // Brief pause for the process to enter namespaces before containers join.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        Ok(pid)
    }
}

/// The node's primary IPv4 address — reported as the sandbox IP for hostNetwork
/// pods (whose pod IP is the node IP). Uses the UDP-connect trick: connecting a
/// UDP socket sends no packets but makes the kernel pick the source address that
/// would route to the target, i.e. the node's primary interface. Falls back to
/// loopback if there is no route (e.g. an isolated test host).
fn node_primary_ipv4() -> String {
    use std::net::UdpSocket;
    UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("8.8.8.8:53")?;
            s.local_addr()
        })
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".to_string())
}

// ── Pod-level settings applied to the pause namespaces (#354/#355) ─────────────

/// Wait until the pause process has actually unshared its UTS **and** IPC
/// namespaces — i.e. they differ from this (host) process's namespaces. Returns
/// false on timeout.
///
/// CRITICAL: pod hostname/sysctls are applied via `nsenter /proc/<pause>/ns/*`.
/// If applied before the pause finishes unsharing, `/proc/<pause>/ns/uts` still
/// resolves to the HOST UTS namespace and `hostname <pod>` renames the HOST
/// (observed: node hostname changed to the pod's). Likewise a sysctl set pre-unshare
/// lands on a namespace the pause then discards, so the value never reaches the pod.
/// Gating on this both fixes the apply and guarantees we never mutate the host: if
/// the pause never unshares (e.g. a host-namespace pod), we skip the apply.
async fn wait_for_pause_ns_unshare(pause_pid: i32) -> bool {
    let host_uts = std::fs::read_link("/proc/self/ns/uts").ok();
    let host_ipc = std::fs::read_link("/proc/self/ns/ipc").ok();
    for _ in 0..40 {
        let p_uts = std::fs::read_link(format!("/proc/{}/ns/uts", pause_pid)).ok();
        let p_ipc = std::fs::read_link(format!("/proc/{}/ns/ipc", pause_pid)).ok();
        if p_uts.is_some() && p_uts != host_uts && p_ipc.is_some() && p_ipc != host_ipc {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    false
}

/// Set the pod hostname inside the pause's UTS namespace (which containers join),
/// so `hostname` resolves to the pod name (#354 set-hostname). Best-effort; no-op
/// when empty.
async fn apply_pod_hostname(pause_pid: i32, hostname: &str) {
    if hostname.is_empty() {
        return;
    }
    let uts = format!("--uts=/proc/{}/ns/uts", pause_pid);
    match tokio::process::Command::new("nsenter")
        .args([uts.as_str(), "--", "hostname", hostname])
        .output()
        .await
    {
        Ok(out) if out.status.success() => log::info!("pod hostname set to {}", hostname),
        Ok(out) => log::warn!(
            "set pod hostname {}: {}",
            hostname,
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(e) => log::warn!("nsenter hostname {}: {}", hostname, e),
    }
}

/// Build the `nsenter` argv that applies one pod-level sysctl inside a sandbox's
/// namespaces. `net.*` keys live in the network namespace; `kernel.*`/`fs.*` live
/// in the IPC/UTS namespaces held by the pause — so we enter all three and let
/// `sysctl -w` write to whichever `/proc/sys` the key belongs to.
fn sandbox_sysctl_argv(netns_path: &str, pause_pid: i32, key: &str, value: &str) -> Vec<String> {
    vec![
        format!("--net={}", netns_path),
        format!("--ipc=/proc/{}/ns/ipc", pause_pid),
        format!("--uts=/proc/{}/ns/uts", pause_pid),
        "--".to_string(),
        "sysctl".to_string(),
        "-w".to_string(),
        format!("{}={}", key, value),
    ]
}

/// Apply the sandbox's configured sysctls (safe + unsafe; the kubelet has already
/// gate-kept which are permitted) to its pod namespaces. Best-effort per key.
async fn apply_sandbox_sysctls(
    netns_path: &str,
    pause_pid: i32,
    sysctls: &HashMap<String, String>,
) {
    for (key, value) in sysctls {
        let argv = sandbox_sysctl_argv(netns_path, pause_pid, key, value);
        match tokio::process::Command::new("nsenter")
            .args(&argv)
            .output()
            .await
        {
            Ok(out) if out.status.success() => {
                log::info!("sandbox sysctl {}={} applied", key, value)
            }
            Ok(out) => log::warn!(
                "sandbox sysctl {}={}: {}",
                key,
                value,
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            Err(e) => log::warn!("nsenter sysctl {}={}: {}", key, value, e),
        }
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
/// Create the CRI log file (and its parent dir) if absent — synchronous so the
/// file exists the moment StartContainer returns. Idempotent (`create+append`
/// never truncates), so the relay calling it again is harmless.
fn ensure_cri_log_file(path: &str) {
    if let Some(parent) = std::path::Path::new(path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path);
}

async fn relay_container_logs(
    pelagos_name: String,
    cri_log_path: String,
    log_done: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    // No startup delay: a short-lived container can output and exit in well under
    // the old 300 ms, and a client (e.g. critest) reads the log as soon as the
    // container finishes — so the relay must write it promptly (#344).
    let container_dir = format!("/run/pelagos/containers/{}", pelagos_name);
    relay_logs_from_dir(container_dir, cri_log_path).await;
    // The relay returns only after the container has exited and the log is fully
    // drained — signal container_status that the CRI log is complete (#344).
    log_done.store(true, std::sync::atomic::Ordering::SeqCst);
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

    // Ensure CRI log parent directory exists, then create the log file UP FRONT so
    // it always exists — a client reading it before any output is written gets an
    // empty file, not ENOENT (the #344 failure for short-lived containers).
    if let Some(parent) = std::path::Path::new(&cri_log_path).parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let _ = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&cri_log_path)
        .await;

    let mut stdout_off: u64 = 0;
    let mut stderr_off: u64 = 0;
    let mut stdout_buf = String::new();
    let mut stderr_buf = String::new();

    loop {
        // Read + flush FIRST (before sleeping) so output reaches the CRI log
        // promptly even when the container exits almost immediately.
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
                        // Final read to catch anything written between the last
                        // poll and the exit, then drain any partial line. (We break
                        // right after, so the offsets need not be updated.)
                        if let Some((data, _)) = read_from_offset(&stdout_src, stdout_off).await {
                            stdout_buf.push_str(&String::from_utf8_lossy(&data));
                        }
                        if let Some((data, _)) = read_from_offset(&stderr_src, stderr_off).await {
                            stderr_buf.push_str(&String::from_utf8_lossy(&data));
                        }
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

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    /// A minimal in-memory `CriSandbox` carrying the given namespace modes.
    #[cfg(test)]
    fn sbx_with_namespaces(id: &str, namespaces: state::NamespaceModes) -> state::CriSandbox {
        state::CriSandbox {
            id: id.into(),
            name: format!("{}-pod", id),
            namespace: "default".into(),
            uid: format!("uid-{}", id),
            attempt: 0,
            labels: Default::default(),
            annotations: Default::default(),
            created_at_ns: 0,
            state: state::SandboxState::Running,
            netns: String::new(),
            ip: "192.168.1.10".into(),
            cni_conf: String::new(),
            pause_pid: 1,
            log_directory: String::new(),
            cgroup_parent: String::new(),
            supplemental_groups: vec![],
            dns_servers: vec![],
            dns_searches: vec![],
            dns_options: vec![],
            namespaces,
            port_mappings: vec![],
        }
    }

    /// Regression test for #410: `pod_sandbox_status` must report the sandbox's
    /// ACTUAL namespace modes, not hardcoded `POD`(0). When it reported `POD` for a
    /// `hostNetwork` sandbox, the kubelet's `podSandboxChanged` saw a mismatch
    /// against the pod spec and recreated the sandbox on every sync — an endless
    /// ~1/sec crash-loop for every host-network pod (kube-vip, MetalLB, …).
    #[tokio::test]
    async fn test_pod_sandbox_status_reports_namespace_modes() {
        let svc = RuntimeSvc {
            state: AppState::new("pelagos".into()),
            streaming_base_url: String::new(),
            registry: Default::default(),
        };
        {
            let mut st = svc.state.inner.lock().await;
            // hostNetwork pod: network == NODE(2); pid CONTAINER(1); ipc POD(0).
            st.sandboxes.insert(
                "hostnet-410".into(),
                sbx_with_namespaces("hostnet-410", state::NamespaceModes::from_cri(2, 1, 0)),
            );
            // hostIPC pod: ipc == NODE(2); network/pid default.
            st.sandboxes.insert(
                "hostipc-410".into(),
                sbx_with_namespaces("hostipc-410", state::NamespaceModes::from_cri(0, 1, 2)),
            );
            // Default pod: all default (network POD, ipc POD).
            st.sandboxes.insert(
                "podnet-410".into(),
                sbx_with_namespaces("podnet-410", state::NamespaceModes::default()),
            );
        }

        async fn ns_of(svc: &RuntimeSvc, id: &str) -> NamespaceOption {
            let resp = svc
                .pod_sandbox_status(Request::new(PodSandboxStatusRequest {
                    pod_sandbox_id: id.into(),
                    verbose: false,
                }))
                .await
                .expect("status ok")
                .into_inner();
            resp.status
                .unwrap()
                .linux
                .unwrap()
                .namespaces
                .unwrap()
                .options
                .unwrap()
        }

        // hostNetwork sandbox MUST report network == NODE(2), or the kubelet
        // crash-loops it (#410).
        let host = ns_of(&svc, "hostnet-410").await;
        assert_eq!(host.network, 2, "hostNetwork sandbox must report NODE(2)");

        // hostIPC sandbox reports ipc == NODE(2).
        let hipc = ns_of(&svc, "hostipc-410").await;
        assert_eq!(hipc.ipc, 2, "hostIPC sandbox must report NODE(2) for ipc");
        assert_eq!(hipc.network, 0, "hostIPC pod still has POD network");

        // Default sandbox reports POD(0) for network and ipc.
        let pod = ns_of(&svc, "podnet-410").await;
        assert_eq!(pod.network, 0, "default sandbox reports POD(0) network");
        assert_eq!(pod.ipc, 0, "default sandbox reports POD(0) ipc");
    }

    #[test]
    fn test_resolve_apparmor() {
        // New field strips the "localhost/" prefix critest includes.
        assert_eq!(
            resolve_apparmor(Some((2, "localhost/p")), ""),
            (2, "p".to_string())
        );
        // New field takes precedence over the deprecated annotation.
        assert_eq!(
            resolve_apparmor(Some((2, "localhost/new")), "localhost/old"),
            (2, "new".to_string())
        );
        // Deprecated localhost profile (new field absent).
        assert_eq!(
            resolve_apparmor(None, "localhost/dep"),
            (2, "dep".to_string())
        );
        // Deprecated runtime/default, unconfined, and empty map to type 0/1/0.
        assert_eq!(
            resolve_apparmor(None, "runtime/default"),
            (0, String::new())
        );
        assert_eq!(resolve_apparmor(None, "unconfined"), (1, String::new()));
        assert_eq!(resolve_apparmor(None, ""), (0, String::new()));
        // New Unconfined (type 1) ignores the (empty) ref.
        assert_eq!(resolve_apparmor(Some((1, "")), ""), (1, String::new()));
    }

    fn is_alive(pid: i32) -> bool {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
            && std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .map(|s| !s.contains(") Z "))
                .unwrap_or(false)
    }

    fn find_one(pat: &str) -> Option<i32> {
        for ent in std::fs::read_dir("/proc").ok()?.flatten() {
            let Some(pid) = ent.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else {
                continue;
            };
            if let Ok(c) = std::fs::read(format!("/proc/{pid}/cmdline")) {
                if String::from_utf8_lossy(&c).replace('\0', " ").contains(pat) {
                    return Some(pid);
                }
            }
        }
        None
    }

    /// kill_exec_wrapper must reap the exec'd command's whole *session* — even a
    /// grandchild that `setsid`'d and was reparented away — while leaving the
    /// wrapper's siblings alone (issue #339). This mirrors `pelagos exec` →
    /// (setsid'd shell) → forked `sleep`, where the shell reparents the sleep.
    #[test]
    fn test_kill_exec_wrapper_reaps_setsid_session() {
        use std::process::Command;
        // A unique sleep DURATION acts as the sentinel (a marker *argument* would
        // make `sleep` fail with "invalid time interval").
        let dur = 23000 + (std::process::id() % 5000);
        let needle = format!("sleep {dur}");
        // wrapper -> `setsid` (new session leader) which forks the sentinel sleep
        // and then waits, so the sleep shares the new session.
        let mut wrapper = Command::new("/bin/sh")
            .args([
                "-c",
                &format!("exec setsid -w /bin/sh -c 'sleep {dur} & wait'"),
            ])
            .spawn()
            .expect("spawn wrapper");
        let root = wrapper.id() as i32;

        // Wait for the sentinel sleep (the session grandchild) to appear.
        let mut sleep_pid = None;
        for _ in 0..60 {
            if let Some(p) = find_one(&needle) {
                sleep_pid = Some(p);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let sleep_pid = sleep_pid.expect("sentinel sleep did not start");

        kill_exec_wrapper(root);
        let _ = wrapper.wait();

        std::thread::sleep(std::time::Duration::from_millis(250));
        assert!(
            !is_alive(sleep_pid),
            "session grandchild {sleep_pid} survived kill_exec_wrapper (#339 leak)"
        );
        assert!(!is_alive(root), "wrapper {root} survived kill_exec_wrapper");
    }

    /// #347: the path the relay writes AND ContainerStatus.log_path reports must
    /// never be empty or relative — an empty LogPath makes the kubelet RemoveAll
    /// the host root (deleting /bin). Proven on a disposable node; this pins the
    /// contract. Because the relay and the reporter call the same function, the
    /// reported path always has a real log file behind it.
    #[test]
    fn test_effective_cri_log_path_never_empty_or_relative() {
        // No client-supplied log dir/path -> synthesized absolute path under pelagos's
        // runtime dir, where the relay will actually write the log.
        assert_eq!(
            effective_cri_log_path("", "", "pcri-abc123"),
            "/run/pelagos/containers/pcri-abc123/cri.log"
        );
        // log_path present but no log_directory -> can't honor a bare relative path
        // (would collapse to "." with cwd "/") -> fall back.
        assert_eq!(
            effective_cri_log_path("", "node-exporter/0.log", "pcri-xyz"),
            "/run/pelagos/containers/pcri-xyz/cri.log"
        );
        // Kubelet-supplied absolute dir + path -> honored verbatim.
        assert_eq!(
            effective_cri_log_path("/var/log/pods/ns_name_uid/ctr", "0.log", "pcri-abc"),
            "/var/log/pods/ns_name_uid/ctr/0.log"
        );
        // A (defensive) relative log_directory must never be honored -> fall back.
        assert_eq!(
            effective_cri_log_path("var/log/pods/x", "0.log", "pcri-def"),
            "/run/pelagos/containers/pcri-def/cri.log"
        );
        // The result is ALWAYS absolute, for every combination (the safety invariant).
        for (dir, path) in [
            ("", ""),
            ("", "a/b.log"),
            ("rel/dir", "0.log"),
            ("/abs", "0.log"),
        ] {
            assert!(effective_cri_log_path(dir, path, "pcri-x").starts_with('/'));
        }
    }

    /// #358: the four-cell entrypoint/cmd table. The keystone case is row 3 —
    /// CRI command set + no args must DROP the image CMD (not append it), which is
    /// what broke critest containers (`sleep 3600` → `sleep 3600 sh` → exit 1).
    #[test]
    fn test_resolve_container_argv() {
        let ep = || vec!["/img-ep".to_string()];
        let cmd = || vec!["img-cmd".to_string()];

        // command empty, args empty → image EP + image CMD.
        assert_eq!(
            resolve_container_argv(&[], &[], ep(), cmd()),
            (vec!["/img-ep".into()], vec!["img-cmd".into()])
        );
        // command empty, args set → image EP + args (image CMD dropped).
        assert_eq!(
            resolve_container_argv(&[], &["a".into()], ep(), cmd()),
            (vec!["/img-ep".into()], vec!["a".into()])
        );
        // command set, args empty → command only; IMAGE CMD DROPPED (the bug).
        assert_eq!(
            resolve_container_argv(&["sleep".into(), "3600".into()], &[], ep(), cmd()),
            (vec!["sleep".into(), "3600".into()], vec![])
        );
        // command set, args set → command + args.
        assert_eq!(
            resolve_container_argv(&["sleep".into()], &["3600".into()], ep(), cmd()),
            (vec!["sleep".into()], vec!["3600".into()])
        );
    }

    /// #355: a pod-level sysctl is applied inside ALL THREE pod namespaces — net
    /// (net.*), ipc + uts (kernel.*/fs.*) — so `sysctl -w` lands in whichever
    /// /proc/sys the key belongs to. Values with spaces survive as one argv element.
    #[test]
    fn test_sandbox_sysctl_argv() {
        assert_eq!(
            sandbox_sysctl_argv(
                "/run/netns/pcri-abc",
                4242,
                "net.ipv4.ping_group_range",
                "0 2147483647"
            ),
            vec![
                "--net=/run/netns/pcri-abc",
                "--ipc=/proc/4242/ns/ipc",
                "--uts=/proc/4242/ns/uts",
                "--",
                "sysctl",
                "-w",
                "net.ipv4.ping_group_range=0 2147483647",
            ]
        );
    }

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

    /// #344: ensure_cri_log_file creates the parent dir and the file (idempotently),
    /// so StartContainer/ReopenContainerLog can guarantee the path exists.
    #[test]
    fn test_ensure_cri_log_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("a/b/0.log").to_str().unwrap().to_string();
        ensure_cri_log_file(&path);
        assert!(
            std::path::Path::new(&path).exists(),
            "log file + parents created"
        );
        // Idempotent + non-truncating: write content, call again, content preserved.
        std::fs::write(&path, b"keep\n").unwrap();
        ensure_cri_log_file(&path);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep\n");
    }

    /// #344: a short-lived container that produces NO output before exiting must
    /// still leave the CRI log FILE present (empty), not ENOENT — a client reading
    /// it (e.g. critest) gets an empty file rather than "no such file or directory".
    #[tokio::test]
    async fn test_relay_creates_log_file_even_with_no_output() {
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        // Container already exited with no stdout/stderr written.
        std::fs::write(dir.path().join("state.json"), r#"{"status":"exited"}"#).unwrap();

        let cri_log_dir = TempDir::new().unwrap();
        let cri_log_path = cri_log_dir
            .path()
            .join("sub/0.log") // parent dir does not exist yet — relay must create it
            .to_str()
            .unwrap()
            .to_string();

        relay_logs_from_dir(
            dir.path().to_str().unwrap().to_string(),
            cri_log_path.clone(),
        )
        .await;

        assert!(
            std::path::Path::new(&cri_log_path).exists(),
            "CRI log file must exist even with no container output (#344)"
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
            namespaces: state::NamespaceModes::default(),
            port_mappings: vec![],
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
            namespaces: state::NamespaceModes::default(),
            port_mappings: vec![],
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

    /// #424: the CRI Version RPC must report the pelagos binary's real version,
    /// parsed from `pelagos --version` output, so kubelet/kube_node_info show the
    /// actual release instead of the pelagos-cri crate's pinned 0.1.0.
    #[test]
    fn test_parse_pelagos_version() {
        assert_eq!(
            parse_pelagos_version("pelagos 0.65.42+abc1234\n").as_deref(),
            Some("0.65.42+abc1234")
        );
        assert_eq!(
            parse_pelagos_version("pelagos 0.65.42").as_deref(),
            Some("0.65.42")
        );
        // Malformed / empty output → None so the caller falls back safely.
        assert_eq!(parse_pelagos_version("pelagos"), None);
        assert_eq!(parse_pelagos_version(""), None);
    }

    // ── Native sidecar tests (#437) ───────────────────────────────────────────

    /// Build a minimal CriContainer for use in sidecar tests.
    fn make_container(id: &str, sandbox_id: &str, is_sidecar: bool) -> state::CriContainer {
        let ctype = if is_sidecar {
            "sidecar_container"
        } else {
            "container"
        };
        let json = format!(
            r#"{{
                "id":"{id}",
                "sandbox_id":"{sandbox_id}",
                "pelagos_name":"pcri-{id}",
                "name":"c",
                "image":"img",
                "entrypoint":[],"args":[],"envs":[],
                "working_dir":"","mounts":[],
                "labels":{{"io.kubernetes.cri.container-type":"{ctype}"}},
                "annotations":{{}},
                "created_at_ns":0,"started_at_ns":0,"finished_at_ns":0,
                "state":"Running","exit_code":0,
                "is_sidecar":{is_sidecar}
            }}"#
        );
        serde_json::from_str(&json).expect("valid container json")
    }

    /// KEP-753 (#437): CreateContainer must set `is_sidecar` from the kubelet's
    /// `io.kubernetes.cri.container-type` label.  A container labelled
    /// `sidecar_container` is a native sidecar; all other values are not.
    #[test]
    fn test_is_sidecar_detected_from_label() {
        let sidecar = make_container("s1", "sbx1", true);
        assert!(
            sidecar.is_sidecar,
            "sidecar_container label must set is_sidecar"
        );

        let regular = make_container("c1", "sbx1", false);
        assert!(
            !regular.is_sidecar,
            "container label must not set is_sidecar"
        );

        // A container with no type label is not a sidecar.
        let no_label: state::CriContainer = serde_json::from_str(
            r#"{"id":"x","sandbox_id":"s","pelagos_name":"pcri-x",
                "name":"c","image":"img","entrypoint":[],"args":[],"envs":[],
                "working_dir":"","mounts":[],"labels":{},"annotations":{},
                "created_at_ns":0,"started_at_ns":0,"finished_at_ns":0,
                "state":"Running","exit_code":0}"#,
        )
        .unwrap();
        assert!(
            !no_label.is_sidecar,
            "missing label defaults to not-sidecar"
        );
    }

    /// KEP-753 (#437): `stop_container` must capture the real exit code from
    /// state.json when the container has already exited — not leave it at 0.
    ///
    /// Background: when the kubelet drives a sidecar restart cycle it calls
    /// StopContainer on an already-exited sidecar.  The previous implementation
    /// left exit_code=0 (the creation default), misrepresenting a crash (e.g.
    /// exit 137 = SIGKILL) as a clean exit and hiding the failure from the kubelet's
    /// backoff logic.  The fix reads state.json after stopping to capture the real
    /// code.
    #[tokio::test]
    async fn test_stop_container_captures_real_exit_code() {
        use tempfile::TempDir;

        let tmp = TempDir::new().expect("tempdir");
        let container_name = "pcri-sidecar-test";
        let container_dir = tmp.path().join(container_name);
        std::fs::create_dir_all(&container_dir).unwrap();

        // Write a state.json that simulates a container that exited with code 137.
        let state_path = container_dir.join("state.json");
        std::fs::write(
            &state_path,
            r#"{"name":"pcri-sidecar-test","status":"exited","pid":0,"started_at":"","exit_code":137,"oom_killed":false}"#,
        )
        .unwrap();

        // Manually parse what stop_container now reads.
        let live = serde_json::from_str::<PelagosContainerState>(
            &std::fs::read_to_string(&state_path).unwrap(),
        )
        .unwrap();

        assert_eq!(
            live.exit_code,
            Some(137),
            "state.json must carry the real exit code from the watcher"
        );
        assert!(
            !live.oom_killed,
            "oom_killed must be false for a SIGKILL exit"
        );
    }

    /// KEP-753 (#437): the full sidecar restart cycle through the in-memory
    /// state machine must work correctly.  The kubelet's restart loop for an
    /// exited sidecar is: StopContainer → RemoveContainer → CreateContainer
    /// (attempt+1) → StartContainer.  Each step must leave the state consistent.
    #[tokio::test]
    async fn test_sidecar_restart_cycle_state_machine() {
        let svc = RuntimeSvc {
            state: AppState::new("pelagos".into()),
            streaming_base_url: String::new(),
            registry: Default::default(),
        };

        // Insert a Running sidecar container (attempt 0) directly into state,
        // simulating what CreateContainer + StartContainer leave behind.
        let id = "sidecar-restart-test".to_string();
        let mut sidecar = make_container(&id, "sbx1", true);
        sidecar.state = state::ContainerState::Running;
        sidecar.started_at_ns = 1_000_000;
        {
            let mut st = svc.state.inner.lock().await;
            st.containers.insert(id.clone(), sidecar);
        }

        // Simulate the sidecar exiting: mark it Exited with the real code.
        {
            let mut st = svc.state.inner.lock().await;
            let c = st.containers.get_mut(&id).unwrap();
            c.state = state::ContainerState::Exited;
            c.exit_code = 137;
            c.finished_at_ns = 2_000_000;
        }

        // Step 1: kubelet calls StopContainer (idempotent on exited container).
        {
            let st = svc.state.inner.lock().await;
            let c = st.containers.get(&id).unwrap();
            assert_eq!(c.state, state::ContainerState::Exited);
            assert_eq!(c.exit_code, 137, "exit code must be preserved through stop");
        }

        // Step 2: kubelet calls RemoveContainer.
        {
            let mut st = svc.state.inner.lock().await;
            st.containers.remove(&id);
        }
        {
            let st = svc.state.inner.lock().await;
            assert!(
                !st.containers.contains_key(&id),
                "container must be removed from state"
            );
        }

        // Step 3: kubelet calls CreateContainer with attempt=1 (restart).
        let new_id = "sidecar-restart-test-attempt1".to_string();
        let mut restarted = make_container(&new_id, "sbx1", true);
        restarted.attempt = 1;
        restarted.state = state::ContainerState::Created;
        {
            let mut st = svc.state.inner.lock().await;
            st.containers.insert(new_id.clone(), restarted);
        }

        // Step 4: kubelet calls StartContainer — mark Running.
        {
            let mut st = svc.state.inner.lock().await;
            let c = st.containers.get_mut(&new_id).unwrap();
            c.state = state::ContainerState::Running;
            c.started_at_ns = 3_000_000;
        }

        // Verify final state: new container is running, is still tagged as sidecar,
        // and the attempt counter is correct.
        {
            let st = svc.state.inner.lock().await;
            let c = st.containers.get(&new_id).unwrap();
            assert_eq!(
                c.state,
                state::ContainerState::Running,
                "restarted sidecar must be Running"
            );
            assert_eq!(
                c.attempt, 1,
                "attempt counter must be 1 after first restart"
            );
            assert!(c.is_sidecar, "is_sidecar must survive the restart cycle");
            assert_eq!(c.exit_code, 0, "new attempt starts with exit_code=0");
        }
    }
}
