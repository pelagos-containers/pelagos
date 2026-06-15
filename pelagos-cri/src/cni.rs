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

/// CNI config directories searched in order; first file found wins.
/// The k3s agent path is listed first so its managed configs (Flannel, etc.)
/// take priority over stale files that may linger in /etc/cni/net.d/.
const CNI_CONF_DIRS: &[&str] = &["/var/lib/rancher/k3s/agent/etc/cni/net.d", "/etc/cni/net.d"];
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

/// Returns the first CNI config file found by searching `CNI_CONF_DIRS` in order.
/// Within each directory files are sorted lexicographically; the first entry wins.
/// Returns `None` if no config is found in any directory.
pub fn find_cni_conf() -> Option<PathBuf> {
    for dir in CNI_CONF_DIRS {
        let Ok(rd) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut entries: Vec<_> = rd
            .filter_map(|e| e.ok())
            .filter(|e| {
                matches!(
                    e.path().extension().and_then(|x| x.to_str()),
                    Some("conf") | Some("conflist")
                )
            })
            .collect();
        if entries.is_empty() {
            continue;
        }
        entries.sort_by_key(|e| e.file_name());
        return entries.into_iter().next().map(|e| e.path());
    }
    None
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
pub fn cni_add(
    sandbox_id: &str,
    netns_path: &str,
    conf_path: &Path,
    cap_args: &serde_json::Value,
) -> Result<String, String> {
    let result = invoke_cni("ADD", sandbox_id, netns_path, conf_path, cap_args)?;
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
pub fn cni_del(sandbox_id: &str, netns_path: &str, conf_path: &Path, cap_args: &serde_json::Value) {
    if let Err(e) = invoke_cni("DEL", sandbox_id, netns_path, conf_path, cap_args) {
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
    cap_args: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let raw = std::fs::read_to_string(conf_path)
        .map_err(|e| format!("read {}: {}", conf_path.display(), e))?;

    if conf_path.extension().and_then(|x| x.to_str()) == Some("conflist") {
        invoke_conflist(command, sandbox_id, netns_path, &raw, cap_args)
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
/// Build CNI capability args (`{"portMappings":[...]}`) from a sandbox's port
/// mappings, for the `portmap` plugin. Returns an empty object when there are no
/// host ports to map (so no `runtimeConfig` is injected).
pub fn port_mapping_cap_args(mappings: &[crate::state::CriPortMapping]) -> serde_json::Value {
    let pms: Vec<serde_json::Value> = mappings
        .iter()
        .filter(|p| p.host_port > 0 && p.container_port > 0)
        .map(|p| {
            serde_json::json!({
                "hostPort": p.host_port,
                "containerPort": p.container_port,
                "protocol": match p.protocol { 1 => "udp", 2 => "sctp", _ => "tcp" },
                "hostIP": p.host_ip,
            })
        })
        .collect();
    if pms.is_empty() {
        serde_json::json!({})
    } else {
        serde_json::json!({ "portMappings": pms })
    }
}

/// For a plugin that declares capabilities (e.g. `{"capabilities":{"portMappings":true}}`),
/// inject `runtimeConfig.<cap>` from the runtime's capability args. This is how the
/// CNI spec passes host-port mappings to the `portmap` plugin (#354) — without it
/// portmap runs but sets up no DNAT, so host ports are unreachable.
fn capability_runtime_config(
    plugin_conf: &serde_json::Value,
    cap_args: &serde_json::Value,
) -> Option<serde_json::Value> {
    let caps = plugin_conf.get("capabilities")?.as_object()?;
    let mut rc = serde_json::Map::new();
    for (cap, enabled) in caps {
        if enabled.as_bool() == Some(true) {
            if let Some(val) = cap_args.get(cap) {
                rc.insert(cap.clone(), val.clone());
            }
        }
    }
    if rc.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(rc))
    }
}

fn invoke_conflist(
    command: &str,
    sandbox_id: &str,
    netns_path: &str,
    raw: &str,
    cap_args: &serde_json::Value,
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
        if let Some(rc) = capability_runtime_config(plugin_conf, cap_args) {
            config["runtimeConfig"] = rc;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::CriPortMapping;

    #[test]
    fn test_port_mapping_cap_args() {
        // No host ports → empty object (no runtimeConfig injected).
        assert_eq!(port_mapping_cap_args(&[]), serde_json::json!({}));

        let pms = vec![
            CriPortMapping {
                protocol: 0,
                container_port: 80,
                host_port: 8080,
                host_ip: String::new(),
            },
            CriPortMapping {
                protocol: 1,
                container_port: 53,
                host_port: 5353,
                host_ip: "127.0.0.1".into(),
            },
            CriPortMapping {
                protocol: 0,
                container_port: 99,
                host_port: 0,
                host_ip: String::new(),
            }, // dropped
        ];
        let args = port_mapping_cap_args(&pms);
        assert_eq!(
            args,
            serde_json::json!({"portMappings":[
                {"hostPort":8080,"containerPort":80,"protocol":"tcp","hostIP":""},
                {"hostPort":5353,"containerPort":53,"protocol":"udp","hostIP":"127.0.0.1"}
            ]})
        );
    }

    /// #354: portmap (capabilities.portMappings) gets runtimeConfig injected;
    /// flannel (no matching capability) does not.
    #[test]
    fn test_capability_runtime_config_injection() {
        let cap_args = serde_json::json!({"portMappings":[{"hostPort":8080,"containerPort":80,"protocol":"tcp"}]});

        let portmap = serde_json::json!({"type":"portmap","capabilities":{"portMappings":true}});
        let rc =
            capability_runtime_config(&portmap, &cap_args).expect("portmap gets runtimeConfig");
        assert_eq!(rc["portMappings"][0]["hostPort"], 8080);

        let flannel = serde_json::json!({"type":"flannel"});
        assert!(capability_runtime_config(&flannel, &cap_args).is_none());

        // Capability declared but no matching arg → nothing injected.
        let bw = serde_json::json!({"type":"bandwidth","capabilities":{"bandwidth":true}});
        assert!(capability_runtime_config(&bw, &cap_args).is_none());
    }
}
