//! The settings window: the repositories and their skills, what needs the user's decision, and the few settings
//! there are. Every change is saved to `config.json` at once; there is no OK button.
//!
//! What it shows comes from [`shell::snapshot`], on a posted `WM_REFRESH`. Handlers read what they need from the
//! state, let go of it, and then act, so a question (which runs a message loop) or a notification sent back from
//! inside a change never finds the state borrowed by them.

use std::cell::{Cell, RefCell};
use std::fs;
use std::path::PathBuf;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{RDW_ALLCHILDREN, RDW_UPDATENOW, RedrawWindow};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::{EM_SETCUEBANNER, LVIF_STATE, LVIS_SELECTED, LVN_ITEMCHANGED, NM_DBLCLK, NMHDR, NMITEMACTIVATE, NMLISTVIEW};
use windows_sys::Win32::UI::HiDpi::{GetDpiForSystem, GetSystemMetricsForDpi};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetFocus, SetFocus};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BN_CLICKED, CB_DELETESTRING, CB_ERR, CB_FINDSTRINGEXACT, CB_GETCURSEL, CB_GETLBTEXT, CB_GETLBTEXTLEN, CB_INSERTSTRING, CB_SETCURSEL, CBN_KILLFOCUS,
    CBN_SELENDOK, CBN_SETFOCUS, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyWindow, ES_AUTOVSCROLL, ES_MULTILINE, ES_READONLY, GetParent, HICON,
    ICON_BIG, ICON_SMALL, IDCANCEL, IDOK, IsChild, IsIconic, IsWindow, KillTimer, PostMessageW, SM_CXICON, SM_CXSMICON, SW_RESTORE, SW_SHOWNORMAL,
    SWP_NOACTIVATE, SWP_NOZORDER, SendMessageW, SetForegroundWindow, SetTimer, SetWindowPos, ShowWindow, WA_INACTIVE, WM_ACTIVATE, WM_APP, WM_CLOSE,
    WM_COMMAND, WM_DPICHANGED, WM_NCDESTROY, WM_NOTIFY, WM_SETICON, WM_TIMER, WS_CAPTION, WS_EX_CLIENTEDGE, WS_EX_CONTROLPARENT, WS_MINIMIZEBOX, WS_OVERLAPPED,
    WS_SYSMENU, WS_TABSTOP, WS_VSCROLL,
};

use super::dialogs::{self, Ask};
use super::shell::{self, Snapshot};
use super::{
    Form, SS_ENDELLIPSIS, SS_NOPREFIX, bench, checked, enable, lv_fill, lv_select, lv_selected, open_folder, place, register_class, sentence, set_checked,
    set_text, show, text_of, tray, trim_memory,
};
use crate::app::Job;
use crate::board::{APP_NAME, Action, Health, Row};
use crate::config::{Config, RepoConfig, RepoSpec};
use crate::interval::Interval;
use crate::pathsafe::printable;
use crate::win::{cred, wide};

const CLASS: &str = "TinySkillSyncSettings";
const STYLE: u32 = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
const EX_STYLE: u32 = WS_EX_CONTROLPARENT;
/// Client size, in DIPs.
const SIZE: (i32, i32) = (600, 580);
/// Show what the snapshot says now.
const WM_REFRESH: u32 = WM_APP + 10;
/// "5 minutes ago" goes stale; this keeps it roughly right.
const TIMER_REFRESH: usize = 1;
const REFRESH_MS: u32 = 60_000;
const PRESETS: &[&str] = &["30 minutes", "1 hour", "6 hours", "12 hours", "1 day", "1 week", "1 month"];
/// Where the details of the selected skill, or of the repository when none is selected, go (x, y, width, height in
/// DIPs): beside the action buttons when there are some, across the window when there are none. About three lines of
/// text fit; what is longer is reached with the scroll bar.
const DETAIL_BESIDE: [i32; 4] = [12, 342, 304, 52];
const DETAIL_ACROSS: [i32; 4] = [12, 342, 576, 52];

const ID_REPOS: i32 = 100;
const ID_EVERY: i32 = 101;
const ID_USE_TOKEN: i32 = 102;
const ID_ENABLED: i32 = 103;
const ID_CHECK_ONE: i32 = 104;
const ID_REMOVE: i32 = 105;
const ID_ADD_TEXT: i32 = 106;
const ID_PRIVATE: i32 = 107;
const ID_ADD: i32 = 108;
const ID_SKILLS: i32 = 110;
const ID_DETAIL: i32 = 111;
const ID_ACTION_1: i32 = 112;
const ID_ACTION_2: i32 = 113;
const ID_DEFAULT: i32 = 120;
const ID_TOKEN_SET: i32 = 121;
const ID_TOKEN_CLEAR: i32 = 122;
const ID_FOLDER: i32 = 123;
const ID_OPEN: i32 = 124;
const ID_BACKUPS: i32 = 125;
const ID_NOTES: i32 = 126;
const ID_CHECK_ALL: i32 = 130;

#[derive(Clone, Copy)]
struct Controls {
    repos: HWND,
    every: HWND,
    use_token: HWND,
    enabled: HWND,
    check_one: HWND,
    remove: HWND,
    add_text: HWND,
    private: HWND,
    skills: HWND,
    detail: HWND,
    actions: [HWND; 2],
    default: HWND,
    token: HWND,
    token_clear: HWND,
    folder: HWND,
    notes: HWND,
    status: HWND,
}

struct Settings {
    form: Form,
    c: Controls,
    icons: [HICON; 2],
    repo_rows: Vec<Vec<String>>,
    /// The repositories' ids, in the order the list shows them.
    repo_ids: Vec<String>,
    skill_rows: Vec<Vec<String>>,
    /// The skills' own names in the order the list shows them. The list shows them written out, so its text is not
    /// the name.
    skill_names: Vec<String>,
    /// The repository whose skills are shown.
    repo: Option<String>,
    /// The skill whose details are shown, by name.
    skill: Option<String>,
    /// What the action buttons do.
    actions: Vec<Action>,
    /// The first item of the "check every" list, which names the default.
    default_item: String,
    /// The user is typing in the "check every" box, for this repository; it is applied when they leave it.
    editing_every: Option<String>,
    editing_default: bool,
    token_saved: bool,
    uses_token: bool,
    skills_dir: PathBuf,
    work_dir: PathBuf,
    notes: String,
    /// The control that had the focus when the window was last deactivated.
    focus: HWND,
}

thread_local! {
    static WINDOW: Cell<HWND> = const { Cell::new(null_mut()) };
    static STATE: RefCell<Option<Settings>> = const { RefCell::new(None) };
    static REFRESH_POSTED: Cell<bool> = const { Cell::new(false) };
}

pub fn window() -> HWND {
    WINDOW.with(Cell::get)
}

pub fn is_open() -> bool {
    !window().is_null()
}

/// Runs `f` on the state, unless the window is closed or the state is in use further up the stack.
fn with_state<R>(f: impl FnOnce(&mut Settings) -> R) -> Option<R> {
    STATE.with(|s| {
        let mut state = s.try_borrow_mut().ok()?;
        state.as_mut().map(f)
    })
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
    if !register_class(CLASS, Some(proc), null_mut()) {
        return;
    }
    let opened = std::time::Instant::now();
    let [x, y, w, h] = place(STYLE, EX_STYLE, SIZE, unsafe { GetDpiForSystem() }, null_mut());
    let hwnd = unsafe {
        CreateWindowExW(EX_STYLE, wide(CLASS).as_ptr(), wide(APP_NAME).as_ptr(), STYLE, x, y, w, h, null_mut(), null_mut(), GetModuleHandleW(null()), null())
    };
    if hwnd.is_null() {
        return;
    }
    let mut form = Form::new(hwnd);
    let c = build(&mut form);
    let icons = [SM_CXICON, SM_CXSMICON].map(|metric| tray::icon(Health::Ok, unsafe { GetSystemMetricsForDpi(metric, form.dpi) }));
    unsafe {
        SendMessageW(hwnd, WM_SETICON, ICON_BIG as usize, icons[0] as isize);
        SendMessageW(hwnd, WM_SETICON, ICON_SMALL as usize, icons[1] as isize);
    }
    // Now that the window knows its monitor's DPI.
    let [x, y, w, h] = place(STYLE, EX_STYLE, SIZE, form.dpi, null_mut());
    let state = Settings {
        form,
        c,
        icons,
        repo_rows: Vec::new(),
        repo_ids: Vec::new(),
        skill_rows: Vec::new(),
        skill_names: Vec::new(),
        repo: None,
        skill: None,
        actions: Vec::new(),
        default_item: "Default".to_string(),
        editing_every: None,
        editing_default: false,
        token_saved: cred::has_token(),
        uses_token: false,
        skills_dir: PathBuf::new(),
        work_dir: PathBuf::new(),
        notes: String::new(),
        focus: null_mut(),
    };
    STATE.with(|s| *s.borrow_mut() = Some(state));
    WINDOW.with(|w| w.set(hwnd));
    REFRESH_POSTED.with(|p| p.set(false));
    update();
    let first = with_state(|st| if st.repo_ids.is_empty() { st.c.add_text } else { st.c.repos }).unwrap_or(null_mut());
    unsafe {
        SetWindowPos(hwnd, null_mut(), x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
        SetTimer(hwnd, TIMER_REFRESH, REFRESH_MS, None);
        ShowWindow(hwnd, SW_SHOWNORMAL);
        SetForegroundWindow(hwnd);
        SetFocus(first);
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

fn build(f: &mut Form) -> Controls {
    let heading = f.label("Repositories", [12, 8, 576, 16]);
    f.embolden(heading);
    let repos = f.list([12, 26, 576, 112], ID_REPOS, &[("Repository", 220), ("Every", 110), ("Last check", 100), ("Status", 120)]);
    f.label("Check every", [12, 150, 72, 16]);
    let every_items: Vec<&str> = std::iter::once("Default").chain(PRESETS.iter().copied()).collect();
    let every = f.combo([86, 146, 120, 200], ID_EVERY, &every_items);
    let use_token = f.check("Use my token", [216, 147, 110, 22], ID_USE_TOKEN);
    let enabled = f.check("Enabled", [330, 147, 80, 22], ID_ENABLED);
    let check_one = f.button("Check now", [420, 145, 80, 26], ID_CHECK_ONE);
    let remove = f.button("Remove…", [508, 145, 80, 26], ID_REMOVE);
    let add_text = f.edit([12, 180, 398, 24], ID_ADD_TEXT, 0);
    unsafe { SendMessageW(add_text, EM_SETCUEBANNER, 1, wide("owner/name, or the address of a GitHub folder").as_ptr() as isize) };
    let private = f.check("Private", [420, 181, 80, 22], ID_PRIVATE);
    f.button("Add", [508, 179, 80, 26], ID_ADD);

    let heading = f.label("Skills", [12, 214, 576, 16]);
    f.embolden(heading);
    let skills = f.list([12, 232, 576, 104], ID_SKILLS, &[("Skill", 170), ("Status", 380)]);
    let detail_style = WS_TABSTOP | WS_VSCROLL | (ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL) as u32;
    let detail = f.add("EDIT", "", detail_style, WS_EX_CLIENTEDGE, DETAIL_ACROSS, ID_DETAIL);
    let actions = [f.button("", [324, 342, 128, 26], ID_ACTION_1), f.button("", [460, 342, 128, 26], ID_ACTION_2)];

    f.label("Default check interval", [12, 406, 130, 16]);
    let default = f.combo([144, 402, 110, 200], ID_DEFAULT, PRESETS);
    let token = f.label("", [270, 406, 186, 16]);
    f.button("Set…", [464, 401, 60, 26], ID_TOKEN_SET);
    let token_clear = f.button("Clear", [528, 401, 60, 26], ID_TOKEN_CLEAR);
    f.label("Skills folder", [12, 440, 72, 16]);
    let folder = f.edit([86, 436, 342, 24], ID_FOLDER, ES_READONLY as u32);
    f.button("Open", [436, 435, 74, 26], ID_OPEN);
    f.button("Backups", [514, 435, 74, 26], ID_BACKUPS);

    let heading = f.label("Messages", [12, 468, 576, 16]);
    f.embolden(heading);
    let notes_style = WS_TABSTOP | WS_VSCROLL | (ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL) as u32;
    let notes = f.add("EDIT", "", notes_style, WS_EX_CLIENTEDGE, [12, 486, 576, 52], ID_NOTES);
    let status = f.add("STATIC", "", SS_NOPREFIX | SS_ENDELLIPSIS, 0, [12, 551, 400, 16], 0);
    f.button("Check all", [420, 546, 80, 26], ID_CHECK_ALL);
    f.button("Close", [508, 546, 80, 26], IDCANCEL);

    Controls {
        repos,
        every,
        use_token,
        enabled,
        check_one,
        remove,
        add_text,
        private,
        skills,
        detail,
        actions,
        default,
        token,
        token_clear,
        folder,
        notes,
        status,
    }
}

fn update() {
    REFRESH_POSTED.with(|p| p.set(false));
    if let Some(snap) = shell::snapshot() {
        with_state(|st| st.show(&snap));
    }
}

impl Settings {
    fn show(&mut self, snap: &Snapshot) {
        let c = self.c;
        let default = snap.cfg.default_interval;
        let default_item = format!("Default ({})", default.describe());

        let rows: Vec<Vec<String>> = snap
            .views
            .iter()
            .map(|v| {
                let every = snap.cfg.find(&v.id).and_then(|r| r.interval).map_or_else(|| format!("{} (default)", default.describe()), Interval::describe);
                vec![v.label.clone(), every, v.last_check_text(snap.now), v.status.clone()]
            })
            .collect();
        lv_fill(c.repos, &rows, &mut self.repo_rows);
        self.repo_ids = snap.views.iter().map(|v| v.id.clone()).collect();
        if self.repo.as_ref().is_none_or(|id| !self.repo_ids.contains(id)) {
            self.repo = self.repo_ids.first().cloned();
            self.skill = None;
        }
        let index = self.repo.as_ref().and_then(|id| self.repo_ids.iter().position(|x| x == id));
        if lv_selected(c.repos) != index {
            lv_select(c.repos, index);
        }
        let view = index.map(|i| &snap.views[i]);
        let repo = view.and_then(|v| snap.cfg.find(&v.id));

        if self.default_item != default_item {
            unsafe {
                SendMessageW(c.every, CB_DELETESTRING, 0, 0);
                SendMessageW(c.every, CB_INSERTSTRING, 0, wide(&default_item).as_ptr() as isize);
            }
            self.default_item = default_item.clone();
        }
        if self.editing_every.is_none() {
            show_choice(c.every, &repo.map_or_else(String::new, |r| r.interval.map_or(default_item, Interval::describe)));
        }
        set_checked(c.use_token, repo.is_some_and(|r| r.use_token));
        set_checked(c.enabled, repo.is_some_and(|r| r.enabled));
        for control in [c.every, c.use_token, c.enabled, c.remove] {
            enable(control, repo.is_some());
        }
        enable(c.check_one, repo.is_some_and(|r| r.enabled));

        let (skill_rows, skill_names) = view.map(|v| skill_list(&v.skills)).unwrap_or_default();
        self.skill_names = skill_names;
        lv_fill(c.skills, &skill_rows, &mut self.skill_rows);
        let skill_index = self.skill.as_ref().and_then(|name| view?.skills.iter().position(|r| &r.name == name));
        if skill_index.is_none() {
            self.skill = None;
        }
        if lv_selected(c.skills) != skill_index {
            lv_select(c.skills, skill_index);
        }
        let row = view.zip(skill_index).map(|(v, i)| &v.skills[i]);
        let detail = match (row, view) {
            (Some(row), _) => format!("{}: {}", row.name, row.text),
            (None, Some(v)) => match &v.detail {
                Some(detail) => detail.clone(),
                None if v.skills.iter().any(|r| !r.actions.is_empty()) => "Select a skill to see what you can do about it.".to_string(),
                None => String::new(),
            },
            (None, None) => String::new(),
        };
        set_text(c.detail, &printable(&detail));
        self.actions = row.map(|r| r.actions.clone()).unwrap_or_default();
        self.form.resize(c.detail, if self.actions.is_empty() { DETAIL_ACROSS } else { DETAIL_BESIDE });
        for (i, button) in c.actions.into_iter().enumerate() {
            if let Some(action) = self.actions.get(i) {
                set_text(button, action.label());
            }
            show(button, i < self.actions.len());
        }

        if !self.editing_default {
            show_choice(c.default, &default.describe());
        }
        set_text(c.token, if self.token_saved { "GitHub token: saved" } else { "GitHub token: not saved" });
        enable(c.token_clear, self.token_saved);
        set_text(c.folder, &snap.skills_dir.display().to_string());
        self.skills_dir = snap.skills_dir.clone();
        self.work_dir = snap.work_dir.clone();
        self.uses_token = snap.cfg.repos.iter().any(|r| r.enabled && r.use_token);
        if self.notes != snap.notes {
            set_text(c.notes, &snap.notes);
            self.notes = snap.notes.clone();
        }
        set_text(c.status, &snap.status);
    }

    /// The selected repository's id and how the list shows it.
    fn selected_repo(&self) -> Option<(String, String)> {
        let id = self.repo.clone()?;
        let index = self.repo_ids.iter().position(|x| *x == id)?;
        Some((id, self.repo_rows.get(index)?.first()?.clone()))
    }

    /// The user selected (or deselected) a row. Whether that changes what is shown.
    fn select(&mut self, repos: bool, index: usize, on: bool) -> bool {
        if repos {
            // Clicking empty space deselects; the details stay with the last repository.
            let Some(id) = self.repo_ids.get(index).filter(|_| on) else { return false };
            if self.repo.as_ref() == Some(id) {
                return false;
            }
            self.repo = Some(id.clone());
            self.skill = None;
            return true;
        }
        pick_skill(&mut self.skill, &self.skill_names, index, on)
    }
}

/// What the skills list shows, and the skills' own names in the same order. Names and messages come from the
/// repository, so the list shows them written out (`printable`); a skill is told apart, chosen and acted on by its
/// real name, which the text shown may not be.
fn skill_list(skills: &[Row]) -> (Vec<Vec<String>>, Vec<String>) {
    let rows = skills.iter().map(|r| vec![printable(&r.name).into_owned(), printable(&r.text).into_owned()]).collect();
    (rows, skills.iter().map(|r| r.name.clone()).collect())
}

/// A row of the skills list was selected (`on`) or deselected. Changes which skill is chosen, and says whether it did.
fn pick_skill(chosen: &mut Option<String>, names: &[String], index: usize, on: bool) -> bool {
    let name = names.get(index).cloned();
    if on {
        if *chosen == name {
            return false;
        }
        *chosen = name;
    } else {
        if name.is_none() || *chosen != name {
            return false;
        }
        *chosen = None;
    }
    true
}

/// Shows `text` in a combo box, selecting the item that says it if there is one.
fn show_choice(combo: HWND, text: &str) {
    let found = unsafe { SendMessageW(combo, CB_FINDSTRINGEXACT, usize::MAX, wide(text).as_ptr() as isize) };
    let current = unsafe { SendMessageW(combo, CB_GETCURSEL, 0, 0) };
    if found >= 0 {
        if current != found || text_of(combo) != text {
            unsafe { SendMessageW(combo, CB_SETCURSEL, found as usize, 0) };
        }
    } else {
        if current != CB_ERR as isize {
            unsafe { SendMessageW(combo, CB_SETCURSEL, usize::MAX, 0) };
        }
        set_text(combo, text);
    }
}

fn combo_item(combo: HWND, index: isize) -> String {
    let Ok(at) = usize::try_from(index) else { return String::new() };
    let len = unsafe { SendMessageW(combo, CB_GETLBTEXTLEN, at, 0) };
    if len < 0 {
        return String::new();
    }
    let mut buf = vec![0u16; len as usize + 1];
    let got = unsafe { SendMessageW(combo, CB_GETLBTEXT, at, buf.as_mut_ptr() as isize) };
    String::from_utf16_lossy(&buf[..got.clamp(0, len) as usize])
}

/// What the "check every" box says: `None` for the default interval.
fn parse_every(text: &str) -> Result<Option<Interval>, String> {
    let text = text.trim();
    if text.is_empty() || text.to_ascii_lowercase().starts_with("default") {
        return Ok(None);
    }
    Interval::parse(text)?.validate_poll().map(Some)
}

fn repo_mut<'a>(cfg: &'a mut Config, id: &str) -> Result<&'a mut RepoConfig, String> {
    cfg.repos.iter_mut().find(|r| r.id == id).ok_or_else(|| "that repository is no longer in the settings".to_string())
}

#[derive(Clone, Copy)]
enum Field {
    Every,
    Default,
}

fn on_combo(field: Field, code: u32) {
    match code {
        CBN_SETFOCUS => {
            with_state(|st| match field {
                Field::Every => st.editing_every = st.repo.clone(),
                Field::Default => st.editing_default = true,
            });
        }
        CBN_SELENDOK => {
            let Some(combo) = with_state(|st| combo_of(st, field)) else { return };
            let text = combo_item(combo, unsafe { SendMessageW(combo, CB_GETCURSEL, 0, 0) });
            apply(field, &text);
        }
        CBN_KILLFOCUS => {
            let Some(combo) = with_state(|st| combo_of(st, field)) else { return };
            apply(field, &text_of(combo));
            with_state(|st| match field {
                Field::Every => st.editing_every = None,
                Field::Default => st.editing_default = false,
            });
            refresh();
        }
        _ => {}
    }
}

fn combo_of(st: &Settings, field: Field) -> HWND {
    match field {
        Field::Every => st.c.every,
        Field::Default => st.c.default,
    }
}

/// Saves what the user chose or typed in a combo box. Nothing is saved when it says what is set already.
fn apply(field: Field, text: &str) {
    match field {
        Field::Every => {
            let Some(repo) = with_state(|st| st.editing_every.clone().or_else(|| st.repo.clone())).flatten() else { return };
            match parse_every(text) {
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
        Field::Default => match Interval::parse(text).and_then(Interval::validate_poll) {
            Ok(every) => {
                let _ = shell::edit_config(|cfg| {
                    let changed = cfg.default_interval != every;
                    cfg.default_interval = every;
                    Ok(changed)
                });
            }
            Err(e) => shell::tell(format!("Default check interval: {e}")),
        },
    }
    refresh();
}

/// Enter: what it means depends on where the user is typing.
fn on_enter() {
    let focus = unsafe { GetFocus() };
    let Some(c) = with_state(|st| st.c) else { return };
    // A combo box that can be typed in has an edit box of its own, which is what has the focus.
    let within = |combo: HWND| !focus.is_null() && (focus == combo || unsafe { GetParent(focus) } == combo);
    if focus == c.add_text {
        add();
    } else if within(c.every) {
        apply(Field::Every, &text_of(c.every));
    } else if within(c.default) {
        apply(Field::Default, &text_of(c.default));
    }
}

fn add() {
    let Some((edit, private)) = with_state(|st| (st.c.add_text, st.c.private)) else { return };
    let spec = match RepoSpec::parse(&text_of(edit)) {
        Ok(spec) => spec,
        Err(e) => {
            shell::tell(sentence(&e));
            unsafe { SetFocus(edit) };
            return;
        }
    };
    let use_token = checked(private);
    let mut added = None;
    let result = shell::edit_config(|cfg| {
        let full_name = spec.full_name();
        if let Some(existing) = cfg.repos.iter().find(|r| r.repo.eq_ignore_ascii_case(&full_name) && r.branch == spec.branch && r.path == spec.path) {
            return Err(format!("{} is already set up", existing.label()));
        }
        added = Some(cfg.add_repo(&spec, None, use_token).id.clone());
        Ok(true)
    });
    if let (Ok(true), Some(id)) = (result, added) {
        set_text(edit, "");
        set_checked(private, false);
        with_state(|st| {
            st.repo = Some(id.clone());
            st.skill = None;
        });
        shell::want(Job::Check(Some(id)));
    }
    refresh();
}

/// "Use my token" or "Enabled" was clicked.
fn toggle(id: i32) {
    let Some((repo, button)) = with_state(|st| Some((st.repo.clone()?, if id == ID_ENABLED { st.c.enabled } else { st.c.use_token }))).flatten() else {
        return;
    };
    let on = checked(button);
    let mut enabled = false;
    let result = shell::edit_config(|cfg| {
        let r = repo_mut(cfg, &repo)?;
        let field = if id == ID_ENABLED { &mut r.enabled } else { &mut r.use_token };
        let changed = *field != on;
        *field = on;
        enabled = r.enabled;
        Ok(changed)
    });
    // Turned on, it can be checked now: it was off, or it may have failed for want of the token.
    if on && enabled && matches!(result, Ok(true)) {
        shell::want(Job::Check(Some(repo)));
    }
    refresh();
}

fn remove(hwnd: HWND) {
    let Some((repo, label)) = with_state(|st| st.selected_repo()).flatten() else { return };
    let heading = format!("Remove {label}?");
    let question = Ask {
        title: APP_NAME,
        heading: &heading,
        text: "Its skills are no longer kept up to date.",
        choices: &["Leave its skills installed; they become yours", "Delete its skills too; skills you edited are kept"],
        secret: false,
        ok: "Remove",
    };
    if let Some(reply) = dialogs::ask(hwnd, &question) {
        shell::want(Job::Remove { repo, delete_skills: reply.choice == 1 });
    }
}

/// One of the selected skill's action buttons was clicked.
fn act(hwnd: HWND, index: usize) {
    let Some((repo, label, skill, action)) = with_state(|st| {
        let (repo, label) = st.selected_repo()?;
        Some((repo, label, st.skill.clone()?, *st.actions.get(index)?))
    })
    .flatten() else {
        return;
    };
    match action.resolution() {
        Some(how) => {
            shell::want(Job::Resolve { repo, skill, how });
        }
        None => {
            let heading = format!("Delete the skills that are gone from {label}?");
            let question = Ask {
                title: APP_NAME,
                heading: &heading,
                text: "It lists no skills at all now, which can be a mistake upstream. Skills you edited are kept.",
                choices: &[],
                secret: false,
                ok: "Delete them",
            };
            if dialogs::ask(hwnd, &question).is_some() {
                shell::want(Job::DeleteGone(repo));
            }
        }
    }
}

fn set_token(hwnd: HWND) {
    let question = Ask {
        title: APP_NAME,
        heading: "Save a GitHub token",
        text: "It is kept in Windows Credential Manager and used for the repositories that have \"Use my token\" turned on. \
               Read access to their contents is enough. Without a saved token, the GitHub CLI's sign-in is used, if there is one.",
        choices: &[],
        secret: true,
        ok: "Save",
    };
    let Some(secret) = dialogs::ask(hwnd, &question).and_then(|reply| reply.secret) else { return };
    let saved = cred::write_token(secret.expose());
    drop(secret);
    match saved {
        Ok(()) => {
            let uses_token = with_state(|st| {
                st.token_saved = true;
                st.uses_token
            });
            if uses_token == Some(true) {
                shell::want(Job::Check(None));
            } else {
                shell::tell("Your GitHub token is saved.".to_string());
            }
        }
        Err(e) => shell::tell(format!("The token could not be saved: {e}")),
    }
    refresh();
}

fn clear_token(hwnd: HWND) {
    let question = Ask {
        title: APP_NAME,
        heading: "Delete your saved GitHub token?",
        text: "Repositories that use a token then use the GitHub CLI's sign-in, if there is one.",
        choices: &[],
        secret: false,
        ok: "Delete",
    };
    if dialogs::ask(hwnd, &question).is_none() {
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
    let Some(dir) = with_state(|st| st.skills_dir.clone()) else { return };
    if !dir.is_dir() {
        shell::tell("The skills folder is made when the first skill is installed.".to_string());
    } else if let Err(e) = open_folder(&dir) {
        shell::tell(e);
    }
}

fn open_backups() {
    let Some(work) = with_state(|st| st.work_dir.clone()) else { return };
    let any = fs::read_dir(&work).is_ok_and(|entries| entries.flatten().any(|e| e.file_name().to_string_lossy().starts_with("backup-")));
    if !any {
        shell::tell("There are no backups yet.".to_string());
    } else if let Err(e) = open_folder(&work) {
        shell::tell(e);
    }
}

/// A skill was double-clicked: show its folder.
fn open_skill(index: usize) {
    let Some((dir, name)) = with_state(|st| Some((st.skills_dir.clone(), st.skill_names.get(index)?.clone()))).flatten() else { return };
    // Only a plain folder name: the name comes from upstream, and a rejected one is listed too.
    let plain = !name.is_empty() && name != "." && name != ".." && !name.contains(['\\', '/', ':']);
    let path = dir.join(&name);
    if !plain || !path.is_dir() {
        shell::tell(format!("{} is not in the skills folder.", printable(&name)));
    } else if let Err(e) = open_folder(&path) {
        shell::tell(e);
    }
}

fn on_command(hwnd: HWND, id: i32, code: u32) {
    match id {
        ID_EVERY => on_combo(Field::Every, code),
        ID_DEFAULT => on_combo(Field::Default, code),
        _ if code != BN_CLICKED => {}
        IDOK => on_enter(),
        IDCANCEL => unsafe {
            DestroyWindow(hwnd);
        },
        ID_USE_TOKEN | ID_ENABLED => toggle(id),
        ID_CHECK_ONE => {
            if let Some(repo) = with_state(|st| st.repo.clone()).flatten() {
                shell::want(Job::Check(Some(repo)));
            }
        }
        ID_REMOVE => remove(hwnd),
        ID_ADD => add(),
        ID_ACTION_1 => act(hwnd, 0),
        ID_ACTION_2 => act(hwnd, 1),
        ID_TOKEN_SET => set_token(hwnd),
        ID_TOKEN_CLEAR => clear_token(hwnd),
        ID_OPEN => open_skills_folder(),
        ID_BACKUPS => open_backups(),
        ID_CHECK_ALL => {
            shell::want(Job::Check(None));
        }
        _ => {}
    }
}

fn on_notify(lparam: LPARAM) {
    let hdr = unsafe { &*(lparam as *const NMHDR) };
    let id = hdr.idFrom as i32;
    match (id, hdr.code) {
        (ID_REPOS | ID_SKILLS, LVN_ITEMCHANGED) => {
            let nm = unsafe { &*(lparam as *const NMLISTVIEW) };
            if nm.uChanged & LVIF_STATE == 0 || (nm.uNewState ^ nm.uOldState) & LVIS_SELECTED == 0 || nm.iItem < 0 {
                return;
            }
            let on = nm.uNewState & LVIS_SELECTED != 0;
            // Borrowed means the change was made by `show`, which knows already.
            if with_state(|st| st.select(id == ID_REPOS, nm.iItem as usize, on)) == Some(true) {
                refresh();
            }
        }
        (ID_SKILLS, NM_DBLCLK) => {
            let nm = unsafe { &*(lparam as *const NMITEMACTIVATE) };
            if let Ok(index) = usize::try_from(nm.iItem) {
                open_skill(index);
            }
        }
        _ => {}
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
    // A token may have been saved or deleted with `tss` meanwhile.
    let saved = cred::has_token();
    let focus = with_state(|st| {
        st.token_saved = saved;
        st.focus
    })
    .unwrap_or(null_mut());
    refresh();
    if !focus.is_null() && unsafe { IsWindow(focus) } != 0 {
        unsafe { SetFocus(focus) };
        return true;
    }
    false
}

fn on_destroyed(hwnd: HWND) {
    unsafe { KillTimer(hwnd, TIMER_REFRESH) };
    let state = STATE.with(|s| s.try_borrow_mut().ok().and_then(|mut s| s.take()));
    WINDOW.with(|w| w.set(null_mut()));
    REFRESH_POSTED.with(|p| p.set(false));
    if let Some(state) = state {
        for icon in state.icons {
            unsafe { DestroyIcon(icon) };
        }
    }
    if shell::is_idle() {
        trim_memory();
    }
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_REFRESH => update(),
        WM_COMMAND => on_command(hwnd, (wparam & 0xFFFF) as i32, ((wparam >> 16) & 0xFFFF) as u32),
        WM_NOTIFY => on_notify(lparam),
        WM_TIMER if wparam == TIMER_REFRESH => refresh(),
        WM_ACTIVATE => {
            if on_activate(hwnd, (wparam & 0xFFFF) as u32 != WA_INACTIVE) {
                return 0;
            }
            return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
        }
        WM_DPICHANGED => {
            with_state(|st| st.form.set_dpi((wparam & 0xFFFF) as u32));
            let rc = unsafe { &*(lparam as *const RECT) };
            unsafe { SetWindowPos(hwnd, null_mut(), rc.left, rc.top, rc.right - rc.left, rc.bottom - rc.top, SWP_NOZORDER | SWP_NOACTIVATE) };
        }
        WM_CLOSE => unsafe {
            DestroyWindow(hwnd);
        },
        WM_NCDESTROY => {
            on_destroyed(hwnd);
            return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
        }
        _ => return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::Level;
    use crate::interval::Unit;

    fn row(name: &str, text: &str) -> Row {
        Row { name: name.to_string(), text: text.to_string(), level: Level::Attention, actions: Vec::new() }
    }

    #[test]
    fn check_every_is_the_default_unless_it_names_an_interval_that_may_be_polled() {
        assert_eq!(parse_every(""), Ok(None));
        assert_eq!(parse_every("  Default (1 day)"), Ok(None));
        assert_eq!(parse_every("6 hours"), Ok(Some(Interval::new(6, Unit::Hour))));
        assert_eq!(parse_every(" 2h "), Ok(Some(Interval::new(2, Unit::Hour))));
        assert!(parse_every("1 minute").is_err());
        assert!(parse_every("often").is_err());
    }

    #[test]
    fn the_skills_list_shows_text_written_out_and_keeps_the_real_names() {
        let skills = [row("alpha", "Up to date"), row("rlo-\u{202e}gpj", "Edited\u{1b}")];
        let (rows, names) = skill_list(&skills);
        assert_eq!(rows, [vec!["alpha", "Up to date"], vec!["rlo-\\u{202e}gpj", "Edited\\u{1b}"]]);
        assert_eq!(names, ["alpha", "rlo-\u{202e}gpj"]);
    }

    #[test]
    fn a_skill_is_chosen_by_its_real_name_not_by_how_the_list_shows_it() {
        let names = vec!["alpha".to_string(), "rlo-\u{202e}gpj".to_string()];
        let mut chosen = None;
        assert!(pick_skill(&mut chosen, &names, 1, true));
        assert_eq!(chosen.as_deref(), Some("rlo-\u{202e}gpj"));
        assert!(!pick_skill(&mut chosen, &names, 1, true), "the same row again changes nothing");
        assert!(!pick_skill(&mut chosen, &names, 0, false), "deselecting another row changes nothing");
        assert!(pick_skill(&mut chosen, &names, 1, false));
        assert_eq!(chosen, None);
        assert!(!pick_skill(&mut chosen, &names, 9, false), "a row that is not there changes nothing");
    }
}
