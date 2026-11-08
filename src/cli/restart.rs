//! `pelagos restart` — stop then start a container.
//!
//! For a running container: send SIGTERM, wait up to `--time` seconds for a
//! clean exit, send SIGKILL if it does not stop in time, then re-run it with
//! its saved SpawnConfig (detached).
//!
//! For an exited container: equivalent to `pelagos start`.

use super::start::cmd_start;
use super::stop::cmd_stop;
use super::{read_state, ContainerStatus};

pub fn cmd_restart(name: &str, time: u64) -> Result<(), Box<dyn std::error::Error>> {
    let state = read_state(name).map_err(|_| format!("no container named '{}'", name))?;

    // Fail fast, before stopping anything (#549): a container with no
    // SpawnConfig can never be started back up by `cmd_start`, so checking
    // only on the start half left it stopped with no way back. Check while
    // it's still running so an unrestartable container is reported without
    // ever being torn down.
    if state.spawn_config.is_none() {
        return Err(format!(
            "container '{}' has no spawn config — refusing to restart. \
             This container has no saved parameters to restart from (either \
             its state predates `pelagos start`, or it was created by a tool \
             that doesn't populate one). It has NOT been stopped; recreate \
             it with its original `pelagos run`/`pelagos compose up` command \
             instead.",
            name
        )
        .into());
    }

    if state.status == ContainerStatus::Running {
        // SIGTERM + wait + SIGKILL if needed — all handled by cmd_stop.
        cmd_stop(name, time)?;
    }

    cmd_start(&[name.to_string()], false, None)
}
