//! Reads a GitHub tree listing and decides which folders in it are skills.
//!
//! A skill is a folder with a `SKILL.md`. If the configured folder itself holds a `SKILL.md`, the whole
//! folder is ONE skill (its sub-folders belong to it). Otherwise every immediate child folder with a
//! `SKILL.md` is a skill. A skill that breaks a safety rule is rejected as a whole, never half-installed.

use std::collections::{BTreeMap, HashMap};

use serde::Deserialize;

use crate::pathsafe;

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
        match entry.kind {
            EntryKind::Tree => continue,
            EntryKind::Commit => return Err(format!("'{rel}' is a submodule, which is not supported")),
            EntryKind::Blob => {}
        }
        match entry.mode.as_str() {
            "100644" | "100664" | "100755" => {}
            "120000" => return Err(format!("'{rel}' is a symbolic link, which is not supported")),
            other => return Err(format!("'{rel}' has an unsupported file mode {other}")),
        }
        let size = entry.size.ok_or_else(|| format!("'{rel}' has no size in the listing"))?;
        if size > MAX_FILE_BYTES {
            return Err(format!("'{rel}' is larger than {} MiB", MAX_FILE_BYTES / 1024 / 1024));
        }
        if pathsafe::installed_len(install_root_len, name, rel) > pathsafe::MAX_INSTALLED_PATH {
            return Err(format!("'{rel}' would be installed at a path longer than {} characters", pathsafe::MAX_INSTALLED_PATH));
        }
        files.push(SkillFile { path: (*rel).to_string(), blob: entry.sha.clone(), size });
    }
    if files.len() > MAX_SKILL_FILES {
        return Err(format!("it has more than {MAX_SKILL_FILES} files"));
    }
    check_case_collisions(&files)?;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Skill { name: name.to_string(), files })
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
        let body = br#"{"sha":"a06be6f0","url":"u","tree":[
            {"path":"alpha","mode":"040000","type":"tree","sha":"11","url":"u"},
            {"path":"alpha/SKILL.md","mode":"100644","type":"blob","sha":"22","size":42,"url":"u"}],"truncated":false}"#;
        let snap = parse_snapshot(body).unwrap();
        assert_eq!(snap.sha, "a06be6f0");
        assert!(!snap.truncated);
        assert_eq!(snap.entries.len(), 2);
        assert_eq!(snap.entries[1].size, Some(42));
        assert_eq!(snap.entries[0].kind, EntryKind::Tree);
        assert!(parse_snapshot(b"{").is_err());
        assert!(parse_snapshot(br#"{"tree":[{"path":"a","mode":"1","type":"weird","sha":"1"}]}"#).is_err());
        assert!(parse_snapshot(br#"{"truncated":true,"tree":[]}"#).unwrap().truncated);
    }
}
