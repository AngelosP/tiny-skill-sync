//! One source, one listing. A source's skills, agents and instructions lanes all read the same conditional,
//! recursive listing of the branch root and the inventory built from it, so an unchanged source costs one request.
//! Any failure before the inventory exists ends the whole source; nothing is decided from it.

use super::plugins::CheckRequest;
use super::*;
use crate::inventory::{self, InstallRoots, Inventory, ManifestFiles, Policy};
use std::cell::RefCell;

/// A branch-root listing that can be used at all: complete, well formed, and with the source's folder in it.
pub(super) fn check_root(snapshot: &tree::Snapshot, repo: &str, folder: &str) -> Result<(), RemoteError> {
    if snapshot.truncated {
        return Err(RemoteError::new(FailureKind::TooLarge, TRUNCATED));
    }
    tree::entry_index(snapshot).map_err(protocol)?;
    if !folder.is_empty() && !snapshot.entries.iter().any(|e| e.path == folder && e.kind == tree::EntryKind::Tree && e.mode == "040000") {
        return Err(RemoteError::new(FailureKind::NotFound, format!("the folder '{}' is not on this branch of {repo}", pathsafe::printable(folder))));
    }
    Ok(())
}

/// GitHub answered 'not modified', with or without the token.
pub(super) struct Unchanged {
    private: bool,
}

/// The branch root as one complete listing, and what is in it for the source's folder.
pub(super) struct RootListing {
    pub(super) snapshot: tree::Snapshot,
    pub(super) inventory: Inventory,
    /// GitHub's ETag with the folder rules it was read under, as each lane stores it (see `tagged`).
    pub(super) etag: Option<String>,
    pub(super) token: Option<Arc<Secret>>,
    pub(super) private: bool,
    /// The commit the plugin manifests were read at, when one was needed. A lane that needs a pin and finds none
    /// asks for the branch's commit itself; `github::confirm_source_at` still proves it shows the listed folder.
    pub(super) pin: Option<String>,
    /// The branch listed: the configured one, or the default branch's name when it was resolved first.
    pub(super) branch: String,
    /// Every plugin manifest in the source's folder, by branch-root path, and the branch-root marketplace, as read.
    pub(super) manifests: HashMap<String, Vec<u8>>,
    pub(super) marketplace: Option<Vec<u8>>,
    /// The folder, pin and tree ID `github::confirm_source_at` last proved for a lane view of this listing, so the
    /// next lane of the same location reuses that one request instead of asking GitHub again.
    pub(super) confirmed: RefCell<Option<String>>,
}

const TRUNCATED: &str = "GitHub could not list the repository in one piece; nothing was changed";
const OVER_BUDGET: &str = "The plugin manifests reached this check's download limit; nothing was changed";

/// Fails before anything is downloaded when the listed sizes of `wanted` do not fit in what is left of `max`, or
/// when there are more manifests than a skill may have files.
pub(super) fn check_budget(wanted: &ManifestFiles, spent: u64, max: u64) -> Result<(), RemoteError> {
    let total = wanted.plugins.iter().chain(&wanted.marketplace).fold(spent, |sum, f| sum.saturating_add(f.size));
    if total > max || wanted.plugins.len() > tree::MAX_SKILL_FILES {
        return Err(RemoteError::new(FailureKind::TooLarge, OVER_BUDGET));
    }
    Ok(())
}

/// The plugin manifests by path, and the marketplace catalog when there is one.
type Manifests = (HashMap<String, Vec<u8>>, Option<Vec<u8>>);

/// Downloads every plugin manifest and the marketplace `wanted` names at `commit`, each checked against its blob
/// ID. The listed size is checked against the budget (`spent` so far, of `max`) before each download, and a stop
/// is honoured between files.
pub(super) fn read_manifests(
    remote: &dyn Remote,
    at: &RepoRef,
    commit: &str,
    wanted: &ManifestFiles,
    token: Option<&Secret>,
    budget: (&mut u64, u64),
    cancel: &AtomicBool,
) -> Result<Manifests, RemoteError> {
    let (spent, max) = budget;
    let mut manifests = HashMap::new();
    let mut marketplace = None;
    for (file, is_market) in wanted.plugins.iter().map(|f| (f, false)).chain(wanted.marketplace.iter().map(|f| (f, true))) {
        if cancel.load(Ordering::Relaxed) {
            return Err(RemoteError::new(FailureKind::Cancelled, "the check was stopped"));
        }
        if spent.saturating_add(file.size) > max {
            return Err(RemoteError::new(FailureKind::TooLarge, OVER_BUDGET));
        }
        let bytes = download(remote, at, commit, &file.path, file, token, cancel)?;
        *spent += bytes.len() as u64;
        if is_market {
            marketplace = Some(bytes);
        } else {
            manifests.insert(file.path.clone(), bytes);
        }
    }
    Ok((manifests, marketplace))
}

fn protocol(why: impl Into<String>) -> RemoteError {
    RemoteError::new(FailureKind::Protocol, why)
}

/// A stored ETag also names the folder rules the listing was judged by, so a changed rule is never answered with
/// GitHub's "not modified" for the old judgement.
fn tagged(etag: &str, rules: &str) -> String {
    format!("{etag}#{rules}")
}

fn untagged<'a>(stored: &'a str, rules: &str) -> Option<&'a str> {
    stored.strip_suffix(rules)?.strip_suffix('#')
}

/// The folder rules a source's listing is judged by, effective for `lane`'s source.
fn policy_of(s: &Session, lane: &RepoConfig) -> Result<Policy, RemoteError> {
    let global = s.config.global();
    let policy = match s.config.find_source(&lane.id) {
        Some(source) => source.effective_policy(global),
        None => Ok(Policy { include_repo_items: global.include_repo_items, ignore_folders: global.ignore_folders.iter().map(|n| fold(n)).collect() }),
    };
    policy.map_err(|why| RemoteError::new(FailureKind::Local, why))
}

fn rules_key(policy: &Policy) -> String {
    blob_hex(format!("{policy:?}").as_bytes())[..12].to_owned()
}

fn lane_failure(report: &RepoReport) -> Option<&Failure> {
    match &report.outcome {
        Outcome::Failed(f) if f.kind != FailureKind::Local => Some(f),
        _ => None,
    }
}

impl Engine {
    /// The ETag every lane of the source agrees on, when each lane may skip an unchanged listing.
    fn shared_etag(&self, s: &Session, lanes: &[&RepoConfig], opts: &SyncOptions, request: CheckRequest<'_>) -> Option<String> {
        if opts.ignore_etag || opts.confirm_mass_delete || request.choice.is_some() {
            return None;
        }
        let rules = rules_key(&policy_of(s, lanes.first()?).ok()?);
        let mut shared: Option<String> = None;
        for lane in lanes {
            let tag = match lane.kind {
                SourceKind::Skills => {
                    let r = s.state.repo(&lane.id)?;
                    if !(r.source.is_empty() || r.source == source_key(lane)) {
                        return None;
                    }
                    r.etag.clone()?
                }
                SourceKind::Plugins => {
                    // A retry, or saved attention whose rows this process no longer holds, needs a listing. An explicit
                    // check of approved plugins also asks Copilot again, so it reads GitHub too.
                    if self.schedule.retry_of(&lane.id).is_some() {
                        return None;
                    }
                    match s.state.plugin_sources().get(&lane.id) {
                        Some(r) if !r.origin.matches_lane(lane) => return None,
                        Some(r) if request.explicit || (r.poll.needs_attention && !self.plugin_attention_rows.contains(&lane.id)) => return None,
                        Some(r) => r.poll.etag.clone()?,
                        None => {
                            let poll = s.state.lane_poll(lane)?;
                            if poll.attention && !self.plugin_attention_rows.contains(&lane.id) {
                                return None;
                            }
                            poll.etag.clone()?
                        }
                    }
                }
                _ => match s.state.loose_sources().get(&lane.id) {
                    Some(r) if r.poll.source != source_key(lane) => return None,
                    Some(r) => r.poll.etag.clone()?,
                    None => s.state.lane_poll(lane)?.etag.clone()?,
                },
            };
            if shared.as_ref().is_some_and(|t| *t != tag) {
                return None;
            }
            shared = Some(tag);
        }
        shared.and_then(|t| untagged(&t, &rules).map(str::to_owned))
    }

    /// Every lane stands as it was: one report per lane, as from a 'not modified' reply.
    fn unchanged_reports(&self, s: &mut Session, lanes: &[&RepoConfig], opts: &SyncOptions) -> Vec<RepoReport> {
        lanes
            .iter()
            .map(|lane| {
                let mut report = RepoReport::new(lane);
                report.lists_nothing = self.listed_nothing(s, lane);
                if lane.kind == SourceKind::Plugins && self.plugin_lane_needs_attention(s, lane) {
                    report.outcome = Outcome::NeedsAttention;
                }
                if !opts.dry_run {
                    s.touch_check(&lane.id, self.now());
                }
                report
            })
            .collect()
    }

    fn listed_nothing(&self, s: &Session, lane: &RepoConfig) -> bool {
        let unrecorded = || s.state.lane_poll(lane).is_some_and(|p| p.lists_nothing);
        match lane.kind {
            SourceKind::Skills => s.state.repo(&lane.id).is_some_and(|r| r.lists_nothing),
            SourceKind::Plugins => s.state.plugin_sources().get(&lane.id).map_or_else(unrecorded, |r| r.poll.lists_nothing),
            _ => s.state.loose_sources().get(&lane.id).map_or_else(unrecorded, |r| r.poll.lists_nothing),
        }
    }

    /// Lists the branch root once for `lanes` (the source's skills, agents, instructions and plugins lanes that are on,
    /// in that order) and runs each lane on it. A remote failure in one lane ends the source: the lanes after it report
    /// the same failure and are not run. A local failure (such as Copilot CLI) ends only its lane. With `confirmed`,
    /// `opts.confirm_mass_delete` applies only to the lane with that ID.
    pub(super) fn sync_source(
        &mut self,
        s: &mut Session,
        lanes: &[&RepoConfig],
        opts: &SyncOptions,
        confirmed: Option<&str>,
        request: CheckRequest<'_>,
        cancel: &AtomicBool,
    ) -> Vec<RepoReport> {
        let fail_all = |e: RemoteError| -> Vec<RepoReport> {
            lanes
                .iter()
                .map(|l| {
                    if e.kind == FailureKind::Cancelled {
                        RepoReport { outcome: Outcome::Cancelled, ..RepoReport::new(l) }
                    } else {
                        RepoReport::new(l).fail(e.clone())
                    }
                })
                .collect()
        };
        let Some(first) = lanes.first() else { return Vec::new() };
        // A file lane that cannot be placed fails before anything is asked of GitHub.
        for lane in lanes.iter().filter(|l| matches!(l.kind, SourceKind::Agents | SourceKind::Instructions)) {
            if let Err(why) = s.state.binding_for(lane).and(s.roots.path(lane.kind).map(|_| ())) {
                return fail_all(RemoteError::new(FailureKind::Local, why));
            }
        }
        let skills = lanes.iter().find(|l| l.kind.is_skills()).copied();
        let mut head = None;
        let mut has_searches = false;
        if let Some(repo) = skills {
            let problems = repo.problems();
            if !problems.is_empty() {
                return fail_all(RemoteError::new(FailureKind::Local, problems.join("; ")));
            }
            let searches = s.state.repo(&repo.id).map(|r| r.searches.clone()).unwrap_or_default();
            has_searches = !searches.is_empty();
            if has_searches {
                let source = source_key(repo);
                let cache_valid = searches.values().all(|h| h.paused || (h.complete && h.source == source && evidence_valid(s, h)));
                let same_source = s.state.repo(&repo.id).is_none_or(|r| r.source.is_empty() || r.source == source);
                let reference = match RepoRef::of(repo) {
                    Ok(r) => r,
                    Err(why) => return fail_all(RemoteError::new(FailureKind::Local, why)),
                };
                match self.history_head(s, repo, &reference, cancel) {
                    Ok(h) => head = Some(h),
                    Err(e) => return fail_all(e),
                }
                // A tree can return to exactly the same bytes after new history was added. A cached comparison
                // therefore polls the branch commit, not the tree ETag; it still costs just one request when unchanged.
                // An unchanged commit is an unchanged tree, so the other lanes of the source stand as they were too,
                // provided each of them could have skipped the listing on its own.
                if cache_valid
                    && same_source
                    && !opts.ignore_etag
                    && policy_of(s, repo).is_ok_and(|p| {
                        let rules = rules_key(&p);
                        s.state.repo(&repo.id).is_some_and(|r| r.etag.as_deref().is_some_and(|t| untagged(t, &rules).is_some()) && r.history_head == head)
                    })
                    && (lanes.len() == 1 || self.shared_etag(s, lanes, opts, request).is_some())
                {
                    return self.unchanged_reports(s, lanes, opts);
                }
            }
        }
        let tag = if has_searches { None } else { self.shared_etag(s, lanes, opts, request) };
        let needs_pin = lanes.iter().any(|l| matches!(l.kind, SourceKind::Agents | SourceKind::Instructions));
        // Plugins install from a named branch, so a full check of a bare repository resolves its default branch.
        let resolve = lanes.iter().any(|l| l.kind == SourceKind::Plugins);
        let root = match self.root_listing(s, first, tag.as_deref(), needs_pin, head.clone(), resolve, opts.max_download_bytes, cancel) {
            Ok(Ok(root)) => root,
            Ok(Err(Unchanged { private })) => {
                // Exactly one valid 304: every lane stands as it was, and no file is read or written unless whether
                // the repository needs the token changed.
                if !opts.dry_run {
                    Self::remember_private(s, lanes, private);
                }
                return self.unchanged_reports(s, lanes, opts);
            }
            Err(e) => return fail_all(e),
        };
        if let Some(repo) = skills.filter(|_| !opts.dry_run) {
            Self::remember_private(s, &[repo], root.private);
        }
        let mut reports: Vec<RepoReport> = Vec::with_capacity(lanes.len());
        let mut stop: Option<Result<Failure, ()>> = None;
        let mut resolved = None;
        for lane in lanes {
            if let Some(stop) = &stop {
                reports.push(match stop {
                    Ok(f) => RepoReport::new(lane).fail(f.clone()),
                    Err(()) => RepoReport { outcome: Outcome::Cancelled, ..RepoReport::new(lane) },
                });
                continue;
            }
            // A confirmed mass deletion covers only the lane it was confirmed for.
            let lane_opts = SyncOptions { confirm_mass_delete: opts.confirm_mass_delete && confirmed.is_none_or(|id| id == lane.id), ..opts.clone() };
            let opts = &lane_opts;
            let report = match lane.kind {
                SourceKind::Skills => self.sync_skills_lane(s, lane, opts, cancel, &root, head.clone()),
                SourceKind::Plugins => self.sync_plugin_lane(s, lane, opts, request, &root, &mut resolved, cancel),
                _ => self.sync_files(s, lane, opts, cancel, &root),
            };
            if matches!(report.outcome, Outcome::Cancelled) {
                stop = Some(Err(()));
            } else if let Some(f) = lane_failure(&report) {
                stop = Some(Ok(f.clone()));
            }
            reports.push(report);
        }
        let branch = resolved.unwrap_or_else(|| root.branch.clone());
        s.seen.push((first.source.clone(), crate::seen::Seen { at: self.now(), branch, summary: root.inventory.summary() }));
        reports
    }

    /// One conditional, recursive listing of the branch root of `lane`'s repository, then every plugin manifest and
    /// the marketplace in the source's folder, downloaded at one commit and checked against their blob IDs. `None`
    /// means GitHub answered 'not modified' to `etag`. With `pin`, that commit is used; otherwise one is asked for
    /// when a manifest must be read or `needs_pin` is set. With `resolve`, a bare repository's default branch is
    /// asked for on a full check (no `etag`, or a poll at the default HEAD that came back changed) and that branch is
    /// listed by name, so the listing and the branch name always agree. The
    /// manifests count against `max` bytes of this check's downloads.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn root_listing(
        &self,
        s: &mut Session,
        lane: &RepoConfig,
        etag: Option<&str>,
        needs_pin: bool,
        pin: Option<String>,
        resolve: bool,
        max: u64,
        cancel: &AtomicBool,
    ) -> Result<Result<RootListing, Unchanged>, RemoteError> {
        let problems = lane.problems();
        if !problems.is_empty() {
            return Err(RemoteError::new(FailureKind::Local, problems.join("; ")));
        }
        let lane_ref = RepoRef::of(lane).map_err(|why| RemoteError::new(FailureKind::Local, why))?;
        // Plugins install from a named branch, so a source that follows the default branch lists it by name for a
        // full check. Polls with an ETag ask at the default HEAD, so an unchanged source costs one request.
        let bare = resolve && lane.branch.is_empty();
        let named = |token: Option<&Secret>| -> Result<String, RemoteError> {
            let branch = self.remote.default_branch(&lane_ref, token, cancel)?;
            let problems = RepoConfig { branch: branch.clone(), ..lane.clone() }.problems();
            if problems.is_empty() { Ok(branch) } else { Err(protocol(problems.join("; "))) }
        };
        let by_name = |branch: &str, token: Option<&Secret>| -> Result<(Option<String>, tree::Snapshot), RemoteError> {
            match self.remote.tree(&RepoRef { path: "", branch, ..lane_ref }, None, token, cancel)? {
                TreeReply::Snapshot { etag, snapshot } => Ok((etag, snapshot)),
                TreeReply::NotModified => Err(protocol("GitHub answered 'not modified' to a request that had no ETag")),
            }
        };
        let (branch, etag, snapshot, token, private) = if bare && etag.is_none() {
            let (branch, token, private) = self.ask_github(Some(s), lane, cancel, named)?;
            let (etag, snapshot, token, private) = if token.is_some() {
                // The default branch already showed which token reads this repository.
                let (etag, snapshot) = by_name(&branch, token.as_deref())?;
                (etag, snapshot, token, private)
            } else {
                match self.request_tree(Some(s), lane, &RepoRef { path: "", branch: &branch, ..lane_ref }, None, cancel)? {
                    (TreeReply::Snapshot { etag, snapshot }, token, private) => (etag, snapshot, token, private),
                    (TreeReply::NotModified, _, _) => return Err(protocol("GitHub answered 'not modified' to a request that had no ETag")),
                }
            };
            (branch, etag, snapshot, token, private)
        } else {
            let (reply, token, private) = self.request_tree(Some(s), lane, &RepoRef { path: "", branch: &lane.branch, ..lane_ref }, etag, cancel)?;
            match reply {
                TreeReply::NotModified if etag.is_some() => return Ok(Err(Unchanged { private })),
                TreeReply::NotModified => return Err(protocol("GitHub answered 'not modified' to a request that had no ETag")),
                TreeReply::Snapshot { etag: head_etag, snapshot: at_head } if bare => {
                    // Changed at HEAD: every lane reads the branch by name, so the plugins lane and the listing agree.
                    // The HEAD reply's ETag is kept when both show the same tree, so the next poll at HEAD can be a 304.
                    let branch = named(token.as_deref())?;
                    let (etag, snapshot) = by_name(&branch, token.as_deref())?;
                    let etag = if snapshot.sha == at_head.sha { head_etag } else { etag };
                    (branch, etag, snapshot, token, private)
                }
                TreeReply::Snapshot { etag, snapshot } => (lane.branch.clone(), etag, snapshot, token, private),
            }
        };
        let root_ref = RepoRef { path: "", branch: &branch, ..lane_ref };
        if cancel.load(Ordering::Relaxed) {
            return Err(RemoteError::new(FailureKind::Cancelled, "the check was stopped"));
        }
        check_root(&snapshot, &lane.repo, &lane.path)?;
        let policy = policy_of(s, lane)?;
        let etag = etag.map(|e| tagged(&e, &rules_key(&policy)));
        let wanted = inventory::manifest_files(&snapshot, &lane.path).map_err(protocol)?;
        check_budget(&wanted, s.downloaded, max)?;
        let mut pin = pin;
        if pin.is_none() && (needs_pin || wanted.marketplace.is_some() || !wanted.plugins.is_empty()) {
            pin = Some(self.remote.commit(&root_ref, token.as_deref(), cancel)?);
        }
        let commit = pin.clone().unwrap_or_default();
        let (manifests, marketplace) = read_manifests(self.remote.as_ref(), &root_ref, &commit, &wanted, token.as_deref(), (&mut s.downloaded, max), cancel)?;
        let utf16 = |kind| s.roots.path(kind).map(|p| p.to_string_lossy().encode_utf16().count()).unwrap_or(0);
        let roots = InstallRoots { skills: root_len(&s.applier), agents: utf16(SourceKind::Agents), instructions: utf16(SourceKind::Instructions) };
        let name = lane.owner_and_name().map(|(_, n)| n.to_owned()).unwrap_or_default();
        let inventory = inventory::inventory(&snapshot, &name, &lane.path, &manifests, marketplace.as_deref(), &policy, roots).map_err(protocol)?;
        Ok(Ok(RootListing { snapshot, inventory, etag, token, private, pin, branch, manifests, marketplace, confirmed: RefCell::new(None) }))
    }

    /// A fresh, unconditional listing for a choice the user made about one item.
    pub(super) fn fresh_root(&self, s: &mut Session, lane: &RepoConfig, cancel: &AtomicBool) -> Result<RootListing, RemoteError> {
        self.root_listing(s, lane, None, false, None, lane.kind == SourceKind::Plugins, DEFAULT_MAX_DOWNLOAD, cancel)?
            .map_err(|_| protocol("GitHub answered 'not modified' to a request that had no ETag"))
    }

    /// Records whether the repository needed the token, on each lane that keeps that fact, only when it changed.
    fn remember_private(s: &mut Session, lanes: &[&RepoConfig], private: bool) {
        for lane in lanes {
            if lane.kind.is_skills() {
                if private != s.state.repo(&lane.id).is_some_and(|r| r.needs_token) {
                    s.state.repo_mut(&lane.id).needs_token = private;
                    s.must_save = true;
                }
            } else if s.state.loose_sources().get(&lane.id).is_some_and(|r| r.poll.needs_token != private) {
                if let Ok(poll) = s.state.leaf_poll_mut(lane) {
                    poll.needs_token = private;
                    s.must_save = true;
                }
            }
        }
    }
}
