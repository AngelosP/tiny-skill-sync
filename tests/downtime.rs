//! The promise behind "your edits are safe": a change made while the app was NOT running is found when it starts again.
//!
//! Every test tells the same story, with the real file system and a restarted engine (nothing is kept in memory, and
//! nothing watched the folder in between):
//!
//! 1. the skills are installed and the app looks at them once;
//! 2. the app stops;
//! 3. somebody changes the skills folder the way a person or a program would;
//! 4. the app starts and has to say what happened.
//!
//! The first part is every kind of change. The second part is what happens when the drive cannot vouch for files (no
//! change journal, a different journal, a drive that does not answer): every file is read instead. The third part
//! proves that even a record that has been forged cannot make an update or a removal destroy an edit.
//!
//! On an NTFS drive the check is "quick": a stored fingerprint vouches for a file, so no unchanged file is read. On any
//! other drive it is "full". Each test says which one it expects. Set `TSS_TEST_ROOT` to a folder on another drive or a
//! network share to run all of this there.

mod common;

use std::fs::{self, FileTimes, OpenOptions};
use std::io::{self, Write};
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use common::*;
use tiny_skill_sync::apply::Step;
use tiny_skill_sync::engine::{Access, ApplyResult, LocalStatus, Outcome, StartupReport, Tracking, VerifyReport};
use tiny_skill_sync::reconcile::{DirtyInfo, Hold, LocalState, Verdict};
use tiny_skill_sync::state::{State, VolumeId};
use tiny_skill_sync::verify::Check;
use tiny_skill_sync::win::fs::{probe, volume_id};
use windows_sys::Win32::Storage::FileSystem::{FILE_BASIC_INFO, FileBasicInfo, GetFileInformationByHandleEx, SetFileInformationByHandle};

/// The files the standard repository installs: three in alpha, one each in beta and gamma.
const FILES: usize = 5;
const GUIDE: &str = "alpha/references/guide.md";
const GUIDE_IN_SKILL: &str = "references/guide.md";
/// "guide v1\n" with one letter changed: the length stays the same.
const GUIDE_V2: &[u8] = b"guide v2\n";

// ---------------------------------------------------------------------------------------------------------------------
// The ways a file can be changed, as the programs people use do it
// ---------------------------------------------------------------------------------------------------------------------

fn basic_info(path: &Path) -> FILE_BASIC_INFO {
    let file = OpenOptions::new().read(true).open(path).unwrap();
    // SAFETY: the structure is plain integers, so all zeroes is a valid value.
    let mut info: FILE_BASIC_INFO = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        GetFileInformationByHandleEx(file.as_raw_handle(), FileBasicInfo, (&mut info as *mut FILE_BASIC_INFO).cast(), size_of::<FILE_BASIC_INFO>() as u32)
    };
    assert_ne!(ok, 0, "cannot read the times of {}: {}", path.display(), io::Error::last_os_error());
    info
}

fn set_basic_info(path: &Path, info: &FILE_BASIC_INFO) {
    let file = OpenOptions::new().write(true).open(path).unwrap();
    let ok = unsafe {
        SetFileInformationByHandle(file.as_raw_handle(), FileBasicInfo, (info as *const FILE_BASIC_INFO).cast(), size_of::<FILE_BASIC_INFO>() as u32)
    };
    assert_ne!(ok, 0, "cannot set the times of {}: {}", path.display(), io::Error::last_os_error());
}

/// Overwrites the file from its first byte, in place and without truncating it, then puts back every time it had: created,
/// written, and the change time that `SetFileTime` cannot set. A check that looks at size and times sees nothing at all.
/// On a drive that has a change journal the helper also proves that the forgery is complete and that the journal still
/// saw the write, so the test is as hard as it says it is.
fn overwrite_and_forge_every_timestamp(path: &Path, body: &[u8]) {
    assert_eq!(body.len() as u64, fs::metadata(path).unwrap().len(), "an in-place overwrite keeps the length");
    let before = probe(path).unwrap().expect("the file exists").fp;
    let times = basic_info(path);
    OpenOptions::new().write(true).open(path).unwrap().write_all(body).unwrap();
    set_basic_info(path, &times);
    let after = probe(path).unwrap().expect("the file exists").fp;
    if before.usn != 0 {
        assert_eq!((after.size, after.write, after.change), (before.size, before.write, before.change), "every timestamp is as it was");
        assert_ne!(after.usn, before.usn, "and yet the write is in the change journal");
    }
}

fn put_the_modified_time_back(path: &Path, modified: SystemTime) {
    OpenOptions::new().write(true).open(path).unwrap().set_times(FileTimes::new().set_modified(modified)).unwrap();
}

/// What `fs::write`, PowerShell's `Set-Content` and most programs do: truncate, write, and (here) restore the modified time.
fn truncate_and_write_with_the_old_modified_time(path: &Path, body: &[u8]) {
    let modified = fs::metadata(path).unwrap().modified().unwrap();
    fs::write(path, body).unwrap();
    put_the_modified_time_back(path, modified);
}

/// How editors save: write a new file beside the old one, then rename it over the old one. The new file gets the old time.
fn replace_by_rename(path: &Path, body: &[u8]) {
    let modified = fs::metadata(path).unwrap().modified().unwrap();
    let temp = path.with_file_name("saving.tmp");
    fs::write(&temp, body).unwrap();
    put_the_modified_time_back(&temp, modified);
    fs::rename(&temp, path).unwrap();
}

fn copy_dir_all(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap().flatten() {
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir_all(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// The story
// ---------------------------------------------------------------------------------------------------------------------

/// The app stops. Whatever it knew in memory is gone; only `state.json` remains.
fn stop_the_app(s: &mut Setup) {
    let next = s.restarted();
    drop(std::mem::replace(&mut s.engine, next));
}

fn restart_with(s: &mut Setup, volume: fn(&Path) -> io::Result<VolumeId>) {
    let mut app = s.restarted();
    app.set_volume_probe(volume);
    s.engine = app;
}

fn edit_state(w: &World, change: impl FnOnce(&mut State)) {
    let mut state = w.state();
    change(&mut state);
    state.save(&w.state_file()).unwrap();
}

fn dirty(modified: &[&str], missing: &[&str]) -> LocalStatus {
    let list = |names: &[&str]| names.iter().map(|n| n.to_string()).collect();
    LocalStatus::Checked(LocalState::Dirty(DirtyInfo { modified: list(modified), missing: list(missing) }))
}

fn edited_guide() -> LocalStatus {
    dirty(&[GUIDE_IN_SKILL], &[])
}

/// How many files were read. A quick check reads only the files whose fingerprint did not match; a full check reads every
/// file whose size is as recorded. Which one applies depends on the drive.
fn assert_reads(v: &VerifyReport, quick: usize, full: usize) {
    let (check, hashed) = if v.tracking.journal { (Check::Quick, quick) } else { (Check::Full, full) };
    assert_eq!((v.tracking.check, v.stats.hashed), (check, hashed), "{:?}", v.tracking);
}

fn assert_clean_except(v: &VerifyReport, changed: &[&str]) {
    for name in ["alpha", "beta", "gamma"].into_iter().filter(|n| !changed.contains(n)) {
        assert_eq!(status_of(v, name), clean(), "{name} was not touched");
    }
}

/// Installs the skills, lets the app look at them once, stops it, makes `change` and starts the app again.
fn start_after(name: &str, change: impl FnOnce(&World)) -> (Setup, StartupReport) {
    let mut s = Setup::installed(name);
    let before = s.engine.startup(&s.cfg).unwrap();
    assert!(before.problem.is_none(), "{:?}", before.problem);
    assert_clean_except(&before.verify, &[]);
    // From the moment they were written the files carry fingerprints, so a quick check needs to read nothing.
    assert_reads(&before.verify, 0, FILES);

    stop_the_app(&mut s);
    change(&s.w);
    let report = s.engine.startup(&s.cfg).unwrap();
    assert!(report.problem.is_none(), "{:?}", report.problem);
    assert!(!report.verify.tracking.volume_changed, "the drive is the same one");
    (s, report)
}

// ---------------------------------------------------------------------------------------------------------------------
// Part 1: every kind of change
// ---------------------------------------------------------------------------------------------------------------------

#[test]
fn when_nothing_changed_while_the_app_was_stopped_no_file_is_read() {
    let (_s, report) = start_after("nothing", |_| {});
    assert_clean_except(&report.verify, &[]);
    assert_reads(&report.verify, 0, FILES);
}

#[test]
fn a_same_size_overwrite_with_every_timestamp_put_back_is_found_and_stays_found() {
    let (mut s, report) = start_after("forged", |w| overwrite_and_forge_every_timestamp(&w.skills.join(GUIDE), GUIDE_V2));
    assert_eq!(status_of(&report.verify, "alpha"), edited_guide());
    assert_clean_except(&report.verify, &["alpha"]);
    assert_reads(&report.verify, 1, FILES);

    // Finding an edit does not make it "known": it is reported at every check, with that one file read each time, until the
    // user deals with it.
    let again = s.engine.verify(&s.cfg, Access::Write).unwrap();
    assert_eq!(status_of(&again, "alpha"), edited_guide());
    assert_reads(&again, 1, FILES);
    stop_the_app(&mut s);
    let later = s.engine.startup(&s.cfg).unwrap();
    assert_eq!(status_of(&later.verify, "alpha"), edited_guide());
    assert_reads(&later.verify, 1, FILES);

    // Finding it changed nothing on disk.
    assert_eq!(s.w.read(GUIDE), "guide v2\n");
    assert!(s.w.backups().is_empty() && s.w.work_leftovers().is_empty());
}

#[test]
fn an_edit_straight_after_the_install_is_found_too() {
    // No check in between, and no pause: the edit follows the install within a few milliseconds, inside one clock tick.
    let mut s = Setup::installed("straight-after");
    stop_the_app(&mut s);
    overwrite_and_forge_every_timestamp(&s.w.skills.join(GUIDE), GUIDE_V2);

    let report = s.engine.startup(&s.cfg).unwrap();
    assert_eq!(status_of(&report.verify, "alpha"), edited_guide());
    assert_clean_except(&report.verify, &["alpha"]);
}

#[test]
fn a_truncating_rewrite_that_puts_the_old_modified_time_back_is_found() {
    let (_s, report) = start_after("truncating", |w| truncate_and_write_with_the_old_modified_time(&w.skills.join(GUIDE), GUIDE_V2));
    assert_eq!(status_of(&report.verify, "alpha"), edited_guide());
    assert_clean_except(&report.verify, &["alpha"]);
    assert_reads(&report.verify, 1, FILES);
}

#[test]
fn a_write_that_has_not_been_closed_yet_is_found_at_once() {
    // The change journal hears about a write when it happens, not when the program closes the file.
    let mut s = Setup::installed("open-write");
    stop_the_app(&mut s);
    let mut writer = OpenOptions::new().write(true).open(s.w.skills.join(GUIDE)).unwrap();
    writer.write_all(GUIDE_V2).unwrap();

    let while_open = s.engine.verify(&s.cfg, Access::ReadOnly).unwrap();
    assert_eq!(status_of(&while_open, "alpha"), edited_guide());
    assert_reads(&while_open, 1, FILES);

    drop(writer);
    let closed = s.engine.verify(&s.cfg, Access::ReadOnly).unwrap();
    assert_eq!(status_of(&closed, "alpha"), edited_guide());
    assert_clean_except(&closed, &["alpha"]);
}

#[test]
fn a_file_replaced_by_rename_the_way_editors_save_is_found() {
    let (_s, report) = start_after("rename", |w| replace_by_rename(&w.skills.join(GUIDE), GUIDE_V2));
    assert_eq!(status_of(&report.verify, "alpha"), edited_guide());
    assert_clean_except(&report.verify, &["alpha"]);
    assert_reads(&report.verify, 1, FILES);
}

#[test]
fn a_longer_or_shorter_file_is_found() {
    let (_s, report) = start_after("sizes", |w| {
        fs::write(w.skills.join(GUIDE), "g\n").unwrap();
        let mut beta = OpenOptions::new().append(true).open(w.skills.join("beta/SKILL.md")).unwrap();
        beta.write_all(b"one more line\n").unwrap();
    });
    assert_eq!(status_of(&report.verify, "alpha"), edited_guide());
    assert_eq!(status_of(&report.verify, "beta"), dirty(&["SKILL.md"], &[]));
    assert_eq!(status_of(&report.verify, "gamma"), clean());
    // Text of another size can still be the same text (other line breaks, blank lines), so the two files are read.
    assert_reads(&report.verify, 2, FILES);
}

#[test]
fn a_file_deleted_and_made_again_with_other_content_is_found() {
    let (_s, report) = start_after("recreated-other", |w| {
        fs::remove_file(w.skills.join(GUIDE)).unwrap();
        fs::write(w.skills.join(GUIDE), GUIDE_V2).unwrap();
    });
    assert_eq!(status_of(&report.verify, "alpha"), edited_guide());
    assert_clean_except(&report.verify, &["alpha"]);
    assert_reads(&report.verify, 1, FILES);
}

#[test]
fn a_file_deleted_and_made_again_with_the_same_content_is_clean_and_not_read_again() {
    let (mut s, report) = start_after("recreated-same", |w| {
        fs::remove_file(w.skills.join(GUIDE)).unwrap();
        fs::write(w.skills.join(GUIDE), "guide v1\n").unwrap();
    });
    assert_clean_except(&report.verify, &[]);
    // It is a different file now (new identity), so it was read once to be sure...
    assert_reads(&report.verify, 1, FILES);
    // ...and what was learned is kept, even after the app stops.
    stop_the_app(&mut s);
    let again = s.engine.verify(&s.cfg, Access::ReadOnly).unwrap();
    assert_clean_except(&again, &[]);
    assert_reads(&again, 0, FILES);
}

#[test]
fn touching_or_rewriting_with_the_same_bytes_is_not_an_edit_and_is_learned() {
    let (mut s, report) = start_after("touched", |w| {
        put_the_modified_time_back(&w.skills.join(GUIDE), SystemTime::now() + Duration::from_secs(3600));
        let beta = w.skills.join("beta/SKILL.md");
        fs::write(&beta, fs::read(&beta).unwrap()).unwrap();
    });
    assert_clean_except(&report.verify, &[]);
    assert_reads(&report.verify, 2, FILES);
    stop_the_app(&mut s);
    let again = s.engine.verify(&s.cfg, Access::ReadOnly).unwrap();
    assert_reads(&again, 0, FILES);
}

#[test]
fn changing_only_an_attribute_is_not_an_edit() {
    let (mut s, report) = start_after("attribute", |w| {
        let path = w.skills.join(GUIDE);
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&path, permissions).unwrap();
    });
    assert_clean_except(&report.verify, &[]);
    assert_reads(&report.verify, 1, FILES);

    let path = s.w.skills.join(GUIDE);
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)] // Windows only: this clears the read-only attribute and nothing else
    permissions.set_readonly(false);
    fs::set_permissions(&path, permissions).unwrap();
    s.w.write(GUIDE, "guide v1\n");
    let after = s.engine.verify(&s.cfg, Access::ReadOnly).unwrap();
    assert_clean_except(&after, &[]);
}

#[test]
fn each_skill_is_judged_on_its_own_when_several_things_happen_at_once() {
    let (_s, report) = start_after("several", |w| {
        overwrite_and_forge_every_timestamp(&w.skills.join(GUIDE), GUIDE_V2);
        fs::remove_file(w.skills.join("beta/SKILL.md")).unwrap();
        fs::write(w.skills.join("gamma/notes.txt"), "mine\n").unwrap();
    });
    assert_eq!(status_of(&report.verify, "alpha"), edited_guide());
    assert_eq!(status_of(&report.verify, "beta"), dirty(&[], &["SKILL.md"]));
    assert_eq!(status_of(&report.verify, "gamma"), clean(), "a file the user added is theirs, not an edit of the skill");
    // Quick: only the forged file is read. Full: alpha's three files, nothing of beta (it is gone), gamma's one.
    assert_reads(&report.verify, 1, 4);
}

#[test]
fn a_deleted_skill_folder_is_reported_as_missing() {
    let (_s, report) = start_after("folder-gone", |w| w.remove("alpha"));
    assert_eq!(status_of(&report.verify, "alpha"), LocalStatus::Checked(LocalState::FolderMissing));
    assert_clean_except(&report.verify, &["alpha"]);
    assert_reads(&report.verify, 0, 2);
}

#[test]
fn a_folder_swapped_for_an_identical_copy_is_judged_by_its_content() {
    let (mut s, report) = start_after("swapped", |w| {
        let alpha = w.skills.join("alpha");
        let copy = w.skills.join("alpha.copy");
        copy_dir_all(&alpha, &copy);
        fs::remove_dir_all(&alpha).unwrap();
        fs::rename(&copy, &alpha).unwrap();
    });
    // Every file is a new file with a new identity: all three are read, and all three match.
    assert_clean_except(&report.verify, &[]);
    assert_reads(&report.verify, 3, FILES);
    stop_the_app(&mut s);
    let again = s.engine.verify(&s.cfg, Access::ReadOnly).unwrap();
    assert_reads(&again, 0, FILES);
}

/// The same-tick race, many times over: each edit comes right after a check that vouched for the file, with all times
/// put back, and each undo comes right after the edit was found. None may be missed, and nothing may be reported twice.
#[test]
fn edits_made_in_the_same_instant_as_the_check_before_them_are_all_found() {
    // (skill, file in the skill, text, same-length different text)
    const CASES: [(&str, &str, &str, &str); 5] = [
        ("alpha", "SKILL.md", ALPHA_V1, ALPHA_V2),
        ("alpha", "references/guide.md", "guide v1\n", "guide v2\n"),
        ("alpha", "scripts/hello.ps1", "Write-Output 'hello'\n", "Write-Output 'jello'\n"),
        ("beta", "SKILL.md", "---\nname: beta\ndescription: second skill\n---\n# Beta v1\n", "---\nname: beta\ndescription: second skill\n---\n# Beta v2\n"),
        ("gamma", "SKILL.md", "---\nname: gamma\ndescription: third skill\n---\n# Gamma v1\n", "---\nname: gamma\ndescription: third skill\n---\n# Gamma v2\n"),
    ];
    let mut s = Setup::installed("same-tick");
    for round in 0..100 {
        let (skill_name, file, original, edited) = CASES[round % CASES.len()];
        let path = s.w.skills.join(skill_name).join(file);

        let look = s.engine.verify(&s.cfg, Access::Write).unwrap();
        assert_clean_except(&look, &[]);
        overwrite_and_forge_every_timestamp(&path, edited.as_bytes());
        let found = s.engine.verify(&s.cfg, Access::Write).unwrap();
        assert_eq!(status_of(&found, skill_name), dirty(&[file], &[]), "round {round}: the edit of {skill_name}/{file} was missed");
        assert_clean_except(&found, &[skill_name]);

        overwrite_and_forge_every_timestamp(&path, original.as_bytes());
    }
    let last = s.engine.verify(&s.cfg, Access::Write).unwrap();
    assert_clean_except(&last, &[]);
}

// ---------------------------------------------------------------------------------------------------------------------
// Part 2: drives that cannot vouch for a file
// ---------------------------------------------------------------------------------------------------------------------

/// A FAT or exFAT drive, a network share, or an NTFS drive whose change journal was deleted.
fn no_journal(dir: &Path) -> io::Result<VolumeId> {
    volume_id(dir).map(|v| VolumeId { journal: 0, ..v })
}

fn cannot_be_asked(_: &Path) -> io::Result<VolumeId> {
    Err(io::Error::other("this drive does not answer"))
}

/// Network shares have no 128-bit file IDs. That must cost the quick check, never the whole synchronisation.
#[test]
fn every_drive_can_be_named_whatever_it_supports() {
    let w = World::new("name-the-drive");
    fs::create_dir_all(&w.skills).unwrap();
    let named = volume_id(&w.skills).expect("a drive without 128-bit file ids still has a name");
    assert_eq!(volume_id(&w.skills).unwrap(), named, "and the name does not change");
}

#[test]
fn without_a_change_journal_every_file_is_read_and_the_edit_is_still_found() {
    let mut s = Setup::fresh("no-journal");
    s.engine.set_volume_probe(no_journal);
    let installed = s.sync();
    assert!(matches!(only(&installed).outcome, Outcome::Synced), "{installed:?}");

    let full = Tracking { check: Check::Full, volume_changed: false, journal: false, folder_missing: false };
    let before = s.engine.startup(&s.cfg).unwrap();
    assert_eq!((before.verify.tracking, before.verify.stats.hashed), (full, FILES), "nothing can vouch, so everything is read");
    assert_clean_except(&before.verify, &[]);

    restart_with(&mut s, no_journal);
    overwrite_and_forge_every_timestamp(&s.w.skills.join(GUIDE), GUIDE_V2);
    let after = s.engine.startup(&s.cfg).unwrap();
    assert_eq!((after.verify.tracking, after.verify.stats.hashed), (full, FILES));
    assert_eq!(status_of(&after.verify, "alpha"), edited_guide());
    assert_clean_except(&after.verify, &["alpha"]);
}

#[test]
fn a_journal_that_was_deleted_and_made_again_means_nothing_stored_is_trusted() {
    let mut s = Setup::installed("journal-again");
    let journal = s.engine.verify(&s.cfg, Access::ReadOnly).unwrap().tracking.journal;
    stop_the_app(&mut s);
    overwrite_and_forge_every_timestamp(&s.w.skills.join(GUIDE), GUIDE_V2);
    // The state says the fingerprints were taken under another journal.
    edit_state(&s.w, |state| state.volume.as_mut().expect("the drive is recorded").journal ^= 0xFFFF);

    let report = s.engine.verify(&s.cfg, Access::Write).unwrap();
    assert!(report.tracking.volume_changed);
    assert_eq!((report.tracking.check, report.stats.hashed), (Check::Full, FILES), "every file is read");
    assert_eq!(status_of(&report, "alpha"), edited_guide());
    assert_clean_except(&report, &["alpha"]);
    // Whatever was recorded under the other journal is dropped, for the edited file too: nothing from it can vouch later.
    let state = s.w.state();
    let guide = state.repo("fx").unwrap().skill("alpha").unwrap().files.iter().find(|f| f.path == GUIDE_IN_SKILL).unwrap().clone();
    assert_eq!(guide.fp, None);

    // The journal that is there now is remembered, and the files that match are fingerprinted afresh.
    let next = s.engine.verify(&s.cfg, Access::Write).unwrap();
    assert!(!next.tracking.volume_changed);
    assert_eq!(status_of(&next, "alpha"), edited_guide());
    if journal {
        assert_eq!((next.tracking.check, next.stats.hashed), (Check::Quick, 1), "only the edited file is read again");
    }
}

#[test]
fn a_drive_that_cannot_be_asked_is_read_in_full_and_trusted_again_when_it_answers() {
    let mut s = Setup::installed("cannot-ask");
    stop_the_app(&mut s);
    overwrite_and_forge_every_timestamp(&s.w.skills.join(GUIDE), GUIDE_V2);

    restart_with(&mut s, cannot_be_asked);
    let report = s.engine.verify(&s.cfg, Access::Write).unwrap();
    let full = Tracking { check: Check::Full, volume_changed: false, journal: false, folder_missing: false };
    assert_eq!((report.tracking, report.stats.hashed), (full, FILES));
    assert_eq!(status_of(&report, "alpha"), edited_guide());
    assert_clean_except(&report, &["alpha"]);

    // When the drive answers again the app picks up where it was: the edit is still reported, and only it is read.
    stop_the_app(&mut s);
    let back = s.engine.verify(&s.cfg, Access::Write).unwrap();
    assert!(!back.tracking.volume_changed);
    assert_eq!(status_of(&back, "alpha"), edited_guide());
    assert_reads(&back, 1, FILES);
}

#[test]
fn a_fingerprint_that_has_no_change_number_is_never_believed() {
    let mut s = Setup::installed("no-usn");
    stop_the_app(&mut s);
    overwrite_and_forge_every_timestamp(&s.w.skills.join(GUIDE), GUIDE_V2);
    // Records as they would be if the drive had given no change number when they were taken.
    edit_state(&s.w, |state| {
        for file in state.repos.values_mut().flat_map(|r| r.skills.iter_mut()).flat_map(|k| k.files.iter_mut()) {
            if let Some(fp) = file.fp.as_mut() {
                fp.usn = 0;
            }
        }
    });

    let report = s.engine.verify(&s.cfg, Access::Write).unwrap();
    assert_eq!(report.stats.hashed, FILES, "nothing can vouch, so every file is read");
    assert_eq!(status_of(&report, "alpha"), edited_guide());
    assert_clean_except(&report, &["alpha"]);
    // The files that matched now carry a proper fingerprint again.
    let next = s.engine.verify(&s.cfg, Access::Write).unwrap();
    assert_reads(&next, 1, FILES);
}

// ---------------------------------------------------------------------------------------------------------------------
// Part 3: even a fooled quick check cannot cost anybody an edit
// ---------------------------------------------------------------------------------------------------------------------

/// An edit made while the app was stopped, and a record forged to say that nothing happened: it holds exactly what the file
/// reports now. Nothing on the disk tells a quick check otherwise. This is the one thing a fingerprint cannot be asked to
/// survive, and it is why every change and every removal reads the folder in full first.
fn with_a_fooled_record(name: &str) -> Setup {
    let mut s = Setup::installed(name);
    stop_the_app(&mut s);
    let path = s.w.skills.join(GUIDE);
    overwrite_and_forge_every_timestamp(&path, GUIDE_V2);
    let now = probe(&path).unwrap().expect("the file exists").fp;
    edit_state(&s.w, |state| {
        let skill = state.repo_mut("fx").skill_mut("alpha").expect("alpha is owned");
        skill.files.iter_mut().find(|f| f.path == GUIDE_IN_SKILL).expect("the guide is recorded").fp = Some(now);
    });
    s
}

/// Remembers every step a change reaches. A change that is held back before it starts reaches none of the steps that move a folder.
fn record_steps(s: &mut Setup) -> Arc<Mutex<Vec<Step>>> {
    let steps = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&steps);
    s.engine.set_step_hook(move |step| {
        seen.lock().unwrap().push(step);
        Ok(())
    });
    steps
}

fn assert_quick_check_is_fooled(s: &mut Setup) -> bool {
    let report = s.engine.verify(&s.cfg, Access::ReadOnly).unwrap();
    if report.tracking.journal {
        assert_eq!(status_of(&report, "alpha"), clean(), "the quick check believes the record: that is the limit of a fingerprint");
    } else {
        // Without a journal there is no change number to vouch with, so the file is read and the edit seen.
        assert_eq!(status_of(&report, "alpha"), edited_guide());
    }
    report.tracking.journal
}

#[test]
fn an_edit_made_while_stopped_holds_back_an_update_that_is_there_at_the_first_check() {
    let mut s = Setup::installed("held-after-downtime");
    stop_the_app(&mut s);
    overwrite_and_forge_every_timestamp(&s.w.skills.join(GUIDE), GUIDE_V2);
    s.remote.put(REPO, "skills/alpha/SKILL.md", ALPHA_V2);
    s.advance(3600);
    let before = s.w.skills_snapshot();

    // The restarted app has never looked at the edit: its very first act is this check.
    let report = s.sync();
    let repo = only(&report);
    assert!(matches!(repo.outcome, Outcome::NeedsAttention), "{:?}", repo.outcome);
    assert_eq!(skill(repo, "alpha").verdict, Verdict::Held(Hold::UpdateBlocked(DirtyInfo { modified: vec![GUIDE_IN_SKILL.into()], ..DirtyInfo::default() })));
    assert_eq!(skill(repo, "beta").verdict, Verdict::InSync);
    assert_eq!(s.remote.calls().file, 0, "nothing is downloaded for a skill that cannot be updated");
    assert_eq!(s.w.skills_snapshot(), before, "not one file of the skills folder was created, changed or removed");
    assert!(s.w.backups().is_empty() && s.w.work_leftovers().is_empty());
}

#[test]
fn a_fooled_quick_check_cannot_make_an_update_overwrite_an_edit() {
    let mut s = with_a_fooled_record("fooled-update");
    let journal = assert_quick_check_is_fooled(&mut s);
    s.remote.put(REPO, "skills/alpha/SKILL.md", ALPHA_V2);
    s.advance(3600);
    let before = s.w.skills_snapshot();
    let steps = record_steps(&mut s);

    let report = s.sync();
    let repo = only(&report);
    assert!(matches!(repo.outcome, Outcome::NeedsAttention), "{:?}", repo.outcome);
    let alpha = skill(repo, "alpha");
    if journal {
        // Planned as an update, and stopped by the full check right before the swap.
        assert_eq!(alpha.verdict, Verdict::Update);
        assert!(matches!(&alpha.result, ApplyResult::Held(why) if why.contains("edited locally")), "{:?}", alpha.result);
    } else {
        assert!(matches!(alpha.verdict, Verdict::Held(Hold::UpdateBlocked(_))), "{:?}", alpha.verdict);
    }
    // Stopped before the folder was moved, not undone afterwards: nothing was swapped, not even for a moment.
    let expected = if journal { vec![Step::Staged] } else { vec![] };
    assert_eq!(*steps.lock().unwrap(), expected);
    assert_eq!(s.w.skills_snapshot(), before, "not one file of the skills folder was created, changed or removed");
    assert_eq!(s.w.read(GUIDE), "guide v2\n");
    assert_eq!(s.w.read("alpha/SKILL.md"), ALPHA_V1);
    assert!(s.w.backups().is_empty());
    assert!(s.w.work_leftovers().is_empty(), "the downloaded copy was discarded: {:?}", s.w.work_leftovers());
    assert!(s.w.state().repo("fx").unwrap().skill("alpha").unwrap().pending.is_none(), "nothing was left half done");
}

#[test]
fn a_fooled_quick_check_cannot_make_a_removal_delete_an_edited_skill() {
    let mut s = with_a_fooled_record("fooled-delete");
    let journal = assert_quick_check_is_fooled(&mut s);
    s.remote.remove(REPO, "skills/alpha");
    s.advance(3600);
    let before = s.w.skills_snapshot();
    let steps = record_steps(&mut s);

    let report = s.sync();
    let repo = only(&report);
    assert!(matches!(repo.outcome, Outcome::NeedsAttention), "{:?}", repo.outcome);
    let alpha = skill(repo, "alpha");
    if journal {
        assert_eq!(alpha.verdict, Verdict::Delete);
        assert!(matches!(&alpha.result, ApplyResult::Held(why) if why.contains("edited locally")), "{:?}", alpha.result);
    } else {
        assert!(matches!(alpha.verdict, Verdict::Held(Hold::DeleteBlocked(_))), "{:?}", alpha.verdict);
    }
    assert_eq!(*steps.lock().unwrap(), Vec::<Step>::new(), "the folder was never moved, not even for a moment");
    assert_eq!(s.w.skills_snapshot(), before, "the skill is exactly where it was");
    assert_eq!(s.w.read(GUIDE), "guide v2\n");
    assert!(s.w.backups().is_empty() && s.w.work_leftovers().is_empty());
    assert!(s.w.state().repo("fx").unwrap().skill("alpha").is_some(), "the skill is still on record, so it is still protected");
}
