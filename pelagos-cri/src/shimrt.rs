//! Shim-backed runtime handlers (epic #574, #599).
//!
//! [`ShimRuntime`] owns everything the CRI needs to run pods through a
//! containerd shim v2 binary (Kata, runsc, runwasi): the events sink and exit
//! tracker, one shim per pod sandbox, and per-container task state (bundle,
//! rootfs plan, stdio pump). It is deliberately independent of CRI protobufs
//! and of root: `runtime.rs` prepares the pod network (`podnet`) and calls in.
//!
//! Container-to-shim strategy: a container first tries the **sandbox's shim
//! address** (how a pod's containers share one VM). If that shim rejects the
//! container's `Create`, we fall back to running the shim `start` bootstrap
//! for the container and using the address it returns. Kata validation
//! (epic item) decides which is canonical; both are tested.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use containerd_shim_protos::api;

use crate::shim::bootstrap::{self, StartParams};
use crate::shim::bundle::Bundle;
use crate::shim::client::ShimClient;
use crate::shim::events::{placeholder_publish_binary, EventsSink};
use crate::shim::exec::{self, ExecLimits, ExecOutcome, ExecRequest, ExecSession};
use crate::shim::exit::{ContainerExit, ExitTracker};
use crate::shim::rootfs::{self, LayeredUsers, RootfsPlan, StoreLayout};
use crate::shim::spec::{
    self, ContainerSpecInput, ImageDefaults, SandboxResources, SandboxSpecInput,
};
use crate::shim::stdio::{Chunk, CriLogWriter, Pump, Sink, StdinWriter, Stdio};
use crate::state::{CriContainer, CriMount, CriSandbox};

/// containerd namespace used for all pelagos-cri tasks.
pub const NAMESPACE: &str = "k8s.io";
const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(30);
/// Grace for an exit event to arrive after the Wait RPC failed.
const EXIT_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum ShimError {
    #[error("no shim configured for runtime handler {0:?}")]
    UnknownHandler(String),
    #[error("unknown sandbox {0}")]
    UnknownSandbox(String),
    #[error("unknown container {0}")]
    UnknownContainer(String),
    #[error("{0}")]
    Spec(String),
    #[error("rootfs: {0}")]
    Rootfs(#[from] rootfs::RootfsError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("shim bootstrap: {0}")]
    Bootstrap(#[from] bootstrap::BootstrapError),
    #[error("shim: {0}")]
    Rpc(#[from] containerd_shim_protos::ttrpc::Error),
    #[error("exec: {0}")]
    Exec(#[from] exec::ExecError),
}

/// Static configuration.
#[derive(Debug, Clone)]
pub struct ShimConfig {
    /// runtime handler name -> shim binary.
    pub handlers: HashMap<String, String>,
    /// Bundles, events socket and FIFOs live under here.
    pub root: PathBuf,
    pub store: StoreLayout,
    /// Image whose rootfs the pod sandbox ("pause") task runs from, resolved
    /// in the local image store (so it must already be pulled). VM runtimes
    /// need it: the guest agent execs `/pause` from the task's rootfs and
    /// fails with "the file /pause was not found" otherwise (#609). `None`
    /// sends no rootfs (shims whose sandbox needs none, and unit mocks).
    pub sandbox_image: Option<String>,
}

/// Default pause image for shim-backed pods (`--shim-sandbox-image`).
pub const DEFAULT_SANDBOX_IMAGE: &str = "registry.k8s.io/pause:3.10";

impl ShimConfig {
    pub fn system(handlers: HashMap<String, String>) -> Self {
        Self {
            handlers,
            root: "/run/pelagos-cri/shim".into(),
            store: StoreLayout::system(),
            sandbox_image: Some(DEFAULT_SANDBOX_IMAGE.into()),
        }
    }
}

struct SandboxShim {
    client: Arc<ShimClient>,
    binary: String,
    bundle: Bundle,
    /// Host-side overlay scratch for the pause rootfs; cleaned after Delete.
    plan: Option<RootfsPlan>,
}

impl ShimRuntime {
    /// Layers of the configured sandbox image, top-first.
    fn sandbox_layers(&self) -> Result<Option<Vec<PathBuf>>, ShimError> {
        let Some(image) = self.cfg.sandbox_image.as_deref() else {
            return Ok(None);
        };
        let store = &self.cfg.store;
        let manifest = rootfs::find_manifest(store, image)?;
        Ok(Some(rootfs::layers_top_first(store, image, &manifest)?))
    }
}

struct TaskCtx {
    sandbox_id: String,
    client: Arc<ShimClient>,
    bundle: Bundle,
    plan: RootfsPlan,
    stdio: Stdio,
    pump: tokio::sync::Mutex<Option<Pump>>,
    stdin: Option<StdinWriter>,
}

/// What re-adoption found for a container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readopted {
    /// Task alive; log pump, stdin and exit watch re-attached.
    Running,
    /// Task finished while we were down (exit recorded).
    Exited(u32),
    /// The shim or task is gone; recorded as exit 137.
    Lost,
}

/// A started streaming exec (CRI `Exec`).
pub struct ExecStream {
    pub session: ExecSession,
    /// Live output; subscribed before the process started.
    pub output: tokio::sync::broadcast::Receiver<Chunk>,
    pub client: Arc<ShimClient>,
}

/// A streaming attach (CRI `Attach`) to a running container's stdio.
pub struct AttachStream {
    pub output: tokio::sync::broadcast::Receiver<Chunk>,
    pub stdin: Option<StdinWriter>,
}

#[derive(Debug, Default)]
pub struct ReadoptReport {
    /// Sandboxes whose shim could not be reached.
    pub sandboxes_lost: Vec<String>,
    pub containers: Vec<(String, Readopted)>,
}

pub struct ShimRuntime {
    cfg: ShimConfig,
    events_addr: String,
    pub exits: ExitTracker,
    sink: tokio::sync::Mutex<EventsSink>,
    sandboxes: Mutex<HashMap<String, Arc<SandboxShim>>>,
    tasks: Mutex<HashMap<String, Arc<TaskCtx>>>,
    /// Serialises pause-image pulls so concurrent pods pull it once.
    pull_lock: tokio::sync::Mutex<()>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl ShimRuntime {
    /// Start the events sink and exit tracker.
    pub async fn start(cfg: ShimConfig) -> std::io::Result<Self> {
        std::fs::create_dir_all(&cfg.root)?;
        let (sink, rx) = EventsSink::start(&cfg.root.join("events.sock")).await?;
        Ok(Self {
            events_addr: sink.address(),
            exits: ExitTracker::spawn(rx),
            sink: tokio::sync::Mutex::new(sink),
            cfg,
            sandboxes: Mutex::new(HashMap::new()),
            tasks: Mutex::new(HashMap::new()),
            pull_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// The configured sandbox image if it is absent from the local store.
    pub fn missing_sandbox_image(&self) -> Option<String> {
        let image = self.cfg.sandbox_image.as_deref()?;
        match rootfs::find_manifest(&self.cfg.store, image) {
            Err(rootfs::RootfsError::ImageNotFound(_)) => Some(image.to_string()),
            _ => None,
        }
    }

    /// Make sure the sandbox (pause) image is in the local store, pulling it
    /// with `<bin> image pull` when it is not (#619). VM runtimes cannot start
    /// a pod without it, and nothing else pulls it: the kubelet only pulls
    /// workload images. Concurrent callers pull once.
    pub async fn ensure_sandbox_image(&self, bin: &str) -> Result<(), String> {
        if self.missing_sandbox_image().is_none() {
            return Ok(());
        }
        let _guard = self.pull_lock.lock().await;
        let Some(image) = self.missing_sandbox_image() else {
            return Ok(());
        };
        log::info!("pulling sandbox image {image}");
        match crate::invoke::run_pelagos(bin, &["image", "pull", &image]).await {
            Ok(o) if o.success => Ok(()),
            Ok(o) => Err(format!("pull sandbox image {image}: {}", o.stderr.trim())),
            Err(e) => Err(format!("pull sandbox image {image}: {e}")),
        }
    }

    pub async fn shutdown(&self) {
        self.sink.lock().await.shutdown().await;
    }

    pub fn handles(&self, handler: &str) -> bool {
        self.cfg.handlers.contains_key(handler)
    }

    pub fn handler_names(&self) -> Vec<String> {
        let mut v: Vec<_> = self.cfg.handlers.keys().cloned().collect();
        v.sort();
        v
    }

    fn start_params(&self, binary: &str, id: &str, bundle: &Bundle) -> StartParams {
        StartParams {
            binary: binary.to_string(),
            namespace: NAMESPACE.into(),
            id: id.into(),
            address: self
                .cfg
                .root
                .join("containerd.sock")
                .to_string_lossy()
                .into(),
            ttrpc_address: self.events_addr.clone(),
            publish_binary: placeholder_publish_binary(),
            bundle: bundle.dir().to_path_buf(),
        }
    }

    fn sandbox(&self, id: &str) -> Result<Arc<SandboxShim>, ShimError> {
        lock(&self.sandboxes)
            .get(id)
            .cloned()
            .ok_or_else(|| ShimError::UnknownSandbox(id.into()))
    }

    fn task(&self, id: &str) -> Result<Arc<TaskCtx>, ShimError> {
        lock(&self.tasks)
            .get(id)
            .cloned()
            .ok_or_else(|| ShimError::UnknownContainer(id.into()))
    }

    // ── sandbox ──────────────────────────────────────────────────────────

    /// Bootstrap the pod's shim and run the sandbox (pause) task in it. The
    /// pod netns (CNI applied) must already exist.
    pub async fn start_sandbox(
        &self,
        handler: &str,
        sb: &CriSandbox,
        netns_path: &str,
        hostname: Option<&str>,
        resources: Option<SandboxResources>,
    ) -> Result<(), ShimError> {
        let binary = self
            .cfg
            .handlers
            .get(handler)
            .ok_or_else(|| ShimError::UnknownHandler(handler.into()))?
            .clone();
        let sbspec = spec::build_sandbox_spec(&SandboxSpecInput {
            sandbox: sb,
            netns_path: Some(netns_path),
            resources,
            hostname,
            pause_args: &[],
        })
        .map_err(|e| ShimError::Spec(e.to_string()))?;
        let plan = match self.sandbox_layers()? {
            Some(layers) => Some(rootfs::plan_rootfs(&self.cfg.store, &sb.id, layers)?),
            None => None,
        };
        let cleanup_plan = |plan: &Option<RootfsPlan>| {
            if let Some(p) = plan {
                let _ = p.cleanup();
            }
        };
        let bundle = Bundle::create(&self.cfg.root.join("bundles"), NAMESPACE, &sb.id)
            .inspect_err(|_| cleanup_plan(&plan))?;
        let prepared = async {
            bundle.write_config(
                &spec::to_config_json(&sbspec).map_err(|e| ShimError::Spec(e.to_string()))?,
            )?;
            let params = self.start_params(&binary, &sb.id, &bundle);
            let addr = bootstrap::start(&params, BOOTSTRAP_TIMEOUT).await?;
            bundle.persist_shim(&binary, &addr)?;
            Ok::<_, ShimError>(Arc::new(ShimClient::connect(&addr.address).await?))
        }
        .await;
        let client = match prepared {
            Ok(c) => c,
            Err(e) => {
                cleanup_plan(&plan);
                let _ = bundle.remove();
                return Err(e);
            }
        };

        let run = async {
            let mut req = api::CreateTaskRequest::new();
            req.id = sb.id.clone();
            req.bundle = bundle.dir().to_string_lossy().into_owned();
            if let Some(p) = &plan {
                req.rootfs = vec![p.to_api_mount()];
            }
            client.create(req).await?;
            client.start(&sb.id).await?;
            Ok::<_, ShimError>(())
        }
        .await;
        if let Err(e) = run {
            // Do not leave a half-started VM behind.
            let _ = client.shutdown(&sb.id).await;
            let _ = client.delete(&sb.id).await;
            cleanup_plan(&plan);
            let _ = bundle.remove();
            return Err(e);
        }
        lock(&self.sandboxes).insert(
            sb.id.clone(),
            Arc::new(SandboxShim {
                client,
                binary,
                bundle,
                plan,
            }),
        );
        Ok(())
    }

    /// Stop the sandbox task and shim. Best effort past the first error: the
    /// caller goes on to CNI DEL / netns removal, which must happen after the
    /// VM is gone (`podnet::teardown_after_shim_stop`).
    pub async fn stop_sandbox(&self, id: &str) {
        let Ok(sb) = self.sandbox(id) else { return };
        if let Err(e) = sb.client.kill(id, 9, true).await {
            log::debug!("stop_sandbox {id}: kill: {e}");
        }
        let _ = tokio::time::timeout(
            Duration::from_secs(10),
            self.exits.wait_exit_or_poll(id, &sb.client, EXIT_GRACE),
        )
        .await;
        if let Err(e) = sb.client.delete(id).await {
            log::debug!("stop_sandbox {id}: delete: {e}");
        }
        if let Err(e) = sb.client.shutdown(id).await {
            log::debug!("stop_sandbox {id}: shutdown: {e}");
        }
    }

    /// Forget a stopped sandbox and clean up after it. Sends the shim a second
    /// `Shutdown`: the kubelet stops the pod sandbox first and only garbage-
    /// collects its containers afterwards, and gVisor's shim ignores `Shutdown`
    /// while container tasks still exist, so the one sent by `stop_sandbox` can
    /// be a no-op. By the time the kubelet removes the sandbox the tasks are
    /// gone and this one lets the shim exit (#625).
    pub async fn remove_sandbox(&self, id: &str) {
        let removed = lock(&self.sandboxes).remove(id);
        if let Some(sb) = removed {
            let _ = tokio::time::timeout(Duration::from_secs(5), sb.client.shutdown(id)).await;
            if let Some(p) = &sb.plan {
                if let Err(e) = p.cleanup() {
                    log::warn!("remove_sandbox {id}: rootfs cleanup: {e}");
                }
            }
            let _ = sb.bundle.remove();
        }
        self.exits.forget(id);
    }

    // ── re-adoption ─────────────────────────────────────────────────────

    /// Rebuild runtime state for pods created before a CRI restart. `containers`
    /// pairs each CRI container (of a shim sandbox) with its CRI log path.
    /// Unreachable shims/tasks are *reported*, never silently dropped, and
    /// their bundles are left on disk (a dead shim may leave mounts; deleting
    /// the tree blindly could reach through them).
    pub async fn readopt(
        &self,
        sandboxes: &[CriSandbox],
        containers: &[(CriContainer, String)],
    ) -> ReadoptReport {
        let mut report = ReadoptReport::default();
        let bundles = self.cfg.root.join("bundles");
        for sb in sandboxes
            .iter()
            .filter(|s| self.handles(&s.runtime_handler))
        {
            match self.reconnect_sandbox(&bundles, sb).await {
                Ok(()) => {}
                Err(e) => {
                    log::warn!("readopt: sandbox {} unreachable: {e}", sb.id);
                    report.sandboxes_lost.push(sb.id.clone());
                }
            }
        }
        for (c, log_path) in containers {
            let outcome = if report.sandboxes_lost.contains(&c.sandbox_id) {
                None
            } else {
                self.readopt_container(&bundles, c, log_path).await
            };
            let outcome = outcome.unwrap_or_else(|| {
                self.exits
                    .apply(crate::shim::exit::TaskEvent::Exit(ContainerExit {
                        container_id: c.id.clone(),
                        exec_id: None,
                        pid: 0,
                        exit_status: 137,
                        exited_at: None,
                    }));
                Readopted::Lost
            });
            report.containers.push((c.id.clone(), outcome));
        }
        report
    }

    async fn reconnect_sandbox(
        &self,
        bundles: &std::path::Path,
        sb: &CriSandbox,
    ) -> Result<(), ShimError> {
        let bundle = Bundle::open(bundles, NAMESPACE, &sb.id)?;
        let rec = bundle.load_shim()?;
        let client = Arc::new(ShimClient::connect(&rec.address).await?);
        client.connect_task(&sb.id).await?;
        let plan = self
            .sandbox_layers()
            .ok()
            .flatten()
            .map(|layers| rootfs::reattach_rootfs(&self.cfg.store, &sb.id, layers));
        lock(&self.sandboxes).insert(
            sb.id.clone(),
            Arc::new(SandboxShim {
                client,
                binary: rec.binary,
                bundle,
                plan,
            }),
        );
        Ok(())
    }

    /// `None` = lost.
    async fn readopt_container(
        &self,
        bundles: &std::path::Path,
        c: &CriContainer,
        cri_log_path: &str,
    ) -> Option<Readopted> {
        let sandbox = self.sandbox(&c.sandbox_id).ok()?;
        let bundle = Bundle::open(bundles, NAMESPACE, &c.id).ok()?;
        // A container that needed the per-container fallback has its own shim.
        let client = match bundle.load_shim() {
            Ok(rec) => Arc::new(ShimClient::connect(&rec.address).await.ok()?),
            Err(_) => sandbox.client.clone(),
        };
        let st = match client.state(&c.id).await {
            Ok(st) => st,
            Err(e) => {
                log::warn!("readopt: container {}: {e}", c.id);
                return None;
            }
        };
        let store = &self.cfg.store;
        let layers = rootfs::find_manifest(store, &c.image)
            .ok()
            .and_then(|m| rootfs::layers_top_first(store, &c.image, &m).ok())
            .unwrap_or_default();
        let plan = rootfs::reattach_rootfs(store, &c.id, layers);
        let opt = |s: &str| (!s.is_empty()).then(|| PathBuf::from(s));
        let stdio = Stdio {
            stdin: opt(&st.stdin),
            stdout: PathBuf::from(&st.stdout),
            stderr: opt(&st.stderr),
            terminal: st.terminal,
        };
        use containerd_shim_protos::api::Status as S;
        let stopped = matches!(st.status.enum_value(), Ok(S::STOPPED));
        let (pump, stdin) = if stopped {
            (None, None)
        } else {
            let w = match CriLogWriter::open(cri_log_path.as_ref()) {
                Ok(w) => w,
                Err(e) => {
                    log::warn!("readopt: container {}: log {cri_log_path}: {e}", c.id);
                    return None;
                }
            };
            let pump = Pump::spawn(&stdio, Sink::Log(w)).ok()?;
            let stdin = stdio
                .stdin
                .as_deref()
                .and_then(|p| StdinWriter::open(p).ok());
            (Some(pump), stdin)
        };
        let ctx = Arc::new(TaskCtx {
            sandbox_id: c.sandbox_id.clone(),
            client,
            bundle,
            plan,
            stdio,
            pump: tokio::sync::Mutex::new(pump),
            stdin,
        });
        lock(&self.tasks).insert(c.id.clone(), ctx.clone());
        if stopped {
            self.exits
                .apply(crate::shim::exit::TaskEvent::Exit(ContainerExit {
                    container_id: c.id.clone(),
                    exec_id: None,
                    pid: st.pid,
                    exit_status: st.exit_status,
                    exited_at: st.exited_at.as_ref().map(|t| (t.seconds, t.nanos)),
                }));
            Some(Readopted::Exited(st.exit_status))
        } else {
            self.spawn_exit_watch(&c.id, ctx);
            Some(Readopted::Running)
        }
    }

    // ── containers ───────────────────────────────────────────────────────

    /// Create and start a container's task in its pod's VM. Returns the pid.
    pub async fn start_container(
        &self,
        sb: &CriSandbox,
        c: &CriContainer,
        cri_log_path: &str,
        netns_path: &str,
    ) -> Result<u32, ShimError> {
        let sandbox = self.sandbox(&sb.id)?;
        let store = &self.cfg.store;
        let manifest = rootfs::find_manifest(store, &c.image)?;
        let image = ImageDefaults::from_manifest(&manifest);
        let layers = rootfs::layers_top_first(store, &c.image, &manifest)?;
        let plan = rootfs::plan_rootfs(store, &c.id, layers.clone())?;
        let users = LayeredUsers::new(layers);

        let bundle_root = self.cfg.root.join("bundles");
        let bundle = Bundle::create(&bundle_root, NAMESPACE, &c.id)?;
        let built = self
            .prepare_container(sb, c, &image, &users, netns_path, &sandbox, &bundle)
            .await;
        let stdio = match built {
            Ok(s) => s,
            Err(e) => {
                let _ = plan.cleanup();
                let _ = bundle.remove();
                return Err(e);
            }
        };

        let result = self
            .create_and_start(sb, c, &plan, &bundle, &stdio, cri_log_path, &sandbox)
            .await;
        match result {
            Ok((pid, ctx)) => {
                self.spawn_exit_watch(&c.id, ctx.clone());
                lock(&self.tasks).insert(c.id.clone(), ctx);
                Ok(pid)
            }
            Err(e) => {
                stdio.remove();
                let _ = plan.cleanup();
                let _ = bundle.remove();
                Err(e)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn prepare_container(
        &self,
        sb: &CriSandbox,
        c: &CriContainer,
        image: &ImageDefaults,
        users: &LayeredUsers,
        netns_path: &str,
        sandbox: &SandboxShim,
        bundle: &Bundle,
    ) -> Result<Stdio, ShimError> {
        let devices = c
            .devices
            .iter()
            .map(spec::resolve_device)
            .collect::<Result<Vec<_>, _>>()?;
        let seccomp = if c.seccomp_profile_type == 2 && !c.privileged {
            Some(spec::load_seccomp_profile(&c.seccomp_profile_path)?)
        } else {
            None
        };
        // Pod DNS: the guest has no host resolver config, so hand it the
        // sandbox's as a bind mount (the shim shares it into the VM).
        let mut extra_mounts: Vec<CriMount> = Vec::new();
        if !sb.dns_servers.is_empty() {
            let p = sandbox.bundle.dir().join("resolv.conf");
            std::fs::write(&p, resolv_conf(sb))?;
            extra_mounts.push(CriMount {
                host_path: p.to_string_lossy().into_owned(),
                container_path: "/etc/resolv.conf".into(),
                readonly: true,
                recursive_read_only: false,
                propagation: 0,
            });
        }
        let cspec = spec::build_container_spec(&ContainerSpecInput {
            container: c,
            sandbox: sb,
            image,
            netns_path: Some(netns_path),
            extra_mounts: &extra_mounts,
            devices: &devices,
            seccomp,
            hostname: None,
            users: Some(users),
        })
        .map_err(|e| ShimError::Spec(e.to_string()))?;
        bundle.write_config(
            &spec::to_config_json(&cspec).map_err(|e| ShimError::Spec(e.to_string()))?,
        )?;
        Ok(Stdio::create(
            &self.cfg.root.join("io"),
            &c.id,
            c.stdin,
            c.tty,
        )?)
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_and_start(
        &self,
        sb: &CriSandbox,
        c: &CriContainer,
        plan: &RootfsPlan,
        bundle: &Bundle,
        stdio: &Stdio,
        cri_log_path: &str,
        sandbox: &SandboxShim,
    ) -> Result<(u32, Arc<TaskCtx>), ShimError> {
        let pump = Pump::spawn(stdio, Sink::Log(CriLogWriter::open(cri_log_path.as_ref())?))?;
        let stdin = match &stdio.stdin {
            Some(p) => StdinWriter::open(p).ok(),
            None => None,
        };
        let make_req = || {
            let mut r = api::CreateTaskRequest::new();
            r.id = c.id.clone();
            r.bundle = bundle.dir().to_string_lossy().into_owned();
            r.rootfs = vec![plan.to_api_mount()];
            stdio.fill_create(&mut r);
            r
        };

        // Preferred: the pod's own shim.
        let mut client = sandbox.client.clone();
        if let Err(e) = client.create(make_req()).await {
            // Fallback: ask the shim binary to bootstrap this container.
            log::warn!(
                "container {}: sandbox shim rejected Create ({e}); falling back to shim start",
                c.id
            );
            let params = self.start_params(&sandbox.binary, &c.id, bundle);
            // Once `start` has launched a dedicated shim, every later failure
            // must stop it again: it lives in its own systemd scope, so nothing
            // else would (#624).
            let launched = std::sync::atomic::AtomicBool::new(false);
            let started = async {
                let addr = bootstrap::start(&params, BOOTSTRAP_TIMEOUT).await?;
                launched.store(true, std::sync::atomic::Ordering::Relaxed);
                bundle.persist_shim(&sandbox.binary, &addr)?;
                let cl = Arc::new(ShimClient::connect(&addr.address).await?);
                if let Err(e) = cl.create(make_req()).await {
                    let _ = cl.shutdown(&c.id).await;
                    return Err(e.into());
                }
                Ok::<_, ShimError>(cl)
            }
            .await;
            match started {
                Ok(cl) => client = cl,
                Err(e2) => {
                    if launched.load(std::sync::atomic::Ordering::Relaxed) {
                        // Covers a shim we could not even connect to.
                        let _ = bootstrap::delete(&params, BOOTSTRAP_TIMEOUT).await;
                    }
                    pump.finish().await;
                    return Err(e2);
                }
            }
        }
        let dedicated = !Arc::ptr_eq(&client, &sandbox.client);
        let pid = match client.start(&c.id).await {
            Ok(r) => r.pid,
            Err(e) => {
                let _ = client.delete(&c.id).await;
                if dedicated {
                    let _ = client.shutdown(&c.id).await;
                }
                pump.finish().await;
                return Err(e.into());
            }
        };
        let ctx = Arc::new(TaskCtx {
            sandbox_id: sb.id.clone(),
            client,
            bundle: bundle.clone(),
            plan: plan.clone(),
            stdio: stdio.clone(),
            pump: tokio::sync::Mutex::new(Some(pump)),
            stdin,
        });
        Ok((pid, ctx))
    }

    /// Feed the exit tracker from the Wait RPC as well as events, so a shim
    /// that never publishes still reports exits.
    /// When the task exits the log pump is drained and closed right away, so
    /// the CRI log is complete by the time the exit is visible.
    fn spawn_exit_watch(&self, id: &str, ctx: Arc<TaskCtx>) {
        let exits = self.exits.clone();
        let id = id.to_string();
        tokio::spawn(async move {
            match exits.wait_exit_or_poll(&id, &ctx.client, EXIT_GRACE).await {
                Ok(_) => finish_task_io(&ctx).await,
                Err(e) => log::warn!("container {id}: {e}"),
            }
        });
    }

    /// Drain and close a container's log pump (idempotent).
    pub async fn flush_logs(&self, id: &str) {
        if let Ok(t) = self.task(id) {
            finish_task_io(&t).await;
        }
    }

    pub fn container_exit(&self, id: &str) -> Option<ContainerExit> {
        self.exits.exit_of(id)
    }

    #[cfg(test)]
    pub fn is_running(&self, id: &str) -> bool {
        lock(&self.tasks).contains_key(id) && self.exits.exit_of(id).is_none()
    }

    /// Stop a container: `signal`, wait up to `timeout`, then SIGKILL. Returns
    /// the exit, or `None` if the shim was lost.
    pub async fn stop_container(
        &self,
        id: &str,
        signal: u32,
        timeout: Duration,
    ) -> Option<ContainerExit> {
        let t = self.task(id).ok()?;
        if self.exits.exit_of(id).is_none() {
            if let Err(e) = t.client.kill(id, signal, false).await {
                log::debug!("stop_container {id}: kill {signal}: {e}");
            }
            let waited = tokio::time::timeout(
                timeout,
                self.exits.wait_exit_or_poll(id, &t.client, EXIT_GRACE),
            )
            .await;
            if waited.is_err() {
                log::info!("container {id}: SIGKILL after {timeout:?}");
                let _ = t.client.kill(id, 9, true).await;
                let _ = tokio::time::timeout(
                    Duration::from_secs(10),
                    self.exits.wait_exit_or_poll(id, &t.client, EXIT_GRACE),
                )
                .await;
            }
        }
        finish_task_io(&t).await;
        self.exits.exit_of(id)
    }

    /// Delete the task and clean up host-side state. Idempotent.
    pub async fn remove_container(&self, id: &str) {
        let Some(t) = lock(&self.tasks).remove(id) else {
            return;
        };
        finish_task_io(&t).await;
        if let Err(e) = t.client.delete(id).await {
            log::debug!("remove_container {id}: delete: {e}");
        }
        // A container that needed the per-container fallback owns its shim
        // (it has its own shim.json); stop it with the task (#624).
        if t.bundle.load_shim().is_ok() {
            let _ = t.client.shutdown(id).await;
        }
        t.stdio.remove();
        if let Err(e) = t.plan.cleanup() {
            log::warn!("remove_container {id}: rootfs cleanup: {e}");
        }
        let _ = t.bundle.remove();
        self.exits.forget(id);
    }

    /// Current CPU/memory of a running container, from the shim's `Stats`.
    /// `None` when the container is unknown, exited, or the shim cannot answer
    /// (stats must never fail a CRI list call).
    pub async fn usage(&self, id: &str) -> Option<crate::shim::stats::TaskUsage> {
        let t = self.task(id).ok()?;
        if self.exits.exit_of(id).is_some() {
            return None;
        }
        let resp = tokio::time::timeout(Duration::from_secs(5), t.client.stats(id))
            .await
            .ok()?
            .map_err(|e| log::debug!("stats {id}: {e}"))
            .ok()?;
        crate::shim::stats::decode_usage(resp.stats.as_ref()?)
            .map_err(|e| log::warn!("stats {id}: {e}"))
            .ok()
    }

    /// Containers of a sandbox this runtime currently tracks.
    pub fn container_ids_of(&self, sandbox_id: &str) -> Vec<String> {
        lock(&self.tasks)
            .iter()
            .filter(|(_, t)| t.sandbox_id == sandbox_id)
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Start a streaming exec in a container.
    pub async fn start_exec(
        &self,
        container_id: &str,
        req: ExecRequest,
    ) -> Result<ExecStream, ShimError> {
        let t = self.task(container_id)?;
        let base = exec::base_process(t.bundle.dir())?;
        let exec_id = format!("exec-{}", uuid_like());
        let mut output = None;
        let session = ExecSession::start_with(
            &t.client,
            container_id,
            &exec_id,
            &base,
            &req,
            &self.cfg.root.join("io"),
            |p| output = Some(p.subscribe()),
        )
        .await?;
        Ok(ExecStream {
            session,
            output: output.expect("before_start ran"),
            client: t.client.clone(),
        })
    }

    /// Attach to a running container: follow its live output and write to its
    /// stdin. Output before the attach point is not replayed.
    pub async fn attach(&self, container_id: &str) -> Result<AttachStream, ShimError> {
        let t = self.task(container_id)?;
        let guard = t.pump.lock().await;
        let pump = guard
            .as_ref()
            .ok_or_else(|| ShimError::UnknownContainer(container_id.into()))?;
        Ok(AttachStream {
            output: pump.subscribe(),
            stdin: t.stdin.clone(),
        })
    }

    /// Reopen a container's CRI log after rotation (`ReopenContainerLog`).
    pub async fn reopen_log(&self, container_id: &str) {
        if let Ok(t) = self.task(container_id) {
            if let Some(p) = t.pump.lock().await.as_ref() {
                if let Err(e) = p.reopen_log() {
                    log::warn!("reopen log {container_id}: {e}");
                }
            }
        }
    }

    pub async fn exec_sync(
        &self,
        container_id: &str,
        args: Vec<String>,
        timeout: Option<Duration>,
    ) -> Result<ExecOutcome, ShimError> {
        let t = self.task(container_id)?;
        let base = exec::base_process(t.bundle.dir())?;
        let exec_id = format!("exec-{}", uuid_like());
        Ok(exec::exec_sync(
            &t.client,
            container_id,
            &exec_id,
            &base,
            &ExecRequest {
                args,
                ..Default::default()
            },
            &self.cfg.root.join("io"),
            ExecLimits {
                timeout,
                ..Default::default()
            },
        )
        .await?)
    }
}

async fn finish_task_io(t: &TaskCtx) {
    if let Some(w) = &t.stdin {
        w.close().await;
    }
    if let Some(p) = t.pump.lock().await.take() {
        p.finish().await;
    }
}

fn uuid_like() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    format!(
        "{:x}-{:x}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

fn resolv_conf(sb: &CriSandbox) -> String {
    let mut s = String::new();
    for n in &sb.dns_servers {
        s.push_str(&format!("nameserver {n}\n"));
    }
    if !sb.dns_searches.is_empty() {
        s.push_str(&format!("search {}\n", sb.dns_searches.join(" ")));
    }
    if !sb.dns_options.is_empty() {
        s.push_str(&format!("options {}\n", sb.dns_options.join(" ")));
    }
    s
}

#[cfg(test)]
pub(crate) mod tests;
