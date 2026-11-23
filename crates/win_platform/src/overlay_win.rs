use std::ffi::c_void;

use anyhow::{Context, Result};
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Dwm::{
    DWM_WINDOW_CORNER_PREFERENCE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTOPRIMARY, MONITOR_DEFAULTTONEAREST, MONITORINFO,
    MonitorFromPoint,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
    VK_MENU,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, HWND_TOPMOST, IsWindowVisible, SW_HIDE, SW_SHOW, SWP_SHOWWINDOW,
    SetForegroundWindow, SetWindowPos, ShowWindow,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayMonitorPolicy {
    Cursor,
    Primary,
}

/// Apply Win11 native rounded corners to the overlay HWND.
pub fn apply_win11_rounded_corners(hwnd: HWND) {
    let pref: DWM_WINDOW_CORNER_PREFERENCE = DWMWCP_ROUND;
    let _ = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &pref as *const _ as *const c_void,
            std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        )
    };
}

/// Returns `(work_area_physical_rect, scale_factor)` for the chosen monitor.
pub fn target_monitor_work_area(policy: OverlayMonitorPolicy) -> Result<(RECT, f32)> {
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

    Ok((info.rcWork, scale))
}

/// Position the overlay centered on the target monitor's work area, show it topmost, and focus it.
pub fn show_and_focus_overlay(
    hwnd: HWND,
    policy: OverlayMonitorPolicy,
    width_ratio: f32,
    height_ratio: f32,
) -> Result<(RECT, f32)> {
    let (work, scale) = target_monitor_work_area(policy)?;
    let work_w = (work.right - work.left).max(640);
    let work_h = (work.bottom - work.top).max(480);

    let win_w = ((work_w as f32) * width_ratio).round() as i32;
    let win_h = ((work_h as f32) * height_ratio).round() as i32;
    let win_w = win_w.clamp(760.min(work_w), work_w - 24);
    let win_h = win_h.clamp(500.min(work_h), work_h - 24);

    let x = work.left + (work_w - win_w) / 2;
    let y = work.top + (work_h - win_h) / 2;

    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            x,
            y,
            win_w,
            win_h,
            SWP_SHOWWINDOW,
        );
        let _ = ShowWindow(hwnd, SW_SHOW);
        if !SetForegroundWindow(hwnd).as_bool() {
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
            let _ = SetForegroundWindow(hwnd);
        }
    }

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

pub fn hide_overlay(hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
}

pub fn is_overlay_visible(hwnd: HWND) -> bool {
    unsafe { IsWindowVisible(hwnd) }.as_bool()
}
