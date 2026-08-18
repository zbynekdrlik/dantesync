# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Development Guidelines

- **FOCUSED APPLICATION:** This is NOT a general-purpose all-situations application. This is a highly focused app for our exact Dante audio network synchronization use case. Code that adds complexity for unused features should be removed, not kept "just in case."
- Act as senior Rust, Windows, hardware, and clock-skilled developer
- Use TDD approach and ensure all code has test coverage
- **Critical Self-Review:** Be skeptical of your own conclusions. Before assuming something "doesn't work" or "isn't supported":
  1. Search online documentation and GitHub issues for evidence
  2. Verify from multiple independent sources (official docs, GitHub, Stack Overflow)
  3. Never make assumptions about API behavior without documentation
  4. If debugging, confirm the actual cause before implementing workarounds
  5. When you find contradicting evidence to your assumption, acknowledge the error immediately
- **Study Open Source Code:** When using open source libraries/frameworks:
  1. Read the actual source code to understand internal behavior
  2. Don't rely solely on documentation - verify by reading implementation
  3. If something doesn't work as expected, investigate the source to find why
  4. Consider fixing issues in the library itself rather than working around them
- **No Circular Development:** Never give up on a promising approach after first struggles:
  1. If an approach should theoretically work, investigate WHY it doesn't instead of reverting
  2. Don't cycle between approaches (try A → fail → try B → fail → try A again)
  3. Commit to achieving the goal (e.g., Linux-level 50µs precision) - don't settle for inferior solutions
  4. If a library has issues, consider contributing fixes rather than abandoning it
- **HARD REQUIREMENT - Precision Target <50µs:**
  1. The ONLY acceptable precision for both Linux AND Windows is <50 microseconds
  2. NEVER accept, propose, or implement solutions with worse precision (100ms, 1ms, etc.)
  3. If current approach shows precision worse than 50µs, it is FAILING - do not present it as "working"
  4. Before trying new approaches, RESEARCH how other projects (PTPSync, Meinberg, ptpd) achieve <50µs on Windows
  5. Consult with user before switching approaches - do not endlessly iterate without results
- **Test Quality:** All code must have 100% test coverage with high-quality, complex E2E tests. Never put out a broken version. The `tests/simulation_e2e.rs` contains critical simulation tests that validate the servo and controller behavior under various conditions.
- **CI/CD Verification:** Wait until GitHub Actions CI/CD pipeline has successfully finished (green checkmark) before telling the user to update or run commands. Monitor `gh run view` until completion
- **Autonomous Deployment:** Install and verify updates on remote machines (Windows/Linux) listed in `TARGETS.md` using available tools (SSH, etc.)
- **STRICT CI - Fight Regressions:** CI is configured to be maximally strict to prevent regressions:
  1. **Version bump required**: CI blocks merging if Cargo.toml version matches an existing release tag
     - ALWAYS bump version before merging ANY change to master — including docs-only /
       process-only commits (e.g. an `autopilot-log.md` entry) with zero functional change. The
       gate checks the version string only; it does not know or care that your diff is "just
       docs" (#61 confirmed this live: a docs-only follow-up PR still needed its own bump and
       produced a real tagged release).
     - This prevents shipping different code under the same version number
  2. **Coverage threshold**: Minimum 60% project coverage, max 1% drop allowed, new code requires 80% coverage
  3. **Security audit**: Blocks CI on ANY security advisory (`cargo audit --deny warnings`)
     - Known transitive dependency advisories are explicitly ignored with `--ignore RUSTSEC-XXXX` after review
     - When adding ignores, document WHY it doesn't affect our code (e.g., we don't use the affected API)
     - NEW advisories will still fail CI - only reviewed/documented ones are ignored
  4. **All checks must pass**: Lint, tests, build (Linux + Windows), coverage, security - ALL must be green
  5. Never weaken CI checks. If a check fails, fix the code, don't disable the check
  6. When adding features, always add corresponding tests to maintain coverage

## Branch Policy

- **EXACTLY two branches:** `master` and `dev`. No other branches — no feature branches, no fix branches, no release branches.
- **All work happens on `dev`:** Commit directly to `dev`, then open a PR from `dev` to `master` when ready to release.
- **No direct pushes to master:** All changes to `master` must go through a PR merge from `dev`. Branch protection enforces this for all users including admins.
- **Never create additional branches.** If a branch other than `master` or `dev` exists, it is a mistake and should be deleted.

## Local Build Policy

**Tier 0 (default) — CI builds the release binaries; local checkouts run cheap checks only.**

`.github/workflows/release.yml` builds and publishes `dantesync-linux-amd64` and
`dantesync-windows-amd64.exe` (plus `dantesync-tray-windows-amd64.exe`) on every tag push, gated
all-or-nothing via `needs: build` (a failure on either platform skips `publish` entirely — no
release with a missing asset). `ci.yml`'s own `test`/`build` jobs already run the full test suite
and a real Linux+Windows compile on every PR. There is no reason to run a local release build or a
full local test suite — CI is what actually produces and verifies the shipped binaries.

Use `cargo check` and `cargo test`. Do NOT run `cargo build --release` locally — CI builds both
Linux and Windows. Run locally before every push:

```bash
cargo fmt --all --check
cargo check
cargo clippy -- -D warnings -A dead_code
cargo test --no-run
```

If you need to actually EXECUTE a test locally (e.g. to observe RED before GREEN on a bug fix),
run only that one targeted test binary (`cargo test --test <file>` or `cargo test --lib <module>`),
never the full suite and never `cargo build --release`.

**Why:** cargo's project-local `target/` has no garbage collection (rust-lang/cargo#5026) — every
incremental/profile/bin combination accumulates and is never auto-removed, so a local release
build (or a full `cargo test` run) regrows `target/` unbounded over time. `scripts/purge-target.sh`
(installed via `scripts/install-git-hooks.sh` as a `pre-push` hook) is the automated backstop: it
purges `target/` once it crosses a size budget, and always skips while the dantesync daemon/tray
is running.

**Windows cross-compilation** requires Npcap SDK 1.13+ with `LIB` env var set to `npcap-sdk/Lib/x64`.

## Playbook router

- Windows-only (`#[cfg(windows)]`) code — what CI actually verifies + a free local compile-check
  without the Npcap SDK → `.claude/rules/windows-only-code.md` (auto-loads on its `paths:`)
- Editing `.github/workflows/*.yml` — local `actionlint` validation, and why a workflow being
  syntactically valid doesn't prove a new step actually works at runtime → `.claude/rules/github-workflows.md`
  (auto-loads on its `paths:`)
- Shell-script testing (`scripts/**`) — the source-guard + curated-PATH conventions from #61's
  purge-target backstop, and a clippy-scope gotcha → `.claude/rules/shell-script-testing.md`
  (auto-loads on its `paths:`)
- Clock discipline + how to test a control loop (closed-loop mocks vs constant ones, MAD models
  jitter not a drift ramp, Instant vs SystemTime on a daemon that steps its own clock, the
  additive-only `/status` contract) → `.claude/rules/clock-discipline-and-testing.md`
- Adding a config key (the `serde_json` `IndexMut` startup-panic trap, serde defaults, flooring
  nonsense values) → `.claude/rules/config-migration.md`
- Multi-homed PTP receive interface selection (net.rs/net_pcap.rs/gm_filter.rs — pick the NIC on the
  trusted GM subnet, reuse the #53 selector, pure-logic-in-gm_filter vs Windows-glue split, overlap
  semantics + ambiguity guard) → `.claude/rules/multi-homed-interface-selection.md`

## GOTCHA — `gh pr edit --body-file`/`--body` fails with a GraphQL "Projects (classic)" error

`gh pr edit <N> --body-file <file>` on this repo fails with `GraphQL: Projects (classic) is being
deprecated... (repository.pullRequest.projectCards)` and exit code 1 — `gh`'s GraphQL mutation for
editing a PR fetches a legacy `projectCards` field even when you never touch project cards, and
this repo/org still has that field wired up. **The body is silently NOT updated** when this
happens — always re-read the body afterward to confirm, since the CLI's own error output alone
doesn't make the silent no-op obvious. The direct REST PATCH sidesteps the broken GraphQL response
and works every time:

```bash
gh api repos/zbynekdrlik/dantesync/pulls/<N> -X PATCH -F body=@/path/to/new-body.md
```

Same for `gh issue view <N>` (also fails the same way — use
`gh api repos/zbynekdrlik/dantesync/issues/<N> --jq '{title,body,state}'` instead).

**`gh issue comment <N> -F <file>` (POSTING a new comment) is unaffected** — confirmed working
live (#61) with no GraphQL error. Only editing/viewing an existing PR/issue body hits the
`projectCards` bug above; posting a fresh comment does not touch that field at all.

## GOTCHA — the design/validated/reviewed-comment gate's repo resolution ignores an in-command `cd`

The airuleset design-gate hook (`hooks/post-record-design-comment.sh`, external to this repo) that
classifies a posted `gh issue comment` as a design/validated/reviewed marker resolves which repo
the comment belongs to from the **session's own ambient working directory**, not from a `cd
<path> &&` prefix inside the SAME Bash command. A worker whose session launched in a sibling repo
(e.g. `camera-box`) and runs `cd /home/newlevel/devel/dantesync && gh issue comment 71 -F
body.md` — even though the comment genuinely posts to the right repo (confirmed by the returned
`.../dantesync/issues/71#issuecomment-...` URL) — has that comment silently classified against
the WRONG repo (or not at all), because the hook's own `cwd` metadata never followed the `cd`.
The commit-blocking gate (`hooks/block-commit-without-design.sh`) then fires as if no design
comment exists at all, even though one is genuinely posted and readable on the issue.

**This recurred across two separate work cycles** (issue 68's cycle, and issue 71's cycle) before
being promoted here — it is not a one-off. **Always pass `-R zbynekdrlik/dantesync` explicitly on
every `gh issue comment` call that posts a design/validated/reviewed marker**, regardless of
whether the session is already `cd`'d into this repo:

```bash
gh issue comment <N> -R zbynekdrlik/dantesync -F body.md
```

`-R` makes the hook use the explicit repo directly, bypassing the ambient-cwd resolution entirely.
If a commit is blocked despite a comment you're sure is posted, check
`ls ~/.claude/design-posted/ ~/.claude/validated-posted/ ~/.claude/reviewed-posted/ | grep
dantesync` for the missing marker, then simply repost the SAME comment with `-R` explicit (a
duplicate comment on the issue thread is a harmless cosmetic cost next to a stuck commit gate) —
each `gh issue comment` call only ever classifies its own invocation's LATEST fresh comment on
that issue, so post design/validated/reviewed comments as SEPARATE Bash calls too, never batched
together in one command with `&&`.

**The SAME family of gotcha hits `git commit` itself too (issue 80's cycle), with a different,
narrower fix.** `hooks/block-commit-without-design.sh`'s own `resolve_work_cwd()` DOES honor an
inline `cd <path> &&`/`cd <path>;` — but ONLY when it is the LITERAL FIRST statement of the SAME
Bash tool call the `git commit` itself runs in. A shape like:

```bash
cd /home/newlevel/devel/dantesync && git add file1 file2
git commit -m "$(cat <<'EOF'
...
EOF
)"
```

— i.e. `cd && git add` on one line, then a NEWLINE, then `git commit` on the next line — still
executes correctly (both commands genuinely run against the right repo; shell `cd` state persists
across newline-separated statements within one Bash call) but gets BLOCKED anyway: the hook's own
repo resolution apparently does not associate a `cd` that preceded an EARLIER, different statement
with a `git commit` appearing later in the same multi-line call. The fix is to chain the `cd`
directly through to the `git commit` invocation itself via literal `&&`, all on one shell line (a
heredoc's OWN body lines are part of the SAME statement, not new top-level ones, so they don't
break this):

```bash
cd /home/newlevel/devel/dantesync && git add file1 file2 && git commit -m "$(cat <<'EOF'
...
EOF
)"
```

If a commit is blocked and reports the WRONG repo name in its own error message (e.g. "no design
comment posted yet for #N (repo camera-box)" while working in dantesync), check `pwd` first — a
worker's session `cwd` can be a sibling repo's checkout even mid-session, and this shape is the
fix, not `-R` (which `git commit` has no equivalent flag for at all).

## GOTCHA — `block-ungated-issue-filing.sh`'s `-F <file>` resolution reads the RAW command text, a shell `$VAR` in the path never resolves

Cross-repo lesson from filing camera-box#1021 while working this issue (#83): the airuleset
`Scope-gate:`/`Dedup-checked:` enforcement hook (`hooks/block-ungated-issue-filing.sh`, external to
this repo, fires on ANY `gh issue create`/`gh api .../issues` call in ANY project) resolves a
`-F <file>` / `--body-file <file>` argument by reading the **literal, un-expanded text** of the
Bash tool call it receives — it never runs a real shell, so it cannot expand a variable. A command
like:

```bash
SCRATCH=/tmp/some/scratchpad
gh issue create -R owner/repo --title "..." -F "$SCRATCH/body.md"
```

makes the hook see the literal token `$SCRATCH/body.md` (dollar sign and all) as the file path —
not the shell-expanded absolute path — so `os.path.isabs()` on it is `False`, the hook tries
`os.path.join(cwd, "$SCRATCH/body.md")`, that path does not exist, the body resolves to `None`, and
the whole call BLOCKS with `criterion=none dedup="none"` **even when the body file genuinely exists
on disk with correct `Scope-gate:`/`Dedup-checked:` lines** — the block message gives no hint this
is the actual cause (it lists the same generic 7 possible reasons regardless).

**Fix: write the scratch body file (its own separate Bash call, per the global `gh-cli-recipes.md`
atomic-block gotcha) and then reference it in `gh issue create -F` by its LITERAL, fully-expanded
absolute path** — never a `$VAR` interpolation in that specific argument, even though the variable
resolves correctly for every OTHER purpose (the file write itself, `cat`, etc. all work fine since
those genuinely execute in a real shell). This is a narrower instance of the general "hook sees your
command's TEXT, not its runtime-expanded result" class of gotcha already documented above for the
design-gate's `cd` resolution.

## Hardware Constraints (CRITICAL)

**This project implements SOFTWARE-ONLY PTP frequency synchronization:**

- NONE of the target computers have NICs with hardware timestamping support
- Standard consumer/enterprise Ethernet NICs (Intel, Realtek) are used
- DO NOT waste time on approaches requiring hardware timestamping (SIO_TIMESTAMPING, PTP hardware clocks, etc.)
- The goal is to achieve <50µs precision using SOFTWARE timestamps only
- Linux achieves this with kernel-level SO_TIMESTAMPNS - Windows needs equivalent software approach
- Audinate/Dante drivers are NOT involved - we use standard Windows/Linux network stack

## Architecture Overview

DanteSync is a high-precision PTP (Precision Time Protocol) synchronization tool for Dante Audio networks. It implements PTPv1 over UDP multicast (ports 319/320) with a hybrid NTP+PTP approach.

### Core Components

- **`main.rs`** - Entry point, CLI parsing, Windows service logic, IPC server setup, and main sync loop orchestration
- **`controller.rs`** - `PtpController<C, N, S>` - Generic controller that coordinates PTP sync. Contains rate-based servo logic, mode transitions (ACQ→PROD→LOCK), and clock adjustments
- **`ptp.rs`** - PTPv1 packet parsing (headers, Sync bodies, FollowUp bodies)
- **`clock/mod.rs`** - `SystemClock` trait with platform-specific implementations:
  - `linux.rs` - Uses `adjtimex` for frequency adjustment
  - `windows.rs` - Uses `SetSystemTimeAdjustmentPrecise` API
- **`traits.rs`** - `NtpSource` and `PtpNetwork` traits (mockable for testing)
- **`config.rs`** - `SystemConfig`, `ServoConfig`, `FilterConfig` - tuning parameters with different defaults for Linux vs Windows
- **`net.rs`** / **`net_pcap.rs`** / **`net_winsock.rs`** - Network utilities (multicast, timestamping, platform-specific packet capture)
- **`ntp.rs`** - NTP client for UTC alignment
- **`status.rs`** - `SyncStatus` struct shared via IPC to tray app (includes `is_locked`, `smoothed_rate_ppm`, `mode`)

### CRITICAL: Dante Time vs UTC Time

**Dante PTP provides DEVICE UPTIME, not UTC time.** This is fundamental to the architecture:

- Dante grandmaster clock uses device uptime (time since power-on), NOT real UTC
- The PTP offset between local clock and Dante master is MEANINGLESS for absolute time
- PTP is used ONLY for **frequency synchronization** (making clocks tick at the same rate)
- NTP is used for **UTC phase alignment** (setting the correct absolute time)

**Dual-Source Architecture:**

1. **PTP (Dante)** → `adjust_frequency()` - controls clock tick rate
2. **NTP (UTC)** → `step_clock()` - periodically corrects absolute time

These operations are INDEPENDENT:

- `step_clock()` sets absolute time value (does NOT affect frequency)
- `adjust_frequency()` sets tick rate (does NOT affect absolute time)

**PTP stepping has been removed from the codebase** - stepping based on Dante offset would desync from UTC. Only NTP steps the clock via `check_ntp_utc_tracking()`.

### Sync Flow

1. NTP sync for initial coarse UTC alignment
2. Join PTP multicast groups (224.0.1.129 on ports 319/320)
3. Process Sync messages → store pending with receive timestamp
4. Match FollowUp messages → calculate phase offset from (T1, T2) pair
5. Lucky packet filter selects minimum offset from sample window
6. PI servo calculates frequency adjustment in PPM
7. Platform clock adjusts system frequency (PTP controls rate only)
8. Periodic NTP checks maintain UTC alignment (NTP controls absolute time)

### Key Design Patterns

- **Generic controller**: `PtpController<C: SystemClock, N: PtpNetwork, S: NtpSource>` allows dependency injection and mocking
- **Lucky packet filtering**: Selects minimum offset from N samples to filter network jitter
- **Platform abstraction**: `clock/mod.rs` re-exports `PlatformClock` based on target OS

### Configuration

Config file locations:

- Linux: `/etc/dantesync/config.json`
- Windows: `C:\ProgramData\DanteSync\config.json`

Key tunable parameters (in `config.rs`):

- Servo gains: `kp`, `ki` (reference only - controller uses adaptive gains)
- Filter settings: `sample_window_size`, `min_delta_ns`, `calibration_samples`, `warmup_secs`

### Binaries

- `dantesync` - Main sync daemon/service
- `dantesync-tray` - Windows tray application:
  - Dynamic icon with pulsing ring based on drift rate
  - Toast notifications for state transitions (lock/unlock/offline)
  - Service control (Restart/Stop) via menu
  - Reads status via named pipe IPC (`\\.\pipe\dantesync`)
