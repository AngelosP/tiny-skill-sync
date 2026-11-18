//! The main window: a band across the top that says how everything is, the repositories down the left, and on the
//! right the page chosen there (a repository and its skills, Activity or Settings). "Show changes" turns the whole
//! window into the differences of one skill until Back. Every change is saved to `config.json` at once; there is no OK
//! button.
//!
//! What it shows comes from [`shell::snapshot`], on a posted `WM_REFRESH`; [`page`] lays it out and [`control`] draws
//! it. Handlers read what they need from the state, let go of it, and then act, so a question (which runs a message
//! loop) or a notification sent back from inside a change never finds the state borrowed by them.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::fs;
use std::mem::zeroed;
use std::path::PathBuf;
use std::process::Command;
use std::ptr::{null, null_mut};
use std::time::Instant;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{RDW_ALLCHILDREN, RDW_ERASE, RDW_FRAME, RDW_INVALIDATE, RDW_UPDATENOW, RedrawWindow};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForSystem, GetDpiForWindow, GetSystemMetricsForDpi};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetFocus, SetFocus};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BM_GETCHECK, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyWindow, GetClientRect, GetCursorPos, GetDlgCtrlID, HICON, ICON_BIG, ICON_SMALL, IDCANCEL,
    IsChild, IsIconic, IsWindow, IsWindowVisible, IsZoomed, KillTimer, MINMAXINFO, PostMessageW, SIZE_MINIMIZED, SM_CXICON, SM_CXSMICON, SM_CXVSCROLL,
    SPI_GETWORKAREA, SW_HIDE, SW_RESTORE, SW_SHOWNORMAL, SWP_NOACTIVATE, SWP_NOZORDER, SWP_SHOWWINDOW, SendMessageW, SetForegroundWindow, SetTimer,
    SetWindowPos, ShowWindow, SystemParametersInfoW, WA_INACTIVE, WM_ACTIVATE, WM_APP, WM_CLOSE, WM_COMMAND, WM_DPICHANGED, WM_ERASEBKGND, WM_EXITSIZEMOVE,
    WM_GETMINMAXINFO, WM_NCDESTROY, WM_PAINT, WM_SETICON, WM_SETTINGCHANGE, WM_SIZE, WM_SYSCOLORCHANGE, WM_THEMECHANGED, WM_TIMER, WS_CLIPCHILDREN,
    WS_EX_CONTROLPARENT, WS_OVERLAPPEDWINDOW, WindowFromPoint,
};

use super::control::{self, BN_CLICKED, CBN_SELENDOK, Surface};
use super::dialogs::{self, Ask, Check, Field};
use super::page::{self, CUSTOM, DIFF_FILES_W, DiffView, Fit, HERO_H, Hit, NAV_W, Page, Part, RepoPage, Report, SYNC_TIP, SettingsPage, id};
use super::paint::{self, Font, Rect, glyph};
use super::shell::{self, Snapshot};
use super::theme::{self, Role};
use super::{bench, local_time, open_folder, place, register_class, scale, sentence, text_of, tray, trim_memory};
use crate::app::Job;
use crate::autostart::{Keys, Startup};
use crate::board::{APP_NAME, Action, Health, RepoView, Row};
use crate::config::{Config, Paths, RepoConfig, RepoSpec};
use crate::interval::{Interval, MIN_POLL_MINUTES};
use crate::pathsafe::printable;
use crate::win::{autorun, cred, wide};

const CLASS: &str = "TinySkillSyncWindow";
const STYLE: u32 = WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN;
const EX_STYLE: u32 = WS_EX_CONTROLPARENT;
/// The client size the first time it opens, and the smallest it can be made, in DIPs.
const SIZE: (i32, i32) = (980, 680);
const MIN_SIZE: (i32, i32) = (640, 480);
/// Where the size the user last gave the window is kept, in the app's folder.
const SIZE_FILE: &str = "window.txt";
/// Show what the snapshot says now.
const WM_REFRESH: u32 = WM_APP + 10;
/// "5 minutes ago" goes stale; this keeps it roughly right.
const TIMER_REFRESH: usize = 1;
const REFRESH_MS: u32 = 60_000;
/// The list on the left widens once the mouse has rested on it this long, so passing over it does nothing.
const TIMER_LIST: usize = 2;
const LIST_REST_MS: u32 = 250;
/// The panels' ids. Not 1 or 2: Enter and Escape make the dialog manager look for a child with those.
const HERO: i32 = 101;
const NAV: i32 = 102;
const PAGE: i32 = 103;
const HEAD: i32 = 104;
const FILES: i32 = 105;
const CODE: i32 = 106;
/// The entries of a repository's More menu.
const SYNC_ONE: &str = "Sync this repository";
const ON_GITHUB: &str = "Open on GitHub";
const SKILLS_FOLDER: &str = "Open skills folder";
const REMOVE: &str = "Remove repository…";

#[derive(Clone, Copy)]
struct Panels {
    hero: HWND,
    nav: HWND,
    page: HWND,
    /// The diff view: its header, the files that are different, and the lines of one.
    head: HWND,
    files: HWND,
    code: HWND,
}

impl Panels {
    fn all(self) -> [HWND; 6] {
        [self.hero, self.nav, self.page, self.head, self.files, self.code]
    }
}

struct Win {
    p: Panels,
    icons: [HICON; 2],
    snap: Option<Snapshot>,
    /// The page on the right; `None` is the welcome page.
    page: Option<Page>,
    /// The skills of the repository shown that the user opened or closed, by name.
    toggled: HashSet<String>,
    diff: Option<DiffView>,
    /// The control that opened the diff view; it gets the focus back.
    diff_from: i32,
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
    /// How wide the list on the left is laid out, in DIPs; also for painting.
    static LIST_SHOWN: Cell<i32> = const { Cell::new(NAV_W) };
    /// The mouse is on the list on the left and the rest timer runs.
    static LIST_RESTING: Cell<bool> = const { Cell::new(false) };
    /// The mouse rested on the list, so it is as wide as its names need.
    static LIST_WIDE: Cell<bool> = const { Cell::new(false) };
}

pub fn window() -> HWND {
    WINDOW.with(Cell::get)
}

pub fn is_open() -> bool {
    !window().is_null()
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

/// What [`save_size`] wrote: the client width and height in DIPs, made at least the smallest.
fn parse_size(text: &str) -> Option<(i32, i32)> {
    let mut parts = text.split_whitespace().map(|p| p.parse::<i32>().ok());
    let (w, h) = (parts.next()??, parts.next()??);
    if parts.next().is_some() {
        return None;
    }
    Some((w.clamp(MIN_SIZE.0, 10_000), h.clamp(MIN_SIZE.1, 10_000)))
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

/// Remembers the size the user gave the window, unless it is maximized or minimized. Written only when it changed.
fn save_size(hwnd: HWND) {
    if unsafe { IsZoomed(hwnd) != 0 || IsIconic(hwnd) != 0 } {
        return;
    }
    let (w, h) = client(hwnd);
    let file = size_file();
    let text = format!("{w} {h}\n");
    if fs::read_to_string(&file).ok().as_deref() != Some(text.as_str()) {
        let _ = fs::write(&file, text);
    }
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
    let wanted = fs::read_to_string(size_file()).ok().and_then(|t| parse_size(&t)).unwrap_or(SIZE);
    let system = unsafe { GetDpiForSystem() };
    let [x, y, w, h] = place(STYLE, EX_STYLE, within_work_area(wanted, system), system, null_mut());
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
        page: control::panel(hwnd, PAGE, Role::None),
        head: control::panel(hwnd, HEAD, Role::None),
        files: control::panel(hwnd, FILES, Role::None),
        code: control::panel(hwnd, CODE, Role::None),
    };
    for panel in [p.head, p.files, p.code] {
        unsafe { ShowWindow(panel, SW_HIDE) };
    }
    let icons = [SM_CXICON, SM_CXSMICON].map(|metric| tray::icon(Health::Ok, unsafe { GetSystemMetricsForDpi(metric, dpi) }));
    unsafe {
        SendMessageW(hwnd, WM_SETICON, ICON_BIG as usize, icons[0] as isize);
        SendMessageW(hwnd, WM_SETICON, ICON_SMALL as usize, icons[1] as isize);
    }
    let state = Win {
        p,
        icons,
        snap: None,
        page: None,
        toggled: HashSet::new(),
        diff: None,
        diff_from: 0,
        token_saved: cred::has_token(),
        startup: None,
        focus: null_mut(),
        tipped: null_mut(),
    };
    STATE.with(|s| *s.borrow_mut() = Some(state));
    WINDOW.with(|w| w.set(hwnd));
    REFRESH_POSTED.with(|p| p.set(false));
    DIFF_MODE.with(|d| d.set(false));
    // Now that the window knows its monitor's DPI.
    let [x, y, w, h] = place(STYLE, EX_STYLE, within_work_area(wanted, dpi), dpi, null_mut());
    unsafe { SetWindowPos(hwnd, null_mut(), x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE) };
    update(hwnd);
    control::hide_cues(hwnd);
    // The repository shown, in the list, or Add repository when there is none.
    let first = with_state(|st| {
        let index = match &st.page {
            Some(Page::Repo(shown)) => st.snap.as_ref().and_then(|s| s.views.iter().position(|v| v.id == *shown)),
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
            let waiting = st.diff.as_mut().filter(|d| d.repo_id == shown.repo && d.skill == shown.skill && d.report == Report::Loading);
            if let Some(d) = waiting {
                d.report = match shown.result {
                    Ok(report) => Report::Ready(report),
                    Err(why) => Report::Failed(why),
                };
            }
        }
        let snap = snap?;
        let page = page::settle(st.page.clone(), &snap.views);
        let moved = page != st.page;
        if moved {
            st.page = page;
            st.toggled.clear();
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

fn lay_out(st: &Win, list_w: i32, w: i32, h: i32, bar: i32, fit: Fit) -> Plan {
    let p = st.p;
    let Some(snap) = &st.snap else { return Vec::new() };
    // Laid out again without the scroll bar's width when it does not fit.
    let fitted = |at: Rect, make: &dyn Fn(i32) -> Surface| {
        let surface = make(at[2]);
        let surface = if surface.height > at[3] { make((at[2] - bar).max(0)) } else { surface };
        Some((at, surface))
    };
    if let Some(d) = &st.diff {
        let l = page::diff_layout(w, h, list_w);
        return vec![
            (p.hero, None),
            (p.nav, None),
            (p.page, None),
            (p.head, Some((l.head, page::diff_head(d, &l, fit)))),
            (p.files, fitted(l.files, &|w| page::diff_files(d, w, fit))),
            (p.code, fitted(l.code, &|w| page::diff_code(d, w, fit))),
        ];
    }
    let body = (h - HERO_H).max(0);
    let hero = page::hero_of(&snap.views, snap.problem.as_deref(), &snap.status, snap.news, snap.now);
    let can_sync = snap.cfg.repos.iter().any(|r| r.enabled);
    let welcome = Page::Repo(String::new());
    let shown = st.page.as_ref().unwrap_or(&welcome);
    vec![
        (p.hero, Some(([0, 0, w, HERO_H], page::hero(&hero, can_sync, w, fit)))),
        (p.nav, fitted([0, HERO_H, list_w, body], &|w| page::nav(&snap.views, shown, w, body, fit))),
        (p.page, fitted([list_w, HERO_H, (w - list_w).max(0), body], &|w| page_of(st, snap, w, fit))),
        (p.head, None),
        (p.files, None),
        (p.code, None),
    ]
}

/// How wide the active list on the left is, including the card borders in the diff view.
fn list_width(st: &Win, w: i32, h: i32, bar: i32, fit: Fit) -> i32 {
    let wide = LIST_WIDE.with(Cell::get);
    if let Some(d) = &st.diff {
        if !wide {
            return DIFF_FILES_W;
        }
        let l = page::diff_layout(w, h, DIFF_FILES_W);
        let scrolls = page::diff_files(d, l.files[2], fit).height > l.files[3];
        return page::diff_files_width(d, w, if scrolls { bar } else { 0 }, fit);
    }
    let Some(snap) = st.snap.as_ref().filter(|_| wide) else { return NAV_W };
    let body = (h - HERO_H).max(0);
    let shown = st.page.clone().unwrap_or(Page::Repo(String::new()));
    let scrolls = page::nav(&snap.views, &shown, NAV_W, body, fit).height > body;
    page::nav_width(&snap.views, w, if scrolls { bar } else { 0 }, fit)
}

fn page_of(st: &Win, snap: &Snapshot, w: i32, fit: Fit) -> Surface {
    let default = snap.cfg.default_interval;
    match &st.page {
        Some(Page::Repo(shown)) => match (snap.views.iter().find(|v| v.id == *shown), snap.cfg.find(shown)) {
            (Some(view), Some(repo)) => page::repo_page(&RepoPage { view, repo, default, now: snap.now, toggled: &st.toggled }, w, fit),
            _ => page::welcome(w, fit),
        },
        Some(Page::Activity) => page::activity_page(&snap.notes, &local_time, w, fit),
        Some(Page::Settings) => {
            let skills_dir = snap.skills_dir.display().to_string();
            let startup = match &st.startup {
                Some(Ok(state)) => Ok(state),
                Some(Err(e)) => Err(e.as_str()),
                None => Err("it was not read"),
            };
            let settings = SettingsPage { default, skills_dir: &skills_dir, token_saved: st.token_saved, startup, version: env!("CARGO_PKG_VERSION") };
            page::settings_page(&settings, w, fit)
        }
        None => page::welcome(w, fit),
    }
}

/// Lays everything out again for the window as it is now.
fn render(hwnd: HWND) {
    lay_out_again(hwnd, false);
}

/// Lays everything out again; with `if_list_moves`, only when the list on the left is to change width.
fn lay_out_again(hwnd: HWND, if_list_moves: bool) {
    let started = Instant::now();
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
    let laid = with_state(|st| {
        let diff = st.diff.is_some();
        if diff != DIFF_MODE.with(Cell::get) {
            unsafe { KillTimer(hwnd, TIMER_LIST) };
            LIST_RESTING.with(|r| r.set(false));
            LIST_WIDE.with(|w| w.set(false));
        }
        let list_w = list_width(st, w, h, bar, &fit);
        let moves = list_w != LIST_SHOWN.with(Cell::get);
        (moves || !if_list_moves).then(|| (lay_out(st, list_w, w, h, bar, &fit), diff, st.p.hero, list_w))
    });
    let Some((plan, diff, hero, list_w)) = laid.flatten() else { return };
    let switched = DIFF_MODE.with(|d| d.replace(diff)) != diff;
    let widened = LIST_SHOWN.with(|n| n.replace(list_w)) != list_w;
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
    if switched || widened {
        unsafe { RedrawWindow(hwnd, null(), null_mut(), RDW_INVALIDATE | RDW_ALLCHILDREN) };
    }
    if if_list_moves && bench::on() {
        // As for "settings": paint now, so that the mark includes drawing the panels at their new widths.
        unsafe { RedrawWindow(hwnd, null(), null_mut(), RDW_UPDATENOW | RDW_ALLCHILDREN) };
        bench::mark(if diff { "files" } else { "nav" }, Some(started.elapsed()));
    }
    let sync = control::child(hero, id::SYNC);
    if !sync.is_null() && with_state(|st| std::mem::replace(&mut st.tipped, sync)).is_some_and(|before| before != sync) {
        control::tip(sync, SYNC_TIP);
    }
    if switched {
        on_hover(hwnd);
    }
}

/// What the window draws behind its panels, into the canvas at its origin.
fn draw_bg(hwnd: HWND) {
    let (w, h) = client(hwnd);
    paint::fill([0, 0, w, h], 0, Role::Bg, Role::None);
    if DIFF_MODE.with(Cell::get) {
        control::draw(&page::diff_backdrop(&page::diff_layout(w, h, LIST_SHOWN.with(Cell::get)), w, h));
    } else {
        control::draw(&page::backdrop(LIST_SHOWN.with(Cell::get), w, h));
    }
}

/// Is the mouse on `panel` or one of its controls, with nothing over them?
fn pointer_on(panel: HWND) -> bool {
    let mut pt = POINT { x: 0, y: 0 };
    if unsafe { GetCursorPos(&mut pt) } == 0 {
        return false;
    }
    let under = unsafe { WindowFromPoint(pt) };
    !under.is_null() && (under == panel || unsafe { IsChild(panel, under) } != 0)
}

/// The mouse came onto or left a part of the window. The list on the left widens once the mouse has rested on it for
/// [`LIST_REST_MS`], and narrows as soon as the mouse is anywhere else.
fn on_hover(hwnd: HWND) {
    let Some(list) = with_state(|st| if st.diff.is_some() { st.p.files } else { st.p.nav }) else { return };
    if pointer_on(list) {
        if !LIST_WIDE.with(Cell::get) && !LIST_RESTING.with(|r| r.replace(true)) {
            unsafe { SetTimer(hwnd, TIMER_LIST, LIST_REST_MS, None) };
        }
        return;
    }
    if LIST_RESTING.with(|r| r.replace(false)) {
        unsafe { KillTimer(hwnd, TIMER_LIST) };
    }
    if LIST_WIDE.with(|w| w.replace(false)) {
        lay_out_again(hwnd, true);
    }
}

fn on_rested(hwnd: HWND) {
    unsafe { KillTimer(hwnd, TIMER_LIST) };
    LIST_RESTING.with(|r| r.set(false));
    let Some(list) = with_state(|st| if st.diff.is_some() { st.p.files } else { st.p.nav }) else { return };
    if pointer_on(list) && !LIST_WIDE.with(|w| w.replace(true)) {
        lay_out_again(hwnd, true);
    }
}

/// Shows another page on the right.
fn go(hwnd: HWND, page: Option<Page>) {
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

/// Runs `f` on the repository whose page is shown: its settings and how it is.
fn shown_repo<R>(f: impl FnOnce(&RepoConfig, &RepoView) -> R) -> Option<R> {
    with_state(|st| {
        let Some(Page::Repo(shown)) = &st.page else { return None };
        let snap = st.snap.as_ref()?;
        Some(f(snap.cfg.find(shown)?, snap.views.iter().find(|v| v.id == *shown)?))
    })
    .flatten()
}

fn repo_mut<'a>(cfg: &'a mut Config, id: &str) -> Result<&'a mut RepoConfig, String> {
    cfg.repos.iter_mut().find(|r| r.id == id).ok_or_else(|| "that repository is no longer in the settings".to_string())
}

fn add(hwnd: HWND) {
    let Some((repos, default)) = with_state(|st| st.snap.as_ref().map(|s| (s.cfg.repos.clone(), s.cfg.default_interval))).flatten() else { return };
    let (mut items, shown) = page::every_choices(None, Some(default));
    items.retain(|item| item != CUSTOM);
    let check: Check = Box::new(move |text| page::spec_hint(text, &repos));
    let question = Ask {
        heading: "Add a repository".to_string(),
        text: "Its skills are installed in your skills folder and kept up to date.".to_string(),
        field: Some(Field::Text {
            label: "GitHub repository or folder".to_string(),
            hint: "owner/name, or the address of a folder on GitHub".to_string(),
            initial: String::new(),
            check: Some(check),
        }),
        combo: Some(("Check every".to_string(), items, shown)),
        note: Some((
            glyph::LOCK,
            "Private repositories work automatically when you are signed in with the GitHub CLI or have saved a token in Settings.".to_string(),
        )),
        ok: "Add".to_string(),
        ..Ask::default()
    };
    let Some(reply) = dialogs::ask(hwnd, question) else { return };
    let spec = match RepoSpec::parse(&reply.text) {
        Ok(spec) => spec,
        Err(e) => {
            shell::tell(sentence(&e));
            return;
        }
    };
    let every = page::parse_every(&reply.combo).unwrap_or(None);
    let mut added = None;
    let mut first = false;
    let result = shell::edit_config(|cfg| {
        let full_name = spec.full_name();
        if let Some(existing) = cfg.repos.iter().find(|r| r.repo.eq_ignore_ascii_case(&full_name) && r.branch == spec.branch && r.path == spec.path) {
            return Err(format!("{} is already set up", existing.label()));
        }
        first = cfg.repos.is_empty();
        added = Some(cfg.add_repo(&spec, every, false).id.clone());
        Ok(true)
    });
    let saved = matches!(result, Ok(true));
    if let (true, Some(id)) = (saved, added) {
        let panel = with_state(|st| {
            st.page = Some(Page::Repo(id.clone()));
            st.toggled.clear();
            st.p.page
        });
        if let Some(panel) = panel {
            control::scroll_top(panel);
        }
        shell::want(Job::Check(Some(id)));
    }
    update(hwnd);
    if saved && first {
        offer_startup(hwnd);
    }
}

/// Offers to start with Windows, when the first repository was just added and it does not yet. "Not now" is not
/// remembered: the offer comes again only when a first repository is added again.
fn offer_startup(hwnd: HWND) {
    if startup_now() != Ok(Startup::Off) {
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
    if dialogs::ask(hwnd, question).is_some() {
        let now = switch_startup(true);
        with_state(|st| st.startup = Some(now));
        refresh();
    }
}

/// Check every, of the repository shown or (`default`) in Settings: an entry was chosen.
fn every(hwnd: HWND, combo: HWND, default: bool) {
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
    } else if let Some(repo) = shown_repo(|r, _| r.id.clone()) {
        match page::parse_every(&text) {
            Ok(every) => {
                let _ = shell::edit_config(|cfg| {
                    let r = repo_mut(cfg, &repo)?;
                    let changed = r.interval != every;
                    r.interval = every;
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
        Box::new(|text| Interval::parse(text).and_then(Interval::validate_poll).map(|i| format!("Every {}", i.describe())).map_err(|e| sentence(&e)))
    } else {
        Box::new(|text| match page::parse_every(text) {
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
        }),
        ok: "Save".to_string(),
        ..Ask::default()
    };
    dialogs::ask(hwnd, question).map(|reply| reply.text)
}

/// Keep in sync was switched.
fn set_enabled(toggle: HWND) {
    let on = unsafe { SendMessageW(toggle, BM_GETCHECK, 0, 0) } != 0;
    let Some(repo) = shown_repo(|r, _| r.id.clone()) else { return };
    let result = shell::edit_config(|cfg| {
        let r = repo_mut(cfg, &repo)?;
        let changed = r.enabled != on;
        r.enabled = on;
        Ok(changed)
    });
    // Turned on, it is checked now: it was not while it was off.
    if on && matches!(result, Ok(true)) {
        shell::want(Job::Check(Some(repo)));
    }
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
    let Some(repo) = shown_repo(|r, _| r.clone()) else { return };
    let mut items = Vec::new();
    if repo.enabled {
        items.push(SYNC_ONE);
    }
    items.extend([ON_GITHUB, SKILLS_FOLDER, "-", REMOVE]);
    let Some(chosen) = control::menu(button, &items, None, true) else { return };
    match items[chosen] {
        SYNC_ONE => {
            shell::want(Job::Check(Some(repo.id)));
        }
        ON_GITHUB => open_on_github(&repo),
        SKILLS_FOLDER => open_skills_folder(),
        REMOVE => remove(hwnd, &repo),
        _ => {}
    }
}

fn open_on_github(repo: &RepoConfig) {
    let plain = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    match repo.owner_and_name() {
        Some((owner, name)) if plain(owner) && plain(name) => {
            if let Err(e) = Command::new("explorer.exe").arg(format!("https://github.com/{owner}/{name}")).spawn() {
                shell::tell(format!("The browser could not be started: {e}"));
            }
        }
        _ => shell::tell(format!("{} can’t be opened on GitHub.", printable(&repo.repo))),
    }
}

fn remove(hwnd: HWND, repo: &RepoConfig) {
    let question = Ask {
        heading: format!("Remove {}?", printable(&repo.label())),
        text: "Its skills are no longer kept up to date.".to_string(),
        choices: vec!["Leave its skills installed; they become yours".to_string(), "Delete its skills too; skills you edited are kept".to_string()],
        ok: "Remove".to_string(),
        ..Ask::default()
    };
    if let Some(reply) = dialogs::ask(hwnd, question) {
        shell::want(Job::Remove { repo: repo.id.clone(), delete_skills: reply.choice == 1 });
    }
}

/// A part of a skill's row was clicked.
fn skill(hwnd: HWND, index: usize, part: Part) {
    let Some((repo, label, row)) = shown_repo(|r, v| Some((r.id.clone(), v.label.clone(), v.skills.get(index)?.clone()))).flatten() else { return };
    match part {
        Part::Row => {
            with_state(|st| {
                if !st.toggled.remove(&row.name) {
                    st.toggled.insert(row.name.clone());
                }
            });
            render(hwnd);
        }
        Part::Compare => compare(hwnd, repo, label, row, id::SKILL + 8 * index as i32 + 2),
        Part::Open => open_skill(&row.name),
        Part::Action(k) => {
            if let Some(&action) = row.actions.get(k) {
                act(hwnd, repo, &label, row.name, action);
            }
        }
    }
}

fn act(hwnd: HWND, repo: String, label: &str, skill: String, action: Action) {
    match action.resolution() {
        Some(how) => {
            shell::want(Job::Resolve { repo, skill, how });
        }
        None => {
            let question = Ask {
                heading: format!("Delete the skills that are gone from {}?", printable(label)),
                text: "It lists no skills at all now, which can be a mistake upstream. Skills you edited are kept.".to_string(),
                ok: "Delete them".to_string(),
                ..Ask::default()
            };
            if dialogs::ask(hwnd, question).is_some() {
                shell::want(Job::DeleteGone(repo));
            }
        }
    }
}

/// Show changes: the window shows the differences of one skill, read by the worker.
fn compare(hwnd: HWND, repo: String, repo_label: String, row: Row, from: i32) {
    let view =
        DiffView { repo_id: repo.clone(), repo_label, skill: row.name.clone(), actions: row.actions, report: Report::Loading, file: 0, gaps: Vec::new() };
    let Some(p) = with_state(|st| {
        st.diff = Some(view);
        st.diff_from = from;
        st.p
    }) else {
        return;
    };
    shell::want(Job::Diff { repo, skill: row.name });
    control::scroll_top(p.files);
    control::scroll_top(p.code);
    render(hwnd);
    let back = control::child(p.head, id::BACK);
    if !back.is_null() {
        unsafe { SetFocus(back) };
    }
}

fn leave_diff(hwnd: HWND) {
    let Some((from, p)) = with_state(|st| st.diff.take().map(|_| (st.diff_from, st.p))).flatten() else { return };
    render(hwnd);
    // Show changes, or the skill's row if it is closed now.
    let back = [from, from - 2].into_iter().map(|n| control::child(p.page, n)).find(|h| !h.is_null()).unwrap_or(hwnd);
    unsafe { SetFocus(back) };
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
        let d = st.diff.as_ref()?;
        Some((d.repo_id.clone(), d.skill.clone(), *d.actions.get(index)?))
    })
    .flatten() else {
        return;
    };
    leave_diff(hwnd);
    if let Some(how) = action.resolution() {
        shell::want(Job::Resolve { repo, skill, how });
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
    let Some(dir) = with_state(|st| st.snap.as_ref().map(|s| s.skills_dir.clone())).flatten() else { return };
    // Only a plain folder name: the name comes from upstream, and a rejected one is listed too.
    let plain = !name.is_empty() && name != "." && name != ".." && !name.contains(['\\', '/', ':']);
    let path = dir.join(name);
    if !plain || !path.is_dir() {
        shell::tell(format!("{} is not in the skills folder.", printable(name)));
    } else if let Err(e) = open_folder(&path) {
        shell::tell(e);
    }
}

/// Escape: back from the diff view, else the window closes.
fn escape(hwnd: HWND) {
    if DIFF_MODE.with(Cell::get) {
        leave_diff(hwnd);
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
        (_, code) if code != BN_CLICKED => {}
        (Hit::Repo(i), _) => {
            if let Some(page) = with_state(|st| st.snap.as_ref()?.views.get(i).map(|v| Page::Repo(v.id.clone()))).flatten() {
                go(hwnd, Some(page));
            }
        }
        (Hit::Skill(j, part), _) => skill(hwnd, j, part),
        (Hit::File(i), _) => show_file(hwnd, i),
        (Hit::Gap(r), _) => open_gap(hwnd, r),
        (Hit::DiffAction(k), _) => diff_action(hwnd, k),
        (Hit::Other(n), _) => match n {
            id::SYNC => {
                shell::want(Job::Check(None));
            }
            id::NAV_ADD | id::WELCOME_ADD => add(hwnd),
            id::NAV_ACTIVITY => go(hwnd, Some(Page::Activity)),
            id::NAV_SETTINGS | id::OPEN_SETTINGS => go(hwnd, Some(Page::Settings)),
            id::MORE => more(hwnd, ctl),
            id::ENABLED => set_enabled(ctl),
            id::STARTUP => set_startup(ctl),
            id::RETRY => {
                if let Some(repo) = shown_repo(|r, _| r.id.clone()) {
                    shell::want(Job::Check(Some(repo)));
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
    unsafe {
        KillTimer(hwnd, TIMER_REFRESH);
        KillTimer(hwnd, TIMER_LIST);
    }
    let state = STATE.with(|s| s.try_borrow_mut().ok().and_then(|mut s| s.take()));
    WINDOW.with(|w| w.set(null_mut()));
    REFRESH_POSTED.with(|p| p.set(false));
    DIFF_MODE.with(|d| d.set(false));
    LIST_SHOWN.with(|n| n.set(NAV_W));
    LIST_RESTING.with(|r| r.set(false));
    LIST_WIDE.with(|w| w.set(false));
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
        WM_TIMER if wp == TIMER_LIST => on_rested(hwnd),
        control::WM_HOVER => on_hover(hwnd),
        WM_SIZE => {
            if wp as u32 != SIZE_MINIMIZED {
                render(hwnd);
            }
        }
        WM_EXITSIZEMOVE => save_size(hwnd),
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
    fn the_saved_size_is_two_numbers_and_never_smaller_than_the_smallest() {
        assert_eq!(parse_size("1200 800\n"), Some((1200, 800)));
        assert_eq!(parse_size("100 50"), Some(MIN_SIZE));
        assert_eq!(parse_size("1200"), None);
        assert_eq!(parse_size("1200 800 3"), None);
        assert_eq!(parse_size("wide tall"), None);
        assert_eq!(parse_size(""), None);
    }
}
