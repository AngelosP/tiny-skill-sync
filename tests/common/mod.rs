//! Shared by the integration tests: a throw-away world to work in, and a GitHub that lives in memory.
//!
//! The fake keeps the rules of the real service that the engine relies on:
//! * a listing carries an ETag that changes exactly when the listing changes;
//! * a file is served for one commit, and every change to a repository makes a new commit;
//! * a private repository answers "not found" to a caller without its token;
//! * a folder that does not exist (renamed, removed) answers "not found" instead of an empty listing.

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tiny_skill_sync::config::{Config, Paths, RepoConfig};
use tiny_skill_sync::engine::{Engine, LocalStatus, Outcome, RepoReport, SkillReport, SyncOptions, SyncReport, VerifyReport, Which};
use tiny_skill_sync::fsx::unique_suffix;
use tiny_skill_sync::github::{FailureKind, Remote, RemoteError, RepoRef, TokenProvider, TreeReply};
use tiny_skill_sync::hash::blob_hex;
use tiny_skill_sync::reconcile::LocalState;
use tiny_skill_sync::secret::Secret;
use tiny_skill_sync::state::State;
use tiny_skill_sync::tree::{EntryKind, Snapshot, TreeEntry};

/// The time the tests start at (2027-01-15). Tests move it forward by hand.
pub const T0: i64 = 1_800_000_000;

// ---------------------------------------------------------------------------------------------------------------------
// The world: a home folder, a skills folder and the work folder next to it, all deleted when the test ends.
// ---------------------------------------------------------------------------------------------------------------------

pub struct World {
    pub root: PathBuf,
    pub home: PathBuf,
    pub skills: PathBuf,
}

impl World {
    /// Lives in the temp folder, or in the folder named by `TSS_TEST_ROOT` (another drive, a network share) to run the
    /// same tests on a file system that has no change journal.
    pub fn new(name: &str) -> World {
        let base = std::env::var_os("TSS_TEST_ROOT").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let root = base.join(format!("tss-it-{name}-{}", unique_suffix()));
        let home = root.join("home");
        let skills = root.join("skills");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&skills).unwrap();
        World { root, home, skills }
    }

    pub fn paths(&self) -> Paths {
        Paths::new(self.home.clone(), self.skills.clone())
    }

    pub fn engine(&self, remote: &Arc<FakeRemote>) -> Engine {
        self.engine_with(remote, &FakeTokens::none())
    }

    pub fn engine_with(&self, remote: &Arc<FakeRemote>, tokens: &FakeTokens) -> Engine {
        Engine::new(self.paths(), remote.client(), tokens.provider())
    }

    /// An engine whose clock is the returned counter.
    pub fn engine_at(&self, remote: &Arc<FakeRemote>, tokens: &FakeTokens) -> (Engine, Arc<AtomicI64>) {
        let clock = Arc::new(AtomicI64::new(T0));
        let mut engine = self.engine_with(remote, tokens);
        let c = Arc::clone(&clock);
        engine.set_clock(move || c.load(Ordering::Relaxed));
        (engine, clock)
    }

    pub fn state_file(&self) -> PathBuf {
        self.paths().state_file()
    }

    pub fn work_dir(&self) -> PathBuf {
        self.paths().work_dir()
    }

    /// The recorded state, read from the file.
    pub fn state(&self) -> State {
        State::load(&self.state_file()).expect("state.json can be read").state
    }

    pub fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.skills.join(rel)).unwrap_or_else(|e| panic!("cannot read {rel}: {e}"))
    }

    pub fn write(&self, rel: &str, body: &str) {
        let path = self.skills.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    pub fn exists(&self, rel: &str) -> bool {
        self.skills.join(rel).exists()
    }

    pub fn remove(&self, rel: &str) {
        let path = self.skills.join(rel);
        if path.is_dir() { fs::remove_dir_all(path).unwrap() } else { fs::remove_file(path).unwrap() }
    }

    /// Every file below one skill folder: relative path (with `/`) to its text.
    pub fn files_of(&self, skill: &str) -> BTreeMap<String, String> {
        let base = self.skills.join(skill);
        let mut out = BTreeMap::new();
        walk(&base, &base, &mut out, false);
        out.into_iter().map(|(k, v)| (k, String::from_utf8_lossy(&v).into_owned())).collect()
    }

    /// Names of the folders directly in the skills folder, sorted.
    pub fn skill_names(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(&self.skills).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    /// Everything below the root: files with their bytes, folders as empty entries ending in `/`. Two equal snapshots
    /// prove that nothing was created, changed or removed in between.
    pub fn snapshot(&self) -> BTreeMap<String, Vec<u8>> {
        let mut out = BTreeMap::new();
        walk(&self.root, &self.root, &mut out, true);
        out
    }

    /// The same, for the skills folder only.
    pub fn skills_snapshot(&self) -> BTreeMap<String, Vec<u8>> {
        snapshot_of(&self.skills)
    }

    /// Folders the app keeps for what it replaced or could not remove safely.
    pub fn backups(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = fs::read_dir(self.work_dir())
            .map(|d| d.flatten().filter(|e| e.file_name().to_string_lossy().starts_with("backup-")).map(|e| e.path()).collect())
            .unwrap_or_default();
        out.sort();
        out
    }

    /// Everything in the work folder, for "nothing is left behind" checks.
    pub fn work_leftovers(&self) -> Vec<String> {
        let mut names: Vec<String> =
            fs::read_dir(self.work_dir()).map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
        names.sort();
        names
    }
}

impl Drop for World {
    fn drop(&mut self) {
        // A failed test leaves its folders behind so they can be looked at.
        if std::thread::panicking() {
            eprintln!("kept for inspection: {}", self.root.display());
        } else {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

/// Everything below `dir`, as `World::snapshot` gives it.
pub fn snapshot_of(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out, true);
    out
}

fn walk(dir: &Path, base: &Path, out: &mut BTreeMap<String, Vec<u8>>, with_dirs: bool) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let rel = path.strip_prefix(base).unwrap().to_string_lossy().replace('\\', "/");
        if path.is_dir() {
            if with_dirs {
                out.insert(format!("{rel}/"), Vec::new());
            }
            walk(&path, base, out, with_dirs);
        } else {
            out.insert(rel, fs::read(&path).unwrap_or_default());
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------------------------------------------------------

#[derive(Clone)]
pub struct FakeTokens {
    value: Option<String>,
    calls: Arc<AtomicUsize>,
}

impl FakeTokens {
    pub fn none() -> FakeTokens {
        FakeTokens { value: None, calls: Arc::default() }
    }

    pub fn some(value: &str) -> FakeTokens {
        FakeTokens { value: Some(value.to_string()), calls: Arc::default() }
    }

    pub fn provider(&self) -> Box<dyn TokenProvider> {
        Box::new(self.clone())
    }

    /// How often the engine asked for a token.
    pub fn asked(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

impl TokenProvider for FakeTokens {
    fn token(&self) -> Result<Secret, String> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        match &self.value {
            Some(v) => Ok(Secret::new(v.clone())),
            None => Err("no GitHub token is available".to_string()),
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// The fake GitHub
// ---------------------------------------------------------------------------------------------------------------------

/// What the engine asked of the fake, to prove what it did not do.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Calls {
    pub tree: usize,
    /// Listings that carried an ETag.
    pub conditional: usize,
    pub commit: usize,
    pub file: usize,
}

#[derive(Default)]
struct FakeRepo {
    files: BTreeMap<String, Vec<u8>>,
    commits: Vec<String>,
    history: BTreeMap<String, BTreeMap<String, Vec<u8>>>,
    private_token: Option<String>,
    /// Answers a listing without a token with this error, as GitHub does once its limit for anonymous requests is
    /// reached.
    anonymous_error: Option<RemoteError>,
    tree_error: Option<RemoteError>,
    commit_error: Option<RemoteError>,
    file_errors: BTreeMap<String, RemoteError>,
    lies: BTreeMap<String, Vec<u8>>,
    truncated: bool,
}

impl FakeRepo {
    fn new_commit(&mut self) {
        let id = format!("{:040x}", self.commits.len() + 1);
        self.history.insert(id.clone(), self.files.clone());
        self.commits.push(id);
    }

    fn head(&self) -> String {
        self.commits.last().cloned().unwrap_or_else(|| format!("{:040x}", 0))
    }
}

struct Inner {
    repos: BTreeMap<String, FakeRepo>,
    calls: Calls,
    log: Vec<String>,
    tokens: Vec<(String, Option<String>)>,
    chunk: usize,
}

type Hook = Box<dyn FnMut(&str) + Send>;

pub struct FakeRemote {
    inner: Mutex<Inner>,
    hook: Mutex<Option<Hook>>,
}

/// What the engine owns: a handle onto the fake the test keeps.
struct Client(Arc<FakeRemote>);

fn key(repo: &RepoRef) -> String {
    format!("{}/{}", repo.owner, repo.name)
}

fn not_found(what: &str) -> RemoteError {
    RemoteError::new(FailureKind::NotFound, format!("{what} was not found"))
}

fn listing(files: &BTreeMap<String, Vec<u8>>, folder: &str) -> Option<Vec<TreeEntry>> {
    let prefix = if folder.is_empty() { String::new() } else { format!("{folder}/") };
    let mut dirs = BTreeSet::new();
    let mut entries = Vec::new();
    for (path, body) in files {
        let Some(rel) = path.strip_prefix(&prefix) else { continue };
        let mut from = 0;
        while let Some(i) = rel[from..].find('/') {
            dirs.insert(rel[..from + i].to_string());
            from += i + 1;
        }
        entries.push(TreeEntry { path: rel.to_string(), kind: EntryKind::Blob, mode: "100644".into(), sha: blob_hex(body), size: Some(body.len() as u64) });
    }
    if entries.is_empty() && !folder.is_empty() {
        return None;
    }
    entries.extend(dirs.into_iter().map(|d| TreeEntry { path: d, kind: EntryKind::Tree, mode: "040000".into(), sha: "0".repeat(40), size: None }));
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Some(entries)
}

fn etag_of(entries: &[TreeEntry]) -> String {
    let text: String = entries.iter().map(|e| format!("{}:{}\n", e.path, e.sha)).collect();
    format!("\"{}\"", blob_hex(text.as_bytes()))
}

fn check_access(repo: &FakeRepo, token: Option<&Secret>, what: &str) -> Result<(), RemoteError> {
    match (&repo.private_token, token) {
        (None, _) => Ok(()),
        (Some(want), Some(t)) if t.expose() == want => Ok(()),
        (Some(_), Some(_)) => Err(RemoteError::new(FailureKind::Auth, "the token was rejected")),
        (Some(_), None) => Err(not_found(what)),
    }
}

impl FakeRemote {
    pub fn new() -> Arc<FakeRemote> {
        Arc::new(FakeRemote {
            inner: Mutex::new(Inner { repos: BTreeMap::new(), calls: Calls::default(), log: Vec::new(), tokens: Vec::new(), chunk: 7 }),
            hook: Mutex::new(None),
        })
    }

    /// What goes into `Engine::new`.
    pub fn client(self: &Arc<Self>) -> Box<dyn Remote> {
        Box::new(Client(Arc::clone(self)))
    }

    fn with<T>(&self, repo: &str, f: impl FnOnce(&mut FakeRepo) -> T) -> T {
        let mut g = self.inner.lock().unwrap();
        f(g.repos.entry(repo.to_string()).or_default())
    }

    // ----- what the test does to the "upstream" ---------------------------------------------------------------------

    /// Adds or replaces a file. Every call is one commit.
    pub fn put(&self, repo: &str, path: &str, body: &str) {
        self.put_bytes(repo, path, body.as_bytes());
    }

    pub fn put_bytes(&self, repo: &str, path: &str, body: &[u8]) {
        self.with(repo, |r| {
            r.files.insert(path.to_string(), body.to_vec());
            r.new_commit();
        });
    }

    /// Adds many files in one commit.
    pub fn seed(&self, repo: &str, files: &[(&str, &str)]) {
        self.with(repo, |r| {
            for (path, body) in files {
                r.files.insert((*path).to_string(), body.as_bytes().to_vec());
            }
            r.new_commit();
        });
    }

    /// Removes a file, or every file below a folder.
    pub fn remove(&self, repo: &str, path: &str) {
        self.with(repo, |r| {
            let folder = format!("{path}/");
            r.files.retain(|p, _| p != path && !p.starts_with(&folder));
            r.new_commit();
        });
    }

    /// The tree the live fixture repositories have, under `skills/`.
    pub fn seed_standard(&self, repo: &str) {
        self.seed(repo, STANDARD);
    }

    pub fn make_private(&self, repo: &str, token: &str) {
        self.with(repo, |r| r.private_token = Some(token.to_string()));
    }

    pub fn make_public(&self, repo: &str) {
        self.with(repo, |r| r.private_token = None);
    }

    /// Fails only the listings asked for without a token.
    pub fn fail_anonymous(&self, repo: &str, error: Option<RemoteError>) {
        self.with(repo, |r| r.anonymous_error = error);
    }

    pub fn fail_tree(&self, repo: &str, error: Option<RemoteError>) {
        self.with(repo, |r| r.tree_error = error);
    }

    pub fn fail_commit(&self, repo: &str, error: Option<RemoteError>) {
        self.with(repo, |r| r.commit_error = error);
    }

    /// `path` is the path in the repository, for example `skills/alpha/SKILL.md`.
    pub fn fail_file(&self, repo: &str, path: &str, error: Option<RemoteError>) {
        self.with(repo, |r| match error {
            Some(e) => {
                r.file_errors.insert(path.to_string(), e);
            }
            None => {
                r.file_errors.remove(path);
            }
        });
    }

    /// Serves other bytes for a file than the listing promises (what a push between listing and download looks like).
    pub fn lie_about(&self, repo: &str, path: &str, body: &str) {
        self.with(repo, |r| {
            r.lies.insert(path.to_string(), body.as_bytes().to_vec());
        });
    }

    pub fn set_truncated(&self, repo: &str, on: bool) {
        self.with(repo, |r| r.truncated = on);
    }

    /// Size of the pieces files are served in. Small by default, so that every file arrives in several pieces.
    pub fn set_chunk(&self, bytes: usize) {
        self.inner.lock().unwrap().chunk = bytes;
    }

    /// Runs before every call, with its log line, and may change the fake (the lock is not held meanwhile).
    pub fn on_call(&self, hook: impl FnMut(&str) + Send + 'static) {
        *self.hook.lock().unwrap() = Some(Box::new(hook));
    }

    pub fn clear_hook(&self) {
        *self.hook.lock().unwrap() = None;
    }

    // ----- what the test learns -------------------------------------------------------------------------------------

    pub fn calls(&self) -> Calls {
        self.inner.lock().unwrap().calls.clone()
    }

    /// One line per call: `tree o/a@main:skills`, `commit o/a`, `file o/a skills/alpha/SKILL.md`.
    pub fn log(&self) -> Vec<String> {
        self.inner.lock().unwrap().log.clone()
    }

    /// The token each call carried, by call kind.
    pub fn tokens_seen(&self) -> Vec<(String, Option<String>)> {
        self.inner.lock().unwrap().tokens.clone()
    }

    /// Forgets the counters, the log and the tokens seen so far.
    pub fn reset(&self) {
        let mut g = self.inner.lock().unwrap();
        g.calls = Calls::default();
        g.log.clear();
        g.tokens.clear();
    }

    pub fn head(&self, repo: &str) -> String {
        self.with(repo, |r| r.head())
    }

    fn note(&self, kind: &str, line: String, token: Option<&Secret>, bump: impl FnOnce(&mut Calls)) {
        {
            let mut g = self.inner.lock().unwrap();
            bump(&mut g.calls);
            g.log.push(line.clone());
            g.tokens.push((kind.to_string(), token.map(|t| t.expose().to_string())));
        }
        // The hook runs without any lock held, so it may call back into the fake.
        let taken = self.hook.lock().unwrap().take();
        if let Some(mut hook) = taken {
            hook(&line);
            let mut slot = self.hook.lock().unwrap();
            if slot.is_none() {
                *slot = Some(hook);
            }
        }
    }
}

impl Remote for Client {
    fn tree(&self, repo: &RepoRef, etag: Option<&str>, token: Option<&Secret>, cancel: &AtomicBool) -> Result<TreeReply, RemoteError> {
        let fake = &self.0;
        fake.note("tree", format!("tree {}/{}@{}:{}", repo.owner, repo.name, repo.branch, repo.path), token, |c| {
            c.tree += 1;
            if etag.is_some() {
                c.conditional += 1;
            }
        });
        if cancel.load(Ordering::Relaxed) {
            return Err(RemoteError::new(FailureKind::Cancelled, "cancelled"));
        }
        let g = fake.inner.lock().unwrap();
        let r = g.repos.get(&key(repo)).ok_or_else(|| not_found(&repo.label()))?;
        check_access(r, token, &repo.label())?;
        if let Some(e) = r.anonymous_error.as_ref().filter(|_| token.is_none()) {
            return Err(e.clone());
        }
        if let Some(e) = &r.tree_error {
            return Err(e.clone());
        }
        let entries = listing(&r.files, repo.path).ok_or_else(|| not_found(&format!("{} : {}", repo.label(), repo.path)))?;
        let current = etag_of(&entries);
        if etag == Some(current.as_str()) {
            return Ok(TreeReply::NotModified);
        }
        Ok(TreeReply::Snapshot { etag: Some(current.clone()), snapshot: Snapshot { sha: current, truncated: r.truncated, entries } })
    }

    fn commit(&self, repo: &RepoRef, token: Option<&Secret>, cancel: &AtomicBool) -> Result<String, RemoteError> {
        let fake = &self.0;
        fake.note("commit", format!("commit {}/{}", repo.owner, repo.name), token, |c| c.commit += 1);
        if cancel.load(Ordering::Relaxed) {
            return Err(RemoteError::new(FailureKind::Cancelled, "cancelled"));
        }
        let g = fake.inner.lock().unwrap();
        let r = g.repos.get(&key(repo)).ok_or_else(|| not_found(&repo.label()))?;
        check_access(r, token, &repo.label())?;
        if let Some(e) = &r.commit_error {
            return Err(e.clone());
        }
        Ok(r.head())
    }

    fn file(
        &self,
        repo: &RepoRef,
        commit: &str,
        rel: &str,
        _expected_size: u64,
        token: Option<&Secret>,
        cancel: &AtomicBool,
        sink: &mut dyn FnMut(&[u8]) -> bool,
    ) -> Result<(), RemoteError> {
        let fake = &self.0;
        let path = if repo.path.is_empty() { rel.to_string() } else { format!("{}/{rel}", repo.path) };
        fake.note("file", format!("file {}/{} {path}", repo.owner, repo.name), token, |c| c.file += 1);
        let (body, chunk) = {
            let g = fake.inner.lock().unwrap();
            let r = g.repos.get(&key(repo)).ok_or_else(|| not_found(&repo.label()))?;
            check_access(r, token, &repo.label())?;
            if let Some(e) = r.file_errors.get(&path) {
                return Err(e.clone());
            }
            let snapshot = r.history.get(commit).ok_or_else(|| not_found(&format!("commit {commit}")))?;
            let real = snapshot.get(&path).ok_or_else(|| not_found(&format!("{path} at {commit}")))?;
            (r.lies.get(&path).cloned().unwrap_or_else(|| real.clone()), g.chunk.max(1))
        };
        for piece in body.chunks(chunk) {
            if cancel.load(Ordering::Relaxed) {
                return Err(RemoteError::new(FailureKind::Cancelled, "cancelled"));
            }
            if !sink(piece) {
                return Err(RemoteError::new(FailureKind::Cancelled, "stopped"));
            }
        }
        Ok(())
    }
}

/// What `skills/alpha/SKILL.md` says in `STANDARD`, and what the tests change it to.
pub const ALPHA_V1: &str = "---\nname: alpha\ndescription: first skill\n---\n# Alpha v1\n";
pub const ALPHA_V2: &str = "---\nname: alpha\ndescription: first skill\n---\n# Alpha v2\n";

/// The repository the live fixtures have: three skills and a folder that is not one, under `skills/`.
pub const STANDARD: &[(&str, &str)] = &[
    ("README.md", "# fixtures\n"),
    ("docs/notes.md", "notes\n"),
    ("skills/alpha/SKILL.md", ALPHA_V1),
    ("skills/alpha/references/guide.md", "guide v1\n"),
    ("skills/alpha/scripts/hello.ps1", "Write-Output 'hello'\n"),
    ("skills/beta/SKILL.md", "---\nname: beta\ndescription: second skill\n---\n# Beta v1\n"),
    ("skills/gamma/SKILL.md", "---\nname: gamma\ndescription: third skill\n---\n# Gamma v1\n"),
    ("skills/not-a-skill/readme.txt", "this folder has no SKILL.md\n"),
];

// ---------------------------------------------------------------------------------------------------------------------
// Settings and reports
// ---------------------------------------------------------------------------------------------------------------------

pub fn repo(id: &str, name: &str, path: &str) -> RepoConfig {
    RepoConfig { id: id.into(), repo: name.into(), path: path.into(), ..RepoConfig::default() }
}

pub fn config(repos: Vec<RepoConfig>) -> Config {
    Config { repos, ..Config::default() }
}

/// One repository, `o/fixtures`, id `fx`, folder `skills`.
pub fn standard_config() -> Config {
    config(vec![repo("fx", "o/fixtures", "skills")])
}

pub fn sync_all(engine: &mut Engine, cfg: &Config) -> SyncReport {
    sync_with(engine, cfg, SyncOptions::default())
}

pub fn sync_with(engine: &mut Engine, cfg: &Config, opts: SyncOptions) -> SyncReport {
    engine.sync(cfg, &Which::All, &opts, &AtomicBool::new(false)).expect("the check could run")
}

pub fn sync_which(engine: &mut Engine, cfg: &Config, which: &Which) -> SyncReport {
    engine.sync(cfg, which, &SyncOptions::default(), &AtomicBool::new(false)).expect("the check could run")
}

/// The one repository a report covers.
pub fn only(report: &SyncReport) -> &RepoReport {
    assert_eq!(report.repos.len(), 1, "expected exactly one repository in {report:?}");
    &report.repos[0]
}

pub fn skill<'a>(report: &'a RepoReport, name: &str) -> &'a SkillReport {
    report
        .skills
        .iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("no skill '{name}' in {:?}", report.skills.iter().map(|s| &s.name).collect::<Vec<_>>()))
}

pub fn cancel_flag() -> AtomicBool {
    AtomicBool::new(false)
}

/// What a start-up or verification said about one skill.
pub fn status_of(report: &VerifyReport, name: &str) -> LocalStatus {
    report.skills.iter().find(|k| k.name == name).unwrap_or_else(|| panic!("no skill '{name}' in {:?}", report.skills)).status.clone()
}

pub fn clean() -> LocalStatus {
    LocalStatus::Checked(LocalState::Clean)
}

/// The repository name the standard setup uses.
pub const REPO: &str = "o/fixtures";

/// A world, a fake GitHub with the standard repository, one configured repository and an engine whose clock the
/// test moves by hand.
pub struct Setup {
    pub w: World,
    pub remote: Arc<FakeRemote>,
    pub cfg: Config,
    pub engine: Engine,
    pub clock: Arc<AtomicI64>,
    pub tokens: FakeTokens,
}

impl Setup {
    /// Everything ready, nothing installed yet.
    pub fn fresh(name: &str) -> Setup {
        let w = World::new(name);
        let remote = FakeRemote::new();
        remote.seed_standard(REPO);
        let tokens = FakeTokens::none();
        let (engine, clock) = w.engine_at(&remote, &tokens);
        Setup { w, remote, cfg: standard_config(), engine, clock, tokens }
    }

    /// The first check has run: alpha, beta and gamma are installed. The fake's counters start from zero again.
    pub fn installed(name: &str) -> Setup {
        let mut s = Setup::fresh(name);
        let report = s.sync();
        assert!(matches!(only(&report).outcome, Outcome::Synced), "setup did not install cleanly: {report:?}");
        s.remote.reset();
        s
    }

    pub fn sync(&mut self) -> SyncReport {
        sync_all(&mut self.engine, &self.cfg)
    }

    pub fn advance(&self, secs: i64) {
        self.clock.fetch_add(secs, Ordering::Relaxed);
    }

    /// A second engine on the same folders and the same clock, as if the app had been restarted.
    pub fn restarted(&self) -> Engine {
        let mut engine = self.w.engine_with(&self.remote, &self.tokens);
        let clock = Arc::clone(&self.clock);
        engine.set_clock(move || clock.load(Ordering::Relaxed));
        engine
    }
}
