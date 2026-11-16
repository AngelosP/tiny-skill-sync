//! Small file helpers on top of `std::fs` that the rest of the app relies on for safety.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::sleep;
use std::time::Duration;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A short suffix that is unique in this process and very unlikely to repeat across runs.
pub fn unique_suffix() -> String {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    format!("{:x}{:x}{:x}", std::process::id(), t & 0xffff_ffff, n)
}

/// Writes a file so a reader sees either the old or the new content, never a partial file, even after a crash.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = path.with_file_name(format!("{name}.{}.tmp", unique_suffix()));
    let result = (|| {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        rename_retry(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn is_transient(e: &io::Error) -> bool {
    // Access denied and sharing violations are what a virus scanner or the search indexer holding a file open looks like.
    matches!(e.raw_os_error(), Some(5) | Some(32) | Some(33)) || e.kind() == io::ErrorKind::PermissionDenied
}

fn retry<T>(mut op: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let mut delay = 15;
    for _ in 0..7 {
        match op() {
            Err(e) if is_transient(&e) => {
                sleep(Duration::from_millis(delay));
                delay = (delay * 2).min(250);
            }
            other => return other,
        }
    }
    op()
}

pub fn rename_retry(from: &Path, to: &Path) -> io::Result<()> {
    retry(|| fs::rename(from, to))
}

/// Removes a folder tree; a missing folder counts as success.
pub fn remove_dir_all_retry(path: &Path) -> io::Result<()> {
    match retry(|| fs::remove_dir_all(path)) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

pub fn exists_no_follow(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// True for symbolic links and junctions, which the app never writes through or deletes through.
pub fn is_link(path: &Path) -> bool {
    fs::symlink_metadata(path).map(|m| m.file_type().is_symlink()).unwrap_or(false)
}

pub fn join_rel(base: &Path, rel: &str) -> PathBuf {
    let mut p = base.to_path_buf();
    for part in rel.split('/') {
        p.push(part);
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tss-fsx-{name}-{}", unique_suffix()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn atomic_write_creates_and_replaces() {
        let dir = temp("atomic");
        let file = dir.join("sub").join("state.json");
        write_atomic(&file, b"one").unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"one");
        write_atomic(&file, b"two").unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"two");
        let leftovers: Vec<_> = fs::read_dir(file.parent().unwrap()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(leftovers.len(), 1, "no temp files left: {leftovers:?}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn atomic_write_waits_for_a_program_that_has_the_file_open_for_a_moment() {
        use std::fs::OpenOptions;
        use std::os::windows::fs::OpenOptionsExt;

        let dir = temp("atomic-busy");
        let file = dir.join("state.json");
        write_atomic(&file, b"old").unwrap();
        // What a virus scanner or the search indexer looks like: the file is open and cannot be replaced yet.
        let holder = OpenOptions::new().read(true).share_mode(1).open(&file).unwrap();
        let release = std::thread::spawn(move || {
            sleep(Duration::from_millis(150));
            drop(holder);
        });

        write_atomic(&file, b"new").unwrap();

        release.join().unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"new");
        let leftovers: Vec<_> = fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(leftovers.len(), 1, "no temp files left: {leftovers:?}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn atomic_write_gives_up_and_leaves_the_old_file_when_the_file_stays_open() {
        use std::fs::OpenOptions;
        use std::os::windows::fs::OpenOptionsExt;

        let dir = temp("atomic-stuck");
        let file = dir.join("state.json");
        write_atomic(&file, b"old").unwrap();
        let holder = OpenOptions::new().read(true).share_mode(1).open(&file).unwrap();

        assert!(write_atomic(&file, b"new").is_err());

        drop(holder);
        assert_eq!(fs::read(&file).unwrap(), b"old");
        let leftovers: Vec<_> = fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(leftovers.len(), 1, "no temp files left: {leftovers:?}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn remove_handles_read_only_files_and_missing_paths() {
        let dir = temp("remove");
        let tree = dir.join("t");
        fs::create_dir_all(tree.join("a")).unwrap();
        let f = tree.join("a").join("ro.txt");
        fs::write(&f, b"x").unwrap();
        let mut perm = fs::metadata(&f).unwrap().permissions();
        perm.set_readonly(true);
        fs::set_permissions(&f, perm).unwrap();
        remove_dir_all_retry(&tree).unwrap();
        assert!(!tree.exists());
        remove_dir_all_retry(&tree).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn suffixes_do_not_repeat() {
        let a = unique_suffix();
        let b = unique_suffix();
        assert_ne!(a, b);
    }

    #[test]
    fn join_rel_splits_on_forward_slashes() {
        assert_eq!(join_rel(Path::new(r"C:\x"), "a/b/c.md"), PathBuf::from(r"C:\x\a\b\c.md"));
    }
}
