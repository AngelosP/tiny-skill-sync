use super::*;
use crate::reconcile::{Hold, LocalState, Skip};
use crate::source::SourceKind;
use std::fs;

const NAME: &str = "review.agent.md";
const PIN: &str = "abababababababababababababababababababab";
const ONE: &[u8] = b"review\none\n";
const TWO: &[u8] = b"review\ntwo\n";
const MINE: &[u8] = b"my local notes\n";

fn file(name: &str, bytes: &[u8]) -> SkillFile {
    SkillFile { path: name.into(), blob: blob_hex(bytes), size: bytes.len() as u64 }
}

fn listing(f: &SkillFile) -> LeafListing<'_> {
    LeafListing::listed(f)
}

struct World {
    base: PathBuf,
    root: PathBuf,
    source: RepoConfig,
    binding: DestinationBinding,
    a: Applier,
    state: State,
}

impl World {
    fn new() -> Self {
        let base = crate::apply::fixture_root("leaf");
        let root = base.join("agents");
        let a = Applier::new(base.join("skills"), base.join(".tss-work"), base.join("home").join("state.json"), 42);
        let source = RepoConfig { repo: "fixture/one".into(), id: "first".into(), kind: SourceKind::Agents, ..Default::default() };
        let binding = resolve_destination(&source, &root, &[NAME.into()], None, |_| Ok(Entry::Missing)).unwrap();
        let mut state = State::default();
        state.bind_destination(&source, binding.clone()).unwrap();
        Self { base, root, source, binding, a, state }
    }

    fn write(&self, bytes: &[u8]) {
        fs::create_dir_all(&self.root).unwrap();
        fs::write(self.root.join(NAME), bytes).unwrap();
    }

    fn read(&self) -> Vec<u8> {
        fs::read(self.root.join(NAME)).unwrap()
    }

    fn run(&mut self, list: LeafListing, pin: Option<&PinnedLeaf>, expected: &Verdict) -> Result<Applied> {
        let t = LeafTarget { source: &self.source, root: &self.root, binding: &self.binding, other_claim: None };
        self.a.apply_leaf(&mut self.state, t, list, pin, expected)
    }

    fn put(&mut self, bytes: &[u8], expected: Verdict) -> Result<Applied> {
        let f = file(NAME, bytes);
        let p = PinnedLeaf::new(&f, PIN, bytes)?;
        self.run(listing(&f), Some(&p), &expected)
    }

    fn recover(&mut self) -> Result<Applied> {
        self.a.hook = None;
        self.state = State::load_sources_with(&self.a.state_path, false).unwrap().state;
        let t = LeafTarget { source: &self.source, root: &self.root, binding: &self.binding, other_claim: None };
        self.a.recover_leaf(&mut self.state, t, NAME)
    }

    fn crash(&mut self, at: Step) {
        self.a.hook = Some(Box::new(move |s| if s == at { Err(local("fixture interruption")) } else { Ok(()) }));
    }

    fn work_files(&self) -> (PathBuf, PathBuf) {
        let txn = &self.state.loose_sources()["first"].leaf(NAME).unwrap().pending.as_ref().unwrap().txn;
        let w = self.base.join(WORK);
        (w.join(format!("leaf-{txn}.new")), w.join(format!("backup-leaf-{txn}.old")))
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = crate::fsx::remove_dir_all_retry(&self.base);
    }
}

#[test]
fn adopt_update_and_remove_only_the_verified_file() {
    use std::os::windows::fs::OpenOptionsExt;
    let mut w = World::new();
    w.write(b"review\r\n\r\none");
    let neighbor = w.root.join("hand-written.agent.md");
    fs::write(&neighbor, MINE).unwrap();
    // Not even a read or a carry of the user's file can succeed.
    let locked_neighbor = fs::OpenOptions::new().read(true).share_mode(0).open(&neighbor).unwrap();
    let f = file(NAME, ONE);
    let pin = PinnedLeaf::new(&f, PIN, ONE).unwrap();
    assert!(matches!(PinnedLeaf::new(&f, PIN, b"review\r\n\r\none"), Err(e) if e.kind == crate::github::FailureKind::Changed));
    assert!(matches!(PinnedLeaf::new(&f, PIN, TWO), Err(e) if e.kind == crate::github::FailureKind::Changed));
    assert_eq!(entry(&w.root.join(NAME), std::slice::from_ref(pin.file()), &mut w.a.hasher).unwrap(), Entry::CompatibleFile);
    assert_eq!(w.put(ONE, Verdict::Adopt).unwrap(), Applied::Done);
    assert_eq!(w.read(), b"review\r\n\r\none");
    let before = w.state.clone();
    w.write(MINE);
    assert!(matches!(w.put(TWO, Verdict::Update), Ok(Applied::Held(_))));
    assert!(matches!(w.run(LeafListing::confirmed_removal(NAME), None, &Verdict::Delete), Ok(Applied::Held(_))));
    assert_eq!(w.read(), MINE);
    assert_eq!(w.state, before);
    w.write(b"\nreview\r\none");
    assert_eq!(w.put(TWO, Verdict::Update).unwrap(), Applied::Done);
    assert_eq!(w.read(), TWO);
    w.write(b"review\r\ntwo\n\n");
    let source = w.state.loose_sources()["first"].recorded_source("first").unwrap();
    let snapshot = |truncated| crate::tree::Snapshot { sha: PIN.into(), truncated, entries: Vec::new() };
    assert!(crate::tree::discover_source(&source, snapshot(true), 0).is_err());
    let discovery = crate::tree::discover_source(&source, snapshot(false), 0).unwrap();
    assert!(LeafListing::from_discovery(NAME, &discovery, &RepoConfig { path: "other".into(), ..source.clone() }, true).is_err());
    let empty = LeafListing::from_discovery(NAME, &discovery, &source, false).unwrap();
    let t = LeafTarget { source: &w.source, root: &w.root, binding: &w.binding, other_claim: None };
    assert_eq!(w.a.leaf_decision(&w.state, t, empty, None).unwrap(), Verdict::Held(Hold::MassDeletion));
    assert!(matches!(w.run(empty, None, &Verdict::Delete), Ok(Applied::Held(_))));
    let confirmed = LeafListing::from_discovery(NAME, &discovery, &source, true).unwrap();
    assert_eq!(w.run(confirmed, None, &Verdict::Delete).unwrap(), Applied::Done);
    assert!(!w.root.join(NAME).exists());
    assert!(w.state.loose_sources()["first"].leaves().is_empty());
    drop(locked_neighbor);
    assert_eq!(fs::read(neighbor).unwrap(), MINE);
}

#[test]
fn restart_rolls_back_or_finishes_by_evidence_not_by_work_names() {
    for step in [Step::Marked, Step::OldMoved, Step::NewInPlace] {
        let mut w = World::new();
        assert_eq!(w.put(ONE, Verdict::Install).unwrap(), Applied::Done);
        w.crash(step);
        assert!(w.put(TWO, Verdict::Update).is_err());
        assert!(State::load(&w.a.state_path).is_err());
        assert!(w.state.loose_sources()["first"].leaf(NAME).unwrap().interrupted());
        if step == Step::NewInPlace {
            assert_eq!(w.recover().unwrap(), Applied::Done);
            assert_eq!(w.read(), TWO);
        } else {
            assert!(matches!(w.recover(), Ok(Applied::Held(_))));
            assert_eq!(w.read(), ONE);
        }
        assert!(!w.state.loose_sources()["first"].leaf(NAME).unwrap().interrupted());
        assert_eq!(crate::verify::leaf::status(&w.state.loose_sources()["first"], &w.binding, NAME, &mut w.a.hasher).unwrap(), LocalState::Clean);
    }
}

#[test]
fn late_edits_to_either_name_and_a_recreated_live_file_survive() {
    for slot in ["backup", "reformatted-backup", "live", "recreated", "stage"] {
        let mut w = World::new();
        assert_eq!(w.put(ONE, Verdict::Install).unwrap(), Applied::Done);
        w.crash(if slot == "stage" || slot == "recreated" { Step::OldMoved } else { Step::NewInPlace });
        assert!(w.put(TWO, Verdict::Update).is_err());
        let (stage, backup) = w.work_files();
        match slot {
            "backup" => fs::write(&backup, MINE).unwrap(),
            "reformatted-backup" => fs::write(&backup, b"review\r\none").unwrap(),
            "stage" => fs::write(&stage, MINE).unwrap(),
            _ => w.write(MINE),
        }
        assert!(matches!(w.recover(), Ok(Applied::Held(_))));
        if slot == "reformatted-backup" {
            assert_eq!(w.read(), b"review\r\none", "the journal describes the old object's exact bytes, not its text equivalence class");
        } else if slot == "stage" {
            assert_eq!(fs::read(stage).unwrap(), MINE);
        } else {
            assert_eq!(w.read(), MINE);
        }
        if !matches!(slot, "backup" | "reformatted-backup") {
            assert_eq!(fs::read(backup).unwrap(), ONE);
            assert!(w.state.loose_sources()["first"].leaf(NAME).unwrap().interrupted());
        }
    }
}

#[test]
fn pending_first_install_reserves_the_physical_name_even_without_settings() {
    let mut w = World::new();
    w.crash(Step::Marked);
    assert!(w.put(ONE, Verdict::Install).is_err());
    let (stage, _) = w.work_files();
    let second = RepoConfig { repo: "fixture/two".into(), id: "other".into(), ..w.source.clone() };
    let upper = "REVIEW.agent.md".to_string();
    let root = w.root.with_file_name("Agents");
    let binding = resolve_destination(&second, &root, std::slice::from_ref(&upper), None, |_| Ok(Entry::Missing)).unwrap();
    w.state.bind_destination(&second, binding.clone()).unwrap();
    let second_target = LeafTarget { source: &second, root: &root, binding: &binding, other_claim: None };
    let f = file(&upper, ONE);
    assert_eq!(w.a.leaf_decision(&w.state, second_target, listing(&f), None).unwrap(), Verdict::Skipped(Skip::Collision { owner: "first".into() }));
    assert!(w.state.forget_loose_source(&w.source).is_err());
    // A skills sweeper cannot consume any part of an interrupted loose transaction.
    fs::create_dir_all(w.a.work_dir.join("stage-fixture")).unwrap();
    w.a.sweep(&w.state);
    assert!(w.a.work_dir.join("stage-fixture").is_dir());
    assert_eq!(fs::read(stage).unwrap(), ONE);
}

#[test]
fn missing_unmanaged_and_catch_up_never_overwrite_local_content() {
    let mut w = World::new();
    w.write(MINE);
    assert!(matches!(w.put(ONE, Verdict::Install), Ok(Applied::Held(_))));
    assert!(matches!(w.put(ONE, Verdict::Adopt), Ok(Applied::Held(_))));
    assert_eq!(w.read(), MINE);
    fs::remove_file(w.root.join(NAME)).unwrap();
    assert!(matches!(w.put(ONE, Verdict::Adopt), Ok(Applied::Held(_))));
    assert!(!w.root.join(NAME).exists());
    assert_eq!(w.put(ONE, Verdict::Install).unwrap(), Applied::Done);
    w.write(b"review\r\n\r\ntwo");
    assert_eq!(w.put(TWO, Verdict::CatchUp).unwrap(), Applied::Done);
    assert_eq!(w.read(), b"review\r\n\r\ntwo");
    fs::remove_file(w.root.join(NAME)).unwrap();
    assert!(matches!(w.put(ONE, Verdict::Update), Ok(Applied::Held(_))));
    assert!(matches!(w.run(LeafListing::confirmed_removal(NAME), None, &Verdict::Delete), Ok(Applied::Held(_))));
    assert!(w.state.loose_sources()["first"].leaf(NAME).is_some());
}

#[test]
fn changed_binding_directory_or_work_and_hard_link_are_not_authority() {
    let mut w = World::new();
    assert_eq!(w.put(ONE, Verdict::Install).unwrap(), Applied::Done);
    let f = file(NAME, TWO);
    let original = LeafTarget { source: &w.source, root: &w.root, binding: &w.binding, other_claim: None };
    let moved = w.base.join("moved");
    assert!(w.a.leaf_decision(&w.state, LeafTarget { root: &moved, ..original }, listing(&f), None).is_err());
    let neighbor = w.root.join("my-other-name.md");
    fs::hard_link(w.root.join(NAME), &neighbor).unwrap();
    assert!(w.put(TWO, Verdict::Update).is_err());
    assert_eq!(fs::read(&neighbor).unwrap(), ONE);
    fs::remove_file(neighbor).unwrap();
    fs::rename(&w.root, &moved).unwrap();
    w.write(ONE);
    assert!(w.put(TWO, Verdict::Update).is_err());
    assert_eq!(w.read(), ONE);
    fs::rename(&w.root, w.base.join("spare")).unwrap();
    fs::rename(moved, &w.root).unwrap();
    w.crash(Step::OldMoved);
    assert!(w.put(TWO, Verdict::Update).is_err());
    let (_, backup) = w.work_files();
    let misplaced = w.base.join("original-work");
    fs::rename(w.base.join(WORK), &misplaced).unwrap();
    fs::create_dir(w.base.join(WORK)).unwrap();
    assert!(w.recover().is_err());
    assert!(!w.root.join(NAME).exists());
    assert_eq!(fs::read(misplaced.join(backup.file_name().unwrap())).unwrap(), ONE);
}

#[test]
fn an_empty_destination_is_pinned_through_first_install() {
    let mut w = World::new();
    let root = w.root.clone();
    let moved = w.base.join("moved");
    w.a.hook = Some(Box::new(move |step| {
        if step == Step::Marked {
            assert!(fs::rename(&root, &moved).is_err(), "the destination itself must remain pinned");
        }
        Ok(())
    }));
    assert_eq!(w.put(ONE, Verdict::Install).unwrap(), Applied::Done);
    assert_eq!(w.read(), ONE);
}

#[test]
fn interrupted_removal_does_not_dispose_of_a_replacement_or_late_edit() {
    for different_object in [false, true] {
        let mut w = World::new();
        assert_eq!(w.put(ONE, Verdict::Install).unwrap(), Applied::Done);
        w.crash(Step::OldMoved);
        assert!(w.run(LeafListing::confirmed_removal(NAME), None, &Verdict::Delete).is_err());
        let (_, backup) = w.work_files();
        if different_object {
            fs::rename(&backup, w.base.join("saved-original")).unwrap();
            fs::write(&backup, ONE).unwrap(); // same bytes, not the original object
            assert!(matches!(w.recover(), Ok(Applied::Held(_))));
            assert_eq!(fs::read(backup).unwrap(), ONE);
            assert!(w.state.loose_sources()["first"].leaf(NAME).unwrap().interrupted());
        } else {
            fs::write(&backup, MINE).unwrap();
            assert!(matches!(w.recover(), Ok(Applied::Held(_))));
            assert_eq!(w.read(), MINE);
            assert!(!w.state.loose_sources()["first"].leaf(NAME).unwrap().interrupted());
        }
    }
}

#[test]
fn continuation_a_committed_leaf_change_reports_a_backup_it_could_not_retire() {
    let mut w = World::new();
    assert_eq!(w.put(ONE, Verdict::Install).unwrap(), Applied::Done);
    let work = w.base.join(WORK);
    w.a.hook = Some(Box::new(move |step| {
        if step == Step::NewInPlace {
            let old = fs::read_dir(&work).unwrap().map(|e| e.unwrap().path()).find(|p| p.extension().is_some_and(|ext| ext == "old")).unwrap();
            let mut permissions = fs::metadata(&old).unwrap().permissions();
            permissions.set_readonly(true);
            fs::set_permissions(old, permissions).unwrap();
        }
        Ok(())
    }));
    let result = w.put(TWO, Verdict::Update);
    assert_eq!(w.read(), TWO);
    let state = State::load_sources_with(&w.a.state_path, false).unwrap().state;
    assert!(!state.loose_sources()["first"].leaf(NAME).unwrap().interrupted());
    let Ok(Applied::Displaced { backup, .. }) = result else { panic!("replacement was committed, but reported {result:?}") };
    assert_eq!(fs::read(backup).unwrap(), ONE);
}

#[test]
fn continuation_a_foreign_work_folder_is_not_used_for_private_drafts() {
    let mut w = World::new();
    let work = w.base.join(WORK);
    fs::create_dir(&work).unwrap();
    fs::write(work.join("personal.txt"), MINE).unwrap();
    let result = w.put(ONE, Verdict::Install);
    assert!(result.is_err(), "an existing, unowned work folder was used: {result:?}");
    assert!(!w.root.join(NAME).exists());
    assert_eq!(fs::read(work.join("personal.txt")).unwrap(), MINE);
    assert_eq!(fs::read_dir(work).unwrap().count(), 1);
}
