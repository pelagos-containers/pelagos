//! `pelagos stats` — live container resource usage (CPU%, memory, PIDs).

use clap::Parser;
use std::error::Error;
use std::time::{Duration, Instant};

use super::{check_liveness, list_containers, ContainerStatus};
use pelagos::cgroup::{open_cgroup, read_stats, ResourceStats};

#[derive(Parser, Debug)]
pub struct StatsArgs {
    /// Print a single snapshot instead of streaming
    #[clap(long = "no-stream")]
    pub no_stream: bool,

    /// Container names to show (default: all running)
    pub names: Vec<String>,
}

/// Number of online processors, for CPU% normalisation.
fn num_online_cpus() -> u64 {
    let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    if n <= 0 {
        1
    } else {
        n as u64
    }
}

/// Format bytes as a human-readable string (B / KiB / MiB / GiB).
fn fmt_bytes(b: u64) -> String {
    if b >= 1 << 30 {
        format!("{:.2} GiB", b as f64 / (1u64 << 30) as f64)
    } else if b >= 1 << 20 {
        format!("{:.2} MiB", b as f64 / (1u64 << 20) as f64)
    } else if b >= 1 << 10 {
        format!("{:.2} KiB", b as f64 / (1u64 << 10) as f64)
    } else {
        format!("{} B", b)
    }
}

struct Row {
    name: String,
    cpu_pct: Option<f64>,
    mem_usage: Option<u64>,
    mem_limit: Option<u64>,
    pids: Option<u64>,
}

fn print_table(rows: &[Row]) {
    println!(
        "{:<24} {:>8}  {:<28} {:>6}  {:>5}",
        "NAME", "CPU %", "MEM USAGE / LIMIT", "MEM %", "PIDS"
    );
    for r in rows {
        let cpu_str = r
            .cpu_pct
            .map(|p| format!("{:.2}%", p))
            .unwrap_or_else(|| "--".to_string());
        let mem_usage_str = r
            .mem_usage
            .map(fmt_bytes)
            .unwrap_or_else(|| "--".to_string());
        let mem_limit_str = r
            .mem_limit
            .map(fmt_bytes)
            .unwrap_or_else(|| "--".to_string());
        let mem_str = format!("{} / {}", mem_usage_str, mem_limit_str);
        let mem_pct = match (r.mem_usage, r.mem_limit) {
            (Some(u), Some(lim)) if lim > 0 => format!("{:.2}%", u as f64 / lim as f64 * 100.0),
            _ => "--".to_string(),
        };
        let pids_str = r
            .pids
            .map(|p| p.to_string())
            .unwrap_or_else(|| "--".to_string());
        println!(
            "{:<24} {:>8}  {:<28} {:>6}  {:>5}",
            r.name, cpu_str, mem_str, mem_pct, pids_str
        );
    }
}

pub fn cmd_stats(args: StatsArgs) -> Result<(), Box<dyn Error>> {
    let nprocs = num_online_cpus();
    let sleep_ms = if args.no_stream { 500 } else { 1000 };

    loop {
        let containers: Vec<_> = list_containers()
            .into_iter()
            .filter(|s| s.status == ContainerStatus::Running && check_liveness(s.pid))
            .filter(|s| args.names.is_empty() || args.names.contains(&s.name))
            .collect();

        // First sample (None if no cgroup_name)
        let t0 = Instant::now();
        let samples0: Vec<Option<ResourceStats>> = containers
            .iter()
            .map(|s| {
                s.cgroup_name
                    .as_deref()
                    .and_then(open_cgroup)
                    .and_then(|cg| read_stats(&cg).ok())
            })
            .collect();

        std::thread::sleep(Duration::from_millis(sleep_ms));
        let wall_us = t0.elapsed().as_micros() as f64;

        // Second sample + build rows — every running container gets a row
        let rows: Vec<Row> = containers
            .iter()
            .zip(samples0.iter())
            .map(|(s, s0)| {
                let s1 = s
                    .cgroup_name
                    .as_deref()
                    .and_then(open_cgroup)
                    .and_then(|cg| read_stats(&cg).ok());

                let cpu_pct = match (s0, &s1) {
                    (Some(s0), Some(s1)) => {
                        let delta_us = s1
                            .cpu_usage_ns
                            .saturating_sub(s0.cpu_usage_ns)
                            .saturating_div(1000) as f64;
                        Some(delta_us / (wall_us * nprocs as f64) * 100.0)
                    }
                    _ => None,
                };

                Row {
                    name: s.name.clone(),
                    cpu_pct,
                    mem_usage: s1.as_ref().map(|s| s.memory_current_bytes),
                    mem_limit: s1.as_ref().and_then(|s| s.memory_limit_bytes),
                    pids: s1.as_ref().map(|s| s.pids_current),
                }
            })
            .collect();

        if !args.no_stream {
            // Clear screen + move cursor to top-left
            print!("\x1b[2J\x1b[H");
        }

        if rows.is_empty() {
            println!("No running containers.");
        } else {
            print_table(&rows);
        }

        if args.no_stream {
            break;
        }
    }
    Ok(())
}
