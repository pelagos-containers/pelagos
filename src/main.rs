#![allow(unused_imports)]

use clap::{Parser, Subcommand};
use core::ffi::CStr;
use libc::{gid_t, uid_t, MS_BIND};
use log::{error, info, warn};
use remora::container::{Child, Command, Error, GidMap, Namespace, Stdio, UidMap};
use std::{
    env::current_dir,
    ffi::{CString, OsStr, OsString},
    fs::read_link,
    os::unix::prelude::{IntoRawFd, OsStrExt},
    path::PathBuf,
    ptr,
    str::FromStr,
};

const SYSFS: &str = "sysfs";

// ---------------------------------------------------------------------------
// OCI subcommands
// ---------------------------------------------------------------------------

#[derive(Subcommand, Debug)]
enum OciCmd {
    /// Set up a container from an OCI bundle, suspend before exec.
    Create {
        /// Container ID
        id: String,
        /// Path to the OCI bundle directory (must contain config.json + rootfs/)
        bundle: PathBuf,
    },
    /// Signal a created container to exec its process.
    Start {
        /// Container ID
        id: String,
    },
    /// Print the current state of a container as JSON.
    State {
        /// Container ID
        id: String,
    },
    /// Send a signal to a container's process.
    Kill {
        /// Container ID
        id: String,
        /// Signal name (e.g. SIGTERM) or number (e.g. 15)
        #[clap(default_value = "SIGTERM")]
        signal: String,
    },
    /// Remove a stopped container's state directory.
    Delete {
        /// Container ID
        id: String,
    },
}

// ---------------------------------------------------------------------------
// Top-level CLI
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[clap(author, version, about = "Remora container runtime")]
struct Cli {
    #[clap(subcommand)]
    command: CliCommand,
}

#[derive(Subcommand, Debug)]
enum CliCommand {
    /// OCI lifecycle: create a container
    Create {
        id: String,
        bundle: PathBuf,
    },
    /// OCI lifecycle: start a created container
    Start {
        id: String,
    },
    /// OCI lifecycle: print container state
    State {
        id: String,
    },
    /// OCI lifecycle: send a signal to a container
    Kill {
        id: String,
        #[clap(default_value = "SIGTERM")]
        signal: String,
    },
    /// OCI lifecycle: delete a stopped container
    Delete {
        id: String,
    },
    /// Run an interactive container (legacy CLI mode)
    Run {
        #[clap(short, long)]
        rootfs: String,
        #[clap(short, long)]
        exe: String,
        #[clap(short, long)]
        uid: u32,
        #[clap(short, long)]
        gid: u32,
        /// Optional network namespace to join
        #[clap(short = 'n', long)]
        join_netns: Option<String>,
    },
}

fn main() {
    env_logger::init();
    info!("Entering main!");

    let cli = Cli::parse();

    let result = match cli.command {
        CliCommand::Create { id, bundle } => {
            remora::oci::cmd_create(&id, &bundle).map_err(|e| e.to_string())
        }
        CliCommand::Start { id } => {
            remora::oci::cmd_start(&id).map_err(|e| e.to_string())
        }
        CliCommand::State { id } => {
            remora::oci::cmd_state(&id).map_err(|e| e.to_string())
        }
        CliCommand::Kill { id, signal } => {
            remora::oci::cmd_kill(&id, &signal).map_err(|e| e.to_string())
        }
        CliCommand::Delete { id } => {
            remora::oci::cmd_delete(&id).map_err(|e| e.to_string())
        }
        CliCommand::Run {
            rootfs,
            exe,
            uid: p_uid,
            gid: p_gid,
            join_netns,
        } => run_interactive(rootfs, exe, p_uid, p_gid, join_netns).map_err(|e| e.to_string()),
    };

    if let Err(e) = result {
        eprintln!("remora: error: {}", e);
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// Legacy interactive run mode
// ---------------------------------------------------------------------------

fn run_interactive(
    rootfs: String,
    exe: String,
    p_uid: u32,
    p_gid: u32,
    join_netns: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let cur_dir = std::env::current_dir().unwrap();
    info!("current dir: {:?}", cur_dir);
    info!("uid: {}, gid: {}", p_uid, p_gid);

    let mut rootfs_path = cur_dir.clone();
    rootfs_path.push(&rootfs);

    let mut path = cur_dir.clone();
    path.push(&rootfs);
    path.push("sys");
    let sys_mount =
        CString::new(path.into_os_string().into_string().unwrap().as_bytes()).unwrap();

    // Mount sys from parent process (we still have privilege here)
    match mount_sys(sys_mount.as_ref()) {
        Ok(_) => info!("mounted sys"),
        Err(e) => info!("failed to mount sys: {:?}", e),
    }

    let result = child_interactive(rootfs_path, &exe, p_uid, p_gid, join_netns);

    // Unmount filesystems when child returns
    match umount_sys(sys_mount.as_ref()) {
        Ok(_) => info!("unmounted sys"),
        Err(e) => info!("failed to unmount sys {:?}", e),
    }

    // Try to unmount proc if it leaked out of mount namespace
    let mut proc_path = cur_dir;
    proc_path.push(&rootfs);
    proc_path.push("proc");
    let proc_mount =
        CString::new(proc_path.into_os_string().into_string().unwrap().as_bytes()).unwrap();
    let _ = umount_sys(proc_mount.as_ref());

    result
}

fn child_interactive(
    rootfs_path: PathBuf,
    exe: &str,
    _uid_parent: uid_t,
    _gid_parent: gid_t,
    join_netns: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    unsafe {
        info!("current user and group before spawn: uid {}, gid {}", libc::getuid(), libc::getgid());
        info!("setting command info and spawning");

        let mut cmd = Command::new(exe)
            .stdin(Stdio::Inherit)
            .stdout(Stdio::Inherit)
            .stderr(Stdio::Inherit)
            .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
            .with_chroot(rootfs_path)
            .with_proc_mount()
            .with_namespaces(Namespace::UTS | Namespace::MOUNT | Namespace::CGROUP);

        if let Some(netns_name) = join_netns {
            let netns_path = format!("/var/run/netns/{}", netns_name);
            info!("Joining network namespace: {}", netns_path);
            cmd = cmd.with_namespace_join(netns_path, Namespace::NET);
        }

        let session = cmd.spawn_interactive()?;

        info!("spawned child {}", session.child.pid());
        match session.run() {
            Ok(_status) => std::process::exit(0),
            Err(e) => {
                error!("relay loop failed: {}", e);
                std::process::exit(1);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Mount helpers (kept from original main.rs)
// ---------------------------------------------------------------------------

fn mount_sys(target_str: &CStr) -> std::io::Result<()> {
    let src_str = CString::new("/sys").unwrap();
    unsafe {
        let src_str_ptr = src_str.as_ptr();
        info!("source is {:?}", src_str);
        let target_str_ptr = target_str.as_ptr();
        info!("target is {:?}", target_str);
        let fs_type_str = CString::new(SYSFS)?;
        let fs_type_str_ptr = fs_type_str.as_ptr();
        info!("fs_type is {:?}", fs_type_str);

        match libc::mount(src_str_ptr, target_str_ptr, fs_type_str_ptr, MS_BIND, ptr::null()) {
            0 => Ok(()),
            _ => Err(std::io::Error::last_os_error()),
        }
    }
}

fn umount_sys(sys_mount: &CStr) -> std::io::Result<()> {
    let target_str_ptr = sys_mount.as_ptr();
    unsafe {
        match libc::umount(target_str_ptr) {
            0 => Ok(()),
            _ => Err(std::io::Error::last_os_error()),
        }
    }
}
