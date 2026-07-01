//! `pelagos stop` — send a configurable signal to a running container, wait for exit, SIGKILL if needed.

use super::{check_liveness, read_state, write_state, ContainerStatus};

pub fn cmd_stop(name: &str, time: u64) -> Result<(), Box<dyn std::error::Error>> {
    let mut state = read_state(name).map_err(|_| format!("no container named '{}'", name))?;

    if state.status != ContainerStatus::Running {
        // Already stopped — idempotent like `docker stop`. Fixes #232.
        return Ok(());
    }

    // In detached mode there is a brief window where pid==0: the watcher has
    // written state.json but hasn't yet spawned the container process.  Poll
    // until the real PID appears so we send SIGTERM to the right process.
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

    if !check_liveness(state.pid) {
        // Already dead — update state and return.
        state.status = ContainerStatus::Exited;
        write_state(&state)?;
        return Ok(());
    }

    let pid = state.pid;

    // Use the configured stop signal, or fall back to SIGTERM.
    let stop_sig = state
        .spawn_config
        .as_ref()
        .and_then(|sc| sc.stop_signal.as_deref())
        .map(parse_signal)
        .unwrap_or(libc::SIGTERM);

    let r = unsafe { libc::kill(pid, stop_sig) };
    if r != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            // Process already gone.
        } else {
            return Err(format!("kill({}): {}", pid, err).into());
        }
    }

    // Wait up to `time` seconds for the process to exit cleanly.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(time.max(1));
    while check_liveness(pid) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    // If still alive after the grace period, escalate to SIGKILL.
    if check_liveness(pid) {
        unsafe { libc::kill(pid, libc::SIGKILL) };
        let kill_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while check_liveness(pid) && std::time::Instant::now() < kill_deadline {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    // Belt-and-suspenders: the single-PID signals above miss forked/`setsid`'d
    // descendants and processes reparented to init. SIGKILL the whole container
    // cgroup so nothing survives as an orphan holding a port etc. (#412).
    if let Some(ref cg) = state.cgroup_name {
        pelagos::cgroup::kill_cgroup(cg);
    }

    // Update state to exited.
    state.status = ContainerStatus::Exited;
    write_state(&state)?;

    Ok(())
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
