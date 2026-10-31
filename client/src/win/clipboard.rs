//! Plain-text clipboard sync with the Mac. Text copied here goes to the host,
//! which puts it on the Mac's pasteboard; text copied on the Mac arrives as a
//! `clipboard` message and goes on ours. What we wrote is not sent back.

use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber, OpenClipboard,
    SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_UNICODETEXT;

/// Larger text is not synced (the host's limit too).
const MAX_BYTES: usize = 1 << 20;

pub struct ClipboardSync {
    /// The clipboard sequence number last read or written.
    seen: u32,
    /// The text last sent or received, in Windows form.
    last: Option<String>,
}

impl ClipboardSync {
    pub fn new() -> Self {
        ClipboardSync {
            seen: unsafe { GetClipboardSequenceNumber() },
            last: None,
        }
    }

    /// Text to send the Mac: what was copied here since the last call, or with
    /// `now` whatever the clipboard holds (on connect). None when it is not
    /// text, too large, or what we last sent or received.
    pub fn poll(&mut self, hwnd: HWND, now: bool) -> Option<String> {
        let seq = unsafe { GetClipboardSequenceNumber() };
        if seq == self.seen && !now {
            return None;
        }
        // Another app can hold the clipboard open; try again next time.
        let text = read_text(hwnd).ok()?;
        self.seen = seq;
        let text =
            text.filter(|t| t.len() <= MAX_BYTES && (now || Some(t) != self.last.as_ref()))?;
        self.last = Some(text.clone());
        Some(to_mac(&text))
    }

    /// Put text copied on the Mac on our clipboard.
    pub fn set(&mut self, hwnd: HWND, text: &str) {
        let text = to_windows(text);
        if text.len() > MAX_BYTES || Some(&text) == self.last.as_ref() {
            return;
        }
        match write_text(hwnd, &text) {
            Ok(()) => {
                self.seen = unsafe { GetClipboardSequenceNumber() };
                self.last = Some(text);
            }
            Err(e) => eprintln!("clipboard: could not set the clipboard: {e}"),
        }
    }
}

/// Mac text uses `\n`; Windows apps expect `\r\n`.
fn to_windows(text: &str) -> String {
    to_mac(text).replace('\n', "\r\n")
}

fn to_mac(text: &str) -> String {
    text.replace("\r\n", "\n")
}

/// Open the clipboard, retrying briefly while another app has it.
unsafe fn open(hwnd: HWND) -> windows::core::Result<()> {
    let mut tries = 0;
    loop {
        match OpenClipboard(hwnd) {
            Ok(()) => return Ok(()),
            Err(_) if tries < 4 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            Err(e) => return Err(e),
        }
    }
}

/// The clipboard's text; None when it holds no text.
fn read_text(hwnd: HWND) -> windows::core::Result<Option<String>> {
    unsafe {
        open(hwnd)?;
        let text = GetClipboardData(CF_UNICODETEXT.0 as u32).ok().and_then(|h| {
            let mem = HGLOBAL(h.0);
            let ptr = GlobalLock(mem) as *const u16;
            if ptr.is_null() {
                return None;
            }
            let mut len = 0;
            while *ptr.add(len) != 0 {
                len += 1;
            }
            let text = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
            let _ = GlobalUnlock(mem);
            Some(text)
        });
        let _ = CloseClipboard();
        Ok(text)
    }
}

fn write_text(hwnd: HWND, text: &str) -> windows::core::Result<()> {
    let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    unsafe {
        let mem = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2)?;
        let ptr = GlobalLock(mem) as *mut u16;
        if ptr.is_null() {
            let _ = GlobalFree(mem);
            return Err(windows::core::Error::from_win32());
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
        let _ = GlobalUnlock(mem);
        if let Err(e) = open(hwnd) {
            let _ = GlobalFree(mem);
            return Err(e);
        }
        let result = EmptyClipboard()
            .and_then(|()| SetClipboardData(CF_UNICODETEXT.0 as u32, HANDLE(mem.0)).map(|_| ()));
        // The clipboard owns the memory once SetClipboardData succeeds.
        if result.is_err() {
            let _ = GlobalFree(mem);
        }
        let _ = CloseClipboard();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_endings() {
        assert_eq!(to_windows("a\nb\r\nc"), "a\r\nb\r\nc");
        assert_eq!(to_mac("a\r\nb\nc"), "a\nb\nc");
    }
}
