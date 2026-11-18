//! A question in a small modal window: a heading, some text, and optionally a text box (plain or hidden), a list,
//! choices and a note; then OK and Cancel. [`page::ask`] lays it out and the controls draw it. The text box is a native
//! edit control over a drawn box, so typing, selecting and pasting work as everywhere else in Windows.
//!
//! A custom window, not `TaskDialogIndirect`: that lives only in comctl32 version 6, which the test executables do not
//! load, so importing it would stop them from starting.

use std::cell::{Cell, RefCell};
use std::mem::zeroed;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    CreateSolidBrush, DeleteObject, HBRUSH, HDC, RDW_ERASE, RDW_FRAME, RDW_INVALIDATE, RedrawWindow, SetBkColor, SetTextColor,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::{EM_LIMITTEXT, EM_SETSEL};
use windows_sys::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForSystem, GetDpiForWindow};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, GetFocus, SetFocus};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, ES_AUTOHSCROLL, ES_PASSWORD, GetClientRect, GetDlgItem, GetWindowRect, GetWindowTextLengthW,
    GetWindowTextW, HMENU, HWND_TOP, IDCANCEL, IDOK, IsWindow, MB_ICONWARNING, SW_SHOWNORMAL, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SendMessageW,
    SetForegroundWindow, SetWindowPos, SetWindowTextW, ShowWindow, WM_CLOSE, WM_COMMAND, WM_CTLCOLOREDIT, WM_DESTROY, WM_DPICHANGED, WM_ERASEBKGND, WM_SETFONT,
    WM_SIZE, WS_CAPTION, WS_CHILD, WS_EX_CONTROLPARENT, WS_EX_DLGMODALFRAME, WS_POPUP, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};

use super::control::{self, BN_CLICKED, CBN_SELENDOK};
use super::page::{self, ASK_W, Hint, Question, ask_id};
use super::paint::{self, Font};
use super::theme::{self, Role};
use super::{place, register_class, scale, shell, text_of};
use crate::board::APP_NAME;
use crate::secret::Secret;
use crate::win::wide;

// Declared here: windows-sys files it under System::Diagnostics::Debug, a large feature to build for one function.
#[link(name = "user32")]
unsafe extern "system" {
    fn MessageBeep(kind: u32) -> i32;
}

const CLASS: &str = "TinySkillSyncAsk";
const STYLE: u32 = WS_POPUP | WS_CAPTION | WS_SYSMENU;
const EX_STYLE: u32 = WS_EX_DLGMODALFRAME | WS_EX_CONTROLPARENT;
const PANEL: i32 = 100;
const EN_SETFOCUS: u32 = 0x100;
const EN_KILLFOCUS: u32 = 0x200;
const EN_CHANGE: u32 = 0x300;
const EM_SETMARGINS: u32 = 0xD3;
const EC_BOTH: usize = 3;

/// Reads a text box as it is typed: `Ok` is what the text means, `Err` what is wrong with it.
/// Says what the typed text means, or what is wrong with it.
pub type Verify = dyn Fn(&str) -> Result<String, String>;
pub type Check = Box<Verify>;

pub enum Field {
    /// A line of text. `hint` is shown under it while it is empty.
    Text { label: String, hint: String, initial: String, check: Option<Check> },
    /// A hidden line, such as a token. OK refuses an empty one.
    Secret { label: String, hint: String },
}

#[derive(Default)]
pub struct Ask {
    pub heading: String,
    pub text: String,
    pub field: Option<Field>,
    /// A list's label, its entries and the one shown to begin with.
    pub combo: Option<(String, Vec<String>, String)>,
    /// One choice each; the first is chosen to begin with. Empty for a plain yes/no question.
    pub choices: Vec<String>,
    pub note: Option<(char, String)>,
    /// The OK button's label.
    pub ok: String,
    /// The Cancel button's label; "Cancel" when empty.
    pub cancel: String,
}

pub struct Reply {
    /// Index into [`Ask::choices`]; 0 when there were none.
    pub choice: usize,
    /// The text box's text, trimmed; empty when there was none or it was hidden.
    pub text: String,
    /// The hidden text, trimmed, for a [`Field::Secret`].
    pub secret: Option<Secret>,
    /// The list entry chosen.
    pub combo: String,
}

struct State {
    q: Question,
    edit: HWND,
    secret: bool,
    /// The hint while the text box is empty.
    plain: String,
    check: Option<Check>,
    done: bool,
    reply: Option<Reply>,
}

thread_local! {
    static OPEN: Cell<HWND> = const { Cell::new(null_mut()) };
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    /// The brush behind the text box's text, and its colour.
    static BRUSH: Cell<(HBRUSH, u32)> = const { Cell::new((null_mut(), 0)) };
}

/// The question window that is open, or null.
pub fn active() -> HWND {
    OPEN.with(Cell::get)
}

fn dpi_of(hwnd: HWND) -> u32 {
    match if hwnd.is_null() { 0 } else { unsafe { GetDpiForWindow(hwnd) } } {
        0 => unsafe { GetDpiForSystem() },
        dpi => dpi,
    }
}

fn hint_of(text: &str, plain: &str, check: Option<&Verify>) -> Hint {
    match check {
        Some(check) if !text.trim().is_empty() => match check(text.trim()) {
            Ok(meaning) => Hint::Ok(meaning),
            Err(e) => Hint::Err(e),
        },
        _ => Hint::Plain(plain.to_string()),
    }
}

/// Asks, and waits for the answer while the app keeps running. `None` when the user cancels, the window is closed,
/// or another question is already open (that one is brought to the front instead).
pub fn ask(owner: HWND, a: Ask) -> Option<Reply> {
    let open = active();
    if !open.is_null() {
        unsafe { SetForegroundWindow(open) };
        return None;
    }
    if !register_class(CLASS, Some(proc), null_mut()) || !control::register() {
        return None;
    }
    let (field, plain, initial, secret, check) = match a.field {
        Some(Field::Text { label, hint, initial, check }) => {
            let shown = hint_of(&initial, &hint, check.as_deref());
            (Some((label, shown)), hint, initial, false, check)
        }
        Some(Field::Secret { label, hint }) => (Some((label, Hint::Plain(hint.clone()))), hint, String::new(), true, None),
        None => (None, String::new(), String::new(), false, None),
    };
    let has_field = field.is_some();
    let q = Question {
        heading: a.heading,
        text: a.text,
        field,
        focused: has_field,
        combo: a.combo,
        choices: a.choices,
        choice: 0,
        note: a.note,
        ok: a.ok,
        cancel: a.cancel,
    };
    // Made where it will roughly end up, so it starts on the right monitor and DPI; sized again once it is there.
    let dpi = dpi_of(owner);
    let height = page::ask(&q, &|t: &str, f: Font, w: i32| paint::measure(t, f, w, dpi)).0.height;
    let [x, y, w, h] = place(STYLE, EX_STYLE, (ASK_W, height), dpi, owner);
    let hwnd = unsafe {
        CreateWindowExW(EX_STYLE, wide(CLASS).as_ptr(), wide(APP_NAME).as_ptr(), STYLE, x, y, w, h, owner, null_mut(), GetModuleHandleW(null()), null())
    };
    if hwnd.is_null() {
        return None;
    }
    theme::frame(hwnd);
    let panel = control::panel(hwnd, PANEL, Role::Card);
    fit_panel(hwnd);
    let edit = if has_field { make_edit(panel, &initial, secret, dpi_of(hwnd)) } else { null_mut() };
    STATE.with(|s| *s.borrow_mut() = Some(State { q, edit, secret, plain, check, done: false, reply: None }));
    OPEN.with(|o| o.set(hwnd));
    update(hwnd);
    control::hide_cues(hwnd);
    let focus = [edit, control::child(panel, ask_id::CHOICE), control::child(panel, ask_id::OK)].into_iter().find(|h| !h.is_null()).unwrap_or(hwnd);
    unsafe {
        if !owner.is_null() {
            EnableWindow(owner, 0);
        }
        ShowWindow(hwnd, SW_SHOWNORMAL);
        SetForegroundWindow(hwnd);
        SetFocus(focus);
        if focus == edit {
            SendMessageW(edit, EM_SETSEL, 0, -1);
        }
    }

    let done = || STATE.with(|s| s.try_borrow().is_ok_and(|s| s.as_ref().is_none_or(|s| s.done)));
    shell::pump(Some(&done));

    unsafe {
        // The owner is enabled first, so Windows activates it (not some other app) when the question goes away.
        if !owner.is_null() {
            EnableWindow(owner, 1);
        }
        if IsWindow(hwnd) != 0 {
            DestroyWindow(hwnd);
        }
    }
    OPEN.with(|o| o.set(null_mut()));
    let state = STATE.with(|s| s.borrow_mut().take());
    let (brush, _) = BRUSH.with(|b| b.replace((null_mut(), 0)));
    if !brush.is_null() {
        unsafe { DeleteObject(brush) };
    }
    if !owner.is_null() && unsafe { IsWindow(owner) } != 0 {
        unsafe { SetForegroundWindow(owner) };
    }
    state.and_then(|s| s.reply)
}

fn make_edit(panel: HWND, initial: &str, secret: bool, dpi: u32) -> HWND {
    let style = WS_CHILD | WS_VISIBLE | WS_TABSTOP | ES_AUTOHSCROLL as u32 | if secret { ES_PASSWORD as u32 } else { 0 };
    let edit = unsafe {
        CreateWindowExW(
            0,
            wide("EDIT").as_ptr(),
            wide(initial).as_ptr(),
            style,
            0,
            0,
            0,
            0,
            panel,
            ask_id::EDIT as usize as HMENU,
            GetModuleHandleW(null()),
            null(),
        )
    };
    if !edit.is_null() {
        unsafe { SendMessageW(edit, EM_LIMITTEXT, 4096, 0) };
        set_font(edit, dpi);
    }
    edit
}

fn set_font(edit: HWND, dpi: u32) {
    unsafe {
        SendMessageW(edit, WM_SETFONT, paint::font(Font::Body, dpi) as usize, 1);
        SendMessageW(edit, EM_SETMARGINS, EC_BOTH, 0);
    }
}

fn fit_panel(hwnd: HWND) {
    let mut rc: RECT = unsafe { zeroed() };
    unsafe {
        GetClientRect(hwnd, &mut rc);
        SetWindowPos(GetDlgItem(hwnd, PANEL), null_mut(), 0, 0, rc.right, rc.bottom, SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

/// Lays the question out again, after the user typed, chose or moved the focus.
fn update(hwnd: HWND) {
    let Some((q, edit)) = STATE.with(|s| s.try_borrow().ok()?.as_ref().filter(|s| !s.done).map(|s| (s.q.clone(), s.edit))) else { return };
    let dpi = dpi_of(hwnd);
    let (surface, edit_at) = page::ask(&q, &|t: &str, f: Font, w: i32| paint::measure(t, f, w, dpi));
    let mut rc = RECT { left: 0, top: 0, right: scale(ASK_W, dpi), bottom: scale(surface.height, dpi) };
    let mut now: RECT = unsafe { zeroed() };
    unsafe {
        AdjustWindowRectExForDpi(&mut rc, STYLE, 0, EX_STYLE, dpi);
        GetWindowRect(hwnd, &mut now);
    }
    let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
    if now.right - now.left != w || now.bottom - now.top != h {
        unsafe { SetWindowPos(hwnd, null_mut(), 0, 0, w, h, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE) };
    }
    control::set(unsafe { GetDlgItem(hwnd, PANEL) }, surface);
    if let (false, Some(at)) = (edit.is_null(), edit_at) {
        let [x, y, w, h] = at.map(|v| scale(v, dpi));
        // On top, so it comes first when Tab moves through the controls.
        unsafe { SetWindowPos(edit, HWND_TOP, x, y, w, h, SWP_NOACTIVATE) };
    }
}

fn with_state(f: impl FnOnce(&mut State)) {
    STATE.with(|s| {
        if let Ok(mut s) = s.try_borrow_mut() {
            if let Some(s) = s.as_mut().filter(|s| !s.done) {
                f(s);
            }
        }
    });
}

fn finish(reply: Option<Reply>) {
    with_state(|s| {
        s.done = true;
        s.reply = reply;
    });
}

/// Reads the hidden field and empties it, so the text does not stay in the control.
fn take_secret(edit: HWND) -> Secret {
    let mut buf = vec![0u16; unsafe { GetWindowTextLengthW(edit) }.max(0) as usize + 1];
    let got = unsafe { GetWindowTextW(edit, buf.as_mut_ptr(), buf.len() as i32) }.max(0) as usize;
    let text = Secret::new(String::from_utf16_lossy(&buf[..got]));
    for unit in &mut buf {
        unsafe { std::ptr::write_volatile(unit, 0) };
    }
    unsafe { SetWindowTextW(edit, wide("").as_ptr()) };
    text
}

fn refuse(edit: HWND) {
    unsafe {
        MessageBeep(MB_ICONWARNING);
        SetFocus(edit);
    }
}

fn accept() {
    let Some((edit, secret, combo, choice)) =
        STATE.with(|s| s.try_borrow().ok()?.as_ref().map(|s| (s.edit, s.secret, s.q.combo.as_ref().map(|c| c.2.clone()).unwrap_or_default(), s.q.choice)))
    else {
        return;
    };
    let mut reply = Reply { choice, text: String::new(), secret: None, combo };
    if !edit.is_null() && secret {
        let text = take_secret(edit);
        if text.expose().trim().is_empty() {
            refuse(edit);
            return;
        }
        reply.secret = Some(Secret::new(text.expose().trim()));
    } else if !edit.is_null() {
        let text = text_of(edit).trim().to_string();
        let wrong = STATE.with(|s| s.try_borrow().ok()?.as_ref()?.check.as_ref().map(|check| check(&text).is_err())).unwrap_or(false);
        if text.is_empty() || wrong {
            refuse(edit);
            return;
        }
        reply.text = text;
    }
    finish(Some(reply));
}

/// The colour behind the text box's text, as a brush; one is kept and made again when the colour changes.
fn brush(argb: u32) -> HBRUSH {
    BRUSH.with(|b| {
        let (old, color) = b.get();
        if !old.is_null() && color == argb {
            return old;
        }
        if !old.is_null() {
            unsafe { DeleteObject(old) };
        }
        let new = unsafe { CreateSolidBrush(theme::colorref(argb)) };
        b.set((new, argb));
        new
    })
}

fn on_command(hwnd: HWND, wp: WPARAM, lp: LPARAM) {
    let id = (wp & 0xFFFF) as i32;
    let code = ((wp >> 16) & 0xFFFF) as u32;
    match (id, code) {
        (IDOK, BN_CLICKED) => accept(),
        (IDCANCEL, BN_CLICKED) => finish(None),
        (ask_id::EDIT, EN_CHANGE) => {
            let edit = lp as HWND;
            let mut changed = false;
            with_state(|s| {
                // A hidden text is never read until OK.
                if !s.secret {
                    let hint = hint_of(&text_of(edit), &s.plain, s.check.as_deref());
                    if let Some((_, shown)) = s.q.field.as_mut().filter(|(_, shown)| *shown != hint) {
                        *shown = hint;
                        changed = true;
                    }
                }
            });
            if changed {
                update(hwnd);
            }
        }
        (ask_id::EDIT, EN_SETFOCUS | EN_KILLFOCUS) => {
            with_state(|s| s.q.focused = code == EN_SETFOCUS);
            update(hwnd);
            unsafe { RedrawWindow(lp as HWND, null(), null_mut(), RDW_INVALIDATE | RDW_ERASE | RDW_FRAME) };
        }
        (ask_id::COMBO, CBN_SELENDOK) => {
            let chosen = text_of(lp as HWND);
            with_state(|s| {
                if let Some(combo) = s.q.combo.as_mut() {
                    combo.2 = chosen;
                }
            });
        }
        (n, BN_CLICKED) if n >= ask_id::CHOICE => {
            with_state(|s| s.q.choice = (n - ask_id::CHOICE) as usize);
            update(hwnd);
        }
        _ => {}
    }
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            on_command(hwnd, wp, lp);
            0
        }
        WM_CTLCOLOREDIT => {
            let dc = wp as HDC;
            let bg = theme::color(if unsafe { GetFocus() } == lp as HWND { Role::Card } else { Role::Ctl });
            unsafe {
                SetTextColor(dc, theme::colorref(theme::color(Role::Text)));
                SetBkColor(dc, theme::colorref(bg));
            }
            brush(bg) as LRESULT
        }
        WM_ERASEBKGND => 1,
        WM_SIZE => {
            fit_panel(hwnd);
            0
        }
        WM_CLOSE | WM_DESTROY => {
            finish(None);
            0
        }
        WM_DPICHANGED => {
            let rc = unsafe { &*(lp as *const RECT) };
            unsafe { SetWindowPos(hwnd, null_mut(), rc.left, rc.top, rc.right - rc.left, rc.bottom - rc.top, SWP_NOZORDER | SWP_NOACTIVATE) };
            let edit = STATE.with(|s| s.try_borrow().ok()?.as_ref().map(|s| s.edit)).unwrap_or(null_mut());
            if !edit.is_null() {
                set_font(edit, (wp & 0xFFFF) as u32);
            }
            update(hwnd);
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}
