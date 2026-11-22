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
//! * a timer that fires much later than it was set for (the computer slept) waits 30 seconds for the network;
//! * started by Windows at sign-in, settings load on the worker, but the first look at the disk and the first check
//!   wait 30 seconds, so that Windows can finish starting and the network can come up, and then run as background
//!   work. The user coming (the window, or a job they ask for) starts the look at once.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::board::{Board, Health, Level, Row, SourceView, capitalized, preview_source_view, previewed_plugins, recorded_plugin_rows, source_label};
use crate::config::{Config, Paths};
use crate::engine::{
    Access, DiffReport, Engine, EngineError, LocationPreview, Outcome, RemoveReport, RepoReport, Resolution, ResolveReport, SkillFacts, Stamp, StartupReport,
    SyncOptions, SyncReport, VerifyReport, Which,
};
use crate::interval::{Interval, UnixSecs};
use crate::location::{Location, Target};
use crate::plugins::Choice;
use crate::replacement::VerifiedTray;
use crate::source::SourceKind;
use crate::sources::{GlobalSettings, SourceConfig};
use crate::state::State;
use crate::update_clock::ClockState;
use crate::updater::{Action, Locations, Request, Updates};

/// How long to wait before trying again when another check held the engine.
pub const BUSY_RETRY: UnixSecs = 60;
/// The least time between two checks the timer starts.
pub const MIN_POLL_GAP: UnixSecs = 30;
/// After waking from sleep, the network gets this long to come back before checking.
pub const RESUME_GRACE: UnixSecs = 30;
/// Started by Windows at sign-in, the network gets this long to come up before the first check.
pub const SIGN_IN_GRACE: UnixSecs = RESUME_GRACE;
/// A timer that fires this much later than it was set for means the computer slept.
pub const LATE_TIMER: UnixSecs = 60;
/// The first wait after the engine could not work at all. It doubles with every failure in a row.
pub const FAILURE_BACKOFF: UnixSecs = 5 * 60;
pub const MAX_FAILURE_BACKOFF: UnixSecs = 60 * 60;
const KINDS: [SourceKind; 4] = [SourceKind::Skills, SourceKind::Agents, SourceKind::Instructions, SourceKind::Plugins];

#[derive(Clone, Debug, PartialEq)]
pub enum Job {
    /// Read settings on the worker before the first look at skills or GitHub.
    LoadSettings,
    /// The first job, and the first for another skills folder: look at the disk and find out whether the skills folder
    /// can be used.
    Startup,
    /// Something changed on disk (in the skills folder, or `state.json` by `tss`): look again, change nothing.
    Look,
    /// The timer: check the repositories whose time has come. Never a look at the disk first.
    Poll,
    /// The user asked: look at the disk, then check every enabled repository (`None`) or one.
    Check(Option<String>),
    PreviewSource(Arc<DraftInput>),
    /// Save the reviewed location; with `true`, approve the reviewed plugins after the save.
    SaveSource(Arc<DraftInput>, Arc<PreviewSnapshot>, Box<SourceConfig>, bool),
    AppUpdate(Action),
    ScheduledUpdate,
    /// Only after receiving a successful Flush during an explicit Quit, not on logoff.
    InstallOnQuit {
        before: Instant,
    },
    Resolve {
        repo: String,
        skill: String,
        how: Resolution,
    },
    ResolvePlugin {
        repo: String,
        choice: Choice,
    },
    /// Upstream lists no skills at all, and the user confirmed that the ones gone from it are deleted.
    DeleteGone(String),
    /// Stop managing a whole source (given its source ID or any lane ID) before it leaves the settings, deleting the
    /// unedited skills if asked.
    Remove {
        repo: String,
        delete_skills: bool,
    },
    /// "Show changes": what is different between a skill on GitHub and the copy on this PC; or, with `preview`, the files
    /// GitHub put in it as they are on this PC. Changes nothing.
    Diff {
        repo: String,
        skill: String,
        preview: bool,
    },
    /// Quitting: save what is waiting to be saved.
    Flush,
    /// The settings changed: work out when to wake up next. Reads nothing.
    Plan,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DraftInput {
    pub ticket: u64,
    /// A preview request identity: settings may be changed back to earlier values while an older request is in flight.
    pub request: u64,
    /// The location as the user typed it.
    pub text: String,
    /// An immutable snapshot of the discovery choices for this request.
    pub settings: SourceConfig,
}

#[derive(Clone, Debug)]
pub struct SourceDraft {
    pub input: Arc<DraftInput>,
    /// Editable choices stay separate from the remote facts being revalidated.
    pub settings: SourceConfig,
    /// The last result, retained only as stale content while refreshing or after a failure.
    pub previous_preview: Option<Arc<PreviewSnapshot>>,
    pub stage: DraftStage,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PreviewSnapshot {
    location: LocationPreview,
    view: SourceView,
}

impl PreviewSnapshot {
    pub fn new(location: LocationPreview) -> PreviewSnapshot {
        let view = preview_source_view(&location);
        PreviewSnapshot { location, view }
    }

    pub fn location(&self) -> &LocationPreview {
        &self.location
    }

    pub fn view(&self) -> &SourceView {
        &self.view
    }
}

#[derive(Clone, Debug)]
pub enum DraftStage {
    Loading,
    Reviewed(Arc<PreviewSnapshot>),
    /// The location is already a source, with this ID.
    AlreadySetUp(String),
    Failed(String),
    Saving,
}

pub(crate) fn duplicate_source_message(id: &str) -> String {
    format!("This source is already set up. Its ID is '{id}'.")
}

/// The source already saved for the same repository (in any case), branch and folder.
pub(crate) fn same_location<'a>(cfg: &'a Config, repo: &str, branch: &str, path: &str) -> Option<&'a SourceConfig> {
    cfg.sources().iter().find(|s| s.repo.eq_ignore_ascii_case(repo) && s.branch == branch && s.path == path)
}

fn already_added(cfg: &Config, preview: &LocationPreview) -> Option<String> {
    let s = &preview.source;
    same_location(cfg, &s.repo, &s.branch, &s.path).map(|old| old.id.clone())
}

fn preview_with_settings(preview: &LocationPreview, settings: &SourceConfig, global: &GlobalSettings) -> LocationPreview {
    let mut preview = preview.clone();
    preview.source.interval = settings.interval;
    preview.source.enabled = settings.enabled;
    preview.source.types = settings.types;
    preview.source.include_repo_items = settings.include_repo_items;
    preview.source.ignore_add.clone_from(&settings.ignore_add);
    preview.source.ignore_allow.clone_from(&settings.ignore_allow);
    preview.types = preview.source.effective_types(global);
    preview
}

fn generated_settings(preview: &LocationPreview, settings: &SourceConfig) -> SourceConfig {
    let mut generated = preview.source.clone();
    generated.interval = settings.interval;
    generated.enabled = settings.enabled;
    generated.types = settings.types;
    generated.include_repo_items = settings.include_repo_items;
    generated.ignore_add.clone_from(&settings.ignore_add);
    generated.ignore_allow.clone_from(&settings.ignore_allow);
    generated
}

pub(crate) fn preview_syncs_for_settings(preview: &PreviewSnapshot, settings: &SourceConfig, global: &GlobalSettings, kind: SourceKind) -> bool {
    if !settings.effective_types(global).enabled(kind) {
        return false;
    }
    preview.view().kinds.iter().find(|view| view.kind == kind).is_some_and(|view| view.found.unwrap_or(0) > 0)
}

/// Save a reviewed location as a new source, after proving that what GitHub has, where it would go and the settings
/// are all as they were when the user looked. Plugins are approved only after the save, and only the reviewed ones.
#[allow(clippy::too_many_arguments)]
fn save_source(
    engine: &mut Engine,
    cfg: &mut Config,
    store: Option<&SettingsSource>,
    (portable_mode, skills_dir): (bool, &PathBuf),
    input: &DraftInput,
    reviewed: &PreviewSnapshot,
    settings: &SourceConfig,
    approve_plugins: bool,
    cancel: &AtomicBool,
) -> Result<Answer, EngineError> {
    let reviewed_location = preview_with_settings(reviewed.location(), settings, cfg.global());
    if let Some(id) = already_added(cfg, &reviewed_location) {
        return Err(EngineError::Failed(duplicate_source_message(&id)));
    }
    if engine.preview_location_with_settings(cfg, &input.text, false, settings, cancel)? != reviewed_location {
        return Err(EngineError::Failed("What GitHub has, or where it would go, changed since the preview. Preview the source again.".into()));
    }
    // A record left on this PC under one of the new lane IDs would be taken over by the new source.
    for id in KINDS.into_iter().filter_map(|kind| reviewed_location.source.lanes.id(kind)) {
        engine.check_fresh_id(id)?;
    }
    let store = store.ok_or_else(|| EngineError::Failed("The settings folder is unavailable.".into()))?;
    let active = store.load(portable_mode, true, true).map_err(EngineError::Failed)?;
    if !active.is_some_and(|s| s.config == *cfg && s.portable == portable_mode && s.skills_dir == *skills_dir) {
        return Err(EngineError::Failed("The settings changed. Preview the source again.".into()));
    }
    let source = reviewed_location.source.clone();
    let mut after = cfg.clone();
    after.add_source_config(source.clone()).map_err(EngineError::Failed)?;
    if cancel.load(Ordering::Acquire) {
        return Err(EngineError::Failed("Adding the source was cancelled.".into()));
    }
    store.save(cfg, &after, portable_mode).map_err(EngineError::Failed)?;
    let approved = if approve_plugins && !previewed_plugins(&reviewed_location).is_empty() {
        engine
            .approve_location_plugins(&after, &source.id, &reviewed_location)
            .map_err(|e| format!("The source was saved, but its plugins could not be approved: {e}"))
    } else {
        Ok(())
    };
    *cfg = after;
    Ok(Answer::SourceSaved { source, approved })
}

/// The lanes of a source to let go of: every type that syncs, and every type that is off but still has items on record.
pub(crate) fn removal_lanes(engine: &Engine, cfg: &Config, source: &SourceConfig) -> Result<Vec<(String, SourceKind)>, String> {
    let file = engine.paths().state_file();
    let state = State::load_sources_with(&file, false).map(|loaded| loaded.state).map_err(|e| format!("cannot read {}: {e}", file.display()))?;
    let recorded = |id: &str| state.source_ids().any(|s| s == id);
    Ok(KINDS.into_iter().filter_map(|kind| source.lanes.id(kind).filter(|id| cfg.find(id).is_some() || recorded(id)).map(|id| (id.to_owned(), kind))).collect())
}

/// Let go of a whole source, lane by lane. Plugins go first: when an uninstall fails, nothing else has been let go of
/// and the source stays. `delete_skills` applies to the skills lane only. The caller removes the source from the
/// settings afterwards.
pub(crate) fn remove_whole_source(
    engine: &mut Engine,
    cfg: &Config,
    lanes: &[(String, SourceKind)],
    delete_skills: bool,
    uninstall_plugins: bool,
) -> Result<Vec<(String, SourceKind, RemoveReport)>, EngineError> {
    let plugins_first = lanes.iter().filter(|(_, k)| *k == SourceKind::Plugins).chain(lanes.iter().filter(|(_, k)| *k != SourceKind::Plugins));
    let mut reports = Vec::new();
    for (id, kind) in plugins_first {
        let report = if *kind == SourceKind::Plugins && uninstall_plugins {
            engine.remove_source_with_plugins(cfg, id, &AtomicBool::new(false))?
        } else {
            remove_source(engine, cfg, id, delete_skills && *kind == SourceKind::Skills)?
        };
        reports.push((id.clone(), *kind, report));
    }
    Ok(reports)
}

pub(crate) fn remove_source(engine: &mut Engine, cfg: &Config, id: &str, delete_skills: bool) -> Result<RemoveReport, EngineError> {
    if let Some(source) = cfg.find(id) {
        if delete_skills && source.kind != SourceKind::Skills {
            return Err(EngineError::Failed("--delete-skills is only for a Skills source. Remove this source without it to keep its files.".into()));
        }
    }
    engine.remove_source(cfg, id, delete_skills)
}

impl Job {
    pub fn shows_work(&self) -> bool {
        !matches!(self, Job::LoadSettings | Job::Look | Job::Flush | Job::Plan | Job::Diff { .. } | Job::PreviewSource(_) | Job::SaveSource(..))
    }

    fn target(&self) -> Option<&str> {
        match self {
            Job::Check(Some(id))
            | Job::DeleteGone(id)
            | Job::Resolve { repo: id, .. }
            | Job::ResolvePlugin { repo: id, .. }
            | Job::Remove { repo: id, .. }
            | Job::Diff { repo: id, .. } => Some(id),
            _ => None,
        }
    }

    /// The user asked for it, so they are told how it went.
    fn by_user(&self) -> bool {
        matches!(self, Job::Check(_) | Job::AppUpdate(_) | Job::Resolve { .. } | Job::ResolvePlugin { .. } | Job::DeleteGone(_) | Job::Remove { .. })
    }

    fn update_request(&self) -> Option<Request> {
        match self {
            Self::AppUpdate(action) => Some(Request::Manual(*action)),
            Self::ScheduledUpdate => Some(Request::Scheduled),
            Self::InstallOnQuit { .. } => Some(Request::Quit),
            _ => None,
        }
    }
}

/// Where settings live. Portable companion files are probed on the worker; local config is loaded there at start
/// and when falling back from portable mode, while its existing folder notification remains unchanged.
#[derive(Clone, Debug)]
pub struct SettingsSource {
    pub local_file: PathBuf,
    pub executable_dir: PathBuf,
}

impl SettingsSource {
    pub fn new(local_file: PathBuf, executable_dir: PathBuf) -> SettingsSource {
        SettingsSource { local_file, executable_dir }
    }

    fn load(&self, portable_mode: bool, force_local: bool, probe_portable: bool) -> Result<Option<LoadedSettings>, String> {
        if probe_portable {
            if let Some(config) = crate::portable::load_sources(&self.executable_dir)? {
                let profile = std::env::var_os("USERPROFILE").map(PathBuf::from);
                let skills_dir = crate::portable::resolve_skills_dir(&config, std::env::var_os("TSS_SKILLS_DIR").as_deref(), profile.as_deref(), |name| {
                    std::env::var_os(name).map(|value| value.to_string_lossy().into_owned())
                })?;
                return Ok(Some(LoadedSettings { config, skills_dir, portable: true, first_run: !self.local_file.exists(), local_stamp: None }));
            }
        }

        if !force_local && !portable_mode {
            return Ok(None);
        }

        let local_stamp = crate::engine::stamp_of(&self.local_file);
        let config = Config::load_sources(&self.local_file)?;
        let skills_dir = Paths::skills_dir(&config);
        Ok(Some(LoadedSettings { config, skills_dir, portable: false, first_run: !self.local_file.exists(), local_stamp }))
    }

    fn save(&self, before: &Config, after: &Config, portable_mode: bool) -> Result<(), String> {
        if before.app_update != after.app_update {
            let active = self.load(portable_mode, true, true)?.ok_or("cannot locate the active settings")?;
            if active.portable != portable_mode {
                return Err("settings changed since they were loaded; reload before saving".into());
            }
            Locations { home: self.local_file.parent().ok_or("cannot locate the settings folder")?, dir: &self.executable_dir, skills: &active.skills_dir }
                .check()?;
        }
        if portable_mode {
            crate::portable::save_changes(&self.executable_dir, before, after).map(|_| ())
        } else {
            if crate::portable::load_sources(&self.executable_dir)?.is_some() {
                return Err("portable settings appeared since the local settings were loaded; reload before saving".into());
            }
            after.save_if_unchanged(&self.local_file, before)
        }
    }
}

#[derive(Clone, Debug)]
pub struct LoadedSettings {
    pub config: Config,
    pub skills_dir: PathBuf,
    pub portable: bool,
    pub first_run: bool,
    pub local_stamp: Option<Stamp>,
}

impl LoadedSettings {
    pub(crate) fn matches_local_file(&self, path: &std::path::Path) -> bool {
        self.portable || self.local_stamp == crate::engine::stamp_of(path)
    }
}

#[derive(Debug)]
pub struct SettingsOutcome {
    pub first_run: bool,
    pub result: Result<LoadedSettings, String>,
}

#[derive(Clone, Debug)]
pub struct ConfigSave {
    pub before: Config,
    pub after: Config,
    pub portable: bool,
    pub revert_on_failure: bool,
}

#[derive(Clone, Debug)]
pub struct ConfigSaveOutcome {
    pub before: Config,
    /// What was saved: the settings asked for, with any lane the worker had to give a fresh ID.
    pub after: Config,
    /// The settings the window asked to save, which it is showing.
    pub requested: Config,
    pub revert_on_failure: bool,
    pub result: Result<(), String>,
}

impl ConfigSave {
    pub(crate) fn outcome(self, result: Result<(), String>) -> ConfigSaveOutcome {
        let requested = self.after.clone();
        self.outcome_of(requested, result)
    }

    fn outcome_of(self, requested: Config, result: Result<(), String>) -> ConfigSaveOutcome {
        ConfigSaveOutcome { before: self.before, after: self.after, requested, revert_on_failure: self.revert_on_failure, result }
    }
}

/// One job for the worker, with the settings it runs against. Settings that change while it runs are only used by
/// the jobs after it; changing them queues a `Plan`, so the next wake-up is always worked out with the newest.
#[derive(Clone, Debug)]
pub struct Order {
    pub job: Job,
    pub cfg: Config,
    pub skills_dir: PathBuf,
    pub settings_source: Option<SettingsSource>,
    pub portable_mode: bool,
    pub settings_initialized: bool,
    pub reload_settings: bool,
    pub config_saves: Vec<ConfigSave>,
    plugin_rows: Vec<String>,
    update_generation: u64,
    /// Run it as background work: lower CPU, I/O and memory priority, so it never gets in the way.
    pub background: bool,
}

#[derive(Debug)]
pub enum Answer {
    Started(StartupReport),
    Looked(VerifyReport),
    Checked { verified: Option<VerifyReport>, sync: SyncReport },
    SettingsLoaded,
    SourcePreview(Arc<PreviewSnapshot>),
    SourceSaved { source: SourceConfig, approved: Result<(), String> },
    Resolved(ResolveReport),
    PluginResolved(RepoReport),
    Removed { source: String, label: String, lanes: Vec<(String, SourceKind, RemoveReport)> },
    Diffed(DiffReport),
    Flushed,
    Planned,
    AppUpdate(String),
}

#[derive(Debug)]
pub struct UpdateState {
    generation: u64,
    clock: Option<ClockState>,
    staged: Option<crate::release::Version>,
    status: Option<Result<String, String>>,
    disk: Option<Result<String, String>>,
    restart: Option<Result<VerifiedTray, String>>,
}

impl UpdateState {
    /// The tray a successful install put in place, held and proven, or why it could not be: see [`Settled::restart`].
    pub fn take_restart(&mut self) -> Option<Result<VerifiedTray, String>> {
        self.restart.take()
    }
}

/// What the worker sends back: the answer, and what the engine knows now.
#[derive(Debug)]
pub struct Done {
    pub job: Job,
    pub result: Result<Answer, EngineError>,
    pub next_wake: Option<UnixSecs>,
    pub last_checks: Vec<(String, UnixSecs)>,
    pub kept: Vec<(String, Vec<String>)>,
    /// What the record says about each repository's skills.
    pub facts: Vec<(String, Vec<SkillFacts>)>,
    pub plugin_records: Vec<(String, Vec<Row>)>,
    /// Whether the last listing of each repository had no skills in it at all.
    pub lists_nothing: Vec<(String, bool)>,
    /// Whether each repository was found to need the token.
    pub private: Vec<(String, bool)>,
    /// The stamp of `state.json` as this job left it.
    pub stamp: Option<Stamp>,
    /// Settings loaded before a startup, poll, check or window-open reload.
    pub settings: Option<SettingsOutcome>,
    /// Settings saves completed with this order.
    pub config_saves: Vec<ConfigSaveOutcome>,
    /// Whether this order checked for portable companions.
    pub settings_checked: bool,
    /// The display cache, read when the app starts.
    pub seen: Option<std::collections::BTreeMap<String, crate::seen::Seen>>,
    pub update: Option<UpdateState>,
}

/// Runs one order. Called on the worker thread, which owns the engine.
/// Where the worker gave a lane a fresh ID while saving (`asked` became `saved`), give the same lane in `target` that
/// ID too, as long as it still has the ID that was asked for.
fn carry_lane_ids(asked: &Config, saved: &Config, target: &mut Config) {
    let mut moves = Vec::new();
    for source in saved.sources() {
        let Some(old) = asked.sources().iter().find(|s| s.id == source.id) else { continue };
        for kind in KINDS {
            if let (Some(from), Some(to)) = (old.lanes.id(kind), source.lanes.id(kind))
                && from != to
                && target.sources().iter().any(|s| s.id == source.id && s.lanes.id(kind) == Some(from))
            {
                moves.push((source.id.clone(), kind, to.to_owned()));
            }
        }
    }
    if moves.is_empty() {
        return;
    }
    target.edit(|list, _| {
        for (id, kind, lane) in moves {
            if let Some(s) = list.iter_mut().find(|s| s.id == id) {
                s.lanes = std::mem::take(&mut s.lanes).with(kind, lane);
            }
        }
    });
}

pub fn perform(engine: &mut Engine, order: &Order, cancel: &AtomicBool) -> Done {
    perform_with_updates(engine, order, cancel, None, crate::interval::now_secs())
}

pub(crate) fn perform_with_updates(engine: &mut Engine, order: &Order, cancel: &AtomicBool, mut updates: Option<&mut Updates>, now: UnixSecs) -> Done {
    let mut config_saves = Vec::with_capacity(order.config_saves.len());
    let mut cfg = order.cfg.clone();
    let mut renamed: Vec<(Config, Config)> = Vec::new();
    for save in &order.config_saves {
        let mut save = save.clone();
        let requested = save.after.clone();
        for (asked, saved) in &renamed {
            carry_lane_ids(asked, saved, &mut save.before);
            carry_lane_ids(asked, saved, &mut save.after);
        }
        let carried = save.after.clone();
        let result = engine.fresh_lane_ids(&save.before, &mut save.after).and_then(|()| {
            order
                .settings_source
                .as_ref()
                .map_or_else(|| Err("the settings folder is unavailable".to_string()), |source| source.save(&save.before, &save.after, save.portable))
        });
        if result.is_ok() && save.after != carried {
            renamed.push((carried, save.after.clone()));
        }
        config_saves.push(save.outcome_of(requested, result));
    }
    for (requested, saved) in &renamed {
        carry_lane_ids(requested, saved, &mut cfg);
    }

    let update_request = order.job.update_request();
    let target = order.job.target();
    let explicit_check = matches!(order.job, Job::Check(_)) || target.is_some() || update_request.is_some();
    let probe_portable = !order.settings_initialized || order.portable_mode || order.reload_settings || explicit_check;
    let loads_settings = order.job == Job::LoadSettings
        || (!order.settings_initialized && order.job == Job::Startup)
        || update_request.is_some()
        || target.is_some()
        || (probe_portable && (matches!(order.job, Job::Poll | Job::Check(_)) || order.reload_settings));
    let force_local = target.is_some()
        || order.reload_settings
        || update_request.is_some()
        || (!order.settings_initialized && matches!(order.job, Job::LoadSettings | Job::Startup));
    let mut skills_dir = order.skills_dir.clone();
    let portable_mode = order.portable_mode;
    let mut settings = None;
    let mut settings_error = None;
    if loads_settings {
        if let Some(source) = &order.settings_source {
            let first_run = !source.local_file.exists();
            match source.load(portable_mode, force_local, probe_portable) {
                Ok(Some(loaded)) => {
                    cfg = loaded.config.clone();
                    skills_dir = loaded.skills_dir.clone();
                    settings = Some(SettingsOutcome { first_run: loaded.first_run, result: Ok(loaded) });
                }
                Ok(None) => {}
                Err(error) => {
                    settings_error = Some(error.clone());
                    settings = Some(SettingsOutcome { first_run, result: Err(error) });
                }
            }
        }
    }
    let moved = skills_dir != order.skills_dir;
    let reload_only = order.job == Job::LoadSettings || (order.job == Job::Plan && order.reload_settings);
    let skip_for_folder_change = moved && matches!(order.job, Job::Poll | Job::Check(_));
    if update_request.is_none() && settings_error.is_none() && !skip_for_folder_change && !reload_only && engine.paths().skills != skills_dir {
        engine.set_skills_dir(skills_dir.clone());
    }

    let mut update_status = None;
    let mut disk = None;
    let mut restart = None;
    let source = order.settings_source.as_ref();
    if let Some(store) = source {
        let shared = settings.as_ref().and_then(|s| s.result.as_ref().ok()).map_or(portable_mode, |s| s.portable);
        engine.settings_at(store.executable_dir.clone(), shared);
    }
    if let Some(worker) = updates.as_deref_mut() {
        worker.reset_if_changed(order.update_generation, cfg.app_update);
        if settings_error.is_none() {
            if let Some(source) = source {
                let loc = Locations { home: &engine.paths().home, dir: &source.executable_dir, skills: &skills_dir };
                if order.reload_settings {
                    disk = Some(loc.check().and_then(|()| worker.disk_status(loc.dir)));
                }
                if order.job == Job::Startup {
                    if let Err(e) = worker.initialize(&loc) {
                        update_status = Some(Err(e));
                    }
                } else if order.reload_settings && worker.clock.is_none() && order.job == Job::Plan {
                    // A previously unreadable clock can be repaired and loaded without restarting.
                    if let Err(e) = worker.initialize(&loc) {
                        update_status = Some(Err(e));
                    }
                }
            }
        }
    }
    let result = if let Some(error) = settings_error {
        Err(EngineError::Failed(error))
    } else if let Some(request) = update_request {
        let mut run = || -> Result<String, String> {
            let source = source.ok_or("cannot locate the installation folder")?;
            let worker = updates.as_deref_mut().ok_or("the app update worker is unavailable")?;
            let loaded_portable = settings.as_ref().and_then(|s| s.result.as_ref().ok()).is_some_and(|s| s.portable);
            if !matches!(request, Request::Manual(_)) && (cfg.app_update != order.cfg.app_update || moved || loaded_portable != order.portable_mode) {
                return Ok("App update settings changed; the schedule will use the new settings.".into());
            }
            let loc = Locations { home: &engine.paths().home, dir: &source.executable_dir, skills: &skills_dir };
            let confirm = || {
                if config_saves.iter().any(|s| s.result.is_err()) {
                    return Err("settings could not be saved; app update cancelled".into());
                }
                let active = source.load(loaded_portable, true, true)?.ok_or("cannot reload app update settings")?;
                if active.skills_dir != skills_dir || active.portable != loaded_portable {
                    return Err("the active settings folder changed; check again".into());
                }
                if !matches!(request, Request::Manual(_)) && (!active.config.app_update.enabled || active.config.app_update != cfg.app_update) {
                    return Err("app update settings changed; automatic installation cancelled".into());
                }
                if matches!(order.job, Job::InstallOnQuit { before } if Instant::now() >= before) {
                    return Err("the clean Quit timed out before replacement; neither program was installed".into());
                }
                Ok(())
            };
            let result = worker.run(&loc, request, cfg.app_update, now, cancel, &confirm);
            let installed = worker.take_restart();
            if result.is_ok() {
                restart = installed;
            }
            disk = Some(loc.check().and_then(|()| worker.disk_status(loc.dir)));
            result
        };
        run().map(Answer::AppUpdate).map_err(EngineError::Failed)
    } else if target.is_some_and(|id| {
        moved
            || if matches!(order.job, Job::Remove { .. }) {
                order.cfg.find_source(id).is_none() || cfg.find_source(id) != order.cfg.find_source(id)
            } else {
                order.cfg.find(id).is_none() || cfg.find(id) != order.cfg.find(id)
            }
    }) {
        Err(EngineError::Failed("The source changed. Choose the source you want again.".into()))
    } else if reload_only || skip_for_folder_change {
        Ok(Answer::SettingsLoaded)
    } else {
        match &order.job {
            Job::LoadSettings => Ok(Answer::SettingsLoaded),
            Job::Startup => engine.startup(&cfg).map(Answer::Started),
            Job::Look => engine.verify(&cfg, Access::ReadOnly).map(Answer::Looked),
            Job::PreviewSource(input) => engine
                .preview_location_with_settings(&cfg, &input.text, false, &input.settings, cancel)
                .map(|preview| Answer::SourcePreview(Arc::new(PreviewSnapshot::new(preview)))),
            Job::SaveSource(input, reviewed, settings, approve) => {
                save_source(engine, &mut cfg, source, (portable_mode, &skills_dir), input, reviewed, settings, *approve, cancel)
            }
            Job::Poll => {
                engine.retain_plugin_attention_rows(&order.plugin_rows);
                engine.sync(&cfg, &Which::Due, &SyncOptions::default(), cancel).map(|sync| Answer::Checked { verified: None, sync })
            }
            Job::Check(which) => engine.verify(&cfg, Access::Write).and_then(|verified| {
                let which = which.as_ref().map_or(Which::All, |id| Which::One(id.clone()));
                let sync = engine.sync(&cfg, &which, &SyncOptions::default(), cancel)?;
                Ok(Answer::Checked { verified: Some(verified), sync })
            }),
            Job::Resolve { repo, skill, how } => engine.resolve(&cfg, repo, skill, *how, cancel).map(Answer::Resolved),
            Job::ResolvePlugin { repo, choice } => engine.resolve_plugin(&cfg, repo, choice, cancel).map(Answer::PluginResolved),
            Job::DeleteGone(id) => {
                let opts = SyncOptions { ignore_etag: true, confirm_mass_delete: true, ..SyncOptions::default() };
                engine.sync(&cfg, &Which::One(id.clone()), &opts, cancel).map(|sync| Answer::Checked { verified: None, sync })
            }
            Job::Remove { repo, delete_skills } => (|| {
                let whole = cfg.find_source(repo).ok_or_else(|| EngineError::Failed("The source changed. Choose the source you want again.".into()))?;
                let lanes = removal_lanes(engine, &cfg, whole).map_err(EngineError::Failed)?;
                if *delete_skills && !lanes.iter().any(|(_, k)| *k == SourceKind::Skills) {
                    return Err(EngineError::Failed(
                        "Only a source with skills can delete them. Remove this source without deleting to keep its files.".into(),
                    ));
                }
                let id = whole.id.clone();
                let label = source_label(whole);
                let lanes = remove_whole_source(engine, &cfg, &lanes, *delete_skills, false)?;
                Ok(Answer::Removed { source: id, label, lanes })
            })(),
            Job::Diff { repo, skill, preview: false } => engine.diff(&cfg, repo, skill, cancel).map(Answer::Diffed),
            Job::Diff { repo, skill, preview: true } => engine.preview(&cfg, repo, skill, cancel).map(Answer::Diffed),
            Job::Flush => engine.flush().map(|()| Answer::Flushed),
            Job::Plan => Ok(Answer::Planned),
            Job::AppUpdate(_) | Job::ScheduledUpdate | Job::InstallOnQuit { .. } => unreachable!("release jobs are handled separately"),
        }
    };
    Done {
        job: order.job.clone(),
        next_wake: engine.next_wake(&cfg),
        last_checks: cfg.repos().iter().filter_map(|r| engine.last_check(&r.id).map(|at| (r.id.clone(), at))).collect(),
        kept: cfg.repos().iter().map(|r| (r.id.clone(), engine.kept(&r.id))).collect(),
        facts: cfg.repos().iter().filter(|r| r.kind != SourceKind::Plugins).map(|r| (r.id.clone(), engine.facts(&r.id))).collect(),
        plugin_records: if matches!(result, Ok(Answer::Started(_) | Answer::Looked(_) | Answer::Checked { .. } | Answer::PluginResolved(_))) {
            cfg.repos()
                .iter()
                .filter(|r| r.kind == SourceKind::Plugins)
                .map(|r| (r.id.clone(), engine.plugin_record(r).map(recorded_plugin_rows).unwrap_or_default()))
                .collect()
        } else {
            Vec::new()
        },
        lists_nothing: cfg.repos().iter().map(|r| (r.id.clone(), engine.lists_nothing(&r.id))).collect(),
        private: cfg.repos().iter().map(|r| (r.id.clone(), engine.needs_token(&r.id))).collect(),
        stamp: engine.state_stamp(),
        settings,
        config_saves,
        settings_checked: loads_settings && probe_portable,
        seen: (order.job == Job::Startup).then(|| crate::seen::load(&engine.paths().seen_file())),
        result,
        update: updates.map(|u| UpdateState { generation: order.update_generation, clock: u.clock, staged: u.staged(), status: update_status, disk, restart }),
    }
}

/// What the window has to do after a job.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Settled {
    /// The engine let go of this repository: take it out of the settings.
    pub removed: Option<String>,
    /// How a job the user asked for went, for the status line.
    pub feedback: Option<String>,
    /// The state changed without the user, so what they were last told no longer holds.
    pub stale_feedback: bool,
    /// "Show changes" finished.
    pub diff: Option<Shown>,
    /// The tray may restart into an app update this job installed, if it installed one: the user asked for the
    /// install, or the schedule made it under the settings in use, and the app is not quitting.
    pub restart: bool,
}

/// What "Show changes" or Preview found for a skill, or why it could not be shown.
#[derive(Debug, PartialEq, Eq)]
pub struct Shown {
    pub repo: String,
    pub skill: String,
    pub preview: bool,
    pub result: Result<DiffReport, String>,
}

/// The state the UI thread keeps.
#[derive(Debug)]
pub struct App {
    pub board: Board,
    cfg: Config,
    skills_dir: PathBuf,
    settings_source: Option<SettingsSource>,
    portable_mode: bool,
    settings_initialized: bool,
    reload_settings_pending: bool,
    portable_probe_pending: bool,
    config_saves: Vec<ConfigSave>,
    draft: Option<SourceDraft>,
    next_ticket: u64,
    next_request: u64,
    queue: VecDeque<Job>,
    busy: Option<Job>,
    next_wake: Option<UnixSecs>,
    update_clock: Option<ClockState>,
    update_hold: UnixSecs,
    update_generation: u64,
    staged_for_quit: Option<crate::release::Version>,
    app_disk: String,
    pub app_update_status: String,
    /// No timer-driven job starts before this.
    not_before: UnixSecs,
    /// Jobs in a row that failed as a whole.
    failures: u32,
    started: bool,
    /// When the timer was last set to fire.
    armed: Option<UnixSecs>,
    /// Windows started the app at sign-in, and its first look, still to come, is background work.
    at_sign_in: bool,
    /// Started at sign-in, the first look waits for the timer, or for the user to come.
    sign_in_wait: bool,
    quitting: bool,
    state_stamp: Option<Stamp>,
    /// Said on the status line when the first look is done: this tray restarted into the update the one before it
    /// installed.
    restart_note: Option<String>,
}

impl App {
    /// Startup is the first job.
    pub fn new(cfg: Config, skills_dir: PathBuf) -> App {
        App {
            board: Board::new(),
            cfg,
            skills_dir,
            settings_source: None,
            portable_mode: false,
            settings_initialized: false,
            reload_settings_pending: false,
            portable_probe_pending: false,
            config_saves: Vec::new(),
            draft: None,
            next_ticket: 0,
            next_request: 0,
            queue: VecDeque::from([Job::Startup]),
            busy: None,
            next_wake: None,
            update_clock: None,
            update_hold: 0,
            update_generation: 0,
            staged_for_quit: None,
            app_disk: String::new(),
            app_update_status: "Not checked in this session.".into(),
            not_before: 0,
            failures: 0,
            started: false,
            armed: None,
            at_sign_in: false,
            sign_in_wait: false,
            quitting: false,
            state_stamp: None,
            restart_note: None,
        }
    }

    pub fn cfg(&self) -> &Config {
        &self.cfg
    }

    pub fn draft(&self) -> Option<&SourceDraft> {
        self.draft.as_ref()
    }

    pub fn saving_source(&self) -> bool {
        matches!(self.busy, Some(Job::SaveSource(..))) || self.draft.as_ref().is_some_and(|d| matches!(d.stage, DraftStage::Saving))
    }

    /// Preview a location the user typed. A malformed location is refused at once; a bare `owner/repo` that is already a
    /// source gets a draft for the refusal, but no worker job.
    pub fn preview_location(&mut self, text: &str, every: Option<Interval>) -> Result<u64, String> {
        if self.quitting || self.saving_source() {
            return Err("Cannot start another preview now.".into());
        }
        let location = Location::parse(text)?;
        let known = match location.target {
            Target::Bare => same_location(&self.cfg, &format!("{}/{}", location.owner, location.name), "", "").map(|old| old.id.clone()),
            _ => None,
        };
        self.dismiss_source();
        self.next_ticket += 1;
        self.next_request += 1;
        let settings = SourceConfig { interval: every, enabled: false, ..SourceConfig::default() };
        let input = Arc::new(DraftInput { ticket: self.next_ticket, request: self.next_request, text: text.trim().to_string(), settings: settings.clone() });
        let stage = match known {
            Some(id) => DraftStage::AlreadySetUp(id),
            None => DraftStage::Loading,
        };
        let read = matches!(stage, DraftStage::Loading);
        self.draft = Some(SourceDraft { input: input.clone(), settings, previous_preview: None, stage });
        if read {
            self.want(Job::PreviewSource(input));
        }
        Ok(self.next_ticket)
    }

    /// Retry the current preview with its existing settings and retain any previous result as stale content.
    pub fn retry_source(&mut self, ticket: u64) -> Result<(), String> {
        if self.quitting || self.saving_source() {
            return Err("Cannot retry this preview now.".into());
        }
        let Some(draft) = self.draft.as_mut().filter(|d| d.input.ticket == ticket) else {
            return Err("This preview is no longer current.".into());
        };
        let previous_preview = match &draft.stage {
            DraftStage::Reviewed(preview) => Some(preview.clone()),
            _ => draft.previous_preview.clone(),
        };
        self.next_request += 1;
        let input = Arc::new(DraftInput { ticket, request: self.next_request, text: draft.input.text.clone(), settings: draft.settings.clone() });
        draft.input = input.clone();
        draft.previous_preview = previous_preview;
        draft.stage = DraftStage::Loading;
        self.queue.retain(|job| !matches!(job, Job::PreviewSource(_)));
        self.want(Job::PreviewSource(input));
        Ok(())
    }

    /// Change the location of an unsaved source while keeping its staged type, interval and folder choices.
    pub fn edit_draft_location(&mut self, ticket: u64, text: &str, every: Option<Interval>) -> Result<(), String> {
        if self.quitting || self.saving_source() {
            return Err("Cannot edit this preview now.".into());
        }
        let location = Location::parse(text)?;
        let Some(draft) = self.draft.as_mut().filter(|d| d.input.ticket == ticket) else {
            return Err("This preview is no longer current.".into());
        };
        let mut settings = draft.settings.clone();
        settings.id.clear();
        settings.repo = format!("{}/{}", location.owner, location.name);
        settings.branch.clear();
        settings.path.clear();
        settings.lanes = Default::default();
        settings.use_token = false;
        settings.enabled = false;
        settings.interval = every;
        let known = match location.target {
            Target::Bare => same_location(&self.cfg, &settings.repo, "", "").map(|old| old.id.clone()),
            _ => None,
        };
        self.next_request += 1;
        let input = Arc::new(DraftInput { ticket, request: self.next_request, text: text.trim().to_string(), settings: settings.clone() });
        draft.input = input.clone();
        draft.settings = settings;
        draft.previous_preview = None;
        draft.stage = known.map_or(DraftStage::Loading, DraftStage::AlreadySetUp);
        self.queue.retain(|job| !matches!(job, Job::PreviewSource(_)));
        if matches!(draft.stage, DraftStage::Loading) {
            self.want(Job::PreviewSource(input));
        }
        Ok(())
    }

    /// Edit an unsaved source. Folder rules and repository-specific-item choices rediscover the preview.
    pub fn edit_draft(&mut self, ticket: u64, change: impl FnOnce(&mut SourceConfig)) -> Result<bool, String> {
        if self.quitting || self.saving_source() {
            return Err("Cannot edit this preview now.".into());
        }
        let Some(draft) = self.draft.as_mut().filter(|d| d.input.ticket == ticket) else {
            return Err("This preview is no longer current.".into());
        };
        let before = draft.settings.clone();
        change(&mut draft.settings);
        if draft.settings == before {
            return Ok(false);
        }
        draft.settings.effective_policy(self.cfg.global()).map_err(|why| capitalized(&why))?;

        let rediscover = draft.settings.ignore_add != before.ignore_add
            || draft.settings.ignore_allow != before.ignore_allow
            || draft.settings.include_repo_items != before.include_repo_items;
        if rediscover {
            let previous_preview = match &draft.stage {
                DraftStage::Reviewed(preview) => Some(preview.clone()),
                _ => draft.previous_preview.clone(),
            };
            self.next_request += 1;
            let input = Arc::new(DraftInput { ticket, request: self.next_request, text: draft.input.text.clone(), settings: draft.settings.clone() });
            draft.input = input.clone();
            draft.previous_preview = previous_preview;
            draft.stage = DraftStage::Loading;
            self.queue.retain(|job| !matches!(job, Job::PreviewSource(_)));
            self.want(Job::PreviewSource(input));
        }
        Ok(true)
    }

    pub fn dismiss_source(&mut self) {
        if !self.saving_source() {
            self.draft = None;
            self.queue.retain(|job| !matches!(job, Job::PreviewSource(_)));
        }
    }

    /// Save the reviewed preview. `approve_plugins` approves the plugins it showed, after the save, and only those.
    pub fn confirm_source(&mut self, ticket: u64, approve_plugins: bool) -> Result<(), String> {
        let draft = self.draft.as_mut().filter(|d| !self.quitting && d.input.ticket == ticket).ok_or("This preview is no longer current.")?;
        let DraftStage::Reviewed(preview) = &draft.stage else { return Err("There is no finished preview to add.".into()) };
        if !KINDS.iter().any(|&kind| preview_syncs_for_settings(preview, &draft.settings, self.cfg.global(), kind)) {
            return Err("Nothing here would sync. Turn on an item type or change the folder rules.".into());
        }
        draft.settings.enabled = true;
        let job = Job::SaveSource(draft.input.clone(), preview.clone(), Box::new(draft.settings.clone()), approve_plugins);
        draft.previous_preview = Some(preview.clone());
        draft.stage = DraftStage::Saving;
        self.want(job);
        Ok(())
    }

    /// Change one source's settings (by source ID or lane ID) and save them. False when nothing changed, so nothing is saved.
    pub fn edit_source(&mut self, id: &str, change: impl FnOnce(&mut SourceConfig)) -> Result<bool, String> {
        let before = self.cfg.clone();
        let source = before.find_source(id).ok_or("The source changed. Choose the source you want again.")?;
        let mut edited = source.clone();
        change(&mut edited);
        if edited == *source {
            return Ok(false);
        }
        edited.effective_policy(before.global()).map_err(|why| capitalized(&why))?;
        let sid = source.id.clone();
        let mut after = before.clone();
        after.edit(|sources, _| {
            if let Some(s) = sources.iter_mut().find(|s| s.id == sid) {
                *s = edited;
            }
        });
        self.save_edit(before, after);
        Ok(true)
    }

    /// Change the defaults every source follows, and save them. False when nothing changed, so nothing is saved.
    pub fn edit_global(&mut self, change: impl FnOnce(&mut GlobalSettings)) -> Result<bool, String> {
        let before = self.cfg.clone();
        let mut global = before.global().clone();
        change(&mut global);
        if global == *before.global() {
            return Ok(false);
        }
        for source in before.sources() {
            source.effective_policy(&global).map_err(|why| capitalized(&why))?;
        }
        let mut after = before.clone();
        after.edit(|_, g| *g = global);
        self.save_edit(before, after);
        Ok(true)
    }

    fn save_edit(&mut self, before: Config, after: Config) {
        self.set_cfg(after.clone(), self.skills_dir.clone());
        self.queue_config_save(before, after, self.portable_mode, true);
    }

    pub fn skills_dir(&self) -> &PathBuf {
        &self.skills_dir
    }

    pub fn set_settings_source(&mut self, source: SettingsSource) {
        self.settings_source = Some(source);
    }

    pub fn queue_initial_settings_load(&mut self) {
        self.queue.push_front(Job::LoadSettings);
    }

    pub fn portable_mode(&self) -> bool {
        self.portable_mode
    }

    pub fn settings_initialized(&self) -> bool {
        self.settings_initialized
    }

    pub fn portable_probe_pending(&self) -> bool {
        self.portable_probe_pending
    }

    pub fn request_settings_reload(&mut self) {
        self.reload_settings_pending = true;
        self.portable_probe_pending = true;
        self.want(Job::Plan);
    }

    pub(crate) fn reload_before_next_job(&mut self) {
        if !self.quitting {
            self.request_settings_reload();
            self.queue.retain(|job| *job != Job::Plan);
            self.queue.push_front(Job::Plan);
        }
    }

    pub fn queue_config_save(&mut self, before: Config, after: Config, portable: bool, revert_on_failure: bool) {
        self.config_saves.push(ConfigSave { before, after, portable, revert_on_failure });
        self.want(Job::Plan);
    }

    /// Takes in settings loaded on the worker. A changed skills folder is always started before it is checked.
    pub fn settings_reloaded(&mut self, loaded: LoadedSettings, during: &Job) {
        let moved = loaded.skills_dir != self.skills_dir;
        if self.settings_initialized && self.portable_mode != loaded.portable {
            self.revoke_update();
            self.invalidate_draft();
        }
        self.retain_current_work(&loaded.config, moved);
        self.cfg = loaded.config;
        self.skills_dir = loaded.skills_dir;
        self.portable_mode = loaded.portable;
        self.settings_initialized = true;
        if moved {
            self.started = false;
            if *during != Job::Startup && !self.sign_in_wait {
                self.want(Job::Startup);
            }
        } else if !self.started && *during != Job::Startup && !self.sign_in_wait {
            self.want(Job::Startup);
        }
    }

    /// New settings. The next job uses them; when to wake up is worked out again. Another skills folder is taken in
    /// the way the app starts, so the change is recorded, and the user told, at once rather than at the next check.
    pub fn set_cfg(&mut self, cfg: Config, skills_dir: PathBuf) {
        let moved = skills_dir != self.skills_dir;
        self.retain_current_work(&cfg, moved);
        self.cfg = cfg;
        self.skills_dir = skills_dir;
        if moved {
            self.started = false;
            self.want(Job::Startup);
        } else {
            self.want(Job::Plan);
        }
    }

    fn retain_current_work(&mut self, cfg: &Config, moved: bool) {
        if moved || self.cfg != *cfg {
            self.invalidate_draft();
        }
        if moved || self.cfg.app_update != cfg.app_update {
            self.revoke_update();
        }
        for old in self.cfg.repos().iter().filter(|r| r.kind == SourceKind::Plugins) {
            if cfg.find(&old.id).is_none_or(|new| old.kind != new.kind || old.repo != new.repo || old.branch != new.branch || old.path != new.path) {
                self.board.forget_repo(&old.id);
            }
        }
        self.queue.retain(|job| {
            job.target().is_none_or(|repo| {
                if matches!(job, Job::Remove { .. }) {
                    return !moved
                        && self
                            .cfg
                            .find_source(repo)
                            .zip(cfg.find_source(repo))
                            .is_some_and(|(old, new)| old.id == new.id && old.repo == new.repo && old.branch == new.branch && old.path == new.path);
                }
                !moved
                    && self
                        .cfg
                        .find(repo)
                        .zip(cfg.find(repo))
                        .is_some_and(|(old, new)| old.repo == new.repo && old.branch == new.branch && old.path == new.path && old.kind == new.kind)
            })
        });
    }

    fn invalidate_draft(&mut self) {
        if let Some(draft) = self.draft.as_mut() {
            draft.stage = DraftStage::Failed("The settings changed. Preview the source again.".into());
            self.queue.retain(|j| !matches!(j, Job::PreviewSource(_) | Job::SaveSource(..)));
        }
    }

    fn revoke_update(&mut self) {
        self.update_generation = self.update_generation.wrapping_add(1);
        if self.staged_for_quit.is_some() {
            self.app_update_status = "Pending automatic install cancelled.".into();
        }
        self.staged_for_quit = None;
        self.queue.retain(|job| !matches!(job, Job::ScheduledUpdate | Job::InstallOnQuit { .. }));
    }

    pub(crate) fn accepts_update(&self, done: &Done) -> bool {
        done.update.as_ref().is_none_or(|u| u.generation == self.update_generation)
    }

    pub(crate) fn release_summary(&self) -> String {
        let pending = self.staged_for_quit.map_or_else(|| "Nothing waiting for Quit.".into(), |v| format!("Waiting for Quit: {v}."));
        format!("{} {pending}\nLast result: {}", self.app_disk, self.app_update_status).trim_start().to_owned()
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

    /// Asks for a job. A job that one already waiting covers is dropped. Returns whether it was queued. A job the
    /// user asks for ends the wait after sign-in: the first look runs before it.
    pub fn want(&mut self, job: Job) -> bool {
        if self.quitting || self.queue.contains(&job) {
            return false;
        }
        if matches!(job, Job::Check(_)) {
            self.portable_probe_pending = true;
        }
        if job.by_user() {
            self.user_here();
        }
        let has = |q: &VecDeque<Job>, f: fn(&Job) -> bool| q.iter().any(f);
        let covered = match &job {
            Job::Poll | Job::Check(Some(_)) => has(&self.queue, |j| *j == Job::Check(None)),
            // The first look after sign-in is still to come, and it looks anyway.
            Job::Look => self.sign_in_wait || has(&self.queue, |j| matches!(j, Job::Startup | Job::Look | Job::Check(_))),
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
            self.sign_in_wait = false;
            self.board.set_waiting(false);
            self.queue.push_front(job);
        } else if matches!(job, Job::Resolve { .. } | Job::ResolvePlugin { .. } | Job::Remove { .. }) {
            let at = self
                .queue
                .iter()
                .position(|j| !matches!(j, Job::Startup | Job::Resolve { .. } | Job::ResolvePlugin { .. } | Job::Remove { .. }))
                .unwrap_or(self.queue.len());
            self.queue.insert(at, job);
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
        // A preview never carries a pending settings write. Settle existing edits first.
        let job = if (self.reload_settings_pending || !self.config_saves.is_empty())
            && matches!(self.queue.front(), Some(Job::PreviewSource(_) | Job::SaveSource(..)))
        {
            Job::Plan
        } else {
            self.queue.pop_front()?
        };
        if job.shows_work() {
            self.board.set_working(true);
        }
        self.busy = Some(job.clone());
        self.armed = None;
        let background = matches!(job, Job::Poll | Job::ScheduledUpdate) || (matches!(job, Job::LoadSettings | Job::Startup) && self.at_sign_in);
        let reload_settings = std::mem::take(&mut self.reload_settings_pending);
        let config_saves = std::mem::take(&mut self.config_saves);
        let plugin_rows = if job == Job::Poll {
            self.cfg.repos().iter().filter(|r| r.kind == SourceKind::Plugins && self.board.has_plugin_rows(&r.id)).map(|r| r.id.clone()).collect()
        } else {
            Vec::new()
        };
        Some(Order {
            plugin_rows,
            job,
            cfg: self.cfg.clone(),
            skills_dir: self.skills_dir.clone(),
            settings_source: self.settings_source.clone(),
            portable_mode: self.portable_mode,
            settings_initialized: self.settings_initialized,
            reload_settings,
            update_generation: self.update_generation,
            config_saves,
            background,
        })
    }

    /// Takes in what the worker sent back.
    pub fn finish(&mut self, done: Done, now: UnixSecs) -> Settled {
        self.busy = None;
        self.board.set_working(false);
        let before = self.health();
        let mut settled = Settled::default();
        let current = self.accepts_update(&done);
        if let Some(update) = done.update {
            self.update_clock = update.clock;
            if current {
                self.staged_for_quit = update.staged;
                if let Some(disk) = update.disk {
                    self.app_disk = disk.unwrap_or_else(|why| format!("Could not read the installed pair: {why}"));
                }
                if let Some(status) = update.status {
                    self.note_app_update(status, now, &mut settled);
                }
            }
        }
        let job = done.job;
        if let Some(seen) = done.seen {
            self.board.set_seen(seen);
        }
        let config_saves = done.config_saves;
        if done.settings_checked && !self.reload_settings_pending && !self.queue.iter().any(|job| matches!(job, Job::Check(_))) {
            self.portable_probe_pending = false;
        }
        let by_user = job.by_user();
        let mut checked = None;

        for (id, rows) in done.plugin_records {
            self.board.note_recorded_plugins(&id, rows);
        }
        match done.result {
            Err(e) if matches!(job, Job::PreviewSource(_) | Job::SaveSource(..)) => {
                let (input, failed_preview) = match &job {
                    Job::PreviewSource(i) => (i, None),
                    Job::SaveSource(i, preview, ..) => (i, Some(preview.clone())),
                    _ => unreachable!(),
                };
                if let Some(draft) = self.draft.as_mut().filter(|d| d.input == *input) {
                    let text = format!("Source not added: {e}");
                    if let Some(preview) = failed_preview {
                        draft.settings.enabled = false;
                        draft.previous_preview = Some(preview);
                    }
                    draft.stage = DraftStage::Failed(text.clone());
                    settled.feedback = Some(text);
                }
            }
            Err(e) if job.update_request().is_some() => {
                if current || matches!(job, Job::AppUpdate(_)) {
                    self.note_app_update(Err(e.to_string()), now, &mut settled);
                }
                if job == Job::ScheduledUpdate {
                    self.update_hold = now.saturating_add(FAILURE_BACKOFF);
                }
            }
            Err(e) if matches!(job, Job::Diff { .. }) => settled.diff = shown(&job, Err(capitalized(&e.to_string()))),
            Err(EngineError::Busy) => match job {
                Job::Startup | Job::Poll => self.hold(now + BUSY_RETRY),
                _ if by_user => settled.feedback = Some(capitalized(&EngineError::Busy.to_string())),
                _ => {}
            },
            Err(EngineError::Failed(why)) => {
                if by_user {
                    if let Job::ResolvePlugin { repo, .. } = &job {
                        if self.cfg.find(repo).is_some_and(|r| r.kind == SourceKind::Plugins) {
                            self.board.note_plugin_failure(repo, &why);
                        }
                    }
                    self.board.say(now, Level::Error, why.clone());
                    settled.feedback = Some(why);
                } else if matches!(job, Job::LoadSettings | Job::Startup | Job::Poll | Job::Look) {
                    self.failures += 1;
                    self.board.note_failure(now, Some(&why));
                    self.hold(now + backoff(self.failures));
                    if job == Job::LoadSettings {
                        self.queue.retain(|queued| *queued != Job::Startup);
                    }
                }
            }
            Ok(answer) => {
                if !matches!(
                    answer,
                    Answer::Flushed
                        | Answer::Planned
                        | Answer::SettingsLoaded
                        | Answer::Diffed(_)
                        | Answer::AppUpdate(_)
                        | Answer::SourcePreview(..)
                        | Answer::SourceSaved { .. }
                ) {
                    self.failures = 0;
                    self.board.note_failure(now, None);
                }
                match answer {
                    Answer::SourcePreview(preview) => {
                        if let Job::PreviewSource(input) = &job {
                            let known = already_added(&self.cfg, preview.location());
                            if let Some(d) = self.draft.as_mut().filter(|d| d.input == *input && matches!(d.stage, DraftStage::Loading)) {
                                d.stage = match known {
                                    Some(id) => {
                                        d.previous_preview = None;
                                        DraftStage::AlreadySetUp(id)
                                    }
                                    None => {
                                        d.settings = generated_settings(preview.location(), &d.settings);
                                        d.previous_preview = None;
                                        DraftStage::Reviewed(preview)
                                    }
                                };
                            }
                        }
                    }
                    Answer::SourceSaved { source, approved } => {
                        if let Job::SaveSource(input, ..) = &job {
                            if self.draft.as_ref().is_some_and(|d| d.input == *input && matches!(d.stage, DraftStage::Saving)) {
                                self.draft = None;
                                let label = source_label(&source);
                                let id = source.id.clone();
                                if let Err(why) = self.cfg.add_source_config(source) {
                                    // The worker saved it, so the settings must be read again.
                                    self.board.say(now, Level::Error, why);
                                    self.reload_before_next_job();
                                }
                                match approved {
                                    Ok(()) => {
                                        settled.feedback = Some(format!("Added {label} [{id}]."));
                                        // Any lane but the plugins one checks the whole source.
                                        let lanes = self.cfg.lanes_of(&id);
                                        if let Some(lane) = lanes.iter().find(|r| r.kind != SourceKind::Plugins).or(lanes.first()) {
                                            let lane = lane.id.clone();
                                            self.want(Job::Check(Some(lane)));
                                        }
                                    }
                                    Err(why) => {
                                        self.board.say(now, Level::Error, why.clone());
                                        settled.feedback = Some(why);
                                    }
                                }
                            } else {
                                // Settings changed while the worker was replying. Never check a replacement with this ID.
                                self.reload_before_next_job();
                            }
                        }
                    }
                    Answer::AppUpdate(message) => {
                        if current || matches!(job, Job::AppUpdate(_)) {
                            self.note_app_update(Ok(message), now, &mut settled);
                        }
                        settled.restart = !self.quitting && (job == Job::AppUpdate(Action::Install) || (job == Job::ScheduledUpdate && current));
                    }
                    Answer::Started(report) => {
                        self.board.note_startup(&report, now);
                        self.started = true;
                        self.at_sign_in = false;
                        settled.feedback = self.restart_note.take();
                    }
                    Answer::Looked(report) => self.board.note_verify(&report, now),
                    Answer::Checked { verified, sync } => {
                        if let Some(report) = &verified {
                            self.board.note_verify(report, now);
                        }
                        self.board.note_source_sync(&sync, now, &self.cfg);
                        if by_user {
                            checked = Some(sync);
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
                    Answer::PluginResolved(report) => {
                        self.board.note_source_report(&report, now, &self.cfg);
                        let feedback = match (&report.outcome, &job) {
                            (Outcome::Failed(why), _) => Some((Level::Error, why.message.clone())),
                            (_, Job::ResolvePlugin { repo, choice }) => {
                                let name = choice.name();
                                self.board.plugin_tag(repo, name).map(|tag| (Level::Ok, format!("{name}: {tag}.")))
                            }
                            _ => None,
                        };
                        if let Some((level, text)) = feedback {
                            self.board.say(now, level, text.clone());
                            settled.feedback = Some(text);
                        }
                    }
                    Answer::Removed { source, label, lanes } => {
                        let (text, attention) = removed_text(&label, &lanes);
                        for (lane, ..) in &lanes {
                            self.board.forget_repo(lane);
                        }
                        self.board.say(now, if attention { Level::Attention } else { Level::Ok }, text.clone());
                        settled.feedback = Some(text);
                        settled.removed = Some(source);
                    }
                    Answer::Diffed(report) => settled.diff = shown(&job, Ok(report)),
                    Answer::SettingsLoaded => {}
                    Answer::Flushed | Answer::Planned => {}
                }
            }
        }

        for save in config_saves {
            if let Err(error) = save.result {
                if save.revert_on_failure && self.cfg == save.requested {
                    self.cfg = save.before;
                    self.queue.retain(|queued| !matches!(queued, Job::Check(_)));
                }
                let text = format!("Settings could not be saved: {}", capitalized(&error));
                self.board.say(now, Level::Error, text.clone());
                settled.feedback = Some(text);
            } else {
                if self.cfg == save.before {
                    self.set_cfg(save.after.clone(), self.skills_dir.clone());
                } else if save.requested != save.after {
                    let mut cfg = self.cfg.clone();
                    carry_lane_ids(&save.requested, &save.after, &mut cfg);
                    for pending in &mut self.config_saves {
                        carry_lane_ids(&save.requested, &save.after, &mut pending.before);
                        carry_lane_ids(&save.requested, &save.after, &mut pending.after);
                    }
                    if cfg != self.cfg {
                        self.set_cfg(cfg, self.skills_dir.clone());
                    }
                }
                if self.cfg == save.after {
                    // A lane that syncs again, because its source or its type was turned back on, is checked now.
                    for r in save.after.repos() {
                        if r.enabled && save.before.find(&r.id).is_none_or(|old| !old.enabled) {
                            self.want(Job::Check(Some(r.id.clone())));
                        }
                    }
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
        for (id, facts) in &done.facts {
            self.board.note_facts(id, facts);
        }
        for (id, nothing) in &done.lists_nothing {
            self.board.note_lists_nothing(id, *nothing);
        }
        for (id, needs_token) in &done.private {
            self.board.note_private(id, *needs_token);
        }
        self.state_stamp = done.stamp;
        if job == Job::Poll {
            self.hold(now + MIN_POLL_GAP);
        }
        // Told once the board knows all the job brought back, so that it never says all is well when it is not.
        if let Some(sync) = checked {
            settled.feedback = Some(summary(&sync, self.health() == Health::Ok, &self.cfg));
        }
        settled.stale_feedback = settled.feedback.is_none() && self.health() != before;
        settled
    }

    fn note_app_update(&mut self, result: Result<String, String>, now: UnixSecs, settled: &mut Settled) {
        let (level, message) = match result {
            Ok(message) => (Level::Ok, message),
            Err(why) => (Level::Error, format!("App update failed: {why}")),
        };
        self.app_update_status = message.clone();
        self.board.say(now, level, message.clone());
        settled.feedback = Some(message);
    }

    /// The tray could not restart into the update it installed, so it keeps running the build it has. What the user is
    /// told, for the status line.
    pub fn restart_failed(&mut self, why: &str, now: UnixSecs) -> String {
        let message = format!("Could not restart the tray: {why}. Quit and open it again to use the update.");
        self.app_update_status = format!("{} {message}", self.app_update_status).trim_start().to_owned();
        self.board.say(now, Level::Error, message.clone());
        message
    }

    /// This tray was started by the one before it, which installed this build and then quit. The status line says so
    /// once the first look at the skills folder is done, which would otherwise clear it.
    pub fn restarted(&mut self, version: &str, now: UnixSecs) {
        let message = format!("Restarted after the app update. Now running {version}.");
        self.app_update_status = message.clone();
        self.board.say(now, Level::Ok, message.clone());
        self.restart_note = Some(message);
    }

    fn update_due(&self, now: UnixSecs) -> Option<UnixSecs> {
        self.update_clock?.due(self.cfg.app_update, now).map(|t| t.max(self.update_hold))
    }

    pub(crate) fn has_quit_update(&self) -> bool {
        self.staged_for_quit.is_some() && self.cfg.app_update.enabled
    }

    /// When the timer should fire next, or `None` for no timer. Call after every change.
    pub fn arm(&mut self, now: UnixSecs) -> Option<UnixSecs> {
        let at = if self.busy.is_some() || self.quitting || !self.queue.is_empty() {
            None
        } else {
            let skills = if self.started { self.next_wake.map(|wake| wake.max(self.not_before)) } else { Some(self.not_before) };
            skills.into_iter().chain(self.update_due(now)).min()
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
            self.resumed(now);
            return;
        }
        if now >= self.not_before {
            if !self.started {
                self.want(Job::Startup);
            } else if self.next_wake.is_some_and(|wake| wake <= now) {
                self.want(Job::Poll);
            }
        }
        if self.update_due(now).is_some_and(|at| at <= now) {
            self.want(Job::ScheduledUpdate);
        }
    }

    /// The computer woke up: give the network a moment.
    pub fn resumed(&mut self, now: UnixSecs) {
        self.hold(now + RESUME_GRACE);
        self.update_hold = self.update_hold.max(now + RESUME_GRACE);
    }

    /// Windows started the app as the user signed in. Settings load on the worker, but the first look at the disk waits
    /// as long as the first check, which gives the network time to come up, and both are background work. When the
    /// wait is over, the timer queues the look as it does after a failed one.
    pub fn started_by_windows(&mut self, now: UnixSecs) {
        self.at_sign_in = true;
        self.hold(now + SIGN_IN_GRACE);
        self.update_hold = self.update_hold.max(now + SIGN_IN_GRACE);
        if self.queue.contains(&Job::Startup) {
            self.queue.retain(|job| *job != Job::Startup);
            self.sign_in_wait = true;
            self.board.set_waiting(true);
        }
    }

    /// The user opened the window or asked for a job. A first look still waiting after sign-in starts now, at full
    /// speed, since they are waiting for it. A check the timer starts still gives the network its time.
    pub fn user_here(&mut self) {
        if self.sign_in_wait {
            self.at_sign_in = false;
            self.want(Job::Startup);
        }
    }

    /// Nothing more starts; the queue ends with saving what is waiting to be saved.
    pub fn quit(&mut self) {
        self.queue.clear();
        self.queue.push_back(Job::Flush);
        self.quitting = true;
    }

    pub(crate) fn finish_on_quit(&mut self, clean_and_flushed: bool, before: Instant) -> bool {
        if clean_and_flushed && self.quitting && self.has_quit_update() && self.is_idle() && Instant::now() < before {
            self.queue.push_back(Job::InstallOnQuit { before });
            true
        } else {
            false
        }
    }

    fn hold(&mut self, until: UnixSecs) {
        self.not_before = self.not_before.max(until);
    }
}

/// What removing a whole source did, in one line, and whether something now needs the user's eye.
fn removed_text(label: &str, lanes: &[(String, SourceKind, RemoveReport)]) -> (String, bool) {
    let mut text = format!("Removed {label}.");
    let mut deleted = Vec::new();
    let mut kept = Vec::new();
    let mut attention = false;
    for (_, _, report) in lanes {
        if let Some(message) = report.message {
            text.push(' ');
            text.push_str(message);
            continue;
        }
        attention |= !report.kept.is_empty();
        deleted.extend(report.deleted.iter().cloned());
        kept.extend(report.kept.iter().map(|(name, why)| format!("{name} ({why})")));
    }
    if !deleted.is_empty() {
        text.push_str(&format!(" Deleted: {}.", deleted.join(", ")));
    }
    if !kept.is_empty() {
        text.push_str(&format!(" Kept as yours: {}.", kept.join(", ")));
    }
    (text, attention)
}

fn backoff(failures: u32) -> UnixSecs {
    let doublings = failures.saturating_sub(1).min(16);
    (FAILURE_BACKOFF << doublings).min(MAX_FAILURE_BACKOFF)
}

fn shown(job: &Job, result: Result<DiffReport, String>) -> Option<Shown> {
    match job {
        Job::Diff { repo, skill, preview } => Some(Shown { repo: repo.clone(), skill: skill.clone(), preview: *preview, result }),
        _ => None,
    }
}

/// One line about a check the user asked for. The rows say the details. The line is cut at its end when it is too long,
/// so the reason comes first and the repository after it. `all_well` is whether nothing needs the user once the check is
/// taken in: a check that found nothing new does not mean an edit made earlier is settled.
fn summary(report: &SyncReport, all_well: bool, cfg: &Config) -> String {
    // The user sees one entry per source, so a source's types are told as one repository.
    let source = |r: &RepoReport| cfg.find_source(&r.repo_id).map_or_else(|| (r.repo_id.clone(), r.label.clone()), |s| (s.id.clone(), source_label(s)));
    let mut failed = Vec::new();
    // As the entry's "Nothing found": every type the source was checked for listed nothing, and nothing was left to do.
    let mut sources: Vec<(String, String, bool)> = Vec::new();
    for r in &report.repos {
        let (id, label) = source(r);
        if let Outcome::Failed(f) = &r.outcome {
            if !failed.iter().any(|(seen, ..): &(String, String, _)| *seen == id) {
                failed.push((id.clone(), label.clone(), f));
            }
        }
        let empty = r.lists_nothing && r.skills.is_empty() && r.plugins.is_empty();
        match sources.iter_mut().find(|(seen, ..)| *seen == id) {
            Some(entry) => entry.2 &= empty,
            None => sources.push((id, label, empty)),
        }
    }
    let empty: Vec<_> = sources.iter().filter(|(.., empty)| *empty).collect();
    let any = |f: fn(&Outcome) -> bool| report.repos.iter().any(|r| f(&r.outcome));
    // Each sentence names the types of the lanes it is about, so a source that only changed skills says "skills".
    let items = |about: &dyn Fn(&RepoReport) -> bool| {
        crate::board::item_nouns(report.repos.iter().filter(|r| about(r)).filter_map(|r| cfg.find(&r.repo_id).map(|s| s.kind))).1
    };
    let emptied = || items(&|r| empty.iter().any(|(id, ..)| *id == source(r).0));
    match failed.as_slice() {
        _ if report.repos.is_empty() => "There is nothing to check: no repository is enabled".to_string(),
        [(_, label, failure)] => format!("{} ({label})", capitalized(&failure.message)),
        [_, ..] => format!("{} repositories could not be checked", failed.len()),
        [] if any(|o| matches!(o, Outcome::Cancelled)) => "The check was stopped".to_string(),
        [] if any(|o| matches!(o, Outcome::NeedsAttention)) => {
            format!("Checked: some {} need your attention", items(&|r| matches!(r.outcome, Outcome::NeedsAttention)))
        }
        [] if empty.len() == 1 => format!("Checked: no {} found ({})", emptied(), empty[0].1),
        [] if !empty.is_empty() => format!("Checked: no {} found in {} repositories", emptied(), empty.len()),
        [] if any(|o| matches!(o, Outcome::Synced)) => format!("Checked: {} were updated", items(&|r| matches!(r.outcome, Outcome::Synced))),
        [] if all_well => "Checked: everything is up to date".to_string(),
        [] => "Checked: nothing new on GitHub".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RepoSpec;
    use crate::engine::{Failure, RepoReport};
    use crate::github::FailureKind;

    fn repo_report(label: &str, outcome: Outcome) -> RepoReport {
        RepoReport {
            repo_id: label.to_string(),
            label: label.to_string(),
            outcome,
            skills: Vec::new(),
            plugins: Vec::new(),
            rejected: Vec::new(),
            stats: Default::default(),
            downloaded: 0,
            finished_at: 0,
            lists_nothing: false,
        }
    }

    fn checked(repos: Vec<RepoReport>) -> SyncReport {
        SyncReport { repos, notes: Vec::new(), warnings: Vec::new(), seen: Vec::new() }
    }

    fn failed_check(label: &str, message: &str) -> SyncReport {
        let outcome = Outcome::Failed(Failure { kind: FailureKind::Network, message: message.to_string(), retry_at: None });
        checked(vec![repo_report(label, outcome)])
    }

    #[test]
    fn a_failed_check_says_why_first_because_the_status_line_cuts_the_end() {
        let none = Config::default();
        let lost = failed_check("owner/repo@main : skills", "the connection was lost");
        assert_eq!(summary(&lost, false, &none), "The connection was lost (owner/repo@main : skills)");
        let refused = failed_check("owner/repo@main : skills", "GitHub refused access to owner/repo");
        assert_eq!(summary(&refused, false, &none), "GitHub refused access to owner/repo (owner/repo@main : skills)");
    }

    #[test]
    fn a_check_that_found_no_skills_does_not_say_everything_is_up_to_date() {
        let none = Config::default();
        let empty = |label| RepoReport { lists_nothing: true, ..repo_report(label, Outcome::Unchanged) };
        assert_eq!(summary(&checked(vec![empty("o/a"), repo_report("o/b", Outcome::Synced)]), false, &none), "Checked: no skills found (o/a)");
        assert_eq!(summary(&checked(vec![empty("o/a"), empty("o/b")]), false, &none), "Checked: no skills found in 2 repositories");
        // Skills that need the user come first.
        let attention = checked(vec![empty("o/a"), repo_report("o/b", Outcome::NeedsAttention)]);
        assert_eq!(summary(&attention, false, &none), "Checked: some skills need your attention");
        assert_eq!(summary(&checked(vec![repo_report("o/b", Outcome::Unchanged)]), true, &none), "Checked: everything is up to date");
    }

    #[test]
    fn a_check_is_told_per_source_not_per_type() {
        let mut cfg = Config::default();
        let (id, lanes) = crate::sources::allocate_source_ids("kit", &mut cfg.taken_ids());
        let source = crate::sources::SourceConfig { id, repo: "o/kit".into(), path: "skills".into(), lanes, ..Default::default() };
        cfg.add_source_config(source.clone()).unwrap();
        let label = crate::board::source_label(&source);
        let kinds = [SourceKind::Skills, SourceKind::Agents, SourceKind::Instructions, SourceKind::Plugins];
        let lanes = |outcome: fn(SourceKind) -> (Outcome, bool)| {
            checked(
                kinds
                    .iter()
                    .map(|&kind| {
                        let (outcome, lists_nothing) = outcome(kind);
                        RepoReport { repo_id: source.lanes.id(kind).unwrap().into(), lists_nothing, ..repo_report(&format!("{kind:?} lane"), outcome) }
                    })
                    .collect(),
            )
        };
        let nothing = lanes(|_| (Outcome::Unchanged, true));
        assert_eq!(summary(&nothing, false, &cfg), format!("Checked: no items found ({label})"));
        // A type the source does not have is no news while another type of it was installed, and the line names only
        // the type that changed, as the band above it does.
        let only_skills = lanes(|kind| if kind == SourceKind::Skills { (Outcome::Synced, false) } else { (Outcome::Unchanged, true) });
        assert_eq!(summary(&only_skills, false, &cfg), "Checked: skills were updated");
        let both = lanes(|kind| match kind {
            SourceKind::Skills | SourceKind::Agents => (Outcome::Synced, false),
            _ => (Outcome::Unchanged, true),
        });
        assert_eq!(summary(&both, false, &cfg), "Checked: items were updated");
        let agent_waits = lanes(|kind| match kind {
            SourceKind::Skills => (Outcome::Synced, false),
            SourceKind::Agents => (Outcome::NeedsAttention, false),
            _ => (Outcome::Unchanged, true),
        });
        assert_eq!(summary(&agent_waits, false, &cfg), "Checked: some agents need your attention");
        let lost = Failure { kind: FailureKind::Network, message: "the connection was lost".into(), retry_at: None };
        let failed = checked(
            kinds.iter().map(|&k| RepoReport { repo_id: source.lanes.id(k).unwrap().into(), ..repo_report("lane", Outcome::Failed(lost.clone())) }).collect(),
        );
        assert_eq!(summary(&failed, false, &cfg), format!("The connection was lost ({label})"));
    }

    #[test]
    fn the_wait_after_failures_doubles_up_to_an_hour() {
        let waits: Vec<UnixSecs> = (1..=7).map(backoff).collect();
        assert_eq!(waits, [300, 600, 1200, 2400, 3600, 3600, 3600]);
        assert_eq!(backoff(u32::MAX), MAX_FAILURE_BACKOFF);
    }

    #[test]
    fn historical_choices_run_before_queued_checks() {
        let mut app = App::new(Config::default(), PathBuf::from("skills"));
        app.queue.clear();
        app.want(Job::Check(None));
        let choice = Job::Resolve { repo: "r".into(), skill: "alpha".into(), how: Resolution::PauseHistory };
        app.want(choice.clone());
        assert_eq!(app.next_order().unwrap().job, choice);
        assert!(app.next_order().is_none(), "only one worker owns the engine");
    }

    #[test]
    fn app_deadline_survives_a_busy_skill_job_with_no_extra_timer_or_poll() {
        let mut app = App::new(Config::default(), PathBuf::from("skills"));
        app.queue.clear();
        app.started = true;
        app.update_clock = Some(ClockState::default());
        app.next_wake = Some(50);
        assert_eq!(app.arm(50), Some(50));
        app.on_timer(50);
        assert_eq!(app.next_order().unwrap().job, Job::Poll);
        assert_eq!(app.queued(), &[Job::ScheduledUpdate]);
        assert_eq!(app.arm(55), None);
        assert!(app.next_order().is_none());
        app.busy = None;
        assert_eq!(app.next_order().unwrap().job, Job::ScheduledUpdate);
        app.busy = None;
        app.next_wake = None;
        app.not_before = 9999; // failed skill job or no configured repositories
        assert_eq!(app.arm(61), Some(61));
        app.resumed(61);
        assert_eq!(app.arm(61), Some(61 + RESUME_GRACE));
        app.on_timer(62);
        assert!(app.is_idle());
        app.on_timer(61 + RESUME_GRACE);
        assert_eq!(app.next_order().unwrap().job, Job::ScheduledUpdate);
    }

    #[test]
    fn app_checks_honor_sign_in_grace_even_after_a_user_opens_the_window() {
        let mut app = App::new(Config::default(), PathBuf::from("skills"));
        app.started_by_windows(100);
        app.user_here();
        assert_eq!(app.next_order().unwrap().job, Job::Startup);
        app.busy = None;
        app.started = true;
        app.update_clock = Some(ClockState::default());
        assert_eq!(app.arm(101), Some(100 + SIGN_IN_GRACE));
        app.on_timer(100 + SIGN_IN_GRACE);
        assert_eq!(app.next_order().unwrap().job, Job::ScheduledUpdate);
    }

    #[test]
    fn disabling_discards_a_queued_app_update_and_revokes_a_late_staged_answer() {
        let mut app = App::new(Config::default(), PathBuf::from("skills"));
        app.cfg.app_update.timing = crate::config::UpdateTiming::OnQuit;
        app.queue.clear();
        app.started = true;
        app.update_clock = Some(ClockState::default());
        app.staged_for_quit = Some("1.2.3".parse().unwrap());
        let generation = app.update_generation;
        app.want(Job::ScheduledUpdate);
        let mut off = app.cfg().clone();
        off.app_update.enabled = false;
        app.set_cfg(off, app.skills_dir.clone());
        assert!(!app.queued().contains(&Job::ScheduledUpdate));
        let delayed = Done {
            job: Job::ScheduledUpdate,
            result: Ok(Answer::AppUpdate("staged before Off".into())),
            update: Some(UpdateState { generation, clock: app.update_clock, staged: Some("1.2.3".parse().unwrap()), status: None, disk: None, restart: None }),
            seen: None,
            next_wake: None,
            last_checks: vec![],
            kept: vec![],
            facts: vec![],
            plugin_records: vec![],
            lists_nothing: vec![],
            private: vec![],
            stamp: None,
            settings: None,
            config_saves: vec![],
            settings_checked: true,
        };
        assert!(app.finish(delayed, 100).feedback.is_none());
        assert!(!app.has_quit_update());
        app.queue.clear();
        assert_eq!(app.arm(101), None);
        assert!(app.want(Job::AppUpdate(Action::Check)));
    }

    #[test]
    fn plugin_app_does_not_queue_the_first_check_when_approval_fails_after_save() {
        let mut app = App::new(Config::default(), PathBuf::from("unused"));
        app.queue.clear();
        app.preview_location("https://github.com/o/r/tree/main", None).unwrap();
        app.queue.clear();
        let draft = app.draft.as_mut().unwrap();
        draft.stage = DraftStage::Saving;
        let source = SourceConfig {
            id: "o-r".into(),
            repo: "o/r".into(),
            branch: "main".into(),
            lanes: crate::sources::Lanes::default().with(SourceKind::Plugins, "o-r".into()),
            ..SourceConfig::default()
        };
        let preview = LocationPreview {
            source: source.clone(),
            branch: "main".into(),
            follows_default: false,
            types: crate::sources::Types::default(),
            inventory: crate::inventory::Inventory::default(),
            destinations: vec![],
        };
        let job = Job::SaveSource(draft.input.clone(), Arc::new(PreviewSnapshot::new(preview)), Box::new(draft.settings.clone()), true);
        let message = "The source was saved, but its plugins could not be approved.";
        let done = app_update_done(job, app.update_generation, Ok(Answer::SourceSaved { source, approved: Err(message.into()) }));
        assert_eq!(app.finish(done, 10).feedback.as_deref(), Some(message));
        assert!(app.cfg.find_source("o-r").is_some(), "the source was already saved");
        assert!(app.queue.is_empty(), "do not start the first check after this refusal");
    }

    #[test]
    fn plugin_choice_feedback_names_the_chosen_plugins_new_tag() {
        use crate::engine::{ApplyResult, PluginReport};
        use crate::plugins::{Operation, Status};
        let mut cfg = Config::default();
        let id = cfg.add_source(SourceKind::Plugins, &RepoSpec::parse("https://github.com/o/r/tree/main").unwrap(), None, false).id.clone();
        let mut app = App::new(cfg, PathBuf::from("unused"));
        let job = Job::ResolvePlugin { repo: id.clone(), choice: Choice::Approve { name: "alpha".into(), publisher: None } };
        let plugin =
            |name: &str, status, result| PluginReport { name: name.into(), version: None, publisher: None, path: None, status, choices: Vec::new(), result };
        let report = RepoReport {
            repo_id: id,
            plugins: vec![
                plugin("beta", Status::NeedsApproval, ApplyResult::NotApplicable),
                plugin("alpha", Status::Planned(Operation::Approve), ApplyResult::Done),
            ],
            ..repo_report("o/r", Outcome::NeedsAttention)
        };
        let done = app_update_done(job, app.update_generation, Ok(Answer::PluginResolved(report)));
        assert_eq!(app.finish(done, 10).feedback.as_deref(), Some("alpha: Up to date."));
    }

    #[test]
    fn plugin_choice_errors_reach_the_source_detail_even_without_a_report() {
        let mut cfg = Config::default();
        let id = cfg.add_source(SourceKind::Plugins, &RepoSpec::parse("https://github.com/o/r/tree/main").unwrap(), None, false).id.clone();
        let mut app = App::new(cfg, PathBuf::from("unused"));
        let job = Job::ResolvePlugin { repo: id, choice: Choice::Uninstall("alpha".into()) };
        let why = "Could not save the plugin record.";
        let done = app_update_done(job, app.update_generation, Err(EngineError::Failed(why.into())));
        assert_eq!(app.finish(done, 10).feedback.as_deref(), Some(why));
        let view = app.board.views(&app.cfg, 10).remove(0);
        assert_eq!(view.detail.as_deref(), Some(why));
        assert_eq!(view.level, Level::Error);
    }

    #[test]
    fn turning_a_type_on_checks_its_lane_now() {
        let mut before = Config::default();
        let (id, lanes) = crate::sources::allocate_source_ids("s", &mut before.taken_ids());
        let mut source = SourceConfig { id: id.clone(), repo: "o/s".into(), lanes, ..SourceConfig::default() };
        source.set_type(SourceKind::Agents, Some(false));
        before.add_source_config(source).unwrap();
        let mut after = before.clone();
        after.edit(|sources, _| sources[0].set_type(SourceKind::Agents, Some(true)));
        let lane = |kind| after.find_source(&id).and_then(|s| s.lanes.id(kind)).unwrap().to_owned();
        let (agents, skills) = (lane(SourceKind::Agents), lane(SourceKind::Skills));
        assert!(before.find(&agents).is_none() && after.find(&agents).is_some_and(|r| r.enabled));

        let mut app = App::new(before.clone(), PathBuf::from("unused"));
        app.queue.clear();
        let mut done = app_update_done(Job::Plan, app.update_generation, Ok(Answer::Planned));
        done.config_saves = vec![ConfigSave { before, after: after.clone(), portable: false, revert_on_failure: false }.outcome(Ok(()))];
        app.finish(done, 10);
        assert_eq!(app.cfg, after);
        assert!(app.queue.contains(&Job::Check(Some(agents))), "the lane turned on is checked: {:?}", app.queue);
        assert!(!app.queue.contains(&Job::Check(Some(skills))), "a lane that was on already waits for its schedule");
    }

    fn app_update_done(job: Job, generation: u64, result: Result<Answer, EngineError>) -> Done {
        Done {
            job,
            result,
            update: Some(UpdateState { generation, clock: None, staged: None, status: None, disk: None, restart: None }),
            seen: None,
            next_wake: None,
            last_checks: vec![],
            kept: vec![],
            facts: vec![],
            plugin_records: vec![],
            lists_nothing: vec![],
            private: vec![],
            stamp: None,
            settings: None,
            config_saves: vec![],
            settings_checked: false,
        }
    }

    #[test]
    fn only_an_install_the_user_asked_for_or_a_current_schedule_restarts_the_tray() {
        let mut app = App::new(Config::default(), PathBuf::from("skills"));
        let generation = app.update_generation;
        let installed = || Ok(Answer::AppUpdate("Installed 1.2.3 on disk (both programs).".into()));
        let mut restarts = |job: Job, generation: u64, result| app.finish(app_update_done(job, generation, result), 100).restart;
        assert!(restarts(Job::AppUpdate(Action::Install), generation, installed()));
        assert!(restarts(Job::ScheduledUpdate, generation, installed()));
        // The settings changed while it ran: the answer is stale, whatever it did.
        assert!(!restarts(Job::ScheduledUpdate, generation.wrapping_add(1), installed()));
        assert!(!restarts(Job::AppUpdate(Action::Check), generation, installed()));
        assert!(!restarts(Job::InstallOnQuit { before: Instant::now() }, generation, installed()));
        assert!(!restarts(Job::AppUpdate(Action::Install), generation, Err(EngineError::Failed("the download failed".into()))));
        assert!(!restarts(Job::Check(None), generation, Ok(Answer::Flushed)));
        app.quit();
        for job in [Job::AppUpdate(Action::Install), Job::ScheduledUpdate] {
            assert!(!app.finish(app_update_done(job, generation, installed()), 100).restart);
        }
    }

    #[test]
    fn a_restarted_tray_says_so_when_its_first_look_is_done() {
        let started = || {
            let tracking = crate::engine::Tracking { check: crate::verify::Check::Full, volume_changed: false, journal: false, folder_missing: false };
            let verify = crate::engine::VerifyReport {
                skills: vec![],
                lanes: vec![],
                orphans: vec![],
                stats: Default::default(),
                tracking,
                notes: vec![],
                warnings: vec![],
            };
            let report = crate::engine::StartupReport { verify, problem: None };
            Done { update: None, ..app_update_done(Job::Startup, 0, Ok(Answer::Started(report))) }
        };
        let mut app = App::new(Config::default(), PathBuf::from("skills"));
        app.restarted("1.2.3", 100);
        assert_eq!(app.finish(started(), 101).feedback.as_deref(), Some("Restarted after the app update. Now running 1.2.3."));
        assert_eq!(app.finish(started(), 102).feedback, None, "said once");
    }

    #[test]
    fn an_engine_error_reads_as_a_sentence() {
        assert_eq!(capitalized("another check"), "Another check");
        assert_eq!(capitalized(""), "");
    }
}
