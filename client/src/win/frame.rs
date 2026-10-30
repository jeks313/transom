//! Native Windows chrome is outside the exact-pixel streamed client viewport.
#[cfg(test)]
use crate::wire::Size;
use windows::Win32::{
    Foundation::RECT,
    UI::{
        HiDpi::AdjustWindowRectExForDpi,
        Input::KeyboardAndMouse::{GetAsyncKeyState, VK_MENU},
        WindowsAndMessaging::*,
    },
};
/// The chrome around `hwnd`'s client area as it is now (caption, frame and a
/// menu bar, wrapped or not), or the bare frame at `dpi` when it cannot be
/// measured (minimized).
pub fn inset(hwnd: windows::Win32::Foundation::HWND, dpi: u32) -> (i32, i32) {
    let mut window = RECT::default();
    let mut client = RECT::default();
    unsafe {
        if !IsIconic(hwnd).as_bool()
            && GetWindowRect(hwnd, &mut window).is_ok()
            && GetClientRect(hwnd, &mut client).is_ok()
            && client.right > 0
            && client.bottom > 0
        {
            return (
                (window.right - window.left) - client.right,
                (window.bottom - window.top) - client.bottom,
            );
        }
    }
    outer_size(0, 0, dpi)
}
pub fn hit_test(native: u32) -> u32 {
    if native == HTCLIENT && unsafe { GetAsyncKeyState(VK_MENU.0 as i32) } < 0 {
        HTCAPTION
    } else {
        native
    }
}
pub fn outer_size(w: u32, h: u32, dpi: u32) -> (i32, i32) {
    let mut r = RECT {
        left: 0,
        top: 0,
        right: w as i32,
        bottom: h as i32,
    };
    unsafe {
        let _ =
            AdjustWindowRectExForDpi(&mut r, WS_OVERLAPPEDWINDOW, false, Default::default(), dpi);
    }
    (r.right - r.left, r.bottom - r.top)
}
#[cfg(test)]
pub fn client_size(w: u32, h: u32, dpi: u32) -> Size {
    let inset = outer_size(0, 0, dpi);
    Size {
        w: (w as i32 - inset.0).max(1) as u32,
        h: (h as i32 - inset.1).max(1) as u32,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chrome_is_excluded_at_every_dpi() {
        for dpi in [96, 144, 192] {
            let outer = outer_size(1234, 789, dpi);
            assert_eq!(
                client_size(outer.0 as u32, outer.1 as u32, dpi),
                Size { w: 1234, h: 789 }
            );
        }
    }
}
