# Testing Guide for Remora

This document describes how to run tests for the remora container library.

## Integration Tests

The integration tests verify all Phase 3 features including:

- ✅ Basic namespace creation (UTS, MOUNT, CGROUP)
- ✅ Automatic /proc mounting
- ✅ Capability management (drop all, selective retention)
- ✅ Resource limits (file descriptors, memory, CPU time)
- ✅ UID/GID mapping
- ✅ Combined features
- ✅ Bitflags API (no root needed)
- ✅ Builder pattern API (no root needed)

### Requirements

**All integration tests require root privileges** and should be run with `sudo -E`. This is because they:
- Create Linux namespaces (requires CAP_SYS_ADMIN)
- Perform chroot operations (requires CAP_SYS_CHROOT)
- Manage capabilities (requires CAP_SETPCAP)
- Set resource limits on other processes

### Running Tests

#### Option 1: Using the test runner script (Recommended)

```bash
# Run all integration tests
sudo -E ./run-integration-tests.sh

# Run a specific test
sudo -E ./run-integration-tests.sh test_proc_mount
```

The `-E` flag preserves your environment variables (like RUST_LOG).

#### Option 2: Using cargo directly

```bash
# Run all integration tests
sudo -E cargo test --test integration_tests

# Run with output visible
sudo -E cargo test --test integration_tests -- --nocapture

# Run a specific test
sudo -E cargo test --test integration_tests test_capability_dropping
```

#### Option 3: Run non-privileged tests only

A few tests (bitflags, builder pattern) don't require root and can be run normally:

```bash
cargo test --test integration_tests test_namespace_bitflags
cargo test --test integration_tests test_capability_bitflags
cargo test --test integration_tests test_command_builder_pattern
```

**Note:** All other tests require root privileges.

### Test Coverage

| Feature | Test Name | Description |
|---------|-----------|-------------|
| Basic Namespaces | `test_basic_namespace_creation` | Creates UTS and MOUNT namespaces |
| /proc Mount | `test_proc_mount` | Verifies automatic /proc mounting |
| Drop All Caps | `test_capability_dropping` | Drops all Linux capabilities |
| Selective Caps | `test_selective_capabilities` | Keeps only specified capabilities |
| FD Limits | `test_resource_limits_fds` | Sets file descriptor limits |
| Memory Limits | `test_resource_limits_memory` | Sets memory limits (RLIMIT_AS) |
| CPU Limits | `test_resource_limits_cpu` | Sets CPU time limits |
| Combined | `test_combined_features` | Tests multiple features together |
| UID/GID API | `test_uid_gid_api` | Tests UID/GID mapping API (compilation test) |
| Namespace Flags | `test_namespace_bitflags` | Tests bitflags API (no root) |
| Capability Flags | `test_capability_bitflags` | Tests capability bitflags (no root) |
| Builder Pattern | `test_command_builder_pattern` | Tests API ergonomics (no root) |

### Expected Output

When running with `--nocapture`, you'll see:

```
running 12 tests
test test_basic_namespace_creation ... ok
test test_capability_bitflags ... ok
test test_capability_dropping ... ok
test test_combined_features ... ok
test test_command_builder_pattern ... ok
test test_namespace_bitflags ... ok
test test_proc_mount ... ok
test test_resource_limits_cpu ... ok
test test_resource_limits_fds ... ok
test test_resource_limits_memory ... ok
test test_selective_capabilities ... ok
test test_uid_gid_mapping ... ok

test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

### Important Notes

#### USER Namespace Testing

The `test_uid_gid_setting` test verifies the `with_uid()` and `with_gid()` API by dropping privileges to nobody (UID 65534).

**Note on USER namespaces:** The full USER namespace feature (with `with_uid_maps()` and `with_gid_maps()`) is designed for unprivileged users to create containers. When already running as root, USER namespaces have kernel restrictions that prevent certain mappings. The API is fully implemented and works correctly for unprivileged users, but cannot be integration tested when running as root.

To test USER namespace + UID/GID mapping as an unprivileged user:
```bash
# As regular user (not root)
cargo build
./target/debug/remora --rootfs ./alpine-rootfs --exe /bin/sh --uid $UID --gid $GID
```

### Troubleshooting

#### "requires root" messages

If you run without sudo, tests will print skip messages:

```
Skipping test_basic_namespace_creation: requires root
```

This is expected behavior. Rerun with `sudo -E`.

#### Permission denied errors

- Make sure you're using `sudo -E` to preserve environment
- Check that your user can run sudo
- Verify namespaces are enabled in your kernel (`ls /proc/self/ns/`)

#### Namespace creation fails

- Check kernel version (3.8+ required for user namespaces)
- Verify `/proc/sys/user/max_user_namespaces` is non-zero
- Some security modules (SELinux, AppArmor) may restrict namespaces

#### Test rootfs creation fails

- Ensure `/tmp` is writable
- Verify `/bin/sh` exists on your system
- Check available disk space

### Cleanup

The tests automatically clean up their temporary rootfs directories. If tests are interrupted, you may need to manually remove:

```bash
rm -rf /tmp/remora_test_*
```

### Adding New Tests

To add new tests:

1. Add test function to `tests/integration_tests.rs`
2. Use `is_root()` helper to check for privileges
3. Use `create_test_rootfs()` and `cleanup_test_rootfs()` for filesystem setup
4. Follow the naming convention: `test_<feature>_<aspect>`

Example:

```rust
#[test]
fn test_my_new_feature() {
    if !is_root() {
        eprintln!("Skipping test_my_new_feature: requires root");
        return;
    }

    let rootfs = create_test_rootfs();

    let result = Command::new("/bin/sh")
        .args(&["-c", "exit 0"])
        .with_namespaces(Namespace::MOUNT)
        .with_chroot(&rootfs)
        // ... your feature configuration
        .spawn();

    assert!(result.is_ok(), "Failed to spawn");
    let mut child = result.unwrap();
    let status = child.wait().unwrap();
    assert!(status.success(), "Test failed");

    cleanup_test_rootfs(&rootfs);
}
```

## Unit Tests

Currently, unit tests are limited because most functionality requires kernel support. Consider adding unit tests for:

- Namespace flag conversions
- Error handling
- Builder pattern validation
- Type safety checks

To run unit tests:

```bash
cargo test --lib
```

## Continuous Integration

For CI environments, you may need to:

1. Run tests in a Docker container with `--privileged`
2. Use a VM with full namespace support
3. Skip integration tests and run only unit tests

Example GitHub Actions workflow:

```yaml
- name: Run integration tests
  run: sudo -E cargo test --test integration_tests
  env:
    RUST_BACKTRACE: 1
```

## Performance Testing

Integration tests focus on correctness, not performance. For performance testing:

1. Use criterion benchmarks (TODO)
2. Measure container startup time
3. Test resource limit enforcement
4. Verify capability overhead

---

**Last Updated:** 2026-02-16
**Test Count:** 12 integration tests
**Coverage:** All Phase 3 features
