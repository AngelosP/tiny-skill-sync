mod common;

use std::sync::atomic::AtomicBool;

use common::{FakePluginManager, PluginCall, PluginReply};
use tiny_skill_sync::plugins::native::{ManagerError, NativePlugin, NativeSnapshot, PluginManager};

#[test]
fn fake_plugin_manager_distinguishes_false_success_failure_and_real_snapshot_change() {
    let fake = FakePluginManager::new(r"D:\fake-copilot");
    let stopped = AtomicBool::new(false);
    let call = PluginCall::Install("one@tools".into());
    fake.push(call.clone(), PluginReply::Done(None));
    fake.install("one@tools", &stopped).unwrap();
    assert!(fake.list(&stopped).unwrap().is_empty());
    fake.push(call.clone(), PluginReply::Failed(ManagerError::TimedOut));
    assert_eq!(fake.install("one@tools", &stopped), Err(ManagerError::TimedOut));
    let plugin = NativePlugin { name: "one".into(), marketplace: "tools".into(), version: "2".into(), enabled: false, source: "installed".into() };
    fake.push(call.clone(), PluginReply::Done(Some(NativeSnapshot { home: fake.home().unwrap(), plugins: vec![plugin.clone()], markets: vec![] })));
    fake.install("one@tools", &stopped).unwrap();
    assert_eq!(fake.list(&stopped).unwrap(), [plugin]);
    assert_eq!(fake.calls(), [call.clone(), PluginCall::List, call.clone(), PluginCall::Home, call, PluginCall::List]);
    assert_eq!(fake.remaining(), 0);
}
