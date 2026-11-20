//! A worker's look at a draft. No session, ETag, recovery, schedule, or cached result.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use super::{Engine, EngineError, SIGN_IN, failed};
use crate::config::{Config, RepoConfig};
use crate::github::{self, FailureKind, RemoteError};
use crate::pathsafe;
use crate::source::{DestinationBinding, Entry, PreviewDestination, SourceKind, SourcePreview, resolve_destination};
use crate::state::State;
use crate::tree::{self, SourceDiscovery};
use crate::win::fs::{Kind, LeafDirectory, check_leaf_path, kind_at, open_leaf};

fn stopped(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) { Err("preview was stopped".into()) } else { Ok(()) }
}

fn path_len(path: &Path) -> usize {
    path.as_os_str().encode_wide().count()
}

enum At {
    Missing,
    Directory,
    File(File),
    Link,
}

/// Pin directory chains through the verifier's primitives; only the chosen root and repo child
/// are candidate directories. Reuse the pins across names, never enumerate or read leaf content.
#[derive(Default)]
struct PlainPaths {
    dirs: HashMap<PathBuf, LeafDirectory>,
}

impl PlainPaths {
    fn pin(&mut self, dir: &Path) -> Result<bool, String> {
        if self.dirs.contains_key(dir) {
            return Ok(true);
        }
        match LeafDirectory::pin(dir, false) {
            Ok(pin) => {
                self.dirs.insert(dir.into(), pin);
                Ok(true)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound && dir.parent().is_some() => {
                // A missing folder on an available volume, not an unavailable drive or share.
                self.pin(dir.ancestors().last().expect("a path has itself as an ancestor"))?;
                Ok(false)
            }
            Err(e) => Err(format!("cannot inspect {}: {e}", dir.display())),
        }
    }

    fn at(&mut self, path: &Path) -> Result<At, String> {
        let why = |e| format!("cannot inspect {}: {e}", path.display());
        check_leaf_path(path).map_err(why)?;
        if !self.pin(path.parent().unwrap_or(path))? {
            return Ok(At::Missing);
        }
        match kind_at(path).map_err(why)? {
            None => Ok(At::Missing),
            Some(Kind::Link) => Ok(At::Link),
            Some(Kind::Dir) if self.pin(path)? => Ok(At::Directory),
            Some(Kind::Dir) => Err(format!("{} changed during the preview", path.display())),
            Some(Kind::File) => open_leaf(path, false).map(At::File).map_err(why),
        }
    }

    fn placement_entry(&mut self, path: &Path) -> Result<Entry, String> {
        Ok(match self.at(path)? {
            At::Missing => Entry::Missing,
            At::Directory => Entry::Directory,
            // Only queried for this source's exact listed names in the two allowed layouts. A regular
            // file is evidence *for placement*, even if edited or unowned. Never use it to adopt.
            At::File(_) => Entry::CompatibleFile,
            At::Link => Entry::Link,
        })
    }
}

/// Read the typed state without quarantining it. Hold the leaf and its ancestors through the read;
/// a missing file needs no second path lookup, and damaged state is not evidence that a draft is unbound.
fn preview_state(path: &Path) -> Result<State, String> {
    let mut paths = PlainPaths::default();
    match paths.at(path)? {
        At::Missing => Ok(State::default()),
        At::File(_pin) => {
            let loaded = State::load_sources_with(path, false).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            if let Some(warning) = loaded.warning {
                return Err(warning);
            }
            Ok(loaded.state)
        }
        _ => Err(format!("{} is not a plain state file", path.display())),
    }
}

fn check_claims(state: &State, source: &RepoConfig, dir: &Path, names: &[String]) -> Result<(), String> {
    let key = |p: &Path| PathBuf::from(pathsafe::fold(&p.to_string_lossy()));
    let dir_key = key(dir);
    let listed: HashSet<_> = names.iter().map(|n| pathsafe::fold(n)).collect();
    for (id, other) in state.loose_sources() {
        if *id != source.id && key(&other.binding().directory()) == dir_key {
            if let Some(leaf) = other.leaves().iter().find(|l| listed.contains(&pathsafe::fold(&l.name))) {
                return Err(format!("{} is already claimed by source '{id}'", dir.join(&leaf.name).display()));
            }
        }
    }
    Ok(())
}

/// Resolve placement only. The caller still needs the verifier before managing any leaf.
pub(super) fn locate_files(
    source: &RepoConfig,
    root: &Path,
    names: &[String],
    bound: Option<&DestinationBinding>,
    cancel: &AtomicBool,
) -> Result<DestinationBinding, String> {
    let mut paths = PlainPaths::default();
    let binding = resolve_destination(source, root, names, bound, |path| {
        stopped(cancel)?;
        paths.placement_entry(path)
    })?;
    let len = path_len(&binding.directory());
    for name in names {
        tree::check_leaf_length(len, name)?;
    }
    Ok(binding)
}

impl Engine {
    /// Read-only, including on failure or cancellation. The worker supplies the draft itself (which
    /// need not be saved) and the engine's already resolved local/portable skills root. Do not cache
    /// or use the returned display data as authority to install, adopt, or remove anything.
    pub fn preview_source(&mut self, cfg: &Config, source: &RepoConfig, cancel: &AtomicBool) -> Result<SourcePreview, EngineError> {
        stopped(cancel).map_err(failed)?;
        let problems = source.problems();
        if !problems.is_empty() {
            return Err(failed(problems.join("; ")));
        }
        if !source.id.trim().is_empty() && cfg.repos.iter().filter(|r| r.id == source.id).count() > 1 {
            return Err(failed(format!("two sources share the id '{}'", source.id)));
        }
        let root = self.paths.root(source.kind);
        let len = root.as_deref().map_or(0, |p| if source.kind.is_skills() { path_len(p).max(path_len(&self.paths.work_dir()) + 30) } else { path_len(p) });
        let found = self.preview_listing(source, len, cancel).map_err(|e| failed(e.message))?;
        let names = found.names();
        let errors: Vec<_> = found.rejected().iter().map(|r| format!("{}: {}", pathsafe::printable(&r.name), pathsafe::printable(&r.reason))).collect();
        let destination = if errors.is_empty() {
            let resolved = root.and_then(|root| {
                let _roots = super::roots::check(&self.paths, cfg, Some(source.kind))?;
                let state = preview_state(&self.paths.state_file())?;
                let bound = if source.id.trim().is_empty() { None } else { state.binding_for(source)? };
                if bound.is_some() && cfg.find(&source.id).is_some_and(|saved| !found.same_scope(saved)) {
                    return Err("the draft differs from the source with this ID; cannot reuse its recorded destination".into());
                }
                if source.kind.is_skills() {
                    return match PlainPaths::default().at(&root)? {
                        At::Directory | At::Missing => Ok(root),
                        _ => Err(format!("{} is not a plain skills folder", root.display())),
                    };
                }
                if names.is_empty() && bound.is_none() {
                    let suffix = if source.kind == SourceKind::Agents { ".agent.md" } else { ".instructions.md" };
                    return Err(format!("no *{suffix} files directly in the selected folder; no destination selected"));
                }
                let dir = locate_files(source, &root, &names, bound, cancel)?.directory();
                // A claim (including by an orphan) may block placement, never select a layout.
                check_claims(&state, source, &dir, &names)?;
                Ok(dir)
            });
            match resolved {
                Ok(dir) => PreviewDestination::Resolved(dir),
                Err(why) => PreviewDestination::Held(pathsafe::printable(&why).into_owned()),
            }
        } else {
            // An omitted rejected name could have been the only match in the other layout.
            PreviewDestination::NotChecked
        };
        stopped(cancel).map_err(failed)?;
        Ok(SourcePreview { kind: source.kind, names, destination, errors })
    }

    fn preview_listing<'a>(&self, source: &'a RepoConfig, len: usize, cancel: &AtomicBool) -> Result<SourceDiscovery<'a>, RemoteError> {
        let list = |token| github::discover_source(self.remote.as_ref(), source, len, token, cancel);
        let token = || self.tokens.token().map_err(|why| RemoteError::new(FailureKind::Auth, why));
        if source.use_token {
            return list(Some(&token()?));
        }
        match list(None) {
            Err(e) if matches!(e.kind, FailureKind::NotFound | FailureKind::RateLimited) && !cancel.load(Ordering::Relaxed) => match token() {
                Ok(token) => list(Some(&token)),
                Err(_) if e.kind == FailureKind::NotFound => Err(RemoteError::new(e.kind, format!("{}. {SIGN_IN}", e.message))),
                Err(_) => Err(e),
            },
            other => other,
        }
    }
}
