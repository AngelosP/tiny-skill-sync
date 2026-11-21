//! The tray app's cycle without its windows: an `App` hands orders to a real engine on a fake GitHub and takes the
//! answers back, as the window and the worker thread do.

mod common;

use std::path::PathBuf;

use common::*;
use tiny_skill_sync::app::{
    self, Answer, App, BUSY_RETRY, Done, DraftStage, FAILURE_BACKOFF, Job, MIN_POLL_GAP, RESUME_GRACE, SIGN_IN_GRACE, SettingsSource, Settled,
};
use tiny_skill_sync::board::{Action, Health, Level, Row};
use tiny_skill_sync::config::Config;
use tiny_skill_sync::engine::{self, Engine, EngineError, Resolution, SyncReport};
use tiny_skill_sync::interval::{Interval, Unit};
use tiny_skill_sync::reconcile::Verdict;
use tiny_skill_sync::source::SourceKind;

const DAY: i64 = 24 * 60 * 60;

#[test]
fn historical_search_rows_and_timer_survive_restart_and_pause() {
    let w = World::new("historical-app");
    let remote = FakeRemote::new();
    remote.put(REPO, "skills/alpha/SKILL.md", "old");
    for n in 0..25 {
        remote.put(REPO, "skills/alpha/SKILL.md", &format!("new {n}"));
    }
    w.write("alpha/SKILL.md", "old");
    let cfg = standard_config();
    let (mut engine, _) = w.engine_at(&remote, &FakeTokens::none());
    let mut app = App::new(cfg.clone(), w.skills.clone());
    run(&mut app, &mut engine, T0);
    app.want(Job::Check(None));
    run(&mut app, &mut engine, T0);
    assert_eq!(row(&app, "alpha").tag, "Searching history");
    assert_eq!(app.arm(T0), Some(T0 + 30));
    app.on_timer(T0);
    assert!(app.queued().is_empty());
    app.want(Job::Resolve { repo: fx(), skill: "alpha".into(), how: Resolution::PauseHistory });
    run(&mut app, &mut engine, T0);
    let (mut engine, _) = w.engine_at(&remote, &FakeTokens::none());
    let mut app = App::new(cfg, w.skills.clone());
    run(&mut app, &mut engine, T0);
    assert_eq!(row(&app, "alpha").tag, "Search paused");
    assert_eq!(row(&app, "alpha").actions, [Action::ResumeHistory, Action::Overwrite, Action::KeepMine]);
}

/// Runs every waiting job to the end. Returns what each one settled.
fn run(app: &mut App, engine: &mut Engine, now: i64) -> Vec<(Job, Settled)> {
    let mut out = Vec::new();
    while let Some(order) = app.next_order() {
        let mut done = app::perform(engine, &order, &cancel_flag());
        if let Some(settings) = done.settings.take() {
            if let Ok(loaded) = settings.result {
                app.settings_reloaded(loaded, &done.job);
            }
        }
        out.push((order.job, app.finish(done, now)));
    }
    out
}

/// Runs the next job. Returns it, and whether it ran as background work.
fn run_next(app: &mut App, engine: &mut Engine, now: i64) -> (Job, bool) {
    let order = app.next_order().expect("a job is waiting");
    let mut done = app::perform(engine, &order, &cancel_flag());
    if let Some(settings) = done.settings.take() {
        if let Ok(loaded) = settings.result {
            app.settings_reloaded(loaded, &done.job);
        }
    }
    app.finish(done, now);
    (order.job, order.background)
}

/// What the last job settled.
fn last(mut settled: Vec<(Job, Settled)>) -> Settled {
    settled.pop().expect("a job ran").1
}

/// Answers the next job without an engine, as an engine that fails would.
fn answer(app: &mut App, result: Result<Answer, EngineError>, next_wake: Option<i64>, now: i64) -> Settled {
    let order = app.next_order().expect("a job is waiting");
    app.finish(
        Done {
            job: order.job,
            result,
            next_wake,
            last_checks: Vec::new(),
            kept: Vec::new(),
            facts: Vec::new(),
            plugin_records: Vec::new(),
            lists_nothing: Vec::new(),
            private: Vec::new(),
            stamp: None,
            settings: None,
            config_saves: Vec::new(),
            settings_checked: false,
            update: None,
            seen: None,
        },
        now,
    )
}

fn broken() -> Result<Answer, EngineError> {
    Err(EngineError::Failed("cannot read state.json".to_string()))
}

fn checked_nothing() -> Result<Answer, EngineError> {
    Ok(Answer::Checked { verified: None, sync: SyncReport { repos: Vec::new(), notes: Vec::new(), warnings: Vec::new(), seen: Vec::new() } })
}

fn app_for(s: &Setup) -> App {
    App::new(s.cfg.clone(), s.w.skills.clone())
}

/// No engine behind it: for the rules about the queue and the timer.
fn bare() -> App {
    App::new(standard_config(), PathBuf::from("not used"))
}

/// The app has run its startup job.
fn started(s: &mut Setup) -> App {
    let mut app = app_for(s);
    run(&mut app, &mut s.engine, T0);
    assert!(app.started());
    app
}

fn row(app: &App, name: &str) -> Row {
    let view = app.board.views(app.cfg(), T0).remove(0);
    view.skills.into_iter().find(|r| r.name == name).unwrap_or_else(|| panic!("no row for {name}"))
}

fn fx() -> String {
    "fx".to_string()
}

fn portable_source(s: &Setup, cfg: &Config) -> SettingsSource {
    let exe_dir = s.w.root.join("portable-bin");
    std::fs::create_dir_all(&exe_dir).unwrap();
    write_portable(&exe_dir, cfg);
    SettingsSource::new(s.w.home.join(tiny_skill_sync::config::CONFIG_FILE), exe_dir)
}

fn write_portable(exe_dir: &std::path::Path, cfg: &Config) {
    let settings = serde_json::json!({
        "version": 1,
        "default_interval": serde_json::to_value(cfg.default_interval).unwrap(),
        "skills_dir": cfg.skills_dir
    });
    let sources = serde_json::json!({
        "version": 1,
        "repos": serde_json::to_value(cfg.repos()).unwrap()
    });
    std::fs::write(exe_dir.join(tiny_skill_sync::portable::SETTINGS_FILE), serde_json::to_vec_pretty(&settings).unwrap()).unwrap();
    std::fs::write(exe_dir.join(tiny_skill_sync::portable::SOURCES_FILE), serde_json::to_vec_pretty(&sources).unwrap()).unwrap();
}

#[test]
fn source_preview_dismissal_failure_and_late_answers_cannot_save() {
    let mut s = Setup::fresh("app-source-preview");
    let mut app = started(&mut s);
    s.remote.put("o/new", "alpha/SKILL.md", "one");
    let before = s.w.snapshot();
    assert!(app.preview_location("o/new/extra", None).is_err(), "a malformed location is refused at once");
    assert!(app.queued().is_empty());
    let first = app.preview_location("o/new", None).unwrap();
    let order = app.next_order().unwrap();
    let late = app::perform(&mut s.engine, &order, &cancel_flag());
    app.dismiss_source();
    let second = app.preview_location("https://github.com/o/new", None).unwrap();
    app.finish(late, T0);
    assert!(matches!(app.draft().unwrap().stage, DraftStage::Loading));
    assert!(app.confirm_source(first, false).is_err());
    assert!(app.confirm_source(second, false).is_err());
    answer(&mut app, broken(), None, T0);
    assert!(matches!(app.draft().unwrap().stage, DraftStage::Failed(_)));
    assert!(app.confirm_source(second, false).is_err());
    assert!(app.queued().is_empty());

    let ticket = app.preview_location("o/new", None).unwrap();
    run_next(&mut app, &mut s.engine, T0);
    let DraftStage::Reviewed(preview) = &app.draft().unwrap().stage else { panic!("expected a finished preview") };
    assert_eq!((preview.source.repo.as_str(), preview.branch.as_str(), preview.follows_default), ("o/new", "main", true));
    assert_eq!(preview.inventory.skills[0].install_name, "alpha");
    let mut empty = (**preview).clone();
    empty.inventory.skills.clear();
    app.dismiss_source();
    assert!(app.confirm_source(ticket, false).is_err());

    let ticket = app.preview_location("o/new", None).unwrap();
    answer(&mut app, Ok(Answer::SourcePreview(empty.into())), None, T0);
    assert!(matches!(app.draft().unwrap().stage, DraftStage::Reviewed(_)));
    assert!(app.confirm_source(ticket, false).is_err(), "nothing would sync");
    assert!(app.is_idle());
    assert_eq!(s.w.snapshot(), before);
}

/// An app with local settings in a file, on a fake GitHub with a marketplace plugin and a skill in `o/fixtures`.
fn plugin_app(name: &str) -> (World, std::sync::Arc<FakeRemote>, FakePluginManager, Engine, App, PathBuf) {
    let w = World::new(name);
    let remote = FakeRemote::new();
    let manager = FakePluginManager::new(w.root.join("native"));
    let (mut engine, _) = w.plugin_engine(&remote, &manager);
    let file = w.home.join(tiny_skill_sync::config::CONFIG_FILE);
    let dir = w.root.join("exe");
    std::fs::create_dir_all(&dir).unwrap();
    let initial = Config { skills_dir: Some(w.skills.to_string_lossy().into_owned()), ..Config::default() };
    initial.save(&file).unwrap();
    let mut app = App::new(initial, w.skills.clone());
    app.set_settings_source(SettingsSource::new(file.clone(), dir));
    run(&mut app, &mut engine, T0);
    remote.put(REPO, "marketplace.json", r#"{"name":"tools","plugins":[{"name":"alpha","source":"./plug"}]}"#);
    remote.put(REPO, "plug/plugin.json", r#"{"name":"alpha","version":"1","author":{"name":"Review Team"}}"#);
    remote.put(REPO, "skills/beta/SKILL.md", "beta");
    (w, remote, manager, engine, app, file)
}

#[test]
fn plugin_app_approves_the_reviewed_plugins_only_after_saving() {
    let (w, _remote, manager, mut engine, mut app, file) = plugin_app("app-plugin-save");
    let ticket = app.preview_location(REPO, None).unwrap();
    run_next(&mut app, &mut engine, T0);
    assert!(manager.calls().is_empty());
    assert!(w.state().plugin_sources().is_empty());
    // Refuse replacement while still allowing the worker to load and recheck settings.
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
    let held = std::fs::OpenOptions::new().read(true).share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE).open(&file).unwrap();
    app.confirm_source(ticket, true).unwrap();
    run_next(&mut app, &mut engine, T0);
    assert!(matches!(app.draft().unwrap().stage, DraftStage::Failed(_)));
    assert!(manager.calls().is_empty(), "a refused save never records approval");
    assert!(w.state().plugin_sources().is_empty());
    drop(held);

    let ticket = app.preview_location(REPO, None).unwrap();
    run_next(&mut app, &mut engine, T0);
    let DraftStage::Reviewed(preview) = &app.draft().unwrap().stage else { panic!("expected a completed preview") };
    let preview = preview.clone();
    let id = preview.source.id.clone();
    let plugins = preview.source.lanes.id(SourceKind::Plugins).unwrap().to_owned();
    let skills = preview.source.lanes.id(SourceKind::Skills).unwrap().to_owned();
    app.confirm_source(ticket, true).unwrap();
    assert!(matches!(app.queued().front(), Some(Job::SaveSource(_, reviewed, true)) if std::sync::Arc::ptr_eq(reviewed, &preview)));
    run_next(&mut app, &mut engine, T0);
    assert!(Config::load_sources(&file).unwrap().find_source(&id).is_some());
    assert_eq!(w.state().plugin_sources()[&plugins].approved, [("alpha".into(), Some("Review Team".into()))].into());
    assert_eq!(manager.calls(), [PluginCall::Home]);
    assert_eq!(app.queued().front(), Some(&Job::Check(Some(skills))));
}

#[test]
fn plugin_app_adds_without_approval_and_never_asks_copilot() {
    let (w, _remote, manager, mut engine, mut app, file) = plugin_app("app-plugin-no-approval");
    let ticket = app.preview_location(REPO, None).unwrap();
    run_next(&mut app, &mut engine, T0);
    let DraftStage::Reviewed(preview) = &app.draft().unwrap().stage else { panic!("expected a completed preview") };
    let id = preview.source.id.clone();
    app.confirm_source(ticket, false).unwrap();
    let (job, _) = run_next(&mut app, &mut engine, T0);
    assert!(matches!(job, Job::SaveSource(_, _, false)));
    let saved = Config::load_sources(&file).unwrap();
    assert!(saved.find_source(&id).is_some());
    assert!(manager.calls().is_empty(), "adding without approval never launches Copilot CLI");
    assert!(w.state().plugin_sources().values().all(|r| r.approved.is_empty()));
}
#[test]
fn plugin_app_keeps_attention_rows_and_choices_on_a_timer_304() {
    use std::sync::atomic::Ordering;

    let w = World::new("app-plugin-304");
    let remote = FakeRemote::new();
    remote.put(REPO, "marketplace.json", r#"{"name":"tools","plugins":[{"name":"alpha","source":"./plug"}]}"#);
    remote.put(REPO, "plug/plugin.json", r#"{"name":"alpha","version":"1","author":{"name":"Review Team"}}"#);
    let cfg = config(vec![tiny_skill_sync::config::RepoConfig { kind: SourceKind::Plugins, branch: "main".into(), ..repo("fx", REPO, "") }]);
    let manager = FakePluginManager::new(w.root.join("native"));
    let (mut engine, clock) = w.plugin_engine(&remote, &manager);
    let mut app = App::new(cfg, w.skills.clone());
    run(&mut app, &mut engine, T0);
    app.want(Job::Check(None)); // Sync now
    run(&mut app, &mut engine, T0);
    let review = row(&app, "alpha");
    assert_eq!(review.actions, [Action::ApprovePlugin]);
    assert_eq!(app.health(), Health::Attention);
    let count = manager.calls().len();
    let at = app.arm(T0).unwrap();
    clock.store(at, Ordering::Relaxed);
    remote.reset();
    app.on_timer(at);
    assert_eq!(run_next(&mut app, &mut engine, at), (Job::Poll, true));
    assert_eq!(row(&app, "alpha"), review);
    assert_eq!(app.health(), Health::Attention);
    assert_eq!(remote.calls().conditional, 1);
    assert_eq!(manager.calls().len(), count);

    app.want(Job::Check(None));
    remote.reset();
    assert_eq!(run_next(&mut app, &mut engine, at), (Job::Check(None), false));
    assert_eq!(manager.calls().len(), count, "plugins nobody approved are shown without asking Copilot CLI");
    assert_eq!(remote.calls().conditional, 1, "with nothing approved, an unchanged source costs one request like any other");
    assert_eq!(row(&app, "alpha"), review);
}

#[test]
fn plugin_app_add_reserves_fresh_ids_and_refuses_one_taken_before_saving() {
    use tiny_skill_sync::state::State;

    let (w, _remote, manager, mut engine, mut app, file) = plugin_app("app-plugin-add-id");
    let mut state = State::default();
    state.repo_mut("o-fixtures").last_check = Some(T0);
    state.save(&w.state_file()).unwrap();
    let lanes = |app: &App| {
        let DraftStage::Reviewed(preview) = &app.draft().unwrap().stage else { panic!("expected a finished preview") };
        let ids: Vec<String> = [SourceKind::Skills, SourceKind::Agents, SourceKind::Instructions, SourceKind::Plugins]
            .into_iter()
            .filter_map(|kind| preview.source.lanes.id(kind).map(str::to_owned))
            .collect();
        (preview.source.id.clone(), ids)
    };
    let ticket = app.preview_location(REPO, None).unwrap();
    run_next(&mut app, &mut engine, T0);
    let (_, ids) = lanes(&app);
    assert!(!ids.is_empty() && !ids.contains(&"o-fixtures".to_string()), "{ids:?}");

    // Another writer may take a lane ID before confirmation, even with no settings entry.
    let mut occupied = state.clone();
    occupied.repo_mut(&ids[0]).last_check = Some(T0);
    occupied.save(&w.state_file()).unwrap();
    let before = w.snapshot();
    app.confirm_source(ticket, true).unwrap();
    run_next(&mut app, &mut engine, T0);
    assert!(matches!(&app.draft().unwrap().stage, DraftStage::Failed(why) if why.contains("again")), "{:?}", app.draft().unwrap().stage);
    assert!(manager.calls().is_empty());
    assert_eq!(w.snapshot(), before);

    let ticket = app.preview_location(REPO, None).unwrap();
    run_next(&mut app, &mut engine, T0);
    let (id, fresh) = lanes(&app);
    assert!(fresh.iter().all(|lane| !ids.contains(lane)), "{fresh:?} {ids:?}");
    app.confirm_source(ticket, false).unwrap();
    run_next(&mut app, &mut engine, T0);
    assert!(Config::load_sources(&file).unwrap().find_source(&id).is_some());
    assert_eq!(w.state().repo("o-fixtures"), occupied.repo("o-fixtures"));
    assert_eq!(w.state().repo(&ids[0]), occupied.repo(&ids[0]));
}

#[test]
fn a_listing_that_changed_after_the_preview_saves_nothing() {
    let (w, remote, manager, mut engine, mut app, file) = plugin_app("app-source-changed");
    let ticket = app.preview_location(REPO, None).unwrap();
    run_next(&mut app, &mut engine, T0);
    remote.put(REPO, "skills/gamma/SKILL.md", "gamma");
    let before = w.snapshot();
    let saved = std::fs::read(&file).unwrap();
    app.confirm_source(ticket, true).unwrap();
    run_next(&mut app, &mut engine, T0);
    assert!(matches!(&app.draft().unwrap().stage, DraftStage::Failed(why) if why.contains("changed since the preview")), "{:?}", app.draft().unwrap().stage);
    assert_eq!(std::fs::read(&file).unwrap(), saved, "the settings were not touched");
    assert!(app.cfg().sources().is_empty());
    assert!(manager.calls().is_empty());
    assert_eq!(w.snapshot(), before);
    assert!(app.queued().is_empty());
}

#[test]
fn a_location_already_set_up_is_refused_without_saving() {
    let mut s = Setup::fresh("app-source-duplicate");
    s.cfg = config(vec![repo("nw", "o/new", ""), tiny_skill_sync::config::RepoConfig { branch: "main".into(), ..repo("fx", REPO, "skills") }]);
    let mut app = started(&mut s);
    let before = s.w.snapshot();
    app.preview_location("O/New", None).unwrap();
    assert!(app.queued().is_empty(), "a bare duplicate needs no worker job");
    assert!(matches!(&app.draft().unwrap().stage, DraftStage::AlreadySetUp(id) if id == "nw"));

    let ticket = app.preview_location("https://github.com/o/fixtures/tree/main/skills", None).unwrap();
    run_next(&mut app, &mut s.engine, T0);
    assert!(matches!(&app.draft().unwrap().stage, DraftStage::AlreadySetUp(id) if id == "fx"), "{:?}", app.draft().unwrap().stage);
    assert!(app.confirm_source(ticket, false).is_err());
    assert!(app.queued().is_empty());
    assert_eq!(s.w.snapshot(), before);
}

#[test]
fn source_preview_portable_save_must_finish_before_a_check_is_queued() {
    let mut s = Setup::fresh("app-source-save");
    let shared = Config { skills_dir: Some(s.w.skills.to_string_lossy().into_owned()), ..s.cfg.clone() };
    let store = portable_source(&s, &shared);
    let dir = store.executable_dir.clone();
    let mut app = App::new(Config::default(), s.w.skills.clone());
    app.set_settings_source(store);
    run(&mut app, &mut s.engine, T0);
    s.remote.put("o/new", "one/SKILL.md", "one");
    for conflict in [true, false] {
        let before = s.w.snapshot();
        let ticket = app.preview_location("o/new", None).unwrap();
        run_next(&mut app, &mut s.engine, T0);
        assert_eq!(s.w.snapshot(), before, "the preview changed a file");
        let DraftStage::Reviewed(preview) = &app.draft().unwrap().stage else { panic!("expected a finished preview") };
        let skills = preview.source.lanes.id(SourceKind::Skills).unwrap().to_owned();
        app.confirm_source(ticket, false).unwrap();
        assert!(matches!(app.queued().as_slices(), ([Job::SaveSource(..)], [])));
        assert_eq!(app.cfg(), &shared);
        if conflict {
            let mut other = shared.clone();
            other.clear_sources();
            write_portable(&dir, &other);
        }
        run_next(&mut app, &mut s.engine, T0);
        if conflict {
            assert!(app.is_idle() && matches!(&app.draft().unwrap().stage, DraftStage::Failed(why) if why.contains("settings changed")));
            assert_eq!(app.cfg(), &shared);
            assert!(tiny_skill_sync::portable::load_sources(&dir).unwrap().unwrap().repos().is_empty());
            write_portable(&dir, &shared);
        } else {
            assert_eq!(app.queued(), &[Job::Check(Some(skills))]);
            assert_eq!(app.cfg().sources().len(), 2);
            assert_eq!(tiny_skill_sync::portable::load_sources(&dir).unwrap().as_ref(), Some(app.cfg()));
        }
        assert!(s.w.skill_names().is_empty());
    }
}

#[test]
fn dismissing_a_source_during_portable_save_keeps_the_save_and_following_check() {
    let mut s = Setup::fresh("app-source-save-dismiss");
    let shared = Config { skills_dir: Some(s.w.skills.to_string_lossy().into_owned()), ..s.cfg.clone() };
    let store = portable_source(&s, &shared);
    let dir = store.executable_dir.clone();
    let mut app = App::new(Config::default(), s.w.skills.clone());
    app.set_settings_source(store);
    run(&mut app, &mut s.engine, T0);

    s.remote.put("o/new", "one/SKILL.md", "one");
    let ticket = app.preview_location("o/new", Some(Interval::parse("6h").unwrap())).unwrap();
    run_next(&mut app, &mut s.engine, T0);
    let DraftStage::Reviewed(preview) = &app.draft().unwrap().stage else { panic!("expected a finished preview") };
    let (id, skills) = (preview.source.id.clone(), preview.source.lanes.id(SourceKind::Skills).unwrap().to_owned());
    app.confirm_source(ticket, false).unwrap();
    let order = app.next_order().unwrap();
    assert!(matches!(order.job, Job::SaveSource(..)));

    app.dismiss_source();
    assert!(app.saving_source());
    let settled = app.finish(app::perform(&mut s.engine, &order, &cancel_flag()), T0);

    assert!(settled.feedback.as_deref().is_some_and(|text| text.starts_with("Added ")));
    assert!(app.draft().is_none());
    assert_eq!(app.cfg().sources().len(), shared.sources().len() + 1);
    assert_eq!(app.cfg().find_source(&id).unwrap().interval, Some(Interval::parse("6h").unwrap()));
    assert_eq!(tiny_skill_sync::portable::load_sources(&dir).unwrap().as_ref(), Some(app.cfg()));
    assert_eq!(app.queued(), &[Job::Check(Some(skills.clone()))]);

    assert_eq!(run_next(&mut app, &mut s.engine, T0).0, Job::Check(Some(skills)));
    assert_eq!(s.w.read("one/SKILL.md"), "one");
}

#[test]
fn source_and_global_edits_are_checked_then_saved_by_the_worker() {
    use tiny_skill_sync::sources::IgnoreAction;

    let mut s = Setup::fresh("app-source-edit");
    let file = s.w.home.join(tiny_skill_sync::config::CONFIG_FILE);
    let initial = Config { skills_dir: Some(s.w.skills.to_string_lossy().into_owned()), ..s.cfg.clone() };
    initial.save(&file).unwrap();
    let mut app = App::new(initial.clone(), s.w.skills.clone());
    app.set_settings_source(SettingsSource::new(file.clone(), s.w.root.join("exe")));
    run(&mut app, &mut s.engine, T0);

    assert!(app.edit_source("fx", |source| source.ignore_add.push("a/b".into())).is_err(), "a folder name cannot hold a slash");
    assert!(app.edit_global(|global| global.ignore_folders.push("..".into())).is_err());
    assert!(app.edit_source("missing", |_| {}).is_err());
    assert_eq!(app.edit_source("fx", |_| {}), Ok(false));
    assert!(app.queued().is_empty());
    assert_eq!(Config::load_sources(&file).unwrap(), initial);

    assert_eq!(app.edit_source("fx", |source| source.ignore(IgnoreAction::Add, "drafts").unwrap()), Ok(true));
    assert_eq!(app.edit_global(|global| global.include_repo_items = true), Ok(true));
    assert!(!app.queued().is_empty());
    run(&mut app, &mut s.engine, T0);
    let saved = Config::load_sources(&file).unwrap();
    assert_eq!(saved.find_source("fx").unwrap().ignore_add, ["drafts"]);
    assert!(saved.global().include_repo_items);
    assert_eq!(&saved, app.cfg());
}

#[test]
fn removing_by_any_lane_id_lets_go_of_the_whole_source() {
    let (w, _remote, manager, mut engine, mut app, _file) = plugin_app("app-remove-source");
    let ticket = app.preview_location(REPO, None).unwrap();
    run_next(&mut app, &mut engine, T0);
    let DraftStage::Reviewed(preview) = &app.draft().unwrap().stage else { panic!("expected a finished preview") };
    let id = preview.source.id.clone();
    let plugins = preview.source.lanes.id(SourceKind::Plugins).unwrap().to_owned();
    let skills = preview.source.lanes.id(SourceKind::Skills).unwrap().to_owned();
    app.confirm_source(ticket, false).unwrap();
    run(&mut app, &mut engine, T0);
    assert_eq!(w.skill_names(), ["beta"]);
    assert!(w.state().repo(&skills).is_some());
    let calls = manager.calls().len();

    assert!(app.want(Job::Remove { repo: plugins.clone(), delete_skills: false }));
    let settled = last(run(&mut app, &mut engine, T0));
    assert_eq!(settled.removed.as_deref(), Some(id.as_str()));
    assert!(settled.feedback.as_deref().is_some_and(|text| text.starts_with("Removed ")), "{:?}", settled.feedback);
    let state = w.state();
    assert!(state.plugin_sources().is_empty() && state.repo(&skills).is_none(), "every lane was let go of");
    assert_eq!(w.skill_names(), ["beta"], "kept skills become the user's");
    assert!(manager.calls()[calls..].iter().all(|c| matches!(c, PluginCall::Home | PluginCall::List | PluginCall::Markets)), "nothing was uninstalled");
}
#[test]
fn enabling_a_source_waits_for_the_local_settings_save() {
    let mut s = Setup::fresh("app-deferred-enable");
    let mut before = Config { skills_dir: Some(s.w.skills.to_string_lossy().into_owned()), ..s.cfg.clone() };
    before.update_lane_at(0, |r| r.enabled = false);
    let file = s.w.home.join(tiny_skill_sync::config::CONFIG_FILE);
    before.save(&file).unwrap();
    let mut app = App::new(before.clone(), s.w.skills.clone());
    app.set_settings_source(SettingsSource::new(file.clone(), s.w.root.join("exe")));
    run(&mut app, &mut s.engine, T0);
    let mut after = before.clone();
    after.update_lane_at(0, |r| r.enabled = true);
    for conflict in [true, false] {
        app.queue_config_save(before.clone(), after.clone(), false, false);
        assert_eq!(app.cfg(), &before);
        assert_eq!(app.queued(), &[Job::Plan]);
        if conflict {
            let mut emptied = before.clone();
            emptied.clear_sources();
            emptied.save(&file).unwrap();
        }
        run_next(&mut app, &mut s.engine, T0);
        if conflict {
            assert!(app.is_idle());
            assert_eq!(app.cfg(), &before);
            assert!(Config::load_sources(&file).unwrap().repos().is_empty());
            before.save(&file).unwrap();
        } else {
            assert_eq!(Config::load_sources(&file).unwrap(), after);
            assert_eq!(app.cfg(), &after);
            assert!(app.queued().contains(&Job::Check(Some(fx()))));
        }
    }
}

#[test]
fn startup_loads_portable_settings_on_the_worker_before_the_first_engine_job() {
    let mut s = Setup::fresh("app-portable-start");
    let mut shared = s.cfg.clone();
    shared.skills_dir = Some(s.w.skills.to_string_lossy().into_owned());
    let source = portable_source(&s, &shared);
    let mut app = App::new(Config::default(), s.w.skills.clone());
    app.set_settings_source(source);

    run(&mut app, &mut s.engine, T0);
    app.want(Job::Check(None));
    run(&mut app, &mut s.engine, T0 + 1);

    assert!(app.started());
    assert!(app.portable_mode());
    assert_eq!(app.cfg().repos(), shared.repos());
    assert_eq!(s.w.skill_names(), ["alpha", "beta", "gamma"]);
}

#[test]
fn a_malformed_present_portable_file_does_not_fall_back_to_local_config() {
    let mut s = Setup::fresh("app-portable-malformed");
    s.cfg.save(&s.w.home.join(tiny_skill_sync::config::CONFIG_FILE)).unwrap();
    let source = portable_source(&s, &s.cfg);
    std::fs::write(source.executable_dir.join(tiny_skill_sync::portable::SETTINGS_FILE), "{ broken").unwrap();
    let mut app = App::new(Config::default(), s.w.skills.clone());
    app.set_settings_source(source);

    let order = app.next_order().unwrap();
    let done = app::perform(&mut s.engine, &order, &cancel_flag());

    assert!(matches!(done.result, Err(EngineError::Failed(_))));
    assert!(done.settings.unwrap().result.is_err());
    assert!(!app.started());
    assert!(s.w.skill_names().is_empty());
    assert!(!s.w.state_file().exists());
}

#[test]
fn opening_the_window_can_reload_shared_settings_without_a_folder_watch() {
    let mut s = Setup::fresh("app-portable-window-reload");
    let mut shared = s.cfg.clone();
    shared.skills_dir = Some(s.w.skills.to_string_lossy().into_owned());
    let source = portable_source(&s, &shared);
    let exe_dir = source.executable_dir.clone();
    let mut app = App::new(Config::default(), s.w.skills.clone());
    app.set_settings_source(source);
    run(&mut app, &mut s.engine, T0);
    assert!(app.cfg().repos()[0].enabled);

    shared.update_lane_at(0, |r| r.enabled = false);
    write_portable(&exe_dir, &shared);
    app.request_settings_reload();
    run(&mut app, &mut s.engine, T0 + 1);

    assert!(!app.cfg().repos()[0].enabled);
}

#[test]
fn a_timer_poll_reloads_portable_settings_before_syncing() {
    let mut s = Setup::fresh("app-portable-poll");
    let mut shared = s.cfg.clone();
    shared.skills_dir = Some(s.w.skills.to_string_lossy().into_owned());
    let source = portable_source(&s, &shared);
    let exe_dir = source.executable_dir.clone();
    let mut app = App::new(Config::default(), s.w.skills.clone());
    app.set_settings_source(source);
    run(&mut app, &mut s.engine, T0);
    app.want(Job::Check(None));
    run(&mut app, &mut s.engine, T0);
    assert_eq!(app.arm(T0), Some(T0 + DAY));

    shared.update_lane_at(0, |r| r.enabled = false);
    write_portable(&exe_dir, &shared);
    app.on_timer(T0 + DAY);
    assert_eq!(app.queued(), &[Job::Poll]);
    run(&mut app, &mut s.engine, T0 + DAY);

    assert!(!app.cfg().repos()[0].enabled);
}

#[test]
fn a_portable_source_disappearing_makes_skills_orphans_not_deletions() {
    let mut s = Setup::fresh("app-portable-orphan");
    let mut shared = s.cfg.clone();
    shared.skills_dir = Some(s.w.skills.to_string_lossy().into_owned());
    let source = portable_source(&s, &shared);
    let exe_dir = source.executable_dir.clone();
    let mut app = App::new(Config::default(), s.w.skills.clone());
    app.set_settings_source(source);
    run(&mut app, &mut s.engine, T0);
    app.want(Job::Check(None));
    run(&mut app, &mut s.engine, T0 + 1);
    assert_eq!(s.w.skill_names(), ["alpha", "beta", "gamma"]);

    write_portable(&exe_dir, &Config { skills_dir: shared.skills_dir.clone(), ..Config::default() });
    app.want(Job::Check(None));
    run(&mut app, &mut s.engine, T0 + 1);

    assert!(app.cfg().repos().is_empty());
    assert_eq!(s.w.skill_names(), ["alpha", "beta", "gamma"]);
    assert!(s.w.state().repo("fx").is_some(), "unlisted sources remain recorded as orphans");
}

#[test]
fn portable_saves_run_on_the_worker_and_report_shared_file_conflicts() {
    let mut s = Setup::fresh("app-portable-save");
    let mut shared = s.cfg.clone();
    shared.skills_dir = Some(s.w.skills.to_string_lossy().into_owned());
    let source = portable_source(&s, &shared);
    let exe_dir = source.executable_dir.clone();
    let mut app = App::new(Config::default(), s.w.skills.clone());
    app.set_settings_source(source);
    run(&mut app, &mut s.engine, T0);

    let before = app.cfg().clone();
    let after = Config { default_interval: Interval::new(12, Unit::Hour), ..before.clone() };
    app.set_cfg(after.clone(), s.w.skills.clone());
    app.queue_config_save(before.clone(), after.clone(), true, true);
    let order = app.next_order().unwrap();
    let done = app::perform(&mut s.engine, &order, &cancel_flag());
    assert!(matches!(done.config_saves[0].result, Ok(())));
    app.finish(done, T0);
    assert_eq!(tiny_skill_sync::portable::load(&exe_dir).unwrap().unwrap().default_interval, Interval::new(12, Unit::Hour));

    write_portable(&exe_dir, &Config { default_interval: Interval::new(6, Unit::Hour), ..before.clone() });
    app.queue_config_save(before.clone(), after, true, true);
    let order = app.next_order().unwrap();
    let done = app::perform(&mut s.engine, &order, &cancel_flag());
    let Err(error) = &done.config_saves[0].result else { panic!("expected a settings conflict") };
    assert!(error.contains("changed since it was loaded"));
    app.finish(done, T0);
    assert_eq!(app.cfg(), &before, "a failed shared edit is not kept in use");
    assert_eq!(tiny_skill_sync::portable::load(&exe_dir).unwrap().unwrap().default_interval, Interval::new(6, Unit::Hour));
}

#[test]
fn a_skills_row_tells_what_the_record_says_about_it() {
    let mut s = Setup::installed("app-facts");
    let mut app = started(&mut s);
    let alpha = row(&app, "alpha");
    let facts = alpha.facts.expect("the row of a skill the app installed has facts");
    assert_eq!((facts.commit, facts.files, facts.updated), (s.remote.head(REPO), 3, Some(T0)));
    assert!(alpha.listed);

    s.remote.put(REPO, "skills/alpha/SKILL.md", ALPHA_V2);
    s.advance(60);
    app.want(Job::Check(None));
    run(&mut app, &mut s.engine, T0 + 60);
    assert_eq!(row(&app, "alpha").facts.and_then(|f| f.updated), Some(T0 + 60), "an update is told at once");
}

#[test]
fn the_first_start_looks_then_checks_then_sleeps_until_the_next_check_is_due() {
    let mut s = Setup::fresh("app-first");
    let mut app = app_for(&s);
    assert_eq!(app.queued(), &[Job::Startup]);
    assert_eq!(app.arm(T0), None, "the first job does not wait for a timer");

    let order = app.next_order().unwrap();
    assert_eq!(app.health(), Health::Working);
    let done = app::perform(&mut s.engine, &order, &cancel_flag());
    assert_eq!(app.finish(done, T0), Settled::default());
    assert!(app.started());
    assert_eq!(app.health(), Health::Ok, "never having checked is not a problem");
    assert_eq!(app.arm(T0), Some(T0), "a repository that was never checked is due now");

    app.on_timer(T0);
    assert_eq!(app.queued(), &[Job::Poll]);
    assert_eq!(last(run(&mut app, &mut s.engine, T0)), Settled::default(), "the timer's checks are not reported in the status line");
    assert_eq!(s.w.skill_names(), ["alpha", "beta", "gamma"]);
    assert_eq!(app.tooltip(), "Tiny Skill Sync: all skills are up to date");
    assert_eq!(row(&app, "alpha").text, Verdict::InSync.message());
    assert_eq!(app.board.views(app.cfg(), T0)[0].last_check, Some(T0));
    assert_eq!(app.arm(T0), Some(T0 + DAY), "the next check is a day away");

    app.on_timer(T0 + 10);
    assert!(app.queued().is_empty(), "a timer that fires early (the clock was changed) checks nothing");
}

#[test]
fn a_repository_found_to_be_private_is_shown_as_private_and_still_is_after_a_restart() {
    let mut s = Setup::fresh("app-private");
    s.tokens = FakeTokens::some("tok");
    s.engine = s.restarted();
    s.remote.make_private(REPO, "tok");
    let mut app = started(&mut s);
    assert!(!app.board.views(app.cfg(), T0)[0].private, "nothing was asked yet");

    app.want(Job::Check(None));
    run(&mut app, &mut s.engine, T0);
    assert_eq!(s.w.skill_names(), ["alpha", "beta", "gamma"]);
    assert!(app.board.views(app.cfg(), T0)[0].private);

    let mut engine = s.restarted();
    let mut again = app_for(&s);
    run(&mut again, &mut engine, T0);
    assert!(again.board.views(again.cfg(), T0)[0].private, "remembered");
}

#[test]
fn a_job_that_is_already_waiting_covers_a_smaller_one() {
    let mut app = bare();
    assert!(!app.want(Job::Look), "starting up looks at the disk anyway");
    assert!(app.want(Job::Poll));
    assert!(!app.want(Job::Poll), "the same job twice is one job");
    assert!(app.want(Job::Check(Some(fx()))));
    assert!(app.want(Job::Check(None)));
    assert_eq!(app.queued(), &[Job::Startup, Job::Check(None)], "checking everything covers the rest");

    assert!(!app.want(Job::Poll));
    assert!(!app.want(Job::Check(Some(fx()))));
    assert!(!app.want(Job::Look));
    assert!(!app.want(Job::Plan), "every job works out when to wake up");
    let keep = Job::Resolve { repo: fx(), skill: "alpha".to_string(), how: Resolution::KeepMine };
    assert!(app.want(keep.clone()), "a choice the user made is never covered");
    assert_eq!(app.queued(), &[Job::Startup, keep, Job::Check(None)]);

    let mut app = bare();
    app.next_order();
    assert!(app.want(Job::Look), "the look of the startup job is already under way");
    assert!(!app.want(Job::Look));
    assert!(app.want(Job::Check(Some(fx()))));
    assert!(!app.want(Job::Look), "a check looks at the disk first");
    assert!(app.want(Job::Startup), "a start that has to be tried again");
    assert_eq!(app.queued(), &[Job::Startup, Job::Look, Job::Check(Some(fx()))], "goes first");
}

#[test]
fn quitting_drops_what_is_waiting_and_saves_last() {
    let mut app = bare();
    app.want(Job::Check(None));
    app.quit();
    assert_eq!(app.queued(), &[Job::Flush]);
    assert!(!app.want(Job::Check(None)), "nothing new starts");
    assert_eq!(app.arm(T0), None);

    let order = app.next_order().unwrap();
    assert_ne!(app.health(), Health::Working, "saving is not checking");
    let done = Done {
        job: order.job,
        result: Ok(Answer::Flushed),
        next_wake: Some(T0),
        last_checks: Vec::new(),
        kept: Vec::new(),
        facts: Vec::new(),
        plugin_records: Vec::new(),
        lists_nothing: Vec::new(),
        private: Vec::new(),
        stamp: None,
        settings: None,
        config_saves: Vec::new(),
        settings_checked: false,
        update: None,
        seen: None,
    };
    app.finish(done, T0);
    assert!(app.is_idle());
    assert_eq!(app.arm(T0), None, "no timer once quitting");
    app.on_timer(T0);
    assert!(app.queued().is_empty());
}

#[test]
fn checking_settings_blocks_local_edits_until_the_worker_finishes() {
    let mut app = bare();
    assert!(app.want(Job::Check(None)));
    assert!(app.portable_probe_pending());
    answer(&mut app, Err(EngineError::Failed("test startup".to_string())), None, T0);

    let order = app.next_order().unwrap();
    app.finish(
        Done {
            job: order.job,
            result: checked_nothing(),
            next_wake: None,
            last_checks: Vec::new(),
            kept: Vec::new(),
            facts: Vec::new(),
            plugin_records: Vec::new(),
            lists_nothing: Vec::new(),
            private: Vec::new(),
            stamp: None,
            settings: None,
            config_saves: Vec::new(),
            settings_checked: true,
            update: None,
            seen: None,
        },
        T0,
    );
    assert!(!app.portable_probe_pending());
}

#[test]
fn a_busy_engine_is_tried_again_later_or_told_to_the_user() {
    let mut app = bare();
    app.want(Job::Check(None));
    assert_eq!(answer(&mut app, Err(EngineError::Busy), None, T0), Settled::default(), "starting up");
    assert!(!app.started());

    let told = answer(&mut app, Err(EngineError::Busy), None, T0);
    assert_eq!(told.feedback.as_deref(), Some("Another check is running right now; try again in a moment"));
    assert_eq!(app.board.messages().count(), 0, "the status line says it; it is not news worth keeping");

    assert!(app.want(Job::Look));
    assert_eq!(answer(&mut app, Err(EngineError::Busy), None, T0), Settled::default(), "the other program's change brings its own look");
    assert!(app.queued().is_empty());

    assert_eq!(app.arm(T0), Some(T0 + BUSY_RETRY), "starting up is tried again in a minute");
    app.on_timer(T0 + 10);
    assert!(app.queued().is_empty(), "not before its time");
    app.arm(T0 + 10);
    app.on_timer(T0 + BUSY_RETRY);
    assert_eq!(app.queued(), &[Job::Startup]);

    let mut s = Setup::fresh("app-busy-poll");
    let mut app = started(&mut s);
    app.on_timer(T0);
    assert_eq!(answer(&mut app, Err(EngineError::Busy), Some(T0), T0), Settled::default());
    assert_eq!(app.arm(T0), Some(T0 + BUSY_RETRY), "the timer's check too");
}

#[test]
fn an_engine_that_cannot_work_is_an_error_and_is_tried_less_often_until_it_works() {
    let mut s = Setup::fresh("app-broken");
    let mut app = started(&mut s);
    app.on_timer(T0);
    assert_eq!(answer(&mut app, broken(), Some(T0), T0), Settled { stale_feedback: true, ..Settled::default() });
    assert_eq!(app.health(), Health::Error);
    assert_eq!(app.tooltip(), "Tiny Skill Sync: cannot read state.json");
    assert_eq!(app.arm(T0), Some(T0 + FAILURE_BACKOFF), "still due, but not in a loop");

    let mut now = T0 + FAILURE_BACKOFF;
    for wait in [600, 1200, 2400, 3600, 3600] {
        app.on_timer(now);
        assert_eq!(app.queued(), &[Job::Poll]);
        answer(&mut app, broken(), Some(T0), now);
        assert_eq!(app.arm(now), Some(now + wait));
        now += wait;
    }
    let errors = app.board.messages().filter(|m| m.level == Level::Error).count();
    assert_eq!(errors, 1, "told once, not at every try");

    app.on_timer(now);
    run(&mut app, &mut s.engine, now);
    assert_eq!((app.health(), app.board.failure()), (Health::Ok, None), "it works again");
    assert_eq!(s.w.skill_names(), ["alpha", "beta", "gamma"]);

    app.want(Job::Look);
    answer(&mut app, broken(), Some(now), now);
    assert_eq!(app.arm(now), Some(now + FAILURE_BACKOFF), "the count started again");

    app.want(Job::Check(None));
    let told = answer(&mut app, Err(EngineError::Failed("disk full".to_string())), Some(now), now);
    assert_eq!(told.feedback.as_deref(), Some("disk full"), "the user asked, so the user is told");
    assert_eq!(app.board.messages().last().map(|m| (m.level, m.text.as_str())), Some((Level::Error, "disk full")));
    assert_eq!(app.arm(now), Some(now + FAILURE_BACKOFF), "and the timer is not pushed back for it");
}

#[test]
fn two_checks_by_the_timer_are_never_right_after_each_other() {
    let mut s = Setup::fresh("app-gap");
    let mut app = started(&mut s);
    assert_eq!(app.arm(T0), Some(T0), "no gap after starting up");

    app.on_timer(T0);
    answer(&mut app, checked_nothing(), Some(T0), T0);
    assert_eq!(app.arm(T0), Some(T0 + MIN_POLL_GAP), "whatever the engine says is due");

    app.want(Job::Check(None));
    answer(&mut app, checked_nothing(), Some(T0), T0 + MIN_POLL_GAP);
    assert_eq!(app.arm(T0 + MIN_POLL_GAP), Some(T0 + MIN_POLL_GAP), "a check the user asked for adds no gap");
}

#[test]
fn a_timer_that_fires_long_after_its_time_waits_for_the_network() {
    let mut s = Setup::fresh("app-late");
    let mut app = started(&mut s);
    app.on_timer(T0);
    run(&mut app, &mut s.engine, T0);
    assert_eq!(app.arm(T0), Some(T0 + DAY));

    let woke = T0 + DAY + 61;
    app.on_timer(woke);
    assert!(app.queued().is_empty(), "the computer slept through the check");
    assert_eq!(app.arm(woke), Some(woke + RESUME_GRACE));
    app.on_timer(woke + RESUME_GRACE);
    assert_eq!(app.queued(), &[Job::Poll]);

    let mut app = started(&mut s);
    app.on_timer(T0);
    run(&mut app, &mut s.engine, T0);
    app.arm(T0);
    app.on_timer(T0 + DAY + 60);
    assert_eq!(app.queued(), &[Job::Poll], "a minute late is on time");

    let mut app = started(&mut s);
    let woke = T0 + DAY + 5;
    app.resumed(woke);
    assert_eq!(app.arm(woke), Some(woke + RESUME_GRACE), "waking up says the same");
}

#[test]
fn a_start_at_sign_in_waits_before_it_looks_then_looks_and_checks_as_background_work() {
    let mut s = Setup::fresh("app-signin");
    let mut app = app_for(&s);
    app.started_by_windows(T0);
    assert!(app.next_order().is_none(), "not even a look at the disk while Windows is still starting");
    assert_eq!(app.health(), Health::Waiting);
    assert_eq!(app.tooltip(), "Tiny Skill Sync: waiting for Windows to finish starting", "nothing was looked at, so nothing is up to date yet");
    assert!(!app.want(Job::Look), "a change on disk waits for the first look, which looks anyway");
    assert_eq!(app.arm(T0), Some(T0 + SIGN_IN_GRACE));
    app.on_timer(T0 + SIGN_IN_GRACE - 1);
    assert!(app.queued().is_empty());

    let at = T0 + SIGN_IN_GRACE;
    app.on_timer(at);
    assert_eq!(run_next(&mut app, &mut s.engine, at), (Job::Startup, true), "its look gives way to whatever the user is doing");
    assert_eq!(app.arm(at), Some(at), "a check is due, and the network has had its time");
    app.on_timer(at);
    assert_eq!(run_next(&mut app, &mut s.engine, at), (Job::Poll, true));
    assert_eq!(s.w.skill_names(), ["alpha", "beta", "gamma"]);
    assert_eq!(app.health(), Health::Ok);
    assert!(app.want(Job::Look), "from now on a change on disk is looked at");
    run(&mut app, &mut s.engine, at);

    app.set_cfg(s.cfg.clone(), s.w.skills.join("moved"));
    let order = app.next_order().expect("a look at the new skills folder");
    assert_eq!((order.job, order.background), (Job::Startup, false), "only the first look after sign-in gives way");

    let mut app = app_for(&s);
    assert_eq!(run_next(&mut app, &mut s.engine, T0), (Job::Startup, false), "started by the user, it looks as usual");
}

#[test]
fn initial_settings_load_does_not_duplicate_startup() {
    let mut s = Setup::fresh("app-initial-settings");
    let source = portable_source(&s, &s.cfg);
    let mut app = app_for(&s);
    app.set_settings_source(source);
    app.queue_initial_settings_load();

    let order = app.next_order().expect("settings load comes first");
    assert_eq!(order.job, Job::LoadSettings);
    let mut done = app::perform(&mut s.engine, &order, &cancel_flag());
    let loaded = done.settings.take().expect("settings were read").result.unwrap();
    app.settings_reloaded(loaded, &done.job);
    app.finish(done, T0);

    assert_eq!(app.queued(), &[Job::Startup]);
}

#[test]
fn a_start_at_sign_in_loads_settings_on_the_worker_without_ending_the_wait() {
    let mut s = Setup::fresh("app-signin-settings");
    let source = portable_source(&s, &s.cfg);
    let mut app = app_for(&s);
    app.set_settings_source(source);
    app.queue_initial_settings_load();
    app.started_by_windows(T0);

    let order = app.next_order().expect("settings can be read while the first look waits");
    assert_eq!((order.job.clone(), order.background), (Job::LoadSettings, true));
    let mut done = app::perform(&mut s.engine, &order, &cancel_flag());
    let loaded = done.settings.take().expect("settings were read").result.unwrap();
    app.settings_reloaded(loaded, &done.job);
    app.finish(done, T0);

    assert!(app.settings_initialized());
    assert!(!app.started());
    assert!(app.queued().is_empty(), "the first look still waits for the sign-in grace period");
    assert_eq!(app.arm(T0), Some(T0 + SIGN_IN_GRACE));
    app.on_timer(T0 + SIGN_IN_GRACE);
    assert_eq!(app.queued(), &[Job::Startup]);
}

#[test]
fn someone_who_comes_while_the_app_waits_after_sign_in_is_not_kept_waiting() {
    let mut s = Setup::fresh("app-signin-user");
    let mut app = app_for(&s);
    app.started_by_windows(T0);
    app.user_here();
    assert_eq!(run_next(&mut app, &mut s.engine, T0 + 5), (Job::Startup, false), "the window shows what it finds, so it looks at once, at full speed");
    app.user_here();
    assert!(app.queued().is_empty(), "one look is enough");
    assert_eq!(app.arm(T0 + 5), Some(T0 + SIGN_IN_GRACE), "the timer's check still gives the network its time");

    let mut engine = s.restarted();
    let mut app = app_for(&s);
    app.started_by_windows(T0);
    assert!(app.want(Job::Check(None)));
    assert_eq!(app.queued(), &[Job::Startup, Job::Check(None)], "Sync now: the first look, then the check");
    assert_eq!(run_next(&mut app, &mut engine, T0 + 5), (Job::Startup, false));
    assert_eq!(run_next(&mut app, &mut engine, T0 + 5), (Job::Check(None), false));
    assert_eq!(s.w.skill_names(), ["alpha", "beta", "gamma"]);
}

#[test]
fn a_change_by_another_program_is_noticed_from_the_stamp_and_looked_at() {
    let mut s = Setup::installed("app-foreign");
    let mut app = started(&mut s);
    let file = s.w.state_file();
    assert_eq!(engine::stamp_of(&file), app.state_stamp(), "nothing changed since the last job");
    assert!(app.state_stamp().is_some());

    // `tss resolve fx alpha keep`
    let mut tss = s.restarted();
    tss.resolve(&s.cfg, "fx", "alpha", Resolution::KeepMine, &cancel_flag()).unwrap();
    assert_ne!(engine::stamp_of(&file), app.state_stamp());

    assert!(app.want(Job::Look));
    let order = app.next_order().unwrap();
    assert_ne!(app.health(), Health::Working, "a look is too quick to show");
    let done = app::perform(&mut s.engine, &order, &cancel_flag());
    app.finish(done, T0);
    assert_eq!(row(&app, "alpha").text, "Kept as yours");
    assert_eq!(row(&app, "alpha").actions, [Action::ManageAgain]);
    assert_eq!(engine::stamp_of(&file), app.state_stamp(), "seen");
}

#[test]
fn a_conflict_is_shown_and_settled_as_the_user_chose() {
    let mut s = Setup::installed("app-resolve");
    let mut app = started(&mut s);
    s.remote.put(REPO, "skills/alpha/SKILL.md", ALPHA_V2);
    s.w.write("alpha/SKILL.md", "my alpha\n");

    app.want(Job::Check(None));
    let told = last(run(&mut app, &mut s.engine, T0));
    assert_eq!(told.feedback.as_deref(), Some("Checked: some skills need your attention"));
    assert_eq!(app.health(), Health::Attention);
    assert_eq!(app.tooltip(), "Tiny Skill Sync: 1 skill needs attention");
    assert!(row(&app, "alpha").actions.contains(&Action::Overwrite));
    assert_eq!(s.w.read("alpha/SKILL.md"), "my alpha\n");

    app.want(Job::Resolve { repo: fx(), skill: "alpha".to_string(), how: Resolution::Overwrite });
    let told = last(run(&mut app, &mut s.engine, T0));
    assert_eq!(s.w.read("alpha/SKILL.md"), ALPHA_V2);
    assert!(told.feedback.unwrap().starts_with("'alpha' now matches the repository"));
    let news = app.board.messages().last().unwrap().text.clone();
    assert!(news.contains("The replaced copy is in "), "{news}");
    assert_eq!(app.health(), Health::Ok);
}

#[test]
fn a_check_that_finds_nothing_new_does_not_say_all_is_well_while_a_skill_still_needs_the_user() {
    let mut s = Setup::installed("app-nothing-new");
    let mut app = started(&mut s);
    app.want(Job::Check(None));
    let told = last(run(&mut app, &mut s.engine, T0));
    assert_eq!(told.feedback.as_deref(), Some("Checked: everything is up to date"));

    s.w.write("alpha/SKILL.md", "my alpha\n");
    app.want(Job::Check(None));
    let told = last(run(&mut app, &mut s.engine, T0));
    assert_eq!(app.health(), Health::Attention);
    assert_eq!(told.feedback.as_deref(), Some("Checked: nothing new on GitHub"), "the edit still needs the user");
}

#[test]
fn what_the_user_was_told_is_dropped_once_the_state_changes_without_them() {
    let mut s = Setup::installed("app-told");
    let mut app = started(&mut s);
    app.want(Job::Check(None));
    let told = last(run(&mut app, &mut s.engine, T0));
    assert!(told.feedback.is_some());
    assert!(!told.stale_feedback);

    app.want(Job::Look);
    assert!(!last(run(&mut app, &mut s.engine, T0)).stale_feedback, "nothing changed, so it still holds");

    s.w.write("alpha/SKILL.md", "my alpha\n");
    app.want(Job::Look);
    let looked = last(run(&mut app, &mut s.engine, T0));
    assert_eq!(app.health(), Health::Attention);
    assert_eq!((looked.feedback, looked.stale_feedback), (None, true));
}

#[test]
fn skills_gone_upstream_all_at_once_are_deleted_only_when_the_user_says_so() {
    let mut s = Setup::installed("app-gone");
    let mut app = started(&mut s);
    for name in ["alpha", "beta", "gamma"] {
        s.remote.remove(REPO, &format!("skills/{name}"));
    }

    app.want(Job::Check(Some(fx())));
    run(&mut app, &mut s.engine, T0);
    assert_eq!(row(&app, "alpha").actions, [Action::RemoveThem, Action::KeepMine]);
    assert_eq!(s.w.skill_names(), ["alpha", "beta", "gamma"]);

    app.want(Job::DeleteGone(fx()));
    let told = last(run(&mut app, &mut s.engine, T0));
    assert!(s.w.skill_names().is_empty());
    assert_eq!(told.feedback.as_deref(), Some("Checked: skills were updated"));
    // Nothing is left to settle, but a repository that lists no skills is not "up to date" either.
    assert_eq!(app.health(), Health::Attention);
    assert_eq!(app.board.views(app.cfg(), T0)[0].status, "No skills found");

    // GitHub now answers "not modified": the listing it stands for is still empty, and the user is told so again.
    app.want(Job::Check(Some(fx())));
    let told = last(run(&mut app, &mut s.engine, T0));
    let label = app.cfg().repos()[0].label();
    assert_eq!(told.feedback, Some(format!("Checked: no skills found ({label})")));
    assert_eq!(app.board.views(app.cfg(), T0)[0].status, "No skills found");
}

#[test]
fn after_a_restart_only_a_repository_whose_listing_was_empty_says_no_skills_found() {
    // Upstream lists one skill, and the user has a different folder with its name: it is held, nothing is installed.
    let mut s = Setup::fresh("app-nothing");
    for name in ["beta", "gamma"] {
        s.remote.remove(REPO, &format!("skills/{name}"));
    }
    s.w.write("alpha/SKILL.md", "my own alpha\n");
    let mut app = started(&mut s);
    app.want(Job::Check(None));
    run(&mut app, &mut s.engine, T0);
    assert_eq!(row(&app, "alpha").actions, [Action::Overwrite, Action::KeepMine]);

    let status = |app: &App| app.board.views(app.cfg(), T0).remove(0).status;
    let mut engine = s.restarted();
    let mut again = app_for(&s);
    run(&mut again, &mut engine, T0);
    assert_ne!(status(&again), "No skills found", "its listing had a skill in it");

    // Once upstream really lists nothing, a restart says so too.
    s.remote.remove(REPO, "skills/alpha");
    again.want(Job::Check(None));
    run(&mut again, &mut engine, T0);
    assert_eq!(status(&again), "No skills found");
    let mut engine = s.restarted();
    let mut third = app_for(&s);
    run(&mut third, &mut engine, T0);
    assert_eq!(status(&third), "No skills found");
    assert_eq!(s.w.read("alpha/SKILL.md"), "my own alpha\n");
}

#[test]
fn a_removed_repository_lets_go_of_its_skills_before_it_leaves_the_settings() {
    let mut s = Setup::installed("app-remove");
    s.w.write("beta/SKILL.md", "my beta\n");
    let mut app = started(&mut s);

    app.want(Job::Remove { repo: fx(), delete_skills: true });
    let told = last(run(&mut app, &mut s.engine, T0));
    assert_eq!(told.removed.as_deref(), Some("fx"), "the window takes it out of the settings");
    let text = told.feedback.unwrap();
    assert!(text.starts_with("Removed o/fixtures : skills. Deleted: alpha, gamma. Kept as yours: beta ("), "{text}");
    assert_eq!(s.w.skill_names(), ["beta"], "the edited one stays");
    assert_eq!(app.board.messages().last().map(|m| m.level), Some(Level::Attention));
    assert!(app.board.views(app.cfg(), T0)[0].skills.is_empty(), "forgotten");

    app.set_cfg(Config::default(), s.w.skills.clone());
    run(&mut app, &mut s.engine, T0);
    assert_eq!(app.health(), Health::NotConfigured);
    assert_eq!(app.arm(T0), None, "nothing to wake up for");
}

#[test]
fn new_settings_are_planned_for_before_the_timer_is_set() {
    let mut s = Setup::installed("app-plan");
    let mut app = started(&mut s);
    assert_eq!(app.arm(T0), Some(T0 + DAY));

    let mut cfg = s.cfg.clone();
    cfg.push_lane(repo("two", "o/two", "skills"));
    app.set_cfg(cfg, s.w.skills.clone());
    assert_eq!(app.queued(), &[Job::Plan]);
    assert_eq!(app.arm(T0), None, "the plan comes first");
    assert_eq!(last(run(&mut app, &mut s.engine, T0)), Settled::default());
    assert_eq!(app.arm(T0), Some(T0), "the new repository is due now");
    assert_ne!(app.health(), Health::Working);
}

#[test]
fn another_skills_folder_in_the_settings_is_taken_in_at_once_the_way_the_app_starts() {
    let mut s = Setup::installed("app-folder");
    let mut app = started(&mut s);
    let home = s.w.skills.clone();
    let new = s.w.root.join("elsewhere").join("skills");

    app.set_cfg(s.cfg.clone(), new.clone());
    assert_eq!(app.queued(), &[Job::Startup], "a look would change nothing, and the change would wait for a check");
    run(&mut app, &mut s.engine, T0);
    assert_eq!(s.w.state().skills_dir.as_deref(), new.to_str(), "recorded at once, so it is said once");
    assert_eq!(app.arm(T0), Some(T0), "the new folder gets its own copies now");

    s.w.write("beta/SKILL.md", "edited while the app looked elsewhere\n");
    app.set_cfg(s.cfg.clone(), home.clone());
    run(&mut app, &mut s.engine, T0);
    assert_eq!(s.w.state().skills_dir.as_deref(), home.to_str());
    assert!(row(&app, "beta").text.starts_with("Edited locally"), "managed as before: {:?}", row(&app, "beta"));
    assert_eq!(app.arm(T0), Some(T0 + DAY), "and checked when it was due anyway");

    app.set_cfg(s.cfg.clone(), new);
    assert_eq!(answer(&mut app, Err(EngineError::Busy), None, T0), Settled::default());
    app.on_timer(T0 + BUSY_RETRY);
    assert_eq!(app.queued(), &[Job::Startup], "tried again while tss works, like the first start");
}

#[test]
fn what_is_different_is_answered_to_the_window_and_is_neither_news_nor_a_failure_of_the_app() {
    let mut s = Setup::installed("app-diff");
    let mut app = started(&mut s);
    s.w.write("alpha/SKILL.md", "my alpha\n");
    let news = app.board.messages().count();
    let alpha = || Job::Diff { repo: fx(), skill: "alpha".to_string(), preview: false };

    assert!(app.want(alpha()));
    let order = app.next_order().unwrap();
    assert_ne!(app.health(), Health::Working, "the window shows that it is reading, not the tray");
    let settled = app.finish(app::perform(&mut s.engine, &order, &cancel_flag()), T0);
    assert_eq!(settled.feedback, None);
    let shown = settled.diff.expect("an answer for the window");
    assert_eq!((shown.repo.as_str(), shown.skill.as_str(), shown.preview), ("fx", "alpha", false));
    assert_eq!(shown.result.unwrap().files.len(), 1);
    assert_eq!(s.w.read("alpha/SKILL.md"), "my alpha\n");

    assert!(app.want(Job::Diff { repo: fx(), skill: "alpha".to_string(), preview: true }));
    let order = app.next_order().unwrap();
    let shown = app.finish(app::perform(&mut s.engine, &order, &cancel_flag()), T0).diff.expect("an answer for the window");
    assert!(shown.preview, "an answer says which it is, so the window never takes one for the other");
    assert_eq!(shown.result.unwrap().files.len(), 3, "Preview shows every file GitHub put there");

    app.want(alpha());
    let shown = answer(&mut app, Err(EngineError::Busy), None, T0).diff.unwrap();
    assert_eq!(shown.result, Err("Another check is running right now; try again in a moment".to_string()));
    app.want(alpha());
    let settled = answer(&mut app, Err(EngineError::Failed("the connection dropped".to_string())), Some(T0), T0);
    assert_eq!((settled.feedback, settled.diff.unwrap().result), (None, Err("The connection dropped".to_string())));
    assert_eq!(app.board.messages().count(), news, "looking is not news");
    assert_eq!((app.health(), app.arm(T0)), (Health::Ok, Some(T0)), "nor a reason to wait before the next check");

    app.on_timer(T0);
    answer(&mut app, broken(), Some(T0), T0);
    app.want(alpha());
    answer(&mut app, Ok(Answer::Diffed(engine::DiffReport { on_github: true, here: true, files: Vec::new() })), Some(T0), T0);
    assert_eq!((app.health(), app.arm(T0)), (Health::Error, Some(T0 + FAILURE_BACKOFF)), "nor proof that checks work again");
}

#[test]
fn a_stale_diff_requires_fresh_confirmation_before_overwrite() {
    let mut s = Setup::fresh("app-stale-diff");
    // Install just these fixtures without a poll so the comparison is created by the later sync.
    for skill in ["alpha", "beta"] {
        s.engine.resolve(&s.cfg, "fx", skill, Resolution::Overwrite, &cancel_flag()).expect("fixture installs");
    }
    let file = "alpha/SKILL.md";
    let reviewed = "my alpha\n";
    s.w.write(file, reviewed);
    s.remote.put(REPO, "skills/alpha/SKILL.md", ALPHA_V2);
    let mut app = started(&mut s);
    let neighbor = s.w.files_of("beta");

    let review = |app: &mut App, s: &mut Setup| {
        assert!(app.want(Job::Diff { repo: fx(), skill: "alpha".into(), preview: false }));
        let report = last(run(app, &mut s.engine, T0)).diff.unwrap().result.unwrap();
        let [change] = report.files.as_slice() else { panic!("expected one changed file: {report:?}") };
        assert_eq!(change.path, "SKILL.md");
        let tiny_skill_sync::diff::Body::Text(diff) = &change.body else { panic!("expected a text comparison: {change:?}") };
        assert_eq!(diff.new.last().map(String::as_str), ALPHA_V2.lines().last());
        diff.old.join("\n")
    };
    assert_eq!(review(&mut app, &mut s), reviewed.trim_end());

    let edited = "edited after Show changes\r\n";
    s.w.write(file, edited);
    assert!(row(&app, "alpha").actions.contains(&Action::Overwrite), "the window's action check still passes");
    // This is the job diff_action dispatches for "Use the GitHub version".
    let overwrite = || Job::Resolve { repo: fx(), skill: "alpha".into(), how: Action::Overwrite.resolution().unwrap() };
    assert!(app.want(overwrite()));
    let stale = last(run(&mut app, &mut s.engine, T0));
    assert_eq!(s.w.files_of("beta"), neighbor);
    assert_eq!(
        (s.w.read(file).as_str(), s.w.backups().len()),
        (edited, 0),
        "a stale comparison must leave the edited copy in place without a backup until fresh confirmation; status: {:?}",
        stale.feedback
    );
    assert!(stale.diff.is_some() || stale.feedback.is_some(), "a stale choice must not be silently ignored");

    assert_eq!(review(&mut app, &mut s), edited.trim_end());
    assert!(app.want(overwrite()));
    run(&mut app, &mut s.engine, T0);
    assert_eq!(s.w.read(file), ALPHA_V2);
    let backups = s.w.backups();
    assert_eq!(backups.len(), 1);
    assert_eq!(std::fs::read(backups[0].join("SKILL.md")).unwrap(), edited.as_bytes());
    assert_eq!(s.w.files_of("beta"), neighbor);
}
