//! The tray app without its windows: which job runs next, what the board shows and when to wake up.
//!
//! The UI thread owns an [`App`]. It hands one [`Order`] at a time to the worker thread, which owns the [`Engine`],
//! runs the order with [`perform`] and answers with a [`Done`]. Everything here is plain data, so the whole cycle can
//! be tested with a real engine and a fake GitHub, without a window.
//!
//! Guards against busy loops, whatever the engine answers:
//! * a job that failed as a whole (the engine could not work) is retried after 5 minutes, doubling up to an hour;
//! * a job that found the engine busy (`tss` was working) is retried after a minute;
//! * two timer-driven checks are at least 30 seconds apart;
//! * a timer that fires much later than it was set for (the computer slept) waits 30 seconds for the network.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use crate::board::{Board, Health, Level, capitalized};
use crate::config::Config;
use crate::engine::{
    Access, Engine, EngineError, Outcome, RemoveReport, Resolution, ResolveReport, Stamp, StartupReport, SyncOptions, SyncReport, VerifyReport, Which,
};
use crate::interval::UnixSecs;

/// How long to wait before trying again when another check held the engine.
pub const BUSY_RETRY: UnixSecs = 60;
/// The least time between two checks the timer starts.
pub const MIN_POLL_GAP: UnixSecs = 30;
/// After waking from sleep, the network gets this long to come back before checking.
pub const RESUME_GRACE: UnixSecs = 30;
/// A timer that fires this much later than it was set for means the computer slept.
pub const LATE_TIMER: UnixSecs = 60;
/// The first wait after the engine could not work at all. It doubles with every failure in a row.
pub const FAILURE_BACKOFF: UnixSecs = 5 * 60;
pub const MAX_FAILURE_BACKOFF: UnixSecs = 60 * 60;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Job {
    /// The first job, and the first for another skills folder: look at the disk and find out whether the skills folder
    /// can be used.
    Startup,
    /// Something changed on disk (in the skills folder, or `state.json` by `tss`): look again, change nothing.
    Look,
    /// The timer: check the repositories whose time has come. Never a look at the disk first.
    Poll,
    /// The user asked: look at the disk, then check every enabled repository (`None`) or one.
    Check(Option<String>),
    Resolve {
        repo: String,
        skill: String,
        how: Resolution,
    },
    /// Upstream lists no skills at all, and the user confirmed that the ones gone from it are deleted.
    DeleteGone(String),
    /// Stop managing a repository's skills before it leaves the settings, deleting the unedited ones if asked.
    Remove {
        repo: String,
        delete_skills: bool,
    },
    /// Quitting: save what is waiting to be saved.
    Flush,
    /// The settings changed: work out when to wake up next. Reads nothing.
    Plan,
}

impl Job {
    /// The tray shows "checking for updates…" while it runs.
    pub fn shows_work(&self) -> bool {
        !matches!(self, Job::Look | Job::Flush | Job::Plan)
    }

    /// The user asked for it, so they are told how it went.
    fn by_user(&self) -> bool {
        matches!(self, Job::Check(_) | Job::Resolve { .. } | Job::DeleteGone(_) | Job::Remove { .. })
    }
}

/// One job for the worker, with the settings it runs against. Settings that change while it runs are only used by
/// the jobs after it; changing them queues a `Plan`, so the next wake-up is always worked out with the newest.
#[derive(Clone, Debug)]
pub struct Order {
    pub job: Job,
    pub cfg: Config,
    pub skills_dir: PathBuf,
}

#[derive(Debug)]
pub enum Answer {
    Started(StartupReport),
    Looked(VerifyReport),
    Checked { verified: Option<VerifyReport>, sync: SyncReport },
    Resolved(ResolveReport),
    Removed(RemoveReport),
    Flushed,
    Planned,
}

/// What the worker sends back: the answer, and what the engine knows now.
#[derive(Debug)]
pub struct Done {
    pub job: Job,
    pub result: Result<Answer, EngineError>,
    pub next_wake: Option<UnixSecs>,
    pub last_checks: Vec<(String, UnixSecs)>,
    pub kept: Vec<(String, Vec<String>)>,
    /// Whether the last listing of each repository had no skills in it at all.
    pub lists_nothing: Vec<(String, bool)>,
    /// The stamp of `state.json` as this job left it.
    pub stamp: Option<Stamp>,
}

/// Runs one order. Called on the worker thread, which owns the engine.
pub fn perform(engine: &mut Engine, order: &Order, cancel: &AtomicBool) -> Done {
    if engine.paths().skills != order.skills_dir {
        engine.set_skills_dir(order.skills_dir.clone());
    }
    let cfg = &order.cfg;
    let result = match &order.job {
        Job::Startup => engine.startup(cfg).map(Answer::Started),
        Job::Look => engine.verify(cfg, Access::ReadOnly).map(Answer::Looked),
        Job::Poll => engine.sync(cfg, &Which::Due, &SyncOptions::default(), cancel).map(|sync| Answer::Checked { verified: None, sync }),
        Job::Check(which) => engine.verify(cfg, Access::Write).and_then(|verified| {
            let which = which.as_ref().map_or(Which::All, |id| Which::One(id.clone()));
            let sync = engine.sync(cfg, &which, &SyncOptions::default(), cancel)?;
            Ok(Answer::Checked { verified: Some(verified), sync })
        }),
        Job::Resolve { repo, skill, how } => engine.resolve(cfg, repo, skill, *how, cancel).map(Answer::Resolved),
        Job::DeleteGone(id) => {
            let opts = SyncOptions { ignore_etag: true, confirm_mass_delete: true, ..SyncOptions::default() };
            engine.sync(cfg, &Which::One(id.clone()), &opts, cancel).map(|sync| Answer::Checked { verified: None, sync })
        }
        Job::Remove { repo, delete_skills } => engine.remove_repo(repo, *delete_skills).map(Answer::Removed),
        Job::Flush => engine.flush().map(|()| Answer::Flushed),
        Job::Plan => Ok(Answer::Planned),
    };
    Done {
        job: order.job.clone(),
        result,
        next_wake: engine.next_wake(cfg),
        last_checks: cfg.repos.iter().filter_map(|r| engine.last_check(&r.id).map(|at| (r.id.clone(), at))).collect(),
        kept: cfg.repos.iter().map(|r| (r.id.clone(), engine.kept(&r.id))).collect(),
        lists_nothing: cfg.repos.iter().map(|r| (r.id.clone(), engine.lists_nothing(&r.id))).collect(),
        stamp: engine.state_stamp(),
    }
}

/// What the window has to do after a job.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Settled {
    /// The engine let go of this repository: take it out of the settings.
    pub removed: Option<String>,
    /// How a job the user asked for went, for the status line.
    pub feedback: Option<String>,
}

/// The state the UI thread keeps.
#[derive(Debug)]
pub struct App {
    pub board: Board,
    cfg: Config,
    skills_dir: PathBuf,
    queue: VecDeque<Job>,
    busy: Option<Job>,
    next_wake: Option<UnixSecs>,
    /// No timer-driven job starts before this.
    not_before: UnixSecs,
    /// Jobs in a row that failed as a whole.
    failures: u32,
    started: bool,
    /// When the timer was last set to fire.
    armed: Option<UnixSecs>,
    quitting: bool,
    state_stamp: Option<Stamp>,
}

impl App {
    /// Startup is the first job.
    pub fn new(cfg: Config, skills_dir: PathBuf) -> App {
        App {
            board: Board::new(),
            cfg,
            skills_dir,
            queue: VecDeque::from([Job::Startup]),
            busy: None,
            next_wake: None,
            not_before: 0,
            failures: 0,
            started: false,
            armed: None,
            quitting: false,
            state_stamp: None,
        }
    }

    pub fn cfg(&self) -> &Config {
        &self.cfg
    }

    pub fn skills_dir(&self) -> &PathBuf {
        &self.skills_dir
    }

    /// New settings. The next job uses them; when to wake up is worked out again. Another skills folder is taken in
    /// the way the app starts, so the change is recorded, and the user told, at once rather than at the next check.
    pub fn set_cfg(&mut self, cfg: Config, skills_dir: PathBuf) {
        let moved = skills_dir != self.skills_dir;
        self.cfg = cfg;
        self.skills_dir = skills_dir;
        if moved {
            self.started = false;
            self.want(Job::Startup);
        } else {
            self.want(Job::Plan);
        }
    }

    pub fn health(&self) -> Health {
        self.board.health(&self.cfg)
    }

    pub fn tooltip(&self) -> String {
        self.board.tooltip(&self.cfg)
    }

    pub fn started(&self) -> bool {
        self.started
    }

    pub fn busy(&self) -> Option<&Job> {
        self.busy.as_ref()
    }

    pub fn queued(&self) -> &VecDeque<Job> {
        &self.queue
    }

    pub fn is_idle(&self) -> bool {
        self.busy.is_none() && self.queue.is_empty()
    }

    pub fn next_wake(&self) -> Option<UnixSecs> {
        self.next_wake
    }

    /// The stamp of `state.json` as the last job left it. Another stamp on disk means another program changed it.
    pub fn state_stamp(&self) -> Option<Stamp> {
        self.state_stamp
    }

    /// Asks for a job. A job that one already waiting covers is dropped. Returns whether it was queued.
    pub fn want(&mut self, job: Job) -> bool {
        if self.quitting || self.queue.contains(&job) {
            return false;
        }
        let has = |q: &VecDeque<Job>, f: fn(&Job) -> bool| q.iter().any(f);
        let covered = match &job {
            Job::Poll | Job::Check(Some(_)) => has(&self.queue, |j| *j == Job::Check(None)),
            Job::Look => has(&self.queue, |j| matches!(j, Job::Startup | Job::Look | Job::Check(_))),
            Job::Plan => !self.queue.is_empty(),
            _ => false,
        };
        if covered {
            return false;
        }
        if job == Job::Check(None) {
            self.queue.retain(|j| !matches!(j, Job::Poll | Job::Look | Job::Check(_)));
        }
        if job == Job::Startup {
            self.queue.push_front(job);
        } else {
            self.queue.push_back(job);
        }
        true
    }

    /// The next order for the worker, unless it is still working on one.
    pub fn next_order(&mut self) -> Option<Order> {
        if self.busy.is_some() {
            return None;
        }
        let job = self.queue.pop_front()?;
        if job.shows_work() {
            self.board.set_working(true);
        }
        self.busy = Some(job.clone());
        self.armed = None;
        Some(Order { job, cfg: self.cfg.clone(), skills_dir: self.skills_dir.clone() })
    }

    /// Takes in what the worker sent back.
    pub fn finish(&mut self, done: Done, now: UnixSecs) -> Settled {
        self.busy = None;
        self.board.set_working(false);
        let mut settled = Settled::default();
        let job = done.job;
        let by_user = job.by_user();

        match done.result {
            Err(EngineError::Busy) => match job {
                Job::Startup | Job::Poll => self.hold(now + BUSY_RETRY),
                _ if by_user => settled.feedback = Some(capitalized(&EngineError::Busy.to_string())),
                _ => {}
            },
            Err(EngineError::Failed(why)) => {
                if by_user {
                    self.board.say(now, Level::Error, why.clone());
                    settled.feedback = Some(why);
                } else if matches!(job, Job::Startup | Job::Poll | Job::Look) {
                    self.failures += 1;
                    self.board.note_failure(now, Some(&why));
                    self.hold(now + backoff(self.failures));
                }
            }
            Ok(answer) => {
                if !matches!(answer, Answer::Flushed | Answer::Planned) {
                    self.failures = 0;
                    self.board.note_failure(now, None);
                }
                match answer {
                    Answer::Started(report) => {
                        self.board.note_startup(&report, now);
                        self.started = true;
                    }
                    Answer::Looked(report) => self.board.note_verify(&report, now),
                    Answer::Checked { verified, sync } => {
                        if let Some(report) = &verified {
                            self.board.note_verify(report, now);
                        }
                        self.board.note_sync(&sync, now);
                        if by_user {
                            settled.feedback = Some(summary(&sync));
                        }
                    }
                    Answer::Resolved(report) => {
                        if let Job::Resolve { repo, skill, how } = &job {
                            self.board.note_resolved(repo, skill, *how);
                        }
                        let mut text = format!("{}.", report.message);
                        for backup in &report.backups {
                            text.push_str(&format!(" The replaced copy is in {}.", backup.display()));
                        }
                        self.board.say(now, Level::Ok, text);
                        settled.feedback = Some(capitalized(&report.message));
                    }
                    Answer::Removed(report) => {
                        if let Job::Remove { repo, .. } = &job {
                            let text = self.removed_text(repo, &report);
                            self.board.forget_repo(repo);
                            self.board.say(now, if report.kept.is_empty() { Level::Ok } else { Level::Attention }, text.clone());
                            settled.feedback = Some(text);
                            settled.removed = Some(repo.clone());
                        }
                    }
                    Answer::Flushed | Answer::Planned => {}
                }
            }
        }

        self.next_wake = done.next_wake;
        for (id, at) in &done.last_checks {
            self.board.note_last_check(id, *at);
        }
        for (id, names) in &done.kept {
            self.board.note_kept(id, names);
        }
        for (id, nothing) in &done.lists_nothing {
            self.board.note_lists_nothing(id, *nothing);
        }
        self.state_stamp = done.stamp;
        if job == Job::Poll {
            self.hold(now + MIN_POLL_GAP);
        }
        settled
    }

    /// When the timer should fire next, or `None` for no timer. Call after every change.
    pub fn arm(&mut self, now: UnixSecs) -> Option<UnixSecs> {
        let at = if self.busy.is_some() || self.quitting || !self.queue.is_empty() {
            None
        } else if !self.started {
            Some(self.not_before)
        } else {
            self.next_wake.map(|wake| wake.max(self.not_before))
        };
        self.armed = at.map(|at| at.max(now));
        self.armed
    }

    /// The timer fired: queue what is due. A timer that is far too late means the computer slept, so wait a little.
    pub fn on_timer(&mut self, now: UnixSecs) {
        let expected = self.armed.take();
        if self.busy.is_some() || self.quitting {
            return;
        }
        if expected.is_some_and(|at| now > at + LATE_TIMER) {
            self.hold(now + RESUME_GRACE);
            return;
        }
        if now < self.not_before {
            return;
        }
        if !self.started {
            self.want(Job::Startup);
        } else if self.next_wake.is_some_and(|wake| wake <= now) {
            self.want(Job::Poll);
        }
    }

    /// The computer woke up: give the network a moment.
    pub fn resumed(&mut self, now: UnixSecs) {
        self.hold(now + RESUME_GRACE);
    }

    /// Nothing more starts; the queue ends with saving what is waiting to be saved.
    pub fn quit(&mut self) {
        self.queue.clear();
        self.queue.push_back(Job::Flush);
        self.quitting = true;
    }

    fn hold(&mut self, until: UnixSecs) {
        self.not_before = self.not_before.max(until);
    }

    fn removed_text(&self, repo: &str, report: &RemoveReport) -> String {
        let label = self.cfg.find(repo).map_or_else(|| repo.to_string(), |r| r.label());
        let mut text = format!("Removed {label}.");
        if !report.deleted.is_empty() {
            text.push_str(&format!(" Deleted: {}.", report.deleted.join(", ")));
        }
        if !report.kept.is_empty() {
            let kept: Vec<String> = report.kept.iter().map(|(name, why)| format!("{name} ({why})")).collect();
            text.push_str(&format!(" Kept as yours: {}.", kept.join(", ")));
        }
        text
    }
}

fn backoff(failures: u32) -> UnixSecs {
    let doublings = failures.saturating_sub(1).min(16);
    (FAILURE_BACKOFF << doublings).min(MAX_FAILURE_BACKOFF)
}

/// One line about a check the user asked for. The rows say the details. The line is cut at its end when it is too long,
/// so the reason comes first and the repository after it.
fn summary(report: &SyncReport) -> String {
    let failed: Vec<_> = report.repos.iter().filter_map(|r| if let Outcome::Failed(f) = &r.outcome { Some((r, f)) } else { None }).collect();
    // As the row's "No skills found": the listing was empty and nothing was left to do about it.
    let empty: Vec<_> = report.repos.iter().filter(|r| r.lists_nothing && r.skills.is_empty()).collect();
    let any = |f: fn(&Outcome) -> bool| report.repos.iter().any(|r| f(&r.outcome));
    match failed.as_slice() {
        _ if report.repos.is_empty() => "There is nothing to check: no repository is enabled".to_string(),
        [(repo, failure)] => format!("{} ({})", capitalized(&failure.message), repo.label),
        [_, ..] => format!("{} repositories could not be checked", failed.len()),
        [] if any(|o| matches!(o, Outcome::Cancelled)) => "The check was stopped".to_string(),
        [] if any(|o| matches!(o, Outcome::NeedsAttention)) => "Checked: some skills need your attention".to_string(),
        [] if empty.len() == 1 => format!("Checked: no skills found ({})", empty[0].label),
        [] if !empty.is_empty() => format!("Checked: no skills found in {} repositories", empty.len()),
        [] if any(|o| matches!(o, Outcome::Synced)) => "Checked: skills were updated".to_string(),
        [] => "Checked: everything is up to date".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Failure, RepoReport};
    use crate::github::FailureKind;

    fn repo_report(label: &str, outcome: Outcome) -> RepoReport {
        RepoReport {
            repo_id: label.to_string(),
            label: label.to_string(),
            outcome,
            skills: Vec::new(),
            rejected: Vec::new(),
            stats: Default::default(),
            downloaded: 0,
            finished_at: 0,
            lists_nothing: false,
        }
    }

    fn checked(repos: Vec<RepoReport>) -> SyncReport {
        SyncReport { repos, notes: Vec::new(), warnings: Vec::new() }
    }

    fn failed_check(label: &str, message: &str) -> SyncReport {
        let outcome = Outcome::Failed(Failure { kind: FailureKind::Network, message: message.to_string(), retry_at: None });
        checked(vec![repo_report(label, outcome)])
    }

    #[test]
    fn a_failed_check_says_why_first_because_the_status_line_cuts_the_end() {
        let lost = failed_check("owner/repo@main : skills", "the connection was lost");
        assert_eq!(summary(&lost), "The connection was lost (owner/repo@main : skills)");
        let refused = failed_check("owner/repo@main : skills", "GitHub refused access to owner/repo");
        assert_eq!(summary(&refused), "GitHub refused access to owner/repo (owner/repo@main : skills)");
    }

    #[test]
    fn a_check_that_found_no_skills_does_not_say_everything_is_up_to_date() {
        let empty = |label| RepoReport { lists_nothing: true, ..repo_report(label, Outcome::Unchanged) };
        assert_eq!(summary(&checked(vec![empty("o/a"), repo_report("o/b", Outcome::Synced)])), "Checked: no skills found (o/a)");
        assert_eq!(summary(&checked(vec![empty("o/a"), empty("o/b")])), "Checked: no skills found in 2 repositories");
        // Skills that need the user come first.
        let attention = checked(vec![empty("o/a"), repo_report("o/b", Outcome::NeedsAttention)]);
        assert_eq!(summary(&attention), "Checked: some skills need your attention");
        assert_eq!(summary(&checked(vec![repo_report("o/b", Outcome::Unchanged)])), "Checked: everything is up to date");
    }

    #[test]
    fn the_wait_after_failures_doubles_up_to_an_hour() {
        let waits: Vec<UnixSecs> = (1..=7).map(backoff).collect();
        assert_eq!(waits, [300, 600, 1200, 2400, 3600, 3600, 3600]);
        assert_eq!(backoff(u32::MAX), MAX_FAILURE_BACKOFF);
    }

    #[test]
    fn an_engine_error_reads_as_a_sentence() {
        assert_eq!(capitalized("another check"), "Another check");
        assert_eq!(capitalized(""), "");
    }
}
