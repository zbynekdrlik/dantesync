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
