---
paths:
  - "src/net_pcap.rs"
  - "src/net_winsock.rs"
  - "src/clock/windows.rs"
  - "src/bin/tray.rs"
---

# Windows-only (`#[cfg(windows)]`) code — what CI actually verifies, and how to check more locally

`net_pcap.rs`, `net_winsock.rs`, `clock/windows.rs`, and the tray binary are gated behind
`#[cfg(windows)]` (module-level in `lib.rs`, or `[target.'cfg(windows)'.dependencies]` in
`Cargo.toml`). This means a plain `cargo check`/`cargo test`/`cargo clippy` on this Linux dev box
**skips this code entirely** — it isn't even parsed, let alone compiled or tested.

## What PR CI actually does with this code (dantesync#53 → #56 → #58)

- `ci.yml`'s `test` job runs `cargo test --lib`/`--test '*'` **on `ubuntu-latest` only** — this job
  still never touches `#[cfg(windows)]` code at all.
- `ci.yml`'s `build` job compile-checks this code for real on `windows-latest` via
  `cargo build --release --bin dantesync --bin dantesync-tray` (with the Npcap SDK installed) — a
  genuine type/borrow/API error in `#[cfg(windows)]` **production** code fails a PR here.
- **#56 (superseded by #58 below):** added `cargo test --no-run --verbose` to compile+link every
  Windows test binary without executing it, closing the specific compile/link-break class that
  shipped v1.8.22 (dantesync#53/PR #55's `pcap_ts_to_systemtime` regression, invisible to a
  `cargo build`-only check). At the time, the team believed a bare `cargo test` crashed
  (`STATUS_DLL_NOT_FOUND`) only in `ci.yml`'s own cache-restored environment, and that
  `release.yml`'s own `cargo test --verbose` "kept working reliably" — **that belief was wrong**
  (see #58 below), so `--no-run` only ever papered over a crash that was about to hit `release.yml`
  too.
- **#58 — root cause + real fix:** `pcap`'s `#[link(name = "wpcap")]` puts a hard PE import for
  `wpcap.dll` in every binary/test that transitively links `net_pcap.rs`
  (`PcapNtpTransport`/`NpcapPtpNetwork`, added by dantesync#53/PR #55). Every `windows-latest`
  runner only ever has the Npcap **SDK** (link-time `.lib` stubs) installed, never the Npcap
  **runtime** (`wpcap.dll`/`Packet.dll`) — GitHub-hosted runners don't ship it, and the free Npcap
  installer has no silent-install switch at all (only Npcap OEM, a paid subscription, supports
  silent CI install — confirmed via nmap/npcap's own docs/issues and rust-pcap/pcap's own official
  CI, which uses `NPCAP_OEM_USERNAME`/`PASSWORD` secrets for exactly this). So the OS loader refuses
  to even START the test process (`0xc0000135`/`STATUS_DLL_NOT_FOUND`) — this hit `release.yml`
  for real in run 30336428951 (tag v1.8.23), proving the old #56 comment's "cache-specific, release
  keeps working" theory false. **Fix:** `build.rs` now delay-loads `wpcap.dll` on Windows
  (`-C link-arg=/DELAYLOAD:wpcap.dll` + `delayimp.lib`), deferring DLL resolution to the first
  actual `pcap::` call. Verified no existing `#[test]` makes one (every `net_pcap.rs` test is pure
  arithmetic/logic; `Device::list()`/`Capture::from_device()` are only reached from production
  code), so `ci.yml`'s Windows leg is now back to a REAL `cargo test --verbose` — genuinely
  executing, not just compiling — and `release.yml`'s own `cargo test --verbose` step no longer
  crashes either.
- `release.yml` itself is **all-or-nothing** (#56): both platform legs upload their binaries as
  workflow artifacts instead of publishing directly; a single `publish` job (`needs: build`, which
  only runs once *every* matrix leg succeeds) creates the GitHub Release exactly once with the
  complete asset set. A Windows (or Linux) failure at release time now means NO release is created
  at all, rather than a half-published one.

**Consequence:** both compile/link breaks AND runtime/logic breaks in Windows-only `mod tests` are
now caught at PR time — `ci.yml`'s Windows leg genuinely executes every test (#58), not just
compiles it (#56's interim state). It is still worth pulling interesting *logic* into a plain
(non-`cfg`-gated) module wherever possible (see `src/ntp_packet.rs`, added in #53) so it gets real
Linux-CI execution too, with zero dependency on the Windows leg at all.

## Getting a REAL local compile-check without the Npcap SDK

`rustup target list --installed` on this box already includes `x86_64-pc-windows-gnu` (distinct
from the MSVC target CI actually builds/links against, `x86_64-pc-windows-msvc`). `cargo check`
against the GNU target does **not** need the Npcap SDK / `LIB` env var (that's only needed to
**link**, i.e. `cargo build`, against the real `.lib` files — `cargo check` never links) and will
catch real type/borrow/API errors in `#[cfg(windows)]` code that a Linux-only check silently skips:

```bash
cargo check --target x86_64-pc-windows-gnu --bin dantesync --bin dantesync-tray
cargo check --target x86_64-pc-windows-gnu --tests --lib   # ALSO check mod tests, not just bins
cargo clippy --target x86_64-pc-windows-gnu --bin dantesync --bin dantesync-tray -- -A dead_code
```

**The `--tests --lib` line matters on its own** — the original recipe here only checked `--bin`
targets, which would NOT have caught #56's `pcap_ts_to_systemtime` break (that error lived entirely
inside a `#[cfg(test)] mod tests` block, invisible to a bins-only check). Always run BOTH lines when
touching a file this rule is scoped to: bins-only would have shipped v1.8.22's regression again even
with this exact local-check habit already in place.

This is a `cargo check`/`cargo clippy` invocation, so it's allowed under this project's Tier-0
local-build policy without a bypass. It is NOT a substitute for the real MSVC build (different
target triple, and `pcap`/`windows-service`/`tray-icon`'s actual linking is still unverified) — but
it is free, fast, and catches the large majority of mistakes (wrong types, missing `?`, borrow
errors, wrong trait bounds) that would otherwise only surface after pushing and waiting for the
`build (windows-latest)` CI job.
