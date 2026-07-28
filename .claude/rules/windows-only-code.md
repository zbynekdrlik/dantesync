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

## What PR CI actually does with this code (dantesync#53, gap closed by #56)

- `ci.yml`'s `test` job runs `cargo test --lib`/`--test '*'` **on `ubuntu-latest` only** — this job
  still never touches `#[cfg(windows)]` code at all.
- `ci.yml`'s `build` job compile-checks this code for real on `windows-latest` via
  `cargo build --release --bin dantesync --bin dantesync-tray` (with the Npcap SDK installed) — a
  genuine type/borrow/API error in `#[cfg(windows)]` **production** code fails a PR here. But
  `cargo build` never activates `#[cfg(test)]` code, so this alone proved nothing about the
  `mod tests` blocks in these same files.
- **Fixed in #56:** `ci.yml`'s `build` job's Windows leg now ALSO runs `cargo test --no-run
  --verbose` (right after the Npcap SDK install, before the release build step) — this compiles
  AND links every test binary (lib + every `#[cfg(test)] mod tests`) without executing any of
  them. This is what PR CI was missing: dantesync#53/PR #55 refactored `pcap_ts_to_systemtime` out
  of `NpcapPtpNetwork` into a free function but left 5 Windows-only test call sites calling the old
  associated-function path; that compile break passed every PR gate (nothing on a PR ever activated
  `#[cfg(test)]` on Windows) and only surfaced in `release.yml` **after** the tag was already
  pushed and the Linux asset already published in parallel (v1.8.22, run 30333447928: Linux asset
  live, Windows leg failed, no `.exe` at all). `--no-run` is the deliberate choice, not a bare
  `cargo test`: a bare `cargo test --verbose` was tried first here and hit a real
  `STATUS_DLL_NOT_FOUND` crash starting the produced test binary, specific to THIS job (it restores
  a cached `target/` via `actions/cache`, which `release.yml`'s job never does — `release.yml`'s own
  `cargo test --verbose` keeps working reliably in its cache-free environment). `--no-run` sidesteps
  that whole question: linking only needs the Npcap SDK's import library (already installed here),
  never the runtime DLL, so it is 100% deterministic regardless of what caused the execution crash.
  This still fully closes the gap for the class of bug that shipped v1.8.22 — a compile/link error
  in `mod tests` — which is exactly what PR CI was missing.
- `release.yml` itself is now **all-or-nothing** (#56): both platform legs upload their binaries as
  workflow artifacts instead of publishing directly; a single `publish` job (`needs: build`, which
  only runs once *every* matrix leg succeeds) creates the GitHub Release exactly once with the
  complete asset set. A Windows (or Linux) failure at release time now means NO release is created
  at all, rather than a half-published one — the previous v1.8.22 incident cannot recur even if a
  *different* class of Windows-only bug slips past the now-hardened PR gate above.

**Consequence:** the historic "no PR-time signal beyond compiles" caveat is closed for
*compile-time and link-time* breaks in `mod tests` — but PR CI still never EXECUTES a Windows-only
test (only `release.yml` does, after merge); a logic bug inside a Windows-only test body is still
only caught at release time. It is still worth pulling interesting *logic* into a plain
(non-`cfg`-gated) module wherever possible (see `src/ntp_packet.rs`, added in #53) so it gets real
Linux-CI execution too — the Windows PR job proves Windows-specific glue compiles and links; a
Linux-side plain-module test is what actually exercises the logic on every PR.

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
