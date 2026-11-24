use std::os::unix::fs::PermissionsExt;

use serde_json::json;
use serial_test::serial;

use super::*;
use crate::state::NsMode;

fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

// ── policy (no root needed) ────────────────────────────────────────────────

#[test]
fn shim_policy_rejects_host_network_and_missing_cni() {
    let pod = NamespaceModes::default();
    let conf = Some(PathBuf::from("/etc/cni/net.d/10-x.conflist"));

    let plan = shim_network_plan(&pod, conf.clone()).unwrap();
    assert_eq!(plan.conf_path, conf.clone().unwrap());

    let host = NamespaceModes {
        network: NsMode::Node,
        ..NamespaceModes::default()
    };
    let e = shim_network_plan(&host, conf).unwrap_err();
    assert_eq!(e.code(), tonic::Code::InvalidArgument);
    assert!(e.message().contains("hostNetwork"), "{}", e.message());

    let e = shim_network_plan(&pod, None).unwrap_err();
    assert_eq!(e.code(), tonic::Code::FailedPrecondition);
    assert!(e.message().contains("CNI"), "{}", e.message());
}

#[test]
#[serial(cni_env)]
fn cni_bin_dirs_env_override_is_searched_first() {
    std::env::set_var("PELAGOS_CNI_BIN_DIRS", "/tmp/a::/tmp/b");
    let d = cni::bin_dirs();
    std::env::remove_var("PELAGOS_CNI_BIN_DIRS");
    assert_eq!(&d[..2], ["/tmp/a", "/tmp/b"]);
    assert!(d.contains(&"/opt/cni/bin".to_string()), "defaults retained");
    assert!(!cni::bin_dirs().contains(&"/tmp/a".to_string()));
}

// ── fake CNI plugin + real netns (root) ────────────────────────────────────

struct Fake {
    _dir: tempfile::TempDir,
    conf: PathBuf,
    log: PathBuf,
    marker: PathBuf,
}

/// A CNI plugin that logs each call, records whether the netns existed, and
/// whether a "shim stopped" marker was present (for the DEL ordering check).
/// `FAKECNI_FAIL=1` makes ADD fail.
fn fake_cni() -> Fake {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("fakecni");
    let log = dir.path().join("calls.log");
    let marker = dir.path().join("shim-stopped");
    std::fs::write(
        &bin,
        format!(
            r#"#!/bin/sh
cat > /dev/null
exists=no; [ -e "$CNI_NETNS" ] && exists=yes
stopped=no; [ -e "{marker}" ] && stopped=yes
echo "$CNI_COMMAND netns_exists=$exists stopped=$stopped path=$CNI_NETNS" >> "{log}"
if [ "$CNI_COMMAND" = ADD ]; then
  [ "$FAKECNI_FAIL" = 1 ] && {{ echo boom >&2; exit 1; }}
  echo '{{"cniVersion":"0.4.0","ips":[{{"address":"10.1.2.3/24"}}]}}'
fi
"#,
            marker = marker.display(),
            log = log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let conf = dir.path().join("10-fake.conf");
    std::fs::write(
        &conf,
        json!({"cniVersion": "0.4.0", "name": "fake", "type": "fakecni"}).to_string(),
    )
    .unwrap();
    std::env::set_var("PELAGOS_CNI_BIN_DIRS", dir.path());
    Fake {
        _dir: dir,
        conf,
        log,
        marker,
    }
}

fn calls(f: &Fake) -> Vec<String> {
    std::fs::read_to_string(&f.log)
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect()
}

fn pod() -> PodRef<'static> {
    PodRef {
        name: "p",
        namespace: "default",
        uid: "uid-1",
    }
}

fn unique_id() -> String {
    format!("t595{:08x}{:04x}", std::process::id(), rand_suffix())
}

fn rand_suffix() -> u16 {
    use std::sync::atomic::{AtomicU16, Ordering};
    static N: AtomicU16 = AtomicU16::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

fn lo_is_up(netns_path: &str) -> bool {
    netns_native::in_net_ns(netns_path, || {
        #[repr(C)]
        struct Ifreq {
            name: [u8; 16],
            flags: libc::c_short,
            _pad: [u8; 22],
        }
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
        let mut req = Ifreq {
            name: [0; 16],
            flags: 0,
            _pad: [0; 22],
        };
        req.name[..2].copy_from_slice(b"lo");
        let rc = unsafe { libc::ioctl(fd, libc::SIOCGIFFLAGS as _, &mut req) };
        unsafe { libc::close(fd) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(req.flags & libc::IFF_UP as libc::c_short != 0)
    })
    .unwrap()
}

fn sandbox_for(net: &PodNetwork, id: &str) -> CriSandbox {
    serde_json::from_value(json!({
        "id": id, "name": "p", "namespace": "default", "uid": "uid-1",
        "attempt": 0, "labels": {}, "annotations": {}, "created_at_ns": 0,
        "state": "Running",
        "netns": net.ns_name,
        "cni_conf": net.conf_path.to_string_lossy(),
    }))
    .unwrap()
}

/// ADD runs against an existing netns and has finished, with `lo` up, by the
/// time setup returns: the caller can start a VM immediately afterwards.
#[tokio::test]
#[serial(cni_env)]
async fn setup_runs_add_into_an_existing_netns_and_brings_lo_up() {
    if !is_root() {
        eprintln!("skipping podnet setup test: needs root (netns)");
        return;
    }
    let f = fake_cni();
    let id = unique_id();
    let net = prepare_shim_sandbox_network(
        &id,
        pod(),
        &NamespaceModes::default(),
        Some(f.conf.clone()),
        json!({}),
    )
    .await
    .unwrap();

    assert_eq!(net.ip, "10.1.2.3");
    assert_eq!(net.netns_path, format!("/run/netns/{}", net.ns_name));
    assert!(Path::new(&net.netns_path).exists());
    let c = calls(&f);
    assert_eq!(c.len(), 1, "{c:?}");
    assert!(
        c[0].starts_with("ADD netns_exists=yes"),
        "CNI ADD must run into an already-created netns: {c:?}"
    );
    assert!(
        lo_is_up(&net.netns_path),
        "lo must be up before the pod starts"
    );

    teardown_pod_network(&id, &sandbox_for(&net, &id)).await;
    assert!(!Path::new(&net.netns_path).exists());
    std::env::remove_var("PELAGOS_CNI_BIN_DIRS");
}

/// A failed ADD must not leave the netns behind for the kubelet's retry, and
/// is reported as INTERNAL.
#[tokio::test]
#[serial(cni_env)]
async fn add_failure_cleans_up_the_netns() {
    if !is_root() {
        eprintln!("skipping podnet failure test: needs root (netns)");
        return;
    }
    let f = fake_cni();
    std::env::set_var("FAKECNI_FAIL", "1");
    let id = unique_id();
    let r = setup_pod_network(&id, pod(), &f.conf, json!({})).await;
    std::env::remove_var("FAKECNI_FAIL");
    std::env::remove_var("PELAGOS_CNI_BIN_DIRS");
    let e = r.unwrap_err();
    assert_eq!(e.code(), tonic::Code::Internal);
    assert!(e.message().contains("CNI ADD"), "{}", e.message());
    let ns = format!("/run/netns/pcri-{}", &id[..12]);
    assert!(
        !Path::new(&ns).exists(),
        "netns {ns} leaked after failed ADD"
    );
}

/// Shim teardown order: the shim sandbox is stopped (marker written) before
/// CNI DEL runs, and the netns is gone only afterwards.
#[tokio::test]
#[serial(cni_env)]
async fn shim_teardown_stops_the_shim_before_cni_del() {
    if !is_root() {
        eprintln!("skipping podnet teardown-order test: needs root (netns)");
        return;
    }
    let f = fake_cni();
    let id = unique_id();
    let net = setup_pod_network(&id, pod(), &f.conf, json!({}))
        .await
        .unwrap();
    let sb = sandbox_for(&net, &id);

    let marker = f.marker.clone();
    let netns_path = net.netns_path.clone();
    teardown_after_shim_stop(
        async move {
            // While the shim is "stopping" the netns must still exist.
            assert!(Path::new(&netns_path).exists());
            std::fs::write(marker, b"").unwrap();
        },
        &id,
        &sb,
    )
    .await;
    std::env::remove_var("PELAGOS_CNI_BIN_DIRS");

    let c = calls(&f);
    assert_eq!(c.len(), 2, "{c:?}");
    assert!(
        c[1].starts_with("DEL netns_exists=yes stopped=yes"),
        "CNI DEL must run after the shim stopped and before netns removal: {c:?}"
    );
    assert!(!Path::new(&net.netns_path).exists());
}

/// Policy rejections happen before any netns or CNI side effect.
#[tokio::test]
#[serial(cni_env)]
async fn rejected_shim_sandbox_has_no_side_effects() {
    let f = fake_cni();
    let host = NamespaceModes {
        network: NsMode::Node,
        ..NamespaceModes::default()
    };
    let id = unique_id();
    let r = prepare_shim_sandbox_network(&id, pod(), &host, Some(f.conf.clone()), json!({})).await;
    assert_eq!(r.unwrap_err().code(), tonic::Code::InvalidArgument);
    let r =
        prepare_shim_sandbox_network(&id, pod(), &NamespaceModes::default(), None, json!({})).await;
    assert_eq!(r.unwrap_err().code(), tonic::Code::FailedPrecondition);
    std::env::remove_var("PELAGOS_CNI_BIN_DIRS");
    assert!(calls(&f).is_empty(), "no CNI call on a rejected sandbox");
    assert!(!Path::new(&format!("/run/netns/pcri-{}", &id[..12])).exists());
}
