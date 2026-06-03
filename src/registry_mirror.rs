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
    fn test_is_insecure_endpoint() {
        assert!(is_insecure_endpoint("http://nazgul:5000"));
        assert!(!is_insecure_endpoint("https://mirror.example.com"));
    }

    #[test]
    fn test_normalise_registry_key_index_docker_io() {
        assert_eq!(normalise_registry_key("index.docker.io"), "docker.io");
    }
}
