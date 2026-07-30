//! #61 — CLAUDE.md instructed local release builds in this Tier-0 repo, and there was no
//! target/-purge backstop (no scripts/lib/, no pre-push hook — .git/hooks/ held only the default
//! *.sample files). This locks the fix in three parts:
//!
//!   1. `scripts/lib/purge-target-decision.sh` — the PURE purge decision (over budget? daemon
//!      live?), sourced + unit-tested directly (mirrors camera-box's
//!      scripts/lib/disk-guard-thresholds.sh + its
//!      appliance_boot_hardening.rs::disk_guard_threshold_decision_correct test).
//!   2. `scripts/purge-target.sh` + `scripts/install-git-hooks.sh` — the backstop itself, wired
//!      through a real `pre-push` hook (mirrors camera-box's #185 fix).
//!   3. `CLAUDE.md`'s `## Local Build Policy` section — the root fix: an explicit Tier-0 policy
//!      replacing the old contradictory "prioritize local cargo build" guidance.
//!
//! The dantesync daemon is a LIVE PTP sync process on real deployments (confirmed running on
//! dev1 while writing this) — every test below that needs a "daemon not live" scenario stubs
//! `pgrep` explicitly rather than relying on the real environment's process table.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn lib_script() -> PathBuf {
    let p = manifest_dir().join("scripts/lib/purge-target-decision.sh");
    assert!(p.exists(), "{} not found", p.display());
    p
}

fn purge_script() -> PathBuf {
    let p = manifest_dir().join("scripts/purge-target.sh");
    assert!(p.exists(), "{} not found", p.display());
    p
}

fn installer_script() -> PathBuf {
    let p = manifest_dir().join("scripts/install-git-hooks.sh");
    assert!(p.exists(), "{} not found", p.display());
    p
}

fn read(rel: &str) -> String {
    let p = manifest_dir().join(rel);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

fn run_bash(script: &str) -> (i32, String, String) {
    let out = Command::new("bash")
        .arg("-c")
        .arg(script)
        .output()
        .expect("failed to run bash");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn run_bash_in(dir: &std::path::Path, script: &str) -> (i32, String, String) {
    let out = Command::new("bash")
        .arg("-c")
        .arg(script)
        .current_dir(dir)
        .output()
        .expect("failed to run bash");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

// ---------------------------------------------------------------------------------------------
// scripts/lib/purge-target-decision.sh — pure decision functions, sourced + unit-tested directly.
// ---------------------------------------------------------------------------------------------

#[test]
fn purge_target_decision_lib_exists_and_is_shebanged() {
    let bytes = fs::read(lib_script()).unwrap();
    assert!(
        bytes.starts_with(b"#!"),
        "purge-target-decision.sh must start with a shebang"
    );
}

#[test]
fn purge_target_should_purge_boundary_is_strictly_greater_than() {
    let check = |size_mb: i64, threshold_mb: i64| -> String {
        let cmd = format!(
            "set -uo pipefail; . {path} && purge_target_should_purge {size} {thr} && echo YES || echo NO",
            path = lib_script().display(),
            size = size_mb,
            thr = threshold_mb,
        );
        let (_, stdout, _) = run_bash(&cmd);
        stdout.trim().to_string()
    };

    // Equal to the budget -> NOT over budget (matches camera-box's `-le` skip semantics).
    assert_eq!(
        check(4096, 4096),
        "NO",
        "purge_target_should_purge(4096, 4096) must be NO -- equal to budget is not over"
    );
    // One MB under -> not over budget.
    assert_eq!(check(4095, 4096), "NO");
    // One MB over -> over budget.
    assert_eq!(
        check(4097, 4096),
        "YES",
        "purge_target_should_purge(4097, 4096) must be YES -- one MB over the budget"
    );
    // Empty target/ -> never over budget.
    assert_eq!(check(0, 4096), "NO");
    // Well over budget.
    assert_eq!(check(9000, 4096), "YES");
}

#[test]
fn purge_target_daemon_live_true_when_a_matching_process_is_found() {
    let cmd = format!(
        r#"set -uo pipefail
. {path}
pgrep() {{ [ "$1" = "-x" ] && [ "$2" = "dantesync|dantesync-tray" ] && return 0; return 1; }}
purge_target_daemon_live && echo LIVE || echo NOTLIVE
"#,
        path = lib_script().display()
    );
    let (code, stdout, stderr) = run_bash(&cmd);
    assert_eq!(code, 0, "harness itself must exit 0\nstderr={stderr}");
    assert_eq!(stdout.trim(), "LIVE");
}

#[test]
fn purge_target_daemon_live_false_when_no_matching_process() {
    let cmd = format!(
        r#"set -uo pipefail
. {path}
pgrep() {{ return 1; }}
purge_target_daemon_live && echo LIVE || echo NOTLIVE
"#,
        path = lib_script().display()
    );
    let (code, stdout, stderr) = run_bash(&cmd);
    assert_eq!(code, 0, "harness itself must exit 0\nstderr={stderr}");
    assert_eq!(stdout.trim(), "NOTLIVE");
}

#[test]
fn purge_target_daemon_comm_pattern_matches_both_binary_names() {
    // Never a full-cmdline match (which would false-positive on this very repo mentioning
    // "dantesync" in scripts/comments) -- must be matched by PROCESS NAME via `pgrep -x`.
    let src = fs::read_to_string(lib_script()).unwrap();
    assert!(
        src.contains("pgrep -x"),
        "daemon-live check must match by process NAME (pgrep -x), not a full-cmdline grep"
    );
    assert!(
        src.contains("dantesync-tray"),
        "daemon-live guard must also cover the Windows tray companion process"
    );
}

// ---------------------------------------------------------------------------------------------
// scripts/purge-target.sh — structure + functional behaviour.
// ---------------------------------------------------------------------------------------------

#[test]
fn purge_target_sh_sources_the_decision_lib_and_has_a_source_guard() {
    let src = fs::read_to_string(purge_script()).unwrap();
    assert!(
        src.contains("lib/purge-target-decision.sh"),
        "purge-target.sh must source the pure decision lib, not re-implement the logic inline"
    );
    assert!(
        src.contains(r#"if [[ "${BASH_SOURCE[0]}" == "${0}" ]]"#)
            || src.contains(r#"if [[ "${BASH_SOURCE[0]}" == "$0" ]]"#),
        "purge-target.sh must gate its main flow behind an executed-not-sourced guard \
         (so it is sourceable for tests, mirroring cam-disk-guard.sh's #403 convention)"
    );
    assert!(
        src.contains("purge_target_main"),
        "purge-target.sh must expose its flow as a purge_target_main function"
    );
}

/// Build a scratch git repo with `scripts/purge-target.sh` + `scripts/lib/purge-target-decision.sh`
/// copied in at the right relative paths, so `HERE` resolution + `git rev-parse --show-toplevel`
/// both work exactly as they would in the real repo.
struct ScratchRepo {
    dir: tempfile::TempDir,
}

impl ScratchRepo {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let scripts = dir.path().join("scripts");
        let lib = scripts.join("lib");
        fs::create_dir_all(&lib).unwrap();
        fs::copy(purge_script(), scripts.join("purge-target.sh")).unwrap();
        fs::copy(lib_script(), lib.join("purge-target-decision.sh")).unwrap();
        let (code, _, stderr) = run_bash_in(dir.path(), "git init -q");
        assert_eq!(code, 0, "git init failed: {stderr}");
        Self { dir }
    }

    fn path(&self) -> &std::path::Path {
        self.dir.path()
    }

    fn purge_target_sh(&self) -> PathBuf {
        self.path().join("scripts/purge-target.sh")
    }

    fn target_dir(&self) -> PathBuf {
        self.path().join("target")
    }

    /// Create target/ with a real ~2 MB file so `du -sm` reports a non-trivial, deterministic size.
    fn make_target_dir(&self) {
        let t = self.target_dir();
        fs::create_dir_all(&t).unwrap();
        let (code, _, stderr) = run_bash(&format!(
            "dd if=/dev/zero of={:?} bs=1M count=2 status=none",
            t.join("junk.bin")
        ));
        assert_eq!(code, 0, "dd failed: {stderr}");
    }
}

#[test]
fn purge_target_reports_nothing_to_do_when_no_target_dir_exists() {
    let repo = ScratchRepo::new();
    let cmd = format!(
        "set -uo pipefail; . {} --check",
        repo.purge_target_sh().display()
    );
    let (code, stdout, stderr) = run_bash(&cmd);
    assert_eq!(code, 0, "stderr={stderr}");
    assert!(
        stdout.contains("no target/"),
        "expected 'no target/' message, got: {stdout}"
    );
    assert!(!repo.target_dir().exists());
}

#[test]
fn purge_target_check_reports_size_and_never_purges_even_when_over_budget() {
    let repo = ScratchRepo::new();
    repo.make_target_dir();
    let cmd = format!(
        "set -uo pipefail; THRESHOLD_MB=1; . {} --check",
        repo.purge_target_sh().display()
    );
    let (code, stdout, stderr) = run_bash(&cmd);
    assert_eq!(code, 0, "stderr={stderr}");
    assert!(
        stdout.contains("target/ ="),
        "expected a size report line, got: {stdout}"
    );
    assert!(
        repo.target_dir().exists(),
        "--check must NEVER purge, even when over budget"
    );
}

#[test]
fn purge_target_skips_and_leaves_target_dir_when_daemon_is_live() {
    let repo = ScratchRepo::new();
    repo.make_target_dir();
    let cmd = format!(
        r#"set -uo pipefail
THRESHOLD_MB=1
. {script}
pgrep() {{ return 0; }}
purge_target_main --force
"#,
        script = repo.purge_target_sh().display()
    );
    let (code, stdout, stderr) = run_bash(&cmd);
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    assert!(
        stderr.contains("RUNNING") || stdout.contains("RUNNING"),
        "expected a live-daemon skip message, stdout={stdout} stderr={stderr}"
    );
    assert!(
        repo.target_dir().exists(),
        "must NEVER purge while the daemon is live, even with --force"
    );
}

#[test]
fn purge_target_skips_when_under_budget_and_not_forced() {
    let repo = ScratchRepo::new();
    repo.make_target_dir();
    let cmd = format!(
        r#"set -uo pipefail
THRESHOLD_MB=100
. {script}
pgrep() {{ return 1; }}
purge_target_main
"#,
        script = repo.purge_target_sh().display()
    );
    let (code, stdout, stderr) = run_bash(&cmd);
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    assert!(
        stdout.contains("under budget"),
        "expected an 'under budget' message, got: {stdout}"
    );
    assert!(
        repo.target_dir().exists(),
        "must not purge when under budget and not forced"
    );
}

#[test]
fn purge_target_purges_via_rm_when_cargo_is_unavailable() {
    let repo = ScratchRepo::new();
    repo.make_target_dir();
    // Restrict PATH to exclude cargo (which lives outside /usr/bin:/bin on this box) so the
    // script must take its `rm -rf` fallback branch.
    let cmd = format!(
        r#"set -uo pipefail
export PATH=/usr/bin:/bin
THRESHOLD_MB=1
. {script}
pgrep() {{ return 1; }}
purge_target_main --force
"#,
        script = repo.purge_target_sh().display()
    );
    let (code, stdout, stderr) = run_bash(&cmd);
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    assert!(
        stdout.contains("done"),
        "expected a 'done' message, got: {stdout}"
    );
    assert!(
        !repo.target_dir().exists(),
        "target/ must be gone after a forced purge with no cargo available"
    );
}

#[test]
fn purge_target_purges_via_cargo_clean_when_over_budget_and_cargo_available() {
    let repo = ScratchRepo::new();
    // A minimal, dependency-free cargo project so `cargo clean` runs instantly, offline.
    fs::write(
        repo.path().join("Cargo.toml"),
        "[package]\nname = \"scratch\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(repo.path().join("src/lib.rs"), "").unwrap();
    repo.make_target_dir();

    let cmd = format!(
        r#"set -uo pipefail
THRESHOLD_MB=1
. {script}
pgrep() {{ return 1; }}
purge_target_main --force
"#,
        script = repo.purge_target_sh().display()
    );
    let (code, stdout, stderr) = run_bash(&cmd);
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    assert!(
        !repo.target_dir().exists(),
        "target/ must be gone after cargo clean"
    );
}

// ---------------------------------------------------------------------------------------------
// scripts/install-git-hooks.sh — installs a real pre-push hook that calls purge-target.sh.
// ---------------------------------------------------------------------------------------------

#[test]
fn install_git_hooks_creates_an_executable_pre_push_hook_calling_purge_target() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (code, _, stderr) = run_bash_in(dir.path(), "git init -q");
    assert_eq!(code, 0, "git init failed: {stderr}");

    let (code, stdout, stderr) = run_bash_in(
        dir.path(),
        &format!("bash {}", installer_script().display()),
    );
    assert_eq!(code, 0, "installer failed: stdout={stdout} stderr={stderr}");

    let hook = dir.path().join(".git/hooks/pre-push");
    assert!(hook.exists(), "pre-push hook was not created");

    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(&hook).unwrap().permissions().mode();
    assert!(mode & 0o111 != 0, "pre-push hook must be executable");

    let content = fs::read_to_string(&hook).unwrap();
    assert!(
        content.contains("scripts/purge-target.sh"),
        "pre-push hook must call scripts/purge-target.sh, got:\n{content}"
    );
    assert!(
        content.contains("|| true") || content.contains("exit 0"),
        "pre-push hook must be non-blocking -- a purge failure must never stop a push"
    );
}

#[test]
fn install_git_hooks_round_trip_invokes_purge_target_successfully() {
    let dir = tempfile::tempdir().expect("tempdir");
    let scripts = dir.path().join("scripts");
    let lib = scripts.join("lib");
    fs::create_dir_all(&lib).unwrap();
    fs::copy(purge_script(), scripts.join("purge-target.sh")).unwrap();
    fs::copy(lib_script(), lib.join("purge-target-decision.sh")).unwrap();

    let (code, _, stderr) = run_bash_in(dir.path(), "git init -q");
    assert_eq!(code, 0, "git init failed: {stderr}");

    let (code, _, stderr) = run_bash_in(
        dir.path(),
        &format!("bash {}", installer_script().display()),
    );
    assert_eq!(code, 0, "installer failed: {stderr}");

    // Invoke the INSTALLED hook directly, exactly as git would before a real push.
    let hook = dir.path().join(".git/hooks/pre-push");
    let (code, stdout, stderr) = run_bash_in(dir.path(), &format!("{}", hook.display()));
    assert_eq!(
        code, 0,
        "installed pre-push hook must exit 0 (non-blocking)\nstdout={stdout} stderr={stderr}"
    );
    assert!(
        stdout.contains("no target/"),
        "expected purge-target.sh's own output via the installed hook, got: {stdout}"
    );
}

// ---------------------------------------------------------------------------------------------
// CLAUDE.md -- the root fix: an explicit Tier-0 Local Build Policy, replacing the contradictory
// "prioritize local cargo build" guidance.
// ---------------------------------------------------------------------------------------------

#[test]
fn claude_md_has_an_explicit_local_build_policy_section() {
    let claude_md = read("CLAUDE.md");
    assert!(
        claude_md.contains("## Local Build Policy"),
        "CLAUDE.md must have an explicit ## Local Build Policy section (absent marker = Tier 0, \
         but it must be STATED, not left implicit)"
    );
}

#[test]
fn claude_md_no_longer_instructs_a_local_release_build() {
    let claude_md = read("CLAUDE.md");
    assert!(
        !claude_md.contains("Prioritize local `cargo build`"),
        "the old contradictory 'Local Verification' bullet must be removed"
    );
    assert!(
        !claude_md.contains("## Build Commands"),
        "the old '## Build Commands' block (leading with `cargo build --release`) must be removed"
    );
    assert!(
        !claude_md.contains("# Build release binary"),
        "the literal 'Build release binary' cargo build --release invitation must be gone"
    );
}

#[test]
fn claude_md_local_build_policy_names_the_release_workflow_and_cheap_checks() {
    let claude_md = read("CLAUDE.md");
    let section_start = claude_md
        .find("## Local Build Policy")
        .expect("Local Build Policy section must exist");
    let section = &claude_md[section_start..];

    assert!(
        section.contains("release.yml"),
        "must name .github/workflows/release.yml as the thing that actually builds+publishes"
    );
    assert!(
        section.contains("dantesync-linux-amd64") && section.contains("dantesync-windows-amd64.exe"),
        "must name the actual published release assets"
    );
    for cheap_check in ["cargo fmt", "cargo check", "cargo clippy", "cargo test --no-run"] {
        assert!(
            section.contains(cheap_check),
            "Local Build Policy must list '{cheap_check}' as an allowed cheap check"
        );
    }
    assert!(
        section.contains("cargo build --release"),
        "must explicitly say cargo build --release belongs to CI, not local"
    );
}

#[test]
fn claude_md_keeps_the_npcap_cross_compilation_note() {
    let claude_md = read("CLAUDE.md");
    assert!(
        claude_md.contains("Npcap"),
        "the Npcap cross-compilation note is still true and useful -- must be kept"
    );
}
