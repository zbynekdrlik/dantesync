#!/usr/bin/env bash
# airuleset:script-ok sourced library (not executed directly) — deliberately does NOT set -e so
# sourcing it never changes the CALLER's shell options (matches camera-box's
# scripts/lib/disk-guard-thresholds.sh and scripts/imag-host.sh convention for pure, sourced-only
# function libraries).
#
# scripts/lib/purge-target-decision.sh — pure/testable decisions for purge-target.sh (#61).
#
# No filesystem writes, no `cargo` invocation, no SSH — just the two decisions that gate a purge:
#   1. purge_target_should_purge <size_mb> <threshold_mb> — is target/ over budget?
#   2. purge_target_daemon_live                            — is a live sync running right now?
# This mirrors camera-box's scripts/lib/disk-guard-thresholds.sh split (#369): the policy that
# decides "purge or not" lives in ONE place, sourced by purge-target.sh, and is unit-tested
# directly (tests/purge_target.rs) instead of being buried inline in the pre-push hook where a
# future edit could silently change the threshold/safety behaviour with nothing to catch it.

# Process names (comm) that indicate a live dantesync sync — never purge target/ while one of
# these is running. Matched by PROCESS NAME via `pgrep -x`, never a full-cmdline match, so this
# script itself, an editor, or a grep merely mentioning "dantesync" can never false-positive it
# (same discipline as camera-box's probe-binary purge guard).
PURGE_TARGET_DAEMON_COMM="${PURGE_TARGET_DAEMON_COMM:-dantesync|dantesync-tray}"

# purge_target_should_purge <size_mb> <threshold_mb>
# Pure: no I/O, no side effects. Returns 0 (true — should purge) when size_mb is STRICTLY greater
# than threshold_mb, 1 (false — under budget) otherwise (including when equal).
purge_target_should_purge() {
    local size_mb="${1:-0}"
    local threshold_mb="${2:-4096}"
    [ "${size_mb}" -gt "${threshold_mb}" ]
}

# purge_target_daemon_live
# Returns 0 (true — a live sync is running) when a process named dantesync or dantesync-tray is
# currently running, 1 (false) otherwise. Purging target/ while the daemon (or its Windows tray
# companion) is running/being measured is never acceptable — this is the safety gate.
purge_target_daemon_live() {
    pgrep -x "${PURGE_TARGET_DAEMON_COMM}" >/dev/null 2>&1
}
