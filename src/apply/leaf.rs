//! One file, not a one-file skill. The engine supplies a freshly resolved binding, the complete
//! listing (or an explicit removal), and bytes downloaded at a pinned commit. Never enumerate,
//! swap, carry or prune a shared destination. Never recover by promoting a stage.

use super::{Applied, Applier, RemoteError, STALE_REVIEW, Step, changed, local};
use crate::config::RepoConfig;
use crate::fsx::unique_suffix;
use crate::hash::{blob_hex, is_sha1_hex};
use crate::pathsafe::{self, fold};
use crate::reconcile::{LeafFacts, LeafListing, Verdict, plan_leaf};
use crate::source::{DestinationBinding, Entry, resolve_destination};
use crate::state::{DiskId, LeafStamp, LeafTxn, LeafVersion, ManagedFile, OwnedLeaf, State};
use crate::tree::{MAX_FILE_BYTES, SkillFile};
use crate::verify::leaf::{Found, Proof, entry, optional};
use crate::win::fs::{LeafDirectory, check_leaf_path, kind_at, same_win_name};
use crate::win::install as os;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

pub(crate) const WORK: &str = ".tss-leaf-work";
type Result<T> = std::result::Result<T, RemoteError>;

trait Local<T> {
    fn at(self, path: &Path) -> Result<T>;
}

impl<T> Local<T> for io::Result<T> {
    fn at(self, path: &Path) -> Result<T> {
        self.map_err(|e| local(format!("{}: {e}", path.display())))
    }
}

/// Bound to the listed byte hash, not merely to the text comparison hash. Borrowing the immutable
/// bytes also prevents them from changing between comparison and staging.
pub struct PinnedLeaf<'a> {
    file: ManagedFile,
    commit: &'a str,
    bytes: &'a [u8],
}

impl<'a> PinnedLeaf<'a> {
    pub fn file(&self) -> &ManagedFile {
        &self.file
    }

    pub fn new(file: &SkillFile, commit: &'a str, bytes: &'a [u8]) -> Result<Self> {
        pathsafe::check_component(&file.path).map_err(|e| local(e.to_string()))?;
        if !is_sha1_hex(commit) || file.size > MAX_FILE_BYTES {
            return Err(local("a loose file needs a pinned commit and a bounded size"));
        }
        if file.size != bytes.len() as u64 || blob_hex(bytes) != file.blob {
            return Err(changed(file));
        }
        let file = ManagedFile { canon: Some(crate::text::canon(bytes)), ..ManagedFile::from(file) };
        Ok(Self { file, commit, bytes })
    }
}

/// `root` is today's configured root, NOT the recorded directory read from state. `other_claim`
/// can additionally reserve this physical target for another source in the engine's current plan.
#[derive(Clone, Copy)]
pub struct LeafTarget<'a> {
    pub source: &'a RepoConfig,
    pub root: &'a Path,
    pub binding: &'a DestinationBinding,
    pub other_claim: Option<&'a str>,
}

struct Layout {
    dir: PathBuf,
    work: PathBuf,
    live: PathBuf,
    name: String,
}

fn overlaps(a: &Path, b: &Path) -> bool {
    let key = |p: &Path| PathBuf::from(fold(&p.to_string_lossy().replace('/', "\\")));
    let (a, b) = (key(a), key(b));
    a.starts_with(&b) || b.starts_with(&a)
}

fn is_leaf_work(p: &Path) -> bool {
    p.file_name().and_then(|n| n.to_str()).is_some_and(|s| s.eq_ignore_ascii_case(WORK))
}

impl LeafTarget<'_> {
    fn layout(self, a: &mut Applier, state: &State, name: &str) -> Result<Layout> {
        crate::state::check_leaf_name(self.source.kind, name).map_err(local)?;
        self.binding.check_source(self.source).map_err(local)?;
        let bound = state.binding_for(self.source).map_err(local)?;
        if bound.is_some_and(|b| b != self.binding) {
            return Err(local("the source destination changed"));
        }
        let dir = self.binding.directory();
        let work = self.root.parent().ok_or_else(|| local("the loose destination needs a parent for staging"))?.join(WORK);
        let home = a.state_path.parent().ok_or_else(|| local("missing state directory"))?;
        if self.root.ancestors().any(is_leaf_work)
            || dir.ancestors().any(is_leaf_work)
            || [a.skills_dir.as_path(), &a.work_dir, home]
                .into_iter()
                .chain(state.skills_dir.iter().map(Path::new))
                .chain(state.set_aside.iter().map(|s| Path::new(&s.skills_dir)))
                .any(|p| overlaps(p, self.root) || overlaps(p, &work))
            || state.loose_sources().values().any(|r| overlaps(&work, &r.binding().directory()))
        {
            return Err(local("shared destinations, skills, settings and work directories must be separate"));
        }
        // Check the configured root against the immutable binding BEFORE probing even a directory.
        // Check the alternate layout by exact name, but don't treat a dirty *bound* leaf as
        // evidence for relocation. Its content goes to reconcile below.
        resolve_destination(self.source, self.root, &[], Some(self.binding), |p| entry(p, &[], &mut a.hasher)).map_err(local)?;
        let flat = fold(&dir.to_string_lossy()) == fold(&self.root.to_string_lossy());
        let alternate = if flat { self.root.join(self.source.owner_and_name().unwrap().1) } else { self.root.to_owned() };
        if entry(&alternate.join(name), &[], &mut a.hasher).map_err(local)? != Entry::Missing {
            return Err(local("a file also occupies the alternate destination"));
        }
        let live = dir.join(name);
        crate::tree::check_leaf_length(dir.to_string_lossy().encode_utf16().count(), name).map_err(local)?;
        Ok(Layout { dir, work, live, name: name.into() })
    }

    fn owner(self, state: &State, layout: &Layout, dir_id: Option<DiskId>) -> Option<String> {
        if let Some(other) = self.other_claim {
            return Some(other.into());
        }
        for (id, r) in state.loose_sources() {
            // Including pending FIRST installs (no installed version yet), disabled sources and
            // orphans. Compare physical directory identities as well as not-yet-created paths.
            if !(dir_id.is_some() && dir_id == r.directory_id) && !same_win_name(&layout.dir.to_string_lossy(), &r.binding().directory().to_string_lossy()) {
                continue;
            }
            if r.leaves().iter().any(|l| {
                let folded = fold(&l.name) == fold(&layout.name);
                (folded || same_win_name(&l.name, &layout.name)) && (id != &self.source.id || !folded)
            }) {
                return Some(id.clone());
            }
        }
        None
    }
}

struct Inspection {
    layout: Layout,
    dir: Option<LeafDirectory>,
    found: Found,
    verdict: Verdict,
}

pub(crate) struct ViewedLeaf {
    pub(crate) bytes: Option<Vec<u8>>,
    pub(crate) stamp: Option<LeafStamp>,
}

fn check_review(found: &Found, reviewed: Option<&Option<LeafStamp>>) -> Result<()> {
    let Some(reviewed) = reviewed else { return Ok(()) };
    match (found, reviewed) {
        (Found::Missing, None) => Ok(()),
        (Found::File(_, proof), Some(stamp)) if proof.id == stamp.id && proof.blob == stamp.blob => Ok(()),
        _ => Err(local(STALE_REVIEW)),
    }
}

#[derive(Default)]
struct LeafAuthority<'a> {
    replace: bool,
    reviewed: Option<&'a Option<LeafStamp>>,
}

impl Applier {
    /// Keep at most the display budget, but a review needs a byte proof even for a hidden file.
    pub(crate) fn read_leaf_view(&mut self, state: &State, target: LeafTarget, name: &str, limit: u64, review: bool) -> Result<Option<ViewedLeaf>> {
        let layout = target.layout(self, state, name)?;
        let Some(dir) = optional(LeafDirectory::pin(&layout.dir, false)).at(&layout.dir)? else { return Ok(None) };
        let id = dir.id().at(&layout.dir)?;
        if state.loose_sources().get(&target.source.id).and_then(|r| r.directory_id).is_some_and(|r| r != id) {
            return Err(local("the recorded destination directory changed"));
        }
        if let Some(owner) = target.owner(state, &layout, Some(id)) {
            return Err(local(format!("another source ({owner}) claims this file")));
        }
        let Some(f) = optional(crate::win::fs::open_leaf(&layout.live, false)).at(&layout.live)? else { return Ok(None) };
        if !review && f.metadata().at(&layout.live)?.len() > limit {
            return Ok(Some(ViewedLeaf { bytes: None, stamp: None }));
        }
        let read = self.hasher.read_leaf(&f).at(&layout.live)?;
        let stamp = if review { Some(LeafStamp { id: os::id(&f).at(&layout.live)?, blob: read.blob }) } else { None };
        Ok(Some(ViewedLeaf { bytes: read.bytes, stamp }))
    }

    /// Settings/skills may have been configured under another spelling. Check physical
    /// containment before any save or move; do not walk entries or probe dormant destinations.
    fn separate_leaf_dirs(&self, dir: &LeafDirectory, home: &LeafDirectory, work: Option<&LeafDirectory>) -> Result<()> {
        let refuse = |e: io::Error| local(format!("cannot separate the destination, work and settings folders: {e}"));
        dir.separate(home).map_err(refuse)?;
        if let Some(work) = work {
            dir.separate(work).and_then(|()| home.separate(work)).map_err(refuse)?;
        }
        for path in [&self.skills_dir, &self.work_dir] {
            if let Some(skills) = optional(LeafDirectory::pin(path, false)).at(path)? {
                dir.separate(&skills).map_err(refuse)?;
                if let Some(work) = work {
                    work.separate(&skills).map_err(refuse)?;
                }
            }
        }
        Ok(())
    }

    fn inspect_leaf(
        &mut self,
        state: &State,
        target: LeafTarget,
        listing: LeafListing,
        pin: Option<&PinnedLeaf>,
        create: bool,
        movable: bool,
    ) -> Result<Inspection> {
        if !listing.applies_to(target.source) {
            return Err(local("the listing is for another source"));
        }
        let source = state.loose_sources().get(&target.source.id);
        let leaf = source.and_then(|s| s.leaf(listing.name));
        let desired = listing.desired();
        if let Some(p) = pin {
            if !desired.is_some_and(|f| f.path == p.file.path && f.blob == p.file.blob && f.size == p.file.size) {
                return Err(local("the pinned file does not match this listing"));
            }
        }
        let name = leaf.map(|l| l.name.as_str()).or_else(|| desired.map(|f| f.path.as_str())).unwrap_or(listing.name);
        let layout = target.layout(self, state, name)?;
        let recorded_id = source.and_then(|s| s.directory_id);
        let dir = optional(LeafDirectory::pin(&layout.dir, create && recorded_id.is_none())).at(&layout.dir)?;
        let id = dir.as_ref().map(LeafDirectory::id).transpose().at(&layout.dir)?;
        if id.is_some() && recorded_id.is_some() && id != recorded_id {
            return Err(local("the loose destination is not the recorded directory"));
        }
        let owner = target.owner(state, &layout, id);
        let interrupted = leaf.is_some_and(OwnedLeaf::interrupted);
        let ignored = source.is_some_and(|s| s.poll.ignored.iter().any(|n| fold(n) == fold(name)));
        let found = if dir.is_some() && owner.is_none() && !interrupted && !ignored {
            Found::at(&layout.live, movable, &mut self.hasher).at(&layout.live)?
        } else {
            Found::Missing
        };
        let installed = leaf.and_then(|r| r.installed.as_ref());
        let matches_upstream = match &found {
            Found::File(_, p) => pin.is_some_and(|v| p.matches(&v.file)) || desired.is_some_and(|v| p.blob == v.blob),
            _ => false,
        };
        let facts = LeafFacts {
            owned: installed.map(|v| (&v.file, found.local(&v.file))),
            exists: !matches!(found, Found::Missing),
            matches_upstream,
            interrupted,
            ignored,
            other_owner: owner.as_deref(),
        };
        Ok(Inspection { verdict: plan_leaf(listing, facts), found, layout, dir })
    }

    /// Full read-only decision, also for dry run / unbound preview. None needs no download:
    /// supply pinned bytes to distinguish a text-equivalent local copy from an edit.
    pub fn leaf_decision(&mut self, state: &State, target: LeafTarget, listing: LeafListing, pin: Option<&PinnedLeaf>) -> Result<Verdict> {
        self.reviewed_leaf_decision(state, target, listing, pin, None)
    }

    pub(crate) fn reviewed_leaf_decision(
        &mut self,
        state: &State,
        target: LeafTarget,
        listing: LeafListing,
        pin: Option<&PinnedLeaf>,
        reviewed: Option<&Option<LeafStamp>>,
    ) -> Result<Verdict> {
        let inspection = self.inspect_leaf(state, target, listing, pin, false, false)?;
        check_review(&inspection.found, reviewed)?;
        Ok(inspection.verdict)
    }

    /// Run only the expected action, after repeating its decision and full proof. In particular
    /// asking to adopt a proven historical version cannot install it if that copy disappeared.
    pub fn apply_leaf(&mut self, state: &mut State, target: LeafTarget, listing: LeafListing, pin: Option<&PinnedLeaf>, expected: &Verdict) -> Result<Applied> {
        self.change_leaf(state, target, listing, pin, expected, LeafAuthority::default())
    }

    /// Explicit, single-file replace/restore. An existing file is kept under its journal's backup name.
    pub(crate) fn replace_leaf(
        &mut self,
        state: &mut State,
        target: LeafTarget,
        listing: LeafListing,
        pin: &PinnedLeaf,
        expected: &Verdict,
        reviewed: Option<&Option<LeafStamp>>,
    ) -> Result<Applied> {
        use crate::reconcile::Hold;
        let replace = matches!(
            expected,
            Verdict::Held(Hold::UpdateBlocked(_) | Hold::UpdateBlockedMissing | Hold::UnmanagedDiffers) | Verdict::ModifiedLocally(_) | Verdict::MissingLocally
        );
        self.change_leaf(state, target, listing, Some(pin), expected, LeafAuthority { replace, reviewed })
    }

    fn change_leaf(
        &mut self,
        state: &mut State,
        target: LeafTarget,
        listing: LeafListing,
        pin: Option<&PinnedLeaf>,
        expected: &Verdict,
        authority: LeafAuthority<'_>,
    ) -> Result<Applied> {
        if state.binding_for(target.source).map_err(local)? != Some(target.binding) {
            return Err(local("record the resolved destination before changing a loose file"));
        }
        let LeafAuthority { replace, reviewed } = authority;
        let verdict = self.reviewed_leaf_decision(state, target, listing, pin, reviewed)?;
        if (!replace && !expected.is_action()) || &verdict != expected {
            return Ok(Applied::Held(verdict.message()));
        }
        if pin.is_none() && *expected != Verdict::Delete {
            return Err(local("this file change needs verified bytes from a pinned commit"));
        }
        let movable = replace || matches!(expected, Verdict::Update | Verdict::Delete);
        let Inspection { layout, dir, found, verdict } = self.inspect_leaf(state, target, listing, pin, replace || *expected == Verdict::Install, movable)?;
        check_review(&found, reviewed)?;
        if &verdict != expected {
            return Ok(Applied::Held(verdict.message()));
        }
        let dir = dir.ok_or_else(|| local("the destination directory is missing"))?;
        let dir_id = dir.id().at(&layout.dir)?;
        let home = self.leaf_home()?;
        let next = pin.map(|p| LeafVersion { file: ManagedFile { path: layout.name.clone(), ..p.file.clone() }, commit: p.commit.into(), updated: self.now });
        if matches!(verdict, Verdict::Adopt | Verdict::CatchUp) {
            self.separate_leaf_dirs(&dir, &home, None)?;
            self.save_leaf(state, target, dir_id, &layout.name, next, None)?;
            return Ok(Applied::Done);
        }
        let work = self.leaf_work(&layout.work, &home)?;
        self.separate_leaf_dirs(&dir, &home, Some(&work))?;
        let work_id = work.id().at(&layout.work)?;
        if work_id[..8] != dir_id[..8] || work_id == dir_id {
            return Err(local("the work folder must be a separate folder on the destination drive"));
        }
        let txn = unique_suffix();
        let (stage, backup) = layout.work_files(&txn);
        for path in [&stage, &backup] {
            if kind_at(path).at(path)?.is_some() {
                return Err(local(format!("the transaction name {} is already in use", path.display())));
            }
        }
        let mut staged = if let Some(p) = pin { Some(self.stage_leaf(&stage, p)?) } else { None };
        self.at(Step::Staged)?;
        let after = next.zip(staged.as_ref()).map(|(v, s)| (s.id, v));
        let old = state.loose_sources()[&target.source.id].leaf(&layout.name).and_then(|l| l.installed.clone());
        let before = match &found {
            Found::File(_, proof) => Some(LeafStamp { id: proof.id, blob: proof.blob.clone() }),
            Found::Missing => None,
            Found::Other => return Err(local("the file is not a plain file")),
        };
        let pending = LeafTxn { txn, work: work_id, before, after, keep_backup: replace };
        self.save_leaf(state, target, dir_id, &layout.name, old, Some(pending))?;
        if let Some(s) = &mut staged {
            s.keep = true;
        }
        self.at(Step::Marked)?;
        if let Found::File(old, _) = found {
            os::rename(&old, &backup).at(&backup)?;
            drop(old);
            self.at(Step::OldMoved)?;
        }
        if let Some(s) = staged {
            if let Err(e) = os::rename(&s.file, &layout.live) {
                drop(s);
                // Recovery can only restore into an empty name; leave a late arrival in place.
                let result = self.recover_leaf(state, target, &layout.name)?;
                return Ok(match result {
                    Applied::Done => Applied::Held(format!("could not put the new file in place: {e}")),
                    other => other,
                });
            }
            drop(s);
            self.at(Step::NewInPlace)?;
        }
        self.recover_leaf(state, target, &layout.name)
    }

    fn leaf_home(&self) -> Result<LeafDirectory> {
        check_leaf_path(&self.state_path).at(&self.state_path)?;
        let home = self.state_path.parent().ok_or_else(|| local("missing state directory"))?;
        let pin = LeafDirectory::pin(home, true).at(home)?;
        // Do not save evidence through links or over aliases of other data.
        if kind_at(&self.state_path).at(&self.state_path)?.is_some() {
            crate::win::fs::open_leaf(&self.state_path, false).at(&self.state_path)?;
        }
        Ok(pin)
    }

    fn leaf_work(&self, path: &Path, home: &LeafDirectory) -> Result<LeafDirectory> {
        let parent = path.parent().ok_or_else(|| local("the work folder needs a parent"))?;
        let _parent = LeafDirectory::pin(parent, false).at(parent)?;
        let created = match std::fs::create_dir(path) {
            Ok(()) => true,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => false,
            Err(e) => return Err(e).at(path),
        };
        let work = LeafDirectory::pin(path, false).at(path)?;
        let owner = path.join(".owner");
        let expected = [home.id().at(path)?, work.id().at(path)?].concat();
        if created {
            let mut file = crate::win::fs::create_leaf(&owner).at(&owner)?;
            file.write_all(&expected).and_then(|()| file.sync_all()).at(&owner)?;
        } else {
            let mut bytes = Vec::new();
            let file = crate::win::fs::open_leaf(&owner, false).map_err(|e| local(format!("cannot verify who owns {}: {e}", path.display())))?;
            file.take(expected.len() as u64 + 1).read_to_end(&mut bytes).at(&owner)?;
            if bytes != expected {
                return Err(local(format!("{} belongs to a different settings or work folder", path.display())));
            }
        }
        Ok(work)
    }

    fn stage_leaf(&mut self, path: &Path, pin: &PinnedLeaf) -> Result<Stage> {
        let file = crate::win::fs::create_leaf(path).at(path)?;
        let mut stage = Stage { file, id: [0; 24], keep: false };
        stage.file.write_all(pin.bytes).and_then(|()| stage.file.sync_all()).at(path)?;
        let proof = Proof::read(&stage.file, &mut self.hasher).at(path)?;
        if proof.blob != pin.file.blob {
            return Err(local("the staged file differs from the verified download"));
        }
        stage.id = proof.id;
        Ok(stage)
    }

    fn save_leaf(
        &self,
        state: &mut State,
        target: LeafTarget,
        dir: DiskId,
        name: &str,
        installed: Option<LeafVersion>,
        pending: Option<LeafTxn>,
    ) -> Result<()> {
        // In-memory authority changes only after the durable record does.
        let mut next = state.clone();
        let source = next.bound_loose_mut(target.source).map_err(local)?;
        source.directory_id = Some(dir);
        let leaf = (installed.is_some() || pending.is_some()).then(|| OwnedLeaf { name: name.into(), installed, pending });
        source.set_leaf(name, leaf);
        self.save(&next)?;
        *state = next;
        Ok(())
    }
}

struct Stage {
    file: File,
    id: DiskId,
    keep: bool,
}

impl Drop for Stage {
    fn drop(&mut self) {
        if !self.keep {
            let _ = os::delete(&self.file);
        }
    }
}

impl Layout {
    fn work_files(&self, txn: &str) -> (PathBuf, PathBuf) {
        (self.work.join(format!("leaf-{txn}.new")), self.work.join(format!("backup-leaf-{txn}.old")))
    }
}

impl Applier {
    /// Recover exactly one journal under today's matching source and directory identities. A
    /// missing live name is never a reason to promote staging. A new occupant is never replaced.
    pub fn recover_leaf(&mut self, state: &mut State, target: LeafTarget, name: &str) -> Result<Applied> {
        let source = state.loose_sources().get(&target.source.id).ok_or_else(|| local("no bound source to recover"))?;
        source.validate().map_err(local)?;
        let leaf = source.leaf(name).ok_or_else(|| local("no recorded file to recover"))?;
        let txn = leaf.pending.clone().ok_or_else(|| local("no interrupted file change"))?;
        let prior = leaf.installed.clone();
        let layout = target.layout(self, state, &leaf.name)?;
        let dir = LeafDirectory::pin(&layout.dir, false).at(&layout.dir)?;
        let dir_id = dir.id().at(&layout.dir)?;
        let work = LeafDirectory::pin(&layout.work, false).at(&layout.work)?;
        if source.directory_id != Some(dir_id) || work.id().at(&layout.work)? != txn.work || dir_id[..8] != txn.work[..8] || dir_id == txn.work {
            return Err(local("the interrupted change belongs to other directories"));
        }
        if let Some(owner) = target.owner(state, &layout, Some(dir_id)) {
            return Ok(Applied::Held(format!("another source ({owner}) claims this file")));
        }
        let home = self.leaf_home()?;
        self.separate_leaf_dirs(&dir, &home, Some(&work))?;
        let (stage, backup) = layout.work_files(&txn.txn);
        let new_matches = |proof: &Proof| txn.after.as_ref().is_some_and(|(id, new)| proof.id == *id && proof.blob == new.file.blob);
        let held = || Applied::Held(format!("the interrupted change is not proven; the live file and work at {} were left alone", layout.work.display()));
        let staged = Found::at(&stage, true, &mut self.hasher).at(&stage)?;
        let staged = match staged {
            Found::Missing => None,
            Found::File(f, p) if new_matches(&p) => Some(f),
            _ => return Ok(held()),
        };
        let old = match Found::at(&backup, true, &mut self.hasher).at(&backup)? {
            Found::Missing => None,
            Found::File(f, p) if txn.before.as_ref().is_some_and(|b| b.id == p.id) => {
                let clean = txn.before.as_ref().is_some_and(|b| b.blob == p.blob) && (txn.keep_backup || prior.as_ref().is_some_and(|v| p.matches(&v.file)));
                Some((f, clean))
            }
            _ => return Ok(held()),
        };
        let live = Found::at(&layout.live, true, &mut self.hasher).at(&layout.live)?;
        let live_new = matches!(&live, Found::File(_, p) if new_matches(p));
        let live_missing = matches!(live, Found::Missing);

        // Either the original file was never moved, or it is in its verified work slot.
        // Never infer that a missing backup was already deleted: we save BEFORE disposing it.
        let complete = (if txn.after.is_some() { live_new } else { live_missing })
            && staged.is_none()
            && (if txn.before.is_some() { old.as_ref().is_some_and(|(_, clean)| *clean) } else { old.is_none() });
        if complete {
            self.save_leaf(state, target, dir_id, &layout.name, txn.after.map(|(_, new)| new), None)?;
            // Only the original, fully verified, still-pinned file may be retired. A crash
            // here can leave a backup, never a dangling journal authorizing its deletion.
            if let Some((old, _)) = old {
                if txn.keep_backup || os::delete(&old).is_err() {
                    return Ok(Applied::Displaced { paths: vec![layout.name], backup });
                }
            }
            self.at(Step::Settled)?;
            return Ok(Applied::Done);
        }

        let mut discard = staged;
        if let Some((old, _)) = old {
            if !live_missing {
                if !live_new || discard.is_some() {
                    return Ok(held()); // a late edit or another file now owns the live name
                }
                let Found::File(new, _) = live else { unreachable!() };
                os::rename(&new, &stage).at(&stage)?;
                discard = Some(new);
            }
            // Restore the original object, INCLUDING any late edit, and only into a free name.
            os::rename(&old, &layout.live).at(&layout.live)?;
        } else {
            let original = match (&txn.before, &live) {
                (Some(b), Found::File(_, p)) => b.id == p.id,
                (None, Found::Missing) => true,
                _ => false,
            };
            if !original {
                return Ok(held());
            }
        }
        self.save_leaf(state, target, dir_id, &layout.name, prior, None)?;
        if let Some(stage_file) = discard {
            os::delete(&stage_file).at(&stage)?;
        }
        Ok(Applied::Held("the interrupted change was undone; the original file was kept".into()))
    }
}

#[cfg(test)]
mod tests;
