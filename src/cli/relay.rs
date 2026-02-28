//! Single-thread epoll-based log relay.
//!
//! Replaces the two per-container relay threads with one thread that
//! multiplexes stdout and stderr pipes via epoll(7), reducing the static
//! thread count per container by one (3 → 2 for the common case).

use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::process::{ChildStderr, ChildStdout};

const BUF: usize = 8192;

/// Spawn a single relay thread that copies `stdout` → `stdout_path` and
/// `stderr` → `stderr_path` using one epoll loop.
///
/// The returned `JoinHandle` completes once both pipes reach EOF.
pub fn start_log_relay(
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || relay_loop(stdout, stderr, stdout_path, stderr_path))
}

fn relay_loop(
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
) {
    let epfd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    if epfd < 0 {
        log::warn!(
            "relay: epoll_create1 failed: {}",
            std::io::Error::last_os_error()
        );
        return;
    }

    // Slot 0 = stdout, slot 1 = stderr.
    // done[i] starts true; set to false only when the fd is registered.
    let mut files: [Option<File>; 2] = [None, None];
    let mut done: [bool; 2] = [true, true];
    let mut raw_fds: [i32; 2] = [-1, -1];

    let mut register = |idx: usize, fd: i32, path: &PathBuf| {
        if let Ok(f) = File::create(path) {
            let mut ev = libc::epoll_event {
                events: (libc::EPOLLIN | libc::EPOLLHUP) as u32,
                u64: idx as u64,
            };
            if unsafe { libc::epoll_ctl(epfd, libc::EPOLL_CTL_ADD, fd, &mut ev) } == 0 {
                files[idx] = Some(f);
                done[idx] = false;
                raw_fds[idx] = fd;
            }
        }
    };

    if let Some(ref s) = stdout {
        register(0, s.as_raw_fd(), &stdout_path);
    }
    if let Some(ref s) = stderr {
        register(1, s.as_raw_fd(), &stderr_path);
    }

    let mut buf = [0u8; BUF];
    let mut events = [libc::epoll_event { events: 0, u64: 0 }; 4];

    while !done[0] || !done[1] {
        let n = unsafe { libc::epoll_wait(epfd, events.as_mut_ptr(), 4, -1) };
        if n < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            break;
        }

        for ev in &events[..n as usize] {
            let idx = ev.u64 as usize;
            if done[idx] {
                continue;
            }
            let fd = raw_fds[idx];

            // One read per epoll event; LT will re-notify if more data remains.
            let nread = loop {
                let r = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
                if r >= 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                    break r;
                }
                // EINTR: retry the read
            };

            if nread > 0 {
                if let Some(ref mut f) = files[idx] {
                    let _ = f.write_all(&buf[..nread as usize]);
                }
            } else {
                // EOF (0) or non-EINTR error (-1): deregister and mark done.
                unsafe { libc::epoll_ctl(epfd, libc::EPOLL_CTL_DEL, fd, std::ptr::null_mut()) };
                done[idx] = true;
            }
        }
    }

    unsafe { libc::close(epfd) };
    // stdout / stderr are dropped here, closing the pipe read-ends.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_relay_captures_stdout_and_stderr() {
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("printf 'hello stdout'; printf 'hello stderr' >&2")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn sh");

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let tmp = tempfile::TempDir::new().expect("tempdir");
        let out_path = tmp.path().join("stdout.log");
        let err_path = tmp.path().join("stderr.log");

        let handle = start_log_relay(stdout, stderr, out_path.clone(), err_path.clone());
        child.wait().expect("wait");
        handle.join().expect("join relay thread");

        assert_eq!(
            std::fs::read_to_string(&out_path).unwrap_or_default(),
            "hello stdout"
        );
        assert_eq!(
            std::fs::read_to_string(&err_path).unwrap_or_default(),
            "hello stderr"
        );
    }

    #[test]
    fn test_relay_large_output() {
        // Write more than BUF bytes to exercise multiple read/epoll cycles.
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("yes x | head -c 65536")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn sh");

        let stdout = child.stdout.take();

        let tmp = tempfile::TempDir::new().expect("tempdir");
        let out_path = tmp.path().join("stdout.log");
        let err_path = tmp.path().join("stderr.log");

        let handle = start_log_relay(stdout, None, out_path.clone(), err_path.clone());
        child.wait().expect("wait");
        handle.join().expect("join relay thread");

        let out = std::fs::read(&out_path).unwrap_or_default();
        assert_eq!(out.len(), 65536, "expected 65536 bytes, got {}", out.len());
    }

    #[test]
    fn test_relay_none_handles() {
        // Both None: relay thread should start and exit immediately.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let out_path = tmp.path().join("stdout.log");
        let err_path = tmp.path().join("stderr.log");

        let handle = start_log_relay(None, None, out_path.clone(), err_path.clone());
        handle.join().expect("join relay thread");
        // No log files created — that's fine.
    }
}
