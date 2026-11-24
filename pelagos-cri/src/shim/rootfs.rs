//! Rootfs `Mount` descriptors for shim-backed containers (#593, epic #574).
//!
//! The containerd contract makes the **shim** responsible for mounting the
//! rootfs: `CreateTaskRequest.rootfs` carries `Mount` descriptors and the shim
//! mounts them into `<bundle>/rootfs` and unmounts on delete
//! (`docs/SHIM_V2_CLIENT_DESIGN.md` section 5). So nothing here mounts anything
//! on the host. This module only *describes* one `overlay` mount over
//! pelagos's content-addressed layer store, with an ephemeral per-container
//! upper/work directory, and cleans those directories up after Delete.
//!
//! It also resolves image users (`name[:group]`) against the *layered*
//! `/etc/passwd` and `/etc/group` without mounting, so the spec generator can
//! accept usernames (see [`LayeredUsers`]).

use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

use containerd_shim_protos::api;

use super::spec::UserDb;

/// Marker pelagos writes last when a layer extraction committed
/// (`image::LAYER_COMPLETE_MARKER`).
const LAYER_COMPLETE_MARKER: &str = ".pelagos_complete";

/// `mount(2)` copies its data argument into one page; stay clear of it so the
/// shim can append its own options.
pub const MAX_OPTIONS_LEN: usize = 3900;

/// overlayfs' hard cap on lower layers (`OVL_MAX_STACK`).
pub const MAX_LOWER_LAYERS: usize = 500;

/// Where images, layers and per-container scratch live.
#[derive(Debug, Clone)]
pub struct StoreLayout {
    pub images: PathBuf,
    pub layers: PathBuf,
    /// Parent of the per-container `cri-shim-<id>/{upper,work}` directories.
    /// Must be on disk (not tmpfs), as for native containers (#427).
    pub scratch: PathBuf,
    /// Short-path symlink directory used only when the full `lowerdir` list
    /// would not fit in one mount option page.
    pub alias_root: PathBuf,
}

impl StoreLayout {
    /// The root-owned system store `pelagos-cri` runs against.
    pub fn system() -> Self {
        Self {
            images: "/var/lib/pelagos/images".into(),
            layers: "/var/lib/pelagos/layers".into(),
            scratch: "/var/lib/pelagos/scratch".into(),
            alias_root: "/run/pelagos-cri/ovl".into(),
        }
    }

    /// A store rooted at `root` (tests).
    pub fn under(root: &Path) -> Self {
        Self {
            images: root.join("images"),
            layers: root.join("layers"),
            scratch: root.join("scratch"),
            alias_root: root.join("ovl"),
        }
    }
}

#[derive(Debug)]
pub enum RootfsError {
    ImageNotFound(String),
    /// Wasm images have no overlay rootfs.
    WasmImage(String),
    NoLayers(String),
    LayerMissing(String),
    TooManyLayers(usize),
    OptionsTooLong {
        len: usize,
        layers: usize,
    },
    InvalidId(String),
    /// Refusing to delete scratch while the overlay is still mounted.
    StillMounted(PathBuf),
    Io(io::Error),
}

impl std::fmt::Display for RootfsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RootfsError::ImageNotFound(r) => write!(f, "image {r:?} not found in the local store"),
            RootfsError::WasmImage(r) => write!(f, "image {r:?} is a Wasm image; it has no overlay rootfs"),
            RootfsError::NoLayers(r) => write!(f, "image {r:?} has no layers"),
            RootfsError::LayerMissing(d) => write!(f, "layer {d} is missing or incompletely extracted"),
            RootfsError::TooManyLayers(n) => write!(f, "{n} layers exceeds overlayfs' limit of {MAX_LOWER_LAYERS}"),
            RootfsError::OptionsTooLong { len, layers } => write!(
                f,
                "overlay options for {layers} layers are {len} bytes even with short aliases (limit {MAX_OPTIONS_LEN})"
            ),
            RootfsError::InvalidId(i) => write!(f, "invalid container id {i:?} for a scratch path"),
            RootfsError::StillMounted(p) => write!(f, "{} is still mounted; not removing its scratch", p.display()),
            RootfsError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RootfsError {}

impl From<io::Error> for RootfsError {
    fn from(e: io::Error) -> Self {
        RootfsError::Io(e)
    }
}

fn dirname(reference: &str) -> String {
    reference.replace([':', '/', '@'], "_")
}

/// Expand a short reference the way `pelagos image pull` stores it
/// (`busybox` -> `docker.io/library/busybox:latest`; `PELAGOS_DEFAULT_REGISTRY`
/// overrides the default registry). Mirrors `pelagos::image::normalise_reference`.
fn normalise(reference: &str) -> String {
    let r = if !reference.contains(':') && !reference.contains('@') {
        format!("{reference}:latest")
    } else {
        reference.to_string()
    };
    if r.contains('/') {
        return r;
    }
    let reg = std::env::var("PELAGOS_DEFAULT_REGISTRY")
        .unwrap_or_else(|_| "docker.io/library".to_string());
    format!("{reg}/{r}")
}

/// The CRI image id of a stored image: the sha256 of its `oci-config.json`
/// (what `ImageStatus`/`PullImage` report), if that file exists.
fn config_digest(image_dir: &Path) -> Option<String> {
    use sha2::Digest as _;
    let bytes = fs::read(image_dir.join("oci-config.json")).ok()?;
    Some(format!("sha256:{:x}", sha2::Sha256::digest(&bytes)))
}

/// Find an image's `manifest.json` by reference, manifest digest, CRI image id
/// (config digest) or stored directory name. The kubelet creates containers by
/// image **id**, not the tag it pulled, so the id must resolve here (#622).
pub fn find_manifest(
    store: &StoreLayout,
    image_ref: &str,
) -> Result<serde_json::Value, RootfsError> {
    // Fast path: the directory pelagos itself would use, for the reference as
    // given and for its normalised form (the kubelet passes the pod's image
    // string, which may be short, e.g. `busybox`).
    let normalised = normalise(image_ref);
    for r in [image_ref, normalised.as_str()] {
        let direct = store.images.join(dirname(r)).join("manifest.json");
        if let Ok(data) = fs::read_to_string(&direct) {
            if let Ok(v) = serde_json::from_str(&data) {
                return Ok(v);
            }
        }
    }
    let rd = match fs::read_dir(&store.images) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(RootfsError::ImageNotFound(image_ref.into()))
        }
        Err(e) => return Err(e.into()),
    };
    for ent in rd.flatten() {
        let Ok(data) = fs::read_to_string(ent.path().join("manifest.json")) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&data) else {
            continue;
        };
        if v["reference"].as_str() == Some(image_ref)
            || v["digest"].as_str() == Some(image_ref)
            || (image_ref.starts_with("sha256:")
                && config_digest(&ent.path()).as_deref() == Some(image_ref))
        {
            return Ok(v);
        }
    }
    Err(RootfsError::ImageNotFound(image_ref.into()))
}

fn layer_path(store: &StoreLayout, digest: &str) -> PathBuf {
    store
        .layers
        .join(digest.strip_prefix("sha256:").unwrap_or(digest))
}

/// Layer directories for an image, **top-first** (overlayfs `lowerdir` order),
/// de-duplicated (overlayfs rejects repeats) and verified complete.
pub fn layers_top_first(
    store: &StoreLayout,
    image_ref: &str,
    manifest: &serde_json::Value,
) -> Result<Vec<PathBuf>, RootfsError> {
    if manifest["layer_types"]
        .as_array()
        .map(|a| {
            a.iter()
                .any(|t| t.as_str().map(|s| s.contains("wasm")).unwrap_or(false))
        })
        .unwrap_or(false)
    {
        return Err(RootfsError::WasmImage(image_ref.into()));
    }
    let digests: Vec<&str> = manifest["layers"]
        .as_array()
        .map(|a| a.iter().filter_map(|d| d.as_str()).collect())
        .unwrap_or_default();
    if digests.is_empty() {
        return Err(RootfsError::NoLayers(image_ref.into()));
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for d in digests.iter().rev() {
        let p = layer_path(store, d);
        if !seen.insert(p.clone()) {
            continue;
        }
        if !p.is_dir() || !p.join(LAYER_COMPLETE_MARKER).exists() {
            return Err(RootfsError::LayerMissing((*d).to_string()));
        }
        out.push(p);
    }
    Ok(out)
}

/// The `overlay` mount options for `layers` (top-first).
pub fn overlay_options(layers: &[PathBuf], upper: &Path, work: &Path) -> String {
    let lower = layers
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(":");
    format!(
        "lowerdir={lower},upperdir={},workdir={},metacopy=off",
        upper.display(),
        work.display()
    )
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && id != ".."
        && !id.contains('/')
        && !id.contains('\0')
        && !id.contains(':')
        && !id.contains(',')
}

/// A planned rootfs: the descriptor to hand the shim plus the directories we
/// created and must remove after Delete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootfsPlan {
    pub typ: String,
    pub source: String,
    pub options: Vec<String>,
    scratch: PathBuf,
    upper: PathBuf,
    alias_dir: Option<PathBuf>,
    /// Layer directories, top-first (kept for user resolution).
    pub layers: Vec<PathBuf>,
}

impl RootfsPlan {
    /// The shim API `Mount` for `CreateTaskRequest.rootfs`. `target` is left
    /// empty: the shim mounts into its own `<bundle>/rootfs`.
    pub fn to_api_mount(&self) -> api::Mount {
        let mut m = api::Mount::new();
        m.type_ = self.typ.clone();
        m.source = self.source.clone();
        m.options = self.options.clone();
        m
    }

    pub fn upper_dir(&self) -> &Path {
        &self.upper
    }

    pub fn scratch_dir(&self) -> &Path {
        &self.scratch
    }

    /// Whether short aliases were needed for the lower layers.
    pub fn uses_aliases(&self) -> bool {
        self.alias_dir.is_some()
    }

    /// Remove the scratch (upper/work) and alias directories. Call after the
    /// shim's Delete. Refuses while the overlay is still mounted, since
    /// deleting a live upper dir corrupts the mount.
    pub fn cleanup(&self) -> Result<(), RootfsError> {
        let mi = fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
        if mountinfo_has_upper(&mi, &self.upper) {
            return Err(RootfsError::StillMounted(self.upper.clone()));
        }
        remove_if_exists(&self.scratch)?;
        if let Some(a) = &self.alias_dir {
            remove_if_exists(a)?;
        }
        Ok(())
    }
}

fn remove_if_exists(p: &Path) -> io::Result<()> {
    match fs::remove_dir_all(p) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        r => r,
    }
}

/// Whether `/proc/self/mountinfo` text lists an overlay using `upper` as its
/// `upperdir`.
pub fn mountinfo_has_upper(mountinfo: &str, upper: &Path) -> bool {
    let want = format!("upperdir={}", upper.display());
    mountinfo.lines().any(|l| {
        l.contains(" - overlay ")
            && l.rsplit(' ')
                .next()
                .map(|opts| opts.split(',').any(|o| o == want))
                .unwrap_or(false)
    })
}

/// Plan the rootfs for `container_id` over `layers` (top-first): create the
/// scratch dirs and, only if the option string would not fit one page, short
/// symlink aliases for the lower layers.
pub fn plan_rootfs(
    store: &StoreLayout,
    container_id: &str,
    layers: Vec<PathBuf>,
) -> Result<RootfsPlan, RootfsError> {
    if !valid_id(container_id) {
        return Err(RootfsError::InvalidId(container_id.into()));
    }
    if layers.is_empty() {
        return Err(RootfsError::NoLayers(container_id.into()));
    }
    if layers.len() > MAX_LOWER_LAYERS {
        return Err(RootfsError::TooManyLayers(layers.len()));
    }

    // Scratch parent is 0700; the upper/work dirs inside stay 0755 like the
    // native overlay scratch.
    let scratch = store.scratch.join(format!("cri-shim-{container_id}"));
    let mut db = fs::DirBuilder::new();
    db.recursive(true).mode(0o700);
    if let Some(parent) = scratch.parent() {
        db.create(parent)?;
    }
    db.create(&scratch)?;
    let upper = scratch.join("upper");
    let work = scratch.join("work");
    for d in [&upper, &work] {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(d)?;
    }

    let mut lowers = layers.clone();
    let mut alias_dir = None;
    let mut opts = overlay_options(&lowers, &upper, &work);
    if opts.len() > MAX_OPTIONS_LEN {
        // Short, stable alias names keep the option string small. overlayfs
        // resolves the symlinks at mount time.
        let short = &container_id[..container_id.len().min(12)];
        let dir = store.alias_root.join(short);
        let mut b = fs::DirBuilder::new();
        b.recursive(true).mode(0o700);
        let setup = (|| -> io::Result<()> {
            b.create(&dir)?;
            for (i, l) in layers.iter().enumerate() {
                let link = dir.join(i.to_string());
                match fs::remove_file(&link) {
                    Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                    _ => {}
                }
                std::os::unix::fs::symlink(l, &link)?;
            }
            Ok(())
        })();
        if let Err(e) = setup {
            let _ = remove_if_exists(&scratch);
            let _ = remove_if_exists(&dir);
            return Err(e.into());
        }
        lowers = (0..layers.len()).map(|i| dir.join(i.to_string())).collect();
        opts = overlay_options(&lowers, &upper, &work);
        alias_dir = Some(dir);
        if opts.len() > MAX_OPTIONS_LEN {
            let _ = remove_if_exists(&scratch);
            if let Some(a) = &alias_dir {
                let _ = remove_if_exists(a);
            }
            return Err(RootfsError::OptionsTooLong {
                len: opts.len(),
                layers: layers.len(),
            });
        }
    }

    Ok(RootfsPlan {
        typ: "overlay".into(),
        source: "overlay".into(),
        options: opts.split(',').map(String::from).collect::<Vec<_>>(),
        scratch,
        upper,
        alias_dir,
        layers,
    })
}

/// Rebuild the cleanup handle of an already-planned rootfs after a CRI
/// restart (re-adoption): nothing is created, the scratch paths are derived
/// exactly as [`plan_rootfs`] derived them, and the alias directory is
/// included only if it exists. `layers` may be empty when the image is gone;
/// it only serves user resolution, which a running container no longer needs.
pub fn reattach_rootfs(
    store: &StoreLayout,
    container_id: &str,
    layers: Vec<PathBuf>,
) -> RootfsPlan {
    let scratch = store.scratch.join(format!("cri-shim-{container_id}"));
    let short = &container_id[..container_id.len().min(12)];
    let alias = store.alias_root.join(short);
    RootfsPlan {
        typ: "overlay".into(),
        source: "overlay".into(),
        options: Vec::new(),
        upper: scratch.join("upper"),
        scratch,
        alias_dir: alias.is_dir().then_some(alias),
        layers,
    }
}

// ── layered /etc/passwd + /etc/group ─────────────────────────────────────────

/// Is `path` an overlay-opaque directory (hides lower layers)?
fn is_opaque(path: &Path) -> bool {
    let c = match std::ffi::CString::new(path.as_os_str().as_bytes()) {
        Ok(c) => c,
        Err(_) => return false,
    };
    for name in [
        &b"trusted.overlay.opaque\0"[..],
        &b"user.overlay.opaque\0"[..],
    ] {
        let mut buf = [0u8; 4];
        let n = unsafe {
            libc::lgetxattr(
                c.as_ptr(),
                name.as_ptr() as *const libc::c_char,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
            )
        };
        if n > 0 && buf[0] == b'y' {
            return true;
        }
    }
    false
}

/// Read `rel` (e.g. `etc/passwd`) as the merged overlay would show it,
/// without mounting: top layer first, whiteouts (char device 0:0) hide the
/// entry and everything below, opaque directories hide lower layers.
/// Symlinks are refused rather than followed: an absolute link would escape
/// the layer and read the host.
pub fn read_layered(layers: &[PathBuf], rel: &str) -> Result<Option<String>, String> {
    let comps: Vec<&str> = rel.split('/').filter(|c| !c.is_empty()).collect();
    for layer in layers {
        let mut cur = layer.clone();
        let mut opaque_here = false;
        'walk: for (i, comp) in comps.iter().enumerate() {
            cur.push(comp);
            let md = match fs::symlink_metadata(&cur) {
                Ok(md) => md,
                Err(e) if e.kind() == io::ErrorKind::NotFound => break 'walk,
                Err(e) => return Err(format!("{}: {e}", cur.display())),
            };
            let ft = md.file_type();
            if ft.is_char_device() && md.rdev() == 0 {
                return Ok(None); // whiteout
            }
            if ft.is_symlink() {
                return Err(format!(
                    "{} is a symlink; refusing to follow it out of a layer",
                    cur.display()
                ));
            }
            if i + 1 < comps.len() {
                if !ft.is_dir() {
                    break 'walk;
                }
                if is_opaque(&cur) {
                    opaque_here = true;
                }
            } else if ft.is_file() {
                return fs::read_to_string(&cur)
                    .map(Some)
                    .map_err(|e| format!("{}: {e}", cur.display()));
            }
        }
        if opaque_here {
            return Ok(None);
        }
    }
    Ok(None)
}

/// [`UserDb`] over an image's layers.
pub struct LayeredUsers {
    layers: Vec<PathBuf>,
}

impl LayeredUsers {
    /// `layers` top-first, as from [`layers_top_first`].
    pub fn new(layers: Vec<PathBuf>) -> Self {
        Self { layers }
    }

    fn passwd(&self) -> Result<Vec<(String, u32, u32)>, String> {
        let text = read_layered(&self.layers, "etc/passwd")?.unwrap_or_default();
        Ok(parse_passwd(&text))
    }
}

/// `name:x:uid:gid:...` -> `(name, uid, gid)`; comments and junk skipped.
pub fn parse_passwd(text: &str) -> Vec<(String, u32, u32)> {
    text.lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            if f.len() < 4 {
                return None;
            }
            Some((f[0].to_string(), f[2].parse().ok()?, f[3].parse().ok()?))
        })
        .collect()
}

/// `name:x:gid:members` -> `(name, gid)`.
pub fn parse_group(text: &str) -> Vec<(String, u32)> {
    text.lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            if f.len() < 3 {
                return None;
            }
            Some((f[0].to_string(), f[2].parse().ok()?))
        })
        .collect()
}

impl UserDb for LayeredUsers {
    fn passwd_gid_by_uid(&self, uid: u32) -> Result<Option<u32>, String> {
        Ok(self
            .passwd()?
            .into_iter()
            .find(|(_, u, _)| *u == uid)
            .map(|(_, _, g)| g))
    }

    fn passwd_by_name(&self, name: &str) -> Result<Option<(u32, u32)>, String> {
        Ok(self
            .passwd()?
            .into_iter()
            .find(|(n, _, _)| n == name)
            .map(|(_, u, g)| (u, g)))
    }

    fn gid_by_group_name(&self, name: &str) -> Result<Option<u32>, String> {
        let text = read_layered(&self.layers, "etc/group")?.unwrap_or_default();
        Ok(parse_group(&text)
            .into_iter()
            .find(|(n, _)| n == name)
            .map(|(_, g)| g))
    }
}

#[cfg(test)]
mod tests;
