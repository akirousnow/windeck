use std::ffi::c_void;

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleDC, CreateDIBSection,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, HALFTONE, HBITMAP, HDC, HGDIOBJ, ReleaseDC,
    SRCCOPY, SelectObject, SetBrushOrgEx, SetStretchBltMode, StretchBlt,
};
use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowRect, IsHungAppWindow, IsIconic, IsWindow,
};

const PW_RENDERFULLCONTENT: u32 = 0x0000_0002;

/// Top-down 32-bit BGRA bitmap captured from the desktop composition surface or target window.
/// Directly compatible with GPUI `RenderImage` (`DXGI_FORMAT_B8G8R8A8_UNORM`).
#[derive(Clone)]
pub struct CapturedBitmap {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

struct ScreenDcGuard(HDC);
impl Drop for ScreenDcGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = ReleaseDC(None, self.0);
            }
        }
    }
}

struct MemDcGuard(HDC);
impl Drop for MemDcGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = DeleteDC(self.0);
            }
        }
    }
}

struct OwnedBitmapGuard(HBITMAP);
impl Drop for OwnedBitmapGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = DeleteObject(self.0.into());
            }
        }
    }
}

struct SelectGuard {
    dc: HDC,
    prev: HGDIOBJ,
}
impl Drop for SelectGuard {
    fn drop(&mut self) {
        if !self.dc.is_invalid() && !self.prev.is_invalid() {
            unsafe {
                let _ = SelectObject(self.dc, self.prev);
            }
        }
    }
}

fn create_top_down_dib(
    compat_dc: HDC,
    width: i32,
    height: i32,
) -> Option<(MemDcGuard, OwnedBitmapGuard, SelectGuard, *mut u8)> {
    let mem_dc = MemDcGuard(unsafe { CreateCompatibleDC(Some(compat_dc)) });
    if mem_dc.0.is_invalid() {
        return None;
    }

    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height, // top-down DIB
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };

    let mut bits_ptr: *mut c_void = std::ptr::null_mut();
    let dib_handle = unsafe {
        CreateDIBSection(
            Some(mem_dc.0),
            &bmi,
            DIB_RGB_COLORS,
            &mut bits_ptr,
            None,
            0,
        )
    }
    .ok()?;
    if dib_handle.is_invalid() || bits_ptr.is_null() {
        return None;
    }
    let dib = OwnedBitmapGuard(dib_handle);

    let prev_obj = unsafe { SelectObject(mem_dc.0, dib.0.into()) };
    if prev_obj.is_invalid() {
        return None;
    }
    let select_guard = SelectGuard {
        dc: mem_dc.0,
        prev: prev_obj,
    };

    Some((mem_dc, dib, select_guard, bits_ptr as *mut u8))
}

fn blit_dc_region_to_bgra(
    src_dc: HDC,
    src_rect: RECT,
    dst_w: i32,
    dst_h: i32,
) -> Option<CapturedBitmap> {
    let src_w = src_rect.right - src_rect.left;
    let src_h = src_rect.bottom - src_rect.top;
    if src_w < 2 || src_h < 2 || dst_w < 2 || dst_h < 2 {
        return None;
    }

    let (mem_dc, _dib, _sel, bits_ptr) = create_top_down_dib(src_dc, dst_w, dst_h)?;

    let blt_ok = if dst_w == src_w && dst_h == src_h {
        unsafe {
            BitBlt(
                mem_dc.0,
                0,
                0,
                dst_w,
                dst_h,
                Some(src_dc),
                src_rect.left,
                src_rect.top,
                SRCCOPY,
            )
        }
        .is_ok()
    } else {
        unsafe {
            let _ = SetStretchBltMode(mem_dc.0, HALFTONE);
            let _ = SetBrushOrgEx(mem_dc.0, 0, 0, None);
            StretchBlt(
                mem_dc.0,
                0,
                0,
                dst_w,
                dst_h,
                Some(src_dc),
                src_rect.left,
                src_rect.top,
                src_w,
                src_h,
                SRCCOPY,
            )
        }
        .as_bool()
    };

    if !blt_ok {
        return None;
    }

    let byte_len = (dst_w as usize) * (dst_h as usize) * 4;
    let mut bgra = vec![0u8; byte_len];
    unsafe {
        std::ptr::copy_nonoverlapping(bits_ptr as *const u8, bgra.as_mut_ptr(), byte_len);
    }

    // GDI BitBlt/StretchBlt from screen/window DCs may leave alpha 0; force opaque BGRA8.
    for px in bgra.chunks_exact_mut(4) {
        px[3] = 255;
    }

    Some(CapturedBitmap {
        width: dst_w as u32,
        height: dst_h as u32,
        bgra,
    })
}

/// Captures a physical screen rectangle from the DWM desktop composition surface into a
/// top-down BGRA8 bitmap with alpha forced to `255` (fully opaque).
///
/// Uses plain `SRCCOPY` (without `CAPTUREBLT`) to avoid hardware cursor flicker. If the captured
/// region is wider than `max_width`, downscales smoothly using `HALFTONE` `StretchBlt`.
pub fn capture_screen_rect_bgra(phys_rect: RECT, max_width: u32) -> Option<CapturedBitmap> {
    let src_w = phys_rect.right - phys_rect.left;
    let src_h = phys_rect.bottom - phys_rect.top;
    if src_w < 4 || src_h < 4 || src_w > 8192 || src_h > 8192 {
        return None;
    }

    let max_w = max_width.max(32) as i32;
    let (dst_w, dst_h) = if src_w > max_w {
        let scale = max_w as f32 / src_w as f32;
        let h = ((src_h as f32) * scale).round().max(4.0) as i32;
        (max_w, h)
    } else {
        (src_w, src_h)
    };

    let screen_dc = ScreenDcGuard(unsafe { GetDC(None) });
    if screen_dc.0.is_invalid() {
        return None;
    }

    blit_dc_region_to_bgra(screen_dc.0, phys_rect, dst_w, dst_h)
}

/// Captures a specific top-level window via `PrintWindow(..., PW_RENDERFULLCONTENT)` and scales/crops
/// it to `dst_w × dst_h` BGRA8. Used as a fallback when desktop screen capture is unavailable
/// (e.g., clipped card or capture-restricted session).
///
/// Skips iconic or hung windows so the UI thread never blocks.
pub fn capture_window_bgra(
    hwnd_raw: isize,
    dst_w: u32,
    dst_h: u32,
    cover_crop: bool,
) -> Option<CapturedBitmap> {
    if hwnd_raw == 0 || dst_w < 4 || dst_h < 4 {
        return None;
    }
    let hwnd = HWND(hwnd_raw as *mut c_void);
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool()
        || unsafe { IsIconic(hwnd) }.as_bool()
        || unsafe { IsHungAppWindow(hwnd) }.as_bool()
    {
        return None;
    }

    let mut wr = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut wr) }.ok()?;
    let win_w = wr.right - wr.left;
    let win_h = wr.bottom - wr.top;
    if win_w < 4 || win_h < 4 || win_w > 8192 || win_h > 8192 {
        return None;
    }

    let screen_dc = ScreenDcGuard(unsafe { GetDC(None) });
    if screen_dc.0.is_invalid() {
        return None;
    }

    let (win_dc, _win_dib, _win_sel, _) = create_top_down_dib(screen_dc.0, win_w, win_h)?;
    let ok = unsafe { PrintWindow(hwnd, win_dc.0, PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT)) }
        .as_bool();
    if !ok {
        return None;
    }

    let dst_wi = dst_w.min(1024) as i32;
    let dst_hi = dst_h.min(1024) as i32;
    let src_rect = if cover_crop {
        let aspect_src = win_w as f32 / win_h.max(1) as f32;
        let aspect_dst = dst_wi as f32 / dst_hi.max(1) as f32;
        if (aspect_src - aspect_dst).abs() <= 0.01 {
            RECT {
                left: 0,
                top: 0,
                right: win_w,
                bottom: win_h,
            }
        } else if aspect_src > aspect_dst {
            let keep_w = ((win_h as f32) * aspect_dst)
                .round()
                .clamp(1.0, win_w as f32) as i32;
            let pad_x = (win_w - keep_w) / 2;
            RECT {
                left: pad_x,
                top: 0,
                right: pad_x + keep_w,
                bottom: win_h,
            }
        } else {
            let keep_h = ((win_w as f32) / aspect_dst)
                .round()
                .clamp(1.0, win_h as f32) as i32;
            RECT {
                left: 0,
                top: 0,
                right: win_w,
                bottom: keep_h,
            }
        }
    } else {
        RECT {
            left: 0,
            top: 0,
            right: win_w,
            bottom: win_h,
        }
    };

    blit_dc_region_to_bgra(win_dc.0, src_rect, dst_wi, dst_hi)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blit_dc_region_downscales_and_forces_opaque_bgra() {
        let screen_dc = ScreenDcGuard(unsafe { GetDC(None) });
        assert!(!screen_dc.0.is_invalid());
        let (src_dc, _dib, _sel, ptr) =
            create_top_down_dib(screen_dc.0, 160, 100).expect("create source DIB");

        // Paint a solid BGRA color (B=0x33, G=0x66, R=0xCC, A=0x00) into the source DIB.
        unsafe {
            let buf = std::slice::from_raw_parts_mut(ptr, 160 * 100 * 4);
            for px in buf.chunks_exact_mut(4) {
                px[0] = 0x33;
                px[1] = 0x66;
                px[2] = 0xCC;
                px[3] = 0x00;
            }
        }

        let bmp = blit_dc_region_to_bgra(
            src_dc.0,
            RECT {
                left: 0,
                top: 0,
                right: 160,
                bottom: 100,
            },
            80,
            50,
        )
        .expect("downscaled blit from memory DC should succeed");

        assert_eq!(bmp.width, 80);
        assert_eq!(bmp.height, 50);
        assert_eq!(bmp.bgra.len(), 80 * 50 * 4);
        assert!(bmp.bgra.chunks_exact(4).all(|px| px[3] == 255));
        let center = ((25 * 80 + 40) * 4) as usize;
        assert!((bmp.bgra[center] as i32 - 0x33).abs() <= 2);
        assert!((bmp.bgra[center + 1] as i32 - 0x66).abs() <= 2);
        assert!((bmp.bgra[center + 2] as i32 - 0xCC).abs() <= 2);
    }

    #[test]
    fn capture_rejects_degenerate_bounds_and_invalid_hwnd() {
        assert!(capture_screen_rect_bgra(RECT::default(), 400).is_none());
        assert!(
            capture_screen_rect_bgra(
                RECT {
                    left: 10,
                    top: 10,
                    right: 12,
                    bottom: 100,
                },
                400,
            )
            .is_none()
        );
        assert!(capture_window_bgra(0, 380, 226, true).is_none());
    }
}
