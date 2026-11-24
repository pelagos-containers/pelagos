use containerd_shim_protos::protobuf::Message;

use super::*;

fn any(url: &str, bytes: Vec<u8>) -> Any {
    let mut a = Any::new();
    a.type_url = url.into();
    a.value = bytes;
    a
}

pub(crate) fn v1_payload(cpu_total: u64, usage: u64, inactive: u64) -> Any {
    let mut m = V1::new();
    m.cpu
        .mut_or_insert_default()
        .usage
        .mut_or_insert_default()
        .total = cpu_total;
    let mem = m.memory.mut_or_insert_default();
    mem.usage.mut_or_insert_default().usage = usage;
    mem.total_inactive_file = inactive;
    any(
        "types.containerd.io/io.containerd.cgroups.v1.Metrics",
        m.write_to_bytes().unwrap(),
    )
}

#[test]
fn v1_cpu_and_working_set() {
    let u = decode_usage(&v1_payload(5_000_000, 1000, 400)).unwrap();
    assert_eq!(
        u,
        TaskUsage {
            cpu_nanos: 5_000_000,
            mem_usage: 1000,
            mem_working_set: 600
        }
    );
    // inactive > usage must not underflow.
    assert_eq!(
        decode_usage(&v1_payload(0, 10, 99))
            .unwrap()
            .mem_working_set,
        0
    );
}

#[test]
fn v2_usec_converted_to_nanos() {
    let mut m = V2::new();
    m.cpu.mut_or_insert_default().usage_usec = 7;
    let mem = m.memory.mut_or_insert_default();
    mem.usage = 2000;
    mem.inactive_file = 500;
    let u = decode_usage(&any(
        "types.containerd.io/io.containerd.cgroups.v2.Metrics",
        m.write_to_bytes().unwrap(),
    ))
    .unwrap();
    assert_eq!(
        u,
        TaskUsage {
            cpu_nanos: 7000,
            mem_usage: 2000,
            mem_working_set: 1500
        }
    );
}

#[test]
fn empty_metrics_are_zero_and_junk_is_an_error() {
    let u = decode_usage(&any("x/io.containerd.cgroups.v1.Metrics", vec![])).unwrap();
    assert_eq!(u, TaskUsage::default());
    assert!(matches!(
        decode_usage(&any("x/other.Type", vec![])),
        Err(StatsError::Unsupported(_))
    ));
    assert!(matches!(
        decode_usage(&any(
            "x/io.containerd.cgroups.v1.Metrics",
            vec![0xff, 0xff, 0xff]
        )),
        Err(StatsError::Malformed(..))
    ));
}
