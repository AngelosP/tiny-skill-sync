//! What the user configures: which repositories to follow, how often, and where skills are installed.
//! `config.json` is meant to be readable and hand-editable.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fsx;
use crate::interval::{Interval, Unit};
use crate::pathsafe;

pub const CONFIG_FILE: &str = "config.json";
pub const CONFIG_VERSION: u32 = 1;
pub const DEFAULT_INTERVAL: Interval = Interval::new(1, Unit::Day);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    /// Where skills are installed. Empty means `%USERPROFILE%\.copilot\skills`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills_dir: Option<String>,
    pub default_interval: Interval,
    pub repos: Vec<RepoConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Config { version: CONFIG_VERSION, skills_dir: None, default_interval: DEFAULT_INTERVAL, repos: Vec::new() }
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
    /// The folder that holds the skills. Empty means the repository root.
    pub path: String,
    /// Empty means the default interval.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval: Option<Interval>,
    /// Send the user's GitHub token. Needed for private repositories; also raises the rate limit.
    pub use_token: bool,
    pub enabled: bool,
}

impl Default for RepoConfig {
    fn default() -> Self {
        RepoConfig { id: String::new(), repo: String::new(), branch: String::new(), path: String::new(), interval: None, use_token: false, enabled: true }
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
        if !self.branch.is_empty() {
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
    pub fn parse(bytes: &[u8]) -> Result<Config, String> {
        // Notepad and Windows PowerShell 5.1 start UTF-8 files with a byte order mark.
        let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
        let mut cfg: Config = serde_json::from_slice(bytes).map_err(|e| format!("config.json is not valid: {e}"))?;
        if cfg.version > CONFIG_VERSION {
            return Err(format!("config.json was written by a newer version (format {}, this app understands {CONFIG_VERSION})", cfg.version));
        }
        // A relative folder would depend on the folder the app is started in, which at sign-in is Windows' own.
        if let Some(dir) = cfg.skills_dir.as_deref().map(str::trim).filter(|d| !d.is_empty() && !Path::new(d).is_absolute()) {
            return Err(format!("the skills folder in {CONFIG_FILE} must be a full path, such as D:\\skills; '{dir}' is not one"));
        }
        cfg.version = CONFIG_VERSION;
        cfg.assign_missing_ids();
        Ok(cfg)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("config serializes") + "\n"
    }

    /// A missing file is an empty configuration; a file that cannot be read or understood is an error, so it is never
    /// overwritten by mistake.
    pub fn load(path: &Path) -> Result<Config, String> {
        match fs::read(path) {
            Ok(bytes) => Config::parse(&bytes).map_err(|e| format!("{e} ({})", path.display())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        fsx::write_atomic(path, self.to_json().as_bytes()).map_err(|e| format!("cannot save {}: {e}", path.display()))
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
        let id = self.unique_id(&spec.full_name());
        self.repos.push(RepoConfig { id, repo: spec.full_name(), branch: spec.branch.clone(), path: spec.path.clone(), interval, use_token, enabled: true });
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
        Paths { home, skills }
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn a_missing_file_is_an_empty_config_and_a_saved_one_loads_back() {
        let dir = std::env::temp_dir().join(format!("tss-config-{}", fsx::unique_suffix()));
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
