use std::os::unix::fs::{MetadataExt, PermissionsExt};

use serde_json::json;

use super::*;
use crate::shim::spec::resolve_user;

fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

fn hex(n: usize) -> String {
    format!("{n:064x}")
}

/// A store with `n` complete layers (`l0` is the bottom) and one image whose
/// manifest lists them bottom-to-top.
fn fake_store(root: &Path, n: usize) -> (StoreLayout, serde_json::Value) {
    let store = StoreLayout::under(root);
    for i in 0..n {
        let d = store.layers.join(hex(i));
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join(LAYER_COMPLETE_MARKER), b"").unwrap();
    }
    let layers: Vec<String> = (0..n).map(|i| format!("sha256:{}", hex(i))).collect();
    let manifest = json!({
        "reference": "docker.io/library/app:1",
        "digest": "sha256:feed",
        "layers": layers,
        "layer_types": vec![""; n],
        "config": {},
    });
    let dir = store.images.join(dirname("docker.io/library/app:1"));
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("manifest.json"), manifest.to_string()).unwrap();
    (store, manifest)
}

#[test]
fn find_manifest_by_reference_digest_and_missing() {
    let t = tempfile::tempdir().unwrap();
    let (store, _) = fake_store(t.path(), 2);
    for key in ["docker.io/library/app:1", "sha256:feed"] {
        let m = find_manifest(&store, key).unwrap();
        assert_eq!(m["reference"], "docker.io/library/app:1", "{key}");
    }
    assert!(matches!(
        find_manifest(&store, "nope:1"),
        Err(RootfsError::ImageNotFound(_))
    ));
    // No images directory at all is "not found", not an I/O error.
    let empty = StoreLayout::under(&t.path().join("none"));
    assert!(matches!(
        find_manifest(&empty, "x"),
        Err(RootfsError::ImageNotFound(_))
    ));
}

#[test]
fn find_manifest_expands_short_references() {
    // #630: the kubelet passes the pod's image string, e.g. `app:1`.
    let t = tempfile::tempdir().unwrap();
    let (store, _) = fake_store(t.path(), 1);
    let m = find_manifest(&store, "app:1").unwrap();
    assert_eq!(m["reference"], "docker.io/library/app:1");
    assert_eq!(normalise("busybox"), "docker.io/library/busybox:latest");
    assert_eq!(normalise("reg.io/x/y:2"), "reg.io/x/y:2");
    assert!(matches!(
        find_manifest(&store, "app"), // -> app:latest, not stored
        Err(RootfsError::ImageNotFound(_))
    ));
}

#[test]
fn layers_are_top_first_deduplicated_and_verified() {
    let t = tempfile::tempdir().unwrap();
    let (store, m) = fake_store(t.path(), 3);
    let got = layers_top_first(&store, "app", &m).unwrap();
    assert_eq!(
        got,
        vec![
            store.layers.join(hex(2)),
            store.layers.join(hex(1)),
            store.layers.join(hex(0))
        ],
        "top layer first, as overlayfs lowerdir expects"
    );

    // The same layer listed twice (empty-layer digests repeat) appears once:
    // overlayfs rejects duplicates in lowerdir.
    let mut dup = m.clone();
    dup["layers"] = json!([
        format!("sha256:{}", hex(0)),
        format!("sha256:{}", hex(1)),
        format!("sha256:{}", hex(0))
    ]);
    assert_eq!(layers_top_first(&store, "app", &dup).unwrap().len(), 2);

    // An interrupted extraction (no completion marker) is an error naming the layer.
    fs::remove_file(store.layers.join(hex(1)).join(LAYER_COMPLETE_MARKER)).unwrap();
    match layers_top_first(&store, "app", &m) {
        Err(RootfsError::LayerMissing(d)) => assert!(d.contains(&hex(1))),
        other => panic!("{other:?}"),
    }
}

#[test]
fn wasm_and_layerless_images_are_rejected() {
    let t = tempfile::tempdir().unwrap();
    let (store, m) = fake_store(t.path(), 1);
    let mut w = m.clone();
    w["layer_types"] = json!(["application/vnd.wasm.content.layer.v1+wasm"]);
    assert!(matches!(
        layers_top_first(&store, "app", &w),
        Err(RootfsError::WasmImage(_))
    ));
    let mut e = m.clone();
    e["layers"] = json!([]);
    assert!(matches!(
        layers_top_first(&store, "app", &e),
        Err(RootfsError::NoLayers(_))
    ));
}

#[test]
fn overlay_options_string() {
    let o = overlay_options(
        &["/l/b".into(), "/l/a".into()],
        Path::new("/s/upper"),
        Path::new("/s/work"),
    );
    assert_eq!(
        o,
        "lowerdir=/l/b:/l/a,upperdir=/s/upper,workdir=/s/work,metacopy=off"
    );
}

#[test]
fn plan_describes_overlay_creates_scratch_and_cleans_up() {
    let t = tempfile::tempdir().unwrap();
    let (store, m) = fake_store(t.path(), 3);
    let layers = layers_top_first(&store, "app", &m).unwrap();
    let plan = plan_rootfs(&store, "ctr1", layers.clone()).unwrap();

    assert_eq!(
        (plan.typ.as_str(), plan.source.as_str()),
        ("overlay", "overlay")
    );
    assert!(!plan.uses_aliases(), "short lists use the real layer paths");
    let opts = plan.options.join(",");
    assert!(opts.starts_with(&format!(
        "lowerdir={}:{}:{}",
        layers[0].display(),
        layers[1].display(),
        layers[2].display()
    )));
    assert!(plan.options.contains(&"metacopy=off".to_string()));
    assert!(plan
        .options
        .contains(&format!("upperdir={}", plan.upper_dir().display())));

    // Scratch parent is private; upper/work are traversable like native overlay scratch.
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(plan.scratch_dir()), 0o700);
    assert_eq!(mode(plan.upper_dir()), 0o755);
    assert!(plan.scratch_dir().join("work").is_dir());
    assert!(plan.scratch_dir().ends_with("cri-shim-ctr1"));

    let am = plan.to_api_mount();
    assert_eq!(am.type_, "overlay");
    assert_eq!(am.source, "overlay");
    assert_eq!(
        am.target, "",
        "the shim mounts into its own <bundle>/rootfs"
    );
    assert_eq!(am.options, plan.options);

    plan.cleanup().unwrap();
    assert!(!plan.scratch_dir().exists());
    plan.cleanup().unwrap(); // idempotent
}

#[test]
fn invalid_ids_and_empty_layers_rejected() {
    let t = tempfile::tempdir().unwrap();
    let (store, m) = fake_store(t.path(), 1);
    let layers = layers_top_first(&store, "app", &m).unwrap();
    for bad in ["", "..", "a/b", "a:b", "a,b"] {
        assert!(
            matches!(
                plan_rootfs(&store, bad, layers.clone()),
                Err(RootfsError::InvalidId(_))
            ),
            "{bad:?}"
        );
    }
    assert!(matches!(
        plan_rootfs(&store, "ok", vec![]),
        Err(RootfsError::NoLayers(_))
    ));
    assert!(!store.scratch.exists(), "nothing created on rejection");
}

/// Long layer paths blow the one-page mount option limit; the plan must fall
/// back to short symlink aliases that preserve order and point at the layers.
#[test]
fn many_layers_fall_back_to_short_aliases() {
    let t = tempfile::tempdir().unwrap();
    let (store, m) = fake_store(t.path(), 60);
    let layers = layers_top_first(&store, "app", &m).unwrap();
    assert!(
        overlay_options(&layers, Path::new("/u"), Path::new("/w")).len() > MAX_OPTIONS_LEN,
        "test layers must exceed the limit with real paths"
    );
    let plan = plan_rootfs(&store, "abcdef0123456789", layers.clone()).unwrap();
    assert!(plan.uses_aliases());
    let opts = plan.options.join(",");
    assert!(opts.len() <= MAX_OPTIONS_LEN, "{} bytes", opts.len());
    let lower = plan.options[0].strip_prefix("lowerdir=").unwrap();
    let aliases: Vec<&str> = lower.split(':').collect();
    assert_eq!(aliases.len(), 60);
    for (i, a) in aliases.iter().enumerate() {
        assert!(a.ends_with(&format!("abcdef012345/{i}")), "{a}");
        assert_eq!(fs::read_link(a).unwrap(), layers[i], "alias {i} order");
    }
    plan.cleanup().unwrap();
    assert!(!store.alias_root.join("abcdef012345").exists());
    assert!(!plan.scratch_dir().exists());
}

#[test]
fn too_many_layers_and_unfittable_options_are_errors_that_leave_nothing_behind() {
    let t = tempfile::tempdir().unwrap();
    let store = StoreLayout::under(t.path());
    let many = |n: usize| -> Vec<PathBuf> { (0..n).map(|i| store.layers.join(hex(i))).collect() };
    assert!(matches!(
        plan_rootfs(&store, "c", many(MAX_LOWER_LAYERS + 1)),
        Err(RootfsError::TooManyLayers(501))
    ));
    // 400 layers fit overlayfs' stack but not one option page even aliased.
    assert!(matches!(
        plan_rootfs(&store, "c", many(400)),
        Err(RootfsError::OptionsTooLong { layers: 400, .. })
    ));
    assert!(!store.scratch.join("cri-shim-c").exists());
    assert!(!store.alias_root.join("c").exists());
}

#[test]
fn mountinfo_parsing_detects_a_live_upper() {
    let upper = Path::new("/var/lib/pelagos/scratch/cri-shim-x/upper");
    let live = "89 40 0:51 / /run/k/rootfs rw - overlay overlay rw,lowerdir=/a,upperdir=/var/lib/pelagos/scratch/cri-shim-x/upper,workdir=/w";
    let other = "89 40 0:51 / /run/k/rootfs rw - overlay overlay rw,lowerdir=/a,upperdir=/var/lib/pelagos/scratch/cri-shim-xy/upper,workdir=/w";
    let not_overlay =
        "30 1 8:1 / / rw - ext4 /dev/sda1 rw,upperdir=/var/lib/pelagos/scratch/cri-shim-x/upper";
    assert!(mountinfo_has_upper(live, upper));
    assert!(
        !mountinfo_has_upper(other, upper),
        "prefix of another id is not a match"
    );
    assert!(!mountinfo_has_upper(not_overlay, upper));
    assert!(!mountinfo_has_upper("", upper));
}

// ── layered passwd/group ───────────────────────────────────────────────────

fn write(layer: &Path, rel: &str, body: &str) {
    let p = layer.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

const PASSWD: &str = "root:x:0:0:root:/root:/bin/sh\napp:x:1000:2000::/home/app:/bin/sh\n";
const GROUP: &str = "root:x:0:\nstaff:x:50:\napp:x:2000:app\n";

#[test]
fn parse_passwd_and_group_skip_comments_and_junk() {
    let p = parse_passwd("# c\n\nroot:x:0:0:r:/root:/bin/sh\nbroken\nbad:x:nan:0\nn:x:5:6\n");
    assert_eq!(p, [("root".into(), 0, 0), ("n".into(), 5, 6)]);
    let g = parse_group("# c\nroot:x:0:\nshort:x\nstaff:x:50:a,b\n");
    assert_eq!(g, [("root".into(), 0), ("staff".into(), 50)]);
}

#[test]
fn top_layer_overrides_lower_layers() {
    let t = tempfile::tempdir().unwrap();
    let (bottom, top) = (t.path().join("bottom"), t.path().join("top"));
    write(&bottom, "etc/passwd", PASSWD);
    write(&bottom, "etc/group", GROUP);
    write(
        &top,
        "etc/passwd",
        "root:x:0:0::/root:/bin/sh\nnew:x:7:8::/:/bin/sh\n",
    );
    let layers = vec![top.clone(), bottom.clone()];

    let u = LayeredUsers::new(layers.clone());
    // passwd comes entirely from the top layer (it shadows the whole file) ...
    assert_eq!(resolve_user("new", Some(&u)).unwrap(), (7, 8));
    assert!(
        resolve_user("app", Some(&u)).is_err(),
        "bottom passwd is shadowed"
    );
    // ... but group, absent from the top, falls through to the bottom.
    assert_eq!(resolve_user("0:staff", Some(&u)).unwrap(), (0, 50));
}

#[test]
fn layered_users_resolve_with_containerd_rules() {
    let t = tempfile::tempdir().unwrap();
    let l = t.path().join("l");
    write(&l, "etc/passwd", PASSWD);
    write(&l, "etc/group", GROUP);
    let u = LayeredUsers::new(vec![l]);
    assert_eq!(resolve_user("app", Some(&u)).unwrap(), (1000, 2000));
    assert_eq!(resolve_user("1000", Some(&u)).unwrap(), (1000, 2000));
    assert_eq!(resolve_user("4242", Some(&u)).unwrap(), (4242, 0));
    assert_eq!(resolve_user("app:staff", Some(&u)).unwrap(), (1000, 50));
    assert!(resolve_user("ghost", Some(&u)).is_err());
}

#[test]
fn missing_passwd_leaves_numeric_uids_working() {
    let t = tempfile::tempdir().unwrap();
    let u = LayeredUsers::new(vec![t.path().to_path_buf()]);
    assert_eq!(resolve_user("1234", Some(&u)).unwrap(), (1234, 0));
    assert!(resolve_user("root", Some(&u)).is_err());
}

/// An image-supplied symlink must never be followed: an absolute
/// `etc/passwd -> /etc/passwd` would read the *host's* user database.
#[test]
fn symlinked_passwd_is_refused_not_followed() {
    let t = tempfile::tempdir().unwrap();
    let l = t.path().join("l");
    fs::create_dir_all(l.join("etc")).unwrap();
    std::os::unix::fs::symlink("/etc/passwd", l.join("etc/passwd")).unwrap();
    let err = read_layered(std::slice::from_ref(&l), "etc/passwd").unwrap_err();
    assert!(err.contains("symlink"), "{err}");
    // A symlinked parent directory is refused too.
    let l2 = t.path().join("l2");
    fs::create_dir_all(&l2).unwrap();
    std::os::unix::fs::symlink("/etc", l2.join("etc")).unwrap();
    assert!(read_layered(&[l2], "etc/passwd")
        .unwrap_err()
        .contains("symlink"));
}

fn mknod_whiteout(p: &Path) -> bool {
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
    unsafe { libc::mknod(c.as_ptr(), libc::S_IFCHR, 0) == 0 }
}

/// overlayfs whiteouts (char device 0:0) hide the entry and all lower layers.
/// Needs CAP_MKNOD, so it only runs as root (ipc7).
#[test]
fn whiteout_hides_lower_layers() {
    if !is_root() {
        eprintln!("skipping whiteout test: needs root for mknod");
        return;
    }
    let t = tempfile::tempdir().unwrap();
    let (bottom, top) = (t.path().join("bottom"), t.path().join("top"));
    write(&bottom, "etc/passwd", PASSWD);
    fs::create_dir_all(top.join("etc")).unwrap();
    assert!(mknod_whiteout(&top.join("etc/passwd")));
    assert_eq!(
        read_layered(&[top.clone(), bottom.clone()], "etc/passwd").unwrap(),
        None
    );
    // Whiteout of a parent directory hides the whole subtree.
    let top2 = t.path().join("top2");
    fs::create_dir_all(&top2).unwrap();
    assert!(mknod_whiteout(&top2.join("etc")));
    assert_eq!(read_layered(&[top2, bottom], "etc/passwd").unwrap(), None);
    // Sanity: the node really is a 0:0 char device.
    assert_eq!(fs::metadata(top.join("etc/passwd")).unwrap().rdev(), 0);
}

/// An opaque directory hides lower layers' content from that directory down
/// (root-only: the xattr is `trusted.*`).
#[test]
fn opaque_directory_hides_lower_layers() {
    if !is_root() {
        eprintln!("skipping opaque test: needs root for trusted.* xattrs");
        return;
    }
    let t = tempfile::tempdir().unwrap();
    let (bottom, top) = (t.path().join("bottom"), t.path().join("top"));
    write(&bottom, "etc/passwd", PASSWD);
    fs::create_dir_all(top.join("etc")).unwrap();
    let c = std::ffi::CString::new(top.join("etc").as_os_str().as_bytes()).unwrap();
    let rc = unsafe {
        libc::lsetxattr(
            c.as_ptr(),
            b"trusted.overlay.opaque\0".as_ptr() as *const libc::c_char,
            b"y".as_ptr() as *const libc::c_void,
            1,
            0,
        )
    };
    if rc != 0 {
        eprintln!("skipping opaque test: filesystem rejects trusted xattrs");
        return;
    }
    assert!(is_opaque(&top.join("etc")));
    assert_eq!(
        read_layered(&[top.clone(), bottom.clone()], "etc/passwd").unwrap(),
        None,
        "opaque etc/ in the top layer hides the bottom passwd"
    );
    // An opaque dir that itself contains the file still serves it.
    write(&top, "etc/passwd", PASSWD);
    assert!(read_layered(&[top, bottom], "etc/passwd")
        .unwrap()
        .is_some());
}

// ── real store (root, only where an image is pulled; ipc7) ─────────────────

/// First complete image in the system store, if any.
fn system_image() -> Option<(StoreLayout, String, serde_json::Value)> {
    let store = StoreLayout::system();
    for ent in fs::read_dir(&store.images).ok()?.flatten() {
        let Ok(data) = fs::read_to_string(ent.path().join("manifest.json")) else {
            continue;
        };
        let Ok(m) = serde_json::from_str::<serde_json::Value>(&data) else {
            continue;
        };
        let r = m["reference"].as_str()?.to_string();
        if layers_top_first(&store, &r, &m).is_ok() {
            return Some((store, r, m));
        }
    }
    None
}

/// The whole point: the descriptor we build is a mount a shim can really
/// perform. Mount it exactly as a shim would (from the `Mount` fields), read
/// image content through it, write into the upper, then unmount and clean up.
#[test]
fn descriptor_mounts_a_real_image_as_a_shim_would() {
    if !is_root() {
        eprintln!("skipping real-overlay test: needs root");
        return;
    }
    let Some((store, reference, manifest)) = system_image() else {
        eprintln!("skipping real-overlay test: no pulled image in the system store");
        return;
    };
    let layers = layers_top_first(&store, &reference, &manifest).unwrap();
    let id = format!("t593-{}", std::process::id());
    let plan = plan_rootfs(&store, &id, layers).unwrap();
    let m = plan.to_api_mount();

    let target = tempfile::tempdir().unwrap();
    let (typ, src, data) = (
        std::ffi::CString::new(m.type_.clone()).unwrap(),
        std::ffi::CString::new(m.source.clone()).unwrap(),
        std::ffi::CString::new(m.options.join(",")).unwrap(),
    );
    let tgt = std::ffi::CString::new(target.path().as_os_str().as_bytes()).unwrap();
    let rc = unsafe {
        libc::mount(
            src.as_ptr(),
            tgt.as_ptr(),
            typ.as_ptr(),
            0,
            data.as_ptr() as *const libc::c_void,
        )
    };
    assert_eq!(
        rc,
        0,
        "overlay mount from the descriptor failed: {}",
        io::Error::last_os_error()
    );

    // While mounted, cleanup must refuse to delete the live upper.
    assert!(matches!(plan.cleanup(), Err(RootfsError::StillMounted(_))));

    let has_bin = target.path().join("bin").exists() || target.path().join("usr/bin").exists();
    fs::write(target.path().join("pelagos-593-probe"), b"x").unwrap();
    let in_upper = plan.upper_dir().join("pelagos-593-probe").exists();

    let urc = unsafe { libc::umount(tgt.as_ptr()) };
    assert_eq!(urc, 0, "umount: {}", io::Error::last_os_error());
    assert!(has_bin, "image content visible through the overlay");
    assert!(in_upper, "writes land in the ephemeral upper dir");

    plan.cleanup().unwrap();
    assert!(!plan.scratch_dir().exists());
}

/// Resolve users through a real image's layers.
#[test]
fn real_image_user_resolution() {
    if !is_root() {
        eprintln!("skipping real-image user test: needs root");
        return;
    }
    let Some((store, reference, manifest)) = system_image() else {
        eprintln!("skipping real-image user test: no pulled image in the system store");
        return;
    };
    let layers = layers_top_first(&store, &reference, &manifest).unwrap();
    let u = LayeredUsers::new(layers);
    match resolve_user("root", Some(&u)) {
        Ok(ids) => assert_eq!(ids, (0, 0)),
        Err(e) => eprintln!("image {reference} has no readable root passwd entry: {e}"),
    }
    // Numeric uid never errors, passwd entry or not.
    assert!(resolve_user("4242", Some(&u)).is_ok());
}
