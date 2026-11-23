use std::ffi::c_void;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, GetLastError, HWND, LPARAM, WPARAM,
};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_TERMINATE, QueryFullProcessImageNameW, TerminateProcess,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
    VK_MENU,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, GetForegroundWindow, GetWindowPlacement, GetWindowThreadProcessId,
    IsHungAppWindow, IsIconic, IsWindow, IsWindowVisible, IsZoomed, PostMessageW, SW_HIDE,
    SW_RESTORE, SW_SHOWMAXIMIZED, SetForegroundWindow, ShowWindow, WINDOWPLACEMENT, WM_CLOSE,
    WPF_RESTORETOMAXIMIZED,
};
use windows_core::{BOOL, PWSTR};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForceQuitOutcome {
    /// Target process was forcefully terminated via `TerminateProcess`.
    ProcessTerminated { pid: u32 },
    /// Window closed cleanly via `WM_CLOSE` (e.g. File Explorer shell window or responsive multi-window app).
    WindowClosedSafely,
}

#[derive(Debug, Clone)]
pub struct ForceQuitReport {
    pub target_hwnd: isize,
    pub pid: u32,
    pub process_name: String,
    pub outcome: ForceQuitOutcome,
}

fn process_name_for_pid(pid: u32) -> String {
    if pid == 0 {
        return String::new();
    }
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return String::new();
        };
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(handle);
        if ok && len > 0 {
            let full = String::from_utf16_lossy(&buf[..len as usize]);
            Path::new(&full)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or(full)
        } else {
            String::new()
        }
    }
}

fn is_protected_shell_process(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "explorer.exe"
            | "dwm.exe"
            | "csrss.exe"
            | "winlogon.exe"
            | "sihost.exe"
            | "shellexperiencehost.exe"
            | "startmenuexperiencehost.exe"
            | "searchhost.exe"
            | "taskmgr.exe"
    )
}

/// For UWP windows hosted by `ApplicationFrameHost.exe`, finds the real inner app process PID
/// from its `CoreWindow` child so we never kill `ApplicationFrameHost.exe` itself.
fn resolve_uwp_child_pid(frame_hwnd: HWND, host_pid: u32) -> Option<u32> {
    struct SearchCtx {
        host_pid: u32,
        found_pid: Option<u32>,
    }

    unsafe extern "system" fn enum_child(child: HWND, lparam: LPARAM) -> BOOL {
        let ctx = unsafe { &mut *(lparam.0 as *mut SearchCtx) };
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(child, Some(&mut pid)) };
        if pid != 0 && pid != ctx.host_pid {
            ctx.found_pid = Some(pid);
            return BOOL(0);
        }
        BOOL(1)
    }

    let mut ctx = SearchCtx {
        host_pid,
        found_pid: None,
    };
    unsafe {
        let _ = EnumChildWindows(
            Some(frame_hwnd),
            Some(enum_child),
            LPARAM(&mut ctx as *mut SearchCtx as isize),
        );
    }
    ctx.found_pid
}

fn terminate_pid(pid: u32) -> Result<()> {
    if pid == 0 || pid == unsafe { GetCurrentProcessId() } {
        return Err(anyhow!("拒绝终止当前或无效进程"));
    }
    unsafe {
        let handle = match OpenProcess(PROCESS_TERMINATE, false, pid) {
            Ok(h) => h,
            Err(e) => {
                if GetLastError() == ERROR_ACCESS_DENIED {
                    return Err(anyhow!("权限不足（该窗口为管理员权限运行）"));
                }
                return Err(anyhow!("无法打开目标进程 PID {pid}：{e}"));
            }
        };
        let res = TerminateProcess(handle, 1);
        let err = if res.is_err() {
            Some(GetLastError())
        } else {
            None
        };
        let _ = CloseHandle(handle);
        if let Some(code) = err {
            if code == ERROR_ACCESS_DENIED {
                return Err(anyhow!("权限不足（该窗口为管理员权限运行）"));
            }
            return Err(anyhow!("TerminateProcess(PID {pid}) 失败：{code:?}"));
        }
    }
    Ok(())
}

fn wait_until_window_closed(hwnd: HWND, max_wait: Duration) -> bool {
    let deadline = Instant::now() + max_wait;
    while Instant::now() < deadline {
        if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            return true;
        }
        thread::sleep(Duration::from_millis(15));
    }
    !unsafe { IsWindow(Some(hwnd)) }.as_bool()
}

/// Force-quits a targeted window safely:
/// - Protected shell hosts (e.g. `explorer.exe` File Explorer): sends `WM_CLOSE` only (never kills taskbar/shell).
/// - UWP apps (`ApplicationFrameHost.exe`): resolves inner app child PID and terminates ONLY that UWP app.
/// - Single-window or hung apps: calls `TerminateProcess` immediately (zero modal dialogs, kills frozen apps).
/// - Healthy multi-window processes (`same_pid_window_count > 1`): attempts `WM_CLOSE` for 120ms so sibling
///   windows survive if responsive; escalates directly to `TerminateProcess` if the window refuses or hangs.
pub fn force_quit_window(target_raw: isize, same_pid_window_count: usize) -> Result<ForceQuitReport> {
    let hwnd = HWND(target_raw as *mut c_void);
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return Err(anyhow!("目标窗口已关闭"));
    }

    let mut outer_pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut outer_pid)) };
    if outer_pid == 0 {
        return Err(anyhow!("无法获取目标窗口进程 ID"));
    }
    if outer_pid == unsafe { GetCurrentProcessId() } {
        return Err(anyhow!("不能强制退出 WinDeck 自身"));
    }

    let outer_name = process_name_for_pid(outer_pid);

    // 1. Never terminate critical Windows Shell processes (e.g. explorer.exe); close the window cleanly instead.
    if is_protected_shell_process(&outer_name) {
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        wait_until_window_closed(hwnd, Duration::from_millis(160));
        return Ok(ForceQuitReport {
            target_hwnd: target_raw,
            pid: outer_pid,
            process_name: outer_name,
            outcome: ForceQuitOutcome::WindowClosedSafely,
        });
    }

    // 2. UWP apps hosted inside ApplicationFrameHost.exe -> resolve inner app PID so we don't kill other UWP apps.
    if outer_name.eq_ignore_ascii_case("ApplicationFrameHost.exe") {
        if let Some(real_pid) = resolve_uwp_child_pid(hwnd, outer_pid) {
            let real_name = process_name_for_pid(real_pid);
            terminate_pid(real_pid)?;
            wait_until_window_closed(hwnd, Duration::from_millis(140));
            return Ok(ForceQuitReport {
                target_hwnd: target_raw,
                pid: real_pid,
                process_name: if real_name.is_empty() {
                    outer_name
                } else {
                    real_name
                },
                outcome: ForceQuitOutcome::ProcessTerminated { pid: real_pid },
            });
        }
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        wait_until_window_closed(hwnd, Duration::from_millis(160));
        return Ok(ForceQuitReport {
            target_hwnd: target_raw,
            pid: outer_pid,
            process_name: outer_name,
            outcome: ForceQuitOutcome::WindowClosedSafely,
        });
    }

    // 3. Multi-window responsive process: give the single window 120ms to close cleanly before escalating to kill.
    let is_hung = unsafe { IsHungAppWindow(hwnd) }.as_bool();
    if same_pid_window_count > 1 && !is_hung {
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        if wait_until_window_closed(hwnd, Duration::from_millis(120)) {
            return Ok(ForceQuitReport {
                target_hwnd: target_raw,
                pid: outer_pid,
                process_name: outer_name,
                outcome: ForceQuitOutcome::WindowClosedSafely,
            });
        }
    }

    // 4. Single-window, hung, or unresponsive window -> immediate TerminateProcess.
    terminate_pid(outer_pid)?;
    wait_until_window_closed(hwnd, Duration::from_millis(140));
    Ok(ForceQuitReport {
        target_hwnd: target_raw,
        pid: outer_pid,
        process_name: outer_name,
        outcome: ForceQuitOutcome::ProcessTerminated { pid: outer_pid },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_shell_processes_are_guarded() {
        assert!(is_protected_shell_process("explorer.exe"));
        assert!(is_protected_shell_process("EXPLORER.EXE"));
        assert!(is_protected_shell_process("dwm.exe"));
        assert!(!is_protected_shell_process("notepad.exe"));
        assert!(!is_protected_shell_process("chrome.exe"));
    }

    #[test]
    fn force_quit_rejects_invalid_hwnd_and_self_pid() {
        assert!(force_quit_window(0, 1).is_err());
        assert!(terminate_pid(unsafe { GetCurrentProcessId() }).is_err());
    }
}
