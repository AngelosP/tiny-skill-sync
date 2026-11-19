//! Copy text to the clipboard: Copy path and Copy GitHub link in a skill's card.

use std::ffi::OsStr;
use std::io;
use std::ptr;
use std::thread;
use std::time::Duration;

use windows_sys::Win32::Foundation::{GlobalFree, HWND};
use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};

const CF_UNICODETEXT: u32 = 13;

/// Puts `text` on the clipboard, owned by `owner` (a window of this app: without one Windows refuses the text).
pub fn set_text(owner: HWND, text: &OsStr) -> io::Result<()> {
    let wide = super::wide_os(text);
    // Another app can hold the clipboard for a moment.
    let mut tries = 0;
    while unsafe { OpenClipboard(owner) } == 0 {
        tries += 1;
        if tries == 10 {
            return Err(io::Error::last_os_error());
        }
        thread::sleep(Duration::from_millis(20));
    }
    let done = unsafe { put(&wide) };
    unsafe { CloseClipboard() };
    done
}

/// Safety: the clipboard is open.
unsafe fn put(wide: &[u16]) -> io::Result<()> {
    if unsafe { EmptyClipboard() } == 0 {
        return Err(io::Error::last_os_error());
    }
    let bytes = std::mem::size_of_val(wide);
    let mem = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) };
    if mem.is_null() {
        return Err(io::Error::last_os_error());
    }
    let to = unsafe { GlobalLock(mem) }.cast::<u16>();
    if to.is_null() {
        let e = io::Error::last_os_error();
        unsafe { GlobalFree(mem) };
        return Err(e);
    }
    unsafe {
        ptr::copy_nonoverlapping(wide.as_ptr(), to, wide.len());
        GlobalUnlock(mem);
    }
    // The clipboard owns the memory once it takes it.
    if unsafe { SetClipboardData(CF_UNICODETEXT, mem) }.is_null() {
        let e = io::Error::last_os_error();
        unsafe { GlobalFree(mem) };
        return Err(e);
    }
    Ok(())
}
