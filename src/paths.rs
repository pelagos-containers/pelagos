//! Centralised path resolution for all Pelagos filesystem locations.
//!
//! **Data (images, layers, volumes):** shared between root and rootless via
//! `/var/lib/pelagos/` (pelagos group, mode 2775).  Non-root users in the
//! `pelagos` group can pull and read images alongside root.
//!
//! **Runtime (containers, networking, DNS, compose):** per-UID.  Root uses
//! `/run/pelagos/`; rootless uses `$XDG_RUNTIME_DIR/pelagos/`.  Execution is
//! a security boundary — non-root users cannot see root's running containers,
//! matching the Podman/containerd model.

use std::path::PathBuf;

/// Returns `true` when running as a non-root user.
pub fn is_rootless() -> bool {
    unsafe { libc::getuid() != 0 }
}

/// Pelagos config file.
///
/// - If `$XDG_CONFIG_HOME` is set (any UID): `$XDG_CONFIG_HOME/pelagos/config.toml`
/// - Rootless (no `$XDG_CONFIG_HOME`): `~/.config/pelagos/config.toml`
/// - Root (no `$XDG_CONFIG_HOME`): `/etc/pelagos/config.toml`
pub fn config_file() -> PathBuf {
    // XDG_CONFIG_HOME takes priority for any UID — useful for testing and
    // for users who want an explicit override.
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("pelagos/config.toml");
        }
    }
    if is_rootless() {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(".config/pelagos/config.toml");
        }
    }
    PathBuf::from("/etc/pelagos/config.toml")
}

/// Persistent data directory.
///
/// - Root (or system store already initialised): `/var/lib/pelagos/`
/// - Rootless with no system store: `$XDG_DATA_HOME/pelagos/` (default `~/.local/share/pelagos/`)
///
/// If `/var/lib/pelagos/` already exists we always use it, regardless of the
/// current UID.  This means a non-root user can pull images into the same
/// store that `sudo pelagos` uses, once root has initialised the directory
/// (which happens automatically on the first root pull/run).
pub fn data_dir() -> PathBuf {
    let system_dir = PathBuf::from("/var/lib/pelagos");
    // Use the system store if it already exists OR if we are root.
    if system_dir.exists() || !is_rootless() {
        return system_dir;
    }
    // Pure rootless: system store has never been initialised, use XDG dir.
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("pelagos");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".local/share/pelagos");
    }
    // Last resort: /tmp fallback (unlikely on any real system).
    PathBuf::from(format!("/tmp/pelagos-data-{}", unsafe { libc::getuid() }))
}

/// Ephemeral runtime directory (per-UID).
///
/// - Root: `/run/pelagos/`
/// - Rootless: `$XDG_RUNTIME_DIR/pelagos/` (fallback `/tmp/pelagos-<uid>/`, mode 0700)
///
/// Unlike `data_dir()`, the runtime dir is **never** shared between root and
/// non-root users.  Execution is a security boundary: a non-root user must not
/// be able to inspect, stop, or interact with containers started by root.
/// This matches the Podman/containerd model.
pub fn runtime_dir() -> PathBuf {
    if !is_rootless() {
        return PathBuf::from("/run/pelagos");
    }
    // Rootless: always use the per-user XDG runtime dir.
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("pelagos");
        }
    }
    let uid = unsafe { libc::getuid() };
    let fallback = PathBuf::from(format!("/tmp/pelagos-{}", uid));
    if !fallback.exists() {
        let _ = std::fs::create_dir_all(&fallback);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&fallback, std::fs::Permissions::from_mode(0o700));
        }
    }
    fallback
}

// ── Derived from data_dir() ─────────────────────────────────────────────────

/// Directory for OCI image manifests: `<data>/images/`.
pub fn images_dir() -> PathBuf {
    data_dir().join("images")
}

/// Content-addressable layer store: `<data>/layers/`.
pub fn layers_dir() -> PathBuf {
    data_dir().join("layers")
}

/// Named volumes: `<data>/volumes/`.
pub fn volumes_dir() -> PathBuf {
    data_dir().join("volumes")
}

/// Imported rootfs store: `<data>/rootfs/`.
pub fn rootfs_store_dir() -> PathBuf {
    data_dir().join("rootfs")
}

/// Auto-incrementing container name counter file: `<runtime>/container_counter`.
///
/// This is ephemeral per-user state (container naming), not persistent data, so
/// it lives in the runtime dir. Rootless users write to their own runtime dir
/// (`$XDG_RUNTIME_DIR/pelagos/` or `/tmp/pelagos-<uid>/`); root uses `/run/pelagos/`.
pub fn counter_file() -> PathBuf {
    runtime_dir().join("container_counter")
}

/// Build cache directory: `<data>/build-cache/`.
pub fn build_cache_dir() -> PathBuf {
    data_dir().join("build-cache")
}

/// Raw compressed blob store: `<data>/blobs/`.
///
/// Stores the original `.tar.gz` bytes for each layer, keyed by digest.
/// Required for `pelagos image push`.
pub fn blobs_dir() -> PathBuf {
    data_dir().join("blobs")
}

/// Path for a single blob: `<data>/blobs/<hex>.tar.gz`.
///
/// `digest` may include the `sha256:` prefix or be a bare hex string.
pub fn blob_path(digest: &str) -> PathBuf {
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    blobs_dir().join(format!("{}.tar.gz", hex))
}

/// Sidecar file storing the uncompressed-tar `diff_id` for a given blob digest.
///
/// Path: `<data>/blobs/<hex>.diffid`
pub fn blob_diffid_path(digest: &str) -> PathBuf {
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    blobs_dir().join(format!("{}.diffid", hex))
}

// ── Derived from runtime_dir() ──────────────────────────────────────────────

/// Per-container state directories: `<runtime>/containers/`.
pub fn containers_dir() -> PathBuf {
    runtime_dir().join("containers")
}

/// OCI runtime state directory: `<runtime>/<id>/`.
pub fn oci_state_dir(id: &str) -> PathBuf {
    runtime_dir().join(id)
}

/// Per-UID **disk-backed** scratch root for container overlays.
///
/// Unlike `data_dir()` (which is shared once `/var/lib/pelagos` exists, so a
/// non-root user can read the same image store as root), the overlay *scratch*
/// is a per-UID execution boundary — like `runtime_dir()`, but on **disk**
/// instead of the RAM-backed `/run` tmpfs. The container's writable layer is
/// therefore bounded by disk (not RAM) and can't OOM the node. This matches
/// docker/containerd/podman, which all keep the writable layer on disk.
///
/// - Root: `/var/lib/pelagos/scratch/`
/// - Rootless: `$XDG_DATA_HOME/pelagos/scratch/` (default `~/.local/share/pelagos/scratch/`)
///
/// The scratch root is created mode 0700 (the per-UID boundary); individual
/// `overlay-<pid>-<n>` dirs inside stay 0755 (the kernel checks overlay
/// upper/work perms against the post-`setuid` fsuid), gated by the 0700 parent.
pub fn scratch_root() -> PathBuf {
    if !is_rootless() {
        return PathBuf::from("/var/lib/pelagos/scratch");
    }
    // Rootless: always the per-user disk dir (NOT the shared /var/lib/pelagos),
    // because scratch must be writable by — and isolated to — the running user.
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("pelagos/scratch");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".local/share/pelagos/scratch");
    }
    PathBuf::from(format!("/tmp/pelagos-scratch-{}", unsafe {
        libc::getuid()
    }))
}

/// Whether the overlay scratch should use the RAM-backed tmpfs (`runtime_dir()`)
/// instead of the disk default. Global opt-in via `PELAGOS_OVERLAY_TMPFS=1`;
/// the per-command `--overlay-tmpfs` flag is threaded in as the `tmpfs` arg.
fn overlay_tmpfs_env() -> bool {
    matches!(
        std::env::var("PELAGOS_OVERLAY_TMPFS").ok().as_deref(),
        Some("1") | Some("true")
    )
}

/// Overlay scratch directory for a container (holds `upper/`, `work/`, `merged/`).
///
/// **Defaults to disk** (`scratch_root()/overlay-<pid>-<n>`). Opt into the
/// RAM-backed tmpfs with `tmpfs = true` or `PELAGOS_OVERLAY_TMPFS=1`; or point it
/// anywhere with `PELAGOS_OVERLAY_DIR` (highest precedence).
pub fn overlay_scratch_base(pid: i32, n: u32, tmpfs: bool) -> PathBuf {
    let name = format!("overlay-{}-{}", pid, n);
    if let Ok(dir) = std::env::var("PELAGOS_OVERLAY_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join(name);
        }
    }
    if tmpfs || overlay_tmpfs_env() {
        return runtime_dir().join(name);
    }
    scratch_root().join(name)
}

/// Back-compat wrapper — overlay scratch on the **disk** default.
/// Prefer `overlay_scratch_base(pid, n, tmpfs)` at call sites that know the mode.
pub fn overlay_base(pid: i32, n: u32) -> PathBuf {
    overlay_scratch_base(pid, n, false)
}

/// DNS temp directory: `<runtime>/dns-<pid>-<n>/`.
pub fn dns_dir(pid: i32, n: u32) -> PathBuf {
    runtime_dir().join(format!("dns-{}-{}", pid, n))
}

/// Hosts temp directory: `<runtime>/hosts-<pid>-<n>/`.
pub fn hosts_dir(pid: i32, n: u32) -> PathBuf {
    runtime_dir().join(format!("hosts-{}-{}", pid, n))
}

/// IPAM next-IP file: `<runtime>/next_ip`.
pub fn ipam_file() -> PathBuf {
    runtime_dir().join("next_ip")
}

/// NAT reference count file: `<runtime>/nat_refcount`.
pub fn nat_refcount_file() -> PathBuf {
    runtime_dir().join("nat_refcount")
}

/// Port-forward entries file: `<runtime>/port_forwards`.
pub fn port_forwards_file() -> PathBuf {
    runtime_dir().join("port_forwards")
}

// ── DNS daemon paths ─────────────────────────────────────────────────────────

/// DNS daemon config directory: `<runtime>/dns/`.
pub fn dns_config_dir() -> PathBuf {
    runtime_dir().join("dns")
}

/// DNS daemon PID file: `<runtime>/dns/pid`.
pub fn dns_pid_file() -> PathBuf {
    dns_config_dir().join("pid")
}

/// Per-network DNS config file: `<runtime>/dns/<network_name>`.
pub fn dns_network_file(name: &str) -> PathBuf {
    dns_config_dir().join(name)
}

/// DNS backend marker file: `<runtime>/dns/backend`.
pub fn dns_backend_file() -> PathBuf {
    dns_config_dir().join("backend")
}

/// dnsmasq generated config: `<runtime>/dns/dnsmasq.conf`.
pub fn dns_dnsmasq_conf() -> PathBuf {
    dns_config_dir().join("dnsmasq.conf")
}

/// Per-network hosts file for dnsmasq: `<runtime>/dns/hosts.<network>`.
pub fn dns_hosts_file(network_name: &str) -> PathBuf {
    dns_config_dir().join(format!("hosts.{}", network_name))
}

// ── Compose directories ─────────────────────────────────────────────────────

/// Compose project root: `<runtime>/compose/`.
pub fn compose_dir() -> PathBuf {
    runtime_dir().join("compose")
}

/// Compose project directory: `<runtime>/compose/<project>/`.
pub fn compose_project_dir(project: &str) -> PathBuf {
    compose_dir().join(project)
}

/// Compose project state file: `<runtime>/compose/<project>/state.json`.
pub fn compose_state_file(project: &str) -> PathBuf {
    compose_project_dir(project).join("state.json")
}

// ── Per-network directories ─────────────────────────────────────────────────

/// Persistent config directory for all named networks: `<data>/networks/`.
pub fn networks_config_dir() -> PathBuf {
    data_dir().join("networks")
}

/// Config directory for a specific network: `<data>/networks/<name>/`.
pub fn network_config_dir(name: &str) -> PathBuf {
    networks_config_dir().join(name)
}

/// Runtime state directory for a specific network: `<runtime>/networks/<name>/`.
pub fn network_runtime_dir(name: &str) -> PathBuf {
    runtime_dir().join("networks").join(name)
}

/// Per-network IPAM next-IP file: `<runtime>/networks/<name>/next_ip`.
pub fn network_ipam_file(name: &str) -> PathBuf {
    network_runtime_dir(name).join("next_ip")
}

/// Per-network NAT refcount file: `<runtime>/networks/<name>/nat_refcount`.
pub fn network_nat_refcount_file(name: &str) -> PathBuf {
    network_runtime_dir(name).join("nat_refcount")
}

/// Per-network port-forward entries file: `<runtime>/networks/<name>/port_forwards`.
pub fn network_port_forwards_file(name: &str) -> PathBuf {
    network_runtime_dir(name).join("port_forwards")
}

/// Per-network IPv6 IPAM counter file: `<runtime>/networks/<name>/next_ipv6`.
pub fn network_ipv6_ipam_file(name: &str) -> PathBuf {
    network_runtime_dir(name).join("next_ipv6")
}
// ── Sandbox directories ──────────────────────────────────────────────────────

/// Parent directory for all sandbox state: `<runtime>/sandboxes/`.
pub fn sandboxes_dir() -> PathBuf {
    runtime_dir().join("sandboxes")
}

/// State directory for a specific sandbox: `<runtime>/sandboxes/<id>/`.
pub fn sandbox_dir(id: &str) -> PathBuf {
    sandboxes_dir().join(id)
}

/// PID file for a sandbox's pause process: `<runtime>/sandboxes/<id>/pause.pid`.
pub fn sandbox_pid_file(id: &str) -> PathBuf {
    sandbox_dir(id).join("pause.pid")
}

/// Named network namespace name file: `<runtime>/sandboxes/<id>/ns_name`.
///
/// Contains the `/run/netns/<name>` namespace name used at teardown.
pub fn sandbox_ns_name_file(id: &str) -> PathBuf {
    sandbox_dir(id).join("ns_name")
}

/// Optional human-readable name file: `<runtime>/sandboxes/<id>/name`.
pub fn sandbox_name_file(id: &str) -> PathBuf {
    sandbox_dir(id).join("name")
}

// ── Install invariant checker ────────────────────────────────────────────────

/// A single invariant violation found by [`validate_install`].
#[derive(Debug)]
pub struct InstallIssue {
    pub path: std::path::PathBuf,
    pub message: String,
}

impl std::fmt::Display for InstallIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.message)
    }
}

/// Check that the pelagos data and runtime directories exist with the expected
/// ownership and permissions.  Returns a list of issues; empty means all good.
///
/// Called at startup (after `Cli::parse()`) so that missing-dir failures surface
/// as a clear "run sudo scripts/setup.sh" message rather than a cryptic ENOENT
/// later in a subcommand.
///
/// Only checks directories that are relevant to the current UID:
/// - Root: data dir + all subdirs + runtime dir.
/// - Rootless (system store present): checks readability of the dirs it needs.
pub fn validate_install() -> Vec<InstallIssue> {
    use std::os::unix::fs::MetadataExt;

    let mut issues = Vec::new();

    // Look up the "pelagos" group GID once.
    let pelagos_gid: Option<u32> = {
        // SAFETY: getgrnam_r would be safer but nix covers it; this is a
        // read-only lookup called before any threads are spawned.
        let name = std::ffi::CString::new("pelagos").unwrap();
        let grp = unsafe { libc::getgrnam(name.as_ptr()) };
        if grp.is_null() {
            None
        } else {
            Some(unsafe { (*grp).gr_gid })
        }
    };

    // ── Data directory ────────────────────────────────────────────────────────

    let data = data_dir();

    // If the system data dir doesn't exist at all, this is a fresh machine or
    // a pure-rootless environment — not a misconfiguration.  Commands that need
    // the dir will create it or fail with a clear ENOENT.  Only validate when
    // the dir already exists (i.e. setup.sh has been run at least once).
    if !data.exists() {
        return issues;
    }

    // Subdirectories that must exist, with expected (owner_uid, group_gid, min_mode).
    // gid=None means we don't enforce the group (e.g. root:root dirs).
    // The mode is a minimum — extra bits are fine.
    struct DirSpec {
        name: &'static str,
        expected_uid: u32,
        expected_gid: Option<u32>, // None = pelagos group, Some(x) = exact gid
        min_mode: u32,
    }

    let group_writable_dirs = ["images", "layers", "blobs", "build-cache"];
    let root_only_dirs = ["volumes", "networks", "rootfs"];

    let dir_specs: Vec<DirSpec> = group_writable_dirs
        .iter()
        .map(|&name| DirSpec {
            name,
            expected_uid: 0,
            expected_gid: None, // pelagos group
            min_mode: 0o2775,
        })
        .chain(root_only_dirs.iter().map(|&name| DirSpec {
            name,
            expected_uid: 0,
            expected_gid: Some(0), // root:root
            min_mode: 0o755,
        }))
        .collect();

    for spec in &dir_specs {
        let path = data.join(spec.name);
        match std::fs::metadata(&path) {
            Err(_) => {
                issues.push(InstallIssue {
                    path,
                    message: "does not exist — run: sudo scripts/setup.sh".into(),
                });
            }
            Ok(meta) => {
                let mode = meta.mode() & 0o7777;
                let uid = meta.uid();
                let gid = meta.gid();
                let expected_gid = spec
                    .expected_gid
                    .unwrap_or_else(|| pelagos_gid.unwrap_or(u32::MAX));

                if uid != spec.expected_uid {
                    issues.push(InstallIssue {
                        path: path.clone(),
                        message: format!(
                            "owned by uid {} (expected 0) — run: sudo scripts/setup.sh",
                            uid
                        ),
                    });
                }
                if pelagos_gid.is_some() && gid != expected_gid {
                    issues.push(InstallIssue {
                        path: path.clone(),
                        message: format!(
                            "group gid {} (expected {}) — run: sudo scripts/setup.sh",
                            gid, expected_gid
                        ),
                    });
                }
                if mode & spec.min_mode != spec.min_mode {
                    issues.push(InstallIssue {
                        path,
                        message: format!(
                            "mode {:04o} (expected at least {:04o}) — run: sudo scripts/setup.sh",
                            mode, spec.min_mode
                        ),
                    });
                }
            }
        }
    }

    // The runtime dir (/run/pelagos) is intentionally not checked here.
    // It is ephemeral (tmpfs, cleared on reboot) and its subdirs (containers/,
    // overlay-*, dns-*) are created lazily on first use.  Checking for them
    // at startup is fragile and produces circular errors ("run pelagos run
    // to create /run/pelagos/containers" blocks the very command that creates it).

    let _ = dir_specs;
    issues
}

// ── Host-destructive-removal guard (issue #347) ─────────────────────────────

/// Lexically normalize an absolute path: collapse `.`, `..`, and repeated
/// separators WITHOUT touching the filesystem (no symlink resolution, no
/// existence requirement). Returns `None` for a non-absolute path.
fn normalize_abs(path: &std::path::Path) -> Option<PathBuf> {
    use std::path::Component;
    if !path.is_absolute() {
        return None;
    }
    let mut out = PathBuf::from("/");
    for comp in path.components() {
        match comp {
            Component::RootDir | Component::Prefix(_) | Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(c) => out.push(c),
        }
    }
    Some(out)
}

/// Directories pelagos manages but must NEVER remove wholesale — removing one of
/// these would wipe every container/sandbox/image/layer/volume at once (e.g. an
/// empty container name making `containers_dir().join("")` resolve to
/// `containers_dir()` itself).
fn protected_parent_dirs() -> Vec<PathBuf> {
    [
        containers_dir(),
        sandboxes_dir(),
        images_dir(),
        layers_dir(),
        volumes_dir(),
        runtime_dir(),
        data_dir(),
    ]
    .into_iter()
    .filter_map(|p| normalize_abs(&p))
    .collect()
}

/// True iff `path` is safe for pelagos to recursively remove: an absolute,
/// lexically-normalized path that is a **strict descendant** of a managed root
/// (`/run/pelagos`, `/var/lib/pelagos`, or the rootless equivalents) and is not
/// one of the wholesale parent dirs.
///
/// Guards against issue #347 — a host-destructive bug where an inconsistent
/// ("phantom") sandbox/container yielded an empty or absolute base path that
/// `PathBuf::join` resolved OUTSIDE pelagos's directories (e.g. to the host
/// `/bin` symlink), and a teardown `remove_dir_all`/unlink then deleted it.
pub fn is_safe_to_remove(path: &std::path::Path) -> bool {
    let Some(norm) = normalize_abs(path) else {
        return false;
    };
    // Never a managed parent dir itself.
    if protected_parent_dirs().contains(&norm) {
        return false;
    }
    // Must be a strict descendant of a managed root.
    [runtime_dir(), data_dir()]
        .iter()
        .filter_map(|r| normalize_abs(r))
        .any(|root| norm.starts_with(&root) && norm != root)
}

/// `remove_dir_all`, but refuses (logs an error and returns `Ok`) for any path
/// not strictly under a pelagos-managed root — a belt-and-suspenders guard
/// against host-destructive removals (#347). `NotFound` is treated as success.
pub fn guarded_remove_dir_all(path: &std::path::Path) -> std::io::Result<()> {
    if !is_safe_to_remove(path) {
        log::error!(
            "refusing to remove path outside pelagos-managed dirs: {} (#347 guard)",
            path.display()
        );
        return Ok(());
    }
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// `remove_file`/unlink with the same managed-root guard as
/// [`guarded_remove_dir_all`].
pub fn guarded_remove_file(path: &std::path::Path) -> std::io::Result<()> {
    if !is_safe_to_remove(path) {
        log::error!(
            "refusing to unlink path outside pelagos-managed dirs: {} (#347 guard)",
            path.display()
        );
        return Ok(());
    }
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_guard_rejects_host_and_root_paths() {
        // Host system paths — the #347 destruction targets.
        assert!(!is_safe_to_remove(std::path::Path::new("/")));
        assert!(!is_safe_to_remove(std::path::Path::new("/bin")));
        assert!(!is_safe_to_remove(std::path::Path::new("/usr/bin")));
        assert!(!is_safe_to_remove(std::path::Path::new("/etc")));
        // Non-absolute / empty.
        assert!(!is_safe_to_remove(std::path::Path::new("")));
        assert!(!is_safe_to_remove(std::path::Path::new("relative/path")));
        // The managed roots and wholesale parent dirs themselves.
        assert!(!is_safe_to_remove(&runtime_dir()));
        assert!(!is_safe_to_remove(&data_dir()));
        assert!(!is_safe_to_remove(&containers_dir()));
        assert!(!is_safe_to_remove(&sandboxes_dir()));
        assert!(!is_safe_to_remove(&images_dir()));
        assert!(!is_safe_to_remove(&layers_dir()));
    }

    #[test]
    fn test_guard_allows_managed_subpaths() {
        assert!(is_safe_to_remove(&containers_dir().join("pcri-abc123")));
        assert!(is_safe_to_remove(&sandbox_dir("0123456789abcdef")));
        assert!(is_safe_to_remove(&runtime_dir().join("overlay-1234-5")));
        assert!(is_safe_to_remove(&images_dir().join("alpine_latest")));
        assert!(is_safe_to_remove(&layers_dir().join("deadbeef")));
    }

    #[test]
    fn test_guarded_remove_refuses_unmanaged_dir() {
        // A real directory OUTSIDE pelagos's managed roots must NOT be removed by
        // the guarded wrapper — it returns Ok (refusal logged) and the dir
        // survives. This is the functional proof that the #347 guard prevents
        // host-destructive removals.
        let tmp = tempfile::tempdir().unwrap();
        let victim = tmp.path().join("not-pelagos");
        std::fs::create_dir_all(&victim).unwrap();
        assert!(!is_safe_to_remove(&victim));
        guarded_remove_dir_all(&victim).unwrap();
        assert!(victim.exists(), "guard must not remove an unmanaged dir");
    }

    #[test]
    fn test_guard_blocks_join_and_traversal_escapes() {
        // PathBuf::join with an absolute component replaces the base -> escapes.
        assert!(!is_safe_to_remove(&containers_dir().join("/bin")));
        assert!(!is_safe_to_remove(&sandbox_dir("/bin")));
        // `..` traversal out of the managed root.
        assert!(!is_safe_to_remove(
            &containers_dir().join("../../../../bin")
        ));
        // Empty id collapses to the protected parent dir.
        assert!(!is_safe_to_remove(&containers_dir().join("")));
        assert!(!is_safe_to_remove(&sandbox_dir("")));
    }

    #[test]
    fn test_is_rootless_returns_bool() {
        // Just verify it doesn't panic. The actual value depends on who runs the test.
        let _ = is_rootless();
    }

    #[test]
    fn test_data_dir_is_absolute() {
        assert!(data_dir().is_absolute());
    }

    #[test]
    fn test_runtime_dir_is_absolute() {
        assert!(runtime_dir().is_absolute());
    }

    #[test]
    fn test_derived_paths_under_data_dir() {
        let data = data_dir();
        assert!(images_dir().starts_with(&data));
        assert!(layers_dir().starts_with(&data));
        assert!(volumes_dir().starts_with(&data));
        assert!(rootfs_store_dir().starts_with(&data));
        assert!(blobs_dir().starts_with(&data));
    }

    #[test]
    fn test_blob_path() {
        let p = blob_path("sha256:abc123");
        assert_eq!(p, blobs_dir().join("abc123.tar.gz"));
        let p2 = blob_path("abc123");
        assert_eq!(p2, blobs_dir().join("abc123.tar.gz"));
    }

    #[test]
    fn test_derived_paths_under_runtime_dir() {
        let rt = runtime_dir();
        assert!(containers_dir().starts_with(&rt));
        assert!(oci_state_dir("test").starts_with(&rt));
        // Overlay in tmpfs mode still lives under the runtime tmpfs dir.
        assert!(overlay_scratch_base(1, 0, true).starts_with(&rt));
        assert!(dns_dir(1, 0).starts_with(&rt));
        assert!(hosts_dir(1, 0).starts_with(&rt));
        assert!(ipam_file().starts_with(&rt));
        assert!(nat_refcount_file().starts_with(&rt));
        assert!(port_forwards_file().starts_with(&rt));
        assert!(counter_file().starts_with(&rt));
    }

    #[test]
    fn test_overlay_scratch_disk_default_tmpfs_optin() {
        // Guard against a stray override leaking in from the environment.
        if std::env::var("PELAGOS_OVERLAY_DIR").is_ok()
            || std::env::var("PELAGOS_OVERLAY_TMPFS").is_ok()
        {
            return;
        }
        // Default (tmpfs=false) → disk scratch root, NOT the /run tmpfs.
        let disk = overlay_scratch_base(7, 3, false);
        assert!(disk.starts_with(scratch_root()));
        assert!(!disk.starts_with(runtime_dir()));
        // The back-compat wrapper defaults to disk too.
        assert!(overlay_base(7, 3).starts_with(scratch_root()));
        // Opt-in (tmpfs=true) → the runtime tmpfs.
        assert!(overlay_scratch_base(7, 3, true).starts_with(runtime_dir()));
        // Disk scratch must be a distinct location from the tmpfs runtime dir.
        assert_ne!(scratch_root(), runtime_dir());
    }

    #[test]
    fn test_network_config_paths_under_data_dir() {
        let data = data_dir();
        assert!(networks_config_dir().starts_with(&data));
        assert!(network_config_dir("frontend").starts_with(&data));
        assert_eq!(
            network_config_dir("frontend"),
            networks_config_dir().join("frontend")
        );
    }

    #[test]
    fn test_network_runtime_paths_under_runtime_dir() {
        let rt = runtime_dir();
        assert!(network_runtime_dir("frontend").starts_with(&rt));
        assert!(network_ipam_file("frontend").starts_with(&rt));
        assert!(network_nat_refcount_file("frontend").starts_with(&rt));
        assert!(network_port_forwards_file("frontend").starts_with(&rt));
    }

    #[test]
    fn test_compose_paths_under_runtime_dir() {
        let rt = runtime_dir();
        assert!(compose_dir().starts_with(&rt));
        assert!(compose_project_dir("myapp").starts_with(&rt));
        assert!(compose_state_file("myapp").starts_with(&rt));
        assert_eq!(compose_project_dir("myapp"), compose_dir().join("myapp"));
        assert_eq!(
            compose_state_file("myapp"),
            compose_project_dir("myapp").join("state.json")
        );
    }

    #[test]
    fn test_dns_paths_under_runtime_dir() {
        let rt = runtime_dir();
        assert!(dns_config_dir().starts_with(&rt));
        assert!(dns_pid_file().starts_with(&rt));
        assert!(dns_network_file("pelagos0").starts_with(&rt));
        assert_eq!(
            dns_network_file("frontend"),
            dns_config_dir().join("frontend")
        );
    }

    #[test]
    fn test_dns_dnsmasq_paths_under_runtime_dir() {
        let rt = runtime_dir();
        assert!(dns_backend_file().starts_with(&rt));
        assert!(dns_dnsmasq_conf().starts_with(&rt));
        assert!(dns_hosts_file("pelagos0").starts_with(&rt));
        assert_eq!(
            dns_hosts_file("frontend"),
            dns_config_dir().join("hosts.frontend")
        );
    }
}
