//! Portable companion-file configuration.
//!
//! The CLI or shell should call [`load`] with the executable's directory. It returns `None` when
//! neither companion file exists, or one merged [`Config`](crate::config::Config) when portable mode
//! is selected. Use [`save_changes`] with the previously loaded and edited configs to write only the
//! changed part. Resolve skills paths with [`resolve_skills_dir`], passing environment inputs from
//! the caller so portable path expansion is testable and never changes the stored setting.
//! Cooperating writers and exports lock the same directory through comparison and publication. This is
//! neither a two-file transaction nor a lock across independently synchronized cloud replicas.

use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{
    CONFIG_VERSION, Config, DEFAULT_INTERVAL, RepoConfig, UpdatePrefs, parse_json_object, read_optional, settings_write_lock, write_json_atomic,
};
use crate::interval::Interval;

pub const SETTINGS_FILE: &str = "tiny-skill-sync.settings.json";
pub const SOURCES_FILE: &str = "tiny-skill-sync.sources.json";
pub const SETTINGS_VERSION: u32 = 1;
pub const SOURCES_VERSION: u32 = 1;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WriteResult {
    pub settings_written: bool,
    pub sources_written: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    version: u32,
    #[serde(default = "default_interval")]
    default_interval: Interval,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    skills_dir: Option<String>,
    #[serde(default, deserialize_with = "UpdatePrefs::read_object", skip_serializing_if = "UpdatePrefs::is_default")]
    app_update: UpdatePrefs,
}

fn default_interval() -> Interval {
    DEFAULT_INTERVAL
}

impl Default for Settings {
    fn default() -> Self {
        Settings { version: SETTINGS_VERSION, default_interval: DEFAULT_INTERVAL, skills_dir: None, app_update: UpdatePrefs::default() }
    }
}

impl Settings {
    fn from_config(config: &Config) -> Settings {
        Settings { version: SETTINGS_VERSION, default_interval: config.default_interval, skills_dir: config.skills_dir.clone(), app_update: config.app_update }
    }

    fn validate_version(&mut self, file: &str) -> Result<(), String> {
        validate_version(file, self.version, SETTINGS_VERSION)?;
        self.version = SETTINGS_VERSION;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Sources {
    version: u32,
    #[serde(default)]
    repos: Vec<RepoConfig>,
}

impl Default for Sources {
    fn default() -> Self {
        Sources { version: SOURCES_VERSION, repos: Vec::new() }
    }
}

impl Sources {
    fn from_config(config: &Config) -> Sources {
        Sources { version: SOURCES_VERSION, repos: config.repos.clone() }
    }

    fn validate_version(&mut self, file: &str) -> Result<(), String> {
        validate_version(file, self.version, SOURCES_VERSION)?;
        self.version = SOURCES_VERSION;
        Ok(())
    }
}

/// Exports a local configuration without replacing either companion file. The skills folder is
/// deliberately omitted because its default belongs to each machine.
pub fn export(directory: &Path, config: &Config) -> Result<(), String> {
    let _lock = settings_write_lock(directory)?;
    check_export_target(directory)?;
    let settings_path = directory.join(SETTINGS_FILE);
    let sources_path = directory.join(SOURCES_FILE);
    let settings = Settings { skills_dir: None, ..Settings::from_config(config) };
    let sources = Sources::from_config(config);
    let settings_bytes = serde_json::to_vec_pretty(&settings).expect("portable settings serialize");
    let sources_bytes = serde_json::to_vec_pretty(&sources).expect("portable sources serialize");
    let settings = write_new(&settings_path, &settings_bytes).map_err(|error| format!("cannot create {}: {error}", settings_path.display()))?;
    if let Err(error) = write_new(&sources_path, &sources_bytes) {
        let _ = crate::win::install::delete(&settings);
        return Err(format!("cannot create {}: {error}", sources_path.display()));
    }
    Ok(())
}

/// Refuses export before the local configuration is read, and is repeated by [`export`] to close
/// the race between this check and the exclusive file creations.
pub fn check_export_target(directory: &Path) -> Result<(), String> {
    for path in [directory.join(SETTINGS_FILE), directory.join(SOURCES_FILE)] {
        match fs::symlink_metadata(&path) {
            Ok(_) => return Err(format!("{} already exists; refusing to overwrite it", path.display())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("cannot check {}: {error}", path.display())),
        }
    }
    Ok(())
}

fn write_new(path: &Path, bytes: &[u8]) -> io::Result<fs::File> {
    let mut file = crate::win::install::create(path)?;
    if let Err(e) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        let _ = crate::win::install::delete(&file);
        return Err(e);
    }
    Ok(file)
}

fn validate_version(file: &str, version: u32, current: u32) -> Result<(), String> {
    if version > current {
        Err(format!("{file} was written by a newer version (format {version}, this app understands {current})"))
    } else if version < current {
        Err(format!("{file} uses an unsupported format version ({version})"))
    } else {
        Ok(())
    }
}

fn parse_settings(bytes: &[u8], path: &Path) -> Result<Settings, String> {
    let mut settings: Settings = parse_json_object(bytes).map_err(|error| format!("{} is not valid: {error} ({})", SETTINGS_FILE, path.display()))?;
    settings.validate_version(SETTINGS_FILE).map_err(|error| format!("{error} ({})", path.display()))?;
    Ok(settings)
}

fn parse_sources(bytes: &[u8], path: &Path) -> Result<Sources, String> {
    let mut sources: Sources = parse_json_object(bytes).map_err(|error| format!("{} is not valid: {error} ({})", SOURCES_FILE, path.display()))?;
    sources.validate_version(SOURCES_FILE).map_err(|error| format!("{error} ({})", path.display()))?;
    let mut config = Config { repos: sources.repos, ..Config::default() };
    config.assign_missing_ids();
    sources.repos = config.repos;
    Ok(sources)
}

/// Loads portable companions beside `directory`. `Ok(None)` means neither exists and local mode
/// should remain active; a present but unreadable, malformed, unknown-key, or unsupported file is
/// an error and must not be replaced.
pub fn load(directory: &Path) -> Result<Option<Config>, String> {
    let settings_path = directory.join(SETTINGS_FILE);
    let sources_path = directory.join(SOURCES_FILE);
    let settings_bytes = read_optional(&settings_path)?;
    let sources_bytes = read_optional(&sources_path)?;
    if settings_bytes.is_none() && sources_bytes.is_none() {
        return Ok(None);
    }

    let settings = settings_bytes.as_deref().map(|bytes| parse_settings(bytes, &settings_path)).transpose()?.unwrap_or_default();
    let mut sources = sources_bytes.as_deref().map(|bytes| parse_sources(bytes, &sources_path)).transpose()?.unwrap_or_default();

    let mut config = Config {
        version: CONFIG_VERSION,
        skills_dir: settings.skills_dir,
        default_interval: settings.default_interval,
        app_update: settings.app_update,
        repos: std::mem::take(&mut sources.repos),
    };
    config.assign_missing_ids();
    Ok(Some(config))
}

/// Writes the settings and repository portions whose values changed from `before` to `after`.
/// Existing companions are parsed before any write, and a changed file must still match `before`;
/// this prevents malformed or concurrently edited settings from being overwritten.
pub fn save_changes(directory: &Path, before: &Config, after: &Config) -> Result<WriteResult, String> {
    let settings_path = directory.join(SETTINGS_FILE);
    let sources_path = directory.join(SOURCES_FILE);
    let _lock = (before != after).then(|| settings_write_lock(directory)).transpose()?;
    let settings_bytes = read_optional(&settings_path)?;
    let sources_bytes = read_optional(&sources_path)?;
    if settings_bytes.is_none() && sources_bytes.is_none() {
        return Err("portable settings disappeared since they were loaded; reload settings and try again".into());
    }
    let disk_settings = settings_bytes.as_deref().map(|bytes| parse_settings(bytes, &settings_path)).transpose()?.unwrap_or_default();
    let disk_sources = sources_bytes.as_deref().map(|bytes| parse_sources(bytes, &sources_path)).transpose()?.unwrap_or_default();

    let old_settings = Settings::from_config(before);
    let new_settings = Settings::from_config(after);
    let old_sources = Sources::from_config(before);
    let new_sources = Sources::from_config(after);
    let settings_changed = old_settings != new_settings;
    let sources_changed = old_sources != new_sources;

    if settings_changed && disk_settings != old_settings {
        return Err(format!("{} changed since it was loaded ({})", SETTINGS_FILE, settings_path.display()));
    }
    if sources_changed && disk_sources != old_sources {
        return Err(format!("{} changed since it was loaded ({})", SOURCES_FILE, sources_path.display()));
    }

    if settings_changed {
        let json = serde_json::to_vec_pretty(&new_settings).expect("portable settings serialize");
        write_json_atomic(&settings_path, &json).map_err(|error| format!("cannot save {}: {error}", settings_path.display()))?;
    }
    if sources_changed {
        let json = serde_json::to_vec_pretty(&new_sources).expect("portable sources serialize");
        write_json_atomic(&sources_path, &json).map_err(|error| format!("cannot save {}: {error}", sources_path.display()))?;
    }

    Ok(WriteResult { settings_written: settings_changed, sources_written: sources_changed })
}

/// Resolves the effective skills folder using caller-supplied environment inputs.
///
/// `skills_override` corresponds to `TSS_SKILLS_DIR` and is returned without expansion. `user_profile`
/// supplies the machine-local base for the default. `variable` resolves `%NAME%` references in the
/// portable setting; returning `None` for a name is an error. Local `config.json` parsing is not
/// changed and should continue to use [`crate::config::Paths::skills_dir`].
pub fn resolve_skills_dir(
    config: &Config,
    skills_override: Option<&OsStr>,
    user_profile: Option<&Path>,
    variable: impl Fn(&str) -> Option<String>,
) -> Result<PathBuf, String> {
    if let Some(path) = skills_override.filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path));
    }

    if let Some(raw) = config.skills_dir.as_deref().map(str::trim).filter(|path| !path.is_empty()) {
        let expanded = expand_variables(raw, variable)?;
        let path = PathBuf::from(expanded);
        if !path.is_absolute() {
            return Err(format!("the expanded skills folder must be a full path; '{}' is not one", path.display()));
        }
        return Ok(path);
    }

    let profile = user_profile.ok_or_else(|| "USERPROFILE is not set; cannot resolve the default skills folder".to_string())?;
    let path = profile.join(".copilot").join("skills");
    if !path.is_absolute() {
        return Err(format!("USERPROFILE must be a full path to resolve the default skills folder; '{}' is not one", path.display()));
    }
    Ok(path)
}

fn expand_variables(raw: &str, variable: impl Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut expanded = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(open) = rest.find('%') {
        expanded.push_str(&rest[..open]);
        let after_open = &rest[open + 1..];
        let close = after_open.find('%').ok_or_else(|| "skills folder contains an unmatched '%'".to_string())?;
        let name = &after_open[..close];
        if name.is_empty() {
            return Err("skills folder contains an empty environment variable".to_string());
        }
        let value = variable(name).ok_or_else(|| format!("unknown environment variable '%{name}%' in skills folder"))?;
        expanded.push_str(&value);
        rest = &after_open[close + 1..];
    }
    expanded.push_str(rest);
    Ok(expanded)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;

    use crate::config::{Config, RepoConfig, RepoSpec, UpdateInterval, UpdateTiming};
    use crate::interval::{Interval, Unit};

    fn temp_dir(label: &str) -> PathBuf {
        let base = std::env::var_os("TSS_TEST_ROOT").filter(|s| !s.is_empty()).map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let dir = base.join(format!("tss-portable-{label}-{}", crate::fsx::unique_suffix()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_json(dir: &Path, file: &str, json: &str) {
        fs::write(dir.join(file), json).unwrap();
    }

    #[test]
    fn missing_files_select_local_mode_and_one_file_supplies_defaults() {
        let dir = temp_dir("missing");
        assert_eq!(load(&dir).unwrap(), None);

        write_json(&dir, SETTINGS_FILE, r#"{"version":1,"default_interval":"12h","skills_dir":null}"#);
        let config = load(&dir).unwrap().unwrap();
        assert_eq!(config.default_interval, Interval::new(12, Unit::Hour));
        assert_eq!(config.app_update, UpdatePrefs::default());
        assert!(config.repos.is_empty());
        assert_eq!(config.skills_dir, None);

        fs::remove_file(dir.join(SETTINGS_FILE)).unwrap();
        write_json(&dir, SOURCES_FILE, r#"{"version":1,"repos":[{"id":"shared-id","repo":"o/r"}]}"#);
        let config = load(&dir).unwrap().unwrap();
        assert_eq!(config.default_interval, DEFAULT_INTERVAL);
        assert_eq!(config.app_update, UpdatePrefs::default());
        assert_eq!(config.skills_dir, None);
        assert_eq!(config.repos[0].id, "shared-id");

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn portable_files_merge_and_preserve_repository_ids_and_settings() {
        let dir = temp_dir("merge");
        write_json(&dir, SETTINGS_FILE, r#"{"version":1,"default_interval":"1d","skills_dir":"%USERPROFILE%\\.copilot\\skills"}"#);
        write_json(
            &dir,
            SOURCES_FILE,
            r#"{"version":1,"repos":[{"id":"stable-id","repo":"o/r","branch":"main","path":"skills","interval":"6h","use_token":true,"enabled":false}]}"#,
        );

        let config = load(&dir).unwrap().unwrap();
        assert_eq!(config.repos[0].id, "stable-id");
        assert_eq!(config.repos[0].repo, "o/r");
        assert_eq!(config.repos[0].interval, Some(Interval::new(6, Unit::Hour)));
        assert!(!config.repos[0].enabled);
        assert!(config.repos[0].use_token);
        assert_eq!(config.skills_dir.as_deref(), Some(r"%USERPROFILE%\.copilot\skills"));

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn malformed_newer_unsupported_and_unknown_top_level_files_are_errors() {
        let cases = [
            (SETTINGS_FILE, "{ broken"),
            (SETTINGS_FILE, r#"{"default_interval":"1d"}"#),
            (SETTINGS_FILE, r#"{"version":99}"#),
            (SETTINGS_FILE, r#"{"version":0}"#),
            (SETTINGS_FILE, r#"{"unknown":true}"#),
            (SETTINGS_FILE, r#"{"version":1,"app_update":[]}"#),
            (SETTINGS_FILE, r#"{"version":1,"app_update":{"timing":"later"}}"#),
            (SOURCES_FILE, "{ broken"),
            (SOURCES_FILE, r#"{"repos":[]}"#),
            (SOURCES_FILE, r#"{"version":99}"#),
            (SOURCES_FILE, r#"{"version":0}"#),
            (SOURCES_FILE, r#"{"version":1,"unknown":true}"#),
            (SOURCES_FILE, r#"{"version":1,"app_update":{"enabled":false}}"#),
        ];
        for (index, (file, contents)) in cases.into_iter().enumerate() {
            let dir = temp_dir(&format!("invalid-{index}"));
            write_json(&dir, file, contents);
            assert!(load(&dir).is_err(), "{file}: {contents}");
            fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn failed_load_and_save_do_not_overwrite_malformed_present_files() {
        let dir = temp_dir("preserve-invalid");
        let original = br#"{"version":99}"#;
        fs::write(dir.join(SETTINGS_FILE), original).unwrap();
        assert!(load(&dir).is_err());
        let before = Config::default();
        let after = Config { default_interval: Interval::new(12, Unit::Hour), ..before.clone() };
        assert!(save_changes(&dir, &before, &after).is_err());
        assert_eq!(fs::read(dir.join(SETTINGS_FILE)).unwrap(), original);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn save_changes_writes_only_changed_part_and_keeps_other_bytes_untouched() {
        let dir = temp_dir("split-write");
        let mut before = Config::default();
        let repo = RepoSpec::parse("o/r").unwrap();
        before.add_repo(&repo, Some(Interval::new(6, Unit::Hour)), true);
        before.repos[0].id = "original-id".to_string();
        let sources = Sources::from_config(&before);
        fs::write(dir.join(SOURCES_FILE), serde_json::to_vec_pretty(&sources).unwrap()).unwrap();
        let source_bytes = fs::read(dir.join(SOURCES_FILE)).unwrap();

        let mut after = before.clone();
        after.default_interval = Interval::new(12, Unit::Hour);
        let written = save_changes(&dir, &before, &after).unwrap();

        assert_eq!(written, WriteResult { settings_written: true, sources_written: false });
        assert_eq!(fs::read(dir.join(SOURCES_FILE)).unwrap(), source_bytes);
        assert_eq!(load(&dir).unwrap().unwrap().default_interval, Interval::new(12, Unit::Hour));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn save_changes_leaves_a_logically_unchanged_file_unwritten() {
        let dir = temp_dir("no-op");
        write_json(&dir, SOURCES_FILE, r#"{"version":1,"repos":[]}"#);
        let before = Config::default();
        let mut after = before.clone();
        after.repos.push(RepoConfig { id: "kept-id".into(), repo: "o/r".into(), ..RepoConfig::default() });
        let written = save_changes(&dir, &before, &after).unwrap();
        assert_eq!(written, WriteResult { settings_written: false, sources_written: true });

        let source_bytes = fs::read(dir.join(SOURCES_FILE)).unwrap();
        let loaded = load(&dir).unwrap().unwrap();
        assert_eq!(save_changes(&dir, &loaded, &loaded).unwrap(), WriteResult::default());
        assert_eq!(fs::read(dir.join(SOURCES_FILE)).unwrap(), source_bytes);
        assert!(!dir.join(SETTINGS_FILE).exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn save_refuses_to_replace_a_part_changed_since_it_was_loaded() {
        let dir = temp_dir("stale");
        let before = Config::default();
        let after = Config { default_interval: Interval::new(12, Unit::Hour), ..before.clone() };
        write_json(&dir, SETTINGS_FILE, r#"{"version":1,"default_interval":"6h","skills_dir":null}"#);
        assert!(save_changes(&dir, &before, &after).is_err());
        assert!(load(&dir).unwrap().unwrap().default_interval == Interval::new(6, Unit::Hour));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn preference_changes_only_write_settings_and_participate_in_conflict_checks() {
        let dir = temp_dir("update-prefs");
        let sources = r#"{"version":1, "repos": [{"repo":"o/r"}]}"#;
        write_json(&dir, SOURCES_FILE, sources);
        let before = load(&dir).unwrap().unwrap();
        let after = Config { app_update: UpdatePrefs { enabled: false, interval: UpdateInterval::Weekly, timing: UpdateTiming::OnQuit }, ..before.clone() };
        let written = save_changes(&dir, &before, &after).unwrap();
        assert_eq!(written, WriteResult { settings_written: true, sources_written: false });
        assert_eq!(fs::read(dir.join(SOURCES_FILE)).unwrap(), sources.as_bytes());
        assert_eq!(load(&dir).unwrap().unwrap(), after);

        let stale = Config { default_interval: Interval::new(6, Unit::Hour), ..before.clone() };
        assert!(save_changes(&dir, &before, &stale).unwrap_err().contains("changed since it was loaded"));
        assert_eq!(load(&dir).unwrap().unwrap(), after);
        assert_eq!(fs::read(dir.join(SOURCES_FILE)).unwrap(), sources.as_bytes());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn portable_writers_and_export_share_exclusion_but_readers_do_not() {
        let dir = temp_dir("writer");
        let lock = dir.join(".tss-settings.lock");
        fs::write(&lock, b"someone else's file").unwrap();
        let before = Config::default();
        let after = Config { app_update: UpdatePrefs { enabled: false, ..before.app_update }, ..before.clone() };
        assert_eq!(load(&dir).unwrap(), None, "a lock alone does not select portable mode");
        assert!(export(&dir, &after).is_err());
        assert!(!dir.join(SETTINGS_FILE).exists() && !dir.join(SOURCES_FILE).exists());
        write_json(&dir, SETTINGS_FILE, r#"{"version":1}"#);
        assert!(save_changes(&dir, &before, &after).is_err());
        assert_eq!(load(&dir).unwrap(), Some(before.clone()));
        assert_eq!(fs::read(&lock).unwrap(), b"someone else's file");

        fs::remove_file(&lock).unwrap();
        let held = settings_write_lock(&dir).unwrap();
        assert!(save_changes(&dir, &before, &after).is_err());
        assert_eq!(load(&dir).unwrap(), Some(before.clone()));
        drop(held);
        assert!(save_changes(&dir, &before, &after).unwrap().settings_written);
        assert!(!lock.exists() && !dir.join(SOURCES_FILE).exists(), "no extra companion on success");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_stale_sources_only_session_cannot_recreate_portable_mode_over_local_off() {
        let dir = temp_dir("missing-both");
        let local = dir.join(crate::config::CONFIG_FILE);
        write_json(&dir, SOURCES_FILE, r#"{"version":1,"repos":[]}"#);
        let before = load(&dir).unwrap().unwrap();
        let after = Config { default_interval: Interval::new(6, Unit::Hour), ..before.clone() };
        fs::remove_file(dir.join(SOURCES_FILE)).unwrap();
        fs::write(&local, br#"{"app_update":{"enabled":false}}"#).unwrap();
        assert!(save_changes(&dir, &before, &after).is_err());
        assert_eq!(load(&dir).unwrap(), None);
        assert!(!Config::load(&local).unwrap().app_update.enabled);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_present_but_unreadable_companion_or_local_config_is_not_missing() {
        use std::os::windows::fs::{OpenOptionsExt, symlink_file};
        let dir = temp_dir("dangling");
        let missing = dir.join("absent");
        let link = dir.join(SETTINGS_FILE);
        write_json(&dir, SETTINGS_FILE, r#"{"version":1}"#);
        let held = fs::OpenOptions::new().read(true).share_mode(0).open(&link).unwrap();
        assert!(load(&dir).is_err());
        assert!(Config::load(&link).is_err());
        drop(held);
        fs::remove_file(&link).unwrap();
        // Creating a file symlink needs the privilege or developer mode, and a supporting test drive.
        if symlink_file(&missing, &link).is_ok() {
            assert!(load(&dir).is_err());
            assert!(Config::load(&link).is_err());
            assert!(Config::default().save(&link).is_err());
            assert!(fs::symlink_metadata(&link).is_ok());
            fs::remove_file(&link).unwrap();
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn export_copies_only_nondefault_app_preferences_and_omits_the_local_skills_path() {
        for prefs in [UpdatePrefs::default(), UpdatePrefs { enabled: false, interval: UpdateInterval::Weekly, timing: UpdateTiming::OnQuit }] {
            let dir = temp_dir("export-updates");
            let local = Config { skills_dir: Some(r"C:\local-skills".into()), app_update: prefs, ..Config::default() };
            export(&dir, &local).unwrap();
            let settings: serde_json::Value = serde_json::from_slice(&fs::read(dir.join(SETTINGS_FILE)).unwrap()).unwrap();
            assert_eq!(settings["version"], 1);
            assert!(settings.get("skills_dir").is_none());
            if prefs == UpdatePrefs::default() {
                assert!(settings.get("app_update").is_none(), "must remain readable by strict v1 readers");
            } else {
                assert_eq!(settings["app_update"], serde_json::json!({"enabled":false,"interval":"weekly","timing":"on_quit"}));
            }
            assert_eq!(load(&dir).unwrap().unwrap().app_update, prefs);
            fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn skills_path_expansion_uses_injected_values_and_keeps_the_raw_setting() {
        let raw = r"%USERPROFILE%\portable skills";
        let config = Config { skills_dir: Some(raw.into()), ..Config::default() };
        let path = resolve_skills_dir(&config, None, None, |name| (name == "USERPROFILE").then(|| r"C:\Users\other".into())).unwrap();
        assert_eq!(path, PathBuf::from(r"C:\Users\other\portable skills"));
        assert_eq!(config.skills_dir.as_deref(), Some(raw));
    }

    #[test]
    fn skills_path_rejects_unknown_variables_unmatched_percent_and_relative_expansions() {
        let config = Config { skills_dir: Some(r"%MISSING%\skills".into()), ..Config::default() };
        assert!(resolve_skills_dir(&config, None, None, |_| None).unwrap_err().contains("MISSING"));

        let config = Config { skills_dir: Some(r"C:\skills\%BROKEN".into()), ..Config::default() };
        assert!(resolve_skills_dir(&config, None, None, |_| None).is_err());

        let config = Config { skills_dir: Some(r"%ROOT%\skills".into()), ..Config::default() };
        assert!(resolve_skills_dir(&config, None, None, |_| Some("relative".into())).unwrap_err().contains("full path"));
    }

    #[test]
    fn skills_override_is_not_expanded_and_default_uses_injected_profile() {
        let config = Config { skills_dir: Some(r"%ROOT%\skills".into()), ..Config::default() };
        let override_dir = std::ffi::OsStr::new(r"%ROOT%\override");
        assert_eq!(resolve_skills_dir(&config, Some(override_dir), None, |_| None).unwrap(), PathBuf::from(r"%ROOT%\override"));

        let defaults = Config::default();
        assert_eq!(
            resolve_skills_dir(&defaults, None, Some(Path::new(r"D:\Profiles\person")), |_| None).unwrap(),
            PathBuf::from(r"D:\Profiles\person\.copilot\skills")
        );
    }
}
