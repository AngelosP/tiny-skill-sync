//! A source schedules one conditional listing, not a poll per file. Only a fresh listing of the
//! recorded scope, corroborated at one commit, supplies negative evidence. Placement isn't ownership.

use super::*;
use crate::apply::STALE_REVIEW;
use crate::apply::leaf::{LeafTarget, PinnedLeaf};
use crate::github::confirm_source_at;
use crate::reconcile::{Hold, LeafListing};
use crate::source::DestinationBinding;
use crate::state::LeafStamp;
use crate::tree::SourceDiscovery;

fn local(why: impl Into<String>) -> RemoteError {
    RemoteError::new(FailureKind::Local, why)
}

fn running(cancel: &AtomicBool) -> Result<(), RemoteError> {
    if cancel.load(Ordering::Acquire) { Err(RemoteError::new(FailureKind::Cancelled, "the check was stopped")) } else { Ok(()) }
}

struct Listing<'a> {
    found: SourceDiscovery<'a>,
    recorded: RepoConfig,
    root: PathBuf,
    binding: DestinationBinding,
    pin: String,
    reference: RepoRef<'a>,
    token: Option<Arc<Secret>>,
    private: bool,
    etag: Option<String>,
}

/// Display rows can omit binary content and bytes that differ only in line endings.
/// A choice must instead match the exact object and bytes read for the comparison.
pub(super) struct FileReview {
    source: RepoConfig,
    name: String,
    binding: DestinationBinding,
    desired: Option<SkillFile>,
    local: Option<LeafStamp>,
}

impl Listing<'_> {
    fn target(&self) -> LeafTarget<'_> {
        LeafTarget { source: &self.recorded, root: &self.root, binding: &self.binding, other_claim: None }
    }

    fn item<'a>(&'a self, name: &'a str, confirm: bool) -> Result<LeafListing<'a>, RemoteError> {
        LeafListing::from_discovery(name, &self.found, &self.recorded, confirm).map_err(local)
    }

    fn decide(&self, s: &mut Session, item: LeafListing, pin: Option<&PinnedLeaf>, held: &mut bool) -> Result<Verdict, RemoteError> {
        match s.applier.leaf_decision(&s.state, self.target(), item, pin) {
            Err(e) if e.kind == FailureKind::Local && s.state.binding_for(&self.recorded).map_err(local)?.is_some() => {
                // A proven layout is never changed by an uncertain leaf. Hold this name
                // without caching the ETag, so a later check can see that the block cleared.
                *held = true;
                Ok(Verdict::Skipped(Skip::Rejected { reason: e.message }))
            }
            verdict => verdict,
        }
    }
}

impl Engine {
    /// None means exactly one valid 304: do not open even one destination leaf on this path.
    fn file_listing<'a>(&self, s: &mut Session, repo: &'a RepoConfig, conditional: bool, cancel: &AtomicBool) -> Result<Option<Listing<'a>>, RemoteError> {
        running(cancel)?;
        let problems = repo.problems();
        if !problems.is_empty() {
            return Err(local(problems.join("; ")));
        }
        let bound = s.state.binding_for(repo).map_err(local)?.cloned();
        let root = s.roots.path(repo.kind).map_err(local)?.to_owned();
        let reference = RepoRef::of(repo).map_err(local)?;
        let record = s.state.loose_sources().get(&repo.id);
        let recorded = record.and_then(|r| r.recorded_source(&repo.id)).unwrap_or_else(|| repo.clone());
        let tag = record.filter(|r| conditional && r.poll.source == source_key(repo)).and_then(|r| r.poll.etag.clone());
        let (reply, token, private) = self.request_tree(Some(s), repo, &reference, tag.as_deref(), cancel)?;
        running(cancel)?;
        let TreeReply::Snapshot { snapshot, etag } = reply else {
            if tag.is_some() {
                return Ok(None);
            }
            return Err(RemoteError::new(FailureKind::Protocol, "GitHub answered 'not modified' without an ETag"));
        };
        if snapshot.truncated {
            return Err(RemoteError::new(FailureKind::TooLarge, "GitHub cut the folder listing short; nothing was changed"));
        }
        // A case-only suffix change can be filtered out by type discovery yet still occupy the
        // very same Windows name. That is not evidence that the owner's object went away.
        if let Some(record) = s.state.loose_sources().get(&repo.id) {
            for entry in snapshot.entries.iter().filter(|e| !e.path.contains('/')) {
                if crate::state::check_leaf_name(repo.kind, &entry.path).is_err() && record.leaves().iter().any(|l| winfs::same_win_name(&entry.path, &l.name))
                {
                    return Err(local("the listing filtered a spelling of a managed file; nothing was removed"));
                }
            }
        }
        let found =
            tree::discover_source(repo, snapshot, root.to_string_lossy().encode_utf16().count()).map_err(|why| RemoteError::new(FailureKind::Protocol, why))?;
        if !found.complete_for(&recorded) {
            let reason = found.rejected().first().map(|r| format!("{}: {}", r.name, r.reason)).unwrap_or_else(|| "unsafe or inconsistent entries".into());
            return Err(RemoteError::new(FailureKind::Protocol, format!("the listing cannot confirm this source ({reason}); nothing was changed")));
        }
        // Only a new source needs a scan for placement. A bound source still checks the
        // alternate destination for EACH name before touching that file.
        let names = if bound.is_none() { found.names() } else { Vec::new() };
        let binding = source_preview::locate_files(repo, &root, &names, bound.as_ref(), cancel).map_err(local)?;
        let pin = self.remote.commit(&reference, token.as_deref(), cancel)?;
        confirm_source_at(self.remote.as_ref(), &recorded, &found, &pin, token.as_deref(), cancel)?;
        Ok(Some(Listing { found, recorded, root, binding, pin, reference, token, private, etag }))
    }

    fn file_bytes(&self, list: &Listing, file: &SkillFile, cancel: &AtomicBool) -> Result<Vec<u8>, RemoteError> {
        download(self.remote.as_ref(), &list.reference, &list.pin, &file.path, file, list.token.as_deref(), cancel)
    }

    pub(super) fn sync_files(&self, s: &mut Session, repo: &RepoConfig, opts: &SyncOptions, cancel: &AtomicBool) -> RepoReport {
        let mut report = RepoReport::new(repo);
        match self.sync_files_in(s, repo, opts, cancel, &mut report) {
            Ok(()) => report,
            Err(e) if e.kind == FailureKind::Cancelled => RepoReport { outcome: Outcome::Cancelled, ..report },
            Err(e) => report.fail(e),
        }
    }

    fn sync_files_in(&self, s: &mut Session, repo: &RepoConfig, opts: &SyncOptions, cancel: &AtomicBool, report: &mut RepoReport) -> Result<(), RemoteError> {
        let Some(list) = self.file_listing(s, repo, !opts.ignore_etag && !opts.confirm_mass_delete, cancel)? else {
            report.lists_nothing = s.state.loose_sources()[&repo.id].poll.lists_nothing;
            if !opts.dry_run {
                s.touch_check(&repo.id, self.now());
            }
            return Ok(());
        };
        let mut names = list.found.names();
        report.lists_nothing = names.is_empty();
        let mut seen: HashSet<_> = names.iter().map(|n| fold(n)).collect();
        if let Some(r) = s.state.loose_sources().get(&repo.id) {
            names.extend(r.leaves().iter().filter(|l| seen.insert(fold(&l.name))).map(|l| l.name.clone()));
        }
        // Finish the remote work before the first transaction. A bad download (even the last
        // one) cannot turn this listing into permission to delete. Bytes are bounded by the check budget.
        let mut plan = Vec::new();
        let mut local_held = false;
        for name in &names {
            running(cancel)?;
            let item = list.item(name, opts.confirm_mass_delete)?;
            let mut verdict = list.decide(s, item, None, &mut local_held)?;
            let mut bytes = None;
            if let Some(file) = item.desired().filter(|_| {
                matches!(
                    verdict,
                    Verdict::Install | Verdict::Update | Verdict::Adopt | Verdict::CatchUp | Verdict::Held(Hold::UnmanagedDiffers | Hold::UpdateBlocked(_))
                )
            }) {
                if s.downloaded.saturating_add(file.size) > opts.max_download_bytes {
                    return Err(RemoteError::new(FailureKind::TooLarge, "the source reached this check's download limit; nothing was changed"));
                }
                let body = self.file_bytes(&list, file, cancel)?;
                s.downloaded += body.len() as u64;
                let pin = PinnedLeaf::new(file, &list.pin, &body)?;
                verdict = list.decide(s, item, Some(&pin), &mut local_held)?;
                if verdict.is_action() {
                    bytes = Some(body);
                }
            }
            plan.push((item, verdict, bytes));
        }
        running(cancel)?;
        if !opts.dry_run {
            s.authorize()?;
            if s.state.binding_for(repo).map_err(local)?.is_none() {
                s.state.bind_destination(repo, list.binding.clone()).map_err(local)?;
                s.must_save = true;
            }
        }
        let mut settled = !local_held;
        let mut acted = false;
        let mut attention = false;
        for (item, verdict, bytes) in plan {
            running(cancel)?;
            let result = if !verdict.is_action() {
                ApplyResult::NotApplicable
            } else if opts.dry_run {
                ApplyResult::DryRun
            } else {
                let pin = bytes.as_deref().zip(item.desired()).map(|(b, f)| PinnedLeaf::new(f, &list.pin, b)).transpose()?;
                match s.applier.apply_leaf(&mut s.state, list.target(), item, pin.as_ref(), &verdict).inspect_err(|_| s.suspect = true)? {
                    Applied::Done => ApplyResult::Done,
                    Applied::Held(why) => ApplyResult::Held(why),
                    Applied::Displaced { backup, .. } => {
                        s.warnings.push(format!("'{}': the change finished, but the previous copy is still in {}", item.name, backup.display()));
                        ApplyResult::Done
                    }
                }
            };
            acted |= verdict.is_action();
            attention |= verdict.needs_attention() || matches!(result, ApplyResult::Held(_));
            settled &= !matches!(verdict, Verdict::Held(_)) && is_settled(&verdict, &result);
            report.skills.push(SkillReport { name: item.name.to_owned(), verdict, result });
        }
        report.outcome = match (opts.dry_run, acted, attention) {
            (true, true, _) => Outcome::WouldChange,
            (_, _, true) => Outcome::NeedsAttention,
            (false, true, false) => Outcome::Synced,
            _ => Outcome::Unchanged,
        };
        if !opts.dry_run {
            let poll = s.state.leaf_poll_mut(repo).map_err(local)?;
            let old = poll.clone();
            poll.etag = list.etag.filter(|_| settled);
            poll.needs_token = list.private;
            poll.source = source_key(repo);
            poll.lists_nothing = report.lists_nothing;
            s.must_save |= old != *poll;
            s.touch_check(&repo.id, self.now());
        }
        Ok(())
    }

    pub(super) fn resolve_file(
        &self,
        s: &mut Session,
        repo: &RepoConfig,
        name: &str,
        how: Resolution,
        cancel: &AtomicBool,
    ) -> Result<ResolveReport, RemoteError> {
        crate::state::check_leaf_name(repo.kind, name).map_err(local)?;
        running(cancel)?;
        let r = s.state.loose_sources().get(&repo.id).ok_or_else(|| local("check this source before resolving a file"))?;
        let leaf = r.leaf(name);
        if leaf.is_some_and(|l| l.interrupted()) {
            return Err(local("finish the interrupted file change before resolving it"));
        }
        let ignored = r.poll.ignored.iter().any(|n| fold(n) == fold(name));
        let mut backups = Vec::new();
        let message = match how {
            Resolution::ManageAgain if ignored => {
                s.authorize()?;
                let poll = s.state.leaf_poll_mut(repo).map_err(local)?;
                poll.ignored.retain(|n| fold(n) != fold(name));
                poll.etag = None;
                format!("'{name}' will be checked again")
            }
            Resolution::ManageAgain => return Err(local("this file is not marked as yours")),
            Resolution::PauseHistory | Resolution::ResumeHistory => return Err(local("earlier-version searches are for skills, not individual files")),
            Resolution::KeepMine => {
                if leaf.is_none() && !ignored {
                    let list = self.file_listing(s, repo, false, cancel)?.ok_or_else(|| local("a fresh listing is required"))?;
                    let item = list.item(name, false)?;
                    if item.desired().is_none() || matches!(s.applier.leaf_decision(&s.state, list.target(), item, None)?, Verdict::Skipped(_)) {
                        return Err(local("this file is not managed or available from this source"));
                    }
                }
                s.authorize()?;
                let r = s.state.bound_loose_mut(repo).map_err(local)?;
                r.set_leaf(name, None);
                if !ignored {
                    r.poll.ignored.push(name.into());
                }
                format!("'{name}' is now yours: it will not be changed or removed")
            }
            Resolution::Overwrite if ignored => return Err(local("this file is marked as yours; choose 'manage again' first")),
            Resolution::Overwrite => {
                let review = self.file_review.as_ref().filter(|r| r.source.id == repo.id && fold(&r.name) == fold(name));
                let list = self.file_listing(s, repo, false, cancel)?.ok_or_else(|| local("a fresh listing is required"))?;
                let item = list.item(name, false)?;
                if review.is_some_and(|r| r.source != *repo || r.binding != list.binding || r.desired.as_ref() != item.desired()) {
                    return Err(local(STALE_REVIEW));
                }
                let file = item.desired().ok_or_else(|| local("this source no longer lists the file"))?;
                let bytes = self.file_bytes(&list, file, cancel)?;
                let pin = PinnedLeaf::new(file, &list.pin, &bytes)?;
                let reviewed = review.map(|r| &r.local);
                let verdict = s.applier.reviewed_leaf_decision(&s.state, list.target(), item, Some(&pin), reviewed)?;
                running(cancel)?;
                s.authorize()?;
                if verdict != Verdict::InSync {
                    match s.applier.replace_leaf(&mut s.state, list.target(), item, &pin, &verdict, reviewed)? {
                        Applied::Done => {}
                        Applied::Displaced { backup, .. } => backups.push(backup),
                        Applied::Held(why) => return Err(local(why)),
                    }
                }
                s.state.leaf_poll_mut(repo).map_err(local)?.etag = None;
                format!("'{name}' matches the repository")
            }
        };
        s.must_save = true;
        Ok(ResolveReport { message, backups })
    }

    pub(super) fn view_file(
        &self,
        s: &mut Session,
        repo: &RepoConfig,
        name: &str,
        whole: bool,
        cancel: &AtomicBool,
    ) -> Result<(DiffReport, Option<FileReview>), RemoteError> {
        running(cancel)?;
        crate::state::check_leaf_name(repo.kind, name).map_err(local)?;
        let list = if whole { None } else { self.file_listing(s, repo, false, cancel)? };
        let r = s.state.loose_sources().get(&repo.id);
        let leaf = r.and_then(|r| r.leaf(name));
        if leaf.is_some_and(|l| l.interrupted()) {
            return Err(local("the file has an interrupted change"));
        }
        let installed = leaf.and_then(|l| l.installed.as_ref());
        let desired = list.as_ref().map(|l| l.item(name, false)).transpose()?.and_then(|i| i.desired());
        if installed.is_none() && desired.is_none() {
            return Err(local("this file is not recorded or listed by this source"));
        }
        let root = s.roots.path(repo.kind).map_err(local)?;
        let binding = list.as_ref().map(|l| &l.binding).or_else(|| r.map(|r| r.binding())).ok_or_else(|| local("the destination is not bound"))?;
        let target = LeafTarget { source: repo, root, binding, other_claim: None };
        let name = leaf.map(|l| l.name.as_str()).or_else(|| desired.map(|f| f.path.as_str())).unwrap_or(name);
        let mut ours = s.applier.read_leaf_view(&s.state, target, name, DIFF_FILE_BYTES, !whole)?;
        let here = ours.is_some();
        let stamp = ours.as_mut().and_then(|view| view.stamp.take());
        let ours: Vec<_> = ours.map(|view| (name.to_owned(), view.bytes.map_or_else(|| Content::Hidden(TOO_BIG.into()), Content::Bytes))).into_iter().collect();
        if whole {
            if !here {
                return Err(local("the installed file is missing on this PC"));
            }
            let files = ours.into_iter().map(|(path, c)| FileChange { path, presence: Presence::Both, body: diff::whole(c) }).collect();
            return Ok((DiffReport { on_github: true, here, files }, None));
        }
        let theirs = if let (Some(list), Some(file)) = (&list, desired) {
            let content = if file.size <= DIFF_FILE_BYTES { Content::Bytes(self.file_bytes(list, file, cancel)?) } else { Content::Hidden(TOO_BIG.into()) };
            vec![(name.to_owned(), content)]
        } else {
            vec![]
        };
        let review = FileReview { source: repo.clone(), name: name.into(), binding: binding.clone(), desired: desired.cloned(), local: stamp };
        Ok((DiffReport { on_github: desired.is_some(), here, files: diff::compare(theirs, ours) }, Some(review)))
    }
}

impl Session {
    pub(super) fn recover_files(&mut self, cfg: &Config) -> Result<(), EngineError> {
        let pending: Vec<_> = self
            .state
            .loose_sources()
            .iter()
            .filter_map(|(id, record)| {
                let names: Vec<_> = record.leaves().iter().filter(|leaf| leaf.interrupted()).map(|leaf| leaf.name.clone()).collect();
                (!names.is_empty()).then(|| (id.clone(), record.recorded_source(id), record.binding().clone(), names))
            })
            .collect();
        for (id, source, binding, names) in pending {
            let Some(source) = source else {
                self.suspect = true;
                for name in names {
                    self.notes.push(Note {
                        repo: id.clone(),
                        skill: name,
                        text: "could not finish the interrupted file change: the recorded source origin is missing".into(),
                    });
                }
                continue;
            };
            let active = cfg.find(&id);
            if !active.is_some_and(|active| tree::same_source_scope(active, &source)) {
                continue;
            }
            self.authorize().map_err(|e| failed(e.message))?;
            let root = binding.root().to_owned();
            let target = LeafTarget { source: &source, binding: &binding, root: &root, other_claim: None };
            for name in names {
                let text = match self.applier.recover_leaf(&mut self.state, target, &name) {
                    Ok(Applied::Done) => "finished the interrupted file change".into(),
                    Ok(Applied::Held(why)) => why,
                    Ok(Applied::Displaced { backup, .. }) => format!("finished the file change; the previous copy is in {}", backup.display()),
                    Err(e) => {
                        self.suspect = true;
                        format!("could not finish the interrupted file change: {e}")
                    }
                };
                self.notes.push(Note { repo: id.clone(), skill: name, text });
            }
        }
        Ok(())
    }

    pub(super) fn verify_files(&mut self, repo: &RepoConfig) -> Vec<(String, LocalStatus)> {
        let Some(r) = self.state.loose_sources().get(&repo.id) else { return vec![] };
        r.leaves()
            .iter()
            .map(|l| {
                let status = if l.interrupted() {
                    LocalStatus::Interrupted
                } else {
                    match verify::leaf::status(r, r.binding(), &l.name, &mut self.applier.hasher) {
                        Ok(local) => LocalStatus::Checked(local),
                        Err(e) => LocalStatus::Unavailable(e),
                    }
                };
                (l.name.clone(), status)
            })
            .collect()
    }
}
