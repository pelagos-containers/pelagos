//! Wasm/WASI runtime integration.
//!
//! Detects WebAssembly binaries by magic bytes (`\0asm`) and dispatches
//! execution to an installed runtime (wasmtime or WasmEdge) via subprocess.
//!
//! # Example
//!
//! ```no_run
//! use pelagos::wasm::{WasmRuntime, WasiConfig, spawn_wasm};
//! use std::path::Path;
//! use std::process;
//!
//! let wasi = WasiConfig {
//!     runtime: WasmRuntime::Auto,
//!     env: vec![("KEY".into(), "val".into())],
//!     preopened_dirs: vec![("/data".into(), "/data".into())],
//! };
//! let child = spawn_wasm(
//!     Path::new("/app/module.wasm"),
//!     &[],
//!     &wasi,
//!     process::Stdio::inherit(),
//!     process::Stdio::inherit(),
//!     process::Stdio::inherit(),
//! ).expect("spawn wasm");
//! ```

use std::io;
use std::path::{Path, PathBuf};

/// WebAssembly module magic bytes: `\0asm` (0x00 0x61 0x73 0x6D).
const WASM_MAGIC: [u8; 4] = [0x00, 0x61, 0x73, 0x6D];

/// OCI layer media types that carry a raw WebAssembly module blob (not a tarball).
pub const WASM_LAYER_MEDIA_TYPES: &[&str] = &[
    "application/vnd.bytecodealliance.wasm.component.layer.v0+wasm",
    "application/vnd.wasm.content.layer.v1+wasm",
    "application/wasm",
];

/// Returns `true` if `media_type` is a recognised Wasm OCI layer type.
pub fn is_wasm_media_type(media_type: &str) -> bool {
    WASM_LAYER_MEDIA_TYPES.contains(&media_type)
}

/// Preferred Wasm runtime backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WasmRuntime {
    /// Use wasmtime (Bytecode Alliance reference implementation).
    Wasmtime,
    /// Use WasmEdge (CNCF project, strong WASI preview 2 support).
    WasmEdge,
    /// Auto-detect: try wasmtime first, then WasmEdge.
    #[default]
    Auto,
}

/// WASI configuration for a Wasm container.
#[derive(Debug, Clone, Default)]
pub struct WasiConfig {
    /// Preferred runtime backend.
    pub runtime: WasmRuntime,
    /// WASI environment variables (supplement to the process environment).
    pub env: Vec<(String, String)>,
    /// Host→guest directory mappings to preopen for WASI filesystem access.
    ///
    /// Each entry is `(host_path, guest_path)`.  For identity mappings
    /// (host and guest are the same path) set both to the same value.
    pub preopened_dirs: Vec<(PathBuf, PathBuf)>,
}

/// Wasm module version tag (bytes 4-7): `01 00 00 00`.
/// Components share the `\0asm` magic but carry a different version tag.
const WASM_MODULE_VERSION: [u8; 4] = [0x01, 0x00, 0x00, 0x00];

/// Returns `true` if the file at `path` begins with WebAssembly magic bytes.
///
/// Returns `false` (not an error) when the file is missing, too short, or
/// cannot be read.
pub fn is_wasm_binary(path: &Path) -> io::Result<bool> {
    use std::io::Read;
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let mut magic = [0u8; 4];
    match f.read_exact(&mut magic) {
        Ok(()) => Ok(magic == WASM_MAGIC),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(e),
    }
}

/// Returns `true` if the file at `path` is a WebAssembly Component (not a plain module).
///
/// Both plain modules and components share the `\0asm` magic prefix.  They are
/// distinguished by bytes 4-7: modules have `[0x01, 0x00, 0x00, 0x00]` (version 1),
/// components have a different layer-type version tag (e.g. `[0x0d, 0x00, 0x01, 0x00]`).
///
/// Returns `false` (not an error) when the file is missing, too short, cannot be
/// read, or does not start with the Wasm magic prefix.
pub fn is_wasm_component_binary(path: &Path) -> io::Result<bool> {
    use std::io::Read;
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let mut header = [0u8; 8];
    match f.read_exact(&mut header) {
        Ok(()) => {
            if header[..4] != WASM_MAGIC {
                return Ok(false);
            }
            Ok(header[4..8] != WASM_MODULE_VERSION)
        }
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(e),
    }
}

/// Find an installed Wasm runtime binary in PATH.
///
/// Returns `(WasmRuntime, PathBuf)` for the first runtime found, or `None`
/// if neither wasmtime nor wasmedge is installed.
///
/// Preference order: `Auto`/`Wasmtime` → wasmtime first; `WasmEdge` → WasmEdge first.
pub fn find_wasm_runtime(preferred: WasmRuntime) -> Option<(WasmRuntime, PathBuf)> {
    let candidates: &[(&str, WasmRuntime)] = match preferred {
        WasmRuntime::WasmEdge => &[
            ("wasmedge", WasmRuntime::WasmEdge),
            ("wasmtime", WasmRuntime::Wasmtime),
        ],
        _ => &[
            ("wasmtime", WasmRuntime::Wasmtime),
            ("wasmedge", WasmRuntime::WasmEdge),
        ],
    };
    for (name, rt) in candidates {
        if let Some(path) = find_in_path(name) {
            return Some((*rt, path));
        }
    }
    None
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path_env = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_env) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Spawn a WebAssembly module through an installed Wasm runtime subprocess.
///
/// `program` — path to the `.wasm` file on the host filesystem.
/// `extra_args` — forwarded verbatim to the Wasm module as WASI argv[1..].
///
/// # Errors
///
/// Returns `Err` if no runtime is found in PATH or if the subprocess fails to
/// start.
pub fn spawn_wasm(
    program: &Path,
    extra_args: &[std::ffi::OsString],
    wasi: &WasiConfig,
    stdin: std::process::Stdio,
    stdout: std::process::Stdio,
    stderr: std::process::Stdio,
) -> io::Result<std::process::Child> {
    let (rt, runtime_bin) = find_wasm_runtime(wasi.runtime).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no Wasm runtime found in PATH — install wasmtime or wasmedge",
        )
    })?;

    log::info!(
        "spawning Wasm module '{}' via {:?} ({})",
        program.display(),
        rt,
        runtime_bin.display()
    );

    let mut cmd = match rt {
        WasmRuntime::Wasmtime => build_wasmtime_cmd(&runtime_bin, program, extra_args, wasi),
        WasmRuntime::WasmEdge => build_wasmedge_cmd(&runtime_bin, program, extra_args, wasi),
        WasmRuntime::Auto => unreachable!("Auto resolved to a concrete runtime above"),
    };

    cmd.stdin(stdin).stdout(stdout).stderr(stderr).spawn()
}

fn build_wasmtime_cmd(
    runtime: &Path,
    wasm: &Path,
    extra_args: &[std::ffi::OsString],
    wasi: &WasiConfig,
) -> std::process::Command {
    let mut cmd = std::process::Command::new(runtime);
    cmd.arg("run");
    // wasmtime >= 14: --dir host::guest
    for (host, guest) in &wasi.preopened_dirs {
        cmd.arg("--dir")
            .arg(format!("{}::{}", host.display(), guest.display()));
    }
    for (k, v) in &wasi.env {
        cmd.arg("--env").arg(format!("{k}={v}"));
    }
    cmd.arg("--").arg(wasm);
    cmd.args(extra_args);
    cmd
}

fn build_wasmedge_cmd(
    runtime: &Path,
    wasm: &Path,
    extra_args: &[std::ffi::OsString],
    wasi: &WasiConfig,
) -> std::process::Command {
    let mut cmd = std::process::Command::new(runtime);
    // wasmedge: --dir host:guest (single colon)
    for (host, guest) in &wasi.preopened_dirs {
        cmd.arg("--dir")
            .arg(format!("{}:{}", host.display(), guest.display()));
    }
    for (k, v) in &wasi.env {
        cmd.arg("--env").arg(format!("{k}={v}"));
    }
    cmd.arg(wasm);
    cmd.args(extra_args);
    cmd
}

/// Run a Wasm module in-process via embedded wasmtime.
///
/// Only available with `--features embedded-wasm`. Runs synchronously — call
/// from a dedicated thread when a non-blocking `Child` handle is needed.
///
/// Returns the WASI exit code (0 = success). Panics from the Wasm module are
/// logged and mapped to exit code 1.
#[cfg(feature = "embedded-wasm")]
pub fn run_wasm_embedded(
    program: &Path,
    extra_args: &[std::ffi::OsString],
    wasi: &WasiConfig,
) -> i32 {
    match run_embedded_inner(program, extra_args, wasi) {
        Ok(code) => code,
        Err(e) => {
            log::error!("embedded wasm: {}", e);
            1
        }
    }
}

#[cfg(feature = "embedded-wasm")]
fn run_embedded_inner(
    program: &Path,
    extra_args: &[std::ffi::OsString],
    wasi: &WasiConfig,
) -> Result<i32, Box<dyn std::error::Error + Send + Sync>> {
    if is_wasm_component_binary(program).unwrap_or(false) {
        log::info!(
            "embedded wasm: '{}' is a component — using P2 path",
            program.display()
        );
        run_embedded_component_file(program, extra_args, wasi)
    } else {
        use wasmtime::{Engine, Module};
        let engine = Engine::default();
        let module = Module::from_file(&engine, program)?;
        run_embedded_module(&engine, &module, extra_args, wasi)
    }
}

/// Load and run a Wasm Component file via embedded wasmtime (P2 / Component Model path).
#[cfg(feature = "embedded-wasm")]
fn run_embedded_component_file(
    program: &Path,
    extra_args: &[std::ffi::OsString],
    wasi: &WasiConfig,
) -> Result<i32, Box<dyn std::error::Error + Send + Sync>> {
    use wasmtime::component::Component;
    use wasmtime::{Config, Engine};
    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config)?;
    let component = Component::from_file(&engine, program)?;
    run_embedded_component(&engine, &component, extra_args, wasi)
}

/// Execute a pre-compiled Wasm module synchronously via embedded wasmtime.
///
/// Exposed as `pub` so integration tests in `tests/` can pass WAT-compiled modules
/// without going through the filesystem.
#[cfg(feature = "embedded-wasm")]
pub fn run_embedded_module(
    engine: &wasmtime::Engine,
    module: &wasmtime::Module,
    extra_args: &[std::ffi::OsString],
    wasi: &WasiConfig,
) -> Result<i32, Box<dyn std::error::Error + Send + Sync>> {
    use wasmtime::{Linker, Store};
    use wasmtime_wasi::p1::{self, WasiP1Ctx};
    use wasmtime_wasi::{DirPerms, FilePerms, WasiCtxBuilder};

    let mut builder = WasiCtxBuilder::new();
    builder.inherit_stdin().inherit_stdout().inherit_stderr();

    for (k, v) in &wasi.env {
        builder.env(k, v);
    }

    for (host, guest) in &wasi.preopened_dirs {
        builder.preopened_dir(
            host,
            guest.to_string_lossy(),
            DirPerms::all(),
            FilePerms::all(),
        )?;
    }

    // argv[0] = "module.wasm" (conventional placeholder), then caller-supplied args.
    builder.arg("module.wasm");
    for arg in extra_args {
        builder.arg(arg.to_string_lossy());
    }

    let wasi_ctx: WasiP1Ctx = builder.build_p1();
    let mut store = Store::new(engine, wasi_ctx);
    let mut linker: Linker<WasiP1Ctx> = Linker::new(engine);
    p1::add_to_linker_sync(&mut linker, |s| s)?;

    let instance = linker.instantiate(&mut store, module)?;
    let start = instance.get_typed_func::<(), ()>(&mut store, "_start")?;

    match start.call(&mut store, ()) {
        Ok(()) => Ok(0),
        Err(e) => {
            // proc_exit wraps I32Exit in the anyhow error chain (outer context is a
            // wasmtime backtrace frame); traverse the chain to find it.
            if let Some(exit) = e
                .chain()
                .find_map(|ce| ce.downcast_ref::<wasmtime_wasi::I32Exit>())
            {
                Ok(exit.0)
            } else {
                Err(e.into())
            }
        }
    }
}

/// Execute a Wasm Component synchronously via embedded wasmtime (WASI Preview 2).
///
/// Uses the `wasi:cli/run` interface exported by WASI Command components.  The
/// component must implement the `wasi:cli/run@0.2.0` world (produced by Rust's
/// `wasm32-wasip2` target or `wasm-tools component new`).
///
/// Exposed as `pub` so integration tests can pass pre-loaded components without
/// going through the filesystem.
#[cfg(feature = "embedded-wasm")]
pub fn run_embedded_component(
    engine: &wasmtime::Engine,
    component: &wasmtime::component::Component,
    extra_args: &[std::ffi::OsString],
    wasi: &WasiConfig,
) -> Result<i32, Box<dyn std::error::Error + Send + Sync>> {
    use wasmtime::component::Linker;
    use wasmtime::Store;
    use wasmtime_wasi::{DirPerms, FilePerms, ResourceTable, WasiCtx, WasiCtxBuilder, WasiView};

    struct WasiState {
        ctx: WasiCtx,
        table: ResourceTable,
    }

    impl WasiView for WasiState {
        fn ctx(&mut self) -> wasmtime_wasi::WasiCtxView<'_> {
            wasmtime_wasi::WasiCtxView {
                ctx: &mut self.ctx,
                table: &mut self.table,
            }
        }
    }

    let mut builder = WasiCtxBuilder::new();
    builder.inherit_stdin().inherit_stdout().inherit_stderr();

    // argv[0] = "module.wasm", then caller-supplied args.
    builder.arg("module.wasm");
    for arg in extra_args {
        builder.arg(arg.to_string_lossy());
    }

    for (k, v) in &wasi.env {
        builder.env(k, v);
    }

    for (host, guest) in &wasi.preopened_dirs {
        builder.preopened_dir(
            host,
            guest.to_string_lossy(),
            DirPerms::all(),
            FilePerms::all(),
        )?;
    }

    let state = WasiState {
        ctx: builder.build(),
        table: ResourceTable::new(),
    };

    let mut store = Store::new(engine, state);
    let mut linker: Linker<WasiState> = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;

    let command =
        wasmtime_wasi::p2::bindings::sync::Command::instantiate(&mut store, component, &linker)?;

    match command.wasi_cli_run().call_run(&mut store) {
        Ok(Ok(())) => Ok(0),
        Ok(Err(())) => Ok(1),
        Err(e) => {
            // proc_exit wraps I32Exit in the anyhow error chain; traverse the chain to find it.
            if let Some(exit) = e
                .chain()
                .find_map(|ce| ce.downcast_ref::<wasmtime_wasi::I32Exit>())
            {
                Ok(exit.0)
            } else {
                Err(e.into())
            }
        }
    }
}

#[cfg(all(test, feature = "embedded-wasm"))]
mod embedded_tests {
    use super::*;
    use wasmtime::{Engine, Module};

    fn run_wat(wat: &str) -> i32 {
        let engine = Engine::default();
        let module = Module::new(&engine, wat.as_bytes()).unwrap();
        run_embedded_module(&engine, &module, &[], &WasiConfig::default()).unwrap()
    }

    // WASI P1 requires modules to export a `memory` (used by many WASI syscalls).
    // WAT requires imports before definitions, hence the import comes first.
    const WAT_EXIT_0: &str = r#"(module
        (import "wasi_snapshot_preview1" "proc_exit" (func $proc_exit (param i32)))
        (memory 1)
        (export "memory" (memory 0))
        (func $_start i32.const 0 call $proc_exit)
        (export "_start" (func $_start)))"#;

    const WAT_EXIT_42: &str = r#"(module
        (import "wasi_snapshot_preview1" "proc_exit" (func $proc_exit (param i32)))
        (memory 1)
        (export "memory" (memory 0))
        (func $_start i32.const 42 call $proc_exit)
        (export "_start" (func $_start)))"#;

    #[test]
    fn test_embedded_exit_zero() {
        assert_eq!(run_wat(WAT_EXIT_0), 0);
    }

    #[test]
    fn test_embedded_exit_nonzero() {
        assert_eq!(run_wat(WAT_EXIT_42), 42);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_is_wasm_binary_magic_bytes() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        // Wasm module header: magic + version 1
        tmp.write_all(&[0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00])
            .unwrap();
        tmp.flush().unwrap();
        assert!(is_wasm_binary(tmp.path()).unwrap());
    }

    #[test]
    fn test_is_wasm_binary_elf_is_false() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"\x7fELF\x02\x01\x01\x00").unwrap();
        tmp.flush().unwrap();
        assert!(!is_wasm_binary(tmp.path()).unwrap());
    }

    #[test]
    fn test_is_wasm_binary_too_short() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"\x00\x61").unwrap();
        tmp.flush().unwrap();
        assert!(!is_wasm_binary(tmp.path()).unwrap());
    }

    #[test]
    fn test_is_wasm_binary_empty_file() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        assert!(!is_wasm_binary(tmp.path()).unwrap());
    }

    #[test]
    fn test_is_wasm_binary_missing_path() {
        // Missing file → Ok(false), not an error.
        let result = is_wasm_binary(Path::new("/tmp/__pelagos_nonexistent_abc123.wasm"));
        assert!(result.is_ok());
        assert!(!result.unwrap());
    }

    #[test]
    fn test_is_wasm_component_binary_module_is_false() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        // Plain module: magic + version 01 00 00 00
        tmp.write_all(&[0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00])
            .unwrap();
        tmp.flush().unwrap();
        assert!(!is_wasm_component_binary(tmp.path()).unwrap());
    }

    #[test]
    fn test_is_wasm_component_binary_component_is_true() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        // Component: magic + component version 0d 00 01 00
        tmp.write_all(&[0x00, 0x61, 0x73, 0x6D, 0x0d, 0x00, 0x01, 0x00])
            .unwrap();
        tmp.flush().unwrap();
        assert!(is_wasm_component_binary(tmp.path()).unwrap());
    }

    #[test]
    fn test_is_wasm_component_binary_too_short_is_false() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(&[0x00, 0x61, 0x73, 0x6D]).unwrap(); // only 4 bytes
        tmp.flush().unwrap();
        assert!(!is_wasm_component_binary(tmp.path()).unwrap());
    }

    #[test]
    fn test_is_wasm_component_binary_non_wasm_is_false() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"\x7fELF\x02\x01\x01\x00").unwrap();
        tmp.flush().unwrap();
        assert!(!is_wasm_component_binary(tmp.path()).unwrap());
    }

    #[test]
    fn test_is_wasm_media_type_known_types() {
        assert!(is_wasm_media_type(
            "application/vnd.bytecodealliance.wasm.component.layer.v0+wasm"
        ));
        assert!(is_wasm_media_type(
            "application/vnd.wasm.content.layer.v1+wasm"
        ));
        assert!(is_wasm_media_type("application/wasm"));
    }

    #[test]
    fn test_is_wasm_media_type_standard_layer_is_false() {
        assert!(!is_wasm_media_type(
            "application/vnd.oci.image.layer.v1.tar+gzip"
        ));
        assert!(!is_wasm_media_type(
            "application/vnd.docker.image.rootfs.diff.tar.gzip"
        ));
        assert!(!is_wasm_media_type(""));
    }

    #[test]
    fn test_find_wasm_runtime_does_not_panic() {
        // Verify no panic regardless of whether runtimes are installed.
        let _ = find_wasm_runtime(WasmRuntime::Auto);
        let _ = find_wasm_runtime(WasmRuntime::Wasmtime);
        let _ = find_wasm_runtime(WasmRuntime::WasmEdge);
    }

    // ── Regression tests for host→guest dir mapping (fix: Vec<PathBuf> → Vec<(PathBuf,PathBuf)>) ──
    //
    // Before the fix, preopened_dirs was Vec<PathBuf> and both wasmtime and
    // wasmedge received `--dir /host::/host` (identity), so a module that
    // opened `/data/file` would fail when the bind-mount was specified as
    // `--bind /host:/data`.

    fn args_of(cmd: &std::process::Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn test_wasmtime_cmd_identity_dir_mapping() {
        // with_wasi_preopened_dir("/data") → --dir /data::/data
        let wasi = WasiConfig {
            runtime: WasmRuntime::Wasmtime,
            env: vec![],
            preopened_dirs: vec![(PathBuf::from("/data"), PathBuf::from("/data"))],
        };
        let cmd = build_wasmtime_cmd(Path::new("wasmtime"), Path::new("app.wasm"), &[], &wasi);
        let args = args_of(&cmd);
        assert!(
            args.iter().any(|a| a == "/data::/data"),
            "expected --dir /data::/data in args: {args:?}"
        );
    }

    #[test]
    fn test_wasmtime_cmd_mapped_dir() {
        // with_wasi_preopened_dir_mapped("/host/binddata", "/data") → --dir /host/binddata::/data
        // This is the regression case: host path ≠ guest path.
        let wasi = WasiConfig {
            runtime: WasmRuntime::Wasmtime,
            env: vec![],
            preopened_dirs: vec![(PathBuf::from("/host/binddata"), PathBuf::from("/data"))],
        };
        let cmd = build_wasmtime_cmd(Path::new("wasmtime"), Path::new("app.wasm"), &[], &wasi);
        let args = args_of(&cmd);
        assert!(
            args.iter().any(|a| a == "/host/binddata::/data"),
            "expected --dir /host/binddata::/data in args: {args:?}"
        );
        // Regression: must NOT produce the old identity-mapped form.
        assert!(
            !args.iter().any(|a| a == "/host/binddata::/host/binddata"),
            "regression: produced identity mapping --dir /host/binddata::/host/binddata"
        );
    }

    #[test]
    fn test_wasmedge_cmd_mapped_dir() {
        // wasmedge uses single-colon: --dir /host/binddata:/data
        let wasi = WasiConfig {
            runtime: WasmRuntime::WasmEdge,
            env: vec![],
            preopened_dirs: vec![(PathBuf::from("/host/binddata"), PathBuf::from("/data"))],
        };
        let cmd = build_wasmedge_cmd(Path::new("wasmedge"), Path::new("app.wasm"), &[], &wasi);
        let args = args_of(&cmd);
        assert!(
            args.iter().any(|a| a == "/host/binddata:/data"),
            "expected --dir /host/binddata:/data in args: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a == "/host/binddata:/host/binddata"),
            "regression: produced identity mapping --dir /host/binddata:/host/binddata"
        );
    }

    #[test]
    fn test_wasmtime_cmd_env_vars() {
        let wasi = WasiConfig {
            runtime: WasmRuntime::Wasmtime,
            env: vec![("FOO".into(), "bar".into()), ("BAZ".into(), "qux".into())],
            preopened_dirs: vec![],
        };
        let cmd = build_wasmtime_cmd(Path::new("wasmtime"), Path::new("app.wasm"), &[], &wasi);
        let args = args_of(&cmd);
        assert!(
            args.iter().any(|a| a == "FOO=bar"),
            "expected FOO=bar in args: {args:?}"
        );
        assert!(
            args.iter().any(|a| a == "BAZ=qux"),
            "expected BAZ=qux in args: {args:?}"
        );
    }
}
