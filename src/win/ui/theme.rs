//! Colours: the Windows 11 light and dark palettes, and the high contrast colours when that is on.
//!
//! The app follows the "app mode" the user chose in Windows (`AppsUseLightTheme`); `TSS_THEME=dark` or `light`
//! overrides it, for screenshots. The title bar and menus are asked to match through DWM and uxtheme, both loaded only
//! when a window opens, so the idle tray app never loads them.

use std::cell::Cell;
use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Graphics::Gdi::GetSysColor;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::UI::WindowsAndMessaging::{SPI_GETHIGHCONTRAST, SystemParametersInfoW};

use crate::win::wide;

/// What a colour is for. Each one has a light, a dark and a high contrast value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Bg,
    Layer,
    LayerBorder,
    Card,
    CardBorder,
    Divider,
    Text,
    Text2,
    Text3,
    Accent,
    AccentText,
    OnAccent,
    Ctl,
    CtlBorder,
    CtlBottom,
    Hover,
    Sel,
    Ok,
    OkBg,
    Warn,
    WarnBg,
    Err,
    ErrBg,
    Off,
    OffBg,
    AddBg,
    AddGut,
    DelBg,
    DelGut,
    GapBg,
    DialogFoot,
    DotWarn,
    DotGlyph,
    None,
}

const COUNT: usize = Role::None as usize;

/// 0xAARRGGBB, in the order of [`Role`].
#[rustfmt::skip]
const LIGHT: [u32; COUNT] = [
    0xFFF3F3F3, 0xFFF9F9F9, 0x0F000000, 0xFFFFFFFF, 0x13000000, 0x12000000, 0xFF1A1A1A, 0xFF5D5D5D, 0xFF8B8B8B,
    0xFF005FB8, 0xFF005FB8, 0xFFFFFFFF, 0xFFFDFDFD, 0x14000000, 0x2E000000, 0x09000000, 0x0E000000,
    0xFF0F7B0F, 0xFFDFF6DD, 0xFF9D5D00, 0xFFFFF4CE, 0xFFC42B1C, 0xFFFDE7E9, 0xFF6B6B6B, 0x0F000000,
    0xFFE7F3E7, 0xFFD3EAD3, 0xFFFCEBEA, 0xFFF6D5D2, 0xFFF2F5F9, 0xFFF3F3F3, 0xFFC17F00, 0xFFFFFFFF,
];

#[rustfmt::skip]
const DARK: [u32; COUNT] = [
    0xFF202020, 0xFF272727, 0x0DFFFFFF, 0xFF2D2D2D, 0xFF1B1B1B, 0x0FFFFFFF, 0xFFFFFFFF, 0xFFCFCFCF, 0xFF9A9A9A,
    0xFF60CDFF, 0xFF99EBFF, 0xFF000000, 0xFF353535, 0x12FFFFFF, 0x1AFFFFFF, 0x0DFFFFFF, 0x14FFFFFF,
    0xFF6CCB5F, 0xFF393D1B, 0xFFFCE100, 0xFF433519, 0xFFFF99A4, 0xFF442726, 0xFFA8A8A8, 0x14FFFFFF,
    0xFF213226, 0xFF2A4231, 0xFF3B2528, 0xFF4F2E33, 0xFF2A2F36, 0xFF202020, 0xFFFCE100, 0xFF000000,
];

// The system colour indexes high contrast uses.
const WINDOW: u8 = 5;
const WINDOW_TEXT: u8 = 8;
const HIGHLIGHT: u8 = 13;
const HIGHLIGHT_TEXT: u8 = 14;
const GRAY_TEXT: u8 = 17;
const HOT_LIGHT: u8 = 26;
/// Drawn as nothing in high contrast: hover and selection tints, which have no system colour of their own.
const CLEAR: u8 = 0;

/// The system colour for each role in high contrast.
#[rustfmt::skip]
const CONTRAST: [u8; COUNT] = [
    WINDOW, WINDOW, WINDOW_TEXT, WINDOW, WINDOW_TEXT, WINDOW_TEXT, WINDOW_TEXT, WINDOW_TEXT, GRAY_TEXT,
    HIGHLIGHT, HOT_LIGHT, HIGHLIGHT_TEXT, WINDOW, WINDOW_TEXT, WINDOW_TEXT, CLEAR, CLEAR,
    WINDOW_TEXT, WINDOW, WINDOW_TEXT, WINDOW, WINDOW_TEXT, WINDOW, WINDOW_TEXT, CLEAR,
    WINDOW, WINDOW, WINDOW, WINDOW, WINDOW, WINDOW, WINDOW_TEXT, WINDOW,
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Light,
    Dark,
    Contrast,
}

thread_local! {
    static MODE: Cell<Option<Mode>> = const { Cell::new(None) };
}

#[repr(C)]
struct HighContrast {
    size: u32,
    flags: u32,
    scheme: *mut u16,
}

const HCF_HIGHCONTRASTON: u32 = 1;
const RRF_RT_REG_DWORD: u32 = 0x10;

#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegGetValueW(key: isize, subkey: *const u16, value: *const u16, flags: u32, kind: *mut u32, data: *mut c_void, len: *mut u32) -> u32;
}

fn detect() -> Mode {
    let mut hc = HighContrast { size: size_of::<HighContrast>() as u32, flags: 0, scheme: null_mut() };
    if unsafe { SystemParametersInfoW(SPI_GETHIGHCONTRAST, hc.size, (&raw mut hc).cast(), 0) } != 0 && hc.flags & HCF_HIGHCONTRASTON != 0 {
        return Mode::Contrast;
    }
    match std::env::var("TSS_THEME").as_deref() {
        Ok("dark") => return Mode::Dark,
        Ok("light") => return Mode::Light,
        _ => {}
    }
    // HKEY_CURRENT_USER, sign-extended as Windows defines it.
    let hkcu = 0x8000_0001u32 as i32 as isize;
    let mut light = 1u32;
    let mut len = size_of::<u32>() as u32;
    let key = wide(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
    let found =
        unsafe { RegGetValueW(hkcu, key.as_ptr(), wide("AppsUseLightTheme").as_ptr(), RRF_RT_REG_DWORD, null_mut(), (&raw mut light).cast(), &mut len) };
    if found == 0 && light == 0 { Mode::Dark } else { Mode::Light }
}

fn mode() -> Mode {
    MODE.with(|m| {
        m.get().unwrap_or_else(|| {
            let found = detect();
            m.set(Some(found));
            found
        })
    })
}

/// Reads the Windows setting again. True when it changed.
pub fn refresh() -> bool {
    let found = detect();
    MODE.with(|m| m.replace(Some(found))) != Some(found)
}

pub fn dark() -> bool {
    match mode() {
        Mode::Dark => true,
        Mode::Light => false,
        // Dark when the high contrast window colour is.
        Mode::Contrast => luminance(color(Role::Bg)) < 128,
    }
}

pub fn contrast() -> bool {
    mode() == Mode::Contrast
}

fn luminance(c: u32) -> u32 {
    (((c >> 16) & 0xFF) * 3 + ((c >> 8) & 0xFF) * 6 + (c & 0xFF)) / 10
}

/// The colour for a role, as 0xAARRGGBB.
pub fn color(role: Role) -> u32 {
    let i = role as usize;
    if i >= COUNT {
        return 0;
    }
    match mode() {
        Mode::Light => LIGHT[i],
        Mode::Dark => DARK[i],
        Mode::Contrast => match CONTRAST[i] {
            CLEAR => 0,
            index => argb(unsafe { GetSysColor(index as i32) }),
        },
    }
}

/// A Windows COLORREF (0x00BBGGRR) as an opaque 0xAARRGGBB.
pub fn argb(colorref: u32) -> u32 {
    0xFF00_0000 | ((colorref & 0xFF) << 16) | (colorref & 0xFF00) | ((colorref >> 16) & 0xFF)
}

/// An 0xAARRGGBB colour as a COLORREF, ignoring its alpha.
pub fn colorref(argb: u32) -> u32 {
    ((argb & 0xFF) << 16) | (argb & 0xFF00) | ((argb >> 16) & 0xFF)
}

fn proc_of(dll: &str, name: *const u8) -> *const c_void {
    unsafe {
        let module = LoadLibraryW(wide(dll).as_ptr());
        if module.is_null() {
            return std::ptr::null();
        }
        GetProcAddress(module, name).map_or(std::ptr::null(), |f| f as *const c_void)
    }
}

type SetAttribute = unsafe extern "system" fn(HWND, u32, *const c_void, u32) -> i32;

/// Makes a window's frame match: a dark title bar in dark mode, rounded corners, and a caption the colour of the
/// window, so the title bar and the window read as one surface (Windows 11; older versions ignore what they lack).
pub fn frame(hwnd: HWND) {
    let f = proc_of("dwmapi.dll", c"DwmSetWindowAttribute".as_ptr().cast());
    if f.is_null() {
        return;
    }
    let set: SetAttribute = unsafe { std::mem::transmute(f) };
    let dark = u32::from(dark());
    let round = 2u32;
    let caption = colorref(color(Role::Bg));
    let text = colorref(color(Role::Text));
    unsafe {
        set(hwnd, 20, (&raw const dark).cast(), 4);
        set(hwnd, 33, (&raw const round).cast(), 4);
        if !contrast() {
            set(hwnd, 35, (&raw const caption).cast(), 4);
            set(hwnd, 36, (&raw const text).cast(), 4);
        }
    }
}

/// Asks for dark or light menus (the tray menu and drop-downs), to match. Undocumented uxtheme exports, by ordinal:
/// 135 `SetPreferredAppMode` and 136 `FlushMenuThemes`; missing on old Windows, where menus stay light.
pub fn menus() {
    let set = proc_of("uxtheme.dll", 135usize as *const u8);
    let flush = proc_of("uxtheme.dll", 136usize as *const u8);
    if set.is_null() || flush.is_null() {
        return;
    }
    unsafe {
        let set: unsafe extern "system" fn(i32) -> i32 = std::mem::transmute(set);
        let flush: unsafe extern "system" fn() = std::mem::transmute(flush);
        // 2 forces dark, 3 forces light.
        set(if dark() && !contrast() { 2 } else { 3 });
        flush();
    }
}
