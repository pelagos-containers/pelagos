//! CDI (Container Device Interface) spec parsing and resolution.
//!
//! CDI is a sibling spec to the OCI Runtime Spec — <https://github.com/cncf-tags/container-device-interface>.
//! Vendor tooling (e.g. `nvidia-ctk cdi generate`) writes declarative spec files
//! describing what a fully-qualified device name (`vendor.com/class=name`, e.g.
//! `nvidia.com/gpu=all`) needs injected into a container: device nodes, mounts,
//! env vars, and hooks. The spec itself defines the default well-known search
//! directories (`/etc/cdi`, `/var/run/cdi`) — there is no registry or discovery
//! protocol; every CDI-aware engine just agrees to scan those paths by convention.
//!
//! Pelagos implements CDI resolution at the OCI bundle layer (`pelagos create`):
//! when `config.json` carries `cdi.k8s.io/*`-prefixed annotations naming CDI
//! devices, we parse the on-disk specs, resolve those names, and merge the
//! resulting device nodes / mounts / env / hooks into the config before spawn.

use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::PathBuf;

/// Default CDI spec search directories, per the CDI spec's well-known-locations
/// convention. `/var/run/cdi` takes priority over `/etc/cdi` for devices defined
/// in both (dynamic overrides static) — see `CdiRegistry::load`.
pub fn default_spec_dirs() -> Vec<PathBuf> {
    vec![PathBuf::from("/etc/cdi"), PathBuf::from("/var/run/cdi")]
}

/// Annotation key prefix used to request CDI devices in an OCI bundle's
/// `config.json` `annotations` map, matching the convention used by
/// CRI-O/containerd before Kubernetes grew a native `CDIDevices` CRI field:
/// `cdi.k8s.io/<claim-name>: <comma-separated fully-qualified CDI device names>`.
pub const ANNOTATION_PREFIX: &str = "cdi.k8s.io/";

#[derive(Debug, thiserror::Error)]
pub enum CdiError {
    #[error("invalid CDI device name '{0}': expected 'vendor.com/class=name'")]
    InvalidDeviceName(String),
    #[error("CDI device '{0}' not found in any loaded spec")]
    DeviceNotFound(String),
    #[error("failed to read CDI spec dir {0}: {1}")]
    ReadDir(PathBuf, io::Error),
    #[error("failed to parse CDI spec {0}: {1}")]
    Parse(PathBuf, String),
}

// ---------------------------------------------------------------------------
// CDI spec file schema (subset of fields Pelagos consumes)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ContainerEdits {
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default, rename = "deviceNodes")]
    pub device_nodes: Vec<CdiDeviceNode>,
    #[serde(default)]
    pub mounts: Vec<CdiMount>,
    #[serde(default)]
    pub hooks: Vec<CdiHook>,
}

impl ContainerEdits {
    fn merge(&mut self, other: &ContainerEdits) {
        self.env.extend(other.env.iter().cloned());
        self.device_nodes.extend(other.device_nodes.iter().cloned());
        self.mounts.extend(other.mounts.iter().cloned());
        self.hooks.extend(other.hooks.iter().cloned());
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CdiDeviceNode {
    pub path: String,
    /// Host path the node should be created from, if different from `path`.
    #[serde(rename = "hostPath")]
    pub host_path: Option<String>,
    #[serde(rename = "type", default = "default_device_type")]
    pub kind: String,
    pub major: Option<i64>,
    pub minor: Option<i64>,
    #[serde(rename = "fileMode")]
    pub file_mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
}

fn default_device_type() -> String {
    "c".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct CdiMount {
    #[serde(rename = "hostPath")]
    pub host_path: String,
    #[serde(rename = "containerPath")]
    pub container_path: String,
    #[serde(default)]
    pub options: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CdiHook {
    #[serde(rename = "hookName")]
    pub hook_name: String,
    pub path: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
}

impl CdiHook {
    /// If this hook's `args` invoke a `create-symlinks` subcommand with
    /// `--link TARGET::LINK_PATH` pairs — the convention `nvidia-ctk cdi
    /// generate` embeds to recreate the driver's SONAME symlink chain, e.g.
    /// `libnvidia-ml.so.580.173.02` -> `libnvidia-ml.so.1` ->
    /// `libnvidia-ml.so` — parse those pairs out. `target` is the (relative)
    /// symlink target, `link_path` is the absolute path (inside the
    /// container) where the symlink should be created, matching `ln -sf
    /// TARGET LINK_PATH` semantics.
    ///
    /// Deliberately does NOT check `path` / the invoking binary's name.
    /// nvidia-container-toolkit has shipped this exact `create-symlinks
    /// --link A::B` convention under at least two different binaries
    /// (`nvidia-ctk hook create-symlinks` on older toolkit versions,
    /// `nvidia-cdi-hook create-symlinks` on 1.17+) — see #518, where
    /// pattern-matching on `path == ".../nvidia-ctk"` silently broke on a
    /// system using the newer binary. The `--link` args fully describe the
    /// hook's behavior regardless of what its binary happens to be called,
    /// so recognition is keyed on the args shape alone.
    ///
    /// Returns `None` for any hook that isn't this convention — Pelagos does
    /// not execute arbitrary CDI hook binaries on the CLI path (see module
    /// docs for why: by the time a createContainer-equivalent point is
    /// reached, pelagos has already chrooted, so a host-side hook binary
    /// would not be found inside the container's rootfs). `create-symlinks`
    /// is handled as a native built-in instead, since its behavior is fully
    /// described by its own args and needs no external binary.
    pub fn create_symlinks_pairs(&self) -> Option<Vec<(String, String)>> {
        if !self.args.iter().any(|a| a == "create-symlinks") {
            return None;
        }
        let mut pairs = Vec::new();
        let mut iter = self.args.iter();
        while let Some(arg) = iter.next() {
            if arg == "--link" {
                if let Some(spec) = iter.next() {
                    if let Some((target, link_path)) = spec.split_once("::") {
                        pairs.push((target.to_string(), link_path.to_string()));
                    }
                }
            }
        }
        Some(pairs)
    }

    /// If this hook's `args` invoke an `update-ldcache` subcommand with a
    /// `--folder DIR` argument — the convention `nvidia-ctk cdi generate`
    /// embeds to refresh the dynamic linker cache for a directory of driver
    /// libraries dropped in alongside the `create-symlinks` hook above — return
    /// that directory. See #529: without this, libraries resolved via the
    /// ldconfig cache (Triton, PyTorch's inductor backend) aren't found even
    /// though `create-symlinks` placed them correctly on disk.
    ///
    /// Same rationale as `create_symlinks_pairs()` for not checking `path`/the
    /// invoking binary's name, and for being handled as a native built-in
    /// (`ldconfig <folder>`, run inside the container's own mount namespace)
    /// rather than executing the host's hook binary.
    pub fn update_ldcache_folder(&self) -> Option<String> {
        if !self.args.iter().any(|a| a == "update-ldcache") {
            return None;
        }
        let mut iter = self.args.iter();
        while let Some(arg) = iter.next() {
            if arg == "--folder" {
                return iter.next().cloned();
            }
        }
        None
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CdiDevice {
    pub name: String,
    #[serde(rename = "containerEdits", default)]
    pub container_edits: ContainerEdits,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CdiSpec {
    #[allow(dead_code)]
    #[serde(rename = "cdiVersion")]
    pub cdi_version: String,
    pub kind: String,
    #[serde(default)]
    pub devices: Vec<CdiDevice>,
    #[serde(rename = "containerEdits", default)]
    pub container_edits: ContainerEdits,
}

// ---------------------------------------------------------------------------
// Registry: parsed specs, keyed by "vendor.com/class"
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct CdiRegistry {
    /// kind ("vendor.com/class") -> spec
    specs: HashMap<String, CdiSpec>,
}

impl CdiRegistry {
    /// Scan `dirs` in order and parse every `*.yaml`/`*.yml`/`*.json` CDI spec
    /// file found. Later directories override earlier ones for the same kind,
    /// matching the CDI convention that dynamic (`/var/run/cdi`) specs take
    /// priority over static (`/etc/cdi`) ones. Missing directories are skipped
    /// (not an error — a host with no GPU simply has no `/etc/cdi`).
    pub fn load(dirs: &[PathBuf]) -> Result<Self, CdiError> {
        let mut specs = HashMap::new();
        for dir in dirs {
            let entries = match fs::read_dir(dir) {
                Ok(e) => e,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(CdiError::ReadDir(dir.clone(), e)),
            };
            let mut paths: Vec<PathBuf> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    matches!(
                        p.extension().and_then(|e| e.to_str()),
                        Some("yaml") | Some("yml") | Some("json")
                    )
                })
                .collect();
            paths.sort();
            for path in paths {
                let content = fs::read_to_string(&path)
                    .map_err(|e| CdiError::Parse(path.clone(), e.to_string()))?;
                let spec: CdiSpec = serde_yaml::from_str(&content)
                    .map_err(|e| CdiError::Parse(path.clone(), e.to_string()))?;
                specs.insert(spec.kind.clone(), spec);
            }
        }
        Ok(Self { specs })
    }

    /// Resolve a fully-qualified CDI device name (`vendor.com/class=name`) into
    /// the `ContainerEdits` it requires. `name` of `all` resolves to the union
    /// of every device in that kind's spec (matching CDI's `=all` convention).
    pub fn resolve(&self, qualified_name: &str) -> Result<ContainerEdits, CdiError> {
        let (kind, name) = qualified_name
            .rsplit_once('=')
            .ok_or_else(|| CdiError::InvalidDeviceName(qualified_name.to_string()))?;
        if kind.is_empty() || name.is_empty() || !kind.contains('/') {
            return Err(CdiError::InvalidDeviceName(qualified_name.to_string()));
        }
        let spec = self
            .specs
            .get(kind)
            .ok_or_else(|| CdiError::DeviceNotFound(qualified_name.to_string()))?;

        let mut edits = ContainerEdits::default();
        if name == "all" {
            for dev in &spec.devices {
                edits.merge(&dev.container_edits);
            }
        } else {
            let dev = spec
                .devices
                .iter()
                .find(|d| d.name == name)
                .ok_or_else(|| CdiError::DeviceNotFound(qualified_name.to_string()))?;
            edits.merge(&dev.container_edits);
        }
        // Spec-level edits apply to every device drawn from that spec.
        edits.merge(&spec.container_edits);
        Ok(edits)
    }

    /// Resolve multiple qualified device names, merging their edits together.
    pub fn resolve_all(&self, names: &[String]) -> Result<ContainerEdits, CdiError> {
        let mut edits = ContainerEdits::default();
        for name in names {
            edits.merge(&self.resolve(name)?);
        }
        Ok(edits)
    }
}

/// Collect requested CDI device names from an OCI bundle's annotations map:
/// every value under a `cdi.k8s.io/`-prefixed key, comma-separated.
pub fn devices_from_annotations(annotations: &HashMap<String, String>) -> Vec<String> {
    let mut names = Vec::new();
    let mut keys: Vec<&String> = annotations
        .keys()
        .filter(|k| k.starts_with(ANNOTATION_PREFIX))
        .collect();
    keys.sort();
    for key in keys {
        let value = &annotations[key];
        for part in value.split(',') {
            let part = part.trim();
            if !part.is_empty() {
                names.push(part.to_string());
            }
        }
    }
    names
}

/// Directories to scan for CDI specs. Honors `PELAGOS_CDI_SPEC_DIRS` (colon-separated)
/// for tests and non-standard installs; falls back to the spec's default locations.
pub fn spec_dirs() -> Vec<PathBuf> {
    match std::env::var("PELAGOS_CDI_SPEC_DIRS") {
        Ok(val) if !val.is_empty() => val.split(':').map(PathBuf::from).collect(),
        _ => default_spec_dirs(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn sample_spec() -> &'static str {
        r#"
cdiVersion: "0.6.0"
kind: "vendor.com/gpu"
devices:
  - name: "0"
    containerEdits:
      deviceNodes:
        - path: "/dev/gpu0"
          type: "c"
          major: 195
          minor: 0
      mounts:
        - hostPath: "/usr/lib/gpu/libgpu.so.1"
          containerPath: "/usr/lib/gpu/libgpu.so.1"
          options: ["ro", "bind"]
  - name: "1"
    containerEdits:
      deviceNodes:
        - path: "/dev/gpu1"
          type: "c"
          major: 195
          minor: 1
containerEdits:
  env:
    - "GPU_VISIBLE=1"
"#
    }

    fn write_spec(dir: &Path, name: &str, contents: &str) {
        fs::write(dir.join(name), contents).unwrap();
    }

    #[test]
    fn resolve_single_device() {
        let tmp = tempfile::tempdir().unwrap();
        write_spec(tmp.path(), "vendor.yaml", sample_spec());
        let reg = CdiRegistry::load(&[tmp.path().to_path_buf()]).unwrap();

        let edits = reg.resolve("vendor.com/gpu=0").unwrap();
        assert_eq!(edits.device_nodes.len(), 1);
        assert_eq!(edits.device_nodes[0].path, "/dev/gpu0");
        assert_eq!(edits.mounts.len(), 1);
        // Spec-level edits are always merged in.
        assert_eq!(edits.env, vec!["GPU_VISIBLE=1".to_string()]);
    }

    #[test]
    fn resolve_all_aggregates_devices() {
        let tmp = tempfile::tempdir().unwrap();
        write_spec(tmp.path(), "vendor.yaml", sample_spec());
        let reg = CdiRegistry::load(&[tmp.path().to_path_buf()]).unwrap();

        let edits = reg.resolve("vendor.com/gpu=all").unwrap();
        assert_eq!(edits.device_nodes.len(), 2);
        assert_eq!(edits.env, vec!["GPU_VISIBLE=1".to_string()]);
    }

    #[test]
    fn unknown_device_errors() {
        let tmp = tempfile::tempdir().unwrap();
        write_spec(tmp.path(), "vendor.yaml", sample_spec());
        let reg = CdiRegistry::load(&[tmp.path().to_path_buf()]).unwrap();

        assert!(matches!(
            reg.resolve("vendor.com/gpu=99"),
            Err(CdiError::DeviceNotFound(_))
        ));
        assert!(matches!(
            reg.resolve("vendor.com/gpu"),
            Err(CdiError::InvalidDeviceName(_))
        ));
    }

    #[test]
    fn missing_spec_dir_is_not_an_error() {
        let reg = CdiRegistry::load(&[PathBuf::from("/no/such/cdi/dir")]).unwrap();
        assert!(reg.specs.is_empty());
    }

    #[test]
    fn later_dir_overrides_earlier_for_same_kind() {
        let etc = tempfile::tempdir().unwrap();
        let run = tempfile::tempdir().unwrap();
        write_spec(etc.path(), "vendor.yaml", sample_spec());
        // /run copy only has device "0", not "1" — proves it fully replaced /etc's spec.
        write_spec(
            run.path(),
            "vendor.yaml",
            r#"
cdiVersion: "0.6.0"
kind: "vendor.com/gpu"
devices:
  - name: "0"
    containerEdits:
      deviceNodes:
        - path: "/dev/gpu0-override"
          type: "c"
          major: 195
          minor: 0
"#,
        );
        let reg = CdiRegistry::load(&[etc.path().to_path_buf(), run.path().to_path_buf()]).unwrap();
        let edits = reg.resolve("vendor.com/gpu=0").unwrap();
        assert_eq!(edits.device_nodes[0].path, "/dev/gpu0-override");
        assert!(reg.resolve("vendor.com/gpu=1").is_err());
    }

    #[test]
    fn devices_from_annotations_collects_prefixed_keys() {
        let mut ann = HashMap::new();
        ann.insert(
            "cdi.k8s.io/claim1".to_string(),
            "vendor.com/gpu=0, vendor.com/gpu=1".to_string(),
        );
        ann.insert("other.annotation".to_string(), "ignored".to_string());
        let names = devices_from_annotations(&ann);
        assert_eq!(names, vec!["vendor.com/gpu=0", "vendor.com/gpu=1"]);
    }

    #[test]
    fn nvidia_create_symlinks_parses_link_pairs() {
        let hook = CdiHook {
            hook_name: "createContainer".to_string(),
            path: "/usr/bin/nvidia-ctk".to_string(),
            args: vec![
                "nvidia-ctk".to_string(),
                "hook".to_string(),
                "create-symlinks".to_string(),
                "--link".to_string(),
                "libnvidia-ml.so.580.173.02::/usr/lib/aarch64-linux-gnu/libnvidia-ml.so.1"
                    .to_string(),
                "--link".to_string(),
                "libnvidia-ml.so.1::/usr/lib/aarch64-linux-gnu/libnvidia-ml.so".to_string(),
            ],
            env: vec![],
        };
        let pairs = hook.create_symlinks_pairs().unwrap();
        assert_eq!(
            pairs,
            vec![
                (
                    "libnvidia-ml.so.580.173.02".to_string(),
                    "/usr/lib/aarch64-linux-gnu/libnvidia-ml.so.1".to_string()
                ),
                (
                    "libnvidia-ml.so.1".to_string(),
                    "/usr/lib/aarch64-linux-gnu/libnvidia-ml.so".to_string()
                ),
            ]
        );
    }

    #[test]
    fn create_symlinks_recognized_regardless_of_binary_name() {
        // Regression test for #518: nvidia-container-toolkit 1.17+ ships this
        // exact create-symlinks/--link convention under a different binary,
        // `nvidia-cdi-hook`, not `nvidia-ctk`. Recognition must not depend on
        // `path`.
        let hook = CdiHook {
            hook_name: "createContainer".to_string(),
            path: "/usr/bin/nvidia-cdi-hook".to_string(),
            args: vec![
                "nvidia-cdi-hook".to_string(),
                "create-symlinks".to_string(),
                "--link".to_string(),
                "libnvidia-ml.so.580.173.02::/usr/lib/aarch64-linux-gnu/libnvidia-ml.so.1"
                    .to_string(),
                "--link".to_string(),
                "libnvidia-ml.so.1::/usr/lib/aarch64-linux-gnu/libnvidia-ml.so".to_string(),
            ],
            env: vec![],
        };
        let pairs = hook.create_symlinks_pairs().unwrap();
        assert_eq!(
            pairs,
            vec![
                (
                    "libnvidia-ml.so.580.173.02".to_string(),
                    "/usr/lib/aarch64-linux-gnu/libnvidia-ml.so.1".to_string()
                ),
                (
                    "libnvidia-ml.so.1".to_string(),
                    "/usr/lib/aarch64-linux-gnu/libnvidia-ml.so".to_string()
                ),
            ]
        );
    }

    #[test]
    fn nvidia_create_symlinks_ignores_other_hooks() {
        let hook = CdiHook {
            hook_name: "createContainer".to_string(),
            path: "/usr/bin/some-other-tool".to_string(),
            args: vec!["some-other-tool".to_string(), "do-something".to_string()],
            env: vec![],
        };
        assert!(hook.create_symlinks_pairs().is_none());
    }

    #[test]
    fn update_ldcache_parses_folder_arg() {
        // Regression test for #529: the CDI spec's second createContainer hook,
        // alongside create-symlinks, must also be recognized natively.
        let hook = CdiHook {
            hook_name: "createContainer".to_string(),
            path: "/usr/bin/nvidia-cdi-hook".to_string(),
            args: vec![
                "nvidia-cdi-hook".to_string(),
                "update-ldcache".to_string(),
                "--folder".to_string(),
                "/usr/lib/aarch64-linux-gnu".to_string(),
            ],
            env: vec![],
        };
        assert_eq!(
            hook.update_ldcache_folder(),
            Some("/usr/lib/aarch64-linux-gnu".to_string())
        );
    }

    #[test]
    fn update_ldcache_ignores_other_hooks() {
        let hook = CdiHook {
            hook_name: "createContainer".to_string(),
            path: "/usr/bin/nvidia-cdi-hook".to_string(),
            args: vec![
                "nvidia-cdi-hook".to_string(),
                "disable-device-node-modification".to_string(),
            ],
            env: vec![],
        };
        assert!(hook.update_ldcache_folder().is_none());
    }

    #[test]
    fn update_ldcache_missing_folder_arg_returns_none() {
        let hook = CdiHook {
            hook_name: "createContainer".to_string(),
            path: "/usr/bin/nvidia-cdi-hook".to_string(),
            args: vec!["nvidia-cdi-hook".to_string(), "update-ldcache".to_string()],
            env: vec![],
        };
        assert!(hook.update_ldcache_folder().is_none());
    }
}
