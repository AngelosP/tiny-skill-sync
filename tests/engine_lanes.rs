//! One source per GitHub location: one listing of the branch root feeds a lane per type, and nothing the listing
//! still shows is ever removed.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::{Calls, FakeRemote, FakeTokens, T0, World, cancel_flag, only, skill, sync_all, sync_which, sync_with};
use tiny_skill_sync::config::Config;
use tiny_skill_sync::engine::{Access, Outcome, RepoReport, SyncOptions, Which};
use tiny_skill_sync::github::{FailureKind, RemoteError};
use tiny_skill_sync::inventory::Status;
use tiny_skill_sync::reconcile::{Hold, Skip, Verdict};
use tiny_skill_sync::source::SourceKind;
use tiny_skill_sync::sources::{SourceConfig, allocate_source_ids};

const REPO: &str = "o/kit";
const AGENT: &str = "review.agent.md";
const RULE: &str = "style.instructions.md";
const MARKET: &str = r#"{"name":"work","owner":{"name":"Org"},"plugins":[{"name":"tool","source":"./plugins/tool","version":"2"}]}"#;
const TOOL: &str = r#"{"name":"tool","author":{"name":"Team"},"hooks":{},"skills":"skills"}"#;

/// One branch with a standalone skill, an agent, a modular instruction and a plugin that bundles a skill.
fn kit(remote: &FakeRemote) {
    remote.seed(
        REPO,
        &[
            ("skills/alpha/SKILL.md", "alpha v1"),
            ("agents/review.agent.md", "review v1"),
            ("instructions/style.instructions.md", "style v1"),
            ("marketplace.json", MARKET),
            ("plugins/tool/.plugin/plugin.json", TOOL),
            ("plugins/tool/skills/explain/SKILL.md", "explain"),
        ],
    );
}

/// Adds a source the way `tss add` will: fresh IDs for it and its lanes. Plugins stay off here: their lane is the
/// v0.8.0 pipeline, which needs Copilot CLI.
fn add(cfg: &mut Config, path: &str, set: impl FnOnce(&mut SourceConfig)) -> SourceConfig {
    let (id, lanes) = allocate_source_ids("kit", &mut cfg.taken_ids());
    let mut source = SourceConfig { id, repo: REPO.into(), path: path.into(), lanes, ..SourceConfig::default() };
    source.types.set(SourceKind::Plugins, false);
    set(&mut source);
    cfg.add_source_config(source.clone()).unwrap();
    source
}

fn only_kinds(kinds: &[SourceKind]) -> impl FnOnce(&mut SourceConfig) + '_ {
    move |s| {
        for kind in [SourceKind::Skills, SourceKind::Agents, SourceKind::Instructions, SourceKind::Plugins] {
            s.types.set(kind, kinds.contains(&kind));
        }
    }
}

fn find(dir: &Path, name: &str) -> Option<PathBuf> {
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find(&path, name) {
                return Some(found);
            }
        } else if entry.file_name() == name {
            return Some(path);
        }
    }
    None
}

fn failed(report: &RepoReport) -> &tiny_skill_sync::engine::Failure {
    match &report.outcome {
        Outcome::Failed(f) => f,
        other => panic!("{}: expected a failure, got {other:?}", report.repo_id),
    }
}

fn lane(source: &SourceConfig, kind: SourceKind) -> String {
    source.lanes.id(kind).unwrap().to_owned()
}

#[test]
fn one_branch_with_every_type_is_one_source_and_an_off_type_is_listed_but_never_synced() {
    let w = World::new("lanes-all");
    let remote = FakeRemote::new();
    kit(&remote);
    let mut cfg = Config::default();
    let source = add(&mut cfg, "", |_| {});
    let mut engine = w.engine(&remote);

    let report = sync_all(&mut engine, &cfg);
    assert_eq!(
        report.repos.iter().map(|r| r.repo_id.clone()).collect::<Vec<_>>(),
        [source.id.clone(), lane(&source, SourceKind::Agents), lane(&source, SourceKind::Instructions)]
    );
    assert!(report.repos.iter().all(|r| matches!(r.outcome, Outcome::Synced)), "{report:?}");
    assert_eq!(remote.calls().tree, 1, "one listing of the branch root for the whole source");
    // The plugin's own skill is part of the plugin: never a loose copy.
    assert_eq!(w.skill_names(), ["alpha"]);
    let agent = find(&w.root.join("agents"), AGENT).expect("the agent is installed");
    let rule = find(&w.root.join("instructions"), RULE).expect("the instruction is installed");

    // Agents off for this source: listed, not synced, and what was installed stays as it is with its record.
    cfg.edit(|sources, _| sources[0].types.agents = Some(false));
    remote.put(REPO, "agents/review.agent.md", "review v2");
    remote.put(REPO, "instructions/style.instructions.md", "style v2");
    remote.put(REPO, "skills/alpha/SKILL.md", "alpha v2");
    let report = sync_all(&mut engine, &cfg);
    assert_eq!(report.repos.len(), 2, "{report:?}");
    assert_eq!(fs::read_to_string(&agent).unwrap(), "review v1");
    assert_eq!(fs::read_to_string(&rule).unwrap(), "style v2");
    assert_eq!(w.read("alpha/SKILL.md"), "alpha v2");
    // Even gone upstream, an off type deletes nothing.
    remote.remove(REPO, "agents/review.agent.md");
    sync_all(&mut engine, &cfg);
    assert_eq!(fs::read_to_string(&agent).unwrap(), "review v1");
    assert!(w.state().loose_sources()[&lane(&source, SourceKind::Agents)].leaf(AGENT).is_some(), "the off lane keeps its record");
    let verify = engine.verify(&cfg, Access::ReadOnly).unwrap();
    assert!(verify.orphans.is_empty(), "an off lane is still configured: {:?}", verify.orphans);
}

#[test]
fn root_defaults_skip_repository_items_and_examples_but_each_rule_can_be_changed_alone() {
    let w = World::new("lanes-monorepo");
    let remote = FakeRemote::new();
    remote.seed(
        REPO,
        &[
            ("packages/api/.github/agents/review.agent.md", "review"),
            (".github/instructions/style.instructions.md", "style"),
            ("examples/team/demo/SKILL.md", "demo"),
        ],
    );
    let mut cfg = Config::default();
    add(&mut cfg, "", |_| {});
    let mut engine = w.engine(&remote);
    sync_all(&mut engine, &cfg);
    assert!(find(&w.root.join("instructions"), RULE).is_some(), "instructions in .github are synced");
    assert!(find(&w.root.join("agents"), AGENT).is_none(), "an agent in a repository's .github folder is not");
    assert!(w.skill_names().is_empty(), "nor a skill in an examples folder");

    cfg.edit(|sources, _| sources[0].include_repo_items = Some(true));
    sync_all(&mut engine, &cfg);
    assert!(find(&w.root.join("agents"), AGENT).is_some());
    assert!(w.skill_names().is_empty(), "including repository items does not change the folder rule");

    cfg.edit(|sources, _| sources[0].ignore_allow = vec!["examples".into()]);
    sync_all(&mut engine, &cfg);
    assert_eq!(w.skill_names(), ["demo"]);

    // Pointed at the folders themselves, the rules (which only look below the folder) do not apply.
    let w = World::new("lanes-monorepo-folders");
    let mut cfg = Config::default();
    add(&mut cfg, "packages/api/.github/agents", |_| {});
    add(&mut cfg, "examples/team", |_| {});
    let mut engine = w.engine(&remote);
    let report = sync_all(&mut engine, &cfg);
    assert!(report.repos.iter().all(|r| !matches!(r.outcome, Outcome::Failed(_))), "{report:?}");
    assert!(find(&w.root.join("agents"), AGENT).is_some());
    assert_eq!(w.skill_names(), ["demo"]);
}

#[test]
fn a_skill_inside_a_plugin_is_never_a_loose_copy_but_a_source_at_its_folder_installs_it() {
    let w = World::new("lanes-bundled");
    let remote = FakeRemote::new();
    kit(&remote);
    let mut cfg = Config::default();
    add(&mut cfg, "", only_kinds(&[SourceKind::Skills]));
    let mut engine = w.engine(&remote);
    sync_all(&mut engine, &cfg);
    assert_eq!(w.skill_names(), ["alpha"]);

    add(&mut cfg, "plugins/tool/skills", only_kinds(&[SourceKind::Skills]));
    let report = sync_all(&mut engine, &cfg);
    assert!(report.repos.iter().all(|r| !matches!(r.outcome, Outcome::Failed(_))), "{report:?}");
    assert_eq!(w.skill_names(), ["alpha", "explain"]);
}

#[test]
fn a_folder_rule_keeps_an_owned_skill_with_its_files_and_record_and_lifting_it_still_holds_an_edit() {
    let w = World::new("lanes-protected");
    let remote = FakeRemote::new();
    remote.seed(REPO, &[("skills/alpha/SKILL.md", "alpha v1"), ("extra/beta/SKILL.md", "beta v1")]);
    let mut cfg = Config::default();
    let source = add(&mut cfg, "", only_kinds(&[SourceKind::Skills]));
    let mut engine = w.engine(&remote);
    sync_all(&mut engine, &cfg);
    assert_eq!(w.skill_names(), ["alpha", "beta"]);

    cfg.edit(|sources, _| sources[0].ignore_add = vec!["extra".into()]);
    remote.put(REPO, "extra/beta/SKILL.md", "beta v2");
    let report = sync_all(&mut engine, &cfg);
    assert!(matches!(&skill(only(&report), "beta").verdict, Verdict::Skipped(Skip::Protected { .. })), "{report:?}");
    assert_eq!(w.read("beta/SKILL.md"), "beta v1", "kept as it is, not updated");
    assert!(w.state().repo(&source.id).unwrap().skills.iter().any(|s| s.name == "beta"), "still owned");

    // Lifted again, the normal full check applies: an edit made meanwhile holds the update.
    w.write("beta/SKILL.md", "mine");
    cfg.edit(|sources, _| sources[0].ignore_add.clear());
    let report = sync_all(&mut engine, &cfg);
    assert!(skill(only(&report), "beta").verdict.needs_attention(), "{report:?}");
    assert_eq!(w.read("beta/SKILL.md"), "mine");
}

#[test]
fn turning_a_type_off_runs_no_lane_for_it() {
    let w = World::new("lanes-type-off");
    let remote = FakeRemote::new();
    kit(&remote);
    let mut cfg = Config::default();
    add(&mut cfg, "", only_kinds(&[SourceKind::Skills, SourceKind::Instructions]));
    let mut engine = w.engine(&remote);
    sync_all(&mut engine, &cfg);
    cfg.edit(|sources, _| sources[0].types.skills = Some(false));
    remote.remove(REPO, "skills/alpha");
    let report = sync_all(&mut engine, &cfg);
    assert_eq!(report.repos.len(), 1);
    assert_eq!(w.read("alpha/SKILL.md"), "alpha v1", "an off type is never reconciled");
    // Off for every type, a source does not even list.
    cfg.edit(|sources, _| sources[0].types.instructions = Some(false));
    remote.reset();
    assert!(sync_all(&mut engine, &cfg).repos.is_empty());
    assert_eq!(remote.calls(), Calls::default());
}

#[test]
fn two_items_with_one_windows_name_install_neither_and_keep_the_owner() {
    let w = World::new("lanes-collision");
    let remote = FakeRemote::new();
    remote.seed(REPO, &[("a/tool/SKILL.md", "a1"), ("c/other/SKILL.md", "o1")]);
    let mut cfg = Config::default();
    let source = add(&mut cfg, "", only_kinds(&[SourceKind::Skills]));
    let mut engine = w.engine(&remote);
    sync_all(&mut engine, &cfg);
    assert_eq!(w.skill_names(), ["other", "tool"]);

    remote.seed(REPO, &[("b/Tool/SKILL.md", "b1"), ("a/tool/SKILL.md", "a2"), ("c/other/SKILL.md", "o2")]);
    sync_all(&mut engine, &cfg);
    assert_eq!(w.read("tool/SKILL.md"), "a1", "neither copy is installed over the owned one");
    assert!(w.state().repo(&source.id).unwrap().skills.iter().any(|s| s.name == "tool"), "the owner keeps it");
    assert_eq!(w.read("other/SKILL.md"), "o2", "unrelated items still sync");
    assert_eq!(w.skill_names(), ["other", "tool"]);
}

#[test]
fn a_cut_short_or_failed_listing_changes_nothing_in_any_lane() {
    let w = World::new("lanes-cut-short");
    let remote = FakeRemote::new();
    kit(&remote);
    let mut cfg = Config::default();
    add(&mut cfg, "", |_| {});
    let mut engine = w.engine(&remote);
    sync_all(&mut engine, &cfg);
    remote.remove(REPO, "skills/alpha");
    remote.remove(REPO, "agents/review.agent.md");
    let before = w.snapshot();

    remote.set_truncated(REPO, true);
    let report = sync_all(&mut engine, &cfg);
    assert_eq!(report.repos.len(), 3);
    for r in &report.repos {
        let f = failed(r);
        assert_eq!(f.kind, FailureKind::TooLarge);
        assert!(f.message.contains("in one piece") && f.message.contains("nothing was changed") && !f.message.contains("folder"), "{}", f.message);
    }
    assert_eq!(w.snapshot(), before);

    remote.set_truncated(REPO, false);
    remote.fail_tree(REPO, Some(RemoteError::new(FailureKind::Network, "offline")));
    let report = sync_all(&mut engine, &cfg);
    assert!(report.repos.iter().all(|r| failed(r).kind == FailureKind::Network));
    assert_eq!(w.snapshot(), before);
}

#[test]
fn a_failed_or_wrong_manifest_stops_the_whole_source_before_any_lane() {
    for (name, wrong) in [("fails", true), ("lies", false)] {
        let w = World::new(&format!("lanes-manifest-{name}"));
        let remote = FakeRemote::new();
        kit(&remote);
        let mut cfg = Config::default();
        add(&mut cfg, "", |_| {});
        let mut engine = w.engine(&remote);
        sync_all(&mut engine, &cfg);
        remote.remove(REPO, "skills/alpha");
        remote.put(REPO, "instructions/style.instructions.md", "style v2");
        if wrong {
            remote.fail_file(REPO, "plugins/tool/.plugin/plugin.json", Some(RemoteError::new(FailureKind::Network, "offline")));
        } else {
            remote.lie_about(REPO, "marketplace.json", "{}");
        }
        let before = w.snapshot();
        let report = sync_all(&mut engine, &cfg);
        assert_eq!(report.repos.len(), 3, "{name}");
        assert!(report.repos.iter().all(|r| matches!(r.outcome, Outcome::Failed(_))), "{name}: {report:?}");
        assert_eq!(w.snapshot(), before, "{name}");
    }
}

#[test]
fn a_remote_failure_in_one_lane_ends_the_source() {
    let w = World::new("lanes-lane-failure");
    let remote = FakeRemote::new();
    kit(&remote);
    let mut cfg = Config::default();
    add(&mut cfg, "", |_| {});
    let mut engine = w.engine(&remote);
    sync_all(&mut engine, &cfg);
    let rule = find(&w.root.join("instructions"), RULE).unwrap();
    remote.put(REPO, "agents/review.agent.md", "review v2");
    remote.put(REPO, "instructions/style.instructions.md", "style v2");
    remote.fail_file(REPO, "agents/review.agent.md", Some(RemoteError::new(FailureKind::Network, "offline")));

    let report = sync_all(&mut engine, &cfg);
    let (agents, rules) = (failed(&report.repos[1]), failed(&report.repos[2]));
    assert_eq!(agents.kind, FailureKind::Network);
    assert_eq!(rules.message, agents.message, "the lane after it reports the same reason");
    assert_eq!(fs::read_to_string(rule).unwrap(), "style v1", "and was not run");
}

#[test]
fn an_empty_listing_for_one_type_holds_its_deletions_even_when_another_type_is_listed() {
    let w = World::new("lanes-empty-guard");
    let remote = FakeRemote::new();
    kit(&remote);
    let mut cfg = Config::default();
    add(&mut cfg, "", only_kinds(&[SourceKind::Skills, SourceKind::Agents]));
    let mut engine = w.engine(&remote);
    sync_all(&mut engine, &cfg);
    remote.remove(REPO, "skills");
    remote.remove(REPO, "plugins");

    let report = sync_all(&mut engine, &cfg);
    assert_eq!(skill(&report.repos[0], "alpha").verdict, Verdict::Held(Hold::MassDeletion), "{report:?}");
    assert!(matches!(report.repos[1].outcome, Outcome::Synced | Outcome::Unchanged), "{report:?}");
    assert_eq!(w.read("alpha/SKILL.md"), "alpha v1");

    let report = sync_with(&mut engine, &cfg, SyncOptions { confirm_mass_delete: true, ..SyncOptions::default() });
    assert_eq!(skill(&report.repos[0], "alpha").verdict, Verdict::Delete);
    assert!(w.skill_names().is_empty());
}

#[test]
fn an_empty_listing_with_only_bundled_or_filtered_items_of_its_type_still_holds_its_deletions() {
    for (case, kind) in [("bundled", SourceKind::Skills), ("filtered", SourceKind::Skills), ("bundled", SourceKind::Agents)] {
        let w = World::new(&format!("lanes-empty-{case}-{kind:?}"));
        let remote = FakeRemote::new();
        kit(&remote);
        remote.put(REPO, "plugins/tool/agents/helper.agent.md", "bundled agent");
        remote.put(REPO, "examples/team/demo/SKILL.md", "filtered demo");
        let mut cfg = Config::default();
        add(&mut cfg, "", only_kinds(&[SourceKind::Skills, SourceKind::Agents]));
        let mut engine = w.engine(&remote);
        sync_all(&mut engine, &cfg);
        assert_eq!(w.skill_names(), ["alpha"], "neither the bundled nor the filtered skill is installed");
        let agent = find(&w.root.join("agents"), AGENT).expect("the agent is installed");
        assert!(find(&w.root.join("agents"), "helper.agent.md").is_none(), "nor the bundled agent");

        remote.remove(REPO, if kind == SourceKind::Skills { "skills" } else { "agents" });
        if case == "filtered" {
            remote.remove(REPO, "plugins");
        }
        let report = sync_all(&mut engine, &cfg);
        let (lane, name) = if kind == SourceKind::Skills { (&report.repos[0], "alpha") } else { (&report.repos[1], AGENT) };
        assert_eq!(skill(lane, name).verdict, Verdict::Held(Hold::MassDeletion), "{case} {kind:?}: {report:?}");
        assert_eq!(w.read("alpha/SKILL.md"), "alpha v1", "{case} {kind:?}");
        assert_eq!(fs::read_to_string(&agent).unwrap(), "review v1", "{case} {kind:?}");
    }
}

#[test]
fn an_unchanged_source_costs_one_request_and_writes_nothing_and_any_lane_id_checks_the_source() {
    let w = World::new("lanes-unchanged");
    let remote = FakeRemote::new();
    kit(&remote);
    let mut cfg = Config::default();
    let source = add(&mut cfg, "", |_| {});
    let mut engine = w.engine(&remote);
    sync_all(&mut engine, &cfg);
    let state = fs::read(w.state_file()).unwrap();
    let stamp = engine.state_stamp();
    remote.reset();

    let report = sync_all(&mut engine, &cfg);
    assert_eq!(report.repos.len(), 3);
    assert!(report.repos.iter().all(|r| matches!(r.outcome, Outcome::Unchanged)), "{report:?}");
    assert_eq!(remote.calls(), Calls { tree: 1, conditional: 1, ..Calls::default() });
    assert_eq!(fs::read(w.state_file()).unwrap(), state);
    assert_eq!(engine.state_stamp(), stamp);

    for id in [source.id.clone(), lane(&source, SourceKind::Instructions)] {
        let report = sync_which(&mut engine, &cfg, &Which::One(id));
        assert_eq!(report.repos.len(), 3, "one lane ID checks its whole source");
    }
}

#[test]
fn a_finished_history_search_keeps_an_unchanged_mixed_source_at_one_request() {
    let w = World::new("lanes-searched");
    let remote = FakeRemote::new();
    kit(&remote);
    w.write("alpha/SKILL.md", "my own alpha");
    let mut cfg = Config::default();
    let source = add(&mut cfg, "", |_| {});
    let mut engine = w.engine(&remote);
    let report = sync_all(&mut engine, &cfg);
    assert_eq!(skill(&report.repos[0], "alpha").verdict, Verdict::Held(Hold::UnmanagedDiffers), "{report:?}");
    assert!(!w.state().repo(&source.id).unwrap().searches.is_empty(), "the held folder's search is saved");
    let state = fs::read(w.state_file()).unwrap();
    let stamp = engine.state_stamp();
    remote.reset();

    let report = sync_all(&mut engine, &cfg);
    assert_eq!(report.repos.len(), 3);
    assert!(report.repos.iter().all(|r| matches!(r.outcome, Outcome::Unchanged)), "{report:?}");
    assert_eq!(remote.calls(), Calls { commit: 1, ..Calls::default() }, "only the branch commit is asked for");
    assert_eq!(fs::read(w.state_file()).unwrap(), state);
    assert_eq!(engine.state_stamp(), stamp);
    assert_eq!(w.read("alpha/SKILL.md"), "my own alpha");

    // A new commit still reaches every lane.
    remote.put(REPO, "agents/review.agent.md", "review v2");
    sync_all(&mut engine, &cfg);
    assert_eq!(fs::read_to_string(find(&w.root.join("agents"), AGENT).unwrap()).unwrap(), "review v2");
}

#[test]
fn a_type_turned_on_beside_a_finished_history_search_is_still_listed() {
    let w = World::new("lanes-searched-on");
    let remote = FakeRemote::new();
    kit(&remote);
    w.write("alpha/SKILL.md", "my own alpha");
    let mut cfg = Config::default();
    add(&mut cfg, "", only_kinds(&[SourceKind::Skills, SourceKind::Agents]));
    let mut engine = w.engine(&remote);
    sync_all(&mut engine, &cfg);
    sync_all(&mut engine, &cfg);
    assert!(find(&w.root.join("instructions"), RULE).is_none());

    cfg.edit(|sources, _| sources[0].types.instructions = Some(true));
    let report = sync_all(&mut engine, &cfg);
    assert_eq!(report.repos.len(), 3, "{report:?}");
    assert!(find(&w.root.join("instructions"), RULE).is_some(), "the new type is listed and installed: {report:?}");
}

#[test]
fn a_location_preview_resolves_and_lists_without_writing_or_saving() {
    let w = World::new("lanes-preview");
    let remote = FakeRemote::new();
    kit(&remote);
    remote.set_heads(REPO, &["main", "feature/x"]);
    let mut cfg = Config::default();
    let existing = add(&mut cfg, "", |_| {});
    cfg.edit(|_, global| global.types.agents = false);
    let mut engine = w.engine(&remote);
    let before = w.snapshot();

    let p = engine.preview_location(&cfg, "https://github.com/o/kit/tree/feature/x/plugins/tool/skills/", false, &cancel_flag()).unwrap();
    assert_eq!(
        (p.branch.as_str(), p.follows_default, p.source.branch.as_str(), p.source.path.as_str()),
        ("feature/x", false, "feature/x", "plugins/tool/skills")
    );
    assert_eq!(p.inventory.skills.iter().map(|i| (i.install_name.as_str(), &i.status)).collect::<Vec<_>>(), [("explain", &Status::Installable)]);
    assert_ne!(p.source.id, existing.id, "fresh IDs");
    assert!(p.source.lanes.id(SourceKind::Agents).is_some_and(|id| !cfg.taken_ids().contains(id)));

    let p = engine.preview_location(&cfg, "o/kit", false, &cancel_flag()).unwrap();
    assert_eq!((p.branch.as_str(), p.follows_default, p.source.branch.as_str()), ("main", true, ""));
    let agent = &p.inventory.agents[0];
    assert_eq!(agent.install_name, AGENT);
    assert!(!p.syncs(SourceKind::Agents, &agent.status), "listed with Sync off");
    assert!(p.syncs(SourceKind::Instructions, &p.inventory.instructions[0].status));
    assert_eq!(p.inventory.plugins.len(), 1);
    assert!(p.inventory.plugins[0].data.capabilities.hooks);
    assert!(p.inventory.skills.iter().any(|i| i.install_name == "explain" && matches!(i.status, Status::Bundled { .. })));

    for (bad, words) in [
        ("https://github.com/o/kit/tree/main/nope", "not on this branch"),
        ("https://github.com/o/kit/blob/main/x", "not a file"),
        ("https://github.com/o/kit/tree/v9", "not a branch"),
    ] {
        let err = engine.preview_location(&cfg, bad, false, &cancel_flag()).unwrap_err().to_string();
        assert!(err.contains(words), "{bad}: {err}");
    }
    assert_eq!(w.snapshot(), before, "a preview writes nothing");
    assert!(!remote.log().iter().any(|l| l.starts_with("file") && !l.contains("plugin.json") && !l.contains("marketplace.json")));
}

#[test]
fn a_source_with_none_of_a_type_binds_nothing_for_it_and_still_waits_its_interval_on_one_request() {
    let w = World::new("lanes-empty-type");
    let remote = FakeRemote::new();
    remote.seed(REPO, &[("skills/alpha/SKILL.md", "alpha")]);
    let mut cfg = Config::default();
    let source = add(&mut cfg, "", |_| {});
    let (mut engine, clock) = w.engine_at(&remote, &FakeTokens::none());
    let report = sync_all(&mut engine, &cfg);
    assert!(report.repos.iter().all(|r| !matches!(r.outcome, Outcome::Failed(_))), "{report:?}");
    assert!(report.repos[1].lists_nothing && report.repos[2].lists_nothing);
    assert!(w.state().loose_sources().is_empty(), "no destination is chosen for a type the source does not hold");
    let due = engine.next_wake(&cfg).unwrap();
    assert!(due > T0, "not due again at once");
    assert!(sync_which(&mut engine, &cfg, &Which::Due).repos.is_empty());

    clock.store(due, std::sync::atomic::Ordering::Relaxed);
    remote.reset();
    let report = sync_which(&mut engine, &cfg, &Which::Due);
    assert_eq!(report.repos.len(), 3);
    assert_eq!(remote.calls(), Calls { tree: 1, conditional: 1, ..Calls::default() });

    // Once the type turns up, the lane binds and installs as any new source would.
    remote.put(REPO, "agents/review.agent.md", "review");
    sync_which(&mut engine, &cfg, &Which::One(lane(&source, SourceKind::Agents)));
    assert!(find(&w.root.join("agents"), AGENT).is_some());
}

#[test]
fn a_mass_deletion_confirmed_for_one_lane_removes_only_that_lanes_items() {
    for named in [SourceKind::Agents, SourceKind::Skills] {
        let w = World::new(&format!("lanes-confirm-{named:?}"));
        let remote = FakeRemote::new();
        remote.seed(REPO, &[("skills/alpha/SKILL.md", "alpha v1"), ("agents/review.agent.md", "review v1"), ("README.md", "readme")]);
        let mut cfg = Config::default();
        let source = add(&mut cfg, "", only_kinds(&[SourceKind::Skills, SourceKind::Agents]));
        let mut engine = w.engine(&remote);
        sync_all(&mut engine, &cfg);
        let agent = find(&w.root.join("agents"), AGENT).expect("the agent is installed");
        remote.remove(REPO, "skills");
        remote.remove(REPO, "agents");
        let report = sync_all(&mut engine, &cfg);
        assert_eq!(skill(&report.repos[0], "alpha").verdict, Verdict::Held(Hold::MassDeletion), "{report:?}");
        assert_eq!(skill(&report.repos[1], AGENT).verdict, Verdict::Held(Hold::MassDeletion), "{report:?}");

        // A source ID names its skills lane; the agents lane has its own ID.
        let id = if named == SourceKind::Skills { source.id.clone() } else { lane(&source, SourceKind::Agents) };
        let opts = SyncOptions { confirm_mass_delete: true, ..SyncOptions::default() };
        let report = engine.sync(&cfg, &Which::One(id), &opts, &cancel_flag()).unwrap();
        assert_eq!(
            (w.skill_names().is_empty(), !agent.exists()),
            (named == SourceKind::Skills, named == SourceKind::Agents),
            "(skills removed, agents removed): confirming the {named:?} lane must remove only that lane's items: {report:?}"
        );
        let (other, name) = if named == SourceKind::Skills { (&report.repos[1], AGENT) } else { (&report.repos[0], "alpha") };
        assert_eq!(skill(other, name).verdict, Verdict::Held(Hold::MassDeletion), "the lane not named stays held: {report:?}");
    }
}

#[test]
fn a_protected_spelling_with_the_same_windows_name_keeps_an_owned_skill() {
    let w = World::new("lanes-protected-sigma");
    let remote = FakeRemote::new();
    remote.seed(REPO, &[("skills/ab\u{3c3}/SKILL.md", "sigma")]);
    let mut cfg = Config::default();
    add(&mut cfg, "", only_kinds(&[SourceKind::Skills]));
    let mut engine = w.engine(&remote);
    sync_all(&mut engine, &cfg);
    assert_eq!(w.skill_names(), ["ab\u{3c3}"]);

    // Windows treats a capital sigma as the same name as a small one (NTFS refuses both side by side), while
    // lowercasing turns the final capital into the final small sigma; the default `examples` rule protects it.
    remote.remove(REPO, "skills/ab\u{3c3}");
    remote.put(REPO, "examples/ab\u{3a3}/SKILL.md", "sigma");
    let report = sync_all(&mut engine, &cfg);
    assert!(w.exists("ab\u{3c3}/SKILL.md"), "the owned skill was deleted although upstream still has it under a protected folder: {report:?}");
    assert!(matches!(&skill(only(&report), "ab\u{3c3}").verdict, Verdict::Skipped(Skip::Protected { .. })), "{report:?}");
}

#[test]
fn plugin_manifests_count_against_the_download_budget_before_each_download() {
    let w = World::new("lanes-manifest-budget");
    let remote = FakeRemote::new();
    let manifest = |name: &str| format!(r#"{{"name":"{name}","description":"{}"}}"#, "x".repeat(100));
    let (a, b, c) = (manifest("a"), manifest("b"), manifest("c"));
    remote.seed(
        REPO,
        &[
            ("skills/alpha/SKILL.md", "alpha"),
            ("plugins/a/.plugin/plugin.json", &a),
            ("plugins/b/.plugin/plugin.json", &b),
            ("plugins/c/.plugin/plugin.json", &c),
        ],
    );
    let mut cfg = Config::default();
    add(&mut cfg, "", only_kinds(&[SourceKind::Skills]));
    let mut engine = w.engine(&remote);
    sync_all(&mut engine, &cfg);
    remote.put(REPO, "skills/alpha/SKILL.md", "alpha v2");
    remote.reset();
    // Every check takes `home\engine.lock` first, so compare only the destinations and the saved state.
    let places = || {
        let read = |dir: &str| common::snapshot_of(&w.root.join(dir));
        (w.skills_snapshot(), read("agents"), read("instructions"), read("plugins"), fs::read_to_string(w.state_file()).ok())
    };
    let before = places();

    let report = sync_with(&mut engine, &cfg, SyncOptions { max_download_bytes: 150, ..SyncOptions::default() });
    assert!(
        remote.calls().file <= 1,
        "manifests were downloaded past the 150-byte budget (each is about 125 bytes): {:?}",
        remote.log().iter().filter(|l| l.starts_with("file")).collect::<Vec<_>>()
    );
    assert!(report.repos.iter().all(|r| failed(r).kind == FailureKind::TooLarge), "{report:?}");
    assert_eq!(places(), before, "nothing installed or changed");
}

#[test]
fn a_bare_repository_preview_resolves_the_default_branch_before_listing_it() {
    let w = World::new("lanes-preview-bare");
    let remote = FakeRemote::new();
    kit(&remote);
    remote.set_heads(REPO, &["trunk"]);
    let cfg = Config::default();
    let mut engine = w.engine(&remote);

    let p = engine.preview_location(&cfg, "o/kit", false, &cancel_flag()).unwrap();
    assert_eq!((p.branch.as_str(), p.follows_default), ("trunk", true));
    let log = remote.log();
    let resolve = log.iter().position(|l| l.starts_with("default_branch"));
    let list = log.iter().position(|l| l.starts_with("tree"));
    assert!(
        resolve.is_some_and(|r| list.is_some_and(|t| r < t)) && log.iter().any(|l| l == "tree o/kit@trunk:"),
        "the listing must be of the resolved branch by name, after resolving it: {log:?}"
    );
}

fn seen_two_sources(name: &str) -> (World, std::sync::Arc<FakeRemote>, Config, SourceConfig, SourceConfig) {
    let w = World::new(name);
    let remote = FakeRemote::new();
    remote.seed(REPO, &[("skills/alpha/SKILL.md", "alpha")]);
    remote.seed("o/other", &[("skills/beta/SKILL.md", "beta")]);
    let mut cfg = Config::default();
    let a = add(&mut cfg, "", only_kinds(&[SourceKind::Skills]));
    let (id, lanes) = allocate_source_ids("other", &mut cfg.taken_ids());
    let mut b = SourceConfig { id, repo: "o/other".into(), lanes, ..SourceConfig::default() };
    only_kinds(&[SourceKind::Skills])(&mut b);
    cfg.add_source_config(b.clone()).unwrap();
    (w, remote, cfg, a, b)
}

#[test]
fn seen_json_keeps_what_another_engine_saved_since_this_one_loaded_it() {
    let (w, remote, cfg, a, b) = seen_two_sources("lanes-seen-merge");
    let file = w.paths().seen_file();
    let mut first = w.engine(&remote);
    sync_all(&mut first, &cfg);

    let mut second = w.engine(&remote);
    remote.put("o/other", "skills/gamma/SKILL.md", "gamma");
    sync_which(&mut second, &cfg, &Which::One(b.id.clone()));
    let newer_b = tiny_skill_sync::seen::load(&file)[&b.id].clone();
    assert_eq!(newer_b.summary.found[0], 2, "the second engine saved the new listing of b");

    remote.put(REPO, "skills/delta/SKILL.md", "delta");
    sync_which(&mut first, &cfg, &Which::One(a.id.clone()));
    let saved = tiny_skill_sync::seen::load(&file);
    assert_eq!(saved[&a.id].summary.found[0], 2, "a's new listing is saved");
    assert_eq!(saved[&b.id], newer_b, "the first engine saved its stale copy of b over what the second engine saved");
}

#[test]
fn seen_json_from_a_newer_version_is_never_overwritten() {
    let (w, remote, cfg, ..) = seen_two_sources("lanes-seen-newer");
    let file = w.paths().seen_file();
    let newer = br#"{"version":2,"sources":{"future":{"at":1}}}"#;
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, newer).unwrap();
    let mut engine = w.engine(&remote);
    let report = sync_all(&mut engine, &cfg);
    assert!(report.repos.iter().all(|r| !matches!(r.outcome, Outcome::Failed(_))), "{report:?}");
    assert_eq!(fs::read_to_string(&file).unwrap(), String::from_utf8_lossy(newer), "a check overwrote seen.json written by a newer version");
}

/// A source shaped like an auto-detected one: every type on, skills found, and either only the plugins lane or the
/// agents, instructions and plugins lanes listing nothing. Checked once by one engine at `T0`.
fn checked_with_empty_lanes(name: &str, files: bool) -> (World, std::sync::Arc<FakeRemote>, Config, SourceConfig, Option<i64>) {
    let w = World::new(name);
    let remote = FakeRemote::new();
    remote.seed(REPO, &[("skills/alpha/SKILL.md", "alpha")]);
    if files {
        remote.seed(REPO, &[("agents/review.agent.md", "review"), ("instructions/style.instructions.md", "style")]);
    }
    let mut cfg = Config::default();
    let source = add(&mut cfg, "", only_kinds(&[SourceKind::Skills, SourceKind::Agents, SourceKind::Instructions, SourceKind::Plugins]));
    let (mut engine, _) = w.engine_at(&remote, &FakeTokens::none());
    let report = sync_all(&mut engine, &cfg);
    assert_eq!(report.repos.len(), 4, "{report:?}");
    assert!(report.repos.iter().all(|r| !matches!(r.outcome, Outcome::Failed(_))), "{report:?}");
    let due = engine.next_wake(&cfg);
    assert!(due.is_some_and(|d| d > T0), "the first engine waits its interval: {due:?}");
    (w, remote, cfg, source, due)
}

#[test]
fn after_a_restart_lanes_that_listed_nothing_wait_their_interval_and_nothing_is_requested() {
    let mut problems = Vec::new();
    for (shape, files) in [("agents, instructions and plugins empty", false), ("only plugins empty", true)] {
        let (w, remote, cfg, _, due) = checked_with_empty_lanes(&format!("lanes-empty-restart-{files}"), files);
        let (mut engine, clock) = w.engine_at(&remote, &FakeTokens::none());
        clock.store(T0 + 60, std::sync::atomic::Ordering::Relaxed);
        remote.reset();
        engine.startup(&cfg).unwrap();
        let wake = engine.next_wake(&cfg);
        let report = sync_which(&mut engine, &cfg, &Which::Due);
        if wake != due {
            problems.push(format!("{shape}: next_wake after restart is {wake:?}, expected the first engine's {due:?} (last check + interval)"));
        }
        if !report.repos.is_empty() || remote.calls() != Calls::default() {
            let lanes: Vec<_> = report.repos.iter().map(|r| &r.repo_id).collect();
            problems.push(format!("{shape}: a due check before the interval checked {lanes:?} with GitHub requests {:?}", remote.log()));
        }
    }
    assert!(problems.is_empty(), "empty lanes forgot their last check on restart:\n{}", problems.join("\n"));
}

#[test]
fn a_new_process_checks_an_unchanged_source_with_empty_lanes_on_one_conditional_request() {
    let mut problems = Vec::new();
    for (shape, files) in [("agents, instructions and plugins empty", false), ("only plugins empty", true)] {
        let (w, remote, cfg, source, _) = checked_with_empty_lanes(&format!("lanes-empty-process-{files}"), files);
        let (mut engine, clock) = w.engine_at(&remote, &FakeTokens::none());
        clock.store(T0 + 60, std::sync::atomic::Ordering::Relaxed);
        remote.reset();
        let report = sync_which(&mut engine, &cfg, &Which::One(source.id.clone()));
        let calls = remote.calls();
        // The fake answers 'not modified' exactly when the request carried the current ETag (`conditional`).
        if calls != (Calls { tree: 1, conditional: 1, ..Calls::default() }) {
            problems.push(format!("{shape}: expected one conditional listing answered 'not modified', got {calls:?}: {:?}", remote.log()));
        }
        for r in &report.repos {
            if !matches!(r.outcome, Outcome::Unchanged) || r.downloaded != 0 {
                problems.push(format!("{shape}: lane {} ended {:?} with {} bytes downloaded, expected Unchanged and 0", r.repo_id, r.outcome, r.downloaded));
            }
        }
    }
    assert!(problems.is_empty(), "a fresh engine could not send a conditional request for a source with empty lanes:\n{}", problems.join("\n"));
}

fn lane_report<'a>(report: &'a tiny_skill_sync::engine::SyncReport, id: &str) -> &'a RepoReport {
    report.repos.iter().find(|r| r.repo_id == id).unwrap_or_else(|| panic!("no lane {id}: {report:?}"))
}

fn count(dir: &Path, name: &str) -> usize {
    fs::read_dir(dir)
        .map_or(0, |entries| entries.flatten().map(|e| if e.path().is_dir() { count(&e.path(), name) } else { usize::from(e.file_name() == name) }).sum())
}

#[test]
fn turning_agents_off_and_on_holds_an_edit_made_meanwhile() {
    let w = World::new("lanes-agents-off-on");
    let remote = FakeRemote::new();
    kit(&remote);
    let mut cfg = Config::default();
    let source = add(&mut cfg, "", only_kinds(&[SourceKind::Skills, SourceKind::Agents]));
    let mut engine = w.engine(&remote);
    sync_all(&mut engine, &cfg);
    let agent = find(&w.root.join("agents"), AGENT).expect("the agent is installed");

    // Off: the user edits the agent and GitHub changes it; nothing runs for agents.
    cfg.edit(|sources, _| sources[0].types.agents = Some(false));
    fs::write(&agent, "mine").unwrap();
    remote.put(REPO, "agents/review.agent.md", "review v2");
    let report = sync_all(&mut engine, &cfg);
    assert_eq!(report.repos.len(), 1, "{report:?}");
    assert_eq!(fs::read_to_string(&agent).unwrap(), "mine");

    // On again: the full check finds the edit and holds the update for the user's choice.
    cfg.edit(|sources, _| sources[0].types.agents = Some(true));
    let report = sync_all(&mut engine, &cfg);
    let agents = lane_report(&report, &lane(&source, SourceKind::Agents));
    assert!(matches!(skill(agents, AGENT).verdict, Verdict::Held(Hold::UpdateBlocked(_))), "{agents:?}");
    assert!(matches!(agents.outcome, Outcome::NeedsAttention), "{agents:?}");
    assert_eq!(fs::read_to_string(&agent).unwrap(), "mine", "the user's bytes are kept");
    assert!(w.state().loose_sources()[&lane(&source, SourceKind::Agents)].leaf(AGENT).is_some(), "still owned");
}

#[test]
fn two_sources_listing_one_agent_install_it_once_and_keep_the_owner() {
    let w = World::new("lanes-two-owners");
    let remote = FakeRemote::new();
    kit(&remote);
    let mut cfg = Config::default();
    let first = add(&mut cfg, "", only_kinds(&[SourceKind::Skills, SourceKind::Agents]));
    let second = add(&mut cfg, "agents", only_kinds(&[SourceKind::Agents]));
    let (owner, other) = (lane(&first, SourceKind::Agents), lane(&second, SourceKind::Agents));
    let mut engine = w.engine(&remote);
    let report = sync_all(&mut engine, &cfg);
    assert_eq!(skill(lane_report(&report, &owner), AGENT).verdict, Verdict::Install, "{report:?}");
    assert_eq!(skill(lane_report(&report, &other), AGENT).verdict, Verdict::Skipped(Skip::Collision { owner: owner.clone() }), "{report:?}");
    assert_eq!(count(&w.root.join("agents"), AGENT), 1, "installed once");
    let agent = find(&w.root.join("agents"), AGENT).unwrap();

    // Changed upstream: the owner updates it, the other source still does not write it, and the skill still syncs.
    remote.put(REPO, "agents/review.agent.md", "review v2");
    remote.put(REPO, "skills/alpha/SKILL.md", "alpha v2");
    let report = sync_all(&mut engine, &cfg);
    assert_eq!(skill(lane_report(&report, &owner), AGENT).verdict, Verdict::Update, "{report:?}");
    assert_eq!(skill(lane_report(&report, &other), AGENT).verdict, Verdict::Skipped(Skip::Collision { owner: owner.clone() }), "{report:?}");
    assert_eq!(fs::read_to_string(&agent).unwrap(), "review v2");
    assert_eq!(count(&w.root.join("agents"), AGENT), 1);
    assert_eq!(w.read("alpha/SKILL.md"), "alpha v2", "unrelated items still sync");
    let state = w.state();
    assert!(state.loose_sources()[&owner].leaf(AGENT).is_some(), "the first owner keeps it");
    assert!(state.loose_sources().get(&other).is_none_or(|s| s.leaf(AGENT).is_none()), "the other source never owns it");
}

#[test]
fn a_location_preview_holds_a_file_destination_another_source_claims() {
    let w = World::new("lanes-preview-claim");
    let remote = FakeRemote::new();
    kit(&remote);
    remote.seed("o/other", &[("agents/review.agent.md", "theirs")]);
    remote.seed("o/third", &[("agents/fresh.agent.md", "new")]);
    let mut cfg = Config::default();
    let source = add(&mut cfg, "", only_kinds(&[SourceKind::Agents]));
    let mut engine = w.engine(&remote);
    sync_all(&mut engine, &cfg);
    assert!(find(&w.root.join("agents"), AGENT).is_some(), "fixture: the agent is installed and owned");

    let before = w.snapshot();
    let preview = engine.preview_location(&cfg, "o/other", false, &cancel_flag()).unwrap();
    let held = preview.destinations.iter().find(|(k, _)| *k == SourceKind::Agents).map(|(_, d)| d.clone());
    let claimed = format!("is already claimed by source '{}'", lane(&source, SourceKind::Agents));
    assert!(matches!(&held, Some(tiny_skill_sync::source::PreviewDestination::Held(why)) if why.contains(AGENT) && why.contains(&claimed)), "{held:?}");
    // A name nobody claims resolves to the same folder.
    let preview = engine.preview_location(&cfg, "o/third", false, &cancel_flag()).unwrap();
    let free = preview.destinations.iter().find(|(k, _)| *k == SourceKind::Agents).map(|(_, d)| d.clone());
    assert!(matches!(free, Some(tiny_skill_sync::source::PreviewDestination::Resolved(_))), "{free:?}");
    assert_eq!(w.snapshot(), before, "a preview writes nothing");
}

#[test]
fn an_empty_instructions_listing_in_a_mixed_source_holds_its_deletions() {
    let w = World::new("lanes-empty-instructions");
    let remote = FakeRemote::new();
    kit(&remote);
    let mut cfg = Config::default();
    let source = add(&mut cfg, "", only_kinds(&[SourceKind::Skills, SourceKind::Instructions]));
    let mut engine = w.engine(&remote);
    sync_all(&mut engine, &cfg);
    let rule = find(&w.root.join("instructions"), RULE).expect("the instruction is installed");
    remote.remove(REPO, "instructions");

    let report = sync_all(&mut engine, &cfg);
    let rules = lane_report(&report, &lane(&source, SourceKind::Instructions));
    assert_eq!(skill(rules, RULE).verdict, Verdict::Held(Hold::MassDeletion), "{report:?}");
    assert!(matches!(lane_report(&report, &source.id).outcome, Outcome::Synced | Outcome::Unchanged), "{report:?}");
    assert_eq!(fs::read_to_string(&rule).unwrap(), "style v1");
    assert_eq!(w.skill_names(), ["alpha"]);

    let report = sync_with(&mut engine, &cfg, SyncOptions { confirm_mass_delete: true, ..SyncOptions::default() });
    assert_eq!(skill(lane_report(&report, &lane(&source, SourceKind::Instructions)), RULE).verdict, Verdict::Delete, "{report:?}");
    assert!(!rule.exists());
    assert_eq!(w.skill_names(), ["alpha"], "the skill stays");
}
