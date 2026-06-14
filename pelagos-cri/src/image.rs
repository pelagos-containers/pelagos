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

/// Recursively sum up apparent file sizes in a directory tree using `du -sb --apparent-size`.
async fn dir_disk_usage(path: &str) -> u64 {
    let out = tokio::process::Command::new("du")
        .args(["--apparent-size", "-sb", path])
        .output()
        .await;
    if let Ok(o) = out {
        if o.status.success() {
            if let Ok(s) = std::str::from_utf8(&o.stdout) {
                if let Some(n) = s.split_whitespace().next() {
                    return n.parse::<u64>().unwrap_or(0);
                }
            }
        }
    }
    0
}

/// Count inodes (files + directories) under a path via `du --inodes -s`.
async fn count_inodes(path: &str) -> u64 {
    let out = tokio::process::Command::new("du")
        .args(["--inodes", "-s", path])
        .output()
        .await;
    if let Ok(o) = out {
        if o.status.success() {
            if let Ok(s) = std::str::from_utf8(&o.stdout) {
                if let Some(n) = s.split_whitespace().next() {
                    return n.parse::<u64>().unwrap_or(0);
                }
            }
        }
    }
    0
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
            // identical for the same image across tags and registries. Fall back
            // to the manifest digest if the config blob isn't present (e.g. an
            // older or locally-built image).
            let config_digest = match tokio::fs::read(dir.join("oci-config.json")).await {
                Ok(bytes) => format!("sha256:{:x}", Sha256::digest(&bytes)),
                Err(_) => m.digest.clone(),
            };
            out.push(StoredImage {
                reference: m.reference,
                manifest_digest: m.digest,
                config_digest,
                layers: m.layers,
                user: m.config.user,
            });
        }
        out
    }

    async fn layers_size(layers: &[String]) -> u64 {
        let mut total: u64 = 0;
        for layer in layers {
            let digest = layer.strip_prefix("sha256:").unwrap_or(layer.as_str());
            total += dir_disk_usage(&format!("{}/{}", LAYERS_DIR, digest)).await;
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
        images
            .iter()
            .find(|img| {
                img.id == r
                    || img.repo_tags.iter().any(|t| t == r)
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
        let used = dir_disk_usage(LAYERS_DIR).await;
        let usage = FilesystemUsage {
            timestamp: now_ns(),
            fs_id: Some(FilesystemIdentifier {
                mountpoint: LAYERS_DIR.to_string(),
            }),
            used_bytes: Some(UInt64Value { value: used }),
            inodes_used: Some(UInt64Value {
                value: count_inodes(LAYERS_DIR).await,
            }),
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
