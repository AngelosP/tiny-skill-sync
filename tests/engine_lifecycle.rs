//! The life of the app around a check: dry runs, crashes in the middle of a change, other programs using the same
//! files, damaged or changed state, a changed drive, and the schedule.

mod common;

use std::fs::{self, File, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use tiny_skill_sync::apply::Step;
use tiny_skill_sync::config::RepoConfig;
use tiny_skill_sync::engine::{Access, ApplyResult, LocalStatus, Outcome, RepoReport, Resolution, SyncOptions, Which};
use tiny_skill_sync::github::{FailureKind, RemoteError};
use tiny_skill_sync::interval::{Interval, Unit};
use tiny_skill_sync::reconcile::{DirtyInfo, Hold, LocalState, Skip, Verdict};
use tiny_skill_sync::state::VolumeId;
use tiny_skill_sync::verify::Check;

fn dry_run() -> SyncOptions {
    SyncOptions { dry_run: true, ..SyncOptions::default() }
}

fn verdicts(repo: &RepoReport) -> Vec<(String, Verdict)> {
    repo.skills.iter().map(|k| (k.name.clone(), k.verdict.clone())).collect()
}

fn last_check_on_disk(w: &World) -> Option<i64> {
    w.state().repo("fx").and_then(|r| r.last_check)
}

/// A crash: the code stops dead the first time it reaches `step`, with no cleanup, as if the power went out.
fn crash_once_at(step: Step) -> impl FnMut(Step) -> Result<(), RemoteError> + Send + 'static {
    let mut fired = false;
    move |at| {
        if at == step && !fired {
            fired = true;
            Err(RemoteError::new(FailureKind::Local, "the power went out"))
        } else {
            Ok(())
        }
    }
}

/// What alpha looks like: whole and old, whole and new, or not there. Anything in between fails the test.
fn alpha(w: &World) -> &'static str {
    if !w.exists("alpha") {
        return "missing";
    }
    let files = w.files_of("alpha");
    assert_eq!(files.len(), 3, "alpha is never a partial folder: {:?}", files.keys().collect::<Vec<_>>());
    assert_eq!(files["references/guide.md"], "guide v1\n");
    match files["SKILL.md"].as_str() {
        ALPHA_V1 => "v1",
        ALPHA_V2 => "v2",
        other => panic!("alpha/SKILL.md is neither version: {other:?}"),
    }
}

/// Installed, then upstream changes alpha, then the check is cut off at `step`.
fn update_cut_off_at(name: &str, step: Step) -> Setup {
    let mut s = Setup::installed(name);
    s.remote.put(REPO, "skills/alpha/SKILL.md", ALPHA_V2);
    s.engine.set_step_hook(crash_once_at(step));
    let report = s.sync();
    let k = skill(only(&report), "alpha");
    assert_eq!(k.verdict, Verdict::Update);
    assert!(matches!(&k.result, ApplyResult::Failed(why) if why.contains("power")), "{:?}", k.result);
    s
}

/// A folder opened without letting others rename it: what "in use by another program" looks like to the app.
fn hold_open(dir: &Path) -> File {
    OpenOptions::new().read(true).custom_flags(0x0200_0000).share_mode(1 | 2).open(dir).unwrap()
}

fn work_folder_starting_with(w: &World, prefix: &str) -> PathBuf {
    fs::read_dir(w.work_dir())
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.file_name().unwrap().to_string_lossy().starts_with(prefix))
        .unwrap_or_else(|| panic!("no {prefix}* folder in {:?}", w.work_leftovers()))
}

fn network_error() -> RemoteError {
    RemoteError::new(FailureKind::Network, "the network is down, on purpose")
}

// ---------------------------------------------------------------------------------------------------------------------
// Dry runs
// ---------------------------------------------------------------------------------------------------------------------

#[test]
fn a_dry_run_on_a_fresh_machine_creates_nothing_and_says_what_the_real_run_does() {
    let mut s = Setup::fresh("dry-fresh");
    let before = s.w.snapshot();

    let dry = sync_with(&mut s.engine, &s.cfg, dry_run());
    let planned = verdicts(only(&dry));

    assert!(matches!(only(&dry).outcome, Outcome::WouldChange), "{:?}", only(&dry).outcome);
    assert_eq!(planned.len(), 3);
    for k in &only(&dry).skills {
        assert_eq!((k.verdict.clone(), k.result.clone()), (Verdict::Install, ApplyResult::DryRun), "{}", k.name);
    }
    assert_eq!(s.w.snapshot(), before, "no file, no folder, not even the lock file or the list of managed skills");
    assert_eq!(s.remote.calls().file, 0, "nothing is downloaded");
    assert!(s.engine.schedule().retry_of("fx").is_none());

    let real = s.sync();
    assert_eq!(verdicts(only(&real)), planned);
    assert!(matches!(only(&real).outcome, Outcome::Synced));
}

#[test]
fn a_dry_run_on_an_installed_machine_changes_nothing_either() {
    let mut s = Setup::installed("dry-installed");
    s.remote.put(REPO, "skills/alpha/SKILL.md", ALPHA_V2);
    s.remote.remove(REPO, "skills/gamma");
    s.remote.put(REPO, "skills/delta/SKILL.md", "---\nname: delta\n---\n");
    s.advance(3600);
    let before = s.w.snapshot();

    let dry = sync_with(&mut s.engine, &s.cfg, dry_run());
    let repo = only(&dry);

    assert_eq!(skill(repo, "alpha").verdict, Verdict::Update);
    assert_eq!(skill(repo, "gamma").verdict, Verdict::Delete);
    assert_eq!(skill(repo, "delta").verdict, Verdict::Install);
    assert!(repo.skills.iter().filter(|k| k.verdict.is_action()).all(|k| k.result == ApplyResult::DryRun));
    assert_eq!(s.w.snapshot(), before);
    assert_eq!(s.engine.last_check("fx"), Some(T0), "looking is not checking: the schedule does not move");

    let real = s.sync();
    assert_eq!(verdicts(only(&real)), verdicts(repo));
    assert!(only(&real).skills.iter().filter(|k| k.verdict.is_action()).all(|k| k.result == ApplyResult::Done));
    assert_eq!(s.w.skill_names(), ["alpha", "beta", "delta"]);
}

#[test]
fn a_dry_run_that_fails_does_not_make_the_app_wait() {
    let mut s = Setup::installed("dry-fails");
    s.remote.fail_tree(REPO, Some(network_error()));
    s.advance(3600);

    let dry = sync_with(&mut s.engine, &s.cfg, dry_run());

    assert!(matches!(only(&dry).outcome, Outcome::Failed(_)));
    assert!(s.engine.schedule().retry_of("fx").is_none());
    assert_eq!(s.engine.next_wake(&s.cfg), Some(T0 + 86_400));
}

// ---------------------------------------------------------------------------------------------------------------------
// Crashes in the middle of a change
// ---------------------------------------------------------------------------------------------------------------------

/// Where an update of alpha can be cut off: what the folder looks like right then, whether the change was marked as
/// unfinished, what recovery says it did, and what alpha is once it is sorted out.
const UPDATE_CUTS: [(Step, &str, bool, &str, &str); 5] = [
    (Step::Staged, "v1", false, "", "v1"),
    (Step::Marked, "v1", true, "before it changed anything", "v1"),
    (Step::OldMoved, "missing", true, "the previous version is back", "v1"),
    (Step::NewInPlace, "v2", true, "finished an update", "v2"),
    (Step::Settled, "v2", true, "finished an update", "v2"),
];

#[test]
fn an_update_cut_off_at_any_step_is_sorted_out_at_the_next_start() {
    for (step, right_after, marked, note, after) in UPDATE_CUTS {
        let s = update_cut_off_at(&format!("cut-{step:?}"), step);
        assert_eq!(alpha(&s.w), right_after, "{step:?}: the folder right after the crash");
        assert_eq!(s.w.state().has_pending(), marked, "{step:?}: marked as unfinished");

        let mut app = s.restarted();
        let before = s.w.snapshot();
        let looked = app.verify(&s.cfg, Access::ReadOnly).unwrap();
        assert_eq!(s.w.snapshot(), before, "{step:?}: looking writes nothing");
        if marked {
            assert_eq!(status_of(&looked, "alpha"), LocalStatus::Interrupted, "{step:?}");
        }

        let started = app.startup(&s.cfg).unwrap();
        let notes: Vec<&str> = started.verify.notes.iter().map(|n| n.text.as_str()).collect();
        if marked {
            assert_eq!(notes.len(), 1, "{step:?}: {notes:?}");
            assert!(notes[0].contains(note), "{step:?}: {}", notes[0]);
            assert_eq!(started.verify.notes[0].skill, "alpha");
        } else {
            assert!(notes.is_empty(), "{step:?}: {notes:?}");
        }
        assert_eq!(alpha(&s.w), after, "{step:?}");
        assert!(!s.w.state().has_pending(), "{step:?}");
        assert!(s.w.work_leftovers().is_empty(), "{step:?}: {:?}", s.w.work_leftovers());
        for name in ["alpha", "beta", "gamma"] {
            assert_eq!(status_of(&started.verify, name), clean(), "{step:?}: {name}");
        }

        // The next check does whatever is left, and every starting point ends in the same place.
        let report = sync_all(&mut app, &s.cfg);
        assert!(!matches!(only(&report).outcome, Outcome::Failed(_)), "{step:?}: {:?}", only(&report).outcome);
        assert_eq!(alpha(&s.w), "v2", "{step:?}");
        let state = s.w.state();
        assert_eq!(state.repo("fx").unwrap().skill("alpha").unwrap().commit, s.remote.head(REPO), "{step:?}");
        assert!(!state.has_pending(), "{step:?}");
        assert!(s.w.work_leftovers().is_empty(), "{step:?}: {:?}", s.w.work_leftovers());
    }
}

#[test]
fn a_plain_check_sorts_out_an_interrupted_update_before_it_decides_anything() {
    for (step, _, marked, note, _) in UPDATE_CUTS {
        let s = update_cut_off_at(&format!("cut-check-{step:?}"), step);
        let mut app = s.restarted();

        let report = sync_all(&mut app, &s.cfg);

        if marked {
            assert_eq!(report.notes.len(), 1, "{step:?}: {:?}", report.notes);
            assert!(report.notes[0].text.contains(note), "{step:?}: {}", report.notes[0].text);
        } else {
            assert!(report.notes.is_empty(), "{step:?}");
        }
        assert_eq!(alpha(&s.w), "v2", "{step:?}");
        assert!(!s.w.state().has_pending(), "{step:?}");
        // Only the harmless half-made folder from before the marker can be left; the next start sweeps it.
        let left = s.w.work_leftovers();
        assert!(left.iter().all(|n| n.starts_with("stage-")), "{step:?}: {left:?}");
        assert_eq!(left.is_empty(), marked, "{step:?}: {left:?}");
    }
}

#[test]
fn a_removal_cut_off_at_any_step_is_sorted_out_at_the_next_start() {
    // Where the folder is after recovery, and what recovery says.
    let cuts =
        [(Step::Marked, true, "before it changed anything"), (Step::OldMoved, false, "finished a removal"), (Step::Settled, false, "had already finished")];
    for (step, folder_stays, note) in cuts {
        let mut s = Setup::installed(&format!("cut-delete-{step:?}"));
        s.remote.remove(REPO, "skills/gamma");
        s.engine.set_step_hook(crash_once_at(step));
        let report = s.sync();
        let k = skill(only(&report), "gamma");
        assert_eq!(k.verdict, Verdict::Delete);
        assert!(matches!(&k.result, ApplyResult::Failed(why) if why.contains("power")), "{step:?}: {:?}", k.result);
        assert!(s.w.state().has_pending(), "{step:?}");

        let mut app = s.restarted();
        let started = app.startup(&s.cfg).unwrap();

        assert_eq!(started.verify.notes.len(), 1, "{step:?}: {:?}", started.verify.notes);
        assert!(started.verify.notes[0].text.contains(note), "{step:?}: {}", started.verify.notes[0].text);
        assert_eq!(s.w.exists("gamma"), folder_stays, "{step:?}");
        assert!(!s.w.state().has_pending(), "{step:?}");
        assert!(s.w.work_leftovers().is_empty(), "{step:?}: {:?}", s.w.work_leftovers());

        let report = sync_all(&mut app, &s.cfg);
        assert!(!matches!(only(&report).outcome, Outcome::Failed(_)), "{step:?}: {:?}", only(&report).outcome);
        assert_eq!(s.w.skill_names(), ["alpha", "beta"], "{step:?}");
        assert!(s.w.state().repo("fx").unwrap().skill("gamma").is_none(), "{step:?}");
        assert!(s.w.work_leftovers().is_empty(), "{step:?}");
    }
}

#[test]
fn a_first_install_cut_off_leaves_no_half_skill_and_is_finished_later() {
    for (step, kept_by_recovery, note) in [(Step::Marked, false, "the folder is gone"), (Step::NewInPlace, true, "finished an update")] {
        let mut s = Setup::fresh(&format!("cut-install-{step:?}"));
        s.engine.set_step_hook(crash_once_at(step));

        let report = s.sync();
        let repo = only(&report);
        assert!(matches!(&skill(repo, "alpha").result, ApplyResult::Failed(_)), "{step:?}");
        assert_eq!(skill(repo, "beta").result, ApplyResult::Done, "{step:?}: the others are not stopped by it");
        assert_eq!(skill(repo, "gamma").result, ApplyResult::Done, "{step:?}");

        let mut app = s.restarted();
        let started = app.startup(&s.cfg).unwrap();
        assert_eq!(started.verify.notes.len(), 1, "{step:?}: {:?}", started.verify.notes);
        assert!(started.verify.notes[0].text.contains(note), "{step:?}: {}", started.verify.notes[0].text);
        assert_eq!(alpha(&s.w) != "missing", kept_by_recovery, "{step:?}");
        assert!(!s.w.state().has_pending(), "{step:?}");
        assert!(s.w.work_leftovers().is_empty(), "{step:?}: {:?}", s.w.work_leftovers());

        let report = sync_all(&mut app, &s.cfg);
        assert!(!matches!(only(&report).outcome, Outcome::Failed(_)), "{step:?}");
        assert_eq!(alpha(&s.w), "v1", "{step:?}");
        assert_eq!(s.w.skill_names(), ["alpha", "beta", "gamma"], "{step:?}");
        assert!(s.w.work_leftovers().is_empty(), "{step:?}");
    }
}

const EDIT: &str = "my precious edit\n";

fn edit_is_somewhere(w: &World) -> bool {
    w.snapshot().values().any(|bytes| String::from_utf8_lossy(bytes).contains(EDIT))
}

fn edit_is_safe(w: &World) -> bool {
    let live = w.exists("alpha/SKILL.md") && w.read("alpha/SKILL.md") == EDIT;
    let backed_up = w.backups().iter().any(|b| fs::read_to_string(b.join("SKILL.md")).is_ok_and(|text| text == EDIT));
    live || backed_up
}

#[test]
fn a_crash_during_an_overwrite_never_loses_the_edit() {
    for step in [Step::Staged, Step::Marked, Step::OldMoved, Step::NewInPlace, Step::Settled] {
        let mut s = Setup::installed(&format!("cut-overwrite-{step:?}"));
        s.w.write("alpha/SKILL.md", EDIT);
        s.remote.put(REPO, "skills/alpha/SKILL.md", ALPHA_V2);
        s.engine.set_step_hook(crash_once_at(step));

        let err = s.engine.resolve(&s.cfg, "fx", "alpha", Resolution::Overwrite, &cancel_flag()).unwrap_err();
        assert!(err.to_string().contains("power"), "{step:?}: {err}");
        assert!(edit_is_somewhere(&s.w), "{step:?}: the edit is gone right after the crash");

        let mut app = s.restarted();
        app.startup(&s.cfg).unwrap();

        assert!(edit_is_safe(&s.w), "{step:?}: after recovery the edit is neither in the folder nor in a backup");
        assert!(!s.w.state().has_pending(), "{step:?}");
        let left = s.w.work_leftovers();
        assert!(left.iter().all(|n| n.starts_with("backup-")), "{step:?}: {left:?}");
    }
}

#[test]
fn a_folder_put_back_by_hand_after_a_crash_is_never_replaced() {
    // The update was cut off with the old folder moved aside, so alpha is missing until the next start.
    let s = update_cut_off_at("by-hand", Step::OldMoved);
    assert_eq!(alpha(&s.w), "missing");
    s.w.write("alpha/SKILL.md", "my own alpha\n");

    let mut app = s.restarted();
    let started = app.startup(&s.cfg).unwrap();

    assert_eq!(s.w.files_of("alpha").len(), 1);
    assert_eq!(s.w.read("alpha/SKILL.md"), "my own alpha\n");
    let status = status_of(&started.verify, "alpha");
    assert!(matches!(status, LocalStatus::Checked(LocalState::Dirty(_))), "recovery must not call the user's folder clean: {status:?}");

    // Upstream moves on again: the user's folder is held, not replaced.
    s.remote.put(REPO, "skills/alpha/SKILL.md", "# alpha v3\n");
    let report = sync_all(&mut app, &s.cfg);
    let verdict = &skill(only(&report), "alpha").verdict;
    assert!(matches!(verdict, Verdict::Held(Hold::UpdateBlocked(_))), "{verdict:?}");
    assert_eq!(s.w.read("alpha/SKILL.md"), "my own alpha\n");
    assert_eq!(s.w.files_of("alpha").len(), 1);
}

#[test]
fn an_interrupted_change_that_cannot_be_sorted_out_yet_is_left_alone_and_tried_again() {
    let s = update_cut_off_at("stuck", Step::OldMoved);
    assert_eq!(alpha(&s.w), "missing");
    let old = work_folder_starting_with(&s.w, "old-");
    // Another program has the saved old version open, so it cannot be moved back for now.
    let holder = hold_open(&old);

    let mut app = s.restarted();
    let started = app.startup(&s.cfg).unwrap();
    assert!(started.verify.notes[0].text.contains("could not restore the previous version yet"), "{:?}", started.verify.notes);
    assert_eq!(status_of(&started.verify, "alpha"), LocalStatus::Interrupted);
    assert!(s.w.state().has_pending());
    assert!(old.join("SKILL.md").exists(), "the saved version is still there");

    // A check in this state decides nothing about alpha, however much upstream has changed.
    let report = sync_all(&mut app, &s.cfg);
    assert_eq!(skill(only(&report), "alpha").verdict, Verdict::Held(Hold::Interrupted));
    assert!(matches!(only(&report).outcome, Outcome::NeedsAttention), "{:?}", only(&report).outcome);
    assert!(!s.w.exists("alpha"));

    drop(holder);
    let started = s.restarted().startup(&s.cfg).unwrap();
    assert!(started.verify.notes[0].text.contains("the previous version is back"), "{:?}", started.verify.notes);
    assert_eq!(alpha(&s.w), "v1");
    assert!(!s.w.state().has_pending());
    assert!(s.w.work_leftovers().is_empty(), "{:?}", s.w.work_leftovers());
}

#[test]
fn start_up_removes_half_made_leftovers_but_never_a_backup() {
    let s = Setup::installed("sweep");
    let work = s.w.work_dir();
    for (dir, text) in [("stage-1a", "half"), ("junk-2b", "junk"), ("backup-alpha-3c", "mine")] {
        fs::create_dir_all(work.join(dir)).unwrap();
        fs::write(work.join(dir).join("f.txt"), text).unwrap();
    }

    s.restarted().startup(&s.cfg).unwrap();

    assert_eq!(s.w.work_leftovers(), ["backup-alpha-3c"]);
    assert_eq!(fs::read_to_string(work.join("backup-alpha-3c").join("f.txt")).unwrap(), "mine");
}

// ---------------------------------------------------------------------------------------------------------------------
// Other programs, and other copies of this one
// ---------------------------------------------------------------------------------------------------------------------

#[test]
fn a_second_engine_gives_up_quickly_while_one_is_working() {
    let mut s = Setup::fresh("busy");
    let mut other = s.w.engine_with(&s.remote, &s.tokens);
    other.set_lock_wait(Duration::from_millis(60));
    let cfg = s.cfg.clone();
    let answers = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&answers);
    // The hook runs inside the first engine's check, while it holds the lock.
    s.remote.on_call(move |line| {
        if !line.starts_with("tree") {
            return;
        }
        let cancel = AtomicBool::new(false);
        let mut out = sink.lock().unwrap();
        out.push(format!("check: {:?}", other.sync(&cfg, &Which::All, &SyncOptions::default(), &cancel).map(|_| ())));
        out.push(format!("dry run: {:?}", other.sync(&cfg, &Which::All, &dry_run(), &cancel).map(|_| ())));
        out.push(format!("verify: {:?}", other.verify(&cfg, Access::ReadOnly).map(|_| ())));
        out.push(format!("start-up: {:?}", other.startup(&cfg).map(|_| ())));
    });

    let report = s.sync();

    assert!(matches!(only(&report).outcome, Outcome::Synced), "{:?}", only(&report).outcome);
    let answers = answers.lock().unwrap();
    assert_eq!(answers.len(), 4, "{answers:?}");
    for answer in answers.iter() {
        assert!(answer.ends_with("Err(Busy)"), "{answer}");
    }
    assert_eq!(s.remote.calls().tree, 1, "the ones that gave up made no request");
    assert_eq!(s.w.skill_names(), ["alpha", "beta", "gamma"]);
}

#[test]
fn an_engine_notices_when_another_one_changed_the_list_of_managed_skills() {
    let mut s = Setup::installed("reread");
    s.w.write("alpha/SKILL.md", "my own words\n");
    s.remote.put(REPO, "skills/alpha/SKILL.md", ALPHA_V2);

    // The command line is told to leave alpha alone while the tray's engine sits with its own copy of the state.
    let mut cli = s.restarted();
    cli.resolve(&s.cfg, "fx", "alpha", Resolution::KeepMine, &cancel_flag()).unwrap();

    let report = s.sync();

    assert_eq!(skill(only(&report), "alpha").verdict, Verdict::Skipped(Skip::Ignored));
    assert_eq!(s.w.read("alpha/SKILL.md"), "my own words\n");
}

#[test]
fn a_change_the_user_asked_for_fails_loudly_when_it_cannot_be_saved() {
    let mut s = Setup::installed("save-fails");
    s.w.write("alpha/SKILL.md", "my own words\n");
    let before = s.w.snapshot();
    // Another program has the file open and does not let it be replaced.
    let holder = OpenOptions::new().read(true).share_mode(1).open(s.w.state_file()).unwrap();

    let err = s.engine.resolve(&s.cfg, "fx", "alpha", Resolution::KeepMine, &cancel_flag()).unwrap_err();

    assert!(err.to_string().contains("could not save"), "{err}");
    drop(holder);
    assert_eq!(s.w.snapshot(), before, "nothing changed, and no temporary file was left");

    // The engine does not go on acting on what it could not save: it reads the file again, where alpha is still managed.
    let report = sync_with(&mut s.engine, &s.cfg, SyncOptions { ignore_etag: true, ..SyncOptions::default() });
    let verdict = skill(only(&report), "alpha").verdict.clone();
    assert!(matches!(verdict, Verdict::ModifiedLocally(_)), "{verdict:?}");

    // And asking again works.
    s.engine.resolve(&s.cfg, "fx", "alpha", Resolution::KeepMine, &cancel_flag()).unwrap();
    assert!(s.w.state().repo("fx").unwrap().is_ignored("alpha"));
}

#[test]
fn a_damaged_list_of_managed_skills_is_kept_aside_and_nothing_is_deleted_because_of_it() {
    let s = Setup::installed("damaged");
    s.remote.remove(REPO, "skills/gamma");
    fs::write(s.w.state_file(), "{ this is not json").unwrap();
    let mut app = s.restarted();

    // Looking alone leaves the damaged file exactly where it is.
    let before = s.w.snapshot();
    let looked = app.verify(&s.cfg, Access::ReadOnly).unwrap();
    assert!(looked.warnings.iter().any(|w| w.contains("state.json")), "{:?}", looked.warnings);
    assert_eq!(s.w.snapshot(), before);

    let report = sync_all(&mut app, &s.cfg);

    assert!(report.warnings.iter().any(|w| w.contains("state.json")), "{:?}", report.warnings);
    let kept: Vec<String> = fs::read_dir(&s.w.home)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("state.json.damaged-"))
        .collect();
    assert_eq!(kept.len(), 1, "{kept:?}");
    let repo = only(&report);
    // Nothing is known to be owned, so nothing is deleted; identical folders are taken over without a download.
    assert_eq!(skill(repo, "alpha").verdict, Verdict::Adopt);
    assert_eq!(skill(repo, "beta").verdict, Verdict::Adopt);
    assert!(repo.skills.iter().all(|k| k.name != "gamma"));
    assert_eq!(s.w.skill_names(), ["alpha", "beta", "gamma"]);
    assert_eq!(s.remote.calls().file, 0);
    assert_eq!(s.w.state().repo("fx").unwrap().skills.len(), 2);
}

// ---------------------------------------------------------------------------------------------------------------------
// The drive
// ---------------------------------------------------------------------------------------------------------------------

#[test]
fn a_changed_drive_means_nothing_stored_is_trusted_for_that_pass() {
    let mut s = Setup::installed("volume");
    let steady = s.engine.verify(&s.cfg, Access::ReadOnly).unwrap();
    let journal = steady.tracking.journal;
    assert!(!steady.tracking.volume_changed);
    if journal {
        assert_eq!((steady.tracking.check, steady.stats.hashed), (Check::Quick, 0), "installed files carry fingerprints from the moment they were written");
    }

    // The state says it was written on another drive.
    let mut state = s.w.state();
    let stored = state.volume.expect("the drive is recorded");
    state.volume = Some(VolumeId { serial: stored.serial ^ 0xFFFF, journal: stored.journal });
    state.save(&s.w.state_file()).unwrap();
    let mut app = s.restarted();

    let first = app.verify(&s.cfg, Access::Write).unwrap();
    assert!(first.tracking.volume_changed);
    assert_eq!(first.tracking.check, Check::Full);
    assert_eq!(first.stats.hashed, 5, "every file is read: 3 + 1 + 1");
    for name in ["alpha", "beta", "gamma"] {
        assert_eq!(status_of(&first, name), clean(), "{name}");
    }
    assert_eq!(s.w.state().volume, Some(stored), "the right drive is remembered again");

    let second = app.verify(&s.cfg, Access::Write).unwrap();
    assert!(!second.tracking.volume_changed);
    if journal {
        assert_eq!((second.tracking.check, second.stats.hashed), (Check::Quick, 0));
    } else {
        assert_eq!((second.tracking.check, second.stats.hashed), (Check::Full, 5));
    }
}

#[test]
fn a_first_install_into_a_skills_folder_that_does_not_exist_yet_is_trusted_by_the_next_session() {
    let mut s = Setup::fresh("volume-new-folder");
    fs::remove_dir(&s.w.skills).unwrap();
    let report = s.sync();
    assert!(matches!(only(&report).outcome, Outcome::Synced), "{report:?}");
    assert_eq!(s.w.skill_names(), ["alpha", "beta", "gamma"]);
    assert!(s.w.state().volume.is_some(), "the drive is recorded with the first install");

    let next = s.restarted().verify(&s.cfg, Access::Write).unwrap();
    assert!(!next.tracking.volume_changed, "the fingerprints the install wrote are kept");
    if next.tracking.journal {
        assert_eq!((next.tracking.check, next.stats.hashed), (Check::Quick, 0), "nothing is read again");
    }
}

#[test]
fn a_changed_drive_forgets_the_fingerprints_that_the_check_cannot_renew() {
    let mut s = Setup::installed("volume-forgets");
    let journal = s.engine.verify(&s.cfg, Access::ReadOnly).unwrap().tracking.journal;
    let fingerprint = |w: &World, file: &str| {
        let state = w.state();
        let skill = state.repo("fx").unwrap().skill("alpha").unwrap();
        skill.files.iter().find(|f| f.path == file).unwrap_or_else(|| panic!("alpha has no {file}")).fp
    };
    if journal {
        assert!(fingerprint(&s.w, "SKILL.md").is_some(), "the install recorded a fingerprint");
    }
    s.w.write("alpha/SKILL.md", "edited while the drive was somewhere else\n");

    let mut state = s.w.state();
    let stored = state.volume.expect("the drive is recorded");
    state.volume = Some(VolumeId { serial: stored.serial ^ 0xFFFF, journal: stored.journal });
    state.save(&s.w.state_file()).unwrap();
    let report = s.restarted().verify(&s.cfg, Access::Write).unwrap();

    assert!(report.tracking.volume_changed);
    assert!(matches!(status_of(&report, "alpha"), LocalStatus::Checked(LocalState::Dirty(_))));
    // An edited file is not renewed, so nothing recorded for it on the other drive may stay behind.
    assert_eq!(fingerprint(&s.w, "SKILL.md"), None);
    if journal {
        assert!(fingerprint(&s.w, "references/guide.md").is_some(), "files that match are fingerprinted afresh");
    }
}

#[test]
fn a_record_from_an_older_version_learns_how_to_compare_its_files_at_the_first_start_and_keeps_it() {
    let s = Setup::installed("learn-canon");
    let mut state = s.w.state();
    for skill in state.repos.values_mut().flat_map(|r| r.skills.iter_mut()) {
        skill.files.iter_mut().for_each(|f| f.canon = None);
    }
    state.save(&s.w.state_file()).unwrap();

    let first = s.restarted().startup(&s.cfg).unwrap();
    assert_eq!(first.verify.stats.hashed, 5, "every file is read once: 3 + 1 + 1");
    let saved = s.w.state();
    assert!(saved.repo("fx").unwrap().skills.iter().flat_map(|k| &k.files).all(|f| f.canon.is_some()), "what was learned is saved");

    let next = s.restarted().startup(&s.cfg).unwrap();
    if next.verify.tracking.journal {
        assert_eq!(next.verify.stats.hashed, 0, "nothing is read again");
    }
}

#[test]
fn a_machine_without_a_skills_folder_gets_one() {
    let mut s = Setup::fresh("no-folder");
    fs::remove_dir(&s.w.skills).unwrap();

    let started = s.engine.startup(&s.cfg).unwrap();
    assert!(started.problem.is_none(), "{:?}", started.problem);
    assert!(started.verify.skills.is_empty());

    let report = s.sync();
    assert!(matches!(only(&report).outcome, Outcome::Synced), "{:?}", only(&report).outcome);
    assert_eq!(s.w.skill_names(), ["alpha", "beta", "gamma"]);
}

// ---------------------------------------------------------------------------------------------------------------------
// The skills folder
// ---------------------------------------------------------------------------------------------------------------------

/// Another skills folder in the same world, so it goes when the world goes.
fn elsewhere(w: &World) -> PathBuf {
    w.root.join("elsewhere").join("skills")
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    names
}

#[test]
fn a_new_skills_folder_gets_its_own_copies_and_the_old_one_is_left_as_it_is() {
    let mut s = Setup::installed("moved");
    s.w.write("alpha/SKILL.md", "my own words\n");
    let old = s.w.skills_snapshot();
    let new = elsewhere(&s.w);
    s.engine.set_skills_dir(new.clone());
    let old_name = s.w.skills.display().to_string();
    let told = |warnings: &[String]| warnings.iter().any(|w| w.contains(&old_name) && w.contains("left there"));

    // A look before the next check finds nothing installed, rather than three skills deleted by hand.
    let look = s.engine.verify(&s.cfg, Access::ReadOnly).unwrap();
    assert!(look.skills.is_empty(), "{:?}", look.skills);
    assert!(told(&look.warnings), "{:?}", look.warnings);

    let report = s.sync();

    assert!(matches!(only(&report).outcome, Outcome::Synced), "{report:?}");
    for name in ["alpha", "beta", "gamma"] {
        assert_eq!(skill(only(&report), name).verdict, Verdict::Install, "{name}");
    }
    assert!(told(&report.warnings), "{:?}", report.warnings);
    assert_eq!(names_in(&new), ["alpha", "beta", "gamma"]);
    assert_eq!(s.w.skills_snapshot(), old, "the old folder, edit and all, is as it was");
    let state = s.w.state();
    assert_eq!(state.skills_dir.as_deref(), new.to_str());
    assert_eq!(state.repo("fx").unwrap().skills.len(), 3);

    let next = s.sync();
    assert!(next.warnings.is_empty(), "it is said once: {:?}", next.warnings);
}

#[test]
fn going_back_to_a_folder_used_before_manages_its_skills_as_they_were() {
    let mut s = Setup::installed("moved-back");
    let home = s.w.skills.clone();
    let new = elsewhere(&s.w);
    s.engine.set_skills_dir(new.clone());
    assert!(matches!(only(&s.sync()).outcome, Outcome::Synced));
    s.w.write("alpha/SKILL.md", "edited while the app looked elsewhere\n");
    let there = snapshot_of(&new);
    s.remote.put(REPO, "skills/alpha/SKILL.md", ALPHA_V2);
    s.remote.reset();

    s.engine.set_skills_dir(home.clone());
    let report = s.sync();

    let back = format!("back to {}", home.display());
    assert!(report.warnings.iter().any(|w| w.contains(&back) && w.contains("left there")), "{:?}", report.warnings);
    let repo = only(&report);
    assert!(
        matches!(skill(repo, "alpha").verdict, Verdict::Held(Hold::UpdateBlocked(_))),
        "still known as an edit of a managed skill: {:?}",
        skill(repo, "alpha").verdict
    );
    assert_eq!(skill(repo, "beta").verdict, Verdict::InSync);
    assert_eq!(skill(repo, "gamma").verdict, Verdict::InSync);
    assert_eq!(s.w.read("alpha/SKILL.md"), "edited while the app looked elsewhere\n");
    assert_eq!(snapshot_of(&new), there, "the folder it left is as it was");
    assert_eq!(s.remote.calls().file, 0);
    let state = s.w.state();
    assert_eq!(state.set_aside.len(), 1);
    assert_eq!(state.set_aside[0].skills_dir, new.display().to_string());
}

#[test]
fn a_change_cut_off_in_the_old_folder_is_left_there_and_never_finished_in_the_new_one() {
    let s = update_cut_off_at("moved-mid-change", Step::OldMoved);
    assert_eq!(alpha(&s.w), "missing");
    let saved = work_folder_starting_with(&s.w, "old-");
    let old_skills = s.w.skills_snapshot();
    let old_work = snapshot_of(&s.w.work_dir());
    let new = elsewhere(&s.w);

    let mut app = s.restarted();
    app.set_skills_dir(new.clone());
    let started = app.startup(&s.cfg).unwrap();

    let work_name = s.w.work_dir().display().to_string();
    assert!(started.verify.warnings.iter().any(|w| w.contains("alpha was interrupted") && w.contains(&work_name)), "{:?}", started.verify.warnings);
    assert!(started.verify.notes.is_empty(), "nothing was sorted out: {:?}", started.verify.notes);
    let state = s.w.state();
    assert!(!state.has_pending());
    assert!(state.has_pending_set_aside(), "kept for when the old folder is used again");

    let report = sync_all(&mut app, &s.cfg);
    assert_eq!(skill(only(&report), "alpha").verdict, Verdict::Install);
    assert_eq!(fs::read_to_string(new.join("alpha").join("SKILL.md")).unwrap(), ALPHA_V2);
    assert_eq!(s.w.skills_snapshot(), old_skills, "the old folder is as the crash left it");
    assert_eq!(snapshot_of(&s.w.work_dir()), old_work, "and so is its work folder");
    assert!(saved.join("SKILL.md").exists());
}

#[test]
fn a_change_cut_off_in_a_folder_left_behind_is_never_finished_in_the_folder_next_to_it() {
    // The two folders share one work folder, and the new one holds the user's own folder of the same name.
    let s = update_cut_off_at("moved-next-door", Step::OldMoved);
    let next_door = s.w.root.join("skills2");
    fs::create_dir_all(next_door.join("alpha")).unwrap();
    fs::write(next_door.join("alpha").join("SKILL.md"), "my own alpha\n").unwrap();
    let mine = snapshot_of(&next_door);
    let old_skills = s.w.skills_snapshot();
    let work = snapshot_of(&s.w.work_dir());
    assert!(work.keys().any(|k| k.starts_with("stage-")) && work.keys().any(|k| k.starts_with("old-")), "{:?}", work.keys());

    let mut app = s.restarted();
    app.set_skills_dir(next_door.clone());
    let started = app.startup(&s.cfg).unwrap();

    assert!(started.verify.notes.is_empty(), "{:?}", started.verify.notes);
    assert_eq!(snapshot_of(&next_door), mine, "the user's own alpha is untouched");
    assert_eq!(s.w.skills_snapshot(), old_skills);
    assert_eq!(snapshot_of(&s.w.work_dir()), work, "nothing in the shared work folder was finished, undone or swept");

    // Back in the first folder, the change is sorted out where it started.
    let mut back = s.restarted();
    let started = back.startup(&s.cfg).unwrap();
    assert_eq!(started.verify.notes.len(), 1, "{:?}", started.verify.notes);
    assert!(started.verify.notes[0].text.contains("the previous version is back"), "{}", started.verify.notes[0].text);
    assert_eq!(alpha(&s.w), "v1");
    assert_eq!(snapshot_of(&next_door), mine);
    let state = s.w.state();
    assert!(!state.has_pending() && !state.has_pending_set_aside());
}

#[test]
fn a_choice_a_removal_or_a_late_write_waits_until_a_check_has_taken_in_a_new_skills_folder() {
    let mut s = Setup::installed("moved-then-asked");
    s.advance(86_400);
    assert!(matches!(only(&sync_which(&mut s.engine, &s.cfg, &Which::Due)).outcome, Outcome::Unchanged));
    let state_file = s.w.home.join("state.json");
    let on_disk = fs::read(&state_file).unwrap();
    let skills = s.w.skills_snapshot();
    let new = elsewhere(&s.w);
    s.engine.set_skills_dir(new.clone());
    let cancel = AtomicBool::new(false);

    let refused = s.engine.resolve(&s.cfg, "fx", "alpha", Resolution::KeepMine, &cancel).unwrap_err().to_string();
    assert!(refused.contains("skills folder changed"), "{refused}");
    let refused = s.engine.remove_repo("fx", true).unwrap_err().to_string();
    assert!(refused.contains("skills folder changed"), "{refused}");
    s.engine.flush().unwrap();

    assert_eq!(fs::read(&state_file).unwrap(), on_disk, "nothing was saved");
    assert_eq!(s.w.skills_snapshot(), skills);
    assert!(!new.exists());

    let report = s.sync();
    assert!(report.warnings.iter().any(|w| w.contains("left there")), "{:?}", report.warnings);
    s.engine.resolve(&s.cfg, "fx", "alpha", Resolution::KeepMine, &cancel).unwrap();
}

#[test]
fn a_repository_removed_while_another_folder_is_used_leaves_its_skills_in_the_first_folder_alone_too() {
    let mut s = Setup::installed("moved-then-removed");
    let home = s.w.skills.clone();
    s.engine.set_skills_dir(elsewhere(&s.w));
    assert!(matches!(only(&s.sync()).outcome, Outcome::Synced));
    s.engine.remove_repo("fx", false).unwrap();
    assert!(s.w.state().set_aside.is_empty(), "{:?}", s.w.state().set_aside);

    // The same repository added again, after going back to the first folder: its skills there are the user's now.
    s.remote.put(REPO, "skills/alpha/SKILL.md", ALPHA_V2);
    s.engine.set_skills_dir(home);
    let report = s.sync();

    let repo = only(&report);
    assert_eq!(skill(repo, "alpha").verdict, Verdict::Held(Hold::UnmanagedDiffers));
    assert_eq!(skill(repo, "beta").verdict, Verdict::Adopt);
    assert_eq!(s.w.read("alpha/SKILL.md"), ALPHA_V1);
}

// ---------------------------------------------------------------------------------------------------------------------
// The schedule
// ---------------------------------------------------------------------------------------------------------------------

#[test]
fn each_repository_is_checked_on_its_own_schedule() {
    let w = World::new("intervals");
    let remote = FakeRemote::new();
    remote.seed("o/fast", &[("skills/f/SKILL.md", "fast\n")]);
    remote.seed("o/slow", &[("skills/s/SKILL.md", "slow\n")]);
    let (mut engine, clock) = w.engine_at(&remote, &FakeTokens::none());
    let fast = RepoConfig { interval: Some(Interval::new(10, Unit::Minute)), ..repo("fast", "o/fast", "skills") };
    let cfg = config(vec![fast, repo("slow", "o/slow", "skills")]);
    let ids = |report: &tiny_skill_sync::engine::SyncReport| {
        let mut ids: Vec<String> = report.repos.iter().map(|r| r.repo_id.clone()).collect();
        ids.sort();
        ids
    };

    assert_eq!(ids(&sync_which(&mut engine, &cfg, &Which::Due)), ["fast", "slow"], "never checked, so both are due");

    clock.fetch_add(599, Ordering::Relaxed);
    assert!(sync_which(&mut engine, &cfg, &Which::Due).repos.is_empty(), "one second early");
    assert_eq!(engine.next_wake(&cfg), Some(T0 + 600));

    clock.fetch_add(1, Ordering::Relaxed);
    assert_eq!(ids(&sync_which(&mut engine, &cfg, &Which::Due)), ["fast"], "the slow one keeps its own time");
    assert_eq!(engine.next_wake(&cfg), Some(T0 + 1200));

    clock.store(T0 + 86_400, Ordering::Relaxed);
    assert_eq!(ids(&sync_which(&mut engine, &cfg, &Which::Due)), ["fast", "slow"]);
}

#[test]
fn a_repository_that_keeps_failing_is_tried_less_and_less_often_and_a_success_resets_it() {
    let mut s = Setup::fresh("backoff");
    s.remote.fail_tree(REPO, Some(network_error()));
    let (mut at, mut wait) = (T0, 300);

    for failures in 1..=3 {
        let report = sync_which(&mut s.engine, &s.cfg, &Which::Due);
        assert!(matches!(&only(&report).outcome, Outcome::Failed(f) if f.kind == FailureKind::Network), "{:?}", only(&report).outcome);
        let retry = s.engine.schedule().retry_of("fx").expect("a retry is planned");
        assert_eq!((retry.failures, retry.not_before), (failures, at + wait), "after failure {failures}");

        s.advance(wait - 1);
        assert!(sync_which(&mut s.engine, &s.cfg, &Which::Due).repos.is_empty(), "one second early, after failure {failures}");
        s.advance(1);
        at += wait;
        wait *= 2;
    }

    s.remote.fail_tree(REPO, None);
    let report = sync_which(&mut s.engine, &s.cfg, &Which::Due);
    assert!(matches!(only(&report).outcome, Outcome::Synced), "{:?}", only(&report).outcome);
    assert!(s.engine.schedule().retry_of("fx").is_none());
    assert_eq!(s.engine.next_wake(&s.cfg), Some(at + 86_400), "back to the normal interval");
}

#[test]
fn a_check_that_found_nothing_new_is_written_down_at_exit_not_before() {
    let mut s = Setup::installed("flush");
    assert_eq!(last_check_on_disk(&s.w), Some(T0));
    s.advance(86_400);

    let report = sync_which(&mut s.engine, &s.cfg, &Which::Due);

    assert!(matches!(only(&report).outcome, Outcome::Unchanged), "{:?}", only(&report).outcome);
    assert_eq!(last_check_on_disk(&s.w), Some(T0), "a check that changed nothing costs no write");
    assert_eq!(s.engine.last_check("fx"), Some(T0 + 86_400));
    assert_eq!(s.engine.next_wake(&s.cfg), Some(T0 + 2 * 86_400));

    // If the app dies now, all that is lost is the time: the next start simply asks again, cheaply.
    let mut crashed = s.restarted();
    assert_eq!(sync_which(&mut crashed, &s.cfg, &Which::Due).repos.len(), 1);

    s.engine.flush().unwrap();
    assert_eq!(last_check_on_disk(&s.w), Some(T0 + 86_400));
    let mut restarted = s.restarted();
    assert!(sync_which(&mut restarted, &s.cfg, &Which::Due).repos.is_empty(), "a clean exit remembers when the check was made");
}

#[test]
fn a_repository_holding_a_skill_is_checked_once_more_after_a_restart_so_the_window_can_show_it() {
    let mut s = Setup::fresh("held-restart");
    s.w.write("alpha/SKILL.md", "my own alpha\n");
    let report = sync_which(&mut s.engine, &s.cfg, &Which::Due);
    assert_eq!(skill(only(&report), "alpha").verdict, Verdict::Held(Hold::UnmanagedDiffers));
    assert!(sync_which(&mut s.engine, &s.cfg, &Which::Due).repos.is_empty(), "not again in the same run");
    s.engine.flush().unwrap();

    // What was held lived only in the memory of the run that ended, so the next start looks again, once.
    let mut restarted = s.restarted();
    let report = sync_which(&mut restarted, &s.cfg, &Which::Due);
    assert_eq!(skill(only(&report), "alpha").verdict, Verdict::Held(Hold::UnmanagedDiffers));
    assert!(sync_which(&mut restarted, &s.cfg, &Which::Due).repos.is_empty(), "once per start");
    assert_eq!(s.w.read("alpha/SKILL.md"), "my own alpha\n");
}

// ---------------------------------------------------------------------------------------------------------------------
// Removing a repository, and looking at what is installed
// ---------------------------------------------------------------------------------------------------------------------

#[test]
fn removing_a_repository_and_keeping_its_skills_leaves_them_alone_for_good() {
    let mut s = Setup::installed("remove-keep");
    let before = s.w.skills_snapshot();

    let report = s.engine.remove_repo("fx", false).unwrap();

    assert!(report.deleted.is_empty());
    assert_eq!(report.kept.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(), ["alpha", "beta", "gamma"]);
    assert_eq!(s.w.skills_snapshot(), before);
    assert!(s.w.state().repo("fx").is_none());

    // They are the user's now. The same repository added again can neither replace nor delete them.
    s.remote.put(REPO, "skills/alpha/SKILL.md", ALPHA_V2);
    s.remote.remove(REPO, "skills/gamma");
    let again = config(vec![repo("again", REPO, "skills")]);
    let report = sync_all(&mut s.engine, &again);
    let repo = only(&report);
    assert_eq!(skill(repo, "alpha").verdict, Verdict::Held(Hold::UnmanagedDiffers));
    assert_eq!(skill(repo, "beta").verdict, Verdict::Adopt, "identical, so it is taken over without a download");
    assert_eq!(s.w.read("alpha/SKILL.md"), ALPHA_V1);
    assert!(s.w.exists("gamma"));
}

#[test]
fn removing_a_repository_and_its_skills_deletes_only_what_is_untouched() {
    let mut s = Setup::installed("remove-delete");
    s.w.write("beta/SKILL.md", "my edit\n");

    let report = s.engine.remove_repo("fx", true).unwrap();

    assert_eq!(report.deleted, ["alpha", "gamma"]);
    assert_eq!(report.kept.len(), 1);
    assert_eq!(report.kept[0].0, "beta");
    assert!(report.kept[0].1.contains("edited locally"), "{}", report.kept[0].1);
    assert_eq!(s.w.skill_names(), ["beta"]);
    assert_eq!(s.w.read("beta/SKILL.md"), "my edit\n");
    assert!(s.w.state().repo("fx").is_none());
    assert!(s.w.work_leftovers().is_empty(), "{:?}", s.w.work_leftovers());
}

#[test]
fn verify_tells_clean_edited_and_missing_skills_apart() {
    let mut s = Setup::installed("verify-kinds");
    let report = s.engine.verify(&s.cfg, Access::ReadOnly).unwrap();
    assert!(report.skills.iter().all(|k| k.status == clean()), "{:?}", report.skills);
    assert!(report.orphans.is_empty());

    s.w.write("alpha/SKILL.md", "edited\n");
    s.w.write("alpha/extra.txt", "not part of the skill\n");
    s.w.remove("beta");
    let report = s.engine.verify(&s.cfg, Access::ReadOnly).unwrap();

    let expected = DirtyInfo { modified: vec!["SKILL.md".into()], missing: Vec::new() };
    assert_eq!(status_of(&report, "alpha"), LocalStatus::Checked(LocalState::Dirty(expected)), "a file the user added is not an edit");
    assert_eq!(status_of(&report, "beta"), LocalStatus::Checked(LocalState::FolderMissing));
    assert_eq!(status_of(&report, "gamma"), clean());
}

#[test]
fn skills_of_a_repository_that_left_the_settings_stay_protected() {
    let mut s = Setup::installed("orphans");

    let report = s.engine.verify(&config(vec![]), Access::ReadOnly).unwrap();
    assert!(report.skills.is_empty());
    assert_eq!(report.orphans, ["fx"]);

    // Another entry for the same repository cannot take the names over while the first still owns them.
    let other = config(vec![repo("other", REPO, "skills")]);
    let report = sync_all(&mut s.engine, &other);
    let repo = only(&report);
    for name in ["alpha", "beta", "gamma"] {
        assert_eq!(skill(repo, name).verdict, Verdict::Skipped(Skip::Collision { owner: "fx".into() }), "{name}");
    }
    assert!(matches!(repo.outcome, Outcome::NeedsAttention), "{:?}", repo.outcome);
    assert_eq!(s.w.skill_names(), ["alpha", "beta", "gamma"]);
    assert!(s.w.state().repo("fx").is_some_and(|r| r.skills.len() == 3));
}
