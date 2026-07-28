---
paths:
  - ".github/workflows/*.yml"
---

# Editing `.github/workflows/*.yml` — validate locally before pushing, don't wait for a CI round-trip

Both `ci.yml` and `release.yml` are hand-maintained YAML with real logic in them (matrix jobs,
`needs:` gating, conditional steps per `runner.os`, embedded PowerShell/bash) — a typo or wrong
input name here fails silently until a full CI run (minutes) or, worse, a tag-push release run
(#56: a structural gap in these exact files shipped a half-published release).

## `actionlint` catches most mistakes in seconds, no cargo/link needed

No local Rust build is required to validate a workflow file — `actionlint` (a standalone Go binary,
static analysis + embedded shellcheck) parses the YAML, checks `uses:`/`with:` against known action
schemas, validates expression syntax (`${{ }}`), and shellchecks every `run:` block. Not available
via `apt`/`snap` on this box; download the release binary directly (small, checksummed by GitHub's
own release infra, no build):

```bash
curl -sL https://github.com/rhysd/actionlint/releases/download/v1.7.7/actionlint_1.7.7_linux_amd64.tar.gz \
  -o /tmp/actionlint.tar.gz
tar xzf /tmp/actionlint.tar.gz -C /tmp actionlint && chmod +x /tmp/actionlint
/tmp/actionlint .github/workflows/*.yml
```

**Diff against the base commit before trusting the output** — a repo can have pre-existing,
unrelated findings (this repo does: two shellcheck quoting nits elsewhere in `ci.yml`, and
`softprops/action-gh-release@v1` flagged as an older action version). Run actionlint against the
PRE-change version too (`git show <base-sha>:.github/workflows/X.yml > /tmp/orig.yml`) and diff the
finding sets — only NEW findings are yours to fix.

## A YAML/logic fix is not proof the job actually WORKS — the runtime environment can still surprise you

actionlint (and `python3 -c "import yaml; yaml.safe_load(...)"` for pure syntax) only prove the
workflow is well-formed; they cannot predict runtime behavior inside a job (#56: a `cargo test`
step that is syntactically fine crashed with `STATUS_DLL_NOT_FOUND` on `windows-latest` in one job
but not an apparently-identical one — traced to a stale `actions/cache`-restored `target/` in the
failing job, not a workflow syntax problem). When adding a NEW execution step to an existing job,
prefer the narrowest command that proves what you actually need (e.g. `cargo test --no-run` proves
compile+link without ever executing the binary) over the broadest one, if the narrower one is
still sufficient for the goal — it sidesteps whole categories of runtime-environment risk.
