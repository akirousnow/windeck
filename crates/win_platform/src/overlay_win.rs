use std::ffi::c_void;
use std::sync::Once;

use anyhow::{Context, Result};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWM_WINDOW_CORNER_PREFERENCE, DWMWA_BORDER_COLOR, DWMWA_CLOAKED,
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DWMWCP_ROUND, DwmExtendFrameIntoClientArea,
    DwmGetWindowAttribute, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTOPRIMARY, MONITOR_DEFAULTTONEAREST, MONITORINFO,
    MonitorFromPoint,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleA, GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Controls::MARGINS;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_KEYUP, SendInput, VK_MENU,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, CreateWindowExW, DefWindowProcW, DestroyWindow, GW_HWNDNEXT, GWL_EXSTYLE,
    GWL_STYLE, GetClientRect, GetCursorPos, GetForegroundWindow, GetWindow, GetWindowLongPtrW,
    GetWindowRect, GetWindowThreadProcessId, HWND_NOTOPMOST, HWND_TOPMOST, IsHungAppWindow,
    IsIconic, IsWindowVisible, RegisterClassW, SET_WINDOW_POS_FLAGS, SW_HIDE, SW_SHOWNOACTIVATE,
    SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW,
    SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, ShowWindow, WNDCLASSW, WS_BORDER,
    WS_CAPTION, WS_DLGFRAME, WS_EX_CLIENTEDGE, WS_EX_DLGMODALFRAME, WS_EX_NOACTIVATE,
    WS_EX_NOREDIRECTIONBITMAP, WS_EX_STATICEDGE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_EX_TRANSPARENT, WS_EX_WINDOWEDGE, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_POPUP, WS_SYSMENU,
    WS_THICKFRAME,
};
use windows_core::{BOOL, PCSTR, w};

const DWMWA_COLOR_NONE: u32 = 0xFFFFFFFE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayMonitorPolicy {
    Cursor,
    Primary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackdropStyle {
    Blurred,
    Transparent,
    Opaque,
}

#[repr(C)]
struct WindowCompositionAttribData {
    attrib: u32,
    pv_data: *mut c_void,
    cb_data: usize,
}

#[repr(C)]
struct AccentPolicy {
    accent_state: u32,
    accent_flags: u32,
    gradient_color: u32,
    animation_id: u32,
}

fn apply_accent_policy(hwnd: HWND, state: u32, flags: u32, rgba: (u8, u8, u8, u8)) {
    unsafe {
        type SetWcaFn =
            unsafe extern "system" fn(HWND, *mut WindowCompositionAttribData) -> BOOL;
        let Ok(user32) = GetModuleHandleA(PCSTR::from_raw(c"user32.dll".as_ptr() as *const u8))
        else {
            return;
        };
        let Some(proc_addr) =
            GetProcAddress(user32, PCSTR::from_raw(c"SetWindowCompositionAttribute".as_ptr() as *const u8))
        else {
            return;
        };
        let set_wca: SetWcaFn = std::mem::transmute(proc_addr);
        let accent = AccentPolicy {
            accent_state: state,
            accent_flags: flags,
            gradient_color: (rgba.0 as u32)
                | ((rgba.1 as u32) << 8)
                | ((rgba.2 as u32) << 16)
                | ((rgba.3 as u32) << 24),
            animation_id: 0,
        };
        let mut data = WindowCompositionAttribData {
            attrib: 19, // WCA_ACCENT_POLICY
            pv_data: &accent as *const _ as *mut c_void,
            cb_data: std::mem::size_of::<AccentPolicy>(),
        };
        let _ = set_wca(hwnd, &mut data);
    }
}

/// Applies the frosted acrylic / translucent / opaque backdrop directly to a Win32 HWND.
/// `blur_opacity` controls the tint density (`0.05..=0.95`, lower = more transparent Gaussian blur).
pub fn apply_backdrop_style(hwnd: HWND, style: BackdropStyle, blur_opacity: f32) {
    let alpha = (blur_opacity.clamp(0.02, 0.95) * 255.0).round() as u8;
    match style {
        BackdropStyle::Blurred => {
            // ACCENT_ENABLE_ACRYLICBLURBEHIND: accent_flags=0 + low alpha yields luminous macOS Gaussian blur
            apply_accent_policy(hwnd, 4, 0, (10, 14, 22, alpha.max(1)));
        }
        BackdropStyle::Transparent => {
            // ACCENT_ENABLE_TRANSPARENTGRADIENT with adjustable dark translucent veil
            apply_accent_policy(hwnd, 2, 2, (10, 14, 22, alpha.max(24)));
        }
        BackdropStyle::Opaque => {
            // ACCENT_ENABLE_GRADIENT with deep slate canvas
            apply_accent_policy(hwnd, 1, 2, (12, 15, 22, 255));
        }
    }
}

/// Dedicated bottom-layer Win32 HWND that hosts the frosted Acrylic backdrop AND all
/// `DwmRegisterThumbnail` live window streams strictly beneath the interactive GPUI overlay HWND.
///
/// This two-layer split enables GPUI elements (selection rings, embedded bottom-right / bottom-left /
/// center app icon + title pills) to composite *above* live 60fps DWM thumbnails with full alpha blending.
pub struct ThumbnailHostWindow {
    hwnd: HWND,
}

impl ThumbnailHostWindow {
    /// Creates the hidden bottom-layer host HWND with frame attributes ready.
    /// The Acrylic / backdrop accent policy is deferred until [`Self::configure`] is called
    /// once the host window is visible and sized to its real monitor rect.
    pub fn create(fullscreen: bool) -> Result<Self> {
        static REGISTER_ONCE: Once = Once::new();
        let class_name = w!("WinDeckThumbHostClass");
        let hinstance = unsafe { GetModuleHandleW(None) }?;

        unsafe extern "system" fn host_proc(
            hwnd: HWND,
            msg: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }

        REGISTER_ONCE.call_once(|| {
            let wc = WNDCLASSW {
                lpfnWndProc: Some(host_proc),
                hInstance: hinstance.into(),
                lpszClassName: class_name,
                ..Default::default()
            };
            unsafe { RegisterClassW(&wc) };
        });

        let ex_style = WS_EX_TOOLWINDOW
            | WS_EX_NOACTIVATE
            | WS_EX_TRANSPARENT
            | WS_EX_TOPMOST
            | WS_EX_NOREDIRECTIONBITMAP;

        let hwnd = unsafe {
            CreateWindowExW(
                ex_style,
                class_name,
                w!("WinDeckThumbHost"),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                Some(hinstance.into()),
                None,
            )
        }
        .context("CreateWindowExW for ThumbnailHostWindow")?;

        let host = Self { hwnd };
        host.configure_frame_attrs(fullscreen);
        Ok(host)
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    fn configure_frame_attrs(&self, fullscreen: bool) {
        unsafe {
            let margins = MARGINS {
                cxLeftWidth: -1,
                cxRightWidth: -1,
                cyTopHeight: -1,
                cyBottomHeight: -1,
            };
            let _ = DwmExtendFrameIntoClientArea(self.hwnd, &margins);

            let no_border: u32 = DWMWA_COLOR_NONE;
            let _ = DwmSetWindowAttribute(
                self.hwnd,
                DWMWA_BORDER_COLOR,
                &no_border as *const _ as *const c_void,
                std::mem::size_of::<u32>() as u32,
            );

            let corner_pref: DWM_WINDOW_CORNER_PREFERENCE = if fullscreen {
                DWMWCP_DONOTROUND
            } else {
                DWMWCP_ROUND
            };
            let _ = DwmSetWindowAttribute(
                self.hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &corner_pref as *const _ as *const c_void,
                std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
            );
        }
    }

    pub fn configure(&self, fullscreen: bool, backdrop: BackdropStyle, blur_opacity: f32) {
        self.configure_frame_attrs(fullscreen);
        apply_backdrop_style(self.hwnd, backdrop, blur_opacity);
    }

    /// Resets `WCA_ACCENT_POLICY` to disabled before re-applying the target backdrop,
    /// forcing DWM to rebuild the Acrylic composition surface during recovery.
    pub fn force_reconfigure(&self, fullscreen: bool, backdrop: BackdropStyle, blur_opacity: f32) {
        apply_accent_policy(self.hwnd, 0, 0, (0, 0, 0, 0));
        self.configure(fullscreen, backdrop, blur_opacity);
    }
}

impl Drop for ThumbnailHostWindow {
    fn drop(&mut self) {
        if !self.hwnd.is_invalid() {
            let _ = unsafe { DestroyWindow(self.hwnd) };
            self.hwnd = HWND::default();
        }
    }
}

/// Strips every Win32/DWM border, caption, frame, and corner rounding so the GPUI window
/// becomes a 100% seamless edge-to-edge borderless canvas (macOS Mission Control style).
pub fn configure_borderless_immersive(hwnd: HWND, fullscreen: bool) {
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
        let stripped_style = (style
            & !(WS_CAPTION.0
                | WS_THICKFRAME.0
                | WS_BORDER.0
                | WS_DLGFRAME.0
                | WS_SYSMENU.0
                | WS_MINIMIZEBOX.0
                | WS_MAXIMIZEBOX.0))
            | WS_POPUP.0;
        SetWindowLongPtrW(hwnd, GWL_STYLE, stripped_style as isize);

        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        // Note: Win32 ignores WS_EX_TOPMOST in SetWindowLongPtrW; topmost band membership is
        // enforced via SetWindowPos(HWND_TOPMOST) in show_and_focus_layered_overlay / pin_layered_z_order.
        let clean_ex = (ex_style
            & !(WS_EX_WINDOWEDGE.0
                | WS_EX_CLIENTEDGE.0
                | WS_EX_DLGMODALFRAME.0
                | WS_EX_STATICEDGE.0))
            | WS_EX_TOOLWINDOW.0;
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, clean_ex as isize);

        let no_border: u32 = DWMWA_COLOR_NONE;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            &no_border as *const _ as *const c_void,
            std::mem::size_of::<u32>() as u32,
        );

        let corner_pref: DWM_WINDOW_CORNER_PREFERENCE = if fullscreen {
            DWMWCP_DONOTROUND
        } else {
            DWMWCP_ROUND
        };
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner_pref as *const _ as *const c_void,
            std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        );

        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// Returns `(full_monitor_rect, work_area_rect, scale_factor)` for the chosen monitor.
pub fn target_monitor_rects(policy: OverlayMonitorPolicy) -> Result<(RECT, RECT, f32)> {
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

    Ok((info.rcMonitor, info.rcWork, scale))
}

fn compute_overlay_rect(
    policy: OverlayMonitorPolicy,
    immersive_fullscreen: bool,
) -> Result<(i32, i32, i32, i32, f32)> {
    let (monitor_rc, work_rc, scale) = target_monitor_rects(policy)?;
    let (x, y, win_w, win_h) = if immersive_fullscreen {
        (
            monitor_rc.left,
            monitor_rc.top,
            (monitor_rc.right - monitor_rc.left).max(640),
            (monitor_rc.bottom - monitor_rc.top).max(480),
        )
    } else {
        let work_w = (work_rc.right - work_rc.left).max(640);
        let work_h = (work_rc.bottom - work_rc.top).max(480);
        let w = ((work_w as f32) * 0.90).round() as i32;
        let h = ((work_h as f32) * 0.88).round() as i32;
        let w = w.clamp(760.min(work_w), work_w - 16);
        let h = h.clamp(500.min(work_h), work_h - 16);
        (
            work_rc.left + (work_w - w) / 2,
            work_rc.top + (work_h - h) / 2,
            w,
            h,
        )
    };
    Ok((x, y, win_w, win_h, scale))
}

/// Returns the live cursor position in physical screen coordinates.
pub fn current_cursor_screen_pos() -> Option<(i32, i32)> {
    let mut pt = POINT::default();
    unsafe { GetCursorPos(&mut pt) }.ok().map(|()| (pt.x, pt.y))
}

fn focus_hwnd(hwnd: HWND) {
    unsafe {
        if SetForegroundWindow(hwnd).as_bool() && GetForegroundWindow() == hwnd {
            return;
        }

        // First fallback: briefly attach to the active non-hung foreground thread's input queue.
        // This works reliably even when triggered from WH_MOUSE_LL while Alt is physically held
        // (where injecting a synthetic VK_MENU tap is forbidden).
        let fg = GetForegroundWindow();
        if !fg.is_invalid() && fg != hwnd && !IsHungAppWindow(fg).as_bool() {
            let cur_tid = GetCurrentThreadId();
            let fg_tid = GetWindowThreadProcessId(fg, None);
            if fg_tid != 0 && fg_tid != cur_tid {
                let attached = AttachThreadInput(cur_tid, fg_tid, true).as_bool();
                let _ = BringWindowToTop(hwnd);
                let _ = SetForegroundWindow(hwnd);
                if attached {
                    let _ = AttachThreadInput(cur_tid, fg_tid, false);
                }
                if GetForegroundWindow() == hwnd {
                    return;
                }
            }
        }

        // Never inject a synthetic VK_MENU KEYUP while the user is physically holding Alt,
        // as doing so would break hold-Alt + arrow + release-to-jump tracking.
        if (GetAsyncKeyState(VK_MENU.0 as i32) as u16 & 0x8000) == 0 {
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
        }
        let _ = SetForegroundWindow(hwnd);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayeredOverlayDiagnostics {
    pub gpui_visible: bool,
    pub host_visible: bool,
    pub host_iconic: bool,
    pub host_cloaked: u32,
    pub host_directly_behind_gpui: bool,
    pub gpui_topmost: bool,
    pub host_topmost: bool,
    pub gpui_foreground: bool,
    pub gpui_size: (i32, i32),
    pub host_size: (i32, i32),
}

impl LayeredOverlayDiagnostics {
    pub fn is_healthy(&self) -> bool {
        self.gpui_visible
            && self.host_visible
            && !self.host_iconic
            && self.host_cloaked == 0
            && self.host_directly_behind_gpui
            && self.gpui_topmost
            && self.host_topmost
            && self.host_size.0 > 1
            && self.host_size.1 > 1
            && self.host_size == self.gpui_size
    }
}

/// Inspects live Win32/DWM visibility, Z-order adjacency, topmost flags, cloak state, and sizes
/// for the two-layer overlay pair (`gpui_hwnd` directly over `thumb_host_hwnd`).
pub fn inspect_layered_overlay_state(
    gpui_hwnd: HWND,
    thumb_host_hwnd: HWND,
) -> LayeredOverlayDiagnostics {
    unsafe {
        let gpui_visible = IsWindowVisible(gpui_hwnd).as_bool();
        let host_visible = IsWindowVisible(thumb_host_hwnd).as_bool();
        let host_iconic = IsIconic(thumb_host_hwnd).as_bool();

        let mut host_cloaked: u32 = 0;
        let _ = DwmGetWindowAttribute(
            thumb_host_hwnd,
            DWMWA_CLOAKED,
            &mut host_cloaked as *mut u32 as *mut c_void,
            std::mem::size_of::<u32>() as u32,
        );

        let gpui_ex = GetWindowLongPtrW(gpui_hwnd, GWL_EXSTYLE) as u32;
        let host_ex = GetWindowLongPtrW(thumb_host_hwnd, GWL_EXSTYLE) as u32;
        let gpui_topmost = (gpui_ex & WS_EX_TOPMOST.0) != 0;
        let host_topmost = (host_ex & WS_EX_TOPMOST.0) != 0;
        let gpui_foreground = GetForegroundWindow() == gpui_hwnd;

        let mut host_directly_behind_gpui = false;
        let mut intervening_visible = 0u32;
        let mut cur = GetWindow(gpui_hwnd, GW_HWNDNEXT).ok();
        let mut steps = 0u32;
        while let Some(hwnd) = cur {
            if hwnd == thumb_host_hwnd {
                host_directly_behind_gpui = intervening_visible == 0;
                break;
            }
            if IsWindowVisible(hwnd).as_bool() {
                intervening_visible += 1;
            }
            steps += 1;
            if steps > 512 {
                break;
            }
            cur = GetWindow(hwnd, GW_HWNDNEXT).ok();
        }

        let (gpui_size, _) = query_window_and_client_sizes(gpui_hwnd);
        let (host_size, _) = query_window_and_client_sizes(thumb_host_hwnd);

        LayeredOverlayDiagnostics {
            gpui_visible,
            host_visible,
            host_iconic,
            host_cloaked,
            host_directly_behind_gpui,
            gpui_topmost,
            host_topmost,
            gpui_foreground,
            gpui_size,
            host_size,
        }
    }
}

unsafe fn ensure_shown_noactivate(hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        // The very first ShowWindow call in a process may be overridden by STARTUPINFO.wShowWindow
        // (e.g. SW_HIDE / SW_MINIMIZE). Retry once so the first layer is never left hidden or iconic.
        if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
    }
}

unsafe fn promote_topmost(
    hwnd: HWND,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    flags: SET_WINDOW_POS_FLAGS,
) {
    unsafe {
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, w, h, flags);
        if (GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST.0) == 0 {
            // Win32 quirk: SetWindowPos(..., HWND_TOPMOST) can return TRUE without granting
            // WS_EX_TOPMOST when a window sits behind another popup or has stale band linkage.
            // Bouncing through HWND_NOTOPMOST forces win32k to re-link the window at the top
            // of the non-topmost band so the immediate HWND_TOPMOST promotion succeeds reliably.
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_NOTOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
            let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, w, h, flags);
        }
    }
}

/// Enforces strict two-layer Z-order lockstep: `gpui_hwnd` topmost at `rect`,
/// and `thumb_host_hwnd` topmost, visible, and positioned directly beneath `gpui_hwnd`.
pub fn pin_layered_z_order(gpui_hwnd: HWND, thumb_host_hwnd: HWND, rect: RECT) {
    let x = rect.left;
    let y = rect.top;
    let w = (rect.right - rect.left).max(1);
    let h = (rect.bottom - rect.top).max(1);
    unsafe {
        if !IsWindowVisible(thumb_host_hwnd).as_bool() || IsIconic(thumb_host_hwnd).as_bool() {
            ensure_shown_noactivate(thumb_host_hwnd);
        }
        if !IsWindowVisible(gpui_hwnd).as_bool() || IsIconic(gpui_hwnd).as_bool() {
            ensure_shown_noactivate(gpui_hwnd);
        }
        // Promote both layers into HWND_TOPMOST explicitly first: in Win32,
        // SetWindowPos(non_topmost, Some(topmost), ...) does NOT grant WS_EX_TOPMOST,
        // and placing a topmost host behind a non-topmost GPUI window would strip WS_EX_TOPMOST.
        promote_topmost(
            thumb_host_hwnd,
            x,
            y,
            w,
            h,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
        promote_topmost(gpui_hwnd, x, y, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
        let _ = SetWindowPos(
            thumb_host_hwnd,
            Some(gpui_hwnd),
            x,
            y,
            w,
            h,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
    }
}

/// Re-pins the two-layer topmost overlay stack and reclaims Win32 foreground focus for `gpui_hwnd`
/// (e.g. after Explorer bumps foreground during a background virtual-desktop switch).
pub fn refocus_layered_overlay(gpui_hwnd: HWND, thumb_host_hwnd: HWND, rect: RECT) {
    pin_layered_z_order(gpui_hwnd, thumb_host_hwnd, rect);
    focus_hwnd(gpui_hwnd);
}

/// Positions and reveals both layers in strict lockstep Z-order:
/// `gpui_hwnd` (topmost, per-pixel transparent interactive UI + embedded labels) directly above
/// `thumb_host_hwnd` (frosted Acrylic backdrop + live hardware-accelerated DWM thumbnails).
pub fn show_and_focus_layered_overlay(
    gpui_hwnd: HWND,
    thumb_host_hwnd: HWND,
    policy: OverlayMonitorPolicy,
    immersive_fullscreen: bool,
) -> Result<(RECT, f32)> {
    let (x, y, win_w, win_h, scale) = compute_overlay_rect(policy, immersive_fullscreen)?;
    let rect = RECT {
        left: x,
        top: y,
        right: x + win_w,
        bottom: y + win_h,
    };

    unsafe {
        // 1. Reveal & size bottom thumbnail/backdrop host without stealing focus
        ensure_shown_noactivate(thumb_host_hwnd);
        promote_topmost(
            thumb_host_hwnd,
            x,
            y,
            win_w,
            win_h,
            SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_FRAMECHANGED,
        );

        // 2. Reveal & size interactive GPUI overlay directly on top without premature activation
        ensure_shown_noactivate(gpui_hwnd);
        promote_topmost(
            gpui_hwnd,
            x,
            y,
            win_w,
            win_h,
            SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_FRAMECHANGED,
        );
    }

    // 3. Pin Z-order lockstep before activation, activate gpui_hwnd, then re-pin Z-order lockstep
    //    so focus changes (including VK_MENU fallback) can never leave thumb_host_hwnd behind another window.
    pin_layered_z_order(gpui_hwnd, thumb_host_hwnd, rect);
    focus_hwnd(gpui_hwnd);
    pin_layered_z_order(gpui_hwnd, thumb_host_hwnd, rect);

    Ok((rect, scale))
}

pub fn hide_layered_overlay(gpui_hwnd: HWND, thumb_host_hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(gpui_hwnd, SW_HIDE);
        let _ = ShowWindow(thumb_host_hwnd, SW_HIDE);
    }
}

pub fn query_window_and_client_sizes(hwnd: HWND) -> ((i32, i32), (i32, i32)) {
    let mut wr = RECT::default();
    let mut cr = RECT::default();
    let _ = unsafe { GetWindowRect(hwnd, &mut wr) };
    let _ = unsafe { GetClientRect(hwnd, &mut cr) };
    (
        (wr.right - wr.left, wr.bottom - wr.top),
        (cr.right - cr.left, cr.bottom - cr.top),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use windows::Win32::UI::WindowsAndMessaging::{HWND_BOTTOM, WINDOW_EX_STYLE};

    use super::*;

    static OVERLAY_TEST_LOCK: Mutex<()> = Mutex::new(());

    struct TestHwnd(HWND);

    impl TestHwnd {
        fn create_hidden_popup(width: i32, height: i32) -> Self {
            static REGISTER_TEST_CLASS: Once = Once::new();
            let class_name = w!("WinDeckOverlayTestPopup");
            let hinstance = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");

            unsafe extern "system" fn wnd_proc(
                hwnd: HWND,
                msg: u32,
                wparam: WPARAM,
                lparam: LPARAM,
            ) -> LRESULT {
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }

            REGISTER_TEST_CLASS.call_once(|| {
                let wc = WNDCLASSW {
                    lpfnWndProc: Some(wnd_proc),
                    hInstance: hinstance.into(),
                    lpszClassName: class_name,
                    ..Default::default()
                };
                unsafe { RegisterClassW(&wc) };
            });

            let hwnd = unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    class_name,
                    w!("WinDeckOverlayTest"),
                    WS_POPUP,
                    0,
                    0,
                    width,
                    height,
                    None,
                    None,
                    Some(hinstance.into()),
                    None,
                )
            }
            .expect("CreateWindowExW test popup");
            Self(hwnd)
        }
    }

    impl Drop for TestHwnd {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                let _ = unsafe { DestroyWindow(self.0) };
            }
        }
    }

    #[test]
    fn layered_overlay_show_and_hide_maintains_visibility_size_and_z_order() {
        let _guard = OVERLAY_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let gpui_win = TestHwnd::create_hidden_popup(800, 600);
        configure_borderless_immersive(gpui_win.0, true);
        let thumb_host = ThumbnailHostWindow::create(true).expect("ThumbnailHostWindow::create");

        let before = inspect_layered_overlay_state(gpui_win.0, thumb_host.hwnd());
        assert!(!before.gpui_visible);
        assert!(!before.host_visible);
        assert!(!before.is_healthy());

        let (rect, _scale) = show_and_focus_layered_overlay(
            gpui_win.0,
            thumb_host.hwnd(),
            OverlayMonitorPolicy::Primary,
            true,
        )
        .expect("show_and_focus_layered_overlay");
        thumb_host.configure(true, BackdropStyle::Blurred, 0.25);

        let shown = inspect_layered_overlay_state(gpui_win.0, thumb_host.hwnd());
        assert!(shown.gpui_visible, "gpui window should be visible");
        assert!(shown.host_visible, "thumb_host should be visible");
        assert!(!shown.host_iconic, "thumb_host should not be minimized");
        assert_eq!(shown.host_cloaked, 0, "thumb_host should not be cloaked");
        assert!(shown.gpui_topmost, "gpui window should be WS_EX_TOPMOST");
        assert!(shown.host_topmost, "thumb_host should be WS_EX_TOPMOST");
        assert!(
            shown.host_directly_behind_gpui,
            "thumb_host must sit directly beneath gpui window"
        );
        assert_eq!(shown.host_size, (rect.right - rect.left, rect.bottom - rect.top));
        assert_eq!(shown.gpui_size, shown.host_size);
        assert!(shown.is_healthy(), "overlay diagnostics must be healthy: {shown:?}");

        hide_layered_overlay(gpui_win.0, thumb_host.hwnd());
        let hidden = inspect_layered_overlay_state(gpui_win.0, thumb_host.hwnd());
        assert!(!hidden.gpui_visible);
        assert!(!hidden.host_visible);
    }

    #[test]
    fn pin_layered_z_order_recovers_from_intervening_window_and_hidden_host() {
        let _guard = OVERLAY_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let gpui_win = TestHwnd::create_hidden_popup(800, 600);
        configure_borderless_immersive(gpui_win.0, false);
        let thumb_host = ThumbnailHostWindow::create(false).expect("ThumbnailHostWindow::create");
        let interloper = TestHwnd::create_hidden_popup(640, 480);

        // Use non-fullscreen bounds here so parallel test processes cannot trigger Windows'
        // fullscreen-window WS_EX_TOPMOST auto-demotion mid-assertion.
        let (rect, _scale) = show_and_focus_layered_overlay(
            gpui_win.0,
            thumb_host.hwnd(),
            OverlayMonitorPolicy::Primary,
            false,
        )
        .expect("show_and_focus_layered_overlay");
        let after_show = inspect_layered_overlay_state(gpui_win.0, thumb_host.hwnd());
        assert!(
            after_show.is_healthy(),
            "show_and_focus_layered_overlay must promote both layers to topmost even with a sibling popup: {after_show:?}"
        );

        // Deliberately push thumb_host to bottom and wedge a visible window directly beneath gpui_win.
        unsafe {
            let _ = SetWindowPos(
                thumb_host.hwnd(),
                Some(HWND_BOTTOM),
                0,
                0,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
            );
            let _ = SetWindowPos(
                interloper.0,
                Some(gpui_win.0),
                rect.left,
                rect.top,
                640,
                480,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        }

        let broken_z = inspect_layered_overlay_state(gpui_win.0, thumb_host.hwnd());
        assert!(
            !broken_z.host_directly_behind_gpui,
            "interloper should break direct Z-adjacency before recovery"
        );
        assert!(!broken_z.is_healthy());

        pin_layered_z_order(gpui_win.0, thumb_host.hwnd(), rect);
        let recovered_z = inspect_layered_overlay_state(gpui_win.0, thumb_host.hwnd());
        assert!(
            recovered_z.host_directly_behind_gpui && recovered_z.is_healthy(),
            "pin_layered_z_order must restore strict Z-order lockstep: {recovered_z:?}"
        );

        // Deliberately hide thumb_host (simulating STARTUPINFO SW_HIDE override on first ShowWindow).
        unsafe {
            let _ = ShowWindow(thumb_host.hwnd(), SW_HIDE);
        }
        let broken_vis = inspect_layered_overlay_state(gpui_win.0, thumb_host.hwnd());
        assert!(!broken_vis.host_visible);
        assert!(!broken_vis.is_healthy());

        pin_layered_z_order(gpui_win.0, thumb_host.hwnd(), rect);
        thumb_host.force_reconfigure(false, BackdropStyle::Blurred, 0.25);
        let recovered_vis = inspect_layered_overlay_state(gpui_win.0, thumb_host.hwnd());
        assert!(
            recovered_vis.host_visible
                && recovered_vis.host_directly_behind_gpui
                && recovered_vis.is_healthy(),
            "pin_layered_z_order must recover a hidden host window: {recovered_vis:?}"
        );

        hide_layered_overlay(gpui_win.0, thumb_host.hwnd());
    }
}
