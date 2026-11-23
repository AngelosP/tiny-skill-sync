//! Colours: the Windows 11 light and dark palettes, the accent colours and the header band's picture, worked out from
//! the user's Windows accent colour, and the high contrast colours when that is on.
//!
//! The app follows the "app mode" the user chose in Windows (`AppsUseLightTheme`) and the seven shades Windows makes
//! from the accent colour (`AccentPalette`). `TSS_THEME=dark` or `light` and `TSS_ACCENT` (seven `RRGGBB` shades,
//! lightest first, separated by commas) override them, for screenshots. The colours that depend on the accent are
//! worked out the first time a window asks for one, so the tray, which needs only the mode, never does. The title bar
//! and menus are asked to match through DWM and uxtheme, both loaded only when a window opens, so the idle tray app
//! never loads them.

use std::cell::RefCell;
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
    /// The header band's sky, title, caption and bottom rule.
    Band,
    BandText,
    BandCaption,
    BandEdge,
    /// The type icons on their discs of sky, riding the middle hill.
    BandMark,
    /// The check-dot for "in sync": its disc, the check on it and its thin edge.
    DotOk,
    DotOkInk,
    DotOkEdge,
    /// The band's main button (Sync now, Overwrite, Restore).
    BandButton,
    BandButtonEdge,
    BandButtonText,
    BandButtonHover,
    BandButtonDown,
    None,
}

const COUNT: usize = Role::None as usize;

/// 0xAARRGGBB, in the order of [`Role`]. `Accent` and `AccentText` are worked out by [`palette`].
#[rustfmt::skip]
const LIGHT: [u32; COUNT] = [
    0xFFF3F3F3, 0xFFF9F9F9, 0x0F000000, 0xFFFFFFFF, 0x13000000, 0x12000000, 0xFF1A1A1A, 0xFF5D5D5D, 0xFF8B8B8B,
    0, 0, 0xFFFFFFFF, 0xFFFDFDFD, 0x14000000, 0x2E000000, 0x09000000, 0x0E000000,
    0xFF0F7B0F, 0xFFDFF6DD, 0xFF905500, 0xFFFFF4CE, 0xFFBD291B, 0xFFFDE7E9, 0xFF6B6B6B, 0x0F000000,
    0xFFE7F3E7, 0xFFD3EAD3, 0xFFFCEBEA, 0xFFF6D5D2, 0xFFF2F5F9, 0xFFF3F3F3, 0xFFB07300, 0xFFFFFFFF,
    0xFFFFFFFF, 0xFF202020, 0xFF505050, 0xFFC0C0C0, 0xFF404040,
    0xFF389457, 0xFFFFFFFF, 0xFF206F3A, 0xFF252525, 0xFFFFFFFF, 0xFFFFFFFF, 0xFF383838, 0xFF484848,
];

#[rustfmt::skip]
const DARK: [u32; COUNT] = [
    0xFF202020, 0xFF272727, 0x0DFFFFFF, 0xFF2D2D2D, 0xFF1B1B1B, 0x0FFFFFFF, 0xFFFFFFFF, 0xFFCFCFCF, 0xFF9A9A9A,
    0, 0, 0xFF000000, 0xFF353535, 0x12FFFFFF, 0x1AFFFFFF, 0x0DFFFFFF, 0x14FFFFFF,
    0xFF6CCB5F, 0xFF393D1B, 0xFFFCE100, 0xFF433519, 0xFFFF99A4, 0xFF442726, 0xFFA8A8A8, 0x14FFFFFF,
    0xFF213226, 0xFF2A4231, 0xFF3B2528, 0xFF4F2E33, 0xFF2A2F36, 0xFF202020, 0xFFFCE100, 0xFF000000,
    0xFF000000, 0xFFF8F8F8, 0xFFD0D0D0, 0xFF505050, 0xFFE0E0E0,
    0xFF5FB574, 0xFF000000, 0xFF5FB574, 0xFFF2F2F2, 0xFF101010, 0xFF101010, 0xFFE0E0E0, 0xFFCCCCCC,
];

// The system colour indexes high contrast uses.
const WINDOW: u8 = 5;
const WINDOW_TEXT: u8 = 8;
const HIGHLIGHT: u8 = 13;
const HIGHLIGHT_TEXT: u8 = 14;
const GRAY_TEXT: u8 = 17;
const HOT_LIGHT: u8 = 26;
/// Drawn as nothing in high contrast: decorative inks and tints.
const CLEAR: u8 = 0;

/// The system colour for each role in high contrast.
#[rustfmt::skip]
const CONTRAST: [u8; COUNT] = [
    WINDOW, WINDOW, WINDOW_TEXT, WINDOW, WINDOW_TEXT, WINDOW_TEXT, WINDOW_TEXT, WINDOW_TEXT, GRAY_TEXT,
    HIGHLIGHT, HOT_LIGHT, HIGHLIGHT_TEXT, WINDOW, WINDOW_TEXT, WINDOW_TEXT, CLEAR, CLEAR,
    WINDOW_TEXT, WINDOW, WINDOW_TEXT, WINDOW, WINDOW_TEXT, WINDOW, WINDOW_TEXT, CLEAR,
    WINDOW, WINDOW, WINDOW, WINDOW, WINDOW, WINDOW, WINDOW_TEXT, WINDOW,
    WINDOW, WINDOW_TEXT, WINDOW_TEXT, WINDOW_TEXT, CLEAR,
    WINDOW_TEXT, WINDOW, WINDOW_TEXT, HIGHLIGHT, HIGHLIGHT_TEXT, HIGHLIGHT_TEXT, HIGHLIGHT, HIGHLIGHT,
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Light,
    Dark,
    Contrast,
}

/// The seven shades Windows makes from the accent colour, as 0xRRGGBB: light 3, 2 and 1, the accent itself, then dark
/// 1, 2 and 3.
type Ramp = [u32; 7];

/// Windows' default blue.
const BLUE: Ramp = [0x99EBFF, 0x4CC2FF, 0x0091F8, 0x0078D4, 0x0067C0, 0x003E92, 0x001A68];

/// What the colours are worked out from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Look {
    mode: Mode,
    ramp: Ramp,
}

struct Palette {
    roles: [u32; COUNT],
    hills: [u32; 5],
}

struct State {
    look: Look,
    /// Worked out when first asked for; never in high contrast.
    palette: Option<Palette>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

#[repr(C)]
struct HighContrast {
    size: u32,
    flags: u32,
    scheme: *mut u16,
}

const HCF_HIGHCONTRASTON: u32 = 1;
const RRF_RT_REG_BINARY: u32 = 0x08;
const RRF_RT_REG_DWORD: u32 = 0x10;
/// HKEY_CURRENT_USER, sign-extended as Windows defines it.
const HKCU: isize = 0x8000_0001u32 as i32 as isize;

#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegGetValueW(key: isize, subkey: *const u16, value: *const u16, flags: u32, kind: *mut u32, data: *mut c_void, len: *mut u32) -> u32;
}

fn detect_mode() -> Mode {
    let mut hc = HighContrast { size: size_of::<HighContrast>() as u32, flags: 0, scheme: null_mut() };
    if unsafe { SystemParametersInfoW(SPI_GETHIGHCONTRAST, hc.size, (&raw mut hc).cast(), 0) } != 0 && hc.flags & HCF_HIGHCONTRASTON != 0 {
        return Mode::Contrast;
    }
    match std::env::var("TSS_THEME").as_deref() {
        Ok("dark") => return Mode::Dark,
        Ok("light") => return Mode::Light,
        _ => {}
    }
    let mut light = 1u32;
    let mut len = size_of::<u32>() as u32;
    let key = wide(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
    let found =
        unsafe { RegGetValueW(HKCU, key.as_ptr(), wide("AppsUseLightTheme").as_ptr(), RRF_RT_REG_DWORD, null_mut(), (&raw mut light).cast(), &mut len) };
    if found == 0 && light == 0 { Mode::Dark } else { Mode::Light }
}

/// `TSS_ACCENT` when it is seven shades, else the accent the user chose in Windows, else Windows' default blue.
fn accent() -> Ramp {
    if let Some(ramp) = std::env::var("TSS_ACCENT").ok().as_deref().and_then(parse_ramp) {
        return ramp;
    }
    let mut bytes = [0u8; 32];
    let mut len = bytes.len() as u32;
    let key = wide(r"Software\Microsoft\Windows\CurrentVersion\Explorer\Accent");
    let found = unsafe { RegGetValueW(HKCU, key.as_ptr(), wide("AccentPalette").as_ptr(), RRF_RT_REG_BINARY, null_mut(), bytes.as_mut_ptr().cast(), &mut len) };
    if found == 0 && len >= 28 { ramp_of(&bytes) } else { BLUE }
}

/// Seven `RRGGBB` shades separated by commas, each with or without a `#`.
fn parse_ramp(text: &str) -> Option<Ramp> {
    let mut parts = text.split(',');
    let mut ramp = [0; 7];
    for shade in &mut ramp {
        let hex = parts.next()?.trim();
        let hex = hex.strip_prefix('#').unwrap_or(hex);
        if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        *shade = u32::from_str_radix(hex, 16).ok()?;
    }
    parts.next().is_none().then_some(ramp)
}

/// `AccentPalette` holds eight colours as red, green, blue and an unused byte; the first seven are the shades.
fn ramp_of(bytes: &[u8; 32]) -> Ramp {
    std::array::from_fn(|i| (u32::from(bytes[i * 4]) << 16) | (u32::from(bytes[i * 4 + 1]) << 8) | u32::from(bytes[i * 4 + 2]))
}

fn detect() -> Look {
    Look { mode: detect_mode(), ramp: accent() }
}

fn with_state<T>(f: impl FnOnce(&mut State) -> T) -> T {
    STATE.with(|s| f(s.borrow_mut().get_or_insert_with(|| State { look: detect(), palette: None })))
}

fn mode() -> Mode {
    with_state(|s| s.look.mode)
}

/// What the palette says, worked out on first use; nothing in high contrast, which uses the system colours.
fn derived<T>(f: impl FnOnce(&Palette) -> T) -> Option<T> {
    with_state(|s| {
        let look = s.look;
        (look.mode != Mode::Contrast).then(|| f(s.palette.get_or_insert_with(|| palette(look))))
    })
}

/// Reads the Windows settings again. True when they changed.
pub fn refresh() -> bool {
    let look = detect();
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let changed = s.as_ref().is_none_or(|s| s.look != look);
        if changed {
            *s = Some(State { look, palette: None });
        }
        changed
    })
}

/// The header band's picture; none in high contrast, where the band stays plain.
pub fn hills() -> Option<[u32; 5]> {
    derived(|p| p.hills)
}

/// Puts a look in place of what Windows says (none detects it again on next use) and returns the one it replaced.
#[cfg(test)]
fn set(look: Option<Look>) -> Option<Look> {
    STATE.with(|s| s.replace(look.map(|look| State { look, palette: None })).map(|s| s.look))
}

#[cfg(test)]
pub(super) fn with_light_theme(test: impl FnOnce()) {
    let saved = set(Some(Look { mode: Mode::Light, ramp: BLUE }));
    test();
    set(saved);
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
    derived(|p| p.roles[i]).unwrap_or_else(|| match CONTRAST[i] {
        CLEAR => 0,
        index => argb(unsafe { GetSysColor(index as i32) }),
    })
}

const WHITE: u32 = 0xFF_FFFF;
const BLACK: u32 = 0;
/// Contrast for page links and button lettering.
const READ: f64 = 4.6;
/// Contrast for buttons and switches.
const SHAPE: f64 = 3.0;

/// `a` moved `t` of the way to `b`, channel by channel.
fn mix(a: u32, b: u32, t: f64) -> u32 {
    [16, 8, 0].into_iter().fold(0, |out, shift| {
        let (x, y) = (f64::from((a >> shift) & 0xFF), f64::from((b >> shift) & 0xFF));
        out | (((x + (y - x) * t).round() as u32) << shift)
    })
}

/// The WCAG relative luminance.
fn lum(c: u32) -> f64 {
    let linear = |shift: u32| {
        let v = f64::from((c >> shift) & 0xFF) / 255.0;
        if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * linear(16) + 0.7152 * linear(8) + 0.0722 * linear(0)
}

/// The WCAG contrast ratio.
fn ratio(a: u32, b: u32) -> f64 {
    let (a, b) = (lum(a), lum(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// Back a hollow mark only when its fixed tray ink would disappear against the high contrast band.
pub(super) fn logo_backing(look: crate::logo::Look) -> Option<u32> {
    let crate::logo::Look::Hollow(ink) = look else { return None };
    if !contrast() || ratio(ink, color(Role::Band)) >= SHAPE {
        return None;
    }
    Some(0xFF00_0000 | if ratio(ink, BLACK) > ratio(ink, WHITE) { BLACK } else { WHITE })
}

/// The first colour from `from` toward `to`, in twentieths, that is `ok`; `to` when none is.
fn toward(from: u32, to: u32, ok: impl Fn(u32) -> bool) -> u32 {
    (0..=20).map(|k| mix(from, to, f64::from(k) / 20.0)).find(|c| ok(*c)).unwrap_or(to)
}

/// Transfer the five reference tones to the Windows hue. Each swatch gives a channel curve (shadow,
/// midtone, highlight); the accent's saturation determines how much colour to use.
fn hill_colors(light: bool, ramp: Ramp) -> [u32; 5] {
    let swatches = if light { [0xDAEBF8, 0xB2D4EF, 0x7CB3DF, 0x438FC8, 0x186BA9] } else { [0x76CFFF, 0x44B5FF, 0x1599FA, 0x087FD9, 0x0665B5] };
    let channels = |c: u32| [16, 8, 0].map(|s| f64::from((c >> s) & 255u32));
    let range = |[r, g, b]: [f64; 3]| (r.min(g).min(b), r.max(g).max(b));
    let shade = channels(ramp[2]);
    let (lo, hi) = range(shade);
    let base = channels(ramp[3]);
    let (base_lo, base_hi) = range(base);
    let saturation = 0.75 * (base_hi - base_lo) / base_hi.max(1.0) + 0.25 * (hi - lo) / hi.max(1.0);
    // A grey shade contributes to saturation as before, but has no hue of its own.
    let (shade, lo, hi) = if hi == lo { (base, base_lo, base_hi) } else { (shade, lo, hi) };
    let hue = shade.map(|c| (c - lo) / (hi - lo).max(1.0));
    let blue_mid = 145.0 / 248.0; // middle channel of BLUE[2], normalized between its red and blue
    std::array::from_fn(|k| {
        let [shadow, mid, highlight] = channels(swatches[k]);
        let strength = if light { saturation } else { saturation.powf(0.70 - 0.14 * k.saturating_sub(2) as f64) };
        // Muted accents approach a stepped grey, not five whites. Give warm dark-mode highlights some headroom.
        let grey = shadow + (highlight - shadow) * (0.35 + 0.30 * base_hi / 255.0);
        let peak = if light { highlight } else { highlight + (255.0 - highlight) * 0.36 * shade[0] / hi.max(1.0) };
        let peak = grey + (peak - grey) * (3.0 * saturation).min(1.0);
        hue.into_iter().fold(0, |rgb, u| {
            let tone = if u <= blue_mid { shadow + (mid - shadow) * u / blue_mid } else { mid + (highlight - mid) * (u - blue_mid) / (1.0 - blue_mid) };
            (rgb << 8) | (peak * (1.0 - strength * (1.0 - tone / highlight))).round() as u32
        })
    })
}

fn palette(Look { mode, ramp }: Look) -> Palette {
    let light = mode != Mode::Dark;
    let table = if light { &LIGHT } else { &DARK };
    let rgb = |role: Role| table[role as usize] & WHITE;
    let [l3, l2, _, _, d1, _, _] = ramp;
    let pages = [Role::Bg, Role::Layer, Role::Card].map(rgb);
    // Every surface a button or link sits on: the pages, and the pages under a hovered or selected row.
    let surfaces =
        [0, table[Role::Hover as usize], table[Role::Sel as usize]].map(|tint| pages.map(|page| mix(page, tint & WHITE, f64::from(tint >> 24) / 255.0)));
    let (on, end) = (rgb(Role::OnAccent), if light { BLACK } else { WHITE });
    let against_pages = |c, minimum| surfaces.iter().flatten().all(|s| ratio(c, *s) >= minimum);
    let accent = toward(if light { d1 } else { l2 }, end, |c| ratio(c, on) >= READ && against_pages(c, SHAPE));
    let accent_text = toward(if light { d1 } else { l3 }, end, |c| against_pages(c, READ));

    let mut roles = *table;
    roles[Role::Accent as usize] = 0xFF00_0000 | accent;
    roles[Role::AccentText as usize] = 0xFF00_0000 | accent_text;
    Palette { roles, hills: hill_colors(light, ramp) }
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
/// window's background (`Bg`), so the title bar matches the window's edges and the band under it stands out from both
/// (Windows 11; older versions ignore what they lack).
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::win::ui::{control, page, paint, paint::Tone, scale};

    /// The WCAG relative luminance of a colour.
    fn luminance_of(c: u32) -> f64 {
        let linear = |shift: u32| {
            let v = f64::from((c >> shift) & 0xFF) / 255.0;
            if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * linear(16) + 0.7152 * linear(8) + 0.0722 * linear(0)
    }

    fn contrast_of(a: u32, b: u32) -> f64 {
        let (a, b) = (luminance_of(a), luminance_of(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    /// The red, green and blue differences added up.
    fn distance(a: u32, b: u32) -> u32 {
        [16, 8, 0].into_iter().map(|s| ((a >> s) & 0xFF).abs_diff((b >> s) & 0xFF)).sum()
    }

    fn render(decos: &[control::Deco], dpi: u32, [x, y, width, height]: paint::Rect) -> Vec<u32> {
        assert!(paint::begin(dpi, width, height, x, y));
        control::draw(decos);
        paint::pixels()
    }

    fn render_band(w: i32, dpi: u32, at: paint::Rect) -> Vec<u32> {
        render(&page::band(w), dpi, at)
    }

    /// The band with the type icons on it.
    fn marked_band(w: i32) -> Vec<control::Deco> {
        let mut decos = page::band(w).to_vec();
        decos.extend(page::stations(w, &[]));
        decos
    }

    /// All painted backgrounds including the soft edges of the hills, apart from the bottom rule.
    fn band_colors(w: i32) -> Vec<u32> {
        let mut pixels = render_band(w, 96, [0, 0, w, page::HERO_H]);
        pixels.truncate(pixels.len() - w as usize);
        pixels.sort_unstable();
        pixels.dedup();
        pixels
    }

    /// Windows' blue and ten other accents, as Windows shades them, from the vivid to the nearly grey, the palest and
    /// the darkest; then one set by hand (in the registry or `TSS_ACCENT`) whose darkest shade is still pale.
    #[rustfmt::skip]
    const RAMPS: [Ramp; 12] = [
        BLUE,
        [0xE3D4FE, 0xC2A6EF, 0xA079D8, 0x8764B8, 0x7755A5, 0x513279, 0x321352],
        [0xFED1D7, 0xFF97A8, 0xFF4476, 0xEA005E, 0xCE0252, 0x8C0035, 0x54001C],
        [0xB7E5E7, 0x75C2C5, 0x1B9EA2, 0x038387, 0x067276, 0x004C4E, 0x002B2D],
        [0xFFD2C1, 0xFE9A73, 0xEC611D, 0xCA5010, 0xB44301, 0x7A2A00, 0x481500],
        [0xFFEFD5, 0xFFDCA0, 0xFFC75E, 0xFFB900, 0xE0A203, 0x986D02, 0x5C4001],
        [0xB8E6B4, 0x76C171, 0x279A25, 0x107C10, 0x006D01, 0x004800, 0x002900],
        [0xFFFDD3, 0xFFFA9E, 0xFFF560, 0xFFF100, 0xE0D407, 0x999001, 0x5C5602],
        [0xF9F9F9, 0xF3F3F3, 0xECECEC, 0xE8E8E8, 0xCCCCCC, 0x8A8A8A, 0x535353],
        [0xC2C2C2, 0x808080, 0x424242, 0x1F1F1F, 0x1A1A1A, 0x0E0E0E, 0x050505],
        [0xDEDBDB, 0xB8B4B3, 0x928C8B, 0x7A7574, 0x6B6665, 0x474342, 0x292524],
        [0xFFFFFF, 0xF4F4F4, 0xE8E8E8, 0xDCDCDC, 0xD0D0D0, 0xC4C4C4, 0xB8B8B8],
    ];

    /// Runs `check` with every accent in light and in dark mode, then puts back what was there.
    fn each_look(mut check: impl FnMut(Look)) {
        let saved = set(None);
        for ramp in RAMPS {
            for mode in [Mode::Light, Mode::Dark] {
                set(Some(Look { mode, ramp }));
                check(Look { mode, ramp });
            }
        }
        set(saved);
        paint::release();
    }

    /// The colour a 1 by 1 canvas shows after filling it with each role in turn.
    fn layered(roles: &[Role]) -> u32 {
        assert!(paint::begin(96, 1, 1, 10, 10));
        for role in roles {
            paint::fill([0, 0, 32, 32], 4, *role, Role::None);
        }
        paint::pixels()[0]
    }

    #[test]
    fn accent_buttons_and_links_stay_readable_with_every_accent() {
        each_look(|look| {
            assert_eq!(color(Role::Accent) >> 24, 255, "{look:?}: the page must not show through the button");
            for tint in [Role::None, Role::Hover, Role::Sel] {
                assert!(contrast_of(color(Role::OnAccent), layered(&[Role::Accent, tint])) >= 4.5, "{look:?}: {tint:?} button");
                for page in [Role::Bg, Role::Layer, Role::Card] {
                    let under = layered(&[page, tint]);
                    assert!(contrast_of(color(Role::AccentText), under) >= 4.5, "{look:?}: a link on {page:?} under {tint:?}");
                    assert!(contrast_of(color(Role::Accent), under) >= 3.0, "{look:?}: a button or switch on {page:?} under {tint:?}");
                }
            }
        });
    }

    #[test]
    fn blue_matches_the_approved_swatches_and_iris_and_gold_keep_their_glow() {
        assert_eq!(hill_colors(true, BLUE), [0xDAEBF8, 0xB2D4EF, 0x7CB3DF, 0x438FC8, 0x186BA9]);
        assert_eq!(hill_colors(false, BLUE), [0x76CFFF, 0x44B5FF, 0x1599FA, 0x087FD9, 0x0665B5]);
        let iris = [0xCFB0FF, 0xB892FF, 0xA477FC, 0x8959E9, 0x713ECF];
        let gold = [0xFFDA80, 0xFFC150, 0xF9AA2A, 0xE99212, 0xC9790D];
        for (ramp, sample) in [(RAMPS[1], iris), (RAMPS[5], gold)] {
            for (hill, want) in hill_colors(false, ramp).into_iter().zip(sample) {
                for shift in [16, 8, 0] {
                    assert!(((hill >> shift) & 255u32).abs_diff((want >> shift) & 255) <= 14, "got #{hill:06x}, target #{want:06x}");
                }
            }
        }
    }

    #[test]
    fn hills_keep_the_base_hue_when_the_lighter_accent_is_grey() {
        let mut ramp = BLUE;
        ramp[2] = 0xBBBBBB;
        ramp[3] = 0x008040;
        for light in [true, false] {
            for hill in hill_colors(light, ramp) {
                let [_, r, g, b] = hill.to_be_bytes();
                assert!(g > b && b > r, "#{hill:06x} lost the base accent's green hue (light={light})");
            }
        }
    }

    #[test]
    fn the_band_button_reads_and_stands_out_from_every_layer_of_the_band_at_rest_hovered_and_pressed() {
        each_look(|look| {
            let layers = band_colors(640);
            let mut fills = Vec::new();
            for tint in [Role::None, Role::Hover, Role::Sel] {
                let (face, border, ink) = control::Kind::BandAccent.colors(true, tint);
                let (fill, edge, text) = (color(face), color(border), color(ink));
                for c in [fill, edge, text] {
                    assert_eq!(c >> 24, 255);
                    assert_eq!(c & 0xFFFF, (c >> 8) & 0xFFFF, "{look:?}: neutral button");
                }
                assert_eq!(luminance_of(fill) > 0.5, look.mode == Mode::Dark);
                assert!(contrast_of(text, fill) >= 4.5 && contrast_of(edge, fill) >= 3.0);
                assert!(!fills.contains(&fill), "{look:?}: the fill changes for {tint:?}");
                fills.push(fill);
                for under in &layers {
                    let shown = contrast_of(fill, *under).max(contrast_of(edge, *under));
                    assert!(shown >= 3.0, "{look:?}: the button under {tint:?} on #{under:06x} stands out {shown:.2}:1");
                }
            }
        });
    }

    #[test]
    fn the_check_dot_is_soft_but_its_check_and_its_outline_stay_clear() {
        each_look(|look| {
            let rgb = |role| color(role) & 0xFFFFFF;
            // v0.13's disc was a deep green (#0F7B0F, luminance 0.14) in light and a bright one (#6CCB5F, 0.47) in dark.
            let glow = luminance_of(rgb(Tone::Ok.dot()));
            assert!(if look.mode == Mode::Dark { glow <= 0.42 } else { glow >= 0.19 }, "{look:?}: the check-dot's disc ({glow:.2}) is no softer than before");
            for tone in [Tone::Ok, Tone::Warn, Tone::Err, Tone::Off] {
                assert!(contrast_of(rgb(tone.dot_ink()), rgb(tone.dot())) >= 3.0, "{look:?}: the mark on the {tone:?} dot");
            }
            // The disc or its thin edge, whichever shows more, must read as a dot on the pages and under a hovered or
            // selected row.
            for page in [Role::Bg, Role::Layer, Role::Card] {
                for tint in [Role::None, Role::Hover, Role::Sel] {
                    let under = layered(&[page, tint]);
                    let shown = contrast_of(rgb(Tone::Ok.dot()), under).max(contrast_of(rgb(Tone::Ok.dot_edge()), under));
                    assert!(shown >= 3.0, "{look:?}: the check-dot on {page:?} under {tint:?} stands out {shown:.2}:1");
                }
            }
        });
    }

    #[test]
    fn a_dot_is_drawn_in_its_own_ink_and_only_the_check_dot_has_an_edge() {
        each_look(|look| {
            let rgb = |role| color(role) & 0xFFFFFF;
            for tone in [Tone::Ok, Tone::Warn, Tone::Err, Tone::Off] {
                assert!(paint::begin(96, 16, 16, 0, 0));
                paint::fill([0, 0, 16, 16], 0, Role::Bg, Role::None);
                paint::dot([0, 0, 16, 16], tone);
                let pixels = paint::pixels();
                let (disc, ink) = (rgb(tone.dot()), rgb(tone.dot_ink()));
                let edge = if tone == Tone::Ok { rgb(tone.dot_edge()) } else { disc };
                // The edge is the outermost pixel of the dot's middle row.
                assert!(distance(pixels[8 * 16], edge) <= 12, "{look:?}: the edge of the {tone:?} dot is #{:06x}", pixels[8 * 16]);
                assert!(distance(pixels[8 * 16 + 1], disc) <= 12, "{look:?}: inside the {tone:?} dot is #{:06x}", pixels[8 * 16 + 1]);
                // The mark is thin at 16 DIPs, but at its strongest it is a quarter of the way from the disc's colour to its ink.
                let strongest = (3..13usize).flat_map(|y| (3..13usize).map(move |x| y * 16 + x)).map(|i| distance(pixels[i], ink)).min().expect("pixels");
                assert!(4 * strongest <= 3 * distance(disc, ink), "{look:?}: the mark on the {tone:?} dot is not in its ink #{ink:06x}");
            }
        });
    }

    #[test]
    fn entry_chip_inks_and_text_read_on_every_entry_background() {
        let check = |look: Look| {
            let light = look.mode == Mode::Light;
            for tint in [Role::None, Role::Hover, Role::Sel] {
                let entry = layered(&[Role::Bg, tint]);
                let chip = |fill| layered(&[Role::Bg, tint, fill]);
                let reads = |what: &str, ink: Role, under: u32, floor: f64| {
                    let shown = contrast_of(color(ink), under);
                    assert!(shown >= floor, "{look:?}: {what} under {tint:?} reads {shown:.2}:1, below {floor:.1}:1");
                };
                reads("the repository", Role::Text, entry, READ);
                reads("the owner and an up to date status", Role::Text2, entry, READ);
                reads("the label of a chip", Role::Text, chip(Role::Ctl), READ);
                reads("the icon of a chip", Role::Text2, chip(Role::Ctl), READ);
                reads("a chip that needs the user", Role::Warn, chip(Role::WarnBg), READ);
                reads("a status that needs the user", Role::Warn, entry, READ);
                reads("a status of a failed check", Role::Err, entry, READ);
                // An off chip is an inactive control, which WCAG leaves out of its contrast rule, and what it says ("Sync off") is also
                // in the entry's tooltip and its accessible name. Its faint ink is checked where it reaches SHAPE; on a hovered or
                // selected entry in light mode it does not (2.8 and 2.7:1), and in high contrast the gray is the user's choice.
                if !(light && tint != Role::None) && look.mode != Mode::Contrast {
                    reads("a chip of a type that is off", Role::Text3, entry, SHAPE);
                }
            }
        };
        each_look(check);
        let high = Look { mode: Mode::Contrast, ramp: BLUE };
        let saved = set(Some(high));
        check(high);
        set(saved);
        paint::release();
    }

    #[test]
    fn the_warning_and_error_inks_read_on_every_page_tint_and_fill() {
        // Every status word, note and hint uses these two inks, so they are held on each page, under a hovered or selected row, and
        // as the text of a pill or chip on its own fill.
        each_look(|look| {
            for page in [Role::Bg, Role::Layer, Role::Card] {
                for tint in [Role::None, Role::Hover, Role::Sel] {
                    for (what, ink, own) in [("a warning", Role::Warn, Role::WarnBg), ("an error", Role::Err, Role::ErrBg)] {
                        for (under, fill) in [(layered(&[page, tint]), Role::None), (layered(&[page, tint, own]), own)] {
                            let shown = contrast_of(color(ink), under);
                            assert!(shown >= READ, "{look:?}: {what} on {page:?} under {tint:?} over {fill:?} reads {shown:.2}:1, below {READ:.1}:1");
                        }
                    }
                }
            }
        });
    }

    #[test]
    fn the_focus_ring_stays_visible_on_every_entry_background() {
        // The ring is the text ink, drawn over an entry and its tint on any of the pages. Over the accent bar of a selected
        // entry it reads with some accents only (Text on Accent: 1.0 to 3.6:1 in light mode, 1.1 to 3.3:1 in dark), so the
        // bar is left out.
        let check = |look: Look| {
            for page in [Role::Bg, Role::Layer, Role::Card] {
                for tint in [Role::None, Role::Hover, Role::Sel] {
                    let shown = contrast_of(color(Role::Text), layered(&[page, tint]));
                    assert!(shown >= SHAPE, "{look:?}: the focus ring on {page:?} under {tint:?} reads {shown:.2}:1");
                }
            }
        };
        each_look(check);
        let high = Look { mode: Mode::Contrast, ramp: BLUE };
        let saved = set(Some(high));
        check(high);
        set(saved);
        paint::release();
    }

    #[test]
    fn high_contrast_keeps_the_system_colours_for_the_dots_and_header() {
        let saved = set(Some(Look { mode: Mode::Contrast, ramp: BLUE }));
        for (new, old) in [
            (Role::DotOk, Role::Ok),
            (Role::DotOkInk, Role::DotGlyph),
            (Role::DotOkEdge, Role::Ok),
            (Role::Band, Role::Bg),
            (Role::BandEdge, Role::Text),
            (Role::BandText, Role::Text),
            (Role::BandCaption, Role::Text),
            (Role::BandButton, Role::Accent),
            (Role::BandButtonHover, Role::Accent),
            (Role::BandButtonDown, Role::Accent),
            (Role::BandButtonEdge, Role::OnAccent),
            (Role::BandButtonText, Role::OnAccent),
        ] {
            assert_eq!(color(new), color(old), "{new:?} in high contrast");
        }
        set(saved);
    }

    #[test]
    fn a_hollow_logo_stays_visible_when_its_ink_matches_the_high_contrast_window() {
        let saved = set(Some(Look { mode: Mode::Contrast, ramp: BLUE }));
        let at = [0, 0, 40, 40];
        let window = color(Role::Band) & WHITE;
        let opposite = if contrast_of(window, BLACK) > contrast_of(window, WHITE) { BLACK } else { WHITE };
        // Match the ink to Window here rather than changing the user's system colours.
        for ink in [window, opposite] {
            assert!(paint::begin(96, 40, 40, 0, 0));
            paint::fill(at, 0, Role::Band, Role::None);
            paint::logo(at, crate::logo::Look::Hollow(ink));
            let pixels = paint::pixels();
            assert_eq!(pixels[20 * 40 + 20], ink, "the spark keeps its tray colour");
            assert_eq!(pixels[0], window, "no backing outside the tile");
            let gap = pixels[20 * 40 + 10];
            if ink == window {
                assert_eq!(gap, opposite, "the hollow space must contrast with the spark");
            } else {
                assert_eq!(gap, window, "no backing when the ink already contrasts");
            }
        }
        set(saved);
        paint::release();
    }

    #[test]
    fn the_band_renders_five_accent_hills_on_flat_neutral_sky_or_a_plain_system_band() {
        let band_at = |w| [0, 0, w, page::HERO_H];
        let rgb = |role| color(role) & 0xFFFFFF;
        let saved = set(Some(Look { mode: Mode::Contrast, ramp: BLUE }));
        assert_eq!(hills(), None);
        let mut pixels = render_band(640, 96, band_at(640));
        let rule = pixels.split_off(pixels.len() - 640);
        assert!(rule.iter().all(|p| *p == rgb(Role::Text)) && pixels.iter().all(|p| *p == rgb(Role::Bg)), "high contrast keeps the band plain");
        set(saved);
        let mut prints = Vec::new();
        each_look(|look| {
            let hills = hills().expect("a picture outside high contrast");
            let sky = if look.mode == Mode::Dark { BLACK } else { WHITE };
            prints.push((look.mode == Mode::Dark, hills));
            assert_eq!(rgb(Role::Band), sky, "{look:?}: the icons' discs are sky");
            let w = 860;
            let pixels = render_band(w, 96, band_at(w));
            assert!(pixels[..w as usize].iter().all(|p| *p == sky), "{look:?}: sky at the top");
            assert!(pixels[(w * (page::HERO_H - 1)) as usize..].iter().all(|p| *p == rgb(Role::BandEdge)), "{look:?}: the rule under the band");
            // A hundred DIPs from the right edge every hill is up, each top at least 4 DIPs under the one behind it. Just
            // under its top, each hill shows its own colour, at 100% and at 200%.
            for dpi in [96, 192] {
                let (width, height) = (scale(w, dpi), scale(page::HERO_H, dpi));
                let pixels = render_band(w, dpi, [0, 0, width, height]);
                let s = dpi as f32 / 96.0;
                let x = width - (100.0 * s) as i32;
                let v = ((width - x) as f32 - 0.5) / s;
                for (k, ink) in hills.into_iter().enumerate() {
                    let y = (height as f32 - paint::hill(k, v) * s) as i32 + 2;
                    assert_eq!(pixels[(y * width + x) as usize], ink, "{look:?}: hill {k} at {x},{y} at {dpi} DPI");
                }
            }
            assert!((0..80).all(|y| pixels[(y * w + 10) as usize] == sky), "{look:?}: no sky gradient");
            for pair in hills.windows(2) {
                assert!(distance(pair[0], pair[1]) >= 24 && luminance_of(pair[0]) > luminance_of(pair[1]), "{look:?}: distinct receding hills {pair:x?}");
            }
            for page in [Role::Bg, Role::Layer] {
                assert!(distance(rgb(Role::BandEdge), rgb(page)) >= 30, "{look:?}: the rule stands apart from {page:?}");
            }
            for ink in [Role::BandText, Role::BandCaption, Role::BandMark] {
                let c = rgb(ink);
                assert_eq!(c & 0xFFFF, (c >> 8) & 0xFFFF, "{look:?}: neutral ink");
                assert!(contrast_of(c, sky) >= 4.5);
            }
        });
        let looks = prints.len();
        prints.sort_unstable();
        prints.dedup();
        assert_eq!(prints.len(), looks, "every accent paints its own picture");
    }

    #[test]
    fn header_text_reads_as_on_sky_across_a_bright_ridge_without_erasing_the_rest_of_the_label() {
        use windows_sys::Win32::Graphics::Gdi::{DT_END_ELLIPSIS, DT_SINGLELINE, DT_VCENTER};
        let saved = set(None);
        for look in [Look { mode: Mode::Dark, ramp: RAMPS[5] }, Look { mode: Mode::Light, ramp: BLUE }] {
            set(Some(look));
            let sky = color(Role::Band) & WHITE;
            for (w, text) in [(640, "Long source and skill name · ".repeat(10)), (980, "Up to date".into())] {
                let title = ([78, 23, w - 220, 28], paint::Font::Title, Role::BandText);
                let sub = ([56, 55, w - 80, 16], paint::Font::Caption, Role::BandCaption);
                for (at, font, ink) in [title, sub] {
                    let dpi = 120;
                    let rect = [0, 0, scale(w, dpi), scale(page::HERO_H, dpi)];
                    let band = page::band(w);
                    let label = || paint::text(at, &text, font, ink, DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS);
                    render(&band[..1], dpi, rect);
                    label();
                    let on_sky = paint::pixels();
                    let artwork = render(&band, dpi, rect);
                    label();
                    let drawn = paint::pixels();
                    let end = scale(at[0] + paint::measure(&text, font, 0, dpi).0.min(at[2]) + 1, dpi);
                    let right = scale(at[0] + at[2], dpi);
                    let rows = scale(at[1], dpi)..scale(at[1] + at[3], dpi);
                    let lettering = color(ink) & WHITE;
                    let (mut visible_glyph, mut spare_art) = (false, false);
                    let mut backed = 0;
                    for (i, &pixel) in drawn.iter().enumerate() {
                        if on_sky[i] != sky {
                            assert_eq!(pixel, on_sky[i], "{look:?}: lettering crossing hills must read as on sky");
                            visible_glyph |= distance(pixel, lettering) < distance(pixel, sky);
                        }
                        let x = i as i32 % rect[2];
                        if x >= end {
                            assert_eq!(pixel, artwork[i], "{look:?}: no fill after the measured words");
                            spare_art |= x < right && rows.contains(&(i as i32 / rect[2])) && pixel != sky;
                        } else if artwork[i] != sky && pixel == sky {
                            backed += 1;
                        }
                    }
                    assert!(visible_glyph, "{look:?}: the {font:?} glyphs are visible");
                    if w == 640 {
                        assert!(backed > 10, "{look:?}: the long {font:?} run crosses the artwork");
                    } else {
                        assert_eq!(backed, 0, "{look:?}: the short run leaves the artwork alone");
                        assert!(spare_art, "{look:?}: hills show in the unused part of the {font:?} label");
                    }
                }
            }
        }
        set(saved);
        paint::release();
    }

    #[test]
    fn the_type_icons_ride_the_middle_hill_on_discs_of_sky_and_are_left_out_in_high_contrast() {
        let w = 860;
        let at = [0, 0, w, page::HERO_H];
        let discs: Vec<paint::Rect> =
            page::stations(w, &[]).iter().filter_map(|d| if let control::Deco::Fill { at, .. } = d { Some(*at) } else { None }).collect();
        assert_eq!(discs.len(), 4);
        let saved = set(Some(Look { mode: Mode::Contrast, ramp: BLUE }));
        assert!(render_band(w, 96, at) == render(&marked_band(w), 96, at), "high contrast shows the plain band, without the icons");
        set(saved);
        each_look(|look| {
            let (plain, marked) = (render_band(w, 96, at), render(&marked_band(w), 96, at));
            let rgb = |role| color(role) & 0xFFFFFF;
            let (sky, ink) = (rgb(Role::Band), rgb(Role::BandMark));
            assert!(contrast_of(ink, sky) >= 7.0, "{look:?}: the icons read on their discs");
            for &[x, y, dw, dh] in &discs {
                let (cx, cy) = (x + dw / 2, y + dh / 2);
                let beside = (cy * w + cx - 8) as usize;
                assert!(plain[beside] != sky && marked[beside] == sky, "{look:?}: the disc at {cx},{cy} shows sky where a hill was");
                let marked = &marked;
                let inside = (y..y + dh).flat_map(|py| (x..x + dw).map(move |px| marked[(py * w + px) as usize]));
                // ClearType draws thin strokes with coloured fringes, so count the pixels nearer the ink than the sky.
                assert!(inside.filter(|p| distance(*p, ink) < distance(*p, sky)).count() >= 6, "{look:?}: the disc at {cx},{cy} holds an icon in its ink");
            }
        });
    }

    #[test]
    fn repainting_a_child_samples_the_same_band_as_the_window() {
        let saved = set(Some(Look { mode: Mode::Light, ramp: BLUE }));
        for (w, dpi) in [(641, 96), (767, 120), (1021, 144)] {
            let (width, height) = (scale(w, dpi), scale(page::HERO_H, dpi));
            let decos = marked_band(w);
            let whole = render(&decos, dpi, [0, 0, width, height]);
            // Repaint where the hills climb, then a smaller rectangle among the tall hills at the right.
            for (inset, span, oy) in [(370, 180, 3), (220, 140, 9)] {
                let (ox, pw) = (width - scale(inset, dpi) + 1, scale(span, dpi));
                let child = render(&decos, dpi, [ox, oy, pw, height - oy]);
                for (i, row) in child.chunks_exact(pw as usize).enumerate() {
                    let start = (i + oy as usize) * width as usize + ox as usize;
                    assert_eq!(row, &whole[start..start + row.len()], "row {i} from {ox},{oy} at {dpi} DPI");
                }
            }
        }
        set(saved);
        paint::release();
    }

    #[test]
    fn the_accent_shades_come_from_windows_bytes_or_seven_hex_colours() {
        let mut bytes = [0u8; 32];
        for (i, shade) in BLUE.iter().enumerate() {
            bytes[i * 4..i * 4 + 3].copy_from_slice(&shade.to_be_bytes()[1..]);
        }
        bytes[28..].copy_from_slice(&[0x12, 0x34, 0x56, 0x78]);
        assert_eq!(ramp_of(&bytes), BLUE, "red, green, blue, then a byte to skip; the eighth colour is not a shade");
        let blue = "99EBFF,4CC2FF,0091F8,0078D4,0067C0,003E92,001A68";
        assert_eq!(parse_ramp(blue), Some(BLUE));
        assert_eq!(parse_ramp(" #99ebff, #4cc2ff,0091f8,0078d4,0067c0,003e92,001a68 "), Some(BLUE));
        let (more, sign, letter) = (format!("{blue},000000"), blue.replacen('9', "+", 1), blue.replacen("FF", "FG", 1));
        for bad in ["", "99EBFF", &blue[..blue.len() - 7], more.as_str(), sign.as_str(), letter.as_str()] {
            assert_eq!(parse_ramp(bad), None, "{bad:?}");
        }
    }
}
