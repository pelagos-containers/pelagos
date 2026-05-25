//! CRI ImageService implementation.

use crate::cri::image_service_server::ImageService;
use crate::cri::{
    Image, ImageFsInfoRequest, ImageFsInfoResponse, ImageSpec, ImageStatusRequest,
    ImageStatusResponse, ListImagesRequest, ListImagesResponse, PullImageRequest,
    PullImageResponse, RemoveImageRequest, RemoveImageResponse, StreamImagesRequest,
    StreamImagesResponse,
};
use crate::invoke::run_pelagos;
use crate::state::AppState;
use serde::Deserialize;
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

#[derive(Deserialize)]
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

pub struct ImageSvc {
    pub state: AppState,
}

impl ImageSvc {
    async fn read_manifests(&self) -> Vec<ImageManifest> {
        let Ok(mut rd) = tokio::fs::read_dir(IMAGES_DIR).await else {
            return Vec::new();
        };
        let mut manifests = Vec::new();
        while let Ok(Some(entry)) = rd.next_entry().await {
            if !entry.path().is_dir() {
                continue;
            }
            let manifest_path = entry.path().join("manifest.json");
            if let Ok(data) = tokio::fs::read_to_string(&manifest_path).await {
                if let Ok(m) = serde_json::from_str::<ImageManifest>(&data) {
                    manifests.push(m);
                }
            }
        }
        manifests
    }

    async fn compute_image_size(manifest: &ImageManifest) -> u64 {
        let mut total: u64 = 0;
        for layer in &manifest.layers {
            // Layers are stored as extracted directories; digest may carry a "sha256:" prefix.
            let digest = layer.strip_prefix("sha256:").unwrap_or(layer.as_str());
            let layer_dir = format!("{}/{}", LAYERS_DIR, digest);
            total += dir_disk_usage(&layer_dir).await;
        }
        // Always report at least 1 byte; kubelet rejects images with Size_ == 0.
        total.max(1)
    }

    fn manifest_to_image(manifest: &ImageManifest, size: u64) -> Image {
        Image {
            id: manifest.digest.clone(),
            repo_tags: vec![manifest.reference.clone()],
            repo_digests: vec![manifest.digest.clone()],
            size,
            uid: None,
            username: String::new(),
            spec: Some(ImageSpec {
                image: manifest.reference.clone(),
                annotations: HashMap::new(),
                ..Default::default()
            }),
            pinned: false,
        }
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
        let manifests = self.read_manifests().await;
        let mut images = Vec::new();
        for m in &manifests {
            let size = Self::compute_image_size(m).await;
            images.push(Self::manifest_to_image(m, size));
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

        let manifests = self.read_manifests().await;
        let found = manifests
            .iter()
            .find(|m| m.reference == image_ref || m.digest == image_ref);

        let image = if let Some(m) = found {
            let size = Self::compute_image_size(m).await;
            Some(Self::manifest_to_image(m, size))
        } else {
            None
        };

        Ok(Response::new(ImageStatusResponse {
            image,
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
            Ok(out) if out.success => Ok(Response::new(PullImageResponse {
                image_ref: image_ref.clone(),
            })),
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

        let bin = self.get_bin().await;
        let _ = run_pelagos(&bin, &["image", "rm", &image_ref]).await;
        Ok(Response::new(RemoveImageResponse {}))
    }

    async fn image_fs_info(
        &self,
        _request: Request<ImageFsInfoRequest>,
    ) -> Result<Response<ImageFsInfoResponse>, Status> {
        use crate::cri::{FilesystemIdentifier, FilesystemUsage, UInt64Value};
        let usage = FilesystemUsage {
            timestamp: 0,
            fs_id: Some(FilesystemIdentifier {
                mountpoint: LAYERS_DIR.to_string(),
            }),
            used_bytes: Some(UInt64Value { value: 0 }),
            inodes_used: Some(UInt64Value { value: 0 }),
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
