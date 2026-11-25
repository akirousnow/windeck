pub mod dwm_thumb;
pub mod os_version;
pub mod overlay_win;
pub mod process_metrics;
pub mod screen_capture;
pub mod shell_host;
pub mod updater;
pub mod virtual_desktop;
pub mod window_enum;
pub mod window_icon;
pub mod window_switch;

pub use dwm_thumb::DwmThumbnail;
pub use os_version::OsVersion;
pub use overlay_win::{
    BackdropStyle, LayeredOverlayDiagnostics, OverlayMonitorPolicy, ThumbnailHostWindow,
    apply_backdrop_style, configure_borderless_immersive, current_cursor_screen_pos,
    hide_layered_overlay, inspect_layered_overlay_state, pin_layered_z_order,
    query_window_and_client_sizes, refocus_layered_overlay, show_and_focus_layered_overlay,
    target_monitor_rects,
};
pub use process_metrics::{
    ProcessResourceSnapshot, capture_process_resources, current_session_desktop_names,
};
pub use screen_capture::{CapturedBitmap, capture_screen_rect_bgra, capture_window_bgra};
pub use shell_host::{
    DesktopSwitchDir, HoldNavDirection, HotkeySpec, PrimaryModifier, QuickDragModifiers,
    QuickDragMouseButton, QuickDragShortcut, QuickDragTriggerInfo, ShellEvent, ShellHost,
    SingleInstanceGuard, current_physical_modifiers, is_autostart_enabled,
    notify_existing_instance, set_autostart_enabled,
};
pub use updater::{
    CURRENT_VERSION, GITHUB_OWNER, GITHUB_RELEASES_PAGE, GITHUB_REPO, ReleaseInfo,
    UpdateCheckResult, check_latest_release, download_and_stage_update, is_newer_version,
    normalize_version_str, relaunch_updated_exe, wait_for_pid_and_cleanup_old_exe,
};
pub use virtual_desktop::{
    ComProbeStatus, DesktopId, VirtualDesktopClient, VirtualDesktopInfo, WindowDesktopLookup,
};
pub use window_enum::{
    MonitorInfo, WindowInfo, enumerate_monitors, enumerate_windows, query_exe_app_name,
    refresh_window_geometry,
};
pub use window_icon::{IconBgra, extract_window_icon};
pub use window_switch::{
    DwmTransitionSuppressGuard, ForceQuitOutcome, ForceQuitReport, ForegroundActivationResult,
    PreRestoreResult, SwitchReport, activate_window_foreground, force_quit_window,
    pre_restore_minimized_window, switch_desktop_and_wait, switch_to_window,
};
