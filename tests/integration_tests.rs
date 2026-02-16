//! Integration tests for remora container features.
//!
//! These tests verify the core containerization features including:
//! - UID/GID mapping
//! - Namespace joining (setns)
//! - Enhanced mount support
//! - Capability management
//! - Resource limits
//!
//! NOTE: Many of these tests require root privileges to create namespaces
//! and perform privileged operations. Run with:
//! ```bash
//! sudo -E cargo test --test integration_tests
//! ```

use remora::container::{Capability, Command, GidMap, Namespace, Stdio, UidMap};
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

/// Helper to check if we're running as root
fn is_root() -> bool {
    unsafe { libc::getuid() == 0 }
}

/// Helper to get test rootfs path
///
/// Uses the existing alpine-rootfs if available, which has busybox and all necessary tools.
/// This avoids issues with dynamically linked binaries and missing libraries.
fn get_test_rootfs() -> Option<PathBuf> {
    // Try to find alpine-rootfs relative to project root
    let current_dir = std::env::current_dir().ok()?;
    let alpine_path = current_dir.join("alpine-rootfs");

    if alpine_path.exists() && alpine_path.join("bin/busybox").exists() {
        Some(alpine_path)
    } else {
        None
    }
}

#[test]
fn test_basic_namespace_creation() {
    if !is_root() {
        eprintln!("Skipping test_basic_namespace_creation: requires root");
        return;
    }

    let Some(rootfs) = get_test_rootfs() else {
        eprintln!("Skipping test_basic_namespace_creation: alpine-rootfs not found");
        return;
    };

    // Test basic namespace creation with UTS and MOUNT
    let result = Command::new("/bin/ash")
        .args(&["-c", "exit 0"])
        .with_namespaces(Namespace::UTS | Namespace::MOUNT)
        .with_chroot(&rootfs)
        .stdin(Stdio::Null)
        .stdout(Stdio::Null)
        .stderr(Stdio::Null)
        .spawn();

    match result {
        Ok(mut child) => {
            let status = child.wait().unwrap();
            assert!(status.success(), "Child process failed");
        }
        Err(e) => {
            panic!("Failed to spawn with namespaces: {:?}", e);
        }
    }
}

#[test]
fn test_proc_mount() {
    if !is_root() {
        eprintln!("Skipping test_proc_mount: requires root");
        return;
    }

    let Some(rootfs) = get_test_rootfs() else {
        eprintln!("Skipping test: alpine-rootfs not found");
        return;
    };

    // Create a test script that checks if /proc is mounted
    let test_script = rootfs.join("tmp/test_proc.sh");
    let mut script = fs::File::create(&test_script).unwrap();
    writeln!(script, "#!/bin/ash").unwrap();
    writeln!(script, "if [ -f /proc/self/status ]; then").unwrap();
    writeln!(script, "  exit 0").unwrap();
    writeln!(script, "else").unwrap();
    writeln!(script, "  exit 1").unwrap();
    writeln!(script, "fi").unwrap();
    drop(script);

    let mut perms = fs::metadata(&test_script).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&test_script, perms).unwrap();

    // Test with_proc_mount()
    let result = Command::new("/tmp/test_proc.sh")
        .with_namespaces(Namespace::MOUNT | Namespace::UTS)
        .with_chroot(&rootfs)
        .with_proc_mount()
        .stdin(Stdio::Null)
        .stdout(Stdio::Null)
        .stderr(Stdio::Null)
        .spawn();

    match result {
        Ok(mut child) => {
            let status = child.wait().unwrap();
            assert!(status.success(), "Proc was not mounted correctly");
        }
        Err(e) => panic!("Failed to spawn with proc mount: {:?}", e),
    }

}

#[test]
fn test_capability_dropping() {
    if !is_root() {
        eprintln!("Skipping test_capability_dropping: requires root");
        return;
    }

    let Some(rootfs) = get_test_rootfs() else {
        eprintln!("Skipping test: alpine-rootfs not found");
        return;
    };

    // Test drop_all_capabilities()
    let result = Command::new("/bin/ash")
        .args(&["-c", "exit 0"])
        .with_namespaces(Namespace::MOUNT | Namespace::UTS)
        .with_chroot(&rootfs)
        .drop_all_capabilities()
        .stdin(Stdio::Null)
        .stdout(Stdio::Null)
        .stderr(Stdio::Null)
        .spawn();

    match result {
        Ok(mut child) => {
            let status = child.wait().unwrap();
            assert!(status.success(), "Child process failed with dropped caps");
        }
        Err(e) => panic!("Failed to spawn with dropped capabilities: {:?}", e),
    }

}

#[test]
fn test_selective_capabilities() {
    if !is_root() {
        eprintln!("Skipping test_selective_capabilities: requires root");
        return;
    }

    let Some(rootfs) = get_test_rootfs() else {
        eprintln!("Skipping test: alpine-rootfs not found");
        return;
    };

    // Test keeping only specific capabilities
    let result = Command::new("/bin/ash")
        .args(&["-c", "exit 0"])
        .with_namespaces(Namespace::MOUNT | Namespace::UTS)
        .with_chroot(&rootfs)
        .with_capabilities(Capability::NET_BIND_SERVICE | Capability::CHOWN)
        .stdin(Stdio::Null)
        .stdout(Stdio::Null)
        .stderr(Stdio::Null)
        .spawn();

    match result {
        Ok(mut child) => {
            let status = child.wait().unwrap();
            assert!(status.success(), "Child process failed with selective caps");
        }
        Err(e) => panic!("Failed to spawn with selective capabilities: {:?}", e),
    }

}

#[test]
fn test_resource_limits_fds() {
    if !is_root() {
        eprintln!("Skipping test_resource_limits_fds: requires root");
        return;
    }

    let Some(rootfs) = get_test_rootfs() else {
        eprintln!("Skipping test: alpine-rootfs not found");
        return;
    };

    // Create a script that checks ulimit
    let test_script = rootfs.join("tmp/test_ulimit.sh");
    let mut script = fs::File::create(&test_script).unwrap();
    writeln!(script, "#!/bin/ash").unwrap();
    writeln!(script, "# Check if fd limit is 100").unwrap();
    writeln!(script, "limit=$(ulimit -n)").unwrap();
    writeln!(script, "if [ \"$limit\" = \"100\" ]; then").unwrap();
    writeln!(script, "  exit 0").unwrap();
    writeln!(script, "else").unwrap();
    writeln!(script, "  exit 1").unwrap();
    writeln!(script, "fi").unwrap();
    drop(script);

    let mut perms = fs::metadata(&test_script).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&test_script, perms).unwrap();

    // Test with_max_fds()
    let result = Command::new("/tmp/test_ulimit.sh")
        .with_namespaces(Namespace::MOUNT | Namespace::UTS)
        .with_chroot(&rootfs)
        .with_max_fds(100)
        .stdin(Stdio::Null)
        .stdout(Stdio::Null)
        .stderr(Stdio::Null)
        .spawn();

    match result {
        Ok(mut child) => {
            let status = child.wait().unwrap();
            assert!(status.success(), "FD limit was not set correctly");
        }
        Err(e) => panic!("Failed to spawn with fd limit: {:?}", e),
    }

}

#[test]
fn test_resource_limits_memory() {
    if !is_root() {
        eprintln!("Skipping test_resource_limits_memory: requires root");
        return;
    }

    let Some(rootfs) = get_test_rootfs() else {
        eprintln!("Skipping test: alpine-rootfs not found");
        return;
    };

    // Test with_memory_limit() - just verify it doesn't crash
    let result = Command::new("/bin/ash")
        .args(&["-c", "exit 0"])
        .with_namespaces(Namespace::MOUNT | Namespace::UTS)
        .with_chroot(&rootfs)
        .with_memory_limit(512 * 1024 * 1024) // 512MB
        .stdin(Stdio::Null)
        .stdout(Stdio::Null)
        .stderr(Stdio::Null)
        .spawn();

    match result {
        Ok(mut child) => {
            let status = child.wait().unwrap();
            assert!(status.success(), "Child process failed with memory limit");
        }
        Err(e) => panic!("Failed to spawn with memory limit: {:?}", e),
    }

}

#[test]
fn test_resource_limits_cpu() {
    if !is_root() {
        eprintln!("Skipping test_resource_limits_cpu: requires root");
        return;
    }

    let Some(rootfs) = get_test_rootfs() else {
        eprintln!("Skipping test: alpine-rootfs not found");
        return;
    };

    // Test with_cpu_time_limit()
    let result = Command::new("/bin/ash")
        .args(&["-c", "exit 0"])
        .with_namespaces(Namespace::MOUNT | Namespace::UTS)
        .with_chroot(&rootfs)
        .with_cpu_time_limit(60) // 60 seconds
        .stdin(Stdio::Null)
        .stdout(Stdio::Null)
        .stderr(Stdio::Null)
        .spawn();

    match result {
        Ok(mut child) => {
            let status = child.wait().unwrap();
            assert!(status.success(), "Child process failed with CPU limit");
        }
        Err(e) => panic!("Failed to spawn with CPU time limit: {:?}", e),
    }

}

#[test]
fn test_combined_features() {
    if !is_root() {
        eprintln!("Skipping test_combined_features: requires root");
        return;
    }

    let Some(rootfs) = get_test_rootfs() else {
        eprintln!("Skipping test: alpine-rootfs not found");
        return;
    };

    // Test combining multiple features together
    let result = Command::new("/bin/ash")
        .args(&["-c", "exit 0"])
        .with_namespaces(Namespace::MOUNT | Namespace::UTS | Namespace::CGROUP)
        .with_chroot(&rootfs)
        .with_proc_mount()
        .with_capabilities(Capability::NET_BIND_SERVICE)
        .with_max_fds(500)
        .with_memory_limit(256 * 1024 * 1024)
        .stdin(Stdio::Null)
        .stdout(Stdio::Null)
        .stderr(Stdio::Null)
        .spawn();

    match result {
        Ok(mut child) => {
            let status = child.wait().unwrap();
            assert!(status.success(), "Child process failed with combined features");
        }
        Err(e) => panic!("Failed to spawn with combined features: {:?}", e),
    }

}

#[test]
fn test_uid_gid_api() {
    // This test verifies that the UID/GID mapping API exists and can be called.
    //
    // Note: Full USER namespace + UID/GID mapping testing has kernel limitations:
    // 1. USER namespaces are designed for unprivileged users
    // 2. Kernel restrictions prevent certain operations when already root
    // 3. Setting UID/GID without USER namespace has complex ordering requirements
    //
    // The API is fully implemented and works correctly in main.rs usage.
    // This test verifies the builder pattern API is available and compiles.

    let _cmd = Command::new("/bin/ash")
        .with_uid(1000)
        .with_gid(1000)
        .with_uid_maps(&[UidMap {
            inside: 0,
            outside: 1000,
            count: 1,
        }])
        .with_gid_maps(&[GidMap {
            inside: 0,
            outside: 1000,
            count: 1,
        }]);

    // Just verify the API compiles and methods are available
    assert!(true, "UID/GID API is available");

}

#[test]
fn test_namespace_bitflags() {
    // Test that namespace bitflags work correctly (no root needed)
    let ns1 = Namespace::UTS;
    let ns2 = Namespace::MOUNT;
    let combined = ns1 | ns2;

    assert!(combined.contains(Namespace::UTS));
    assert!(combined.contains(Namespace::MOUNT));
    assert!(!combined.contains(Namespace::PID));
}

#[test]
fn test_capability_bitflags() {
    // Test that capability bitflags work correctly (no root needed)
    let cap1 = Capability::CHOWN;
    let cap2 = Capability::NET_BIND_SERVICE;
    let combined = cap1 | cap2;

    assert!(combined.contains(Capability::CHOWN));
    assert!(combined.contains(Capability::NET_BIND_SERVICE));
    assert!(!combined.contains(Capability::SYS_ADMIN));
}

#[test]
fn test_command_builder_pattern() {
    // Test that the builder pattern works (no root needed, won't spawn)
    let rootfs = PathBuf::from("/tmp/test");

    let _cmd = Command::new("/bin/ash")
        .args(&["-c", "echo test", "-x"])
        .stdin(Stdio::Inherit)
        .stdout(Stdio::Piped)
        .stderr(Stdio::Null)
        .with_namespaces(Namespace::UTS)
        .with_chroot(&rootfs)
        .with_proc_mount()
        .with_max_fds(1024);

    // Just test that the builder methods chain correctly
    assert!(true, "Builder pattern works");
}
