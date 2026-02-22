//! DNS daemon management — start/stop/update DNS for container name resolution.
//!
//! Supports two backends:
//! - **Builtin** (default): the `remora-dns` daemon — minimal A-record server
//! - **dnsmasq**: production-grade DNS with caching, AAAA, EDNS, DNSSEC
//!
//! Backend selection: `REMORA_DNS_BACKEND` env var or `--dns-backend` CLI flag.
//! Config files are stored in `<runtime>/dns/`, one per network. Both backends
//! reload on SIGHUP when entries change.

use std::io;
use std::net::Ipv4Addr;
use std::path::PathBuf;

/// DNS backend selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DnsBackend {
    Builtin,
    Dnsmasq,
}

/// Returns the active DNS backend, cached for the lifetime of the process.
///
/// Selection priority:
/// 1. `REMORA_DNS_BACKEND` env var (`builtin` or `dnsmasq`)
/// 2. Default: `Builtin`
pub fn active_backend() -> DnsBackend {
    static BACKEND: std::sync::OnceLock<DnsBackend> = std::sync::OnceLock::new();
    *BACKEND.get_or_init(|| match std::env::var("REMORA_DNS_BACKEND").as_deref() {
        Ok("dnsmasq") => DnsBackend::Dnsmasq,
        _ => DnsBackend::Builtin,
    })
}

/// Config directory for DNS daemon files.
pub fn dns_config_dir() -> PathBuf {
    crate::paths::dns_config_dir()
}

/// Read the daemon PID from the PID file. Returns `None` if not running.
fn daemon_pid() -> Option<i32> {
    let pid_file = crate::paths::dns_pid_file();
    let content = std::fs::read_to_string(pid_file).ok()?;
    let pid: i32 = content.trim().parse().ok()?;
    // Check if the process is actually alive.
    if unsafe { libc::kill(pid, 0) } == 0 {
        Some(pid)
    } else {
        None
    }
}

/// Send SIGHUP to the DNS daemon (reload config).
fn signal_reload() -> io::Result<()> {
    if let Some(pid) = daemon_pid() {
        let ret = unsafe { libc::kill(pid, libc::SIGHUP) };
        if ret != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Read which backend the running daemon was started with.
fn running_backend() -> Option<DnsBackend> {
    let content = std::fs::read_to_string(crate::paths::dns_backend_file()).ok()?;
    match content.trim() {
        "dnsmasq" => Some(DnsBackend::Dnsmasq),
        "builtin" => Some(DnsBackend::Builtin),
        _ => None,
    }
}

/// Write the backend marker file.
fn write_backend_marker(backend: DnsBackend) -> io::Result<()> {
    let label = match backend {
        DnsBackend::Builtin => "builtin",
        DnsBackend::Dnsmasq => "dnsmasq",
    };
    std::fs::write(crate::paths::dns_backend_file(), label)
}

/// Stop the running DNS daemon (SIGTERM + brief wait).
fn stop_daemon() -> io::Result<()> {
    if let Some(pid) = daemon_pid() {
        log::info!("stopping DNS daemon (PID {})", pid);
        unsafe { libc::kill(pid, libc::SIGTERM) };
        // Brief wait for process to exit.
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            if unsafe { libc::kill(pid, 0) } != 0 {
                break;
            }
        }
        // Clean up PID file.
        let _ = std::fs::remove_file(crate::paths::dns_pid_file());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Start the DNS daemon if not already running.
///
/// Dispatches to the builtin `remora-dns` or dnsmasq based on `active_backend()`.
/// If the running daemon uses a different backend, it is stopped and restarted.
/// If dnsmasq is requested but not available, falls back to builtin with a warning.
pub fn ensure_dns_daemon() -> io::Result<()> {
    let desired = active_backend();

    // If a daemon is running with a different backend, stop it first.
    if daemon_pid().is_some() {
        if let Some(running) = running_backend() {
            if running != desired {
                log::info!(
                    "DNS backend changed ({:?} → {:?}), restarting daemon",
                    running,
                    desired
                );
                stop_daemon()?;
            } else {
                return Ok(());
            }
        } else {
            return Ok(());
        }
    }

    let config_dir = dns_config_dir();
    std::fs::create_dir_all(&config_dir)?;

    match desired {
        DnsBackend::Builtin => {
            ensure_builtin_daemon()?;
            write_backend_marker(DnsBackend::Builtin)?;
        }
        DnsBackend::Dnsmasq => match ensure_dnsmasq_daemon() {
            Ok(()) => {
                write_backend_marker(DnsBackend::Dnsmasq)?;
            }
            Err(e) => {
                log::warn!("dnsmasq backend failed ({}), falling back to builtin", e);
                ensure_builtin_daemon()?;
                write_backend_marker(DnsBackend::Builtin)?;
            }
        },
    }

    Ok(())
}

/// Add a container entry to a network's DNS config file.
///
/// Creates the file if it doesn't exist (first container on network).
/// Sends SIGHUP to the daemon to reload. Starts the daemon if not running.
pub fn dns_add_entry(
    network_name: &str,
    container_name: &str,
    ip: Ipv4Addr,
    gateway: Ipv4Addr,
    upstream: &[String],
) -> io::Result<()> {
    let config_dir = dns_config_dir();
    std::fs::create_dir_all(&config_dir)?;

    let config_file = crate::paths::dns_network_file(network_name);

    // Use file locking to prevent races between concurrent `remora run` invocations.
    let lock_path = config_dir.join(format!("{}.lock", network_name));
    let lock_file = std::fs::File::create(&lock_path)?;
    flock_exclusive(&lock_file)?;

    // Read existing content or create new.
    let content = std::fs::read_to_string(&config_file).unwrap_or_default();

    let new_content = if content.is_empty() {
        // Create new config file with header.
        let upstream_str = if upstream.is_empty() {
            "8.8.8.8,1.1.1.1".to_string()
        } else {
            upstream.join(",")
        };
        format!("{} {}\n{} {}\n", gateway, upstream_str, container_name, ip)
    } else {
        // Append entry (remove old entry for same container name first).
        let mut lines: Vec<String> = content
            .lines()
            .filter(|line| {
                // Keep lines that don't start with this container name.
                let first_word = line.split_whitespace().next().unwrap_or("");
                first_word != container_name
            })
            .map(|s| s.to_string())
            .collect();
        lines.push(format!("{} {}", container_name, ip));
        lines.join("\n") + "\n"
    };

    std::fs::write(&config_file, new_content)?;

    // Drop the lock before signaling.
    drop(lock_file);
    let _ = std::fs::remove_file(&lock_path);

    // Ensure firewall allows DNS on this bridge.
    if let Ok(net_def) = crate::network::load_network_def(network_name) {
        allow_dns_on_bridge(&net_def.bridge_name);
    }

    // For dnsmasq backend: regenerate hosts file.
    if active_backend() == DnsBackend::Dnsmasq {
        regenerate_dnsmasq_hosts(network_name)?;
        generate_dnsmasq_conf()?;
    }

    // Ensure daemon is running and signal reload.
    ensure_dns_daemon()?;
    signal_reload()
}

/// Remove a container entry from a network's DNS config file.
///
/// If the file becomes empty (no containers), removes it. Sends SIGHUP to
/// the daemon to reload. If all config files are gone, the daemon will
/// auto-exit on the next reload (builtin) or be stopped (dnsmasq).
pub fn dns_remove_entry(network_name: &str, container_name: &str) -> io::Result<()> {
    let config_dir = dns_config_dir();
    let config_file = crate::paths::dns_network_file(network_name);

    if !config_file.exists() {
        return Ok(());
    }

    // Use file locking.
    let lock_path = config_dir.join(format!("{}.lock", network_name));
    let lock_file = std::fs::File::create(&lock_path)?;
    flock_exclusive(&lock_file)?;

    let content = std::fs::read_to_string(&config_file)?;
    let mut header = String::new();
    let mut entries = Vec::new();

    for (i, line) in content.lines().enumerate() {
        if i == 0 {
            header = line.to_string();
            continue;
        }
        let first_word = line.split_whitespace().next().unwrap_or("");
        if first_word != container_name && !line.trim().is_empty() {
            entries.push(line.to_string());
        }
    }

    if entries.is_empty() {
        // No more containers on this network — remove config file and firewall rule.
        let _ = std::fs::remove_file(&config_file);
        if let Ok(net_def) = crate::network::load_network_def(network_name) {
            disallow_dns_on_bridge(&net_def.bridge_name);
        }
        // Remove dnsmasq hosts file for this network.
        if active_backend() == DnsBackend::Dnsmasq {
            let _ = std::fs::remove_file(crate::paths::dns_hosts_file(network_name));
        }
    } else {
        let mut new_content = header + "\n";
        for entry in &entries {
            new_content.push_str(entry);
            new_content.push('\n');
        }
        std::fs::write(&config_file, new_content)?;

        // Regenerate dnsmasq hosts file.
        if active_backend() == DnsBackend::Dnsmasq {
            regenerate_dnsmasq_hosts(network_name)?;
        }
    }

    // Drop the lock before signaling.
    drop(lock_file);
    let _ = std::fs::remove_file(&lock_path);

    // For dnsmasq: regenerate conf (listen addresses may have changed).
    if active_backend() == DnsBackend::Dnsmasq {
        generate_dnsmasq_conf()?;
    }

    // Signal reload.
    signal_reload()
}

// ---------------------------------------------------------------------------
// Builtin backend (remora-dns)
// ---------------------------------------------------------------------------

/// Start the builtin `remora-dns` daemon via double-fork.
fn ensure_builtin_daemon() -> io::Result<()> {
    // Already running?
    if daemon_pid().is_some() {
        return Ok(());
    }

    let config_dir = dns_config_dir();
    std::fs::create_dir_all(&config_dir)?;

    // Find the remora-dns binary next to the current executable.
    let dns_bin = find_dns_binary()?;

    log::info!("starting builtin DNS daemon: {}", dns_bin.display());

    // Double-fork to daemonize.
    let fork1 = unsafe { libc::fork() };
    match fork1 {
        -1 => return Err(io::Error::last_os_error()),
        0 => {
            // First child: setsid + second fork.
            unsafe { libc::setsid() };
            let fork2 = unsafe { libc::fork() };
            match fork2 {
                -1 => unsafe { libc::_exit(1) },
                0 => {
                    // Grandchild: exec the DNS daemon.
                    // Redirect stdin/stdout/stderr to /dev/null.
                    let devnull = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDWR) };
                    if devnull >= 0 {
                        unsafe {
                            libc::dup2(devnull, 0);
                            libc::dup2(devnull, 1);
                            // Keep stderr for daemon's own logging
                            libc::close(devnull);
                        }
                    }

                    let config_dir_str = config_dir.to_string_lossy().to_string();
                    let err = exec_dns_binary(&dns_bin, &config_dir_str);
                    eprintln!("remora: failed to exec remora-dns: {}", err);
                    unsafe { libc::_exit(1) };
                }
                _ => {
                    // First child exits immediately.
                    unsafe { libc::_exit(0) };
                }
            }
        }
        child_pid => {
            // Parent: wait for first child to exit.
            unsafe {
                libc::waitpid(child_pid, std::ptr::null_mut(), 0);
            }
            // Give the daemon a moment to start and write its PID file.
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    Ok(())
}

/// Find the `remora-dns` binary. Looks next to the current executable first,
/// then falls back to PATH.
fn find_dns_binary() -> io::Result<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        // Try next to current executable (e.g. target/debug/remora-dns).
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("remora-dns");
            if candidate.exists() {
                return Ok(candidate);
            }
            // During `cargo test`, exe is in target/debug/deps/ — try parent.
            if let Some(parent) = dir.parent() {
                let candidate = parent.join("remora-dns");
                if candidate.exists() {
                    return Ok(candidate);
                }
            }
        }
    }

    // Fall back to PATH lookup.
    if let Ok(output) = std::process::Command::new("which")
        .arg("remora-dns")
        .output()
    {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Ok(PathBuf::from(path));
            }
        }
    }

    Err(io::Error::other(
        "remora-dns binary not found (expected next to remora binary or in PATH)",
    ))
}

/// Exec the DNS binary (called in the grandchild after double-fork).
fn exec_dns_binary(bin: &std::path::Path, config_dir: &str) -> io::Error {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let bin_c = CString::new(bin.as_os_str().as_bytes()).unwrap();
    let arg_config = CString::new("--config-dir").unwrap();
    let arg_dir = CString::new(config_dir).unwrap();
    let args = [
        bin_c.as_ptr(),
        arg_config.as_ptr(),
        arg_dir.as_ptr(),
        std::ptr::null(),
    ];

    unsafe {
        libc::execv(bin_c.as_ptr(), args.as_ptr());
    }
    io::Error::last_os_error()
}

// ---------------------------------------------------------------------------
// dnsmasq backend
// ---------------------------------------------------------------------------

/// Check if dnsmasq is available on PATH.
fn find_dnsmasq() -> io::Result<PathBuf> {
    let output = std::process::Command::new("which")
        .arg("dnsmasq")
        .output()
        .map_err(|e| io::Error::other(format!("failed to search for dnsmasq: {}", e)))?;

    if output.status.success() {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !path.is_empty() {
            return Ok(PathBuf::from(path));
        }
    }

    Err(io::Error::other(
        "dnsmasq not found on PATH (install dnsmasq or use --dns-backend builtin)",
    ))
}

/// Generate `<runtime>/dns/dnsmasq.conf` from per-network config files.
///
/// Scans all per-network config files, extracts gateway IPs and upstream servers,
/// and writes a dnsmasq config with `--bind-dynamic`, `listen-address` directives,
/// and `addn-hosts` for each network.
fn generate_dnsmasq_conf() -> io::Result<()> {
    let config_dir = dns_config_dir();
    let mut listen_addresses = Vec::new();
    let mut upstream_servers = Vec::new();
    let mut hosts_files = Vec::new();

    // Scan per-network config files (same format as builtin daemon).
    let entries = std::fs::read_dir(&config_dir)?;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Skip non-network files.
        if name_str == "pid"
            || name_str == "backend"
            || name_str == "dnsmasq.conf"
            || name_str.ends_with(".lock")
            || name_str.starts_with("hosts.")
        {
            continue;
        }

        let content = match std::fs::read_to_string(entry.path()) {
            Ok(c) => c,
            Err(_) => continue,
        };

        // First line: "<gateway> <upstream1>,<upstream2>"
        if let Some(header) = content.lines().next() {
            let mut parts = header.split_whitespace();
            if let Some(gw) = parts.next() {
                if gw.parse::<Ipv4Addr>().is_ok() && !listen_addresses.contains(&gw.to_string()) {
                    listen_addresses.push(gw.to_string());
                }
            }
            if let Some(upstreams) = parts.next() {
                for server in upstreams.split(',') {
                    let s = server.trim().to_string();
                    if !s.is_empty() && !upstream_servers.contains(&s) {
                        upstream_servers.push(s);
                    }
                }
            }
        }

        // Generate hosts file for this network.
        let hosts_path = crate::paths::dns_hosts_file(&name_str);
        if hosts_path.exists() {
            hosts_files.push(hosts_path.to_string_lossy().to_string());
        }
    }

    if listen_addresses.is_empty() {
        // No networks with DNS entries — nothing to configure.
        return Ok(());
    }

    let pid_file = crate::paths::dns_pid_file();
    let mut conf = String::from("# Auto-generated by remora — do not edit\n");
    conf.push_str("no-resolv\n");
    conf.push_str("no-daemon\n");
    conf.push_str("bind-dynamic\n");
    conf.push_str("local-service\n");
    conf.push_str(&format!("pid-file={}\n", pid_file.display()));

    for addr in &listen_addresses {
        conf.push_str(&format!("listen-address={}\n", addr));
    }
    for server in &upstream_servers {
        conf.push_str(&format!("server={}\n", server));
    }
    for hosts in &hosts_files {
        conf.push_str(&format!("addn-hosts={}\n", hosts));
    }

    std::fs::write(crate::paths::dns_dnsmasq_conf(), conf)
}

/// Regenerate `<runtime>/dns/hosts.<network>` from the per-network config file.
///
/// Reads lines 2+ from the network config (format: `<name> <ip>`) and writes
/// them in `/etc/hosts` format (`<ip> <name>`).
fn regenerate_dnsmasq_hosts(network_name: &str) -> io::Result<()> {
    let config_file = crate::paths::dns_network_file(network_name);
    let content = match std::fs::read_to_string(&config_file) {
        Ok(c) => c,
        Err(_) => {
            // Config gone — remove hosts file.
            let _ = std::fs::remove_file(crate::paths::dns_hosts_file(network_name));
            return Ok(());
        }
    };

    let mut hosts = String::new();
    for (i, line) in content.lines().enumerate() {
        if i == 0 {
            continue; // Skip header line.
        }
        let mut parts = line.split_whitespace();
        if let (Some(name), Some(ip)) = (parts.next(), parts.next()) {
            // /etc/hosts format: IP NAME
            hosts.push_str(&format!("{}\t{}\n", ip, name));
        }
    }

    std::fs::write(crate::paths::dns_hosts_file(network_name), hosts)
}

/// Start dnsmasq as a daemon via double-fork.
fn ensure_dnsmasq_daemon() -> io::Result<()> {
    // Already running?
    if daemon_pid().is_some() {
        return Ok(());
    }

    let dnsmasq_bin = find_dnsmasq()?;

    // Generate config before starting.
    generate_dnsmasq_conf()?;

    let conf_path = crate::paths::dns_dnsmasq_conf();
    if !conf_path.exists() {
        return Err(io::Error::other("no DNS config to serve"));
    }

    log::info!("starting dnsmasq DNS daemon");

    // Double-fork to daemonize.
    let fork1 = unsafe { libc::fork() };
    match fork1 {
        -1 => return Err(io::Error::last_os_error()),
        0 => {
            unsafe { libc::setsid() };
            let fork2 = unsafe { libc::fork() };
            match fork2 {
                -1 => unsafe { libc::_exit(1) },
                0 => {
                    // Grandchild: exec dnsmasq.
                    let devnull = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDWR) };
                    if devnull >= 0 {
                        unsafe {
                            libc::dup2(devnull, 0);
                            libc::dup2(devnull, 1);
                            libc::close(devnull);
                        }
                    }

                    let err = exec_dnsmasq(&dnsmasq_bin, &conf_path);
                    eprintln!("remora: failed to exec dnsmasq: {}", err);
                    unsafe { libc::_exit(1) };
                }
                _ => {
                    unsafe { libc::_exit(0) };
                }
            }
        }
        child_pid => {
            unsafe {
                libc::waitpid(child_pid, std::ptr::null_mut(), 0);
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }

    Ok(())
}

/// Exec dnsmasq with the generated config file.
fn exec_dnsmasq(bin: &std::path::Path, conf: &std::path::Path) -> io::Error {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let bin_c = CString::new(bin.as_os_str().as_bytes()).unwrap();
    let conf_str = format!("--conf-file={}", conf.display());
    let arg_conf = CString::new(conf_str.as_bytes()).unwrap();
    // --keep-in-foreground: dnsmasq stays in foreground (we daemonize ourselves),
    // allowing us to track its PID via the pid-file directive in the config.
    let arg_fg = CString::new("--keep-in-foreground").unwrap();
    let args = [
        bin_c.as_ptr(),
        arg_conf.as_ptr(),
        arg_fg.as_ptr(),
        std::ptr::null(),
    ];

    unsafe {
        libc::execv(bin_c.as_ptr(), args.as_ptr());
    }
    io::Error::last_os_error()
}

// ---------------------------------------------------------------------------
// Firewall helpers
// ---------------------------------------------------------------------------

/// Add an iptables INPUT rule to allow UDP port 53 on a bridge interface.
///
/// Hosts with restrictive INPUT policies (DROP/REJECT) block DNS queries
/// from containers to the gateway. This rule ensures the DNS daemon can
/// receive queries on the bridge.
fn allow_dns_on_bridge(bridge: &str) {
    use std::process::Command as SysCmd;

    // Purge any stale duplicates first.
    while SysCmd::new("iptables")
        .args([
            "-D", "INPUT", "-i", bridge, "-p", "udp", "--dport", "53", "-j", "ACCEPT",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
    {}

    // Insert fresh rule.
    let _ = SysCmd::new("iptables")
        .args([
            "-I", "INPUT", "-i", bridge, "-p", "udp", "--dport", "53", "-j", "ACCEPT",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Remove the iptables INPUT rule for DNS on a bridge interface.
fn disallow_dns_on_bridge(bridge: &str) {
    use std::process::Command as SysCmd;

    while SysCmd::new("iptables")
        .args([
            "-D", "INPUT", "-i", bridge, "-p", "udp", "--dport", "53", "-j", "ACCEPT",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
    {}
}

/// Apply an exclusive flock on the file.
fn flock_exclusive(file: &std::fs::File) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if ret != 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
