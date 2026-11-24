//! Pod network setup and teardown shared by native and shim-backed sandboxes
//! (#595, epic #574).
//!
//! A shim that boots a micro-VM (Kata) needs the pod network namespace to exist
//! **with CNI already applied** before the VM starts: the shim wires the VM's
//! NIC to the interfaces CNI created in that netns. So the sequence is fixed:
//!
//! 1. create the named netns;
//! 2. run CNI ADD into it (concurrency-bounded, fail-fast `RESOURCE_EXHAUSTED`);
//! 3. bring `lo` up and allow unprivileged low ports inside it;
//! 4. only then start whatever holds the pod (the native *pause* process, or a
//!    shim sandbox).
//!
//! Teardown is the mirror image: stop the holder first, then CNI DEL, then
//! delete the netns. This module owns steps 1-3 and the CNI DEL + netns delete,
//! so native and shim pods cannot drift apart. The extraction from
//! `run_pod_sandbox` / `stop_pod_sandbox` is behaviour-preserving.

use std::future::Future;
use std::path::{Path, PathBuf};

use tonic::Status;

use crate::cni;
use crate::netns_native;
use crate::state::{CriSandbox, NamespaceModes};

/// Pod identity passed to CNI as `CNI_ARGS`.
#[derive(Clone, Copy)]
pub struct PodRef<'a> {
    pub name: &'a str,
    pub namespace: &'a str,
    pub uid: &'a str,
}

/// A configured pod network namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodNetwork {
    /// Named netns, e.g. `pcri-a1b2c3d4e5f6` (what `CriSandbox.netns` stores).
    pub ns_name: String,
    /// `/run/netns/<ns_name>`; goes in the spec's network namespace entry.
    pub netns_path: String,
    /// IPv4 address CNI assigned (empty if the plugin reported none).
    pub ip: String,
    pub conf_path: PathBuf,
}

/// Steps 1-3 above. On any failure the netns is removed again, so nothing is
/// left behind for the kubelet's retry.
///
/// `ip netns add` and CNI ADD both shell out, and ADD can block for the
/// plugin's full retry window (e.g. while Cilium's agent socket is missing), so
/// both run on the blocking pool; ADD is additionally concurrency-bounded so a
/// post-reboot burst cannot pin async workers or thundering-herd the CNI
/// provider (#494). A saturated semaphore maps to `RESOURCE_EXHAUSTED`, which
/// the kubelet retries with its normal backoff (#500).
pub async fn setup_pod_network(
    sandbox_id: &str,
    pod: PodRef<'_>,
    conf_path: &Path,
    cap_args: serde_json::Value,
) -> Result<PodNetwork, Status> {
    let ns_name = format!("pcri-{}", &sandbox_id[..sandbox_id.len().min(12)]);

    let ns_name_bg = ns_name.clone();
    let netns_path = tokio::task::spawn_blocking(move || cni::create_netns(&ns_name_bg))
        .await
        .map_err(|e| Status::internal(format!("create netns task join: {}", e)))?
        .map_err(|e| Status::internal(format!("create netns for CNI sandbox: {}", e)))?;

    let ip = match cni::cni_add_bounded(
        sandbox_id.to_string(),
        netns_path.clone(),
        conf_path.to_path_buf(),
        cap_args,
        pod.name.to_string(),
        pod.namespace.to_string(),
        pod.uid.to_string(),
    )
    .await
    {
        Ok(ip) => ip,
        Err(cni::CniAddError::ResourceExhausted) => {
            cni::delete_netns(&ns_name);
            return Err(Status::resource_exhausted(
                "CNI call semaphore at capacity, retry later",
            ));
        }
        Err(e @ cni::CniAddError::Plugin(_)) => {
            cni::delete_netns(&ns_name);
            return Err(Status::internal(format!("CNI ADD: {}", e)));
        }
    };

    // The CNI spec requires a loopback plugin in the chain, but k3s/Flannel omit
    // it; without `lo` up all intra-pod 127.0.0.1 traffic fails, breaking the
    // sidecar pattern (#331). The unprivileged-port sysctl matches containerd's
    // default and lets non-root processes (e.g. coredns) bind low ports. Done
    // natively inside the netns (#564), best-effort.
    let netns_for_setup = netns_path.clone();
    let setup = tokio::task::spawn_blocking(move || {
        netns_native::in_net_ns(&netns_for_setup, || {
            netns_native::set_link_up("lo")?;
            netns_native::write_sysctl("net.ipv4.ip_unprivileged_port_start", "0")
        })
    })
    .await;
    match setup {
        Ok(Ok(())) => {}
        Ok(Err(e)) => log::warn!(
            "sandbox netns setup (lo up / unprivileged ports) in {}: {}",
            ns_name,
            e
        ),
        Err(e) => log::warn!("sandbox netns setup task join error in {}: {}", ns_name, e),
    }

    Ok(PodNetwork {
        ns_name,
        netns_path,
        ip,
        conf_path: conf_path.to_path_buf(),
    })
}

/// CNI DEL (if the sandbox was set up with CNI) then delete the netns. The
/// holder of the netns (pause or shim sandbox) must already be stopped.
pub async fn teardown_pod_network(sandbox_id: &str, sb: &CriSandbox) {
    let netns_path = format!("/run/netns/{}", sb.netns);
    if !sb.cni_conf.is_empty() {
        let cap_args = cni::port_mapping_cap_args(&sb.port_mappings);
        log::debug!(
            "StopPodSandbox {} step=cni_del netns={}",
            sandbox_id,
            sb.netns
        );
        // CNI DEL shells out to the plugin binary and can block for its full
        // retry window; run on the (concurrency-bounded) blocking pool (#494).
        cni::cni_del_bounded(
            sandbox_id.to_string(),
            netns_path,
            PathBuf::from(&sb.cni_conf),
            cap_args,
            sb.name.clone(),
            sb.namespace.clone(),
            sb.uid.clone(),
        )
        .await;
        log::debug!("StopPodSandbox {} step=cni_del DONE", sandbox_id);
    }
    log::debug!("StopPodSandbox {} step=delete_netns", sandbox_id);
    let ns_bg = sb.netns.clone();
    let _ = tokio::task::spawn_blocking(move || cni::delete_netns(&ns_bg)).await;
    log::debug!("StopPodSandbox {} step=delete_netns DONE", sandbox_id);
}

// ── shim-backed sandboxes ────────────────────────────────────────────────────
//
// Not called from the CRI request path yet: the backend wiring item of epic
// #574 consumes these, hence the `dead_code` allowances.

/// What a shim-backed sandbox needs from the network, after policy checks.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShimNetPlan {
    pub conf_path: PathBuf,
}

/// Policy for shim (micro-VM) sandboxes:
///
/// - `hostNetwork` pods are rejected: a VM cannot join the host network
///   namespace, and silently giving it a private one would break the pod's
///   contract.
/// - A real CNI config is required: the native pelagos bridge sandbox is
///   meaningless to a VM, so falling back to it would hand the shim no usable
///   netns.
// `Status` is tonic's standard (large) error type returned by every CRI call.
#[allow(clippy::result_large_err, dead_code)]
pub fn shim_network_plan(
    namespaces: &NamespaceModes,
    cni_conf: Option<PathBuf>,
) -> Result<ShimNetPlan, Status> {
    if namespaces.host_network() {
        return Err(Status::invalid_argument(
            "hostNetwork pods are not supported by shim-backed (VM) runtime handlers",
        ));
    }
    match cni_conf {
        Some(conf_path) => Ok(ShimNetPlan { conf_path }),
        None => Err(Status::failed_precondition(
            "shim-backed runtime handlers need a CNI config; none was found",
        )),
    }
}

/// Apply the shim policy, then run [`setup_pod_network`]. **No pause process
/// is spawned.** Returns once the netns is fully configured, so the caller can
/// start the shim sandbox and pass `netns_path` to the spec generator.
#[allow(clippy::result_large_err, dead_code)]
pub async fn prepare_shim_sandbox_network(
    sandbox_id: &str,
    pod: PodRef<'_>,
    namespaces: &NamespaceModes,
    cni_conf: Option<PathBuf>,
    cap_args: serde_json::Value,
) -> Result<PodNetwork, Status> {
    let plan = shim_network_plan(namespaces, cni_conf)?;
    setup_pod_network(sandbox_id, pod, &plan.conf_path, cap_args).await
}

/// Shim teardown order, enforced by construction: `stop_shim` (which releases
/// the netns) completes **before** CNI DEL and netns deletion begin.
#[allow(dead_code)]
pub async fn teardown_after_shim_stop<F>(stop_shim: F, sandbox_id: &str, sb: &CriSandbox)
where
    F: Future<Output = ()>,
{
    stop_shim.await;
    teardown_pod_network(sandbox_id, sb).await;
}

#[cfg(test)]
mod tests;
