//! CRI -> OCI runtime-spec `config.json` generation for shim-backed runtimes
//! (#591, epic #574).
//!
//! A containerd shim (Kata, runsc, runwasi) is handed an OCI bundle, so for
//! shim-backed pods the CRI must produce a real `config.json` instead of the
//! `pelagos run` flags the native path uses. [`build_container_spec`] and
//! [`build_sandbox_spec`] are **pure**: they take already-resolved inputs and do
//! no filesystem or network access, so every rule below is unit-testable.
//! Anything that needs I/O (device `stat`, reading a localhost seccomp profile,
//! loading image config) lives in a small adapter next to them.
//!
//! Invariants that matter to Kata (`docs/SHIM_V2_CLIENT_DESIGN.md` section 3):
//!
//! - `io.kubernetes.cri.container-type` is **always** set (`sandbox` or
//!   `container`). Kata treats a missing key as a pod *sandbox* and would boot
//!   a VM per container.
//! - Containers **always** carry `io.kubernetes.cri.sandbox-id`.
//! - Those reserved keys are written *after* user annotations, so a pod
//!   annotation can never override them.
//!
//! The rootfs is `{path: "rootfs"}`: the shim mounts the overlay from the
//! `CreateTaskRequest` mounts (separate epic item), not this module.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::str::FromStr;

use oci_spec::runtime::{
    Capability, LinuxBuilder, LinuxCapabilitiesBuilder, LinuxCpuBuilder, LinuxDevice,
    LinuxDeviceBuilder, LinuxDeviceCgroupBuilder, LinuxDeviceType, LinuxHugepageLimitBuilder,
    LinuxMemoryBuilder, LinuxNamespaceBuilder, LinuxNamespaceType, LinuxResourcesBuilder,
    LinuxSeccomp, MountBuilder, ProcessBuilder, RootBuilder, Spec, SpecBuilder, UserBuilder,
};

use oci_spec::OciSpecError;

use crate::state::{CriContainer, CriDevice, CriMount, CriSandbox};

/// OCI runtime-spec version written to `ociVersion`.
pub const OCI_VERSION: &str = "1.1.0";

pub const ANN_CONTAINER_TYPE: &str = "io.kubernetes.cri.container-type";
pub const ANN_SANDBOX_ID: &str = "io.kubernetes.cri.sandbox-id";
pub const ANN_CONTAINER_NAME: &str = "io.kubernetes.cri.container-name";
pub const ANN_IMAGE_NAME: &str = "io.kubernetes.cri.image-name";
pub const ANN_SANDBOX_NAME: &str = "io.kubernetes.cri.sandbox-name";
pub const ANN_SANDBOX_NAMESPACE: &str = "io.kubernetes.cri.sandbox-namespace";
pub const ANN_SANDBOX_UID: &str = "io.kubernetes.cri.sandbox-uid";
pub const ANN_SANDBOX_CPU_QUOTA: &str = "io.kubernetes.cri.sandbox-cpu-quota";
pub const ANN_SANDBOX_CPU_PERIOD: &str = "io.kubernetes.cri.sandbox-cpu-period";
pub const ANN_SANDBOX_CPU_SHARES: &str = "io.kubernetes.cri.sandbox-cpu-shares";
pub const ANN_SANDBOX_MEMORY: &str = "io.kubernetes.cri.sandbox-memory";

/// Every key this module owns; user annotations with these keys are discarded.
const RESERVED_ANNOTATIONS: &[&str] = &[
    ANN_CONTAINER_TYPE,
    ANN_SANDBOX_ID,
    ANN_CONTAINER_NAME,
    ANN_IMAGE_NAME,
    ANN_SANDBOX_NAME,
    ANN_SANDBOX_NAMESPACE,
    ANN_SANDBOX_UID,
    ANN_SANDBOX_CPU_QUOTA,
    ANN_SANDBOX_CPU_PERIOD,
    ANN_SANDBOX_CPU_SHARES,
    ANN_SANDBOX_MEMORY,
];

/// Docker/containerd default capability set for unprivileged containers.
const DEFAULT_CAPS: &[&str] = &[
    "CHOWN",
    "DAC_OVERRIDE",
    "FSETID",
    "FOWNER",
    "MKNOD",
    "NET_RAW",
    "SETGID",
    "SETUID",
    "SETFCAP",
    "SETPCAP",
    "NET_BIND_SERVICE",
    "SYS_CHROOT",
    "KILL",
    "AUDIT_WRITE",
];

/// Every Linux capability; the privileged set.
const ALL_CAPS: &[&str] = &[
    "AUDIT_CONTROL",
    "AUDIT_READ",
    "AUDIT_WRITE",
    "BLOCK_SUSPEND",
    "BPF",
    "CHECKPOINT_RESTORE",
    "CHOWN",
    "DAC_OVERRIDE",
    "DAC_READ_SEARCH",
    "FOWNER",
    "FSETID",
    "IPC_LOCK",
    "IPC_OWNER",
    "KILL",
    "LEASE",
    "LINUX_IMMUTABLE",
    "MAC_ADMIN",
    "MAC_OVERRIDE",
    "MKNOD",
    "NET_ADMIN",
    "NET_BIND_SERVICE",
    "NET_BROADCAST",
    "NET_RAW",
    "PERFMON",
    "SETFCAP",
    "SETGID",
    "SETPCAP",
    "SETUID",
    "SYS_ADMIN",
    "SYS_BOOT",
    "SYS_CHROOT",
    "SYS_MODULE",
    "SYS_NICE",
    "SYS_PACCT",
    "SYS_PTRACE",
    "SYS_RAWIO",
    "SYS_RESOURCE",
    "SYS_TIME",
    "SYS_TTY_CONFIG",
    "SYSLOG",
    "WAKE_ALARM",
];

/// OCI-recommended paths hidden from unprivileged containers when the CRI
/// request does not specify its own.
const DEFAULT_MASKED_PATHS: &[&str] = &[
    "/proc/acpi",
    "/proc/asound",
    "/proc/kcore",
    "/proc/keys",
    "/proc/latency_stats",
    "/proc/timer_list",
    "/proc/timer_stats",
    "/proc/sched_debug",
    "/proc/scsi",
    "/sys/firmware",
    "/sys/devices/virtual/powercap",
];

/// OCI-recommended read-only paths (see [`DEFAULT_MASKED_PATHS`]).
const DEFAULT_READONLY_PATHS: &[&str] = &[
    "/proc/bus",
    "/proc/fs",
    "/proc/irq",
    "/proc/sys",
    "/proc/sysrq-trigger",
];

#[derive(Debug)]
pub enum SpecError {
    /// Entrypoint and cmd are both empty after merging CRI and image values.
    EmptyArgs,
    /// The user is (partly) a name but no [`UserDb`] was supplied to resolve it.
    UnresolvedUser(String),
    /// Resolving the user against the image's passwd/group files failed.
    UserLookup(String),
    InvalidCapability(String),
    Oci(OciSpecError),
}

impl std::fmt::Display for SpecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpecError::EmptyArgs => write!(f, "container has no command (entrypoint and cmd empty)"),
            SpecError::UnresolvedUser(u) => write!(
                f,
                "user {u:?} names a user or group but no image user database was provided (use numeric uid:gid)"
            ),
            SpecError::UserLookup(m) => write!(f, "user lookup failed: {m}"),
            SpecError::InvalidCapability(c) => write!(f, "unknown capability {c:?}"),
            SpecError::Oci(e) => write!(f, "invalid OCI spec: {e}"),
        }
    }
}

impl std::error::Error for SpecError {}

impl From<OciSpecError> for SpecError {
    fn from(e: OciSpecError) -> Self {
        SpecError::Oci(e)
    }
}

/// Defaults taken from the image config.
#[derive(Debug, Clone, Default)]
pub struct ImageDefaults {
    pub entrypoint: Vec<String>,
    pub cmd: Vec<String>,
    /// `KEY=VALUE` pairs in image order.
    pub env: Vec<(String, String)>,
    pub working_dir: String,
    /// Image `User`: `uid`, `uid:gid`, or a name (rejected, see
    /// [`SpecError::UnresolvedUser`]). Empty means root.
    pub user: String,
}

impl ImageDefaults {
    /// Read the `config` object `pelagos image pull` stores in an image's
    /// `manifest.json`.
    pub fn from_manifest(manifest: &serde_json::Value) -> Self {
        let strs = |v: &serde_json::Value| -> Vec<String> {
            v.as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default()
        };
        let cfg = &manifest["config"];
        let env = strs(&cfg["env"])
            .into_iter()
            .filter_map(|kv| {
                kv.split_once('=')
                    .map(|(k, v)| (k.to_string(), v.to_string()))
            })
            .collect();
        Self {
            entrypoint: strs(&cfg["entrypoint"]),
            cmd: strs(&cfg["cmd"]),
            env,
            working_dir: cfg["working_dir"]
                .as_str()
                .or_else(|| cfg["workingDir"].as_str())
                .unwrap_or("")
                .to_string(),
            user: cfg["user"].as_str().unwrap_or("").to_string(),
        }
    }
}

/// A host device resolved by [`resolve_device`] (needs `stat`, so it is not
/// part of the pure builder).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedDevice {
    pub path: String,
    /// `c`, `b` or `p`.
    pub kind: char,
    pub major: i64,
    pub minor: i64,
    pub file_mode: u32,
    pub uid: u32,
    pub gid: u32,
    /// Cgroup permissions, e.g. `rwm`.
    pub permissions: String,
}

/// `stat` the host device behind a CRI device. The container-side path is
/// `container_path` (or the host path when empty).
pub fn resolve_device(d: &CriDevice) -> std::io::Result<ResolvedDevice> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let md = std::fs::metadata(&d.host_path)?;
    let ft = md.file_type();
    let kind = if ft.is_char_device() {
        'c'
    } else if ft.is_block_device() {
        'b'
    } else if ft.is_fifo() {
        'p'
    } else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} is not a device node", d.host_path),
        ));
    };
    let rdev = md.rdev();
    Ok(ResolvedDevice {
        path: if d.container_path.is_empty() {
            d.host_path.clone()
        } else {
            d.container_path.clone()
        },
        kind,
        major: libc::major(rdev) as i64,
        minor: libc::minor(rdev) as i64,
        file_mode: md.mode() & 0o7777,
        uid: md.uid(),
        gid: md.gid(),
        permissions: if d.permissions.is_empty() {
            "rwm".to_string()
        } else {
            d.permissions.clone()
        },
    })
}

/// Pod-level resource hints Kata sizes the VM from.
#[derive(Debug, Clone, Copy, Default)]
pub struct SandboxResources {
    pub cpu_quota: i64,
    pub cpu_period: u64,
    pub cpu_shares: u64,
    pub memory_limit: i64,
}

impl SandboxResources {
    /// Pod-level sizing from the CRI `LinuxPodSandboxConfig.resources` (the
    /// kubelet's sum of container limits plus RuntimeClass overhead). `None`
    /// when the kubelet gave nothing usable (e.g. a container without a
    /// limit), in which case the shim's own defaults size the VM.
    pub fn from_cri(res: Option<&crate::cri::LinuxContainerResources>) -> Option<Self> {
        let r = res?;
        let out = Self {
            cpu_quota: r.cpu_quota.max(0),
            cpu_period: r.cpu_period.max(0) as u64,
            cpu_shares: r.cpu_shares.max(0) as u64,
            memory_limit: r.memory_limit_in_bytes.max(0),
        };
        (out.cpu_quota != 0 || out.cpu_shares != 0 || out.memory_limit != 0).then_some(out)
    }
}

pub struct ContainerSpecInput<'a> {
    pub container: &'a CriContainer,
    pub sandbox: &'a CriSandbox,
    pub image: &'a ImageDefaults,
    /// Path of the pod's network namespace (CNI already applied).
    pub netns_path: Option<&'a str>,
    /// Extra bind mounts the caller wants (e.g. sandbox `resolv.conf`).
    pub extra_mounts: &'a [CriMount],
    pub devices: &'a [ResolvedDevice],
    /// Pre-loaded seccomp profile for `Localhost` mode (see [`load_seccomp_profile`]).
    pub seccomp: Option<LinuxSeccomp>,
    pub hostname: Option<&'a str>,
    /// The image's user database, to resolve user *names* and passwd gids
    /// (see [`resolve_user`]). `None` allows only all-numeric users.
    pub users: Option<&'a dyn UserDb>,
}

pub struct SandboxSpecInput<'a> {
    pub sandbox: &'a CriSandbox,
    pub netns_path: Option<&'a str>,
    pub resources: Option<SandboxResources>,
    pub hostname: Option<&'a str>,
    /// Pause process argv; defaults to `["/pause"]` when empty.
    pub pause_args: &'a [String],
}

/// Read and parse a localhost seccomp profile (OCI `linux.seccomp` JSON).
pub fn load_seccomp_profile(path: &str) -> std::io::Result<LinuxSeccomp> {
    let data = std::fs::read(path)?;
    serde_json::from_slice(&data).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("seccomp profile {path}: {e}"),
        )
    })
}

/// Serialize a spec as the bytes for `Bundle::write_config`.
pub fn to_config_json(spec: &Spec) -> serde_json::Result<Vec<u8>> {
    serde_json::to_vec_pretty(spec)
}

fn cap(name: &str) -> Result<Capability, SpecError> {
    let n = name.trim().to_ascii_uppercase();
    let n = n.strip_prefix("CAP_").unwrap_or(&n);
    Capability::from_str(n).map_err(|_| SpecError::InvalidCapability(name.to_string()))
}

/// Default (or all, when privileged) set, minus `drop`, then plus `add`: drops
/// apply first so the common `drop: [ALL], add: [X]` yields exactly `{X}`.
/// `ALL` is honoured in both lists.
fn capability_set(
    privileged: bool,
    add: &[String],
    drop: &[String],
) -> Result<BTreeSet<String>, SpecError> {
    let names = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>();
    let mut set = if privileged {
        names(ALL_CAPS)
    } else {
        names(DEFAULT_CAPS)
    };
    let norm = |c: &str| {
        let u = c.trim().to_ascii_uppercase();
        u.strip_prefix("CAP_").unwrap_or(&u).to_string()
    };
    for c in drop {
        if norm(c) == "ALL" {
            set.clear();
        } else {
            cap(c)?;
            set.remove(&norm(c));
        }
    }
    for c in add {
        if norm(c) == "ALL" {
            set = names(ALL_CAPS);
        } else {
            cap(c)?; // validate
            set.insert(norm(c));
        }
    }
    Ok(set)
}

fn caps_to_oci(set: &BTreeSet<String>) -> Result<std::collections::HashSet<Capability>, SpecError> {
    set.iter().map(|c| cap(c)).collect()
}

/// Read access to an image's user database (`/etc/passwd`, `/etc/group`).
/// Methods return `Err` only for unreadable data, never for "not found".
pub trait UserDb {
    /// Primary gid of the first passwd entry with this uid.
    fn passwd_gid_by_uid(&self, uid: u32) -> Result<Option<u32>, String>;
    /// `(uid, gid)` of the passwd entry with this name.
    fn passwd_by_name(&self, name: &str) -> Result<Option<(u32, u32)>, String>;
    /// gid of the group with this name.
    fn gid_by_group_name(&self, name: &str) -> Result<Option<u32>, String>;
}

const MAX_ID: u64 = i32::MAX as u64;

fn numeric_id(s: &str) -> Option<Result<u32, ()>> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(match s.parse::<u64>() {
        Ok(v) if v <= MAX_ID => Ok(v as u32),
        _ => Err(()),
    })
}

/// Resolve a user string to `(uid, gid)` with containerd's rules
/// (`oci.WithUser`):
///
/// - `uid`: gid from the passwd entry with that uid; **gid 0 when there is no
///   entry** (not an error);
/// - `name`: both from the passwd entry; unknown name is an error;
/// - `uid:gid` (both numeric): taken as given, no lookup;
/// - `name:group` / `uid:group` / `name:gid`: the name comes from passwd (uid
///   only), the group from `/etc/group`; unknown names are errors.
///
/// Without a `db`, only the all-numeric forms resolve (a bare uid gets gid 0,
/// as containerd does for an empty passwd).
pub fn resolve_user(user: &str, db: Option<&dyn UserDb>) -> Result<(u32, u32), SpecError> {
    if user.is_empty() {
        return Ok((0, 0));
    }
    let bad = |what: &str| {
        SpecError::UserLookup(format!("invalid USER value {user:?}: {what} out of range"))
    };
    let lookup = |e: String| SpecError::UserLookup(e);
    let need_db = || db.ok_or_else(|| SpecError::UnresolvedUser(user.to_string()));
    let parts: Vec<&str> = user.split(':').collect();
    match parts.as_slice() {
        [u] => match numeric_id(u) {
            Some(Ok(uid)) => {
                let gid = match db {
                    Some(db) => db.passwd_gid_by_uid(uid).map_err(lookup)?.unwrap_or(0),
                    None => 0,
                };
                Ok((uid, gid))
            }
            Some(Err(())) => Err(bad("uid")),
            None => need_db()?
                .passwd_by_name(u)
                .map_err(lookup)?
                .ok_or_else(|| SpecError::UserLookup(format!("user {u:?} not found in image"))),
        },
        [u, g] => {
            let uid = match numeric_id(u) {
                Some(Ok(v)) => v,
                Some(Err(())) => return Err(bad("uid")),
                None => {
                    need_db()?
                        .passwd_by_name(u)
                        .map_err(lookup)?
                        .ok_or_else(|| {
                            SpecError::UserLookup(format!("user {u:?} not found in image"))
                        })?
                        .0
                }
            };
            let gid = match numeric_id(g) {
                Some(Ok(v)) => v,
                Some(Err(())) => return Err(bad("gid")),
                None => need_db()?
                    .gid_by_group_name(g)
                    .map_err(lookup)?
                    .ok_or_else(|| {
                        SpecError::UserLookup(format!("group {g:?} not found in image"))
                    })?,
            };
            Ok((uid, gid))
        }
        _ => Err(SpecError::UserLookup(format!(
            "invalid USER value {user:?}"
        ))),
    }
}

fn merge_env(image: &[(String, String)], cri: &[(String, String)]) -> Vec<String> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (k, v) in image.iter().chain(cri.iter()) {
        match out.iter_mut().find(|(ek, _)| ek == k) {
            Some(slot) => slot.1 = v.clone(),
            None => out.push((k.clone(), v.clone())),
        }
    }
    out.into_iter().map(|(k, v)| format!("{k}={v}")).collect()
}

fn std_mounts() -> Result<Vec<oci_spec::runtime::Mount>, SpecError> {
    let m = |dest: &str, typ: &str, src: &str, opts: &[&str]| {
        MountBuilder::default()
            .destination(PathBuf::from(dest))
            .typ(typ.to_string())
            .source(PathBuf::from(src))
            .options(opts.iter().map(|s| s.to_string()).collect::<Vec<_>>())
            .build()
    };
    Ok(vec![
        m("/proc", "proc", "proc", &["nosuid", "noexec", "nodev"])?,
        m(
            "/dev",
            "tmpfs",
            "tmpfs",
            &["nosuid", "strictatime", "mode=755", "size=65536k"],
        )?,
        m(
            "/dev/pts",
            "devpts",
            "devpts",
            &[
                "nosuid",
                "noexec",
                "newinstance",
                "ptmxmode=0666",
                "mode=0620",
                "gid=5",
            ],
        )?,
        m(
            "/dev/shm",
            "tmpfs",
            "shm",
            &["nosuid", "noexec", "nodev", "mode=1777", "size=65536k"],
        )?,
        m(
            "/dev/mqueue",
            "mqueue",
            "mqueue",
            &["nosuid", "noexec", "nodev"],
        )?,
        m(
            "/sys",
            "sysfs",
            "sysfs",
            &["nosuid", "noexec", "nodev", "ro"],
        )?,
        m(
            "/sys/fs/cgroup",
            "cgroup",
            "cgroup",
            &["nosuid", "noexec", "nodev", "relatime", "ro"],
        )?,
    ])
}

/// CRI bind mount -> OCI mount. Propagation: 0 private, 1 host-to-container
/// (`rslave`), 2 bidirectional (`rshared`). Read-only is `ro`, recursive
/// read-only `rro`.
fn bind_mount(m: &CriMount) -> Result<oci_spec::runtime::Mount, SpecError> {
    let mut opts = vec!["rbind".to_string()];
    opts.push(
        match m.propagation {
            1 => "rslave",
            2 => "rshared",
            _ => "rprivate",
        }
        .to_string(),
    );
    if m.readonly {
        opts.push(if m.recursive_read_only { "rro" } else { "ro" }.to_string());
    }
    Ok(MountBuilder::default()
        .destination(PathBuf::from(&m.container_path))
        .typ("bind".to_string())
        .source(PathBuf::from(&m.host_path))
        .options(opts)
        .build()?)
}

fn namespace(
    t: LinuxNamespaceType,
    path: Option<&str>,
) -> Result<oci_spec::runtime::LinuxNamespace, SpecError> {
    let mut b = LinuxNamespaceBuilder::default().typ(t);
    if let Some(p) = path.filter(|p| !p.is_empty()) {
        b = b.path(PathBuf::from(p));
    }
    Ok(b.build()?)
}

fn device_to_oci(d: &ResolvedDevice) -> Result<LinuxDevice, SpecError> {
    let typ = match d.kind {
        'b' => LinuxDeviceType::B,
        'p' => LinuxDeviceType::P,
        _ => LinuxDeviceType::C,
    };
    Ok(LinuxDeviceBuilder::default()
        .path(PathBuf::from(&d.path))
        .typ(typ)
        .major(d.major)
        .minor(d.minor)
        .file_mode(d.file_mode)
        .uid(d.uid)
        .gid(d.gid)
        .build()?)
}

fn sandbox_annotations(sb: &CriSandbox, container_type: &str) -> HashMap<String, String> {
    let mut a: HashMap<String, String> = HashMap::new();
    a.insert(ANN_CONTAINER_TYPE.into(), container_type.into());
    a.insert(ANN_SANDBOX_ID.into(), sb.id.clone());
    a.insert(ANN_SANDBOX_NAME.into(), sb.name.clone());
    a.insert(ANN_SANDBOX_NAMESPACE.into(), sb.namespace.clone());
    a.insert(ANN_SANDBOX_UID.into(), sb.uid.clone());
    a
}

/// User annotations first (reserved keys filtered out), then ours on top.
fn merge_annotations(
    user: &HashMap<String, String>,
    ours: HashMap<String, String>,
) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = user
        .iter()
        .filter(|(k, _)| !RESERVED_ANNOTATIONS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    out.extend(ours);
    out
}

/// Build the spec for one pod container.
pub fn build_container_spec(inp: &ContainerSpecInput<'_>) -> Result<Spec, SpecError> {
    let c = inp.container;
    let sb = inp.sandbox;

    // argv: CRI command overrides the entrypoint and drops the image CMD
    // (same rule as the native path, `resolve_container_argv`).
    let (entrypoint, cmd) = crate::runtime::resolve_container_argv(
        &c.entrypoint,
        &c.args,
        inp.image.entrypoint.clone(),
        inp.image.cmd.clone(),
    );
    let args: Vec<String> = entrypoint.into_iter().chain(cmd).collect();
    if args.is_empty() {
        return Err(SpecError::EmptyArgs);
    }

    // user: build the same user string containerd's CRI plugin does (CRI
    // runAsUser[:runAsGroup], else runAsUserName, else the image user) and
    // resolve it with containerd's rules.
    let user_str = match (c.run_as_user, c.run_as_group) {
        (Some(u), Some(g)) => format!("{u}:{g}"),
        (Some(u), None) => u.to_string(),
        (None, _) if !c.run_as_username.is_empty() => c.run_as_username.clone(),
        (None, _) => inp.image.user.clone(),
    };
    let (uid, gid) = resolve_user(&user_str, inp.users)?;
    let mut extra_gids: Vec<u32> = Vec::new();
    for g in sb
        .supplemental_groups
        .iter()
        .chain(c.supplemental_groups.iter())
    {
        let g = *g as u32;
        if !extra_gids.contains(&g) {
            extra_gids.push(g);
        }
    }
    let mut user = UserBuilder::default().uid(uid).gid(gid);
    if !extra_gids.is_empty() {
        user = user.additional_gids(extra_gids);
    }

    let cwd = if !c.working_dir.is_empty() {
        c.working_dir.clone()
    } else if !inp.image.working_dir.is_empty() {
        inp.image.working_dir.clone()
    } else {
        "/".to_string()
    };

    let cap_names = capability_set(c.privileged, &c.cap_add, &c.cap_drop)?;
    let caps = caps_to_oci(&cap_names)?;
    let capabilities = LinuxCapabilitiesBuilder::default()
        .bounding(caps.clone())
        .effective(caps.clone())
        .permitted(caps)
        .build()?;

    let mut process = ProcessBuilder::default()
        .terminal(c.tty)
        .user(user.build()?)
        .args(args)
        .env(merge_env(&inp.image.env, &c.envs))
        .cwd(PathBuf::from(cwd))
        .capabilities(capabilities)
        .no_new_privileges(c.no_new_privs && !c.privileged);
    if c.oom_score_adj != i32::MIN {
        process = process.oom_score_adj(c.oom_score_adj);
    }
    if c.apparmor_profile_type == 2 && !c.apparmor_profile_path.is_empty() && !c.privileged {
        process = process.apparmor_profile(c.apparmor_profile_path.clone());
    }
    if !c.selinux_label.is_empty() {
        process = process.selinux_label(c.selinux_label.clone());
    }

    // mounts: standard set, then CRI mounts, then caller extras.
    let mut mounts = std_mounts()?;
    for m in c.mounts.iter().chain(inp.extra_mounts.iter()) {
        mounts.push(bind_mount(m)?);
    }

    // namespaces: the pod netns is joined by path; host-shared modes omit the
    // namespace so the shim does not unshare it.
    let ns = &sb.namespaces;
    let mut namespaces = vec![namespace(LinuxNamespaceType::Mount, None)?];
    if !ns.host_pid() {
        namespaces.push(namespace(LinuxNamespaceType::Pid, None)?);
    }
    if !ns.host_ipc() {
        namespaces.push(namespace(LinuxNamespaceType::Ipc, None)?);
    }
    namespaces.push(namespace(LinuxNamespaceType::Uts, None)?);
    if !ns.host_network() {
        namespaces.push(namespace(LinuxNamespaceType::Network, inp.netns_path)?);
    }

    // resources
    let mut res = LinuxResourcesBuilder::default();
    if c.memory_limit > 0 || c.memory_swap_limit != 0 {
        let mut m = LinuxMemoryBuilder::default();
        if c.memory_limit > 0 {
            m = m.limit(c.memory_limit);
        }
        if c.memory_swap_limit != 0 {
            m = m.swap(c.memory_swap_limit);
        }
        res = res.memory(m.build()?);
    }
    if c.cpu_shares > 0
        || c.cpu_quota > 0
        || c.cpu_period > 0
        || !c.cpuset_cpus.is_empty()
        || !c.cpuset_mems.is_empty()
    {
        let mut cpu = LinuxCpuBuilder::default();
        if c.cpu_shares > 0 {
            cpu = cpu.shares(c.cpu_shares as u64);
        }
        if c.cpu_quota > 0 {
            cpu = cpu.quota(c.cpu_quota);
        }
        if c.cpu_period > 0 {
            cpu = cpu.period(c.cpu_period as u64);
        }
        if !c.cpuset_cpus.is_empty() {
            cpu = cpu.cpus(c.cpuset_cpus.clone());
        }
        if !c.cpuset_mems.is_empty() {
            cpu = cpu.mems(c.cpuset_mems.clone());
        }
        res = res.cpu(cpu.build()?);
    }
    // The kubelet sends `<size>=0` for every hugepage size a pod does not
    // request. In a VM or sandbox kernel that restricts nothing useful, and
    // gVisor's runsc fails the whole container writing it on a cgroup v2 host
    // (`hugetlb.2MB.limit_in_bytes: no such file`, the v1 filename; #623).
    // Pass only real (non-zero) limits.
    let hugepage_limits: Vec<_> = c.hugepage_limits.iter().filter(|(_, l)| *l != 0).collect();
    if !hugepage_limits.is_empty() {
        let mut v = Vec::new();
        for (size, limit) in hugepage_limits {
            v.push(
                LinuxHugepageLimitBuilder::default()
                    .page_size(size.clone())
                    .limit(*limit as i64)
                    .build()?,
            );
        }
        res = res.hugepage_limits(v);
    }
    if !inp.devices.is_empty() {
        let mut rules = Vec::new();
        for d in inp.devices {
            rules.push(
                LinuxDeviceCgroupBuilder::default()
                    .allow(true)
                    .typ(match d.kind {
                        'b' => LinuxDeviceType::B,
                        'p' => LinuxDeviceType::P,
                        _ => LinuxDeviceType::C,
                    })
                    .major(d.major)
                    .minor(d.minor)
                    .access(d.permissions.clone())
                    .build()?,
            );
        }
        res = res.devices(rules);
    }

    let mut linux = LinuxBuilder::default()
        .namespaces(namespaces)
        .resources(res.build()?);
    if !sb.cgroup_parent.is_empty() {
        linux = linux.cgroups_path(PathBuf::from(format!("{}/{}", sb.cgroup_parent, c.id)));
    }
    if c.privileged {
        // The builder's default populates the standard masked/readonly paths;
        // a privileged container must see none of them.
        linux = linux
            .masked_paths(Vec::<String>::new())
            .readonly_paths(Vec::<String>::new());
    } else {
        let masked = if c.masked_paths.is_empty() {
            DEFAULT_MASKED_PATHS.iter().map(|s| s.to_string()).collect()
        } else {
            c.masked_paths.clone()
        };
        let readonly = if c.readonly_paths.is_empty() {
            DEFAULT_READONLY_PATHS
                .iter()
                .map(|s| s.to_string())
                .collect()
        } else {
            c.readonly_paths.clone()
        };
        linux = linux.masked_paths(masked).readonly_paths(readonly);
        // RuntimeDefault (0) and Unconfined (1) emit no profile: the guest /
        // shim default applies; only Localhost (2) carries an explicit one.
        if c.seccomp_profile_type == 2 {
            if let Some(s) = inp.seccomp.clone() {
                linux = linux.seccomp(s);
            }
        }
    }
    if !inp.devices.is_empty() {
        let mut devs = Vec::new();
        for d in inp.devices {
            devs.push(device_to_oci(d)?);
        }
        linux = linux.devices(devs);
    }

    // annotations: reserved CRI keys always win over user annotations.
    let mut ours = sandbox_annotations(sb, "container");
    ours.insert(ANN_CONTAINER_NAME.into(), c.name.clone());
    ours.insert(ANN_IMAGE_NAME.into(), c.image.clone());
    let annotations = merge_annotations(&c.annotations, ours);

    let mut spec = SpecBuilder::default()
        .version(OCI_VERSION.to_string())
        .root(
            RootBuilder::default()
                .path(PathBuf::from("rootfs"))
                .readonly(c.read_only_rootfs)
                .build()?,
        )
        .mounts(mounts)
        .process(process.build()?)
        .linux(linux.build()?)
        .annotations(annotations);
    if let Some(h) = inp.hostname.filter(|h| !h.is_empty()) {
        spec = spec.hostname(h.to_string());
    }
    Ok(spec.build()?)
}

/// Build the spec for the pod sandbox ("pause") container.
pub fn build_sandbox_spec(inp: &SandboxSpecInput<'_>) -> Result<Spec, SpecError> {
    let sb = inp.sandbox;
    let args: Vec<String> = if inp.pause_args.is_empty() {
        vec!["/pause".to_string()]
    } else {
        inp.pause_args.to_vec()
    };

    let ns = &sb.namespaces;
    let mut namespaces = vec![namespace(LinuxNamespaceType::Mount, None)?];
    if !ns.host_pid() {
        namespaces.push(namespace(LinuxNamespaceType::Pid, None)?);
    }
    if !ns.host_ipc() {
        namespaces.push(namespace(LinuxNamespaceType::Ipc, None)?);
    }
    namespaces.push(namespace(LinuxNamespaceType::Uts, None)?);
    if !ns.host_network() {
        namespaces.push(namespace(LinuxNamespaceType::Network, inp.netns_path)?);
    }

    let mut linux = LinuxBuilder::default()
        .namespaces(namespaces)
        .masked_paths(
            DEFAULT_MASKED_PATHS
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
        )
        .readonly_paths(
            DEFAULT_READONLY_PATHS
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
        );
    if !sb.cgroup_parent.is_empty() {
        linux = linux.cgroups_path(PathBuf::from(format!("{}/{}", sb.cgroup_parent, sb.id)));
    }
    // Pod sysctls ride in the sandbox spec so they reach the guest (host-side
    // net sysctls do not cross into a VM's netns).
    if !sb.sysctls.is_empty() {
        linux = linux.sysctl(sb.sysctls.clone());
    }

    let caps = caps_to_oci(&capability_set(false, &[], &[])?)?;
    let capabilities = LinuxCapabilitiesBuilder::default()
        .bounding(caps.clone())
        .effective(caps.clone())
        .permitted(caps)
        .build()?;
    let process = ProcessBuilder::default()
        .user(UserBuilder::default().uid(0u32).gid(0u32).build()?)
        .args(args)
        .env(vec![
            "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
        ])
        .cwd(PathBuf::from("/"))
        .capabilities(capabilities)
        .no_new_privileges(true)
        .build()?;

    let mut ours = sandbox_annotations(sb, "sandbox");
    if let Some(r) = inp.resources {
        if r.cpu_quota != 0 {
            ours.insert(ANN_SANDBOX_CPU_QUOTA.into(), r.cpu_quota.to_string());
        }
        if r.cpu_period != 0 {
            ours.insert(ANN_SANDBOX_CPU_PERIOD.into(), r.cpu_period.to_string());
        }
        if r.cpu_shares != 0 {
            ours.insert(ANN_SANDBOX_CPU_SHARES.into(), r.cpu_shares.to_string());
        }
        if r.memory_limit != 0 {
            ours.insert(ANN_SANDBOX_MEMORY.into(), r.memory_limit.to_string());
        }
    }
    let annotations = merge_annotations(&sb.annotations, ours);

    let mut spec = SpecBuilder::default()
        .version(OCI_VERSION.to_string())
        .root(
            RootBuilder::default()
                .path(PathBuf::from("rootfs"))
                .readonly(true)
                .build()?,
        )
        .mounts(std_mounts()?)
        .process(process)
        .linux(linux.build()?)
        .annotations(annotations);
    if let Some(h) = inp.hostname.filter(|h| !h.is_empty()) {
        spec = spec.hostname(h.to_string());
    }
    Ok(spec.build()?)
}

#[cfg(test)]
mod tests;
