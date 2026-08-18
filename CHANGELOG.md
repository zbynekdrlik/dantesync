# Changelog

All notable changes to DanteSync will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.8.45] - 2026-08-18

### Added

- **Step-storm alarm on the fleet NTP master (issue #91).** When the PTP grandmaster goes
  unreachable, the master correctly falls out of genuine lock and `server_step_threshold_us`
  drops to the tight 200us threshold, which against a real oscillator-vs-UTC frequency error
  step-corrects UTC every ~10s check — 129–180 steps/h, live-observed on strih — and the whole
  NTP fleet chases each step (fleet-wide frame skips). Because the NTP loop cannot slew a real
  frequency error away (PTP owns frequency), no threshold change can remove this; the honest fix
  is to make the degradation LOUD. The master now tracks its own trailing-hour step count and,
  when a server-mode node exceeds `NTP_STEP_STORM_THRESHOLD_PER_HOUR` (120/h — comfortably above
  the ~84/h ceiling of healthy locked stepping at the worst-ever 66ppm GM rate), emits a loud,
  grep-able `[NTP][STEP-STORM]` warning (rate-limited) and sets `/status.ntp_step_storm`. The
  raw metric is published as `/status.ntp_steps_last_hour` (the "steps/h" health signal issue
  #67 asked for), both additive fields a dev1 watchdog can poll. This closes the 19h-silent gap
  that let the storm run unalerted. No step threshold was widened.

## [1.8.43] - 2026-08-16

### Fixed

- **A multi-homed box now attaches its PTP capture to the grandmaster's NIC (camera-box issue
  1073, second half).** The Windows PTP receive path inherited the OS default interface
  (`net::get_default_interface` → first non-wireless bindable NIC) for BOTH the pcap capture and the
  IGMP `224.0.1.129` join. On the dual-homed stream box (rig `Ethernet` `10.77.9.204` + mbc
  `Ethernet 2` `10.77.7.204`) that was the mbc NIC, so the box never joined the group on the rig NIC
  and never captured the rig grandmaster `10.77.9.184` — it only saw the foreign `10.77.7.x` PTP
  (correctly dropped by the 1.8.42 `gm_allowlist`, leaving the box in NTP fallback with no
  grandmaster at all). DanteSync now picks the capture/join interface whose subnet is on a trusted
  `gm_allowlist` prefix (`GmAllowlist::select_interface`), so both attach to the rig NIC and the box
  receives `10.77.9.184`. This mirrors the dual-homed interface selection already proven for the NTP
  transport in 1.8.x (`find_device_for_ntp_server`).

  **Backward compatible — no config change for anyone already working.** An empty/unrestricted
  `gm_allowlist` (the default), a `/0`-only allowlist, or a box with no interface on a trusted
  subnet keeps the historical default-interface behavior byte-for-byte, so single-homed boxes are
  unaffected. Only a **multi-homed** box **with** a restricting `gm_allowlist` whose default
  interface is not on the grandmaster's subnet changes behavior — its PTP capture/join moves to the
  grandmaster-subnet NIC. The Linux socket receive path is unchanged.

## [1.8.42] - 2026-08-16

### Fixed

- **A foreign-subnet grandmaster can no longer steal the PTP lock (camera-box issue 1073).** The
  PTP client had no best-master election: it adopted the source of the last Sync packet the capture
  saw (last-writer-wins), so a node that also sees a foreign subnet's PTP multicast could silently
  lock onto the wrong grandmaster. Live incident: the stream box (rig `10.77.9.x`) also sees mbc's
  `10.77.7.x` and locked onto `10.77.7.109` instead of the rig grandmaster `10.77.9.184`.

### Added

- **`system.gm_allowlist` — a trusted grandmaster-source allowlist.** Each entry is an exact IPv4
  (`"10.77.9.184"`) or a CIDR prefix (`"10.77.9.0/24"`); a PTP packet whose source IP is not
  permitted is dropped as-if it never arrived. **EMPTY (the default, and every existing config) =
  UNRESTRICTED — accept any source, unchanged behavior**, so a single-GM network needs no change.
  Parsing is fail-open (an all-invalid list degrades to unrestricted with a loud startup warning),
  and a valid-but-wrong allowlist is made diagnosable (a rate-limited drop warning, and the
  PTP-offline log distinguishes "grandmaster absent" from "grandmaster blocked by the allowlist").

  To restrict a box to the rig subnet, add to its `config.json` (Linux `/etc/dantesync/config.json`,
  Windows `C:\ProgramData\DanteSync\config.json`) and restart the service:

  ```json
  "system": { "gm_allowlist": ["10.77.9.0/24"] }
  ```

## [1.8.29] - 2026-08-11

### Fixed

- **The NTP master no longer free-runs from boot (#68).** In `ntp_server_mode` DanteSync synced to
  its upstream exactly ONCE at service start and then disabled periodic queries ("this machine IS
  the time source"). That is true of the fleet's mutual coherence and false of UTC: the clock is
  frequency-locked to the Dante grandmaster, whose rate is not UTC's, so the master's UTC phase
  error integrated from boot with nothing subtracting from it — 6-19 ppm measured on the live rig,
  i.e. ~21 ms nineteen minutes after a restart and 1.04 s over two days, with the whole fleet
  coherently following it. The master now keeps re-querying upstream and disciplines itself,
  reusing the same threshold + two-agreeing-samples + step machinery every client node uses.
  Restarting the service is no longer the remedy for drift.
- **A benign UDP reset no longer stalls the NTP server.** On Windows, `WSAECONNRESET` (os error
  10054) is raised routinely on a server socket when an earlier reply drew an ICMP
  port-unreachable. It was treated as an unexpected error — logged at error severity and followed
  by a 100 ms sleep during which the fleet's time source answered nobody. It is now classified as
  benign and skipped immediately, and on Windows `SIO_UDP_CONNRESET` is disabled at socket creation
  so the stack stops raising it at all.
- **`ntp_failed` reflects freshness, not only explicit query errors.** It previously had two
  writers, both inside the query path, so a node that had stopped querying reported `false`
  indefinitely. It is now also raised when no successful measurement lands within
  `system.ntp_stale_secs` — and going stale now also ARMS the query, so a node whose PTP never
  reaches lock keeps tracking UTC and recovers by itself instead of latching the alarm forever.
- **The boot-time NTP sync publishes its measurement,** and a corrected offset is published as the
  RESIDUAL. Neither happened before: nineteen minutes after a restart that measured +1.039 s and
  stepped the clock by it, `/status` still served `ntp_offset_us: 0, ntp_sample_count: 0`.
- **The served NTP reference timestamp tracks the last real upstream sync** (and is stamped with
  the measured UTC instant, so it can never advertise a reference in the future, which RFC 5905 has
  conforming clients discard).
- **A malformed `ntp_server_mode` in `config.json` no longer aborts startup** — the version
  migration checks the value is an object before indexing it.

### Added

- `/status` (and the named pipe) gain `ntp_updated_ts` (epoch second of the last successful NTP
  measurement, `0` = never) and `ntp_age_s` (seconds since; `null` = never measured). Additive
  only — no field renamed or removed, and pre-existing status JSON still parses. Grade these before
  trusting `ntp_offset_us`: `updated_ts` is written by the PTP loop and says nothing about the NTP
  fields beside it.
- `ntp_server_mode.max_step_us` (default 100 000 µs) — upper bound on a single server-mode UTC
  correction, so one wrong-but-consistent upstream reading cannot move the whole fleet at once.
  Never fires in steady state; a 1.04 s error is worked off over ~10 minutes unattended. `0` =
  unbounded. The boot-time sync is never bounded.
- `system.ntp_stale_secs` (default 180 s = 6× the query cadence, floored at one cadence) — the
  freshness window above.
- The NTP server runs supervised: an unexpected loop exit — including a panic — re-binds and
  restarts it, loudly, carrying its status source across.

### Note for operators

In server mode the master now takes small periodic steps it never took before (~0.7 ms every
~2 min at 6 ppm, ~1.7 ms every ~1.5 min at 19 ppm), and each propagates to the fleet one or two
client intervals later. That is the deliberate trade for UTC error that no longer grows without
limit; `max_step_us` is the lever if it ever matters.

## [1.8.18] - 2026-07-10

### Added
- HTTP status endpoint (`GET http://<host>:8898/status`), bound to the LAN interface, serving
  the same status JSON the named pipe (`\\.\pipe\dantesync`) already emits — lets CI/automation
  read PTP/NTP lock status over the network without a human or an SMB/pipe bridge (#47). Enabled
  by default; configurable via `http_status.enabled` / `http_status.port` in `config.json`. The
  named pipe is unchanged.

## [1.8.0] - 2025-12-28

### Changed
- **BREAKING:** Renamed project from "DanteTimeSync" to "DanteSync"
  - Main binary: `dantetimesync` → `dantesync`
  - Tray binary: `dantetray` → `dantesync-tray`
  - Windows service: `dantetimesync` → `dantesync`
  - Install directory: `C:\Program Files\DanteTimeSync` → `C:\Program Files\DanteSync`
  - Config directory: `C:\ProgramData\DanteTimeSync` → `C:\ProgramData\DanteSync`
  - Linux config: `/etc/dantetimesync` → `/etc/dantesync`
  - Named pipe: `\\.\pipe\dantetimesync` → `\\.\pipe\dantesync`
  - GitHub repository: `zbynekdrlik/dantetimesync` → `zbynekdrlik/dantesync`

## [1.7.5] - 2025-12-28

### Changed
- Installer now shows version from GitHub release (single source of truth: Cargo.toml)
- Added CI check to prevent hardcoded versions in scripts

## [1.7.4] - 2025-12-28

### Fixed
- Removed outdated hardcoded version from installer script (was showing v1.6.2)

## [1.7.3] - 2025-12-28

### Changed
- Version bump to test update badge notification feature

## [1.7.2] - 2025-12-28

### Added
- Update badge on tray icon: orange dot in corner when new version available
- Start Menu shortcut: "DanteSync" now appears in Windows Start Menu for easy access

### Fixed
- Tray icon now visually indicates when update is available (persistent badge)

## [1.7.1] - 2025-12-28

### Fixed
- Tray menu now shows "Start Service" when service is stopped (was always showing "Stop Service")
- Restart Service menu item is disabled when service is already stopped

## [1.7.0] - 2025-12-28

### Added
- Automatic update check: tray app periodically checks GitHub for new versions (every 6 hours)
- Update notification: toast notification when new version is available
- Upgrade menu item: one-click upgrade via PowerShell IRM from tray menu
- Version comparison logic to detect newer releases

### Dependencies
- Added reqwest HTTP client for GitHub API communication

## [1.6.4] - 2025-12-27

### Fixed
- Jitter estimator now persists across NTP step corrections (was incorrectly clearing on each step)

## [1.6.3] - 2025-12-27

### Added
- Adaptive jitter smoothing for high-jitter systems (Realtek NICs, Hyper-V hosts)
- JitterEstimator measures stddev of drift rate over 30-sample window
- Dynamic EMA alpha: 0.3 for low jitter (<2 µs/s) → 0.1 for high jitter (>8 µs/s)
- Jitter logging every 50 samples when adaptive smoothing is active

### Changed
- EMA alpha now adapts based on measured jitter level instead of fixed value

## [1.5.6] - 2025-12-26

### Added
- PTP offline detection: graceful fallback to NTP-only sync when PTP masters are unavailable
- Orange tray icon for NTP-only mode (PTP offline)
- Toast notifications for PTP offline/restored transitions
- NTP failure tracking with tray notifications when NTP server is unreachable
- Windows Add/Remove Programs registration in installer
- Unit tests for PTP offline detection

### Changed
- Tightened NTP step threshold from 2000µs to 500µs for better UTC alignment

### Fixed
- Application no longer hangs when PTP Dante masters are switched off

## [1.5.5] - 2024-12-24

### Added
- Grandmaster switch detection with sync source tracking
- Soft reset on grandmaster switch to preserve learned frequency
- Unit tests for sync source change detection
- CI coverage reporting with Codecov integration
- Npcap SDK checksum verification in CI
- Unit tests for net.rs (interface selection, socket binding, wireless detection)
- Unit tests for clock/windows.rs (PPM conversion math, adjustment calculation)
- Unit tests for net_pcap.rs (timestamp conversion, packet structure validation)
- Unit tests for net_winsock.rs (QPC math, control message parsing, constants)

### Changed
- Improved installer version handling (dynamic extraction from binary)
- Increased codecov patch target to 70%

### Fixed
- RwLock poison handling in IPC server (prevents service crash)
- UTF-16 allocations moved outside IPC loop (performance improvement)
- Tray icon ghost on exit

## [1.5.4] - 2024-12-23

### Added
- NANO mode exit hysteresis (require 5 consecutive samples above threshold)

## [1.5.3] - 2024-12-23

### Fixed
- was_nano initialization in tray app

## [1.5.2] - 2024-12-23

### Added
- NANO mode cyan icon and notification to tray app

## [1.5.1] - 2024-12-23

### Changed
- NANO mode: show drift in nanoseconds, lower entry threshold

## [1.5.0] - 2024-12-23

### Added
- NANO mode for ultra-precise sub-microsecond systems
- Single-instance check to dantetray

## [1.4.8] - 2024-12-22

### Added
- BPF filter for PTP packets to reduce DVS conflict

## [1.4.7] - 2024-12-22

### Fixed
- Disabled promiscuous mode to fix DVS coexistence

## [1.4.6] - 2024-12-22

### Changed
- Faster ACQ mode, cleaner Windows logs
- Disable W32Time in installer

## [1.4.5] - 2024-12-22

### Changed
- Move FreqMeasure warning to debug level

## [1.4.4] - 2024-12-22

### Changed
- Unified logs, faster ACQ, show version in install.sh

## [1.4.3] - 2024-12-22

### Changed
- Simplified config and fixed service control
