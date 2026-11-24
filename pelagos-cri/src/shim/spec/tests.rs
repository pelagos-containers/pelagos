use super::*;
use serde_json::json;

fn sandbox() -> CriSandbox {
    serde_json::from_value(json!({
        "id": "sb1", "name": "pod-a", "namespace": "default", "uid": "uid-1",
        "attempt": 0, "labels": {}, "annotations": {}, "created_at_ns": 0,
        "state": "Running",
    }))
    .unwrap()
}

fn container() -> CriContainer {
    serde_json::from_value(json!({
        "id": "c1", "sandbox_id": "sb1", "pelagos_name": "pcri-c1", "name": "app",
        "image": "docker.io/library/alpine:3", "entrypoint": [], "args": [],
        "envs": [], "working_dir": "", "mounts": [], "labels": {}, "annotations": {},
        "created_at_ns": 0, "started_at_ns": 0, "finished_at_ns": 0,
        "state": "Created", "exit_code": 0,
    }))
    .unwrap()
}

fn image() -> ImageDefaults {
    ImageDefaults {
        entrypoint: vec!["/entry".into()],
        cmd: vec!["--serve".into()],
        env: vec![("PATH".into(), "/bin".into()), ("A".into(), "img".into())],
        working_dir: "/srv".into(),
        user: String::new(),
    }
}

fn build(c: &CriContainer, sb: &CriSandbox, img: &ImageDefaults) -> Result<Spec, SpecError> {
    build_container_spec(&ContainerSpecInput {
        container: c,
        sandbox: sb,
        image: img,
        netns_path: Some("/var/run/netns/pcri-sb1"),
        extra_mounts: &[],
        devices: &[],
        seccomp: None,
        hostname: Some("pod-a"),
        users: None,
    })
}

fn ann(s: &Spec) -> &HashMap<String, String> {
    s.annotations().as_ref().unwrap()
}

fn caps_of(s: &Spec) -> BTreeSet<String> {
    s.process()
        .as_ref()
        .unwrap()
        .capabilities()
        .as_ref()
        .unwrap()
        .bounding()
        .as_ref()
        .unwrap()
        .iter()
        .map(|c| {
            serde_json::to_value(c)
                .unwrap()
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect()
}

// ── annotation invariants (the Kata-critical part) ──────────────────────────

#[test]
fn container_always_typed_container_with_sandbox_id() {
    let s = build(&container(), &sandbox(), &image()).unwrap();
    assert_eq!(ann(&s)[ANN_CONTAINER_TYPE], "container");
    assert_eq!(ann(&s)[ANN_SANDBOX_ID], "sb1");
    assert_eq!(ann(&s)[ANN_CONTAINER_NAME], "app");
    assert_eq!(ann(&s)[ANN_SANDBOX_NAME], "pod-a");
    assert_eq!(ann(&s)[ANN_SANDBOX_NAMESPACE], "default");
    assert_eq!(ann(&s)[ANN_IMAGE_NAME], "docker.io/library/alpine:3");
}

#[test]
fn sandbox_always_typed_sandbox() {
    let s = build_sandbox_spec(&SandboxSpecInput {
        sandbox: &sandbox(),
        netns_path: None,
        resources: None,
        hostname: None,
        pause_args: &[],
    })
    .unwrap();
    assert_eq!(ann(&s)[ANN_CONTAINER_TYPE], "sandbox");
    assert_eq!(ann(&s)[ANN_SANDBOX_ID], "sb1");
    assert_eq!(
        s.process().as_ref().unwrap().args().as_ref().unwrap(),
        &vec!["/pause".to_string()]
    );
}

/// A pod annotation must never be able to flip the container type or sandbox
/// id: that would silently boot a VM per container (or join the wrong pod).
#[test]
fn user_annotations_cannot_override_reserved_keys() {
    let mut c = container();
    c.annotations
        .insert(ANN_CONTAINER_TYPE.into(), "sandbox".into());
    c.annotations.insert(ANN_SANDBOX_ID.into(), "evil".into());
    c.annotations.insert("keep.me".into(), "yes".into());
    let s = build(&c, &sandbox(), &image()).unwrap();
    assert_eq!(ann(&s)[ANN_CONTAINER_TYPE], "container");
    assert_eq!(ann(&s)[ANN_SANDBOX_ID], "sb1");
    assert_eq!(ann(&s)["keep.me"], "yes");

    let mut sb = sandbox();
    sb.annotations
        .insert(ANN_CONTAINER_TYPE.into(), "container".into());
    let s = build_sandbox_spec(&SandboxSpecInput {
        sandbox: &sb,
        netns_path: None,
        resources: None,
        hostname: None,
        pause_args: &[],
    })
    .unwrap();
    assert_eq!(ann(&s)[ANN_CONTAINER_TYPE], "sandbox");
}

#[test]
fn sidecar_label_does_not_change_cri_container_type() {
    let mut c = container();
    c.is_sidecar = true;
    c.labels.insert(
        "io.kubernetes.cri.container-type".into(),
        "sidecar_container".into(),
    );
    let s = build(&c, &sandbox(), &image()).unwrap();
    assert_eq!(ann(&s)[ANN_CONTAINER_TYPE], "container");
}

#[test]
fn sandbox_resources_become_annotations() {
    let s = build_sandbox_spec(&SandboxSpecInput {
        sandbox: &sandbox(),
        netns_path: None,
        resources: Some(SandboxResources {
            cpu_quota: 50_000,
            cpu_period: 100_000,
            cpu_shares: 512,
            memory_limit: 1 << 30,
        }),
        hostname: None,
        pause_args: &[],
    })
    .unwrap();
    assert_eq!(ann(&s)[ANN_SANDBOX_CPU_QUOTA], "50000");
    assert_eq!(ann(&s)[ANN_SANDBOX_CPU_PERIOD], "100000");
    assert_eq!(ann(&s)[ANN_SANDBOX_CPU_SHARES], "512");
    assert_eq!(ann(&s)[ANN_SANDBOX_MEMORY], "1073741824");
}

#[test]
fn sandbox_resources_from_cri_sum() {
    use crate::cri::LinuxContainerResources as R;
    assert!(SandboxResources::from_cri(None).is_none());
    // The kubelet sends zeros when some container has no limit.
    assert!(SandboxResources::from_cri(Some(&R::default())).is_none());
    let r = R {
        cpu_quota: 50_000,
        cpu_period: 100_000,
        cpu_shares: 512,
        memory_limit_in_bytes: 256 << 20,
        ..Default::default()
    };
    let s = SandboxResources::from_cri(Some(&r)).unwrap();
    assert_eq!(
        (s.cpu_quota, s.cpu_period, s.cpu_shares),
        (50_000, 100_000, 512)
    );
    assert_eq!(s.memory_limit, 256 << 20);
}

// ── process ─────────────────────────────────────────────────────────────────

#[test]
fn argv_follows_cri_entrypoint_cmd_rules() {
    // No CRI overrides: image entrypoint + cmd.
    let s = build(&container(), &sandbox(), &image()).unwrap();
    assert_eq!(
        s.process().as_ref().unwrap().args().as_ref().unwrap(),
        &vec!["/entry".to_string(), "--serve".to_string()]
    );
    // CRI command overrides the entrypoint AND drops the image cmd.
    let mut c = container();
    c.entrypoint = vec!["/bin/sh".into()];
    let s = build(&c, &sandbox(), &image()).unwrap();
    assert_eq!(
        s.process().as_ref().unwrap().args().as_ref().unwrap(),
        &vec!["/bin/sh".to_string()]
    );
    // CRI args alone replace only the cmd.
    let mut c = container();
    c.args = vec!["-x".into()];
    let s = build(&c, &sandbox(), &image()).unwrap();
    assert_eq!(
        s.process().as_ref().unwrap().args().as_ref().unwrap(),
        &vec!["/entry".to_string(), "-x".to_string()]
    );
}

#[test]
fn empty_argv_is_an_error() {
    let r = build(&container(), &sandbox(), &ImageDefaults::default());
    assert!(matches!(r, Err(SpecError::EmptyArgs)), "{r:?}");
}

#[test]
fn env_merges_image_then_cri_overriding_by_key() {
    let mut c = container();
    c.envs = vec![("A".into(), "cri".into()), ("B".into(), "2".into())];
    let s = build(&c, &sandbox(), &image()).unwrap();
    assert_eq!(
        s.process().as_ref().unwrap().env().as_ref().unwrap(),
        &vec![
            "PATH=/bin".to_string(),
            "A=cri".to_string(),
            "B=2".to_string()
        ]
    );
}

#[test]
fn cwd_prefers_cri_then_image_then_root() {
    let mut c = container();
    c.working_dir = "/work".into();
    assert_eq!(
        build(&c, &sandbox(), &image())
            .unwrap()
            .process()
            .as_ref()
            .unwrap()
            .cwd(),
        &PathBuf::from("/work")
    );
    assert_eq!(
        build(&container(), &sandbox(), &image())
            .unwrap()
            .process()
            .as_ref()
            .unwrap()
            .cwd(),
        &PathBuf::from("/srv")
    );
    let mut img = image();
    img.working_dir.clear();
    assert_eq!(
        build(&container(), &sandbox(), &img)
            .unwrap()
            .process()
            .as_ref()
            .unwrap()
            .cwd(),
        &PathBuf::from("/")
    );
}

/// In-memory image user database for the resolution rules.
struct Db {
    passwd: Vec<(&'static str, u32, u32)>,
    group: Vec<(&'static str, u32)>,
}

impl UserDb for Db {
    fn passwd_gid_by_uid(&self, uid: u32) -> Result<Option<u32>, String> {
        Ok(self.passwd.iter().find(|p| p.1 == uid).map(|p| p.2))
    }
    fn passwd_by_name(&self, name: &str) -> Result<Option<(u32, u32)>, String> {
        Ok(self.passwd.iter().find(|p| p.0 == name).map(|p| (p.1, p.2)))
    }
    fn gid_by_group_name(&self, name: &str) -> Result<Option<u32>, String> {
        Ok(self.group.iter().find(|g| g.0 == name).map(|g| g.1))
    }
}

fn db() -> Db {
    Db {
        passwd: vec![
            ("root", 0, 0),
            ("app", 1000, 2000),
            ("nobody", 65534, 65534),
        ],
        group: vec![("staff", 50), ("app", 2000)],
    }
}

/// containerd's `oci.WithUser` rules, including the one a naive reading gets
/// wrong: a bare uid takes its gid from passwd and falls back to **0**, not to
/// the uid.
#[test]
fn user_string_follows_containerd_rules() {
    let d = db();
    let r = |u: &str| resolve_user(u, Some(&d));
    assert_eq!(r("").unwrap(), (0, 0));
    assert_eq!(r("1000").unwrap(), (1000, 2000), "gid from passwd");
    assert_eq!(r("4242").unwrap(), (4242, 0), "no passwd entry: gid 0");
    assert_eq!(r("app").unwrap(), (1000, 2000), "name: both from passwd");
    assert_eq!(r("1000:7").unwrap(), (1000, 7), "numeric pair as given");
    assert_eq!(
        r("app:staff").unwrap(),
        (1000, 50),
        "uid from name, gid from group"
    );
    assert_eq!(r("4242:staff").unwrap(), (4242, 50));
    assert_eq!(r("app:77").unwrap(), (1000, 77));
    assert!(matches!(r("ghost"), Err(SpecError::UserLookup(_))));
    assert!(matches!(r("app:ghosts"), Err(SpecError::UserLookup(_))));
    assert!(matches!(r("1:2:3"), Err(SpecError::UserLookup(_))));
    assert!(matches!(r("99999999999"), Err(SpecError::UserLookup(_))));
    // Without a database only all-numeric forms resolve; bare uid gets gid 0.
    assert_eq!(resolve_user("1000", None).unwrap(), (1000, 0));
    assert_eq!(resolve_user("1000:2000", None).unwrap(), (1000, 2000));
    assert!(matches!(
        resolve_user("app", None),
        Err(SpecError::UnresolvedUser(_))
    ));
}

#[test]
fn user_resolution_in_spec() {
    let uidgid = |c: &CriContainer, img: &ImageDefaults, users: Option<&dyn UserDb>| {
        let s = build_container_spec(&ContainerSpecInput {
            container: c,
            sandbox: &sandbox(),
            image: img,
            netns_path: None,
            extra_mounts: &[],
            devices: &[],
            seccomp: None,
            hostname: None,
            users,
        })
        .unwrap();
        let u = s.process().as_ref().unwrap().user().clone();
        (u.uid(), u.gid(), u.additional_gids().clone())
    };
    let d = db();
    assert_eq!(uidgid(&container(), &image(), Some(&d)), (0, 0, None));
    // Image user.
    let mut img = image();
    img.user = "app".into();
    assert_eq!(uidgid(&container(), &img, Some(&d)), (1000, 2000, None));
    // CRI runAsUser wins over the image user; its gid comes from passwd.
    let mut c = container();
    c.run_as_user = Some(1000);
    assert_eq!(uidgid(&c, &img, Some(&d)), (1000, 2000, None));
    // runAsUser + runAsGroup are taken as a numeric pair.
    c.run_as_group = Some(65533);
    c.supplemental_groups = vec![7, 8];
    let mut sb = sandbox();
    sb.supplemental_groups = vec![8, 9];
    let s = build_container_spec(&ContainerSpecInput {
        container: &c,
        sandbox: &sb,
        image: &img,
        netns_path: None,
        extra_mounts: &[],
        devices: &[],
        seccomp: None,
        hostname: None,
        users: Some(&d),
    })
    .unwrap();
    let u = s.process().as_ref().unwrap().user().clone();
    assert_eq!((u.uid(), u.gid()), (1000, 65533));
    assert_eq!(
        u.additional_gids().clone(),
        Some(vec![8, 9, 7]),
        "merged, deduped"
    );
    // runAsUserName resolves through the database.
    let mut c = container();
    c.run_as_username = "nobody".into();
    assert_eq!(uidgid(&c, &image(), Some(&d)), (65534, 65534, None));
}

#[test]
fn usernames_error_without_a_user_database() {
    let mut img = image();
    img.user = "nobody".into();
    assert!(matches!(
        build(&container(), &sandbox(), &img),
        Err(SpecError::UnresolvedUser(_))
    ));
    let mut c = container();
    c.run_as_username = "nobody".into();
    assert!(matches!(
        build(&c, &sandbox(), &image()),
        Err(SpecError::UnresolvedUser(_))
    ));
}

#[test]
fn capabilities_default_add_drop_all_privileged() {
    let def = caps_of(&build(&container(), &sandbox(), &image()).unwrap());
    assert!(def.contains("CAP_CHOWN") && def.contains("CAP_NET_BIND_SERVICE"));
    assert!(!def.contains("CAP_SYS_ADMIN"));
    assert_eq!(def.len(), DEFAULT_CAPS.len());

    let mut c = container();
    c.cap_add = vec!["SYS_ADMIN".into(), "cap_net_admin".into()];
    c.cap_drop = vec!["CAP_CHOWN".into()];
    let s = caps_of(&build(&c, &sandbox(), &image()).unwrap());
    assert!(s.contains("CAP_SYS_ADMIN") && s.contains("CAP_NET_ADMIN"));
    assert!(!s.contains("CAP_CHOWN"));

    let mut c = container();
    c.cap_drop = vec!["ALL".into()];
    assert!(caps_of(&build(&c, &sandbox(), &image()).unwrap()).is_empty());
    c.cap_add = vec!["NET_BIND_SERVICE".into()];
    assert_eq!(
        caps_of(&build(&c, &sandbox(), &image()).unwrap()),
        BTreeSet::from(["CAP_NET_BIND_SERVICE".to_string()])
    );

    let mut c = container();
    c.privileged = true;
    let p = caps_of(&build(&c, &sandbox(), &image()).unwrap());
    assert!(p.contains("CAP_SYS_ADMIN") && p.contains("CAP_BPF"));
    assert_eq!(p.len(), ALL_CAPS.len());
}

#[test]
fn unknown_capability_is_an_error() {
    let mut c = container();
    c.cap_add = vec!["NOT_A_CAP".into()];
    assert!(matches!(
        build(&c, &sandbox(), &image()),
        Err(SpecError::InvalidCapability(_))
    ));
}

#[test]
fn every_listed_capability_parses() {
    for n in ALL_CAPS.iter().chain(DEFAULT_CAPS.iter()) {
        cap(n).unwrap_or_else(|_| panic!("{n} must be a known capability"));
    }
}

#[test]
fn process_flags() {
    let mut c = container();
    c.tty = true;
    c.no_new_privs = true;
    c.oom_score_adj = 500;
    c.selinux_label = "system_u:system_r:container_t:s0".into();
    c.apparmor_profile_type = 2;
    c.apparmor_profile_path = "my-profile".into();
    let s = build(&c, &sandbox(), &image()).unwrap();
    let p = s.process().as_ref().unwrap();
    assert_eq!(p.terminal(), Some(true));
    assert_eq!(p.no_new_privileges(), Some(true));
    assert_eq!(p.oom_score_adj(), Some(500));
    assert_eq!(p.apparmor_profile().as_deref(), Some("my-profile"));
    assert_eq!(
        p.selinux_label().as_deref(),
        Some("system_u:system_r:container_t:s0")
    );
    // Unset oom_score_adj (i32::MIN sentinel) is omitted.
    let s = build(&container(), &sandbox(), &image()).unwrap();
    assert_eq!(s.process().as_ref().unwrap().oom_score_adj(), None);
}

// ── root, mounts, namespaces, paths ────────────────────────────────────────

#[test]
fn root_is_relative_rootfs_and_honours_readonly() {
    let s = build(&container(), &sandbox(), &image()).unwrap();
    assert_eq!(s.root().as_ref().unwrap().path(), &PathBuf::from("rootfs"));
    assert_eq!(s.root().as_ref().unwrap().readonly(), Some(false));
    let mut c = container();
    c.read_only_rootfs = true;
    let s = build(&c, &sandbox(), &image()).unwrap();
    assert_eq!(s.root().as_ref().unwrap().readonly(), Some(true));
}

#[test]
fn mounts_standard_set_then_cri_binds_in_order() {
    let mut c = container();
    c.mounts = vec![
        CriMount {
            host_path: "/data".into(),
            container_path: "/mnt/data".into(),
            readonly: false,
            recursive_read_only: false,
            propagation: 0,
        },
        CriMount {
            host_path: "/etc/cfg".into(),
            container_path: "/cfg".into(),
            readonly: true,
            recursive_read_only: true,
            propagation: 2,
        },
        CriMount {
            host_path: "/ro".into(),
            container_path: "/ro".into(),
            readonly: true,
            recursive_read_only: false,
            propagation: 1,
        },
    ];
    let extra = [CriMount {
        host_path: "/run/x/resolv.conf".into(),
        container_path: "/etc/resolv.conf".into(),
        readonly: true,
        recursive_read_only: false,
        propagation: 0,
    }];
    let s = build_container_spec(&ContainerSpecInput {
        container: &c,
        sandbox: &sandbox(),
        image: &image(),
        netns_path: None,
        extra_mounts: &extra,
        devices: &[],
        seccomp: None,
        hostname: None,
        users: None,
    })
    .unwrap();
    let ms = s.mounts().as_ref().unwrap();
    let dests: Vec<_> = ms
        .iter()
        .map(|m| m.destination().display().to_string())
        .collect();
    assert_eq!(
        &dests[..7],
        [
            "/proc",
            "/dev",
            "/dev/pts",
            "/dev/shm",
            "/dev/mqueue",
            "/sys",
            "/sys/fs/cgroup"
        ]
    );
    assert_eq!(
        &dests[7..],
        ["/mnt/data", "/cfg", "/ro", "/etc/resolv.conf"]
    );
    let opts = |i: usize| ms[i].options().clone().unwrap();
    assert_eq!(opts(7), ["rbind", "rprivate"]);
    assert_eq!(opts(8), ["rbind", "rshared", "rro"]);
    assert_eq!(opts(9), ["rbind", "rslave", "ro"]);
    assert_eq!(ms[7].typ().as_deref(), Some("bind"));
    assert_eq!(ms[7].source().as_ref().unwrap(), &PathBuf::from("/data"));
}

fn ns_types(s: &Spec) -> Vec<(LinuxNamespaceType, Option<String>)> {
    s.linux()
        .as_ref()
        .unwrap()
        .namespaces()
        .as_ref()
        .unwrap()
        .iter()
        .map(|n| (n.typ(), n.path().as_ref().map(|p| p.display().to_string())))
        .collect()
}

#[test]
fn pod_netns_joined_by_path_and_host_modes_omit_namespaces() {
    let s = build(&container(), &sandbox(), &image()).unwrap();
    let ns = ns_types(&s);
    assert!(ns.contains(&(
        LinuxNamespaceType::Network,
        Some("/var/run/netns/pcri-sb1".into())
    )));
    for t in [
        LinuxNamespaceType::Mount,
        LinuxNamespaceType::Pid,
        LinuxNamespaceType::Ipc,
        LinuxNamespaceType::Uts,
    ] {
        assert!(ns.contains(&(t, None)), "{t:?} must be unshared");
    }

    let mut sb = sandbox();
    sb.namespaces = crate::state::NamespaceModes::from_cri(2, 2, 2); // NODE x3
    let ns = ns_types(&build(&container(), &sb, &image()).unwrap());
    assert!(!ns.iter().any(|(t, _)| matches!(
        t,
        LinuxNamespaceType::Network | LinuxNamespaceType::Pid | LinuxNamespaceType::Ipc
    )));
    assert!(ns.contains(&(LinuxNamespaceType::Mount, None)));
}

#[test]
fn masked_and_readonly_paths_default_explicit_and_privileged() {
    let lin = |s: &Spec| s.linux().clone().unwrap();
    let s = build(&container(), &sandbox(), &image()).unwrap();
    assert!(lin(&s)
        .masked_paths()
        .clone()
        .unwrap()
        .contains(&"/proc/kcore".to_string()));
    assert!(lin(&s)
        .readonly_paths()
        .clone()
        .unwrap()
        .contains(&"/proc/sys".to_string()));

    let mut c = container();
    c.masked_paths = vec!["/only/this".into()];
    c.readonly_paths = vec!["/and/this".into()];
    let s = build(&c, &sandbox(), &image()).unwrap();
    assert_eq!(lin(&s).masked_paths().clone().unwrap(), ["/only/this"]);
    assert_eq!(lin(&s).readonly_paths().clone().unwrap(), ["/and/this"]);

    let mut c = container();
    c.privileged = true;
    let s = build(&c, &sandbox(), &image()).unwrap();
    assert_eq!(
        lin(&s).masked_paths().clone().unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(
        lin(&s).readonly_paths().clone().unwrap(),
        Vec::<String>::new()
    );
}

// ── resources, cgroup path, seccomp, devices ───────────────────────────────

#[test]
fn resources_and_cgroup_path() {
    let mut c = container();
    c.memory_limit = 256 << 20;
    c.memory_swap_limit = 512 << 20;
    c.cpu_shares = 512;
    c.cpu_quota = 50_000;
    c.cpu_period = 100_000;
    c.cpuset_cpus = "0-1".into();
    c.cpuset_mems = "0".into();
    // A zero limit (the kubelet's "not requested") must be dropped (#623).
    c.hugepage_limits = vec![("2MB".into(), 4 << 20), ("1GB".into(), 0)];
    let mut sb = sandbox();
    sb.cgroup_parent = "kubepods/besteffort/pod1".into();
    let s = build(&c, &sb, &image()).unwrap();
    let l = s.linux().clone().unwrap();
    let r = l.resources().clone().unwrap();
    let m = r.memory().unwrap();
    assert_eq!((m.limit(), m.swap()), (Some(256 << 20), Some(512 << 20)));
    let cpu = r.cpu().clone().unwrap();
    assert_eq!(
        (cpu.shares(), cpu.quota(), cpu.period()),
        (Some(512), Some(50_000), Some(100_000))
    );
    assert_eq!(cpu.cpus().as_deref(), Some("0-1"));
    assert_eq!(cpu.mems().as_deref(), Some("0"));
    assert_eq!(r.hugepage_limits().clone().unwrap().len(), 1);
    assert_eq!(
        l.cgroups_path().clone().unwrap(),
        PathBuf::from("kubepods/besteffort/pod1/c1")
    );
    // No limits and no parent: nothing emitted.
    let s = build(&container(), &sandbox(), &image()).unwrap();
    let l = s.linux().clone().unwrap();
    assert_eq!(l.cgroups_path(), &None);
    assert_eq!(l.resources().clone().unwrap().memory(), &None);
    assert_eq!(l.resources().clone().unwrap().cpu(), &None);
}

fn seccomp_profile() -> LinuxSeccomp {
    serde_json::from_value(json!({
        "defaultAction": "SCMP_ACT_ERRNO",
        "syscalls": [{"names": ["read"], "action": "SCMP_ACT_ALLOW"}],
    }))
    .unwrap()
}

#[test]
fn seccomp_only_emitted_for_localhost_and_never_when_privileged() {
    let with = |c: &CriContainer| {
        build_container_spec(&ContainerSpecInput {
            container: c,
            sandbox: &sandbox(),
            image: &image(),
            netns_path: None,
            extra_mounts: &[],
            devices: &[],
            seccomp: Some(seccomp_profile()),
            hostname: None,
            users: None,
        })
        .unwrap()
        .linux()
        .clone()
        .unwrap()
        .seccomp()
        .is_some()
    };
    let mut c = container();
    c.seccomp_profile_type = 0;
    assert!(!with(&c), "RuntimeDefault emits no explicit profile");
    c.seccomp_profile_type = 1;
    assert!(!with(&c), "Unconfined emits none");
    c.seccomp_profile_type = 2;
    assert!(with(&c), "Localhost carries the loaded profile");
    c.privileged = true;
    assert!(!with(&c), "privileged ignores any seccomp profile");
}

#[test]
fn seccomp_profile_loader_round_trips_json() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("p.json");
    std::fs::write(
        &p,
        br#"{"defaultAction":"SCMP_ACT_ERRNO","syscalls":[{"names":["read"],"action":"SCMP_ACT_ALLOW"}]}"#,
    )
    .unwrap();
    assert!(load_seccomp_profile(p.to_str().unwrap()).is_ok());
    std::fs::write(&p, b"not json").unwrap();
    assert!(load_seccomp_profile(p.to_str().unwrap()).is_err());
    assert!(load_seccomp_profile("/nonexistent/profile").is_err());
}

#[test]
fn devices_listed_and_allowed_in_cgroup() {
    let d = resolve_device(&CriDevice {
        host_path: "/dev/null".into(),
        container_path: "/dev/mynull".into(),
        permissions: "rw".into(),
    })
    .unwrap();
    assert_eq!((d.kind, d.major, d.minor), ('c', 1, 3));
    assert_eq!(d.path, "/dev/mynull");
    assert_eq!(d.permissions, "rw");
    let s = build_container_spec(&ContainerSpecInput {
        container: &container(),
        sandbox: &sandbox(),
        image: &image(),
        netns_path: None,
        extra_mounts: &[],
        devices: std::slice::from_ref(&d),
        seccomp: None,
        hostname: None,
        users: None,
    })
    .unwrap();
    let l = s.linux().clone().unwrap();
    let devs = l.devices().clone().unwrap();
    assert_eq!(devs.len(), 1);
    assert_eq!(devs[0].path(), &PathBuf::from("/dev/mynull"));
    let rules = l.resources().clone().unwrap().devices().clone().unwrap();
    assert_eq!(rules.len(), 1);
    assert!(rules[0].allow());
    assert_eq!(rules[0].access().as_deref(), Some("rw"));
    // Not a device node.
    assert!(resolve_device(&CriDevice {
        host_path: "/etc/hostname".into(),
        container_path: String::new(),
        permissions: String::new(),
    })
    .is_err());
}

// ── image defaults + serialization round trips ─────────────────────────────

#[test]
fn image_defaults_from_manifest() {
    let m = json!({"config": {
        "entrypoint": ["/e"], "cmd": ["a", "b"], "env": ["K=v", "X=y=z", "bad"],
        "working_dir": "/w", "user": "10:20"}});
    let d = ImageDefaults::from_manifest(&m);
    assert_eq!(d.entrypoint, ["/e"]);
    assert_eq!(d.cmd, ["a", "b"]);
    assert_eq!(
        d.env,
        [
            ("K".to_string(), "v".to_string()),
            ("X".to_string(), "y=z".to_string())
        ]
    );
    assert_eq!((d.working_dir.as_str(), d.user.as_str()), ("/w", "10:20"));
    // Missing config is all defaults.
    let d = ImageDefaults::from_manifest(&json!({}));
    assert!(d.entrypoint.is_empty() && d.env.is_empty() && d.user.is_empty());
}

/// The generated config.json must parse back as an OCI Spec and also through
/// pelagos's own OCI parser, and carry the Kata-critical annotations on disk.
#[test]
fn config_json_round_trips_and_is_written_to_a_bundle() {
    let mut c = container();
    c.tty = true;
    c.mounts = vec![CriMount {
        host_path: "/data".into(),
        container_path: "/d".into(),
        readonly: true,
        recursive_read_only: false,
        propagation: 0,
    }];
    let spec = build(&c, &sandbox(), &image()).unwrap();
    let bytes = to_config_json(&spec).unwrap();

    let back: Spec = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(back, spec, "serialize -> parse is lossless");

    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["ociVersion"], OCI_VERSION);
    assert_eq!(v["root"]["path"], "rootfs");
    assert_eq!(v["annotations"][ANN_CONTAINER_TYPE], "container");
    assert_eq!(v["annotations"][ANN_SANDBOX_ID], "sb1");
    assert_eq!(v["process"]["args"][0], "/entry");
    assert_eq!(v["process"]["terminal"], true);

    let t = tempfile::tempdir().unwrap();
    let b = crate::shim::bundle::Bundle::create(&t.path().join("b"), "k8s.io", "c1").unwrap();
    b.write_config(&bytes).unwrap();
    let on_disk = std::fs::read(b.dir().join("config.json")).unwrap();
    assert_eq!(on_disk, bytes);
}

/// pelagos's own `config.json` parser (used by `pelagos create`) must accept
/// what we generate, with the fields we set coming back intact.
#[test]
fn generated_spec_is_readable_by_pelagos_oci_parser() {
    let mut c = container();
    c.working_dir = "/work".into();
    let spec = build(&c, &sandbox(), &image()).unwrap();
    let cfg: pelagos::oci::OciConfig =
        serde_json::from_slice(&to_config_json(&spec).unwrap()).unwrap();
    assert_eq!(cfg.oci_version, OCI_VERSION);
    assert_eq!(cfg.root.path, "rootfs");
    let p = cfg.process.expect("process");
    assert_eq!(p.args, ["/entry", "--serve"]);
    assert_eq!(p.cwd, "/work");
    assert_eq!(cfg.hostname.as_deref(), Some("pod-a"));
    let a = cfg.annotations.expect("annotations");
    assert_eq!(a[ANN_CONTAINER_TYPE], "container");
    assert_eq!(a[ANN_SANDBOX_ID], "sb1");
}

#[test]
fn pod_sysctls_are_emitted_in_the_sandbox_spec_only() {
    let mut sb = sandbox();
    sb.sysctls
        .insert("net.ipv4.ip_local_port_range".into(), "1024 2048".into());
    let s = build_sandbox_spec(&SandboxSpecInput {
        sandbox: &sb,
        netns_path: None,
        resources: None,
        hostname: None,
        pause_args: &[],
    })
    .unwrap();
    let got = s.linux().clone().unwrap().sysctl().clone().unwrap();
    assert_eq!(got["net.ipv4.ip_local_port_range"], "1024 2048");
    // None configured: no sysctl map at all; containers never carry pod sysctls.
    let none = build_sandbox_spec(&SandboxSpecInput {
        sandbox: &sandbox(),
        netns_path: None,
        resources: None,
        hostname: None,
        pause_args: &[],
    })
    .unwrap();
    assert_eq!(none.linux().clone().unwrap().sysctl(), &None);
    let c = build(&container(), &sb, &image()).unwrap();
    assert_eq!(c.linux().clone().unwrap().sysctl(), &None);
}
