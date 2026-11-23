//! Per-container OCI bundle directory and persisted shim address.
//!
//! Layout: `<root>/<namespace>/<id>/{config.json,address,shim.json,rootfs/}`.
//! The bundle is the shim's cwd and holds `config.json`, which Kata's `start`
//! reads. The `address` file and `shim.json` let a restarted `pelagos-cri`
//! reconnect to a live shim (re-adoption item of epic #574).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::bootstrap::ShimAddress;

/// Persisted shim metadata (`shim.json`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShimRecord {
    pub namespace: String,
    pub id: String,
    pub binary: String,
    pub address: String,
    pub version: u32,
}

#[derive(Debug, Clone)]
pub struct Bundle {
    dir: PathBuf,
    namespace: String,
    id: String,
}

fn valid_component(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && !s.contains('/') && !s.contains('\0')
}

impl Bundle {
    /// Create the bundle directory (idempotent). Rejects path-unsafe ids.
    pub fn create(root: &Path, namespace: &str, id: &str) -> io::Result<Self> {
        if !valid_component(namespace) || !valid_component(id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid namespace or id for bundle path",
            ));
        }
        let dir = root.join(namespace).join(id);
        fs::create_dir_all(dir.join("rootfs"))?;
        Ok(Self {
            dir,
            namespace: namespace.into(),
            id: id.into(),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn write_config(&self, config_json: &[u8]) -> io::Result<()> {
        write_atomic(&self.dir.join("config.json"), config_json)
    }

    /// Persist the shim address (plain `address` file, as containerd does)
    /// and the full record.
    pub fn persist_shim(&self, binary: &str, addr: &ShimAddress) -> io::Result<()> {
        write_atomic(&self.dir.join("address"), addr.address.as_bytes())?;
        let rec = ShimRecord {
            namespace: self.namespace.clone(),
            id: self.id.clone(),
            binary: binary.into(),
            address: addr.address.clone(),
            version: addr.version,
        };
        let json = serde_json::to_vec_pretty(&rec).map_err(io::Error::other)?;
        write_atomic(&self.dir.join("shim.json"), &json)
    }

    pub fn load_shim(&self) -> io::Result<ShimRecord> {
        let data = fs::read(self.dir.join("shim.json"))?;
        serde_json::from_slice(&data).map_err(io::Error::other)
    }

    /// Open an existing bundle without creating it.
    pub fn open(root: &Path, namespace: &str, id: &str) -> io::Result<Self> {
        if !valid_component(namespace) || !valid_component(id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid namespace or id for bundle path",
            ));
        }
        let dir = root.join(namespace).join(id);
        if !dir.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "bundle directory not found",
            ));
        }
        Ok(Self {
            dir,
            namespace: namespace.into(),
            id: id.into(),
        })
    }

    /// Remove the bundle directory. Callers must have run the shim `delete`
    /// first so the shim has unmounted `rootfs/`.
    pub fn remove(&self) -> io::Result<()> {
        match fs::remove_dir_all(&self.dir) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            r => r,
        }
    }
}

/// Enumerate persisted shim records under `root` (for re-adoption).
pub fn list_records(root: &Path) -> Vec<ShimRecord> {
    let mut out = Vec::new();
    let Ok(nss) = fs::read_dir(root) else {
        return out;
    };
    for ns in nss.flatten() {
        let Ok(ids) = fs::read_dir(ns.path()) else {
            continue;
        };
        for id in ids.flatten() {
            if let Ok(data) = fs::read(id.path().join("shim.json")) {
                if let Ok(rec) = serde_json::from_slice::<ShimRecord>(&data) {
                    out.push(rec);
                }
            }
        }
    }
    out
}

fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, data)?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr() -> ShimAddress {
        ShimAddress {
            address: "unix:///run/s/x".into(),
            version: 3,
            protocol: "ttrpc".into(),
        }
    }

    #[test]
    fn create_persist_load_remove() {
        let t = tempfile::tempdir().unwrap();
        let b = Bundle::create(t.path(), "k8s.io", "abc").unwrap();
        assert!(b.dir().join("rootfs").is_dir());
        b.write_config(b"{}").unwrap();
        assert_eq!(fs::read(b.dir().join("config.json")).unwrap(), b"{}");
        b.persist_shim("containerd-shim-kata-v2", &addr()).unwrap();
        assert_eq!(
            fs::read_to_string(b.dir().join("address")).unwrap(),
            "unix:///run/s/x"
        );
        let rec = b.load_shim().unwrap();
        assert_eq!(rec.address, "unix:///run/s/x");
        assert_eq!(rec.version, 3);
        assert_eq!(list_records(t.path()), vec![rec]);
        b.remove().unwrap();
        assert!(!b.dir().exists());
        b.remove().unwrap(); // idempotent
    }

    #[test]
    fn rejects_unsafe_ids() {
        let t = tempfile::tempdir().unwrap();
        for bad in ["", ".", "..", "a/b"] {
            assert!(Bundle::create(t.path(), "k8s.io", bad).is_err());
            assert!(Bundle::create(t.path(), bad, "x").is_err());
            assert!(Bundle::open(t.path(), "k8s.io", bad).is_err());
        }
    }

    #[test]
    fn open_missing_is_not_found() {
        let t = tempfile::tempdir().unwrap();
        let e = Bundle::open(t.path(), "k8s.io", "nope").unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
    }
}
