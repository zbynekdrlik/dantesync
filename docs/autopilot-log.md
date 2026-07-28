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
- CI hardening: `008345a`, revised in `<pending>` after the PR's own CI caught a real problem with
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
