mod common;

use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use common::{FakePluginManager, FakeRemote, FakeTokens, PluginCall as Call, PluginReply as Reply, World, config, only, repo};
use tiny_skill_sync::config::{Config, RepoConfig};
use tiny_skill_sync::engine::{ApplyResult, Engine, Outcome, PluginReport, RepoReport, SyncOptions, Which};
use tiny_skill_sync::github::{FailureKind, RemoteError};
use tiny_skill_sync::plugins::native::{ManagerError, MarketOrigin, NativeMarket, NativePlugin, NativeSnapshot};
use tiny_skill_sync::plugins::{Choice, Operation, Status};
use tiny_skill_sync::source::{SourceKind, SourcePreview};
use tiny_skill_sync::state::State;

const REPO: &str = "o/packages";
const ID: &str = "packages";
const MARKET: &str = "tools";
const MARKET_CONFLICT: &str = "Copilot already has a marketplace named tools that does not follow o/packages@test/v1.";
static RUN: AtomicBool = AtomicBool::new(false);

struct Fixture {
    world: World,
    remote: Arc<FakeRemote>,
    manager: FakePluginManager,
    engine: Engine,
    config: Config,
    clock: Arc<AtomicI64>,
}

impl Fixture {
    fn new(test: &str) -> Self {
        let world = World::new(test);
        let remote = FakeRemote::new();
        let manager = FakePluginManager::new(world.root.join("native"));
        let (engine, clock) = world.plugin_engine(&remote, &manager);
        let config = config(vec![RepoConfig { branch: "test/v1".into(), kind: SourceKind::Plugins, ..repo(ID, REPO, "pack") }]);
        let f = Self { world, remote, manager, engine, config, clock };
        f.market(&["alpha"]);
        f.manifest("alpha", "1", "Team");
        f
    }

    fn market(&self, names: &[&str]) {
        let mut plugins: Vec<_> = names.iter().map(|n| serde_json::json!({"name": n, "source": format!("./pack/{n}")})).collect();
        plugins.extend([
            serde_json::json!({"name":"elsewhere","source":{"url":"https://example.test/x"}}),
            serde_json::json!({"name":"outside","source":"packages/outside"}),
        ]);
        self.remote.put(REPO, "marketplace.json", &serde_json::json!({"name": MARKET, "plugins": plugins}).to_string());
    }

    fn manifest(&self, name: &str, version: &str, publisher: &str) {
        self.remote.put(
            REPO,
            &format!("pack/{name}/.plugin/plugin.json"),
            &serde_json::json!({
                "name": name, "version": version, "author":{"name":publisher}, "mcpServers":{}
            })
            .to_string(),
        );
    }

    fn native(&self, plugins: &[(&str, &str, bool)]) -> NativeSnapshot {
        NativeSnapshot {
            home: self.world.root.join("native"),
            markets: vec![NativeMarket {
                name: MARKET.into(),
                source: "GitHub: o/Packages".into(),
                is_default: false,
                origin: Some(MarketOrigin { kind: "github".into(), repo: Some(REPO.into()), reference: Some("test/v1".into()) }),
            }],
            plugins: plugins
                .iter()
                .map(|&(n, v, e)| NativePlugin { name: n.into(), marketplace: MARKET.into(), version: v.into(), enabled: e, source: "installed".into() })
                .collect(),
        }
    }

    fn conflict_market(&self, plugins: &[(&str, &str, bool)]) {
        let mut native = self.native(plugins);
        native.markets[0].origin.as_mut().unwrap().reference = Some("other-branch".into());
        self.manager.set_snapshot(native);
    }

    fn preview(&mut self) -> SourcePreview {
        self.engine.preview_source(&self.config, &self.config.repos[0], &RUN).expect("preview of a valid marketplace")
    }

    fn approve(&mut self) {
        let preview = self.preview();
        self.engine.approve_plugin_preview(&self.config, ID, &preview).expect("save local approval");
    }

    fn sync(&mut self) -> RepoReport {
        let sync = self.engine.sync(&self.config, &Which::All, &SyncOptions::default(), &RUN).unwrap();
        only(&sync).clone()
    }

    fn choose(&mut self, choice: Choice) -> RepoReport {
        self.engine.resolve_plugin(&self.config, ID, &choice, &RUN).expect("choice report")
    }

    fn install(&mut self) {
        self.approve();
        self.manager.set_snapshot(self.native(&[]));
        self.manager.push(Call::Install("alpha@tools".into()), Reply::Done(Some(self.native(&[("alpha", "1", true)]))));
        synced(&self.sync());
    }

    fn mutations(&self) -> Vec<Call> {
        self.manager.calls().into_iter().filter(|c| !matches!(c, Call::Home | Call::List | Call::Markets)).collect()
    }
}

fn row<'a>(report: &'a RepoReport, name: &str) -> &'a PluginReport {
    report.plugins.iter().find(|r| r.name == name).unwrap_or_else(|| panic!("missing {name}: {report:?}"))
}

fn synced(report: &RepoReport) {
    assert!(matches!(report.outcome, Outcome::Synced), "{report:?}");
}

fn failed(report: &RepoReport) -> &tiny_skill_sync::engine::Failure {
    match &report.outcome {
        Outcome::Failed(f) => f,
        unexpected => panic!("expected a failure, got {unexpected:?}"),
    }
}

#[test]
fn plugin_first_check_registers_then_installs_only_what_the_saved_preview_approved() {
    let mut f = Fixture::new("plugin-first");
    let review = f.preview();
    f.market(&["alpha", "beta"]);
    f.manifest("beta", "1", "Team");
    f.engine.approve_plugin_preview(&f.config, ID, &review).unwrap();
    assert!(f.mutations().is_empty());
    let add = Call::AddMarket { repo: REPO.into(), branch: "test/v1".into() };
    f.manager.push(add.clone(), Reply::Done(Some(f.native(&[]))));
    let install = Call::Install("alpha@tools".into());
    f.manager.push(install.clone(), Reply::Done(Some(f.native(&[("alpha", "1", true)]))));
    let report = f.sync();
    assert!(matches!(report.outcome, Outcome::NeedsAttention));
    assert_eq!(f.mutations(), [add, install]);
    let state = f.world.state();
    let record = &state.plugin_sources()[ID];
    assert!(record.registered_by_app && record.plugins["alpha"].by_app);
    assert_eq!(record.approved.keys().collect::<Vec<_>>(), &["alpha"]);
    let r = row(&report, "alpha");
    assert_eq!(r.result, ApplyResult::Done);
    assert_eq!(r.status, Status::Planned(Operation::Install));
    assert_eq!(r.version.as_deref(), Some("1"));
    assert_eq!(r.publisher.as_deref(), Some("Team"));
    assert_eq!(r.path.as_deref(), Some("pack/alpha"));
    assert_eq!(row(&report, "beta").choices, [Choice::Approve { name: "beta".into(), publisher: Some("Team".into()) }]);
}

#[test]
fn plugin_preview_fetches_only_root_marketplace_and_matched_manifests_and_is_read_only() {
    let mut f = Fixture::new("plugin-preview");
    f.remote.put(REPO, "pack/alpha/hooks.json", "not downloaded");
    f.remote.put(REPO, "packages/outside/plugin.json", "not downloaded");
    let files = f.world.snapshot();
    let preview = f.preview();
    let catalog = preview.plugins.as_ref().unwrap();
    assert_eq!(catalog.market(), MARKET);
    assert_eq!(preview.names, ["alpha"]);
    assert_eq!(catalog.hosted_elsewhere(), ["elsewhere"]);
    let plugin = &catalog.plugins()["alpha"];
    assert_eq!(plugin.publisher.as_deref(), Some("Team"));
    assert_eq!(plugin.version.as_deref(), Some("1"));
    assert!(plugin.capabilities.hooks && plugin.capabilities.mcp && !plugin.capabilities.lsp);
    assert_eq!(
        f.remote.log(),
        ["tree o/packages@test/v1:", "commit o/packages", "file o/packages marketplace.json", "file o/packages pack/alpha/.plugin/plugin.json"]
    );
    assert!(f.manager.calls().is_empty());
    assert_eq!(f.world.snapshot(), files);

    f.manifest("alpha", "1", "Team");
    f.market(&["alpha", "missing"]);
    let blocked = f.preview();
    assert_eq!(blocked.errors.len(), 1);
    assert!(blocked.errors[0].contains("missing"));
    assert!(f.engine.approve_plugin_preview(&f.config, ID, &blocked).is_err());
    assert!(f.manager.calls().is_empty());
    assert_eq!(f.world.snapshot(), files);
}

#[test]
fn plugin_update_waits_while_turned_off_and_then_verifies_the_new_version() {
    let mut f = Fixture::new("plugin-update");
    f.install();
    let before = f.world.state().plugin_sources()[ID].plugins["alpha"].fingerprint.clone();
    f.manifest("alpha", "2", "Team");
    f.manager.set_snapshot(f.native(&[("alpha", "1", false)]));
    let report = f.sync();
    assert_eq!(row(&report, "alpha").status, Status::TurnedOff);
    assert!(matches!(report.outcome, Outcome::Unchanged));
    assert_eq!(row(&report, "alpha").version.as_deref(), Some("1"));
    let state = f.world.state();
    assert_eq!(state.plugin_sources()[ID].plugins["alpha"].fingerprint, before);
    let poll = &state.plugin_sources()[ID].poll;
    assert!(poll.etag.is_some() && !poll.needs_attention, "a disabled plugin does not prevent a quiet poll");
    assert_eq!(f.mutations(), [Call::Install("alpha@tools".into())]);

    let update = Call::Update("alpha@tools".into());
    f.manager.set_snapshot(f.native(&[("alpha", "1", true)]));
    f.manager.push(update.clone(), Reply::Done(None));
    assert!(failed(&f.sync()).message.contains("did not confirm"));
    assert_eq!(f.world.state().plugin_sources()[ID].plugins["alpha"].fingerprint, before);
    assert_eq!(f.mutations(), [Call::Install("alpha@tools".into()), update.clone()]);
    f.manager.push(update, Reply::Done(Some(f.native(&[("alpha", "2", true)]))));
    synced(&f.sync());
    let state = f.world.state();
    let owned = &state.plugin_sources()[ID].plugins["alpha"];
    assert_ne!(owned.fingerprint, before);
    assert_eq!(owned.version.as_deref(), Some("2"));
}

#[test]
fn plugin_explicit_check_detects_native_removal_but_only_reinstall_restores_it() {
    let mut f = Fixture::new("plugin-reinstall");
    f.install();
    f.manager.set_snapshot(f.native(&[]));
    f.remote.reset();
    let report = f.sync();
    assert!(matches!(report.outcome, Outcome::Unchanged));
    assert_eq!(f.remote.calls().conditional, 0, "an explicit check asks for the full listing");
    assert_eq!(row(&report, "alpha").status, Status::RemovedInCopilot);
    let choice = Choice::Reinstall("alpha".into());
    assert_eq!(row(&report, "alpha").choices, std::slice::from_ref(&choice));
    assert_eq!(f.mutations(), [Call::Install("alpha@tools".into())]);
    assert!(f.world.state().plugin_sources()[ID].plugins.contains_key("alpha"));
    f.manager.push(Call::Install("alpha@tools".into()), Reply::Done(Some(f.native(&[("alpha", "1", true)]))));
    synced(&f.choose(choice));
    assert_eq!(f.mutations().len(), 2);
}

#[test]
fn plugin_removed_from_github_is_held_even_if_the_marketplace_is_empty_and_uninstall_must_be_verified() {
    let mut f = Fixture::new("plugin-uninstall");
    f.install();
    f.market(&[]);
    let report = f.sync();
    assert!(matches!(report.outcome, Outcome::NeedsAttention));
    let r = row(&report, "alpha");
    assert_eq!(r.status, Status::RemovedFromGitHub);
    let choice = Choice::Uninstall("alpha".into());
    assert_eq!(r.choices, [choice.clone(), Choice::Forget("alpha".into())]);
    assert_eq!(f.mutations().len(), 1);
    let uninstall = Call::Uninstall("alpha@tools".into());
    f.manager.push(uninstall.clone(), Reply::Done(None));
    assert!(failed(&f.choose(choice.clone())).message.contains("did not confirm"));
    assert!(f.world.state().plugin_sources()[ID].plugins.contains_key("alpha"));
    f.manager.push(uninstall.clone(), Reply::Done(Some(f.native(&[]))));
    synced(&f.choose(choice));
    assert_eq!(f.mutations(), [Call::Install("alpha@tools".into()), uninstall.clone(), uninstall]);
    assert!(f.world.state().plugin_sources()[ID].plugins.is_empty());
}

#[test]
fn plugin_lifecycle_leaves_no_copies_in_the_skills_agents_or_instructions_folders() {
    let mut f = Fixture::new("plugin-no-copies");
    let (agents, instructions) = (f.world.root.join("agents"), f.world.root.join("instructions"));
    for path in [f.world.skills.join("mine").join("SKILL.md"), agents.join("mine.agent.md"), instructions.join("mine.instructions.md")] {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "written by hand\n").unwrap();
    }
    let outside_copilot = |f: &Fixture| {
        let mut files = f.world.snapshot();
        files.retain(|k, _| !["home", "native"].iter().any(|d| k == d || k.starts_with(&format!("{d}/"))));
        files
    };
    let before = outside_copilot(&f);
    f.install();
    f.manifest("alpha", "2", "Team");
    f.manager.set_snapshot(f.native(&[("alpha", "1", true)]));
    f.manager.push(Call::Update("alpha@tools".into()), Reply::Done(Some(f.native(&[("alpha", "2", true)]))));
    synced(&f.sync());
    f.market(&[]);
    assert!(matches!(f.sync().outcome, Outcome::NeedsAttention));
    f.manager.push(Call::Uninstall("alpha@tools".into()), Reply::Done(Some(f.native(&[]))));
    synced(&f.choose(Choice::Uninstall("alpha".into())));
    assert_eq!(f.mutations().len(), 3, "install, update and uninstall all ran");
    assert_eq!(outside_copilot(&f), before);
}

#[test]
fn plugin_partial_install_records_only_verified_plugins_and_does_not_retry_in_this_check() {
    let mut f = Fixture::new("plugin-partial");
    f.market(&["alpha", "beta"]);
    f.manifest("beta", "2", "Other");
    f.approve();
    f.manager.set_snapshot(f.native(&[]));
    let alpha = Call::Install("alpha@tools".into());
    let beta = Call::Install("beta@tools".into());
    f.manager.push(alpha.clone(), Reply::Done(None));
    f.manager.push(beta.clone(), Reply::Done(Some(f.native(&[("beta", "2", true)]))));
    let report = f.sync();
    assert!(failed(&report).message.contains("did not confirm"));
    assert!(matches!(row(&report, "alpha").result, ApplyResult::Failed(_)));
    assert_eq!(row(&report, "beta").result, ApplyResult::Done);
    assert_eq!(f.mutations(), [alpha, beta]);
    let state = f.world.state();
    let record = &state.plugin_sources()[ID];
    assert_eq!(record.plugins.keys().collect::<Vec<_>>(), &["beta"]);
    assert!(record.poll.etag.is_none());
}

#[test]
fn plugin_origin_is_rechecked_before_a_mutation_and_wrong_registration_is_not_recorded() {
    let mut f = Fixture::new("plugin-origin");
    f.approve();
    let mut wrong = f.native(&[]);
    wrong.markets[0].origin.as_mut().unwrap().reference = Some("other".into());
    let add = Call::AddMarket { repo: REPO.into(), branch: "test/v1".into() };
    f.manager.push(add.clone(), Reply::Done(Some(wrong.clone())));
    assert!(failed(&f.sync()).message.contains("did not confirm"));
    assert!(!f.world.state().plugin_sources()[ID].registered_by_app);
    assert_eq!(f.mutations(), [add]);
}

#[test]
fn plugin_publisher_approval_is_local_exact_and_only_affects_the_chosen_plugin() {
    let mut f = Fixture::new("plugin-approval");
    f.market(&["alpha", "beta"]);
    f.manifest("beta", "1", "Team");
    f.manager.set_snapshot(f.native(&[]));
    let report = f.sync();
    assert_eq!(row(&report, "alpha").status, Status::NeedsApproval);
    let choice = row(&report, "alpha").choices[0].clone();
    assert!(f.mutations().is_empty());
    f.manifest("alpha", "1", "Different");
    assert!(matches!(f.choose(choice).outcome, Outcome::Failed(_)));
    assert!(f.mutations().is_empty());
    let choice = Choice::Approve { name: "alpha".into(), publisher: Some("Different".into()) };
    f.manager.push(Call::Install("alpha@tools".into()), Reply::Done(Some(f.native(&[("alpha", "1", true)]))));
    let report = f.choose(choice);
    assert_eq!(row(&report, "alpha").result, ApplyResult::Done);
    assert_eq!(row(&report, "beta").status, Status::NeedsApproval);
    assert_eq!(f.mutations(), [Call::Install("alpha@tools".into())]);

    f.manifest("alpha", "2", "New team");
    let report = f.sync();
    assert_eq!(row(&report, "alpha").status, Status::NeedsApproval);
    assert_eq!(f.world.state().plugin_sources()[ID].approved["alpha"].as_deref(), Some("Different"));
    assert_eq!(f.mutations().len(), 1);
}

#[test]
fn plugin_preview_dry_run_and_non_plugin_checks_never_call_the_manager() {
    let mut f = Fixture::new("plugin-no-launch");
    let files = f.world.snapshot();
    let _ = f.preview();
    let options = SyncOptions { dry_run: true, ..Default::default() };
    let report = f.engine.sync(&f.config, &Which::All, &options, &RUN).unwrap();
    assert_eq!(row(only(&report), "alpha").status, Status::Blocked);
    assert!(f.manager.calls().is_empty());
    assert_eq!(f.world.snapshot(), files);

    let cfg = common::standard_config();
    f.remote.seed_standard("o/fixtures");
    synced(only(&f.engine.sync(&cfg, &Which::All, &SyncOptions::default(), &RUN).unwrap()));
    assert!(f.manager.calls().is_empty());
}

#[test]
fn plugin_timer_304_is_one_root_request_with_no_manager_calls_or_state_write() {
    let mut f = Fixture::new("plugin-304");
    f.install();
    let count = f.manager.calls().len();
    let files = f.world.snapshot();
    let stamp = fs::metadata(f.world.state_file()).unwrap().modified().unwrap();
    f.remote.reset();
    f.clock.fetch_add(3 * 24 * 3600, Ordering::Relaxed);
    let report = f.engine.sync(&f.config, &Which::Due, &SyncOptions::default(), &RUN).unwrap();
    assert!(matches!(only(&report).outcome, Outcome::Unchanged));
    assert_eq!(f.remote.log(), ["tree o/packages@test/v1:"]);
    assert_eq!(f.remote.calls().conditional, 1);
    assert_eq!(f.manager.calls().len(), count);
    assert_eq!(f.world.snapshot(), files);
    assert_eq!(fs::metadata(f.world.state_file()).unwrap().modified().unwrap(), stamp);
}

#[test]
fn plugin_failed_truncated_cancelled_or_bad_download_never_reaches_native_planning() {
    let mut f = Fixture::new("plugin-incomplete");
    f.install();
    let count = f.manager.calls().len();
    let files = f.world.snapshot();
    f.market(&[]);
    f.remote.fail_tree(REPO, Some(RemoteError::new(FailureKind::Server, "test listing error")));
    assert_eq!(failed(&f.sync()).kind, FailureKind::Server);
    f.remote.fail_tree(REPO, None);
    f.remote.set_truncated(REPO, true);
    let choice = Choice::Uninstall("alpha".into());
    assert_eq!(failed(&f.choose(choice)).kind, FailureKind::TooLarge);
    f.remote.set_truncated(REPO, false);
    let cancelled = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&cancelled);
    f.remote.on_call(move |_| signal.store(true, Ordering::Relaxed));
    let report = f.engine.sync(&f.config, &Which::All, &SyncOptions::default(), &cancelled).unwrap();
    assert_eq!(failed(only(&report)).kind, FailureKind::Cancelled);
    f.remote.clear_hook();
    f.market(&["alpha"]);
    f.manifest("alpha", "2", "Team");
    f.remote.lie_about(REPO, "pack/alpha/.plugin/plugin.json", r#"{"name":"beta","version":"2","author":{"name":"Team"},"mcpServers":{}}"#);
    assert_eq!(failed(&f.sync()).kind, FailureKind::Changed);
    assert_eq!(f.manager.calls().len(), count);
    assert_eq!(f.world.snapshot(), files);
}

#[test]
fn plugin_remove_keeps_installs_by_default_and_explicit_uninstall_persists_partial_success() {
    let mut f = Fixture::new("plugin-remove");
    f.market(&["alpha", "beta"]);
    f.manifest("beta", "1", "Team");
    f.approve();
    f.manager.set_snapshot(f.native(&[("alpha", "1", true), ("beta", "1", true)]));
    synced(&f.sync()); // adopts both without taking credit for their install
    let alpha = Call::Uninstall("alpha@tools".into());
    let beta = Call::Uninstall("beta@tools".into());
    f.manager.push(alpha.clone(), Reply::Failed(ManagerError::Failed("Test failure".into())));
    f.manager.push(beta.clone(), Reply::Done(Some(f.native(&[("alpha", "1", true)]))));
    assert!(f.engine.remove_source_with_plugins(&f.config, ID, &RUN).unwrap_err().to_string().contains("Test failure"));
    assert_eq!(f.mutations(), [alpha, beta]);
    let state = f.world.state();
    assert_eq!(state.plugin_sources()[ID].plugins.keys().collect::<Vec<_>>(), &["alpha"]);

    let count = f.manager.calls().len();
    let report = f.engine.remove_source(&f.config, ID, false).unwrap();
    assert!(report.message.unwrap().contains("stay installed"));
    assert_eq!(report.kept[0].0, "alpha");
    assert_eq!(f.manager.calls().len(), count);
    assert!(f.world.state().plugin_sources().is_empty());
}

#[test]
fn plugin_second_source_does_not_claim_the_same_native_plugin() {
    let mut f = Fixture::new("plugin-overlap");
    f.install();
    f.config.repos.push(RepoConfig { id: "second".into(), path: "pack/alpha".into(), ..f.config.repos[0].clone() });
    let preview = f.engine.preview_source(&f.config, &f.config.repos[1], &RUN).unwrap();
    f.engine.approve_plugin_preview(&f.config, "second", &preview).unwrap();
    let report = f.engine.sync(&f.config, &Which::One("second".into()), &SyncOptions::default(), &RUN).unwrap();
    assert_eq!(row(only(&report), "alpha").status, Status::AlreadyTracked(ID.into()));
    assert_eq!(f.mutations().len(), 1);
    assert!(f.world.state().plugin_sources()["second"].plugins.is_empty());
}

#[test]
fn plugin_changed_source_says_to_remove_and_add_it_again() {
    let mut f = Fixture::new("plugin-home");
    f.install();
    let mut expected = f.world.state();
    let mut record = expected.plugin_sources()[ID].clone();
    record.poll.etag = None;
    record.poll.lists_nothing = true;
    record.poll.needs_token = true;
    record.poll.needs_attention = true;
    expected.set_plugin_source(ID, record).unwrap();

    f.market(&[]);
    f.remote.make_private(REPO, "fixture-value");
    let mut engine = f.world.engine_with(&f.remote, &FakeTokens::some("fixture-value")).with_plugin_manager(f.manager.manager());
    let mut changed = f.native(&[]);
    changed.home = f.world.root.join("new-home");
    f.manager.set_snapshot(changed);
    let message = "This source no longer matches what was approved on this PC. Remove it and add it again to review its plugins.";
    let report = engine.sync(&f.config, &Which::All, &SyncOptions::default(), &RUN).unwrap();
    assert_eq!(failed(only(&report)).message, message);
    assert_eq!(f.world.state(), expected, "only the poll metadata may change");

    let before = f.world.snapshot();
    assert_eq!(engine.remove_source_with_plugins(&f.config, ID, &RUN).unwrap_err().to_string(), message);
    assert_eq!(f.mutations().len(), 1);
    assert_eq!(f.world.snapshot(), before);
}

#[test]
fn plugin_approval_never_replaces_owned_records_for_a_changed_identity() {
    for changed in ["origin", "market", "home"] {
        let mut f = Fixture::new(&format!("plugin-approval-{changed}"));
        f.install();
        let before = f.world.snapshot();
        match changed {
            "origin" => f.config.repos[0].branch = "different".into(),
            "market" => f.remote.put(REPO, "marketplace.json", r#"{"name":"new-tools","plugins":[{"name":"alpha","source":"pack/alpha"}]}"#),
            _ => {
                let mut native = f.native(&[]);
                native.home = f.world.root.join("new-home");
                f.manager.set_snapshot(native);
            }
        }
        let review = f.preview();
        let err = f.engine.approve_plugin_preview(&f.config, ID, &review).unwrap_err().to_string();
        assert!(err.contains("Remove it and add it again"), "{err}");
        assert_eq!(f.world.snapshot(), before, "{changed} must not release ownership");
        assert_eq!(f.mutations(), [Call::Install("alpha@tools".into())]);
    }
}

#[test]
fn plugin_forget_only_releases_the_removed_plugins_ownership() {
    let mut f = Fixture::new("plugin-forget");
    f.install();
    f.market(&[]);
    let report = f.sync();
    assert_eq!(row(&report, "alpha").status, Status::RemovedFromGitHub);
    let result = f.choose(Choice::Forget("alpha".into()));
    synced(&result);
    assert_eq!(row(&result, "alpha").result, ApplyResult::Done);
    let state = f.world.state();
    let record = &state.plugin_sources()[ID];
    assert!(record.plugins.is_empty() && record.approved.is_empty());
    assert_eq!(f.mutations(), [Call::Install("alpha@tools".into())]);
}

#[test]
fn plugin_registration_does_not_turn_an_explicit_choice_into_a_full_check() {
    let mut f = Fixture::new("plugin-choice-register");
    f.approve(); // alpha is already approved, but not installed
    f.market(&["alpha", "beta"]);
    f.manifest("beta", "1", "Team");
    let add = Call::AddMarket { repo: REPO.into(), branch: "test/v1".into() };
    f.manager.push(add.clone(), Reply::Done(Some(f.native(&[]))));
    let install = Call::Install("beta@tools".into());
    f.manager.push(install.clone(), Reply::Done(Some(f.native(&[("beta", "1", true)]))));
    let report = f.choose(Choice::Approve { name: "beta".into(), publisher: Some("Team".into()) });
    assert_eq!(row(&report, "alpha").result, ApplyResult::NotTried);
    assert_eq!(row(&report, "beta").result, ApplyResult::Done);
    assert_eq!(f.mutations(), [add, install]);
}

#[test]
fn plugin_attention_poll_is_quiet_and_a_new_engine_rebuilds_rows() {
    let mut f = Fixture::new("plugin-attention-304");
    let report = f.sync();
    assert_eq!(row(&report, "alpha").status, Status::NeedsApproval);
    assert!(matches!(report.outcome, Outcome::NeedsAttention));
    let state = f.world.state();
    let poll = &state.plugin_sources()[ID].poll;
    assert!(poll.etag.is_some() && poll.needs_attention);

    let count = f.manager.calls().len();
    let files = f.world.snapshot();
    let stamp = fs::metadata(f.world.state_file()).unwrap().modified().unwrap();
    f.remote.reset();
    let at = f.clock.fetch_add(3 * 24 * 3600, Ordering::Relaxed) + 3 * 24 * 3600;
    let report = f.engine.sync(&f.config, &Which::Due, &SyncOptions::default(), &RUN).unwrap();
    assert!(matches!(only(&report).outcome, Outcome::NeedsAttention));
    assert!(only(&report).plugins.is_empty(), "the board keeps its existing rows");
    assert_eq!(f.remote.log(), ["tree o/packages@test/v1:"]);
    assert_eq!(f.remote.calls().conditional, 1);
    assert_eq!(f.manager.calls().len(), count);
    assert_eq!(f.world.snapshot(), files);
    assert_eq!(fs::metadata(f.world.state_file()).unwrap().modified().unwrap(), stamp);

    let (mut restarted, clock) = f.world.plugin_engine(&f.remote, &f.manager);
    clock.store(at, Ordering::Relaxed);
    let report = restarted.sync(&f.config, &Which::Due, &SyncOptions::default(), &RUN).unwrap();
    assert!(matches!(only(&report).outcome, Outcome::NeedsAttention));
    assert_eq!(row(only(&report), "alpha").choices, [Choice::Approve { name: "alpha".into(), publisher: Some("Team".into()) }]);
    assert_eq!(&f.manager.calls()[count..], &[Call::Home, Call::List, Call::Markets]);
}

#[test]
fn plugin_blocked_choice_cannot_hide_behind_a_timer_304() {
    let mut f = Fixture::new("plugin-choice-blocked-304");
    let choice = row(&f.sync(), "alpha").choices[0].clone();
    let state = f.world.state();
    let poll = &state.plugin_sources()[ID].poll;
    assert!(poll.etag.is_some() && poll.needs_attention);

    f.conflict_market(&[]);
    f.clock.fetch_add(3 * 24 * 3600, Ordering::Relaxed);
    let blocked = f.choose(choice);
    assert_eq!(failed(&blocked).message, MARKET_CONFLICT);
    assert_eq!(row(&blocked, "alpha").status, Status::Blocked);
    assert!(row(&blocked, "alpha").choices.is_empty());
    let saved = f.world.state();
    let saved = &saved.plugin_sources()[ID].poll;

    // Only Copilot changed. GitHub would still return 304 for the saved ETag.
    f.remote.reset();
    let timer = f.engine.sync(&f.config, &Which::Due, &SyncOptions::default(), &RUN).unwrap();
    assert_eq!(failed(only(&timer)).message, MARKET_CONFLICT);
    assert_eq!(row(only(&timer), "alpha").status, Status::Blocked);
    assert_eq!(f.remote.calls().conditional, 0);
    assert!(saved.etag.is_none() && saved.needs_attention);
    assert_eq!(saved.last_check, poll.last_check);
    assert!(f.mutations().is_empty());
}

#[test]
fn plugin_stale_choice_requires_a_full_timer_check() {
    let mut f = Fixture::new("plugin-choice-stale-304");
    f.install();
    f.manager.set_snapshot(f.native(&[]));
    let choice = row(&f.sync(), "alpha").choices[0].clone();
    assert_eq!(choice, Choice::Reinstall("alpha".into()));
    let before = f.world.state();
    let before = &before.plugin_sources()[ID].poll;
    assert!(before.etag.is_some() && !before.needs_attention);

    // The user reinstalled in Copilot while the choice was open; the remote listing did not change.
    f.manager.set_snapshot(f.native(&[("alpha", "1", true)]));
    f.clock.fetch_add(3 * 24 * 3600, Ordering::Relaxed);
    let stale = f.choose(choice);
    assert_eq!(failed(&stale).message, "That plugin choice no longer applies. Check the source again.");
    assert_eq!(row(&stale, "alpha").status, Status::UpToDate);
    let saved = f.world.state();
    let saved = &saved.plugin_sources()[ID].poll;

    f.remote.reset();
    let timer = f.engine.sync(&f.config, &Which::Due, &SyncOptions::default(), &RUN).unwrap();
    assert_eq!(f.remote.calls().conditional, 0, "a stale choice must force a full check");
    assert_eq!(row(only(&timer), "alpha").status, Status::UpToDate);
    assert!(saved.etag.is_none() && saved.needs_attention);
    assert_eq!(saved.last_check, before.last_check);
    assert_eq!(f.mutations(), [Call::Install("alpha@tools".into())]);
}

#[test]
fn plugin_blocked_timer_check_is_not_unchanged_after_restart() {
    let mut f = Fixture::new("plugin-timer-blocked-restart");
    f.install();
    let initial = f.world.state();
    let initial = &initial.plugin_sources()[ID].poll;
    assert!(initial.etag.is_some() && !initial.needs_attention);
    f.conflict_market(&[("alpha", "1", true)]);

    // Give this timer check a full listing without changing GitHub's ETag for the next run.
    let elapsed = 3 * 24 * 3600;
    let at = f.clock.fetch_add(elapsed, Ordering::Relaxed) + elapsed;
    let options = SyncOptions { ignore_etag: true, ..Default::default() };
    let blocked = f.engine.sync(&f.config, &Which::Due, &options, &RUN).unwrap();
    assert_eq!(failed(only(&blocked)).message, MARKET_CONFLICT);
    let saved = f.world.state();
    let saved = &saved.plugin_sources()[ID].poll;

    f.remote.reset();
    let (mut restarted, clock) = f.world.plugin_engine(&f.remote, &f.manager);
    clock.store(at, Ordering::Relaxed);
    let timer = restarted.sync(&f.config, &Which::Due, &SyncOptions::default(), &RUN).unwrap();
    assert_eq!(failed(only(&timer)).message, MARKET_CONFLICT);
    assert_eq!(row(only(&timer), "alpha").status, Status::Blocked);
    assert_eq!(f.remote.calls().conditional, 0);
    assert!(saved.etag.is_none() && saved.needs_attention);
    assert_eq!(saved.last_check, initial.last_check);
}

#[test]
fn plugin_already_installed_is_informational_and_never_taken_over() {
    let mut f = Fixture::new("plugin-other-install");
    f.approve();
    f.manager.set_snapshot(f.native(&[("alpha", "other version", true)]));
    let report = f.sync();
    assert_eq!(row(&report, "alpha").status, Status::AlreadyInstalled);
    assert!(matches!(report.outcome, Outcome::Unchanged));
    assert!(f.mutations().is_empty());
    assert!(f.world.state().plugin_sources()[ID].plugins.is_empty());
}

#[test]
fn plugin_actions_reuse_the_latest_snapshot() {
    let mut f = Fixture::new("plugin-probes");
    f.market(&["alpha", "beta"]);
    f.manifest("beta", "1", "Team");
    f.approve();
    f.manager.set_snapshot(f.native(&[("alpha", "1", true)]));
    let start = f.manager.calls().len();
    let beta = Call::Install("beta@tools".into());
    f.manager.push(beta.clone(), Reply::Done(Some(f.native(&[("alpha", "1", true), ("beta", "1", true)]))));
    synced(&f.sync());
    assert_eq!(&f.manager.calls()[start..], &[Call::Home, Call::List, Call::Markets, beta, Call::Home, Call::List, Call::Markets]);
    assert!(!f.world.state().plugin_sources()[ID].plugins["alpha"].by_app);

    f.manager.set_snapshot(f.native(&[("alpha", "1", false), ("beta", "1", true)]));
    f.manifest("alpha", "1", "New Team");
    f.market(&["alpha"]);
    let start = f.manager.calls().len();
    f.choose(Choice::Approve { name: "alpha".into(), publisher: Some("New Team".into()) });
    assert_eq!(&f.manager.calls()[start..], &[Call::Home, Call::List, Call::Markets]);
    let start = f.manager.calls().len();
    f.choose(Choice::Forget("beta".into()));
    assert_eq!(&f.manager.calls()[start..], &[Call::Home, Call::List, Call::Markets]);
    assert_eq!(f.world.state().plugin_sources()[ID].approved["alpha"].as_deref(), Some("New Team"));
    assert!(!f.world.state().plugin_sources()[ID].plugins.contains_key("beta"));
}

#[test]
fn plugin_ready_and_verified_guard_consecutive_commands() {
    let mut f = Fixture::new("plugin-ready-verified");
    f.market(&["alpha", "beta"]);
    f.manifest("beta", "1", "Team");
    f.approve();
    f.manager.set_snapshot(f.native(&[]));
    // Copilot installed the wrong version of alpha, and beta appeared after planning. Record neither.
    let alpha = Call::Install("alpha@tools".into());
    let beta = Call::Install("beta@tools".into());
    f.manager.push(alpha.clone(), Reply::Done(Some(f.native(&[("alpha", "wrong version", true), ("beta", "1", true)]))));
    f.manager.push(beta, Reply::Done(None));
    let report = f.sync();
    assert!(failed(&report).message.contains("did not confirm"));
    assert!(matches!(&row(&report, "beta").result, ApplyResult::Failed(reason) if reason.contains("changed in Copilot")));
    assert_eq!(f.mutations(), [alpha]);
    assert!(f.world.state().plugin_sources()[ID].plugins.is_empty());
}

#[test]
fn plugin_timeout_leaves_beta_for_next_check_and_other_errors_allow_it() {
    let mut f = Fixture::new("plugin-timeout");
    f.market(&["alpha", "beta"]);
    f.manifest("beta", "1", "Team");
    f.approve();
    f.manager.set_snapshot(f.native(&[]));
    let alpha = Call::Install("alpha@tools".into());
    let beta = Call::Install("beta@tools".into());
    f.manager.push(alpha.clone(), Reply::Failed(ManagerError::TimedOut));
    // If the check incorrectly continues, beta would succeed immediately.
    f.manager.push(beta.clone(), Reply::Done(Some(f.native(&[("beta", "1", true)]))));
    let report = f.sync();
    assert!(failed(&report).message.contains("took too long"));
    assert!(matches!(&row(&report, "alpha").result, ApplyResult::Failed(reason) if reason.contains("took too long")));
    assert_eq!(row(&report, "beta").result, ApplyResult::NotTried);
    assert_eq!(f.mutations(), std::slice::from_ref(&alpha));
    assert!(f.world.state().plugin_sources()[ID].plugins.is_empty());
    assert!(f.world.state().plugin_sources()[ID].poll.etag.is_none());

    // List order is by name. A non-timeout failure for alpha still allows beta to finish.
    f.manager.push_first(alpha.clone(), Reply::Failed(ManagerError::Failed("Test failure".into())));
    let report = f.sync();
    assert_eq!(failed(&report).message, "Test failure");
    assert_eq!(row(&report, "alpha").result, ApplyResult::Failed("Test failure".into()));
    assert_eq!(row(&report, "beta").result, ApplyResult::Done);
    assert_eq!(f.mutations(), [alpha.clone(), alpha, beta]);
    let state = f.world.state();
    assert!(state.plugin_sources()[ID].plugins.contains_key("beta"));
    let poll = &state.plugin_sources()[ID].poll;
    assert!(poll.etag.is_none() && poll.needs_attention);
}

#[test]
fn plugin_late_cancel_still_verifies_and_saves_the_successful_install() {
    let mut f = Fixture::new("plugin-late-cancel");
    f.approve();
    f.manager.set_snapshot(f.native(&[]));
    let cancel = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&cancel);
    let alpha = Call::Install("alpha@tools".into());
    f.manager.push(alpha.clone(), Reply::Done(Some(f.native(&[("alpha", "1", true)]))));
    f.manager.after_call(move |call| {
        if *call == alpha {
            signal.store(true, Ordering::Relaxed);
        }
    });
    let sync = f.engine.sync(&f.config, &Which::All, &SyncOptions::default(), &cancel).unwrap();
    assert!(cancel.load(Ordering::Relaxed));
    assert_eq!(row(only(&sync), "alpha").result, ApplyResult::Done);
    assert!(f.world.state().plugin_sources()[ID].plugins["alpha"].by_app);
}

#[test]
fn plugin_verified_action_is_on_disk_before_the_next_command() {
    let mut f = Fixture::new("plugin-save-as-you-go");
    f.market(&["alpha", "beta"]);
    f.manifest("beta", "1", "Team");
    f.approve();
    f.manager.set_snapshot(f.native(&[]));
    let path = f.world.state_file();
    f.manager.after_call(move |call| {
        if matches!(call, Call::Install(name) if name == "beta@tools") {
            let state = State::load_sources_with(&path, false).unwrap().state;
            assert!(state.plugin_sources()[ID].plugins["alpha"].by_app, "the first verified install is durable already");
            assert!(state.plugin_sources()[ID].poll.etag.is_none(), "a crash must not hide the rest of the check behind a 304");
        }
    });
    f.manager.push(Call::Install("alpha@tools".into()), Reply::Done(Some(f.native(&[("alpha", "1", true)]))));
    f.manager.push(Call::Install("beta@tools".into()), Reply::Done(Some(f.native(&[("alpha", "1", true), ("beta", "1", true)]))));
    synced(&f.sync());
}

#[test]
fn plugin_failed_verification_leaves_the_install_for_the_next_check_to_adopt() {
    let mut f = Fixture::new("plugin-verify-stopped");
    f.approve();
    f.manager.set_snapshot(f.native(&[]));
    let install = Call::Install("alpha@tools".into());
    f.manager.push(install.clone(), Reply::Done(Some(f.native(&[("alpha", "1", true)]))));
    f.manager.push(Call::List, Reply::Failed(ManagerError::Cancelled));
    let report = f.sync();
    assert_eq!(failed(&report).kind, FailureKind::Cancelled);
    assert!(matches!(row(&report, "alpha").result, ApplyResult::Failed(_)));
    assert!(f.world.state().plugin_sources()[ID].plugins.is_empty());
    assert!(f.world.state().plugin_sources()[ID].poll.etag.is_none());
    assert_eq!(row(&f.sync(), "alpha").status, Status::Planned(Operation::Adopt));
    assert!(!f.world.state().plugin_sources()[ID].plugins["alpha"].by_app);
    assert_eq!(f.mutations(), [install]);
}

#[test]
fn plugin_probe_failure_before_planning_changes_nothing() {
    for probe in [Call::List, Call::Markets] {
        let mut f = Fixture::new("plugin-probe-failure");
        f.install();
        f.manifest("alpha", "2", "Team");
        let calls = f.mutations();
        let files = f.world.snapshot();
        f.manager.push(probe, Reply::Failed(ManagerError::BadOutput));
        assert!(failed(&f.sync()).message.contains("could not be verified"));
        assert_eq!(f.mutations(), calls);
        assert_eq!(f.world.snapshot(), files);
    }
}

#[test]
fn plugin_conflicting_marketplace_is_reported_without_mutations() {
    for different_repo in [false, true] {
        let mut f = Fixture::new("plugin-market-conflict");
        f.approve();
        let mut conflict = f.native(&[]);
        let market = &mut conflict.markets[0];
        if different_repo {
            market.source = "GitHub: other/repo".into();
            market.origin.as_mut().unwrap().repo = Some("other/repo".into());
        } else {
            market.origin.as_mut().unwrap().reference = Some("other-branch".into());
        }
        f.manager.set_snapshot(conflict);
        assert_eq!(failed(&f.sync()).message, MARKET_CONFLICT);
        assert!(f.mutations().is_empty());
        let state = f.world.state();
        assert!(!state.plugin_sources()[ID].registered_by_app);
        assert!(state.plugin_sources()[ID].plugins.is_empty());
    }
}

#[test]
fn plugin_blocked_first_check_does_not_bind_or_save_a_source() {
    let mut f = Fixture::new("plugin-first-blocked");
    f.conflict_market(&[]);
    let path = f.world.state_file();
    State::default().save(&path).unwrap();
    let bytes = fs::read(&path).unwrap();
    let stamp = fs::metadata(&path).unwrap().modified().unwrap();

    let report = f.sync();
    assert_eq!(failed(&report).message, MARKET_CONFLICT);
    assert_eq!(row(&report, "alpha").status, Status::Blocked);
    assert!(f.mutations().is_empty());
    assert!(!f.world.state().plugin_sources().contains_key(ID), "a blocked source must not claim the marketplace");
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), stamp);
}

#[test]
fn plugin_removal_keeps_records_that_cannot_be_verified_for_uninstall() {
    for missing_market in [false, true] {
        let mut f = Fixture::new("plugin-remove-held");
        f.install();
        f.config.save(&f.world.home.join(tiny_skill_sync::config::CONFIG_FILE)).unwrap();
        let before = f.world.snapshot();
        let mut native = f.native(&[("alpha", "1", true)]);
        if missing_market {
            native.markets.clear();
        } else {
            native.plugins[0].source = "live".into();
        }
        f.manager.set_snapshot(native);
        let why = f.engine.remove_source_with_plugins(&f.config, ID, &RUN).unwrap_err().to_string();
        assert!(why.contains("could not be verified"), "{why}");
        assert_eq!(f.mutations(), [Call::Install("alpha@tools".into())]);
        assert!(f.world.state().plugin_sources()[ID].plugins.contains_key("alpha"));
        assert_eq!(f.world.snapshot(), before);
    }
}

#[test]
fn plugin_preview_refuses_an_id_used_by_a_skill_record() {
    let mut f = Fixture::new("plugin-preview-id");
    let mut state = State::default();
    state.repo_mut(ID).last_check = Some(common::T0);
    state.save(&f.world.state_file()).unwrap();
    let before = f.world.snapshot();
    let message = f.engine.preview_source(&f.config, &f.config.repos[0], &RUN).unwrap_err().to_string();
    assert!(message.contains(ID) && message.contains("already used"), "{message}");
    assert!(f.manager.calls().is_empty());
    assert!(f.remote.log().is_empty());
    assert_eq!(f.world.snapshot(), before);
}
