//! Docker Engine API server that delegates to the `pelagos` CLI.
//! Linux-only binary.

#[cfg(target_os = "linux")]
mod pelagos_state;
#[cfg(target_os = "linux")]
mod state;
#[cfg(target_os = "linux")]
mod types;
#[cfg(target_os = "linux")]
mod handlers;

fn main() {
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("pelagos-dockerd only runs on Linux");
        std::process::exit(1);
    }
    #[cfg(target_os = "linux")]
    linux_run();
}

#[cfg(target_os = "linux")]
#[derive(clap::Parser)]
#[clap(name = "pelagos-dockerd", about = "Docker Engine API shim for pelagos")]
struct Args {
    /// Unix socket path to listen on.
    #[clap(long, default_value = "/var/run/pelagos-dockerd.sock")]
    socket: String,
}

#[cfg(target_os = "linux")]
fn linux_run() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    use clap::Parser;
    let args = Args::parse();

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    if let Err(e) = rt.block_on(async_run(args)) {
        log::error!("{}", e);
        std::process::exit(1);
    }
}

#[cfg(target_os = "linux")]
async fn async_run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::UnixListener;

    if std::path::Path::new(&args.socket).exists() {
        std::fs::remove_file(&args.socket)?;
    }
    if let Some(parent) = std::path::Path::new(&args.socket).parent() {
        std::fs::create_dir_all(parent)?;
    }

    let app_state = state::AppState::new();
    let router = handlers::router(app_state);

    let listener = UnixListener::bind(&args.socket)?;
    std::fs::set_permissions(&args.socket, std::fs::Permissions::from_mode(0o660))?;

    log::info!("listening on {}", args.socket);

    axum::serve(listener, router).await?;
    Ok(())
}
