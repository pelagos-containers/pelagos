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

    if state.status == ContainerStatus::Running {
        // SIGTERM + wait + SIGKILL if needed — all handled by cmd_stop.
        cmd_stop(name, time)?;
    }

    cmd_start(&[name.to_string()], false, None)
}
