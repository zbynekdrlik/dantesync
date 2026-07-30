#!/usr/bin/env bash
# purge-target.sh — bound the local Cargo target/ so it stops regrowing unbounded (#61).
# See the extended header below `set -euo pipefail` for the full rationale + usage.

set -euo pipefail

# WHY (#61): dantesync is Tier-0 — CI's release workflow (.github/workflows/release.yml) builds
# and publishes dantesync-linux-amd64 + dantesync-windows-amd64.exe (+ tray) on tag push, gated
# all-or-nothing (needs: build); locally we run only cheap checks (cargo check / clippy / test
# --no-run). But cargo's project-local target/ has NO garbage collection (rust-lang/cargo#5026):
# every incremental/profile/bin combination leaves artifacts that are never auto-removed. The
# ROOT fix is CLAUDE.md's Local Build Policy (cheap checks only, no release build locally); this
# script is the BACKSTOP that resets target/ before it grows unbounded even if a session strays,
# mirroring camera-box's scripts/purge-target.sh (#185).
#
# Usage:
#   scripts/purge-target.sh              # purge if target/ > THRESHOLD_MB (default 4096 = 4 GB)
#   THRESHOLD_MB=2048 scripts/purge-target.sh
#   scripts/purge-target.sh --force      # purge regardless of size
#   scripts/purge-target.sh --check      # report size only, never purge (exit 0)
#
# SAFETY: NEVER purges while the dantesync daemon or its Windows tray companion is running — this
# repo's own binary is a live PTP sync process; purging out from under a running measurement is
# not acceptable. See scripts/lib/purge-target-decision.sh for the pure decision functions
# (unit-tested by tests/purge_target.rs).
#
# Run from anywhere — repo root is resolved via `git rev-parse --show-toplevel`.

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/lib/purge-target-decision.sh
. "$HERE/lib/purge-target-decision.sh"

# purge_target_main <args...> — the whole flow, as a function so it can be sourced (tests) without
# running (source-guard below), same convention as camera-box's cam-disk-guard.sh (#403).
purge_target_main() {
    local threshold_mb="${THRESHOLD_MB:-4096}"
    local force=0
    local check_only=0

    for arg in "$@"; do
        case "$arg" in
            --force) force=1 ;;
            --check) check_only=1 ;;
            -h|--help)
                sed -n '2,22p' "$HERE/purge-target.sh" | sed 's/^# \{0,1\}//'
                return 0
                ;;
            *)
                echo "purge-target.sh: unknown arg '$arg'" >&2
                return 2
                ;;
        esac
    done

    local repo_root target_dir
    repo_root="$(git -C "$HERE" rev-parse --show-toplevel 2>/dev/null || (cd "$HERE/.." && pwd))"
    target_dir="$repo_root/target"

    if [ ! -d "$target_dir" ]; then
        echo "purge-target.sh: no target/ — nothing to do."
        return 0
    fi

    local size_mb
    size_mb="$(du -sm "$target_dir" 2>/dev/null | cut -f1)"
    size_mb="${size_mb:-0}"
    echo "purge-target.sh: target/ = ${size_mb} MB (budget ${threshold_mb} MB)"

    if [ "$check_only" -eq 1 ]; then
        return 0
    fi

    if purge_target_daemon_live; then
        echo "purge-target.sh: dantesync daemon/tray is RUNNING (live sync) — skipping purge (safety)." >&2
        return 0
    fi

    if [ "$force" -eq 0 ] && ! purge_target_should_purge "$size_mb" "$threshold_mb"; then
        echo "purge-target.sh: under budget — no purge needed."
        return 0
    fi

    echo "purge-target.sh: purging target/ (${size_mb} MB) — CI rebuilds; cheap checks recompile fast."
    if command -v cargo >/dev/null 2>&1; then
        ( cd "$repo_root" && cargo clean )
    else
        rm -rf "$target_dir"
    fi
    echo "purge-target.sh: done — target/ reset."
}

# Run main ONLY when executed, not when sourced — so purge_target_main is unit-testable
# (mirrors camera-box's cam-disk-guard.sh #403 convention).
if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
    purge_target_main "$@"
fi
