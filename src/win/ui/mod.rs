//! The tray app's windows: the tray icon, the settings window and small questions. Only the UI thread runs this code.
//!
//! A window procedure can be entered again while it runs: a control notifies its parent from inside the
//! `SendMessageW` that changed it. So the state the procedures share sits in thread-local `RefCell`s that are only
//! borrowed with `try_borrow_mut`; a notification that finds the state borrowed was caused by our own update and is
//! ignored. No borrow is held across a call that runs a message loop (a question, the tray menu, `DestroyWindow`).

mod bench;
pub mod dialogs;
pub mod settings;
pub mod shell;
pub mod tray;

use std::mem::{size_of, zeroed};
use std::path::Path;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{ERROR_CLASS_ALREADY_EXISTS, FILETIME, GetLastError, HWND, RECT, SYSTEMTIME};
use windows_sys::Win32::Graphics::Gdi::{
    COLOR_BTNFACE, CreateFontIndirectW, DT_CALCRECT, DT_EDITCONTROL, DT_NOPREFIX, DT_WORDBREAK, DeleteObject, DrawTextW, GetDC, HBRUSH, HFONT, ReleaseDC,
    SelectObject,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};
use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
use windows_sys::Win32::UI::Controls::{
    BST_CHECKED, BST_UNCHECKED, LVCF_SUBITEM, LVCF_TEXT, LVCF_WIDTH, LVCOLUMNW, LVIF_TEXT, LVIS_FOCUSED, LVIS_SELECTED, LVITEMW, LVM_DELETEALLITEMS,
    LVM_ENSUREVISIBLE, LVM_GETITEMCOUNT, LVM_GETNEXTITEM, LVM_INSERTCOLUMNW, LVM_INSERTITEMW, LVM_SETCOLUMNWIDTH, LVM_SETEXTENDEDLISTVIEWSTYLE,
    LVM_SETITEMSTATE, LVM_SETITEMTEXTW, LVNI_SELECTED, LVS_EX_DOUBLEBUFFER, LVS_EX_FULLROWSELECT, LVS_NOSORTHEADER, LVS_REPORT, LVS_SHOWSELALWAYS,
    LVS_SINGLESEL, SetWindowTheme,
};
use windows_sys::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow, SystemParametersInfoForDpi};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, IsWindowEnabled};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BM_GETCHECK, BM_SETCHECK, BS_AUTOCHECKBOX, BS_PUSHBUTTON, CB_ADDSTRING, CBS_AUTOHSCROLL, CBS_DROPDOWN, CS_HREDRAW, CS_VREDRAW, CreateWindowExW,
    ES_AUTOHSCROLL, GetWindowRect, GetWindowTextLengthW, GetWindowTextW, HICON, HMENU, IDC_ARROW, IsWindowVisible, LoadCursorW, NONCLIENTMETRICSW,
    RegisterClassExW, SPI_GETNONCLIENTMETRICS, SPI_GETWORKAREA, SW_HIDE, SW_SHOWNA, SWP_NOACTIVATE, SWP_NOZORDER, SendMessageW, SetWindowPos, SetWindowTextW,
    ShowWindow, SystemParametersInfoW, WM_SETFONT, WNDCLASSEXW, WNDPROC, WS_CHILD, WS_EX_CLIENTEDGE, WS_TABSTOP, WS_VISIBLE, WS_VSCROLL,
};

use super::wide;
use crate::interval::{UnixSecs, format_utc};

/// `SS_NOPREFIX`: a static control shows `&` as it is.
pub const SS_NOPREFIX: u32 = 0x80;
/// `SS_ENDELLIPSIS`: text that does not fit ends in "…".
pub const SS_ENDELLIPSIS: u32 = 0x4000;
/// A semibold font weight, for headings.
const HEADING_WEIGHT: i32 = 600;

/// Device-independent pixels (1/96 inch) to pixels at a DPI.
pub fn scale(v: i32, dpi: u32) -> i32 {
    (v * dpi as i32 + 48) / 96
}

/// The font Windows uses for message boxes at a DPI, and a semibold copy of it for headings.
fn message_fonts(dpi: u32) -> (HFONT, HFONT) {
    unsafe {
        let mut ncm: NONCLIENTMETRICSW = zeroed();
        ncm.cbSize = size_of::<NONCLIENTMETRICSW>() as u32;
        SystemParametersInfoForDpi(SPI_GETNONCLIENTMETRICS, ncm.cbSize, (&raw mut ncm).cast(), 0, dpi);
        let normal = CreateFontIndirectW(&ncm.lfMessageFont);
        let mut heading = ncm.lfMessageFont;
        heading.lfWeight = HEADING_WEIGHT;
        (normal, CreateFontIndirectW(&heading))
    }
}

struct Child {
    hwnd: HWND,
    /// Where it goes, in DIPs: x, y, width, height.
    rect: [i32; 4],
    bold: bool,
    /// List view column widths, in DIPs.
    columns: Vec<i32>,
}

/// A window and the controls on it, laid out in DIPs so the layout can follow the window to a monitor with another
/// DPI. Owns the fonts; drop it after the window is gone.
pub struct Form {
    pub hwnd: HWND,
    pub dpi: u32,
    font: HFONT,
    bold: HFONT,
    children: Vec<Child>,
}

impl Form {
    pub fn new(hwnd: HWND) -> Form {
        let dpi = match unsafe { GetDpiForWindow(hwnd) } {
            0 => 96,
            dpi => dpi,
        };
        let (font, bold) = message_fonts(dpi);
        Form { hwnd, dpi, font, bold, children: Vec::new() }
    }

    pub fn px(&self, v: i32) -> i32 {
        scale(v, self.dpi)
    }

    pub fn add(&mut self, class: &str, text: &str, style: u32, ex_style: u32, rect: [i32; 4], id: i32) -> HWND {
        let hwnd = unsafe {
            CreateWindowExW(
                ex_style,
                wide(class).as_ptr(),
                wide(text).as_ptr(),
                WS_CHILD | WS_VISIBLE | style,
                self.px(rect[0]),
                self.px(rect[1]),
                self.px(rect[2]),
                self.px(rect[3]),
                self.hwnd,
                id as usize as HMENU,
                GetModuleHandleW(null()),
                null(),
            )
        };
        unsafe { SendMessageW(hwnd, WM_SETFONT, self.font as usize, 0) };
        self.children.push(Child { hwnd, rect, bold: false, columns: Vec::new() });
        hwnd
    }

    pub fn label(&mut self, text: &str, rect: [i32; 4]) -> HWND {
        self.add("STATIC", text, SS_NOPREFIX, 0, rect, 0)
    }

    pub fn button(&mut self, text: &str, rect: [i32; 4], id: i32) -> HWND {
        self.add("BUTTON", text, WS_TABSTOP | BS_PUSHBUTTON as u32, 0, rect, id)
    }

    pub fn check(&mut self, text: &str, rect: [i32; 4], id: i32) -> HWND {
        self.add("BUTTON", text, WS_TABSTOP | BS_AUTOCHECKBOX as u32, 0, rect, id)
    }

    pub fn edit(&mut self, rect: [i32; 4], id: i32, style: u32) -> HWND {
        self.add("EDIT", "", WS_TABSTOP | ES_AUTOHSCROLL as u32 | style, WS_EX_CLIENTEDGE, rect, id)
    }

    /// A combo box the user can also type in. The height includes the drop-down list.
    pub fn combo(&mut self, rect: [i32; 4], id: i32, items: &[&str]) -> HWND {
        let hwnd = self.add("COMBOBOX", "", WS_TABSTOP | WS_VSCROLL | (CBS_DROPDOWN | CBS_AUTOHSCROLL) as u32, 0, rect, id);
        for item in items {
            unsafe { SendMessageW(hwnd, CB_ADDSTRING, 0, wide(item).as_ptr() as isize) };
        }
        hwnd
    }

    /// A report-style list view with one selectable row at a time.
    pub fn list(&mut self, rect: [i32; 4], id: i32, columns: &[(&str, i32)]) -> HWND {
        let style = WS_TABSTOP | LVS_REPORT | LVS_SINGLESEL | LVS_SHOWSELALWAYS | LVS_NOSORTHEADER;
        let hwnd = self.add("SysListView32", "", style, WS_EX_CLIENTEDGE, rect, id);
        unsafe {
            SendMessageW(hwnd, LVM_SETEXTENDEDLISTVIEWSTYLE, 0, (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER) as isize);
            SetWindowTheme(hwnd, wide("Explorer").as_ptr(), null());
        }
        for (i, (name, width)) in columns.iter().enumerate() {
            let mut text = wide(name);
            let mut col: LVCOLUMNW = unsafe { zeroed() };
            col.mask = LVCF_TEXT | LVCF_WIDTH | LVCF_SUBITEM;
            col.pszText = text.as_mut_ptr();
            col.cx = self.px(*width);
            col.iSubItem = i as i32;
            unsafe { SendMessageW(hwnd, LVM_INSERTCOLUMNW, i, (&raw const col) as isize) };
        }
        if let Some(child) = self.children.last_mut() {
            child.columns = columns.iter().map(|(_, w)| *w).collect();
        }
        hwnd
    }

    /// Shows a control's text in the heading font.
    pub fn embolden(&mut self, hwnd: HWND) {
        if let Some(child) = self.children.iter_mut().find(|c| c.hwnd == hwnd) {
            child.bold = true;
            unsafe { SendMessageW(hwnd, WM_SETFONT, self.bold as usize, 1) };
        }
    }

    /// The height `text` needs when wrapped at `width` DIPs, in DIPs.
    pub fn measure(&self, text: &str, width: i32, bold: bool) -> i32 {
        unsafe {
            let dc = GetDC(self.hwnd);
            let old = SelectObject(dc, if bold { self.bold } else { self.font });
            let mut rc = RECT { left: 0, top: 0, right: self.px(width), bottom: 0 };
            DrawTextW(dc, wide(text).as_ptr(), -1, &mut rc, DT_CALCRECT | DT_WORDBREAK | DT_NOPREFIX | DT_EDITCONTROL);
            SelectObject(dc, old);
            ReleaseDC(self.hwnd, dc);
            let dpi = self.dpi as i32;
            ((rc.bottom - rc.top) * 96 + dpi - 1) / dpi
        }
    }

    /// The window moved to a monitor with another DPI: new fonts, and every control moved and resized to match.
    pub fn set_dpi(&mut self, dpi: u32) {
        if dpi == self.dpi || dpi == 0 {
            return;
        }
        let (font, bold) = message_fonts(dpi);
        self.dpi = dpi;
        for child in &self.children {
            let [x, y, w, h] = child.rect.map(|v| scale(v, dpi));
            unsafe {
                SetWindowPos(child.hwnd, null_mut(), x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
                SendMessageW(child.hwnd, WM_SETFONT, if child.bold { bold } else { font } as usize, 1);
                for (i, width) in child.columns.iter().enumerate() {
                    SendMessageW(child.hwnd, LVM_SETCOLUMNWIDTH, i, scale(*width, dpi) as isize);
                }
            }
        }
        unsafe {
            DeleteObject(self.font);
            DeleteObject(self.bold);
        }
        self.font = font;
        self.bold = bold;
    }
}

impl Drop for Form {
    fn drop(&mut self) {
        unsafe {
            DeleteObject(self.font);
            DeleteObject(self.bold);
        }
    }
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

/// Position and outer size of a window with a client area of `client` DIPs, centred over `over` (or the work area)
/// and kept on the work area.
pub fn place(style: u32, ex_style: u32, client: (i32, i32), dpi: u32, over: HWND) -> [i32; 4] {
    unsafe {
        let mut rc = RECT { left: 0, top: 0, right: scale(client.0, dpi), bottom: scale(client.1, dpi) };
        AdjustWindowRectExForDpi(&mut rc, style, 0, ex_style, dpi);
        let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
        let mut work: RECT = zeroed();
        SystemParametersInfoW(SPI_GETWORKAREA, 0, (&raw mut work).cast(), 0);
        let mut area = work;
        if over.is_null() || IsWindowVisible(over) == 0 || GetWindowRect(over, &mut area) == 0 {
            area = work;
        }
        let x = (area.left + (area.right - area.left - w) / 2).clamp(work.left, (work.right - w).max(work.left));
        let y = (area.top + (area.bottom - area.top - h) / 2).clamp(work.top, (work.bottom - h).max(work.top));
        [x, y, w, h]
    }
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

/// Sets a control's text, unless it already shows it (which would flicker and move the caret).
pub fn set_text(hwnd: HWND, text: &str) {
    if text_of(hwnd) != text {
        unsafe { SetWindowTextW(hwnd, wide(text).as_ptr()) };
    }
}

pub fn checked(hwnd: HWND) -> bool {
    unsafe { SendMessageW(hwnd, BM_GETCHECK, 0, 0) == BST_CHECKED as isize }
}

pub fn set_checked(hwnd: HWND, on: bool) {
    if checked(hwnd) != on {
        unsafe { SendMessageW(hwnd, BM_SETCHECK, if on { BST_CHECKED } else { BST_UNCHECKED } as usize, 0) };
    }
}

pub fn enable(hwnd: HWND, on: bool) {
    if unsafe { IsWindowEnabled(hwnd) != 0 } != on {
        unsafe { EnableWindow(hwnd, on.into()) };
    }
}

pub fn show(hwnd: HWND, on: bool) {
    if unsafe { IsWindowVisible(hwnd) != 0 } != on {
        unsafe { ShowWindow(hwnd, if on { SW_SHOWNA } else { SW_HIDE }) };
    }
}

/// The selected row of a list view.
pub fn lv_selected(list: HWND) -> Option<usize> {
    let index = unsafe { SendMessageW(list, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize) };
    usize::try_from(index).ok()
}

/// Selects one row of a list view, or none.
pub fn lv_select(list: HWND, index: Option<usize>) {
    let mut item: LVITEMW = unsafe { zeroed() };
    item.stateMask = LVIS_SELECTED | LVIS_FOCUSED;
    unsafe {
        SendMessageW(list, LVM_SETITEMSTATE, usize::MAX, (&raw const item) as isize);
        if let Some(i) = index {
            item.state = LVIS_SELECTED | LVIS_FOCUSED;
            SendMessageW(list, LVM_SETITEMSTATE, i, (&raw const item) as isize);
            SendMessageW(list, LVM_ENSUREVISIBLE, i, 0);
        }
    }
}

fn lv_set_cell(list: HWND, row: usize, col: usize, text: &str) {
    let mut t = wide(text);
    let mut item: LVITEMW = unsafe { zeroed() };
    item.iSubItem = col as i32;
    item.pszText = t.as_mut_ptr();
    unsafe { SendMessageW(list, LVM_SETITEMTEXTW, row, (&raw const item) as isize) };
}

/// Shows `rows` in a list view. `shown` is what it showed before; only cells that changed are touched, so the
/// selection and the scroll position stay where they are.
pub fn lv_fill(list: HWND, rows: &[Vec<String>], shown: &mut Vec<Vec<String>>) {
    let count = unsafe { SendMessageW(list, LVM_GETITEMCOUNT, 0, 0) } as usize;
    if count != rows.len() || shown.len() != rows.len() {
        unsafe { SendMessageW(list, LVM_DELETEALLITEMS, 0, 0) };
        for (i, row) in rows.iter().enumerate() {
            let mut first = wide(row.first().map_or("", String::as_str));
            let mut item: LVITEMW = unsafe { zeroed() };
            item.mask = LVIF_TEXT;
            item.iItem = i as i32;
            item.pszText = first.as_mut_ptr();
            unsafe { SendMessageW(list, LVM_INSERTITEMW, 0, (&raw const item) as isize) };
            for (col, cell) in row.iter().enumerate().skip(1) {
                lv_set_cell(list, i, col, cell);
            }
        }
    } else {
        for (i, (row, old)) in rows.iter().zip(shown.iter()).enumerate() {
            for (col, cell) in row.iter().enumerate() {
                if old.get(col) != Some(cell) {
                    lv_set_cell(list, i, col, cell);
                }
            }
        }
    }
    *shown = rows.to_vec();
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

/// Opens a folder in File Explorer. Started as a process, so no COM is needed on this thread.
pub fn open_folder(path: &Path) -> Result<(), String> {
    std::process::Command::new("explorer.exe").arg(path).spawn().map(drop).map_err(|e| format!("File Explorer could not be started: {e}"))
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
