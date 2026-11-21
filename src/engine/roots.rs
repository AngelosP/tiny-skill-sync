//! Keep settings, destinations and scratch space separate before either transaction implementation runs.
//! Match missing suffixes relative to known directory identities too (two drive letters may name one volume).

use super::*;
use crate::source::SourceKind;
use crate::state::DiskId;
use crate::win::fs::{LeafDirectory, check_leaf_path, same_win_name};

struct Location {
    path: PathBuf,
    role: Role,
    addresses: Vec<(DiskId, PathBuf)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Home,
    Destination(SourceKind),
    SkillWork,
    LeafWork,
}

pub(super) struct Roots {
    locations: Vec<Location>,
    pins: HashMap<PathBuf, (LeafDirectory, DiskId)>,
}

fn prefix(a: &Path, b: &Path) -> bool {
    let mut parts = b.components();
    a.components().all(|c| parts.next().is_some_and(|p| same_win_name(&c.as_os_str().to_string_lossy(), &p.as_os_str().to_string_lossy())))
}

fn overlap(a: &Path, b: &Path) -> bool {
    prefix(a, b) || prefix(b, a)
}

impl Roots {
    fn add(&mut self, path: PathBuf, role: Role) -> Result<(), String> {
        let err = |e| format!("cannot validate {}: {e}", path.display());
        check_leaf_path(&path).map_err(err)?;
        let mut addresses = Vec::new();
        for dir in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
            if !self.pins.contains_key(dir) {
                match LeafDirectory::pin(dir, false) {
                    Ok(pin) => {
                        let id = pin.id().map_err(err)?;
                        self.pins.insert(dir.into(), (pin, id));
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound && dir.parent().is_some() => break,
                    Err(e) => return Err(err(e)),
                }
            }
            addresses.push((self.pins[dir].1, path.strip_prefix(dir).unwrap().to_owned()));
        }
        for old in &self.locations {
            // Leaf work is shared ONLY by exact journal-owned names; it is never swept.
            if old.role == role && role == Role::LeafWork && old.path == path {
                return Ok(());
            }
            if overlap(&old.path, &path) || addresses.iter().any(|(id, p)| old.addresses.iter().any(|(other, q)| id == other && overlap(p, q))) {
                return Err(format!("folders must not overlap: {} and {}", old.path.display(), path.display()));
            }
        }
        self.locations.push(Location { path, role, addresses });
        Ok(())
    }

    pub(super) fn validate_state(&self, state: &State, cfg: &Config) -> Result<(), String> {
        for s in &cfg.repos {
            if s.kind == SourceKind::Plugins {
                state.check_plugin_id(&s.id)?;
                continue;
            }
            // Let pending work settle at its persisted destination before today's settings can
            // reject or redirect the source that originally created the transaction.
            if state.loose_sources().get(&s.id).is_some_and(|r| r.binding().kind() == s.kind && r.leaves().iter().any(|leaf| leaf.interrupted())) {
                continue;
            }
            if let Some(binding) = state.binding_for(s)? {
                let root = self.path(s.kind)?;
                if !same_win_name(&root.to_string_lossy(), &binding.root().to_string_lossy()) {
                    return Err(format!("source '{}' is bound to another destination root", s.id));
                }
            }
        }
        for r in state.loose_sources().values() {
            for loc in &self.locations {
                if loc.role != Role::Destination(r.binding().kind())
                    && (overlap(r.binding().root(), &loc.path) || loc.addresses.iter().any(|(id, _)| Some(*id) == r.directory_id))
                {
                    return Err(format!("{} overlaps a recorded file destination", loc.path.display()));
                }
            }
        }
        Ok(())
    }

    pub(super) fn path(&self, kind: SourceKind) -> Result<&Path, String> {
        self.locations.iter().find(|p| p.role == Role::Destination(kind)).map(|l| l.path.as_path()).ok_or_else(|| format!("no destination for {kind:?}"))
    }
}

pub(super) fn check(paths: &Paths, cfg: &Config, draft: Option<SourceKind>) -> Result<Roots, String> {
    let mut roots = Roots { locations: vec![], pins: HashMap::new() };
    roots.add(paths.home.clone(), Role::Home)?;
    roots.add(paths.root(SourceKind::Skills)?, Role::Destination(SourceKind::Skills))?;
    roots.add(paths.work_dir(), Role::SkillWork)?;
    for kind in [SourceKind::Agents, SourceKind::Instructions] {
        let required = paths.has_file_root(kind) || draft == Some(kind) || cfg.repos.iter().any(|r| r.kind == kind);
        let dir = match paths.root(kind) {
            Ok(dir) => dir,
            Err(_) if !required => continue, // isolated skills-only copy: never infer a real profile root
            Err(e) => return Err(e),
        };
        let work = dir.parent().ok_or("the file destination needs a parent for staging")?.join(crate::apply::leaf::WORK);
        roots.add(dir, Role::Destination(kind))?;
        roots.add(work, Role::LeafWork)?;
    }
    Ok(roots)
}
