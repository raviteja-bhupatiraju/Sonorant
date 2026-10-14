//! Raw Win32 clipboard access for the temporary writes around copying a selection and pasting:
//! a full multi-format snapshot / restore of the user's clipboard, and writes that Windows keeps
//! out of clipboard history (Win+V) and cloud sync.

use std::time::Duration;

use windows_sys::Win32::Foundation::{GetLastError, GlobalFree, SetLastError, HANDLE, HGLOBAL};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData, GetClipboardFormatNameW,
    GetClipboardSequenceNumber, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::System::Ole::{
    CF_BITMAP, CF_DSPBITMAP, CF_DSPENHMETAFILE, CF_DSPMETAFILEPICT, CF_ENHMETAFILE, CF_GDIOBJFIRST, CF_GDIOBJLAST,
    CF_METAFILEPICT, CF_OWNERDISPLAY, CF_PALETTE, CF_PRIVATEFIRST, CF_PRIVATELAST, CF_UNICODETEXT,
};

use super::win::wide;

/// Formats larger than this are not kept (and are lost when the clipboard is restored).
const MAX_FORMAT_BYTES: usize = 64 << 20;
/// Stop saving further formats once this much has been copied.
const MAX_TOTAL_BYTES: usize = 256 << 20;

/// Registered formats that tell Windows not to record the content in clipboard history, the
/// cloud clipboard or clipboard monitors. Each is set to a DWORD 0.
const EXCLUSION_FORMATS: [&str; 3] =
    ["ExcludeClipboardContentFromMonitorProcessing", "CanIncludeInClipboardHistory", "CanUploadToCloudClipboard"];
/// OLE's private formats point at the previous owner's live data object; restoring them would make
/// OLE-aware applications ask an owner that is gone. Without them OLE wraps the plain formats.
const SKIPPED_NAMES: [&str; 2] = ["DataObject", "Ole Private Data"];

/// The clipboard, opened by this thread; closed on drop.
struct Open;

impl Open {
    /// The clipboard is briefly locked by whichever application is using it: retry for ~250 ms.
    fn new() -> Option<Self> {
        for i in 0..25 {
            if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
                return Some(Open);
            }
            if i < 24 {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        None
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        unsafe {
            CloseClipboard();
        }
    }
}

/// A copy of everything on the clipboard.
pub enum Saved {
    /// The formats that could be copied (empty if the clipboard really was empty).
    Formats(Vec<(u32, Vec<u8>)>),
    /// The clipboard could not be read: leave it alone rather than overwrite it with nothing.
    Failed,
}

fn format_name(format: u32) -> Option<String> {
    let mut buf = [0u16; 256];
    let n = unsafe { GetClipboardFormatNameW(format, buf.as_mut_ptr(), buf.len() as i32) };
    (n > 0).then(|| String::from_utf16_lossy(&buf[..n as usize]))
}

/// Whether `format`'s data is a GDI or other non-memory handle that cannot be copied as bytes.
fn is_handle_format(format: u32) -> bool {
    let f = format as u16;
    format <= 0xFFFF
        && (matches!(
            f,
            CF_BITMAP
                | CF_METAFILEPICT
                | CF_PALETTE
                | CF_ENHMETAFILE
                | CF_OWNERDISPLAY
                | CF_DSPBITMAP
                | CF_DSPMETAFILEPICT
                | CF_DSPENHMETAFILE
        ) || (CF_PRIVATEFIRST..=CF_PRIVATELAST).contains(&f)
            || (CF_GDIOBJFIRST..=CF_GDIOBJLAST).contains(&f))
}

/// Copy the contents of the HGLOBAL `h`. The clipboard must be open.
unsafe fn read_global(h: HANDLE) -> Option<Vec<u8>> {
    let size = GlobalSize(h as HGLOBAL);
    if size > MAX_FORMAT_BYTES {
        return None;
    }
    if size == 0 {
        return Some(Vec::new());
    }
    let p = GlobalLock(h as HGLOBAL) as *const u8;
    if p.is_null() {
        return None;
    }
    let bytes = std::slice::from_raw_parts(p, size).to_vec();
    GlobalUnlock(h as HGLOBAL);
    Some(bytes)
}

/// Save every memory-based format on the clipboard.
pub fn save() -> Saved {
    let Some(_open) = Open::new() else {
        log::warn!("clipboard: could not open it to save its content; it will not be restored");
        return Saved::Failed;
    };
    let mut formats = Vec::new();
    let (mut total, mut seen) = (0usize, 0usize);
    let mut format = 0;
    loop {
        // Earlier calls may have left an error code; 0 then means "no more formats".
        unsafe { SetLastError(0) };
        format = unsafe { EnumClipboardFormats(format) };
        if format == 0 {
            if unsafe { GetLastError() } != 0 {
                log::warn!("clipboard: enumerating formats failed; it will not be restored");
                return Saved::Failed;
            }
            break;
        }
        seen += 1;
        if is_handle_format(format) {
            continue;
        }
        if format >= 0xC000
            && format_name(format)
                .is_some_and(|n| SKIPPED_NAMES.contains(&n.as_str()) || EXCLUSION_FORMATS.contains(&n.as_str()))
        {
            continue;
        }
        let h = unsafe { GetClipboardData(format) };
        if h.is_null() {
            continue;
        }
        match unsafe { read_global(h) } {
            Some(bytes) if total + bytes.len() <= MAX_TOTAL_BYTES => {
                total += bytes.len();
                formats.push((format, bytes));
            }
            Some(_) => log::warn!("clipboard: content over {} MB; format {format} not saved", MAX_TOTAL_BYTES >> 20),
            None => log::warn!("clipboard: format {format} is unreadable or over {} MB; not saved", MAX_FORMAT_BYTES >> 20),
        }
    }
    if formats.is_empty() && seen > 0 {
        log::warn!("clipboard: none of its {seen} formats could be saved; it will not be restored");
        return Saved::Failed;
    }
    Saved::Formats(formats)
}

/// Place `bytes` on the open clipboard as `format`.
unsafe fn put(format: u32, bytes: &[u8]) -> bool {
    let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1));
    if h.is_null() {
        return false;
    }
    let p = GlobalLock(h) as *mut u8;
    if p.is_null() {
        GlobalFree(h);
        return false;
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
    GlobalUnlock(h);
    // On success the system owns the memory; on failure it is still ours.
    if SetClipboardData(format, h as HANDLE).is_null() {
        GlobalFree(h);
        return false;
    }
    true
}

/// Keep what is now on the open clipboard out of clipboard history and cloud sync.
unsafe fn exclude_from_history() {
    for name in EXCLUSION_FORMATS {
        let format = RegisterClipboardFormatW(wide(name).as_ptr());
        if format != 0 {
            put(format, &0u32.to_ne_bytes());
        }
    }
}

/// Put back what `save` copied. A failed save leaves the clipboard as it is.
pub fn restore(saved: Saved) {
    let Saved::Formats(formats) = saved else { return };
    let Some(_open) = Open::new() else {
        log::warn!("clipboard: could not open it to restore its content");
        return;
    };
    unsafe {
        if EmptyClipboard() == 0 {
            log::warn!("clipboard: could not empty it to restore its content");
            return;
        }
        for (format, bytes) in &formats {
            if !put(*format, bytes) {
                log::warn!("clipboard: could not restore format {format}");
            }
        }
        // The restored content was recorded in the history when the user copied it.
        if !formats.is_empty() {
            exclude_from_history();
        }
    }
}

/// Replace the clipboard with `text`, kept out of clipboard history with `exclude`. Returns the
/// clipboard sequence number afterwards, to detect a later change by someone else.
pub fn set_text(text: &str, exclude: bool) -> anyhow::Result<u32> {
    let open = Open::new().ok_or_else(|| anyhow::anyhow!("clipboard is busy"))?;
    unsafe {
        if EmptyClipboard() == 0 {
            anyhow::bail!("could not empty the clipboard (error {})", GetLastError());
        }
        let bytes: Vec<u8> = wide(text).iter().flat_map(|u| u.to_ne_bytes()).collect();
        if !put(CF_UNICODETEXT as u32, &bytes) {
            anyhow::bail!("could not write to the clipboard (error {})", GetLastError());
        }
        if exclude {
            exclude_from_history();
        }
    }
    drop(open);
    Ok(unsafe { GetClipboardSequenceNumber() })
}
