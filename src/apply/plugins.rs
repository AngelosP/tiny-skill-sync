//! The only executor of native plugin plans. Use the latest observation before acting, and
//! verify a command with a fresh observation before recording its result under the engine lock.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::config::RepoConfig;
use crate::plugins::native::{ManagerError, NativeSnapshot, PluginManager};
use crate::plugins::{Action, Operation};
use crate::state::State;

fn running(cancel: &AtomicBool) -> Result<(), ManagerError> {
    if cancel.load(Ordering::Relaxed) { Err(ManagerError::Cancelled) } else { Ok(()) }
}

#[inline(never)]
pub(crate) fn snapshot(manager: &dyn PluginManager, cancel: &AtomicBool) -> Result<NativeSnapshot, ManagerError> {
    running(cancel)?;
    let home = manager.home()?;
    let plugins = manager.list(cancel)?;
    let markets = manager.markets(cancel)?;
    running(cancel)?;
    Ok(NativeSnapshot { home, plugins, markets })
}

pub(crate) struct ActionError {
    pub why: ManagerError,
    /// A timeout, cancellation or loss of the latest observation leaves the rest for another check.
    pub stop: bool,
}

impl From<ManagerError> for ActionError {
    fn from(why: ManagerError) -> Self {
        let stop = matches!(why, ManagerError::TimedOut | ManagerError::Cancelled);
        Self { why, stop }
    }
}

pub(crate) fn execute(
    manager: &dyn PluginManager,
    source: &RepoConfig,
    state: &mut State,
    action: &Action,
    latest: &mut NativeSnapshot,
    cancel: &AtomicBool,
) -> Result<(), ActionError> {
    if !action.ready(source, state, latest) {
        return Err(ManagerError::Failed("The plugin or marketplace changed in Copilot. Check again.".into()).into());
    }
    running(cancel)?;
    let command = match action.operation() {
        Operation::Register => manager.add_market(&action.origin().repo, &action.origin().branch, cancel),
        op @ (Operation::Install | Operation::Update | Operation::Uninstall) => {
            let name = action.qualified_name().expect("a plugin command has a qualified name");
            match op {
                Operation::Install => manager.install(&name, cancel),
                Operation::Update => manager.update(&name, cancel),
                _ => manager.uninstall(&name, cancel),
            }
        }
        Operation::Adopt | Operation::Approve | Operation::Forget => {
            return Ok(action.record_verified(state, latest).map_err(ManagerError::Failed)?);
        }
    };
    if let Err(why @ (ManagerError::TimedOut | ManagerError::Cancelled)) = command {
        return Err(why.into());
    }
    // Once a command returns it may have changed Copilot, even on failure. Finish the bounded
    // verification despite a late cancel; never proceed if we could not get a fresh observation.
    match snapshot(manager, &AtomicBool::new(false)) {
        Ok(after) => *latest = after,
        Err(why) => return Err(ActionError { why: command.err().unwrap_or(why), stop: true }),
    }
    command?;
    Ok(action.record_verified(state, latest).map_err(ManagerError::Failed)?)
}
