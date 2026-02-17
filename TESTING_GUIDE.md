# Remora Security Testing Guide

Complete guide for running all security tests and verifying Phase 1 features.

---

## Prerequisites

### 1. Build the Alpine rootfs (tests need it):

```bash
./build-rootfs-docker.sh    # If you have Docker
# or
./build-rootfs-tarball.sh   # If you don't have Docker
```

### 2. Verify rootfs exists:

```bash
ls alpine-rootfs/bin/busybox
```

### 3. Compile the syscall test program (for seccomp demo):

```bash
gcc -static test_syscalls.c -o test_syscalls
cp test_syscalls alpine-rootfs/bin/
```

---

## 1. Unit Tests (No Root Required)

These test the API without actually spawning containers:

```bash
cargo test --lib
```

**Expected output:**
```
test result: ok. 15 passed; 0 failed; 2 ignored
```

**What it tests:**
- Namespace bitflags
- Capability bitflags
- Builder pattern API
- Error handling
- Stdio conversion
- Seccomp filter compilation

---

## 2. Integration Tests (Requires Root)

These actually spawn containers and test security features:

```bash
sudo -E cargo test --test integration_tests
```

**What gets tested:**

### Seccomp Tests (5 tests):
- `test_seccomp_docker_blocks_reboot` - Verifies reboot syscall is blocked
- `test_seccomp_docker_allows_normal_syscalls` - Normal ops work
- `test_seccomp_minimal_is_restrictive` - Minimal profile works
- `test_seccomp_profile_api` - API availability
- `test_seccomp_without_flag_works` - Backward compatibility

### Phase 1 Security Tests (5 tests):
- `test_no_new_privileges` - Verifies NoNewPrivs flag is set
- `test_readonly_rootfs` - Verifies writes fail on read-only fs
- `test_masked_paths_default` - Verifies paths are masked
- `test_masked_paths_custom` - Custom path masking
- `test_combined_phase1_security` - All Phase 1 features together

### Other Container Tests (12 tests):
- Basic namespace creation
- Namespace joining
- Mount operations (/proc, /sys, /dev)
- Capability dropping
- Resource limits (fds, memory, CPU)
- UID/GID API
- Combined features

**Expected output:**
```
test result: ok. 22 passed; 0 failed; 0 ignored
```

---

## 3. Run All Tests Together

```bash
# Build rootfs first
./build-rootfs-docker.sh

# Run all tests (needs sudo for integration tests)
sudo -E cargo test
```

This runs:
- Unit tests (15 tests)
- Integration tests (22 tests)
- Doc tests (7 tests)

**Total: 44 tests**

---

## 4. Security Examples/Demos

### Seccomp Demo

Tests that dangerous syscalls are actually blocked:

```bash
sudo -E cargo run --example seccomp_demo
```

**What it tests:**
- Test 1: Normal syscalls work (echo)
- Test 2: `reboot()` syscall blocked with EPERM
- Test 3: `mount()` syscall blocked with EPERM
- Test 4: No seccomp allows everything

**Expected output:**
```
=== Seccomp Demonstration ===

Test 1: Running echo with Docker's default seccomp profile
Expected: Should work fine (read/write/brk syscalls are allowed)

Hello from secured container!
Exit status: ExitStatus { inner: ExitStatus(unix_wait_status(0)) }

Test 2: Directly calling reboot() syscall (blocked by seccomp)
Expected: Syscall should fail with EPERM

Testing direct syscall: reboot()
reboot() failed: Operation not permitted (errno=1)
SUCCESS: Seccomp blocked reboot syscall with EPERM
Exit status: ExitStatus { inner: ExitStatus(unix_wait_status(0)) }

Test 3: Attempting to mount tmpfs (blocked by seccomp)
Expected: Mount syscall should fail with EPERM

mount: permission denied (are you root?)
SUCCESS: Mount blocked by seccomp (exit code: 1)
Exit status: ExitStatus { inner: ExitStatus(unix_wait_status(0)) }

Test 4: Running without seccomp (all syscalls allowed)
Warning: This is less secure but sometimes needed for compatibility

Running without syscall filtering
Exit status: ExitStatus { inner: ExitStatus(unix_wait_status(0)) }

=== Demonstration Complete ===
```

### Secure Container Demo

Shows all Phase 1 security features working together:

```bash
sudo -E cargo run --example secure_container
```

**What it demonstrates:**
- ✅ Seccomp filtering
- ✅ No-new-privileges
- ✅ Read-only rootfs
- ✅ Masked paths
- ✅ No capabilities
- ✅ Resource limits

**Expected output:**
```
=== Phase 1 Secure Container Demo ===

This container has ALL Phase 1 security features enabled:
- ✅ Seccomp filtering (Docker's default profile)
- ✅ No-new-privileges (blocks setuid escalation)
- ✅ Read-only rootfs (immutable filesystem)
- ✅ Masked paths (hidden kernel info)
- ✅ No capabilities (completely unprivileged)
- ✅ Resource limits (controlled resource usage)

Starting secure container...

=== Container Environment ===
Hostname: ...
Working directory: /

=== Testing Security Features ===

1. Testing read-only rootfs:
  ✓ Cannot write to rootfs (read-only)

2. Testing seccomp (attempt to mount):
  ✓ Mount blocked by seccomp

3. Testing masked paths:
  ✓ /proc/kcore is masked

4. Checking no-new-privileges:
  (NoNewPrivs status check)

5. Container can still run normally:
  ✓ Echo works
  ✓ Process isolation active
  ✓ Filesystem access works (read-only)

=== Security Demo Complete ===

Container exited with status: ExitStatus { ... }

✅ All Phase 1 security features working correctly!
```

---

## 5. Run Specific Test Categories

### Only seccomp tests:
```bash
sudo -E cargo test --test integration_tests seccomp
```

### Only Phase 1 security tests:
```bash
sudo -E cargo test --test integration_tests phase1
sudo -E cargo test --test integration_tests no_new_privileges
sudo -E cargo test --test integration_tests readonly
sudo -E cargo test --test integration_tests masked
```

### Only capability tests:
```bash
sudo -E cargo test --test integration_tests capability
```

### Only namespace tests:
```bash
sudo -E cargo test --test integration_tests namespace
```

### Only mount tests:
```bash
sudo -E cargo test --test integration_tests mount
```

### Only resource limit tests:
```bash
sudo -E cargo test --test integration_tests resource
```

---

## 6. Verbose Test Output

To see what each test is actually doing:

```bash
sudo -E cargo test --test integration_tests -- --nocapture
```

This shows all stdout/stderr from the containers.

### See test names only:
```bash
cargo test --test integration_tests -- --list
```

---

## 7. Build and Test Everything

Complete verification workflow:

```bash
# 1. Build rootfs
./build-rootfs-docker.sh

# 2. Compile syscall test program
gcc -static test_syscalls.c -o test_syscalls
cp test_syscalls alpine-rootfs/bin/

# 3. Build the project
cargo build --all-targets

# 4. Run all tests
sudo -E cargo test

# 5. Run examples
sudo -E cargo run --example seccomp_demo
sudo -E cargo run --example secure_container

# 6. Verify everything
echo "✅ All tests and examples working!"
```

---

## 8. Quick One-Liner Tests

### Quick test everything:
```bash
./build-rootfs-docker.sh && sudo -E cargo test && sudo -E cargo run --example secure_container
```

This will:
1. Build the rootfs
2. Run all 44 tests (unit + integration + doc)
3. Run the comprehensive security demo

### Quick check (no rootfs rebuild):
```bash
sudo -E cargo test && sudo -E cargo run --example seccomp_demo
```

### Fast unit tests only (no root needed):
```bash
cargo test --lib
```

---

## Troubleshooting

### "alpine-rootfs not found":
```bash
./build-rootfs-docker.sh
# or
./build-rootfs-tarball.sh
```

### "Permission denied" / "Operation not permitted":
```bash
# Make sure you're using sudo
sudo -E cargo test --test integration_tests

# Check you're running as root
id
```

### "test_syscalls not found" in seccomp demo:
```bash
# Compile and copy the test program
gcc -static test_syscalls.c -o test_syscalls
cp test_syscalls alpine-rootfs/bin/

# Verify it's there
ls -lh alpine-rootfs/bin/test_syscalls
```

### Tests hang or timeout:
```bash
# Kill any stuck containers
sudo pkill -9 remora

# Clean up any leaked mounts
sudo umount alpine-rootfs/sys 2>/dev/null || true
sudo umount alpine-rootfs/proc 2>/dev/null || true
```

### Compilation errors:
```bash
# Clean and rebuild
cargo clean
cargo build --all-targets
```

### "Cannot execute binary file":
```bash
# Make sure test_syscalls is compiled for the right architecture
file test_syscalls
# Should show: ELF 64-bit LSB executable, x86-64, statically linked

# Recompile if needed
gcc -static test_syscalls.c -o test_syscalls
```

---

## Test Coverage Summary

### Security Features Tested:

| Feature | Unit Tests | Integration Tests | Examples |
|---------|-----------|-------------------|----------|
| Seccomp filtering | ✅ | ✅ (5 tests) | seccomp_demo |
| No-new-privileges | - | ✅ (1 test) | secure_container |
| Read-only rootfs | - | ✅ (1 test) | secure_container |
| Masked paths | - | ✅ (2 tests) | secure_container |
| Capabilities | ✅ | ✅ (2 tests) | - |
| Resource limits | - | ✅ (3 tests) | - |
| Namespaces | ✅ | ✅ (3 tests) | - |
| Mounts | - | ✅ (1 test) | - |
| UID/GID API | - | ✅ (1 test) | - |

**Total Coverage:** All Phase 1 security features tested ✅

---

## Expected Test Results

### All tests passing:
```
running 15 tests (unit)
test result: ok. 15 passed; 0 failed; 2 ignored

running 22 tests (integration)
test result: ok. 22 passed; 0 failed; 0 ignored

running 7 tests (doc)
test result: ok. 7 passed; 0 failed; 20 ignored

TOTAL: 44 tests passed ✅
```

### Security demos passing:
```
seccomp_demo:
- ✅ Normal syscalls work
- ✅ reboot() blocked with EPERM
- ✅ mount() blocked with EPERM
- ✅ No seccomp works

secure_container:
- ✅ Read-only rootfs enforced
- ✅ Mount blocked by seccomp
- ✅ Masked paths working
- ✅ All Phase 1 features active
```

---

## Performance Benchmarking

### Time the tests:
```bash
time sudo -E cargo test
```

**Expected time:**
- Unit tests: < 1 second
- Integration tests: 1-3 seconds
- Doc tests: < 1 second
- Total: ~3-5 seconds

### Profile container startup:
```bash
time sudo -E cargo run --example secure_container
```

**Expected:** < 1 second for full secure container startup

---

## CI/CD Integration

### GitHub Actions example:
```yaml
- name: Build rootfs
  run: ./build-rootfs-docker.sh

- name: Run tests
  run: sudo -E cargo test

- name: Run security demos
  run: |
    sudo -E cargo run --example seccomp_demo
    sudo -E cargo run --example secure_container
```

### Local pre-commit hook:
```bash
#!/bin/bash
# .git/hooks/pre-commit

echo "Running tests before commit..."
cargo test --lib
echo "✅ Tests passed"
```

---

## Continuous Testing During Development

### Watch mode (requires cargo-watch):
```bash
cargo install cargo-watch

# Auto-run unit tests on file changes
cargo watch -x "test --lib"

# Auto-run integration tests (needs sudo setup)
# NOT RECOMMENDED - integration tests need root
```

### Quick iteration loop:
```bash
# Edit code
vim src/container.rs

# Quick compile check
cargo check

# Run unit tests (fast)
cargo test --lib

# Run specific integration test
sudo -E cargo test --test integration_tests test_no_new_privileges

# Full verification when done
sudo -E cargo test
```

---

## Summary

**Minimal test suite:**
```bash
sudo -E cargo test
```

**Full verification:**
```bash
./build-rootfs-docker.sh && \
gcc -static test_syscalls.c -o test_syscalls && \
cp test_syscalls alpine-rootfs/bin/ && \
sudo -E cargo test && \
sudo -E cargo run --example seccomp_demo && \
sudo -E cargo run --example secure_container
```

**Expected result:** All 44 tests pass ✅

---

## Need Help?

- Tests failing? Check troubleshooting section above
- New to Rust testing? Read: https://doc.rust-lang.org/book/ch11-00-testing.html
- Need more details? See individual test source in `tests/integration_tests.rs`
- Questions? Check `README.md` and `PHASE1_COMPLETE.md`
