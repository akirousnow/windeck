use std::ffi::c_void;

use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::StationsAndDesktops::{
    GetProcessWindowStation, GetThreadDesktop, GetUserObjectInformationW, UOI_NAME,
};
use windows::Win32::System::Threading::{
    GR_GDIOBJECTS, GR_USEROBJECTS, GetCurrentProcess, GetCurrentThreadId, GetGuiResources,
    GetProcessHandleCount, GetProcessTimes,
};

#[derive(Debug, Clone, Copy)]
pub struct ProcessResourceSnapshot {
    pub gdi_objects: u32,
    pub user_objects: u32,
    pub kernel_handles: u32,
    pub cpu_time_ms: f64,
}

fn filetime_to_100ns(ft: FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | (ft.dwLowDateTime as u64)
}

pub fn capture_process_resources() -> ProcessResourceSnapshot {
    let proc = unsafe { GetCurrentProcess() };
    let gdi_objects = unsafe { GetGuiResources(proc, GR_GDIOBJECTS) };
    let user_objects = unsafe { GetGuiResources(proc, GR_USEROBJECTS) };
    let mut kernel_handles = 0u32;
    let _ = unsafe { GetProcessHandleCount(proc, &mut kernel_handles) };

    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let cpu_time_ms = if unsafe {
        GetProcessTimes(proc, &mut creation, &mut exit, &mut kernel, &mut user)
    }
    .is_ok()
    {
        let total_100ns = filetime_to_100ns(kernel) + filetime_to_100ns(user);
        total_100ns as f64 / 10_000.0
    } else {
        0.0
    };

    ProcessResourceSnapshot {
        gdi_objects,
        user_objects,
        kernel_handles,
        cpu_time_ms,
    }
}

fn query_user_object_name(handle: *mut c_void) -> String {
    if handle.is_null() {
        return "<null>".to_string();
    }
    let mut buf = [0u16; 256];
    let mut needed = 0u32;
    let ok = unsafe {
        GetUserObjectInformationW(
            windows::Win32::Foundation::HANDLE(handle),
            UOI_NAME,
            Some(buf.as_mut_ptr() as *mut c_void),
            (buf.len() * 2) as u32,
            Some(&mut needed),
        )
    };
    if ok.is_ok() {
        let nul = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..nul])
    } else {
        "<error>".to_string()
    }
}

/// Returns `(window_station_name, thread_desktop_name)`.
pub fn current_session_desktop_names() -> (String, String) {
    let winsta = unsafe { GetProcessWindowStation() }
        .map(|h| h.0)
        .unwrap_or(std::ptr::null_mut());
    let desk = unsafe { GetThreadDesktop(GetCurrentThreadId()) }
        .map(|h| h.0)
        .unwrap_or(std::ptr::null_mut());
    (query_user_object_name(winsta), query_user_object_name(desk))
}
