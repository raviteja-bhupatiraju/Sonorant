//! A small always-on-top status pill ("Recording", "Transcribing...", the transcribed text) shown
//! near the bottom of the screen while dictating. It never takes focus, ignores the mouse, and
//! must be created and used on the UI thread.
//!
//! Rendering is deliberately independent of the window's paint cycle: the pill is drawn into an
//! off-screen 32-bit bitmap (GDI for the text, the shared per-pixel maths in `crate::ui::overlay` for
//! the anti-aliased shape) and handed to the compositor in one `UpdateLayeredWindow` call. No `WM_PAINT`, no window
//! region, no reliance on a layered window's surface being initialised: what was rendered is
//! exactly what appears.
//!
//! The pill sizes itself to its content. Long text is wrapped to a maximum width and cut after a
//! few lines with an ellipsis, so it can never overflow the pill or the screen.

use windows_sys::Win32::Foundation::{COLORREF, HWND, POINT, RECT, SIZE};
use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, DrawTextW, GdiFlush, GetDC, ReleaseDC,
    SelectObject, SetBkMode, SetTextColor, AC_SRC_ALPHA, AC_SRC_OVER,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, CLEARTYPE_QUALITY, DIB_RGB_COLORS, DT_CALCRECT, DT_LEFT,
    DT_NOCLIP, DT_NOPREFIX, DT_SINGLELINE, DT_WORDBREAK, HDC, HFONT, TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, SetWindowPos, ShowWindow, SystemParametersInfoW,
    UpdateLayeredWindow, HWND_TOPMOST, SPI_GETWORKAREA, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_HIDE,
    SW_SHOWNOACTIVATE, ULW_ALPHA, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_EX_TRANSPARENT, WS_POPUP,
};

use super::win::wide;
use crate::ui::overlay::{apply_alpha, draw_dot, fill_pill, fit, Kind, HINT_RGB, MAX_LINES, TEXT_RGB};

fn rgb((r, g, b): (u8, u8, u8)) -> COLORREF {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}

unsafe fn font(px: f32, weight: i32) -> HFONT {
    CreateFontW(
        -(px.round() as i32),
        0,
        0,
        0,
        weight,
        0,
        0,
        0,
        1,
        0,
        0,
        CLEARTYPE_QUALITY as u32,
        0,
        wide("Segoe UI").as_ptr(),
    )
}

/// Size of `text` wrapped to `width` pixels with the font currently selected into `hdc`.
unsafe fn measure(hdc: HDC, text: &str, width: i32) -> (i32, i32) {
    let w = wide(text);
    let mut rc = RECT { left: 0, top: 0, right: width.max(1), bottom: 0 };
    DrawTextW(hdc, w.as_ptr(), -1, &mut rc, DT_CALCRECT | DT_WORDBREAK | DT_LEFT | DT_NOPREFIX);
    (rc.right - rc.left, rc.bottom - rc.top)
}

pub struct Overlay {
    hwnd: HWND,
}

impl Overlay {
    pub fn new() -> Option<Self> {
        unsafe {
            let class = wide("SonorantOverlay");
            let hinstance = GetModuleHandleW(std::ptr::null());
            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(DefWindowProcW),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinstance,
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: class.as_ptr(),
            };
            RegisterClassW(&wc); // fails harmlessly if already registered
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED | WS_EX_TRANSPARENT,
                class.as_ptr(),
                wide("Sonorant").as_ptr(),
                WS_POPUP,
                0,
                0,
                10,
                10,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                hinstance,
                std::ptr::null(),
            );
            if hwnd.is_null() {
                return None;
            }
            Some(Self { hwnd })
        }
    }

    /// Show `text` (and an optional small `hint` line under it), sizing the pill to fit.
    pub fn show(&self, kind: Kind, text: &str, hint: Option<&str>) {
        unsafe {
            let scale = GetDpiForWindow(self.hwnd).max(96) as f32 / 96.0;
            let mut area: RECT = std::mem::zeroed();
            SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut area as *mut RECT as *mut _, 0);
            let work_w = area.right - area.left;

            // ---- layout, in device pixels ----
            let pad_left = (44.0 * scale) as i32; // room for the status dot
            let pad_right = (20.0 * scale) as i32;
            let pad_v = (12.0 * scale) as i32;
            let min_w = (230.0 * scale) as i32;
            let min_h = (46.0 * scale) as i32;
            let max_w = ((560.0 * scale) as i32).min(work_w * 7 / 10).max(min_w);
            let max_text_w = max_w - pad_left - pad_right;

            let screen = GetDC(std::ptr::null_mut());
            let hdc = CreateCompatibleDC(screen);
            let main_font = font(15.0 * scale, 400);
            let small_font = font(11.5 * scale, 400);
            let old_font = SelectObject(hdc, main_font as _);

            let line_h = measure(hdc, "Ag", max_text_w).1;
            // While recording the text is the live transcript, which grows at its end.
            let (text, text_w, text_h) =
                fit(text, line_h * MAX_LINES, kind == Kind::Recording, |t| measure(hdc, t, max_text_w));
            let hint = hint.unwrap_or("");
            let (mut hint_w, mut hint_h) = (0, 0);
            if !hint.is_empty() {
                SelectObject(hdc, small_font as _);
                (hint_w, hint_h) = measure(hdc, hint, max_text_w);
                SelectObject(hdc, main_font as _);
            }
            let gap = if hint_h > 0 { (2.0 * scale) as i32 } else { 0 };
            let slack = (8.0 * scale) as i32; // wrapping at draw time can never differ from measuring
            let w = (pad_left + text_w.max(hint_w) + slack + pad_right).clamp(min_w, max_w);
            let content_h = text_h + gap + hint_h;
            let h = (content_h + 2 * pad_v).max(min_h);
            let top = (h - content_h) / 2;
            let text_area_w = w - pad_left - pad_right;

            // ---- off-screen 32-bit bitmap ----
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h, // top-down
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB,
                    biSizeImage: 0,
                    biXPelsPerMeter: 0,
                    biYPelsPerMeter: 0,
                    biClrUsed: 0,
                    biClrImportant: 0,
                },
                bmiColors: [std::mem::zeroed()],
            };
            let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
            let bitmap = CreateDIBSection(hdc, &bmi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
            if bitmap.is_null() || bits.is_null() {
                SelectObject(hdc, old_font);
                DeleteObject(main_font as _);
                DeleteObject(small_font as _);
                DeleteDC(hdc);
                ReleaseDC(std::ptr::null_mut(), screen);
                return;
            }
            let old_bitmap = SelectObject(hdc, bitmap as _);
            let pixels = std::slice::from_raw_parts_mut(bits as *mut u32, (w * h) as usize);

            let coverage = fill_pill(pixels, w, h, scale);
            // Status dot beside the first line.
            draw_dot(pixels, w, (22.0 * scale, (top + line_h / 2) as f32), 6.0 * scale, kind.dot_rgb());

            // Text.
            SetBkMode(hdc, TRANSPARENT as i32);
            SetTextColor(hdc, rgb(TEXT_RGB));
            let wtext = wide(&text);
            let mut text_rc = RECT { left: pad_left, top, right: pad_left + text_area_w, bottom: top + text_h };
            let flags = if text_h <= line_h {
                DT_SINGLELINE | DT_LEFT | DT_NOPREFIX | DT_NOCLIP
            } else {
                DT_WORDBREAK | DT_LEFT | DT_NOPREFIX | DT_NOCLIP
            };
            DrawTextW(hdc, wtext.as_ptr(), -1, &mut text_rc, flags);
            if !hint.is_empty() {
                SelectObject(hdc, small_font as _);
                SetTextColor(hdc, rgb(HINT_RGB));
                let hw = wide(hint);
                let y0 = top + text_h + gap;
                let mut hint_rc = RECT { left: pad_left, top: y0, right: pad_left + text_area_w, bottom: y0 + hint_h };
                DrawTextW(hdc, hw.as_ptr(), -1, &mut hint_rc, DT_WORDBREAK | DT_LEFT | DT_NOPREFIX | DT_NOCLIP);
            }
            GdiFlush();

            apply_alpha(pixels, &coverage);

            // Hand the finished bitmap to the compositor.
            let x = area.left + (work_w - w) / 2;
            let y = area.bottom - h - (28.0 * scale) as i32;
            let dst = POINT { x, y };
            let size = SIZE { cx: w, cy: h };
            let src = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let ok = UpdateLayeredWindow(self.hwnd, screen, &dst, &size, hdc, &src, 0, &blend, ULW_ALPHA);
            if ok == 0 {
                log::warn!("overlay: UpdateLayeredWindow failed ({}x{} at {},{}): error {}", w, h, x, y, windows_sys::Win32::Foundation::GetLastError());
            }
            ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            SetWindowPos(self.hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);

            SelectObject(hdc, old_bitmap);
            SelectObject(hdc, old_font);
            DeleteObject(bitmap as _);
            DeleteObject(main_font as _);
            DeleteObject(small_font as _);
            DeleteDC(hdc);
            ReleaseDC(std::ptr::null_mut(), screen);
        }
    }

    pub fn hide(&self) {
        unsafe {
            ShowWindow(self.hwnd, SW_HIDE);
        }
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        unsafe {
            DestroyWindow(self.hwnd);
        }
    }
}
