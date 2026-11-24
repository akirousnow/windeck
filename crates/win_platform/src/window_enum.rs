use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use anyhow::Result;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{
    DWM_CLOAKED_APP, DWM_CLOAKED_INHERITED, DWM_CLOAKED_SHELL, DWMWA_CLOAKED,
    DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFOEXW,
    MonitorFromRect, MonitorFromWindow,
};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::Storage::FileSystem::{
    FILE_VER_GET_LOCALISED, GetFileVersionInfoExW, GetFileVersionInfoSizeExW,
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
};
use windows::Win32::System::Threading::{
    OpenProcess, OpenProcessToken, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GA_ROOTOWNER, GWL_EXSTYLE, GetAncestor, GetClassNameW, GetLastActivePopup,
    GetWindowLongW, GetWindowPlacement, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, IsZoomed, SW_SHOWMAXIMIZED,
    WINDOWPLACEMENT, WPF_RESTORETOMAXIMIZED, WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
};
use windows_core::{BOOL, HSTRING, PCWSTR, PWSTR};

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
    pub app_name: String,
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
    pub visible_rect: RECT,
    pub normal_rect: RECT,
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

fn get_extended_frame_bounds(hwnd: HWND) -> Option<RECT> {
    let mut ext = RECT::default();
    let hr = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut ext as *mut RECT as *mut c_void,
            std::mem::size_of::<RECT>() as u32,
        )
    };
    if hr.is_ok() && ext.right > ext.left && ext.bottom > ext.top {
        Some(ext)
    } else {
        None
    }
}

struct WindowGeometryFields {
    is_minimized: bool,
    is_maximized: bool,
    restore_to_maximized: bool,
    monitor_index: Option<usize>,
    rect: RECT,
    visible_rect: RECT,
    normal_rect: RECT,
}

fn query_window_geometry_fields(hwnd: HWND, monitors: &[MonitorInfo]) -> WindowGeometryFields {
    let is_iconic = unsafe { IsIconic(hwnd) }.as_bool();
    let mut rect = RECT::default();
    let _ = unsafe { GetWindowRect(hwnd, &mut rect) };

    let mut placement = WINDOWPLACEMENT {
        length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
        ..Default::default()
    };
    let has_placement = unsafe { GetWindowPlacement(hwnd, &mut placement) }.is_ok();
    let is_maximized = unsafe { IsZoomed(hwnd) }.as_bool()
        || (has_placement && placement.showCmd == SW_SHOWMAXIMIZED.0 as u32);
    let restore_to_maximized =
        has_placement && (placement.flags.0 & WPF_RESTORETOMAXIMIZED.0) != 0;

    // For minimized windows, GetWindowRect is iconic (-32000, -32000), so MonitorFromWindow
    // would bias toward the top-left-most monitor. Resolve monitor from rcNormalPosition instead.
    let hmon = if is_iconic
        && has_placement
        && placement.rcNormalPosition.right > placement.rcNormalPosition.left
        && placement.rcNormalPosition.bottom > placement.rcNormalPosition.top
    {
        unsafe { MonitorFromRect(&placement.rcNormalPosition, MONITOR_DEFAULTTONEAREST) }
    } else {
        unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) }
    };
    let monitor_index = monitors.iter().position(|m| m.handle == hmon.0 as isize);

    let visible_rect = if !is_iconic {
        get_extended_frame_bounds(hwnd).unwrap_or(rect)
    } else {
        rect
    };

    let (ws_off_x, ws_off_y) = monitor_index
        .and_then(|idx| monitors.get(idx))
        .map(|m| (m.work_area.left - m.bounds.left, m.work_area.top - m.bounds.top))
        .unwrap_or((0, 0));
    let normal_rect = if has_placement {
        RECT {
            left: placement.rcNormalPosition.left + ws_off_x,
            top: placement.rcNormalPosition.top + ws_off_y,
            right: placement.rcNormalPosition.right + ws_off_x,
            bottom: placement.rcNormalPosition.bottom + ws_off_y,
        }
    } else {
        RECT::default()
    };

    WindowGeometryFields {
        is_minimized: is_iconic,
        is_maximized,
        restore_to_maximized,
        monitor_index,
        rect,
        visible_rect,
        normal_rect,
    }
}

/// Refreshes a `WindowInfo`'s live Win32/DWM geometry fields (minimized/maximized flags,
/// monitor index, `rect`, `visible_rect`, and `normal_rect`) in place.
/// Returns `false` if the HWND is no longer valid.
pub fn refresh_window_geometry(info: &mut WindowInfo, monitors: &[MonitorInfo]) -> bool {
    let hwnd = HWND(info.hwnd as *mut c_void);
    if info.hwnd == 0 || !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return false;
    }
    let g = query_window_geometry_fields(hwnd, monitors);
    info.is_minimized = g.is_minimized;
    info.is_maximized = g.is_maximized;
    info.restore_to_maximized = g.restore_to_maximized;
    info.monitor_index = g.monitor_index;
    info.rect = g.rect;
    info.visible_rect = g.visible_rect;
    info.normal_rect = g.normal_rect;
    true
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

fn is_generic_product_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    (lower.contains("windows") && lower.contains("operating system"))
        || name.contains("操作系统")
        || lower == "application frame host"
}

fn ver_query_string(buf: &[u8], sub_block: &str) -> Option<String> {
    let wide_sub = HSTRING::from(sub_block);
    let mut ptr: *mut c_void = std::ptr::null_mut();
    let mut len: u32 = 0;
    let ok = unsafe {
        VerQueryValueW(
            buf.as_ptr() as *const c_void,
            PCWSTR(wide_sub.as_ptr()),
            &mut ptr,
            &mut len,
        )
    };
    if !ok.as_bool() || ptr.is_null() || len == 0 {
        return None;
    }
    let slice = unsafe { std::slice::from_raw_parts(ptr as *const u16, len as usize) };
    let nul_pos = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
    let text = String::from_utf16_lossy(&slice[..nul_pos]).trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn read_version_info_bytes(exe_path: &str) -> Option<Vec<u8>> {
    let wide = HSTRING::from(exe_path);
    let mut handle = 0u32;
    let size = unsafe {
        GetFileVersionInfoSizeExW(FILE_VER_GET_LOCALISED, PCWSTR(wide.as_ptr()), &mut handle)
    };
    if size > 0 {
        let mut buf = vec![0u8; size as usize];
        if unsafe {
            GetFileVersionInfoExW(
                FILE_VER_GET_LOCALISED,
                PCWSTR(wide.as_ptr()),
                None,
                size,
                buf.as_mut_ptr() as *mut c_void,
            )
        }
        .is_ok()
        {
            return Some(buf);
        }
    }

    let size = unsafe { GetFileVersionInfoSizeW(PCWSTR(wide.as_ptr()), None) };
    if size == 0 {
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    unsafe {
        GetFileVersionInfoW(
            PCWSTR(wide.as_ptr()),
            None,
            size,
            buf.as_mut_ptr() as *mut c_void,
        )
    }
    .ok()?;
    Some(buf)
}

fn extract_exe_app_name_uncached(exe_path: &str) -> String {
    if exe_path.is_empty() {
        return String::new();
    }
    if Path::new(exe_path)
        .file_name()
        .is_some_and(|n| n.eq_ignore_ascii_case("ApplicationFrameHost.exe"))
    {
        return String::new();
    }

    let Some(buf) = read_version_info_bytes(exe_path) else {
        return String::new();
    };

    let mut translations: Vec<(u16, u16)> = Vec::new();
    let trans_sub = HSTRING::from(r"\VarFileInfo\Translation");
    let mut trans_ptr: *mut c_void = std::ptr::null_mut();
    let mut trans_bytes: u32 = 0;
    if unsafe {
        VerQueryValueW(
            buf.as_ptr() as *const c_void,
            PCWSTR(trans_sub.as_ptr()),
            &mut trans_ptr,
            &mut trans_bytes,
        )
    }
    .as_bool()
        && !trans_ptr.is_null()
        && trans_bytes >= 4
    {
        let pair_count = (trans_bytes as usize) / 4;
        let words = unsafe { std::slice::from_raw_parts(trans_ptr as *const u16, pair_count * 2) };
        for &[lang, cp] in words.as_chunks::<2>().0 {
            translations.push((lang, cp));
        }
    }

    // Prefer Chinese localized string tables (primary lang ID 0x0004: zh-CN / zh-TW / zh-HK) first.
    translations.sort_by_key(|&(lang, _)| if (lang & 0x03FF) == 0x0004 { 0u8 } else { 1u8 });

    for fallback in [(0x0804u16, 0x04B0u16), (0x0409, 0x04B0), (0x0409, 0x04E4)] {
        if !translations.contains(&fallback) {
            translations.push(fallback);
        }
    }

    let mut names: Vec<String> = Vec::new();
    fn push_unique(names: &mut Vec<String>, candidate: String) {
        let trimmed = candidate.trim();
        if trimmed.is_empty() || is_generic_product_name(trimmed) || names.len() >= 3 {
            return;
        }
        let cand_lower = trimmed.to_lowercase();
        if names
            .iter()
            .any(|existing| existing.to_lowercase().contains(&cand_lower))
        {
            return;
        }
        names.push(trimmed.to_string());
    }

    for (lang, cp) in translations {
        let prefix = format!(r"\StringFileInfo\{lang:04x}{cp:04x}");
        if let Some(desc) = ver_query_string(&buf, &format!(r"{prefix}\FileDescription")) {
            push_unique(&mut names, desc);
        }
        if let Some(prod) = ver_query_string(&buf, &format!(r"{prefix}\ProductName")) {
            push_unique(&mut names, prod);
        }
        if names.len() >= 3 {
            break;
        }
    }

    names.join(" ")
}

/// Resolves localized application display names (`FileDescription` / `ProductName`) from an
/// executable's PE version resource, cached per path so repeated window enumerations do zero I/O.
pub fn query_exe_app_name(exe_path: &str) -> String {
    if exe_path.is_empty() {
        return String::new();
    }
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = exe_path.to_ascii_lowercase();
    if let Ok(guard) = cache.lock()
        && let Some(cached) = guard.get(&key)
    {
        return cached.clone();
    }
    let resolved = extract_exe_app_name_uncached(exe_path);
    if let Ok(mut guard) = cache.lock() {
        guard.insert(key, resolved.clone());
    }
    resolved
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
        let geom = query_window_geometry_fields(hwnd, monitors);
        let w = geom.rect.right - geom.rect.left;
        let h = geom.rect.bottom - geom.rect.top;
        if !geom.is_minimized && (w <= 0 || h <= 0) {
            continue;
        }

        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        let (process_name, exe_path, is_elevated) = query_process_info(pid);
        let app_name = query_exe_app_name(&exe_path);

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
            app_name,
            exe_path,
            class_name,
            title,
            desktop_id,
            desktop_index,
            is_pinned: lookup.is_pinned,
            is_minimized: geom.is_minimized,
            is_maximized: geom.is_maximized,
            restore_to_maximized: geom.restore_to_maximized,
            is_elevated,
            cloaked_flags: cloaked,
            monitor_index: geom.monitor_index,
            rect: geom.rect,
            visible_rect: geom.visible_rect,
            normal_rect: geom.normal_rect,
        });
    }

    Ok(windows_out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_exe_app_name_skips_uwp_host_and_reads_real_system_exe() {
        assert_eq!(
            query_exe_app_name(r"C:\Windows\System32\ApplicationFrameHost.exe"),
            ""
        );
        assert_eq!(query_exe_app_name(""), "");

        let notepad = r"C:\Windows\System32\notepad.exe";
        if Path::new(notepad).exists() {
            let first = query_exe_app_name(notepad);
            let second = query_exe_app_name(notepad);
            assert!(!first.is_empty(), "expected non-empty version name for notepad.exe");
            assert_eq!(first, second);
            assert!(
                !first.to_ascii_lowercase().contains("operating system"),
                "generic OS product name should be filtered out, got: {first}"
            );
        }
    }
}
