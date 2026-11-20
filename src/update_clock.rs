//! When to check for an app release. Only the preferences are shared; each tray keeps its own clock in its home.
//! Times are UTC seconds supplied by the caller. Planning reads and writes nothing. Save `attempted` before sending
//! a request, then save its outcome; an interrupted request still has a deadline across restarts.

use std::fs;
use std::io::{self, Read};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::{UpdatePrefs, parse_json_object, write_json_atomic};
use crate::scheduler::{BACKOFF_FIRST_SECS, LONGEST_WAIT_SECS};

pub const CLOCK_FILE: &str = "update-clock.json";
const VERSION: u32 = 1;
const MAX_BYTES: u64 = 4096;

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ClockState(Option<Record>);

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    /// Start of a request, or the time its result was received.
    at: i64,
    failures: u8,
    // The server's absolute floor, independent of local cadence/backoff. A missing field is not an expired wait.
    #[serde(deserialize_with = "Option::deserialize")]
    retry_at: Option<i64>,
}

impl ClockState {
    /// Only a missing file is a fresh clock. A damaged or unsupported file stays in place for inspection.
    pub fn load(home: &Path) -> Result<Self, String> {
        let path = home.join(CLOCK_FILE);
        match fs::symlink_metadata(&path) {
            Ok(m) if !m.is_file() => return Err(format!("{} is not a regular file", path.display())),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        }
        let read_error = |e: io::Error| format!("cannot read {}: {e}", path.display());
        let f = fs::File::open(&path).map_err(read_error)?;
        let len = f.metadata().map_err(read_error)?.len();
        let bytes = bounded_read(f, len).map_err(read_error)?;
        let record: Record = parse_json_object(&bytes).map_err(|e| format!("{} is not valid: {e}", path.display()))?;
        if record.version != VERSION {
            return Err(format!("{} uses format {}; this app understands format {VERSION}", path.display(), record.version));
        }
        if record.retry_at.is_some_and(|at| record.failures == 0 || at < record.at) {
            return Err(format!("{} has an invalid retry time", path.display()));
        }
        Ok(Self(Some(record)))
    }

    /// Single writer (the tray for this home). Recheck the file before replacing it: even a stale in-memory clock
    /// must not write over damage or a format this version cannot read.
    pub fn save(&self, home: &Path) -> Result<(), String> {
        if *self == Self::load(home)? {
            return Ok(());
        }
        let path = home.join(CLOCK_FILE);
        let record = self.0.ok_or_else(|| format!("refusing to clear {}", path.display()))?;
        let bytes = serde_json::to_vec_pretty(&record).expect("clock record serializes");
        write_json_atomic(&path, &bytes).map_err(|e| format!("cannot save {}: {e}", path.display()))
    }

    /// Absolute deadline (possibly already past), or no timer when disabled. A rollback can reanchor the local
    /// schedule once, but never shortens a server wait. `now + interval` on each replan would slide indefinitely.
    pub fn due(&self, prefs: UpdatePrefs, now: i64) -> Option<i64> {
        if !prefs.enabled {
            return None;
        }
        let Some(r) = self.0 else { return Some(now) };
        let local = if r.at > now {
            now
        } else if r.failures == 0 {
            r.at.saturating_add(prefs.interval.seconds())
        } else {
            let wait = BACKOFF_FIRST_SECS.saturating_mul(2_i64.saturating_pow(u32::from(r.failures - 1))).min(LONGEST_WAIT_SECS);
            r.at.saturating_add(wait)
        };
        Some(local.max(r.retry_at.unwrap_or(local)))
    }

    pub fn attempted(&mut self, now: i64) {
        self.0 = Some(Record { version: VERSION, at: now, failures: self.failures(), retry_at: self.server_wait(now, None) });
    }

    pub fn succeeded(&mut self, now: i64) {
        self.0 = Some(Record { version: VERSION, at: now, failures: 0, retry_at: None });
    }

    /// Back off for five minutes, doubling up to a day. A later absolute retry time from the server takes precedence.
    pub fn failed(&mut self, now: i64, retry_at: Option<i64>) {
        let failures = self.failures().saturating_add(1);
        self.0 = Some(Record { version: VERSION, at: now, failures, retry_at: self.server_wait(now, retry_at) });
    }

    fn failures(&self) -> u8 {
        self.0.map_or(0, |r| r.failures)
    }

    fn server_wait(&self, now: i64, retry_at: Option<i64>) -> Option<i64> {
        self.0.and_then(|r| r.retry_at).max(retry_at).filter(|&at| at > now)
    }
}

fn bounded_read(reader: impl Read, len: u64) -> io::Result<Vec<u8>> {
    let too_large = || io::Error::new(io::ErrorKind::InvalidData, "clock record is too large (limit: 4096 bytes)");
    if len > MAX_BYTES {
        return Err(too_large());
    }
    // Length is only a hint: the file could grow between the metadata query and the read.
    let mut bytes = Vec::with_capacity(len as usize);
    reader.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES { Err(too_large()) } else { Ok(bytes) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{UpdateInterval, UpdateTiming};
    use crate::fsx;
    use std::path::PathBuf;

    const NOW: i64 = 1_800_000_000;
    const DAY: i64 = 86_400;

    fn temp_dir(label: &str) -> PathBuf {
        let base = std::env::var_os("TSS_TEST_ROOT").filter(|s| !s.is_empty()).map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        base.join(format!("tss-it-update-clock-{label}-{}", fsx::unique_suffix()))
    }

    #[test]
    fn normal_deadlines_follow_the_current_interval_and_off_has_no_deadline() {
        let mut clock = ClockState::default();
        let off = UpdatePrefs { enabled: false, ..UpdatePrefs::default() };
        assert_eq!(clock.due(off, NOW), None);
        assert_eq!(clock.due(UpdatePrefs::default(), NOW), Some(NOW));
        clock.succeeded(NOW);
        for (interval, days) in [(UpdateInterval::Daily, 1), (UpdateInterval::Weekly, 7), (UpdateInterval::Monthly, 30)] {
            let prefs = UpdatePrefs { interval, timing: UpdateTiming::OnQuit, ..UpdatePrefs::default() };
            assert_eq!(clock.due(prefs, NOW + 1), Some(NOW + days * DAY));
            assert_eq!(clock.due(prefs, NOW + 31 * DAY), Some(NOW + days * DAY), "overdue is not postponed on restart");
        }
        clock.failed(NOW, Some(NOW + 3600));
        assert_eq!(clock.due(off, NOW), None);
    }

    #[test]
    fn attempts_and_server_waits_survive_restarts_in_only_their_own_home() {
        let root = temp_dir("restart");
        let home = root.join("first");
        let other = root.join("second");
        let prefs = UpdatePrefs::default();
        let mut clock = ClockState::load(&home).unwrap();
        assert!(!home.exists(), "loading an untouched home writes nothing");

        clock.attempted(NOW);
        clock.save(&home).unwrap();
        let mut restarted = ClockState::load(&home).unwrap();
        assert_eq!(restarted.due(prefs, NOW + 2), Some(NOW + DAY), "terminated before receiving a response");
        assert_eq!(ClockState::load(&other).unwrap().due(prefs, NOW + 2), Some(NOW + 2));
        assert_eq!(fs::read_dir(&home).unwrap().count(), 1);
        assert!(home.join(CLOCK_FILE).is_file());

        let server = NOW + 3 * DAY;
        restarted.failed(NOW + 3, Some(server));
        restarted.save(&home).unwrap();
        let mut restarted = ClockState::load(&home).unwrap();
        assert_eq!(restarted.due(prefs, NOW + DAY), Some(server), "a later server deadline wins");
        restarted.attempted(server);
        restarted.failed(server + 2, None);
        assert_eq!(restarted.due(prefs, server + 2), Some(server + 2 + 600), "the failure streak also survived restart");
        restarted.succeeded(server + 700);
        restarted.save(&home).unwrap();
        assert_eq!(ClockState::load(&home).unwrap().due(prefs, server + 701), Some(server + 700 + DAY));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn consecutive_failures_back_off_to_a_day_and_success_clears_the_streak() {
        let mut clock = ClockState::default();
        let prefs = UpdatePrefs::default();
        let mut now = NOW;
        for wait in [300, 600, 1200, 2400, 4800, 9600, 19200, 38400, 76800, DAY, DAY] {
            clock.attempted(now);
            clock.failed(now, Some(now - 1));
            assert_eq!(clock.due(prefs, now), Some(now + wait));
            now += wait;
        }
        clock.succeeded(now);
        clock.attempted(now + DAY);
        clock.failed(now + DAY, None);
        assert_eq!(clock.due(prefs, now + DAY), Some(now + DAY + 300));
    }

    #[test]
    fn a_rollback_reanchors_once_instead_of_sliding_the_deadline() {
        let mut clock = ClockState::default();
        let prefs = UpdatePrefs::default();
        clock.succeeded(NOW);
        let corrected = NOW - 365 * DAY;
        assert_eq!(clock.due(prefs, corrected), Some(corrected));
        clock.attempted(corrected);
        assert_eq!(clock.due(prefs, corrected), Some(corrected + DAY), "another restart does not force another recheck");
        clock.failed(NOW, None);
        assert_eq!(clock.due(prefs, corrected), Some(corrected), "local backoff may reanchor too");
        clock.failed(i64::MAX - 1, None);
        assert_eq!(clock.due(prefs, i64::MAX - 1), Some(i64::MAX));
    }

    #[test]
    fn a_persisted_server_deadline_still_applies_after_clock_rollback() {
        let home = temp_dir("server-rollback");
        let server = NOW + 7200;
        let mut clock = ClockState::default();
        clock.failed(NOW, Some(server));
        clock.save(&home).unwrap();
        let restarted = ClockState::load(&home).unwrap();
        assert_eq!(restarted.due(UpdatePrefs::default(), NOW - DAY), Some(server));
        assert_eq!(restarted.due(UpdatePrefs::default(), server - 1), Some(server));
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn unreadable_or_unsupported_clocks_are_not_reset_by_a_stale_writer() {
        let home = temp_dir("preserve");
        let mut clock = ClockState::load(&home).unwrap();
        clock.attempted(NOW);
        clock.save(&home).unwrap();
        clock.succeeded(NOW + 1);
        let path = home.join(CLOCK_FILE);
        for bad in [
            "not JSON",
            "{}",
            "[1,42,0,null]",
            r#"{"version":2,"at":42,"failures":0,"retry_at":null}"#,
            r#"{"version":1,"at":42,"failures":1}"#,
            r#"{"version":1,"at":42,"failures":1,"retry_at":0}"#,
            r#"{"version":1,"at":42,"failures":0,"retry_at":null,"extra":true}"#,
        ] {
            fs::write(&path, bad).unwrap();
            assert!(ClockState::load(&home).is_err(), "{bad}");
            assert!(clock.save(&home).is_err(), "{bad}");
            assert_eq!(fs::read(&path).unwrap(), bad.as_bytes());
        }
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn an_oversized_but_valid_clock_is_never_loaded_or_replaced() {
        let home = temp_dir("oversized");
        fs::create_dir_all(&home).unwrap();
        let path = home.join(CLOCK_FILE);
        let mut bytes = br#"{"version":1,"at":42,"failures":0,"retry_at":null}"#.to_vec();
        bytes.resize(4096, b' ');
        fs::write(&path, &bytes).unwrap();
        let mut clock = ClockState::load(&home).unwrap();
        clock.attempted(NOW);
        bytes.push(b' ');
        fs::write(&path, &bytes).unwrap();
        assert!(ClockState::load(&home).unwrap_err().contains("too large"));
        assert!(clock.save(&home).is_err());
        assert_eq!(fs::read(&path).unwrap(), bytes);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn size_preflight_and_the_reader_both_enforce_the_clock_limit() {
        let mut input = io::Cursor::new([0; 8192]);
        assert!(bounded_read(&mut input, 8192).is_err());
        assert_eq!(input.position(), 0, "an oversized record does not need any content read");
        assert!(bounded_read(&mut input, 100).is_err());
        assert_eq!(input.position(), 4097, "even an understated length stops after one overflow byte");
    }
}
