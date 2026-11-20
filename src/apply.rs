//! Changing the skills folder, safely. This is the only module that moves or deletes anything in it.
//!
//! Every change to a skill the app owns follows one pattern:
//! 1. Get the new version ready beside the skills folder (`stage-<txn>`), every file proven byte-for-byte right.
//! 2. Prove again that the folder on disk is still what was installed (the same as [`crate::text`] says: line endings,
//!    the line break at the end and blank lines don't count).
//! 3. Write a note (the pending marker) in the state file saying what is about to happen.
//! 4. Swap with renames. A rename is atomic, so the skills folder never holds half a skill.
//! 5. Move every file the user added to the skill into the new folder, one by one, never replacing anything (a file in
//!    the way of one of GitHub's stays with the old folder). Then look at the old folder once more, now that nobody can
//!    reach it by its old name. If a late edit slipped in, put the user's files and the old folder back and hold the
//!    update.
//! 6. Write the final state and clear the marker.
//!
//! A removal follows the same pattern with an empty new version when the user added files to the skill, so that only
//! what GitHub put there goes.
//!
//! A crash between any two steps is understood by `recover` the next time the app starts: it finishes the change or
//! undoes it, and never loses a file.
//!
//! The rule behind all of it: **a folder is deleted only after it has been proven to hold exactly a version the app
//! installed and nothing else**, with only differences that don't count. Anything else is kept, as `backup-*` in the
//! work folder, and is never deleted automatically.

use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::fsx::{exists_no_follow, join_rel, move_no_replace_retry, remove_dir_all_retry, rename_retry, unique_suffix};
use crate::github::{FailureKind, RemoteError};
use crate::hash::BlobHasher;
use crate::interval::UnixSecs;
use crate::pathsafe::{self, fold};
use crate::reconcile::LocalState;
use crate::state::{ManagedFile, OwnedSkill, Pending, State};
use crate::tree::{Skill, SkillFile};
use crate::verify::{self, Check, Likeness, Spot, Spots, Stats};
use crate::win::fs::{FileHasher, Kind, kind_at, list_entries, volume_id};

const EMPTY_BLOB: &str = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";
const ERROR_DIR_NOT_EMPTY: i32 = 145;

/// Supplies one file's bytes to the sink, in order. The sink returns false to stop the transfer.
pub type Fetch<'a> = &'a mut dyn FnMut(&SkillFile, &mut dyn FnMut(&[u8]) -> bool) -> Result<(), RemoteError>;

/// The moments at which a change can be interrupted. Used by tests to stop dead, or to make something happen,
/// at exactly that point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// The new version is complete and verified in the work folder.
    Staged,
    /// The pending marker is saved.
    Marked,
    /// The old folder has been moved out of the skills folder.
    OldMoved,
    /// The new folder is in place.
    NewInPlace,
    /// One file the user added has been moved from the old folder into the new one.
    Carried,
    /// The old folder has been audited and removed (or kept as a backup); the final state is not yet saved.
    Settled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Only replace a folder that is verified untouched.
    Normal,
    /// The user chose upstream over their own edits. Whatever was there is kept as a backup.
    Overwrite,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Applied {
    Done,
    /// Done, but files the user had added were where the new version has files of its own (`paths`, sorted). They are
    /// kept in `backup`, with the rest of the replaced folder. Only an update in [`Mode::Normal`] says so: an overwrite
    /// keeps the whole folder it replaced, and its caller reports that backup.
    Displaced {
        paths: Vec<String>,
        backup: PathBuf,
    },
    /// Nothing the user owns was changed. The sentence says why, for the user.
    Held(String),
}

/// Tells the user that files they had added were where GitHub's new version has its own, and where they are now.
pub fn displaced_text(paths: &[String], backup: &Path) -> String {
    let list = match paths {
        [one] => one.clone(),
        [first @ .., last] if paths.len() <= 3 => format!("{} and {last}", first.join(", ")),
        [a, b, rest @ ..] => format!("{a}, {b} and {} more", rest.len()),
        [] => String::new(),
    };
    let (thing, were) = if paths.len() == 1 { ("the one", "was") } else { ("the ones", "were") };
    format!("GitHub's new version has its own {list}, so {thing} you had added {were} moved to {}", backup.display())
}

/// What recovery did about one interrupted change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    pub repo: String,
    pub skill: String,
    pub text: String,
}

type Hook = Box<dyn FnMut(Step) -> Result<(), RemoteError> + Send>;

pub struct Applier {
    pub skills_dir: PathBuf,
    pub work_dir: PathBuf,
    pub state_path: PathBuf,
    pub hasher: FileHasher,
    /// The time a change made now is recorded at.
    pub now: UnixSecs,
    /// Tests only. Returning an error models a crash at that step: the code stops with no cleanup at all.
    pub hook: Option<Hook>,
}

fn local(message: impl Into<String>) -> RemoteError {
    RemoteError::new(FailureKind::Local, message)
}

fn io_err(what: &str, e: &io::Error) -> RemoteError {
    local(format!("{what}: {e}"))
}

fn changed(file: &SkillFile) -> RemoteError {
    RemoteError::new(
        FailureKind::Changed,
        format!("{} did not match the listing (the repository probably changed while it was being downloaded); nothing was installed", file.path),
    )
}

fn check_names(skill: &Skill) -> Result<(), RemoteError> {
    let bad =
        |what: &str, why: pathsafe::PathError| RemoteError::new(FailureKind::Protocol, format!("'{what}' is not a safe name ({why}); nothing was installed"));
    pathsafe::check_component(&skill.name).map_err(|e| bad(&skill.name, e))?;
    if skill.files.is_empty() {
        return Err(RemoteError::new(FailureKind::Protocol, format!("skill '{}' has no files", skill.name)));
    }
    for f in &skill.files {
        pathsafe::check_rel_path(&f.path).map_err(|e| bad(&f.path, e))?;
    }
    Ok(())
}

/// Streams one file into the stage folder, hashing as it goes, and flushes it to disk.
fn write_staged(stage: &Path, file: &SkillFile, fetch: Fetch) -> Result<(), RemoteError> {
    let path = join_rel(stage, &file.path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| io_err(&format!("cannot create a folder for {}", file.path), &e))?;
    }
    let mut out = OpenOptions::new().write(true).create_new(true).open(&path).map_err(|e| io_err(&format!("cannot create {}", file.path), &e))?;
    let flush = |out: &fs::File| out.sync_all().map_err(|e| io_err(&format!("cannot save {}", file.path), &e));
    if file.size == 0 && file.blob == EMPTY_BLOB {
        return flush(&out);
    }
    let mut hasher = BlobHasher::new(file.size);
    let mut total = 0u64;
    let mut write_error: Option<io::Error> = None;
    let result = fetch(file, &mut |chunk: &[u8]| {
        total += chunk.len() as u64;
        if total > file.size {
            return false;
        }
        hasher.update(chunk);
        match out.write_all(chunk) {
            Ok(()) => true,
            Err(e) => {
                write_error = Some(e);
                false
            }
        }
    });
    if let Some(e) = write_error {
        return Err(io_err(&format!("cannot write {}", file.path), &e));
    }
    if total > file.size {
        return Err(changed(file));
    }
    result?;
    if total != file.size || hasher.finish_hex() != file.blob {
        return Err(changed(file));
    }
    flush(&out)
}

/// Checks that the skills folder and the work folder are on the same drive, because only then are renames between them atomic.
pub fn preflight(skills_dir: &Path, work_dir: &Path) -> Result<(), String> {
    for dir in [skills_dir, work_dir] {
        fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let skills = volume_id(skills_dir).map_err(|e| format!("cannot inspect {}: {e}", skills_dir.display()))?;
    let work = volume_id(work_dir).map_err(|e| format!("cannot inspect {}: {e}", work_dir.display()))?;
    if skills.serial != work.serial {
        return Err(format!(
            "{} is on a different drive than {} (the skills folder may be a link to another drive); the app needs both on the same drive to swap folders safely",
            skills_dir.display(),
            work_dir.display()
        ));
    }
    Ok(())
}

/// The folded paths of `files`, and of every folder on their way.
fn paths_and_folders(files: &[ManagedFile]) -> (HashSet<String>, HashSet<String>) {
    let mut paths = HashSet::new();
    let mut folders = HashSet::new();
    for f in files {
        let key = fold(&f.path);
        for (at, _) in key.match_indices('/') {
            folders.insert(key[..at].to_string());
        }
        paths.insert(key);
    }
    (paths, folders)
}

/// Whether `dir` is a plain folder that holds `files` and nothing else: no other file, link or empty folder.
fn holds_only(dir: &Path, files: &[ManagedFile]) -> bool {
    let (paths, _) = paths_and_folders(files);
    list_entries(dir).is_ok_and(|entries| entries.len() == paths.len() && entries.iter().all(|e| e.kind == Kind::File && paths.contains(&fold(&e.rel))))
}

/// Makes each folder on the way to `rel` below `root`, one at a time, so that nothing is ever made through a link.
/// False when something other than a plain folder is in the way.
fn make_way(root: &Path, rel: &str) -> io::Result<bool> {
    let mut at = root.to_path_buf();
    let Some((folders, _)) = rel.rsplit_once('/') else { return Ok(true) };
    for part in folders.split('/') {
        at.push(part);
        if kind_at(&at)?.is_none() {
            match fs::create_dir(&at) {
                Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
                _ => {}
            }
        }
        if kind_at(&at)? != Some(Kind::Dir) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Removes the folders on the way to `rel` below `root` that are now empty, deepest first. `root` itself stays.
fn prune(root: &Path, rel: &str) {
    let mut rest = rel;
    while let Some((parent, _)) = rest.rsplit_once('/') {
        if fs::remove_dir(join_rel(root, parent)).is_err() {
            return;
        }
        rest = parent;
    }
}

/// Moves one entry from below `from` to the same place below `to`, never replacing anything and never through a link.
/// False when it was not moved because something is in the way or it is gone.
fn move_entry(from: &Path, to: &Path, rel: &str) -> Result<bool, RemoteError> {
    let cannot = |e: &io::Error| io_err(&format!("cannot move {rel}"), e);
    if !make_way(to, rel).map_err(|e| cannot(&e))? {
        return Ok(false);
    }
    match move_no_replace_retry(&join_rel(from, rel), &join_rel(to, rel)) {
        Ok(()) => {
            prune(from, rel);
            Ok(true)
        }
        Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::AlreadyExists) => Ok(false),
        Err(e) => Err(cannot(&e)),
    }
}

enum Retired {
    Gone,
    Deleted,
    /// Kept as this backup.
    Kept(PathBuf),
}

impl Applier {
    pub fn new(skills_dir: PathBuf, work_dir: PathBuf, state_path: PathBuf, now: UnixSecs) -> Applier {
        Applier { skills_dir, work_dir, state_path, hasher: FileHasher::default(), now, hook: None }
    }

    fn at(&mut self, step: Step) -> Result<(), RemoteError> {
        match self.hook.as_mut() {
            Some(hook) => hook(step),
            None => Ok(()),
        }
    }

    fn save(&self, state: &State) -> Result<(), RemoteError> {
        state.save(&self.state_path).map_err(|e| io_err("could not save the list of managed skills", &e))
    }

    fn live(&self, name: &str) -> PathBuf {
        self.skills_dir.join(name)
    }

    fn stage_dir(&self, txn: &str) -> PathBuf {
        self.work_dir.join(format!("stage-{txn}"))
    }

    fn old_dir(&self, txn: &str) -> PathBuf {
        self.work_dir.join(format!("old-{txn}"))
    }

    fn trash_dir(&self, txn: &str) -> PathBuf {
        self.work_dir.join(format!("trash-{txn}"))
    }

    /// Where a new version goes while a change is being undone.
    fn new_dir(&self, txn: &str) -> PathBuf {
        self.work_dir.join(format!("new-{txn}"))
    }

    fn fresh_backup(&self, name: &str, txn: &str) -> PathBuf {
        let base = self.work_dir.join(format!("backup-{name}-{txn}"));
        let mut candidate = base.clone();
        let mut n = 1;
        while exists_no_follow(&candidate) {
            n += 1;
            candidate = self.work_dir.join(format!("backup-{name}-{txn}-{n}"));
        }
        candidate
    }

    fn ensure_dirs(&self) -> Result<(), RemoteError> {
        fs::create_dir_all(&self.skills_dir).map_err(|e| io_err("cannot create the skills folder", &e))?;
        fs::create_dir_all(&self.work_dir).map_err(|e| io_err("cannot create the work folder", &e))
    }

    fn discard_stage(&self, stage: &Path) {
        let _ = remove_dir_all_retry(stage);
    }

    /// Deletes a folder that has already been proven removable. If a program holds a file in it, it is renamed to
    /// `junk-*` so that a later sweep can finish the job.
    fn discard(&self, dir: &Path) {
        if remove_dir_all_retry(dir).is_err() && exists_no_follow(dir) {
            let _ = rename_retry(dir, &self.work_dir.join(format!("junk-{}", unique_suffix())));
        }
    }

    /// Whether every one of `files` in `dir` is as recorded. Anything else in the folder is not looked at.
    fn is_clean(&mut self, dir: &Path, files: &[ManagedFile]) -> bool {
        verify::verify_files(dir, files, Check::Full, &mut self.hasher).local == LocalState::Clean
    }

    /// Whether `dir` is exactly `files`, as recorded, with nothing else in it: only then may it be deleted.
    fn exactly(&mut self, dir: &Path, files: &[ManagedFile]) -> bool {
        holds_only(dir, files) && self.is_clean(dir, files)
    }

    /// Deletes `dir` only if it is exactly `files`. Anything else is kept as a backup and reported as `Kept`.
    fn retire(&mut self, dir: &Path, files: &[ManagedFile], name: &str, txn: &str) -> Result<Retired, RemoteError> {
        if !exists_no_follow(dir) {
            return Ok(Retired::Gone);
        }
        if self.exactly(dir, files) {
            self.discard(dir);
            return Ok(Retired::Deleted);
        }
        let keep = self.fresh_backup(name, txn);
        match rename_retry(dir, &keep) {
            Ok(()) => Ok(Retired::Kept(keep)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Retired::Gone),
            Err(e) => Err(io_err(&format!("cannot move {} aside", dir.display()), &e)),
        }
    }

    /// Puts the old folder back in place of a new one: the new tree is moved out of the skills folder first, so the
    /// old one can always go back, and is then deleted (or kept aside if it is not exactly what was downloaded).
    fn roll_back_swap(&mut self, live: &Path, old: &Path, new_files: &[ManagedFile], name: &str, txn: &str) -> Result<(), RemoteError> {
        let new = self.new_dir(txn);
        if exists_no_follow(live) {
            rename_retry(live, &new).map_err(|e| io_err(&format!("cannot move {} aside", live.display()), &e))?;
        }
        if let Err(e) = rename_retry(old, live) {
            let _ = rename_retry(&new, live);
            return Err(io_err(&format!("cannot put {} back", live.display()), &e));
        }
        self.retire(&new, new_files, name, txn)?;
        Ok(())
    }

    /// Moves everything the user added to the old folder (all that is not one of the `known` files GitHub put there)
    /// into the new one, one entry at a time and never replacing anything. What is in the way of a file of the new
    /// version stays behind, so that it is kept with the old folder; those paths are returned, sorted, so the user can
    /// be told.
    fn carry(&mut self, old: &Path, live: &Path, known: &[ManagedFile], new_files: &[ManagedFile]) -> Result<Vec<String>, RemoteError> {
        let cannot = |what: &Path, e: &io::Error| io_err(&format!("cannot look at {}", what.display()), e);
        match kind_at(old) {
            Ok(Some(Kind::Dir)) => {}
            Ok(_) => return Ok(Vec::new()),
            Err(e) => return Err(cannot(old, &e)),
        }
        match kind_at(live) {
            Ok(Some(Kind::Dir)) => {}
            Ok(_) => return Err(local(format!("{} is not a plain folder", live.display()))),
            Err(e) => return Err(cannot(live, &e)),
        }
        let entries = list_entries(old).map_err(|e| cannot(old, &e))?;
        let (known_paths, known_folders) = paths_and_folders(known);
        let (new_paths, new_folders) = paths_and_folders(new_files);
        let mut in_the_way = Vec::new();
        for entry in entries {
            let key = fold(&entry.rel);
            if known_paths.contains(&key) {
                continue;
            }
            if entry.kind == Kind::Dir {
                if new_folders.contains(&key) {
                    // The new version has this folder already; an empty copy of it is nothing to keep.
                    if fs::remove_dir(join_rel(old, &entry.rel)).is_ok() {
                        prune(old, &entry.rel);
                    }
                    continue;
                }
                if known_folders.contains(&key) {
                    continue;
                }
            }
            let below_a_new_file = key.match_indices('/').any(|(at, _)| new_paths.contains(&key[..at]));
            if new_paths.contains(&key) || new_folders.contains(&key) || below_a_new_file {
                in_the_way.push(entry.rel);
                continue;
            }
            if move_entry(old, live, &entry.rel)? {
                self.at(Step::Carried)?;
            }
        }
        in_the_way.sort_by_key(|rel| fold(rel));
        Ok(in_the_way)
    }

    /// The reverse of [`Applier::carry`], for a change that is undone: everything in the new folder that is not a file
    /// of the new version goes back to the old one.
    fn uncarry(&mut self, live: &Path, old: &Path, new_files: &[ManagedFile]) -> Result<(), RemoteError> {
        if kind_at(live).ok().flatten() != Some(Kind::Dir) || kind_at(old).ok().flatten() != Some(Kind::Dir) {
            return Ok(());
        }
        let entries = list_entries(live).map_err(|e| io_err(&format!("cannot look at {}", live.display()), &e))?;
        let (new_paths, new_folders) = paths_and_folders(new_files);
        for entry in entries {
            let key = fold(&entry.rel);
            if new_paths.contains(&key) || (entry.kind == Kind::Dir && new_folders.contains(&key)) {
                continue;
            }
            move_entry(live, old, &entry.rel)?;
        }
        Ok(())
    }

    /// Records the end of a change: the new version, or no record at all once a removal is done. A folder emptied by a
    /// removal goes too; one that still holds files the user added stays.
    fn finish(&mut self, state: &mut State, repo: &str, name: &str, commit: &str, new_files: &[ManagedFile]) {
        if new_files.is_empty() {
            state.repo_mut(repo).remove_skill(name);
            let live = self.live(name);
            if kind_at(&live).ok().flatten() == Some(Kind::Dir) {
                let _ = fs::remove_dir(&live);
            }
        } else {
            self.commit_new(state, repo, name, commit, new_files.to_vec());
        }
    }

    /// Records a finished install or update: fingerprints are taken now, at the final location.
    fn commit_new(&mut self, state: &mut State, repo: &str, name: &str, commit: &str, mut files: Vec<ManagedFile>) {
        verify::capture(&self.live(name), &mut files, &mut self.hasher);
        state.repo_mut(repo).upsert_skill(OwnedSkill { name: name.to_string(), commit: commit.to_string(), files, updated: Some(self.now), pending: None });
    }

    /// Undoes the pending marker in memory and on disk, after a change was abandoned before it touched anything.
    fn abandon(&mut self, state: &mut State, repo: &str, name: &str, before: &Option<OwnedSkill>, stage: Option<&Path>) {
        restore_record(state, repo, name, before);
        let _ = self.save(state);
        if let Some(stage) = stage {
            self.discard_stage(stage);
        }
    }

    /// Installs a new skill or updates an owned one. `fetch` supplies each file's bytes.
    #[allow(clippy::too_many_arguments)]
    pub fn install(
        &mut self,
        state: &mut State,
        repo_id: &str,
        skill: &Skill,
        commit: &str,
        mode: Mode,
        fetch: Fetch,
        cancel: &AtomicBool,
    ) -> Result<Applied, RemoteError> {
        check_names(skill)?;
        let before = state.repo(repo_id).and_then(|r| r.skill(&skill.name)).cloned();
        if before.as_ref().is_some_and(|o| o.pending.is_some()) {
            return Ok(Applied::Held("an earlier change to this skill was interrupted and is not sorted out yet".into()));
        }
        let live = self.live(&skill.name);
        if before.is_none() && mode == Mode::Normal && exists_no_follow(&live) {
            return Ok(Applied::Held("a folder with this name appeared; it was left alone".into()));
        }
        // What the user added moves along with the skill, so a folder that cannot be listed (a path too long, no access)
        // is not touched, and nothing is downloaded for it.
        if (before.is_some() || mode == Mode::Overwrite) && kind_at(&live).ok().flatten() == Some(Kind::Dir) {
            if let Err(e) = list_entries(&live) {
                return Ok(Applied::Held(format!("the skill folder could not be read ({e}); it was not replaced")));
            }
        }
        self.ensure_dirs()?;
        let txn = unique_suffix();
        let stage = self.stage_dir(&txn);

        let staged = fs::create_dir_all(&stage).map_err(|e| io_err("cannot create the stage folder", &e)).and_then(|()| {
            for file in &skill.files {
                if cancel.load(Ordering::Relaxed) {
                    return Err(RemoteError::new(FailureKind::Cancelled, "cancelled"));
                }
                write_staged(&stage, file, &mut *fetch)?;
            }
            Ok(())
        });
        if let Err(e) = staged {
            self.discard_stage(&stage);
            return Err(e);
        }
        self.at(Step::Staged)?;

        // The check that matters most: right before the swap, is every file GitHub put in the folder still as installed?
        if let (Mode::Normal, Some(old)) = (mode, &before) {
            match verify::verify_files(&live, &old.files, Check::Full, &mut self.hasher).local {
                LocalState::Clean => {}
                LocalState::Dirty(info) => {
                    self.discard_stage(&stage);
                    return Ok(Applied::Held(format!("edited locally ({info}); the update was held")));
                }
                LocalState::FolderMissing => {
                    self.discard_stage(&stage);
                    return Ok(Applied::Held("the folder was deleted locally; the update was held".into()));
                }
            }
        }

        if cancel.load(Ordering::Relaxed) {
            self.discard_stage(&stage);
            return Err(RemoteError::new(FailureKind::Cancelled, "cancelled"));
        }
        let new_files: Vec<ManagedFile> = skill.files.iter().map(ManagedFile::from).collect();
        self.swap_in(state, repo_id, &skill.name, &before, mode, &txn, commit, &new_files)
    }

    /// Puts the staged folder in place of the skill's folder (steps 3 to 6 of the module's pattern). The files the user
    /// added move into the new folder. An empty `new_files` removes the skill: what GitHub put there goes and only the
    /// user's files stay.
    #[allow(clippy::too_many_arguments)]
    fn swap_in(
        &mut self,
        state: &mut State,
        repo_id: &str,
        name: &str,
        before: &Option<OwnedSkill>,
        mode: Mode,
        txn: &str,
        commit: &str,
        new_files: &[ManagedFile],
    ) -> Result<Applied, RemoteError> {
        let removing = new_files.is_empty();
        let live = self.live(name);
        let stage = self.stage_dir(txn);
        let mut record =
            before.clone().unwrap_or_else(|| OwnedSkill { name: name.to_string(), commit: String::new(), files: Vec::new(), updated: None, pending: None });
        record.pending = Some(Pending::Update { txn: txn.to_string(), commit: commit.to_string(), files: new_files.to_vec() });
        state.repo_mut(repo_id).upsert_skill(record);
        if let Err(e) = self.save(state) {
            restore_record(state, repo_id, name, before);
            self.discard_stage(&stage);
            return Err(e);
        }
        self.at(Step::Marked)?;

        let old = self.old_dir(txn);
        let mut moved_old = false;
        if (before.is_some() || mode == Mode::Overwrite) && exists_no_follow(&live) {
            if let Err(e) = rename_retry(&live, &old) {
                self.abandon(state, repo_id, name, before, Some(&stage));
                let not_done = if removing { "removed" } else { "replaced" };
                return Ok(Applied::Held(format!("the skill folder is in use by another program, so it was not {not_done} ({e}); it will be tried again")));
            }
            moved_old = true;
        }
        self.at(Step::OldMoved)?;

        if let Err(e) = rename_retry(&stage, &live) {
            if moved_old {
                if let Err(e2) = rename_retry(&old, &live) {
                    return Err(local(format!(
                        "could not put the new version in place ({e}) and could not restore the old one ({e2}); the old version is in {} and will be restored the next time the app starts",
                        old.display()
                    )));
                }
            }
            self.abandon(state, repo_id, name, before, Some(&stage));
            let busy = e.kind() == io::ErrorKind::AlreadyExists || e.raw_os_error() == Some(ERROR_DIR_NOT_EMPTY);
            return if busy {
                Ok(Applied::Held("a folder with this name appeared; it was left alone".into()))
            } else {
                Err(io_err("could not put the new version in place", &e))
            };
        }
        self.at(Step::NewInPlace)?;

        let mut displaced = None;
        if moved_old {
            let known = before.as_ref().map_or(&[][..], |o| o.files.as_slice());
            let in_the_way = match self.carry(&old, &live, known, new_files) {
                Ok(paths) => paths,
                Err(e) => {
                    return Err(local(format!(
                        "could not move a file you added into the new version ({}); it is kept in {} and the change will be finished at the next check",
                        e.message,
                        old.display()
                    )));
                }
            };
            // Nobody can reach the old folder by its old name any more, so what is in it now is final.
            let only = holds_only(&old, known);
            let audit = verify::verify_files(&old, known, Check::Full, &mut self.hasher).local;
            let clean = audit == LocalState::Clean;
            if !clean && mode == Mode::Normal {
                self.uncarry(&live, &old, new_files)?;
                self.roll_back_swap(&live, &old, new_files, name, txn)?;
                restore_record(state, repo_id, name, before);
                self.save(state)?;
                let what = if let LocalState::Dirty(info) = audit { format!(" ({info})") } else { String::new() };
                return Ok(Applied::Held(if removing {
                    format!("edited locally while it was being removed{what}; your version was kept")
                } else {
                    format!("edited locally while the update was being made{what}; your version was kept and the update was held")
                }));
            }
            if only && clean {
                self.discard(&old);
            } else {
                let keep = self.fresh_backup(name, txn);
                rename_retry(&old, &keep).map_err(|e| io_err("cannot keep a backup of the replaced folder", &e))?;
                if !in_the_way.is_empty() && mode == Mode::Normal {
                    displaced = Some(Applied::Displaced { paths: in_the_way, backup: keep });
                }
            }
        }
        self.at(Step::Settled)?;

        self.finish(state, repo_id, name, commit, new_files);
        self.save(state)?;
        Ok(displaced.unwrap_or(Applied::Done))
    }

    /// Removes an owned skill that is gone upstream, after proving that what GitHub put there is untouched. Files the
    /// user added to the folder stay where they are.
    pub fn delete(&mut self, state: &mut State, repo_id: &str, name: &str) -> Result<Applied, RemoteError> {
        let Some(owned) = state.repo(repo_id).and_then(|r| r.skill(name)).cloned() else { return Ok(Applied::Done) };
        if owned.pending.is_some() {
            return Ok(Applied::Held("an earlier change to this skill was interrupted and is not sorted out yet".into()));
        }
        let live = self.live(&owned.name);
        match verify::verify_files(&live, &owned.files, Check::Full, &mut self.hasher).local {
            LocalState::Clean => {}
            LocalState::Dirty(info) => return Ok(Applied::Held(format!("edited locally ({info}); it was not removed"))),
            LocalState::FolderMissing => {
                self.forget(state, repo_id, &owned.name)?;
                return Ok(Applied::Done);
            }
        }
        let entries = match list_entries(&live) {
            Ok(entries) => entries,
            Err(e) => return Ok(Applied::Held(format!("the skill folder could not be read ({e}); it was not removed"))),
        };
        self.ensure_dirs()?;
        let txn = unique_suffix();
        let before = Some(owned.clone());
        let (known, _) = paths_and_folders(&owned.files);
        if entries.iter().any(|e| e.kind != Kind::File || !known.contains(&fold(&e.rel))) {
            // The user added something: swap in an empty folder, and move what they added into it.
            let stage = self.stage_dir(&txn);
            fs::create_dir_all(&stage).map_err(|e| io_err("cannot create the stage folder", &e))?;
            self.at(Step::Staged)?;
            return self.swap_in(state, repo_id, &owned.name, &before, Mode::Normal, &txn, &owned.commit, &[]);
        }

        let trash = self.trash_dir(&txn);
        state.repo_mut(repo_id).upsert_skill(OwnedSkill { pending: Some(Pending::Delete { txn: txn.clone() }), ..owned.clone() });
        if let Err(e) = self.save(state) {
            restore_record(state, repo_id, &owned.name, &before);
            return Err(e);
        }
        self.at(Step::Marked)?;

        if let Err(e) = rename_retry(&live, &trash) {
            self.abandon(state, repo_id, &owned.name, &before, None);
            return Ok(Applied::Held(format!("the skill folder is in use by another program, so it was not removed ({e}); it will be tried again")));
        }
        self.at(Step::OldMoved)?;

        if !self.exactly(&trash, &owned.files) {
            rename_retry(&trash, &live).map_err(|e| io_err(&format!("cannot put {} back", live.display()), &e))?;
            restore_record(state, repo_id, &owned.name, &before);
            self.save(state)?;
            return Ok(Applied::Held("edited locally while it was being removed; your version was kept".into()));
        }
        self.discard(&trash);
        self.at(Step::Settled)?;
        state.repo_mut(repo_id).remove_skill(&owned.name);
        self.save(state)?;
        Ok(Applied::Done)
    }

    /// Starts managing a folder that is already the same as upstream. Not one byte of it is written. `theirs` holds what
    /// the copies on GitHub of the files whose bytes differ are compared by, by path (see [`verify::compare_unmanaged`]).
    pub fn adopt(&mut self, state: &mut State, repo_id: &str, skill: &Skill, commit: &str, theirs: &HashMap<String, String>) -> Result<Applied, RemoteError> {
        check_names(skill)?;
        let live = self.live(&skill.name);
        if verify::compare_unmanaged(&live, skill, theirs, &mut self.hasher) != Likeness::Same {
            return Ok(Applied::Held("the existing folder changed while it was being checked; it was left alone".into()));
        }
        let mut files: Vec<ManagedFile> = skill.files.iter().map(|f| ManagedFile { canon: theirs.get(&f.path).cloned(), ..ManagedFile::from(f) }).collect();
        verify::capture(&live, &mut files, &mut self.hasher);
        if files.iter().any(|f| f.fp.is_none()) {
            return Ok(Applied::Held("the existing folder changed while it was being checked; it was left alone".into()));
        }
        state.repo_mut(repo_id).upsert_skill(OwnedSkill {
            name: skill.name.clone(),
            commit: commit.to_string(),
            files,
            updated: Some(self.now),
            pending: None,
        });
        self.save(state)?;
        Ok(Applied::Done)
    }

    /// Brings the record of a skill we own up to date with its folder, which was edited here and is now what upstream
    /// has, file for file. The same as [`Applier::adopt`] for a folder nobody owns: not one byte of the folder is
    /// written, and it is looked at again first. `theirs` holds what the copies on GitHub of the files whose bytes
    /// differ are compared by, by path. The record already says it for the files upstream did not change.
    pub fn catch_up(
        &mut self,
        state: &mut State,
        repo_id: &str,
        skill: &Skill,
        commit: &str,
        theirs: &HashMap<String, String>,
    ) -> Result<Applied, RemoteError> {
        check_names(skill)?;
        let Some(before) = state.repo(repo_id).and_then(|r| r.skill(&skill.name)).cloned() else {
            return Ok(Applied::Held("it is no longer recorded as installed; it was left alone".into()));
        };
        if before.pending.is_some() {
            return Ok(Applied::Held("an earlier change to it is not sorted out yet; it was left alone".into()));
        }
        let mut known = verify::known_canons(skill, &before.files);
        known.extend(theirs.iter().map(|(path, canon)| (path.clone(), canon.clone())));
        let live = self.live(&skill.name);
        let changed = || Applied::Held("the folder changed while it was being checked; it was left alone".to_string());
        if verify::compare_owned(&live, skill, &before.files, &known, &mut self.hasher) != Likeness::Same {
            return Ok(changed());
        }
        let mut files: Vec<ManagedFile> = skill.files.iter().map(|f| ManagedFile { canon: known.get(&f.path).cloned(), ..ManagedFile::from(f) }).collect();
        verify::capture(&live, &mut files, &mut self.hasher);
        if files.iter().any(|f| f.fp.is_none()) {
            return Ok(changed());
        }
        state.repo_mut(repo_id).upsert_skill(OwnedSkill {
            name: skill.name.clone(),
            commit: commit.to_string(),
            files,
            updated: Some(self.now),
            pending: None,
        });
        self.save(state)?;
        Ok(Applied::Done)
    }

    /// Drops the ownership record. No file is touched.
    pub fn forget(&mut self, state: &mut State, repo_id: &str, name: &str) -> Result<(), RemoteError> {
        state.repo_mut(repo_id).remove_skill(name);
        self.save(state)
    }

    /// Finishes or undoes every change that a crash interrupted. Run once at startup, before any sync.
    pub fn recover(&mut self, state: &mut State) -> Result<Vec<Note>, RemoteError> {
        let todo: Vec<(String, String)> =
            state.repos.iter().flat_map(|(id, r)| r.skills.iter().filter(|s| s.pending.is_some()).map(move |s| (id.clone(), s.name.clone()))).collect();
        let mut notes = Vec::new();
        for (repo, name) in todo {
            let Some(owned) = state.repo(&repo).and_then(|r| r.skill(&name)).cloned() else { continue };
            let text = match owned.pending.clone() {
                Some(Pending::Update { txn, commit, files }) => self.recover_update(state, &repo, &owned, &txn, &commit, files),
                Some(Pending::Delete { txn }) => self.recover_delete(state, &repo, &owned, &txn),
                None => continue,
            };
            notes.push(Note { repo, skill: name, text });
        }
        if !notes.is_empty() {
            self.save(state)?;
        }
        Ok(notes)
    }

    fn recover_update(&mut self, state: &mut State, repo: &str, owned: &OwnedSkill, txn: &str, commit: &str, new_files: Vec<ManagedFile>) -> String {
        let live = self.live(&owned.name);
        let old = self.old_dir(txn);
        self.discard_stage(&self.stage_dir(txn));
        let known_before = !owned.files.is_empty();
        // An empty list of new files is a removal that keeps the files the user added.
        let removing = new_files.is_empty();
        let what = if removing { "a removal" } else { "an update" };

        let text = match (exists_no_follow(&old), exists_no_follow(&live)) {
            (true, true) => {
                // A folder put back by hand stays; a link there is never followed, so the change is undone.
                let plain = kind_at(&live).ok().flatten() == Some(Kind::Dir);
                if known_before && plain && self.is_clean(&old, &owned.files) {
                    let done = self
                        .carry(&old, &live, &owned.files, &new_files)
                        .and_then(|in_the_way| Ok((in_the_way, self.retire(&old, &owned.files, &owned.name, txn)?)));
                    match done {
                        Ok((in_the_way, retired)) => {
                            self.finish(state, repo, &owned.name, commit, &new_files);
                            match retired {
                                Retired::Kept(backup) if !in_the_way.is_empty() => {
                                    format!("finished {what} that was interrupted; {}", displaced_text(&in_the_way, &backup))
                                }
                                _ => format!("finished {what} that was interrupted"),
                            }
                        }
                        Err(e) => format!("could not finish {what} that was interrupted yet ({}); it will be tried again at the next check", e.message),
                    }
                } else {
                    match self.uncarry(&live, &old, &new_files).and_then(|()| self.roll_back_swap(&live, &old, &new_files, &owned.name, txn)) {
                        Ok(()) => {
                            settle(state, repo, owned);
                            format!("undid {what} that was interrupted, because the folder had local changes")
                        }
                        Err(e) => format!("could not undo {what} that was interrupted yet ({}); it will be tried again at the next start", e.message),
                    }
                }
            }
            (true, false) => match rename_retry(&old, &live) {
                Ok(()) => {
                    settle(state, repo, owned);
                    if removing {
                        "undid a removal that was interrupted; the skill is back".to_string()
                    } else {
                        "undid an update that was interrupted; the previous version is back".to_string()
                    }
                }
                Err(e) => format!("could not restore the previous version yet ({e}); it will be tried again at the next start"),
            },
            (false, true) => {
                // A file that only the old version had must be gone, or nothing proves the change was made: an empty
                // list of new files is met by any folder.
                let (new_paths, _) = paths_and_folders(&new_files);
                let mut spots = Spots::new(&live);
                let mut stats = Stats::default();
                let left_over =
                    owned.files.iter().filter(|f| !new_paths.contains(&fold(&f.path))).any(|f| !matches!(spots.find(&f.path, &mut stats), Spot::Missing));
                if !left_over && self.is_clean(&live, &new_files) {
                    self.finish(state, repo, &owned.name, commit, &new_files);
                    format!("finished {what} that was interrupted")
                } else if known_before && self.is_clean(&live, &owned.files) {
                    settle(state, repo, owned);
                    format!("{what} was interrupted before it changed anything")
                } else {
                    settle(state, repo, owned);
                    format!("{what} was interrupted and the folder does not match any known version; it was left as it is")
                }
            }
            (false, false) if removing => {
                state.repo_mut(repo).remove_skill(&owned.name);
                "a removal was interrupted and had already finished".to_string()
            }
            (false, false) => {
                settle(state, repo, owned);
                "an update was interrupted and the folder is gone".to_string()
            }
        };
        // An undo that was itself interrupted can leave the new version aside.
        let new = self.new_dir(txn);
        if exists_no_follow(&new) {
            let _ = self.retire(&new, &new_files, &owned.name, txn);
        }
        text
    }

    fn recover_delete(&mut self, state: &mut State, repo: &str, owned: &OwnedSkill, txn: &str) -> String {
        let live = self.live(&owned.name);
        let trash = self.trash_dir(txn);
        match (exists_no_follow(&live), exists_no_follow(&trash)) {
            (true, true) => {
                let keep = self.fresh_backup(&owned.name, txn);
                match rename_retry(&trash, &keep) {
                    Ok(()) => {
                        settle(state, repo, owned);
                        format!("a removal was interrupted and the folder is back; the old copy was kept in {}", keep.display())
                    }
                    Err(e) => format!("could not sort out an interrupted removal yet ({e}); it will be tried again at the next start"),
                }
            }
            (true, false) => {
                settle(state, repo, owned);
                "a removal was interrupted before it changed anything".to_string()
            }
            (false, true) => {
                if self.exactly(&trash, &owned.files) {
                    self.discard(&trash);
                    state.repo_mut(repo).remove_skill(&owned.name);
                    "finished a removal that was interrupted".to_string()
                } else {
                    match rename_retry(&trash, &live) {
                        Ok(()) => {
                            settle(state, repo, owned);
                            "undid a removal that was interrupted, because the folder had local changes".to_string()
                        }
                        Err(e) => format!("could not restore a folder yet ({e}); it will be tried again at the next start"),
                    }
                }
            }
            (false, false) => {
                state.repo_mut(repo).remove_skill(&owned.name);
                "a removal was interrupted and had already finished".to_string()
            }
        }
    }

    /// Deletes leftovers that can never hold anyone's data: half-downloaded stages, and folders already proven
    /// removable. Does nothing while any change is unsettled, including one in a skills folder used before, which may
    /// share this work folder.
    pub fn sweep(&self, state: &State) {
        if state.has_pending() || state.has_pending_set_aside() {
            return;
        }
        let Ok(entries) = fs::read_dir(&self.work_dir) else { return };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("stage-") || name.starts_with("junk-") {
                let _ = remove_dir_all_retry(&entry.path());
            }
        }
    }
}

fn restore_record(state: &mut State, repo: &str, name: &str, before: &Option<OwnedSkill>) {
    let repo = state.repo_mut(repo);
    match before {
        Some(o) => repo.upsert_skill(o.clone()),
        None => repo.remove_skill(name),
    }
}

/// Clears the marker and keeps the record with its old file list, so the verifier reports whatever is on disk
/// honestly. A record that never had files is dropped: the folder then counts as one nobody owns.
fn settle(state: &mut State, repo: &str, owned: &OwnedSkill) {
    let repo = state.repo_mut(repo);
    if owned.files.is_empty() {
        repo.remove_skill(&owned.name);
    } else {
        repo.upsert_skill(OwnedSkill { pending: None, ..owned.clone() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::blob_hex;
    use crate::verify::verify_skill;
    use crate::win::fs::{Kind, list_entries, list_files, probe};
    use std::collections::BTreeMap;
    use std::os::windows::fs::OpenOptionsExt;
    use std::sync::{Arc, Mutex};

    const C1: &str = "1111111111111111111111111111111111111111";
    const C2: &str = "2222222222222222222222222222222222222222";
    const REPO: &str = "repo";
    const V1: &[(&str, &str)] = &[("SKILL.md", "# alpha v1\n"), ("scripts/run.ps1", "echo one\n"), ("references/a.md", "ref a\n")];
    const V2: &[(&str, &str)] = &[("SKILL.md", "# alpha v2\n"), ("scripts/run.ps1", "echo two, longer\n"), ("references/b.md", "ref b\n")];
    const EDITED: &str = "# alpha vX\n";
    /// The time the applier of `Env::restart` records changes at.
    const T0: UnixSecs = 1_700_000_000;

    type Files<'a> = &'a [(&'a str, &'a str)];

    struct Env {
        root: PathBuf,
        skills: PathBuf,
        work: PathBuf,
        state: PathBuf,
    }

    impl Env {
        fn new(name: &str) -> Env {
            let root = std::env::temp_dir().join(format!("tss-apply-{name}-{}", unique_suffix()));
            fs::create_dir_all(&root).unwrap();
            Env { skills: root.join("skills"), work: root.join(".tss-work"), state: root.join("home").join("state.json"), root }
        }

        /// A fresh applier and the state as saved on disk: what the app sees after a restart.
        fn restart(&self) -> (Applier, State) {
            (Applier::new(self.skills.clone(), self.work.clone(), self.state.clone(), T0), State::load(&self.state).unwrap().state)
        }

        fn live(&self, name: &str) -> PathBuf {
            self.skills.join(name)
        }

        fn work_entries(&self) -> Vec<String> {
            let mut names: Vec<String> =
                fs::read_dir(&self.work).map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
            names.sort();
            names
        }

        fn work_dirs_starting(&self, prefix: &str) -> Vec<PathBuf> {
            self.work_entries().into_iter().filter(|n| n.starts_with(prefix)).map(|n| self.work.join(n)).collect()
        }
    }

    impl Drop for Env {
        fn drop(&mut self) {
            let _ = remove_dir_all_retry(&self.root);
        }
    }

    fn skill(name: &str, files: Files) -> Skill {
        Skill {
            name: name.into(),
            files: files.iter().map(|(p, c)| SkillFile { path: (*p).into(), blob: blob_hex(c.as_bytes()), size: c.len() as u64 }).collect(),
        }
    }

    type Fetcher<'a> = Box<dyn FnMut(&SkillFile, &mut dyn FnMut(&[u8]) -> bool) -> Result<(), RemoteError> + 'a>;

    /// Serves files from memory in two pieces, so that streaming is exercised.
    fn serve<'a>(files: Files<'a>) -> Fetcher<'a> {
        Box::new(move |file, sink| {
            let content = files
                .iter()
                .find(|(p, _)| *p == file.path)
                .map(|(_, c)| c.as_bytes())
                .ok_or_else(|| RemoteError::new(FailureKind::NotFound, "no such file"))?;
            let mid = content.len() / 2;
            for part in [&content[..mid], &content[mid..]] {
                if !part.is_empty() && !sink(part) {
                    return Err(RemoteError::new(FailureKind::Cancelled, "stopped"));
                }
            }
            Ok(())
        })
    }

    fn install(a: &mut Applier, state: &mut State, name: &str, files: Files, commit: &str, mode: Mode) -> Result<Applied, RemoteError> {
        let mut fetch = serve(files);
        a.install(state, REPO, &skill(name, files), commit, mode, &mut fetch, &AtomicBool::new(false))
    }

    fn tree_of(dir: &Path) -> BTreeMap<String, String> {
        list_files(dir).unwrap().into_iter().map(|f| (f.rel.clone(), fs::read_to_string(join_rel(dir, &f.rel)).unwrap())).collect()
    }

    fn expected(files: Files) -> BTreeMap<String, String> {
        files.iter().map(|(p, c)| (p.to_string(), c.to_string())).collect()
    }

    const DIR: &str = "<folder>";
    const LINK: &str = "<link>";
    /// Files the user added to a skill: at the top, deep inside a folder the skill has, and in a folder of their own.
    const MINE: Files = &[("notes.txt", "my notes"), ("references/deep/mine.md", "deep"), ("mine/x.md", "x")];

    /// Everything in a folder, looked at without following a link: each file's text, `DIR` for an empty folder and
    /// `LINK` for a link.
    fn entries_of(dir: &Path) -> BTreeMap<String, String> {
        let what = |f: &crate::win::fs::Found| match f.kind {
            Kind::File => fs::read_to_string(join_rel(dir, &f.rel)).unwrap(),
            Kind::Dir => DIR.to_string(),
            Kind::Link => LINK.to_string(),
        };
        list_entries(dir).unwrap().iter().map(|f| (f.rel.clone(), what(f))).collect()
    }

    /// `files` and `more` together.
    fn with(files: Files, more: Files) -> BTreeMap<String, String> {
        let mut all = expected(files);
        all.extend(expected(more));
        all
    }

    fn add(dir: &Path, files: Files) {
        write_files(dir, &files.iter().map(|(p, c)| (p.to_string(), c.to_string())).collect::<Vec<_>>());
    }

    fn junction(link: &Path, target: &Path) {
        let made = std::process::Command::new("cmd").args(["/C", "mklink", "/J"]).arg(link).arg(target).output().unwrap().status.success();
        assert!(made, "creating a junction needs no privileges");
    }

    fn work_dir_starting(work: &Path, prefix: &str) -> PathBuf {
        fs::read_dir(work).unwrap().flatten().find(|e| e.file_name().to_string_lossy().starts_with(prefix)).unwrap().path()
    }

    /// Each file the user added is in the skill's folder, as it was, and nowhere else: not in a backup, not left in the
    /// work folder.
    fn mine_is_only_in_the_skill(env: &Env, label: &str) {
        let mut homes = vec![env.live("alpha")];
        homes.extend(env.work_entries().into_iter().map(|n| env.work.join(n)));
        for (rel, content) in MINE {
            let copies: Vec<&PathBuf> = homes.iter().filter(|h| fs::read_to_string(join_rel(h, rel)).is_ok_and(|t| t == *content)).collect();
            assert_eq!(copies, [&env.live("alpha")], "{label}: {rel}");
        }
    }

    fn crash_at(a: &mut Applier, step: Step) {
        a.hook = Some(Box::new(move |s| if s == step { Err(local(format!("simulated crash after {s:?}"))) } else { Ok(()) }));
    }

    fn record<'a>(state: &'a State, name: &str) -> &'a OwnedSkill {
        state.repo(REPO).unwrap().skill(name).unwrap()
    }

    fn local_state(a: &mut Applier, env: &Env, state: &State, name: &str) -> LocalState {
        verify_skill(&env.live(name), record(state, name), Check::Full, &mut a.hasher).local
    }

    fn installed_v1(env: &Env) -> (Applier, State) {
        let (mut a, mut state) = env.restart();
        assert_eq!(install(&mut a, &mut state, "alpha", V1, C1, Mode::Normal).unwrap(), Applied::Done);
        (a, state)
    }

    fn edit_skill_file(env: &Env) {
        fs::write(env.live("alpha").join("SKILL.md"), EDITED).unwrap();
    }

    fn held_reason(r: Result<Applied, RemoteError>) -> String {
        match r.unwrap() {
            Applied::Held(why) => why,
            done => panic!("expected the change to be held: {done:?}"),
        }
    }

    /// Does any copy of the user's edited SKILL.md still exist, in the skills folder or kept aside?
    fn edited_copy_survives(env: &Env) -> bool {
        let mut homes = vec![env.live("alpha")];
        homes.extend(env.work_dirs_starting("backup-"));
        homes.extend(env.work_dirs_starting("old-"));
        homes.extend(env.work_dirs_starting("trash-"));
        homes.iter().any(|d| fs::read_to_string(d.join("SKILL.md")).map(|t| t == EDITED).unwrap_or(false))
    }

    #[test]
    fn a_new_skill_is_installed_with_verified_files_and_fingerprints() {
        let env = Env::new("fresh");
        let (mut a, state) = installed_v1(&env);
        assert_eq!(tree_of(&env.live("alpha")), expected(V1));
        let rec = record(&state, "alpha");
        assert_eq!(rec.commit, C1);
        assert!(rec.pending.is_none());
        assert!(rec.files.iter().all(|f| f.fp.is_some()), "every installed file has a fingerprint for later quick checks");
        assert!(env.work_entries().is_empty(), "nothing is left in the work folder: {:?}", env.work_entries());
        assert_eq!(State::load(&env.state).unwrap().state, state, "the state on disk is the state in memory");
        assert_eq!(local_state(&mut a, &env, &state, "alpha"), LocalState::Clean);
    }

    #[test]
    fn an_update_replaces_the_folder_and_leaves_nothing_behind() {
        let env = Env::new("update");
        let (mut a, mut state) = installed_v1(&env);
        assert_eq!(install(&mut a, &mut state, "alpha", V2, C2, Mode::Normal).unwrap(), Applied::Done);
        assert_eq!(tree_of(&env.live("alpha")), expected(V2), "old files are gone, new files are there");
        assert_eq!(record(&state, "alpha").commit, C2);
        assert!(env.work_entries().is_empty(), "{:?}", env.work_entries());
        assert_eq!(local_state(&mut a, &env, &state, "alpha"), LocalState::Clean);
    }

    #[test]
    fn empty_files_are_created_without_asking_the_network() {
        let env = Env::new("empty-file");
        let (mut a, mut state) = env.restart();
        let files: Files = &[("SKILL.md", "# x\n"), ("references/__init__.md", "")];
        let mut asked = Vec::new();
        let mut fetch = |file: &SkillFile, sink: &mut dyn FnMut(&[u8]) -> bool| {
            asked.push(file.path.clone());
            serve(files)(file, sink)
        };
        let r = a.install(&mut state, REPO, &skill("alpha", files), C1, Mode::Normal, &mut fetch, &AtomicBool::new(false)).unwrap();
        assert_eq!(r, Applied::Done);
        assert_eq!(asked, vec!["SKILL.md".to_string()]);
        assert_eq!(tree_of(&env.live("alpha")), expected(files));
    }

    #[test]
    fn local_edits_hold_an_update_and_nothing_changes() {
        let env = Env::new("hold");
        let (mut a, mut state) = installed_v1(&env);
        let steps = Arc::new(Mutex::new(Vec::new()));
        let log = steps.clone();
        a.hook = Some(Box::new(move |s| {
            log.lock().unwrap().push(s);
            Ok(())
        }));
        edit_skill_file(&env);
        let held_early = |a: &mut Applier, state: &mut State| {
            steps.lock().unwrap().clear();
            let why = held_reason(install(a, state, "alpha", V2, C2, Mode::Normal));
            let seen = steps.lock().unwrap().clone();
            assert!(seen.iter().all(|s| *s == Step::Staged), "it must be held before a marker is written or a folder is moved: {seen:?} ({why})");
            why
        };
        let why = held_early(&mut a, &mut state);
        assert!(why.contains("edited locally") && why.contains("SKILL.md"), "{why}");
        assert_eq!(fs::read_to_string(env.live("alpha").join("SKILL.md")).unwrap(), EDITED);
        assert_eq!(record(&state, "alpha").commit, C1);
        assert!(record(&state, "alpha").pending.is_none());
        assert!(env.work_entries().is_empty(), "the downloaded copy was removed again: {:?}", env.work_entries());
        assert_eq!(State::load(&env.state).unwrap().state, state);

        // Every other kind of local deviation holds the update the same way.
        fs::write(env.live("alpha").join("SKILL.md"), V1[0].1).unwrap();
        fs::remove_file(env.live("alpha").join("references").join("a.md")).unwrap();
        assert!(held_early(&mut a, &mut state).contains("missing: references/a.md"));
        remove_dir_all_retry(&env.live("alpha")).unwrap();
        assert!(held_early(&mut a, &mut state).contains("deleted locally"));
        assert!(!env.live("alpha").exists(), "a skill the user deleted is not brought back silently");
    }

    #[test]
    fn files_the_user_added_move_into_the_new_version() {
        let env = Env::new("carry");
        let (mut a, mut state) = installed_v1(&env);
        let live = env.live("alpha");
        let outside = env.root.join("outside");
        add(&outside, &[("theirs.md", "not in the skill")]);
        add(&live, MINE);
        fs::create_dir_all(live.join("drafts")).unwrap();
        junction(&live.join("linked"), &outside);

        assert_eq!(install(&mut a, &mut state, "alpha", V2, C2, Mode::Normal).unwrap(), Applied::Done);

        let mut want = with(V2, MINE);
        want.insert("drafts".into(), DIR.into());
        want.insert("linked".into(), LINK.into());
        assert_eq!(entries_of(&live), want, "GitHub's files are the new version and the user's are where they were");
        assert_eq!(entries_of(&outside), expected(&[("theirs.md", "not in the skill")]), "the link was moved, not what it points to");
        let recorded: Vec<&str> = record(&state, "alpha").files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(recorded, V2.iter().map(|(p, _)| *p).collect::<Vec<_>>(), "only GitHub's files are recorded");
        assert_eq!(local_state(&mut a, &env, &state, "alpha"), LocalState::Clean);
        assert!(env.work_entries().is_empty(), "the old version was exactly V1 once the user's files were out: {:?}", env.work_entries());
    }

    #[test]
    fn a_file_the_user_added_where_github_now_has_one_is_kept_as_a_backup() {
        let env = Env::new("carry-clash");
        let (mut a, mut state) = installed_v1(&env);
        let live = env.live("alpha");
        const NEW: Files = &[("SKILL.md", "# alpha v3\n"), ("tools/run.md", "run\n"), ("notes", "github's notes\n"), ("references/b.md", "ref b\n")];
        // The same path as a new file, a file where a folder is needed, and a file below a path that is now a file.
        const CLASHING: Files = &[("references/b.md", "my b"), ("tools", "my tools file"), ("notes/x.md", "my x")];
        add(&live, CLASHING);
        add(&live, &[("keep.md", "mine")]);

        let applied = install(&mut a, &mut state, "alpha", NEW, C2, Mode::Normal).unwrap();

        assert_eq!(entries_of(&live), with(NEW, &[("keep.md", "mine")]), "GitHub's files win and the rest of the user's stay");
        let backups = env.work_dirs_starting("backup-alpha-");
        assert_eq!(backups.len(), 1, "{:?}", env.work_entries());
        assert_eq!(env.work_entries().len(), 1, "{:?}", env.work_entries());
        let paths = ["notes/x.md", "references/b.md", "tools"].map(String::from).to_vec();
        assert_eq!(applied, Applied::Displaced { paths, backup: backups[0].clone() }, "the user is told what had to go, and where it is");
        let kept = entries_of(&backups[0]);
        for (rel, content) in CLASHING {
            assert_eq!(kept.get(*rel).map(String::as_str), Some(*content), "{rel}: {kept:?}");
        }
        assert!(!kept.contains_key("keep.md"), "{kept:?}");
        assert_eq!(local_state(&mut a, &env, &state, "alpha"), LocalState::Clean);
    }

    #[test]
    fn a_folder_that_appears_while_a_new_skill_is_being_prepared_is_left_alone() {
        let env = Env::new("appears");
        let (mut a, mut state) = env.restart();
        let live = env.live("alpha");
        a.hook = Some(Box::new(move |step| {
            if step == Step::Marked {
                fs::create_dir_all(&live).unwrap();
                fs::write(live.join("SKILL.md"), "mine").unwrap();
            }
            Ok(())
        }));
        let why = held_reason(install(&mut a, &mut state, "alpha", V1, C1, Mode::Normal));
        assert!(why.contains("left alone"), "{why}");
        assert_eq!(tree_of(&env.live("alpha")), expected(&[("SKILL.md", "mine")]));
        assert!(state.repo(REPO).is_none_or(|r| r.skills.is_empty()), "no record and no marker are left");
        assert_eq!(State::load(&env.state).unwrap().state, state);
        assert!(env.work_entries().is_empty(), "{:?}", env.work_entries());
    }

    #[test]
    fn a_state_file_that_cannot_be_saved_stops_the_change_before_anything_moves() {
        let env = Env::new("unsavable");
        let (mut a, mut state) = installed_v1(&env);
        let before = state.clone();
        let blocker = env.root.join("blocker");
        fs::write(&blocker, "a file where a folder is needed").unwrap();
        a.state_path = blocker.join("state.json");
        assert_eq!(install(&mut a, &mut state, "alpha", V2, C2, Mode::Normal).unwrap_err().kind, FailureKind::Local);
        assert_eq!(a.delete(&mut state, REPO, "alpha").unwrap_err().kind, FailureKind::Local);
        assert_eq!(tree_of(&env.live("alpha")), expected(V1));
        assert_eq!(state, before, "the in-memory record was put back");
        assert!(env.work_entries().is_empty(), "{:?}", env.work_entries());
    }

    #[test]
    fn an_existing_unowned_folder_is_never_touched_by_a_normal_install() {
        let env = Env::new("unowned");
        let (mut a, mut state) = env.restart();
        fs::create_dir_all(env.live("alpha")).unwrap();
        fs::write(env.live("alpha").join("SKILL.md"), "mine").unwrap();
        assert!(held_reason(install(&mut a, &mut state, "alpha", V1, C1, Mode::Normal)).contains("left alone"));
        assert_eq!(tree_of(&env.live("alpha")), expected(&[("SKILL.md", "mine")]));
        assert!(state.repo(REPO).map(|r| r.skills.is_empty()).unwrap_or(true));
        assert!(env.work_entries().is_empty());
    }

    #[test]
    fn a_failed_download_changes_nothing() {
        let env = Env::new("flaky");
        let (mut a, mut state) = installed_v1(&env);
        let before = state.clone();
        let mut calls = 0;
        let mut flaky = |file: &SkillFile, sink: &mut dyn FnMut(&[u8]) -> bool| {
            calls += 1;
            if calls == 2 { Err(RemoteError::new(FailureKind::Network, "connection lost")) } else { serve(V2)(file, sink) }
        };
        let e = a.install(&mut state, REPO, &skill("alpha", V2), C2, Mode::Normal, &mut flaky, &AtomicBool::new(false)).unwrap_err();
        assert_eq!(e.kind, FailureKind::Network);
        assert_eq!(tree_of(&env.live("alpha")), expected(V1));
        assert_eq!(state, before);
        assert!(env.work_entries().is_empty(), "the half-downloaded copy is removed: {:?}", env.work_entries());
    }

    #[test]
    fn a_cancelled_install_stops_between_files_and_cleans_up() {
        let env = Env::new("cancel");
        let (mut a, mut state) = env.restart();
        let cancel = AtomicBool::new(false);
        let mut fetch = |file: &SkillFile, sink: &mut dyn FnMut(&[u8]) -> bool| {
            cancel.store(true, Ordering::Relaxed);
            serve(V1)(file, sink)
        };
        let e = a.install(&mut state, REPO, &skill("alpha", V1), C1, Mode::Normal, &mut fetch, &cancel).unwrap_err();
        assert_eq!(e.kind, FailureKind::Cancelled);
        assert!(!env.live("alpha").exists());
        assert!(env.work_entries().is_empty());
        assert!(state.repo(REPO).is_none_or(|r| r.skills.is_empty()));
    }

    #[test]
    fn content_that_does_not_match_the_listing_is_never_installed() {
        let env = Env::new("mismatch");
        let (mut a, mut state) = env.restart();
        let wrong_bytes = |file: &SkillFile, sink: &mut dyn FnMut(&[u8]) -> bool| {
            let mut content = V1.iter().find(|(p, _)| *p == file.path).unwrap().1.as_bytes().to_vec();
            content[0] ^= 1;
            sink(&content);
            Ok(())
        };
        let too_long = |file: &SkillFile, sink: &mut dyn FnMut(&[u8]) -> bool| {
            let mut content = V1.iter().find(|(p, _)| *p == file.path).unwrap().1.as_bytes().to_vec();
            content.push(b'!');
            let _ = sink(&content);
            Ok(())
        };
        let too_short = |file: &SkillFile, sink: &mut dyn FnMut(&[u8]) -> bool| {
            let content = V1.iter().find(|(p, _)| *p == file.path).unwrap().1.as_bytes();
            sink(&content[..content.len() - 1]);
            Ok(())
        };
        let cases: [(&str, Fetch); 3] = [("wrong bytes", &mut { wrong_bytes }), ("too long", &mut { too_long }), ("too short", &mut { too_short })];
        for (label, fetch) in cases {
            let e = a.install(&mut state, REPO, &skill("alpha", V1), C1, Mode::Normal, fetch, &AtomicBool::new(false)).unwrap_err();
            assert_eq!(e.kind, FailureKind::Changed, "{label}");
            assert!(!env.live("alpha").exists(), "{label}");
            assert!(env.work_entries().is_empty(), "{label}: {:?}", env.work_entries());
            assert!(state.repo(REPO).is_none_or(|r| r.skills.is_empty()), "{label}");
        }
    }

    #[test]
    fn unsafe_names_are_refused_before_anything_is_written() {
        let env = Env::new("unsafe");
        let (mut a, mut state) = env.restart();
        let evil: &[(&str, &str)] = &[("SKILL.md", "x"), ("../escape.txt", "y")];
        let e = install(&mut a, &mut state, "alpha", evil, C1, Mode::Normal).unwrap_err();
        assert_eq!(e.kind, FailureKind::Protocol);
        for name in ["..", "a\\b", "CON", "x.", ""] {
            assert_eq!(install(&mut a, &mut state, name, V1, C1, Mode::Normal).unwrap_err().kind, FailureKind::Protocol, "{name:?}");
        }
        assert!(!env.work.exists() && !env.skills.exists(), "no folder was even created");
        assert!(!env.root.join("escape.txt").exists());
    }

    #[test]
    fn overwrite_replaces_local_edits_but_keeps_them_as_a_backup() {
        let env = Env::new("overwrite");
        let (mut a, mut state) = installed_v1(&env);
        edit_skill_file(&env);
        fs::write(env.live("alpha").join("notes.txt"), "my notes").unwrap();
        assert_eq!(install(&mut a, &mut state, "alpha", V2, C2, Mode::Overwrite).unwrap(), Applied::Done);
        assert_eq!(entries_of(&env.live("alpha")), with(V2, &[("notes.txt", "my notes")]), "the file the user added stays where it is");
        assert_eq!(local_state(&mut a, &env, &state, "alpha"), LocalState::Clean);
        let backups = env.work_dirs_starting("backup-alpha-");
        assert_eq!(backups.len(), 1, "{:?}", env.work_entries());
        assert_eq!(fs::read_to_string(backups[0].join("SKILL.md")).unwrap(), EDITED);
        assert!(!backups[0].join("notes.txt").exists());
        assert_eq!(env.work_entries().len(), 1, "only the backup remains: {:?}", env.work_entries());
        a.sweep(&state);
        assert_eq!(env.work_dirs_starting("backup-alpha-").len(), 1, "backups are never swept");
    }

    #[test]
    fn overwrite_over_an_unowned_folder_keeps_that_folder_as_a_backup() {
        let env = Env::new("overwrite-unowned");
        let (mut a, mut state) = env.restart();
        fs::create_dir_all(env.live("alpha")).unwrap();
        fs::write(env.live("alpha").join("SKILL.md"), EDITED).unwrap();
        fs::write(env.live("alpha").join("notes.txt"), "my notes").unwrap();
        assert_eq!(install(&mut a, &mut state, "alpha", V1, C1, Mode::Overwrite).unwrap(), Applied::Done);
        assert_eq!(entries_of(&env.live("alpha")), with(V1, &[("notes.txt", "my notes")]), "only what GitHub has was replaced");
        assert!(edited_copy_survives(&env));
        assert_eq!(local_state(&mut a, &env, &state, "alpha"), LocalState::Clean);
    }

    #[test]
    fn a_folder_that_a_program_is_using_postpones_the_update_without_loss() {
        let env = Env::new("in-use");
        let (mut a, mut state) = installed_v1(&env);
        let before = state.clone();
        // A directory handle that does not share delete is what a process with this folder as its current directory holds.
        let _holder = OpenOptions::new().read(true).custom_flags(0x0200_0000).share_mode(1 | 2).open(env.live("alpha")).unwrap();
        let why = held_reason(install(&mut a, &mut state, "alpha", V2, C2, Mode::Normal));
        assert!(why.contains("in use"), "{why}");
        assert_eq!(tree_of(&env.live("alpha")), expected(V1));
        assert_eq!(state, before, "no marker is left behind");
        assert_eq!(State::load(&env.state).unwrap().state, before);
        assert!(env.work_entries().is_empty(), "{:?}", env.work_entries());
        drop(_holder);
        assert_eq!(install(&mut a, &mut state, "alpha", V2, C2, Mode::Normal).unwrap(), Applied::Done, "it works once the program lets go");
    }

    #[test]
    fn an_edit_that_lands_during_the_swap_is_put_back_and_held() {
        let env = Env::new("race");
        let (mut a, mut state) = installed_v1(&env);
        add(&env.live("alpha"), MINE);
        let before = state.clone();
        let work = env.work.clone();
        a.hook = Some(Box::new(move |step| {
            if step == Step::NewInPlace {
                // The user saves a file through a handle that was already open on the old folder.
                fs::write(work_dir_starting(&work, "old-").join("SKILL.md"), EDITED).unwrap();
            }
            Ok(())
        }));
        let why = held_reason(install(&mut a, &mut state, "alpha", V2, C2, Mode::Normal));
        assert!(why.contains("edited locally") && why.contains("kept"), "{why}");
        let mut v1_edited = with(V1, MINE);
        v1_edited.insert("SKILL.md".into(), EDITED.into());
        assert_eq!(entries_of(&env.live("alpha")), v1_edited, "the user's version is back in place, with the files they added");
        assert_eq!(state, before);
        assert_eq!(State::load(&env.state).unwrap().state, before);
        assert!(env.work_entries().is_empty(), "{:?}", env.work_entries());
    }

    #[test]
    fn a_new_version_that_was_tampered_with_is_kept_aside_not_deleted_when_rolling_back() {
        let env = Env::new("tampered-new");
        let (mut a, mut state) = installed_v1(&env);
        let (work, live) = (env.work.clone(), env.live("alpha"));
        a.hook = Some(Box::new(move |step| {
            if step == Step::NewInPlace {
                let old = fs::read_dir(&work).unwrap().flatten().find(|e| e.file_name().to_string_lossy().starts_with("old-")).unwrap().path();
                fs::write(old.join("SKILL.md"), EDITED).unwrap();
                fs::write(live.join("SKILL.md"), "someone wrote into the new folder").unwrap();
            }
            Ok(())
        }));
        assert!(held_reason(install(&mut a, &mut state, "alpha", V2, C2, Mode::Normal)).contains("edited locally"));
        assert_eq!(fs::read_to_string(env.live("alpha").join("SKILL.md")).unwrap(), EDITED);
        let kept = env.work_dirs_starting("backup-alpha-");
        assert_eq!(kept.len(), 1, "{:?}", env.work_entries());
        assert_eq!(fs::read_to_string(kept[0].join("SKILL.md")).unwrap(), "someone wrote into the new folder");
    }

    #[test]
    fn deleting_a_clean_skill_removes_the_folder_and_the_record() {
        let env = Env::new("delete");
        let (mut a, mut state) = installed_v1(&env);
        assert_eq!(a.delete(&mut state, REPO, "alpha").unwrap(), Applied::Done);
        assert!(!env.live("alpha").exists());
        assert!(state.repo(REPO).unwrap().skills.is_empty());
        assert!(env.work_entries().is_empty(), "{:?}", env.work_entries());
        assert_eq!(State::load(&env.state).unwrap().state, state);
        assert_eq!(a.delete(&mut state, REPO, "alpha").unwrap(), Applied::Done, "deleting twice is harmless");
    }

    #[test]
    fn deleting_never_removes_local_edits() {
        let env = Env::new("delete-edited");
        let (mut a, mut state) = installed_v1(&env);
        edit_skill_file(&env);
        let before = state.clone();
        assert!(held_reason(a.delete(&mut state, REPO, "alpha")).contains("edited locally"));
        assert_eq!(fs::read_to_string(env.live("alpha").join("SKILL.md")).unwrap(), EDITED);
        assert_eq!(state, before);
        assert!(env.work_entries().is_empty());
    }

    #[test]
    fn removing_a_skill_keeps_the_files_the_user_added() {
        let env = Env::new("delete-keeps-mine");
        let (mut a, mut state) = installed_v1(&env);
        let live = env.live("alpha");
        add(&live, MINE);
        fs::create_dir_all(live.join("drafts")).unwrap();
        assert_eq!(a.delete(&mut state, REPO, "alpha").unwrap(), Applied::Done);
        let mut want = expected(MINE);
        want.insert("drafts".into(), DIR.into());
        assert_eq!(entries_of(&live), want, "only what came from GitHub was removed");
        assert!(state.repo(REPO).unwrap().skill("alpha").is_none(), "what is left is the user's");
        assert_eq!(State::load(&env.state).unwrap().state, state);
        assert!(env.work_entries().is_empty(), "{:?}", env.work_entries());

        // An empty folder the user made is theirs too, even if it is all they added.
        let env = Env::new("delete-keeps-folder");
        let (mut a, mut state) = installed_v1(&env);
        fs::create_dir_all(env.live("alpha").join("references").join("empty")).unwrap();
        assert_eq!(a.delete(&mut state, REPO, "alpha").unwrap(), Applied::Done);
        assert_eq!(entries_of(&env.live("alpha")), BTreeMap::from([("references/empty".to_string(), DIR.to_string())]));
        assert!(env.work_entries().is_empty(), "{:?}", env.work_entries());
    }

    #[test]
    fn an_edit_that_lands_while_removing_a_skill_with_added_files_puts_everything_back() {
        let env = Env::new("delete-mine-race");
        let (mut a, mut state) = installed_v1(&env);
        add(&env.live("alpha"), MINE);
        let before = state.clone();
        let work = env.work.clone();
        a.hook = Some(Box::new(move |step| {
            if step == Step::NewInPlace {
                fs::write(work_dir_starting(&work, "old-").join("SKILL.md"), EDITED).unwrap();
            }
            Ok(())
        }));
        let why = held_reason(a.delete(&mut state, REPO, "alpha"));
        assert!(why.contains("edited locally") && why.contains("kept"), "{why}");
        let mut v1_edited = with(V1, MINE);
        v1_edited.insert("SKILL.md".into(), EDITED.into());
        assert_eq!(entries_of(&env.live("alpha")), v1_edited);
        assert_eq!(state, before);
        assert_eq!(State::load(&env.state).unwrap().state, before);
        assert!(env.work_entries().is_empty(), "{:?}", env.work_entries());
    }

    #[test]
    fn a_file_added_while_a_skill_is_being_removed_is_never_deleted_with_it() {
        let env = Env::new("delete-added-late");
        let (mut a, mut state) = installed_v1(&env);
        let work = env.work.clone();
        a.hook = Some(Box::new(move |step| {
            if step == Step::OldMoved {
                fs::write(work_dir_starting(&work, "trash-").join("notes.txt"), "my notes").unwrap();
            }
            Ok(())
        }));
        assert!(held_reason(a.delete(&mut state, REPO, "alpha")).contains("kept"));
        assert_eq!(entries_of(&env.live("alpha")), with(V1, &[("notes.txt", "my notes")]));
        a.hook = None;
        assert_eq!(a.delete(&mut state, REPO, "alpha").unwrap(), Applied::Done, "the next try removes what came from GitHub");
        assert_eq!(entries_of(&env.live("alpha")), expected(&[("notes.txt", "my notes")]));

        // The same, with the app stopped dead right after the file appeared.
        let env = Env::new("delete-added-crash");
        let (mut a, mut state) = installed_v1(&env);
        let work = env.work.clone();
        a.hook = Some(Box::new(move |step| {
            if step == Step::OldMoved {
                fs::write(work_dir_starting(&work, "trash-").join("notes.txt"), "my notes").unwrap();
                return Err(local("simulated crash"));
            }
            Ok(())
        }));
        assert!(a.delete(&mut state, REPO, "alpha").is_err());
        drop((a, state));
        let (mut a, mut state) = env.restart();
        let notes = a.recover(&mut state).unwrap();
        assert!(notes[0].text.contains("undid"), "{notes:?}");
        assert_eq!(entries_of(&env.live("alpha")), with(V1, &[("notes.txt", "my notes")]));
        assert!(env.work_entries().is_empty(), "{:?}", env.work_entries());
    }

    #[test]
    fn a_file_written_into_the_new_version_during_the_swap_is_never_replaced_by_one_carried_over() {
        let env = Env::new("carry-no-replace");
        let (mut a, mut state) = installed_v1(&env);
        add(&env.live("alpha"), &[("notes.txt", "my notes")]);
        let live = env.live("alpha");
        a.hook = Some(Box::new(move |step| {
            if step == Step::NewInPlace {
                fs::write(live.join("notes.txt"), "written meanwhile").unwrap();
            }
            Ok(())
        }));
        assert_eq!(install(&mut a, &mut state, "alpha", V2, C2, Mode::Normal).unwrap(), Applied::Done);
        assert_eq!(entries_of(&env.live("alpha")), with(V2, &[("notes.txt", "written meanwhile")]));
        let backups = env.work_dirs_starting("backup-");
        assert_eq!(backups.len(), 1, "{:?}", env.work_entries());
        assert_eq!(fs::read_to_string(backups[0].join("notes.txt")).unwrap(), "my notes");
    }

    #[test]
    fn a_removal_whose_added_files_are_gone_by_the_end_leaves_no_empty_folder() {
        // The file goes while the folder is being swapped.
        let env = Env::new("delete-mine-gone");
        let (mut a, mut state) = installed_v1(&env);
        add(&env.live("alpha"), &[("notes.txt", "my notes")]);
        let work = env.work.clone();
        a.hook = Some(Box::new(move |step| {
            if step == Step::NewInPlace {
                fs::remove_file(work_dir_starting(&work, "old-").join("notes.txt")).unwrap();
            }
            Ok(())
        }));
        assert_eq!(a.delete(&mut state, REPO, "alpha").unwrap(), Applied::Done);
        assert!(!exists_no_follow(&env.live("alpha")));
        assert!(state.repo(REPO).unwrap().skills.is_empty());

        // The file goes after a crash, before the removal is finished at the next start.
        let env = Env::new("delete-mine-gone-crash");
        let (mut a, mut state) = installed_v1(&env);
        add(&env.live("alpha"), &[("notes.txt", "my notes")]);
        crash_at(&mut a, Step::Settled);
        assert!(a.delete(&mut state, REPO, "alpha").is_err());
        drop((a, state));
        fs::remove_file(env.live("alpha").join("notes.txt")).unwrap();
        let (mut a, mut state) = env.restart();
        let notes = a.recover(&mut state).unwrap();
        assert!(notes[0].text.contains("finished"), "{notes:?}");
        assert!(!exists_no_follow(&env.live("alpha")));
        assert!(state.repo(REPO).unwrap().skills.is_empty());
        assert!(env.work_entries().is_empty(), "{:?}", env.work_entries());
    }

    /// Adds a folder of the user's to `skill` that this process cannot open: the path is too long, and tests are not
    /// long-path aware. The skill's folder can then not be listed, so what the user added could not be moved along.
    fn add_unlistable(skill: &Path) -> PathBuf {
        let deep: PathBuf = ["node_modules", &"a".repeat(80), &"b".repeat(80), &"c".repeat(80)].iter().collect();
        let deep = skill.join(deep).join("index.js");
        fs::create_dir_all(deep.parent().unwrap()).unwrap();
        fs::write(&deep, "mine").unwrap();
        assert!(list_entries(skill).is_err(), "the test needs a folder that cannot be listed");
        deep
    }

    #[test]
    fn a_skill_whose_folder_cannot_be_listed_is_not_changed_and_nothing_the_user_added_is_moved() {
        let env = Env::new("unlistable");
        let (mut a, mut state) = installed_v1(&env);
        let mine = add_unlistable(&env.live("alpha"));
        let before = state.clone();
        for mode in [Mode::Normal, Mode::Overwrite] {
            let why = held_reason(install(&mut a, &mut state, "alpha", V2, C2, mode));
            assert!(why.contains("could not be read"), "{mode:?}: {why}");
            assert_eq!(state, before, "{mode:?}");
            assert_eq!(fs::read_to_string(&mine).unwrap(), "mine", "{mode:?}");
            assert_eq!(fs::read_to_string(env.live("alpha").join("SKILL.md")).unwrap(), "# alpha v1\n", "{mode:?}");
            assert!(env.work_entries().is_empty(), "{mode:?}: {:?}", env.work_entries());
        }
        assert!(held_reason(a.delete(&mut state, REPO, "alpha")).contains("could not be read"));
        assert_eq!(fs::read_to_string(&mine).unwrap(), "mine");
        assert_eq!(fs::read_to_string(env.live("alpha").join("SKILL.md")).unwrap(), "# alpha v1\n");

        // The same for a folder the app does not own, replaced at the user's request.
        let env = Env::new("unlistable-unowned");
        let (mut a, mut state) = env.restart();
        let mine = add_unlistable(&env.live("alpha"));
        fs::write(env.live("alpha").join("SKILL.md"), EDITED).unwrap();
        let before = state.clone();
        let why = held_reason(install(&mut a, &mut state, "alpha", V1, C1, Mode::Overwrite));
        assert!(why.contains("could not be read"), "{why}");
        assert_eq!(state, before);
        assert_eq!(fs::read_to_string(&mine).unwrap(), "mine");
        assert_eq!(fs::read_to_string(env.live("alpha").join("SKILL.md")).unwrap(), EDITED);
        assert!(env.work_entries().is_empty(), "{:?}", env.work_entries());
    }

    #[test]
    fn deleting_a_skill_whose_folder_is_already_gone_just_drops_the_record() {
        let env = Env::new("delete-gone");
        let (mut a, mut state) = installed_v1(&env);
        remove_dir_all_retry(&env.live("alpha")).unwrap();
        assert_eq!(a.delete(&mut state, REPO, "alpha").unwrap(), Applied::Done);
        assert!(state.repo(REPO).unwrap().skills.is_empty());
    }

    #[test]
    fn an_edit_that_lands_while_removing_restores_the_folder() {
        let env = Env::new("delete-race");
        let (mut a, mut state) = installed_v1(&env);
        let before = state.clone();
        let work = env.work.clone();
        a.hook = Some(Box::new(move |step| {
            if step == Step::OldMoved {
                let trash = fs::read_dir(&work).unwrap().flatten().find(|e| e.file_name().to_string_lossy().starts_with("trash-")).unwrap().path();
                fs::write(trash.join("SKILL.md"), EDITED).unwrap();
            }
            Ok(())
        }));
        assert!(held_reason(a.delete(&mut state, REPO, "alpha")).contains("kept"));
        assert_eq!(fs::read_to_string(env.live("alpha").join("SKILL.md")).unwrap(), EDITED);
        assert_eq!(state, before);
        assert!(env.work_entries().is_empty());
    }

    #[test]
    fn a_folder_in_use_postpones_the_removal() {
        let env = Env::new("delete-in-use");
        let (mut a, mut state) = installed_v1(&env);
        let before = state.clone();
        let _holder = OpenOptions::new().read(true).custom_flags(0x0200_0000).share_mode(1 | 2).open(env.live("alpha")).unwrap();
        assert!(held_reason(a.delete(&mut state, REPO, "alpha")).contains("in use"));
        assert_eq!(tree_of(&env.live("alpha")), expected(V1));
        assert_eq!(state, before);
    }

    #[test]
    fn an_identical_unowned_folder_is_adopted_without_touching_a_byte() {
        let env = Env::new("adopt");
        let (mut a, mut state) = env.restart();
        for (rel, content) in V1 {
            let p = join_rel(&env.live("alpha"), rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, content).unwrap();
        }
        let stamp = |rel: &str| probe(&join_rel(&env.live("alpha"), rel)).unwrap().unwrap().fp;
        let before: Vec<_> = V1.iter().map(|(rel, _)| stamp(rel)).collect();
        assert_eq!(a.adopt(&mut state, REPO, &skill("alpha", V1), C1, &HashMap::new()).unwrap(), Applied::Done);
        let after: Vec<_> = V1.iter().map(|(rel, _)| stamp(rel)).collect();
        assert_eq!(before, after, "no file was written, not even its timestamps");
        assert_eq!(local_state(&mut a, &env, &state, "alpha"), LocalState::Clean);
        assert!(record(&state, "alpha").files.iter().all(|f| f.fp.is_some()));
        assert_eq!(record(&state, "alpha").updated, Some(T0), "adopting counts as the skill's last change");
    }

    /// The card's "Updated 2 days ago" is the time the app last put a version of the skill in place, and nothing else.
    #[test]
    fn a_skill_records_when_a_version_was_last_put_in_place_and_anything_that_keeps_the_old_version_keeps_its_time() {
        let env = Env::new("updated");
        let (mut a, mut state) = installed_v1(&env);
        assert_eq!(record(&state, "alpha").updated, Some(T0));
        a.now = T0 + 10;
        assert_eq!(install(&mut a, &mut state, "alpha", V2, C2, Mode::Normal).unwrap(), Applied::Done);
        assert_eq!(record(&state, "alpha").updated, Some(T0 + 10));
        assert_eq!(State::load(&env.state).unwrap().state, state, "saved with the update");

        edit_skill_file(&env);
        a.now = T0 + 20;
        held_reason(install(&mut a, &mut state, "alpha", V1, C1, Mode::Normal));
        assert_eq!(record(&state, "alpha").updated, Some(T0 + 10), "a held update changes nothing");
        assert_eq!(install(&mut a, &mut state, "alpha", V1, C1, Mode::Overwrite).unwrap(), Applied::Done);
        assert_eq!(record(&state, "alpha").updated, Some(T0 + 20));

        // After a crash, recovery records the time it finished the update; an undone update keeps the old time.
        for (step, finished) in [(Step::Marked, false), (Step::Settled, true)] {
            let env = Env::new("updated-crash");
            let (mut a, mut state) = installed_v1(&env);
            a.now = T0 + 10;
            crash_at(&mut a, step);
            assert!(install(&mut a, &mut state, "alpha", V2, C2, Mode::Normal).is_err());
            drop((a, state));
            let (mut a, mut state) = env.restart();
            a.now = T0 + 30;
            a.recover(&mut state).unwrap();
            assert_eq!(record(&state, "alpha").updated, Some(if finished { T0 + 30 } else { T0 }), "{step:?}");
        }
    }

    #[test]
    fn a_folder_that_differs_is_not_adopted() {
        let env = Env::new("adopt-differs");
        let (mut a, mut state) = env.restart();
        fs::create_dir_all(env.live("alpha")).unwrap();
        fs::write(env.live("alpha").join("SKILL.md"), EDITED).unwrap();
        assert!(held_reason(a.adopt(&mut state, REPO, &skill("alpha", V1), C1, &HashMap::new())).contains("left alone"));
        assert!(state.repo(REPO).is_none_or(|r| r.skills.is_empty()));
    }

    #[test]
    fn a_folder_with_every_file_of_upstream_and_more_is_adopted_and_the_rest_stays_the_users() {
        let env = Env::new("adopt-more");
        let (mut a, mut state) = env.restart();
        add(&env.live("alpha"), V1);
        add(&env.live("alpha"), &[("notes.md", "mine\n")]);
        assert_eq!(a.adopt(&mut state, REPO, &skill("alpha", V1), C1, &HashMap::new()).unwrap(), Applied::Done);
        let recorded: Vec<&str> = record(&state, "alpha").files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(recorded, V1.iter().map(|(p, _)| *p).collect::<Vec<_>>(), "only GitHub's files are owned");
        assert_eq!(local_state(&mut a, &env, &state, "alpha"), LocalState::Clean);

        assert_eq!(install(&mut a, &mut state, "alpha", V2, C2, Mode::Normal).unwrap(), Applied::Done);
        assert_eq!(entries_of(&env.live("alpha")), with(V2, &[("notes.md", "mine\n")]));
        assert!(env.work_entries().is_empty(), "{:?}", env.work_entries());
    }

    /// The files as a Windows editor might save them: other line breaks and a few blank lines more.
    fn windows_copy(files: Files) -> Vec<(String, String)> {
        files.iter().map(|(p, c)| (p.to_string(), format!("\r\n{}\r\n", c.replace('\n', "\r\n")))).collect()
    }

    fn write_files(dir: &Path, files: &[(String, String)]) {
        for (rel, content) in files {
            let p = join_rel(dir, rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, content).unwrap();
        }
    }

    #[test]
    fn a_copy_with_other_line_breaks_is_adopted_as_it_is_and_then_updated_like_any_owned_skill() {
        let env = Env::new("adopt-text");
        let (mut a, mut state) = env.restart();
        let copy = windows_copy(V1);
        write_files(&env.live("alpha"), &copy);
        assert!(held_reason(a.adopt(&mut state, REPO, &skill("alpha", V1), C1, &HashMap::new())).contains("left alone"), "the copy on GitHub is not known");

        let theirs: HashMap<String, String> = V1.iter().map(|(p, c)| (p.to_string(), crate::text::canon(c.as_bytes()))).collect();
        assert_eq!(a.adopt(&mut state, REPO, &skill("alpha", V1), C1, &theirs).unwrap(), Applied::Done);
        assert_eq!(tree_of(&env.live("alpha")), copy.iter().cloned().collect(), "not one byte was written");
        assert!(record(&state, "alpha").files.iter().all(|f| f.fp.is_some() && f.canon.as_ref().is_some_and(|c| c.len() == 40)));
        assert_eq!(local_state(&mut a, &env, &state, "alpha"), LocalState::Clean);

        assert_eq!(install(&mut a, &mut state, "alpha", V2, C2, Mode::Normal).unwrap(), Applied::Done);
        assert_eq!(tree_of(&env.live("alpha")), expected(V2));
        assert!(env.work_entries().is_empty(), "the old copy was proven the same, so nothing was kept: {:?}", env.work_entries());
    }

    #[test]
    fn an_owned_skill_saved_with_other_line_breaks_is_not_an_edit_and_is_removed_without_a_backup() {
        let env = Env::new("owned-text");
        let (mut a, mut state) = installed_v1(&env);
        write_files(&env.live("alpha"), &windows_copy(V1));
        assert_eq!(local_state(&mut a, &env, &state, "alpha"), LocalState::Clean);
        assert_eq!(a.delete(&mut state, REPO, "alpha").unwrap(), Applied::Done);
        assert!(!exists_no_follow(&env.live("alpha")));
        assert!(env.work_entries().is_empty(), "{:?}", env.work_entries());

        let (mut a, mut state) = installed_v1(&env);
        write_files(&env.live("alpha"), &[("SKILL.md".to_string(), EDITED.replace('\n', "\r\n"))]);
        assert!(held_reason(install(&mut a, &mut state, "alpha", V2, C2, Mode::Normal)).contains("edited locally"));
        assert_eq!(fs::read_to_string(env.live("alpha").join("SKILL.md")).unwrap(), EDITED.replace('\n', "\r\n"), "a real edit still holds");
    }

    /// The folder as the user left it after making the very changes that GitHub's next version has.
    fn edited_into_v2(env: &Env) {
        add(&env.live("alpha"), V2);
        fs::remove_file(env.live("alpha").join("references").join("a.md")).unwrap();
    }

    #[test]
    fn an_edit_that_upstream_now_has_too_is_caught_up_without_touching_a_byte() {
        let env = Env::new("catch-up");
        let (mut a, mut state) = installed_v1(&env);
        edited_into_v2(&env);
        add(&env.live("alpha"), MINE);
        assert!(matches!(local_state(&mut a, &env, &state, "alpha"), LocalState::Dirty(_)), "the record still says the old version");
        let stamp = |rel: &str| probe(&join_rel(&env.live("alpha"), rel)).unwrap().unwrap().fp;
        let before: Vec<_> = V2.iter().map(|(rel, _)| stamp(rel)).collect();
        a.now = T0 + 10;
        assert_eq!(a.catch_up(&mut state, REPO, &skill("alpha", V2), C2, &HashMap::new()).unwrap(), Applied::Done);
        let after: Vec<_> = V2.iter().map(|(rel, _)| stamp(rel)).collect();
        assert_eq!(before, after, "no file was written, not even its timestamps");
        assert_eq!(entries_of(&env.live("alpha")), with(V2, MINE), "what the user added is still there");
        let rec = record(&state, "alpha");
        assert_eq!((rec.commit.as_str(), rec.updated, rec.pending.is_none()), (C2, Some(T0 + 10), true));
        assert_eq!(
            rec.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
            V2.iter().map(|(p, _)| *p).collect::<Vec<_>>(),
            "only GitHub's files are owned"
        );
        assert!(rec.files.iter().all(|f| f.fp.is_some()));
        assert_eq!(local_state(&mut a, &env, &state, "alpha"), LocalState::Clean);
        assert_eq!(State::load(&env.state).unwrap().state, state, "saved");
        assert!(env.work_entries().is_empty(), "{:?}", env.work_entries());
    }

    #[test]
    fn a_catch_up_is_held_when_the_folder_is_not_what_upstream_has() {
        let env = Env::new("catch-up-differs");
        let (mut a, mut state) = installed_v1(&env);
        edited_into_v2(&env);
        fs::write(env.live("alpha").join("SKILL.md"), EDITED).unwrap();
        let before = state.clone();
        assert!(held_reason(a.catch_up(&mut state, REPO, &skill("alpha", V2), C2, &HashMap::new())).contains("left alone"));
        assert_eq!(state, before);
        assert_eq!(fs::read_to_string(env.live("alpha").join("SKILL.md")).unwrap(), EDITED);
        assert!(
            held_reason(a.catch_up(&mut state, REPO, &skill("alpha", V2), C2, &HashMap::from([("SKILL.md".to_string(), crate::text::canon(b"# alpha v2\n"))])))
                .contains("left alone")
        );
        assert_eq!(state, before);
    }

    #[test]
    fn a_catch_up_is_held_while_a_file_that_upstream_dropped_is_still_here() {
        let env = Env::new("catch-up-leftover");
        let (mut a, mut state) = installed_v1(&env);
        add(&env.live("alpha"), V2);
        let before = state.clone();
        assert!(held_reason(a.catch_up(&mut state, REPO, &skill("alpha", V2), C2, &HashMap::new())).contains("left alone"));
        assert_eq!(state, before);
        assert_eq!(fs::read_to_string(env.live("alpha").join("references").join("a.md")).unwrap(), "ref a\n");
    }

    #[test]
    fn a_catch_up_for_a_copy_with_other_line_breaks_needs_the_copy_from_github() {
        let env = Env::new("catch-up-text");
        let (mut a, mut state) = installed_v1(&env);
        fs::remove_file(env.live("alpha").join("references").join("a.md")).unwrap();
        let copy = windows_copy(V2);
        write_files(&env.live("alpha"), &copy);
        let before = state.clone();
        assert!(held_reason(a.catch_up(&mut state, REPO, &skill("alpha", V2), C2, &HashMap::new())).contains("left alone"), "the copy on GitHub is not known");
        assert_eq!(state, before);

        let theirs: HashMap<String, String> = V2.iter().map(|(p, c)| (p.to_string(), crate::text::canon(c.as_bytes()))).collect();
        assert_eq!(a.catch_up(&mut state, REPO, &skill("alpha", V2), C2, &theirs).unwrap(), Applied::Done);
        assert_eq!(tree_of(&env.live("alpha")), copy.iter().cloned().collect(), "not one byte was written");
        assert_eq!(local_state(&mut a, &env, &state, "alpha"), LocalState::Clean);
        assert_eq!(record(&state, "alpha").commit, C2);
    }

    #[test]
    fn a_catch_up_changes_nothing_for_a_skill_that_is_not_recorded_or_is_not_sorted_out() {
        let env = Env::new("catch-up-guard");
        let (mut a, mut state) = installed_v1(&env);
        edited_into_v2(&env);
        let before = state.clone();
        assert!(held_reason(a.catch_up(&mut state, REPO, &skill("beta", V2), C2, &HashMap::new())).contains("no longer recorded"));
        assert_eq!(state, before);

        let env = Env::new("catch-up-pending");
        let (mut a, mut state) = installed_v1(&env);
        crash_at(&mut a, Step::Marked);
        assert!(install(&mut a, &mut state, "alpha", V2, C2, Mode::Normal).is_err());
        a.hook = None;
        edited_into_v2(&env);
        let before = state.clone();
        assert!(held_reason(a.catch_up(&mut state, REPO, &skill("alpha", V2), C2, &HashMap::new())).contains("not sorted out"));
        assert_eq!(state, before);
    }

    #[test]
    fn a_skill_that_is_pending_is_not_touched_again_until_recovered() {
        let env = Env::new("pending-guard");
        let (mut a, mut state) = installed_v1(&env);
        crash_at(&mut a, Step::Marked);
        assert!(install(&mut a, &mut state, "alpha", V2, C2, Mode::Normal).is_err());
        a.hook = None;
        assert!(held_reason(install(&mut a, &mut state, "alpha", V2, C2, Mode::Normal)).contains("interrupted"));
        assert!(held_reason(a.delete(&mut state, REPO, "alpha")).contains("interrupted"));
    }

    #[test]
    fn retire_deletes_only_what_it_can_prove_is_a_known_version() {
        let env = Env::new("retire");
        let (mut a, state) = installed_v1(&env);
        let files = record(&state, "alpha").files.clone();
        fs::create_dir_all(&env.work).unwrap();

        let copy = env.work.join("copy");
        for (rel, content) in V1 {
            let p = join_rel(&copy, rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, content).unwrap();
        }
        assert!(matches!(a.retire(&copy, &files, "alpha", "t1").unwrap(), Retired::Deleted));
        assert!(!copy.exists());

        for (rel, content) in V1 {
            let p = join_rel(&copy, rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, content).unwrap();
        }
        fs::write(copy.join("extra.txt"), "x").unwrap();
        assert!(matches!(a.retire(&copy, &files, "alpha", "t1").unwrap(), Retired::Kept(_)));
        assert!(!copy.exists());
        assert_eq!(fs::read_to_string(env.work.join("backup-alpha-t1").join("extra.txt")).unwrap(), "x");

        assert!(matches!(a.retire(&copy, &files, "alpha", "t1").unwrap(), Retired::Gone));
        let other = env.work.join("other");
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join("SKILL.md"), "not ours").unwrap();
        assert!(matches!(a.retire(&other, &files, "alpha", "t1").unwrap(), Retired::Kept(_)));
        assert!(env.work.join("backup-alpha-t1-2").exists(), "a second backup for the same change gets its own name");
    }

    #[test]
    fn sweep_removes_stages_and_junk_only_and_only_when_nothing_is_pending() {
        let env = Env::new("sweep");
        let (a, mut state) = installed_v1(&env);
        for name in ["stage-1", "junk-2", "backup-alpha-3", "old-4", "trash-5"] {
            fs::create_dir_all(env.work.join(name)).unwrap();
            fs::write(env.work.join(name).join("f.txt"), "x").unwrap();
        }
        let owned = record(&state, "alpha").clone();
        state.repo_mut(REPO).upsert_skill(OwnedSkill { pending: Some(Pending::Delete { txn: "5".into() }), ..owned.clone() });
        a.sweep(&state);
        assert_eq!(env.work_entries().len(), 5, "nothing is swept while a change is unsettled");
        state.repo_mut(REPO).upsert_skill(owned);
        a.sweep(&state);
        assert_eq!(env.work_entries(), vec!["backup-alpha-3".to_string(), "old-4".to_string(), "trash-5".to_string()]);
    }

    #[test]
    fn preflight_accepts_folders_on_one_drive_and_creates_them() {
        let env = Env::new("preflight");
        preflight(&env.skills, &env.work).unwrap();
        assert!(env.skills.is_dir() && env.work.is_dir());
    }

    /// The heart of the safety story: stop the app dead at every step, restart, and nothing may be lost.
    #[test]
    fn a_crash_at_any_step_of_an_update_loses_nothing_and_converges() {
        for step in [Step::Staged, Step::Marked, Step::OldMoved, Step::NewInPlace, Step::Settled] {
            let env = Env::new("crash-update");
            let (mut a, mut state) = installed_v1(&env);
            crash_at(&mut a, step);
            assert!(install(&mut a, &mut state, "alpha", V2, C2, Mode::Normal).is_err(), "{step:?}");
            drop((a, state));

            let (mut a, mut state) = env.restart();
            a.recover(&mut state).unwrap();
            assert!(a.recover(&mut state).unwrap().is_empty(), "{step:?}: recovery has nothing left to do");
            a.sweep(&state);
            let rec = record(&state, "alpha").clone();
            assert!(rec.pending.is_none(), "{step:?}");
            let disk = tree_of(&env.live("alpha"));
            assert!(disk == expected(V1) || disk == expected(V2), "{step:?}: the folder is a whole version, never a mix: {disk:?}");
            assert_eq!(local_state(&mut a, &env, &state, "alpha"), LocalState::Clean, "{step:?}: the record describes what is on disk");
            assert_eq!(State::load(&env.state).unwrap().state, state, "{step:?}: recovery was saved");
            assert!(env.work_entries().is_empty(), "{step:?}: {:?}", env.work_entries());

            if rec.commit != C2 {
                assert_eq!(install(&mut a, &mut state, "alpha", V2, C2, Mode::Normal).unwrap(), Applied::Done, "{step:?}: the next sync finishes the job");
            }
            assert_eq!(tree_of(&env.live("alpha")), expected(V2), "{step:?}");
        }
    }

    #[test]
    fn a_crash_at_any_step_of_a_first_install_leaves_nothing_half_done() {
        for step in [Step::Staged, Step::Marked, Step::OldMoved, Step::NewInPlace, Step::Settled] {
            let env = Env::new("crash-fresh");
            let (mut a, mut state) = env.restart();
            crash_at(&mut a, step);
            assert!(install(&mut a, &mut state, "alpha", V1, C1, Mode::Normal).is_err(), "{step:?}");
            drop((a, state));

            let (mut a, mut state) = env.restart();
            a.recover(&mut state).unwrap();
            a.sweep(&state);
            if env.live("alpha").exists() {
                assert_eq!(tree_of(&env.live("alpha")), expected(V1), "{step:?}");
                assert_eq!(local_state(&mut a, &env, &state, "alpha"), LocalState::Clean, "{step:?}");
            } else {
                assert!(state.repo(REPO).is_none_or(|r| r.skills.is_empty()), "{step:?}: no record for a skill that is not there");
            }
            assert!(env.work_entries().is_empty(), "{step:?}: {:?}", env.work_entries());
            if !env.live("alpha").exists() {
                assert_eq!(install(&mut a, &mut state, "alpha", V1, C1, Mode::Normal).unwrap(), Applied::Done, "{step:?}");
            }
            assert_eq!(tree_of(&env.live("alpha")), expected(V1), "{step:?}");
        }
    }

    #[test]
    fn a_crash_at_any_step_of_an_overwrite_never_loses_the_users_edit() {
        for step in [Step::Staged, Step::Marked, Step::OldMoved, Step::NewInPlace, Step::Settled] {
            let env = Env::new("crash-overwrite");
            let (mut a, mut state) = installed_v1(&env);
            edit_skill_file(&env);
            crash_at(&mut a, step);
            assert!(install(&mut a, &mut state, "alpha", V2, C2, Mode::Overwrite).is_err(), "{step:?}");
            assert!(edited_copy_survives(&env), "{step:?}: right after the crash");
            drop((a, state));

            let (mut a, mut state) = env.restart();
            a.recover(&mut state).unwrap();
            a.sweep(&state);
            assert!(edited_copy_survives(&env), "{step:?}: after recovery");
            assert!(record(&state, "alpha").pending.is_none(), "{step:?}");
            let live = tree_of(&env.live("alpha"));
            assert!(live.get("SKILL.md").map(String::as_str) == Some(EDITED) || live == expected(V2), "{step:?}: {live:?}");
            if live == expected(V2) {
                assert_eq!(local_state(&mut a, &env, &state, "alpha"), LocalState::Clean, "{step:?}");
            }
            let leftovers: Vec<String> = env.work_entries().into_iter().filter(|n| !n.starts_with("backup-")).collect();
            assert!(leftovers.is_empty(), "{step:?}: {leftovers:?}");

            // Try again: the overwrite still works and the edit is still kept somewhere.
            if live != expected(V2) {
                assert_eq!(install(&mut a, &mut state, "alpha", V2, C2, Mode::Overwrite).unwrap(), Applied::Done, "{step:?}");
            }
            assert_eq!(tree_of(&env.live("alpha")), expected(V2), "{step:?}");
            assert!(edited_copy_survives(&env), "{step:?}: after the retry");
        }
    }

    #[test]
    fn a_crash_at_any_step_never_loses_a_file_the_user_added() {
        #[derive(Clone, Copy, Debug, PartialEq)]
        enum Change {
            Update,
            Overwrite,
            Removal,
        }
        let run = |change: Change, a: &mut Applier, state: &mut State| match change {
            Change::Update => install(a, state, "alpha", V2, C2, Mode::Normal),
            Change::Overwrite => install(a, state, "alpha", V2, C2, Mode::Overwrite),
            Change::Removal => a.delete(state, REPO, "alpha"),
        };
        for change in [Change::Update, Change::Overwrite, Change::Removal] {
            for step in [Step::Staged, Step::Marked, Step::OldMoved, Step::NewInPlace, Step::Carried, Step::Settled] {
                let label = format!("{change:?} stopped at {step:?}");
                let env = Env::new("crash-mine");
                let (mut a, mut state) = installed_v1(&env);
                add(&env.live("alpha"), MINE);
                if change == Change::Overwrite {
                    edit_skill_file(&env);
                }
                crash_at(&mut a, step);
                let first = run(change, &mut a, &mut state);
                assert!(first.is_err(), "{label}: {first:?}");
                drop((a, state));

                let (mut a, mut state) = env.restart();
                a.recover(&mut state).unwrap();
                assert!(a.recover(&mut state).unwrap().is_empty(), "{label}: recovery has nothing left to do");
                a.sweep(&state);
                assert_eq!(State::load(&env.state).unwrap().state, state, "{label}: recovery was saved");
                let leftovers: Vec<String> = env.work_entries().into_iter().filter(|n| !n.starts_with("backup-")).collect();
                assert!(leftovers.is_empty(), "{label}: {leftovers:?}");
                mine_is_only_in_the_skill(&env, &format!("{label}, after recovery"));

                let owned = state.repo(REPO).and_then(|r| r.skill("alpha")).cloned();
                let done = match change {
                    Change::Removal => owned.is_none(),
                    _ => owned.as_ref().is_some_and(|o| o.commit == C2),
                };
                if !done {
                    assert_eq!(run(change, &mut a, &mut state).unwrap(), Applied::Done, "{label}: the next try finishes the job");
                }
                mine_is_only_in_the_skill(&env, &format!("{label}, at the end"));
                if change == Change::Removal {
                    assert_eq!(entries_of(&env.live("alpha")), expected(MINE), "{label}");
                    assert!(env.work_entries().is_empty(), "{label}: {:?}", env.work_entries());
                } else {
                    assert_eq!(entries_of(&env.live("alpha")), with(V2, MINE), "{label}");
                    assert_eq!(local_state(&mut a, &env, &state, "alpha"), LocalState::Clean, "{label}");
                }
                if change == Change::Overwrite {
                    assert!(edited_copy_survives(&env), "{label}");
                }
            }
        }
    }

    #[test]
    fn a_file_the_user_added_that_a_program_holds_open_is_moved_at_the_next_check() {
        let env = Env::new("carry-held-open");
        let (mut a, mut state) = installed_v1(&env);
        add(&env.live("alpha"), &[("notes.txt", "my notes")]);
        let holder: Arc<Mutex<Option<fs::File>>> = Arc::default();
        let (work, held) = (env.work.clone(), holder.clone());
        a.hook = Some(Box::new(move |step| {
            if step == Step::NewInPlace {
                let notes = work_dir_starting(&work, "old-").join("notes.txt");
                *held.lock().unwrap() = Some(OpenOptions::new().read(true).share_mode(1 | 2).open(notes).unwrap());
            }
            Ok(())
        }));
        let first = install(&mut a, &mut state, "alpha", V2, C2, Mode::Normal);
        assert!(first.as_ref().is_err_and(|e| e.message.contains("next check")), "{first:?}");
        *holder.lock().unwrap() = None;
        drop((a, state));

        let (mut a, mut state) = env.restart();
        let notes = a.recover(&mut state).unwrap();
        assert!(notes[0].text.contains("finished"), "{notes:?}");
        a.sweep(&state);
        assert_eq!(entries_of(&env.live("alpha")), with(V2, &[("notes.txt", "my notes")]));
        assert_eq!(record(&state, "alpha").commit, C2);
        assert!(env.work_entries().is_empty(), "{:?}", env.work_entries());
    }

    #[test]
    fn an_update_finished_after_a_crash_names_what_the_user_added_that_was_in_the_way() {
        let env = Env::new("crash-clash");
        let (mut a, mut state) = installed_v1(&env);
        add(&env.live("alpha"), &[("references/b.md", "my b"), ("keep.md", "mine")]);
        crash_at(&mut a, Step::NewInPlace);
        assert!(install(&mut a, &mut state, "alpha", V2, C2, Mode::Normal).is_err());
        drop((a, state));

        let (mut a, mut state) = env.restart();
        let notes = a.recover(&mut state).unwrap();

        assert_eq!(entries_of(&env.live("alpha")), with(V2, &[("keep.md", "mine")]));
        let backups = env.work_dirs_starting("backup-alpha-");
        assert_eq!(backups.len(), 1, "{:?}", env.work_entries());
        assert_eq!(fs::read_to_string(backups[0].join("references").join("b.md")).unwrap(), "my b");
        let text = &notes[0].text;
        assert!(text.contains("finished") && text.contains("references/b.md") && text.contains(&backups[0].display().to_string()), "{text}");
        assert!(!text.contains("keep.md"), "{text}");
    }

    #[test]
    fn the_user_is_told_which_of_their_files_were_in_the_way_without_a_long_list() {
        let backup = Path::new(r"C:\w\backup-alpha-1");
        let say = |paths: &[&str]| displaced_text(&paths.iter().map(|p| p.to_string()).collect::<Vec<_>>(), backup);
        assert_eq!(say(&["a.md"]), r"GitHub's new version has its own a.md, so the one you had added was moved to C:\w\backup-alpha-1");
        assert_eq!(say(&["a.md", "b.md"]), r"GitHub's new version has its own a.md and b.md, so the ones you had added were moved to C:\w\backup-alpha-1");
        assert!(say(&["a.md", "b.md", "c.md"]).contains(" own a.md, b.md and c.md, so the ones "));
        assert!(say(&["a.md", "b.md", "c.md", "d.md"]).contains(" own a.md, b.md and 2 more, so the ones "));
    }

    #[test]
    fn a_crash_after_a_late_edit_during_a_swap_puts_the_edited_folder_back() {
        let env = Env::new("crash-late-edit");
        let (mut a, mut state) = installed_v1(&env);
        let work = env.work.clone();
        a.hook = Some(Box::new(move |step| {
            if step == Step::NewInPlace {
                let old = fs::read_dir(&work).unwrap().flatten().find(|e| e.file_name().to_string_lossy().starts_with("old-")).unwrap().path();
                fs::write(old.join("SKILL.md"), EDITED).unwrap();
                return Err(local("simulated crash"));
            }
            Ok(())
        }));
        assert!(install(&mut a, &mut state, "alpha", V2, C2, Mode::Normal).is_err());
        drop((a, state));

        let (mut a, mut state) = env.restart();
        let notes = a.recover(&mut state).unwrap();
        assert!(notes[0].text.contains("local changes"), "{notes:?}");
        assert_eq!(fs::read_to_string(env.live("alpha").join("SKILL.md")).unwrap(), EDITED);
        assert_eq!(record(&state, "alpha").commit, C1);
        assert!(record(&state, "alpha").pending.is_none());
        a.sweep(&state);
        assert!(env.work_entries().is_empty(), "{:?}", env.work_entries());
        assert!(matches!(local_state(&mut a, &env, &state, "alpha"), LocalState::Dirty(_)), "the edit keeps holding future updates");
    }

    #[test]
    fn a_link_put_where_an_interrupted_update_left_no_folder_is_never_followed_and_the_skill_comes_back() {
        let env = Env::new("crash-link-put-back");
        let (mut a, mut state) = installed_v1(&env);
        add(&env.live("alpha"), MINE);
        crash_at(&mut a, Step::OldMoved);
        assert!(install(&mut a, &mut state, "alpha", V2, C2, Mode::Normal).is_err());
        drop((a, state));
        let outside = env.root.join("outside");
        add(&outside, V2);
        junction(&env.live("alpha"), &outside);

        let (mut a, mut state) = env.restart();
        let notes = a.recover(&mut state).unwrap();

        assert!(notes[0].text.contains("undid"), "{notes:?}");
        assert_eq!(entries_of(&env.live("alpha")), with(V1, MINE), "the skill and the files the user added are back");
        assert_eq!(record(&state, "alpha").commit, C1);
        assert!(record(&state, "alpha").pending.is_none());
        assert_eq!(entries_of(&outside), expected(V2), "what the link points to is not touched");
        let kept: Vec<Option<Kind>> = env.work_entries().iter().map(|n| kind_at(&env.work.join(n)).unwrap()).collect();
        assert_eq!(kept, [Some(Kind::Link)], "the link itself is kept aside");
    }

    #[test]
    fn a_crash_at_any_step_of_a_removal_never_loses_a_file() {
        for step in [Step::Marked, Step::OldMoved, Step::Settled] {
            let env = Env::new("crash-delete");
            let (mut a, mut state) = installed_v1(&env);
            crash_at(&mut a, step);
            assert!(a.delete(&mut state, REPO, "alpha").is_err(), "{step:?}");
            drop((a, state));

            let (mut a, mut state) = env.restart();
            a.recover(&mut state).unwrap();
            a.sweep(&state);
            let owned = state.repo(REPO).and_then(|r| r.skill("alpha")).cloned();
            match (env.live("alpha").exists(), owned) {
                (true, Some(rec)) => {
                    assert!(rec.pending.is_none(), "{step:?}");
                    assert_eq!(tree_of(&env.live("alpha")), expected(V1), "{step:?}");
                    assert_eq!(local_state(&mut a, &env, &state, "alpha"), LocalState::Clean, "{step:?}");
                }
                (false, None) => {}
                (live, rec) => panic!("{step:?}: folder present = {live}, record = {rec:?}"),
            }
            assert!(env.work_entries().is_empty(), "{step:?}: {:?}", env.work_entries());
        }
    }

    #[test]
    fn a_removal_interrupted_after_an_edit_restores_the_edited_folder() {
        let env = Env::new("crash-delete-edited");
        let (mut a, mut state) = installed_v1(&env);
        let work = env.work.clone();
        a.hook = Some(Box::new(move |step| {
            if step == Step::OldMoved {
                let trash = fs::read_dir(&work).unwrap().flatten().find(|e| e.file_name().to_string_lossy().starts_with("trash-")).unwrap().path();
                fs::write(trash.join("SKILL.md"), EDITED).unwrap();
                return Err(local("simulated crash"));
            }
            Ok(())
        }));
        assert!(a.delete(&mut state, REPO, "alpha").is_err());
        drop((a, state));

        let (mut a, mut state) = env.restart();
        let notes = a.recover(&mut state).unwrap();
        assert!(notes[0].text.contains("local changes"), "{notes:?}");
        assert_eq!(fs::read_to_string(env.live("alpha").join("SKILL.md")).unwrap(), EDITED);
        assert!(matches!(local_state(&mut a, &env, &state, "alpha"), LocalState::Dirty(_)), "the restored folder reads as edited, so it stays held");
    }

    #[test]
    fn recovery_leaves_a_folder_that_matches_no_known_version_alone() {
        let env = Env::new("crash-unknown");
        let (mut a, mut state) = installed_v1(&env);
        crash_at(&mut a, Step::Marked);
        assert!(install(&mut a, &mut state, "alpha", V2, C2, Mode::Normal).is_err());
        drop((a, state));
        // While the app is not running, the user edits the skill.
        edit_skill_file(&env);
        let (mut a, mut state) = env.restart();
        let notes = a.recover(&mut state).unwrap();
        assert!(notes[0].text.contains("does not match"), "{notes:?}");
        assert_eq!(fs::read_to_string(env.live("alpha").join("SKILL.md")).unwrap(), EDITED);
        assert!(matches!(local_state(&mut a, &env, &state, "alpha"), LocalState::Dirty(_)));
    }

    #[test]
    fn recovery_keeps_an_old_copy_when_the_user_recreated_the_folder_after_a_removal_crash() {
        let env = Env::new("crash-delete-recreated");
        let (mut a, mut state) = installed_v1(&env);
        crash_at(&mut a, Step::OldMoved);
        assert!(a.delete(&mut state, REPO, "alpha").is_err());
        drop((a, state));
        fs::create_dir_all(env.live("alpha")).unwrap();
        fs::write(env.live("alpha").join("SKILL.md"), EDITED).unwrap();
        let (mut a, mut state) = env.restart();
        a.recover(&mut state).unwrap();
        assert_eq!(fs::read_to_string(env.live("alpha").join("SKILL.md")).unwrap(), EDITED);
        assert_eq!(env.work_dirs_starting("backup-alpha-").len(), 1, "{:?}", env.work_entries());
        assert!(env.work_dirs_starting("trash-").is_empty());
    }
}
