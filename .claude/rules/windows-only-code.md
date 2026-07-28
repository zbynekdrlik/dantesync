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

## What PR CI actually does with this code (dantesync#53)

- `ci.yml`'s `test` job runs `cargo test --lib`/`--test '*'` **on `ubuntu-latest` only** — Windows
  test EXECUTION never happens on a PR.
- `ci.yml`'s `build` job DOES compile-check this code for real, on `windows-latest`, via
  `cargo build --release --bin dantesync --bin dantesync-tray` (with the Npcap SDK installed) — so
  a genuine type error / borrow error / missing-API error in `#[cfg(windows)]` code WILL fail a PR.
  But this only proves it **compiles**, not that it behaves correctly.
- `release.yml`'s Windows job runs `cargo test --verbose` (default features, so this code's own
  `#[cfg(test)]` blocks execute) — but **only on a tag push**, i.e. AFTER a PR has already merged.
  A logic bug in a Windows-only unit test is invisible until release time.

**Consequence:** treat any change here with EXTRA manual review rigor before pushing — there is no
PR-time signal beyond "it compiles" for anything Windows-specific. Pull the actual logic out into a
plain (non-`cfg`-gated) module wherever possible (see `src/ntp_packet.rs`, added in #53) so the
interesting behavior gets real Linux-CI test coverage, and keep the `#[cfg(windows)]` file itself
as thin OS/API glue.

## Getting a REAL local compile-check without the Npcap SDK

`rustup target list --installed` on this box already includes `x86_64-pc-windows-gnu` (distinct
from the MSVC target CI actually builds/links against, `x86_64-pc-windows-msvc`). `cargo check`
against the GNU target does **not** need the Npcap SDK / `LIB` env var (that's only needed to
**link**, i.e. `cargo build`, against the real `.lib` files — `cargo check` never links) and will
catch real type/borrow/API errors in `#[cfg(windows)]` code that a Linux-only check silently skips:

```bash
cargo check --target x86_64-pc-windows-gnu --bin dantesync --bin dantesync-tray
cargo clippy --target x86_64-pc-windows-gnu --bin dantesync --bin dantesync-tray -- -A dead_code
```

This is a `cargo check`/`cargo clippy` invocation, so it's allowed under this project's Tier-0
local-build policy without a bypass. It is NOT a substitute for the real MSVC build (different
target triple, and `pcap`/`windows-service`/`tray-icon`'s actual linking is still unverified) — but
it is free, fast, and catches the large majority of mistakes (wrong types, missing `?`, borrow
errors, wrong trait bounds) that would otherwise only surface after pushing and waiting for the
`build (windows-latest)` CI job.
