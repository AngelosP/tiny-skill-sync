//! `tss`, the command line. It drives the same engine as the tray, one command at a time, so every rule about what
//! may be changed or deleted is the same. It is a way to look and to act once (and a surface for tests and
//! measurements), not a second package manager: there is no search, no install by name and no version pinning.
//!
//! Exit codes: 0 everything is fine, 1 something needs the user's attention, 2 a check or command failed,
//! 3 the command line was not understood.

use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use crate::board::{Action, Board, Level, RepoView, Row};
use crate::config::{CONFIG_FILE, Config, Paths, RepoConfig, RepoSpec};
use crate::engine::{Access, ApplyResult, Engine, EngineError, Outcome, Resolution, SkillReport, SyncOptions, Tracking, Which};
use crate::github::{GitHubRemote, Remote, SystemTokens, TokenProvider};
use crate::interval::{Interval, now_secs};
use crate::pathsafe::{fold, printable_lines};
use crate::reconcile::Verdict;
use crate::state::State;
use crate::verify::Check;
use crate::win::{console, cred, proc};

pub const EXIT_OK: i32 = 0;
pub const EXIT_ATTENTION: i32 = 1;
pub const EXIT_FAILED: i32 = 2;
pub const EXIT_USAGE: i32 = 3;

const NO_REPOS: &str = "No repositories are set up. Add one with: tss add OWNER/NAME";

const HELP: &str = "\
tss - keeps Copilot skills in sync with GitHub repositories, one command at a time.
The tray app does the same work on a schedule; both follow the same safety rules.

Usage:
  tss status
      What is installed and what needs attention. Reads only local files.
  tss check [REPO] [--dry-run] [--full-listing] [--delete-all]
      Ask GitHub now and apply what is safe: install, update, remove.
        --dry-run       show what would change, and change nothing
        --full-listing  fetch the whole listing even if GitHub says nothing changed
        --delete-all    REPO lists no skills at all any more: delete its skills anyway
  tss resolve REPO SKILL ACTION
      Settle a skill that needs you. ACTION is one of:
        replace   take upstream's version; what is there now is kept as a backup
        restore   bring back a skill that was deleted here (same as replace)
        keep      the folder is yours: never change or remove it
        leave     a skill that was deleted here stays deleted (same as keep)
        manage    undo keep
  tss add OWNER/NAME [--every INTERVAL] [--use-token]
  tss add https://github.com/OWNER/NAME/tree/BRANCH/FOLDER [...]
      Follow a repository, or one folder of it. A private repository is read with
      the GitHub token by itself; --use-token sends it for every request.
  tss remove REPO [--delete-skills]
      Stop following a repository. Its skills stay installed and become yours,
      unless --delete-skills is given (edited skills are kept even then).
  tss token [status | set | clear]
      The GitHub token, saved in Windows Credential Manager. Without one, the
      GitHub CLI's token is used (gh auth login). 'set' reads the token without
      showing it, or from a pipe: gh auth token | tss token set
  tss help
  tss --version

REPO is the id shown by 'tss status', or OWNER/NAME when only one entry has it.
INTERVAL is a number and a unit: 30m, 6h, 1d, 2w or 1mo (at least 5 minutes).

Exit codes: 0 all fine, 1 something needs attention, 2 failed, 3 bad command line.

Environment:
  TSS_HOME        settings and state (default: %LOCALAPPDATA%\\tiny-skill-sync)
  TSS_SKILLS_DIR  the skills folder (default: from the settings, else
                  %USERPROFILE%\\.copilot\\skills)
";

/// Everything the commands need from outside, so tests can run them against a fake GitHub and a fake credential
/// store.
pub trait Host {
    /// Where the settings and the state are kept.
    fn home(&self) -> PathBuf;
    fn skills_dir(&self, cfg: &Config) -> PathBuf;
    fn remote(&self) -> Result<Box<dyn Remote>, String>;
    fn tokens(&self) -> Box<dyn TokenProvider>;
    fn token_saved(&self) -> bool;
    fn save_token(&self, token: &str) -> Result<(), String>;
    /// True when a token was saved and is now gone.
    fn delete_token(&self) -> Result<bool, String>;
    /// Whether the GitHub CLI would hand out a token, and if not, why.
    fn gh_signed_in(&self) -> Result<(), String>;
    /// A token typed at the console (not shown) or piped in.
    fn read_token(&self) -> Result<String, String>;
}

/// The real computer.
pub struct SystemHost;

impl Host for SystemHost {
    fn home(&self) -> PathBuf {
        Paths::home_dir()
    }

    fn skills_dir(&self, cfg: &Config) -> PathBuf {
        Paths::skills_dir(cfg)
    }

    fn remote(&self) -> Result<Box<dyn Remote>, String> {
        Ok(Box::new(GitHubRemote::from_env()?))
    }

    fn tokens(&self) -> Box<dyn TokenProvider> {
        Box::new(SystemTokens)
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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Help,
    Version,
    Status,
    Check { repo: Option<String>, dry_run: bool, full_listing: bool, delete_all: bool },
    Resolve { repo: String, skill: String, how: Resolution },
    Add { spec: String, every: Option<Interval>, use_token: bool },
    Remove { repo: String, delete_skills: bool },
    Token(TokenAction),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenAction {
    Status,
    Set,
    Clear,
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
        "resolve" => "tss resolve REPO SKILL replace|restore|keep|leave|manage",
        "add" => "tss add OWNER/NAME [--every INTERVAL] [--use-token]",
        "remove" => "tss remove REPO [--delete-skills]",
        "token" => "tss token [status|set|clear]",
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
        _ => Err(format!("'{word}' is not something resolve can do; use replace, restore, keep, leave or manage")),
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
        Action::RemoveThem => "",
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
        "check" => {
            expect_words(&name, words, 0, 1)?;
            let repo = words.first().cloned();
            let (dry_run, full_listing, delete_all) = (flags.switch("dry-run")?, flags.switch("full-listing")?, flags.switch("delete-all")?);
            if delete_all && repo.is_none() {
                return Err("--delete-all is for one repository: tss check REPO --delete-all".to_string());
            }
            Command::Check { repo, dry_run, full_listing, delete_all }
        }
        "resolve" => {
            expect_words(&name, words, 3, 3)?;
            Command::Resolve { repo: words[0].clone(), skill: words[1].clone(), how: resolution(&words[2])? }
        }
        "add" => {
            expect_words(&name, words, 1, 1)?;
            let every = match flags.value("every") {
                Some(text) => Some(Interval::parse(&text).and_then(Interval::validate_poll).map_err(|e| format!("--every: {e}"))?),
                None => None,
            };
            Command::Add { spec: words[0].clone(), every, use_token: flags.switch("use-token")? }
        }
        "remove" => {
            expect_words(&name, words, 1, 1)?;
            Command::Remove { repo: words[0].clone(), delete_skills: flags.switch("delete-skills")? }
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
        _ => return Err(format!("unknown command '{first}'")),
    };
    flags.finish(&name)?;
    Ok(command)
}

/// Finds a repository by its id, or by `owner/name` when only one entry has it. Case does not matter.
pub fn find_repo<'a>(cfg: &'a Config, query: &str) -> Result<&'a RepoConfig, String> {
    if let Some(repo) = cfg.find(query) {
        return Ok(repo);
    }
    let wanted = query.trim().trim_end_matches('/');
    let hits: Vec<&RepoConfig> = cfg.repos.iter().filter(|r| r.id.eq_ignore_ascii_case(wanted) || r.repo.eq_ignore_ascii_case(wanted)).collect();
    let ids = |repos: &mut dyn Iterator<Item = &RepoConfig>| repos.map(|r| r.id.as_str()).collect::<Vec<_>>().join(", ");
    match hits.as_slice() {
        [one] => Ok(one),
        [] if cfg.repos.is_empty() => Err(format!("no repository '{query}' is set up; none is (add one with: tss add OWNER/NAME)")),
        [] => Err(format!("no repository '{query}' is set up; the ids are: {}", ids(&mut cfg.repos.iter()))),
        many => Err(format!("'{query}' matches several repositories ({}); use the id", ids(&mut many.iter().copied()))),
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

/// Runs one command. What it prints goes to `out`, problems go to `err`; the result is the exit code.
///
/// Everything printed passes through `printable_lines`: names and messages come from the repository, and a control
/// character in them could move the cursor or erase the line above.
pub fn run(args: &[String], host: &dyn Host, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let command = match parse(args) {
        Ok(command) => command,
        Err(e) => {
            let _ = writeln!(err, "tss: {}\nRun 'tss help' to see the commands.", printable_lines(&e));
            return EXIT_USAGE;
        }
    };
    let mut text = String::new();
    let result = execute(command, host, &mut text);
    // Output that cannot be written (a closed pipe) is not a reason to fail the command.
    let _ = out.write_all(printable_lines(&text).as_bytes()).and_then(|()| out.flush());
    match result {
        Ok(code) => code,
        Err(Fail::Usage(e)) => {
            let _ = writeln!(err, "tss: {}", printable_lines(&e));
            EXIT_USAGE
        }
        Err(Fail::Failed(e)) => {
            let _ = writeln!(err, "tss: {}", printable_lines(&e));
            EXIT_FAILED
        }
    }
}

fn execute(command: Command, host: &dyn Host, out: &mut String) -> Result<i32, Fail> {
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
        Command::Check { repo, dry_run, full_listing, delete_all } => check(host, out, repo.as_deref(), dry_run, full_listing, delete_all),
        Command::Resolve { repo, skill, how } => resolve(host, out, &repo, &skill, how),
        Command::Add { spec, every, use_token } => add(host, out, &spec, every, use_token),
        Command::Remove { repo, delete_skills } => remove(host, out, &repo, delete_skills),
        Command::Token(action) => token(host, out, action),
    }
}

fn config_path(host: &dyn Host) -> PathBuf {
    host.home().join(CONFIG_FILE)
}

fn load_config(host: &dyn Host) -> Result<Config, Fail> {
    Config::load(&config_path(host)).map_err(Fail::Failed)
}

fn paths(host: &dyn Host, cfg: &Config) -> Paths {
    Paths::new(host.home(), host.skills_dir(cfg))
}

fn open_engine(host: &dyn Host, cfg: &Config) -> Result<Engine, Fail> {
    let remote = host.remote().map_err(Fail::Failed)?;
    Ok(Engine::new(paths(host, cfg), remote, host.tokens()))
}

/// What is recorded, read without the engine's lock and without changing anything. Only used to check names
/// typed on the command line; the engine reads it again under its lock before acting.
fn load_state(host: &dyn Host, cfg: &Config) -> Result<State, Fail> {
    let file = paths(host, cfg).state_file();
    State::load_with(&file, false).map(|loaded| loaded.state).map_err(|e| Fail::Failed(format!("cannot read {}: {e}", file.display())))
}

/// The board judges a disabled repository as "Disabled" and nothing else. Here the user asked about it, so it is
/// judged like any other and marked as disabled in the heading.
fn all_enabled(cfg: &Config) -> Config {
    let mut shown = cfg.clone();
    for repo in &mut shown.repos {
        repo.enabled = true;
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
    if word.is_empty() || !word.chars().all(|c| plain(c) || c.is_alphanumeric() || c == ' ' || c == '+') {
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
        let command = match action.resolution() {
            Some(_) => command_line("resolve", &[repo_id, &row.name, word_for(action)], &[]),
            None => command_line("check", &[repo_id], &["--delete-all"]),
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
    say!(out, "{} [{}]{off}: {}", view.label, view.id, view.status);
    if let Some(detail) = &view.detail {
        say!(out, "  {detail}");
    }
}

/// Rows that need the user are marked with '!' and followed by the commands that settle them.
fn rows(out: &mut String, repo_id: &str, rows: &[Row], all: bool) {
    for row in rows {
        if !all && row.level == Level::Ok {
            continue;
        }
        let mark = if row.level == Level::Ok { ' ' } else { '!' };
        say!(out, "  {mark} {}: {}", row.name, row.text);
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
        say!(out, "{kind}: {}", message.text);
    }
}

fn schedule_text(cfg: &Config, repo: &RepoConfig, sends_token: bool) -> String {
    let mut text = match (repo.enabled, repo.interval) {
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
    let cfg = load_config(host)?;
    say!(out, "Skills folder: {}", host.skills_dir(&cfg).display());
    say!(out, "Settings: {}", config_path(host).display());
    let mut level = Level::Ok;
    for problem in cfg.problems() {
        say!(out, "Settings problem: {problem}");
        level = Level::Attention;
    }

    let mut engine = open_engine(host, &cfg)?;
    let report = engine.verify(&cfg, Access::ReadOnly)?;
    let now = now_secs();
    let mut board = Board::new();
    board.note_verify(&report, now);
    for repo in &cfg.repos {
        if let Some(at) = engine.last_check(&repo.id) {
            board.note_last_check(&repo.id, at);
        }
        board.note_kept(&repo.id, &engine.kept(&repo.id));
        board.note_lists_nothing(&repo.id, engine.lists_nothing(&repo.id));
    }

    if cfg.repos.is_empty() {
        gap(out);
        say!(out, "{NO_REPOS}");
    }
    for (view, repo) in board.views(&all_enabled(&cfg), now).iter().zip(&cfg.repos) {
        gap(out);
        header(out, view, repo);
        say!(out, "  Last check: {}. {}", view.last_check_text(now), schedule_text(&cfg, repo, repo.use_token || engine.needs_token(&repo.id)));
        if view.skills.is_empty() {
            say!(out, "  No skills from it are installed.");
        }
        rows(out, &view.id, &view.skills, true);
        if repo.enabled {
            level = level.max(view.level);
        }
    }
    for id in board.orphans() {
        gap(out);
        say!(out, "Skills recorded for '{id}', which is no longer in the settings, are left alone.");
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
    let cfg = load_config(host)?;
    let which = match query {
        Some(query) => Which::One(find_repo(&cfg, query).map_err(Fail::Usage)?.id.clone()),
        None if cfg.repos.iter().any(|r| r.enabled) => Which::All,
        None if cfg.repos.is_empty() => {
            say!(out, "{NO_REPOS}");
            return Ok(EXIT_OK);
        }
        None => {
            say!(out, "Every repository is disabled. Check one anyway with: tss check REPO");
            return Ok(EXIT_OK);
        }
    };

    let mut engine = open_engine(host, &cfg)?;
    let now = now_secs();
    let mut board = Board::new();
    // The look at the disk comes first, so local edits are reported even when GitHub says nothing changed.
    let verified = engine.verify(&cfg, if dry_run { Access::ReadOnly } else { Access::Write })?;
    board.note_verify(&verified, now);
    // What the last listing had in it, for a check that GitHub answers with "not modified".
    for repo in &cfg.repos {
        board.note_lists_nothing(&repo.id, engine.lists_nothing(&repo.id));
    }
    let opts = SyncOptions { dry_run, ignore_etag: full_listing, confirm_mass_delete: delete_all, ..SyncOptions::default() };
    let mut report = engine.sync(&cfg, &which, &opts, &AtomicBool::new(false))?;
    if !dry_run {
        if let Err(e) = engine.flush() {
            report.warnings.push(format!("the time of this check was not saved: {e}"));
        }
    }
    board.note_sync(&report, now);

    let views = board.views(&all_enabled(&cfg), now);
    let mut level = Level::Ok;
    for repo_report in &report.repos {
        let (Some(view), Some(repo)) = (views.iter().find(|v| v.id == repo_report.repo_id), cfg.find(&repo_report.repo_id)) else { continue };
        gap(out);
        header(out, view, repo);
        if matches!(repo_report.outcome, Outcome::Failed(_)) {
            say!(out, "  Nothing is deleted because a check failed.");
        }
        for skill in &repo_report.skills {
            if let Some(line) = change(skill) {
                say!(out, "  {line}");
            }
        }
        rows(out, &view.id, &view.skills, false);
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
    let cfg = load_config(host)?;
    let repo = find_repo(&cfg, query).map_err(Fail::Usage)?;
    // The engine checks `replace` against upstream itself. The other two only change a list, so a typo would be
    // recorded as it is: the name is checked here.
    let name = match how {
        Resolution::Overwrite => skill.to_string(),
        Resolution::KeepMine => installed_name(host, &cfg, repo, skill)?,
        Resolution::ManageAgain => kept_name(host, &cfg, repo, skill)?,
    };
    let mut engine = open_engine(host, &cfg)?;
    let report = engine.resolve(&cfg, &repo.id, &name, how, &AtomicBool::new(false))?;
    say!(out, "{}.", report.message);
    for backup in &report.backups {
        say!(out, "Backup: {}", backup.display());
    }
    Ok(EXIT_OK)
}

/// The skill as it is recorded for the repository, or a folder of that name in the skills folder.
fn installed_name(host: &dyn Host, cfg: &Config, repo: &RepoConfig, skill: &str) -> Result<String, Fail> {
    let state = load_state(host, cfg)?;
    let key = fold(skill);
    if let Some(owner) = state.names_owned_elsewhere(&repo.id).get(&key) {
        let instead =
            command_line("resolve", &[owner, skill, "keep"], &[]).unwrap_or_else(|| format!("the same command with '{owner}' in place of '{}'", repo.id));
        return Err(Fail::Usage(format!("'{skill}' is managed for '{owner}', not '{}'; use: {instead}", repo.id)));
    }
    if let Some(owned) = state.repo(&repo.id).and_then(|r| r.skill(skill)) {
        return Ok(owned.name.clone());
    }
    let dir = host.skills_dir(cfg);
    let folder = fs::read_dir(&dir).ok().and_then(|entries| {
        entries
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .find(|name| fold(name) == key)
    });
    folder.ok_or_else(|| Fail::Usage(format!("'{skill}' is neither installed from {} nor a folder in {}", repo.id, dir.display())))
}

fn kept_name(host: &dyn Host, cfg: &Config, repo: &RepoConfig, skill: &str) -> Result<String, Fail> {
    let state = load_state(host, cfg)?;
    let kept = state.repo(&repo.id).map(|r| r.ignored.clone()).unwrap_or_default();
    let key = fold(skill);
    kept.iter().find(|name| fold(name) == key).cloned().ok_or_else(|| {
        let which = if kept.is_empty() { "none is".to_string() } else { format!("these are: {}", kept.join(", ")) };
        Fail::Usage(format!("'{skill}' is not kept as yours in {}; {which}", repo.id))
    })
}

fn add(host: &dyn Host, out: &mut String, spec: &str, every: Option<Interval>, use_token: bool) -> Result<i32, Fail> {
    let spec = RepoSpec::parse(spec).map_err(Fail::Usage)?;
    let file = config_path(host);
    let mut cfg = Config::load(&file).map_err(Fail::Failed)?;
    let full_name = spec.full_name();
    if let Some(existing) = cfg.repos.iter().find(|r| r.repo.eq_ignore_ascii_case(&full_name) && r.branch == spec.branch && r.path == spec.path) {
        return Err(Fail::Usage(format!("{} is already set up, as '{}'", existing.label(), existing.id)));
    }
    let added = cfg.add_repo(&spec, every, use_token).clone();
    cfg.save(&file).map_err(Fail::Failed)?;
    say!(out, "Added {} as '{}'. {}", added.label(), added.id, schedule_text(&cfg, &added, added.use_token));
    say!(out, "Install its skills now with: {}", command_line("check", &[&added.id], &[]).unwrap_or_else(|| "tss check".to_string()));
    Ok(EXIT_OK)
}

fn remove(host: &dyn Host, out: &mut String, query: &str, delete_skills: bool) -> Result<i32, Fail> {
    let file = config_path(host);
    let mut cfg = Config::load(&file).map_err(Fail::Failed)?;
    // A repository that left the settings while its skills stayed on record can be let go of too.
    let (id, in_settings) = match find_repo(&cfg, query) {
        Ok(repo) => (repo.id.clone(), true),
        Err(e) => match load_state(host, &cfg)?.repo(query) {
            Some(r) if !r.skills.is_empty() => (query.to_string(), false),
            _ => return Err(Fail::Usage(e)),
        },
    };
    let mut engine = open_engine(host, &cfg)?;
    let report = engine.remove_repo(&id, delete_skills)?;

    let mut saved = Ok(());
    if in_settings {
        let label = cfg.find(&id).map(RepoConfig::label).unwrap_or_default();
        cfg.repos.retain(|r| r.id != id);
        saved = cfg.save(&file);
        if saved.is_ok() {
            say!(out, "Removed {label} [{id}] from the settings.");
        }
    } else {
        say!(out, "Stopped tracking the skills recorded for '{id}'.");
    }
    if !report.deleted.is_empty() {
        say!(out, "Deleted: {}", report.deleted.join(", "));
    }
    if delete_skills {
        for (name, why) in &report.kept {
            say!(out, "Kept {name}, which is now yours: {why}");
        }
    } else if !report.kept.is_empty() {
        let names: Vec<&str> = report.kept.iter().map(|(name, _)| name.as_str()).collect();
        say!(out, "These skills stay installed and are now yours: {}", names.join(", "));
    }
    if let Err(e) = saved {
        return Err(Fail::Failed(format!(
            "{e}. The skills of '{id}' are no longer managed, but it is still in the settings: remove it from {} by hand",
            file.display()
        )));
    }
    Ok(if delete_skills && !report.kept.is_empty() { EXIT_ATTENTION } else { EXIT_OK })
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
            let cfg = load_config(host)?;
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
            let state = load_state(host, &cfg).unwrap_or_default();
            let sends = |r: &RepoConfig| r.use_token || state.repo(&r.id).is_some_and(|s| s.needs_token);
            let users: Vec<&str> = cfg.repos.iter().filter(|r| sends(r)).map(|r| r.id.as_str()).collect();
            if users.is_empty() {
                say!(out, "No repository sends the token. It is tried by itself when GitHub cannot find a repository without it.");
            } else {
                say!(out, "Repositories that send the token: {}", users.join(", "));
            }
            if !available && cfg.repos.iter().any(|r| sends(r) && r.enabled) {
                say!(out, "They cannot be checked until a token is saved (tss token set) or the GitHub CLI is signed in (gh auth login).");
                return Ok(EXIT_ATTENTION);
            }
            Ok(EXIT_OK)
        }
    }
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
    fn commands_and_options_are_understood() {
        assert_eq!(parsed("status"), Command::Status);
        assert_eq!(parsed("STATUS"), Command::Status);
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
        assert_eq!(parsed("add o/r"), Command::Add { spec: "o/r".into(), every: None, use_token: false });
        let every = Some(Interval::new(6, Unit::Hour));
        assert_eq!(parsed("add o/r --every 6h --use-token"), Command::Add { spec: "o/r".into(), every, use_token: true });
        assert_eq!(parsed("add --every=6h o/r"), Command::Add { spec: "o/r".into(), every, use_token: false });
        assert_eq!(parsed("remove fx"), Command::Remove { repo: "fx".into(), delete_skills: false });
        assert_eq!(parsed("remove fx --delete-skills"), Command::Remove { repo: "fx".into(), delete_skills: true });
        assert_eq!(parsed("token"), Command::Token(TokenAction::Status));
        assert_eq!(parsed("token set"), Command::Token(TokenAction::Set));
        assert_eq!(parsed("token CLEAR"), Command::Token(TokenAction::Clear));
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
        assert!(refused("sync").contains("unknown command 'sync'"));
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
        assert!(refused("remove").contains("something is missing"));
        assert!(refused("token show").contains("'show' is not a token action"));
    }

    fn cfg_with(repos: &[(&str, &str, &str)]) -> Config {
        let repos = repos
            .iter()
            .map(|(id, repo, path)| RepoConfig { id: id.to_string(), repo: repo.to_string(), path: path.to_string(), ..RepoConfig::default() })
            .collect();
        Config { repos, ..Config::default() }
    }

    #[test]
    fn repositories_are_found_by_id_or_unique_name() {
        let cfg = cfg_with(&[("fx", "Owner/Fixtures", "skills"), ("a", "o/twice", "x"), ("b", "o/twice", "y")]);
        assert_eq!(find_repo(&cfg, "fx").unwrap().id, "fx");
        assert_eq!(find_repo(&cfg, "FX").unwrap().id, "fx");
        assert_eq!(find_repo(&cfg, "owner/fixtures").unwrap().id, "fx");
        assert_eq!(find_repo(&cfg, "owner/fixtures/").unwrap().id, "fx");
        let e = find_repo(&cfg, "o/twice").unwrap_err();
        assert!(e.contains("matches several") && e.contains("a, b"), "{e}");
        let e = find_repo(&cfg, "nope").unwrap_err();
        assert!(e.contains("fx, a, b"), "{e}");
        assert!(find_repo(&Config::default(), "fx").unwrap_err().contains("none is"));
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
        // Letters, digits and spaces from any language are fine; a word is quoted when it is not plain ASCII.
        assert_eq!(command_line("resolve", &["fx", "caf\u{e9}", "keep"], &[]).unwrap(), "tss resolve fx \"caf\u{e9}\" keep");
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
