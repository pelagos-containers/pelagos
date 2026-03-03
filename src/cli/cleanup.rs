//! `pelagos cleanup` — remove stale network namespaces, overlay dirs, and DNS temp dirs
//! left behind by containers that exited without proper teardown.

use std::path::Path;

/// Scan for and remove orphaned resources whose owning PID is dead.
///
/// Returns the number of stale entries cleaned.
pub fn cmd_cleanup() -> Result<(), Box<dyn std::error::Error>> {
    let mut cleaned = 0u32;

    // 1. Stale named network namespaces: /run/netns/rem-{pid}-{n}
    cleaned += cleanup_netns()?;

    // 2. Stale overlay dirs: /run/pelagos/overlay-{pid}-{n}/
    cleaned += cleanup_dir_pattern("/run/pelagos", "overlay-")?;

    // 3. Stale DNS temp dirs: /run/pelagos/dns-{pid}-{n}/
    cleaned += cleanup_dir_pattern("/run/pelagos", "dns-")?;

    // 4. Stale hosts temp dirs: /run/pelagos/hosts-{pid}-{n}/
    cleaned += cleanup_dir_pattern("/run/pelagos", "hosts-")?;

    if cleaned == 0 {
        println!("No stale resources found.");
    } else {
        println!("Cleaned {} stale resource(s).", cleaned);
    }
    Ok(())
}

/// Remove orphaned `/run/netns/rem-*` entries where the owning PID is dead.
fn cleanup_netns() -> Result<u32, Box<dyn std::error::Error>> {
    let netns_dir = Path::new("/run/netns");
    if !netns_dir.is_dir() {
        return Ok(0);
    }
    let mut count = 0u32;
    for entry in std::fs::read_dir(netns_dir)?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("rem-") {
            continue;
        }
        // Parse PID from rem-{pid}-{n}
        let pid = match extract_pid(&name, "rem-") {
            Some(p) => p,
            None => continue,
        };
        if pid_alive(pid) {
            continue;
        }
        // Use `ip netns del` to properly unmount + remove the entry.
        log::info!("removing stale netns: {}", name);
        let status = std::process::Command::new("ip")
            .args(["netns", "del", &name])
            .status();
        match status {
            Ok(s) if s.success() => {
                count += 1;
                println!("  removed netns {}", name);
            }
            _ => {
                // Fallback: try direct unmount + unlink.
                let path = netns_dir.join(&*name);
                let _ = nix::mount::umount2(&path, nix::mount::MntFlags::MNT_DETACH);
                if std::fs::remove_file(&path).is_ok() {
                    count += 1;
                    println!("  removed netns {} (fallback)", name);
                } else {
                    log::warn!("failed to remove stale netns {}", name);
                }
            }
        }
    }
    Ok(count)
}

/// Remove orphaned `/run/pelagos/{prefix}*` directories where the owning PID is dead.
fn cleanup_dir_pattern(parent: &str, prefix: &str) -> Result<u32, Box<dyn std::error::Error>> {
    let parent = Path::new(parent);
    if !parent.is_dir() {
        return Ok(0);
    }
    let mut count = 0u32;
    for entry in std::fs::read_dir(parent)?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(prefix) {
            continue;
        }
        let pid = match extract_pid(&name, prefix) {
            Some(p) => p,
            None => continue,
        };
        if pid_alive(pid) {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            log::info!("removing stale dir: {}", path.display());
            if std::fs::remove_dir_all(&path).is_ok() {
                count += 1;
                println!("  removed {}", path.display());
            } else {
                log::warn!("failed to remove stale dir {}", path.display());
            }
        }
    }
    Ok(count)
}

/// Extract PID from a name like `{prefix}{pid}-{n}`.
fn extract_pid(name: &str, prefix: &str) -> Option<i32> {
    let rest = name.strip_prefix(prefix)?;
    let pid_str = rest.split('-').next()?;
    pid_str.parse::<i32>().ok()
}

/// Check if a PID is still alive.
fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    unsafe { libc::kill(pid, 0) == 0 }
}
