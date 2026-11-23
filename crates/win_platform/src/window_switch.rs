use std::ffi::c_void;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
    VK_MENU,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowPlacement, IsIconic, IsWindow, IsWindowVisible, IsZoomed,
    SW_HIDE, SW_RESTORE, SW_SHOWMAXIMIZED, SetForegroundWindow, ShowWindow, WINDOWPLACEMENT,
    WPF_RESTORETOMAXIMIZED,
};

use crate::virtual_desktop::{DesktopId, VirtualDesktopClient};

#[derive(Debug, Clone)]
pub struct SwitchReport {
    pub target_hwnd: isize,
    pub target_desktop_id: Option<DesktopId>,
    pub desktop_switch_issued: bool,
    pub was_minimized: bool,
    pub had_restore_to_maximized_flag: bool,
    pub used_alt_input_fallback: bool,
    pub self_hidden_after: Option<bool>,
    pub active_desktop_after: Option<DesktopId>,
    pub desktop_matched: bool,
    pub foreground_hwnd_after: isize,
    pub foreground_matched: bool,
    pub is_iconic_after: bool,
    pub is_zoomed_after: bool,
    pub elapsed_ms: f64,
}

fn send_synthetic_alt_tap() {
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
    unsafe {
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

pub fn switch_to_window(
    vdm: &VirtualDesktopClient,
    target_raw: isize,
    self_raw: Option<isize>,
) -> Result<SwitchReport> {
    let start = Instant::now();
    let target = HWND(target_raw as *mut c_void);
    if !unsafe { IsWindow(Some(target)) }.as_bool() {
        return Err(anyhow!("Target HWND 0x{:X} is no longer valid", target_raw));
    }

    // 1. Hide switcher window first so focus transfers cleanly.
    if let Some(s_raw) = self_raw {
        let self_hwnd = HWND(s_raw as *mut c_void);
        unsafe {
            let _ = ShowWindow(self_hwnd, SW_HIDE);
        }
    }

    // 2. Determine target desktop and switch if needed.
    let desktops_before = vdm.get_desktops().unwrap_or_default();
    let current_before = desktops_before.iter().find(|d| d.is_current).map(|d| d.id);
    let lookup_map = vdm.query_windows(&[target_raw])?;
    let lookup = lookup_map.get(&target_raw).cloned().unwrap_or_default();
    let target_desktop_id = lookup.effective_desktop_id();

    let mut desktop_switch_issued = false;
    if !lookup.is_pinned {
        if let Some(target_did) = target_desktop_id {
            if current_before != Some(target_did) {
                vdm.switch_to_desktop(target_did)?;
                desktop_switch_issued = true;
                // Wait briefly for Explorer/DWM to uncloak target desktop windows.
                let deadline = Instant::now() + Duration::from_millis(200);
                while Instant::now() < deadline {
                    if let Ok(list) = vdm.get_desktops() {
                        if list.iter().any(|d| d.is_current && d.id == target_did) {
                            break;
                        }
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }

    // 3. Inspect placement and restore minimized window preserving maximized state.
    let mut placement = WINDOWPLACEMENT {
        length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
        ..Default::default()
    };
    let _ = unsafe { GetWindowPlacement(target, &mut placement) };
    let was_minimized = unsafe { IsIconic(target) }.as_bool();
    let had_restore_to_maximized_flag = (placement.flags.0 & WPF_RESTORETOMAXIMIZED.0) != 0;

    if was_minimized {
        let cmd = if had_restore_to_maximized_flag {
            SW_SHOWMAXIMIZED
        } else {
            SW_RESTORE
        };
        unsafe {
            let _ = ShowWindow(target, cmd);
        }
    }

    // 4. Bring window to foreground.
    let mut fg_ok = unsafe { SetForegroundWindow(target) }.as_bool();
    let mut used_alt_input_fallback = false;
    if !fg_ok {
        send_synthetic_alt_tap();
        used_alt_input_fallback = true;
        fg_ok = unsafe { SetForegroundWindow(target) }.as_bool();
    }
    let _ = fg_ok;

    // Allow a brief window settle tick before asserting post-conditions.
    thread::sleep(Duration::from_millis(30));

    let self_hidden_after = self_raw.map(|s_raw| {
        let self_hwnd = HWND(s_raw as *mut c_void);
        !unsafe { IsWindowVisible(self_hwnd) }.as_bool()
    });
    let active_desktop_after = vdm
        .get_desktops()
        .ok()
        .and_then(|list| list.into_iter().find(|d| d.is_current).map(|d| d.id));
    let desktop_matched = match (lookup.is_pinned, target_desktop_id, active_desktop_after) {
        (true, _, _) => true,
        (false, Some(t), Some(a)) => t == a,
        (false, None, _) => true,
        _ => false,
    };

    let fg_after = unsafe { GetForegroundWindow() }.0 as isize;
    let is_iconic_after = unsafe { IsIconic(target) }.as_bool();
    let is_zoomed_after = unsafe { IsZoomed(target) }.as_bool();

    Ok(SwitchReport {
        target_hwnd: target_raw,
        target_desktop_id,
        desktop_switch_issued,
        was_minimized,
        had_restore_to_maximized_flag,
        used_alt_input_fallback,
        self_hidden_after,
        active_desktop_after,
        desktop_matched,
        foreground_hwnd_after: fg_after,
        foreground_matched: fg_after == target_raw,
        is_iconic_after,
        is_zoomed_after,
        elapsed_ms: start.elapsed().as_secs_f64() * 1000.0,
    })
}
