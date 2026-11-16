//! Is a skill folder on disk still exactly what this app installed?
//!
//! Two strengths:
//! * `Check::Quick` lets a stored Windows fingerprint (file id, size, times and the file's USN) vouch for a file that
//!   still matches it, so no file is read. It is used only while the volume and its change journal are the ones the
//!   fingerprints were taken on, which is how edits made while the app was not running are still found.
//! * `Check::Full` reads and hashes every file. It runs before anything is changed or deleted, because a fingerprint
//!   alone can miss a write that has not been flushed yet (plan, spike S1b finding 4).
//!
//! Either way, a file that does not match its fingerprint is read and compared by hash: a file that was only touched
//! is clean, and only a real difference counts as an edit. When in doubt a file counts as edited, never as clean.

use std::collections::HashSet;
use std::path::Path;

use crate::fsx::join_rel;
use crate::pathsafe::fold;
use crate::reconcile::{DirtyInfo, LocalState, UnmanagedState};
use crate::state::{Fingerprint, ManagedFile, OwnedSkill, VolumeId};
use crate::tree::Skill;
use crate::win::fs::{FileHasher, Folder, Kind, probe};

const THE_FOLDER_ITSELF: &str = "(the folder itself)";
const CANNOT_LIST: &str = "(the folder cannot be listed)";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Check {
    Quick,
    Full,
}

/// Quick checks are allowed only on the same volume with the same change journal, and only if that journal exists.
pub fn check_for_volume(stored: Option<&VolumeId>, current: &VolumeId) -> Check {
    if current.journal != 0 && stored == Some(current) { Check::Quick } else { Check::Full }
}

/// A stored fingerprint vouches for a file only if it equals what the file reports now and that includes a real USN.
fn vouches(stored: Option<Fingerprint>, now: &Fingerprint) -> bool {
    now.usn != 0 && stored.as_ref() == Some(now)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub probed: usize,
    pub hashed: usize,
    pub hashed_bytes: u64,
}

impl Stats {
    pub fn add(&mut self, other: Stats) {
        self.probed += other.probed;
        self.hashed += other.hashed;
        self.hashed_bytes += other.hashed_bytes;
    }
}

#[derive(Debug)]
pub struct Outcome {
    pub local: LocalState,
    /// Files whose content matched but whose stored fingerprint was missing or out of date, with the one to store.
    pub refreshed: Vec<(String, Fingerprint)>,
    pub stats: Stats,
}

fn dirty(info: DirtyInfo, refreshed: Vec<(String, Fingerprint)>, stats: Stats) -> Outcome {
    let local = if info.is_empty() { LocalState::Clean } else { LocalState::Dirty(info) };
    Outcome { local, refreshed, stats }
}

/// Compares the folder at `dir` with what was installed (`skill.files`).
pub fn verify_skill(dir: &Path, skill: &OwnedSkill, check: Check, hasher: &mut FileHasher) -> Outcome {
    verify_files(dir, &skill.files, check, hasher)
}

/// Compares the folder at `dir` with a list of files, which need not belong to a recorded skill.
pub fn verify_files(dir: &Path, files: &[ManagedFile], check: Check, hasher: &mut FileHasher) -> Outcome {
    let mut stats = Stats::default();
    stats.probed += 1;
    // One handle serves to check the folder and, at the end, to list it.
    let folder = match Folder::open(dir) {
        Ok(None) => return Outcome { local: LocalState::FolderMissing, refreshed: Vec::new(), stats },
        Ok(Some(f)) if f.kind != Kind::Dir => {
            let info = DirtyInfo { modified: vec![THE_FOLDER_ITSELF.to_string()], ..DirtyInfo::default() };
            return dirty(info, Vec::new(), stats);
        }
        Ok(Some(f)) => f,
        Err(_) => {
            let info = DirtyInfo { modified: vec![format!("{THE_FOLDER_ITSELF} (cannot be read)")], ..DirtyInfo::default() };
            return dirty(info, Vec::new(), stats);
        }
    };

    let mut info = DirtyInfo::default();
    let mut refreshed = Vec::new();
    for file in files {
        let path = join_rel(dir, &file.path);
        stats.probed += 1;
        let first = match probe(&path) {
            Ok(Some(p)) => p,
            Ok(None) => {
                info.missing.push(file.path.clone());
                continue;
            }
            Err(_) => {
                info.modified.push(format!("{} (cannot be read)", file.path));
                continue;
            }
        };
        if first.kind != Kind::File {
            info.modified.push(format!("{} (replaced by a folder or link)", file.path));
            continue;
        }
        if check == Check::Quick && vouches(file.fp, &first.fp) {
            continue;
        }
        if first.fp.size != file.size {
            info.modified.push(file.path.clone());
            continue;
        }
        stats.hashed += 1;
        stats.hashed_bytes += file.size;
        match hasher.blob(&path) {
            Ok(h) if h == file.blob => {
                // Hash, then look again: a file that moved while it was being read has no trustworthy fingerprint.
                stats.probed += 1;
                match probe(&path) {
                    Ok(Some(second)) if second.kind == Kind::File && second.fp == first.fp => {
                        if file.fp != Some(first.fp) {
                            refreshed.push((file.path.clone(), first.fp));
                        }
                    }
                    _ => info.modified.push(format!("{} (still changing)", file.path)),
                }
            }
            Ok(_) => info.modified.push(file.path.clone()),
            Err(_) => info.modified.push(format!("{} (cannot be read)", file.path)),
        }
    }

    match folder.list_files() {
        Ok(found) => {
            let known: HashSet<String> = files.iter().map(|f| fold(&f.path)).collect();
            info.extra.extend(found.into_iter().filter(|f| !known.contains(&fold(&f.rel))).map(|f| f.rel));
        }
        Err(_) => info.modified.push(CANNOT_LIST.to_string()),
    }
    info.modified.sort();
    info.missing.sort();
    info.extra.sort();
    dirty(info, refreshed, stats)
}

/// Stores the fingerprint of every file that is on disk, equals its recorded blob and holds still while it is read.
/// Files that do not qualify get no fingerprint, so the next check reads them.
pub fn capture(dir: &Path, files: &mut [ManagedFile], hasher: &mut FileHasher) {
    for file in files {
        file.fp = None;
        let path = join_rel(dir, &file.path);
        let Ok(Some(first)) = probe(&path) else { continue };
        if first.kind != Kind::File || first.fp.size != file.size {
            continue;
        }
        if !matches!(hasher.blob(&path), Ok(h) if h == file.blob) {
            continue;
        }
        if let Ok(Some(second)) = probe(&path) {
            if second.kind == Kind::File && second.fp == first.fp {
                file.fp = Some(first.fp);
            }
        }
    }
}

/// Copies refreshed fingerprints into the skill record. Returns true when anything changed.
pub fn apply_refresh(skill: &mut OwnedSkill, refreshed: &[(String, Fingerprint)]) -> bool {
    let mut changed = false;
    for (path, fp) in refreshed {
        if let Some(file) = skill.files.iter_mut().find(|f| f.path == *path) {
            if file.fp != Some(*fp) {
                file.fp = Some(*fp);
                changed = true;
            }
        }
    }
    changed
}

/// Is an existing folder that nobody owns exactly what upstream has? Only then may it be adopted: the files are
/// the user's, and adopting changes no byte of them.
pub fn compare_unmanaged(dir: &Path, skill: &Skill, hasher: &mut FileHasher) -> UnmanagedState {
    let Ok(Some(folder)) = Folder::open(dir) else { return UnmanagedState::Different };
    let Ok(found) = folder.list_files() else { return UnmanagedState::Different };
    let wanted: HashSet<String> = skill.files.iter().map(|f| fold(&f.path)).collect();
    if found.len() != skill.files.len() || found.iter().any(|f| f.kind != Kind::File || !wanted.contains(&fold(&f.rel))) {
        return UnmanagedState::Different;
    }
    for file in &skill.files {
        let path = join_rel(dir, &file.path);
        if !matches!(probe(&path), Ok(Some(p)) if p.kind == Kind::File && p.fp.size == file.size) {
            return UnmanagedState::Different;
        }
        if !matches!(hasher.blob(&path), Ok(h) if h == file.blob) {
            return UnmanagedState::Different;
        }
    }
    UnmanagedState::Identical
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsx::unique_suffix;
    use crate::hash::blob_hex;
    use crate::tree::SkillFile;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tss-verify-{name}-{}", unique_suffix()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(dir: &Path, rel: &str, content: &str) {
        let p = join_rel(dir, rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    /// Writes the files and returns the record an install would have stored, fingerprints included.
    fn install(dir: &Path, files: &[(&str, &str)]) -> OwnedSkill {
        let mut managed = Vec::new();
        for (path, content) in files {
            write(dir, path, content);
            managed.push(ManagedFile { path: path.to_string(), blob: blob_hex(content.as_bytes()), size: content.len() as u64, fp: None });
        }
        capture(dir, &mut managed, &mut FileHasher::default());
        OwnedSkill { name: "alpha".into(), commit: "c".repeat(40), files: managed, pending: None }
    }

    const FILES: [(&str, &str); 3] = [("SKILL.md", "# alpha\n"), ("scripts/run.ps1", "echo hi\n"), ("refs/a.md", "reference\n")];

    fn verify(dir: &Path, skill: &OwnedSkill, check: Check) -> Outcome {
        verify_skill(dir, skill, check, &mut FileHasher::default())
    }

    fn info(o: &Outcome) -> DirtyInfo {
        match &o.local {
            LocalState::Dirty(i) => i.clone(),
            other => panic!("expected a dirty skill, got {other:?}"),
        }
    }

    #[test]
    fn a_pristine_install_passes_the_quick_check_without_reading_any_file() {
        let dir = temp("quick");
        let skill = install(&dir, &FILES);
        assert!(skill.files.iter().all(|f| f.fp.is_some()), "every installed file gets a fingerprint");
        let o = verify(&dir, &skill, Check::Quick);
        assert_eq!(o.local, LocalState::Clean);
        assert_eq!(o.stats.hashed, 0, "nothing is read when the fingerprints vouch for every file");
        assert!(o.refreshed.is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_full_check_reads_every_file_and_agrees() {
        let dir = temp("full");
        let skill = install(&dir, &FILES);
        let o = verify(&dir, &skill, Check::Full);
        assert_eq!(o.local, LocalState::Clean);
        assert_eq!(o.stats.hashed, 3);
        assert!(o.refreshed.is_empty(), "equal fingerprints need no refresh");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_same_size_edit_with_the_old_timestamp_is_caught_by_both_checks() {
        let dir = temp("sneaky");
        let skill = install(&dir, &FILES);
        let path = dir.join("SKILL.md");
        let before = fs::metadata(&path).unwrap().modified().unwrap();
        fs::write(&path, "# ALPHA\n").unwrap();
        fs::OpenOptions::new().write(true).open(&path).unwrap().set_modified(before).unwrap();
        for check in [Check::Quick, Check::Full] {
            let o = verify(&dir, &skill, check);
            assert_eq!(info(&o).modified, vec!["SKILL.md".to_string()], "{check:?}");
            assert!(o.stats.hashed >= 1);
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn touching_or_rewriting_identical_content_is_clean_and_refreshes_the_fingerprint() {
        let dir = temp("touch");
        let mut skill = install(&dir, &FILES);
        let path = dir.join("scripts").join("run.ps1");
        fs::OpenOptions::new().write(true).open(&path).unwrap().set_modified(SystemTime::now() - Duration::from_secs(7200)).unwrap();
        fs::write(dir.join("refs").join("a.md"), "reference\n").unwrap();

        let o = verify(&dir, &skill, Check::Quick);
        assert_eq!(o.local, LocalState::Clean, "only a real difference in content is an edit");
        assert_eq!(o.stats.hashed, 2, "just the two files whose fingerprint moved are read");
        let mut paths: Vec<&str> = o.refreshed.iter().map(|(p, _)| p.as_str()).collect();
        paths.sort();
        assert_eq!(paths, vec!["refs/a.md", "scripts/run.ps1"]);

        assert!(apply_refresh(&mut skill, &o.refreshed));
        let again = verify(&dir, &skill, Check::Quick);
        assert_eq!(again.local, LocalState::Clean);
        assert_eq!(again.stats.hashed, 0, "after the refresh the quick check is quick again");
        assert!(!apply_refresh(&mut skill, &o.refreshed), "applying the same refresh twice changes nothing");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_files_and_missing_folders_are_reported() {
        let dir = temp("missing");
        let skill = install(&dir, &FILES);
        fs::remove_file(dir.join("refs").join("a.md")).unwrap();
        let o = verify(&dir, &skill, Check::Quick);
        assert_eq!(info(&o).missing, vec!["refs/a.md".to_string()]);

        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(verify(&dir, &skill, Check::Quick).local, LocalState::FolderMissing);
        assert_eq!(verify(&dir, &skill, Check::Full).local, LocalState::FolderMissing);
    }

    #[test]
    fn extra_files_are_reported_wherever_they_are_and_any_case() {
        let dir = temp("extra");
        let skill = install(&dir, &FILES);
        write(&dir, "notes.txt", "mine");
        write(&dir, "scripts/deep/er/more.txt", "mine too");
        let o = verify(&dir, &skill, Check::Quick);
        assert_eq!(info(&o).extra, vec!["notes.txt".to_string(), "scripts/deep/er/more.txt".to_string()]);
        assert_eq!(o.stats.hashed, 0, "extras are found by listing, not by reading");

        fs::remove_file(dir.join("notes.txt")).unwrap();
        fs::remove_dir_all(dir.join("scripts").join("deep")).unwrap();
        assert_eq!(verify(&dir, &skill, Check::Quick).local, LocalState::Clean);
        fs::create_dir_all(dir.join("an-empty-folder")).unwrap();
        assert_eq!(verify(&dir, &skill, Check::Quick).local, LocalState::Clean, "an empty folder holds nothing of anyone's");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_different_size_is_an_edit_found_without_reading_the_file() {
        let dir = temp("size");
        let skill = install(&dir, &FILES);
        write(&dir, "SKILL.md", "# alpha, much longer now\n");
        let o = verify(&dir, &skill, Check::Full);
        assert_eq!(info(&o).modified, vec!["SKILL.md".to_string()]);
        assert_eq!(o.stats.hashed, 2, "the two untouched files are read, the resized one is not");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_fingerprint_without_a_usn_never_vouches() {
        let fp = Fingerprint { file_id: [1, 2], size: 3, write: 4, change: 5, usn: 0 };
        assert!(!vouches(Some(fp), &fp), "no journal entry means no proof");
        let with = Fingerprint { usn: 9, ..fp };
        assert!(vouches(Some(with), &with));
        assert!(!vouches(Some(with), &Fingerprint { usn: 10, ..with }));
        assert!(!vouches(None, &with));
        for moved in
            [Fingerprint { file_id: [1, 3], ..with }, Fingerprint { size: 4, ..with }, Fingerprint { write: 5, ..with }, Fingerprint { change: 6, ..with }]
        {
            assert!(!vouches(Some(with), &moved));
        }
    }

    #[test]
    fn a_stored_fingerprint_from_another_state_forces_a_read() {
        let dir = temp("stale-fp");
        let mut skill = install(&dir, &FILES);
        skill.files[0].fp = skill.files[0].fp.map(|f| Fingerprint { usn: f.usn + 1, ..f });
        skill.files[1].fp = None;
        let o = verify(&dir, &skill, Check::Quick);
        assert_eq!(o.local, LocalState::Clean);
        assert_eq!(o.stats.hashed, 2);
        assert_eq!(o.refreshed.len(), 2);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn quick_checks_need_the_same_volume_and_a_journal() {
        let v = VolumeId { serial: 7, journal: 99 };
        assert_eq!(check_for_volume(Some(&v), &v), Check::Quick);
        assert_eq!(check_for_volume(None, &v), Check::Full, "nothing stored yet");
        assert_eq!(check_for_volume(Some(&VolumeId { journal: 98, ..v }), &v), Check::Full, "the journal was recreated");
        assert_eq!(check_for_volume(Some(&VolumeId { serial: 8, ..v }), &v), Check::Full, "another volume");
        let none = VolumeId { serial: 7, journal: 0 };
        assert_eq!(check_for_volume(Some(&none), &none), Check::Full, "no journal at all");
    }

    #[test]
    fn a_folder_replaced_by_a_file_or_a_link_is_not_clean() {
        let base = temp("replaced");
        let skill = install(&base.join("alpha"), &FILES);
        let dir = base.join("alpha");
        fs::remove_dir_all(&dir).unwrap();
        fs::write(&dir, "I am a file now").unwrap();
        assert_eq!(info(&verify(&dir, &skill, Check::Quick)).modified, vec![THE_FOLDER_ITSELF.to_string()]);
        fs::remove_file(&dir).unwrap();

        let outside = temp("replaced-outside");
        install(&outside, &FILES);
        let made = std::process::Command::new("cmd").args(["/C", "mklink", "/J"]).arg(&dir).arg(&outside).output().unwrap().status.success();
        assert!(made);
        assert_eq!(info(&verify(&dir, &skill, Check::Full)).modified, vec![THE_FOLDER_ITSELF.to_string()], "a junction is never treated as the folder");
        fs::remove_dir(&dir).unwrap();
        fs::remove_dir_all(base).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn a_folder_or_link_in_place_of_a_file_is_an_edit() {
        let dir = temp("file-replaced");
        let skill = install(&dir, &FILES);
        fs::remove_file(dir.join("refs").join("a.md")).unwrap();
        fs::create_dir_all(dir.join("refs").join("a.md")).unwrap();
        let o = verify(&dir, &skill, Check::Quick);
        assert_eq!(info(&o).modified, vec!["refs/a.md (replaced by a folder or link)".to_string()]);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_file_that_cannot_be_read_is_an_edit_in_the_full_check() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = temp("locked");
        let skill = install(&dir, &FILES);
        // Holding the file open without sharing makes reading impossible, as some other program might.
        let _lock = fs::OpenOptions::new().read(true).share_mode(0).open(dir.join("SKILL.md")).unwrap();
        let o = verify(&dir, &skill, Check::Full);
        assert_eq!(info(&o).modified, vec!["SKILL.md (cannot be read)".to_string()]);
        drop(_lock);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn capture_skips_files_that_do_not_match_what_was_recorded() {
        let dir = temp("capture");
        write(&dir, "SKILL.md", "actual");
        write(&dir, "b.md", "ok");
        let mut files = vec![
            ManagedFile { path: "SKILL.md".into(), blob: blob_hex(b"something else"), size: 6, fp: None },
            ManagedFile { path: "b.md".into(), blob: blob_hex(b"ok"), size: 2, fp: None },
            ManagedFile { path: "gone.md".into(), blob: blob_hex(b"x"), size: 1, fp: None },
        ];
        capture(&dir, &mut files, &mut FileHasher::default());
        assert!(files[0].fp.is_none() && files[2].fp.is_none());
        assert!(files[1].fp.is_some());
        fs::remove_dir_all(dir).unwrap();
    }

    fn upstream(files: &[(&str, &str)]) -> Skill {
        Skill {
            name: "alpha".into(),
            files: files.iter().map(|(p, c)| SkillFile { path: p.to_string(), blob: blob_hex(c.as_bytes()), size: c.len() as u64 }).collect(),
        }
    }

    #[test]
    fn an_identical_unmanaged_folder_is_recognised_and_any_difference_is_not() {
        let dir = temp("unmanaged");
        let up = upstream(&FILES);
        for (p, c) in FILES {
            write(&dir, p, c);
        }
        let mut h = FileHasher::default();
        assert_eq!(compare_unmanaged(&dir, &up, &mut h), UnmanagedState::Identical);

        write(&dir, "extra.txt", "x");
        assert_eq!(compare_unmanaged(&dir, &up, &mut h), UnmanagedState::Different, "an extra file");
        fs::remove_file(dir.join("extra.txt")).unwrap();

        write(&dir, "SKILL.md", "# ALPHA\n");
        assert_eq!(compare_unmanaged(&dir, &up, &mut h), UnmanagedState::Different, "same size, other content");
        write(&dir, "SKILL.md", "# alpha\n");
        assert_eq!(compare_unmanaged(&dir, &up, &mut h), UnmanagedState::Identical);

        fs::remove_file(dir.join("refs").join("a.md")).unwrap();
        assert_eq!(compare_unmanaged(&dir, &up, &mut h), UnmanagedState::Different, "a missing file");
        assert_eq!(compare_unmanaged(&dir.join("nope"), &up, &mut h), UnmanagedState::Different, "no folder at all");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_unmanaged_junction_is_never_identical_even_if_its_target_is() {
        let base = temp("unmanaged-link");
        let target = base.join("target");
        for (p, c) in FILES {
            write(&target, p, c);
        }
        let link = base.join("alpha");
        let made = std::process::Command::new("cmd").args(["/C", "mklink", "/J"]).arg(&link).arg(&target).output().unwrap().status.success();
        assert!(made);
        assert_eq!(compare_unmanaged(&link, &upstream(&FILES), &mut FileHasher::default()), UnmanagedState::Different);
        fs::remove_dir(&link).unwrap();
        fs::remove_dir_all(base).unwrap();
    }
}
