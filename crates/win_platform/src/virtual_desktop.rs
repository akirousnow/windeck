use std::collections::HashMap;
use std::ffi::c_void;
use std::fmt;
use std::sync::mpsc;
use std::thread;

use anyhow::{Context, Result, anyhow};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::Com::{
    CLSCTX_ALL, CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoUninitialize, IServiceProvider,
};
use windows::Win32::UI::Shell::Common::IObjectArray;
use windows::Win32::UI::Shell::{IVirtualDesktopManager, VirtualDesktopManager};
use windows_core::{BOOL, GUID, HRESULT, HSTRING, IUnknown, IUnknown_Vtbl, Interface, PCWSTR, interface};
use windows_registry::CURRENT_USER;

use crate::os_version::OsVersion;

const CLSID_IMMERSIVE_SHELL: GUID = GUID::from_u128(0xc2f03a33_21f5_47fa_b4bb_156362a2f239);
const CLSID_VIRTUAL_DESKTOP_MANAGER_INTERNAL: GUID =
    GUID::from_u128(0xc5e0cdca_7b6e_41b2_9fc4_d93975cc467b);
const CLSID_VIRTUAL_DESKTOP_PINNED_APPS: GUID =
    GUID::from_u128(0xb5a399e7_1c87_46b8_88e9_fc5747b171bd);
const CLSID_APPLICATION_VIEW_COLLECTION: GUID =
    GUID::from_u128(0x1841c6d7_4f9d_42c0_af41_8747538f10e5);

// Complete IVirtualDesktopManagerInternal & IVirtualDesktop compatibility profiles across Win10 & Win11:
// 1. Win11 24H2 (26100.2605+) / 25H2 / 26H1 (28000+): IID 53F5CA0B (has SwitchDesktopAndMoveForegroundView) + IVirtualDesktop 3F07F4BE
// 2. Win11 23H2 (22631.3085+) / early 24H2 (<26100.2605): IID 53F5CA0B (no SwitchDesktopAndMoveForegroundView) + IVirtualDesktop 3F07F4BE
// 3. Win11 22H2 (22621.2215..3084) / early 23H2: IID A3175F2D + IVirtualDesktop 3F07F4BE
// 4. Win11 21H2 (22000) / early 22H2 (<22621.2215): IID B2F925B9 (HMONITOR arg) + IVirtualDesktop 536D3495
// 5. Win10 1809..22H2 (17763..19045): IID F31574D6 + IVirtualDesktop FF72FFDD

#[interface("372E1D3B-38D3-42E4-A15B-8AB2B178F513")]
unsafe trait IApplicationView: IUnknown {}

#[interface("1841C6D7-4F9D-42C0-AF41-8747538F10E5")]
unsafe trait IApplicationViewCollection: IUnknown {
    fn get_views(&self, out: *mut *mut c_void) -> HRESULT;
    fn get_views_by_z_order(&self, out: *mut *mut c_void) -> HRESULT;
    fn get_views_by_app_user_model_id(&self, id: PCWSTR, out: *mut *mut c_void) -> HRESULT;
    fn get_view_for_hwnd(&self, hwnd: HWND, out: *mut *mut c_void) -> HRESULT;
}

/// Win11 22H2 (22621.2215+) .. 26H1
#[interface("3F07F4BE-B107-441A-AF0F-39D82529072C")]
unsafe trait IVirtualDesktopModern: IUnknown {
    fn is_view_visible(&self, view: *mut c_void, visible: *mut i32) -> HRESULT;
    fn get_id(&self, id: *mut GUID) -> HRESULT;
    fn get_name(&self, name: *mut HSTRING) -> HRESULT;
}

/// Win11 21H2 (22000) & 22H2 (< 22621.2215)
#[interface("536D3495-B208-4CC9-AE26-DE8111275BF8")]
unsafe trait IVirtualDesktopWin11Legacy: IUnknown {
    fn is_view_visible(&self, view: *mut c_void, visible: *mut i32) -> HRESULT;
    fn get_id(&self, id: *mut GUID) -> HRESULT;
    fn get_monitor(&self, monitor: *mut HMONITOR) -> HRESULT;
    fn get_name(&self, name: *mut HSTRING) -> HRESULT;
}

/// Windows 10 1809..22H2 (Build 17763..19045)
#[interface("FF72FFDD-BE7E-43FC-9C03-AD81681E88E4")]
unsafe trait IVirtualDesktopWin10: IUnknown {
    fn is_view_visible(&self, view: *mut c_void, visible: *mut i32) -> HRESULT;
    fn get_id(&self, id: *mut GUID) -> HRESULT;
}

/// Profile 1: Win11 24H2 (26100.2605+) / 25H2 / 26H1 (28000+)
#[interface("53F5CA0B-158F-4124-900C-057158060B27")]
unsafe trait IVirtualDesktopManagerInternal24H2: IUnknown {
    fn get_count(&self, count: *mut u32) -> HRESULT;
    fn move_view_to_desktop(&self, view: *mut c_void, desktop: *mut c_void) -> HRESULT;
    fn can_view_move_desktops(&self, view: *mut c_void, can_move: *mut i32) -> HRESULT;
    fn get_current_desktop(&self, out: *mut *mut c_void) -> HRESULT;
    fn get_desktops(&self, out: *mut *mut c_void) -> HRESULT;
    fn get_adjacent_desktop(
        &self,
        from: *mut c_void,
        direction: i32,
        out: *mut *mut c_void,
    ) -> HRESULT;
    fn switch_desktop(&self, desktop: *mut c_void) -> HRESULT;
    fn switch_desktop_and_move_foreground_view(&self, desktop: *mut c_void) -> HRESULT;
    fn create_desktop(&self, out: *mut *mut c_void) -> HRESULT;
    fn move_desktop(&self, desktop: *mut c_void, index: i32) -> HRESULT;
    fn remove_desktop(&self, destroy: *mut c_void, fallback: *mut c_void) -> HRESULT;
}

/// Profile 2: Win11 23H2 (22631.3085+) & early 24H2 (< 26100.2605) — same IID 53F5CA0B, no SwitchDesktopAndMoveForegroundView
#[interface("53F5CA0B-158F-4124-900C-057158060B27")]
unsafe trait IVirtualDesktopManagerInternal23H2Late: IUnknown {
    fn get_count(&self, count: *mut u32) -> HRESULT;
    fn move_view_to_desktop(&self, view: *mut c_void, desktop: *mut c_void) -> HRESULT;
    fn can_view_move_desktops(&self, view: *mut c_void, can_move: *mut i32) -> HRESULT;
    fn get_current_desktop(&self, out: *mut *mut c_void) -> HRESULT;
    fn get_desktops(&self, out: *mut *mut c_void) -> HRESULT;
    fn get_adjacent_desktop(
        &self,
        from: *mut c_void,
        direction: i32,
        out: *mut *mut c_void,
    ) -> HRESULT;
    fn switch_desktop(&self, desktop: *mut c_void) -> HRESULT;
    fn create_desktop(&self, out: *mut *mut c_void) -> HRESULT;
    fn move_desktop(&self, desktop: *mut c_void, index: i32) -> HRESULT;
    fn remove_desktop(&self, destroy: *mut c_void, fallback: *mut c_void) -> HRESULT;
}

/// Profile 3: Win11 22H2 (22621.2215..3084) & early 23H2
#[interface("A3175F2D-239C-4BD2-8AA0-EEBA8B0B138E")]
unsafe trait IVirtualDesktopManagerInternal23H2Early: IUnknown {
    fn get_count(&self, count: *mut u32) -> HRESULT;
    fn move_view_to_desktop(&self, view: *mut c_void, desktop: *mut c_void) -> HRESULT;
    fn can_view_move_desktops(&self, view: *mut c_void, can_move: *mut i32) -> HRESULT;
    fn get_current_desktop(&self, out: *mut *mut c_void) -> HRESULT;
    fn get_desktops(&self, out: *mut *mut c_void) -> HRESULT;
    fn get_adjacent_desktop(
        &self,
        from: *mut c_void,
        direction: i32,
        out: *mut *mut c_void,
    ) -> HRESULT;
    fn switch_desktop(&self, desktop: *mut c_void) -> HRESULT;
    fn create_desktop(&self, out: *mut *mut c_void) -> HRESULT;
    fn move_desktop(&self, desktop: *mut c_void, index: i32) -> HRESULT;
    fn remove_desktop(&self, destroy: *mut c_void, fallback: *mut c_void) -> HRESULT;
}

/// Profile 4: Win11 21H2 (22000) & 22H2 (< 22621.2215) — HMONITOR parameter on manager calls
#[interface("B2F925B9-5A0F-4D2E-9F4D-2B1507593C10")]
unsafe trait IVirtualDesktopManagerInternal21H2: IUnknown {
    fn get_count(&self, monitor: HMONITOR, count: *mut u32) -> HRESULT;
    fn move_view_to_desktop(&self, view: *mut c_void, desktop: *mut c_void) -> HRESULT;
    fn can_view_move_desktops(&self, view: *mut c_void, can_move: *mut i32) -> HRESULT;
    fn get_current_desktop(&self, monitor: HMONITOR, out: *mut *mut c_void) -> HRESULT;
    fn get_all_current_desktops(&self, out: *mut *mut c_void) -> HRESULT;
    fn get_desktops(&self, monitor: HMONITOR, out: *mut *mut c_void) -> HRESULT;
    fn get_adjacent_desktop(
        &self,
        from: *mut c_void,
        direction: i32,
        out: *mut *mut c_void,
    ) -> HRESULT;
    fn switch_desktop(&self, monitor: HMONITOR, desktop: *mut c_void) -> HRESULT;
    fn create_desktop(&self, monitor: HMONITOR, out: *mut *mut c_void) -> HRESULT;
    fn move_desktop(&self, desktop: *mut c_void, monitor: HMONITOR, index: i32) -> HRESULT;
    fn remove_desktop(&self, destroy: *mut c_void, fallback: *mut c_void) -> HRESULT;
}

/// Profile 5: Windows 10 1809..22H2 (Build 17763..19045)
#[interface("F31574D6-B682-4CDC-BD56-1827860ABEC6")]
unsafe trait IVirtualDesktopManagerInternalWin10: IUnknown {
    fn get_count(&self, count: *mut u32) -> HRESULT;
    fn move_view_to_desktop(&self, view: *mut c_void, desktop: *mut c_void) -> HRESULT;
    fn can_view_move_desktops(&self, view: *mut c_void, can_move: *mut i32) -> HRESULT;
    fn get_current_desktop(&self, out: *mut *mut c_void) -> HRESULT;
    fn get_desktops(&self, out: *mut *mut c_void) -> HRESULT;
    fn get_adjacent_desktop(
        &self,
        from: *mut c_void,
        direction: i32,
        out: *mut *mut c_void,
    ) -> HRESULT;
    fn switch_desktop(&self, desktop: *mut c_void) -> HRESULT;
    fn create_desktop(&self, out: *mut *mut c_void) -> HRESULT;
    fn remove_desktop(&self, destroy: *mut c_void, fallback: *mut c_void) -> HRESULT;
}

#[interface("4CE81583-1E4C-4632-A621-07A53543148F")]
unsafe trait IVirtualDesktopPinnedApps: IUnknown {
    fn is_app_id_pinned(&self, app_id: PCWSTR, pinned: *mut BOOL) -> HRESULT;
    fn pin_app_id(&self, app_id: PCWSTR) -> HRESULT;
    fn unpin_app_id(&self, app_id: PCWSTR) -> HRESULT;
    fn is_view_pinned(&self, view: *mut c_void, pinned: *mut BOOL) -> HRESULT;
    fn pin_view(&self, view: *mut c_void) -> HRESULT;
    fn unpin_view(&self, view: *mut c_void) -> HRESULT;
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct DesktopId(pub GUID);

impl DesktopId {
    pub fn is_zero(self) -> bool {
        self.0 == GUID::zeroed()
    }

    pub fn from_le_bytes(slice: &[u8]) -> Option<Self> {
        if slice.len() != 16 {
            return None;
        }
        let d1 = u32::from_le_bytes(slice[0..4].try_into().ok()?);
        let d2 = u16::from_le_bytes(slice[4..6].try_into().ok()?);
        let d3 = u16::from_le_bytes(slice[6..8].try_into().ok()?);
        let d4: [u8; 8] = slice[8..16].try_into().ok()?;
        Some(Self(GUID::from_values(d1, d2, d3, d4)))
    }
}

impl fmt::Display for DesktopId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{{{:?}}}", self.0)
    }
}

impl fmt::Debug for DesktopId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DesktopId({{{:?}}})", self.0)
    }
}

#[derive(Debug, Clone)]
pub struct VirtualDesktopInfo {
    pub index: usize,
    pub id: DesktopId,
    pub custom_name: Option<String>,
    pub display_name: String,
    pub is_current: bool,
    pub source: &'static str,
}

#[derive(Debug, Clone)]
pub struct ComProbeStatus {
    pub official_vdm_ok: bool,
    pub internal_profile: Option<&'static str>,
    pub app_view_collection_ok: bool,
    pub pinned_apps_ok: bool,
    pub com_desktop_count: Option<u32>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct WindowDesktopLookup {
    pub official_desktop_id: Option<DesktopId>,
    pub official_is_on_current: Option<bool>,
    pub view_desktop_id: Option<DesktopId>,
    pub has_app_view: bool,
    pub is_pinned: bool,
}

impl WindowDesktopLookup {
    pub fn effective_desktop_id(&self) -> Option<DesktopId> {
        self.official_desktop_id
            .filter(|id| !id.is_zero())
            .or_else(|| self.view_desktop_id.filter(|id| !id.is_zero()))
    }
}

#[derive(Debug, Clone)]
pub struct RegistryDesktopSnapshot {
    pub ordered_ids: Vec<DesktopId>,
    pub current_id: Option<DesktopId>,
    pub custom_names: HashMap<DesktopId, String>,
}

pub fn read_registry_desktops() -> Result<RegistryDesktopSnapshot> {
    let key = CURRENT_USER
        .open(r"Software\Microsoft\Windows\CurrentVersion\Explorer\VirtualDesktops")
        .context("open HKCU VirtualDesktops key")?;

    let mut ordered_ids = Vec::new();
    if let Ok(val) = key.get_value("VirtualDesktopIDs") {
        for chunk in val.chunks_exact(16) {
            if let Some(id) = DesktopId::from_le_bytes(chunk) {
                ordered_ids.push(id);
            }
        }
    }

    let current_id = key
        .get_value("CurrentVirtualDesktop")
        .ok()
        .and_then(|val| DesktopId::from_le_bytes(&val));

    let mut custom_names = HashMap::new();
    for &id in &ordered_ids {
        let sub_path = format!(
            r"Software\Microsoft\Windows\CurrentVersion\Explorer\VirtualDesktops\Desktops\{{{:?}}}",
            id.0
        );
        if let Ok(sub) = CURRENT_USER.open(&sub_path) {
            if let Ok(name) = sub.get_string("Name") {
                let trimmed = name.trim();
                if !trimmed.is_empty() {
                    custom_names.insert(id, trimmed.to_string());
                }
            }
        }
    }

    Ok(RegistryDesktopSnapshot {
        ordered_ids,
        current_id,
        custom_names,
    })
}

enum ComVirtualDesktop {
    Modern(IVirtualDesktopModern),
    Win11Legacy(IVirtualDesktopWin11Legacy),
    Win10(IVirtualDesktopWin10),
}

impl ComVirtualDesktop {
    fn as_raw(&self) -> *mut c_void {
        match self {
            ComVirtualDesktop::Modern(d) => d.as_raw(),
            ComVirtualDesktop::Win11Legacy(d) => d.as_raw(),
            ComVirtualDesktop::Win10(d) => d.as_raw(),
        }
    }

    unsafe fn get_id(&self) -> windows_core::Result<DesktopId> {
        let mut guid = GUID::zeroed();
        let hr = match self {
            ComVirtualDesktop::Modern(d) => unsafe { d.get_id(&mut guid) },
            ComVirtualDesktop::Win11Legacy(d) => unsafe { d.get_id(&mut guid) },
            ComVirtualDesktop::Win10(d) => unsafe { d.get_id(&mut guid) },
        };
        hr.ok()?;
        Ok(DesktopId(guid))
    }

    unsafe fn get_name(&self) -> Option<String> {
        let mut hstr = HSTRING::new();
        let hr = match self {
            ComVirtualDesktop::Modern(d) => unsafe { d.get_name(&mut hstr) },
            ComVirtualDesktop::Win11Legacy(d) => unsafe { d.get_name(&mut hstr) },
            // Win10 IVirtualDesktop (FF72FFDD) has no GetName; Registry supplies custom names on Win10 2004+.
            ComVirtualDesktop::Win10(_) => return None,
        };
        if hr.is_ok() {
            let s = hstr.to_string();
            if s.trim().is_empty() { None } else { Some(s) }
        } else {
            None
        }
    }

    unsafe fn is_view_visible(&self, view: *mut c_void) -> bool {
        let mut vis = 0i32;
        let hr = match self {
            ComVirtualDesktop::Modern(d) => unsafe { d.is_view_visible(view, &mut vis) },
            ComVirtualDesktop::Win11Legacy(d) => unsafe { d.is_view_visible(view, &mut vis) },
            ComVirtualDesktop::Win10(d) => unsafe { d.is_view_visible(view, &mut vis) },
        };
        hr.is_ok() && vis != 0
    }
}

enum InternalVdm {
    Win11_24H2(IVirtualDesktopManagerInternal24H2),
    Win11_23H2Late(IVirtualDesktopManagerInternal23H2Late),
    Win11_23H2Early(IVirtualDesktopManagerInternal23H2Early),
    Win11_21H2(IVirtualDesktopManagerInternal21H2),
    Win10(IVirtualDesktopManagerInternalWin10),
}

impl InternalVdm {
    fn profile_name(&self) -> &'static str {
        match self {
            InternalVdm::Win11_24H2(_) => "Win11_24H2+ (53F5CA0B, 26100.2605+)",
            InternalVdm::Win11_23H2Late(_) => "Win11_23H2+ (53F5CA0B, <26100.2605)",
            InternalVdm::Win11_23H2Early(_) => "Win11_22H2/23H2 (A3175F2D)",
            InternalVdm::Win11_21H2(_) => "Win11_21H2/22H2 (B2F925B9)",
            InternalVdm::Win10(_) => "Win10_1809..22H2 (F31574D6)",
        }
    }

    unsafe fn wrap_raw_desktop(&self, raw: *mut c_void) -> ComVirtualDesktop {
        match self {
            InternalVdm::Win11_24H2(_)
            | InternalVdm::Win11_23H2Late(_)
            | InternalVdm::Win11_23H2Early(_) => {
                ComVirtualDesktop::Modern(unsafe { IVirtualDesktopModern::from_raw(raw) })
            }
            InternalVdm::Win11_21H2(_) => {
                ComVirtualDesktop::Win11Legacy(unsafe { IVirtualDesktopWin11Legacy::from_raw(raw) })
            }
            InternalVdm::Win10(_) => {
                ComVirtualDesktop::Win10(unsafe { IVirtualDesktopWin10::from_raw(raw) })
            }
        }
    }

    unsafe fn get_at(&self, array: &IObjectArray, idx: u32) -> windows_core::Result<ComVirtualDesktop> {
        match self {
            InternalVdm::Win11_24H2(_)
            | InternalVdm::Win11_23H2Late(_)
            | InternalVdm::Win11_23H2Early(_) => {
                Ok(ComVirtualDesktop::Modern(unsafe { array.GetAt::<IVirtualDesktopModern>(idx)? }))
            }
            InternalVdm::Win11_21H2(_) => Ok(ComVirtualDesktop::Win11Legacy(unsafe {
                array.GetAt::<IVirtualDesktopWin11Legacy>(idx)?
            })),
            InternalVdm::Win10(_) => {
                Ok(ComVirtualDesktop::Win10(unsafe { array.GetAt::<IVirtualDesktopWin10>(idx)? }))
            }
        }
    }

    unsafe fn get_count(&self) -> windows_core::Result<u32> {
        let mut count = 0u32;
        let hr = match self {
            InternalVdm::Win11_24H2(v) => unsafe { v.get_count(&mut count) },
            InternalVdm::Win11_23H2Late(v) => unsafe { v.get_count(&mut count) },
            InternalVdm::Win11_23H2Early(v) => unsafe { v.get_count(&mut count) },
            InternalVdm::Win11_21H2(v) => unsafe { v.get_count(HMONITOR::default(), &mut count) },
            InternalVdm::Win10(v) => unsafe { v.get_count(&mut count) },
        };
        hr.ok()?;
        Ok(count)
    }

    unsafe fn get_current_desktop(&self) -> windows_core::Result<ComVirtualDesktop> {
        let mut raw: *mut c_void = std::ptr::null_mut();
        let hr = match self {
            InternalVdm::Win11_24H2(v) => unsafe { v.get_current_desktop(&mut raw) },
            InternalVdm::Win11_23H2Late(v) => unsafe { v.get_current_desktop(&mut raw) },
            InternalVdm::Win11_23H2Early(v) => unsafe { v.get_current_desktop(&mut raw) },
            InternalVdm::Win11_21H2(v) => unsafe {
                v.get_current_desktop(HMONITOR::default(), &mut raw)
            },
            InternalVdm::Win10(v) => unsafe { v.get_current_desktop(&mut raw) },
        };
        hr.ok()?;
        if raw.is_null() {
            return Err(windows_core::Error::from(
                windows::Win32::Foundation::E_POINTER,
            ));
        }
        Ok(unsafe { self.wrap_raw_desktop(raw) })
    }

    unsafe fn get_desktops(&self) -> windows_core::Result<IObjectArray> {
        let mut raw: *mut c_void = std::ptr::null_mut();
        let hr = match self {
            InternalVdm::Win11_24H2(v) => unsafe { v.get_desktops(&mut raw) },
            InternalVdm::Win11_23H2Late(v) => unsafe { v.get_desktops(&mut raw) },
            InternalVdm::Win11_23H2Early(v) => unsafe { v.get_desktops(&mut raw) },
            InternalVdm::Win11_21H2(v) => unsafe { v.get_desktops(HMONITOR::default(), &mut raw) },
            InternalVdm::Win10(v) => unsafe { v.get_desktops(&mut raw) },
        };
        hr.ok()?;
        if raw.is_null() {
            return Err(windows_core::Error::from(
                windows::Win32::Foundation::E_POINTER,
            ));
        }
        Ok(unsafe { IObjectArray::from_raw(raw) })
    }

    unsafe fn switch_desktop(&self, desktop: &ComVirtualDesktop) -> windows_core::Result<()> {
        let hr = match self {
            InternalVdm::Win11_24H2(v) => unsafe { v.switch_desktop(desktop.as_raw()) },
            InternalVdm::Win11_23H2Late(v) => unsafe { v.switch_desktop(desktop.as_raw()) },
            InternalVdm::Win11_23H2Early(v) => unsafe { v.switch_desktop(desktop.as_raw()) },
            InternalVdm::Win11_21H2(v) => unsafe {
                v.switch_desktop(HMONITOR::default(), desktop.as_raw())
            },
            InternalVdm::Win10(v) => unsafe { v.switch_desktop(desktop.as_raw()) },
        };
        hr.ok()
    }

    unsafe fn create_desktop(&self) -> windows_core::Result<ComVirtualDesktop> {
        let mut raw: *mut c_void = std::ptr::null_mut();
        let hr = match self {
            InternalVdm::Win11_24H2(v) => unsafe { v.create_desktop(&mut raw) },
            InternalVdm::Win11_23H2Late(v) => unsafe { v.create_desktop(&mut raw) },
            InternalVdm::Win11_23H2Early(v) => unsafe { v.create_desktop(&mut raw) },
            InternalVdm::Win11_21H2(v) => unsafe {
                v.create_desktop(HMONITOR::default(), &mut raw)
            },
            InternalVdm::Win10(v) => unsafe { v.create_desktop(&mut raw) },
        };
        hr.ok()?;
        if raw.is_null() {
            return Err(windows_core::Error::from(
                windows::Win32::Foundation::E_POINTER,
            ));
        }
        Ok(unsafe { self.wrap_raw_desktop(raw) })
    }

    unsafe fn remove_desktop(
        &self,
        destroy: &ComVirtualDesktop,
        fallback: &ComVirtualDesktop,
    ) -> windows_core::Result<()> {
        let hr = match self {
            InternalVdm::Win11_24H2(v) => unsafe {
                v.remove_desktop(destroy.as_raw(), fallback.as_raw())
            },
            InternalVdm::Win11_23H2Late(v) => unsafe {
                v.remove_desktop(destroy.as_raw(), fallback.as_raw())
            },
            InternalVdm::Win11_23H2Early(v) => unsafe {
                v.remove_desktop(destroy.as_raw(), fallback.as_raw())
            },
            InternalVdm::Win11_21H2(v) => unsafe {
                v.remove_desktop(destroy.as_raw(), fallback.as_raw())
            },
            InternalVdm::Win10(v) => unsafe {
                v.remove_desktop(destroy.as_raw(), fallback.as_raw())
            },
        };
        hr.ok()
    }
}

fn has_24h2_2605_vtable() -> bool {
    OsVersion::current()
        .map(|v| v.build > 26100 || (v.build == 26100 && v.ubr >= 2605))
        .unwrap_or(true)
}

struct ComSession {
    official: Option<IVirtualDesktopManager>,
    internal: Option<InternalVdm>,
    app_views: Option<IApplicationViewCollection>,
    pinned_apps: Option<IVirtualDesktopPinnedApps>,
    init_errors: Vec<String>,
}

fn is_rpc_disconnect(err: &windows_core::Error) -> bool {
    let code = err.code().0 as u32;
    matches!(
        code,
        0x800706BA // RPC_S_SERVER_UNAVAILABLE
        | 0x80010108 // RPC_E_DISCONNECTED
        | 0x800401FD // CO_E_OBJNOTCONNECTED
        | 0x800706BE // RPC_S_CALL_FAILED
    )
}

impl ComSession {
    fn connect() -> Self {
        let mut init_errors = Vec::new();

        let official: Option<IVirtualDesktopManager> =
            match unsafe { CoCreateInstance(&VirtualDesktopManager, None, CLSCTX_ALL) } {
                Ok(v) => Some(v),
                Err(e) => {
                    init_errors.push(format!("IVirtualDesktopManager: {e}"));
                    None
                }
            };

        let sp: Option<IServiceProvider> =
            match unsafe { CoCreateInstance(&CLSID_IMMERSIVE_SHELL, None, CLSCTX_LOCAL_SERVER) } {
                Ok(v) => Some(v),
                Err(e) => {
                    init_errors.push(format!("CLSID_ImmersiveShell: {e}"));
                    None
                }
            };

        let mut internal = None;
        let mut app_views = None;
        let mut pinned_apps = None;

        if let Some(ref sp) = sp {
            // Probe COM profiles newest-to-oldest, using OS build/UBR to disambiguate IID 53F5CA0B's 26100.2605 vtable shift.
            let try_53f5 = if has_24h2_2605_vtable() {
                unsafe {
                    sp.QueryService::<IVirtualDesktopManagerInternal24H2>(
                        &CLSID_VIRTUAL_DESKTOP_MANAGER_INTERNAL,
                    )
                }
                .map(InternalVdm::Win11_24H2)
            } else {
                unsafe {
                    sp.QueryService::<IVirtualDesktopManagerInternal23H2Late>(
                        &CLSID_VIRTUAL_DESKTOP_MANAGER_INTERNAL,
                    )
                }
                .map(InternalVdm::Win11_23H2Late)
            };

            if let Ok(v) = try_53f5 {
                internal = Some(v);
            } else if let Ok(v) = unsafe {
                sp.QueryService::<IVirtualDesktopManagerInternal23H2Early>(
                    &CLSID_VIRTUAL_DESKTOP_MANAGER_INTERNAL,
                )
            } {
                internal = Some(InternalVdm::Win11_23H2Early(v));
            } else if let Ok(v) = unsafe {
                sp.QueryService::<IVirtualDesktopManagerInternal21H2>(
                    &CLSID_VIRTUAL_DESKTOP_MANAGER_INTERNAL,
                )
            } {
                internal = Some(InternalVdm::Win11_21H2(v));
            } else if let Ok(v) = unsafe {
                sp.QueryService::<IVirtualDesktopManagerInternalWin10>(
                    &CLSID_VIRTUAL_DESKTOP_MANAGER_INTERNAL,
                )
            } {
                internal = Some(InternalVdm::Win10(v));
            } else {
                init_errors.push(
                    "IVirtualDesktopManagerInternal: no supported IID matched (53F5CA0B/A3175F2D/B2F925B9/F31574D6)"
                        .to_string(),
                );
            }

            match unsafe {
                sp.QueryService::<IApplicationViewCollection>(&CLSID_APPLICATION_VIEW_COLLECTION)
            } {
                Ok(v) => app_views = Some(v),
                Err(e) => init_errors.push(format!("IApplicationViewCollection: {e}")),
            }

            match unsafe {
                sp.QueryService::<IVirtualDesktopPinnedApps>(&CLSID_VIRTUAL_DESKTOP_PINNED_APPS)
            } {
                Ok(v) => pinned_apps = Some(v),
                Err(e) => init_errors.push(format!("IVirtualDesktopPinnedApps: {e}")),
            }
        }

        Self {
            official,
            internal,
            app_views,
            pinned_apps,
            init_errors,
        }
    }

    fn probe_status(&self) -> ComProbeStatus {
        let com_desktop_count = self
            .internal
            .as_ref()
            .and_then(|v| unsafe { v.get_count().ok() });

        ComProbeStatus {
            official_vdm_ok: self.official.is_some(),
            internal_profile: self.internal.as_ref().map(|v| v.profile_name()),
            app_view_collection_ok: self.app_views.is_some(),
            pinned_apps_ok: self.pinned_apps.is_some(),
            com_desktop_count,
            errors: self.init_errors.clone(),
        }
    }

    fn list_com_desktops(
        &self,
    ) -> windows_core::Result<(Vec<ComVirtualDesktop>, Option<DesktopId>)> {
        let Some(ref internal) = self.internal else {
            return Err(windows_core::Error::from(
                windows::Win32::Foundation::E_NOINTERFACE,
            ));
        };

        let current_id = unsafe { internal.get_current_desktop() }
            .ok()
            .and_then(|d| unsafe { d.get_id() }.ok());

        let array = unsafe { internal.get_desktops()? };
        let count = unsafe { array.GetCount()? };
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count {
            if let Ok(d) = unsafe { internal.get_at(&array, i) } {
                out.push(d);
            }
        }
        Ok((out, current_id))
    }

    fn get_desktops_merged(&mut self) -> Result<Vec<VirtualDesktopInfo>> {
        let reg = read_registry_desktops().ok();

        let com_result = self.list_com_desktops();
        let com_result = match com_result {
            Err(ref e) if is_rpc_disconnect(e) => {
                *self = ComSession::connect();
                self.list_com_desktops()
            }
            other => other,
        };

        if let Ok((com_desktops, current_id)) = com_result {
            if !com_desktops.is_empty() {
                let mut result = Vec::with_capacity(com_desktops.len());
                for (index, d) in com_desktops.iter().enumerate() {
                    let Ok(id) = (unsafe { d.get_id() }) else {
                        continue;
                    };
                    let com_name = unsafe { d.get_name() };
                    let custom_name = com_name.or_else(|| {
                        reg.as_ref()
                            .and_then(|r| r.custom_names.get(&id).cloned())
                    });
                    let display_name = custom_name
                        .clone()
                        .unwrap_or_else(|| format!("Desktop {}", index + 1));
                    let is_current = current_id
                        .or_else(|| reg.as_ref().and_then(|r| r.current_id))
                        .is_some_and(|cur| cur == id);
                    result.push(VirtualDesktopInfo {
                        index,
                        id,
                        custom_name,
                        display_name,
                        is_current,
                        source: "COM+Registry",
                    });
                }
                return Ok(result);
            }
        }

        // Safe fallback: pure Registry snapshot (works on unknown future builds without crashing!)
        let reg = reg.ok_or_else(|| anyhow!("Both COM and Registry virtual desktop reads failed"))?;
        let mut result = Vec::with_capacity(reg.ordered_ids.len());
        for (index, &id) in reg.ordered_ids.iter().enumerate() {
            let custom_name = reg.custom_names.get(&id).cloned();
            let display_name = custom_name
                .clone()
                .unwrap_or_else(|| format!("Desktop {}", index + 1));
            let is_current = reg.current_id.is_some_and(|cur| cur == id);
            result.push(VirtualDesktopInfo {
                index,
                id,
                custom_name,
                display_name,
                is_current,
                source: "Registry-fallback",
            });
        }
        Ok(result)
    }

    fn query_windows_batch(&mut self, hwnds: &[isize]) -> HashMap<isize, WindowDesktopLookup> {
        let mut results = HashMap::with_capacity(hwnds.len());

        // Pre-fetch COM desktop objects once per batch so checking IsViewVisible is O(desktops) without repeated IPC.
        let com_desktops: Vec<(DesktopId, ComVirtualDesktop)> = self
            .list_com_desktops()
            .map(|(list, _)| {
                list.into_iter()
                    .filter_map(|d| unsafe { d.get_id() }.ok().map(|id| (id, d)))
                    .collect()
            })
            .unwrap_or_default();

        for &raw_hwnd in hwnds {
            let hwnd = HWND(raw_hwnd as *mut c_void);
            let mut lookup = WindowDesktopLookup::default();

            if let Some(ref official) = self.official {
                if let Ok(on_cur) = unsafe { official.IsWindowOnCurrentVirtualDesktop(hwnd) } {
                    lookup.official_is_on_current = Some(on_cur.as_bool());
                }
                if let Ok(guid) = unsafe { official.GetWindowDesktopId(hwnd) } {
                    if guid != GUID::zeroed() {
                        lookup.official_desktop_id = Some(DesktopId(guid));
                    }
                }
            }

            if let Some(ref app_views) = self.app_views {
                let mut raw_view: *mut c_void = std::ptr::null_mut();
                if unsafe { app_views.get_view_for_hwnd(hwnd, &mut raw_view) }.is_ok()
                    && !raw_view.is_null()
                {
                    let view = unsafe { IApplicationView::from_raw(raw_view) };
                    lookup.has_app_view = true;

                    if let Some(ref pinned) = self.pinned_apps {
                        let mut is_pinned = BOOL(0);
                        if unsafe { pinned.is_view_pinned(view.as_raw(), &mut is_pinned) }.is_ok() {
                            lookup.is_pinned = is_pinned.as_bool();
                        }
                    }

                    // If not pinned, locate the desktop where IsViewVisible returns true
                    if !lookup.is_pinned {
                        for (desktop_id, desktop) in &com_desktops {
                            if unsafe { desktop.is_view_visible(view.as_raw()) } {
                                lookup.view_desktop_id = Some(*desktop_id);
                                break;
                            }
                        }
                    }
                }
            }

            results.insert(raw_hwnd, lookup);
        }

        results
    }

    fn switch_to_desktop(&mut self, target_id: DesktopId) -> Result<()> {
        for attempt in 0..2 {
            let list = match self.list_com_desktops() {
                Ok((list, _)) => list,
                Err(e) if attempt == 0 && is_rpc_disconnect(&e) => {
                    *self = ComSession::connect();
                    continue;
                }
                Err(e) => {
                    return Err(anyhow!(
                        "Cannot switch virtual desktop via internal COM: {e}"
                    ));
                }
            };

            for desktop in &list {
                if unsafe { desktop.get_id() }.ok() == Some(target_id) {
                    if let Some(ref internal) = self.internal {
                        match unsafe { internal.switch_desktop(desktop) } {
                            Ok(()) => return Ok(()),
                            Err(e) if attempt == 0 && is_rpc_disconnect(&e) => {
                                *self = ComSession::connect();
                                break;
                            }
                            Err(e) => return Err(anyhow!("SwitchDesktop failed: {e}")),
                        }
                    }
                }
            }
            if attempt == 0 {
                return Err(anyhow!("Target desktop {} not found in COM list", target_id));
            }
        }
        Err(anyhow!("SwitchDesktop failed after reconnect retry"))
    }

    fn create_desktop(&mut self) -> Result<DesktopId> {
        for attempt in 0..2 {
            let Some(ref internal) = self.internal else {
                return Err(anyhow!("Internal COM IVirtualDesktopManagerInternal unavailable"));
            };
            match unsafe { internal.create_desktop() } {
                Ok(new_desktop) => {
                    let id = unsafe { new_desktop.get_id() }
                        .context("GetId on newly created virtual desktop")?;
                    return Ok(id);
                }
                Err(e) if attempt == 0 && is_rpc_disconnect(&e) => {
                    *self = ComSession::connect();
                    continue;
                }
                Err(e) => return Err(anyhow!("CreateDesktop failed: {e}")),
            }
        }
        Err(anyhow!("CreateDesktop failed after reconnect retry"))
    }

    fn remove_desktop(&mut self, destroy_id: DesktopId, fallback_id: DesktopId) -> Result<()> {
        for attempt in 0..2 {
            let list = match self.list_com_desktops() {
                Ok((list, _)) => list,
                Err(e) if attempt == 0 && is_rpc_disconnect(&e) => {
                    *self = ComSession::connect();
                    continue;
                }
                Err(e) => return Err(anyhow!("Cannot list desktops for removal: {e}")),
            };
            let mut destroy_obj = None;
            let mut fallback_obj = None;
            for d in &list {
                if let Ok(id) = unsafe { d.get_id() } {
                    if id == destroy_id {
                        destroy_obj = Some(d);
                    }
                    if id == fallback_id {
                        fallback_obj = Some(d);
                    }
                }
            }
            let (Some(destroy_d), Some(fallback_d)) = (destroy_obj, fallback_obj) else {
                return Err(anyhow!("Desktop or fallback desktop not found"));
            };
            if let Some(ref internal) = self.internal {
                match unsafe { internal.remove_desktop(destroy_d, fallback_d) } {
                    Ok(()) => return Ok(()),
                    Err(e) if attempt == 0 && is_rpc_disconnect(&e) => {
                        *self = ComSession::connect();
                        continue;
                    }
                    Err(e) => return Err(anyhow!("RemoveDesktop failed: {e}")),
                }
            }
        }
        Err(anyhow!("RemoveDesktop failed after reconnect retry"))
    }
}

enum Command {
    ProbeStatus(mpsc::Sender<ComProbeStatus>),
    GetDesktops(mpsc::Sender<Result<Vec<VirtualDesktopInfo>>>),
    QueryWindows(Vec<isize>, mpsc::Sender<HashMap<isize, WindowDesktopLookup>>),
    SwitchToDesktop(DesktopId, mpsc::Sender<Result<()>>),
    CreateDesktop(mpsc::Sender<Result<DesktopId>>),
    RemoveDesktop(DesktopId, DesktopId, mpsc::Sender<Result<()>>),
    Reconnect(mpsc::Sender<ComProbeStatus>),
    Shutdown,
}

/// Thread-safe client backed by a dedicated STA COM worker thread.
pub struct VirtualDesktopClient {
    tx: mpsc::Sender<Command>,
    handle: Option<thread::JoinHandle<()>>,
}

impl VirtualDesktopClient {
    pub fn new() -> Result<Self> {
        let (tx, rx) = mpsc::channel::<Command>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();

        let handle = thread::Builder::new()
            .name("win-vdm-sta".to_string())
            .spawn(move || {
                let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
                if hr.is_err() {
                    let _ = ready_tx.send(Err(anyhow!("CoInitializeEx(STA) failed: {:?}", hr)));
                    return;
                }
                let mut session = ComSession::connect();
                let _ = ready_tx.send(Ok(()));

                while let Ok(cmd) = rx.recv() {
                    match cmd {
                        Command::ProbeStatus(reply) => {
                            let _ = reply.send(session.probe_status());
                        }
                        Command::GetDesktops(reply) => {
                            let _ = reply.send(session.get_desktops_merged());
                        }
                        Command::QueryWindows(hwnds, reply) => {
                            let _ = reply.send(session.query_windows_batch(&hwnds));
                        }
                        Command::SwitchToDesktop(id, reply) => {
                            let _ = reply.send(session.switch_to_desktop(id));
                        }
                        Command::CreateDesktop(reply) => {
                            let _ = reply.send(session.create_desktop());
                        }
                        Command::RemoveDesktop(destroy_id, fallback_id, reply) => {
                            let _ = reply.send(session.remove_desktop(destroy_id, fallback_id));
                        }
                        Command::Reconnect(reply) => {
                            session = ComSession::connect();
                            let _ = reply.send(session.probe_status());
                        }
                        Command::Shutdown => break,
                    }
                }

                drop(session);
                unsafe { CoUninitialize() };
            })
            .context("spawn win-vdm-sta thread")?;

        ready_rx
            .recv()
            .context("wait for win-vdm-sta startup")??;

        Ok(Self {
            tx,
            handle: Some(handle),
        })
    }

    pub fn probe_status(&self) -> Result<ComProbeStatus> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Command::ProbeStatus(tx))
            .map_err(|_| anyhow!("vdm worker stopped"))?;
        rx.recv().context("recv probe_status")
    }

    pub fn get_desktops(&self) -> Result<Vec<VirtualDesktopInfo>> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Command::GetDesktops(tx))
            .map_err(|_| anyhow!("vdm worker stopped"))?;
        rx.recv().context("recv get_desktops")?
    }

    pub fn query_windows(&self, hwnds: &[isize]) -> Result<HashMap<isize, WindowDesktopLookup>> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Command::QueryWindows(hwnds.to_vec(), tx))
            .map_err(|_| anyhow!("vdm worker stopped"))?;
        rx.recv().context("recv query_windows")
    }

    pub fn switch_to_desktop(&self, id: DesktopId) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Command::SwitchToDesktop(id, tx))
            .map_err(|_| anyhow!("vdm worker stopped"))?;
        rx.recv().context("recv switch_to_desktop")?
    }

    pub fn create_desktop(&self) -> Result<DesktopId> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Command::CreateDesktop(tx))
            .map_err(|_| anyhow!("vdm worker stopped"))?;
        rx.recv().context("recv create_desktop")?
    }

    pub fn remove_desktop(&self, destroy_id: DesktopId, fallback_id: DesktopId) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Command::RemoveDesktop(destroy_id, fallback_id, tx))
            .map_err(|_| anyhow!("vdm worker stopped"))?;
        rx.recv().context("recv remove_desktop")?
    }

    pub fn reconnect(&self) -> Result<ComProbeStatus> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Command::Reconnect(tx))
            .map_err(|_| anyhow!("vdm worker stopped"))?;
        rx.recv().context("recv reconnect")
    }
}

impl Drop for VirtualDesktopClient {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
