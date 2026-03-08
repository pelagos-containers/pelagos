//! Mandatory Access Control (MAC) helpers for AppArmor and SELinux.
//! Used from the pre_exec hook (to apply profiles at exec time) and from
//! tests / CLI code (to detect whether the LSM is active).

/// Returns `true` if AppArmor is loaded and enabled on this host.
///
/// Reads `/sys/module/apparmor/parameters/enabled`; returns `false` on any
/// I/O error (module not loaded, file absent, etc.).
pub fn is_apparmor_enabled() -> bool {
    std::fs::read_to_string("/sys/module/apparmor/parameters/enabled")
        .map(|s| s.trim() == "Y")
        .unwrap_or(false)
}

/// Returns `true` if SELinux is loaded and mounted on this host.
///
/// Checks for the presence of `/sys/fs/selinux/enforce`.
pub fn is_selinux_enabled() -> bool {
    std::path::Path::new("/sys/fs/selinux/enforce").exists()
}

/// Open `/proc/self/attr/apparmor/exec` before chroot/pivot_root and return
/// the file descriptor.  Returns `-1` if AppArmor is not running or the path
/// is inaccessible — the caller treats `-1` as "skip silently".
///
/// # Safety
/// Must be called from a `pre_exec` closure (async-signal-safe context).
/// Uses raw `libc::open`; no allocation.
pub(crate) unsafe fn open_apparmor_exec_attr() -> libc::c_int {
    libc::open(
        c"/proc/self/attr/apparmor/exec".as_ptr(),
        libc::O_WRONLY | libc::O_CLOEXEC,
    )
}

/// Open `/proc/self/attr/exec` (SELinux exec label) before chroot/pivot_root.
/// Returns `-1` if SELinux is not running or the path is inaccessible.
///
/// # Safety
/// Must be called from a `pre_exec` closure (async-signal-safe context).
pub(crate) unsafe fn open_selinux_exec_attr() -> libc::c_int {
    libc::open(
        c"/proc/self/attr/exec".as_ptr(),
        libc::O_WRONLY | libc::O_CLOEXEC,
    )
}

/// Write a MAC label/profile name to the given pre-opened attr fd, then close
/// it.  Returns `Err` if the write fails (e.g. profile not found in kernel).
/// A negative `fd` is treated as "not available" and returns `Ok(())`.
///
/// # Safety
/// Must be called from a `pre_exec` closure.
pub(crate) unsafe fn write_mac_attr(fd: libc::c_int, label: &str) -> std::io::Result<()> {
    if fd < 0 {
        return Ok(());
    }
    let n = libc::write(fd, label.as_ptr() as *const libc::c_void, label.len());
    libc::close(fd);
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
