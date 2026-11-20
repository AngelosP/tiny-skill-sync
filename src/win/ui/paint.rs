//! Drawing: everything a window shows is drawn into one off-screen bitmap, then copied to the screen at once, so
//! nothing flickers. Rounded boxes are anti-aliased by hand (GDI cannot), text is drawn by GDI with ClearType.
//!
//! Positions are in DIPs (1/96 inch) and become pixels at the canvas DPI. An edge, not a size, is scaled, so boxes
//! that touch in DIPs touch in pixels. The canvas origin says which pixel of the window the bitmap's top-left is, so a
//! control can ask its parent to draw what is under it into the same bitmap (see `control.rs`).

use std::cell::RefCell;
use std::mem::{size_of, zeroed};
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::RECT;
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleDC, CreateDIBSection, CreateFontIndirectW, DIB_RGB_COLORS, DT_CALCRECT, DT_CENTER,
    DT_EDITCONTROL, DT_END_ELLIPSIS, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DT_WORDBREAK, DeleteDC, DeleteObject, DrawTextW, GdiFlush, GetTextFaceW, HBITMAP,
    HDC, HFONT, HGDIOBJ, LOGFONTW, SRCCOPY, SelectObject, SetBkMode, SetTextColor,
};

use super::scale;
use super::theme::{Role, color, colorref};

/// x, y, width, height in DIPs.
pub type Rect = [i32; 4];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Font {
    /// 14, the text of controls and lists.
    Body,
    /// 14 semibold.
    Strong,
    /// 20 semibold, page titles.
    Title,
    /// 12, secondary lines.
    Caption,
    /// 12 semibold, pills.
    CaptionStrong,
    /// 12.5 monospaced, file contents and paths.
    Mono,
    /// A bold "!" in a warning dot.
    Bang,
    /// Segoe Fluent Icons at a size.
    Icon(u8),
}

/// The colour of a status: a foreground, a tinted background and the colour of a filled dot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tone {
    Ok,
    Warn,
    Err,
    Off,
}

impl Tone {
    pub fn ink(self) -> Role {
        match self {
            Tone::Ok => Role::Ok,
            Tone::Warn => Role::Warn,
            Tone::Err => Role::Err,
            Tone::Off => Role::Text2,
        }
    }

    pub fn bg(self) -> Role {
        match self {
            Tone::Ok => Role::OkBg,
            Tone::Warn => Role::WarnBg,
            Tone::Err => Role::ErrBg,
            Tone::Off => Role::OffBg,
        }
    }

    pub fn dot(self) -> Role {
        match self {
            Tone::Ok => Role::Ok,
            Tone::Warn => Role::DotWarn,
            Tone::Err => Role::Err,
            Tone::Off => Role::Off,
        }
    }
}

pub mod glyph {
    pub const OK: char = '\u{E73E}';
    pub const WARN: char = '\u{E7BA}';
    pub const ERR: char = '\u{E711}';
    pub const PAUSE: char = '\u{E769}';
    pub const SYNC: char = '\u{E895}';
    pub const REFRESH: char = '\u{E72C}';
    pub const ADD: char = '\u{E710}';
    pub const HISTORY: char = '\u{E81C}';
    pub const SETTINGS: char = '\u{E713}';
    pub const LOCK: char = '\u{E72E}';
    pub const MORE: char = '\u{E712}';
    pub const TIMER: char = '\u{E916}';
    pub const CHEVRON_DOWN: char = '\u{E70D}';
    pub const CHEVRON_RIGHT: char = '\u{E76C}';
    pub const FILE: char = '\u{E8A5}';
    pub const EDIT: char = '\u{E70F}';
    pub const DELETE: char = '\u{E74D}';
    pub const UNDO: char = '\u{E7A7}';
    pub const OPEN: char = '\u{E8A7}';
    pub const FOLDER: char = '\u{E8B7}';
    pub const KEY: char = '\u{E8D7}';
    pub const POWER: char = '\u{E7E8}';
    pub const INFO: char = '\u{E946}';
    pub const ERROR_BADGE: char = '\u{EA39}';
    pub const BACK: char = '\u{E72B}';
    pub const REMOVE: char = '\u{E738}';
    pub const BINARY: char = '\u{EB9F}';
    pub const VIEW: char = '\u{E890}';
    pub const COPY: char = '\u{E8C8}';
    pub const LINK: char = '\u{E71B}';
}

struct Canvas {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u32,
    /// The allocated size, in pixels.
    cap: (i32, i32),
    /// The size in use.
    w: i32,
    h: i32,
    /// The window pixel the bitmap's top-left shows.
    ox: i32,
    oy: i32,
    dpi: u32,
}

thread_local! {
    static CANVAS: RefCell<Option<Canvas>> = const { RefCell::new(None) };
    static FONTS: RefCell<Vec<(Font, u32, HFONT)>> = const { RefCell::new(Vec::new()) };
    /// A DC for measuring text when nothing is being drawn.
    static MEASURE_DC: RefCell<HDC> = const { RefCell::new(null_mut()) };
}

/// The font for a face at a DPI: (face, fallback, size in tenths of a DIP, weight). A semibold face is chosen by name
/// with the normal weight, so GDI does not make it bolder still.
fn spec(font: Font) -> (&'static str, &'static str, u32, i32) {
    match font {
        Font::Body => ("Segoe UI Variable Text", "Segoe UI", 140, 400),
        Font::Strong => ("Segoe UI Variable Text Semibold", "Segoe UI Semibold", 140, 400),
        // GDI knows the face by its first 31 letters.
        Font::Title => ("Segoe UI Variable Display Semib", "Segoe UI Semibold", 200, 400),
        Font::Caption => ("Segoe UI Variable Text", "Segoe UI", 120, 400),
        Font::CaptionStrong => ("Segoe UI Variable Text Semibold", "Segoe UI Semibold", 120, 400),
        Font::Mono => ("Cascadia Mono", "Consolas", 125, 400),
        Font::Bang => ("Segoe UI", "Segoe UI", 110, 700),
        Font::Icon(size) => ("Segoe Fluent Icons", "Segoe MDL2 Assets", u32::from(size) * 10, 400),
    }
}

fn create_font(face: &str, tenths: u32, weight: i32, dpi: u32) -> HFONT {
    let mut lf: LOGFONTW = unsafe { zeroed() };
    lf.lfHeight = -(((tenths * dpi + 480) / 960) as i32);
    lf.lfWeight = weight;
    lf.lfCharSet = 1;
    lf.lfQuality = 5;
    for (dst, src) in lf.lfFaceName.iter_mut().zip(face.encode_utf16().take(31)) {
        *dst = src;
    }
    unsafe { CreateFontIndirectW(&lf) }
}

/// The face GDI really chose for a font, so a missing one can be replaced.
fn face_of(dc: HDC, font: HFONT) -> String {
    let mut buf = [0u16; 32];
    unsafe {
        let old = SelectObject(dc, font);
        let got = GetTextFaceW(dc, buf.len() as i32, buf.as_mut_ptr());
        SelectObject(dc, old);
        String::from_utf16_lossy(&buf[..(got.max(1) - 1) as usize])
    }
}

fn measure_dc() -> HDC {
    MEASURE_DC.with(|d| {
        let mut d = d.borrow_mut();
        if d.is_null() {
            *d = unsafe { CreateCompatibleDC(null_mut()) };
        }
        *d
    })
}

pub fn font(font: Font, dpi: u32) -> HFONT {
    if let Some(found) = FONTS.with(|f| f.borrow().iter().find(|(f, d, _)| *f == font && *d == dpi).map(|(_, _, h)| *h)) {
        return found;
    }
    let (face, fallback, tenths, weight) = spec(font);
    let mut made = create_font(face, tenths, weight, dpi);
    if face != fallback && !face_of(measure_dc(), made).eq_ignore_ascii_case(face) {
        unsafe { DeleteObject(made) };
        made = create_font(fallback, tenths, weight, dpi);
    }
    FONTS.with(|f| f.borrow_mut().push((font, dpi, made)));
    made
}

/// Frees the bitmap and the fonts. They are made again when something is drawn.
pub fn release() {
    if let Some(c) = CANVAS.with(|c| c.borrow_mut().take()) {
        unsafe {
            SelectObject(c.dc, c.old);
            DeleteObject(c.bitmap);
            DeleteDC(c.dc);
        }
    }
    for (_, _, f) in FONTS.with(|f| std::mem::take(&mut *f.borrow_mut())) {
        unsafe { DeleteObject(f) };
    }
    MEASURE_DC.with(|d| {
        let dc = std::mem::replace(&mut *d.borrow_mut(), null_mut());
        if !dc.is_null() {
            unsafe { DeleteDC(dc) };
        }
    });
}

/// Starts drawing `w` by `h` pixels whose top-left is the window pixel (`ox`, `oy`). False when no bitmap could be made.
pub fn begin(dpi: u32, w: i32, h: i32, ox: i32, oy: i32) -> bool {
    CANVAS.with(|cell| {
        let mut cell = cell.borrow_mut();
        let need = (w.max(1), h.max(1));
        let fits = cell.as_ref().is_some_and(|c| c.cap.0 >= need.0 && c.cap.1 >= need.1);
        if !fits {
            let cap = cell.as_ref().map_or((0, 0), |c| c.cap);
            // Rounded up, so growing a window a little at a time does not make a bitmap every time.
            let up = |v: i32| (v + 127) / 128 * 128;
            let cap = (up(need.0.max(cap.0)), up(need.1.max(cap.1)));
            let mut info: BITMAPINFO = unsafe { zeroed() };
            info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
            info.bmiHeader.biWidth = cap.0;
            info.bmiHeader.biHeight = -cap.1;
            info.bmiHeader.biPlanes = 1;
            info.bmiHeader.biBitCount = 32;
            info.bmiHeader.biCompression = BI_RGB;
            let dc = match cell.take() {
                Some(old) => unsafe {
                    SelectObject(old.dc, old.old);
                    DeleteObject(old.bitmap);
                    old.dc
                },
                None => unsafe { CreateCompatibleDC(null_mut()) },
            };
            let mut bits = null_mut();
            let bitmap = unsafe { CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0) };
            if bitmap.is_null() || dc.is_null() {
                unsafe { DeleteDC(dc) };
                return false;
            }
            let old = unsafe { SelectObject(dc, bitmap) };
            unsafe { SetBkMode(dc, 1) };
            *cell = Some(Canvas { dc, bitmap, old, bits: bits.cast(), cap, w: 0, h: 0, ox: 0, oy: 0, dpi });
        }
        let c = cell.as_mut().expect("made above");
        (c.w, c.h, c.ox, c.oy, c.dpi) = (need.0, need.1, ox, oy, dpi);
        true
    })
}

/// Copies what was drawn to `dc` at (`x`, `y`).
pub fn end(dc: HDC, x: i32, y: i32) {
    CANVAS.with(|c| {
        if let Some(c) = c.borrow().as_ref() {
            unsafe { BitBlt(dc, x, y, c.w, c.h, c.dc, 0, 0, SRCCOPY) };
        }
    });
}

pub fn origin() -> (i32, i32) {
    CANVAS.with(|c| c.borrow().as_ref().map_or((0, 0), |c| (c.ox, c.oy)))
}

pub fn set_origin(x: i32, y: i32) {
    CANVAS.with(|c| {
        if let Some(c) = c.borrow_mut().as_mut() {
            (c.ox, c.oy) = (x, y);
        }
    });
}

/// Does any of the DIP rect fall on the canvas? Lets a long list skip what is out of view.
pub fn visible(at: Rect) -> bool {
    CANVAS.with(|c| {
        c.borrow().as_ref().is_some_and(|c| {
            let (l, t, r, b) = edges(c, at);
            r > 0 && b > 0 && l < c.w && t < c.h
        })
    })
}

/// The DIP rect in bitmap pixels: left, top, right, bottom.
fn edges(c: &Canvas, at: Rect) -> (i32, i32, i32, i32) {
    let s = |v| scale(v, c.dpi);
    (s(at[0]) - c.ox, s(at[1]) - c.oy, s(at[0] + at[2]) - c.ox, s(at[1] + at[3]) - c.oy)
}

fn blend(dst: u32, src: u32, a: u32) -> u32 {
    if a >= 256 {
        return src;
    }
    let mix = |shift: u32| {
        let d = (dst >> shift) & 0xFF;
        let s = (src >> shift) & 0xFF;
        ((d * (256 - a) + s * a) >> 8) << shift
    };
    mix(16) | mix(8) | mix(0)
}

/// Signed distance from a pixel centre to the edge of a box with rounded corners; negative inside.
fn distance(x: f32, y: f32, hx: f32, hy: f32, r: f32) -> f32 {
    let qx = x.abs() - (hx - r);
    let qy = y.abs() - (hy - r);
    let ox = qx.max(0.0);
    let oy = qy.max(0.0);
    (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - r
}

/// A box with rounded corners: `fill` covers all of it, `border` (one device pixel per 96 DPI) is drawn on top along
/// its edge. Colours are 0xAARRGGBB; an alpha of 0 draws nothing.
pub fn fill_argb(at: Rect, radius: i32, fill: u32, border: u32) {
    CANVAS.with(|cell| {
        let cell = cell.borrow();
        let Some(c) = cell.as_ref() else { return };
        let (l, t, r, b) = edges(c, at);
        let (x0, x1, y0, y1) = (l.max(0), r.min(c.w), t.max(0), b.min(c.h));
        if x0 >= x1 || y0 >= y1 || (fill >> 24 == 0 && border >> 24 == 0) {
            return;
        }
        unsafe { GdiFlush() };
        let bw = (c.dpi / 96).max(1) as f32;
        let rad = (radius as f32 * c.dpi as f32 / 96.0).min((r - l) as f32 / 2.0).min((b - t) as f32 / 2.0).max(0.0);
        let (fa, ba) = (((fill >> 24) + 1) * (fill >> 24 != 0) as u32, ((border >> 24) + 1) * (border >> 24 != 0) as u32);
        let (hx, hy) = ((r - l) as f32 / 2.0, (b - t) as f32 / 2.0);
        let (cx, cy) = (l as f32 + hx, t as f32 + hy);
        // Pixels closer to an edge than this need the distance; the rest are plain fill.
        let edge = (rad.max(bw) + 1.0).ceil() as i32;
        let stride = c.cap.0 as usize;
        for y in y0..y1 {
            let row = unsafe { std::slice::from_raw_parts_mut(c.bits.add(y as usize * stride), stride) };
            let band = y < t + edge || y >= b - edge;
            let py = y as f32 + 0.5 - cy;
            let mut shade = |x: i32| {
                let d = distance(x as f32 + 0.5 - cx, py, hx, hy, rad);
                let outer = (0.5 - d).clamp(0.0, 1.0);
                if outer <= 0.0 {
                    return;
                }
                let px = &mut row[x as usize];
                *px = blend(*px, fill, (fa as f32 * outer) as u32);
                if ba > 0 {
                    let inner = (0.5 - d - bw).clamp(0.0, 1.0);
                    *px = blend(*px, border, (ba as f32 * (outer - inner)) as u32);
                }
            };
            if band {
                for x in x0..x1 {
                    shade(x);
                }
                continue;
            }
            let (a, z) = ((l + edge).clamp(x0, x1), (r - edge).clamp(x0, x1));
            for x in x0..a {
                shade(x);
            }
            for x in z.max(a)..x1 {
                shade(x);
            }
            if fa > 0 {
                for px in &mut row[a as usize..z.max(a) as usize] {
                    *px = blend(*px, fill, fa);
                }
            }
        }
    });
}

pub fn fill(at: Rect, radius: i32, fill: Role, border: Role) {
    fill_argb(at, radius, color(fill), color(border));
}

/// Text in a rect, with `DT_*` flags; `&` is shown as it is.
pub fn text(at: Rect, s: &str, font_: Font, ink: Role, flags: u32) {
    // DrawTextW reads the pointer of an empty buffer, which points nowhere.
    if s.is_empty() {
        return;
    }
    let Some((dc, rc, dpi)) = CANVAS.with(|c| {
        let c = c.borrow();
        let c = c.as_ref()?;
        let (l, t, r, b) = edges(c, at);
        Some((c.dc, RECT { left: l, top: t, right: r, bottom: b }, c.dpi))
    }) else {
        return;
    };
    let mut rc = rc;
    let wide: Vec<u16> = s.encode_utf16().collect();
    unsafe {
        let old = SelectObject(dc, font(font_, dpi));
        SetTextColor(dc, colorref(color(ink)));
        DrawTextW(dc, wide.as_ptr(), wide.len() as i32, &mut rc, flags | DT_NOPREFIX);
        SelectObject(dc, old);
    }
}

/// One line, vertically centred, ending in "…" when it does not fit.
pub fn line(at: Rect, s: &str, font: Font, ink: Role) {
    text(at, s, font, ink, DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS);
}

/// An icon centred in a rect.
pub fn glyph(at: Rect, ch: char, size: u8, ink: Role) {
    let mut buf = [0u8; 4];
    text(at, ch.encode_utf8(&mut buf), Font::Icon(size), ink, DT_SINGLELINE | DT_VCENTER | DT_CENTER);
}

/// The 16-DIP Octicons mark, drawn in the same foreground colour as the neighbouring actions.
pub fn github_mark(at: Rect, ink: Role) {
    const MASK: &[u8; 64 * 64] = include_bytes!("../../../assets/github-mark-64.alpha");
    CANVAS.with(|cell| {
        let cell = cell.borrow();
        let Some(c) = cell.as_ref() else { return };
        let (l, t, r, b) = edges(c, at);
        let size = scale(16, c.dpi).min(r - l).min(b - t);
        if size <= 0 {
            return;
        }
        let left = l + (r - l - size) / 2;
        let top = t + (b - t - size) / 2;
        // Average the mask pixels covered by each device pixel, including at fractional display scales.
        let span = |i| {
            let edge = |v: i32| ((v * 64 + size / 2) / size) as usize;
            let start = edge(i).min(63);
            start..edge(i + 1).max(start + 1)
        };
        let fg = color(ink);
        unsafe { GdiFlush() };
        for y in top.max(0)..(top + size).min(c.h) {
            let ys = span(y - top);
            for x in left.max(0)..(left + size).min(c.w) {
                let xs = span(x - left);
                let n = (xs.len() * ys.len()) as u32;
                let sum: u32 = ys.clone().flat_map(|j| &MASK[j * 64 + xs.start..j * 64 + xs.end]).map(|&a| u32::from(a)).sum();
                let px = unsafe { &mut *c.bits.add((y as usize * c.cap.0 as usize) + x as usize) };
                *px = blend(*px, fg, (sum * 256) / (n * 255));
            }
        }
    });
}

/// The size of text in DIPs, rounded up: one line when `width` is 0, else wrapped at `width` DIPs.
pub fn measure(s: &str, font_: Font, width: i32, dpi: u32) -> (i32, i32) {
    if s.is_empty() {
        return (0, 0);
    }
    let dc = measure_dc();
    let wide: Vec<u16> = s.encode_utf16().collect();
    let mut rc = RECT { left: 0, top: 0, right: scale(width, dpi), bottom: 0 };
    let flags = DT_CALCRECT | DT_NOPREFIX | if width > 0 { DT_WORDBREAK | DT_EDITCONTROL } else { DT_SINGLELINE };
    unsafe {
        let old = SelectObject(dc, font(font_, dpi));
        DrawTextW(dc, wide.as_ptr(), wide.len() as i32, &mut rc, flags);
        SelectObject(dc, old);
    }
    let dips = |px: i32| (px * 96 + dpi as i32 - 1) / dpi as i32;
    (dips(rc.right - rc.left), dips(rc.bottom - rc.top))
}

pub const PILL_H: i32 = 22;

/// How wide a pill is.
pub fn pill_width(text: &str, glyph: Option<char>, dpi: u32) -> i32 {
    measure(text, Font::CaptionStrong, 0, dpi).0 + 18 + glyph.map_or(0, |_| 11 + 5)
}

/// A rounded label: a status or a mark such as "Private". `at` is its top-left; the width is [`pill_width`].
pub fn pill(x: i32, y: i32, text: &str, tone: Tone, glyph: Option<char>, dpi: u32) {
    let w = pill_width(text, glyph, dpi);
    fill([x, y, w, PILL_H], PILL_H / 2, tone.bg(), Role::None);
    let mut left = x + 9;
    if let Some(ch) = glyph {
        self::glyph([left, y, 11, PILL_H], ch, 11, tone.ink());
        left += 16;
    }
    // A plain pill (Off) is quieter: normal weight.
    let font = if tone == Tone::Off { Font::Caption } else { Font::CaptionStrong };
    line([left, y, x + w - 9 - left + 2, PILL_H], text, font, tone.ink());
}

/// A filled circle with a status mark in it, `size` DIPs across.
pub fn dot(at: Rect, tone: Tone) {
    fill(at, at[2] / 2, tone.dot(), Role::None);
    match tone {
        Tone::Warn => text(at, "!", Font::Bang, Role::DotGlyph, DT_SINGLELINE | DT_VCENTER | DT_CENTER),
        Tone::Ok => glyph(at, glyph::OK, 9, Role::DotGlyph),
        Tone::Err => glyph(at, glyph::ERR, 9, Role::DotGlyph),
        Tone::Off => glyph(at, glyph::PAUSE, 9, Role::DotGlyph),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_distance_is_negative_inside_and_grows_outside_a_rounded_corner() {
        assert!(distance(0.0, 0.0, 10.0, 10.0, 4.0) < 0.0);
        assert!((distance(10.0, 0.0, 10.0, 10.0, 4.0)).abs() < 1e-4, "on a straight edge");
        // The corner of the square is outside the rounded box by r·(√2 − 1).
        let corner = distance(10.0, 10.0, 10.0, 10.0, 4.0);
        assert!((corner - 4.0 * (2f32.sqrt() - 1.0)).abs() < 1e-4);
    }

    #[test]
    fn blending_mixes_each_channel_by_coverage() {
        assert_eq!(blend(0x000000, 0xFFFFFF, 256), 0xFFFFFF);
        assert_eq!(blend(0x204060, 0xFFFFFF, 0), 0x204060);
        assert_eq!(blend(0x000000, 0xFF8000, 128), 0x7F4000);
    }

    #[test]
    fn github_mark_is_centred_with_the_cat_cut_out_of_the_foreground() {
        for dpi in [96, 120, 192] {
            let s = |v| scale(v, dpi);
            // Paint against the opposite colour so the silhouette is visible in any theme.
            let fg = color(Role::Text);
            let bg = fg ^ 0xFFFFFF;
            assert!(begin(dpi, s(24), s(24), 0, 0));
            fill_argb([0, 0, 24, 24], 0, bg, 0);
            github_mark([0, 0, 24, 24], Role::Text);
            CANVAS.with(|cell| {
                let c = cell.borrow();
                let c = c.as_ref().unwrap();
                let pixel = |x, y| unsafe { *c.bits.add((s(y) * c.cap.0 + s(x)) as usize) & 0xFFFFFF };
                for (x, y) in [(12, 6), (5, 12), (18, 12)] {
                    assert_eq!(pixel(x, y), fg & 0xFFFFFF, "inside the mark at {dpi} DPI");
                }
                for (x, y) in [(12, 3), (3, 12), (4, 4), (12, 12), (12, 18)] {
                    assert_eq!(pixel(x, y), bg & 0xFFFFFF, "background or cutout at {dpi} DPI");
                }
            });
        }
        release();
    }

    // An empty line of a file in the diff view: DrawTextW read through the dangling pointer of an empty buffer and
    // the app crashed in user32.
    #[test]
    fn empty_text_is_drawn_as_nothing() {
        assert!(begin(96, 64, 32, 0, 0));
        let before = CANVAS.with(|c| c.borrow().as_ref().map(|c| unsafe { *c.bits }));
        line([0, 0, 64, 32], "", Font::Mono, Role::Text);
        text([0, 0, 64, 32], "", Font::Body, Role::Text, DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_CENTER);
        assert_eq!(CANVAS.with(|c| c.borrow().as_ref().map(|c| unsafe { *c.bits })), before);
        release();
    }
}
