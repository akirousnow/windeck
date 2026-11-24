use std::ffi::c_void;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, GetLastError, HWND, LPARAM, WPARAM,
};
use windows::Win32::Graphics::Dwm::{DWMWA_TRANSITIONS_FORCEDISABLED, DwmSetWindowAttribute};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_TERMINATE, QueryFullProcessImageNameW, TerminateProcess,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_KEYUP, SendInput, VK_MENU,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, GetForegroundWindow, GetWindowPlacement, GetWindowThreadProcessId,
    IsHungAppWindow, IsIconic, IsWindow, IsWindowVisible, IsZoomed, PostMessageW, SW_HIDE,
    SW_RESTORE, SW_SHOWMAXIMIZED, SW_SHOWNOACTIVATE, SetForegroundWindow, ShowWindow,
    WINDOWPLACEMENT, WM_CLOSE, WPF_RESTORETOMAXIMIZED,
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

#[derive(Debug, Clone, Copy)]
pub struct ForegroundActivationResult {
    pub was_minimized: bool,
    pub had_restore_to_maximized_flag: bool,
    pub used_alt_input_fallback: bool,
    pub foreground_hwnd_after: isize,
    pub foreground_matched: bool,
    pub is_iconic_after: bool,
    pub is_zoomed_after: bool,
}

/// RAII guard that temporarily disables DWM window transitions (`DWMWA_TRANSITIONS_FORCEDISABLED`)
/// while WinDeck pre-restores a minimized window behind its topmost overlay curtain, and
/// unconditionally re-enables transitions when dropped.
#[derive(Debug)]
pub struct DwmTransitionSuppressGuard {
    target_raw: isize,
}

impl DwmTransitionSuppressGuard {
    pub fn acquire(target_raw: isize) -> Option<Self> {
        if target_raw == 0 {
            return None;
        }
        let target = HWND(target_raw as *mut c_void);
        if !unsafe { IsWindow(Some(target)) }.as_bool() {
            return None;
        }
        let disable = BOOL(1);
        let hr = unsafe {
            DwmSetWindowAttribute(
                target,
                DWMWA_TRANSITIONS_FORCEDISABLED,
                &disable as *const BOOL as *const c_void,
                std::mem::size_of::<BOOL>() as u32,
            )
        };
        if hr.is_ok() {
            Some(Self { target_raw })
        } else {
            None
        }
    }
}

impl Drop for DwmTransitionSuppressGuard {
    fn drop(&mut self) {
        if self.target_raw == 0 {
            return;
        }
        let target = HWND(self.target_raw as *mut c_void);
        if unsafe { IsWindow(Some(target)) }.as_bool() {
            let enable = BOOL(0);
            let _ = unsafe {
                DwmSetWindowAttribute(
                    target,
                    DWMWA_TRANSITIONS_FORCEDISABLED,
                    &enable as *const BOOL as *const c_void,
                    std::mem::size_of::<BOOL>() as u32,
                )
            };
        }
    }
}

#[derive(Debug)]
pub struct PreRestoreResult {
    pub was_minimized: bool,
    pub had_restore_to_maximized_flag: bool,
    pub restored: bool,
    pub is_zoomed_after: bool,
    pub dwm_guard: Option<DwmTransitionSuppressGuard>,
}

/// Pre-restores a minimized window behind WinDeck's topmost overlay curtain so a fly-out zoom
/// animation lands directly on the live restored window with zero double-restore animation.
///
/// - Skips hung windows (`IsHungAppWindow`) so the UI thread never blocks.
/// - Temporarily suppresses Windows' DWM restore transition via [`DwmTransitionSuppressGuard`],
///   which the caller must hold alive until the overlay finishes hiding.
/// - Restores minimized windows via `SW_SHOWNOACTIVATE` first so WinDeck keeps foreground focus
///   while the overlay animates; if a `WPF_RESTORETOMAXIMIZED` window does not stay zoomed under
///   `SW_SHOWNOACTIVATE`, falls back to `SW_SHOWMAXIMIZED` (caller should re-pin overlay Z-order right after).
pub fn pre_restore_minimized_window(target_raw: isize) -> PreRestoreResult {
    let target = HWND(target_raw as *mut c_void);
    if target_raw == 0 || !unsafe { IsWindow(Some(target)) }.as_bool() {
        return PreRestoreResult {
            was_minimized: false,
            had_restore_to_maximized_flag: false,
            restored: false,
            is_zoomed_after: false,
            dwm_guard: None,
        };
    }

    let was_minimized = unsafe { IsIconic(target) }.as_bool();
    if !was_minimized {
        return PreRestoreResult {
            was_minimized: false,
            had_restore_to_maximized_flag: false,
            restored: false,
            is_zoomed_after: unsafe { IsZoomed(target) }.as_bool(),
            dwm_guard: None,
        };
    }

    if unsafe { IsHungAppWindow(target) }.as_bool() {
        return PreRestoreResult {
            was_minimized: true,
            had_restore_to_maximized_flag: false,
            restored: false,
            is_zoomed_after: false,
            dwm_guard: None,
        };
    }

    let mut placement = WINDOWPLACEMENT {
        length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
        ..Default::default()
    };
    let _ = unsafe { GetWindowPlacement(target, &mut placement) };
    let had_restore_to_maximized_flag = (placement.flags.0 & WPF_RESTORETOMAXIMIZED.0) != 0;

    let dwm_guard = DwmTransitionSuppressGuard::acquire(target_raw);

    unsafe {
        let _ = ShowWindow(target, SW_SHOWNOACTIVATE);
        if had_restore_to_maximized_flag && !IsZoomed(target).as_bool() {
            let _ = ShowWindow(target, SW_SHOWMAXIMIZED);
        }
    }

    let restored = !unsafe { IsIconic(target) }.as_bool();
    let is_zoomed_after = unsafe { IsZoomed(target) }.as_bool();
    let dwm_guard = if restored { dwm_guard } else { None };

    PreRestoreResult {
        was_minimized,
        had_restore_to_maximized_flag,
        restored,
        is_zoomed_after,
        dwm_guard,
    }
}

fn send_synthetic_alt_tap() {
    // Never inject a synthetic VK_MENU KEYUP while the user is physically holding Alt.
    if (unsafe { GetAsyncKeyState(VK_MENU.0 as i32) } as u16 & 0x8000) != 0 {
        return;
    }
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

/// Switches to `target_did` and polls until Explorer/DWM marks it current (up to `timeout`).
/// Intended to run off the UI thread while an exit transition animates on top.
pub fn switch_desktop_and_wait(
    vdm: &VirtualDesktopClient,
    target_did: DesktopId,
    timeout: Duration,
) -> Result<bool> {
    vdm.switch_to_desktop(target_did)?;
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(list) = vdm.get_desktops() {
            if list.iter().any(|d| d.is_current && d.id == target_did) {
                return Ok(true);
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
    let matched = vdm
        .get_desktops()
        .ok()
        .is_some_and(|list| list.iter().any(|d| d.is_current && d.id == target_did));
    Ok(matched)
}

/// Non-blocking foreground activation (<1ms, zero sleep): restores minimized windows preserving
/// `WPF_RESTORETOMAXIMIZED` and brings `target_raw` to foreground with safe Alt-tap fallback.
pub fn activate_window_foreground(target_raw: isize) -> Result<ForegroundActivationResult> {
    let target = HWND(target_raw as *mut c_void);
    if target_raw == 0 || !unsafe { IsWindow(Some(target)) }.as_bool() {
        return Err(anyhow!("Target HWND 0x{:X} is no longer valid", target_raw));
    }

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

    let mut used_alt_input_fallback = false;
    if unsafe { GetForegroundWindow() } != target {
        let fg_ok = unsafe { SetForegroundWindow(target) }.as_bool();
        if !fg_ok && unsafe { GetForegroundWindow() } != target {
            send_synthetic_alt_tap();
            used_alt_input_fallback = true;
            let _ = unsafe { SetForegroundWindow(target) };
        }
    }

    let fg_after = unsafe { GetForegroundWindow() }.0 as isize;
    let is_iconic_after = unsafe { IsIconic(target) }.as_bool();
    let is_zoomed_after = unsafe { IsZoomed(target) }.as_bool();

    Ok(ForegroundActivationResult {
        was_minimized,
        had_restore_to_maximized_flag,
        used_alt_input_fallback,
        foreground_hwnd_after: fg_after,
        foreground_matched: fg_after == target_raw,
        is_iconic_after,
        is_zoomed_after,
    })
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
                let _ = switch_desktop_and_wait(vdm, target_did, Duration::from_millis(200))?;
                desktop_switch_issued = true;
            }
        }
    }

    // 3 & 4. Restore minimized window (if needed) and bring window to foreground.
    let fg_res = activate_window_foreground(target_raw)?;

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
        was_minimized: fg_res.was_minimized,
        had_restore_to_maximized_flag: fg_res.had_restore_to_maximized_flag,
        used_alt_input_fallback: fg_res.used_alt_input_fallback,
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
    use std::sync::Once;

    use windows::Win32::Foundation::{LRESULT, RECT};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, SW_MINIMIZE,
        SW_SHOWNORMAL, WINDOW_EX_STYLE, WNDCLASSW, WS_OVERLAPPEDWINDOW, WS_POPUP,
    };
    use windows_core::w;

    use crate::overlay_win::{
        OverlayMonitorPolicy, ThumbnailHostWindow, configure_borderless_immersive,
        hide_layered_overlay, inspect_layered_overlay_state, pin_layered_z_order,
        show_and_focus_layered_overlay,
    };
    use crate::window_enum::{WindowInfo, enumerate_monitors, refresh_window_geometry};

    use super::*;

    struct TestWin(HWND);

    impl TestWin {
        fn create_overlapped(x: i32, y: i32, w: i32, h: i32) -> Self {
            static REG: Once = Once::new();
            let cls = w!("WinDeckSwitchTestOverlapped");
            let hinst = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
            unsafe extern "system" fn proc(
                hwnd: HWND,
                msg: u32,
                wp: WPARAM,
                lp: LPARAM,
            ) -> LRESULT {
                unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
            }
            REG.call_once(|| {
                let wc = WNDCLASSW {
                    lpfnWndProc: Some(proc),
                    hInstance: hinst.into(),
                    lpszClassName: cls,
                    ..Default::default()
                };
                unsafe { RegisterClassW(&wc) };
            });
            let hwnd = unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    cls,
                    w!("WinDeckSwitchTestTarget"),
                    WS_OVERLAPPEDWINDOW,
                    x,
                    y,
                    w,
                    h,
                    None,
                    None,
                    Some(hinst.into()),
                    None,
                )
            }
            .expect("CreateWindowExW overlapped");
            Self(hwnd)
        }

        fn create_popup(w: i32, h: i32) -> Self {
            static REG: Once = Once::new();
            let cls = w!("WinDeckSwitchTestPopup");
            let hinst = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
            unsafe extern "system" fn proc(
                hwnd: HWND,
                msg: u32,
                wp: WPARAM,
                lp: LPARAM,
            ) -> LRESULT {
                unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
            }
            REG.call_once(|| {
                let wc = WNDCLASSW {
                    lpfnWndProc: Some(proc),
                    hInstance: hinst.into(),
                    lpszClassName: cls,
                    ..Default::default()
                };
                unsafe { RegisterClassW(&wc) };
            });
            let hwnd = unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    cls,
                    w!("WinDeckSwitchTestOverlay"),
                    WS_POPUP,
                    0,
                    0,
                    w,
                    h,
                    None,
                    None,
                    Some(hinst.into()),
                    None,
                )
            }
            .expect("CreateWindowExW popup");
            Self(hwnd)
        }
    }

    impl Drop for TestWin {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                let _ = unsafe { DestroyWindow(self.0) };
            }
        }
    }

    fn blank_window_info(hwnd: HWND) -> WindowInfo {
        WindowInfo {
            hwnd: hwnd.0 as isize,
            pid: 0,
            process_name: String::new(),
            exe_path: String::new(),
            class_name: String::new(),
            title: String::new(),
            desktop_id: None,
            desktop_index: None,
            is_pinned: false,
            is_minimized: false,
            is_maximized: false,
            restore_to_maximized: false,
            is_elevated: None,
            cloaked_flags: 0,
            monitor_index: None,
            rect: RECT::default(),
            visible_rect: RECT::default(),
            normal_rect: RECT::default(),
        }
    }

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

    #[test]
    fn activate_window_foreground_rejects_invalid_hwnd() {
        assert!(activate_window_foreground(0).is_err());
    }

    #[test]
    fn pre_restore_rejects_invalid_and_skips_non_minimized() {
        let invalid = pre_restore_minimized_window(0);
        assert!(!invalid.was_minimized);
        assert!(!invalid.restored);
        assert!(invalid.dwm_guard.is_none());

        let win = TestWin::create_overlapped(120, 120, 640, 420);
        unsafe {
            let _ = ShowWindow(win.0, SW_SHOWNOACTIVATE);
        }
        let non_min = pre_restore_minimized_window(win.0.0 as isize);
        assert!(!non_min.was_minimized);
        assert!(!non_min.restored);
        assert!(non_min.dwm_guard.is_none());
    }

    #[test]
    fn pre_restore_normal_and_maximized_minimized_windows_behind_overlay() {
        let monitors = enumerate_monitors();
        assert!(!monitors.is_empty());

        let overlay_gpui = TestWin::create_popup(800, 600);
        configure_borderless_immersive(overlay_gpui.0, false);
        let thumb_host = ThumbnailHostWindow::create(false).expect("ThumbnailHostWindow::create");
        let (overlay_rect, _) = show_and_focus_layered_overlay(
            overlay_gpui.0,
            thumb_host.hwnd(),
            OverlayMonitorPolicy::Primary,
            false,
        )
        .expect("show_and_focus_layered_overlay");

        // 1. Normal window minimized -> pre-restore should unminimize without stealing focus
        let normal = TestWin::create_overlapped(120, 100, 720, 480);
        unsafe {
            let _ = ShowWindow(normal.0, SW_SHOWNORMAL);
            let _ = ShowWindow(normal.0, SW_MINIMIZE);
        }
        assert!(unsafe { IsIconic(normal.0) }.as_bool());

        let mut info = blank_window_info(normal.0);
        assert!(refresh_window_geometry(&mut info, &monitors));
        assert!(info.is_minimized);
        assert!(!info.restore_to_maximized);
        assert!(info.normal_rect.right - info.normal_rect.left >= 600);
        assert!(info.monitor_index.is_some());

        pin_layered_z_order(overlay_gpui.0, thumb_host.hwnd(), overlay_rect);
        let res = pre_restore_minimized_window(normal.0.0 as isize);
        pin_layered_z_order(overlay_gpui.0, thumb_host.hwnd(), overlay_rect);

        assert!(res.was_minimized);
        assert!(!res.had_restore_to_maximized_flag);
        assert!(res.restored);
        assert!(!res.is_zoomed_after);
        assert!(res.dwm_guard.is_some());
        assert!(!unsafe { IsIconic(normal.0) }.as_bool());

        assert!(refresh_window_geometry(&mut info, &monitors));
        assert!(!info.is_minimized);
        assert!(info.visible_rect.right - info.visible_rect.left >= 600);
        assert!(info.visible_rect.left > -10000);
        let diag = inspect_layered_overlay_state(overlay_gpui.0, thumb_host.hwnd());
        assert!(
            diag.is_healthy(),
            "overlay must stay topmost above pre-restored normal window: {diag:?}"
        );
        drop(res.dwm_guard);

        // 2. Maximized-then-minimized window -> pre-restore must preserve maximized state
        let maxed = TestWin::create_overlapped(140, 120, 700, 460);
        unsafe {
            let _ = ShowWindow(maxed.0, SW_SHOWMAXIMIZED);
            let _ = ShowWindow(maxed.0, SW_MINIMIZE);
        }
        assert!(unsafe { IsIconic(maxed.0) }.as_bool());

        let mut max_info = blank_window_info(maxed.0);
        assert!(refresh_window_geometry(&mut max_info, &monitors));
        assert!(max_info.is_minimized);
        assert!(max_info.restore_to_maximized);

        pin_layered_z_order(overlay_gpui.0, thumb_host.hwnd(), overlay_rect);
        let max_res = pre_restore_minimized_window(maxed.0.0 as isize);
        pin_layered_z_order(overlay_gpui.0, thumb_host.hwnd(), overlay_rect);

        assert!(max_res.was_minimized);
        assert!(max_res.had_restore_to_maximized_flag);
        assert!(max_res.restored);
        assert!(max_res.is_zoomed_after, "maximized-then-minimized window must restore maximized");
        assert!(max_res.dwm_guard.is_some());
        assert!(!unsafe { IsIconic(maxed.0) }.as_bool());
        assert!(unsafe { IsZoomed(maxed.0) }.as_bool());

        let diag2 = inspect_layered_overlay_state(overlay_gpui.0, thumb_host.hwnd());
        assert!(
            diag2.is_healthy(),
            "overlay must stay topmost above pre-restored maximized window: {diag2:?}"
        );

        drop(max_res.dwm_guard);
        hide_layered_overlay(overlay_gpui.0, thumb_host.hwnd());
    }
}
