//! A worker's look at a draft. No session, ETag, recovery, schedule, or cached result.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use super::{Engine, EngineError, SIGN_IN, failed};
use crate::config::{Config, RepoConfig};
use crate::github::{self, FailureKind, RemoteError, RepoRef, TreeReply};
use crate::inventory::{self, InstallRoots, Inventory, Status};
use crate::location::{self, Location, Target};
use crate::pathsafe;
use crate::secret::Secret;
use crate::source::{DestinationBinding, Entry, PreviewDestination, SourceKind, SourcePreview, resolve_destination};
use crate::sources::{self, SourceConfig, Types};
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

/// The lanes (source ID, kind) that are on in `after` but were off, or absent, in `before`, for sources in both.
fn turned_on(before: &Config, after: &Config) -> Vec<(String, SourceKind)> {
    let mut on = Vec::new();
    for source in after.sources() {
        let Some(old) = before.sources().iter().find(|s| s.id == source.id) else { continue };
        let (was, now) = (old.effective_types(before.global()), source.effective_types(after.global()));
        on.extend(crate::config::KINDS.iter().filter(|&&k| now.enabled(k) && !was.enabled(k)).map(|&k| (source.id.clone(), k)));
    }
    on
}

/// Whether this lane's ID carries state records of another kind, or of another repository, branch or folder.
/// Its own records (same kind and place, kept while the type was off) are not foreign.
fn foreign_records(state: &State, lane: &RepoConfig) -> bool {
    match lane.kind {
        SourceKind::Plugins => state.check_plugin_id(&lane.id).is_err() || state.plugin_sources().get(&lane.id).is_some_and(|r| !r.origin.matches_lane(lane)),
        SourceKind::Skills => {
            state.loose_sources().contains_key(&lane.id)
                || state.plugin_sources().contains_key(&lane.id)
                || state.repo(&lane.id).is_some_and(|r| !r.source.is_empty() && r.source != super::source_key(lane))
        }
        SourceKind::Agents | SourceKind::Instructions => {
            state.binding_for(lane).is_err()
                || state.loose_sources().get(&lane.id).is_some_and(|r| {
                    r.recorded_source(&lane.id).is_none_or(|o| o.kind != lane.kind || o.repo != lane.repo || o.branch != lane.branch || o.path != lane.path)
                })
        }
    }
}

/// Pure: rename each lane `after` turns on whose ID is foreign in `state`, writing the new ID into the source's lanes.
pub(crate) fn fresh_lane_ids(state: &State, before: &Config, after: &mut Config) {
    let mut renames = Vec::new();
    let mut reserved: HashSet<String> = after.taken_ids().iter().chain(&before.taken_ids()).chain(state.source_ids()).map(|id| pathsafe::fold(id)).collect();
    for (source, kind) in turned_on(before, after) {
        let Some(lane) = after.repos().iter().find(|r| r.source == source && r.kind == kind) else { continue };
        if !foreign_records(state, lane) {
            continue;
        }
        let suffix = match kind {
            SourceKind::Skills => "skills",
            SourceKind::Agents => "agents",
            SourceKind::Instructions => "instructions",
            SourceKind::Plugins => "plugins",
        };
        let base = format!("{source}-{suffix}");
        let id = (1..).map(|n| if n == 1 { base.clone() } else { format!("{base}-{n}") }).find(|id| reserved.insert(pathsafe::fold(id))).unwrap();
        renames.push((source, kind, id));
    }
    if renames.is_empty() {
        return;
    }
    after.edit(|list, _| {
        for (source, kind, id) in renames {
            if let Some(s) = list.iter_mut().find(|s| s.id == source) {
                s.lanes = std::mem::take(&mut s.lanes).with(kind, id);
            }
        }
    });
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

/// What a GitHub location holds and what adding it as a source would do. Reading it changed nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocationPreview {
    /// The source that adding it saves: fresh IDs for it and its lanes, the global defaults, no overrides. Save it
    /// with `Config::add_source_config`; plugins still need the user's approval before anything is installed.
    pub source: SourceConfig,
    /// The branch it reads. When the source follows the default branch, this is that branch's current name.
    pub branch: String,
    /// True for a bare repository: the saved source keeps following whatever the default branch becomes.
    pub follows_default: bool,
    /// Which types would sync (the global switches).
    pub types: Types,
    /// Every item found below the folder, with its install name and status (installable, or why not), collisions,
    /// files that are never synced, and each plugin's publisher, version and capabilities.
    pub inventory: Inventory,
    /// Where each file type with installable items would be placed, or why it cannot be. Types may be off in preview
    /// so their local setting can be changed without requesting another GitHub listing.
    pub destinations: Vec<(SourceKind, PreviewDestination)>,
}

impl LocationPreview {
    /// Whether `item` of `kind` would be synced: its type is on and nothing stops it.
    pub fn syncs(&self, kind: SourceKind, status: &Status) -> bool {
        self.types.enabled(kind) && *status == Status::Installable
    }
}

impl Engine {
    /// Reads `input` (`owner/repo`, its GitHub address, or a branch or folder address) and lists what is there,
    /// without writing anything and without launching Copilot CLI.
    pub fn preview_location(&mut self, cfg: &Config, input: &str, use_token: bool, cancel: &AtomicBool) -> Result<LocationPreview, EngineError> {
        self.preview_location_with_settings(cfg, input, use_token, &SourceConfig::default(), cancel)
    }

    /// Like `preview_location`, with source choices applied to the unsaved draft. These choices do not authorize a
    /// save or plugin approval; `save_source` re-runs discovery with the same settings before writing.
    pub fn preview_location_with_settings(
        &mut self,
        cfg: &Config,
        input: &str,
        use_token: bool,
        settings: &SourceConfig,
        cancel: &AtomicBool,
    ) -> Result<LocationPreview, EngineError> {
        stopped(cancel).map_err(failed)?;
        let location = Location::parse(input).map_err(failed)?;
        let repo = format!("{}/{}", location.owner, location.name);
        let mut lane = RepoConfig { id: "preview".into(), repo: repo.clone(), use_token, ..Default::default() };
        let mut ask = Ask { use_token, token: None, decided: false };
        let base = RepoRef::of(&lane).map_err(failed)?;
        if let Target::Tree(segments) = &location.target {
            let heads = ask.call(self, cancel, |t| self.remote.matching_heads(&base, &segments[0], t, cancel))?;
            let (branch, folder) = location::resolve_tree(segments, &heads).map_err(failed)?;
            (lane.branch, lane.path) = (branch, folder);
        }
        let base_ref = RepoRef::of(&lane).map_err(failed)?;
        // A bare repository follows its default branch: its name is resolved first and that branch is listed by name,
        // so the listing and the branch shown always agree.
        let follows_default = lane.branch.is_empty();
        let branch = if follows_default { ask.call(self, cancel, |t| self.remote.default_branch(&base_ref, t, cancel))? } else { lane.branch.clone() };
        let problems = if follows_default { RepoConfig { branch: branch.clone(), ..lane.clone() }.problems() } else { Vec::new() };
        if !problems.is_empty() {
            return Err(failed(problems.join("; ")));
        }
        let root_ref = RepoRef { path: "", branch: &branch, ..base_ref };
        let snapshot = match ask.call(self, cancel, |t| self.remote.tree(&root_ref, None, t, cancel))? {
            TreeReply::Snapshot { snapshot, .. } => snapshot,
            TreeReply::NotModified => return Err(failed("GitHub answered 'not modified' to a request that had no ETag")),
        };
        super::root::check_root(&snapshot, &repo, &lane.path).map_err(|e| failed(e.message))?;

        let draft = SourceConfig {
            repo: repo.clone(),
            branch: lane.branch.clone(),
            path: lane.path.clone(),
            interval: settings.interval,
            use_token,
            enabled: settings.enabled,
            types: settings.types,
            include_repo_items: settings.include_repo_items,
            ignore_add: settings.ignore_add.clone(),
            ignore_allow: settings.ignore_allow.clone(),
            ..Default::default()
        };
        let global = cfg.global();
        let types = draft.effective_types(global);
        let policy = draft.effective_policy(global).map_err(failed)?;
        let wanted = inventory::manifest_files(&snapshot, &lane.path).map_err(failed)?;
        let mut manifests = HashMap::new();
        let mut marketplace = None;
        if wanted.marketplace.is_some() || !wanted.plugins.is_empty() {
            let max = super::DEFAULT_MAX_DOWNLOAD;
            super::root::check_budget(&wanted, 0, max).map_err(|e| failed(e.message))?;
            let commit = ask.call(self, cancel, |t| self.remote.commit(&root_ref, t, cancel))?;
            (manifests, marketplace) =
                super::root::read_manifests(self.remote.as_ref(), &root_ref, &commit, &wanted, ask.token.as_ref(), (&mut 0, max), cancel)
                    .map_err(|e| failed(e.message))?;
        }
        let len = |kind: SourceKind| self.paths.root(kind).map_or(0, |p| path_len(&p));
        let roots = InstallRoots {
            skills: len(SourceKind::Skills).max(path_len(&self.paths.work_dir()) + 30),
            agents: len(SourceKind::Agents),
            instructions: len(SourceKind::Instructions),
        };
        let inventory = inventory::inventory(&snapshot, &location.name, &lane.path, &manifests, marketplace.as_deref(), &policy, roots).map_err(failed)?;
        stopped(cancel).map_err(failed)?;

        let state = preview_state(&self.paths.state_file()).map_err(failed)?;
        let mut reserved = cfg.taken_ids();
        reserved.extend(state.source_ids().cloned());
        let (id, lanes) = sources::allocate_source_ids(&crate::config::id_base(&repo), &mut reserved);
        let source = SourceConfig { id, lanes, ..draft };
        let mut destinations = Vec::new();
        for (kind, items) in [(SourceKind::Agents, &inventory.agents), (SourceKind::Instructions, &inventory.instructions)] {
            let names: Vec<String> = items.iter().filter(|i| i.status == Status::Installable).map(|i| i.install_name.clone()).collect();
            if names.is_empty() {
                continue;
            }
            let view = RepoConfig { id: source.lanes.id(kind).unwrap_or_default().to_owned(), kind, ..lane.clone() };
            let resolved = self.paths.root(kind).and_then(|root| {
                super::roots::check(&self.paths, cfg, Some(kind))?;
                let dir = locate_files(&view, &root, &names, None, cancel)?.directory();
                // Another source's claim may block placement, never select a layout.
                check_claims(&state, &view, &dir, &names)?;
                Ok(dir)
            });
            destinations.push((
                kind,
                match resolved {
                    Ok(dir) => PreviewDestination::Resolved(dir),
                    Err(why) => PreviewDestination::Held(pathsafe::printable(&why).into_owned()),
                },
            ));
        }
        stopped(cancel).map_err(failed)?;
        Ok(LocationPreview { source, branch, follows_default, types, inventory, destinations })
    }

    /// Give every lane a settings change turns on a fresh ID when its own carries records of another kind or place.
    /// Reads `state.json` only when some type is turned on.
    pub(crate) fn fresh_lane_ids(&self, before: &Config, after: &mut Config) -> Result<(), String> {
        if turned_on(before, after).is_empty() {
            return Ok(());
        }
        let state = preview_state(&self.paths.state_file())?;
        fresh_lane_ids(&state, before, after);
        Ok(())
    }

    pub(crate) fn check_fresh_id(&self, id: &str) -> Result<(), EngineError> {
        let state = preview_state(&self.paths.state_file()).map_err(failed)?;
        if state.has_source_id(id) {
            return Err(failed(format!("Source ID '{id}' is already used by another source on this PC. Preview it again.")));
        }
        Ok(())
    }

    /// Read-only, including on failure or cancellation. The worker supplies the draft itself (which
    /// need not be saved) and the engine's already resolved local/portable skills root. Do not cache
    /// or use the returned display data as authority to install, adopt, or remove anything.
    pub fn preview_source(&mut self, cfg: &Config, source: &RepoConfig, cancel: &AtomicBool) -> Result<SourcePreview, EngineError> {
        stopped(cancel).map_err(failed)?;
        let problems = source.problems();
        if !problems.is_empty() {
            return Err(failed(problems.join("; ")));
        }
        if !source.id.trim().is_empty() && cfg.repos().iter().filter(|r| r.id == source.id).count() > 1 {
            return Err(failed(format!("two sources share the id '{}'", source.id)));
        }
        if source.kind == SourceKind::Plugins {
            preview_state(&self.paths.state_file()).and_then(|state| state.check_plugin_id(&source.id)).map_err(failed)?;
            return self.preview_plugins(source, cancel);
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
        Ok(SourcePreview { kind: source.kind, names, destination, errors, plugins: None })
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

/// The token for a location preview's requests. The first request decides whether the repository needs it; the
/// rest reuse that answer.
struct Ask {
    use_token: bool,
    token: Option<Secret>,
    decided: bool,
}

impl Ask {
    fn call<T>(&mut self, engine: &Engine, cancel: &AtomicBool, call: impl Fn(Option<&Secret>) -> Result<T, RemoteError>) -> Result<T, EngineError> {
        let message = |e: RemoteError| failed(e.message);
        if std::mem::replace(&mut self.decided, true) {
            return call(self.token.as_ref()).map_err(message);
        }
        if self.use_token {
            self.token = Some(engine.tokens.token().map_err(failed)?);
            return call(self.token.as_ref()).map_err(message);
        }
        match call(None) {
            Err(e) if matches!(e.kind, FailureKind::NotFound | FailureKind::RateLimited) && !cancel.load(Ordering::Relaxed) => match engine.tokens.token() {
                Ok(token) => call(Some(self.token.insert(token))).map_err(message),
                Err(_) if e.kind == FailureKind::NotFound => Err(failed(format!("{}. {SIGN_IN}", e.message))),
                Err(_) => Err(message(e)),
            },
            other => other.map_err(message),
        }
    }
}
