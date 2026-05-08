//! Pre-flight diagnostic for rootless container setup.
//!
//! On Ubuntu 24.04+ (and other distros that enable
//! `kernel.apparmor_restrict_unprivileged_userns=1`), an unprivileged process
//! can call `unshare(CLONE_NEWUSER)` successfully, but the resulting user
//! namespace is stripped of capabilities. Pelagos's single-UID rootless path
//! then fails with `EACCES` on `/proc/self/setgroups` — a confusing failure
//! mode unless you know that AppArmor is the cause.
//!
//! This module probes the host **before** `unshare` is attempted and, if it
//! detects the conjunction of conditions that will trigger the failure, emits a
//! formatted error listing the precise workarounds. The check is purely
//! advisory: if it false-negatives, the underlying kernel call still produces a
//! faithful errno (the masking bug fixed in #219). The escape hatch
//! `PELAGOS_SKIP_ROOTLESS_CHECK=1` bypasses the check entirely.

use std::path::Path;

const APPARMOR_USERNS_PROC: &str = "/proc/sys/kernel/apparmor_restrict_unprivileged_userns";
const SKIP_ENV: &str = "PELAGOS_SKIP_ROOTLESS_CHECK";

/// Host signals relevant to rootless setup. Pure data so `diagnose` is
/// testable without touching the filesystem.
#[derive(Debug, Clone)]
pub struct Signals {
    /// AppArmor unprivileged-userns restriction is active
    /// (`/proc/sys/kernel/apparmor_restrict_unprivileged_userns == 1`).
    pub apparmor_restricted: bool,
    /// `newuidmap` binary present on PATH.
    pub has_newuidmap: bool,
    /// `newgidmap` binary present on PATH.
    pub has_newgidmap: bool,
    /// Effective GID matches the calling user's `pw_gid` from `/etc/passwd`
    /// (newuidmap/newgidmap reject mismatched callers — common gotcha on
    /// `newgrp <group>` shells and domain-joined hosts).
    pub egid_matches_pw_gid: bool,
    /// Number of `/etc/subuid` ranges allocated to the calling user.
    pub subuid_entries: usize,
    /// Number of `/etc/subgid` ranges allocated to the calling user.
    pub subgid_entries: usize,
    /// Calling user's name from `/etc/passwd`, if discoverable.
    pub username: Option<String>,
}

impl Signals {
    /// Probe the live host for all relevant signals.
    pub fn probe() -> Self {
        let apparmor_restricted = read_apparmor_userns_flag();
        let has_newuidmap = crate::idmap::has_newuidmap();
        let has_newgidmap = crate::idmap::has_newgidmap();
        let egid_matches_pw_gid = crate::idmap::newuidmap_will_work();

        let (username, subuid_entries, subgid_entries) = match crate::idmap::current_user_info() {
            Ok((name, _pw_gid)) => {
                let host_uid = unsafe { libc::getuid() };
                let host_gid = unsafe { libc::getgid() };
                let subuid =
                    crate::idmap::parse_subid_file(Path::new("/etc/subuid"), &name, host_uid)
                        .map(|v| v.len())
                        .unwrap_or(0);
                let subgid =
                    crate::idmap::parse_subid_file(Path::new("/etc/subgid"), &name, host_gid)
                        .map(|v| v.len())
                        .unwrap_or(0);
                (Some(name), subuid, subgid)
            }
            Err(_) => (None, 0, 0),
        };

        Signals {
            apparmor_restricted,
            has_newuidmap,
            has_newgidmap,
            egid_matches_pw_gid,
            subuid_entries,
            subgid_entries,
            username,
        }
    }

    /// True when the newuidmap path will run successfully end-to-end. When this
    /// is true, pelagos never writes `/proc/self/setgroups` directly, so the
    /// AppArmor restriction is irrelevant.
    fn newuidmap_path_works(&self) -> bool {
        self.has_newuidmap
            && self.has_newgidmap
            && self.egid_matches_pw_gid
            && self.subuid_entries > 0
            && self.subgid_entries > 0
    }
}

/// Diagnostic returned when rootless setup is predicted to fail. Its `Display`
/// is the user-facing error text.
#[derive(Debug, Clone)]
pub struct RootlessSetupError {
    pub signals: Signals,
}

impl std::fmt::Display for RootlessSetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = &self.signals;
        let user = s.username.as_deref().unwrap_or("$USER");

        writeln!(
            f,
            "rootless container setup is not supported in this environment."
        )?;
        writeln!(f)?;
        writeln!(f, "Detected:")?;
        if s.apparmor_restricted {
            writeln!(
                f,
                "  • kernel.apparmor_restrict_unprivileged_userns = 1 \
                 (Ubuntu 24.04+ default)"
            )?;
        }
        if !s.has_newuidmap || !s.has_newgidmap {
            writeln!(
                f,
                "  • uidmap package not installed (newuidmap/newgidmap missing)"
            )?;
        }
        if s.subuid_entries == 0 {
            writeln!(f, "  • no /etc/subuid entry for {}", user)?;
        }
        if s.subgid_entries == 0 {
            writeln!(f, "  • no /etc/subgid entry for {}", user)?;
        }
        // Only fire the egid line when the rest of the newuidmap path would
        // actually run — otherwise it's noise (you can't hit the egid check
        // until helpers + subid entries exist).
        if s.has_newuidmap
            && s.has_newgidmap
            && s.subuid_entries > 0
            && s.subgid_entries > 0
            && !s.egid_matches_pw_gid
        {
            writeln!(
                f,
                "  • effective GID does not match passwd pw_gid \
                 (e.g. running under `newgrp` or with a non-default primary group)"
            )?;
        }
        writeln!(f)?;
        writeln!(f, "Pick one of the following:")?;
        writeln!(f)?;
        writeln!(f, "  1. Disable the AppArmor userns restriction:")?;
        writeln!(
            f,
            "       sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0"
        )?;
        writeln!(
            f,
            "       echo 'kernel.apparmor_restrict_unprivileged_userns=0' \\"
        )?;
        writeln!(f, "         | sudo tee /etc/sysctl.d/60-pelagos.conf")?;
        writeln!(f)?;
        writeln!(f, "  2. Install uidmap and configure subordinate UID/GID:")?;
        writeln!(f, "       sudo apt install uidmap")?;
        writeln!(
            f,
            "       sudo usermod --add-subuids 100000-165535 {}",
            user
        )?;
        writeln!(
            f,
            "       sudo usermod --add-subgids 100000-165535 {}",
            user
        )?;
        writeln!(f)?;
        writeln!(f, "  3. Run as root:  sudo -E pelagos run …")?;
        writeln!(f)?;
        write!(
            f,
            "See docs/USER_GUIDE.md → Troubleshooting for details. \
             Set PELAGOS_SKIP_ROOTLESS_CHECK=1 to bypass this check."
        )
    }
}

impl std::error::Error for RootlessSetupError {}

/// Pure decision: given a set of signals, return an error iff rootless setup
/// is predicted to fail. The conjunction is:
/// `apparmor_restricted AND NOT newuidmap_path_works`.
pub fn diagnose(s: &Signals) -> Option<RootlessSetupError> {
    if s.apparmor_restricted && !s.newuidmap_path_works() {
        Some(RootlessSetupError { signals: s.clone() })
    } else {
        None
    }
}

/// Probe the host and return an error if rootless setup is predicted to fail.
/// Honours `PELAGOS_SKIP_ROOTLESS_CHECK=1`.
pub fn check() -> Result<(), RootlessSetupError> {
    if std::env::var_os(SKIP_ENV).is_some() {
        log::debug!("rootless_check: bypassed via {}", SKIP_ENV);
        return Ok(());
    }
    match diagnose(&Signals::probe()) {
        None => Ok(()),
        Some(e) => Err(e),
    }
}

/// Read `/proc/sys/kernel/apparmor_restrict_unprivileged_userns`. File absent
/// (non-Ubuntu kernels, or the LSM not loaded) is treated as `0`.
fn read_apparmor_userns_flag() -> bool {
    std::fs::read_to_string(APPARMOR_USERNS_PROC)
        .map(|s| s.trim() == "1")
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signals(
        apparmor: bool,
        nuid: bool,
        ngid: bool,
        egid_ok: bool,
        subuid: usize,
        subgid: usize,
    ) -> Signals {
        Signals {
            apparmor_restricted: apparmor,
            has_newuidmap: nuid,
            has_newgidmap: ngid,
            egid_matches_pw_gid: egid_ok,
            subuid_entries: subuid,
            subgid_entries: subgid,
            username: Some("alice".into()),
        }
    }

    #[test]
    fn no_apparmor_restriction_means_no_error() {
        // AppArmor unrestricted: even with everything else broken, we let the
        // kernel decide. (In practice newuidmap_will_work etc. matter, but the
        // EACCES-on-setgroups failure that this check exists to catch only
        // happens when AppArmor restricts the userns.)
        let s = signals(false, false, false, false, 0, 0);
        assert!(diagnose(&s).is_none());
    }

    #[test]
    fn apparmor_with_working_newuidmap_path_is_silent() {
        // AppArmor on, but newuidmap will run end-to-end → we never hit
        // the setgroups write → no diagnostic.
        let s = signals(true, true, true, true, 1, 1);
        assert!(diagnose(&s).is_none());
    }

    #[test]
    fn apparmor_plus_no_newuidmap_binary_trips_check() {
        let s = signals(true, false, false, true, 1, 1);
        let err = diagnose(&s).expect("should diagnose");
        let text = format!("{}", err);
        assert!(text.contains("apparmor_restrict_unprivileged_userns"));
        assert!(text.contains("uidmap package not installed"));
    }

    #[test]
    fn apparmor_plus_no_subuid_entry_trips_check() {
        let s = signals(true, true, true, true, 0, 1);
        let err = diagnose(&s).expect("should diagnose");
        let text = format!("{}", err);
        assert!(text.contains("no /etc/subuid entry for alice"));
        // subgid line must NOT fire when subgid is fine
        assert!(!text.contains("no /etc/subgid entry"));
    }

    #[test]
    fn apparmor_plus_no_subgid_entry_trips_check() {
        let s = signals(true, true, true, true, 1, 0);
        let err = diagnose(&s).expect("should diagnose");
        let text = format!("{}", err);
        assert!(text.contains("no /etc/subgid entry for alice"));
    }

    #[test]
    fn apparmor_plus_egid_mismatch_trips_check() {
        // Helpers present, subids configured, but egid != pw_gid (newgrp shell
        // or domain primary group)
        let s = signals(true, true, true, false, 1, 1);
        let err = diagnose(&s).expect("should diagnose");
        let text = format!("{}", err);
        assert!(text.contains("effective GID does not match passwd pw_gid"));
    }

    #[test]
    fn full_diagnostic_lists_every_blocker() {
        let s = signals(true, false, false, false, 0, 0);
        let err = diagnose(&s).expect("should diagnose");
        let text = format!("{}", err);
        for needle in [
            "apparmor_restrict_unprivileged_userns",
            "uidmap package not installed",
            "no /etc/subuid entry for alice",
            "no /etc/subgid entry for alice",
        ] {
            assert!(text.contains(needle), "missing {:?} in:\n{}", needle, text);
        }
        // egid line should NOT fire when uidmap helpers are missing
        // (the user can't even reach the egid check in that case)
        assert!(!text.contains("effective GID does not match"));
    }

    #[test]
    fn display_falls_back_to_user_placeholder() {
        let mut s = signals(true, true, true, true, 0, 0);
        s.username = None;
        let err = diagnose(&s).expect("should diagnose");
        let text = format!("{}", err);
        // No real username known → use $USER placeholder in fix instructions
        assert!(text.contains("$USER"));
    }

    #[test]
    fn display_includes_all_three_workarounds_in_order() {
        let s = signals(true, false, false, false, 0, 0);
        let text = format!("{}", diagnose(&s).expect("should diagnose"));
        let p1 = text.find("1. Disable the AppArmor").expect("workaround 1");
        let p2 = text.find("2. Install uidmap").expect("workaround 2");
        let p3 = text.find("3. Run as root").expect("workaround 3");
        assert!(p1 < p2 && p2 < p3, "workarounds out of order");
        assert!(text.contains("PELAGOS_SKIP_ROOTLESS_CHECK"));
    }
}
