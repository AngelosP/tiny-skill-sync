//! Location-based settings. This module does not read settings, change state or approve plugins.

use std::collections::{BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::config::{RepoConfig, assign_legacy_ids};
use crate::interval::Interval;
use crate::inventory::Policy;
use crate::pathsafe;
use crate::source::SourceKind::{self, Agents, Instructions, Plugins, Skills};

pub const CONFIG_VERSION: u32 = 3;
const KINDS: [SourceKind; 4] = [Skills, Agents, Instructions, Plugins];
const DEFAULT_IGNORE: [&str; 6] = ["sample", "example", "samples", "examples", "test", "tests"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Types {
    #[serde(skip_serializing_if = "is_true")]
    pub skills: bool,
    #[serde(skip_serializing_if = "is_true")]
    pub agents: bool,
    #[serde(skip_serializing_if = "is_true")]
    pub instructions: bool,
    #[serde(skip_serializing_if = "is_true")]
    pub plugins: bool,
}

impl Default for Types {
    fn default() -> Self {
        Self { skills: true, agents: true, instructions: true, plugins: true }
    }
}

impl Types {
    pub fn enabled(&self, kind: SourceKind) -> bool {
        match kind {
            Skills => self.skills,
            Agents => self.agents,
            Instructions => self.instructions,
            Plugins => self.plugins,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TypeOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agents: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugins: Option<bool>,
}

impl TypeOverrides {
    fn field(&mut self, kind: SourceKind) -> &mut Option<bool> {
        match kind {
            Skills => &mut self.skills,
            Agents => &mut self.agents,
            Instructions => &mut self.instructions,
            Plugins => &mut self.plugins,
        }
    }

    pub fn get(&self, kind: SourceKind) -> Option<bool> {
        match kind {
            Skills => self.skills,
            Agents => self.agents,
            Instructions => self.instructions,
            Plugins => self.plugins,
        }
    }

    pub fn set(&mut self, kind: SourceKind, enabled: bool) {
        *self.field(kind) = Some(enabled);
    }
}

/// What to do with one folder name in a source's own ignore lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IgnoreAction {
    /// Ignore it for this source too.
    Add,
    /// Never ignore it for this source, even when the global list has it.
    Allow,
    /// Follow the global list again.
    Remove,
}

/// Why a folder name typed by the user cannot be used in an ignore list.
pub fn check_folder_name(name: &str) -> Result<(), String> {
    validate_ignore_names(&[name.to_owned()]).map_err(|why| format!("That folder name cannot be used: {why}"))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GlobalSettings {
    #[serde(skip_serializing_if = "is_default")]
    pub types: Types,
    #[serde(skip_serializing_if = "is_default")]
    pub include_repo_items: bool,
    #[serde(skip_serializing_if = "default_ignore")]
    pub ignore_folders: Vec<String>,
}

impl Default for GlobalSettings {
    fn default() -> Self {
        Self { types: Types::default(), include_repo_items: false, ignore_folders: DEFAULT_IGNORE.map(str::to_owned).into() }
    }
}

impl GlobalSettings {
    /// Turn a type on or off for every source that follows the defaults. True when it was on and is now off.
    pub fn set_type(&mut self, kind: SourceKind, on: bool) -> bool {
        let field = match kind {
            Skills => &mut self.types.skills,
            Agents => &mut self.types.agents,
            Instructions => &mut self.types.instructions,
            Plugins => &mut self.types.plugins,
        };
        let turned_off = *field && !on;
        *field = on;
        turned_off
    }

    /// Add a folder name to the global ignore list, or take it out.
    pub fn ignore(&mut self, add: bool, name: &str) -> Result<(), String> {
        check_folder_name(name)?;
        let key = pathsafe::fold(name);
        if !add {
            self.ignore_folders.retain(|n| pathsafe::fold(n) != key);
        } else if !self.ignore_folders.iter().any(|n| pathsafe::fold(n) == key) {
            self.ignore_folders.push(name.to_owned());
        }
        Ok(())
    }

    pub fn set_repo_items(&mut self, on: bool) {
        self.include_repo_items = on;
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Lanes {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agents: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugins: Option<String>,
}

impl Lanes {
    fn key(&mut self, kind: SourceKind) -> &mut Option<String> {
        match kind {
            Skills => &mut self.skills,
            Agents => &mut self.agents,
            Instructions => &mut self.instructions,
            Plugins => &mut self.plugins,
        }
    }

    pub fn id(&self, kind: SourceKind) -> Option<&str> {
        match kind {
            Skills => self.skills.as_deref(),
            Agents => self.agents.as_deref(),
            Instructions => self.instructions.as_deref(),
            Plugins => self.plugins.as_deref(),
        }
    }

    pub fn with(mut self, kind: SourceKind, id: String) -> Self {
        *self.key(kind) = Some(id);
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SourceConfig {
    pub id: String,
    pub repo: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub branch: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval: Option<Interval>,
    #[serde(skip_serializing_if = "is_default")]
    pub use_token: bool,
    #[serde(skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(skip_serializing_if = "is_default")]
    pub types: TypeOverrides,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_repo_items: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ignore_add: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ignore_allow: Vec<String>,
    #[serde(skip_serializing_if = "is_default")]
    pub lanes: Lanes,
}

impl Default for SourceConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            repo: String::new(),
            branch: String::new(),
            path: String::new(),
            interval: None,
            use_token: false,
            enabled: true,
            types: TypeOverrides::default(),
            include_repo_items: None,
            ignore_add: Vec::new(),
            ignore_allow: Vec::new(),
            lanes: Lanes::default(),
        }
    }
}

impl SourceConfig {
    pub fn effective_types(&self, global: &GlobalSettings) -> Types {
        Types {
            skills: self.types.skills.unwrap_or(global.types.skills),
            agents: self.types.agents.unwrap_or(global.types.agents),
            instructions: self.types.instructions.unwrap_or(global.types.instructions),
            plugins: self.types.plugins.unwrap_or(global.types.plugins),
        }
    }

    pub fn effective_policy(&self, global: &GlobalSettings) -> Result<Policy, String> {
        for (field, names) in [("ignore_folders", &global.ignore_folders), ("ignore_add", &self.ignore_add), ("ignore_allow", &self.ignore_allow)] {
            validate_ignore_names(names).map_err(|why| format!("{field}: {why}"))?;
        }
        let mut ignore: BTreeSet<_> = global.ignore_folders.iter().chain(&self.ignore_add).map(|name| pathsafe::fold(name)).collect();
        for name in &self.ignore_allow {
            ignore.remove(&pathsafe::fold(name));
        }
        Ok(Policy { include_repo_items: self.include_repo_items.unwrap_or(global.include_repo_items), ignore_folders: ignore })
    }

    /// Turn a type on or off for this source; `None` follows the global setting again.
    pub fn set_type(&mut self, kind: SourceKind, value: Option<bool>) {
        *self.types.field(kind) = value;
    }

    /// Change this source's own ignore lists. A name is in at most one of them.
    pub fn ignore(&mut self, action: IgnoreAction, name: &str) -> Result<(), String> {
        check_folder_name(name)?;
        let key = pathsafe::fold(name);
        let has = |list: &Vec<String>| list.iter().any(|n| pathsafe::fold(n) == key);
        match action {
            IgnoreAction::Add if !has(&self.ignore_add) => self.ignore_add.push(name.to_owned()),
            IgnoreAction::Allow if !has(&self.ignore_allow) => self.ignore_allow.push(name.to_owned()),
            _ => {}
        }
        if action != IgnoreAction::Add {
            self.ignore_add.retain(|n| pathsafe::fold(n) != key);
        }
        if action != IgnoreAction::Allow {
            self.ignore_allow.retain(|n| pathsafe::fold(n) != key);
        }
        Ok(())
    }

    /// Whether skills and agents in repository-specific folders sync; `None` follows the global setting again.
    pub fn set_repo_items(&mut self, value: Option<bool>) {
        self.include_repo_items = value;
    }
}

pub fn validate_ignore_names(names: &[String]) -> Result<(), String> {
    let mut seen = HashSet::new();
    for name in names {
        pathsafe::check_component(name).map_err(|why| format!("'{}' is not a single folder name: {why}", pathsafe::printable(name)))?;
        if !seen.insert(pathsafe::fold(name)) {
            return Err(format!("Folder '{}' is already in this list.", pathsafe::printable(name)));
        }
    }
    Ok(())
}

/// Reserve a unique source ID and all of its lanes for a brand-new source. Pass all existing source IDs and lane IDs,
/// and any state record IDs: a new source must not inherit a record. Never use this for a source that already exists.
/// The skills lane may share the ID of its own source, never the ID of another source or lane.
pub fn allocate_source_ids(base: &str, reserved: &mut HashSet<String>) -> (String, Lanes) {
    let base = if base.trim().is_empty() { "repo" } else { base };
    let id = reserve(base, reserved);
    let mut lanes = Lanes { skills: Some(id.clone()), ..Lanes::default() };
    fill_lanes(&id, &mut lanes, reserved);
    (id, lanes)
}

fn reserve(base: &str, reserved: &mut HashSet<String>) -> String {
    let mut id = base.to_owned();
    for suffix in 2.. {
        if reserved.insert(id.clone()) {
            return id;
        }
        id = format!("{base}-{suffix}");
    }
    unreachable!()
}

fn fill_lanes(id: &str, lanes: &mut Lanes, reserved: &mut HashSet<String>) {
    if lanes.skills.is_none() {
        lanes.skills = Some(if KINDS.iter().any(|&kind| lanes.id(kind) == Some(id)) { reserve(id, reserved) } else { id.to_owned() });
    }
    for (kind, suffix) in [(Agents, "agents"), (Instructions, "instructions"), (Plugins, "plugins")] {
        lanes.key(kind).get_or_insert_with(|| reserve(&format!("{id}-{suffix}"), reserved));
    }
}

/// Group v1/v2 entries in their original order, putting each into the first identical location without its kind yet.
/// Keep legacy IDs even for disabled sources. New lane IDs avoid the legacy IDs and `reserved_ids` (the other IDs
/// already in the settings); it blocks new allocations, never rewrites an existing ownership key. Pass only IDs from
/// the settings, never state records, so the same file migrates to the same IDs on every load.
pub fn migrate_legacy(repos: &[RepoConfig], reserved_ids: &HashSet<String>) -> Result<Vec<SourceConfig>, String> {
    let mut old = repos.to_vec();
    assign_legacy_ids(&mut old);
    let mut reserved = HashSet::new();
    for r in &old {
        if !reserved.insert(r.id.clone()) {
            return Err(format!("Cannot preserve ownership: two old sources share the ID '{}'.", pathsafe::printable(&r.id)));
        }
    }
    reserved.extend(reserved_ids.iter().cloned());
    let mut sources = Vec::new();
    let mut locations = HashMap::new();
    for r in &old {
        let interval_key = r.interval.map(|i| (i.amount, i.unit as u8));
        let location = (&r.repo, &r.branch, &r.path, r.use_token, interval_key, r.enabled);
        let (groups, occurrences): &mut (Vec<usize>, [usize; 4]) = locations.entry(location).or_default();
        let occurrence = &mut occurrences[r.kind as usize];
        if *occurrence == groups.len() {
            groups.push(sources.len());
            sources.push(SourceConfig {
                id: r.id.clone(),
                repo: r.repo.clone(),
                branch: r.branch.clone(),
                path: r.path.clone(),
                interval: r.interval,
                use_token: r.use_token,
                enabled: r.enabled,
                types: TypeOverrides { skills: Some(false), agents: Some(false), instructions: Some(false), plugins: Some(false) },
                ..SourceConfig::default()
            });
        }
        let source = &mut sources[groups[*occurrence]];
        source.types.set(r.kind, true);
        *source.lanes.key(r.kind) = Some(r.id.clone());
        *occurrence += 1;
    }
    for source in &mut sources {
        fill_lanes(&source.id, &mut source.lanes, &mut reserved);
    }
    Ok(sources)
}

/// Fill every missing lane ID, avoiding `reserved` (which gains the new IDs).
pub(crate) fn complete_lanes(sources: &mut [SourceConfig], reserved: &mut HashSet<String>) {
    for source in sources {
        fill_lanes(&source.id.clone(), &mut source.lanes, reserved);
    }
}

fn is_true(value: &bool) -> bool {
    *value
}

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

fn default_ignore(names: &[String]) -> bool {
    names == DEFAULT_IGNORE
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn omitted_settings_inherit_and_explicit_false_survives() {
        let global: GlobalSettings = serde_json::from_value(json!({"types":{"skills":false},"include_repo_items":true})).unwrap();
        let source: SourceConfig = serde_json::from_value(json!({
            "id":"my-source", "repo":"owner/repo", "types":{"skills":true,"plugins":false}, "include_repo_items":false,
            "ignore_add":["Demos","Samples"], "ignore_allow":["sAMPLES", "EXAMPLES"]
        }))
        .unwrap();
        assert_eq!(serde_json::to_value(GlobalSettings::default()).unwrap(), json!({}));
        assert_eq!(serde_json::to_value(&global).unwrap(), json!({"types":{"skills":false},"include_repo_items":true}));
        assert_eq!(source.effective_types(&global), Types { plugins: false, ..Types::default() });
        let wire = serde_json::to_value(&source).unwrap();
        assert_eq!(wire["types"], json!({"skills":true,"plugins":false}));
        assert_eq!(wire["include_repo_items"], false);
        assert!(!SourceConfig::default().effective_types(&global).skills);
        assert_eq!(serde_json::to_value(SourceConfig::default()).unwrap(), json!({"id":"", "repo":""}));
        let effective = source.effective_policy(&global).unwrap();
        assert!(!effective.include_repo_items);
        assert_eq!(effective.ignore_folders, ["demos", "example", "sample", "test", "tests"].map(str::to_owned).into());
    }

    #[test]
    fn invalid_or_duplicate_ignore_names_are_errors() {
        for list in [vec![""], vec!["."], vec!["x/y"], vec![r"x\y"], vec!["Test", "test"]] {
            assert!(validate_ignore_names(&list.into_iter().map(str::to_owned).collect::<Vec<_>>()).is_err());
        }
        let source = SourceConfig { ignore_allow: vec!["nested/folder".into()], ..Default::default() };
        assert!(source.effective_policy(&GlobalSettings::default()).unwrap_err().contains("ignore_allow"));
    }

    #[test]
    fn lane_allocation_reserves_ids_without_rederiving_old_lanes() {
        let mut reserved = ["foo", "foo-2-agents", "foo-2-agents-2", "foo-2-plugins"].map(str::to_owned).into();
        let (id, lanes) = allocate_source_ids("foo", &mut reserved);
        assert_eq!(id, "foo-2");
        assert_eq!(
            (lanes.id(Skills), lanes.id(Agents), lanes.id(Instructions), lanes.id(Plugins)),
            (Some("foo-2"), Some("foo-2-agents-3"), Some("foo-2-instructions"), Some("foo-2-plugins-2"))
        );
        let (next, _) = allocate_source_ids("foo-2-agents-3", &mut reserved);
        assert_eq!(next, "foo-2-agents-3-2");
        assert_eq!(serde_json::from_value::<Lanes>(serde_json::to_value(&lanes).unwrap()).unwrap(), lanes);
    }

    fn legacy(id: &str, kind: SourceKind) -> RepoConfig {
        RepoConfig { id: id.into(), repo: "owner/repo".into(), branch: "main".into(), path: "work".into(), kind, ..RepoConfig::default() }
    }

    #[test]
    fn migration_groups_distinct_kinds_but_preserves_duplicate_kinds_and_ids() {
        let missing_kind = serde_json::from_value(json!({"id":"first-skill", "repo":"owner/repo", "branch":"main", "path":"work"})).unwrap();
        let entries =
            [legacy("foo", Agents), missing_kind, legacy("second-skill", Skills), legacy("second-agent", Agents), legacy("foo-plugins", Instructions)];
        let sources = migrate_legacy(&entries, &HashSet::new()).unwrap();
        assert_eq!(sources.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["foo", "second-skill"]);
        assert_eq!(sources[0].lanes.id(Skills), Some("first-skill"));
        assert_eq!(sources[0].lanes.id(Agents), Some("foo"));
        assert_eq!(sources[0].lanes.id(Instructions), Some("foo-plugins"));
        assert_eq!(sources[0].lanes.id(Plugins), Some("foo-plugins-2"));
        assert_eq!(sources[1].lanes.id(Agents), Some("second-agent"));
        let wire = serde_json::to_value(&sources[1]).unwrap();
        assert_eq!(wire["types"], json!({"skills":true,"agents":true,"instructions":false,"plugins":false}));
        let mut all = HashSet::new();
        for source in sources {
            for kind in KINDS {
                assert!(all.insert(source.lanes.id(kind).unwrap().to_owned()));
            }
        }
    }

    #[test]
    fn migration_preserves_poll_semantics_and_rejects_duplicate_legacy_ids() {
        let one = legacy("agent", Agents);
        let other = legacy("other", Skills);
        let entries = [
            one.clone(),
            RepoConfig { id: "plugins".into(), kind: Plugins, enabled: false, ..other.clone() },
            RepoConfig { id: "interval".into(), interval: Some(Interval::parse("2d").unwrap()), ..other.clone() },
            RepoConfig { id: "auth".into(), use_token: true, ..other.clone() },
            RepoConfig { id: "branch".into(), branch: String::new(), ..other.clone() },
            RepoConfig { id: "path".into(), path: String::new(), ..other.clone() },
            RepoConfig { id: "repo".into(), repo: "owner/different".into(), ..other },
            RepoConfig { id: "agent-2".into(), ..legacy("", Agents) },
        ];
        let sources = migrate_legacy(&entries, &HashSet::new()).unwrap();
        assert_eq!(sources.len(), entries.len());
        assert_eq!(sources[0].lanes.id(Skills), Some("agent-3"));
        assert_eq!(serde_json::to_value(&sources[0]).unwrap()["types"], json!({"skills":false,"agents":true,"instructions":false,"plugins":false}));
        assert!(!sources[1].enabled);
        assert_eq!(sources[1].lanes.id(Plugins), Some("plugins"));
        assert_eq!(sources[3].use_token, entries[3].use_token);
        assert!(migrate_legacy(&[one.clone(), one], &HashSet::new()).unwrap_err().contains("preserve ownership"));
    }

    #[test]
    fn migration_reserves_other_settings_ids_without_moving_known_ownership() {
        let locked = ["tool", "tool-plugins", "tool-plugins-2"].map(str::to_owned).into();
        let sources = migrate_legacy(&[legacy("tool", Skills)], &locked).unwrap();
        assert_eq!(sources[0].lanes.id(Skills), Some("tool"));
        assert_eq!(sources[0].lanes.id(Plugins), Some("tool-plugins-3"));
    }
}
