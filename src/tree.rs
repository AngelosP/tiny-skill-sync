//! Discovers skills or loose files in a listing relative to the configured GitHub folder.
//!
//! A skill is a folder with a `SKILL.md`. If the configured folder itself holds a `SKILL.md`, the whole
//! folder is ONE skill (its sub-folders belong to it). Otherwise every immediate child folder with a
//! `SKILL.md` is a skill. A skill that breaks a safety rule is rejected as a whole, never half-installed.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::Deserialize;

use crate::config::RepoConfig;
use crate::pathsafe;
use crate::source::SourceKind;
use crate::win::fs::same_win_name;

pub const SKILL_FILE: &str = "SKILL.md";
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_SKILL_FILES: usize = 10_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Blob,
    Tree,
    Commit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub path: String,
    pub kind: EntryKind,
    pub mode: String,
    pub sha: String,
    pub size: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub sha: String,
    pub truncated: bool,
    pub entries: Vec<TreeEntry>,
}

#[derive(Deserialize)]
struct RawTree {
    #[serde(default)]
    sha: String,
    tree: Vec<RawEntry>,
    truncated: bool,
}

#[derive(Deserialize)]
struct RawEntry {
    path: String,
    mode: String,
    #[serde(rename = "type")]
    kind: String,
    sha: String,
    size: Option<u64>,
}

pub fn parse_snapshot(body: &[u8]) -> Result<Snapshot, String> {
    let raw: RawTree = serde_json::from_slice(body).map_err(|e| format!("unexpected tree response: {e}"))?;
    let mut entries = Vec::with_capacity(raw.tree.len());
    for e in raw.tree {
        let kind = match e.kind.as_str() {
            "blob" => EntryKind::Blob,
            "tree" => EntryKind::Tree,
            "commit" => EntryKind::Commit,
            other => return Err(format!("unknown tree entry type '{other}'")),
        };
        entries.push(TreeEntry { path: e.path, kind, mode: e.mode, sha: e.sha, size: e.size });
    }
    Ok(Snapshot { sha: raw.sha, truncated: raw.truncated, entries })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillFile {
    pub path: String,
    pub blob: String,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub files: Vec<SkillFile>,
}

impl Skill {
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rejected {
    pub name: String,
    pub reason: String,
}

#[derive(Clone, Debug, Default)]
pub struct Discovery {
    pub skills: Vec<Skill>,
    pub rejected: Vec<Rejected>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SourceItems {
    /// With `single`, file paths are already relative to the requested folder. Otherwise prepend `skill.name/`.
    Skills { single: bool, skills: Vec<Skill> },
    /// Each `path` is both the local leaf name and the exact `rel` for `Remote::file` at a pinned commit.
    Files(Vec<SkillFile>),
}

/// A fresh listing of one specific source, not a cached preview and not proof of local ownership.
#[derive(Debug)]
pub struct SourceDiscovery<'a> {
    source: &'a RepoConfig,
    tree_id: String,
    items: SourceItems,
    rejected: Vec<Rejected>,
    safe_for_absence: bool,
    leaf_names: LeafNames,
}

impl SourceDiscovery<'_> {
    pub fn items(&self) -> &SourceItems {
        &self.items
    }

    pub fn tree_id(&self) -> &str {
        &self.tree_id
    }

    pub fn rejected(&self) -> &[Rejected] {
        &self.rejected
    }

    pub fn names(&self) -> Vec<String> {
        match &self.items {
            SourceItems::Skills { skills, .. } => skills.iter().map(|s| s.name.clone()).collect(),
            SourceItems::Files(files) => files.iter().map(|f| f.path.clone()).collect(),
        }
    }

    /// Only this source's *recorded* scope is eligible to confirm missing names at a pin (`github::confirm_source_at`).
    /// For a loose file, `confirms_leaf_absent` also checks names omitted by the literal suffix filter.
    /// A clean empty listing still needs the mass-deletion guard; local ownership and full verification are separate.
    pub fn complete_for(&self, recorded: &RepoConfig) -> bool {
        self.safe_for_absence && self.rejected.is_empty() && self.same_scope(recorded)
    }

    pub(crate) fn covers_leaf(&self, recorded: &RepoConfig, name: &str) -> bool {
        self.same_scope(recorded)
            && pathsafe::check_component(name).is_ok()
            && loose_suffix(recorded.kind).is_some_and(|suffix| name.len() > suffix.len() && name.ends_with(suffix))
    }

    pub(crate) fn confirms_leaf_absent(&self, recorded: &RepoConfig, name: &str) -> bool {
        self.complete_for(recorded) && self.covers_leaf(recorded, name) && self.leaf_names.count(name) == 0
    }

    pub(crate) fn same_scope(&self, recorded: &RepoConfig) -> bool {
        same_source_scope(self.source, recorded)
    }
}

pub(crate) fn same_source_scope(source: &RepoConfig, recorded: &RepoConfig) -> bool {
    !recorded.id.trim().is_empty()
        && source.id == recorded.id
        && source.kind == recorded.kind
        && source.repo == recorded.repo
        && source.branch == recorded.branch
        && source.path == recorded.path
}

/// `snapshot` must be the reply to a request for `source` itself, not a slice of a repository-wide search.
/// `github::discover_source` keeps the request and its interpretation together.
pub(crate) fn discover_source<'a>(source: &'a RepoConfig, snapshot: Snapshot, install_root_len: usize) -> Result<SourceDiscovery<'a>, String> {
    if source.kind == SourceKind::Plugins {
        return Err("Plugins use the branch-root marketplace catalog".into());
    }
    let safe_for_absence = check_listing(source.kind, &snapshot)?;
    let (items, rejected, leaf_names) = if let Some(suffix) = loose_suffix(source.kind) {
        loose_files(&snapshot.entries, suffix, install_root_len)
    } else {
        let single = is_single_skill(&snapshot.entries);
        let Discovery { skills, rejected } = discover(&snapshot.entries, &source.single_skill_name(), install_root_len);
        (SourceItems::Skills { single, skills }, rejected, LeafNames::default())
    };
    Ok(SourceDiscovery { source, tree_id: snapshot.sha, items, rejected, safe_for_absence, leaf_names })
}

pub(crate) fn entry_index(snapshot: &Snapshot) -> Result<HashMap<&str, &TreeEntry>, String> {
    if snapshot.truncated {
        return Err("GitHub cut the folder listing short; nothing was changed".into());
    }
    if !crate::hash::is_sha1_hex(&snapshot.sha) {
        return Err("GitHub did not provide a valid ID for the tree it listed".into());
    }
    // A recursive Git tree includes a plain tree entry for every parent. Contradictory or missing
    // entries are protocol errors, not an empty folder. Git names are not Windows paths: never normalize them.
    let mut paths = HashMap::with_capacity(snapshot.entries.len());
    for e in &snapshot.entries {
        if e.path.contains('\0')
            || e.path.split('/').any(|p| matches!(p, "" | "." | ".."))
            || !crate::hash::is_sha1_hex(&e.sha)
            || paths.insert(e.path.as_str(), e).is_some()
        {
            return Err(format!("invalid or duplicate entry '{}' in the GitHub listing", pathsafe::printable(&e.path)));
        }
    }
    for e in &snapshot.entries {
        if let Some((parent, _)) = e.path.rsplit_once('/') {
            if !paths.get(parent).is_some_and(|p| p.kind == EntryKind::Tree && p.mode == "040000") {
                return Err(format!("the GitHub listing is missing a plain parent folder for '{}'", pathsafe::printable(&e.path)));
            }
        }
    }
    Ok(paths)
}

fn check_listing(kind: SourceKind, snapshot: &Snapshot) -> Result<bool, String> {
    entry_index(snapshot)?;
    // Positive matches can still be displayed independently. But filtering out a malformed entry,
    // non-plain object or Windows alias must never turn it into negative evidence about an owned name.
    let mut folded = HashSet::new();
    Ok(snapshot.entries.iter().all(|e| {
        pathsafe::check_rel_path(&e.path).is_ok()
            && folded.insert(pathsafe::fold(&e.path))
            && match e.kind {
                EntryKind::Tree => e.mode == "040000" && !(kind.is_skills() && e.path.rsplit('/').next() == Some(SKILL_FILE)),
                EntryKind::Blob => regular_mode(&e.mode) && e.size.is_some(),
                EntryKind::Commit => false,
            }
    }))
}

fn loose_suffix(kind: SourceKind) -> Option<&'static str> {
    match kind {
        SourceKind::Skills | SourceKind::Plugins => None,
        SourceKind::Agents => Some(".agent.md"),
        SourceKind::Instructions => Some(".instructions.md"),
    }
}

#[derive(Debug, Default)]
struct LeafNames {
    ascii: HashMap<String, usize>,
    unicode: Vec<(String, String)>,
}

impl LeafNames {
    fn record(&mut self, name: &str, suffix: &str) {
        let key = pathsafe::fold(name);
        if !key.ends_with(suffix) {
            // Both suffixes start at the penultimate dot. Windows' ordinal table can equate
            // spellings that Unicode lowercasing does not.
            let Some((i, _)) = name.rmatch_indices('.').nth(1) else { return };
            if name[i..].is_ascii() || !same_win_name(&name[i..], suffix) {
                return;
            }
        }
        if name.is_ascii() {
            *self.ascii.entry(key).or_default() += 1;
        } else {
            self.unicode.push((name.to_owned(), key));
        }
    }

    fn count(&self, name: &str) -> usize {
        // ASCII-to-ASCII identity is a hash lookup. Unicode also needs Windows' ordinal comparison.
        let key = pathsafe::fold(name);
        let ascii = if name.is_ascii() {
            self.ascii.get(&key).copied().unwrap_or(0)
        } else {
            self.ascii.iter().filter(|(k, _)| **k == key || same_win_name(k, name)).map(|(_, n)| n).sum()
        };
        ascii + self.unicode.iter().filter(|(n, k)| *k == key || same_win_name(n, name)).count()
    }
}

fn loose_files(entries: &[TreeEntry], suffix: &str, root_len: usize) -> (SourceItems, Vec<Rejected>, LeafNames) {
    let immediate = || entries.iter().filter(|e| !e.path.contains('/'));
    let mut names = LeafNames::default();
    for e in immediate() {
        names.record(&e.path, suffix);
    }
    let mut files = Vec::new();
    let mut rejected = Vec::new();
    for e in immediate().filter(|e| e.path.ends_with(suffix)) {
        let name = &e.path;
        let file = (|| {
            pathsafe::check_component(name).map_err(|why| why.to_string())?;
            if name.len() == suffix.len() {
                return Err(format!("expected a name before '{suffix}'"));
            }
            if names.count(name) != 1 {
                return Err("another entry has the same name apart from upper and lower case".into());
            }
            check_leaf_length(root_len, name)?;
            regular_file(e, name)
        })();
        match file {
            Ok(file) => files.push(file),
            Err(reason) => rejected.push(Rejected { name: name.clone(), reason }),
        }
    }
    files.sort_by_cached_key(|f| pathsafe::fold(&f.path));
    rejected.sort_by(|a, b| a.name.cmp(&b.name));
    (SourceItems::Files(files), rejected, names)
}

pub(crate) fn check_leaf_length(root_len: usize, name: &str) -> Result<(), String> {
    if root_len.saturating_add(1).saturating_add(name.encode_utf16().count()) > pathsafe::MAX_INSTALLED_PATH {
        return Err(format!("'{name}' would be installed at a path longer than {} characters", pathsafe::MAX_INSTALLED_PATH));
    }
    Ok(())
}

type Item<'a> = (&'a TreeEntry, &'a str);

/// True when the listed folder holds a `SKILL.md` itself, so the whole folder is one skill and its files are
/// not under a sub-folder named after it.
pub fn is_single_skill(entries: &[TreeEntry]) -> bool {
    entries.iter().any(|e| e.path == SKILL_FILE && e.kind == EntryKind::Blob)
}

/// `single_name` names the skill when the folder itself is one skill. `install_root_len` is the length of the
/// skills folder path, used to keep installed paths short enough for ordinary Windows tools.
pub fn discover(entries: &[TreeEntry], single_name: &str, install_root_len: usize) -> Discovery {
    let mut found = Discovery::default();
    let single = is_single_skill(entries);

    let mut groups: BTreeMap<&str, Vec<Item>> = BTreeMap::new();
    if single {
        groups.insert(single_name, entries.iter().map(|e| (e, e.path.as_str())).collect());
    } else {
        for e in entries {
            if let Some((top, rest)) = e.path.split_once('/') {
                if !top.starts_with('.') {
                    groups.entry(top).or_default().push((e, rest));
                }
            }
        }
        groups.retain(|_, items| items.iter().any(|(e, rel)| *rel == SKILL_FILE && e.kind == EntryKind::Blob));
    }

    for (name, items) in groups {
        match build_skill(name, &items, install_root_len) {
            Ok(skill) => found.skills.push(skill),
            Err(reason) => found.rejected.push(Rejected { name: name.to_string(), reason }),
        }
    }
    reject_name_collisions(&mut found);
    found.skills.sort_by(|a, b| pathsafe::fold(&a.name).cmp(&pathsafe::fold(&b.name)).then_with(|| a.name.cmp(&b.name)));
    found.rejected.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

fn reject_name_collisions(found: &mut Discovery) {
    let mut by_fold: HashMap<String, Vec<String>> = HashMap::new();
    for s in &found.skills {
        by_fold.entry(pathsafe::fold(&s.name)).or_default().push(s.name.clone());
    }
    for r in &found.rejected {
        by_fold.entry(pathsafe::fold(&r.name)).or_default().push(r.name.clone());
    }
    let clashing: Vec<String> = by_fold.into_iter().filter(|(_, names)| names.len() > 1).map(|(k, _)| k).collect();
    if clashing.is_empty() {
        return;
    }
    let (bad, good): (Vec<Skill>, Vec<Skill>) = std::mem::take(&mut found.skills).into_iter().partition(|s| clashing.contains(&pathsafe::fold(&s.name)));
    found.skills = good;
    for s in bad {
        found.rejected.push(Rejected { reason: "another skill has the same name apart from upper and lower case".to_string(), name: s.name });
    }
}

fn build_skill(name: &str, items: &[Item], install_root_len: usize) -> Result<Skill, String> {
    pathsafe::check_component(name).map_err(|e| format!("skill folder name: {e}"))?;
    if name.starts_with('.') {
        return Err("skill folder names cannot start with a dot".to_string());
    }

    let mut files = Vec::new();
    for (entry, rel) in items {
        pathsafe::check_rel_path(rel).map_err(|e| format!("'{rel}': {e}"))?;
        if entry.kind == EntryKind::Tree && entry.mode == "040000" {
            continue;
        }
        check_leaf_length(install_root_len.saturating_add(1).saturating_add(name.encode_utf16().count()), rel)?;
        files.push(regular_file(entry, rel)?);
    }
    if files.len() > MAX_SKILL_FILES {
        return Err(format!("it has more than {MAX_SKILL_FILES} files"));
    }
    check_case_collisions(&files)?;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Skill { name: name.to_string(), files })
}

fn regular_mode(mode: &str) -> bool {
    matches!(mode, "100644" | "100664" | "100755")
}

fn regular_file(entry: &TreeEntry, rel: &str) -> Result<SkillFile, String> {
    match entry.kind {
        EntryKind::Tree => return Err(format!("'{rel}' is not a regular file")),
        EntryKind::Commit => return Err(format!("'{rel}' is a submodule, which is not supported")),
        EntryKind::Blob => {}
    }
    match entry.mode.as_str() {
        mode if regular_mode(mode) => {}
        "120000" => return Err(format!("'{rel}' is a symbolic link, which is not supported")),
        other => return Err(format!("'{rel}' has an unsupported file mode {other}")),
    }
    let size = entry.size.ok_or_else(|| format!("'{rel}' has no size in the listing"))?;
    if size > MAX_FILE_BYTES {
        return Err(format!("'{rel}' is larger than {} MiB", MAX_FILE_BYTES / 1024 / 1024));
    }
    Ok(SkillFile { path: rel.to_string(), blob: entry.sha.clone(), size })
}

/// Windows treats `a.md` and `A.MD` as one file, so two such paths in a skill cannot both be installed.
fn check_case_collisions(files: &[SkillFile]) -> Result<(), String> {
    let mut seen_files: HashMap<String, &str> = HashMap::new();
    let mut seen_dirs: HashMap<String, &str> = HashMap::new();
    for f in files {
        if let Some(prev) = seen_files.insert(pathsafe::fold(&f.path), &f.path) {
            return Err(format!("'{prev}' and '{}' differ only by case", f.path));
        }
        for (i, _) in f.path.match_indices('/') {
            let dir = &f.path[..i];
            if let Some(prev) = seen_dirs.insert(pathsafe::fold(dir), dir) {
                if prev != dir {
                    return Err(format!("folders '{prev}' and '{dir}' differ only by case"));
                }
            }
        }
    }
    for f in files {
        if let Some(dir) = seen_dirs.get(&pathsafe::fold(&f.path)) {
            return Err(format!("'{}' is both a file and the folder '{dir}'", f.path));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reconcile::{self, LeafFacts, LeafListing, LocalState, Verdict};

    fn blob(path: &str, size: u64) -> TreeEntry {
        TreeEntry { path: path.to_string(), kind: EntryKind::Blob, mode: "100644".into(), sha: crate::hash::blob_hex(path.as_bytes()), size: Some(size) }
    }

    fn dir(path: &str) -> TreeEntry {
        TreeEntry { path: path.to_string(), kind: EntryKind::Tree, mode: "040000".into(), sha: "0".repeat(40), size: None }
    }

    fn names(d: &Discovery) -> Vec<&str> {
        d.skills.iter().map(|s| s.name.as_str()).collect()
    }

    fn fixture() -> Vec<TreeEntry> {
        vec![
            dir("alpha"),
            blob("alpha/SKILL.md", 10),
            dir("alpha/scripts"),
            blob("alpha/scripts/hello.ps1", 20),
            blob("beta/SKILL.md", 5),
            blob("not-a-skill/readme.txt", 5),
            blob("README.md", 5),
            blob(".hidden/SKILL.md", 5),
        ]
    }

    fn snapshot(entries: Vec<TreeEntry>) -> Snapshot {
        Snapshot { sha: "1".repeat(40), truncated: false, entries }
    }

    fn source(kind: SourceKind) -> RepoConfig {
        RepoConfig { id: "team".into(), repo: "owner/Team".into(), branch: "topic/team".into(), path: ".github/Team Files".into(), kind, ..Default::default() }
    }

    fn clean_owned_verdict(managed: &TreeEntry, listing: LeafListing<'_>) -> Verdict {
        let file = crate::state::ManagedFile { path: managed.path.clone(), blob: managed.sha.clone(), size: managed.size.unwrap(), fp: None, canon: None };
        reconcile::plan_leaf(
            listing,
            LeafFacts { owned: Some((&file, LocalState::Clean)), exists: true, matches_upstream: false, interrupted: false, ignored: false, other_owner: None },
        )
    }

    fn agents_listing_verdict(managed: &TreeEntry, entries: Vec<TreeEntry>) -> Result<Verdict, String> {
        let agents = source(SourceKind::Agents);
        let found = discover_source(&agents, snapshot(entries), 20)?;
        let listing = LeafListing::from_discovery(&managed.path, &found, &agents, false)?;
        Ok(clean_owned_verdict(managed, listing))
    }

    #[test]
    fn case_changed_agent_suffix_does_not_remove_a_managed_file() {
        let notes = blob("notes.agent.md", 14);
        let other = blob("other.agent.md", 14);
        assert_eq!(agents_listing_verdict(&notes, vec![other.clone(), notes.clone()]), Ok(Verdict::InSync));

        let renamed = TreeEntry { path: "notes.AGENT.MD".into(), ..notes.clone() };
        let decision = agents_listing_verdict(&notes, vec![other, renamed]);
        assert!(!matches!(decision, Ok(Verdict::Delete | Verdict::Forget)), "only the suffix casing changed: {decision:?}");
    }

    #[test]
    fn agents_listing_cannot_confirm_an_instruction_file_is_absent() {
        let note = blob("note.instructions.md", 20);
        assert_eq!(clean_owned_verdict(&note, LeafListing::confirmed_removal(&note.path)), Verdict::Delete);

        let decision = agents_listing_verdict(&note, vec![blob("other.agent.md", 14)]);
        assert!(!matches!(decision, Ok(Verdict::Delete | Verdict::Forget)), "the listing covers agents, not instructions: {decision:?}");
    }

    #[test]
    fn empty_listing_with_an_invalid_tree_id_is_not_complete() {
        let agents = source(SourceKind::Agents);
        let empty = Snapshot { sha: "g".repeat(40), ..snapshot(vec![]) };
        let found = discover_source(&agents, empty, 20);
        assert!(!found.is_ok_and(|listing| listing.complete_for(&agents)));
    }

    #[test]
    fn typed_discovery_keeps_both_skill_layouts_and_only_immediate_literal_leaves() {
        let entries = vec![
            blob("Plan #1.agent.md", 17),
            blob("Project.instructions.md", 9),
            blob("not-an-agent.AGENT.MD", 1),
            blob("readme.md", 1),
            dir("sub"),
            blob("sub/nested.agent.md", 4),
            blob("sub/SKILL.md", 5),
        ];
        for (kind, name) in [(SourceKind::Agents, "Plan #1.agent.md"), (SourceKind::Instructions, "Project.instructions.md")] {
            let source = source(kind);
            let found = discover_source(&source, snapshot(entries.clone()), 12).unwrap();
            assert!(found.complete_for(&source));
            let SourceItems::Files(files) = found.items() else { panic!("loose files are not skills") };
            assert_eq!(files.len(), 1);
            assert_eq!(files[0].path, name);
            assert_eq!(files[0].blob, crate::hash::blob_hex(name.as_bytes()));
        }
        let source = source(SourceKind::Skills);
        let found = discover_source(&source, snapshot(entries.clone()), 12).unwrap();
        assert!(matches!(found.items(), SourceItems::Skills { single: false, .. }));
        assert_eq!(found.names(), ["sub"]);
        let mut entries = entries;
        entries.push(blob("SKILL.md", 1));
        let found = discover_source(&source, snapshot(entries), 12).unwrap();
        let SourceItems::Skills { single: true, skills } = found.items() else { panic!("the folder itself is one skill") };
        assert_eq!(skills[0].name, "Team Files");
        assert!(skills[0].files.iter().any(|f| f.path == "sub/nested.agent.md"));
    }

    #[test]
    fn rejections_are_not_absence_even_with_an_installable_leaf() {
        let source = source(SourceKind::Agents);
        let entries = vec![
            blob("ok.agent.md", 1),
            blob("Mixed.agent.md", 1),
            blob("mixed.AGENT.md", 1),
            blob("CON.agent.md", 0),
            blob(r"a\b.agent.md", 0),
            blob("a\nb.agent.md", 0),
            blob(".agent.md", 0),
            dir("folder.agent.md"),
            TreeEntry { mode: "120000".into(), ..blob("link.agent.md", 0) },
            TreeEntry { kind: EntryKind::Commit, mode: "160000".into(), ..blob("submodule.agent.md", 0) },
            TreeEntry { mode: "000000".into(), ..blob("mode.agent.md", 0) },
            TreeEntry { size: None, ..blob("no-size.agent.md", 0) },
            blob("large.agent.md", MAX_FILE_BYTES + 1),
        ];
        let found = discover_source(&source, snapshot(entries), 20).unwrap();
        assert_eq!(found.names(), ["ok.agent.md"]);
        assert_eq!(found.rejected().len(), 11);
        assert!(found.rejected().iter().all(|r| !r.reason.is_empty()));
        assert!(!found.complete_for(&source));
        // A supplementary character occupies two UTF-16 units. Exactly at the limit is installable.
        let name = "📎.agent.md";
        let limit = pathsafe::MAX_INSTALLED_PATH - 1 - name.encode_utf16().count();
        let listed = || snapshot(vec![blob(name, 0)]);
        assert_eq!(discover_source(&source, listed(), limit).unwrap().names(), [name]);
        let too_long = discover_source(&source, listed(), limit + 1).unwrap();
        assert!(too_long.names().is_empty() && !too_long.complete_for(&source));
    }

    #[test]
    fn only_a_complete_tree_of_the_same_id_kind_repo_branch_and_folder_can_report_absence() {
        let source = source(SourceKind::Instructions);
        let none = discover_source(&source, snapshot(vec![blob("README.md", 2)]), 20).unwrap();
        assert!(none.names().is_empty() && none.rejected().is_empty() && none.complete_for(&source));
        for other in [
            RepoConfig { id: "other".into(), ..source.clone() },
            RepoConfig { repo: "other/Team".into(), ..source.clone() },
            RepoConfig { kind: SourceKind::Agents, ..source.clone() },
            RepoConfig { branch: "main".into(), ..source.clone() },
            RepoConfig { path: ".github/team files".into(), ..source.clone() },
        ] {
            assert!(!none.complete_for(&other));
        }
        for bad in [
            Snapshot { truncated: true, ..snapshot(vec![]) },
            snapshot(vec![blob("../x.instructions.md", 1)]),
            snapshot(vec![blob("missing-parent/x.instructions.md", 1)]),
            snapshot(vec![blob("x", 1), blob("x/leaf", 1)]),
            snapshot(vec![blob("duplicate", 1), dir("duplicate")]),
            snapshot(vec![TreeEntry { sha: "not-an-id".into(), ..blob("x.instructions.md", 1) }]),
        ] {
            assert!(discover_source(&source, bad, 20).is_err());
        }
        // A malformed row is not evidence that an owned file disappeared, even if it wasn't
        // one of the positive matches. Windows paths that alias or trim must not imply absence.
        for odd in [blob("old.instructions.md.", 1), TreeEntry { mode: "000000".into(), ..blob("README.md", 0) }, TreeEntry { size: None, ..blob("old", 0) }] {
            let found = discover_source(&source, snapshot(vec![blob("good.instructions.md", 1), odd]), 20).unwrap();
            assert_eq!(found.names(), ["good.instructions.md"]);
            assert!(!found.complete_for(&source));
        }
    }

    #[test]
    fn finds_child_folders_that_have_a_skill_file() {
        let d = discover(&fixture(), "ignored", 20);
        assert_eq!(names(&d), ["alpha", "beta"]);
        assert!(d.rejected.is_empty());
        let alpha = &d.skills[0];
        assert_eq!(alpha.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), ["SKILL.md", "scripts/hello.ps1"]);
        assert_eq!(alpha.total_bytes(), 30);
    }

    #[test]
    fn a_folder_with_its_own_skill_file_is_one_skill() {
        let entries = vec![blob("SKILL.md", 5), dir("refs"), blob("refs/a.md", 5), blob("child/SKILL.md", 5), blob("README.md", 1)];
        let d = discover(&entries, "my-skill", 20);
        assert_eq!(names(&d), ["my-skill"]);
        let paths: Vec<_> = d.skills[0].files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["README.md", "SKILL.md", "child/SKILL.md", "refs/a.md"]);
        assert!(is_single_skill(&entries));
        assert!(!is_single_skill(&fixture()), "a SKILL.md one level down does not make the folder itself a skill");
        assert!(!is_single_skill(&[dir("SKILL.md")]), "only a file counts");
    }

    #[test]
    fn single_skill_name_must_be_safe() {
        let d = discover(&[blob("SKILL.md", 1)], ".github", 20);
        assert!(d.skills.is_empty());
        assert_eq!(d.rejected.len(), 1);
        let d = discover(&[blob("SKILL.md", 1)], "CON", 20);
        assert_eq!(d.rejected.len(), 1);
    }

    #[test]
    fn empty_and_skill_free_trees_have_no_skills() {
        assert!(discover(&[], "x", 20).skills.is_empty());
        let d = discover(&[blob("docs/a.md", 1), blob("b.md", 1)], "x", 20);
        assert!(d.skills.is_empty() && d.rejected.is_empty());
    }

    #[test]
    fn hostile_entries_reject_only_their_own_skill() {
        let mut entries = fixture();
        entries.push(blob("evil/SKILL.md", 1));
        entries.push(blob("evil/..%2F..%2Fx", 1));
        entries.push(blob("evil/a\\b", 1));
        let d = discover(&entries, "x", 20);
        assert_eq!(names(&d), ["alpha", "beta"]);
        assert_eq!(d.rejected.len(), 1);
        assert_eq!(d.rejected[0].name, "evil");
    }

    #[test]
    fn symlinks_and_submodules_reject_the_skill() {
        let mut entries = fixture();
        entries.push(TreeEntry { mode: "120000".into(), ..blob("beta/link", 4) });
        let d = discover(&entries, "x", 20);
        assert_eq!(names(&d), ["alpha"]);
        assert!(d.rejected[0].reason.contains("symbolic link"));

        let mut entries = fixture();
        entries.push(TreeEntry { kind: EntryKind::Commit, mode: "160000".into(), ..blob("alpha/sub", 0) });
        let d = discover(&entries, "x", 20);
        assert_eq!(names(&d), ["beta"]);
        assert!(d.rejected[0].reason.contains("submodule"));
    }

    #[test]
    fn windows_unsafe_names_reject_the_skill() {
        for bad in ["alpha/con", "alpha/nul.txt", "alpha/a:b", "alpha/trailing.", "alpha/sp ", "alpha/../x"] {
            let mut entries = fixture();
            entries.push(blob(bad, 1));
            let d = discover(&entries, "x", 20);
            assert_eq!(names(&d), ["beta"], "{bad}");
        }
    }

    #[test]
    fn case_collisions_reject_the_skill() {
        let mut entries = fixture();
        entries.push(blob("alpha/Scripts/x.ps1", 1));
        let d = discover(&entries, "x", 20);
        assert_eq!(names(&d), ["beta"]);
        assert!(d.rejected[0].reason.contains("differ only by case"));

        let mut entries = fixture();
        entries.push(blob("alpha/SKILL.MD", 1));
        assert_eq!(names(&discover(&entries, "x", 20)), ["beta"]);

        let mut entries = fixture();
        entries.push(blob("alpha/Scripts", 1));
        assert_eq!(names(&discover(&entries, "x", 20)), ["beta"]);
    }

    #[test]
    fn skills_whose_names_differ_only_by_case_are_both_rejected() {
        let entries = vec![blob("Dup/SKILL.md", 1), blob("dup/SKILL.md", 1), blob("ok/SKILL.md", 1)];
        let d = discover(&entries, "x", 20);
        assert_eq!(names(&d), ["ok"]);
        assert_eq!(d.rejected.len(), 2);
    }

    #[test]
    fn long_installed_paths_are_rejected() {
        let long = format!("alpha/{}.md", "x".repeat(240));
        let mut entries = fixture();
        entries.push(blob(&long, 1));
        assert_eq!(names(&discover(&entries, "x", 40)), ["beta"]);
        // The same file is fine when the skills folder path is short enough.
        assert_eq!(names(&discover(&entries, "x", 2)), ["alpha", "beta"]);
    }

    #[test]
    fn oversized_files_are_rejected() {
        let mut entries = fixture();
        entries.push(blob("alpha/big.bin", MAX_FILE_BYTES + 1));
        assert_eq!(names(&discover(&entries, "x", 20)), ["beta"]);
        let mut entries = fixture();
        entries.push(TreeEntry { size: None, ..blob("alpha/nosize", 0) });
        assert_eq!(names(&discover(&entries, "x", 20)), ["beta"]);
    }

    #[test]
    fn parses_the_github_response() {
        let body = br#"{"sha":"0123456789abcdef0123456789abcdef01234567","url":"u","tree":[
            {"path":"alpha","mode":"040000","type":"tree","sha":"0123456789abcdef0123456789abcdef01234567","url":"u"},
            {"path":"alpha/SKILL.md","mode":"100644","type":"blob","sha":"0123456789abcdef0123456789abcdef01234567","size":42,"url":"u"}],"truncated":false}"#;
        let snap = parse_snapshot(body).unwrap();
        assert_eq!(snap.sha, "0123456789abcdef0123456789abcdef01234567");
        assert!(!snap.truncated);
        assert_eq!(snap.entries.len(), 2);
        assert_eq!(snap.entries[1].size, Some(42));
        assert_eq!(snap.entries[0].kind, EntryKind::Tree);
        assert!(parse_snapshot(b"{").is_err());
        assert!(parse_snapshot(br#"{"tree":[{"path":"a","mode":"1","type":"weird","sha":"0123456789abcdef0123456789abcdef01234567"}]}"#).is_err());
        assert!(parse_snapshot(br#"{"truncated":true,"tree":[]}"#).unwrap().truncated);
    }
}
