//! The engine: decides what to check and when, asks GitHub through a `Remote`, and changes the skills folder only
//! through `apply`. It owns no window, timer or thread: the tray and the command line drive it.
//!
//! How a call works:
//! 1. Take the lock (`engine.lock`), so the tray and `tss` never work on the skills at the same time.
//! 2. Load `state.json`, unless the copy in memory is still exactly the file on disk (one cheap look at the file
//!    tells). A poll that finds nothing new therefore reads no file and writes none.
//! 3. Do the work, always going through `reconcile` for decisions and `apply` for changes.
//! 4. Save only what changed, then let go of the lock.
//!
//! Two rules matter most:
//! * Anything that goes wrong with the network, the token or GitHub ends the check for that repository before any
//!   decision is made. A failure is never mistaken for "the skill was removed upstream".
//! * Verification before a destructive step always reads the files (`Check::Full`); the cheap fingerprint check is
//!   only used to report status.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::apply::{self, Applied, Applier, Mode, Note, Step};
use crate::config::{Config, Paths, RepoConfig};
use crate::fsx::exists_no_follow;
use crate::github::{FailureKind, Remote, RemoteError, RepoRef, TokenProvider, TreeReply};
use crate::interval::{UnixSecs, now_secs};
use crate::pathsafe::fold;
use crate::reconcile::{self, Decision, DirtyInfo, Input, LocalState, OwnedView, Skip, Verdict};
use crate::scheduler::Schedule;
use crate::secret::Secret;
use crate::state::{Fingerprint, OwnedSkill, Pending, State, VolumeId};
use crate::tree::{self, Discovery, Rejected, SkillFile};
use crate::verify::{self, Check, Stats, check_for_volume};
use crate::win::fs::{Kind, probe, volume_id};

const LOCK_FILE: &str = "engine.lock";
const DEFAULT_LOCK_WAIT: Duration = Duration::from_secs(3);
/// The most one check downloads. A skill that would go past it waits for the next check.
pub const DEFAULT_MAX_DOWNLOAD: u64 = 512 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// Looks only. Writes nothing, not even a note that a file was touched.
    ReadOnly,
    Write,
}

/// Which repositories a check covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Which {
    /// The ones whose time has come.
    Due,
    /// Every enabled one, whether it is due or not ("check now").
    All,
    /// One repository, whether it is due or not, enabled or not.
    One(String),
}

#[derive(Clone, Debug)]
pub struct SyncOptions {
    /// Look, but change nothing: no files, no state, no schedule.
    pub dry_run: bool,
    /// Ask GitHub for the whole listing even if the stored ETag says nothing changed.
    pub ignore_etag: bool,
    /// The user has confirmed that removing every skill of a repository whose listing is empty is intended.
    pub confirm_mass_delete: bool,
    pub max_download_bytes: u64,
}

impl Default for SyncOptions {
    fn default() -> Self {
        SyncOptions { dry_run: false, ignore_etag: false, confirm_mass_delete: false, max_download_bytes: DEFAULT_MAX_DOWNLOAD }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// Take upstream's version. Whatever is in the folder now is kept as a backup. Also restores a deleted folder.
    Overwrite,
    /// The folder is the user's: stop managing it and never touch it.
    KeepMine,
    /// Undo `KeepMine`: the next check handles the skill like any other.
    ManageAgain,
}

#[derive(Debug)]
pub enum EngineError {
    /// Another check (the tray, or `tss`) is working right now.
    Busy,
    Failed(String),
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineError::Busy => f.write_str("another check is running right now; try again in a moment"),
            EngineError::Failed(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for EngineError {}

fn failed(why: impl Into<String>) -> EngineError {
    EngineError::Failed(why.into())
}

#[derive(Clone, Debug)]
pub struct Failure {
    pub kind: FailureKind,
    pub message: String,
    /// GitHub asked us not to come back before this time.
    pub retry_at: Option<UnixSecs>,
}

impl From<RemoteError> for Failure {
    fn from(e: RemoteError) -> Self {
        Failure { kind: e.kind, message: e.message, retry_at: e.retry_at }
    }
}

#[derive(Clone, Debug)]
pub enum Outcome {
    /// Nothing to do: upstream has not changed, or everything already matches.
    Unchanged,
    /// Changes were made and everything is settled.
    Synced,
    /// Settled except for things that need the user (see the skills).
    NeedsAttention,
    /// A dry run found changes it did not make.
    WouldChange,
    /// The check could not be completed. Nothing was deleted because of it.
    Failed(Failure),
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplyResult {
    /// The verdict is not an action (or nothing needed doing).
    NotApplicable,
    Done,
    DryRun,
    /// Left alone, for this reason.
    Held(String),
    Failed(String),
    /// An earlier problem stopped the check before this was reached.
    NotTried,
}

#[derive(Clone, Debug)]
pub struct SkillReport {
    pub name: String,
    pub verdict: Verdict,
    pub result: ApplyResult,
}

#[derive(Clone, Debug)]
pub struct RepoReport {
    pub repo_id: String,
    pub label: String,
    pub outcome: Outcome,
    pub skills: Vec<SkillReport>,
    /// Upstream skills that cannot be installed (and the reason).
    pub rejected: Vec<Rejected>,
    pub stats: Stats,
    pub downloaded: u64,
    pub finished_at: UnixSecs,
}

impl RepoReport {
    fn new(repo: &RepoConfig) -> RepoReport {
        RepoReport {
            repo_id: repo.id.clone(),
            label: repo.label(),
            outcome: Outcome::Unchanged,
            skills: Vec::new(),
            rejected: Vec::new(),
            stats: Stats::default(),
            downloaded: 0,
            finished_at: 0,
        }
    }

    fn fail(mut self, failure: impl Into<Failure>) -> RepoReport {
        self.outcome = Outcome::Failed(failure.into());
        self
    }
}

#[derive(Debug)]
pub struct SyncReport {
    pub repos: Vec<RepoReport>,
    /// What recovery did about changes a crash had interrupted.
    pub notes: Vec<Note>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalStatus {
    Checked(LocalState),
    /// An earlier change was interrupted and is not sorted out yet.
    Interrupted,
}

#[derive(Clone, Debug)]
pub struct LocalReport {
    pub repo_id: String,
    pub name: String,
    pub status: LocalStatus,
}

/// How local edits are being detected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tracking {
    /// `Quick` means stored Windows fingerprints vouched for unchanged files (no file was read); `Full` means every
    /// file was read and hashed.
    pub check: Check,
    /// The drive or its change journal is not the one the fingerprints came from, so nothing stored was trusted.
    pub volume_changed: bool,
    /// The skills drive has an NTFS change journal.
    pub journal: bool,
}

#[derive(Debug)]
pub struct VerifyReport {
    pub skills: Vec<LocalReport>,
    /// Repositories that have installed skills on record but are no longer in the settings. Their skills stay protected.
    pub orphans: Vec<String>,
    pub stats: Stats,
    pub tracking: Tracking,
    pub notes: Vec<Note>,
    pub warnings: Vec<String>,
}

#[derive(Debug)]
pub struct StartupReport {
    pub verify: VerifyReport,
    /// Set when the skills and work folders cannot be used together (for example they are on different drives).
    pub problem: Option<String>,
}

#[derive(Debug)]
pub struct ResolveReport {
    pub message: String,
    /// Folders that now hold what was replaced.
    pub backups: Vec<PathBuf>,
}

#[derive(Debug, Default)]
pub struct RemoveReport {
    pub deleted: Vec<String>,
    /// Skills that stay installed, with the reason, and are now the user's.
    pub kept: Vec<(String, String)>,
}

type StepHook = Box<dyn FnMut(Step) -> Result<(), RemoteError> + Send>;

/// Tells which drive a folder is on and whether that drive keeps a change journal.
type VolumeProbe = fn(&Path) -> io::Result<VolumeId>;

pub struct Engine {
    paths: Paths,
    remote: Box<dyn Remote>,
    tokens: Box<dyn TokenProvider>,
    clock: Box<dyn Fn() -> UnixSecs + Send + Sync>,
    lock_wait: Duration,
    cache: Cache,
    /// `last_check` values that were not worth a write on their own. Saved with the next real save, or by `flush`.
    lazy: HashMap<String, UnixSecs>,
    schedule: Schedule,
    step_hook: Option<StepHook>,
    volume_probe: VolumeProbe,
}

/// The state in memory, and the stamp of the file it was read from or last written to.
#[derive(Default)]
struct Cache {
    state: State,
    /// `None` means the copy cannot be trusted and the file is read again.
    stamp: Option<Stamp>,
}

/// What Windows reports about `state.json`: if it is the same, the file is the same. `Stamp(None)` is a missing file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp(Option<Fingerprint>);

/// The stamp of a file as it is now, or `None` when it cannot be looked at. A metadata query: no content is read.
pub fn stamp_of(path: &Path) -> Option<Stamp> {
    match probe(path) {
        Ok(None) => Some(Stamp(None)),
        Ok(Some(p)) if p.kind == Kind::File => Some(Stamp(Some(p.fp))),
        _ => None,
    }
}

#[derive(Clone, Copy)]
struct Volume {
    current: Option<VolumeId>,
    check: Check,
    changed: bool,
}

/// Everything one call works on. Created by `begin`, returned by `end`.
struct Session {
    _lock: Option<File>,
    access: Access,
    state: State,
    applier: Applier,
    vol: Option<Volume>,
    volume_probe: VolumeProbe,
    token: Option<Result<Arc<Secret>, String>>,
    preflight: Option<Result<(), String>>,
    downloaded: u64,
    stats: Stats,
    /// The state in memory has changes that matter and are not on disk yet.
    must_save: bool,
    /// The state in memory may differ from the file (a failed write, or a read-only change), so it must be read again.
    suspect: bool,
    lazy: HashMap<String, UnixSecs>,
    notes: Vec<Note>,
    warnings: Vec<String>,
}

impl Session {
    /// Writes the state. A failure is remembered as a warning and the state is read from disk again next time.
    fn save(&mut self) -> Result<(), String> {
        if self.access == Access::ReadOnly {
            return Ok(());
        }
        match self.state.save(&self.applier.state_path) {
            Ok(()) => {
                self.must_save = false;
                self.lazy.clear();
                Ok(())
            }
            Err(e) => {
                self.suspect = true;
                let why = format!("could not save the list of managed skills: {e}");
                self.warnings.push(why.clone());
                Err(why)
            }
        }
    }

    /// Which kind of check the stored fingerprints allow. The first call also deals with a changed drive: nothing
    /// stored for the old one can be trusted, so every fingerprint is dropped and every file is read.
    fn volume(&mut self) -> Volume {
        if let Some(v) = self.vol {
            return v;
        }
        let Ok(current) = (self.volume_probe)(&self.applier.skills_dir) else {
            return Volume { current: None, check: Check::Full, changed: false };
        };
        let changed = self.state.volume != Some(current);
        let check = check_for_volume(self.state.volume.as_ref(), &current);
        if changed {
            clear_fingerprints(&mut self.state);
            self.state.volume = Some(current);
            self.must_save = true;
        }
        let v = Volume { current: Some(current), check, changed };
        self.vol = Some(v);
        v
    }

    fn tracking(&mut self) -> Tracking {
        let v = self.volume();
        Tracking { check: v.check, volume_changed: v.changed, journal: v.current.is_some_and(|c| c.journal != 0) }
    }

    /// Remembers a successful check that found nothing new. Not worth a write by itself.
    fn touch_check(&mut self, repo_id: &str, at: UnixSecs) {
        if let Some(r) = self.state.repos.get_mut(repo_id) {
            r.last_check = Some(at);
            self.lazy.insert(repo_id.to_string(), at);
        }
    }

    fn token_for(&mut self, tokens: &dyn TokenProvider, repo: &RepoConfig) -> Result<Option<Arc<Secret>>, Failure> {
        if !repo.use_token {
            return Ok(None);
        }
        let resolved = self.token.get_or_insert_with(|| tokens.token().map(Arc::new));
        match resolved {
            Ok(t) => Ok(Some(Arc::clone(t))),
            Err(why) => Err(Failure { kind: FailureKind::Auth, message: why.clone(), retry_at: None }),
        }
    }

    fn ensure_preflight(&mut self) -> Result<(), RemoteError> {
        let result = self.preflight.get_or_insert_with(|| apply::preflight(&self.applier.skills_dir, &self.applier.work_dir)).clone();
        if result.is_ok() && self.vol.is_none() {
            // The skills folder may only now exist. Its drive is recorded before anything is written into it, so the
            // fingerprints of what this session installs can be trusted later.
            self.volume();
        }
        result.map_err(|why| RemoteError::new(FailureKind::Local, why))
    }
}

fn clear_fingerprints(state: &mut State) {
    for repo in state.repos.values_mut() {
        for skill in &mut repo.skills {
            skill.files.iter_mut().for_each(|f| f.fp = None);
            if let Some(Pending::Update { files, .. }) = &mut skill.pending {
                files.iter_mut().for_each(|f| f.fp = None);
            }
        }
    }
}

fn is_busy(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(32) | Some(33))
}

/// Opens the lock file so that nobody else can have it open at the same time. Read-only callers do not create
/// it: if it was never created, nobody has ever written, so there is nothing to wait for.
fn open_lock(path: &Path, access: Access) -> io::Result<Option<File>> {
    loop {
        match OpenOptions::new().read(true).share_mode(0).open(path) {
            Ok(f) => return Ok(Some(f)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if access == Access::ReadOnly {
                    return Ok(None);
                }
                if let Some(dir) = path.parent() {
                    fs::create_dir_all(dir)?;
                }
                match OpenOptions::new().write(true).create_new(true).share_mode(0).open(path) {
                    Ok(f) => return Ok(Some(f)),
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(e) => return Err(e),
                }
            }
            Err(e) => return Err(e),
        }
    }
}

fn since(now: Stats, before: Stats) -> Stats {
    Stats { probed: now.probed - before.probed, hashed: now.hashed - before.hashed, hashed_bytes: now.hashed_bytes - before.hashed_bytes }
}

fn root_len(applier: &Applier) -> usize {
    let skills = applier.skills_dir.to_string_lossy().chars().count();
    let work = applier.work_dir.to_string_lossy().chars().count() + 30;
    skills.max(work)
}

/// What one repository's check needs besides the session.
struct Ctx<'a> {
    repo: &'a RepoConfig,
    repo_ref: RepoRef<'a>,
    token: Option<Arc<Secret>>,
    /// The configured folder is itself one skill, so its files sit directly in it.
    single: bool,
    opts: &'a SyncOptions,
    cancel: &'a AtomicBool,
}

struct Run {
    results: Vec<ApplyResult>,
    /// A problem that makes the rest of this repository's downloads pointless.
    stopped: Option<Failure>,
    cancelled: bool,
    local_failures: Vec<String>,
}

impl Engine {
    /// Does no I/O. `paths.skills` is the folder skills are installed in: call `set_skills_dir` when the setting changes.
    pub fn new(paths: Paths, remote: Box<dyn Remote>, tokens: Box<dyn TokenProvider>) -> Engine {
        Engine {
            paths,
            remote,
            tokens,
            clock: Box::new(now_secs),
            lock_wait: DEFAULT_LOCK_WAIT,
            cache: Cache::default(),
            lazy: HashMap::new(),
            schedule: Schedule::new(),
            step_hook: None,
            volume_probe: volume_id,
        }
    }

    pub fn set_skills_dir(&mut self, dir: PathBuf) {
        self.paths.skills = dir;
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    pub fn schedule(&self) -> &Schedule {
        &self.schedule
    }

    /// When the app next needs to wake up, or `None` when nothing is configured. Uses what the last call saw.
    pub fn next_wake(&self, cfg: &Config) -> Option<UnixSecs> {
        self.schedule.next_wake(cfg, &self.cache.state, self.now())
    }

    pub fn last_check(&self, repo_id: &str) -> Option<UnixSecs> {
        self.cache.state.repo(repo_id).and_then(|r| r.last_check)
    }

    /// The skills of a repository the user keeps as their own, as of the last call.
    pub fn kept(&self, repo_id: &str) -> Vec<String> {
        self.cache.state.repo(repo_id).map(|r| r.ignored.clone()).unwrap_or_default()
    }

    /// The stamp of `state.json` as the last call left it, or `None` when it is not known. A different stamp on disk
    /// means another program (such as `tss`) changed the file since.
    pub fn state_stamp(&self) -> Option<Stamp> {
        self.cache.stamp
    }

    #[doc(hidden)]
    pub fn set_clock(&mut self, clock: impl Fn() -> UnixSecs + Send + Sync + 'static) {
        self.clock = Box::new(clock);
    }

    #[doc(hidden)]
    pub fn set_lock_wait(&mut self, wait: Duration) {
        self.lock_wait = wait;
    }

    /// Tests only: runs at every step of a change, and an error from it models a crash at that point.
    #[doc(hidden)]
    pub fn set_step_hook(&mut self, hook: impl FnMut(Step) -> Result<(), RemoteError> + Send + 'static) {
        self.step_hook = Some(Box::new(hook));
    }

    /// Tests only: answers the question "which drive is this, and does it keep a change journal?" in place of Windows,
    /// to stand in for drives that cannot be had on a test machine (FAT, a network share, a journal that was deleted).
    #[doc(hidden)]
    pub fn set_volume_probe(&mut self, probe: fn(&Path) -> io::Result<VolumeId>) {
        self.volume_probe = probe;
    }

    fn now(&self) -> UnixSecs {
        (self.clock)()
    }

    fn acquire(&self, access: Access) -> Result<Option<File>, EngineError> {
        let path = self.paths.home.join(LOCK_FILE);
        let deadline = Instant::now() + self.lock_wait;
        loop {
            match open_lock(&path, access) {
                Ok(lock) => return Ok(lock),
                Err(e) if is_busy(&e) => {
                    if Instant::now() >= deadline {
                        return Err(EngineError::Busy);
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(e) => return Err(failed(format!("cannot use the lock file {}: {e}", path.display()))),
            }
        }
    }

    fn begin(&mut self, access: Access) -> Result<Session, EngineError> {
        let lock = self.acquire(access)?;
        let path = self.paths.state_file();
        let mut warnings = Vec::new();
        let mut suspect = false;
        let mut must_save = false;

        let stamp = stamp_of(&path);
        let reusable = self.cache.stamp.is_some() && self.cache.stamp == stamp;
        let mut state = if reusable {
            std::mem::take(&mut self.cache.state)
        } else {
            let loaded = State::load_with(&path, access == Access::Write).map_err(|e| failed(format!("cannot read {}: {e}", path.display())))?;
            let mut state = loaded.state;
            suspect = loaded.warning.is_some() && access == Access::ReadOnly;
            warnings.extend(loaded.warning);
            let dropped = state.sanitize();
            must_save = !dropped.is_empty();
            warnings.extend(dropped);
            state
        };
        // From here on the state lives in the session; `end` puts it back.
        self.cache.stamp = None;

        for (id, at) in &self.lazy {
            if let Some(r) = state.repos.get_mut(id) {
                if r.last_check.is_none_or(|c| c < *at) {
                    r.last_check = Some(*at);
                }
            }
        }

        let mut applier = Applier::new(self.paths.skills.clone(), self.paths.work_dir(), path);
        applier.hook = self.step_hook.take();
        let mut s = Session {
            _lock: lock,
            access,
            state,
            applier,
            vol: None,
            volume_probe: self.volume_probe,
            token: None,
            preflight: None,
            downloaded: 0,
            stats: Stats::default(),
            must_save,
            suspect,
            lazy: std::mem::take(&mut self.lazy),
            notes: Vec::new(),
            warnings,
        };
        if access == Access::Write && s.state.has_pending() {
            match s.applier.recover(&mut s.state) {
                Ok(notes) => s.notes = notes,
                Err(e) => {
                    s.suspect = true;
                    s.warnings.push(format!("could not sort out interrupted changes: {e}"));
                }
            }
        }
        Ok(s)
    }

    /// Saves what must be saved, hands the state back to the cache and lets go of the lock. Returns the warnings.
    fn finish(&mut self, mut s: Session) -> Vec<String> {
        if s.must_save {
            if s.access == Access::Write {
                let _ = s.save();
            } else {
                s.suspect = true;
            }
        }
        self.step_hook = s.applier.hook.take();
        self.lazy = std::mem::take(&mut s.lazy);
        let stamp = if s.suspect { None } else { stamp_of(&self.paths.state_file()) };
        self.cache = Cache { state: s.state, stamp };
        s.warnings
    }

    /// Ends a call that carries out something the user asked for. If that cannot be saved the call fails, instead of
    /// pretending it worked.
    fn conclude<T>(&mut self, mut s: Session, result: Result<T, EngineError>) -> Result<T, EngineError> {
        let saved = if result.is_ok() && s.must_save { s.save() } else { Ok(()) };
        self.finish(s);
        let value = result?;
        saved.map_err(failed)?;
        Ok(value)
    }

    /// Writes `last_check` times that were held back. Call before the program exits.
    pub fn flush(&mut self) -> Result<(), EngineError> {
        if self.lazy.is_empty() {
            return Ok(());
        }
        let mut s = self.begin(Access::Write)?;
        s.must_save = true;
        self.conclude(s, Ok(()))
    }

    /// Run once when the program starts: sorts out changes a crash interrupted, removes leftovers, and checks the
    /// installed skills against what was recorded.
    pub fn startup(&mut self, cfg: &Config) -> Result<StartupReport, EngineError> {
        let mut s = self.begin(Access::Write)?;
        let problem = if s.applier.skills_dir.is_dir() { apply::preflight(&s.applier.skills_dir, &s.applier.work_dir).err() } else { None };
        s.applier.sweep(&s.state);
        let mut verify = self.verify_in(&mut s, cfg);
        verify.warnings.extend(self.finish(s));
        Ok(StartupReport { verify, problem })
    }

    /// Compares every installed skill with what was recorded. Needs no network.
    pub fn verify(&mut self, cfg: &Config, access: Access) -> Result<VerifyReport, EngineError> {
        let mut s = self.begin(access)?;
        let mut report = self.verify_in(&mut s, cfg);
        report.warnings.extend(self.finish(s));
        Ok(report)
    }

    fn verify_in(&self, s: &mut Session, cfg: &Config) -> VerifyReport {
        let before = s.stats;
        let mut skills = Vec::new();
        for repo in &cfg.repos {
            for (name, status) in verify_repo(s, &repo.id) {
                skills.push(LocalReport { repo_id: repo.id.clone(), name, status });
            }
        }
        let known: HashSet<&str> = cfg.repos.iter().map(|r| r.id.as_str()).collect();
        let orphans = s.state.repos.iter().filter(|(id, r)| !known.contains(id.as_str()) && !r.skills.is_empty()).map(|(id, _)| id.clone()).collect();
        let tracking = s.tracking();
        VerifyReport {
            skills,
            orphans,
            stats: since(s.stats, before),
            tracking,
            notes: std::mem::take(&mut s.notes),
            warnings: std::mem::take(&mut s.warnings),
        }
    }

    /// Checks repositories against GitHub and applies what is safe to apply.
    pub fn sync(&mut self, cfg: &Config, which: &Which, opts: &SyncOptions, cancel: &AtomicBool) -> Result<SyncReport, EngineError> {
        let mut s = self.begin(if opts.dry_run { Access::ReadOnly } else { Access::Write })?;
        let result = self.sync_in(&mut s, cfg, which, opts, cancel);
        let notes = std::mem::take(&mut s.notes);
        let warnings = self.finish(s);
        result.map(|repos| SyncReport { repos, notes, warnings })
    }

    fn sync_in(&mut self, s: &mut Session, cfg: &Config, which: &Which, opts: &SyncOptions, cancel: &AtomicBool) -> Result<Vec<RepoReport>, EngineError> {
        let ids: Vec<String> = match which {
            Which::Due => self.schedule.due_repos(cfg, &s.state, self.now()),
            Which::All => cfg.repos.iter().filter(|r| r.enabled).map(|r| r.id.clone()).collect(),
            Which::One(id) => match cfg.find(id) {
                Some(r) => vec![r.id.clone()],
                None => return Err(failed(format!("no repository '{id}' is set up"))),
            },
        };
        let mut reports = Vec::new();
        for id in ids {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let Some(repo) = cfg.find(&id) else { continue };
            let before = s.stats;
            let downloaded = s.downloaded;
            let mut report = self.sync_repo(s, repo, opts, cancel);
            report.stats = since(s.stats, before);
            report.downloaded = s.downloaded - downloaded;
            report.finished_at = self.now();
            if !opts.dry_run {
                let interval = repo.effective_interval(cfg.default_interval);
                match &report.outcome {
                    Outcome::Failed(f) => {
                        self.schedule.on_failure(&id, f.kind, f.retry_at, interval, self.now());
                    }
                    Outcome::Cancelled => {}
                    _ => self.schedule.on_success(&id),
                }
                if s.must_save {
                    // A failed save is recorded as a warning and reported with the rest.
                    let _ = s.save();
                }
            }
            reports.push(report);
        }
        Ok(reports)
    }

    fn sync_repo(&self, s: &mut Session, repo: &RepoConfig, opts: &SyncOptions, cancel: &AtomicBool) -> RepoReport {
        let report = RepoReport::new(repo);
        let problems = repo.problems();
        if !problems.is_empty() {
            return report.fail(Failure { kind: FailureKind::Local, message: problems.join("; "), retry_at: None });
        }
        let repo_ref = match RepoRef::of(repo) {
            Ok(r) => r,
            Err(why) => return report.fail(Failure { kind: FailureKind::Local, message: why, retry_at: None }),
        };
        let token = match s.token_for(self.tokens.as_ref(), repo) {
            Ok(t) => t,
            Err(f) => return report.fail(f),
        };

        let stored_etag = if opts.ignore_etag { None } else { s.state.repo(&repo.id).and_then(|r| r.etag.clone()) };
        let reply = match self.remote.tree(&repo_ref, stored_etag.as_deref(), token.as_deref(), cancel) {
            Ok(r) => r,
            // Whatever went wrong, it ends here: no decision is ever made from an error.
            Err(e) if e.kind == FailureKind::Cancelled => return RepoReport { outcome: Outcome::Cancelled, ..report },
            Err(e) => return report.fail(e),
        };
        let (new_etag, snapshot) = match reply {
            TreeReply::NotModified if stored_etag.is_some() => {
                let mut report = report;
                if !opts.dry_run {
                    s.touch_check(&repo.id, self.now());
                }
                report.outcome = Outcome::Unchanged;
                return report;
            }
            TreeReply::NotModified => {
                return report.fail(RemoteError::new(FailureKind::Protocol, "GitHub answered 'not modified' to a request that had no ETag"));
            }
            TreeReply::Snapshot { etag, snapshot } => (etag, snapshot),
        };
        if snapshot.truncated {
            return report.fail(RemoteError::new(FailureKind::TooLarge, "GitHub cut the listing short because the folder is too big; nothing was changed"));
        }

        let single = tree::is_single_skill(&snapshot.entries);
        let discovery = tree::discover(&snapshot.entries, &repo.single_skill_name(), root_len(&s.applier));
        let decisions = plan_repo(s, repo, &discovery, opts.confirm_mass_delete);

        let cx = Ctx { repo, repo_ref, token, single, opts, cancel };
        let run = self.apply_all(s, &cx, &discovery, &decisions);

        let mut report = report;
        report.rejected = discovery.rejected.clone();
        if !opts.dry_run {
            let settled = run.stopped.is_none()
                && !run.cancelled
                && run.local_failures.is_empty()
                && decisions.iter().zip(&run.results).all(|(d, r)| is_settled(&d.verdict, r));
            let now = self.now();
            let rs = s.state.repo_mut(&repo.id);
            // The ETag is kept only when everything is settled; otherwise the next check must look again.
            rs.etag = if settled { new_etag } else { None };
            if run.stopped.is_none() && !run.cancelled {
                rs.last_check = Some(now);
            }
            s.must_save = true;
        }
        report.outcome = classify(&decisions, &run, opts.dry_run);
        report.skills = decisions.into_iter().zip(run.results).map(|(d, result)| SkillReport { name: d.name, verdict: d.verdict, result }).collect();
        report
    }

    fn apply_all(&self, s: &mut Session, cx: &Ctx, discovery: &Discovery, decisions: &[Decision]) -> Run {
        let mut run = Run { results: vec![ApplyResult::NotApplicable; decisions.len()], stopped: None, cancelled: false, local_failures: Vec::new() };
        let mut commit: Option<String> = None;
        for (i, d) in decisions.iter().enumerate() {
            if !d.verdict.is_action() {
                continue;
            }
            if cx.opts.dry_run {
                run.results[i] = ApplyResult::DryRun;
                continue;
            }
            if cx.cancel.load(Ordering::Relaxed) {
                run.cancelled = true;
            }
            if run.cancelled || run.stopped.is_some() {
                run.results[i] = ApplyResult::NotTried;
                continue;
            }
            match self.act(s, cx, discovery, d, &mut commit) {
                Ok(result) => run.results[i] = result,
                Err(e) => {
                    // The state in memory may no longer match the file.
                    s.suspect = true;
                    match e.kind {
                        FailureKind::Cancelled => {
                            run.cancelled = true;
                            run.results[i] = ApplyResult::NotTried;
                        }
                        FailureKind::Local => {
                            run.results[i] = ApplyResult::Failed(e.message.clone());
                            run.local_failures.push(format!("{}: {}", d.name, e.message));
                        }
                        _ => {
                            run.results[i] = ApplyResult::Failed(e.message.clone());
                            run.stopped = Some(e.into());
                        }
                    }
                }
            }
        }
        run
    }

    fn act(&self, s: &mut Session, cx: &Ctx, discovery: &Discovery, d: &Decision, commit: &mut Option<String>) -> Result<ApplyResult, RemoteError> {
        let result = |applied: Applied| match applied {
            Applied::Done => ApplyResult::Done,
            Applied::Held(why) => ApplyResult::Held(why),
        };
        let id = cx.repo.id.as_str();
        match &d.verdict {
            Verdict::Forget => {
                s.applier.forget(&mut s.state, id, &d.name)?;
                Ok(ApplyResult::Done)
            }
            Verdict::Delete => {
                s.ensure_preflight()?;
                Ok(result(s.applier.delete(&mut s.state, id, &d.name)?))
            }
            Verdict::Install | Verdict::Update | Verdict::Adopt => {
                let skill = discovery
                    .skills
                    .iter()
                    .find(|k| k.name == d.name)
                    .ok_or_else(|| RemoteError::new(FailureKind::Protocol, format!("'{}' is missing from the listing", d.name)))?;
                let downloads = d.verdict != Verdict::Adopt;
                if downloads {
                    let size = skill.total_bytes();
                    if s.downloaded + size > cx.opts.max_download_bytes {
                        return Ok(ApplyResult::Held(format!(
                            "{} MiB would go past the {} MiB limit for one check; it will be tried again at the next check",
                            size.div_ceil(1 << 20),
                            cx.opts.max_download_bytes >> 20
                        )));
                    }
                    s.ensure_preflight()?;
                }
                if commit.is_none() {
                    *commit = Some(self.remote.commit(&cx.repo_ref, cx.token.as_deref(), cx.cancel)?);
                }
                let commit = commit.as_deref().unwrap_or_default();
                if !downloads {
                    return Ok(result(s.applier.adopt(&mut s.state, id, skill, commit)?));
                }
                let bytes = Cell::new(0u64);
                let mut fetch = |file: &SkillFile, sink: &mut dyn FnMut(&[u8]) -> bool| -> Result<(), RemoteError> {
                    let rel = if cx.single { file.path.clone() } else { format!("{}/{}", skill.name, file.path) };
                    self.remote.file(&cx.repo_ref, commit, &rel, file.size, cx.token.as_deref(), cx.cancel, &mut |chunk: &[u8]| {
                        bytes.set(bytes.get() + chunk.len() as u64);
                        sink(chunk)
                    })
                };
                let applied = s.applier.install(&mut s.state, id, skill, commit, Mode::Normal, &mut fetch, cx.cancel);
                s.downloaded += bytes.get();
                Ok(result(applied?))
            }
            _ => Ok(ApplyResult::NotApplicable),
        }
    }

    /// Settles one skill that needs the user's decision.
    pub fn resolve(&mut self, cfg: &Config, repo_id: &str, skill: &str, how: Resolution, cancel: &AtomicBool) -> Result<ResolveReport, EngineError> {
        let repo = cfg.find(repo_id).ok_or_else(|| failed(format!("no repository '{repo_id}' is set up")))?;
        let mut s = self.begin(Access::Write)?;
        let result = self.resolve_in(&mut s, repo, skill, how, cancel);
        self.conclude(s, result)
    }

    fn resolve_in(&self, s: &mut Session, repo: &RepoConfig, skill: &str, how: Resolution, cancel: &AtomicBool) -> Result<ResolveReport, EngineError> {
        let key = fold(skill);
        match how {
            Resolution::KeepMine => {
                let rs = s.state.repo_mut(&repo.id);
                rs.remove_skill(skill);
                if !rs.is_ignored(skill) {
                    rs.ignored.push(skill.to_string());
                }
                rs.etag = None;
                s.must_save = true;
                Ok(ResolveReport { message: format!("'{skill}' is now yours: it will not be changed or removed"), backups: Vec::new() })
            }
            Resolution::ManageAgain => {
                let rs = s.state.repo_mut(&repo.id);
                rs.ignored.retain(|n| fold(n) != key);
                rs.etag = None;
                s.must_save = true;
                Ok(ResolveReport { message: format!("'{skill}' will be handled like any other skill at the next check"), backups: Vec::new() })
            }
            Resolution::Overwrite => {
                if s.state.repo(&repo.id).is_some_and(|r| r.is_ignored(skill)) {
                    return Err(failed(format!("'{skill}' is marked as yours; choose 'manage again' first")));
                }
                if let Some(owner) = s.state.names_owned_elsewhere(&repo.id).get(&key) {
                    return Err(failed(format!("the repository {owner} already provides '{skill}'")));
                }
                let problems = repo.problems();
                if !problems.is_empty() {
                    return Err(failed(problems.join("; ")));
                }
                let repo_ref = RepoRef::of(repo).map_err(failed)?;
                let token = s.token_for(self.tokens.as_ref(), repo).map_err(|f| failed(f.message))?;
                let snapshot = match self.remote.tree(&repo_ref, None, token.as_deref(), cancel) {
                    Ok(TreeReply::Snapshot { snapshot, .. }) => snapshot,
                    Ok(TreeReply::NotModified) => return Err(failed("GitHub answered 'not modified' to a request that had no ETag")),
                    Err(e) => return Err(failed(e.message)),
                };
                if snapshot.truncated {
                    return Err(failed("GitHub cut the listing short because the folder is too big"));
                }
                let single = tree::is_single_skill(&snapshot.entries);
                let discovery = tree::discover(&snapshot.entries, &repo.single_skill_name(), root_len(&s.applier));
                let Some(upstream) = discovery.skills.iter().find(|k| fold(&k.name) == key) else {
                    let why = discovery.rejected.iter().find(|r| fold(&r.name) == key).map(|r| format!(" ({})", r.reason)).unwrap_or_default();
                    return Err(failed(format!("'{skill}' cannot be installed from the repository{why}")));
                };
                s.ensure_preflight().map_err(|e| failed(e.message))?;
                let commit = self.remote.commit(&repo_ref, token.as_deref(), cancel).map_err(|e| failed(e.message))?;
                let backups_before = backups_in(&s.applier.work_dir);
                let bytes = Cell::new(0u64);
                let mut fetch = |file: &SkillFile, sink: &mut dyn FnMut(&[u8]) -> bool| -> Result<(), RemoteError> {
                    let rel = if single { file.path.clone() } else { format!("{}/{}", upstream.name, file.path) };
                    self.remote.file(&repo_ref, &commit, &rel, file.size, token.as_deref(), cancel, &mut |chunk: &[u8]| {
                        bytes.set(bytes.get() + chunk.len() as u64);
                        sink(chunk)
                    })
                };
                let applied = s.applier.install(&mut s.state, &repo.id, upstream, &commit, Mode::Overwrite, &mut fetch, cancel);
                s.downloaded += bytes.get();
                let applied = applied.inspect_err(|_| s.suspect = true).map_err(|e| failed(e.message))?;
                let backups: Vec<PathBuf> = backups_in(&s.applier.work_dir).into_iter().filter(|b| !backups_before.contains(b)).collect();
                match applied {
                    Applied::Done => {
                        s.state.repo_mut(&repo.id).etag = None;
                        s.must_save = true;
                        let kept = if backups.is_empty() { String::new() } else { "; what was there is kept in the backups folder".to_string() };
                        Ok(ResolveReport { message: format!("'{skill}' now matches the repository{kept}"), backups })
                    }
                    Applied::Held(why) => Err(failed(why)),
                }
            }
        }
    }

    /// Stops managing a repository's skills. The caller removes the repository from the settings afterwards.
    pub fn remove_repo(&mut self, repo_id: &str, delete_skills: bool) -> Result<RemoveReport, EngineError> {
        let mut s = self.begin(Access::Write)?;
        let mut report = RemoveReport::default();
        let names: Vec<String> = s.state.repo(repo_id).map(|r| r.skills.iter().map(|k| k.name.clone()).collect()).unwrap_or_default();
        for name in names {
            if !delete_skills {
                report.kept.push((name, "kept as you asked".to_string()));
                continue;
            }
            let outcome = s.ensure_preflight().and_then(|()| s.applier.delete(&mut s.state, repo_id, &name));
            match outcome {
                Ok(Applied::Done) => report.deleted.push(name),
                Ok(Applied::Held(why)) => report.kept.push((name, why)),
                Err(e) => {
                    s.suspect = true;
                    report.kept.push((name, e.message));
                }
            }
        }
        if s.state.repos.remove(repo_id).is_some() {
            s.must_save = true;
        }
        self.schedule.forget(repo_id);
        self.lazy.remove(repo_id);
        s.lazy.remove(repo_id);
        self.conclude(s, Ok(report))
    }
}

/// Checks every owned skill of one repository and says what was found. Interrupted ones are not looked at.
fn verify_repo(s: &mut Session, repo_id: &str) -> Vec<(String, LocalStatus)> {
    let check = s.volume().check;
    let Session { state, applier, stats, must_save, .. } = s;
    let Some(repo) = state.repos.get_mut(repo_id) else { return Vec::new() };
    let mut out = Vec::new();
    for skill in &mut repo.skills {
        if skill.pending.is_some() {
            out.push((skill.name.clone(), LocalStatus::Interrupted));
            continue;
        }
        let dir = applier.skills_dir.join(&skill.name);
        let outcome = verify::verify_skill(&dir, skill, check, &mut applier.hasher);
        stats.add(outcome.stats);
        if verify::apply_refresh(skill, &outcome.refreshed) {
            *must_save = true;
        }
        out.push((skill.name.clone(), LocalStatus::Checked(outcome.local)));
    }
    out
}

/// Turns what is upstream, what is recorded and what is on disk into decisions.
fn plan_repo(s: &mut Session, repo: &RepoConfig, discovery: &Discovery, confirm_mass_delete: bool) -> Vec<Decision> {
    let local: HashMap<String, LocalState> = verify_repo(s, &repo.id)
        .into_iter()
        .filter_map(|(name, status)| match status {
            LocalStatus::Checked(local) => Some((fold(&name), local)),
            LocalStatus::Interrupted => None,
        })
        .collect();
    let repo_state = s.state.repo(&repo.id);
    let owned: Vec<OwnedSkill> = repo_state.map(|r| r.skills.clone()).unwrap_or_default();
    let ignored: HashSet<String> = repo_state.map(|r| r.ignored.iter().map(|n| fold(n)).collect()).unwrap_or_default();
    let interrupted: Vec<String> = owned.iter().filter(|o| o.pending.is_some()).map(|o| o.name.clone()).collect();
    let other_owners = s.state.names_owned_elsewhere(&repo.id);
    let owned_keys: HashSet<String> = owned.iter().map(|o| fold(&o.name)).collect();

    let mut unmanaged = HashMap::new();
    for skill in &discovery.skills {
        let key = fold(&skill.name);
        if owned_keys.contains(&key) || ignored.contains(&key) || other_owners.contains_key(&key) {
            continue;
        }
        let dir = s.applier.skills_dir.join(&skill.name);
        if exists_no_follow(&dir) {
            unmanaged.insert(key, verify::compare_unmanaged(&dir, skill, &mut s.applier.hasher));
        }
    }

    let not_checked = || LocalState::Dirty(DirtyInfo { modified: vec!["(not checked)".to_string()], ..DirtyInfo::default() });
    let views: Vec<OwnedView> = owned
        .iter()
        .filter(|o| o.pending.is_none())
        .map(|o| OwnedView { skill: o, local: local.get(&fold(&o.name)).cloned().unwrap_or_else(not_checked) })
        .collect();
    reconcile::plan(&Input {
        desired: &discovery.skills,
        rejected: &discovery.rejected,
        owned: &views,
        other_owners: &other_owners,
        unmanaged: &unmanaged,
        ignored: &ignored,
        confirm_mass_delete,
        interrupted: &interrupted,
    })
}

/// Is there nothing left to do for this skill, so the listing it came from needs no second look?
fn is_settled(verdict: &Verdict, result: &ApplyResult) -> bool {
    match verdict {
        Verdict::InSync | Verdict::ModifiedLocally(_) | Verdict::MissingLocally | Verdict::Skipped(Skip::Ignored) | Verdict::Skipped(Skip::Rejected { .. }) => {
            true
        }
        Verdict::Install | Verdict::Update | Verdict::Adopt | Verdict::Delete | Verdict::Forget => *result == ApplyResult::Done,
        Verdict::Held(_) | Verdict::Skipped(Skip::Collision { .. }) => false,
    }
}

fn classify(decisions: &[Decision], run: &Run, dry_run: bool) -> Outcome {
    if let Some(f) = &run.stopped {
        return Outcome::Failed(f.clone());
    }
    if run.cancelled {
        return Outcome::Cancelled;
    }
    if !run.local_failures.is_empty() {
        let first = run.local_failures[0].clone();
        let more = if run.local_failures.len() > 1 { format!(" (and {} more)", run.local_failures.len() - 1) } else { String::new() };
        return Outcome::Failed(Failure { kind: FailureKind::Local, message: format!("{first}{more}"), retry_at: None });
    }
    let attention = decisions.iter().zip(&run.results).any(|(d, r)| d.verdict.needs_attention() || matches!(r, ApplyResult::Held(_)));
    let acted = decisions.iter().any(|d| d.verdict.is_action());
    match (dry_run, acted, attention) {
        (true, true, _) => Outcome::WouldChange,
        (_, _, true) => Outcome::NeedsAttention,
        (false, true, false) => Outcome::Synced,
        _ => Outcome::Unchanged,
    }
}

fn backups_in(work_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(work_dir) else { return Vec::new() };
    let mut out: Vec<PathBuf> = entries.flatten().filter(|e| e.file_name().to_string_lossy().starts_with("backup-")).map(|e| e.path()).collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsx::unique_suffix;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tss-engine-{name}-{}", unique_suffix()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_lock_cannot_be_held_twice_and_is_released_on_drop() {
        let dir = temp("lock");
        let path = dir.join(LOCK_FILE);
        let first = open_lock(&path, Access::Write).unwrap().expect("created");
        let second = open_lock(&path, Access::Write).unwrap_err();
        assert!(is_busy(&second), "{second:?}");
        let third = open_lock(&path, Access::ReadOnly).unwrap_err();
        assert!(is_busy(&third), "read-only callers wait for a writer too: {third:?}");
        drop(first);
        assert!(open_lock(&path, Access::Write).unwrap().is_some());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_read_only_caller_creates_nothing_when_nobody_ever_wrote() {
        let dir = temp("lock-ro");
        let path = dir.join("sub").join(LOCK_FILE);
        assert!(open_lock(&path, Access::ReadOnly).unwrap().is_none());
        assert!(!dir.join("sub").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_stamp_follows_the_file() {
        let dir = temp("stamp");
        let path = dir.join("state.json");
        assert!(stamp_of(&path) == Some(Stamp(None)), "a missing file has a stamp of its own");
        State::default().save(&path).unwrap();
        let first = stamp_of(&path).unwrap();
        assert!(first != Stamp(None));
        assert!(stamp_of(&path) == Some(first), "looking does not change it");
        let mut st = State::default();
        st.repo_mut("r").last_check = Some(1);
        st.save(&path).unwrap();
        assert!(stamp_of(&path) != Some(first), "a rewrite changes it");
        fs::remove_dir_all(dir).unwrap();
    }
}
