//! The main window: a band across the top that says how everything is, the repositories down the left, and on the
//! right the page chosen there (a repository and its skills, Activity or Settings). "Show changes" turns the whole
//! window into the differences of one skill until Back. Every change is saved at once; there is no OK button.
//!
//! What it shows comes from [`shell::snapshot`], on a posted `WM_REFRESH`; [`page`] lays it out and [`control`] draws
//! it. Handlers read what they need from the state, let go of it, and then act, so a question (which runs a message
//! loop) or a notification sent back from inside a change never finds the state borrowed by them.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::fs;
use std::mem::zeroed;
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::time::Instant;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{RDW_ALLCHILDREN, RDW_ERASE, RDW_FRAME, RDW_INVALIDATE, RDW_UPDATENOW, RedrawWindow};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForSystem, GetDpiForWindow, GetSystemMetricsForDpi};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetFocus, SetFocus};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BM_GETCHECK, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyWindow, GetClientRect, GetDlgCtrlID, HICON, ICON_BIG, ICON_SMALL, IDCANCEL, IsChild,
    IsIconic, IsWindow, IsWindowVisible, IsZoomed, KillTimer, MINMAXINFO, PostMessageW, SIZE_MINIMIZED, SM_CXICON, SM_CXSMICON, SM_CXVSCROLL, SPI_GETWORKAREA,
    SW_HIDE, SW_RESTORE, SW_SHOWNORMAL, SWP_NOACTIVATE, SWP_NOZORDER, SWP_SHOWWINDOW, SendMessageW, SetForegroundWindow, SetTimer, SetWindowPos, ShowWindow,
    SystemParametersInfoW, WA_INACTIVE, WM_ACTIVATE, WM_APP, WM_CLOSE, WM_COMMAND, WM_DPICHANGED, WM_ERASEBKGND, WM_EXITSIZEMOVE, WM_GETMINMAXINFO,
    WM_NCDESTROY, WM_PAINT, WM_SETICON, WM_SETTINGCHANGE, WM_SIZE, WM_SYSCOLORCHANGE, WM_THEMECHANGED, WM_TIMER, WS_CLIPCHILDREN, WS_EX_CONTROLPARENT,
    WS_OVERLAPPEDWINDOW,
};

use super::control::{self, BN_CLICKED, CBN_SELENDOK, RowCmd, Surface};
use super::dialogs::{self, Ask, Check, Field};
use super::page::{self, CUSTOM, DiffView, Fit, HERO_H, Hit, MIN_LIST_W, NAV_W, Page, Part, Report, SYNC_TIP, SettingsPage, SourcePage, SplitKey, id};
use super::paint::{self, Font, Rect, glyph};
use super::shell::{self, Snapshot};
use super::theme::{self, Role};
use super::{bench, local_time, open_folder, place, register_class, scale, sentence, text_of, tray, trim_memory};
use crate::app::{DraftStage, Job};
use crate::autostart::{Keys, Startup};
use crate::board::{APP_NAME, Action, Row, SourceView, source_label};
use crate::config::{Config, Paths, RepoConfig, UpdatePrefs};
use crate::github::web_url;
use crate::interval::{Interval, MIN_POLL_MINUTES};
use crate::logo;
use crate::pathsafe::printable;
use crate::source::SourceKind;
use crate::sources::SourceConfig;
use crate::win::{autorun, clip, cred, proc, wide};

const CLASS: &str = "TinySkillSyncWindow";
const STYLE: u32 = WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN;
const EX_STYLE: u32 = WS_EX_CONTROLPARENT;
/// The client size the first time it opens, and the smallest it can be made, in DIPs.
const SIZE: (i32, i32) = (980, 680);
const MIN_SIZE: (i32, i32) = (640, 480);
/// Where the window size and preferred list width are kept, in the app's folder.
const SIZE_FILE: &str = "window.txt";
/// Show what the snapshot says now.
const WM_REFRESH: u32 = WM_APP + 10;
/// "5 minutes ago" goes stale; this keeps it roughly right.
const TIMER_REFRESH: usize = 1;
const REFRESH_MS: u32 = 60_000;
/// The panels' ids. Not 1 or 2: Enter and Escape make the dialog manager look for a child with those.
const HERO: i32 = 101;
const NAV: i32 = 102;
const PAGE: i32 = 103;
const HEAD: i32 = 104;
const FILES: i32 = 105;
const CODE: i32 = 106;
const SPLITTER: i32 = 107;
/// The entries of a repository's More menu.
const SYNC_ONE: &str = "Check now";
const ON_GITHUB: &str = "Open on GitHub";
const SKILLS_FOLDER: &str = "Open skills folder";
const REMOVE: &str = "Remove source…";

#[derive(Clone, Copy)]
struct Panels {
    hero: HWND,
    nav: HWND,
    page: HWND,
    /// The diff view: its header, the files that are different, and the lines of one.
    head: HWND,
    files: HWND,
    code: HWND,
    splitter: HWND,
}

impl Panels {
    fn all(self) -> [HWND; 7] {
        [self.hero, self.nav, self.page, self.head, self.files, self.code, self.splitter]
    }
}

struct Win {
    p: Panels,
    icons: [HICON; 2],
    snap: Option<Snapshot>,
    /// The page on the right; `None` is the welcome page.
    page: Option<Page>,
    add_form: Option<(u64, AddForm)>,
    /// The lane to show once the source being saved is in the settings.
    adding: Option<String>,
    /// Keep a rejected source's details for the next Add Source attempt.
    retry_add_form: Option<AddForm>,
    /// The skills of the repository shown that the user opened or closed, by name.
    toggled: HashSet<String>,
    diff: Option<DiffView>,
    /// Return to the comparison button rather than the header if it is still visible.
    from_card: bool,
    token_saved: bool,
    /// Whether Windows starts this copy at sign-in, or why that can't be told. Read when Settings is shown, and only
    /// then: the registry is not read for any other page.
    startup: Option<Result<Startup, String>>,
    /// The control that had the focus when the window was last deactivated.
    focus: HWND,
    /// The Sync now button that has its tooltip.
    tipped: HWND,
}

thread_local! {
    static WINDOW: Cell<HWND> = const { Cell::new(null_mut()) };
    static STATE: RefCell<Option<Win>> = const { RefCell::new(None) };
    static REFRESH_POSTED: Cell<bool> = const { Cell::new(false) };
    /// What the window draws behind the panels; kept apart from the state, so painting never needs to borrow it.
    static DIFF_MODE: Cell<bool> = const { Cell::new(false) };
    static SAVED_SIZE: Cell<WindowSize> = const { Cell::new(WindowSize::DEFAULT) };
    /// A preview of the new width while the mouse is held; never written until capture ends.
    static DRAG_WIDTH: Cell<Option<i32>> = const { Cell::new(None) };
}

pub fn window() -> HWND {
    WINDOW.with(Cell::get)
}

pub fn is_open() -> bool {
    !window().is_null()
}

pub fn offer_startup_once() {
    let hwnd = window();
    if hwnd.is_null() || !shell::snapshot().is_some_and(|snapshot| !snapshot.cfg.repos().is_empty()) {
        return;
    }
    offer_startup(hwnd);
}

/// Runs `f` on the state, unless the window is closed or the state is in use further up the stack.
fn with_state<R>(f: impl FnOnce(&mut Win) -> R) -> Option<R> {
    STATE.with(|s| {
        let mut state = s.try_borrow_mut().ok()?;
        state.as_mut().map(f)
    })
}

fn dpi_of(hwnd: HWND) -> u32 {
    match unsafe { GetDpiForWindow(hwnd) } {
        0 => unsafe { GetDpiForSystem() },
        dpi => dpi,
    }
}

/// Gives the window the logo drawn at the exact sizes of its big and small icons at `dpi`, and returns them; the
/// caller destroys them once they are replaced or the window is gone.
fn set_icons(hwnd: HWND, dpi: u32) -> [HICON; 2] {
    let icons = [SM_CXICON, SM_CXSMICON].map(|metric| tray::icon(logo::Look::Solid(logo::BLUE), unsafe { GetSystemMetricsForDpi(metric, dpi) }));
    unsafe {
        SendMessageW(hwnd, WM_SETICON, ICON_BIG as usize, icons[0] as isize);
        SendMessageW(hwnd, WM_SETICON, ICON_SMALL as usize, icons[1] as isize);
    }
    icons
}

fn dips(px: i32, dpi: u32) -> i32 {
    px * 96 / dpi as i32
}

/// The client area in DIPs.
fn client(hwnd: HWND) -> (i32, i32) {
    let dpi = dpi_of(hwnd);
    let mut rc: RECT = unsafe { zeroed() };
    unsafe { GetClientRect(hwnd, &mut rc) };
    (dips(rc.right, dpi), dips(rc.bottom, dpi))
}

fn size_file() -> PathBuf {
    Paths::home_dir().join(SIZE_FILE)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WindowSize {
    client: (i32, i32),
    list: i32,
}

impl WindowSize {
    const DEFAULT: Self = Self { client: SIZE, list: NAV_W };
}

fn parse_size(text: &str) -> Option<WindowSize> {
    let mut parts = text.split_whitespace();
    let w = parts.next()?.parse::<i32>().ok()?.clamp(MIN_SIZE.0, 10_000);
    let h = parts.next()?.parse::<i32>().ok()?.clamp(MIN_SIZE.1, 10_000);
    let list = parts.next().and_then(|n| n.parse().ok()).filter(|&n| n >= MIN_LIST_W && parts.next().is_none()).unwrap_or(NAV_W);
    Some(WindowSize { client: (w, h), list })
}

fn format_size(WindowSize { client: (w, h), list }: WindowSize) -> String {
    format!("{w} {h} {list}\n")
}

/// A client size that fits on the work area, frame and all.
fn within_work_area((w, h): (i32, i32), dpi: u32) -> (i32, i32) {
    let mut work: RECT = unsafe { zeroed() };
    let mut frame = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    unsafe {
        SystemParametersInfoW(SPI_GETWORKAREA, 0, (&raw mut work).cast(), 0);
        AdjustWindowRectExForDpi(&mut frame, STYLE, 0, EX_STYLE, dpi);
    }
    let room_w = dips(work.right - work.left - (frame.right - frame.left), dpi);
    let room_h = dips(work.bottom - work.top - (frame.bottom - frame.top), dpi);
    (w.min(room_w.max(MIN_SIZE.0)), h.min(room_h.max(MIN_SIZE.1)))
}

fn save_size(size: WindowSize) {
    SAVED_SIZE.with(|s| s.set(size));
    let file = size_file();
    let text = format_size(size);
    if fs::read_to_string(&file).ok().as_deref() != Some(text.as_str()) {
        let _ = fs::write(&file, text);
    }
}

/// Don't replace the normal size with the maximized or minimized size.
fn remember_window_size(hwnd: HWND) {
    if unsafe { IsZoomed(hwnd) == 0 && IsIconic(hwnd) == 0 } {
        save_size(WindowSize { client: client(hwnd), ..SAVED_SIZE.with(Cell::get) });
    }
}

fn requested_list_width() -> i32 {
    DRAG_WIDTH.with(Cell::get).unwrap_or_else(|| SAVED_SIZE.with(Cell::get).list)
}

fn remember_list_width(list: i32) {
    save_size(WindowSize { list, ..SAVED_SIZE.with(Cell::get) });
}

/// Opens the window, or brings it to the front.
pub fn open() {
    let hwnd = window();
    if !hwnd.is_null() {
        unsafe {
            if IsIconic(hwnd) != 0 {
                ShowWindow(hwnd, SW_RESTORE);
            }
            SetForegroundWindow(hwnd);
        }
        return;
    }
    if !register_class(CLASS, Some(proc), null_mut()) || !control::register() {
        return;
    }
    let opened = Instant::now();
    theme::refresh();
    let wanted = fs::read_to_string(size_file()).ok().and_then(|t| parse_size(&t)).unwrap_or(WindowSize::DEFAULT);
    let system = unsafe { GetDpiForSystem() };
    let [x, y, w, h] = place(STYLE, EX_STYLE, within_work_area(wanted.client, system), system, null_mut());
    let hwnd = unsafe {
        CreateWindowExW(EX_STYLE, wide(CLASS).as_ptr(), wide(APP_NAME).as_ptr(), STYLE, x, y, w, h, null_mut(), null_mut(), GetModuleHandleW(null()), null())
    };
    if hwnd.is_null() {
        return;
    }
    theme::frame(hwnd);
    theme::menus();
    let dpi = dpi_of(hwnd);
    let p = Panels {
        hero: control::panel(hwnd, HERO, Role::None),
        nav: control::panel(hwnd, NAV, Role::None),
        head: control::panel(hwnd, HEAD, Role::None),
        files: control::panel(hwnd, FILES, Role::None),
        splitter: control::panel(hwnd, SPLITTER, Role::None),
        page: control::panel(hwnd, PAGE, Role::None),
        code: control::panel(hwnd, CODE, Role::None),
    };
    for panel in [p.head, p.files, p.code] {
        unsafe { ShowWindow(panel, SW_HIDE) };
    }
    let icons = set_icons(hwnd, dpi);
    let state = Win {
        p,
        icons,
        snap: None,
        page: None,
        add_form: None,
        adding: None,
        retry_add_form: None,
        toggled: HashSet::new(),
        diff: None,
        from_card: false,
        token_saved: cred::has_token(),
        startup: None,
        focus: null_mut(),
        tipped: null_mut(),
    };
    STATE.with(|s| *s.borrow_mut() = Some(state));
    WINDOW.with(|w| w.set(hwnd));
    REFRESH_POSTED.with(|p| p.set(false));
    DIFF_MODE.with(|d| d.set(false));
    SAVED_SIZE.with(|s| s.set(wanted));
    shell::reload_settings();
    // Now that the window knows its monitor's DPI.
    let [x, y, w, h] = place(STYLE, EX_STYLE, within_work_area(wanted.client, dpi), dpi, null_mut());
    unsafe { SetWindowPos(hwnd, null_mut(), x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE) };
    update(hwnd);
    control::hide_cues(hwnd);
    // The repository shown, in the list, or Add repository when there is none.
    let first = with_state(|st| {
        let index = match &st.page {
            Some(Page::Repo(shown)) => st.snap.as_ref().and_then(|s| s.sources.iter().position(|v| v.id == *shown)),
            _ => None,
        };
        control::child(st.p.nav, index.map_or(id::NAV_ADD, |i| id::NAV_REPO + i as i32))
    })
    .unwrap_or(null_mut());
    unsafe {
        SetTimer(hwnd, TIMER_REFRESH, REFRESH_MS, None);
        ShowWindow(hwnd, SW_SHOWNORMAL);
        SetForegroundWindow(hwnd);
        if !first.is_null() {
            SetFocus(first);
        }
    }
    if bench::on() {
        // Paint now rather than when the queue is empty, so the mark is taken once the window can be read.
        unsafe { RedrawWindow(hwnd, null(), null_mut(), RDW_UPDATENOW | RDW_ALLCHILDREN) };
        bench::mark("settings", Some(opened.elapsed()));
    }
}

pub fn close() {
    let hwnd = window();
    if !hwnd.is_null() {
        unsafe { DestroyWindow(hwnd) };
    }
}

/// Asks for the window to show what is true now, once, however often it is asked before it does.
pub fn refresh() {
    let hwnd = window();
    if !hwnd.is_null() && !REFRESH_POSTED.with(|p| p.replace(true)) && unsafe { PostMessageW(hwnd, WM_REFRESH, 0, 0) } == 0 {
        REFRESH_POSTED.with(|p| p.set(false));
    }
}

/// Takes in what is true now, and shows it.
fn update(hwnd: HWND) {
    REFRESH_POSTED.with(|p| p.set(false));
    let snap = shell::snapshot();
    let shown = shell::take_diff();
    let moved = with_state(|st| {
        if let Some(shown) = shown {
            let waiting =
                st.diff.as_mut().filter(|d| d.source.id == shown.repo && d.skill == shown.skill && d.preview == shown.preview && d.report == Report::Loading);
            if let Some(d) = waiting {
                d.report = match shown.result {
                    Ok(report) => Report::Ready(report),
                    Err(why) => Report::Failed(why),
                };
            }
        }
        let snap = snap?;
        if snap.draft.is_none() {
            st.add_form = None;
            if st.snap.as_ref().is_some_and(|s| s.draft.is_some()) {
                st.retry_add_form = None;
            }
        }
        if st.diff.as_ref().is_some_and(|d| snap.cfg.find(&d.source.id) != Some(&d.source)) {
            st.diff = None;
        }
        let wanted = if st.page == Some(Page::Add) && snap.draft.is_none() {
            st.snap.as_ref().and_then(|s| s.draft.as_ref()).and(st.adding.take()).map(Page::Repo)
        } else {
            st.page.clone()
        };
        let page = page::settle(wanted, &snap.sources, &snap.cfg);
        let moved = page != st.page;
        if moved {
            st.page = page;
            st.toggled.clear();
        } else if let Some(Page::Repo(id)) = &page {
            if !st.toggled.is_empty() {
                let mut previous = std::mem::take(&mut st.toggled);
                if let Some(view) = snap.sources.iter().find(|view| view.id == *id) {
                    let rows = view.kinds.iter().flat_map(|k| k.rows.iter().map(move |r| (k, r)));
                    st.toggled.extend(rows.filter(|(_, r)| page::expandable(r)).filter_map(|(k, r)| previous.take(&page::toggle_key(&k.lane, &r.name))));
                }
            }
        }
        st.snap = Some(snap);
        moved.then_some(st.p.page)
    })
    .flatten();
    if let Some(panel) = moved {
        control::scroll_top(panel);
    }
    render(hwnd);
}

/// What each panel shows and where, or `None` to hide it.
type Plan = Vec<(HWND, Option<(Rect, Surface)>)>;

fn lay_out(st: &Win, preferred: i32, w: i32, h: i32, bar: i32, fit: Fit) -> Plan {
    let p = st.p;
    let Some(snap) = &st.snap else { return Vec::new() };
    // Laid out again without the scroll bar's width when it does not fit.
    let fitted = |at: Rect, make: &dyn Fn(i32) -> Surface| {
        if at[2] == 0 || at[3] == 0 {
            return None;
        }
        let surface = make(at[2]);
        let surface = if surface.height > at[3] { make((at[2] - bar).max(0)) } else { surface };
        Some((at, surface))
    };
    let splitter = |at: Rect| (at[2] > 0 && at[3] > 0).then(|| (at, page::splitter(at[2], at[3])));
    if let Some(d) = &st.diff {
        let l = page::diff_layout(w, h, preferred);
        return vec![
            (p.hero, None),
            (p.nav, None),
            (p.page, None),
            (p.head, Some((l.head, page::diff_head(d, &l, fit)))),
            (p.files, fitted(l.split.list, &|w| page::diff_files(d, w, fit))),
            (p.splitter, splitter(l.split.splitter)),
            (p.code, fitted(l.split.page, &|w| page::diff_code(d, w, fit))),
        ];
    }
    let body = (h - HERO_H).max(0);
    let l = page::split_layout(w, HERO_H, body, preferred);
    let hero = page::hero_of(&snap.sources, snap.problem.as_deref(), &snap.status, snap.news, snap.now);
    let can_sync = snap.cfg.repos().iter().any(|r| r.enabled);
    let welcome = Page::Repo(String::new());
    let shown = st.page.as_ref().unwrap_or(&welcome);
    vec![
        (p.hero, Some(([0, 0, w, HERO_H], page::hero(&hero, can_sync, w, fit)))),
        (p.nav, fitted(l.list, &|w| page::nav(&snap.sources, shown, w, body, fit))),
        (p.splitter, splitter(l.splitter)),
        (p.page, fitted(l.page, &|w| page_of(st, snap, w, fit))),
        (p.head, None),
        (p.files, None),
        (p.code, None),
    ]
}

fn page_of(st: &Win, snap: &Snapshot, w: i32, fit: Fit) -> Surface {
    let default = snap.cfg.default_interval;
    match &st.page {
        Some(Page::Repo(shown)) => match (snap.sources.iter().find(|v| v.id == *shown), snap.cfg.find_source(shown)) {
            (Some(view), Some(source)) => {
                let p = SourcePage { view, source, global: snap.cfg.global(), default, now: snap.now, toggled: &st.toggled };
                page::source_page(&p, w, fit)
            }
            _ => page::welcome(w, fit),
        },
        Some(Page::Activity) => page::activity_page(&snap.notes, &local_time, w, fit),
        Some(Page::Add) => snap.draft.as_ref().map_or_else(|| page::welcome(w, fit), |draft| page::source_preview(draft, &snap.skills_dir, w, fit)),
        Some(Page::Settings) => {
            let skills_dir = snap.skills_dir.display().to_string();
            let startup = match &st.startup {
                Some(Ok(state)) => Ok(state),
                Some(Err(e)) => Err(e.as_str()),
                None => Err("it was not read"),
            };
            let settings = SettingsPage {
                default,
                skills_dir: &skills_dir,
                token_saved: st.token_saved,
                startup,
                running_version: env!("CARGO_PKG_VERSION"),
                app_update: snap.cfg.app_update,
                app_update_status: &snap.app_update_status,
                global: snap.cfg.global(),
            };
            page::settings_page(&settings, w, fit)
        }
        None => page::welcome(w, fit),
    }
}

/// Lays everything out again for the window as it is now.
fn render(hwnd: HWND) {
    let dpi = dpi_of(hwnd);
    let (w, h) = client(hwnd);
    if w <= 0 || h <= 0 {
        return;
    }
    let fit = |t: &str, f: Font, width: i32| paint::measure(t, f, width, dpi);
    let bar = {
        let px = unsafe { GetSystemMetricsForDpi(SM_CXVSCROLL, dpi) };
        (px * 96 + dpi as i32 - 1) / dpi as i32
    };
    let Some((plan, diff, hero)) = with_state(|st| (lay_out(st, requested_list_width(), w, h, bar, &fit), st.diff.is_some(), st.p.hero)) else { return };
    let switched = DIFF_MODE.with(|d| d.replace(diff)) != diff;
    for (panel, part) in plan {
        match part {
            None => unsafe {
                if IsWindowVisible(panel) != 0 {
                    ShowWindow(panel, SW_HIDE);
                }
            },
            Some((at, surface)) => {
                let (x, y) = (scale(at[0], dpi), scale(at[1], dpi));
                let (cw, ch) = (scale(at[0] + at[2], dpi) - x, scale(at[1] + at[3], dpi) - y);
                unsafe { SetWindowPos(panel, null_mut(), x, y, cw, ch, SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW) };
                control::set(panel, surface);
            }
        }
    }
    if switched {
        invalidate_layout(hwnd);
    }
    let sync = control::child(hero, id::SYNC);
    if !sync.is_null() && with_state(|st| std::mem::replace(&mut st.tipped, sync)).is_some_and(|before| before != sync) {
        control::tip(sync, SYNC_TIP);
    }
}

fn invalidate_layout(hwnd: HWND) {
    unsafe { RedrawWindow(hwnd, null(), null_mut(), RDW_INVALIDATE | RDW_ALLCHILDREN) };
}

/// What the window draws behind its panels, into the canvas at its origin.
fn draw_bg(hwnd: HWND) {
    let (w, h) = client(hwnd);
    paint::fill([0, 0, w, h], 0, Role::Bg, Role::None);
    let preferred = requested_list_width();
    if DIFF_MODE.with(Cell::get) {
        control::draw(&page::diff_backdrop(&page::diff_layout(w, h, preferred), w, h));
    } else {
        let l = page::split_layout(w, HERO_H, (h - HERO_H).max(0), preferred);
        control::draw(&page::backdrop(l.page[0], w, h));
    }
}

fn finish_split_drag() {
    if let Some(list) = DRAG_WIDTH.with(Cell::take) {
        remember_list_width(list);
    }
}

fn move_split(hwnd: HWND, x: i32) {
    let (w, _) = client(hwnd);
    let dpi = dpi_of(hwnd) as i32;
    // Round to the nearest DIP, so grabbing a splitter on a scaled display does not jump by a DIP.
    let wanted = (x.max(0).saturating_mul(96).saturating_add(dpi / 2)) / dpi;
    let list = page::list_width(w, wanted);
    if list != page::list_width(w, requested_list_width()) {
        DRAG_WIDTH.with(|n| n.set(Some(list)));
        render(hwnd);
        invalidate_layout(hwnd);
    }
}

fn key_split(hwnd: HWND, key: SplitKey, shift: bool) {
    let (w, _) = client(hwnd);
    let preferred = requested_list_width();
    let list = page::width_after_key(w, preferred, key, shift);
    DRAG_WIDTH.with(Cell::take);
    remember_list_width(list);
    if list != page::list_width(w, preferred) {
        render(hwnd);
        invalidate_layout(hwnd);
    }
}

/// Shows another page on the right.
fn go(hwnd: HWND, page: Option<Page>) {
    if page != Some(Page::Add) {
        if !escape_cancels_source(shell::saving_source()) {
            return;
        }
        shell::dismiss_source();
    }
    // Read before the state is borrowed, and again each time: it may have been changed in Task Manager meanwhile.
    let startup = (page == Some(Page::Settings)).then(startup_now);
    let read = startup.is_some();
    let Some((changed, panel)) = with_state(|st| {
        let changed = st.page != page;
        if changed {
            st.page = page;
            st.toggled.clear();
        }
        if read {
            st.startup = startup;
        }
        (changed, st.p.page)
    }) else {
        return;
    };
    if changed {
        control::scroll_top(panel);
    }
    if changed || read {
        render(hwnd);
    }
}

/// Whether Windows starts this copy when the user signs in, as the registry says now.
fn startup_now() -> Result<Startup, String> {
    autorun::this_exe().and_then(|exe| autorun::state(&Keys::current(), &exe)).map_err(|e| e.to_string())
}

/// Runs `f` on the source whose page is shown: its settings and how it is.
fn shown_source<R>(f: impl FnOnce(&SourceConfig, &SourceView) -> R) -> Option<R> {
    with_state(|st| {
        let Some(Page::Repo(shown)) = &st.page else { return None };
        let snap = st.snap.as_ref()?;
        Some(f(snap.cfg.find_source(shown)?, snap.sources.iter().find(|v| v.id == *shown)?))
    })
    .flatten()
}

/// The row at `index` on the source's page: the lane it belongs to, whether its type syncs, and the row.
fn shown_row(index: usize) -> Option<(RepoConfig, bool, Row)> {
    shown_source(|s, v| page::row_at(v, index).map(|(k, row)| (page::lane_config(s, k.kind, &k.lane), k.on, row.clone()))).flatten()
}

/// Whether `lane` is still shown with the same settings, and `check` holds for its rows now.
fn lane_shown(lane: &RepoConfig, check: impl FnOnce(&[Row]) -> bool) -> bool {
    shown_source(|s, v| v.kinds.iter().find(|k| k.lane == lane.id).is_some_and(|k| page::lane_config(s, k.kind, &k.lane) == *lane && check(&k.rows)))
        == Some(true)
}

/// Whether the row named `name` of `lane` still offers `action`.
fn still_offers(lane: &RepoConfig, name: &str, action: Action) -> bool {
    lane_shown(lane, |rows| rows.iter().any(|row| row.name == name && row.actions.contains(&action)))
}

fn source_mut<'a>(cfg: &'a Config, shown: &SourceConfig) -> Result<&'a SourceConfig, String> {
    cfg.sources().iter().find(|s| *s == shown).ok_or_else(|| "The source changed. Choose it again.".to_string())
}

#[derive(Clone, Default)]
struct AddForm {
    address: String,
    every: Option<Interval>,
}

fn add_question(default: Interval, form: &AddForm) -> Ask {
    let (mut items, shown) = page::every_choices(form.every, Some(default));
    items.retain(|item| item != CUSTOM);
    Ask {
        heading: "Add source".to_string(),
        text: format!("{} Previewing a source does not save or install anything.", page::LOCATION_FORMS),
        field: Some(Field::Text {
            label: "GitHub repository, branch or folder".to_string(),
            hint: "owner/name, or the address of a repository, branch or folder on GitHub".to_string(),
            initial: form.address.clone(),
            check: Some(Box::new(|text, _| page::location_hint(text))),
            empty_ok: false,
        }),
        combo: Some(("Check every".to_string(), items, shown)),
        note: Some((
            glyph::LOCK,
            "Private repositories work automatically when you are signed in with the GitHub CLI or have saved a token in Settings.".to_string(),
        )),
        ok: "Preview".to_string(),
        ..Ask::default()
    }
}

fn add(hwnd: HWND, initial: AddForm) {
    if shell::saving_source() {
        return;
    }
    let Some(default) = with_state(|st| st.snap.as_ref().map(|s| s.cfg.default_interval)).flatten() else { return };
    let question = add_question(default, &initial);
    let Some(reply) = dialogs::ask(hwnd, question) else {
        update(hwnd);
        return;
    };
    if hwnd != window() {
        return;
    }
    let Ok(every) = page::parse_every(&reply.combo) else { return };
    preview_source(hwnd, AddForm { address: reply.text, every });
}

fn preview_source(hwnd: HWND, form: AddForm) {
    let retry_form = form.clone();
    with_state(|st| st.retry_add_form = Some(retry_form.clone()));
    match shell::preview_location(&form.address, form.every) {
        Ok(ticket) => {
            with_state(|st| st.add_form = Some((ticket, form)));
            go(hwnd, Some(Page::Add));
        }
        Err(e) => {
            shell::tell(sentence(&e));
        }
    }
    update(hwnd);
}

fn current_add_form() -> Option<AddForm> {
    with_state(|st| {
        if st.page != Some(Page::Add) {
            return None;
        }
        let draft = st.snap.as_ref()?.draft.as_ref()?;
        let (ticket, form) = st.add_form.as_ref()?;
        (*ticket == draft.input.ticket).then(|| form.clone())
    })
    .flatten()
}

/// Offers once per machine to start with Windows, when the first repository was just added.
fn offer_startup(hwnd: HWND) {
    match shell::startup_offer_seen() {
        Ok(true) => return,
        Ok(false) => {}
        Err(error) => {
            shell::tell(sentence(&error));
            return;
        }
    }
    if startup_now() != Ok(Startup::Off) {
        if let Err(error) = shell::mark_startup_offer_seen() {
            shell::tell(sentence(&error));
        }
        return;
    }
    let question = Ask {
        heading: "Start with Windows?".to_string(),
        text: "Tiny Skill Sync can start in the tray when you sign in, without opening its window, so your skills stay up to date. \
               It waits a little before it looks at your skills or checks GitHub, so it does not slow your sign-in down. \
               You can change this in Settings."
            .to_string(),
        ok: "Turn on".to_string(),
        cancel: "Not now".to_string(),
        ..Ask::default()
    };
    let accepted = dialogs::ask(hwnd, question).is_some();
    if let Err(error) = shell::mark_startup_offer_seen() {
        shell::tell(sentence(&error));
        return;
    }
    if accepted {
        let now = switch_startup(true);
        with_state(|st| st.startup = Some(now));
        refresh();
    }
}

/// Check every, of the repository shown or (`default`) in Settings: an entry was chosen.
fn every(hwnd: HWND, combo: HWND, default: bool) {
    let source = shown_source(|s, _| s.clone());
    let mut text = text_of(combo);
    if text == CUSTOM {
        match custom_interval(hwnd, default) {
            Some(typed) => text = typed,
            None => {
                // Shows what is set again, instead of "Custom…".
                render(hwnd);
                return;
            }
        }
    }
    if default {
        match Interval::parse(&text).and_then(Interval::validate_poll) {
            Ok(every) => {
                let _ = shell::edit_config(|cfg| {
                    let changed = cfg.default_interval != every;
                    cfg.default_interval = every;
                    Ok(changed)
                });
            }
            Err(e) => shell::tell(format!("Default check interval: {e}")),
        }
    } else if let Some(source) = source.filter(|s| shown_source(|now, _| now == s) == Some(true)) {
        match page::parse_every(&text) {
            Ok(every) => {
                let _ = shell::edit_config(|cfg| {
                    let changed = source_mut(cfg, &source)?.interval != every;
                    cfg.set_interval(&source.id, every);
                    Ok(changed)
                });
            }
            Err(e) => shell::tell(format!("Check every: {e}")),
        }
    }
    update(hwnd);
}

fn custom_interval(hwnd: HWND, default: bool) -> Option<String> {
    let check: Check = if default {
        Box::new(|text, _| Interval::parse(text).and_then(Interval::validate_poll).map(|i| format!("Every {}", i.describe())).map_err(|e| sentence(&e)))
    } else {
        Box::new(|text, _| match page::parse_every(text) {
            Ok(Some(i)) => Ok(format!("Every {}", i.describe())),
            Ok(None) => Ok("The default".to_string()),
            Err(e) => Err(sentence(&e)),
        })
    };
    let question = Ask {
        heading: "Check for updates every".to_string(),
        text: "Type how often, such as 45 minutes, 2 hours, 3 days or 2 weeks.".to_string(),
        field: Some(Field::Text {
            label: "Interval".to_string(),
            hint: format!("At least {MIN_POLL_MINUTES} minutes"),
            initial: String::new(),
            check: Some(check),
            empty_ok: false,
        }),
        ok: "Save".to_string(),
        ..Ask::default()
    };
    dialogs::ask(hwnd, question).map(|reply| reply.text)
}

/// Keep in sync was switched.
fn set_enabled(toggle: HWND) {
    let on = unsafe { SendMessageW(toggle, BM_GETCHECK, 0, 0) } != 0;
    let Some(source) = shown_source(|s, _| s.clone()) else { return };
    let _ = shell::edit_config(|cfg| {
        let changed = source_mut(cfg, &source)?.enabled != on;
        cfg.set_enabled(&source.id, on);
        Ok(changed)
    });
    refresh();
}

/// A "What to sync" choice on a source's page: a type, or repository-specific items.
fn source_choice(hwnd: HWND, n: i32, combo: HWND) {
    let Some(source) = shown_source(|s, _| s.id.clone()) else { return };
    if let Some(edit) = page::source_choice(n, control::combo_value(&text_of(combo))) {
        let _ = shell::edit_source(&source, |s| edit.apply(s));
    }
    update(hwnd);
}

/// Asks for a list of folder names, separated by commas. `None` when the user cancelled; an empty answer means none.
fn ask_folders(hwnd: HWND, heading: &str, text: &str, initial: &[String]) -> Option<String> {
    dialogs::ask(hwnd, folders_question(heading, text, initial)).map(|reply| reply.text)
}

fn folders_question(heading: &str, text: &str, initial: &[String]) -> Ask {
    let check: Check = Box::new(|text, _| match page::parse_folders(text) {
        Ok(names) if names.is_empty() => Ok("No folder names".to_string()),
        Ok(names) if names.len() == 1 => Ok("1 folder name".to_string()),
        Ok(names) => Ok(format!("{} folder names", names.len())),
        Err(why) => Err(why),
    });
    Ask {
        heading: heading.to_string(),
        text: text.to_string(),
        field: Some(Field::Text {
            label: "Folder names".to_string(),
            hint: "Separate names with commas. Leave it empty for none.".to_string(),
            initial: initial.join(", "),
            check: Some(check),
            empty_ok: true,
        }),
        ok: "Save".to_string(),
        ..Ask::default()
    }
}

/// Folder rules on a source's page: the folders it also skips and the ones it never skips.
fn folder_rules(hwnd: HWND) {
    let Some(source) = shown_source(|s, _| s.clone()) else { return };
    let also = "Folders with these names are skipped in this source, as well as the ignored folders in Settings.";
    let Some(add) = ask_folders(hwnd, "Also skip these folders", also, &source.ignore_add) else { return };
    let never = "Folders with these names are searched in this source, even when Settings ignores them.";
    let Some(allow) = ask_folders(hwnd, "Never skip these folders", never, &source.ignore_allow) else { return };
    match page::folder_rules(&add, &allow) {
        Ok(_) if shown_source(|s, _| *s == source) != Some(true) => shell::tell("The source changed. Choose it again.".to_string()),
        Ok(edit) => {
            let _ = shell::edit_source(&source.id, |s| edit.apply(s));
        }
        Err(why) => shell::tell(why),
    }
    update(hwnd);
}

/// A switch in the Sources group of Settings.
fn global_switch(hwnd: HWND, n: i32, toggle: HWND) {
    let on = unsafe { SendMessageW(toggle, BM_GETCHECK, 0, 0) } != 0;
    if let Some(edit) = page::global_switch(n, on) {
        let _ = shell::edit_global(|g| edit.apply(g));
    }
    update(hwnd);
}

/// Ignored folders, in Settings.
fn edit_ignored(hwnd: HWND) {
    let Some(names) = with_state(|st| st.snap.as_ref().map(|s| s.cfg.global().ignore_folders.clone())).flatten() else { return };
    let text = "Folders with these names are skipped in every source, unless a source says otherwise.";
    let Some(typed) = ask_folders(hwnd, "Ignored folders", text, &names) else { return };
    match page::parse_folders(&typed) {
        Ok(names) => {
            let _ = shell::edit_global(|g| page::GlobalEdit::Ignore(names).apply(g));
        }
        Err(why) => shell::tell(why),
    }
    update(hwnd);
}

fn set_app_update(change: impl FnOnce(&mut UpdatePrefs)) {
    let _ = shell::edit_app_update(|cfg| {
        let before = cfg.app_update;
        change(&mut cfg.app_update);
        Ok(cfg.app_update != before)
    });
    refresh();
}

/// Start with Windows was switched.
fn set_startup(toggle: HWND) {
    let on = unsafe { SendMessageW(toggle, BM_GETCHECK, 0, 0) } != 0;
    let now = switch_startup(on);
    with_state(|st| st.startup = Some(now));
    refresh();
}

/// Has Windows start this copy at sign-in, or stop starting it. Says so when that fails, and returns how it is now.
fn switch_startup(on: bool) -> Result<Startup, String> {
    let keys = Keys::current();
    let result = autorun::this_exe().and_then(|exe| if on { autorun::turn_on(&keys, &exe) } else { autorun::turn_off(&keys, &exe) });
    result.or_else(|e| {
        shell::tell(format!("Start with Windows could not be turned {}: {e}", if on { "on" } else { "off" }));
        startup_now()
    })
}

fn more(hwnd: HWND, button: HWND) {
    let Some(source) = shown_source(|s, _| s.clone()) else { return };
    let mut items = Vec::new();
    if source.enabled {
        items.push(SYNC_ONE);
    }
    items.push(ON_GITHUB);
    if source.lanes.id(SourceKind::Skills).is_some() {
        items.push(SKILLS_FOLDER);
    }
    items.extend(["-", REMOVE]);
    let Some(chosen) = control::menu(button, &items, None, true) else { return };
    if shown_source(|s, _| s == &source) != Some(true) {
        return;
    }
    match items[chosen] {
        SYNC_ONE => {
            shell::want(Job::Check(Some(source.id)));
        }
        ON_GITHUB => open_on_github(&page::lane_config(&source, SourceKind::Skills, &source.id)),
        SKILLS_FOLDER => open_skills_folder(),
        REMOVE => remove(hwnd, &source),
        _ => {}
    }
}

fn open_on_github(repo: &RepoConfig) {
    match web_url(repo, None) {
        Some(url) => open_url(&url),
        None => shell::tell(format!("{} can’t be opened on GitHub.", printable(&repo.repo))),
    }
}

fn open_url(url: &str) {
    if let Err(e) = proc::explorer(url.as_ref()) {
        shell::tell(format!("The browser could not be started: {e}"));
    }
}

fn remove(hwnd: HWND, source: &SourceConfig) {
    // Deleting is offered only for a skills lane that syncs: the others keep their files.
    let skills =
        with_state(|st| st.snap.as_ref().is_some_and(|s| source.lanes.id(SourceKind::Skills).is_some_and(|id| s.cfg.find(id).is_some()))).unwrap_or(false);
    let plugins = source.lanes.id(SourceKind::Plugins).is_some();
    let question = Ask {
        heading: format!("Remove {} [{}]?", printable(&source_label(source)), printable(&source.id)),
        text: format!(
            "Nothing from this source will be kept up to date any more.{}{}",
            if skills { "" } else { " Its files stay installed." },
            if plugins { " Its plugins stay installed in Copilot." } else { "" }
        ),
        choices: if skills {
            vec!["Leave its files installed; they become yours".into(), "Delete its skills too; skills you edited are kept".into()]
        } else {
            Vec::new()
        },
        ok: "Remove".to_string(),
        ..Ask::default()
    };
    if let Some(reply) = dialogs::ask(hwnd, question) {
        if shown_source(|s, _| s == source) == Some(true) {
            shell::want(Job::Remove { repo: source.id.clone(), delete_skills: skills && reply.choice == 1 });
        }
    }
}

fn row_command(hwnd: HWND, button: HWND, index: usize, code: u32) {
    let Some((repo, name, commands)) = shown_source(|s, v| {
        let (k, row) = page::row_at(v, index)?;
        let lane = page::lane_config(s, k.kind, &k.lane);
        let commands = page::kind_commands(row, k, web_url(&lane, None).is_some());
        Some((lane, row.name.clone(), commands))
    })
    .flatten() else {
        return;
    };
    let cmd = if code == control::ROW_MENU && !commands.is_empty() {
        let labels: Vec<_> = commands.iter().map(|cmd| cmd.label()).collect();
        let picked = control::menu(button, &labels, None, false).map(|i| commands[i]);
        if unsafe { GetFocus() } == button && lane_shown(&repo, |_| true) {
            focus_skill(hwnd, &repo.id, &name, false);
        }
        picked
    } else {
        RowCmd::from_code(code)
    };
    let Some(cmd) = cmd else { return };
    // The popup runs a message loop: a refresh can remove, reorder, or resolve this skill.
    let target = shown_source(|s, v| command_target(s, v, &repo, &name, cmd)).flatten();
    if let Some((index, part)) = target {
        skill(hwnd, index, part);
    }
}

/// The row, on the source's page as it is now, that a command chosen for the row `name` of `lane` goes to.
fn command_target(source: &SourceConfig, view: &SourceView, lane: &RepoConfig, name: &str, cmd: RowCmd) -> Option<(usize, Part)> {
    let k = view.kinds.iter().find(|k| k.lane == lane.id)?;
    if page::lane_config(source, k.kind, &k.lane) != *lane {
        return None;
    }
    let index = page::row_index(view, &lane.id, name)?;
    let row = k.rows.iter().find(|row| row.name == name)?;
    if !page::kind_commands(row, k, web_url(lane, None).is_some()).contains(&cmd) {
        return None;
    }
    let part = match cmd {
        RowCmd::Open => Part::Open,
        RowCmd::Preview => Part::Preview,
        RowCmd::CopyPath => Part::CopyPath,
        RowCmd::CopyLink => Part::CopyLink,
        RowCmd::OnGitHub => Part::OnGitHub,
        RowCmd::Compare => Part::Compare,
        RowCmd::ManageAgain => Part::Action(row.actions.iter().position(|a| *a == Action::ManageAgain)?),
    };
    Some((index, part))
}

/// A part of a skill's row was clicked.
fn skill(hwnd: HWND, index: usize, part: Part) {
    let Some((repo, on, row)) = shown_row(index) else { return };
    let label = format!("{}: {} [{}]", repo.kind.heading(), repo.label(), repo.id);
    match part {
        Part::Row => {
            if !page::expandable(&row) {
                return;
            }
            let key = page::toggle_key(&repo.id, &row.name);
            with_state(|st| {
                if !st.toggled.remove(&key) {
                    st.toggled.insert(key);
                }
            });
            render(hwnd);
        }
        // A type that does not sync only offers what reads the files already there.
        _ if !on && !matches!(part, Part::Open | Part::CopyPath | Part::CopyLink | Part::OnGitHub) => {}
        Part::Compare if repo.kind != SourceKind::Plugins => compare(hwnd, repo, row, false),
        Part::Open if repo.kind == SourceKind::Skills => open_skill(&row.name),
        Part::Action(k) => {
            if let Some(&action) = row.actions.get(k) {
                if let Some(choice) = row.plugin_choice(action) {
                    if action == Action::UninstallPlugin {
                        let question = Ask {
                            heading: format!("Uninstall {} from Copilot?", printable(&row.name)),
                            text: "GitHub no longer lists it, so this source cannot install it again.".into(),
                            ok: "Uninstall".into(),
                            ..Ask::default()
                        };
                        // A check may finish while the question is open. Recheck the source and choice by name.
                        if dialogs::ask(hwnd, question).is_none() || !still_offers(&repo, &row.name, action) {
                            return;
                        }
                    }
                    shell::want(Job::ResolvePlugin { repo: repo.id, choice });
                } else {
                    act(hwnd, &repo, &label, row.name, action);
                }
            }
        }
        Part::Preview if repo.kind != SourceKind::Plugins => compare(hwnd, repo, row, true),
        Part::CopyPath if repo.kind == SourceKind::Skills => {
            if let Some(dir) = skill_dir(&row.name) {
                copy(hwnd, dir.as_os_str());
            }
        }
        Part::CopyLink | Part::OnGitHub if row.listed => {
            let path = match repo.kind {
                SourceKind::Plugins => row.plugin.as_ref().and_then(|p| p.path.as_deref()),
                SourceKind::Skills => Some(row.name.as_str()),
                _ => None,
            };
            if let Some(path) = path {
                match web_url(&repo, Some(path)) {
                    Some(url) if part == Part::CopyLink => copy(hwnd, url.as_ref()),
                    Some(url) => open_url(&url),
                    None => shell::tell(format!("{} can’t be opened on GitHub.", printable(&row.name))),
                }
            }
        }
        _ => {}
    }
}

fn act(hwnd: HWND, repo: &RepoConfig, label: &str, skill: String, action: Action) {
    match action.resolution() {
        Some(how) => {
            shell::want(Job::Resolve { repo: repo.id.clone(), skill, how });
        }
        None if action == Action::RemoveThem && repo.kind != SourceKind::Plugins => {
            let question = Ask {
                heading: format!("Delete the {} that are gone from {}?", repo.kind.nouns().1, printable(label)),
                text: "It lists no matching items now, which can be a mistake upstream. Items you edited are kept.".to_string(),
                ok: "Delete them".to_string(),
                ..Ask::default()
            };
            if dialogs::ask(hwnd, question).is_some() && still_offers(repo, &skill, action) {
                shell::want(Job::DeleteGone(repo.id.clone()));
            }
        }
        None => {}
    }
}

/// Show changes, or a Preview: the window shows the differences, or the files, of one skill, read by the worker.
fn compare(hwnd: HWND, source: RepoConfig, row: Row, preview: bool) {
    let from_card = !preview && page::expandable(&row);
    let actions = if preview { Vec::new() } else { row.actions };
    let repo = source.id.clone();
    let view = DiffView { source, skill: row.name.clone(), preview, actions, report: Report::Loading, file: 0, gaps: Vec::new() };
    let Some(p) = with_state(|st| {
        st.diff = Some(view);
        st.from_card = from_card;
        st.p
    }) else {
        return;
    };
    shell::want(Job::Diff { repo, skill: row.name, preview });
    control::scroll_top(p.files);
    control::scroll_top(p.code);
    render(hwnd);
    let back = control::child(p.head, id::BACK);
    if !back.is_null() {
        unsafe { SetFocus(back) };
    }
}

fn leave_diff(hwnd: HWND) {
    let Some((d, from_card)) = with_state(|st| st.diff.take().map(|d| (d, st.from_card))).flatten() else { return };
    render(hwnd);
    focus_skill(hwnd, &d.source.id, &d.skill, from_card);
}

fn focus_skill(hwnd: HWND, repo: &str, name: &str, from_card: bool) {
    let Some(p) = with_state(|st| st.diff.is_none().then_some(st.p)).flatten() else { return };
    let index = shown_source(|_, v| page::row_index(v, repo, name)).flatten();
    let back = index
        .and_then(|i| [if from_card { 2 } else { 0 }, 0].into_iter().map(|k| control::child(p.page, page::skill_id(i, k))).find(|h| !h.is_null()))
        .unwrap_or_else(|| control::child(p.hero, id::SYNC));
    unsafe { SetFocus(if back.is_null() { hwnd } else { back }) };
}

fn show_file(hwnd: HWND, index: usize) {
    let code = with_state(|st| {
        let d = st.diff.as_mut().filter(|d| d.file != index)?;
        d.file = index;
        d.gaps.clear();
        Some(st.p.code)
    })
    .flatten();
    if let Some(code) = code {
        control::scroll_top(code);
        render(hwnd);
    }
}

fn open_gap(hwnd: HWND, row: usize) {
    let opened = with_state(|st| match st.diff.as_mut() {
        Some(d) if !d.gaps.contains(&row) => {
            d.gaps.push(row);
            true
        }
        _ => false,
    });
    if opened == Some(true) {
        render(hwnd);
    }
}

/// One of the choices in the diff view's header: it is made, and the window goes back to the repository.
fn diff_action(hwnd: HWND, index: usize) {
    let Some((repo, skill, action)) = with_state(|st| {
        let d = st.diff.as_ref().filter(|d| !d.preview && matches!(d.report, Report::Ready(_)))?;
        Some((d.source.clone(), d.skill.clone(), *d.actions.get(index)?))
    })
    .flatten() else {
        return;
    };
    leave_diff(hwnd);
    if still_offers(&repo, &skill, action) {
        if let Some(how) = action.resolution() {
            shell::want(Job::Resolve { repo: repo.id, skill, how });
        }
    }
}

fn save_token(hwnd: HWND) {
    let question = Ask {
        heading: "Save a GitHub token".to_string(),
        text: "It is kept in Windows Credential Manager and used automatically when a repository can’t be read without it. \
               Read access to contents is enough."
            .to_string(),
        field: Some(Field::Secret { label: "Token".to_string(), hint: "Paste a personal access token. It is hidden as you type.".to_string() }),
        ok: "Save".to_string(),
        ..Ask::default()
    };
    let Some(secret) = dialogs::ask(hwnd, question).and_then(|reply| reply.secret) else { return };
    let saved = cred::write_token(secret.expose());
    drop(secret);
    match saved {
        Ok(()) => {
            with_state(|st| st.token_saved = true);
            // Repositories that could not be read may be readable now.
            shell::want(Job::Check(None));
        }
        Err(e) => shell::tell(format!("The token could not be saved: {e}")),
    }
    refresh();
}

fn delete_token(hwnd: HWND) {
    let question = Ask {
        heading: "Delete your saved GitHub token?".to_string(),
        text: "Repositories that need one then use the GitHub CLI's sign-in, if there is one.".to_string(),
        ok: "Delete".to_string(),
        ..Ask::default()
    };
    if dialogs::ask(hwnd, question).is_none() {
        return;
    }
    match cred::delete_token() {
        Ok(_) => shell::tell("Your saved GitHub token is deleted.".to_string()),
        Err(e) => shell::tell(format!("The token could not be deleted: {e}")),
    }
    with_state(|st| st.token_saved = cred::has_token());
    refresh();
}

fn open_skills_folder() {
    let Some(dir) = with_state(|st| st.snap.as_ref().map(|s| s.skills_dir.clone())).flatten() else { return };
    if !dir.is_dir() {
        shell::tell("The skills folder is made when the first skill is installed.".to_string());
    } else if let Err(e) = open_folder(&dir) {
        shell::tell(e);
    }
}

fn open_backups() {
    let Some(work) = with_state(|st| st.snap.as_ref().map(|s| s.work_dir.clone())).flatten() else { return };
    let any = fs::read_dir(&work).is_ok_and(|entries| entries.flatten().any(|e| e.file_name().to_string_lossy().starts_with("backup-")));
    if !any {
        shell::tell("There are no backups yet.".to_string());
    } else if let Err(e) = open_folder(&work) {
        shell::tell(e);
    }
}

/// Open folder, of a skill.
fn open_skill(name: &str) {
    if let Some(path) = skill_dir(name) {
        if let Err(e) = open_folder(&path) {
            shell::tell(e);
        }
    }
}

/// A skill's folder, if it is there; if not, the status line says so.
fn skill_dir(name: &str) -> Option<PathBuf> {
    let dir = with_state(|st| st.snap.as_ref().map(|s| s.skills_dir.clone())).flatten()?;
    // Only a plain folder name: the name comes from upstream, and a rejected one is listed too.
    let plain = !name.is_empty() && name != "." && name != ".." && !name.contains(['\\', '/', ':']);
    let path = dir.join(name);
    if plain && path.is_dir() {
        Some(path)
    } else {
        shell::tell(format!("{} is not in the skills folder.", printable(name)));
        None
    }
}

/// Copy path and Copy GitHub link.
fn copy(hwnd: HWND, text: &std::ffi::OsStr) {
    match clip::set_text(hwnd, text) {
        Ok(()) => shell::tell(format!("Copied: {}", printable(&text.to_string_lossy()))),
        Err(e) => shell::tell(format!("The clipboard could not be used: {e}")),
    }
}

fn escape_cancels_source(saving: bool) -> bool {
    !saving
}

/// Escape: back from the diff view, cancel an unsaved preview, or close the window.
fn escape(hwnd: HWND) {
    if DIFF_MODE.with(Cell::get) {
        leave_diff(hwnd);
    } else if with_state(|st| st.page == Some(Page::Add)) == Some(true) {
        go(hwnd, None);
        update(hwnd);
    } else {
        unsafe { DestroyWindow(hwnd) };
    }
}

fn on_command(hwnd: HWND, wp: WPARAM, lp: LPARAM) {
    let ctl = lp as HWND;
    let code = ((wp >> 16) & 0xFFFF) as u32;
    // The id in wParam is cut to 16 bits; the control knows all of it.
    let n = if ctl.is_null() { (wp & 0xFFFF) as i32 } else { unsafe { GetDlgCtrlID(ctl) } };
    if n == IDCANCEL {
        if code == BN_CLICKED {
            escape(hwnd);
        }
        return;
    }
    match (page::hit(n), code) {
        (Hit::Other(id::EVERY), CBN_SELENDOK) => every(hwnd, ctl, false),
        (Hit::Other(id::DEFAULT_EVERY), CBN_SELENDOK) => every(hwnd, ctl, true),
        (Hit::Other(id::APP_EVERY), CBN_SELENDOK) => {
            if let Some(every) = page::selection(page::APP_INTERVALS, control::combo_value(&text_of(ctl))) {
                set_app_update(|prefs| prefs.interval = every);
            }
        }
        (Hit::Other(id::APP_WHEN), CBN_SELENDOK) => {
            if let Some(when) = page::selection(page::APP_TIMINGS, control::combo_value(&text_of(ctl))) {
                set_app_update(|prefs| prefs.timing = when);
            }
        }
        (Hit::Other(n), CBN_SELENDOK) if n == id::REPO_ITEMS || (id::SOURCE_TYPE..id::SOURCE_TYPE + 4).contains(&n) => source_choice(hwnd, n, ctl),
        (Hit::Skill(j, Part::Row), code) if code == control::ROW_MENU || RowCmd::from_code(code).is_some() => row_command(hwnd, ctl, j, code),
        (_, code) if code != BN_CLICKED => {}
        (Hit::Repo(i), _) => {
            if let Some(page) = with_state(|st| st.snap.as_ref()?.sources.get(i).map(|v| Page::Repo(v.id.clone()))).flatten() {
                go(hwnd, Some(page));
            }
        }
        (Hit::Skill(j, part), _) => skill(hwnd, j, part),
        (Hit::File(i), _) => show_file(hwnd, i),
        (Hit::Gap(r), _) => open_gap(hwnd, r),
        (Hit::DiffAction(k), _) => diff_action(hwnd, k),
        (Hit::AppUpdate(action), _) => {
            shell::want(Job::AppUpdate(action));
        }
        (Hit::Other(n), _) => match n {
            id::SYNC => {
                shell::want(Job::Check(None));
            }
            id::NAV_ADD | id::WELCOME_ADD => add(hwnd, with_state(|st| st.retry_add_form.clone().unwrap_or_default()).unwrap_or_default()),
            id::RETRY_SOURCE | id::EDIT_SOURCE => {
                if let Some(form) = current_add_form() {
                    if n == id::EDIT_SOURCE {
                        add(hwnd, form);
                    } else {
                        preview_source(hwnd, form);
                    }
                }
            }
            id::CONFIRM_SOURCE | id::APPROVE_SOURCE => {
                let reviewed = with_state(|st| {
                    let draft = st.snap.as_ref()?.draft.as_ref().filter(|_| st.page == Some(Page::Add))?;
                    let DraftStage::Reviewed(preview) = &draft.stage else { return None };
                    let lanes = &preview.source.lanes;
                    let lane = [SourceKind::Skills, SourceKind::Agents, SourceKind::Instructions, SourceKind::Plugins].into_iter().find_map(|k| lanes.id(k));
                    Some((draft.input.ticket, lane.unwrap_or(&preview.source.id).to_owned()))
                })
                .flatten();
                if let Some((ticket, lane)) = reviewed {
                    with_state(|st| st.adding = Some(lane));
                    shell::confirm_source(ticket, n == id::APPROVE_SOURCE);
                    update(hwnd);
                }
            }
            id::CANCEL_SOURCE => {
                go(hwnd, None);
                update(hwnd);
            }
            id::NAV_ACTIVITY => go(hwnd, Some(Page::Activity)),
            id::NAV_SETTINGS | id::OPEN_SETTINGS => go(hwnd, Some(Page::Settings)),
            id::MORE => more(hwnd, ctl),
            id::ENABLED => set_enabled(ctl),
            id::APP_AUTO => {
                let on = unsafe { SendMessageW(ctl, BM_GETCHECK, 0, 0) } != 0;
                set_app_update(|prefs| prefs.enabled = on);
            }
            id::STARTUP => set_startup(ctl),
            id::FOLDER_RULES => folder_rules(hwnd),
            id::IGNORED_EDIT => edit_ignored(hwnd),
            n if n == id::GLOBAL_REPO_ITEMS || (id::GLOBAL_TYPE..id::GLOBAL_TYPE + 4).contains(&n) => global_switch(hwnd, n, ctl),
            id::RETRY => {
                if let Some(source) = shown_source(|s, _| s.id.clone()) {
                    shell::want(Job::Check(Some(source)));
                }
            }
            id::OPEN_SKILLS => open_skills_folder(),
            id::OPEN_BACKUPS => open_backups(),
            id::SAVE_TOKEN => save_token(hwnd),
            id::DELETE_TOKEN => delete_token(hwnd),
            id::BACK => leave_diff(hwnd),
            _ => {}
        },
    }
}

/// Without a dialog manager, activation would put the focus on the window itself; this gives it back to the control
/// that had it.
fn on_activate(hwnd: HWND, active: bool) -> bool {
    if !active {
        let focus = unsafe { GetFocus() };
        if !focus.is_null() && unsafe { IsChild(hwnd, focus) } != 0 {
            with_state(|st| st.focus = focus);
        }
        return false;
    }
    // A token may have been saved or deleted with `tss` meanwhile, and Start with Windows changed in Task Manager.
    let saved = cred::has_token();
    let settings = with_state(|st| st.page == Some(Page::Settings)).unwrap_or(false);
    let startup = settings.then(startup_now);
    let focus = with_state(|st| {
        st.token_saved = saved;
        if startup.is_some() {
            st.startup = startup;
        }
        st.focus
    })
    .unwrap_or(null_mut());
    refresh();
    if !focus.is_null() && unsafe { IsWindow(focus) != 0 && IsWindowVisible(focus) != 0 } {
        unsafe { SetFocus(focus) };
        return true;
    }
    false
}

/// Windows switched between light and dark, or to or from high contrast.
fn on_theme(hwnd: HWND) {
    if !theme::refresh() {
        return;
    }
    theme::frame(hwnd);
    theme::menus();
    if let Some(p) = with_state(|st| st.p) {
        for panel in p.all() {
            control::scrollbar_theme(panel);
        }
    }
    unsafe { RedrawWindow(hwnd, null(), null_mut(), RDW_INVALIDATE | RDW_ERASE | RDW_FRAME | RDW_ALLCHILDREN) };
}

fn on_destroyed(hwnd: HWND) {
    shell::dismiss_source();
    unsafe { KillTimer(hwnd, TIMER_REFRESH) };
    finish_split_drag();
    let state = STATE.with(|s| s.try_borrow_mut().ok().and_then(|mut s| s.take()));
    WINDOW.with(|w| w.set(null_mut()));
    REFRESH_POSTED.with(|p| p.set(false));
    DIFF_MODE.with(|d| d.set(false));
    SAVED_SIZE.with(|s| s.set(WindowSize::DEFAULT));
    if let Some(state) = state {
        for icon in state.icons {
            unsafe { DestroyIcon(icon) };
        }
    }
    // A question still open uses the fonts.
    if dialogs::active().is_null() {
        paint::release();
    }
    if shell::is_idle() {
        trim_memory();
    }
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_REFRESH => update(hwnd),
        WM_COMMAND => on_command(hwnd, wp, lp),
        WM_TIMER if wp == TIMER_REFRESH => refresh(),
        control::WM_SPLIT_DRAG if wp != 0 => finish_split_drag(),
        control::WM_SPLIT_DRAG => move_split(hwnd, lp as i32),
        control::WM_SPLIT_KEY => {
            if let Some(key) = control::split_key(wp) {
                key_split(hwnd, key, lp != 0);
            }
        }
        WM_SIZE => {
            if wp as u32 != SIZE_MINIMIZED {
                render(hwnd);
            }
        }
        WM_EXITSIZEMOVE => remember_window_size(hwnd),
        WM_GETMINMAXINFO => {
            let dpi = dpi_of(hwnd);
            let mut rc = RECT { left: 0, top: 0, right: scale(MIN_SIZE.0, dpi), bottom: scale(MIN_SIZE.1, dpi) };
            unsafe { AdjustWindowRectExForDpi(&mut rc, STYLE, 0, EX_STYLE, dpi) };
            let info = unsafe { &mut *(lp as *mut MINMAXINFO) };
            info.ptMinTrackSize = POINT { x: rc.right - rc.left, y: rc.bottom - rc.top };
        }
        WM_PAINT => control::paint_with(hwnd, &|| draw_bg(hwnd)),
        control::WM_PAINTBG => draw_bg(hwnd),
        WM_ERASEBKGND => return 1,
        WM_ACTIVATE => {
            if on_activate(hwnd, (wp & 0xFFFF) as u32 != WA_INACTIVE) {
                return 0;
            }
            return unsafe { DefWindowProcW(hwnd, msg, wp, lp) };
        }
        WM_DPICHANGED => {
            let rc = unsafe { &*(lp as *const RECT) };
            unsafe { SetWindowPos(hwnd, null_mut(), rc.left, rc.top, rc.right - rc.left, rc.bottom - rc.top, SWP_NOZORDER | SWP_NOACTIVATE) };
            let icons = set_icons(hwnd, dpi_of(hwnd));
            let old = STATE.with(|s| s.try_borrow_mut().ok().and_then(|mut s| s.as_mut().map(|s| std::mem::replace(&mut s.icons, icons))));
            for icon in old.into_iter().flatten() {
                unsafe { DestroyIcon(icon) };
            }
            render(hwnd);
        }
        WM_SETTINGCHANGE | WM_THEMECHANGED | WM_SYSCOLORCHANGE => {
            on_theme(hwnd);
            return unsafe { DefWindowProcW(hwnd, msg, wp, lp) };
        }
        WM_CLOSE => unsafe {
            DestroyWindow(hwnd);
        },
        WM_NCDESTROY => {
            on_destroyed(hwnd);
            return unsafe { DefWindowProcW(hwnd, msg, wp, lp) };
        }
        _ => return unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_folder_list_is_taken_as_none_but_an_empty_location_is_refused() {
        let taken = |a: Ask, text: &str| match a.field {
            Some(Field::Text { check, empty_ok, .. }) => dialogs::accepts(text, empty_ok, check.as_deref(), 0),
            _ => panic!("a text field"),
        };
        let folders = || folders_question("Ignored folders", "", &["node_modules".to_string()]);
        assert!(taken(folders(), ""), "empty means no folders");
        assert!(taken(folders(), "a, b"));
        assert!(!taken(folders(), "a/b"), "a path is not a folder name");
        assert!(!taken(add_question(crate::config::DEFAULT_INTERVAL, &AddForm::default()), ""));
        let Some(Field::Text { check: Some(check), .. }) = folders().field else { panic!("the folder field checks names") };
        assert_eq!(check(",", 0), Ok("No folder names".to_string()));
    }

    #[test]
    fn add_source_question_fits_a_small_work_area() {
        let a = add_question(crate::config::DEFAULT_INTERVAL, &AddForm::default());
        let Some(Field::Text { label, hint, .. }) = a.field else { panic!("the Add source question needs a field") };
        assert!(a.choices.is_empty());
        let q = page::Question {
            heading: a.heading,
            text: a.text,
            field: Some((label, page::Hint::Plain(hint))),
            focused: true,
            combo: a.combo,
            choices: a.choices,
            choice: 0,
            note: a.note,
            ok: a.ok,
            cancel: a.cancel,
        };
        let (surface, _) = page::ask(&q, &page::tests::fit);
        let (work_height, frame_height) = (465, 34);
        let client_height = dialogs::fit_client_height(surface.height, work_height, frame_height);
        println!("Add source layout: {} DIP; visible client: {client_height} DIP", surface.height);
        assert!(client_height < surface.height, "this work area should need scrolling");
        assert_eq!(client_height + frame_height, work_height);
        for id in [page::ask_id::OK, page::ask_id::CANCEL] {
            let at = surface.items.iter().find(|i| i.id == id).unwrap().at;
            let scrolled_y = at[1] - (surface.height - client_height);
            assert!(scrolled_y >= 0 && scrolled_y + at[3] <= client_height, "scrolling to the bottom reveals the footer");
        }
    }

    #[test]
    fn add_source_field_takes_a_location_without_a_type() {
        let q = add_question(crate::config::DEFAULT_INTERVAL, &AddForm::default());
        let Some(Field::Text { check: Some(check), .. }) = q.field else { panic!("the Add source field should validate addresses") };
        assert!(check("https://github.com/o/r/tree/caf\u{e9}/skills", 0).is_ok());
        assert!(check("o/r", 0).is_ok());
        assert!(check("https://github.com/o/r/blob/main/SKILL.md", 0).is_err(), "a file address is not a location");
        assert!(check("https://gitlab.com/o/r", 0).is_err());
    }

    #[test]
    fn continuation_edit_preselects_the_drafts_address_and_interval() {
        let address = "https://github.com/team/tools/tree/main/custom-agents?from=link";
        let form = AddForm { address: address.into(), every: Some(Interval::parse("6h").unwrap()) };
        let question = add_question(Interval::parse("1d").unwrap(), &form);
        assert!(matches!(question.field, Some(Field::Text { initial, .. }) if initial == form.address));
        assert!(question.choices.is_empty(), "the app finds every type itself");
        assert_eq!(page::parse_every(&question.combo.unwrap().2), Ok(form.every));
    }
    #[test]
    fn continuation_escape_leaves_an_in_progress_source_save_showing() {
        assert!(escape_cancels_source(false));
        assert!(!escape_cancels_source(true), "Escape would dismiss a save that is still in progress");
    }

    #[test]
    fn row_menu_dispatch_follows_identity_and_rechecks_availability() {
        let mut cfg = Config::default();
        cfg.edit(|s, _| {
            let lanes = crate::sources::Lanes::default().with(SourceKind::Skills, "one".into()).with(SourceKind::Agents, "one-agents".into());
            s.push(SourceConfig { id: "one".into(), repo: "owner/repo".into(), lanes, ..Default::default() });
        });
        let mut board = crate::board::Board::new();
        board.note_kept("one", &["alpha".into(), "beta".into()]);
        board.note_kept("one-agents", &["alpha".into()]);
        let mut view = board.source_views(&cfg, 0).remove(0);
        assert_eq!(view.kinds.iter().map(|k| k.kind).collect::<Vec<_>>(), [SourceKind::Skills, SourceKind::Agents]);
        view.kinds[0].rows.swap(0, 1);
        let source = cfg.find_source("one").unwrap().clone();
        let skills = cfg.find("one").unwrap().clone();
        let agents = cfg.find("one-agents").unwrap().clone();
        let resolve = |view: &SourceView, lane: &RepoConfig| command_target(&source, view, lane, "alpha", RowCmd::ManageAgain);
        assert_eq!(resolve(&view, &skills), Some((1, Part::Action(0))));
        assert_eq!(resolve(&view, &agents), Some((2, Part::Action(0))), "an agent row is numbered after the skills");
        assert_eq!(resolve(&view, &RepoConfig { id: "other".into(), ..skills.clone() }), None);
        assert_eq!(resolve(&view, &RepoConfig { kind: SourceKind::Agents, ..skills.clone() }), None, "a repurposed lane");
        let mut short = view.clone();
        short.kinds[0].rows.truncate(1);
        assert_eq!(resolve(&short, &skills), None);
        view.kinds[0].rows[1].actions.clear();
        assert_eq!(resolve(&view, &skills), None);
    }

    #[test]
    fn window_txt_loads_legacy_and_splitter_widths() {
        let client = (1200, 800);
        assert_eq!(parse_size("1200 800\n"), Some(WindowSize { client, list: NAV_W }));
        let size = WindowSize { client, list: 328 };
        assert_eq!(parse_size(" 1200 800\t328\r\n"), Some(size));
        assert_eq!(format_size(size), "1200 800 328\n");
        assert_eq!(parse_size(&format_size(size)), Some(size));
        assert_eq!(parse_size("100 50"), Some(WindowSize { client: MIN_SIZE, list: NAV_W }));
        assert_eq!(parse_size("640 500 700"), Some(WindowSize { client: (640, 500), list: 700 }), "keep a preference larger than the current bounds");
    }

    #[test]
    fn window_txt_defaults_a_bad_list_width_without_losing_the_window_size() {
        for third in ["not-a-number", "0", "-160", "159", "2147483648", "250 trailing"] {
            assert_eq!(parse_size(&format!("1200 800 {third}")), Some(WindowSize { client: (1200, 800), list: NAV_W }), "{third:?}");
        }
        for invalid in ["1200", "wide tall", "", "1200 tall 320"] {
            assert_eq!(parse_size(invalid), None);
        }
    }
}
