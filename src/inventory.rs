//! Recursive discovery from a complete **branch-root** snapshot. All upstream paths in the result are
//! exact, branch-root Git paths. Package ownership is relative to the selected scope, not the whole branch.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Bound::{Included, Unbounded};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::hash;
use crate::pathsafe;
use crate::plugins::{NameMap, catalog, normal_name};
use crate::source::SourceKind::{self, Agents, Instructions, Plugins, Skills};
use crate::tree::{self, EntryKind, SKILL_FILE, SkillFile, Snapshot, TreeEntry};
use crate::win::fs::same_win_name;

const REPO_FOLDERS: &[&str] = &[".github", ".agents", ".claude", ".copilot", ".codex", ".cursor", ".gemini"];
const STANDALONE: &str = "Copilot CLI can install a plugin from a branch only through a marketplace that lists it";

/// Effective folder policy, after applying any source overrides. Type switches are applied by the caller,
/// never by this inventory: even a disabled type can own a package or protect a previously installed name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Policy {
    pub include_repo_items: bool,
    pub ignore_folders: BTreeSet<String>,
}

/// Lengths in UTF-16 code units, including any destination subfolder already selected by the caller.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InstallRoots {
    pub skills: usize,
    pub agents: usize,
    pub instructions: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item<T> {
    /// A skill/plugin root (empty means the branch root), or the path to a loose file.
    pub path: String,
    pub install_name: String,
    pub status: Status,
    pub data: T,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Installable,
    Filtered(FilterReason),
    Bundled { owner: String },
    Collision,
    Rejected(String),
    Standalone,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FilterReason {
    RepoSpecificFolder(String),
    IgnoredFolder(String),
}

impl Status {
    pub fn reason(&self) -> Option<String> {
        Some(match self {
            Self::Installable => return None,
            Self::Filtered(FilterReason::RepoSpecificFolder(folder)) => format!("'{}' is a repository-specific folder", pathsafe::printable(folder)),
            Self::Filtered(FilterReason::IgnoredFolder(folder)) => format!("'{}' is an ignored folder", pathsafe::printable(folder)),
            Self::Bundled { owner } if owner.is_empty() => "Bundled in the package at the repository root".into(),
            Self::Bundled { owner } => format!("Bundled in the package at '{}'", pathsafe::printable(owner)),
            Self::Collision => "Another item of this type has the same install name on Windows".into(),
            Self::Rejected(reason) => reason.clone(),
            Self::Standalone => STANDALONE.into(),
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PluginDetails {
    pub publisher: Option<String>,
    pub version: Option<String>,
    pub capabilities: PluginCapabilities,
    pub listed_by_marketplace: bool,
    /// Other spellings declared at this root, e.g. by a marketplace row whose name differs from the manifest.
    pub other_names: BTreeSet<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PluginCapabilities {
    pub hooks: bool,
    pub mcp_servers: bool,
    pub lsp_servers: bool,
    pub agents: bool,
    pub skills: bool,
    pub commands: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Collision {
    pub kind: SourceKind,
    pub paths: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotSynced {
    pub path: String,
    pub reason: &'static str,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Inventory {
    /// Files are populated only for installable skills, and their paths are relative to the package root.
    pub skills: Vec<Item<Vec<SkillFile>>>,
    /// A verified listing descriptor is populated only for installable leaves; its path is branch-root relative.
    pub agents: Vec<Item<Option<SkillFile>>>,
    pub instructions: Vec<Item<Option<SkillFile>>>,
    /// These are discovery facts, not permission to install or proof of the CLI's actual plugin origin.
    pub plugins: Vec<Item<PluginDetails>>,
    pub marketplace: Option<String>,
    /// The marketplace could not prove an install target. Do not treat unknown catalog entries as absent.
    pub marketplace_problem: Option<String>,
    pub collisions: Vec<Collision>,
    pub not_synced: Vec<NotSynced>,
}

/// Most notes a summary keeps; the rest are only counted, so one odd repository cannot bloat the display cache.
const MAX_NOTES: usize = 200;

/// What one complete listing of a source held, small enough to keep between runs so a row can show it without asking
/// GitHub again. Display data only: it never authorises installing, keeping or removing anything.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Summary {
    /// Items found of each type, as [skills, agents, instructions, plugins], whatever their status. Items bundled in a
    /// package are part of that package and are not counted.
    pub found: [u32; 4],
    /// Of those, the items nothing stops from syncing (whether their type is on is decided elsewhere).
    pub installable: [u32; 4],
    /// Of those, the items a folder rule skipped (a repository-specific folder or an ignored folder).
    pub filtered: [u32; 4],
    /// Items that are listed but never synced, with the reason: files that are never synced, collisions, rejected
    /// items and plugins Copilot CLI cannot install from a branch.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<SummaryNote>,
    /// Notes left out after the first `MAX_NOTES`.
    #[serde(skip_serializing_if = "is_zero")]
    pub more_notes: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummaryNote {
    /// The type the item would have been; none for a file that is never synced, such as `AGENTS.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<SourceKind>,
    /// Its branch-root path.
    pub path: String,
    pub reason: String,
}

impl Inventory {
    /// The display summary of this inventory.
    pub fn summary(&self) -> Summary {
        let mut out = Summary::default();
        let mut notes = Vec::new();
        let mut count = |index: usize, kind: SourceKind, path: &str, status: &Status| {
            if matches!(status, Status::Bundled { .. }) {
                return;
            }
            out.found[index] += 1;
            match status {
                Status::Installable => out.installable[index] += 1,
                Status::Filtered(_) => out.filtered[index] += 1,
                other => notes.push(SummaryNote { kind: Some(kind), path: path.to_owned(), reason: other.reason().unwrap_or_default() }),
            }
        };
        self.skills.iter().for_each(|i| count(0, Skills, &i.path, &i.status));
        self.agents.iter().for_each(|i| count(1, Agents, &i.path, &i.status));
        self.instructions.iter().for_each(|i| count(2, Instructions, &i.path, &i.status));
        self.plugins.iter().for_each(|i| count(3, Plugins, &i.path, &i.status));
        notes.extend(self.not_synced.iter().map(|n| SummaryNote { kind: None, path: n.path.clone(), reason: n.reason.to_owned() }));
        if notes.len() > MAX_NOTES {
            out.more_notes = u32::try_from(notes.len() - MAX_NOTES).unwrap_or(u32::MAX);
            notes.truncate(MAX_NOTES);
        }
        out.notes = notes;
        out
    }

    /// Positive evidence, including filtered, bundled, colliding, rejected and standalone items.
    /// Missing names alone do not authorize any removal.
    pub fn present_names(&self, kind: SourceKind) -> BTreeSet<&str> {
        match kind {
            Skills => self.skills.iter().map(|item| item.install_name.as_str()).collect(),
            Agents => self.agents.iter().map(|item| item.install_name.as_str()).collect(),
            Instructions => self.instructions.iter().map(|item| item.install_name.as_str()).collect(),
            Plugins => self.plugins.iter().flat_map(|item| std::iter::once(&item.install_name).chain(&item.data.other_names).map(String::as_str)).collect(),
        }
    }

    pub fn contains_name(&self, kind: SourceKind, name: &str) -> bool {
        let key = pathsafe::fold(name);
        self.present_names(kind)
            .into_iter()
            .any(|other| pathsafe::fold(other) == key || ((!name.is_ascii() || !other.is_ascii()) && same_win_name(name, other)))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ManifestFiles {
    pub marketplace: Option<SkillFile>,
    pub plugins: Vec<SkillFile>,
}

/// Candidates to download at the same pin as the listing. Only the branch-root marketplace is considered.
/// A malformed marketplace location is reported by `inventory`; no bytes should be fetched for it.
pub fn manifest_files(snapshot: &Snapshot, scope: &str) -> Result<ManifestFiles, String> {
    let scoped = ScopedTree::new(snapshot, scope)?;
    Ok(ManifestFiles { marketplace: scoped.catalog.inventory_marketplace_file().unwrap_or(None), plugins: scoped.plugin_files().collect() })
}

struct ScopedTree<'a> {
    scope: &'a str,
    entries: BTreeMap<&'a str, &'a TreeEntry>,
    catalog: catalog::CatalogTree<'a>,
}

impl<'a> ScopedTree<'a> {
    fn new(snapshot: &'a Snapshot, scope: &'a str) -> Result<Self, String> {
        let catalog = catalog::CatalogTree::new(snapshot)?; // also checks entry_index on the whole listing
        if !scope.is_empty() {
            pathsafe::check_rel_path(scope).map_err(|why| format!("Invalid GitHub folder: {why}"))?;
        }
        let entries: BTreeMap<_, _> = snapshot.entries.iter().filter(|e| relative(&e.path, scope).is_some()).map(|e| (e.path.as_str(), e)).collect();
        let scoped = Self { scope, catalog, entries };
        if !scoped.is_dir(scope) {
            return Err(format!("'{}' is missing or is not a folder on this branch", pathsafe::printable(scope)));
        }
        Ok(scoped)
    }

    fn is_dir(&self, path: &str) -> bool {
        path.is_empty() || self.entries.get(path).is_some_and(|e| e.kind == EntryKind::Tree && e.mode == "040000")
    }

    fn plugin_files(&self) -> impl Iterator<Item = SkillFile> + '_ {
        self.entries.values().filter(|e| basename(&e.path) == "plugin.json").filter_map(|e| catalog::inventory_manifest_candidate(e))
    }

    fn skill_files(&self, root: &str, name: &str, root_len: usize) -> Result<Vec<SkillFile>, String> {
        check_remote_path(root)?;
        let prefix = if root.is_empty() { String::new() } else { format!("{root}/") };
        let entries: Vec<_> = self
            .entries
            .range::<str, _>((Included(prefix.as_str()), Unbounded))
            .take_while(|(path, _)| path.starts_with(&prefix))
            .map(|(path, entry)| {
                let mut entry = (**entry).clone();
                entry.path = path[prefix.len()..].to_owned();
                entry
            })
            .collect();
        // Rebase just this package, so the existing single-skill validation sees its entire payload.
        let mut found = tree::discover(&entries, name, root_len);
        if let Some(rejected) = found.rejected.pop() {
            return Err(rejected.reason);
        }
        found.skills.pop().map(|skill| skill.files).ok_or_else(|| "No regular SKILL.md was found".into())
    }
}

/// Use a complete recursive *branch-root* snapshot, with `scope` set to an exact directory in that snapshot.
/// Supply the verified bytes for `manifest_files` by exact Git path, and the optional marketplace separately.
/// Missing or mismatched bytes are errors, never evidence that a package disappeared.
pub fn inventory(
    snapshot: &Snapshot,
    repo_name: &str,
    scope: &str,
    manifests: &HashMap<String, Vec<u8>>,
    marketplace: Option<&[u8]>,
    policy: &Policy,
    roots: InstallRoots,
) -> Result<Inventory, String> {
    let scoped = ScopedTree::new(snapshot, scope)?;
    for file in scoped.plugin_files() {
        verify_download(&file, manifests.get(&file.path).map(Vec::as_slice))?;
    }
    let mut found = Inventory::default();
    let market = match scoped.catalog.inventory_marketplace_file() {
        Ok(Some(file)) => catalog::inventory_marketplace(verify_download(&file, marketplace)?).map(Some),
        Ok(None) if marketplace.is_none() => Ok(None),
        Ok(None) => return Err("The marketplace bytes do not belong to this branch listing".into()),
        Err(reason) => Err(reason),
    };
    let market = match market {
        Ok(market) => {
            if let Some(market) = &market {
                found.marketplace_problem = market.problem.clone();
                if market.problem.is_none() {
                    found.marketplace = Some(market.name.clone());
                }
            }
            market
        }
        Err(reason) => {
            found.marketplace_problem = Some(reason);
            None
        }
    };
    found.plugins = plugin_items(&scoped, repo_name, manifests, market.as_ref(), found.marketplace_problem.as_deref());

    let mut markers = BTreeMap::new();
    for (path, entry) in &scoped.entries {
        if *path == scope {
            continue;
        }
        let leaf = basename(path);
        if leaf.eq_ignore_ascii_case(SKILL_FILE) || (!leaf.is_ascii() && same_win_name(leaf, SKILL_FILE)) {
            let marker = markers.entry(parent(path)).or_insert(*entry);
            if leaf == SKILL_FILE {
                *marker = *entry;
            }
        }
    }
    let mut packages: HashMap<_, _> = markers.iter().filter(|(_, e)| skill_marker(e)).map(|(&path, _)| (path, Skills)).collect();
    for item in &found.plugins {
        if scoped.is_dir(&item.path) {
            packages.insert(item.path.as_str(), Plugins); // plugin wins a tie with a skill
        }
    }
    let folded_ignore: BTreeSet<_> = policy.ignore_folders.iter().map(|name| pathsafe::fold(name)).collect();
    let classify = |kind, folder, package: bool| -> Option<Status> {
        if let Some(owner) = owning_package(&packages, scope, folder, package.then_some(kind)) {
            return Some(Status::Bundled { owner: owner.to_owned() });
        }
        filter(policy.include_repo_items, &folded_ignore, kind, folder, scope).map(Status::Filtered)
    };
    let plugin_status: Vec<_> = found.plugins.iter().map(|item| classify(Plugins, &item.path, true)).collect();
    for (root, entry) in markers {
        let status = classify(Skills, root, true).unwrap_or_else(|| {
            if skill_marker(entry) { Status::Installable } else { Status::Rejected("The skill marker must be a regular file named SKILL.md".into()) }
        });
        found.skills.push(Item { path: root.to_owned(), install_name: package_name(root, repo_name).into(), status, data: Vec::new() });
    }
    for (path, entry) in &scoped.entries {
        if *path == scope {
            continue;
        }
        let folder = parent(path);
        let name = basename(path);
        if let Some((kind, suffix)) = loose_kind(name) {
            let (items, root_len) = if kind == Agents { (&mut found.agents, roots.agents) } else { (&mut found.instructions, roots.instructions) };
            let mut data = None;
            let status = classify(kind, folder, false).unwrap_or_else(|| match loose_file(entry, name, suffix, root_len) {
                Ok(file) => {
                    data = Some(file);
                    Status::Installable
                }
                Err(reason) => Status::Rejected(reason),
            });
            items.push(Item { path: (*path).to_owned(), install_name: name.to_owned(), status, data });
        } else if regular_blob(entry) && owning_package(&packages, scope, folder, None).is_none() {
            if let Some(reason) = incompatible(path).filter(|_| filter(true, &folded_ignore, Instructions, folder, scope).is_none()) {
                found.not_synced.push(NotSynced { path: (*path).to_owned(), reason });
            }
        }
    }
    for (item, status) in found.plugins.iter_mut().zip(plugin_status) {
        if let Some(status) = status {
            item.status = status;
        }
    }
    mark_collisions(Skills, &mut found.skills, |_| None, &mut found.collisions);
    mark_collisions(Agents, &mut found.agents, |_| None, &mut found.collisions);
    mark_collisions(Instructions, &mut found.instructions, |_| None, &mut found.collisions);
    mark_collisions(Plugins, &mut found.plugins, |details| Some(&details.other_names), &mut found.collisions);
    for item in &mut found.skills {
        if item.status == Status::Installable {
            match scoped.skill_files(&item.path, &item.install_name, roots.skills) {
                Ok(files) => item.data = files,
                Err(reason) => item.status = Status::Rejected(reason),
            }
        }
    }
    for item in found.agents.iter_mut().chain(&mut found.instructions) {
        if item.status != Status::Installable {
            item.data = None;
        }
    }
    Ok(found)
}

#[derive(Default)]
struct PluginRoot<'a> {
    has_dot_manifest: bool,
    bare: Option<&'a TreeEntry>,
    rows: Vec<&'a catalog::InventoryMarketEntry>,
}

fn plugin_items(
    scoped: &ScopedTree<'_>,
    repo_name: &str,
    manifests: &HashMap<String, Vec<u8>>,
    market: Option<&catalog::InventoryMarketplace>,
    market_problem: Option<&str>,
) -> Vec<Item<PluginDetails>> {
    let mut candidates: BTreeMap<&str, PluginRoot<'_>> = BTreeMap::new();
    for (&path, &entry) in &scoped.entries {
        if basename(path) != "plugin.json" {
            continue;
        }
        let Some((root, manifest)) = catalog::PLUGIN_PATHS
            .iter()
            .filter_map(|&suffix| {
                let root = if path == suffix { "" } else { path.strip_suffix(suffix)?.strip_suffix('/')? };
                Some((root, suffix))
            })
            .max_by_key(|(_, suffix)| suffix.len())
        else {
            continue;
        };
        if relative(root, scoped.scope).is_some() {
            let candidate = candidates.entry(root).or_default();
            if manifest == "plugin.json" { candidate.bare = Some(entry) } else { candidate.has_dot_manifest = true }
        }
    }
    if let Some(market) = market {
        for row in &market.entries {
            if relative(&row.folder, scoped.scope).is_some() {
                candidates.entry(&row.folder).or_default().rows.push(row);
            }
        }
    }
    let mut items = Vec::new();
    for (root, candidate) in candidates {
        let row = candidate.rows.first().copied();
        let selected = scoped
            .catalog
            .inventory_plugin_file(root)
            .and_then(|file| file.ok_or_else(|| "No plugin manifest (plugin.json) was found in the plugin folder".into()));
        let document = |file: &SkillFile| catalog::inventory_plugin_document(&manifests[&file.path]);
        let (parsed, manifest_error) = match selected.as_ref().map_err(Clone::clone).and_then(document) {
            Ok(m) => (Some(m), None),
            Err(why) => {
                // A valid bare manifest still defines package ownership if a higher-priority alias blocks installation.
                let bare = candidate.bare.and_then(catalog::inventory_manifest_candidate).and_then(|file| document(&file).ok());
                (bare, Some(why))
            }
        };
        let (mut data, metadata_error) = plugin_details(parsed.as_ref(), row, row.and(market).and_then(|m| m.owner.as_deref()));
        // Copilot loads these by convention too, so they are shown before the plugin is approved even if undeclared.
        let has = |paths: &[&str]| scoped.catalog.has_any(root, paths);
        let caps = &mut data.capabilities;
        caps.hooks |= has(catalog::HOOK_PATHS);
        caps.mcp_servers |= has(catalog::MCP_PATHS);
        caps.lsp_servers |= has(catalog::LSP_PATHS);
        caps.agents |= has_files(scoped, root, "agents");
        caps.skills |= has_files(scoped, root, "skills");
        caps.commands |= has_files(scoped, root, "commands");
        let manifest_name = parsed.as_ref().and_then(|m| m.get("name")).and_then(Value::as_str);
        let valid_name = manifest_name.is_some_and(normal_name);
        if !candidate.has_dot_manifest && row.is_none() && !(valid_name && metadata_error.is_none()) {
            continue; // An unrelated root or directory can also have a file named plugin.json.
        }
        let row_name = row.and_then(|r| r.fields.get("name")).and_then(Value::as_str).filter(|s| !s.is_empty());
        let name = row_name.or(manifest_name.filter(|s| !s.is_empty())).unwrap_or_else(|| package_name(root, repo_name));
        data.other_names.extend(
            candidate
                .rows
                .iter()
                .filter_map(|r| r.fields.get("name").and_then(Value::as_str))
                .chain(manifest_name)
                .filter(|s| !s.is_empty() && *s != name)
                .map(str::to_owned),
        );
        let reason = check_remote_path(root)
            .err()
            .or_else(|| (!scoped.is_dir(root)).then(|| "The plugin source is not a plain folder in this branch".into()))
            .or_else(|| (candidate.rows.len() > 1).then(|| "More than one marketplace entry lists this plugin folder".into()))
            .or_else(|| row.and_then(|r| r.problem.clone()))
            .or(manifest_error)
            .or_else(|| (!valid_name).then(|| "The plugin manifest name is not in Copilot's normal form".into()))
            .or_else(|| (row.is_some() && row_name != manifest_name).then(|| "The plugin manifest name does not match the marketplace entry".into()))
            .or(metadata_error)
            .or_else(|| market_problem.map(|why| format!("The branch's marketplace could not be read: {why}")));
        let status = reason.map_or(if data.listed_by_marketplace { Status::Installable } else { Status::Standalone }, Status::Rejected);
        items.push(Item { path: root.to_owned(), install_name: name.to_owned(), status, data });
    }
    items
}

/// Whether the plugin folder `root` has a default `folder` (any spelling Windows treats as the same) with at least
/// one file somewhere under it.
fn has_files(scoped: &ScopedTree<'_>, root: &str, folder: &str) -> bool {
    let prefix = if root.is_empty() { String::new() } else { format!("{root}/") };
    scoped.entries.range::<str, _>((Included(prefix.as_str()), Unbounded)).take_while(|(path, _)| path.starts_with(&prefix)).any(|(path, entry)| {
        entry.kind == EntryKind::Blob
            && path[prefix.len()..]
                .split_once('/')
                .is_some_and(|(first, _)| first.eq_ignore_ascii_case(folder) || (!first.is_ascii() && same_win_name(first, folder)))
    })
}

fn plugin_details(manifest: Option<&NameMap<Value>>, row: Option<&catalog::InventoryMarketEntry>, owner: Option<&str>) -> (PluginDetails, Option<String>) {
    let declared = |key| manifest.and_then(|m| m.get(key));
    let catalog = |key| row.and_then(|r| r.fields.get(key));
    let has = |key| declared(key).or_else(|| catalog(key)).is_some();
    let capabilities = PluginCapabilities {
        hooks: has("hooks"),
        mcp_servers: has("mcpServers"),
        lsp_servers: has("lspServers"),
        agents: has("agents"),
        skills: has("skills"),
        commands: has("commands"),
    };
    let mut data = PluginDetails { capabilities, listed_by_marketplace: row.is_some(), ..PluginDetails::default() };
    let publisher = catalog::inventory_author(declared("author")).or_else(|| catalog::inventory_author(catalog("author"))).or_else(|| owner.map(str::to_owned));
    let version = declared("version").filter(|v| !v.is_null()).or_else(|| catalog("version").filter(|v| !v.is_null()));
    let mut problem = None;
    if let Some(publisher) = publisher {
        problem = catalog::inventory_metadata_problem("publisher", &publisher);
        if problem.is_none() {
            data.publisher = Some(publisher);
        }
    }
    if let Some(version) = version {
        if let Some(version) = version.as_str() {
            let invalid = catalog::inventory_metadata_problem("version", version);
            if invalid.is_none() {
                data.version = Some(version.into());
            }
            problem = problem.or(invalid);
        } else {
            problem.get_or_insert_with(|| "The plugin version is not a string".into());
        }
    }
    (data, problem)
}

fn verify_download<'a>(file: &SkillFile, bytes: Option<&'a [u8]>) -> Result<&'a [u8], String> {
    bytes
        .filter(|b| b.len() as u64 == file.size && hash::blob_hex(b) == file.blob)
        .ok_or_else(|| format!("No verified bytes for '{}' in the branch listing", pathsafe::printable(&file.path)))
}

fn relative<'a>(path: &'a str, scope: &str) -> Option<&'a str> {
    if scope.is_empty() {
        Some(path)
    } else if path == scope {
        Some("")
    } else {
        path.strip_prefix(scope)?.strip_prefix('/')
    }
}
fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or_default()
}
fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(folder, _)| folder)
}
fn package_name<'a>(root: &'a str, repo_name: &'a str) -> &'a str {
    if root.is_empty() { repo_name } else { basename(root) }
}

fn check_remote_path(path: &str) -> Result<(), String> {
    if !path.is_empty() {
        pathsafe::check_rel_path(path).map_err(|why| format!("Unsafe GitHub path '{}': {why}", pathsafe::printable(path)))?;
    }
    Ok(())
}

fn regular_blob(entry: &TreeEntry) -> bool {
    entry.kind == EntryKind::Blob && matches!(entry.mode.as_str(), "100644" | "100664" | "100755")
}
fn skill_marker(entry: &TreeEntry) -> bool {
    basename(&entry.path) == SKILL_FILE && regular_blob(entry)
}

fn loose_kind(name: &str) -> Option<(SourceKind, &'static str)> {
    let (dot, _) = name.rmatch_indices('.').nth(1)?;
    let tail = &name[dot..];
    [(Agents, ".agent.md"), (Instructions, ".instructions.md")]
        .into_iter()
        .find(|(_, suffix)| tail.eq_ignore_ascii_case(suffix) || (!tail.is_ascii() && same_win_name(tail, suffix)))
}

fn loose_file(entry: &TreeEntry, name: &str, suffix: &str, root_len: usize) -> Result<SkillFile, String> {
    check_remote_path(&entry.path)?;
    if name.len() == suffix.len() || !name.ends_with(suffix) {
        return Err(format!("Expected a file name ending in '{suffix}', with a name before the suffix"));
    }
    if !regular_blob(entry) {
        return Err("The listed item is not a regular file".into());
    }
    tree::check_leaf_length(root_len, name)?;
    let size = entry.size.ok_or("GitHub did not provide the file size")?;
    if size > tree::MAX_FILE_BYTES {
        return Err(format!("The file is larger than {} MiB", tree::MAX_FILE_BYTES / 1024 / 1024));
    }
    Ok(SkillFile { path: entry.path.clone(), blob: entry.sha.clone(), size })
}

fn owning_package<'a>(packages: &HashMap<&'a str, SourceKind>, scope: &str, folder: &str, own_kind: Option<SourceKind>) -> Option<&'a str> {
    // In increasing depth, including scope and the item's own folder, but never a package outside this scope.
    std::iter::once(scope)
        .chain(folder.match_indices('/').filter(|(i, _)| *i > scope.len()).map(|(i, _)| &folder[..i]))
        .chain(std::iter::once(folder))
        .find_map(|path| {
            let (&root, &kind) = packages.get_key_value(path)?;
            (root != folder || own_kind != Some(kind)).then_some(root)
        })
}

fn filter(include_repo_items: bool, ignore_folders: &BTreeSet<String>, kind: SourceKind, folder: &str, scope: &str) -> Option<FilterReason> {
    let segments = relative(folder, scope)?.split('/').filter(|s| !s.is_empty());
    if let Some(segment) = segments.clone().find(|s| ignore_folders.contains(&pathsafe::fold(s))) {
        Some(FilterReason::IgnoredFolder(segment.into()))
    } else if !include_repo_items && matches!(kind, Skills | Agents) {
        segments.filter(|s| REPO_FOLDERS.iter().any(|name| s.eq_ignore_ascii_case(name))).map(|s| FilterReason::RepoSpecificFolder(s.into())).next()
    } else {
        None
    }
}

fn incompatible(path: &str) -> Option<&'static str> {
    let leaf = basename(path);
    if leaf.eq_ignore_ascii_case("AGENTS.md") {
        Some("Repository guidance for coding agents, not a custom agent")
    } else if leaf.eq_ignore_ascii_case("copilot-instructions.md") {
        Some("Repository-wide instructions apply only inside that repository")
    } else if leaf.eq_ignore_ascii_case(".cursorrules")
        || (leaf.to_ascii_lowercase().ends_with(".mdc")
            && basename(parent(path)).eq_ignore_ascii_case("rules")
            && basename(parent(parent(path))).eq_ignore_ascii_case(".cursor"))
    {
        Some("Cursor rules are not Copilot instructions")
    } else {
        None
    }
}

fn mark_collisions<T>(kind: SourceKind, items: &mut [Item<T>], other_names: impl Fn(&T) -> Option<&BTreeSet<String>>, out: &mut Vec<Collision>) {
    let mut by_fold = BTreeMap::<_, Vec<usize>>::new();
    let mut by_length = HashMap::<_, Vec<_>>::new();
    for (i, item) in items.iter().enumerate() {
        for name in std::iter::once(&item.install_name).chain(other_names(&item.data).into_iter().flatten()) {
            let key = pathsafe::fold(name);
            let slots = by_fold.entry(key.clone()).or_default();
            if slots.last() != Some(&i) {
                slots.push(i);
            }
            by_length.entry(name.encode_utf16().count()).or_default().push((i, name.as_str(), key));
        }
    }
    let mut groups: BTreeSet<_> = by_fold.into_values().filter(|slots| slots.len() > 1).collect();
    // Windows' ordinal table can equate spellings that Unicode lowercasing does not. ASCII pairs
    // never need that extra comparison; equal ordinal names must have the same UTF-16 length.
    for entries in by_length.values() {
        for (i, name, key) in entries.iter().filter(|(_, name, _)| !name.is_ascii()) {
            for (j, other, other_key) in entries {
                if i != j && key != other_key && (other.is_ascii() || i < j) && same_win_name(name, other) {
                    groups.insert(vec![*i.min(j), *i.max(j)]);
                }
            }
        }
    }
    for indices in groups {
        let paths = indices
            .into_iter()
            .map(|i| {
                let item = &mut items[i];
                // Preserve a more specific hold reason; the collision is reported regardless of policy.
                if item.status == Status::Installable {
                    item.status = Status::Collision;
                }
                item.path.clone()
            })
            .collect();
        out.push(Collision { kind, paths });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MARKET: &[u8] =
        br#"{"name":"work","owner":{"name":"Org"},"plugins":[{"name":"tool","source":"./plugins/tool","lspServers":{},"commands":[],"version":"2"}]}"#;
    const TOOL: &[u8] = br#"{"name":"tool","author":{"name":"Team"},"hooks":{},"mcpServers":{},"agents":"agents","skills":"skills"}"#;

    struct Fixture {
        snapshot: Snapshot,
        bytes: HashMap<String, Vec<u8>>,
    }

    impl Fixture {
        fn new(paths: &[&str], docs: &[(&str, &[u8])]) -> Self {
            let mut entries = BTreeMap::new();
            for (path, body) in paths.iter().map(|path| (*path, &b"a file"[..])).chain(docs.iter().copied()) {
                for (i, _) in path.match_indices('/') {
                    let dir = &path[..i];
                    entries.entry(dir).or_insert_with(|| TreeEntry {
                        path: dir.into(),
                        kind: EntryKind::Tree,
                        mode: "040000".into(),
                        sha: "1".repeat(40),
                        size: None,
                    });
                }
                entries.insert(
                    path,
                    TreeEntry { path: path.into(), kind: EntryKind::Blob, mode: "100644".into(), sha: hash::blob_hex(body), size: Some(body.len() as u64) },
                );
            }
            Self {
                snapshot: Snapshot { sha: "2".repeat(40), truncated: false, entries: entries.into_values().collect() },
                bytes: docs.iter().map(|(path, bytes)| ((*path).into(), bytes.to_vec())).collect(),
            }
        }

        fn run(&self, scope: &str, policy: &Policy, roots: InstallRoots) -> Result<Inventory, String> {
            let plan = manifest_files(&self.snapshot, scope)?;
            let market = plan.marketplace.as_ref().and_then(|file| self.bytes.get(&file.path)).map(Vec::as_slice);
            inventory(&self.snapshot, "repo", scope, &self.bytes, market, policy, roots)
        }

        fn find(&self, scope: &str, policy: &Policy) -> Inventory {
            self.run(scope, policy, InstallRoots::default()).unwrap()
        }

        fn entry(&mut self, path: &str) -> &mut TreeEntry {
            self.snapshot.entries.iter_mut().find(|e| e.path == path).unwrap()
        }
    }

    #[test]
    fn mixed_branch_finds_each_kind_and_declared_plugin_capabilities() {
        let f = Fixture::new(
            &["skills/helper/SKILL.md", "tools/review.agent.md", ".github/instructions/style.instructions.md"],
            &[(".github/plugin/marketplace.json", MARKET), ("plugins/tool/.claude-plugin/plugin.json", TOOL)],
        );
        let i = f.find("", &Policy::default());
        assert_eq!((i.skills.len(), i.agents.len(), i.instructions.len(), i.plugins.len()), (1, 1, 1, 1));
        assert_eq!((i.skills[0].path.as_str(), i.skills[0].install_name.as_str(), i.skills[0].data[0].path.as_str()), ("skills/helper", "helper", "SKILL.md"));
        assert_eq!(i.agents[0].install_name, "review.agent.md");
        assert_eq!(i.instructions[0].install_name, "style.instructions.md");
        let p = &i.plugins[0];
        assert_eq!((p.install_name.as_str(), p.path.as_str(), &p.status), ("tool", "plugins/tool", &Status::Installable));
        assert_eq!((p.data.publisher.as_deref(), p.data.version.as_deref(), i.marketplace.as_deref()), (Some("Team"), Some("2"), Some("work")));
        assert!(p.data.listed_by_marketplace);
        assert_eq!(p.data.capabilities, PluginCapabilities { hooks: true, mcp_servers: true, lsp_servers: true, agents: true, skills: true, commands: true });
    }

    #[test]
    fn convention_files_declare_plugin_capabilities_the_manifest_does_not() {
        // v0.8.0's catalog paths, then the default component folders. Every manifest declares nothing.
        type Case = (&'static str, &'static str, fn(&PluginCapabilities) -> bool);
        let cases: [Case; 12] = [
            ("mcp-dot", ".mcp.json", |c| c.mcp_servers),
            ("mcp-github", ".github/mcp.json", |c| c.mcp_servers),
            ("mcp-plain", "mcp.json", |c| c.mcp_servers),
            ("hooks-plain", "hooks.json", |c| c.hooks),
            ("hooks-folder", "hooks/hooks.json", |c| c.hooks),
            ("hooks-copilot", "com.github.copilot/hooks/hooks.json", |c| c.hooks),
            ("lsp-plain", "lsp.json", |c| c.lsp_servers),
            ("lsp-github", ".github/lsp.json", |c| c.lsp_servers),
            ("lsp-copilot", "com.github.copilot/lsp.json", |c| c.lsp_servers),
            ("agents-folder", "agents/review.agent.md", |c| c.agents),
            ("skills-folder", "skills/helper/SKILL.md", |c| c.skills),
            ("commands-folder", "commands/run.md", |c| c.commands),
        ];
        let files: Vec<String> = cases.iter().map(|(name, rel, _)| format!("plugins/{name}/{rel}")).collect();
        let manifests: Vec<(String, Vec<u8>)> =
            cases.iter().map(|(name, ..)| (format!("plugins/{name}/.claude-plugin/plugin.json"), format!(r#"{{"name":"{name}"}}"#).into_bytes())).collect();
        let f = Fixture::new(
            &files.iter().map(String::as_str).collect::<Vec<_>>(),
            &manifests.iter().map(|(path, body)| (path.as_str(), body.as_slice())).collect::<Vec<_>>(),
        );
        let i = f.find("", &Policy::default());
        let missed: Vec<String> = cases
            .iter()
            .filter_map(|(name, rel, has)| {
                let plugin = i.plugins.iter().find(|p| p.install_name == *name);
                match plugin {
                    None => Some(format!("{name}: plugin not found")),
                    Some(p) if !has(&p.data.capabilities) => Some(format!("{rel} not detected: {:?}", p.data.capabilities)),
                    Some(_) => None,
                }
            })
            .collect();
        assert!(missed.is_empty(), "convention files a plugin folder holds must show in its capabilities:\n{}", missed.join("\n"));
    }

    #[test]
    fn filters_are_per_kind_case_insensitive_and_only_below_scope() {
        let f = Fixture::new(
            &[
                "packages/api/.GitHub/agents/review.agent.md",
                ".github/instructions/style.instructions.md",
                "Examples/team/demo/SKILL.md",
                "not-examples/writing/SKILL.md",
            ],
            &[],
        );
        let policy = Policy { include_repo_items: false, ignore_folders: ["eXaMpLeS".into()].into() };
        let i = f.find("", &policy);
        assert_eq!(i.agents[0].status, Status::Filtered(FilterReason::RepoSpecificFolder(".GitHub".into())));
        assert_eq!(i.instructions[0].status, Status::Installable);
        assert_eq!(i.skills[0].status, Status::Filtered(FilterReason::IgnoredFolder("Examples".into())));
        assert_eq!(i.skills[1].status, Status::Installable, "the filter matches a whole component");
        assert_eq!(i.present_names(Skills), ["demo", "writing"].into());
        assert!(i.contains_name(Agents, "Review.agent.md"));
        let with_repo_items = f.find("", &Policy { include_repo_items: true, ..policy.clone() });
        assert_eq!(with_repo_items.agents[0].status, Status::Installable);
        assert_eq!(with_repo_items.skills, i.skills);
        assert_eq!(with_repo_items.instructions, i.instructions);
        let source = crate::sources::SourceConfig { ignore_allow: vec!["EXAMPLES".into()], ..Default::default() };
        let allowed = f.find("", &source.effective_policy(&crate::sources::GlobalSettings::default()).unwrap());
        assert_eq!(allowed.skills[0].status, Status::Installable);
        assert_eq!(allowed.agents, i.agents);
        assert_eq!(f.find("packages/api/.GitHub/agents", &policy).agents[0].status, Status::Installable);
        assert_eq!(f.find("Examples/team", &policy).skills[0].status, Status::Installable);
    }

    #[test]
    fn one_marketplace_plugin_owns_its_parts_but_scoping_inside_finds_them() {
        let f = Fixture::new(
            &["plugins/tool/skills/explain/SKILL.md", "plugins/tool/agents/helper.agent.md"],
            &[("marketplace.json", MARKET), ("plugins/tool/.plugin/plugin.json", TOOL)],
        );
        let i = f.find("", &Policy::default());
        assert_eq!(i.plugins.len(), 1);
        assert_eq!(i.plugins[0].status, Status::Installable);
        let owner = Status::Bundled { owner: "plugins/tool".into() };
        assert_eq!(i.skills[0].status, owner);
        assert!(i.skills[0].data.is_empty());
        assert_eq!(i.agents[0].status, owner);
        assert_eq!(i.present_names(Skills), ["explain"].into());
        let at_plugin = f.find("plugins/tool", &Policy::default());
        assert_eq!(at_plugin.plugins[0].status, Status::Installable);
        assert_eq!(at_plugin.skills[0].status, owner);
        let scoped = f.find("plugins/tool/skills", &Policy::default());
        assert!(scoped.plugins.is_empty());
        assert_eq!((scoped.skills[0].install_name.as_str(), &scoped.skills[0].status), ("explain", &Status::Installable));
        let plan = manifest_files(&f.snapshot, "plugins/tool/skills").unwrap();
        assert_eq!(plan.marketplace.unwrap().path, "marketplace.json");
        assert!(plan.plugins.is_empty());
    }

    #[test]
    fn ownership_precedes_filters_and_the_outermost_skill_keeps_its_whole_payload() {
        let f = Fixture::new(
            &[
                "alpha/SKILL.md",
                "alpha/nested/SKILL.md",
                "alpha/Examples/payload.txt",
                "alpha/.github/helper.agent.md",
                "alpha/tool/SKILL.md",
                "ignored/tie/SKILL.md",
                "ignored/tie/skills/child/SKILL.md",
                "tie/SKILL.md",
                "tie/child.agent.md",
            ],
            &[("alpha/tool/plugin.json", TOOL), ("ignored/tie/.plugin/plugin.json", TOOL), ("tie/.github/plugin/plugin.json", b"not JSON")],
        );
        let policy = Policy { include_repo_items: false, ignore_folders: ["examples".into(), "ignored".into()].into() };
        let i = f.find("", &policy);
        assert_eq!(i.skills[0].status, Status::Installable);
        assert_eq!(
            i.skills[0].data.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
            [".github/helper.agent.md", "Examples/payload.txt", "SKILL.md", "nested/SKILL.md", "tool/SKILL.md", "tool/plugin.json",]
        );
        for item in &i.skills[1..3] {
            assert_eq!(item.status, Status::Bundled { owner: "alpha".into() });
        }
        assert_eq!(i.plugins[0].status, Status::Bundled { owner: "alpha".into() });
        assert_eq!(i.plugins[1].status, Status::Filtered(FilterReason::IgnoredFolder("ignored".into())));
        assert!(i.plugins[2].status.reason().unwrap().contains("JSON"));
        assert_eq!(i.skills[3].status, Status::Bundled { owner: "ignored/tie".into() });
        assert_eq!(i.skills[4].status, Status::Bundled { owner: "ignored/tie".into() });
        assert_eq!(i.skills[5].status, Status::Bundled { owner: "tie".into() });
        assert_eq!(i.agents[1].status, Status::Bundled { owner: "tie".into() });
    }

    #[test]
    fn collisions_report_both_paths_even_if_one_spelling_is_filtered() {
        let f = Fixture::new(
            &[
                "north/Review/SKILL.md",
                "south/review/SKILL.md",
                "a/Test.agent.md",
                "Examples/test.agent.md",
                "b/Guide.instructions.md",
                "c/guide.instructions.md",
            ],
            &[],
        );
        let i = f.find("", &Policy { ignore_folders: ["examples".into()].into(), ..Policy::default() });
        assert_eq!(
            i.collisions,
            [
                Collision { kind: Skills, paths: vec!["north/Review".into(), "south/review".into()] },
                Collision { kind: Agents, paths: vec!["Examples/test.agent.md".into(), "a/Test.agent.md".into()] },
                Collision { kind: Instructions, paths: vec!["b/Guide.instructions.md".into(), "c/guide.instructions.md".into()] },
            ]
        );
        assert!(i.skills.iter().all(|item| item.status == Status::Collision && item.data.is_empty()));
        assert_eq!(i.agents[1].status, Status::Collision);
        assert_eq!(i.agents[0].status, Status::Filtered(FilterReason::IgnoredFolder("Examples".into())));
        assert!(i.contains_name(Instructions, "GUIDE.instructions.md"));
    }

    #[test]
    fn guidance_and_cursor_rules_are_listed_only_outside_packages_and_ignores() {
        let f = Fixture::new(
            &[
                "AGENTS.md",
                ".cursorrules",
                ".cursor/rules/fmt.mdc",
                "packages/p/copilot-instructions.md",
                "README.md",
                ".cursor/rules/notes.md",
                "examples/AGENTS.md",
                "packages/p/.cursor/rules/sub/other.mdc",
                "skills/tool/SKILL.md",
                "skills/tool/.cursorrules",
                "skills/tool/AGENTS.md",
            ],
            &[],
        );
        let policy = Policy { ignore_folders: ["Examples".into()].into(), ..Policy::default() };
        let i = f.find("", &policy);
        assert_eq!(
            i.not_synced.iter().map(|item| item.path.as_str()).collect::<Vec<_>>(),
            [".cursor/rules/fmt.mdc", ".cursorrules", "AGENTS.md", "packages/p/copilot-instructions.md",]
        );
        assert!(i.not_synced[0].reason.contains("Cursor"));
        assert!(i.not_synced[2].reason.contains("not a custom agent"));
        assert!(i.not_synced[3].reason.contains("only inside that repository"));
        assert_eq!(f.find("examples", &policy).not_synced.len(), 1);
        assert_eq!(f.find(".cursor/rules", &policy).not_synced.len(), 1);
    }

    #[test]
    fn only_a_root_marketplace_can_make_its_listed_plugins_installable() {
        let f = Fixture::new(
            &["unrelated/skill/SKILL.md", "broken/skills/helper/SKILL.md"],
            &[
                ("plugins/marketplace.json", MARKET),
                (".github/tools/tool/plugin.json", TOOL),
                ("unrelated/plugin.json", b"{}"),
                ("broken/.plugin/plugin.json", b"{"),
            ],
        );
        let plan = manifest_files(&f.snapshot, "").unwrap();
        assert!(plan.marketplace.is_none());
        assert_eq!(plan.plugins.len(), 3);
        let i = f.find("", &Policy::default());
        assert_eq!(i.plugins.len(), 2);
        assert_eq!(i.plugins[0].path, ".github/tools/tool");
        assert_eq!(i.plugins[0].status.reason().as_deref(), Some(STANDALONE));
        assert!(!i.plugins[0].data.listed_by_marketplace);
        assert_eq!(i.plugins[1].path, "broken");
        assert!(matches!(i.plugins[1].status, Status::Rejected(_)));
        assert_eq!(i.skills[0].status, Status::Bundled { owner: "broken".into() });
        assert_eq!(i.skills[1].status, Status::Installable);
    }

    #[test]
    fn rejected_market_rows_keep_all_declared_names_and_own_their_packages() {
        let market = br#"{"name":"market","plugins":[{"name":"from-market","source":"tools/wrong"},{"name":"missing","source":"tools/missing"},{"name":"other","source":"no-such-folder"}]}"#;
        let f =
            Fixture::new(&["tools/missing/inside/SKILL.md", "tools/wrong/inside/SKILL.md"], &[("marketplace.json", market), ("tools/wrong/plugin.json", TOOL)]);
        let i = f.find("", &Policy::default());
        assert_eq!(i.plugins.len(), 3);
        assert!(i.plugins.iter().all(|item| item.data.listed_by_marketplace && matches!(item.status, Status::Rejected(_))));
        assert_eq!(i.present_names(Plugins), ["from-market", "missing", "other", "tool"].into());
        assert!(i.skills.iter().all(|item| matches!(item.status, Status::Bundled { .. })));
        let f = Fixture::new(&["tools/helper.agent.md"], &[("marketplace.json", b"malformed"), ("tools/plugin.json", TOOL)]);
        let i = f.find("", &Policy::default());
        assert!(i.marketplace_problem.is_some());
        assert!(i.contains_name(Plugins, "tool"));
        assert!(matches!(i.plugins[0].status, Status::Rejected(_)));
        assert!(matches!(i.agents[0].status, Status::Bundled { .. }));
    }

    #[test]
    fn malformed_marketplace_name_does_not_release_catalog_owned_items() {
        let market = br#"{"name":"bad market name","plugins":[{"name":"tool","source":"tools/tool"}]}"#;
        let f = Fixture::new(&["tools/tool/skills/helper/SKILL.md"], &[("marketplace.json", market)]);
        let i = f.find("", &Policy::default());
        assert!(i.marketplace_problem.is_some());
        assert_eq!(i.skills[0].status, Status::Bundled { owner: "tools/tool".into() });
        assert!(i.contains_name(Plugins, "tool"));
        assert!(matches!(i.plugins[0].status, Status::Rejected(_)));
    }

    #[test]
    fn ambiguous_higher_priority_manifest_cannot_make_a_bare_plugin_installable() {
        let f = Fixture::new(
            &["plugins/tool/skills/helper/SKILL.md"],
            &[("marketplace.json", MARKET), ("plugins/tool/plugin.json", TOOL), ("plugins/tool/.plugin/Plugin.json", b"{}")],
        );
        let i = f.find("", &Policy::default());
        assert_eq!(i.plugins.len(), 1);
        assert!(i.plugins[0].status.reason().unwrap().contains("Ambiguous"));
        assert!(i.plugins[0].data.listed_by_marketplace);
        assert_eq!(i.skills[0].status, Status::Bundled { owner: "plugins/tool".into() });
    }

    #[test]
    fn truncation_missing_parents_duplicate_entries_and_wrong_pins_are_errors() {
        let mut f = Fixture::new(&["tools/a.agent.md"], &[("marketplace.json", MARKET), ("plugins/tool/plugin.json", TOOL)]);
        let policy = Policy::default();
        assert!(f.run("tools/a.agent.md", &policy, InstallRoots::default()).unwrap_err().contains("not a folder"));
        assert!(f.run("tool", &policy, InstallRoots::default()).unwrap_err().contains("not a folder"));
        let run = |f: &Fixture| f.run("", &policy, InstallRoots::default());
        f.snapshot.truncated = true;
        assert!(run(&f).unwrap_err().contains("short"));
        f.snapshot.truncated = false;
        f.snapshot.entries.push(f.snapshot.entries[0].clone());
        assert!(run(&f).unwrap_err().contains("duplicate"));
        f.snapshot.entries.pop();
        f.entry("tools").mode = "120000".into();
        assert!(run(&f).unwrap_err().contains("plain parent"));
        f.entry("tools").mode = "040000".into();
        assert!(inventory(&f.snapshot, "repo", "", &f.bytes, None, &policy, InstallRoots::default()).unwrap_err().contains("verified bytes"));
        f.bytes.remove("plugins/tool/plugin.json");
        assert!(run(&f).unwrap_err().contains("verified bytes"));
        let mut changed = TOOL.to_vec();
        changed[0] = b'['; // same size, different Git hash
        f.bytes.insert("plugins/tool/plugin.json".into(), changed);
        assert!(run(&f).unwrap_err().contains("verified bytes"));
    }

    #[test]
    fn non_plain_and_unsafe_names_are_present_but_not_installable() {
        let mut f = Fixture::new(&["dot/SKILL.md", "CON/SKILL.md", "link/SKILL.md", "module.agent.md", "nested/AUX.instructions.md", "NOTES.Agent.MD"], &[]);
        f.entry("link/SKILL.md").mode = "120000".into();
        let submodule = f.entry("module.agent.md");
        submodule.kind = EntryKind::Commit;
        submodule.mode = "160000".into();
        let roots = InstallRoots { skills: pathsafe::MAX_INSTALLED_PATH - 12, ..Default::default() };
        let i = f.run("", &Policy::default(), roots).unwrap();
        assert!(i.skills.iter().all(|item| matches!(item.status, Status::Rejected(_))));
        assert!(i.agents.iter().all(|item| matches!(item.status, Status::Rejected(_))));
        assert!(matches!(i.instructions[0].status, Status::Rejected(_)));
        assert_eq!(i.present_names(Skills), ["CON", "dot", "link"].into());
        assert!(i.contains_name(Agents, "notes.agent.md"));
        assert!(i.contains_name(Agents, "module.agent.md"));
        assert!(i.skills[1].status.reason().unwrap().contains("259"));
    }

    #[test]
    fn scope_root_skill_takes_its_path_name_or_repo_name_and_obeys_file_count_limit() {
        let names = (0..tree::MAX_SKILL_FILES).map(|n| format!("files/{n}.txt")).collect::<Vec<_>>();
        let paths = std::iter::once("SKILL.md").chain(names.iter().map(String::as_str)).collect::<Vec<_>>();
        let i = Fixture::new(&paths, &[]).find("", &Policy::default());
        assert_eq!(i.skills[0].install_name, "repo");
        assert!(i.skills[0].status.reason().unwrap().contains("more than 10000 files"));
        let f = Fixture::new(&["scoped/thing/SKILL.md", "scoped/thing/example.txt"], &[]);
        let i = f.find("scoped/thing", &Policy::default());
        assert_eq!((i.skills[0].path.as_str(), i.skills[0].install_name.as_str(), i.skills[0].data.len()), ("scoped/thing", "thing", 2));
    }
}
