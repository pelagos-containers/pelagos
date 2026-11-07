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

/// Bound on how long a `du` invocation may run before we give up on it.
const DU_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Run `cmd` with `args`, bounded by `timeout`; best-effort SIGKILL the child
/// and give up rather than hang the caller if it doesn't finish in time.
/// Generic over the command so the timeout/kill behavior can be exercised in
/// a test without depending on `du` ever actually wedging.
async fn run_timed(cmd: &str, args: &[&str], timeout: std::time::Duration) -> Option<String> {
    let child = tokio::process::Command::new(cmd)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let pid = child.id().map(|p| p as i32);
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(o)) if o.status.success() => std::str::from_utf8(&o.stdout).ok().map(str::to_string),
        Ok(_) => None,
        Err(_) => {
            log::warn!("{cmd} {args:?} timed out after {timeout:?}; killing and giving up (#561)");
            if let Some(pid) = pid {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
            None
        }
    }
}

/// Run `du` against `path`, bounded by `DU_TIMEOUT`. A dangling/partially
/// torn-down overlay or FUSE mount left by an aborted layer extraction or
/// rootfs teardown can wedge `du` in kernel D-state indefinitely (#561) —
/// unlike a Rust-level deadlock, that survives a `pelagos-cri` restart, since
/// the stuck process is external to it and a fresh restart just re-runs `du`
/// against the same wedged path. Bound it like every other subprocess wait in
/// this codebase (streaming.rs, runtime.rs) so one bad layer can't wedge
/// ListImages/ImageFsInfo forever.
async fn run_du(path: &str, args: &[&str]) -> Option<String> {
    let mut full_args: Vec<&str> = args.to_vec();
    full_args.push(path);
    run_timed("du", &full_args, DU_TIMEOUT).await
}

/// Recursively sum up apparent file sizes in a directory tree using `du -sb --apparent-size`.
async fn dir_disk_usage(path: &str) -> u64 {
    run_du(path, &["--apparent-size", "-sb"])
        .await
        .and_then(|s| s.split_whitespace().next().and_then(|n| n.parse().ok()))
        .unwrap_or(0)
}

/// Count inodes (files + directories) under a path via `du --inodes -s`.
async fn count_inodes(path: &str) -> u64 {
    run_du(path, &["--inodes", "-s"])
        .await
        .and_then(|s| s.split_whitespace().next().and_then(|n| n.parse().ok()))
        .unwrap_or(0)
}

/// In-memory cache of per-layer on-disk size, keyed by layer digest (without
/// the `sha256:` prefix). Layer content is immutable and content-addressed —
/// a given digest's `du` result can never change — so caching it for the life
/// of the process is always safe, with no invalidation needed.
///
/// Without this, `layers_size` reran `du` on every layer of every image on
/// every single `ListImages` call: real, measured 5.9s for 73 images / 405
/// layers on a cluster node that had simply been running for a while — comfortably
/// past `crictl`'s 2s default client deadline. Since a node's accumulated
/// image/layer count is exactly what a `pelagos-cri` restart does NOT reset,
/// that cost only grows over a node's lifetime and, once it crosses the
/// client's deadline, fails every call thereafter — which looks identical to,
/// and was originally mistaken for, a stuck/wedged `du` (#561). Caching turns
/// repeat `ListImages` calls into O(unique layers) real `du` spawns ever,
/// then pure in-memory lookups — collapsing the common case (many images
/// sharing base layers) to near-zero added latency.
fn layer_size_cache() -> &'static tokio::sync::Mutex<HashMap<String, u64>> {
    static CACHE: std::sync::OnceLock<tokio::sync::Mutex<HashMap<String, u64>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()))
}

/// On-disk size of the layer at `digest`, served from `layer_size_cache()`
/// after the first real `du`.
async fn cached_layer_size(digest: &str) -> u64 {
    if let Some(&size) = layer_size_cache().lock().await.get(digest) {
        return size;
    }
    let size = dir_disk_usage(&format!("{LAYERS_DIR}/{digest}")).await;
    layer_size_cache()
        .lock()
        .await
        .insert(digest.to_string(), size);
    size
}

/// Same caching as `layer_size_cache`, for per-layer inode counts.
fn layer_inode_cache() -> &'static tokio::sync::Mutex<HashMap<String, u64>> {
    static CACHE: std::sync::OnceLock<tokio::sync::Mutex<HashMap<String, u64>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()))
}

/// Inode count of the layer at `digest`, served from `layer_inode_cache()`
/// after the first real `du`.
async fn cached_layer_inodes(digest: &str) -> u64 {
    if let Some(&n) = layer_inode_cache().lock().await.get(digest) {
        return n;
    }
    let n = count_inodes(&format!("{LAYERS_DIR}/{digest}")).await;
    layer_inode_cache()
        .lock()
        .await
        .insert(digest.to_string(), n);
    n
}

/// Total on-disk size and inode count of every layer under `LAYERS_DIR`,
/// built from a single shallow `read_dir` (not a recursive `du`) plus the
/// same per-layer caches `list_images` populates.
///
/// `image_fs_info` used to run `du -sb`/`du --inodes -s` over the ENTIRE
/// `LAYERS_DIR` tree on every call — measured live at 1.7s + 1.65s (809k
/// inodes, 22GB) on a real cluster node, which is what `crictl`'s startup
/// "validate CRI v1 image API" preflight actually calls and what was really
/// producing the reported permanent `DeadlineExceeded` (#561) — the
/// per-image `layers_size` cache above doesn't touch this path at all, since
/// it's one aggregate query over the whole tree rather than per-image. Since
/// `LAYERS_DIR` contains nothing but per-digest layer directories, this total
/// equals the sum of each layer's own cached size/inode count — recomputed
/// from a real `du` only for digests neither this nor `list_images` has seen
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
        used += cached_layer_size(&digest).await;
        inodes += cached_layer_inodes(&digest).await;
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

    /// #561 regression: `ListImages`/`ImageFsInfo` call `du` on every request
    /// with no bound, so a `du` wedged on a dangling/torn-down overlay or FUSE
    /// mount hangs the image service forever — and survives a `pelagos-cri`
    /// restart, since the wedge is in the kernel, external to the process.
    /// `run_timed` must give up and return rather than block past its
    /// timeout, regardless of how long the underlying command actually runs.
    #[tokio::test]
    async fn run_timed_gives_up_rather_than_block_past_its_timeout() {
        let start = std::time::Instant::now();
        let result = run_timed("sleep", &["5"], std::time::Duration::from_millis(100)).await;
        assert!(
            result.is_none(),
            "a command still running past its timeout must report no output, not hang the caller"
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "must return promptly on timeout, not block for anywhere near the full \
             underlying command duration (#561)"
        );
    }

    /// #561 regression: the actual reproduced root cause of the persistent
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
