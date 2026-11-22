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
    /// The header band's sky (the picture's colours are in [`Print`]), its title, caption and bottom rule.
    Band,
    BandText,
    BandCaption,
    BandEdge,
    /// The type icons on their discs of sky, riding the middle hill.
    BandMark,
    None,
}

const COUNT: usize = Role::None as usize;

/// 0xAARRGGBB, in the order of [`Role`]. The zeros (`Accent`, `AccentText` and the band's) are worked out from the
/// accent colour by [`palette`].
#[rustfmt::skip]
const LIGHT: [u32; COUNT] = [
    0xFFF3F3F3, 0xFFF9F9F9, 0x0F000000, 0xFFFFFFFF, 0x13000000, 0x12000000, 0xFF1A1A1A, 0xFF5D5D5D, 0xFF8B8B8B,
    0, 0, 0xFFFFFFFF, 0xFFFDFDFD, 0x14000000, 0x2E000000, 0x09000000, 0x0E000000,
    0xFF0F7B0F, 0xFFDFF6DD, 0xFF9D5D00, 0xFFFFF4CE, 0xFFC42B1C, 0xFFFDE7E9, 0xFF6B6B6B, 0x0F000000,
    0xFFE7F3E7, 0xFFD3EAD3, 0xFFFCEBEA, 0xFFF6D5D2, 0xFFF2F5F9, 0xFFF3F3F3, 0xFFB07300, 0xFFFFFFFF,
    0, 0, 0, 0, 0,
];

#[rustfmt::skip]
const DARK: [u32; COUNT] = [
    0xFF202020, 0xFF272727, 0x0DFFFFFF, 0xFF2D2D2D, 0xFF1B1B1B, 0x0FFFFFFF, 0xFFFFFFFF, 0xFFCFCFCF, 0xFF9A9A9A,
    0, 0, 0xFF000000, 0xFF353535, 0x12FFFFFF, 0x1AFFFFFF, 0x0DFFFFFF, 0x14FFFFFF,
    0xFF6CCB5F, 0xFF393D1B, 0xFFFCE100, 0xFF433519, 0xFFFF99A4, 0xFF442726, 0xFFA8A8A8, 0x14FFFFFF,
    0xFF213226, 0xFF2A4231, 0xFF3B2528, 0xFF4F2E33, 0xFF2A2F36, 0xFF202020, 0xFFFCE100, 0xFF000000,
    0, 0, 0, 0, 0,
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

/// The header band's picture, as 0xRRGGBB: a sky that fades from `sky` at the top to `low` at the bottom, behind five
/// hills, the farthest first.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Print {
    pub sky: u32,
    pub low: u32,
    pub hills: [u32; 5],
}

struct Palette {
    roles: [u32; COUNT],
    print: Print,
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
pub fn print() -> Option<Print> {
    derived(|p| p.print)
}

/// Puts a look in place of what Windows says (none detects it again on next use) and returns the one it replaced.
#[cfg(test)]
fn set(look: Option<Look>) -> Option<Look> {
    STATE.with(|s| s.replace(look.map(|look| State { look, palette: None })).map(|s| s.look))
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
/// Contrast for words, a little above WCAG's 4.5 so that the anti-aliased edges between two colours stay readable.
const READ: f64 = 4.6;
/// Contrast for the type icons: thin strokes at 12 points.
const MARK: f64 = 7.0;
/// How far apart (red, green and blue differences added up) neighbouring hills, the sky and the page, and the band's
/// bottom rule and the page must be.
const HILLS_APART: u32 = 24;
const SKY_APART: u32 = 32;
const EDGE_APART: u32 = 30;

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

/// The red, green and blue differences added up.
fn apart(a: u32, b: u32) -> u32 {
    [16, 8, 0].into_iter().map(|shift| ((a >> shift) & 0xFF).abs_diff((b >> shift) & 0xFF)).sum()
}

/// The first colour from `from` toward `to`, in twentieths, that is `ok`; `to` when none is.
fn toward(from: u32, to: u32, ok: impl Fn(u32) -> bool) -> u32 {
    (0..=20).map(|k| mix(from, to, f64::from(k) / 20.0)).find(|c| ok(*c)).unwrap_or(to)
}

/// The first colour along a path of colours, in twentieths of each step, that is `ok`.
fn first(path: &[u32], ok: impl Fn(u32) -> bool) -> Option<u32> {
    let steps = path.windows(2).flat_map(|w| (0..20).map(move |k| mix(w[0], w[1], f64::from(k) / 20.0)));
    steps.chain(path.last().copied()).find(|c| ok(*c))
}

/// Works out every colour that depends on the accent from its seven shades. Each starts from the shade that suits it
/// and moves, a step at a time, only as far as it must to stay readable and distinct: buttons and links on the
/// pages, the status dots and the type icons on the sky, words on every part of the band, and each hill from the one
/// behind it. A pale or a very dark accent can make a hill turn the other way (lighter where the others get darker) to
/// stay apart.
fn palette(Look { mode, ramp }: Look) -> Palette {
    let light = mode != Mode::Dark;
    let table = if light { &LIGHT } else { &DARK };
    let rgb = |role: Role| table[role as usize] & WHITE;
    let [l3, l2, l1, base, d1, d2, d3] = ramp;
    let pages = [Role::Bg, Role::Layer, Role::Card].map(rgb);
    // Every surface a button or link sits on: the pages, and the pages under a hovered or selected row.
    let mut surfaces = pages.to_vec();
    for tint in [Role::Hover, Role::Sel].map(|role| table[role as usize]) {
        surfaces.extend(pages.map(|page| mix(page, tint & WHITE, f64::from(tint >> 24) / 255.0)));
    }
    let (text, on, end) = (rgb(Role::Text), rgb(Role::OnAccent), if light { BLACK } else { WHITE });
    let dots = [Role::Ok, Role::DotWarn, Role::Err].map(rgb);
    let readable = |c| ratio(text, c) >= READ;
    let plain = |c| readable(c) && dots.iter().all(|dot| ratio(*dot, c) >= 3.0);
    let accent = toward(if light { d1 } else { l2 }, end, |c| ratio(c, on) >= READ && surfaces.iter().all(|s| ratio(c, *s) >= 3.0));
    let accent_text = toward(if light { d1 } else { l3 }, end, |c| surfaces.iter().all(|s| ratio(c, *s) >= READ));

    // The sky must also leave the icons room to reach their contrast, at worst in pure black or white.
    let roomy = |c| plain(c) && ratio(c, end) >= MARK && pages.iter().all(|page| apart(c, *page) >= SKY_APART);
    let sky = if light {
        let path = [mix(WHITE, l3, 0.28), l3, l2, l1, base, d1];
        first(&path, roomy).unwrap_or(path[0])
    } else {
        first(&[mix(pages[0], d3, 0.55), d3, BLACK], roomy).unwrap_or(BLACK)
    };
    let low = toward(if light { mix(sky, l3, 0.45) } else { mix(pages[0], d3, 0.8) }, sky, plain);
    let (mut hills, title, caption, edge, mark) = if light {
        ([l3, l2, mix(l2, l1, 0.5), l1, mix(l1, base, 0.5)], mix(d3, BLACK, 0.25), mix(d3, sky, 0.1), base, d2)
    } else {
        // In dark mode each hill is the one behind it moved toward the next shade down, so the far hills glow and the
        // near ones sink into the darkest shade.
        let mut under = mix(sky, low, 0.5);
        let hills = [(l1, 0.3), (base, 0.5), (d1, 0.75), (d2, 0.9), (d3, 1.0)].map(|(shade, t)| {
            under = mix(under, shade, t);
            under
        });
        (hills, WHITE, mix(l3, WHITE, 0.4), mix(d3, BLACK, 0.4), l2)
    };
    // The farthest hill meets the sky at every height, so it stands apart from all of the sky's fade; each other hill
    // stands apart from the one behind it.
    let mut behind: Vec<u32> = (0..=16).map(|i| mix(sky, low, f64::from(i) / 16.0)).collect();
    for (k, hill) in hills.iter_mut().enumerate() {
        let h = *hill;
        // Light hills darken toward the front; in dark mode the far ones lighten first, then the near ones darken.
        let up = !light && k <= 2;
        let lighter: Vec<u32> = std::iter::once(h).chain(ramp.iter().rev().copied().filter(|c| lum(*c) > lum(h))).chain([WHITE]).collect();
        let darker: Vec<u32> = std::iter::once(h).chain(ramp.iter().copied().filter(|c| lum(*c) < lum(h))).chain([BLACK]).collect();
        let paths = if up { [lighter, darker] } else { [darker, lighter] };
        let fits = |c: u32, strict: bool| {
            readable(c) && behind.iter().all(|b| apart(c, *b) >= HILLS_APART && (!strict || if up { lum(c) >= lum(*b) } else { lum(c) <= lum(*b) }))
        };
        *hill = [true, false]
            .into_iter()
            .find_map(|strict| paths.iter().find_map(|path| first(path, |c| fits(c, strict))))
            .unwrap_or_else(|| toward(h, sky, readable));
        behind = vec![*hill];
    }
    let band: Vec<u32> = [sky, low].into_iter().chain(hills).collect();
    let on_band = |c: u32| band.iter().all(|b| ratio(c, *b) >= READ);
    let mut roles = *table;
    for (role, c) in [
        (Role::Accent, accent),
        (Role::AccentText, accent_text),
        (Role::Band, sky),
        (Role::BandText, toward(title, end, on_band)),
        (Role::BandCaption, toward(caption, end, on_band)),
        (Role::BandEdge, toward(edge, BLACK, |c| pages.iter().all(|page| apart(c, *page) >= EDGE_APART))),
        (Role::BandMark, toward(mark, end, |c| ratio(c, sky) >= MARK)),
    ] {
        roles[role as usize] = 0xFF00_0000 | c;
    }
    Palette { roles, print: Print { sky, low, hills } }
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
        use control::Kind;
        assert_eq!(Kind::BandAccent.colors(false), Kind::Accent.colors(false), "same disabled state");
        let (fill, _, ink) = Kind::BandAccent.colors(true);
        assert_eq!((CONTRAST[fill as usize], CONTRAST[ink as usize]), (HIGHLIGHT, HIGHLIGHT_TEXT));
        each_look(|look| {
            assert_eq!(color(fill) >> 24, 255, "the band must not show through the button");
            for tint in [Role::None, Role::Hover, Role::Sel] {
                assert!(contrast_of(color(ink), layered(&[fill, tint])) >= 4.5, "{look:?}: {tint:?} button");
                for page in [Role::Bg, Role::Layer, Role::Card] {
                    let under = layered(&[page, tint]);
                    assert!(contrast_of(color(Role::AccentText), under) >= 4.5, "{look:?}: a link on {page:?} under {tint:?}");
                    assert!(contrast_of(color(Role::Accent), under) >= 3.0, "{look:?}: a button or switch on {page:?} under {tint:?}");
                }
            }
        });
    }

    #[test]
    fn the_band_is_hills_under_a_sky_in_the_accent_with_readable_words_and_a_plain_system_alternative() {
        let band_at = |w| [0, 0, w, page::HERO_H];
        let rgb = |role| color(role) & 0xFFFFFF;
        let saved = set(Some(Look { mode: Mode::Contrast, ramp: BLUE }));
        assert_eq!(print(), None);
        let mut pixels = render_band(640, 96, band_at(640));
        let rule = pixels.split_off(pixels.len() - 640);
        assert!(rule.iter().all(|p| *p == rgb(Role::Text)) && pixels.iter().all(|p| *p == rgb(Role::Bg)), "high contrast keeps the band plain");
        set(saved);
        let mut prints = Vec::new();
        each_look(|look| {
            let Print { sky, low, hills } = print().expect("a picture outside high contrast");
            prints.push((look.mode == Mode::Dark, sky, low, hills));
            assert_eq!(rgb(Role::Band), sky, "{look:?}: the icons' discs are sky");
            let w = 860;
            let pixels = render_band(w, 96, band_at(w));
            assert!(distance(pixels[0], sky) <= 3, "{look:?}: sky at the top");
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
            // Toward the left the hills sink, and the sky above them fades toward its low colour.
            let fade = pixels[(56 * w + 10) as usize];
            assert!(low == sky || distance(fade, low) < distance(fade, sky), "{look:?}: #{fade:06x} low in the sky is nearer #{sky:06x} than #{low:06x}");
            let layers: Vec<u32> = std::iter::once(sky).chain(hills).collect();
            for pair in layers.windows(2) {
                assert!(distance(pair[0], pair[1]) >= 24, "{look:?}: #{:06x} and #{:06x} must stand apart", pair[0], pair[1]);
            }
            // The farthest hill meets the sky at every height, so it stands apart from all of the sky's fade.
            for y in 0..60 {
                let shade = pixels[(y * w + 10) as usize];
                assert!(distance(shade, hills[0]) >= 24, "{look:?}: the farthest hill #{:06x} and the sky's #{shade:06x} at {y}", hills[0]);
            }
            for page in [Role::Bg, Role::Layer] {
                assert!(distance(sky, rgb(page)) >= 30, "{look:?}: the sky stands apart from {page:?}");
                assert!(distance(rgb(Role::BandEdge), rgb(page)) >= 30, "{look:?}: the rule stands apart from {page:?}");
            }
            for dot in [Tone::Ok, Tone::Warn, Tone::Err].map(Tone::dot) {
                for under in [sky, low, rgb(Role::DotGlyph)] {
                    assert!(contrast_of(rgb(dot), under) >= 3.0, "{look:?}: {dot:?} on #{under:06x}");
                }
            }
            for w in [640, 860] {
                let mut pixels = render_band(w, 96, band_at(w));
                pixels.truncate(pixels.len() - w as usize);
                pixels.sort_unstable();
                pixels.dedup();
                for bg in pixels {
                    for ink in [Role::Text, Role::BandText, Role::BandCaption] {
                        assert!(contrast_of(rgb(ink), bg) >= 4.5, "{look:?}: {ink:?} on #{bg:06x} at {w} DIPs");
                    }
                }
            }
        });
        let looks = prints.len();
        prints.sort_unstable();
        prints.dedup();
        assert_eq!(prints.len(), looks, "every accent paints its own picture");
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
