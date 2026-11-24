//! ttrpc Task client for a running shim.
//!
//! Wraps the generated `containerd.task.v3.Task` client with per-call
//! timeouts and address normalisation. Only the RPCs needed by the first
//! shim-backed lifecycle are exposed; exec, stats, resize and update arrive
//! with the items that use them.

use std::time::Duration;

use containerd_shim_protos::api;
use containerd_shim_protos::shim_async::{Client, TaskClient};
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

pub struct ShimClient {
    task: TaskClient,
    timeout: Duration,
}

impl ShimClient {
    pub async fn connect(address: &str) -> Result<Self> {
        let client = Client::connect(&ttrpc_address(address)).await?;
        Ok(Self {
            task: TaskClient::new(client),
            timeout: DEFAULT_TIMEOUT,
        })
    }

    pub fn with_timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    pub async fn connect_task(&self, id: &str) -> Result<api::ConnectResponse> {
        let mut r = api::ConnectRequest::new();
        r.id = id.into();
        self.task.connect(ctx(self.timeout), &r).await
    }

    pub async fn create(&self, req: api::CreateTaskRequest) -> Result<api::CreateTaskResponse> {
        self.task.create(ctx(self.timeout), &req).await
    }

    pub async fn start(&self, id: &str) -> Result<api::StartResponse> {
        let mut r = api::StartRequest::new();
        r.id = id.into();
        self.task.start(ctx(self.timeout), &r).await
    }

    pub async fn state(&self, id: &str) -> Result<api::StateResponse> {
        let mut r = api::StateRequest::new();
        r.id = id.into();
        self.task.state(ctx(self.timeout), &r).await
    }

    pub async fn pids(&self, id: &str) -> Result<api::PidsResponse> {
        let mut r = api::PidsRequest::new();
        r.id = id.into();
        self.task.pids(ctx(self.timeout), &r).await
    }

    pub async fn kill(&self, id: &str, signal: u32, all: bool) -> Result<()> {
        let mut r = api::KillRequest::new();
        r.id = id.into();
        r.signal = signal;
        r.all = all;
        self.task.kill(ctx(self.timeout), &r).await.map(|_| ())
    }

    /// Block until the task exits. Uses no deadline: a container may run for
    /// arbitrarily long.
    pub async fn wait(&self, id: &str) -> Result<api::WaitResponse> {
        let mut r = api::WaitRequest::new();
        r.id = id.into();
        self.task.wait(ctx(Duration::ZERO), &r).await
    }

    pub async fn delete(&self, id: &str) -> Result<api::DeleteResponse> {
        let mut r = api::DeleteRequest::new();
        r.id = id.into();
        self.task.delete(ctx(self.timeout), &r).await
    }

    // ── Exec processes (stdio item, #597) ───────────────────────────────────

    /// Register an exec process. `spec` is the OCI `Process` JSON.
    pub async fn exec(&self, req: api::ExecProcessRequest) -> Result<()> {
        self.task.exec(ctx(self.timeout), &req).await.map(|_| ())
    }

    pub async fn start_exec(&self, id: &str, exec_id: &str) -> Result<api::StartResponse> {
        let mut r = api::StartRequest::new();
        r.id = id.into();
        r.exec_id = exec_id.into();
        self.task.start(ctx(self.timeout), &r).await
    }

    pub async fn kill_exec(&self, id: &str, exec_id: &str, signal: u32) -> Result<()> {
        let mut r = api::KillRequest::new();
        r.id = id.into();
        r.exec_id = exec_id.into();
        r.signal = signal;
        self.task.kill(ctx(self.timeout), &r).await.map(|_| ())
    }

    /// Block until the exec process exits (no deadline; callers bound it).
    pub async fn wait_exec(&self, id: &str, exec_id: &str) -> Result<api::WaitResponse> {
        let mut r = api::WaitRequest::new();
        r.id = id.into();
        r.exec_id = exec_id.into();
        self.task.wait(ctx(Duration::ZERO), &r).await
    }

    pub async fn delete_exec(&self, id: &str, exec_id: &str) -> Result<api::DeleteResponse> {
        let mut r = api::DeleteRequest::new();
        r.id = id.into();
        r.exec_id = exec_id.into();
        self.task.delete(ctx(self.timeout), &r).await
    }

    pub async fn resize_pty(&self, id: &str, exec_id: &str, width: u32, height: u32) -> Result<()> {
        let mut r = api::ResizePtyRequest::new();
        r.id = id.into();
        r.exec_id = exec_id.into();
        r.width = width;
        r.height = height;
        self.task
            .resize_pty(ctx(self.timeout), &r)
            .await
            .map(|_| ())
    }

    /// Close the process's stdin (EOF to the guest process).
    pub async fn close_io(&self, id: &str, exec_id: &str) -> Result<()> {
        let mut r = api::CloseIORequest::new();
        r.id = id.into();
        r.exec_id = exec_id.into();
        r.stdin = true;
        self.task.close_io(ctx(self.timeout), &r).await.map(|_| ())
    }

    pub async fn stats(&self, id: &str) -> Result<api::StatsResponse> {
        let mut r = api::StatsRequest::new();
        r.id = id.into();
        self.task.stats(ctx(self.timeout), &r).await
    }

    pub async fn shutdown(&self, id: &str) -> Result<()> {
        let mut r = api::ShutdownRequest::new();
        r.id = id.into();
        self.task.shutdown(ctx(self.timeout), &r).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_normalisation() {
        assert_eq!(ttrpc_address("/run/s/x"), "unix:///run/s/x");
        assert_eq!(ttrpc_address("unix:///run/s/x\n"), "unix:///run/s/x");
        assert_eq!(ttrpc_address("vsock://1:2"), "vsock://1:2");
    }
}
