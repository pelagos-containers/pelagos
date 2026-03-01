//! Rootless cgroup v2 delegation support.
//!
//! On systemd-based systems with cgroup v2, each user gets a delegated cgroup
//! subtree (e.g. `/sys/fs/cgroup/user.slice/user-1000.slice/...`) where they
//! can create sub-cgroups and write limits directly — no root required.
//!
//! This module provides the same lifecycle as [`crate::cgroup`] but writes
//! directly to the cgroupfs files instead of going through `cgroups-rs`.

use crate::cgroup::{CgroupConfig, ResourceStats};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// A rootless cgroup created under the user's delegated subtree.
pub struct RootlessCgroup {
    /// Absolute path, e.g. `/sys/fs/cgroup/user.slice/.../remora-<pid>`.
    pub path: PathBuf,
}

/// Read `/proc/self/cgroup` and return the absolute cgroupfs path for the
/// current process's v2 cgroup.
///
/// Looks for the unified v2 entry (`0::<path>`) and returns
/// `/sys/fs/cgroup/<path>` (with leading `/` stripped from `<path>`).
pub fn self_cgroup_path() -> io::Result<PathBuf> {
    parse_cgroup_path(&fs::read_to_string("/proc/self/cgroup")?)
}

/// Parse a `/proc/self/cgroup`-formatted string and extract the v2 path.
fn parse_cgroup_path(contents: &str) -> io::Result<PathBuf> {
    for line in contents.lines() {
        // v2 unified hierarchy: "0::<relative-path>"
        if let Some(rest) = line.strip_prefix("0::") {
            let rel = rest.trim_start_matches('/');
            return Ok(PathBuf::from("/sys/fs/cgroup").join(rel));
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no cgroup v2 entry in /proc/self/cgroup",
    ))
}

/// Check whether cgroup v2 delegation is available for the current user.
///
/// Returns `true` when:
/// 1. `/proc/self/cgroup` has a v2 entry
/// 2. The corresponding directory exists and is writable
/// 3. At least one of `cpu`, `memory`, or `pids` is listed in `cgroup.controllers`
pub fn is_delegation_available() -> bool {
    let path = match self_cgroup_path() {
        Ok(p) => p,
        Err(_) => return false,
    };

    // Check directory exists and we can write to it (try reading controllers).
    let controllers_path = path.join("cgroup.controllers");
    let controllers = match fs::read_to_string(&controllers_path) {
        Ok(c) => c,
        Err(_) => return false,
    };

    // Need at least one useful controller.
    controllers.contains("cpu") || controllers.contains("memory") || controllers.contains("pids")
}

/// Read the set of available controllers from the parent cgroup's
/// `cgroup.controllers` file.
fn available_controllers(parent: &Path) -> io::Result<String> {
    fs::read_to_string(parent.join("cgroup.controllers"))
}

/// Create a sub-cgroup under the user's delegated subtree, apply limits, and
/// move `child_pid` into it.
pub fn setup_rootless_cgroup(cfg: &CgroupConfig, child_pid: u32) -> io::Result<RootlessCgroup> {
    let parent = self_cgroup_path()?;
    let controllers = available_controllers(&parent)?;

    let name = format!("remora-{}", child_pid);
    let cg_path = parent.join(&name);

    fs::create_dir(&cg_path)?;

    // Enable controllers in the subtree one at a time (write to parent's
    // cgroup.subtree_control). Writing all at once can fail atomically if any
    // single controller can't be enabled (e.g. "no internal process" constraint).
    let subtree_control = parent.join("cgroup.subtree_control");
    for ctrl in ["memory", "cpu", "pids"] {
        if controllers.contains(ctrl) {
            let token = format!("+{}", ctrl);
            if let Err(e) = fs::write(&subtree_control, &token) {
                log::debug!("cgroup.subtree_control {}: {}", token, e);
            }
        }
    }

    // Check which controllers are actually available in the sub-cgroup.
    let child_controllers = available_controllers(&cg_path).unwrap_or_default();

    // Apply limits only for controllers that are actually present.
    if child_controllers.contains("memory") {
        if let Some(bytes) = cfg.memory_limit {
            write_limit(&cg_path, "memory.max", &bytes.to_string())?;
        }
        if let Some(swap) = cfg.memory_swap {
            write_limit(&cg_path, "memory.swap.max", &swap.to_string())?;
        }
        if let Some(res) = cfg.memory_reservation {
            write_limit(&cg_path, "memory.low", &res.to_string())?;
        }
    } else if cfg.memory_limit.is_some()
        || cfg.memory_swap.is_some()
        || cfg.memory_reservation.is_some()
    {
        log::warn!("memory controller not available in sub-cgroup, skipping memory limits");
    }

    if child_controllers.contains("cpu") {
        if let Some((quota_us, period_us)) = cfg.cpu_quota {
            write_limit(&cg_path, "cpu.max", &format!("{} {}", quota_us, period_us))?;
        }
        if let Some(shares) = cfg.cpu_shares {
            write_limit(&cg_path, "cpu.weight", &shares.to_string())?;
        }
    } else if cfg.cpu_quota.is_some() || cfg.cpu_shares.is_some() {
        log::warn!("cpu controller not available in sub-cgroup, skipping cpu limits");
    }

    // cpuset: write directly to cpuset.cpus / cpuset.mems if the files exist.
    if let Some(ref cpus) = cfg.cpuset_cpus {
        let knob = cg_path.join("cpuset.cpus");
        if knob.exists() {
            if let Err(e) = fs::write(&knob, cpus) {
                log::warn!("cpuset.cpus={} failed (non-fatal): {}", cpus, e);
            }
        }
    }
    if let Some(ref mems) = cfg.cpuset_mems {
        let knob = cg_path.join("cpuset.mems");
        if knob.exists() {
            if let Err(e) = fs::write(&knob, mems) {
                log::warn!("cpuset.mems={} failed (non-fatal): {}", mems, e);
            }
        }
    }

    if let Some(max) = cfg.pids_limit {
        if child_controllers.contains("pids") {
            write_limit(&cg_path, "pids.max", &max.to_string())?;
        } else {
            log::warn!("pids controller not available in sub-cgroup, skipping pids limit");
        }
    }

    // Move child into the cgroup.
    fs::write(cg_path.join("cgroup.procs"), child_pid.to_string())?;

    log::info!("rootless cgroup created: {}", cg_path.display());

    Ok(RootlessCgroup { path: cg_path })
}

/// Write a value to a cgroup knob file, with a descriptive error on failure.
fn write_limit(cg_path: &Path, knob: &str, value: &str) -> io::Result<()> {
    fs::write(cg_path.join(knob), value).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("writing {} to {}/{}: {}", value, cg_path.display(), knob, e),
        )
    })
}

/// Read resource usage from a rootless cgroup.
pub fn read_rootless_stats(cg: &RootlessCgroup) -> io::Result<ResourceStats> {
    let mut stats = ResourceStats::default();

    // memory.current
    if let Ok(raw) = fs::read_to_string(cg.path.join("memory.current")) {
        if let Ok(bytes) = raw.trim().parse::<u64>() {
            stats.memory_current_bytes = bytes;
        }
    }

    // cpu.stat → usage_usec
    if let Ok(raw) = fs::read_to_string(cg.path.join("cpu.stat")) {
        for line in raw.lines() {
            if let Some(rest) = line.strip_prefix("usage_usec ") {
                if let Ok(usec) = rest.trim().parse::<u64>() {
                    stats.cpu_usage_ns = usec.saturating_mul(1000);
                }
                break;
            }
        }
    }

    // pids.current
    if let Ok(raw) = fs::read_to_string(cg.path.join("pids.current")) {
        if let Ok(n) = raw.trim().parse::<u64>() {
            stats.pids_current = n;
        }
    }

    Ok(stats)
}

/// Remove the sub-cgroup directory. Only succeeds when all tasks have exited.
/// Logs a warning on failure (non-fatal).
pub fn teardown_rootless_cgroup(cg: &RootlessCgroup) {
    if let Err(e) = fs::remove_dir(&cg.path) {
        log::warn!(
            "rootless cgroup remove {} failed (non-fatal): {}",
            cg.path.display(),
            e
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_cgroup_path() {
        let input = "0::/user.slice/user-1000.slice/session-2.scope\n";
        let path = parse_cgroup_path(input).unwrap();
        assert_eq!(
            path,
            PathBuf::from("/sys/fs/cgroup/user.slice/user-1000.slice/session-2.scope")
        );
    }

    #[test]
    fn test_parse_cgroup_path_root() {
        // Container or minimal cgroup setup: "0::/"
        let input = "0::/\n";
        let path = parse_cgroup_path(input).unwrap();
        assert_eq!(path, PathBuf::from("/sys/fs/cgroup"));
    }

    #[test]
    fn test_parse_cgroup_path_no_v2() {
        let input = "1:name=systemd:/user.slice\n";
        let err = parse_cgroup_path(input).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn test_self_cgroup_path() {
        // This test runs on the real system; skip if no v2.
        if let Ok(path) = self_cgroup_path() {
            assert!(
                path.starts_with("/sys/fs/cgroup/"),
                "expected /sys/fs/cgroup/ prefix, got: {}",
                path.display()
            );
            assert!(
                path.exists(),
                "cgroup path does not exist: {}",
                path.display()
            );
        }
    }
}
