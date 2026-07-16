//! OCI image store — filesystem layout, layer extraction, and manifest persistence.
//!
//! This module is purely synchronous. Networking (registry pulls) lives in `cli::image`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

// Legacy constants kept as documentation — all code now uses crate::paths::*.
// pub const IMAGES_DIR: &str = "/var/lib/pelagos/images";
// pub const LAYERS_DIR: &str = "/var/lib/pelagos/layers";

/// Look up the GID of the `pelagos` system group, if it exists.
fn pelagos_group_gid() -> Option<libc::gid_t> {
    let name = std::ffi::CString::new("pelagos").ok()?;
    let gr = unsafe { libc::getgrnam(name.as_ptr()) };
    if gr.is_null() {
        None
    } else {
        Some(unsafe { (*gr).gr_gid })
    }
}

/// Ensure the image store directories exist with correct ownership and mode.
///
/// If the `pelagos` system group exists (created by `scripts/setup.sh`):
///   `images/`, `layers/`, `build-cache/` → root:pelagos 0775
/// Otherwise (system not yet set up, or pure rootless):
///   → root:root 0755 (root-only access)
///
/// Only acts when a directory doesn't exist yet; does not re-chmod existing
/// directories (which would fail if called as a non-root group member).
fn ensure_image_dirs() -> io::Result<()> {
    #[cfg(unix)]
    use std::os::unix::ffi::OsStrExt;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    let pelagos_gid = pelagos_group_gid();
    let mode = if pelagos_gid.is_some() { 0o775 } else { 0o755 };

    for dir in [
        crate::paths::layers_dir(),
        crate::paths::images_dir(),
        crate::paths::build_cache_dir(),
        crate::paths::blobs_dir(),
    ] {
        if !dir.exists() {
            std::fs::create_dir_all(&dir)?;
            #[cfg(unix)]
            {
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode))?;
                if let Some(gid) = pelagos_gid {
                    let path_cstr = std::ffi::CString::new(dir.as_os_str().as_bytes())
                        .map_err(|e| io::Error::other(e.to_string()))?;
                    // u32::MAX == (uid_t)-1: POSIX "don't change owner".
                    unsafe { libc::lchown(path_cstr.as_ptr(), u32::MAX as libc::uid_t, gid) };
                }
            }
        }
    }
    Ok(())
}

/// Create a directory in the pelagos store with group-writable permissions.
///
/// `std::fs::create_dir_all` respects the process umask; when running as root
/// (umask 0o022) this produces 0o755 dirs.  Even with the setgid bit on parent
/// dirs, the new dir is `root:pelagos rwxr-sr-x` — the pelagos group cannot
/// delete files inside it.  This helper explicitly chmods to 0o775 so that
/// group members can remove images/layers that root created.
#[cfg(unix)]
pub fn create_store_dir(path: &std::path::Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path)?;
    // Only set permissions when we own the directory.  Root-created dirs
    // (encountered in mixed root/rootless builds) cannot be chmod'd by non-root;
    // they already have the correct group-writable bits from their original creation.
    let meta = std::fs::metadata(path)?;
    if meta.uid() == unsafe { libc::getuid() } {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o775))?;
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn create_store_dir(path: &std::path::Path) -> io::Result<()> {
    std::fs::create_dir_all(path)
}

// ---------------------------------------------------------------------------
// Health check configuration
// ---------------------------------------------------------------------------

fn default_health_interval() -> u64 {
    30
}
fn default_health_timeout() -> u64 {
    10
}
fn default_health_retries() -> u32 {
    3
}

/// Health check configuration stored in image manifests and container state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthConfig {
    /// Command to run: e.g. `["/bin/sh", "-c", "curl -f http://localhost/"]`.
    /// Empty vec means the healthcheck is explicitly disabled (`HEALTHCHECK NONE`).
    pub cmd: Vec<String>,
    /// Seconds between consecutive health checks.
    #[serde(default = "default_health_interval")]
    pub interval_secs: u64,
    /// Seconds to wait for the check command to complete before declaring it failed.
    #[serde(default = "default_health_timeout")]
    pub timeout_secs: u64,
    /// Seconds to ignore failed checks after container start (grace period).
    #[serde(default)]
    pub start_period_secs: u64,
    /// Number of consecutive failures required to declare the container unhealthy.
    #[serde(default = "default_health_retries")]
    pub retries: u32,
}

// ---------------------------------------------------------------------------
// Image configuration
// ---------------------------------------------------------------------------

/// Image configuration extracted from the OCI config JSON.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImageConfig {
    /// Environment variables, e.g. `["PATH=/usr/bin", "HOME=/root"]`.
    #[serde(default)]
    pub env: Vec<String>,
    /// Default command (Docker `CMD`).
    #[serde(default)]
    pub cmd: Vec<String>,
    /// Entrypoint prefix (Docker `ENTRYPOINT`).
    #[serde(default)]
    pub entrypoint: Vec<String>,
    /// Working directory inside the container, e.g. `"/app"`.
    #[serde(default)]
    pub working_dir: String,
    /// User string, e.g. `"1000"` or `"1000:1000"`.
    #[serde(default)]
    pub user: String,
    /// Key-value labels (Docker `LABEL`).
    #[serde(default)]
    pub labels: HashMap<String, String>,
    /// Signal used to stop the container (Docker `STOPSIGNAL`), e.g. `"SIGTERM"` or `"15"`.
    /// Empty string means SIGTERM (the default when the image omits the field).
    #[serde(default)]
    pub stop_signal: String,
    /// Health check configuration (from `HEALTHCHECK` instruction).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub healthcheck: Option<HealthConfig>,
}

/// Persisted metadata for a pulled image.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageManifest {
    /// Image reference, e.g. `"alpine:latest"`.
    pub reference: String,
    /// Manifest digest, e.g. `"sha256:abc123..."`.
    pub digest: String,
    /// Ordered layer digests, bottom to top.
    pub layers: Vec<String>,
    /// OCI mediaType for each layer, parallel to `layers`.
    ///
    /// Empty string or absent means a standard `application/vnd.oci.image.layer.v1.tar+gzip`
    /// layer. A Wasm mediaType means the layer contains a raw `.wasm` module blob.
    /// Old manifests without this field deserialise cleanly to an all-empty vec.
    #[serde(default)]
    pub layer_types: Vec<String>,
    /// Parsed image configuration.
    pub config: ImageConfig,
}

impl ImageManifest {
    /// Returns `true` if any layer carries a Wasm module blob.
    pub fn is_wasm_image(&self) -> bool {
        self.layer_types
            .iter()
            .any(|t| crate::wasm::is_wasm_media_type(t))
    }

    /// Returns the path to the Wasm module file stored in the last Wasm layer,
    /// or `None` if this is not a Wasm image.
    pub fn wasm_module_path(&self) -> Option<std::path::PathBuf> {
        // Find the topmost Wasm layer (last in bottom-to-top order).
        self.layers
            .iter()
            .zip(self.layer_types.iter())
            .rev()
            .find(|(_, t)| crate::wasm::is_wasm_media_type(t))
            .map(|(digest, _)| layer_dir(digest).join("module.wasm"))
    }
}

/// Expand a bare image reference to a fully-qualified OCI reference.
///
/// Resolution rules:
/// - If the reference already contains `/` it is returned as-is (already has an
///   organisation or hostname component).
/// - Otherwise a `:latest` tag is appended when no `:` or `@` is present, then
///   the result is prefixed with the *default registry*:
///   1. The `PELAGOS_DEFAULT_REGISTRY` environment variable (if set).
///   2. `docker.io/library` — Docker Hub official images (fallback).
///
/// Setting `PELAGOS_DEFAULT_REGISTRY=public.ecr.aws/docker/library` redirects
/// all unqualified pulls to ECR Public, which has no unauthenticated rate limit.
pub fn normalise_reference(reference: &str) -> String {
    let r = if !reference.contains(':') && !reference.contains('@') {
        format!("{}:latest", reference)
    } else {
        reference.to_string()
    };
    if !r.contains('/') {
        let default_reg = std::env::var("PELAGOS_DEFAULT_REGISTRY")
            .unwrap_or_else(|_| "docker.io/library".to_string());
        format!("{}/{}", default_reg, r)
    } else {
        r
    }
}

/// Convert an image reference like `"alpine:latest"` to a safe directory name (`"alpine_latest"`).
pub fn reference_to_dirname(reference: &str) -> String {
    reference.replace([':', '/', '@'], "_")
}

/// Return the image metadata directory for the given reference.
pub fn image_dir(reference: &str) -> PathBuf {
    crate::paths::images_dir().join(reference_to_dirname(reference))
}

/// Return the extracted layer directory for the given digest.
/// Strips the `sha256:` prefix if present.
pub fn layer_dir(digest: &str) -> PathBuf {
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    crate::paths::layers_dir().join(hex)
}

/// Check whether a layer has already been extracted.
pub fn layer_exists(digest: &str) -> bool {
    layer_dir(digest).is_dir()
}

/// Return the raw blob path for the given digest.
pub fn blob_path(digest: &str) -> std::path::PathBuf {
    crate::paths::blob_path(digest)
}

/// Check whether a raw blob (tar.gz) is cached for this digest.
pub fn blob_exists(digest: &str) -> bool {
    crate::paths::blob_path(digest).exists()
}

/// Persist the raw compressed blob bytes for the given digest.
pub fn save_blob(digest: &str, data: &[u8]) -> io::Result<()> {
    let path = crate::paths::blob_path(digest);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, data)
}

/// Load the raw compressed blob bytes for the given digest.
pub fn load_blob(digest: &str) -> io::Result<Vec<u8>> {
    std::fs::read(crate::paths::blob_path(digest))
}

/// Persist the uncompressed-tar diff_id for the given blob digest.
///
/// The diff_id is the `"sha256:<hex>"` of the raw (uncompressed) tar stream.
pub fn save_blob_diffid(blob_digest: &str, diff_id: &str) -> io::Result<()> {
    std::fs::write(crate::paths::blob_diffid_path(blob_digest), diff_id)
}

/// Load the uncompressed-tar diff_id for the given blob digest.
///
/// Returns `None` if the sidecar file was not found.
pub fn load_blob_diffid(blob_digest: &str) -> Option<String> {
    std::fs::read_to_string(crate::paths::blob_diffid_path(blob_digest)).ok()
}

/// Return the path to the raw OCI config JSON for an image reference.
pub fn oci_config_path(reference: &str) -> std::path::PathBuf {
    image_dir(reference).join("oci-config.json")
}

/// Save raw OCI config JSON to the image directory.
pub fn save_oci_config(reference: &str, config_json: &str) -> io::Result<()> {
    let path = oci_config_path(reference);
    if let Err(e) = std::fs::write(&path, config_json) {
        if matches!(e.raw_os_error(), Some(libc::EPERM) | Some(libc::EACCES)) {
            let _ = std::fs::remove_file(&path);
            std::fs::write(&path, config_json)?;
        } else {
            return Err(e);
        }
    }
    Ok(())
}

/// Load raw OCI config JSON from the image directory.
pub fn load_oci_config(reference: &str) -> io::Result<String> {
    std::fs::read_to_string(oci_config_path(reference))
}

/// Extract a tar layer into the content-addressable layer store, branching on `media_type`.
///
/// Supported media types:
/// - `application/vnd.oci.image.layer.v1.tar+gzip` — gzip-compressed tar (OCI)
/// - `application/vnd.docker.image.rootfs.diff.tar.gzip` — gzip-compressed tar (Docker)
/// - `""` (empty string) — treated as gzip for backward compatibility with pre-mediaType callers
/// - `application/vnd.oci.image.layer.v1.tar` — uncompressed tar (OCI, no `+gzip` suffix);
///   used by images that ship large uncompressed layers (e.g. `quay.io/kubevirt/virt-operator`)
///
/// Any other media type returns an `io::Error` with kind `Unsupported`.
///
/// Future extension point: `application/vnd.oci.image.layer.v1.tar+zstd` (zstd-compressed)
/// is not yet implemented; add a `zstd` dependency and a branch here when needed.
///
/// Handles OCI whiteout files:
/// - `.wh.<name>` → creates an overlayfs character device (0,0) named `<name>`.
/// - `.wh..wh..opq` → sets the `trusted.overlay.opaque` xattr on the parent dir.
///
/// Returns the path to the extracted layer directory.
pub fn extract_layer(digest: &str, tar_path: &Path, media_type: &str) -> io::Result<PathBuf> {
    ensure_image_dirs()?;
    let rootless = crate::paths::is_rootless();
    let dest = layer_dir(digest);
    // A rootless (non-CAP_FSETID) unpack silently strips setuid/setgid bits, so a
    // layer extracted rootless degrades any ROOT container that later reuses it —
    // setuid binaries stop escalating (#384). Mark rootless extractions and, when
    // a root extraction finds such a degraded layer in the shared cache, re-extract
    // it with full fidelity rather than reusing it. The marker is a sibling file,
    // never inside the layer dir (which becomes a container rootfs layer).
    let rootless_marker = dest.with_extension("rootless");
    if dest.is_dir() {
        if rootless || !rootless_marker.exists() {
            return Ok(dest); // rootless reuse, or root reusing a root-extracted layer
        }
        // Root extraction over a rootless-degraded layer: re-extract from scratch.
        std::fs::remove_dir_all(&dest)?;
        let _ = std::fs::remove_file(&rootless_marker);
    }

    // Extract to a temporary sibling, then rename atomically.
    let partial = dest.with_extension("partial");
    if partial.exists() {
        std::fs::remove_dir_all(&partial)?;
    }
    std::fs::create_dir_all(&partial)?;

    let file = std::fs::File::open(tar_path)?;

    // Branch on mediaType: gzip-compressed vs uncompressed tar.
    // An empty string is treated as gzip for backward compat with callers that lack
    // mediaType information (e.g. load_image from OCI tar archives without annotations).
    let is_gzip = matches!(
        media_type,
        "" | "application/vnd.oci.image.layer.v1.tar+gzip"
            | "application/vnd.docker.image.rootfs.diff.tar.gzip"
    );
    let is_plain_tar = media_type == "application/vnd.oci.image.layer.v1.tar";

    let mut archive: tar::Archive<Box<dyn std::io::Read>> = if is_gzip {
        let decoder = flate2::read::GzDecoder::new(file);
        tar::Archive::new(Box::new(decoder))
    } else if is_plain_tar {
        tar::Archive::new(Box::new(file))
    } else {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("unsupported layer media type: {}", media_type),
        ));
    };
    archive.set_preserve_permissions(true);
    archive.set_overwrite(true);

    for entry in archive.entries()? {
        let mut entry = entry?;
        let raw_path = entry.path()?.into_owned();
        let file_name = raw_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        if file_name == ".wh..wh..opq" {
            // Opaque whiteout: mark parent as opaque for overlayfs.
            let parent = partial.join(raw_path.parent().unwrap_or(Path::new("")));
            std::fs::create_dir_all(&parent)?;
            if rootless {
                let _ = set_opaque_xattr_userxattr(&parent);
            } else {
                let _ = set_opaque_xattr(&parent);
            }
            continue;
        }

        if let Some(target_name) = file_name.strip_prefix(".wh.") {
            let parent = partial.join(raw_path.parent().unwrap_or(Path::new("")));
            std::fs::create_dir_all(&parent)?;
            let whiteout_path = parent.join(target_name);
            if rootless {
                create_whiteout_userxattr(&whiteout_path)?;
            } else {
                create_whiteout_device(&whiteout_path)?;
            }
            continue;
        }

        // Normal file — unpack.
        entry.unpack_in(&partial)?;

        // Ensure regular files are world-readable after extraction (#452).
        // Image layer directories are owned root:pelagos (setgid), so extracted
        // files inherit the pelagos group. Containers running as other UIDs need
        // to read their own files — security comes from namespace isolation, not
        // from restricting read access on image content.
        if entry.header().entry_type().is_file() {
            use std::os::unix::fs::PermissionsExt as _;
            let extracted = partial.join(&raw_path);
            if let Ok(meta) = std::fs::metadata(&extracted) {
                let mode = meta.permissions().mode();
                if mode & 0o004 == 0 {
                    let _ = std::fs::set_permissions(
                        &extracted,
                        std::fs::Permissions::from_mode(mode | 0o004),
                    );
                }
            }
        }
    }

    // Ensure parent dir exists and rename partial → final.
    std::fs::create_dir_all(dest.parent().unwrap())?;
    std::fs::rename(&partial, &dest)?;

    // Record that this layer was extracted rootless (setuid/setgid bits stripped),
    // so a later ROOT extraction re-extracts it with full fidelity (#384).
    if rootless {
        let _ = std::fs::File::create(&rootless_marker);
    } else {
        let _ = std::fs::remove_file(&rootless_marker);
    }

    Ok(dest)
}

/// Extract a raw Wasm module blob into the content-addressable layer store.
///
/// Unlike `extract_layer()`, the blob is not a tarball — it is a raw `.wasm`
/// file. The file is stored as `<layer_dir>/module.wasm` using the same atomic
/// partial-then-rename pattern as the tar extractor.
///
/// Returns the path to the extracted layer directory.
pub fn extract_wasm_layer(digest: &str, wasm_blob_path: &std::path::Path) -> io::Result<PathBuf> {
    ensure_image_dirs()?;
    let dest = layer_dir(digest);
    if dest.is_dir() {
        return Ok(dest);
    }

    let partial = dest.with_extension("partial");
    if partial.exists() {
        std::fs::remove_dir_all(&partial)?;
    }
    std::fs::create_dir_all(&partial)?;

    std::fs::copy(wasm_blob_path, partial.join("module.wasm"))?;

    std::fs::create_dir_all(dest.parent().unwrap())?;
    std::fs::rename(&partial, &dest)?;

    Ok(dest)
}

/// Create an overlayfs whiteout character device (major 0, minor 0).
fn create_whiteout_device(path: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::other("invalid path for whiteout device"))?;
    let dev = libc::makedev(0, 0);
    let ret = unsafe { libc::mknod(c_path.as_ptr(), libc::S_IFCHR | 0o666, dev) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Set the `trusted.overlay.opaque` extended attribute on a directory.
fn set_opaque_xattr(dir: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(dir.as_os_str().as_bytes())
        .map_err(|_| io::Error::other("invalid path for xattr"))?;
    let name = b"trusted.overlay.opaque\0";
    let value = b"y";
    let ret = unsafe {
        libc::setxattr(
            c_path.as_ptr(),
            name.as_ptr() as *const libc::c_char,
            value.as_ptr() as *const libc::c_void,
            value.len(),
            0,
        )
    };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Rootless whiteout: create a zero-length file with `user.overlay.whiteout` xattr.
///
/// Used instead of `mknod(S_IFCHR, 0,0)` which requires `CAP_MKNOD`.
/// The kernel's overlayfs `userxattr` mount option reads these xattrs.
fn create_whiteout_userxattr(path: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    // Create zero-length file.
    std::fs::File::create(path)?;

    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::other("invalid path for whiteout xattr"))?;
    let name = b"user.overlay.whiteout\0";
    let value = b"y";
    let ret = unsafe {
        libc::setxattr(
            c_path.as_ptr(),
            name.as_ptr() as *const libc::c_char,
            value.as_ptr() as *const libc::c_void,
            value.len(),
            0,
        )
    };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Rootless opaque xattr: set `user.overlay.opaque` on a directory.
///
/// Counterpart of `set_opaque_xattr()` for the `userxattr` overlay mount option.
fn set_opaque_xattr_userxattr(dir: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(dir.as_os_str().as_bytes())
        .map_err(|_| io::Error::other("invalid path for xattr"))?;
    let name = b"user.overlay.opaque\0";
    let value = b"y";
    let ret = unsafe {
        libc::setxattr(
            c_path.as_ptr(),
            name.as_ptr() as *const libc::c_char,
            value.as_ptr() as *const libc::c_void,
            value.len(),
            0,
        )
    };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Persist an image manifest to disk.
pub fn save_image(manifest: &ImageManifest) -> io::Result<()> {
    ensure_image_dirs()?;
    let dir = image_dir(&manifest.reference);
    create_store_dir(&dir)?;
    let json =
        serde_json::to_string_pretty(manifest).map_err(|e| io::Error::other(e.to_string()))?;
    let manifest_path = dir.join("manifest.json");
    // Atomic write: stage to a temp file in the same dir, then rename(2) over the
    // target. rename is atomic, so a concurrent load_image() observes either the
    // complete old or the complete new manifest — never the 0-byte window that
    // `fs::write`'s truncate-then-write exposes. That window parsed as "EOF while
    // parsing a value at line 1 column 0" and flaked tests whenever two of them
    // pulled/saved the same image (e.g. alpine) at once.
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;
    let tmp = tempfile::Builder::new()
        .prefix(".manifest.json.")
        .tempfile_in(&dir)?;
    tmp.as_file().write_all(json.as_bytes())?;
    // Manifests are read by any user of the shared (group=pelagos) store; the
    // temp file starts 0600, so widen it before it becomes manifest.json.
    std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o644))?;
    if let Err(e) = tmp.persist(&manifest_path) {
        // rename can fail if an existing root-owned manifest can't be replaced in
        // place here; remove it (the dir has group-write) and rename the temp in.
        if matches!(
            e.error.raw_os_error(),
            Some(libc::EPERM) | Some(libc::EACCES)
        ) {
            let tmp_path = e.file.path().to_path_buf();
            let _ = std::fs::remove_file(&manifest_path);
            std::fs::rename(&tmp_path, &manifest_path)?;
        } else {
            return Err(io::Error::other(e.error.to_string()));
        }
    }
    Ok(())
}

/// Load an image manifest from disk.
///
/// If `reference` is a bare name (no `:` or `@`), also tries `<reference>:latest`
/// so that `pelagos run myapp` finds an image built with `pelagos build -t myapp`.
pub fn load_image(reference: &str) -> io::Result<ImageManifest> {
    let path = image_dir(reference).join("manifest.json");
    match std::fs::read_to_string(&path) {
        Ok(data) => serde_json::from_str(&data).map_err(|e| io::Error::other(e.to_string())),
        Err(e) if !reference.contains(':') && !reference.contains('@') => {
            let with_latest = format!("{}:latest", reference);
            let path2 = image_dir(&with_latest).join("manifest.json");
            match std::fs::read_to_string(&path2) {
                Ok(data) => {
                    serde_json::from_str(&data).map_err(|e| io::Error::other(e.to_string()))
                }
                Err(_) => Err(e),
            }
        }
        Err(e) => Err(e),
    }
}

/// List all stored images.
pub fn list_images() -> Vec<ImageManifest> {
    let dir = crate::paths::images_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut manifests = Vec::new();
    for entry in entries.flatten() {
        let manifest_path = entry.path().join("manifest.json");
        if let Ok(data) = std::fs::read_to_string(&manifest_path) {
            if let Ok(m) = serde_json::from_str::<ImageManifest>(&data) {
                manifests.push(m);
            }
        }
    }
    manifests
}

/// Remove an image and its metadata (does not remove shared layers).
pub fn remove_image(reference: &str) -> io::Result<()> {
    let dir = image_dir(reference);
    if dir.is_dir() {
        std::fs::remove_dir_all(&dir)
    } else {
        Err(io::Error::other(format!("image '{}' not found", reference)))
    }
}

/// Return layer directories in top-first order (as overlayfs expects for `lowerdir=`).
///
/// Duplicate digests (common in multi-stage builds that add empty layers) are
/// removed — overlayfs rejects `lowerdir=` paths that appear more than once.
pub fn layer_dirs(manifest: &ImageManifest) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    manifest
        .layers
        .iter()
        .rev()
        .map(|d| layer_dir(d))
        .filter(|p| seen.insert(p.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    // Concurrent save_image + load_image on the same reference must never yield a
    // parse error. A non-atomic writer (plain fs::write, truncate-then-write)
    // exposes a 0-byte manifest window that load_image reads as "EOF while parsing
    // a value at line 1 column 0" — the race that flaked detached-run tests under
    // parallel image pulls. With the temp-file + rename write, every load that
    // finds the file sees a complete manifest.
    #[test]
    fn test_save_image_atomic_under_concurrent_load() {
        let reference = format!("pelagos-atomic-test-{}", std::process::id());
        let mk = |n: usize| ImageManifest {
            reference: reference.clone(),
            digest: format!("sha256:{:064x}", n),
            layers: Vec::new(),
            layer_types: Vec::new(),
            config: ImageConfig::default(),
        };
        save_image(&mk(0)).expect("seed save");

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writer = {
            let (reference, stop) = (reference.clone(), stop.clone());
            std::thread::spawn(move || {
                for n in 1..400 {
                    if stop.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    let _ = save_image(&ImageManifest {
                        reference: reference.clone(),
                        digest: format!("sha256:{:064x}", n),
                        layers: Vec::new(),
                        layer_types: Vec::new(),
                        config: ImageConfig::default(),
                    });
                }
            })
        };

        let mut result = Ok(());
        for _ in 0..400 {
            if let Err(e) = load_image(&reference) {
                // We seeded the manifest, so ENOENT is impossible; any error here
                // is a truncated/partial read — the bug this fix prevents.
                result = Err(format!("load_image saw a non-atomic write: {e}"));
                break;
            }
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        writer.join().ok();
        let _ = remove_image(&reference);
        result.unwrap();
    }

    #[test]
    #[serial]
    fn test_normalise_reference_bare() {
        assert_eq!(
            normalise_reference("alpine"),
            "docker.io/library/alpine:latest"
        );
        assert_eq!(
            normalise_reference("alpine:3.19"),
            "docker.io/library/alpine:3.19"
        );
        assert_eq!(
            normalise_reference("ubuntu@sha256:abc"),
            "docker.io/library/ubuntu@sha256:abc"
        );
    }

    #[test]
    fn test_normalise_reference_qualified() {
        assert_eq!(
            normalise_reference("myregistry.io/myorg/myimage:v1"),
            "myregistry.io/myorg/myimage:v1"
        );
        assert_eq!(
            normalise_reference("public.ecr.aws/docker/library/alpine:latest"),
            "public.ecr.aws/docker/library/alpine:latest"
        );
        assert_eq!(
            normalise_reference("ghcr.io/myorg/myapp"),
            "ghcr.io/myorg/myapp:latest"
        );
    }

    #[test]
    #[serial]
    fn test_normalise_reference_default_registry_env() {
        std::env::set_var("PELAGOS_DEFAULT_REGISTRY", "public.ecr.aws/docker/library");
        let result = normalise_reference("alpine");
        std::env::remove_var("PELAGOS_DEFAULT_REGISTRY");
        assert_eq!(result, "public.ecr.aws/docker/library/alpine:latest");
    }

    #[test]
    fn test_blob_path_strips_prefix() {
        let p = blob_path("sha256:deadbeef");
        assert_eq!(p, crate::paths::blobs_dir().join("deadbeef.tar.gz"));
    }

    #[test]
    fn test_blob_exists_false_for_missing() {
        assert!(!blob_exists(
            "sha256:0000000000000000000000000000000000000000000000000000000000000000"
        ));
    }

    #[test]
    fn test_reference_to_dirname() {
        assert_eq!(reference_to_dirname("alpine:latest"), "alpine_latest");
        assert_eq!(
            reference_to_dirname("docker.io/library/alpine:3.19"),
            "docker.io_library_alpine_3.19"
        );
        assert_eq!(
            reference_to_dirname("registry.example.com/foo/bar:v1"),
            "registry.example.com_foo_bar_v1"
        );
    }

    #[test]
    fn test_layer_dir_strips_prefix() {
        let d = layer_dir("sha256:abc123def456");
        assert_eq!(d, crate::paths::layers_dir().join("abc123def456"));
    }

    #[test]
    fn test_layer_dir_no_prefix() {
        let d = layer_dir("abc123def456");
        assert_eq!(d, crate::paths::layers_dir().join("abc123def456"));
    }

    #[test]
    fn test_manifest_roundtrip() {
        // This test writes to disk — only meaningful under root in the integration suite.
        // Here we just verify serialize/deserialize round-trip in memory.
        let manifest = ImageManifest {
            reference: "test:latest".to_string(),
            digest: "sha256:000".to_string(),
            layers: vec!["sha256:aaa".to_string(), "sha256:bbb".to_string()],
            layer_types: vec![String::new(), String::new()],
            config: ImageConfig {
                env: vec!["PATH=/usr/bin".to_string()],
                cmd: vec!["/bin/sh".to_string()],
                entrypoint: Vec::new(),
                working_dir: String::new(),
                user: String::new(),
                labels: HashMap::new(),
                stop_signal: String::new(),
                healthcheck: None,
            },
        };
        let json = serde_json::to_string(&manifest).unwrap();
        let loaded: ImageManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.reference, "test:latest");
        assert_eq!(loaded.layers.len(), 2);
        assert_eq!(loaded.config.cmd, vec!["/bin/sh"]);
    }

    #[test]
    fn test_layer_dirs_order() {
        let manifest = ImageManifest {
            reference: "test:latest".to_string(),
            digest: "sha256:000".to_string(),
            layers: vec!["sha256:bottom".to_string(), "sha256:top".to_string()],
            layer_types: Vec::new(),
            config: ImageConfig {
                env: Vec::new(),
                cmd: Vec::new(),
                entrypoint: Vec::new(),
                working_dir: String::new(),
                user: String::new(),
                labels: HashMap::new(),
                stop_signal: String::new(),
                healthcheck: None,
            },
        };
        let dirs = layer_dirs(&manifest);
        // Top-first for overlayfs lowerdir
        assert_eq!(dirs[0], crate::paths::layers_dir().join("top"));
        assert_eq!(dirs[1], crate::paths::layers_dir().join("bottom"));
    }
}
