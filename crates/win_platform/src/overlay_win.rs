use std::ffi::c_void;
use std::sync::Once;

use anyhow::{Context, Result};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWM_WINDOW_CORNER_PREFERENCE, DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWMWCP_DONOTROUND, DWMWCP_ROUND, DwmExtendFrameIntoClientArea, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTOPRIMARY, MONITOR_DEFAULTTONEAREST, MONITORINFO,
    MonitorFromPoint,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleA, GetModuleHandleW, GetProcAddress};
use windows::Win32::UI::Controls::MARGINS;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_KEYUP, SendInput, VK_MENU,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GWL_EXSTYLE, GWL_STYLE, GetClientRect,
    GetCursorPos, GetWindowLongPtrW, GetWindowRect, HWND_TOPMOST, IsWindowVisible, RegisterClassW,
    SW_HIDE, SW_SHOW, SW_SHOWNOACTIVATE, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOZORDER, SWP_SHOWWINDOW, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    WNDCLASSW, WS_BORDER, WS_CAPTION, WS_DLGFRAME, WS_EX_CLIENTEDGE, WS_EX_DLGMODALFRAME,
    WS_EX_NOACTIVATE, WS_EX_NOREDIRECTIONBITMAP, WS_EX_STATICEDGE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_EX_TRANSPARENT, WS_EX_WINDOWEDGE, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_POPUP, WS_SYSMENU,
    WS_THICKFRAME,
};
use windows_core::{BOOL, PCSTR, w};

const DWMWA_COLOR_NONE: u32 = 0xFFFFFFFE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayMonitorPolicy {
    Cursor,
    Primary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackdropStyle {
    Blurred,
    Transparent,
    Opaque,
}

#[repr(C)]
struct WindowCompositionAttribData {
    attrib: u32,
    pv_data: *mut c_void,
    cb_data: usize,
}

#[repr(C)]
struct AccentPolicy {
    accent_state: u32,
    accent_flags: u32,
    gradient_color: u32,
    animation_id: u32,
}

fn apply_accent_policy(hwnd: HWND, state: u32, flags: u32, rgba: (u8, u8, u8, u8)) {
    unsafe {
        type SetWcaFn =
            unsafe extern "system" fn(HWND, *mut WindowCompositionAttribData) -> BOOL;
        let Ok(user32) = GetModuleHandleA(PCSTR::from_raw(c"user32.dll".as_ptr() as *const u8))
        else {
            return;
        };
        let Some(proc_addr) =
            GetProcAddress(user32, PCSTR::from_raw(c"SetWindowCompositionAttribute".as_ptr() as *const u8))
        else {
            return;
        };
        let set_wca: SetWcaFn = std::mem::transmute(proc_addr);
        let accent = AccentPolicy {
            accent_state: state,
            accent_flags: flags,
            gradient_color: (rgba.0 as u32)
                | ((rgba.1 as u32) << 8)
                | ((rgba.2 as u32) << 16)
                | ((rgba.3 as u32) << 24),
            animation_id: 0,
        };
        let mut data = WindowCompositionAttribData {
            attrib: 19, // WCA_ACCENT_POLICY
            pv_data: &accent as *const _ as *mut c_void,
            cb_data: std::mem::size_of::<AccentPolicy>(),
        };
        let _ = set_wca(hwnd, &mut data);
    }
}

/// Applies the frosted acrylic / translucent / opaque backdrop directly to a Win32 HWND.
/// `blur_opacity` controls the tint density (`0.05..=0.95`, lower = more transparent Gaussian blur).
pub fn apply_backdrop_style(hwnd: HWND, style: BackdropStyle, blur_opacity: f32) {
    let alpha = (blur_opacity.clamp(0.02, 0.95) * 255.0).round() as u8;
    match style {
        BackdropStyle::Blurred => {
            // ACCENT_ENABLE_ACRYLICBLURBEHIND: accent_flags=0 + low alpha yields luminous macOS Gaussian blur
            apply_accent_policy(hwnd, 4, 0, (10, 14, 22, alpha.max(1)));
        }
        BackdropStyle::Transparent => {
            // ACCENT_ENABLE_TRANSPARENTGRADIENT with adjustable dark translucent veil
            apply_accent_policy(hwnd, 2, 2, (10, 14, 22, alpha.max(24)));
        }
        BackdropStyle::Opaque => {
            // ACCENT_ENABLE_GRADIENT with deep slate canvas
            apply_accent_policy(hwnd, 1, 2, (12, 15, 22, 255));
        }
    }
}

/// Dedicated bottom-layer Win32 HWND that hosts the frosted Acrylic backdrop AND all
/// `DwmRegisterThumbnail` live window streams strictly beneath the interactive GPUI overlay HWND.
///
/// This two-layer split enables GPUI elements (selection rings, embedded bottom-right / bottom-left /
/// center app icon + title pills) to composite *above* live 60fps DWM thumbnails with full alpha blending.
pub struct ThumbnailHostWindow {
    hwnd: HWND,
}

impl ThumbnailHostWindow {
    pub fn create(fullscreen: bool, backdrop: BackdropStyle, blur_opacity: f32) -> Result<Self> {
        static REGISTER_ONCE: Once = Once::new();
        let class_name = w!("WinDeckThumbHostClass");
        let hinstance = unsafe { GetModuleHandleW(None) }?;

        unsafe extern "system" fn host_proc(
            hwnd: HWND,
            msg: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }

        REGISTER_ONCE.call_once(|| {
            let wc = WNDCLASSW {
                lpfnWndProc: Some(host_proc),
                hInstance: hinstance.into(),
                lpszClassName: class_name,
                ..Default::default()
            };
            unsafe { RegisterClassW(&wc) };
        });

        let ex_style = WS_EX_TOOLWINDOW
            | WS_EX_NOACTIVATE
            | WS_EX_TRANSPARENT
            | WS_EX_TOPMOST
            | WS_EX_NOREDIRECTIONBITMAP;

        let hwnd = unsafe {
            CreateWindowExW(
                ex_style,
                class_name,
                w!("WinDeckThumbHost"),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                Some(hinstance.into()),
                None,
            )
        }
        .context("CreateWindowExW for ThumbnailHostWindow")?;

        let host = Self { hwnd };
        host.configure(fullscreen, backdrop, blur_opacity);
        Ok(host)
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    pub fn configure(&self, fullscreen: bool, backdrop: BackdropStyle, blur_opacity: f32) {
        unsafe {
            let margins = MARGINS {
                cxLeftWidth: -1,
                cxRightWidth: -1,
                cyTopHeight: -1,
                cyBottomHeight: -1,
            };
            let _ = DwmExtendFrameIntoClientArea(self.hwnd, &margins);

            let no_border: u32 = DWMWA_COLOR_NONE;
            let _ = DwmSetWindowAttribute(
                self.hwnd,
                DWMWA_BORDER_COLOR,
                &no_border as *const _ as *const c_void,
                std::mem::size_of::<u32>() as u32,
            );

            let corner_pref: DWM_WINDOW_CORNER_PREFERENCE = if fullscreen {
                DWMWCP_DONOTROUND
            } else {
                DWMWCP_ROUND
            };
            let _ = DwmSetWindowAttribute(
                self.hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &corner_pref as *const _ as *const c_void,
                std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
            );
        }
        apply_backdrop_style(self.hwnd, backdrop, blur_opacity);
    }
}

impl Drop for ThumbnailHostWindow {
    fn drop(&mut self) {
        if !self.hwnd.is_invalid() {
            let _ = unsafe { DestroyWindow(self.hwnd) };
            self.hwnd = HWND::default();
        }
    }
}

/// Strips every Win32/DWM border, caption, frame, and corner rounding so the GPUI window
/// becomes a 100% seamless edge-to-edge borderless canvas (macOS Mission Control style).
pub fn configure_borderless_immersive(hwnd: HWND, fullscreen: bool) {
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
        let stripped_style = (style
            & !(WS_CAPTION.0
                | WS_THICKFRAME.0
                | WS_BORDER.0
                | WS_DLGFRAME.0
                | WS_SYSMENU.0
                | WS_MINIMIZEBOX.0
                | WS_MAXIMIZEBOX.0))
            | WS_POPUP.0;
        SetWindowLongPtrW(hwnd, GWL_STYLE, stripped_style as isize);

        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        let clean_ex = (ex_style
            & !(WS_EX_WINDOWEDGE.0
                | WS_EX_CLIENTEDGE.0
                | WS_EX_DLGMODALFRAME.0
                | WS_EX_STATICEDGE.0))
            | WS_EX_TOOLWINDOW.0
            | WS_EX_TOPMOST.0;
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, clean_ex as isize);

        let no_border: u32 = DWMWA_COLOR_NONE;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            &no_border as *const _ as *const c_void,
            std::mem::size_of::<u32>() as u32,
        );

        let corner_pref: DWM_WINDOW_CORNER_PREFERENCE = if fullscreen {
            DWMWCP_DONOTROUND
        } else {
            DWMWCP_ROUND
        };
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner_pref as *const _ as *const c_void,
            std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        );

        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

pub fn apply_win11_rounded_corners(hwnd: HWND) {
    configure_borderless_immersive(hwnd, false);
}

/// Returns `(full_monitor_rect, work_area_rect, scale_factor)` for the chosen monitor.
pub fn target_monitor_rects(policy: OverlayMonitorPolicy) -> Result<(RECT, RECT, f32)> {
    let hmon = match policy {
        OverlayMonitorPolicy::Cursor => {
            let mut pt = POINT::default();
            let _ = unsafe { GetCursorPos(&mut pt) };
            unsafe { MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST) }
        }
        OverlayMonitorPolicy::Primary => {
            unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) }
        }
    };

    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe { GetMonitorInfoW(hmon, &mut info) }
        .ok()
        .context("GetMonitorInfoW")?;

    let mut dpi_x = 96u32;
    let mut dpi_y = 96u32;
    let _ = unsafe { GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
    let scale = (dpi_x as f32 / 96.0).max(1.0);

    Ok((info.rcMonitor, info.rcWork, scale))
}

pub fn target_monitor_work_area(policy: OverlayMonitorPolicy) -> Result<(RECT, f32)> {
    let (_, work, scale) = target_monitor_rects(policy)?;
    Ok((work, scale))
}

fn compute_overlay_rect(
    policy: OverlayMonitorPolicy,
    immersive_fullscreen: bool,
) -> Result<(i32, i32, i32, i32, f32)> {
    let (monitor_rc, work_rc, scale) = target_monitor_rects(policy)?;
    let (x, y, win_w, win_h) = if immersive_fullscreen {
        (
            monitor_rc.left,
            monitor_rc.top,
            (monitor_rc.right - monitor_rc.left).max(640),
            (monitor_rc.bottom - monitor_rc.top).max(480),
        )
    } else {
        let work_w = (work_rc.right - work_rc.left).max(640);
        let work_h = (work_rc.bottom - work_rc.top).max(480);
        let w = ((work_w as f32) * 0.90).round() as i32;
        let h = ((work_h as f32) * 0.88).round() as i32;
        let w = w.clamp(760.min(work_w), work_w - 16);
        let h = h.clamp(500.min(work_h), work_h - 16);
        (
            work_rc.left + (work_w - w) / 2,
            work_rc.top + (work_h - h) / 2,
            w,
            h,
        )
    };
    Ok((x, y, win_w, win_h, scale))
}

fn focus_hwnd(hwnd: HWND) {
    unsafe {
        if !SetForegroundWindow(hwnd).as_bool() {
            // Never inject a synthetic VK_MENU KEYUP while the user is physically holding Alt,
            // as doing so would break hold-Alt + arrow + release-to-jump tracking.
            if (GetAsyncKeyState(VK_MENU.0 as i32) as u16 & 0x8000) == 0 {
                let inputs = [
                    INPUT {
                        r#type: INPUT_KEYBOARD,
                        Anonymous: INPUT_0 {
                            ki: KEYBDINPUT {
                                wVk: VK_MENU,
                                dwFlags: KEYBD_EVENT_FLAGS(0),
                                ..Default::default()
                            },
                        },
                    },
                    INPUT {
                        r#type: INPUT_KEYBOARD,
                        Anonymous: INPUT_0 {
                            ki: KEYBDINPUT {
                                wVk: VK_MENU,
                                dwFlags: KEYEVENTF_KEYUP,
                                ..Default::default()
                            },
                        },
                    },
                ];
                SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
            }
            let _ = SetForegroundWindow(hwnd);
        }
    }
}

/// Positions and reveals both layers in strict lockstep Z-order:
/// `gpui_hwnd` (topmost, per-pixel transparent interactive UI + embedded labels) directly above
/// `thumb_host_hwnd` (frosted Acrylic backdrop + live hardware-accelerated DWM thumbnails).
pub fn show_and_focus_layered_overlay(
    gpui_hwnd: HWND,
    thumb_host_hwnd: HWND,
    policy: OverlayMonitorPolicy,
    immersive_fullscreen: bool,
) -> Result<(RECT, f32)> {
    let (x, y, win_w, win_h, scale) = compute_overlay_rect(policy, immersive_fullscreen)?;

    unsafe {
        // 1. Reveal bottom thumbnail/backdrop host without stealing focus
        let _ = SetWindowPos(
            thumb_host_hwnd,
            Some(HWND_TOPMOST),
            x,
            y,
            win_w,
            win_h,
            SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_FRAMECHANGED,
        );
        let _ = ShowWindow(thumb_host_hwnd, SW_SHOWNOACTIVATE);

        // 2. Reveal interactive GPUI overlay directly on top
        let _ = SetWindowPos(
            gpui_hwnd,
            Some(HWND_TOPMOST),
            x,
            y,
            win_w,
            win_h,
            SWP_SHOWWINDOW | SWP_FRAMECHANGED,
        );
        let _ = ShowWindow(gpui_hwnd, SW_SHOW);

        // 3. Pin thumb_host_hwnd strictly 1 Z-slot directly beneath gpui_hwnd
        let _ = SetWindowPos(
            thumb_host_hwnd,
            Some(gpui_hwnd),
            x,
            y,
            win_w,
            win_h,
            SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
        );
    }
    focus_hwnd(gpui_hwnd);

    Ok((
        RECT {
            left: x,
            top: y,
            right: x + win_w,
            bottom: y + win_h,
        },
        scale,
    ))
}

pub fn hide_layered_overlay(gpui_hwnd: HWND, thumb_host_hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(gpui_hwnd, SW_HIDE);
        let _ = ShowWindow(thumb_host_hwnd, SW_HIDE);
    }
}

pub fn show_and_focus_overlay(
    hwnd: HWND,
    policy: OverlayMonitorPolicy,
    immersive_fullscreen: bool,
) -> Result<(RECT, f32)> {
    let (x, y, win_w, win_h, scale) = compute_overlay_rect(policy, immersive_fullscreen)?;
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            x,
            y,
            win_w,
            win_h,
            SWP_SHOWWINDOW | SWP_FRAMECHANGED,
        );
        let _ = ShowWindow(hwnd, SW_SHOW);
    }
    focus_hwnd(hwnd);

    Ok((
        RECT {
            left: x,
            top: y,
            right: x + win_w,
            bottom: y + win_h,
        },
        scale,
    ))
}

pub fn query_window_and_client_sizes(hwnd: HWND) -> ((i32, i32), (i32, i32)) {
    let mut wr = RECT::default();
    let mut cr = RECT::default();
    let _ = unsafe { GetWindowRect(hwnd, &mut wr) };
    let _ = unsafe { GetClientRect(hwnd, &mut cr) };
    (
        (wr.right - wr.left, wr.bottom - wr.top),
        (cr.right - cr.left, cr.bottom - cr.top),
    )
}

pub fn hide_overlay(hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
}

pub fn is_overlay_visible(hwnd: HWND) -> bool {
    unsafe { IsWindowVisible(hwnd) }.as_bool()
}
