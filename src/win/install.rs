//! Windows primitives for replacing the two executables. None of these loads a program or runs it.

use std::fs::{File, OpenOptions};
use std::io;
use std::mem::offset_of;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Component, Path, Prefix};
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FILE_DISPOSITION_INFO,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_RENAME_INFO, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_VER_GET_NEUTRAL, FileAttributeTagInfo, FileDispositionInfo, FileIdInfo, FileRenameInfo, GetFileInformationByHandle,
    GetFileInformationByHandleEx, GetFileVersionInfoExW, GetFileVersionInfoSizeExW, ReOpenFile, SetFileInformationByHandle, VS_FIXEDFILEINFO, VerQueryValueW,
};

use crate::release::Version;
use crate::win::{wide, wide_os};

const NO_LINK: u32 = FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT;

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Hold an old image in the way the Windows filesystem sees a mapped program, without running any code.
#[cfg(test)]
pub(crate) fn map_old_image_read_only(path: &Path) -> io::Result<impl Drop + use<>> {
    use std::os::windows::io::OwnedHandle;
    use std::ptr::null;
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, PAGE_READONLY, SEC_IMAGE_NO_EXECUTE, UnmapViewOfFile,
    };
    struct View(MEMORY_MAPPED_VIEW_ADDRESS);
    impl Drop for View {
        fn drop(&mut self) {
            unsafe { UnmapViewOfFile(self.0) };
        }
    }

    let file = File::open(path)?;
    let section = unsafe { CreateFileMappingW(file.as_raw_handle(), null(), PAGE_READONLY | SEC_IMAGE_NO_EXECUTE, 0, 0, null()) };
    if section.is_null() {
        return Err(io::Error::last_os_error());
    }
    let section = unsafe { OwnedHandle::from_raw_handle(section) };
    let view = unsafe { MapViewOfFile(section.as_raw_handle(), FILE_MAP_READ, 0, 0, 0) };
    if view.Value.is_null() { Err(io::Error::last_os_error()) } else { Ok(View(view)) }
}

// IO_REPARSE_TAG_CLOUD and CLOUD_1 .. CLOUD_F are filter-managed data, not name surrogates.
// In particular, do NOT allow every tag without the name-surrogate bit.
fn cloud_tag(tag: u32) -> bool {
    tag & !0xf000 == 0x9000_001a
}

fn cloud(f: &File, attributes: u32) -> io::Result<bool> {
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
        return Ok(false);
    }
    let mut tag = FILE_ATTRIBUTE_TAG_INFO::default();
    if unsafe {
        GetFileInformationByHandleEx(f.as_raw_handle(), FileAttributeTagInfo, (&mut tag as *mut FILE_ATTRIBUTE_TAG_INFO).cast(), size_of_val(&tag) as u32)
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if !cloud_tag(tag.ReparseTag) {
        return Err(invalid("refusing a link or unsupported reparse point in the executable folder"));
    }
    Ok(true)
}

/// Keep every component in place. OPEN_REPARSE_POINT alone protects only the *last* component.
pub(crate) struct Directory(Vec<File>);

impl Directory {
    pub(crate) fn pin(path: &Path) -> io::Result<Self> {
        Self::walk(path, None, false)
    }

    /// The separate home is the corroborating trust boundary. Do not write evidence back inside the
    /// tree it is meant to vouch for, even through a different spelling of the installation path.
    pub(crate) fn private(path: &Path, install: FileId, create: bool) -> io::Result<Self> {
        Self::walk(path, Some(install), create)
    }

    fn walk(path: &Path, excluded: Option<FileId>, create: bool) -> io::Result<Self> {
        let ordinary_prefix = matches!(
            path.components().next(),
            Some(Component::Prefix(p)) if matches!(p.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_) | Prefix::UNC(_, _) | Prefix::VerbatimUNC(_, _))
        );
        if !path.is_absolute() || !ordinary_prefix || path.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err(invalid("expected an absolute executable folder"));
        }
        let mut held = Vec::new();
        let parents: Vec<_> = path.ancestors().collect();
        for parent in parents.into_iter().rev() {
            // Renaming a child opens its destination directory for FILE_ADD_FILE. Permit that
            // write access, but NOT DELETE: a pinned directory itself cannot be moved aside.
            let open = || {
                OpenOptions::new()
                    .access_mode(FILE_READ_ATTRIBUTES | FILE_LIST_DIRECTORY)
                    .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                    .custom_flags(NO_LINK)
                    .open(parent)
            };
            let f = match open() {
                Err(e) if create && e.kind() == io::ErrorKind::NotFound && !held.is_empty() => {
                    match std::fs::create_dir(parent) {
                        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                        r => r?,
                    }
                    open()?
                }
                other => other?,
            };
            let a = f.metadata()?.file_attributes();
            cloud(&f, a)?;
            if a & FILE_ATTRIBUTE_DIRECTORY == 0 {
                return Err(invalid("the executable folder or one of its parents is not a folder"));
            }
            if let Some(excluded) = excluded {
                if id(&f)? == excluded {
                    return Err(invalid("update receipts require a private home outside the executable folder"));
                }
            }
            held.push(f);
        }
        Ok(Self(held))
    }

    pub(crate) fn id(&self) -> io::Result<FileId> {
        id(self.0.last().expect("an absolute path has a root"))
    }
}

pub(crate) type FileId = [u8; 24];

pub(crate) fn id(f: &File) -> io::Result<FileId> {
    let mut info = FILE_ID_INFO::default();
    if unsafe { GetFileInformationByHandleEx(f.as_raw_handle(), FileIdInfo, (&mut info as *mut FILE_ID_INFO).cast(), size_of::<FILE_ID_INFO>() as u32) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if info.FileId.Identifier == [0; 16] {
        return Err(invalid("the drive did not supply a file identity"));
    }
    let mut id = [0; 24];
    id[..8].copy_from_slice(&info.VolumeSerialNumber.to_le_bytes());
    id[8..].copy_from_slice(&info.FileId.Identifier);
    Ok(id)
}

fn plain(f: File) -> io::Result<File> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(f.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    cloud(&f, info.dwFileAttributes)?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 || info.nNumberOfLinks != 1 {
        return Err(invalid("expected a plain file, not a folder, link, or hard link"));
    }
    Ok(f)
}

/// OPEN_REPARSE_POINT pins the directory entry, but can bypass hydration. For a known Cloud Files
/// placeholder, reopen the *pinned object*, not a potentially redirected path, for the content read.
pub(crate) fn content(f: &File) -> io::Result<Option<File>> {
    if !cloud(f, f.metadata()?.file_attributes())? {
        return Ok(None);
    }
    let h = unsafe { ReOpenFile(f.as_raw_handle(), GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, 0) };
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: ReOpenFile succeeded and returned a separately owned handle.
    let hydrated = plain(unsafe { File::from_raw_handle(h) })?;
    if id(f)? != id(&hydrated)? {
        return Err(invalid("cloud file identity changed on open"));
    }
    Ok(Some(hydrated))
}

/// Never truncate or unlink the lock. A sharing violation (including on SMB) means another writer wins.
pub(crate) fn lock(path: &Path) -> io::Result<File> {
    let f = plain(OpenOptions::new().read(true).write(true).create(true).truncate(false).custom_flags(NO_LINK).share_mode(0).open(path)?)?;
    if f.metadata()?.len() != 0 {
        return Err(invalid("the update lock name contains an unexpected file"));
    }
    Ok(f)
}

/// No other handle can write or rename the file while inspected. DELETE grants the *option* to rename
/// by this handle, not a request to delete an image that may still be mapped.
pub(crate) fn open(path: &Path, movable: bool) -> io::Result<File> {
    plain(OpenOptions::new().access_mode(GENERIC_READ | if movable { DELETE } else { 0 }).share_mode(FILE_SHARE_READ).custom_flags(NO_LINK).open(path)?)
}

pub(crate) fn create(path: &Path) -> io::Result<File> {
    plain(
        OpenOptions::new()
            .write(true)
            .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE)
            .create_new(true)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(NO_LINK)
            .open(path)?,
    )
}

/// Same-volume rename, not copy/delete, not replace. The source is the very handle that was verified.
pub(crate) fn rename(f: &File, to: &Path) -> io::Result<()> {
    rename_handle(f, to, false)
}

/// Publish a newly written settings temp file without closing or reopening its name.
pub(crate) fn rename_replacing(f: &File, to: &Path) -> io::Result<()> {
    rename_handle(f, to, true)
}

fn rename_handle(f: &File, to: &Path, replace: bool) -> io::Result<()> {
    let name = wide_os(to.as_os_str());
    let bytes = (name.len() - 1) * 2;
    // Keep an actual trailing NUL as well as the byte count. The Win32 wrapper converts a DOS
    // pathname before passing the counted name to the filesystem.
    let size = offset_of!(FILE_RENAME_INFO, FileName) + bytes + 2;
    let mut storage = vec![0usize; size.div_ceil(size_of::<usize>())];
    let p = storage.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    // SAFETY: word-aligned, zeroed storage (RootDirectory = NULL);
    // The byte count excludes the terminating NUL.
    unsafe {
        (*p).Anonymous.ReplaceIfExists = replace;
        (*p).FileNameLength = bytes as u32;
        std::ptr::copy_nonoverlapping(name.as_ptr(), std::ptr::addr_of_mut!((*p).FileName).cast::<u16>(), name.len());
        if SetFileInformationByHandle(f.as_raw_handle(), FileRenameInfo, p.cast(), size as u32) == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

pub(crate) fn delete(f: &File) -> io::Result<()> {
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    if unsafe { SetFileInformationByHandle(f.as_raw_handle(), FileDispositionInfo, (&info as *const FILE_DISPOSITION_INFO).cast(), size_of_val(&info) as u32) }
        == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Read only the neutral version resource written by build.rs; never start the file to ask its version.
/// The caller pins the path and its ancestors against replacement for both version.dll calls.
pub(crate) fn version(path: &Path, _pinned: &File, role: &str) -> Result<Version, String> {
    let bad = || format!("{} has no matching Tiny Skill Sync version resource", path.display());
    let name = wide_os(path.as_os_str());
    let size = unsafe { GetFileVersionInfoSizeExW(FILE_VER_GET_NEUTRAL, name.as_ptr(), null_mut()) };
    if size == 0 || size > 1024 * 1024 {
        return Err(bad());
    }
    let mut buf = vec![0u32; (size as usize).div_ceil(4)];
    if unsafe { GetFileVersionInfoExW(FILE_VER_GET_NEUTRAL, name.as_ptr(), 0, size, buf.as_mut_ptr().cast()) } == 0 {
        return Err(bad());
    }
    let query = |key: &str, unit: usize| {
        let (mut value, mut n) = (null_mut(), 0);
        if unsafe { VerQueryValueW(buf.as_ptr().cast(), wide(key).as_ptr(), &mut value, &mut n) } == 0 || n == 0 {
            return None;
        }
        let offset = (value as usize).checked_sub(buf.as_ptr() as usize)?;
        let len = (n as usize).checked_mul(unit)?;
        (offset.checked_add(len)? <= size as usize).then_some((value, n as usize))
    };
    let (ptr, len) = query("\\", 1).ok_or_else(bad)?;
    if len != size_of::<VS_FIXEDFILEINFO>() {
        return Err(bad());
    }
    // SAFETY: query checked the returned range against the buffer, including the complete structure.
    let fixed = unsafe { ptr.cast::<VS_FIXEDFILEINFO>().read_unaligned() };
    let (high, low) = (fixed.dwFileVersionMS, fixed.dwFileVersionLS);
    if fixed.dwSignature != 0xfeef_04bd || fixed.dwFileType != 1 || (high, low) != (fixed.dwProductVersionMS, fixed.dwProductVersionLS) || low & 0xffff != 0 {
        return Err(bad());
    }
    let number = format!("{}.{}.{}", high >> 16, high & 0xffff, low >> 16);
    for (field, expected) in [("ProductName", "Tiny Skill Sync"), ("OriginalFilename", role), ("FileVersion", &number)] {
        let (ptr, len) = query(&format!(r"\StringFileInfo\040904b0\{field}"), 2).ok_or_else(bad)?;
        if ptr.align_offset(2) != 0 {
            return Err(bad());
        }
        // SAFETY: checked alignment and length. VerQueryValueW counts the trailing NUL for strings.
        let words = unsafe { std::slice::from_raw_parts(ptr.cast::<u16>(), len) };
        if !words.iter().copied().eq(expected.encode_utf16().chain([0])) {
            return Err(bad());
        }
    }
    number.parse()
}
