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

mod loose;
mod plugins;
mod root;
mod roots;
mod source_preview;
pub use source_preview::LocationPreview;
pub(crate) use source_preview::fresh_lane_ids;

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
use crate::diff::{self, Content, FileChange, Presence};
use crate::fsx::{exists_no_follow, join_rel};
use crate::github::{FailureKind, Remote, RemoteError, RepoRef, TokenProvider, TreeReply};
use crate::hash::blob_hex;
use crate::interval::{UnixSecs, now_secs};
use crate::pathsafe::{self, fold};
use crate::reconcile::{self, Decision, DirtyInfo, Input, LocalState, OwnedView, Skip, UnmanagedState, Verdict};
use crate::scheduler::Schedule;
use crate::secret::Secret;
use crate::source::SourceKind;
use crate::state::{Fingerprint, Folder, HistorySearch, LocalEvidence, ManagedFile, OwnedSkill, Pending, State, VolumeId};
use crate::text;
use crate::tree::{self, Discovery, Rejected, SkillFile};
use crate::verify::{self, Check, Likeness, Spot, Spots, Stats, check_for_volume};
use crate::win::fs::{self as winfs, FileHasher, Kind, probe, volume_id};

const LOCK_FILE: &str = "engine.lock";
const DEFAULT_LOCK_WAIT: Duration = Duration::from_secs(3);
/// Another writer of the same settings (a second computer sharing the portable folder, or a settings save) holds
/// their lock only while it changes files. A change waits this long for it before the check fails.
const SETTINGS_LOCK_WAIT: Duration = Duration::from_secs(10);
/// The most one check downloads. A skill that would go past it waits for the next check.
pub const DEFAULT_MAX_DOWNLOAD: u64 = 512 * 1024 * 1024;
/// A quick attempt, then the same small slices on the existing worker. This is not a total-history limit.
pub const HISTORY_SLICE: usize = 8;
/// Added to 'not found' when there is no token to try: the repository may be private.
const SIGN_IN: &str = "To read a private repository, sign in with the GitHub CLI (gh auth login) or save a token in Settings";
/// "Show changes" reads and downloads files of at most this size, and at most `DIFF_TOTAL_BYTES` from each side; it
/// names the others without showing them.
pub const DIFF_FILE_BYTES: u64 = 1 << 20;
const DIFF_TOTAL_BYTES: u64 = 4 << 20;
const TOO_BIG: &str = "it is too big to show";
const TOO_MUCH: &str = "the other files that are different are already as much as can be shown";

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
    PauseHistory,
    ResumeHistory,
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

fn folder_changed() -> EngineError {
    failed("the skills folder changed since the app last looked at it; check now (or run 'tss check') first, then try again")
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
pub struct PluginReport {
    pub name: String,
    pub version: Option<String>,
    pub publisher: Option<String>,
    /// Branch-root path on GitHub (not a local destination).
    pub path: Option<String>,
    pub status: crate::plugins::Status,
    pub choices: Vec<crate::plugins::Choice>,
    pub result: ApplyResult,
}

impl PluginReport {
    pub(crate) fn needs_attention(&self) -> bool {
        match self.result {
            ApplyResult::Failed(_) | ApplyResult::Held(_) => true,
            // Dry runs always lack the native observations needed to plan.
            ApplyResult::DryRun if self.status == crate::plugins::Status::Blocked => false,
            _ => self.status.needs_attention(self.result == ApplyResult::Done),
        }
    }
}

#[derive(Clone, Debug)]
pub struct RepoReport {
    pub repo_id: String,
    pub label: String,
    pub outcome: Outcome,
    pub skills: Vec<SkillReport>,
    pub plugins: Vec<PluginReport>,
    /// Upstream skills that cannot be installed (and the reason).
    pub rejected: Vec<Rejected>,
    pub stats: Stats,
    pub downloaded: u64,
    pub finished_at: UnixSecs,
    /// The listing this check read had no skills in it at all. False when no listing was read: the check failed, or
    /// GitHub said nothing changed.
    pub lists_nothing: bool,
}

impl RepoReport {
    fn new(repo: &RepoConfig) -> RepoReport {
        RepoReport {
            repo_id: repo.id.clone(),
            label: repo.label(),
            outcome: Outcome::Unchanged,
            skills: Vec::new(),
            plugins: Vec::new(),
            rejected: Vec::new(),
            stats: Stats::default(),
            downloaded: 0,
            finished_at: 0,
            lists_nothing: false,
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
    /// For display only: what each source whose complete listing this check read held, by source ID. A source that
    /// failed or was unchanged is not here; keep showing what was seen before.
    pub seen: Vec<(String, crate::seen::Seen)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalStatus {
    Checked(LocalState),
    Unavailable(String),
    /// An earlier change was interrupted and is not sorted out yet.
    Interrupted,
    History {
        paused: bool,
        complete: bool,
        waiting: bool,
    },
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
    /// The skills folder does not exist, so its drive could not be asked about a journal.
    pub folder_missing: bool,
}

#[derive(Debug)]
pub struct VerifyReport {
    pub skills: Vec<LocalReport>,
    /// The lanes this look walked: every lane whose type is on. It says nothing about any other lane.
    pub lanes: Vec<String>,
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
    pub message: Option<&'static str>,
    pub deleted: Vec<String>,
    /// Skills that stay installed, with the reason, and are now the user's.
    pub kept: Vec<(String, String)>,
}

/// What "Show changes" found for one skill.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffReport {
    /// GitHub lists the skill. When it does not, every file on this PC shows as only here.
    pub on_github: bool,
    /// The skill's folder is on this PC.
    pub here: bool,
    /// The files that are different. Empty when the two copies are the same.
    pub files: Vec<FileChange>,
}

/// What the record says about a skill the app put in place, for the window. Nothing is read from disk to tell it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillFacts {
    pub name: String,
    /// The GitHub commit its files came from.
    pub commit: String,
    pub files: usize,
    /// The size of its files as GitHub has them.
    pub bytes: u64,
    /// When the app last installed, updated or adopted it. `None` in records written before the app kept this.
    pub updated: Option<UnixSecs>,
}

type StepHook = Box<dyn FnMut(Step) -> Result<(), RemoteError> + Send>;

/// Tells which drive a folder is on and whether that drive keeps a change journal.
type VolumeProbe = fn(&Path) -> io::Result<VolumeId>;

struct SkillComparison {
    source: RepoConfig,
    dir: PathBuf,
    name: String,
    single: bool,
    desired: Option<tree::Skill>,
    local: verify::SkillReview,
}

pub struct Engine {
    paths: Paths,
    /// Production clients identify the active store; an in-process engine can be driven entirely by its caller.
    settings: Option<(PathBuf, bool)>,
    remote: Box<dyn Remote>,
    tokens: Box<dyn TokenProvider>,
    plugin_manager: Box<dyn crate::plugins::native::PluginManager>,
    native_plugin_manager: bool,
    /// Attention rows that a complete check in this process has reported.
    plugin_attention_rows: HashSet<String>,
    clock: Box<dyn Fn() -> UnixSecs + Send + Sync>,
    lock_wait: Duration,
    cache: Cache,
    skill_review: Option<SkillComparison>,
    /// The loose file shown by the worker's last comparison, never persisted or refreshed by a poll.
    file_review: Option<loose::FileReview>,
    /// `last_check` values that were not worth a write on their own. Saved with the next real save, or by `flush`.
    lazy: HashMap<String, UnixSecs>,
    /// What `seen.json` holds, once read in this process (display only, see `crate::seen`).
    seen: Option<std::collections::BTreeMap<String, crate::seen::Seen>>,
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
    folder_missing: bool,
}

/// Everything one call works on. Created by `begin`, returned by `end`.
struct Session {
    _lock: Option<File>,
    /// The settings this call was started with: every source, its lanes and the type and folder rules.
    config: Config,
    roots: roots::Roots,
    settings: Option<(PathBuf, bool, Config)>,
    writer: Option<File>,
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
    /// Sources whose complete listing was read in this call, for `SyncReport::seen`.
    seen: Vec<(String, crate::seen::Seen)>,
    notes: Vec<Note>,
    warnings: Vec<String>,
}

impl Session {
    /// Under engine.lock, re-read the active store before a mutation and exclude cooperating settings writers.
    /// Never acquire this guard just to handle a conditional 304.
    fn authorize(&mut self) -> Result<(), RemoteError> {
        let Some((dir, portable, expected)) = &self.settings else { return Ok(()) };
        if self.writer.is_some() {
            return Ok(());
        }
        let home = self.applier.state_path.parent().expect("state file has a parent");
        let local = |e| RemoteError::new(FailureKind::Local, e);
        let lock = crate::config::settings_write_lock_within(if *portable { dir } else { home }, SETTINGS_LOCK_WAIT).map_err(local)?;
        let shared = crate::portable::load_sources(dir).map_err(local)?;
        if shared.is_some() != *portable
            || match shared {
                Some(active) => active != *expected,
                None => Config::load_sources(&home.join(crate::config::CONFIG_FILE)).map_err(local)? != *expected,
            }
        {
            return Err(local("the source settings changed; reload them and try again".into()));
        }
        self.writer = Some(lock);
        Ok(())
    }

    /// Finishes or undoes the changes a crash interrupted. Only a call that may write does it.
    fn recover(&mut self, cfg: &Config) -> Result<(), EngineError> {
        if self.access == Access::Write && self.state.has_pending() {
            self.authorize().map_err(|e| failed(e.message))?;
            match self.applier.recover(&mut self.state) {
                Ok(notes) => self.notes = notes,
                Err(e) => {
                    self.suspect = true;
                    self.warnings.push(format!("could not sort out interrupted changes: {e}"));
                }
            }
        }
        if self.access == Access::Write {
            self.recover_files(cfg)?;
        }
        Ok(())
    }

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
        let current = match (self.volume_probe)(&self.applier.skills_dir) {
            Ok(current) => current,
            Err(e) => return Volume { current: None, check: Check::Full, changed: false, folder_missing: e.kind() == io::ErrorKind::NotFound },
        };
        let changed = self.state.volume != Some(current);
        let check = check_for_volume(self.state.volume.as_ref(), &current);
        if changed {
            clear_fingerprints(&mut self.state);
            self.state.volume = Some(current);
            self.must_save = true;
        }
        let v = Volume { current: Some(current), check, changed, folder_missing: false };
        self.vol = Some(v);
        v
    }

    fn tracking(&mut self) -> Tracking {
        let v = self.volume();
        Tracking { check: v.check, volume_changed: v.changed, journal: v.current.is_some_and(|c| c.journal != 0), folder_missing: v.folder_missing }
    }

    /// Remembers a successful check that found nothing new. Not worth a write by itself.
    fn touch_check(&mut self, repo_id: &str, at: UnixSecs) {
        if let Some(last) = self.state.last_check_mut(repo_id) {
            *last = Some(at);
            self.lazy.insert(repo_id.to_string(), at);
        }
    }

    /// Remembers the full check of a lane that has no record: worth a write only when more than its time changed.
    fn remember_lane_poll(&mut self, lane: &RepoConfig, etag: Option<String>, at: UnixSecs, lists_nothing: bool, attention: bool) {
        self.must_save |= self.state.set_lane_poll(&lane.id, crate::state::LanePoll::new(lane, etag, at, lists_nothing, attention));
        self.touch_check(&lane.id, at);
    }

    /// The check data of a lane that is no longer set up, or that was recorded for another location, is ignored and
    /// goes with the next save.
    fn prune_lane_polls(&mut self, cfg: &Config) {
        self.state.retain_lane_polls(|id, poll| match cfg.find(id) {
            Some(lane) => poll.matches(lane),
            None => cfg.known_lanes().iter().any(|(known, kind)| known == id && *kind == poll.kind),
        });
    }

    /// The user's token, looked up at most once per session and only when a repository needs it.
    fn token(&mut self, tokens: &dyn TokenProvider) -> Result<Arc<Secret>, String> {
        self.token.get_or_insert_with(|| tokens.token().map(Arc::new)).clone()
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

/// Whether a skill's folder is on this PC. A link is never followed, so a link there is an error.
fn here(dir: &Path) -> Result<bool, EngineError> {
    match winfs::kind_at(dir).map_err(|e| failed(format!("{} cannot be read: {e}", dir.display())))? {
        None => Ok(false),
        Some(Kind::Dir) => Ok(true),
        Some(_) => Err(failed(format!("{} is not a plain folder (a link is never followed)", dir.display()))),
    }
}

/// The paths "Show changes" looks at in a skill's folder: the files on GitHub and the files the app installed there,
/// once each whatever their case. Any other file there is one the user added, which the app never touches or shows.
fn wanted_here(upstream: &[SkillFile], recorded: Option<&OwnedSkill>) -> Vec<String> {
    let mut seen = HashSet::new();
    let installed = recorded.into_iter().flat_map(|k| k.files.iter().map(|f| &f.path));
    upstream.iter().map(|f| &f.path).chain(installed).filter(|path| seen.insert(fold(path))).cloned().collect()
}

/// Display rows may discard lines and entire files. Keep separate evidence for an overwrite choice.
type ReadHere = (Vec<(String, Content)>, HashSet<String>, Vec<(String, verify::ReviewFile)>);

/// Whether there is a plain file at `rel` in a skill's folder, or why it is not read. No link is followed.
fn spot_file(spots: &mut Spots, rel: &str, stats: &mut Stats) -> Result<bool, &'static str> {
    match spots.find(rel, stats) {
        Spot::Missing => Ok(false),
        Spot::Found(found) => match found.kind {
            Kind::File => Ok(true),
            Kind::Link => Err("it is a link, which is never followed"),
            Kind::Dir => Err("it is a folder here, not a file"),
        },
        Spot::Blocked => Err("a folder on its way is a link or a file here, and a link is never followed"),
        Spot::Unreadable => Err("it could not be read"),
    }
}

/// A file read to be shown: its bytes while `budget` has room for them, or why it is not shown (`too_much` when there is
/// no room left).
fn shown_content(read: io::Result<winfs::FileRead>, budget: &mut u64, too_much: &str) -> Content {
    match read {
        Ok(winfs::FileRead { bytes: Some(bytes), len, .. }) => {
            *budget -= len;
            Content::Bytes(bytes)
        }
        Ok(read) if read.len > DIFF_FILE_BYTES => Content::Hidden(TOO_BIG.into()),
        Ok(_) => Content::Hidden(too_much.into()),
        Err(e) if e.kind() == io::ErrorKind::FileTooLarge => Content::Hidden(TOO_BIG.into()),
        Err(e) => Content::Hidden(format!("it could not be read: {e}")),
    }
}

/// Reads the files at `rels` on this PC for "Show changes", comparing each with its copy on GitHub (`theirs`, by folded
/// path). A path with nothing there is left out. Bytes are kept only while there is room.
fn read_here(dir: &Path, rels: Vec<String>, theirs: &HashMap<String, &SkillFile>, cancel: &AtomicBool) -> Result<ReadHere, EngineError> {
    use verify::ReviewFile;
    let mut hasher = FileHasher::default();
    let mut spots = Spots::new(dir);
    let mut stats = Stats::default();
    let mut budget = DIFF_TOTAL_BYTES;
    let mut different = Vec::new();
    let mut same = HashSet::new();
    let mut evidence = Vec::new();
    for rel in rels {
        if cancel.load(Ordering::Relaxed) {
            return Err(failed("showing the changes was stopped"));
        }
        match spot_file(&mut spots, &rel, &mut stats) {
            Ok(true) => {}
            Ok(false) => {
                evidence.push((rel, ReviewFile::Missing));
                continue;
            }
            Err(why) => {
                evidence.push((rel.clone(), ReviewFile::Unreadable));
                different.push((rel, Content::Hidden(why.into())));
                continue;
            }
        }
        let key = fold(&rel);
        let upstream = theirs.get(&key);
        // A file too big to show that is larger than its copy on GitHub is different whatever it holds; it is not read.
        let limit = upstream.map_or(0, |t| t.size).max(DIFF_FILE_BYTES);
        let read = hasher.read(&join_rel(dir, &rel), DIFF_FILE_BYTES.min(budget), limit);
        let proof = match &read {
            Ok(r) => ReviewFile::Read { len: r.len, blob: r.blob.clone() },
            Err(_) => ReviewFile::Unreadable,
        };
        evidence.push((rel.clone(), proof));
        if matches!(&read, Ok(read) if upstream.is_some_and(|t| t.blob == read.blob)) {
            same.insert(key);
            continue;
        }
        different.push((rel, shown_content(read, &mut budget, TOO_MUCH)));
    }
    Ok((different, same, evidence))
}

/// Reads the files at `rels` in a skill's folder for Preview, in order, each whole. A path with nothing there is named
/// with the reason. Bytes are kept only while there is room.
fn read_whole(dir: &Path, rels: Vec<String>, cancel: &AtomicBool) -> Result<Vec<(String, Content)>, EngineError> {
    let mut hasher = FileHasher::default();
    let mut spots = Spots::new(dir);
    let mut stats = Stats::default();
    let mut budget = DIFF_TOTAL_BYTES;
    let mut out = Vec::new();
    for rel in rels {
        if cancel.load(Ordering::Relaxed) {
            return Err(failed("showing the files was stopped"));
        }
        let content = match spot_file(&mut spots, &rel, &mut stats) {
            Ok(true) => {
                let read = hasher.read(&join_rel(dir, &rel), DIFF_FILE_BYTES.min(budget), DIFF_FILE_BYTES);
                shown_content(read, &mut budget, "the files before it are already as much as can be shown")
            }
            Ok(false) => Content::Hidden("it is not on this PC".into()),
            Err(why) => Content::Hidden(why.into()),
        };
        out.push((rel, content));
    }
    Ok(out)
}

/// What Preview shows of one skill: see [`Engine::preview`].
fn preview_in(s: &Session, repo_id: &str, skill: &str, cancel: &AtomicBool) -> Result<DiffReport, EngineError> {
    let owned = s.state.repo(repo_id).and_then(|r| r.skill(skill)).ok_or_else(|| failed(format!("'{skill}' was not installed by this app")))?;
    if owned.pending.is_some() {
        return Err(failed(format!("'{skill}' is in the middle of a change, which the next check finishes")));
    }
    let dir = s.applier.skills_dir.join(&owned.name);
    if !here(&dir)? {
        return Err(failed(format!("the folder of '{skill}' is not on this PC")));
    }
    let mut rels: Vec<String> = owned.files.iter().map(|f| f.path.clone()).collect();
    rels.sort_by_cached_key(|rel| fold(rel));
    let files = read_whole(&dir, rels, cancel)?;
    let files = files.into_iter().map(|(path, content)| FileChange { path, presence: Presence::Both, body: diff::whole(content) }).collect();
    Ok(DiffReport { on_github: true, here: true, files })
}

/// What one repository's check needs besides the session.
struct Ctx<'a> {
    repo: &'a RepoConfig,
    repo_ref: RepoRef<'a>,
    token: Option<Arc<Secret>>,
    /// Folded skill name to the skill's folder relative to the source folder (empty when the source folder is the
    /// skill). A skill not in it sits in a folder named after it.
    dirs: HashMap<String, String>,
    opts: &'a SyncOptions,
    cancel: &'a AtomicBool,
    /// What the copies on GitHub of files read to compare a folder with them are compared by: folded skill name, then
    /// path. For a folder nobody owns that is the same as upstream, and for one we own that was edited into it.
    theirs: HashMap<String, HashMap<String, String>>,
    historical: HashMap<String, HistoricalProof>,
    tree: String,
}

struct HistoricalProof {
    skill: tree::Skill,
    commit: String,
    theirs: HashMap<String, String>,
}

fn source_key(repo: &RepoConfig) -> String {
    format!("{}\n{}\n{}", repo.repo, repo.branch, repo.path)
}

fn evidence_valid(s: &mut Session, h: &HistorySearch) -> bool {
    let vol = s.volume();
    if h.volume != vol.current || h.evidence.is_empty() || h.evidence.values().any(|e| *e == LocalEvidence::Other) {
        return false;
    }
    let dir = s.applier.skills_dir.join(&h.name);
    verify::history_evidence(&dir, h.evidence.keys().map(String::as_str), vol.current.is_some_and(|v| v.journal != 0), &mut s.applier.hasher) == h.evidence
}

impl Ctx<'_> {
    /// Where a skill's file is in the repository's folder.
    fn rel(&self, skill: &str, path: &str) -> String {
        let dir = self.dir(skill);
        if dir.is_empty() { path.to_string() } else { format!("{dir}/{path}") }
    }

    /// Where a skill's folder is in the repository's folder.
    fn dir(&self, skill: &str) -> String {
        self.dirs.get(&fold(skill)).cloned().unwrap_or_else(|| skill.to_string())
    }
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
            settings: None,
            remote,
            tokens,
            plugin_manager: Box::new(crate::win::copilot::Copilot::new()),
            native_plugin_manager: true,
            plugin_attention_rows: HashSet::new(),
            clock: Box::new(now_secs),
            lock_wait: DEFAULT_LOCK_WAIT,
            cache: Cache::default(),
            skill_review: None,
            file_review: None,
            lazy: HashMap::new(),
            seen: None,
            schedule: Schedule::new(),
            step_hook: None,
            volume_probe: volume_id,
        }
    }

    pub fn set_skills_dir(&mut self, dir: PathBuf) {
        self.paths.skills = dir;
        self.plugin_attention_rows.clear();
    }

    pub(crate) fn retain_plugin_attention_rows(&mut self, ids: &[String]) {
        self.plugin_attention_rows.retain(|id| ids.contains(id));
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    pub(crate) fn settings_at(&mut self, executable_dir: PathBuf, portable: bool) {
        self.settings = Some((executable_dir, portable));
    }

    pub fn schedule(&self) -> &Schedule {
        &self.schedule
    }

    /// When the app next needs to wake up, or `None` when nothing is configured. Uses what the last call saw.
    pub fn next_wake(&self, cfg: &Config) -> Option<UnixSecs> {
        self.deadlines(cfg, &self.cache.state).filter_map(|(_, due)| due).min()
    }

    /// Only timing enters the scheduler's skills-shaped input; a projected record is never saved or used for ownership.
    fn deadlines<'a>(&'a self, cfg: &'a Config, state: &'a State) -> impl Iterator<Item = (&'a str, Option<UnixSecs>)> {
        let now = self.now();
        // Saved attention whose rows this process does not hold needs a listing, once in each run.
        let unseen_attention = |id: &str, attention: bool| attention && !self.plugin_attention_rows.contains(id);
        cfg.repos().iter().map(move |repo| {
            let one = Config::lane_only(repo.clone(), cfg.default_interval);
            let mut timing = State::default();
            let view = if repo.kind.is_skills() {
                state
            } else {
                let unrecorded = || state.lane_poll(repo).map(|p| (p.last_check, if unseen_attention(&repo.id, p.attention) { &None } else { &p.etag }));
                let poll = if repo.kind == SourceKind::Plugins {
                    match state.plugin_sources().get(&repo.id) {
                        Some(r) if r.origin.matches_lane(repo) => {
                            let etag = if unseen_attention(&repo.id, r.poll.needs_attention) { &None } else { &r.poll.etag };
                            Some((r.poll.last_check, etag))
                        }
                        Some(_) => None,
                        None => unrecorded(),
                    }
                } else {
                    match state.loose_sources().get(&repo.id) {
                        Some(r) => Some((r.poll.last_check, &r.poll.etag)),
                        None => unrecorded(),
                    }
                };
                if let Some((at, etag)) = poll {
                    let r = timing.repo_mut(&repo.id);
                    r.last_check = at;
                    r.etag.clone_from(etag);
                }
                &timing
            };
            (repo.id.as_str(), self.schedule.next_wake(&one, view, now))
        })
    }

    pub fn last_check(&self, repo_id: &str) -> Option<UnixSecs> {
        self.cache
            .state
            .repo(repo_id)
            .and_then(|r| r.last_check)
            .or_else(|| self.cache.state.loose_sources().get(repo_id)?.poll.last_check)
            .or_else(|| self.cache.state.plugin_sources().get(repo_id)?.poll.last_check)
            .or_else(|| self.cache.state.lane_polls().get(repo_id)?.last_check)
    }

    /// The skills of a repository the user keeps as their own, as of the last call.
    pub fn kept(&self, repo_id: &str) -> Vec<String> {
        self.cache
            .state
            .repo(repo_id)
            .map(|r| r.ignored.clone())
            .or_else(|| Some(self.cache.state.loose_sources().get(repo_id)?.poll.ignored.clone()))
            .unwrap_or_default()
    }

    /// What the record says about each skill of a repository the app put in place, as of the last call. A skill whose
    /// last change was interrupted has nothing to tell until recovery sorts it out.
    pub fn facts(&self, repo_id: &str) -> Vec<SkillFacts> {
        if let Some(files) = self.cache.state.loose_sources().get(repo_id) {
            return files
                .leaves()
                .iter()
                .filter(|l| !l.interrupted())
                .filter_map(|l| {
                    let v = l.installed.as_ref()?;
                    Some(SkillFacts { name: l.name.clone(), commit: v.commit.clone(), files: 1, bytes: v.file.size, updated: Some(v.updated) })
                })
                .collect();
        }
        let Some(repo) = self.cache.state.repo(repo_id) else { return Vec::new() };
        repo.skills
            .iter()
            .filter(|k| k.pending.is_none())
            .map(|k| SkillFacts {
                name: k.name.clone(),
                commit: k.commit.clone(),
                files: k.files.len(),
                bytes: k.files.iter().map(|f| f.size).sum(),
                updated: k.updated,
            })
            .collect()
    }

    /// Local plugin records as of the last call, for read-only status. No Copilot command is run.
    pub(crate) fn plugin_record(&self, source: &RepoConfig) -> Option<&crate::state::PluginSourceState> {
        self.cache.state.plugin_sources().get(&source.id).filter(|r| r.origin.matches_lane(source))
    }

    /// Whether the last listing read for a repository had no skills in it at all, as of the last call.
    pub fn lists_nothing(&self, repo_id: &str) -> bool {
        self.cache.state.repo(repo_id).is_some_and(|r| r.lists_nothing)
            || self.cache.state.loose_sources().get(repo_id).is_some_and(|r| r.poll.lists_nothing)
            || self.cache.state.plugin_sources().get(repo_id).is_some_and(|r| r.poll.lists_nothing)
            || self.cache.state.lane_polls().get(repo_id).is_some_and(|p| p.lists_nothing)
    }

    /// Whether a repository was found to be readable only with the user's GitHub sign-in, as of the last call.
    pub fn needs_token(&self, repo_id: &str) -> bool {
        self.cache.state.repo(repo_id).is_some_and(|r| r.needs_token)
            || self.cache.state.loose_sources().get(repo_id).is_some_and(|r| r.poll.needs_token)
            || self.cache.state.plugin_sources().get(repo_id).is_some_and(|r| r.poll.needs_token)
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

    /// Starts a call: takes the lock, reads the state and ties it to the skills folder. Says whether the records were
    /// made for another skills folder. Changes a crash interrupted are not sorted out yet; `begin` does that.
    fn open(&mut self, cfg: &Config, access: Access) -> Result<(Session, bool), EngineError> {
        let mut ids = HashSet::new();
        if cfg.repos().iter().any(|r| r.id.trim().is_empty() || !ids.insert(&r.id)) {
            return Err(failed("each source needs a distinct, nonempty ID"));
        }
        let roots = roots::check(&self.paths, cfg, None).map_err(failed)?;
        let lock = self.acquire(access)?;
        let path = self.paths.state_file();
        let mut warnings = Vec::new();
        let mut suspect = false;
        let mut must_save = false;
        let mut moved = false;

        let stamp = stamp_of(&path);
        let reusable = self.cache.stamp.is_some() && self.cache.stamp == stamp;
        let mut state = if reusable {
            std::mem::take(&mut self.cache.state)
        } else {
            self.plugin_attention_rows.clear();
            let loaded = State::load_sources_with(&path, access == Access::Write).map_err(|e| failed(format!("cannot read {}: {e}", path.display())))?;
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
        roots.validate_state(&state, cfg).map_err(failed)?;

        // Records made for another skills folder must not be applied to this one, nor their interrupted changes
        // finished here. `begin` does that after this, so this comes first.
        match state.follow_folder(&self.paths.skills.to_string_lossy()) {
            Folder::Same => {}
            Folder::Recorded => must_save |= !state.repos.is_empty(),
            Folder::Moved { from, managed, unfinished, back } => {
                must_save = true;
                moved = true;
                // Check times held back belong to the old records.
                self.lazy.clear();
                let here = self.paths.skills.display();
                if back {
                    let mut text =
                        format!("the skills folder changed from {from} back to {here}, which the app used before: the skills there are managed as they were");
                    if managed {
                        text.push_str(", and those in the old folder were left there as they are and are no longer kept up to date");
                    }
                    warnings.push(text);
                } else if managed {
                    warnings.push(format!(
                        "the skills folder changed from {from} to {here}: the skills installed in the old folder were left there as they are and are no longer kept up to date, and the new folder gets its own copies"
                    ));
                }
                if !unfinished.is_empty() {
                    let old_work = Paths::new(self.paths.home.clone(), PathBuf::from(&from)).work_dir();
                    warnings.push(format!(
                        "a change to {} was interrupted in the old skills folder; it will be finished if the app uses that folder again, and until then what it set aside is in {}",
                        unfinished.join(", "),
                        old_work.display()
                    ));
                }
            }
        }

        for (id, at) in &self.lazy {
            if let Some(last) = state.last_check_mut(id) {
                if last.is_none_or(|c| c < *at) {
                    *last = Some(*at);
                }
            }
        }

        let mut applier = Applier::new(self.paths.skills.clone(), self.paths.work_dir(), path, self.now());
        applier.hook = self.step_hook.take();
        let s = Session {
            _lock: lock,
            config: cfg.clone(),
            roots,
            settings: self.settings.as_ref().map(|(dir, portable)| (dir.clone(), *portable, cfg.clone())),
            writer: None,
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
            seen: Vec::new(),
            notes: Vec::new(),
            warnings,
        };
        Ok((s, moved))
    }

    /// Starts a call that reports warnings, so it may also take in a change of skills folder.
    fn begin(&mut self, cfg: &Config, access: Access) -> Result<Session, EngineError> {
        let (mut s, _) = self.open(cfg, access)?;
        s.recover(cfg)?;
        Ok(s)
    }

    /// Starts a call that changes the records but reports no warnings. If the skills folder changed since the state
    /// was saved, it changes nothing and gives `None`: a check must first take in the change and tell the user.
    fn begin_settled(&mut self, cfg: &Config) -> Result<Option<Session>, EngineError> {
        let (mut s, moved) = self.open(cfg, Access::Write)?;
        if moved {
            s.must_save = false;
            s.suspect = true;
            self.finish(s);
            return Ok(None);
        }
        Ok(Some(s))
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

    /// Writes `last_check` times that were held back. Call before the program exits. If the skills folder changed, the
    /// times belong to records of another folder, so nothing is written.
    pub fn flush(&mut self) -> Result<(), EngineError> {
        if self.lazy.is_empty() {
            return Ok(());
        }
        let Some(mut s) = self.begin_settled(&Config::default())? else { return Ok(()) };
        s.must_save = true;
        self.conclude(s, Ok(()))
    }

    /// Run once when the program starts: sorts out changes a crash interrupted, removes leftovers, and checks the
    /// installed skills against what was recorded.
    pub fn startup(&mut self, cfg: &Config) -> Result<StartupReport, EngineError> {
        let mut s = self.begin(cfg, Access::Write)?;
        s.prune_lane_polls(cfg);
        let problem = if s.applier.skills_dir.is_dir() { apply::preflight(&s.applier.skills_dir, &s.applier.work_dir).err() } else { None };
        if cfg.repos().iter().all(|r| r.kind.is_skills()) {
            s.applier.sweep(&s.state);
        }
        let mut verify = self.verify_in(&mut s, cfg);
        verify.warnings.extend(self.finish(s));
        Ok(StartupReport { verify, problem })
    }

    /// Compares every installed skill with what was recorded. Needs no network.
    pub fn verify(&mut self, cfg: &Config, access: Access) -> Result<VerifyReport, EngineError> {
        let mut s = self.begin(cfg, access)?;
        let mut report = self.verify_in(&mut s, cfg);
        report.warnings.extend(self.finish(s));
        Ok(report)
    }

    fn verify_in(&self, s: &mut Session, cfg: &Config) -> VerifyReport {
        let before = s.stats;
        let mut skills = Vec::new();
        for repo in cfg.repos() {
            let local = match repo.kind {
                SourceKind::Skills => verify_repo(s, &repo.id),
                SourceKind::Agents | SourceKind::Instructions => s.verify_files(repo),
                SourceKind::Plugins => Vec::new(),
            };
            for (name, status) in local {
                skills.push(LocalReport { repo_id: repo.id.clone(), name, status });
            }
        }
        // A lane whose type is off is still configured: its records stay as they are and are never orphans.
        let known = |id: &str| cfg.is_known_lane(id);
        let orphans = s
            .state
            .repos
            .iter()
            .filter(|(id, r)| !known(id) && !r.skills.is_empty())
            .map(|(id, _)| id.clone())
            .chain(s.state.loose_sources().keys().filter(|id| !known(id)).cloned())
            .chain(s.state.plugin_sources().keys().filter(|id| !known(id)).cloned())
            .collect();
        let tracking = s.tracking();
        VerifyReport {
            skills,
            lanes: cfg.repos().iter().map(|repo| repo.id.clone()).collect(),
            orphans,
            stats: since(s.stats, before),
            tracking,
            notes: std::mem::take(&mut s.notes),
            warnings: std::mem::take(&mut s.warnings),
        }
    }

    /// Checks repositories against GitHub and applies what is safe to apply.
    pub fn sync(&mut self, cfg: &Config, which: &Which, opts: &SyncOptions, cancel: &AtomicBool) -> Result<SyncReport, EngineError> {
        let mut s = self.begin(cfg, if opts.dry_run { Access::ReadOnly } else { Access::Write })?;
        if !opts.dry_run {
            s.prune_lane_polls(cfg);
        }
        let result = self.sync_in(&mut s, cfg, which, opts, cancel);
        let notes = std::mem::take(&mut s.notes);
        let seen = std::mem::take(&mut s.seen);
        let mut warnings = self.finish(s);
        if !opts.dry_run && result.is_ok() {
            warnings.extend(self.remember_seen(cfg, &seen));
        }
        result.map(|repos| SyncReport { repos, notes, warnings, seen })
    }

    /// Saves what the complete listings of this call held, only when it differs from what `seen.json` holds. Reads and
    /// writes nothing when no listing was read. A failure is only a warning: the file is for display.
    fn remember_seen(&mut self, cfg: &Config, seen: &[(String, crate::seen::Seen)]) -> Option<String> {
        if seen.is_empty() {
            return None;
        }
        let path = self.paths.seen_file();
        let keep = |all: &mut std::collections::BTreeMap<String, crate::seen::Seen>| {
            all.retain(|id, _| cfg.sources().iter().any(|s| s.id == *id));
            for (id, entry) in seen {
                if all.get(id).is_none_or(|old| old.branch != entry.branch || old.summary != entry.summary) {
                    all.insert(id.clone(), entry.clone());
                }
            }
        };
        // Nothing new against what this engine last read: no file is read or written.
        let held = self.seen.get_or_insert_with(|| crate::seen::load(&path));
        let mut next = held.clone();
        keep(&mut next);
        if next == *held {
            return None;
        }
        match crate::seen::update(&path, keep) {
            Ok(now) => {
                self.seen = now;
                None
            }
            Err(why) => Some(format!("Could not save what the last check found ({why}); the details shown may be older.")),
        }
    }

    fn sync_in(&mut self, s: &mut Session, cfg: &Config, which: &Which, opts: &SyncOptions, cancel: &AtomicBool) -> Result<Vec<RepoReport>, EngineError> {
        let ids: Vec<String> = match which {
            Which::Due => {
                let due: Vec<String> =
                    self.deadlines(cfg, &s.state).filter(|(_, due)| due.is_some_and(|d| d <= self.now())).map(|(id, _)| id.to_owned()).collect();
                // A source is checked as a whole: one of its lanes being due checks every lane that shares its listing.
                cfg.repos()
                    .iter()
                    .filter(|r| due.contains(&r.id) || (r.enabled && cfg.repos().iter().any(|o| o.source == r.source && due.contains(&o.id))))
                    .map(|r| r.id.clone())
                    .collect()
            }
            Which::All => cfg.repos().iter().filter(|r| r.enabled).map(|r| r.id.clone()).collect(),
            Which::One(id) => match (cfg.find(id), cfg.find_source(id)) {
                (Some(r), _) if r.kind == SourceKind::Plugins => vec![r.id.clone()],
                (_, Some(source)) => cfg.repos().iter().filter(|r| r.source == source.id).map(|r| r.id.clone()).collect(),
                _ => return Err(failed(format!("no repository '{id}' is set up"))),
            },
        };
        // Lanes in source order; within a source skills, agents, instructions and plugins share one listing.
        let mut groups: Vec<Vec<&RepoConfig>> = Vec::new();
        for repo in ids.iter().filter_map(|id| cfg.find(id)) {
            match groups.iter_mut().find(|g| g[0].source == repo.source) {
                Some(g) => g.push(repo),
                None => groups.push(vec![repo]),
            }
        }
        let mut reports = Vec::new();
        for mut lanes in groups {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            lanes.sort_by_key(|r| crate::config::KINDS.iter().position(|k| *k == r.kind));
            let before = s.stats;
            let downloaded = s.downloaded;
            let request = plugins::CheckRequest { explicit: *which != Which::Due, choice: None };
            let confirmed = match which {
                Which::One(id) => Some(id.as_str()),
                _ => None,
            };
            let mut done = self.sync_source(s, &lanes, opts, confirmed, request, cancel);
            if let Some(first) = done.first_mut() {
                first.stats = since(s.stats, before);
                first.downloaded = s.downloaded - downloaded;
            }
            for (repo, report) in lanes.iter().zip(done) {
                reports.push(self.record_outcome(s, cfg, repo, report, opts));
            }
        }
        Ok(reports)
    }

    /// Schedules the lane's next check from how this one ended, and saves what changed.
    fn record_outcome(&mut self, s: &mut Session, cfg: &Config, repo: &RepoConfig, mut report: RepoReport, opts: &SyncOptions) -> RepoReport {
        report.finished_at = self.now();
        if opts.dry_run {
            return report;
        }
        let id = &repo.id;
        let interval = repo.effective_interval(cfg.default_interval);
        match &report.outcome {
            Outcome::Failed(f) => {
                let at = self.schedule.on_failure(id, f.kind, f.retry_at, interval, self.now());
                if let Some(rs) = s.state.repos.get_mut(id).filter(|r| r.searches.values().any(|h| !h.paused && !h.complete)) {
                    rs.history_retry_at = Some(at);
                    s.must_save = true;
                }
            }
            Outcome::Cancelled => {}
            _ => {
                self.schedule.on_success(id);
                if let Some(rs) = s.state.repos.get_mut(id) {
                    s.must_save |= rs.history_retry_at.take().is_some();
                }
            }
        }
        if s.must_save {
            // A failed save is recorded as a warning and reported with the rest.
            if let Err(why) = s.save() {
                if repo.kind == SourceKind::Plugins && !matches!(report.outcome, Outcome::Failed(_)) {
                    report = report.fail(RemoteError::new(FailureKind::Local, why));
                }
            }
        }
        report
    }

    /// Lists a repository the way it can be read, and says which token (if any) did it. A repository the user marked
    /// with `use_token` is read with the token only. Any other is read without one first, so a public repository never
    /// makes the app look for the token. When GitHub then says 'not found' (it may be private) or that the limit for
    /// anonymous requests is reached, it is read once more with the token. A repository found to be private this way
    /// is remembered when `learn` is set, so later checks use the token straight away, and forgotten again once it can
    /// be read without one. Whatever fails, the error is returned and nothing is decided from it.
    fn request_tree(
        &self,
        s: Option<&mut Session>,
        repo: &RepoConfig,
        repo_ref: &RepoRef,
        etag: Option<&str>,
        cancel: &AtomicBool,
    ) -> Result<(TreeReply, Option<Arc<Secret>>, bool), RemoteError> {
        self.ask_github(s, repo, cancel, |token| self.remote.tree(repo_ref, etag, token, cancel))
    }

    /// Makes one GitHub request for `repo` with the token rules of `request_tree`.
    fn ask_github<T>(
        &self,
        mut s: Option<&mut Session>,
        repo: &RepoConfig,
        cancel: &AtomicBool,
        call: impl Fn(Option<&Secret>) -> Result<T, RemoteError>,
    ) -> Result<(T, Option<Arc<Secret>>, bool), RemoteError> {
        let tree = |token: Option<&Arc<Secret>>| call(token.map(Arc::as_ref));
        let remembered = s.as_ref().is_some_and(|s| {
            // Every lane of a source reads the same repository, so any lane that learned it is private counts.
            let mut ids = s.config.lane_ids(&repo.id);
            if !ids.contains(&repo.id.as_str()) {
                ids.push(&repo.id);
            }
            ids.iter().any(|id| s.state.repo(id).is_some_and(|r| r.needs_token) || s.state.loose_sources().get(*id).is_some_and(|r| r.poll.needs_token))
                || s.state.plugin_sources().get(&repo.id).is_some_and(|r| r.origin.matches_lane(repo) && r.poll.needs_token)
        });
        let mut token = || match &mut s {
            Some(s) => s.token(self.tokens.as_ref()),
            None => self.tokens.token().map(Arc::new),
        };
        if repo.use_token {
            let token = token().map_err(|why| RemoteError::new(FailureKind::Auth, why))?;
            return Ok((tree(Some(&token))?, Some(token), remembered));
        }
        if remembered {
            if let Ok(token) = token() {
                return Ok((tree(Some(&token))?, Some(token), true));
            }
        }
        match tree(None) {
            Ok(reply) => Ok((reply, None, false)),
            Err(e) if matches!(e.kind, FailureKind::NotFound | FailureKind::RateLimited) && !cancel.load(Ordering::Relaxed) => match token() {
                Ok(token) => {
                    let reply = tree(Some(&token))?;
                    Ok((reply, Some(token), e.kind == FailureKind::NotFound))
                }
                Err(_) if e.kind == FailureKind::NotFound => Err(RemoteError { message: format!("{}. {SIGN_IN}", e.message), ..e }),
                Err(_) => Err(e),
            },
            Err(e) => Err(e),
        }
    }

    /// A skills lane's view of a fresh, unconditional listing of its source, for a choice about one skill.
    fn fresh_skills<'a>(
        &self,
        s: &mut Session,
        repo: &'a RepoConfig,
        cancel: &AtomicBool,
    ) -> Result<(tree::SourceDiscovery<'a>, Option<Arc<Secret>>), RemoteError> {
        let root = self.fresh_root(s, repo, cancel)?;
        let found = tree::lane_discovery(repo, &root.snapshot, &root.inventory).map_err(|why| RemoteError::new(FailureKind::Protocol, why))?;
        Ok((found, root.token))
    }

    fn history_head(&self, s: &mut Session, repo: &RepoConfig, reference: &RepoRef, cancel: &AtomicBool) -> Result<String, RemoteError> {
        let authenticated = repo.use_token || s.state.repo(&repo.id).is_some_and(|r| r.needs_token);
        let token = if authenticated { Some(s.token(self.tokens.as_ref()).map_err(|why| RemoteError::new(FailureKind::Auth, why))?) } else { None };
        match self.remote.commit(reference, token.as_deref(), cancel) {
            Err(e) if token.is_none() && matches!(e.kind, FailureKind::NotFound | FailureKind::RateLimited) => match s.token(self.tokens.as_ref()) {
                Ok(token) => self.remote.commit(reference, Some(&token), cancel),
                Err(_) => Err(e),
            },
            answer => answer,
        }
    }

    /// One skills lane, on its source's listing. `commit` is the branch commit already read for this check, if any.
    fn sync_skills_lane(
        &self,
        s: &mut Session,
        repo: &RepoConfig,
        opts: &SyncOptions,
        cancel: &AtomicBool,
        root: &root::RootListing,
        commit: Option<String>,
    ) -> RepoReport {
        let report = RepoReport::new(repo);
        let repo_ref = match RepoRef::of(repo) {
            Ok(r) => r,
            Err(why) => return report.fail(Failure { kind: FailureKind::Local, message: why, retry_at: None }),
        };
        let source = source_key(repo);
        let same_source = s.state.repo(&repo.id).is_none_or(|r| r.source.is_empty() || r.source == source);
        let mut commit = commit.or_else(|| root.pin.clone());
        let new_etag = root.etag.clone();
        let token = root.token.clone();
        let before_state = s.state.repo(&repo.id).cloned().unwrap_or_default();
        let found = match tree::lane_discovery(repo, &root.snapshot, &root.inventory) {
            Ok(found) => found,
            Err(e) => return report.fail(RemoteError::new(FailureKind::Protocol, e)),
        };
        let tree::SourceItems::Skills { skills, .. } = found.items() else { unreachable!("skills dispatcher") };
        let discovery = Discovery { skills: skills.clone(), rejected: found.rejected().to_vec() };
        let dirs = skills.iter().filter_map(|k| found.remote_path(&k.name).map(|dir| (fold(&k.name), dir.to_owned()))).collect();
        let mut cx = Ctx { repo, repo_ref, token, dirs, opts, cancel, theirs: HashMap::new(), historical: HashMap::new(), tree: found.tree_id().into() };
        // Confirm absence of either a skill or an installed member of a skill before interpreting it. A name the
        // listing still has, synced or not, is never absent.
        let removes = before_state.skills.iter().any(|old| {
            let key = fold(&old.name);
            if crate::reconcile::listed_as(found.rejected(), &old.name).or_else(|| crate::reconcile::listed_as(found.protected(), &old.name)).is_some() {
                return false;
            }
            skills.iter().find(|k| fold(&k.name) == key).is_none_or(|k| old.files.iter().any(|f| !k.files.iter().any(|n| fold(&n.path) == fold(&f.path))))
        });
        if removes {
            if !same_source || !found.complete_for(repo) || before_state.source.is_empty() {
                return report.fail(RemoteError::new(FailureKind::Protocol, "the listing cannot confirm removals for the recorded source"));
            }
            let confirmed = (|| {
                if commit.is_none() {
                    commit = Some(self.remote.commit(&cx.repo_ref, cx.token.as_deref(), cancel)?);
                }
                crate::github::confirm_source_at(self.remote.as_ref(), repo, &found, commit.as_deref().unwrap_or_default(), cx.token.as_deref(), cancel)
            })();
            if let Err(e) = confirmed {
                return report.fail(e);
            }
        }
        let unmanaged = match self.compare_unmanaged(s, &mut cx, &discovery, &mut commit) {
            Ok(u) => u,
            Err(e) if e.kind == FailureKind::Cancelled => return RepoReport { outcome: Outcome::Cancelled, ..report },
            Err(e) => return report.fail(e),
        };
        let local = local_states(s, &repo.id);
        let matching = match self.compare_owned(s, &mut cx, &discovery, &local, &mut commit) {
            Ok(m) => m,
            Err(e) if e.kind == FailureKind::Cancelled => return RepoReport { outcome: Outcome::Cancelled, ..report },
            Err(e) => return report.fail(e),
        };
        let mut decisions = plan_repo(s, repo, &discovery, found.protected(), unmanaged, &local, &matching, opts.confirm_mass_delete);
        // Downloads for positive changes precede removals. A failed transfer must never authorize a deletion later in the plan.
        if removes {
            decisions.sort_by_key(|d| matches!(d.verdict, Verdict::Delete | Verdict::Forget));
        }
        if !opts.dry_run && decisions.iter().any(|d| d.verdict.is_action()) {
            if let Err(e) = s.authorize() {
                return report.fail(e);
            }
        }
        let head = commit.clone();
        let run = self.apply_all(s, &cx, &discovery, &decisions, commit);

        let mut report = report;
        report.rejected = discovery.rejected.clone();
        report.lists_nothing = discovery.skills.is_empty() && discovery.rejected.is_empty() && found.protected().is_empty();
        if !opts.dry_run {
            let settled = run.stopped.is_none()
                && !run.cancelled
                && run.local_failures.is_empty()
                && decisions.iter().zip(&run.results).all(|(d, r)| is_settled(&d.verdict, r));
            let now = self.now();
            let rs = s.state.repo_mut(&repo.id);
            // The ETag is kept only when everything is settled; otherwise the next check must look again.
            rs.etag = if settled { new_etag } else { None };
            rs.lists_nothing = report.lists_nothing;
            rs.source = source;
            rs.history_head = if rs.searches.is_empty() { None } else { head };
            if run.stopped.is_none() && !run.cancelled {
                rs.last_check = Some(now);
            }
            let mut comparable = rs.clone();
            comparable.last_check = before_state.last_check;
            if comparable != before_state {
                s.must_save = true;
            } else if run.stopped.is_none() && !run.cancelled {
                s.touch_check(&repo.id, now);
            }
        }
        report.outcome = classify(&decisions, &run, opts.dry_run);
        report.skills = decisions.into_iter().zip(run.results).map(|(d, result)| SkillReport { name: d.name, verdict: d.verdict, result }).collect();
        report
    }

    /// Compares each folder in the way of a skill that nobody owns with the skill on GitHub, by folded name. A text file
    /// whose bytes differ is read from GitHub to compare its lines. Failing to read it ends the check, like any failure.
    fn compare_unmanaged(
        &self,
        s: &mut Session,
        cx: &mut Ctx,
        discovery: &Discovery,
        commit: &mut Option<String>,
    ) -> Result<HashMap<String, UnmanagedState>, RemoteError> {
        let repo_state = s.state.repo(&cx.repo.id);
        let owned: HashSet<String> = repo_state.map(|r| r.skills.iter().map(|k| fold(&k.name)).collect()).unwrap_or_default();
        let ignored: HashSet<String> = repo_state.map(|r| r.ignored.iter().map(|n| fold(n)).collect()).unwrap_or_default();
        let other_owners = s.state.names_owned_elsewhere(&cx.repo.id);
        let mut out = HashMap::new();
        let mut searches = s.state.repo(&cx.repo.id).map(|r| r.searches.clone()).unwrap_or_default();
        let mut left = HISTORY_SLICE;
        searches.retain(|key, h| {
            (h.paused || discovery.skills.iter().any(|k| fold(&k.name) == *key))
                && (h.paused || !owned.contains(key))
                && !ignored.contains(key)
                && !other_owners.contains_key(key)
        });
        for (key, h) in &searches {
            if h.paused {
                out.insert(key.clone(), UnmanagedState::Paused);
            }
        }
        for skill in &discovery.skills {
            let key = fold(&skill.name);
            let dir = s.applier.skills_dir.join(&skill.name);
            if owned.contains(&key) || ignored.contains(&key) || other_owners.contains_key(&key) {
                continue;
            }
            if searches.get(&key).is_some_and(|h| h.paused) {
                out.insert(key, UnmanagedState::Paused);
                continue;
            }
            if !exists_no_follow(&dir) {
                searches.remove(&key);
                continue;
            }
            let source = source_key(cx.repo);
            let valid = searches.get(&key).is_some_and(|h| h.source == source && h.tree == cx.tree && Some(&h.pin) == commit.as_ref() && evidence_valid(s, h));
            if valid && searches[&key].complete {
                out.insert(key, UnmanagedState::Different);
                continue;
            }
            let vol = s.volume();
            let initial =
                verify::history_evidence(&dir, skill.files.iter().map(|f| f.path.as_str()), vol.current.is_some_and(|v| v.journal != 0), &mut s.applier.hasher);
            if !valid {
                let (same, theirs) = self.compare_version(s, cx, skill, commit)?;
                if same {
                    cx.theirs.insert(key.clone(), theirs);
                    searches.remove(&key);
                    out.insert(key, UnmanagedState::Identical);
                    continue;
                }
                self.pin_listing(cx, commit)?;
                searches.insert(
                    key.clone(),
                    HistorySearch {
                        name: skill.name.clone(),
                        source,
                        tree: cx.tree.clone(),
                        pin: commit.clone().unwrap_or_default(),
                        page: 1,
                        commits: Vec::new(),
                        offset: 0,
                        checked: 0,
                        complete: false,
                        paused: false,
                        volume: vol.current,
                        evidence: initial,
                    },
                );
            }
            let h = searches.get_mut(&key).expect("a search was created");
            let matched = self.search_history(s, cx, h, &mut left)?;
            let state = if let Some(proof) = matched {
                cx.historical.insert(key.clone(), proof);
                UnmanagedState::Historical
            } else if h.complete {
                UnmanagedState::Different
            } else {
                UnmanagedState::Searching
            };
            if !evidence_valid(s, h) {
                // A changing folder supplies no reusable conclusion. It is still never adopted without full reads.
                h.page = 1;
                h.commits.clear();
                h.offset = 0;
                h.checked = 0;
                h.complete = false;
                h.evidence = verify::history_evidence(
                    &dir,
                    skill.files.iter().map(|f| f.path.as_str()),
                    vol.current.is_some_and(|v| v.journal != 0),
                    &mut s.applier.hasher,
                );
                out.insert(key, UnmanagedState::Searching);
            } else {
                out.insert(key, state);
            }
        }
        if s.state.repo(&cx.repo.id).is_none_or(|r| r.searches != searches) {
            s.state.repo_mut(&cx.repo.id).searches = searches;
            s.must_save = true;
        }
        Ok(out)
    }

    fn pin_listing(&self, cx: &Ctx, commit: &mut Option<String>) -> Result<(), RemoteError> {
        if commit.is_none() {
            *commit = Some(self.remote.commit(&cx.repo_ref, cx.token.as_deref(), cx.cancel)?);
        }
        let pinned = self.remote.historical_tree(&cx.repo_ref, commit.as_deref().unwrap_or_default(), "", cx.token.as_deref(), cx.cancel)?;
        if pinned.is_none_or(|t| t.truncated || t.sha != cx.tree) {
            return Err(changed_on_github(&cx.repo.path));
        }
        Ok(())
    }

    fn compare_version(
        &self,
        s: &mut Session,
        cx: &Ctx,
        skill: &tree::Skill,
        commit: &mut Option<String>,
    ) -> Result<(bool, HashMap<String, String>), RemoteError> {
        let dir = s.applier.skills_dir.join(&skill.name);
        let mut theirs = HashMap::new();
        let mut likeness = verify::compare_unmanaged(&dir, skill, &theirs, &mut s.applier.hasher);
        if let Likeness::NeedsText(paths) = &likeness {
            for file in skill.files.iter().filter(|f| paths.contains(&f.path)) {
                if cx.cancel.load(Ordering::Relaxed) {
                    return Err(RemoteError::new(FailureKind::Cancelled, "earlier-version search stopped"));
                }
                theirs.insert(file.path.clone(), self.read_canon(s, cx, skill, file, commit)?);
            }
            likeness = verify::compare_unmanaged(&dir, skill, &theirs, &mut s.applier.hasher);
        }
        Ok((likeness == Likeness::Same, theirs))
    }

    /// Reads one file of a skill from GitHub, within this check's download limit, and returns what its lines count as
    /// (see [`text::canon`]). The file is proved to be the one that was listed. `commit` is asked for when not known yet.
    fn read_canon(&self, s: &mut Session, cx: &Ctx, skill: &tree::Skill, file: &SkillFile, commit: &mut Option<String>) -> Result<String, RemoteError> {
        if s.downloaded.saturating_add(file.size) > cx.opts.max_download_bytes {
            return Err(RemoteError::new(FailureKind::TooLarge, "the comparison reached this check's download limit; it is not a no-match result"));
        }
        if commit.is_none() {
            *commit = Some(self.remote.commit(&cx.repo_ref, cx.token.as_deref(), cx.cancel)?);
        }
        let bytes = download(
            self.remote.as_ref(),
            &cx.repo_ref,
            commit.as_deref().unwrap_or_default(),
            &cx.rel(&skill.name, &file.path),
            file,
            cx.token.as_deref(),
            cx.cancel,
        )?;
        s.downloaded += file.size;
        Ok(text::canon(&bytes))
    }

    /// Finds the skills we own that were edited here, whose upstream moved on, and whose folder is now exactly what
    /// upstream has: the edit reached GitHub too. Only a text file that differs from GitHub's copy in its line breaks
    /// needs a read, one file at a time, so a folder that really differs costs one small read at most. Failing to read
    /// ends the check, like any failure. Going past the download limit only leaves the skill held.
    fn compare_owned(
        &self,
        s: &mut Session,
        cx: &mut Ctx,
        discovery: &Discovery,
        local: &HashMap<String, LocalState>,
        commit: &mut Option<String>,
    ) -> Result<HashSet<String>, RemoteError> {
        let repo_state = s.state.repo(&cx.repo.id);
        let ignored: HashSet<String> = repo_state.map(|r| r.ignored.iter().map(|n| fold(n)).collect()).unwrap_or_default();
        let other_owners = s.state.names_owned_elsewhere(&cx.repo.id);
        let mut matching = HashSet::new();
        for skill in &discovery.skills {
            let key = fold(&skill.name);
            let Some(here) = local.get(&key) else { continue };
            let Some(owned) = s.state.repo(&cx.repo.id).and_then(|r| r.skill(&skill.name)).cloned() else { continue };
            if ignored.contains(&key) || other_owners.contains_key(&key) || !reconcile::needs_comparison(skill, &owned, here) {
                continue;
            }
            match self.compare_with_upstream(s, cx, skill, &owned.files, commit) {
                Ok(Some(theirs)) => {
                    cx.theirs.insert(key.clone(), theirs);
                    matching.insert(key);
                }
                Ok(None) => {}
                Err(e) if e.kind == FailureKind::TooLarge => {}
                Err(e) => return Err(e),
            }
        }
        Ok(matching)
    }

    /// Does the folder of a skill we own have every file upstream has now, and none that upstream dropped? The answer
    /// is the text of the copies on GitHub that the comparison used, by path, and `None` for a folder that differs.
    fn compare_with_upstream(
        &self,
        s: &mut Session,
        cx: &Ctx,
        skill: &tree::Skill,
        recorded: &[ManagedFile],
        commit: &mut Option<String>,
    ) -> Result<Option<HashMap<String, String>>, RemoteError> {
        let dir = s.applier.skills_dir.join(&skill.name);
        let mut theirs = verify::known_canons(skill, recorded);
        loop {
            match verify::compare_owned(&dir, skill, recorded, &theirs, &mut s.applier.hasher) {
                Likeness::Same => return Ok(Some(theirs)),
                Likeness::Different => return Ok(None),
                Likeness::NeedsText(paths) => {
                    let Some(file) = skill.files.iter().find(|f| paths.contains(&f.path) && !theirs.contains_key(&f.path)) else { return Ok(None) };
                    if cx.cancel.load(Ordering::Relaxed) {
                        return Err(RemoteError::new(FailureKind::Cancelled, "the comparison was stopped"));
                    }
                    let canon = self.read_canon(s, cx, skill, file, commit)?;
                    theirs.insert(file.path.clone(), canon);
                }
            }
        }
    }

    fn search_history(&self, s: &mut Session, cx: &Ctx, h: &mut HistorySearch, left: &mut usize) -> Result<Option<HistoricalProof>, RemoteError> {
        let rel = cx.dir(&h.name);
        let rel = rel.as_str();
        while *left > 0 && !h.complete {
            if cx.cancel.load(Ordering::Relaxed) {
                return Err(RemoteError::new(FailureKind::Cancelled, "earlier-version search stopped"));
            }
            if h.offset == h.commits.len() {
                let page = self.remote.history(&cx.repo_ref, &h.pin, rel, h.page, cx.token.as_deref(), cx.cancel)?;
                if page.is_empty() {
                    h.complete = true;
                    break;
                }
                if page.len() > 100 || page.iter().any(|id| id.len() != 40 || !id.bytes().all(|b| b.is_ascii_hexdigit()) || h.commits.contains(id)) {
                    return Err(RemoteError::new(FailureKind::Protocol, "GitHub returned an invalid or repeated history page"));
                }
                h.commits = page;
                h.offset = 0;
                h.page = h.page.checked_add(1).ok_or_else(|| RemoteError::new(FailureKind::Protocol, "history page overflow"))?;
            }
            let id = h.commits[h.offset].clone();
            *left -= 1;
            if let Some(snapshot) = self.remote.historical_tree(&cx.repo_ref, &id, rel, cx.token.as_deref(), cx.cancel)? {
                if snapshot.truncated {
                    return Err(RemoteError::new(FailureKind::TooLarge, "GitHub cut a historical listing short"));
                }
                let found = tree::discover(&snapshot.entries, &h.name, root_len(&s.applier));
                if !found.rejected.is_empty() {
                    return Err(RemoteError::new(FailureKind::Protocol, "an earlier version has unsafe paths or files; it was not used as proof"));
                }
                if tree::is_single_skill(&snapshot.entries) {
                    if let Some(old) = found.skills.into_iter().find(|k| k.name == h.name) {
                        let dir = s.applier.skills_dir.join(&h.name);
                        let quick = s.volume().current.is_some_and(|v| v.journal != 0);
                        let evidence = verify::history_evidence(&dir, old.files.iter().map(|f| f.path.as_str()), quick, &mut s.applier.hasher);
                        for (path, seen) in evidence {
                            if h.evidence.get(&path).is_some_and(|before| *before != seen) {
                                return Err(RemoteError::new(FailureKind::Local, "the local copy changed during the earlier-version search; check again"));
                            }
                            h.evidence.insert(path, seen);
                        }
                        let (same, theirs) = self.compare_version(s, cx, &old, &mut Some(id.clone()))?;
                        if same {
                            return Ok(Some(HistoricalProof { skill: old, commit: id, theirs }));
                        }
                    }
                }
            }
            h.offset += 1;
            h.checked += 1;
            // A short validated page is the end. A full page always asks for the next page, even beyond 100 versions.
            if h.offset == h.commits.len() && h.commits.len() < 100 {
                h.complete = true;
            }
        }
        Ok(None)
    }

    fn apply_all(&self, s: &mut Session, cx: &Ctx, discovery: &Discovery, decisions: &[Decision], mut commit: Option<String>) -> Run {
        let mut run = Run { results: vec![ApplyResult::NotApplicable; decisions.len()], stopped: None, cancelled: false, local_failures: Vec::new() };
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
            if run.cancelled || run.stopped.is_some() || (!run.local_failures.is_empty() && matches!(d.verdict, Verdict::Delete | Verdict::Forget)) {
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
        let result = |applied: Applied, warnings: &mut Vec<String>| match applied {
            Applied::Done => ApplyResult::Done,
            Applied::Displaced { paths, backup } => {
                warnings.push(format!("{}: {}", d.name, apply::displaced_text(&paths, &backup)));
                ApplyResult::Done
            }
            Applied::Held(why) => ApplyResult::Held(why),
        };
        let id = cx.repo.id.as_str();
        match &d.verdict {
            Verdict::Forget => {
                s.applier.forget(&mut s.state, id, &d.name)?;
                Ok(ApplyResult::Done)
            }
            Verdict::CatchUp => {
                let skill = discovery
                    .skills
                    .iter()
                    .find(|k| k.name == d.name)
                    .ok_or_else(|| RemoteError::new(FailureKind::Protocol, format!("'{}' is missing from the listing", d.name)))?;
                if commit.is_none() {
                    *commit = Some(self.remote.commit(&cx.repo_ref, cx.token.as_deref(), cx.cancel)?);
                }
                let theirs = cx.theirs.get(&fold(&skill.name)).cloned().unwrap_or_default();
                let applied = s.applier.catch_up(&mut s.state, id, skill, commit.as_deref().unwrap_or_default(), &theirs)?;
                Ok(result(applied, &mut s.warnings))
            }
            Verdict::Delete => {
                s.ensure_preflight()?;
                let applied = s.applier.delete(&mut s.state, id, &d.name)?;
                Ok(result(applied, &mut s.warnings))
            }
            Verdict::Install | Verdict::Update | Verdict::Adopt | Verdict::AdoptHistorical => {
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
                if d.verdict == Verdict::AdoptHistorical {
                    let proof = cx
                        .historical
                        .get(&fold(&skill.name))
                        .ok_or_else(|| RemoteError::new(FailureKind::Local, "historical adoption has no full-version proof"))?;
                    let adopted = s.applier.adopt(&mut s.state, id, &proof.skill, &proof.commit, &proof.theirs)?;
                    if let Applied::Held(why) = adopted {
                        return Ok(ApplyResult::Held(why));
                    }
                    s.must_save = true;
                }
                if !downloads {
                    let theirs = cx.theirs.get(&fold(&skill.name)).cloned().unwrap_or_default();
                    let applied = s.applier.adopt(&mut s.state, id, skill, commit, &theirs)?;
                    return Ok(result(applied, &mut s.warnings));
                }
                let bytes = Cell::new(0u64);
                let mut fetch = |file: &SkillFile, sink: &mut dyn FnMut(&[u8]) -> bool| -> Result<(), RemoteError> {
                    let rel = cx.rel(&skill.name, &file.path);
                    self.remote
                        .file(&cx.repo_ref, commit, &rel, file.size, cx.token.as_deref(), cx.cancel, &mut |chunk: &[u8]| {
                            bytes.set(bytes.get() + chunk.len() as u64);
                            sink(chunk)
                        })
                        .map_err(|e| missing_means_changed(e, &file.path))
                };
                let applied = s.applier.install(&mut s.state, id, skill, commit, Mode::Normal, &mut fetch, cx.cancel);
                s.downloaded += bytes.get();
                let result = result(applied?, &mut s.warnings);
                if result == ApplyResult::Done && s.state.repo_mut(id).searches.remove(&fold(&skill.name)).is_some() {
                    s.must_save = true;
                }
                Ok(result)
            }
            _ => Ok(ApplyResult::NotApplicable),
        }
    }

    /// Settles one skill that needs the user's decision.
    pub fn resolve(&mut self, cfg: &Config, repo_id: &str, skill: &str, how: Resolution, cancel: &AtomicBool) -> Result<ResolveReport, EngineError> {
        let repo = cfg.find(repo_id).ok_or_else(|| failed(format!("no repository '{repo_id}' is set up")))?;
        if repo.kind == SourceKind::Plugins {
            return Err(failed("Use a plugin choice to resolve a Copilot plugin."));
        }
        pathsafe::check_component(skill).map_err(|why| failed(why.to_string()))?;
        let mut s = self.begin_settled(cfg)?.ok_or_else(folder_changed)?;
        let result = if repo.kind.is_skills() {
            s.authorize().map_err(|e| failed(e.message)).and_then(|()| {
                if s.state.repo(repo_id).and_then(|r| r.skill(skill)).is_some_and(|k| k.pending.is_some()) {
                    return Err(failed("finish the interrupted change before resolving this skill"));
                }
                self.resolve_in(&mut s, repo, skill, how, cancel)
            })
        } else {
            self.resolve_file(&mut s, repo, skill, how, cancel).map_err(|e| failed(e.message))
        };
        if result.is_ok() && !repo.kind.is_skills() && how == Resolution::Overwrite {
            self.file_review = None;
        }
        self.conclude(s, result)
    }

    fn resolve_in(&self, s: &mut Session, repo: &RepoConfig, skill: &str, how: Resolution, cancel: &AtomicBool) -> Result<ResolveReport, EngineError> {
        let key = fold(skill);
        match how {
            Resolution::PauseHistory | Resolution::ResumeHistory => {
                let rs = s.state.repo_mut(&repo.id);
                if how == Resolution::PauseHistory && !rs.searches.contains_key(&key) {
                    return Ok(ResolveReport { message: format!("No earlier-version search remains for '{skill}'; nothing was changed"), backups: Vec::new() });
                }
                let search = rs.searches.get_mut(&key).ok_or_else(|| failed(format!("'{skill}' has no earlier-version search")))?;
                search.paused = how == Resolution::PauseHistory;
                rs.etag = None;
                s.must_save = true;
                let action = if how == Resolution::PauseHistory { "paused until you choose Resume" } else { "resumed; the next check continues it" };
                Ok(ResolveReport { message: format!("Earlier-version search for '{skill}' {action}"), backups: Vec::new() })
            }
            Resolution::KeepMine => {
                let rs = s.state.repo_mut(&repo.id);
                rs.searches.remove(&key);
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
                rs.searches.remove(&key);
                rs.ignored.retain(|n| fold(n) != key);
                rs.etag = None;
                s.must_save = true;
                Ok(ResolveReport { message: format!("'{skill}' will be handled like any other skill at the next check"), backups: Vec::new() })
            }
            Resolution::Overwrite => {
                let review = self.skill_review.as_ref().filter(|r| r.source.id == repo.id && fold(&r.name) == key);
                if review.is_none() {
                    if let Some(search) = s.state.repo_mut(&repo.id).searches.get_mut(&key) {
                        search.paused = true;
                        s.must_save = true;
                    }
                }
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
                let (found, token) = self.fresh_skills(s, repo, cancel).map_err(|f| failed(f.message))?;
                let tree::SourceItems::Skills { skills, .. } = found.items() else { unreachable!("skills lane") };
                let upstream = skills.iter().find(|k| fold(&k.name) == key);
                let dir = upstream.and_then(|k| found.remote_path(&k.name)).unwrap_or(skill).to_owned();
                let single = dir.is_empty();
                if let Some(r) = review {
                    let recorded = s.state.repo(&repo.id).and_then(|state| state.skill(skill));
                    let paths = wanted_here(upstream.map_or(&[][..], |k| k.files.as_slice()), recorded);
                    if r.source != *repo
                        || r.dir != s.applier.skills_dir.join(skill)
                        || r.single != single
                        || r.desired.as_ref() != upstream
                        || !r.local.files.iter().map(|(p, _)| p).eq(&paths)
                    {
                        return Err(failed(apply::STALE_REVIEW));
                    }
                    if r.local.files.iter().any(|(_, file)| matches!(file, verify::ReviewFile::Unreadable)) {
                        return Err(failed("The comparison could not verify every local file. No files were replaced."));
                    }
                }
                let Some(upstream) = upstream else {
                    let why =
                        found.rejected().iter().chain(found.protected()).find(|r| fold(&r.name) == key).map(|r| format!(" ({})", r.reason)).unwrap_or_default();
                    return Err(failed(format!("'{skill}' cannot be installed from the repository{why}")));
                };
                s.ensure_preflight().map_err(|e| failed(e.message))?;
                let commit = self.remote.commit(&repo_ref, token.as_deref(), cancel).map_err(|e| failed(e.message))?;
                let backups_before = backups_in(&s.applier.work_dir);
                let bytes = Cell::new(0u64);
                let mut fetch = |file: &SkillFile, sink: &mut dyn FnMut(&[u8]) -> bool| -> Result<(), RemoteError> {
                    let rel = if single { file.path.clone() } else { format!("{dir}/{}", file.path) };
                    self.remote
                        .file(&repo_ref, &commit, &rel, file.size, token.as_deref(), cancel, &mut |chunk: &[u8]| {
                            bytes.set(bytes.get() + chunk.len() as u64);
                            sink(chunk)
                        })
                        .map_err(|e| missing_means_changed(e, &file.path))
                };
                let applied =
                    s.applier.install_with_review(&mut s.state, &repo.id, upstream, &commit, Mode::Overwrite, &mut fetch, cancel, review.map(|r| &r.local));
                s.downloaded += bytes.get();
                let applied = applied.inspect_err(|_| s.suspect = true).map_err(|e| failed(e.message))?;
                let backups: Vec<PathBuf> = backups_in(&s.applier.work_dir).into_iter().filter(|b| !backups_before.contains(b)).collect();
                match applied {
                    // An overwrite never reports what was in the way: every backup is reported below.
                    Applied::Done | Applied::Displaced { .. } => {
                        s.state.repo_mut(&repo.id).searches.remove(&key);
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

    /// What is different between GitHub and the copy on this PC. Writes nothing, but retains
    /// enough evidence in memory to check a later overwrite choice.
    pub fn diff(&mut self, cfg: &Config, repo_id: &str, skill: &str, cancel: &AtomicBool) -> Result<DiffReport, EngineError> {
        let repo = cfg.find(repo_id).ok_or_else(|| failed(format!("no repository '{repo_id}' is set up")))?;
        if repo.kind == SourceKind::Plugins {
            return Err(failed("Copilot plugins cannot be compared as local files."));
        }
        pathsafe::check_component(skill).map_err(|e| failed(format!("'{skill}' is not a skill name ({e})")))?;
        let mut s = self.begin(cfg, Access::ReadOnly)?;
        let result = if repo.kind.is_skills() {
            self.diff_in(&mut s, repo, skill, cancel).map(|(report, review)| {
                self.skill_review = Some(review);
                report
            })
        } else {
            self.view_file(&mut s, repo, skill, false, cancel)
                .map(|(report, review)| {
                    self.file_review = review;
                    report
                })
                .map_err(|e| failed(e.message))
        };
        self.finish(s);
        result
    }

    fn diff_in(&self, s: &mut Session, repo: &RepoConfig, skill: &str, cancel: &AtomicBool) -> Result<(DiffReport, SkillComparison), EngineError> {
        let problems = repo.problems();
        if !problems.is_empty() {
            return Err(failed(problems.join("; ")));
        }
        let repo_ref = RepoRef::of(repo).map_err(failed)?;
        let dir = s.applier.skills_dir.join(skill);
        let here = here(&dir)?;
        let (found, token) = self.fresh_skills(s, repo, cancel).map_err(|f| failed(f.message))?;
        let tree::SourceItems::Skills { skills, .. } = found.items() else { unreachable!("skills lane") };
        let key = fold(skill);
        let upstream = skills.iter().find(|k| fold(&k.name) == key);
        let remote_dir = upstream.and_then(|k| found.remote_path(&k.name)).unwrap_or(skill).to_owned();
        let single = remote_dir.is_empty();
        let files = upstream.map_or(&[][..], |k| k.files.as_slice());
        let by_name: HashMap<String, &SkillFile> = files.iter().map(|f| (fold(&f.path), f)).collect();

        let recorded = s.state.repo(&repo.id).and_then(|r| r.skill(skill));
        let paths = wanted_here(files, recorded);
        let (ours, same, evidence) = if here {
            read_here(&dir, paths, &by_name, cancel)?
        } else {
            (Vec::new(), HashSet::new(), paths.into_iter().map(|p| (p, verify::ReviewFile::Missing)).collect())
        };
        let mut theirs = Vec::new();
        let mut commit: Option<String> = None;
        let mut budget = DIFF_TOTAL_BYTES;
        // A file not read on this PC is shown only with the reason, so its copy on GitHub is not downloaded either.
        let unread: HashMap<String, &str> =
            ours.iter().filter_map(|(path, content)| if let Content::Hidden(why) = content { Some((fold(path), why.as_str())) } else { None }).collect();
        for file in files.iter().filter(|f| !same.contains(&fold(&f.path))) {
            let content = if let Some(why) = unread.get(&fold(&file.path)) {
                Content::Hidden(why.to_string())
            } else if file.size > DIFF_FILE_BYTES {
                Content::Hidden(TOO_BIG.into())
            } else if file.size > budget {
                Content::Hidden(TOO_MUCH.into())
            } else {
                budget -= file.size;
                if commit.is_none() {
                    commit = Some(self.remote.commit(&repo_ref, token.as_deref(), cancel).map_err(|e| failed(e.message))?);
                }
                let rel = if single { file.path.clone() } else { format!("{remote_dir}/{}", file.path) };
                let bytes = download(self.remote.as_ref(), &repo_ref, commit.as_deref().unwrap_or_default(), &rel, file, token.as_deref(), cancel)
                    .map_err(|e| failed(e.message))?;
                Content::Bytes(bytes)
            };
            theirs.push((file.path.clone(), content));
        }
        let review = SkillComparison {
            source: repo.clone(),
            dir,
            name: skill.into(),
            single,
            desired: upstream.cloned(),
            local: verify::SkillReview { here, files: evidence },
        };
        Ok((DiffReport { on_github: upstream.is_some(), here, files: diff::compare(theirs, ours) }, review))
    }

    /// Preview: the files GitHub put in a skill, as they are on this PC. Only the files the record lists are read, so a
    /// file the user added is never opened. Asks GitHub nothing and changes nothing.
    pub fn preview(&mut self, cfg: &Config, repo_id: &str, skill: &str, cancel: &AtomicBool) -> Result<DiffReport, EngineError> {
        let repo = cfg.find(repo_id).ok_or_else(|| failed(format!("no repository '{repo_id}' is set up")))?;
        if repo.kind == SourceKind::Plugins {
            return Err(failed("Copilot plugins cannot be opened as local files."));
        }
        pathsafe::check_component(skill).map_err(|e| failed(format!("'{skill}' is not a skill name ({e})")))?;
        let mut s = self.begin(cfg, Access::ReadOnly)?;
        let result = if repo.kind.is_skills() {
            preview_in(&s, repo_id, skill, cancel)
        } else {
            self.view_file(&mut s, repo, skill, true, cancel).map(|(report, _)| report).map_err(|e| failed(e.message))
        };
        self.finish(s);
        result
    }

    /// Stops managing a repository's skills. The caller removes the repository from the settings afterwards.
    pub fn remove_repo(&mut self, repo_id: &str, delete_skills: bool) -> Result<RemoveReport, EngineError> {
        self.remove_source(&Config::default(), repo_id, delete_skills)
    }

    pub fn remove_source(&mut self, cfg: &Config, repo_id: &str, delete_skills: bool) -> Result<RemoveReport, EngineError> {
        self.remove_source_in(cfg, repo_id, delete_skills, None)
    }

    fn remove_source_in(&mut self, cfg: &Config, repo_id: &str, delete_skills: bool, uninstall: Option<&AtomicBool>) -> Result<RemoveReport, EngineError> {
        let mut s = self.begin_settled(cfg)?.ok_or_else(folder_changed)?;
        let active = cfg.find(repo_id);
        let is_plugin = active.is_some_and(|r| r.kind == SourceKind::Plugins) || s.state.plugin_sources().contains_key(repo_id);
        if is_plugin || uninstall.is_some() {
            let result = if !is_plugin {
                Err(failed("--uninstall-plugins is only for a Plugins source"))
            } else if delete_skills {
                Err(failed("--delete-skills is only for a Skills source"))
            } else {
                self.remove_plugins_in(&mut s, active, repo_id, uninstall)
            };
            if result.is_ok() {
                self.schedule.forget(repo_id);
                self.plugin_attention_rows.remove(repo_id);
                s.must_save |= s.state.forget_lane_poll(repo_id);
                s.lazy.remove(repo_id);
            }
            return self.conclude(s, result);
        }
        s.authorize().map_err(|e| failed(e.message))?;
        let mut report = RemoveReport::default();
        if let Some(record) = s.state.loose_sources().get(repo_id) {
            if delete_skills {
                return Err(failed("--delete-skills is only for a Skills source"));
            }
            let recorded = record.recorded_source(repo_id);
            let source = active.or(recorded.as_ref()).ok_or_else(|| failed("cannot verify the detached source identity"))?;
            report.kept.extend(record.leaves().iter().map(|l| (l.name.clone(), "kept as you asked".into())));
            s.state.forget_loose_source(source).map_err(failed)?;
            s.must_save = true;
        } else if active.is_some_and(|r| !r.kind.is_skills()) {
            if delete_skills {
                return Err(failed("--delete-skills is only for a Skills source"));
            }
            // A saved source can legitimately have no binding before its first check.
        } else {
            let names: Vec<String> = s.state.repo(repo_id).map(|r| r.skills.iter().map(|k| k.name.clone()).collect()).unwrap_or_default();
            for name in names {
                if !delete_skills {
                    report.kept.push((name, "kept as you asked".to_string()));
                    continue;
                }
                let outcome = s.ensure_preflight().and_then(|()| s.applier.delete(&mut s.state, repo_id, &name));
                match outcome {
                    Ok(Applied::Done | Applied::Displaced { .. }) => report.deleted.push(name),
                    Ok(Applied::Held(why)) => report.kept.push((name, why)),
                    Err(e) => {
                        s.suspect = true;
                        report.kept.push((name, e.message));
                    }
                }
            }
            if s.state.drop_repo(repo_id) {
                s.must_save = true;
            }
        }
        self.schedule.forget(repo_id);
        self.lazy.remove(repo_id);
        s.lazy.remove(repo_id);
        s.must_save |= s.state.forget_lane_poll(repo_id);
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
        if repo.searches.get(&fold(&skill.name)).is_some_and(|h| h.paused) {
            out.push((skill.name.clone(), LocalStatus::History { paused: true, complete: false, waiting: false }));
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
    for search in repo.searches.values().filter(|h| repo.skill(&h.name).is_none() && !repo.is_ignored(&h.name)) {
        out.push((search.name.clone(), LocalStatus::History { paused: search.paused, complete: search.complete, waiting: repo.history_retry_at.is_some() }));
    }
    out
}

/// What the folder of each owned skill is like, by folded name. Interrupted skills, and those whose earlier-version
/// search is paused, are left out.
fn local_states(s: &mut Session, repo_id: &str) -> HashMap<String, LocalState> {
    verify_repo(s, repo_id)
        .into_iter()
        .filter_map(|(name, status)| match status {
            LocalStatus::Checked(local) => Some((fold(&name), local)),
            LocalStatus::Interrupted | LocalStatus::History { .. } | LocalStatus::Unavailable(_) => None,
        })
        .collect()
}

/// Turns what is upstream, what is recorded and what is on disk into decisions. `unmanaged` says how each folder in
/// the way of a skill nobody owns compares with it, by folded name. `local` says what the folder of each skill we own
/// is like, and `matching` which of the edited ones are now what upstream has, also by folded name.
#[allow(clippy::too_many_arguments)]
fn plan_repo(
    s: &Session,
    repo: &RepoConfig,
    discovery: &Discovery,
    protected: &[Rejected],
    unmanaged: HashMap<String, UnmanagedState>,
    local: &HashMap<String, LocalState>,
    matching: &HashSet<String>,
    confirm_mass_delete: bool,
) -> Vec<Decision> {
    let repo_state = s.state.repo(&repo.id);
    let owned: Vec<OwnedSkill> = repo_state.map(|r| r.skills.clone()).unwrap_or_default();
    let ignored: HashSet<String> = repo_state.map(|r| r.ignored.iter().map(|n| fold(n)).collect()).unwrap_or_default();
    let interrupted: Vec<String> = owned.iter().filter(|o| o.pending.is_some()).map(|o| o.name.clone()).collect();
    let other_owners = s.state.names_owned_elsewhere(&repo.id);

    let not_checked = || LocalState::Dirty(DirtyInfo { modified: vec!["(not checked)".to_string()], ..DirtyInfo::default() });
    let views: Vec<OwnedView> = owned
        .iter()
        .filter(|o| o.pending.is_none())
        .map(|o| {
            let key = fold(&o.name);
            OwnedView { skill: o, local: local.get(&key).cloned().unwrap_or_else(not_checked), matches_upstream: matching.contains(&key) }
        })
        .collect();
    reconcile::plan(&Input {
        desired: &discovery.skills,
        rejected: &discovery.rejected,
        protected,
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
        Verdict::InSync
        | Verdict::ModifiedLocally(_)
        | Verdict::MissingLocally
        | Verdict::Skipped(Skip::Ignored)
        | Verdict::Skipped(Skip::Rejected { .. })
        | Verdict::Skipped(Skip::Protected { .. }) => true,
        Verdict::Install | Verdict::Update | Verdict::Adopt | Verdict::AdoptHistorical | Verdict::CatchUp | Verdict::Delete | Verdict::Forget => {
            *result == ApplyResult::Done
        }
        Verdict::Held(reconcile::Hold::UnmanagedDiffers | reconcile::Hold::HistoryPaused) => true,
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

fn changed_on_github(path: &str) -> RemoteError {
    RemoteError::new(FailureKind::Changed, format!("the repository changed on GitHub while {path} was being read; try again"))
}

/// A file the listing named that the commit being read does not have: a push landed between the two reads, or GitHub
/// answered them from caches of different ages. It is never a missing or private repository.
fn missing_means_changed(e: RemoteError, path: &str) -> RemoteError {
    if e.kind == FailureKind::NotFound { changed_on_github(path) } else { e }
}

/// Downloads one file into memory and proves it is the one that was listed.
fn download(
    remote: &dyn Remote,
    repo_ref: &RepoRef,
    commit: &str,
    rel: &str,
    file: &SkillFile,
    token: Option<&Secret>,
    cancel: &AtomicBool,
) -> Result<Vec<u8>, RemoteError> {
    let mut bytes = Vec::with_capacity(file.size as usize);
    let mut over = false;
    let got = remote.file(repo_ref, commit, rel, file.size, token, cancel, &mut |chunk| {
        over |= (bytes.len() + chunk.len()) as u64 > file.size;
        if !over {
            bytes.extend_from_slice(chunk);
        }
        !over
    });
    let changed = || changed_on_github(&file.path);
    if over {
        return Err(changed());
    }
    got.map_err(|e| missing_means_changed(e, &file.path))?;
    if blob_hex(&bytes) != file.blob {
        return Err(changed());
    }
    Ok(bytes)
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
