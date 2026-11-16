//! A small modal question: a heading, some text, optional choices, an optional hidden text field, and OK/Cancel.
//!
//! A custom window, not `TaskDialogIndirect`: that lives only in comctl32 version 6, which the test executables do not
//! load, so importing it would stop them from starting.

use std::cell::{Cell, RefCell};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BS_AUTORADIOBUTTON, BS_DEFPUSHBUTTON, BS_MULTILINE, CreateWindowExW, DefWindowProcW, DestroyWindow, ES_PASSWORD, GetWindowTextLengthW, GetWindowTextW,
    IDCANCEL, IDOK, IsWindow, MB_ICONWARNING, SW_SHOWNORMAL, SWP_NOACTIVATE, SWP_NOZORDER, SetForegroundWindow, SetWindowPos, SetWindowTextW, ShowWindow,
    WM_CLOSE, WM_COMMAND, WM_DESTROY, WM_DPICHANGED, WS_CAPTION, WS_EX_CONTROLPARENT, WS_EX_DLGMODALFRAME, WS_GROUP, WS_POPUP, WS_SYSMENU, WS_TABSTOP,
};

use super::{Form, checked, place, register_class, set_checked, shell};
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
/// Client width, in DIPs.
const WIDTH: i32 = 400;
const MARGIN: i32 = 14;
const BUTTON: (i32, i32) = (84, 26);
/// The room a radio button's circle takes before its text, in DIPs.
const RADIO_INDENT: i32 = 20;
const ID_SECRET: i32 = 10;
const ID_CHOICE: i32 = 20;

pub struct Ask<'a> {
    pub title: &'a str,
    pub heading: &'a str,
    pub text: &'a str,
    /// One radio button each; the first is chosen to begin with. Empty for a plain yes/no question.
    pub choices: &'a [&'a str],
    /// Ask for a hidden line of text, such as a token. OK refuses an empty one.
    pub secret: bool,
    /// The OK button's label.
    pub ok: &'a str,
}

pub struct Reply {
    /// Index into [`Ask::choices`]; 0 when there were none.
    pub choice: usize,
    /// The hidden text, trimmed, when [`Ask::secret`] was set.
    pub secret: Option<Secret>,
}

struct State {
    form: Form,
    radios: Vec<HWND>,
    secret: HWND,
    done: bool,
    reply: Option<Reply>,
}

thread_local! {
    static OPEN: Cell<HWND> = const { Cell::new(null_mut()) };
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// The question window that is open, or null.
pub fn active() -> HWND {
    OPEN.with(Cell::get)
}

/// Asks, and waits for the answer while the app keeps running. `None` when the user cancels, the window is closed,
/// or another question is already open (that one is brought to the front instead).
pub fn ask(owner: HWND, q: &Ask) -> Option<Reply> {
    let open = active();
    if !open.is_null() {
        unsafe { SetForegroundWindow(open) };
        return None;
    }
    if !register_class(CLASS, Some(proc), null_mut()) {
        return None;
    }
    let dpi = match if owner.is_null() { 0 } else { unsafe { GetDpiForWindow(owner) } } {
        0 => unsafe { GetDpiForSystem() },
        dpi => dpi,
    };
    // Made where it will roughly end up, so it starts on the right monitor and DPI; sized once its text is measured.
    let [x, y, w, h] = place(STYLE, EX_STYLE, (WIDTH, 200), dpi, owner);
    let hwnd = unsafe {
        CreateWindowExW(EX_STYLE, wide(CLASS).as_ptr(), wide(q.title).as_ptr(), STYLE, x, y, w, h, owner, null_mut(), GetModuleHandleW(null()), null())
    };
    if hwnd.is_null() {
        return None;
    }
    let mut form = Form::new(hwnd);
    let width = WIDTH - 2 * MARGIN;
    let mut top = MARGIN;

    let height = form.measure(q.heading, width, true);
    let heading = form.label(q.heading, [MARGIN, top, width, height]);
    form.embolden(heading);
    top += height + 8;
    if !q.text.is_empty() {
        let height = form.measure(q.text, width, false);
        form.label(q.text, [MARGIN, top, width, height]);
        top += height + 10;
    }
    let mut radios = Vec::new();
    for (i, choice) in q.choices.iter().enumerate() {
        let height = form.measure(choice, width - RADIO_INDENT, false).max(20);
        let first = if i == 0 { WS_GROUP | WS_TABSTOP } else { 0 };
        let style = first | (BS_AUTORADIOBUTTON | BS_MULTILINE) as u32;
        radios.push(form.add("BUTTON", choice, style, 0, [MARGIN, top, width, height], ID_CHOICE + i as i32));
        top += height + 4;
    }
    if let Some(first) = radios.first() {
        set_checked(*first, true);
        top += 6;
    }
    let secret = if q.secret {
        let edit = form.edit([MARGIN, top, width, 24], ID_SECRET, WS_GROUP | ES_PASSWORD as u32);
        top += 34;
        edit
    } else {
        null_mut()
    };
    let ok = form.add("BUTTON", q.ok, WS_GROUP | WS_TABSTOP | BS_DEFPUSHBUTTON as u32, 0, [WIDTH - MARGIN - 2 * BUTTON.0 - 8, top, BUTTON.0, BUTTON.1], IDOK);
    form.button("Cancel", [WIDTH - MARGIN - BUTTON.0, top, BUTTON.0, BUTTON.1], IDCANCEL);
    top += BUTTON.1 + MARGIN;

    let [x, y, w, h] = place(STYLE, EX_STYLE, (WIDTH, top), form.dpi, owner);
    let focus = if !secret.is_null() { secret } else { radios.first().copied().unwrap_or(ok) };
    STATE.with(|s| *s.borrow_mut() = Some(State { form, radios, secret, done: false, reply: None }));
    OPEN.with(|o| o.set(hwnd));
    unsafe {
        SetWindowPos(hwnd, null_mut(), x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
        if !owner.is_null() {
            EnableWindow(owner, 0);
        }
        ShowWindow(hwnd, SW_SHOWNORMAL);
        SetForegroundWindow(hwnd);
        SetFocus(focus);
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
    if !owner.is_null() && unsafe { IsWindow(owner) } != 0 {
        unsafe { SetForegroundWindow(owner) };
    }
    state.and_then(|s| s.reply)
}

fn finish(reply: Option<Reply>) {
    STATE.with(|s| {
        if let Ok(mut s) = s.try_borrow_mut() {
            if let Some(s) = s.as_mut() {
                if !s.done {
                    s.done = true;
                    s.reply = reply;
                }
            }
        }
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

fn accept() {
    let Some((edit, radios)) = STATE.with(|s| s.try_borrow().ok()?.as_ref().map(|s| (s.secret, s.radios.clone()))) else { return };
    let mut secret = None;
    if !edit.is_null() {
        let text = take_secret(edit);
        if text.expose().trim().is_empty() {
            unsafe {
                MessageBeep(MB_ICONWARNING);
                SetFocus(edit);
            }
            return;
        }
        secret = Some(Secret::new(text.expose().trim()));
    }
    let choice = radios.iter().position(|r| checked(*r)).unwrap_or(0);
    finish(Some(Reply { choice, secret }));
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            match (wp & 0xFFFF) as i32 {
                IDOK => accept(),
                IDCANCEL => finish(None),
                _ => {}
            }
            0
        }
        WM_CLOSE | WM_DESTROY => {
            finish(None);
            0
        }
        WM_DPICHANGED => {
            STATE.with(|s| {
                if let Ok(mut s) = s.try_borrow_mut() {
                    if let Some(s) = s.as_mut() {
                        s.form.set_dpi((wp & 0xFFFF) as u32);
                    }
                }
            });
            let rc = unsafe { &*(lp as *const RECT) };
            unsafe { SetWindowPos(hwnd, null_mut(), rc.left, rc.top, rc.right - rc.left, rc.bottom - rc.top, SWP_NOZORDER | SWP_NOACTIVATE) };
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}
