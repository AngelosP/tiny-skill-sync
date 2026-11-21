//! `tss`, the command line. It drives the same engine as the tray, one command at a time, so every rule about what
//! may be changed or deleted is the same. It is a way to look and to act once (and a surface for tests and
//! measurements), not a second package manager: there is no search, no install by name and no version pinning.
//!
//! Exit codes: 0 everything is fine, 1 something needs the user's attention, 2 a check or command failed,
//! 3 the command line was not understood.

use std::borrow::Cow;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use crate::app::same_location;
use crate::autostart::{Exe, Keys, Startup, Via};
use crate::board::{
    Action, Board, Level, RepoView, Row, SourceView, capitalized, preview_lines, previewed_plugins, recorded_plugin_rows, source_folder, source_label,
};
use crate::config::{CONFIG_FILE, Config, Paths, RepoConfig, UpdateInterval, UpdatePrefs, UpdateTiming};
use crate::engine::{
    Access, ApplyResult, Engine, EngineError, LocationPreview, Outcome, RemoveReport, RepoReport, Resolution, SkillReport, SyncOptions, Tracking, Which,
};
use crate::github::{GitHubRemote, Remote, SystemTokens, TokenProvider};
use crate::interval::{Interval, now_secs};
use crate::location::{Location, Target};
use crate::pathsafe::{check_component, fold, printable, printable_lines};
use crate::plugins::{Choice, normal_name};
use crate::portable;
use crate::reconcile::Verdict;
use crate::source::SourceKind;
use crate::sources::{GlobalSettings, SourceConfig};
use crate::state::State;
use crate::updater::Action as AppAction;
use crate::verify::Check;
use crate::win::{autorun, console, cred, proc};

pub const EXIT_OK: i32 = 0;
pub const EXIT_ATTENTION: i32 = 1;
pub const EXIT_FAILED: i32 = 2;
pub const EXIT_USAGE: i32 = 3;

/// The tray app's file name, which `tss startup` looks for beside `tss.exe`.
const TRAY_EXE: &str = "tiny-skill-sync.exe";

const NO_REPOS: &str = "No repositories are set up. Add one with: tss add OWNER/NAME";

const KINDS: [SourceKind; 4] = [SourceKind::Skills, SourceKind::Agents, SourceKind::Instructions, SourceKind::Plugins];

const HELP: &str = "\
tss - syncs Copilot skills, agents, instructions and CLI plugins with GitHub.
The tray app works on a schedule. Both follow the same safety rules.

Usage:
  tss status
      What is installed and what needs attention. Reads only local files.
  tss check [REPO] [--dry-run] [--full-listing] [--delete-all]
      Check sources, not the app. 'sync' is another name for 'check'.
      Apply what is safe: install, update, remove.
        --dry-run       show what would change, and change nothing
        --full-listing  fetch the whole listing even if nothing changed
        --delete-all    confirm removal if REPO now lists no matches
  tss update [status | check | install | on | off]
  tss update every 1d|7d|30d
  tss update when immediate|quit
      Update Tiny Skill Sync itself, not skills. Status reads only local files.
      'check' looks for a release without installing it. 'install' installs now.
      Both work with automatic app updates off. 'every' takes fixed days.
      Immediate (the default): the tray installs an update when it finds one,
      then restarts into it. 'quit' waits until you choose Quit from the tray
      menu, and the tray stays closed. 'tss update install' does not restart a
      running tray: quit it and open it again to use the new build.
  tss resolve REPO ITEM ACTION
      Settle a skill or a file from this source. ACTION is one of:
        replace   take upstream's version; what is there now is kept as a backup
        restore   bring back an item deleted here (same as replace)
        keep      the item is yours: never change or remove it
        leave     an item deleted here stays deleted (same as keep)
        manage    undo keep
  tss resolve REPO PLUGIN approve [PUBLISHER]
  tss resolve REPO PLUGIN reinstall|uninstall|forget
      Approve installs this plugin and trusts new versions from its branch.
      Give the publisher exactly as shown; for approval,
      omit it only when the publisher is not declared.
      Reinstall restores a plugin removed in Copilot. When GitHub removes a
      plugin, uninstall removes it in Copilot; forget just stops tracking it.
  tss add LOCATION [--preview] [--approve-plugins] [--every INTERVAL]
                   [--use-token] [--yes]
      LOCATION is OWNER/NAME, https://github.com/OWNER/NAME, or the address of
      a branch or folder: https://github.com/OWNER/NAME/tree/BRANCH/FOLDER.
      A bare repository follows its default branch. The skills, agents,
      instructions and plugins in it are found by themselves; there is no
      --type. Adding shows what was found, then asks you to type yes.
        --preview          only show what was found; save and install nothing
        --yes              save without asking (needed when input is a pipe)
        --approve-plugins  also approve the plugins shown, so they install
      Plugins can run programs on this PC: without --approve-plugins they
      wait for your approval. Check now with 'tss check ID' to install, or
      let the tray check later.
      Private repositories use the saved token or the GitHub CLI sign-in when
      needed. --use-token sends the token for every request.
  tss source REPO
  tss source REPO types skills=on|off|default [agents=...] [...]
  tss source REPO ignore add|allow|remove FOLDER
  tss source REPO repo-items on|off|default
      Show or change one source. 'default' follows 'tss settings'. Ignored
      folder names are skipped wherever they appear; 'allow' takes a name off
      the list for this source only. repo-items syncs skills and agents from
      repository-specific folders such as .github. Turning a type off never
      deletes anything. Changes apply on the next check.
  tss settings
  tss settings types skills=on|off [agents=on|off] [...]
  tss settings ignore add|remove FOLDER
  tss settings repo-items on|off
      Show or change the defaults every source follows.
  tss remove REPO [--delete-skills] [--uninstall-plugins]
      Stop following a source, with all of its types. Its files and plugins
      stay installed by default. --delete-skills deletes the skills it
      installed (edited skills stay; agents and instruction files stay).
      --uninstall-plugins uninstalls its plugins. If an uninstall fails, the
      source is kept.
  tss token [status | set | clear]
      The GitHub token, saved in Windows Credential Manager. Without one, the
      GitHub CLI's token is used (gh auth login). 'set' reads the token without
      showing it, or from a pipe: gh auth token | tss token set
  tss startup [status | on | off]
      Whether Windows starts the tray app (tiny-skill-sync.exe, in the same
      folder) when you sign in. It starts without its window and waits a
      little before it looks at your skills or checks GitHub, so it does not
      slow sign-in down.
  tss portable
      Export local settings and repositories to companion files beside tss.exe.
  tss help
  tss --version

REPO is a source ID shown by 'tss status' (or one of its type IDs), or
OWNER/NAME when only one source has it.
INTERVAL is a number and a unit: 30m, 6h, 1d, 2w or 1mo (at least 5 minutes).

Exit codes: 0 all fine, 1 needs attention, 2 failed, 3 bad command line.

Environment:
  TSS_HOME        settings and state (default: %LOCALAPPDATA%\\tiny-skill-sync)
  TSS_SKILLS_DIR  the skills folder (default: from the settings, else
                  %USERPROFILE%\\.copilot\\skills)
  TSS_AGENTS_DIR  agents folder (normally %USERPROFILE%\\.copilot\\agents)
  TSS_INSTRUCTIONS_DIR  instructions folder (normally
                  %USERPROFILE%\\.copilot\\instructions)
                  Set these to isolated folders when testing file sources.
  TSS_COPILOT_CLI full path to copilot.exe (optional override)
";

/// Everything the commands need from outside, so tests can run them against a fake GitHub and a fake credential
/// store.
pub trait Host {
    /// Where the settings and the state are kept.
    fn home(&self) -> PathBuf;
    /// The folder containing this copy's executable and portable companions.
    fn executable_dir(&self) -> Result<PathBuf, String>;
    fn skills_dir(&self, cfg: &Config) -> PathBuf;
    fn portable_skills_dir(&self, cfg: &Config) -> Result<PathBuf, String> {
        Ok(self.skills_dir(cfg))
    }
    fn source_paths(&self, skills: PathBuf) -> Paths {
        Paths::new(self.home(), skills)
    }
    /// Reads the version of the installed tray and CLI pair, not the build running this command.
    fn installed_app_version(&self, directory: &Path) -> Result<String, String>;
    /// Explicit app release operation, separate from the skills engine and its credential sources.
    fn manual_app_update(&self, paths: &Paths, directory: &Path, action: AppAction) -> Result<String, String>;
    fn remote(&self) -> Result<Box<dyn Remote>, String>;
    fn tokens(&self) -> Box<dyn TokenProvider>;
    fn plugin_manager(&self) -> Option<Box<dyn crate::plugins::native::PluginManager>> {
        None
    }
    /// Called only after an add displayed its preview; EOF or anything but an explicit yes cancels.
    fn confirm_source(&self) -> Result<bool, String>;
    /// Whether someone can answer a question: input comes from a console, not a pipe or a file.
    fn interactive(&self) -> bool {
        false
    }
    fn token_saved(&self) -> bool;
    fn save_token(&self, token: &str) -> Result<(), String>;
    /// True when a token was saved and is now gone.
    fn delete_token(&self) -> Result<bool, String>;
    /// Whether the GitHub CLI would hand out a token, and if not, why.
    fn gh_signed_in(&self) -> Result<(), String>;
    /// A token typed at the console (not shown) or piped in.
    fn read_token(&self) -> Result<String, String>;
    /// Whether Windows starts the tray app when the user signs in. Only reads.
    fn startup(&self) -> Result<Startup, String>;
    /// Has Windows start the tray app when the user signs in, or stop starting it. Another copy's entry is only
    /// replaced by turning this one on.
    fn set_startup(&self, on: bool) -> Result<Startup, String>;
}

/// The real computer.
pub struct SystemHost;

impl Host for SystemHost {
    fn home(&self) -> PathBuf {
        Paths::home_dir()
    }

    fn executable_dir(&self) -> Result<PathBuf, String> {
        std::env::current_exe()
            .map_err(|error| format!("the folder this executable is in cannot be found: {error}"))?
            .parent()
            .map(PathBuf::from)
            .ok_or_else(|| "the folder this executable is in cannot be found".to_string())
    }

    fn skills_dir(&self, cfg: &Config) -> PathBuf {
        Paths::skills_dir(cfg)
    }

    fn portable_skills_dir(&self, cfg: &Config) -> Result<PathBuf, String> {
        let profile = std::env::var_os("USERPROFILE").map(PathBuf::from);
        portable::resolve_skills_dir(cfg, std::env::var_os("TSS_SKILLS_DIR").as_deref(), profile.as_deref(), |name| {
            std::env::var_os(name).map(|value| value.to_string_lossy().into_owned())
        })
    }

    fn installed_app_version(&self, directory: &Path) -> Result<String, String> {
        crate::replacement::installed_version(directory).map(|version| version.to_string())
    }

    fn manual_app_update(&self, paths: &Paths, directory: &Path, action: AppAction) -> Result<String, String> {
        crate::updater::manual(&paths.home, directory, &paths.skills, action)
    }

    fn remote(&self) -> Result<Box<dyn Remote>, String> {
        Ok(Box::new(GitHubRemote::from_env()?))
    }

    fn tokens(&self) -> Box<dyn TokenProvider> {
        Box::new(SystemTokens)
    }

    fn confirm_source(&self) -> Result<bool, String> {
        let mut line = String::new();
        io::stdin().read_line(&mut line).map_err(|e| format!("could not read the answer: {e}"))?;
        Ok(line.trim().eq_ignore_ascii_case("yes"))
    }

    fn interactive(&self) -> bool {
        io::stdin().is_terminal()
    }

    fn token_saved(&self) -> bool {
        cred::has_token()
    }

    fn save_token(&self, token: &str) -> Result<(), String> {
        cred::write_token(token).map_err(|e| format!("the token was not saved: {e}"))
    }

    fn delete_token(&self) -> Result<bool, String> {
        cred::delete_token().map_err(|e| format!("the saved token could not be removed: {e}"))
    }

    fn gh_signed_in(&self) -> Result<(), String> {
        proc::gh_token().map(|_| ())
    }

    fn read_token(&self) -> Result<String, String> {
        if io::stdin().is_terminal() {
            eprint!("GitHub token (it is not shown): ");
        }
        let line = console::read_hidden_line().map_err(|e| format!("the token could not be read: {e}"))?;
        let token = line.trim();
        if token.is_empty() {
            return Err("no token was given".to_string());
        }
        Ok(token.to_string())
    }

    fn startup(&self) -> Result<Startup, String> {
        autorun::state(&Keys::current(), &tray_app()?).map_err(|e| e.to_string())
    }

    fn set_startup(&self, on: bool) -> Result<Startup, String> {
        let (keys, tray) = (Keys::current(), tray_app()?);
        let result = if on { autorun::turn_on(&keys, &tray) } else { autorun::turn_off(&keys, &tray) };
        result.map_err(|e| e.to_string())
    }
}

/// The tray app in the folder `tss.exe` is in. It is what Windows starts; `tss` itself never is.
fn tray_app() -> Result<Exe, String> {
    let tss = std::env::current_exe().map_err(|e| format!("the folder tss.exe is in cannot be found: {e}"))?;
    let tray = tss.with_file_name(TRAY_EXE);
    if !tray.is_file() {
        return Err(format!("the tray app, {TRAY_EXE}, is not in the same folder as tss.exe"));
    }
    Ok(autorun::exe_at(&tray))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Help,
    Version,
    Status,
    Update(UpdateCommand),
    Check {
        repo: Option<String>,
        dry_run: bool,
        full_listing: bool,
        delete_all: bool,
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
    Add(AddSource),
    Remove {
        repo: String,
        delete_skills: bool,
        uninstall_plugins: bool,
    },
    /// Show one source's settings, or change one of them.
    Source {
        repo: String,
        change: Option<SourceChange>,
    },
    /// Show the defaults every source follows, or change one of them.
    Settings(Option<SettingsChange>),
    Token(TokenAction),
    Startup(StartupAction),
    Portable,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AddSource {
    pub spec: String,
    pub every: Option<Interval>,
    pub use_token: bool,
    pub preview: bool,
    pub approve_plugins: bool,
    /// Save without asking.
    pub yes: bool,
}

pub use crate::sources::IgnoreAction;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceChange {
    /// `None` follows the global setting again.
    Types(Vec<(SourceKind, Option<bool>)>),
    Ignore(IgnoreAction, String),
    RepoItems(Option<bool>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettingsChange {
    Types(Vec<(SourceKind, bool)>),
    /// True adds the folder name, false removes it.
    Ignore(bool, String),
    RepoItems(bool),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenAction {
    Status,
    Set,
    Clear,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupAction {
    Status,
    On,
    Off,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateCommand {
    Status,
    Manual(AppAction),
    Automatic(bool),
    Every(UpdateInterval),
    When(UpdateTiming),
}

/// Options as given: `--name` or `--name=value`.
#[derive(Default)]
struct Flags(Vec<(String, Option<String>)>);

impl Flags {
    fn remove(&mut self, name: &str) -> Vec<Option<String>> {
        let (hits, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.0).into_iter().partition(|(n, _)| n == name);
        self.0 = rest;
        hits.into_iter().map(|(_, value)| value).collect()
    }

    fn switch(&mut self, name: &str) -> Result<bool, String> {
        let hits = self.remove(name);
        if hits.iter().any(Option::is_some) {
            return Err(format!("--{name} takes no value"));
        }
        Ok(!hits.is_empty())
    }

    fn value(&mut self, name: &str) -> Option<String> {
        self.remove(name).into_iter().flatten().last()
    }

    fn finish(self, command: &str) -> Result<(), String> {
        match self.0.first() {
            Some((name, _)) => Err(format!("'{command}' has no option --{name}")),
            None => Ok(()),
        }
    }
}

fn usage_of(command: &str) -> &'static str {
    match command {
        "check" => "tss check [REPO] [--dry-run] [--full-listing] [--delete-all]",
        "update" => "tss update [status|check|install|on|off|every 1d|7d|30d|when immediate|quit]",
        "resolve" => "tss resolve REPO ITEM ACTION (see 'tss help' for choices)",
        "add" => "tss add LOCATION [--preview] [--approve-plugins] [--every INTERVAL] [--use-token] [--yes]",
        "remove" => "tss remove REPO [--delete-skills] [--uninstall-plugins]",
        "source" => "tss source REPO [types KIND=on|off|default ... | ignore add|allow|remove FOLDER | repo-items on|off|default]",
        "settings" => "tss settings [types KIND=on|off ... | ignore add|remove FOLDER | repo-items on|off]",
        "token" => "tss token [status|set|clear]",
        "startup" => "tss startup [status|on|off]",
        "portable" => "tss portable",
        _ => "tss status",
    }
}

fn expect_words(command: &str, words: &[String], min: usize, max: usize) -> Result<(), String> {
    if words.len() < min {
        return Err(format!("something is missing; use: {}", usage_of(command)));
    }
    match words.get(max) {
        Some(extra) => Err(format!("'{extra}' was not expected; use: {}", usage_of(command))),
        None => Ok(()),
    }
}

fn resolution(word: &str) -> Result<Resolution, String> {
    match word.to_ascii_lowercase().as_str() {
        "replace" | "restore" => Ok(Resolution::Overwrite),
        "keep" | "leave" => Ok(Resolution::KeepMine),
        "manage" => Ok(Resolution::ManageAgain),
        "pause" | "cancel" => Ok(Resolution::PauseHistory),
        "resume" => Ok(Resolution::ResumeHistory),
        _ => Err(format!("'{word}' is not something resolve can do; use replace, restore, keep, leave, manage, pause or resume")),
    }
}

/// The word that asks `resolve` for an action. `RemoveThem` is a check, not a resolution.
fn word_for(action: Action) -> &'static str {
    match action {
        Action::Overwrite => "replace",
        Action::Restore => "restore",
        Action::KeepMine => "keep",
        Action::LeaveRemoved => "leave",
        Action::ManageAgain => "manage",
        Action::PauseHistory => "pause",
        Action::ResumeHistory => "resume",
        Action::ApprovePlugin => "approve",
        Action::ReinstallPlugin => "reinstall",
        Action::UninstallPlugin => "uninstall",
        Action::ForgetPlugin => "forget",
        Action::RemoveThem => "",
    }
}

/// `skills=on` into the type and the value as typed.
fn type_word(word: &str) -> Result<(SourceKind, &str), String> {
    let (kind, value) = word.split_once('=').ok_or_else(|| format!("'{word}' is not KIND=VALUE, for example skills=on"))?;
    Ok((SourceKind::try_from(kind.to_ascii_lowercase())?, value))
}

/// on, off, or (where allowed) default, which is `None`.
fn switch_word(word: &str, default_allowed: bool) -> Result<Option<bool>, String> {
    match word.to_ascii_lowercase().as_str() {
        "on" => Ok(Some(true)),
        "off" => Ok(Some(false)),
        "default" if default_allowed => Ok(None),
        _ if default_allowed => Err(format!("'{word}' is not on, off or default")),
        _ => Err(format!("'{word}' is not on or off")),
    }
}

pub fn parse(args: &[String]) -> Result<Command, String> {
    let mut words = Vec::new();
    let mut flags = Flags::default();
    let mut rest = args.iter();
    let mut only_words = false;
    while let Some(arg) = rest.next() {
        if only_words || !arg.starts_with('-') || arg == "-" {
            words.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => only_words = true,
            "-h" | "--help" => return Ok(Command::Help),
            "-V" | "--version" => return Ok(Command::Version),
            _ => {
                let Some(body) = arg.strip_prefix("--") else { return Err(format!("unknown option '{arg}'")) };
                let (name, value) = match body.split_once('=') {
                    Some((name, value)) => (name, Some(value.to_string())),
                    None => (body, None),
                };
                let value = match value {
                    None if name == "every" => Some(rest.next().cloned().ok_or("--every needs an interval, for example --every 6h")?),
                    None if name == "type" => rest.next().cloned(),
                    value => value,
                };
                flags.0.push((name.to_string(), value));
            }
        }
    }
    let Some(first) = words.first() else { return Err("no command was given".to_string()) };
    let name = first.to_ascii_lowercase();
    let words = &words[1..];
    let command = match name.as_str() {
        "help" => return Ok(Command::Help),
        "version" => {
            expect_words(&name, words, 0, 0)?;
            Command::Version
        }
        "status" => {
            expect_words(&name, words, 0, 0)?;
            Command::Status
        }
        "update" => {
            expect_words(&name, words, 0, 2)?;
            let word = |n: usize| words.get(n).map(|w| w.to_ascii_lowercase());
            let update = match (word(0).as_deref(), word(1).as_deref()) {
                (None | Some("status"), None) => UpdateCommand::Status,
                (Some("check"), None) => UpdateCommand::Manual(AppAction::Check),
                (Some("install"), None) => UpdateCommand::Manual(AppAction::Install),
                (Some("on"), None) => UpdateCommand::Automatic(true),
                (Some("off"), None) => UpdateCommand::Automatic(false),
                (Some("every"), Some("1d")) => UpdateCommand::Every(UpdateInterval::Daily),
                (Some("every"), Some("7d")) => UpdateCommand::Every(UpdateInterval::Weekly),
                (Some("every"), Some("30d")) => UpdateCommand::Every(UpdateInterval::Monthly),
                (Some("when"), Some("immediate")) => UpdateCommand::When(UpdateTiming::Immediate),
                (Some("when"), Some("quit")) => UpdateCommand::When(UpdateTiming::OnQuit),
                _ => return Err(format!("use: {}", usage_of("update"))),
            };
            Command::Update(update)
        }
        "check" | "sync" => {
            expect_words(&name, words, 0, 1)?;
            let repo = words.first().cloned();
            let (dry_run, full_listing, delete_all) = (flags.switch("dry-run")?, flags.switch("full-listing")?, flags.switch("delete-all")?);
            if delete_all && repo.is_none() {
                return Err("--delete-all is for one repository: tss check REPO --delete-all".to_string());
            }
            Command::Check { repo, dry_run, full_listing, delete_all }
        }
        "resolve" => {
            expect_words(&name, words, 3, 4)?;
            let action = words[2].to_ascii_lowercase();
            let repo = words[0].clone();
            let item = words[1].clone();
            if action != "approve" {
                expect_words(&name, words, 3, 3)?;
            }
            let choice = match action.as_str() {
                "approve" => Some(Choice::Approve { name: item.clone(), publisher: words.get(3).cloned() }),
                "reinstall" => Some(Choice::Reinstall(item.clone())),
                "uninstall" => Some(Choice::Uninstall(item.clone())),
                "forget" => Some(Choice::Forget(item.clone())),
                _ => None,
            };
            if let Some(choice) = choice {
                Command::ResolvePlugin { repo, choice }
            } else {
                Command::Resolve { repo, skill: item, how: resolution(&words[2])? }
            }
        }
        "add" => {
            expect_words(&name, words, 1, 1)?;
            if !flags.remove("type").is_empty() {
                return Err("tss add has no --type: the types in a source are found by themselves. \
                            Switch one off or on with: tss source REPO types skills=on|off|default"
                    .into());
            }
            let every = match flags.value("every") {
                Some(text) => Some(Interval::parse(&text).and_then(Interval::validate_poll).map_err(|e| format!("--every: {e}"))?),
                None => None,
            };
            Command::Add(AddSource {
                spec: words[0].clone(),
                every,
                use_token: flags.switch("use-token")?,
                preview: flags.switch("preview")?,
                approve_plugins: flags.switch("approve-plugins")?,
                yes: flags.switch("yes")?,
            })
        }
        "remove" => {
            expect_words(&name, words, 1, 1)?;
            let (delete_skills, uninstall_plugins) = (flags.switch("delete-skills")?, flags.switch("uninstall-plugins")?);
            Command::Remove { repo: words[0].clone(), delete_skills, uninstall_plugins }
        }
        "source" => {
            expect_words(&name, words, 1, 6)?;
            let repo = words[0].clone();
            let verb = words.get(1).map(|w| w.to_ascii_lowercase());
            let change = match verb.as_deref() {
                None => None,
                Some("types") => {
                    let mut changes = Vec::new();
                    for word in &words[2..] {
                        let (kind, value) = type_word(word)?;
                        changes.push((kind, switch_word(value, true).map_err(|e| format!("{word}: {e}"))?));
                    }
                    if changes.is_empty() {
                        return Err(format!("something is missing; use: {}", usage_of("source")));
                    }
                    Some(SourceChange::Types(changes))
                }
                Some("ignore") => {
                    expect_words(&name, words, 4, 4)?;
                    let action = match words[2].to_ascii_lowercase().as_str() {
                        "add" => IgnoreAction::Add,
                        "allow" => IgnoreAction::Allow,
                        "remove" => IgnoreAction::Remove,
                        _ => return Err(format!("'{}' is not an ignore action; use add, allow or remove", words[2])),
                    };
                    Some(SourceChange::Ignore(action, words[3].clone()))
                }
                Some("repo-items") => {
                    expect_words(&name, words, 3, 3)?;
                    Some(SourceChange::RepoItems(switch_word(&words[2], true)?))
                }
                Some(_) => return Err(format!("'{}' is not a source setting; use: {}", words[1], usage_of("source"))),
            };
            Command::Source { repo, change }
        }
        "settings" => {
            expect_words(&name, words, 0, 5)?;
            let verb = words.first().map(|w| w.to_ascii_lowercase());
            let change = match verb.as_deref() {
                None => None,
                Some("types") => {
                    let mut changes = Vec::new();
                    for word in &words[1..] {
                        let (kind, value) = type_word(word)?;
                        let on = switch_word(value, false).map_err(|e| format!("{word}: {e}"))?.unwrap_or(true);
                        changes.push((kind, on));
                    }
                    if changes.is_empty() {
                        return Err(format!("something is missing; use: {}", usage_of("settings")));
                    }
                    Some(SettingsChange::Types(changes))
                }
                Some("ignore") => {
                    expect_words(&name, words, 3, 3)?;
                    let add = match words[1].to_ascii_lowercase().as_str() {
                        "add" => true,
                        "remove" => false,
                        _ => return Err(format!("'{}' is not an ignore action; use add or remove", words[1])),
                    };
                    Some(SettingsChange::Ignore(add, words[2].clone()))
                }
                Some("repo-items") => {
                    expect_words(&name, words, 2, 2)?;
                    Some(SettingsChange::RepoItems(switch_word(&words[1], false)?.unwrap_or(false)))
                }
                Some(_) => return Err(format!("'{}' is not a setting; use: {}", words[0], usage_of("settings"))),
            };
            Command::Settings(change)
        }
        "token" => {
            expect_words(&name, words, 0, 1)?;
            let action = match words.first().map(|w| w.to_ascii_lowercase()).as_deref() {
                None | Some("status") => TokenAction::Status,
                Some("set") => TokenAction::Set,
                Some("clear") => TokenAction::Clear,
                Some(_) => return Err(format!("'{}' is not a token action; use: {}", words[0], usage_of("token"))),
            };
            Command::Token(action)
        }
        "startup" => {
            expect_words(&name, words, 0, 1)?;
            let action = match words.first().map(|w| w.to_ascii_lowercase()).as_deref() {
                None | Some("status") => StartupAction::Status,
                Some("on") => StartupAction::On,
                Some("off") => StartupAction::Off,
                Some(_) => return Err(format!("'{}' is not a startup action; use: {}", words[0], usage_of("startup"))),
            };
            Command::Startup(action)
        }
        "portable" => {
            expect_words(&name, words, 0, 0)?;
            Command::Portable
        }
        _ => return Err(format!("unknown command '{first}'")),
    };
    flags.finish(&name)?;
    Ok(command)
}

/// Finds a source by its ID or one of its type IDs, or by `owner/name` when only one source has it. Case does not
/// matter.
pub fn find_source<'a>(cfg: &'a Config, query: &str) -> Result<&'a SourceConfig, String> {
    let hits = source_matches(cfg, query);
    let ids = |sources: &mut dyn Iterator<Item = &SourceConfig>| sources.map(|s| s.id.as_str()).collect::<Vec<_>>().join(", ");
    match hits.as_slice() {
        [one] => Ok(one),
        [] if cfg.sources().is_empty() => Err(format!("No source '{query}' is set up; none is (add one with: tss add OWNER/NAME)")),
        [] => Err(format!("No source '{query}' is set up; the IDs are: {}", ids(&mut cfg.sources().iter()))),
        many => Err(format!("Several sources match '{query}' ({}); use the ID", ids(&mut many.iter().copied()))),
    }
}

fn source_matches<'a>(cfg: &'a Config, query: &str) -> Vec<&'a SourceConfig> {
    if let Some(exact) = cfg.find_source(query) {
        return vec![exact];
    }
    let wanted = query.trim().trim_end_matches('/');
    let named = |s: &SourceConfig| s.id.eq_ignore_ascii_case(wanted) || KINDS.iter().any(|&k| s.lanes.id(k).is_some_and(|id| id.eq_ignore_ascii_case(wanted)));
    let by_id: Vec<_> = cfg.sources().iter().filter(|s| named(s)).collect();
    if !by_id.is_empty() {
        return by_id;
    }
    cfg.sources().iter().filter(|s| s.repo.eq_ignore_ascii_case(wanted)).collect()
}

/// The lane of a source that acts on `item`: the type ID typed, the only file type that syncs, or the type the item's
/// name has.
fn file_lane<'a>(cfg: &'a Config, query: &str, item: &str) -> Result<&'a RepoConfig, String> {
    let source = find_source(cfg, query)?;
    let named = KINDS.into_iter().find(|&k| source.lanes.id(k).is_some_and(|id| id != source.id && id.eq_ignore_ascii_case(query.trim())));
    let lanes = cfg.lanes_of(&source.id);
    let files: Vec<&RepoConfig> = lanes.iter().copied().filter(|r| r.kind != SourceKind::Plugins).collect();
    let kind = match (named, files.as_slice()) {
        (Some(kind), _) => kind,
        (None, [one]) => one.kind,
        (None, []) if lanes.iter().any(|r| r.kind == SourceKind::Plugins) => {
            return Err("For Plugins, use approve, reinstall, uninstall or forget.".into());
        }
        _ if item.to_ascii_lowercase().ends_with(".agent.md") => SourceKind::Agents,
        _ if item.to_ascii_lowercase().ends_with(".instructions.md") => SourceKind::Instructions,
        _ => SourceKind::Skills,
    };
    if kind == SourceKind::Plugins {
        return Err("For Plugins, use approve, reinstall, uninstall or forget.".into());
    }
    lane_of(cfg, source, kind)
}

/// The lane of one type of a source, when that type syncs.
fn lane_of<'a>(cfg: &'a Config, source: &SourceConfig, kind: SourceKind) -> Result<&'a RepoConfig, String> {
    source
        .lanes
        .id(kind)
        .and_then(|id| cfg.find(id))
        .ok_or_else(|| format!("{} do not sync from '{}'. Turn them on with: tss source {} types {}=on", kind.heading(), source.id, source.id, kind_word(kind)))
}

/// The word for a type on the command line.
fn kind_word(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Skills => "skills",
        SourceKind::Agents => "agents",
        SourceKind::Instructions => "instructions",
        SourceKind::Plugins => "plugins",
    }
}

enum Fail {
    /// The command line asked for something that does not exist or cannot be done as asked.
    Usage(String),
    Failed(String),
}

impl From<EngineError> for Fail {
    fn from(e: EngineError) -> Self {
        Fail::Failed(e.to_string())
    }
}

macro_rules! say {
    ($out:expr, $($arg:tt)*) => {{
        $out.push_str(&format!($($arg)*));
        $out.push('\n');
    }};
}

/// A blank line between sections, never at the start.
fn gap(out: &mut String) {
    if !out.is_empty() {
        out.push('\n');
    }
}

/// Reports may contain localized Windows errors, paths, or upstream names. Commands suggested for
/// copying use ASCII words only: escaping a non-ASCII word would make it name a different item.
fn ascii_lines(text: &str) -> Cow<'_, str> {
    let safe = printable_lines(text);
    if safe.is_ascii() {
        return safe;
    }
    let mut plain = String::with_capacity(safe.len());
    for ch in safe.chars() {
        if ch.is_ascii() {
            plain.push(ch);
        } else {
            plain.extend(ch.escape_unicode());
        }
    }
    Cow::Owned(plain)
}

/// Runs one command. What it prints goes to `out`, problems go to `err`; the result is the exit code.
///
/// Everything printed passes through `printable_lines`: names and messages come from the repository, and a control
/// character in them could move the cursor or erase the line above.
pub fn run(args: &[String], host: &dyn Host, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let command = match parse(args) {
        Ok(command) => command,
        Err(e) => {
            let why = ascii_lines(&printable(&e)).into_owned();
            let _ = writeln!(err, "tss: {why}\nRun 'tss help' to see the commands.");
            return EXIT_USAGE;
        }
    };
    let mut text = String::new();
    let result = execute(command, host, &mut text, out);
    // Output that cannot be written (a closed pipe) is not a reason to fail the command.
    let _ = out.write_all(ascii_lines(&text).as_bytes()).and_then(|()| out.flush());
    match result {
        Ok(code) => code,
        Err(Fail::Usage(e)) => {
            let _ = writeln!(err, "tss: {}", ascii_lines(&printable(&e)));
            EXIT_USAGE
        }
        Err(Fail::Failed(e)) => {
            let _ = writeln!(err, "tss: {}", ascii_lines(&printable(&e)));
            EXIT_FAILED
        }
    }
}

fn execute(command: Command, host: &dyn Host, out: &mut String, screen: &mut dyn Write) -> Result<i32, Fail> {
    match command {
        Command::Help => {
            out.push_str(HELP);
            Ok(EXIT_OK)
        }
        Command::Version => {
            say!(out, "tss {}", env!("CARGO_PKG_VERSION"));
            Ok(EXIT_OK)
        }
        Command::Status => status(host, out),
        Command::Update(command) => update(host, out, command),
        Command::Check { repo, dry_run, full_listing, delete_all } => check(host, out, repo.as_deref(), dry_run, full_listing, delete_all),
        Command::Resolve { repo, skill, how } => resolve(host, out, &repo, &skill, how),
        Command::ResolvePlugin { repo, choice } => resolve_plugin(host, out, &repo, &choice),
        Command::Add(options) => add(host, out, screen, options),
        Command::Remove { repo, delete_skills, uninstall_plugins } => remove(host, out, &repo, delete_skills, uninstall_plugins),
        Command::Source { repo, change } => source_command(host, out, &repo, change),
        Command::Settings(change) => settings_command(host, out, change),
        Command::Token(action) => token(host, out, action),
        Command::Startup(action) => startup(host, out, action),
        Command::Portable => export_portable(host, out),
    }
}

fn config_path(host: &dyn Host) -> PathBuf {
    host.home().join(CONFIG_FILE)
}

fn load_config(host: &dyn Host) -> Result<(Config, Option<PathBuf>), Fail> {
    let directory = host.executable_dir().map_err(Fail::Failed)?;
    let (config, portable) = load_config_at(host, &directory)?;
    Ok((config, portable.then_some(directory)))
}

fn load_config_at(host: &dyn Host, directory: &Path) -> Result<(Config, bool), Fail> {
    match portable::load_sources(directory).map_err(Fail::Failed)? {
        Some(config) => Ok((config, true)),
        None => Config::load_sources(&config_path(host)).map(|config| (config, false)).map_err(Fail::Failed),
    }
}

fn save_config(host: &dyn Host, directory: Option<&Path>, before: &Config, after: &Config) -> Result<(), String> {
    match directory {
        Some(directory) => portable::save_changes(directory, before, after).map(|_| ()),
        None => {
            if portable::load_sources(&host.executable_dir()?)?.is_some() {
                return Err("portable settings appeared since the local settings were loaded; try the command again".into());
            }
            after.save_if_unchanged(&config_path(host), before)
        }
    }
}

fn export_portable(host: &dyn Host, out: &mut String) -> Result<i32, Fail> {
    let directory = host.executable_dir().map_err(Fail::Failed)?;
    portable::check_export_target(&directory).map_err(Fail::Failed)?;
    let local = Config::load_sources(&config_path(host)).map_err(Fail::Failed)?;
    portable::export(&directory, &local).map_err(Fail::Failed)?;
    say!(out, "Exported shared settings and repositories beside tss.exe. The local config.json was not changed.");
    Ok(EXIT_OK)
}

fn paths(host: &dyn Host, cfg: &Config, portable: bool) -> Result<Paths, Fail> {
    let skills = if portable { host.portable_skills_dir(cfg) } else { Ok(host.skills_dir(cfg)) }.map_err(Fail::Failed)?;
    Ok(host.source_paths(skills))
}

fn update_preferences(out: &mut String, prefs: UpdatePrefs) {
    let every = match prefs.interval {
        UpdateInterval::Daily => "1 day",
        UpdateInterval::Weekly => "7 days",
        UpdateInterval::Monthly => "30 days",
    };
    say!(out, "Automatic app updates: {}", if prefs.enabled { "on" } else { "off" });
    say!(out, "Automatic check interval: {every}");
    match prefs.timing {
        UpdateTiming::Immediate => {
            say!(out, "Install timing: immediate. The tray installs an update when it finds one, then restarts into it.")
        }
        UpdateTiming::OnQuit => {
            say!(out, "Install timing: on Quit. Choose Quit from the tray menu to install a pending update. The tray does not restart itself.")
        }
    }
}

fn update(host: &dyn Host, out: &mut String, command: UpdateCommand) -> Result<i32, Fail> {
    let directory = host.executable_dir().map_err(Fail::Failed)?;
    let (cfg, portable) = load_config_at(host, &directory)?;
    let mut prefs = cfg.app_update;
    match command {
        UpdateCommand::Status => {
            update_preferences(out, prefs);
            say!(out, "Running tss build: {}", env!("CARGO_PKG_VERSION"));
            let installed =
                host.installed_app_version(&directory).map_err(|e| Fail::Failed(format!("Cannot verify the installed tray and tss on disk: {e}")))?;
            say!(out, "Installed on disk (tray and tss): {installed}");
            say!(out, "To see which release is offered without installing it, run 'tss update check'.");
            return Ok(EXIT_OK);
        }
        UpdateCommand::Manual(action) => {
            let paths = paths(host, &cfg, portable)?;
            let report = host.manual_app_update(&paths, &directory, action).map_err(|e| Fail::Failed(format!("App update failed: {e}")))?;
            say!(out, "{report}");
            return Ok(EXIT_OK);
        }
        UpdateCommand::Automatic(on) => prefs.enabled = on,
        UpdateCommand::Every(every) => prefs.interval = every,
        UpdateCommand::When(when) => prefs.timing = when,
    }
    if prefs != cfg.app_update {
        let paths = paths(host, &cfg, portable)?;
        crate::updater::Locations { home: &paths.home, dir: &directory, skills: &paths.skills }.check().map_err(Fail::Failed)?;
        let mut changed = cfg.clone();
        changed.app_update = prefs;
        save_config(host, portable.then_some(directory.as_path()), &cfg, &changed).map_err(Fail::Failed)?;
    }
    update_preferences(out, prefs);
    Ok(EXIT_OK)
}

fn open_engine(host: &dyn Host, cfg: &Config, portable: bool) -> Result<Engine, Fail> {
    let remote = host.remote().map_err(Fail::Failed)?;
    let mut engine = Engine::new(paths(host, cfg, portable)?, remote, host.tokens());
    if let Some(manager) = host.plugin_manager() {
        engine = engine.with_plugin_manager(manager);
    }
    engine.settings_at(host.executable_dir().map_err(Fail::Failed)?, portable);
    Ok(engine)
}

/// What is recorded, read without the engine's lock and without changing anything. Only used to check names
/// typed on the command line; the engine reads it again under its lock before acting.
fn load_state(host: &dyn Host, cfg: &Config, portable: bool) -> Result<State, Fail> {
    let file = paths(host, cfg, portable)?.state_file();
    State::load_sources_with(&file, false).map(|loaded| loaded.state).map_err(|e| Fail::Failed(format!("cannot read {}: {e}", file.display())))
}

/// The board judges a disabled repository as "Disabled" and nothing else. Here the user asked about it, so it is
/// judged like any other and marked as disabled in the heading.
fn all_enabled(cfg: &Config) -> Config {
    let mut shown = cfg.clone();
    for id in cfg.sources().iter().map(|s| s.id.clone()) {
        shown.set_enabled(&id, true);
    }
    shown
}

fn exit_code(level: Level) -> i32 {
    match level {
        Level::Ok => EXIT_OK,
        Level::Attention => EXIT_ATTENTION,
        Level::Error => EXIT_FAILED,
    }
}

/// A word as it has to be typed in PowerShell or cmd, or `None` when no way of writing it is safe to paste.
///
/// Skill names come from the repository, and Windows allows `$ ( ) ; ' % ` and curly quotes in a folder name. Inside
/// double quotes PowerShell still runs `$(...)`, treats a backtick as an escape and ends the string at a curly double
/// quote, and cmd expands `%NAME%`; single quotes are no safer, because curly single quotes close them too. So a word is
/// only written out when it is made of letters, digits, spaces and a few plain marks, and the user is pointed to the
/// app's window for any other.
fn quote(word: &str) -> Option<String> {
    let plain = |c: char| c.is_ascii_alphanumeric() || "-_./:".contains(c);
    if word.is_empty() || !word.chars().all(|c| plain(c) || c == ' ' || c == '+') {
        return None;
    }
    Some(if word.chars().all(plain) { word.to_string() } else { format!("\"{word}\"") })
}

/// A command the user can copy. A word that starts with '-' comes after `--`, so it is not read as an option.
fn command_line(command: &str, words: &[&str], options: &[&str]) -> Option<String> {
    let dashed = words.iter().any(|w| w.starts_with('-'));
    let words: Vec<String> = words.iter().map(|w| quote(w)).collect::<Option<_>>()?;
    let mut parts = vec!["tss".to_string(), command.to_string()];
    if dashed {
        parts.extend(options.iter().map(|o| o.to_string()));
        parts.push("--".to_string());
        parts.extend(words);
    } else {
        parts.extend(words);
        parts.extend(options.iter().map(|o| o.to_string()));
    }
    Some(parts.join(" "))
}

/// The commands that settle a row, and the labels of the choices that have no command that is safe to paste.
fn hints(repo_id: &str, row: &Row) -> (Vec<(String, &'static str)>, Vec<&'static str>) {
    let (mut typed, mut untyped) = (Vec::new(), Vec::new());
    for &action in &row.actions {
        let words = [repo_id, &row.name, word_for(action)];
        let publisher = row.plugin.as_ref().and_then(|p| p.publisher.as_deref());
        let command = match (action, publisher) {
            (Action::RemoveThem, _) => command_line("check", &[repo_id], &["--delete-all"]),
            (Action::ApprovePlugin, Some(publisher)) => command_line("resolve", &[words[0], words[1], words[2], publisher], &[]),
            _ => command_line("resolve", &words, &[]),
        };
        match command {
            Some(command) => typed.push((command, action.label())),
            None => untyped.push(action.label()),
        }
    }
    (typed, untyped)
}

fn header(out: &mut String, view: &RepoView, repo: &RepoConfig) {
    let off = if repo.enabled { "" } else { " (disabled)" };
    say!(out, "{} [{}]{off}: {}", printable(&view.label), printable(&view.id), view.status);
    say!(out, "  Type: {}. Source folder: {}.", repo.kind.heading(), printable(source_folder(repo)));
    if let Some(detail) = &view.detail {
        say!(out, "  {detail}");
    }
}

/// "the whole repository" for an empty folder.
fn folder_text(path: &str) -> &str {
    if path.is_empty() { "the whole repository" } else { path }
}

/// `enabled` is the source's own setting: the status judges a disabled source like any other.
fn source_header(out: &mut String, view: &SourceView, enabled: bool) {
    let off = if enabled { "" } else { " (disabled)" };
    say!(out, "{} [{}]{off}: {}", printable(&view.label), printable(&view.id), view.status);
    let branch = match (view.branch.is_empty(), &view.seen_branch) {
        (false, _) => printable(&view.branch).into_owned(),
        (true, Some(seen)) => format!("the default branch ({})", printable(seen)),
        (true, None) => "the default branch".to_string(),
    };
    say!(out, "  Branch: {branch}. Folder: {}.", printable(folder_text(&view.folder)));
    say!(out, "  Types: {}", view.types_line());
    if let Some(detail) = &view.detail {
        say!(out, "  {detail}");
    }
}

/// The items of a source, grouped by type, with what the last listing skipped or never syncs.
fn kind_groups(out: &mut String, cfg: &Config, view: &SourceView) {
    let lanes = cfg.lanes_of(&view.id);
    if view.kinds.iter().all(|k| k.rows.is_empty()) {
        if !lanes.is_empty() && lanes.iter().all(|r| r.kind == SourceKind::Plugins) {
            say!(out, "  No plugins are recorded. Run 'tss check' to check Copilot.");
        } else {
            let files: Vec<_> = lanes.iter().filter(|r| r.kind != SourceKind::Plugins).map(|r| r.kind).collect();
            let nouns = if let [one] = files.as_slice() { one.nouns().1 } else { "items" };
            say!(out, "  No {nouns} from it are installed.");
        }
    }
    for kind in &view.kinds {
        if kind.rows.is_empty() && kind.filtered == 0 && kind.notes.is_empty() {
            continue;
        }
        say!(out, "  {}:", kind.summary());
        rows(out, &kind.lane, &kind.rows, true);
        if kind.filtered > 0 {
            say!(out, "    Skipped {} in repository-specific or ignored folders.", kind.filtered);
        }
        for note in &kind.notes {
            say!(out, "    Not synced: {} ({})", printable(&note.path), printable(&note.reason));
        }
    }
    for note in &view.notes {
        say!(out, "  Not synced: {} ({})", printable(&note.path), printable(&note.reason));
    }
    if view.more_notes > 0 {
        say!(out, "  {} more items are not synced and are not listed.", view.more_notes);
    }
}

/// Rows that need the user are marked with '!' and followed by the commands that settle them.
fn rows(out: &mut String, repo_id: &str, rows: &[Row], all: bool) {
    for row in rows {
        if !all && row.level == Level::Ok {
            continue;
        }
        let mark = if row.level == Level::Ok { ' ' } else { '!' };
        let detail = printable(&row.text);
        if row.plugin.is_some() && !detail.starts_with(row.tag) {
            say!(out, "  {mark} {}: {}. {detail}", printable(&row.name), row.tag);
        } else {
            say!(out, "  {mark} {}: {detail}", printable(&row.name));
        }
        if let Some(plugin) = &row.plugin {
            say!(out, "      {}", printable(&plugin.facts()));
        }
        let (typed, untyped) = hints(repo_id, row);
        let width = typed.iter().map(|(cmd, _)| cmd.len()).max().unwrap_or(0);
        for (cmd, label) in typed {
            say!(out, "      {cmd:<width$}   ({label})");
        }
        if !untyped.is_empty() {
            say!(out, "      No command is shown because the name is not safe to paste. Open Tiny Skill Sync and choose: {}", untyped.join(" / "));
        }
    }
}

fn messages(out: &mut String, board: &Board) {
    let mut first = true;
    for message in board.messages() {
        if first {
            gap(out);
            first = false;
        }
        let kind = if message.level == Level::Ok { "Note" } else { "Warning" };
        say!(out, "{kind}: {}", printable(&message.text));
    }
}

fn schedule_text(cfg: &Config, enabled: bool, interval: Option<Interval>, sends_token: bool) -> String {
    let mut text = match (enabled, interval) {
        (false, _) => "Disabled: checked only when asked".to_string(),
        (true, Some(every)) => format!("Checked every {}", every.describe()),
        (true, None) => format!("Checked every {} (the default)", cfg.default_interval.describe()),
    };
    if sends_token {
        text.push_str("; sends the GitHub token");
    }
    text.push('.');
    text
}

fn tracking_text(tracking: Tracking) -> &'static str {
    match tracking {
        Tracking { volume_changed: true, .. } => "the drive is not the one on record, so every file was read",
        Tracking { check: Check::Quick, .. } => "the NTFS change journal vouched for the files that were not read",
        Tracking { folder_missing: true, .. } => "the skills folder does not exist",
        Tracking { journal: false, .. } => "this drive keeps no change journal, so every file is read",
        _ => "every file was read; later looks can rely on the change journal",
    }
}

fn status(host: &dyn Host, out: &mut String) -> Result<i32, Fail> {
    let (cfg, portable_directory) = load_config(host)?;
    let portable_mode = portable_directory.is_some();
    let skills_dir = if portable_mode { host.portable_skills_dir(&cfg) } else { Ok(host.skills_dir(&cfg)) }.map_err(Fail::Failed)?;
    say!(out, "Skills folder: {}", skills_dir.display());
    if let Some(directory) = &portable_directory {
        say!(out, "Settings: portable companions beside tss.exe ({})", directory.display());
    } else {
        say!(out, "Settings: {}", config_path(host).display());
    }
    let mut level = Level::Ok;
    for problem in cfg.problems() {
        say!(out, "Settings problem: {problem}");
        level = Level::Attention;
    }

    let verified = open_engine(host, &cfg, portable_mode).and_then(|mut engine| {
        let report = engine.verify(&cfg, Access::ReadOnly)?;
        Ok((engine, report))
    });
    let (engine, report) = match verified {
        Ok(pair) => pair,
        Err(error) => {
            // Even when verification cannot run, the configured IDs are known and may be
            // needed to disambiguate two sources. Do not invent item or installation status.
            for s in cfg.sources() {
                say!(out, "{} [{}]: not verified. Folder: {}.", printable(&source_label(s)), printable(&s.id), printable(folder_text(&s.path)));
            }
            return Err(error);
        }
    };
    let now = now_secs();
    let mut board = Board::new();
    board.set_seen(crate::seen::load(&engine.paths().seen_file()));
    board.note_verify(&report, now);
    for repo in cfg.repos() {
        if let Some(at) = engine.last_check(&repo.id) {
            board.note_last_check(&repo.id, at);
        }
        if let Some(record) = engine.plugin_record(repo) {
            board.note_recorded_plugins(&repo.id, recorded_plugin_rows(record));
        }
        board.note_kept(&repo.id, &engine.kept(&repo.id));
        board.note_lists_nothing(&repo.id, engine.lists_nothing(&repo.id));
    }

    if cfg.sources().is_empty() {
        gap(out);
        say!(out, "{NO_REPOS}");
    }
    for view in board.source_views(&all_enabled(&cfg), now) {
        let Some(source) = cfg.find_source(&view.id) else { continue };
        gap(out);
        source_header(out, &view, source.enabled);
        let sends_token = source.use_token || cfg.lanes_of(&source.id).iter().any(|r| engine.needs_token(&r.id));
        say!(out, "  Last check: {}. {}", view.last_check_text(now), schedule_text(&cfg, source.enabled, source.interval, sends_token));
        kind_groups(out, &cfg, &view);
        if source.enabled {
            level = level.max(view.level);
        }
    }
    for id in board.orphans() {
        gap(out);
        say!(out, "Items recorded for '{}', which is no longer in the settings, are left alone.", printable(id));
        say!(
            out,
            "  To stop tracking them (the files stay): {}",
            command_line("remove", &[id], &[]).unwrap_or_else(|| "tss remove, followed by that id".to_string())
        );
    }
    messages(out, &board);
    if report.stats.probed > 0 {
        gap(out);
        say!(out, "Local check: {} files looked at, {} read ({}).", report.stats.probed, report.stats.hashed, tracking_text(report.tracking));
    }
    Ok(exit_code(level))
}

/// What a check did (or would do) to one skill, in a few words.
fn change(skill: &SkillReport) -> Option<String> {
    let done = match skill.result {
        ApplyResult::Done => true,
        ApplyResult::DryRun => false,
        _ => return None,
    };
    let name = &skill.name;
    Some(match (&skill.verdict, done) {
        (Verdict::Install, true) => format!("Installed {name}"),
        (Verdict::Install, false) => format!("Would install {name}"),
        (Verdict::Update, true) => format!("Updated {name}"),
        (Verdict::Update, false) => format!("Would update {name}"),
        (Verdict::Adopt, true) => format!("Now managing {name} (the installed copy matched upstream)"),
        (Verdict::Adopt, false) => format!("Would start managing {name} (the installed copy matches upstream)"),
        (Verdict::CatchUp, true) => format!("Marked {name} as in sync (your edit is on GitHub too)"),
        (Verdict::CatchUp, false) => format!("Would mark {name} as in sync (your edit is on GitHub too)"),
        (Verdict::AdoptHistorical, true) => format!("Verified and adopted an older GitHub copy of {name}, then updated it"),
        (Verdict::AdoptHistorical, false) => format!("Would adopt and update {name} (a complete older GitHub version matches)"),
        (Verdict::Delete, true) => format!("Removed {name} (gone upstream)"),
        (Verdict::Delete, false) => format!("Would remove {name} (gone upstream)"),
        (Verdict::Forget, true) => format!("Stopped tracking {name} (gone upstream and already deleted here)"),
        (Verdict::Forget, false) => format!("Would stop tracking {name} (gone upstream and already deleted here)"),
        _ => return None,
    })
}

fn size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    match bytes {
        ..KIB => format!("{bytes} bytes"),
        KIB..MIB => format!("{:.1} KB", bytes as f64 / KIB as f64),
        MIB.. => format!("{:.1} MB", bytes as f64 / MIB as f64),
    }
}

fn check(host: &dyn Host, out: &mut String, query: Option<&str>, dry_run: bool, full_listing: bool, delete_all: bool) -> Result<i32, Fail> {
    let (cfg, portable_directory) = load_config(host)?;
    let portable_mode = portable_directory.is_some();
    let mut confirm = false;
    let which = match query {
        Some(query) => {
            let source = find_source(&cfg, query).map_err(Fail::Usage)?;
            // A confirmed deletion covers only the type it names: a type ID, or the source as a whole for its skills
            // lane. A source with no skills lane confirms nothing, never another type's lane.
            let named = KINDS.into_iter().filter_map(|k| source.lanes.id(k)).find(|id| id.eq_ignore_ascii_case(query.trim()));
            let lane = named.or(source.lanes.id(SourceKind::Skills)).filter(|_| delete_all);
            confirm = lane.is_some();
            Which::One(lane.unwrap_or(&source.id).to_owned())
        }
        None if cfg.repos().iter().any(|r| r.enabled) => Which::All,
        None if cfg.repos().is_empty() => {
            say!(out, "{NO_REPOS}");
            return Ok(EXIT_OK);
        }
        None => {
            say!(out, "Every repository is disabled. Check one anyway with: tss check REPO");
            return Ok(EXIT_OK);
        }
    };

    let mut engine = open_engine(host, &cfg, portable_mode)?;
    let now = now_secs();
    let mut board = Board::new();
    // The look at the disk comes first, so local edits are reported even when GitHub says nothing changed.
    let verified = engine.verify(&cfg, if dry_run { Access::ReadOnly } else { Access::Write })?;
    board.note_verify(&verified, now);
    // What the last listing had in it, for a check that GitHub answers with "not modified".
    for repo in cfg.repos() {
        board.note_lists_nothing(&repo.id, engine.lists_nothing(&repo.id));
    }
    let opts = SyncOptions { dry_run, ignore_etag: full_listing, confirm_mass_delete: confirm, ..SyncOptions::default() };
    let mut report = engine.sync(&cfg, &which, &opts, &AtomicBool::new(false))?;
    if !dry_run {
        if let Err(e) = engine.flush() {
            report.warnings.push(format!("the time of this check was not saved: {e}"));
        }
    }
    board.note_source_sync(&report, now, &cfg);

    let views = board.views(&all_enabled(&cfg), now);
    // A type a source does not have is no news while another type of it is there, so it is not shown and does not
    // count toward the exit code. A source that found nothing at all still says so for each type.
    let source_of = |id: &str| cfg.find_source(id).map(|s| s.id.as_str());
    let quiet = |r: &RepoReport| {
        r.lists_nothing
            && r.skills.is_empty()
            && r.plugins.is_empty()
            && !matches!(r.outcome, Outcome::Failed(_))
            && views.iter().find(|v| v.id == r.repo_id).is_some_and(|v| v.skills.is_empty())
    };
    let mut level = Level::Ok;
    for repo_report in &report.repos {
        let (Some(view), Some(repo)) = (views.iter().find(|v| v.id == repo_report.repo_id), cfg.find(&repo_report.repo_id)) else { continue };
        if quiet(repo_report) && report.repos.iter().any(|other| source_of(&other.repo_id) == source_of(&repo_report.repo_id) && !quiet(other)) {
            continue;
        }
        gap(out);
        header(out, view, repo);
        if repo.kind != SourceKind::Plugins && matches!(repo_report.outcome, Outcome::Failed(_)) {
            say!(out, "  Nothing is deleted because a check failed.");
        }
        for skill in &repo_report.skills {
            if let Some(line) = change(skill) {
                say!(out, "  {line}");
            }
        }
        rows(out, &view.id, &view.skills, repo.kind == SourceKind::Plugins);
        if view.skills.iter().any(|r| r.actions.contains(&Action::PauseHistory)) {
            say!(out, "  History work is saved. The running tray continues it; otherwise run tss check again to continue.");
        }
        if repo_report.downloaded > 0 {
            say!(out, "  Downloaded {}.", size(repo_report.downloaded));
        }
        level = level.max(view.level);
    }
    messages(out, &board);
    if dry_run {
        gap(out);
        say!(out, "Dry run: nothing was changed.");
    }
    Ok(exit_code(level))
}

fn resolve(host: &dyn Host, out: &mut String, query: &str, skill: &str, how: Resolution) -> Result<i32, Fail> {
    let (cfg, portable_directory) = load_config(host)?;
    let portable_mode = portable_directory.is_some();
    let repo = file_lane(&cfg, query, skill).map_err(Fail::Usage)?;
    // The engine checks `replace` against upstream itself. The other two only change a list, so a typo would be
    // recorded as it is: the name is checked here.
    let name = match how {
        _ if repo.kind != SourceKind::Skills => {
            // Loose files are keyed by this source ID. Only the engine knows its binding and
            // ownership under lock; do not look for a similarly named folder under skills.
            check_component(skill).map_err(|e| Fail::Usage(format!("invalid item name: {e}")))?;
            skill.to_owned()
        }
        Resolution::Overwrite | Resolution::PauseHistory | Resolution::ResumeHistory => skill.to_string(),
        Resolution::KeepMine => installed_name(host, &cfg, portable_mode, repo, skill)?,
        Resolution::ManageAgain => kept_name(host, &cfg, portable_mode, repo, skill)?,
    };
    let mut engine = open_engine(host, &cfg, portable_mode)?;
    let report = engine.resolve(&cfg, &repo.id, &name, how, &AtomicBool::new(false))?;
    say!(out, "{}.", report.message);
    for backup in &report.backups {
        say!(out, "Backup: {}", backup.display());
    }
    Ok(EXIT_OK)
}

fn resolve_plugin(host: &dyn Host, out: &mut String, query: &str, choice: &Choice) -> Result<i32, Fail> {
    let (cfg, portable_directory) = load_config(host)?;
    let source = find_source(&cfg, query).map_err(Fail::Usage)?;
    if source.lanes.id(SourceKind::Plugins).and_then(|id| cfg.find(id)).is_none() {
        return Err(Fail::Usage(format!(
            "Approve, reinstall, uninstall and forget are only for plugins, and plugins do not sync from '{}'. Turn them on with: tss source {} types plugins=on",
            source.id, source.id
        )));
    }
    let repo = lane_of(&cfg, source, SourceKind::Plugins).map_err(Fail::Usage)?;
    if !normal_name(choice.name()) {
        return Err(Fail::Usage("Use the plugin name as shown by 'tss check'.".into()));
    }
    let mut engine = open_engine(host, &cfg, portable_directory.is_some())?;
    let mut report = engine.resolve_plugin(&cfg, &repo.id, choice, &AtomicBool::new(false))?;
    if let (Choice::Approve { name, publisher }, Outcome::Failed(failure)) = (choice, &mut report.outcome) {
        let expected = report.plugins.iter().find(|r| r.name == *name && r.path.is_some() && r.status != crate::plugins::Status::Blocked);
        if let Some(expected) = expected.and_then(|r| r.publisher.as_deref()).filter(|p| Some(*p) != publisher.as_deref()) {
            let command = command_line("resolve", &[&repo.id, name, "approve", expected], &[]);
            let help = command.map_or_else(|| "Approve it in the Tiny Skill Sync window.".into(), |cmd| format!("Approve it with: {cmd}"));
            failure.message = format!("The plugin '{name}' is published by '{}'. {help}", printable(expected));
        }
    }
    let mut board = Board::new();
    let shown = all_enabled(&cfg);
    let now = now_secs();
    board.note_source_report(&report, now, &shown);
    let mut code = EXIT_OK;
    if let Some(view) = board.views(&shown, now).iter().find(|v| v.id == repo.id) {
        header(out, view, repo);
        rows(out, &view.id, &view.skills, true);
        code = exit_code(view.level);
    }
    Ok(if matches!(report.outcome, Outcome::Cancelled) { EXIT_FAILED } else { code })
}

/// The skill as it is recorded for the repository, or a folder of that name in the skills folder.
fn installed_name(host: &dyn Host, cfg: &Config, portable: bool, repo: &RepoConfig, skill: &str) -> Result<String, Fail> {
    let state = load_state(host, cfg, portable)?;
    let key = fold(skill);
    if let Some(owner) = state.names_owned_elsewhere(&repo.id).get(&key) {
        let instead =
            command_line("resolve", &[owner, skill, "keep"], &[]).unwrap_or_else(|| format!("the same command with '{owner}' in place of '{}'", repo.id));
        return Err(Fail::Usage(format!("'{skill}' is managed for '{owner}', not '{}'; use: {instead}", repo.id)));
    }
    if let Some(owned) = state.repo(&repo.id).and_then(|r| r.skill(skill)) {
        return Ok(owned.name.clone());
    }
    let dir = if portable { host.portable_skills_dir(cfg) } else { Ok(host.skills_dir(cfg)) }.map_err(Fail::Failed)?;
    let folder = fs::read_dir(&dir).ok().and_then(|entries| {
        entries
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .find(|name| fold(name) == key)
    });
    folder.ok_or_else(|| Fail::Usage(format!("'{skill}' is neither installed from {} nor a folder in {}", repo.id, dir.display())))
}

fn kept_name(host: &dyn Host, cfg: &Config, portable: bool, repo: &RepoConfig, skill: &str) -> Result<String, Fail> {
    let state = load_state(host, cfg, portable)?;
    let kept = state.repo(&repo.id).map(|r| r.ignored.clone()).unwrap_or_default();
    let key = fold(skill);
    kept.iter().find(|name| fold(name) == key).cloned().ok_or_else(|| {
        let which = if kept.is_empty() { "none is".to_string() } else { format!("these are: {}", kept.join(", ")) };
        Fail::Usage(format!("'{skill}' is not kept as yours in {}; {which}", repo.id))
    })
}

fn already_set_up(old: &SourceConfig) -> String {
    crate::app::duplicate_source_message(&old.id)
}

/// The read-only preview of a location: where it is, and for each type what would sync, what would not and why.
fn print_preview(out: &mut String, engine: &Engine, preview: &LocationPreview) {
    for line in preview_lines(preview, &engine.paths().skills) {
        if line.is_empty() {
            gap(out);
        } else {
            say!(out, "{line}");
        }
    }
}

fn add(host: &dyn Host, out: &mut String, screen: &mut dyn Write, options: AddSource) -> Result<i32, Fail> {
    let location = Location::parse(&options.spec).map_err(Fail::Usage)?;
    if options.preview && options.approve_plugins {
        return Err(Fail::Usage("A preview saves nothing, so it cannot approve plugins. Leave out --preview or --approve-plugins.".into()));
    }
    if !options.preview && !options.yes && !host.interactive() {
        return Err(Fail::Usage(
            "Cannot ask for confirmation because input is not a terminal. Add --yes to save without asking, or --preview to look first.".into(),
        ));
    }
    let (mut cfg, portable_directory) = load_config(host)?;
    let repo = format!("{}/{}", location.owner, location.name);
    if location.target == Target::Bare && !options.preview {
        if let Some(old) = same_location(&cfg, &repo, "", "") {
            return Err(Fail::Usage(already_set_up(old)));
        }
    }
    let cancel = AtomicBool::new(false);
    let mut engine = open_engine(host, &cfg, portable_directory.is_some())?;
    let preview = engine
        .preview_location(&cfg, &options.spec, options.use_token, &cancel)
        .map_err(|why| Fail::Failed(format!("Could not read {repo}: {}. Nothing was saved or installed.", why.to_string().trim_end_matches('.'))))?;
    print_preview(out, &engine, &preview);
    gap(out);
    if let Some(old) = same_location(&cfg, &preview.source.repo, &preview.source.branch, &preview.source.path) {
        if !options.preview {
            return Err(Fail::Usage(already_set_up(old)));
        }
        say!(out, "This location is already set up as '{}'.", old.id);
    }
    if options.preview {
        say!(out, "Nothing was saved or installed.");
        return Ok(EXIT_OK);
    }
    if !options.yes {
        say!(out, "Save this source? Type yes to save, or press Enter to cancel.");
        screen.write_all(ascii_lines(out).as_bytes()).and_then(|()| screen.flush()).map_err(|e| Fail::Failed(format!("could not show the preview: {e}")))?;
        out.clear();
        if !host.confirm_source().map_err(Fail::Failed)? {
            say!(out, "Cancelled. Nothing was saved or installed.");
            return Ok(EXIT_OK);
        }
    }
    // Save only what was shown: what GitHub has, where it would go and the settings must all be as they were.
    let again = engine.preview_location(&cfg, &options.spec, options.use_token, &cancel)?;
    if again != preview {
        return Err(Fail::Failed("What GitHub has, or where it would go, changed since the preview. Preview the source again.".into()));
    }
    let (active, directory) = load_config(host)?;
    if active != cfg || directory != portable_directory || paths(host, &active, directory.is_some())?.skills != engine.paths().skills {
        return Err(Fail::Failed("The settings changed. Preview the source again.".into()));
    }
    let mut source = preview.source.clone();
    source.interval = options.every;
    let before = cfg.clone();
    cfg.add_source_config(source.clone()).map_err(Fail::Failed)?;
    save_config(host, portable_directory.as_deref(), &before, &cfg).map_err(Fail::Failed)?;
    say!(out, "Added {} as '{}'. {}", printable(&source_label(&source)), source.id, schedule_text(&cfg, true, source.interval, source.use_token));

    let plugins = previewed_plugins(&preview);
    if !plugins.is_empty() {
        if options.approve_plugins {
            engine
                .approve_location_plugins(&cfg, &source.id, &preview)
                .map_err(|e| Fail::Failed(format!("The source was saved, but its plugins could not be approved: {e}")))?;
            say!(out, "Approved {} {}: {}", plugins.len(), if plugins.len() == 1 { "plugin" } else { "plugins" }, printable(&plugins.join(", ")));
        } else {
            say!(
                out,
                "Its plugins wait for your approval: {}. After a check, approve each with the command 'tss status' shows.",
                printable(&plugins.join(", "))
            );
        }
    }
    if let Some(command) = command_line("check", &[&source.id], &[]) {
        say!(out, "Install what it has with: {command}");
    }
    Ok(EXIT_OK)
}

fn remove(host: &dyn Host, out: &mut String, query: &str, delete_skills: bool, uninstall_plugins: bool) -> Result<i32, Fail> {
    let (mut cfg, portable_directory) = load_config(host)?;
    let portable_mode = portable_directory.is_some();
    let source = match find_source(&cfg, query) {
        Ok(source) => source.clone(),
        // A source that left the settings while its items stayed on record can be let go of too.
        Err(e) if source_matches(&cfg, query).is_empty() => {
            let flags = (delete_skills, uninstall_plugins);
            return remove_recorded(host, out, &cfg, portable_mode, query, flags, e);
        }
        Err(e) => return Err(Fail::Usage(e)),
    };
    let mut engine = open_engine(host, &cfg, portable_mode)?;
    let lanes = crate::app::removal_lanes(&engine, &cfg, &source).map_err(Fail::Failed)?;
    if delete_skills && !lanes.iter().any(|(_, k)| *k == SourceKind::Skills) {
        return Err(Fail::Usage("--delete-skills is only for a source with skills. Remove this source without it to keep its files.".into()));
    }
    if uninstall_plugins && !lanes.iter().any(|(_, k)| *k == SourceKind::Plugins) {
        return Err(Fail::Usage("--uninstall-plugins is only for a source with plugins.".into()));
    }
    let reports = crate::app::remove_whole_source(&mut engine, &cfg, &lanes, delete_skills, uninstall_plugins)?;

    let before = cfg.clone();
    cfg.remove(&source.id);
    let saved = save_config(host, portable_directory.as_deref(), &before, &cfg);
    if saved.is_ok() {
        say!(out, "Removed {} [{}] from the settings.", printable(&source_label(&source)), source.id);
    }
    let mut kept_skills = false;
    for (_, kind, report) in &reports {
        kept_skills |= describe_removal(out, *kind, report, delete_skills && *kind == SourceKind::Skills, uninstall_plugins);
    }
    if let Err(e) = saved {
        return Err(Fail::Failed(still_in_settings(host, portable_directory.as_deref(), &e, &source.id)));
    }
    Ok(if kept_skills { EXIT_ATTENTION } else { EXIT_OK })
}

/// Says what letting go of one type did. True when skills that were to be deleted were kept.
fn describe_removal(out: &mut String, kind: SourceKind, report: &RemoveReport, deleting: bool, uninstall_plugins: bool) -> bool {
    let items = kind.nouns().1;
    if kind == SourceKind::Plugins && uninstall_plugins {
        say!(out, "Uninstalled {} {} from Copilot.", report.deleted.len(), if report.deleted.len() == 1 { "plugin" } else { "plugins" });
    } else if !report.deleted.is_empty() {
        say!(out, "Deleted: {}", report.deleted.join(", "));
    }
    if let Some(message) = report.message {
        say!(out, "{message}");
    } else if deleting {
        for (name, why) in &report.kept {
            say!(out, "Kept {name}, which is now yours: {why}");
        }
        return !report.kept.is_empty();
    } else if !report.kept.is_empty() {
        let names: Vec<&str> = report.kept.iter().map(|(name, _)| name.as_str()).collect();
        say!(out, "These {items} stay installed and are now yours: {}", names.join(", "));
    }
    false
}

fn still_in_settings(host: &dyn Host, portable_directory: Option<&Path>, error: &str, id: &str) -> String {
    let next_step = if let Some(directory) = portable_directory {
        format!("update the portable settings beside tss.exe in {} by hand", directory.display())
    } else {
        format!("remove it from {} by hand", config_path(host).display())
    };
    format!("{}. The items of '{id}' are no longer managed, but it is still in the settings: {next_step}", capitalized(error))
}

/// Let go of items recorded under an ID that is no longer in the settings.
fn remove_recorded(
    host: &dyn Host,
    out: &mut String,
    cfg: &Config,
    portable_mode: bool,
    id: &str,
    (delete_skills, uninstall_plugins): (bool, bool),
    not_found: String,
) -> Result<i32, Fail> {
    let record = load_state(host, cfg, portable_mode)?;
    let kind = match record.loose_sources().get(id) {
        Some(r) => r.binding().kind(),
        None if record.plugin_sources().contains_key(id) => SourceKind::Plugins,
        None if record.repo(id).is_some_and(|r| !r.skills.is_empty()) => SourceKind::Skills,
        None => return Err(Fail::Usage(not_found)),
    };
    if delete_skills && kind != SourceKind::Skills {
        return Err(Fail::Usage("--delete-skills is only for skills. Remove these items without it to keep their files.".into()));
    }
    if uninstall_plugins && kind != SourceKind::Plugins {
        return Err(Fail::Usage("--uninstall-plugins is only for plugins.".into()));
    }
    let mut engine = open_engine(host, cfg, portable_mode)?;
    let report = if uninstall_plugins {
        engine.remove_source_with_plugins(cfg, id, &AtomicBool::new(false))?
    } else {
        crate::app::remove_source(&mut engine, cfg, id, delete_skills)?
    };
    say!(out, "Stopped tracking the {} recorded for '{}'.", kind.nouns().1, printable(id));
    let kept = describe_removal(out, kind, &report, delete_skills, uninstall_plugins);
    Ok(if kept { EXIT_ATTENTION } else { EXIT_OK })
}

fn on_off(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

/// The ignored folder names: the global list with this source's additions, less what it allows, as typed.
fn ignored_names(global: &GlobalSettings, source: &SourceConfig) -> Vec<String> {
    let allowed: Vec<String> = source.ignore_allow.iter().map(|n| fold(n)).collect();
    let mut names: Vec<String> = Vec::new();
    for name in global.ignore_folders.iter().chain(&source.ignore_add) {
        if !allowed.contains(&fold(name)) && !names.iter().any(|n| fold(n) == fold(name)) {
            names.push(name.clone());
        }
    }
    names
}

fn list_or_none(names: &[String]) -> String {
    if names.is_empty() { "none".to_string() } else { names.iter().map(|n| printable(n).into_owned()).collect::<Vec<_>>().join(", ") }
}

const REPO_FOLDERS_TEXT: &str = ".github, .agents, .claude, .copilot, .codex, .cursor, .gemini";

fn show_source(out: &mut String, cfg: &Config, source: &SourceConfig) {
    let global = cfg.global();
    let off = if source.enabled { "" } else { " (disabled)" };
    say!(out, "{} [{}]{off}", printable(&source_label(source)), printable(&source.id));
    let branch = if source.branch.is_empty() { "the default branch".into() } else { printable(&source.branch) };
    say!(out, "  Branch: {branch}. Folder: {}.", printable(folder_text(&source.path)));
    let types = source.effective_types(global);
    for kind in KINDS {
        let own = source.types.get(kind);
        let why = if own.is_some() { "set for this source" } else { "the default" };
        let id = source.lanes.id(kind).map(|id| format!(" [{}]", printable(id))).unwrap_or_default();
        say!(out, "  {}: Sync {} ({why}){id}", kind.heading(), on_off(types.enabled(kind)));
    }
    let repo_items = source.include_repo_items.unwrap_or(global.include_repo_items);
    let why = if source.include_repo_items.is_some() { "set for this source" } else { "the default" };
    say!(out, "  Skills and agents in repository-specific folders ({REPO_FOLDERS_TEXT}): {} ({why})", if repo_items { "synced" } else { "skipped" });
    say!(out, "  Ignored folders: {}", list_or_none(&ignored_names(global, source)));
    if !source.ignore_add.is_empty() {
        say!(out, "  Also ignored for this source: {}", list_or_none(&source.ignore_add));
    }
    if !source.ignore_allow.is_empty() {
        say!(out, "  Not ignored for this source: {}", list_or_none(&source.ignore_allow));
    }
}

fn show_settings(out: &mut String, global: &GlobalSettings) {
    say!(out, "Defaults for every source:");
    for kind in KINDS {
        say!(out, "  {}: Sync {}", kind.heading(), on_off(global.types.enabled(kind)));
    }
    say!(out, "  Skills and agents in repository-specific folders ({REPO_FOLDERS_TEXT}): {}", if global.include_repo_items { "synced" } else { "skipped" });
    say!(out, "  Ignored folders: {}", list_or_none(&global.ignore_folders));
}

const TURNED_OFF: &str = "Turning a type off deletes nothing: what it installed stays as it is and shows as Sync off.";

fn source_command(host: &dyn Host, out: &mut String, query: &str, change: Option<SourceChange>) -> Result<i32, Fail> {
    let (mut cfg, portable_directory) = load_config(host)?;
    let source = find_source(&cfg, query).map_err(Fail::Usage)?.clone();
    let Some(change) = change else {
        show_source(out, &cfg, &source);
        return Ok(EXIT_OK);
    };
    let mut edited = source.clone();
    match change {
        SourceChange::Types(changes) => {
            for (kind, value) in changes {
                edited.set_type(kind, value);
            }
        }
        SourceChange::Ignore(action, name) => edited.ignore(action, &name).map_err(Fail::Usage)?,
        SourceChange::RepoItems(value) => edited.set_repo_items(value),
    }
    if edited == source {
        say!(out, "Nothing changed: the source already has that setting.");
        show_source(out, &cfg, &source);
        return Ok(EXIT_OK);
    }
    edited.effective_policy(cfg.global()).map_err(|why| Fail::Usage(capitalized(&why)))?;
    let (was, now) = (source.effective_types(cfg.global()), edited.effective_types(cfg.global()));
    let before = cfg.clone();
    cfg.edit(|sources, _| {
        if let Some(s) = sources.iter_mut().find(|s| s.id == source.id) {
            *s = edited.clone();
        }
    });
    if KINDS.iter().any(|&k| !was.enabled(k) && now.enabled(k)) {
        let state = load_state(host, &cfg, portable_directory.is_some())?;
        crate::engine::fresh_lane_ids(&state, &before, &mut cfg);
    }
    let edited = cfg.find_source(&source.id).cloned().unwrap_or(edited);
    save_config(host, portable_directory.as_deref(), &before, &cfg).map_err(Fail::Failed)?;
    say!(out, "Saved. The change applies on the next check of '{}'.", source.id);
    if KINDS.iter().any(|&k| was.enabled(k) && !now.enabled(k)) {
        say!(out, "{TURNED_OFF}");
    }
    show_source(out, &cfg, &edited);
    Ok(EXIT_OK)
}

fn settings_command(host: &dyn Host, out: &mut String, change: Option<SettingsChange>) -> Result<i32, Fail> {
    let (mut cfg, portable_directory) = load_config(host)?;
    let Some(change) = change else {
        show_settings(out, cfg.global());
        return Ok(EXIT_OK);
    };
    let mut global = cfg.global().clone();
    let mut turned_off = false;
    match change {
        SettingsChange::Types(changes) => {
            for (kind, on) in changes {
                turned_off |= global.set_type(kind, on);
            }
        }
        SettingsChange::Ignore(add, name) => global.ignore(add, &name).map_err(Fail::Usage)?,
        SettingsChange::RepoItems(on) => global.set_repo_items(on),
    }
    if global == *cfg.global() {
        say!(out, "Nothing changed: that is already the setting.");
        show_settings(out, cfg.global());
        return Ok(EXIT_OK);
    }
    let before = cfg.clone();
    let turned_on = KINDS.iter().any(|&k| !before.global().types.enabled(k) && global.types.enabled(k));
    cfg.edit(|_, g| *g = global);
    if turned_on {
        let state = load_state(host, &cfg, portable_directory.is_some())?;
        crate::engine::fresh_lane_ids(&state, &before, &mut cfg);
    }
    save_config(host, portable_directory.as_deref(), &before, &cfg).map_err(Fail::Failed)?;
    say!(out, "Saved. The change applies to every source that follows the defaults, on its next check.");
    if turned_off {
        say!(out, "{TURNED_OFF}");
    }
    show_settings(out, cfg.global());
    Ok(EXIT_OK)
}

fn token(host: &dyn Host, out: &mut String, action: TokenAction) -> Result<i32, Fail> {
    match action {
        TokenAction::Set => {
            let token = host.read_token().map_err(Fail::Failed)?;
            host.save_token(&token).map_err(Fail::Failed)?;
            say!(out, "The token was saved in Windows Credential Manager as {}.", cred::TOKEN_TARGET);
            Ok(EXIT_OK)
        }
        TokenAction::Clear => {
            if host.delete_token().map_err(Fail::Failed)? {
                say!(out, "The saved token was removed.");
            } else {
                say!(out, "No token was saved.");
            }
            Ok(EXIT_OK)
        }
        TokenAction::Status => {
            let (cfg, portable_directory) = load_config(host)?;
            let portable_mode = portable_directory.is_some();
            let available = if host.token_saved() {
                say!(out, "Saved token: yes, in Windows Credential Manager ({}).", cred::TOKEN_TARGET);
                say!(out, "GitHub CLI: not asked; the saved token comes first.");
                true
            } else {
                say!(out, "Saved token: none.");
                match host.gh_signed_in() {
                    Ok(()) => {
                        say!(out, "GitHub CLI: signed in; its token is used.");
                        true
                    }
                    Err(why) => {
                        say!(out, "GitHub CLI: {why}.");
                        false
                    }
                }
            };
            // The repositories the user marked, and those found to be private. A state that cannot be read only hides
            // the second kind, so it is not a failure here.
            let state = load_state(host, &cfg, portable_mode).unwrap_or_default();
            let sends = |r: &RepoConfig| {
                r.use_token || state.repo(&r.id).is_some_and(|s| s.needs_token) || state.loose_sources().get(&r.id).is_some_and(|s| s.poll.needs_token)
            };
            let users: Vec<&str> = cfg.repos().iter().filter(|r| sends(r)).map(|r| r.id.as_str()).collect();
            if users.is_empty() {
                say!(out, "No repository sends the token. It is tried by itself when GitHub cannot find a repository without it.");
            } else {
                say!(out, "Repositories that send the token: {}", users.join(", "));
            }
            if !available && cfg.repos().iter().any(|r| sends(r) && r.enabled) {
                say!(out, "They cannot be checked until a token is saved (tss token set) or the GitHub CLI is signed in (gh auth login).");
                return Ok(EXIT_ATTENTION);
            }
            Ok(EXIT_OK)
        }
    }
}

/// Only the registry is read or written: no settings, no state, no skills folder and no network.
fn startup(host: &dyn Host, out: &mut String, action: StartupAction) -> Result<i32, Fail> {
    let state = match action {
        StartupAction::Status => host.startup().map_err(|e| format!("Start with Windows cannot be read: {e}")),
        StartupAction::On => host.set_startup(true).map_err(|e| format!("Start with Windows could not be turned on: {e}")),
        StartupAction::Off => host.set_startup(false).map_err(|e| format!("Start with Windows could not be turned off: {e}")),
    }
    .map_err(Fail::Failed)?;
    say!(out, "Start with Windows: {}", state.summary());
    say!(out, "{}", state.note(Via::Tss));
    Ok(EXIT_OK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interval::Unit;

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_string).collect()
    }

    fn parsed(line: &str) -> Command {
        parse(&args(line)).unwrap_or_else(|e| panic!("'{line}' was refused: {e}"))
    }

    fn refused(line: &str) -> String {
        match parse(&args(line)) {
            Ok(command) => panic!("'{line}' was accepted as {command:?}"),
            Err(e) => e,
        }
    }

    #[test]
    fn plugin_help_keeps_check_and_private_repo_guidance_and_fits_80_columns() {
        assert!(HELP.contains("Apply what is safe: install, update, remove."));
        assert!(HELP.contains("'sync' is another name for 'check'."));
        let add = HELP.split_once("  tss add ").unwrap().1.split_once("  tss remove ").unwrap().0;
        assert!(add.contains("Private repositories use the saved token or the"));
        assert!(add.contains("GitHub CLI sign-in"));
        assert!(HELP.contains("omit it only when the publisher is not declared"));
        assert!(HELP.is_ascii());
        assert!(HELP.lines().all(|l| l.len() <= 80));
    }

    #[test]
    fn commands_and_options_are_understood() {
        assert_eq!(parsed("status"), Command::Status);
        assert_eq!(parsed("STATUS"), Command::Status);
        assert_eq!(parsed("sync fx --dry-run"), parsed("check fx --dry-run"));
        assert_eq!(parsed("check"), Command::Check { repo: None, dry_run: false, full_listing: false, delete_all: false });
        assert_eq!(
            parsed("check fx --dry-run --full-listing --delete-all"),
            Command::Check { repo: Some("fx".into()), dry_run: true, full_listing: true, delete_all: true }
        );
        assert_eq!(parsed("check --dry-run fx"), Command::Check { repo: Some("fx".into()), dry_run: true, full_listing: false, delete_all: false });
        assert_eq!(parsed("resolve fx alpha replace"), Command::Resolve { repo: "fx".into(), skill: "alpha".into(), how: Resolution::Overwrite });
        assert_eq!(parsed("resolve fx alpha Restore"), Command::Resolve { repo: "fx".into(), skill: "alpha".into(), how: Resolution::Overwrite });
        assert_eq!(parsed("resolve fx alpha keep"), Command::Resolve { repo: "fx".into(), skill: "alpha".into(), how: Resolution::KeepMine });
        assert_eq!(parsed("resolve fx alpha leave"), Command::Resolve { repo: "fx".into(), skill: "alpha".into(), how: Resolution::KeepMine });
        assert_eq!(parsed("resolve fx alpha manage"), Command::Resolve { repo: "fx".into(), skill: "alpha".into(), how: Resolution::ManageAgain });
        let add = AddSource { spec: "o/r".into(), ..Default::default() };
        assert_eq!(parsed("add o/r"), Command::Add(add.clone()));
        let every = Some(Interval::new(6, Unit::Hour));
        assert_eq!(parsed("add o/r --every 6h --use-token"), Command::Add(AddSource { every, use_token: true, ..add.clone() }));
        assert_eq!(parsed("add --every=6h o/r"), Command::Add(AddSource { every, ..add.clone() }));
        assert_eq!(parsed("add --preview o/r"), Command::Add(AddSource { preview: true, ..add.clone() }));
        assert_eq!(parsed("add o/r --approve-plugins --yes"), Command::Add(AddSource { approve_plugins: true, yes: true, ..add }));
        assert_eq!(parsed("remove fx"), Command::Remove { repo: "fx".into(), delete_skills: false, uninstall_plugins: false });
        assert_eq!(parsed("source fx"), Command::Source { repo: "fx".into(), change: None });
        assert_eq!(
            parsed("source fx types skills=off Agents=default"),
            Command::Source { repo: "fx".into(), change: Some(SourceChange::Types(vec![(SourceKind::Skills, Some(false)), (SourceKind::Agents, None)])) }
        );
        assert_eq!(
            parsed("source fx ignore allow samples"),
            Command::Source { repo: "fx".into(), change: Some(SourceChange::Ignore(IgnoreAction::Allow, "samples".into())) }
        );
        assert_eq!(parsed("source fx repo-items on"), Command::Source { repo: "fx".into(), change: Some(SourceChange::RepoItems(Some(true))) });
        assert_eq!(parsed("settings"), Command::Settings(None));
        assert_eq!(parsed("settings types plugins=off"), Command::Settings(Some(SettingsChange::Types(vec![(SourceKind::Plugins, false)]))));
        assert_eq!(parsed("settings ignore remove tests"), Command::Settings(Some(SettingsChange::Ignore(false, "tests".into()))));
        assert_eq!(parsed("settings repo-items on"), Command::Settings(Some(SettingsChange::RepoItems(true))));
        assert_eq!(parsed("remove fx --delete-skills"), Command::Remove { repo: "fx".into(), delete_skills: true, uninstall_plugins: false });
        assert_eq!(parsed("token"), Command::Token(TokenAction::Status));
        assert_eq!(parsed("token set"), Command::Token(TokenAction::Set));
        assert_eq!(parsed("token CLEAR"), Command::Token(TokenAction::Clear));
        assert_eq!(parsed("startup"), Command::Startup(StartupAction::Status));
        assert_eq!(parsed("startup status"), Command::Startup(StartupAction::Status));
        assert_eq!(parsed("Startup ON"), Command::Startup(StartupAction::On));
        assert_eq!(parsed("startup off"), Command::Startup(StartupAction::Off));
        assert_eq!(parsed("portable"), Command::Portable);
        for help in ["help", "-h", "--help", "check --help", "help me"] {
            assert_eq!(parsed(help), Command::Help, "{help}");
        }
        for version in ["version", "-V", "--version", "status --version"] {
            assert_eq!(parsed(version), Command::Version, "{version}");
        }
    }

    #[test]
    fn words_after_a_double_dash_are_never_options() {
        assert_eq!(parsed("resolve fx -- -odd keep"), Command::Resolve { repo: "fx".into(), skill: "-odd".into(), how: Resolution::KeepMine });
        assert_eq!(parsed("check -- --help"), Command::Check { repo: Some("--help".into()), dry_run: false, full_listing: false, delete_all: false });
        assert_eq!(parsed("check -"), Command::Check { repo: Some("-".into()), dry_run: false, full_listing: false, delete_all: false });
    }

    #[test]
    fn mistakes_are_refused_with_a_reason() {
        assert!(parse(&[]).unwrap_err().contains("no command"));
        assert!(refused("sycn").contains("unknown command 'sycn'"));
        assert!(refused("status extra").contains("'extra' was not expected"));
        assert!(refused("check a b").contains("'b' was not expected"));
        assert!(refused("check --force").contains("'check' has no option --force"));
        assert!(refused("status --dry-run").contains("'status' has no option --dry-run"));
        assert!(refused("check -x").contains("unknown option '-x'"));
        assert!(refused("check --dry-run=yes").contains("--dry-run takes no value"));
        assert!(refused("check --delete-all").contains("one repository"));
        assert!(refused("resolve fx alpha").contains("something is missing"));
        assert!(refused("resolve fx alpha delete").contains("'delete' is not something resolve can do"));
        assert!(refused("add").contains("something is missing"));
        assert!(refused("add o/r --every").contains("needs an interval"));
        assert!(refused("add o/r --every 1m").contains("--every: polling more often"));
        assert!(refused("add o/r --every soon").contains("--every:"));
        assert!(refused("add o/r --type agents").contains("tss source REPO types"));
        assert!(refused("add o/r --type=skills --preview").contains("found by themselves"));
        assert!(refused("source fx types").contains("something is missing"));
        assert!(refused("source fx types skills=maybe").contains("not on, off or default"));
        assert!(refused("source fx types packages=on").contains("packages"));
        assert!(refused("settings types skills=default").contains("not on or off"));
        assert!(refused("source fx ignore drop tests").contains("not an ignore action"));
        assert!(refused("settings ignore allow tests").contains("use add or remove"));
        assert!(refused("remove").contains("something is missing"));
        assert!(refused("token show").contains("'show' is not a token action"));
        assert!(refused("startup maybe").contains("'maybe' is not a startup action; use: tss startup [status|on|off]"));
        assert!(refused("startup on off").contains("'off' was not expected"));
        assert!(refused("startup on --now").contains("'startup' has no option --now"));
        assert!(refused("portable now").contains("'now' was not expected"));
    }

    fn cfg_with(repos: &[(&str, &str, &str)]) -> Config {
        let repos = repos
            .iter()
            .map(|(id, repo, path)| RepoConfig { id: id.to_string(), repo: repo.to_string(), path: path.to_string(), ..RepoConfig::default() })
            .collect();
        Config::from_lanes(repos)
    }

    #[test]
    fn repositories_are_found_by_id_or_unique_name() {
        let cfg = cfg_with(&[("fx", "Owner/Fixtures", "skills"), ("a", "o/twice", "x"), ("b", "o/twice", "y")]);
        assert_eq!(find_source(&cfg, "fx").unwrap().id, "fx");
        assert_eq!(find_source(&cfg, "FX").unwrap().id, "fx");
        assert_eq!(find_source(&cfg, "owner/fixtures").unwrap().id, "fx");
        assert_eq!(find_source(&cfg, "owner/fixtures/").unwrap().id, "fx");
        assert_eq!(find_source(&cfg, "FX-agents").unwrap().id, "fx");
        let e = find_source(&cfg, "o/twice").unwrap_err();
        assert!(e.contains("Several sources match") && e.contains("a, b"), "{e}");
        let e = find_source(&cfg, "nope").unwrap_err();
        assert!(e.contains("fx, a, b"), "{e}");
        assert!(find_source(&Config::default(), "fx").unwrap_err().contains("none is"));
    }

    #[test]
    fn suggested_commands_can_be_typed_back() {
        let line = |words: &[&str], options: &[&str]| command_line("resolve", words, options).unwrap();
        assert_eq!(line(&["fx", "alpha", "keep"], &[]), "tss resolve fx alpha keep");
        assert_eq!(line(&["fx", "my skill", "keep"], &[]), "tss resolve fx \"my skill\" keep");
        assert_eq!(command_line("check", &["fx"], &["--delete-all"]).unwrap(), "tss check fx --delete-all");
        assert_eq!(command_line("check", &["-fx"], &["--delete-all"]).unwrap(), "tss check --delete-all -- -fx");
        let typed = line(&["fx", "-odd", "keep"], &[]);
        assert_eq!(typed, "tss resolve -- fx -odd keep");
        let typed: Vec<String> = typed.split(' ').skip(1).map(str::to_string).collect();
        assert_eq!(parse(&typed).unwrap(), Command::Resolve { repo: "fx".into(), skill: "-odd".into(), how: Resolution::KeepMine });
        // Quoting a word does not hide a leading '-' from the command line parser.
        assert_eq!(line(&["fx", "-odd one", "keep"], &[]), "tss resolve -- fx \"-odd one\" keep");
    }

    #[test]
    fn a_name_a_shell_could_act_on_is_never_printed_as_a_command() {
        // Windows allows every one of these in a folder name, and inside double quotes PowerShell still runs $(...) and
        // $variables, treats a backtick as an escape and ends the string at a curly double quote; cmd expands %NAME%.
        for name in [
            "a$(b)",
            "$env:USERNAME",
            "a`b",
            "x\u{201d};y;#",
            "x\u{201c};y;#",
            "x\u{201e};y;#",
            "x\u{2019};y;#",
            "x\u{2018};y;#",
            "100%",
            "a;b",
            "a&b",
            "a|b",
            "a'b",
            "a\"b",
            "a<b",
            "a(b)",
            "a,b",
            "a@b",
            "a=b",
            "a[0]",
            "a{b}",
            "a!b",
            "a^b",
            "a~b",
            "a#b",
            "a\\b",
            "a\u{2013}b",
            "a\u{202e}b",
            "a\u{1b}b",
            "a\u{ff04}b",
            "",
        ] {
            assert_eq!(command_line("resolve", &["fx", name, "keep"], &[]), None, "{name:?}");
        }
        // The repository id is a word too.
        assert_eq!(command_line("check", &["a$(b)"], &["--delete-all"]), None);
        // The CLI is ASCII. An escaped name would not select the same item if pasted.
        assert_eq!(command_line("resolve", &["fx", "caf\u{e9}", "keep"], &[]), None);
        assert_eq!(command_line("resolve", &["fx", "my skill 2", "keep"], &[]).unwrap(), "tss resolve fx \"my skill 2\" keep");
        assert_eq!(command_line("resolve", &["fx", "c++", "keep"], &[]).unwrap(), "tss resolve fx \"c++\" keep");
        assert_eq!(command_line("resolve", &["fx", "a_b-c.d", "keep"], &[]).unwrap(), "tss resolve fx a_b-c.d keep");
    }

    #[test]
    fn a_row_whose_name_is_not_safe_to_paste_points_to_the_app_window() {
        let row = |name: &str, actions: &[Action]| Row {
            name: name.into(),
            text: "Edited locally (changed: SKILL.md)".into(),
            tag: "Edited on this PC",
            level: Level::Attention,
            actions: actions.to_vec(),
            facts: None,
            listed: true,
            folder_missing: false,
            plugin: None,
        };

        let mut out = String::new();
        rows(&mut out, "fx", &[row("a$(b)", &[Action::Overwrite, Action::KeepMine])], true);
        assert!(!out.contains("tss resolve"), "{out}");
        assert!(out.contains("! a$(b): Edited locally"), "the name is still shown:\n{out}");
        assert!(out.contains("Open Tiny Skill Sync and choose: Use the GitHub version / Keep my version"), "{out}");

        // A command that does not name the skill can still be typed; the one that does cannot.
        let mut out = String::new();
        rows(&mut out, "fx", &[row("a$(b)", &[Action::RemoveThem, Action::KeepMine])], true);
        assert!(out.contains("tss check fx --delete-all   (Delete them)"), "{out}");
        assert!(!out.contains("tss resolve") && out.contains("Open Tiny Skill Sync and choose: Keep my version"), "{out}");

        let mut out = String::new();
        rows(&mut out, "fx", &[row("alpha", &[Action::Overwrite, Action::KeepMine])], true);
        assert!(out.contains("tss resolve fx alpha replace") && out.contains("tss resolve fx alpha keep") && !out.contains("Open Tiny Skill Sync"), "{out}");
    }

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(size(0), "0 bytes");
        assert_eq!(size(1023), "1023 bytes");
        assert_eq!(size(1024), "1.0 KB");
        assert_eq!(size(1536), "1.5 KB");
        assert_eq!(size(3 * 1024 * 1024), "3.0 MB");
    }

    #[test]
    fn help_is_plain_ascii() {
        assert!(HELP.is_ascii());
    }
}
