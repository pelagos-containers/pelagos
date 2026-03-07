//! `pelagos stop` — send SIGTERM to a running container.

use super::{check_liveness, read_state, write_state, ContainerStatus};

pub fn cmd_stop(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut state = read_state(name).map_err(|_| format!("no container named '{}'", name))?;

    if state.status != ContainerStatus::Running {
        return Err(format!(
            "container '{}' is not running (status: {})",
            name, state.status
        )
        .into());
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

    let r = unsafe { libc::kill(state.pid, libc::SIGTERM) };
    if r != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            // Process already gone.
        } else {
            return Err(format!("kill({}): {}", state.pid, err).into());
        }
    }

    // Update state to exited.
    state.status = ContainerStatus::Exited;
    write_state(&state)?;

    Ok(())
}
