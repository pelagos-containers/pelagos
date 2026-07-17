//! Registry mirror configuration for `pelagos image pull`.
//!
//! Reads `/etc/pelagos/registries.toml` (or the path in `PELAGOS_REGISTRIES`).
//! Format:
//!
//! ```toml
//! [mirrors]
//! "docker.io" = ["http://mirror.local:5000"]
//! "registry.k8s.io" = ["http://mirror.local:5001", "https://fallback.example.com"]
//! ```
//!
//! Each entry maps an origin registry hostname to an ordered list of mirror
//! endpoints.  `pull_with_mirrors` tries each mirror in order before falling
//! back to the origin.

use serde::Deserialize;
use std::collections::HashMap;

const DEFAULT_CONFIG_PATH: &str = "/etc/pelagos/registries.toml";
const ENV_VAR: &str = "PELAGOS_REGISTRIES";

#[derive(Debug, Default, Deserialize)]
struct RegistriesConfig {
    #[serde(default)]
    mirrors: HashMap<String, Vec<String>>,
}

/// Load mirror endpoints for a given origin registry hostname.
/// Returns an empty vec if no config is present or no mirrors are configured
/// for this registry.
pub fn mirrors_for(registry: &str) -> Vec<String> {
    let path = std::env::var(ENV_VAR).unwrap_or_else(|_| DEFAULT_CONFIG_PATH.to_string());

    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return vec![],
    };

    let config: RegistriesConfig = match toml::from_str(&text) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("registries config parse error ({}): {}", path, e);
            return vec![];
        }
    };

    // Normalise the lookup key: strip default ports and trailing slashes.
    let key = normalise_registry_key(registry);
    config.mirrors.get(&key).cloned().unwrap_or_default()
}

/// Rewrite `reference` so that pulls go to `mirror_endpoint` instead of the
/// origin registry.
///
/// Example:
///   reference      = "docker.io/library/alpine:latest"
///   mirror_endpoint = "http://nazgul:5000"
///   result          = "nazgul:5000/library/alpine:latest"
///
/// The mirror endpoint's scheme is stripped from the reference (oci-client
/// handles http vs https via `ClientConfig`); the host:port is substituted
/// for the origin registry.
pub fn rewrite_reference(reference: &str, mirror_endpoint: &str) -> String {
    // Strip scheme from mirror endpoint to get host[:port].
    let mirror_host = mirror_endpoint
        .trim_end_matches('/')
        .trim_start_matches("https://")
        .trim_start_matches("http://");

    // Find where the registry ends in the reference (first '/').
    // oci-client reference format: [registry/]repository[:tag][@digest]
    if let Some(slash) = reference.find('/') {
        format!("{}/{}", mirror_host, &reference[slash + 1..])
    } else {
        // Bare name — shouldn't happen after normalise_reference, but be safe.
        format!("{}/{}", mirror_host, reference)
    }
}

/// True if the mirror endpoint uses HTTP (not HTTPS), so the caller can pass
/// `insecure = true` to oci_client_config.
pub fn is_insecure_endpoint(endpoint: &str) -> bool {
    endpoint.starts_with("http://")
}

fn normalise_registry_key(registry: &str) -> String {
    // docker.io is sometimes called "index.docker.io" by oci-client internals.
    let r = registry
        .trim_end_matches('/')
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    if r == "index.docker.io" {
        return "docker.io".to_string();
    }
    r.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rewrite_reference_docker() {
        let out = rewrite_reference("docker.io/library/alpine:latest", "http://nazgul:5000");
        assert_eq!(out, "nazgul:5000/library/alpine:latest");
    }

    #[test]
    fn test_rewrite_reference_https_mirror() {
        let out = rewrite_reference("registry.k8s.io/pause:3.9", "https://mirror.example.com");
        assert_eq!(out, "mirror.example.com/pause:3.9");
    }

    #[test]
    fn test_rewrite_reference_ecr_public_path_prefix() {
        // ECR Public mirrors Docker Hub official images under a `/docker` path
        // prefix (public.ecr.aws/docker/library/<image>). A mirror endpoint with
        // a path component must be preserved so the rewritten reference resolves
        // to the correct ECR repository. This is what the CI mirror config relies
        // on (ci/registries.toml, issue #388).
        let out = rewrite_reference(
            "docker.io/library/alpine:latest",
            "https://public.ecr.aws/docker",
        );
        assert_eq!(out, "public.ecr.aws/docker/library/alpine:latest");
        // Trailing slash on the endpoint must not double up.
        let out = rewrite_reference(
            "docker.io/library/busybox:1.36",
            "https://public.ecr.aws/docker/",
        );
        assert_eq!(out, "public.ecr.aws/docker/library/busybox:1.36");
    }

    #[test]
    fn test_is_insecure_endpoint() {
        assert!(is_insecure_endpoint("http://nazgul:5000"));
        assert!(!is_insecure_endpoint("https://mirror.example.com"));
    }

    #[test]
    fn test_normalise_registry_key_index_docker_io() {
        assert_eq!(normalise_registry_key("index.docker.io"), "docker.io");
    }

    #[test]
    fn test_rewrite_reference_multiarch_child_digest() {
        // When pull_image resolves a multi-arch image index it uses
        // `oci_ref.clone_with_digest(child_digest)` to build the child reference.
        // That produces `repo@sha256:hex`, NOT the bare `sha256:hex`.  Verify
        // that rewrite_reference handles the `repo@digest` form correctly so the
        // mirror receives `/v2/library/alpine/manifests/sha256:...` and not the
        // mangled `/v2/library/sha256/manifests/...` (#407).
        let digest = "sha256:3805b9089afd837fcf858f26cbb4422ef713b95e31645402464024ccad3a926f";
        // The reference built by clone_with_digest on `docker.io/library/alpine:latest`
        // comes out as `docker.io/library/alpine@sha256:...`.
        let child_ref = format!("docker.io/library/alpine@{}", digest);
        let out = rewrite_reference(&child_ref, "http://nazgul:5000");
        assert_eq!(
            out,
            format!("nazgul:5000/library/alpine@{}", digest),
            "child manifest reference must preserve the image repo, not mangle digest to repo name"
        );
    }
}
