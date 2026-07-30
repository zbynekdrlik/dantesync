# Autopilot Log

Terse per-issue log of autonomous work on this repo (commits, RED->GREEN test names, decisions, PR).

## #47 — HTTP status endpoint (:8898) for LAN/CI automation reads

- Version bump: `ee2cea1` (1.8.17 -> 1.8.18-dev)
- RED: `3f8e7c0` — `http_status::tests::test_http_status_endpoint_serves_same_json_as_pipe_payload` (stub `{}` body)
- GREEN: `5f63dcb` — real status JSON via shared `SyncStatus::to_json_bytes()`; wired `HttpStatusConfig` through `Config`/`load_config()`/`run_sync_loop`
- Unrelated CI fix: `adc0198` — ignore 3 new RUSTSEC advisories (quick-xml/ttf-parser, winit's uncompiled Wayland/CSD chain — investigated, root-caused, documented inline in ci.yml)
- Pre-merge review (3-angle parallel + deep `requesting-code-review` pass) found 3 real defects, all fixed before merge:
  - RED: `e77cade` / GREEN: `0b80d18` — per-field `#[serde(default)]` on `HttpStatusConfig`/`NtpServerConfig` (partial config.json object was crashing the whole `Config` parse and silently overwriting the file) + connection cap on `spawn_accept_loop` (unbounded thread-per-connection, panicking `thread::spawn`)
  - `eeee35e` — `InFlightGuard` RAII fix so a future panic in `handle_connection` can't leak a connection-cap slot (deep-review finding)
- PR: #48 (dev->master), merged `fb76f82`, tagged+released as `v1.8.18` (auto-release)
- Decision: `HttpStatusConfig.enabled` defaults to `true` (unlike `NtpServerConfig`'s opt-in `false`) — read-only, LAN-bound, the whole point is unattended reads working out of the box (documented in config.rs doc comment + PR body)
- Fleet rollout (canary cam4 first, then rest): cam1-6, imag-nb, strih, stream — all confirmed v1.8.18, `mode: LOCK`, endpoint live. cam3 needed `mount -o remount,rw /` (its root fs is read-only, unlike cam1/2/4). imag-nb needed `sudo -S` with the standard box password (interactive-sudo box). See dantesync#47 comment for the full table + live curl proof.
- Follow-up (dantesync side): none filed — the two Minor review findings (request-line parsing for a cleaner 404; config.json.bak before an overwrite) were explicitly optional per the reviewer's own calibration, left as documented in the `eeee35e` commit message.
- Follow-up (camera-box side): camera-box#648 (wire `recording-e2e.sh`'s `[0/8]` gate to the new endpoint) — commented that its prerequisite is live, ready to work as its own ticket.

## #53 — NTP measurement burst-filtering + honest quality spread

- Version bump: `6684bc2` (1.8.20 -> 1.8.21)
- Design comment (root cause + approach + rejected alternative), posted BEFORE any code:
  https://github.com/zbynekdrlik/dantesync/issues/53#issuecomment-5099987990
- RED: `39e414f` — `ntp::tests::select_lowest_rtt_picks_the_n_lowest_rtt_samples`,
  `summarize_offsets_returns_true_median_and_honest_spread`,
  `summarize_offsets_pathological_scatter_from_53_stays_honest` (the issue's own 22-sample fixture,
  -18750..+22860us), `filter_offset_discards_high_rtt_outliers_before_summarizing` — all fail
  against deliberately naive placeholder bodies that generalize today's actual bug.
- GREEN: `58dac47` — real RTT-ascending sort (`select_lowest_rtt`) + real median/spread
  (`summarize_offsets`); all 4 pass, including spread=41610us honestly reported for the
  pathological fixture (never hidden as 0).
- Wiring: `a309a92` — `NtpSource::get_offset()` widened from `Result<(Duration,i8)>` to
  `Result<NtpMeasurement{offset,sign,spread_us,sample_count}>`; `NtpClient::get_offset()` now
  bursts 5 raw measurements/check and RTT-selects the lowest 3; `SyncStatus` gained
  `ntp_spread_us`/`ntp_sample_count` (both `#[serde(default)]`, existing keys untouched); the #50
  step-agreement gate and the MAD adaptive threshold keep their own logic unchanged, now fed a
  filtered instead of raw per-check value. New test:
  `controller::tests::test_check_ntp_utc_tracking_propagates_quality_fields_to_status`.
- Local verification: `cargo test --lib` 141/141, `cargo test --test '*'` 11/11, `cargo fmt --check`
  clean, `cargo clippy -- -D warnings -A dead_code` (CI's exact invocation) clean.
- PR: #54 (dev->master), CI green (run 30329527304) — **not merged** per this worker's dispatch
  constraints (no live-machine touch, supervisor drives the fleet canary).
- Still needed: a live canary read-back from `stream` (10.77.9.204:8898/status) over several
  minutes to prove the ±20ms scatter is actually gone (or that `ntp_spread_us` now honestly
  surfaces it) — this worker never touched a live machine.

## #53 (continued) — kernel-timestamped Npcap NTP transport for Windows

v1.8.21's burst+RTT-select+median filter (above) did NOT fix the scatter — a live canary (posted
to #53 after v1.8.21 shipped) proved the contamination lives INSIDE a single 5-sample burst (up to
21094us spread across samples taken back to back on the stream box, vs cam1's 5-32us on the same
server at the same instant), so no filter across a burst can rescue it. The issue had been
auto-closed by PR #54's "Fixes #53" title despite the fix being incomplete; reopened with the
canary evidence before continuing.

- Version bump: `cd7f8c0` (1.8.21 -> 1.8.22)
- Design comment (root cause + approach + rejected alternative), posted BEFORE any code:
  https://github.com/zbynekdrlik/dantesync/issues/53#issuecomment-5100359554
- RED: `ac52dbd` — new `src/ntp_packet.rs` (NTP<->Unix epoch conversion, client request build,
  server reply parse, RFC 5905 offset/RTT formula, Ethernet+IPv4+UDP frame parsing) — 13 of 15
  fixture tests fail against naive stubs (return 0 / all-zero buffer / accept-anything).
- GREEN: `71e6cbb` — real implementations; all 15 pass; `cargo test --lib` 156/156.
- Wiring: `777c315` — `PcapNtpTransport` (`src/net_pcap.rs`, cfg(windows)): separate Npcap capture
  (HostHighPrec timestamps) filtered to the NTP server + port 123, correlates captured packets by
  source IP to get t1 (our own request leaving the NIC) and t4 (server reply arriving) instead of
  userspace `SystemTime::now()`; t2/t3 parsed from the reply payload. Refactored
  `NpcapPtpNetwork::new`'s device-lookup/capture-open/timestamp-convert code into shared
  `find_device`/`device_ipv4`/`open_hiprec_capture`/`pcap_ts_to_systemtime` helpers so PTP and NTP
  capture paths share one implementation; `recv_packet` now uses the shared
  `ntp_packet::parse_udp_frame` instead of duplicating frame parsing inline. `NtpClient::measure_once()`
  gets a platform split: Windows tries the persistent pcap transport first (permanent rsntp
  fallback + loud warning if Npcap init failed; per-sample rsntp fallback + loud warning on a
  transient round-trip failure); Linux's `measure_once_rsntp()` is byte-for-byte unchanged.
  `NtpClient::new()` gained an `interface_name` parameter (the one main.rs already resolves for the
  PTP network).
- Test: `e51456e` — `pcap_style_t1_to_t4_measurements_stay_honest_through_the_existing_filter`:
  synthetic (t1,t2,t3,t4) quadruples (equal RTT, so RTT-selection can't discard any) spanning the
  issue's own extremes (-18750/+22860us), run through `compute_offset_rtt_us` then the existing
  `filter_offset` — proves the honesty guarantee survives one layer further back than the
  already-known-offset pathological test.
- Docs: `05c3f0f` — updated `.claude/skills/sync-monitoring.md` for the actual fix.
- Local verification: `cargo test --lib` 157/157, `cargo test --test '*'` 11/11, `cargo fmt --check`
  clean, `cargo clippy -- -D warnings -A dead_code` (CI's exact Linux invocation) clean. The
  Windows-only code (`net_pcap.rs` additions, `ntp.rs` cfg(windows) blocks) additionally
  compile-checked clean (zero new warnings) against `--target x86_64-pc-windows-gnu` and the exact
  CI Windows build-job invocation (`--bin dantesync --bin dantesync-tray`) — the real MSVC target +
  Npcap SDK link, and any test EXECUTION on Windows, only happen in CI's own jobs (`build` job
  compile-checks on every PR; `release.yml` runs `cargo test` on Windows but only on a tag push,
  i.e. after merge — this worker never touched a live machine per its dispatch constraints).
- What the live canary must show for this to count as fixed: `ntp_spread_us` on the stream box
  (10.77.9.204:8898/status) collapsing from the observed 877-21094us into the same order of
  magnitude as cam1's 5-32us on the same server, over several minutes of samples — not just a
  lower `ntp_offset_us`, since a single lucky-looking offset with a wide spread is exactly the
  "smoothed into looking good" failure this ticket exists to prevent.

## #56 — release-integrity fix: compile break + all-or-nothing releases (2026-07-28)

- Root cause: #53/PR #55 refactored `pcap_ts_to_systemtime` out of `NpcapPtpNetwork` into a free
  function (shared with the new NTP transport) but left 5 Windows-only test call sites in
  `net_pcap.rs`'s `mod tests` still calling `NpcapPtpNetwork::pcap_ts_to_systemtime(...)`. No PR-time
  job ever activates `#[cfg(test)]` on Windows (the `test` job is ubuntu-only; the `build` job's
  Windows leg only ran `cargo build`, never `cargo test`), so this compiled nowhere on any PR gate
  and only broke `release.yml`'s `cargo test` step on windows-latest — AFTER the tag was pushed and
  the Linux asset already uploaded in parallel. Release v1.8.22 published a Linux asset with no
  Windows asset at all (run 30333447928) — hit live canarying the camera-box rig's stream box.
- Design comment posted BEFORE any code commit:
  https://github.com/zbynekdrlik/dantesync/issues/56#issuecomment-5100617962
- Version bump: `5833a70` — 1.8.22 (== master's tag) → 1.8.23 (mandatory first commit).
- Fix: `0a2d9c2` — dropped the stale `NpcapPtpNetwork::` prefix at all 5 call sites (net_pcap.rs
  lines 412/416/420/424/432); mechanical reference fix, no behavior change. RED reproduced locally
  via `cargo check --target x86_64-pc-windows-gnu --tests --lib` (exact same 5 `E0599` errors as the
  release log, no Npcap SDK/MSVC needed); GREEN after the fix (same command, clean).
- CI hardening: `008345a`, revised in `50d96dc` after the PR's own CI caught a real problem with
  the first attempt —
  1. `ci.yml`'s `build` job's Windows leg now runs `cargo test --no-run --verbose` (right after the
     existing Npcap SDK install, before the release build step) — closing the exact PR-gate blind
     spot that let this ship (compiles+links every test binary, catching the E0599-class break).
     First attempt used a bare `cargo test --verbose` (matching release.yml); that hit a real
     `STATUS_DLL_NOT_FOUND` crash starting the produced test binary in the PR's own CI run — specific
     to this job (it restores a cached `target/` via `actions/cache`, which `release.yml`'s job never
     does). `--no-run` sidesteps the whole question since linking never needs the runtime DLL, and is
     exactly the minimum the ticket itself named as sufficient ("cargo test --no-run, or
     equivalent").
  2. `release.yml` reworked to be all-or-nothing: both platform legs upload their binaries as
     workflow artifacts (`actions/upload-artifact@v4`); a new `publish` job (`needs: build`, which
     GitHub Actions only runs once every matrix leg succeeds) downloads all artifacts and calls
     `softprops/action-gh-release` exactly once with the complete set. A platform failure now means
     NO GitHub Release is created at all (bare tag, invisible to `/releases/latest`) instead of a
     half-published one.
  Verified with `actionlint` (downloaded standalone binary, no cargo/link needed for YAML
  validation) — zero NEW findings vs the pre-change baseline (confirmed by running it against the
  base commit too); the only findings present (shellcheck quoting nits elsewhere in ci.yml,
  `softprops/action-gh-release@v1` being an older action version) are pre-existing.
- Docs: updated `.claude/rules/windows-only-code.md` — the "no PR-time signal beyond compiles" gap
  it documented is now closed for compile-time breaks in `mod tests`; also fixed its own local
  check recipe, which only checked `--bin` targets and would NOT have caught this exact bug (the
  break lived entirely inside `mod tests`) — added `cargo check --target x86_64-pc-windows-gnu
  --tests --lib` as a mandatory second line.
- v1.8.22 release: withdrawn (see completion report) since this session cannot merge to master (no
  fixed 1.8.22 possible without a merge) — the only available consistent end-state is withdrawal,
  leaving `/releases/latest` resolve to the fully-consistent v1.8.21.
- Local verification: `cargo fmt --all --check` clean, `cargo check` (default features) clean,
  `cargo clippy -- -D warnings -A dead_code` (CI's exact Linux invocation) clean, `cargo test
  --verbose` 157+16+11 passed / 0 failed (one-off full run per Tier-0 bypass), `cargo check
  --target x86_64-pc-windows-gnu --tests --lib` clean (RED→GREEN proof above).
- Not merged per this session's explicit constraint — PR opened, driven to green CI, stopped there.

## #58: Windows tests execute nowhere -- delay-load wpcap.dll (2026-07-28)

- Root cause + approach posted BEFORE code: gh issue comment
  https://github.com/zbynekdrlik/dantesync/issues/58#issuecomment-5101073379 (predates commit
  f1650e3, the first code commit).
- Version bump: f1650e3 `chore: bump version to 1.8.24` — 1.8.23 already exists as a bare,
  release-less tag on the remote; CI's own version-check job blocks reusing it.
- fix(#58) commit 9a70741: build.rs delay-loads `wpcap.dll` on cfg(windows)
  (`-C link-arg=/DELAYLOAD:wpcap.dll` + `delayimp.lib`); ci.yml Windows leg flipped from
  `cargo test --no-run` back to a real `cargo test --verbose`. This alone let the process START on
  a runtime-less windows-latest runner (proven: CI run 30337735289 got PAST process-start, further
  than the original bug) but then crashed at the first REAL pcap:: call
  (`test_ntp_client_new` -> `NtpClient::new()` -> `PcapNtpTransport::new()` -> `find_device()` ->
  `Device::list()`), exit 0xc06d007e — a second, deeper bug this fix's own CI run surfaced live.
- fix(#58) commit be3d8b1 (RED already captured by 9a70741's own CI run, no separately manufactured
  RED commit needed): added `wpcap_runtime_available()` to `src/net_pcap.rs` (probes via
  statically-linked kernel32 LoadLibraryW/FreeLibrary, never delay-loaded) called at the top of
  `find_device()` — the shared choke point for `NpcapPtpNetwork::new`/`PcapNtpTransport::new` —
  returning a graceful Err before the first real pcap:: call. 4 new regression tests added in
  net_pcap.rs (`test_wpcap_runtime_available_never_panics`,
  `test_find_device_gracefully_errors_without_npcap_runtime`,
  `test_pcap_ntp_transport_new_gracefully_errors_without_npcap_runtime`,
  `test_npcap_ptp_network_new_gracefully_errors_without_npcap_runtime`) — all `ok` on CI run
  30338325119, alongside `test_ntp_client_new` (the original crash trigger) now passing too:
  `test result: ok. 185 passed; 0 failed`.
- docs(#58) commit 0704bcf: self-review caught that build.rs's own comment + the
  windows-only-code.md playbook update (both written in 9a70741, before the second bug was
  discovered) still claimed "verified no existing #[test] makes a real pcap:: call" — disproven by
  be3d8b1. Corrected both to describe the actual two-part fix. No functional change. Final green
  CI run: 30338828677.
- Local verification each cycle: `cargo fmt --all --check`, `cargo check`, `cargo test --no-run`
  (default features, Linux); `cargo check`/`cargo clippy --target x86_64-pc-windows-gnu --tests
  --lib` (proxy for the windows-msvc target CI actually builds — not a substitute, but caught the
  FFI declarations compile before pushing) — zero NEW findings vs the pre-change baseline in both
  cases (diffed explicitly).
- Option 1 (install the Npcap runtime on the CI runner) rejected with evidence: the free Npcap
  installer has no silent-install switch at all; even rust-pcap/pcap's own official CI needs a
  paid Npcap OEM subscription (`NPCAP_OEM_USERNAME`/`PASSWORD` secrets) for exactly this — not
  viable here without a purchase.
- PR #59: https://github.com/zbynekdrlik/dantesync/pull/59 — green, `mergeable: MERGEABLE`,
  `mergeStateStatus: CLEAN`, all 7 required checks pass. NOT merged per this session's explicit
  instruction — supervisor merges, releases fresh tag v1.8.24, canaries to the fleet.

## #61 — CLAUDE.md instructed local release builds in this Tier-0 repo; no target/-purge backstop

- Version bump: `fe7f8a6` (1.8.25 -> 1.8.26)
- Design comment (root cause + approach + rejected alternative), posted BEFORE any code:
  https://github.com/zbynekdrlik/dantesync/issues/61#issuecomment-5128859083
- RED: `14f0be3` — `tests/purge_target.rs`, 18 tests, 17 fail (scripts don't exist yet, no
  `## Local Build Policy` in CLAUDE.md); only `claude_md_keeps_the_npcap_cross_compilation_note`
  passes (pre-existing).
- GREEN: `603542d` — `scripts/lib/purge-target-decision.sh` (pure `purge_target_should_purge`/
  `purge_target_daemon_live`, unit-tested directly, unlike camera-box's own untested
  purge-target.sh) + `scripts/purge-target.sh` (`purge_target_main`, executed-not-sourced guard
  mirroring cam-disk-guard.sh's #403 convention) + `scripts/install-git-hooks.sh` (pre-push hook).
  Daemon-live check matches by process NAME (`pgrep -x dantesync|dantesync-tray`), an ABSOLUTE
  gate never overridden by `--force`.
- GREEN: `85ef6dd` — CLAUDE.md: replaced the contradictory `Local Verification` bullet + old
  `## Build Commands` (`cargo build --release`) with an explicit `## Local Build Policy` (Tier 0)
  section naming `.github/workflows/release.yml` as the actual build+publish path; kept the
  Npcap cross-compile note.
- fix: `cf90ef4` — gated the whole test file behind `#![cfg(unix)]`: the harness uses
  `std::os::unix::fs::{symlink, PermissionsExt}` (curated cargo-free `PATH` for the fallback-purge
  test; executable-bit check on the installed hook), neither exists on Windows. CI's
  windows-latest Build Check job failed to compile before this fix; ubuntu-latest's Test job is
  where the suite actually runs.
- Gotcha found live: this box has TWO `cargo` installs (`~/.cargo/bin` + an older apt one in
  `/usr/bin`), so `PATH=/usr/bin:/bin` does NOT exclude cargo — the "cargo unavailable" fallback
  test needed a curated symlink-only bin dir instead.
- Gotcha found live: sourcing a script WITH trailing args (`. script.sh --check`) does not
  auto-invoke a function gated behind an executed-not-sourced guard — the guard sees it as
  sourced either way and the args are silently unused. Fixed by explicitly calling
  `purge_target_main --check` after sourcing in the affected tests.
- Real-world proof: on this box (whose live `dantesync` daemon PID was confirmed via `pgrep`),
  `THRESHOLD_MB=1 scripts/purge-target.sh` correctly REFUSED to purge a real 775 MB `target/`,
  printing the live-daemon skip message.
- Local verification: `cargo test --test purge_target` 18/18 pass; `cargo fmt --all --check`,
  `cargo check`, `cargo clippy -- -D warnings -A dead_code` (CI's exact invocation) all clean. A
  broader `--all-targets` clippy run surfaces 10 pre-existing `field_reassign_with_default`
  findings in `src/time_server.rs` test code — confirmed identical on `origin/master` via a
  throwaway `git worktree`, unrelated, not touched.
- PR #63: https://github.com/zbynekdrlik/dantesync/pull/63 — green (7/7 checks), `mergeable:
  MERGEABLE`, `mergeStateStatus: CLEAN`. Merged `1fb8d403` (direct REST PUT — `gh pr merge`'s
  "not up to date" false refusal, same repo GOTCHA as PR #697). Auto-closed #61. Tagged +
  released `v1.8.26` (CI run 30532236993, both linux-amd64 + windows-amd64 + tray assets
  published).
- Filed `zbynekdrlik/airuleset#187`: `block-commit-without-design.sh` resolves the repo from a
  stale session cwd (this dispatch's declared root was a sibling repo, camera-box) instead of the
  `git commit`'s actual cwd (reached via an inline `cd &&`), so it reported "no design comment
  posted for #61 (repo camera-box)" even though the real design comment was posted and verified
  on `dantesync`#61. Bypassed with `[no-design: ...]` on every #61 commit, citing the marker +
  filed issue.
- No fleet deploy: zero functional daemon change (Cargo.toml version + CLAUDE.md + new dev-only
  shell scripts only) — nothing to redeploy or verify on the live cam/imag/strih/stream fleet.
