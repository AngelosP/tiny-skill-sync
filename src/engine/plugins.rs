//! A plugin check prepares the whole branch-root catalog before consulting Copilot. The planner
//! supplies all authority for native actions, which run and verify under the engine's lock.

use super::root::RootListing;
use super::*;
use crate::apply::plugins::{ActionError, execute, snapshot};
use crate::inventory::Status as ItemStatus;
use crate::plugins::catalog::{self, Catalog, CatalogTree, MAX_MANIFEST_BYTES};
use crate::plugins::native::{ManagerError, NativeSnapshot, PluginManager};
use crate::plugins::{self, Choice, NameMap, Operation, Plan, Status};
use crate::source::{PreviewDestination, SourcePreview};
use crate::state::PluginSourceState;

fn local(message: impl Into<String>) -> RemoteError {
    RemoteError::new(FailureKind::Local, message)
}

fn native_error(why: ManagerError) -> RemoteError {
    let kind = if why == ManagerError::Cancelled { FailureKind::Cancelled } else { FailureKind::Local };
    RemoteError::new(kind, why.to_string())
}

fn running(cancel: &AtomicBool) -> Result<(), RemoteError> {
    if cancel.load(Ordering::Relaxed) { Err(native_error(ManagerError::Cancelled)) } else { Ok(()) }
}

/// The plugins lane with this lane ID, or the plugins lane of the source with this source ID.
fn plugin_source<'a>(cfg: &'a Config, id: &str) -> Result<&'a RepoConfig, EngineError> {
    cfg.find(id)
        .filter(|r| r.kind == SourceKind::Plugins)
        .or_else(|| cfg.find_source(id).filter(|s| s.id == id).and_then(|s| s.lanes.id(SourceKind::Plugins)).and_then(|lane| cfg.find(lane)))
        .filter(|r| r.kind == SourceKind::Plugins)
        .ok_or_else(|| failed(format!("No plugin source '{id}' is set up.")))
}

impl Session {
    fn apply_plugin(
        &mut self,
        manager: &dyn PluginManager,
        repo: &RepoConfig,
        action: &plugins::Action,
        latest: &mut NativeSnapshot,
        cancel: &AtomicBool,
    ) -> Result<(), ActionError> {
        execute(manager, repo, &mut self.state, action, latest, cancel)?;
        // Make the verified change durable before another command, with the old ETag already cleared.
        self.must_save = true;
        self.save().map_err(|why| ActionError { why: ManagerError::Failed(why), stop: true })
    }
}

impl Engine {
    /// Inject a manager without doing I/O. Only production previews probe the Windows executable path.
    pub fn with_plugin_manager(mut self, manager: Box<dyn PluginManager>) -> Self {
        self.plugin_manager = manager;
        self.native_plugin_manager = false;
        self
    }

    fn read_catalog(
        &self,
        repo: &RepoConfig,
        root: &RepoRef,
        tree: &tree::Snapshot,
        token: Option<&Secret>,
        budget: (&mut u64, u64),
        cancel: &AtomicBool,
    ) -> Result<Catalog, RemoteError> {
        if tree.truncated {
            return Err(RemoteError::new(FailureKind::TooLarge, "GitHub cut the branch listing short; no plugins were changed."));
        }
        let tree = CatalogTree::new(tree).map_err(|why| RemoteError::new(FailureKind::Protocol, why))?;
        let (spent, max) = budget;
        let mut pin = None;
        let mut get = |file: &SkillFile| {
            running(cancel)?;
            if file.size > MAX_MANIFEST_BYTES || spent.saturating_add(file.size) > max {
                return Err(RemoteError::new(FailureKind::TooLarge, "The plugin manifests reached this check's download limit."));
            }
            if pin.is_none() {
                pin = Some(self.remote.commit(root, token, cancel)?);
            }
            download(self.remote.as_ref(), root, pin.as_deref().unwrap_or_default(), &file.path, file, token, cancel)
                .inspect(|body| *spent += body.len() as u64)
        };
        let market = get(&tree.marketplace_manifest().map_err(local)?)?;
        let pending = tree.match_marketplace(repo, &market).map_err(local)?;
        let mut manifests = NameMap::new();
        for file in pending.manifests() {
            manifests.insert(file.path.clone(), get(file)?);
        }
        running(cancel)?;
        Ok(pending.finish(&manifests))
    }

    /// A preview has no writable session or native command.
    pub(super) fn preview_plugins(&self, source: &RepoConfig, cancel: &AtomicBool) -> Result<SourcePreview, EngineError> {
        if self.native_plugin_manager && !crate::win::copilot::available() {
            return Err(failed("GitHub Copilot CLI was not found. Install it to add plugin sources."));
        }
        let root = RepoRef { path: "", ..RepoRef::of(source).map_err(failed)? };
        let (reply, token, _) = self.request_tree(None, source, &root, None, cancel).map_err(|e| failed(e.message))?;
        let TreeReply::Snapshot { snapshot, .. } = reply else { return Err(failed("GitHub returned no branch listing.")) };
        let catalog = self.read_catalog(source, &root, &snapshot, token.as_deref(), (&mut 0, DEFAULT_MAX_DOWNLOAD), cancel).map_err(|e| failed(e.message))?;
        let names = catalog.plugins().keys().cloned().collect();
        let errors = catalog.rejected().iter().map(|r| format!("{}: {}", pathsafe::printable(&r.name), pathsafe::printable(&r.reason))).collect();
        Ok(SourcePreview { kind: SourceKind::Plugins, names, errors, destination: PreviewDestination::NotChecked, plugins: Some(catalog) })
    }

    /// After saving the source `preview` describes as `source_id` in `cfg`, record approval for exactly the installable
    /// plugins it reviewed, keyed by (Copilot home, source ID, repo, resolved branch, folder, plugin root, name,
    /// publisher). Changes nothing when the preview has no installable plugins. Never launches Copilot CLI.
    /// Approval never installs anything by itself; the next check does, through the marketplace on that branch.
    pub fn approve_location_plugins(&mut self, cfg: &Config, source_id: &str, preview: &LocationPreview) -> Result<(), EngineError> {
        let reviewed: Vec<_> = preview.inventory.plugins.iter().filter(|i| i.status == ItemStatus::Installable && i.data.listed_by_marketplace).collect();
        if reviewed.is_empty() {
            return Ok(());
        }
        let changed = || failed("This source changed after it was reviewed. Review it again to approve its plugins.");
        let saved = cfg.find_source(source_id).filter(|s| s.id == source_id).ok_or_else(|| failed(format!("No source '{source_id}' is set up.")))?;
        let p = &preview.source;
        if saved.repo != p.repo
            || saved.branch != p.branch
            || saved.path != p.path
            || saved.lanes != p.lanes
            || (!saved.branch.is_empty() && saved.branch != preview.branch)
            || saved.branch.is_empty() != preview.follows_default
        {
            return Err(changed());
        }
        let lane = saved
            .lanes
            .id(SourceKind::Plugins)
            .and_then(|id| cfg.find(id))
            .filter(|r| r.kind == SourceKind::Plugins && r.source == saved.id)
            .ok_or_else(|| failed("Plugins are off for this source, so there is nothing to approve."))?;
        let market = preview.inventory.marketplace.as_deref().filter(|m| crate::plugins::normal_name(m)).ok_or_else(changed)?;
        let view = RepoConfig { branch: preview.branch.clone(), ..lane.clone() };
        if !view.problems().is_empty() {
            return Err(changed());
        }
        let home = self.plugin_manager.home().map_err(|e| failed(e.to_string()))?;
        crate::source::check_root(&home).map_err(|_| failed("Copilot home must be a full path."))?;
        let mut record = PluginSourceState::new(&view, &home, market);
        for item in reviewed {
            record.approved.insert(item.install_name.clone(), item.data.publisher.clone());
            record.roots.insert(item.install_name.clone(), item.path.clone());
        }
        let mut s = self.begin_settled(cfg)?.ok_or_else(folder_changed)?;
        let result = (|| {
            s.authorize().map_err(|e| failed(e.message))?;
            if let Some(old) = s.state.plugin_sources().get(&lane.id) {
                let same_place =
                    old.origin.repo == record.origin.repo && old.origin.path == record.origin.path && old.market == record.market && old.copilot_home == home;
                if same_place {
                    // What is installed stays recorded; a new branch is checked against Copilot's marketplace next time.
                    record.plugins.clone_from(&old.plugins);
                    record.registered_by_app = old.registered_by_app;
                    record.poll.clone_from(&old.poll);
                    record.poll.etag = None;
                    record.poll.needs_attention = false;
                } else if !old.plugins.is_empty() {
                    return Err(failed(plugins::Blocker::SourceChanged.to_string()));
                }
            }
            s.state.set_plugin_source(&lane.id, record).map_err(failed)?;
            s.must_save = true;
            Ok(())
        })();
        if result.is_ok() {
            self.plugin_attention_rows.remove(&lane.id);
        }
        self.conclude(s, result)
    }

    /// After saving `cfg`, record just the reviewed names and publishers. This resolves the home without launching Copilot.
    pub fn approve_plugin_preview(&mut self, cfg: &Config, id: &str, preview: &SourcePreview) -> Result<(), EngineError> {
        let source = plugin_source(cfg, id)?;
        let catalog = preview
            .plugins
            .as_ref()
            .filter(|c| preview.kind == SourceKind::Plugins && preview.errors.is_empty() && c.rejected().is_empty() && !c.plugins().is_empty())
            .ok_or_else(|| failed("Approve plugins from a complete preview with matches and no rejected entries."))?;
        let home = self.plugin_manager.home().map_err(|e| failed(e.to_string()))?;
        let mut record = catalog.approved_record(source, &home).map_err(failed)?;
        let mut s = self.begin_settled(cfg)?.ok_or_else(folder_changed)?;
        let result = (|| {
            s.authorize().map_err(|e| failed(e.message))?;
            if let Some(old) = s.state.plugin_sources().get(id) {
                if old.origin == record.origin && old.market == record.market && old.copilot_home == home {
                    record.plugins.clone_from(&old.plugins);
                    record.registered_by_app = old.registered_by_app;
                    record.poll.clone_from(&old.poll);
                    record.poll.etag = None;
                } else if !old.plugins.is_empty() {
                    return Err(failed(plugins::Blocker::SourceChanged.to_string()));
                }
            }
            s.state.set_plugin_source(id, record).map_err(failed)?;
            s.must_save = true;
            Ok(())
        })();
        self.conclude(s, result)
    }

    /// Apply just this choice, after re-reading GitHub and Copilot. Approval must name the reviewed publisher.
    /// `Outcome::Failed` means the choice did not complete (including a choice that no longer applies). `id` is the
    /// plugins lane's ID or its source's ID.
    pub fn resolve_plugin(&mut self, cfg: &Config, id: &str, choice: &Choice, cancel: &AtomicBool) -> Result<RepoReport, EngineError> {
        let lane = plugin_source(cfg, id)?;
        let mut s = self.begin_settled(cfg)?.ok_or_else(folder_changed)?;
        let request = CheckRequest { explicit: true, choice: Some(choice) };
        let mut report = match self.fresh_root(&mut s, lane, cancel) {
            Ok(root) => self.sync_plugin_lane(&mut s, lane, &SyncOptions::default(), request, &root, &mut None, cancel),
            Err(e) => RepoReport::new(lane).fail(e),
        };
        report.finished_at = self.now();
        report.downloaded = s.downloaded;
        self.conclude(s, Ok(report))
    }

    /// Whether the plugins lane's last full check left something for the user.
    pub(super) fn plugin_lane_needs_attention(&self, s: &Session, lane: &RepoConfig) -> bool {
        match s.state.plugin_sources().get(&lane.id) {
            Some(r) => r.poll.needs_attention,
            None => s.state.lane_poll(lane).is_some_and(|p| p.attention),
        }
    }

    /// The plugins lane of a source, on the source's own complete listing. `resolved` receives the branch the lane
    /// read when it had to name it (a source that follows the default branch).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn sync_plugin_lane(
        &mut self,
        s: &mut Session,
        lane: &RepoConfig,
        opts: &SyncOptions,
        request: CheckRequest<'_>,
        root: &RootListing,
        resolved: &mut Option<String>,
        cancel: &AtomicBool,
    ) -> RepoReport {
        let mut report = RepoReport::new(lane);
        match self.plugin_lane_in(s, lane, opts, request, root, resolved, &mut report, cancel) {
            Ok(()) => report,
            Err(e) => report.fail(e),
        }
    }

    /// Remembers, in `state.json` as scheduling data only, the last full check of a plugins lane that has no record,
    /// so an unchanged source is answered by one conditional request and never launches Copilot CLI.
    fn remember_unrecorded(&mut self, s: &mut Session, lane: &RepoConfig, root: &RootListing, lists_nothing: bool, attention: bool) {
        s.remember_lane_poll(lane, root.etag.clone(), self.now(), lists_nothing, attention);
        if attention {
            self.plugin_attention_rows.insert(lane.id.clone());
        } else {
            self.plugin_attention_rows.remove(&lane.id);
        }
    }

    /// The marketplace catalog of the listing, kept to the plugins the source's inventory may install. `None` when
    /// the branch root has no marketplace.
    fn lane_catalog(view: &RepoConfig, root: &RootListing) -> Result<Option<Catalog>, RemoteError> {
        let Some(bytes) = &root.marketplace else { return Ok(None) };
        let tree = CatalogTree::new(&root.snapshot).map_err(|why| RemoteError::new(FailureKind::Protocol, why))?;
        let pending = tree.match_marketplace(view, bytes).map_err(local)?;
        let mut manifests = NameMap::new();
        for file in pending.manifests() {
            if let Some(body) = root.manifests.get(&file.path) {
                manifests.insert(file.path.clone(), body.clone());
            }
        }
        let mut catalog = pending.finish(&manifests);
        catalog.restrict(&root.inventory.plugins);
        Ok(Some(catalog))
    }

    #[allow(clippy::too_many_arguments)]
    fn plugin_lane_in(
        &mut self,
        s: &mut Session,
        lane: &RepoConfig,
        opts: &SyncOptions,
        request: CheckRequest<'_>,
        root: &RootListing,
        resolved: &mut Option<String>,
        report: &mut RepoReport,
        cancel: &AtomicBool,
    ) -> Result<(), RemoteError> {
        running(cancel)?;
        let has_record = s.state.plugin_sources().contains_key(&lane.id);
        let items = &root.inventory.plugins;
        // Nothing to do and nothing owned: no Copilot CLI, no other request.
        if items.is_empty() && !has_record && request.choice.is_none() {
            report.lists_nothing = true;
            if !opts.dry_run {
                self.remember_unrecorded(s, lane, root, true, false);
            }
            return Ok(());
        }
        let branch = if !lane.branch.is_empty() {
            lane.branch.clone()
        } else if !root.branch.is_empty() {
            root.branch.clone()
        } else {
            let at = RepoRef::of(lane).map_err(local)?;
            self.remote.default_branch(&at, root.token.as_deref(), cancel)?
        };
        let view = RepoConfig { branch, ..lane.clone() };
        let problems = view.problems();
        if !problems.is_empty() {
            return Err(local(problems.join("; ")));
        }
        *resolved = Some(view.branch.clone());
        let catalog = match Self::lane_catalog(&view, root) {
            Ok(catalog) => catalog,
            Err(_) if !has_record && request.choice.is_none() => None,
            Err(e) => return Err(e),
        };
        let moved = s.state.plugin_sources().get(&lane.id).filter(|r| r.origin.matches_lane(lane) && r.origin.branch != view.branch).cloned();
        if let Some(record) = moved {
            // The default branch is now another branch: what was approved for the old one does not carry over.
            if request.choice.is_some() {
                return Err(local(format!("This source now follows the branch {}; review and approve its plugins again.", pathsafe::printable(&view.branch))));
            }
            let why = format!("The default branch is now {}; approve this source's plugins again to follow it", pathsafe::printable(&view.branch));
            let mut statuses = NameMap::new();
            for name in catalog.iter().flat_map(|c| c.plugins().keys()) {
                statuses.insert(name.clone(), Status::NeedsApproval);
            }
            for name in record.plugins.keys() {
                statuses.insert(name.clone(), Status::Protected(why.clone()));
            }
            let plan = Plan { actions: Vec::new(), statuses, blocker: None };
            report.plugins = rows_without_copilot(plan, catalog.as_ref(), Some(&record), opts.dry_run);
            for row in &mut report.plugins {
                row.choices.clear();
            }
            report.outcome = Outcome::NeedsAttention;
            if !opts.dry_run {
                if let Some(poll) = s.state.plugin_poll_mut(&lane.id) {
                    s.must_save |= poll.etag != root.etag || !poll.needs_attention;
                    (poll.etag, poll.needs_attention) = (root.etag.clone(), true);
                }
                self.plugin_attention_rows.insert(lane.id.clone());
                s.touch_check(&lane.id, self.now());
            }
            return Ok(());
        }
        let Some(catalog) = catalog else {
            if has_record || request.choice.is_some() {
                return Err(local("No plugin marketplace (marketplace.json) was found at the top of this branch; installed plugins were kept."));
            }
            // Without a marketplace nothing here can be installed from this branch.
            report.lists_nothing = true;
            report.plugins = items
                .iter()
                .filter(|i| !matches!(i.status, ItemStatus::Bundled { .. }))
                .map(|i| PluginReport {
                    name: i.install_name.clone(),
                    version: i.data.version.clone(),
                    publisher: i.data.publisher.clone(),
                    path: Some(i.path.clone()),
                    status: Status::NotSynced(i.status.reason().unwrap_or_else(|| catalog::NOT_LISTED.into())),
                    choices: Vec::new(),
                    result: if opts.dry_run { ApplyResult::DryRun } else { ApplyResult::NotApplicable },
                })
                .collect();
            report.plugins.sort_by(|a, b| a.name.cmp(&b.name));
            report.plugins.dedup_by(|a, b| a.name == b.name);
            if !opts.dry_run {
                self.remember_unrecorded(s, lane, root, true, false);
            }
            return Ok(());
        };
        report.lists_nothing = catalog.plugins().is_empty();
        report.rejected = catalog.rejected().to_vec();
        if !has_record && request.choice.is_none() {
            // Nothing is approved yet: the rows need no Copilot CLI and nothing is written.
            let plan = plugins::plan_unapproved(&view, &catalog, &s.state);
            report.plugins = rows_without_copilot(plan, Some(&catalog), None, opts.dry_run);
            let attention = !report.rejected.is_empty() || report.plugins.iter().any(PluginReport::needs_attention);
            if !opts.dry_run {
                self.remember_unrecorded(s, lane, root, report.lists_nothing, attention);
            }
            if attention {
                report.outcome = Outcome::NeedsAttention;
            }
            return Ok(());
        }
        if opts.dry_run {
            let plan = plugins::plan(&view, Some(&catalog), &s.state, None, None);
            report.plugins = plugin_rows(plan, Some(&catalog), s.state.plugin_sources().get(&lane.id), None).0;
            if !report.rejected.is_empty() {
                report.outcome = Outcome::NeedsAttention;
            }
            return Ok(());
        }
        self.check_plugins_in(s, &view, request, &catalog, root, report, cancel)
    }

    /// The v0.8.0 check of one plugins lane against Copilot, on `view` (the lane with its branch named).
    #[allow(clippy::too_many_arguments)]
    fn check_plugins_in(
        &mut self,
        s: &mut Session,
        repo: &RepoConfig,
        request: CheckRequest<'_>,
        catalog: &Catalog,
        root: &RootListing,
        report: &mut RepoReport,
        cancel: &AtomicBool,
    ) -> Result<(), RemoteError> {
        s.authorize()?;
        let mut native = snapshot(self.plugin_manager.as_ref(), cancel).map_err(native_error)?;
        let mut plan = plugins::plan(repo, Some(catalog), &s.state, Some(&native), request.choice);
        let registration = plan.actions.first().filter(|a| a.operation() == Operation::Register);
        let mut failure = None;
        if let Some(action) = registration {
            match s.apply_plugin(self.plugin_manager.as_ref(), repo, action, &mut native, cancel) {
                Ok(()) => plan = plugins::plan(repo, Some(catalog), &s.state, Some(&native), request.choice),
                Err(error) => failure = Some(native_error(error.why)),
            }
        }
        let blocker = plan.blocker.as_ref().map(|why| local(why.to_string()));
        let (rows, actions) = plugin_rows(plan, Some(catalog), s.state.plugin_sources().get(&repo.id), Some(&native));
        report.plugins = rows;
        let blocked = blocker
            .or_else(|| (request.choice.is_some() && actions.is_empty()).then(|| local("That plugin choice no longer applies. Check the source again.")));
        let may_bind = blocked.is_none();
        let mut acted = false;
        if let Some(why) = blocked {
            failure = Some(why);
        } else {
            for action in actions {
                let Some(name) = action.name() else { continue }; // Registration was attempted exactly once above.
                if let Err(why) = running(cancel) {
                    failure.get_or_insert(why);
                    break;
                }
                let index = report.plugins.binary_search_by(|row| row.name.as_str().cmp(name)).expect("the plan has a row for each action");
                let row = &mut report.plugins[index];
                match s.apply_plugin(self.plugin_manager.as_ref(), repo, &action, &mut native, cancel) {
                    Ok(()) => {
                        acted = true;
                        if let Ok(Some(installed)) = native.plugin(name, action.market()) {
                            row.version = Some(installed.version.clone());
                        }
                        row.result = ApplyResult::Done;
                    }
                    Err(error) => {
                        let why = native_error(error.why);
                        row.result = ApplyResult::Failed(why.message.clone());
                        failure.get_or_insert(why);
                        if error.stop {
                            break;
                        }
                    }
                }
            }
        }
        let attention = failure.is_some() || report.plugins.iter().any(PluginReport::needs_attention);
        // Choices run only their intended action. Leave any other work for a full check.
        let complete = failure.is_none() && request.choice.is_none();
        // A blocked check can invalidate poll data without claiming a new marketplace.
        if may_bind && s.state.plugin_poll_mut(&repo.id).is_none() {
            s.state.set_plugin_source(&repo.id, PluginSourceState::new(repo, &native.home, catalog.market())).map_err(local)?;
            s.must_save = true;
        }
        if let Some(poll) = s.state.plugin_poll_mut(&repo.id) {
            let etag = if complete { root.etag.clone() } else { None };
            let private = root.private;
            s.must_save |= poll.etag != etag || poll.lists_nothing != report.lists_nothing || poll.needs_token != private || poll.needs_attention != attention;
            (poll.etag, poll.lists_nothing, poll.needs_token, poll.needs_attention) = (etag, report.lists_nothing, private, attention);
        }
        if complete && attention {
            self.plugin_attention_rows.insert(repo.id.clone());
        } else {
            self.plugin_attention_rows.remove(&repo.id);
        }
        if let Some(why) = failure {
            return Err(why);
        }
        s.touch_check(&repo.id, self.now());
        report.outcome = if attention {
            Outcome::NeedsAttention
        } else if acted {
            Outcome::Synced
        } else {
            Outcome::Unchanged
        };
        Ok(())
    }

    /// Explicitly uninstall recorded plugins before removing the source. Keep the source on partial failure.
    pub fn remove_source_with_plugins(&mut self, cfg: &Config, id: &str, cancel: &AtomicBool) -> Result<RemoveReport, EngineError> {
        self.remove_source_in(cfg, id, false, Some(cancel))
    }

    pub(super) fn remove_plugins_in(
        &self,
        s: &mut Session,
        active: Option<&RepoConfig>,
        id: &str,
        uninstall: Option<&AtomicBool>,
    ) -> Result<RemoveReport, EngineError> {
        s.authorize().map_err(|e| failed(e.message))?;
        let mut report = RemoveReport::default();
        if let Some(cancel) = uninstall {
            if let Some(record) = s.state.plugin_sources().get(id).filter(|r| !r.plugins.is_empty()) {
                let recorded = record.recorded_source(id);
                // A lane that follows the default branch acts on the branch its record was made for.
                let view = active
                    .filter(|a| a.branch.is_empty() && record.origin.matches_lane(a))
                    .map(|a| RepoConfig { branch: record.origin.branch.clone(), ..a.clone() });
                let source = view.as_ref().or(active).unwrap_or(&recorded);
                let mut native = snapshot(self.plugin_manager.as_ref(), cancel).map_err(|e| failed(e.to_string()))?;
                let plan = plugins::plan_source_removal(source, &s.state, Some(&native));
                if let Some(why) = plan.blocker {
                    return Err(failed(why.to_string()));
                }
                let held = plan.statuses.len() != plan.actions.len();
                let mut error = None;
                for action in plan.actions {
                    match s.apply_plugin(self.plugin_manager.as_ref(), source, &action, &mut native, cancel) {
                        Ok(()) => {
                            if action.operation() == Operation::Uninstall {
                                report.deleted.extend(action.name().map(str::to_owned));
                            }
                        }
                        Err(failure) => {
                            error.get_or_insert(failure.why);
                            if failure.stop {
                                break;
                            }
                        }
                    }
                }
                if let Some(why) = error {
                    return Err(failed(format!("The source was kept. {why}")));
                }
                if held {
                    return Err(failed("Some plugins could not be verified for uninstall. The source was kept."));
                }
            }
        } else {
            report.message = Some("Plugins stay installed in Copilot.");
            if let Some(record) = s.state.plugin_sources().get(id) {
                report.kept.extend(record.plugins.keys().map(|n| (n.clone(), "left installed in Copilot".into())));
            }
        }
        s.must_save |= s.state.forget_plugin_source(id);
        Ok(report)
    }
}

#[derive(Clone, Copy)]
pub(super) struct CheckRequest<'a> {
    pub(super) explicit: bool,
    pub(super) choice: Option<&'a Choice>,
}

/// Rows for a check that did not ask Copilot. Nothing was tried; in a real check that is "not applicable".
fn rows_without_copilot(plan: Plan, catalog: Option<&Catalog>, record: Option<&PluginSourceState>, dry_run: bool) -> Vec<PluginReport> {
    let mut rows = plugin_rows(plan, catalog, record, None).0;
    if !dry_run {
        for row in &mut rows {
            row.result = ApplyResult::NotApplicable;
        }
    }
    rows
}

fn plugin_rows(
    plan: Plan,
    catalog: Option<&Catalog>,
    record: Option<&PluginSourceState>,
    native: Option<&NativeSnapshot>,
) -> (Vec<PluginReport>, Vec<plugins::Action>) {
    let mut rows = Vec::with_capacity(plan.statuses.len());
    for (name, status) in plan.statuses.into_entries() {
        let desired = catalog.and_then(|c| c.plugins().get(&name));
        let publisher = match desired {
            Some(p) => p.publisher.clone(),
            None => record.and_then(|r| r.approved.get(&name)).cloned().flatten(),
        };
        let version = native
            .and_then(|s| s.plugin(&name, catalog?.market()).ok().flatten())
            .map(|p| p.version.clone())
            .or_else(|| desired.and_then(|p| p.version.clone()))
            .or_else(|| record?.plugins.get(&name)?.version.clone());
        let choices = match status {
            Status::NeedsApproval => vec![Choice::Approve { name: name.clone(), publisher: publisher.clone() }],
            Status::RemovedInCopilot => vec![Choice::Reinstall(name.clone())],
            Status::RemovedFromGitHub => vec![Choice::Uninstall(name.clone()), Choice::Forget(name.clone())],
            _ => Vec::new(),
        };
        let result = if matches!(status, Status::Planned(_) | Status::WaitingForMarketplace) {
            ApplyResult::NotTried
        } else if native.is_none() {
            ApplyResult::DryRun
        } else {
            ApplyResult::NotApplicable
        };
        rows.push(PluginReport { name, version, publisher, path: desired.map(|p| p.path.clone()), status, choices, result });
    }
    (rows, plan.actions)
}
