//! The tray app's windows: the tray icon, the main window and small questions. Only the UI thread runs this code.
//!
//! A window procedure can be entered again while it runs: a control notifies its parent from inside the
//! `SendMessageW` that changed it. So the state the procedures share sits in thread-local `RefCell`s that are only
//! borrowed with `try_borrow_mut`; a notification that finds the state borrowed was caused by our own update and is
//! ignored. No borrow is held across a call that runs a message loop (a question, the tray menu, `DestroyWindow`).

mod bench;
pub mod control;
pub mod dialogs;
pub mod page;
pub mod paint;
pub mod shell;
pub mod theme;
pub mod tray;
pub mod window;

use std::mem::{size_of, zeroed};
use std::path::Path;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{ERROR_CLASS_ALREADY_EXISTS, FILETIME, GetLastError, HWND, RECT, SYSTEMTIME};
use windows_sys::Win32::Graphics::Gdi::{COLOR_BTNFACE, GetMonitorInfoW, HBRUSH, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};
use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
use windows_sys::Win32::UI::HiDpi::AdjustWindowRectExForDpi;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, GetWindowRect, GetWindowTextLengthW, GetWindowTextW, HICON, IDC_ARROW, IsWindowVisible, LoadCursorW, RegisterClassExW,
    SPI_GETWORKAREA, SystemParametersInfoW, WNDCLASSEXW, WNDPROC,
};

use super::wide;
use crate::interval::{UnixSecs, format_utc};

/// Device-independent pixels (1/96 inch) to pixels at a DPI.
pub fn scale(v: i32, dpi: u32) -> i32 {
    (v * dpi as i32 + 48) / 96
}

/// Registers a window class with the dialog background colour. Registering it again is not an error.
pub fn register_class(name: &str, proc: WNDPROC, icon: HICON) -> bool {
    let class = wide(name);
    let wc = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: proc,
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: unsafe { GetModuleHandleW(null()) },
        hIcon: icon,
        hCursor: unsafe { LoadCursorW(null_mut(), IDC_ARROW) },
        hbrBackground: (COLOR_BTNFACE + 1) as usize as HBRUSH,
        lpszMenuName: null(),
        lpszClassName: class.as_ptr(),
        hIconSm: icon,
    };
    unsafe { RegisterClassExW(&wc) != 0 || GetLastError() == ERROR_CLASS_ALREADY_EXISTS }
}

fn work_area(over: HWND) -> RECT {
    unsafe {
        if !over.is_null() {
            let mut monitor: MONITORINFO = zeroed();
            monitor.cbSize = size_of::<MONITORINFO>() as u32;
            if GetMonitorInfoW(MonitorFromWindow(over, MONITOR_DEFAULTTONEAREST), &mut monitor) != 0 {
                return monitor.rcWork;
            }
        }
        let mut work: RECT = zeroed();
        SystemParametersInfoW(SPI_GETWORKAREA, 0, (&raw mut work).cast(), 0);
        work
    }
}

fn keep_on_screen(work: RECT, [x, y, w, h]: [i32; 4]) -> [i32; 4] {
    [x.clamp(work.left, (work.right - w).max(work.left)), y.clamp(work.top, (work.bottom - h).max(work.top)), w, h]
}

fn centered(work: RECT, over: HWND, [w, h]: [i32; 2]) -> [i32; 4] {
    let mut area = work;
    if over.is_null() || unsafe { IsWindowVisible(over) } == 0 || unsafe { GetWindowRect(over, &mut area) } == 0 {
        area = work;
    }
    let x = area.left + (area.right - area.left - w) / 2;
    let y = area.top + (area.bottom - area.top - h) / 2;
    keep_on_screen(work, [x, y, w, h])
}

/// Position and outer size of a window with a client area of `client` DIPs, centred over `over` (or the work area)
/// and kept on the work area.
pub fn place(style: u32, ex_style: u32, client: (i32, i32), dpi: u32, over: HWND) -> [i32; 4] {
    let mut rc = RECT { left: 0, top: 0, right: scale(client.0, dpi), bottom: scale(client.1, dpi) };
    unsafe { AdjustWindowRectExForDpi(&mut rc, style, 0, ex_style, dpi) };
    centered(work_area(over), over, [rc.right - rc.left, rc.bottom - rc.top])
}

pub fn text_of(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len as usize + 1];
        let got = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
        String::from_utf16_lossy(&buf[..got.max(0) as usize])
    }
}

/// "2025-06-01 14:05" in the computer's time zone.
pub fn local_time(secs: UnixSecs) -> String {
    let ticks = (secs.max(0) as u64 + 11_644_473_600) * 10_000_000;
    let ft = FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 };
    unsafe {
        let mut utc: SYSTEMTIME = zeroed();
        let mut local: SYSTEMTIME = zeroed();
        if FileTimeToSystemTime(&ft, &mut utc) == 0 || SystemTimeToTzSpecificLocalTime(null(), &utc, &mut local) == 0 {
            return format_utc(secs);
        }
        format!("{:04}-{:02}-{:02} {:02}:{:02}", local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute)
    }
}

/// Opens a folder in File Explorer.
pub fn open_folder(path: &Path) -> Result<(), String> {
    super::proc::explorer(path.as_os_str()).map_err(|e| format!("File Explorer could not be started: {e}"))
}

/// Gives memory the app is not using back to Windows. Pages it touches again come back on demand.
pub fn trim_memory() {
    unsafe { SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX) };
}

/// The first letter in upper case, so an error from deeper down reads as a sentence.
pub fn sentence(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map(|first| first.to_uppercase().chain(chars).collect()).unwrap_or_default()
}
