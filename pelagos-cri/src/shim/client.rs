//! ttrpc Task client for a running shim.
//!
//! Speaks the task API under either service name — `containerd.task.v3.Task`
//! (containerd 2.0's renamed copy) or `containerd.task.v2.Task` (what Kata,
//! gVisor's runsc and most shims register). The messages are wire-identical,
//! so the generated v3 message types serve both (#609). Adds per-call
//! timeouts and address normalisation. Only the RPCs needed by the first
//! shim-backed lifecycle are exposed; exec, stats, resize and update arrive
//! with the items that use them.

use std::time::Duration;

use containerd_shim_protos::api;
use std::sync::atomic::{AtomicBool, Ordering};

use containerd_shim_protos::protobuf::Message;
use containerd_shim_protos::shim_async::Client;
use containerd_shim_protos::ttrpc::context::{self, Context};

pub type Result<T> = std::result::Result<T, containerd_shim_protos::ttrpc::Error>;

/// Default per-RPC deadline.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Normalise a shim address to what `ttrpc` expects (`unix://<path>`).
pub fn ttrpc_address(addr: &str) -> String {
    let t = addr.trim();
    if t.contains("://") {
        t.to_string()
    } else {
        format!("unix://{t}")
    }
}

fn ctx(timeout: Duration) -> Context {
    context::with_timeout(timeout.as_nanos().min(i64::MAX as u128) as i64)
}

const TASK_V3: &str = "containerd.task.v3.Task";
const TASK_V2: &str = "containerd.task.v2.Task";

pub struct ShimClient {
    client: Client,
    /// Set once a v3 call is answered "service does not exist": the shim
    /// speaks v2 only. Probing v3 first matches containerd 2.x shims.
    use_v2: AtomicBool,
    timeout: Duration,
}

/// True when a ttrpc error says the shim doesn't register the service we
/// addressed (as opposed to the method failing).
///
/// Rust shims (Kata) answer INVALID_ARGUMENT "<svc> service does not exist";
/// Go shims (runsc, containerd's own) answer UNIMPLEMENTED "service <svc>".
fn is_service_missing(e: &containerd_shim_protos::ttrpc::Error) -> bool {
    use containerd_shim_protos::ttrpc::{Code, Error};
    match e {
        Error::RpcStatus(st) => {
            st.message().contains("service does not exist")
                || (st.code() == Code::UNIMPLEMENTED && st.message().starts_with("service "))
        }
        other => format!("{other:?}").contains("service does not exist"),
    }
}

impl ShimClient {
    pub async fn connect(address: &str) -> Result<Self> {
        let client = Client::connect(&ttrpc_address(address)).await?;
        Ok(Self {
            client,
            use_v2: AtomicBool::new(false),
            timeout: DEFAULT_TIMEOUT,
        })
    }

    /// Which task service name this client currently addresses.
    pub fn task_service(&self) -> &'static str {
        if self.use_v2.load(Ordering::Relaxed) {
            TASK_V2
        } else {
            TASK_V3
        }
    }

    async fn call_as<Req: Message, Resp: Message>(
        &self,
        service: &str,
        method: &str,
        c: Context,
        req: &Req,
    ) -> Result<Resp> {
        use containerd_shim_protos::ttrpc;
        let payload = req
            .write_to_bytes()
            .map_err(|e| ttrpc::Error::Others(format!("encode {method}: {e}")))?;
        let creq = ttrpc::Request {
            service: service.to_string(),
            method: method.to_string(),
            timeout_nano: c.timeout_nano,
            metadata: ttrpc::context::to_pb(c.metadata),
            payload,
            ..Default::default()
        };
        let res = self.client.request(creq).await?;
        Resp::parse_from_bytes(&res.payload)
            .map_err(|e| ttrpc::Error::Others(format!("decode {method}: {e}")))
    }

    async fn call<Req: Message, Resp: Message>(
        &self,
        method: &str,
        timeout: Duration,
        req: &Req,
    ) -> Result<Resp> {
        let svc = self.task_service();
        match self.call_as(svc, method, ctx(timeout), req).await {
            Err(e) if svc == TASK_V3 && is_service_missing(&e) => {
                log::info!("shim has no {TASK_V3}; falling back to {TASK_V2}");
                self.use_v2.store(true, Ordering::Relaxed);
                self.call_as(TASK_V2, method, ctx(timeout), req).await
            }
            r => r,
        }
    }

    pub fn with_timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    pub async fn connect_task(&self, id: &str) -> Result<api::ConnectResponse> {
        let mut r = api::ConnectRequest::new();
        r.id = id.into();
        self.call("Connect", self.timeout, &r).await
    }

    pub async fn create(&self, req: api::CreateTaskRequest) -> Result<api::CreateTaskResponse> {
        self.call("Create", self.timeout, &req).await
    }

    pub async fn start(&self, id: &str) -> Result<api::StartResponse> {
        let mut r = api::StartRequest::new();
        r.id = id.into();
        self.call("Start", self.timeout, &r).await
    }

    pub async fn state(&self, id: &str) -> Result<api::StateResponse> {
        let mut r = api::StateRequest::new();
        r.id = id.into();
        self.call("State", self.timeout, &r).await
    }

    pub async fn pids(&self, id: &str) -> Result<api::PidsResponse> {
        let mut r = api::PidsRequest::new();
        r.id = id.into();
        self.call("Pids", self.timeout, &r).await
    }

    pub async fn kill(&self, id: &str, signal: u32, all: bool) -> Result<()> {
        let mut r = api::KillRequest::new();
        r.id = id.into();
        r.signal = signal;
        r.all = all;
        self.call::<_, api::Empty>("Kill", self.timeout, &r)
            .await
            .map(|_| ())
    }

    /// Block until the task exits. Uses no deadline: a container may run for
    /// arbitrarily long.
    pub async fn wait(&self, id: &str) -> Result<api::WaitResponse> {
        let mut r = api::WaitRequest::new();
        r.id = id.into();
        self.call("Wait", Duration::ZERO, &r).await
    }

    pub async fn delete(&self, id: &str) -> Result<api::DeleteResponse> {
        let mut r = api::DeleteRequest::new();
        r.id = id.into();
        self.call("Delete", self.timeout, &r).await
    }

    // ── Exec processes (stdio item, #597) ───────────────────────────────────

    /// Register an exec process. `spec` is the OCI `Process` JSON.
    pub async fn exec(&self, req: api::ExecProcessRequest) -> Result<()> {
        self.call::<_, api::Empty>("Exec", self.timeout, &req)
            .await
            .map(|_| ())
    }

    pub async fn start_exec(&self, id: &str, exec_id: &str) -> Result<api::StartResponse> {
        let mut r = api::StartRequest::new();
        r.id = id.into();
        r.exec_id = exec_id.into();
        self.call("Start", self.timeout, &r).await
    }

    pub async fn kill_exec(&self, id: &str, exec_id: &str, signal: u32) -> Result<()> {
        let mut r = api::KillRequest::new();
        r.id = id.into();
        r.exec_id = exec_id.into();
        r.signal = signal;
        self.call::<_, api::Empty>("Kill", self.timeout, &r)
            .await
            .map(|_| ())
    }

    /// Block until the exec process exits (no deadline; callers bound it).
    pub async fn wait_exec(&self, id: &str, exec_id: &str) -> Result<api::WaitResponse> {
        let mut r = api::WaitRequest::new();
        r.id = id.into();
        r.exec_id = exec_id.into();
        self.call("Wait", Duration::ZERO, &r).await
    }

    pub async fn delete_exec(&self, id: &str, exec_id: &str) -> Result<api::DeleteResponse> {
        let mut r = api::DeleteRequest::new();
        r.id = id.into();
        r.exec_id = exec_id.into();
        self.call("Delete", self.timeout, &r).await
    }

    pub async fn resize_pty(&self, id: &str, exec_id: &str, width: u32, height: u32) -> Result<()> {
        let mut r = api::ResizePtyRequest::new();
        r.id = id.into();
        r.exec_id = exec_id.into();
        r.width = width;
        r.height = height;
        self.call::<_, api::Empty>("ResizePty", self.timeout, &r)
            .await
            .map(|_| ())
    }

    /// Close the process's stdin (EOF to the guest process).
    pub async fn close_io(&self, id: &str, exec_id: &str) -> Result<()> {
        let mut r = api::CloseIORequest::new();
        r.id = id.into();
        r.exec_id = exec_id.into();
        r.stdin = true;
        self.call::<_, api::Empty>("CloseIO", self.timeout, &r)
            .await
            .map(|_| ())
    }

    pub async fn stats(&self, id: &str) -> Result<api::StatsResponse> {
        let mut r = api::StatsRequest::new();
        r.id = id.into();
        self.call("Stats", self.timeout, &r).await
    }

    pub async fn shutdown(&self, id: &str) -> Result<()> {
        let mut r = api::ShutdownRequest::new();
        r.id = id.into();
        self.call::<_, api::Empty>("Shutdown", self.timeout, &r)
            .await
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use containerd_shim_protos::shim_async::{create_task, Task};
    use containerd_shim_protos::ttrpc;
    use containerd_shim_protos::ttrpc::asynchronous::{Server, Service, TtrpcContext};

    struct ConnMock;

    #[async_trait::async_trait]
    impl Task for ConnMock {
        async fn connect(
            &self,
            _: &TtrpcContext,
            _: api::ConnectRequest,
        ) -> ttrpc::Result<api::ConnectResponse> {
            let mut r = api::ConnectResponse::new();
            r.shim_pid = 11;
            r.task_pid = 22;
            Ok(r)
        }
    }

    /// Serve `ConnMock` registered under `service` (the generated v3 name, or
    /// re-keyed to v2 to mimic Kata/runsc/wasm shims), return a connected client.
    async fn serve_as(service: &str) -> (tempfile::TempDir, ShimClient) {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("s.sock");
        let mut services: std::collections::HashMap<String, Service> =
            create_task(Arc::new(ConnMock));
        if service != TASK_V3 {
            let svc = services.remove(TASK_V3).expect("generated v3 service");
            services.insert(service.to_string(), svc);
        }
        let mut server = Server::new()
            .bind(&format!("unix://{}", sock.display()))
            .unwrap()
            .register_service(services);
        server.start().await.unwrap();
        let c = ShimClient::connect(&sock.to_string_lossy()).await.unwrap();
        // Keep the server alive for the test's duration.
        std::mem::forget(server);
        (tmp, c)
    }

    /// #609: a shim registering only containerd.task.v2.Task (Kata, runsc, wasm)
    /// is reached via the v3-probe fallback, and the choice sticks.
    #[tokio::test]
    async fn falls_back_to_task_v2() {
        let (_t, c) = serve_as(TASK_V2).await;
        assert_eq!(c.task_service(), TASK_V3);
        let r = c.connect_task("x").await.unwrap();
        assert_eq!((r.shim_pid, r.task_pid), (11, 22));
        assert_eq!(c.task_service(), TASK_V2);
        // Subsequent calls go straight to v2.
        assert_eq!(c.connect_task("x").await.unwrap().task_pid, 22);
    }

    /// A v3 shim (containerd 2.x style) never triggers the fallback.
    #[tokio::test]
    async fn speaks_task_v3_when_registered() {
        let (_t, c) = serve_as(TASK_V3).await;
        assert_eq!(c.connect_task("x").await.unwrap().task_pid, 22);
        assert_eq!(c.task_service(), TASK_V3);
    }

    #[test]
    fn service_missing_matches_rust_and_go_shim_replies() {
        use containerd_shim_protos::ttrpc::{get_status, Code, Error};
        let rust = Error::RpcStatus(get_status(
            Code::INVALID_ARGUMENT,
            "containerd.task.v3.Task service does not exist",
        ));
        let go = Error::RpcStatus(get_status(
            Code::UNIMPLEMENTED,
            "service containerd.task.v3.Task",
        ));
        let method = Error::RpcStatus(get_status(Code::NOT_FOUND, "no such container"));
        let unimpl_method = Error::RpcStatus(get_status(Code::UNIMPLEMENTED, "method Stats"));
        assert!(is_service_missing(&rust) && is_service_missing(&go));
        assert!(!is_service_missing(&method) && !is_service_missing(&unimpl_method));
    }

    #[test]
    fn address_normalisation() {
        assert_eq!(ttrpc_address("/run/s/x"), "unix:///run/s/x");
        assert_eq!(ttrpc_address("unix:///run/s/x\n"), "unix:///run/s/x");
        assert_eq!(ttrpc_address("vsock://1:2"), "vsock://1:2");
    }
}
