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

use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, EndPaint, MapWindowPoints, PAINTSTRUCT, RDW_ALLCHILDREN, RDW_INVALIDATE, RedrawWindow,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::{
    SetScrollInfo, SetWindowTheme, TTF_IDISHWND, TTF_SUBCLASS, TTM_ADDTOOLW, TTM_DELTOOLW, TTM_RELAYEVENT, TTM_SETMAXTIPWIDTH, TTS_ALWAYSTIP, TTS_NOPREFIX,
    TTTOOLINFOW, WM_MOUSELEAVE,
};
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, GetCapture, GetFocus, GetKeyState, IsWindowEnabled, ReleaseCapture, SetCapture, SetFocus, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
    VK_APPS, VK_F10, VK_RETURN, VK_SHIFT, VK_SPACE,
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
    WM_SETFOCUS, WM_SETTEXT, WM_SIZE, WM_SYSKEYDOWN, WM_UPDATEUISTATE, WM_VSCROLL, WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_DISABLED,
    WS_EX_CONTROLPARENT, WS_EX_NOPARENTNOTIFY, WS_POPUP, WS_TABSTOP, WS_VISIBLE, WS_VSCROLL,
};

use super::page::{self, Hit};
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
/// The code a combo sends after updating its window text; [`combo_value`] reads the chosen entry.
pub const CBN_SELENDOK: u32 = 9;
/// Ask the owner to show *all* currently available commands, including those shown inline.
pub const ROW_MENU: u32 = 0x110;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum RowCmd {
    Open = 0x100,
    Preview,
    CopyPath,
    CopyLink,
    OnGitHub,
    Compare,
    ManageAgain,
}

impl RowCmd {
    pub fn label(self) -> &'static str {
        match self {
            Self::Open => "Open folder",
            Self::Preview => "Preview",
            Self::CopyPath => "Copy path",
            Self::CopyLink => "Copy GitHub link",
            Self::OnGitHub => "Open on GitHub",
            Self::Compare => "Show changes",
            Self::ManageAgain => "Manage again",
        }
    }

    pub fn code(self) -> u32 {
        self as u32
    }

    pub fn from_code(code: u32) -> Option<Self> {
        [Self::Open, Self::Preview, Self::CopyPath, Self::CopyLink, Self::OnGitHub, Self::Compare, Self::ManageAgain].into_iter().find(|c| c.code() == code)
    }

    fn draw_icon(self, zone: Rect) {
        let ch = match self {
            Self::Open => glyph::FOLDER,
            Self::Preview => glyph::VIEW,
            Self::CopyPath => glyph::COPY,
            Self::CopyLink => glyph::LINK,
            Self::OnGitHub => return paint::github_mark(zone, Role::Text),
            Self::Compare => glyph::FILE,
            Self::ManageAgain => glyph::UNDO,
        };
        paint::glyph(zone, ch, 16, Role::Text);
    }
}
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
    /// A skill header. `disclosure` is its expanded state, or None for a flat row.
    Row {
        tone: Tone,
        glyph: Option<char>,
        commands: Vec<RowCmd>,
        disclosure: Option<bool>,
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
    /// A button that offers a list. Its window text is the entry, optionally preceded by its purpose and a newline.
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
    layout: OnceCell<RowParts>,
    hot: Option<RowTarget>,
    pressed: Option<RowTarget>,
    /// The panel's shared tooltip, while this row has registered tools with it.
    tools: Option<(HWND, Vec<Vec<u16>>)>,
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
    row_tip: HWND,
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
        Look::Row { disclosure: None, commands, .. } => {
            if !commands.is_empty() {
                notify(hwnd, ROW_MENU);
            }
        }
        Look::Combo { items } => {
            let Some(text) = choose_combo(&text_of(hwnd), &items, |entries, checked| menu(hwnd, entries, checked, false)) else { return };
            if unsafe { IsWindow(hwnd) } == 0 {
                return;
            }
            unsafe { SetWindowTextW(hwnd, wide(&text).as_ptr()) };
            notify(hwnd, CBN_SELENDOK);
        }
        _ => {
            invalidate(hwnd);
            notify(hwnd, BN_CLICKED);
        }
    }
}

/// The two lines of a list entry: its window text is both, so a screen reader reads the status with the name.
/// A name never holds a line break: names pass `printable` first.
pub fn lines(text: &str) -> (&str, &str) {
    text.split_once('\n').unwrap_or((text, ""))
}

/// Named combos expose both purpose and value through WM_GETTEXT, but paint, check and dispatch the value alone.
pub fn combo_value(text: &str) -> &str {
    text.split_once('\n').map_or(text, |(_, value)| value)
}

fn choose_combo(before: &str, items: &[String], choose: impl FnOnce(&[&str], Option<usize>) -> Option<usize>) -> Option<String> {
    let entries: Vec<&str> = items.iter().map(String::as_str).collect();
    let current = combo_value(before);
    let checked = entries.iter().position(|&entry| entry == current);
    let &value = entries.get(choose(&entries, checked)?)?;
    Some(match before.split_once('\n') {
        Some((purpose, _)) => two_lines(purpose, value),
        None => value.to_string(),
    })
}

struct RowText<'a> {
    name: &'a str,
    tag: &'a str,
    facts: &'a str,
    explanation: &'a str,
}

impl<'a> RowText<'a> {
    fn parse(text: &'a str) -> Self {
        let mut fields = text.splitn(4, '\n');
        Self {
            name: fields.next().unwrap_or_default(),
            tag: fields.next().unwrap_or_default(),
            facts: fields.next().unwrap_or_default(),
            explanation: fields.next().unwrap_or_default(),
        }
    }

    fn tip(&self) -> String {
        match (self.explanation.trim(), self.facts.trim()) {
            ("", "") => self.tag.into(),
            ("", facts) => facts.into(),
            (explanation, "") => explanation.into(),
            (explanation, facts) => format!("{explanation}\n{facts}"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RowTarget {
    Cmd(RowCmd),
    More,
    Disclosure,
}

impl RowTarget {
    fn code(self) -> u32 {
        match self {
            Self::Cmd(cmd) => cmd.code(),
            Self::More => ROW_MENU,
            Self::Disclosure => BN_CLICKED,
        }
    }

    fn tip(self) -> &'static str {
        match self {
            Self::Cmd(RowCmd::Open) => "Open this skill’s folder in File Explorer.",
            Self::Cmd(cmd) => cmd.label(),
            Self::More => "More actions",
            Self::Disclosure => "Show or hide repair choices",
        }
    }
}

struct RowParts {
    mark: Rect,
    name: Rect,
    icons: Vec<(RowTarget, Rect)>,
    pill: Rect,
    clipped_pill: bool,
}

fn row_parts([x, y, w, h]: Rect, name_w: i32, pill_w: i32, commands: &[RowCmd]) -> RowParts {
    const SLOT: i32 = 32;
    const GAP: i32 = 4;
    let left = x + 44;
    let right = x + w - 16;
    let reserve = if commands.is_empty() { 0 } else { SLOT + GAP };
    let pw = pill_w.min((right - left - 8 - reserve - 24).max(0));
    let pill = [right - pw, y + (h - paint::PILL_H) / 2, pw, paint::PILL_H];
    let room = (pill[0] - if pw > 0 { 8 } else { 0 } - left).max(0);
    // Keep a recognizable name before fitting commands. If necessary, trade the last slot for More.
    let min_slots = if commands.contains(&RowCmd::Open) && commands.len() > 1 && room >= name_w.min(24) + GAP + 2 * SLOT {
        2 // Shorten the name before hiding Open folder behind More.
    } else {
        i32::from(!commands.is_empty())
    };
    let slots = ((room - name_w.min(80) - GAP) / SLOT).max(min_slots) as usize;
    let count = slots.min(commands.len());
    let icon_w = SLOT.min((room - GAP).max(0));
    let name = [left, y, name_w.min((room - if count == 0 { 0 } else { GAP + icon_w * count as i32 }).max(0)), h];
    let ordered = commands.iter().filter(|&&c| c == RowCmd::Open).chain(commands.iter().filter(|&&c| c != RowCmd::Open));
    let icons = ordered
        .take(count)
        .enumerate()
        .map(|(i, &cmd)| {
            let target = if i + 1 == count && count < commands.len() { RowTarget::More } else { RowTarget::Cmd(cmd) };
            (target, [left + name[2] + GAP + icon_w * i as i32, y + (h - SLOT.min(h)) / 2, icon_w, SLOT.min(h)])
        })
        .collect();
    RowParts { mark: [x + 16, y, 16, h], name, icons, pill, clipped_pill: pw != pill_w }
}

impl Ctl {
    fn row(&self, hwnd: HWND, dpi: u32) -> Option<&RowParts> {
        let Look::Row { glyph, commands, .. } = &self.look else { return None };
        Some(self.layout.get_or_init(|| {
            let text = text_of(hwnd);
            let RowText { name, tag, .. } = RowText::parse(&text);
            row_parts(self.at, paint::measure(name, Font::Body, 0, dpi).0, if tag.is_empty() { 0 } else { paint::pill_width(tag, *glyph, dpi) }, commands)
        }))
    }

    fn target(&self, hwnd: HWND, lp: LPARAM) -> Option<RowTarget> {
        let Look::Row { disclosure, .. } = self.look else { return None };
        let dpi = dpi_of(hwnd);
        let hit = |rect| {
            let r = client_rect(self.at, rect, dpi);
            let (x, y) = (i32::from(lp as i16), i32::from((lp >> 16) as i16));
            x >= r.left && x < r.right && y >= r.top && y < r.bottom
        };
        self.row(hwnd, dpi)?
            .icons
            .iter()
            .find(|(_, r)| hit(*r))
            .map(|(t, _)| *t)
            .or_else(|| (disclosure.is_some() && hit(self.at)).then_some(RowTarget::Disclosure))
    }
}

/// Scale both edges exactly as paint and child placement do, including at fractional DPI.
fn client_rect(at: Rect, [x, y, w, h]: Rect, dpi: u32) -> RECT {
    let (ox, oy) = (scale(at[0], dpi), scale(at[1], dpi));
    RECT { left: scale(x, dpi) - ox, top: scale(y, dpi) - oy, right: scale(x + w, dpi) - ox, bottom: scale(y + h, dpi) - oy }
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
    let tint = if c.down && c.hover && !matches!(c.pressed, Some(RowTarget::Cmd(_) | RowTarget::More)) {
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
        Look::Row { tone, glyph, disclosure, commands } => {
            let RowText { name: text, tag, .. } = RowText::parse(&text);
            paint::fill([x + 4, y + 3, w - 8, h - 6], 4, tint, Role::None);
            let mark = match disclosure {
                Some(true) => glyph::CHEVRON_DOWN,
                Some(false) => glyph::CHEVRON_RIGHT,
                None => glyph::FILE,
            };
            let parts = c.row(hwnd, dpi).expect("row");
            paint::glyph(parts.mark, mark, 14, Role::Text2);
            let pill = parts.pill;
            if parts.clipped_pill && pill[2] > 4 {
                paint::line(pill, tag, Font::Caption, Role::Text2);
            } else if !parts.clipped_pill && pill[2] > 0 {
                paint::pill(pill[0], pill[1], tag, *tone, *glyph, dpi);
            }
            let focus = unsafe { GetFocus() } == hwnd;
            if c.hover || focus {
                for &(target, zone) in &parts.icons {
                    if c.hot == Some(target) {
                        paint::fill(zone, 4, if c.pressed == Some(target) { Role::Sel } else { Role::Hover }, Role::None);
                    }
                    match target {
                        RowTarget::Cmd(cmd) => cmd.draw_icon(zone),
                        _ => paint::glyph(zone, glyph::MORE, 16, Role::Text),
                    }
                }
                if focus && !commands.is_empty() {
                    let left = parts.icons.last().map_or(parts.name[0] + parts.name[2], |(_, r)| r[0] + r[2]) + 6;
                    let room = pill[0] - 6 - left;
                    let hint = "Shift+F10: actions";
                    if room >= paint::measure(hint, Font::Caption, 0, dpi).0 {
                        paint::line([left, y, room, h], hint, Font::Caption, Role::Text2);
                    }
                }
            }
            paint::line(parts.name, text, Font::Body, Role::Text);
        }
        Look::Pill { tone, glyph } => paint::pill(x, y + (h - paint::PILL_H) / 2, &text, *tone, *glyph, dpi),
        Look::Toggle { on } => {
            let track = [x + w - 40, y + (h - 20) / 2, 40, 20];
            let [tx, ty, ..] = track;
            let ink = if enabled { Role::Text2 } else { Role::Text3 };
            if *on {
                paint::fill(track, 10, if enabled { Role::Accent } else { Role::Text3 }, Role::None);
                paint::fill([tx + 24, ty + 4, 12, 12], 6, Role::OnAccent, Role::None);
            } else {
                paint::fill(track, 10, tint, ink);
                paint::fill([tx + 4, ty + 4, 12, 12], 6, ink, Role::None);
            }
            let state = if *on { "On" } else { "Off" };
            paint::text([x, y, w - 52, h], state, Font::Body, if enabled { Role::Text } else { Role::Text3 }, DT_SINGLELINE | DT_VCENTER | DT_RIGHT);
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
            paint::line([x + 12, y, w - 12 - 32, h], combo_value(&text), Font::Body, if enabled { Role::Text } else { Role::Text3 });
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
                let mut c = unsafe { Box::from_raw(ptr) }.into_inner();
                remove_tools(hwnd, &mut c);
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
                let target = c.target(hwnd, lp);
                let mut changed = c.hot != target;
                c.hot = target;
                if !c.hover {
                    c.hover = true;
                    changed = true;
                    track_hover(hwnd);
                }
                if matches!(c.look, Look::Row { .. }) && !c.down {
                    row_tool_move(hwnd, &mut c, wp, lp);
                }
                if changed {
                    invalidate(hwnd);
                }
            }
            return 0;
        }
        WM_MOUSELEAVE => {
            if let Some(Ok(mut c)) = ctl_of(hwnd).map(RefCell::try_borrow_mut) {
                c.hover = false;
                c.hot = None;
                remove_tools(hwnd, &mut c);
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
                c.hot = c.target(hwnd, lp);
                c.pressed = c.hot;
                remove_tools(hwnd, &mut c);
            }
            unsafe { SetCapture(hwnd) };
            invalidate(hwnd);
            return 0;
        }
        WM_LBUTTONUP => {
            let Some(cell) = ctl_of(hwnd) else { return 0 };
            let Ok(c) = cell.try_borrow() else { return 0 };
            let row = matches!(c.look, Look::Row { .. });
            let clicked = c.down && c.hover;
            let target = c.pressed.filter(|&t| c.down && c.target(hwnd, lp) == Some(t));
            drop(c);
            if row && unsafe { GetCapture() } != hwnd {
                return 0;
            }
            unsafe { ReleaseCapture() };
            if row {
                if let Some(t) = target {
                    notify(hwnd, t.code());
                }
            } else if clicked {
                activate(hwnd);
            }
            return 0;
        }
        WM_CAPTURECHANGED => {
            if let Some(Ok(mut c)) = ctl_of(hwnd).map(RefCell::try_borrow_mut) {
                c.down = false;
                c.pressed = None;
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
            if !msg.is_null()
                && unsafe {
                    ((*msg).message == WM_KEYDOWN && (*msg).wParam == usize::from(VK_RETURN))
                        || (matches!((*msg).message, WM_KEYDOWN | WM_SYSKEYDOWN) && row_menu_key(hwnd, (*msg).wParam))
                }
            {
                return DLGC_WANTMESSAGE as LRESULT;
            }
            return 0;
        }
        WM_KEYDOWN | WM_SYSKEYDOWN if row_menu_key(hwnd, wp) => {
            if lp & (1 << 30) == 0 && unsafe { IsWindowEnabled(hwnd) != 0 && GetFocus() == hwnd } {
                notify(hwnd, ROW_MENU);
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
        WM_KILLFOCUS | WM_ENABLE => {
            if msg == WM_ENABLE && wp == 0 {
                reset_row(hwnd);
            }
            invalidate(hwnd);
        }
        WM_UPDATEUISTATE | WM_SETTEXT => {
            if msg == WM_SETTEXT {
                reset_row(hwnd);
            }
            let r = unsafe { DefWindowProcW(hwnd, msg, wp, lp) };
            invalidate(hwnd);
            return r;
        }
        WM_MOUSEWHEEL => return unsafe { SendMessageW(GetParent(hwnd), msg, wp, lp) },
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

fn row_menu_key(hwnd: HWND, key: usize) -> bool {
    (key == usize::from(VK_APPS) || (key == usize::from(VK_F10) && unsafe { GetKeyState(i32::from(VK_SHIFT)) } < 0))
        && ctl_of(hwnd).is_some_and(|cell| cell.try_borrow().is_ok_and(|c| matches!(&c.look, Look::Row { commands, .. } if !commands.is_empty())))
}

fn reset_row(hwnd: HWND) {
    let Some(Ok(mut c)) = ctl_of(hwnd).map(RefCell::try_borrow_mut) else { return };
    if !matches!(c.look, Look::Row { .. }) {
        return;
    }
    remove_tools(hwnd, &mut c);
    c.layout.take();
    c.hot = None;
    c.pressed = None;
    c.down = false;
    drop(c);
    if unsafe { GetCapture() } == hwnd {
        unsafe { ReleaseCapture() };
    }
}

/// Makes an empty panel. `bg` is its colour; `Role::None` lets its parent draw behind it.
pub fn panel(parent: HWND, id: i32, bg: Role) -> HWND {
    let state = Box::into_raw(Box::new(RefCell::new(Panel {
        surface: Surface::default(),
        children: Vec::new(),
        bg,
        scroll: 0,
        dpi: dpi_of(parent),
        hover: false,
        row_tip: null_mut(),
    })));
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
    let captured = unsafe { GetCapture() };
    let focus = unsafe { GetFocus() };
    let mut lost_focus = false;
    let mut old: Vec<Option<(HWND, Item)>> = std::mem::take(&mut p.children).into_iter().zip(old_items).map(Some).collect();
    let mut children = Vec::with_capacity(surface.items.len());
    let (mut reorder, mut last, mut after) = (false, 0, HWND_TOP);
    for item in &surface.items {
        let kept = index.get(&item.id).and_then(|&i| old[i].take().map(|o| (i, o)));
        let hwnd = match kept {
            Some((pos, (hwnd, before))) if !hwnd.is_null() => {
                reorder |= pos < last;
                last = pos;
                // Row ids are positional. Even identical-looking items may be from different
                // repositories, so don't carry a captured press across a new surface.
                if matches!(before.look, Look::Row { .. }) && (hwnd == captured || moved_dpi || before != *item) {
                    reset_row(hwnd);
                } else if hwnd == captured && matches!(before.look, Look::Button { .. }) && matches!(page::hit(before.id), Hit::Skill(..) | Hit::DiffAction(_))
                {
                    // Repair and comparison buttons also use positional ids. Even an unchanged label
                    // cannot prove the target survived a refresh. WM_CAPTURECHANGED cancels the press.
                    unsafe { ReleaseCapture() };
                }
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
                    lost_focus |= hwnd == focus && !item.enabled;
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
    let state = Box::into_raw(Box::new(RefCell::new(Ctl {
        look: item.look.clone(),
        at: item.at,
        hover: false,
        down: false,
        layout: OnceCell::new(),
        hot: None,
        pressed: None,
        tools: None,
    })));
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

/// The current scroll offset in pixels, also used to lay out a question's native edit control.
pub(super) fn scroll_offset(panel: HWND) -> i32 {
    panel_of(panel).and_then(|c| c.try_borrow().ok().map(|p| p.scroll)).unwrap_or(0)
}

fn scroll_by(panel: HWND, delta: i32) {
    scroll_to(panel, scroll_offset(panel) + delta);
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
pub(super) fn reveal(panel: HWND, child: HWND) {
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
                let p = unsafe { Box::from_raw(ptr) }.into_inner();
                if !p.row_tip.is_null() && unsafe { IsWindow(p.row_tip) } != 0 {
                    unsafe { DestroyWindow(p.row_tip) };
                }
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

fn make_tip(ctl: HWND) -> HWND {
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
    if !tip.is_null() {
        unsafe { SendMessageW(tip, TTM_SETMAXTIPWIDTH, 0, scale(320, dpi_of(ctl)) as isize) };
        scrollbar_theme(tip);
    }
    tip
}

fn tool_info(hwnd: HWND, id: usize) -> TTTOOLINFOW {
    let mut info: TTTOOLINFOW = unsafe { zeroed() };
    // Nothing here uses the v6-only lpReserved tail. v5 (e.g. an unmanifested host)
    // rejects the entire tool if given a v6 size.
    info.cbSize = std::mem::offset_of!(TTTOOLINFOW, lpReserved) as u32;
    info.hwnd = hwnd;
    info.uId = id;
    info
}

/// Only a hovered row registers tools. Explicit relay includes the first mouse move, even
/// when the pointer entered directly over an icon and then stayed still.
fn row_tool_move(hwnd: HWND, c: &mut Ctl, wp: WPARAM, lp: LPARAM) {
    if c.tools.is_none() {
        let dpi = dpi_of(hwnd);
        let Some(parts) = c.row(hwnd, dpi) else { return };
        let disclosure = matches!(c.look, Look::Row { disclosure: Some(_), .. }).then_some((RowTarget::Disclosure, parts.mark));
        let parent = unsafe { GetParent(hwnd) };
        let Some(Ok(mut p)) = panel_of(parent).map(RefCell::try_borrow_mut) else { return };
        if p.row_tip.is_null() {
            p.row_tip = make_tip(parent);
        }
        let tip = p.row_tip;
        drop(p);
        if tip.is_null() {
            return;
        }
        let mut strings = Vec::new();
        let mut add = |rect: Rect, text: &str| {
            if rect[2] <= 0 || rect[3] <= 0 {
                return;
            }
            let mut s = wide(text);
            let mut info = tool_info(hwnd, strings.len() + 1);
            info.rect = client_rect(c.at, rect, dpi);
            info.lpszText = s.as_mut_ptr();
            unsafe { SendMessageW(tip, TTM_ADDTOOLW, 0, (&raw const info) as isize) };
            strings.push(s);
        };
        for (target, rect) in parts.icons.iter().copied().chain(disclosure) {
            add(rect, target.tip());
        }
        // Native tools must not overlap. The disclosure spans the row height; command
        // slots form one contiguous band, with body space above and below it.
        let body = RowText::parse(&text_of(hwnd)).tip();
        let [mut left, y, w, h] = c.at;
        let right = left + w;
        if let Some((_, [x, _, width, _])) = disclosure {
            add([left, y, x - left, h], &body);
            left = x + width;
        }
        if let (Some((_, [x, top, ..])), Some((_, [last, _, width, height]))) = (parts.icons.first().copied(), parts.icons.last().copied()) {
            add([left, y, x - left, h], &body);
            left = last + width;
            add([x, y, left - x, top - y], &body);
            add([x, top + height, left - x, y + h - top - height], &body);
        }
        add([left, y, right - left, h], &body);
        c.tools = Some((tip, strings));
    }
    if let Some((tip, _)) = &c.tools {
        let mut msg: MSG = unsafe { zeroed() };
        msg.hwnd = hwnd;
        msg.message = WM_MOUSEMOVE;
        msg.wParam = wp;
        msg.lParam = lp;
        msg.pt = POINT { x: i32::from(lp as i16), y: i32::from((lp >> 16) as i16) };
        unsafe {
            MapWindowPoints(hwnd, null_mut(), &mut msg.pt, 1);
            SendMessageW(*tip, TTM_RELAYEVENT, 0, (&raw const msg) as isize);
        }
    }
}

fn remove_tools(hwnd: HWND, c: &mut Ctl) {
    if let Some((tip, strings)) = c.tools.take() {
        for i in 0..strings.len() {
            let info = tool_info(hwnd, i + 1);
            unsafe { SendMessageW(tip, TTM_DELTOOLW, 0, (&raw const info) as isize) };
        }
    }
}

/// A tooltip for a control.
pub fn tip(ctl: HWND, text: &str) {
    let tip = make_tip(ctl);
    if tip.is_null() {
        return;
    }
    let mut text = wide(text);
    let mut info = tool_info(ctl, ctl as usize);
    info.uFlags = TTF_IDISHWND | TTF_SUBCLASS;
    info.lpszText = text.as_mut_ptr();
    unsafe {
        SendMessageW(tip, TTM_ADDTOOLW, 0, (&raw const info) as isize);
    }
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
            look: Look::Row { tone: Tone::Ok, glyph: None, commands: vec![], disclosure: None },
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

    #[test]
    fn source_loading_moves_keyboard_focus_off_disabled_retry_and_edit_actions() {
        let (top, panel) = test_panel();
        let source_controls = |retry_enabled, edit_enabled| Surface {
            items: vec![
                Item {
                    id: page::id::RETRY_SOURCE,
                    at: [0, 0, 120, 32],
                    text: "Retry".into(),
                    look: Look::Button { kind: Kind::Accent, glyph: None },
                    enabled: retry_enabled,
                },
                Item {
                    id: page::id::EDIT_SOURCE,
                    at: [128, 0, 120, 32],
                    text: "Edit".into(),
                    look: Look::Button { kind: Kind::Standard, glyph: None },
                    enabled: edit_enabled,
                },
                Item {
                    id: page::id::CANCEL_SOURCE,
                    at: [256, 0, 120, 32],
                    text: "Cancel".into(),
                    look: Look::Button { kind: Kind::Standard, glyph: None },
                    enabled: true,
                },
            ],
            height: 48,
            decos: vec![],
        };
        let loading = source_controls(false, false);

        for id in [page::id::RETRY_SOURCE, page::id::EDIT_SOURCE] {
            set(panel, source_controls(true, true));
            let action = child(panel, id);
            unsafe { SetFocus(action) };
            assert_eq!(unsafe { GetFocus() }, action);

            set(panel, loading.clone());
            let focus = unsafe { GetFocus() };
            assert!(
                loading.items.iter().any(|item| item.enabled && item.look.interactive() && child(panel, item.id) == focus),
                "loading retained an invalid focus target {focus:?}"
            );
            assert_eq!(unsafe { IsWindowEnabled(focus) }, 1);
        }

        unsafe { DestroyWindow(top) };
    }

    #[test]
    fn row_icons_follow_the_measured_name_and_overflow_before_the_badge() {
        let commands = [RowCmd::Preview, RowCmd::Open, RowCmd::CopyPath, RowCmd::CopyLink, RowCmd::OnGitHub];
        for dpi in [96, 120, 144, 192] {
            for name in ["a", "a long skill name".repeat(12).as_str()] {
                let nw = paint::measure(name, Font::Body, 0, dpi).0;
                for w in [214, 294, 850] {
                    let at = [3, 5, w, 42];
                    let parts = row_parts(at, nw, 102, &commands);
                    let (first, zone) = parts.icons[0];
                    assert_eq!(zone[0], parts.name[0] + parts.name[2] + 4);
                    assert!(parts.name[2] <= nw);
                    if name == "a" {
                        assert_eq!(parts.name[2], nw);
                    }
                    let last = parts.icons.last().unwrap();
                    assert!(client_rect(at, last.1, dpi).right <= client_rect(at, parts.pill, dpi).left);
                    assert_eq!(last.0 == RowTarget::More, parts.icons.len() < commands.len());
                    if parts.icons.len() > 1 {
                        assert_eq!(first, RowTarget::Cmd(RowCmd::Open));
                    }
                    if w == 850 {
                        assert_eq!(parts.icons.len(), commands.len());
                    } else if w == 294 && name != "a" {
                        assert_eq!(parts.icons.iter().map(|(target, _)| *target).collect::<Vec<_>>(), [RowTarget::Cmd(RowCmd::Open), RowTarget::More]);
                        assert!(parts.name[2] >= 24 && parts.name[2] < nw);
                        assert_eq!(parts.pill[2], 102);
                    } else if w == 214 {
                        assert_eq!(last.0, RowTarget::More);
                    }
                }
            }
        }
    }

    thread_local! {
        static COMMANDS: RefCell<Vec<(i32, u32)>> = const { RefCell::new(Vec::new()) };
    }

    unsafe extern "system" fn record(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        if msg == WM_COMMAND {
            COMMANDS.with(|c| c.borrow_mut().push(((wp & 0xFFFF) as i32, ((wp >> 16) & 0xFFFF) as u32)));
            return 0;
        }
        unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
    }

    fn test_panel() -> (HWND, HWND) {
        register();
        assert!(register_class("TinySkillSyncTestTop", Some(record), null_mut()));
        let top = unsafe {
            CreateWindowExW(
                0,
                wide("TinySkillSyncTestTop").as_ptr(),
                null(),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                800,
                600,
                null_mut(),
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            )
        };
        assert!(!top.is_null());
        let panel = panel(top, 1, Role::Bg);
        unsafe { SetWindowPos(panel, null_mut(), 0, 0, 600, 500, SWP_NOZORDER | SWP_NOACTIVATE) };
        (top, panel)
    }

    #[test]
    fn a_skill_card_button_press_does_not_transfer_to_an_identically_labelled_replacement() {
        let (top, panel) = test_panel();
        let card = |skill: &str, part, label: &str| Surface {
            items: vec![
                Item {
                    id: page::skill_id(0, 0),
                    at: [0, 0, 500, 40],
                    text: skill.into(),
                    look: Look::Row { tone: Tone::Ok, glyph: None, commands: vec![], disclosure: Some(true) },
                    enabled: true,
                },
                Item {
                    id: page::skill_id(0, part),
                    at: [0, 40, 120, 40],
                    text: label.into(),
                    look: Look::Button { kind: Kind::Standard, glyph: None },
                    enabled: true,
                },
            ],
            height: 80,
            decos: vec![],
        };
        for (part, label) in [(2, "Show changes"), (4, "Keep my version")] {
            set(panel, card("alpha", part, label));
            let id = page::skill_id(0, part);
            let button = child(panel, id);
            unsafe { SendMessageW(button, WM_LBUTTONDOWN, 1, 0) };
            set(panel, card("bravo", part, label));
            assert_eq!(child(panel, id), button);
            unsafe { SendMessageW(button, WM_LBUTTONUP, 0, 0) };
            assert!(COMMANDS.with(|c| c.take()).is_empty(), "{label} dispatched to replacement skill");
            unsafe {
                SendMessageW(button, WM_LBUTTONDOWN, 1, 0);
                SendMessageW(button, WM_LBUTTONUP, 0, 0);
            }
            assert_eq!(COMMANDS.with(|c| c.take()).len(), 1, "fresh {label} press should work");
        }
        unsafe { DestroyWindow(top) };
    }

    #[test]
    fn row_details_cover_body_and_badge_without_shadowing_icon_tips() {
        use windows_sys::Win32::UI::Controls::{TTHITTESTINFOW, TTM_GETTEXTW, TTM_GETTOOLCOUNT, TTM_HITTESTW};

        let (top, panel) = test_panel();
        let at = [8, 5, 510, 48];
        let page = |text: &str, commands| Surface {
            items: vec![Item { id: FIRST, at, text: text.into(), look: Look::Row { tone: Tone::Ok, glyph: None, commands, disclosure: None }, enabled: true }],
            decos: vec![],
            height: 60,
        };
        let details = "alpha\nReady\nYesterday\nFirst line\nSecond line";
        set(panel, page(details, vec![]));
        let row = child(panel, FIRST);
        let tip_at = |x, y| {
            let pt = POINT { x, y };
            unsafe { SendMessageW(row, WM_MOUSEMOVE, 0, ((y << 16) | x) as LPARAM) };
            let tip = panel_of(panel).unwrap().borrow().row_tip;
            assert!(!tip.is_null());
            let mut hit = TTHITTESTINFOW { hwnd: row, pt, ti: tool_info(row, 0) };
            assert_ne!(unsafe { SendMessageW(tip, TTM_HITTESTW, 0, (&raw mut hit) as isize) }, 0, "missing tooltip at {x},{y}");
            let mut buf = [0u16; 128];
            hit.ti.lpszText = buf.as_mut_ptr();
            unsafe { SendMessageW(tip, TTM_GETTEXTW, buf.len(), (&raw mut hit.ti) as isize) };
            String::from_utf16_lossy(&buf[..buf.iter().position(|&n| n == 0).unwrap()])
        };
        let body = "First line\nSecond line\nYesterday";
        let dpi = dpi_of(row);
        let bounds = client_rect(at, at, dpi);
        assert_eq!(tip_at(bounds.right - 1, bounds.bottom - 1), body);

        set(panel, page(details, vec![RowCmd::Preview]));
        let tip = panel_of(panel).unwrap().borrow().row_tip;
        assert_eq!(unsafe { SendMessageW(tip, TTM_GETTOOLCOUNT, 0, 0) }, 0);
        let parts = row_parts(at, paint::measure("alpha", Font::Body, 0, dpi).0, paint::pill_width("Ready", None, dpi), &[RowCmd::Preview]);
        let icon = client_rect(at, parts.icons[0].1, dpi);
        let badge = client_rect(at, parts.pill, dpi);
        assert_eq!(ctl_of(row).unwrap().borrow().row(row, dpi).unwrap().pill, parts.pill);
        for (x, y) in [(0, 0), (icon.left, icon.top - 1), (icon.right - 1, icon.bottom), (badge.left, badge.top)] {
            assert_eq!(tip_at(x, y), body);
        }
        assert_eq!(tip_at(icon.left, icon.top), "Preview");
        for (text, expected) in [("alpha\nReady\n\nDetails", "Details"), ("alpha\nReady\nYesterday\n", "Yesterday"), ("alpha\nReady", "Ready")] {
            set(panel, page(text, vec![]));
            assert_eq!(tip_at(1, 1), expected);
        }
        unsafe { DestroyWindow(top) };
    }

    #[test]
    fn row_input_distinguishes_icons_disclosure_and_keyboard_and_cancels_on_rebind() {
        let (top, panel) = test_panel();
        let at = [0, 0, 560, 42];
        let page = |disclosure| Surface {
            items: vec![Item {
                id: FIRST,
                at,
                text: "alpha\nUp to date".to_string(),
                look: Look::Row { tone: Tone::Ok, glyph: Some(glyph::OK), commands: vec![RowCmd::Open, RowCmd::Preview], disclosure },
                enabled: true,
            }],
            decos: Vec::new(),
            height: 42,
        };
        set(panel, page(None));
        let row = child(panel, FIRST);
        let dpi = dpi_of(row);
        let parts = row_parts(
            at,
            paint::measure("alpha", Font::Body, 0, dpi).0,
            paint::pill_width("Up to date", Some(glyph::OK), dpi),
            &[RowCmd::Open, RowCmd::Preview],
        );
        let [zx, zy, zw, zh] = parts.icons[0].1;
        let point = |x: i32, y: i32| ((scale(y, dpi) << 16) | (scale(x, dpi) & 0xFFFF)) as LPARAM;
        let (on_zone, on_name) = (point(zx + zw / 2, zy + zh / 2), point(60, 21));
        unsafe { SendMessageW(row, WM_MOUSEMOVE, 0, on_zone) };
        let tip = panel_of(panel).unwrap().borrow().row_tip;
        assert!(!tip.is_null());
        for (i, expected) in [(1, "Open this skill’s folder in File Explorer."), (2, "Preview")] {
            let mut info = tool_info(row, i);
            let mut buf = [0u16; 128];
            info.lpszText = buf.as_mut_ptr();
            unsafe { SendMessageW(tip, windows_sys::Win32::UI::Controls::TTM_GETTEXTW, buf.len(), (&raw mut info) as isize) };
            assert_eq!(String::from_utf16_lossy(&buf[..buf.iter().position(|&n| n == 0).unwrap()]), expected);
        }
        let click = |down: LPARAM, up: LPARAM| {
            COMMANDS.with(|c| c.borrow_mut().clear());
            unsafe {
                SendMessageW(row, WM_MOUSEMOVE, 0, down);
                SendMessageW(row, WM_LBUTTONDOWN, 1, down);
                SendMessageW(row, WM_MOUSEMOVE, 1, up);
                SendMessageW(row, WM_LBUTTONUP, 0, up);
            }
            COMMANDS.with(|c| c.borrow().clone())
        };

        let on_preview = point(zx + zw + zw / 2, zy + zh / 2);
        assert_eq!(click(on_zone, on_zone), [(FIRST, RowCmd::Open.code())]);
        assert_eq!(click(on_preview, on_preview), [(FIRST, RowCmd::Preview.code())]);
        assert_eq!(click(on_zone, on_preview), []);
        assert_eq!(click(on_name, on_name), []);
        assert_eq!(click(on_zone, on_name), [], "a press that leaves the button is no click");
        assert_eq!(click(on_name, on_zone), []);
        unsafe { SendMessageW(row, WM_KEYDOWN, VK_RETURN.into(), 0) };
        assert_eq!(COMMANDS.with(|c| c.take()), [(FIRST, ROW_MENU)]);
        set(panel, page(Some(false)));
        assert_eq!(click(on_name, on_name), [(FIRST, BN_CLICKED)]);
        assert_eq!(click(on_name, on_zone), []);
        unsafe { SendMessageW(row, WM_KEYDOWN, VK_APPS.into(), 0) };
        assert_eq!(COMMANDS.with(|c| c.take()), [(FIRST, ROW_MENU)]);
        unsafe { SendMessageW(row, WM_LBUTTONDOWN, 1, on_zone) };
        let mut rebound = page(Some(false));
        rebound.items[0].text = "bravo\nUp to date".into();
        set(panel, rebound);
        unsafe { SendMessageW(row, WM_LBUTTONUP, 0, on_zone) };
        assert!(COMMANDS.with(|c| c.take()).is_empty());
        let mut narrow = page(None);
        narrow.items[0].at[2] = 170;
        set(panel, narrow);
        let more = {
            let c = ctl_of(row).unwrap().borrow();
            let &(target, [x, y, w, h]) = c.row(row, dpi).unwrap().icons.last().unwrap();
            assert_eq!(target, RowTarget::More);
            point(x + w / 2, y + h / 2)
        };
        assert_eq!(click(more, more), [(FIRST, ROW_MENU)]);
        unsafe { DestroyWindow(top) };
    }

    #[test]
    fn a_combo_checks_the_current_entry_and_keeps_its_accessible_name_after_a_choice() {
        let items = ["Daily", "Every 7 days", "Every 30 days"].map(String::from);
        for purpose in ["App update check interval\n", ""] {
            let before = format!("{purpose}Every 30 days");
            let changed = choose_combo(&before, &items, |offered, checked| {
                assert_eq!(offered, ["Daily", "Every 7 days", "Every 30 days"]);
                assert_eq!(checked, Some(2));
                Some(1)
            })
            .unwrap();
            assert_eq!(changed, format!("{purpose}Every 7 days"));
            assert_eq!(combo_value(&changed), "Every 7 days", "what the button paints and dispatches");
            assert!(
                choose_combo(&changed, &items, |_, checked| {
                    assert_eq!(checked, Some(1));
                    None
                })
                .is_none()
            );
        }
    }
}
