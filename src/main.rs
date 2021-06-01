use core::panic;
use std::env::args;
use std::ffi::CString;
use std::ptr;
use unshare::{Command, Stdio};

fn fork_exec() {
    let self_exe= palaver::env::exe_path();
    let new_args: Vec<_> = std::env::args_os().skip(1).collect();
    let _child_self_status = Command::new(self_exe.unwrap())
        .args(&new_args)
        .arg0("child")
        .unshare([unshare::Namespace::Uts, 
                unshare::Namespace::Pid].iter())
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .status()
        .expect("Failed to execute command");
}

fn mount_proc() -> std::io::Result<()>{
    let c_to_print = CString::new("proc")?;
    unsafe {libc::mount(c_to_print.as_ptr(),
        c_to_print.as_ptr(), 
        c_to_print.as_ptr(),
        0, 
        ptr::null());}
    Ok(())
}

fn child() {
    unsafe {
        let command_launch_status = Command::new(args().nth(1).unwrap())
            .unshare([unshare::Namespace::Uts, 
                    unshare::Namespace::Pid].iter())
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .chroot_dir("/home/rootfs")
            .pre_exec(mount_proc)
            .status()
            .expect("Failed to execute command");

        match command_launch_status.code() {
            Some(code) => println!("Exited with status code: {}", code),
            None => println!("Process terminated by signal"),
        }    
    }
}

fn main() {
    println!("Hello, world!");

    if args().len() < 2 {
        panic!("Not enough argument supplied.  Gotta run something!")
    }

    match args().nth(0).as_deref() {
        Some("child") => {
            println!("CHILD: {}", std::process::id());
            child();
        },
        Some(_) => {
            println!("PARENT: {}", std::process::id());            
            fork_exec();
        },
        _ => {
            panic!("NEITHER PARENT NOR CHILD?");
        }
    }
}
