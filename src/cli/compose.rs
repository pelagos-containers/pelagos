//! `pelagos compose` — multi-service orchestration with Lisp compose files (`.reml`).

use super::{
    check_liveness, container_dir, containers_dir, now_iso8601, write_state, ContainerState,
    ContainerStatus, HealthStatus, SpawnConfig,
};
use pelagos::compose::{
    parse_compose, topo_sort, ComposeFile, Dependency, HealthCheck, ServiceSpec,
};
use pelagos::container::{Command, Namespace, Stdio, Volume};
use pelagos::lisp::{HookMap, Interpreter};
use pelagos::network::NetworkMode;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

// Set by the SIGTERM/SIGINT handler in the supervisor process to trigger a
// graceful shutdown.  Atomic because signal handlers run on any thread.
static COMPOSE_SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn compose_signal_handler(_: libc::c_int) {
    COMPOSE_SHUTDOWN.store(true, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// CLI args (clap)
// ---------------------------------------------------------------------------

#[derive(Debug, clap::Subcommand)]
pub enum ComposeCmd {
    /// Start all services defined in the compose file
    Up {
        /// Path to compose file (default: compose.reml)
        #[clap(long = "file", short = 'f', default_value = "compose.reml")]
        file: PathBuf,
        /// Project name (default: parent directory name)
        #[clap(long = "project", short = 'p')]
        project: Option<String>,
        /// Run in foreground (don't daemonise)
        #[clap(long)]
        foreground: bool,
        /// Never pull images; fail immediately if an image is not cached locally
        #[clap(long)]
        no_pull: bool,
        /// Return as soon as the supervisor is forked instead of waiting for every service
        /// to start (the old behaviour: a startup failure is then only visible in
        /// `compose ps` and the supervisor log)
        #[clap(long)]
        no_wait: bool,
        /// Seconds to wait for all services to start before giving up waiting (the
        /// supervisor keeps going in the background). Dependency readiness polling alone
        /// can take 60 s per service.
        #[clap(long, value_name = "SECS", default_value = "300")]
        startup_timeout: u64,
    },
    /// Stop and remove all services
    Down {
        /// Path to compose file (default: compose.reml)
        #[clap(long = "file", short = 'f', default_value = "compose.reml")]
        file: PathBuf,
        /// Project name
        #[clap(long = "project", short = 'p')]
        project: Option<String>,
        /// Also remove volumes
        #[clap(long = "volumes", short = 'v')]
        volumes: bool,
    },
    /// List services in the compose project
    Ps {
        /// Path to compose file (default: compose.reml)
        #[clap(long = "file", short = 'f', default_value = "compose.reml")]
        file: PathBuf,
        /// Project name
        #[clap(long = "project", short = 'p')]
        project: Option<String>,
    },
    /// View logs for compose services
    Logs {
        /// Path to compose file (default: compose.reml)
        #[clap(long = "file", short = 'f', default_value = "compose.reml")]
        file: PathBuf,
        /// Project name
        #[clap(long = "project", short = 'p')]
        project: Option<String>,
        /// Follow log output
        #[clap(long)]
        follow: bool,
        /// Service name (show all if omitted)
        service: Option<String>,
    },
}

// ---------------------------------------------------------------------------
// Project state (persisted as JSON)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposeProject {
    pub name: String,
    pub file_path: String,
    pub services: HashMap<String, ComposeServiceState>,
    pub networks: Vec<String>,
    pub volumes: Vec<String>,
    pub supervisor_pid: i32,
    pub started_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposeServiceState {
    pub container_name: String,
    pub status: String,
    pub pid: i32,
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

pub fn cmd_compose(cmd: ComposeCmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        ComposeCmd::Up {
            file,
            project,
            foreground,
            no_pull,
            no_wait,
            startup_timeout,
        } => cmd_compose_up(
            &file,
            project.as_deref(),
            UpOptions {
                foreground,
                no_pull,
                no_wait,
                startup_timeout: Duration::from_secs(startup_timeout),
            },
        ),
        ComposeCmd::Down {
            file,
            project,
            volumes,
        } => cmd_compose_down(&file, project.as_deref(), volumes),
        ComposeCmd::Ps { file, project } => cmd_compose_ps(&file, project.as_deref()),
        ComposeCmd::Logs {
            file,
            project,
            follow,
            service,
        } => cmd_compose_logs(&file, project.as_deref(), follow, service.as_deref()),
    }
}

// ---------------------------------------------------------------------------
// compose up
// ---------------------------------------------------------------------------

/// How `compose up` behaves, besides which file and project.
#[derive(Clone, Copy)]
struct UpOptions {
    foreground: bool,
    no_pull: bool,
    /// Do not wait for the supervisor to report that every service started.
    no_wait: bool,
    startup_timeout: Duration,
}

fn cmd_compose_up(
    file: &std::path::Path,
    project_name: Option<&str>,
    opts: UpOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    cmd_compose_up_reml(file, project_name, opts)
}

// ---------------------------------------------------------------------------
// Startup handshake (#653)
//
// In background mode `compose up` forks a supervisor that starts the services. Before this,
// the parent slept 200 ms and printed "Project started" whatever happened next, so any
// failure after that (a service that cannot spawn, a missing runtime, a dependency that never
// becomes ready) was invisible: the supervisor rolled everything back and the user saw
// "started", exit 0, and later "no project found". Now the supervisor reports over a pipe,
// once, with a framed message: 1 status byte ('S' started / 'F' failed), a 4-byte
// little-endian length, then that many bytes of UTF-8 text (notes / the error). The parent
// waits for it, noticing a dead supervisor and a timeout.
// ---------------------------------------------------------------------------

/// The supervisor's side of the pipe. Sends at most one frame; later calls are no-ops.
pub struct StartupNotifier {
    fd: std::cell::Cell<i32>,
}

impl StartupNotifier {
    /// A notifier that reports to nobody (foreground mode, `--no-wait`).
    pub fn none() -> Self {
        Self {
            fd: std::cell::Cell::new(-1),
        }
    }
    fn new(fd: i32) -> Self {
        Self {
            fd: std::cell::Cell::new(fd),
        }
    }
    fn send(&self, status: u8, text: &str) {
        let fd = self.fd.replace(-1);
        if fd < 0 {
            return;
        }
        let frame = encode_startup_frame(status, text);
        let mut off = 0;
        while off < frame.len() {
            // SAFETY: writing from a live buffer to an fd we own.
            let n = unsafe {
                libc::write(
                    fd,
                    frame[off..].as_ptr() as *const libc::c_void,
                    frame.len() - off,
                )
            };
            if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            if n <= 0 {
                break; // the parent is gone; nothing more to tell it
            }
            off += n as usize;
        }
        unsafe { libc::close(fd) };
    }
    /// Every service started. `notes` carries warnings (e.g. a service already exited).
    pub fn started(&self, notes: &str) {
        self.send(b'S', notes);
    }
    /// Startup failed (after rollback); `error` is what the user should read.
    pub fn failed(&self, error: &str) {
        self.send(b'F', error);
    }
}

fn encode_startup_frame(status: u8, text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut f = Vec::with_capacity(5 + bytes.len());
    f.push(status);
    f.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    f.extend_from_slice(bytes);
    f
}

/// A complete frame at the start of `buf`: `(status, text)`. `None` while it is incomplete.
fn decode_startup_frame(buf: &[u8]) -> Option<(u8, String)> {
    if buf.len() < 5 {
        return None;
    }
    let len = u32::from_le_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
    let body = buf.get(5..5 + len)?;
    Some((buf[0], String::from_utf8_lossy(body).into_owned()))
}

#[derive(Debug, PartialEq, Eq)]
enum StartupWait {
    /// All services started; the string holds any warnings.
    Started(String),
    /// Startup failed and was rolled back; the string is the error.
    Failed(String),
    /// The supervisor exited (or closed the pipe) without reporting.
    SupervisorDied,
    /// Still starting when the timeout ran out.
    TimedOut,
}

/// Wait on `fd` (the read end) for the supervisor's frame. Closes `fd`. If `supervisor_pid`
/// is given it is reaped with WNOHANG as a second way to notice a supervisor that died.
fn wait_for_startup(fd: i32, timeout: Duration, supervisor_pid: Option<i32>) -> StartupWait {
    let deadline = Instant::now() + timeout;
    let mut buf: Vec<u8> = Vec::new();
    let done = |fd: i32, r: StartupWait| {
        unsafe { libc::close(fd) };
        r
    };
    // Non-blocking, so a read after poll() or after the supervisor exited can never hang on a
    // pipe whose write end something else still holds open.
    unsafe {
        let fl = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
    }
    loop {
        if let Some((status, text)) = decode_startup_frame(&buf) {
            return done(
                fd,
                if status == b'S' {
                    StartupWait::Started(text)
                } else {
                    StartupWait::Failed(text)
                },
            );
        }
        let now = Instant::now();
        if now >= deadline {
            return done(fd, StartupWait::TimedOut);
        }
        let slice_ms = (deadline - now).min(Duration::from_millis(200)).as_millis() as i32;
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let r = unsafe { libc::poll(&mut pfd, 1, slice_ms.max(1)) };
        if r > 0 {
            let mut chunk = [0u8; 4096];
            let n = unsafe { libc::read(fd, chunk.as_mut_ptr() as *mut libc::c_void, chunk.len()) };
            if n > 0 {
                buf.extend_from_slice(&chunk[..n as usize]);
            } else if n == 0 {
                // EOF: the writer closed without a complete frame.
                return done(fd, StartupWait::SupervisorDied);
            } else if !matches!(
                std::io::Error::last_os_error().kind(),
                std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
            ) {
                return done(fd, StartupWait::SupervisorDied);
            }
        } else if let Some(pid) = supervisor_pid {
            let mut status = 0;
            let w = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if w == pid {
                // Exited: take whatever it managed to write, then decide.
                let mut chunk = [0u8; 4096];
                let n =
                    unsafe { libc::read(fd, chunk.as_mut_ptr() as *mut libc::c_void, chunk.len()) };
                if n > 0 {
                    buf.extend_from_slice(&chunk[..n as usize]);
                    continue;
                }
                return done(fd, StartupWait::SupervisorDied);
            }
        }
    }
}

/// Pull any images in `spec` that are not already cached locally.
///
/// Deduplicates image references so each distinct image is pulled at most once.
/// Returns an error (with the image name) on the first pull failure.
/// When `no_pull` is true, behaves exactly like the old code: fail immediately
/// if an image is not cached.
fn pull_missing_images(
    spec: &pelagos::compose::ComposeFile,
    no_pull: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // Collect unique image refs in service order.
    let mut seen = std::collections::HashSet::new();
    let refs: Vec<&str> = spec
        .services
        .iter()
        .map(|s| s.image.as_str())
        .filter(|r| seen.insert(*r))
        .collect();

    for image_ref in refs {
        if resolve_image(image_ref).is_ok() {
            continue; // already cached
        }
        if no_pull {
            return Err(format!(
                "image '{}' not found locally (use 'pelagos image pull {}' or omit --no-pull)",
                image_ref, image_ref
            )
            .into());
        }
        println!("Pulling {}...", image_ref);
        super::image::cmd_image_pull(image_ref, None, None, false, false)
            .map_err(|e| format!("failed to pull '{}': {}", image_ref, e))?;
    }
    Ok(())
}

fn cmd_compose_up_reml(
    file: &std::path::Path,
    project_name: Option<&str>,
    opts: UpOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let UpOptions {
        foreground,
        no_pull,
        no_wait,
        startup_timeout,
    } = opts;
    // Derive a preliminary project name from the file path so that
    // imperative `container-start` calls during eval use the right scope.
    let preliminary_project = if let Some(name) = project_name {
        name.to_string()
    } else {
        derive_project_name(file, None)?
    };
    let compose_dir = file
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .to_path_buf();

    let mut interp = Interpreter::new_with_runtime(preliminary_project, compose_dir.clone());
    interp
        .eval_file(file)
        .map_err(|e| format!("{}: {}", file.display(), e))?;

    // If the script never called (compose-up ...) it was purely imperative:
    // containers were started/stopped during evaluation.  Nothing more to do.
    let pending = match interp.take_pending() {
        Some(p) => p,
        None => return Ok(()),
    };
    let spec = pending.spec.ok_or("compose-up: missing spec")?;
    let effective_foreground = pending.foreground || foreground;

    // Honour explicit project override; fall back to spec or file-directory.
    let project = if let Some(name) = project_name {
        name.to_string()
    } else if let Some(name) = pending.project {
        name
    } else {
        derive_project_name(file, None)?
    };

    let hooks = interp.take_hooks();
    let order = topo_sort(&spec.services)?;

    // Check for already-running project, or clean up stale state from a
    // previous failed/crashed run.
    let state_file = pelagos::paths::compose_state_file(&project);
    if state_file.exists() {
        if let Ok(existing) = load_project_state(&project) {
            if existing.supervisor_pid > 0 && check_liveness(existing.supervisor_pid) {
                return Err(format!(
                    "project '{}' is already running (supervisor PID {})",
                    project, existing.supervisor_pid
                )
                .into());
            }
            // Supervisor is dead — stale state from a previous failed/crashed run.
            // Clean up before starting fresh so networks/containers don't conflict.
            log::info!("compose: cleaning up stale state for project '{}'", project);
            cleanup_stale_project(&existing);
        }
    }

    // Pull missing images upfront, before creating networks/volumes or forking.
    // This keeps pull output in the parent process (visible to the user) and
    // fails early before any infrastructure is created.
    pull_missing_images(&spec, no_pull)?;
    preflight_wasm_services(&spec)?;

    let compose_cfg = pelagos::config::PelagosConfig::load();
    let mut created_networks = Vec::new();
    for net in &spec.networks {
        let scoped = scoped_network_name(&project, &net.name);
        let config = pelagos::paths::network_config_dir(&scoped).join("config.json");
        if !config.exists() {
            // When the Remfile specifies an explicit subnet, use it.
            // Otherwise auto-allocate from the config file's pool.
            super::network::cmd_network_create(
                &scoped,
                net.subnet.as_deref(),
                None, // alloc_from: use config's auto_alloc_pool
            )
            .map_err(|e| format!("compose: failed to create network '{}': {}", scoped, e))?;
        }
        created_networks.push(scoped);
    }
    drop(compose_cfg);

    let mut created_volumes = Vec::new();
    for vol in &spec.volumes {
        let scoped = scoped_volume_name(&project, vol);
        let _ = Volume::open(&scoped).or_else(|_| Volume::create(&scoped));
        created_volumes.push(scoped);
    }

    for net in &created_networks {
        let dns_file = pelagos::paths::dns_network_file(net);
        if dns_file.exists() {
            let _ = std::fs::remove_file(&dns_file);
        }
    }

    if effective_foreground {
        run_compose_with_hooks(
            &project,
            file,
            &spec,
            &order,
            &created_networks,
            &created_volumes,
            &hooks,
            &compose_dir,
            &StartupNotifier::none(),
        )
    } else {
        // The startup handshake pipe (#653). CLOEXEC so no service the supervisor spawns
        // keeps the write end open. Skipped with --no-wait.
        let mut fds = [-1i32; 2];
        if !no_wait && unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let fork_result = unsafe { libc::fork() };
        match fork_result {
            -1 => Err(std::io::Error::last_os_error().into()),
            0 => {
                unsafe { libc::setsid() };
                // Become a subreaper so orphaned container descendants are
                // re-parented to us, not host init.
                unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
                let notifier = if no_wait {
                    StartupNotifier::none()
                } else {
                    unsafe { libc::close(fds[0]) };
                    StartupNotifier::new(fds[1])
                };
                if let Err(e) = run_compose_with_hooks(
                    &project,
                    file,
                    &spec,
                    &order,
                    &created_networks,
                    &created_volumes,
                    &hooks,
                    &compose_dir,
                    &notifier,
                ) {
                    log::error!("compose supervisor: {}", e);
                    // Idempotent: a no-op if the rollback path already reported.
                    notifier.failed(&e.to_string());
                    unsafe { libc::_exit(1) };
                }
                unsafe { libc::_exit(0) };
            }
            child_pid => {
                if no_wait {
                    std::thread::sleep(Duration::from_millis(200));
                    println!("Project '{}' started", project);
                    return Ok(());
                }
                unsafe { libc::close(fds[1]) };
                match wait_for_startup(fds[0], startup_timeout, Some(child_pid)) {
                    StartupWait::Started(notes) => {
                        println!("Project '{}' started", project);
                        if !notes.is_empty() {
                            eprintln!("{}", notes);
                        }
                        Ok(())
                    }
                    StartupWait::Failed(err) => {
                        Err(format!("project '{}' failed to start: {}", project, err).into())
                    }
                    StartupWait::SupervisorDied => Err(format!(
                        "the compose supervisor for project '{}' exited before reporting that it \
                         had started (run with RUST_LOG=info for its log)",
                        project
                    )
                    .into()),
                    StartupWait::TimedOut => Err(format!(
                        "project '{}' is still starting after {} s. The supervisor (pid {}) keeps \
                         going in the background: check `pelagos compose ps`, or `pelagos compose \
                         down` to abandon it. Raise --startup-timeout for a slow stack, or use \
                         --no-wait",
                        project,
                        startup_timeout.as_secs(),
                        child_pid
                    )
                    .into()),
                }
            }
        }
    }
}

/// Run compose supervision with optional `on-ready` hooks.
///
/// Called by the compose supervisor to run services with optional `on-ready` hooks.
#[allow(clippy::too_many_arguments)]
pub fn run_compose_with_hooks(
    project: &str,
    file: &std::path::Path,
    compose: &ComposeFile,
    order: &[String],
    created_networks: &[String],
    created_volumes: &[String],
    on_ready: &HookMap,
    compose_dir: &std::path::Path,
    startup: &StartupNotifier,
) -> Result<(), Box<dyn std::error::Error>> {
    let supervisor_pid = unsafe { libc::getpid() };

    // Install SIGTERM/SIGINT handlers so compose down can trigger a clean
    // shutdown.  The handler sets COMPOSE_SHUTDOWN; the main loop detects it
    // and runs the graceful teardown sequence before exiting.
    COMPOSE_SHUTDOWN.store(false, Ordering::Relaxed);
    unsafe {
        libc::signal(
            libc::SIGTERM,
            compose_signal_handler as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGINT,
            compose_signal_handler as *const () as libc::sighandler_t,
        );
    }

    let mut project_state = ComposeProject {
        name: project.to_string(),
        file_path: file.to_string_lossy().into_owned(),
        services: HashMap::new(),
        networks: created_networks.to_vec(),
        volumes: created_volumes.to_vec(),
        supervisor_pid,
        started_at: now_iso8601(),
    };
    save_project_state(&project_state)?;

    let svc_map: HashMap<&str, &ServiceSpec> = compose
        .services
        .iter()
        .map(|s| (s.name.as_str(), s))
        .collect();

    let mut container_pids: HashMap<String, i32> = HashMap::new();
    let mut container_ips: HashMap<String, String> = HashMap::new();
    // Track (container_name, pid, waiter_handle) for already-started services.
    // The JoinHandle is kept so the shutdown path can join each waiter thread,
    // guaranteeing teardown_resources() completes before the supervisor exits.
    let mut started: Vec<(String, i32, std::thread::JoinHandle<()>)> = Vec::new();

    let startup_result = (|| -> Result<(), Box<dyn std::error::Error>> {
        for svc_name in order {
            let svc = svc_map[svc_name.as_str()];
            let container_name = scoped_container_name(project, svc_name);

            for dep in &svc.depends_on {
                wait_for_dependency(project, dep, &container_pids, &container_ips)?;
            }

            log::info!(
                "compose: starting service '{}' as '{}'",
                svc_name,
                container_name
            );
            let (pid, handle) = spawn_service(project, svc, &container_name, compose, compose_dir)?;
            started.push((container_name.clone(), pid, handle));

            if let Ok(cstate) = super::read_state(&container_name) {
                if let Some(ip) = cstate.bridge_ip.as_ref() {
                    container_ips.insert(svc_name.clone(), ip.clone());
                }
                for (net, ip) in &cstate.network_ips {
                    container_ips.insert(svc_name.clone(), ip.clone());
                    let _ = net;
                }
            }

            container_pids.insert(svc_name.clone(), pid);

            // Fire on-ready hooks for this service.
            if let Some(hooks) = on_ready.get(svc_name.as_str()) {
                for hook in hooks {
                    hook().map_err(|e| format!("on-ready '{}': {}", svc_name, e))?;
                }
            }

            project_state.services.insert(
                svc_name.clone(),
                ComposeServiceState {
                    container_name: container_name.clone(),
                    status: "running".into(),
                    pid,
                },
            );
            save_project_state(&project_state)?;
        }
        Ok(())
    })();

    if let Err(e) = startup_result {
        // Roll back: kill all already-started containers, join their waiter
        // threads (so teardown_resources runs), then tear down networks.
        log::warn!("compose: startup failed ({}); rolling back", e);
        for (cn, pid, _handle) in &started {
            if *pid > 0 {
                unsafe { libc::kill(-pid, libc::SIGKILL) };
            }
            let _ = std::fs::remove_dir_all(container_dir(cn));
        }
        // Join waiter threads — teardown_resources() runs inside each one.
        for (_, _, handle) in started {
            let _ = handle.join();
        }
        for svc_name in project_state.services.keys() {
            for net in created_networks {
                let _ = pelagos::dns::dns_remove_entry(net, svc_name);
            }
        }
        for net in created_networks {
            if let Err(re) = super::network::cmd_network_rm(net) {
                log::warn!(
                    "compose: rollback: failed to remove network '{}': {}",
                    net,
                    re
                );
            }
        }
        let project_dir = pelagos::paths::compose_project_dir(project);
        let _ = std::fs::remove_dir_all(&project_dir);
        // Rolled back: now tell `compose up` (#653), so it reports the error and exits non-zero.
        startup.failed(&e.to_string());
        return Err(e);
    }

    // Warn if any container has already exited before the supervisor loop starts.
    // This surfaces unexpected early exits (e.g. bind+network on ZFS — issue #227).
    let mut notes: Vec<String> = Vec::new();
    for (cn, pid, _) in &started {
        if !check_liveness(*pid) {
            let exit_code = super::read_state(cn)
                .ok()
                .and_then(|s| s.exit_code)
                .map(|c| format!("exit code {}", c))
                .unwrap_or_else(|| "unknown exit status (check logs)".to_string());
            log::warn!(
                "compose: service '{}' (pid {}) has already exited ({}) — \
                 check 'pelagos logs {}' and enable RUST_LOG=debug for details",
                cn,
                pid,
                exit_code,
                cn
            );
            notes.push(format!(
                "warning: service '{}' has already exited ({}); check `pelagos logs {}`",
                cn, exit_code, cn
            ));
        }
    }

    // Every service is up: tell `compose up` (#653), with any early-exit warnings.
    startup.started(&notes.join("\n"));
    println!("All services started for project '{}'", project);

    loop {
        std::thread::sleep(Duration::from_secs(2));

        // SIGTERM or SIGINT received — run graceful shutdown then break.
        if COMPOSE_SHUTDOWN.load(Ordering::Relaxed) {
            log::info!("compose: shutdown requested; stopping all services");
            // First pass: SIGTERM to all still-running containers.
            for svc_state in project_state.services.values() {
                if svc_state.status != "exited"
                    && svc_state.pid > 0
                    && check_liveness(svc_state.pid)
                {
                    unsafe { libc::kill(-svc_state.pid, libc::SIGTERM) };
                }
            }
            // Grace period: give containers 5 s to exit cleanly.
            std::thread::sleep(Duration::from_secs(5));
            // Second pass: SIGKILL any that are still running.
            for svc_state in project_state.services.values() {
                if svc_state.pid > 0 && check_liveness(svc_state.pid) {
                    unsafe { libc::kill(-svc_state.pid, libc::SIGKILL) };
                }
            }
            break;
        }

        let mut all_exited = true;
        for (svc_name, svc_state) in &mut project_state.services {
            if svc_state.status == "exited" {
                continue;
            }
            if check_liveness(svc_state.pid) {
                all_exited = false;
            } else {
                log::info!("compose: service '{}' exited", svc_name);
                svc_state.status = "exited".into();
            }
        }
        save_project_state(&project_state)?;
        if all_exited {
            log::info!("compose: all services exited for project '{}'", project);
            break;
        }
    }

    // Join every waiter thread before the supervisor process exits.
    // Each waiter calls child.wait() → teardown_resources(), so joining here
    // guarantees that all veth/netns/nftables cleanup has completed.
    // compose down polls our liveness; when we exit, cleanup is done.
    for (_, _, handle) in started {
        let _ = handle.join();
    }

    Ok(())
}

/// Derive a [`SpawnConfig`] for a compose-managed container so that plain
/// `pelagos restart`/`pelagos start` can bring it back up without requiring
/// a full project teardown+recreate (#549). Mirrors the transformations
/// `spawn_service` itself applies from `ServiceSpec` to its `Command`,
/// translated into the generic `SpawnConfig`/`RunArgs` schema `cmd_run`
/// already understands — not a new restart mechanism, just enough persisted
/// state for the existing one to work on compose containers too.
///
/// Known, accepted fidelity gaps (pre-existing limitations of
/// `pelagos start`'s `SpawnConfig`, not introduced here):
/// - `apparmor_profile`/`selinux_label` have no `SpawnConfig` field at all
///   today — a restarted service loses either if the compose file set them.
/// - Image-config-derived fallbacks for workdir/user/env (used only when the
///   service doesn't set them explicitly) are not reproduced; a restarted
///   service uses only what the compose file itself specifies.
#[allow(clippy::too_many_arguments)]
fn build_compose_spawn_config(
    project: &str,
    svc: &ServiceSpec,
    full_ref: &str,
    exe_and_args: &[String],
    network_names: &[String],
    container_name: &str,
    compose_dir: &std::path::Path,
) -> SpawnConfig {
    // Relative bind-mount host paths are resolved against the compose file's
    // directory, matching the same resolution `spawn_service` already
    // performed (successfully, or this function would never be reached) when
    // it built the container's actual mounts.
    let resolve_host_path = |host_path: &str| -> String {
        if std::path::Path::new(host_path).is_relative() {
            compose_dir
                .join(host_path)
                .canonicalize()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| host_path.to_string())
        } else {
            host_path.to_string()
        }
    };
    let bind_ro: Vec<String> = svc
        .bind_mounts
        .iter()
        .filter(|b| b.read_only)
        .map(|b| format!("{}:{}", resolve_host_path(&b.host_path), b.container_path))
        .collect();
    let bind: Vec<String> = svc
        .bind_mounts
        .iter()
        .filter(|b| !b.read_only)
        .map(|b| format!("{}:{}", resolve_host_path(&b.host_path), b.container_path))
        .collect();
    let volume: Vec<String> = svc
        .volumes
        .iter()
        .map(|v| format!("{}:{}", scoped_volume_name(project, &v.name), v.mount_path))
        .collect();
    let publish: Vec<String> = svc
        .ports
        .iter()
        .map(|p| format!("{}:{}", p.host, p.container))
        .collect();
    let env: Vec<String> = svc
        .env
        .iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect();

    SpawnConfig {
        image: Some(full_ref.to_string()),
        exe: exe_and_args.first().cloned().unwrap_or_default(),
        args: exe_and_args.get(1..).unwrap_or(&[]).to_vec(),
        env,
        bind,
        bind_ro,
        volume,
        network: network_names.to_vec(),
        publish,
        dns: svc.dns.clone(),
        dns_search: vec![],
        dns_options: vec![],
        working_dir: svc.workdir.clone(),
        user: svc.user.clone(),
        hostname: Some(container_name.to_string()),
        cap_drop: svc.cap_drop.clone(),
        cap_add: svc.cap_add.clone(),
        // seccomp + no-new-privileges are always-on hardening defaults
        // `spawn_service` applies unconditionally (`.with_seccomp_default()`,
        // `.with_no_new_privileges(true)`) -- represented here as the same
        // --security-opt strings `pelagos run` would use, so cmd_run's
        // existing handling re-applies them identically on restart.
        security_opt: vec![
            "seccomp=default".to_string(),
            "no-new-privileges".to_string(),
        ],
        read_only: false,
        rm: false,
        nat: !network_names.is_empty(),
        no_nat: false,
        labels: vec![],
        tmpfs: svc.tmpfs_mounts.clone(),
        device: vec![],
        no_pid_ns: false,
        no_ipc_ns: false,
        privileged: false,
        stop_signal: None,
        cpuset_cpus: None,
        cpuset_mems: None,
        masked_paths_default: true,
    }
}

/// What building a service yields: the command to spawn, the argv it runs, and the
/// networks it joins (empty for Wasm services).
type ServiceCommand = (Command, Vec<String>, Vec<String>);

/// Build the `Command` for a Linux-image service: image layers, networks, volumes,
/// limits and hardening. Returns the command, the argv it runs and the networks it
/// joins. (Moved out of `spawn_service` unchanged so Wasm images can take another path.)
fn build_linux_service_command(
    project: &str,
    svc: &ServiceSpec,
    container_name: &str,
    compose: &ComposeFile,
    compose_dir: &std::path::Path,
    manifest: &pelagos::image::ImageManifest,
) -> Result<ServiceCommand, Box<dyn std::error::Error>> {
    let layers = pelagos::image::layer_dirs(manifest);
    if layers.is_empty() {
        return Err(format!("service '{}': image has no layers", svc.name).into());
    }
    let layer_dirs = layers.clone();

    // Determine command.
    let exe_and_args = if let Some(ref cmd) = svc.command {
        cmd.clone()
    } else {
        let mut cmd_vec = manifest.config.entrypoint.clone();
        cmd_vec.extend(manifest.config.cmd.clone());
        if cmd_vec.is_empty() {
            vec!["/bin/sh".to_string()]
        } else {
            cmd_vec
        }
    };

    let exe = &exe_and_args[0];
    let rest = &exe_and_args[1..];

    let mut cmd = Command::new(exe).args(rest).with_image_layers(layers);

    // Apply image config env.
    for env_str in &manifest.config.env {
        if let Some((k, v)) = env_str.split_once('=') {
            cmd = cmd.env(k, v);
        }
    }

    // Apply image config workdir.
    if !manifest.config.working_dir.is_empty() && svc.workdir.is_none() {
        cmd = cmd.with_cwd(&manifest.config.working_dir);
    }

    // Apply image config user as default.
    if svc.user.is_none() && !manifest.config.user.is_empty() {
        let (uid, gid) = super::parse_user_in_layers(&manifest.config.user, &layer_dirs)?;
        cmd = cmd.with_uid(uid);
        if let Some(g) = gid {
            cmd = cmd.with_gid(g);
        }
    }

    // Apply service-specific settings.

    // Networks: first is primary, rest are additional.
    let mut svc_network_names: Vec<String> = svc
        .networks
        .iter()
        .map(|n| scoped_network_name(project, n))
        .collect();
    if svc_network_names.is_empty() {
        // If no networks specified but compose has networks, use the first one.
        if let Some(first_net) = compose.networks.first() {
            svc_network_names.push(scoped_network_name(project, &first_net.name));
        }
    }
    if let Some(primary) = svc_network_names.first() {
        cmd = cmd.with_network(NetworkMode::BridgeNamed(primary.clone()));
    }
    for additional in svc_network_names.iter().skip(1) {
        cmd = cmd.with_additional_network(additional);
    }

    // NAT for internet access.
    if !svc_network_names.is_empty() {
        cmd = cmd.with_nat();
    }

    // Note: DNS nameservers (gateway IPs) are auto-injected by the container
    // runtime's spawn() — no explicit with_dns() needed here, UNLESS the
    // service declares its own (:dns ...) resolvers, which take priority
    // (mirrors `pelagos run --dns`'s "explicit --dns always wins").
    if !svc.dns.is_empty() {
        cmd = cmd.with_dns(&svc.dns);
    }

    // Volumes.
    for vol in &svc.volumes {
        let scoped = scoped_volume_name(project, &vol.name);
        let v = Volume::open(&scoped)?;
        cmd = cmd.with_volume(&v, &vol.mount_path);
    }

    // Bind mounts. Relative host paths are resolved against the compose file's directory.
    for bm in &svc.bind_mounts {
        let host = if std::path::Path::new(&bm.host_path).is_relative() {
            compose_dir
                .join(&bm.host_path)
                .canonicalize()
                .map_err(|e| {
                    format!(
                        "service '{}': bind-mount host path '{}': {}",
                        svc.name, bm.host_path, e
                    )
                })?
                .to_string_lossy()
                .into_owned()
        } else {
            bm.host_path.clone()
        };
        if bm.read_only {
            cmd = cmd.with_bind_mount_ro(&host, &bm.container_path);
        } else {
            cmd = cmd.with_bind_mount(&host, &bm.container_path);
        }
    }

    // tmpfs mounts: in-memory writable filesystems (empty options = kernel defaults).
    for path in &svc.tmpfs_mounts {
        cmd = cmd.with_tmpfs(path, "");
    }

    // Environment: service overrides, then a fallback PATH only if the image
    // didn't already supply one (to avoid clobbering bundler/gem/nvm paths).
    for (k, v) in &svc.env {
        cmd = cmd.env(k, v);
    }
    let image_sets_path = manifest.config.env.iter().any(|e| e.starts_with("PATH="));
    if !image_sets_path {
        cmd = cmd.env(
            "PATH",
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        );
    }

    // Port forwards.
    for port in &svc.ports {
        cmd = cmd.with_port_forward(port.host, port.container);
    }

    // Resource limits.
    if let Some(ref mem) = svc.memory {
        let bytes = super::parse_memory(mem)?;
        cmd = cmd.with_cgroup_memory(bytes);
    }
    if let Some(ref cpus) = svc.cpus {
        let (quota, period) = super::parse_cpus(cpus)?;
        cmd = cmd.with_cgroup_cpu_quota(quota, period);
    }

    // User.
    if let Some(ref u) = svc.user {
        let (uid, gid) = super::parse_user_in_layers(u, &layer_dirs)?;
        cmd = cmd.with_uid(uid);
        if let Some(g) = gid {
            cmd = cmd.with_gid(g);
        }
    }

    // Workdir.
    if let Some(ref w) = svc.workdir {
        cmd = cmd.with_cwd(w);
    }

    // ── Container hardening ──────────────────────────────────────────────────
    //
    // Applied last so we OR with any namespace flags already accumulated (e.g.
    // MOUNT from with_image_layers, NET from Loopback mode).

    // Namespaces: PID isolates the process tree so orphaned sub-processes
    // (e.g. postgres workers) are killed by the kernel when PID 1 exits.
    // UTS gives each container its own hostname; IPC isolates SysV/POSIX IPC.
    let ns = cmd.namespaces();
    cmd = cmd
        .with_namespaces(ns | Namespace::PID | Namespace::UTS | Namespace::IPC)
        .with_hostname(container_name);

    // Security: seccomp + capabilities + no-new-privileges + masked paths.
    //
    // Start from DEFAULT_CAPS (the Podman 11-cap set), then apply (cap-drop ...)
    // and (cap-add ...) from the service spec.  (cap-drop "ALL") drops everything
    // first, giving a zero-cap baseline before any cap-add is applied.
    let drop_all = svc.cap_drop.iter().any(|c| c.eq_ignore_ascii_case("ALL"));
    let mut effective_caps = if drop_all {
        pelagos::container::Capability::empty()
    } else {
        pelagos::container::Capability::DEFAULT_CAPS
    };
    if !drop_all && !svc.cap_drop.is_empty() {
        effective_caps &= !super::parse_capability_mask(&svc.cap_drop);
    }
    if !svc.cap_add.is_empty() {
        effective_caps |= super::parse_capability_mask(&svc.cap_add);
    }
    cmd = cmd
        .with_seccomp_default()
        .with_capabilities(effective_caps)
        .with_no_new_privileges(true)
        .with_masked_paths_default();

    if let Some(ref profile) = svc.apparmor_profile {
        cmd = cmd.with_apparmor_profile(profile);
    }
    if let Some(ref label) = svc.selinux_label {
        cmd = cmd.with_selinux_label(label);
    }

    Ok((cmd, exe_and_args, svc_network_names))
}

/// The compose options a Wasm service cannot honour yet, by name. They are rejected
/// instead of being silently dropped: a Wasm service runs as a `wasmtime`/`wasmedge`
/// subprocess with a WASI environment and preopened directories only; it has no
/// network namespace or WASI sockets (#71), no cgroup, user or capability handling.
pub(crate) fn wasm_service_unsupported(svc: &ServiceSpec) -> Vec<&'static str> {
    let mut bad = Vec::new();
    if !svc.networks.is_empty() {
        bad.push("networks (Wasm services have no network namespace or WASI sockets yet, #71)");
    }
    if !svc.ports.is_empty() {
        bad.push("ports");
    }
    if !svc.dns.is_empty() {
        bad.push("dns");
    }
    if svc.memory.is_some() {
        bad.push("memory");
    }
    if svc.cpus.is_some() {
        bad.push("cpus");
    }
    if svc.user.is_some() {
        bad.push("user");
    }
    if svc.workdir.is_some() {
        bad.push("workdir");
    }
    if !svc.cap_add.is_empty() || !svc.cap_drop.is_empty() {
        bad.push("cap-add / cap-drop");
    }
    if svc.apparmor_profile.is_some() || svc.selinux_label.is_some() {
        bad.push("apparmor / selinux");
    }
    if !svc.tmpfs_mounts.is_empty() {
        bad.push("tmpfs mounts");
    }
    if svc.bind_mounts.iter().any(|b| b.read_only) {
        bad.push("read-only bind mounts (WASI preopens are read-write)");
    }
    bad
}

fn wasm_unsupported_error(svc: &ServiceSpec, unsupported: &[&str]) -> String {
    format!(
        "service '{}': a Wasm image does not support: {}",
        svc.name,
        unsupported.join("; ")
    )
}

/// Refuse, before anything is created or forked, a project whose Wasm services cannot
/// run (#70): options a Wasm service cannot honour, or no Wasm runtime installed.
/// Otherwise the background supervisor fails after `compose up` has already printed
/// "started", and the error is lost.
fn preflight_wasm_services(spec: &ComposeFile) -> Result<(), Box<dyn std::error::Error>> {
    let mut wasm_services = Vec::new();
    for svc in &spec.services {
        let (_, manifest) = resolve_image(&svc.image)?;
        if !manifest.is_wasm_image() {
            continue;
        }
        let bad = wasm_service_unsupported(svc);
        if !bad.is_empty() {
            return Err(wasm_unsupported_error(svc, &bad).into());
        }
        wasm_services.push(svc.name.clone());
    }
    if !wasm_services.is_empty()
        && pelagos::wasm::find_wasm_runtime(pelagos::wasm::WasmRuntime::Auto).is_none()
    {
        return Err(format!(
            "service(s) {} use a Wasm image but no Wasm runtime was found in PATH — install \
             wasmtime or wasmedge (under sudo, pass it on: sudo env \"PATH=$PATH\" pelagos ...)",
            wasm_services.join(", ")
        )
        .into());
    }
    Ok(())
}

/// Build the `Command` for a Wasm-image service: the module runs through the system
/// Wasm runtime with the image and service environment as WASI env, and named
/// volumes and bind mounts as preopened directories (mapped host path -> mount path).
fn build_wasm_service_command(
    project: &str,
    svc: &ServiceSpec,
    compose_dir: &std::path::Path,
    manifest: &pelagos::image::ImageManifest,
) -> Result<ServiceCommand, Box<dyn std::error::Error>> {
    let unsupported = wasm_service_unsupported(svc);
    if !unsupported.is_empty() {
        return Err(wasm_unsupported_error(svc, &unsupported).into());
    }
    let wasm_path = manifest.wasm_module_path().ok_or_else(|| {
        format!(
            "service '{}': Wasm image has no module.wasm layer — re-pull the image",
            svc.name
        )
    })?;
    let wasm_str = wasm_path.to_string_lossy().into_owned();
    // Same argv convention as `pelagos run`: element 0 is the module path.
    let exe_and_args: Vec<String> = match &svc.command {
        Some(c) if !c.is_empty() => c.clone(),
        _ => vec![wasm_str.clone()],
    };
    let mut cmd = Command::new(&wasm_str)
        .args(&exe_and_args[1..])
        .with_wasm_runtime(pelagos::wasm::WasmRuntime::Auto);
    for env_str in &manifest.config.env {
        if let Some((k, v)) = env_str.split_once('=') {
            cmd = cmd.with_wasi_env(k, v);
        }
    }
    for (k, v) in &svc.env {
        cmd = cmd.with_wasi_env(k, v);
    }
    for vol in &svc.volumes {
        let scoped = scoped_volume_name(project, &vol.name);
        let v = Volume::open(&scoped)?;
        cmd = cmd.with_wasi_preopened_dir_mapped(v.path(), &vol.mount_path);
    }
    for bm in &svc.bind_mounts {
        let host = if std::path::Path::new(&bm.host_path).is_relative() {
            compose_dir
                .join(&bm.host_path)
                .canonicalize()
                .map_err(|e| {
                    format!(
                        "service '{}': bind-mount host path '{}': {}",
                        svc.name, bm.host_path, e
                    )
                })?
        } else {
            std::path::PathBuf::from(&bm.host_path)
        };
        cmd = cmd.with_wasi_preopened_dir_mapped(host, &bm.container_path);
    }
    Ok((cmd, exe_and_args, Vec::new()))
}

fn spawn_service(
    project: &str,
    svc: &ServiceSpec,
    container_name: &str,
    compose: &ComposeFile,
    compose_dir: &std::path::Path,
) -> Result<(i32, std::thread::JoinHandle<()>), Box<dyn std::error::Error>> {
    // Resolve image layers.
    let image_ref = &svc.image;
    let (full_ref, manifest) = resolve_image(image_ref)?;

    // A Wasm image has no Linux rootfs to run in: it runs through the Wasm runtime
    // (as `pelagos run` does). Treating it as a Linux container made the service
    // silently fail to start (#70).
    let (mut cmd, exe_and_args, svc_network_names) = if manifest.is_wasm_image() {
        build_wasm_service_command(project, svc, compose_dir, &manifest)?
    } else {
        build_linux_service_command(
            project,
            svc,
            container_name,
            compose,
            compose_dir,
            &manifest,
        )?
    };

    // Spawn detached with log capture.
    std::fs::create_dir_all(containers_dir())?;
    let dir = container_dir(container_name);
    std::fs::create_dir_all(&dir)?;

    let stdout_log = dir.join("stdout.log");
    let stderr_log = dir.join("stderr.log");

    cmd = cmd
        .stdin(Stdio::Null)
        .stdout(Stdio::Piped)
        .stderr(Stdio::Piped);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            // The state dir was created above for a container that never started. The rollback
            // only cleans up services that did start, so remove it here or it is left behind
            // (and `compose ps` then mistakes the failed project for a live one).
            let _ = pelagos::paths::guarded_remove_dir_all(&dir);
            return Err(format!("spawn '{}' failed: {}", svc.name, e).into());
        }
    };
    let pid = child.pid();

    let spawn_config = build_compose_spawn_config(
        project,
        svc,
        &full_ref,
        &exe_and_args,
        &svc_network_names,
        container_name,
        compose_dir,
    );

    // Write container state.
    let cstate = ContainerState {
        name: container_name.to_string(),
        rootfs: full_ref,
        status: ContainerStatus::Running,
        pid,
        watcher_pid: unsafe { libc::getpid() },
        started_at: now_iso8601(),
        exit_code: None,
        oom_killed: false,
        command: exe_and_args.clone(),
        stdout_log: Some(stdout_log.to_string_lossy().into_owned()),
        stderr_log: Some(stderr_log.to_string_lossy().into_owned()),
        bridge_ip: child.container_ip(),
        network_ips: child
            .container_ips()
            .into_iter()
            .map(|(name, ip)| (name.to_string(), ip))
            .collect(),
        health: None,
        health_config: None,
        spawn_config: Some(spawn_config),
        labels: std::collections::HashMap::new(),
        mnt_ns_inode: super::read_mnt_ns_inode(pid),
        upper_dir: None,
        network_ns_name: child.netns_name().map(|s| s.to_string()),
        cgroup_name: child.cgroup_path(),
    };
    write_state(&cstate)?;

    // Register DNS with bare service name.
    //
    // Upstream for the embedded per-network proxy: a service's own (:dns ...)
    // servers (if set) so unknown-name queries get forwarded there rather than
    // only ever reaching the default public resolvers (#552) — dns_add_entry
    // merges this into the network's shared upstream list.
    let dns_upstream: Vec<String> = if svc.dns.is_empty() {
        vec!["8.8.8.8".to_string(), "1.1.1.1".to_string()]
    } else {
        svc.dns.clone()
    };
    let all_ips: Vec<(String, String)> = cstate
        .network_ips
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    for (net_name, ip_str) in &all_ips {
        let ip: Ipv4Addr = match ip_str.parse() {
            Ok(ip) => ip,
            Err(_) => continue,
        };
        let net_def = match pelagos::network::load_network_def(net_name) {
            Ok(d) => d,
            Err(_) => continue,
        };
        // Register with the bare service name (not project-prefixed).
        if let Err(e) =
            pelagos::dns::dns_add_entry(net_name, &svc.name, ip, net_def.gateway, &dns_upstream)
        {
            log::warn!(
                "dns: failed to register '{}' on {}: {}",
                svc.name,
                net_name,
                e
            );
        }
    }

    // Wait for DNS daemon to be reachable on each network's gateway.
    // After dns_add_entry sends SIGHUP, the daemon needs time to reload
    // and bind new sockets. Without this, dependent services may start
    // before DNS is ready.
    for (net_name, _) in &all_ips {
        if let Ok(net_def) = pelagos::network::load_network_def(net_name) {
            let gw = net_def.gateway;
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if probe_dns(gw, &svc.name) {
                    log::debug!("dns: '{}' resolves on {} via {}", svc.name, net_name, gw);
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }

    // Single epoll relay thread: multiplexes stdout and stderr into log files.
    let svc_name = svc.name.clone();
    let cn = container_name.to_string();
    super::relay::start_log_relay(
        child.take_stdout(),
        child.take_stderr(),
        stdout_log.clone(),
        stderr_log.clone(),
    );

    // Spawn a waiter thread that runs teardown when the container exits.
    // The JoinHandle is returned to the supervisor so it can be joined during
    // shutdown — guaranteeing teardown_resources() completes before the
    // supervisor process exits.
    let cn_wait = cn.clone();
    let all_ips_wait = all_ips.clone();
    let svc_name_wait = svc_name.clone();
    let handle = std::thread::spawn(move || {
        let exit = child.wait(); // blocks until container exits; runs teardown_resources()
                                 // Log the exit status so unexpected early exits are visible.
        match &exit {
            Ok(status) => {
                if let Some(code) = status.code() {
                    if code != 0 {
                        log::warn!(
                            "compose: service '{}' exited with code {}",
                            svc_name_wait,
                            code
                        );
                    } else {
                        log::debug!("compose: service '{}' exited (code 0)", svc_name_wait);
                    }
                } else if let Some(sig) = status.signal() {
                    log::warn!(
                        "compose: service '{}' killed by signal {}",
                        svc_name_wait,
                        sig
                    );
                }
            }
            Err(e) => {
                log::warn!("compose: service '{}' wait error: {}", svc_name_wait, e);
            }
        }
        for (net_name, _) in &all_ips_wait {
            let _ = pelagos::dns::dns_remove_entry(net_name, &svc_name_wait);
        }
        // Update container state.
        if let Ok(mut st) = super::read_state(&cn_wait) {
            st.status = ContainerStatus::Exited;
            st.exit_code = exit.ok().and_then(|e| e.code());
            let _ = write_state(&st);
        }
    });

    Ok((pid, handle))
}

fn resolve_image(
    image_ref: &str,
) -> Result<(String, pelagos::image::ImageManifest), Box<dyn std::error::Error>> {
    use pelagos::image;

    if let Ok(m) = image::load_image(image_ref) {
        return Ok((image_ref.to_string(), m));
    }
    let normalised = normalise_image_reference(image_ref);
    let m = image::load_image(&normalised).map_err(|e| {
        format!(
            "image '{}' not found locally (run 'pelagos image pull {}'): {}",
            image_ref, image_ref, e
        )
    })?;
    Ok((normalised, m))
}

fn normalise_image_reference(reference: &str) -> String {
    pelagos::image::normalise_reference(reference)
}

// ---------------------------------------------------------------------------
// compose down
// ---------------------------------------------------------------------------

/// Kill and remove all containers listed in a stale project state, then
/// remove the associated networks and DNS entries.  Called when compose up
/// detects a dead supervisor with leftover state from a previous failed run.
fn cleanup_stale_project(state: &ComposeProject) {
    for svc_state in state.services.values() {
        if svc_state.pid > 0 && check_liveness(svc_state.pid) {
            unsafe { libc::kill(-svc_state.pid, libc::SIGKILL) };
        }
        let _ = std::fs::remove_dir_all(container_dir(&svc_state.container_name));
    }
    for svc_name in state.services.keys() {
        for net in &state.networks {
            let _ = pelagos::dns::dns_remove_entry(net, svc_name);
        }
    }
    for net in &state.networks {
        if let Err(e) = super::network::cmd_network_rm(net) {
            log::warn!(
                "compose: stale cleanup: failed to remove network '{}': {}",
                net,
                e
            );
        }
    }
    let project_dir = pelagos::paths::compose_project_dir(&state.name);
    let _ = std::fs::remove_dir_all(&project_dir);
}

pub(crate) fn parse_signal(s: &str) -> libc::c_int {
    match s.trim().to_uppercase().as_str() {
        "" | "SIGTERM" => libc::SIGTERM,
        "SIGQUIT" => libc::SIGQUIT,
        "SIGINT" => libc::SIGINT,
        "SIGHUP" => libc::SIGHUP,
        "SIGUSR1" => libc::SIGUSR1,
        "SIGUSR2" => libc::SIGUSR2,
        other => other.parse::<libc::c_int>().unwrap_or(libc::SIGTERM),
    }
}

fn cmd_compose_down(
    file: &std::path::Path,
    project_name: Option<&str>,
    remove_volumes: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let project = derive_project_name(file, project_name)?;
    let project_state = load_project_state(&project)
        .map_err(|_| format!("no running project '{}' found", project))?;

    // Signal the supervisor to run its graceful shutdown sequence, then wait
    // for it to exit.  The supervisor joins all waiter threads before exiting,
    // so when check_liveness returns false we know every teardown_resources()
    // call has completed and the network is clean.
    if project_state.supervisor_pid > 0 && check_liveness(project_state.supervisor_pid) {
        unsafe { libc::kill(project_state.supervisor_pid, libc::SIGTERM) };
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            std::thread::sleep(Duration::from_millis(100));
            if !check_liveness(project_state.supervisor_pid) {
                break; // supervisor exited; all network teardown complete
            }
            if std::time::Instant::now() >= deadline {
                log::warn!(
                    "compose: supervisor (pid {}) did not exit within 60 s; sending SIGKILL (network cleanup may be incomplete)",
                    project_state.supervisor_pid
                );
                unsafe { libc::kill(project_state.supervisor_pid, libc::SIGKILL) };
                break;
            }
        }
    }

    // Re-read the compose file to get topo order, per-service grace periods, and
    // stop signals.  Fall back gracefully if the file is unavailable.
    let (order, grace_map, stop_signal_map) = if let Ok(content) = std::fs::read_to_string(file) {
        if let Ok(compose) = parse_compose(&content) {
            let order = topo_sort(&compose.services).unwrap_or_default();
            let grace: HashMap<String, u64> = compose
                .services
                .iter()
                .map(|s| (s.name.clone(), s.stop_grace_period.unwrap_or(10)))
                .collect();
            // Resolve the stop signal: prefer the compose file's image manifest,
            // fall back to SIGTERM when the manifest is unavailable.
            let sig: HashMap<String, libc::c_int> = compose
                .services
                .iter()
                .map(|s| {
                    let raw_sig = resolve_image(&s.image)
                        .map(|(_, m)| m.config.stop_signal.clone())
                        .unwrap_or_default();
                    (s.name.clone(), parse_signal(&raw_sig))
                })
                .collect();
            (order, grace, sig)
        } else {
            (
                project_state.services.keys().cloned().collect(),
                HashMap::new(),
                HashMap::new(),
            )
        }
    } else {
        (
            project_state.services.keys().cloned().collect(),
            HashMap::new(),
            HashMap::new(),
        )
    };

    let reverse_order: Vec<String> = order.into_iter().rev().collect();

    for svc_name in &reverse_order {
        if let Some(svc_state) = project_state.services.get(svc_name) {
            let cn = &svc_state.container_name;
            if svc_state.pid > 0 && check_liveness(svc_state.pid) {
                let stop_sig = stop_signal_map
                    .get(svc_name)
                    .copied()
                    .unwrap_or(libc::SIGTERM);
                let grace_secs = grace_map.get(svc_name).copied().unwrap_or(10);
                log::info!(
                    "compose: stopping service '{}' (signal {}, grace {}s)",
                    svc_name,
                    stop_sig,
                    grace_secs
                );
                // Send stop signal to the entire process group so shell-entrypoint
                // children (e.g. the real prometheus binary) are also terminated.
                unsafe { libc::kill(-svc_state.pid, stop_sig) };
                let deadline = Instant::now() + Duration::from_secs(grace_secs);
                while Instant::now() < deadline && check_liveness(svc_state.pid) {
                    std::thread::sleep(Duration::from_millis(250));
                }
                if check_liveness(svc_state.pid) {
                    log::warn!("compose: SIGKILL process group for service '{}'", svc_name);
                    unsafe { libc::kill(-svc_state.pid, libc::SIGKILL) };
                    // Give the kernel time to fully release file locks before
                    // the next compose up starts a new instance.
                    std::thread::sleep(Duration::from_millis(2000));
                }
            }
            // Remove container state.
            let dir = container_dir(cn);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    // Also stop any services not in the current compose file order.
    for (svc_name, svc_state) in &project_state.services {
        if !reverse_order.contains(svc_name) {
            if svc_state.pid > 0 && check_liveness(svc_state.pid) {
                unsafe { libc::kill(-svc_state.pid, libc::SIGKILL) };
            }
            let dir = container_dir(&svc_state.container_name);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    // Clean up DNS entries for all services on all networks.
    for svc_name in project_state.services.keys() {
        for net in &project_state.networks {
            let _ = pelagos::dns::dns_remove_entry(net, svc_name);
        }
    }

    // Remove networks.
    for net in &project_state.networks {
        if let Err(e) = super::network::cmd_network_rm(net) {
            log::warn!("compose: failed to remove network '{}': {}", net, e);
        }
    }

    // Remove volumes if requested.
    if remove_volumes {
        for vol in &project_state.volumes {
            if let Err(e) = Volume::delete(vol) {
                log::warn!("compose: failed to remove volume '{}': {}", vol, e);
            }
        }
    }

    // Remove project state.
    let project_dir = pelagos::paths::compose_project_dir(&project);
    let _ = std::fs::remove_dir_all(&project_dir);

    println!("Project '{}' stopped and removed", project);
    Ok(())
}

// ---------------------------------------------------------------------------
// compose ps
// ---------------------------------------------------------------------------

fn cmd_compose_ps(
    file: &std::path::Path,
    project_name: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let project = derive_project_name(file, project_name)?;
    let project_state =
        load_project_state(&project).map_err(|_| format!("no project '{}' found", project))?;

    println!(
        "{:<15} {:<25} {:<10} {:<8}",
        "SERVICE", "CONTAINER", "STATUS", "PID"
    );
    for (svc_name, svc_state) in &project_state.services {
        let status = if check_liveness(svc_state.pid) {
            "running"
        } else {
            "exited"
        };
        println!(
            "{:<15} {:<25} {:<10} {:<8}",
            svc_name, svc_state.container_name, status, svc_state.pid
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// compose logs
// ---------------------------------------------------------------------------

fn cmd_compose_logs(
    file: &std::path::Path,
    project_name: Option<&str>,
    follow: bool,
    service: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let project = derive_project_name(file, project_name)?;
    let project_state =
        load_project_state(&project).map_err(|_| format!("no project '{}' found", project))?;

    let services_to_show: Vec<(&str, &ComposeServiceState)> = if let Some(svc_name) = service {
        let svc_state = project_state
            .services
            .get(svc_name)
            .ok_or_else(|| format!("service '{}' not found in project '{}'", svc_name, project))?;
        vec![(svc_name, svc_state)]
    } else {
        project_state
            .services
            .iter()
            .map(|(k, v)| (k.as_str(), v))
            .collect()
    };

    for (svc_name, svc_state) in &services_to_show {
        // Delegate to the existing logs infrastructure.
        let cn = &svc_state.container_name;
        if let Ok(cstate) = super::read_state(cn) {
            if let Some(ref log_path) = cstate.stdout_log {
                if let Ok(data) = std::fs::read(log_path) {
                    if !data.is_empty() {
                        // Prefix each line with service name.
                        let text = String::from_utf8_lossy(&data);
                        for line in text.lines() {
                            println!("{} | {}", svc_name, line);
                        }
                    }
                }
            }
            if let Some(ref log_path) = cstate.stderr_log {
                if let Ok(data) = std::fs::read(log_path) {
                    if !data.is_empty() {
                        let text = String::from_utf8_lossy(&data);
                        for line in text.lines() {
                            eprintln!("{} | {}", svc_name, line);
                        }
                    }
                }
            }
        }
    }

    if follow {
        // Poll for new content.
        loop {
            std::thread::sleep(Duration::from_millis(500));
            // Simple approach: re-check if any services are still running.
            let mut any_running = false;
            for (_, svc_state) in &services_to_show {
                if check_liveness(svc_state.pid) {
                    any_running = true;
                    break;
                }
            }
            if !any_running {
                break;
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Dependency readiness
// ---------------------------------------------------------------------------

fn wait_for_dependency(
    project: &str,
    dep: &Dependency,
    container_pids: &HashMap<String, i32>,
    container_ips: &HashMap<String, String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let dep_name = &dep.service;

    // Make sure the dependency is running.
    let dep_pid = container_pids.get(dep_name.as_str()).copied();
    if let Some(pid) = dep_pid {
        if !check_liveness(pid) {
            return Err(format!("dependency '{}' exited before becoming ready", dep_name).into());
        }
    }

    // If no health_check, just verify the process is alive and return.
    let check = match &dep.health_check {
        Some(c) => c,
        None => return Ok(()),
    };

    let timeout = Duration::from_secs(60);
    let interval = Duration::from_millis(250);
    let deadline = Instant::now() + timeout;

    // `:condition service_healthy` — poll state.json health field.
    if matches!(check, HealthCheck::Healthy) {
        let scoped_name = scoped_container_name(project, dep_name);
        log::info!(
            "compose: waiting for service '{}' to become healthy",
            dep_name
        );
        while Instant::now() < deadline {
            if let Ok(state) = super::read_state(&scoped_name) {
                if state.health == Some(super::HealthStatus::Healthy) {
                    log::info!("compose: service '{}' is healthy", dep_name);
                    return Ok(());
                }
            }
            if let Some(pid) = dep_pid {
                if !check_liveness(pid) {
                    return Err(format!(
                        "dependency '{}' exited before becoming healthy",
                        dep_name
                    )
                    .into());
                }
            }
            std::thread::sleep(interval);
        }
        return Err(format!(
            "dependency '{}' did not become healthy within {}s",
            dep_name,
            timeout.as_secs()
        )
        .into());
    }

    // Resolve the container IP (needed by Port and Http checks).
    let ip_str = container_ips
        .get(dep_name.as_str())
        .map(|s| s.as_str())
        .unwrap_or("");

    log::info!(
        "compose: waiting for service '{}' to pass health check",
        dep_name
    );

    while Instant::now() < deadline {
        if eval_health_check(check, dep_pid.unwrap_or(0), ip_str) {
            log::info!("compose: service '{}' is ready", dep_name);
            return Ok(());
        }
        // Check the dependency is still alive before sleeping.
        if let Some(pid) = dep_pid {
            if !check_liveness(pid) {
                return Err(format!(
                    "dependency '{}' exited while waiting for health check",
                    dep_name
                )
                .into());
            }
        }
        std::thread::sleep(interval);
    }

    Err(format!(
        "dependency '{}' did not become ready within {}s",
        dep_name,
        timeout.as_secs()
    )
    .into())
}

/// Recursively evaluate a [`HealthCheck`] expression.
///
/// - `pid`: PID of the container process (for `Cmd` checks).
/// - `ip`: container IP string (for `Port` and `Http` checks).
fn eval_health_check(check: &HealthCheck, pid: i32, ip: &str) -> bool {
    match check {
        HealthCheck::Port(p) => try_tcp(ip, *p),
        HealthCheck::Http(url) => try_http(url, ip),
        HealthCheck::Cmd(args) => try_exec(pid, args),
        HealthCheck::And(checks) => checks.iter().all(|c| eval_health_check(c, pid, ip)),
        HealthCheck::Or(checks) => checks.iter().any(|c| eval_health_check(c, pid, ip)),
        // Healthy is handled separately in wait_for_dependency via state.json polling.
        HealthCheck::Healthy => false,
    }
}

/// Attempt a TCP connection to `ip:port` with a 500ms timeout.
fn try_tcp(ip: &str, port: u16) -> bool {
    let addr_str = format!("{}:{}", ip, port);
    let addr: SocketAddr = match addr_str.parse() {
        Ok(a) => a,
        Err(_) => return false,
    };
    TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok()
}

/// Attempt an HTTP GET to `url`, replacing the host with `container_ip`.
///
/// Returns true if the response status is 2xx (200–299).
fn try_http(url: &str, container_ip: &str) -> bool {
    // Strip scheme.
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url);

    // Split host:port from path.
    let (host_port, path) = if let Some(slash) = rest.find('/') {
        (&rest[..slash], &rest[slash..])
    } else {
        (rest, "/")
    };

    // Extract port from host:port, defaulting to 80.
    let port: u16 = if let Some(colon) = host_port.rfind(':') {
        host_port[colon + 1..].parse().unwrap_or(80)
    } else {
        80
    };

    let target_url = format!("http://{}:{}{}", container_ip, port, path);

    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_millis(500))
        .build();

    match agent.get(&target_url).call() {
        Ok(resp) => {
            let status = resp.status();
            (200..300).contains(&status)
        }
        Err(_) => false,
    }
}

/// Run `args` inside the container identified by `pid`'s namespaces.
///
/// Returns true if the command exits with status 0.
fn try_exec(pid: i32, args: &[String]) -> bool {
    super::exec::exec_in_container(pid, args).unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn derive_project_name(
    file: &std::path::Path,
    explicit: Option<&str>,
) -> Result<String, Box<dyn std::error::Error>> {
    if let Some(name) = explicit {
        return Ok(name.to_string());
    }
    // Use the parent directory name.
    let abs = std::fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
    let parent = abs
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("default");
    // Sanitise: only alphanumeric + hyphen.
    let sanitised: String = parent
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    Ok(sanitised)
}

fn scoped_network_name(project: &str, net: &str) -> String {
    let name = format!("{}-{}", project, net);
    // Kernel IFNAMSIZ limit is 15; bridge name is "rm-{name}".
    // Network name itself is max 12 chars.
    if name.len() > 12 {
        // Truncate and append short hash.
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        name.hash(&mut hasher);
        let h = hasher.finish();
        format!("{}{:04x}", &name[..8], h as u16)
    } else {
        name
    }
}

fn scoped_volume_name(project: &str, vol: &str) -> String {
    format!("{}-{}", project, vol)
}

fn scoped_container_name(project: &str, service: &str) -> String {
    format!("{}-{}", project, service)
}

/// Send a DNS A-record query for `name` to the given gateway IP on port 53.
/// Returns true if we get any response (even NXDOMAIN means the daemon is alive).
fn probe_dns(gateway: Ipv4Addr, name: &str) -> bool {
    use std::net::UdpSocket;

    let addr = SocketAddr::new(gateway.into(), 53);
    let sock = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(_) => return false,
    };
    let _ = sock.set_read_timeout(Some(Duration::from_millis(200)));

    // Build a minimal DNS A query.
    let mut pkt = Vec::with_capacity(64);
    pkt.extend_from_slice(&[0xAB, 0xCD]); // ID
    pkt.extend_from_slice(&[0x01, 0x00]); // Flags: RD=1
    pkt.extend_from_slice(&[0x00, 0x01]); // QDCOUNT=1
    pkt.extend_from_slice(&[0x00, 0x00]); // ANCOUNT
    pkt.extend_from_slice(&[0x00, 0x00]); // NSCOUNT
    pkt.extend_from_slice(&[0x00, 0x00]); // ARCOUNT
                                          // QNAME
    for label in name.split('.') {
        if !label.is_empty() {
            pkt.push(label.len() as u8);
            pkt.extend_from_slice(label.as_bytes());
        }
    }
    pkt.push(0); // Root label
    pkt.extend_from_slice(&[0x00, 0x01]); // QTYPE = A
    pkt.extend_from_slice(&[0x00, 0x01]); // QCLASS = IN

    if sock.send_to(&pkt, addr).is_err() {
        return false;
    }

    let mut buf = [0u8; 512];
    matches!(sock.recv_from(&mut buf), Ok((n, _)) if n >= 12)
}

fn save_project_state(state: &ComposeProject) -> Result<(), Box<dyn std::error::Error>> {
    let dir = pelagos::paths::compose_project_dir(&state.name);
    std::fs::create_dir_all(&dir)?;
    let json = serde_json::to_string_pretty(state)?;
    std::fs::write(pelagos::paths::compose_state_file(&state.name), json)?;
    Ok(())
}

fn load_project_state(project: &str) -> Result<ComposeProject, Box<dyn std::error::Error>> {
    let data = std::fs::read_to_string(pelagos::paths::compose_state_file(project))?;
    let state: ComposeProject = serde_json::from_str(&data)?;
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pelagos::compose::{BindMount, PortMapping, VolumeMount};

    fn svc() -> ServiceSpec {
        ServiceSpec {
            name: "w".into(),
            image: "wasm-img".into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_plain_wasm_service_is_supported() {
        assert!(wasm_service_unsupported(&svc()).is_empty());
    }

    #[test]
    fn env_volumes_and_read_write_bind_mounts_are_supported() {
        let mut s = svc();
        s.env.insert("K".into(), "v".into());
        s.volumes.push(VolumeMount {
            name: "data".into(),
            mount_path: "/data".into(),
        });
        s.bind_mounts.push(BindMount {
            host_path: "/srv/in".into(),
            container_path: "/in".into(),
            read_only: false,
        });
        s.command = Some(vec!["/m.wasm".into(), "--flag".into()]);
        assert!(wasm_service_unsupported(&s).is_empty());
    }

    /// Every option a Wasm service cannot honour is named, never silently ignored
    /// (#70: it used to be dropped and the service just failed to start).
    #[test]
    fn options_a_wasm_service_cannot_honour_are_rejected_by_name() {
        let cases: Vec<(&str, Box<dyn Fn(&mut ServiceSpec)>)> = vec![
            ("networks", Box::new(|s| s.networks.push("front".into()))),
            (
                "ports",
                Box::new(|s| {
                    s.ports.push(PortMapping {
                        host: 8080,
                        container: 80,
                    })
                }),
            ),
            ("dns", Box::new(|s| s.dns.push("1.1.1.1".into()))),
            ("memory", Box::new(|s| s.memory = Some("64m".into()))),
            ("cpus", Box::new(|s| s.cpus = Some("0.5".into()))),
            ("user", Box::new(|s| s.user = Some("1000".into()))),
            ("workdir", Box::new(|s| s.workdir = Some("/w".into()))),
            ("cap-add", Box::new(|s| s.cap_add.push("NET_RAW".into()))),
            ("cap-drop", Box::new(|s| s.cap_drop.push("ALL".into()))),
            (
                "apparmor",
                Box::new(|s| s.apparmor_profile = Some("p".into())),
            ),
            ("selinux", Box::new(|s| s.selinux_label = Some("l".into()))),
            ("tmpfs", Box::new(|s| s.tmpfs_mounts.push("/tmp".into()))),
            (
                "read-only",
                Box::new(|s| {
                    s.bind_mounts.push(BindMount {
                        host_path: "/a".into(),
                        container_path: "/b".into(),
                        read_only: true,
                    })
                }),
            ),
        ];
        for (what, set) in cases {
            let mut s = svc();
            set(&mut s);
            let bad = wasm_service_unsupported(&s);
            assert!(
                bad.iter().any(|b| b.contains(what)),
                "{what} must be rejected, got {bad:?}"
            );
        }
    }

    #[test]
    fn wasm_service_error_names_the_service_and_every_offending_option() {
        let mut s = svc();
        s.ports.push(PortMapping {
            host: 1,
            container: 2,
        });
        s.memory = Some("1g".into());
        let manifest = pelagos::image::ImageManifest {
            reference: "wasm-img".into(),
            digest: "sha256:0".into(),
            layers: vec![],
            layer_types: vec!["application/wasm".into()],
            config: Default::default(),
        };
        let err = build_wasm_service_command("p", &s, std::path::Path::new("."), &manifest)
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(err.contains("service 'w'"), "{err}");
        assert!(err.contains("ports") && err.contains("memory"), "{err}");
    }

    // ── #653: the startup handshake ──────────────────────────────────────────

    fn pipe() -> (i32, i32) {
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        (fds[0], fds[1])
    }
    fn write_all(fd: i32, bytes: &[u8]) {
        let n = unsafe { libc::write(fd, bytes.as_ptr() as *const libc::c_void, bytes.len()) };
        assert_eq!(n as usize, bytes.len());
    }

    #[test]
    fn startup_frames_round_trip_and_wait_for_the_whole_frame() {
        for (st, text) in [
            (b'S', ""),
            (b'F', "boom"),
            (b'F', "multi\nline \u{1F980} error"),
        ] {
            let f = encode_startup_frame(st, text);
            assert_eq!(decode_startup_frame(&f), Some((st, text.to_string())));
        }
        let f = encode_startup_frame(b'F', "hello world");
        for cut in 0..f.len() {
            assert_eq!(decode_startup_frame(&f[..cut]), None, "cut at {cut}");
        }
        // Bytes after the frame are ignored.
        let mut two = encode_startup_frame(b'S', "a");
        two.extend(encode_startup_frame(b'F', "b"));
        assert_eq!(decode_startup_frame(&two), Some((b'S', "a".to_string())));
    }

    #[test]
    fn wait_reports_started_with_the_supervisors_notes() {
        let (r, w) = pipe();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            write_all(
                w,
                &encode_startup_frame(b'S', "warning: service 'x' exited"),
            );
            unsafe { libc::close(w) };
        });
        let got = wait_for_startup(r, Duration::from_secs(10), None);
        t.join().unwrap();
        assert_eq!(
            got,
            StartupWait::Started("warning: service 'x' exited".into())
        );
    }

    #[test]
    fn wait_reports_the_failure_text() {
        let (r, w) = pipe();
        write_all(
            w,
            &encode_startup_frame(b'F', "spawn 'db' failed: no such file"),
        );
        let got = wait_for_startup(r, Duration::from_secs(10), None);
        unsafe { libc::close(w) };
        assert_eq!(
            got,
            StartupWait::Failed("spawn 'db' failed: no such file".into())
        );
    }

    #[test]
    fn wait_notices_a_supervisor_that_closes_the_pipe_without_reporting() {
        let (r, w) = pipe();
        unsafe { libc::close(w) };
        assert_eq!(
            wait_for_startup(r, Duration::from_secs(10), None),
            StartupWait::SupervisorDied
        );
        // A frame cut short is the same thing.
        let (r, w) = pipe();
        let f = encode_startup_frame(b'F', "partial");
        write_all(w, &f[..f.len() - 3]);
        unsafe { libc::close(w) };
        assert_eq!(
            wait_for_startup(r, Duration::from_secs(10), None),
            StartupWait::SupervisorDied
        );
    }

    #[test]
    fn wait_times_out_while_the_supervisor_is_still_starting() {
        let (r, w) = pipe();
        let t0 = Instant::now();
        let got = wait_for_startup(r, Duration::from_millis(400), None);
        let took = t0.elapsed();
        unsafe { libc::close(w) };
        assert_eq!(got, StartupWait::TimedOut);
        assert!(
            took >= Duration::from_millis(350) && took < Duration::from_secs(5),
            "{took:?}"
        );
    }

    #[test]
    fn wait_notices_a_supervisor_process_that_exited_without_writing() {
        // The pipe's write end stays open here (as if a grandchild inherited it), so only the
        // waitpid check can see the supervisor is gone.
        let (r, w) = pipe();
        let child = std::process::Command::new("sh")
            .args(["-c", "exit 3"])
            .spawn()
            .unwrap();
        let t0 = Instant::now();
        let got = wait_for_startup(r, Duration::from_secs(30), Some(child.id() as i32));
        let took = t0.elapsed();
        unsafe { libc::close(w) };
        assert_eq!(got, StartupWait::SupervisorDied);
        assert!(
            took < Duration::from_secs(10),
            "should not wait out the timeout: {took:?}"
        );
    }

    #[test]
    fn notifier_sends_exactly_one_frame() {
        let (r, w) = pipe();
        let n = StartupNotifier::new(w);
        n.started("first");
        n.failed("second"); // after started(): must be a no-op, and must not write
        n.started("third");
        assert_eq!(
            wait_for_startup(r, Duration::from_secs(5), None),
            StartupWait::Started("first".into())
        );
        // A notifier with nobody to tell is silent.
        StartupNotifier::none().failed("nobody listens");
    }
}
