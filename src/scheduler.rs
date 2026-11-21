//! When is each repository due, and how long to wait after a failure?
//!
//! Pure on purpose: the caller passes the time in, so every case (clock moved back, calendar months, a rate limit
//! that says "wait an hour") is tested without sleeping. All times are UTC seconds.
//!
//! Rules:
//! * A repository that was never checked is due now.
//! * Otherwise it is due one interval after its last successful check. The interval counts from the check, not
//!   from the app starting, so closing and opening the app does not cause extra requests.
//! * One exception: a repository whose last check left something undone (a skill held, a name clash) kept no ETag,
//!   and what was held was known only to the run that found it. Such a repository is checked once in each run of the
//!   app, as soon as it is seen without an ETag: straight after the app starts, or straight after a choice made for
//!   one of its skills (which also drops the ETag) if the run had not checked it yet. So the window can show what
//!   needs attention. That is at most one request per run for each repository.
//! * After a failure it is tried again after 5 minutes, then 10, 20, 40 and so on, but never later than its own
//!   interval. A rate limit waits for the time GitHub named. Nothing here ever runs more often than every 5 minutes.
//! * A last-check time in the future (the clock was set back) counts as "now", so a wrong clock cannot make the
//!   app silent for a long time.

use std::collections::{HashMap, HashSet};

use crate::config::{Config, RepoConfig};
use crate::github::FailureKind;
use crate::interval::{Interval, UnixSecs};
use crate::state::State;

/// First wait after a failure. Equals the shortest allowed poll interval.
pub const BACKOFF_FIRST_SECS: i64 = 5 * 60;
/// A repository that changed while it was being downloaded is worth another try much sooner.
pub const CHANGED_FIRST_SECS: i64 = 60;
/// No waiting time, whatever the server or a wrong clock says, is longer than this.
pub const LONGEST_WAIT_SECS: i64 = 24 * 60 * 60;
const MAX_DOUBLINGS: u32 = 20;

/// What the app remembers about a repository that is failing. Kept in memory only: after a restart the app tries
/// again straight away, which is what a person who restarted it would expect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retry {
    pub failures: u32,
    pub not_before: UnixSecs,
}

#[derive(Debug, Default)]
pub struct Schedule {
    retry: HashMap<String, Retry>,
    /// Repositories checked since the app started, whatever the result.
    checked: HashSet<String>,
}

/// One interval after the last check. Never checked means due now.
pub fn next_poll(last_check: Option<UnixSecs>, interval: Interval, now: UnixSecs) -> UnixSecs {
    match last_check {
        None => now,
        Some(at) => interval.add_to(at.min(now)),
    }
}

impl Schedule {
    pub fn new() -> Schedule {
        Schedule::default()
    }

    pub fn retry_of(&self, repo_id: &str) -> Option<Retry> {
        self.retry.get(repo_id).copied()
    }

    /// The moment this repository is next due.
    pub fn due_at(&self, repo_id: &str, interval: Interval, last_check: Option<UnixSecs>, now: UnixSecs) -> UnixSecs {
        let regular = next_poll(last_check, interval, now);
        match self.retry.get(repo_id) {
            Some(r) => regular.max(r.not_before.min(now + LONGEST_WAIT_SECS)),
            None => regular,
        }
    }

    pub fn on_success(&mut self, repo_id: &str) {
        self.retry.remove(repo_id);
        self.checked.insert(repo_id.to_string());
    }

    /// Records a failed attempt and returns when to try again.
    pub fn on_failure(&mut self, repo_id: &str, kind: FailureKind, server_says: Option<UnixSecs>, interval: Interval, now: UnixSecs) -> UnixSecs {
        let failures = self.retry.get(repo_id).map_or(0, |r| r.failures).saturating_add(1);
        let first = if kind == FailureKind::Changed { CHANGED_FIRST_SECS } else { BACKOFF_FIRST_SECS };
        let doubled = first.saturating_mul(1i64 << (failures - 1).min(MAX_DOUBLINGS));
        // Waiting longer than a normal interval makes no sense (the repository would be due by then anyway), and a
        // failing repository deserves another look at least daily however rarely it normally runs.
        let cap = (interval.add_to(now) - now).min(LONGEST_WAIT_SECS).max(first);
        let mut not_before = now + doubled.min(cap);
        if let Some(at) = server_says {
            not_before = not_before.max(at.min(now + LONGEST_WAIT_SECS));
        }
        self.retry.insert(repo_id.to_string(), Retry { failures, not_before });
        self.checked.insert(repo_id.to_string());
        not_before
    }

    pub fn forget(&mut self, repo_id: &str) {
        self.retry.remove(repo_id);
        self.checked.remove(repo_id);
    }

    /// Repositories that are due, in the order they appear in the settings.
    pub fn due_repos(&self, cfg: &Config, state: &State, now: UnixSecs) -> Vec<String> {
        cfg.repos().iter().filter(|r| schedulable(cfg, r)).filter(|r| self.due_of(cfg, r, state, now) <= now).map(|r| r.id.clone()).collect()
    }

    /// When the app next needs to wake up. `None` means never (nothing is configured): a fully idle process.
    pub fn next_wake(&self, cfg: &Config, state: &State, now: UnixSecs) -> Option<UnixSecs> {
        cfg.repos().iter().filter(|r| schedulable(cfg, r)).map(|r| self.due_of(cfg, r, state, now)).min()
    }

    fn due_of(&self, cfg: &Config, repo: &RepoConfig, state: &State, now: UnixSecs) -> UnixSecs {
        let recorded = state.repo(&repo.id);
        if recorded.is_some_and(|r| r.searches.values().any(|h| !h.paused && !h.complete)) {
            if let Some(retry) = self.retry.get(&repo.id) {
                return retry.not_before;
            }
            if let Some(at) = recorded.and_then(|r| r.history_retry_at) {
                return at.min(now + LONGEST_WAIT_SECS);
            }
            return recorded.and_then(|r| r.last_check).map_or(now, |at| at.min(now) + 30);
        }
        let unsettled = recorded.is_some_and(|r| r.last_check.is_some() && r.etag.is_none());
        if unsettled && !self.checked.contains(&repo.id) {
            return now;
        }
        self.due_at(&repo.id, repo.effective_interval(cfg.default_interval), recorded.and_then(|r| r.last_check), now)
    }
}

/// Paused or misconfigured repositories are shown with their problem but never polled.
fn schedulable(cfg: &Config, repo: &RepoConfig) -> bool {
    repo.enabled && repo.problems().is_empty() && repo.effective_interval(cfg.default_interval).validate_poll().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interval::Unit;

    const T0: UnixSecs = 1_800_000_000;

    fn mins(n: u32) -> Interval {
        Interval::new(n, Unit::Minute)
    }

    fn hours(n: u32) -> Interval {
        Interval::new(n, Unit::Hour)
    }

    fn repo(id: &str, name: &str) -> RepoConfig {
        RepoConfig { id: id.into(), repo: name.into(), ..RepoConfig::default() }
    }

    fn config(default_interval: Interval, repos: Vec<RepoConfig>) -> Config {
        let mut cfg = Config::from_lanes(repos);
        cfg.default_interval = default_interval;
        cfg
    }

    /// Repositories whose last check settled everything (so they kept an ETag).
    fn state_with(checks: &[(&str, UnixSecs)]) -> State {
        let mut s = State::default();
        for (id, at) in checks {
            s.repo_mut(id).last_check = Some(*at);
            s.repo_mut(id).etag = Some("\"e\"".into());
        }
        s
    }

    #[test]
    fn a_repository_that_was_never_checked_is_due_now() {
        assert_eq!(next_poll(None, hours(6), T0), T0);
        assert_eq!(Schedule::new().due_at("a", hours(6), None, T0), T0);
    }

    #[test]
    fn it_is_due_one_interval_after_the_last_check() {
        assert_eq!(next_poll(Some(T0), mins(15), T0 + 10), T0 + 15 * 60);
        assert_eq!(next_poll(Some(T0), hours(6), T0 + 10), T0 + 6 * 3600);
        assert_eq!(next_poll(Some(T0), Interval::new(2, Unit::Week), T0 + 10), T0 + 14 * 86_400);
    }

    #[test]
    fn months_are_calendar_months() {
        let jan_31: UnixSecs = 1_769_817_600;
        assert_eq!(crate::interval::format_utc(jan_31), "2026-01-31T00:00:00Z");
        let due = next_poll(Some(jan_31), Interval::new(1, Unit::Month), jan_31 + 5);
        assert_eq!(crate::interval::format_utc(due), "2026-02-28T00:00:00Z");
    }

    #[test]
    fn a_last_check_in_the_future_counts_as_now() {
        // The clock was set back a year. Without the clamp the next check would be a year and a day away.
        let wrong = T0 + 365 * 86_400;
        assert_eq!(next_poll(Some(wrong), Interval::new(1, Unit::Day), T0), T0 + 86_400);
    }

    #[test]
    fn failures_back_off_five_ten_twenty_minutes_and_stop_at_the_interval() {
        let mut s = Schedule::new();
        let interval = hours(1);
        let waits: Vec<i64> = (0..6).map(|_| s.on_failure("a", FailureKind::Network, None, interval, T0) - T0).collect();
        assert_eq!(waits, [300, 600, 1200, 2400, 3600, 3600]);
        assert_eq!(s.retry_of("a").unwrap().failures, 6);
    }

    #[test]
    fn a_short_interval_caps_the_backoff_but_never_below_the_first_step() {
        let mut s = Schedule::new();
        let waits: Vec<i64> = (0..4).map(|_| s.on_failure("a", FailureKind::Server, None, mins(15), T0) - T0).collect();
        assert_eq!(waits, [300, 600, 900, 900]);
        let mut s = Schedule::new();
        let waits: Vec<i64> = (0..3).map(|_| s.on_failure("a", FailureKind::Server, None, mins(5), T0) - T0).collect();
        assert_eq!(waits, [300, 300, 300]);
    }

    #[test]
    fn a_repository_that_changed_mid_download_is_retried_quickly() {
        let mut s = Schedule::new();
        let waits: Vec<i64> = (0..3).map(|_| s.on_failure("a", FailureKind::Changed, None, hours(6), T0) - T0).collect();
        assert_eq!(waits, [60, 120, 240]);
    }

    #[test]
    fn a_rate_limit_waits_as_long_as_github_says_even_past_the_interval() {
        let mut s = Schedule::new();
        let until = s.on_failure("a", FailureKind::RateLimited, Some(T0 + 7200), hours(1), T0);
        assert_eq!(until, T0 + 7200, "the server's time wins over a shorter backoff");
        let early = s.on_failure("b", FailureKind::RateLimited, Some(T0 + 10), hours(1), T0);
        assert_eq!(early, T0 + 300, "a server time earlier than the backoff changes nothing");
    }

    #[test]
    fn an_absurd_wait_from_the_server_is_cut_to_a_day() {
        let mut s = Schedule::new();
        let until = s.on_failure("a", FailureKind::RateLimited, Some(T0 + 10 * 365 * 86_400), hours(1), T0);
        assert_eq!(until, T0 + LONGEST_WAIT_SECS);
        // A clock that jumps back after the failure cannot push the next try beyond a day either.
        assert!(s.due_at("a", hours(1), Some(T0 - 10_000_000), T0 - 5 * 86_400) <= T0 - 5 * 86_400 + LONGEST_WAIT_SECS);
    }

    #[test]
    fn success_clears_the_backoff() {
        let mut s = Schedule::new();
        s.on_failure("a", FailureKind::Network, None, hours(1), T0);
        s.on_success("a");
        assert!(s.retry_of("a").is_none());
        let again = s.on_failure("a", FailureKind::Network, None, hours(1), T0);
        assert_eq!(again, T0 + 300, "the wait starts over");
    }

    #[test]
    fn a_failing_repository_is_not_due_until_its_wait_is_over() {
        let mut s = Schedule::new();
        s.on_failure("a", FailureKind::Network, None, hours(6), T0);
        // It was last checked successfully long ago, so it is overdue, but the wait still applies.
        let last = Some(T0 - 10 * 3600);
        assert_eq!(s.due_at("a", hours(6), last, T0), T0 + 300);
        assert!(s.due_at("a", hours(6), last, T0 + 300) <= T0 + 300);
    }

    #[test]
    fn many_failures_do_not_overflow_and_never_wait_beyond_a_day() {
        let mut s = Schedule::new();
        for _ in 0..200 {
            s.on_failure("a", FailureKind::Network, None, Interval::new(10_000, Unit::Month), T0);
        }
        let r = s.retry_of("a").unwrap();
        assert_eq!(r.failures, 200);
        assert_eq!(r.not_before, T0 + LONGEST_WAIT_SECS, "even a very long interval is retried daily while it fails");
    }

    #[test]
    fn due_repositories_and_the_next_wake_follow_each_repositorys_own_interval() {
        let cfg = config(hours(6), vec![repo("a", "o/a"), RepoConfig { interval: Some(mins(30)), ..repo("b", "o/b") }, repo("c", "o/c")]);
        let state = state_with(&[("a", T0), ("b", T0), ("c", T0 - 7 * 3600)]);
        let s = Schedule::new();
        assert_eq!(s.due_repos(&cfg, &state, T0 + 60), ["c"], "only the one that is overdue");
        assert_eq!(s.next_wake(&cfg, &state, T0 + 60), Some(T0 - 7 * 3600 + 6 * 3600), "an overdue repository is due at its own time, in the past");
        assert_eq!(s.due_repos(&cfg, &state, T0 + 30 * 60), ["b", "c"], "exactly on time counts as due");
    }

    #[test]
    fn the_next_wake_is_the_earliest_due_time() {
        let cfg = config(hours(6), vec![repo("a", "o/a"), RepoConfig { interval: Some(mins(30)), ..repo("b", "o/b") }]);
        let state = state_with(&[("a", T0), ("b", T0)]);
        assert_eq!(Schedule::new().next_wake(&cfg, &state, T0 + 5), Some(T0 + 30 * 60));
    }

    #[test]
    fn disabled_and_misconfigured_repositories_are_never_scheduled() {
        let cfg = config(
            hours(6),
            vec![
                RepoConfig { enabled: false, ..repo("off", "o/off") },
                repo("bad-name", "not a repo"),
                RepoConfig { interval: Some(mins(1)), ..repo("too-fast", "o/fast") },
            ],
        );
        let state = State::default();
        let s = Schedule::new();
        assert!(s.due_repos(&cfg, &state, T0).is_empty());
        assert_eq!(s.next_wake(&cfg, &state, T0), None);
    }

    #[test]
    fn with_nothing_configured_the_app_never_wakes() {
        assert_eq!(Schedule::new().next_wake(&Config::default(), &State::default(), T0), None);
    }

    #[test]
    fn an_invalid_default_interval_stops_inheriting_repositories_from_polling() {
        let cfg = config(mins(1), vec![repo("inherits", "o/a"), RepoConfig { interval: Some(hours(1)), ..repo("own", "o/b") }]);
        let state = State::default();
        assert_eq!(Schedule::new().due_repos(&cfg, &state, T0), ["own"]);
    }

    #[test]
    fn a_repository_left_unsettled_is_checked_once_after_a_start() {
        // Its last check held something, so it kept no ETag, and what was held was known only to the run that ended.
        let cfg = config(hours(24), vec![repo("held", "o/held"), repo("settled", "o/settled")]);
        let mut state = state_with(&[("held", T0), ("settled", T0)]);
        state.repo_mut("held").etag = None;
        let now = T0 + 60;
        let mut s = Schedule::new();
        assert_eq!(s.due_repos(&cfg, &state, now), ["held"]);
        assert_eq!(s.next_wake(&cfg, &state, now), Some(now));
        s.on_success("held");
        assert!(s.due_repos(&cfg, &state, now).is_empty(), "once per run");
        assert_eq!(s.next_wake(&cfg, &state, now), Some(T0 + 24 * 3600));

        let mut s = Schedule::new();
        s.on_failure("held", FailureKind::Network, None, hours(24), now);
        assert!(s.due_repos(&cfg, &state, now).is_empty(), "a failure counts as the check");
        s.forget("held");
        assert_eq!(s.due_repos(&cfg, &state, now), ["held"]);
    }

    #[test]
    fn forgetting_a_repository_drops_its_backoff() {
        let mut s = Schedule::new();
        s.on_failure("a", FailureKind::Auth, None, hours(1), T0);
        s.forget("a");
        assert!(s.retry_of("a").is_none());
    }
}
