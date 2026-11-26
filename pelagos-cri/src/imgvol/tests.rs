use super::*;
use std::os::unix::fs::symlink;

fn tree() -> tempfile::TempDir {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    std::fs::create_dir_all(r.join("etc/sub")).unwrap();
    std::fs::write(r.join("etc/passwd"), "root:x:0:0\n").unwrap();
    std::fs::create_dir_all(r.join("usr/lib")).unwrap();
    // Absolute symlink: must resolve against the image root, not the host.
    symlink("/usr/lib", r.join("lib")).unwrap();
    // Relative symlink.
    symlink("etc", r.join("conf")).unwrap();
    // Symlinks that try to climb out of the image.
    symlink("../../../../../../etc", r.join("escape")).unwrap();
    symlink("/etc/passwd", r.join("abs-host-passwd")).unwrap();
    symlink("loop-b", r.join("loop-a")).unwrap();
    symlink("loop-a", r.join("loop-b")).unwrap();
    t
}

#[test]
fn secure_join_resolves_plain_and_empty_paths() {
    let t = tree();
    let r = t.path();
    assert_eq!(secure_join(r, "").unwrap(), r);
    assert_eq!(secure_join(r, "etc").unwrap(), r.join("etc"));
    assert_eq!(secure_join(r, "etc/sub").unwrap(), r.join("etc/sub"));
    assert_eq!(secure_join(r, "/etc/passwd").unwrap(), r.join("etc/passwd"));
    assert_eq!(secure_join(r, "./etc/./sub/").unwrap(), r.join("etc/sub"));
}

#[test]
fn secure_join_follows_symlinks_relative_to_the_image_root() {
    let t = tree();
    let r = t.path();
    assert_eq!(secure_join(r, "conf/passwd").unwrap(), r.join("etc/passwd"));
    // /usr/lib inside the image, not the host's /usr/lib.
    assert_eq!(secure_join(r, "lib").unwrap(), r.join("usr/lib"));
}

#[test]
fn secure_join_cannot_escape_the_root() {
    let t = tree();
    let r = t.path();
    // `..` is clamped at the root.
    assert_eq!(secure_join(r, "etc/../../../..").unwrap(), r);
    assert_eq!(secure_join(r, "../../etc").unwrap(), r.join("etc"));
    // A symlink pointing far above the root lands on the image's own /etc.
    assert_eq!(secure_join(r, "escape").unwrap(), r.join("etc"));
    // An absolute link to a host file resolves inside the image: /etc/passwd there.
    assert_eq!(
        secure_join(r, "abs-host-passwd").unwrap(),
        r.join("etc/passwd")
    );
    // Whatever it returns is always under the root.
    for p in [
        "escape/passwd",
        "escape/../..",
        "lib/../../..",
        "conf/../..",
    ] {
        let got = secure_join(r, p).unwrap();
        assert!(got.starts_with(r), "{p} -> {}", got.display());
    }
}

#[test]
fn secure_join_reports_missing_paths_and_loops() {
    let t = tree();
    let r = t.path();
    assert_eq!(
        secure_join(r, "not-existing").unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    assert_eq!(
        secure_join(r, "etc/missing/deeper").unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    let e = secure_join(r, "loop-a").unwrap_err();
    assert!(e.to_string().contains("symbolic links"), "{e}");
    assert!(secure_join(r, "a\0b").is_err());
}

#[test]
fn mounts_under_matches_the_dir_and_its_children_only() {
    let mi = "1 0 0:1 / / rw - ext4 /dev/x rw\n\
              2 1 0:2 / /run/x/1/root rw - overlay overlay rw\n\
              3 1 0:3 / /run/x/1/root/deep rw - tmpfs tmpfs rw\n\
              4 1 0:4 / /run/x/10/root rw - tmpfs tmpfs rw\n";
    let got = mounts_under(mi, Path::new("/run/x/1"));
    assert_eq!(
        got,
        [
            PathBuf::from("/run/x/1/root"),
            PathBuf::from("/run/x/1/root/deep")
        ]
    );
    assert!(mounts_under(mi, Path::new("/run/x/2")).is_empty());
}

#[test]
fn stage_rejects_bad_ids_and_unknown_images_without_leaving_anything() {
    let t = tempfile::tempdir().unwrap();
    let store = StoreLayout::under(&t.path().join("store"));
    let root = t.path().join("imgvol");
    for bad in ["", "..", "a/b", "a:b"] {
        assert!(matches!(
            stage(&store, &root, bad, 0, "nope:1", ""),
            Err(ImgVolError::BadId(_))
        ));
    }
    assert!(matches!(
        stage(&store, &root, "c1", 0, "docker.io/library/nope:1", ""),
        Err(ImgVolError::Image(RootfsError::ImageNotFound(_)))
    ));
    assert!(
        !root.join("c1").exists(),
        "a failed stage leaves no directory"
    );
}

#[test]
fn cleanup_stale_keeps_only_listed_containers() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path();
    for c in ["keep1", "stale1", "stale2"] {
        std::fs::create_dir_all(root.join(c).join("0/root")).unwrap();
    }
    let keep: HashSet<String> = ["keep1".to_string()].into();
    cleanup_stale(root, &keep);
    assert!(root.join("keep1").exists());
    assert!(!root.join("stale1").exists() && !root.join("stale2").exists());
}

/// A real mount round trip (needs root): two layers, the upper one shadowing a file
/// and adding a directory; sub path selection; read-only; missing sub path fails and
/// cleans up; unstage detaches and removes everything.
#[test]
fn stage_mounts_layers_read_only_and_unstage_detaches() {
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("Skipping stage_mounts_layers_read_only_and_unstage_detaches: needs root");
        return;
    }
    let t = tempfile::tempdir().unwrap();
    let store = StoreLayout::under(&t.path().join("store"));
    let hex = |n: u8| format!("{:064x}", n);
    let mk_layer = |n: u8| {
        let d = store.layers.join(hex(n));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(".pelagos_complete"), b"").unwrap();
        d
    };
    let (l1, l2) = (mk_layer(1), mk_layer(2));
    std::fs::create_dir_all(l1.join("etc")).unwrap();
    std::fs::write(l1.join("etc/passwd"), "bottom").unwrap();
    std::fs::write(l1.join("only-bottom"), "b").unwrap();
    std::fs::create_dir_all(l2.join("etc")).unwrap();
    std::fs::write(l2.join("etc/passwd"), "top").unwrap();
    let manifest = serde_json::json!({
        "reference": "docker.io/library/imgvol:1", "digest": "sha256:feed",
        "layers": [format!("sha256:{}", hex(1)), format!("sha256:{}", hex(2))],
        "layer_types": ["", ""],
        "config": {"cmd": [], "env": [], "working_dir": "/"},
    });
    let dir = store.images.join("docker.io_library_imgvol_1");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("manifest.json"), manifest.to_string()).unwrap();

    let root = t.path().join("imgvol");
    let cid = "ctr1";
    // Whole image.
    let whole = stage(&store, &root, cid, 0, "docker.io/library/imgvol:1", "").unwrap();
    assert_eq!(
        std::fs::read_to_string(whole.join("etc/passwd")).unwrap(),
        "top"
    );
    assert!(whole.join("only-bottom").exists());
    assert!(
        std::fs::write(whole.join("new-file"), b"x").is_err(),
        "the staged image must be read-only"
    );
    // Sub path.
    let sub = stage(&store, &root, cid, 1, "docker.io/library/imgvol:1", "etc").unwrap();
    assert_eq!(std::fs::read_to_string(sub.join("passwd")).unwrap(), "top");
    assert!(!sub.join("only-bottom").exists());
    // Missing sub path: clear error, and its staging dir is gone.
    let err = stage(
        &store,
        &root,
        cid,
        2,
        "docker.io/library/imgvol:1",
        "no/such",
    )
    .unwrap_err();
    assert!(matches!(err, ImgVolError::SubPathMissing { .. }), "{err}");
    assert!(!root.join(cid).join("2").exists());

    unstage(&root, cid);
    let mi = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
    assert!(
        mounts_under(&mi, &root).is_empty(),
        "unstage must detach every mount"
    );
    assert!(!root.join(cid).exists());
    // The layers themselves are untouched.
    assert_eq!(
        std::fs::read_to_string(l2.join("etc/passwd")).unwrap(),
        "top"
    );
}
