//! Decode `Task.Stats` payloads (epic #574, #604).
//!
//! A shim answers `Stats` with a protobuf `Any` holding containerd's cgroups
//! `Metrics`: v1 (`io.containerd.cgroups.v1.Metrics`, what Kata's guest
//! reports) or v2 (`io.containerd.cgroups.v2.Metrics`). Only the CRI-relevant
//! numbers are extracted.

use containerd_shim_protos::cgroups::metrics::Metrics as V1;
use containerd_shim_protos::cgroups_v2::metrics::Metrics as V2;
use containerd_shim_protos::protobuf::well_known_types::any::Any;
use containerd_shim_protos::protobuf::Message;

/// CPU and memory usage of one task.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TaskUsage {
    pub cpu_nanos: u64,
    pub mem_usage: u64,
    /// `usage - inactive_file`, the figure the kubelet's eviction uses.
    pub mem_working_set: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum StatsError {
    #[error("unsupported stats type {0:?}")]
    Unsupported(String),
    #[error("malformed {0} payload: {1}")]
    Malformed(&'static str, String),
}

pub fn decode_usage(any: &Any) -> Result<TaskUsage, StatsError> {
    let url = any.type_url.as_str();
    if url.ends_with("cgroups.v1.Metrics") {
        let m = V1::parse_from_bytes(&any.value)
            .map_err(|e| StatsError::Malformed("cgroups v1", e.to_string()))?;
        let usage = m.memory.usage.usage;
        let inactive = m.memory.total_inactive_file;
        Ok(TaskUsage {
            cpu_nanos: m.cpu.usage.total,
            mem_usage: usage,
            mem_working_set: usage.saturating_sub(inactive),
        })
    } else if url.ends_with("cgroups.v2.Metrics") {
        let m = V2::parse_from_bytes(&any.value)
            .map_err(|e| StatsError::Malformed("cgroups v2", e.to_string()))?;
        let usage = m.memory.usage;
        Ok(TaskUsage {
            cpu_nanos: m.cpu.usage_usec.saturating_mul(1000),
            mem_usage: usage,
            mem_working_set: usage.saturating_sub(m.memory.inactive_file),
        })
    } else {
        Err(StatsError::Unsupported(url.to_string()))
    }
}

#[cfg(test)]
pub(crate) mod tests;
