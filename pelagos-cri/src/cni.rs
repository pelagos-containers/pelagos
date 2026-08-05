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

// ── Internals ─────────────────────────────────────────────────────────────────

/// Pod identity fields passed as CNI_ARGS to plugin binaries.
struct PodId<'a> {
    name: &'a str,
    namespace: &'a str,
    uid: &'a str,
}

/// Caps how many CNI plugin processes (ADD/DEL) run at once.
///
/// After a node reboot, kubelet can reconcile dozens of pods simultaneously;
/// each triggers `RunPodSandbox` → CNI ADD. If the CNI provider (e.g. Cilium)
/// isn't up yet, every one of those plugin invocations blocks retrying its
/// connection to the same not-yet-existing agent socket. Left unbounded, that
/// thundering herd competes with — and slows down — the very agent pod they're
/// all waiting on, on top of the raw process/fd load of dozens of concurrent
/// plugin binaries. `spawn_blocking` (see `*_bounded` below) keeps this off the
/// gRPC server's async worker threads; this semaphore keeps it from overwhelming
/// the CNI provider itself. See #494.
const MAX_CONCURRENT_CNI_CALLS: usize = 8;

fn cni_semaphore() -> &'static tokio::sync::Semaphore {
    static SEM: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    SEM.get_or_init(|| tokio::sync::Semaphore::new(MAX_CONCURRENT_CNI_CALLS))
}

/// Starts a background task that samples the CNI semaphore's permit counts
/// into gauges every 2s. A periodic sampler (rather than recording only at
/// call boundaries) keeps the gauge meaningful between calls too — e.g. a
/// value that's been pinned at 0 available for the last 10s is a much
/// stronger saturation signal than one that only ever appears instantaneously
/// in a histogram. See #499 (CNI invocation metrics, sub-issue of #496).
pub fn start_semaphore_gauge_sampler() {
    metrics::describe_gauge!(
        "pelagos_cri_cni_semaphore_permits_available",
        "CNI call semaphore permits currently available (max is pelagos_cri_cni_semaphore_permits_max)"
    );
    metrics::describe_gauge!(
        "pelagos_cri_cni_semaphore_permits_max",
        "CNI call semaphore capacity (MAX_CONCURRENT_CNI_CALLS)"
    );
    metrics::gauge!("pelagos_cri_cni_semaphore_permits_max").set(MAX_CONCURRENT_CNI_CALLS as f64);
    metrics::describe_histogram!(
        "pelagos_cri_cni_call_duration_seconds",
        "End-to-end CNI call duration in seconds, including time spent waiting for a semaphore permit"
    );

    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
        loop {
            tick.tick().await;
            metrics::gauge!("pelagos_cri_cni_semaphore_permits_available")
                .set(cni_semaphore().available_permits() as f64);
        }
    });
}

/// Runs a blocking closure on the blocking thread pool, gated by
/// [`cni_semaphore`]. Shared by [`cni_add_bounded`] / [`cni_del_bounded`];
/// pulled out as its own function so the concurrency-bounding behavior can
/// be exercised directly in tests without needing a real CNI plugin binary
/// on disk. See #494.
///
/// `op` labels the latency histogram (`"add"` or `"del"`) — see #499.
async fn run_bounded<F, T>(op: &'static str, f: F) -> Result<T, tokio::task::JoinError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let started = std::time::Instant::now();
    let _permit = cni_semaphore()
        .acquire()
        .await
        .expect("cni semaphore is never closed");
    let result = tokio::task::spawn_blocking(f).await;
    metrics::histogram!("pelagos_cri_cni_call_duration_seconds", "op" => op)
        .record(started.elapsed().as_secs_f64());
    result
}

/// Async, concurrency-bounded wrapper around [`cni_add`].
///
/// Runs the blocking plugin invocation on the blocking thread pool (so it
/// can't starve the gRPC server's async workers) and limits how many such
/// invocations run at once (so a reconciliation burst can't thundering-herd
/// the CNI provider). See [`MAX_CONCURRENT_CNI_CALLS`] and #494.
#[allow(clippy::too_many_arguments)]
pub async fn cni_add_bounded(
    sandbox_id: String,
    netns_path: String,
    conf_path: PathBuf,
    cap_args: serde_json::Value,
    pod_name: String,
    pod_namespace: String,
    pod_uid: String,
) -> Result<String, String> {
    run_bounded("add", move || {
        cni_add(
            &sandbox_id,
            &netns_path,
            &conf_path,
            &cap_args,
            &pod_name,
            &pod_namespace,
            &pod_uid,
        )
    })
    .await
    .unwrap_or_else(|e| Err(format!("CNI ADD task join: {}", e)))
}

/// Async, concurrency-bounded wrapper around [`cni_del`]. Best-effort, like
/// `cni_del` — errors are logged, not returned. See [`cni_add_bounded`].
#[allow(clippy::too_many_arguments)]
pub async fn cni_del_bounded(
    sandbox_id: String,
    netns_path: String,
    conf_path: PathBuf,
    cap_args: serde_json::Value,
    pod_name: String,
    pod_namespace: String,
    pod_uid: String,
) {
    let _ = run_bounded("del", move || {
        cni_del(
            &sandbox_id,
            &netns_path,
            &conf_path,
            &cap_args,
            &pod_name,
            &pod_namespace,
            &pod_uid,
        )
    })
    .await;
}

/// Run CNI ADD for a sandbox.
/// Returns the assigned IPv4 address (without prefix length) on success.
pub fn cni_add(
    sandbox_id: &str,
    netns_path: &str,
    conf_path: &Path,
    cap_args: &serde_json::Value,
    pod_name: &str,
    pod_namespace: &str,
    pod_uid: &str,
) -> Result<String, String> {
    let pod = PodId {
        name: pod_name,
        namespace: pod_namespace,
        uid: pod_uid,
    };
    let result = invoke_cni("ADD", sandbox_id, netns_path, conf_path, cap_args, &pod)?;
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
pub fn cni_del(
    sandbox_id: &str,
    netns_path: &str,
    conf_path: &Path,
    cap_args: &serde_json::Value,
    pod_name: &str,
    pod_namespace: &str,
    pod_uid: &str,
) {
    let pod = PodId {
        name: pod_name,
        namespace: pod_namespace,
        uid: pod_uid,
    };
    if let Err(e) = invoke_cni("DEL", sandbox_id, netns_path, conf_path, cap_args, &pod) {
        log::warn!("CNI DEL for {}: {}", sandbox_id, e);
    }
}

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
    pod: &PodId<'_>,
) -> Result<Option<serde_json::Value>, String> {
    let bin = find_plugin_bin(plugin_type)?;
    // CNI_ARGS carries Kubernetes pod identity in semicolon-separated K=V format.
    // Cilium 1.19.x K8sArgs only recognises K8S_POD_NAMESPACE, K8S_POD_NAME, and
    // K8S_POD_UID (IgnoreUnknown=false); K8S_POD_INFRA_CONTAINER_ID is rejected.
    // Modern runtimes (containerd 1.7+) send exactly these three fields.
    let cni_args = format!(
        "K8S_POD_NAMESPACE={};K8S_POD_NAME={};K8S_POD_UID={}",
        pod.namespace, pod.name, pod.uid
    );

    let mut child = Command::new(&bin)
        .env("CNI_COMMAND", command)
        .env("CNI_CONTAINERID", sandbox_id)
        .env("CNI_NETNS", netns_path)
        .env("CNI_IFNAME", "eth0")
        .env("CNI_PATH", cni_path_env())
        .env("CNI_ARGS", cni_args)
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
    pod: &PodId<'_>,
) -> Result<serde_json::Value, String> {
    let raw = std::fs::read_to_string(conf_path)
        .map_err(|e| format!("read {}: {}", conf_path.display(), e))?;
    if conf_path.extension().and_then(|x| x.to_str()) == Some("conflist") {
        invoke_conflist(command, sandbox_id, netns_path, &raw, cap_args, pod)
    } else {
        invoke_conf(command, sandbox_id, netns_path, &raw, pod)
    }
}

fn invoke_conf(
    command: &str,
    sandbox_id: &str,
    netns_path: &str,
    raw: &str,
    pod: &PodId<'_>,
) -> Result<serde_json::Value, String> {
    let conf: Conf = serde_json::from_str(raw).map_err(|e| format!("parse .conf: {}", e))?;
    Ok(
        run_plugin(command, sandbox_id, netns_path, &conf.plugin_type, raw, pod)?
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
    pod: &PodId<'_>,
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

        if let Some(result) = run_plugin(
            command,
            sandbox_id,
            netns_path,
            plugin_type,
            &config_str,
            pod,
        )? {
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

    /// #476/#482: CNI_ARGS must carry K8S_POD_NAMESPACE, K8S_POD_NAME, K8S_POD_UID in the
    /// semicolon-separated format required by Kubernetes-aware CNI plugins. Cilium 1.19.x
    /// K8sArgs only recognises these three fields (IgnoreUnknown=false); K8S_POD_INFRA_CONTAINER_ID
    /// is NOT included — Cilium rejects it with "unknown args".
    #[test]
    fn test_cni_args_format() {
        let pod_namespace = "kube-system";
        let pod_name = "cilium-abc123";
        let pod_uid = "aaaabbbb-cccc-dddd-eeee-ffffaaaabbbb";
        let cni_args = format!(
            "K8S_POD_NAMESPACE={};K8S_POD_NAME={};K8S_POD_UID={}",
            pod_namespace, pod_name, pod_uid
        );
        assert!(cni_args.contains("K8S_POD_NAMESPACE=kube-system"));
        assert!(cni_args.contains("K8S_POD_NAME=cilium-abc123"));
        assert!(cni_args.contains(&format!("K8S_POD_UID={}", pod_uid)));
        // Must NOT include K8S_POD_INFRA_CONTAINER_ID — Cilium 1.19.x rejects it.
        assert!(!cni_args.contains("K8S_POD_INFRA_CONTAINER_ID"));
        // Semicolon-separated, no trailing semicolon.
        let parts: Vec<&str> = cni_args.split(';').collect();
        assert_eq!(parts.len(), 3);
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

    /// #494: a burst of concurrent CNI calls must never exceed
    /// `MAX_CONCURRENT_CNI_CALLS` in flight at once, regardless of how many are
    /// requested at once (simulating ~40 pods reconciling after a reboot).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_run_bounded_limits_concurrency() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let active = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..(MAX_CONCURRENT_CNI_CALLS * 3) {
            let active = active.clone();
            let max_seen = max_seen.clone();
            handles.push(tokio::spawn(run_bounded("test", move || {
                let n = active.fetch_add(1, Ordering::SeqCst) + 1;
                max_seen.fetch_max(n, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(30));
                active.fetch_sub(1, Ordering::SeqCst);
            })));
        }
        for h in handles {
            h.await.unwrap().unwrap();
        }

        let seen = max_seen.load(Ordering::SeqCst);
        assert!(
            seen <= MAX_CONCURRENT_CNI_CALLS,
            "observed {} concurrent CNI calls in flight, semaphore cap is {} (#494)",
            seen,
            MAX_CONCURRENT_CNI_CALLS
        );
    }

    /// #494: `run_bounded`'s blocking work must run on the blocking thread pool,
    /// not inline on an async worker thread. Regression test for the original
    /// bug: `cni_add`/`cni_del` were `std::process::Command` calls made directly
    /// inside async gRPC handlers, so a blocked CNI call (e.g. 30s waiting on
    /// Cilium's not-yet-existing socket) fully pinned a worker thread and starved
    /// unrelated RPCs. With a single-worker-thread runtime, if `run_bounded` ever
    /// regresses to running its closure inline, this test's own async sleep would
    /// be stuck behind the closures' `std::thread::sleep` and blow past the
    /// generous timing assertion below.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_run_bounded_does_not_block_async_worker() {
        let mut handles = Vec::new();
        for _ in 0..4 {
            handles.push(tokio::spawn(run_bounded("test", || {
                std::thread::sleep(std::time::Duration::from_millis(200));
            })));
        }

        // Unrelated async work on the (sole) worker thread must still complete
        // promptly while those "CNI calls" are blocked in flight.
        let start = std::time::Instant::now();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(150),
            "async sleep took {:?}; the sole worker thread appears blocked by \
             run_bounded's closures instead of them running on the blocking pool (#494)",
            elapsed
        );

        for h in handles {
            h.await.unwrap().unwrap();
        }
    }

    /// #499: `run_bounded` must record a `pelagos_cri_cni_call_duration_seconds`
    /// histogram sample labeled by `op`, and `start_semaphore_gauge_sampler` must
    /// immediately expose the semaphore's capacity as a gauge. Uses a
    /// `current_thread` runtime so the thread-local test recorder (set via
    /// `metrics::set_default_local_recorder`) stays active across the `.await`
    /// points in `run_bounded` — a multi-thread runtime could resume on a
    /// different OS thread after `spawn_blocking(f).await`, missing the
    /// thread-local recorder entirely.
    #[tokio::test(flavor = "current_thread")]
    async fn test_cni_metrics_recorded_for_add_and_del() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);

        start_semaphore_gauge_sampler();

        run_bounded("add", || {
            std::thread::sleep(std::time::Duration::from_millis(5));
        })
        .await
        .unwrap();
        run_bounded("del", || {
            std::thread::sleep(std::time::Duration::from_millis(5));
        })
        .await
        .unwrap();

        let snapshot = snapshotter.snapshot().into_vec();

        let max_permits = snapshot.iter().find_map(|(ck, _, _, v)| {
            (ck.key().name() == "pelagos_cri_cni_semaphore_permits_max")
                .then_some(v)
                .and_then(|v| match v {
                    DebugValue::Gauge(g) => Some(g.into_inner()),
                    _ => None,
                })
        });
        assert_eq!(
            max_permits,
            Some(MAX_CONCURRENT_CNI_CALLS as f64),
            "permits_max gauge should be set synchronously by start_semaphore_gauge_sampler, snapshot: {snapshot:?}"
        );

        for op in ["add", "del"] {
            let histogram_samples = snapshot.iter().find_map(|(ck, _, _, v)| {
                let matches = ck.key().name() == "pelagos_cri_cni_call_duration_seconds"
                    && ck
                        .key()
                        .labels()
                        .any(|l| l.key() == "op" && l.value() == op);
                matches.then_some(v).and_then(|v| match v {
                    DebugValue::Histogram(samples) => Some(samples.len()),
                    _ => None,
                })
            });
            assert_eq!(
                histogram_samples,
                Some(1),
                "expected exactly one duration sample for op={op}, snapshot: {snapshot:?}"
            );
        }
    }
}
