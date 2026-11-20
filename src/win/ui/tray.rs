//! The notification-area icon: the logo in one look per [`Health`], drawn at run time for the taskbar's scale, and the
//! tooltip.

use std::cell::RefCell;
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Graphics::Gdi::{BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS, DeleteObject};
use windows_sys::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow, GetSystemMetricsForDpi};
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, DestroyIcon, FindWindowW, HICON, ICONINFO, SM_CXSMICON, WM_APP};

use crate::board::Health;
use crate::logo::{self, Look};
use crate::win::wide;

/// Sent to the main window when the user does something with the icon.
pub const WM_TRAY: u32 = WM_APP + 1;
const ICON_ID: u32 = 1;

struct Tray {
    hwnd: HWND,
    size: i32,
    /// Made the first time each one is shown, indexed by `Health as usize`.
    icons: [HICON; 6],
    /// What the icon shows now, if it is in the notification area.
    shown: Option<(Health, String)>,
    /// What it should show.
    want: Option<(Health, String)>,
}

impl Tray {
    fn icon(&mut self, health: Health) -> HICON {
        let slot = &mut self.icons[health as usize];
        if slot.is_null() {
            *slot = icon(look(health), self.size);
        }
        *slot
    }
}

thread_local! {
    static TRAY: RefCell<Option<Tray>> = const { RefCell::new(None) };
}

/// Prepares the icon for `hwnd`. Nothing is shown until [`show`].
pub fn init(hwnd: HWND) {
    let size = taskbar_size();
    TRAY.with(|t| *t.borrow_mut() = Some(Tray { hwnd, size, icons: [null_mut(); 6], shown: None, want: None }));
}

/// The size of a notification-area icon at the taskbar's DPI. The app's own system DPI is fixed when it starts, so it
/// would miss a change of scale made while it runs.
fn taskbar_size() -> i32 {
    unsafe {
        let taskbar = FindWindowW(wide("Shell_TrayWnd").as_ptr(), null());
        let dpi = if taskbar.is_null() { 0 } else { GetDpiForWindow(taskbar) };
        let dpi = if dpi == 0 { GetDpiForSystem() } else { dpi };
        GetSystemMetricsForDpi(SM_CXSMICON, dpi).max(16)
    }
}

/// The display or its scale may have changed: draws the icon again if the taskbar now wants another size.
pub fn rescale() {
    let size = taskbar_size();
    let old = TRAY.with(|t| {
        let mut t = t.try_borrow_mut().ok()?;
        let t = t.as_mut()?;
        if t.size == size {
            return None;
        }
        t.size = size;
        // So that `show` updates it: adding it fails while it is there, and then it is modified.
        t.shown = None;
        Some((std::mem::replace(&mut t.icons, [null_mut(); 6]), t.want.clone()))
    });
    let Some((icons, want)) = old else { return };
    if let Some((health, tip)) = want {
        show(health, &tip);
    }
    // Only after the shell has the new icon: it may still be drawing the old one.
    for icon in icons.into_iter().filter(|i| !i.is_null()) {
        unsafe { DestroyIcon(icon) };
    }
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

/// How the logo shows each state: the colour says how things are, and a hollow logo says the app is busy or has nothing
/// to do.
fn look(health: Health) -> Look {
    match health {
        Health::Ok => Look::Solid(logo::GREEN),
        Health::Working | Health::Waiting => Look::Hollow(logo::BLUE),
        Health::NotConfigured => Look::Hollow(logo::GREY),
        Health::Attention => Look::Solid(logo::AMBER),
        Health::Error => Look::Solid(logo::RED),
    }
}

/// The logo in the requested look, drawn at `size` pixels square. The caller destroys it.
pub fn icon(look: Look, size: i32) -> HICON {
    let size = size.max(1);
    let drawn = logo::draw(look, size as u32);
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
        // Icons take straight (not premultiplied) alpha, which is what the logo is drawn in.
        std::slice::from_raw_parts_mut(bits.cast::<u32>(), drawn.len()).copy_from_slice(&drawn);
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
    fn each_state_the_readme_names_looks_different() {
        assert_eq!(look(Health::Ok), Look::Solid(logo::GREEN));
        assert_eq!(look(Health::Working), look(Health::Waiting), "both are 'busy, nothing wrong'");
        assert_eq!(look(Health::Working), Look::Hollow(logo::BLUE));
        let states = [Health::NotConfigured, Health::Ok, Health::Working, Health::Attention, Health::Error];
        for (i, a) in states.iter().enumerate() {
            for b in &states[i + 1..] {
                assert_ne!(look(*a), look(*b), "{a:?} and {b:?}");
            }
        }
    }
}
