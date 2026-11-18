//! What the tray and the window show: one honest line per repository and per skill.
//!
//! This is a pure model, so every sentence and every button can be tested. The engine reports what it found
//! (`StartupReport`, `VerifyReport`, `SyncReport`); the board remembers the newest word about each skill and turns it
//! into text, a level (fine, needs attention, error) and the buttons that make sense.
//!
//! Two kinds of news arrive and are kept apart:
//! * a look at the disk (`verify`): is the folder clean, edited or gone? It knows nothing about upstream.
//! * a check against GitHub (`sync`): what upstream says. It also says what the disk looked like at that moment.
//!
//! The newest news about a skill wins. A check that fails, or that finds "not modified", says nothing new about the
//! skills, so the rows from the last real listing stay.

use std::collections::{BTreeMap, HashMap, VecDeque};

use crate::apply::Note;
use crate::config::{Config, RepoConfig};
use crate::engine::{ApplyResult, Failure, LocalStatus, Outcome, RepoReport, Resolution, SkillReport, StartupReport, SyncReport, VerifyReport};
use crate::github::FailureKind;
use crate::interval::UnixSecs;
use crate::pathsafe::fold;
use crate::reconcile::{Hold, LocalState, Skip, Verdict};

pub const APP_NAME: &str = "Tiny Skill Sync";
/// A tray tooltip holds 127 UTF-16 units (128 with the closing zero).
pub const TOOLTIP_UNITS: usize = 127;
const MAX_MESSAGES: usize = 30;

/// "owner/repo" from a label such as "owner/repo@main : skills"; neither part of a repository name has '@' or a space.
pub fn repo_name(label: &str) -> &str {
    label.split(['@', ' ']).next().unwrap_or(label)
}

/// How serious something is. The worst one wins when levels are rolled up.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Ok,
    Attention,
    Error,
}

/// The state of the whole app, for the tray icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Health {
    NotConfigured,
    Ok,
    /// Started at sign-in, the app has not looked at anything yet: it lets Windows finish starting first.
    Waiting,
    Working,
    Attention,
    Error,
}

/// Something the user can do about a skill. A row lists only the ones that make sense for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Take upstream's version. Whatever is in the folder now is kept as a backup.
    Overwrite,
    /// The folder was deleted here: bring upstream's version back.
    Restore,
    /// The folder is the user's: stop managing it.
    KeepMine,
    /// The folder was deleted here on purpose: never install it again.
    LeaveRemoved,
    /// Undo "keep mine".
    ManageAgain,
    PauseHistory,
    ResumeHistory,
    /// Upstream lists no skills at all: delete the skills that are gone from it. This is a check with the
    /// mass-deletion confirmation, not a `Resolution`.
    RemoveThem,
}

impl Action {
    pub fn label(self) -> &'static str {
        match self {
            Action::Overwrite => "Use the GitHub version",
            Action::Restore => "Restore",
            Action::KeepMine => "Keep my version",
            Action::LeaveRemoved => "Leave it deleted",
            Action::ManageAgain => "Manage again",
            Action::PauseHistory => "Cancel search",
            Action::ResumeHistory => "Resume search",
            Action::RemoveThem => "Delete them",
        }
    }

    /// What to ask the engine to do. `None` for `RemoveThem`, which is a check with `confirm_mass_delete`.
    pub fn resolution(self) -> Option<Resolution> {
        match self {
            Action::Overwrite | Action::Restore => Some(Resolution::Overwrite),
            Action::KeepMine | Action::LeaveRemoved => Some(Resolution::KeepMine),
            Action::ManageAgain => Some(Resolution::ManageAgain),
            Action::PauseHistory => Some(Resolution::PauseHistory),
            Action::ResumeHistory => Some(Resolution::ResumeHistory),
            Action::RemoveThem => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub name: String,
    /// The whole sentence: what is the matter and what the app will do.
    pub text: String,
    /// A few words for a label next to the name: "Up to date", "Edited on this PC".
    pub tag: &'static str,
    pub level: Level,
    pub actions: Vec<Action>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoView {
    pub id: String,
    pub label: String,
    pub enabled: bool,
    /// Short: "Up to date", "Needs attention (2)", "No connection".
    pub status: String,
    /// The whole sentence behind a failed check.
    pub detail: Option<String>,
    pub level: Level,
    /// The last check that got a real answer from GitHub.
    pub last_check: Option<UnixSecs>,
    /// Read with the GitHub token: the user said so, or GitHub could not find it without one.
    pub private: bool,
    /// What needs the user first, then by name.
    pub skills: Vec<Row>,
}

impl RepoView {
    pub fn last_check_text(&self, now: UnixSecs) -> String {
        match self.last_check {
            Some(at) => ago(now, at),
            None => "never".to_string(),
        }
    }
}

/// Something worth telling the user once: what recovery did after a crash, or a problem that was not fatal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub at: UnixSecs,
    pub level: Level,
    pub text: String,
}

/// What GitHub said about a skill, boiled down to what the screen needs.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Fact {
    InSync,
    /// Will be installed.
    New,
    /// An update is waiting.
    Newer,
    /// Gone upstream, so it goes here too.
    Removed,
    /// An identical folder exists and will be taken over.
    Adopting,
    Historical(bool),
    Searching,
    HistoryWaiting,
    Paused,
    /// A different folder with this name already exists.
    NameTaken,
    /// Upstream lists no skills at all, so nothing was deleted.
    EmptyListing,
    /// The user keeps it as their own.
    Kept,
    /// The user asked for it to be managed again; the next check decides how.
    Waiting,
    Clash(String),
    NotSafe(String),
    /// The check left it alone, for this reason.
    Stopped(String),
    Failed(String),
}

impl Fact {
    /// These only make sense for a skill we own. Once a look at the disk finds no such skill, they are out of date.
    fn needs_ownership(&self) -> bool {
        matches!(self, Fact::InSync | Fact::Newer | Fact::Removed | Fact::EmptyListing)
    }
}

#[derive(Debug)]
struct SkillBoard {
    name: String,
    local: Option<LocalStatus>,
    fact: Option<Fact>,
}

impl SkillBoard {
    /// `None` when the skill is gone and has nothing left to show.
    fn from_report(report: &SkillReport) -> Option<SkillBoard> {
        let SkillReport { name, verdict, result } = report;
        let done = *result == ApplyResult::Done;
        if done && matches!(verdict, Verdict::Delete | Verdict::Forget) {
            return None;
        }
        let fact = match result {
            ApplyResult::Held(why) => Some(Fact::Stopped(why.clone())),
            ApplyResult::Failed(why) => Some(Fact::Failed(why.clone())),
            _ => fact_of(verdict, done),
        };
        Some(SkillBoard { name: name.clone(), local: hint_of(verdict), fact })
    }

    fn row(&self) -> Row {
        let shown = present(self.local.as_ref(), self.fact.as_ref());
        Row { name: self.name.clone(), text: shown.text, tag: shown.tag, level: shown.level, actions: shown.actions }
    }
}

/// What upstream says, from a verdict. `None` only for a skill whose last change was interrupted: nothing is known.
fn fact_of(verdict: &Verdict, done: bool) -> Option<Fact> {
    Some(match verdict {
        Verdict::InSync | Verdict::ModifiedLocally(_) | Verdict::MissingLocally => Fact::InSync,
        Verdict::Install if done => Fact::InSync,
        Verdict::Install => Fact::New,
        Verdict::Update if done => Fact::InSync,
        Verdict::Update => Fact::Newer,
        Verdict::Adopt if done => Fact::InSync,
        Verdict::Adopt => Fact::Adopting,
        Verdict::AdoptHistorical => Fact::Historical(done),
        Verdict::Delete | Verdict::Forget => Fact::Removed,
        Verdict::Held(Hold::UpdateBlocked(_) | Hold::UpdateBlockedMissing) => Fact::Newer,
        Verdict::Held(Hold::DeleteBlocked(_)) => Fact::Removed,
        Verdict::Held(Hold::UnmanagedDiffers) => Fact::NameTaken,
        Verdict::Held(Hold::HistorySearching) => Fact::Searching,
        Verdict::Held(Hold::HistoryPaused) => Fact::Paused,
        Verdict::Held(Hold::MassDeletion) => Fact::EmptyListing,
        Verdict::Held(Hold::Interrupted) => return None,
        Verdict::Skipped(Skip::Ignored) => Fact::Kept,
        Verdict::Skipped(Skip::Collision { owner }) => Fact::Clash(owner.clone()),
        Verdict::Skipped(Skip::Rejected { reason }) => Fact::NotSafe(reason.clone()),
    })
}

/// What the disk looked like when the check decided, as far as the verdict tells. Only an edit, a deletion or an
/// interruption is worth remembering: a folder that is fine is shown the same with or without a hint.
fn hint_of(verdict: &Verdict) -> Option<LocalStatus> {
    let state = match verdict {
        Verdict::Forget | Verdict::MissingLocally | Verdict::Held(Hold::UpdateBlockedMissing) => LocalState::FolderMissing,
        Verdict::ModifiedLocally(info) | Verdict::Held(Hold::UpdateBlocked(info) | Hold::DeleteBlocked(info)) => LocalState::Dirty(info.clone()),
        Verdict::Held(Hold::Interrupted) => return Some(LocalStatus::Interrupted),
        _ => return None,
    };
    Some(LocalStatus::Checked(state))
}

struct Shown {
    text: String,
    tag: &'static str,
    level: Level,
    actions: Vec<Action>,
}

fn shown(text: impl Into<String>, tag: &'static str, level: Level, actions: &[Action]) -> Shown {
    Shown { text: text.into(), tag, level, actions: actions.to_vec() }
}

const EDITED: [Action; 2] = [Action::Overwrite, Action::KeepMine];
const DELETED: [Action; 2] = [Action::Restore, Action::LeaveRemoved];
pub const UP_TO_DATE_TAG: &str = "Up to date";
pub const INSTALLED_TAG: &str = "Installed";
pub const EDITED_TAG: &str = "Edited on this PC";
pub const DELETED_TAG: &str = "Deleted on this PC";
const UPDATE_TAG: &str = "Update available";
const REMOVED_TAG: &str = "Removed on GitHub";
const NEW_TAG: &str = "New";
const DIFFERENT_TAG: &str = "Different copy here";
const LISTS_NOTHING_TAG: &str = "GitHub lists nothing";
const KEPT_TAG: &str = "Kept as yours";
const CLASH_TAG: &str = "Name clash";
const NOT_SAFE_TAG: &str = "Not safe";
const NOT_CHANGED_TAG: &str = "Not changed";
const FAILED_TAG: &str = "Failed";
const INTERRUPTED_TAG: &str = "Interrupted";

impl Row {
    /// "Show changes" has something to show: a copy here that may differ from GitHub's, under a name that is safe to
    /// look up and in a folder this repository owns.
    pub fn can_compare(&self) -> bool {
        matches!(
            self.tag,
            EDITED_TAG | UPDATE_TAG | REMOVED_TAG | DIFFERENT_TAG | LISTS_NOTHING_TAG | KEPT_TAG | NOT_CHANGED_TAG | FAILED_TAG | INTERRUPTED_TAG
        )
    }

    /// There may be a folder to open.
    pub fn has_folder(&self) -> bool {
        !matches!(self.tag, DELETED_TAG | NEW_TAG | NOT_SAFE_TAG)
    }
}

/// Puts what is known about the disk and about upstream together. An edit or a deletion is the user's doing and is
/// shown the same way whatever upstream says; the choices differ only where upstream has no version to offer.
fn present(local: Option<&LocalStatus>, fact: Option<&Fact>) -> Shown {
    use Level::{Attention, Ok};
    if let Some(LocalStatus::Interrupted) = local {
        return shown(Verdict::Held(Hold::Interrupted).message(), INTERRUPTED_TAG, Attention, &[]);
    }
    let state = match local {
        Some(LocalStatus::Checked(state)) => Some(state),
        _ => None,
    };
    match fact {
        None | Some(Fact::InSync) => match state {
            Some(LocalState::Dirty(info)) => shown(Verdict::ModifiedLocally(info.clone()).message(), EDITED_TAG, Attention, &EDITED),
            Some(LocalState::FolderMissing) => shown(Verdict::MissingLocally.message(), DELETED_TAG, Attention, &DELETED),
            _ if fact.is_some() => shown(Verdict::InSync.message(), UP_TO_DATE_TAG, Ok, &[]),
            _ => shown("Installed", INSTALLED_TAG, Ok, &[]),
        },
        Some(Fact::Newer) => match state {
            Some(LocalState::Dirty(info)) => shown(Verdict::Held(Hold::UpdateBlocked(info.clone())).message(), EDITED_TAG, Attention, &EDITED),
            Some(LocalState::FolderMissing) => shown(Verdict::Held(Hold::UpdateBlockedMissing).message(), DELETED_TAG, Attention, &DELETED),
            _ => shown("Update available: will be installed at the next check", UPDATE_TAG, Ok, &[]),
        },
        Some(Fact::Removed) => match state {
            Some(LocalState::Dirty(info)) => shown(Verdict::Held(Hold::DeleteBlocked(info.clone())).message(), EDITED_TAG, Attention, &[Action::KeepMine]),
            Some(LocalState::FolderMissing) => shown(Verdict::Forget.message(), REMOVED_TAG, Ok, &[]),
            _ => shown(Verdict::Delete.message(), REMOVED_TAG, Ok, &[]),
        },
        Some(Fact::New) => shown(Verdict::Install.message(), NEW_TAG, Ok, &[]),
        Some(Fact::Adopting) => shown("Already installed and identical: will be managed at the next check", "Will be managed", Ok, &[]),
        Some(Fact::Historical(true)) => shown("An older copy from GitHub was verified, adopted and updated", "Older copy updated", Ok, &[]),
        Some(Fact::Historical(false)) => shown(Verdict::AdoptHistorical.message(), "Older GitHub copy", Ok, &[]),
        Some(Fact::Searching) => {
            shown(Verdict::Held(Hold::HistorySearching).message(), "Searching history", Attention, &[Action::PauseHistory, Action::Overwrite, Action::KeepMine])
        }
        Some(Fact::HistoryWaiting) => shown(
            "Earlier-version search is waiting before retrying GitHub; only this skill waits",
            "Search waiting",
            Attention,
            &[Action::PauseHistory, Action::Overwrite, Action::KeepMine],
        ),
        Some(Fact::Paused) => {
            shown(Verdict::Held(Hold::HistoryPaused).message(), "Search paused", Attention, &[Action::ResumeHistory, Action::Overwrite, Action::KeepMine])
        }
        Some(Fact::NameTaken) => shown(Verdict::Held(Hold::UnmanagedDiffers).message(), DIFFERENT_TAG, Attention, &EDITED),
        Some(Fact::EmptyListing) => shown(Verdict::Held(Hold::MassDeletion).message(), LISTS_NOTHING_TAG, Attention, &[Action::RemoveThem, Action::KeepMine]),
        Some(Fact::Kept) => shown(Verdict::Skipped(Skip::Ignored).message(), KEPT_TAG, Ok, &[Action::ManageAgain]),
        Some(Fact::Waiting) => shown("Managed again: the next check will handle it", "Managed again", Ok, &[]),
        Some(Fact::Clash(owner)) => shown(Verdict::Skipped(Skip::Collision { owner: owner.clone() }).message(), CLASH_TAG, Attention, &[]),
        Some(Fact::NotSafe(reason)) => shown(Verdict::Skipped(Skip::Rejected { reason: reason.clone() }).message(), NOT_SAFE_TAG, Attention, &[]),
        Some(Fact::Stopped(why)) => shown(format!("Not changed: {why}"), NOT_CHANGED_TAG, Attention, &[]),
        Some(Fact::Failed(why)) => shown(format!("Failed: {why}"), FAILED_TAG, Level::Error, &[]),
    }
}

#[derive(Debug, Default)]
struct RepoBoard {
    /// Keyed by the case-folded name, so the order is stable and `Foo` and `foo` are one skill.
    skills: BTreeMap<String, SkillBoard>,
    /// How the latest check ended.
    check: Option<Outcome>,
    last_check: Option<UnixSecs>,
    /// The last listing had no skills in it at all.
    lists_nothing: bool,
    /// GitHub could not find the repository without the token.
    needs_token: bool,
}

impl RepoBoard {
    fn rows(&self) -> Vec<Row> {
        let mut rows: Vec<Row> = self.skills.values().map(SkillBoard::row).collect();
        rows.sort_by_key(|row| std::cmp::Reverse(row.level));
        rows
    }

    /// A listing is the whole truth about a repository's skills, so it replaces every row.
    fn replace_skills(&mut self, report: &RepoReport) {
        self.skills.clear();
        for skill in &report.skills {
            if let Some(board) = SkillBoard::from_report(skill) {
                self.skills.insert(fold(&skill.name), board);
            }
        }
        // A rejected skill that is not installed has no decision, so it only appears here.
        for rejected in &report.rejected {
            self.skills.entry(fold(&rejected.name)).or_insert_with(|| SkillBoard {
                name: rejected.name.clone(),
                local: None,
                fact: Some(Fact::NotSafe(rejected.reason.clone())),
            });
        }
    }
}

#[derive(Debug, Default)]
pub struct Board {
    repos: HashMap<String, RepoBoard>,
    orphans: Vec<String>,
    problem: Option<String>,
    /// The engine itself could not do its work (for example `state.json` cannot be read). Nothing is checked until
    /// a later call succeeds.
    failure: Option<String>,
    working: bool,
    /// Started at sign-in, nothing was looked at yet: the app waits for Windows to finish starting.
    waiting: bool,
    messages: VecDeque<Message>,
}

impl Board {
    pub fn new() -> Board {
        Board::default()
    }

    /// Run once at startup: the look at the disk, and whether the skills folder can be used at all.
    pub fn note_startup(&mut self, report: &StartupReport, now: UnixSecs) {
        self.problem = report.problem.clone();
        self.note_verify(&report.verify, now);
    }

    /// A look at the disk replaces what was known about every folder. What upstream said stays.
    pub fn note_verify(&mut self, report: &VerifyReport, now: UnixSecs) {
        for repo in self.repos.values_mut() {
            for skill in repo.skills.values_mut() {
                skill.local = None;
            }
        }
        for found in &report.skills {
            let repo = self.repos.entry(found.repo_id.clone()).or_default();
            let skill = repo.skills.entry(fold(&found.name)).or_insert_with(|| SkillBoard { name: found.name.clone(), local: None, fact: None });
            skill.local = Some(found.status.clone());
            if let LocalStatus::History { paused, complete, waiting } = found.status {
                skill.fact = Some(if paused {
                    Fact::Paused
                } else if complete {
                    Fact::NameTaken
                } else if waiting {
                    Fact::HistoryWaiting
                } else {
                    Fact::Searching
                });
            }
        }
        for repo in self.repos.values_mut() {
            repo.skills.retain(|_, skill| skill.local.is_some() || skill.fact.as_ref().is_some_and(|fact| !fact.needs_ownership()));
        }
        self.orphans = report.orphans.clone();
        self.tell(now, &report.notes, &report.warnings);
    }

    /// Only real checks belong here, not dry runs.
    pub fn note_sync(&mut self, report: &SyncReport, now: UnixSecs) {
        for repo_report in &report.repos {
            let repo = self.repos.entry(repo_report.repo_id.clone()).or_default();
            // A listing replaces the rows, even one with nothing in it. No listing at all means the check did not get
            // as far, or upstream was not modified: keep the rows.
            if repo_report.lists_nothing || !repo_report.skills.is_empty() || !repo_report.rejected.is_empty() {
                repo.replace_skills(repo_report);
                repo.lists_nothing = repo_report.lists_nothing;
            }
            if matches!(repo_report.outcome, Outcome::Unchanged | Outcome::Synced | Outcome::NeedsAttention) {
                repo.last_check = Some(repo_report.finished_at);
            }
            repo.check = Some(repo_report.outcome.clone());
        }
        for repo_report in &report.repos {
            self.tell_changes(now, repo_report);
        }
        self.tell(now, &report.notes, &report.warnings);
    }

    /// One message for each kind of change a check made on this PC. Up to three skills are named, more are counted.
    fn tell_changes(&mut self, now: UnixSecs, report: &RepoReport) {
        let from = repo_name(&report.label);
        for verdict in [Verdict::Install, Verdict::Update, Verdict::Delete, Verdict::Adopt, Verdict::AdoptHistorical] {
            let names: Vec<&str> = report.skills.iter().filter(|s| s.result == ApplyResult::Done && s.verdict == verdict).map(|s| s.name.as_str()).collect();
            let what = match names.as_slice() {
                [] => continue,
                [one] => one.to_string(),
                [a, b] => format!("{a} and {b}"),
                [a, b, c] => format!("{a}, {b} and {c}"),
                many => format!("{} skills", many.len()),
            };
            let (them, they) = if names.len() == 1 { ("it", "it is") } else { ("them", "they are") };
            let text = match verdict {
                Verdict::Install => format!("Installed {what} from {from}"),
                Verdict::Update => format!("Updated {what} from {from}"),
                Verdict::Delete => format!("Deleted {what}: {from} no longer has {them}"),
                Verdict::AdoptHistorical => format!("Verified and adopted an older GitHub copy of {what}, then updated {them} from {from}"),
                _ => format!("{what} on this PC matched {from}, so {they} now kept in sync"),
            };
            self.say(now, Level::Ok, text);
        }
    }

    /// The user settled one skill, so the screen can say so before the next check.
    pub fn note_resolved(&mut self, repo_id: &str, skill: &str, how: Resolution) {
        let repo = self.repos.entry(repo_id.to_string()).or_default();
        let key = fold(skill);
        let name = repo.skills.get(&key).map_or_else(|| skill.to_string(), |s| s.name.clone());
        let (fact, local) = match how {
            Resolution::Overwrite => (Fact::InSync, None),
            Resolution::KeepMine => (Fact::Kept, None),
            Resolution::ManageAgain => (Fact::Waiting, None),
            Resolution::PauseHistory => (Fact::Paused, None),
            Resolution::ResumeHistory => (Fact::Searching, None),
        };
        repo.skills.insert(key, SkillBoard { name, local, fact: Some(fact) });
    }

    /// Seeds the time of the last check from what the engine remembers.
    pub fn note_last_check(&mut self, repo_id: &str, at: UnixSecs) {
        self.repos.entry(repo_id.to_string()).or_default().last_check = Some(at);
    }

    /// Seeds whether the last listing of a repository had no skills in it, from what the engine remembers.
    pub fn note_lists_nothing(&mut self, repo_id: &str, nothing: bool) {
        if nothing || self.repos.contains_key(repo_id) {
            self.repos.entry(repo_id.to_string()).or_default().lists_nothing = nothing;
        }
    }

    /// Whether the engine found that the repository can only be read with the token, from what it remembers.
    pub fn note_private(&mut self, repo_id: &str, needs_token: bool) {
        if needs_token || self.repos.contains_key(repo_id) {
            self.repos.entry(repo_id.to_string()).or_default().needs_token = needs_token;
        }
    }

    /// The skills the user keeps as their own, as recorded. A look at the disk only covers the skills the app manages,
    /// so these are shown from the record, and a name that is no longer kept stops being shown as kept.
    pub fn note_kept(&mut self, repo_id: &str, names: &[String]) {
        let kept: Vec<String> = names.iter().map(|name| fold(name)).collect();
        if kept.is_empty() && !self.repos.contains_key(repo_id) {
            return;
        }
        let repo = self.repos.entry(repo_id.to_string()).or_default();
        repo.skills.retain(|key, skill| skill.fact != Some(Fact::Kept) || kept.contains(key));
        for (name, key) in names.iter().zip(kept) {
            repo.skills.entry(key).or_insert_with(|| SkillBoard { name: name.clone(), local: None, fact: Some(Fact::Kept) });
        }
    }

    /// The engine could not do its work at all (`Some`), or did it again (`None`). A new failure is also told once.
    pub fn note_failure(&mut self, now: UnixSecs, failure: Option<&str>) {
        if let Some(why) = failure {
            if self.failure.as_deref() != Some(why) {
                self.say(now, Level::Error, why.to_string());
            }
        }
        self.failure = failure.map(str::to_string);
    }

    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    /// The repository was removed from the settings.
    pub fn forget_repo(&mut self, repo_id: &str) {
        self.repos.remove(repo_id);
    }

    pub fn set_working(&mut self, working: bool) {
        self.working = working;
    }

    pub fn set_waiting(&mut self, waiting: bool) {
        self.waiting = waiting;
    }

    /// Repositories that have skills on record but are no longer in the settings. Their skills stay protected.
    pub fn orphans(&self) -> &[String] {
        &self.orphans
    }

    /// Set when the skills folder cannot be used for changes at all.
    pub fn problem(&self) -> Option<&str> {
        self.problem.as_deref()
    }

    pub fn messages(&self) -> impl Iterator<Item = &Message> {
        self.messages.iter()
    }

    /// One entry per repository in the settings, in the same order.
    pub fn views(&self, cfg: &Config, now: UnixSecs) -> Vec<RepoView> {
        cfg.repos.iter().map(|repo| self.view_of(repo, now)).collect()
    }

    pub fn health(&self, cfg: &Config) -> Health {
        if self.working {
            return Health::Working;
        }
        if !cfg.repos.iter().any(|repo| repo.enabled) {
            return Health::NotConfigured;
        }
        let from_repos = self.views(cfg, 0).iter().filter(|view| view.enabled).map(|view| view.level).max().unwrap_or(Level::Ok);
        let from_problem = if self.problem.is_some() || self.failure.is_some() { Level::Error } else { Level::Ok };
        match from_repos.max(from_problem) {
            // Nothing was looked at yet, so all is not known to be well.
            Level::Ok if self.waiting => Health::Waiting,
            Level::Ok => Health::Ok,
            Level::Attention => Health::Attention,
            Level::Error => Health::Error,
        }
    }

    /// The tray tooltip. It says what the state is, never when: it is not redrawn as time passes.
    pub fn tooltip(&self, cfg: &Config) -> String {
        let views = self.views(cfg, 0);
        let enabled = || views.iter().filter(|view| view.enabled);
        let body = match self.health(cfg) {
            Health::Working => "checking for updates…".to_string(),
            Health::Waiting => "waiting for Windows to finish starting".to_string(),
            Health::NotConfigured => "no repositories are set up".to_string(),
            Health::Ok => "all skills are up to date".to_string(),
            Health::Attention => {
                let count: usize = enabled().map(|view| view.skills.iter().filter(|row| row.level >= Level::Attention).count()).sum();
                // A repository that needs attention without a single skill to blame is one that lists none.
                let empty: Vec<&RepoView> = enabled().filter(|view| view.level >= Level::Attention && view.skills.is_empty()).collect();
                match (count, empty.as_slice()) {
                    (0, [one]) => format!("{}: {}", one.label, one.status),
                    (0, many) if !many.is_empty() => format!("{} repositories have no skills", many.len()),
                    (1, _) => "1 skill needs attention".to_string(),
                    (n, _) => format!("{n} skills need attention"),
                }
            }
            Health::Error => {
                let failed: Vec<&RepoView> = enabled().filter(|view| view.level == Level::Error).collect();
                match (self.problem.as_ref().or(self.failure.as_ref()), failed.as_slice()) {
                    (Some(problem), _) => problem.clone(),
                    (None, [one]) => format!("{}: {}", one.label, one.status),
                    (None, many) => format!("{} repositories could not be checked", many.len()),
                }
            }
        };
        cut(&format!("{APP_NAME}: {body}"), TOOLTIP_UNITS)
    }

    fn view_of(&self, repo: &RepoConfig, now: UnixSecs) -> RepoView {
        let board = self.repos.get(&repo.id);
        let skills = board.map(RepoBoard::rows).unwrap_or_default();
        let (level, status, detail) = judge(repo, board, &skills, now);
        RepoView {
            id: repo.id.clone(),
            label: repo.label(),
            enabled: repo.enabled,
            status,
            detail,
            level,
            last_check: board.and_then(|b| b.last_check),
            private: repo.use_token || board.is_some_and(|b| b.needs_token),
            skills,
        }
    }

    fn tell(&mut self, now: UnixSecs, notes: &[Note], warnings: &[String]) {
        for note in notes {
            self.say(now, Level::Ok, format!("{}: {}", note.skill, note.text));
        }
        for warning in warnings {
            self.say(now, Level::Attention, warning.clone());
        }
    }

    /// Adds a message to the log the window's Activity page shows.
    pub fn say(&mut self, at: UnixSecs, level: Level, text: String) {
        // The same message again is the same message: only when it was last seen changes.
        self.messages.retain(|m| !(m.level == level && m.text == text));
        if self.messages.len() == MAX_MESSAGES {
            self.messages.pop_front();
        }
        self.messages.push_back(Message { at, level, text });
    }
}

/// The level, the short status and the long explanation of one repository. A disabled repository is the user's
/// choice and counts for nothing; a failed check is an error until a later check gets an answer.
fn judge(repo: &RepoConfig, board: Option<&RepoBoard>, rows: &[Row], now: UnixSecs) -> (Level, String, Option<String>) {
    if !repo.enabled {
        return (Level::Ok, "Disabled".to_string(), None);
    }
    let outcome = board.and_then(|b| b.check.as_ref());
    if let Some(Outcome::Failed(failure)) = outcome {
        return (Level::Error, short(failure.kind).to_string(), Some(explain(failure, now)));
    }
    let level = rows.iter().map(|row| row.level).max().unwrap_or(Level::Ok);
    let attention = rows.iter().filter(|row| row.level >= Level::Attention).count();
    let never_checked = board.is_none_or(|b| b.last_check.is_none());
    // A listing with nothing in it is not "up to date": it is usually a repository added as a whole when its skills
    // are in a subfolder, and without this it would sit there looking fine while nothing was ever installed. Only the
    // listing tells: a repository whose skills were all held or skipped has no rows after a restart either.
    let lists_nothing = board.is_some_and(|b| b.lists_nothing);
    if lists_nothing && rows.is_empty() && matches!(outcome, None | Some(Outcome::Unchanged | Outcome::Synced)) {
        return (Level::Attention, NO_SKILLS.to_string(), Some(no_skills_advice(repo)));
    }
    let status = match outcome {
        _ if attention > 0 => format!("Needs attention ({attention})"),
        Some(Outcome::Cancelled) => "Check cancelled".to_string(),
        Some(Outcome::WouldChange) => "Changes pending".to_string(),
        None if never_checked => "Not checked yet".to_string(),
        _ => "Up to date".to_string(),
    };
    (level, status, None)
}

const NO_SKILLS: &str = "No skills found";

/// What to do when a repository has no skills where the app looked. A skill is a folder with a SKILL.md in it.
fn no_skills_advice(repo: &RepoConfig) -> String {
    let name = &repo.repo;
    let skill = "A skill is a folder with a SKILL.md file in it.";
    if repo.path.is_empty() {
        let branch = if repo.branch.is_empty() { "main" } else { &repo.branch };
        format!(
            "No skills were found at the top of {name}. {skill} If yours are in a subfolder, remove this repository and add the address of that folder, \
             for example https://github.com/{name}/tree/{branch}/skills"
        )
    } else {
        format!("No skills were found in the folder '{}' of {name}. {skill} Check that the folder and the branch are right.", repo.path)
    }
}

fn short(kind: FailureKind) -> &'static str {
    match kind {
        FailureKind::Auth => "Access refused",
        FailureKind::NotFound => "Not found",
        FailureKind::RateLimited => "Rate limited",
        FailureKind::Server => "GitHub is having problems",
        FailureKind::Redirect => "Repository moved",
        FailureKind::Protocol => "Unexpected answer",
        FailureKind::Network => "No connection",
        FailureKind::TooLarge => "Too large",
        FailureKind::Cancelled => "Cancelled",
        FailureKind::Changed => "Changed while checking",
        FailureKind::Local => "Could not update files",
    }
}

/// The first letter in upper case, so a message from deeper down reads as a sentence.
pub fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map(|first| first.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

fn explain(failure: &Failure, now: UnixSecs) -> String {
    let message = capitalized(&failure.message);
    match failure.retry_at.filter(|&at| at > now) {
        Some(at) => format!("{message} Next try in {}.", span(at - now)),
        None => message,
    }
}

/// "5 minutes ago". A time in the future (the clock moved) reads as "just now".
pub fn ago(now: UnixSecs, then: UnixSecs) -> String {
    let secs = now - then;
    if secs < 60 { "just now".to_string() } else { format!("{} ago", span(secs)) }
}

/// "5 minutes", "2 hours", "3 days": the biggest whole unit, rounded down.
pub fn span(secs: i64) -> String {
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    const TWO_WEEKS: i64 = 14 * DAY;
    const TWO_MONTHS: i64 = 60 * DAY;
    let (n, unit) = match secs {
        ..MINUTE => return "less than a minute".to_string(),
        MINUTE..HOUR => (secs / MINUTE, "minute"),
        HOUR..DAY => (secs / HOUR, "hour"),
        DAY..TWO_WEEKS => (secs / DAY, "day"),
        TWO_WEEKS..TWO_MONTHS => (secs / (7 * DAY), "week"),
        TWO_MONTHS.. => (secs / (30 * DAY), "month"),
    };
    format!("{n} {unit}{}", if n == 1 { "" } else { "s" })
}

/// Shortens text to at most `max_units` UTF-16 units, ending in "…", without ever splitting a character.
fn cut(text: &str, max_units: usize) -> String {
    if text.encode_utf16().count() <= max_units {
        return text.to_string();
    }
    let mut out = String::new();
    let mut units = 0;
    for c in text.chars() {
        // One unit is kept back for the ellipsis.
        if units + c.len_utf16() + 1 > max_units {
            break;
        }
        out.push(c);
        units += c.len_utf16();
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::engine::{LocalReport, Tracking};
    use crate::reconcile::DirtyInfo;
    use crate::tree::Rejected;
    use crate::verify::{Check, Stats};

    const NOW: UnixSecs = 1_800_000_000;
    const CHECKED: UnixSecs = NOW - 30;

    fn repo(id: &str, enabled: bool) -> RepoConfig {
        RepoConfig { id: id.to_string(), repo: format!("owner/{id}"), enabled, ..RepoConfig::default() }
    }

    fn cfg(repos: &[(&str, bool)]) -> Config {
        Config { repos: repos.iter().map(|(id, enabled)| repo(id, *enabled)).collect(), ..Config::default() }
    }

    fn one() -> Config {
        cfg(&[("a", true)])
    }

    fn info(file: &str) -> DirtyInfo {
        DirtyInfo { modified: vec![file.to_string()], ..DirtyInfo::default() }
    }

    fn skill(name: &str, verdict: Verdict, result: ApplyResult) -> SkillReport {
        SkillReport { name: name.to_string(), verdict, result }
    }

    fn in_sync(name: &str) -> SkillReport {
        skill(name, Verdict::InSync, ApplyResult::NotApplicable)
    }

    fn deleted_locally(name: &str) -> SkillReport {
        skill(name, Verdict::MissingLocally, ApplyResult::NotApplicable)
    }

    fn report(id: &str, outcome: Outcome, skills: Vec<SkillReport>) -> RepoReport {
        RepoReport {
            repo_id: id.to_string(),
            label: format!("owner/{id}"),
            outcome,
            skills,
            rejected: Vec::new(),
            stats: Stats::default(),
            downloaded: 0,
            finished_at: CHECKED,
            lists_nothing: false,
        }
    }

    /// A check that read a listing with no skills in it at all.
    fn nothing_listed(id: &str, outcome: Outcome, skills: Vec<SkillReport>) -> RepoReport {
        RepoReport { lists_nothing: true, ..report(id, outcome, skills) }
    }

    fn sync_of(repos: Vec<RepoReport>) -> SyncReport {
        SyncReport { repos, notes: Vec::new(), warnings: Vec::new() }
    }

    fn failure(kind: FailureKind) -> Failure {
        Failure { kind, message: "it went wrong".to_string(), retry_at: None }
    }

    fn failed(kind: FailureKind) -> Outcome {
        Outcome::Failed(failure(kind))
    }

    /// A board that has just seen one check of repository `a`.
    fn after(outcome: Outcome, skills: Vec<SkillReport>) -> Board {
        let mut board = Board::new();
        board.note_sync(&sync_of(vec![report("a", outcome, skills)]), NOW);
        board
    }

    /// A board that has just seen one check of repository `a`, whose listing had no skills in it.
    fn after_nothing(outcome: Outcome, skills: Vec<SkillReport>) -> Board {
        let mut board = Board::new();
        board.note_sync(&sync_of(vec![nothing_listed("a", outcome, skills)]), NOW);
        board
    }

    fn verified(found: Vec<(&str, &str, LocalStatus)>) -> VerifyReport {
        VerifyReport {
            skills: found.into_iter().map(|(repo_id, name, status)| LocalReport { repo_id: repo_id.to_string(), name: name.to_string(), status }).collect(),
            orphans: Vec::new(),
            stats: Stats::default(),
            tracking: Tracking { check: Check::Full, volume_changed: false, journal: true, folder_missing: false },
            notes: Vec::new(),
            warnings: Vec::new(),
        }
    }

    fn clean() -> LocalStatus {
        LocalStatus::Checked(LocalState::Clean)
    }

    fn edited(file: &str) -> LocalStatus {
        LocalStatus::Checked(LocalState::Dirty(info(file)))
    }

    fn missing() -> LocalStatus {
        LocalStatus::Checked(LocalState::FolderMissing)
    }

    fn view(board: &Board) -> RepoView {
        board.views(&one(), NOW).remove(0)
    }

    fn row(board: &Board, name: &str) -> Row {
        view(board).skills.into_iter().find(|r| r.name == name).unwrap_or_else(|| panic!("no row for {name}"))
    }

    fn names(board: &Board) -> Vec<String> {
        view(board).skills.into_iter().map(|r| r.name).collect()
    }

    fn sorted(mut list: Vec<String>) -> Vec<String> {
        list.sort();
        list
    }

    #[test]
    fn every_verdict_gets_the_right_text_level_and_buttons() {
        use Action::*;
        let na = ApplyResult::NotApplicable;
        let held = || ApplyResult::Held("busy".to_string());
        let broke = || ApplyResult::Failed("disk full".to_string());
        type Case = (Verdict, ApplyResult, Level, Vec<Action>, &'static str, &'static str);
        let cases: Vec<Case> = vec![
            (Verdict::InSync, na.clone(), Level::Ok, vec![], "In sync", "Up to date"),
            (Verdict::Install, ApplyResult::Done, Level::Ok, vec![], "In sync", "Up to date"),
            (Verdict::Install, ApplyResult::DryRun, Level::Ok, vec![], "will be installed", "New"),
            (Verdict::Install, ApplyResult::NotTried, Level::Ok, vec![], "will be installed", "New"),
            (Verdict::Install, held(), Level::Attention, vec![], "Not changed: busy", "Not changed"),
            (Verdict::Install, broke(), Level::Error, vec![], "Failed: disk full", "Failed"),
            (Verdict::Update, ApplyResult::Done, Level::Ok, vec![], "In sync", "Up to date"),
            (Verdict::Update, ApplyResult::DryRun, Level::Ok, vec![], "Update available", "Update available"),
            (Verdict::Update, ApplyResult::NotTried, Level::Ok, vec![], "Update available", "Update available"),
            (Verdict::Update, held(), Level::Attention, vec![], "Not changed: busy", "Not changed"),
            (Verdict::Update, broke(), Level::Error, vec![], "Failed: disk full", "Failed"),
            (Verdict::Adopt, ApplyResult::Done, Level::Ok, vec![], "In sync", "Up to date"),
            (Verdict::Adopt, ApplyResult::DryRun, Level::Ok, vec![], "will be managed", "Will be managed"),
            (Verdict::Adopt, held(), Level::Attention, vec![], "Not changed", "Not changed"),
            (Verdict::Delete, ApplyResult::DryRun, Level::Ok, vec![], "will be deleted", "Removed on GitHub"),
            (Verdict::Delete, ApplyResult::NotTried, Level::Ok, vec![], "will be deleted", "Removed on GitHub"),
            (Verdict::Delete, held(), Level::Attention, vec![], "Not changed", "Not changed"),
            (Verdict::Delete, broke(), Level::Error, vec![], "Failed", "Failed"),
            (Verdict::Forget, ApplyResult::DryRun, Level::Ok, vec![], "Removed upstream and locally", "Removed on GitHub"),
            (
                Verdict::ModifiedLocally(info("a.md")),
                na.clone(),
                Level::Attention,
                vec![Overwrite, KeepMine],
                "Edited locally (changed: a.md)",
                "Edited on this PC",
            ),
            (Verdict::MissingLocally, na.clone(), Level::Attention, vec![Restore, LeaveRemoved], "Deleted locally", "Deleted on this PC"),
            (Verdict::Held(Hold::UpdateBlocked(info("a.md"))), na.clone(), Level::Attention, vec![Overwrite, KeepMine], "Update held", "Edited on this PC"),
            (Verdict::Held(Hold::UpdateBlockedMissing), na.clone(), Level::Attention, vec![Restore, LeaveRemoved], "Update held", "Deleted on this PC"),
            (Verdict::Held(Hold::DeleteBlocked(info("a.md"))), na.clone(), Level::Attention, vec![KeepMine], "Removal held", "Edited on this PC"),
            (Verdict::Held(Hold::UnmanagedDiffers), na.clone(), Level::Attention, vec![Overwrite, KeepMine], "already exists", "Different copy here"),
            (Verdict::Held(Hold::MassDeletion), na.clone(), Level::Attention, vec![RemoveThem, KeepMine], "no skills at all", "GitHub lists nothing"),
            (Verdict::Held(Hold::Interrupted), na.clone(), Level::Attention, vec![], "interrupted", "Interrupted"),
            (Verdict::Skipped(Skip::Ignored), na.clone(), Level::Ok, vec![ManageAgain], "Kept as yours", "Kept as yours"),
            (Verdict::Skipped(Skip::Collision { owner: "o/b".to_string() }), na.clone(), Level::Attention, vec![], "o/b", "Name clash"),
            (Verdict::Skipped(Skip::Rejected { reason: "too deep".to_string() }), na, Level::Attention, vec![], "too deep", "Not safe"),
        ];
        for (verdict, result, level, actions, text, tag) in cases {
            let board = after(Outcome::Unchanged, vec![skill("s", verdict.clone(), result.clone())]);
            let got = row(&board, "s");
            let what = format!("{verdict:?} / {result:?}");
            assert_eq!(got.level, level, "level for {what}");
            assert_eq!(got.actions, actions, "buttons for {what}");
            assert!(got.text.contains(text), "text for {what} was '{}', which should contain '{text}'", got.text);
            assert_eq!(got.tag, tag, "tag for {what}");
        }
    }

    #[test]
    fn show_changes_is_offered_for_a_copy_here_that_may_differ_and_open_folder_where_one_may_be() {
        let na = || ApplyResult::NotApplicable;
        let cases = [
            (Verdict::InSync, na(), false, true),
            (Verdict::Install, ApplyResult::DryRun, false, false),
            (Verdict::Update, ApplyResult::DryRun, true, true),
            (Verdict::Delete, ApplyResult::DryRun, true, true),
            (Verdict::Install, ApplyResult::Failed("disk full".to_string()), true, true),
            (Verdict::ModifiedLocally(info("a.md")), na(), true, true),
            (Verdict::MissingLocally, na(), false, false),
            (Verdict::Held(Hold::UnmanagedDiffers), na(), true, true),
            (Verdict::Held(Hold::MassDeletion), na(), true, true),
            (Verdict::Skipped(Skip::Ignored), na(), true, true),
            (Verdict::Skipped(Skip::Collision { owner: "o/b".to_string() }), na(), false, true),
            (Verdict::Skipped(Skip::Rejected { reason: "too deep".to_string() }), na(), false, false),
        ];
        for (verdict, result, compare, folder) in cases {
            let board = after(Outcome::Unchanged, vec![skill("s", verdict.clone(), result.clone())]);
            let got = row(&board, "s");
            assert_eq!((got.can_compare(), got.has_folder()), (compare, folder), "{verdict:?} / {result:?} ({})", got.tag);
        }
    }

    #[test]
    fn a_delete_that_was_done_leaves_no_row() {
        for verdict in [Verdict::Delete, Verdict::Forget] {
            let board = after(Outcome::Synced, vec![in_sync("keep"), skill("gone", verdict.clone(), ApplyResult::Done)]);
            assert_eq!(names(&board), ["keep"], "{verdict:?}");
        }
    }

    #[test]
    fn a_rejected_skill_nobody_installed_still_gets_a_row() {
        let rejected_but_owned = skill("dup", Verdict::Skipped(Skip::Rejected { reason: "too deep".to_string() }), ApplyResult::NotApplicable);
        let mut listing = report("a", Outcome::Unchanged, vec![in_sync("good"), rejected_but_owned]);
        listing.rejected = vec![
            Rejected { name: "bad".to_string(), reason: "the name has a dot".to_string() },
            Rejected { name: "dup".to_string(), reason: "the listing's reason".to_string() },
        ];
        let mut board = Board::new();
        board.note_sync(&sync_of(vec![listing]), NOW);

        assert_eq!(names(&board).iter().filter(|n| *n == "dup").count(), 1, "a skill with a decision that is also listed as rejected is one row");
        assert!(row(&board, "dup").text.contains("too deep"), "the decision is the better source: {}", row(&board, "dup").text);
        let bad = row(&board, "bad");
        assert_eq!((bad.level, bad.actions.is_empty()), (Level::Attention, true));
        assert!(bad.text.contains("the name has a dot"), "{}", bad.text);
    }

    #[test]
    fn a_listing_with_only_rejected_skills_still_replaces_the_rows() {
        let mut board = after(Outcome::Unchanged, vec![in_sync("old")]);
        let mut listing = report("a", Outcome::Unchanged, Vec::new());
        listing.rejected = vec![Rejected { name: "bad".to_string(), reason: "x".to_string() }];
        board.note_sync(&sync_of(vec![listing]), NOW);
        assert_eq!(names(&board), ["bad"]);
    }

    #[test]
    fn a_check_that_never_got_a_listing_keeps_the_rows_and_shows_the_failure() {
        let mut board = after(Outcome::Unchanged, vec![in_sync("fine"), skill("later", Verdict::Update, ApplyResult::DryRun)]);
        let before = view(&board).skills;

        let mut broke = report("a", failed(FailureKind::Network), Vec::new());
        broke.finished_at = NOW;
        board.note_sync(&sync_of(vec![broke]), NOW);

        let now = view(&board);
        assert_eq!(now.skills, before, "an error says nothing about the skills");
        assert_eq!((now.level, now.status.as_str(), now.detail.as_deref()), (Level::Error, "No connection", Some("It went wrong")));
        assert_eq!(now.last_check, Some(CHECKED), "a failed check is not a check");
    }

    #[test]
    fn not_modified_keeps_the_rows_and_counts_as_a_check() {
        let mut board = after(Outcome::Unchanged, vec![skill("x", Verdict::ModifiedLocally(info("a.md")), ApplyResult::NotApplicable)]);
        let before = view(&board).skills;

        let mut again = report("a", Outcome::Unchanged, Vec::new());
        again.finished_at = NOW;
        board.note_sync(&sync_of(vec![again]), NOW);

        let now = view(&board);
        assert_eq!(now.skills, before);
        assert_eq!(now.last_check, Some(NOW));
    }

    #[test]
    fn a_new_listing_replaces_the_rows() {
        let mut board = after(Outcome::Unchanged, vec![in_sync("old"), in_sync("both")]);
        board.note_sync(&sync_of(vec![report("a", Outcome::Synced, vec![in_sync("both"), skill("fresh", Verdict::Install, ApplyResult::Done)])]), NOW);
        assert_eq!(names(&board), ["both", "fresh"]);
    }

    #[test]
    fn a_check_that_failed_part_way_shows_how_far_it_got() {
        let board = after(
            failed(FailureKind::Network),
            vec![skill("first", Verdict::Install, ApplyResult::Done), skill("second", Verdict::Install, ApplyResult::NotTried)],
        );
        assert_eq!(row(&board, "first").text, "In sync");
        assert!(row(&board, "second").text.contains("will be installed"));
        assert_eq!((view(&board).status.as_str(), view(&board).level), ("No connection", Level::Error));
    }

    #[test]
    fn a_look_at_the_disk_overrides_what_the_check_saw() {
        let mut board = after(Outcome::Unchanged, vec![in_sync("s")]);
        assert_eq!(row(&board, "s").text, "In sync");

        board.note_verify(&verified(vec![("a", "s", edited("SKILL.md"))]), NOW);
        let got = row(&board, "s");
        assert_eq!((got.level, got.actions.clone()), (Level::Attention, vec![Action::Overwrite, Action::KeepMine]));
        assert!(got.text.contains("Edited locally") && got.text.contains("SKILL.md"), "{}", got.text);

        board.note_verify(&verified(vec![("a", "s", missing())]), NOW);
        assert_eq!(row(&board, "s").actions, [Action::Restore, Action::LeaveRemoved]);

        board.note_verify(&verified(vec![("a", "s", clean())]), NOW);
        let got = row(&board, "s");
        assert_eq!((got.text.as_str(), got.level, got.actions.is_empty()), ("In sync", Level::Ok, true));
    }

    #[test]
    fn a_look_at_the_disk_alone_says_installed_and_nothing_about_upstream() {
        let mut board = Board::new();
        board.note_verify(&verified(vec![("a", "s", clean()), ("a", "t", edited("x.md")), ("a", "u", missing())]), NOW);
        assert_eq!((row(&board, "s").text.as_str(), row(&board, "s").level, row(&board, "s").tag), ("Installed", Level::Ok, "Installed"));
        assert_eq!(row(&board, "t").actions, [Action::Overwrite, Action::KeepMine]);
        assert_eq!(row(&board, "u").actions, [Action::Restore, Action::LeaveRemoved]);
    }

    #[test]
    fn a_look_at_the_disk_drops_skills_that_are_no_longer_ours() {
        let mut board = after(
            Outcome::NeedsAttention,
            vec![
                in_sync("owned"),
                skill("edited", Verdict::ModifiedLocally(info("a.md")), ApplyResult::NotApplicable),
                skill("update", Verdict::Update, ApplyResult::DryRun),
                skill("going", Verdict::Delete, ApplyResult::DryRun),
                skill("empty", Verdict::Held(Hold::MassDeletion), ApplyResult::NotApplicable),
                skill("taken", Verdict::Held(Hold::UnmanagedDiffers), ApplyResult::NotApplicable),
                skill("mine", Verdict::Skipped(Skip::Ignored), ApplyResult::NotApplicable),
                skill("new", Verdict::Install, ApplyResult::NotTried),
                skill("clash", Verdict::Skipped(Skip::Collision { owner: "o/b".to_string() }), ApplyResult::NotApplicable),
            ],
        );
        board.note_verify(&verified(vec![("a", "owned", clean())]), NOW);
        // What only makes sense for an owned skill goes; what was never about ownership stays.
        assert_eq!(sorted(names(&board)), ["clash", "mine", "new", "owned", "taken"]);
    }

    #[test]
    fn names_that_differ_only_in_case_are_one_skill() {
        let mut board = after(Outcome::Unchanged, vec![in_sync("Mixed-Case")]);
        board.note_verify(&verified(vec![("a", "MIXED-CASE", edited("SKILL.md"))]), NOW);
        assert_eq!(names(&board), ["Mixed-Case"]);
        assert_eq!(row(&board, "Mixed-Case").actions, [Action::Overwrite, Action::KeepMine]);
    }

    #[test]
    fn an_interrupted_change_is_shown_whatever_else_is_known() {
        let mut board = after(Outcome::Unchanged, vec![skill("s", Verdict::Update, ApplyResult::DryRun)]);
        board.note_verify(&verified(vec![("a", "s", LocalStatus::Interrupted)]), NOW);
        let got = row(&board, "s");
        assert_eq!((got.level, got.actions.is_empty(), got.tag), (Level::Attention, true, "Interrupted"));
        assert!(got.text.contains("interrupted"), "{}", got.text);

        let from_check = after(Outcome::NeedsAttention, vec![skill("s", Verdict::Held(Hold::Interrupted), ApplyResult::NotApplicable)]);
        assert!(row(&from_check, "s").text.contains("interrupted"));
    }

    #[test]
    fn settling_a_skill_updates_its_row_at_once() {
        let mut board = after(Outcome::NeedsAttention, vec![skill("Mixed-Case", Verdict::ModifiedLocally(info("a")), ApplyResult::NotApplicable)]);

        board.note_resolved("a", "mixed-case", Resolution::Overwrite);
        let got = row(&board, "Mixed-Case");
        assert_eq!((got.text.as_str(), got.level, got.actions.is_empty()), ("In sync", Level::Ok, true));

        board.note_resolved("a", "Mixed-Case", Resolution::KeepMine);
        let got = row(&board, "Mixed-Case");
        assert_eq!((got.text.as_str(), got.level, got.actions.clone()), ("Kept as yours", Level::Ok, vec![Action::ManageAgain]));

        board.note_resolved("a", "Mixed-Case", Resolution::ManageAgain);
        let got = row(&board, "Mixed-Case");
        assert!(got.text.contains("next check") && got.actions.is_empty() && got.level == Level::Ok, "{got:?}");
        assert_eq!(got.tag, "Managed again");
        assert_eq!(names(&board), ["Mixed-Case"], "the skill stays one row, under the name it already had");
    }

    #[test]
    fn health_is_the_worst_thing_that_matters() {
        let attention = || vec![deleted_locally("s")];
        let auth = || failed(FailureKind::Auth);
        let one_on_one_off = cfg(&[("a", false), ("b", true)]);

        assert_eq!(Board::new().health(&Config::default()), Health::NotConfigured, "no repositories");
        assert_eq!(Board::new().health(&cfg(&[("a", false)])), Health::NotConfigured, "only disabled repositories");
        assert_eq!(Board::new().health(&one()), Health::Ok, "never having checked is not a problem");
        assert_eq!(after(Outcome::Synced, vec![in_sync("s")]).health(&one()), Health::Ok);
        assert_eq!(after(Outcome::Cancelled, Vec::new()).health(&one()), Health::Ok, "a cancelled check is not a problem");
        assert_eq!(after(Outcome::NeedsAttention, attention()).health(&one()), Health::Attention);
        assert_eq!(after(auth(), Vec::new()).health(&one()), Health::Error);
        assert_eq!(after(auth(), attention()).health(&one()), Health::Error, "an error outranks attention");
        assert_eq!(after(auth(), attention()).health(&one_on_one_off), Health::Ok, "a disabled repository counts for nothing");

        let mut mixed = Board::new();
        mixed.note_sync(&sync_of(vec![report("a", Outcome::Synced, vec![in_sync("s")]), report("b", Outcome::NeedsAttention, attention())]), NOW);
        assert_eq!(mixed.health(&cfg(&[("a", true), ("b", true)])), Health::Attention, "one repository is enough");

        let mut busy = after(auth(), Vec::new());
        busy.set_working(true);
        assert_eq!(busy.health(&one()), Health::Working, "working shows over everything");
        busy.set_working(false);
        assert_eq!(busy.health(&one()), Health::Error);

        let mut waiting = Board::new();
        waiting.set_waiting(true);
        assert_eq!(waiting.health(&one()), Health::Waiting, "nothing was looked at yet, so it is not up to date");
        assert_eq!(waiting.health(&Config::default()), Health::NotConfigured, "there is nothing to wait for");
        let mut waiting = after(auth(), Vec::new());
        waiting.set_waiting(true);
        assert_eq!(waiting.health(&one()), Health::Error, "waiting never hides a problem");

        let mut recovered = after(auth(), Vec::new());
        recovered.note_sync(&sync_of(vec![report("a", Outcome::Unchanged, vec![in_sync("s")])]), NOW);
        assert_eq!(recovered.health(&one()), Health::Ok, "a later good check clears the failure");

        let mut resolved = after(Outcome::NeedsAttention, attention());
        resolved.note_resolved("a", "s", Resolution::Overwrite);
        assert_eq!(resolved.health(&one()), Health::Ok, "settling the last skill that needs the user ends the warning");
        assert_eq!(view(&resolved).status, "Up to date");
    }

    #[test]
    fn a_problem_with_the_skills_folder_is_an_error_until_the_next_startup_says_otherwise() {
        let mut board = Board::new();
        let mut look = verified(vec![("a", "s", clean())]);
        look.orphans = vec!["old".to_string()];
        board.note_startup(&StartupReport { verify: look, problem: Some("the folders are on different drives".to_string()) }, NOW);

        assert_eq!(board.problem(), Some("the folders are on different drives"));
        assert_eq!(board.orphans(), ["old"]);
        assert_eq!(board.health(&one()), Health::Error);
        assert_eq!(row(&board, "s").text, "Installed", "startup also looks at the disk");

        board.note_startup(&StartupReport { verify: verified(vec![("a", "s", clean())]), problem: None }, NOW);
        assert_eq!((board.problem(), board.orphans().is_empty()), (None, true));
        assert_eq!(board.health(&one()), Health::Ok, "orphans are protected, not a problem");
    }

    #[test]
    fn the_tooltip_says_what_the_state_is() {
        let tip = |board: &Board, config: &Config| board.tooltip(config);
        assert_eq!(tip(&Board::new(), &Config::default()), "Tiny Skill Sync: no repositories are set up");
        assert_eq!(tip(&after(Outcome::Synced, vec![in_sync("s")]), &one()), "Tiny Skill Sync: all skills are up to date");
        assert_eq!(tip(&after(Outcome::NeedsAttention, vec![deleted_locally("s")]), &one()), "Tiny Skill Sync: 1 skill needs attention");
        assert_eq!(tip(&after(failed(FailureKind::Network), Vec::new()), &one()), "Tiny Skill Sync: owner/a: No connection");

        let mut busy = after(failed(FailureKind::Network), Vec::new());
        busy.set_working(true);
        assert_eq!(tip(&busy, &one()), "Tiny Skill Sync: checking for updates…");

        let two = cfg(&[("a", true), ("b", true)]);
        let mut both = Board::new();
        both.note_sync(
            &sync_of(vec![
                report("a", Outcome::NeedsAttention, vec![deleted_locally("s"), deleted_locally("t")]),
                report("b", Outcome::NeedsAttention, vec![deleted_locally("u")]),
            ]),
            NOW,
        );
        assert_eq!(tip(&both, &two), "Tiny Skill Sync: 3 skills need attention", "counted over every repository");

        let mut one_failed = Board::new();
        one_failed.note_sync(
            &sync_of(vec![report("a", failed(FailureKind::Network), Vec::new()), report("b", Outcome::NeedsAttention, vec![deleted_locally("u")])]),
            NOW,
        );
        assert_eq!(tip(&one_failed, &two), "Tiny Skill Sync: owner/a: No connection", "a repository that only needs attention did not fail");

        let mut broken = Board::new();
        broken.note_sync(&sync_of(vec![report("a", failed(FailureKind::Auth), Vec::new()), report("b", failed(FailureKind::Network), Vec::new())]), NOW);
        assert_eq!(tip(&broken, &two), "Tiny Skill Sync: 2 repositories could not be checked");

        broken.note_startup(&StartupReport { verify: verified(vec![]), problem: Some("different drives".to_string()) }, NOW);
        assert_eq!(tip(&broken, &two), "Tiny Skill Sync: different drives", "the problem that stops everything comes first");
    }

    #[test]
    fn the_tooltip_always_fits() {
        let long = RepoConfig { id: "a".to_string(), repo: format!("owner/{}", "n".repeat(300)), ..RepoConfig::default() };
        let config = Config { repos: vec![long], ..Config::default() };
        let tip = after(failed(FailureKind::Network), Vec::new()).tooltip(&config);
        assert_eq!(tip.encode_utf16().count(), TOOLTIP_UNITS, "{tip}");
        assert!(tip.ends_with('…'));
    }

    #[test]
    fn cutting_text_never_splits_a_character_or_goes_over() {
        assert_eq!(cut("short", 127), "short");
        let exact = "x".repeat(127);
        assert_eq!(cut(&exact, 127), exact, "text that fits is left alone");

        let one_over = "x".repeat(128);
        let cut_text = cut(&one_over, 127);
        assert_eq!(cut_text.encode_utf16().count(), 127);
        assert!(cut_text.ends_with('…') && cut_text.starts_with(&"x".repeat(126)));

        // Each of these is two UTF-16 units, so a cut in the wrong place would leave half of one.
        let emoji = "😀".repeat(100);
        let cut_emoji = cut(&emoji, 127);
        assert!(cut_emoji.encode_utf16().count() <= 127);
        assert!(cut_emoji.ends_with('…'));
        assert!(cut_emoji.trim_end_matches('…').chars().all(|c| c == '😀'));
    }

    #[test]
    fn times_read_naturally() {
        let day = 86_400;
        let cases = [
            (0, "just now"),
            (59, "just now"),
            (60, "1 minute ago"),
            (119, "1 minute ago"),
            (120, "2 minutes ago"),
            (3_599, "59 minutes ago"),
            (3_600, "1 hour ago"),
            (day - 1, "23 hours ago"),
            (day, "1 day ago"),
            (13 * day, "13 days ago"),
            (14 * day, "2 weeks ago"),
            (59 * day, "8 weeks ago"),
            (60 * day, "2 months ago"),
            (400 * day, "13 months ago"),
        ];
        for (secs, text) in cases {
            assert_eq!(ago(NOW, NOW - secs), text, "{secs} seconds");
        }
        assert_eq!(ago(NOW, NOW + 500), "just now", "a time in the future (the clock moved)");
        assert_eq!(span(30), "less than a minute");
    }

    #[test]
    fn each_kind_of_failure_gets_its_own_short_status() {
        use FailureKind::*;
        let table = [
            (Auth, "Access refused"),
            (NotFound, "Not found"),
            (RateLimited, "Rate limited"),
            (Server, "GitHub is having problems"),
            (Redirect, "Repository moved"),
            (Protocol, "Unexpected answer"),
            (Network, "No connection"),
            (TooLarge, "Too large"),
            (Cancelled, "Cancelled"),
            (Changed, "Changed while checking"),
            (Local, "Could not update files"),
        ];
        for (kind, status) in table {
            let got = view(&after(failed(kind), Vec::new()));
            assert_eq!((got.level, got.status.as_str(), got.detail.as_deref()), (Level::Error, status, Some("It went wrong")), "{kind:?}");
        }
    }

    #[test]
    fn a_wait_asked_for_by_github_is_explained_while_it_lasts() {
        let limited = |retry_at| Outcome::Failed(Failure { kind: FailureKind::RateLimited, message: "Limit reached.".to_string(), retry_at: Some(retry_at) });
        assert_eq!(view(&after(limited(NOW + 600), Vec::new())).detail.as_deref(), Some("Limit reached. Next try in 10 minutes."));
        assert_eq!(view(&after(limited(NOW - 1), Vec::new())).detail.as_deref(), Some("Limit reached."), "a wait that is over is not mentioned");
        assert_eq!(view(&after(limited(NOW), Vec::new())).detail.as_deref(), Some("Limit reached."), "nor one that ends this very second");
    }

    #[test]
    fn buttons_map_to_what_the_engine_understands() {
        let all = [Action::Overwrite, Action::Restore, Action::KeepMine, Action::LeaveRemoved, Action::ManageAgain, Action::RemoveThem];
        let expected = [
            Some(Resolution::Overwrite),
            Some(Resolution::Overwrite),
            Some(Resolution::KeepMine),
            Some(Resolution::KeepMine),
            Some(Resolution::ManageAgain),
            None,
        ];
        for (action, want) in all.iter().zip(expected) {
            assert_eq!(action.resolution(), want, "{action:?}");
        }
        let labels: HashSet<&str> = all.iter().map(|a| a.label()).collect();
        assert_eq!(labels.len(), all.len(), "every button has its own label");
    }

    #[test]
    fn what_needs_the_user_comes_first() {
        let board = after(
            Outcome::NeedsAttention,
            vec![
                in_sync("a-fine"),
                skill("z-failed", Verdict::Install, ApplyResult::Failed("x".to_string())),
                deleted_locally("b-attention"),
                in_sync("c-fine"),
                deleted_locally("a-attention"),
            ],
        );
        assert_eq!(names(&board), ["z-failed", "a-attention", "b-attention", "a-fine", "c-fine"]);
    }

    #[test]
    fn a_repository_says_in_a_few_words_where_it_stands() {
        let status = |board: &Board| view(board).status;
        assert_eq!(status(&Board::new()), "Not checked yet");
        assert_eq!(view(&Board::new()).last_check_text(NOW), "never");
        assert_eq!(status(&after(Outcome::Unchanged, vec![in_sync("s")])), "Up to date");
        assert_eq!(status(&after(Outcome::Synced, vec![skill("s", Verdict::Install, ApplyResult::Done)])), "Up to date");
        let mixed = after(Outcome::NeedsAttention, vec![deleted_locally("s"), deleted_locally("t"), in_sync("u")]);
        assert_eq!((status(&mixed), view(&mixed).level), ("Needs attention (2)".to_string(), Level::Attention), "a fine skill does not hide the others");
        assert_eq!(status(&after(Outcome::NeedsAttention, vec![deleted_locally("s")])), "Needs attention (1)");
        assert_eq!(status(&after(Outcome::Cancelled, Vec::new())), "Check cancelled");
        assert_eq!(status(&after(Outcome::WouldChange, vec![skill("s", Verdict::Install, ApplyResult::DryRun)])), "Changes pending");

        let mut remembered = Board::new();
        remembered.note_last_check("a", 5);
        remembered.note_verify(&verified(vec![("a", "s", clean())]), NOW);
        assert_eq!(status(&remembered), "Up to date", "checked before this run");

        let off = after(failed(FailureKind::Auth), vec![deleted_locally("s")]).views(&cfg(&[("a", false)]), NOW).remove(0);
        assert_eq!((off.status.as_str(), off.level, off.enabled, off.detail), ("Disabled", Level::Ok, false, None));
    }

    #[test]
    fn a_repository_that_lists_no_skills_does_not_pass_as_up_to_date() {
        // The usual mistake: the skills are in a subfolder, and the whole repository was added.
        let none = after_nothing(Outcome::Unchanged, Vec::new());
        let got = view(&none);
        assert_eq!((got.status.as_str(), got.level), ("No skills found", Level::Attention));
        let detail = got.detail.expect("it says what to do about it");
        assert!(detail.contains("owner/a") && detail.contains("SKILL.md"), "{detail}");
        assert!(detail.contains("https://github.com/owner/a/tree/main/skills"), "{detail}");
        assert!(detail.is_ascii(), "the command line prints it: {detail}");
        assert_eq!(none.health(&one()), Health::Attention);
        assert_eq!(none.tooltip(&one()), "Tiny Skill Sync: owner/a: No skills found");

        // After a restart the record says what the last listing had in it.
        let mut remembered = Board::new();
        remembered.note_last_check("a", 5);
        remembered.note_lists_nothing("a", true);
        assert_eq!(view(&remembered).status, "No skills found");

        // Deleting the last of them, when upstream lists none, ends the same way.
        let emptied = after_nothing(Outcome::Synced, vec![skill("s", Verdict::Delete, ApplyResult::Done)]);
        assert_eq!(view(&emptied).status, "No skills found");
    }

    #[test]
    fn a_listing_whose_skills_were_all_held_or_skipped_is_not_no_skills() {
        // Each skill it listed was held on a folder of the user's, or skipped. The record owns none of them, so a
        // restart has no rows to show, but the listing was not empty.
        let mut restarted = Board::new();
        restarted.note_last_check("a", 5);
        restarted.note_lists_nothing("a", false);
        assert_ne!(view(&restarted).status, "No skills found");
        // A check that GitHub answers with "not modified" says nothing new about the listing, either way.
        restarted.note_sync(&sync_of(vec![report("a", Outcome::Unchanged, Vec::new())]), NOW);
        assert_ne!(view(&restarted).status, "No skills found");
        let mut empty = Board::new();
        empty.note_lists_nothing("a", true);
        empty.note_sync(&sync_of(vec![report("a", Outcome::Unchanged, Vec::new())]), NOW);
        assert_eq!(view(&empty).status, "No skills found");

        // A listing with nothing in it replaces what an earlier one showed, and a listing with skills undoes it.
        let mut refused = report("a", Outcome::NeedsAttention, Vec::new());
        refused.rejected = vec![Rejected { name: "bad".to_string(), reason: "too long".to_string() }];
        let mut board = Board::new();
        board.note_sync(&sync_of(vec![refused]), NOW);
        board.note_sync(&sync_of(vec![nothing_listed("a", Outcome::Unchanged, Vec::new())]), NOW);
        assert_eq!(view(&board).status, "No skills found");
        board.note_sync(&sync_of(vec![report("a", Outcome::Unchanged, vec![in_sync("s")])]), NOW);
        board.note_verify(&verified(Vec::new()), NOW);
        assert!(view(&board).skills.is_empty());
        assert_ne!(view(&board).status, "No skills found");
    }

    #[test]
    fn a_repository_is_private_when_it_is_marked_so_or_found_to_be() {
        let mut board = Board::new();
        assert!(!view(&board).private);
        board.note_private("a", true);
        assert!(view(&board).private, "found to need the token");
        board.note_private("a", false);
        assert!(!view(&board).private, "read without the token again");

        let marked = Config { repos: vec![RepoConfig { use_token: true, ..repo("a", true) }], ..Config::default() };
        assert!(Board::new().views(&marked, NOW)[0].private, "marked to send the token");

        let mut untouched = Board::new();
        untouched.note_private("a", false);
        assert!(untouched.repos.is_empty(), "a public repository adds nothing to the board");
    }

    #[test]
    fn what_to_do_about_no_skills_depends_on_where_the_repository_was_looked_at() {
        let said = |branch: &str, path: &str| {
            let config = Config { repos: vec![RepoConfig { branch: branch.to_string(), path: path.to_string(), ..repo("a", true) }], ..Config::default() };
            after_nothing(Outcome::Unchanged, Vec::new()).views(&config, NOW).remove(0).detail.expect("a detail")
        };
        assert!(said("", "").contains("https://github.com/owner/a/tree/main/skills"));
        assert!(said("dev", "").contains("https://github.com/owner/a/tree/dev/skills"));
        let in_folder = said("", "docs/skills");
        assert!(in_folder.contains("'docs/skills'") && in_folder.contains("owner/a") && !in_folder.contains("tree/"), "{in_folder}");
        assert!(in_folder.is_ascii());
    }

    #[test]
    fn only_a_real_answer_with_nothing_in_it_is_no_skills() {
        let remembered = || {
            let mut board = Board::new();
            board.note_last_check("a", 5);
            board.note_lists_nothing("a", true);
            board
        };
        let status_after = |outcome: Outcome| {
            let mut board = remembered();
            board.note_sync(&sync_of(vec![report("a", outcome, Vec::new())]), NOW);
            view(&board).status
        };
        assert_eq!(status_after(failed(FailureKind::Network)), "No connection", "a failed check says what failed");
        assert_eq!(status_after(Outcome::Cancelled), "Check cancelled");
        assert_eq!(status_after(Outcome::WouldChange), "Changes pending");
        assert_eq!(view(&Board::new()).status, "Not checked yet", "nothing was asked yet");

        let off = after_nothing(Outcome::Unchanged, Vec::new()).views(&cfg(&[("a", false)]), NOW).remove(0);
        assert_eq!((off.status.as_str(), off.level), ("Disabled", Level::Ok), "a disabled repository is the user's choice");

        // Anything shown as a row is something found, even when it is only kept as the user's own or not safe.
        let mut kept = remembered();
        kept.note_kept("a", &["Mine".to_string()]);
        assert_eq!(view(&kept).status, "Up to date");
        let mut unsafe_only = report("a", Outcome::NeedsAttention, Vec::new());
        unsafe_only.rejected = vec![Rejected { name: "bad".to_string(), reason: "too long".to_string() }];
        let mut board = remembered();
        board.note_sync(&sync_of(vec![unsafe_only]), NOW);
        assert_eq!(view(&board).status, "Needs attention (1)");
    }

    #[test]
    fn the_tooltip_names_a_repository_that_lists_no_skills() {
        let two = cfg(&[("a", true), ("b", true)]);
        let tip = |reports: Vec<RepoReport>| {
            let mut board = Board::new();
            board.note_sync(&sync_of(reports), NOW);
            board.tooltip(&two)
        };
        let empty = |id: &str| nothing_listed(id, Outcome::Unchanged, Vec::new());
        assert_eq!(tip(vec![empty("a"), empty("b")]), "Tiny Skill Sync: 2 repositories have no skills");
        assert_eq!(tip(vec![empty("a"), report("b", Outcome::Synced, vec![in_sync("s")])]), "Tiny Skill Sync: owner/a: No skills found");
        let needs_the_user = report("b", Outcome::NeedsAttention, vec![deleted_locally("s")]);
        assert_eq!(tip(vec![empty("a"), needs_the_user]), "Tiny Skill Sync: 1 skill needs attention", "what the user can settle comes first");
    }

    #[test]
    fn the_last_check_follows_real_answers_only() {
        let cases = [
            (Outcome::Unchanged, true),
            (Outcome::Synced, true),
            (Outcome::NeedsAttention, true),
            (Outcome::WouldChange, false),
            (Outcome::Cancelled, false),
            (failed(FailureKind::Network), false),
        ];
        for (outcome, counts) in cases {
            let board = after(outcome.clone(), Vec::new());
            assert_eq!(view(&board).last_check, counts.then_some(CHECKED), "{outcome:?}");
        }

        let mut board = Board::new();
        board.note_last_check("a", 42);
        assert_eq!(view(&board).last_check, Some(42));
        assert_eq!(view(&board).last_check_text(100_000), "1 day ago");
    }

    #[test]
    fn notes_and_warnings_are_kept_briefly_and_never_repeated() {
        let mut board = Board::new();
        let mut news = sync_of(Vec::new());
        news.notes = vec![Note { repo: "a".to_string(), skill: "s".to_string(), text: "finished the interrupted update".to_string() }];
        news.warnings = vec!["could not save".to_string()];
        board.note_sync(&news, 100);
        board.note_sync(&news, 200);

        let got: Vec<&Message> = board.messages().collect();
        assert_eq!(got.len(), 2, "the same news twice is one message");
        assert_eq!((got[0].level, got[0].text.as_str(), got[0].at), (Level::Ok, "s: finished the interrupted update", 200));
        assert_eq!((got[1].level, got[1].text.as_str(), got[1].at), (Level::Attention, "could not save", 200));

        let mut flood = sync_of(Vec::new());
        flood.warnings = (0..40).map(|i| format!("problem {i}")).collect();
        board.note_sync(&flood, 300);
        let texts: Vec<&str> = board.messages().map(|m| m.text.as_str()).collect();
        assert_eq!(texts.len(), MAX_MESSAGES);
        assert_eq!(texts.last(), Some(&"problem 39"), "the newest is kept");
        assert!(!texts.contains(&"problem 0"), "the oldest goes first");
    }

    #[test]
    fn what_a_check_changed_on_this_pc_is_told_in_the_activity() {
        let changed = vec![
            skill("new", Verdict::Install, ApplyResult::Done),
            skill("old", Verdict::Update, ApplyResult::Done),
            skill("gone", Verdict::Delete, ApplyResult::Done),
            skill("same", Verdict::Adopt, ApplyResult::Done),
            skill("historical", Verdict::AdoptHistorical, ApplyResult::Done),
            skill("history-held", Verdict::AdoptHistorical, ApplyResult::Held("edited".to_string())),
            skill("history-trial", Verdict::AdoptHistorical, ApplyResult::DryRun),
            skill("record", Verdict::Forget, ApplyResult::Done),
            skill("held", Verdict::Update, ApplyResult::Held("edited".to_string())),
            skill("trial", Verdict::Install, ApplyResult::DryRun),
            skill("broken", Verdict::Install, ApplyResult::Failed("disk full".to_string())),
            in_sync("quiet"),
        ];
        let mut board = Board::new();
        board.note_sync(&sync_of(vec![RepoReport { label: "owner/a@main : skills".to_string(), ..report("a", Outcome::Synced, changed) }]), NOW);
        let got: Vec<(Level, &str)> = board.messages().map(|m| (m.level, m.text.as_str())).collect();
        assert_eq!(
            got,
            [
                (Level::Ok, "Installed new from owner/a"),
                (Level::Ok, "Updated old from owner/a"),
                (Level::Ok, "Deleted gone: owner/a no longer has it"),
                (Level::Ok, "same on this PC matched owner/a, so it is now kept in sync"),
                (Level::Ok, "Verified and adopted an older GitHub copy of historical, then updated it from owner/a"),
            ]
        );
        assert_eq!(row(&board, "historical").tag, "Older copy updated");
        assert_ne!(row(&board, "history-held").tag, "Older copy updated");
        assert_ne!(row(&board, "history-trial").tag, "Older copy updated");

        // Many at once are counted, so they never push everything else out of the log.
        let done = |names: &[&str], verdict: Verdict| names.iter().map(|n| skill(n, verdict.clone(), ApplyResult::Done)).collect::<Vec<_>>();
        let mut many = done(&["a", "b", "c", "d"], Verdict::Install);
        many.extend(done(&["e", "f"], Verdict::Delete));
        many.extend(done(&["g", "h", "i"], Verdict::Adopt));
        let mut board = Board::new();
        board.note_sync(&sync_of(vec![report("b", Outcome::Synced, many)]), NOW);
        let got: Vec<&str> = board.messages().map(|m| m.text.as_str()).collect();
        assert_eq!(
            got,
            [
                "Installed 4 skills from owner/b",
                "Deleted e and f: owner/b no longer has them",
                "g, h and i on this PC matched owner/b, so they are now kept in sync"
            ]
        );
    }

    #[test]
    fn a_look_at_the_disk_brings_its_news_too() {
        let mut board = Board::new();
        let mut look = verified(vec![]);
        look.notes = vec![Note { repo: "a".to_string(), skill: "s".to_string(), text: "your version was kept".to_string() }];
        look.warnings = vec!["could not save".to_string()];
        board.note_verify(&look, NOW);
        let levels: Vec<Level> = board.messages().map(|m| m.level).collect();
        assert_eq!(levels, [Level::Ok, Level::Attention]);
    }

    #[test]
    fn a_removed_repository_is_forgotten() {
        let mut board = after(failed(FailureKind::Auth), vec![deleted_locally("s")]);
        board.forget_repo("a");
        let got = view(&board);
        assert_eq!((got.skills.is_empty(), got.status.as_str(), got.level), (true, "Not checked yet", Level::Ok));
    }

    #[test]
    fn skills_kept_as_the_users_own_are_shown_from_the_record() {
        let mut board = Board::new();
        board.note_verify(&verified(vec![("a", "owned", clean())]), NOW);
        board.note_kept("a", &["Mine".to_string()]);
        let mine = row(&board, "Mine");
        assert_eq!((mine.text.as_str(), mine.level, mine.actions.as_slice()), ("Kept as yours", Level::Ok, &[Action::ManageAgain][..]));
        assert_eq!(row(&board, "owned").text, "Installed");

        board.note_verify(&verified(vec![("a", "owned", clean())]), NOW);
        assert_eq!(row(&board, "Mine").text, "Kept as yours", "a look at the disk does not know about it, and keeps it");

        board.note_kept("a", &["MINE".to_string()]);
        assert_eq!(names(&board), ["Mine", "owned"], "the same name in another case is the same skill");

        board.note_kept("a", &[]);
        assert_eq!(names(&board), ["owned"], "kept no more (managed again elsewhere)");

        board.note_kept("other", &[]);
        assert!(!board.repos.contains_key("other"), "nothing kept, nothing to remember");
    }

    #[test]
    fn an_engine_that_cannot_work_is_an_error_until_it_can() {
        let mut board = after(Outcome::Synced, vec![in_sync("s")]);
        board.note_failure(100, Some("cannot read state.json"));
        board.note_failure(200, Some("cannot read state.json"));
        assert_eq!(board.health(&one()), Health::Error);
        assert_eq!(board.tooltip(&one()), "Tiny Skill Sync: cannot read state.json");
        assert_eq!(board.failure(), Some("cannot read state.json"));
        let told: Vec<(UnixSecs, Level)> = board.messages().map(|m| (m.at, m.level)).collect();
        assert_eq!(told, [(100, Level::Error)], "told once, when it started");

        board.note_failure(300, None);
        assert_eq!((board.health(&one()), board.failure()), (Health::Ok, None));
    }

    #[test]
    fn views_follow_the_settings_not_the_history() {
        let mut board = Board::new();
        board.note_sync(
            &sync_of(vec![report("gone", Outcome::NeedsAttention, vec![deleted_locally("s")]), report("b", Outcome::Unchanged, vec![in_sync("t")])]),
            NOW,
        );
        let views = board.views(&cfg(&[("b", true), ("new", true)]), NOW);
        let ids: Vec<&str> = views.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(ids, ["b", "new"], "in the order of the settings; a repository that is not in them is not shown");
        assert_eq!(views[0].label, "owner/b");
        assert_eq!(board.health(&cfg(&[("b", true)])), Health::Ok, "nor does it count");
    }
}
