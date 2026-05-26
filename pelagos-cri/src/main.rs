//! Kubernetes CRI gRPC server that delegates to the pelagos container runtime.

pub mod cri {
    #![allow(clippy::all)]
    tonic::include_proto!("runtime.v1");
}

mod cni;
mod image;
mod invoke;
mod runtime;
mod state;
mod streaming;

use clap::Parser;
use image::ImageSvc;
use runtime::RuntimeSvc;
use state::AppState;
use tokio::signal::unix::{signal, SignalKind};
use tokio_stream::wrappers::UnixListenerStream;

#[derive(Parser)]
#[clap(name = "pelagos-cri", about = "CRI gRPC server for pelagos")]
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

    let app_state = AppState::new(args.pelagos_bin.clone());

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
