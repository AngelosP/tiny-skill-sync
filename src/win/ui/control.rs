//! The controls. One window class, [`CTL_CLASS`], draws every kind the app shows (its [`Look`]); a panel
//! ([`PANEL_CLASS`]) holds them, scrolls them and draws what is behind them: cards, dividers, icons and plain text.
//!
//! A page is described as a [`Surface`] and given to [`set`], which matches it against the controls already there by
//! id: a control that stays keeps its window, so the keyboard focus and the mouse hover survive a refresh. Each control
//! is a real child window with the id and the text of what it shows, so the tests (and screen readers) can find it.
//! A window costs about 0.15 ms to make, so only the controls near the part of the page shown are made ([`NEAR`]); the
//! others are made as the page scrolls to them, or when asked for by id ([`child`]).
//!
//! A control has no background of its own: it asks its parent to draw what is under it into the same bitmap first
//! ([`WM_PAINTBG`]), so rounded corners and tints blend with the card or page beneath.

use std::cell::RefCell;
use std::collections::HashMap;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, EndPaint, MapWindowPoints, PAINTSTRUCT, RDW_ALLCHILDREN, RDW_INVALIDATE, RedrawWindow,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::{
    SetScrollInfo, SetWindowTheme, TTF_IDISHWND, TTF_SUBCLASS, TTM_ADDTOOLW, TTM_SETMAXTIPWIDTH, TTS_ALWAYSTIP, TTS_NOPREFIX, TTTOOLINFOW, WM_MOUSELEAVE,
};
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, GetFocus, IsWindowEnabled, ReleaseCapture, SetCapture, SetFocus, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VK_RETURN, VK_SPACE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, BM_CLICK, CREATESTRUCTW, CreatePopupMenu, CreateWindowExW, DLGC_WANTMESSAGE, DefWindowProcW, DestroyMenu, DestroyWindow, GA_ROOT,
    GWLP_USERDATA, GetAncestor, GetClientRect, GetCursorPos, GetDlgCtrlID, GetDlgItem, GetParent, GetScrollInfo, GetWindowLongPtrW, GetWindowRect, HMENU,
    HTTRANSPARENT, HWND_TOP, IsChild, IsWindow, MF_CHECKED, MF_SEPARATOR, MF_STRING, MSG, PostMessageW, SB_BOTTOM, SB_LINEDOWN, SB_LINEUP, SB_PAGEDOWN,
    SB_PAGEUP, SB_THUMBTRACK, SB_TOP, SB_VERT, SCROLLINFO, SIF_PAGE, SIF_POS, SIF_RANGE, SIF_TRACKPOS, SPI_GETWHEELSCROLLLINES, SW_INVALIDATE,
    SW_SCROLLCHILDREN, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOREDRAW, SWP_NOSIZE, SWP_NOZORDER, ScrollWindowEx, SendMessageW, SetWindowLongPtrW, SetWindowPos,
    SetWindowTextW, SystemParametersInfoW, TPM_RETURNCMD, TPM_RIGHTALIGN, TrackPopupMenuEx, UIS_INITIALIZE, UISF_HIDEACCEL, UISF_HIDEFOCUS, WM_APP,
    WM_CAPTURECHANGED, WM_CHANGEUISTATE, WM_COMMAND, WM_CTLCOLOREDIT, WM_CTLCOLORSTATIC, WM_ENABLE, WM_ERASEBKGND, WM_GETDLGCODE, WM_KEYDOWN, WM_KILLFOCUS,
    WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCCREATE, WM_NCDESTROY, WM_NCHITTEST, WM_PAINT, WM_QUERYUISTATE,
    WM_SETFOCUS, WM_SETTEXT, WM_SIZE, WM_UPDATEUISTATE, WM_VSCROLL, WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_DISABLED, WS_EX_CONTROLPARENT,
    WS_EX_NOPARENTNOTIFY, WS_POPUP, WS_TABSTOP, WS_VISIBLE, WS_VSCROLL,
};

use super::paint::{self, Font, Rect, Tone, glyph};
use super::theme::{self, Role};
use super::{register_class, scale, text_of};
use crate::win::wide;

pub const CTL_CLASS: &str = "TinySkillSyncCtl";
pub const PANEL_CLASS: &str = "TinySkillSyncPanel";
/// Sent to a window to draw what is under one of its children into the canvas, whose origin is already set.
pub const WM_PAINTBG: u32 = WM_APP + 5;
/// From a control that got the focus to its panel, which scrolls it into view.
const WM_REVEAL: u32 = WM_APP + 6;
/// Posted to the top-level window when the mouse comes onto or leaves a control or a panel, so that it can follow where
/// the mouse is without asking all the time.
pub const WM_HOVER: u32 = WM_APP + 7;
pub const BN_CLICKED: u32 = 0;
/// The code a combo sends when an entry was chosen; the combo's text is then the entry.
pub const CBN_SELENDOK: u32 = 9;
const BM_GETCHECK: u32 = 0xF0;
const BM_SETCHECK: u32 = 0xF1;
/// How far above and below the part of a page shown its controls are made, in DIPs: about two screens, so the next
/// control the keyboard or the wheel goes to is always there, and a page of 500 skills opens as fast as one of 10.
const NEAR: i32 = 1200;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Standard,
    Accent,
    /// No fill until hovered, and only an icon.
    Icon,
    /// No fill until hovered, in the accent text colour: a quiet action inside content.
    Link,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mark {
    Dot(Tone),
    Glyph(char, Role),
}

/// What a control is and how it is drawn. Its text is the window text.
#[derive(Clone, PartialEq, Debug)]
pub enum Look {
    /// Text that cannot be clicked, with `DT_*` flags.
    Text {
        font: Font,
        ink: Role,
        flags: u32,
    },
    Button {
        kind: Kind,
        glyph: Option<char>,
    },
    /// An entry in a list to choose from: a mark, a line and an optional second line (see [`lines`]).
    Nav {
        mark: Mark,
        ink: Role,
        selected: bool,
    },
    /// A skill: its name, and its status in a pill (see [`lines`]).
    Row {
        tone: Tone,
        glyph: Option<char>,
    },
    Pill {
        tone: Tone,
        glyph: Option<char>,
    },
    /// An on/off switch; the text is what it is for, it shows "On" or "Off".
    Toggle {
        on: bool,
    },
    /// One of a set of choices; clicking it turns it on, and its owner turns the others off.
    Radio {
        on: bool,
    },
    /// A button that offers a list; the text is the entry shown.
    Combo {
        items: Vec<String>,
    },
}

impl Look {
    fn interactive(&self) -> bool {
        !matches!(self, Look::Text { .. } | Look::Pill { .. })
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct Item {
    pub id: i32,
    pub at: Rect,
    pub text: String,
    pub look: Look,
    pub enabled: bool,
}

/// What a panel draws itself, behind its controls.
#[derive(Clone, PartialEq, Debug)]
pub enum Deco {
    Fill { at: Rect, radius: i32, fill: Role, border: Role },
    Glyph { at: Rect, ch: char, size: u8, ink: Role },
    Text { at: Rect, text: String, font: Font, ink: Role, flags: u32 },
}

#[derive(Clone, PartialEq, Debug, Default)]
pub struct Surface {
    pub items: Vec<Item>,
    pub decos: Vec<Deco>,
    /// The height of everything, in DIPs; more than the panel shows makes it scroll.
    pub height: i32,
}

struct Ctl {
    look: Look,
    at: Rect,
    hover: bool,
    down: bool,
}

struct Panel {
    surface: Surface,
    /// One per item of the surface; null for an item not made yet (see [`NEAR`]).
    children: Vec<HWND>,
    bg: Role,
    scroll: i32,
    dpi: u32,
    /// The mouse is on the panel itself, not on one of its controls.
    hover: bool,
}

fn ctl_of(hwnd: HWND) -> Option<&'static RefCell<Ctl>> {
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const RefCell<Ctl>;
    // Only ours: the pointer was put there in WM_NCCREATE and lives until WM_NCDESTROY.
    (!ptr.is_null() && class_is(hwnd, CTL_CLASS)).then(|| unsafe { &*ptr })
}

fn panel_of(hwnd: HWND) -> Option<&'static RefCell<Panel>> {
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const RefCell<Panel>;
    (!ptr.is_null() && class_is(hwnd, PANEL_CLASS)).then(|| unsafe { &*ptr })
}

fn class_is(hwnd: HWND, class: &str) -> bool {
    let mut buf = [0u16; 32];
    let got = unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    got > 0 && buf[..got as usize].iter().copied().eq(class.encode_utf16())
}

pub fn register() -> bool {
    register_class(CTL_CLASS, Some(ctl_proc), null_mut()) && register_class(PANEL_CLASS, Some(panel_proc), null_mut())
}

fn dpi_of(hwnd: HWND) -> u32 {
    match unsafe { GetDpiForWindow(hwnd) } {
        0 => 96,
        dpi => dpi,
    }
}

/// Asks the parent of `hwnd` to draw what is under it into the canvas.
fn under(hwnd: HWND) {
    let parent = unsafe { GetParent(hwnd) };
    if parent.is_null() {
        return;
    }
    let mut pt = POINT { x: 0, y: 0 };
    unsafe { MapWindowPoints(hwnd, parent, &mut pt, 1) };
    let (x, y) = paint::origin();
    paint::set_origin(x + pt.x, y + pt.y);
    unsafe { SendMessageW(parent, WM_PAINTBG, 0, 0) };
    paint::set_origin(x, y);
}

/// Paints a window through the canvas: `draw` gets the canvas with its origin at the window's top-left pixel.
pub fn paint_with(hwnd: HWND, draw: &dyn Fn()) {
    let mut ps: PAINTSTRUCT = unsafe { zeroed() };
    let dc = unsafe { BeginPaint(hwnd, &mut ps) };
    let rc = ps.rcPaint;
    if paint::begin(dpi_of(hwnd), rc.right - rc.left, rc.bottom - rc.top, rc.left, rc.top) {
        draw();
        paint::end(dc, rc.left, rc.top);
    }
    unsafe { EndPaint(hwnd, &ps) };
}

fn notify(hwnd: HWND, code: u32) {
    let id = unsafe { GetDlgCtrlID(hwnd) } as u32;
    unsafe { SendMessageW(GetParent(hwnd), WM_COMMAND, ((code << 16) | (id & 0xFFFF)) as usize, hwnd as isize) };
}

/// The mouse came onto `hwnd`: asks for `WM_MOUSELEAVE` when it goes, and says so to the top-level window.
fn track_hover(hwnd: HWND) {
    let mut track = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
    unsafe { TrackMouseEvent(&mut track) };
    hovered(hwnd);
}

/// Posted, not sent: the control that says so may be in the middle of a change.
fn hovered(hwnd: HWND) {
    unsafe { PostMessageW(GetAncestor(hwnd, GA_ROOT), WM_HOVER, 0, 0) };
}

fn invalidate(hwnd: HWND) {
    unsafe { RedrawWindow(hwnd, null(), null_mut(), RDW_INVALIDATE) };
}

/// The keyboard focus is shown only once the keyboard was used, as Windows does.
fn focus_shown(hwnd: HWND) -> bool {
    unsafe { GetFocus() == hwnd && (SendMessageW(hwnd, WM_QUERYUISTATE, 0, 0) as u32 & UISF_HIDEFOCUS) == 0 }
}

/// Starts a top-level window with its focus ring hidden when it was opened with the mouse, as a dialog box does.
pub fn hide_cues(top: HWND) {
    let wp = UIS_INITIALIZE | (UISF_HIDEFOCUS | UISF_HIDEACCEL) << 16;
    unsafe { SendMessageW(top, WM_CHANGEUISTATE, wp as usize, 0) };
}

/// A list to choose from, under `anchor`. `items` are the entries ("-" is a separator); the chosen index, or `None`.
pub fn menu(anchor: HWND, items: &[&str], checked: Option<usize>, right: bool) -> Option<usize> {
    let mut rc: RECT = unsafe { zeroed() };
    unsafe { GetWindowRect(anchor, &mut rc) };
    let menu = unsafe { CreatePopupMenu() };
    if menu.is_null() {
        return None;
    }
    for (i, item) in items.iter().enumerate() {
        let flags = if *item == "-" { MF_SEPARATOR } else { MF_STRING | if checked == Some(i) { MF_CHECKED } else { 0 } };
        unsafe { AppendMenuW(menu, flags, i + 1, wide(item).as_ptr()) };
    }
    theme::menus();
    let x = if right { rc.right } else { rc.left };
    let flags = TPM_RETURNCMD | if right { TPM_RIGHTALIGN } else { 0 };
    let chosen = unsafe { TrackPopupMenuEx(menu, flags, x, rc.bottom + 2, anchor, null()) };
    unsafe { DestroyMenu(menu) };
    usize::try_from(chosen).ok().filter(|&c| c > 0).map(|c| c - 1)
}

/// Does what clicking the control does.
fn activate(hwnd: HWND) {
    if unsafe { IsWindowEnabled(hwnd) } == 0 {
        return;
    }
    let Some(cell) = ctl_of(hwnd) else { return };
    let look = match cell.try_borrow_mut() {
        Ok(mut c) => {
            match &mut c.look {
                Look::Toggle { on } => *on = !*on,
                Look::Radio { on } => *on = true,
                _ => {}
            }
            c.look.clone()
        }
        Err(_) => return,
    };
    match look {
        Look::Combo { items } => {
            let current = text_of(hwnd);
            let refs: Vec<&str> = items.iter().map(String::as_str).collect();
            let Some(chosen) = menu(hwnd, &refs, items.iter().position(|i| *i == current), false) else { return };
            if unsafe { IsWindow(hwnd) } == 0 {
                return;
            }
            unsafe { SetWindowTextW(hwnd, wide(&items[chosen]).as_ptr()) };
            notify(hwnd, CBN_SELENDOK);
        }
        _ => {
            invalidate(hwnd);
            notify(hwnd, BN_CLICKED);
        }
    }
}

/// The two lines of a list entry or a skill: its window text is both, so a screen reader reads the status with the name.
/// A name never holds a line break: names pass `printable` first.
pub fn lines(text: &str) -> (&str, &str) {
    text.split_once('\n').unwrap_or((text, ""))
}

/// The window text of an entry with a second line.
pub fn two_lines(first: &str, second: &str) -> String {
    if second.is_empty() { first.to_string() } else { format!("{first}\n{second}") }
}

fn draw_ctl(hwnd: HWND) {
    let Some(cell) = ctl_of(hwnd) else { return };
    let Ok(c) = cell.try_borrow() else { return };
    let (bx, by) = paint::origin();
    under(hwnd);
    let dpi = dpi_of(hwnd);
    paint::set_origin(bx + scale(c.at[0], dpi), by + scale(c.at[1], dpi));
    let text = text_of(hwnd);
    let enabled = unsafe { IsWindowEnabled(hwnd) } != 0;
    let [x, y, w, h] = c.at;
    let tint = if c.down && c.hover {
        Role::Sel
    } else if c.hover {
        Role::Hover
    } else {
        Role::None
    };
    match &c.look {
        Look::Text { font, ink, flags } => paint::text(c.at, &text, *font, *ink, *flags),
        Look::Button { kind, glyph } => {
            let (fill, border, ink) = match kind {
                Kind::Accent if enabled => (Role::Accent, Role::None, Role::OnAccent),
                Kind::Standard | Kind::Accent => (Role::Ctl, Role::CtlBorder, if enabled { Role::Text } else { Role::Text3 }),
                Kind::Icon => (Role::None, Role::None, if enabled { Role::Text } else { Role::Text3 }),
                Kind::Link => (Role::None, Role::None, if enabled { Role::AccentText } else { Role::Text3 }),
            };
            paint::fill(c.at, 4, fill, border);
            if matches!(kind, Kind::Standard | Kind::Accent) && !theme::contrast() {
                // The darker bottom edge Windows 11 buttons have.
                let bottom = if *kind == Kind::Accent && enabled { 0x40000000 } else { theme::color(Role::CtlBottom) };
                paint::fill_argb([x + 3, y + h - 1, w - 6, 1], 0, bottom, 0);
            }
            if enabled && tint != Role::None {
                paint::fill(c.at, 4, tint, Role::None);
            }
            let ink = if c.down && c.hover && !matches!(kind, Kind::Accent | Kind::Link) { Role::Text2 } else { ink };
            let size = if *kind == Kind::Icon {
                16
            } else if *kind == Kind::Link {
                12
            } else {
                14
            };
            let font = if *kind == Kind::Link { Font::Caption } else { Font::Body };
            let tw = if text.is_empty() || *kind == Kind::Icon { 0 } else { paint::measure(&text, font, 0, dpi).0 };
            let gw = if glyph.is_some() { i32::from(size) + if tw > 0 { 8 } else { 0 } } else { 0 };
            let left = x + (w - tw - gw) / 2;
            if let Some(ch) = glyph {
                paint::glyph([left, y, i32::from(size), h], *ch, size, ink);
            }
            if tw > 0 {
                paint::line([left + gw, y, tw + 2, h], &text, font, ink);
            }
        }
        Look::Nav { mark, ink, selected } => {
            let (text, sub) = lines(&text);
            let bg = if *selected { Role::Sel } else { tint };
            paint::fill(c.at, 4, bg, Role::None);
            if *selected {
                paint::fill([x, y + (h - 16) / 2, 3, 16], 2, Role::Accent, Role::None);
            }
            match mark {
                Mark::Dot(tone) => paint::dot([x + 12, y + (h - 16) / 2, 16, 16], *tone),
                Mark::Glyph(ch, role) => paint::glyph([x + 12, y, 16, h], *ch, 16, *role),
            }
            let left = x + 40;
            if sub.is_empty() {
                paint::line([left, y, w - left + x - 8, h], text, Font::Body, *ink);
            } else {
                let top = y + (h - 36) / 2;
                paint::line([left, top, w - left + x - 8, 20], text, Font::Body, *ink);
                paint::line([left, top + 20, w - left + x - 8, 16], sub, Font::Caption, Role::Text2);
            }
        }
        Look::Row { tone, glyph } => {
            let (text, tag) = lines(&text);
            paint::fill([x + 4, y + 3, w - 8, h - 6], 4, tint, Role::None);
            paint::glyph([x + 16, y, 16, h], glyph::FILE, 14, Role::Text3);
            let pw = if tag.is_empty() { 0 } else { paint::pill_width(tag, *glyph, dpi) };
            if pw > 0 {
                paint::pill(x + w - 16 - pw, y + (h - paint::PILL_H) / 2, tag, *tone, *glyph, dpi);
            }
            paint::line([x + 44, y, w - 44 - 16 - pw - 12, h], text, Font::Body, Role::Text);
        }
        Look::Pill { tone, glyph } => paint::pill(x, y + (h - paint::PILL_H) / 2, &text, *tone, *glyph, dpi),
        Look::Toggle { on } => {
            let track = [x + w - 40, y + (h - 20) / 2, 40, 20];
            let [tx, ty, ..] = track;
            if *on {
                paint::fill(track, 10, if enabled { Role::Accent } else { Role::Text3 }, Role::None);
                paint::fill([tx + 24, ty + 4, 12, 12], 6, Role::OnAccent, Role::None);
            } else {
                paint::fill(track, 10, tint, Role::Text2);
                paint::fill([tx + 4, ty + 4, 12, 12], 6, Role::Text2, Role::None);
            }
            paint::text([x, y, w - 52, h], if *on { "On" } else { "Off" }, Font::Body, Role::Text, DT_SINGLELINE | DT_VCENTER | DT_RIGHT);
        }
        Look::Radio { on } => {
            if tint != Role::None {
                paint::fill(c.at, 4, tint, Role::None);
            }
            let ring = [x + 6, y + (h - 20) / 2, 20, 20];
            let [rx, ry, ..] = ring;
            if *on {
                paint::fill(ring, 10, if enabled { Role::Accent } else { Role::Text3 }, Role::None);
                paint::fill([rx + 6, ry + 6, 8, 8], 4, Role::OnAccent, Role::None);
            } else {
                paint::fill(ring, 10, Role::Ctl, Role::Text2);
            }
            paint::text([x + 36, y, w - 44, h], &text, Font::Body, if enabled { Role::Text } else { Role::Text3 }, DT_VCENTER | DT_SINGLELINE);
        }
        Look::Combo { .. } => {
            paint::fill(c.at, 4, Role::Ctl, Role::CtlBorder);
            if !theme::contrast() {
                paint::fill_argb([x + 3, y + h - 1, w - 6, 1], 0, theme::color(Role::CtlBottom), 0);
            }
            if enabled && tint != Role::None {
                paint::fill(c.at, 4, tint, Role::None);
            }
            paint::line([x + 12, y, w - 12 - 32, h], &text, Font::Body, if enabled { Role::Text } else { Role::Text3 });
            paint::glyph([x + w - 32, y, 22, h], glyph::CHEVRON_DOWN, 12, Role::Text2);
        }
    }
    if focus_shown(hwnd) {
        let radius = if matches!(c.look, Look::Toggle { .. } | Look::Row { .. }) { 4 } else { 5 };
        paint::fill(c.at, radius, Role::None, Role::Text);
        paint::fill([x + 1, y + 1, w - 2, h - 2], radius - 1, Role::None, Role::Text);
    }
}

unsafe extern "system" fn ctl_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            let cs = unsafe { &*(lp as *const CREATESTRUCTW) };
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize) };
        }
        WM_NCDESTROY => {
            let ptr = unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) } as *mut RefCell<Ctl>;
            if !ptr.is_null() {
                drop(unsafe { Box::from_raw(ptr) });
            }
        }
        WM_NCHITTEST => {
            if ctl_of(hwnd).and_then(|c| c.try_borrow().ok().map(|c| c.look.interactive())) == Some(false) {
                return HTTRANSPARENT as LRESULT;
            }
        }
        WM_ERASEBKGND => return 1,
        WM_PAINT => {
            paint_with(hwnd, &|| draw_ctl(hwnd));
            return 0;
        }
        WM_MOUSEMOVE => {
            if let Some(Ok(mut c)) = ctl_of(hwnd).map(RefCell::try_borrow_mut) {
                if !c.hover {
                    c.hover = true;
                    track_hover(hwnd);
                    invalidate(hwnd);
                }
            }
            return 0;
        }
        WM_MOUSELEAVE => {
            if let Some(Ok(mut c)) = ctl_of(hwnd).map(RefCell::try_borrow_mut) {
                c.hover = false;
                invalidate(hwnd);
            }
            hovered(hwnd);
            return 0;
        }
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
            unsafe { SetFocus(hwnd) };
            if let Some(Ok(mut c)) = ctl_of(hwnd).map(RefCell::try_borrow_mut) {
                c.down = true;
                c.hover = true;
            }
            unsafe { SetCapture(hwnd) };
            invalidate(hwnd);
            return 0;
        }
        WM_LBUTTONUP => {
            let was_down = ctl_of(hwnd).and_then(|c| c.try_borrow().ok().map(|c| c.down && c.hover)).unwrap_or(false);
            unsafe { ReleaseCapture() };
            if was_down {
                activate(hwnd);
            }
            return 0;
        }
        WM_CAPTURECHANGED => {
            if let Some(Ok(mut c)) = ctl_of(hwnd).map(RefCell::try_borrow_mut) {
                c.down = false;
                // Still under the mouse? Hover is checked again on the next move.
                let mut pt = POINT { x: 0, y: 0 };
                let mut rc: RECT = unsafe { zeroed() };
                unsafe {
                    GetCursorPos(&mut pt);
                    GetWindowRect(hwnd, &mut rc);
                }
                c.hover = pt.x >= rc.left && pt.x < rc.right && pt.y >= rc.top && pt.y < rc.bottom;
            }
            invalidate(hwnd);
            return 0;
        }
        WM_GETDLGCODE => {
            let msg = lp as *const MSG;
            if !msg.is_null() && unsafe { (*msg).message == WM_KEYDOWN && (*msg).wParam == usize::from(VK_RETURN) } {
                return DLGC_WANTMESSAGE as LRESULT;
            }
            return 0;
        }
        WM_KEYDOWN if wp == usize::from(VK_RETURN) || wp == usize::from(VK_SPACE) => {
            activate(hwnd);
            return 0;
        }
        BM_CLICK => {
            activate(hwnd);
            return 0;
        }
        BM_GETCHECK => {
            return ctl_of(hwnd)
                .and_then(|c| c.try_borrow().ok().map(|c| matches!(c.look, Look::Toggle { on: true } | Look::Radio { on: true })))
                .unwrap_or(false) as LRESULT;
        }
        BM_SETCHECK => {
            if let Some(Ok(mut c)) = ctl_of(hwnd).map(RefCell::try_borrow_mut) {
                if let Look::Toggle { on } | Look::Radio { on } = &mut c.look {
                    *on = wp != 0;
                }
            }
            invalidate(hwnd);
            return 0;
        }
        WM_SETFOCUS => {
            unsafe { SendMessageW(GetParent(hwnd), WM_REVEAL, hwnd as usize, 0) };
            invalidate(hwnd);
        }
        WM_KILLFOCUS | WM_ENABLE => invalidate(hwnd),
        WM_UPDATEUISTATE | WM_SETTEXT => {
            let r = unsafe { DefWindowProcW(hwnd, msg, wp, lp) };
            invalidate(hwnd);
            return r;
        }
        WM_MOUSEWHEEL => return unsafe { SendMessageW(GetParent(hwnd), msg, wp, lp) },
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

/// Makes an empty panel. `bg` is its colour; `Role::None` lets its parent draw behind it.
pub fn panel(parent: HWND, id: i32, bg: Role) -> HWND {
    let state =
        Box::into_raw(Box::new(RefCell::new(Panel { surface: Surface::default(), children: Vec::new(), bg, scroll: 0, dpi: dpi_of(parent), hover: false })));
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_CONTROLPARENT | WS_EX_NOPARENTNOTIFY,
            wide(PANEL_CLASS).as_ptr(),
            null(),
            WS_CHILD | WS_VISIBLE | WS_CLIPCHILDREN | WS_CLIPSIBLINGS | WS_VSCROLL,
            0,
            0,
            0,
            0,
            parent,
            id as usize as HMENU,
            GetModuleHandleW(null()),
            state.cast(),
        )
    };
    if hwnd.is_null() {
        drop(unsafe { Box::from_raw(state) });
    } else {
        scrollbar_theme(hwnd);
        update_scrollbar(hwnd);
    }
    hwnd
}

/// Dark scroll bars in dark mode.
pub fn scrollbar_theme(hwnd: HWND) {
    let name = if theme::dark() && !theme::contrast() { "DarkMode_Explorer" } else { "Explorer" };
    unsafe { SetWindowTheme(hwnd, wide(name).as_ptr(), null()) };
}

fn pixels(at: Rect, scroll: i32, dpi: u32) -> (i32, i32, i32, i32) {
    let s = |v| scale(v, dpi);
    let (x, y) = (s(at[0]), s(at[1]));
    (x, y - scroll, s(at[0] + at[2]) - x, s(at[1] + at[3]) - y)
}

fn place_child(child: HWND, at: Rect, scroll: i32, dpi: u32, flags: u32) {
    let (x, y, w, h) = pixels(at, scroll, dpi);
    unsafe { SetWindowPos(child, null_mut(), x, y, w, h, flags | SWP_NOZORDER | SWP_NOACTIVATE) };
}

/// Is `at` (DIPs) within [`NEAR`] of the part of the panel shown: `page` pixels from `scroll`?
fn near(at: Rect, scroll: i32, page: i32, dpi: u32) -> bool {
    let dips = |px: i32| px * 96 / dpi.max(1) as i32;
    at[1] + at[3] > dips(scroll) - NEAR && at[1] < dips(scroll + page) + NEAR
}

/// Shows a surface: controls that stay are updated in place, those near the view made, the rest destroyed. A control is
/// compared with what it shows now, not with what it was last given, so a toggle or combo the user changed is put right
/// too.
pub fn set(panel: HWND, surface: Surface) {
    let Some(cell) = panel_of(panel) else { return };
    let page = client_height(panel);
    let Ok(mut p) = cell.try_borrow_mut() else { return };
    let dpi = dpi_of(panel);
    let moved_dpi = dpi != p.dpi;
    p.dpi = dpi;
    let mut changed = moved_dpi || p.surface.decos != surface.decos || p.surface.height != surface.height;
    let old_items = std::mem::take(&mut p.surface.items);
    let index: HashMap<i32, usize> = old_items.iter().enumerate().map(|(i, item)| (item.id, i)).collect();
    let mut old: Vec<Option<(HWND, Item)>> = std::mem::take(&mut p.children).into_iter().zip(old_items).map(Some).collect();
    let mut children = Vec::with_capacity(surface.items.len());
    let (mut reorder, mut last, mut after) = (false, 0, HWND_TOP);
    for item in &surface.items {
        let kept = index.get(&item.id).and_then(|&i| old[i].take().map(|o| (i, o)));
        let hwnd = match kept {
            Some((pos, (hwnd, before))) if !hwnd.is_null() => {
                reorder |= pos < last;
                last = pos;
                if before.at != item.at || moved_dpi {
                    place_child(hwnd, item.at, p.scroll, dpi, SWP_NOREDRAW);
                    changed = true;
                }
                if text_of(hwnd) != item.text {
                    unsafe { SetWindowTextW(hwnd, wide(&item.text).as_ptr()) };
                    changed = true;
                }
                if let Some(Ok(mut c)) = ctl_of(hwnd).map(RefCell::try_borrow_mut) {
                    if c.look != item.look || c.at != item.at {
                        c.look = item.look.clone();
                        c.at = item.at;
                        changed = true;
                    }
                }
                if (unsafe { IsWindowEnabled(hwnd) } != 0) != item.enabled {
                    unsafe { EnableWindow(hwnd, item.enabled.into()) };
                    changed = true;
                }
                hwnd
            }
            _ if near(item.at, p.scroll, page, dpi) => {
                changed = true;
                make_ctl(panel, item, p.scroll, dpi, after)
            }
            _ => null_mut(),
        };
        if !hwnd.is_null() {
            after = hwnd;
        }
        children.push(hwnd);
    }
    p.children = children.clone();
    p.surface = surface;
    drop(p);
    let focus = unsafe { GetFocus() };
    let mut lost_focus = false;
    for (hwnd, _) in old.into_iter().flatten().filter(|(hwnd, _)| !hwnd.is_null()) {
        lost_focus |= hwnd == focus;
        changed = true;
        unsafe { DestroyWindow(hwnd) };
    }
    if reorder {
        let mut after = HWND_TOP;
        for &child in children.iter().filter(|h| !h.is_null()) {
            unsafe { SetWindowPos(child, after, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOREDRAW) };
            after = child;
        }
    }
    if !changed {
        return;
    }
    update_scrollbar(panel);
    if lost_focus {
        // The control with the focus went away: the first one that can take it gets it, so the keyboard still works.
        if let Some(first) = first_tab_stop(panel) {
            unsafe { SetFocus(first) };
        }
    }
    unsafe { RedrawWindow(panel, null(), null_mut(), RDW_INVALIDATE | RDW_ALLCHILDREN) };
}

fn first_tab_stop(panel: HWND) -> Option<HWND> {
    let p = panel_of(panel)?.try_borrow().ok()?;
    p.children.iter().zip(&p.surface.items).find(|(h, i)| !h.is_null() && i.enabled && i.look.interactive()).map(|(h, _)| *h)
}

/// Makes the control for `item`, in the tab order just after `after` (first, given `HWND_TOP`).
fn make_ctl(panel: HWND, item: &Item, scroll: i32, dpi: u32, after: HWND) -> HWND {
    let state = Box::into_raw(Box::new(RefCell::new(Ctl { look: item.look.clone(), at: item.at, hover: false, down: false })));
    let mut style = WS_CHILD | WS_VISIBLE;
    if item.look.interactive() {
        style |= WS_TABSTOP;
    }
    if !item.enabled {
        style |= WS_DISABLED;
    }
    let (x, y, w, h) = pixels(item.at, scroll, dpi);
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_NOPARENTNOTIFY,
            wide(CTL_CLASS).as_ptr(),
            wide(&item.text).as_ptr(),
            style,
            x,
            y,
            w,
            h,
            panel,
            item.id as usize as HMENU,
            GetModuleHandleW(null()),
            state.cast(),
        )
    };
    if hwnd.is_null() {
        drop(unsafe { Box::from_raw(state) });
        return hwnd;
    }
    unsafe { SetWindowPos(hwnd, after, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOREDRAW) };
    hwnd
}

/// Makes the controls the view has come near since [`set`], after a scroll or a resize.
fn fill(panel: HWND) {
    let Some(cell) = panel_of(panel) else { return };
    let page = client_height(panel);
    let Ok(mut p) = cell.try_borrow_mut() else { return };
    let (scroll, dpi) = (p.scroll, p.dpi);
    let Panel { surface, children, .. } = &mut *p;
    let mut after = HWND_TOP;
    for (child, item) in children.iter_mut().zip(&surface.items) {
        if child.is_null() && near(item.at, scroll, page, dpi) {
            *child = make_ctl(panel, item, scroll, dpi, after);
        }
        if !child.is_null() {
            after = *child;
        }
    }
}

/// A control of a panel, by id, made now if it is not near the view yet; null when the panel shows no such control.
pub fn child(panel: HWND, id: i32) -> HWND {
    let found = unsafe { GetDlgItem(panel, id) };
    if !found.is_null() {
        return found;
    }
    let Some(Ok(mut p)) = panel_of(panel).map(RefCell::try_borrow_mut) else { return found };
    let Some(i) = p.surface.items.iter().position(|item| item.id == id) else { return found };
    let after = p.children[..i].iter().rev().copied().find(|h| !h.is_null()).unwrap_or(HWND_TOP);
    let made = make_ctl(panel, &p.surface.items[i], p.scroll, p.dpi, after);
    p.children[i] = made;
    made
}

/// Scrolls back to the top, for a new page.
pub fn scroll_top(panel: HWND) {
    scroll_to(panel, 0);
}

fn client_height(hwnd: HWND) -> i32 {
    let mut rc: RECT = unsafe { zeroed() };
    unsafe { GetClientRect(hwnd, &mut rc) };
    rc.bottom
}

fn update_scrollbar(panel: HWND) {
    let Some((height, scroll)) = panel_of(panel).and_then(|c| c.try_borrow().ok().map(|p| (scale(p.surface.height, p.dpi), p.scroll))) else { return };
    let page = client_height(panel);
    let si = SCROLLINFO {
        cbSize: size_of::<SCROLLINFO>() as u32,
        fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
        nMin: 0,
        nMax: height.max(1) - 1,
        nPage: page.max(0) as u32,
        nPos: scroll,
        nTrackPos: 0,
    };
    unsafe { SetScrollInfo(panel, SB_VERT, &si, 1) };
    // A page that got shorter, or a window that got taller, may leave it scrolled past the end.
    let max = (height - page).max(0);
    if scroll > max {
        scroll_to(panel, max);
    }
}

fn scroll_to(panel: HWND, to: i32) {
    let Some(cell) = panel_of(panel) else { return };
    let page = client_height(panel);
    let (delta, now, see_through) = {
        let Ok(mut p) = cell.try_borrow_mut() else { return };
        let max = (scale(p.surface.height, p.dpi) - page).max(0);
        let to = to.clamp(0, max);
        let delta = p.scroll - to;
        p.scroll = to;
        (delta, to, p.bg == Role::None)
    };
    if delta == 0 {
        return;
    }
    let si = SCROLLINFO { cbSize: size_of::<SCROLLINFO>() as u32, fMask: SIF_POS, nMin: 0, nMax: 0, nPage: 0, nPos: now, nTrackPos: 0 };
    unsafe {
        ScrollWindowEx(panel, 0, delta, null(), null(), null_mut(), null_mut(), SW_SCROLLCHILDREN | SW_INVALIDATE);
        SetScrollInfo(panel, SB_VERT, &si, 1);
        if see_through {
            // What the parent draws behind it stays where it is, so copying the pixels would move it too.
            RedrawWindow(panel, null(), null_mut(), RDW_INVALIDATE | RDW_ALLCHILDREN);
        }
    }
    fill(panel);
}

fn scroll_by(panel: HWND, delta: i32) {
    let now = panel_of(panel).and_then(|c| c.try_borrow().ok().map(|p| p.scroll)).unwrap_or(0);
    scroll_to(panel, now + delta);
}

/// Can it scroll at all?
fn scrolls(panel: HWND) -> bool {
    panel_of(panel).and_then(|c| c.try_borrow().ok().map(|p| scale(p.surface.height, p.dpi) > client_height(panel))).unwrap_or(false)
}

fn on_vscroll(panel: HWND, code: i32) {
    let dpi = dpi_of(panel);
    let page = client_height(panel);
    let line = scale(48, dpi);
    match code {
        SB_LINEUP => scroll_by(panel, -line),
        SB_LINEDOWN => scroll_by(panel, line),
        SB_PAGEUP => scroll_by(panel, -page),
        SB_PAGEDOWN => scroll_by(panel, page),
        SB_TOP => scroll_to(panel, 0),
        SB_BOTTOM => scroll_to(panel, i32::MAX),
        SB_THUMBTRACK => {
            let mut si = SCROLLINFO { cbSize: size_of::<SCROLLINFO>() as u32, fMask: SIF_TRACKPOS, nMin: 0, nMax: 0, nPage: 0, nPos: 0, nTrackPos: 0 };
            unsafe { GetScrollInfo(panel, SB_VERT, &mut si) };
            scroll_to(panel, si.nTrackPos);
        }
        _ => {}
    }
}

/// Scrolls a child into view when the keyboard moved to it.
fn reveal(panel: HWND, child: HWND) {
    if unsafe { IsChild(panel, child) } == 0 {
        return;
    }
    let mut rc: RECT = unsafe { zeroed() };
    unsafe { GetWindowRect(child, &mut rc) };
    let mut pt = [POINT { x: rc.left, y: rc.top }, POINT { x: rc.right, y: rc.bottom }];
    unsafe { MapWindowPoints(null_mut(), panel, pt.as_mut_ptr(), 2) };
    let page = client_height(panel);
    let pad = scale(12, dpi_of(panel));
    if pt[0].y < 0 {
        scroll_by(panel, pt[0].y - pad);
    } else if pt[1].y > page {
        scroll_by(panel, (pt[1].y - page + pad).min(pt[0].y));
    }
}

fn draw_panel(hwnd: HWND) {
    let Some(cell) = panel_of(hwnd) else { return };
    let Ok(p) = cell.try_borrow() else { return };
    let (bx, by) = paint::origin();
    if p.bg == Role::None {
        under(hwnd);
    } else {
        let mut rc: RECT = unsafe { zeroed() };
        unsafe { GetClientRect(hwnd, &mut rc) };
        let dips = |v: i32| (v * 96 + p.dpi as i32 - 1) / p.dpi as i32;
        paint::set_origin(bx, by);
        paint::fill([0, 0, dips(rc.right), dips(rc.bottom)], 0, p.bg, Role::None);
    }
    paint::set_origin(bx, by + p.scroll);
    draw(&p.surface.decos);
    paint::set_origin(bx, by);
}

/// Draws decorations into the canvas at its current origin, skipping those outside what is being painted.
pub fn draw(decos: &[Deco]) {
    for deco in decos {
        let (Deco::Fill { at, .. } | Deco::Glyph { at, .. } | Deco::Text { at, .. }) = deco;
        if !paint::visible(*at) {
            continue;
        }
        match deco {
            Deco::Fill { at, radius, fill, border } => paint::fill(*at, *radius, *fill, *border),
            Deco::Glyph { at, ch, size, ink } => paint::glyph(*at, *ch, *size, *ink),
            Deco::Text { at, text, font, ink, flags } => paint::text(*at, text, *font, *ink, *flags),
        }
    }
}

unsafe extern "system" fn panel_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            let cs = unsafe { &*(lp as *const CREATESTRUCTW) };
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize) };
        }
        WM_NCDESTROY => {
            let ptr = unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) } as *mut RefCell<Panel>;
            if !ptr.is_null() {
                drop(unsafe { Box::from_raw(ptr) });
            }
        }
        WM_ERASEBKGND => return 1,
        WM_PAINT => {
            paint_with(hwnd, &|| draw_panel(hwnd));
            return 0;
        }
        WM_PAINTBG => {
            draw_panel(hwnd);
            return 0;
        }
        // A native edit box inside a panel is coloured by the window that owns the panel.
        WM_COMMAND | WM_CTLCOLOREDIT | WM_CTLCOLORSTATIC => return unsafe { SendMessageW(GetParent(hwnd), msg, wp, lp) },
        WM_SIZE => {
            update_scrollbar(hwnd);
            fill(hwnd);
        }
        WM_VSCROLL => {
            on_vscroll(hwnd, (wp & 0xFFFF) as i32);
            return 0;
        }
        WM_MOUSEWHEEL => {
            if !scrolls(hwnd) {
                return unsafe { SendMessageW(GetParent(hwnd), msg, wp, lp) };
            }
            let notches = f32::from((wp >> 16) as u16 as i16) / 120.0;
            let mut lines = 3u32;
            unsafe { SystemParametersInfoW(SPI_GETWHEELSCROLLLINES, 0, (&raw mut lines).cast(), 0) };
            let step = scale(16, dpi_of(hwnd)) * lines.clamp(1, 10) as i32;
            scroll_by(hwnd, -(notches * step as f32) as i32);
            return 0;
        }
        WM_REVEAL => {
            reveal(hwnd, wp as HWND);
            return 0;
        }
        WM_MOUSEMOVE => {
            if let Some(Ok(mut p)) = panel_of(hwnd).map(RefCell::try_borrow_mut) {
                if !p.hover {
                    p.hover = true;
                    track_hover(hwnd);
                }
            }
        }
        WM_MOUSELEAVE => {
            if let Some(Ok(mut p)) = panel_of(hwnd).map(RefCell::try_borrow_mut) {
                p.hover = false;
            }
            hovered(hwnd);
            return 0;
        }
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

/// A tooltip for a control.
pub fn tip(ctl: HWND, text: &str) {
    let tip = unsafe {
        CreateWindowExW(
            0,
            wide("tooltips_class32").as_ptr(),
            null(),
            WS_POPUP | TTS_ALWAYSTIP | TTS_NOPREFIX,
            0,
            0,
            0,
            0,
            ctl,
            null_mut(),
            GetModuleHandleW(null()),
            null(),
        )
    };
    if tip.is_null() {
        return;
    }
    let mut text = wide(text);
    let mut info: TTTOOLINFOW = unsafe { zeroed() };
    info.cbSize = size_of::<TTTOOLINFOW>() as u32;
    info.uFlags = TTF_IDISHWND | TTF_SUBCLASS;
    info.hwnd = ctl;
    info.uId = ctl as usize;
    info.lpszText = text.as_mut_ptr();
    unsafe {
        SendMessageW(tip, TTM_SETMAXTIPWIDTH, 0, scale(320, dpi_of(ctl)) as isize);
        SendMessageW(tip, TTM_ADDTOOLW, 0, (&raw const info) as isize);
    }
    scrollbar_theme(tip);
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::{GW_CHILD, GW_HWNDNEXT, GetWindow, WS_OVERLAPPEDWINDOW};

    const ROWS: i32 = 463;
    const FIRST: i32 = 1000;

    /// A page of skills as long as the longest repository measured (github/awesome-copilot).
    fn long_page() -> Surface {
        let row = |i: i32| Item {
            id: FIRST + i,
            at: [0, i * 48, 560, 44],
            text: format!("skill {i}"),
            look: Look::Row { tone: Tone::Ok, glyph: None },
            enabled: true,
        };
        Surface { items: (0..ROWS).map(row).collect(), decos: Vec::new(), height: ROWS * 48 }
    }

    /// The ids of the controls a panel has, in tab order.
    fn made(panel: HWND) -> Vec<i32> {
        let mut ids = Vec::new();
        let mut child = unsafe { GetWindow(panel, GW_CHILD) };
        while !child.is_null() {
            ids.push(unsafe { GetDlgCtrlID(child) });
            child = unsafe { GetWindow(child, GW_HWNDNEXT) };
        }
        ids
    }

    fn in_order(ids: &[i32]) -> bool {
        ids.windows(2).all(|w| w[0] < w[1])
    }

    #[test]
    fn a_long_page_makes_only_the_controls_near_the_view() {
        register();
        let top = unsafe {
            CreateWindowExW(0, wide("STATIC").as_ptr(), null(), WS_OVERLAPPEDWINDOW, 0, 0, 800, 600, null_mut(), null_mut(), GetModuleHandleW(null()), null())
        };
        assert!(!top.is_null());
        let panel = panel(top, 1, Role::Bg);
        unsafe { SetWindowPos(panel, null_mut(), 0, 0, 600, 500, SWP_NOZORDER | SWP_NOACTIVATE) };

        set(panel, long_page());
        let opened = made(panel);
        assert!(opened.len() < 100, "{} controls made for a page that shows about 10", opened.len());
        assert_eq!(opened.first(), Some(&FIRST));
        assert!(in_order(&opened), "{opened:?}");

        scroll_to(panel, i32::MAX);
        let scrolled = made(panel);
        let (last, middle) = (FIRST + ROWS - 1, FIRST + ROWS / 2);
        assert!(scrolled.contains(&last) && !scrolled.contains(&middle), "{scrolled:?}");
        assert!(in_order(&scrolled), "{scrolled:?}");

        assert!(!child(panel, middle).is_null(), "a control asked for by id is made");
        let asked = made(panel);
        assert!(asked.contains(&middle) && in_order(&asked), "{asked:?}");

        set(panel, long_page());
        assert_eq!(made(panel), asked, "the same page again keeps what was made and makes nothing more");
        unsafe { DestroyWindow(top) };
    }
}
