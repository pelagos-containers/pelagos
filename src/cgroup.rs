//! Cgroups v2 resource management for containers.
//!
//! This module wraps the `cgroups-rs` crate to provide an ergonomic interface for
//! creating, configuring, and tearing down cgroups for containerized processes.
//!
//! The cgroup lifecycle is managed entirely from the **parent process**:
//! 1. [`CgroupConfig`] is built via [`crate::container::Command`] builder methods.
//! 2. After fork+exec, the parent creates the cgroup and adds the child PID.
//! 3. After the child exits, the parent deletes the cgroup.
//!
//! # Naming
//!
//! Each container's cgroup is named `pelagos-{child_pid}` to guarantee uniqueness
//! across concurrent containers.

use cgroups_rs::{
    fs::MaxValue,
    fs::{
        cgroup_builder::CgroupBuilder, cpu::CpuController, cpuset::CpuSetController, hierarchies,
        memory::MemController, net_cls::NetClsController, net_prio::NetPrioController,
        pid::PidController, Cgroup,
    },
    CgroupPid,
};
use std::io;

/// Resource limits to apply via cgroups v2.
///
/// All fields are optional — set only what you need. Unset fields use the
/// kernel's default (no limit). Coexists with rlimits without conflict.
///
/// # Examples
///
/// ```ignore
/// Command::new("/bin/sh")
///     .with_cgroup_memory(256 * 1024 * 1024)   // 256 MB
///     .with_cgroup_cpu_shares(512)              // half default weight
///     .with_cgroup_pids_limit(64)               // max 64 processes
///     .spawn()?;
/// ```
#[derive(Debug, Clone, Default)]
pub struct CgroupConfig {
    /// Memory hard limit in bytes (`memory.max`).
    /// The process is OOM-killed if it exceeds this limit.
    pub memory_limit: Option<i64>,

    /// Memory + swap combined limit in bytes.
    /// Maps to `memory.swap.max` on v2, `memory.memsw.limit_in_bytes` on v1.
    /// -1 means unlimited swap on top of the memory limit.
    pub memory_swap: Option<i64>,

    /// Soft memory limit / low-water mark in bytes.
    /// Maps to `memory.low` on v2, `memory.soft_limit_in_bytes` on v1.
    pub memory_reservation: Option<i64>,

    /// Swappiness hint (0–100) for the memory controller (v1 only; silently ignored on v2).
    pub memory_swappiness: Option<u64>,

    /// CPU weight (1–10000). Maps to `cpu.weight` in v2, `cpu.shares` in v1.
    /// Default kernel weight is 100. Higher values get proportionally more CPU.
    pub cpu_shares: Option<u64>,

    /// CPU quota: `(quota_microseconds, period_microseconds)`.
    /// Example: `(50_000, 100_000)` = 50% of one CPU core per 100 ms period.
    pub cpu_quota: Option<(i64, u64)>,

    /// CPUs this cgroup may use (cpuset string, e.g. `"0-3,6"`).
    /// Maps to `cpuset.cpus`.
    pub cpuset_cpus: Option<String>,

    /// Memory nodes this cgroup may use (cpuset string, e.g. `"0-1"`).
    /// Maps to `cpuset.mems`.
    pub cpuset_mems: Option<String>,

    /// Maximum number of live processes/threads in the cgroup (`pids.max`).
    pub pids_limit: Option<u64>,

    /// Block I/O weight (10–1000). Maps to `io.weight` on v2, `blkio.weight` on v1.
    pub blkio_weight: Option<u16>,

    /// Per-device block I/O throttle rules (major, minor, bytes_per_sec).
    pub blkio_throttle_read_bps: Vec<(u64, u64, u64)>,
    pub blkio_throttle_write_bps: Vec<(u64, u64, u64)>,
    pub blkio_throttle_read_iops: Vec<(u64, u64, u64)>,
    pub blkio_throttle_write_iops: Vec<(u64, u64, u64)>,

    /// Device cgroup allow/deny rules.
    /// Each entry: (allow, type_char, major, minor, access_string).
    /// Silently ignored on cgroupv2 (requires eBPF; not implemented).
    pub device_rules: Vec<CgroupDeviceRule>,

    /// net_cls classid (v1 only; silently ignored on v2).
    pub net_classid: Option<u64>,

    /// net_prio interface priority map (v1 only; silently ignored on v2).
    pub net_priorities: Vec<(String, u64)>,

    /// Explicit cgroup path from OCI `linux.cgroupsPath`.
    /// If set, used as-is as the cgroup name/path; otherwise defaults to `pelagos-{pid}`.
    pub path: Option<String>,

    /// HugePage limits: `(page_size_string, limit_in_bytes)`.
    /// Example: `("2MB", 1073741824)` limits 2 MB hugepages to 1 GiB.
    /// Written to `hugetlb.<size>.limit_in_bytes` on cgroupv1 or
    /// `hugetlb.<size>.max` on cgroupv2; skipped if the file doesn't exist.
    pub hugepage_limits: Vec<(String, u64)>,
}

/// A single device cgroup allow/deny rule.
#[derive(Debug, Clone)]
pub struct CgroupDeviceRule {
    pub allow: bool,
    /// Device type: 'c' (char), 'b' (block), 'a' (all).
    pub kind: char,
    /// -1 means wildcard.
    pub major: i64,
    /// -1 means wildcard.
    pub minor: i64,
    /// Access string: combination of 'r', 'w', 'm'.
    pub access: String,
}

/// Create a cgroup, apply configured limits, and return the handle and
/// `cgroup.procs` path **without** adding any process.
///
/// The caller passes `cgroup_procs_path` to the container's `pre_exec` hook so
/// the container process can add its own PID before doing any memory-intensive
/// work, eliminating the parent-side race in [`setup_cgroup`].
///
/// If `cfg.path` is set it is used as the cgroup name (supporting nested paths
/// such as `kubepods/besteffort/pod<uid>/<container-id>`); otherwise `name` is
/// used and must be unique — use [`cgroup_unique_name`] to generate one before
/// fork when the child PID is not yet known.
///
/// Verifies the resulting `cgroup.procs` file exists before returning. On
/// hybrid v1/v2 cgroup hierarchies the underlying `cgroups-rs` library
/// silently swallows controller-enable failures (`enable_controllers` ignores
/// write errors), so `mkdir` can succeed on a tmpfs entry that the kernel
/// never populates with the v2 control files. Without this check, the failure
/// surfaces only later as a bare `ENOENT` on `cgroup.procs` write inside
/// `pre_exec`, where `std::process::Command`'s wire protocol strips the
/// context and the user sees only `Failed to spawn process: No such file or
/// directory (os error 2)`.
pub fn create_cgroup_no_task(cfg: &CgroupConfig, name: &str) -> io::Result<(Cgroup, String)> {
    // Honor an explicit cgroup path (e.g. kubepods hierarchy from CRI).
    // When the path contains slashes we must ensure the parent directory exists
    // before cgroups-rs tries to create the leaf; kubelet creates the pod-level
    // parent but not the container-level leaf.
    let effective_name: &str = cfg.path.as_deref().unwrap_or(name);
    let cg_dir = std::path::Path::new("/sys/fs/cgroup").join(effective_name);
    if let Some(parent) = cg_dir.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            io::Error::other(format!("cgroup parent dir '{}': {}", parent.display(), e))
        })?;
    }
    let cg = build_cgroup(cfg, effective_name)?;
    let procs_path = format!("/sys/fs/cgroup/{}/cgroup.procs", effective_name);
    if !std::path::Path::new(&procs_path).exists() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "cgroup limits requested but {} does not exist after creation \
                 — pelagos requires the kernel cgroup v2 unified hierarchy at \
                 /sys/fs/cgroup; hybrid v1/v2 setups are not supported",
                procs_path
            ),
        ));
    }
    Ok((cg, procs_path))
}

/// Generate a unique cgroup name before fork (when the child PID is not yet known).
///
/// Uses the current PID plus an atomic counter so names never collide between
/// concurrent spawns in the same process.
pub fn cgroup_unique_name() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("pelagos-{}-{}", unsafe { libc::getpid() }, n)
}

/// Internal helper: build a [`Cgroup`] with all configured limits but without
/// adding any process.  Shared by [`setup_cgroup`] and [`create_cgroup_no_task`].
fn build_cgroup(cfg: &CgroupConfig, name: &str) -> io::Result<Cgroup> {
    let hier = hierarchies::auto();

    let mut builder = CgroupBuilder::new(name);

    // --- Memory ---
    if cfg.memory_limit.is_some()
        || cfg.memory_swap.is_some()
        || cfg.memory_reservation.is_some()
        || cfg.memory_swappiness.is_some()
    {
        let mut mb = builder.memory();
        if let Some(limit) = cfg.memory_limit {
            mb = mb.memory_hard_limit(limit);
        }
        if let Some(swap) = cfg.memory_swap {
            mb = mb.memory_swap_limit(swap);
        }
        if let Some(res) = cfg.memory_reservation {
            mb = mb.memory_soft_limit(res);
        }
        if let Some(swp) = cfg.memory_swappiness {
            mb = mb.swappiness(swp);
        }
        builder = mb.done();
    }

    // --- CPU ---
    let has_cpu = cfg.cpu_shares.is_some() || cfg.cpu_quota.is_some();
    if has_cpu {
        let mut cb = builder.cpu();
        if let Some(shares) = cfg.cpu_shares {
            cb = cb.shares(shares);
        }
        if let Some((quota, period)) = cfg.cpu_quota {
            cb = cb.quota(quota).period(period);
        }
        builder = cb.done();
    }

    // --- PIDs ---
    if let Some(max_pids) = cfg.pids_limit {
        builder = builder
            .pid()
            .maximum_number_of_processes(MaxValue::Value(max_pids as i64))
            .done();
    }

    // --- Block I/O ---
    let has_blkio = cfg.blkio_weight.is_some()
        || !cfg.blkio_throttle_read_bps.is_empty()
        || !cfg.blkio_throttle_write_bps.is_empty()
        || !cfg.blkio_throttle_read_iops.is_empty()
        || !cfg.blkio_throttle_write_iops.is_empty();
    if has_blkio {
        let mut bb = builder.blkio();
        if let Some(w) = cfg.blkio_weight {
            bb = bb.weight(w);
        }
        if !cfg.blkio_throttle_read_bps.is_empty() {
            bb = bb.throttle_bps();
            for &(major, minor, rate) in &cfg.blkio_throttle_read_bps {
                bb = bb.read(major, minor, rate);
            }
        }
        if !cfg.blkio_throttle_write_bps.is_empty() {
            bb = bb.throttle_bps();
            for &(major, minor, rate) in &cfg.blkio_throttle_write_bps {
                bb = bb.write(major, minor, rate);
            }
        }
        if !cfg.blkio_throttle_read_iops.is_empty() {
            bb = bb.throttle_iops();
            for &(major, minor, rate) in &cfg.blkio_throttle_read_iops {
                bb = bb.read(major, minor, rate);
            }
        }
        if !cfg.blkio_throttle_write_iops.is_empty() {
            bb = bb.throttle_iops();
            for &(major, minor, rate) in &cfg.blkio_throttle_write_iops {
                bb = bb.write(major, minor, rate);
            }
        }
        builder = bb.done();
    }

    // --- Network (v1 only; silently skip on v2-only systems) ---
    let has_net = cfg.net_classid.is_some() || !cfg.net_priorities.is_empty();
    if has_net {
        let mut nb = builder.network();
        if let Some(class_id) = cfg.net_classid {
            nb = nb.class_id(class_id);
        }
        for (name, prio) in &cfg.net_priorities {
            nb = nb.priority(name.clone(), *prio);
        }
        builder = nb.done();
    }

    // --- Device cgroup (v1 only; silently skip on v2-only systems) ---
    if !cfg.device_rules.is_empty() {
        use cgroups_rs::fs::devices::{DevicePermissions, DeviceType};
        let mut db = builder.devices();
        for rule in &cfg.device_rules {
            let devtype = match rule.kind {
                'b' => DeviceType::Block,
                'c' => DeviceType::Char,
                _ => DeviceType::All,
            };
            let access = DevicePermissions::from_str(&rule.access)
                .unwrap_or_else(|_| DevicePermissions::all());
            db = db.device(rule.major, rule.minor, devtype, rule.allow, access);
        }
        builder = db.done();
    }

    let cg = builder
        .build(hier)
        .map_err(|e| io::Error::other(format!("cgroup create '{}': {}", name, e)))?;

    // --- CpuSet (must be applied via controller_of after cgroup is created) ---
    if cfg.cpuset_cpus.is_some() || cfg.cpuset_mems.is_some() {
        if let Some(cs) = cg.controller_of::<CpuSetController>() {
            if let Some(ref cpus) = cfg.cpuset_cpus {
                if let Err(e) = cs.set_cpus(cpus) {
                    log::warn!("cgroup cpuset.cpus={} failed (non-fatal): {}", cpus, e);
                }
            }
            if let Some(ref mems) = cfg.cpuset_mems {
                if let Err(e) = cs.set_mems(mems) {
                    log::warn!("cgroup cpuset.mems={} failed (non-fatal): {}", mems, e);
                }
            }
        } else {
            log::debug!("cpuset controller unavailable; cpus/mems not applied");
        }
    }

    // --- HugePage limits (direct filesystem write; cgroups-rs has no controller) ---
    if !cfg.hugepage_limits.is_empty() {
        for (page_size, limit) in &cfg.hugepage_limits {
            // cgroupv2: hugetlb.<size>.max; cgroupv1: hugetlb.<size>.limit_in_bytes
            let v2_path = format!("/sys/fs/cgroup/{}/hugetlb.{}.max", name, page_size);
            let v1_path = format!(
                "/sys/fs/cgroup/{}/hugetlb.{}.limit_in_bytes",
                name, page_size
            );
            if std::path::Path::new(&v2_path).exists() {
                if let Err(e) = std::fs::write(&v2_path, format!("{}\n", limit)) {
                    log::warn!("hugetlb.{}.max write failed (non-fatal): {}", page_size, e);
                }
            } else if std::path::Path::new(&v1_path).exists() {
                if let Err(e) = std::fs::write(&v1_path, format!("{}\n", limit)) {
                    log::warn!(
                        "hugetlb.{}.limit_in_bytes write failed (non-fatal): {}",
                        page_size,
                        e
                    );
                }
            } else {
                log::debug!(
                    "hugetlb.{}.max not present; hugepage limit not applied",
                    page_size
                );
            }
        }
    }

    // --- Net class / prio validation (v1 only; log if unavailable) ---
    if cfg.net_classid.is_some() && cg.controller_of::<NetClsController>().is_none() {
        log::debug!("net_cls controller unavailable (v2-only system); classid not applied");
    }
    if !cfg.net_priorities.is_empty() && cg.controller_of::<NetPrioController>().is_none() {
        log::debug!("net_prio controller unavailable (v2-only system); priorities not applied");
    }

    // Enable OOM group killing when a memory limit is configured: the OOM killer
    // will send SIGKILL to ALL processes in the cgroup, not just the one that
    // triggered the OOM event.  This matches Docker's behaviour and ensures the
    // limit is enforced even when memory-intensive work happens in a child
    // process of a longer-lived shell.
    if cfg.memory_limit.is_some() {
        let oom_group_path = format!("/sys/fs/cgroup/{}/memory.oom.group", name);
        if let Err(e) = std::fs::write(&oom_group_path, b"1\n") {
            log::debug!("memory.oom.group not available (non-fatal): {}", e);
        }
    }

    Ok(cg)
}

/// Create a cgroup named `pelagos-{child_pid}`, apply configured limits, and add
/// the child process to it.
///
/// Returns the live [`Cgroup`] handle — the caller must call [`teardown_cgroup`]
/// after the child exits.
///
/// # Errors
///
/// Returns an error if the cgroup cannot be created (e.g. missing permissions,
/// cgroup fs not mounted) or if the PID cannot be added.
pub fn setup_cgroup(cfg: &CgroupConfig, child_pid: u32) -> io::Result<Cgroup> {
    let name = cfg
        .path
        .clone()
        .unwrap_or_else(|| format!("pelagos-{}", child_pid));
    let cg = build_cgroup(cfg, &name)?;
    cg.add_task_by_tgid(CgroupPid::from(child_pid as u64))
        .map_err(|e| io::Error::other(format!("cgroup add_task pid={}: {}", child_pid, e)))?;
    Ok(cg)
}

/// Delete a cgroup after the container process has exited.
///
/// Errors are logged at `warn` level but not propagated — cleanup failures
/// are non-fatal since the kernel will reclaim resources automatically once
/// all tasks have exited.
pub fn teardown_cgroup(cg: Cgroup) {
    if let Err(e) = cg.delete() {
        log::warn!("cgroup delete failed (non-fatal): {}", e);
    }
}

/// Resource usage statistics read from a container's live cgroup.
#[derive(Debug, Clone, Default)]
pub struct ResourceStats {
    /// Current memory usage in bytes (`memory.current`).
    pub memory_current_bytes: u64,
    /// Memory hard limit in bytes (`memory.max`); `None` means unlimited.
    pub memory_limit_bytes: Option<u64>,
    /// Total CPU time consumed in nanoseconds (from `cpu.stat usage_usec * 1000`).
    pub cpu_usage_ns: u64,
    /// Current number of live processes/threads (`pids.current`).
    pub pids_current: u64,
}

/// Open an existing cgroup by name without creating or modifying it.
///
/// Returns `None` if the cgroup directory does not exist (container has exited).
pub fn open_cgroup(name: &str) -> Option<Cgroup> {
    let path = format!("/sys/fs/cgroup/{}", name);
    if !std::path::Path::new(&path).exists() {
        return None;
    }
    Some(Cgroup::load(hierarchies::auto(), name))
}

/// Read current resource usage from a container's cgroup.
///
/// Controllers that are unavailable (e.g. not enabled in the hierarchy) return 0
/// for their respective fields rather than failing.
pub fn read_stats(cg: &Cgroup) -> io::Result<ResourceStats> {
    let mut stats = ResourceStats::default();

    // Memory: usage_in_bytes from memory.current (v2) or memory.usage_in_bytes (v1)
    if let Some(mem_ctrl) = cg.controller_of::<MemController>() {
        let ms = mem_ctrl.memory_stat();
        stats.memory_current_bytes = ms.usage_in_bytes;
        // limit_in_bytes == -1 means "max" (unlimited)
        if ms.limit_in_bytes > 0 {
            stats.memory_limit_bytes = Some(ms.limit_in_bytes as u64);
        }
    }

    // CPU: parse "usage_usec N" from the raw cpu.stat string
    if let Some(cpu_ctrl) = cg.controller_of::<CpuController>() {
        let raw = cpu_ctrl.cpu().stat;
        for line in raw.lines() {
            if let Some(rest) = line.strip_prefix("usage_usec ") {
                if let Ok(usec) = rest.trim().parse::<u64>() {
                    stats.cpu_usage_ns = usec.saturating_mul(1000);
                }
                break;
            }
        }
    }

    // PIDs: pids.current
    if let Some(pid_ctrl) = cg.controller_of::<PidController>() {
        if let Ok(current) = pid_ctrl.get_pid_current() {
            stats.pids_current = current;
        }
    }

    Ok(stats)
}

/// SIGKILL **every** process in a container's cgroup subtree (cgroup v2).
///
/// The per-container stop/rm paths signal only the single recorded `state.pid`,
/// which misses forked or `setsid`'d descendants and processes reparented to init —
/// leaving orphans that keep holding resources (e.g. a listening port), the root
/// cause of the orphaned-hostNetwork-process bug (#412). Killing the whole cgroup
/// catches them all regardless of reparenting, the way runc/containerd do.
///
/// `cgroup_name` is the path **relative to `/sys/fs/cgroup`** (as stored in the
/// container state's `cgroup_name`). No-op if the cgroup is absent/unknown. Prefers
/// the atomic `cgroup.kill` (kernel ≥ 5.14); otherwise walks `cgroup.procs` across
/// the subtree until it drains.
pub fn kill_cgroup(cgroup_name: &str) {
    let dir = std::path::Path::new("/sys/fs/cgroup").join(cgroup_name.trim_start_matches('/'));
    if !dir.is_dir() {
        return;
    }
    // Atomic subtree kill (cgroup v2, kernel >= 5.14).
    let kill_file = dir.join("cgroup.kill");
    if kill_file.exists() && std::fs::write(&kill_file, "1").is_ok() {
        return;
    }
    // Fallback: SIGKILL every pid in cgroup.procs across the subtree, retrying a few
    // times since a shell can spawn a straggler while it is being torn down.
    for _ in 0..20 {
        let mut killed_any = false;
        kill_cgroup_subtree(&dir, &mut killed_any);
        if !killed_any {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn kill_cgroup_subtree(dir: &std::path::Path, killed_any: &mut bool) {
    if let Ok(contents) = std::fs::read_to_string(dir.join("cgroup.procs")) {
        for line in contents.lines() {
            if let Ok(pid) = line.trim().parse::<i32>() {
                // Never signal pid <= 1 (init / group-sentinel guard).
                if pid > 1 {
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                    *killed_any = true;
                }
            }
        }
    }
    if let Ok(rd) = std::fs::read_dir(dir) {
        for ent in rd.flatten() {
            let p = ent.path();
            if p.is_dir() {
                kill_cgroup_subtree(&p, killed_any);
            }
        }
    }
}
