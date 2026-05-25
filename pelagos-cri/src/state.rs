//! In-memory CRI state backed by disk at `/run/pelagos-cri/`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

const SANDBOXES_DIR: &str = "/run/pelagos-cri/sandboxes";
const CONTAINERS_DIR: &str = "/run/pelagos-cri/containers";

// ── CRI sandbox metadata ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CriSandbox {
    pub id: String,
    pub name: String,
    pub namespace: String,
    pub uid: String,
    pub attempt: u32,
    pub labels: HashMap<String, String>,
    pub annotations: HashMap<String, String>,
    pub created_at_ns: i64,
    pub state: SandboxState,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SandboxState {
    Running,
    NotReady,
}

// ── CRI container metadata ───────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CriMount {
    pub host_path: String,
    pub container_path: String,
    pub readonly: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CriContainer {
    pub id: String,
    pub sandbox_id: String,
    pub pelagos_name: String,
    pub name: String,
    pub image: String,
    /// CRI `command` field — overrides image ENTRYPOINT.
    pub entrypoint: Vec<String>,
    /// CRI `args` field — overrides image CMD.
    pub args: Vec<String>,
    pub envs: Vec<(String, String)>,
    pub working_dir: String,
    pub mounts: Vec<CriMount>,
    pub labels: HashMap<String, String>,
    pub annotations: HashMap<String, String>,
    pub created_at_ns: i64,
    pub started_at_ns: i64,
    pub finished_at_ns: i64,
    pub state: ContainerState,
    pub exit_code: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ContainerState {
    Created,
    Running,
    Exited,
    Unknown,
}

// ── StateInner ───────────────────────────────────────────────────────────────

pub struct StateInner {
    pub sandboxes: HashMap<String, CriSandbox>,
    pub containers: HashMap<String, CriContainer>,
    pub pelagos_bin: String,
}

impl StateInner {
    fn load() -> Self {
        let sandboxes = load_all_sandboxes();
        let containers = load_all_containers();
        StateInner {
            sandboxes,
            containers,
            pelagos_bin: String::new(),
        }
    }
}

// ── AppState ─────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct AppState {
    pub inner: Arc<Mutex<StateInner>>,
}

impl AppState {
    pub fn new(pelagos_bin: String) -> Self {
        let _ = std::fs::create_dir_all(SANDBOXES_DIR);
        let _ = std::fs::create_dir_all(CONTAINERS_DIR);
        let mut inner = StateInner::load();
        inner.pelagos_bin = pelagos_bin;
        AppState {
            inner: Arc::new(Mutex::new(inner)),
        }
    }
}

// ── Disk helpers ─────────────────────────────────────────────────────────────

pub fn save_sandbox(s: &CriSandbox) -> std::io::Result<()> {
    let _ = std::fs::create_dir_all(SANDBOXES_DIR);
    let path = format!("{}/{}.json", SANDBOXES_DIR, s.id);
    let json = serde_json::to_string(s)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(path, json)
}

pub fn save_container(c: &CriContainer) -> std::io::Result<()> {
    let _ = std::fs::create_dir_all(CONTAINERS_DIR);
    let path = format!("{}/{}.json", CONTAINERS_DIR, c.id);
    let json = serde_json::to_string(c)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(path, json)
}

pub fn remove_sandbox_file(id: &str) {
    let _ = std::fs::remove_file(format!("{}/{}.json", SANDBOXES_DIR, id));
}

pub fn remove_container_file(id: &str) {
    let _ = std::fs::remove_file(format!("{}/{}.json", CONTAINERS_DIR, id));
}

fn load_all_sandboxes() -> HashMap<String, CriSandbox> {
    let Ok(entries) = std::fs::read_dir(SANDBOXES_DIR) else {
        return HashMap::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "json").unwrap_or(false))
        .filter_map(|e| {
            std::fs::read_to_string(e.path())
                .ok()
                .and_then(|d| serde_json::from_str::<CriSandbox>(&d).ok())
        })
        .map(|s| (s.id.clone(), s))
        .collect()
}

fn load_all_containers() -> HashMap<String, CriContainer> {
    let Ok(entries) = std::fs::read_dir(CONTAINERS_DIR) else {
        return HashMap::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "json").unwrap_or(false))
        .filter_map(|e| {
            std::fs::read_to_string(e.path())
                .ok()
                .and_then(|d| serde_json::from_str::<CriContainer>(&d).ok())
        })
        .map(|c| (c.id.clone(), c))
        .collect()
}
