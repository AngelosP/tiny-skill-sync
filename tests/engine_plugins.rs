mod common;

use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use common::{FakePluginManager, FakeRemote, FakeTokens, PluginCall as Call, PluginReply as Reply, World, config, only, repo};
use tiny_skill_sync::config::{Config, RepoConfig};
use tiny_skill_sync::engine::{ApplyResult, Engine, LocationPreview, Outcome, PluginReport, RepoReport, SyncOptions, SyncReport, Which};
use tiny_skill_sync::github::{FailureKind, RemoteError};
use tiny_skill_sync::plugins::native::{ManagerError, MarketOrigin, NativeMarket, NativePlugin, NativeSnapshot};
use tiny_skill_sync::plugins::{Choice, Operation, Status};
use tiny_skill_sync::source::{SourceKind, SourcePreview};
use tiny_skill_sync::sources::SourceConfig;
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

    /// Takes the plugin out of the repository: its marketplace entry and its manifest.
    fn drop_plugin(&self, name: &str, rest: &[&str]) {
        self.market(rest);
        self.remote.put(REPO, "pack/README.md", "Plugins live here.");
        self.remote.remove(REPO, &format!("pack/{name}/.plugin/plugin.json"));
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
        self.engine.preview_source(&self.config, &self.config.repos()[0], &RUN).expect("preview of a valid marketplace")
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
    f.drop_plugin("alpha", &[]);
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
    f.drop_plugin("alpha", &[]);
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
    assert_eq!(row(only(&report), "alpha").status, Status::NeedsApproval);
    assert!(f.manager.calls().is_empty());
    assert_eq!(f.world.snapshot(), files);

    let cfg = common::standard_config();
    f.remote.seed_standard("o/fixtures");
    synced(only(&f.engine.sync(&cfg, &Which::All, &SyncOptions::default(), &RUN).unwrap()));
    assert!(f.manager.calls().is_empty());
}

#[test]
fn plugin_bare_repository_check_resolves_the_default_branch_before_listing_it() {
    let mut f = Fixture::new("plugin-bare");
    f.remote.set_heads(REPO, &["test/v1"]);
    f.config = config(vec![RepoConfig { kind: SourceKind::Plugins, ..repo(ID, REPO, "pack") }]);
    f.remote.reset();
    let options = SyncOptions { dry_run: true, ..Default::default() };
    let report = f.engine.sync(&f.config, &Which::All, &options, &RUN).unwrap();
    let log = f.remote.log();
    let resolve = log.iter().position(|l| l.starts_with("default_branch"));
    let list = log.iter().position(|l| l.starts_with("tree"));
    assert!(
        resolve.is_some_and(|r| list.is_some_and(|t| r < t)) && log.iter().any(|l| l == "tree o/packages@test/v1:"),
        "the listing must be of the resolved branch by name, after resolving it: {log:?} {report:?}"
    );
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
    assert!(matches!(only(&report).outcome, Outcome::Cancelled), "{report:?}");
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
    f.config.push_lane(RepoConfig { id: "second".into(), path: "pack/alpha".into(), ..f.config.repos()[0].clone() });
    let preview = f.engine.preview_source(&f.config, &f.config.repos()[1], &RUN).unwrap();
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
            "origin" => {
                f.config.update_lane_at(0, |r| r.branch = "different".into());
            }
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
    f.drop_plugin("alpha", &[]);
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
    assert!(f.manager.calls().is_empty(), "plugins nobody approved need no Copilot CLI");
    assert!(!f.world.state().plugin_sources().contains_key(ID), "and no record");

    let count = f.manager.calls().len();
    let files = f.world.snapshot();
    let path = f.world.state_file();
    let saved: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert!(
        saved["version"] == 3 && saved.get("plugin_sources").is_none() && saved["lane_polls"][ID]["attention"] == true,
        "an unapproved check saves only the lane's check data: {saved}"
    );
    let bytes = fs::read(&path).unwrap();
    let stamp = fs::metadata(&path).unwrap().modified().unwrap();
    f.remote.reset();
    let at = f.clock.fetch_add(3 * 24 * 3600, Ordering::Relaxed) + 3 * 24 * 3600;
    let report = f.engine.sync(&f.config, &Which::Due, &SyncOptions::default(), &RUN).unwrap();
    assert!(matches!(only(&report).outcome, Outcome::NeedsAttention));
    assert!(only(&report).plugins.is_empty(), "the board keeps its existing rows");
    assert_eq!(f.remote.log(), ["tree o/packages@test/v1:"]);
    assert_eq!(f.remote.calls().conditional, 1);
    assert_eq!(f.manager.calls().len(), count);
    assert_eq!(f.world.snapshot(), files);
    assert_eq!(fs::read(&path).unwrap(), bytes, "a 304 writes nothing");
    assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), stamp);

    let (mut restarted, clock) = f.world.plugin_engine(&f.remote, &f.manager);
    clock.store(at, Ordering::Relaxed);
    let report = restarted.sync(&f.config, &Which::Due, &SyncOptions::default(), &RUN).unwrap();
    assert!(matches!(only(&report).outcome, Outcome::NeedsAttention));
    assert_eq!(row(only(&report), "alpha").choices, [Choice::Approve { name: "alpha".into(), publisher: Some("Team".into()) }]);
    assert_eq!(f.remote.calls().conditional, 1, "the restarted engine lists the source in full");
    assert!(f.manager.calls().is_empty());
}

#[test]
fn plugin_blocked_choice_cannot_hide_behind_a_timer_304() {
    let mut f = Fixture::new("plugin-choice-blocked-304");
    let choice = row(&f.sync(), "alpha").choices[0].clone();

    f.conflict_market(&[]);
    f.clock.fetch_add(3 * 24 * 3600, Ordering::Relaxed);
    let blocked = f.choose(choice.clone());
    assert_eq!(failed(&blocked).message, MARKET_CONFLICT);
    assert_eq!(row(&blocked, "alpha").status, Status::Blocked);
    assert!(row(&blocked, "alpha").choices.is_empty());
    assert!(!f.world.state().plugin_sources().contains_key(ID), "a blocked choice binds nothing");

    // Only Copilot changed. GitHub would still return 304 for the remembered ETag, but the rows are gone.
    f.remote.reset();
    let timer = f.engine.sync(&f.config, &Which::Due, &SyncOptions::default(), &RUN).unwrap();
    assert_eq!(f.remote.calls().conditional, 0);
    assert!(matches!(only(&timer).outcome, Outcome::NeedsAttention));
    assert_eq!(row(only(&timer), "alpha").choices, std::slice::from_ref(&choice));
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
    f.drop_plugin("beta", &["alpha"]);
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

    // The first check asks nothing of Copilot; approving is what meets the conflict.
    let report = f.sync();
    assert_eq!(row(&report, "alpha").status, Status::NeedsApproval);
    assert!(f.manager.calls().is_empty());
    let bytes = fs::read(&path).unwrap();
    let stamp = fs::metadata(&path).unwrap().modified().unwrap();
    let blocked = f.choose(row(&report, "alpha").choices[0].clone());
    assert_eq!(failed(&blocked).message, MARKET_CONFLICT);
    assert_eq!(row(&blocked, "alpha").status, Status::Blocked);
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
    let message = f.engine.preview_source(&f.config, &f.config.repos()[0], &RUN).unwrap_err().to_string();
    assert!(message.contains(ID) && message.contains("already used"), "{message}");
    assert!(f.manager.calls().is_empty());
    assert!(f.remote.log().is_empty());
    assert_eq!(f.world.snapshot(), before);
}

// ---- Sources that name a GitHub location: the plugins lane runs on the source's own listing and inventory. ----

const KIT: &str = "o/kit";
const WORK: &str = "work";

fn kit_manifest(name: &str, version: &str, publisher: &str) -> String {
    serde_json::json!({"name": name, "version": version, "author": {"name": publisher}, "mcpServers": {}}).to_string()
}

fn kit_market(entries: &[(&str, &str)]) -> String {
    let plugins: Vec<_> = entries.iter().map(|(n, at)| serde_json::json!({"name": n, "source": format!("./{at}")})).collect();
    serde_json::json!({"name": WORK, "plugins": plugins}).to_string()
}

/// A source added the way `tss add` does: previewed, saved with the preview's IDs, then (optionally) approved.
struct Kit {
    world: World,
    remote: Arc<FakeRemote>,
    manager: FakePluginManager,
    engine: Engine,
    cfg: Config,
    source: SourceConfig,
    clock: Arc<AtomicI64>,
}

impl Kit {
    /// A branch with a skill, a marketplace that lists `tool`, a standalone `solo` and `far` listed outside `plugins`.
    fn new(test: &str, input: &str) -> Kit {
        let world = World::new(test);
        let remote = FakeRemote::new();
        remote.seed(
            KIT,
            &[
                ("skills/alpha/SKILL.md", "alpha v1"),
                ("marketplace.json", &kit_market(&[("tool", "plugins/tool"), ("far", "other/far")])),
                ("plugins/tool/.plugin/plugin.json", &kit_manifest("tool", "1", "Team")),
                ("plugins/solo/.plugin/plugin.json", &kit_manifest("solo", "1", "Team")),
                ("other/far/.plugin/plugin.json", &kit_manifest("far", "1", "Team")),
            ],
        );
        let manager = FakePluginManager::new(world.root.join("native"));
        let (mut engine, clock) = world.plugin_engine(&remote, &manager);
        let mut cfg = Config::default();
        let preview = engine.preview_location(&cfg, input, false, &RUN).expect("preview");
        let source = preview.source.clone();
        cfg.add_source_config(source.clone()).unwrap();
        Kit { world, remote, manager, engine, cfg, source, clock }
    }

    fn preview(&mut self, input: &str) -> LocationPreview {
        let mut cfg = Config::default();
        std::mem::swap(&mut cfg, &mut self.cfg);
        let mut preview = self.engine.preview_location(&Config::default(), input, false, &RUN).expect("preview");
        self.cfg = cfg;
        // Reviewing again keeps the saved IDs, as the app does for a saved source.
        preview.source.id.clone_from(&self.source.id);
        preview.source.lanes = self.source.lanes.clone();
        preview
    }

    fn approve(&mut self, input: &str) {
        let preview = self.preview(input);
        self.engine.approve_location_plugins(&self.cfg, &self.source.id, &preview).expect("approval");
    }

    fn lane(&self, kind: SourceKind) -> String {
        self.source.lanes.id(kind).unwrap().to_owned()
    }

    fn sync(&mut self, which: &Which) -> SyncReport {
        self.engine.sync(&self.cfg, which, &SyncOptions::default(), &RUN).unwrap()
    }

    fn native(&self, branch: &str, plugins: &[(&str, &str)]) -> NativeSnapshot {
        NativeSnapshot {
            home: self.world.root.join("native"),
            markets: vec![NativeMarket {
                name: WORK.into(),
                source: "GitHub: o/kit".into(),
                is_default: false,
                origin: Some(MarketOrigin { kind: "github".into(), repo: Some(KIT.into()), reference: Some(branch.into()) }),
            }],
            plugins: plugins
                .iter()
                .map(|&(n, v)| NativePlugin { name: n.into(), marketplace: WORK.into(), version: v.into(), enabled: true, source: "installed".into() })
                .collect(),
        }
    }

    /// Approve and install `names` (what the preview of `input` lists) from `branch`.
    fn install(&mut self, input: &str, branch: &str, names: &[&str]) {
        self.approve(input);
        let add = Call::AddMarket { repo: KIT.into(), branch: branch.into() };
        self.manager.push(add, Reply::Done(Some(self.native(branch, &[]))));
        let mut installed = Vec::new();
        for name in names {
            installed.push((*name, "1"));
            self.manager.push(Call::Install(format!("{name}@work")), Reply::Done(Some(self.native(branch, &installed))));
        }
        let report = self.sync(&Which::All);
        synced(lane_of(&report, &self.lane(SourceKind::Plugins)));
        assert_eq!(self.manager.remaining(), 0);
    }

    fn mutations(&self) -> Vec<Call> {
        self.manager.calls().into_iter().filter(|c| !matches!(c, Call::Home | Call::List | Call::Markets)).collect()
    }
}

fn lane_of<'a>(report: &'a SyncReport, id: &str) -> &'a RepoReport {
    report.repos.iter().find(|r| r.repo_id == id).unwrap_or_else(|| panic!("no lane {id}: {report:?}"))
}

#[test]
fn location_without_plugins_never_launches_copilot_and_an_unchanged_source_costs_one_listing() {
    let w = World::new("loc-no-plugins");
    let remote = FakeRemote::new();
    remote.seed(KIT, &[("skills/alpha/SKILL.md", "alpha v1")]);
    let manager = FakePluginManager::new(w.root.join("native"));
    let (mut engine, clock) = w.plugin_engine(&remote, &manager);
    let mut cfg = Config::default();
    let preview = engine.preview_location(&cfg, KIT, false, &RUN).unwrap();
    assert!(preview.types.plugins, "plugins are on");
    engine.approve_location_plugins(&cfg, &preview.source.id, &preview).expect("nothing to approve is not an error");
    assert!(!w.state_file().exists(), "and writes nothing");
    cfg.add_source_config(preview.source.clone()).unwrap();
    let plugins = preview.source.lanes.id(SourceKind::Plugins).unwrap().to_owned();
    remote.reset();

    let report = engine.sync(&cfg, &Which::All, &SyncOptions::default(), &RUN).unwrap();
    assert!(report.repos.iter().all(|r| matches!(r.outcome, Outcome::Synced | Outcome::Unchanged)), "{report:?}");
    assert!(lane_of(&report, &plugins).lists_nothing);
    assert_eq!(w.skill_names(), ["alpha"]);
    assert!(manager.calls().is_empty(), "no Copilot CLI for a source without plugins");
    assert_eq!(remote.calls().tree, 1);
    // A bare source with a plugins lane names its default branch first, then lists that branch by name.
    let log = remote.log();
    assert!(log.first().is_some_and(|c| c.starts_with("default_branch")) && !log.iter().any(|c| c == "tree o/kit@:"), "{log:?}");
    let listed = log.iter().find(|c| c.starts_with("tree ")).cloned().unwrap();
    assert_eq!(listed, "tree o/kit@main:");

    remote.reset();
    clock.fetch_add(3 * 24 * 3600, Ordering::Relaxed);
    let report = engine.sync(&cfg, &Which::Due, &SyncOptions::default(), &RUN).unwrap();
    assert!(report.repos.iter().all(|r| matches!(r.outcome, Outcome::Unchanged)), "{report:?}");
    assert_eq!(remote.log(), ["tree o/kit@:"], "one conditional listing at the default HEAD for every lane");
    assert_eq!(remote.calls().conditional, 1);
    assert!(manager.calls().is_empty());

    // Changed at HEAD: the source is listed again by the default branch's name, so the plugins lane and the listing
    // agree, and the next unchanged poll is again one conditional request at HEAD.
    remote.put(KIT, "skills/alpha/SKILL.md", "alpha v2");
    remote.reset();
    clock.fetch_add(3 * 24 * 3600, Ordering::Relaxed);
    engine.sync(&cfg, &Which::Due, &SyncOptions::default(), &RUN).unwrap();
    let log = remote.log();
    assert_eq!(log[..3], ["tree o/kit@:", "default_branch o/kit", "tree o/kit@main:"], "{log:?}");
    assert_eq!(w.read("alpha/SKILL.md"), "alpha v2");
    remote.reset();
    clock.fetch_add(3 * 24 * 3600, Ordering::Relaxed);
    engine.sync(&cfg, &Which::Due, &SyncOptions::default(), &RUN).unwrap();
    assert_eq!(remote.log(), ["tree o/kit@:"]);
    assert_eq!(remote.calls().conditional, 1);
}

#[test]
fn a_bare_source_keeps_the_head_etag_so_the_next_unchanged_poll_is_a_304() {
    let w = World::new("loc-head-etag");
    let remote = FakeRemote::new();
    remote.seed(KIT, &[("skills/alpha/SKILL.md", "alpha v1")]);
    // A listing at HEAD and one of `main` by name answer with different ETags, so the test sees which one was kept.
    remote.set_branch_etags(KIT, true);
    let manager = FakePluginManager::new(w.root.join("native"));
    let (mut engine, clock) = w.plugin_engine(&remote, &manager);
    let mut cfg = Config::default();
    let preview = engine.preview_location(&cfg, KIT, false, &RUN).unwrap();
    cfg.add_source_config(preview.source.clone()).unwrap();
    assert_eq!(preview.source.branch, "", "a bare source");
    assert!(preview.source.lanes.id(SourceKind::Plugins).is_some());
    engine.sync(&cfg, &Which::All, &SyncOptions::default(), &RUN).unwrap();

    // Changed at HEAD: listed again by name, which shows the same tree as the HEAD reply.
    remote.put(KIT, "skills/alpha/SKILL.md", "alpha v2");
    remote.reset();
    clock.fetch_add(3 * 24 * 3600, Ordering::Relaxed);
    engine.sync(&cfg, &Which::Due, &SyncOptions::default(), &RUN).unwrap();
    let log = remote.log();
    assert_eq!(log[..3], ["tree o/kit@:", "default_branch o/kit", "tree o/kit@main:"], "{log:?}");
    assert_eq!(w.read("alpha/SKILL.md"), "alpha v2");

    // Unchanged: the ETag kept is the HEAD reply's, so the poll at HEAD is one conditional request answered 304.
    remote.reset();
    clock.fetch_add(3 * 24 * 3600, Ordering::Relaxed);
    let report = engine.sync(&cfg, &Which::Due, &SyncOptions::default(), &RUN).unwrap();
    assert_eq!(remote.log(), ["tree o/kit@:"]);
    assert_eq!(remote.calls(), common::Calls { tree: 1, conditional: 1, ..Default::default() });
    assert!(!report.repos.is_empty() && report.repos.iter().all(|r| matches!(r.outcome, Outcome::Unchanged)), "{report:?}");
    assert!(manager.calls().is_empty());
}

#[test]
fn location_installs_only_marketplace_plugins_in_its_folder_and_protects_a_filtered_owned_one() {
    let input = "https://github.com/o/kit/tree/main/plugins";
    let mut k = Kit::new("loc-folder", input);
    let plugins = k.lane(SourceKind::Plugins);
    let report = k.sync(&Which::All);
    let lane = lane_of(&report, &plugins);
    assert_eq!(row(lane, "tool").status, Status::NeedsApproval);
    assert!(matches!(&row(lane, "solo").status, Status::NotSynced(why) if why.contains("only through a marketplace that lists it")), "{lane:?}");
    assert!(row(lane, "solo").choices.is_empty());
    assert!(lane.plugins.iter().all(|r| r.name != "far"), "a plugin outside the folder is not this source's");
    assert!(k.manager.calls().is_empty());

    k.install(input, "main", &["tool"]);
    let state = k.world.state();
    let record = &state.plugin_sources()[&plugins];
    assert_eq!(record.plugins.keys().collect::<Vec<_>>(), ["tool"]);
    assert_eq!(k.mutations(), [Call::AddMarket { repo: KIT.into(), branch: "main".into() }, Call::Install("tool@work".into())]);

    // Ignored now, and changed and gone from the marketplace upstream: kept as it is, never updated or removed.
    k.cfg.edit(|sources, _| sources[0].ignore_add = vec!["tool".into()]);
    k.remote.put(KIT, "plugins/tool/.plugin/plugin.json", &kit_manifest("tool", "2", "Team"));
    k.remote.put(KIT, "marketplace.json", &kit_market(&[("far", "other/far")]));
    k.manager.set_snapshot(k.native("main", &[("tool", "1")]));
    let report = k.sync(&Which::All);
    let lane = lane_of(&report, &plugins);
    assert!(matches!(row(lane, "tool").status, Status::Protected(_)), "{lane:?}");
    assert!(row(lane, "tool").choices.is_empty());
    assert_eq!(k.mutations().len(), 2);
    assert_eq!(k.world.state().plugin_sources()[&plugins].plugins.keys().collect::<Vec<_>>(), ["tool"]);
}

#[test]
fn location_approval_records_exactly_the_reviewed_tuple_and_changes_need_approval_again() {
    let input = "https://github.com/o/kit/tree/main/plugins";
    let mut k = Kit::new("loc-approve", input);
    let plugins = k.lane(SourceKind::Plugins);
    let preview = k.preview(input);
    assert!(k.manager.calls().is_empty(), "a preview never launches Copilot CLI");

    // A preview of another place is refused.
    let mut other = preview.clone();
    other.source.path = "other".into();
    assert!(k.engine.approve_location_plugins(&k.cfg, &k.source.id, &other).is_err());
    assert!(!k.world.state_file().exists());

    k.engine.approve_location_plugins(&k.cfg, &k.source.id, &preview).unwrap();
    let state = k.world.state();
    let record = &state.plugin_sources()[&plugins];
    assert_eq!(record.approved.iter().collect::<Vec<_>>(), [(&"tool".to_string(), &Some("Team".to_string()))]);
    assert_eq!(record.roots.iter().collect::<Vec<_>>(), [(&"tool".to_string(), &"plugins/tool".to_string())]);
    assert_eq!((record.origin.repo.as_str(), record.origin.branch.as_str(), record.origin.path.as_str()), (KIT, "main", "plugins"));
    assert_eq!(record.copilot_home, k.world.root.join("native"));
    assert_eq!(record.market, WORK);
    assert!(k.mutations().is_empty(), "approval installs nothing by itself");

    // A new publisher, a new plugin and a moved plugin root each need approval; nothing is installed.
    k.manager.set_snapshot(k.native("main", &[]));
    k.remote.put(KIT, "plugins/tool/.plugin/plugin.json", &kit_manifest("tool", "1", "Someone else"));
    k.remote.put(KIT, "plugins/more/.plugin/plugin.json", &kit_manifest("more", "1", "Team"));
    k.remote.put(KIT, "marketplace.json", &kit_market(&[("tool", "plugins/tool"), ("more", "plugins/more")]));
    let report = k.sync(&Which::All);
    let lane = lane_of(&report, &plugins);
    assert_eq!(row(lane, "tool").status, Status::NeedsApproval);
    assert_eq!(row(lane, "more").status, Status::NeedsApproval);

    k.remote.put(KIT, "plugins/tool/.plugin/plugin.json", &kit_manifest("tool", "1", "Team"));
    k.remote.remove(KIT, "plugins/tool/.plugin/plugin.json");
    k.remote.put(KIT, "plugins/moved/.plugin/plugin.json", &kit_manifest("tool", "1", "Team"));
    k.remote.put(KIT, "marketplace.json", &kit_market(&[("tool", "plugins/moved")]));
    let report = k.sync(&Which::All);
    assert_eq!(row(lane_of(&report, &plugins), "tool").status, Status::NeedsApproval);
    assert!(k.mutations().is_empty());
}

#[test]
fn bare_repository_installs_from_its_default_branch_and_a_rename_needs_approval_without_touching_installs() {
    let mut k = Kit::new("loc-default", KIT);
    k.remote.set_heads(KIT, &["trunk"]);
    let plugins = k.lane(SourceKind::Plugins);
    assert_eq!(k.source.branch, "");
    k.install(KIT, "trunk", &["far", "tool"]);
    let state = k.world.state();
    assert_eq!(state.plugin_sources()[&plugins].origin.branch, "trunk");
    assert_eq!(k.world.skill_names(), ["alpha"]);

    // The default branch is renamed and a skill changes: the skill updates, the plugin waits for approval.
    k.remote.set_heads(KIT, &["next"]);
    k.remote.put(KIT, "skills/alpha/SKILL.md", "alpha v2");
    k.manager.set_snapshot(k.native("trunk", &[("far", "1"), ("tool", "1")]));
    let report = k.sync(&Which::All);
    let lane = lane_of(&report, &plugins);
    assert!(matches!(lane.outcome, Outcome::NeedsAttention), "{lane:?}");
    assert!(matches!(&row(lane, "tool").status, Status::Protected(why) if why.contains("next")), "{lane:?}");
    synced(lane_of(&report, &k.source.id));
    assert_eq!(k.world.read("alpha/SKILL.md"), "alpha v2");
    assert_eq!(k.mutations().len(), 3, "nothing reinstalled or uninstalled");
    let choice = Choice::Approve { name: "tool".into(), publisher: Some("Team".into()) };
    assert!(matches!(k.engine.resolve_plugin(&k.cfg, &k.source.id, &choice, &RUN).unwrap().outcome, Outcome::Failed(_)));
    let state = k.world.state();
    let record = &state.plugin_sources()[&plugins];
    assert_eq!(record.origin.branch, "trunk");
    assert!(record.plugins.contains_key("tool"));

    // Fresh approval follows the new branch and keeps what is installed.
    k.approve(KIT);
    let state = k.world.state();
    let record = &state.plugin_sources()[&plugins];
    assert_eq!(record.origin.branch, "next");
    assert!(record.plugins.contains_key("tool"));
    assert_eq!(k.mutations().len(), 3);
}

#[test]
fn location_copilot_failure_fails_only_the_plugins_lane() {
    let mut k = Kit::new("loc-cli-missing", KIT);
    k.approve(KIT);
    k.manager.push(Call::Home, Reply::Failed(ManagerError::NotFound));
    let report = k.sync(&Which::All);
    let lane = lane_of(&report, &k.lane(SourceKind::Plugins));
    assert!(matches!(&lane.outcome, Outcome::Failed(f) if f.message.contains("Copilot CLI was not found")), "{lane:?}");
    synced(lane_of(&report, &k.source.id));
    assert_eq!(k.world.skill_names(), ["alpha"]);
    assert!(k.mutations().is_empty());
}

#[test]
fn location_seen_is_reported_for_a_read_listing_and_an_unchanged_poll_touches_no_file() {
    let mut k = Kit::new("loc-seen", KIT);
    let report = k.sync(&Which::All);
    assert_eq!(report.seen.len(), 1, "{:?}", report.seen);
    let (id, seen) = &report.seen[0];
    assert_eq!(id, &k.source.id);
    assert_eq!(seen.branch, "main");
    assert!(seen.summary != Default::default());
    let file = k.world.paths().seen_file();
    assert_eq!(tiny_skill_sync::seen::load(&file).get(id), Some(seen));
    let stamp = fs::metadata(&file).unwrap().modified().unwrap();

    k.remote.reset();
    k.clock.fetch_add(3 * 24 * 3600, Ordering::Relaxed);
    let report = k.sync(&Which::Due);
    assert!(report.repos.iter().all(|r| !matches!(r.outcome, Outcome::Failed(_))), "{report:?}");
    assert_eq!(k.remote.calls().conditional, 1);
    assert!(report.seen.is_empty());
    assert_eq!(fs::metadata(&file).unwrap().modified().unwrap(), stamp);
    assert!(k.manager.calls().is_empty());
}

#[test]
fn migrated_plugin_records_keep_their_approvals_and_installs_without_a_reinstall() {
    let mut f = Fixture::new("plugin-migrated");
    f.install();
    // A v0.8.0 record has no plugin roots.
    let path = f.world.state_file();
    let mut json: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    fn strip(v: &mut serde_json::Value) -> usize {
        match v {
            serde_json::Value::Object(map) => map.remove("roots").map_or(0, |_| 1) + map.values_mut().map(strip).sum::<usize>(),
            serde_json::Value::Array(items) => items.iter_mut().map(strip).sum(),
            _ => 0,
        }
    }
    assert_eq!(strip(&mut json), 1);
    fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
    let before = f.world.state().plugin_sources()[ID].clone();
    assert!(before.roots.is_empty());

    let (mut engine, _) = f.world.plugin_engine(&f.remote, &f.manager);
    let report = engine.sync(&f.config, &Which::All, &SyncOptions::default(), &RUN).unwrap();
    assert_eq!(row(only(&report), "alpha").status, Status::UpToDate);
    assert_eq!(f.mutations(), [Call::Install("alpha@tools".into())], "no reinstall");
    let after = f.world.state().plugin_sources()[ID].clone();
    assert_eq!((after.approved, after.plugins), (before.approved, before.plugins));
}
