use std::collections::HashSet;
use std::ffi::c_void;
use std::path::Path;

use anyhow::Result;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{
    DWM_CLOAKED_APP, DWM_CLOAKED_INHERITED, DWM_CLOAKED_SHELL, DWMWA_CLOAKED,
    DwmGetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFOEXW,
    MonitorFromWindow,
};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Threading::{
    OpenProcess, OpenProcessToken, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GA_ROOTOWNER, GWL_EXSTYLE, GetAncestor, GetClassNameW, GetLastActivePopup,
    GetWindowLongW, GetWindowPlacement, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, IsZoomed, SW_SHOWMAXIMIZED,
    WINDOWPLACEMENT, WPF_RESTORETOMAXIMIZED, WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
};
use windows_core::{BOOL, PWSTR};

use crate::virtual_desktop::{DesktopId, VirtualDesktopClient, VirtualDesktopInfo};

#[derive(Debug, Clone)]
pub struct MonitorInfo {
    pub index: usize,
    pub handle: isize,
    pub device_name: String,
    pub is_primary: bool,
    pub bounds: RECT,
    pub work_area: RECT,
    pub dpi_x: u32,
    pub dpi_y: u32,
    pub scale_factor: f32,
}

#[derive(Debug, Clone)]
pub struct WindowInfo {
    pub hwnd: isize,
    pub pid: u32,
    pub process_name: String,
    pub exe_path: String,
    pub class_name: String,
    pub title: String,
    pub desktop_id: Option<DesktopId>,
    pub desktop_index: Option<usize>,
    pub is_pinned: bool,
    pub is_minimized: bool,
    pub is_maximized: bool,
    pub restore_to_maximized: bool,
    pub is_elevated: Option<bool>,
    pub cloaked_flags: u32,
    pub monitor_index: Option<usize>,
    pub rect: RECT,
}

struct OwnedHandle(HANDLE);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

pub fn enumerate_monitors() -> Vec<MonitorInfo> {
    let mut handles: Vec<HMONITOR> = Vec::new();
    unsafe extern "system" fn callback(
        hmon: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        lparam: LPARAM,
    ) -> BOOL {
        let vec = unsafe { &mut *(lparam.0 as *mut Vec<HMONITOR>) };
        vec.push(hmon);
        BOOL(1)
    }

    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(callback),
            LPARAM(&mut handles as *mut _ as isize),
        );
    }

    handles
        .into_iter()
        .enumerate()
        .map(|(index, hmon)| {
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
            let _ = unsafe { GetMonitorInfoW(hmon, &mut info.monitorInfo as *mut _) };

            let mut dpi_x = 96u32;
            let mut dpi_y = 96u32;
            let _ = unsafe { GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };

            let nul = info
                .szDevice
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(info.szDevice.len());
            let device_name = String::from_utf16_lossy(&info.szDevice[..nul]);

            MonitorInfo {
                index,
                handle: hmon.0 as isize,
                device_name,
                is_primary: (info.monitorInfo.dwFlags & 1) != 0,
                bounds: info.monitorInfo.rcMonitor,
                work_area: info.monitorInfo.rcWork,
                dpi_x,
                dpi_y,
                scale_factor: dpi_x as f32 / 96.0,
            }
        })
        .collect()
}

fn get_window_text(hwnd: HWND) -> String {
    let len = unsafe { GetWindowTextLengthW(hwnd) };
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; (len as usize) + 1];
    let copied = unsafe { GetWindowTextW(hwnd, &mut buf) };
    if copied <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..copied as usize])
}

fn get_class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let copied = unsafe { GetClassNameW(hwnd, &mut buf) };
    if copied <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..copied as usize])
}

fn get_cloaked_flags(hwnd: HWND) -> u32 {
    let mut cloaked: u32 = 0;
    let hr = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut c_void,
            std::mem::size_of::<u32>() as u32,
        )
    };
    if hr.is_ok() { cloaked } else { 0 }
}

fn query_process_info(pid: u32) -> (String, String, Option<bool>) {
    let Ok(proc_raw) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) })
    else {
        return (String::new(), String::new(), None);
    };
    let proc_handle = OwnedHandle(proc_raw);

    let mut path_buf = [0u16; 1024];
    let mut size = path_buf.len() as u32;
    let exe_path = if unsafe {
        QueryFullProcessImageNameW(
            proc_handle.0,
            PROCESS_NAME_WIN32,
            PWSTR(path_buf.as_mut_ptr()),
            &mut size,
        )
    }
    .is_ok()
    {
        String::from_utf16_lossy(&path_buf[..size as usize])
    } else {
        String::new()
    };

    let process_name = Path::new(&exe_path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut token_raw = HANDLE::default();
    let is_elevated =
        if unsafe { OpenProcessToken(proc_handle.0, TOKEN_QUERY, &mut token_raw) }.is_ok() {
            let token = OwnedHandle(token_raw);
            let mut elevation = TOKEN_ELEVATION::default();
            let mut ret_len = 0u32;
            if unsafe {
                GetTokenInformation(
                    token.0,
                    TokenElevation,
                    Some(&mut elevation as *mut _ as *mut c_void),
                    std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                    &mut ret_len,
                )
            }
            .is_ok()
            {
                Some(elevation.TokenIsElevated != 0)
            } else {
                None
            }
        } else {
            None
        };

    (process_name, exe_path, is_elevated)
}

/// Raymond Chen's Alt-Tab root owner / last active popup walk.
fn passes_alt_tab_owner_walk(hwnd: HWND) -> bool {
    let mut hwnd_walk = unsafe { GetAncestor(hwnd, GA_ROOTOWNER) };
    if hwnd_walk.is_invalid() {
        return false;
    }
    loop {
        let hwnd_try = unsafe { GetLastActivePopup(hwnd_walk) };
        if hwnd_try == hwnd_walk {
            break;
        }
        if unsafe { IsWindowVisible(hwnd_try) }.as_bool() {
            hwnd_walk = hwnd_try;
            break;
        }
        hwnd_walk = hwnd_try;
    }
    hwnd_walk == hwnd
}

fn is_excluded_shell_class(class_name: &str) -> bool {
    matches!(
        class_name,
        "Progman"
            | "WorkerW"
            | "Shell_TrayWnd"
            | "Shell_SecondaryTrayWnd"
            | "Windows.UI.Core.CoreWindow"
            | "ApplicationManager_ImmersiveShellWindow"
            | "EdgeUiInputTopWndClass"
            | "NativeHWNDHost"
            | "MultitaskingViewFrame"
            | "ForegroundStaging"
            | "Xaml_WindowedPopupClass"
    )
}

pub fn enumerate_windows(
    vdm: &VirtualDesktopClient,
    desktops: &[VirtualDesktopInfo],
    monitors: &[MonitorInfo],
    exclude_hwnds: &[isize],
) -> Result<Vec<WindowInfo>> {
    let mut raw_hwnds: Vec<HWND> = Vec::new();
    unsafe extern "system" fn enum_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let list = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
        list.push(hwnd);
        BOOL(1)
    }
    unsafe {
        let _ = EnumWindows(Some(enum_cb), LPARAM(&mut raw_hwnds as *mut _ as isize));
    }

    // First-pass lightweight pre-filter: only visible top-level windows (other desktops are still IsWindowVisible == true, just DWM-cloaked).
    let visible_hwnds: Vec<HWND> = raw_hwnds
        .into_iter()
        .filter(|&h| unsafe { IsWindowVisible(h) }.as_bool())
        .collect();

    let visible_raw: Vec<isize> = visible_hwnds.iter().map(|h| h.0 as isize).collect();
    let desktop_lookups = vdm.query_windows(&visible_raw)?;

    let valid_desktop_ids: HashSet<DesktopId> = desktops.iter().map(|d| d.id).collect();
    let current_desktop_id: Option<DesktopId> =
        desktops.iter().find(|d| d.is_current).map(|d| d.id);

    let mut windows_out = Vec::new();

    for hwnd in visible_hwnds {
        let raw_id = hwnd.0 as isize;
        if exclude_hwnds.contains(&raw_id) {
            continue;
        }

        let ex_style = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) } as u32;
        if (ex_style & WS_EX_TOOLWINDOW.0) != 0 {
            continue;
        }
        if (ex_style & WS_EX_NOACTIVATE.0) != 0 && (ex_style & WS_EX_APPWINDOW.0) == 0 {
            continue;
        }

        let class_name = get_class_name(hwnd);
        if is_excluded_shell_class(&class_name) {
            continue;
        }

        let title = get_window_text(hwnd);
        if title.trim().is_empty() {
            continue;
        }

        if (ex_style & WS_EX_APPWINDOW.0) == 0 && !passes_alt_tab_owner_walk(hwnd) {
            continue;
        }

        let cloaked = get_cloaked_flags(hwnd);
        if (cloaked & (DWM_CLOAKED_APP | DWM_CLOAKED_INHERITED)) != 0 {
            continue;
        }

        let lookup = desktop_lookups.get(&raw_id).cloned().unwrap_or_default();
        if (cloaked & DWM_CLOAKED_SHELL) != 0 {
            // Shell-cloaked windows are EITHER legitimate windows on another virtual desktop
            // OR suspended UWP / hidden shell frames (which have no valid desktop or sit on current desktop while cloaked).
            match lookup.effective_desktop_id() {
                Some(did) if valid_desktop_ids.contains(&did) => {
                    if current_desktop_id == Some(did) && !lookup.is_pinned {
                        continue;
                    }
                }
                _ => continue,
            }
        }

        // Check zero-sized non-minimized ghost windows
        let is_iconic = unsafe { IsIconic(hwnd) }.as_bool();
        let mut rect = RECT::default();
        let _ = unsafe { GetWindowRect(hwnd, &mut rect) };
        let w = rect.right - rect.left;
        let h = rect.bottom - rect.top;
        if !is_iconic && (w <= 0 || h <= 0) {
            continue;
        }

        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        let (process_name, exe_path, is_elevated) = query_process_info(pid);

        let mut placement = WINDOWPLACEMENT {
            length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
            ..Default::default()
        };
        let _ = unsafe { GetWindowPlacement(hwnd, &mut placement) };
        let is_maximized = unsafe { IsZoomed(hwnd) }.as_bool()
            || placement.showCmd == SW_SHOWMAXIMIZED.0 as u32;
        let restore_to_maximized = (placement.flags.0 & WPF_RESTORETOMAXIMIZED.0) != 0;

        let hmon = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
        let monitor_index = monitors.iter().position(|m| m.handle == hmon.0 as isize);

        // Determine desktop assignment
        let desktop_id = if lookup.is_pinned {
            current_desktop_id.or_else(|| lookup.effective_desktop_id())
        } else {
            lookup.effective_desktop_id().or_else(|| {
                if cloaked == 0 {
                    current_desktop_id
                } else {
                    None
                }
            })
        };
        let desktop_index = desktop_id
            .and_then(|did| desktops.iter().position(|d| d.id == did));

        windows_out.push(WindowInfo {
            hwnd: raw_id,
            pid,
            process_name,
            exe_path,
            class_name,
            title,
            desktop_id,
            desktop_index,
            is_pinned: lookup.is_pinned,
            is_minimized: is_iconic,
            is_maximized,
            restore_to_maximized,
            is_elevated,
            cloaked_flags: cloaked,
            monitor_index,
            rect,
        });
    }

    Ok(windows_out)
}
