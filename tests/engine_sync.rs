//! The engine against a fake GitHub: what happens to the skills folder, check by check.

mod common;

use std::fs;

use common::*;
use tiny_skill_sync::diff::{Body, FileChange, Presence};
use tiny_skill_sync::engine::{ApplyResult, DIFF_FILE_BYTES, DiffReport, Outcome, Resolution, SyncOptions, SyncReport};
use tiny_skill_sync::github::{FailureKind, RemoteError};
use tiny_skill_sync::reconcile::{Hold, Skip, Verdict};

fn names(map: &std::collections::BTreeMap<String, String>) -> Vec<&str> {
    map.keys().map(String::as_str).collect()
}

#[test]
fn the_first_check_installs_every_skill_and_nothing_else() {
    let mut s = Setup::fresh("first");
    let report = s.sync();
    let repo = only(&report);

    assert!(matches!(repo.outcome, Outcome::Synced), "{:?}", repo.outcome);
    for name in ["alpha", "beta", "gamma"] {
        assert_eq!(skill(repo, name).verdict, Verdict::Install, "{name}");
        assert_eq!(skill(repo, name).result, ApplyResult::Done, "{name}");
    }
    assert_eq!(repo.skills.len(), 3, "a folder without a SKILL.md is not a skill");
    assert_eq!(s.w.skill_names(), ["alpha", "beta", "gamma"]);
    assert_eq!(names(&s.w.files_of("alpha")), ["SKILL.md", "references/guide.md", "scripts/hello.ps1"]);
    assert_eq!(s.w.read("alpha/references/guide.md"), "guide v1\n");
    assert_eq!(s.w.read("alpha/SKILL.md"), ALPHA_V1);
    assert!(s.w.work_leftovers().is_empty(), "nothing is left in the work folder: {:?}", s.w.work_leftovers());

    let state = s.w.state();
    let rs = state.repo("fx").expect("the repository is recorded");
    assert_eq!(rs.skills.len(), 3);
    assert!(rs.etag.is_some(), "a clean check stores the ETag so that the next one can be a 304");
    assert_eq!(rs.last_check, Some(T0));
    let head = s.remote.head(REPO);
    assert!(rs.skills.iter().all(|k| k.commit == head && k.pending.is_none()));
    // One listing, one lookup of the commit for the whole check (not one per skill), and each of the five files once.
    assert_eq!(s.remote.calls(), Calls { tree: 1, conditional: 0, commit: 1, file: 5, ..Calls::default() });
}

#[test]
fn a_check_that_finds_nothing_new_is_one_request_and_changes_no_file() {
    let mut s = Setup::installed("unchanged");
    s.advance(3600);
    let before = s.w.snapshot();

    let report = s.sync();
    let repo = only(&report);

    assert!(matches!(repo.outcome, Outcome::Unchanged), "{:?}", repo.outcome);
    assert_eq!(s.remote.calls(), Calls { tree: 1, conditional: 1, commit: 0, file: 0, ..Calls::default() });
    assert_eq!(s.w.snapshot(), before, "not one file was created, changed or removed, not even state.json");
    assert_eq!(s.engine.last_check("fx"), Some(T0 + 3600), "the check is remembered in memory");
}

#[test]
fn an_update_replaces_only_the_skill_that_changed() {
    let mut s = Setup::installed("update");
    s.remote.put(REPO, "skills/alpha/SKILL.md", ALPHA_V2);
    s.remote.remove(REPO, "skills/alpha/scripts");
    s.remote.put(REPO, "skills/alpha/references/extra.md", "extra\n");

    let report = s.sync();
    let repo = only(&report);

    assert!(matches!(repo.outcome, Outcome::Synced), "{:?}", repo.outcome);
    assert_eq!(skill(repo, "alpha").verdict, Verdict::Update);
    assert_eq!(skill(repo, "alpha").result, ApplyResult::Done);
    assert_eq!(skill(repo, "beta").verdict, Verdict::InSync);
    assert_eq!(skill(repo, "gamma").verdict, Verdict::InSync);
    assert_eq!(names(&s.w.files_of("alpha")), ["SKILL.md", "references/extra.md", "references/guide.md"]);
    assert_eq!(s.w.read("alpha/SKILL.md"), ALPHA_V2);
    let fetched: Vec<String> = s.remote.log().into_iter().filter(|l| l.starts_with("file ")).collect();
    assert!(!fetched.is_empty() && fetched.iter().all(|l| l.contains("skills/alpha/")), "only alpha was downloaded: {fetched:?}");
    assert!(s.w.backups().is_empty(), "a skill nobody touched needs no backup");
    assert!(s.w.work_leftovers().is_empty(), "{:?}", s.w.work_leftovers());
}

#[test]
fn a_skill_edited_locally_is_never_overwritten_and_is_updated_once_the_edit_is_undone() {
    let mut s = Setup::installed("held");
    s.w.write("alpha/SKILL.md", "my own words\n");
    s.remote.put(REPO, "skills/alpha/SKILL.md", ALPHA_V2);

    let report = s.sync();
    let repo = only(&report);
    assert!(matches!(repo.outcome, Outcome::NeedsAttention), "{:?}", repo.outcome);
    assert!(matches!(skill(repo, "alpha").verdict, Verdict::Held(Hold::UpdateBlocked(_))), "{:?}", skill(repo, "alpha").verdict);
    assert_eq!(s.w.read("alpha/SKILL.md"), "my own words\n", "the user's edit is untouched");
    assert!(s.w.backups().is_empty());
    assert!(s.remote.calls().file == 0, "nothing was downloaded for a skill that cannot be updated");

    // Still held at the next check: the ETag was not stored, so the listing is looked at again.
    s.advance(60);
    s.remote.reset();
    let report = s.sync();
    assert!(matches!(skill(only(&report), "alpha").verdict, Verdict::Held(Hold::UpdateBlocked(_))));
    assert_eq!(s.remote.calls().tree, 1);
    assert_eq!(s.remote.calls().conditional, 0, "a held skill means the next check asks for the full listing");

    // The user undoes the edit: the next check applies the update by itself.
    s.w.write("alpha/SKILL.md", ALPHA_V1);
    let report = s.sync();
    assert_eq!(skill(only(&report), "alpha").result, ApplyResult::Done);
    assert_eq!(s.w.read("alpha/SKILL.md"), ALPHA_V2);
}

#[test]
fn an_edit_with_no_upstream_change_is_reported_and_left_alone() {
    let mut s = Setup::installed("edited");
    s.w.write("beta/SKILL.md", "beta, my way\n");
    s.w.write("beta/notes.txt", "extra file\n");

    // Upstream did not change, so the check is a 304 and reads nothing.
    let report = s.sync();
    assert!(matches!(only(&report).outcome, Outcome::Unchanged));
    assert_eq!(s.w.read("beta/SKILL.md"), "beta, my way\n");

    // Whenever a listing is looked at, the edit is a status, never a reason to change anything.
    let report = sync_with(&mut s.engine, &s.cfg, SyncOptions { ignore_etag: true, ..SyncOptions::default() });
    let repo = only(&report);
    assert!(matches!(skill(repo, "beta").verdict, Verdict::ModifiedLocally(_)), "{:?}", skill(repo, "beta").verdict);
    assert!(matches!(repo.outcome, Outcome::NeedsAttention));
    assert_eq!(s.w.read("beta/SKILL.md"), "beta, my way\n");
    assert_eq!(s.w.read("beta/notes.txt"), "extra file\n");
    assert!(s.w.backups().is_empty());
}

#[test]
fn overwrite_takes_the_upstream_version_and_keeps_the_edited_copy() {
    let mut s = Setup::installed("overwrite");
    s.w.write("alpha/SKILL.md", "my own words\n");
    s.w.write("alpha/mine.txt", "only mine\n");
    s.remote.put(REPO, "skills/alpha/SKILL.md", ALPHA_V2);
    s.sync();

    let report = s.engine.resolve(&s.cfg, "fx", "alpha", Resolution::Overwrite, &cancel_flag()).expect("overwrite works");

    assert_eq!(s.w.read("alpha/SKILL.md"), ALPHA_V2);
    assert_eq!(s.w.read("alpha/mine.txt"), "only mine\n", "a file the user added is theirs and stays where it is");
    let backups = s.w.backups();
    assert_eq!(backups.len(), 1, "{backups:?}");
    assert_eq!(report.backups, backups);
    assert_eq!(fs::read_to_string(backups[0].join("SKILL.md")).unwrap(), "my own words\n");
    assert!(!backups[0].join("mine.txt").exists());

    // And it is managed normally again.
    let report = s.sync();
    assert_eq!(skill(only(&report), "alpha").verdict, Verdict::InSync);
}

#[test]
fn a_skill_deleted_locally_is_not_reinstalled_unless_asked_to() {
    let mut s = Setup::installed("deleted");
    s.w.remove("beta");

    let report = sync_with(&mut s.engine, &s.cfg, SyncOptions { ignore_etag: true, ..SyncOptions::default() });
    let repo = only(&report);
    assert_eq!(skill(repo, "beta").verdict, Verdict::MissingLocally);
    assert!(!s.w.exists("beta"), "a skill the user deleted stays deleted");
    assert_eq!(s.remote.calls().file, 0);

    s.engine.resolve(&s.cfg, "fx", "beta", Resolution::Overwrite, &cancel_flag()).expect("restore works");
    assert_eq!(s.w.read("beta/SKILL.md"), "---\nname: beta\ndescription: second skill\n---\n# Beta v1\n");
    assert!(s.w.backups().is_empty(), "there was nothing to keep");
}

#[test]
fn keep_mine_stops_managing_a_skill_and_manage_again_resumes() {
    let mut s = Setup::installed("keep");
    s.w.write("alpha/SKILL.md", "mine\n");
    s.remote.put(REPO, "skills/alpha/SKILL.md", ALPHA_V2);

    s.engine.resolve(&s.cfg, "fx", "alpha", Resolution::KeepMine, &cancel_flag()).expect("keep works");
    let state = s.w.state();
    assert!(state.repo("fx").unwrap().skill("alpha").is_none(), "no longer managed");
    assert!(state.repo("fx").unwrap().is_ignored("alpha"));

    // Whatever upstream does, the folder is left alone, even when upstream removes the skill.
    let report = s.sync();
    assert_eq!(skill(only(&report), "alpha").verdict, Verdict::Skipped(Skip::Ignored));
    s.remote.remove(REPO, "skills/alpha");
    let report = s.sync();
    assert!(only(&report).skills.iter().all(|k| k.name != "alpha" || !k.verdict.is_action()));
    assert_eq!(s.w.read("alpha/SKILL.md"), "mine\n");

    // Managing it again: the folder differs from anything upstream has, so it is held, never overwritten.
    s.remote.put(REPO, "skills/alpha/SKILL.md", ALPHA_V2);
    s.engine.resolve(&s.cfg, "fx", "alpha", Resolution::ManageAgain, &cancel_flag()).expect("manage again works");
    let report = s.sync();
    assert_eq!(skill(only(&report), "alpha").verdict, Verdict::Held(Hold::UnmanagedDiffers));
    assert_eq!(s.w.read("alpha/SKILL.md"), "mine\n");
}

#[test]
fn a_decision_about_a_skill_makes_the_next_check_look_at_the_repository_again() {
    // Upstream never changes here, so only a forgotten ETag could make the next check look again. A check that
    // looked at the listing reports every skill; one that was answered 'not modified' reports none.
    fn looked_again(s: &mut Setup) -> bool {
        !only(&s.sync()).skills.is_empty()
    }

    let mut s = Setup::installed("decide-keep");
    s.w.write("alpha/SKILL.md", "mine\n");
    s.engine.resolve(&s.cfg, "fx", "alpha", Resolution::KeepMine, &cancel_flag()).unwrap();
    assert!(looked_again(&mut s), "after 'keep mine'");
    assert!(!looked_again(&mut s), "and not once that is settled");
    s.engine.resolve(&s.cfg, "fx", "alpha", Resolution::ManageAgain, &cancel_flag()).unwrap();
    assert!(looked_again(&mut s), "after 'manage again'");

    let mut s = Setup::installed("decide-overwrite");
    s.w.write("alpha/SKILL.md", "mine\n");
    s.engine.resolve(&s.cfg, "fx", "alpha", Resolution::Overwrite, &cancel_flag()).unwrap();
    assert!(looked_again(&mut s), "after 'overwrite'");
}

#[test]
fn what_the_user_did_to_a_skill_is_a_status_not_pending_work_so_polls_stay_cheap() {
    // Once a check has looked at the whole listing and found nothing it can do, the next one is a plain
    // 'not modified' again: one request, nothing downloaded.
    fn full_listing(s: &mut Setup) -> SyncReport {
        sync_with(&mut s.engine, &s.cfg, SyncOptions { ignore_etag: true, ..SyncOptions::default() })
    }
    fn next_check_is_not_modified(s: &mut Setup) -> bool {
        s.remote.reset();
        let report = s.sync();
        only(&report).skills.is_empty() && s.remote.calls() == Calls { tree: 1, conditional: 1, commit: 0, file: 0, ..Calls::default() }
    }

    let mut s = Setup::installed("cheap-edited");
    s.w.write("alpha/SKILL.md", "mine\n");
    let report = full_listing(&mut s);
    assert!(matches!(skill(only(&report), "alpha").verdict, Verdict::ModifiedLocally(_)));
    assert!(next_check_is_not_modified(&mut s), "a skill the user edited");

    let mut s = Setup::installed("cheap-deleted");
    s.w.remove("beta");
    let report = full_listing(&mut s);
    assert_eq!(skill(only(&report), "beta").verdict, Verdict::MissingLocally);
    assert!(next_check_is_not_modified(&mut s), "a skill the user deleted");

    // Two folders whose names differ only by case cannot both exist on Windows, so upstream's pair is refused.
    let mut s = Setup::installed("cheap-unsafe");
    s.remote.put(REPO, "skills/Alpha/SKILL.md", "---\nname: Alpha\n---\n");
    let report = s.sync();
    let verdict = &skill(only(&report), "alpha").verdict;
    assert!(matches!(verdict, Verdict::Skipped(Skip::Rejected { .. })), "{verdict:?}");
    assert!(next_check_is_not_modified(&mut s), "a skill that upstream made unsafe to install");
}

#[test]
fn a_skill_that_lost_a_name_clash_is_installed_as_soon_as_the_name_is_free() {
    let w = World::new("clash-freed");
    let remote = FakeRemote::new();
    remote.put("o/one", "skills/alpha/SKILL.md", "one\n");
    remote.put("o/one", "skills/other/SKILL.md", "other\n");
    remote.put("o/two", "skills/alpha/SKILL.md", "two\n");
    let cfg = config(vec![repo("one", "o/one", "skills"), repo("two", "o/two", "skills")]);
    let mut engine = w.engine(&remote);
    let report = sync_all(&mut engine, &cfg);
    assert!(matches!(skill(&report.repos[1], "alpha").verdict, Verdict::Skipped(Skip::Collision { .. })));

    // The first repository lets go of the name. The second one itself has not changed at all.
    remote.remove("o/one", "skills/alpha");
    let report = sync_all(&mut engine, &cfg);

    assert_eq!(skill(&report.repos[0], "alpha").result, ApplyResult::Done);
    assert_eq!(skill(&report.repos[1], "alpha").verdict, Verdict::Install);
    assert_eq!(w.read("alpha/SKILL.md"), "two\n");
}

#[test]
fn a_skill_removed_upstream_is_deleted_only_when_it_is_untouched() {
    let mut s = Setup::installed("removed");
    s.remote.remove(REPO, "skills/gamma");
    s.remote.remove(REPO, "skills/beta");
    s.w.write("beta/SKILL.md", "beta, my way\n");

    let report = s.sync();
    let repo = only(&report);
    assert_eq!(skill(repo, "gamma").verdict, Verdict::Delete);
    assert_eq!(skill(repo, "gamma").result, ApplyResult::Done);
    assert!(matches!(skill(repo, "beta").verdict, Verdict::Held(Hold::DeleteBlocked(_))), "{:?}", skill(repo, "beta").verdict);
    assert!(matches!(repo.outcome, Outcome::NeedsAttention));
    assert_eq!(s.w.skill_names(), ["alpha", "beta"]);
    assert_eq!(s.w.read("beta/SKILL.md"), "beta, my way\n");
    let state = s.w.state();
    assert!(state.repo("fx").unwrap().skill("gamma").is_none());
    assert!(state.repo("fx").unwrap().skill("beta").is_some(), "still recorded, still protected");
    assert!(s.w.work_leftovers().is_empty(), "{:?}", s.w.work_leftovers());
}

#[test]
fn a_skill_removed_upstream_takes_only_its_own_files_and_leaves_the_ones_the_user_added() {
    let mut s = Setup::installed("removed-keeps-mine");
    s.w.write("alpha/notes/mine.md", "my notes\n");
    s.w.write("alpha/references/more.md", "more\n");
    s.remote.remove(REPO, "skills/alpha");

    let report = s.sync();
    let repo = only(&report);
    assert_eq!(skill(repo, "alpha").verdict, Verdict::Delete);
    assert_eq!(skill(repo, "alpha").result, ApplyResult::Done);
    let left: Vec<(String, String)> = s.w.files_of("alpha").into_iter().collect();
    assert_eq!(left, [("notes/mine.md".to_string(), "my notes\n".to_string()), ("references/more.md".to_string(), "more\n".to_string())]);
    assert!(s.w.state().repo("fx").unwrap().skill("alpha").is_none(), "what is left is the user's, not the app's");
    assert!(s.w.work_leftovers().is_empty(), "{:?}", s.w.work_leftovers());

    // From now on the folder is the user's: nothing reports it or touches it.
    let report = sync_with(&mut s.engine, &s.cfg, SyncOptions { ignore_etag: true, ..SyncOptions::default() });
    assert!(only(&report).skills.iter().all(|k| k.name != "alpha"), "{:?}", only(&report).skills);
    assert_eq!(s.w.files_of("alpha").len(), 2);
}

#[test]
fn a_file_the_user_added_that_github_now_has_too_is_kept_aside_and_the_user_is_told() {
    let mut s = Setup::installed("clash-told");
    s.w.write("alpha/notes.md", "my notes\n");
    s.w.write("alpha/mine.md", "mine\n");
    s.remote.put(REPO, "skills/alpha/notes.md", "github's notes\n");

    let report = s.sync();

    let repo = only(&report);
    assert_eq!((&skill(repo, "alpha").verdict, &skill(repo, "alpha").result), (&Verdict::Update, &ApplyResult::Done));
    assert_eq!(s.w.read("alpha/notes.md"), "github's notes\n");
    assert_eq!(s.w.read("alpha/mine.md"), "mine\n");
    let backups = s.w.backups();
    assert_eq!(backups.len(), 1, "{:?}", s.w.work_leftovers());
    assert_eq!(fs::read_to_string(backups[0].join("notes.md")).unwrap(), "my notes\n");
    assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
    let told = &report.warnings[0];
    assert!(told.starts_with("alpha: ") && told.contains("notes.md") && told.contains(&backups[0].display().to_string()), "{told}");
    assert!(!told.contains("mine.md"), "{told}");

    // Said once: the next check has nothing to tell.
    let report = sync_with(&mut s.engine, &s.cfg, SyncOptions { ignore_etag: true, ..SyncOptions::default() });
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
}

#[test]
fn a_skill_gone_both_upstream_and_locally_is_just_forgotten() {
    let mut s = Setup::installed("forget");
    s.remote.remove(REPO, "skills/gamma");
    s.w.remove("gamma");

    let report = s.sync();
    let repo = only(&report);
    assert_eq!(skill(repo, "gamma").verdict, Verdict::Forget);
    assert_eq!(skill(repo, "gamma").result, ApplyResult::Done);
    assert!(s.w.state().repo("fx").unwrap().skill("gamma").is_none());
}

#[test]
fn an_identical_folder_that_already_exists_is_adopted_without_downloading_or_writing() {
    let mut s = Setup::fresh("adopt");
    for (path, body) in STANDARD.iter().filter(|(p, _)| p.starts_with("skills/") && !p.contains("not-a-skill")) {
        s.w.write(&path["skills/".len()..], body);
    }
    let before = s.w.skills_snapshot();

    let report = s.sync();
    let repo = only(&report);

    assert!(matches!(repo.outcome, Outcome::Synced), "{:?}", repo.outcome);
    for name in ["alpha", "beta", "gamma"] {
        assert_eq!(skill(repo, name).verdict, Verdict::Adopt, "{name}");
        assert_eq!(skill(repo, name).result, ApplyResult::Done, "{name}");
    }
    assert_eq!(s.remote.calls().file, 0, "nothing was downloaded");
    assert_eq!(s.w.skills_snapshot(), before, "not a byte of the skills folder changed");
    assert_eq!(s.w.state().repo("fx").unwrap().skills.len(), 3);
}

#[test]
fn a_different_folder_with_the_same_name_is_left_alone_and_reported() {
    let mut s = Setup::fresh("differs");
    s.w.write("alpha/SKILL.md", "my own alpha\n");

    let report = s.sync();
    let repo = only(&report);

    assert!(matches!(repo.outcome, Outcome::NeedsAttention), "{:?}", repo.outcome);
    assert_eq!(skill(repo, "alpha").verdict, Verdict::Held(Hold::UnmanagedDiffers));
    assert_eq!(s.w.read("alpha/SKILL.md"), "my own alpha\n");
    assert!(!s.w.exists("alpha/references"), "nothing was added to it either");
    assert_eq!(skill(repo, "beta").result, ApplyResult::Done, "the other skills are installed as usual");
    assert!(s.w.state().repo("fx").unwrap().skill("alpha").is_none());
}

/// Writes a skill of `STANDARD` as a Windows editor might save it: other line breaks and a blank line more at the end.
fn write_windows_copy(w: &World, name: &str) {
    for (path, body) in STANDARD.iter().filter(|(p, _)| p.starts_with(&format!("skills/{name}/"))) {
        w.write(&path["skills/".len()..], &format!("{}\r\n", body.replace('\n', "\r\n")));
    }
}

#[test]
fn a_folder_that_differs_only_in_line_breaks_is_adopted_after_reading_just_its_text_from_github() {
    let mut s = Setup::fresh("adopt-text");
    write_windows_copy(&s.w, "alpha");
    for name in ["beta", "gamma"] {
        let (path, body) = STANDARD.iter().find(|(p, _)| *p == format!("skills/{name}/SKILL.md")).unwrap();
        s.w.write(&path["skills/".len()..], body);
    }
    let before = s.w.skills_snapshot();

    let report = s.sync();
    let repo = only(&report);
    assert!(matches!(repo.outcome, Outcome::Synced), "{:?}", repo.outcome);
    for name in ["alpha", "beta", "gamma"] {
        assert_eq!(skill(repo, name).verdict, Verdict::Adopt, "{name}");
        assert_eq!(skill(repo, name).result, ApplyResult::Done, "{name}");
    }
    assert_eq!(s.remote.calls().file, 3, "only alpha's three files, whose bytes differ, were read");
    let alpha_bytes: usize = STANDARD.iter().filter(|(p, _)| p.starts_with("skills/alpha/")).map(|(_, body)| body.len()).sum();
    assert_eq!(repo.downloaded, alpha_bytes as u64, "what was read to compare counts as downloaded");
    assert_eq!(s.w.skills_snapshot(), before, "not a byte of the skills folder changed");

    s.remote.put(REPO, "skills/alpha/SKILL.md", ALPHA_V2);
    let report = s.sync();
    assert_eq!(skill(only(&report), "alpha").verdict, Verdict::Update);
    assert_eq!(skill(only(&report), "alpha").result, ApplyResult::Done);
    assert_eq!(s.w.read("alpha/SKILL.md"), ALPHA_V2);
    assert!(s.w.backups().is_empty(), "the copy it replaced was the same, so there was nothing to keep");
}

#[test]
fn a_text_folder_that_really_differs_is_held_and_failing_to_read_github_decides_nothing() {
    let mut s = Setup::fresh("differs-text");
    write_windows_copy(&s.w, "alpha");
    s.w.write("alpha/references/guide.md", "guide, my way\r\n");
    s.remote.fail_file(REPO, "skills/alpha/SKILL.md", Some(RemoteError::new(FailureKind::Network, "the network is down")));
    let before = s.w.skills_snapshot();

    let report = s.sync();
    let repo = only(&report);
    assert!(matches!(&repo.outcome, Outcome::Failed(f) if f.kind == FailureKind::Network), "{:?}", repo.outcome);
    assert!(repo.skills.is_empty(), "no decision was made");
    assert_eq!(s.w.skills_snapshot(), before, "nothing was installed either");

    s.remote.fail_file(REPO, "skills/alpha/SKILL.md", None);
    let report = s.sync();
    let repo = only(&report);
    assert_eq!(skill(repo, "alpha").verdict, Verdict::Held(Hold::UnmanagedDiffers));
    assert_eq!(s.w.read("alpha/references/guide.md"), "guide, my way\r\n");
    assert_eq!(skill(repo, "beta").result, ApplyResult::Done);
    assert!(s.w.state().repo("fx").unwrap().skill("alpha").is_none());
}

#[test]
fn reading_files_from_github_to_compare_them_counts_toward_the_download_limit() {
    let mut s = Setup::fresh("adopt-text-limit");
    write_windows_copy(&s.w, "alpha");
    let report = sync_with(&mut s.engine, &s.cfg, SyncOptions { max_download_bytes: 10, ..SyncOptions::default() });
    assert!(matches!(only(&report).outcome, Outcome::Failed(_)), "a comparison limit is not a no-match result");
    assert_eq!(s.remote.calls().file, 0);

    let report = s.sync();
    assert_eq!(skill(only(&report), "alpha").verdict, Verdict::Adopt, "the next check reads them");
    assert_eq!(skill(only(&report), "alpha").result, ApplyResult::Done);
}

#[test]
fn a_managed_skill_saved_with_other_line_breaks_is_not_an_edit_and_follows_upstream() {
    let mut s = Setup::installed("owned-text");
    write_windows_copy(&s.w, "alpha");
    let report = sync_with(&mut s.engine, &s.cfg, SyncOptions { ignore_etag: true, ..SyncOptions::default() });
    assert_eq!(skill(only(&report), "alpha").verdict, Verdict::InSync);

    s.remote.remove(REPO, "skills/alpha");
    let report = s.sync();
    assert_eq!(skill(only(&report), "alpha").verdict, Verdict::Delete);
    assert_eq!(skill(only(&report), "alpha").result, ApplyResult::Done);
    assert!(!s.w.exists("alpha"));
    assert!(s.w.backups().is_empty(), "it was the installed version, so there was nothing to keep");
}

#[test]
fn a_folder_the_user_made_is_never_removed_even_when_it_is_not_in_any_repository() {
    let mut s = Setup::installed("unmanaged");
    s.w.write("my-own-skill/SKILL.md", "mine\n");
    s.remote.remove(REPO, "skills/alpha");
    s.remote.remove(REPO, "skills/beta");
    s.remote.remove(REPO, "skills/gamma");

    s.sync();
    s.sync();
    assert!(s.w.exists("my-own-skill/SKILL.md"));
}

#[test]
fn two_repositories_cannot_provide_the_same_skill_name() {
    let w = World::new("collision");
    let remote = FakeRemote::new();
    remote.put("o/one", "skills/alpha/SKILL.md", "one\n");
    remote.put("o/two", "skills/alpha/SKILL.md", "two\n");
    remote.put("o/two", "skills/delta/SKILL.md", "delta\n");
    let cfg = config(vec![repo("one", "o/one", "skills"), repo("two", "o/two", "skills")]);
    let mut engine = w.engine(&remote);

    let report = sync_all(&mut engine, &cfg);

    assert_eq!(report.repos.len(), 2);
    assert_eq!(w.read("alpha/SKILL.md"), "one\n", "the first repository in the settings wins");
    assert!(matches!(skill(&report.repos[1], "alpha").verdict, Verdict::Skipped(Skip::Collision { .. })));
    assert_eq!(skill(&report.repos[1], "delta").result, ApplyResult::Done);

    // The skipped copy never turns into a deletion, however many checks pass.
    remote.put("o/two", "skills/alpha/SKILL.md", "two v2\n");
    sync_all(&mut engine, &cfg);
    assert_eq!(w.read("alpha/SKILL.md"), "one\n");
}

#[test]
fn a_listing_with_no_skills_does_not_wipe_the_installed_ones_until_it_is_confirmed() {
    let mut s = Setup::installed("mass");
    for name in ["alpha", "beta", "gamma"] {
        s.remote.remove(REPO, &format!("skills/{name}"));
    }
    // skills/not-a-skill is still there, so GitHub answers with a listing that has no skills, not with "not found".

    let report = s.sync();
    let repo = only(&report);
    assert!(matches!(repo.outcome, Outcome::NeedsAttention), "{:?}", repo.outcome);
    for name in ["alpha", "beta", "gamma"] {
        assert_eq!(skill(repo, name).verdict, Verdict::Held(Hold::MassDeletion), "{name}");
    }
    assert_eq!(s.w.skill_names(), ["alpha", "beta", "gamma"]);

    let report = sync_with(&mut s.engine, &s.cfg, SyncOptions { confirm_mass_delete: true, ..SyncOptions::default() });
    for name in ["alpha", "beta", "gamma"] {
        assert_eq!(skill(only(&report), name).result, ApplyResult::Done, "{name}");
    }
    assert!(s.w.skill_names().is_empty());
}

#[test]
fn a_folder_that_is_itself_one_skill_is_named_after_its_last_segment() {
    let w = World::new("single");
    let remote = FakeRemote::new();
    remote.seed("o/tools", &[("tools/helper/SKILL.md", "helper\n"), ("tools/helper/refs/a.md", "a\n"), ("tools/README.md", "readme\n")]);
    let cfg = config(vec![repo("t", "o/tools", "tools/helper")]);
    let mut engine = w.engine(&remote);

    let report = sync_all(&mut engine, &cfg);

    assert!(matches!(only(&report).outcome, Outcome::Synced), "{report:?}");
    assert_eq!(w.skill_names(), ["helper"]);
    assert_eq!(w.read("helper/refs/a.md"), "a\n");
    assert!(remote.log().contains(&"file o/tools tools/helper/refs/a.md".to_string()), "{:?}", remote.log());
}

#[test]
fn a_whole_repository_that_is_one_skill_is_named_after_the_repository() {
    let w = World::new("root-skill");
    let remote = FakeRemote::new();
    remote.seed("o/just-one", &[("SKILL.md", "root skill\n"), ("scripts/run.ps1", "run\n")]);
    let cfg = config(vec![repo("r", "o/just-one", "")]);
    let mut engine = w.engine(&remote);

    sync_all(&mut engine, &cfg);

    assert_eq!(w.skill_names(), ["just-one"]);
    assert_eq!(w.read("just-one/scripts/run.ps1"), "run\n");
    assert!(remote.log().contains(&"file o/just-one scripts/run.ps1".to_string()), "{:?}", remote.log());
}

#[test]
fn a_skill_that_is_not_safe_to_install_is_rejected_whole_and_the_rest_still_installs() {
    let w = World::new("rejected");
    let remote = FakeRemote::new();
    remote.seed("o/odd", &[("skills/ok/SKILL.md", "ok\n"), ("skills/bad/SKILL.md", "bad\n"), ("skills/bad/CON.txt", "reserved device name\n")]);
    let cfg = config(vec![repo("odd", "o/odd", "skills")]);
    let mut engine = w.engine(&remote);

    let report = sync_all(&mut engine, &cfg);
    let repo = only(&report);

    assert_eq!(w.skill_names(), ["ok"], "the bad skill is not half-installed");
    assert_eq!(repo.rejected.len(), 1, "{:?}", repo.rejected);
    assert_eq!(repo.rejected[0].name, "bad");
    assert_eq!(skill(repo, "ok").result, ApplyResult::Done);
}

// ---------------------------------------------------------------------------------------------------------------------
// Show changes
// ---------------------------------------------------------------------------------------------------------------------

fn diff(s: &mut Setup, skill: &str) -> DiffReport {
    s.engine.diff(&s.cfg, "fx", skill, &cancel_flag()).expect("the changes could be shown")
}

fn shown(report: &DiffReport) -> Vec<(&str, Presence)> {
    report.files.iter().map(|f| (f.path.as_str(), f.presence)).collect()
}

fn lines(change: &FileChange) -> (usize, usize) {
    match &change.body {
        Body::Text(text) => (text.removed, text.added),
        other => panic!("{} is not shown as text: {other:?}", change.path),
    }
}

#[test]
fn show_changes_downloads_only_the_files_that_differ_and_changes_nothing() {
    let mut s = Setup::installed("diff");
    s.w.write("alpha/SKILL.md", &ALPHA_V1.replace("v1", "mine"));
    s.w.write("alpha/notes.md", "my notes\n");
    s.remote.put(REPO, "skills/alpha/new.md", "new\n");
    s.remote.remove(REPO, "skills/alpha/scripts/hello.ps1");
    let before = s.w.snapshot();

    let report = diff(&mut s, "alpha");

    assert!(report.on_github && report.here);
    assert_eq!(
        shown(&report),
        [("new.md", Presence::OnlyOnGitHub), ("scripts/hello.ps1", Presence::OnlyHere), ("SKILL.md", Presence::Both)],
        "a file the user added is theirs, so it is not shown"
    );
    let changed = ALPHA_V1.lines().filter(|l| l.contains("v1")).count();
    assert_eq!(lines(&report.files[2]), (changed, changed));
    assert_eq!(lines(&report.files[0]), (0, 1), "a file only on GitHub would come");
    assert_eq!(lines(&report.files[1]), (1, 0), "a file GitHub removed would go");
    // The guide is the same on both sides, so it is not downloaded.
    assert_eq!(s.remote.calls(), Calls { tree: 1, conditional: 0, commit: 1, file: 2, ..Calls::default() });
    assert!(!s.remote.log().iter().any(|l| l.contains("guide.md")), "{:?}", s.remote.log());
    assert_eq!(s.w.snapshot(), before, "showing changes writes nothing, not even state.json");
}

#[test]
fn show_changes_of_a_folder_nobody_owns_shows_only_what_github_has() {
    let mut s = Setup::fresh("diff-unowned");
    s.w.write("alpha/SKILL.md", "my own alpha\n");
    s.w.write("alpha/mine.md", "mine\n");
    s.sync();

    let report = diff(&mut s, "alpha");

    assert!(report.on_github && report.here);
    assert_eq!(shown(&report), [("references/guide.md", Presence::OnlyOnGitHub), ("scripts/hello.ps1", Presence::OnlyOnGitHub), ("SKILL.md", Presence::Both)]);
}

#[test]
fn show_changes_of_a_skill_deleted_here_or_gone_from_github() {
    let mut s = Setup::installed("diff-gone");
    fs::remove_dir_all(s.w.skills.join("beta")).unwrap();
    let report = diff(&mut s, "Beta");
    assert!(report.on_github && !report.here, "the name is matched whatever its case");
    assert_eq!(shown(&report), [("SKILL.md", Presence::OnlyOnGitHub)]);

    s.remote.remove(REPO, "skills/gamma");
    s.w.write("gamma/mine.md", "mine\n");
    s.remote.reset();
    let report = diff(&mut s, "Gamma");
    assert!(!report.on_github && report.here);
    assert_eq!(shown(&report), [("SKILL.md", Presence::OnlyHere)], "the file the user added would stay, so it is not shown");
    assert_eq!(s.remote.calls(), Calls { tree: 1, conditional: 0, commit: 0, file: 0, ..Calls::default() }, "nothing to download");

    s.remote.reset();
    assert!(diff(&mut s, "alpha").files.is_empty(), "a skill that is the same on both sides has nothing to show");
    assert_eq!(s.remote.calls(), Calls { tree: 1, conditional: 0, commit: 0, file: 0, ..Calls::default() });
}

#[test]
fn show_changes_of_a_private_repository_sends_the_token_without_remembering_that_it_is_private() {
    let mut s = Setup::installed("diff-private");
    s.remote.make_private(REPO, "secret");
    s.tokens = FakeTokens::some("secret");
    s.engine = s.restarted();
    s.w.write("beta/SKILL.md", "mine\n");
    let state = fs::read(s.w.state_file()).unwrap();

    let report = diff(&mut s, "beta");

    assert_eq!(shown(&report), [("SKILL.md", Presence::Both)]);
    let seen = s.remote.tokens_seen();
    let seen: Vec<(&str, bool)> = seen.iter().map(|(kind, token)| (kind.as_str(), token.is_some())).collect();
    assert_eq!(seen, [("tree", false), ("tree", true), ("commit", true), ("file", true)]);
    assert!(!s.engine.needs_token("fx"), "only a check learns that a repository is private");
    assert_eq!(fs::read(s.w.state_file()).unwrap(), state);
}

#[test]
fn show_changes_fails_as_a_whole_and_changes_nothing_when_github_fails() {
    let mut s = Setup::installed("diff-fail");
    s.w.write("alpha/SKILL.md", "mine\n");
    let before = s.w.snapshot();

    s.remote.fail_file(REPO, "skills/alpha/SKILL.md", Some(RemoteError::new(FailureKind::Network, "the connection dropped")));
    let err = s.engine.diff(&s.cfg, "fx", "alpha", &cancel_flag()).unwrap_err();
    assert!(err.to_string().contains("the connection dropped"), "{err}");

    s.remote.fail_file(REPO, "skills/alpha/SKILL.md", None);
    s.remote.lie_about(REPO, "skills/alpha/SKILL.md", "pushed meanwhile\n");
    let err = s.engine.diff(&s.cfg, "fx", "alpha", &cancel_flag()).unwrap_err();
    assert!(err.to_string().contains("changed on GitHub"), "a download that does not match the listing is not shown: {err}");
    s.remote.lie_about(REPO, "skills/alpha/SKILL.md", &ALPHA_V1.repeat(2));
    let err = s.engine.diff(&s.cfg, "fx", "alpha", &cancel_flag()).unwrap_err();
    assert!(err.to_string().contains("changed on GitHub"), "nor one longer than listed: {err}");
    // What GitHub's servers say when the listing is newer than the commit the files are read at.
    s.remote.lie_about(REPO, "skills/alpha/SKILL.md", ALPHA_V1);
    s.remote.fail_file(REPO, "skills/alpha/SKILL.md", Some(RemoteError::new(FailureKind::NotFound, "GitHub could not find it: it may be private")));
    let err = s.engine.diff(&s.cfg, "fx", "alpha", &cancel_flag()).unwrap_err();
    assert!(
        err.to_string().contains("changed on GitHub while SKILL.md was being read") && !err.to_string().contains("private"),
        "a file the listing names is there: {err}"
    );
    s.remote.fail_file(REPO, "skills/alpha/SKILL.md", None);

    s.remote.make_private(REPO, "secret");
    let err = s.engine.diff(&s.cfg, "fx", "alpha", &cancel_flag()).unwrap_err();
    assert!(err.to_string().contains("gh auth login"), "{err}");

    assert!(s.engine.diff(&s.cfg, "nope", "alpha", &cancel_flag()).is_err());
    let err = s.engine.diff(&s.cfg, "fx", "..", &cancel_flag()).unwrap_err();
    assert!(err.to_string().contains("not a skill name"), "only a skill name is accepted: {err}");
    assert_eq!(s.w.snapshot(), before);
}

#[test]
fn show_changes_names_what_it_does_not_read_and_never_follows_a_link() {
    let mut s = Setup::fresh("diff-big");
    let big = vec![b'x'; DIFF_FILE_BYTES as usize + 1];
    s.remote.put_bytes(REPO, "skills/alpha/same.bin", &big);
    s.remote.put_bytes(REPO, "skills/alpha/changed.bin", &big);
    s.sync();
    s.remote.remove(REPO, "skills/alpha/references/guide.md");
    let alpha = s.w.skills.join("alpha");
    let mut mine = big.clone();
    mine[0] = b'y';
    fs::write(alpha.join("changed.bin"), &mine).unwrap();
    let outside = s.w.root.join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("SKILL.md"), "not in the skill\n").unwrap();
    let junction = |link: &std::path::Path| {
        let made = std::process::Command::new("cmd").args(["/C", "mklink", "/J"]).arg(link).arg(&outside).output().unwrap().status.success();
        assert!(made, "creating a junction needs no privileges");
    };
    // A file the app installed, now a link; and a link the user added, which is theirs and not shown.
    let guide = alpha.join("references").join("guide.md");
    fs::remove_file(&guide).unwrap();
    junction(&guide);
    junction(&alpha.join("linked"));
    // A folder on the way to a file, now a link to a folder where that file is the same: it is not looked into.
    fs::remove_dir_all(alpha.join("scripts")).unwrap();
    fs::write(outside.join("hello.ps1"), "Write-Output 'hello'\n").unwrap();
    junction(&alpha.join("scripts"));
    s.remote.reset();

    let report = diff(&mut s, "alpha");

    let bodies: Vec<(&str, &Body)> = report.files.iter().map(|f| (f.path.as_str(), &f.body)).collect();
    assert_eq!(bodies.len(), 3, "the big file that is the same is left out: {bodies:?}");
    assert_eq!(bodies[0].0, "changed.bin");
    assert!(matches!(bodies[0].1, Body::Hidden(why) if why.contains("too big")), "{bodies:?}");
    assert_eq!(bodies[1].0, "references/guide.md");
    assert!(matches!(bodies[1].1, Body::Hidden(why) if why.contains("never followed")), "{bodies:?}");
    assert_eq!(bodies[2].0, "scripts/hello.ps1");
    assert!(matches!(bodies[2].1, Body::Hidden(why) if why.contains("never followed")), "{bodies:?}");
    assert_eq!(s.remote.calls().file, 0, "a file too big to show is not downloaded");
    fs::remove_dir(&guide).unwrap();
    fs::remove_dir(alpha.join("linked")).unwrap();
    fs::remove_dir(alpha.join("scripts")).unwrap();

    fs::remove_dir_all(&alpha).unwrap();
    junction(&alpha);
    let err = s.engine.diff(&s.cfg, "fx", "alpha", &cancel_flag()).unwrap_err();
    assert!(err.to_string().contains("link"), "{err}");
    fs::remove_dir(&alpha).unwrap();
}

#[test]
fn show_changes_of_a_folder_that_is_itself_one_skill() {
    let w = World::new("diff-single");
    let remote = FakeRemote::new();
    remote.seed("o/tools", &[("tools/helper/SKILL.md", "helper\n"), ("tools/helper/refs/a.md", "a\n")]);
    let cfg = config(vec![repo("t", "o/tools", "tools/helper")]);
    let mut engine = w.engine(&remote);
    sync_all(&mut engine, &cfg);
    w.write("helper/refs/a.md", "mine\n");

    let report = engine.diff(&cfg, "t", "helper", &cancel_flag()).unwrap();

    assert_eq!(shown(&report), [("refs/a.md", Presence::Both)]);
    assert!(remote.log().contains(&"file o/tools tools/helper/refs/a.md".to_string()), "{:?}", remote.log());
}

#[test]
fn show_changes_reads_at_most_a_few_megabytes_from_each_side_not_counting_files_that_are_the_same() {
    let mut s = Setup::fresh("diff-budget");
    let mib = vec![0u8; DIFF_FILE_BYTES as usize];
    for i in 1..=5 {
        s.remote.put_bytes(REPO, &format!("skills/alpha/a-same-{i}.bin"), &mib);
        s.remote.put_bytes(REPO, &format!("skills/alpha/up-{i}.bin"), &mib);
        s.remote.put_bytes(REPO, &format!("skills/alpha/here-{i}.bin"), &mib);
    }
    s.sync();
    let alpha = s.w.skills.join("alpha");
    for i in 1..=5 {
        fs::remove_file(alpha.join(format!("up-{i}.bin"))).unwrap();
        s.remote.remove(REPO, &format!("skills/alpha/here-{i}.bin"));
    }
    s.remote.reset();

    let report = diff(&mut s, "alpha");

    let hidden: Vec<&str> =
        report.files.iter().filter(|f| matches!(&f.body, Body::Hidden(why) if why.contains("as much as can be shown"))).map(|f| f.path.as_str()).collect();
    assert_eq!(report.files.len(), 10, "{:?}", shown(&report));
    assert_eq!(hidden.len(), 2, "four files from each side are shown and the fifth is only named: {hidden:?}");
    assert!(hidden.iter().any(|p| p.starts_with("up-")) && hidden.iter().any(|p| p.starts_with("here-")), "{hidden:?}");
    assert_eq!(s.remote.calls().file, 4);
}
#[test]
fn historical_whole_copy_is_updated_without_backup_and_extras_are_carried() {
    let w = World::new("historical-old");
    let remote = FakeRemote::new();
    remote.seed(REPO, &[("skills/alpha/SKILL.md", "old\n"), ("skills/alpha/refs/a.md", "guide\n")]);
    w.write("alpha/SKILL.md", "\r\nold\r\n\r\n");
    w.write("alpha/refs/a.md", "guide");
    w.write("alpha/mine.txt", "mine");
    remote.seed(REPO, &[("skills/alpha/SKILL.md", "new\n"), ("skills/alpha/new.txt", "new file")]);
    let report = sync_all(&mut w.engine(&remote), &standard_config());
    assert_eq!(w.read("alpha/SKILL.md"), "new\n", "{report:?}");
    assert_eq!(w.read("alpha/mine.txt"), "mine");
    assert_eq!(w.read("alpha/new.txt"), "new file");
    assert!(w.backups().is_empty());
    assert_eq!(skill(only(&report), "alpha").result, ApplyResult::Done);
}

#[test]
fn historical_search_yields_and_resumes_after_restart_beyond_one_page() {
    let w = World::new("historical-paging");
    let remote = FakeRemote::new();
    remote.put(REPO, "skills/alpha/SKILL.md", "old");
    w.write("alpha/SKILL.md", "old");
    for n in 0..105 {
        remote.put(REPO, "skills/alpha/SKILL.md", &format!("version {n}"));
    }
    remote.put(REPO, "skills/beta/SKILL.md", "another skill");
    let (mut engine, clock) = w.engine_at(&remote, &FakeTokens::none());
    let first = sync_all(&mut engine, &standard_config());
    assert_eq!(skill(only(&first), "alpha").verdict, Verdict::Held(Hold::HistorySearching));
    assert!(w.exists("beta/SKILL.md"), "only the searching skill waits");
    assert_eq!(engine.next_wake(&standard_config()), Some(T0 + 30), "search must continue before the ordinary interval");
    drop(engine);
    let mut engine = w.engine(&remote);
    for _ in 0..20 {
        sync_all(&mut engine, &standard_config());
        if w.read("alpha/SKILL.md") == "version 104" {
            break;
        }
    }
    assert_eq!(w.read("alpha/SKILL.md"), "version 104");
    assert!(remote.log().iter().any(|l| l.contains("page 2")), "all history pages must be searched");
    assert!(w.backups().is_empty());
    drop(clock);
}

#[test]
fn historical_completed_mismatch_is_quiet_but_nested_repair_invalidates_it() {
    let w = World::new("historical-cache");
    let remote = FakeRemote::new();
    remote.seed(REPO, &[("skills/alpha/SKILL.md", "old"), ("skills/alpha/refs/a", "old guide")]);
    remote.seed(REPO, &[("skills/alpha/SKILL.md", "new"), ("skills/alpha/refs/a", "new guide")]);
    w.write("alpha/SKILL.md", "old");
    w.write("alpha/refs/a", "my guide");
    let mut engine = w.engine(&remote);
    let cfg = standard_config();
    let report = sync_all(&mut engine, &cfg);
    assert_eq!(skill(only(&report), "alpha").verdict, Verdict::Held(Hold::UnmanagedDiffers));
    let before = tiny_skill_sync::engine::stamp_of(&w.state_file());
    remote.reset();
    sync_all(&mut engine, &cfg);
    assert_eq!(remote.calls(), Calls { commit: 1, ..Calls::default() });
    assert_eq!(tiny_skill_sync::engine::stamp_of(&w.state_file()), before);
    w.write("alpha/refs/a", "old guide");
    sync_all(&mut engine, &cfg);
    assert_eq!(w.read("alpha/SKILL.md"), "new");
}

#[test]
fn historical_subset_preserves_extras_and_backs_up_only_collisions() {
    let w = World::new("historical-subset");
    let remote = FakeRemote::new();
    remote.put(REPO, "skills/alpha/SKILL.md", "old");
    w.write("alpha/SKILL.md", "old");
    w.write("alpha/new.txt", "my collision");
    w.write("alpha/notes.txt", "mine");
    remote.seed(REPO, &[("skills/alpha/SKILL.md", "new"), ("skills/alpha/new.txt", "upstream")]);
    let report = sync_all(&mut w.engine(&remote), &standard_config());
    assert_eq!(skill(only(&report), "alpha").verdict, Verdict::AdoptHistorical);
    assert_eq!(w.read("alpha/SKILL.md"), "new");
    assert_eq!(w.read("alpha/notes.txt"), "mine");
    assert_eq!(w.read("alpha/new.txt"), "upstream");
    assert_eq!(w.backups().len(), 1);
    assert_eq!(std::fs::read_to_string(w.backups()[0].join("new.txt")).unwrap(), "my collision");
}

#[test]
fn historical_mixed_versions_never_supply_a_whole_version_match() {
    let w = World::new("historical-mixed");
    let remote = FakeRemote::new();
    remote.seed(REPO, &[("skills/alpha/SKILL.md", "old"), ("skills/alpha/a", "old guide")]);
    remote.seed(REPO, &[("skills/alpha/SKILL.md", "new"), ("skills/alpha/a", "new guide")]);
    w.write("alpha/SKILL.md", "old");
    w.write("alpha/a", "new guide");
    let before = w.skills_snapshot();
    let report = sync_all(&mut w.engine(&remote), &standard_config());
    assert_eq!(skill(only(&report), "alpha").verdict, Verdict::Held(Hold::UnmanagedDiffers));
    assert_eq!(w.skills_snapshot(), before);
    assert!(w.state().repo("fx").unwrap().skills.is_empty());
}

#[test]
fn historical_deleted_and_recreated_path_and_single_skill_source_are_searched() {
    for single in [false, true] {
        let w = World::new("historical-recreated");
        let remote = FakeRemote::new();
        remote.seed(REPO, &[("README.md", "readme"), ("skills/alpha/SKILL.md", "old")]);
        remote.remove(REPO, "skills/alpha");
        remote.put(REPO, "skills/alpha/SKILL.md", "new");
        w.write("alpha/SKILL.md", "old");
        let cfg = config(vec![repo("fx", REPO, if single { "skills/alpha" } else { "skills" })]);
        let report = sync_all(&mut w.engine(&remote), &cfg);
        assert_eq!(skill(only(&report), "alpha").verdict, Verdict::AdoptHistorical);
        assert_eq!(w.read("alpha/SKILL.md"), "new");
    }
}

#[test]
fn historical_new_history_invalidates_no_match_even_when_the_current_tree_returns_to_the_same_bytes() {
    let w = World::new("historical-revert");
    let remote = FakeRemote::new();
    remote.put(REPO, "skills/alpha/SKILL.md", "current");
    w.write("alpha/SKILL.md", "future historical copy");
    let cfg = standard_config();
    let mut engine = w.engine(&remote);
    sync_all(&mut engine, &cfg);
    remote.put(REPO, "skills/alpha/SKILL.md", "future historical copy");
    remote.put(REPO, "skills/alpha/SKILL.md", "current");
    let report = sync_all(&mut engine, &cfg);
    assert_eq!(w.read("alpha/SKILL.md"), "current", "{report:?}");
    assert_eq!(skill(only(&report), "alpha").verdict, Verdict::AdoptHistorical);
}
