//! `pelagos stop` — graceful shutdown with cgroup kill as the authoritative termination.
//!
//! Kill order (#459):
//!   1. SIGTERM (or container-configured stop signal) — courtesy, allows graceful shutdown.
//!      Skipped when `--time 0` is passed (immediate kill path).
//!   2. Cgroup kill — unconditional after the grace period.  Kills the entire cgroup
//!      subtree: main pid, forked descendants, setsid'd processes, reparented orphans.
//!      Port release happens here: sockets are closed when the process exits (transitions
//!      to zombie), not when the zombie is reaped.
//!   3. Direct SIGKILL to the recorded PID — belt-and-suspenders for the rare case where
//!      cgroup_name is absent from state.json.
//!   4. Wait up to 5 s for `is_live_process` to return false.  Unlike `kill(pid, 0)`,
//!      `is_live_process` reads `/proc/{pid}/status` and returns false for zombies,
//!      so we do not spin against a process that has already released its ports.

use super::{check_liveness, read_state, write_state, ContainerStatus};

pub fn cmd_stop(name: &str, time: u64) -> Result<(), Box<dyn std::error::Error>> {
    let mut state = read_state(name).map_err(|_| format!("no container named '{}'", name))?;

    if state.status != ContainerStatus::Running {
        return Ok(());
    }

    // In detached mode there is a brief window where pid==0: the watcher has
    // written state.json but hasn't yet spawned the container process.  Poll
    // until the real PID appears so we send the stop signal to the right process.
    if state.pid == 0 && check_liveness(state.watcher_pid) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while state.pid == 0
            && state.status == ContainerStatus::Running
            && check_liveness(state.watcher_pid)
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(50));
            if let Ok(s) = read_state(name) {
                state = s;
            }
        }
    }

    let pid = state.pid;

    // Phase 1: Graceful shutdown via SIGTERM (or image-configured stop signal).
    // Skipped when time=0 (caller wants an immediate kill).
    if time > 0 && pid > 1 {
        let stop_sig = state
            .spawn_config
            .as_ref()
            .and_then(|sc| sc.stop_signal.as_deref())
            .map(parse_signal)
            .unwrap_or(libc::SIGTERM);
        unsafe { libc::kill(pid, stop_sig) };

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(time);
        while is_live_process(pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    // Phase 2: Cgroup kill — the authoritative termination (#459).
    // Kills the full cgroup subtree regardless of what the PID poll above found.
    // Done unconditionally: even if SIGTERM appeared to work, we want to catch
    // any descendants that outlived the main process.
    if let Some(ref cg) = state.cgroup_name {
        pelagos::cgroup::kill_cgroup(cg);
    }

    // Phase 3: Direct SIGKILL to the recorded PID.
    // Belt-and-suspenders for containers whose state.json has no cgroup_name
    // (older containers, non-cgroup builds, edge cases).
    if pid > 1 {
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }

    // Phase 4: Wait for confirmed death.
    // `is_live_process` checks /proc/{pid}/status and returns false for zombies,
    // so this loop exits as soon as the process has truly exited — not just when
    // the zombie entry is reaped.  5 s is ample; cgroup.kill is near-instantaneous.
    let kill_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while is_live_process(pid) && std::time::Instant::now() < kill_deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    state.status = ContainerStatus::Exited;
    write_state(&state)?;

    Ok(())
}

/// True only if the process exists AND is not a zombie.
///
/// Reads `/proc/{pid}/status` and checks the `State:` field.  Returns false
/// for zombies ('Z') and for any process whose /proc entry has disappeared.
/// This avoids the false-positive from `kill(pid, 0)`, which returns 0 for
/// zombies (they have not yet been reaped but have already released all their
/// resources including sockets and port bindings).
fn is_live_process(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    match std::fs::read_to_string(format!("/proc/{}/status", pid)) {
        Err(_) => false,
        Ok(s) => s
            .lines()
            .find(|l| l.starts_with("State:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .map(|c| !c.starts_with('Z'))
            .unwrap_or(false),
    }
}

/// Parse a signal name ("SIGTERM", "TERM", "15") into a libc signal number.
/// Unknown signals fall back to SIGTERM.
fn parse_signal(s: &str) -> libc::c_int {
    let upper = s.trim().to_uppercase();
    let name = upper.strip_prefix("SIG").unwrap_or(&upper);
    match name {
        "HUP" => libc::SIGHUP,
        "INT" => libc::SIGINT,
        "QUIT" => libc::SIGQUIT,
        "ILL" => libc::SIGILL,
        "TRAP" => libc::SIGTRAP,
        "ABRT" | "IOT" => libc::SIGABRT,
        "BUS" => libc::SIGBUS,
        "FPE" => libc::SIGFPE,
        "KILL" => libc::SIGKILL,
        "USR1" => libc::SIGUSR1,
        "SEGV" => libc::SIGSEGV,
        "USR2" => libc::SIGUSR2,
        "PIPE" => libc::SIGPIPE,
        "ALRM" => libc::SIGALRM,
        "TERM" => libc::SIGTERM,
        "CHLD" => libc::SIGCHLD,
        "CONT" => libc::SIGCONT,
        "STOP" => libc::SIGSTOP,
        "TSTP" => libc::SIGTSTP,
        "TTIN" => libc::SIGTTIN,
        "TTOU" => libc::SIGTTOU,
        "URG" => libc::SIGURG,
        "XCPU" => libc::SIGXCPU,
        "XFSZ" => libc::SIGXFSZ,
        "WINCH" => libc::SIGWINCH,
        _ => s.parse::<libc::c_int>().unwrap_or(libc::SIGTERM),
    }
}
