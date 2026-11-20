//! Starting other programs: the GitHub CLI for its token, File Explorer, and the tray an app update installed, after
//! its bytes were proven to be the verified release. `gh` is only a token source here; it never makes the requests.
//!
//! Programs are started with `CreateProcessW` rather than `std::process`, which costs the tray exe about 60 KB of its
//! 1 MiB budget.

use std::ffi::{OsStr, OsString};
use std::io;
use std::mem::{size_of, size_of_val, zeroed};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::SystemInformation::{GetSystemDirectoryW, GetSystemWindowsDirectoryW};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess,
    InitializeProcThreadAttributeList, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess,
    UpdateProcThreadAttribute, WaitForSingleObject,
};

use super::{wide, wide_os};
use crate::secret::Secret;

pub const GH_TIMEOUT: Duration = Duration::from_secs(5);
/// Without `--hostname`, gh answers for whichever host it considers the default (`GH_HOST`, or the only host it is signed
/// in to), and that token would then be sent to github.com.
const TOKEN_ARGS: [&str; 4] = ["auth", "token", "--hostname", "github.com"];
const GH_ENV: [(&str, &str); 3] = [("GH_PROMPT_DISABLED", "1"), ("GH_NO_UPDATE_NOTIFIER", "1"), ("NO_COLOR", "1")];
const MAX_OUTPUT: usize = 64 * 1024;

struct Owned(HANDLE);

// SAFETY: a kernel handle may be used and closed from any thread.
unsafe impl Send for Owned {}

impl Drop for Owned {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn last() -> io::Error {
    io::Error::last_os_error()
}

/// Runs a program without a console window and returns its exit code and (at most 64 KiB of) standard output. `env`
/// is added to this process's environment. `None` means it did not finish in time and was killed.
pub fn run_capture(program: &str, args: &[&str], env: &[(&str, &str)], timeout: Duration) -> io::Result<Option<(u32, Vec<u8>)>> {
    let path = find(program)?;
    let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    let mut line = command_line(&path, &args)?;
    let block = env_block(std::env::vars_os(), env);
    let inherit = SECURITY_ATTRIBUTES { nLength: size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: null_mut(), bInheritHandle: 1 };
    let (mut read, mut write) = (null_mut(), null_mut());
    if unsafe { CreatePipe(&mut read, &mut write, &inherit, 0) } == 0 {
        return Err(last());
    }
    let (read, write) = (Owned(read), Owned(write));
    if unsafe { SetHandleInformation(read.0, HANDLE_FLAG_INHERIT, 0) } == 0 {
        return Err(last());
    }
    let nul = wide("NUL");
    let nul = unsafe { CreateFileW(nul.as_ptr(), GENERIC_READ | GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE, &inherit, OPEN_EXISTING, 0, null_mut()) };
    if nul == INVALID_HANDLE_VALUE {
        return Err(last());
    }
    let nul = Owned(nul);
    let process = create(&path, &mut line, Some(&block), Some((nul.0, write.0)))?;
    // Only the child holds the write end now, so the reader sees the end of the output when the child ends.
    drop((write, nul));
    let reader = std::thread::spawn(move || read_some(read));
    let ms = timeout.as_millis().min(u128::from(u32::MAX - 1)) as u32;
    if unsafe { WaitForSingleObject(process.0, ms) } != WAIT_OBJECT_0 {
        unsafe { TerminateProcess(process.0, 1) };
        // The reader is not joined: a grandchild that inherited the pipe could keep it open.
        return Ok(None);
    }
    let mut code = 0u32;
    if unsafe { GetExitCodeProcess(process.0, &mut code) } == 0 {
        return Err(last());
    }
    Ok(Some((code, reader.join().unwrap_or_default())))
}

/// Starts File Explorer on a folder or a web address, without waiting. Explorer is taken from the Windows folder, never
/// from `PATH`. Started as a process, so no COM is needed on this thread.
pub fn explorer(arg: &OsStr) -> io::Result<()> {
    let exe = system_dir(GetSystemWindowsDirectoryW).ok_or_else(last)?.join("explorer.exe");
    let mut line = command_line(&exe, &[arg])?;
    create(&exe, &mut line, None, None).map(drop)
}

/// A program that was started and left to run. Dropping this closes the handle, not the program.
pub struct Started(Owned);

impl Started {
    /// Waits up to `wait` for the program to end: its exit code if it did, `None` while it still runs.
    pub fn ended_within(&self, wait: Duration) -> Option<u32> {
        let ms = wait.as_millis().min(u128::from(u32::MAX - 1)) as u32;
        let mut code = 0u32;
        (unsafe { WaitForSingleObject(self.0.0, ms) } == WAIT_OBJECT_0 && unsafe { GetExitCodeProcess(self.0.0, &mut code) } != 0).then_some(code)
    }

    /// Ends the program and waits a moment for it to be gone.
    pub fn stop(&self) {
        unsafe { TerminateProcess(self.0.0, 1) };
        self.ended_within(Duration::from_secs(5));
    }
}

/// Starts `program`, which the caller holds open after proving it is the program it means, and does not wait. The
/// program gets this process's environment and current folder, and no handles.
pub fn start(program: &Path, args: &[&OsStr]) -> io::Result<Started> {
    let mut line = command_line(program, args)?;
    create(program, &mut line, None, None).map(Started)
}

fn read_some(h: Owned) -> Vec<u8> {
    let mut buf = vec![0u8; MAX_OUTPUT];
    let mut len = 0;
    while len < MAX_OUTPUT {
        let mut got = 0u32;
        let ok = unsafe { ReadFile(h.0, buf[len..].as_mut_ptr(), (MAX_OUTPUT - len) as u32, &mut got, null_mut()) };
        if ok == 0 || got == 0 {
            break;
        }
        len += got as usize;
    }
    buf.truncate(len);
    buf
}

/// Starts `program` with no console window. With `stdio` (standard input and error, then standard output) the child
/// gets those handles and no other; without, it inherits nothing.
fn create(program: &Path, line: &mut [u16], env: Option<&[u16]>, stdio: Option<(HANDLE, HANDLE)>) -> io::Result<Owned> {
    let app = wide_os(program.as_os_str());
    // SAFETY: plain integers and pointers, for which all-zero is a valid value.
    let mut si: STARTUPINFOEXW = unsafe { zeroed() };
    si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    let mut flags = CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT;
    let mut list_mem: Vec<usize> = Vec::new();
    let handles: [HANDLE; 2];
    if let Some((nul, out)) = stdio {
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        (si.StartupInfo.hStdInput, si.StartupInfo.hStdOutput, si.StartupInfo.hStdError) = (nul, out, nul);
        handles = [nul, out];
        let mut size = 0usize;
        unsafe { InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut size) };
        list_mem = vec![0; size.div_ceil(size_of::<usize>())];
        let list = list_mem.as_mut_ptr().cast();
        if unsafe { InitializeProcThreadAttributeList(list, 1, 0, &mut size) } == 0 {
            return Err(last());
        }
        si.lpAttributeList = list;
        let value = handles.as_ptr().cast();
        if unsafe { UpdateProcThreadAttribute(list, 0, PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize, value, size_of_val(&handles), null_mut(), null()) } == 0 {
            let e = last();
            unsafe { DeleteProcThreadAttributeList(list) };
            return Err(e);
        }
        flags |= EXTENDED_STARTUPINFO_PRESENT;
    }
    // SAFETY: as above.
    let mut pi: PROCESS_INFORMATION = unsafe { zeroed() };
    let environment = env.map_or(null(), |e| e.as_ptr().cast());
    let inherit = i32::from(stdio.is_some());
    let ok = unsafe { CreateProcessW(app.as_ptr(), line.as_mut_ptr(), null(), null(), inherit, flags, environment, null(), (&raw const si).cast(), &mut pi) };
    let failed = (ok == 0).then(last);
    if !si.lpAttributeList.is_null() {
        unsafe { DeleteProcThreadAttributeList(si.lpAttributeList) };
    }
    drop(list_mem);
    if let Some(e) = failed {
        return Err(e);
    }
    unsafe { CloseHandle(pi.hThread) };
    Ok(Owned(pi.hProcess))
}

fn system_dir(get: unsafe extern "system" fn(*mut u16, u32) -> u32) -> Option<PathBuf> {
    let mut buf = [0u16; 260];
    let n = unsafe { get(buf.as_mut_ptr(), buf.len() as u32) } as usize;
    (n > 0 && n < buf.len()).then(|| PathBuf::from(OsString::from_wide(&buf[..n])))
}

/// Finds a program given by name: see `candidates`.
fn find(name: &str) -> io::Result<PathBuf> {
    let app_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
    let system: Vec<PathBuf> = [system_dir(GetSystemDirectoryW), system_dir(GetSystemWindowsDirectoryW)].into_iter().flatten().collect();
    let path = std::env::var_os("PATH");
    let found = candidates(name, app_dir.as_deref(), &system, path.as_deref()).into_iter().find(|p| p.is_file());
    found.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{name} was not found")))
}

/// Where a program is looked for, in order: the app's own folder, the Windows folders, then each folder on `PATH`
/// that is a full path. Never the current folder. `.exe` is added when the name has no extension.
fn candidates(name: &str, app_dir: Option<&Path>, system: &[PathBuf], path: Option<&OsStr>) -> Vec<PathBuf> {
    let file = if Path::new(name).extension().is_some() { name.to_string() } else { format!("{name}.exe") };
    let on_path = path.map(|p| std::env::split_paths(p).filter(|d| d.is_absolute()).collect::<Vec<_>>()).unwrap_or_default();
    app_dir.into_iter().chain(system.iter().map(PathBuf::as_path)).chain(on_path.iter().map(PathBuf::as_path)).map(|d| d.join(&file)).collect()
}

/// The program in quotes, then each argument, in quotes only when it is empty or has a space or a tab: `cmd /C` needs
/// its switch bare. Backslashes before a closing quote are doubled, so they stay backslashes. An argument with a `"` is
/// refused: nothing here needs one.
fn command_line(program: &Path, args: &[&OsStr]) -> io::Result<Vec<u16>> {
    const QUOTE: u16 = b'"' as u16;
    let mut line = vec![QUOTE];
    line.extend(program.as_os_str().encode_wide());
    line.push(QUOTE);
    for arg in args {
        let units: Vec<u16> = arg.encode_wide().collect();
        if units.contains(&QUOTE) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "an argument has a quote in it"));
        }
        line.push(b' ' as u16);
        let quoted = units.is_empty() || units.iter().any(|&u| u == b' ' as u16 || u == b'\t' as u16);
        if quoted {
            line.push(QUOTE);
        }
        line.extend(&units);
        if quoted {
            let backslashes = units.iter().rev().take_while(|&&u| u == b'\\' as u16).count();
            line.extend(std::iter::repeat_n(b'\\' as u16, backslashes));
            line.push(QUOTE);
        }
    }
    line.push(0);
    Ok(line)
}

/// `vars` with `set` in place of any variable of the same name (case does not matter), sorted by name as Windows
/// expects: each `NAME=value` ends in a NUL, and the block in another.
fn env_block(vars: impl Iterator<Item = (OsString, OsString)>, set: &[(&str, &str)]) -> Vec<u16> {
    let key = |k: &OsStr| k.to_string_lossy().to_uppercase();
    let replaced = |k: &OsStr| set.iter().any(|(s, _)| s.to_uppercase() == key(k));
    let mut all: Vec<(String, Vec<u16>)> = vars
        .filter(|(k, _)| !replaced(k))
        .map(|(k, v)| (key(&k), k.encode_wide().chain([b'=' as u16]).chain(v.encode_wide()).collect()))
        .chain(set.iter().map(|(k, v)| (k.to_uppercase(), format!("{k}={v}").encode_utf16().collect())))
        .collect();
    all.sort_by(|a, b| a.0.cmp(&b.0));
    let mut block: Vec<u16> = all.into_iter().flat_map(|(_, entry)| entry.into_iter().chain([0])).collect();
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    block
}

/// The token from `gh auth token` output: one line of printable ASCII.
pub fn parse_token_output(out: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(out).ok()?.trim();
    let ok = !text.is_empty() && text.len() <= 512 && text.bytes().all(|b| b.is_ascii_graphic());
    ok.then(|| text.to_string())
}

/// The error text is plain words for the status line; it never contains the token.
pub fn gh_token() -> Result<Secret, String> {
    match run_capture("gh", &TOKEN_ARGS, &GH_ENV, GH_TIMEOUT) {
        Ok(Some((0, out))) => parse_token_output(&out).map(Secret::new).ok_or_else(|| "the GitHub CLI (gh) returned no token".to_string()),
        Ok(Some(_)) => Err("the GitHub CLI (gh) is installed but not signed in; run: gh auth login".to_string()),
        Ok(None) => Err("the GitHub CLI (gh) did not answer within 5 seconds".to_string()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err("the GitHub CLI (gh) was not found".to_string()),
        Err(e) => Err(format!("the GitHub CLI (gh) could not be started: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    fn line_of(program: &str, args: &[&str]) -> io::Result<String> {
        let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
        let line = command_line(Path::new(program), &args)?;
        Ok(String::from_utf16_lossy(&line[..line.len() - 1]))
    }

    #[test]
    fn output_is_accepted_only_when_it_looks_like_a_token() {
        assert_eq!(parse_token_output(b"gho_abc123\r\n").as_deref(), Some("gho_abc123"));
        assert_eq!(parse_token_output(b""), None);
        assert_eq!(parse_token_output(b"two\nlines"), None);
        assert_eq!(parse_token_output(b"has space"), None);
        assert_eq!(parse_token_output(&[0xff, 0xfe]), None);
        assert_eq!(parse_token_output("é".as_bytes()), None);
        assert_eq!(parse_token_output("a".repeat(513).as_bytes()), None);
    }

    #[test]
    fn the_token_asked_for_is_always_the_one_for_github_com() {
        assert_eq!(line_of(r"C:\gh\gh.exe", &TOKEN_ARGS).unwrap(), r#""C:\gh\gh.exe" auth token --hostname github.com"#);
        let set = String::from_utf16_lossy(&env_block(std::iter::empty(), &GH_ENV));
        assert_eq!(set, "GH_NO_UPDATE_NOTIFIER=1\0GH_PROMPT_DISABLED=1\0NO_COLOR=1\0\0");
    }

    #[test]
    fn arguments_are_quoted_only_when_they_need_it_and_a_quote_is_refused() {
        let got = line_of(r"C:\Program Files\x.exe", &["/C", "a b", "", r"C:\my dir\", r"C:\plain\"]).unwrap();
        assert_eq!(got, r#""C:\Program Files\x.exe" /C "a b" "" "C:\my dir\\" C:\plain\"#);
        assert_eq!(line_of("x.exe", &["say \"hi\""]).unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn a_program_is_looked_for_beside_the_app_then_in_windows_then_on_path_but_never_in_the_current_folder() {
        let system = [PathBuf::from(r"C:\Windows\System32"), PathBuf::from(r"C:\Windows")];
        let path = OsStr::new(r"C:\tools;.;relative\bin;;D:\more");
        let got = candidates("gh", Some(Path::new(r"C:\app")), &system, Some(path));
        let want = [r"C:\app\gh.exe", r"C:\Windows\System32\gh.exe", r"C:\Windows\gh.exe", r"C:\tools\gh.exe", r"D:\more\gh.exe"];
        assert_eq!(got, want.map(PathBuf::from));
        assert_eq!(candidates("run.cmd", None, &[], Some(OsStr::new(r"C:\t"))), [PathBuf::from(r"C:\t\run.cmd")], "an extension is kept");
    }

    #[test]
    fn the_childs_environment_is_ours_with_the_settings_in_place_sorted_by_name() {
        let ours = [("Path", r"C:\x"), ("no_color", "0"), ("=C:", r"C:\here"), ("alpha", "1")].map(|(k, v)| (OsString::from(k), OsString::from(v)));
        let block = env_block(ours.into_iter(), &[("NO_COLOR", "1"), ("GH_X", "1")]);
        assert_eq!(String::from_utf16_lossy(&block), "=C:=C:\\here\0alpha=1\0GH_X=1\0NO_COLOR=1\0Path=C:\\x\0\0");
        assert_eq!(env_block(std::iter::empty(), &[]), [0, 0], "an empty block is two NULs");
    }

    #[test]
    fn a_command_that_finishes_returns_its_exit_code_output_and_environment() {
        let (code, out) = run_capture("cmd", &["/C", "echo %TSS_PROC_TEST%"], &[("TSS_PROC_TEST", "hello")], Duration::from_secs(10)).unwrap().unwrap();
        assert_eq!(code, 0);
        assert_eq!(String::from_utf8_lossy(&out).trim(), "hello");
        let (code, _) = run_capture("cmd", &["/C", "exit 3"], &[], Duration::from_secs(10)).unwrap().unwrap();
        assert_eq!(code, 3);
    }

    #[test]
    fn a_command_that_hangs_is_killed_at_the_deadline() {
        let start = Instant::now();
        assert!(run_capture("cmd", &["/C", "ping -n 30 127.0.0.1 >nul"], &[], Duration::from_millis(300)).unwrap().is_none());
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_missing_program_is_not_found() {
        let err = run_capture("tss-no-such-program-xyz", &[], &[], Duration::from_secs(1)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn only_a_tray_proven_and_held_is_started_and_it_is_left_to_run() {
        let base = std::env::var_os("TSS_TEST_ROOT").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let dir = base.join(format!("tss-it-restart-{}", crate::fsx::unique_suffix()));
        std::fs::create_dir(&dir).unwrap();
        let tray = dir.join("tiny-skill-sync.exe");
        let bytes = std::fs::read(system_dir(GetSystemDirectoryW).unwrap().join("cmd.exe")).unwrap();
        std::fs::write(&tray, &bytes).unwrap();
        let mut wrong = crate::release::sha256(&bytes);
        wrong[31] ^= 1;
        assert!(crate::replacement::verified_tray(&dir, wrong).is_err());

        let proven = crate::replacement::verified_tray(&dir, crate::release::sha256(&bytes)).unwrap();
        assert!(std::fs::rename(&tray, dir.join("other.exe")).is_err(), "the proven file cannot be swapped before it starts");
        assert!(std::fs::write(&tray, b"not the tray").is_err());
        let ended = start(proven.path(), &[OsStr::new("/C"), OsStr::new("exit 7")]).unwrap();
        assert_eq!(ended.ended_within(Duration::from_secs(10)), Some(7));
        let running = start(proven.path(), &[OsStr::new("/K")]).unwrap();
        assert_eq!(running.ended_within(Duration::from_millis(200)), None);
        running.stop();
        assert_eq!(running.ended_within(Duration::ZERO), Some(1));
        drop(proven);
        std::fs::rename(&tray, dir.join("other.exe")).expect("only the hold refused the swap");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
