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
    FilesystemUsage, GetEventsRequest, ImageSpec, LinuxPodSandboxStatus, ListContainerStatsRequest,
    ListContainerStatsResponse, ListContainersRequest, ListContainersResponse,
    ListMetricDescriptorsRequest, ListMetricDescriptorsResponse, ListPodSandboxMetricsRequest,
    ListPodSandboxMetricsResponse, ListPodSandboxRequest, ListPodSandboxResponse,
    ListPodSandboxStatsRequest, ListPodSandboxStatsResponse, MemoryUsage, Namespace,
    NamespaceOption, PodSandbox, PodSandboxMetadata, PodSandboxNetworkStatus, PodSandboxState,
    PodSandboxStatsRequest, PodSandboxStatsResponse, PodSandboxStatus as CriPodSandboxStatus,
    PodSandboxStatusRequest, PodSandboxStatusResponse, PortForwardRequest, PortForwardResponse,
    RemoveContainerRequest, RemoveContainerResponse, RemovePodSandboxRequest,
    RemovePodSandboxResponse, ReopenContainerLogRequest, ReopenContainerLogResponse,
    RuntimeCondition, RuntimeConfigRequest, RuntimeConfigResponse, RuntimeStatus,
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
use crate::state::{
    self, AppState, ContainerState as MyContainerState, CriContainer, CriMount, CriSandbox,
    SandboxState,
};
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
    #[allow(dead_code)]
    pid: i32,
    #[allow(dead_code)]
    started_at: String,
    #[serde(default)]
    exit_code: Option<i32>,
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as i64
}

// Linux CLK_TCK is 100 on virtually all architectures (jiffies).
const CLK_TCK: u64 = 100;

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

fn build_container_stats(c: &CriContainer) -> ContainerStats {
    let ts = now_ns();
    let pid = read_pelagos_container_state(&c.pelagos_name)
        .map(|s| s.pid)
        .unwrap_or(0);
    let (cpu_nanos, mem_bytes) = if pid > 0 {
        (read_proc_cpu_nanos(pid), read_proc_mem_bytes(pid))
    } else {
        (0, 0)
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
            working_set_bytes: Some(UInt64Value { value: mem_bytes }),
            available_bytes: None,
            usage_bytes: Some(UInt64Value { value: mem_bytes }),
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
}

impl RuntimeSvc {
    async fn bin(&self) -> String {
        self.state.inner.lock().await.pelagos_bin.clone()
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
            let id = uuid::Uuid::new_v4().simple().to_string()[..16].to_string();
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

        let id = uuid::Uuid::new_v4().simple().to_string();
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
                args.push(format!("{}{}:ro", m.host_path, m.container_path));
            } else {
                args.push(format!("{}:{}", m.host_path, m.container_path));
            }
        }

        args.push(container.image.clone());

        // pelagos run treats all positional args after the image as the full
        // command (replacing both ENTRYPOINT and CMD). Combine CRI command
        // (entrypoint override) + args (cmd override) in order.
        args.extend(container.entrypoint.iter().cloned());
        args.extend(container.args.iter().cloned());

        let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let out = run_pelagos(&bin, &args_ref)
            .await
            .map_err(|e| Status::internal(format!("exec error: {}", e)))?;

        if !out.success {
            return Err(Status::internal(format!(
                "pelagos run failed: {}",
                out.stderr
            )));
        }

        let mut st = self.state.inner.lock().await;
        if let Some(c) = st.containers.get_mut(&container_id) {
            c.state = MyContainerState::Running;
            c.started_at_ns = now_ns();
            let _ = state::save_container(c);
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
            message: String::new(),
            labels: container.labels.clone(),
            annotations: container.annotations.clone(),
            mounts: vec![],
            log_path: String::new(),
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

    async fn exec(&self, _request: Request<ExecRequest>) -> Result<Response<ExecResponse>, Status> {
        Err(Status::unimplemented("not implemented"))
    }

    async fn attach(
        &self,
        _request: Request<AttachRequest>,
    ) -> Result<Response<AttachResponse>, Status> {
        Err(Status::unimplemented("not implemented"))
    }

    async fn port_forward(
        &self,
        _request: Request<PortForwardRequest>,
    ) -> Result<Response<PortForwardResponse>, Status> {
        Err(Status::unimplemented("not implemented"))
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
        _request: Request<PodSandboxStatsRequest>,
    ) -> Result<Response<PodSandboxStatsResponse>, Status> {
        Err(Status::unimplemented("not implemented"))
    }

    async fn list_pod_sandbox_stats(
        &self,
        _request: Request<ListPodSandboxStatsRequest>,
    ) -> Result<Response<ListPodSandboxStatsResponse>, Status> {
        Err(Status::unimplemented("not implemented"))
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
