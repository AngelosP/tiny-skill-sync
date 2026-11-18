//! The `tss` commands, run in-process against the fake GitHub and a fake credential store, and the real executable
//! run against a GitHub that does not answer. Every test works in its own temporary folders.

mod common;

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
use tiny_skill_sync::config::{CONFIG_FILE, Config};
use tiny_skill_sync::github::{FailureKind, Remote, RemoteError, TokenProvider};
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
}

impl Host for TestHost {
    fn home(&self) -> PathBuf {
        self.world.home.clone()
    }

    fn skills_dir(&self, _cfg: &Config) -> PathBuf {
        self.world.skills.clone()
    }

    fn remote(&self) -> Result<Box<dyn Remote>, String> {
        Ok(self.remote.client())
    }

    fn tokens(&self) -> Box<dyn TokenProvider> {
        self.tokens.provider()
    }

    fn token_saved(&self) -> bool {
        self.saved.lock().unwrap().is_some()
    }

    fn save_token(&self, token: &str) -> Result<(), String> {
        *self.saved.lock().unwrap() = Some(token.to_string());
        Ok(())
    }

    fn delete_token(&self) -> Result<bool, String> {
        Ok(self.saved.lock().unwrap().take().is_some())
    }

    fn gh_signed_in(&self) -> Result<(), String> {
        self.gh.clone()
    }

    fn read_token(&self) -> Result<String, String> {
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
        TestHost {
            world: World::new(name),
            remote: FakeRemote::new(),
            tokens: FakeTokens::none(),
            saved: Mutex::new(None),
            gh: Err("the GitHub CLI is not signed in".to_string()),
            typed: Mutex::new(None),
            startup: Mutex::new(Ok(Startup::Off)),
        }
    }

    fn save(&self, cfg: &Config) {
        cfg.save(&self.config_file()).unwrap();
    }

    fn config_file(&self) -> PathBuf {
        self.world.home.join(CONFIG_FILE)
    }

    fn config(&self) -> Config {
        Config::load(&self.config_file()).unwrap()
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
    cfg.repos.push(repo("other", "o/other", "skills"));
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
    cfg.repos[0].enabled = false;
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
    let host = TestHost::empty("cli-add");
    expect(&tss(&host, "status"), 0, &["No repositories are set up. Add one with: tss add OWNER/NAME"]);
    expect(&tss(&host, "check"), 0, &["No repositories are set up"]);

    let ran = tss(&host, "add o/fixtures --every 6h");
    expect(&ran, 0, &["Added o/fixtures as 'o-fixtures'. Checked every 6 hours.", "tss check o-fixtures"]);
    let repos = host.config().repos;
    assert_eq!((repos.len(), repos[0].interval.map(|i| i.to_string())), (1, Some("6h".to_string())));

    expect(&tss(&host, "add O/Fixtures"), 3, &["already set up, as 'o-fixtures'"]);
    expect(&tss(&host, "add not-a-repo"), 3, &[]);
    expect(&tss(&host, "add https://github.com/o/fixtures/tree/main/skills --use-token"), 0, &["as 'o-fixtures-2'", "sends the GitHub token"]);
    assert_eq!(host.config().repos.len(), 2);
    expect(&tss(&host, "check o/fixtures"), 3, &["matches several repositories (o-fixtures, o-fixtures-2); use the id"]);
    expect(&tss(&host, "check nope"), 3, &["no repository 'nope' is set up; the ids are: o-fixtures, o-fixtures-2"]);
}

#[test]
fn a_repository_whose_skills_are_in_a_subfolder_is_not_reported_as_fine() {
    let host = TestHost::empty("cli-subfolder");
    host.remote.seed_standard("o/fixtures");
    expect(&tss(&host, "add o/fixtures"), 0, &[]);

    // Added as a whole, it has no skills at the top: the check must say so, and say what to do about it.
    let ran = tss(&host, "check");
    expect(&ran, 1, &["[o-fixtures]: No skills found", "No skills were found at the top of o/fixtures", "https://github.com/o/fixtures/tree/main/skills"]);
    assert!(ran.out.is_ascii(), "{ran:?}");
    assert!(host.world.skill_names().is_empty());
    expect(&tss(&host, "status"), 1, &["[o-fixtures]: No skills found", "No skills from it are installed."]);
    // A check that GitHub answers with "not modified" still knows what the listing had in it.
    expect(&tss(&host, "check"), 1, &["[o-fixtures]: No skills found"]);

    // Doing what it says works.
    expect(&tss(&host, "remove o-fixtures"), 0, &[]);
    expect(&tss(&host, "add https://github.com/o/fixtures/tree/main/skills"), 0, &[]);
    expect(&tss(&host, "check"), 0, &["Installed alpha"]);
    assert_eq!(host.world.skill_names(), ["alpha", "beta", "gamma"]);
}

#[test]
fn a_repository_whose_only_skill_is_held_is_not_reported_as_empty() {
    let host = TestHost::empty("cli-held-only");
    host.remote.put("o/one", "skills/mine/SKILL.md", "upstream\n");
    host.world.write("mine/SKILL.md", "my own\n");
    expect(&tss(&host, "add https://github.com/o/one/tree/main/skills"), 0, &[]);

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
    host.remote.put("o/one", "skills/a\u{1b}b/SKILL.md", "x\n");
    expect(&tss(&host, "add https://github.com/o/one/tree/main/skills"), 0, &[]);

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
    assert!(host.config().repos.is_empty());
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
    expect(&tss(&host, "status"), 0, &["Skills recorded for 'fx', which is no longer in the settings, are left alone.", "tss remove fx"]);
    expect(&tss(&host, "remove fx"), 0, &["Stopped tracking the skills recorded for 'fx'.", "now yours: alpha, beta, gamma"]);
    let ran = tss(&host, "status");
    assert!(!ran.out.contains("'fx'"), "{ran:?}");
    assert_eq!(host.world.skill_names(), ["alpha", "beta", "gamma"]);
    expect(&tss(&host, "remove fx"), 3, &["no repository 'fx' is set up"]);
}

#[test]
fn the_token_is_saved_shown_and_cleared() {
    let host = TestHost::new("cli-token");
    expect(&tss(&host, "token"), 0, &["Saved token: none.", "GitHub CLI: the GitHub CLI is not signed in.", "No repository sends the token."]);
    let mut cfg = standard_config();
    cfg.repos[0].use_token = true;
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
    assert!(!host.config().repos[0].use_token, "the settings are not changed");
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
    expect(&tss(&host, "resolve nope alpha keep"), 3, &["no repository 'nope'"]);
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

// ---------------------------------------------------------------------------------------------------------------------
// The executable itself
// ---------------------------------------------------------------------------------------------------------------------

/// `tss.exe` with its own home and skills folder, talking to a loopback port nothing listens on.
fn exe(world: &World, dead: &str, args: &[&str]) -> Ran {
    let output = Command::new(env!("CARGO_BIN_EXE_tss"))
        .args(args)
        .env("TSS_HOME", &world.home)
        .env("TSS_SKILLS_DIR", &world.skills)
        .env("TSS_API_BASE", dead)
        .env("TSS_RAW_BASE", dead)
        .output()
        .unwrap();
    Ran { code: output.status.code().unwrap(), out: String::from_utf8(output.stdout).unwrap(), err: String::from_utf8(output.stderr).unwrap() }
}

#[test]
fn the_executable_runs_against_its_own_folders() {
    let world = World::new("cli-exe");
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let dead = format!("http://127.0.0.1:{port}");

    let ran = exe(&world, &dead, &["--version"]);
    expect(&ran, 0, &[&format!("tss {}", env!("CARGO_PKG_VERSION"))]);
    expect(&exe(&world, &dead, &["frobnicate"]), 3, &["unknown command"]);
    expect(&exe(&world, &dead, &["add", "o/fixtures"]), 0, &["Added o/fixtures as 'o-fixtures'."]);
    assert!(world.home.join(CONFIG_FILE).is_file());

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
    let world = World::new("cli-exe-startup");
    let keys = OwnKeys::of(&world.home);
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let dead = format!("http://127.0.0.1:{port}");

    expect(&exe(&world, &dead, &["startup"]), 0, &["Start with Windows: off"]);
    expect(&exe(&world, &dead, &["startup", "on"]), 0, &["Start with Windows: on"]);
    let entry = keys.entry().expect("the entry is in the key of this app folder");
    let tray = autorun::exe_at(Path::new(env!("CARGO_BIN_EXE_tiny-skill-sync")));
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
