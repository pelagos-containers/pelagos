//! CRI ImageService implementation.

use crate::cri::image_service_server::ImageService;
use crate::cri::{
    Image, ImageFsInfoRequest, ImageFsInfoResponse, ImageSpec, ImageStatusRequest,
    ImageStatusResponse, Int64Value, ListImagesRequest, ListImagesResponse, PullImageRequest,
    PullImageResponse, RemoveImageRequest, RemoveImageResponse, StreamImagesRequest,
    StreamImagesResponse,
};
use crate::invoke::run_pelagos;
use crate::state::AppState;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use tonic::{Request, Response, Status};

const IMAGES_DIR: &str = "/var/lib/pelagos/images";
const LAYERS_DIR: &str = "/var/lib/pelagos/layers";

#[derive(Deserialize)]
struct ImageManifest {
    reference: String,
    digest: String,
    #[allow(dead_code)]
    layers: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)]
    layer_types: Vec<String>,
    #[allow(dead_code)]
    config: ImageConfig,
}

#[derive(Deserialize, Default)]
struct ImageConfig {
    #[serde(default)]
    #[allow(dead_code)]
    env: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)]
    cmd: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)]
    entrypoint: Vec<String>,
    /// OCI image config `User` field — may be "uid", "user", "uid:gid", or "user:group".
    #[serde(default)]
    user: String,
}

/// Recursively compute the apparent (logical byte) size and inode count of
/// everything under `path`, in one filesystem walk — natively, with no `du`
/// subprocess (#563/#564).
///
/// Matches `du`'s actual semantics:
/// - `lstat`-equivalent metadata — does NOT follow symlinks (a symlink counts
///   as itself, not its target, same as `du`'s default).
/// - Deduplicates hard-linked files by `(dev, ino)`, so a file linked
///   multiple times within the scanned tree is only counted once (toward
///   both size and inodes) — matching `du`'s real accounting.
/// - Directories count toward the inode total but NOT the size total — only
///   regular files'/symlinks' own apparent size contributes to `size`.
///   Confirmed by live cross-check against real `du` on a production layer:
///   an earlier version of this function that also summed each directory's
///   own (non-trivial, filesystem-block-dependent) `st_size` overcounted by
///   exactly the sum of every directory's raw size in the tree.
///
/// A dangling/partially torn-down overlay or FUSE mount left by an aborted
/// layer extraction could in principle make a real `du` binary wedge in
/// kernel D-state indefinitely (#561's original theory, since disproven by
/// live reproduction — the actual #561 cause was uncached repeated `du` work,
/// not a wedge). A plain synchronous walk has the same exposure in principle
/// (a hung read on a wedged mount blocks this call same as it would `du`),
/// but carries none of the subprocess-spawn overhead, binary/flag dependency,
/// or stdout-parsing fragility that motivated replacing it; run on the
/// blocking thread pool via `tokio::task::spawn_blocking` so a slow walk
/// never stalls the async runtime's worker threads.
fn walk_dir_usage(path: &std::path::Path) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    let mut size = 0u64;
    let mut inodes = 0u64;
    let mut seen: std::collections::HashSet<(u64, u64)> = std::collections::HashSet::new();

    // GNU `du --apparent-size` counts directories toward the inode total but
    // NOT toward the size total — only regular files'/symlinks' own apparent
    // size contributes. Confirmed by live cross-check against real `du` on a
    // production layer: a naive walk that also summed each directory's own
    // (non-trivial, ext4-block-dependent) stat size overcounted by exactly
    // the sum of every directory's raw `st_size` in the tree.
    let mut count = |meta: &std::fs::Metadata| {
        if seen.insert((meta.dev(), meta.ino())) {
            if !meta.is_dir() {
                size += meta.len();
            }
            inodes += 1;
        }
    };

    if let Ok(meta) = std::fs::symlink_metadata(path) {
        count(&meta);
    }

    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            // DirEntry::metadata() is lstat-equivalent — does not follow symlinks.
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            count(&meta);
            if meta.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    (size, inodes)
}

/// Async wrapper around [`walk_dir_usage`], off the async runtime's worker
/// threads.
async fn dir_usage(path: &str) -> (u64, u64) {
    let path = std::path::PathBuf::from(path);
    tokio::task::spawn_blocking(move || walk_dir_usage(&path))
        .await
        .unwrap_or((0, 0))
}

/// In-memory cache of per-layer on-disk size, keyed by layer digest (without
/// the `sha256:` prefix). Layer content is immutable and content-addressed —
/// a given digest's on-disk size/inode count can never change — so caching it
/// for the life of the process is always safe, with no invalidation needed.
///
/// Without this, `layers_size` recomputed every layer of every image on every
/// single `ListImages` call: real, measured 5.9s for 73 images / 405 layers
/// (back when this walked via a `du` subprocess per layer) on a cluster node
/// that had simply been running for a while — comfortably past `crictl`'s 2s
/// default client deadline. Since a node's accumulated image/layer count is
/// exactly what a `pelagos-cri` restart does NOT reset, that cost only grows
/// over a node's lifetime and, once it crosses the client's deadline, fails
/// every call thereafter (#561). Caching turns repeat `ListImages` calls into
/// O(unique layers) real filesystem walks ever, then pure in-memory lookups —
/// collapsing the common case (many images sharing base layers) to near-zero
/// added latency.
fn layer_size_cache() -> &'static tokio::sync::Mutex<HashMap<String, u64>> {
    static CACHE: std::sync::OnceLock<tokio::sync::Mutex<HashMap<String, u64>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()))
}

/// Same caching as `layer_size_cache`, for per-layer inode counts.
fn layer_inode_cache() -> &'static tokio::sync::Mutex<HashMap<String, u64>> {
    static CACHE: std::sync::OnceLock<tokio::sync::Mutex<HashMap<String, u64>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()))
}

/// On-disk size and inode count of the layer at `digest`, served from
/// `layer_size_cache()`/`layer_inode_cache()` after the first real walk.
/// Computes both from a single [`walk_dir_usage`] pass — not two — since
/// `layers_dir_usage` below needs both for every digest it sees.
async fn cached_layer_usage(digest: &str) -> (u64, u64) {
    {
        let sizes = layer_size_cache().lock().await;
        let inodes = layer_inode_cache().lock().await;
        if let (Some(&size), Some(&n)) = (sizes.get(digest), inodes.get(digest)) {
            return (size, n);
        }
    }
    let (size, n) = dir_usage(&format!("{LAYERS_DIR}/{digest}")).await;
    layer_size_cache()
        .lock()
        .await
        .insert(digest.to_string(), size);
    layer_inode_cache()
        .lock()
        .await
        .insert(digest.to_string(), n);
    (size, n)
}

/// On-disk size of the layer at `digest`. See [`cached_layer_usage`].
async fn cached_layer_size(digest: &str) -> u64 {
    cached_layer_usage(digest).await.0
}

/// Total on-disk size and inode count of every layer under `LAYERS_DIR`,
/// built from a single shallow `read_dir` (not a recursive walk of the whole
/// tree) plus the same per-layer caches `list_images` populates.
///
/// `image_fs_info` used to walk the ENTIRE `LAYERS_DIR` tree on every call via
/// `du -sb`/`du --inodes -s` — measured live at 1.7s + 1.65s (809k inodes,
/// 22GB) on a real cluster node, which is what `crictl`'s startup "validate
/// CRI v1 image API" preflight actually calls and what was really producing
/// the reported permanent `DeadlineExceeded` (#561) — the per-image
/// `layers_size` cache above doesn't touch this path at all, since it's one
/// aggregate query over the whole tree rather than per-image. Since
/// `LAYERS_DIR` contains nothing but per-digest layer directories, this total
/// equals the sum of each layer's own cached size/inode count — recomputed
/// from a real walk only for digests neither this nor `list_images` has seen
/// before.
async fn layers_dir_usage(dir: &str) -> (u64, u64) {
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else {
        return (0, 0);
    };
    let mut used = 0u64;
    let mut inodes = 0u64;
    while let Ok(Some(entry)) = rd.next_entry().await {
        let Some(digest) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let (size, n) = cached_layer_usage(&digest).await;
        used += size;
        inodes += n;
    }
    (used, inodes)
}

/// Current time in nanoseconds since the Unix epoch (for CRI timestamps).
fn now_ns() -> i64 {
    chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
}

/// A single on-disk image entry (one per stored reference/tag), enriched with the
/// content-addressable **config digest** that identifies the image regardless of
/// which tag or registry it was pulled under.
struct StoredImage {
    /// The stored reference, e.g. `"docker.io/library/alpine:3.20"`.
    reference: String,
    /// The OCI **manifest** digest (registry-specific; goes in repo_digests).
    manifest_digest: String,
    /// The OCI **config** digest — the stable image id (matches containerd).
    config_digest: String,
    /// Layer digests, for size computation.
    layers: Vec<String>,
    /// OCI config `User`.
    user: String,
}

/// Lock serializing RemoveImage so concurrent removals of the same image don't
/// race (critest "should not fail on simultaneous RemoveImage calls").
fn remove_lock() -> &'static tokio::sync::Mutex<()> {
    static L: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    L.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// The repository part of a reference, with any `:tag` or `@digest` suffix
/// stripped. e.g. `docker.io/library/alpine:3.20` -> `docker.io/library/alpine`.
fn repo_of(reference: &str) -> &str {
    let base = reference.split('@').next().unwrap_or(reference);
    // A ':' is a tag separator only when it comes after the last '/', so a
    // registry host:port (e.g. `localhost:5000/img`) is not mistaken for a tag.
    match base.rfind('/') {
        Some(slash) => match base[slash..].find(':') {
            Some(colon) => &base[..slash + colon],
            None => base,
        },
        None => match base.find(':') {
            Some(colon) => &base[..colon],
            None => base,
        },
    }
}

pub struct ImageSvc {
    pub state: AppState,
}

impl ImageSvc {
    /// Read every stored image entry, computing each one's config digest.
    async fn read_stored(&self) -> Vec<StoredImage> {
        let Ok(mut rd) = tokio::fs::read_dir(IMAGES_DIR).await else {
            return Vec::new();
        };
        let mut out = Vec::new();
        while let Ok(Some(entry)) = rd.next_entry().await {
            if !entry.path().is_dir() {
                continue;
            }
            let dir = entry.path();
            let Ok(data) = tokio::fs::read_to_string(dir.join("manifest.json")).await else {
                continue;
            };
            let Ok(m) = serde_json::from_str::<ImageManifest>(&data) else {
                continue;
            };
            // The config digest = sha256 of the raw OCI config blob, which is
            // identical for the same image across tags and registries. Also read
            // the OCI config's `User` straight from that blob — the authoritative
            // source — so ImageStatus reports uid/username even when the stored
            // manifest didn't capture it (#382). Fall back to the manifest digest
            // and manifest config.user if the config blob isn't present.
            let (config_digest, oci_user) = match tokio::fs::read(dir.join("oci-config.json")).await
            {
                Ok(bytes) => {
                    let digest = format!("sha256:{:x}", Sha256::digest(&bytes));
                    let user = serde_json::from_slice::<serde_json::Value>(&bytes)
                        .ok()
                        .and_then(|v| v["config"]["User"].as_str().map(String::from))
                        .unwrap_or_default();
                    (digest, user)
                }
                Err(_) => (m.digest.clone(), String::new()),
            };
            let user = if oci_user.is_empty() {
                m.config.user
            } else {
                oci_user
            };
            out.push(StoredImage {
                reference: m.reference,
                manifest_digest: m.digest,
                config_digest,
                layers: m.layers,
                user,
            });
        }
        out
    }

    async fn layers_size(layers: &[String]) -> u64 {
        let mut total: u64 = 0;
        for layer in layers {
            let digest = layer.strip_prefix("sha256:").unwrap_or(layer.as_str());
            total += cached_layer_size(digest).await;
        }
        // kubelet rejects images with Size_ == 0.
        total.max(1)
    }

    /// Aggregate stored entries by config digest into one CRI `Image` each — with
    /// all tags collected into `repo_tags` and all `<repo>@<manifest-digest>`
    /// forms into `repo_digests` — WITHOUT computing on-disk size (which is the
    /// expensive part). Each image is paired with the layer digests needed to
    /// size it lazily. Sizing every image on an ImageStatus call would be far too
    /// slow on a node with many images (#340).
    async fn aggregate_unsized(&self) -> Vec<(Image, Vec<String>)> {
        let stored = self.read_stored().await;
        // Preserve a stable grouping order keyed by config digest.
        let mut order: Vec<String> = Vec::new();
        let mut groups: HashMap<String, Vec<StoredImage>> = HashMap::new();
        for s in stored {
            if !groups.contains_key(&s.config_digest) {
                order.push(s.config_digest.clone());
            }
            groups.entry(s.config_digest.clone()).or_default().push(s);
        }

        let mut images = Vec::new();
        for id in order {
            let group = &groups[&id];
            let mut repo_tags: Vec<String> = Vec::new();
            let mut repo_digests: Vec<String> = Vec::new();
            for s in group {
                // Tag references (no '@') go to repo_tags; digest refs are skipped
                // here and represented in repo_digests below.
                if !s.reference.contains('@') && !repo_tags.contains(&s.reference) {
                    repo_tags.push(s.reference.clone());
                }
                let rd = format!("{}@{}", repo_of(&s.reference), s.manifest_digest);
                if !repo_digests.contains(&rd) {
                    repo_digests.push(rd);
                }
            }
            let user_str = group[0].user.split(':').next().unwrap_or("").trim();
            let (uid, username) = if let Ok(n) = user_str.parse::<i64>() {
                (Some(Int64Value { value: n }), String::new())
            } else if user_str.is_empty() {
                (None, String::new())
            } else {
                (None, user_str.to_string())
            };
            let image = Image {
                id: id.clone(),
                repo_tags,
                repo_digests,
                size: 0,
                uid,
                username,
                spec: Some(ImageSpec {
                    image: id,
                    annotations: HashMap::new(),
                    ..Default::default()
                }),
                pinned: false,
            };
            images.push((image, group[0].layers.clone()));
        }
        images
    }

    /// Find the aggregated image matching `r`, which may be a tag, a
    /// `repo@digest`, the config-digest id, or a bare manifest digest.
    fn match_image(images: &[Image], r: &str) -> Option<Image> {
        // A bare repo reference (no `:tag`, no `@digest`) defaults to `:latest`,
        // so `ImageStatus("repo")` finds the image pulled as `repo:latest` (#382).
        let r_latest = (!r.contains('@') && repo_of(r) == r).then(|| format!("{}:latest", r));
        images
            .iter()
            .find(|img| {
                img.id == r
                    || img.repo_tags.iter().any(|t| t == r)
                    || r_latest
                        .as_deref()
                        .is_some_and(|rl| img.repo_tags.iter().any(|t| t == rl))
                    || img.repo_digests.iter().any(|d| d == r)
                    // also accept a bare manifest digest (the `@sha256:...` part)
                    || img
                        .repo_digests
                        .iter()
                        .any(|d| d.split('@').nth(1) == Some(r))
            })
            .cloned()
    }

    /// Resolve `r` to its aggregated image with size filled in (sizes only the
    /// one matched image, not the whole store).
    async fn status_of(&self, r: &str) -> Option<Image> {
        let entries = self.aggregate_unsized().await;
        let idx = entries
            .iter()
            .position(|(img, _)| Self::match_image(std::slice::from_ref(img), r).is_some())?;
        let (mut img, layers) = entries.into_iter().nth(idx)?;
        img.size = Self::layers_size(&layers).await;
        Some(img)
    }

    async fn get_bin(&self) -> String {
        self.state.inner.lock().await.pelagos_bin.clone()
    }
}

#[tonic::async_trait]
impl ImageService for ImageSvc {
    async fn list_images(
        &self,
        _request: Request<ListImagesRequest>,
    ) -> Result<Response<ListImagesResponse>, Status> {
        let mut images = Vec::new();
        for (mut img, layers) in self.aggregate_unsized().await {
            img.size = Self::layers_size(&layers).await;
            images.push(img);
        }
        Ok(Response::new(ListImagesResponse { images }))
    }

    async fn image_status(
        &self,
        request: Request<ImageStatusRequest>,
    ) -> Result<Response<ImageStatusResponse>, Status> {
        let image_ref = request
            .into_inner()
            .image
            .map(|s| s.image)
            .unwrap_or_default();
        if image_ref.is_empty() {
            return Ok(Response::new(ImageStatusResponse {
                image: None,
                info: HashMap::new(),
            }));
        }
        Ok(Response::new(ImageStatusResponse {
            image: self.status_of(&image_ref).await,
            info: HashMap::new(),
        }))
    }

    async fn pull_image(
        &self,
        request: Request<PullImageRequest>,
    ) -> Result<Response<PullImageResponse>, Status> {
        let inner = request.into_inner();
        let image_ref = inner.image.map(|s| s.image).unwrap_or_default();
        if image_ref.is_empty() {
            return Err(Status::invalid_argument("image reference is required"));
        }

        let bin = self.get_bin().await;
        let result = run_pelagos(&bin, &["image", "pull", &image_ref]).await;
        match result {
            Ok(out) if out.success => {
                // CRI requires PullImageResponse.image_ref == Image.id. Resolve the
                // just-pulled image to its config-digest id so ListImages/
                // ImageStatus return the same identifier (#340).
                let entries = self.aggregate_unsized().await;
                let images: Vec<Image> = entries.into_iter().map(|(i, _)| i).collect();
                let id = Self::match_image(&images, &image_ref)
                    .map(|img| img.id)
                    .unwrap_or(image_ref);
                Ok(Response::new(PullImageResponse { image_ref: id }))
            }
            Ok(out) => Err(Status::internal(format!(
                "pelagos image pull failed: {}",
                out.stderr
            ))),
            Err(e) => Err(Status::internal(format!("exec error: {}", e))),
        }
    }

    async fn remove_image(
        &self,
        request: Request<RemoveImageRequest>,
    ) -> Result<Response<RemoveImageResponse>, Status> {
        let image_ref = request
            .into_inner()
            .image
            .map(|s| s.image)
            .unwrap_or_default();
        if image_ref.is_empty() {
            return Ok(Response::new(RemoveImageResponse {}));
        }

        // Serialize removals so concurrent calls for the same image are safe.
        let _guard = remove_lock().lock().await;
        let bin = self.get_bin().await;

        // Per the CRI spec, removing an image by ONE tag removes the image and ALL
        // of its tags (across registries) that resolve to the same digest. Resolve
        // the target to its aggregated image and remove every underlying reference.
        let entries = self.aggregate_unsized().await;
        let images: Vec<Image> = entries.into_iter().map(|(i, _)| i).collect();
        if let Some(img) = Self::match_image(&images, &image_ref) {
            for r in &img.repo_tags {
                let _ = run_pelagos(&bin, &["image", "rm", r]).await;
            }
            // Some refs may exist only in digest form; best-effort remove those too.
            for d in &img.repo_digests {
                let _ = run_pelagos(&bin, &["image", "rm", d]).await;
            }
        }
        // Not found is a no-op success (idempotent), matching the CRI spec.
        Ok(Response::new(RemoveImageResponse {}))
    }

    async fn image_fs_info(
        &self,
        _request: Request<ImageFsInfoRequest>,
    ) -> Result<Response<ImageFsInfoResponse>, Status> {
        use crate::cri::{FilesystemIdentifier, FilesystemUsage, UInt64Value};
        // Report real usage of the layer store so the kubelet's image GC works.
        let (used, inodes) = layers_dir_usage(LAYERS_DIR).await;
        let usage = FilesystemUsage {
            timestamp: now_ns(),
            fs_id: Some(FilesystemIdentifier {
                mountpoint: LAYERS_DIR.to_string(),
            }),
            used_bytes: Some(UInt64Value { value: used }),
            inodes_used: Some(UInt64Value { value: inodes }),
        };
        Ok(Response::new(ImageFsInfoResponse {
            image_filesystems: vec![usage],
            container_filesystems: vec![],
        }))
    }

    type StreamImagesStream = std::pin::Pin<
        Box<dyn futures_core::Stream<Item = Result<StreamImagesResponse, Status>> + Send>,
    >;

    async fn stream_images(
        &self,
        _request: Request<StreamImagesRequest>,
    ) -> Result<Response<Self::StreamImagesStream>, Status> {
        Err(Status::unimplemented("not implemented"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #563/#564: `walk_dir_usage` must compute apparent size and inode count
    /// matching `du -sb --apparent-size`/`du --inodes -s` semantics natively —
    /// no `du` subprocess, no dependency on GNU-specific flags being present.
    /// Builds a small real tree (two files, a subdirectory, a symlink, and a
    /// hard link) and asserts: file sizes are summed exactly; directories
    /// contribute to the inode count but NOT the size total (confirmed by a
    /// live cross-check against real `du` on a production layer, where an
    /// earlier version of this function overcounted by exactly the sum of
    /// every directory's own raw size — `du`'s apparent-size total only
    /// reflects actual file/symlink content, not directory metadata); the
    /// symlink is counted as itself (its own size — the target path's byte
    /// length), not traversed into its target, matching `du`'s default
    /// `lstat`-based behavior; the hard link is NOT double-counted, matching
    /// `du`'s real `(dev, ino)` dedup.
    #[test]
    fn walk_dir_usage_matches_du_semantics() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        std::fs::write(root.join("a.txt"), b"hello").unwrap(); // 5 bytes
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/b.txt"), b"world!").unwrap(); // 6 bytes
        std::fs::hard_link(root.join("a.txt"), root.join("sub/a-hardlink.txt")).unwrap();
        let target = root.join("a.txt");
        // A symlink's own apparent size (lstat) is the byte length of its
        // target path string.
        let symlink_size = target.as_os_str().len() as u64;
        std::os::unix::fs::symlink(&target, root.join("link-to-a")).unwrap();

        let (size, inodes) = walk_dir_usage(root);

        // Entries: root dir, a.txt, sub dir, sub/b.txt, link-to-a (symlink
        // itself, not traversed) = 5 inodes. a.txt's hardlink in sub/ shares
        // the same (dev, ino) as a.txt, so it must NOT add a 6th.
        assert_eq!(
            inodes, 5,
            "hard link to an already-counted file must not be double-counted"
        );
        // Size = ONLY the two real files' content (5 + 6) + the symlink's own
        // size. The root and sub DIRECTORIES must contribute nothing to size
        // (only to the inode count above) -- matching a live cross-check
        // against real `du` on a production layer, where an earlier version
        // of this function that also summed directories' own sizes
        // overcounted by exactly the sum of every directory's raw size.
        assert_eq!(
            size,
            5 + 6 + symlink_size,
            "directories must not contribute to the size total, only to inodes: got {size}"
        );
    }

    /// #563/#564: the other half of #561's real fix: the actual reproduced root cause
    /// `DeadlineExceeded` on `ListImages`/`crictl images` was not a wedged
    /// mount but plain O(images x layers) `du` re-execution on every call —
    /// measured at 5.9s for 73 images / 405 layers on a real cluster node,
    /// comfortably past `crictl`'s 2s default deadline, and only growing as a
    /// node accumulates more images (exactly what a `pelagos-cri` restart
    /// does not reset). `cached_layer_size` must populate the cache on first
    /// lookup — even when `du` reports 0 for a nonexistent/inaccessible path
    /// — so a later lookup for the same digest never re-runs `du` at all.
    #[tokio::test]
    async fn cached_layer_size_populates_the_cache_after_first_lookup() {
        let digest = "deadbeef561testonly-does-not-exist";
        assert!(
            layer_size_cache().lock().await.get(digest).is_none(),
            "test digest must not already be cached"
        );
        let _ = cached_layer_size(digest).await;
        assert!(
            layer_size_cache().lock().await.contains_key(digest),
            "a layer's size must be cached after the first lookup, even when `du` \
             reports 0 for it, so repeat ListImages calls never re-run `du` for \
             the same digest (#561)"
        );
    }

    /// #561 regression, part 2: `image_fs_info` used to run `du -sb`/`du
    /// --inodes -s` over the ENTIRE layers tree on every call — measured live
    /// at 1.7s + 1.65s (809k inodes, 22GB) on a real cluster node. This path,
    /// not `list_images`'s per-image loop, is what `crictl`'s "validate CRI
    /// v1 image API" preflight actually calls, so it alone was enough to keep
    /// reproducing the reported permanent `DeadlineExceeded` even after
    /// caching `list_images`'s per-image loop. `layers_dir_usage` must read
    /// the directory's entries and serve each from the SAME per-layer caches
    /// `list_images` populates, rather than re-walking the whole tree.
    /// Proven here by pre-populating the caches with values a real `du`
    /// could never produce for these (empty) directories, then asserting
    /// those exact values come back — which only happens on a cache hit.
    #[tokio::test]
    async fn layers_dir_usage_sums_cached_sizes_without_rerunning_du() {
        let dir = tempfile::tempdir().expect("tempdir");
        let digests = ["fakedigest561-a", "fakedigest561-b"];
        for d in digests {
            std::fs::create_dir(dir.path().join(d)).expect("mkdir");
            layer_size_cache().lock().await.insert(d.to_string(), 1000);
            layer_inode_cache().lock().await.insert(d.to_string(), 7);
        }

        let (used, inodes) = layers_dir_usage(dir.path().to_str().unwrap()).await;

        assert_eq!(
            used, 2000,
            "must sum the CACHED per-layer sizes, not re-run du over the whole tree (#561)"
        );
        assert_eq!(
            inodes, 14,
            "must sum the cached per-layer inode counts the same way"
        );
    }

    fn stored(
        reference: &str,
        manifest_digest: &str,
        config_digest: &str,
        user: &str,
    ) -> StoredImage {
        StoredImage {
            reference: reference.into(),
            manifest_digest: manifest_digest.into(),
            config_digest: config_digest.into(),
            layers: vec![],
            user: user.into(),
        }
    }

    /// Group StoredImages exactly as ImageSvc::aggregate does (sans disk I/O for
    /// size), so we can unit-test tag/digest aggregation deterministically.
    fn aggregate(stored: Vec<StoredImage>) -> Vec<Image> {
        let mut order: Vec<String> = Vec::new();
        let mut groups: HashMap<String, Vec<StoredImage>> = HashMap::new();
        for s in stored {
            if !groups.contains_key(&s.config_digest) {
                order.push(s.config_digest.clone());
            }
            groups.entry(s.config_digest.clone()).or_default().push(s);
        }
        let mut images = Vec::new();
        for id in order {
            let group = &groups[&id];
            let mut repo_tags: Vec<String> = Vec::new();
            let mut repo_digests: Vec<String> = Vec::new();
            for s in group {
                if !s.reference.contains('@') && !repo_tags.contains(&s.reference) {
                    repo_tags.push(s.reference.clone());
                }
                let rd = format!("{}@{}", repo_of(&s.reference), s.manifest_digest);
                if !repo_digests.contains(&rd) {
                    repo_digests.push(rd);
                }
            }
            let user_str = group[0].user.split(':').next().unwrap_or("").trim();
            let (uid, username) = if let Ok(n) = user_str.parse::<i64>() {
                (Some(Int64Value { value: n }), String::new())
            } else if user_str.is_empty() {
                (None, String::new())
            } else {
                (None, user_str.to_string())
            };
            images.push(Image {
                id: id.clone(),
                repo_tags,
                repo_digests,
                size: 1,
                uid,
                username,
                spec: Some(ImageSpec {
                    image: id,
                    annotations: HashMap::new(),
                    ..Default::default()
                }),
                pinned: false,
            });
        }
        images
    }

    #[test]
    fn test_repo_of_strips_tag_and_digest() {
        assert_eq!(repo_of("alpine:3.20"), "alpine");
        assert_eq!(
            repo_of("docker.io/library/alpine:latest"),
            "docker.io/library/alpine"
        );
        assert_eq!(repo_of("localhost:5000/img:v1"), "localhost:5000/img");
        assert_eq!(repo_of("alpine@sha256:deadbeef"), "alpine");
        assert_eq!(
            repo_of("docker.io/library/alpine"),
            "docker.io/library/alpine"
        );
    }

    #[test]
    fn test_multiple_tags_same_config_aggregate_to_one_image() {
        // Three tags of the same content -> one image with three repoTags
        // (critest: "listImage should get exactly 3 repoTags").
        let imgs = aggregate(vec![
            stored("alpine:3.20", "sha256:m1", "sha256:cfg", ""),
            stored("alpine:latest", "sha256:m1", "sha256:cfg", ""),
            stored("myalpine:test", "sha256:m1", "sha256:cfg", ""),
        ]);
        assert_eq!(
            imgs.len(),
            1,
            "same config digest must aggregate to one image"
        );
        assert_eq!(imgs[0].id, "sha256:cfg");
        assert_eq!(imgs[0].repo_tags.len(), 3);
        assert!(imgs[0].repo_tags.contains(&"alpine:3.20".to_string()));
    }

    #[test]
    fn test_different_registries_same_config_aggregate() {
        // Same content from two registries -> one stable id (the config digest),
        // both repo_digests present (critest "same identifier from different
        // registries" / "removing from one registry removes all tags").
        let imgs = aggregate(vec![
            stored("registry-a.io/lib/img:v1", "sha256:ma", "sha256:cfg", ""),
            stored("registry-b.io/lib/img:v1", "sha256:mb", "sha256:cfg", ""),
        ]);
        assert_eq!(imgs.len(), 1);
        assert_eq!(imgs[0].id, "sha256:cfg");
        assert_eq!(imgs[0].repo_digests.len(), 2);
        assert!(imgs[0]
            .repo_digests
            .contains(&"registry-a.io/lib/img@sha256:ma".to_string()));
    }

    #[test]
    fn test_match_image_by_tag_digest_and_id() {
        let imgs = aggregate(vec![stored(
            "alpine:3.20",
            "sha256:m1",
            "sha256:cfg",
            "1000",
        )]);
        assert!(ImageSvc::match_image(&imgs, "alpine:3.20").is_some()); // by tag
        assert!(ImageSvc::match_image(&imgs, "sha256:cfg").is_some()); // by id
        assert!(ImageSvc::match_image(&imgs, "alpine@sha256:m1").is_some()); // by repo_digest
        assert!(ImageSvc::match_image(&imgs, "sha256:m1").is_some()); // by bare manifest digest
        assert!(ImageSvc::match_image(&imgs, "nonexistent:1").is_none());
    }

    #[test]
    fn test_match_image_bare_repo_defaults_latest() {
        // #382: ImageStatus("repo") with no tag must find the image pulled as
        // "repo:latest" (note the repo name itself ends in "-latest" to guard the
        // tag-vs-repo parsing).
        let imgs = aggregate(vec![stored(
            "gcr.io/x/test-image-latest:latest",
            "sha256:m",
            "sha256:c",
            "",
        )]);
        assert!(ImageSvc::match_image(&imgs, "gcr.io/x/test-image-latest").is_some());
        assert!(ImageSvc::match_image(&imgs, "gcr.io/x/test-image-latest:latest").is_some());
        // A bare repo not pulled as :latest must NOT match (no implicit cross-tag).
        let other = aggregate(vec![stored("repo:v1", "sha256:m", "sha256:c2", "")]);
        assert!(ImageSvc::match_image(&other, "repo").is_none());
    }

    #[test]
    fn test_uid_username_from_config_user() {
        let numeric = aggregate(vec![stored("i:1", "sha256:m", "sha256:c1", "1000")]);
        assert_eq!(numeric[0].uid, Some(Int64Value { value: 1000 }));
        assert_eq!(numeric[0].username, "");
        let named = aggregate(vec![stored("i:2", "sha256:m", "sha256:c2", "nobody")]);
        assert_eq!(named[0].uid, None);
        assert_eq!(named[0].username, "nobody");
        let root = aggregate(vec![stored("i:3", "sha256:m", "sha256:c3", "0")]);
        assert_eq!(root[0].uid, Some(Int64Value { value: 0 }));
        let empty = aggregate(vec![stored("i:4", "sha256:m", "sha256:c4", "")]);
        assert_eq!(empty[0].uid, None);
        assert_eq!(empty[0].username, "");
    }
}
