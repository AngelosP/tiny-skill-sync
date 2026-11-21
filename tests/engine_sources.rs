//! A mixed repository, one settings home and three independent destinations.
mod common;

use common::*;
use std::fs;
use std::os::windows::fs::OpenOptionsExt;
use std::sync::atomic::Ordering;
use tiny_skill_sync::apply::Step;
use tiny_skill_sync::config::{Config, RepoSpec};
use tiny_skill_sync::engine::{Access, ApplyResult, Outcome, Resolution, SyncOptions, Which};
use tiny_skill_sync::github::{FailureKind, RemoteError};
use tiny_skill_sync::reconcile::{Hold, Verdict};
use tiny_skill_sync::source::SourceKind;
use tiny_skill_sync::sources::{SourceConfig, allocate_source_ids};

const AGENT: &str = "review.agent.md";
const RULE: &str = "rust.instructions.md";

fn mixed() -> (World, std::sync::Arc<FakeRemote>, Config) {
    let w = World::new("mixed");
    let remote = FakeRemote::new();
    remote.seed(REPO, &[("pack/demo/SKILL.md", "skill"), ("pack/review.agent.md", "one\n"), ("pack/rust.instructions.md", "rule"), ("pack/README", "other")]);
    let mut cfg = Config::default();
    let spec = RepoSpec::parse("https://github.com/o/fixtures/tree/main/pack").unwrap();
    for kind in [SourceKind::Skills, SourceKind::Agents, SourceKind::Instructions] {
        cfg.add_source(kind, &spec, None, false);
    }
    (w, remote, cfg)
}

#[test]
fn a_full_check_confirms_the_shared_listing_once_for_both_file_lanes() {
    let w = World::new("confirm-once");
    let remote = FakeRemote::new();
    remote.seed(REPO, &[("pack/review.agent.md", "one"), ("pack/rust.instructions.md", "rule")]);
    let mut cfg = Config::default();
    let (id, lanes) = allocate_source_ids("pack", &mut cfg.taken_ids());
    let mut source = SourceConfig { id, repo: REPO.into(), branch: "main".into(), path: "pack".into(), lanes, ..SourceConfig::default() };
    for kind in [SourceKind::Skills, SourceKind::Plugins] {
        source.types.set(kind, false);
    }
    cfg.add_source_config(source).unwrap();
    let mut engine = w.engine(&remote);
    let report = sync_all(&mut engine, &cfg);
    assert!(report.repos.iter().all(|r| matches!(r.outcome, Outcome::Synced)), "{report:?}");

    remote.put(REPO, "pack/review.agent.md", "two");
    remote.put(REPO, "pack/rust.instructions.md", "rule two");
    remote.reset();
    let report = sync_all(&mut engine, &cfg);
    assert_eq!(report.repos.len(), 2, "{report:?}");
    assert!(report.repos.iter().all(|r| matches!(r.outcome, Outcome::Synced)), "{report:?}");
    let pinned: Vec<String> = remote.log().into_iter().filter(|c| c.starts_with("historical_tree ")).collect();
    assert_eq!(pinned.len(), 1, "one pinned root tree confirms the listing for both lanes: {:?}", remote.log());
    assert_eq!(fs::read_to_string(w.root.join("instructions").join(RULE)).unwrap(), "rule two");
}

#[test]
fn mixed_sources_adopt_install_hold_and_poll_independently() {
    let (w, remote, cfg) = mixed();
    let agents = w.root.join("agents").join("fixtures");
    let rules = w.root.join("instructions");
    fs::create_dir_all(&agents).unwrap();
    fs::write(agents.join(AGENT), "one\r\n").unwrap();
    let neighbor = agents.join("personal.agent.md");
    fs::write(&neighbor, "private").unwrap();
    let lock = fs::OpenOptions::new().read(true).share_mode(0).open(&neighbor).unwrap();
    let (mut engine, clock) = w.engine_at(&remote, &FakeTokens::none());
    let id = &cfg.repos()[1].id;
    let report = sync_which(&mut engine, &cfg, &Which::Due);
    assert_eq!(report.repos.len(), 3);
    assert_eq!(skill(&report.repos[1], AGENT).verdict, Verdict::Adopt);
    assert_eq!(w.read("demo/SKILL.md"), "skill");
    assert_eq!(fs::read_to_string(agents.join(AGENT)).unwrap(), "one\r\n");
    assert_eq!(fs::read_to_string(rules.join(RULE)).unwrap(), "rule");
    assert_eq!(remote.calls().tree, 3);
    assert!(sync_which(&mut engine, &cfg, &Which::Due).repos.is_empty());
    clock.fetch_add(86_400, Ordering::Relaxed);
    remote.reset();
    let stamp = engine.state_stamp();
    // Not even a content read of the managed file can succeed during a 304.
    let locked_file = fs::OpenOptions::new().read(true).share_mode(0).open(agents.join(AGENT)).unwrap();
    let report = sync_which(&mut engine, &cfg, &Which::Due);
    assert_eq!(report.repos.len(), 3);
    assert!(report.repos.iter().all(|r| matches!(r.outcome, Outcome::Unchanged)));
    assert_eq!(remote.calls(), Calls { tree: 3, conditional: 3, ..Default::default() });
    assert_eq!(stamp, engine.state_stamp());
    drop(locked_file);
    fs::write(agents.join(AGENT), "mine").unwrap();
    remote.put(REPO, "pack/review.agent.md", "two");
    let one = Which::One(id.clone());
    let report = sync_which(&mut engine, &cfg, &one);
    assert!(matches!(skill(only(&report), AGENT).verdict, Verdict::Held(Hold::UpdateBlocked(_))));
    assert_eq!(engine.diff(&cfg, id, AGENT, &cancel_flag()).unwrap().files.len(), 1);
    assert_eq!(engine.preview(&cfg, id, AGENT, &cancel_flag()).unwrap().files[0].path, AGENT);
    let chosen = engine.resolve(&cfg, id, AGENT, Resolution::Overwrite, &cancel_flag()).unwrap();
    assert_eq!(chosen.backups.len(), 1);
    assert_eq!(fs::read_to_string(&chosen.backups[0]).unwrap(), "mine");
    assert_eq!(fs::read_to_string(agents.join(AGENT)).unwrap(), "two");
    drop(lock);
    assert_eq!(fs::read_to_string(neighbor).unwrap(), "private");
}

#[test]
fn loose_file_review_rejects_changed_bytes_and_a_successful_overwrite_clears_the_review() {
    let (w, remote, cfg) = mixed();
    let mut engine = w.engine(&remote);
    sync_all(&mut engine, &cfg);
    let source = &cfg.repos()[1];
    let target = w.state().loose_sources()[&source.id].binding().directory().join(AGENT);
    remote.put(REPO, "pack/review.agent.md", "two");

    engine.diff(&cfg, &source.id, AGENT, &cancel_flag()).unwrap();
    fs::write(&target, "changed after comparison").unwrap();
    let refused = engine.resolve(&cfg, &source.id, AGENT, Resolution::Overwrite, &cancel_flag()).unwrap_err();
    assert!(refused.to_string().contains("comparison changed"), "{refused}");
    assert_eq!(fs::read(&target).unwrap(), b"changed after comparison");

    engine.diff(&cfg, &source.id, AGENT, &cancel_flag()).unwrap();
    engine.resolve(&cfg, &source.id, AGENT, Resolution::Overwrite, &cancel_flag()).unwrap();
    let retry = engine.resolve(&cfg, &source.id, AGENT, Resolution::Overwrite, &cancel_flag());
    assert!(retry.is_ok(), "a retry must not reuse a comparison from before the successful overwrite: {retry:?}");
    assert_eq!(fs::read(&target).unwrap(), b"two");
}

#[test]
fn incomplete_or_unconfirmed_removal_never_deletes_a_file() {
    let (w, remote, cfg) = mixed();
    let mut e = w.engine(&remote);
    sync_all(&mut e, &cfg);
    let target = w.root.join("agents").join(AGENT);
    let one = Which::One(cfg.repos()[1].id.clone());
    remote.remove(REPO, "pack/review.agent.md");
    let confirmed = SyncOptions { confirm_mass_delete: true, ignore_etag: true, ..Default::default() };
    remote.set_truncated(REPO, true);
    let check = |e: &mut _, opts: &_| e_sync(e, &cfg, &one, opts);
    assert!(matches!(only(&check(&mut e, &confirmed)).outcome, Outcome::Failed(_)));
    remote.set_truncated(REPO, false);
    remote.fail_historical_tree(REPO, Some(RemoteError::new(FailureKind::Server, "pin unavailable")));
    assert!(matches!(only(&check(&mut e, &confirmed)).outcome, Outcome::Failed(_)));
    remote.fail_historical_tree(REPO, None);
    remote.put(REPO, "pack/CON.agent.md", "rejected");
    assert!(matches!(only(&check(&mut e, &confirmed)).outcome, Outcome::Failed(_)));
    remote.remove(REPO, "pack/CON.agent.md");
    assert!(matches!(skill(only(&check(&mut e, &SyncOptions::default())), AGENT).verdict, Verdict::Held(Hold::MassDeletion)));
    assert_eq!(fs::read_to_string(&target).unwrap(), "one\n");
    assert_eq!(skill(only(&check(&mut e, &confirmed)), AGENT).verdict, Verdict::Delete);
    assert!(!target.exists());
    assert_eq!(fs::read_to_string(w.root.join("instructions").join(RULE)).unwrap(), "rule");
}

fn e_sync(e: &mut tiny_skill_sync::engine::Engine, cfg: &Config, which: &Which, opts: &SyncOptions) -> tiny_skill_sync::engine::SyncReport {
    e.sync(cfg, which, opts, &cancel_flag()).unwrap()
}

#[test]
fn restart_recovers_only_a_configured_leaf_and_keeps_detached_work() {
    let (w, remote, cfg) = mixed();
    let mut engine = w.engine(&remote);
    sync_all(&mut engine, &cfg);
    let agents = w.root.join("agents");
    fs::write(agents.join("neighbor"), "mine").unwrap();
    remote.put(REPO, "pack/review.agent.md", "new");
    engine.set_step_hook(|s| if s == Step::OldMoved { Err(RemoteError::new(FailureKind::Local, "fixture stop")) } else { Ok(()) });
    let id = &cfg.repos()[1].id;
    assert!(matches!(only(&sync_which(&mut engine, &cfg, &Which::One(id.clone()))).outcome, Outcome::Failed(_)));
    let mut detached = cfg.clone();
    detached.remove(id);
    let mut restarted = w.engine(&remote);
    let work = w.root.join(".tss-leaf-work");
    let before = snapshot_of(&work);
    assert!(!before.is_empty());
    let started = restarted.startup(&detached).unwrap();
    assert!(started.verify.orphans.contains(id));
    assert!(!agents.join(AGENT).exists());
    assert_eq!(snapshot_of(&work), before);
    assert!(restarted.startup(&cfg).unwrap().verify.notes.iter().any(|n| n.skill == AGENT));
    assert_eq!(status_of(&restarted.verify(&cfg, Access::ReadOnly).unwrap(), AGENT), clean());
    assert_eq!(fs::read_to_string(agents.join(AGENT)).unwrap(), "one\n");
    assert_eq!(fs::read_to_string(agents.join("neighbor")).unwrap(), "mine");
}

#[test]
fn interrupted_loose_file_waits_for_its_original_source_before_recovery() {
    let w = World::new("loose-pending-rebound");
    let remote = FakeRemote::new();
    remote.seed(REPO, &[("pack/review.agent.md", "one")]);
    let spec = RepoSpec::parse("https://github.com/o/fixtures/tree/main/pack").unwrap();
    let mut cfg = Config::default();
    cfg.add_source(SourceKind::Agents, &spec, None, false);

    let source_id = cfg.repos()[0].id.clone();
    let mut engine = w.engine(&remote);
    assert!(matches!(only(&sync_all(&mut engine, &cfg)).outcome, Outcome::Synced));
    let record = w.state().loose_sources()[&source_id].binding().clone();
    let target = record.directory().join(AGENT);
    let neighbor = record.directory().join("personal.agent.md");
    fs::write(&neighbor, "private").unwrap();
    remote.put(REPO, "pack/review.agent.md", "two");

    engine.set_step_hook(|step| if step == Step::Marked { Err(RemoteError::new(FailureKind::Local, "fixture interruption")) } else { Ok(()) });
    let interrupted = sync_which(&mut engine, &cfg, &Which::One(source_id.clone()));
    assert!(matches!(only(&interrupted).outcome, Outcome::Failed(_)), "{interrupted:?}");
    assert!(w.state().loose_sources()[&source_id].leaf(AGENT).unwrap().interrupted());
    assert_eq!(fs::read(&target).unwrap(), b"one");
    let work = w.root.join(".tss-leaf-work");
    let before = snapshot_of(&work);

    let mut rebound = cfg.clone();
    rebound.update_lane_at(0, |r| r.branch = "feature/other".into());
    assert_eq!(rebound.repos()[0].id, source_id);
    let mut restarted = w.engine(&remote);
    restarted.startup(&rebound).unwrap();
    assert!(w.state().loose_sources()[&source_id].leaf(AGENT).unwrap().interrupted());
    assert_eq!(snapshot_of(&work), before);
    assert_eq!(fs::read(&target).unwrap(), b"one");
    assert_eq!(fs::read(&neighbor).unwrap(), b"private");

    restarted.startup(&cfg).unwrap();
    assert!(!w.state().loose_sources()[&source_id].leaf(AGENT).unwrap().interrupted());
    assert_eq!(fs::read(&target).unwrap(), b"one");
    assert_eq!(fs::read(&neighbor).unwrap(), b"private");
    assert!(w.work_leftovers().is_empty());
}

#[test]
fn explicit_sync_on_rebound_agents_source_fails_before_remote_request() {
    let w = World::new("loose-pending-explicit-rebound");
    let remote = FakeRemote::new();
    remote.seed(REPO, &[("pack/review.agent.md", "one")]);
    let spec = RepoSpec::parse("https://github.com/o/fixtures/tree/main/pack").unwrap();
    let mut cfg = Config::default();
    cfg.add_source(SourceKind::Agents, &spec, None, false);

    let source_id = cfg.repos()[0].id.clone();
    let mut engine = w.engine(&remote);
    assert!(matches!(only(&sync_all(&mut engine, &cfg)).outcome, Outcome::Synced));
    let binding = w.state().loose_sources()[&source_id].binding().clone();
    let target = binding.directory().join(AGENT);
    remote.put(REPO, "pack/review.agent.md", "two");

    engine.set_step_hook(|step| if step == Step::Marked { Err(RemoteError::new(FailureKind::Local, "fixture interruption")) } else { Ok(()) });
    let interrupted = sync_which(&mut engine, &cfg, &Which::One(source_id.clone()));
    assert!(matches!(only(&interrupted).outcome, Outcome::Failed(_)), "{interrupted:?}");
    assert!(w.state().loose_sources()[&source_id].leaf(AGENT).unwrap().interrupted());

    let mut rebound = cfg.clone();
    rebound.update_lane_at(0, |r| r.branch = "feature/other".into());
    assert_eq!(rebound.repos()[0].id, source_id);
    let mut restarted = w.engine(&remote);
    restarted.startup(&rebound).unwrap();
    assert!(w.state().loose_sources()[&source_id].leaf(AGENT).unwrap().interrupted());

    let destination_before = snapshot_of(&binding.directory());
    let work = w.root.join(".tss-leaf-work");
    let work_before = snapshot_of(&work);
    let state_before = fs::read(w.state_file()).unwrap();
    let file_before = fs::read(&target).unwrap();
    remote.reset();

    let report = restarted.sync(&rebound, &Which::One(source_id.clone()), &SyncOptions::default(), &cancel_flag()).unwrap();
    assert!(
        matches!(
            &only(&report).outcome,
            Outcome::Failed(failure)
                if failure.kind == FailureKind::Local
                    && failure.message == format!("source '{source_id}' no longer describes the recorded repository, branch and folder")
        ),
        "{:?}",
        only(&report).outcome
    );
    assert_eq!(remote.calls(), Calls::default());
    assert_eq!(fs::read(&target).unwrap(), file_before);
    assert_eq!(snapshot_of(&binding.directory()), destination_before);
    assert_eq!(snapshot_of(&work), work_before);
    assert_eq!(fs::read(w.state_file()).unwrap(), state_before);
    assert!(w.state().loose_sources()[&source_id].leaf(AGENT).unwrap().interrupted());
}

#[test]
fn interrupted_loose_file_rejects_a_skills_source_reusing_its_id() {
    let w = World::new("loose-pending-skills-id-collision");
    let remote = FakeRemote::new();
    remote.seed(REPO, &[("pack/review.agent.md", "one")]);
    let spec = RepoSpec::parse("https://github.com/o/fixtures/tree/main/pack").unwrap();
    let mut agents_cfg = Config::default();
    agents_cfg.add_source(SourceKind::Agents, &spec, None, false);

    let source_id = agents_cfg.repos()[0].id.clone();
    let mut engine = w.engine(&remote);
    assert!(matches!(only(&sync_all(&mut engine, &agents_cfg)).outcome, Outcome::Synced));
    let record = w.state().loose_sources()[&source_id].binding().clone();
    let managed = record.directory().join(AGENT);
    let neighbor = record.directory().join("personal.agent.md");
    fs::write(&neighbor, "private").unwrap();
    remote.put(REPO, "pack/review.agent.md", "two");

    engine.set_step_hook(|step| if step == Step::Marked { Err(RemoteError::new(FailureKind::Local, "fixture interruption")) } else { Ok(()) });
    let interrupted = sync_which(&mut engine, &agents_cfg, &Which::One(source_id.clone()));
    assert!(matches!(only(&interrupted).outcome, Outcome::Failed(_)), "{interrupted:?}");
    assert!(w.state().loose_sources()[&source_id].leaf(AGENT).unwrap().interrupted());

    let mut skills_cfg = Config::default();
    skills_cfg.add_source(SourceKind::Skills, &spec, None, false);
    assert_eq!(skills_cfg.repos()[0].id, source_id);

    let managed_before = fs::read(&managed).unwrap();
    let neighbor_before = fs::read(&neighbor).unwrap();
    let state_before = fs::read(w.state_file()).unwrap();
    let work = w.root.join(".tss-leaf-work");
    let work_before = snapshot_of(&work);

    let mut restarted = w.engine(&remote);
    let startup = restarted.startup(&skills_cfg);
    let managed_unchanged = fs::read(&managed).unwrap() == managed_before;
    let neighbor_unchanged = fs::read(&neighbor).unwrap() == neighbor_before;
    let state_unchanged = fs::read(w.state_file()).unwrap() == state_before;
    let work_unchanged = snapshot_of(&work) == work_before;
    assert!(
        startup.is_err(),
        "startup must reject a Skills source colliding with interrupted Agents work, got {startup:?}; unchanged: managed={managed_unchanged}, neighbor={neighbor_unchanged}, state/journal={state_unchanged}, work={work_unchanged}"
    );
    assert!(managed_unchanged, "the managed file must remain unchanged");
    assert!(neighbor_unchanged, "the neighbor must remain unchanged");
    assert!(state_unchanged, "the state and journal record must remain unchanged");
    assert!(work_unchanged, "the leaf work tree must remain unchanged");
}

#[test]
fn continuation_a_blocked_bound_file_does_not_hold_or_relocate_its_sibling() {
    let mut failures = Vec::new();
    for alternate in [false, true] {
        let (w, remote, cfg) = mixed();
        let root = w.root.join("agents");
        let bound = root.join("fixtures");
        let alpha = bound.join(AGENT);
        let beta = "beta.agent.md";
        fs::create_dir_all(&bound).unwrap();
        fs::write(&alpha, "one\r\n").unwrap();
        remote.put(REPO, "pack/beta.agent.md", "beta before");
        let mut engine = w.engine(&remote);
        let which = Which::One(cfg.repos()[1].id.clone());
        let installed = sync_which(&mut engine, &cfg, &which);
        assert!(matches!(only(&installed).outcome, Outcome::Synced), "{installed:?}");
        assert_eq!(skill(only(&installed), AGENT).result, ApplyResult::Done);
        fs::write(&alpha, "my edit\r\n").unwrap();
        let flat_alpha = root.join(AGENT);
        let held = if alternate {
            fs::write(&flat_alpha, "another local file").unwrap();
            None
        } else {
            Some(fs::OpenOptions::new().read(true).share_mode(0).open(&alpha).unwrap())
        };
        remote.put(REPO, "pack/review.agent.md", "new alpha");
        remote.put(REPO, "pack/beta.agent.md", "new beta");
        let report = sync_which(&mut engine, &cfg, &which);
        drop(held);
        if !matches!(only(&report).outcome, Outcome::NeedsAttention) || fs::read(bound.join(beta)).unwrap() != b"new beta" {
            failures.push(format!("alternate={alternate}: {:?}", only(&report).outcome));
        }
        assert_eq!(fs::read(alpha).unwrap(), b"my edit\r\n");
        assert!(!root.join(beta).exists());
        if alternate {
            assert_eq!(fs::read(flat_alpha).unwrap(), b"another local file");
        } else {
            assert!(!flat_alpha.exists());
        }
    }
    assert!(failures.is_empty(), "a local problem held the whole source: {failures:?}");
}
