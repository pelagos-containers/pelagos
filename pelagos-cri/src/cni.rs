//! CNI plugin invocation for pod sandbox networking.
//!
//! Implements the standard CNI calling convention for both `.conf` and
//! `.conflist` files.  For `.conflist`, plugins are called in order (ADD)
//! or reverse order (DEL) with each plugin's result forwarded as `prevResult`
//! to the next.
//!
//! When no CNI config is present (e.g. crictl standalone testing), callers
//! should fall back to pelagos native bridge networking.

use serde::Deserialize;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const CNI_CONF_DIR: &str = "/etc/cni/net.d";
const CNI_BIN_DIRS: &[&str] = &[
    "/opt/cni/bin",
    "/var/lib/rancher/k3s/data/current/bin",
    "/usr/lib/cni",
    "/usr/libexec/cni",
];

// ── Config file types ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ConfList {
    name: String,
    #[serde(rename = "cniVersion")]
    cni_version: String,
    plugins: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct Conf {
    #[serde(rename = "type")]
    plugin_type: String,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Returns the path to the first CNI config file found in `/etc/cni/net.d/`,
/// sorted lexicographically.  Returns `None` if the directory is absent or empty.
pub fn find_cni_conf() -> Option<PathBuf> {
    let mut entries: Vec<_> = std::fs::read_dir(CNI_CONF_DIR)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| {
            matches!(
                e.path().extension().and_then(|x| x.to_str()),
                Some("conf") | Some("conflist")
            )
        })
        .collect();
    entries.sort_by_key(|e| e.file_name());
    entries.into_iter().next().map(|e| e.path())
}

/// Create a named network namespace via `ip netns add`.
/// Returns the netns path (`/run/netns/<name>`) on success.
pub fn create_netns(name: &str) -> Result<String, String> {
    let out = Command::new("ip")
        .args(["netns", "add", name])
        .output()
        .map_err(|e| format!("ip netns add: {}", e))?;
    if !out.status.success() {
        return Err(format!(
            "ip netns add {} failed: {}",
            name,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(format!("/run/netns/{}", name))
}

/// Delete a named network namespace.  Best-effort; errors are ignored.
pub fn delete_netns(name: &str) {
    let _ = Command::new("ip").args(["netns", "del", name]).output();
}

/// Run CNI ADD for a sandbox.
/// Returns the assigned IPv4 address (without prefix length) on success.
pub fn cni_add(sandbox_id: &str, netns_path: &str, conf_path: &Path) -> Result<String, String> {
    let result = invoke_cni("ADD", sandbox_id, netns_path, conf_path)?;
    let ip = result
        .get("ips")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|ip| ip.get("address"))
        .and_then(|a| a.as_str())
        .map(|s| s.split('/').next().unwrap_or(s).to_string())
        .unwrap_or_default();
    Ok(ip)
}

/// Run CNI DEL for a sandbox.  Best-effort; errors are logged but not returned.
pub fn cni_del(sandbox_id: &str, netns_path: &str, conf_path: &Path) {
    if let Err(e) = invoke_cni("DEL", sandbox_id, netns_path, conf_path) {
        log::warn!("CNI DEL for {}: {}", sandbox_id, e);
    }
}

// ── Internals ─────────────────────────────────────────────────────────────────

fn cni_path_env() -> String {
    CNI_BIN_DIRS.join(":")
}

fn find_plugin_bin(plugin_type: &str) -> Result<PathBuf, String> {
    for dir in CNI_BIN_DIRS {
        let p = Path::new(dir).join(plugin_type);
        if p.is_file() {
            return Ok(p);
        }
    }
    Err(format!(
        "CNI plugin '{}' not found in CNI_BIN_DIRS",
        plugin_type
    ))
}

/// Run a single CNI plugin binary.  Returns the parsed JSON result, or `None`
/// for DEL responses (which may have empty stdout).
fn run_plugin(
    command: &str,
    sandbox_id: &str,
    netns_path: &str,
    plugin_type: &str,
    config_json: &str,
) -> Result<Option<serde_json::Value>, String> {
    let bin = find_plugin_bin(plugin_type)?;

    let mut child = Command::new(&bin)
        .env("CNI_COMMAND", command)
        .env("CNI_CONTAINERID", sandbox_id)
        .env("CNI_NETNS", netns_path)
        .env("CNI_IFNAME", "eth0")
        .env("CNI_PATH", cni_path_env())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn '{}': {}", plugin_type, e))?;

    child
        .stdin
        .take()
        .unwrap()
        .write_all(config_json.as_bytes())
        .map_err(|e| format!("write stdin to '{}': {}", plugin_type, e))?;

    let out = child
        .wait_with_output()
        .map_err(|e| format!("wait '{}': {}", plugin_type, e))?;

    if !out.status.success() {
        return Err(format!(
            "{} '{}' failed (exit {:?}): {}",
            command,
            plugin_type,
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }

    if out.stdout.is_empty() {
        return Ok(None);
    }

    serde_json::from_slice(&out.stdout).map(Some).map_err(|e| {
        format!(
            "parse {} result from '{}': {} (raw: {})",
            command,
            plugin_type,
            e,
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

/// Dispatch to conflist or single-conf invoker based on file extension.
fn invoke_cni(
    command: &str,
    sandbox_id: &str,
    netns_path: &str,
    conf_path: &Path,
) -> Result<serde_json::Value, String> {
    let raw = std::fs::read_to_string(conf_path)
        .map_err(|e| format!("read {}: {}", conf_path.display(), e))?;

    if conf_path.extension().and_then(|x| x.to_str()) == Some("conflist") {
        invoke_conflist(command, sandbox_id, netns_path, &raw)
    } else {
        invoke_conf(command, sandbox_id, netns_path, &raw)
    }
}

fn invoke_conf(
    command: &str,
    sandbox_id: &str,
    netns_path: &str,
    raw: &str,
) -> Result<serde_json::Value, String> {
    let conf: Conf = serde_json::from_str(raw).map_err(|e| format!("parse .conf: {}", e))?;
    Ok(
        run_plugin(command, sandbox_id, netns_path, &conf.plugin_type, raw)?
            .unwrap_or_else(|| serde_json::json!({})),
    )
}

/// For a conflist, call each plugin in order (ADD) or reverse (DEL), forwarding
/// the result of each plugin as `prevResult` to the next.
fn invoke_conflist(
    command: &str,
    sandbox_id: &str,
    netns_path: &str,
    raw: &str,
) -> Result<serde_json::Value, String> {
    let conflist: ConfList =
        serde_json::from_str(raw).map_err(|e| format!("parse .conflist: {}", e))?;

    let plugins: Vec<serde_json::Value> = if command == "DEL" {
        conflist.plugins.iter().rev().cloned().collect()
    } else {
        conflist.plugins.clone()
    };

    let mut prev_result: Option<serde_json::Value> = None;
    let mut last_result = serde_json::json!({});

    for plugin_conf in &plugins {
        let plugin_type = plugin_conf
            .get("type")
            .and_then(|t| t.as_str())
            .ok_or_else(|| "conflist plugin missing 'type' field".to_string())?;

        // Build the per-plugin config: conflist header + plugin stanza + prevResult.
        let mut config = serde_json::json!({
            "cniVersion": conflist.cni_version,
            "name": conflist.name,
        });
        if let Some(obj) = plugin_conf.as_object() {
            for (k, v) in obj {
                config[k] = v.clone();
            }
        }
        if let Some(ref pr) = prev_result {
            config["prevResult"] = pr.clone();
        }

        let config_str = serde_json::to_string(&config)
            .map_err(|e| format!("serialize config for '{}': {}", plugin_type, e))?;

        if let Some(result) = run_plugin(command, sandbox_id, netns_path, plugin_type, &config_str)?
        {
            prev_result = Some(result.clone());
            last_result = result;
        }
    }

    Ok(last_result)
}
