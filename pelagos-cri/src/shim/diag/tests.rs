use super::*;

/// The error ipc9's CRI logged 2,242 times (#659), shortened in the backtraces only.
const IPC9_ERR: &str = r#"rpc status: Status { code: UNKNOWN, message: "Others(\"failed to handle message create container\\n\\nCaused by:\\n    0: agent create container\\n    1: rpc status: Status { code: INTERNAL, message: \\\"No such file or directory (os error 2)\\\\n\\\\nStack backtrace:\\\\n   0: <unknown>\\\\n   1: <unknown>\\\\n  11: __libc_start_main\\\\n  12: <unknown>\\\", details: [], special_fields: SpecialFields { unknown_fields: UnknownFields { fields: None }, cached_size: CachedSize { size: 0 } } }\\n\\nStack backtrace:\\n   0: <anyhow::Error as core::convert::From<ttrpc::error::Error>>::from\\n   1: <agent::kata::KataAgent as agent::Agent>::create_container::{closure#0}::{closure#0}\\n  17: <std::sys::thread::unix::Thread>::new::thread_start\")", details: [], special_fields: SpecialFields { unknown_fields: UnknownFields { fields: None }, cached_size: CachedSize { size: 0 } } }"#;

#[test]
fn the_real_ipc9_error_is_boiled_down_to_its_cause() {
    assert_eq!(
        root_cause(IPC9_ERR),
        "agent create container: No such file or directory (os error 2)"
    );
}

/// A host-side failure from the forced repro on ipc7: Kata names the source itself, inside a
/// long debug dump of the mount.
const HOST_SIDE_ERR: &str = r#"rpc status: Status { code: UNKNOWN, message: "Others(\"failed to handle message create container\\n\\nCaused by:\\n    0: handler volumes\\n    1: new share fs volume Mount { destination: \\\"/gone\\\", typ: Some(\\\"bind\\\"), source: Some(\\\"/nonexistent-659-source\\\"), uid_mappings: None }\\n    2: mount shared volume\\n    3: share to guest\\n    4: failed to bind mount /nonexistent-659-source to /run/kata-containers/shared/x\\n    5: Invalid path: /nonexistent-659-source: Others(\\n\\nStack backtrace:\\n   0: <unknown>\")" }"#;

#[test]
fn a_host_side_error_stays_readable() {
    let c = root_cause(HOST_SIDE_ERR);
    assert!(
        c.starts_with("handler volumes > new share fs volume Mount"),
        "{c}"
    );
    assert!(c.contains("Invalid path: /nonexistent-659-source"), "{c}");
    assert!(!c.contains("Others("), "{c}");
    assert!(c.len() < 700, "{} chars: {c}", c.len());
}

#[test]
fn an_unstructured_error_is_kept_as_is_and_bounded() {
    assert_eq!(root_cause("boom"), "boom");
    let long = "x".repeat(1000);
    assert_eq!(root_cause(&long).len(), 300);
}

#[test]
fn escalation_points_are_sparse() {
    let hits: Vec<u32> = (1..=450).filter(|n| is_escalation_point(*n)).collect();
    assert_eq!(hits, vec![3, 10, 30, 100, 200, 300, 400]);
    assert!(!is_escalation_point(1) && !is_escalation_point(2));
}

#[test]
fn missing_bind_sources_are_named_and_other_mounts_ignored() {
    let cfg = br#"{"mounts":[
        {"destination":"/proc","type":"proc","source":"proc"},
        {"destination":"/etc/pki/ca","type":"bind","source":"/var/lib/kubelet/pods/u/ca","options":["rbind","ro"]},
        {"destination":"/etc/pki/sa","source":"/var/lib/kubelet/pods/u/sa","options":["rbind"]},
        {"destination":"/data","type":"bind","source":"/exists","options":["rbind"]},
        {"destination":"/tmp","type":"tmpfs","source":"tmpfs"}]}"#;
    let missing = missing_mount_sources(cfg, |p| p == Path::new("/exists"));
    assert_eq!(
        missing,
        vec![
            "/etc/pki/ca <- /var/lib/kubelet/pods/u/ca",
            "/etc/pki/sa <- /var/lib/kubelet/pods/u/sa"
        ]
    );
    assert!(missing_mount_sources(b"not json", |_| false).is_empty());
}

#[tokio::test]
async fn a_bundle_names_the_cause_and_the_missing_mount_and_old_ones_are_pruned() {
    let t = tempfile::tempdir().unwrap();
    let bundle = t.path().join("bundle");
    std::fs::create_dir_all(&bundle).unwrap();
    std::fs::write(
        bundle.join("config.json"),
        r#"{"mounts":[{"destination":"/etc/x","type":"bind","source":"/nonexistent/659","options":["rbind"]}]}"#,
    )
    .unwrap();
    let diag = t.path().join("diag");
    let mut last = None;
    for n in [3u32, 10, 30, 100, 200, 300, 400] {
        last = capture(&diag, "sbx659", "ctr659abcdef0123", &bundle, IPC9_ERR, n).await;
    }
    let dir = last.expect("bundle written");
    let summary = std::fs::read_to_string(dir.join("summary.txt")).unwrap();
    assert!(
        summary.contains("consecutive Create failures: 400"),
        "{summary}"
    );
    assert!(
        summary.contains("agent create container: No such file or directory"),
        "{summary}"
    );
    assert!(summary.contains("/etc/x <- /nonexistent/659"), "{summary}");
    for f in [
        "error.txt",
        "config.json",
        "shared-sandbox.txt",
        "shim-journal.txt",
        "mounts.txt",
    ] {
        assert!(dir.join(f).exists(), "missing {f}");
    }
    assert_eq!(
        std::fs::read_dir(diag.join("sbx659")).unwrap().count(),
        5,
        "only the newest 5 kept"
    );
    assert!(
        !diag.join("sbx659").join("00003-ctr659abcdef").exists(),
        "oldest pruned"
    );
}

#[test]
fn recycling_is_capped_per_pod_and_per_window() {
    use std::time::{Duration, Instant};
    let t0 = Instant::now();
    let at = |secs| t0 + Duration::from_secs(secs);
    let mut l = RecycleLimiter::default();
    assert_eq!(l.try_acquire("ns/p/u1", at(0)), Ok(1));
    assert_eq!(l.try_acquire("ns/p/u1", at(60)), Ok(2));
    // Third within the hour: refused, and the refusal is not recorded.
    assert_eq!(l.try_acquire("ns/p/u1", at(120)), Err(2));
    assert_eq!(l.try_acquire("ns/p/u1", at(3599)), Err(2));
    // Another pod is independent.
    assert_eq!(l.try_acquire("ns/q/u2", at(120)), Ok(1));
    // After the first recycle ages out of the window one more is allowed.
    assert_eq!(l.try_acquire("ns/p/u1", at(3601)), Ok(2));
    assert_eq!(l.try_acquire("ns/p/u1", at(3602)), Err(2));
}
