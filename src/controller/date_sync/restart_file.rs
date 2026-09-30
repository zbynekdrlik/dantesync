//! dantesync#126 — the master's saved fleet date offset on disk: `date-offset.json` beside
//! `config.json` (`/etc/dantesync/`, `C:\ProgramData\DanteSync\`), in a file of its own.
//!
//! One flat JSON object of integers (all ns, the grandmaster as its six bytes), `version` first.
//! It is written by temp + rename (the temp file synced first), so a crash never leaves a half
//! record; a record that does not parse, or of another version, is not restored — the master then
//! takes the pre-1.15 path (boot step, new session), loudly. The pure record and the restore
//! decision are `crate::date_offset::persist`.

use crate::date_offset::{AuthorityState, DateOffsetState, DateSlew, STATE_VERSION};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct SlewFile {
    from_ns: i64,
    to_ns: i64,
    start_ptp_ns: i64,
    ppm: u32,
}

/// The file's shape. Every non-optional field is required (a record missing one is not
/// restored); a missing optional one reads as none.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct StateFile {
    version: u32,
    gm_uuid: [u8; 6],
    d_ns: i64,
    since_ptp_ns: i64,
    seq: u32,
    pending_d_ns: Option<i64>,
    pending_effective_ptp_ns: Option<i64>,
    slew: Option<SlewFile>,
    micro: bool,
    daily_last_step_wall_ns: Option<i64>,
    daily_last_step_ns: Option<i64>,
    written_wall_ns: i64,
    written_ptp_ns: i64,
}

/// The record as the file's JSON text.
pub(super) fn encode(state: &DateOffsetState) -> String {
    let a = &state.authority;
    let file = StateFile {
        version: STATE_VERSION,
        gm_uuid: state.gm_uuid,
        d_ns: a.d_ns,
        since_ptp_ns: a.since_ptp_ns,
        seq: a.seq,
        pending_d_ns: a.pending.map(|p| p.0),
        pending_effective_ptp_ns: a.pending.map(|p| p.1),
        slew: a.slew.map(|s| SlewFile {
            from_ns: s.from_ns,
            to_ns: s.to_ns,
            start_ptp_ns: s.start_ptp_ns,
            ppm: s.ppm,
        }),
        micro: a.micro,
        daily_last_step_wall_ns: a.daily_last_step.map(|s| s.0),
        daily_last_step_ns: a.daily_last_step.map(|s| s.1),
        written_wall_ns: state.written_wall_ns,
        written_ptp_ns: state.written_ptp_ns,
    };
    // A struct of integers, options and bools always serializes.
    serde_json::to_string_pretty(&file).unwrap_or_default()
}

/// The record from the file's JSON text, or why it is not usable.
pub(super) fn decode(text: &str) -> Result<DateOffsetState, String> {
    let f: StateFile = serde_json::from_str(text).map_err(|e| format!("not a saved state: {e}"))?;
    if f.version != STATE_VERSION {
        return Err(format!(
            "version {} (this build reads {})",
            f.version, STATE_VERSION
        ));
    }
    let pending = match (f.pending_d_ns, f.pending_effective_ptp_ns) {
        (Some(d), Some(eff)) => Some((d, eff)),
        (None, None) => None,
        _ => return Err("a pending step without its D or its instant".to_string()),
    };
    let daily_last_step = match (f.daily_last_step_wall_ns, f.daily_last_step_ns) {
        (Some(wall), Some(n)) => Some((wall, n)),
        (None, None) => None,
        _ => return Err("a nightly step without its instant or its size".to_string()),
    };
    if pending.is_some() && f.slew.is_some() {
        return Err("a pending step and a slew at once".to_string());
    }
    Ok(DateOffsetState {
        authority: AuthorityState {
            d_ns: f.d_ns,
            since_ptp_ns: f.since_ptp_ns,
            seq: f.seq,
            pending,
            slew: f.slew.map(|s| DateSlew {
                from_ns: s.from_ns,
                to_ns: s.to_ns,
                start_ptp_ns: s.start_ptp_ns,
                ppm: crate::date_offset::clamp_slew_ppm(s.ppm),
            }),
            micro: f.micro,
            daily_last_step,
        },
        gm_uuid: f.gm_uuid,
        written_wall_ns: f.written_wall_ns,
        written_ptp_ns: f.written_ptp_ns,
    })
}

/// Read the saved record: `Ok(None)` when there is no file, an error when it is unreadable or
/// not a usable record.
pub(super) fn read(path: &Path) -> Result<Option<DateOffsetState>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => decode(&text).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("unreadable: {e}")),
    }
}

/// Write the record by temp + rename in the same directory, the temp file synced first: a
/// reader (the next start) sees the old record or the new one, never a torn one. On Unix the
/// directory is synced after the rename, so a power cut cannot bring the previous record back
/// (review round 2).
pub(super) fn write_atomic(path: &Path, state: &DateOffsetState) -> std::io::Result<()> {
    write_atomic_with(path, state, sync_dir)
}

/// The directory a record at `path` lives in (the directory to sync after the rename).
fn dir_to_sync(path: &Path) -> Option<PathBuf> {
    path.parent()
        .filter(|d| !d.as_os_str().is_empty())
        .map(Path::to_path_buf)
}

/// Sync a directory (Unix; a no-op elsewhere).
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    std::fs::File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

fn write_atomic_with(
    path: &Path,
    state: &DateOffsetState,
    sync_dir: impl FnOnce(&Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(encode(state).as_bytes())?;
        f.write_all(b"\n")?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    if let Some(dir) = dir_to_sync(path) {
        sync_dir(&dir)?;
    }
    Ok(())
}

/// Remove the record if there is one: `Ok(true)` removed, `Ok(false)` none. The check comes
/// first because a camera box's root is READ-ONLY, where unlinking even a missing file fails with
/// EROFS instead of NotFound (review round 2: a false warning on every start otherwise).
pub(super) fn remove_if_present(path: &Path) -> std::io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
        Ok(_) => std::fs::remove_file(path).map(|()| true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: i64 = 1_000_000_000;

    fn sample(pending: Option<(i64, i64)>, slew: Option<DateSlew>) -> DateOffsetState {
        DateOffsetState {
            authority: AuthorityState {
                d_ns: 1_789_274_439_109_968_443,
                since_ptp_ns: 3 * 86_400 * S + 17,
                seq: 7,
                pending,
                slew,
                micro: slew.is_some(),
                daily_last_step: Some((1_790_388_010 * S, -1_295_250_000)),
            },
            gm_uuid: [0x00, 0x1d, 0xc1, 0x0a, 0x0b, 0x0c],
            written_wall_ns: 1_790_748_120 * S + 5,
            written_ptp_ns: 4 * 86_400 * S,
        }
    }

    #[test]
    fn the_saved_state_round_trips_every_field_126() {
        let slew = DateSlew {
            from_ns: 7 * S,
            to_ns: 7 * S - 400_000,
            start_ptp_ns: 12 * S,
            ppm: 100,
        };
        for st in [
            sample(None, None),
            sample(
                Some((1_789_274_439_357_265_443, 4 * 86_400 * S + 10 * S)),
                None,
            ),
            sample(None, Some(slew)),
        ] {
            let text = encode(&st);
            assert!(text.contains("\"version\": 1"), "{text}");
            assert_eq!(decode(&text), Ok(st), "{text}");
        }
        let mut none = sample(None, None);
        none.authority.daily_last_step = None;
        assert_eq!(decode(&encode(&none)), Ok(none));
    }

    #[test]
    fn a_record_that_is_not_a_complete_current_one_is_not_restored_126() {
        let good = encode(&sample(None, None));
        let v: serde_json::Value = serde_json::from_str(&good).expect("JSON");
        let with = |key: &str, value: serde_json::Value| {
            let mut v = v.clone();
            v[key] = value;
            v.to_string()
        };
        let without = |key: &str| {
            let mut v = v.clone();
            v.as_object_mut().expect("object").remove(key);
            v.to_string()
        };
        for bad in [
            String::new(),
            "{not json".to_string(),
            "null".to_string(),
            "[]".to_string(),
            with("version", serde_json::json!(2)),
            with("d_ns", serde_json::json!("x")),
            with("gm_uuid", serde_json::json!([1, 2, 3])),
            with("pending_d_ns", serde_json::json!(5)),
            with("daily_last_step_ns", serde_json::Value::Null),
            without("d_ns"),
            without("seq"),
            without("gm_uuid"),
        ] {
            assert!(decode(&bad).is_err(), "{bad}");
        }
        // Both a pending step and a slew: never written by an authority, never restored.
        let mut both: serde_json::Value = serde_json::from_str(&encode(&sample(
            Some((5, 6)),
            Some(DateSlew {
                from_ns: 7 * S,
                to_ns: 7 * S - 1,
                start_ptp_ns: 1,
                ppm: 100,
            }),
        )))
        .expect("JSON");
        both["micro"] = serde_json::json!(false);
        assert!(decode(&both.to_string()).is_err());
    }

    #[test]
    fn a_record_is_removed_only_when_present_126() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("date-offset.json");
        assert!(!remove_if_present(&path).expect("none: not an error"));
        write_atomic(&path, &sample(None, None)).expect("written");
        assert!(remove_if_present(&path).expect("removed"));
        assert!(!path.exists());
    }

    #[test]
    fn the_file_is_written_whole_and_read_back_and_a_missing_one_is_none_126() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("date-offset.json");
        assert_eq!(read(&path), Ok(None), "no file yet");
        let st = sample(Some((11, 12)), None);
        write_atomic(&path, &st).expect("written");
        assert_eq!(read(&path), Ok(Some(st)));
        assert!(
            !dir.path().join("date-offset.json.tmp").exists(),
            "the temp file is renamed away"
        );
        // Overwritten whole by the next record.
        let st2 = sample(None, None);
        write_atomic(&path, &st2).expect("rewritten");
        assert_eq!(read(&path), Ok(Some(st2)));
        std::fs::write(&path, "{torn").expect("corrupt it");
        assert!(read(&path).is_err());
        // A directory that does not exist: an error, never a panic.
        assert!(write_atomic(&dir.path().join("no/such/dir/x.json"), &st2).is_err());
    }

    #[test]
    fn a_directory_sync_that_fails_after_the_rename_still_saved_the_record_126() {
        // The new record is in place once renamed: a directory that refuses fsync (EINVAL on
        // some filesystems) must not read as "not saved" — that retried every 10 s for ever
        // with a false "a restart would step the fleet" (review round 3).
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("date-offset.json");
        let st = sample(None, None);
        let mut synced = None;
        write_atomic_with(&path, &st, |d| {
            synced = Some(d.to_path_buf());
            Err(std::io::Error::from_raw_os_error(22))
        })
        .expect("saved: the rename is done");
        assert_eq!(synced.as_deref(), Some(dir.path()), "its own directory");
        assert_eq!(read(&path), Ok(Some(st)));
    }

    #[test]
    fn the_directory_synced_is_the_record_s_own_even_for_a_bare_file_name_126() {
        assert_eq!(
            dir_to_sync(Path::new("/etc/dantesync/date-offset.json")),
            Some(PathBuf::from("/etc/dantesync"))
        );
        assert_eq!(
            dir_to_sync(Path::new("date-offset.json")),
            Some(PathBuf::from(".")),
            "the current directory, not skipped"
        );
    }
}
