//! A branch-root Git tree plus its manifests is enough to preview a marketplace. Nothing here downloads files.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

use crate::config::RepoConfig;
use crate::hash;
use crate::inventory::{Item, PluginDetails, Status as ItemStatus};
use crate::pathsafe;
use crate::source::SourceKind;
use crate::state::{PluginOrigin, PluginSourceState};
use crate::tree::{self, EntryKind, Rejected, SkillFile, Snapshot, TreeEntry};

use super::{NameMap, name_key, normal_name, read_json_object};

pub const MAX_MANIFEST_BYTES: u64 = 256 * 1024;
pub const MARKETPLACE_PATHS: &[&str] = &["marketplace.json", ".plugin/marketplace.json", ".github/plugin/marketplace.json", ".claude-plugin/marketplace.json"];
pub const PLUGIN_PATHS: &[&str] = &[".plugin/plugin.json", "plugin.json", ".github/plugin/plugin.json", ".claude-plugin/plugin.json"];
/// Files Copilot CLI loads from a plugin folder by convention, whether or not the manifest names them.
pub const HOOK_PATHS: &[&str] = &["hooks.json", "hooks/hooks.json", "com.github.copilot/hooks/hooks.json"];
pub const MCP_PATHS: &[&str] = &[".mcp.json", ".github/mcp.json", "mcp.json"];
pub const LSP_PATHS: &[&str] = &["lsp.json", ".github/lsp.json", "com.github.copilot/lsp.json"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plugin {
    pub name: String,
    /// A branch-root Git path, not a local destination.
    pub path: String,
    pub version: Option<String>,
    pub publisher: Option<String>,
    pub description: Option<String>,
    pub fingerprint: String,
}

/// Constructed only from a structurally complete listing with a readable marketplace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Catalog {
    pub(super) origin: PluginOrigin,
    pub(super) market: String,
    owner: Option<String>,
    pub(super) plugins: NameMap<Plugin>,
    pub(super) hosted_elsewhere: Vec<String>,
    pub(super) rejected: Vec<Rejected>,
    /// Plugin names the source has that are not installed from it (see `restrict`).
    pub(super) held: Vec<Held>,
}

/// A plugin name present in the source's folder that this app does not install: standalone, filtered, bundled, or
/// known by another name. An owned plugin with such a name is kept as it is and never removed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Held {
    pub name: String,
    pub reason: String,
    /// Shown as a row even when no plugin with this name is owned. Bundled items and other names are not.
    pub shown: bool,
}

/// Why a plugin root that is not listed by the branch-root marketplace is never installed (spike S4).
pub const NOT_LISTED: &str = "Copilot CLI can install a plugin from a branch only through a marketplace that lists it";

impl Catalog {
    #[cfg(test)]
    pub(super) fn fixture(source: RepoConfig, market: &str, plugin: Plugin) -> Self {
        Self {
            origin: (&source).into(),
            market: market.into(),
            owner: None,
            plugins: [(plugin.name.clone(), plugin)].into(),
            hosted_elsewhere: vec![],
            rejected: vec![],
            held: vec![],
        }
    }

    pub fn market(&self) -> &str {
        &self.market
    }
    pub fn owner(&self) -> Option<&str> {
        self.owner.as_deref()
    }
    pub fn plugins(&self) -> &NameMap<Plugin> {
        &self.plugins
    }
    pub fn hosted_elsewhere(&self) -> &[String] {
        &self.hosted_elsewhere
    }
    pub fn rejected(&self) -> &[Rejected] {
        &self.rejected
    }
    pub fn held(&self) -> &[Held] {
        &self.held
    }

    /// Keeps only the plugins the source's inventory says may be installed: an installable plugin root, listed by the
    /// branch-root marketplace, at the same root and with the same name. Its publisher is the one the inventory (and
    /// so the user's review) shows. Every other plugin name in the inventory is held with its reason, and rejected or
    /// colliding items are rejected, so an owned plugin with such a name is never treated as gone.
    pub fn restrict(&mut self, items: &[Item<PluginDetails>]) {
        let same = |item: &Item<PluginDetails>, p: &Plugin| item.path == p.path && name_key(&item.install_name) == name_key(&p.name);
        let mut kept = NameMap::new();
        let mut unused = Vec::new();
        for (name, mut plugin) in std::mem::take(&mut self.plugins).into_entries() {
            match items.iter().find(|i| i.status == ItemStatus::Installable && i.data.listed_by_marketplace && same(i, &plugin)) {
                Some(item) => {
                    plugin.publisher.clone_from(&item.data.publisher);
                    kept.insert(name, plugin);
                }
                None => unused.push(plugin),
            }
        }
        self.plugins = kept;
        let known = |held: &[Held], rejected: &[Rejected], plugins: &NameMap<Plugin>, name: &str| {
            plugins.contains_key(name) || rejected.iter().any(|r| r.name == name) || held.iter().any(|h| h.name == name)
        };
        for item in items {
            let installed = self.plugins.get(&item.install_name).is_some_and(|p| same(item, p));
            if !installed && !known(&self.held, &self.rejected, &self.plugins, &item.install_name) {
                match &item.status {
                    ItemStatus::Rejected(_) | ItemStatus::Collision => {
                        self.rejected.push(Rejected { name: item.install_name.clone(), reason: item.status.reason().unwrap_or_default() });
                    }
                    status => {
                        let reason = match status {
                            ItemStatus::Installable if !item.data.listed_by_marketplace => NOT_LISTED.into(),
                            ItemStatus::Installable => "The marketplace entry for this plugin could not be used".into(),
                            other => other.reason().unwrap_or_default(),
                        };
                        let shown = !matches!(status, ItemStatus::Bundled { .. });
                        self.held.push(Held { name: item.install_name.clone(), reason, shown });
                    }
                }
            }
            for other in &item.data.other_names {
                if !known(&self.held, &self.rejected, &self.plugins, other) {
                    self.held.push(Held { name: other.clone(), reason: "This source has the plugin under another name".into(), shown: false });
                }
            }
        }
        for plugin in unused {
            if !known(&self.held, &self.rejected, &self.plugins, &plugin.name) {
                self.held.push(Held { name: plugin.name, reason: "This plugin is not part of this source".into(), shown: false });
            }
        }
    }

    /// Use the exact catalog the user approved. Creating a record does not adopt any native installs.
    pub fn approved_record(&self, source: &RepoConfig, home: &Path) -> Result<PluginSourceState, String> {
        if !self.origin.matches(source) || self.plugins.is_empty() || !self.rejected.is_empty() {
            return Err("Approve plugins from a complete preview with matches and no rejected entries".into());
        }
        crate::source::check_root(home).map_err(|_| "Copilot home must be a full path")?;
        let mut record = PluginSourceState::new(source, home, &self.market);
        for plugin in self.plugins.values() {
            record.approved.insert(plugin.name.clone(), plugin.publisher.clone());
            record.roots.insert(plugin.name.clone(), plugin.path.clone());
        }
        Ok(record)
    }
}

pub struct CatalogTree<'a> {
    root: &'a str,
    entries: &'a [TreeEntry],
    paths: HashMap<String, usize>,
}

#[inline(never)]
fn path_key(path: &str) -> String {
    // Copilot runs on Windows. Do not select a lower-priority manifest if a case variant or an
    // alias Windows may open at a higher-priority path is present.
    if path.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"/-_.".contains(&b)) && !path.contains("./") && !path.ends_with('.') {
        return path.into();
    }
    pathsafe::fold(&path.split('/').map(|p| p.trim_end_matches(['.', ' '])).collect::<Vec<_>>().join("/"))
}

impl<'a> CatalogTree<'a> {
    /// Pass the full recursive tree of the branch root, never the configured subfolder's listing.
    #[inline(never)]
    pub fn new(snapshot: &'a Snapshot) -> Result<Self, String> {
        tree::entry_index(snapshot)?;
        let entries = &snapshot.entries;
        let mut paths = HashMap::with_capacity(entries.len());
        for (i, entry) in entries.iter().enumerate() {
            // An out-of-range index marks a Windows spelling shared by several Git paths.
            paths.entry(path_key(&entry.path)).and_modify(|slot| *slot = entries.len()).or_insert(i);
        }
        Ok(Self { root: &snapshot.sha, entries, paths })
    }

    fn entry(&self, path: &str) -> Result<Option<&'a TreeEntry>, String> {
        let Some(&i) = self.paths.get(&path_key(path)) else { return Ok(None) };
        self.entries
            .get(i)
            .filter(|e| e.path == path)
            .map(Some)
            .ok_or_else(|| format!("Ambiguous spelling for '{}' in the GitHub listing", pathsafe::printable(path)))
    }

    fn manifest(&self, folder: &str, paths: &[&str]) -> Result<Option<SkillFile>, String> {
        'candidate: for rel in paths {
            let path = join(folder, rel);
            for (separator, _) in path.match_indices('/') {
                match self.entry(&path[..separator])? {
                    Some(e) if plain_tree(e) => {}
                    None => continue 'candidate,
                    Some(_) => return Err("A manifest parent is not a plain folder".into()),
                }
            }
            if let Some(entry) = self.entry(&path)? {
                if entry.kind != EntryKind::Blob || !matches!(entry.mode.as_str(), "100644" | "100755") {
                    return Err("The first manifest is not a regular file".into());
                }
                let size = entry.size.ok_or("GitHub did not provide the manifest size")?;
                if size > MAX_MANIFEST_BYTES {
                    return Err("The manifest is larger than 256 KiB".into());
                }
                return Ok(Some(SkillFile { path, blob: entry.sha.clone(), size }));
            }
        }
        Ok(None)
    }

    pub fn marketplace_manifest(&self) -> Result<SkillFile, String> {
        self.manifest("", MARKETPLACE_PATHS)?.ok_or_else(|| "No plugin marketplace (marketplace.json) was found at the top of this branch.".into())
    }

    /// After downloading the selected marketplace file, select only in-scope, unambiguous plugin manifests.
    #[inline(never)]
    pub fn match_marketplace(&self, source: &RepoConfig, bytes: &[u8]) -> Result<PendingCatalog, String> {
        if source.kind != SourceKind::Plugins || !source.problems().is_empty() {
            return Err("Expected a valid plugin source with a named branch".into());
        }
        let file = self.marketplace_manifest()?;
        let market: Marketplace = read_manifest(&file, bytes)?;
        if !normal_name(&market.name) {
            return Err("The marketplace name is not in Copilot's normal form".into());
        }
        let mut counts = HashMap::new();
        for name in market.plugins.iter().filter_map(|row| row.get("name").and_then(Value::as_str)) {
            *counts.entry(name_key(name)).or_insert(0usize) += 1;
        }
        let mut catalog = Catalog {
            origin: source.into(),
            market: market.name,
            owner: author_name(market.owner.as_ref()),
            plugins: NameMap::new(),
            hosted_elsewhere: Vec::new(),
            rejected: Vec::new(),
            held: Vec::new(),
        };
        let mut matched = Vec::new();
        for entry in market.plugins {
            let name = entry.get("name").and_then(Value::as_str).unwrap_or_default();
            match entry.get("source") {
                Some(Value::Object(_)) => catalog.hosted_elsewhere.push(name.into()),
                Some(Value::String(s)) if is_url(s) => catalog.hosted_elsewhere.push(name.into()),
                Some(Value::String(s)) => {
                    let candidate = (|| {
                        // `./` names the whole repository. It is not the same as a missing source field.
                        let folder = if s == "./" { "" } else { checked_folder(s)? };
                        if !source.path.is_empty() && folder != source.path && !folder.strip_prefix(&source.path).is_some_and(|rest| rest.starts_with('/')) {
                            return Ok(None);
                        }
                        if !normal_name(name) {
                            return Err("The plugin name is not in Copilot's normal form".into());
                        }
                        if counts[&name_key(name)] != 1 {
                            return Err("Duplicate plugin name".into());
                        }
                        let sha = if folder.is_empty() {
                            self.root
                        } else {
                            &self.entry(folder)?.filter(|e| plain_tree(e)).ok_or("The plugin source is not a plain folder in this branch")?.sha
                        };
                        let file = self.manifest(folder, PLUGIN_PATHS)?.ok_or("No plugin manifest (plugin.json) was found in the plugin folder")?;
                        Ok(Some(Matched { name: name.into(), folder: folder.into(), tree: sha.into(), file }))
                    })();
                    match candidate {
                        Ok(Some(m)) => matched.push((entry, m)),
                        Ok(None) => {}
                        Err(reason) => catalog.rejected.push(Rejected { name: name.into(), reason }),
                    }
                }
                _ => catalog.rejected.push(Rejected { name: name.into(), reason: "The marketplace entry has no readable plugin source".into() }),
            }
        }
        Ok(PendingCatalog { catalog, matched })
    }

    /// Whether the listing has any of `candidates` below `folder`, in any spelling Windows treats as the same.
    pub(crate) fn has_any(&self, folder: &str, candidates: &[&str]) -> bool {
        candidates.iter().any(|rel| self.paths.contains_key(&path_key(&join(folder, rel))))
    }
}

#[derive(Deserialize)]
struct Marketplace {
    name: String,
    owner: Option<Value>,
    plugins: Vec<Value>,
}

struct Matched {
    name: String,
    folder: String,
    file: SkillFile,
    tree: String,
}

pub struct PendingCatalog {
    catalog: Catalog,
    matched: Vec<(Value, Matched)>,
}

impl PendingCatalog {
    pub fn manifests(&self) -> impl Iterator<Item = &SkillFile> {
        let mut paths = HashSet::new();
        self.matched.iter().map(|(_, m)| &m.file).filter(move |f| paths.insert(f.path.as_str()))
    }

    /// `manifests` is keyed by the exact branch-root paths returned by `manifests()`. The engine still
    /// aborts a check on a download or pin failure; absent/malformed plugin manifests are held, not removed.
    pub fn finish(mut self, manifests: &NameMap<Vec<u8>>) -> Catalog {
        for (entry, matched) in self.matched {
            match matched.facts(entry, manifests, &self.catalog.owner) {
                Ok(plugin) => {
                    self.catalog.plugins.insert(plugin.name.clone(), plugin);
                }
                Err(rejected) => self.catalog.rejected.push(rejected),
            }
        }
        self.catalog
    }
}

impl Matched {
    #[inline(never)]
    fn facts(self, entry: Value, manifests: &NameMap<Vec<u8>>, owner: &Option<String>) -> Result<Plugin, Rejected> {
        let reject = |reason: &str| Rejected { name: self.name.clone(), reason: reason.into() };
        let bytes = manifests.get(&self.file.path).ok_or_else(|| reject("The plugin manifest could not be read"))?;
        let manifest: NameMap<Value> = read_manifest(&self.file, bytes).map_err(|why| reject(&why))?;
        if manifest.get("name").and_then(Value::as_str) != Some(&self.name) {
            return Err(reject("The plugin manifest name does not match the marketplace entry"));
        }
        let version = match manifest.get("version").filter(|v| !v.is_null()).or_else(|| entry.get("version").filter(|v| !v.is_null())) {
            Some(value) => Some(value.as_str().ok_or_else(|| reject("The plugin version is not a string"))?.to_owned()),
            None => None,
        };
        let publisher = author_name(manifest.get("author")).or_else(|| owner.clone());
        for (field, value) in [("publisher", publisher.as_deref()), ("version", version.as_deref())] {
            if let Some(text) = value {
                if text.chars().count() > 64 {
                    return Err(reject(&format!("The plugin {field} is longer than 64 characters")));
                }
                if text.chars().any(|ch| ch.is_control() || invisible_format(ch)) {
                    return Err(reject(&format!("The plugin {field} contains a control or invisible formatting character")));
                }
            }
        }
        let mut sha = hash::Sha1::new();
        sha.update(&serde_json::to_vec(&entry).expect("JSON values can be serialized"));
        sha.update(self.tree.as_bytes());
        sha.update(self.file.blob.as_bytes());
        Ok(Plugin {
            name: self.name,
            path: self.folder,
            version,
            publisher,
            description: string(manifest.get("description")).or_else(|| string(entry.get("description"))),
            fingerprint: hash::hex(&sha.finish()),
        })
    }
}

fn invisible_format(ch: char) -> bool {
    matches!(ch, '\u{ad}' | '\u{61c}' | '\u{180e}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{2069}' | '\u{feff}')
}

#[inline(never)]
fn read_manifest<'de, T: Deserialize<'de>>(file: &SkillFile, bytes: &'de [u8]) -> Result<T, String> {
    if bytes.len() as u64 != file.size || hash::blob_hex(bytes) != file.blob {
        return Err("The manifest does not match the branch listing".into());
    }
    read_json_object(bytes).ok_or_else(|| "The manifest is not a readable JSON object with the expected fields".into())
}

fn checked_folder(path: &str) -> Result<&str, String> {
    let path = path.strip_prefix("./").unwrap_or(path);
    pathsafe::check_rel_path(path).map_err(|why| format!("Unsafe plugin source path: {why}"))?;
    Ok(path)
}

fn is_url(path: &str) -> bool {
    path.split_once("://").is_some_and(|(scheme, _)| {
        scheme.len() > 1 && scheme.starts_with(|c: char| c.is_ascii_alphabetic()) && scheme.bytes().all(|b| b.is_ascii_alphanumeric() || b"+-.".contains(&b))
    })
}

fn join(folder: &str, rel: &str) -> String {
    if folder.is_empty() { rel.into() } else { format!("{folder}/{rel}") }
}

fn plain_tree(entry: &TreeEntry) -> bool {
    entry.kind == EntryKind::Tree && entry.mode == "040000"
}
fn string(value: Option<&Value>) -> Option<String> {
    value?.as_str().map(str::to_owned)
}
fn author_name(value: Option<&Value>) -> Option<String> {
    string(value?.get("name"))
}

impl CatalogTree<'_> {
    pub(crate) fn inventory_marketplace_file(&self) -> Result<Option<SkillFile>, String> {
        self.manifest("", MARKETPLACE_PATHS)
    }

    pub(crate) fn inventory_plugin_file(&self, folder: &str) -> Result<Option<SkillFile>, String> {
        self.manifest(folder, PLUGIN_PATHS)
    }
}

pub(crate) fn inventory_manifest_candidate(entry: &TreeEntry) -> Option<SkillFile> {
    let size = entry.size?;
    (entry.kind == EntryKind::Blob && matches!(entry.mode.as_str(), "100644" | "100755") && size <= MAX_MANIFEST_BYTES).then(|| SkillFile {
        path: entry.path.clone(),
        blob: entry.sha.clone(),
        size,
    })
}

pub(crate) struct InventoryMarketplace {
    pub name: String,
    pub owner: Option<String>,
    pub entries: Vec<InventoryMarketEntry>,
    pub problem: Option<String>,
}

pub(crate) struct InventoryMarketEntry {
    pub folder: String,
    pub fields: Value,
    pub problem: Option<String>,
}

/// The inventory needs the rows even if their manifests are broken or missing. This is not a Catalog eligible for approval.
pub(crate) fn inventory_marketplace(bytes: &[u8]) -> Result<InventoryMarketplace, String> {
    let market: Marketplace = read_json_object(bytes).ok_or("The marketplace is not a readable JSON object with the expected fields")?;
    let problem = (!normal_name(&market.name)).then(|| "The marketplace name is not in Copilot's normal form".into());
    let mut counts = HashMap::new();
    for name in market.plugins.iter().filter_map(|row| row.get("name").and_then(Value::as_str)) {
        *counts.entry(name_key(name)).or_insert(0usize) += 1;
    }
    let mut entries = Vec::new();
    for fields in market.plugins {
        let Some(source) = fields.get("source").and_then(Value::as_str).filter(|s| !s.is_empty() && !is_url(s)) else { continue };
        let name = fields.get("name").and_then(Value::as_str).unwrap_or_default();
        let checked = if source == "./" { Ok("") } else { checked_folder(source) };
        let problem = checked.as_ref().err().cloned().or_else(|| {
            if !normal_name(name) {
                Some("The plugin name is not in Copilot's normal form".into())
            } else if counts[&name_key(name)] != 1 {
                Some("Duplicate plugin name".into())
            } else {
                None
            }
        });
        let folder = checked.unwrap_or_else(|_| source.strip_prefix("./").unwrap_or(source)).to_owned();
        entries.push(InventoryMarketEntry { folder, fields, problem });
    }
    Ok(InventoryMarketplace { name: market.name, owner: inventory_author(market.owner.as_ref()), entries, problem })
}

/// Both inventory parsers take bytes the caller has checked against the exact entry in the branch listing.
pub(crate) fn inventory_plugin_document(bytes: &[u8]) -> Result<NameMap<Value>, String> {
    read_json_object(bytes).ok_or_else(|| "The plugin manifest is not a readable JSON object".into())
}

pub(crate) fn inventory_author(author: Option<&Value>) -> Option<String> {
    author_name(author).or_else(|| string(author))
}

pub(crate) fn inventory_metadata_problem(field: &str, value: &str) -> Option<String> {
    if value.chars().count() > 64 {
        Some(format!("The plugin {field} is longer than 64 characters"))
    } else if value.chars().any(|ch| ch.is_control() || invisible_format(ch)) {
        Some(format!("The plugin {field} contains a control or invisible formatting character"))
    } else {
        None
    }
}

#[cfg(test)]
mod tests;
