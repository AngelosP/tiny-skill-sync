//! The only place that talks to Win32 directly. The rest of the crate stays plain Rust and easy to test.

pub mod autorun;
mod clip;
pub mod console;
pub(crate) mod copilot;
pub mod cred;
pub mod fs;
pub mod http;
pub(crate) mod install;
pub mod proc;
pub(crate) mod release_hash;
mod ui;

/// Runs the tray app until it quits. The process exit code.
pub use ui::shell::run as run_tray;

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

/// UTF-16 with a terminating NUL, the form Win32 expects.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn wide_os(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}
