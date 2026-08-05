fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_server(true)
        .build_client(false)
        .compile_protos(&["proto/api.proto"], &["proto"])?;

    emit_pelagos_release_version();

    Ok(())
}

/// `pelagos-cri`'s own crate version never changes (`0.1.0` in
/// `pelagos-cri/Cargo.toml`) — it's a workspace member, not what gets
/// released. Read the workspace root's `[package].version` instead, so
/// metrics/logging can report the actual pelagos release version. See #499
/// (k3s-agent flagged the build_info gauge showing "0.1.0" as confusing).
fn emit_pelagos_release_version() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let root_manifest = std::path::Path::new(&manifest_dir).join("../Cargo.toml");
    println!("cargo:rerun-if-changed={}", root_manifest.display());

    let contents = std::fs::read_to_string(&root_manifest)
        .unwrap_or_else(|e| panic!("reading {}: {e}", root_manifest.display()));
    let version = contents
        .lines()
        .find_map(|line| {
            let line = line.trim();
            line.strip_prefix("version")
                .map(str::trim_start)
                .and_then(|rest| rest.strip_prefix('='))
                .map(str::trim)
                .and_then(|rest| rest.strip_prefix('"'))
                .and_then(|rest| rest.strip_suffix('"'))
        })
        .unwrap_or_else(|| {
            panic!(
                "no version = \"...\" line found in {}",
                root_manifest.display()
            )
        });

    println!("cargo:rustc-env=PELAGOS_RELEASE_VERSION={version}");
}
