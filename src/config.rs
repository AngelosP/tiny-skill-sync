//! What the user configures: repositories, where skills are installed, and app update preferences.
//! `config.json` is meant to be readable and hand-editable.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use windows_sys::Win32::Foundation::GENERIC_WRITE;
use windows_sys::Win32::Storage::FileSystem::{DELETE, FILE_FLAG_DELETE_ON_CLOSE, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ};

use crate::fsx;
use crate::interval::{Interval, Unit};
use crate::pathsafe;
use crate::source::SourceKind;

pub const CONFIG_FILE: &str = "config.json";
pub const CONFIG_VERSION: u32 = 2;
pub const DEFAULT_INTERVAL: Interval = Interval::new(1, Unit::Day);

/// Hold through re-read, decision, and replacement. The directory must exist. Cooperating writers must see the
/// *same filesystem directory*; independently synchronized cloud replicas do not share a lock. Readers need no lock.
pub(crate) fn settings_write_lock(directory: &Path) -> Result<File, String> {
    let path = directory.join(".tss-settings.lock");
    // CREATE_NEW never takes over an existing entry. Delete-on-close removes only the file this handle created,
    // including on process exit; unlinking a lock by name could remove somebody else's lock.
    OpenOptions::new()
        .write(true)
        .access_mode(GENERIC_WRITE | DELETE)
        .create_new(true)
        .share_mode(0)
        .custom_flags(FILE_FLAG_DELETE_ON_CLOSE | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&path)
        .map_err(|e| format!("cannot lock settings at {} (another writer may be busy); try again: {e}", path.display()))
}

fn local_settings_lock(path: &Path) -> Result<File, String> {
    let dir = path.parent().ok_or_else(|| format!("{} has no settings folder", path.display()))?;
    fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    settings_write_lock(dir)
}

pub(crate) fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, String> {
    // A read of a dangling link can say NotFound; only a missing directory entry selects defaults/local mode.
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot check {}: {e}", path.display())),
        Ok(_) => fs::read(path).map(Some).map_err(|e| format!("cannot read {}: {e}", path.display())),
    }
}

pub(crate) fn write_json_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = path.with_file_name(format!("{name}.{}.tmp", fsx::unique_suffix()));
    write_with_temp(path, bytes, &tmp)
}

fn write_with_temp(path: &Path, bytes: &[u8], tmp: &Path) -> io::Result<()> {
    // A colliding name is not ours: neither open it for overwrite nor run failure cleanup on it.
    let mut f = OpenOptions::new()
        .write(true)
        .access_mode(GENERIC_WRITE | DELETE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .create_new(true)
        .open(tmp)?;
    let result = f.write_all(bytes).and_then(|()| f.sync_all()).and_then(|()| fsx::rename_retry(tmp, path));
    if result.is_err() {
        let _ = crate::win::install::delete(&f);
    }
    result
}

// Hand-edited JSON may have a UTF-8 BOM. Require an object: Serde also accepts arrays for structs, and e.g. `[]`
// must not select enabled defaults as if a settings file were missing.
pub(crate) fn json_object_bytes(bytes: &[u8]) -> Option<&[u8]> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    bytes.trim_ascii_start().starts_with(b"{").then_some(bytes)
}

pub(crate) fn parse_json_object<'de, T: Deserialize<'de>>(bytes: &'de [u8]) -> Result<T, String> {
    let bytes = json_object_bytes(bytes).ok_or_else(|| "expected a JSON object".to_owned())?;
    serde_json::from_slice(bytes).map_err(|e| e.to_string())
}

pub(crate) const fn legacy_version() -> u32 {
    1
}

/// Read no payload fields until we know which format they use.
#[derive(Deserialize)]
pub(crate) struct Format {
    #[serde(default = "legacy_version")]
    pub version: u32,
}

pub(crate) fn source_version(repos: &[RepoConfig]) -> u32 {
    if repos.iter().all(|r| r.kind.is_skills()) { 1 } else { 2 }
}

/// `app_update` in both local and portable settings. Omitted members mean enabled, daily and immediate.
/// An entirely default object is not written, so legacy version 1 readers can still use those files.
#[derive(Copy, Clone, Eq, PartialEq, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UpdatePrefs {
    pub enabled: bool,
    #[serde(deserialize_with = "read_choice")]
    pub interval: UpdateInterval,
    #[serde(deserialize_with = "read_choice")]
    pub timing: UpdateTiming,
}

impl Default for UpdatePrefs {
    fn default() -> Self {
        Self { enabled: true, interval: UpdateInterval::Daily, timing: UpdateTiming::Immediate }
    }
}

impl UpdatePrefs {
    pub(crate) fn is_default(&self) -> bool {
        *self == Self::default()
    }

    // Serde normally accepts positional arrays for structs; in particular `[]` would silently turn updates on.
    pub(crate) fn read_object<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Object;
        impl<'de> serde::de::Visitor<'de> for Object {
            type Value = UpdatePrefs;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an app_update settings object")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(self, map: A) -> Result<UpdatePrefs, A::Error> {
                UpdatePrefs::deserialize(serde::de::value::MapAccessDeserializer::new(map))
            }
        }
        deserializer.deserialize_map(Object)
    }
}

// Only the documented strings are choices, not Serde's alternate externally tagged enum objects.
fn read_choice<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(deserializer: D) -> Result<T, D::Error> {
    T::deserialize(serde::de::value::StringDeserializer::new(String::deserialize(deserializer)?))
}

/// JSON: `"daily"`, `"weekly"` or `"monthly"`. These are fixed durations, unlike a repository's calendar month.
#[derive(Copy, Clone, Eq, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateInterval {
    Daily,
    Weekly,
    Monthly,
}

impl UpdateInterval {
    pub const fn seconds(self) -> i64 {
        let days = match self {
            Self::Daily => 1,
            Self::Weekly => 7,
            Self::Monthly => 30,
        };
        days * 86_400
    }
}

/// JSON: `"immediate"` or `"on_quit"`. This does not change how often releases are checked.
#[derive(Copy, Clone, Eq, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateTiming {
    Immediate,
    OnQuit,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, remote = "Self")]
pub struct Config {
    /// Where skills are installed. Empty means `%USERPROFILE%\.copilot\skills`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills_dir: Option<String>,
    pub default_interval: Interval,
    #[serde(deserialize_with = "UpdatePrefs::read_object", skip_serializing_if = "UpdatePrefs::is_default")]
    pub app_update: UpdatePrefs,
    pub repos: Vec<RepoConfig>,
}

impl Serialize for Config {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // The format is a property of the payload, not a mutable setting that can get out of sync.
        #[derive(Serialize)]
        struct Wire<'a> {
            version: u32,
            #[serde(flatten, serialize_with = "Config::serialize")]
            settings: &'a Config,
        }
        Wire { version: self.wire_version(), settings: self }.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Config {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Config::deserialize(deserializer)
    }
}

impl Default for Config {
    fn default() -> Self {
        Config { skills_dir: None, default_interval: DEFAULT_INTERVAL, app_update: UpdatePrefs::default(), repos: Vec::new() }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RepoConfig {
    /// Stable key for this entry. Ownership of installed skills is recorded under it, so it never changes.
    pub id: String,
    /// `owner/name`.
    pub repo: String,
    /// Empty means the default branch.
    pub branch: String,
    /// The source folder for this kind. Empty means the repository root.
    pub path: String,
    /// Empty means the default interval.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval: Option<Interval>,
    /// Send the user's GitHub token. Needed for private repositories; also raises the rate limit.
    pub use_token: bool,
    pub enabled: bool,
    #[serde(skip_serializing_if = "SourceKind::is_skills")]
    pub kind: SourceKind,
}

impl Default for RepoConfig {
    fn default() -> Self {
        RepoConfig {
            id: String::new(),
            repo: String::new(),
            branch: String::new(),
            path: String::new(),
            interval: None,
            use_token: false,
            enabled: true,
            kind: SourceKind::Skills,
        }
    }
}

impl RepoConfig {
    pub fn effective_interval(&self, default: Interval) -> Interval {
        self.interval.unwrap_or(default)
    }

    pub fn owner_and_name(&self) -> Option<(&str, &str)> {
        let (owner, name) = self.repo.split_once('/')?;
        Some((owner, name))
    }

    /// Short text for lists and messages: `owner/name`, plus the branch and folder when they are set.
    pub fn label(&self) -> String {
        let mut s = self.repo.clone();
        if !self.branch.is_empty() {
            s.push('@');
            s.push_str(&self.branch);
        }
        if !self.path.is_empty() {
            s.push_str(" : ");
            s.push_str(&self.path);
        }
        s
    }

    /// The name used for a skill when the configured folder is itself one skill.
    pub fn single_skill_name(&self) -> String {
        match self.path.rsplit('/').next().filter(|s| !s.is_empty()) {
            Some(last) => last.to_string(),
            None => self.owner_and_name().map(|(_, n)| n.to_string()).unwrap_or_default(),
        }
    }

    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        match RepoSpec::parse(&self.repo) {
            Ok(spec) if spec.branch.is_empty() && spec.path.is_empty() && spec.full_name() == self.repo => {}
            Ok(_) | Err(_) => out.push(format!("'{}' is not a repository name like owner/name", self.repo)),
        }
        let plugin_branch_problem = if self.kind != SourceKind::Plugins {
            None
        } else if self.branch.is_empty() || self.branch == "HEAD" {
            Some("Plugins need a named branch, for example https://github.com/OWNER/NAME/tree/main.")
        } else {
            check_plugin_branch_chars(&self.branch).err()
        };
        if let Some(why) = plugin_branch_problem {
            out.push(why.into());
        } else if !self.branch.is_empty() {
            if let Err(e) = check_branch(&self.branch) {
                out.push(format!("branch '{}': {e}", self.branch));
            }
        }
        match pathsafe::normalize_repo_dir(&self.path) {
            Ok(n) if n == self.path => {}
            Ok(_) => out.push(format!("folder '{}' should be written like skills/sub, without a leading or trailing slash", self.path)),
            Err(e) => out.push(format!("folder '{}': {e}", self.path)),
        }
        if let Some(i) = self.interval {
            if let Err(e) = i.validate_poll() {
                out.push(e);
            }
        }
        out
    }
}

impl Config {
    pub fn wire_version(&self) -> u32 {
        source_version(&self.repos)
    }

    pub fn parse(bytes: &[u8]) -> Result<Config, String> {
        let invalid = |e| format!("config.json is not valid: {e}");
        let Format { version } = parse_json_object(bytes).map_err(invalid)?;
        if version > CONFIG_VERSION {
            return Err(format!("config.json was written by a newer version (format {version}, this app understands {CONFIG_VERSION})"));
        }
        let mut cfg: Config = parse_json_object(bytes).map_err(invalid)?;
        if version < 2 && cfg.wire_version() == 2 {
            return Err("typed sources require config.json format 2".into());
        }
        // A relative folder would depend on the folder the app is started in, which at sign-in is Windows' own.
        if let Some(dir) = cfg.skills_dir.as_deref().map(str::trim).filter(|d| !d.is_empty() && !Path::new(d).is_absolute()) {
            return Err(format!("the skills folder in {CONFIG_FILE} must be a full path, such as D:\\skills; '{dir}' is not one"));
        }
        cfg.assign_missing_ids();
        cfg.check_typed_ids()?;
        Ok(cfg)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("config serializes") + "\n"
    }

    /// A missing file is an empty configuration; a file that cannot be read or understood is an error, so it is never
    /// overwritten by mistake.
    pub fn load(path: &Path) -> Result<Config, String> {
        Self::load_sources(path)?.skills_only()
    }

    /// Settings model for typed callers. The existing app/CLI must not dispatch these IDs as skills.
    pub fn load_sources(path: &Path) -> Result<Config, String> {
        match read_optional(path)? {
            Some(bytes) => Config::parse(&bytes).map_err(|e| format!("{e} ({})", path.display())),
            None => Ok(Config::default()),
        }
    }

    pub(crate) fn skills_only(self) -> Result<Self, String> {
        self.require_skills()?;
        Ok(self)
    }

    pub(crate) fn require_skills(&self) -> Result<(), String> {
        match self.repos.iter().find(|r| !r.kind.is_skills()) {
            Some(r) => Err(format!("source '{}' is not a skills source; this version cannot sync its kind", r.id)),
            None => Ok(()),
        }
    }

    /// Explicit replacement of a readable config. Use `save_if_unchanged` for an edit based on a snapshot.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let _lock = local_settings_lock(path)?;
        Config::load_sources(path)?;
        self.write(path)
    }

    /// For an edit based on a snapshot. As with portable saves, compare before replacing the file, not just
    /// when the settings window opens.
    pub fn save_if_unchanged(&self, path: &Path, before: &Config) -> Result<(), String> {
        let _lock = (self != before).then(|| local_settings_lock(path)).transpose()?;
        if Config::load_sources(path)? != *before {
            return Err(format!("{CONFIG_FILE} changed since it was loaded ({})", path.display()));
        }
        if self == before { Ok(()) } else { self.write(path) }
    }

    fn write(&self, path: &Path) -> Result<(), String> {
        self.check_typed_ids()?;
        write_json_atomic(path, self.to_json().as_bytes()).map_err(|e| format!("cannot save {}: {e}", path.display()))
    }

    pub(crate) fn check_typed_ids(&self) -> Result<(), String> {
        // Legacy settings still report duplicates through `problems`. Typed ownership cannot be
        // keyed by an ambiguous ID. Missing IDs are assigned by the unchanged legacy algorithm.
        if self.wire_version() > 1 {
            let mut ids = std::collections::HashSet::new();
            for r in &self.repos {
                if !r.id.trim().is_empty() && !ids.insert(&r.id) {
                    return Err(format!("two sources share the id '{}'", r.id));
                }
            }
        }
        Ok(())
    }

    /// Entries written by hand may lack an id. The id is derived from the repository name, so it is the same on every load.
    pub fn assign_missing_ids(&mut self) {
        for i in 0..self.repos.len() {
            if self.repos[i].id.trim().is_empty() {
                let id = self.unique_id(&self.repos[i].repo.clone());
                self.repos[i].id = id;
            }
        }
    }

    fn unique_id(&self, repo: &str) -> String {
        let base: String = repo.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '-' }).collect();
        let base = if base.is_empty() { "repo".to_string() } else { base };
        let mut candidate = base.clone();
        let mut n = 2;
        while self.repos.iter().any(|r| r.id == candidate) {
            candidate = format!("{base}-{n}");
            n += 1;
        }
        candidate
    }

    pub fn add_repo(&mut self, spec: &RepoSpec, interval: Option<Interval>, use_token: bool) -> &RepoConfig {
        self.add_source(SourceKind::Skills, spec, interval, use_token)
    }

    pub fn add_source(&mut self, kind: SourceKind, spec: &RepoSpec, interval: Option<Interval>, use_token: bool) -> &RepoConfig {
        let id = self.unique_id(&spec.full_name());
        self.repos.push(RepoConfig {
            id,
            kind,
            repo: spec.full_name(),
            branch: spec.branch.clone(),
            path: spec.path.clone(),
            interval,
            use_token,
            enabled: true,
        });
        self.repos.last().expect("just pushed")
    }

    pub fn find(&self, id: &str) -> Option<&RepoConfig> {
        self.repos.iter().find(|r| r.id == id)
    }

    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Err(e) = self.default_interval.validate_poll() {
            out.push(format!("default interval: {e}"));
        }
        for (i, r) in self.repos.iter().enumerate() {
            if self.repos[..i].iter().any(|o| o.id == r.id) {
                out.push(format!("two repositories share the id '{}'", r.id));
            }
            out.extend(r.problems().into_iter().map(|p| format!("{}: {p}", r.id)));
        }
        out
    }
}

/// A repository as a person types it: `owner/name`, or a pasted GitHub address that may name a branch and folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoSpec {
    pub owner: String,
    pub name: String,
    pub branch: String,
    pub path: String,
}

impl RepoSpec {
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    pub fn parse(input: &str) -> Result<RepoSpec, String> {
        Self::parse_for_kind(input, SourceKind::Skills)
    }

    pub(crate) fn parse_for_kind(input: &str, kind: SourceKind) -> Result<RepoSpec, String> {
        let text = input.trim();
        if text.is_empty() {
            return Err("enter a repository like owner/name".to_string());
        }
        let mut rest = text;
        let mut from_url = false;
        for prefix in ["https://", "http://"] {
            if let Some(r) = text.strip_prefix(prefix) {
                rest = r;
                from_url = true;
            }
        }
        if let Some(r) = rest.strip_prefix("www.") {
            rest = r;
        }
        if let Some(r) = rest.strip_prefix("github.com/") {
            rest = r;
            from_url = true;
        } else if from_url {
            return Err("only github.com repositories are supported".to_string());
        }
        let rest = rest.split(['?', '#']).next().unwrap_or(rest);
        let parts: Vec<&str> = rest.trim_matches('/').split('/').collect();
        if parts.len() < 2 {
            return Err("enter a repository like owner/name".to_string());
        }
        let owner = parts[0];
        let name = parts[1].strip_suffix(".git").unwrap_or(parts[1]);
        check_owner(owner)?;
        check_repo_name(name)?;
        let (mut branch, mut path) = (String::new(), String::new());
        if parts.len() > 2 {
            if !from_url {
                return Err("use owner/name, or paste the address of the folder on github.com".to_string());
            }
            if parts[2] != "tree" || parts.len() < 4 {
                return Err("only repository and folder addresses are supported (.../owner/name or .../tree/branch/folder)".to_string());
            }
            branch = parts[3].to_string();
            if kind == SourceKind::Plugins {
                check_plugin_branch_chars(&branch)?;
            }
            check_branch(&branch)?;
            path = pathsafe::normalize_repo_dir(&parts[4..].join("/")).map_err(|e| format!("folder: {e}"))?;
        }
        Ok(RepoSpec { owner: owner.to_string(), name: name.to_string(), branch, path })
    }
}

fn check_owner(owner: &str) -> Result<(), String> {
    let ok = !owner.is_empty() && owner.len() <= 39 && owner.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if ok { Ok(()) } else { Err(format!("'{owner}' is not a valid GitHub user or organization name")) }
}

fn check_repo_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 100
        && name != "."
        && name != ".."
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.');
    if ok { Ok(()) } else { Err(format!("'{name}' is not a valid repository name")) }
}

/// Copilot's marketplace shorthand uses a narrower alphabet than Git branch names.
pub(crate) fn check_plugin_branch_chars(branch: &str) -> Result<(), &'static str> {
    if branch.bytes().all(|b| b.is_ascii_alphanumeric() || b"._/+-".contains(&b)) {
        Ok(())
    } else {
        Err("This branch name has characters the app will not pass to Copilot CLI: use letters, digits, '.', '_', '/', '+' or '-'.")
    }
}

/// A conservative subset of `git check-ref-format`.
pub fn check_branch(branch: &str) -> Result<(), String> {
    let bad = branch.is_empty()
        || branch.starts_with('/')
        || branch.ends_with('/')
        || branch.starts_with('-')
        || branch.ends_with('.')
        || branch.ends_with(".lock")
        || branch.contains("..")
        || branch.contains("//")
        || branch.contains("@{")
        || branch.chars().any(|c| (c as u32) < 0x20 || c == '\u{7f}' || " ~^:?*[\\".contains(c));
    if bad { Err("is not a valid branch name".to_string()) } else { Ok(()) }
}

/// Where the app keeps its own files. `TSS_HOME` and `TSS_SKILLS_DIR` exist so tests never touch real data.
#[derive(Clone, Debug)]
pub struct Paths {
    pub home: PathBuf,
    pub skills: PathBuf,
    file_roots: Option<[PathBuf; 2]>,
}

impl Paths {
    /// The app folder `TSS_HOME` gives this copy, when it does: the scripts and tests use one of their own.
    pub fn own_home() -> Option<PathBuf> {
        std::env::var_os("TSS_HOME").filter(|v| !v.is_empty()).map(PathBuf::from)
    }

    pub fn home_dir() -> PathBuf {
        if let Some(h) = Paths::own_home() {
            return h;
        }
        let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        base.join("tiny-skill-sync")
    }

    pub fn skills_dir(cfg: &Config) -> PathBuf {
        if let Some(d) = std::env::var_os("TSS_SKILLS_DIR").filter(|v| !v.is_empty()) {
            return PathBuf::from(d);
        }
        if let Some(d) = cfg.skills_dir.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
            return PathBuf::from(d);
        }
        let profile = std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        profile.join(".copilot").join("skills")
    }

    pub fn new(home: PathBuf, skills: PathBuf) -> Paths {
        Paths { home, skills, file_roots: None }
    }

    /// Explicit, per-engine roots (also used by in-process fixtures, without changing process-wide environment).
    pub fn with_file_roots(mut self, agents: PathBuf, instructions: PathBuf) -> Self {
        self.file_roots = Some([agents, instructions]);
        self
    }

    pub(crate) fn root(&self, kind: SourceKind) -> Result<PathBuf, String> {
        kind.root_with(&self.skills, |key| match (&self.file_roots, key) {
            (Some(roots), "TSS_AGENTS_DIR") => Some(roots[0].as_os_str().into()),
            (Some(roots), "TSS_INSTRUCTIONS_DIR") => Some(roots[1].as_os_str().into()),
            _ => std::env::var_os(key),
        })
    }

    pub(crate) fn has_file_root(&self, kind: SourceKind) -> bool {
        let key = match kind {
            SourceKind::Agents => "TSS_AGENTS_DIR",
            SourceKind::Instructions => "TSS_INSTRUCTIONS_DIR",
            SourceKind::Skills | SourceKind::Plugins => return false,
        };
        self.file_roots.is_some() || std::env::var_os(key).is_some()
    }

    pub fn config_file(&self) -> PathBuf {
        self.home.join(CONFIG_FILE)
    }

    pub fn state_file(&self) -> PathBuf {
        self.home.join("state.json")
    }

    /// Scratch space for downloads and replaced folders. It sits beside the skills folder rather than inside it,
    /// so Copilot never sees half-installed skills, and it is on the same volume so renames are instant and atomic.
    pub fn work_dir(&self) -> PathBuf {
        match self.skills.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.join(".tss-work"),
            _ => self.skills.join(".tss-work"),
        }
    }
}

/// A short name for an app folder, the same every time. One copy of the app runs per folder, and a copy with a folder
/// of its own (`TSS_HOME`) also has a Start with Windows entry of its own.
pub fn instance_tag(home: &Path) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in home.to_string_lossy().to_lowercase().bytes() {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

// Frozen shapes of the shipped readers. Do not reuse the evolving source model for a downgrade test.
#[cfg(test)]
#[allow(dead_code)]
pub(crate) mod v1_reader {
    use super::*;

    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Repo {
        id: String,
        repo: String,
        branch: String,
        path: String,
        interval: Option<Interval>,
        use_token: bool,
        #[serde(default = "enabled")]
        enabled: bool,
    }

    fn enabled() -> bool {
        true
    }
    fn version() -> u32 {
        1
    }
    fn daily() -> Interval {
        Interval::new(1, Unit::Day)
    }

    #[derive(Deserialize)]
    struct Local {
        #[serde(default = "version")]
        version: u32,
        skills_dir: Option<String>,
        #[serde(default = "daily")]
        default_interval: Interval,
        #[serde(default, deserialize_with = "UpdatePrefs::read_object")]
        app_update: UpdatePrefs,
        #[serde(default)]
        repos: Vec<Repo>,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Sources {
        version: u32,
        #[serde(default)]
        repos: Vec<Repo>,
    }

    pub fn accepts_local(bytes: &[u8]) -> bool {
        parse_json_object::<Local>(bytes).unwrap().version <= 1
    }

    pub fn accepts_sources(bytes: &[u8]) -> bool {
        parse_json_object::<Sources>(bytes).unwrap().version == 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_source_config_round_trips_as_typed_settings() {
        let bytes = br#"{"version":2,"repos":[{"repo":"owner/tools","branch":"main","path":"tools","kind":"plugins"}]}"#;
        let cfg = Config::parse(bytes).unwrap();
        assert!(cfg.problems().is_empty());
        assert_eq!(cfg.wire_version(), 2);
        assert!(!v1_reader::accepts_local(cfg.to_json().as_bytes()));
        assert_eq!(Config::parse(cfg.to_json().as_bytes()).unwrap(), cfg);
        for branch in ["", "HEAD", "unusable#ref"] {
            let source = RepoConfig { branch: branch.into(), ..cfg.repos[0].clone() };
            assert!(!source.problems().is_empty(), "{branch}");
        }
        let mut no_version = serde_json::from_slice::<serde_json::Value>(bytes).unwrap();
        no_version.as_object_mut().unwrap().remove("version");
        assert!(Config::parse(&serde_json::to_vec(&no_version).unwrap()).is_err());
    }

    #[test]
    fn plugin_sources_only_allow_branch_characters_copilot_accepts() {
        let expected = "This branch name has characters the app will not pass to Copilot CLI: use letters, digits, '.', '_', '/', '+' or '-'.";
        for branch in ["caf\u{e9}", "test!", "bad#ref", "a~b"] {
            let source = RepoConfig { repo: "owner/repo".into(), branch: branch.into(), kind: SourceKind::Plugins, ..RepoConfig::default() };
            assert_eq!(source.problems(), [expected], "{branch}");
            if check_branch(branch).is_ok() {
                for kind in [SourceKind::Skills, SourceKind::Agents, SourceKind::Instructions] {
                    assert!(RepoConfig { kind, ..source.clone() }.problems().is_empty(), "{kind:?}: {branch}");
                }
            }
        }
        let allowed = RepoConfig { repo: "owner/repo".into(), branch: "Release/v1.2_rc+test-3".into(), kind: SourceKind::Plugins, ..RepoConfig::default() };
        assert!(allowed.problems().is_empty());
    }

    fn temp_dir(label: &str) -> PathBuf {
        let base = std::env::var_os("TSS_TEST_ROOT").filter(|s| !s.is_empty()).map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        base.join(format!("tss-config-{label}-{}", fsx::unique_suffix()))
    }

    #[test]
    fn parses_plain_names_and_addresses() {
        let spec = RepoSpec::parse("AngelosP/tiny-skill-sync-fixtures-public").unwrap();
        assert_eq!(
            (spec.owner.as_str(), spec.name.as_str(), spec.branch.as_str(), spec.path.as_str()),
            ("AngelosP", "tiny-skill-sync-fixtures-public", "", "")
        );

        let spec = RepoSpec::parse("https://github.com/o/r.git").unwrap();
        assert_eq!(spec.full_name(), "o/r");

        let spec = RepoSpec::parse("https://github.com/o/r/tree/main/skills/x?tab=1").unwrap();
        assert_eq!((spec.branch.as_str(), spec.path.as_str()), ("main", "skills/x"));

        let spec = RepoSpec::parse("github.com/o/r/tree/dev").unwrap();
        assert_eq!((spec.branch.as_str(), spec.path.as_str()), ("dev", ""));
    }

    #[test]
    fn rejects_bad_repositories() {
        for bad in [
            "",
            "owner",
            "a/b/c",
            "https://gitlab.com/a/b",
            "https://github.com/a/b/blob/main/x",
            "https://github.com/a/b/tree",
            "a b/c",
            "a/b c",
            "-/..",
            "a/..",
            "o/r/tree/main/../x",
        ] {
            assert!(RepoSpec::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn branch_rules() {
        for ok in ["main", "feature/x", "v1.2", "release-2026"] {
            assert!(check_branch(ok).is_ok(), "{ok}");
        }
        for bad in ["", "a..b", "a b", "a:b", "-x", "x/", "/x", "x.lock", "a//b", "a@{b", "a~b", "a\\b"] {
            assert!(check_branch(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn config_round_trips_and_fills_defaults() {
        let mut cfg = Config::default();
        let spec = RepoSpec::parse("o/r").unwrap();
        cfg.add_repo(&spec, Some(Interval::new(6, Unit::Hour)), true);
        cfg.add_repo(&spec, None, false);
        assert_eq!(cfg.repos[0].id, "o/r".replace('/', "-"));
        assert_eq!(cfg.repos[1].id, "o-r-2");
        let back = Config::parse(cfg.to_json().as_bytes()).unwrap();
        assert_eq!(back, cfg);
        assert!(cfg.problems().is_empty(), "{:?}", cfg.problems());
    }

    #[test]
    fn app_update_is_optional_in_v1_and_non_defaults_round_trip() {
        let legacy = Config::parse(br#"{"version":1,"repos":[{"repo":"o/r"}]}"#).unwrap();
        assert_eq!(legacy.app_update, UpdatePrefs { enabled: true, interval: UpdateInterval::Daily, timing: UpdateTiming::Immediate });
        let json: serde_json::Value = serde_json::from_str(&legacy.to_json()).unwrap();
        assert_eq!(json["version"], 1);
        assert!(json.get("app_update").is_none());

        let off = Config::parse(br#"{"app_update":{"enabled":false}}"#).unwrap();
        assert_eq!(off.app_update, UpdatePrefs { enabled: false, ..legacy.app_update });
        let cfg = Config::parse(br#"{"app_update":{"interval":"monthly","timing":"on_quit"}}"#).unwrap();
        assert_eq!(cfg.app_update, UpdatePrefs { enabled: true, interval: UpdateInterval::Monthly, timing: UpdateTiming::OnQuit });
        assert_eq!(Config::parse(cfg.to_json().as_bytes()).unwrap(), cfg);
    }

    #[test]
    fn invalid_present_update_preferences_never_fall_back_to_enabled() {
        for bad in [
            "null",
            "[]",
            r#"[false,"weekly","on_quit"]"#,
            r#"{"enabled":null}"#,
            r#"{"enabled":"false"}"#,
            r#"{"enable":false}"#,
            r#"{"enabled":false,"enabled":true}"#,
            r#"{"interval":"1mo"}"#,
            r#"{"timing":"on_start"}"#,
            r#"{"interval":{"daily":null}}"#,
        ] {
            let json = format!(r#"{{"app_update":{bad}}}"#);
            assert!(Config::parse(json.as_bytes()).is_err(), "{json}");
        }
        assert!(Config::parse(b"[]").is_err());
    }

    #[test]
    fn new_configs_and_repositories_default_to_one_day() {
        let daily = Interval::new(1, Unit::Day);
        assert_eq!(DEFAULT_INTERVAL, daily);

        let mut cfg = Config::default();
        let default_interval = cfg.default_interval;
        let spec = RepoSpec::parse("o/r").unwrap();
        let added = cfg.add_repo(&spec, None, false);
        assert_eq!(default_interval, daily);
        assert_eq!(added.interval, None);
        assert_eq!(added.effective_interval(default_interval), daily);
    }

    #[test]
    fn minimal_hand_written_config_works() {
        let cfg = Config::parse(br#"{ "default_interval": "12h", "repos": [ { "repo": "a/b" }, { "repo": "a/b", "path": "skills" } ] }"#).unwrap();
        assert_eq!(cfg.default_interval, Interval::new(12, Unit::Hour));
        assert!(cfg.repos[0].enabled && !cfg.repos[0].use_token);
        assert_eq!(cfg.repos[0].id, "a-b");
        assert_eq!(cfg.repos[1].id, "a-b-2");
        // Ids are derived, so loading the same file twice assigns the same ids.
        let again = Config::parse(br#"{ "default_interval": "12h", "repos": [ { "repo": "a/b" }, { "repo": "a/b", "path": "skills" } ] }"#).unwrap();
        assert_eq!(cfg, again);
    }

    #[test]
    fn config_from_a_newer_version_is_refused() {
        assert!(Config::parse(br#"{ "version": 99 }"#).is_err());
        assert!(Config::parse(b"not json").is_err());
    }

    #[test]
    fn an_explicit_invalid_kind_is_not_a_legacy_skill() {
        for kind in ["null", r#""plugins""#, r#"["skills","agents"]"#, r#"{"skills":null}"#] {
            let json = format!(r#"{{"repos":[{{"repo":"o/r","kind":{kind}}}]}}"#);
            assert!(Config::parse(json.as_bytes()).is_err(), "{json}");
        }
    }

    #[test]
    fn only_a_loose_source_gates_the_local_writer_and_skills_dispatch() {
        let dir = temp_dir("typed");
        let path = dir.join(CONFIG_FILE);
        let mut cfg = Config::parse(br#"{"repos":[{"repo":"o/r"},{"repo":"o/r","kind":"skills","id":"o-r-2","path":"skills"}]}"#).unwrap();
        let legacy = cfg.to_json();
        assert!(v1_reader::accepts_local(legacy.as_bytes()));
        assert!(!legacy.contains("\"kind\""));
        let spec = RepoSpec::parse("https://github.com/o/r/tree/main/more").unwrap();
        for kind in [SourceKind::Agents, SourceKind::Instructions] {
            cfg.add_source(kind, &spec, None, false);
        }
        assert_eq!(cfg.repos.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["o-r", "o-r-2", "o-r-3", "o-r-4"]);
        cfg.save(&path).unwrap();
        let bytes = fs::read(&path).unwrap();
        assert!(!v1_reader::accepts_local(&bytes), "the old payload decoded, but its version gate refused");
        assert_eq!(Config::load_sources(&path).unwrap(), cfg);
        assert!(Config::load(&path).is_err(), "never give mixed settings to the skills worker");
        assert_eq!(fs::read(&path).unwrap(), bytes);
        let before = cfg.clone();
        cfg.repos.truncate(2);
        cfg.save_if_unchanged(&path, &before).unwrap();
        assert_eq!(fs::read(&path).unwrap(), legacy.as_bytes());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn invalid_or_ungated_typed_local_settings_cannot_be_overwritten() {
        let dir = temp_dir("kind-gate");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(CONFIG_FILE);
        for bad in [
            r#"{"version":1,"repos":[{"kind":"agents"}]}"#,
            r#"{"version":2,"repos":[{"kind":null}]}"#,
            r#"{"version":2,"repos":[{"kind":"packages"}]}"#,
            r#"{"version":2,"repos":[{"kind":"skills","kind":"instructions"}]}"#,
            r#"{"version":2,"repos":[{"id":"r","kind":"agents"},{"id":"r"}]}"#,
            r#"{"repos":[{"kind":null}],"version":99}"#,
        ] {
            fs::write(&path, bad).unwrap();
            let err = Config::load_sources(&path).unwrap_err();
            if bad.contains("99") {
                assert!(err.contains("newer version"), "{err}");
            }
            assert!(Config::default().save(&path).is_err());
            assert_eq!(fs::read(&path).unwrap(), bad.as_bytes());
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_missing_file_is_an_empty_config_and_a_saved_one_loads_back() {
        let dir = temp_dir("load");
        let file = dir.join(CONFIG_FILE);
        assert_eq!(Config::load(&file).unwrap(), Config::default());

        let mut cfg = Config::default();
        cfg.add_repo(&RepoSpec::parse("o/r").unwrap(), Some(Interval::new(6, Unit::Hour)), true);
        cfg.save(&file).unwrap();
        assert_eq!(Config::load(&file).unwrap(), cfg);

        let mut with_bom = b"\xEF\xBB\xBF".to_vec();
        with_bom.extend_from_slice(br#"{ "repos": [ { "repo": "a/b" } ] }"#);
        fs::write(&file, with_bom).unwrap();
        assert_eq!(Config::load(&file).unwrap().repos[0].id, "a-b", "a hand-edited file with a byte order mark");

        fs::write(&file, b"{ broken").unwrap();
        let e = Config::load(&file).unwrap_err();
        assert!(e.contains("not valid") && e.contains(CONFIG_FILE), "{e}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_local_preference_edit_preserves_conflicting_or_unreadable_settings() {
        let dir = temp_dir("stale");
        let file = dir.join(CONFIG_FILE);
        let before = Config::load(&file).unwrap();
        let off = Config { app_update: UpdatePrefs { enabled: false, ..before.app_update }, ..before.clone() };
        off.save_if_unchanged(&file, &before).unwrap();
        let stale = Config { default_interval: Interval::new(6, Unit::Hour), ..before.clone() };
        assert!(stale.save_if_unchanged(&file, &before).unwrap_err().contains("changed since it was loaded"));
        assert_eq!(Config::load(&file).unwrap(), off);

        let invalid = br#"{"app_update":{"timing":"invalid"}}"#;
        fs::write(&file, invalid).unwrap();
        assert!(off.save_if_unchanged(&file, &before).is_err());
        assert!(off.save(&file).is_err());
        assert_eq!(fs::read(&file).unwrap(), invalid);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_competing_local_settings_writer_cannot_undo_off() {
        let dir = temp_dir("writer");
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join(CONFIG_FILE);
        let lock = dir.join(".tss-settings.lock");
        let before = Config::load(&file).unwrap();
        assert!(!lock.exists(), "read-only status creates no lock");
        let stale = Config { default_interval: Interval::new(6, Unit::Hour), ..before.clone() };
        let off = Config { app_update: UpdatePrefs { enabled: false, ..before.app_update }, ..before.clone() };

        // The first writer has compared its snapshot but has not replaced the file yet.
        let held = settings_write_lock(&dir).unwrap();
        assert!(stale.save_if_unchanged(&file, &before).is_err());
        assert_eq!(Config::load(&file).unwrap(), before);
        fs::write(&file, off.to_json()).unwrap();
        drop(held);
        assert!(!lock.exists());
        fs::write(&lock, b"not ours to remove").unwrap();
        assert!(stale.save(&file).is_err(), "never take over an unrelated entry at the lock name");
        assert_eq!(fs::read(&lock).unwrap(), b"not ours to remove");
        fs::remove_file(&lock).unwrap();

        assert!(stale.save_if_unchanged(&file, &before).is_err());
        assert_eq!(Config::load(&file).unwrap(), off);
        assert!(!lock.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_colliding_atomic_temp_is_not_opened_for_overwrite_or_cleaned_up() {
        let dir = temp_dir("atomic-collision");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(crate::update_clock::CLOCK_FILE);
        let temp = dir.join("occupied.tmp");
        fs::write(&temp, b"not the writer's file").unwrap();
        fs::write(&path, b"last good record").unwrap();
        assert!(write_with_temp(&path, b"new record", &temp).is_err());
        assert_eq!(fs::read(&temp).unwrap(), b"not the writer's file");
        assert_eq!(fs::read(&path).unwrap(), b"last good record");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn problems_are_reported_in_plain_words() {
        let cfg = Config::parse(br#"{ "default_interval": "1m", "repos": [ { "id": "x", "repo": "nope" }, { "id": "x", "repo": "a/b", "interval": "2m", "path": "../x", "branch": "a b" } ] }"#).unwrap();
        let problems = cfg.problems();
        assert!(problems.iter().any(|p| p.contains("default interval")), "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("share the id")), "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("owner/name")), "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("folder")), "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("branch")), "{problems:?}");
    }

    #[test]
    fn a_skills_folder_that_is_not_a_full_path_is_a_settings_error() {
        // Each would depend on the folder the app happens to be started in, which at sign-in is Windows' own.
        for relative in ["skills", r".\skills", r"\skills", "C:skills", r"%USERPROFILE%\skills", " skills "] {
            let json = serde_json::json!({ "skills_dir": relative }).to_string();
            let e = Config::parse(json.as_bytes()).unwrap_err();
            assert!(e.starts_with("the skills folder") && e.contains("full path") && e.contains(relative.trim()), "{relative}: {e}");
        }
        for fine in [r"C:\skills", "D:/skills", r"\\server\share\skills", r"\\?\C:\skills", "", "  "] {
            let json = serde_json::json!({ "skills_dir": fine }).to_string();
            assert!(Config::parse(json.as_bytes()).is_ok(), "{fine}");
        }
    }

    #[test]
    fn single_skill_name_comes_from_the_folder_or_the_repo() {
        let mut r = RepoConfig { repo: "o/my-skill".into(), ..RepoConfig::default() };
        assert_eq!(r.single_skill_name(), "my-skill");
        r.path = "tools/deep".into();
        assert_eq!(r.single_skill_name(), "deep");
    }

    #[test]
    fn work_dir_is_a_sibling_of_the_skills_folder() {
        let p = Paths::new(PathBuf::from(r"C:\h"), PathBuf::from(r"C:\Users\u\.copilot\skills"));
        assert_eq!(p.work_dir(), PathBuf::from(r"C:\Users\u\.copilot\.tss-work"));
    }
}
