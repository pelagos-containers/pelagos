//! Native namespace-join helpers, replacing `nsenter`/`ip netns` subprocess
//! shell-outs (#564).
//!
//! Mirrors the proven pattern already used by the `pelagos` library's
//! `netlink::in_netns`/`netns_create`/`netns_del` — duplicated here rather
//! than shared directly, since `pelagos-cri` deliberately doesn't link the
//! full `pelagos` crate (to avoid pulling in cgroups-rs/seccompiler/oci-client
//! transitively just for a CRI shim).
//!
//! **Why a dedicated OS thread, not `tokio::spawn`/`spawn_blocking`:**
//! `setns`/`unshare` change namespace membership for the CALLING THREAD only.
//! Tokio's worker and blocking-pool threads are reused across unrelated
//! tasks, so changing one's namespace would silently leak into whatever work
//! that pooled thread picks up next — a correctness landmine in a long-lived
//! server process. A brand-new `std::thread::spawn` thread that is joined
//! and discarded immediately after one use has no such risk: it terminates
//! for good as soon as the closure returns, taking its altered namespace
//! membership with it.

use std::ffi::CString;
use std::io;

/// Open every `(namespace_path, CLONE_NEW* flag)` pair's namespace file **on
/// the calling thread** (so a bad path surfaces immediately, before any
/// thread is spawned), then enter them in order and run `f`, all within ONE
/// throwaway thread. Mirrors `nsenter --net=A --ipc=B --uts=C -- <command>`
/// for however many namespaces are given.
fn in_namespaces<T, F>(namespaces: &[(String, libc::c_int)], f: F) -> io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    let mut fds: Vec<(i32, libc::c_int)> = Vec::with_capacity(namespaces.len());
    for (path, ns_type) in namespaces {
        let cpath = CString::new(path.as_bytes()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "namespace path contains NUL")
        });
        let cpath = match cpath {
            Ok(c) => c,
            Err(e) => {
                for (fd, _) in &fds {
                    unsafe { libc::close(*fd) };
                }
                return Err(e);
            }
        };
        let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            let e = io::Error::last_os_error();
            for (prev_fd, _) in &fds {
                unsafe { libc::close(*prev_fd) };
            }
            return Err(e);
        }
        fds.push((fd, *ns_type));
    }

    std::thread::spawn(move || -> io::Result<T> {
        for (fd, ns_type) in &fds {
            let ret = unsafe { libc::setns(*fd, *ns_type) };
            unsafe { libc::close(*fd) };
            if ret < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        f()
    })
    .join()
    .map_err(|_| io::Error::other("namespace-join thread panicked"))?
}

/// Run `f` after entering the network namespace at `netns_path` (e.g.
/// `/run/netns/<name>`) on a throwaway thread. Mirrors `nsenter --net=<path>`.
pub fn in_net_ns<T, F>(netns_path: &str, f: F) -> io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    in_namespaces(&[(netns_path.to_string(), libc::CLONE_NEWNET)], f)
}

/// Run `f` after entering the UTS namespace at `uts_path` (e.g.
/// `/proc/<pid>/ns/uts`) on a throwaway thread. Mirrors `nsenter --uts=<path>`.
pub fn in_uts_ns<T, F>(uts_path: &str, f: F) -> io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    in_namespaces(&[(uts_path.to_string(), libc::CLONE_NEWUTS)], f)
}

/// Run `f` after entering a pod sandbox's net, IPC, and UTS namespaces (in
/// that order) on ONE throwaway thread. Mirrors `nsenter --net=A --ipc=B
/// --uts=C -- <command>` — the exact three-namespace combination a pod's
/// sysctls (`net.*` in the net ns; `kernel.*`/`fs.*` in IPC/UTS) need to be
/// visible in.
pub fn in_pod_ns<T, F>(net_path: &str, ipc_path: &str, uts_path: &str, f: F) -> io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    in_namespaces(
        &[
            (net_path.to_string(), libc::CLONE_NEWNET),
            (ipc_path.to_string(), libc::CLONE_NEWIPC),
            (uts_path.to_string(), libc::CLONE_NEWUTS),
        ],
        f,
    )
}

/// Bring up a network interface by name using `SIOCSIFFLAGS` to set
/// `IFF_UP`. Must be called from within the target network namespace (e.g.
/// via [`in_net_ns`]) — this only ever touches the CALLING thread's current
/// netns, same as `ip link set <ifname> up` would.
pub fn set_link_up(ifname: &str) -> io::Result<()> {
    // A minimal ifreq layout sufficient for SIOCGIFFLAGS / SIOCSIFFLAGS:
    //   char   ifr_name[16];   // IFNAMSIZ
    //   short  ifr_flags;      // part of the union
    //   u8     _pad[22];       // rest of the 24-byte union
    #[repr(C)]
    struct Ifreq {
        ifr_name: [u8; 16],
        ifr_flags: libc::c_short,
        _pad: [u8; 22],
    }

    if ifname.len() >= 16 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "interface name too long for IFNAMSIZ",
        ));
    }
    let mut req = Ifreq {
        ifr_name: [0u8; 16],
        ifr_flags: 0,
        _pad: [0u8; 22],
    };
    req.ifr_name[..ifname.len()].copy_from_slice(ifname.as_bytes());

    unsafe {
        let sock = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        if sock < 0 {
            return Err(io::Error::last_os_error());
        }

        let ret = libc::ioctl(sock, libc::SIOCGIFFLAGS as _, &mut req as *mut Ifreq);
        if ret < 0 {
            let e = io::Error::last_os_error();
            libc::close(sock);
            return Err(e);
        }

        req.ifr_flags |= libc::IFF_UP as libc::c_short;

        let ret = libc::ioctl(sock, libc::SIOCSIFFLAGS as _, &mut req as *mut Ifreq);
        libc::close(sock);
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Write a sysctl by its dotted key (e.g. `net.ipv4.ip_unprivileged_port_start`),
/// translating it to its `/proc/sys/...` path the same way `sysctl -w` does
/// internally. Must be called from within whichever namespace the key is
/// scoped to (net.* -> net ns, kernel.*/fs.* -> UTS/IPC/mount ns as
/// applicable) — `/proc/sys` entries are resolved against the CALLING
/// task's own namespace memberships regardless of which fd was `setns`'d
/// last, so entering the right combination (see [`in_pod_ns`]) beforehand is
/// sufficient; no extra routing logic is needed here.
pub fn write_sysctl(key: &str, value: &str) -> io::Result<()> {
    let path = format!("/proc/sys/{}", key.replace('.', "/"));
    std::fs::write(path, value)
}

/// Set this (namespace-entered) thread's hostname via `sethostname(2)` — the
/// same syscall `hostname <name>` wraps, and what `/proc/sys/kernel.hostname`
/// writes through to as well. Must be called from within the target UTS
/// namespace (e.g. via [`in_uts_ns`]).
pub fn set_hostname(name: &str) -> io::Result<()> {
    let ret = unsafe { libc::sethostname(name.as_ptr() as *const libc::c_char, name.len()) };
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Create a named network namespace at `/run/netns/<name>`, the same
/// persistent-bind-mount trick `ip netns add <name>` uses: `unshare` a new
/// net namespace on a throwaway thread, then bind-mount a reference to it
/// over an empty file so it outlives the thread. Returns the netns path.
/// Idempotent — returns `Ok` if the path already exists.
pub fn create_netns(name: &str) -> io::Result<String> {
    let path = format!("/run/netns/{name}");
    std::fs::create_dir_all("/run/netns")?;

    let cpath = CString::new(path.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "netns name contains NUL"))?;
    let fd = unsafe {
        libc::open(
            cpath.as_ptr(),
            libc::O_RDONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC,
            0o444_u32,
        )
    };
    if fd < 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EEXIST) {
            return Ok(path);
        }
        return Err(e);
    }
    unsafe { libc::close(fd) };

    let path_clone = path.clone();
    let result = std::thread::spawn(move || -> io::Result<()> {
        let ret = unsafe { libc::unshare(libc::CLONE_NEWNET) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        let src = CString::new("/proc/thread-self/ns/net").unwrap();
        let dst = CString::new(path_clone.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad path"))?;
        let ret = unsafe {
            libc::mount(
                src.as_ptr(),
                dst.as_ptr(),
                std::ptr::null(),
                libc::MS_BIND,
                std::ptr::null(),
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    })
    .join()
    .map_err(|_| io::Error::other("netns_create thread panicked"))?;

    if result.is_err() {
        let _ = std::fs::remove_file(&path);
    }
    result.map(|()| path)
}

/// Delete a named network namespace created by [`create_netns`]: unmount the
/// bind-mounted reference and remove the file. Best-effort — mirrors the
/// previous `ip netns del` call site, which also ignored errors (the
/// namespace may already be gone).
pub fn delete_netns(name: &str) {
    let path = format!("/run/netns/{name}");
    let Ok(cpath) = CString::new(path.as_bytes()) else {
        return;
    };
    unsafe {
        libc::umount2(cpath.as_ptr(), libc::MNT_DETACH);
        libc::unlink(cpath.as_ptr());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #564: `set_link_up` must actually set `IFF_UP` on a real interface.
    /// Uses `lo` in the CURRENT (test process's) namespace rather than
    /// creating a new one — no root/CAP_NET_ADMIN required to toggle `lo`'s
    /// own flags via a plain `AF_INET`/`SOCK_DGRAM` socket.
    #[test]
    fn set_link_up_sets_iff_up_on_loopback() {
        set_link_up("lo").expect("set_link_up(lo) must succeed in any namespace");
        // Confirm via sysfs rather than re-deriving ifreq-reading logic here.
        let flags = std::fs::read_to_string("/sys/class/net/lo/flags")
            .expect("read /sys/class/net/lo/flags");
        let flags = u32::from_str_radix(flags.trim().trim_start_matches("0x"), 16)
            .expect("parse flags as hex");
        assert_eq!(
            flags & libc::IFF_UP as u32,
            libc::IFF_UP as u32,
            "IFF_UP must be set on lo after set_link_up"
        );
    }

    /// #564: name-too-long must be rejected before ever touching a socket —
    /// not panic on the fixed-size `ifr_name` buffer.
    #[test]
    fn set_link_up_rejects_overlong_name() {
        let err = set_link_up("this-name-is-way-too-long-for-ifnamsiz")
            .expect_err("a 16+ byte name must be rejected");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    /// #564: `write_sysctl` must translate dotted keys to `/proc/sys/...`
    /// paths exactly as `sysctl -w` does — dots become slashes, nothing else.
    #[test]
    fn write_sysctl_translates_dots_to_proc_path() {
        // net.ipv4.ip_forward is readable on any Linux host without root;
        // write back whatever value is already there so the test is a no-op
        // on the real system state either way.
        let key = "net.ipv4.ip_forward";
        let current = std::fs::read_to_string("/proc/sys/net/ipv4/ip_forward")
            .expect("read current ip_forward value");
        write_sysctl(key, current.trim()).expect("write_sysctl must write the translated path");
    }

    /// #564: `in_net_ns` entering the CURRENT process's own net namespace
    /// (via `/proc/self/ns/net`) must succeed and run the closure — proves
    /// the thread-based setns plumbing works without needing CAP_NET_ADMIN
    /// to create a brand-new namespace just for the test.
    #[test]
    fn in_net_ns_enters_and_runs_closure() {
        let ran = in_net_ns("/proc/self/ns/net", || Ok(42)).expect("in_net_ns must succeed");
        assert_eq!(ran, 42);
    }

    /// #564: a bad namespace path must surface as an `Err` from the calling
    /// thread (the `open` happens before any thread is spawned).
    #[test]
    fn in_net_ns_rejects_missing_path() {
        let err = in_net_ns("/proc/self/ns/this-does-not-exist-564", || {
            Ok::<(), io::Error>(())
        })
        .expect_err("a nonexistent namespace path must fail to open");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
