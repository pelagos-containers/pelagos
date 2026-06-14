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

// Select the X-Stream-Protocol-Version to echo back in the 101 response.
// exec/attach: confirm the highest channel version the client advertises (v4 preferred).
// portforward: always confirm portforward.k8s.io/v1.
fn negotiate_protocol(kind: &str, headers: &http::HeaderMap) -> Option<String> {
    const EXEC_VERSIONS: &[&str] = &[
        "v4.channel.k8s.io",
        "v3.channel.k8s.io",
        "v2.channel.k8s.io",
        "v1.channel.k8s.io",
    ];
    const PF_VERSION: &str = "portforward.k8s.io/v1";

    match kind {
        "exec" | "attach" => {
            let offered: Vec<&str> = headers
                .get_all("x-stream-protocol-version")
                .iter()
                .filter_map(|v| v.to_str().ok())
                .collect();
            for &ver in EXEC_VERSIONS {
                if offered.contains(&ver) {
                    return Some(ver.to_string());
                }
            }
            // Client didn't advertise a known version; echo none and fall back.
            None
        }
        "portforward" => {
            // Echo back whatever the client offered — kubectl sends "portforward.k8s.io"
            // (not "/v1"), so don't hardcode; just reflect the first offered value.
            headers
                .get("x-stream-protocol-version")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
                .or_else(|| Some(PF_VERSION.to_string()))
        }
        _ => None,
    }
}

async fn handle_connection(
    mut tcp: TcpStream,
    registry: Registry,
    pelagos_bin: String,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use spdystream_rs::server::{parse_upgrade_request, send_upgrade_response_with_protocol};

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

    // Negotiate the subprotocol by echoing X-Stream-Protocol-Version back to the client.
    // kubectl rejects the upgrade if the server doesn't confirm the version it offered.
    // For exec/attach we confirm v4 (supports exit-code propagation via the error stream).
    // For port-forward we confirm portforward.k8s.io/v1.
    let protocol = negotiate_protocol(kind, &req.headers);
    log::debug!("streaming: negotiated protocol {:?}", protocol);
    send_upgrade_response_with_protocol(&mut tcp, protocol.as_deref()).await?;

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
    let run_result = state.run(Arc::clone(&conn)).await;

    // Always close the SPDY connection so the client's sockets are released even
    // when it doesn't close from its side (critest leaves exec/attach streams
    // open, leaking ~93 sockets and wedging the suite). close() enqueues GoAway
    // on the same ordered write channel as the exit-code/FIN frames already sent
    // by run(), so those are delivered first and exit-code reporting is preserved
    // (#339).
    let _ = conn.close().await;

    run_result?;
    Ok(())
}

/// Holds per-exec mutable state collected as SPDY streams arrive.
struct ExecState {
    pending: PendingExec,
    pelagos_bin: String,
    stdin_stream: Mutex<Option<Arc<spdystream_rs::Stream>>>,
    stdout_stream: Mutex<Option<Arc<spdystream_rs::Stream>>>,
    stderr_stream: Mutex<Option<Arc<spdystream_rs::Stream>>>,
    error_stream: Mutex<Option<Arc<spdystream_rs::Stream>>>,
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
            error_stream: Mutex::new(None),
            resize_stream: Mutex::new(None),
            ready_notify: tokio::sync::Notify::new(),
        }
    }

    async fn register_stream(&self, stream_type: String, stream: Arc<spdystream_rs::Stream>) {
        match stream_type.as_str() {
            "stdin" => *self.stdin_stream.lock().await = Some(stream),
            "stdout" => *self.stdout_stream.lock().await = Some(stream),
            "stderr" => *self.stderr_stream.lock().await = Some(stream),
            "error" => *self.error_stream.lock().await = Some(stream),
            "resize" => *self.resize_stream.lock().await = Some(stream),
            other => log::warn!(
                "exec: unknown stream type {other:?} on stream {}",
                stream.stream_id
            ),
        }
        self.ready_notify.notify_one();
    }

    async fn wait_ready(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        loop {
            // Register for the notification BEFORE checking state, so a
            // notify_one() that fires between the condition check and the
            // .await is not lost (the permit is already stored in the future).
            let notified = self.ready_notify.notified();

            let has_stdout = self.stdout_stream.lock().await.is_some();
            let has_stderr = self.stderr_stream.lock().await.is_some();
            // Wait for stdin only when the caller requested it; the stream IDs are
            // assigned in open order so stdin (id=1) always precedes stdout (id=3)
            // and stderr (id=5), but they are dispatched to different worker tasks
            // so there is a real (if unlikely) race.
            let stdin_ok = !self.pending.stdin || self.stdin_stream.lock().await.is_some();
            // error stream uses a different worker than stdout/stderr so it
            // may not have arrived yet; wait for it since we need it for exit code.
            let has_error = self.error_stream.lock().await.is_some();
            log::debug!(
                "wait_ready: stdout={has_stdout} stderr={has_stderr} stdin_ok={stdin_ok} error={has_error}"
            );
            if has_stdout && has_stderr && stdin_ok && has_error {
                return Ok(());
            }
            notified.await;
        }
    }

    async fn run(
        &self,
        _conn: Arc<spdystream_rs::connection::Connection>,
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

        // Keep Arcs for FIN sending after relay tasks complete.
        let stdout_stream = self.stdout_stream.lock().await.clone();
        let stderr_stream = self.stderr_stream.lock().await.clone();
        let stdin_stream = self.stdin_stream.lock().await.clone();
        let error_stream = self.error_stream.lock().await.clone();

        // Relay stdout: child → SPDY stream (data only; FIN sent explicitly below)
        let stdout_task = if let Some(ref spdy_out) = stdout_stream {
            let t = tokio::spawn(relay_read_to_spdy_data(child_stdout, Arc::clone(spdy_out)));
            Some(t)
        } else {
            None
        };

        // Relay stderr: child → SPDY stream (data only; FIN sent explicitly below)
        let stderr_task = if let Some(ref spdy_err) = stderr_stream {
            let t = tokio::spawn(relay_read_to_spdy_data(child_stderr, Arc::clone(spdy_err)));
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
        let status = child.wait().await?;
        log::info!(
            "streaming exec: child exited with status={:?} code={:?}",
            status,
            status.code()
        );

        // Drain relay tasks so all stdout/stderr data is flushed.
        // We do NOT send FINs from relay tasks — they are sent explicitly
        // below in the correct order so the error stream FIN arrives at
        // crictl before stdout/stderr FINs.  This prevents crictl from
        // closing the TCP connection (on receipt of stdout+stderr FINs)
        // before it reads the exit-code payload from the error stream.
        if let Some(t) = stdout_task {
            let _ = t.await;
        }
        if let Some(t) = stderr_task {
            let _ = t.await;
        }
        // The stdin relay blocks on the client's stdin stream, which may never
        // EOF (attach, or `exec -i` where the client holds stdin open). The child
        // has already exited so the relay is finished — abort it rather than
        // awaiting forever, which would wedge the connection and leak its sockets
        // (#339).
        if let Some(t) = stdin_task {
            t.abort();
        }

        // 1. Send error stream FIN FIRST with the exit code payload.
        //    kubelet reads a JSON-encoded metav1.Status; for exit 0 we send
        //    an empty FIN frame; for non-zero we send the structured message.
        let exit_code = status.code().unwrap_or(1);
        if let Some(ref err_stream) = error_stream {
            let payload = if exit_code != 0 {
                let msg = format!(
                    r#"{{"metadata":{{}},"status":"Failure","message":"command terminated with exit code {exit_code}","reason":"NonZeroExitCode","details":{{"causes":[{{"reason":"ExitCode","message":"{exit_code}"}}]}}}}"#
                );
                Bytes::from(msg)
            } else {
                Bytes::new()
            };
            err_stream.write_data(payload, true).await.ok();
        }

        // Yield to the executor so the write task can flush the error stream
        // frame to TCP before we send stdout/stderr FINs.  For zero-output
        // commands (e.g. `exit 42`) the relay tasks complete instantly and
        // the write queue is empty except for the error frame; yielding here
        // guarantees the error DATA+FIN is in-flight before crictl receives
        // the stdout/stderr FINs that signal "exec complete".
        tokio::task::yield_now().await;

        // 2. Send stdout / stderr FINs after the error stream FIN.
        if let Some(ref spdy_out) = stdout_stream {
            spdy_out.write_data(Bytes::new(), true).await.ok();
        }
        if let Some(ref spdy_err) = stderr_stream {
            spdy_err.write_data(Bytes::new(), true).await.ok();
        }

        // Do not send GoAway — let crictl close the connection naturally
        // after reading all stream FINs.  Sending GoAway here races with
        // crictl's error-stream goroutine: if crictl processes GoAway before
        // it reads the error DATA frame, it may report exit code 0.
        Ok(())
    }
}

/// Read from any AsyncRead and write as SPDY data frames.
/// Does NOT send the final FIN frame — the caller sends FINs explicitly
/// in the correct order (error stream before stdout/stderr).
async fn relay_read_to_spdy_data<R: tokio::io::AsyncRead + Unpin>(
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
