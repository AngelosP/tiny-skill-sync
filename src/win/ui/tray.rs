//! The notification-area icon: one picture per [`Health`], drawn at run time, and the tooltip.

use std::cell::RefCell;
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Graphics::Gdi::{BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS, DeleteObject};
use windows_sys::Win32::UI::HiDpi::{GetDpiForSystem, GetSystemMetricsForDpi};
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, DestroyIcon, HICON, ICONINFO, SM_CXSMICON, WM_APP};

use crate::board::Health;

/// Sent to the main window when the user does something with the icon.
pub const WM_TRAY: u32 = WM_APP + 1;
const ICON_ID: u32 = 1;

struct Tray {
    hwnd: HWND,
    size: i32,
    /// Made the first time each one is shown, indexed by `Health as usize`.
    icons: [HICON; 5],
    /// What the icon shows now, if it is in the notification area.
    shown: Option<(Health, String)>,
    /// What it should show.
    want: Option<(Health, String)>,
}

impl Tray {
    fn icon(&mut self, health: Health) -> HICON {
        let slot = &mut self.icons[health as usize];
        if slot.is_null() {
            *slot = icon(health, self.size);
        }
        *slot
    }
}

thread_local! {
    static TRAY: RefCell<Option<Tray>> = const { RefCell::new(None) };
}

/// Prepares the icon for `hwnd`. Nothing is shown until [`show`].
pub fn init(hwnd: HWND) {
    let size = unsafe { GetSystemMetricsForDpi(SM_CXSMICON, GetDpiForSystem()) }.max(16);
    TRAY.with(|t| *t.borrow_mut() = Some(Tray { hwnd, size, icons: [null_mut(); 5], shown: None, want: None }));
}

fn data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut nid: NOTIFYICONDATAW = unsafe { zeroed() };
    nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = ICON_ID;
    nid
}

fn notify(op: u32, hwnd: HWND, icon: HICON, tip: &str) -> bool {
    let mut nid = data(hwnd);
    nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
    nid.uCallbackMessage = WM_TRAY;
    nid.hIcon = icon;
    for (dst, src) in nid.szTip.iter_mut().take(127).zip(tip.encode_utf16()) {
        *dst = src;
    }
    unsafe {
        if Shell_NotifyIconW(op, &nid) == 0 {
            return false;
        }
        if op == NIM_ADD {
            nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            Shell_NotifyIconW(NIM_SETVERSION, &nid);
        }
    }
    true
}

/// Shows `health` and `tip`, adding the icon if it is not there. Does nothing when it already shows them.
pub fn show(health: Health, tip: &str) {
    let plan = TRAY.with(|t| {
        let mut t = t.try_borrow_mut().ok()?;
        let t = t.as_mut()?;
        let want = Some((health, tip.to_string()));
        if t.shown == want {
            return None;
        }
        t.want = want;
        Some((t.hwnd, t.icon(health), t.shown.is_some()))
    });
    let Some((hwnd, icon, added)) = plan else { return };
    // The shell is another process; no borrow is held while it is called. If the icon vanished (Explorer
    // restarted), modifying fails and it is added again; if adding fails because it is still there, it is modified.
    let (first, second) = if added { (NIM_MODIFY, NIM_ADD) } else { (NIM_ADD, NIM_MODIFY) };
    let ok = notify(first, hwnd, icon, tip) || notify(second, hwnd, icon, tip);
    TRAY.with(|t| {
        if let Ok(mut t) = t.try_borrow_mut() {
            if let Some(t) = t.as_mut() {
                t.shown = if ok { Some((health, tip.to_string())) } else { None };
            }
        }
    });
}

/// Explorer started again (it broadcasts `TaskbarCreated`): the icon is gone and must be added again.
pub fn readd() {
    let want = TRAY.with(|t| {
        let mut t = t.try_borrow_mut().ok()?;
        let t = t.as_mut()?;
        t.shown = None;
        t.want.clone()
    });
    if let Some((health, tip)) = want {
        show(health, &tip);
    }
}

/// Takes the icon out of the notification area for good.
pub fn remove() {
    let Some(tray) = TRAY.with(|t| t.try_borrow_mut().ok().and_then(|mut t| t.take())) else { return };
    unsafe {
        if tray.shown.is_some() {
            Shell_NotifyIconW(NIM_DELETE, &data(tray.hwnd));
        }
        for icon in tray.icons {
            if !icon.is_null() {
                DestroyIcon(icon);
            }
        }
    }
}

const BLUE: u32 = 0x2D7DD2;
const GREY: u32 = 0x8A8A8A;
const AMBER: u32 = 0xD98C00;
const RED: u32 = 0xD13438;
const WHITE: u32 = 0xFFFFFF;

/// Distance from `(x, y)` to the segment `a`–`b`, minus `half_width`: negative inside the stroke.
fn segment(x: f32, y: f32, a: (f32, f32), b: (f32, f32), half_width: f32) -> f32 {
    let (px, py, bx, by) = (x - a.0, y - a.1, b.0 - a.0, b.1 - a.1);
    let h = ((px * bx + py * by) / (bx * bx + by * by)).clamp(0.0, 1.0);
    (px - bx * h).hypot(py - by * h) - half_width
}

/// How much of a pixel a shape covers, from its signed distance in pixels.
fn coverage(distance: f32) -> f32 {
    (0.5 - distance).clamp(0.0, 1.0)
}

fn mix(a: u32, b: u32, t: f32) -> u32 {
    let channel = |shift: u32| {
        let (ca, cb) = (((a >> shift) & 0xFF) as f32, ((b >> shift) & 0xFF) as f32);
        ((ca + (cb - ca) * t).round() as u32) << shift
    };
    channel(16) | channel(8) | channel(0)
}

/// The colour and coverage of one pixel, `(x, y)` measured from the centre, in an icon of radius `r`.
fn pixel(health: Health, x: f32, y: f32, r: f32) -> (u32, f32) {
    let disc = x.hypot(y) - r;
    let ring = |colour| {
        let thickness = (0.32 * r).max(1.6);
        (colour, coverage((disc + thickness / 2.0).abs() - thickness / 2.0))
    };
    match health {
        Health::Ok => (BLUE, coverage(disc)),
        Health::Working => ring(BLUE),
        Health::NotConfigured => ring(GREY),
        Health::Attention => {
            let bar = segment(x, y, (0.0, -0.55 * r), (0.0, 0.1 * r), 0.14 * r);
            let dot = x.hypot(y - 0.5 * r) - 0.15 * r;
            (mix(AMBER, WHITE, coverage(bar.min(dot))), coverage(disc))
        }
        Health::Error => {
            let d = 0.38 * r;
            let cross = segment(x, y, (-d, -d), (d, d), 0.13 * r).min(segment(x, y, (-d, d), (d, -d), 0.13 * r));
            (mix(RED, WHITE, coverage(cross)), coverage(disc))
        }
    }
}

/// A round, anti-aliased icon `size` pixels square. The caller destroys it.
pub fn icon(health: Health, size: i32) -> HICON {
    unsafe {
        let mut bmi: BITMAPINFO = zeroed();
        bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = size;
        bmi.bmiHeader.biHeight = -size;
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB;
        let mut bits: *mut c_void = null_mut();
        let color = CreateDIBSection(null_mut(), &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        if color.is_null() || bits.is_null() {
            return null_mut();
        }
        let pixels = std::slice::from_raw_parts_mut(bits.cast::<u32>(), (size * size) as usize);
        let half = size as f32 / 2.0;
        let r = half - 0.5;
        for y in 0..size {
            for x in 0..size {
                let (colour, cover) = pixel(health, x as f32 + 0.5 - half, y as f32 + 0.5 - half, r);
                // Icons take straight (not premultiplied) alpha.
                let alpha = (cover * 255.0).round() as u32;
                pixels[(y * size + x) as usize] = if alpha == 0 { 0 } else { (alpha << 24) | colour };
            }
        }
        // All zero: with a 32-bit colour bitmap the alpha channel decides, not the mask.
        let mask_bits = vec![0u8; ((size + 15) / 16 * 2 * size) as usize];
        let mask = CreateBitmap(size, size, 1, 1, mask_bits.as_ptr().cast());
        let info = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&info);
        DeleteObject(color);
        DeleteObject(mask);
        icon
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyphs_are_white_inside_the_disc_and_the_ring_is_hollow() {
        let r = 15.5;
        assert_eq!(pixel(Health::Attention, 0.0, -0.3 * r, r), (WHITE, 1.0));
        assert_eq!(pixel(Health::Attention, 0.5 * r, 0.0, r), (AMBER, 1.0));
        assert_eq!(pixel(Health::Error, 0.0, 0.0, r), (WHITE, 1.0));
        assert_eq!(pixel(Health::Error, 0.0, 0.6 * r, r).0, RED);
        assert_eq!(pixel(Health::Working, 0.0, 0.0, r).1, 0.0);
        assert_eq!(pixel(Health::Working, 0.0, r - 1.0, r).1, 1.0);
        assert_eq!(pixel(Health::Ok, 0.0, r + 1.0, r).1, 0.0);
    }
}
