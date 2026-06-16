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
    },
}

// ── Dispatch ─────────────────────────────────────────────────────────────────

pub fn cmd_sandbox(cmd: SandboxCmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        SandboxCmd::Create { name } => cmd_sandbox_create(name.as_deref()),
        SandboxCmd::Ls { json } => cmd_sandbox_ls(json),
        SandboxCmd::Rm { id } => cmd_sandbox_rm(&id),
        SandboxCmd::Pause { ns_name } => cmd_sandbox_pause(&ns_name),
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
fn cmd_sandbox_pause(ns_name: &str) -> Result<(), Box<dyn std::error::Error>> {
    // Join the named network namespace.
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

    // Unshare IPC and UTS namespaces so containers in the sandbox share them.
    let rc = unsafe { libc::unshare(libc::CLONE_NEWIPC | libc::CLONE_NEWUTS) };
    if rc != 0 {
        return Err(format!("unshare IPC/UTS: {}", std::io::Error::last_os_error()).into());
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
