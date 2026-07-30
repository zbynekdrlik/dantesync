---
paths:
  - "scripts/**"
  - "tests/purge_target.rs"
  - "src/time_server.rs"
---

# Shell-script testing conventions (#61) + a clippy scope gotcha

`tests/purge_target.rs` is this repo's FIRST bash-script test file (previously only
`tests/simulation_e2e.rs` existed, no `scripts/lib/` convention). It ported camera-box's own
convention (source the script, call functions/`main` directly, an executed-not-sourced guard) —
follow the SAME shape for any future shell-script coverage rather than inventing a new style.

## Sourcing a script WITH trailing args does NOT auto-invoke a guarded main

A script written with the standard convention —

```bash
purge_target_main() { ... }
if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
    purge_target_main "$@"
fi
```

— correctly treats `. script.sh --check` (sourcing WITH args) as sourced either way: the guard
still sees `BASH_SOURCE[0] != $0`, so `purge_target_main` is never invoked and the trailing
`--check` is silently unused. This is not a bug in the script — it is `set -uo pipefail`, plain
bash semantics. A test that wants to exercise `main` after sourcing must explicitly call the
function: `. script.sh; purge_target_main --check`, not `. script.sh --check`.

## This box has TWO separate `cargo` installs — `PATH=/usr/bin:/bin` does NOT exclude cargo

`~/.cargo/bin/cargo` (the primary rustup toolchain) AND an older apt-installed `cargo` (1.75.0) in
`/usr/bin` both exist on dev1. A test that needs to exercise a "cargo unavailable" fallback branch
cannot just restrict `PATH` to `/usr/bin:/bin` — it will still find the apt cargo and take the
WRONG branch (confirmed live: the intended `rm -rf` fallback branch instead ran `cargo clean`,
which then failed with "could not find Cargo.toml" because the scratch dir wasn't a real cargo
project). Build a curated bin/ directory containing ONLY symlinks to the specific tools the script
needs (`dirname`, `git`, `du`, `cut`, `rm`, `sed`, `cat`, ...), deliberately excluding `cargo`, and
set `PATH` to just that directory — see `minimal_path_without_cargo()` in
`tests/purge_target.rs` for the pattern.

## CI's clippy invocation is narrower than camera-box's — do not add `--all-targets` here

`ci.yml`'s actual clippy step is `cargo clippy -- -D warnings -A dead_code` — NOT
`--all-targets`. Running `cargo clippy --all-targets -- -D warnings` locally (camera-box's own
convention, which this repo's `## Local Build Policy` deliberately does NOT copy verbatim)
surfaces 10 pre-existing `field_reassign_with_default` findings inside `src/time_server.rs`'s
OWN test code — confirmed identical on `origin/master` via a throwaway `git worktree`, unrelated
to whatever you're working on, and NOT what CI actually gates on. Use the exact command
`## Local Build Policy` documents (`cargo clippy -- -D warnings -A dead_code`) for a true signal
on whether YOUR change introduces a new clippy finding; don't chase the `--all-targets` extras as
your own regression.
