//! The `tss` commands, run in-process against the fake GitHub and a fake credential store, and the real executable
//! run against a GitHub that does not answer. Every test works in its own temporary folders.

mod common;

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr::null_mut;
use std::sync::{Arc, Mutex};

use common::*;
use tiny_skill_sync::autostart::{Keys, Startup, TEST_ROOT, VALUE_NAME, program};
use tiny_skill_sync::board::APP_NAME;
use tiny_skill_sync::cli::{self, Host};
use tiny_skill_sync::config::{CONFIG_FILE, Config, Paths, RepoSpec, UpdateInterval, UpdatePrefs, UpdateTiming};
use tiny_skill_sync::github::{FailureKind, Remote, RemoteError, TokenProvider};
use tiny_skill_sync::logo;
use tiny_skill_sync::portable::{SETTINGS_FILE, SOURCES_FILE};
use tiny_skill_sync::source::SourceKind;
use tiny_skill_sync::updater::Action as AppAction;
use tiny_skill_sync::win::{autorun, wide};
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VS_FFI_SIGNATURE, VS_FIXEDFILEINFO, VerQueryValueW};
use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_SZ, RegDeleteTreeW, RegGetValueW};

struct TestHost {
    world: World,
    remote: Arc<FakeRemote>,
    tokens: FakeTokens,
    saved: Mutex<Option<String>>,
    gh: Result<(), String>,
    typed: Mutex<Option<String>>,
    /// What the registry layer would say. It is tested on the registry itself; here only what `tss` makes of it.
    startup: Mutex<Result<Startup, String>>,
    app: Option<FakeApp>,
    change_before_second_home_read: Mutex<Option<Config>>,
    home_reads: std::cell::Cell<u32>,
    plugins: FakePluginManager,
    confirm_add: std::cell::Cell<Option<bool>>,
    /// Whether input comes from a terminal, so `tss add` may ask.
    interactive: std::cell::Cell<bool>,
}

#[derive(Debug, PartialEq, Eq)]
enum AppCall {
    ReadPair,
    Manual(AppAction),
}

struct FakeApp {
    calls: Mutex<Vec<AppCall>>,
    installed: Result<&'static str, &'static str>,
    failure: Option<&'static str>,
}

impl Host for TestHost {
    fn home(&self) -> PathBuf {
        self.home_reads.set(self.home_reads.get() + 1);
        if self.home_reads.get() == 2 {
            if let Some(external) = self.change_before_second_home_read.lock().unwrap().take() {
                self.save(&external);
            }
        }
        self.world.home.clone()
    }

    fn executable_dir(&self) -> Result<PathBuf, String> {
        Ok(self.world.root.join("exe"))
    }

    fn skills_dir(&self, _cfg: &Config) -> PathBuf {
        self.world.skills.clone()
    }

    fn source_paths(&self, skills: PathBuf) -> Paths {
        Paths::new(self.home(), skills).with_file_roots(self.world.root.join("agents"), self.world.root.join("instructions"))
    }

    fn installed_app_version(&self, directory: &Path) -> Result<String, String> {
        assert_eq!(directory, self.world.root.join("exe"));
        let app = self.app.as_ref().expect("a skill command inspected the app binaries");
        app.calls.lock().unwrap().push(AppCall::ReadPair);
        app.installed.map(str::to_string).map_err(str::to_string)
    }

    fn manual_app_update(&self, paths: &Paths, directory: &Path, action: AppAction) -> Result<String, String> {
        assert_eq!(directory, self.world.root.join("exe"));
        assert_eq!(paths.home, self.world.home);
        assert_eq!(paths.skills, self.world.skills);
        let app = self.app.as_ref().expect("a skill command asked the app updater");
        app.calls.lock().unwrap().push(AppCall::Manual(action));
        if let Some(why) = app.failure {
            return Err(why.to_string());
        }
        Ok(match action {
            AppAction::Check => "Installed on disk: 8.0.0. Available: 9.0.0.",
            AppAction::Install => "Installed 9.0.0 on disk.",
        }
        .into())
    }

    fn remote(&self) -> Result<Box<dyn Remote>, String> {
        self.skill_services_only();
        Ok(self.remote.client())
    }

    fn tokens(&self) -> Box<dyn TokenProvider> {
        self.skill_services_only();
        self.tokens.provider()
    }

    fn plugin_manager(&self) -> Option<Box<dyn tiny_skill_sync::plugins::native::PluginManager>> {
        Some(self.plugins.manager())
    }

    fn confirm_source(&self) -> Result<bool, String> {
        Ok(self.confirm_add.take().expect("a command not adding a source asked for confirmation"))
    }

    fn interactive(&self) -> bool {
        self.interactive.get()
    }

    fn token_saved(&self) -> bool {
        self.skill_services_only();
        self.saved.lock().unwrap().is_some()
    }

    fn save_token(&self, token: &str) -> Result<(), String> {
        self.skill_services_only();
        *self.saved.lock().unwrap() = Some(token.to_string());
        Ok(())
    }

    fn delete_token(&self) -> Result<bool, String> {
        self.skill_services_only();
        Ok(self.saved.lock().unwrap().take().is_some())
    }

    fn gh_signed_in(&self) -> Result<(), String> {
        self.skill_services_only();
        self.gh.clone()
    }

    fn read_token(&self) -> Result<String, String> {
        self.skill_services_only();
        self.typed.lock().unwrap().take().ok_or_else(|| "no token was given".to_string())
    }

    fn startup(&self) -> Result<Startup, String> {
        self.startup.lock().unwrap().clone()
    }

    fn set_startup(&self, on: bool) -> Result<Startup, String> {
        let mut state = self.startup.lock().unwrap();
        let now = match (on, state.clone()?) {
            (true, Startup::TooLong) => return Err("this copy's path is too long for Windows to start it".to_string()),
            (true, _) => Startup::On,
            (false, Startup::On | Startup::OffInWindows) => Startup::Off,
            (false, other) => other,
        };
        *state = Ok(now.clone());
        Ok(now)
    }
}

impl TestHost {
    /// The standard fixtures upstream and `fx` set up, nothing installed yet.
    fn new(name: &str) -> TestHost {
        let host = TestHost::empty(name);
        host.remote.seed_standard(REPO);
        host.save(&standard_config());
        host
    }

    /// No settings at all.
    fn empty(name: &str) -> TestHost {
        let world = World::new(name);
        std::fs::create_dir_all(world.root.join("exe")).unwrap();
        let remote = FakeRemote::new();
        TestHost {
            plugins: FakePluginManager::new(world.root.join("native")),
            world,
            remote,
            tokens: FakeTokens::none(),
            saved: Mutex::new(None),
            gh: Err("the GitHub CLI is not signed in".to_string()),
            typed: Mutex::new(None),
            startup: Mutex::new(Ok(Startup::Off)),
            app: None,
            change_before_second_home_read: Mutex::new(None),
            home_reads: std::cell::Cell::new(0),
            confirm_add: std::cell::Cell::new(None),
            interactive: std::cell::Cell::new(true),
        }
    }

    fn for_app(name: &str) -> Self {
        Self { app: Some(FakeApp { calls: Mutex::new(Vec::new()), installed: Ok("8.0.0"), failure: None }), ..Self::empty(name) }
    }

    fn skill_services_only(&self) {
        assert!(self.app.is_none(), "an app command asked for the skills remote or credentials");
    }

    fn save(&self, cfg: &Config) {
        cfg.save(&self.config_file()).unwrap();
    }

    fn config_file(&self) -> PathBuf {
        self.world.home.join(CONFIG_FILE)
    }

    fn config(&self) -> Config {
        Config::load_sources(&self.config_file()).unwrap()
    }
}

struct Ran {
    code: i32,
    out: String,
    err: String,
}

impl std::fmt::Debug for Ran {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "exit {}\n--- out ---\n{}--- err ---\n{}", self.code, self.out, self.err)
    }
}

fn tss(host: &TestHost, line: &str) -> Ran {
    let args: Vec<String> = line.split_whitespace().map(str::to_string).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = cli::run(&args, host, &mut out, &mut err);
    Ran { code, out: String::from_utf8(out).unwrap(), err: String::from_utf8(err).unwrap() }
}

#[test]
fn cli_update_preference_edit_cannot_reenable_external_off_from_a_stale_snapshot() {
    let host = TestHost::for_app("cli-concurrent-update-prefs");
    let off = Config { app_update: UpdatePrefs { enabled: false, ..UpdatePrefs::default() }, ..Config::default() };
    *host.change_before_second_home_read.lock().unwrap() = Some(off.clone());
    expect(&tss(&host, "update every 7d"), 2, &["changed since"]);
    assert_eq!(host.config(), off);
}

#[track_caller]
fn expect(ran: &Ran, code: i32, wanted: &[&str]) {
    assert_eq!(ran.code, code, "{ran:?}");
    let all = format!("{}{}", ran.out, ran.err);
    for text in wanted {
        assert!(all.contains(text), "'{text}' is missing from:\n{ran:?}");
    }
}

#[test]
fn a_check_installs_and_status_looks_only_at_the_disk() {
    let host = TestHost::new("cli-install");
    let ran = tss(&host, "check");
    expect(&ran, 0, &["o/fixtures : skills [fx]: Up to date", "Installed alpha", "Installed beta", "Installed gamma", "Downloaded "]);
    assert_eq!(host.world.skill_names(), ["alpha", "beta", "gamma"]);

    host.remote.reset();
    let ran = tss(&host, "status");
    expect(&ran, 0, &["Skills folder: ", "Last check: just now. Checked every ", "(the default).", "    alpha: Installed", "Local check: "]);
    assert!(ran.err.is_empty(), "{ran:?}");

    host.world.write("alpha/SKILL.md", "mine\n");
    let ran = tss(&host, "status");
    expect(
        &ran,
        1,
        &["Needs attention (1)", "  ! alpha: Edited locally", "tss resolve fx alpha replace", "(Use the GitHub version)", "tss resolve fx alpha keep"],
    );
    assert!(host.remote.log().is_empty(), "status asked GitHub: {:?}", host.remote.log());
}

#[test]
fn app_status_and_manual_actions_work_with_automation_off_and_a_bad_skill_repository() {
    let host = TestHost::for_app("cli-app-manual");
    let mut cfg = standard_config();
    cfg.update_lane_at(0, |r| r.repo = "not a repository".into());
    cfg.update_lane_at(0, |r| r.use_token = true);
    cfg.app_update = UpdatePrefs { enabled: false, interval: UpdateInterval::Weekly, timing: UpdateTiming::OnQuit };
    host.save(&cfg);
    host.world.write("handmade/SKILL.md", "leave this alone\n");
    let before = host.world.snapshot();

    let status = tss(&host, "update");
    expect(
        &status,
        0,
        &[
            "Automatic app updates: off",
            "7 days",
            "on Quit",
            &format!("Running tss build: {}", env!("CARGO_PKG_VERSION")),
            "Installed on disk (tray and tss): 8.0.0",
        ],
    );
    assert!(!status.out.contains("9.0.0"), "a local status has not checked the release feed");
    expect(&tss(&host, "update check"), 0, &["Installed on disk: 8.0.0. Available: 9.0.0."]);
    expect(&tss(&host, "update install"), 0, &["Installed 9.0.0 on disk."]);
    assert_eq!(*host.app.as_ref().unwrap().calls.lock().unwrap(), [AppCall::ReadPair, AppCall::Manual(AppAction::Check), AppCall::Manual(AppAction::Install)]);
    assert_eq!(host.world.snapshot(), before, "the CLI itself did not change settings, skills or state");
}

#[test]
fn app_preferences_use_the_active_settings_and_do_not_rewrite_skill_sources() {
    let host = TestHost::for_app("cli-app-settings");
    let cfg = standard_config();
    host.save(&cfg);
    for (command, interval) in
        [("update every 7d", UpdateInterval::Weekly), ("update every 30d", UpdateInterval::Monthly), ("update every 1d", UpdateInterval::Daily)]
    {
        expect(&tss(&host, command), 0, &[]);
        assert_eq!(host.config().app_update.interval, interval);
    }
    expect(&tss(&host, "update off"), 0, &["Automatic app updates: off"]);
    expect(&tss(&host, "update when quit"), 0, &["on Quit", "does not restart itself"]);
    let local = host.config();
    assert_eq!(local, Config { app_update: UpdatePrefs { enabled: false, interval: UpdateInterval::Daily, timing: UpdateTiming::OnQuit }, ..cfg });
    expect(&tss(&host, "update on"), 0, &["Automatic app updates: on"]);
    assert!(host.config().app_update.enabled);

    let dir = host.executable_dir().unwrap();
    let sources = r#"{"version":1,"repos":[{"id":"keep","repo":"o/keep","enabled":false}]} "#;
    std::fs::write(dir.join(SOURCES_FILE), sources).unwrap();
    let local_before = std::fs::read(host.config_file()).unwrap();
    expect(&tss(&host, "update every 30d"), 0, &["30 days"]);
    expect(&tss(&host, "update when quit"), 0, &["on Quit"]);
    expect(&tss(&host, "update when immediate"), 0, &["timing: immediate.", "then restarts into it"]);
    let shared = tiny_skill_sync::portable::load(&dir).unwrap().unwrap().app_update;
    assert_eq!((shared.interval, shared.timing), (UpdateInterval::Monthly, UpdateTiming::Immediate));
    assert_eq!(std::fs::read_to_string(dir.join(SOURCES_FILE)).unwrap(), sources);
    assert_eq!(std::fs::read(host.config_file()).unwrap(), local_before);
    assert!(host.app.as_ref().unwrap().calls.lock().unwrap().is_empty(), "editing preferences is not a request to check or install");
}

#[test]
fn app_update_usage_settings_and_release_failures_do_not_report_success() {
    let mut host = TestHost::for_app("cli-app-errors");
    host.app.as_mut().unwrap().installed = Err("the two file versions differ");
    host.app.as_mut().unwrap().failure = Some("the release could not be verified: bad\u{202e}name, \u{e9}");
    let before = host.world.snapshot();
    for bad in ["update every 2d", "update every 1mo", "update when restart", "update check install", "update install --dry-run"] {
        expect(&tss(&host, bad), cli::EXIT_USAGE, &["tss:"]);
    }
    assert!(host.app.as_ref().unwrap().calls.lock().unwrap().is_empty());
    let status = tss(&host, "update status");
    expect(&status, cli::EXIT_FAILED, &["Running tss build:", "Cannot verify the installed tray and tss on disk: the two file versions differ"]);
    assert!(!status.out.contains("Installed on disk (tray and tss):"), "{status:?}");
    let install = tss(&host, "update install");
    expect(&install, cli::EXIT_FAILED, &["the release could not be verified: bad\\u{202e}name, \\u{e9}"]);
    assert!(install.out.is_empty(), "{install:?}");
    assert!(install.err.is_ascii(), "{install:?}");
    assert_eq!(host.world.snapshot(), before);

    let settings = host.executable_dir().unwrap().join(SETTINGS_FILE);
    std::fs::write(&settings, "{ bad").unwrap();
    let before = host.world.snapshot();
    for cmd in ["update off", "update check", "update install"] {
        expect(&tss(&host, cmd), cli::EXIT_FAILED, &[SETTINGS_FILE]);
    }
    assert_eq!(host.world.snapshot(), before);
    assert_eq!(*host.app.as_ref().unwrap().calls.lock().unwrap(), [AppCall::ReadPair, AppCall::Manual(AppAction::Install)]);
}

#[test]
fn status_says_the_skills_folder_is_missing_rather_than_blaming_the_drive() {
    let host = TestHost::new("cli-no-folder");
    expect(&tss(&host, "check"), 0, &["Installed alpha"]);
    std::fs::remove_dir_all(&host.world.skills).unwrap();

    let ran = tss(&host, "status");
    expect(&ran, 1, &["  ! alpha: Deleted locally", " 0 read (the skills folder does not exist)."]);
    assert!(!ran.out.contains("change journal"), "{ran:?}");
    assert!(!host.world.skills.exists(), "status only looks");
}

#[test]
fn an_edit_holds_an_update_until_the_user_settles_it() {
    let host = TestHost::new("cli-held");
    expect(&tss(&host, "check"), 0, &[]);
    host.world.write("alpha/SKILL.md", "mine\n");
    host.remote.put(REPO, "skills/alpha/SKILL.md", ALPHA_V2);

    let ran = tss(&host, "check");
    expect(&ran, 1, &["! alpha: Update held: edited locally", "tss resolve fx alpha replace", "tss resolve fx alpha keep"]);
    assert!(!ran.out.contains("beta"), "rows that are fine are not listed after a check:\n{ran:?}");
    assert_eq!(host.world.read("alpha/SKILL.md"), "mine\n");

    let ran = tss(&host, "resolve fx alpha replace");
    expect(&ran, 0, &["Backup: "]);
    assert_eq!(host.world.read("alpha/SKILL.md"), ALPHA_V2);
    assert_eq!(host.world.backups().len(), 1);
    expect(&tss(&host, "check"), 0, &["Up to date"]);
}

#[test]
fn a_converged_edit_is_not_reported_as_edited_or_held() {
    let host = TestHost::new("cli-converged");
    expect(&tss(&host, "check"), 0, &[]);
    // The user edited alpha, and the same text was pushed to GitHub afterwards.
    host.world.write("alpha/SKILL.md", ALPHA_V2);
    host.remote.put(REPO, "skills/alpha/SKILL.md", ALPHA_V2);
    let before = host.world.skills_snapshot();

    let ran = tss(&host, "check");
    expect(&ran, 0, &[]);
    let said = format!("{}{}", ran.out, ran.err).to_lowercase();
    assert!(!said.contains("edited") && !said.contains("held"), "the check reports it as edited or held:\n{ran:?}");
    assert_eq!(host.world.skills_snapshot(), before, "not one file changed");
    assert!(host.world.backups().is_empty(), "{:?}", host.world.backups());

    let ran = tss(&host, "status");
    expect(&ran, 0, &[]);
    let said = format!("{}{}", ran.out, ran.err);
    assert!(!said.contains("Needs attention") && !said.contains("Edited"), "status says it needs attention:\n{ran:?}");
}

#[test]
fn a_converged_edit_is_told_as_a_catch_up_and_a_dry_run_only_says_so() {
    let host = TestHost::new("cli-catch-up");
    expect(&tss(&host, "check"), 0, &[]);
    host.world.write("alpha/SKILL.md", ALPHA_V2);
    host.remote.put(REPO, "skills/alpha/SKILL.md", ALPHA_V2);

    let ran = tss(&host, "check fx --dry-run");
    expect(&ran, 0, &["Would mark alpha as in sync (your edit is on GitHub too)", "Dry run: nothing was changed."]);
    let ran = tss(&host, "check");
    expect(&ran, 0, &["Marked alpha as in sync (your edit is on GitHub too)"]);
    expect(&tss(&host, "check"), 0, &["Up to date"]);
}

#[test]
fn a_dry_run_changes_nothing_at_all() {
    let host = TestHost::new("cli-dry");
    let before = host.world.snapshot();
    let ran = tss(&host, "check fx --dry-run");
    expect(&ran, 0, &["Changes pending", "Would install alpha", "Would install gamma", "Dry run: nothing was changed."]);
    assert_eq!(host.world.snapshot(), before);
}

#[test]
fn a_failed_check_deletes_nothing() {
    let host = TestHost::new("cli-offline");
    expect(&tss(&host, "check"), 0, &[]);
    host.remote.remove(REPO, "skills/alpha");
    host.remote.fail_tree(REPO, Some(RemoteError::new(FailureKind::Network, "the network is down")));
    let ran = tss(&host, "check");
    expect(&ran, 2, &["[fx]: No connection", "The network is down", "Nothing is deleted because a check failed."]);
    assert_eq!(host.world.skill_names(), ["alpha", "beta", "gamma"]);
}

#[test]
fn an_empty_listing_deletes_only_when_confirmed() {
    let host = TestHost::new("cli-empty");
    expect(&tss(&host, "check"), 0, &[]);
    for skill in ["alpha", "beta", "gamma"] {
        host.remote.remove(REPO, &format!("skills/{skill}"));
    }
    let ran = tss(&host, "check");
    expect(&ran, 1, &["Removal held: upstream lists no skills at all", "tss check fx --delete-all", "(Delete them)", "tss resolve fx alpha keep"]);
    assert_eq!(host.world.skill_names(), ["alpha", "beta", "gamma"]);

    expect(&tss(&host, "check --delete-all"), 3, &["one repository"]);
    let ran = tss(&host, "check fx --delete-all");
    // Nothing is left to settle, but a repository that lists no skills is not "up to date" either.
    expect(&ran, 1, &["Removed alpha (gone upstream)", "Removed gamma (gone upstream)", "[fx]: No skills found"]);
    assert!(host.world.skill_names().is_empty());
}

#[test]
fn delete_all_for_a_source_confirms_only_its_skills_lane() {
    let host = TestHost::empty("cli-delete-all-source");
    host.remote.put(REPO, "alpha/SKILL.md", "one\n");
    host.remote.put(REPO, "helper.agent.md", "two\n");
    // A legacy config whose first entry is the agents lane: the migrated source takes that lane's ID.
    let legacy = r#"{"version":2,"repos":[{"id":"kit-ag","repo":"o/fixtures","kind":"agents"},{"id":"kit","repo":"o/fixtures"}]}"#;
    std::fs::write(host.config_file(), legacy).unwrap();
    let source = host.config().sources()[0].clone();
    assert_eq!((source.id.as_str(), source.lanes.id(SourceKind::Skills), source.lanes.id(SourceKind::Agents)), ("kit-ag", Some("kit"), Some("kit-ag")));
    let agent = host.world.root.join("agents").join("helper.agent.md");
    expect(&tss(&host, "check"), 0, &["Installed alpha"]);
    assert!(agent.exists() && host.world.skill_names() == ["alpha"], "fixture: both items are installed");

    host.remote.remove(REPO, "alpha");
    host.remote.remove(REPO, "helper.agent.md");
    let ran = tss(&host, "check");
    assert_eq!(ran.code, 1, "both removals are held: {ran:?}");
    assert!(agent.exists() && host.world.skill_names() == ["alpha"], "nothing is deleted unconfirmed: {ran:?}");

    // The source named by its repository confirms only its skills lane, never the agents lane whose ID it took.
    let ran = tss(&host, "check o/fixtures --delete-all");
    assert!(host.world.skill_names().is_empty(), "the skills removal was confirmed: {ran:?}");
    assert!(agent.exists(), "the agents removal was confirmed by a query that names the source, not the agents lane: {ran:?}");
}

#[test]
fn delete_all_for_a_source_with_skills_off_confirms_nothing() {
    let host = TestHost::empty("cli-delete-all-no-skills");
    host.remote.put(REPO, "helper.agent.md", "two\n");
    // A legacy config with only an agents lane: the migrated source takes that lane's ID, and its skills lane is only
    // a reserved ID with the type off (every source gets an ID for each type, so none lacks a skills lane).
    let legacy = r#"{"version":2,"repos":[{"id":"kit-ag","repo":"o/fixtures","kind":"agents"}]}"#;
    std::fs::write(host.config_file(), legacy).unwrap();
    let source = host.config().sources()[0].clone();
    assert_eq!((source.id.as_str(), source.lanes.id(SourceKind::Skills), source.lanes.id(SourceKind::Agents)), ("kit-ag", Some("kit-ag-2"), Some("kit-ag")));
    assert_eq!(source.types.skills, Some(false));
    let agent = host.world.root.join("agents").join("helper.agent.md");
    expect(&tss(&host, "check"), 0, &[]);
    assert!(agent.exists(), "fixture: the agent is installed");

    host.remote.remove(REPO, "helper.agent.md");
    host.remote.put(REPO, "README.md", "nothing to sync\n");
    let ran = tss(&host, "check o/fixtures --delete-all");
    assert_eq!(ran.code, 1, "the removal is still held: {ran:?}");
    assert!(agent.exists(), "a query naming the source confirms only its (off) skills lane, never the agents lane: {ran:?}");

    // Naming the agents lane itself confirms it.
    tss(&host, "check kit-ag --delete-all");
    assert!(!agent.exists(), "the lane named by its ID was confirmed");
}

#[test]
fn names_from_upstream_cannot_steer_the_terminal() {
    let host = TestHost::new("cli-spoof");
    // A folder name that erases the line and returns the cursor, and a file with such a name inside an ordinary skill.
    host.remote.put(REPO, "skills/a\u{1b}[2K\rb/SKILL.md", "x\n");
    host.remote.put(REPO, "skills/plain/SKILL.md", "x\n");
    host.remote.put(REPO, "skills/plain/\u{1b}[31mred.md", "x\n");

    let ran = tss(&host, "check");
    expect(&ran, 1, &["Installed alpha", "a\\u{1b}[2K\\u{d}b: Skipped: not safe to install", "\\u{1b}[31mred.md"]);
    let shown = format!("{}{}", ran.out, ran.err);
    assert!(shown.chars().all(|c| c == '\n' || !c.is_control()), "a control character reached the terminal:\n{shown:?}");
    assert_eq!(host.world.skill_names(), ["alpha", "beta", "gamma"]);
}

#[test]
fn a_skill_name_a_shell_could_run_gets_no_command_to_paste() {
    let host = TestHost::new("cli-paste");
    host.remote.put(REPO, "skills/a$(b)/SKILL.md", "x\n");
    expect(&tss(&host, "check"), 0, &["Installed a$(b)"]);

    host.world.write("a$(b)/SKILL.md", "mine\n");
    let ran = tss(&host, "status");
    expect(&ran, 1, &["! a$(b): Edited locally", "Open Tiny Skill Sync and choose: Use the GitHub version / Keep my version"]);
    assert!(!ran.out.contains("tss resolve fx a$(b)") && !ran.out.contains("\"a$(b)\""), "{ran:?}");
}

#[test]
fn keep_and_manage_check_the_name_first() {
    let host = TestHost::new("cli-keep");
    expect(&tss(&host, "check"), 0, &[]);
    expect(&tss(&host, "resolve fx nope keep"), 3, &["'nope' is neither installed from fx nor a folder in"]);
    assert!(host.world.state().repo("fx").unwrap().ignored.is_empty());

    expect(&tss(&host, "resolve fx ALPHA keep"), 0, &[]);
    assert!(host.world.state().repo("fx").unwrap().is_ignored("alpha"));
    let ran = tss(&host, "status");
    expect(&ran, 0, &["    alpha: Kept as yours", "tss resolve fx alpha manage", "(Manage again)"]);

    expect(&tss(&host, "resolve fx alpha manage"), 0, &[]);
    expect(&tss(&host, "resolve fx alpha manage"), 3, &["'alpha' is not kept as yours in fx; none is"]);
    expect(&tss(&host, "check"), 0, &["Up to date"]);
}

#[test]
fn keep_refuses_a_skill_another_repository_manages() {
    let host = TestHost::new("cli-keep-other");
    host.remote.seed_standard("o/other");
    let mut cfg = standard_config();
    cfg.push_lane(repo("other", "o/other", "skills"));
    host.save(&cfg);
    let ran = tss(&host, "check");
    expect(&ran, 1, &["another repository (fx) already provides this name"]);
    expect(&tss(&host, "resolve other alpha keep"), 3, &["managed for 'fx', not 'other'", "tss resolve fx alpha keep"]);
}

#[test]
fn a_disabled_repository_is_shown_but_checked_only_when_named() {
    let host = TestHost::new("cli-disabled");
    expect(&tss(&host, "check"), 0, &[]);
    let mut cfg = standard_config();
    cfg.update_lane_at(0, |r| r.enabled = false);
    host.save(&cfg);
    host.world.write("alpha/SKILL.md", "mine\n");

    let ran = tss(&host, "status");
    expect(&ran, 0, &["[fx] (disabled): Needs attention (1)", "Disabled: checked only when asked.", "  ! alpha: Edited locally"]);
    host.remote.reset();
    expect(&tss(&host, "check"), 0, &["Every repository is disabled. Check one anyway with: tss check REPO"]);
    assert!(host.remote.log().is_empty());
    expect(&tss(&host, "check fx"), 1, &["[fx] (disabled): Needs attention (1)", "! alpha: Edited locally"]);
}

#[test]
fn repositories_are_added_once_and_named_unambiguously() {
    let host = TestHost { tokens: FakeTokens::some("tok"), ..TestHost::empty("cli-add") };
    expect(&tss(&host, "status"), 0, &["No repositories are set up. Add one with: tss add OWNER/NAME"]);
    expect(&tss(&host, "check"), 0, &["No repositories are set up"]);

    host.remote.seed_standard(REPO);
    let ran = tss(&host, "add o/fixtures --every 6h --yes");
    expect(&ran, 0, &["Added o/fixtures as 'o-fixtures'. Checked every 6 hours.", "tss check o-fixtures"]);
    let config = host.config();
    assert_eq!(config.sources().len(), 1);
    assert_eq!(config.sources()[0].interval.map(|i| i.to_string()), Some("6h".to_string()));

    expect(&tss(&host, "add O/Fixtures --yes"), 3, &["This source is already set up. Its ID is 'o-fixtures'."]);
    expect(&tss(&host, "add not-a-repo"), 3, &[]);
    expect(&tss(&host, "add https://github.com/o/fixtures/tree/main/skills --use-token --yes"), 0, &["as 'o-fixtures-2'", "sends the GitHub token"]);
    assert_eq!(host.config().sources().len(), 2);
    expect(&tss(&host, "check o/fixtures"), 3, &["Several sources match 'o/fixtures' (o-fixtures, o-fixtures-2); use the ID"]);
    expect(&tss(&host, "check nope"), 3, &["No source 'nope' is set up; the IDs are: o-fixtures, o-fixtures-2"]);
}

#[test]
fn source_and_settings_changes_round_trip_into_the_settings_file() {
    let host = TestHost::new("cli-source-settings");
    expect(&tss(&host, "settings"), 0, &["Defaults for every source:", "Skills: Sync on", "Ignored folders: "]);
    expect(&tss(&host, "settings types plugins=off agents=off"), 0, &["Saved."]);
    expect(&tss(&host, "settings ignore add vendor"), 0, &["Saved."]);
    expect(&tss(&host, "settings ignore remove tests"), 0, &["Saved."]);
    expect(&tss(&host, "settings repo-items on"), 0, &["Saved."]);
    expect(&tss(&host, "settings ignore add ../up"), 3, &["That folder name cannot be used"]);
    let global = host.config().global().clone();
    assert!(!global.types.plugins && !global.types.agents && global.types.skills && global.include_repo_items);
    assert!(global.ignore_folders.contains(&"vendor".to_string()) && !global.ignore_folders.contains(&"tests".to_string()));

    expect(&tss(&host, "source fx types agents=on skills=default"), 0, &["Saved. The change applies on the next check of 'fx'."]);
    expect(&tss(&host, "source fx-agents ignore allow samples"), 0, &["Saved."]);
    expect(&tss(&host, "source o/fixtures ignore add drafts"), 0, &["Saved."]);
    expect(&tss(&host, "source fx repo-items off"), 0, &["Saved."]);
    expect(&tss(&host, "source fx types instructions=default"), 0, &["Saved."]);
    expect(&tss(&host, "source fx repo-items off"), 0, &["Nothing changed"]);
    let source = host.config().find_source("fx").unwrap().clone();
    assert_eq!(
        (source.types.skills, source.types.agents, source.types.instructions),
        (None, Some(true), None),
        "a migrated source kept its old types until changed"
    );
    assert_eq!((source.ignore_allow, source.ignore_add, source.include_repo_items), (vec!["samples".to_string()], vec!["drafts".to_string()], Some(false)));
    expect(&tss(&host, "source fx-agents"), 0, &["[fx]", "Also ignored for this source", "drafts", "Not ignored for this source", "samples"]);
    expect(&tss(&host, "source nope"), 3, &["No source 'nope'"]);
    assert!(host.remote.log().is_empty(), "settings commands never ask GitHub");
}

#[test]
fn status_shows_one_entry_per_source_grouped_by_type_from_what_was_seen() {
    use tiny_skill_sync::inventory::{Summary, SummaryNote};
    let host = TestHost::new("cli-status-seen");
    expect(&tss(&host, "check"), 0, &["Installed alpha"]);
    expect(&tss(&host, "source fx types agents=on"), 0, &["Saved."]);
    let summary = Summary {
        found: [3, 2, 1, 0],
        installable: [3, 2, 1, 0],
        filtered: [0, 4, 0, 0],
        notes: vec![SummaryNote { kind: None, path: "AGENTS.md".into(), reason: "It is read by Copilot from the repository itself".into() }],
        more_notes: 0,
    };
    let seen = BTreeMap::from([("fx".to_string(), tiny_skill_sync::seen::Seen { at: 1, branch: "main".into(), summary })]);
    tiny_skill_sync::seen::save(&host.source_paths(host.world.skills.clone()).seen_file(), &seen).unwrap();

    let ran = tss(&host, "status");
    expect(
        &ran,
        0,
        &[
            "Skills 3 | Agents 2 | Instructions: Sync off",
            "Skipped 4 in repository-specific or ignored folders",
            "Not synced: AGENTS.md",
            "    alpha: Installed",
        ],
    );
    assert_eq!(ran.out.matches("[fx").count(), 1, "one entry for the source, not one per type:\n{ran:?}");
    assert!(!ran.out.contains("Plugins"), "a type neither found nor owned is not shown:\n{ran:?}");
}

#[test]
fn remove_takes_away_every_type_of_a_source() {
    let host = TestHost::new("cli-remove-source");
    expect(&tss(&host, "settings types agents=off instructions=off plugins=off"), 0, &[]);
    expect(&tss(&host, "check"), 0, &["Installed alpha"]);
    assert_eq!(host.config().lane_ids("fx").len(), 4);
    expect(&tss(&host, "remove fx-agents --uninstall-plugins"), 3, &["only for a source with plugins"]);
    expect(&tss(&host, "remove fx-agents --delete-skills"), 0, &["Removed", "[fx]"]);
    let cfg = host.config();
    assert!(cfg.sources().is_empty() && cfg.repos().is_empty(), "every lane went with the source: {cfg:?}");
    assert!(host.world.skill_names().is_empty(), "proven copies were deleted");
}

#[test]
fn add_previews_every_type_and_saves_only_what_was_confirmed() {
    let host = TestHost::empty("cli-source-preview");
    host.remote.put(REPO, "caf\u{e9}/SKILL.md", "one");
    host.remote.put(REPO, "ordinary.agent.md", "one");
    host.remote.put(REPO, "tests/hidden.agent.md", "skipped");
    host.remote.put(REPO, "AGENTS.md", "never");
    let before = host.world.snapshot();
    let preview = tss(&host, "add o/fixtures --preview");
    expect(
        &preview,
        0,
        &[
            "Read-only preview of o/fixtures",
            "(default branch)",
            "Skills: Sync on",
            "caf\\u{e9}",
            "Destination:",
            "Agents: Sync on",
            "ordinary.agent.md: will sync",
            "Skipped 1 by a folder rule",
            "Instructions: Sync on",
            "None found.",
            "Never synced:",
            "AGENTS.md",
            "Nothing was saved or installed.",
        ],
    );
    assert!(preview.out.is_ascii());
    assert_eq!(host.world.snapshot(), before);
    assert!(host.plugins.calls().is_empty(), "a preview never starts Copilot CLI");

    host.confirm_add.set(Some(false));
    expect(&tss(&host, "add o/fixtures"), 0, &["Skills: Sync on", "Cancelled. Nothing was saved or installed."]);
    assert_eq!(host.world.snapshot(), before);

    host.interactive.set(false);
    expect(&tss(&host, "add o/fixtures"), 3, &["not a terminal", "--yes"]);
    assert_eq!(host.world.snapshot(), before);
    host.interactive.set(true);

    host.remote.set_truncated(REPO, true);
    host.confirm_add.set(Some(true));
    expect(&tss(&host, "add o/fixtures"), 2, &["Nothing was saved or installed."]);
    assert_eq!(host.world.snapshot(), before);
    host.remote.set_truncated(REPO, false);
    expect(&tss(&host, "add o/fixtures --type agents"), 3, &["tss source REPO types"]);
    host.confirm_add.set(Some(true));
    expect(&tss(&host, "add o/fixtures"), 0, &["Agents: Sync on", "Added o/fixtures as 'o-fixtures'."]);
    let cfg = host.config();
    assert_eq!(cfg.sources().len(), 1);
    assert_eq!(cfg.lane_ids("o-fixtures").len(), 4, "one source fans out into a lane per type");
    assert!(host.world.skill_names().is_empty() && !host.world.state_file().exists());
}

#[test]
fn source_preview_rechecks_listing_before_saving() {
    let host = TestHost::empty("cli-source-stale");
    host.remote.put(REPO, "first/SKILL.md", "one");
    let remote = Arc::downgrade(&host.remote);
    let mut listings = 0;
    host.remote.on_call(move |line| {
        if line.starts_with("tree ") {
            listings += 1;
            if listings == 2 {
                remote.upgrade().unwrap().put(REPO, "second/SKILL.md", "two");
            }
        }
    });
    host.confirm_add.set(Some(true));
    let before = host.world.snapshot();
    expect(&tss(&host, "add o/fixtures"), 2, &["first", "changed since the preview"]);
    assert_eq!(host.world.snapshot(), before);
}

#[test]
fn add_refuses_to_save_when_the_settings_changed_while_it_asked() {
    let host = TestHost::empty("cli-source-settings-changed");
    host.remote.put(REPO, "first/SKILL.md", "one");
    let mut other = Config::default();
    other.add_source(SourceKind::Skills, &RepoSpec::parse("o/other").unwrap(), None, false);
    // The first read is the one `add` previews with; the second is the reread just before saving.
    *host.change_before_second_home_read.lock().unwrap() = Some(other.clone());
    host.confirm_add.set(Some(true));
    expect(&tss(&host, "add o/fixtures"), 2, &["The settings changed. Preview the source again."]);
    assert_eq!(host.config(), other);
}

#[test]
fn add_without_preview_or_yes_never_saves_from_a_script() {
    let host = TestHost::empty("cli-source-script");
    host.remote.put(REPO, "first/SKILL.md", "one");
    host.interactive.set(false);
    let before = host.world.snapshot();
    expect(&tss(&host, "add o/fixtures"), 3, &["Cannot ask for confirmation"]);
    assert_eq!(host.world.snapshot(), before);
    assert!(host.remote.log().is_empty(), "refused before asking GitHub");
    expect(&tss(&host, "add o/fixtures --preview"), 0, &["Nothing was saved or installed."]);
    expect(&tss(&host, "add o/fixtures --yes"), 0, &["Added o/fixtures as 'o-fixtures'."]);
}

const PLUGIN_URL: &str = "https://github.com/o/fixtures/tree/main/pack";

#[test]
fn add_of_a_branch_github_does_not_have_saves_nothing() {
    let host = plugin_host("cli-plugin-branch-chars");
    let before = host.world.snapshot();
    for branch in ["bad~ref", "caf\u{e9}"] {
        for flag in [" --yes", " --preview"] {
            let ran = tss(&host, &format!("add https://github.com/o/fixtures/tree/{branch}/pack{flag}"));
            expect(&ran, 2, &["Could not read o/fixtures", "Nothing was saved or installed."]);
        }
    }
    assert!(host.plugins.calls().is_empty());
    assert_eq!(host.world.snapshot(), before);
}

#[test]
fn plugin_cli_preview_says_when_a_publisher_is_not_declared() {
    let host = plugin_host("cli-plugin-no-publisher");
    host.remote.put(REPO, "pack/alpha/plugin.json", r#"{"name":"alpha","version":"1.0.0"}"#);
    expect(&tss(&host, &format!("add {PLUGIN_URL} --preview")), 0, &["Publisher: not stated. Version: 1.0.0."]);
}

#[test]
fn add_approves_the_previewed_plugins_only_when_asked() {
    let plugins_lane = |host: &TestHost| host.config().sources()[0].lanes.id(SourceKind::Plugins).unwrap().to_string();

    let host = plugin_host("cli-add-plugins-unapproved");
    expect(&tss(&host, &format!("add {PLUGIN_URL} --yes")), 0, &["Its plugins wait for your approval: alpha."]);
    assert!(host.world.state().plugin_sources().get(&plugins_lane(&host)).is_none_or(|record| record.approved.is_empty()));
    assert!(host.plugins.calls().is_empty(), "adding without approval never starts Copilot CLI");

    let host = plugin_host("cli-add-plugins-approved");
    expect(&tss(&host, &format!("add {PLUGIN_URL} --approve-plugins --yes")), 0, &["Approved 1 plugin: alpha"]);
    assert_eq!(host.world.state().plugin_sources()[&plugins_lane(&host)].approved, [("alpha".into(), Some("Review Team".into()))].into());
}

fn plugin_host(test: &str) -> TestHost {
    let host = TestHost::empty(test);
    host.remote.put(
        REPO,
        "marketplace.json",
        r#"{"name":"tools","plugins":[{"name":"alpha","source":"./pack/alpha"},{"name":"other","source":{"url":"https://example.test/other"}}]}"#,
    );
    host.remote.put(REPO, "pack/alpha/plugin.json", r#"{"name":"alpha","version":"1.0.0","author":{"name":"Review Team"},"mcpServers":{}}"#);
    host
}

/// A Plugins source saved and approved the way the app window does it; `tss add` approves through the location path.
fn approved_plugin_source(host: &TestHost) -> String {
    use tiny_skill_sync::engine::Engine;
    let mut cfg = Config::default();
    let id = cfg.add_source(SourceKind::Plugins, &RepoSpec::parse(PLUGIN_URL).unwrap(), None, false).id.clone();
    host.save(&cfg);
    let paths = host.source_paths(host.world.skills.clone());
    let mut engine = Engine::new(paths, host.remote.client(), host.tokens.provider()).with_plugin_manager(host.plugins.manager());
    let lane = cfg.find(&id).unwrap().clone();
    let preview = engine.preview_source(&cfg, &lane, &std::sync::atomic::AtomicBool::new(false)).unwrap();
    engine.approve_plugin_preview(&cfg, &id, &preview).unwrap();
    id
}

#[test]
fn plugin_cli_parses_add_and_explicit_choices() {
    use cli::{AddSource, Command};
    use tiny_skill_sync::plugins::Choice;
    let parse = |words: &[&str]| cli::parse(&words.iter().map(|s| (*s).into()).collect::<Vec<_>>());
    assert_eq!(
        parse(&["add", PLUGIN_URL, "--approve-plugins", "--yes"]),
        Ok(Command::Add(AddSource { spec: PLUGIN_URL.into(), approve_plugins: true, yes: true, ..AddSource::default() }))
    );
    assert_eq!(
        parse(&["resolve", "r", "alpha", "approve", "Review Team"]),
        Ok(Command::ResolvePlugin { repo: "r".into(), choice: Choice::Approve { name: "alpha".into(), publisher: Some("Review Team".into()) } })
    );
    for (word, choice) in
        [("reinstall", Choice::Reinstall("alpha".into())), ("uninstall", Choice::Uninstall("alpha".into())), ("forget", Choice::Forget("alpha".into()))]
    {
        assert_eq!(parse(&["resolve", "r", "alpha", word]), Ok(Command::ResolvePlugin { repo: "r".into(), choice }));
    }
    assert_eq!(
        parse(&["remove", "r", "--delete-skills", "--uninstall-plugins"]),
        Ok(Command::Remove { repo: "r".into(), delete_skills: true, uninstall_plugins: true })
    );
}

#[test]
fn plugin_cli_shared_sources_are_held_and_approval_must_name_the_publisher() {
    let host = plugin_host("cli-plugin-shared");
    let mut cfg = Config::default();
    let id = cfg.add_source(SourceKind::Plugins, &RepoSpec::parse(PLUGIN_URL).unwrap(), None, false).id.clone();
    tiny_skill_sync::portable::export(&host.executable_dir().unwrap(), &cfg).unwrap();
    let ran = tss(&host, "sync");
    expect(&ran, 1, &["alpha: Needs your approval", &format!("tss resolve {id} alpha approve \"Review Team\"")]);
    for publisher in ["", " Wrong"] {
        let ran = tss(&host, &format!("resolve {id} alpha approve{publisher}"));
        expect(&ran, 2, &["The plugin 'alpha' is published by 'Review Team'.", &format!("tss resolve {id} alpha approve \"Review Team\"")]);
        assert!(ran.out.is_ascii() && ran.err.is_ascii());
    }
    assert!(host.world.state().plugin_sources().get(&id).is_none_or(|record| record.approved.is_empty()));
    assert!(host.plugins.calls().iter().all(|c| matches!(c, PluginCall::Home | PluginCall::List | PluginCall::Markets)));
}

#[test]
fn plugin_cli_choices_are_usage_errors_on_the_wrong_source_type() {
    let host = TestHost::new("cli-plugin-wrong-kind");
    let mut cfg = host.config();
    let id = cfg.add_source(SourceKind::Plugins, &RepoSpec::parse(PLUGIN_URL).unwrap(), None, false).id.clone();
    host.save(&cfg);
    expect(&tss(&host, "check fx"), 0, &["alpha"]);
    let before = host.world.snapshot();
    for (command, message) in [
        ("resolve fx alpha forget".into(), "only for plugins, and plugins do not sync from 'fx'"),
        ("resolve fx ALPHA forget".into(), "tss source fx types plugins=on"),
        (format!("resolve {id} ALPHA forget"), "Use the plugin name"),
        (format!("resolve {id} alpha keep"), "For Plugins, use approve, reinstall, uninstall or forget"),
        ("remove fx --uninstall-plugins".into(), "only for a source with plugins"),
    ] {
        expect(&tss(&host, &command), 3, &[message]);
        assert!(host.plugins.calls().is_empty());
        assert_eq!(host.world.snapshot(), before);
    }
}

#[test]
fn plugin_cli_check_shows_an_existing_install_without_an_attention_exit_code() {
    use tiny_skill_sync::plugins::native::{MarketOrigin, NativeMarket, NativePlugin, NativeSnapshot};

    let host = plugin_host("cli-plugin-already-installed");
    approved_plugin_source(&host);
    host.plugins.set_snapshot(NativeSnapshot {
        home: host.world.root.join("native"),
        markets: vec![NativeMarket {
            name: "tools".into(),
            source: "GitHub: o/fixtures".into(),
            is_default: false,
            origin: Some(MarketOrigin { kind: "github".into(), repo: Some(REPO.into()), reference: Some("main".into()) }),
        }],
        plugins: vec![NativePlugin { name: "alpha".into(), marketplace: "tools".into(), version: "2".into(), enabled: true, source: "installed".into() }],
    });
    expect(&tss(&host, "check"), 0, &["alpha: Already installed in Copilot from somewhere else; left as it is."]);
    assert!(host.world.state().plugin_sources().values().all(|r| r.plugins.is_empty()));
}

#[test]
fn plugin_cli_add_skips_ids_used_by_all_three_state_sections() {
    use tiny_skill_sync::source::{Entry, resolve_destination};
    use tiny_skill_sync::state::{PluginSourceState, State};

    let host = plugin_host("cli-plugin-add-id");
    let mut state = State::default();
    state.repo_mut("o-fixtures").last_check = Some(T0);
    let leaf = tiny_skill_sync::config::RepoConfig { kind: SourceKind::Agents, ..repo("o-fixtures-2", REPO, "") };
    let binding = resolve_destination(&leaf, &host.world.root.join("agents"), &["x.agent.md".into()], None, |_| Ok(Entry::Missing)).unwrap();
    state.bind_destination(&leaf, binding).unwrap();
    let native = tiny_skill_sync::config::RepoConfig { kind: SourceKind::Plugins, branch: "main".into(), ..repo("o-fixtures-3", REPO, "pack") };
    let record = PluginSourceState::new(&native, &host.world.root.join("native"), "tools");
    state.set_plugin_source(&native.id, record).unwrap();
    state.save(&host.world.state_file()).unwrap();

    expect(&tss(&host, &format!("add {PLUGIN_URL} --preview")), 0, &["alpha (pack/alpha): will sync after you approve it"]);
    assert!(host.plugins.calls().is_empty());
    host.confirm_add.set(Some(true));
    expect(&tss(&host, &format!("add {PLUGIN_URL}")), 0, &["as 'o-fixtures-4'", "Its plugins wait for your approval: alpha"]);
    assert_eq!(host.world.state(), state, "saving a source records nothing until it is checked or approved");
    assert!(host.config().find_source("o-fixtures-4").is_some());
    assert!(host.plugins.calls().is_empty(), "adding without approval never starts Copilot CLI");
}

#[test]
fn plugin_cli_add_refuses_a_location_that_is_already_set_up() {
    use tiny_skill_sync::state::State;

    let host = plugin_host("cli-plugin-preview-conflict");
    let mut cfg = Config::default();
    let id = cfg.add_source(SourceKind::Plugins, &RepoSpec::parse(PLUGIN_URL).unwrap(), None, false).id.clone();
    host.save(&cfg);
    let mut state = State::default();
    state.repo_mut(&id).last_check = Some(T0);
    state.save(&host.world.state_file()).unwrap();
    let before = host.world.snapshot();
    expect(&tss(&host, &format!("add {PLUGIN_URL} --preview")), 0, &[&format!("This location is already set up as '{id}'."), "Nothing was saved"]);
    host.confirm_add.set(Some(true));
    expect(&tss(&host, &format!("add {PLUGIN_URL} --yes")), 3, &[&format!("Its ID is '{id}'.")]);
    assert!(host.plugins.calls().is_empty());
    assert_eq!(host.world.snapshot(), before);
}

#[test]
fn plugin_cli_preview_is_read_only_without_copilot_and_shows_consent() {
    let host = plugin_host("cli-plugin-preview");
    let before = host.world.snapshot();
    let ran = tss(&host, &format!("add {PLUGIN_URL} --preview"));
    expect(
        &ran,
        0,
        &[
            "Branch: main\n",
            "Folder: pack",
            "Plugins: Sync on",
            "alpha (pack/alpha): will sync after you approve it",
            "Publisher: Review Team. Version: 1.0.0. Declares: MCP servers.",
            "Plugins can run programs on this PC.",
            "Nothing was saved or installed.",
        ],
    );
    assert!(ran.out.is_ascii());
    assert!(host.plugins.calls().is_empty());
    assert_eq!(host.world.snapshot(), before);
}

#[test]
fn plugin_cli_partial_uninstall_keeps_the_source_and_failed_record() {
    use tiny_skill_sync::plugins::native::{MarketOrigin, NativeMarket, NativePlugin, NativeSnapshot};

    let host = plugin_host("cli-plugin-remove");
    let home = host.world.root.join("native");
    let mut snapshot = NativeSnapshot {
        home,
        markets: vec![NativeMarket {
            name: "tools".into(),
            source: "GitHub: o/fixtures".into(),
            is_default: false,
            origin: Some(MarketOrigin { kind: "github".into(), repo: Some(REPO.into()), reference: Some("main".into()) }),
        }],
        plugins: vec![],
    };
    host.remote.put(REPO, "marketplace.json", r#"{"name":"tools","plugins":[{"name":"alpha","source":"pack/alpha"},{"name":"beta","source":"pack/beta"}]}"#);
    host.remote.put(REPO, "pack/beta/plugin.json", r#"{"name":"beta","version":"1.0.0"}"#);
    host.plugins.set_snapshot(snapshot.clone());
    approved_plugin_source(&host);
    assert_eq!(host.plugins.calls(), [PluginCall::Home], "recording approval does not start Copilot");
    let cfg = host.config();
    let id = &cfg.repos()[0].id;
    for name in ["alpha", "beta"] {
        snapshot.plugins.push(NativePlugin {
            name: name.into(),
            marketplace: "tools".into(),
            version: "1.0.0".into(),
            enabled: true,
            source: "installed".into(),
        });
        host.plugins.push(PluginCall::Install(format!("{name}@tools")), PluginReply::Done(Some(snapshot.clone())));
    }
    expect(&tss(&host, "sync"), 0, &["alpha", "beta"]);
    let calls = host.plugins.calls();
    expect(&tss(&host, "status"), 0, &["alpha: Last recorded. Version last recorded by this app; sync to check Copilot now", "1.0.0", "Review Team"]);
    assert_eq!(host.plugins.calls(), calls, "status only reads the local record");

    snapshot.plugins.remove(0);
    host.plugins.push(PluginCall::Uninstall("alpha@tools".into()), PluginReply::Done(Some(snapshot.clone())));
    host.plugins.push(PluginCall::Uninstall("beta@tools".into()), PluginReply::Done(None));
    expect(&tss(&host, &format!("remove {id} --uninstall-plugins")), 2, &["source was kept", "did not confirm"]);
    assert_eq!(host.config(), cfg);
    let state = host.world.state();
    assert_eq!(state.plugin_sources()[id].plugins.keys().collect::<Vec<_>>(), &["beta"]);
    assert_eq!(host.plugins.remaining(), 0);

    let calls = host.plugins.calls();
    expect(&tss(&host, &format!("remove {id}")), 0, &["Plugins stay installed in Copilot."]);
    assert_eq!(host.plugins.calls(), calls);
    assert!(host.config().repos().is_empty());
}

#[test]
fn source_ids_disambiguate_types_and_delete_skills_never_means_files() {
    let host = TestHost::empty("cli-source-select");
    let spec = RepoSpec::parse(REPO).unwrap();
    let mut cfg = Config::default();
    let skills = cfg.add_source(SourceKind::Skills, &spec, None, false).id.clone();
    let agents = cfg.add_source(SourceKind::Agents, &spec, None, false).id.clone();
    host.save(&cfg);
    let before = host.world.snapshot();
    expect(&tss(&host, "resolve o/fixtures mine.agent.md keep"), 3, &["Several sources match", &skills, &agents]);
    expect(&tss(&host, "remove o/fixtures --delete-skills"), 3, &["Several sources match", &skills, &agents]);
    expect(&tss(&host, &format!("remove {agents} --delete-skills")), 3, &["only for a source with skills"]);
    assert_eq!(cli::find_source(&cfg, &agents).unwrap().lanes.id(SourceKind::Agents), Some(agents.as_str()));
    assert_eq!(host.world.snapshot(), before);
    assert!(host.remote.log().is_empty());
}

#[test]
fn hand_written_sources_keep_their_lane_ids_after_a_check_records_items() {
    use tiny_skill_sync::config::KINDS;
    let ids = |cfg: &Config| {
        let s = &cfg.sources()[0];
        (s.id.clone(), KINDS.map(|k| s.lanes.id(k).map(str::to_owned)))
    };
    let mut drift = Vec::new();
    for (label, source) in [("with-id", r#"{"id":"kit","repo":"o/fixtures"}"#), ("no-id", r#"{"repo":"o/fixtures"}"#)] {
        let host = TestHost::empty(&format!("cli-hand-lanes-{label}"));
        host.remote.put(REPO, "alpha/SKILL.md", "one\n");
        host.remote.put(REPO, "helper.agent.md", "two\n");
        std::fs::write(host.config_file(), format!(r#"{{"version":3,"sources":[{source}]}}"#)).unwrap();
        let before = ids(&host.config());

        expect(&tss(&host, "check"), 0, &["Installed alpha"]);
        assert!(host.world.root.join("agents").join("helper.agent.md").exists(), "{label}: the agent was not installed");
        let after = ids(&host.config());
        let again = tss(&host, "check");
        if after != before || again.code != 0 || again.out.contains("already provides") || again.out.contains("settings changed") {
            drift.push(format!("{label}: before the check {before:?}, after it {after:?}; the next check: {again:?}"));
        }
    }
    assert!(drift.is_empty(), "lane or source IDs of a hand-written source changed once a check recorded items under them:\n{}", drift.join("\n"));
}

#[test]
fn turning_a_type_on_never_adopts_an_orphan_record_of_the_same_kind_from_another_repository() {
    let host = TestHost::empty("cli-type-on-orphan");
    host.remote.put("o/one", "old.agent.md", "old\n");
    host.remote.put(REPO, "helper.agent.md", "two\n");
    std::fs::write(host.config_file(), r#"{"version":3,"sources":[{"id":"tool","repo":"o/one","types":{"skills":false},"lanes":{"agents":"tool-agents"}}]}"#)
        .unwrap();
    expect(&tss(&host, "check"), 0, &[]);
    let old = host.world.root.join("agents").join("old.agent.md");
    assert!(old.exists(), "the first agent was not installed");

    std::fs::write(
        host.config_file(),
        r#"{"version":3,"sources":[{"id":"tool","repo":"o/fixtures","types":{"agents":false},"lanes":{"agents":"tool-agents"}}]}"#,
    )
    .unwrap();
    expect(&tss(&host, "source tool types agents=on"), 0, &["Saved."]);
    let lane = host.config().sources()[0].lanes.id(SourceKind::Agents).map(str::to_owned);
    assert_ne!(lane.as_deref(), Some("tool-agents"), "the lane kept the ID of an orphan record from another repository");
    let ran = tss(&host, "check");
    assert_eq!(ran.code, 0, "{ran:?}");
    assert!(host.world.root.join("agents").join("helper.agent.md").exists(), "the agent was not installed: {ran:?}");
    assert_eq!(std::fs::read_to_string(&old).unwrap(), "old\n", "the orphan record's file was touched");
}

#[test]
fn turning_a_type_on_never_attaches_its_lane_to_another_kinds_records() {
    let host = TestHost::empty("cli-type-on-lane");
    let instructions = host.world.root.join("instructions").join("rules.instructions.md");
    host.remote.put("o/one", "rules.instructions.md", "rules\n");
    host.remote.put(REPO, "helper.agent.md", "agent\n");
    // An earlier source recorded instructions under the ID "z"...
    let earlier = r#"{"version":3,"sources":[{"id":"s","repo":"o/one","lanes":{"skills":"s","agents":"s-a","instructions":"z","plugins":"s-p"}}]}"#;
    std::fs::write(host.config_file(), earlier).unwrap();
    expect(&tss(&host, "check"), 0, &[]);
    assert!(instructions.exists(), "fixture: the instructions file was not installed");
    let recorded = host.world.state().loose_sources()["z"].clone();
    assert_eq!(recorded.binding().kind(), SourceKind::Instructions);
    // ...and the settings that replace it (for example from another PC) keep "z" as an agents lane that is off.
    let now = r#"{"version":3,"sources":[{"id":"kit","repo":"o/fixtures","types":{"agents":false},
        "lanes":{"skills":"kit","agents":"z","instructions":"kit-i","plugins":"kit-p"}}]}"#;
    std::fs::write(host.config_file(), now).unwrap();

    expect(&tss(&host, "source kit types agents=on"), 0, &["Saved."]);
    let agents_lane = host.config().find_source("kit").unwrap().lanes.id(SourceKind::Agents).unwrap().to_string();
    let ran = tss(&host, "check");
    let mut problems = Vec::new();
    if agents_lane == "z" {
        problems.push("the agents lane was turned on under 'z', the ID of a recorded instructions source".to_string());
    }
    if ran.code != 0 {
        problems.push(format!("the next check failed: {ran:?}"));
    }
    if !host.world.root.join("agents").join("helper.agent.md").exists() {
        problems.push("the agent was not installed".into());
    }
    if host.world.state().loose_sources().get("z") != Some(&recorded) || !instructions.exists() {
        problems.push("the instructions record 'z' or its file changed".into());
    }
    assert!(problems.is_empty(), "turning agents on attached the lane to another kind's state record:\n{}", problems.join("\n"));
}

#[test]
fn turning_a_type_on_globally_never_attaches_a_lane_to_another_kinds_records() {
    let host = TestHost::empty("cli-global-type-on-lane");
    let instructions = host.world.root.join("instructions").join("rules.instructions.md");
    host.remote.put("o/one", "rules.instructions.md", "rules\n");
    host.remote.put(REPO, "helper.agent.md", "agent\n");
    // An earlier source recorded instructions under the ID "kit-agents"...
    let earlier = r#"{"version":3,"sources":[{"id":"s","repo":"o/one","lanes":{"skills":"s","agents":"s-a","instructions":"kit-agents","plugins":"s-p"}}]}"#;
    std::fs::write(host.config_file(), earlier).unwrap();
    expect(&tss(&host, "check"), 0, &[]);
    assert!(instructions.exists(), "fixture: the instructions file was not installed");
    let recorded = host.world.state().loose_sources()["kit-agents"].clone();
    assert_eq!(recorded.binding().kind(), SourceKind::Instructions);
    // ...and the settings that replace it keep "kit-agents" as the agents lane of a source that follows the global
    // default, which has agents off.
    let now = r#"{"version":3,"global":{"types":{"agents":false}},"sources":[{"id":"kit","repo":"o/fixtures",
        "lanes":{"skills":"kit","agents":"kit-agents","instructions":"kit-instructions","plugins":"kit-plugins"}}]}"#;
    std::fs::write(host.config_file(), now).unwrap();

    expect(&tss(&host, "settings types agents=on"), 0, &["Saved."]);
    let agents_lane = host.config().find_source("kit").unwrap().lanes.id(SourceKind::Agents).unwrap().to_string();
    let ran = tss(&host, "check");
    let mut problems = Vec::new();
    if agents_lane == "kit-agents" {
        problems.push("the agents lane was turned on under 'kit-agents', the ID of a recorded instructions source".to_string());
    }
    if ran.code != 0 {
        problems.push(format!("the next check failed: {ran:?}"));
    }
    if !host.world.root.join("agents").join("helper.agent.md").exists() {
        problems.push("the agent was not installed".into());
    }
    if host.world.state().loose_sources().get("kit-agents") != Some(&recorded) || !instructions.exists() {
        problems.push("the instructions record 'kit-agents' or its file changed".into());
    }
    assert!(problems.is_empty(), "turning agents on in the global settings attached a lane to another kind's state record:\n{}", problems.join("\n"));
}

#[test]
fn a_repository_whose_skills_are_in_a_subfolder_installs_them_from_the_repository_root() {
    let host = TestHost::empty("cli-subfolder");
    expect(&tss(&host, "settings types agents=off instructions=off plugins=off"), 0, &["Agents: Sync off"]);
    host.remote.seed_standard("o/fixtures");
    expect(&tss(&host, "add o/fixtures --yes"), 0, &[]);

    // Discovery is recursive: a source added as a whole finds the skills below its root.
    let ran = tss(&host, "check");
    expect(&ran, 0, &["Installed alpha"]);
    assert!(!ran.out.contains("No skills found"), "{ran:?}");
    assert!(ran.out.is_ascii(), "{ran:?}");
    assert_eq!(host.world.skill_names(), ["alpha", "beta", "gamma"]);
    // A check that GitHub answers with "not modified" still knows what the listing had in it.
    expect(&tss(&host, "check"), 0, &[]);
    expect(&tss(&host, "status"), 0, &["[o-fixtures]"]);
}

#[test]
fn a_source_that_holds_only_skills_says_nothing_about_the_other_types_it_looks_for() {
    let host = TestHost::empty("cli-only-skills");
    host.remote.seed_standard("o/fixtures");
    expect(&tss(&host, "add https://github.com/o/fixtures/tree/main/skills --yes"), 0, &[]);

    // Every type is on by default; a type the folder does not have is no problem while another type is there.
    let ran = tss(&host, "check");
    expect(&ran, 0, &["Installed alpha"]);
    for quiet in ["No agents", "No instruction", "No plugins", "Nothing found"] {
        assert!(!ran.out.contains(quiet), "'{quiet}' in:\n{ran:?}");
    }
    // A later process does not remember the empty types, only what the listing held.
    let ran = tss(&host, "status");
    expect(&ran, 0, &["Up to date", "Last check: just now"]);
    assert!(!ran.out.contains("Not checked yet"), "{ran:?}");
    expect(&tss(&host, "check"), 0, &[]);
}

#[test]
fn a_repository_whose_only_skill_is_held_is_not_reported_as_empty() {
    let host = TestHost::empty("cli-held-only");
    expect(&tss(&host, "settings types agents=off instructions=off plugins=off"), 0, &["Agents: Sync off"]);
    host.remote.put("o/one", "skills/mine/SKILL.md", "upstream\n");
    host.world.write("mine/SKILL.md", "my own\n");
    expect(&tss(&host, "add https://github.com/o/one/tree/main/skills --yes"), 0, &[]);

    let ran = tss(&host, "check");
    expect(&ran, 1, &["mine", "already exists"]);
    assert!(!ran.out.contains("No skills found"), "{ran:?}");
    // The record owns nothing from it, so `status` has no rows for it, but its listing was not empty.
    let status = tss(&host, "status");
    assert!(!status.out.contains("No skills found"), "{status:?}");
    assert_eq!(host.world.read("mine/SKILL.md"), "my own\n");
}

#[test]
fn a_repository_whose_only_skill_is_refused_is_not_reported_as_empty() {
    let host = TestHost::empty("cli-refused-only");
    expect(&tss(&host, "settings types agents=off instructions=off plugins=off"), 0, &["Agents: Sync off"]);
    host.remote.put("o/one", "skills/a\u{1b}b/SKILL.md", "x\n");
    expect(&tss(&host, "add https://github.com/o/one/tree/main/skills --yes"), 0, &[]);

    let ran = tss(&host, "check");
    expect(&ran, 1, &["a\\u{1b}b: Skipped: not safe to install"]);
    assert!(!ran.out.contains("No skills found"), "{ran:?}");
    // A refused skill is not in the record either, so after the check only the listing tells it apart from nothing.
    let status = tss(&host, "status");
    assert!(!status.out.contains("No skills found"), "{status:?}");
    assert!(host.world.skill_names().is_empty());
}

#[test]
fn removing_a_repository_leaves_its_skills_unless_asked() {
    let host = TestHost::new("cli-remove");
    expect(&tss(&host, "check"), 0, &[]);
    let ran = tss(&host, "remove fx");
    expect(&ran, 0, &["Removed o/fixtures : skills [fx] from the settings.", "These skills stay installed and are now yours: alpha, beta, gamma"]);
    assert!(host.config().repos().is_empty());
    assert!(host.world.state().repo("fx").is_none());
    assert_eq!(host.world.skill_names(), ["alpha", "beta", "gamma"]);
    expect(&tss(&host, "status"), 0, &["No repositories are set up"]);

    let host = TestHost::new("cli-remove-delete");
    expect(&tss(&host, "check"), 0, &[]);
    host.world.write("alpha/SKILL.md", "mine\n");
    let ran = tss(&host, "remove fx --delete-skills");
    expect(&ran, 1, &["Deleted: ", "Kept alpha, which is now yours: "]);
    assert_eq!(host.world.skill_names(), ["alpha"]);
    assert_eq!(host.world.read("alpha/SKILL.md"), "mine\n");
}

#[test]
fn skills_of_a_repository_gone_from_the_settings_can_be_let_go() {
    let host = TestHost::new("cli-orphan");
    expect(&tss(&host, "check"), 0, &[]);
    host.save(&Config::default());
    expect(&tss(&host, "status"), 0, &["Items recorded for 'fx', which is no longer in the settings, are left alone.", "tss remove fx"]);
    expect(&tss(&host, "remove fx"), 0, &["Stopped tracking the skills recorded for 'fx'.", "now yours: alpha, beta, gamma"]);
    let ran = tss(&host, "status");
    assert!(!ran.out.contains("'fx'"), "{ran:?}");
    assert_eq!(host.world.skill_names(), ["alpha", "beta", "gamma"]);
    expect(&tss(&host, "remove fx"), 3, &["No source 'fx' is set up"]);
}

#[test]
fn the_token_is_saved_shown_and_cleared() {
    let host = TestHost::new("cli-token");
    expect(&tss(&host, "token"), 0, &["Saved token: none.", "GitHub CLI: the GitHub CLI is not signed in.", "No repository sends the token."]);
    let mut cfg = standard_config();
    cfg.update_lane_at(0, |r| r.use_token = true);
    host.save(&cfg);
    expect(&tss(&host, "token status"), 1, &["Repositories that send the token: fx", "tss token set"]);

    expect(&tss(&host, "token set"), 2, &["no token was given"]);
    *host.typed.lock().unwrap() = Some("ghp_example".to_string());
    expect(&tss(&host, "token set"), 0, &["saved in Windows Credential Manager"]);
    assert_eq!(host.saved.lock().unwrap().as_deref(), Some("ghp_example"));
    let ran = tss(&host, "token");
    expect(&ran, 0, &["Saved token: yes", "GitHub CLI: not asked"]);
    assert!(!ran.out.contains("ghp_example"), "the token is never shown:\n{ran:?}");

    expect(&tss(&host, "token clear"), 0, &["The saved token was removed."]);
    expect(&tss(&host, "token clear"), 0, &["No token was saved."]);
}

#[test]
fn start_with_windows_is_shown_and_switched_and_nothing_else_is_touched() {
    let host = TestHost::new("cli-startup");
    let before = host.world.snapshot();
    let set = |state: Startup| *host.startup.lock().unwrap() = Ok(state);
    expect(&tss(&host, "startup"), 0, &["Start with Windows: off\nStarts in the tray when you sign in to Windows, without opening its window.\n"]);
    expect(&tss(&host, "startup on"), 0, &["Start with Windows: on\n"]);
    expect(&tss(&host, "startup status"), 0, &["Start with Windows: on\n"]);
    expect(&tss(&host, "Startup OFF"), 0, &["Start with Windows: off\n"]);
    assert_eq!(*host.startup.lock().unwrap(), Ok(Startup::Off));

    set(Startup::OffInWindows);
    expect(&tss(&host, "startup"), 0, &["Start with Windows: off\n", "Task Manager or Settings). Run 'tss startup on' to start it again."]);
    let other = r"D:\old\tiny-skill-sync.exe";
    set(Startup::OtherCopy(other.to_string()));
    let ran = tss(&host, "startup off");
    expect(&ran, 0, &["Start with Windows: off\n", &format!("Windows starts another copy when you sign in: {other}. Run 'tss startup on'")]);
    assert!(ran.err.is_empty(), "{ran:?}");

    set(Startup::TooLong);
    expect(&tss(&host, "startup"), 0, &["Start with Windows: not possible\n", "path is too long for Windows to start it"]);
    expect(&tss(&host, "startup on"), 2, &["tss: Start with Windows could not be turned on: this copy's path is too long"]);
    *host.startup.lock().unwrap() = Err("Access is denied. (os error 5)".to_string());
    let ran = tss(&host, "startup");
    expect(&ran, 2, &["tss: Start with Windows cannot be read: Access is denied. (os error 5)"]);
    assert!(ran.out.is_empty(), "nothing is said to be on or off: {ran:?}");
    expect(&tss(&host, "startup off"), 2, &["tss: Start with Windows could not be turned off: Access is denied."]);

    assert_eq!(host.world.snapshot(), before, "no settings, state or skills");
    assert!(host.remote.log().is_empty(), "nothing is asked of GitHub");
}

#[test]
fn a_repository_found_to_be_private_is_shown_as_sending_the_token() {
    let host = TestHost { tokens: FakeTokens::some("tok"), gh: Ok(()), ..TestHost::new("cli-auto-token") };
    host.remote.make_private(REPO, "tok");

    expect(&tss(&host, "check"), 0, &[]);

    assert_eq!(host.world.skill_names(), ["alpha", "beta", "gamma"], "read with the token without being told");
    assert!(!host.config().repos()[0].use_token, "the settings are not changed");
    expect(&tss(&host, "token status"), 0, &["Repositories that send the token: fx"]);
    expect(&tss(&host, "status"), 0, &["sends the GitHub token"]);
}

#[test]
fn a_bad_command_line_changes_nothing() {
    let host = TestHost::new("cli-usage");
    let before = host.world.snapshot();
    expect(&tss(&host, ""), 3, &["tss: no command was given", "Run 'tss help'"]);
    expect(&tss(&host, "frobnicate"), 3, &["unknown command 'frobnicate'"]);
    expect(&tss(&host, "check fx --force"), 3, &["'check' has no option --force"]);
    expect(&tss(&host, "resolve nope alpha keep"), 3, &["No source 'nope'"]);
    assert_eq!(host.world.snapshot(), before);
    assert!(host.remote.log().is_empty());
    expect(&tss(&host, "help"), 0, &["Usage:", "Exit codes:"]);
}

#[test]
fn broken_settings_are_reported_not_replaced() {
    let host = TestHost::new("cli-broken");
    std::fs::write(host.config_file(), "{ not json").unwrap();
    expect(&tss(&host, "status"), 2, &["config.json"]);
    expect(&tss(&host, "add o/other"), 2, &["config.json"]);
    assert_eq!(std::fs::read_to_string(host.config_file()).unwrap(), "{ not json");
}

#[test]
fn portable_companions_select_and_update_shared_settings_without_changing_local_config() {
    let host = TestHost::empty("cli-portable-route");
    host.save(&standard_config());
    let local_before = std::fs::read(host.config_file()).unwrap();
    let executable_dir = host.world.root.join("exe");

    std::fs::write(executable_dir.join(SETTINGS_FILE), r#"{"version":1,"default_interval":"12h"}"#).unwrap();
    let settings_only = tss(&host, "status");
    expect(&settings_only, 0, &["No repositories are set up"]);
    assert!(!settings_only.out.contains("[fx]"), "local config was used:\n{settings_only:?}");

    std::fs::write(executable_dir.join(SETTINGS_FILE), "{ not json").unwrap();
    expect(&tss(&host, "status"), 2, &["tiny-skill-sync.settings.json is not valid"]);
    assert_eq!(std::fs::read(host.config_file()).unwrap(), local_before, "a portable settings error must not fall back to config.json");

    std::fs::remove_file(executable_dir.join(SETTINGS_FILE)).unwrap();
    std::fs::write(executable_dir.join(SOURCES_FILE), r#"{"version":1,"repos":[{"id":"shared","repo":"o/shared","enabled":false,"interval":"6h"}]}"#).unwrap();
    let sources_only = tss(&host, "status");
    expect(&sources_only, 0, &["[shared] (disabled)", "Disabled: checked only when asked"]);
    assert!(!sources_only.out.contains("[fx]"), "local config was used:\n{sources_only:?}");

    host.remote.put("o/new", "one/SKILL.md", "one");
    expect(&tss(&host, "add o/new --every 3h --yes"), 0, &["Added o/new as 'o-new'."]);
    let sources: serde_json::Value = serde_json::from_slice(&std::fs::read(executable_dir.join(SOURCES_FILE)).unwrap()).unwrap();
    assert_eq!(sources["sources"].as_array().unwrap().len(), 2);
    assert_eq!(sources["sources"][0]["id"], "shared");
    assert_eq!(sources["sources"][0]["enabled"], false);
    assert_eq!(sources["sources"][0]["interval"], "6h");
    assert_eq!(sources["sources"][1]["id"], "o-new");
    assert_eq!(std::fs::read(host.config_file()).unwrap(), local_before, "portable commands must not mutate config.json");
}

#[test]
fn portable_export_omits_machine_local_skills_path_and_leaves_config_untouched() {
    let host = TestHost::empty("cli-portable-export");
    let mut local = standard_config();
    local.default_interval = tiny_skill_sync::interval::Interval::new(12, tiny_skill_sync::interval::Unit::Hour);
    local.skills_dir = Some(host.world.root.join("custom-skills").to_string_lossy().into_owned());
    local.update_lane_at(0, |r| r.enabled = false);
    local.update_lane_at(0, |r| r.interval = Some(tiny_skill_sync::interval::Interval::new(6, tiny_skill_sync::interval::Unit::Hour)));
    host.save(&local);
    let local_before = std::fs::read(host.config_file()).unwrap();

    expect(&tss(&host, "portable"), 0, &["Exported shared settings and repositories", "config.json was not changed"]);

    let executable_dir = host.world.root.join("exe");
    let settings: serde_json::Value = serde_json::from_slice(&std::fs::read(executable_dir.join(SETTINGS_FILE)).unwrap()).unwrap();
    assert_eq!(settings["version"], 1);
    assert_eq!(settings["default_interval"], "12h");
    assert!(settings.get("skills_dir").is_none(), "{settings}");
    let sources: serde_json::Value = serde_json::from_slice(&std::fs::read(executable_dir.join(SOURCES_FILE)).unwrap()).unwrap();
    assert_eq!(sources["sources"][0]["enabled"], false);
    assert_eq!(sources["sources"][0]["interval"], "6h");
    assert_eq!(std::fs::read(host.config_file()).unwrap(), local_before, "export must not mutate config.json");
}

#[test]
fn portable_export_refuses_to_overwrite_either_companion() {
    for existing in [SETTINGS_FILE, SOURCES_FILE] {
        let host = TestHost::empty("cli-portable-no-overwrite");
        host.save(&standard_config());
        let local_before = std::fs::read(host.config_file()).unwrap();
        let executable_dir = host.world.root.join("exe");
        let path = executable_dir.join(existing);
        std::fs::write(&path, b"keep exactly").unwrap();

        let ran = tss(&host, "portable");
        expect(&ran, 2, &["already exists; refusing to overwrite"]);
        assert_eq!(std::fs::read(path).unwrap(), b"keep exactly");
        let other = if existing == SETTINGS_FILE { SOURCES_FILE } else { SETTINGS_FILE };
        assert!(!executable_dir.join(other).exists(), "partial export was left behind");
        assert_eq!(std::fs::read(host.config_file()).unwrap(), local_before);
    }
}

/// Two machines share one portable folder. One machine's check holds the shared settings lock for a moment while it
/// installs; the other's check waits for it rather than failing and backing off for minutes.
#[test]
fn a_check_waits_while_another_machine_briefly_holds_the_shared_settings_lock() {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Foundation::GENERIC_WRITE;
    use windows_sys::Win32::Storage::FileSystem::{DELETE, FILE_FLAG_DELETE_ON_CLOSE};

    let host = TestHost::new("cli-portable-lock-wait");
    expect(&tss(&host, "portable"), 0, &["Exported shared settings and repositories"]);
    let held = std::fs::OpenOptions::new()
        .write(true)
        .access_mode(GENERIC_WRITE | DELETE)
        .create_new(true)
        .share_mode(0)
        .custom_flags(FILE_FLAG_DELETE_ON_CLOSE)
        .open(host.world.root.join("exe").join(".tss-settings.lock"))
        .unwrap();
    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(400));
        drop(held);
    });
    let ran = tss(&host, "check");
    release.join().unwrap();
    expect(&ran, 0, &["Installed alpha", "Installed beta", "Installed gamma"]);
    assert_eq!(host.world.skill_names(), ["alpha", "beta", "gamma"]);
}

// ---------------------------------------------------------------------------------------------------------------------
// The executable itself
// ---------------------------------------------------------------------------------------------------------------------

/// Only copies of locally built binaries are run; neither the build output nor downloaded files are execution targets.
fn world_with_binaries(name: &str) -> World {
    let world = World::new(name);
    let bin = world.root.join("bin");
    std::fs::create_dir(&bin).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_tss"), bin.join("tss.exe")).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_tiny-skill-sync"), bin.join("tiny-skill-sync.exe")).unwrap();
    world
}

/// `tss.exe` with its own executables, home and skills folder, talking to a loopback port nothing listens on.
fn exe(world: &World, dead: &str, args: &[&str]) -> Ran {
    let output = Command::new(world.root.join("bin").join("tss.exe"))
        .args(args)
        .env("TSS_HOME", &world.home)
        .env("TSS_SKILLS_DIR", &world.skills)
        .env("TSS_NO_APP_UPDATES", "1")
        .env("TSS_NO_SAVED_TOKEN", "1")
        .env("TSS_API_BASE", dead)
        .env("TSS_RAW_BASE", dead)
        .output()
        .unwrap();
    Ran { code: output.status.code().unwrap(), out: String::from_utf8(output.stdout).unwrap(), err: String::from_utf8(output.stderr).unwrap() }
}

#[test]
fn the_executable_runs_against_its_own_folders() {
    let world = world_with_binaries("cli-exe");
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let dead = format!("http://127.0.0.1:{port}");

    let ran = exe(&world, &dead, &["--version"]);
    expect(&ran, 0, &[&format!("tss {}", env!("CARGO_PKG_VERSION"))]);
    expect(&exe(&world, &dead, &["frobnicate"]), 3, &["unknown command"]);
    expect(&exe(&world, &dead, &["add", "o/fixtures"]), 3, &["input is not a terminal", "--yes"]);
    expect(&exe(&world, &dead, &["add", "o/fixtures", "--yes"]), 2, &["Could not read o/fixtures", "Nothing was saved or installed."]);
    assert!(!world.home.join(CONFIG_FILE).exists());
    let mut cfg = Config::default();
    cfg.add_source(SourceKind::Skills, &RepoSpec::parse(REPO).unwrap(), None, false);
    cfg.save(&world.home.join(CONFIG_FILE)).unwrap();

    let ran = exe(&world, &dead, &["status"]);
    expect(&ran, 0, &[&format!("Skills folder: {}", world.skills.display()), "Last check: never."]);

    let ran = exe(&world, &dead, &["check"]);
    expect(&ran, 2, &["[o-fixtures]: No connection", "Nothing is deleted because a check failed."]);
    assert!(world.skill_names().is_empty());
}

/// The startup entry's keys of an app folder, which Windows never reads. Deleted when the test ends.
struct OwnKeys(Keys);

impl OwnKeys {
    fn of(home: &Path) -> OwnKeys {
        let keys = Keys::choose(Some(home));
        assert!(keys.run.starts_with(TEST_ROOT) && keys.approved.starts_with(TEST_ROOT), "{keys:?}");
        OwnKeys(keys)
    }

    fn entry(&self) -> Option<String> {
        let (key, name) = (wide(&self.0.run), wide(VALUE_NAME));
        let mut buf = vec![0u16; 2048];
        let mut size = (buf.len() * 2) as u32;
        let status = unsafe { RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), RRF_RT_REG_SZ, null_mut(), buf.as_mut_ptr().cast(), &mut size) };
        (status == ERROR_SUCCESS).then(|| String::from_utf16_lossy(&buf[..size as usize / 2]).trim_end_matches('\0').to_string())
    }
}

impl Drop for OwnKeys {
    fn drop(&mut self) {
        let root = self.0.run.strip_suffix(r"\Run").expect("a key of its own");
        unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide(root).as_ptr()) };
    }
}

#[test]
fn the_executable_has_windows_start_the_tray_app_beside_it() {
    let world = world_with_binaries("cli-exe-startup");
    let keys = OwnKeys::of(&world.home);
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let dead = format!("http://127.0.0.1:{port}");

    expect(&exe(&world, &dead, &["startup"]), 0, &["Start with Windows: off"]);
    expect(&exe(&world, &dead, &["startup", "on"]), 0, &["Start with Windows: on"]);
    let entry = keys.entry().expect("the entry is in the key of this app folder");
    let tray = autorun::exe_at(&world.root.join("bin").join("tiny-skill-sync.exe"));
    assert!(tray.is(program(&entry)) && entry.ends_with("\" --background"), "{entry}");
    expect(&exe(&world, &dead, &["startup"]), 0, &["Start with Windows: on"]);
    expect(&exe(&world, &dead, &["startup", "off"]), 0, &["Start with Windows: off"]);
    assert_eq!(keys.entry(), None);
}

/// A program's version resource, as Windows reads it.
struct VersionInfo(Vec<u8>);

impl VersionInfo {
    fn of(exe: &str) -> Option<VersionInfo> {
        let path = wide(exe);
        let size = unsafe { GetFileVersionInfoSizeW(path.as_ptr(), null_mut()) };
        let mut data = vec![0u8; size as usize];
        (size > 0 && unsafe { GetFileVersionInfoW(path.as_ptr(), 0, size, data.as_mut_ptr().cast()) } != 0).then_some(VersionInfo(data))
    }

    /// The value at `path`, and its size: in bytes, or in characters for a string.
    fn query(&self, path: &str) -> Option<(*const c_void, usize)> {
        let (mut value, mut len) = (null_mut(), 0u32);
        let found = unsafe { VerQueryValueW(self.0.as_ptr().cast(), wide(path).as_ptr(), &mut value, &mut len) } != 0;
        (found && !value.is_null()).then_some((value.cast_const(), len as usize))
    }

    /// One of its strings, in the language its translation names, as Task Manager and Explorer look it up.
    fn text(&self, name: &str) -> Option<String> {
        let (value, len) = self.query(r"\VarFileInfo\Translation")?;
        assert!(len >= 4, "a translation is a language and a code page");
        let (language, page) = unsafe { (*value.cast::<u16>(), *value.cast::<u16>().add(1)) };
        let (value, len) = self.query(&format!(r"\StringFileInfo\{language:04x}{page:04x}\{name}"))?;
        let units = unsafe { std::slice::from_raw_parts(value.cast::<u16>(), len) };
        Some(String::from_utf16_lossy(units).trim_end_matches('\0').to_string())
    }

    /// Its file and product versions, in numbers.
    fn numbers(&self) -> Option<[u32; 4]> {
        let (value, len) = self.query(r"\")?;
        assert_eq!(len, size_of::<VS_FIXEDFILEINFO>());
        let fixed = unsafe { &*value.cast::<VS_FIXEDFILEINFO>() };
        assert_eq!(fixed.dwSignature, VS_FFI_SIGNATURE as u32);
        assert_eq!((fixed.dwProductVersionMS, fixed.dwProductVersionLS), (fixed.dwFileVersionMS, fixed.dwFileVersionLS));
        Some([fixed.dwFileVersionMS >> 16, fixed.dwFileVersionMS & 0xFFFF, fixed.dwFileVersionLS >> 16, fixed.dwFileVersionLS & 0xFFFF])
    }
}

#[test]
fn both_executables_tell_windows_their_name_and_version() {
    let number = |n: &str| n.parse::<u32>().unwrap();
    let numbers = [number(env!("CARGO_PKG_VERSION_MAJOR")), number(env!("CARGO_PKG_VERSION_MINOR")), number(env!("CARGO_PKG_VERSION_PATCH")), 0];
    for (exe, file, description) in [
        (env!("CARGO_BIN_EXE_tiny-skill-sync"), "tiny-skill-sync.exe", APP_NAME.to_string()),
        (env!("CARGO_BIN_EXE_tss"), "tss.exe", format!("{APP_NAME} command line")),
    ] {
        let info = VersionInfo::of(exe).unwrap_or_else(|| panic!("{exe} has no version resource"));
        assert_eq!(info.text("FileDescription"), Some(description), "the name Task Manager's startup apps show");
        assert_eq!(info.text("ProductName").as_deref(), Some(APP_NAME));
        assert_eq!(info.text("OriginalFilename").as_deref(), Some(file));
        assert_eq!(info.text("FileVersion").as_deref(), Some(env!("CARGO_PKG_VERSION")));
        assert_eq!(info.text("ProductVersion").as_deref(), Some(env!("CARGO_PKG_VERSION")));
        assert_eq!(info.numbers(), Some(numbers), "{file}");
    }
}

/// The DLLs an exe imports, lowercase, read from its PE import table.
fn imported_dlls(exe: &str) -> Vec<String> {
    let data = std::fs::read(exe).unwrap();
    let u16_at = |at: usize| u16::from_le_bytes([data[at], data[at + 1]]) as usize;
    let u32_at = |at: usize| u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]) as usize;
    let pe = u32_at(0x3C);
    assert_eq!(&data[pe..pe + 4], b"PE\0\0", "{exe} is a PE file");
    let optional = pe + 24;
    assert_eq!(u16_at(optional), 0x20B, "{exe} is a 64-bit exe");
    let sections = optional + u16_at(pe + 20);
    let file_offset = |rva: usize| {
        (0..u16_at(pe + 6))
            .map(|i| sections + 40 * i)
            .find_map(|s| (u32_at(s + 12)..u32_at(s + 12) + u32_at(s + 8)).contains(&rva).then(|| u32_at(s + 20) + rva - u32_at(s + 12)))
            .unwrap_or_else(|| panic!("{exe}: address {rva:#x} is in a section"))
    };
    // The import directory is the second data directory; each descriptor is 20 bytes, its DLL name at offset 12.
    let mut at = file_offset(u32_at(optional + 120));
    let mut dlls = Vec::new();
    while u32_at(at + 12) != 0 {
        let name = file_offset(u32_at(at + 12));
        let end = name + data[name..].iter().position(|&b| b == 0).unwrap();
        dlls.push(String::from_utf8_lossy(&data[name..end]).to_ascii_lowercase());
        at += 20;
    }
    dlls
}

#[test]
fn both_executables_use_the_c_runtime_windows_includes() {
    for exe in [env!("CARGO_BIN_EXE_tiny-skill-sync"), env!("CARGO_BIN_EXE_tss")] {
        let dlls = imported_dlls(exe);
        assert!(dlls.iter().any(|dll| dll.starts_with("api-ms-win-crt-")), "{exe} links the Universal C Runtime from Windows: {dlls:?}");
        let redistributable = ["vcruntime", "msvcp", "ucrtbased", "concrt", "vccorlib"];
        assert!(!dlls.iter().any(|dll| redistributable.iter().any(|r| dll.starts_with(r))), "{exe} needs no Visual C++ redistributable: {dlls:?}");
    }
}

/// The pixels of an exe's icon at `size` as Windows decodes it from the exe's resources, as straight-alpha
/// `0xAARRGGBB`, or `None` when it has no icon.
fn exe_icon(exe: &str, size: i32) -> Option<Vec<u32>> {
    use windows_sys::Win32::Foundation::FreeLibrary;
    use windows_sys::Win32::Graphics::Gdi::{BI_RGB, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits, ReleaseDC};
    use windows_sys::Win32::System::LibraryLoader::{LOAD_LIBRARY_AS_DATAFILE, LOAD_LIBRARY_AS_IMAGE_RESOURCE, LoadLibraryExW};
    use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, ICONINFO, IMAGE_ICON, LR_DEFAULTCOLOR, LoadImageW};
    unsafe {
        let module = LoadLibraryExW(wide(exe).as_ptr(), null_mut(), LOAD_LIBRARY_AS_DATAFILE | LOAD_LIBRARY_AS_IMAGE_RESOURCE);
        assert!(!module.is_null(), "{exe} can be opened");
        // The first icon group, which Explorer shows.
        let icon = LoadImageW(module, 1 as _, IMAGE_ICON, size, size, LR_DEFAULTCOLOR);
        FreeLibrary(module);
        if icon.is_null() {
            return None;
        }
        let mut info: ICONINFO = std::mem::zeroed();
        assert_ne!(GetIconInfo(icon, &mut info), 0);
        let mut bmi: BITMAPINFO = std::mem::zeroed();
        bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = size;
        bmi.bmiHeader.biHeight = -size;
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB;
        let mut pixels = vec![0u32; (size * size) as usize];
        let dc = GetDC(null_mut());
        let rows = GetDIBits(dc, info.hbmColor, 0, size as u32, pixels.as_mut_ptr().cast(), &mut bmi, DIB_RGB_COLORS);
        ReleaseDC(null_mut(), dc);
        DeleteObject(info.hbmColor);
        DeleteObject(info.hbmMask);
        DestroyIcon(icon);
        assert_eq!(rows, size, "the icon's pixels can be read");
        Some(pixels)
    }
}

#[test]
fn both_executables_show_the_logo_as_their_icon() {
    for exe in [env!("CARGO_BIN_EXE_tiny-skill-sync"), env!("CARGO_BIN_EXE_tss")] {
        for size in [16, 20, 24, 32, 48, 256] {
            let got = exe_icon(exe, size).unwrap_or_else(|| panic!("{exe} has no icon"));
            let want = logo::draw(logo::Look::Solid(logo::BLUE), size as u32);
            for (i, (g, w)) in got.iter().zip(&want).enumerate() {
                // Windows decodes the PNG in the exe; allow it a rounding step.
                let close = (0..4).all(|k| ((g >> (8 * k)) & 0xFF).abs_diff((w >> (8 * k)) & 0xFF) <= 2);
                assert!(close || (g >> 24 == 0 && w >> 24 == 0), "{exe} at {size} px, pixel {i}: {g:08x} instead of {w:08x}");
            }
        }
    }
}
