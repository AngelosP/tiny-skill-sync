//! File-system facts that `std` does not give: a per-file change fingerprint, the identity of the
//! volume's change journal, and a streaming Git blob hash of a file.
//!
//! Evidence for how these are used is in the plan (spikes S1a and S1b): the file's USN moved in every one of
//! 4,800 same-size edits, while timestamps alone missed up to 88 % of them. Bulk directory listings can be stale
//! while a writer holds a file open, so every managed file is asked through its own handle.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::mem::offset_of;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_FILES, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_SEQUENTIAL_SCAN, FILE_FULL_DIR_INFO, FILE_ID_INFO, FILE_INFO_BY_HANDLE_CLASS, FILE_LIST_DIRECTORY,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO, FileBasicInfo, FileFullDirectoryInfo,
    FileFullDirectoryRestartInfo, FileIdInfo, FileStandardInfo, GetFileInformationByHandle, GetFileInformationByHandleEx, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_FILE_USN_DATA};

use crate::hash::BlobHasher;
use crate::state::{Fingerprint, VolumeId};
use crate::win::wide_os;

const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
const ERROR_DIRECTORY: i32 = 267;
/// Opens the path itself: a link is never followed, and a folder can be opened at all.
const NO_FOLLOW: u32 = FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT;

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn open(path: &Path, access: u32, flags: u32) -> io::Result<Handle> {
    let wide = wide_os(path.as_os_str());
    let h = unsafe { CreateFileW(wide.as_ptr(), access, SHARE_ALL, null(), OPEN_EXISTING, flags, null_mut()) };
    if h == INVALID_HANDLE_VALUE { Err(io::Error::last_os_error()) } else { Ok(Handle(h)) }
}

fn info<T>(h: &Handle, class: FILE_INFO_BY_HANDLE_CLASS) -> io::Result<T> {
    // SAFETY: the info structures used here are plain integers, for which all-zero is a valid value.
    let mut value: T = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandleEx(h.0, class, (&mut value as *mut T).cast(), size_of::<T>() as u32) };
    if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(value) }
}

fn is_missing(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(ERROR_DIRECTORY)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    /// Any reparse point: symbolic link, junction, cloud placeholder. Never followed.
    Link,
}

fn kind_of(attributes: u32) -> Kind {
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        Kind::Link
    } else if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        Kind::Dir
    } else {
        Kind::File
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Probe {
    pub kind: Kind,
    pub fp: Fingerprint,
}

/// The file's current USN, or 0 when the volume has no change journal for it.
fn file_usn(h: &Handle) -> u64 {
    let mut out = [0u8; 1024];
    let mut returned = 0u32;
    let ok = unsafe { DeviceIoControl(h.0, FSCTL_READ_FILE_USN_DATA, null(), 0, out.as_mut_ptr().cast(), out.len() as u32, &mut returned, null_mut()) };
    if ok == 0 || returned < 48 {
        return 0;
    }
    let offset = match u16::from_le_bytes([out[4], out[5]]) {
        2 => 24,
        3 => 40,
        _ => return 0,
    };
    u64::from_le_bytes(out[offset..offset + 8].try_into().unwrap())
}

/// Reads the fingerprint of one path through an attribute-only handle: no content is read and no lock is taken.
/// Returns `None` when the path (or a parent) does not exist.
pub fn probe(path: &Path) -> io::Result<Option<Probe>> {
    let h = match open(path, FILE_READ_ATTRIBUTES, NO_FOLLOW) {
        Ok(h) => h,
        Err(e) if is_missing(&e) => return Ok(None),
        Err(e) => return Err(e),
    };
    let basic: FILE_BASIC_INFO = info(&h, FileBasicInfo)?;
    let standard: FILE_STANDARD_INFO = info(&h, FileStandardInfo)?;
    let kind = kind_of(basic.FileAttributes);
    // A volume without 128-bit file IDs (some network shares) simply yields an untrusted all-zero identity.
    let file_id = info::<FILE_ID_INFO>(&h, FileIdInfo)
        .map(|id| {
            let b = id.FileId.Identifier;
            [u64::from_le_bytes(b[..8].try_into().unwrap()), u64::from_le_bytes(b[8..].try_into().unwrap())]
        })
        .unwrap_or([0, 0]);
    let usn = if kind == Kind::File { file_usn(&h) } else { 0 };
    Ok(Some(Probe {
        kind,
        fp: Fingerprint { file_id, size: standard.EndOfFile.max(0) as u64, write: basic.LastWriteTime as u64, change: basic.ChangeTime as u64, usn },
    }))
}

/// Identity of the volume that holds `dir` and of its change journal (0 when there is none or it cannot be queried).
/// A stored USN means something only while both numbers are unchanged.
pub fn volume_id(dir: &Path) -> io::Result<VolumeId> {
    let h = open(dir, FILE_READ_ATTRIBUTES | FILE_LIST_DIRECTORY, FILE_FLAG_BACKUP_SEMANTICS)?;
    let serial = match info::<FILE_ID_INFO>(&h, FileIdInfo) {
        Ok(id) => id.VolumeSerialNumber,
        // Network shares and some older file systems have no 128-bit file IDs; the older call still names the drive.
        Err(_) => legacy_volume_serial(&h)?,
    };
    let mut out = [0u8; 128];
    let mut returned = 0u32;
    let ok = unsafe { DeviceIoControl(h.0, FSCTL_QUERY_USN_JOURNAL, null(), 0, out.as_mut_ptr().cast(), out.len() as u32, &mut returned, null_mut()) };
    let journal = if ok != 0 && returned >= 8 { u64::from_le_bytes(out[..8].try_into().unwrap()) } else { 0 };
    Ok(VolumeId { serial, journal })
}

fn legacy_volume_serial(h: &Handle) -> io::Result<u64> {
    // SAFETY: the structure is plain integers, for which all-zero is a valid value.
    let mut data: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(h.0, &mut data) } == 0 { Err(io::Error::last_os_error()) } else { Ok(u64::from(data.dwVolumeSerialNumber)) }
}

/// Hashes files as Git blobs. Keeps its read buffer between calls.
pub struct FileHasher {
    buf: Vec<u8>,
}

impl Default for FileHasher {
    fn default() -> Self {
        FileHasher { buf: vec![0; 64 * 1024] }
    }
}

/// A file read by [`FileHasher::read`].
#[derive(Debug, PartialEq, Eq)]
pub struct FileRead {
    pub blob: String,
    pub len: u64,
    /// The file's bytes, when there were few enough to keep.
    pub bytes: Option<Vec<u8>>,
}

impl FileHasher {
    /// Git blob SHA-1 of the file as it is now. Fails with `InvalidData` if the file changes size while being read.
    pub fn blob(&mut self, path: &Path) -> io::Result<String> {
        let mut file = OpenOptions::new().read(true).custom_flags(FILE_FLAG_SEQUENTIAL_SCAN).open(path)?;
        let len = file.metadata()?.len();
        self.hash(&mut file, len, None)
    }

    /// Reads a plain file without following a link: its Git blob hash, and its bytes when there are at most `keep`.
    /// A file longer than `limit` is not read (`FileTooLarge`); a link or a folder is not read either (`InvalidInput`).
    pub fn read(&mut self, path: &Path, keep: u64, limit: u64) -> io::Result<FileRead> {
        let mut file = OpenOptions::new().read(true).custom_flags(NO_FOLLOW | FILE_FLAG_SEQUENTIAL_SCAN).open(path)?;
        let meta = file.metadata()?;
        if kind_of(meta.file_attributes()) != Kind::File {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "it is a link or a folder"));
        }
        let len = meta.len();
        if len > limit {
            return Err(io::ErrorKind::FileTooLarge.into());
        }
        let mut bytes = (len <= keep).then(|| Vec::with_capacity(len as usize));
        let blob = self.hash(&mut file, len, bytes.as_mut())?;
        Ok(FileRead { blob, len, bytes })
    }

    fn hash(&mut self, file: &mut File, len: u64, mut keep: Option<&mut Vec<u8>>) -> io::Result<String> {
        let mut hasher = BlobHasher::new(len);
        let mut total = 0u64;
        loop {
            let n = file.read(&mut self.buf)?;
            if n == 0 {
                break;
            }
            total += n as u64;
            if total > len {
                break;
            }
            hasher.update(&self.buf[..n]);
            if let Some(bytes) = keep.as_mut() {
                bytes.extend_from_slice(&self.buf[..n]);
            }
        }
        if total != len {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "the file changed while it was being read"));
        }
        Ok(hasher.finish_hex())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    /// Path below the walked folder, separated by `/`.
    pub rel: String,
    /// `File` or `Link`; folders are walked, never listed.
    pub kind: Kind,
}

/// Lists every file below `root` without following links. Empty folders are not reported. Fails unless `root` is a
/// plain folder.
pub fn list_files(root: &Path) -> io::Result<Vec<Found>> {
    match Folder::open(root)? {
        Some(folder) => folder.list_files(),
        None => Err(io::ErrorKind::NotFound.into()),
    }
}

/// A path opened once, without following a link, to learn what it is and, when it is a folder, to list it through
/// the same handle. A skill is checked with one open of its folder instead of two, which matters when there are
/// hundreds of small skills (plan, "Measurements of the real tray application").
pub struct Folder {
    h: Handle,
    path: PathBuf,
    pub kind: Kind,
}

impl Folder {
    /// Returns `None` when the path (or a parent) does not exist.
    pub fn open(path: &Path) -> io::Result<Option<Folder>> {
        let h = match open(path, FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES, NO_FOLLOW) {
            Ok(h) => h,
            Err(e) if is_missing(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        let kind = kind_of(info::<FILE_BASIC_INFO>(&h, FileBasicInfo)?.FileAttributes);
        Ok(Some(Folder { h, path: path.to_path_buf(), kind }))
    }

    /// Lists every file below the folder without following links. Empty folders are not reported. Fails for
    /// anything that is not a plain folder.
    pub fn list_files(&self) -> io::Result<Vec<Found>> {
        if self.kind != Kind::Dir {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{} is not a plain folder", self.path.display())));
        }
        let mut out = Vec::new();
        // Holds many entries per call; u64 keeps it aligned as the entries need.
        let mut buf = vec![0u64; 2048];
        walk(&self.h, &self.path, "", &mut buf, &mut out)?;
        Ok(out)
    }
}

const DOT: u16 = b'.' as u16;
const NAME_AT: usize = offset_of!(FILE_FULL_DIR_INFO, FileName);

fn garbled() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "the folder listing is garbled")
}

fn read_u32(entry: &[u8], at: usize) -> io::Result<u32> {
    entry.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])).ok_or_else(garbled)
}

/// Whether a failed directory query means the listing is complete. Only the first query may say "not found" (a file
/// system without "." and ".." entries says so for an empty folder); later it is an error, so a listing cut short is
/// never taken as complete.
fn listing_ended(code: Option<i32>, first: bool) -> bool {
    code == Some(ERROR_NO_MORE_FILES as i32) || (first && code == Some(ERROR_FILE_NOT_FOUND as i32))
}

fn walk(h: &Handle, dir: &Path, prefix: &str, buf: &mut [u64], out: &mut Vec<Found>) -> io::Result<()> {
    let mut folders = Vec::new();
    let mut first = true;
    loop {
        // The handle keeps its place in the listing, so the first query starts again from the beginning.
        let class = if first { FileFullDirectoryRestartInfo } else { FileFullDirectoryInfo };
        let ok = unsafe { GetFileInformationByHandleEx(h.0, class, buf.as_mut_ptr().cast(), size_of_val(buf) as u32) };
        if ok == 0 {
            let e = io::Error::last_os_error();
            if listing_ended(e.raw_os_error(), first) {
                break;
            }
            return Err(e);
        }
        first = false;
        // SAFETY: the buffer is initialised memory owned by the caller; it is only read here, as bytes.
        let bytes: &[u8] = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast(), size_of_val(buf)) };
        let mut at = 0;
        loop {
            let entry = bytes.get(at..).ok_or_else(garbled)?;
            let next = read_u32(entry, offset_of!(FILE_FULL_DIR_INFO, NextEntryOffset))? as usize;
            let attributes = read_u32(entry, offset_of!(FILE_FULL_DIR_INFO, FileAttributes))?;
            let len = read_u32(entry, offset_of!(FILE_FULL_DIR_INFO, FileNameLength))? as usize;
            let name: Vec<u16> = entry.get(NAME_AT..NAME_AT + len).ok_or_else(garbled)?.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            if name != [DOT] && name != [DOT, DOT] {
                let name = OsString::from_wide(&name);
                let text = name.to_string_lossy();
                let rel = if prefix.is_empty() { text.into_owned() } else { format!("{prefix}/{text}") };
                match kind_of(attributes) {
                    Kind::Dir => folders.push((name, rel)),
                    kind => out.push(Found { rel, kind }),
                }
            }
            if next == 0 {
                break;
            }
            at += next;
        }
    }
    for (name, rel) in folders {
        let path = dir.join(&name);
        let sub = open(&path, FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES, NO_FOLLOW)?;
        // It was a plain folder when it was listed; if it has become a link since, it is reported, never followed.
        match kind_of(info::<FILE_BASIC_INFO>(&sub, FileBasicInfo)?.FileAttributes) {
            Kind::Dir => walk(&sub, &path, &rel, buf, out)?,
            kind => out.push(Found { rel, kind }),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tss-winfs-{name}-{}", crate::fsx::unique_suffix()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn missing_paths_are_none_not_errors() {
        let dir = temp("missing");
        assert!(probe(&dir.join("nope")).unwrap().is_none());
        fs::write(dir.join("f"), b"x").unwrap();
        assert!(probe(&dir.join("f").join("child")).unwrap().is_none());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn files_and_folders_are_told_apart_and_files_have_a_usn() {
        let dir = temp("kinds");
        fs::write(dir.join("f.txt"), b"hello\n").unwrap();
        let f = probe(&dir.join("f.txt")).unwrap().unwrap();
        assert_eq!(f.kind, Kind::File);
        assert_eq!(f.fp.size, 6);
        assert_ne!(f.fp.usn, 0, "this machine's temp volume should keep a change journal");
        assert_ne!(f.fp.file_id, [0, 0]);
        let d = probe(&dir).unwrap().unwrap();
        assert_eq!(d.kind, Kind::Dir);
        assert_eq!(d.fp.usn, 0);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn every_kind_of_write_moves_the_fingerprint_and_reading_does_not() {
        let dir = temp("moves");
        let path = dir.join("f.txt");
        fs::write(&path, b"aaaa").unwrap();
        let base = probe(&path).unwrap().unwrap().fp;
        assert_eq!(probe(&path).unwrap().unwrap().fp, base, "an unchanged file reads back identically");
        FileHasher::default().blob(&path).unwrap();
        assert_eq!(probe(&path).unwrap().unwrap().fp, base, "reading does not change it");

        fs::write(&path, b"bbbb").unwrap();
        let same_size = probe(&path).unwrap().unwrap().fp;
        assert_ne!(same_size, base, "same-size rewrite");
        assert_ne!(same_size.usn, base.usn);

        fs::write(&path, b"bbbb").unwrap();
        let identical = probe(&path).unwrap().unwrap().fp;
        assert_ne!(identical.usn, same_size.usn, "an identical rewrite still moves the USN (it is only a hint, the hash decides)");

        let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(3600)).unwrap();
        drop(file);
        let touched = probe(&path).unwrap().unwrap().fp;
        assert_ne!(touched, identical, "a timestamp-only change");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn replacing_a_file_by_rename_changes_its_identity() {
        let dir = temp("replace");
        let path = dir.join("f.txt");
        fs::write(&path, b"one").unwrap();
        let base = probe(&path).unwrap().unwrap().fp;
        fs::write(dir.join("new.tmp"), b"two").unwrap();
        fs::rename(dir.join("new.tmp"), &path).unwrap();
        let after = probe(&path).unwrap().unwrap().fp;
        assert_ne!(after.file_id, base.file_id);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_volume_has_an_identity_and_a_journal() {
        let dir = temp("volume");
        let v = volume_id(&dir).unwrap();
        assert_ne!(v.serial, 0);
        assert_ne!(v.journal, 0, "FSCTL_QUERY_USN_JOURNAL should work for a standard user on NTFS");
        assert_eq!(volume_id(&dir).unwrap(), v);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn hashing_matches_git_and_handles_empty_and_large_files() {
        let dir = temp("hash");
        let mut h = FileHasher::default();
        fs::write(dir.join("hello"), b"hello\n").unwrap();
        assert_eq!(h.blob(&dir.join("hello")).unwrap(), "ce013625030ba8dba906f756967f9e9ca394464a");
        fs::write(dir.join("empty"), b"").unwrap();
        assert_eq!(h.blob(&dir.join("empty")).unwrap(), "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
        let big: Vec<u8> = (0..200_000u32).map(|i| ((i * 7 + 13) % 251) as u8).collect();
        fs::write(dir.join("big"), &big).unwrap();
        assert_eq!(h.blob(&dir.join("big")).unwrap(), "4640ffc49ec7665e2d82c862e3cf58e13f0b7927");
        assert!(h.blob(&dir.join("absent")).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn listing_reports_files_and_links_but_not_folders_and_never_follows_links() {
        let dir = temp("list");
        fs::create_dir_all(dir.join("a").join("b")).unwrap();
        fs::create_dir_all(dir.join("empty")).unwrap();
        fs::write(dir.join("top.md"), b"1").unwrap();
        fs::write(dir.join("a").join("b").join("deep.md"), b"2").unwrap();
        let outside = temp("list-outside");
        fs::write(outside.join("secret.txt"), b"s").unwrap();
        let made = std::process::Command::new("cmd").args(["/C", "mklink", "/J"]).arg(dir.join("junction")).arg(&outside).output().unwrap().status.success();
        assert!(made, "creating a junction needs no privileges");

        let mut found = list_files(&dir).unwrap();
        found.sort_by(|a, b| a.rel.cmp(&b.rel));
        assert_eq!(
            found,
            vec![
                Found { rel: "a/b/deep.md".into(), kind: Kind::File },
                Found { rel: "junction".into(), kind: Kind::Link },
                Found { rel: "top.md".into(), kind: Kind::File },
            ]
        );
        assert_eq!(probe(&dir.join("junction")).unwrap().unwrap().kind, Kind::Link);
        // Remove the junction itself before the tree so the outside folder is never touched.
        fs::remove_dir(dir.join("junction")).unwrap();
        assert!(outside.join("secret.txt").exists());
        fs::remove_dir_all(dir).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn a_folder_is_looked_at_and_listed_through_one_handle_and_a_link_is_never_listed() {
        let dir = temp("folder");
        fs::write(dir.join("f.txt"), b"1").unwrap();
        let outside = temp("folder-outside");
        fs::write(outside.join("secret.txt"), b"s").unwrap();
        let junction = dir.join("junction");
        let made = std::process::Command::new("cmd").args(["/C", "mklink", "/J"]).arg(&junction).arg(&outside).output().unwrap().status.success();
        assert!(made, "creating a junction needs no privileges");

        assert!(Folder::open(&dir.join("absent")).unwrap().is_none());
        assert!(Folder::open(&dir.join("f.txt").join("child")).unwrap().is_none());
        assert_eq!(Folder::open(&dir.join("f.txt")).unwrap().unwrap().kind, Kind::File);
        let link = Folder::open(&junction).unwrap().unwrap();
        assert_eq!(link.kind, Kind::Link);
        assert!(link.list_files().is_err(), "a link is reported, never listed");
        assert!(list_files(&junction).is_err(), "not even when it is the folder asked for");

        let folder = Folder::open(&dir).unwrap().unwrap();
        assert_eq!(folder.kind, Kind::Dir);
        let mut found = folder.list_files().unwrap();
        found.sort_by(|a, b| a.rel.cmp(&b.rel));
        assert_eq!(found, vec![Found { rel: "f.txt".into(), kind: Kind::File }, Found { rel: "junction".into(), kind: Kind::Link }]);

        fs::remove_dir(&junction).unwrap();
        assert!(outside.join("secret.txt").exists());
        fs::remove_dir_all(dir).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn a_file_is_read_with_its_hash_and_kept_only_when_small_and_a_link_is_never_read() {
        let dir = temp("read");
        fs::write(dir.join("f.txt"), b"hello\n").unwrap();
        let outside = temp("read-outside");
        fs::write(outside.join("secret.txt"), b"s").unwrap();
        let junction = dir.join("junction");
        let made = std::process::Command::new("cmd").args(["/C", "mklink", "/J"]).arg(&junction).arg(&outside).output().unwrap().status.success();
        assert!(made, "creating a junction needs no privileges");
        let mut hasher = FileHasher::default();
        let blob = crate::hash::blob_hex(b"hello\n");
        let read = |hasher: &mut FileHasher, keep, limit| hasher.read(&dir.join("f.txt"), keep, limit);

        assert_eq!(read(&mut hasher, 6, 6).unwrap(), FileRead { blob: blob.clone(), len: 6, bytes: Some(b"hello\n".to_vec()) });
        assert_eq!(read(&mut hasher, 5, 6).unwrap(), FileRead { blob, len: 6, bytes: None }, "hashed, but not kept");
        assert_eq!(read(&mut hasher, 6, 5).unwrap_err().kind(), io::ErrorKind::FileTooLarge);
        assert_eq!(hasher.read(&junction, 100, 100).unwrap_err().kind(), io::ErrorKind::InvalidInput, "the link is looked at, not followed");

        fs::remove_dir(&junction).unwrap();
        fs::remove_dir_all(dir).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn a_folder_lists_the_same_files_every_time_it_is_asked() {
        let dir = temp("twice");
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("a.md"), b"1").unwrap();
        fs::write(dir.join("sub").join("b.md"), b"2").unwrap();
        let folder = Folder::open(&dir).unwrap().unwrap();
        let mut first = folder.list_files().unwrap();
        let mut second = folder.list_files().unwrap();
        first.sort_by(|a, b| a.rel.cmp(&b.rel));
        second.sort_by(|a, b| a.rel.cmp(&b.rel));
        assert_eq!(first.len(), 2);
        assert_eq!(second, first, "a second listing through the same handle must not come back empty");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_folder_too_big_for_one_buffer_is_listed_in_full() {
        let dir = temp("many");
        fs::create_dir_all(dir.join("nested")).unwrap();
        let long = "x".repeat(60);
        for i in 0..600 {
            fs::write(dir.join(format!("{long}-{i:04}.md")), b"").unwrap();
        }
        for i in 0..300 {
            fs::write(dir.join("nested").join(format!("{long}-{i:04}.md")), b"").unwrap();
        }
        let mut expected: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n != "nested")
            .chain(fs::read_dir(dir.join("nested")).unwrap().map(|e| format!("nested/{}", e.unwrap().file_name().into_string().unwrap())))
            .collect();
        expected.sort();
        let mut found: Vec<String> = list_files(&dir).unwrap().into_iter().map(|f| f.rel).collect();
        found.sort();
        assert_eq!(found.len(), 900);
        assert_eq!(found, expected);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn only_the_first_query_of_a_listing_may_say_not_found() {
        let no_more = Some(ERROR_NO_MORE_FILES as i32);
        let not_found = Some(ERROR_FILE_NOT_FOUND as i32);
        assert!(listing_ended(no_more, true));
        assert!(listing_ended(no_more, false));
        assert!(listing_ended(not_found, true), "an empty folder on a file system without '.' and '..'");
        assert!(!listing_ended(not_found, false), "a listing cut short must be an error, never a complete listing");
        assert!(!listing_ended(Some(5), true));
        assert!(!listing_ended(None, true));
    }
}
