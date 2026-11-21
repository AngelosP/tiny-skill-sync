//! Consent and observations decide what may be done. Only a verified action changes a plugin record.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use crate::config::RepoConfig;
use crate::source::SourceKind;
use crate::state::{OwnedPlugin, PluginOrigin, PluginSourceState, State};

use super::catalog::{Catalog, Plugin};
use super::native::{NativePlugin, NativeSnapshot};
use super::{NameMap, name_key};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Choice {
    Approve { name: String, publisher: Option<String> },
    Reinstall(String),
    Uninstall(String),
    Forget(String),
}

impl Choice {
    pub(crate) fn name(&self) -> &str {
        match self {
            Self::Approve { name, .. } | Self::Reinstall(name) | Self::Uninstall(name) | Self::Forget(name) => name,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Blocker {
    IncompleteCatalog,
    NoNativeSnapshot,
    SourceChanged,
    BadNativeSnapshot,
    MarketplaceConflict { market: String, repo: String, branch: String },
}

impl fmt::Display for Blocker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IncompleteCatalog => f.write_str("The plugin catalog could not be read completely."),
            Self::NoNativeSnapshot => f.write_str("Could not check the plugins installed in Copilot."),
            Self::SourceChanged => f.write_str("This source no longer matches what was approved on this PC. Remove it and add it again to review its plugins."),
            Self::BadNativeSnapshot => f.write_str("Copilot returned plugin data that could not be verified."),
            Self::MarketplaceConflict { market, repo, branch } => {
                write!(f, "Copilot already has a marketplace named {market} that does not follow {repo}@{branch}.")
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    NeedsApproval,
    UpToDate,
    TurnedOff,
    RemovedInCopilot,
    RemovedFromGitHub,
    AlreadyInstalled,
    AlreadyTracked(String),
    HostedElsewhere,
    Rejected(String),
    WaitingForMarketplace,
    Planned(Operation),
    Blocked,
}

impl Status {
    pub(crate) fn needs_attention(&self, done: bool) -> bool {
        match self {
            Self::NeedsApproval | Self::RemovedFromGitHub | Self::Rejected(_) | Self::WaitingForMarketplace | Self::Blocked => true,
            Self::Planned(_) => !done,
            Self::UpToDate | Self::TurnedOff | Self::RemovedInCopilot | Self::AlreadyInstalled | Self::AlreadyTracked(_) | Self::HostedElsewhere => false,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub actions: Vec<Action>,
    pub statuses: NameMap<Status>,
    pub blocker: Option<Blocker>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Register,
    Install,
    Adopt,
    Update,
    Uninstall,
    Approve,
    Forget,
}

#[derive(Clone, Debug)]
struct Desired {
    publisher: Option<String>,
    version: Option<String>,
    fingerprint: String,
}

#[derive(Clone, Debug)]
pub struct Action {
    operation: Operation,
    id: String,
    origin: PluginOrigin,
    home: PathBuf,
    market: String,
    name: Option<String>,
    before: Option<NativePlugin>,
    had_record: bool,
    owned: Option<OwnedPlugin>,
    approved: Option<Option<String>>,
    desired: Option<Desired>,
}

impl Action {
    pub fn operation(&self) -> Operation {
        self.operation
    }
    pub fn origin(&self) -> &PluginOrigin {
        &self.origin
    }
    pub fn market(&self) -> &str {
        &self.market
    }
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
    pub fn qualified_name(&self) -> Option<String> {
        self.name().map(|name| format!("{name}@{}", self.market))
    }

    fn record_still_applies(&self, state: &State) -> bool {
        if state.check_plugin_id(&self.id).is_err() || self.name().is_some_and(|name| state.plugin_owner_elsewhere(&self.id, &self.market, name).is_some()) {
            return false;
        }
        state.plugin_sources().get(&self.id).map_or(!self.had_record, |record| {
            self.had_record
                && record.origin == self.origin
                && record.market == self.market
                && record.copilot_home == self.home
                && self.name().is_none_or(|name| record.plugins.get(name) == self.owned.as_ref() && record.approved.get(name) == self.approved.as_ref())
        })
    }

    fn registered(&self, snapshot: &NativeSnapshot) -> Option<bool> {
        match snapshot.market(&self.market).ok()? {
            None => Some(false),
            Some(market) if market.confirms(&self.market, &self.origin.repo, &self.origin.branch) => Some(true),
            Some(_) => None,
        }
    }

    fn same_plugin(&self, snapshot: &NativeSnapshot) -> bool {
        self.name().is_none_or(|name| snapshot.plugin(name, &self.market).is_ok_and(|p| p == self.before.as_ref()))
    }

    /// Check the latest observation just before acting. Hold the state lock through the command and verification.
    pub fn ready(&self, source: &RepoConfig, state: &State, snapshot: &NativeSnapshot) -> bool {
        source.id == self.id
            && self.origin.matches(source)
            && self.record_still_applies(state)
            && snapshot.home == self.home
            && self.same_plugin(snapshot)
            && match self.operation {
                Operation::Register => self.registered(snapshot) == Some(false),
                Operation::Approve | Operation::Forget => self.registered(snapshot).is_some(),
                _ => self.registered(snapshot) == Some(true),
            }
    }

    /// Check the native snapshot after a successful command; exit zero alone is not proof.
    /// Copilot may skip an update that brings no new version.
    pub fn verified(&self, after: &NativeSnapshot) -> bool {
        if after.home != self.home {
            return false;
        }
        if matches!(self.operation, Operation::Approve | Operation::Forget) {
            return self.registered(after).is_some() && self.same_plugin(after);
        }
        if self.registered(after) != Some(true) {
            return false;
        }
        if self.operation == Operation::Register {
            return true;
        }
        let name = self.name().expect("plugin actions have a name");
        let Ok(found) = after.plugin(name, &self.market) else {
            return false;
        };
        if self.operation == Operation::Uninstall {
            return found.is_none();
        }
        let Some(plugin) = found.filter(|p| installed(p, name, &self.market)) else {
            return false;
        };
        let version = self.desired.as_ref().and_then(|d| d.version.as_deref());
        version.is_none_or(|v| v == plugin.version)
            && match self.operation {
                Operation::Adopt => version.is_some() && self.same_plugin(after),
                Operation::Install | Operation::Update => true,
                _ => false,
            }
    }

    /// Use a fresh native snapshot after a successful command, or the latest one if no command was needed.
    pub fn record_verified(&self, state: &mut State, after: &NativeSnapshot) -> Result<(), String> {
        if !self.verified(after) || !self.record_still_applies(state) {
            return Err("Copilot did not confirm the planned plugin change".into());
        }
        let mut record = state.plugin_sources().get(&self.id).cloned().unwrap_or_else(|| PluginSourceState {
            origin: self.origin.clone(),
            copilot_home: self.home.clone(),
            market: self.market.clone(),
            registered_by_app: false,
            poll: Default::default(),
            approved: NameMap::new(),
            plugins: NameMap::new(),
        });
        if self.operation == Operation::Register {
            record.registered_by_app = true;
        } else {
            let name = self.name().expect("plugin actions have a name");
            if let Some(desired) = &self.desired {
                record.approved.insert(name.into(), desired.publisher.clone());
                if self.operation != Operation::Approve {
                    let native = after.plugin(name, &self.market).expect("verified plugin list").expect("verified plugin");
                    let by_app = match self.operation {
                        Operation::Adopt => false,
                        Operation::Update => self.owned.as_ref().is_some_and(|owned| owned.by_app),
                        _ => true,
                    };
                    record.plugins.insert(name.into(), OwnedPlugin { fingerprint: desired.fingerprint.clone(), version: Some(native.version.clone()), by_app });
                }
            } else {
                record.plugins.remove(name);
                record.approved.remove(name);
            }
        }
        record.poll.etag = None;
        state.set_plugin_source(&self.id, record)
    }
}

fn installed(p: &NativePlugin, name: &str, market: &str) -> bool {
    p.source == "installed" && p.name == name && p.marketplace == market
}

struct Context<'a> {
    source: &'a RepoConfig,
    market: &'a str,
    record: Option<&'a PluginSourceState>,
    native: &'a NativeSnapshot,
    registered: bool,
}

impl<'a> Context<'a> {
    fn new(source: &'a RepoConfig, market: &'a str, state: &'a State, snapshot: Option<&'a NativeSnapshot>) -> Result<Self, Blocker> {
        let native = snapshot.ok_or(Blocker::NoNativeSnapshot)?;
        if crate::source::check_root(&native.home).is_err() {
            return Err(Blocker::BadNativeSnapshot);
        }
        let record = state.plugin_sources().get(&source.id);
        if source.kind != SourceKind::Plugins
            || state.check_plugin_id(&source.id).is_err()
            || record.is_some_and(|r| !r.origin.matches(source) || r.copilot_home != native.home || r.market != market)
        {
            return Err(Blocker::SourceChanged);
        }
        let registered = match native.market(market).map_err(|_| Blocker::BadNativeSnapshot)? {
            None => false,
            Some(m) if m.confirms(market, &source.repo, &source.branch) => true,
            Some(_) => return Err(Blocker::MarketplaceConflict { market: market.into(), repo: source.repo.clone(), branch: source.branch.clone() }),
        };
        Ok(Self { source, market, record, native, registered })
    }

    fn owned(&self, name: &str) -> Option<&OwnedPlugin> {
        self.record?.plugins.get(name)
    }
    fn approved(&self, name: &str) -> Option<&Option<String>> {
        self.record?.approved.get(name)
    }

    fn action(&self, operation: Operation, name: Option<&str>, before: Option<&NativePlugin>, plugin: Option<&Plugin>) -> Action {
        Action {
            operation,
            id: self.source.id.clone(),
            origin: self.source.into(),
            home: self.native.home.clone(),
            market: self.market.into(),
            name: name.map(str::to_owned),
            before: before.cloned(),
            had_record: self.record.is_some(),
            owned: name.and_then(|n| self.owned(n)).cloned(),
            approved: name.and_then(|n| self.approved(n)).cloned(),
            desired: plugin.map(|p| Desired { publisher: p.publisher.clone(), version: p.version.clone(), fingerprint: p.fingerprint.clone() }),
        }
    }

    fn remove(&self, name: &str, before: Option<&NativePlugin>, uninstall: bool) -> (Status, Option<Action>) {
        let op = if !uninstall || before.is_none() {
            Operation::Forget
        } else if self.registered && before.is_some_and(|p| installed(p, name, self.market)) {
            Operation::Uninstall
        } else {
            return (Status::AlreadyInstalled, None);
        };
        (Status::Planned(op), Some(self.action(op, Some(name), before, None)))
    }
}

fn blocked(catalog: Option<&Catalog>, record: Option<&PluginSourceState>, blocker: Blocker) -> Plan {
    let mut statuses = NameMap::new();
    for name in catalog.into_iter().flat_map(|c| c.plugins.keys()).chain(record.into_iter().flat_map(|r| r.plugins.keys())) {
        statuses.insert(name.clone(), Status::Blocked);
    }
    Plan { actions: vec![], statuses, blocker: Some(blocker) }
}

/// `None` means no complete catalog or native snapshot; it never means an empty successful listing.
/// A choice only acts on its intended plugin; unrelated actions can wait for a normal check.
/// When registration is needed it is the only action. Verify it and replan with the new snapshot and the same choice.
pub fn plan(source: &RepoConfig, catalog: Option<&Catalog>, state: &State, snapshot: Option<&NativeSnapshot>, choice: Option<&Choice>) -> Plan {
    let hold = |why| blocked(catalog, state.plugin_sources().get(&source.id), why);
    let Some(catalog) = catalog else {
        return hold(Blocker::IncompleteCatalog);
    };
    if !catalog.origin.matches(source) {
        return hold(Blocker::SourceChanged);
    }
    let cx = match Context::new(source, &catalog.market, state, snapshot) {
        Ok(cx) => cx,
        Err(why) => return hold(why),
    };
    let mut plan = Plan::default();
    let mut needs_market = false;
    for plugin in catalog.plugins.values() {
        let name = &plugin.name;
        let Ok(before) = cx.native.plugin(name, cx.market) else {
            return hold(Blocker::BadNativeSnapshot);
        };
        let approve = matches!(choice, Some(Choice::Approve { name: n, publisher }) if n == name && *publisher == plugin.publisher);
        let (mut status, op) = if let Some(other) = state.plugin_owner_elsewhere(&source.id, cx.market, name) {
            (Status::AlreadyTracked(other.into()), None)
        } else if !approve && cx.approved(name) != Some(&plugin.publisher) {
            (Status::NeedsApproval, None)
        } else {
            decide(&cx, plugin, before, choice)
        };
        let chosen = match choice {
            None => true,
            Some(Choice::Approve { .. }) => approve,
            Some(Choice::Reinstall(n)) => n == name && cx.owned(name).is_some() && before.is_none(),
            Some(Choice::Uninstall(_) | Choice::Forget(_)) => false,
        };
        if let Some(op) = op.filter(|_| chosen) {
            if !cx.registered {
                needs_market = true;
                status = Status::WaitingForMarketplace;
            } else {
                plan.actions.push(cx.action(op, Some(name), before, Some(plugin)));
            }
        } else if approve && status != Status::NeedsApproval && !matches!(status, Status::AlreadyTracked(_)) && cx.approved(name) != Some(&plugin.publisher) {
            plan.actions.push(cx.action(Operation::Approve, Some(name), before, Some(plugin)));
        }
        plan.statuses.insert(name.clone(), status);
    }

    let mut rejected = BTreeMap::new();
    let mut hosted = std::collections::BTreeSet::new();
    for name in &catalog.hosted_elsewhere {
        hosted.insert(name_key(name));
        if !plan.statuses.contains_key(name) {
            plan.statuses.insert(name.clone(), Status::HostedElsewhere);
        }
    }
    for (i, r) in catalog.rejected.iter().enumerate() {
        rejected.insert(name_key(&r.name), i);
        plan.statuses.insert(r.name.clone(), Status::Rejected(r.reason.clone()));
    }
    if let Some(record) = cx.record {
        for name in record.plugins.keys().filter(|n| !catalog.plugins.contains_key(n)) {
            let status = if let Some(&i) = rejected.get(name).or_else(|| rejected.get("")) {
                Status::Rejected(catalog.rejected[i].reason.clone())
            } else if hosted.contains(name) || hosted.contains("") {
                Status::HostedElsewhere
            } else if matches!(choice, Some(Choice::Uninstall(n) | Choice::Forget(n)) if n == name) {
                let Ok(before) = cx.native.plugin(name, cx.market) else {
                    return hold(Blocker::BadNativeSnapshot);
                };
                let (status, action) = cx.remove(name, before, matches!(choice, Some(Choice::Uninstall(_))));
                plan.actions.extend(action);
                status
            } else {
                Status::RemovedFromGitHub
            };
            plan.statuses.insert(name.clone(), status);
        }
    }
    if needs_market {
        plan.actions = vec![cx.action(Operation::Register, None, None, None)];
    }
    plan
}

fn decide(cx: &Context<'_>, plugin: &Plugin, before: Option<&NativePlugin>, choice: Option<&Choice>) -> (Status, Option<Operation>) {
    let name = &plugin.name;
    let op = match (cx.owned(name), before) {
        (None, None) => Operation::Install,
        (Some(_), None) if matches!(choice, Some(Choice::Reinstall(n)) if n == name) => Operation::Install,
        (Some(_), None) => return (Status::RemovedInCopilot, None),
        (_, Some(p)) if !installed(p, name, cx.market) => return (Status::AlreadyInstalled, None),
        (None, Some(p)) if plugin.version.as_deref() == Some(p.version.as_str()) => Operation::Adopt,
        (None, Some(_)) => return (Status::AlreadyInstalled, None),
        (Some(_), Some(p)) if !p.enabled => return (Status::TurnedOff, None),
        (Some(owned), Some(_)) if owned.fingerprint != plugin.fingerprint => Operation::Update,
        (Some(_), Some(_)) => return (Status::UpToDate, None),
    };
    (Status::Planned(op), Some(op))
}

/// Only for an explicit remove-source-and-uninstall request, never for a sync or a plain source removal.
/// Keep the source if any of these actions fail or any plugin is held.
pub fn plan_source_removal(source: &RepoConfig, state: &State, snapshot: Option<&NativeSnapshot>) -> Plan {
    let Some(record) = state.plugin_sources().get(&source.id) else {
        return Plan::default();
    };
    let hold = |why| blocked(None, Some(record), why);
    let cx = match Context::new(source, &record.market, state, snapshot) {
        Ok(cx) => cx,
        Err(why) => return hold(why),
    };
    let mut plan = Plan::default();
    for name in record.plugins.keys() {
        let Ok(before) = cx.native.plugin(name, cx.market) else {
            return hold(Blocker::BadNativeSnapshot);
        };
        let (status, action) = cx.remove(name, before, true);
        plan.statuses.insert(name.clone(), status);
        plan.actions.extend(action);
    }
    plan
}

#[cfg(test)]
mod tests;
