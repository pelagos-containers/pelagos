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
