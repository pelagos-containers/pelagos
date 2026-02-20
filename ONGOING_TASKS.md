# Ongoing Tasks

## Current Task: Rootless E2E Test Script

**Status:** COMPLETE

Added `scripts/test-rootless.sh` — comprehensive E2E test script covering all rootless
phases via the CLI binary. 8 sections: core rootless, minimal /dev, multi-UID mapping,
cgroup delegation, loopback networking, pasta networking, filesystem features, and
integration test runner. Follows `scripts/test-dev.sh` pattern. Refuses root, skips
sections with missing prerequisites.

---

## Completed Phases

### Phase A+B: Storage Path Abstraction + Rootless Overlay (v0.2.0)
**RELEASED** — rootless image pull and container run with single-UID mapping.

### Phase D: Minimal `/dev` Setup
**COMPLETE** — tmpfs + safe devices replacing host /dev bind-mount.

### Phase C: Multi-UID Mapping via Subordinate Ranges
**COMPLETE** — `newuidmap`/`newgidmap` helpers with pipe+thread sync; auto-detects
subordinate ranges from `/etc/subuid` and `/etc/subgid`; falls back to single-UID
mapping when helpers unavailable.

### Phase E: Rootless Cgroup v2 Delegation
**COMPLETE** — direct cgroupfs writes under user's delegated cgroup scope.

---

## Previous Releases

### v0.1.0 — Initial Release
Full feature set: namespaces, seccomp, capabilities, cgroups v2, overlay, networking,
OCI image pull, container exec, OCI runtime compliance, interactive PTY.

---

## Planned (Deferred)

### AppArmor / SELinux — MAC Profile Support
Deferred: seccomp + capabilities + masked paths stack is solid. Revisit if there's demand.
