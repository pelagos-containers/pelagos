//! Exec and ExecSync over the shim Task API (epic #574, #597).
//!
//! An exec process shares the container's task id and is addressed by a unique
//! `exec_id`. Its OCI `Process` is the container's own process (capabilities,
//! rlimits, user, no-new-privileges, ...) with args, env, cwd and terminal
//! replaced, exactly what containerd's CRI plugin derives.
//!
//! - [`exec_sync`]: run to completion with captured, bounded output and a
//!   timeout (CRI `ExecSync`).
//! - [`ExecSession`]: streaming exec (CRI `Exec`): FIFOs + pump + stdin, with
//!   `wait`, `resize` and `close_stdin`. The SPDY plumbing in `streaming.rs`
//!   drives it when the backend is wired in.

use std::path::{Path, PathBuf};
use std::time::Duration;

use containerd_shim_protos::api;
use containerd_shim_protos::protobuf::well_known_types::any::Any;
use oci_spec::runtime::{Process, Spec};

use super::client::ShimClient;
use super::stdio::{Captured, Pump, Sink, StdinWriter, Stdio};

/// Type URL containerd uses for the exec process spec.
pub const PROCESS_TYPE_URL: &str = "types.containerd.io/opencontainers/runtime-spec/1/Process";

/// Default cap on captured ExecSync output per stream.
pub const DEFAULT_CAPTURE_LIMIT: usize = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("exec: empty command")]
    EmptyCommand,
    #[error("reading bundle config: {0}")]
    Config(String),
    #[error("stdio: {0}")]
    Io(#[from] std::io::Error),
    #[error("shim: {0}")]
    Shim(#[from] containerd_shim_protos::ttrpc::Error),
}

/// What to run.
#[derive(Debug, Clone, Default)]
pub struct ExecRequest {
    pub args: Vec<String>,
    /// Extra `KEY=VALUE` entries; override same-key entries of the container.
    pub env: Vec<String>,
    pub cwd: Option<String>,
    pub tty: bool,
    pub stdin: bool,
}

/// The container's process as recorded in its bundle `config.json`.
pub fn base_process(bundle_dir: &Path) -> Result<Process, ExecError> {
    let spec =
        Spec::load(bundle_dir.join("config.json")).map_err(|e| ExecError::Config(e.to_string()))?;
    spec.process()
        .clone()
        .ok_or_else(|| ExecError::Config("config.json has no process".into()))
}

/// Derive the exec process spec (as an `Any`) from the container's process.
pub fn exec_spec(base: &Process, req: &ExecRequest) -> Result<Any, ExecError> {
    if req.args.is_empty() {
        return Err(ExecError::EmptyCommand);
    }
    let mut p = base.clone();
    p.set_args(Some(req.args.clone()));
    p.set_terminal(Some(req.tty));
    if let Some(cwd) = &req.cwd {
        if !cwd.is_empty() {
            p.set_cwd(PathBuf::from(cwd));
        }
    }
    let mut env = p.env().clone().unwrap_or_default();
    for kv in &req.env {
        let key = kv.split('=').next().unwrap_or("");
        env.retain(|e| e.split('=').next() != Some(key));
        env.push(kv.clone());
    }
    p.set_env(Some(env));
    let mut any = Any::new();
    any.type_url = PROCESS_TYPE_URL.into();
    any.value = serde_json::to_vec(&p).map_err(|e| ExecError::Config(e.to_string()))?;
    Ok(any)
}

fn exec_request(id: &str, exec_id: &str, stdio: &Stdio, spec: Any) -> api::ExecProcessRequest {
    let mut r = api::ExecProcessRequest::new();
    r.id = id.into();
    r.exec_id = exec_id.into();
    stdio.fill_exec(&mut r);
    r.spec = Some(spec).into();
    r
}

/// Bounds on an [`exec_sync`]: `timeout == None` waits forever; each captured
/// stream is capped at `capture_limit` bytes.
#[derive(Debug, Clone, Copy)]
pub struct ExecLimits {
    pub timeout: Option<Duration>,
    pub capture_limit: usize,
}

impl Default for ExecLimits {
    fn default() -> Self {
        Self {
            timeout: None,
            capture_limit: DEFAULT_CAPTURE_LIMIT,
        }
    }
}

/// Result of a completed [`exec_sync`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOutcome {
    pub exit_code: i32,
    pub captured: Captured,
    pub timed_out: bool,
}

/// Exit code reported for a timed-out exec (matches the kubelet's convention
/// of 128+SIGKILL).
pub const TIMEOUT_EXIT_CODE: i32 = 137;

/// Run a command in the container and wait for it. `timeout == None` waits
/// forever. On timeout the process is SIGKILLed and `timed_out` is set.
pub async fn exec_sync(
    client: &ShimClient,
    id: &str,
    exec_id: &str,
    base: &Process,
    req: &ExecRequest,
    fifo_dir: &Path,
    limits: ExecLimits,
) -> Result<ExecOutcome, ExecError> {
    let ExecLimits {
        timeout,
        capture_limit,
    } = limits;
    let spec = exec_spec(base, req)?;
    // ExecSync has no stdin; a tty only changes how the guest sets up output.
    let stdio = Stdio::create(fifo_dir, exec_id, false, req.tty)?;
    let result = async {
        let pump = Pump::spawn(
            &stdio,
            Sink::Capture {
                limit: capture_limit,
            },
        )?;
        let run = async {
            client.exec(exec_request(id, exec_id, &stdio, spec)).await?;
            client.start_exec(id, exec_id).await?;
            let wait = client.wait_exec(id, exec_id);
            let (code, timed_out) = match timeout {
                None => (wait.await?.exit_status as i32, false),
                Some(t) => match tokio::time::timeout(t, wait).await {
                    Ok(r) => (r?.exit_status as i32, false),
                    Err(_) => {
                        if let Err(e) = client.kill_exec(id, exec_id, 9).await {
                            log::warn!("exec_sync {id}/{exec_id}: kill after timeout: {e}");
                        }
                        // Reap so the shim can release the process.
                        let _ = tokio::time::timeout(
                            Duration::from_secs(5),
                            client.wait_exec(id, exec_id),
                        )
                        .await;
                        (TIMEOUT_EXIT_CODE, true)
                    }
                },
            };
            if let Err(e) = client.delete_exec(id, exec_id).await {
                log::debug!("exec_sync {id}/{exec_id}: delete: {e}");
            }
            Ok::<_, ExecError>((code, timed_out))
        }
        .await;
        let captured = pump.finish().await.unwrap_or_default();
        run.map(|(exit_code, timed_out)| ExecOutcome {
            exit_code,
            captured,
            timed_out,
        })
    }
    .await;
    stdio.remove();
    result
}

/// A running streaming exec. Output is read through [`ExecSession::pump`]
/// (`subscribe`); drop-in for the SPDY handler.
pub struct ExecSession {
    pub id: String,
    pub exec_id: String,
    pub stdio: Stdio,
    pub pump: Pump,
    pub stdin: Option<StdinWriter>,
}

impl ExecSession {
    /// Register and start the exec process. The caller should `subscribe` to
    /// `pump` *before* output matters; bytes pumped earlier are not replayed
    /// (so subscribe via [`ExecSession::start_with`] when that matters).
    pub async fn start(
        client: &ShimClient,
        id: &str,
        exec_id: &str,
        base: &Process,
        req: &ExecRequest,
        fifo_dir: &Path,
    ) -> Result<Self, ExecError> {
        Self::start_with(client, id, exec_id, base, req, fifo_dir, |_| {}).await
    }

    /// Like [`start`](Self::start), calling `before_start` with the pump after
    /// it exists but before the process runs, so a subscriber misses nothing.
    pub async fn start_with(
        client: &ShimClient,
        id: &str,
        exec_id: &str,
        base: &Process,
        req: &ExecRequest,
        fifo_dir: &Path,
        before_start: impl FnOnce(&Pump),
    ) -> Result<Self, ExecError> {
        let spec = exec_spec(base, req)?;
        let stdio = Stdio::create(fifo_dir, exec_id, req.stdin, req.tty)?;
        let pump = match Pump::spawn(&stdio, Sink::None) {
            Ok(p) => p,
            Err(e) => {
                stdio.remove();
                return Err(e.into());
            }
        };
        before_start(&pump);
        let stdin = match &stdio.stdin {
            Some(p) => StdinWriter::open(p).ok(),
            None => None,
        };
        let started = async {
            client.exec(exec_request(id, exec_id, &stdio, spec)).await?;
            client.start_exec(id, exec_id).await?;
            Ok::<_, ExecError>(())
        }
        .await;
        if let Err(e) = started {
            pump.finish().await;
            stdio.remove();
            return Err(e);
        }
        Ok(Self {
            id: id.into(),
            exec_id: exec_id.into(),
            stdio,
            pump,
            stdin,
        })
    }

    pub async fn wait(&self, client: &ShimClient) -> Result<i32, ExecError> {
        Ok(client.wait_exec(&self.id, &self.exec_id).await?.exit_status as i32)
    }

    pub async fn resize(
        &self,
        client: &ShimClient,
        width: u32,
        height: u32,
    ) -> Result<(), ExecError> {
        Ok(client
            .resize_pty(&self.id, &self.exec_id, width, height)
            .await?)
    }

    pub async fn close_stdin(&self, client: &ShimClient) {
        if let Some(w) = &self.stdin {
            w.close().await;
        }
        if let Err(e) = client.close_io(&self.id, &self.exec_id).await {
            log::debug!("exec {}/{}: close_io: {e}", self.id, self.exec_id);
        }
    }

    /// Tear down after the process exited: delete it in the shim, drain the
    /// output and remove the FIFOs.
    pub async fn finish(self, client: &ShimClient) {
        if let Some(w) = &self.stdin {
            w.close().await;
        }
        if let Err(e) = client.delete_exec(&self.id, &self.exec_id).await {
            log::debug!("exec {}/{}: delete: {e}", self.id, self.exec_id);
        }
        self.pump.finish().await;
        self.stdio.remove();
    }
}

#[cfg(test)]
mod tests;
