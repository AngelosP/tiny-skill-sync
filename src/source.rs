//! Source and preview contracts. Discovery supplies the complete set of installable names;
//! destination resolution asks only about those names at the two supported locations.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf, Prefix};

use serde::{Deserialize, Serialize};

use crate::config::RepoConfig;
use crate::pathsafe;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", try_from = "String")]
pub enum SourceKind {
    #[default]
    Skills,
    Agents,
    Instructions,
    Plugins,
}

impl TryFrom<String> for SourceKind {
    type Error = String;

    fn try_from(s: String) -> Result<Self, String> {
        match s.as_str() {
            "skills" => Ok(Self::Skills),
            "agents" => Ok(Self::Agents),
            "instructions" => Ok(Self::Instructions),
            "plugins" => Ok(Self::Plugins),
            _ => Err(format!("unknown source kind '{s}'; expected skills, agents, instructions or plugins")),
        }
    }
}

impl SourceKind {
    pub(crate) fn is_skills(&self) -> bool {
        *self == Self::Skills
    }

    /// `skills` is the already resolved local or portable skills setting, including its legacy override.
    pub fn root(self, skills: &Path) -> Result<PathBuf, String> {
        self.root_with(skills, |key| std::env::var_os(key))
    }

    pub(crate) fn root_with(self, skills: &Path, env: impl Fn(&str) -> Option<OsString>) -> Result<PathBuf, String> {
        let (folder, key) = match self {
            Self::Skills => ("skills", "TSS_SKILLS_DIR"),
            Self::Agents => ("agents", "TSS_AGENTS_DIR"),
            Self::Instructions => ("instructions", "TSS_INSTRUCTIONS_DIR"),
            Self::Plugins => return Err("Plugins do not have a file destination".into()),
        };
        let isolated = ["TSS_HOME", "TSS_SKILLS_DIR", "TSS_TEST_ROOT", "TSS_AGENTS_DIR", "TSS_INSTRUCTIONS_DIR"].iter().any(|key| env(key).is_some());
        if self.is_skills() && !isolated {
            return Ok(skills.to_owned());
        }
        let profile = env("USERPROFILE").map(PathBuf::from);
        let path = match (self, env(key)) {
            (Self::Skills, _) => skills.to_owned(),
            (_, Some(p)) => PathBuf::from(p),
            (_, None) if isolated => return Err(format!("set {key} to an isolated folder for this copy")),
            (_, None) => profile.as_deref().ok_or("USERPROFILE is not set")?.join(".copilot").join(folder),
        };
        check_root(&path)?;
        if isolated {
            if !self.is_skills() && profile.as_deref().is_none_or(|p| check_root(p).is_err()) {
                return Err(format!("cannot check {key} against the Copilot profile: USERPROFILE is missing or is not a valid absolute path"));
            }
            if let Some(profile) = profile {
                // This rules out overlapping path names, not links or other filesystem aliases.
                let protected = folded_path(&profile.join(".copilot"));
                let chosen = folded_path(&path);
                if chosen.starts_with(&protected) || protected.starts_with(chosen) {
                    return Err(format!("{key} must name an isolated folder outside the Copilot profile"));
                }
            }
        }
        Ok(path)
    }
}

fn folded_path(path: &Path) -> PathBuf {
    PathBuf::from(pathsafe::fold(&path.to_string_lossy()))
}

pub(crate) fn check_root(path: &Path) -> Result<(), String> {
    if !path.is_absolute() || !matches!(path.components().next(), Some(Component::Prefix(p)) if matches!(p.kind(), Prefix::Disk(_) | Prefix::UNC(_, _))) {
        return Err("the destination root must be a full drive or network share path".into());
    }
    for c in path.components() {
        match c {
            Component::Normal(s) => pathsafe::check_component(s.to_str().ok_or("the destination cannot be recorded as text")?).map_err(|e| e.to_string())?,
            Component::ParentDir => return Err("the destination root must not contain '..'".into()),
            _ => {}
        }
    }
    Ok(())
}

/// Local to one source ID on one machine. Never store this in a settings companion.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DestinationBinding {
    #[serde(deserialize_with = "loose_kind")]
    kind: SourceKind,
    root: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    child: Option<String>,
}

fn loose_kind<'de, D: serde::Deserializer<'de>>(d: D) -> Result<SourceKind, D::Error> {
    let kind = SourceKind::deserialize(d)?;
    check_loose_kind(kind).map_err(serde::de::Error::custom)?;
    Ok(kind)
}

fn check_loose_kind(kind: SourceKind) -> Result<(), &'static str> {
    match kind {
        SourceKind::Agents | SourceKind::Instructions => Ok(()),
        SourceKind::Skills => Err("skills cannot have a loose-file destination"),
        SourceKind::Plugins => Err("Plugins do not have a file destination"),
    }
}

impl DestinationBinding {
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub fn kind(&self) -> SourceKind {
        self.kind
    }

    pub fn directory(&self) -> PathBuf {
        self.child.as_ref().map_or_else(|| self.root.clone(), |name| self.root.join(name))
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        check_loose_kind(self.kind)?;
        check_root(&self.root)?;
        if let Some(child) = &self.child {
            pathsafe::check_component(child).map_err(|e| format!("unsafe destination folder: {e}"))?;
        }
        Ok(())
    }

    pub(crate) fn check_source(&self, source: &RepoConfig) -> Result<(), String> {
        self.validate()?;
        if self.kind != source.kind || self.child.as_deref().is_some_and(|c| source.owner_and_name().is_none_or(|(_, n)| !c.eq_ignore_ascii_case(n))) {
            return Err(format!("source '{}' does not match its recorded destination", source.id));
        }
        Ok(())
    }
}

/// Read-only evidence from the verifier. `CompatibleFile` means proven compatible for *this* source,
/// not merely a regular file. Report unreadable/uncertain paths as errors, never as missing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entry {
    Missing,
    Directory,
    CompatibleFile,
    Conflict,
    Link,
}

/// The probe must not follow links (including ancestors). It is asked about exact paths, never
/// to enumerate a directory. This is placement evidence, not authority to adopt, replace or delete.
pub fn resolve_destination(
    source: &RepoConfig,
    root: &Path,
    names: &[String],
    bound: Option<&DestinationBinding>,
    mut probe: impl FnMut(&Path) -> Result<Entry, String>,
) -> Result<DestinationBinding, String> {
    let flat = DestinationBinding { kind: source.kind, root: root.into(), child: None };
    flat.validate()?;
    let (_, repo_name) = source.owner_and_name().ok_or("expected an owner/name repository")?;
    pathsafe::check_component(repo_name).map_err(|e| format!("unsafe repository folder name: {e}"))?;
    let nested = DestinationBinding { child: Some(repo_name.to_owned()), ..flat.clone() };
    if let Some(bound) = bound {
        bound.check_source(source)?;
        if folded_path(root) != folded_path(&bound.root) {
            return Err(format!("source '{}' is bound to another destination root", source.id));
        }
    } else if names.is_empty() {
        return Err("cannot select a new destination before the source lists a file".into());
    }
    for name in names {
        pathsafe::check_component(name).map_err(|e| format!("unsafe destination file name: {e}"))?;
    }
    let plain = |entry, dir: &Path| match entry {
        Entry::Directory => Ok(true),
        Entry::Missing => Ok(false),
        _ => Err(format!("{} is not a plain folder", dir.display())),
    };
    let at_root = plain(probe(root)?, root)?;
    let child = nested.directory();
    let at_child = at_root && plain(probe(&child)?, &child)?;
    let mut occupied = |dir: &Path| -> Result<bool, String> {
        let mut found = false;
        for name in names {
            let path = dir.join(name);
            match probe(&path)? {
                Entry::Missing => {}
                Entry::CompatibleFile => found = true,
                _ => return Err(format!("{} conflicts with this source", path.display())),
            }
        }
        Ok(found)
    };
    let here = at_root && occupied(root)?;
    let there = at_child && occupied(&child)?;
    if here && there {
        return Err(format!("source files exist in both {} and {}", root.display(), child.display()));
    }
    if let Some(bound) = bound {
        if (bound.child.is_some() && here) || (bound.child.is_none() && there) {
            return Err(format!("source files exist outside the recorded destination {}", bound.directory().display()));
        }
        return Ok(bound.clone());
    }
    Ok(if there { nested } else { flat })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreviewDestination {
    Resolved(PathBuf),
    Held(String),
    NotChecked,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourcePreview {
    pub plugins: Option<crate::plugins::catalog::Catalog>,
    pub kind: SourceKind,
    pub names: Vec<String>,
    pub destination: PreviewDestination,
    pub errors: Vec<String>,
}

impl SourcePreview {
    pub fn count(&self) -> usize {
        self.names.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(kind: SourceKind, vars: &[(&str, &str)]) -> Result<PathBuf, String> {
        kind.root_with(Path::new(r"D:\already-resolved\skills"), |key| vars.iter().find(|(k, _)| *k == key).map(|(_, v)| OsString::from(v)))
    }

    #[test]
    fn isolated_roots_are_explicit_and_never_derived_from_skills_or_profile() {
        let profile = ("USERPROFILE", r"C:\test-profile");
        for (kind, key, folder) in [(SourceKind::Agents, "TSS_AGENTS_DIR", "agents"), (SourceKind::Instructions, "TSS_INSTRUCTIONS_DIR", "instructions")] {
            assert_eq!(root(kind, &[profile]).unwrap(), Path::new(profile.1).join(".copilot").join(folder));
            for mode in ["TSS_HOME", "TSS_SKILLS_DIR", "TSS_TEST_ROOT"] {
                assert!(root(kind, &[profile, (mode, r"D:\sandbox")]).unwrap_err().contains(key));
            }
            for path in ["", "relative", r"D:\sandbox\..\other", r"C:\test-profile\.copilot\agents"] {
                assert!(root(kind, &[profile, (key, path)]).is_err(), "{path}");
            }
        }
        let agents = ("TSS_AGENTS_DIR", r"D:\sandbox\custom-a");
        let instructions = ("TSS_INSTRUCTIONS_DIR", r"E:\sandbox\custom-i");
        assert!(root(SourceKind::Agents, &[profile, instructions]).is_err());
        assert!(root(SourceKind::Instructions, &[profile, agents]).is_err());
        assert_eq!(root(SourceKind::Agents, &[profile, agents, instructions]).unwrap(), Path::new(agents.1));
        assert_eq!(root(SourceKind::Instructions, &[profile, agents, instructions]).unwrap(), Path::new(instructions.1));
        assert_eq!(root(SourceKind::Skills, &[profile, agents]).unwrap(), Path::new(r"D:\already-resolved\skills"));
        let default_skills = Path::new(profile.1).join(r".copilot\skills");
        assert!(
            SourceKind::Skills
                .root_with(&default_skills, |key| match key {
                    "USERPROFILE" => Some(profile.1.into()),
                    "TSS_HOME" => Some(r"D:\sandbox".into()),
                    _ => None,
                })
                .is_err()
        );
        assert!(root(SourceKind::Instructions, &[]).is_err());
    }

    #[test]
    fn places_profile_defaults_are_isolated_without_explicit_loose_roots() {
        let profile = ("USERPROFILE", r"D:\fixture\places-profile");
        let local = ("LOCALAPPDATA", r"D:\fixture\places-local");
        let no_saved_token = ("TSS_NO_SAVED_TOKEN", "1");
        let no_app_updates = ("TSS_NO_APP_UPDATES", "1");
        let vars = [profile, local, no_saved_token, no_app_updates];

        assert_eq!(root(SourceKind::Agents, &vars).unwrap(), Path::new(profile.1).join(".copilot").join("agents"));
        assert_eq!(root(SourceKind::Instructions, &vars).unwrap(), Path::new(profile.1).join(".copilot").join("instructions"));
    }

    #[test]
    fn issue7_isolated_loose_roots_require_a_trusted_profile() {
        let home = ("TSS_HOME", r"D:\fixture\settings");
        let mut unexpected = Vec::new();
        for (kind, key, path) in [
            (SourceKind::Agents, "TSS_AGENTS_DIR", r"D:\fixture\profile\.copilot\agents"),
            (SourceKind::Instructions, "TSS_INSTRUCTIONS_DIR", r"D:\fixture\profile\.copilot\instructions"),
        ] {
            let dest = (key, path);
            assert_eq!(root(kind, &[home, dest, ("USERPROFILE", r"C:\test-profile")]).unwrap(), Path::new(path));

            for profile in [None, Some(""), Some(r"fixture\profile"), Some(r"D:\fixture\other\..\profile")] {
                let mut vars = vec![home, dest];
                vars.extend(profile.map(|p| ("USERPROFILE", p)));
                let result = root(kind, &vars);
                if !result.as_ref().is_err_and(|e| {
                    let why = e.to_ascii_lowercase();
                    why.contains("profile") || why.contains("isolat")
                }) {
                    unexpected.push((kind, profile, result));
                }
            }
        }
        assert!(unexpected.is_empty(), "expected explicit isolation errors, got {unexpected:?}");
    }

    fn source() -> RepoConfig {
        RepoConfig { id: "source-id".into(), repo: "owner/repo".into(), kind: SourceKind::Agents, ..Default::default() }
    }

    fn resolve(facts: &[(&str, Entry)], bound: Option<&DestinationBinding>) -> Result<DestinationBinding, String> {
        let root = Path::new(r"D:\isolated\agents");
        resolve_destination(&source(), root, &["one.agent.md".into(), "two.agent.md".into()], bound, |path| {
            let rel = path.strip_prefix(root).unwrap().to_str().unwrap();
            // Asking about *anything* other than the root, safe repo child and listed leaves is a bug.
            assert!(["", "repo", "one.agent.md", "two.agent.md", r"repo\one.agent.md", r"repo\two.agent.md"].contains(&rel));
            Ok(facts.iter().find(|(name, _)| *name == rel).map_or(Entry::Missing, |(_, entry)| *entry))
        })
    }

    #[test]
    fn plugin_sources_never_select_a_file_destination() {
        let source = RepoConfig { kind: SourceKind::Plugins, ..source() };
        let root = Path::new(r"D:\isolated\plugins");
        assert!(source.kind.root_with(root, |_| panic!("no root lookup for plugins")).is_err());
        assert!(resolve_destination(&source, root, &["tool.agent.md".into()], None, |_| panic!("no file probe for plugins")).is_err());
        assert!(serde_json::from_str::<DestinationBinding>(r#"{"kind":"plugins","root":"D:\\isolated"}"#).is_err());
    }

    #[test]
    fn reuse_only_one_compatible_layout_and_do_not_relocate_a_bound_source() {
        use Entry::{CompatibleFile as File, Directory as Dir};
        let root = Path::new(r"D:\isolated\agents");
        let flat = resolve(&[("", Dir), ("repo", Dir)], None).unwrap(); // an empty repo child is not evidence
        assert_eq!(flat.directory(), root);
        assert_eq!(resolve(&[], None).unwrap(), flat);
        assert_eq!(resolve(&[("", Dir), ("two.agent.md", File)], None).unwrap(), flat);
        let facts = [("", Dir), ("repo", Dir), (r"repo\one.agent.md", File)];
        let nested = resolve(&facts, None).unwrap();
        assert_eq!(nested.directory(), root.join("repo"));
        assert_eq!(resolve(&[], Some(&nested)).unwrap(), nested, "missing files do not release an existing binding");
        assert!(resolve(&facts, Some(&flat)).is_err());
        assert!(resolve(&[("", Dir), ("two.agent.md", File)], Some(&nested)).is_err());
        assert!(resolve_destination(&source(), Path::new(r"E:\moved"), &[], Some(&nested), |_| panic!("no probe after root change")).is_err());
    }

    #[test]
    fn ambiguous_conflicting_and_unreadable_candidates_hold() {
        use Entry::{CompatibleFile as File, Conflict, Directory as Dir, Link};
        for facts in [
            vec![("", Dir), ("repo", Dir), ("one.agent.md", File), (r"repo\one.agent.md", File)],
            vec![("", Dir), ("repo", Dir), ("one.agent.md", File), (r"repo\two.agent.md", File)],
            vec![("", Dir), ("repo", Dir), ("one.agent.md", File), (r"repo\two.agent.md", Conflict)],
            vec![("", Dir), ("repo", Link)],
            vec![("", Link)],
            vec![("", Dir), ("one.agent.md", Link)],
            vec![("", Dir), ("two.agent.md", Dir)],
        ] {
            assert!(resolve(&facts, None).is_err(), "{facts:?}");
        }
        let root = Path::new(r"D:\isolated\agents");
        assert!(resolve_destination(&source(), root, &["leaf".into()], None, |p| if p == root { Ok(Dir) } else { Err("cannot read".into()) }).is_err());
        for (repo, name) in [("o/CON", "leaf"), ("o/../../x", "leaf"), ("o/r", "../leaf"), ("o/r", "con.txt")] {
            let source = RepoConfig { repo: repo.into(), ..source() };
            assert!(resolve_destination(&source, root, &[name.into()], None, |_| panic!("no unsafe paths probed")).is_err());
        }
    }
}
