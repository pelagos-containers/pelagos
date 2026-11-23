//! Runtime backends: the seam between the CRI server and whatever actually
//! runs a pod's containers.
//!
//! Today there is one backend, [`NativeBackend`], which drives the `pelagos`
//! CLI exactly as `runtime.rs` always did. The seam exists so a containerd
//! shim v2 backend (Kata and friends, epic #574) can be selected per pod via
//! the CRI `runtime_handler` / Kubernetes `RuntimeClass` without touching the
//! native path.
//!
//! Scope of this first extraction: the operations that spawn, stop and remove
//! container and (native) sandbox processes through the CLI. CNI and pause
//! handling stay in `runtime.rs` for now; they are native-backend internals
//! that move when a second backend actually needs a different shape. Expect
//! these signatures to widen when the shim backend lands.
//!
//! Dispatch is by enum rather than `dyn` so we need neither `async_trait` nor
//! boxed futures; adding a backend means adding a variant.

use tonic::Status;

use crate::invoke::{run_pelagos, CmdOutput};
use crate::scope;

/// The handler name kubelet sends when no `RuntimeClass` is requested.
pub const DEFAULT_HANDLER: &str = "";
/// Explicit name for the native backend; equivalent to [`DEFAULT_HANDLER`].
pub const NATIVE_HANDLER: &str = "pelagos";

/// Operations a backend must provide. See the module docs for scope.
pub trait RuntimeBackend {
    /// Start a detached container. `args` is the full `pelagos run` argument
    /// vector (without the binary itself).
    async fn start_container(
        &self,
        pelagos_name: &str,
        args: &[&str],
        envs: &[(String, String)],
    ) -> std::io::Result<CmdOutput>;

    /// Kill a container (`pelagos stop`, the single kill path, #459) without
    /// touching its supervisor unit. Best effort.
    async fn kill_container(&self, pelagos_name: &str);

    /// Stop a container and reap its supervisor scope. Best effort.
    async fn stop_container(&self, pelagos_name: &str);

    /// Remove a container's state and supervisor scope. Best effort.
    async fn remove_container(&self, pelagos_name: &str);

    /// Create a native pelagos sandbox (no-CNI path); stdout is the sandbox id.
    async fn create_sandbox(&self, uid: &str) -> std::io::Result<CmdOutput>;

    /// Remove a native pelagos sandbox. Best effort.
    async fn remove_sandbox(&self, sandbox_id: &str);
}

/// Program and arguments used to launch `pelagos run --detach`.
///
/// Under systemd the run is wrapped in a transient scope under `pelagos.slice`
/// so the watcher it forks lives outside `pelagos-cri.service` and survives a
/// runtime restart (#336). Otherwise pelagos is invoked directly.
pub(crate) fn start_invocation(
    systemd: bool,
    bin: &str,
    pelagos_name: &str,
    args: &[&str],
    envs: &[(String, String)],
) -> (String, Vec<String>) {
    if systemd {
        let unit = scope::container_unit(pelagos_name);
        let mut argv = scope::build_scope_argv(&unit, bin, args, envs);
        let program = argv.remove(0);
        (program, argv)
    } else {
        (
            bin.to_string(),
            args.iter().map(|s| s.to_string()).collect(),
        )
    }
}

/// The original behavior: shell out to the `pelagos` binary.
#[derive(Clone, Debug)]
pub struct NativeBackend {
    bin: String,
}

impl NativeBackend {
    pub fn new(bin: String) -> Self {
        Self { bin }
    }
}

impl RuntimeBackend for NativeBackend {
    async fn start_container(
        &self,
        pelagos_name: &str,
        args: &[&str],
        envs: &[(String, String)],
    ) -> std::io::Result<CmdOutput> {
        let systemd = scope::systemd_available();
        if systemd {
            // Clear any stale unit for this name before relaunching.
            scope::stop_unit(&scope::container_unit(pelagos_name)).await;
        }
        let (program, argv) = start_invocation(systemd, &self.bin, pelagos_name, args, envs);
        let argv_ref: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
        run_pelagos(&program, &argv_ref).await
    }

    async fn kill_container(&self, pelagos_name: &str) {
        let _ = run_pelagos(&self.bin, &["stop", pelagos_name]).await;
    }

    async fn stop_container(&self, pelagos_name: &str) {
        self.kill_container(pelagos_name).await;
        // The watcher has exited, so the transient scope is now empty;
        // --collect reaps it, but stop it explicitly for determinism (#336).
        if scope::systemd_available() {
            scope::stop_unit(&scope::container_unit(pelagos_name)).await;
        }
    }

    async fn remove_container(&self, pelagos_name: &str) {
        let _ = run_pelagos(&self.bin, &["rm", pelagos_name]).await;
        if scope::systemd_available() {
            scope::stop_unit(&scope::container_unit(pelagos_name)).await;
        }
    }

    async fn create_sandbox(&self, uid: &str) -> std::io::Result<CmdOutput> {
        run_pelagos(&self.bin, &["sandbox", "create", "--name", uid]).await
    }

    async fn remove_sandbox(&self, sandbox_id: &str) {
        let _ = run_pelagos(&self.bin, &["sandbox", "rm", sandbox_id]).await;
    }
}

/// A concrete backend. Enum dispatch; see module docs.
#[derive(Clone, Debug)]
pub enum Backend {
    Native(NativeBackend),
}

macro_rules! dispatch {
    ($self:ident, $method:ident($($arg:expr),*)) => {
        match $self {
            Backend::Native(b) => b.$method($($arg),*).await,
        }
    };
}

impl RuntimeBackend for Backend {
    async fn start_container(
        &self,
        pelagos_name: &str,
        args: &[&str],
        envs: &[(String, String)],
    ) -> std::io::Result<CmdOutput> {
        dispatch!(self, start_container(pelagos_name, args, envs))
    }
    async fn kill_container(&self, pelagos_name: &str) {
        dispatch!(self, kill_container(pelagos_name))
    }
    async fn stop_container(&self, pelagos_name: &str) {
        dispatch!(self, stop_container(pelagos_name))
    }
    async fn remove_container(&self, pelagos_name: &str) {
        dispatch!(self, remove_container(pelagos_name))
    }
    async fn create_sandbox(&self, uid: &str) -> std::io::Result<CmdOutput> {
        dispatch!(self, create_sandbox(uid))
    }
    async fn remove_sandbox(&self, sandbox_id: &str) {
        dispatch!(self, remove_sandbox(sandbox_id))
    }
}

/// Maps a CRI `runtime_handler` string to a [`Backend`].
#[derive(Clone, Debug)]
pub struct BackendRegistry {
    native: Backend,
}

impl BackendRegistry {
    pub fn new(pelagos_bin: String) -> Self {
        Self {
            native: Backend::Native(NativeBackend::new(pelagos_bin)),
        }
    }

    /// Resolve a handler name. Unknown handlers are rejected rather than
    /// silently run natively, matching containerd / CRI-O.
    // `Status` is tonic's standard (large) error type; every CRI call site
    // returns it directly, so boxing here would only add conversions.
    #[allow(clippy::result_large_err)]
    pub fn resolve(&self, handler: &str) -> Result<&Backend, Status> {
        match handler {
            DEFAULT_HANDLER | NATIVE_HANDLER => Ok(&self.native),
            other => Err(Status::invalid_argument(format!(
                "unknown runtime handler {:?}; known handlers: {:?}",
                other,
                self.handler_names()
            ))),
        }
    }

    /// The backend used when a sandbox's recorded handler can no longer be
    /// resolved (e.g. after reconfiguration) or its sandbox is gone. Cleanup
    /// paths must never fail on this, so they fall back to native.
    pub fn default_backend(&self) -> &Backend {
        &self.native
    }

    /// Handler names to advertise in the CRI `Status` response.
    pub fn handler_names(&self) -> Vec<String> {
        vec![DEFAULT_HANDLER.to_string(), NATIVE_HANDLER.to_string()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg() -> BackendRegistry {
        BackendRegistry::new("pelagos".to_string())
    }

    #[test]
    fn default_and_native_handlers_resolve() {
        assert!(reg().resolve("").is_ok());
        assert!(reg().resolve("pelagos").is_ok());
    }

    #[test]
    fn unknown_handler_is_invalid_argument() {
        let err = reg().resolve("kata").unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains("kata"));
    }

    #[test]
    fn handler_names_advertise_default_and_native() {
        assert_eq!(reg().handler_names(), vec!["", "pelagos"]);
    }

    #[test]
    fn start_invocation_direct_when_not_systemd() {
        let (prog, argv) = start_invocation(
            false,
            "/usr/bin/pelagos",
            "c1",
            &["run", "--detach", "--name", "c1"],
            &[],
        );
        assert_eq!(prog, "/usr/bin/pelagos");
        assert_eq!(argv, vec!["run", "--detach", "--name", "c1"]);
    }

    #[test]
    fn start_invocation_wraps_in_scope_under_systemd() {
        let (prog, argv) = start_invocation(
            true,
            "/usr/bin/pelagos",
            "c1",
            &["run", "--detach"],
            &[("K".to_string(), "V".to_string())],
        );
        assert_eq!(prog, "systemd-run");
        assert!(argv.iter().any(|a| a == "--scope"));
        assert!(argv
            .iter()
            .any(|a| a == &format!("--unit={}", scope::container_unit("c1"))));
        // Wrapped command follows the `--` separator, intact and in order.
        let sep = argv.iter().position(|a| a == "--").expect("-- separator");
        assert_eq!(&argv[sep + 1..], ["/usr/bin/pelagos", "run", "--detach"]);
    }
}
