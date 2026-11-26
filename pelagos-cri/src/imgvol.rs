//! Image volumes (CRI `Mount.image` / `image_sub_path`, KEP-4639, #616).
//!
//! The kubelet pulls the image and passes it to `CreateContainer` as a mount with
//! `image` set and no host path; the runtime must present the image's filesystem,
//! read-only, at the container path (optionally just one sub path of it).
//!
//! Approach: stage the image on the host at `CreateContainer` and hand every
//! runtime an ordinary host path to bind-mount read-only, so the native handler and
//! the Kata/gVisor shim path need nothing special:
//!
//! 1. resolve the image to its layer directories (the same store walk the shim path
//!    uses),
//! 2. mount them read-only at `<root>/<container id>/<n>/root` (a read-only
//!    overlay of the layers, or a read-only bind for a single-layer image),
//! 3. resolve `image_sub_path` *inside* that mount without ever following a symlink
//!    out of it, and fail if it does not exist,
//! 4. the staged path becomes the mount's source; [`unstage`] detaches it when the
//!    container is removed.

use std::collections::HashSet;
use std::ffi::{CString, OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use crate::shim::rootfs::{self, RootfsError, StoreLayout};

/// Where staged image volumes live (tmpfs, cleared at boot with the mounts).
pub const IMGVOL_ROOT: &str = "/run/pelagos-cri/imgvol";

/// Symlinks followed while resolving a sub path before giving up (Linux uses 40).
const MAX_SYMLINK_HOPS: u32 = 40;

#[derive(Debug, thiserror::Error)]
pub enum ImgVolError {
    #[error("{0}")]
    Image(#[from] RootfsError),
    #[error("image volume sub path {path:?} does not exist in the image")]
    SubPathMissing { path: String },
    #[error("invalid image volume sub path {path:?}: {why}")]
    BadSubPath { path: String, why: String },
    #[error("invalid container id {0:?}")]
    BadId(String),
    #[error("image volume mount: {0}")]
    Mount(String),
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id != "." && id != ".." && !id.contains(['/', '\0', ':', ','])
}

/// `<root>/<container id>`: everything staged for one container.
fn container_dir(root: &Path, container_id: &str) -> PathBuf {
    root.join(container_id)
}

/// Resolve `rel` inside `root` as if `root` were `/`: symlinks (absolute ones too)
/// are followed relative to `root`, and `..` cannot climb above it. The result is a
/// real path under `root` made only of non-symlink components, so binding it can
/// never reach outside the image. `NotFound` means the path does not exist.
pub fn secure_join(root: &Path, rel: &str) -> io::Result<PathBuf> {
    if rel.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "sub path contains a NUL byte",
        ));
    }
    let mut resolved: Vec<OsString> = Vec::new();
    let mut pending: std::collections::VecDeque<OsString> = std::collections::VecDeque::new();
    let push_components = |pending: &mut std::collections::VecDeque<OsString>, p: &Path| {
        for c in p.components().rev() {
            match c {
                Component::Normal(n) => pending.push_front(n.to_os_string()),
                Component::ParentDir => pending.push_front(OsString::from("..")),
                Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
            }
        }
    };
    push_components(&mut pending, Path::new(rel));

    let mut hops = 0;
    while let Some(c) = pending.pop_front() {
        if c == OsStr::new("..") {
            resolved.pop(); // clamped at the root
            continue;
        }
        let mut candidate = root.to_path_buf();
        candidate.extend(&resolved);
        candidate.push(&c);
        let meta = std::fs::symlink_metadata(&candidate)?;
        if meta.file_type().is_symlink() {
            hops += 1;
            if hops > MAX_SYMLINK_HOPS {
                return Err(io::Error::other("too many levels of symbolic links"));
            }
            let target = std::fs::read_link(&candidate)?;
            if target.is_absolute() {
                resolved.clear();
            }
            push_components(&mut pending, &target);
        } else {
            resolved.push(c);
        }
    }
    let mut out = root.to_path_buf();
    out.extend(&resolved);
    Ok(out)
}

/// Mount points under (or at) `dir`, from `/proc/self/mountinfo` text.
fn mounts_under(mountinfo: &str, dir: &Path) -> Vec<PathBuf> {
    let prefix = format!("{}/", dir.display());
    mountinfo
        .lines()
        .filter_map(|l| l.split(' ').nth(4))
        .filter(|m| Path::new(m) == dir || m.starts_with(&prefix))
        .map(PathBuf::from)
        .collect()
}

fn c_path(p: &Path) -> io::Result<CString> {
    CString::new(p.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

fn mount_call(
    source: &str,
    target: &Path,
    fstype: Option<&str>,
    flags: libc::c_ulong,
    data: Option<&str>,
) -> io::Result<()> {
    let src = CString::new(source).map_err(io::Error::other)?;
    let tgt = c_path(target)?;
    let typ = fstype
        .map(CString::new)
        .transpose()
        .map_err(io::Error::other)?;
    let dat = data
        .map(CString::new)
        .transpose()
        .map_err(io::Error::other)?;
    // SAFETY: all pointers are valid NUL-terminated strings for the call's duration.
    let rc = unsafe {
        libc::mount(
            src.as_ptr(),
            tgt.as_ptr(),
            typ.as_ref().map_or(std::ptr::null(), |t| t.as_ptr()),
            flags,
            dat.as_ref()
                .map_or(std::ptr::null(), |d| d.as_ptr() as *const libc::c_void),
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Mount `layers` (top-first) read-only at `mnt`.
fn mount_layers(layers: &[PathBuf], mnt: &Path, alias_dir: &Path) -> Result<(), ImgVolError> {
    if let [only] = layers {
        // A read-only overlay needs two lower layers; one layer is a plain bind.
        let layer = only.to_string_lossy();
        mount_call(&layer, mnt, None, libc::MS_BIND, None)
            .map_err(|e| ImgVolError::Mount(format!("bind {layer}: {e}")))?;
        mount_call(
            "none",
            mnt,
            None,
            libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY,
            None,
        )
        .map_err(|e| ImgVolError::Mount(format!("remount read-only: {e}")))?;
        return Ok(());
    }
    let join = |v: &[PathBuf]| {
        v.iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(":")
    };
    let mut lower = join(layers);
    if lower.len() + 40 > rootfs::MAX_OPTIONS_LEN {
        // The kernel copies mount options in at one page and truncates the rest (#612):
        // mount through short symlinks instead.
        std::fs::create_dir_all(alias_dir)?;
        let mut aliases = Vec::with_capacity(layers.len());
        for (i, l) in layers.iter().enumerate() {
            let link = alias_dir.join(i.to_string());
            std::os::unix::fs::symlink(l, &link)?;
            aliases.push(link);
        }
        lower = join(&aliases);
        if lower.len() + 40 > rootfs::MAX_OPTIONS_LEN {
            return Err(ImgVolError::Mount(format!(
                "{} layers do not fit in the overlay mount options even with short aliases",
                layers.len()
            )));
        }
    }
    let opts = format!("lowerdir={lower},metacopy=off");
    mount_call(
        "overlay",
        mnt,
        Some("overlay"),
        libc::MS_RDONLY,
        Some(&opts),
    )
    .map_err(|e| ImgVolError::Mount(format!("overlay of {} layers: {e}", layers.len())))
}

/// Stage `image_ref` (optionally only `sub_path` of it) for `container_id` as mount
/// number `index`, returning the host directory to bind read-only into the
/// container. Nothing is left behind on error.
pub fn stage(
    store: &StoreLayout,
    root: &Path,
    container_id: &str,
    index: usize,
    image_ref: &str,
    sub_path: &str,
) -> Result<PathBuf, ImgVolError> {
    if !valid_id(container_id) {
        return Err(ImgVolError::BadId(container_id.into()));
    }
    if Path::new(sub_path)
        .components()
        .any(|c| matches!(c, Component::Prefix(_)))
    {
        return Err(ImgVolError::BadSubPath {
            path: sub_path.into(),
            why: "not a relative path".into(),
        });
    }
    let manifest = rootfs::find_manifest(store, image_ref)?;
    let layers = rootfs::layers_top_first(store, image_ref, &manifest)?;

    let base = container_dir(root, container_id).join(index.to_string());
    let mnt = base.join("root");
    std::fs::create_dir_all(&mnt)?;
    let fail = |e: ImgVolError| {
        unstage_one(&base);
        e
    };
    mount_layers(&layers, &mnt, &base.join("l")).map_err(fail)?;
    match secure_join(&mnt, sub_path) {
        Ok(p) => Ok(p),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(fail(ImgVolError::SubPathMissing {
            path: sub_path.into(),
        })),
        Err(e) => Err(fail(ImgVolError::BadSubPath {
            path: sub_path.into(),
            why: e.to_string(),
        })),
    }
}

/// Detach and remove one staged volume (`<container>/<n>`). Never removes anything
/// while a mount is still present under it: deleting through a live mount would
/// reach into the layer store.
fn unstage_one(base: &Path) {
    let mnt = base.join("root");
    for _ in 0..3 {
        let Ok(c) = c_path(&mnt) else { return };
        // SAFETY: valid NUL-terminated path.
        let rc = unsafe { libc::umount2(c.as_ptr(), libc::MNT_DETACH) };
        if rc != 0 {
            break; // EINVAL: not (or no longer) a mount point
        }
    }
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    if !mounts_under(&mountinfo, base).is_empty() {
        log::warn!(
            "image volume {}: still mounted, leaving it in place",
            base.display()
        );
        return;
    }
    let _ = std::fs::remove_dir_all(base); // symlinks are removed, never followed
}

/// Detach and remove everything staged for `container_id`.
pub fn unstage(root: &Path, container_id: &str) {
    if !valid_id(container_id) {
        return;
    }
    let dir = container_dir(root, container_id);
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            unstage_one(&e.path());
        }
    }
    let _ = std::fs::remove_dir(&dir);
}

/// After a restart, drop staged volumes of containers that no longer exist.
pub fn cleanup_stale(root: &Path, keep: &HashSet<String>) {
    let Ok(rd) = std::fs::read_dir(root) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !keep.contains(&name) {
            log::info!("removing stale image volume staging for container {name}");
            unstage(root, &name);
        }
    }
}

#[cfg(test)]
mod tests;
