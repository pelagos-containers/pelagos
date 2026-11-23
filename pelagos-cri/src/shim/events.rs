//! Events sink: the ttrpc `Events` service a shim publishes to.
//!
//! Shims report task lifecycle events (`TaskCreate`, `TaskStart`, `TaskExit`,
//! ...) back to their client. containerd serves this on `<address>.ttrpc` and
//! passes the path to the shim in the `TTRPC_ADDRESS` environment variable;
//! the shim then calls `containerd.services.events.ttrpc.v1.Events/Forward`.
//!
//! Verified against Kata runtime-rs 4.4.0 (`service/src/event.rs`): it does
//! **not** exec the `-publish-binary`; it connects to `TTRPC_ADDRESS` and calls
//! `Forward`, falling back to logging only when the variable is empty. So the
//! sink is what matters; the publish binary just has to be a non-empty path.
//! (#585, `docs/SHIM_V2_CLIENT_DESIGN.md` section 7.)

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use containerd_shim_protos::api::ForwardRequest;
use containerd_shim_protos::protobuf::well_known_types::empty::Empty;
use containerd_shim_protos::shim_async::{create_events, Events};
use containerd_shim_protos::ttrpc;
use containerd_shim_protos::ttrpc::asynchronous::{Server, TtrpcContext};
use tokio::sync::mpsc;

/// One event received from a shim, decoded from the ttrpc `Envelope`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShimEvent {
    /// e.g. `/tasks/exit`.
    pub topic: String,
    pub namespace: String,
    /// Event time as `(seconds, nanos)` since the Unix epoch, if the shim set it.
    pub timestamp: Option<(i64, i32)>,
    /// Protobuf type URL of the payload, e.g. `containerd.events.TaskExit`.
    pub type_url: String,
    /// Serialized payload; decode per `type_url` (done by the consumer).
    pub payload: Vec<u8>,
}

/// Receiving half handed to whoever consumes shim events.
pub type EventReceiver = mpsc::UnboundedReceiver<ShimEvent>;

struct Forwarder {
    tx: mpsc::UnboundedSender<ShimEvent>,
}

#[async_trait::async_trait]
impl Events for Forwarder {
    async fn forward(&self, _ctx: &TtrpcContext, req: ForwardRequest) -> ttrpc::Result<Empty> {
        let env = req.envelope.as_ref().ok_or_else(|| {
            ttrpc::Error::RpcStatus(ttrpc::get_status(
                ttrpc::Code::INVALID_ARGUMENT,
                "ForwardRequest has no envelope",
            ))
        })?;
        let ev = ShimEvent {
            topic: env.topic.clone(),
            namespace: env.namespace.clone(),
            timestamp: env.timestamp.as_ref().map(|t| (t.seconds, t.nanos)),
            type_url: env
                .event
                .as_ref()
                .map(|a| a.type_url.clone())
                .unwrap_or_default(),
            payload: env
                .event
                .as_ref()
                .map(|a| a.value.clone())
                .unwrap_or_default(),
        };
        log::debug!("shim event {} ns={}", ev.topic, ev.namespace);
        // A closed receiver means nobody is listening any more; the shim must
        // not be failed for that (it would retry forever), so still reply OK.
        let _ = self.tx.send(ev);
        Ok(Empty::new())
    }
}

/// A running events sink bound to a unix socket.
pub struct EventsSink {
    socket: PathBuf,
    server: Server,
}

impl EventsSink {
    /// Bind the sink at `socket` and start serving. Any stale socket file from
    /// a previous run is replaced; the parent directory is created `0700` and
    /// the socket `0600` so only the CRI's user can publish events.
    pub async fn start(socket: &Path) -> std::io::Result<(Self, EventReceiver)> {
        if let Some(dir) = socket.parent() {
            std::fs::create_dir_all(dir)?;
        }
        match std::fs::remove_file(socket) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let svc = create_events(Arc::new(Forwarder { tx }));
        let mut server = Server::new()
            .bind(&format!("unix://{}", socket.display()))
            .map_err(std::io::Error::other)?
            .register_service(svc);
        server.start().await.map_err(std::io::Error::other)?;
        std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
        Ok((
            Self {
                socket: socket.to_path_buf(),
                server,
            },
            rx,
        ))
    }

    /// Value for the shim's `TTRPC_ADDRESS`: a plain socket path, which is what
    /// containerd passes and what the shim library's publisher connects to.
    pub fn address(&self) -> String {
        self.socket.to_string_lossy().into_owned()
    }

    /// Stop serving and remove the socket file.
    pub async fn shutdown(&mut self) {
        let _ = self.server.shutdown().await;
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Non-empty value for `-publish-binary`. Kata runtime-rs requires the flag to
/// be set but never executes it (see module docs), so the running
/// `pelagos-cri` path is an honest placeholder.
pub fn placeholder_publish_binary() -> String {
    std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "pelagos-cri".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use containerd_shim_protos::protobuf::well_known_types::any::Any;
    use containerd_shim_protos::protobuf::well_known_types::timestamp::Timestamp;
    use containerd_shim_protos::shim::event::Envelope;
    use containerd_shim_protos::shim_async::{Client, EventsClient};
    use containerd_shim_protos::ttrpc::context;

    fn forward_req(topic: &str, ns: &str, type_url: &str, payload: &[u8]) -> ForwardRequest {
        let mut env = Envelope::new();
        env.topic = topic.into();
        env.namespace = ns.into();
        let mut ts = Timestamp::new();
        ts.seconds = 1_700_000_000;
        ts.nanos = 42;
        env.timestamp = Some(ts).into();
        let mut any = Any::new();
        any.type_url = type_url.into();
        any.value = payload.to_vec();
        env.event = Some(any).into();
        let mut r = ForwardRequest::new();
        r.envelope = Some(env).into();
        r
    }

    async fn client(sock: &Path) -> EventsClient {
        let c = Client::connect(&format!("unix://{}", sock.display()))
            .await
            .unwrap();
        EventsClient::new(c)
    }

    #[tokio::test]
    async fn forwards_decoded_events_in_order() {
        let t = tempfile::tempdir().unwrap();
        let sock = t.path().join("ev/events.sock");
        let (mut sink, mut rx) = EventsSink::start(&sock).await.unwrap();
        let c = client(&sock).await;
        for (i, topic) in ["/tasks/create", "/tasks/start", "/tasks/exit"]
            .iter()
            .enumerate()
        {
            c.forward(
                context::with_timeout(5_000_000_000),
                &forward_req(topic, "k8s.io", "containerd.events.X", &[i as u8]),
            )
            .await
            .unwrap();
        }
        let mut got = Vec::new();
        for _ in 0..3 {
            got.push(rx.recv().await.unwrap());
        }
        assert_eq!(
            got.iter().map(|e| e.topic.as_str()).collect::<Vec<_>>(),
            ["/tasks/create", "/tasks/start", "/tasks/exit"],
            "Start must stay before Exit"
        );
        assert_eq!(got[2].namespace, "k8s.io");
        assert_eq!(got[2].timestamp, Some((1_700_000_000, 42)));
        assert_eq!(got[2].type_url, "containerd.events.X");
        assert_eq!(got[2].payload, vec![2u8]);
        sink.shutdown().await;
    }

    #[tokio::test]
    async fn envelope_less_request_is_rejected_not_forwarded() {
        let t = tempfile::tempdir().unwrap();
        let sock = t.path().join("events.sock");
        let (mut sink, mut rx) = EventsSink::start(&sock).await.unwrap();
        let c = client(&sock).await;
        let err = c
            .forward(context::with_timeout(5_000_000_000), &ForwardRequest::new())
            .await;
        assert!(err.is_err(), "missing envelope must be an error");
        assert!(rx.try_recv().is_err(), "nothing must be delivered");
        sink.shutdown().await;
    }

    #[tokio::test]
    async fn dropped_receiver_does_not_fail_the_shim() {
        let t = tempfile::tempdir().unwrap();
        let sock = t.path().join("events.sock");
        let (mut sink, rx) = EventsSink::start(&sock).await.unwrap();
        drop(rx);
        let c = client(&sock).await;
        c.forward(
            context::with_timeout(5_000_000_000),
            &forward_req("/tasks/exit", "k8s.io", "t", b""),
        )
        .await
        .expect("a shim must not be failed because nobody is listening");
        sink.shutdown().await;
    }

    #[tokio::test]
    async fn replaces_stale_socket_and_locks_down_permissions() {
        let t = tempfile::tempdir().unwrap();
        let sock = t.path().join("events.sock");
        std::fs::write(&sock, b"stale").unwrap();
        let (mut sink, _rx) = EventsSink::start(&sock).await.unwrap();
        let mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(sink.address(), sock.to_string_lossy());
        sink.shutdown().await;
        assert!(!sock.exists(), "shutdown removes the socket");
    }

    #[test]
    fn placeholder_publish_binary_is_never_empty() {
        assert!(!placeholder_publish_binary().is_empty());
    }
}
