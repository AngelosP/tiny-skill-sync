//! The engine against a fake GitHub: what happens to the skills folder, check by check.

mod common;

use std::fs;

use common::*;
use tiny_skill_sync::engine::{ApplyResult, Outcome, Resolution, SyncOptions, SyncReport};
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
    assert_eq!(s.remote.calls(), Calls { tree: 1, conditional: 0, commit: 1, file: 5 });
}

#[test]
fn a_check_that_finds_nothing_new_is_one_request_and_changes_no_file() {
    let mut s = Setup::installed("unchanged");
    s.advance(3600);
    let before = s.w.snapshot();

    let report = s.sync();
    let repo = only(&report);

    assert!(matches!(repo.outcome, Outcome::Unchanged), "{:?}", repo.outcome);
    assert_eq!(s.remote.calls(), Calls { tree: 1, conditional: 1, commit: 0, file: 0 });
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
    assert!(!s.w.exists("alpha/mine.txt"), "the folder is exactly upstream's again");
    let backups = s.w.backups();
    assert_eq!(backups.len(), 1, "{backups:?}");
    assert_eq!(report.backups, backups);
    assert_eq!(fs::read_to_string(backups[0].join("SKILL.md")).unwrap(), "my own words\n");
    assert_eq!(fs::read_to_string(backups[0].join("mine.txt")).unwrap(), "only mine\n");

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
        only(&report).skills.is_empty() && s.remote.calls() == Calls { tree: 1, conditional: 1, commit: 0, file: 0 }
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
