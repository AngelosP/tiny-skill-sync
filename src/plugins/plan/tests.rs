use super::*;
use crate::plugins::catalog::{Capabilities, Plugin};
use crate::plugins::native::{MarketOrigin, NativeMarket};
use crate::tree::Rejected;

fn fixture() -> (RepoConfig, Catalog, State, NativeSnapshot) {
    let source = RepoConfig {
        id: "this".into(),
        kind: SourceKind::Plugins,
        repo: "owner/repo".into(),
        branch: "release/v2".into(),
        path: "tools".into(),
        ..Default::default()
    };
    let plugin = Plugin {
        name: "tool".into(),
        path: "tools/tool".into(),
        version: Some("2".into()),
        publisher: Some("Team".into()),
        description: None,
        capabilities: Capabilities::default(),
        fingerprint: "b".repeat(40),
    };
    let catalog = Catalog::fixture(source.clone(), "market", plugin);
    let snapshot = NativeSnapshot {
        home: r"D:\isolated\copilot".into(),
        plugins: vec![],
        markets: vec![NativeMarket {
            name: "market".into(),
            source: "GitHub: Owner/Repo".into(),
            is_default: false,
            origin: Some(MarketOrigin { kind: "github".into(), repo: Some("owner/repo".into()), reference: Some("release/v2".into()) }),
        }],
    };
    let mut state = State::default();
    state.set_plugin_source("this", catalog.approved_record(&source, &snapshot.home).unwrap()).unwrap();
    (source, catalog, state, snapshot)
}

fn native(version: &str, enabled: bool) -> NativePlugin {
    NativePlugin { name: "tool".into(), marketplace: "market".into(), version: version.into(), enabled, source: "installed".into() }
}

fn owned(state: &mut State) {
    let mut record = state.plugin_sources()["this"].clone();
    record.plugins.insert("tool".into(), OwnedPlugin { fingerprint: "a".repeat(40), version: Some("1".into()), by_app: false });
    state.set_plugin_source("this", record).unwrap();
}

#[test]
fn new_or_changed_publishers_need_specific_local_approval() {
    let (source, mut catalog, mut state, mut snapshot) = fixture();
    assert!(plan(&source, Some(&catalog), &State::default(), Some(&snapshot), None).actions.is_empty());
    owned(&mut state);
    snapshot.plugins.push(native("1", true));
    catalog.plugins.get_mut("tool").unwrap().publisher = Some("Other".into());
    let stale = Choice::Approve { name: "tool".into(), publisher: Some("Team".into()) };
    for choice in [None, Some(&stale)] {
        let planned = plan(&source, Some(&catalog), &state, Some(&snapshot), choice);
        assert_eq!(planned.statuses["tool"], Status::NeedsApproval);
        assert!(planned.actions.is_empty());
    }
    let approval = Choice::Approve { name: "tool".into(), publisher: Some("Other".into()) };
    let planned = plan(&source, Some(&catalog), &state, Some(&snapshot), Some(&approval));
    let [action] = planned.actions.as_slice() else { panic!("expected one update") };
    assert_eq!(action.operation(), Operation::Update);
    assert!(action.ready(&source, &state, &snapshot));
    let before = state.clone();
    assert!(!action.verified(&snapshot), "the declared new version must appear in Copilot");
    assert!(action.record_verified(&mut state, &snapshot).is_err());
    assert_eq!(state, before, "a false success changes neither approval nor ownership");
    snapshot.plugins[0].version = "2".into();
    assert!(action.verified(&snapshot));
    action.record_verified(&mut state, &snapshot).unwrap();
    assert_eq!(state.plugin_sources()["this"].approved["tool"].as_deref(), Some("Other"));
    assert_eq!(state.plugin_sources()["this"].plugins["tool"], OwnedPlugin { fingerprint: "b".repeat(40), version: Some("2".into()), by_app: false });
}

#[test]
fn registration_alone_precedes_mutations_and_both_origin_proofs_are_required() {
    let (source, catalog, mut state, mut snapshot) = fixture();
    let proven = snapshot.markets.pop().unwrap();
    let planned = plan(&source, Some(&catalog), &state, Some(&snapshot), None);
    let [register] = planned.actions.as_slice() else { panic!("register is the only allowed action") };
    assert_eq!(register.operation(), Operation::Register);
    assert!(register.ready(&source, &state, &snapshot));
    assert!(!register.verified(&snapshot));
    for bad in [
        NativeMarket { is_default: true, ..proven.clone() },
        NativeMarket { origin: None, ..proven.clone() },
        NativeMarket { source: "Local: C:/test".into(), ..proven.clone() },
        NativeMarket { origin: Some(MarketOrigin { reference: None, ..proven.origin.clone().unwrap() }), ..proven.clone() },
        NativeMarket { origin: Some(MarketOrigin { reference: Some("release/V2".into()), ..proven.origin.clone().unwrap() }), ..proven.clone() },
        NativeMarket { origin: Some(MarketOrigin { repo: Some("someone/else".into()), ..proven.origin.clone().unwrap() }), ..proven.clone() },
    ] {
        snapshot.markets = vec![bad];
        assert!(!register.verified(&snapshot));
        let plan = plan(&source, Some(&catalog), &state, Some(&snapshot), None);
        assert!(matches!(plan.blocker, Some(Blocker::MarketplaceConflict { .. })));
        assert!(plan.actions.is_empty());
    }
    snapshot.markets = vec![proven];
    assert!(register.verified(&snapshot));
    register.record_verified(&mut state, &snapshot).unwrap();
    assert!(state.plugin_sources()["this"].registered_by_app);
    let install = plan(&source, Some(&catalog), &state, Some(&snapshot), None).actions.remove(0);
    assert_eq!(install.operation(), Operation::Install);
    assert_eq!(install.qualified_name().as_deref(), Some("tool@market"));
    assert!(install.ready(&source, &state, &snapshot));
    assert!(!install.verified(&snapshot));
    snapshot.plugins.push(native("1", true));
    assert!(!install.verified(&snapshot));
    snapshot.plugins[0].version = "2".into();
    assert!(!install.ready(&source, &state, &snapshot), "cannot install over a plugin that appeared meanwhile");
    assert!(install.verified(&snapshot));
    install.record_verified(&mut state, &snapshot).unwrap();
    assert!(state.plugin_sources()["this"].plugins["tool"].by_app);
    assert_eq!(plan(&source, Some(&catalog), &state, Some(&snapshot), None).statuses["tool"], Status::UpToDate);
}

#[test]
fn adoption_and_updates_require_native_evidence_and_respect_disabled_plugins() {
    let (source, catalog, mut state, mut snapshot) = fixture();
    snapshot.plugins = vec![native("1", true)];
    assert_eq!(plan(&source, Some(&catalog), &state, Some(&snapshot), None).statuses["tool"], Status::AlreadyInstalled);
    snapshot.plugins[0].version = "2".into();
    snapshot.plugins[0].source = "live".into();
    assert_eq!(plan(&source, Some(&catalog), &state, Some(&snapshot), None).statuses["tool"], Status::AlreadyInstalled);
    snapshot.plugins = vec![native("2", true)];
    let adopt = plan(&source, Some(&catalog), &state, Some(&snapshot), None).actions.remove(0);
    assert_eq!(adopt.operation(), Operation::Adopt);
    let confirmed = snapshot.markets[0].origin.take();
    assert!(!adopt.ready(&source, &state, &snapshot));
    assert!(adopt.record_verified(&mut state, &snapshot).is_err());
    snapshot.markets[0].origin = confirmed;
    adopt.record_verified(&mut state, &snapshot).unwrap();
    assert!(!state.plugin_sources()["this"].plugins["tool"].by_app);
    owned(&mut state);
    snapshot.plugins = vec![native("1", false)];
    let turned_off = plan(&source, Some(&catalog), &state, Some(&snapshot), None);
    assert_eq!(turned_off.statuses["tool"], Status::TurnedOff);
    assert!(turned_off.actions.is_empty());
    snapshot.plugins[0].enabled = true;
    let update = plan(&source, Some(&catalog), &state, Some(&snapshot), None).actions.remove(0);
    assert_eq!(update.operation(), Operation::Update);
    assert!(!update.verified(&snapshot));
    snapshot.plugins[0].version = "2".into();
    assert!(update.verified(&snapshot));
}

#[test]
fn removal_needs_a_choice_and_a_complete_catalog_and_uninstall_must_be_verified() {
    let (source, mut catalog, mut state, mut snapshot) = fixture();
    owned(&mut state);
    let missing = plan(&source, Some(&catalog), &state, Some(&snapshot), None);
    assert_eq!(missing.statuses["tool"], Status::RemovedInCopilot);
    assert!(missing.actions.is_empty());
    assert_eq!(plan(&source, Some(&catalog), &state, Some(&snapshot), Some(&Choice::Reinstall("tool".into()))).actions[0].operation(), Operation::Install);
    snapshot.plugins = vec![native("1", true)];
    catalog.plugins.clear();
    let removed = plan(&source, Some(&catalog), &state, Some(&snapshot), None);
    assert_eq!(removed.statuses["tool"], Status::RemovedFromGitHub);
    assert!(removed.actions.is_empty());
    let choice = Choice::Uninstall("tool".into());
    assert!(plan(&source, None, &state, Some(&snapshot), Some(&choice)).actions.is_empty());
    assert!(plan(&source, Some(&catalog), &state, None, Some(&choice)).actions.is_empty());
    catalog.rejected.push(Rejected { name: "tool".into(), reason: "Bad manifest".into() });
    let rejected = plan(&source, Some(&catalog), &state, Some(&snapshot), Some(&choice));
    assert!(rejected.actions.is_empty());
    assert!(matches!(rejected.statuses["tool"], Status::Rejected(_)));
    catalog.rejected.clear();
    let remove = plan_source_removal(&source, &state, Some(&snapshot));
    assert_eq!(remove.actions[0].operation(), Operation::Uninstall);
    let plan = plan(&source, Some(&catalog), &state, Some(&snapshot), Some(&choice));
    let action = &plan.actions[0];
    assert_eq!(action.operation(), Operation::Uninstall);
    assert!(!action.verified(&snapshot));
    assert!(action.record_verified(&mut state, &snapshot).is_err());
    let forget = plan_source_removal(&source, &state, Some(&snapshot));
    assert_eq!(forget.actions[0].operation(), Operation::Uninstall);
    snapshot.plugins.clear();
    action.record_verified(&mut state, &snapshot).unwrap();
    assert!(state.plugin_sources()["this"].plugins.is_empty());
    assert!(state.plugin_sources()["this"].approved.is_empty());
}

#[test]
fn changed_identity_or_other_owners_hold_even_after_an_approval() {
    let (source, catalog, mut state, mut snapshot) = fixture();
    let choice = Choice::Approve { name: "tool".into(), publisher: Some("Team".into()) };
    let native_home = snapshot.home.clone();
    snapshot.home = r"E:\other-copilot".into();
    let held = plan(&source, Some(&catalog), &state, Some(&snapshot), Some(&choice));
    assert_eq!(held.blocker, Some(Blocker::SourceChanged));
    assert!(held.actions.is_empty());
    snapshot.home = native_home;
    let other = RepoConfig { id: "other".into(), ..source.clone() };
    let mut record = PluginSourceState::new(&other, &snapshot.home, "market");
    record.plugins.insert("tool".into(), OwnedPlugin { fingerprint: "a".repeat(40), by_app: true, version: None });
    state.set_plugin_source("other", record).unwrap();
    let held = plan(&source, Some(&catalog), &state, Some(&snapshot), Some(&choice));
    assert_eq!(held.statuses["tool"], Status::AlreadyTracked("other".into()));
    assert!(held.actions.is_empty());
    let changed = RepoConfig { path: "elsewhere".into(), ..source };
    let held = plan(&changed, Some(&catalog), &state, Some(&snapshot), Some(&choice));
    assert_eq!(held.blocker, Some(Blocker::SourceChanged));
    assert!(held.actions.is_empty());
}

#[test]
fn a_stale_uninstall_choice_does_not_update_a_plugin_that_reappeared() {
    let (source, catalog, mut state, mut snapshot) = fixture();
    owned(&mut state);
    snapshot.plugins.push(native("1", true));
    let choice = Choice::Uninstall("tool".into());
    assert!(plan(&source, Some(&catalog), &state, Some(&snapshot), Some(&choice)).actions.is_empty());
    let automatic = plan(&source, Some(&catalog), &state, Some(&snapshot), None);
    assert_eq!(automatic.actions[0].operation(), Operation::Update);
}

#[test]
fn update_settles_without_a_version_change_when_the_declared_version_matches() {
    update_settles(Some("1"));
}

#[test]
fn update_settles_without_a_version_change_when_no_version_is_declared() {
    update_settles(None);
}

fn update_settles(version: Option<&str>) {
    let (source, mut catalog, mut state, mut snapshot) = fixture();
    owned(&mut state);
    snapshot.plugins.push(native("1", true));
    catalog.plugins.get_mut("tool").unwrap().version = version.map(str::to_owned);
    let planned = plan(&source, Some(&catalog), &state, Some(&snapshot), None);
    let [update] = planned.actions.as_slice() else { panic!("expected one update") };
    assert_eq!(update.operation(), Operation::Update);
    assert!(update.ready(&source, &state, &snapshot));

    let mut invalid = snapshot.clone();
    invalid.plugins[0].source = "live".into();
    assert!(!update.verified(&invalid));
    invalid.plugins.clear();
    assert!(!update.verified(&invalid));

    assert!(update.verified(&snapshot), "the installed plugin still satisfies the manifest");
    update.record_verified(&mut state, &snapshot).unwrap();
    assert_eq!(state.plugin_sources()["this"].plugins["tool"].fingerprint, catalog.plugins["tool"].fingerprint);
    let next = plan(&source, Some(&catalog), &state, Some(&snapshot), None);
    assert_eq!(next.statuses["tool"], Status::UpToDate);
    assert!(next.actions.is_empty());
}

#[test]
fn source_removal_holds_an_install_without_marketplace_or_install_proof() {
    let (source, _, mut state, mut snapshot) = fixture();
    owned(&mut state);
    snapshot.plugins = vec![native("1", true)];
    let proven = snapshot.markets.pop().unwrap();
    let missing_market = plan_source_removal(&source, &state, Some(&snapshot));
    assert_eq!(missing_market.statuses["tool"], Status::AlreadyInstalled);
    assert!(missing_market.actions.is_empty());
    snapshot.markets.push(proven);
    snapshot.plugins[0].source = "live".into();
    let different_install = plan_source_removal(&source, &state, Some(&snapshot));
    assert_eq!(different_install.statuses["tool"], Status::AlreadyInstalled);
    assert!(different_install.actions.is_empty());
}
