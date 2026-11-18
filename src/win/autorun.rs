//! Start with Windows, in the registry: the entry under the user's `Run` key, and Windows' mark under
//! `StartupApproved\Run`. [`crate::autostart`] decides what they mean and what is written.
//!
//! Nothing here runs unless the user turns the switch, accepts the offer or runs `tss startup`, and nothing is ever put
//! back on its own: an entry the user deleted, or switched off in Task Manager, stays that way.

use std::io;
use std::path::Path;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_SUCCESS, ERROR_UNSUPPORTED_TYPE, WIN32_ERROR};
use windows_sys::Win32::Storage::FileSystem::{GetLongPathNameW, GetShortPathNameW};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_ROUTINE_FLAGS, REG_SZ, RRF_RT_REG_BINARY, RRF_RT_REG_SZ, RegCloseKey, RegCreateKeyExW,
    RegDeleteKeyValueW, RegGetValueW, RegSetValueExW,
};

use super::wide;
use crate::autostart::{Exe, Keys, Startup, VALUE_NAME};
use crate::config::Paths;

/// Whether Windows starts this copy when the user signs in. Only reads.
pub fn state(keys: &Keys, exe: &Exe) -> io::Result<Startup> {
    // RRF_RT_REG_SZ takes a REG_EXPAND_SZ entry too, with its variables expanded.
    let entry = read(&keys.run, RRF_RT_REG_SZ)?.map(|bytes| text_of(&bytes));
    let mark = read(&keys.approved, RRF_RT_REG_BINARY)?;
    Ok(Startup::decide(entry.as_deref(), mark.as_deref(), exe))
}

/// Has Windows start this copy when the user signs in, also when it was switched off in Task Manager or Settings, and
/// in place of another copy.
pub fn turn_on(keys: &Keys, exe: &Exe) -> io::Result<Startup> {
    guard(keys)?;
    let command = exe.command().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "this copy's path is too long for Windows to start it"))?;
    write(&keys.run, &command)?;
    delete(&keys.approved)?;
    state(keys, exe)
}

/// Stops Windows starting this copy. Another copy's entry is left as it is.
pub fn turn_off(keys: &Keys, exe: &Exe) -> io::Result<Startup> {
    guard(keys)?;
    if matches!(state(keys, exe)?, Startup::On | Startup::OffInWindows) {
        delete(&keys.run)?;
        delete(&keys.approved)?;
    }
    state(keys, exe)
}

/// This program, in both forms of its path.
pub fn this_exe() -> io::Result<Exe> {
    Ok(exe_at(&std::env::current_exe()?))
}

/// A program at `path` in both forms of its path, whichever form `path` is in: started by its short name, it still
/// knows its long one. It must exist.
pub fn exe_at(path: &Path) -> Exe {
    let given = path.to_string_lossy();
    let long = path_call(GetLongPathNameW, &verbatim(&given)).unwrap_or_else(|| given.to_string());
    let short = path_call(GetShortPathNameW, &verbatim(&long));
    Exe::new(&long, short.as_deref())
}

/// Writes go to Windows' own keys only from a real copy of the app: never from the tests, nor from a copy with an app
/// folder of its own (`TSS_HOME`, as the scripts start it), whatever keys it was handed.
fn guard(keys: &Keys) -> io::Result<()> {
    let windows = !cfg!(test) && Paths::own_home().is_none();
    if keys.allowed(windows) { Ok(()) } else { Err(io::Error::new(io::ErrorKind::PermissionDenied, "these are not the keys of the startup entry")) }
}

/// The entry's value in `key`, or `None` when there is none, or it is of another type.
fn read(key: &str, flags: REG_ROUTINE_FLAGS) -> io::Result<Option<Vec<u8>>> {
    let (key, name) = (wide(key), wide(VALUE_NAME));
    let mut buf: Vec<u8> = Vec::new();
    // First its size; then it may have grown before it is read, or grow when its variables are expanded.
    for _ in 0..4 {
        let mut size = buf.len() as u32;
        let data = if buf.is_empty() { null_mut() } else { buf.as_mut_ptr().cast() };
        match unsafe { RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), flags, null_mut(), data, &mut size) } {
            ERROR_SUCCESS if !buf.is_empty() => {
                buf.truncate(size as usize);
                return Ok(Some(buf));
            }
            ERROR_SUCCESS | ERROR_MORE_DATA => buf = vec![0; size.max(2) as usize],
            ERROR_FILE_NOT_FOUND | ERROR_UNSUPPORTED_TYPE => return Ok(None),
            e => return Err(os_error(e)),
        }
    }
    Err(io::Error::other("the startup entry kept changing while it was read"))
}

fn write(key: &str, text: &str) -> io::Result<()> {
    let mut hkey: HKEY = null_mut();
    let status =
        unsafe { RegCreateKeyExW(HKEY_CURRENT_USER, wide(key).as_ptr(), 0, null(), REG_OPTION_NON_VOLATILE, KEY_SET_VALUE, null(), &mut hkey, null_mut()) };
    if status != ERROR_SUCCESS {
        return Err(os_error(status));
    }
    let data = wide(text);
    let status = unsafe { RegSetValueExW(hkey, wide(VALUE_NAME).as_ptr(), 0, REG_SZ, data.as_ptr().cast(), (data.len() * 2) as u32) };
    unsafe { RegCloseKey(hkey) };
    if status == ERROR_SUCCESS { Ok(()) } else { Err(os_error(status)) }
}

/// Deletes the entry's value in `key`. One that is not there is deleted already.
fn delete(key: &str) -> io::Result<()> {
    match unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, wide(key).as_ptr(), wide(VALUE_NAME).as_ptr()) } {
        ERROR_SUCCESS | ERROR_FILE_NOT_FOUND => Ok(()),
        e => Err(os_error(e)),
    }
}

fn os_error(e: WIN32_ERROR) -> io::Error {
    io::Error::from_raw_os_error(e as i32)
}

/// UTF-16 text up to its NUL.
fn text_of(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
    String::from_utf16_lossy(&units)
}

/// The `\\?\` form, which the path functions take at any length, also in a program that is not long path aware.
fn verbatim(path: &str) -> String {
    let path = path.replace('/', "\\");
    if path.starts_with(r"\\?\") {
        path
    } else if let Some(share) = path.strip_prefix(r"\\") {
        format!(r"\\?\UNC\{share}")
    } else {
        format!(r"\\?\{path}")
    }
}

/// Calls `GetLongPathNameW` or `GetShortPathNameW` with a buffer that is big enough.
fn path_call(f: unsafe extern "system" fn(*const u16, *mut u16, u32) -> u32, path: &str) -> Option<String> {
    let path = wide(path);
    let mut buf = vec![0u16; 300];
    for _ in 0..3 {
        let n = unsafe { f(path.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
        if n == 0 {
            return None;
        }
        if n < buf.len() {
            return String::from_utf16(&buf[..n]).ok();
        }
        // Too small: `n` is the size it needs.
        buf = vec![0; n];
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use windows_sys::Win32::System::Registry::{REG_BINARY, REG_EXPAND_SZ, REG_VALUE_TYPE, RegDeleteTreeW};

    use crate::autostart::{MAX_COMMAND, TEST_ROOT};

    const LONG: &str = r"C:\Users\someone\AppData\Local\Programs\tiny-skill-sync\tiny-skill-sync.exe";
    const OFF_IN_WINDOWS: [u8; 12] = [3, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8];

    /// Keys of one test's own, deleted when it ends.
    struct Own {
        keys: Keys,
        root: String,
    }

    impl Own {
        /// Under the test root, as a copy with an app folder of its own has them.
        fn keys(name: &str) -> Own {
            let home = std::env::temp_dir().join(format!("tss-winfs-autorun-{name}-{}", crate::fsx::unique_suffix()));
            let keys = Keys::choose(Some(&home));
            let root = keys.run.strip_suffix(r"\Run").expect("a key of its own").to_string();
            assert!(root.starts_with(TEST_ROOT));
            Own { keys, root }
        }

        /// Beside the test root, where nothing may be written.
        fn beside(name: &str) -> Own {
            let root = format!(r"{TEST_ROOT}-{name}-{}", crate::fsx::unique_suffix());
            Own { keys: Keys { run: format!(r"{root}\Run"), approved: format!(r"{root}\StartupApproved\Run") }, root }
        }
    }

    impl Drop for Own {
        fn drop(&mut self) {
            unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide(&self.root).as_ptr()) };
        }
    }

    /// Writes a value the way another program would.
    fn put(key: &str, kind: REG_VALUE_TYPE, data: &[u8]) {
        let mut hkey: HKEY = null_mut();
        let status =
            unsafe { RegCreateKeyExW(HKEY_CURRENT_USER, wide(key).as_ptr(), 0, null(), REG_OPTION_NON_VOLATILE, KEY_SET_VALUE, null(), &mut hkey, null_mut()) };
        assert_eq!(status, ERROR_SUCCESS);
        let status = unsafe { RegSetValueExW(hkey, wide(VALUE_NAME).as_ptr(), 0, kind, data.as_ptr(), data.len() as u32) };
        unsafe { RegCloseKey(hkey) };
        assert_eq!(status, ERROR_SUCCESS);
    }

    fn utf16(text: &str) -> Vec<u8> {
        wide(text).iter().flat_map(|u| u.to_le_bytes()).collect()
    }

    fn entry(keys: &Keys) -> Option<String> {
        read(&keys.run, RRF_RT_REG_SZ).unwrap().map(|b| text_of(&b))
    }

    fn mark(keys: &Keys) -> Option<Vec<u8>> {
        read(&keys.approved, RRF_RT_REG_BINARY).unwrap()
    }

    #[test]
    fn turned_on_and_off_the_entry_is_written_and_deleted_and_windows_mark_with_it() {
        let own = Own::keys("onoff");
        let keys = &own.keys;
        let exe = Exe::new(LONG, None);
        assert_eq!(state(keys, &exe).unwrap(), Startup::Off, "no keys yet");
        assert_eq!(turn_on(keys, &exe).unwrap(), Startup::On);
        assert_eq!(entry(keys), Some(format!("\"{LONG}\" --background")));

        put(&keys.approved, REG_BINARY, &OFF_IN_WINDOWS);
        assert_eq!(state(keys, &exe).unwrap(), Startup::OffInWindows, "switched off in Task Manager");
        assert_eq!(turn_on(keys, &exe).unwrap(), Startup::On, "turned on here, it is on again");
        assert_eq!(mark(keys), None);

        assert_eq!(turn_off(keys, &exe).unwrap(), Startup::Off);
        assert_eq!(entry(keys), None);
        assert_eq!(turn_off(keys, &exe).unwrap(), Startup::Off, "off twice");

        turn_on(keys, &exe).unwrap();
        put(&keys.approved, REG_BINARY, &OFF_IN_WINDOWS);
        assert_eq!(turn_off(keys, &exe).unwrap(), Startup::Off);
        assert_eq!((entry(keys), mark(keys)), (None, None), "nothing is left behind");
    }

    #[test]
    fn another_copys_entry_is_shown_and_replaced_only_when_this_one_is_turned_on() {
        let own = Own::keys("other");
        let keys = &own.keys;
        let exe = Exe::new(LONG, None);
        let other = r"D:\old\tiny-skill-sync.exe";
        put(&keys.run, REG_SZ, &utf16(&format!("\"{other}\" --background")));
        assert_eq!(state(keys, &exe).unwrap(), Startup::OtherCopy(other.to_string()));
        assert_eq!(turn_off(keys, &exe).unwrap(), Startup::OtherCopy(other.to_string()), "turning this one off leaves it");
        assert!(entry(keys).is_some_and(|e| e.contains(other)));
        assert_eq!(turn_on(keys, &exe).unwrap(), Startup::On);
        assert_eq!(entry(keys), Some(format!("\"{LONG}\" --background")));
    }

    #[test]
    fn an_entry_with_variables_is_read_expanded_and_one_of_another_type_is_none() {
        let own = Own::keys("expand");
        let keys = &own.keys;
        let profile = std::env::var("USERPROFILE").unwrap();
        let exe = Exe::new(&format!(r"{profile}\tss\tiny-skill-sync.exe"), None);
        put(&keys.run, REG_EXPAND_SZ, &utf16(r"%USERPROFILE%\tss\tiny-skill-sync.exe --background"));
        assert_eq!(state(keys, &exe).unwrap(), Startup::On);

        put(&keys.run, REG_BINARY, &[1, 2, 3]);
        assert_eq!(state(keys, &exe).unwrap(), Startup::Off);
        put(&keys.run, REG_SZ, &utf16(&format!("\"{}\"", "x".repeat(5000))));
        assert_eq!(entry(keys).map(|e| e.len()), Some(5002), "longer than the first buffer");
    }

    #[test]
    fn a_path_too_long_for_the_entry_writes_nothing() {
        let own = Own::keys("long");
        let keys = &own.keys;
        let exe = Exe::new(&format!(r"C:\{}\tiny-skill-sync.exe", "d".repeat(MAX_COMMAND)), None);
        assert_eq!(state(keys, &exe).unwrap(), Startup::TooLong);
        assert_eq!(turn_on(keys, &exe).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert_eq!(entry(keys), None);
    }

    #[test]
    fn no_key_but_the_entrys_is_ever_written() {
        assert_eq!(guard(&Keys::windows()).unwrap_err().kind(), io::ErrorKind::PermissionDenied, "not Windows' own keys, from a test");
        let beside = Own::beside("guard");
        let exe = Exe::new(LONG, None);
        assert_eq!(turn_on(&beside.keys, &exe).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(turn_off(&beside.keys, &exe).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(entry(&beside.keys), None);
    }

    /// A new folder, by its long path also where `%TEMP%` is given in its short form.
    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tss-winfs-{name}-{}", crate::fsx::unique_suffix()));
        fs::create_dir_all(&d).unwrap();
        let long = path_call(GetLongPathNameW, &verbatim(&d.to_string_lossy())).unwrap();
        PathBuf::from(crate::autostart::plain(&long).into_owned())
    }

    #[test]
    fn a_program_knows_both_forms_of_its_path_whichever_it_was_started_by() {
        let dir = temp("exe");
        let folder = dir.join("A Folder With A Long Name");
        fs::create_dir_all(&folder).unwrap();
        let file = folder.join("tiny-skill-sync.exe");
        fs::write(&file, b"").unwrap();
        let exe = exe_at(&file);
        assert_eq!(exe.long, file.to_string_lossy());
        if let Some(short) = &exe.short {
            assert!(short.contains('~'), "{short}");
            assert_eq!(exe_at(Path::new(short)), exe, "started by its short name");
        }

        // Deeper than MAX_PATH. Tests are not long path aware, so the folders are made with the \\?\ form.
        let deep = folder.join("d".repeat(120)).join("e".repeat(120));
        fs::create_dir_all(verbatim(&deep.to_string_lossy())).unwrap();
        let file = deep.join("tiny-skill-sync.exe");
        fs::write(verbatim(&file.to_string_lossy()), b"").unwrap();
        let exe = exe_at(&file);
        assert_eq!(exe.long, file.to_string_lossy());
        assert_eq!(exe.command().is_some(), exe.short.is_some(), "only its short name fits: {exe:?}");
        fs::remove_dir_all(verbatim(&dir.to_string_lossy())).unwrap();
    }
}
