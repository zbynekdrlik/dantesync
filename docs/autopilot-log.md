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
