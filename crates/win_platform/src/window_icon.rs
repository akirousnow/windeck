use std::ffi::c_void;

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC,
    DeleteObject, GetDIBits, GetObjectW, HBITMAP, HDC,
};
use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
use windows::Win32::UI::Shell::{SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGetFileInfoW};
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyIcon, GCLP_HICON, GCLP_HICONSM, GetClassLongPtrW, GetIconInfo, HICON, ICON_BIG,
    ICON_SMALL, ICON_SMALL2, ICONINFO, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_GETICON,
};
use windows_core::{HSTRING, PCWSTR};

/// Extracted window icon in top-down BGRA8 format (directly compatible with GPUI `RenderImage`).
#[derive(Clone)]
pub struct IconBgra {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

struct OwnedIcon(HICON);
impl Drop for OwnedIcon {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _ = unsafe { DestroyIcon(self.0) };
        }
    }
}

struct OwnedBitmap(HBITMAP);
impl Drop for OwnedBitmap {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _ = unsafe { DeleteObject(self.0.into()) };
        }
    }
}

struct OwnedDc(HDC);
impl Drop for OwnedDc {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _ = unsafe { DeleteDC(self.0) };
        }
    }
}

fn query_window_icon_nonblocking(hwnd: HWND, kind: u32) -> Option<HICON> {
    let mut result: usize = 0;
    let ok = unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_GETICON,
            WPARAM(kind as usize),
            LPARAM(0),
            SMTO_ABORTIFHUNG,
            35,
            Some(&mut result),
        )
    };
    if ok.0 != 0 && result != 0 {
        Some(HICON(result as *mut c_void))
    } else {
        None
    }
}

fn hicon_to_bgra(hicon: HICON) -> Option<IconBgra> {
    let mut info = ICONINFO::default();
    unsafe { GetIconInfo(hicon, &mut info) }.ok()?;
    let color_guard = OwnedBitmap(info.hbmColor);
    let mask_guard = OwnedBitmap(info.hbmMask);

    let target_bmp = if !color_guard.0.is_invalid() {
        color_guard.0
    } else {
        return None;
    };

    let mut bmp = BITMAP::default();
    let sz = unsafe {
        GetObjectW(
            target_bmp.into(),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bmp as *mut _ as *mut c_void),
        )
    };
    if sz == 0 || bmp.bmWidth <= 0 || bmp.bmHeight <= 0 {
        return None;
    }

    let w = bmp.bmWidth as u32;
    let h = bmp.bmHeight as u32;
    let hdc = OwnedDc(unsafe { CreateCompatibleDC(None) });
    if hdc.0.is_invalid() {
        return None;
    }

    let mut bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w as i32,
            biHeight: -(h as i32), // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };

    let mut bgra = vec![0u8; (w * h * 4) as usize];
    let lines = unsafe {
        GetDIBits(
            hdc.0,
            target_bmp,
            0,
            h,
            Some(bgra.as_mut_ptr() as *mut c_void),
            &mut bmi,
            DIB_RGB_COLORS,
        )
    };
    if lines == 0 {
        return None;
    }

    // Legacy 24-bit icons leave alpha channel all-zero; reconstruct alpha from hbmMask if needed.
    let has_any_alpha = bgra.chunks_exact(4).any(|px| px[3] != 0);
    if !has_any_alpha && !mask_guard.0.is_invalid() {
        let mut mask_bgra = vec![0u8; (w * h * 4) as usize];
        let mask_lines = unsafe {
            GetDIBits(
                hdc.0,
                mask_guard.0,
                0,
                h,
                Some(mask_bgra.as_mut_ptr() as *mut c_void),
                &mut bmi,
                DIB_RGB_COLORS,
            )
        };
        if mask_lines > 0 {
            for (px, mpx) in bgra.chunks_exact_mut(4).zip(mask_bgra.chunks_exact(4)) {
                px[3] = if mpx[0] == 0 { 255 } else { 0 };
            }
        } else {
            for px in bgra.chunks_exact_mut(4) {
                px[3] = 255;
            }
        }
    }

    Some(IconBgra {
        width: w,
        height: h,
        bgra,
    })
}

/// Extract a window's icon safely (non-blocking timeout, zero leaked GDI/HICON handles).
pub fn extract_window_icon(hwnd_raw: isize, exe_path: &str) -> Option<IconBgra> {
    let hwnd = HWND(hwnd_raw as *mut c_void);

    // 1. Borrowed window / class icons (do NOT DestroyIcon on these!)
    let borrowed = query_window_icon_nonblocking(hwnd, ICON_BIG)
        .or_else(|| query_window_icon_nonblocking(hwnd, ICON_SMALL2))
        .or_else(|| query_window_icon_nonblocking(hwnd, ICON_SMALL))
        .or_else(|| {
            let p = unsafe { GetClassLongPtrW(hwnd, GCLP_HICON) };
            (p != 0).then(|| HICON(p as *mut c_void))
        })
        .or_else(|| {
            let p = unsafe { GetClassLongPtrW(hwnd, GCLP_HICONSM) };
            (p != 0).then(|| HICON(p as *mut c_void))
        });

    if let Some(hicon) = borrowed {
        if let Some(img) = hicon_to_bgra(hicon) {
            return Some(img);
        }
    }

    // 2. Fallback: executable shell icon (owned HICON -> wrapped in OwnedIcon RAII guard!)
    if !exe_path.is_empty() {
        let wide = HSTRING::from(exe_path);
        let mut sfi = SHFILEINFOW::default();
        let res = unsafe {
            SHGetFileInfoW(
                PCWSTR(wide.as_ptr()),
                FILE_FLAGS_AND_ATTRIBUTES(0),
                Some(&mut sfi),
                std::mem::size_of::<SHFILEINFOW>() as u32,
                SHGFI_ICON | SHGFI_LARGEICON,
            )
        };
        if res != 0 && !sfi.hIcon.is_invalid() {
            let owned = OwnedIcon(sfi.hIcon);
            return hicon_to_bgra(owned.0);
        }
    }

    None
}
