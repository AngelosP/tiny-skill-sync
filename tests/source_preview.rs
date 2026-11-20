mod common;

use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use common::{Calls, FakeRemote, World, cancel_flag, config, repo};
use tiny_skill_sync::config::RepoConfig;
use tiny_skill_sync::github::{FailureKind, RemoteError, RepoRef, confirm_source_at, discover_source, web_url};
use tiny_skill_sync::source::{Entry, PreviewDestination, SourceKind, resolve_destination};
use tiny_skill_sync::state::State;
use tiny_skill_sync::tree::SourceItems;

const REPO: &str = "team/Kit";

fn draft(kind: SourceKind, path: &str) -> RepoConfig {
    RepoConfig { kind, branch: "feature/team".into(), ..repo("draft", REPO, path) }
}

// Only the loose-layout test in this Windows integration binary uses these overrides.
struct Override(&'static str, Option<OsString>);

impl Override {
    fn new(key: &'static str, path: &Path) -> Self {
        let old = std::env::var_os(key);
        unsafe { std::env::set_var(key, path) };
        Self(key, old)
    }
}

impl Drop for Override {
    fn drop(&mut self) {
        unsafe {
            match &self.1 {
                Some(value) => std::env::set_var(self.0, value),
                None => std::env::remove_var(self.0),
            }
        }
    }
}

#[test]
fn loose_preview_uses_only_the_drafts_names_in_the_two_layouts_without_writing() {
    let w = World::new("source-placement");
    let agents = w.root.join("agents");
    let instructions = w.root.join("instructions");
    let _agents = Override::new("TSS_AGENTS_DIR", &agents);
    let _instructions = Override::new("TSS_INSTRUCTIONS_DIR", &instructions);
    let remote = FakeRemote::new();
    remote.seed(
        REPO,
        &[
            ("selected/Mixed #1.agent.md", "one"),
            ("selected/Two.agent.md", "two"),
            ("selected/Rust.instructions.md", "rule"),
            ("selected/nested/Inner.agent.md", "not immediate"),
            ("selected/other.md", "not an agent"),
            ("elsewhere/Wrong.agent.md", "not in selected"),
        ],
    );
    let mut engine = tiny_skill_sync::engine::Engine::new(
        tiny_skill_sync::config::Paths::new(w.home.clone(), w.skills.clone()),
        remote.client(),
        common::FakeTokens::none().provider(),
    );
    let cfg = config(vec![repo("other", REPO, "elsewhere")]);
    let source = draft(SourceKind::Agents, "selected");
    let mut preview = |source: &RepoConfig| {
        let before = w.snapshot();
        let stamp = engine.state_stamp();
        remote.reset();
        let result = engine.preview_source(&cfg, source, &cancel_flag()).unwrap();
        assert_eq!(remote.calls(), Calls { tree: 1, ..Default::default() }, "no conditional listing, pin, history or download");
        assert_eq!(remote.log(), [format!("tree {REPO}@feature/team:{}", source.path)]);
        assert_eq!(w.snapshot(), before, "no install, scratch folder, lock, settings or state write");
        assert_eq!(engine.state_stamp(), stamp);
        assert_eq!(engine.last_check(&source.id), None);
        result
    };

    let first = preview(&source);
    assert_eq!(first.kind, SourceKind::Agents);
    assert_eq!(first.names, ["Mixed #1.agent.md", "Two.agent.md"]);
    assert_eq!(first.destination, PreviewDestination::Resolved(agents.clone()));
    assert!(first.errors.is_empty());

    let rules = draft(SourceKind::Instructions, "selected");
    let rule = preview(&rules);
    assert_eq!(rule.names, ["Rust.instructions.md"]);
    assert_eq!(rule.destination, PreviewDestination::Resolved(instructions.clone()));
    let blocker = w.root.join("plain-file");
    fs::write(&blocker, "not a directory").unwrap();
    {
        let _blocked = Override::new("TSS_INSTRUCTIONS_DIR", &blocker.join("child").join("instructions"));
        assert!(matches!(preview(&rules).destination, PreviewDestination::Held(why) if why.contains("plain folder")));
    }
    let zero = preview(&draft(SourceKind::Instructions, "elsewhere"));
    assert_eq!(zero.count(), 0);
    assert!(zero.errors.is_empty());
    assert!(matches!(zero.destination, PreviewDestination::Held(why) if why.contains("no *.instructions.md")));

    let child = agents.join("Kit");
    fs::create_dir_all(&child).unwrap();
    fs::write(child.join("unrelated.agent.md"), "mine").unwrap();
    assert_eq!(preview(&source).destination, first.destination, "a repo folder is not a matching filename");
    fs::write(child.join("Mixed #1.agent.md"), "local edits, not proof of ownership").unwrap();
    let nested = preview(&source);
    fs::write(agents.join("Two.agent.md"), "mine too").unwrap();
    assert!(matches!(preview(&source).destination, PreviewDestination::Held(why) if why.contains("both")));

    let bound = resolve_destination(&rules, &instructions, &rule.names, None, |p| {
        Ok(if p == instructions || p == instructions.join("Kit") {
            Entry::Directory
        } else if p == instructions.join("Kit").join("Rust.instructions.md") {
            Entry::CompatibleFile
        } else {
            Entry::Missing
        })
    })
    .unwrap();
    let mut state = State::default();
    state.bind_destination(&rules, bound.clone()).unwrap();
    state.save(&w.state_file()).unwrap();
    assert_eq!(preview(&rules).destination, PreviewDestination::Resolved(bound.directory()), "an empty bound child does not relocate");
    fs::create_dir(&instructions).unwrap();
    fs::write(instructions.join("Rust.instructions.md"), "not in the recorded layout").unwrap();
    assert!(matches!(preview(&rules).destination, PreviewDestination::Held(why) if why.contains("outside the recorded")));

    remote.put(REPO, "selected/CON.instructions.md", "unsafe");
    let rejected = preview(&rules);
    drop(w);
    assert_eq!(nested.destination, PreviewDestination::Resolved(child));
    assert_eq!(rejected.names, rule.names);
    assert!(rejected.errors.iter().any(|e| e.contains("CON.instructions.md") && e.contains("reserved")));
    assert_eq!(rejected.destination, PreviewDestination::NotChecked);
}

#[test]
fn skills_preview_distinguishes_empty_from_failure_and_never_quarantines_state() {
    let w = World::new("source-read-only");
    let remote = FakeRemote::new();
    remote.seed_standard(REPO);
    let mut engine = w.engine(&remote);
    let cfg = config(vec![]);
    let source = draft(SourceKind::Skills, "skills");
    let before = w.snapshot();
    let preview = engine.preview_source(&cfg, &source, &cancel_flag()).unwrap();
    assert_eq!(preview.names, ["alpha", "beta", "gamma"]);
    assert_eq!(preview.destination, PreviewDestination::Resolved(w.skills.clone()));
    assert_eq!(engine.preview_source(&cfg, &draft(SourceKind::Skills, "skills/alpha"), &cancel_flag()).unwrap().names, ["alpha"]);
    assert_eq!(engine.preview_source(&cfg, &draft(SourceKind::Skills, "docs"), &cancel_flag()).unwrap().count(), 0);
    remote.set_truncated(REPO, true);
    assert!(engine.preview_source(&cfg, &source, &cancel_flag()).unwrap_err().to_string().contains("short"));
    remote.set_truncated(REPO, false);
    remote.fail_tree(REPO, Some(RemoteError::new(FailureKind::Server, "server unavailable")));
    assert!(engine.preview_source(&cfg, &source, &cancel_flag()).unwrap_err().to_string().contains("server unavailable"));
    remote.reset();
    assert!(engine.preview_source(&cfg, &source, &AtomicBool::new(true)).is_err());
    assert_eq!(remote.calls(), Calls::default());
    assert_eq!(w.snapshot(), before);

    remote.fail_tree(REPO, None);
    fs::write(w.state_file(), "{").unwrap();
    let before = w.snapshot();
    let preview = engine.preview_source(&cfg, &source, &cancel_flag()).unwrap();
    let after = w.snapshot();
    drop(w);
    assert!(matches!(preview.destination, PreviewDestination::Held(why) if why.contains("damaged")));
    assert_eq!(after, before);
}

#[test]
fn discovered_leaf_download_uses_its_spelling_once_at_the_pinned_commit() {
    let remote = FakeRemote::new();
    let source = draft(SourceKind::Agents, "folder & team");
    let path = "folder & team/Plan #1%.agent.md";
    remote.put(REPO, path, "before");
    let client = remote.client();
    let cancel = cancel_flag();
    let listing = discover_source(client.as_ref(), &source, 20, None, &cancel).unwrap();
    let SourceItems::Files(files) = listing.items() else { panic!("expected loose files") };
    assert_eq!(files.len(), 1);
    let f = &files[0];
    let reference = RepoRef::of(&source).unwrap();
    let pin = client.commit(&reference, None, &cancel).unwrap();
    assert!(confirm_source_at(client.as_ref(), &RepoConfig { path: "other".into(), ..source.clone() }, &listing, &pin, None, &cancel).is_err());
    assert_eq!(remote.calls().historical_tree, 0);
    remote.put(REPO, path, "after");
    let new_pin = client.commit(&reference, None, &cancel).unwrap();
    assert_eq!(confirm_source_at(client.as_ref(), &source, &listing, &new_pin, None, &cancel).unwrap_err().kind, FailureKind::Changed);
    confirm_source_at(client.as_ref(), &source, &listing, &pin, None, &cancel).unwrap();
    let mut bytes = vec![];
    client
        .file(&reference, &pin, &f.path, f.size, None, &cancel, &mut |chunk| {
            bytes.extend_from_slice(chunk);
            true
        })
        .unwrap();
    assert_eq!(bytes, b"before");
    assert_eq!(tiny_skill_sync::hash::blob_hex(&bytes), f.blob);
    assert_eq!(web_url(&source, Some(&f.path)).as_deref(), Some("https://github.com/team/Kit/blob/feature/team/folder%20%26%20team/Plan%20%231%25.agent.md"));
}
