//! A plugin check prepares the whole branch-root catalog before consulting Copilot. The planner
//! supplies all authority for native actions, which run and verify under the engine's lock.

use super::*;
use crate::apply::plugins::{ActionError, execute, snapshot};
use crate::plugins::catalog::{Catalog, CatalogTree, MAX_MANIFEST_BYTES};
use crate::plugins::native::{ManagerError, NativeSnapshot, PluginManager};
use crate::plugins::{self, Choice, NameMap, Operation, Plan, Status};
use crate::source::{PreviewDestination, SourcePreview};
use crate::state::PluginSourceState;

struct Listing {
    catalog: Catalog,
    etag: Option<String>,
    private: bool,
}

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

fn plugin_source<'a>(cfg: &'a Config, id: &str) -> Result<&'a RepoConfig, EngineError> {
    cfg.find(id).filter(|r| r.kind == SourceKind::Plugins).ok_or_else(|| failed(format!("No plugin source '{id}' is set up.")))
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

    fn plugin_listing(
        &self,
        s: &mut Session,
        repo: &RepoConfig,
        opts: &SyncOptions,
        request: CheckRequest<'_>,
        cancel: &AtomicBool,
    ) -> Result<Option<Listing>, RemoteError> {
        running(cancel)?;
        let problems = repo.problems();
        if !problems.is_empty() {
            return Err(local(problems.join("; ")));
        }
        let root = RepoRef { path: "", ..RepoRef::of(repo).map_err(local)? };
        let stored = s.state.plugin_sources().get(&repo.id).filter(|r| r.origin.matches(repo));
        // An explicit check refreshes Copilot even when GitHub is unchanged. The first
        // timer check of saved attention needs a listing to rebuild the rows and choices.
        let tag = stored
            .filter(|r| !r.poll.needs_attention || self.plugin_attention_rows.contains(&repo.id))
            .filter(|_| !request.explicit && !opts.ignore_etag && self.schedule.retry_of(&repo.id).is_none())
            .and_then(|r| r.poll.etag.clone());
        let (reply, token, private) = self.request_tree(Some(s), repo, &root, tag.as_deref(), cancel)?;
        running(cancel)?;
        if matches!(reply, TreeReply::NotModified) && tag.is_some() {
            return Ok(None);
        }
        let TreeReply::Snapshot { snapshot, etag } = reply else {
            return Err(RemoteError::new(FailureKind::Protocol, "GitHub answered 'not modified' without an ETag."));
        };
        let catalog = self.read_catalog(repo, &root, &snapshot, token.as_deref(), (&mut s.downloaded, opts.max_download_bytes), cancel)?;
        Ok(Some(Listing { catalog, etag, private }))
    }

    pub(super) fn sync_plugins(&mut self, s: &mut Session, repo: &RepoConfig, opts: &SyncOptions, explicit: bool, cancel: &AtomicBool) -> RepoReport {
        self.check_plugins(s, repo, opts, CheckRequest { explicit, choice: None }, cancel)
    }

    /// Apply just this choice, after re-reading GitHub and Copilot. Approval must name the reviewed publisher.
    /// `Outcome::Failed` means the choice did not complete (including a choice that no longer applies).
    pub fn resolve_plugin(&mut self, cfg: &Config, id: &str, choice: &Choice, cancel: &AtomicBool) -> Result<RepoReport, EngineError> {
        let source = plugin_source(cfg, id)?;
        let mut s = self.begin_settled(cfg)?.ok_or_else(folder_changed)?;
        let mut report = self.check_plugins(&mut s, source, &SyncOptions::default(), CheckRequest { explicit: true, choice: Some(choice) }, cancel);
        report.finished_at = self.now();
        report.downloaded = s.downloaded;
        self.conclude(s, Ok(report))
    }

    fn check_plugins(&mut self, s: &mut Session, repo: &RepoConfig, opts: &SyncOptions, request: CheckRequest<'_>, cancel: &AtomicBool) -> RepoReport {
        let mut report = RepoReport::new(repo);
        let result = self.check_plugins_in(s, repo, opts, request, &mut report, cancel);
        if let Err(e) = result { report.fail(e) } else { report }
    }

    fn check_plugins_in(
        &mut self,
        s: &mut Session,
        repo: &RepoConfig,
        opts: &SyncOptions,
        request: CheckRequest<'_>,
        report: &mut RepoReport,
        cancel: &AtomicBool,
    ) -> Result<(), RemoteError> {
        let Some(Listing { catalog, etag, private }) = self.plugin_listing(s, repo, opts, request, cancel)? else {
            let poll = &s.state.plugin_sources()[&repo.id].poll;
            report.lists_nothing = poll.lists_nothing;
            if poll.needs_attention {
                report.outcome = Outcome::NeedsAttention;
            }
            if !opts.dry_run {
                s.touch_check(&repo.id, self.now());
            }
            return Ok(());
        };
        report.lists_nothing = catalog.plugins().is_empty();
        report.rejected = catalog.rejected().to_vec();
        if opts.dry_run {
            let plan = plugins::plan(repo, Some(&catalog), &s.state, None, None);
            report.plugins = plugin_rows(plan, &catalog, s.state.plugin_sources().get(&repo.id), None).0;
            if !report.rejected.is_empty() {
                report.outcome = Outcome::NeedsAttention;
            }
            return Ok(());
        }
        s.authorize()?;
        let mut native = snapshot(self.plugin_manager.as_ref(), cancel).map_err(native_error)?;
        let mut plan = plugins::plan(repo, Some(&catalog), &s.state, Some(&native), request.choice);
        let registration = plan.actions.first().filter(|a| a.operation() == Operation::Register);
        let mut failure = None;
        if let Some(action) = registration {
            match s.apply_plugin(self.plugin_manager.as_ref(), repo, action, &mut native, cancel) {
                Ok(()) => plan = plugins::plan(repo, Some(&catalog), &s.state, Some(&native), request.choice),
                Err(error) => failure = Some(native_error(error.why)),
            }
        }
        let blocker = plan.blocker.as_ref().map(|why| local(why.to_string()));
        let (rows, actions) = plugin_rows(plan, &catalog, s.state.plugin_sources().get(&repo.id), Some(&native));
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
            let etag = if complete { etag } else { None };
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
                let source = active.unwrap_or(&recorded);
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
struct CheckRequest<'a> {
    explicit: bool,
    choice: Option<&'a Choice>,
}

fn plugin_rows(
    plan: Plan,
    catalog: &Catalog,
    record: Option<&PluginSourceState>,
    native: Option<&NativeSnapshot>,
) -> (Vec<PluginReport>, Vec<plugins::Action>) {
    let mut rows = Vec::with_capacity(plan.statuses.len());
    for (name, status) in plan.statuses.into_entries() {
        let desired = catalog.plugins().get(&name);
        let publisher = match desired {
            Some(p) => p.publisher.clone(),
            None => record.and_then(|r| r.approved.get(&name)).cloned().flatten(),
        };
        let version = native
            .and_then(|s| s.plugin(&name, catalog.market()).ok().flatten())
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
