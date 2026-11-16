#![windows_subsystem = "windows"]
//! Spike S3: the smallest honest Win32 tray app, used to measure what a native tray costs.
//!
//! It contains only what the product shell needs:
//!   * a hidden top-level window (so it receives the `TaskbarCreated` broadcast),
//!   * a version-4 tray icon with an icon generated at run time, and a context menu built on demand,
//!   * a settings window built on demand and destroyed on close (list view, edit, combo, check box, buttons),
//!   * one lazily spawned worker thread running a WinHTTP conditional GET
//!     (two requests on one connection; the second sends the ETag learned from the first).
//!
//! Driven by `spikes\s3-measure.ps1` with `PostMessageW(WM_COMMAND, id)`:
//!   1001 open settings, 1002 quit, 1003 poll once, 1004 write the report now.
//!
//! Options
//!   `--report <file>`    write `key=value` lines when the app quits (or on command 1004)
//!   `--exit-after-ready` quit as soon as the tray icon is added (the report is written on the way out)
//!   `--trim`             trim the working set after start-up, after a poll and after the settings window closes
//!   `TSS_SPIKE_TOKEN`    environment variable with a bearer token for the poll (never written to the report)

use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};
use std::sync::{Mutex, Once, OnceLock};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::Networking::WinHttp::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
use windows_sys::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::System::WindowsProgramming::QueryProcessCycleTime;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const WM_TRAY: u32 = WM_APP + 1;
const WM_POLL_DONE: u32 = WM_APP + 2;
const NIN_KEYSELECT: u32 = NIN_SELECT | NINF_KEY;

const CMD_SETTINGS: usize = 1001;
const CMD_QUIT: usize = 1002;
const CMD_POLL: usize = 1003;
const CMD_REPORT: usize = 1004;

const ID_LIST: i32 = 2001;
const ID_REPO: i32 = 2002;
const ID_INTERVAL: i32 = 2003;
const ID_USE_GH: i32 = 2004;
const ID_ADD: i32 = 2005;
const ID_REMOVE: i32 = 2006;
const ID_CHECK: i32 = 2007;
const ID_CLOSE: i32 = 2008;

const MAIN_CLASS: &str = "TssSpikeMain";
const SETTINGS_CLASS: &str = "TssSpikeSettings";
const POLL_HOST: &str = "api.github.com";
const POLL_PATH: &str = "/repos/AngelosP/tiny-skill-sync-fixtures-public/git/trees/main:skills";
const INTERVALS: [&str; 4] = ["Daily", "Weekly", "Monthly", "Every 6 hours"];

struct Options {
    report: Option<String>,
    exit_after_ready: bool,
    trim: bool,
}

static OPTIONS: OnceLock<Options> = OnceLock::new();
static MARKS: Mutex<Vec<(&'static str, u64)>> = Mutex::new(Vec::new());
static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());
static MAIN_WND: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static SETTINGS_WND: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static TRAY_ICON: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
static POLLING: AtomicBool = AtomicBool::new(false);
static REPORT_WRITTEN: AtomicBool = AtomicBool::new(false);
static POLL_RUNS: AtomicU32 = AtomicU32::new(0);
static SETTINGS_OPENS: AtomicU32 = AtomicU32::new(0);

fn options() -> &'static Options {
    OPTIONS.get().expect("options are set in main")
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn put_wide<const N: usize>(dst: &mut [u16; N], s: &str) {
    for (i, unit) in s.encode_utf16().take(N - 1).enumerate() {
        dst[i] = unit;
    }
}

fn filetime_u64(ft: FILETIME) -> u64 {
    (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime)
}

fn now_100ns() -> u64 {
    let mut ft = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    unsafe { GetSystemTimePreciseAsFileTime(&mut ft) };
    filetime_u64(ft)
}

fn ms_since(start_100ns: u64) -> f64 {
    now_100ns().saturating_sub(start_100ns) as f64 / 10_000.0
}

/// Creation, kernel and user time of this process, in 100 ns units.
fn process_times() -> (u64, u64, u64) {
    let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    unsafe { GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user) };
    (filetime_u64(created), filetime_u64(kernel), filetime_u64(user))
}

fn mark(name: &'static str) {
    MARKS.lock().unwrap().push((name, now_100ns()));
}

fn log(line: String) {
    LOG.lock().unwrap().push(line);
}

fn snapshot(tag: &str) {
    let mut c: PROCESS_MEMORY_COUNTERS_EX = unsafe { zeroed() };
    c.cb = size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    unsafe { GetProcessMemoryInfo(GetCurrentProcess(), (&mut c as *mut PROCESS_MEMORY_COUNTERS_EX).cast(), c.cb) };
    let (created, kernel, user) = process_times();
    let mut cycles = 0u64;
    unsafe { QueryProcessCycleTime(GetCurrentProcess(), &mut cycles) };
    log(format!("{tag}.working_set_kib={}", c.WorkingSetSize / 1024));
    log(format!("{tag}.peak_working_set_kib={}", c.PeakWorkingSetSize / 1024));
    log(format!("{tag}.private_kib={}", c.PrivateUsage / 1024));
    log(format!("{tag}.page_faults={}", c.PageFaultCount));
    log(format!("{tag}.cpu_user_ms={:.1}", user as f64 / 10_000.0));
    log(format!("{tag}.cpu_kernel_ms={:.1}", kernel as f64 / 10_000.0));
    log(format!("{tag}.cycles={cycles}"));
    log(format!("{tag}.since_creation_ms={:.2}", now_100ns().saturating_sub(created) as f64 / 10_000.0));
}

fn trim_working_set() {
    unsafe { SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX) };
}

fn write_report() {
    let Some(path) = options().report.as_deref() else { return };
    if REPORT_WRITTEN.swap(true, Ordering::AcqRel) {
        return;
    }
    snapshot("final");
    let created = process_times().0;
    let mut out = String::new();
    for (name, at) in MARKS.lock().unwrap().iter() {
        out.push_str(&format!("mark.{name}_ms={:.2}\n", at.saturating_sub(created) as f64 / 10_000.0));
    }
    for line in LOG.lock().unwrap().iter() {
        out.push_str(line);
        out.push('\n');
    }
    let _ = std::fs::write(path, out);
}

fn main() {
    mark("main");
    let args: Vec<String> = std::env::args().collect();
    let _ = OPTIONS.set(Options {
        report: args.iter().position(|a| a == "--report").and_then(|i| args.get(i + 1)).cloned(),
        exit_after_ready: args.iter().any(|a| a == "--exit-after-ready"),
        trim: args.iter().any(|a| a == "--trim"),
    });
    run();
}

fn run() {
    unsafe {
        let hinst = GetModuleHandleW(null());
        let class = wide(MAIN_CLASS);
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(main_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: null_mut(),
            hCursor: null_mut(),
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: class.as_ptr(),
            hIconSm: null_mut(),
        };
        RegisterClassExW(&wc);
        TASKBAR_CREATED.store(RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()), Ordering::Relaxed);
        mark("class_registered");

        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class.as_ptr(),
            wide("tiny-skill-sync spike").as_ptr(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            hinst,
            null(),
        );
        if hwnd.is_null() {
            log(format!("error=CreateWindowExW {}", GetLastError()));
            write_report();
            return;
        }
        MAIN_WND.store(hwnd, Ordering::Release);
        mark("window_created");

        let icon = build_icon(GetSystemMetrics(SM_CXSMICON));
        TRAY_ICON.store(icon, Ordering::Release);
        mark("icon_built");

        let added = tray_add(hwnd, "tiny-skill-sync spike");
        mark("tray_added");
        log(format!("tray.added={added}"));
        let mut rect: RECT = zeroed();
        let id = NOTIFYICONIDENTIFIER { cbSize: size_of::<NOTIFYICONIDENTIFIER>() as u32, hWnd: hwnd, uID: 1, guidItem: zeroed() };
        let hr = Shell_NotifyIconGetRect(&id, &mut rect);
        log(format!("tray.rect_hr=0x{:08X}", hr as u32));
        log(format!("tray.rect={},{},{},{}", rect.left, rect.top, rect.right, rect.bottom));
        snapshot("ready");
        if options().trim {
            trim_working_set();
            snapshot("ready_trimmed");
        }
        if options().exit_after_ready {
            DestroyWindow(hwnd);
        }

        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            let settings = SETTINGS_WND.load(Ordering::Acquire);
            if !settings.is_null() && IsDialogMessageW(settings, &msg) != 0 {
                continue;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn main_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TRAY => {
                on_tray(hwnd, wp, lp);
                0
            }
            WM_COMMAND => {
                match wp & 0xFFFF {
                    CMD_SETTINGS => open_settings(),
                    CMD_QUIT => {
                        DestroyWindow(hwnd);
                    }
                    CMD_POLL => start_poll(hwnd),
                    CMD_REPORT => write_report(),
                    _ => {}
                }
                0
            }
            WM_POLL_DONE => {
                tray_tip(hwnd, &format!("spike: last poll HTTP {wp}"));
                if options().trim {
                    trim_working_set();
                }
                0
            }
            WM_QUERYENDSESSION => 1,
            WM_ENDSESSION => {
                if wp != 0 {
                    tray_remove(hwnd);
                }
                0
            }
            WM_DESTROY => {
                tray_remove(hwnd);
                write_report();
                PostQuitMessage(0);
                0
            }
            m if m != 0 && m == TASKBAR_CREATED.load(Ordering::Relaxed) => {
                // Explorer restarted: the icon is gone, add it again.
                tray_add(hwnd, "tiny-skill-sync spike");
                0
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

// ---------------------------------------------------------------- tray

fn tray_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut nid: NOTIFYICONDATAW = unsafe { zeroed() };
    nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    nid
}

fn tray_add(hwnd: HWND, tip: &str) -> bool {
    let mut nid = tray_data(hwnd);
    nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
    nid.uCallbackMessage = WM_TRAY;
    nid.hIcon = TRAY_ICON.load(Ordering::Acquire);
    put_wide(&mut nid.szTip, tip);
    unsafe {
        let added = Shell_NotifyIconW(NIM_ADD, &nid) != 0;
        nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        let versioned = Shell_NotifyIconW(NIM_SETVERSION, &nid) != 0;
        added && versioned
    }
}

fn tray_tip(hwnd: HWND, tip: &str) {
    let mut nid = tray_data(hwnd);
    nid.uFlags = NIF_TIP | NIF_SHOWTIP;
    put_wide(&mut nid.szTip, tip);
    unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) };
}

fn tray_remove(hwnd: HWND) {
    let nid = tray_data(hwnd);
    unsafe {
        Shell_NotifyIconW(NIM_DELETE, &nid);
        let icon = TRAY_ICON.swap(null_mut(), Ordering::AcqRel);
        if !icon.is_null() {
            DestroyIcon(icon);
        }
    }
}

fn on_tray(hwnd: HWND, wp: WPARAM, lp: LPARAM) {
    match (lp as u32) & 0xFFFF {
        WM_CONTEXTMENU => {
            let x = i32::from((wp & 0xFFFF) as u16 as i16);
            let y = i32::from(((wp >> 16) & 0xFFFF) as u16 as i16);
            show_menu(hwnd, x, y);
        }
        NIN_SELECT | NIN_KEYSELECT => open_settings(),
        _ => {}
    }
}

fn show_menu(hwnd: HWND, x: i32, y: i32) {
    unsafe {
        let menu = CreatePopupMenu();
        AppendMenuW(menu, MF_STRING, CMD_SETTINGS, wide("Open settings").as_ptr());
        AppendMenuW(menu, MF_STRING, CMD_POLL, wide("Check now").as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        AppendMenuW(menu, MF_STRING, CMD_QUIT, wide("Quit").as_ptr());
        // Required so the menu closes when the user clicks elsewhere.
        SetForegroundWindow(hwnd);
        TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, x, y, 0, hwnd, null());
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
    }
}

/// A round, anti-aliased icon drawn into a 32-bit DIB. The product will build its status variants the same way.
fn build_icon(size: i32) -> HICON {
    unsafe {
        let mut bmi: BITMAPINFO = zeroed();
        bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = size;
        bmi.bmiHeader.biHeight = -size;
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB;
        let mut bits: *mut c_void = null_mut();
        let color = CreateDIBSection(null_mut(), &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        if color.is_null() || bits.is_null() {
            return null_mut();
        }
        let pixels = std::slice::from_raw_parts_mut(bits.cast::<u32>(), (size * size) as usize);
        let center = (size as f32 - 1.0) / 2.0;
        let radius = size as f32 / 2.0 - 0.5;
        for y in 0..size {
            for x in 0..size {
                let dist = (((x as f32 - center).powi(2) + (y as f32 - center).powi(2)).sqrt() - radius).clamp(-0.5, 0.5);
                let alpha = ((0.5 - dist) * 255.0) as u32;
                let (r, g, b) = (0x2D * alpha / 255, 0x7D * alpha / 255, 0xD2 * alpha / 255);
                pixels[(y * size + x) as usize] = (alpha << 24) | (r << 16) | (g << 8) | b;
            }
        }
        let mask_bits = vec![0u8; ((size + 15) / 16 * 2 * size) as usize];
        let mask = CreateBitmap(size, size, 1, 1, mask_bits.as_ptr().cast());
        let info = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&info);
        DeleteObject(color);
        DeleteObject(mask);
        icon
    }
}

// ---------------------------------------------------------------- settings window

fn open_settings() {
    unsafe {
        let existing = SETTINGS_WND.load(Ordering::Acquire);
        if !existing.is_null() {
            ShowWindow(existing, SW_RESTORE);
            SetForegroundWindow(existing);
            return;
        }
        let started = now_100ns();
        let hinst = GetModuleHandleW(null());
        static REGISTER: Once = Once::new();
        REGISTER.call_once(|| {
            let icc = INITCOMMONCONTROLSEX { dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_LISTVIEW_CLASSES };
            InitCommonControlsEx(&icc);
            let class = wide(SETTINGS_CLASS);
            let wc = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(settings_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinst,
                hIcon: null_mut(),
                hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                hbrBackground: (COLOR_BTNFACE + 1) as usize as HBRUSH,
                lpszMenuName: null(),
                lpszClassName: class.as_ptr(),
                hIconSm: null_mut(),
            };
            RegisterClassExW(&wc);
        });

        let dpi = GetDpiForSystem();
        let scale = |v: i32| (v * dpi as i32 + 48) / 96;
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let ex_style = WS_EX_CONTROLPARENT;
        let mut rc = RECT { left: 0, top: 0, right: scale(544), bottom: scale(322) };
        AdjustWindowRectExForDpi(&mut rc, style, 0, ex_style, dpi);
        let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
        let mut work: RECT = zeroed();
        SystemParametersInfoW(SPI_GETWORKAREA, 0, (&mut work as *mut RECT).cast(), 0);
        let x = work.left + ((work.right - work.left) - w) / 2;
        let y = work.top + ((work.bottom - work.top) - h) / 2;

        let hwnd = CreateWindowExW(
            ex_style,
            wide(SETTINGS_CLASS).as_ptr(),
            wide("tiny-skill-sync - settings (spike)").as_ptr(),
            style,
            x,
            y,
            w,
            h,
            null_mut(),
            null_mut(),
            hinst,
            null(),
        );
        if hwnd.is_null() {
            log(format!("settings.error=CreateWindowExW {}", GetLastError()));
            return;
        }
        SETTINGS_WND.store(hwnd, Ordering::Release);
        ShowWindow(hwnd, SW_SHOW);
        UpdateWindow(hwnd);
        SetForegroundWindow(hwnd);
        let n = SETTINGS_OPENS.fetch_add(1, Ordering::AcqRel) + 1;
        log(format!("settings.open{n}_ms={:.2}", ms_since(started)));
    }
}

fn message_font(dpi: u32) -> HFONT {
    unsafe {
        let mut ncm: NONCLIENTMETRICSW = zeroed();
        ncm.cbSize = size_of::<NONCLIENTMETRICSW>() as u32;
        SystemParametersInfoForDpi(SPI_GETNONCLIENTMETRICS, ncm.cbSize, (&mut ncm as *mut NONCLIENTMETRICSW).cast(), 0, dpi);
        CreateFontIndirectW(&ncm.lfMessageFont)
    }
}

#[allow(clippy::too_many_arguments)]
fn make_child(parent: HWND, class: &str, text: &str, style: u32, ex_style: u32, r: [i32; 4], id: i32, dpi: u32, font: HFONT) -> HWND {
    let s = |v: i32| (v * dpi as i32 + 48) / 96;
    unsafe {
        let h = CreateWindowExW(
            ex_style,
            wide(class).as_ptr(),
            wide(text).as_ptr(),
            WS_CHILD | WS_VISIBLE | style,
            s(r[0]),
            s(r[1]),
            s(r[2]),
            s(r[3]),
            parent,
            id as usize as HMENU,
            GetModuleHandleW(null()),
            null(),
        );
        SendMessageW(h, WM_SETFONT, font as usize, 1);
        h
    }
}

fn lv_add_column(list: HWND, index: i32, text: &str, width: i32) {
    let mut t = wide(text);
    let mut col: LVCOLUMNW = unsafe { zeroed() };
    col.mask = LVCF_TEXT | LVCF_WIDTH | LVCF_SUBITEM;
    col.pszText = t.as_mut_ptr();
    col.cx = width;
    col.iSubItem = index;
    unsafe { SendMessageW(list, LVM_INSERTCOLUMNW, index as usize, &col as *const LVCOLUMNW as isize) };
}

fn lv_add_row(list: HWND, cells: &[&str]) {
    unsafe {
        let count = SendMessageW(list, LVM_GETITEMCOUNT, 0, 0) as i32;
        let mut first = wide(cells[0]);
        let mut item: LVITEMW = zeroed();
        item.mask = LVIF_TEXT;
        item.iItem = count;
        item.pszText = first.as_mut_ptr();
        let row = SendMessageW(list, LVM_INSERTITEMW, 0, &item as *const LVITEMW as isize) as usize;
        for (i, cell) in cells.iter().enumerate().skip(1) {
            let mut t = wide(cell);
            let mut sub: LVITEMW = zeroed();
            sub.iSubItem = i as i32;
            sub.pszText = t.as_mut_ptr();
            SendMessageW(list, LVM_SETITEMTEXTW, row, &sub as *const LVITEMW as isize);
        }
    }
}

fn build_settings(hwnd: HWND) {
    unsafe {
        let dpi = GetDpiForWindow(hwnd);
        let font = message_font(dpi);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, font as isize);
        let label = |text: &str, x: i32, y: i32, w: i32| {
            make_child(hwnd, "STATIC", text, 0, 0, [x, y, w, 18], 0, dpi, font);
        };
        label("Repositories", 12, 10, 300);
        let list = make_child(
            hwnd,
            "SysListView32",
            "",
            WS_TABSTOP | WS_BORDER | LVS_REPORT | LVS_SINGLESEL | LVS_SHOWSELALWAYS,
            0,
            [12, 30, 520, 150],
            ID_LIST,
            dpi,
            font,
        );
        SendMessageW(list, LVM_SETEXTENDEDLISTVIEWSTYLE, 0, (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER) as isize);
        SetWindowTheme(list, wide("Explorer").as_ptr(), null());
        for (i, (name, width)) in [("Repository", 210), ("Interval", 90), ("Last check", 110), ("Status", 100)].iter().enumerate() {
            lv_add_column(list, i as i32, name, (width * dpi as i32 + 48) / 96);
        }
        lv_add_row(list, &["AngelosP/tiny-skill-sync-fixtures-public", "Daily", "never", "pending"]);
        lv_add_row(list, &["AngelosP/tiny-skill-sync-fixtures-private", "Weekly", "never", "pending"]);

        label("Repository (owner/name)", 12, 192, 300);
        make_child(hwnd, "EDIT", "", WS_TABSTOP | ES_AUTOHSCROLL as u32, WS_EX_CLIENTEDGE, [12, 210, 300, 24], ID_REPO, dpi, font);
        label("Check", 324, 192, 150);
        let combo = make_child(hwnd, "COMBOBOX", "", WS_TABSTOP | WS_VSCROLL | CBS_DROPDOWNLIST as u32, 0, [324, 210, 150, 200], ID_INTERVAL, dpi, font);
        for item in INTERVALS {
            SendMessageW(combo, CB_ADDSTRING, 0, wide(item).as_ptr() as isize);
        }
        SendMessageW(combo, CB_SETCURSEL, 0, 0);
        make_child(
            hwnd,
            "BUTTON",
            "Use the GitHub CLI token for private repositories",
            WS_TABSTOP | BS_AUTOCHECKBOX as u32,
            0,
            [12, 244, 400, 22],
            ID_USE_GH,
            dpi,
            font,
        );

        let button = |text: &str, x: i32, id: i32, default: bool| {
            let kind = if default { BS_DEFPUSHBUTTON } else { BS_PUSHBUTTON };
            make_child(hwnd, "BUTTON", text, WS_TABSTOP | kind as u32, 0, [x, 282, 84, 26], id, dpi, font);
        };
        button("Add", 12, ID_ADD, true);
        button("Remove", 104, ID_REMOVE, false);
        button("Check now", 196, ID_CHECK, false);
        button("Close", 448, ID_CLOSE, false);
    }
}

fn on_settings_command(hwnd: HWND, id: i32) {
    unsafe {
        match id {
            ID_ADD | IDOK => {
                let edit = GetDlgItem(hwnd, ID_REPO);
                let mut buf = [0u16; 256];
                let n = GetWindowTextW(edit, buf.as_mut_ptr(), buf.len() as i32);
                if n > 0 {
                    let repo = String::from_utf16_lossy(&buf[..n as usize]);
                    let selected = SendMessageW(GetDlgItem(hwnd, ID_INTERVAL), CB_GETCURSEL, 0, 0) as usize;
                    let interval = INTERVALS.get(selected).copied().unwrap_or(INTERVALS[0]);
                    lv_add_row(GetDlgItem(hwnd, ID_LIST), &[&repo, interval, "never", "pending"]);
                    SetWindowTextW(edit, wide("").as_ptr());
                }
            }
            ID_REMOVE => {
                let list = GetDlgItem(hwnd, ID_LIST);
                let selected = SendMessageW(list, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize);
                if selected >= 0 {
                    SendMessageW(list, LVM_DELETEITEM, selected as usize, 0);
                }
            }
            ID_CHECK => {
                PostMessageW(MAIN_WND.load(Ordering::Acquire), WM_COMMAND, CMD_POLL, 0);
            }
            ID_CLOSE | IDCANCEL => {
                DestroyWindow(hwnd);
            }
            _ => {}
        }
    }
}

unsafe extern "system" fn settings_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_CREATE => {
                build_settings(hwnd);
                0
            }
            WM_COMMAND => {
                on_settings_command(hwnd, (wp & 0xFFFF) as i32);
                0
            }
            WM_CLOSE => {
                DestroyWindow(hwnd);
                0
            }
            WM_DESTROY => {
                let font = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as HFONT;
                if !font.is_null() {
                    DeleteObject(font);
                }
                SETTINGS_WND.store(null_mut(), Ordering::Release);
                if options().trim {
                    trim_working_set();
                }
                0
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

// ---------------------------------------------------------------- poll (worker thread)

struct Handle(*mut c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { WinHttpCloseHandle(self.0) };
        }
    }
}

struct Response {
    status: u32,
    etag: String,
    limit: String,
    remaining: String,
    bytes: usize,
    first_byte: u8,
    ms_headers: f64,
    ms_total: f64,
}

fn start_poll(main: HWND) {
    if POLLING.swap(true, Ordering::AcqRel) {
        return;
    }
    let main_addr = main as usize;
    let spawned = std::thread::Builder::new().name("poll".into()).stack_size(512 * 1024).spawn(move || {
        let status = poll_once();
        POLLING.store(false, Ordering::Release);
        unsafe { PostMessageW(main_addr as HWND, WM_POLL_DONE, status as usize, 0) };
    });
    if spawned.is_err() {
        POLLING.store(false, Ordering::Release);
        log("poll.error=thread spawn failed".to_string());
    }
}

fn poll_once() -> u32 {
    let n = POLL_RUNS.fetch_add(1, Ordering::AcqRel) + 1;
    let tag = format!("poll{n}");
    let token = std::env::var("TSS_SPIKE_TOKEN").ok().filter(|t| !t.is_empty());
    log(format!("{tag}.auth={}", if token.is_some() { "token" } else { "anonymous" }));
    let started = now_100ns();
    let mut last_status = 0;
    unsafe {
        let background = SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN);
        log(format!("{tag}.thread_background_mode={}", background != 0));

        let session = Handle(WinHttpOpen(wide("tiny-skill-sync-spike/0.1").as_ptr(), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, null(), null(), 0));
        if session.0.is_null() {
            log(format!("{tag}.error=WinHttpOpen {}", GetLastError()));
            return 0;
        }
        WinHttpSetTimeouts(session.0, 5_000, 5_000, 5_000, 10_000);
        let connect = Handle(WinHttpConnect(session.0, wide(POLL_HOST).as_ptr(), 443, 0));
        if connect.0.is_null() {
            log(format!("{tag}.error=WinHttpConnect {}", GetLastError()));
            return 0;
        }
        log(format!("{tag}.session_open_ms={:.2}", ms_since(started)));

        let mut etag: Option<String> = None;
        for round in 1..=2 {
            match http_get(connect.0, POLL_PATH, token.as_deref(), etag.as_deref()) {
                Ok(r) => {
                    log(format!("{tag}.r{round}.status={}", r.status));
                    log(format!("{tag}.r{round}.etag={}", r.etag));
                    log(format!("{tag}.r{round}.sent_if_none_match={}", etag.is_some()));
                    log(format!("{tag}.r{round}.ratelimit_limit={}", r.limit));
                    log(format!("{tag}.r{round}.ratelimit_remaining={}", r.remaining));
                    log(format!("{tag}.r{round}.body_bytes={}", r.bytes));
                    log(format!("{tag}.r{round}.body_first_byte={}", r.first_byte));
                    log(format!("{tag}.r{round}.headers_ms={:.2}", r.ms_headers));
                    log(format!("{tag}.r{round}.total_ms={:.2}", r.ms_total));
                    last_status = r.status;
                    if !r.etag.is_empty() {
                        etag = Some(r.etag);
                    }
                }
                Err(e) => {
                    log(format!("{tag}.r{round}.error={e}"));
                    break;
                }
            }
        }
    }
    log(format!("{tag}.total_ms={:.2}", ms_since(started)));
    last_status
}

fn query_text(req: *mut c_void, level: u32, name: Option<&str>) -> String {
    let name_w = name.map(wide);
    let name_ptr = name_w.as_ref().map_or(null(), |v| v.as_ptr());
    let mut buf = [0u16; 256];
    let mut len = (buf.len() * 2) as u32;
    let ok = unsafe { WinHttpQueryHeaders(req, level, name_ptr, buf.as_mut_ptr().cast(), &mut len, null_mut()) };
    if ok == 0 { String::new() } else { String::from_utf16_lossy(&buf[..len as usize / 2]) }
}

fn http_get(connect: *mut c_void, path: &str, token: Option<&str>, etag: Option<&str>) -> Result<Response, String> {
    unsafe {
        let started = now_100ns();
        let req = Handle(WinHttpOpenRequest(connect, wide("GET").as_ptr(), wide(path).as_ptr(), null(), null(), null(), WINHTTP_FLAG_SECURE));
        if req.0.is_null() {
            return Err(format!("WinHttpOpenRequest {}", GetLastError()));
        }
        let decompress: u32 = WINHTTP_DECOMPRESSION_FLAG_GZIP | WINHTTP_DECOMPRESSION_FLAG_DEFLATE;
        WinHttpSetOption(req.0, WINHTTP_OPTION_DECOMPRESSION, (&decompress as *const u32).cast(), 4);
        // A redirect must stay visible: the product treats it as "indeterminate", never as a result.
        let never: u32 = WINHTTP_OPTION_REDIRECT_POLICY_NEVER;
        WinHttpSetOption(req.0, WINHTTP_OPTION_REDIRECT_POLICY, (&never as *const u32).cast(), 4);

        let mut headers = String::from("Accept: application/vnd.github+json\r\nX-GitHub-Api-Version: 2022-11-28\r\n");
        if let Some(t) = token {
            headers.push_str(&format!("Authorization: Bearer {t}\r\n"));
        }
        if let Some(e) = etag {
            headers.push_str(&format!("If-None-Match: {e}\r\n"));
        }
        let headers_w: Vec<u16> = headers.encode_utf16().collect();
        if WinHttpSendRequest(req.0, headers_w.as_ptr(), headers_w.len() as u32, null(), 0, 0, 0) == 0 {
            return Err(format!("WinHttpSendRequest {}", GetLastError()));
        }
        if WinHttpReceiveResponse(req.0, null_mut()) == 0 {
            return Err(format!("WinHttpReceiveResponse {}", GetLastError()));
        }
        let ms_headers = ms_since(started);

        let mut status: u32 = 0;
        let mut len = 4u32;
        WinHttpQueryHeaders(req.0, WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER, null(), (&mut status as *mut u32).cast(), &mut len, null_mut());
        let etag = query_text(req.0, WINHTTP_QUERY_ETAG, None);
        let limit = query_text(req.0, WINHTTP_QUERY_CUSTOM, Some("x-ratelimit-limit"));
        let remaining = query_text(req.0, WINHTTP_QUERY_CUSTOM, Some("x-ratelimit-remaining"));

        let (mut bytes, mut first_byte) = (0usize, 0u8);
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            let mut available = 0u32;
            if WinHttpQueryDataAvailable(req.0, &mut available) == 0 {
                return Err(format!("WinHttpQueryDataAvailable {}", GetLastError()));
            }
            if available == 0 {
                break;
            }
            let want = (available as usize).min(chunk.len());
            let mut read = 0u32;
            if WinHttpReadData(req.0, chunk.as_mut_ptr().cast(), want as u32, &mut read) == 0 {
                return Err(format!("WinHttpReadData {}", GetLastError()));
            }
            if read == 0 {
                break;
            }
            if bytes == 0 {
                first_byte = chunk[0];
            }
            bytes += read as usize;
            if bytes > 8 << 20 {
                return Err("body larger than 8 MiB".to_string());
            }
        }
        Ok(Response { status, etag, limit, remaining, bytes, first_byte, ms_headers, ms_total: ms_since(started) })
    }
}
