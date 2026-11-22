//! App releases are separate from skills. Only the worker owns a pending pair; a record on disk cannot authorize a future install.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::Instant;

use crate::config::{Paths, UpdatePrefs, UpdateTiming};
use crate::github::SystemTokens;
use crate::interval::now_secs;
use crate::release::{Feed, Offer, Official, ReleaseError, VerifiedPair, Version};
use crate::replacement::{self, VerifiedTray};
use crate::update_clock::ClockState;

/// Debug builds only: the next Install skips the network and the replacement and offers to restart into the tray that
/// is installed now, so a script can watch the handoff. 1 proves the right digest, 2 a wrong one.
#[cfg(debug_assertions)]
pub static TEST_RESTART: AtomicU8 = AtomicU8::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Check,
    Install,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Request {
    Manual(Action),
    Scheduled,
    Quit,
}

pub(crate) struct Locations<'a> {
    pub home: &'a Path,
    pub dir: &'a Path,
    pub skills: &'a Path,
}

// resolve missing leaves too: a first run need not have created its skills or work folder yet.
fn resolved(path: &Path) -> Result<PathBuf, String> {
    match fs::canonicalize(path) {
        Ok(path) => Ok(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && path.is_absolute() => {
            let parent = path.parent().zip(path.file_name()).ok_or_else(|| format!("cannot locate {}", path.display()))?;
            Ok(resolved(parent.0)?.join(parent.1))
        }
        Err(e) => Err(format!("cannot locate {}: {e}", path.display())),
    }
}

impl Locations<'_> {
    pub(crate) fn check(&self) -> Result<(), String> {
        let folded = |p: &Path| -> Result<PathBuf, String> {
            let path = resolved(p)?;
            Ok(PathBuf::from(path.to_str().ok_or("cannot compare the app and skills folder names")?.to_lowercase()))
        };
        let skills = folded(self.skills)?;
        let work = folded(&Paths::new(self.home.into(), self.skills.into()).work_dir())?;
        for dir in [self.dir, self.home] {
            let dir = folded(dir)?;
            if protected(&dir, &skills)? || protected(&dir, &work)? {
                return Err("keep the executables and app settings outside the skills and skill work folders".into());
            }
        }
        Ok(())
    }
}

// Two drive letters or UNC names may address the same filesystem objects.
fn protected(target: &Path, root: &Path) -> Result<bool, String> {
    if target.starts_with(root) {
        return Ok(true);
    }
    if target.components().next() == root.components().next() {
        return Ok(false);
    }
    fn existing(path: &Path) -> Result<Option<crate::win::install::FileId>, String> {
        match crate::win::install::Directory::pin(path) {
            Ok(d) => d.id().map(Some).map_err(|e| e.to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("cannot identify {}: {e}", path.display())),
        }
    }
    for ancestor in root.ancestors() {
        if let Some(id) = existing(ancestor)? {
            let suffix = root.strip_prefix(ancestor).expect("ancestor");
            for other in target.ancestors() {
                if target.strip_prefix(other).expect("ancestor").starts_with(suffix) && existing(other)? == Some(id) {
                    return Ok(true);
                }
            }
            return Ok(false);
        }
    }
    Err("cannot identify the protected folder".into())
}

/// Closing and entering use the same atomic state: once shutdown sees no transaction, none can start.
#[derive(Clone, Default)]
pub(crate) struct Transactions(Arc<AtomicU8>);

impl Transactions {
    fn enter(&self) -> Result<InTransaction<'_>, String> {
        self.0.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire).map_err(|_| "app update cancelled during shutdown")?;
        Ok(InTransaction(self))
    }

    pub fn close(&self) -> bool {
        self.0.fetch_or(2, Ordering::AcqRel) & 1 != 0
    }
}

struct InTransaction<'a>(&'a Transactions);
impl Drop for InTransaction<'_> {
    fn drop(&mut self) {
        self.0.0.fetch_and(!1, Ordering::Release);
    }
}

trait Files {
    fn version(&self, dir: &Path) -> Result<Version, String>;
    fn recover(&self, loc: &Locations<'_>) -> Result<(), String>;
    fn install(&self, loc: &Locations<'_>, old: &Offer, pair: &VerifiedPair, authorize: &dyn Fn() -> Result<(), String>) -> Result<bool, String>;
}

struct Disk;

impl Files for Disk {
    fn version(&self, dir: &Path) -> Result<Version, String> {
        replacement::installed_version(dir)
    }

    fn recover(&self, loc: &Locations<'_>) -> Result<(), String> {
        replacement::recover(loc.home, loc.dir)
    }

    fn install(&self, loc: &Locations<'_>, old: &Offer, pair: &VerifiedPair, authorize: &dyn Fn() -> Result<(), String>) -> Result<bool, String> {
        replacement::install(loc.home, loc.dir, old, pair, authorize)
    }
}

pub(crate) struct Updates {
    feed: Box<dyn Feed>,
    files: Box<dyn Files>,
    pending: Option<(Offer, VerifiedPair)>,
    pub transactions: Transactions,
    pub clock: Option<ClockState>,
    generation: u64,
    isolated: bool,
    /// Set by a run that replaced the pair, except at Quit: the tray it installed, held and proven, or why it could not be.
    restart: Option<Result<VerifiedTray, String>>,
}

impl Updates {
    pub fn system() -> Self {
        Self {
            feed: Box::new(Official::new(SystemTokens)),
            files: Box::new(Disk),
            pending: None,
            transactions: Transactions::default(),
            clock: None,
            generation: 0,
            isolated: std::env::var_os("TSS_NO_APP_UPDATES").is_some(),
            restart: None,
        }
    }

    pub fn reset_if_changed(&mut self, generation: u64, prefs: UpdatePrefs) {
        if self.generation != generation || !prefs.enabled || prefs.timing != UpdateTiming::OnQuit {
            self.pending = None;
        }
        self.generation = generation;
    }

    pub fn staged(&self) -> Option<Version> {
        self.pending.as_ref().map(|(_, pair)| pair.version())
    }

    /// What the last successful run installed for the tray to restart into, once.
    pub fn take_restart(&mut self) -> Option<Result<VerifiedTray, String>> {
        self.restart.take()
    }

    /// Once at startup, not at every skill poll. A bad update record must not stop skills from syncing.
    pub fn initialize(&mut self, loc: &Locations<'_>) -> Result<(), String> {
        if !self.isolated {
            loc.check()?;
            let clock = ClockState::load(loc.home);
            self.clock = clock.as_ref().ok().copied();
            let _guard = self.transactions.enter()?;
            let recovery = self.files.recover(loc);
            match (clock, recovery) {
                (Err(c), Err(r)) => return Err(format!("{c}; {r}")),
                (Err(e), _) | (_, Err(e)) => return Err(e),
                _ => {}
            }
        }
        Ok(())
    }

    pub fn disk_status(&self, dir: &Path) -> Result<String, String> {
        let version = self.files.version(dir)?;
        Ok(format!("Installed on disk: {version}."))
    }

    pub fn run(
        &mut self,
        loc: &Locations<'_>,
        request: Request,
        prefs: UpdatePrefs,
        now: i64,
        cancel: &AtomicBool,
        permission: &dyn Fn() -> Result<(), String>,
    ) -> Result<String, String> {
        self.restart = None;
        #[cfg(debug_assertions)]
        if request == Request::Manual(Action::Install) {
            match TEST_RESTART.swap(0, Ordering::AcqRel) {
                0 => {}
                test => return Ok(self.test_restart(loc.dir, test == 2)),
            }
        }
        if self.isolated {
            return Err("app release requests and installation are disabled for this test copy".into());
        }
        let authorize = || {
            if cancel.load(Ordering::Acquire) {
                return Err("app update cancelled".into());
            }
            permission()?;
            if cancel.load(Ordering::Acquire) { Err("app update cancelled".into()) } else { Ok(()) }
        };
        let automatic = !matches!(request, Request::Manual(_));
        if automatic && (!prefs.enabled || (request == Request::Quit && prefs.timing != UpdateTiming::OnQuit)) {
            self.pending = None;
            return Ok("Automatic installation is not enabled for this action.".into());
        }
        loc.check()?;
        authorize()?;
        if request == Request::Quit {
            let (old, pair) = self.pending.take().ok_or("no verified app update is waiting for Quit")?;
            return self.commit(loc, &old, &pair, true, false, &authorize);
        }
        if request == Request::Scheduled {
            let mut state = ClockState::load(loc.home)?;
            self.clock = Some(state);
            if !state.due(prefs, now).is_some_and(|at| at <= now) {
                return self.disk_status(loc.dir);
            }
            state.attempted(now);
            self.clock = Some(state);
            state.save(loc.home)?;
        }
        let started = Instant::now();
        let finished_at = || now.saturating_add(started.elapsed().as_secs().min(i64::MAX as u64) as i64);
        let mut retry_at = None;
        let mut failure = |error: ReleaseError| {
            retry_at = error.retry_at;
            error.message
        };
        let result = (|| {
            // Check-only is read-only, including when a previous install needs recovery.
            if request != Request::Manual(Action::Check) {
                let _guard = self.transactions.enter()?;
                self.files.recover(loc)?;
            }
            let installed = self.files.version(loc.dir)?;
            authorize()?;
            let offer = self.feed.check(installed, now, cancel).map_err(&mut failure)?;
            let Some(offer) = offer else {
                self.pending = None;
                return Ok(format!("Installed on disk: {installed}. No newer stable release."));
            };
            if self.pending.as_ref().is_some_and(|(old, p)| old.version != installed || !p.matches(&offer)) {
                self.pending = None;
            }
            if request == Request::Manual(Action::Check) {
                return Ok(format!("Installed on disk: {installed}. Available: {}.", offer.version));
            }
            authorize()?;
            let (old, pair) = if let Some(verified) = self.pending.take() {
                verified
            } else {
                let old = self.feed.originals(&offer, installed, finished_at(), cancel).map_err(&mut failure)?;
                authorize()?;
                (old, self.feed.fetch(&offer, finished_at(), cancel).map_err(&mut failure)?)
            };
            authorize()?;
            if request == Request::Scheduled && prefs.timing == UpdateTiming::OnQuit {
                self.pending = Some((old, pair));
                Ok(format!("Installed on disk: {installed}. Verified {} for the next clean Quit.", offer.version))
            } else {
                self.commit(loc, &old, &pair, automatic, true, &authorize)
            }
        })();
        if request == Request::Scheduled {
            let state = self.clock.as_mut().expect("scheduled attempt recorded above");
            if result.is_ok() {
                state.succeeded(finished_at());
            } else {
                state.failed(finished_at(), retry_at);
            }
            if let Err(e) = state.save(loc.home) {
                self.pending = None;
                self.restart = None;
                let cause = match result {
                    Err(why) => format!("{why}; "),
                    _ => String::new(),
                };
                return Err(format!("{cause}could not record the app check: {e}. No update is waiting for Quit; check the versions on disk."));
            }
        }
        result
    }

    fn commit(
        &mut self,
        loc: &Locations<'_>,
        old: &Offer,
        pair: &VerifiedPair,
        automatic: bool,
        restart: bool,
        authorize: &dyn Fn() -> Result<(), String>,
    ) -> Result<String, String> {
        let _guard = self.transactions.enter()?;
        // Hold the writer lock from the final preference read through commit. If Off was saved
        // first its value is visible; if another writer is still saving, do not install.
        let settings_lock = std::cell::RefCell::new(None);
        let decide = || {
            if automatic && settings_lock.borrow().is_none() {
                let shared = crate::portable::load(loc.dir)?.is_some();
                settings_lock.replace(Some(crate::config::settings_write_lock(if shared { loc.dir } else { loc.home })?));
            }
            authorize()
        };
        let version = pair.version();
        if self.files.install(loc, old, pair, &decide)? {
            if restart {
                self.restart = Some(replacement::verified_tray(loc.dir, pair.file(0).1));
            }
            Ok(format!("Installed {version} on disk (both programs)."))
        } else {
            Ok(format!("No replacement needed. {}", self.disk_status(loc.dir)?))
        }
    }

    #[cfg(debug_assertions)]
    fn test_restart(&mut self, dir: &Path, wrong: bool) -> String {
        let path = dir.join("tiny-skill-sync.exe");
        let digest = fs::read(&path).map(|bytes| crate::release::sha256(&bytes)).map_err(|e| format!("{}: {e}", path.display()));
        self.restart = Some(digest.and_then(|mut digest| {
            digest[0] ^= u8::from(wrong);
            replacement::verified_tray(dir, digest)
        }));
        format!("Installed {} on disk (both programs). Debug test: nothing was downloaded or replaced.", env!("CARGO_PKG_VERSION"))
    }
}

/// Manual CLI actions share the tray's metadata fallback. They never open the skill ownership record or restart a tray.
pub fn manual(home: &Path, dir: &Path, skills: &Path, action: Action) -> Result<String, String> {
    let mut updates = Updates::system();
    let message =
        updates.run(&Locations { home, dir, skills }, Request::Manual(action), UpdatePrefs::default(), now_secs(), &AtomicBool::new(false), &|| Ok(()))?;
    Ok(match updates.take_restart() {
        Some(_) => format!("{message} A tray that is running keeps its current build until you quit it and open it again."),
        None => message,
    })
}

#[cfg(test)]
mod test_image;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::release::Offer;
    use std::cell::Cell;
    use std::rc::Rc;

    struct World {
        root: PathBuf,
        home: PathBuf,
        bin: PathBuf,
        skills: PathBuf,
    }

    impl World {
        fn new() -> Self {
            let base = std::env::var_os("TSS_TEST_ROOT").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
            let root = base.join(format!("tss-app-update-{}", crate::fsx::unique_suffix()));
            let w = Self { home: root.join("home"), bin: root.join("bin"), skills: root.join("skills"), root };
            for path in [&w.home, &w.bin, &w.skills] {
                fs::create_dir_all(path).unwrap();
            }
            fs::write(w.skills.join("user-file"), b"mine").unwrap();
            fs::write(w.bin.join("tiny-skill-sync.exe"), b"tiny-skill-sync.exe 0.9.9\nold tray").unwrap();
            fs::write(w.bin.join("tss.exe"), b"tss.exe 0.9.9\nold cli").unwrap();
            w
        }

        fn loc(&self) -> Locations<'_> {
            Locations { home: &self.home, dir: &self.bin, skills: &self.skills }
        }
    }

    impl Drop for World {
        fn drop(&mut self) {
            assert_eq!(fs::read(self.skills.join("user-file")).unwrap(), b"mine");
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn windows_version_and_transaction_accept_both_inert_resource_only_images() {
        let w = World::new();
        let (old, tray) = test_image::image(true, 1);
        let (_, cli) = test_image::image(false, 1);
        let originals = Offer::fixture_files(old, &tray, &cli);
        fs::write(w.bin.join("tiny-skill-sync.exe"), &tray).unwrap();
        fs::write(w.bin.join("tss.exe"), &cli).unwrap();
        assert_eq!(replacement::installed_version(&w.bin).unwrap(), old);
        let mapped = ["tiny-skill-sync.exe", "tss.exe"].map(|name| crate::win::install::map_old_image_read_only(&w.bin.join(name)).unwrap());
        let (offered, tray) = test_image::image(true, 2);
        let (_, cli) = test_image::image(false, 2);
        let pair = VerifiedPair::fixture(offered, &tray, &cli);
        let incorrect = VerifiedPair::fixture("9.9.9".parse().unwrap(), &tray, &cli);
        assert!(replacement::install(&w.home, &w.bin, &originals, &incorrect, || panic!("wrong version was authorized")).is_err());
        assert_eq!(replacement::installed_version(&w.bin).unwrap(), old);
        assert!(replacement::install(&w.home, &w.bin, &originals, &pair, || Ok(())).unwrap());
        assert_eq!(replacement::installed_version(&w.bin).unwrap(), offered);
        assert_eq!(fs::read(w.bin.join("tiny-skill-sync.exe")).unwrap(), tray);
        assert_eq!(fs::read(w.bin.join("tss.exe")).unwrap(), cli);
        drop(mapped);
        replacement::recover(&w.home, &w.bin).unwrap();
        assert_eq!(fs::read_dir(&w.bin).unwrap().count(), 3); // the pair and the persistent lock
    }

    struct FeedDouble {
        checks: Rc<Cell<usize>>,
        downloads: Rc<Cell<usize>>,
        fail: bool,
    }

    impl Feed for FeedDouble {
        fn check(&self, installed: Version, now: i64, _: &AtomicBool) -> Result<Option<Offer>, ReleaseError> {
            self.checks.set(self.checks.get() + 1);
            if self.fail {
                return Err(ReleaseError { message: "rate limited".into(), retry_at: Some(now + 12_345) });
            }
            let available = "1.2.3".parse().unwrap();
            Ok((available > installed).then(|| Offer::fixture_files(available, b"tiny-skill-sync.exe 1.2.3\nnew tray", b"tss.exe 1.2.3\nnew cli")))
        }
        fn originals(&self, _: &Offer, installed: Version, _: i64, _: &AtomicBool) -> Result<Offer, ReleaseError> {
            Ok(Offer::fixture_files(installed, b"tiny-skill-sync.exe 0.9.9\nold tray", b"tss.exe 0.9.9\nold cli"))
        }
        fn fetch(&self, offer: &Offer, _: i64, _: &AtomicBool) -> Result<VerifiedPair, ReleaseError> {
            self.downloads.set(self.downloads.get() + 1);
            Ok(VerifiedPair::fixture(offer.version, b"tiny-skill-sync.exe 1.2.3\nnew tray", b"tss.exe 1.2.3\nnew cli"))
        }
    }

    struct FixtureDisk;

    impl Files for FixtureDisk {
        fn version(&self, dir: &Path) -> Result<Version, String> {
            replacement::fixture::installed_version(dir)
        }
        fn recover(&self, loc: &Locations<'_>) -> Result<(), String> {
            replacement::recover(loc.home, loc.dir)
        }
        fn install(&self, loc: &Locations<'_>, old: &Offer, pair: &VerifiedPair, authorize: &dyn Fn() -> Result<(), String>) -> Result<bool, String> {
            replacement::fixture::install(loc.home, loc.dir, old, pair, authorize)
        }
    }

    struct Fixture {
        checks: Rc<Cell<usize>>,
        downloads: Rc<Cell<usize>>,
    }

    impl Fixture {
        fn new() -> Self {
            Self { checks: Rc::default(), downloads: Rc::default() }
        }
        fn worker(&self, fail: bool) -> Updates {
            Updates {
                feed: Box::new(FeedDouble { checks: self.checks.clone(), downloads: self.downloads.clone(), fail }),
                files: Box::new(FixtureDisk),
                pending: None,
                transactions: Transactions::default(),
                clock: None,
                generation: 0,
                isolated: false,
                restart: None,
            }
        }
    }

    #[test]
    fn quit_consumes_only_a_verified_in_memory_pair_and_never_requests_on_exit() {
        let w = World::new();
        let f = Fixture::new();
        let mut u = f.worker(false);
        let prefs = UpdatePrefs { timing: UpdateTiming::OnQuit, ..UpdatePrefs::default() };
        let run = |u: &mut Updates, req, now| u.run(&w.loc(), req, prefs, now, &AtomicBool::new(false), &|| Ok(()));
        assert!(run(&mut u, Request::Quit, 10).is_err());
        assert!(run(&mut u, Request::Scheduled, 10).unwrap().contains("Verified 1.2.3"));
        assert_eq!(replacement::fixture::installed_version(&w.bin).unwrap().to_string(), "0.9.9");
        assert!(f.worker(false).run(&w.loc(), Request::Quit, prefs, 11, &AtomicBool::new(false), &|| Ok(())).is_err());
        assert!(run(&mut u, Request::Quit, 12).unwrap().contains("Installed 1.2.3"));
        assert_eq!(fs::read(w.bin.join("tiny-skill-sync.exe")).unwrap(), b"tiny-skill-sync.exe 1.2.3\nnew tray");
        assert_eq!(fs::read(w.bin.join("tss.exe")).unwrap(), b"tss.exe 1.2.3\nnew cli");
        assert_eq!((f.checks.get(), f.downloads.get()), (1, 1));
        assert!(run(&mut u, Request::Quit, 13).is_err());
    }

    fn rename_tray(w: &World) -> std::io::Result<()> {
        let (from, to) = (w.bin.join("tiny-skill-sync.exe"), w.bin.join("moved.exe"));
        fs::rename(&from, &to)?;
        fs::rename(&to, &from)
    }

    #[test]
    fn an_install_that_replaced_the_pair_holds_the_proven_tray_and_quit_never_restarts() {
        let new_tray: &[u8] = b"tiny-skill-sync.exe 1.2.3\nnew tray";
        for request in [Request::Manual(Action::Install), Request::Scheduled] {
            let w = World::new();
            let f = Fixture::new();
            let mut u = f.worker(false);
            let run = |u: &mut Updates, request| u.run(&w.loc(), request, UpdatePrefs::default(), 1, &AtomicBool::new(false), &|| Ok(())).unwrap();
            assert!(run(&mut u, request).contains("Installed 1.2.3 on disk"));
            let tray = u.take_restart().expect("an install offers a restart").unwrap();
            assert!(u.take_restart().is_none(), "taken once");
            assert_eq!(tray.path(), w.bin.join("tiny-skill-sync.exe"));
            assert_eq!(fs::read(tray.path()).unwrap(), new_tray);
            assert!(rename_tray(&w).is_err(), "the proven tray cannot be moved while it is held");
            drop(tray);
            rename_tray(&w).unwrap();
            assert!(replacement::verified_tray(&w.bin, crate::release::sha256(b"tiny-skill-sync.exe 1.2.4\nother tray")).is_err());
            // A restart nobody took goes with the next run, here one with nothing newer to install.
            fs::write(w.bin.join("tiny-skill-sync.exe"), b"tiny-skill-sync.exe 0.9.9\nold tray").unwrap();
            fs::write(w.bin.join("tss.exe"), b"tss.exe 0.9.9\nold cli").unwrap();
            assert!(run(&mut u, Request::Manual(Action::Install)).contains("Installed 1.2.3 on disk"));
            assert!(rename_tray(&w).is_err());
            assert!(run(&mut u, Request::Manual(Action::Install)).contains("No newer stable release"));
            assert!(u.take_restart().is_none());
            rename_tray(&w).unwrap();
        }

        let w = World::new();
        let f = Fixture::new();
        let mut u = f.worker(false);
        let prefs = UpdatePrefs { timing: UpdateTiming::OnQuit, ..UpdatePrefs::default() };
        u.run(&w.loc(), Request::Scheduled, prefs, 1, &AtomicBool::new(false), &|| Ok(())).unwrap();
        assert!(u.take_restart().is_none(), "staged, not installed");
        assert!(u.run(&w.loc(), Request::Quit, prefs, 2, &AtomicBool::new(false), &|| Ok(())).unwrap().contains("Installed 1.2.3 on disk"));
        assert_eq!(fs::read(w.bin.join("tiny-skill-sync.exe")).unwrap(), new_tray);
        assert!(u.take_restart().is_none(), "an install at Quit stays quit");
    }

    #[test]
    fn a_scheduled_install_whose_clock_cannot_be_saved_offers_no_restart() {
        let w = World::new();
        let f = Fixture::new();
        let mut u = f.worker(false);
        let clock = w.home.join(crate::update_clock::CLOCK_FILE);
        let break_clock = || {
            if f.downloads.get() == 1 && clock.is_file() {
                fs::remove_file(&clock).unwrap();
                fs::create_dir(&clock).unwrap();
            }
            Ok(())
        };
        let error = u.run(&w.loc(), Request::Scheduled, UpdatePrefs::default(), 1, &AtomicBool::new(false), &break_clock).unwrap_err();
        assert!(error.contains("could not record the app check"), "{error}");
        assert_eq!(fs::read(w.bin.join("tiny-skill-sync.exe")).unwrap(), b"tiny-skill-sync.exe 1.2.3\nnew tray");
        assert!(u.take_restart().is_none());
    }

    #[test]
    fn manual_check_and_install_work_while_off_without_mutating_the_clock() {
        let w = World::new();
        let f = Fixture::new();
        let mut u = f.worker(false);
        let prefs = UpdatePrefs { enabled: false, ..UpdatePrefs::default() };
        let mut run = |req| u.run(&w.loc(), req, prefs, 1, &AtomicBool::new(false), &|| Ok(())).unwrap();
        run(Request::Scheduled);
        assert_eq!(f.checks.get(), 0);
        assert!(run(Request::Manual(Action::Check)).contains("Available: 1.2.3"));
        assert_eq!(f.downloads.get(), 0);
        assert!(run(Request::Manual(Action::Install)).contains("Installed 1.2.3 on disk"));
        assert_eq!(fs::read_dir(&w.home).unwrap().count(), 0);
        assert_eq!((f.checks.get(), f.downloads.get()), (2, 1));
    }

    #[test]
    fn scheduled_failure_honors_the_server_wait_across_a_fresh_worker() {
        let w = World::new();
        let f = Fixture::new();
        let run = |u: &mut Updates, now| u.run(&w.loc(), Request::Scheduled, UpdatePrefs::default(), now, &AtomicBool::new(false), &|| Ok(()));
        assert!(run(&mut f.worker(true), 500).is_err());
        let mut fresh = f.worker(false);
        fresh.initialize(&w.loc()).unwrap();
        run(&mut fresh, 501).unwrap();
        assert_eq!((f.checks.get(), f.downloads.get()), (1, 0));
        assert!(run(&mut fresh, 12_845).unwrap().contains("Installed 1.2.3"));
        assert_eq!((f.checks.get(), f.downloads.get()), (2, 1));
    }

    #[test]
    fn off_before_quit_or_revocation_after_download_cannot_install() {
        let w = World::new();
        let f = Fixture::new();
        let mut u = f.worker(false);
        let mut prefs = UpdatePrefs { timing: UpdateTiming::OnQuit, ..UpdatePrefs::default() };
        let cancel = AtomicBool::new(false);
        u.run(&w.loc(), Request::Scheduled, prefs, 44, &cancel, &|| Ok(())).unwrap();
        prefs.enabled = false;
        u.run(&w.loc(), Request::Quit, prefs, 45, &cancel, &|| Ok(())).unwrap();
        prefs.enabled = true;
        assert!(u.staged().is_none());
        assert!(u.run(&w.loc(), Request::Quit, prefs, 46, &cancel, &|| Ok(())).is_err());
        prefs.timing = UpdateTiming::Immediate;
        let permission = || if f.downloads.get() > 1 { Err("disabled during download".into()) } else { Ok(()) };
        assert!(u.run(&w.loc(), Request::Scheduled, prefs, 44 + 86_400, &cancel, &permission).is_err());
        assert_eq!(replacement::fixture::installed_version(&w.bin).unwrap().to_string(), "0.9.9");
    }

    #[test]
    fn installation_inside_the_skills_folder_is_refused_before_any_work() {
        let w = World::new();
        let f = Fixture::new();
        let loc = Locations { dir: &w.skills, ..w.loc() };
        assert!(f.worker(false).run(&loc, Request::Scheduled, UpdatePrefs::default(), 0, &AtomicBool::new(false), &|| Ok(())).is_err());
        assert_eq!(f.checks.get(), 0);
        assert_eq!(fs::read_dir(&w.home).unwrap().count(), 0);
    }

    #[test]
    fn bad_schedule_and_bad_recovery_record_are_reported_independently() {
        let w = World::new();
        fs::write(w.home.join(crate::update_clock::CLOCK_FILE), b"malformed").unwrap();
        fs::write(w.bin.join(".tss-update.pending"), b"unrelated user file").unwrap();
        let mut u = Fixture::new().worker(false);
        let error = u.initialize(&w.loc()).unwrap_err();
        assert!(error.contains("clock") && error.contains("receipt"), "{error}");
        assert!(u.clock.is_none());
        assert_eq!(fs::read(w.bin.join(".tss-update.pending")).unwrap(), b"unrelated user file");
    }

    #[test]
    fn session_end_revokes_preparation_and_cannot_abandon_an_active_filesystem_transaction() {
        let gate = Transactions::default();
        let active = gate.enter().unwrap();
        assert!(gate.close(), "shutdown must wait for this transaction");
        drop(active);
        assert!(!gate.close());
        assert!(gate.enter().is_err(), "late worker cannot start after shutdown");
        let before = Transactions::default();
        assert!(!before.close());
        assert!(before.enter().is_err());
    }

    #[test]
    fn existing_worker_loads_active_settings_stages_then_requires_a_clean_flush_for_quit() {
        use crate::app::{App, Job, SettingsSource, perform_with_updates};
        use crate::config::{CONFIG_FILE, Config};
        use crate::engine::Engine;
        use crate::github::{GitHubRemote, TokenProvider};
        use crate::secret::Secret;
        use std::time::{Duration, Instant};

        struct NoCredentials;
        impl TokenProvider for NoCredentials {
            fn token(&self) -> Result<Secret, String> {
                panic!("an app update must not ask for skill credentials")
            }
        }

        for disable_before_quit in [true, false] {
            let w = World::new();
            let f = Fixture::new();
            let mut u = f.worker(false);
            let prefs = UpdatePrefs { timing: UpdateTiming::OnQuit, ..UpdatePrefs::default() };
            let cfg = Config { skills_dir: Some(w.skills.to_string_lossy().into_owned()), app_update: prefs, ..Config::default() };
            let file = w.home.join(CONFIG_FILE);
            cfg.save(&file).unwrap();
            let remote = GitHubRemote::with_bases("http://127.0.0.1:1", "http://127.0.0.1:1").unwrap();
            let paths = Paths::new(w.home.clone(), w.skills.clone()).with_file_roots(w.root.join("agents"), w.root.join("instructions"));
            let mut engine = Engine::new(paths, Box::new(remote), Box::new(NoCredentials));
            let mut app = App::new(cfg.clone(), w.skills.clone());
            app.set_settings_source(SettingsSource::new(file.clone(), w.bin.clone()));
            let mut dispatch = |app: &mut App| {
                let order = app.next_order().unwrap();
                // Refuse to run a skill job against any folder inherited from the invoking environment.
                assert_eq!(order.skills_dir, w.skills);
                let mut done = perform_with_updates(&mut engine, &order, &AtomicBool::new(false), Some(&mut u), 100);
                if let Some(settings) = done.settings.take() {
                    let loaded = settings.result.unwrap();
                    assert_eq!(loaded.skills_dir, w.skills, "run tests without a process-wide skills override");
                    app.settings_reloaded(loaded, &done.job);
                }
                assert!(done.result.is_ok(), "{:?}", done.result);
                app.finish(done, 100);
                order.job
            };
            app.queue_initial_settings_load();
            assert_eq!(dispatch(&mut app), Job::LoadSettings);
            assert_eq!(dispatch(&mut app), Job::Startup);
            assert_eq!(app.arm(100), Some(100));
            app.on_timer(100);
            assert_eq!(dispatch(&mut app), Job::ScheduledUpdate);
            assert!(app.has_quit_update());
            assert_eq!(fs::read(w.bin.join("tss.exe")).unwrap(), b"tss.exe 0.9.9\nold cli");
            let deadline = Instant::now() + Duration::from_secs(2);
            app.quit();
            assert!(!app.finish_on_quit(true, deadline)); // Flush has not returned yet.
            assert_eq!(dispatch(&mut app), Job::Flush);
            assert!(!app.finish_on_quit(false, deadline)); // logoff or a failed save/flush
            assert!(!app.finish_on_quit(true, Instant::now())); // elapsed timeout
            if disable_before_quit {
                let off = Config { app_update: UpdatePrefs { enabled: false, ..prefs }, ..cfg };
                off.save(&file).unwrap(); // another process disabled updates after staging
            }
            assert!(app.finish_on_quit(true, deadline));
            assert!(matches!(dispatch(&mut app), Job::InstallOnQuit { .. }));
            let installed = replacement::fixture::installed_version(&w.bin).unwrap();
            assert_eq!(installed.to_string(), if disable_before_quit { "0.9.9" } else { "1.2.3" });
            assert_eq!((f.checks.get(), f.downloads.get()), (1, 1));
            assert!(app.is_idle());
        }
    }

    #[test]
    fn external_off_mid_immediate_job_preserves_both_files_and_reloads_before_skill_work() {
        use crate::app::{App, Job, SettingsSource, perform_with_updates};
        use crate::config::{CONFIG_FILE, Config};
        use crate::engine::Engine;
        use crate::github::{GitHubRemote, TokenProvider};
        use crate::secret::Secret;
        struct NoCredentials;
        impl TokenProvider for NoCredentials {
            fn token(&self) -> Result<Secret, String> {
                panic!("no credentials for this job")
            }
        }
        struct BeforeDecision {
            off: Config,
            path: PathBuf,
        }
        impl Files for BeforeDecision {
            fn version(&self, dir: &Path) -> Result<Version, String> {
                FixtureDisk.version(dir)
            }
            fn recover(&self, loc: &Locations<'_>) -> Result<(), String> {
                FixtureDisk.recover(loc)
            }
            fn install(&self, loc: &Locations<'_>, old: &Offer, pair: &VerifiedPair, authorize: &dyn Fn() -> Result<(), String>) -> Result<bool, String> {
                FixtureDisk.install(loc, old, pair, &|| {
                    self.off.save(&self.path)?;
                    authorize()
                })
            }
        }
        let w = World::new();
        let path = w.home.join(CONFIG_FILE);
        let cfg = Config { skills_dir: Some(w.skills.to_string_lossy().into_owned()), ..Config::default() };
        cfg.save(&path).unwrap();
        let mut off = cfg.clone();
        off.app_update.enabled = false;
        let f = Fixture::new();
        let mut u = f.worker(false);
        u.files = Box::new(BeforeDecision { off: off.clone(), path: path.clone() });
        let remote = GitHubRemote::with_bases("http://127.0.0.1:1", "http://127.0.0.1:1").unwrap();
        let paths = Paths::new(w.home.clone(), w.skills.clone()).with_file_roots(w.root.join("agents"), w.root.join("instructions"));
        let mut engine = Engine::new(paths, Box::new(remote), Box::new(NoCredentials));
        let mut app = App::new(cfg, w.skills.clone());
        app.set_settings_source(SettingsSource::new(path.clone(), w.bin.clone()));
        let mut next = |app: &mut App| {
            let order = app.next_order().unwrap();
            assert_eq!(order.skills_dir, w.skills);
            perform_with_updates(&mut engine, &order, &AtomicBool::new(false), Some(&mut u), 10)
        };
        let mut done = next(&mut app);
        assert_eq!(done.job, Job::Startup);
        app.settings_reloaded(done.settings.take().unwrap().result.unwrap(), &done.job);
        app.finish(done, 10);
        app.arm(10);
        app.on_timer(10);
        let mut done = next(&mut app);
        assert_eq!(done.job, Job::ScheduledUpdate);
        assert!(done.result.as_ref().unwrap_err().to_string().contains("settings changed"));
        assert!(!done.settings.take().unwrap().result.unwrap().matches_local_file(&path));
        app.reload_before_next_job();
        app.finish(done, 10);
        let mut reload = next(&mut app);
        assert_eq!(reload.job, Job::Plan);
        app.settings_reloaded(reload.settings.take().unwrap().result.unwrap(), &reload.job);
        app.finish(reload, 10);
        assert_eq!(app.cfg(), &off);
        assert!(!app.has_quit_update());
        assert_eq!(app.arm(10), None);
        assert_eq!(fs::read(w.bin.join("tiny-skill-sync.exe")).unwrap(), b"tiny-skill-sync.exe 0.9.9\nold tray");
        assert_eq!(fs::read(w.bin.join("tss.exe")).unwrap(), b"tss.exe 0.9.9\nold cli");
    }
}
