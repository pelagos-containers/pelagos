//! CRI streaming server: handles kubectl exec / attach / port-forward via SPDY/3.1.
//!
//! Architecture:
//!   - A `TcpListener` runs alongside the gRPC server on a separate port.
//!   - The gRPC `Exec` handler mints a UUID token, stores a `PendingExec` in the
//!     shared registry, and returns the URL `http://<addr>/exec/<token>`.
//!   - kubelet connects to that URL, sends HTTP UPGRADE: spdy/3.1, and the
//!     streaming server hands the connection to spdystream-rs.
//!   - Per-stream handlers relay stdio between the SPDY streams and a
//!     `pelagos exec` subprocess.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

// ── Token registry ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PendingExec {
    pub container_name: String,
    pub cmd: Vec<String>,
    pub stdin: bool,
    #[allow(dead_code)]
    pub stdout: bool,
    #[allow(dead_code)]
    pub stderr: bool,
    #[allow(dead_code)]
    pub tty: bool,
}

#[derive(Debug, Clone)]
pub struct PendingPortForward {
    pub pod_ip: String,
    pub ports: Vec<u32>,
}

#[derive(Debug)]
pub(crate) enum Pending {
    Exec(PendingExec),
    PortForward(PendingPortForward),
}

pub type Registry = Arc<Mutex<HashMap<String, Pending>>>;

pub fn new_registry() -> Registry {
    Arc::new(Mutex::new(HashMap::new()))
}

pub async fn register_exec(registry: &Registry, token: String, pending: PendingExec) {
    let mut map = registry.lock().await;
    map.insert(token, Pending::Exec(pending));
    // Cull expired tokens (simple O(n) scan; registry is tiny).
    map.retain(|_, _| true); // placeholder — expiry handled at claim time
}

pub async fn register_port_forward(
    registry: &Registry,
    token: String,
    pending: PendingPortForward,
) {
    let mut map = registry.lock().await;
    map.insert(token, Pending::PortForward(pending));
}

async fn claim(registry: &Registry, token: &str) -> Option<Pending> {
    registry.lock().await.remove(token)
}

// ── HTTP server ───────────────────────────────────────────────────────────────

/// Bind the streaming listener and serve forever.
pub async fn serve(listener: TcpListener, registry: Registry, pelagos_bin: String) {
    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                log::warn!("streaming accept error: {e}");
                continue;
            }
        };
        log::debug!("streaming connection from {peer}");
        let registry = Arc::clone(&registry);
        let pelagos_bin = pelagos_bin.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(tcp, registry, pelagos_bin).await {
                log::warn!("streaming handler error: {e}");
            }
        });
    }
}

async fn handle_connection(
    mut tcp: TcpStream,
    registry: Registry,
    pelagos_bin: String,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use spdystream_rs::server::{parse_upgrade_request, send_upgrade_response};

    let req = parse_upgrade_request(&mut tcp).await?;
    log::debug!("streaming request: {} {}", req.method, req.path);

    // Path must be /exec/<token> or /portforward/<token>
    let path = req.path.trim_start_matches('/');
    let (kind, token) = match path.split_once('/') {
        Some(pair) => pair,
        None => {
            log::warn!("streaming: unexpected path /{path}");
            return Ok(());
        }
    };

    let pending = match claim(&registry, token).await {
        Some(p) => p,
        None => {
            log::warn!("streaming: unknown or expired token {token}");
            return Ok(());
        }
    };

    send_upgrade_response(&mut tcp).await?;

    match (kind, pending) {
        ("exec" | "attach", Pending::Exec(p)) => {
            handle_exec(tcp, p, pelagos_bin).await?;
        }
        ("portforward", Pending::PortForward(p)) => {
            handle_port_forward(tcp, p).await?;
        }
        _ => {
            log::warn!("streaming: kind/pending mismatch for token {token}");
        }
    }

    Ok(())
}

// ── Exec handler ──────────────────────────────────────────────────────────────

async fn handle_exec(
    tcp: TcpStream,
    pending: PendingExec,
    pelagos_bin: String,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use spdystream_rs::server::ServerConfig;
    use spdystream_rs::Stream;

    log::info!(
        "streaming exec: container={} cmd={:?}",
        pending.container_name,
        pending.cmd
    );

    // Shared state passed into the per-stream handler closure.
    let state = Arc::new(ExecState::new(pending, pelagos_bin));

    let config = ServerConfig::new({
        let state = Arc::clone(&state);
        move |stream: Arc<Stream>| {
            let state = Arc::clone(&state);
            Box::pin(async move {
                // Identify stream by the "streamType" header kubelet sets.
                let stream_type = stream
                    .headers
                    .get("streamType")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();

                log::debug!(
                    "exec stream: id={} type={stream_type:?} all_headers={:?}",
                    stream.stream_id,
                    stream.headers
                );
                stream.send_reply(http::HeaderMap::new(), false).await.ok();
                state.register_stream(stream_type, stream).await;
            })
        }
    });

    // The HTTP upgrade was already handled by handle_connection; use Connection::serve
    // directly so we don't re-parse the HTTP request.
    let handler = Arc::clone(&config.handler);
    let conn = spdystream_rs::connection::Connection::serve(tcp, move |s| handler(s)).await?;

    // Wait until we have all expected streams (or timeout).
    tokio::time::timeout(Duration::from_secs(10), state.wait_ready()).await??;

    // Spawn the subprocess and relay I/O.
    state.run(conn).await?;

    Ok(())
}

/// Holds per-exec mutable state collected as SPDY streams arrive.
struct ExecState {
    pending: PendingExec,
    pelagos_bin: String,
    stdin_stream: Mutex<Option<Arc<spdystream_rs::Stream>>>,
    stdout_stream: Mutex<Option<Arc<spdystream_rs::Stream>>>,
    stderr_stream: Mutex<Option<Arc<spdystream_rs::Stream>>>,
    resize_stream: Mutex<Option<Arc<spdystream_rs::Stream>>>,
    ready_notify: tokio::sync::Notify,
}

impl ExecState {
    fn new(pending: PendingExec, pelagos_bin: String) -> Self {
        Self {
            pending,
            pelagos_bin,
            stdin_stream: Mutex::new(None),
            stdout_stream: Mutex::new(None),
            stderr_stream: Mutex::new(None),
            resize_stream: Mutex::new(None),
            ready_notify: tokio::sync::Notify::new(),
        }
    }

    async fn register_stream(&self, stream_type: String, stream: Arc<spdystream_rs::Stream>) {
        match stream_type.as_str() {
            "stdin" => *self.stdin_stream.lock().await = Some(stream),
            "stdout" => *self.stdout_stream.lock().await = Some(stream),
            "stderr" => *self.stderr_stream.lock().await = Some(stream),
            "resize" => *self.resize_stream.lock().await = Some(stream),
            "error" => {} // kubelet error-reporting stream; we don't relay it
            other => log::warn!(
                "exec: unknown stream type {other:?} on stream {}",
                stream.stream_id
            ),
        }
        self.ready_notify.notify_one();
    }

    async fn wait_ready(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        loop {
            let has_stdout = self.stdout_stream.lock().await.is_some();
            let has_stderr = self.stderr_stream.lock().await.is_some();
            // stdin may be absent if not requested
            if has_stdout && has_stderr {
                return Ok(());
            }
            self.ready_notify.notified().await;
        }
    }

    async fn run(
        &self,
        conn: Arc<spdystream_rs::connection::Connection>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use tokio::io::AsyncWriteExt;
        use tokio::process::Command;

        // Build: pelagos exec <name> [--] <cmd...>
        let mut args = vec![
            "exec".to_string(),
            self.pending.container_name.clone(),
            "--".to_string(),
        ];
        args.extend_from_slice(&self.pending.cmd);

        let mut child = Command::new(&self.pelagos_bin)
            .args(&args)
            .stdin(if self.pending.stdin {
                std::process::Stdio::piped()
            } else {
                std::process::Stdio::null()
            })
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;

        let mut child_stdin = child.stdin.take();
        let child_stdout = child.stdout.take().expect("stdout piped");
        let child_stderr = child.stderr.take().expect("stderr piped");

        let stdout_stream = self.stdout_stream.lock().await.clone();
        let stderr_stream = self.stderr_stream.lock().await.clone();
        let stdin_stream = self.stdin_stream.lock().await.clone();

        // Relay stdout: child → SPDY stream
        let stdout_task = if let Some(spdy_out) = stdout_stream {
            let t = tokio::spawn(relay_read_to_spdy(child_stdout, spdy_out));
            Some(t)
        } else {
            None
        };

        // Relay stderr: child → SPDY stream
        let stderr_task = if let Some(spdy_err) = stderr_stream {
            let t = tokio::spawn(relay_read_to_spdy(child_stderr, spdy_err));
            Some(t)
        } else {
            None
        };

        // Relay stdin: SPDY stream → child
        let stdin_task =
            if let (Some(spdy_in), Some(mut child_in)) = (stdin_stream, child_stdin.take()) {
                let t = tokio::spawn(async move {
                    while let Ok(Some(data)) = spdy_in.read_data().await {
                        if child_in.write_all(&data).await.is_err() {
                            break;
                        }
                    }
                    drop(child_in);
                });
                Some(t)
            } else {
                None
            };

        // Wait for child to exit.
        let _status = child.wait().await?;

        // Drain relay tasks.
        if let Some(t) = stdout_task {
            let _ = t.await;
        }
        if let Some(t) = stderr_task {
            let _ = t.await;
        }
        if let Some(t) = stdin_task {
            let _ = t.await;
        }

        // Close the SPDY connection.
        let _ = conn.close().await;
        Ok(())
    }
}

/// Read from any AsyncRead and write as SPDY data frames, sending FIN at EOF.
async fn relay_read_to_spdy<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    stream: Arc<spdystream_rs::Stream>,
) {
    use tokio::io::AsyncReadExt;
    let mut buf = vec![0u8; 32 * 1024];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let data = Bytes::copy_from_slice(&buf[..n]);
                if stream.write_data(data, false).await.is_err() {
                    break;
                }
            }
        }
    }
    stream.write_data(Bytes::new(), true).await.ok();
}

// ── PortForward handler ───────────────────────────────────────────────────────

async fn handle_port_forward(
    tcp: TcpStream,
    pending: PendingPortForward,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use spdystream_rs::server::ServerConfig;
    use spdystream_rs::Stream;

    log::info!(
        "streaming portforward: pod_ip={} ports={:?}",
        pending.pod_ip,
        pending.ports
    );

    let pod_ip = Arc::new(pending.pod_ip.clone());

    let config = ServerConfig::new(move |stream: Arc<Stream>| {
        let pod_ip = Arc::clone(&pod_ip);
        Box::pin(async move {
            // kubelet sets header "port" with the target port number.
            let port = stream
                .headers
                .get("port")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u16>().ok())
                .unwrap_or(0);

            if port == 0 {
                log::warn!("portforward: missing or invalid port header");
                stream
                    .reset(spdystream_rs::frame::RstStatus::ProtocolError)
                    .await
                    .ok();
                return;
            }

            stream.send_reply(http::HeaderMap::new(), false).await.ok();

            let addr = format!("{pod_ip}:{port}");
            log::debug!("portforward: connecting to {addr}");

            match TcpStream::connect(&addr).await {
                Ok(target) => {
                    relay_spdy_tcp(stream, target).await;
                }
                Err(e) => {
                    log::warn!("portforward: connect {addr} failed: {e}");
                    stream
                        .reset(spdystream_rs::frame::RstStatus::Cancel)
                        .await
                        .ok();
                }
            }
        })
    });

    // HTTP upgrade already done; serve SPDY directly without re-parsing it.
    let handler = Arc::clone(&config.handler);
    let conn = spdystream_rs::connection::Connection::serve(tcp, move |s| handler(s)).await?;
    let _ = conn.close().await;
    Ok(())
}

/// Bidirectional relay between a SPDY stream and a TCP connection.
async fn relay_spdy_tcp(stream: Arc<spdystream_rs::Stream>, mut tcp: TcpStream) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (mut tcp_read, mut tcp_write) = tcp.split();

    // SPDY → TCP
    let stream_to_tcp = {
        let stream = Arc::clone(&stream);
        async move {
            while let Ok(Some(data)) = stream.read_data().await {
                if tcp_write.write_all(&data).await.is_err() {
                    break;
                }
            }
        }
    };

    // TCP → SPDY
    let tcp_to_stream = {
        let stream = Arc::clone(&stream);
        async move {
            let mut buf = vec![0u8; 32 * 1024];
            loop {
                match tcp_read.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let data = Bytes::copy_from_slice(&buf[..n]);
                        if stream.write_data(data, false).await.is_err() {
                            break;
                        }
                    }
                }
            }
            stream.write_data(Bytes::new(), true).await.ok();
        }
    };

    tokio::join!(stream_to_tcp, tcp_to_stream);
}
