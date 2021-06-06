#![crate_name = "remora"]

use core::panic;
use std::env::args;
use std::ffi::CString;
use std::ptr;
#[allow(unused_imports)]
use unshare::{Child, Command, Error, GidMap, Stdio, UidMap};

fn fork_exec() -> Result<Child, Error> {
    let self_exe = palaver::env::exe_path();
    let new_args: Vec<_> = std::env::args_os().skip(1).collect();
    unsafe {
        Command::new(self_exe.unwrap())
            .args(&new_args)
            .arg0("child")
            .unshare(
                [
                    unshare::Namespace::Uts,
                    unshare::Namespace::Pid,
                    //unshare::Namespace::User,
                ]
                .iter(),
            )
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            /*
            .set_id_maps(
                vec![UidMap {
                    inside_uid: 1000,
                    outside_uid: 1000,
                    count: 1,
                }],
                vec![GidMap {
                    inside_gid: 1000,
                    outside_gid: 1000,
                    count: 1,
                }],
            )
            */
            .chroot_dir("/home/rootfs")
            .pre_exec(mount_proc)
            .spawn()
    }
}

/// callback that mounts a new proc filesystem
/// this cannot allocate
fn mount_proc() -> std::io::Result<()> {
    unsafe {
        let c_to_print = CString::new("proc")?;
        match libc::mount(
            c_to_print.as_ptr(),
            c_to_print.as_ptr(),
            c_to_print.as_ptr(),
            0,
            ptr::null(),
        ) {
            0 => Ok(()),
            _ => Err(std::io::Error::last_os_error()),
        }
    }
}

/// launch actual child process in new uts and pid namespaces
/// with chroot and new proc filesystem
fn child() -> Result<Child, Error> {
    Command::new(args().nth(1).unwrap())
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .uid(1000)
        .spawn()
}

fn main() {
    println!("Hello, world!");

    if args().len() < 2 {
        panic!("Not enough arguments supplied.  Gotta run something!")
    }

    match args().nth(0).as_deref() {
        Some("child") => {
            println!("CHILD: {}", std::process::id());
            panic_spawn("child", &child);
        }
        Some(_) => {
            println!("PARENT: {}", std::process::id());
            panic_spawn("fork_exec", &fork_exec);
        }
        _ => {
            panic!("NEITHER PARENT NOR CHILD?");
        }
    }
}

fn panic_spawn(which: &'static str, p: &(dyn Fn() -> Result<Child, Error>)) {
    println!("spawning {}", which);
    p().expect(format!("panicking on {}", which).as_str())
        .wait()
        .expect(format!("failed to wait for {} to exit", which).as_str());
}
