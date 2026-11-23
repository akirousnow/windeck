pub mod dwm_thumb;
pub mod os_version;
pub mod overlay_win;
pub mod process_metrics;
pub mod shell_host;
pub mod virtual_desktop;
pub mod window_enum;
pub mod window_icon;
pub mod window_switch;

pub use dwm_thumb::DwmThumbnail;
pub use os_version::OsVersion;
pub use overlay_win::{
    OverlayMonitorPolicy, apply_win11_rounded_corners, hide_overlay, is_overlay_visible,
    show_and_focus_overlay, target_monitor_work_area,
};
pub use process_metrics::{
    ProcessResourceSnapshot, capture_process_resources, current_session_desktop_names,
};
pub use shell_host::{HotkeySpec, ShellEvent, ShellHost};
pub use virtual_desktop::{
    ComProbeStatus, DesktopId, VirtualDesktopClient, VirtualDesktopInfo, WindowDesktopLookup,
};
pub use window_enum::{
    MonitorInfo, RawWindowDiagnostic, WindowInfo, enumerate_monitors, enumerate_windows,
};
pub use window_icon::{IconBgra, extract_window_icon};
pub use window_switch::{SwitchReport, switch_to_window};
