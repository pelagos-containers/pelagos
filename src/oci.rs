//! OCI Runtime Specification v1.0.2 implementation.
//!
//! Implements the five lifecycle subcommands (create, start, state, kill, delete)
//! and config.json parsing for OCI bundle compatibility.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// config.json types (first-pass — required fields only)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OciConfig {
    pub oci_version: String,
    pub root: OciRoot,
    pub process: OciProcess,
    pub hostname: Option<String>,
    pub linux: Option<OciLinux>,
    #[serde(default)]
    pub mounts: Vec<OciMount>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OciRoot {
    pub path: String,
    #[serde(default)]
    pub readonly: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OciProcess {
    pub args: Vec<String>,
    pub cwd: String,
    #[serde(default)]
    pub env: Vec<String>,
    pub user: Option<OciUser>,
    #[serde(default)]
    pub no_new_privileges: bool,
    #[serde(default)]
    pub terminal: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OciUser {
    #[serde(default)]
    pub uid: u32,
    #[serde(default)]
    pub gid: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OciLinux {
    #[serde(default)]
    pub namespaces: Vec<OciNamespace>,
    #[serde(default)]
    pub uid_mappings: Vec<OciIdMapping>,
    #[serde(default)]
    pub gid_mappings: Vec<OciIdMapping>,
}

#[derive(Debug, Deserialize)]
pub struct OciNamespace {
    #[serde(rename = "type")]
    pub ns_type: String,
    pub path: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OciIdMapping {
    pub host_id: u32,
    pub container_id: u32,
    pub size: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OciMount {
    pub destination: String,
    #[serde(rename = "type")]
    pub mount_type: Option<String>,
    pub source: Option<String>,
    #[serde(default)]
    pub options: Vec<String>,
}

// ---------------------------------------------------------------------------
// State types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OciState {
    pub oci_version: String,
    pub id: String,
    pub status: String,
    pub pid: i32,
    pub bundle: String,
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

pub fn state_dir(id: &str) -> PathBuf {
    PathBuf::from(format!("/run/remora/{}", id))
}

pub fn state_path(id: &str) -> PathBuf {
    state_dir(id).join("state.json")
}

pub fn exec_sock_path(id: &str) -> PathBuf {
    state_dir(id).join("exec.sock")
}

// ---------------------------------------------------------------------------
// State I/O
// ---------------------------------------------------------------------------

pub fn read_state(id: &str) -> io::Result<OciState> {
    let content = fs::read(state_path(id))?;
    serde_json::from_slice(&content)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub fn write_state(id: &str, state: &OciState) -> io::Result<()> {
    let content = serde_json::to_vec_pretty(state)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    fs::write(state_path(id), content)
}

// ---------------------------------------------------------------------------
// Config loading
// ---------------------------------------------------------------------------

pub fn config_from_bundle(bundle: &Path) -> io::Result<OciConfig> {
    let config_path = bundle.join("config.json");
    let content = fs::read(&config_path)?;
    serde_json::from_slice(&content)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

// ---------------------------------------------------------------------------
// Build a container::Command from OCI config
// ---------------------------------------------------------------------------

pub fn build_command(
    config: &OciConfig,
    bundle: &Path,
) -> io::Result<crate::container::Command> {
    use crate::container::{Command, Namespace};

    let root_path = bundle.join(&config.root.path);
    let exe = config
        .process
        .args
        .first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "process.args is empty"))?;

    let mut cmd = Command::new(exe)
        .env_clear()
        .with_chroot(&root_path)
        .with_cwd(&config.process.cwd)
        .stdout(crate::container::Stdio::Inherit)
        .stderr(crate::container::Stdio::Inherit);

    // Remaining args (exe is args[0])
    if config.process.args.len() > 1 {
        let rest: Vec<&str> = config.process.args[1..].iter().map(|s| s.as_str()).collect();
        cmd = cmd.args(&rest);
    }

    // Environment
    for entry in &config.process.env {
        if let Some(eq) = entry.find('=') {
            cmd = cmd.env(&entry[..eq], &entry[eq + 1..]);
        } else {
            cmd = cmd.env(entry, "");
        }
    }

    // User (uid/gid)
    if let Some(ref user) = config.process.user {
        cmd = cmd.with_uid(user.uid).with_gid(user.gid);
    }

    // Security flags
    if config.process.no_new_privileges {
        cmd = cmd.with_no_new_privileges(true);
    }
    if config.root.readonly {
        cmd = cmd.with_readonly_rootfs(true);
    }

    // Linux namespaces + UID/GID mappings
    if let Some(ref linux) = config.linux {
        let mut ns_flags = Namespace::empty();
        for ns in &linux.namespaces {
            let flag = match ns.ns_type.as_str() {
                "mount" => Some(Namespace::MOUNT),
                "uts" => Some(Namespace::UTS),
                "ipc" => Some(Namespace::IPC),
                "user" => Some(Namespace::USER),
                "pid" => Some(Namespace::PID),
                "network" => Some(Namespace::NET),
                "cgroup" => Some(Namespace::CGROUP),
                _ => None,
            };
            if let Some(flag) = flag {
                if let Some(ref path) = ns.path {
                    // Join an existing namespace by path
                    cmd = cmd.with_namespace_join(path, flag);
                } else {
                    ns_flags |= flag;
                }
            }
        }
        if !ns_flags.is_empty() {
            cmd = cmd.with_namespaces(ns_flags);
        }

        // Mount proc automatically when a mount namespace is requested
        let has_mount_ns = linux.namespaces.iter().any(|n| n.ns_type == "mount" && n.path.is_none());
        if has_mount_ns {
            cmd = cmd.with_proc_mount();
        }

        // UID/GID mappings
        if !linux.uid_mappings.is_empty() {
            let maps: Vec<crate::container::UidMap> = linux
                .uid_mappings
                .iter()
                .map(|m| crate::container::UidMap {
                    inside: m.container_id,
                    outside: m.host_id,
                    count: m.size,
                })
                .collect();
            cmd = cmd.with_uid_maps(&maps);
        }
        if !linux.gid_mappings.is_empty() {
            let maps: Vec<crate::container::GidMap> = linux
                .gid_mappings
                .iter()
                .map(|m| crate::container::GidMap {
                    inside: m.container_id,
                    outside: m.host_id,
                    count: m.size,
                })
                .collect();
            cmd = cmd.with_gid_maps(&maps);
        }
    }

    // OCI mounts (processed in order)
    for mount in &config.mounts {
        let dest = &mount.destination;
        let is_ro = mount.options.iter().any(|o| o == "ro" || o == "readonly");
        let mount_type = mount.mount_type.as_deref().unwrap_or("bind");

        match mount_type {
            "tmpfs" => {
                let opts: Vec<&str> = mount.options.iter().map(|s| s.as_str()).collect();
                cmd = cmd.with_tmpfs(dest, &opts.join(","));
            }
            _ => {
                // bind mount
                if let Some(ref source) = mount.source {
                    if is_ro {
                        cmd = cmd.with_bind_mount_ro(source, dest);
                    } else {
                        cmd = cmd.with_bind_mount(source, dest);
                    }
                }
            }
        }
    }

    Ok(cmd)
}

// ---------------------------------------------------------------------------
// Socket helpers
// ---------------------------------------------------------------------------

/// Create a Unix domain socket listener at `path`. Returns the listening fd.
fn create_listen_socket(path: &Path) -> io::Result<i32> {
    use std::os::unix::ffi::OsStrExt;
    let path_bytes = path.as_os_str().as_bytes();
    if path_bytes.len() >= 108 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "socket path too long"));
    }

    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        std::ptr::copy_nonoverlapping(
            path_bytes.as_ptr() as *const libc::c_char,
            addr.sun_path.as_mut_ptr(),
            path_bytes.len(),
        );
        let addr_len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;

        let ret = libc::bind(
            fd,
            &addr as *const libc::sockaddr_un as *const libc::sockaddr,
            addr_len,
        );
        if ret != 0 {
            libc::close(fd);
            return Err(io::Error::last_os_error());
        }

        let ret = libc::listen(fd, 1);
        if ret != 0 {
            libc::close(fd);
            return Err(io::Error::last_os_error());
        }

        Ok(fd)
    }
}

/// Connect to a Unix domain socket at `path`. Returns the connected fd.
fn connect_socket(path: &Path) -> io::Result<i32> {
    use std::os::unix::ffi::OsStrExt;
    let path_bytes = path.as_os_str().as_bytes();

    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        std::ptr::copy_nonoverlapping(
            path_bytes.as_ptr() as *const libc::c_char,
            addr.sun_path.as_mut_ptr(),
            path_bytes.len(),
        );
        let addr_len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;

        let ret = libc::connect(
            fd,
            &addr as *const libc::sockaddr_un as *const libc::sockaddr,
            addr_len,
        );
        if ret != 0 {
            libc::close(fd);
            return Err(io::Error::last_os_error());
        }

        Ok(fd)
    }
}

// ---------------------------------------------------------------------------
// OCI subcommand implementations
// ---------------------------------------------------------------------------

/// `remora create <id> <bundle>` — set up container, suspend before exec.
///
/// Forks a shim that calls `command.spawn()`. The container's pre_exec writes
/// its PID to a ready pipe (signalling "created"), then blocks on accept().
/// The parent reads the PID, writes state.json, and exits. The shim is orphaned
/// and waits for the container; `remora start` later unblocks it.
pub fn cmd_create(id: &str, bundle_path: &Path) -> io::Result<()> {
    let dir = state_dir(id);
    if dir.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("container '{}' already exists — run 'remora delete {}' first", id, id),
        ));
    }

    let bundle = bundle_path.canonicalize()?;
    let config = config_from_bundle(&bundle)?;
    fs::create_dir_all(&dir)?;

    // Ready pipe: grandchild writes PID → parent reads it.
    let mut pipe_fds = [0i32; 2];
    if unsafe { libc::pipe2(pipe_fds.as_mut_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let (ready_r, ready_w) = (pipe_fds[0], pipe_fds[1]);

    // Listen socket: grandchild blocks on accept() until "remora start" connects.
    let sock_path = exec_sock_path(id);
    let listen_fd = create_listen_socket(&sock_path).map_err(|e| {
        unsafe { libc::close(ready_r); libc::close(ready_w); }
        e
    })?;

    // Build the container command with OCI sync hooks.
    let command = match build_command(&config, &bundle) {
        Ok(c) => c.with_oci_sync(ready_w, listen_fd),
        Err(e) => {
            unsafe { libc::close(ready_r); libc::close(ready_w); libc::close(listen_fd); }
            let _ = fs::remove_dir_all(&dir);
            return Err(e);
        }
    };

    // Fork shim. The shim calls command.spawn() (which forks AGAIN to create
    // the actual container). Rust's spawn() blocks the shim until the container
    // execs; the parent reads the ready pipe and exits without waiting.
    match unsafe { libc::fork() } {
        -1 => {
            unsafe { libc::close(ready_r); libc::close(ready_w); libc::close(listen_fd); }
            let _ = fs::remove_dir_all(&dir);
            Err(io::Error::last_os_error())
        }
        0 => {
            // SHIM: detach from the parent's stdio so that the parent's
            // `output()` / pipe can receive EOF as soon as the parent exits.
            // Without this, the shim (and any grandchild) hold the write
            // ends of the test's stdout/stderr pipes open indefinitely,
            // causing `run_remora` to hang waiting for EOF.
            unsafe {
                libc::close(ready_r);
                let dev_null = libc::open(
                    b"/dev/null\0".as_ptr() as *const libc::c_char,
                    libc::O_RDWR,
                    0,
                );
                if dev_null >= 0 {
                    libc::dup2(dev_null, 0);
                    libc::dup2(dev_null, 1);
                    libc::dup2(dev_null, 2);
                    if dev_null > 2 {
                        libc::close(dev_null);
                    }
                }
            }
            let mut child = match command.spawn() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("remora: create: spawn failed: {}", e);
                    unsafe { libc::_exit(1) };
                }
            };
            // Container exec'd. Wait for it, then exit (shim is orphaned at this point).
            child.wait().ok();
            unsafe { libc::_exit(0) };
        }
        _ => {
            // PARENT: close the write ends (child has them).
            unsafe { libc::close(ready_w) };
            unsafe { libc::close(listen_fd) };

            // Read container PID (4 bytes) written by the grandchild's pre_exec.
            let mut pid_buf = [0u8; 4];
            let n = unsafe {
                libc::read(ready_r, pid_buf.as_mut_ptr() as *mut libc::c_void, 4)
            };
            unsafe { libc::close(ready_r) };

            if n != 4 {
                let _ = fs::remove_dir_all(&dir);
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "container setup failed (ready pipe closed before PID was written)",
                ));
            }
            let container_pid = i32::from_ne_bytes(pid_buf);

            // Write state.json with status=created.
            let state = OciState {
                oci_version: "1.0.2".to_string(),
                id: id.to_string(),
                status: "created".to_string(),
                pid: container_pid,
                bundle: bundle.to_string_lossy().into_owned(),
            };
            write_state(id, &state)?;

            // Parent exits; the shim (blocking in spawn()) is adopted by init.
            Ok(())
        }
    }
}

/// `remora start <id>` — signal the container to exec.
pub fn cmd_start(id: &str) -> io::Result<()> {
    let state = read_state(id)?;
    if state.status != "created" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "container '{}' is not in 'created' state (current: {})",
                id, state.status
            ),
        ));
    }

    // Connect to exec.sock and send the start byte.
    let sock_path = exec_sock_path(id);
    let fd = connect_socket(&sock_path)?;
    unsafe {
        let buf = [1u8];
        libc::write(fd, buf.as_ptr() as *const libc::c_void, 1);
        libc::close(fd);
    }

    // Update state to running.
    let mut state = state;
    state.status = "running".to_string();
    write_state(id, &state)?;

    // Remove exec.sock — the container has exec'd and no longer listens.
    let _ = fs::remove_file(&sock_path);

    Ok(())
}

/// `remora state <id>` — print container state JSON to stdout.
pub fn cmd_state(id: &str) -> io::Result<()> {
    let mut state = read_state(id)?;

    // Determine actual liveness via kill(pid, 0).
    if state.status == "created" || state.status == "running" {
        let alive = unsafe { libc::kill(state.pid, 0) } == 0;
        if !alive {
            state.status = "stopped".to_string();
        }
    }

    let json = serde_json::to_string_pretty(&state)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    println!("{}", json);
    Ok(())
}

/// `remora kill <id> <signal>` — send a signal to the container process.
pub fn cmd_kill(id: &str, signal: &str) -> io::Result<()> {
    let state = read_state(id)?;

    let sig: i32 = match signal {
        "SIGTERM" | "TERM" | "15" => libc::SIGTERM,
        "SIGKILL" | "KILL" | "9" => libc::SIGKILL,
        "SIGHUP" | "HUP" | "1" => libc::SIGHUP,
        "SIGINT" | "INT" | "2" => libc::SIGINT,
        "SIGUSR1" | "USR1" | "10" => libc::SIGUSR1,
        "SIGUSR2" | "USR2" | "12" => libc::SIGUSR2,
        s => s.parse::<i32>().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown signal '{}' — use a name (SIGTERM) or number (15)", s),
            )
        })?,
    };

    let ret = unsafe { libc::kill(state.pid, sig) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `remora delete <id>` — remove state dir after container has stopped.
pub fn cmd_delete(id: &str) -> io::Result<()> {
    let state = read_state(id)?;

    // Allow delete if process is gone (stopped) regardless of state.json status.
    let alive = unsafe { libc::kill(state.pid, 0) } == 0;
    if alive {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "container '{}' is still running (pid {}); stop it first",
                id, state.pid
            ),
        ));
    }

    fs::remove_dir_all(state_dir(id))?;
    Ok(())
}
