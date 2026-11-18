//! Is a skill folder on disk still what this app installed?
//!
//! Two strengths:
//! * `Check::Quick` lets a stored Windows fingerprint (file id, size, times and the file's USN) vouch for a file that
//!   still matches it, so no file is read. It is used only while the volume and its change journal are the ones the
//!   fingerprints were taken on, which is how edits made while the app was not running are still found.
//! * `Check::Full` reads and hashes every file. It runs before anything is changed or deleted, because a fingerprint
//!   alone can miss a write that has not been flushed yet (plan, spike S1b finding 4).
//!
//! Either way, a file that does not match its fingerprint is read and compared: a file that was only touched is clean,
//! and only a real difference counts as an edit. Text is compared as [`crate::text`] says, so a copy saved with other
//! line breaks or blank lines is clean too. When in doubt a file counts as edited, never as clean.
//!
//! Only the files that came from GitHub are looked at. Whatever the user added to a skill's folder is theirs: it is
//! never read, never counts as an edit and never holds a change.

use std::collections::HashMap;
use std::io;
use std::path::Path;

use crate::fsx::join_rel;
use crate::pathsafe::fold;
use crate::reconcile::{DirtyInfo, LocalState};
use crate::state::{Fingerprint, ManagedFile, OwnedSkill, VolumeId};
use crate::text::{self, TEXT_LIMIT};
use crate::tree::Skill;
use crate::win::fs::{FileHasher, Kind, Probe, kind_at, probe};

const THE_FOLDER_ITSELF: &str = "(the folder itself)";

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
    /// Files that are the same but whose stored fingerprint, or what they are compared by, is missing or out of date.
    pub refreshed: Vec<Refresh>,
    pub stats: Stats,
}

/// What to store for one file that was found unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refresh {
    pub path: String,
    pub fp: Fingerprint,
    /// Learned from the file, when the record did not have it yet.
    pub canon: Option<String>,
}

fn dirty(info: DirtyInfo, refreshed: Vec<Refresh>, stats: Stats) -> Outcome {
    let local = if info.is_empty() { LocalState::Clean } else { LocalState::Dirty(info) };
    Outcome { local, refreshed, stats }
}

fn is_text(file: &ManagedFile) -> bool {
    file.canon.as_deref().is_some_and(|c| !c.is_empty())
}

/// A file of another size can still be the same only when it is text short enough to be compared as text.
fn size_rules_out(file: &ManagedFile, size: u64) -> bool {
    size != file.size && !(is_text(file) && size <= TEXT_LIMIT)
}

/// How a file on disk compares with its record.
enum Content {
    Differs,
    /// The same. Carries what the file is compared by when the record did not have it yet.
    Same(Option<String>),
}

fn compare_file(path: &Path, file: &ManagedFile, hasher: &mut FileHasher) -> io::Result<Content> {
    let keep = if file.canon.as_deref() == Some("") { 0 } else { TEXT_LIMIT };
    let read = hasher.read(path, keep, u64::MAX)?;
    if read.blob == file.blob {
        // The bytes are exactly the recorded ones, so what they are compared by is learned from them.
        let learned = file.canon.is_none().then(|| read.bytes.as_deref().map(text::canon).unwrap_or_default());
        return Ok(Content::Same(learned));
    }
    let same_text = is_text(file) && read.bytes.as_deref().is_some_and(|bytes| file.canon.as_deref() == Some(text::canon(bytes).as_str()));
    Ok(if same_text { Content::Same(None) } else { Content::Differs })
}

/// What is at a path below a folder.
#[derive(Clone, Copy, Debug)]
pub enum Spot {
    Missing,
    /// A folder on the way is a link or a file, so the path is not looked at.
    Blocked,
    Unreadable,
    Found(Probe),
}

/// Looks at paths below one folder without following a link anywhere on the way. Windows follows a link in every part
/// of a path but the last, so each folder on the way is looked at first, once, from the top.
pub struct Spots<'a> {
    dir: &'a Path,
    /// By folded path: `None` for a plain folder, otherwise why nothing below it can be looked at.
    ways: HashMap<String, Option<Spot>>,
}

impl<'a> Spots<'a> {
    pub fn new(dir: &'a Path) -> Spots<'a> {
        Spots { dir, ways: HashMap::new() }
    }

    pub fn find(&mut self, rel: &str, stats: &mut Stats) -> Spot {
        for (at, _) in rel.match_indices('/') {
            let folder = &rel[..at];
            let way = match self.ways.get(&fold(folder)) {
                Some(way) => *way,
                None => {
                    stats.probed += 1;
                    let way = match kind_at(&join_rel(self.dir, folder)) {
                        Ok(Some(Kind::Dir)) => None,
                        Ok(Some(_)) => Some(Spot::Blocked),
                        Ok(None) => Some(Spot::Missing),
                        Err(_) => Some(Spot::Unreadable),
                    };
                    self.ways.insert(fold(folder), way);
                    way
                }
            };
            if let Some(spot) = way {
                return spot;
            }
        }
        stats.probed += 1;
        match probe(&join_rel(self.dir, rel)) {
            Ok(Some(p)) => Spot::Found(p),
            Ok(None) => Spot::Missing,
            Err(_) => Spot::Unreadable,
        }
    }
}

/// Compares the folder at `dir` with what was installed (`skill.files`).
pub fn verify_skill(dir: &Path, skill: &OwnedSkill, check: Check, hasher: &mut FileHasher) -> Outcome {
    verify_files(dir, &skill.files, check, hasher)
}

/// Compares the folder at `dir` with a list of files, which need not belong to a recorded skill. Nothing else in the
/// folder is looked at.
pub fn verify_files(dir: &Path, files: &[ManagedFile], check: Check, hasher: &mut FileHasher) -> Outcome {
    let mut stats = Stats::default();
    stats.probed += 1;
    match kind_at(dir) {
        Ok(None) => return Outcome { local: LocalState::FolderMissing, refreshed: Vec::new(), stats },
        Ok(Some(Kind::Dir)) => {}
        Ok(Some(_)) => {
            let info = DirtyInfo { modified: vec![THE_FOLDER_ITSELF.to_string()], ..DirtyInfo::default() };
            return dirty(info, Vec::new(), stats);
        }
        Err(_) => {
            let info = DirtyInfo { modified: vec![format!("{THE_FOLDER_ITSELF} (cannot be read)")], ..DirtyInfo::default() };
            return dirty(info, Vec::new(), stats);
        }
    }

    let mut info = DirtyInfo::default();
    let mut refreshed = Vec::new();
    let mut spots = Spots::new(dir);
    for file in files {
        let path = join_rel(dir, &file.path);
        let first = match spots.find(&file.path, &mut stats) {
            Spot::Found(p) => p,
            Spot::Missing => {
                info.missing.push(file.path.clone());
                continue;
            }
            Spot::Blocked => {
                info.modified.push(format!("{} (a folder on its way is a link or a file)", file.path));
                continue;
            }
            Spot::Unreadable => {
                info.modified.push(format!("{} (cannot be read)", file.path));
                continue;
            }
        };
        if first.kind != Kind::File {
            info.modified.push(format!("{} (replaced by a folder or link)", file.path));
            continue;
        }
        if check == Check::Quick && file.canon.is_some() && vouches(file.fp, &first.fp) {
            continue;
        }
        if size_rules_out(file, first.fp.size) {
            info.modified.push(file.path.clone());
            continue;
        }
        stats.hashed += 1;
        stats.hashed_bytes += first.fp.size;
        match compare_file(&path, file, hasher) {
            Ok(Content::Same(canon)) => {
                // Read, then look again: a file that moved while it was being read has no trustworthy fingerprint.
                stats.probed += 1;
                match probe(&path) {
                    Ok(Some(second)) if second.kind == Kind::File && second.fp == first.fp => {
                        if file.fp != Some(first.fp) || canon.is_some() {
                            refreshed.push(Refresh { path: file.path.clone(), fp: first.fp, canon });
                        }
                    }
                    _ => info.modified.push(format!("{} (still changing)", file.path)),
                }
            }
            Ok(Content::Differs) => info.modified.push(file.path.clone()),
            Err(_) => info.modified.push(format!("{} (cannot be read)", file.path)),
        }
    }

    info.modified.sort();
    info.missing.sort();
    dirty(info, refreshed, stats)
}

/// Stores the fingerprint of every file that is on disk, is the same as its record and holds still while it is read,
/// and what each file is compared by when the record did not have it yet. Files that do not qualify get no
/// fingerprint, so the next check reads them.
pub fn capture(dir: &Path, files: &mut [ManagedFile], hasher: &mut FileHasher) {
    for file in files {
        file.fp = None;
        let path = join_rel(dir, &file.path);
        let Ok(Some(first)) = probe(&path) else { continue };
        if first.kind != Kind::File || size_rules_out(file, first.fp.size) {
            continue;
        }
        let Ok(Content::Same(learned)) = compare_file(&path, file, hasher) else { continue };
        if learned.is_some() {
            file.canon = learned;
        }
        if let Ok(Some(second)) = probe(&path) {
            if second.kind == Kind::File && second.fp == first.fp {
                file.fp = Some(first.fp);
            }
        }
    }
}

/// Copies what a check found into the skill record. Returns true when anything changed.
pub fn apply_refresh(skill: &mut OwnedSkill, refreshed: &[Refresh]) -> bool {
    let mut changed = false;
    for r in refreshed {
        if let Some(file) = skill.files.iter_mut().find(|f| f.path == r.path) {
            if file.fp != Some(r.fp) {
                file.fp = Some(r.fp);
                changed = true;
            }
            if r.canon.is_some() && file.canon != r.canon {
                file.canon = r.canon.clone();
                changed = true;
            }
        }
    }
    changed
}

/// How an existing folder that nobody owns compares with upstream.
#[derive(Debug, PartialEq, Eq)]
pub enum Likeness {
    Same,
    Different,
    /// Everything else is the same, but these files (by their path upstream) are text whose bytes differ from the
    /// copy on GitHub. Whether their lines are the same is known only from that copy.
    NeedsText(Vec<String>),
}

/// Is an existing folder that nobody owns the same as upstream? Only then may it be adopted: the files are the user's,
/// and adopting changes no byte of them. Only upstream's files are compared; anything else in the folder is the user's
/// and stays as it is. `theirs` holds what the copies on GitHub of some files are compared by ([`text::canon`]), by path.
pub fn compare_unmanaged(dir: &Path, skill: &Skill, theirs: &HashMap<String, String>, hasher: &mut FileHasher) -> Likeness {
    if !matches!(kind_at(dir), Ok(Some(Kind::Dir))) {
        return Likeness::Different;
    }
    let mut spots = Spots::new(dir);
    let mut stats = Stats::default();
    let mut needs = Vec::new();
    for file in &skill.files {
        let path = join_rel(dir, &file.path);
        let Spot::Found(p) = spots.find(&file.path, &mut stats) else { return Likeness::Different };
        let may_be_text = file.size <= TEXT_LIMIT && p.fp.size <= TEXT_LIMIT;
        if p.kind != Kind::File || (p.fp.size != file.size && !may_be_text) {
            return Likeness::Different;
        }
        let Ok(read) = hasher.read(&path, if may_be_text { TEXT_LIMIT } else { 0 }, u64::MAX) else { return Likeness::Different };
        if read.blob == file.blob {
            continue;
        }
        let mine = read.bytes.as_deref().map(text::canon).unwrap_or_default();
        if mine.is_empty() {
            return Likeness::Different;
        }
        match theirs.get(&file.path) {
            Some(canon) if *canon == mine => {}
            Some(_) => return Likeness::Different,
            None => needs.push(file.path.clone()),
        }
    }
    if needs.is_empty() { Likeness::Same } else { Likeness::NeedsText(needs) }
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
            managed.push(ManagedFile { path: path.to_string(), blob: blob_hex(content.as_bytes()), size: content.len() as u64, fp: None, canon: None });
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
        let mut paths: Vec<&str> = o.refreshed.iter().map(|r| r.path.as_str()).collect();
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
    fn files_the_user_added_are_not_an_edit() {
        let dir = temp("added");
        let skill = install(&dir, &FILES);
        write(&dir, "notes.txt", "mine");
        write(&dir, "scripts/deep/er/more.txt", "mine too");
        fs::create_dir_all(dir.join("an-empty-folder")).unwrap();
        for check in [Check::Quick, Check::Full] {
            let o = verify(&dir, &skill, check);
            assert_eq!(o.local, LocalState::Clean, "{check:?}: only what came from GitHub is compared");
            assert_eq!(o.stats.hashed, if check == Check::Quick { 0 } else { 3 }, "{check:?}: an added file is never read");
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_subfolder_replaced_by_a_link_is_an_edit_and_is_not_read_through() {
        let dir = temp("sub-link");
        let skill = install(&dir, &FILES);
        let outside = temp("sub-link-outside");
        write(&outside, "a.md", "reference\n");
        fs::remove_dir_all(dir.join("refs")).unwrap();
        let made = std::process::Command::new("cmd").args(["/C", "mklink", "/J"]).arg(dir.join("refs")).arg(&outside).output().unwrap().status.success();
        assert!(made);
        for check in [Check::Quick, Check::Full] {
            let o = verify(&dir, &skill, check);
            assert_eq!(info(&o).modified, vec!["refs/a.md (a folder on its way is a link or a file)".to_string()], "{check:?}");
            assert_eq!(o.stats.hashed, if check == Check::Quick { 0 } else { 2 }, "{check:?}: nothing is read through the link");
        }
        fs::remove_dir(dir.join("refs")).unwrap();
        write(&dir, "refs", "a file where the folder was");
        assert_eq!(info(&verify(&dir, &skill, Check::Full)).modified, vec!["refs/a.md (a folder on its way is a link or a file)".to_string()]);
        fs::remove_dir_all(dir).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn a_different_size_is_an_edit_found_without_reading_the_file_unless_it_is_text() {
        let dir = temp("size");
        let skill = install(&dir, &[("SKILL.md", "# alpha\n"), ("logo.bin", "\0\x01\x02\n")]);
        write(&dir, "logo.bin", "\0\x01\x02\r\n");
        let o = verify(&dir, &skill, Check::Full);
        assert_eq!(info(&o).modified, vec!["logo.bin".to_string()]);
        assert_eq!(o.stats.hashed, 1, "the resized file that is not text is not read");

        write(&dir, "logo.bin", "\0\x01\x02\n");
        write(&dir, "SKILL.md", "# alpha, much longer now\n");
        let o = verify(&dir, &skill, Check::Full);
        assert_eq!(info(&o).modified, vec!["SKILL.md".to_string()]);
        assert_eq!(o.stats.hashed, 2, "text of another size may still have the same lines, so it is read");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn line_breaks_and_blank_lines_are_not_edits() {
        let dir = temp("crlf");
        let mut skill = install(&dir, &FILES);
        assert!(skill.files.iter().all(|f| f.canon.as_ref().is_some_and(|c| c.len() == 40)), "the install records how text is compared");
        write(&dir, "SKILL.md", "\r\n# alpha\r\n\r\n");
        write(&dir, "refs/a.md", "reference");
        for check in [Check::Quick, Check::Full] {
            let o = verify(&dir, &skill, check);
            assert_eq!(o.local, LocalState::Clean, "{check:?}");
        }
        let o = verify(&dir, &skill, Check::Quick);
        let mut paths: Vec<&str> = o.refreshed.iter().map(|r| r.path.as_str()).collect();
        paths.sort();
        assert_eq!(paths, ["SKILL.md", "refs/a.md"], "the new copies are vouched for from now on");
        assert!(apply_refresh(&mut skill, &o.refreshed));
        assert_eq!(verify(&dir, &skill, Check::Quick).stats.hashed, 0);
        assert_eq!(verify(&dir, &skill, Check::Full).local, LocalState::Clean);

        write(&dir, "SKILL.md", "\r\n# alpha, edited\r\n\r\n");
        for check in [Check::Quick, Check::Full] {
            assert_eq!(info(&verify(&dir, &skill, check)).modified, vec!["SKILL.md".to_string()], "{check:?}");
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_record_from_an_older_version_learns_how_its_files_are_compared_once() {
        let dir = temp("learn");
        let mut skill = install(&dir, &FILES);
        let expected: Vec<Option<String>> = skill.files.iter().map(|f| f.canon.clone()).collect();
        skill.files.iter_mut().for_each(|f| f.canon = None);

        let o = verify(&dir, &skill, Check::Quick);
        assert_eq!(o.local, LocalState::Clean);
        assert_eq!(o.stats.hashed, 3, "each file is read once to learn it, fingerprint or not");
        assert!(apply_refresh(&mut skill, &o.refreshed));
        assert_eq!(skill.files.iter().map(|f| f.canon.clone()).collect::<Vec<_>>(), expected);
        assert_eq!(verify(&dir, &skill, Check::Quick).stats.hashed, 0);

        // Until it is learned, nothing but the exact bytes can prove a file untouched.
        skill.files.iter_mut().for_each(|f| f.canon = None);
        write(&dir, "SKILL.md", "# alpha\r\n");
        assert_eq!(info(&verify(&dir, &skill, Check::Full)).modified, vec!["SKILL.md".to_string()]);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_file_that_is_not_text_is_compared_byte_for_byte() {
        let dir = temp("binary");
        let skill = install(&dir, &[("SKILL.md", "# alpha\n"), ("data.bin", "a\0\r\nb\r\n")]);
        assert_eq!(skill.files[1].canon.as_deref(), Some(""));
        write(&dir, "data.bin", "a\0\nb\n\n");
        assert_eq!(info(&verify(&dir, &skill, Check::Full)).modified, vec!["data.bin".to_string()]);
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
        let file = |path: &str, content: &[u8], size| ManagedFile { path: path.into(), blob: blob_hex(content), size, fp: None, canon: None };
        let mut files = vec![file("SKILL.md", b"something else", 6), file("b.md", b"ok", 2), file("gone.md", b"x", 1)];
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
        let none = HashMap::new();
        assert_eq!(compare_unmanaged(&dir, &up, &none, &mut h), Likeness::Same);

        write(&dir, "extra.txt", "x");
        write(&dir, "refs/deeper/mine.md", "x");
        assert_eq!(compare_unmanaged(&dir, &up, &none, &mut h), Likeness::Same, "files the user added are not compared");

        let outside = temp("unmanaged-outside");
        write(&outside, "a.md", "reference\n");
        fs::rename(dir.join("refs"), dir.join("refs-real")).unwrap();
        let made = std::process::Command::new("cmd").args(["/C", "mklink", "/J"]).arg(dir.join("refs")).arg(&outside).output().unwrap().status.success();
        assert!(made);
        assert_eq!(compare_unmanaged(&dir, &up, &none, &mut h), Likeness::Different, "a file reached through a link is not in the folder");
        fs::remove_dir(dir.join("refs")).unwrap();
        fs::rename(dir.join("refs-real"), dir.join("refs")).unwrap();
        fs::remove_dir_all(outside).unwrap();

        write(&dir, "SKILL.md", "# ALPHA\n");
        assert_eq!(compare_unmanaged(&dir, &up, &HashMap::from([("SKILL.md".into(), text::canon(b"# alpha\n"))]), &mut h), Likeness::Different);
        write(&dir, "SKILL.md", "# alpha\n");
        assert_eq!(compare_unmanaged(&dir, &up, &none, &mut h), Likeness::Same);

        fs::remove_file(dir.join("refs").join("a.md")).unwrap();
        assert_eq!(compare_unmanaged(&dir, &up, &none, &mut h), Likeness::Different, "a missing file");
        assert_eq!(compare_unmanaged(&dir.join("nope"), &up, &none, &mut h), Likeness::Different, "no folder at all");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_unmanaged_copy_with_other_line_breaks_is_the_same_once_the_copy_on_github_is_known() {
        let dir = temp("unmanaged-text");
        let up = upstream(&[("SKILL.md", "# alpha\n\nbody\n"), ("logo.bin", "\0\n"), ("refs/a.md", "reference\n")]);
        write(&dir, "SKILL.md", "# alpha\r\nbody\r\n\r\n");
        write(&dir, "logo.bin", "\0\n");
        write(&dir, "refs/a.md", "reference\n");
        let mut h = FileHasher::default();
        assert_eq!(compare_unmanaged(&dir, &up, &HashMap::new(), &mut h), Likeness::NeedsText(vec!["SKILL.md".into()]));
        let theirs = |content: &[u8]| HashMap::from([("SKILL.md".to_string(), text::canon(content))]);
        assert_eq!(compare_unmanaged(&dir, &up, &theirs(b"# alpha\n\nbody\n"), &mut h), Likeness::Same);
        assert_eq!(compare_unmanaged(&dir, &up, &theirs(b"# alpha\n\nother body\n"), &mut h), Likeness::Different);
        assert_eq!(compare_unmanaged(&dir, &up, &theirs(b"\0"), &mut h), Likeness::Different, "a copy on GitHub that is not text");

        write(&dir, "logo.bin", "\0\r\n");
        assert_eq!(compare_unmanaged(&dir, &up, &HashMap::new(), &mut h), Likeness::Different, "a file that is not text needs no download to tell");
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
        assert_eq!(compare_unmanaged(&link, &upstream(&FILES), &HashMap::new(), &mut FileHasher::default()), Likeness::Different);
        fs::remove_dir(&link).unwrap();
        fs::remove_dir_all(base).unwrap();
    }
}
