//! `pelagos sandbox` — create and manage pod sandboxes (shared namespaces).
//!
//! A sandbox holds a bridge-attached network namespace open via a lightweight
//! "pause" process.  Containers started with `--sandbox <id>` (or the library
//! `Command::with_sandbox(id)` API) join the sandbox's NET/IPC/UTS namespaces
//! instead of creating new ones.
//!
//! ## Subcommands
//!
//! - `pelagos sandbox create [--name <name>]` — allocate a sandbox, print ID
//! - `pelagos sandbox ls`                     — list running sandboxes
//! - `pelagos sandbox rm <id>`                — teardown a sandbox
//! - `pelagos sandbox __pause__ <ns_name>`    — internal: pause process loop

use pelagos::sandbox::{create_sandbox, list_sandboxes, remove_sandbox, SandboxState};

// ── CLI args ─────────────────────────────────────────────────────────────────

#[derive(Debug, clap::Subcommand)]
pub enum SandboxCmd {
    /// Create a new pod sandbox (allocates network namespace, starts pause process)
    Create {
        /// Optional human-readable name for the sandbox
        #[clap(long)]
        name: Option<String>,
    },
    /// List running sandboxes
    Ls {
        /// Output as JSON
        #[clap(long)]
        json: bool,
    },
    /// Remove a sandbox (stops pause process, cleans up netns)
    Rm {
        /// Sandbox ID
        id: String,
    },
    /// Internal: pause process (holds namespaces open) — do not call directly
    #[clap(hide = true, name = "__pause__")]
    Pause {
        /// Network namespace name
        ns_name: String,
        /// Don't unshare IPC — the pod shares the host IPC namespace (hostIPC: true).
        #[clap(long = "host-ipc")]
        host_ipc: bool,
        /// Don't join a netns — the pod shares the host network namespace
        /// (hostNetwork: true). `ns_name` is ignored for the network join.
        #[clap(long = "host-net")]
        host_net: bool,
        /// Become PID 1 of a shared pod PID namespace (shareProcessNamespace:
        /// true) by unsharing PID and forking an init child.
        #[clap(long = "pod-pid")]
        pod_pid: bool,
    },
}

// ── Dispatch ─────────────────────────────────────────────────────────────────

pub fn cmd_sandbox(cmd: SandboxCmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        SandboxCmd::Create { name } => cmd_sandbox_create(name.as_deref()),
        SandboxCmd::Ls { json } => cmd_sandbox_ls(json),
        SandboxCmd::Rm { id } => cmd_sandbox_rm(&id),
        SandboxCmd::Pause {
            ns_name,
            host_ipc,
            host_net,
            pod_pid,
        } => cmd_sandbox_pause(&ns_name, host_ipc, host_net, pod_pid),
    }
}

// ── sandbox create ────────────────────────────────────────────────────────────

fn cmd_sandbox_create(name: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let state = create_sandbox(name)?;
    // Print the sandbox ID to stdout — callers capture this.
    println!("{}", state.id);
    Ok(())
}

// ── sandbox ls ───────────────────────────────────────────────────────────────

fn cmd_sandbox_ls(json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let sandboxes = list_sandboxes();

    if json {
        let alive: Vec<&SandboxState> = sandboxes.iter().filter(|s| s.is_alive()).collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&alive)
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?
        );
        return Ok(());
    }

    // Table output.
    if sandboxes.is_empty() {
        return Ok(());
    }

    println!(
        "{:<18} {:<20} {:<8} {:<16} NS_NAME",
        "ID", "NAME", "STATUS", "IP"
    );
    for s in &sandboxes {
        let status = if s.is_alive() { "running" } else { "dead" };
        let name = s.name.as_deref().unwrap_or("-");
        println!(
            "{:<18} {:<20} {:<8} {:<16} {}",
            s.id, name, status, s.container_ip, s.ns_name
        );
    }
    Ok(())
}

// ── sandbox rm ───────────────────────────────────────────────────────────────

fn cmd_sandbox_rm(id: &str) -> Result<(), Box<dyn std::error::Error>> {
    remove_sandbox(id)?;
    Ok(())
}

// ── sandbox __pause__ ────────────────────────────────────────────────────────

/// Internal pause process.  Joins the sandbox's network namespace and then
/// creates fresh IPC and UTS namespaces, then sleeps forever (SIGTERM exits).
///
/// The pause process purpose is to keep the IPC and UTS namespaces alive even
/// after containers in the sandbox exit.  The NET namespace is the named netns
/// `/run/netns/<ns_name>` which persists independently.
///
/// When `host_ipc` is set (pod `hostIPC: true`, `namespace_options.ipc == NODE`)
/// the pause does **not** unshare IPC, so `/proc/<pause>/ns/ipc` is the host IPC
/// namespace.  Containers join that namespace via `with_sandbox()`, making host
/// System V IPC objects visible inside the pod (CRI conformance #386 / #352).
///
/// When `host_net` is set (pod `hostNetwork: true`, `namespace_options.network ==
/// NODE`) the pause does **not** `setns` into a netns — it stays in the host
/// network namespace, and `with_sandbox()` skips the NET join so containers stay
/// in the host netns too (CRI conformance #394 / #352).
///
/// When `pod_pid` is set (pod `shareProcessNamespace: true`,
/// `namespace_options.pid == POD`) the pause becomes the PID-1 init of a shared
/// pod PID namespace: it `unshare(CLONE_NEWPID)`s and forks, the **child** is
/// PID 1 (reaps zombies, exits on SIGTERM) and the **parent** supervises it
/// (so the systemd MainPID stays stable, preserving #336 restart-survival).
/// Containers join `/proc/<child>/ns/pid` via `with_sandbox()` (#398 / #352).
fn cmd_sandbox_pause(
    ns_name: &str,
    host_ipc: bool,
    host_net: bool,
    pod_pid: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // Join the named network namespace — unless this is a hostNetwork pod, in
    // which case there is no named netns and the pause stays in the host netns.
    if !host_net {
        let netns_path = format!("/run/netns/{}", ns_name);
        let netns_c = std::ffi::CString::new(netns_path.as_bytes())
            .map_err(|e| format!("invalid netns path: {}", e))?;

        let fd = unsafe { libc::open(netns_c.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(format!(
                "open netns '{}': {}",
                netns_path,
                std::io::Error::last_os_error()
            )
            .into());
        }
        let rc = unsafe { libc::setns(fd, libc::CLONE_NEWNET) };
        unsafe { libc::close(fd) };
        if rc != 0 {
            return Err(format!(
                "setns netns '{}': {}",
                netns_path,
                std::io::Error::last_os_error()
            )
            .into());
        }
    }

    // Unshare UTS (pod hostname) and — unless the pod requested host IPC — IPC,
    // so containers in the sandbox share a private IPC namespace.  For a hostIPC
    // pod we skip the IPC unshare so the pause (and every container joining it)
    // stays in the host IPC namespace.
    let unshare_flags = if host_ipc {
        libc::CLONE_NEWUTS
    } else {
        libc::CLONE_NEWIPC | libc::CLONE_NEWUTS
    };
    let rc = unsafe { libc::unshare(unshare_flags) };
    if rc != 0 {
        return Err(format!("unshare UTS/IPC: {}", std::io::Error::last_os_error()).into());
    }

    // Shared pod PID namespace (shareProcessNamespace). A process can't put
    // *itself* into a new PID namespace — unshare(CLONE_NEWPID) only places future
    // children there — so we unshare and fork: the child is PID 1 of the pod PID
    // namespace, the parent supervises it (keeping the spawned/MainPID stable).
    if pod_pid {
        let rc = unsafe { libc::unshare(libc::CLONE_NEWPID) };
        if rc != 0 {
            return Err(format!("unshare PID: {}", std::io::Error::last_os_error()).into());
        }
        let child = unsafe { libc::fork() };
        if child < 0 {
            return Err(format!("fork pod-pid init: {}", std::io::Error::last_os_error()).into());
        }
        if child == 0 {
            // ── Child: PID 1 of the pod PID namespace (the real pause/init) ──
            // run_pid1_init_loop() diverges (it is the init's forever-loop).
            run_pid1_init_loop();
        }
        // ── Parent: supervise the PID-1 child; exit when it does so the systemd
        // unit / leaked child cleanly finishes (teardown SIGKILLs the cgroup,
        // which includes the child; killing PID 1 tears the pod PID ns down). ──
        let mut status: libc::c_int = 0;
        loop {
            let r = unsafe { libc::waitpid(child, &mut status, 0) };
            if r == child
                || (r < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR))
            {
                break;
            }
        }
        return Ok(());
    }

    // Block until terminated. `pause()` returns on ANY caught signal — a stray
    // SIGCHLD/SIGURG/SIGWINCH or a handler the runtime installs — so calling it
    // once would let the first such signal exit the pause, orphaning the
    // sandbox's IPC/UTS namespaces and tripping the phantom-sandbox reaper, which
    // then yanks still-live containers out from under the kubelet (root cause of
    // #351; surfaced as flaky AppArmor/lifecycle failures in #353). Loop so only a
    // real termination signal (SIGTERM from `pelagos sandbox rm` / systemd
    // KillMode, which terminates the process regardless) ends the pause.
    loop {
        unsafe { libc::pause() };
    }
}

/// PID-1 init loop for a shared pod PID namespace (#398). Runs in the pause's
/// child after `unshare(CLONE_NEWPID)` + `fork`. PID 1 has two init duties:
///   - **Reap** orphaned zombies (pod container processes are reparented here on
///     exit) — otherwise they accumulate in the pod PID namespace.
///   - **Exit on SIGTERM** — the kernel ignores default-action signals for PID 1,
///     so without a handler `systemctl stop` could only SIGKILL it after the stop
///     timeout. A SIGTERM handler makes teardown prompt (and, since PID 1 exiting
///     tears the namespace down, also kills any lingering container processes).
fn run_pid1_init_loop() -> ! {
    use nix::sys::signal::{signal, SigHandler, Signal};

    extern "C" fn on_term(_: libc::c_int) {
        // PID 1 exiting collapses the pod PID namespace.
        unsafe { libc::_exit(0) };
    }
    // A no-op SIGCHLD handler ensures `pause()` returns so we can reap.
    extern "C" fn on_chld(_: libc::c_int) {}

    unsafe {
        let _ = signal(Signal::SIGTERM, SigHandler::Handler(on_term));
        let _ = signal(Signal::SIGCHLD, SigHandler::Handler(on_chld));
    }

    loop {
        // Reap every dead child without blocking.
        let mut status: libc::c_int = 0;
        while unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) } > 0 {}
        // Sleep until the next signal (SIGCHLD to reap, SIGTERM to exit).
        unsafe { libc::pause() };
    }
}
