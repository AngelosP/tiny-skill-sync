//! A data-only transaction for the two executables in one installation directory.
//!
//! The exclusive lock, staging names and backups all belong to **that directory**, not a settings
//! home. The shared journal contains no paths and is not an instruction to install anything. It must
//! match the independent witness in the initiating private home, both directories, both receipt
//! file identities, and the original and new release bytes. Recovery never promotes staging.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::release::{Offer, VerifiedPair, Version, sha256, sha256_reader};
use crate::win::install as os;

const LIVE: [&str; 2] = ["tiny-skill-sync.exe", "tss.exe"];
const STAGE: [&str; 2] = [".tss-update-tray.new", ".tss-update-cli.new"];
const BACKUP: [&str; 2] = [".tss-update-tray.old", ".tss-update-cli.old"];
const JOURNAL: &str = ".tss-update.pending";
const LOCK: &str = ".tss-update.lock";
const MAX_EXE: u64 = 64 * 1024 * 1024;
const MAX_RECEIPT: u64 = 512;

type VersionReader = fn(&Path, &File, &str) -> Result<Version, String>;
type Hook<'a> = &'a dyn Fn(Step) -> Result<(), String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Recorded,
    Staged(usize),
    OnDisk,
    Ready,
    Committing,
    Saved(usize),
    Placed(usize),
    Restored(usize),
    Retire(usize),
}

fn proceed(_: Step) -> Result<(), String> {
    Ok(())
}

trait At<T> {
    fn at(self, path: &Path) -> Result<T, String>;
}

impl<T> At<T> for io::Result<T> {
    fn at(self, path: &Path) -> Result<T, String> {
        self.map_err(|e| format!("{}: {e}", path.display()))
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn read_bounded<T>(f: &File, limit: u64, consume: impl FnOnce(&mut io::Take<&File>) -> io::Result<T>) -> io::Result<T> {
    let content = os::content(f)?;
    let mut f = content.as_ref().unwrap_or(f);
    let len = f.metadata()?.len();
    if len > limit {
        return Err(invalid("file is too large to verify"));
    }
    f.rewind()?;
    let mut reader = f.take(len + 1);
    let result = consume(&mut reader)?;
    if reader.limit() != 1 {
        return Err(invalid("file changed while it was read"));
    }
    Ok(result)
}

fn read(f: &File, limit: u64) -> io::Result<Vec<u8>> {
    read_bounded(f, limit, |reader| {
        let mut bytes = Vec::with_capacity((reader.limit() - 1) as usize);
        reader.read_to_end(&mut bytes)?;
        Ok(bytes)
    })
}

fn disk_hash(f: &File) -> io::Result<[u8; 32]> {
    read_bounded(f, MAX_EXE, |reader| sha256_reader(reader))
}

fn present(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

fn work_present(dir: &Path) -> Result<bool, String> {
    for name in [JOURNAL].into_iter().chain(STAGE).chain(BACKUP) {
        let p = dir.join(name);
        if present(&p).at(&p)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn locked(dir: &Path) -> Result<File, String> {
    os::lock(&dir.join(LOCK)).map_err(|e| format!("cannot lock the executable folder for updating (another copy may be updating it): {e}"))
}

fn read_pair(dir: &Path, names: [&str; 2], inspect: VersionReader) -> Result<(Version, [File; 2]), String> {
    let paths = names.map(|n| dir.join(n));
    let files = [os::open(&paths[0], false).at(&paths[0])?, os::open(&paths[1], false).at(&paths[1])?];
    let version = inspect(&paths[0], &files[0], LIVE[0])?;
    if inspect(&paths[1], &files[1], LIVE[1])? != version {
        return Err("the tray and command line have different versions; neither was replaced".into());
    }
    Ok((version, files))
}

/// Read both roles from their passive Windows version resources. No lock file or settings writes.
pub fn installed_version(dir: &Path) -> Result<Version, String> {
    version_with(dir, os::version)
}

fn version_with(dir: &Path, inspect: VersionReader) -> Result<Version, String> {
    let _folder = os::Directory::pin(dir).at(dir)?;
    Ok(read_pair(dir, LIVE, inspect)?.0)
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    id: os::FileId,
    hash: [u8; 32],
}

impl Stamp {
    fn of(f: &File) -> io::Result<Self> {
        Ok(Self { id: os::id(f)?, hash: disk_hash(f)? })
    }
}

#[derive(PartialEq, Eq)]
struct Receipt {
    old: [Stamp; 2],
    new: [Stamp; 2],
}

/// Before the receipt has been flushed, only the creator's still-open handle proves ownership.
struct Created {
    f: File,
    keep: bool,
}

impl Created {
    fn new(path: &Path) -> Result<Self, String> {
        Ok(Self { f: os::create(path).at(path)?, keep: false })
    }
}

impl Drop for Created {
    fn drop(&mut self) {
        if !self.keep {
            let _ = os::delete(&self.f);
        }
    }
}

fn witness_name(id: &os::FileId) -> String {
    format!(".tss-update-{}.receipt", crate::hash::hex(id))
}

fn private_work_present(home: &Path, dir: &Path, folder: &os::Directory) -> Result<bool, String> {
    let _home = match os::Directory::pin(home) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        other => other.at(home)?,
    };
    let path = home.join(witness_name(&folder.id().at(dir)?));
    present(&path).at(&path)
}

/// Never derive a path from mutable journal bytes. An install has its own witness even when two
/// installations use the same home. A different home cannot adopt an interrupted transaction.
struct Records {
    shared: Created,
    local: Created,
    _home: os::Directory,
    binding: Vec<u8>,
}

impl Records {
    fn open(home: &Path, dir: &Path, folder: &os::Directory, existing: Option<File>) -> Result<Self, String> {
        let install = folder.id().at(dir)?;
        let private = os::Directory::private(home, install, existing.is_none()).at(home)?;
        let witness = home.join(witness_name(&install));
        let shared_path = dir.join(JOURNAL);
        let (local, shared) = match existing {
            Some(f) => (Created { f: os::open(&witness, true).at(&witness)?, keep: true }, Created { f, keep: true }),
            None => (Created::new(&witness)?, Created::new(&shared_path)?),
        };
        let mut binding = b"TSS executable pair receipt v2\0".to_vec();
        for id in [install, private.id().at(home)?, os::id(&shared.f).at(&shared_path)?, os::id(&local.f).at(&witness)?] {
            binding.extend(id);
        }
        Ok(Self { shared, local, _home: private, binding })
    }

    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        for f in [&mut self.local.f, &mut self.shared.f] {
            f.seek(SeekFrom::End(0))?;
            f.write_all(bytes)?;
            f.sync_all()?;
        }
        Ok(())
    }

    fn prepare(&mut self, receipt: &Receipt) -> io::Result<[u8; 32]> {
        let mut bytes = self.binding.clone();
        for s in receipt.old.iter().chain(&receipt.new) {
            bytes.extend(s.id);
            bytes.extend(s.hash);
        }
        self.append(&bytes)?;
        let (checked, committed) = self.read()?;
        if checked != *receipt || committed {
            return Err(invalid("update receipts could not be verified on disk"));
        }
        self.local.keep = true;
        self.shared.keep = true;
        // Only the originating worker writes this phase to the private witness, after authorization.
        Ok(sha256(&bytes))
    }

    fn read(&self) -> io::Result<(Receipt, bool)> {
        let record = read(&self.shared.f, MAX_RECEIPT)?;
        if record != read(&self.local.f, MAX_RECEIPT)? {
            return Err(invalid("shared and private update receipts disagree"));
        }
        let bad = || invalid("invalid or foreign update receipt");
        const ID: usize = size_of::<os::FileId>();
        const STAMP: usize = ID + 32;
        let rest = record.strip_prefix(&self.binding[..]).ok_or_else(bad)?;
        let (bytes, commit) = rest.split_at_checked(4 * STAMP).ok_or_else(bad)?;
        if !commit.is_empty() && commit != sha256(&record[..record.len() - commit.len()]) {
            return Err(bad());
        }
        let stamp = |i| {
            let s = &bytes[i * STAMP..(i + 1) * STAMP];
            Stamp { id: s[..ID].try_into().unwrap(), hash: s[ID..].try_into().unwrap() }
        };
        Ok((Receipt { old: [stamp(0), stamp(1)], new: [stamp(2), stamp(3)] }, !commit.is_empty()))
    }

    fn retire(&self) -> io::Result<()> {
        os::delete(&self.shared.f)?;
        os::delete(&self.local.f)
    }
}

fn stage(
    home: &Path,
    dir: &Path,
    folder: &os::Directory,
    old: [Stamp; 2],
    pair: &VerifiedPair,
    hook: Hook<'_>,
) -> Result<(Receipt, Records, [u8; 32]), String> {
    let mut records = Records::open(home, dir, folder, None)?;
    let mut stage = [Created::new(&dir.join(STAGE[0]))?, Created::new(&dir.join(STAGE[1]))?];
    let new = |i: usize| -> Result<Stamp, String> { Ok(Stamp { id: os::id(&stage[i].f).at(&dir.join(STAGE[i]))?, hash: pair.file(i).1 }) };
    let receipt = Receipt { old, new: [new(0)?, new(1)?] };
    let commit = records.prepare(&receipt).at(&dir.join(JOURNAL))?;
    for s in &mut stage {
        s.keep = true;
    }
    hook(Step::Recorded)?;
    for i in 0..2 {
        let path = dir.join(STAGE[i]);
        let (bytes, hash) = pair.file(i);
        stage[i].keep = false; // a normal write failure can discard this still-private partial write
        stage[i].f.write_all(bytes).and_then(|()| stage[i].f.sync_all()).at(&path)?;
        if disk_hash(&stage[i].f).at(&path)? != hash {
            return Err(format!("{} did not match the verified download", path.display()));
        }
        stage[i].keep = true;
        hook(Step::Staged(i))?;
    }
    Ok((receipt, records, commit))
}

/// Pin the checked file through the rename/deletion. Version APIs require a separate read-only
/// inspection, so re-check identity AND disk contents when changing to a DELETE-capable handle.
fn movable(path: &Path, expected: Stamp) -> io::Result<File> {
    let f = os::open(path, true)?;
    if os::id(&f)? != expected.id || disk_hash(&f)? != expected.hash {
        return Err(invalid("file changed since it was verified; nothing else was moved"));
    }
    Ok(f)
}

/// A read-only pinned file in one of the six fixed slots. Unknown objects are errors, never absence.
struct Entry {
    path: PathBuf,
    f: File,
    stamp: Stamp,
}

impl Entry {
    fn get(path: PathBuf) -> Result<Option<Self>, String> {
        let f = match os::open(&path, false) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let stamp = Stamp::of(&f).at(&path)?;
        Ok(Some(Self { path, f, stamp }))
    }

    fn into_movable(self) -> io::Result<File> {
        drop(self.f);
        movable(&self.path, self.stamp)
    }

    fn move_to(self, path: PathBuf) -> Result<Self, String> {
        let stamp = self.stamp;
        let f = self.into_movable().at(&path)?;
        os::rename(&f, &path).at(&path)?;
        Ok(Self { path, stamp, f })
    }

    fn remove(self) -> io::Result<()> {
        os::delete(&self.into_movable()?)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Slot {
    Original,
    Gap,
    Replaced,
}

struct Role {
    live: Option<Entry>,
    backup: Option<Entry>,
    stage: Option<Entry>,
    slot: Slot,
    stage_ready: bool,
}

impl Role {
    fn inspect(dir: &Path, i: usize, r: &Receipt) -> Result<Self, String> {
        let live = Entry::get(dir.join(LIVE[i]))?;
        let backup = Entry::get(dir.join(BACKUP[i]))?;
        let stage = Entry::get(dir.join(STAGE[i]))?;
        let unexpected = || format!("unexpected update files for {}; kept everything for inspection", LIVE[i]);
        let backup_ok = backup.as_ref().is_none_or(|e| e.stamp == r.old[i]);
        let stage_ready = stage.as_ref().is_some_and(|e| e.stamp == r.new[i]);
        // A crash before writing a newly created stage can leave its exact, recorded empty file.
        // Only accept that in the untouched-originals state, never as a replacement or in a gap.
        let stage_ok = stage_ready || stage.as_ref().is_none_or(|e| e.stamp.id == r.new[i].id && e.stamp.hash == sha256(&[]));
        if !backup_ok || !stage_ok {
            return Err(unexpected());
        }
        let slot = match live.as_ref().map(|e| e.stamp) {
            Some(s) if s == r.old[i] && backup.is_none() => Slot::Original,
            None if backup.is_some() && stage_ready => Slot::Gap,
            Some(s) if s == r.new[i] && stage.is_none() => Slot::Replaced,
            _ => return Err(unexpected()),
        };
        Ok(Self { live, backup, stage, slot, stage_ready })
    }

    fn restore(&mut self, dir: &Path, i: usize, hook: Hook<'_>) -> Result<(), String> {
        if self.slot == Slot::Original {
            return Ok(());
        }
        if self.slot == Slot::Replaced {
            self.stage = Some(self.live.take().unwrap().move_to(dir.join(STAGE[i]))?);
        }
        self.live = Some(self.backup.take().unwrap().move_to(dir.join(LIVE[i]))?);
        hook(Step::Restored(i))
    }
}

fn in_use(e: &io::Error) -> bool {
    // Access denied, sharing/lock violation, user-mapped section. Never treat bad bytes as "busy".
    matches!(e.raw_os_error(), Some(5 | 32 | 33 | 1224))
}

/// Returns false when a proven file could not yet be retired. Both canonical names are then already
/// coherent. Keep the receipt and retry on a subsequent visit, not with a timer or another process.
fn recover_locked(home: &Path, dir: &Path, folder: &os::Directory, hook: Hook<'_>) -> Result<bool, String> {
    let path = dir.join(JOURNAL);
    let marker = match os::open(&path, true) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return if work_present(dir)? || private_work_present(home, dir, folder)? {
                Err("update files without a matching shared receipt; nothing was changed".into())
            } else {
                Ok(true)
            };
        }
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let records = Records::open(home, dir, folder, Some(marker))?;
    let (r, authorized) = records.read().map_err(|_| "cannot verify this folder's update receipt against its private copy; nothing was changed".to_owned())?;
    let mut roles = [Role::inspect(dir, 0, &r)?, Role::inspect(dir, 1, &r)?];
    use Slot::{Gap, Original, Replaced};
    if !authorized && roles.iter().any(|role| role.slot != Original) {
        return Err("the update files moved without a recorded commit; nothing was changed".into());
    }
    let done = match (roles[0].slot, roles[1].slot) {
        (Replaced, Replaced) => true,
        (Original, Original) => false, // includes interrupted cleanup of a cancelled or rolled-back update
        (Gap | Replaced, Original) | (Replaced, Gap)
            if roles.iter().all(|role| match role.slot {
                Replaced => role.backup.is_some(),
                Original | Gap => role.stage_ready,
            }) =>
        {
            false
        }
        _ => return Err("inconsistent update files; nothing was changed".into()),
    };
    if !done {
        // Reverse commit order: there is always at least one usable executable name.
        for i in [1, 0] {
            roles[i].restore(dir, i, hook)?;
        }
    }
    for (i, role) in roles.iter_mut().enumerate() {
        let spare = if done { role.backup.take() } else { role.stage.take() };
        if let Some(spare) = spare {
            hook(Step::Retire(i))?;
            let name = dir.join(if done { BACKUP[i] } else { STAGE[i] });
            match spare.remove() {
                Err(e) if in_use(&e) => return Ok(false),
                result => result.at(&name)?,
            }
            // Disposition can be pending until another reader releases its handle.
            match present(&name) {
                Ok(false) => {}
                Ok(true) => return Ok(false),
                Err(e) if in_use(&e) => return Ok(false),
                Err(e) => return Err(format!("{}: {e}", name.display())),
            }
        }
    }
    records.retire().at(&path)?;
    Ok(true)
}

pub fn recover(home: &Path, dir: &Path) -> Result<(), String> {
    let folder = os::Directory::pin(dir).at(dir)?;
    if work_present(dir)? || private_work_present(home, dir, &folder)? {
        let _lock = locked(dir)?;
        recover_locked(home, dir, &folder, &proceed)?;
    }
    Ok(())
}

/// The installed tray after its bytes were proven to be the verified release. The file stays open so it cannot be
/// written, renamed or deleted, and its folders stay pinned so none can be moved, until this is dropped. Nothing here
/// runs it.
pub struct VerifiedTray {
    path: PathBuf,
    _file: File,
    _folder: os::Directory,
}

impl VerifiedTray {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl fmt::Debug for VerifiedTray {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VerifiedTray").field("path", &self.path).finish_non_exhaustive()
    }
}

/// Opens the tray in `dir` and proves it has the `digest` of the verified release, the same way an install checks
/// what it placed.
pub fn verified_tray(dir: &Path, digest: [u8; 32]) -> Result<VerifiedTray, String> {
    let folder = os::Directory::pin(dir).at(dir)?;
    let path = dir.join(LIVE[0]);
    let file = os::open(&path, false).at(&path)?;
    if disk_hash(&file).at(&path)? != digest {
        return Err(format!("{} is not the verified release", LIVE[0]));
    }
    Ok(VerifiedTray { path, _file: file, _folder: folder })
}

/// True only if this call replaced the pair. Re-check the installed pair under the install-directory
/// lock. Check permission before recording commit intent and again immediately before the first rename.
pub fn install(home: &Path, dir: &Path, original: &Offer, pair: &VerifiedPair, authorize: impl Fn() -> Result<(), String>) -> Result<bool, String> {
    install_with(home, dir, original, pair, &authorize, os::version, &proceed)
}

fn install_with(
    home: &Path,
    dir: &Path,
    original: &Offer,
    pair: &VerifiedPair,
    authorize: &dyn Fn() -> Result<(), String>,
    inspect: VersionReader,
    hook: Hook<'_>,
) -> Result<bool, String> {
    let folder = os::Directory::pin(dir).at(dir)?;
    let _lock = locked(dir)?;
    let ready = recover_locked(home, dir, &folder, hook)?;
    let (version, live) = read_pair(dir, LIVE, inspect)?;
    if version >= pair.version() {
        return Ok(false);
    }
    if !ready {
        return Err("a previous update is still in use; try after the old program has closed".into());
    }
    if original.version != version {
        return Err("the installed version changed; check the original release again".into());
    }
    let original_file = |i: usize| {
        let path = dir.join(LIVE[i]);
        let (size, hash) = original.expected(i);
        let unknown = || format!("{} does not match the official installed release; neither program was replaced", LIVE[i]);
        if live[i].metadata().at(&path)?.len() != size as u64 {
            return Err(unknown());
        }
        let stamp = Stamp::of(&live[i]).at(&path)?;
        if stamp.hash != hash {
            return Err(unknown());
        }
        Ok(stamp)
    };
    let old = [original_file(0)?, original_file(1)?];
    let commit = (|| {
        let (r, mut records, seal) = stage(home, dir, &folder, old, pair, hook)?;
        hook(Step::OnDisk)?;
        let (staged_version, staged) = read_pair(dir, STAGE, inspect)?;
        if staged_version != pair.version() {
            return Err("the staged executables do not have the verified release version".into());
        }
        drop((live, staged));
        let pin = |names: [&str; 2], expected: [Stamp; 2]| -> Result<[File; 2], String> {
            let p = names.map(|n| dir.join(n));
            Ok([movable(&p[0], expected[0]).at(&p[0])?, movable(&p[1], expected[1]).at(&p[1])?])
        };
        let (originals, replacements) = (pin(LIVE, r.old)?, pin(STAGE, r.new)?);
        hook(Step::Ready)?;
        for name in BACKUP {
            let p = dir.join(name);
            if present(&p).at(&p)? {
                return Err(format!("{} is occupied; nothing was replaced", p.display()));
            }
        }
        let path = dir.join(JOURNAL);
        authorize()?;
        records.append(&seal).at(&path)?;
        let (checked, authorized) = records.read().at(&path)?;
        if checked != r || !authorized {
            return Err("the update commit could not be verified on disk".into());
        }
        hook(Step::Committing)?;
        authorize()?;
        for i in 0..2 {
            let backup = dir.join(BACKUP[i]);
            os::rename(&originals[i], &backup).at(&backup)?;
            hook(Step::Saved(i))?;
            let live = dir.join(LIVE[i]);
            os::rename(&replacements[i], &live).at(&live)?;
            hook(Step::Placed(i))?;
        }
        Ok(())
    })();
    // On an ordinary error, restore now if possible. A crash or sharing conflict leaves the same
    // receipt and known names for the next recovery. Do not mask an unknown or unrestored state.
    let recovered = recover_locked(home, dir, &folder, hook);
    match (commit, recovered) {
        (Ok(()), Ok(_)) => Ok(true),
        (Err(e), Ok(_)) | (Ok(()), Err(e)) => Err(e),
        (Err(e), Err(recovery)) => Err(format!("{e}; update recovery: {recovery}")),
    }
}

#[cfg(test)]
pub(crate) mod fixture {
    use super::*;

    pub(super) fn version(_: &Path, f: &File, role: &str) -> Result<Version, String> {
        let bytes = read(f, MAX_EXE).map_err(|e| e.to_string())?;
        let (line, _) = bytes.split_at(bytes.iter().position(|&b| b == b'\n').ok_or("missing fixture version")?);
        let (name, number) = std::str::from_utf8(line).map_err(|_| "invalid fixture version")?.split_once(' ').ok_or("invalid fixture header")?;
        if name != role {
            return Err("wrong fixture executable role".into());
        }
        number.parse()
    }

    pub(crate) fn installed_version(dir: &Path) -> Result<Version, String> {
        version_with(dir, version)
    }

    pub(crate) fn install(home: &Path, dir: &Path, original: &Offer, pair: &VerifiedPair, authorize: impl Fn() -> Result<(), String>) -> Result<bool, String> {
        install_with(home, dir, original, pair, &authorize, version, &proceed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    const OLD: [&[u8]; 2] = [b"tiny-skill-sync.exe 1.0.0\nold tray", b"tss.exe 1.0.0\nold cli"];
    const NEW: [&[u8]; 2] = [b"tiny-skill-sync.exe 2.0.0\nnew tray", b"tss.exe 2.0.0\nnew cli"];
    const NEIGHBOR: &str = "tiny-skill-sync.settings.json";

    fn update() -> VerifiedPair {
        VerifiedPair::fixture("2.0.0".parse().unwrap(), NEW[0], NEW[1])
    }

    fn original() -> Offer {
        Offer::fixture_files("1.0.0".parse().unwrap(), OLD[0], OLD[1])
    }

    // Every "exe" is just labelled bytes. Do not start a child process to simulate a file in use.
    struct World(PathBuf, PathBuf); // executable directory, separate private home

    impl World {
        fn new() -> Self {
            let root = std::env::var_os("TSS_TEST_ROOT").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
            let dir = root.join(format!("tss-replacement-{}", crate::fsx::unique_suffix()));
            fs::create_dir(&dir).unwrap();
            let home = dir.with_extension("home");
            fs::create_dir(&home).unwrap();
            for i in 0..2 {
                fs::write(dir.join(LIVE[i]), OLD[i]).unwrap();
            }
            fs::write(dir.join(NEIGHBOR), b"the user's companion").unwrap();
            Self(dir, home)
        }

        fn file(&self, name: &str) -> Vec<u8> {
            fs::read(self.0.join(name)).unwrap()
        }

        fn check(&self, pair: [&[u8]; 2]) {
            assert_eq!(self.file(LIVE[0]), pair[0]);
            assert_eq!(self.file(LIVE[1]), pair[1]);
            assert_eq!(self.file(NEIGHBOR), b"the user's companion");
        }

        fn run(&self, hook: Hook<'_>) -> Result<bool, String> {
            install_with(&self.1, &self.0, &original(), &update(), &|| Ok(()), fixture::version, hook)
        }

        fn install(&self, pair: &VerifiedPair, authorize: impl Fn() -> Result<(), String>) -> Result<bool, String> {
            fixture::install(&self.1, &self.0, &original(), pair, authorize)
        }

        fn recover(&self) -> Result<(), String> {
            recover(&self.1, &self.0)
        }

        fn witness(&self) -> PathBuf {
            self.1.join(witness_name(&os::Directory::pin(&self.0).unwrap().id().unwrap()))
        }

        fn stop(&self, step: Step) {
            let hit = Cell::new(false);
            let result = catch_unwind(AssertUnwindSafe(|| {
                self.run(&|s| {
                    if s == step {
                        hit.set(true);
                        panic!("simulated stop at {s:?}");
                    }
                    Ok(())
                })
            }));
            assert!(hit.get() && result.is_err(), "did not reach {step:?}");
        }
    }

    impl Drop for World {
        fn drop(&mut self) {
            // Flat, private test directory. Even cleanup never recursively follows a link.
            for dir in [&self.0, &self.1] {
                if let Ok(entries) = fs::read_dir(dir) {
                    for entry in entries.flatten() {
                        let _ = fs::remove_file(entry.path()).or_else(|_| fs::remove_dir(entry.path()));
                    }
                }
                let _ = fs::remove_dir(dir);
            }
        }
    }

    #[test]
    fn passive_look_and_empty_recovery_create_nothing_and_a_newer_pair_wins() {
        let w = World::new();
        assert_eq!(fixture::installed_version(&w.0).unwrap().to_string(), "1.0.0");
        w.recover().unwrap();
        assert!(!w.0.join(LOCK).exists());
        assert!(w.install(&update(), || Ok(())).unwrap());
        for requested in ["1.0.0", "2.0.0"] {
            let stale = VerifiedPair::fixture(requested.parse().unwrap(), OLD[0], OLD[1]);
            assert!(!w.install(&stale, || panic!("no commit")).unwrap());
            w.check(NEW);
            assert!(!work_present(&w.0).unwrap());
        }
    }

    #[test]
    fn missing_unreadable_mixed_or_reversed_roles_never_reach_commit() {
        for bytes in [None, Some(&b"not an executable"[..]), Some(NEW[1]), Some(OLD[0])] {
            let w = World::new();
            match bytes {
                Some(b) => fs::write(w.0.join(LIVE[1]), b).unwrap(),
                None => fs::remove_file(w.0.join(LIVE[1])).unwrap(),
            }
            assert!(fixture::installed_version(&w.0).is_err());
            assert!(w.install(&update(), || panic!("no commit")).is_err());
            assert_eq!(w.file(LIVE[0]), OLD[0]);
            assert_eq!(fs::read(w.0.join(LIVE[1])).ok().as_deref(), bytes);
            assert!(!work_present(&w.0).unwrap());
        }
    }

    #[test]
    fn matching_roles_version_and_size_do_not_establish_ownership_of_either_original() {
        for i in 0..2 {
            let w = World::new();
            let mut custom = OLD[i].to_vec();
            *custom.last_mut().unwrap() ^= 1;
            fs::write(w.0.join(LIVE[i]), &custom).unwrap();
            assert_eq!(fixture::installed_version(&w.0).unwrap(), original().version);
            assert!(w.install(&update(), || panic!("never replace local bytes")).unwrap_err().contains("official installed release"));
            assert_eq!(w.file(LIVE[i]), custom);
            assert_eq!(w.file(LIVE[1 - i]), OLD[1 - i]);
            assert!(!w.witness().exists() && !work_present(&w.0).unwrap());
        }
        let w = World::new();
        let wrong_version = Offer::fixture_files("0.9.9".parse().unwrap(), OLD[0], OLD[1]);
        assert!(fixture::install(&w.1, &w.0, &wrong_version, &update(), || panic!("stale original metadata")).is_err());
        w.check(OLD);
    }

    #[test]
    fn authorization_sees_both_stages_and_the_originals_and_a_second_writer_cannot_enter() {
        let w = World::new();
        let other = World::new();
        let called = Cell::new(false);
        let result = w.install(&update(), || {
            called.set(true);
            w.check(OLD);
            for i in 0..2 {
                assert_eq!(w.file(STAGE[i]), NEW[i]);
                assert!(!w.0.join(BACKUP[i]).exists());
            }
            // Both workers open independent handles on this executable directory despite different homes.
            let lost = std::thread::scope(|s| {
                s.spawn(|| fixture::install(&other.1, &w.0, &original(), &update(), || panic!("second writer entered"))).join().unwrap()
            });
            assert!(lost.unwrap_err().contains("cannot lock"));
            assert!(w.recover().unwrap_err().contains("cannot lock"));
            Err("cancelled after staging".into())
        });
        assert!(called.get());
        assert!(result.unwrap_err().contains("cancelled after staging"));
        w.check(OLD);
        assert!(!work_present(&w.0).unwrap());
    }

    #[test]
    fn cancellation_or_a_settings_error_after_the_commit_flush_never_moves_an_original() {
        for reason in ["update cancelled", "settings could not be read"] {
            let w = World::new();
            let (flushed, calls) = (Cell::new(false), Cell::new(0));
            let held = RefCell::new(None);
            let result = install_with(
                &w.1,
                &w.0,
                &original(),
                &update(),
                &|| {
                    calls.set(calls.get() + 1);
                    if flushed.get() { Err(reason.into()) } else { Ok(()) }
                },
                fixture::version,
                &|s| {
                    match s {
                        Step::Committing => flushed.set(true),
                        Step::Saved(_) => panic!("a rename after permission was lost"),
                        // Leave the durable commit receipt and one stage in place across a restart.
                        Step::Retire(0) => *held.borrow_mut() = Some(OpenOptions::new().read(true).share_mode(1).open(w.0.join(STAGE[0])).unwrap()),
                        _ => {}
                    }
                    Ok(())
                },
            );
            assert_eq!(result, Err(reason.to_owned()));
            assert_eq!(calls.get(), 2);
            w.check(OLD);
            assert!(w.0.join(JOURNAL).exists());
            drop(held.into_inner());
            w.recover().unwrap();
            w.check(OLD);
            assert!(!work_present(&w.0).unwrap());
        }
    }

    #[test]
    fn a_reader_denying_delete_is_not_a_reason_to_copy_over_the_image() {
        let w = World::new();
        let hold = OpenOptions::new().read(true).share_mode(1).open(w.0.join(LIVE[1])).unwrap();
        assert!(w.install(&update(), || panic!("cannot acquire both images")).is_err());
        w.check(OLD);
        assert!(!work_present(&w.0).unwrap());
        drop(hold);
        // A running image permitting a rename is a different case: keep a read handle to the old
        // file across the swap. Reading that handle must still see the unmutilated old bytes.
        let old = File::open(w.0.join(LIVE[0])).unwrap();
        assert!(w.run(&proceed).unwrap());
        assert_eq!(read(&old, MAX_EXE).unwrap(), OLD[0]);
        w.check(NEW);
        drop(old);
        w.recover().unwrap();
        assert!(!work_present(&w.0).unwrap());
    }

    #[test]
    fn an_in_use_old_backup_defers_retirement_not_success_or_the_equal_version_noop() {
        let w = World::new();
        let held = RefCell::new(None);
        assert!(
            w.run(&|s| {
                if s == Step::Retire(0) {
                    *held.borrow_mut() = Some(OpenOptions::new().read(true).share_mode(1).open(w.0.join(BACKUP[0])).unwrap());
                }
                Ok(())
            })
            .unwrap()
        );
        w.check(NEW);
        assert_eq!(w.file(BACKUP[0]), OLD[0]);
        assert!(w.0.join(JOURNAL).exists());
        w.recover().unwrap();
        assert!(!w.install(&update(), || panic!("already installed")).unwrap());
        drop(held.into_inner());
        w.recover().unwrap();
        w.check(NEW);
        assert!(!work_present(&w.0).unwrap());
    }

    #[test]
    fn stops_at_durable_boundaries_rollback_until_both_are_live_then_only_finalize() {
        use Step::*;
        for step in [Recorded, Staged(0), Staged(1), OnDisk, Ready, Committing, Saved(0), Placed(0), Saved(1), Placed(1), Retire(0), Retire(1)] {
            let w = World::new();
            w.stop(step);
            assert!(LIVE.iter().any(|name| w.0.join(name).is_file()), "both executable names disappeared at {step:?}");
            w.recover().unwrap();
            let expected = if matches!(step, Placed(1) | Retire(_)) { NEW } else { OLD };
            w.check(expected);
            assert!(!work_present(&w.0).unwrap());
            w.recover().unwrap();
            w.check(expected);
        }
    }

    #[test]
    fn ordinary_errors_rollback_now_and_a_stop_during_rollback_can_resume() {
        let w = World::new();
        let fail = |s| if s == Step::Saved(1) { Err("second placement failed".into()) } else { Ok(()) };
        assert!(w.run(&fail).unwrap_err().contains("second placement failed"));
        w.check(OLD);
        assert!(!work_present(&w.0).unwrap());
        let interrupted = catch_unwind(AssertUnwindSafe(|| {
            w.run(&|s| {
                assert_ne!(s, Step::Restored(1), "stop after restoring the CLI");
                fail(s)
            })
        }));
        assert!(interrupted.is_err());
        assert!(w.0.join(JOURNAL).exists());
        w.recover().unwrap();
        w.check(OLD);
    }

    #[test]
    fn a_staged_file_with_plausible_metadata_but_different_disk_bytes_is_never_published() {
        let w = World::new();
        let mut cli = NEW[1].to_vec();
        cli.resize(256 * 1024 + 1, b'x');
        let pair = VerifiedPair::fixture("2.0.0".parse().unwrap(), NEW[0], &cli);
        let mut changed = cli;
        *changed.last_mut().unwrap() = b'y'; // same size and metadata; differs in the last hash chunk
        assert!(
            install_with(&w.1, &w.0, &original(), &pair, &|| panic!("unverified disk hash"), fixture::version, &|s| {
                if s == Step::OnDisk {
                    fs::write(w.0.join(STAGE[1]), &changed).unwrap();
                }
                Ok(())
            })
            .is_err()
        );
        assert!(w.recover().is_err());
        w.check(OLD);
        assert_eq!(w.file(STAGE[1]), changed);
    }

    #[test]
    fn tampered_or_copied_receipts_and_unproven_backups_are_never_replayed() {
        let w = World::new();
        w.stop(Step::Saved(0));
        let copy = World::new();
        fs::remove_file(copy.0.join(LIVE[0])).unwrap();
        for name in [LIVE[1], JOURNAL, BACKUP[0], STAGE[0], STAGE[1]] {
            fs::copy(w.0.join(name), copy.0.join(name)).unwrap();
        }
        assert!(copy.recover().is_err());
        assert!(!copy.0.join(LIVE[0]).exists());
        let sealed = w.file(JOURNAL);
        let mut bad = sealed.clone();
        let last = bad.len() - 1;
        bad[last] ^= 0xff;
        fs::write(w.0.join(JOURNAL), bad).unwrap();
        assert!(w.recover().is_err());
        assert!(!w.0.join(LIVE[0]).exists());
        fs::write(w.0.join(JOURNAL), sealed).unwrap();
        fs::write(w.0.join(BACKUP[0]), b"tiny-skill-sync.exe 1.0.0\nnot the original").unwrap();
        assert!(w.recover().is_err());
        assert!(!w.0.join(LIVE[0]).exists());
        assert_eq!(w.file(STAGE[0]), NEW[0]);
        // Even a byte-identical substitute is not the original file. Keep the existing object
        // under another name so its file ID cannot be immediately recycled.
        fs::rename(w.0.join(BACKUP[0]), w.0.join("keep-me")).unwrap();
        fs::write(w.0.join(BACKUP[0]), OLD[0]).unwrap();
        assert!(w.recover().is_err());
        assert!(!w.0.join(LIVE[0]).exists());
        assert_eq!(w.file(BACKUP[0]), OLD[0]);
        assert_eq!(w.file("keep-me"), b"tiny-skill-sync.exe 1.0.0\nnot the original");
    }

    #[test]
    fn private_evidence_cannot_be_substituted_copied_to_another_home_or_overwritten() {
        let (w, other) = (World::new(), World::new());
        let witness = w.witness();
        fs::write(&witness, b"unrelated private file").unwrap();
        assert!(w.recover().is_err() && w.run(&proceed).is_err());
        w.check(OLD);
        assert_eq!(fs::read(&witness).unwrap(), b"unrelated private file");
        fs::remove_file(&witness).unwrap();
        w.stop(Step::Saved(0));
        let private = fs::read(&witness).unwrap();
        assert!(recover(&other.1, &w.0).is_err()); // missing witness
        let foreign = other.1.join(witness.file_name().unwrap());
        fs::copy(&witness, &foreign).unwrap();
        assert!(recover(&other.1, &w.0).is_err()); // even a byte-identical copy has the wrong identities
        assert_eq!(fs::read(&foreign).unwrap(), private);
        fs::write(&witness, b"damaged private receipt").unwrap();
        assert!(w.recover().is_err());
        assert!(!w.0.join(LIVE[0]).exists());
        assert!(w.0.join(JOURNAL).exists());
        assert_eq!(w.file(BACKUP[0]), OLD[0]);
        assert_eq!(w.file(STAGE[0]), NEW[0]);
        fs::write(&witness, private).unwrap();
        w.recover().unwrap();
        w.check(OLD);
        assert!(!witness.exists());
        assert!(foreign.exists()); // never sweep a different home
    }

    #[test]
    fn the_corresponding_private_evidence_must_live_outside_the_executable_tree() {
        let w = World::new();
        let nested = w.0.join("not-private");
        assert!(fixture::install(&nested, &w.0, &original(), &update(), || panic!("no independent home")).is_err());
        assert!(!nested.exists());
        assert!(!work_present(&w.0).unwrap());
        w.check(OLD);
    }

    #[test]
    fn a_prepare_receipt_is_not_evidence_that_a_commit_was_authorized() {
        let w = World::new();
        w.stop(Step::Ready); // authorize has not run
        // Simulate out-of-order shared-folder changes that look just like a completed swap.
        for i in 0..2 {
            fs::rename(w.0.join(LIVE[i]), w.0.join(BACKUP[i])).unwrap();
            fs::rename(w.0.join(STAGE[i]), w.0.join(LIVE[i])).unwrap();
        }
        assert!(w.recover().is_err());
        for i in 0..2 {
            assert_eq!(w.file(BACKUP[i]), OLD[i]);
        }
        assert!(w.0.join(JOURNAL).exists());
    }

    #[test]
    fn a_failed_second_backup_never_overwrites_or_sweeps_an_unknown_neighbor() {
        let w = World::new();
        let fail = |s| {
            if s == Step::Placed(0) {
                fs::write(w.0.join(BACKUP[1]), b"someone else's file").unwrap();
            }
            Ok(())
        };
        assert!(w.run(&fail).is_err());
        assert!(w.recover().is_err());
        assert_eq!(w.file(BACKUP[1]), b"someone else's file");
        assert_eq!(w.file(BACKUP[0]), OLD[0]);
        assert_eq!(w.file(LIVE[1]), OLD[1]);
        assert_eq!(w.file(STAGE[1]), NEW[1]);
        fs::rename(w.0.join(BACKUP[1]), w.0.join("keep-me")).unwrap();
        w.recover().unwrap();
        w.check(OLD);
        assert_eq!(w.file("keep-me"), b"someone else's file");
    }

    #[test]
    fn unauthenticated_work_names_and_links_are_left_alone() {
        let w = World::new();
        fs::write(w.0.join(LOCK), b"unrelated").unwrap();
        assert!(w.run(&proceed).is_err());
        assert_eq!(w.file(LOCK), b"unrelated");
        fs::remove_file(w.0.join(LOCK)).unwrap();
        fs::create_dir(w.0.join(STAGE[0])).unwrap();
        let child = w.0.join(STAGE[0]).join("mine");
        fs::write(&child, b"preserve").unwrap();
        assert!(w.recover().is_err());
        assert!(w.run(&proceed).is_err());
        assert_eq!(fs::read(&child).unwrap(), b"preserve");
        fs::remove_file(child).unwrap();
        fs::remove_dir(w.0.join(STAGE[0])).unwrap();
        fs::remove_file(w.0.join(LIVE[1])).unwrap();
        fs::hard_link(w.0.join(NEIGHBOR), w.0.join(LIVE[1])).unwrap();
        assert!(w.run(&proceed).is_err());
        assert_eq!(w.file(LIVE[1]), b"the user's companion");
        assert_eq!(w.file(NEIGHBOR), b"the user's companion");
    }

    #[test]
    fn a_link_in_an_ancestor_does_not_redirect_the_update() {
        let (w, outside) = (World::new(), World::new());
        let alias = w.0.join("alias");
        if let Err(e) = std::os::windows::fs::symlink_dir(&outside.0, &alias) {
            if e.raw_os_error() == Some(1314) {
                return; // Windows has not granted symlink creation to this test user.
            }
            panic!("cannot create test link: {e}");
        }
        let sub = outside.0.join("app");
        fs::create_dir(&sub).unwrap();
        for i in 0..2 {
            fs::write(sub.join(LIVE[i]), OLD[i]).unwrap();
        }
        assert!(fixture::install(&w.1, &alias.join("app"), &original(), &update(), || panic!("outside install folder")).is_err());
        assert!(!sub.join(LOCK).exists());
        for i in 0..2 {
            assert_eq!(fs::read(sub.join(LIVE[i])).unwrap(), OLD[i]);
            fs::remove_file(sub.join(LIVE[i])).unwrap();
        }
        fs::remove_dir(sub).unwrap();
        fs::remove_dir(alias).unwrap();
    }
}
