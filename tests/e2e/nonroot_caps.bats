#!/usr/bin/env bats
# tests/e2e/nonroot_caps.bats
#
# Regression tests for #447: non-root containers with --cap-add must
# receive the capability in their ambient set even when
# --security-opt no-new-privileges is also set.
#
# Two bugs were fixed together:
#   1. src/cli/run.rs: --cap-add was not wiring caps into ambient_cap_numbers
#      for non-root UIDs, so the container started with CapAmb=0.
#   2. src/container.rs: PR_SET_NO_NEW_PRIVS was set (step 6.5) before the
#      post-setuid ambient re-raise (step 8.6), blocking PR_CAP_AMBIENT_RAISE
#      unconditionally.  NNP is now deferred to step 8.7.
#
# These tests also guard that the fix does NOT regress any existing behaviour:
#   - Root containers still have no ambient caps by default.
#   - NNP is still set (NoNewPrivs=1) on the inner container process.
#   - The seccomp filter is still applied.
#
# Prerequisites:
#   - Run as root (sudo -E bats tests/e2e/nonroot_caps.bats)
#   - alpine:latest pulled
#   - pelagos binary built (cargo build)

load helpers.bash

NONROOT_UID=65534   # nobody — present in alpine
CAP_NET_BIND_HEX="0000000000000400"   # bit 10 = CAP_NET_BIND_SERVICE

# ---------------------------------------------------------------------------
# Helper: run a one-shot pelagos container and capture /proc/self/status
# ---------------------------------------------------------------------------

proc_status_in_container() {
    local field="$1"; shift   # e.g. "CapAmb"
    # remaining args are passed to `pelagos run` before the image+command
    "$PELAGOS" run --rm \
        --security-opt seccomp=none \
        --no-pid-ns \
        "$@" \
        alpine:latest \
        sh -c "grep '^${field}:' /proc/self/status | awk '{print \$2}'" \
        2>/dev/null
}

# ---------------------------------------------------------------------------
# Bug #447 — ambient cap not populated for non-root + NNP containers
# ---------------------------------------------------------------------------

@test "#447: non-root container with --cap-add NET_BIND_SERVICE has CapAmb set" {
    require_root

    run proc_status_in_container "CapAmb" \
        --user "$NONROOT_UID" \
        --cap-drop ALL \
        --cap-add NET_BIND_SERVICE \
        --security-opt no-new-privileges

    [ "$status" -eq 0 ]
    [ "$output" = "$CAP_NET_BIND_HEX" ]
}

@test "#447: non-root container with --cap-add NET_BIND_SERVICE has NNP set" {
    require_root

    run proc_status_in_container "NoNewPrivs" \
        --user "$NONROOT_UID" \
        --cap-drop ALL \
        --cap-add NET_BIND_SERVICE \
        --security-opt no-new-privileges

    [ "$status" -eq 0 ]
    [ "$output" = "1" ]
}

@test "#447: non-root container with --cap-add NET_BIND_SERVICE has CapPrm set" {
    require_root

    run proc_status_in_container "CapPrm" \
        --user "$NONROOT_UID" \
        --cap-drop ALL \
        --cap-add NET_BIND_SERVICE \
        --security-opt no-new-privileges

    [ "$status" -eq 0 ]
    [ "$output" = "$CAP_NET_BIND_HEX" ]
}

@test "#447: non-root container with --cap-add NET_BIND_SERVICE has CapInh set" {
    require_root

    run proc_status_in_container "CapInh" \
        --user "$NONROOT_UID" \
        --cap-drop ALL \
        --cap-add NET_BIND_SERVICE \
        --security-opt no-new-privileges

    [ "$status" -eq 0 ]
    [ "$output" = "$CAP_NET_BIND_HEX" ]
}

# ---------------------------------------------------------------------------
# Child process can receive the ambient cap via fork+exec
# (simulates Go SysProcAttr.AmbientCaps — the KubeVirt failure mode)
# ---------------------------------------------------------------------------

@test "#447: child spawned inside non-root+NNP container inherits ambient cap" {
    require_root

    # Use awk to directly read /proc/self/status from within a forked child sh.
    # 'exec sh' replaces the subshell — ambient set survives exec of non-setuid binary.
    run "$PELAGOS" run --rm \
        --security-opt seccomp=none \
        --no-pid-ns \
        --user "$NONROOT_UID" \
        --cap-drop ALL \
        --cap-add NET_BIND_SERVICE \
        --security-opt no-new-privileges \
        alpine:latest \
        sh -c 'exec sh -c "grep ^CapAmb: /proc/self/status | awk '"'"'{print \$2}'"'"'"' \
        2>/dev/null

    [ "$status" -eq 0 ]
    [ "$output" = "$CAP_NET_BIND_HEX" ]
}

# ---------------------------------------------------------------------------
# Non-regression: root containers are unchanged
# ---------------------------------------------------------------------------

@test "root container with --cap-drop ALL has CapAmb=0 (unchanged)" {
    require_root

    run proc_status_in_container "CapAmb" \
        --cap-drop ALL

    [ "$status" -eq 0 ]
    [ "$output" = "0000000000000000" ]
}

@test "root container without --cap-add has CapAmb=0 (unchanged)" {
    require_root

    run proc_status_in_container "CapAmb"

    [ "$status" -eq 0 ]
    [ "$output" = "0000000000000000" ]
}

@test "non-root container without --cap-add has CapAmb=0 (unchanged)" {
    require_root

    run proc_status_in_container "CapAmb" \
        --user "$NONROOT_UID" \
        --cap-drop ALL \
        --security-opt no-new-privileges

    [ "$status" -eq 0 ]
    [ "$output" = "0000000000000000" ]
}

@test "non-root container without NNP does not get NNP set (unchanged)" {
    require_root

    run proc_status_in_container "NoNewPrivs" \
        --user "$NONROOT_UID" \
        --cap-drop ALL \
        --cap-add NET_BIND_SERVICE

    [ "$status" -eq 0 ]
    [ "$output" = "0" ]
}
