//! What the user configures: repositories, where skills are installed, and app update preferences.
//! `config.json` is meant to be readable and hand-editable.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use windows_sys::Win32::Foundation::GENERIC_WRITE;
use windows_sys::Win32::Storage::FileSystem::{DELETE, FILE_FLAG_DELETE_ON_CLOSE, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ};

use crate::fsx;
use crate::interval::{Interval, Unit};
use crate::pathsafe;
use crate::source::SourceKind;
use crate::sources::{GlobalSettings, SourceConfig};

pub const CONFIG_FILE: &str = "config.json";
pub const CONFIG_VERSION: u32 = crate::sources::CONFIG_VERSION;
pub const DEFAULT_INTERVAL: Interval = Interval::new(1, Unit::Day);

/// Hold through re-read, decision, and replacement. The directory must exist. Cooperating writers must see the
/// *same filesystem directory*; independently synchronized cloud replicas do not share a lock. Readers need no lock.
pub(crate) fn settings_write_lock(directory: &Path) -> Result<File, String> {
    settings_write_lock_within(directory, Duration::ZERO)
}

/// `settings_write_lock`, trying again while another writer holds the lock until `wait` has passed. Waiting changes
/// nothing, so it may be stopped at any point.
pub(crate) fn settings_write_lock_within(directory: &Path, wait: Duration) -> Result<File, String> {
    let path = directory.join(".tss-settings.lock");
    let deadline = Instant::now() + wait;
    loop {
        // CREATE_NEW never takes over an existing entry. Delete-on-close removes only the file this handle created,
        // including on process exit; unlinking a lock by name could remove somebody else's lock.
        let opened = OpenOptions::new()
            .write(true)
            .access_mode(GENERIC_WRITE | DELETE)
            .create_new(true)
            .share_mode(0)
            .custom_flags(FILE_FLAG_DELETE_ON_CLOSE | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&path);
        match opened {
            Err(e) if held_by_another(&e) && Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            done => return done.map_err(|e| format!("cannot lock settings at {} (another writer may be busy); try again: {e}", path.display())),
        }
    }
}

/// The lock file exists, is being deleted by a writer that just let it go (access denied), or is open (sharing).
fn held_by_another(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::AlreadyExists || matches!(e.raw_os_error(), Some(5 | 32))
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

/// The settings. Sources and the global type and filter settings are the single source of truth; the lane views the
/// engine runs (`repos`) and the set of every persisted lane ID are derived from them after every change, so they
/// can never drift.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// Where skills are installed. Empty means `%USERPROFILE%\.copilot\skills`.
    pub skills_dir: Option<String>,
    pub default_interval: Interval,
    pub app_update: UpdatePrefs,
    /// Private inside, so it can only change through the methods that rebuild the lane views.
    pub source_set: SourceSet,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SourceSet {
    sources: Vec<SourceConfig>,
    global: GlobalSettings,
    lanes: Vec<RepoConfig>,
    known: Vec<(String, SourceKind)>,
}

/// Format 1 and 2: one entry per typed source. Read only, to migrate.
#[derive(Deserialize)]
#[serde(default)]
pub(crate) struct LegacyConfig {
    pub skills_dir: Option<String>,
    pub default_interval: Interval,
    #[serde(deserialize_with = "UpdatePrefs::read_object")]
    pub app_update: UpdatePrefs,
    pub repos: Vec<RepoConfig>,
    /// Format 3 fields. Present in an older-format file, they mean a missing or wrong `version`, not data to drop.
    sources: Option<serde::de::IgnoredAny>,
    global: Option<serde::de::IgnoredAny>,
}

impl Default for LegacyConfig {
    fn default() -> Self {
        let Config { skills_dir, default_interval, app_update, .. } = Config::default();
        LegacyConfig { skills_dir, default_interval, app_update, repos: Vec::new(), sources: None, global: None }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Wire {
    version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    skills_dir: Option<String>,
    default_interval: Interval,
    #[serde(deserialize_with = "UpdatePrefs::read_object", skip_serializing_if = "UpdatePrefs::is_default")]
    app_update: UpdatePrefs,
    #[serde(skip_serializing_if = "is_default_global")]
    global: GlobalSettings,
    sources: Vec<SourceConfig>,
    #[serde(skip_serializing)]
    repos: Option<serde::de::IgnoredAny>,
}

impl Default for Wire {
    fn default() -> Self {
        let Config { skills_dir, default_interval, app_update, .. } = Config::default();
        Wire { version: CONFIG_VERSION, skills_dir, default_interval, app_update, global: GlobalSettings::default(), sources: Vec::new(), repos: None }
    }
}

fn is_default_global(g: &GlobalSettings) -> bool {
    *g == GlobalSettings::default()
}

impl Default for Config {
    fn default() -> Self {
        Config { skills_dir: None, default_interval: DEFAULT_INTERVAL, app_update: UpdatePrefs::default(), source_set: SourceSet::default() }
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
    /// The ID of the source this lane belongs to (not persisted on a lane).
    #[serde(skip)]
    pub source: String,
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
            source: String::new(),
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
        // An empty branch follows the default branch; a full plugin check resolves its name first.
        let plugin_branch_problem = if self.kind != SourceKind::Plugins || self.branch.is_empty() {
            None
        } else if self.branch == "HEAD" {
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

/// Give hand-written legacy entries without an ID one derived from the repository name, the same on every load.
pub(crate) fn assign_legacy_ids(repos: &mut [RepoConfig]) {
    for i in 0..repos.len() {
        if repos[i].id.trim().is_empty() {
            let taken: HashSet<String> = repos.iter().map(|r| r.id.clone()).collect();
            repos[i].id = unique_id(&repos[i].repo, &taken);
        }
    }
}

pub(crate) fn id_base(repo: &str) -> String {
    let base: String = repo.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '-' }).collect();
    if base.is_empty() { "repo".to_string() } else { base }
}

fn unique_id(repo: &str, taken: &HashSet<String>) -> String {
    let base = id_base(repo);
    let mut candidate = base.clone();
    let mut n = 2;
    while taken.contains(&candidate) {
        candidate = format!("{base}-{n}");
        n += 1;
    }
    candidate
}

pub const KINDS: [SourceKind; 4] = [SourceKind::Skills, SourceKind::Agents, SourceKind::Instructions, SourceKind::Plugins];

impl Config {
    pub fn wire_version(&self) -> u32 {
        CONFIG_VERSION
    }

    /// A missing source or lane ID (a legacy file being migrated, or a hand-written source) is derived from the file
    /// alone, the same on every load, so that records saved under it keep matching.
    pub fn parse(bytes: &[u8]) -> Result<Config, String> {
        let invalid = |e| format!("config.json is not valid: {e}");
        let Format { version } = parse_json_object(bytes).map_err(invalid)?;
        if version > CONFIG_VERSION {
            return Err(format!("config.json was written by a newer version (format {version}, this app understands {CONFIG_VERSION})"));
        }
        let cfg = if version >= 3 {
            let wire: Wire = parse_json_object(bytes).map_err(invalid)?;
            if wire.repos.is_some() {
                return Err(invalid("format 3 lists its repositories under \"sources\", not \"repos\"".into()));
            }
            Config::from_parts(wire.skills_dir, wire.default_interval, wire.app_update, wire.global, wire.sources)?
        } else {
            let legacy: LegacyConfig = parse_json_object(bytes).map_err(invalid)?;
            if legacy.sources.is_some() || legacy.global.is_some() {
                return Err(invalid("\"sources\" and \"global\" are format 3 settings, but the file does not say \"version\": 3".into()));
            }
            let sources = migrate_file(version, legacy.repos)?;
            Config::from_parts(legacy.skills_dir, legacy.default_interval, legacy.app_update, GlobalSettings::default(), sources)?
        };
        // A relative folder would depend on the folder the app is started in, which at sign-in is Windows' own.
        if let Some(dir) = cfg.skills_dir.as_deref().map(str::trim).filter(|d| !d.is_empty() && !Path::new(d).is_absolute()) {
            return Err(format!("the skills folder in {CONFIG_FILE} must be a full path, such as D:\\skills; '{dir}' is not one"));
        }
        Ok(cfg)
    }

    pub(crate) fn from_parts(
        skills_dir: Option<String>,
        default_interval: Interval,
        app_update: UpdatePrefs,
        global: GlobalSettings,
        mut sources: Vec<SourceConfig>,
    ) -> Result<Config, String> {
        let missing = sources.iter().any(|s| s.id.trim().is_empty() || KINDS.iter().any(|&k| s.lanes.id(k).is_none()));
        if missing {
            // Only the IDs in the settings themselves count, never state records: anything else could give a lane a
            // different ID on the next load and detach what it installed.
            let mut taken = HashSet::new();
            for s in &sources {
                taken.extend(std::iter::once(s.id.clone()).chain(KINDS.iter().filter_map(|&k| s.lanes.id(k).map(str::to_owned))));
            }
            taken.remove("");
            for s in &mut sources {
                if s.id.trim().is_empty() {
                    s.id = unique_id(&s.repo, &taken);
                    taken.insert(s.id.clone());
                }
            }
            crate::sources::complete_lanes(&mut sources, &mut taken);
        }
        check_ids(&sources)?;
        let mut cfg = Config { skills_dir, default_interval, app_update, source_set: SourceSet { sources, global, ..SourceSet::default() } };
        cfg.rebuild();
        Ok(cfg)
    }

    fn rebuild(&mut self) {
        self.source_set.lanes.clear();
        self.source_set.known.clear();
        for s in &self.source_set.sources {
            let types = s.effective_types(&self.source_set.global);
            for kind in KINDS {
                let Some(id) = s.lanes.id(kind) else { continue };
                self.source_set.known.push((id.to_owned(), kind));
                if types.enabled(kind) {
                    self.source_set.lanes.push(RepoConfig {
                        id: id.to_owned(),
                        repo: s.repo.clone(),
                        branch: s.branch.clone(),
                        path: s.path.clone(),
                        interval: s.interval,
                        use_token: s.use_token,
                        enabled: s.enabled,
                        kind,
                        source: s.id.clone(),
                    });
                }
            }
        }
    }

    pub fn to_json(&self) -> String {
        let wire = Wire {
            version: CONFIG_VERSION,
            skills_dir: self.skills_dir.clone(),
            default_interval: self.default_interval,
            app_update: self.app_update,
            global: self.source_set.global.clone(),
            sources: self.source_set.sources.clone(),
            repos: None,
        };
        serde_json::to_string_pretty(&wire).expect("config serializes") + "\n"
    }

    /// A missing file is an empty configuration; a file that cannot be read or understood is an error, so it is never
    /// overwritten by mistake.
    pub fn load(path: &Path) -> Result<Config, String> {
        Self::load_sources(path)
    }

    /// A legacy file is migrated in memory, and missing IDs are derived from the file alone (see `parse`).
    pub fn load_sources(path: &Path) -> Result<Config, String> {
        match read_optional(path)? {
            Some(bytes) => Config::parse(&bytes).map_err(|e| format!("{e} ({})", path.display())),
            None => Ok(Config::default()),
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
        check_ids(&self.source_set.sources)
    }

    /// The lanes to run: every lane whose type is on, in source order, then skills, agents, instructions, plugins.
    pub fn repos(&self) -> &[RepoConfig] {
        &self.source_set.lanes
    }

    pub fn sources(&self) -> &[SourceConfig] {
        &self.source_set.sources
    }

    pub fn global(&self) -> &GlobalSettings {
        &self.source_set.global
    }

    /// Every persisted lane ID, on or off. A state record under one of these is still configured.
    pub fn known_lanes(&self) -> &[(String, SourceKind)] {
        &self.source_set.known
    }

    pub fn is_known_lane(&self, id: &str) -> bool {
        self.source_set.known.iter().any(|(k, _)| k == id)
    }

    /// Every source ID and lane ID in use, for allocating new ones.
    pub fn taken_ids(&self) -> HashSet<String> {
        self.source_set.sources.iter().map(|s| s.id.clone()).chain(self.source_set.known.iter().map(|(id, _)| id.clone())).collect()
    }

    /// The lane view of an on lane.
    pub fn find(&self, id: &str) -> Option<&RepoConfig> {
        self.source_set.lanes.iter().find(|r| r.id == id)
    }

    /// The source with this source ID or lane ID.
    pub fn find_source(&self, id: &str) -> Option<&SourceConfig> {
        self.source_set
            .sources
            .iter()
            .find(|s| s.id == id)
            .or_else(|| self.source_set.sources.iter().find(|s| KINDS.iter().any(|&k| s.lanes.id(k) == Some(id))))
    }

    /// Every lane ID of the source that has `id` (its own ID or any lane's), whether its type is on or off.
    pub fn lane_ids(&self, id: &str) -> Vec<&str> {
        self.find_source(id).map(|s| KINDS.iter().filter_map(|&k| s.lanes.id(k)).collect()).unwrap_or_default()
    }

    /// The on lanes of the source with this source ID or lane ID.
    pub fn lanes_of(&self, id: &str) -> Vec<&RepoConfig> {
        match self.find_source(id) {
            Some(s) => self.source_set.lanes.iter().filter(|r| r.source == s.id).collect(),
            None => Vec::new(),
        }
    }

    /// Change sources or global settings; the lane views are rebuilt afterwards.
    pub fn edit<R>(&mut self, f: impl FnOnce(&mut Vec<SourceConfig>, &mut GlobalSettings) -> R) -> R {
        let out = f(&mut self.source_set.sources, &mut self.source_set.global);
        self.rebuild();
        out
    }

    fn edit_source(&mut self, id: &str, f: impl FnOnce(&mut SourceConfig)) -> bool {
        let Some(index) = self.find_source(id).and_then(|s| self.source_set.sources.iter().position(|o| o.id == s.id)) else { return false };
        self.edit(|sources, _| f(&mut sources[index]));
        true
    }

    pub fn set_enabled(&mut self, id: &str, enabled: bool) -> bool {
        self.edit_source(id, |s| s.enabled = enabled)
    }

    pub fn set_interval(&mut self, id: &str, interval: Option<Interval>) -> bool {
        self.edit_source(id, |s| s.interval = interval)
    }

    pub fn set_use_token(&mut self, id: &str, use_token: bool) -> bool {
        self.edit_source(id, |s| s.use_token = use_token)
    }

    /// Remove the whole source with this source ID or lane ID.
    pub fn remove(&mut self, id: &str) -> bool {
        let Some(sid) = self.find_source(id).map(|s| s.id.clone()) else { return false };
        self.edit(|sources, _| sources.retain(|s| s.id != sid));
        true
    }

    /// Stop syncing one lane: its type is turned off for its source, or the whole source goes when no other lane of it
    /// is on. Returns false when no source has this ID.
    pub fn remove_lane(&mut self, id: &str) -> bool {
        let Some(source) = self.find_source(id) else { return false };
        let sid = source.id.clone();
        let kind = KINDS.into_iter().find(|&k| source.lanes.id(k) == Some(id));
        let others = self.source_set.lanes.iter().any(|r| r.source == sid && r.id != id);
        match kind {
            Some(kind) if others => self.edit(|sources, _| {
                if let Some(s) = sources.iter_mut().find(|s| s.id == sid) {
                    s.types.set(kind, false);
                }
            }),
            _ => self.edit(|sources, _| sources.retain(|s| s.id != sid)),
        }
        true
    }

    /// A projection that holds one lane and no sources, for timing calculations only. Never save it.
    pub(crate) fn lane_only(lane: RepoConfig, default_interval: Interval) -> Config {
        Config { default_interval, source_set: SourceSet { lanes: vec![lane], ..SourceSet::default() }, ..Config::default() }
    }

    /// Build settings from typed entries the way migration does (entries at the same location with distinct kinds
    /// share a source; every entry keeps its ID). For tests and fixtures.
    pub fn from_lanes(lanes: Vec<RepoConfig>) -> Config {
        let mut lanes = lanes;
        assign_legacy_ids(&mut lanes);
        let sources = crate::sources::migrate_legacy(&lanes, &HashSet::new()).expect("unique IDs");
        Config::from_parts(None, DEFAULT_INTERVAL, UpdatePrefs::default(), GlobalSettings::default(), sources).expect("valid IDs")
    }

    /// Change one on lane as if it were a typed entry. Location, schedule and token changes apply to its whole source;
    /// a changed `id` renames this lane only. Returns false when no on lane has this ID.
    pub fn update_lane(&mut self, id: &str, f: impl FnOnce(&mut RepoConfig)) -> bool {
        let Some(old) = self.find(id).cloned() else { return false };
        let mut new = old.clone();
        f(&mut new);
        assert_eq!(new.kind, old.kind, "a lane cannot change its type");
        self.edit(|sources, _| {
            let Some(s) = sources.iter_mut().find(|s| s.id == old.source) else { return };
            s.repo = new.repo;
            s.branch = new.branch;
            s.path = new.path;
            s.interval = new.interval;
            s.use_token = new.use_token;
            s.enabled = new.enabled;
            if new.id != old.id {
                s.lanes = s.lanes.clone().with(old.kind, new.id);
            }
        });
        true
    }

    /// `update_lane` by position in `repos()`.
    pub fn update_lane_at(&mut self, index: usize, f: impl FnOnce(&mut RepoConfig)) -> bool {
        let id = self.source_set.lanes[index].id.clone();
        self.update_lane(&id, f)
    }

    /// Remove every source.
    pub fn clear_sources(&mut self) {
        self.edit(|sources, _| sources.clear());
    }

    /// Keep only the sources whose on lanes all pass `keep` (a source without on lanes is kept).
    pub fn retain_lanes(&mut self, mut keep: impl FnMut(&RepoConfig) -> bool) {
        let drop: HashSet<String> = self.source_set.lanes.iter().filter(|r| !keep(r)).map(|r| r.source.clone()).collect();
        if !drop.is_empty() {
            self.edit(|sources, _| sources.retain(|s| !drop.contains(&s.id)));
        }
    }

    pub fn add_repo(&mut self, spec: &RepoSpec, interval: Option<Interval>, use_token: bool) -> &RepoConfig {
        self.add_source(SourceKind::Skills, spec, interval, use_token)
    }

    /// Interim typed add: a new source with only `kind` on (the other three types explicitly off).
    pub fn add_source(&mut self, kind: SourceKind, spec: &RepoSpec, interval: Option<Interval>, use_token: bool) -> &RepoConfig {
        self.push_lane(RepoConfig {
            id: String::new(),
            kind,
            repo: spec.full_name(),
            branch: spec.branch.clone(),
            path: spec.path.clone(),
            interval,
            use_token,
            enabled: true,
            source: String::new(),
        })
    }

    /// Add one typed entry the way migration does: a new source with only its kind on, the lane keeping the entry's ID
    /// (an empty ID gets a fresh one). Panics if the ID is already in use.
    pub fn push_lane(&mut self, mut lane: RepoConfig) -> &RepoConfig {
        let mut taken = self.taken_ids();
        if lane.id.trim().is_empty() {
            lane.id = unique_id(&lane.repo, &taken);
        }
        assert!(!taken.contains(&lane.id), "the ID '{}' is already in use", lane.id);
        taken.insert(lane.id.clone());
        let id = lane.id.clone();
        let mut new = crate::sources::migrate_legacy(std::slice::from_ref(&lane), &taken).expect("one entry");
        self.edit(|sources, _| sources.append(&mut new));
        self.find(&id).expect("its type is on")
    }

    /// Add a source built elsewhere (for example from a preview). Its IDs must be unique.
    pub fn add_source_config(&mut self, source: SourceConfig) -> Result<(), String> {
        let mut all = self.source_set.sources.clone();
        all.push(source);
        check_ids(&all)?;
        self.edit(|sources, _| *sources = all);
        Ok(())
    }

    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Err(e) = self.default_interval.validate_poll() {
            out.push(format!("default interval: {e}"));
        }
        if let Err(e) = check_ids(&self.source_set.sources) {
            out.push(e);
        }
        if let Err(e) = crate::sources::validate_ignore_names(&self.source_set.global.ignore_folders) {
            out.push(format!("ignore_folders: {e}"));
        }
        for s in &self.source_set.sources {
            if let Err(e) = s.effective_policy(&self.source_set.global) {
                out.push(format!("{}: {e}", s.id));
            }
        }
        for r in &self.source_set.lanes {
            out.extend(r.problems().into_iter().map(|p| format!("{}: {p}", r.id)));
        }
        out
    }
}

fn migrate_file(version: u32, mut repos: Vec<RepoConfig>) -> Result<Vec<SourceConfig>, String> {
    if version < 2 && source_version(&repos) == 2 {
        return Err("typed sources require config.json format 2".into());
    }
    assign_legacy_ids(&mut repos);
    crate::sources::migrate_legacy(&repos, &HashSet::new())
}

/// Source IDs are unique; lane IDs are unique across all sources. A source's skills lane may share its source's ID.
fn check_ids(sources: &[SourceConfig]) -> Result<(), String> {
    let mut owner: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for (i, s) in sources.iter().enumerate() {
        if s.id.trim().is_empty() {
            return Err(format!("a source for '{}' has no id", s.repo));
        }
        if owner.insert(&s.id, i).is_some() {
            return Err(format!("two sources share the id '{}'", s.id));
        }
    }
    let mut lanes = HashSet::new();
    for (i, s) in sources.iter().enumerate() {
        for kind in KINDS {
            let Some(id) = s.lanes.id(kind) else { continue };
            if id.trim().is_empty() || !lanes.insert(id) || owner.get(id).is_some_and(|&o| o != i) {
                return Err(format!("two sources share the id '{id}'"));
            }
        }
    }
    Ok(())
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

pub(crate) fn check_owner(owner: &str) -> Result<(), String> {
    let ok = !owner.is_empty() && owner.len() <= 39 && owner.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if ok { Ok(()) } else { Err(format!("'{owner}' is not a valid GitHub user or organization name")) }
}

pub(crate) fn check_repo_name(name: &str) -> Result<(), String> {
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

    /// The display-only cache of what each source's last complete listing held (`crate::seen`).
    pub fn seen_file(&self) -> PathBuf {
        self.home.join(crate::seen::SEEN_FILE)
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
        parse_json_object::<Sources>(bytes).is_ok_and(|s| s.version == 1)
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
        assert_eq!(cfg.wire_version(), 3);
        assert!(!v1_reader::accepts_local(cfg.to_json().as_bytes()));
        assert!(!cfg.to_json().contains("\"kind\""), "format 3 writes types, not kind");
        assert_eq!(Config::parse(cfg.to_json().as_bytes()).unwrap(), cfg);
        for branch in ["HEAD", "unusable#ref"] {
            let source = RepoConfig { branch: branch.into(), ..cfg.repos()[0].clone() };
            assert!(!source.problems().is_empty(), "{branch}");
        }
        // No branch follows the default branch; the check names it before using Copilot CLI.
        assert!(RepoConfig { branch: String::new(), ..cfg.repos()[0].clone() }.problems().is_empty());
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
        assert_eq!(cfg.repos()[0].id, "o/r".replace('/', "-"));
        assert_eq!(cfg.repos()[1].id, "o-r-2");
        let back = Config::parse(cfg.to_json().as_bytes()).unwrap();
        assert_eq!(back, cfg);
        assert!(cfg.problems().is_empty(), "{:?}", cfg.problems());
    }

    #[test]
    fn format_3_keeps_global_rules_and_source_overrides_and_writes_no_kind() {
        let text = br#"{"version":3,"global":{"types":{"plugins":false},"include_repo_items":true,"ignore_folders":["demo"]},
            "sources":[{"id":"kit","repo":"o/kit","path":"tools","types":{"agents":false,"skills":true},"include_repo_items":false,
            "ignore_add":["drafts"],"ignore_allow":["demo"],"lanes":{"skills":"kit","agents":"kit-agents"}}]}"#;
        let cfg = Config::parse(text).unwrap();
        assert_eq!(cfg.global().ignore_folders, ["demo"]);
        let source = &cfg.sources()[0];
        assert_eq!((source.types.agents, source.types.instructions, source.include_repo_items), (Some(false), None, Some(false)));
        assert_eq!(
            cfg.repos().iter().map(|r| (r.id.as_str(), r.kind)).collect::<Vec<_>>(),
            [("kit", SourceKind::Skills), ("kit-instructions", SourceKind::Instructions)],
            "agents off here, plugins off globally, and a missing lane ID is given on load"
        );
        assert!(cfg.is_known_lane("kit-agents"), "an off lane is still configured");
        let json = cfg.to_json();
        assert!(!json.contains("\"kind\""), "{json}");
        assert_eq!(Config::parse(json.as_bytes()).unwrap(), cfg);
    }

    #[test]
    fn app_update_is_optional_in_v1_and_non_defaults_round_trip() {
        let legacy = Config::parse(br#"{"version":1,"repos":[{"repo":"o/r"}]}"#).unwrap();
        assert_eq!(legacy.app_update, UpdatePrefs { enabled: true, interval: UpdateInterval::Daily, timing: UpdateTiming::Immediate });
        let json: serde_json::Value = serde_json::from_str(&legacy.to_json()).unwrap();
        assert_eq!(json["version"], 3, "every save writes format 3");
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
        assert!(cfg.repos()[0].enabled && !cfg.repos()[0].use_token);
        assert_eq!(cfg.repos()[0].id, "a-b");
        assert_eq!(cfg.repos()[1].id, "a-b-2");
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
    fn legacy_typed_entries_migrate_to_sources_that_keep_every_lane_id() {
        let dir = temp_dir("typed");
        let path = dir.join(CONFIG_FILE);
        let v2 = br#"{"version":2,"repos":[
            {"id":"tools","repo":"o/r","branch":"main","path":"x"},
            {"id":"tools-ag","repo":"o/r","branch":"main","path":"x","kind":"agents"},
            {"id":"again","repo":"o/r","branch":"main","path":"x","kind":"agents"},
            {"id":"other","repo":"o/r","branch":"main","path":"x","kind":"instructions","use_token":true}]}"#;
        let cfg = Config::parse(v2).unwrap();
        let ids = |c: &Config| c.sources().iter().map(|s| s.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&cfg), ["tools", "again", "other"], "same location merges; the same kind twice and a different token stay apart");
        let tools = &cfg.sources()[0];
        assert_eq!((tools.lanes.id(SourceKind::Skills), tools.lanes.id(SourceKind::Agents)), (Some("tools"), Some("tools-ag")));
        assert_eq!((tools.types.instructions, tools.types.plugins), (Some(false), Some(false)), "other types are off explicitly");
        let lanes = cfg.repos().iter().map(|r| (r.id.as_str(), r.kind, r.source.as_str())).collect::<Vec<_>>();
        assert_eq!(
            lanes,
            [
                ("tools", SourceKind::Skills, "tools"),
                ("tools-ag", SourceKind::Agents, "tools"),
                ("again", SourceKind::Agents, "again"),
                ("other", SourceKind::Instructions, "other")
            ]
        );
        let off = tools.lanes.id(SourceKind::Instructions).unwrap().to_owned();
        assert!(cfg.is_known_lane(&off) && cfg.find(&off).is_none(), "an off lane is known, never run");

        cfg.save(&path).unwrap();
        let bytes = fs::read(&path).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["version"], 3);
        assert!(json.get("repos").is_none() && !String::from_utf8_lossy(&bytes).contains("\"kind\""));
        assert!(!v1_reader::accepts_local(&bytes), "an older app refuses format 3");
        assert_eq!(Config::load(&path).unwrap(), cfg, "mixed types load normally now");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn lanes_follow_every_change_to_their_source() {
        let mut cfg = Config::parse(br#"{"version":2,"repos":[{"id":"s","repo":"o/r"},{"id":"s-ag","repo":"o/r","kind":"agents"}]}"#).unwrap();
        let run = |c: &Config| c.repos().iter().map(|r| (r.id.clone(), r.enabled, r.use_token, r.interval)).collect::<Vec<_>>();
        cfg.set_enabled("s-ag", false);
        cfg.set_use_token("s", true);
        cfg.set_interval("s", Some(Interval::new(6, Unit::Hour)));
        let six = Some(Interval::new(6, Unit::Hour));
        assert_eq!(run(&cfg), [("s".to_owned(), false, true, six), ("s-ag".to_owned(), false, true, six)], "one source, one schedule");
        cfg.edit(|sources, _| sources[0].types.set(SourceKind::Skills, false));
        assert_eq!(run(&cfg).len(), 1, "a type turned off leaves the run list at once");
        assert!(cfg.is_known_lane("s"));
        cfg.edit(|sources, global| {
            sources[0].types.agents = None;
            global.types.agents = false;
        });
        assert!(cfg.repos().is_empty() && cfg.is_known_lane("s-ag"), "an inherited global switch also applies");
        assert_eq!(Config::parse(cfg.to_json().as_bytes()).unwrap(), cfg);
        cfg.remove("s-ag");
        assert!(cfg.sources().is_empty() && !cfg.is_known_lane("s"));
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
    fn a_versionless_file_with_format_3_fields_is_refused_and_never_overwritten() {
        let dir = temp_dir("versionless-v3");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(CONFIG_FILE);
        let mut problems = Vec::new();
        for v3 in [r#"{"sources":[{"id":"kit","repo":"o/kit"}]}"#, r#"{"global":{"include_repo_items":true}}"#] {
            fs::write(&path, v3).unwrap();
            match Config::load_sources(&path) {
                Ok(cfg) => problems.push(format!("{v3} loaded with {} sources instead of being refused", cfg.sources().len())),
                Err(e) if !e.contains("version") => problems.push(format!("{v3}: the error does not say the file needs a \"version\": {e}")),
                Err(_) => {}
            }
            if Config::default().save(&path).is_ok() || fs::read(&path).unwrap() != v3.as_bytes() {
                problems.push(format!("{v3} was overwritten by a save"));
            }
        }
        fs::remove_dir_all(dir).unwrap();
        assert!(problems.is_empty(), "a versionless file with format 3 fields must be refused, not read as an empty format 1 file:\n{}", problems.join("\n"));
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
        assert_eq!(Config::load(&file).unwrap().repos()[0].id, "a-b", "a hand-edited file with a byte order mark");

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
        let shared = Config::parse(br#"{ "repos": [ { "id": "x", "repo": "a/b" }, { "id": "x", "repo": "c/d" } ] }"#).unwrap_err();
        assert!(shared.contains("share the ID"), "{shared}");
        let cfg = Config::parse(br#"{ "default_interval": "1m", "repos": [ { "id": "x", "repo": "nope" }, { "id": "y", "repo": "a/b", "interval": "2m", "path": "../x", "branch": "a b" } ] }"#).unwrap();
        let problems = cfg.problems();
        assert!(problems.iter().any(|p| p.contains("default interval")), "{problems:?}");
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
