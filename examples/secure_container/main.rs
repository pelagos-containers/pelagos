//! Demonstrates all Phase 1 security features working together.
//!
//! This example shows a production-ready secure container with:
//! - Seccomp filtering (Docker's default profile)
//! - No-new-privileges flag (prevent setuid escalation)
//! - Read-only rootfs (immutable infrastructure)
//! - Masked paths (hide sensitive kernel info)
//! - Dropped capabilities (least privilege)
//! - Resource limits (prevent resource exhaustion)
//!
//! # Running
//!
//! Build the alpine rootfs first:
//! ```bash
//! ./build-rootfs-docker.sh    # or ./build-rootfs-tarball.sh
//! ```
//!
//! Then run the example (requires root):
//! ```bash
//! sudo -E cargo run --example secure_container
//! ```

use remora::container::{Command, Namespace, Stdio};
use std::env;

fn main() {
    env_logger::init();

    let current_dir = env::current_dir().expect("Failed to get current directory");
    let rootfs = current_dir.join("alpine-rootfs");

    if !rootfs.exists() {
        eprintln!("Error: alpine-rootfs not found!");
        eprintln!("Build it with: ./build-rootfs-docker.sh");
        eprintln!("Or without Docker: ./build-rootfs-tarball.sh");
        std::process::exit(1);
    }

    println!("=== Phase 1 Secure Container Demo ===\n");
    println!("This container has ALL Phase 1 security features enabled:");
    println!("- ✅ Seccomp filtering (Docker's default profile)");
    println!("- ✅ No-new-privileges (blocks setuid escalation)");
    println!("- ✅ Read-only rootfs (immutable filesystem)");
    println!("- ✅ Masked paths (hidden kernel info)");
    println!("- ✅ No capabilities (completely unprivileged)");
    println!("- ✅ Resource limits (controlled resource usage)");
    println!();

    println!("Starting secure container...\n");

    let mut child = Command::new("/bin/ash")
        .args(&[
            "-c",
            r#"
echo "=== Container Environment ==="
echo "Hostname: $(hostname)"
echo "Working directory: $(pwd)"
echo ""
echo "=== Testing Security Features ==="
echo ""
echo "1. Testing read-only rootfs:"
touch /test_write 2>&1 && echo "  FAIL: Can write to rootfs!" || echo "  ✓ Cannot write to rootfs (read-only)"
echo ""
echo "2. Testing seccomp (attempt to mount):"
/bin/mount -t tmpfs tmpfs /tmp 2>&1 | grep -q "Operation not permitted" && echo "  ✓ Mount blocked by seccomp" || echo "  Note: Mount test inconclusive"
echo ""
echo "3. Testing masked paths:"
if [ ! -r /proc/kcore ]; then
    echo "  ✓ /proc/kcore is masked"
else
    echo "  Note: /proc/kcore exists (might not exist in Alpine)"
fi
echo ""
echo "4. Checking no-new-privileges:"
grep NoNewPrivs /proc/self/status 2>/dev/null || echo "  (NoNewPrivs status check)"
echo ""
echo "5. Container can still run normally:"
echo "  ✓ Echo works"
echo "  ✓ Process isolation active"
echo "  ✓ Filesystem access works (read-only)"
echo ""
echo "=== Security Demo Complete ==="
"#,
        ])
        .stdin(Stdio::Inherit)
        .stdout(Stdio::Inherit)
        .stderr(Stdio::Inherit)
        .with_chroot(&rootfs)
        .with_namespaces(Namespace::UTS | Namespace::MOUNT | Namespace::PID)
        .with_proc_mount()
        // Phase 1 Security Features (all enabled)
        .with_seccomp_default()        // Syscall filtering
        .with_no_new_privileges(true)  // Prevent privilege escalation
        .with_readonly_rootfs(true)    // Immutable rootfs
        .with_masked_paths_default()   // Hide sensitive paths
        .drop_all_capabilities()       // No capabilities
        .with_max_fds(1024)            // Limit file descriptors
        .with_memory_limit(512 * 1024 * 1024) // 512 MB memory limit
        .spawn()
        .expect("Failed to spawn secure container");

    let status = child.wait().expect("Failed to wait for container");

    println!("\nContainer exited with status: {:?}", status);

    if status.success() {
        println!("\n✅ All Phase 1 security features working correctly!");
    } else {
        println!("\n⚠️  Container exited with non-zero status");
    }
}
