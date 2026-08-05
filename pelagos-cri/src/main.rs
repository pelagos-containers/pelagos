//! Kubernetes CRI gRPC server that delegates to the pelagos container runtime.

pub mod cri {
    #![allow(clippy::all)]
    tonic::include_proto!("runtime.v1");
}

mod cni;
mod cri_metrics;
mod image;
mod invoke;
mod runtime;
mod scope;
mod state;
mod streaming;

use clap::Parser;
use image::ImageSvc;
use runtime::RuntimeSvc;
use state::AppState;
use tokio::signal::unix::{signal, SignalKind};
use tokio_stream::wrappers::UnixListenerStream;

#[derive(Parser)]
#[clap(name = "pelagos-cri", about = "CRI gRPC server for pelagos", version)]
struct Args {
    /// Unix socket path to listen on.
    #[clap(long, default_value = "/run/pelagos/cri.sock")]
    socket: String,
    /// Path to the pelagos binary.
    #[clap(long, default_value = "pelagos")]
    pelagos_bin: String,
    /// TCP address for the SPDY streaming server (exec/attach/port-forward).
    #[clap(long, default_value = "127.0.0.1:0")]
    streaming_addr: String,
    /// TCP address for the Prometheus /metrics HTTP endpoint. Loopback-only
    /// by default — this is operational telemetry, not meant to be exposed
    /// beyond the node without an explicit operator choice.
    #[clap(long, default_value = "127.0.0.1:9091")]
    metrics_addr: String,
}

/// Scan `/run/pelagos/containers/*/state.json` for containers that were
/// `running` when the previous CRI instance died but whose watcher process is
/// now gone.  Such orphans hold OS resources (bound ports, cgroup quota, netns)
/// that block the next pod restart.
///
/// For each orphan found: send SIGKILL to the container process and log it.
/// Runs synchronously before the gRPC server accepts connections so the kubelet
/// never sees a pod fail to start because of an orphan from the last run (#472).
fn kill_orphaned_containers() {
    #[derive(serde::Deserialize)]
    struct State {
        #[serde(default)]
        status: String,
        #[serde(default)]
        pid: i32,
        #[serde(default)]
        watcher_pid: i32,
    }

    let containers_dir = std::path::Path::new("/run/pelagos/containers");
    let Ok(entries) = std::fs::read_dir(containers_dir) else {
        return;
    };

    for entry in entries.flatten() {
        let state_path = entry.path().join("state.json");
        let Ok(data) = std::fs::read_to_string(&state_path) else {
            continue;
        };
        let Ok(s) = serde_json::from_str::<State>(&data) else {
            continue;
        };
        if s.status != "running" {
            continue;
        }
        // watcher_pid == 0 means either a non-detached container or a very old
        // state.json before this field was added — skip rather than guess.
        if s.watcher_pid <= 0 {
            continue;
        }
        let watcher_alive = unsafe { libc::kill(s.watcher_pid, 0) == 0 };
        if watcher_alive {
            continue;
        }
        // Watcher is dead. Check whether the container process is still running.
        if s.pid <= 0 {
            continue;
        }
        let container_alive = unsafe { libc::kill(s.pid, 0) == 0 };
        if !container_alive {
            continue;
        }
        log::warn!(
            "orphaned container process {} (watcher {} dead) — sending SIGKILL (#472)",
            s.pid,
            s.watcher_pid
        );
        unsafe {
            libc::kill(s.pid, libc::SIGKILL);
        }
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = Args::parse();

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    if let Err(e) = rt.block_on(async_run(args)) {
        log::error!("{}", e);
        std::process::exit(1);
    }
}

async fn async_run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;

    // Create socket directory
    if let Some(parent) = std::path::Path::new(&args.socket).parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    // Remove stale socket
    if std::path::Path::new(&args.socket).exists() {
        std::fs::remove_file(&args.socket)?;
    }

    // Bind streaming TCP listener before starting anything else so we know the
    // assigned port (when streaming_addr uses port 0).
    let streaming_listener = tokio::net::TcpListener::bind(&args.streaming_addr).await?;
    let streaming_base_url = format!("http://{}", streaming_listener.local_addr()?);
    log::info!("pelagos-cri streaming server on {streaming_base_url}");

    let registry = streaming::new_registry();

    cri_metrics::install(args.metrics_addr.parse()?)?;
    cni::start_semaphore_gauge_sampler();

    // Before accepting any kubelet requests, kill container processes that were
    // orphaned by a watcher that died during the previous CRI lifetime (e.g. a
    // pelagos upgrade). If the watcher is gone but the container process is still
    // running, it holds resources (ports, cgroup quota) that the next pod restart
    // needs. PR_SET_PDEATHSIG (set since v0.65.62) prevents NEW orphans; this
    // scan recovers PRE-EXISTING ones (#472).
    kill_orphaned_containers();

    let app_state = AppState::new(args.pelagos_bin.clone());

    // Periodically reap dead-pause ("phantom") sandboxes so we never keep
    // presenting the kubelet an orphaned, un-operable sandbox to garbage-collect
    // — the path that deleted the host /bin (#347). Without this, a phantom
    // lingers in the live listing until the next restart's reconciliation.
    {
        let reaper_state = app_state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                tick.tick().await;
                reaper_state.reconcile_stale_sandboxes().await;
            }
        });
    }

    let runtime_svc = RuntimeSvc {
        state: app_state.clone(),
        streaming_base_url: streaming_base_url.clone(),
        registry: registry.clone(),
    };
    let image_svc = ImageSvc {
        state: app_state.clone(),
    };

    // Spawn the SPDY streaming server.
    let pelagos_bin = args.pelagos_bin.clone();
    tokio::spawn(streaming::serve(streaming_listener, registry, pelagos_bin));

    let uds = tokio::net::UnixListener::bind(&args.socket)?;
    std::fs::set_permissions(&args.socket, std::fs::Permissions::from_mode(0o660))?;
    log::info!("pelagos-cri listening on {}", args.socket);

    let incoming = UnixListenerStream::new(uds);

    let mut sigterm = signal(SignalKind::terminate())?;

    tonic::transport::Server::builder()
        .add_service(cri::runtime_service_server::RuntimeServiceServer::new(
            runtime_svc,
        ))
        .add_service(cri::image_service_server::ImageServiceServer::new(
            image_svc,
        ))
        .serve_with_incoming_shutdown(incoming, async move {
            sigterm.recv().await;
            log::info!("received SIGTERM, shutting down");
        })
        .await?;

    Ok(())
}
