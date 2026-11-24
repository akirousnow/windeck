use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui::{
    App, Bounds, ClickEvent, Context, FocusHandle, Focusable, KeyDownEvent, MouseButton, Pixels,
    RenderImage, ScrollDelta, ScrollWheelEvent, SharedString, Window, WindowBackgroundAppearance,
    canvas, div, img, point, prelude::*, px, rgb, rgba, size,
};
use smallvec::smallvec;
use win_platform::{
    DesktopId, DwmTransitionSuppressGuard, ForceQuitOutcome, HoldNavDirection, IconBgra,
    MonitorInfo, PreRestoreResult, ReleaseInfo, ShellHost, SingleInstanceGuard,
    ThumbnailHostWindow, VirtualDesktopClient, VirtualDesktopInfo, WindowInfo,
    activate_window_foreground, configure_borderless_immersive, enumerate_monitors,
    enumerate_windows, extract_window_icon, force_quit_window, hide_layered_overlay,
    inspect_layered_overlay_state, pin_layered_z_order, pre_restore_minimized_window,
    query_window_and_client_sizes, refresh_window_geometry, show_and_focus_layered_overlay,
    switch_desktop_and_wait, switch_to_window,
};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::WindowsAndMessaging::{
    GA_ROOTOWNER, GetAncestor, GetForegroundWindow, GetShellWindow, GetWindowThreadProcessId,
    IsWindow, SetForegroundWindow,
};

use crate::animation::{
    CrossDesktopSample, ExitTransition, ExitTransitionKind, LeavingCardSnapshot,
    SelectionTransition, TabTransition, TransitionProfile, bounds_to_phys_rect, ease_out_cubic,
    exp_damp, lerp_bounds, resolve_exit_target_phys_rect, sample_cross_desktop_window,
    scale_rect_around_center,
};
use crate::config::{AppConfig, LabelPosition, PreviewFit, append_log};
use crate::nav::{NavDirection, navigate};
use crate::thumb_manager::ThumbnailManager;
use crate::update_dialog::UpdateStatus;
use crate::window_order::{SelectionCard, StableWindowOrder, pick_initial_selection_index};

// Balanced, breathable macOS Mission Control spatial constants (logical px)
const TOP_SPACES_BAR_H: f32 = 58.0;
const CANVAS_PAD_X: f32 = 52.0;
const CANVAS_MAX_W: f32 = 2280.0;
const CONTENT_PAD_TOP: f32 = 14.0;
const CONTENT_PAD_BOTTOM: f32 = 36.0;
const GROUP_HEADER_H: f32 = 28.0;
const GROUP_TOP_GAP: f32 = 28.0;
const OUTER_GLOW_PAD: f32 = 4.0;
const MID_GLOW_PAD: f32 = 3.0;
const INNER_RING_PAD: f32 = 2.5;
const TOTAL_RING_PAD: f32 = OUTER_GLOW_PAD + MID_GLOW_PAD + INNER_RING_PAD;
const BELOW_LABEL_GAP: f32 = 8.0;
const BELOW_LABEL_H: f32 = 26.0;
const ROW_GAP: f32 = 22.0;
const COL_GAP: f32 = 24.0;
const EMPTY_GROUP_H: f32 = 38.0;

#[derive(Clone)]
struct WindowCard {
    flat_index: usize,
    info: WindowInfo,
    icon: Option<Arc<RenderImage>>,
    monogram: SharedString,
    accent_color: u32,
}

#[derive(Clone)]
struct DesktopGroup {
    title: SharedString,
    desktop_id: Option<DesktopId>,
    is_current: bool,
    cards: Vec<WindowCard>,
}

/// Deterministic logical coordinates for a single card in the current grid layout.
#[derive(Clone, Copy)]
struct CardLayoutRect {
    /// Outer 3-ring focus frame bounds inside the viewport before subtracting `scroll_y`.
    ring_content_bounds: Bounds<Pixels>,
    /// Natural (un-offset) `thumb_pane` bounds in GPUI window coordinates at `scroll_y`.
    thumb_window_bounds: Bounds<Pixels>,
}

fn is_window_alive_with_pid(hwnd_raw: isize, expected_pid: u32) -> bool {
    let hwnd = HWND(hwnd_raw as *mut std::ffi::c_void);
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return false;
    }
    if expected_pid == 0 {
        return true;
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid == expected_pid
}

fn icon_bgra_to_render_image(icon: IconBgra) -> Option<Arc<RenderImage>> {
    let rgba_buf = image::RgbaImage::from_raw(icon.width, icon.height, icon.bgra)?;
    let frame = image::Frame::new(rgba_buf);
    Some(Arc::new(RenderImage::new(smallvec![frame])))
}

fn monogram_and_color(process_name: &str, title: &str) -> (SharedString, u32) {
    let source = if !process_name.is_empty() {
        process_name
    } else {
        title
    };
    let ch = source
        .chars()
        .find(|c| c.is_alphanumeric())
        .unwrap_or('W')
        .to_uppercase()
        .to_string();
    let palette = [
        0x0A84FF, 0x30D158, 0xFF9F0A, 0xFF375F, 0xBF5AF2, 0x64D2FF, 0xFF453A, 0x5E5CE6,
    ];
    let hash: usize = source
        .bytes()
        .fold(0usize, |acc, b| acc.wrapping_mul(31).wrapping_add(b as usize));
    (SharedString::from(ch), palette[hash % palette.len()])
}

/// Crisp geometric trash-can icon built from GPUI primitives (DPI-sharp, zero font/emoji fallback issues).
fn trash_icon() -> gpui::Div {
    div()
        .w(px(12.0))
        .h(px(13.0))
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        // Lid handle
        .child(
            div()
                .w(px(4.5))
                .h(px(1.5))
                .rounded_t(px(1.0))
                .bg(rgb(0xFFFFFF)),
        )
        // Lid bar
        .child(
            div()
                .w(px(11.0))
                .h(px(1.5))
                .rounded(px(1.0))
                .bg(rgb(0xFFFFFF)),
        )
        // Bin body with two inner ribs
        .child(
            div()
                .mt(px(1.0))
                .w(px(9.0))
                .h(px(8.5))
                .rounded_b(px(2.0))
                .border_l(px(1.3))
                .border_r(px(1.3))
                .border_b(px(1.3))
                .border_color(rgb(0xFFFFFF))
                .flex()
                .flex_row()
                .items_center()
                .justify_center()
                .gap(px(2.0))
                .child(div().w(px(1.0)).h(px(4.5)).rounded(px(0.5)).bg(rgba(0xFFFFFFDC)))
                .child(div().w(px(1.0)).h(px(4.5)).rounded(px(0.5)).bg(rgba(0xFFFFFFDC))),
        )
}

pub struct SwitcherView {
    pub(crate) focus_handle: FocusHandle,
    pub(crate) gpui_hwnd: HWND,
    pub(crate) thumb_host: ThumbnailHostWindow,
    vdm: Arc<VirtualDesktopClient>,
    pub(crate) shell_host: Arc<Option<ShellHost>>,
    pub(crate) single_instance_guard: Arc<Mutex<Option<SingleInstanceGuard>>>,
    pub(crate) config: AppConfig,
    bench_mode: bool,
    pub(crate) effective_version: String,
    pub(crate) update_status: UpdateStatus,
    pub(crate) pending_update: Option<ReleaseInfo>,
    pub(crate) update_etag: Option<String>,
    pub(crate) update_check_in_flight: bool,
    pub(crate) last_update_check: Option<Instant>,
    pub(crate) dismissed_update_tag: Option<String>,
    pub(crate) notified_balloon_tag: Option<String>,
    pub(crate) update_dialog_open: bool,
    pub(crate) update_dialog_error: Option<SharedString>,
    pub(crate) visible: bool,
    suppress_blur_hide: bool,
    has_completed_first_open: bool,
    pub(crate) last_overlay_rect: Option<RECT>,
    shown_at: Option<Instant>,
    pub(crate) hold_active: bool,
    pub(crate) hold_navigated: bool,
    prev_foreground_hwnd: isize,
    prev_foreground_root_hwnd: isize,
    desktops: Vec<VirtualDesktopInfo>,
    monitors: Vec<MonitorInfo>,
    all_windows: Vec<WindowInfo>,
    window_order: StableWindowOrder,
    desktop_filter: Option<DesktopId>,
    groups: Vec<DesktopGroup>,
    pub(crate) flat_hwnds: Vec<isize>,
    selected_flat: usize,
    hovered_flat: Option<usize>,
    pub(crate) armed_delete_desktop: Option<DesktopId>,
    pub(crate) settings_open: bool,
    pub(crate) recording_hotkey_letter: bool,
    pub(crate) recording_force_quit_key: bool,
    pub(crate) settings_error: Option<SharedString>,
    pub(crate) win_arrow_patch_error: Option<SharedString>,
    pub(crate) settings_scroll_y: f32,
    pub(crate) autostart_enabled: bool,
    scroll_y: f32,
    scroll_target_y: f32,
    scroll_last_tick: Option<Instant>,
    pub(crate) overlay_phys_rect: RECT,
    pub(crate) overlay_scale: f32,
    viewport_clip: Rc<RefCell<Option<Bounds<Pixels>>>>,
    last_thumb_bounds: Rc<RefCell<HashMap<isize, Bounds<Pixels>>>>,
    pub(crate) viewport_width_px: Rc<Cell<f32>>,
    pub(crate) viewport_height_px: Rc<Cell<f32>>,
    icon_cache: HashMap<String, Option<Arc<RenderImage>>>,
    pub(crate) thumbs: Rc<RefCell<ThumbnailManager>>,
    tab_anim: Option<TabTransition>,
    selection_anim: Option<SelectionTransition>,
    exit_anim: Option<ExitTransition>,
    last_open_ms: f64,
    last_enum_ms: f64,
    total_window_count: usize,
    pub(crate) hotkey_warning: Option<SharedString>,
    pub(crate) transient_toast: Option<(SharedString, Instant)>,
}

impl Focusable for SwitcherView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl SwitcherView {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        gpui_hwnd: HWND,
        thumb_host: ThumbnailHostWindow,
        vdm: Arc<VirtualDesktopClient>,
        shell_host: Arc<Option<ShellHost>>,
        single_instance_guard: Arc<Mutex<Option<SingleInstanceGuard>>>,
        config: AppConfig,
        hotkey_warning: Option<String>,
        effective_version: String,
        bench_mode: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle);
        let thumbs = ThumbnailManager::new(thumb_host.hwnd());

        Self {
            focus_handle,
            gpui_hwnd,
            thumb_host,
            vdm,
            shell_host,
            single_instance_guard,
            config,
            bench_mode,
            effective_version,
            update_status: UpdateStatus::Idle,
            pending_update: None,
            update_etag: None,
            update_check_in_flight: false,
            last_update_check: None,
            dismissed_update_tag: None,
            notified_balloon_tag: None,
            update_dialog_open: false,
            update_dialog_error: None,
            visible: false,
            suppress_blur_hide: false,
            has_completed_first_open: false,
            last_overlay_rect: None,
            shown_at: None,
            hold_active: false,
            hold_navigated: false,
            prev_foreground_hwnd: 0,
            prev_foreground_root_hwnd: 0,
            desktops: Vec::new(),
            monitors: Vec::new(),
            all_windows: Vec::new(),
            window_order: StableWindowOrder::new(),
            desktop_filter: None,
            groups: Vec::new(),
            flat_hwnds: Vec::new(),
            selected_flat: 0,
            hovered_flat: None,
            armed_delete_desktop: None,
            settings_open: false,
            recording_hotkey_letter: false,
            recording_force_quit_key: false,
            settings_error: None,
            win_arrow_patch_error: None,
            settings_scroll_y: 0.0,
            autostart_enabled: false,
            scroll_y: 0.0,
            scroll_target_y: 0.0,
            scroll_last_tick: None,
            overlay_phys_rect: RECT::default(),
            overlay_scale: 1.0,
            viewport_clip: Rc::new(RefCell::new(None)),
            last_thumb_bounds: Rc::new(RefCell::new(HashMap::new())),
            viewport_width_px: Rc::new(Cell::new(2560.0)),
            viewport_height_px: Rc::new(Cell::new(1380.0)),
            icon_cache: HashMap::new(),
            thumbs: Rc::new(RefCell::new(thumbs)),
            tab_anim: None,
            selection_anim: None,
            exit_anim: None,
            last_open_ms: 0.0,
            last_enum_ms: 0.0,
            total_window_count: 0,
            hotkey_warning: hotkey_warning.map(SharedString::from),
            transient_toast: None,
        }
    }

    pub fn update_config(
        &mut self,
        new_config: AppConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        configure_borderless_immersive(self.gpui_hwnd, new_config.immersive);
        self.thumb_host.configure(
            new_config.immersive,
            new_config.background,
            new_config.blur_opacity,
        );
        window.set_background_appearance(WindowBackgroundAppearance::Transparent);
        self.config = new_config;
        cx.notify();
    }

    pub fn handle_explorer_restarted(&mut self) {
        let res = self.vdm.reconnect();
        append_log(&format!("Explorer restarted -> VDM reconnect: {:?}", res));
    }

    /// Schedules a repaint of `SwitcherView` on the next DWM vsync frame without calling
    /// `window.request_animation_frame()` (which in GPUI 0.2.2 calls `current_view()` and panics
    /// outside element layout/prepaint/paint callbacks).
    fn schedule_next_frame(window: &Window, cx: &Context<Self>) {
        let entity = cx.entity_id();
        window.on_next_frame(move |_, app| app.notify(entity));
    }

    pub(crate) fn clear_all_animations(&mut self, prune_thumbs: bool) {
        self.tab_anim = None;
        self.selection_anim = None;
        self.exit_anim = None;
        self.scroll_y = self.scroll_target_y;
        self.scroll_last_tick = None;
        if prune_thumbs {
            self.thumbs.borrow_mut().retain_targets(&self.flat_hwnds);
        }
    }

    pub fn handle_window_activation_changed(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.visible
            || !self.config.hide_on_blur
            || self.suppress_blur_hide
            || self.is_downloading_update()
            || self.exit_anim.is_some()
        {
            return;
        }
        if !window.is_window_active() {
            if let Some(shown_at) = self.shown_at {
                if shown_at.elapsed() < Duration::from_millis(180) {
                    return;
                }
            }
            self.hide(cx);
        }
    }

    fn sync_visibility_to_host(&self) {
        if let Some(ref host) = *self.shell_host {
            host.set_overlay_visible(self.visible);
        }
    }

    pub(crate) fn disarm_hold_mode(&mut self) {
        self.hold_active = false;
        self.hold_navigated = false;
        if let Some(ref host) = *self.shell_host {
            host.disarm_hold_mode();
        }
    }

    pub fn toggle_with_hold(&mut self, hold_armed: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.exit_anim.is_some() {
            self.finish_exit_transition(cx);
            return;
        }
        if self.visible {
            if !self.is_downloading_update() {
                self.hide(cx);
            }
        } else {
            self.show_and_refresh_inner(window, hold_armed, cx);
            self.hold_active = hold_armed;
            self.hold_navigated = false;
            cx.notify();
        }
    }

    pub fn handle_hold_navigate(
        &mut self,
        dir: HoldNavDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.visible || self.exit_anim.is_some() {
            return;
        }
        self.armed_delete_desktop = None;
        self.hold_active = true;
        self.hold_navigated = true;

        let nav_dir = match dir {
            HoldNavDirection::Left => NavDirection::Left,
            HoldNavDirection::Right => NavDirection::Right,
            HoldNavDirection::Up => NavDirection::Up,
            HoldNavDirection::Down => NavDirection::Down,
        };
        let (cols, _, _, _, _) = self.fixed_grid_geometry();
        let sizes = self.group_sizes();
        let next_flat = navigate(
            &sizes,
            cols,
            self.selected_flat,
            nav_dir,
            self.config.cross_desktop_nav,
        );
        self.move_selection(next_flat, window, cx);
    }

    pub fn handle_hold_modifier_released(
        &mut self,
        commit: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.exit_anim.is_some() {
            self.hold_active = false;
            self.hold_navigated = false;
            return;
        }
        let should_jump = self.visible && commit && self.hold_navigated;
        self.hold_active = false;
        self.hold_navigated = false;
        if should_jump {
            if let Some(&hwnd) = self.flat_hwnds.get(self.selected_flat) {
                self.activate_window(hwnd, window, cx);
            } else if let Some(did) = self.desktop_filter {
                self.switch_to_desktop_only(did, window, cx);
            } else {
                cx.notify();
            }
        } else {
            if self.visible && !self.update_dialog_open {
                if let Some(ref rel) = self.pending_update {
                    if self.dismissed_update_tag.as_deref() != Some(rel.tag_name.as_str()) {
                        self.open_update_dialog(cx);
                        return;
                    }
                }
            }
            cx.notify();
        }
    }

    pub fn hide(&mut self, cx: &mut Context<Self>) {
        self.clear_all_animations(false);
        self.thumbs.borrow_mut().unregister_all();
        self.last_thumb_bounds.borrow_mut().clear();
        hide_layered_overlay(self.gpui_hwnd, self.thumb_host.hwnd());
        self.visible = false;
        self.shown_at = None;
        self.hold_active = false;
        self.hold_navigated = false;
        self.hovered_flat = None;
        self.armed_delete_desktop = None;
        self.settings_open = false;
        self.update_dialog_open = false;
        self.recording_hotkey_letter = false;
        self.recording_force_quit_key = false;
        self.settings_error = None;
        self.win_arrow_patch_error = None;
        self.transient_toast = None;
        self.sync_visibility_to_host();
        cx.notify();
    }

    pub fn show_and_refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_and_refresh_inner(window, false, cx);
    }

    fn capture_foreground_candidates(&mut self) {
        let fg_hwnd = unsafe { GetForegroundWindow() };
        let fg_raw = fg_hwnd.0 as isize;
        let self_gpui = self.gpui_hwnd.0 as isize;
        let self_host = self.thumb_host.hwnd().0 as isize;
        if fg_raw == self_gpui || fg_raw == self_host {
            return;
        }
        self.prev_foreground_hwnd = fg_raw;
        if fg_raw == 0 {
            self.prev_foreground_root_hwnd = 0;
            return;
        }
        let root_raw = unsafe { GetAncestor(fg_hwnd, GA_ROOTOWNER) }.0 as isize;
        self.prev_foreground_root_hwnd = if root_raw != 0
            && root_raw != fg_raw
            && root_raw != self_gpui
            && root_raw != self_host
        {
            root_raw
        } else {
            0
        };
    }

    fn show_and_refresh_inner(
        &mut self,
        window: &mut Window,
        hold_armed: bool,
        cx: &mut Context<Self>,
    ) {
        let t_start = Instant::now();
        self.capture_foreground_candidates();
        self.hovered_flat = None;
        self.armed_delete_desktop = None;
        self.settings_open = false;
        self.update_dialog_open = false;
        self.recording_hotkey_letter = false;
        self.recording_force_quit_key = false;
        self.settings_error = None;
        self.hold_active = false;
        self.hold_navigated = false;
        self.clear_all_animations(false);
        self.last_thumb_bounds.borrow_mut().clear();
        self.scroll_y = 0.0;
        self.scroll_target_y = 0.0;
        self.scroll_last_tick = None;
        *self.viewport_clip.borrow_mut() = None;
        // Always open in full panorama ("全部") so all desktops' windows are visible at once
        self.desktop_filter = None;

        self.refresh_snapshot();

        let prompt_cached_update = !self.bench_mode
            && !hold_armed
            && self.pending_update.as_ref().is_some_and(|rel| {
                self.dismissed_update_tag.as_deref() != Some(rel.tag_name.as_str())
            });
        if prompt_cached_update {
            self.update_dialog_open = true;
        }

        self.visible = true;
        self.sync_visibility_to_host();
        self.shown_at = Some(Instant::now());
        window.set_background_appearance(WindowBackgroundAppearance::Transparent);

        // Reveal and size both layers in lockstep Z-order BEFORE applying DWM Acrylic backdrop
        // and registering DWM thumbnails, so cold-start Layer 1 is never initialized while hidden at 1x1.
        if let Ok((phys_rc, scale)) = show_and_focus_layered_overlay(
            self.gpui_hwnd,
            self.thumb_host.hwnd(),
            self.config.monitor,
            self.config.immersive,
        ) {
            self.overlay_phys_rect = phys_rc;
            self.overlay_scale = scale;
            self.last_overlay_rect = Some(phys_rc);
            let logical_w = ((phys_rc.right - phys_rc.left) as f32 / scale).max(640.0);
            let logical_h = ((phys_rc.bottom - phys_rc.top) as f32 / scale).max(420.0);
            self.viewport_width_px.set(logical_w);
            self.viewport_height_px
                .set((logical_h - TOP_SPACES_BAR_H).max(260.0));
        }

        self.thumb_host.configure(
            self.config.immersive,
            self.config.background,
            self.config.blur_opacity,
        );

        if prompt_cached_update {
            self.thumbs.borrow_mut().unregister_all();
        } else {
            self.thumbs.borrow_mut().sync_targets(&self.flat_hwnds);
        }

        window.focus(&self.focus_handle);
        self.ensure_selected_visible_instant();
        window.refresh();

        let diag = inspect_layered_overlay_state(self.gpui_hwnd, self.thumb_host.hwnd());
        self.last_open_ms = t_start.elapsed().as_secs_f64() * 1000.0;
        append_log(&format!(
            "OPEN total={:.2}ms enum={:.2}ms desktops={} windows={} thumbs={} fg=0x{:X} sel={} preview={}x{} fit={} label={} host_vis={} host_z={} host_top={} gpui_top={} gpui_fg={} host_cloak={} host_sz={}x{}",
            self.last_open_ms,
            self.last_enum_ms,
            self.desktops.len(),
            self.total_window_count,
            self.thumbs.borrow().active_count(),
            self.prev_foreground_hwnd,
            self.selected_flat,
            self.config.preview_width as i32,
            self.config.preview_height as i32,
            self.config.preview_fit.as_config_str(),
            self.config.label_position.as_config_str(),
            diag.host_visible,
            diag.host_directly_behind_gpui,
            diag.host_topmost,
            diag.gpui_topmost,
            diag.gpui_foreground,
            diag.host_cloaked,
            diag.host_size.0,
            diag.host_size.1,
        ));

        if !self.has_completed_first_open && !self.bench_mode {
            self.has_completed_first_open = true;
            let window_handle = Window::window_handle(window);
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
                let _ = cx.update(|cx| {
                    let _ = window_handle.update(cx, |_, window, cx| {
                        let _ = this.update(cx, |v, cx| {
                            v.run_first_open_settle(window, cx);
                        });
                    });
                });
            })
            .detach();
        }

        if !self.bench_mode && self.config.auto_update {
            self.trigger_update_check(false, cx);
        }

        cx.notify();
    }

    fn run_first_open_settle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.visible || self.exit_anim.is_some() {
            return;
        }
        let before = inspect_layered_overlay_state(self.gpui_hwnd, self.thumb_host.hwnd());
        if let Some(rect) = self.last_overlay_rect {
            pin_layered_z_order(self.gpui_hwnd, self.thumb_host.hwnd(), rect);
        }
        if !before.is_healthy() {
            self.thumb_host.force_reconfigure(
                self.config.immersive,
                self.config.background,
                self.config.blur_opacity,
            );
            if !self.settings_open && !self.update_dialog_open {
                self.tab_anim = None;
                self.thumbs.borrow_mut().unregister_all();
                self.thumbs.borrow_mut().sync_targets(&self.flat_hwnds);
            }
        } else {
            self.thumb_host.configure(
                self.config.immersive,
                self.config.background,
                self.config.blur_opacity,
            );
            if !self.settings_open && !self.update_dialog_open {
                if self.tab_anim.is_some() {
                    self.thumbs.borrow_mut().ensure_targets(&self.flat_hwnds);
                } else {
                    self.thumbs.borrow_mut().sync_targets(&self.flat_hwnds);
                }
            }
        }
        window.refresh();
        cx.notify();
        let after = inspect_layered_overlay_state(self.gpui_hwnd, self.thumb_host.hwnd());
        append_log(&format!(
            "OPEN_SETTLED healthy_before={} healthy_after={} host_vis={} host_z={} host_top={} gpui_top={} gpui_fg={} host_cloak={} host_sz={}x{} thumbs={}",
            before.is_healthy(),
            after.is_healthy(),
            after.host_visible,
            after.host_directly_behind_gpui,
            after.host_topmost,
            after.gpui_topmost,
            after.gpui_foreground,
            after.host_cloaked,
            after.host_size.0,
            after.host_size.1,
            self.thumbs.borrow().active_count(),
        ));
    }

    pub fn refresh_snapshot(&mut self) {
        let t_enum = Instant::now();
        let desktops = self.vdm.get_desktops().unwrap_or_default();
        let monitors = enumerate_monitors();
        let exclude = [self.gpui_hwnd.0 as isize, self.thumb_host.hwnd().0 as isize];
        let windows =
            enumerate_windows(&self.vdm, &desktops, &monitors, &exclude).unwrap_or_default();
        self.last_enum_ms = t_enum.elapsed().as_secs_f64() * 1000.0;

        // If filtered/armed desktop was removed externally, reset cleanly
        if let Some(fid) = self.desktop_filter {
            if !desktops.iter().any(|d| d.id == fid) {
                self.desktop_filter = None;
            }
        }
        if let Some(aid) = self.armed_delete_desktop {
            if !desktops.iter().any(|d| d.id == aid) {
                self.armed_delete_desktop = None;
            }
        }

        self.desktops = desktops;
        self.monitors = monitors;
        self.all_windows = windows;
        self.window_order.sync(
            &self.all_windows,
            &self.desktops,
            is_window_alive_with_pid,
        );
        self.rebuild_visible_groups(true);
    }

    /// Rebuilds visible desktop groups & flat HWND index list according to `self.desktop_filter`.
    fn rebuild_visible_groups(&mut self, pick_smart_initial: bool) {
        let desktops = self.desktops.clone();
        let windows = self.window_order.ordered(&self.all_windows);
        let filter = self.desktop_filter;

        let mut pinned_windows = Vec::new();
        let mut by_desktop: Vec<Vec<WindowInfo>> = vec![Vec::new(); desktops.len()];
        let mut unassigned = Vec::new();

        for w in windows {
            if w.is_pinned {
                pinned_windows.push(w);
            } else if let Some(idx) = w.desktop_index {
                if idx < by_desktop.len() {
                    by_desktop[idx].push(w);
                } else {
                    unassigned.push(w);
                }
            } else {
                unassigned.push(w);
            }
        }

        let mut groups = Vec::new();
        let mut flat_hwnds = Vec::new();
        let mut flat_idx = 0usize;

        if filter.is_none() && !pinned_windows.is_empty() {
            let mut cards = Vec::with_capacity(pinned_windows.len());
            for info in pinned_windows.clone() {
                let icon = self.cached_icon(&info);
                let (monogram, accent_color) = monogram_and_color(&info.process_name, &info.title);
                flat_hwnds.push(info.hwnd);
                cards.push(WindowCard {
                    flat_index: flat_idx,
                    info,
                    icon,
                    monogram,
                    accent_color,
                });
                flat_idx += 1;
            }
            groups.push(DesktopGroup {
                title: SharedString::from("所有桌面"),
                desktop_id: None,
                is_current: false,
                cards,
            });
        }

        for (d_idx, d_info) in desktops.iter().enumerate() {
            if let Some(fid) = filter {
                if d_info.id != fid {
                    continue;
                }
            }
            let label = d_info
                .custom_name
                .clone()
                .unwrap_or_else(|| format!("桌面 {}", d_idx + 1));
            let label_shared = SharedString::from(label);
            let mut cards = Vec::with_capacity(by_desktop[d_idx].len());
            for info in std::mem::take(&mut by_desktop[d_idx]) {
                let icon = self.cached_icon(&info);
                let (monogram, accent_color) = monogram_and_color(&info.process_name, &info.title);
                flat_hwnds.push(info.hwnd);
                cards.push(WindowCard {
                    flat_index: flat_idx,
                    info,
                    icon,
                    monogram,
                    accent_color,
                });
                flat_idx += 1;
            }
            // When filtering a single desktop, also include pinned windows ("shown on all desktops")
            if filter.is_some() {
                for info in &pinned_windows {
                    let icon = self.cached_icon(info);
                    let (monogram, accent_color) =
                        monogram_and_color(&info.process_name, &info.title);
                    flat_hwnds.push(info.hwnd);
                    cards.push(WindowCard {
                        flat_index: flat_idx,
                        info: info.clone(),
                        icon,
                        monogram,
                        accent_color,
                    });
                    flat_idx += 1;
                }
            }
            groups.push(DesktopGroup {
                title: label_shared,
                desktop_id: Some(d_info.id),
                is_current: d_info.is_current,
                cards,
            });
        }

        if filter.is_none() && !unassigned.is_empty() {
            let mut cards = Vec::with_capacity(unassigned.len());
            for info in unassigned {
                let icon = self.cached_icon(&info);
                let (monogram, accent_color) = monogram_and_color(&info.process_name, &info.title);
                flat_hwnds.push(info.hwnd);
                cards.push(WindowCard {
                    flat_index: flat_idx,
                    info,
                    icon,
                    monogram,
                    accent_color,
                });
                flat_idx += 1;
            }
            groups.push(DesktopGroup {
                title: SharedString::from("其他"),
                desktop_id: None,
                is_current: false,
                cards,
            });
        }

        self.total_window_count = flat_hwnds.len();
        self.groups = groups;
        self.flat_hwnds = flat_hwnds;
        if pick_smart_initial {
            self.selected_flat = self.pick_initial_selection();
        } else if self.flat_hwnds.is_empty() {
            self.selected_flat = 0;
        } else {
            self.selected_flat = self.selected_flat.min(self.flat_hwnds.len() - 1);
        }
    }

    fn tab_order_index(&self, filter: Option<DesktopId>) -> usize {
        match filter {
            None => 0,
            Some(did) => self
                .desktops
                .iter()
                .position(|d| d.id == did)
                .map(|i| i + 1)
                .unwrap_or(0),
        }
    }

    fn set_desktop_filter(
        &mut self,
        new_filter: Option<DesktopId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.exit_anim.is_some() {
            return;
        }
        self.disarm_hold_mode();
        if self.desktop_filter == new_filter {
            return;
        }

        let animate = self.config.tab_transition_enabled() && self.visible;
        let old_idx = self.tab_order_index(self.desktop_filter);
        let new_idx = self.tab_order_index(new_filter);
        let slide_dir = if new_idx >= old_idx { 1.0 } else { -1.0 };

        // Snapshot current cards' on-screen bounds BEFORE mutating desktop_filter/groups
        let old_cards_snapshot: Vec<LeavingCardSnapshot> = if animate {
            let (cols, preview_w, preview_h, tile_outer_w, grid_total_w) =
                self.fixed_grid_geometry();
            let (total_h, _) = self.compute_layout_metrics(cols, preview_h);
            let view_h = self.viewport_height_px.get().max(100.0);
            let max_scroll = (total_h - view_h).max(0.0);
            let optical_top_pad = Self::compute_optical_top_pad(max_scroll, total_h, view_h);
            let vp_origin_y = self
                .viewport_clip
                .borrow()
                .map(|b| f32::from(b.origin.y))
                .unwrap_or(TOP_SPACES_BAR_H);
            let old_layouts = self.compute_card_layout_rects(
                cols,
                preview_w,
                preview_h,
                tile_outer_w,
                grid_total_w,
                optical_top_pad,
                self.scroll_y,
                vp_origin_y,
            );
            let cached_bounds = self.last_thumb_bounds.borrow();
            let thumbs = self.thumbs.borrow();

            let mut snap = Vec::with_capacity(self.flat_hwnds.len());
            for g in &self.groups {
                for card in &g.cards {
                    let hwnd = card.info.hwnd;
                    let bounds = cached_bounds
                        .get(&hwnd)
                        .copied()
                        .or_else(|| old_layouts.get(&card.flat_index).map(|r| r.thumb_window_bounds));
                    if let Some(thumb_bounds) = bounds {
                        let fallback_label = if card.info.process_name.is_empty() {
                            SharedString::from("已最小化")
                        } else {
                            SharedString::from(card.info.process_name.clone())
                        };
                        snap.push(LeavingCardSnapshot {
                            hwnd,
                            thumb_bounds,
                            has_live_surface: thumbs.source_size(hwnd).is_some(),
                            icon: card.icon.clone(),
                            fallback_label,
                        });
                    }
                }
            }
            snap
        } else {
            Vec::new()
        };

        self.desktop_filter = new_filter;
        self.scroll_y = 0.0;
        self.scroll_target_y = 0.0;
        self.scroll_last_tick = None;
        self.selection_anim = None;
        self.hovered_flat = None;
        self.selected_flat = 0;
        self.rebuild_visible_groups(false);

        if animate && (!old_cards_snapshot.is_empty() || !self.flat_hwnds.is_empty()) {
            let new_set: HashSet<isize> = self.flat_hwnds.iter().copied().collect();
            let mut old_set: HashSet<isize> = HashSet::with_capacity(old_cards_snapshot.len());
            let mut shared_from_bounds = HashMap::new();
            let mut leaving_cards = Vec::new();

            for item in old_cards_snapshot {
                old_set.insert(item.hwnd);
                if new_set.contains(&item.hwnd) {
                    shared_from_bounds.insert(item.hwnd, item.thumb_bounds);
                } else {
                    leaving_cards.push(item);
                }
            }

            let entering_hwnds: HashSet<isize> = self
                .flat_hwnds
                .iter()
                .copied()
                .filter(|h| !old_set.contains(h))
                .collect();

            // Keep both entering/shared AND currently leaving DWM thumbnails alive during fade-out,
            // while pruning any older interrupted transition leftovers.
            let mut keep_hwnds: Vec<isize> =
                Vec::with_capacity(self.flat_hwnds.len() + leaving_cards.len());
            keep_hwnds.extend_from_slice(&self.flat_hwnds);
            for lc in &leaving_cards {
                if lc.has_live_surface {
                    keep_hwnds.push(lc.hwnd);
                }
            }
            {
                let mut thumbs = self.thumbs.borrow_mut();
                thumbs.retain_targets(&keep_hwnds);
                thumbs.ensure_targets(&self.flat_hwnds);
            }

            let profile = TransitionProfile::from_effect(self.config.transition_effect);
            self.tab_anim = Some(TabTransition {
                started_at: Instant::now(),
                duration: profile.tab_duration,
                slide_dir,
                slide_px: profile.tab_slide_px,
                use_spring: profile.use_spring,
                shared_from_bounds,
                leaving_cards,
                entering_hwnds,
            });
        } else {
            self.tab_anim = None;
            // Immediately synchronize DWM thumbnails so filtered-out desktops release their surfaces (zero ghosts)
            self.thumbs.borrow_mut().sync_targets(&self.flat_hwnds);
            window.refresh();
        }
        cx.notify();
    }

    fn create_new_desktop(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.disarm_hold_mode();
        match self.vdm.create_desktop() {
            Ok(new_id) => {
                self.refresh_snapshot();
                self.thumbs.borrow_mut().sync_targets(&self.flat_hwnds);
                let new_label = self
                    .desktops
                    .iter()
                    .find(|d| d.id == new_id)
                    .map(|d| {
                        d.custom_name
                            .clone()
                            .unwrap_or_else(|| format!("桌面 {}", d.index + 1))
                    })
                    .unwrap_or_else(|| format!("桌面 {}", self.desktops.len()));
                self.transient_toast = Some((
                    SharedString::from(format!("已新建 {new_label}（双击顶部胶囊可直接切换）")),
                    Instant::now(),
                ));
                append_log(&format!(
                    "CREATE_DESKTOP id={} total_desktops={}",
                    new_id,
                    self.desktops.len()
                ));
                window.refresh();
                cx.notify();
            }
            Err(e) => {
                self.transient_toast = Some((
                    SharedString::from(format!("新建桌面失败：{e}")),
                    Instant::now(),
                ));
                append_log(&format!("CREATE_DESKTOP ERROR: {e:#}"));
                cx.notify();
            }
        }
    }

    /// If a desktop tab is currently armed as a trash-can button, cancels it and returns `true`.
    fn dismiss_armed_delete(&mut self, cx: &mut Context<Self>) -> bool {
        if self.armed_delete_desktop.take().is_some() {
            cx.notify();
            true
        } else {
            false
        }
    }

    fn remove_desktop_by_id(
        &mut self,
        desktop_id: DesktopId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.disarm_hold_mode();
        self.armed_delete_desktop = None;
        if self.desktops.len() <= 1 {
            self.transient_toast = Some((
                SharedString::from("至少需要保留 1 个虚拟桌面"),
                Instant::now(),
            ));
            cx.notify();
            return;
        }
        let Some(idx) = self.desktops.iter().position(|d| d.id == desktop_id) else {
            return;
        };
        let fallback_idx = if idx > 0 { idx - 1 } else { 1 };
        let fallback_id = self.desktops[fallback_idx].id;

        let removed_label = self.desktops[idx]
            .custom_name
            .clone()
            .unwrap_or_else(|| format!("桌面 {}", idx + 1));
        let fallback_label = self.desktops[fallback_idx]
            .custom_name
            .clone()
            .unwrap_or_else(|| format!("桌面 {}", fallback_idx + 1));
        let was_current = self.desktops[idx].is_current;
        let moved_windows_count = self
            .all_windows
            .iter()
            .filter(|w| !w.is_pinned && w.desktop_id == Some(desktop_id))
            .count();

        // Guard against transient focus loss while Explorer tears down the desktop
        self.shown_at = Some(Instant::now());

        match self.vdm.remove_desktop(desktop_id, fallback_id) {
            Ok(()) => {
                if self.desktop_filter == Some(desktop_id) {
                    self.desktop_filter = None;
                    self.scroll_y = 0.0;
                    self.scroll_target_y = 0.0;
                    self.scroll_last_tick = None;
                }
                self.tab_anim = None;
                self.selection_anim = None;
                if moved_windows_count > 0 || was_current {
                    std::thread::sleep(Duration::from_millis(45));
                }
                self.refresh_snapshot();
                self.thumbs.borrow_mut().sync_targets(&self.flat_hwnds);
                if was_current {
                    unsafe {
                        let _ = SetForegroundWindow(self.gpui_hwnd);
                    }
                    window.focus(&self.focus_handle);
                }
                let toast = if moved_windows_count > 0 {
                    format!(
                        "已关闭 {removed_label}，{moved_windows_count} 个窗口已移至 {fallback_label}"
                    )
                } else {
                    format!("已关闭 {removed_label}")
                };
                self.transient_toast = Some((SharedString::from(toast), Instant::now()));
                append_log(&format!(
                    "REMOVE_DESKTOP id={} fallback={} was_current={} moved={} total_desktops={}",
                    desktop_id,
                    fallback_id,
                    was_current,
                    moved_windows_count,
                    self.desktops.len()
                ));
                self.clamp_scroll();
                window.refresh();
                cx.notify();
            }
            Err(e) => {
                self.transient_toast = Some((
                    SharedString::from(format!("关闭桌面失败：{e}")),
                    Instant::now(),
                ));
                append_log(&format!("REMOVE_DESKTOP ERROR id={}: {e:#}", desktop_id));
                cx.notify();
            }
        }
    }

    fn pick_initial_selection(&self) -> usize {
        if self.flat_hwnds.is_empty() {
            return 0;
        }
        let cards: Vec<SelectionCard> = self
            .groups
            .iter()
            .flat_map(|g| {
                let in_current_desktop = g.is_current;
                g.cards.iter().map(move |c| SelectionCard {
                    hwnd: c.info.hwnd,
                    flat_index: c.flat_index,
                    in_current_desktop,
                    is_pinned: c.info.is_pinned,
                    is_cloaked: c.info.cloaked_flags != 0,
                })
            })
            .collect();
        pick_initial_selection_index(
            &self.all_windows,
            &cards,
            &[self.prev_foreground_hwnd, self.prev_foreground_root_hwnd],
        )
    }

    fn cached_icon(&mut self, info: &WindowInfo) -> Option<Arc<RenderImage>> {
        let is_uwp_host = info
            .process_name
            .eq_ignore_ascii_case("ApplicationFrameHost.exe")
            || info.exe_path.is_empty();
        let cache_key = if is_uwp_host {
            format!("hwnd:0x{:X}", info.hwnd)
        } else {
            info.exe_path.to_ascii_lowercase()
        };

        if let Some(cached) = self.icon_cache.get(&cache_key) {
            return cached.clone();
        }
        let decoded =
            extract_window_icon(info.hwnd, &info.exe_path).and_then(icon_bgra_to_render_image);
        self.icon_cache.insert(cache_key, decoded.clone());
        decoded
    }

    fn group_sizes(&self) -> Vec<usize> {
        self.groups.iter().map(|g| g.cards.len()).collect()
    }

    /// Returns `(cols, preview_w, preview_h, tile_outer_w, grid_total_w)`.
    /// Every window card uses the exact uniform fixed `(preview_w, preview_h)`.
    fn fixed_grid_geometry(&self) -> (usize, f32, f32, f32, f32) {
        let vw = self.viewport_width_px.get().max(640.0);
        let max_avail_w = (vw - CANVAS_PAD_X * 2.0).clamp(320.0, CANVAS_MAX_W);

        // Respect fixed configured preview size (clamped only if single card exceeds entire viewport)
        let preview_w = self
            .config
            .preview_width
            .min((max_avail_w - TOTAL_RING_PAD * 2.0).max(160.0));
        let preview_h = self.config.preview_height;
        let tile_outer_w = preview_w + TOTAL_RING_PAD * 2.0;

        let cols = self
            .config
            .resolve_columns(max_avail_w, tile_outer_w, COL_GAP);
        let grid_total_w =
            (cols as f32) * tile_outer_w + (cols.saturating_sub(1) as f32) * COL_GAP;

        (cols, preview_w, preview_h, tile_outer_w, grid_total_w)
    }

    fn extra_item_h(&self) -> f32 {
        if self.config.label_position.is_embedded() {
            TOTAL_RING_PAD * 2.0
        } else {
            TOTAL_RING_PAD * 2.0 + BELOW_LABEL_GAP + BELOW_LABEL_H
        }
    }

    fn compute_layout_metrics(
        &self,
        cols: usize,
        preview_h: f32,
    ) -> (f32, Option<(f32, f32)>) {
        let cols = cols.max(1);
        let item_h = preview_h + self.extra_item_h();
        let mut y = CONTENT_PAD_TOP;
        let mut selected_range = None;

        for (g_idx, group) in self.groups.iter().enumerate() {
            if g_idx > 0 {
                y += GROUP_TOP_GAP;
            }
            let group_header_top = y;
            y += GROUP_HEADER_H;

            if group.cards.is_empty() {
                y += EMPTY_GROUP_H;
                continue;
            }

            let rows = group.cards.len().div_ceil(cols);
            for r in 0..rows {
                let row_top = y;
                let row_bottom = row_top + item_h;
                let start = r * cols;
                let end = (start + cols).min(group.cards.len());
                for card in &group.cards[start..end] {
                    if card.flat_index == self.selected_flat {
                        let top = if r == 0 { group_header_top } else { row_top };
                        selected_range = Some((top, row_bottom));
                    }
                }
                y = row_bottom;
                if r + 1 < rows {
                    y += ROW_GAP;
                }
            }
        }

        y += CONTENT_PAD_BOTTOM;
        (y, selected_range)
    }

    fn compute_optical_top_pad(max_scroll: f32, total_content_h: f32, view_h: f32) -> f32 {
        if max_scroll <= 1.0 && total_content_h < view_h {
            ((view_h - total_content_h) * 0.32).min(76.0)
        } else {
            0.0
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn compute_card_layout_rects(
        &self,
        cols: usize,
        preview_w: f32,
        preview_h: f32,
        tile_outer_w: f32,
        grid_total_w: f32,
        optical_top_pad: f32,
        scroll_y: f32,
        vp_origin_y: f32,
    ) -> HashMap<usize, CardLayoutRect> {
        let cols = cols.max(1);
        let vw = self.viewport_width_px.get().max(640.0);
        let grid_left = ((vw - grid_total_w) * 0.5).max(CANVAS_PAD_X);
        let item_h = preview_h + self.extra_item_h();
        let ring_h = preview_h + TOTAL_RING_PAD * 2.0;

        let mut map = HashMap::with_capacity(self.flat_hwnds.len());
        let mut y = CONTENT_PAD_TOP;

        for (g_idx, group) in self.groups.iter().enumerate() {
            if g_idx > 0 {
                y += GROUP_TOP_GAP;
            }
            y += GROUP_HEADER_H;

            if group.cards.is_empty() {
                y += EMPTY_GROUP_H;
                continue;
            }

            let rows = group.cards.len().div_ceil(cols);
            for r in 0..rows {
                let row_top = y;
                let start = r * cols;
                let end = (start + cols).min(group.cards.len());
                for (c, card) in group.cards[start..end].iter().enumerate() {
                    let item_left = grid_left + (c as f32) * (tile_outer_w + COL_GAP);
                    let ring_content_y = optical_top_pad + row_top;
                    let ring_content_bounds = Bounds {
                        origin: point(px(item_left), px(ring_content_y)),
                        size: size(px(tile_outer_w), px(ring_h)),
                    };
                    let thumb_window_bounds = Bounds {
                        origin: point(
                            px(item_left + TOTAL_RING_PAD),
                            px(vp_origin_y + ring_content_y - scroll_y + TOTAL_RING_PAD),
                        ),
                        size: size(px(preview_w), px(preview_h)),
                    };
                    map.insert(
                        card.flat_index,
                        CardLayoutRect {
                            ring_content_bounds,
                            thumb_window_bounds,
                        },
                    );
                }
                y = row_top + item_h;
                if r + 1 < rows {
                    y += ROW_GAP;
                }
            }
        }

        map
    }

    pub(crate) fn clamp_scroll(&mut self) {
        let (cols, _, preview_h, _, _) = self.fixed_grid_geometry();
        let (total_h, _) = self.compute_layout_metrics(cols, preview_h);
        let view_h = self.viewport_height_px.get().max(100.0);
        let max_scroll = (total_h - view_h).max(0.0);
        self.scroll_target_y = self.scroll_target_y.clamp(0.0, max_scroll);
        self.scroll_y = self.scroll_y.clamp(0.0, max_scroll);
    }

    fn ensure_selected_visible_instant(&mut self) {
        self.ensure_selected_visible();
        self.scroll_y = self.scroll_target_y;
        self.scroll_last_tick = None;
    }

    fn ensure_selected_visible(&mut self) {
        let (cols, _, preview_h, _, _) = self.fixed_grid_geometry();
        let (total_h, selected_range) = self.compute_layout_metrics(cols, preview_h);
        let view_h = self.viewport_height_px.get().max(100.0);
        let max_scroll = (total_h - view_h).max(0.0);

        if let Some((top, bottom)) = selected_range {
            let margin = 14.0;
            if top - margin < self.scroll_target_y {
                self.scroll_target_y = (top - margin).max(0.0);
            } else if bottom + margin > self.scroll_target_y + view_h {
                self.scroll_target_y = (bottom + margin - view_h).min(max_scroll);
            }
        }
        self.scroll_target_y = self.scroll_target_y.clamp(0.0, max_scroll);
        if !self.config.selection_transition_enabled() {
            self.scroll_y = self.scroll_target_y;
            self.scroll_last_tick = None;
        } else if (self.scroll_target_y - self.scroll_y).abs() >= 0.45 {
            self.scroll_last_tick.get_or_insert_with(Instant::now);
        }
    }

    fn move_selection(&mut self, next_flat: usize, _window: &mut Window, cx: &mut Context<Self>) {
        let old_flat = self.selected_flat;
        self.hovered_flat = None;
        self.selected_flat = next_flat;

        if self.config.selection_transition_enabled()
            && next_flat != old_flat
            && self.tab_anim.is_none()
            && self.exit_anim.is_none()
        {
            let now = Instant::now();
            let (cols, preview_w, preview_h, tile_outer_w, grid_total_w) =
                self.fixed_grid_geometry();
            let (total_h, _) = self.compute_layout_metrics(cols, preview_h);
            let view_h = self.viewport_height_px.get().max(100.0);
            let max_scroll = (total_h - view_h).max(0.0);
            let optical_top_pad = Self::compute_optical_top_pad(max_scroll, total_h, view_h);
            let vp_origin_y = self
                .viewport_clip
                .borrow()
                .map(|b| f32::from(b.origin.y))
                .unwrap_or(TOP_SPACES_BAR_H);
            let layouts = self.compute_card_layout_rects(
                cols,
                preview_w,
                preview_h,
                tile_outer_w,
                grid_total_w,
                optical_top_pad,
                self.scroll_y,
                vp_origin_y,
            );

            let from_content_rect = self
                .selection_anim
                .and_then(|anim| {
                    layouts
                        .get(&anim.to_flat)
                        .map(|to| anim.sample(to.ring_content_bounds, now).0)
                })
                .or_else(|| layouts.get(&old_flat).map(|r| r.ring_content_bounds));

            if let Some(from_rect) = from_content_rect {
                let profile = TransitionProfile::from_effect(self.config.transition_effect);
                self.selection_anim = Some(SelectionTransition {
                    started_at: now,
                    duration: profile.selection_duration,
                    use_spring: profile.use_spring,
                    from_content_rect: from_rect,
                    to_flat: next_flat,
                });
            }
        } else if !self.config.selection_transition_enabled() {
            self.selection_anim = None;
        }

        self.ensure_selected_visible();
        cx.notify();
    }

    /// Pre-restores a minimized window behind WinDeck's topmost overlay curtain while guarding
    /// `hide_on_blur` and re-pinning both overlay layers strictly above the restored window.
    fn pre_restore_behind_curtain(&mut self, target_hwnd: isize) -> PreRestoreResult {
        self.suppress_blur_hide = true;
        let res = pre_restore_minimized_window(target_hwnd);
        pin_layered_z_order(
            self.gpui_hwnd,
            self.thumb_host.hwnd(),
            self.overlay_phys_rect,
        );
        self.suppress_blur_hide = false;
        res
    }

    #[allow(clippy::too_many_arguments)]
    fn on_cross_desktop_switch_ready(
        &mut self,
        target_hwnd: isize,
        mut target_info: Option<WindowInfo>,
        should_pre_restore: bool,
        switch_res: anyhow::Result<bool>,
        ready: &Arc<AtomicBool>,
        err_slot: &Arc<Mutex<Option<String>>>,
        cx: &mut Context<Self>,
    ) {
        let desktop_switched_ok = match switch_res {
            Ok(matched) => matched,
            Err(e) => {
                if let Ok(mut guard) = err_slot.lock() {
                    *guard = Some(format!("{e:#}"));
                }
                false
            }
        };

        if should_pre_restore {
            let matches_active_exit = self.exit_anim.as_ref().is_some_and(|exit| {
                !exit.finishing
                    && matches!(
                        exit.kind,
                        ExitTransitionKind::CrossDesktopWindow {
                            target_hwnd: active_hwnd,
                            ..
                        } if active_hwnd == target_hwnd
                    )
            });
            if matches_active_exit {
                if desktop_switched_ok {
                    let pre = self.pre_restore_behind_curtain(target_hwnd);
                    let refreshed_dest = if pre.restored {
                        target_info.as_mut().and_then(|info| {
                            refresh_window_geometry(info, &self.monitors).then(|| {
                                resolve_exit_target_phys_rect(
                                    info,
                                    self.overlay_phys_rect,
                                    &self.monitors,
                                )
                            })?
                        })
                    } else {
                        None
                    };
                    let live_now = if pre.restored {
                        self.thumbs
                            .borrow_mut()
                            .refresh_source_size(target_hwnd)
                            .is_some()
                    } else {
                        false
                    };

                    if let Some(ref mut exit) = self.exit_anim {
                        if pre.restored {
                            exit.dwm_transition_guard = pre.dwm_guard;
                        }
                        if let ExitTransitionKind::CrossDesktopWindow {
                            to_dest_phys,
                            has_live_surface,
                            ..
                        } = &mut exit.kind
                        {
                            if pre.restored {
                                *has_live_surface = live_now;
                                *to_dest_phys = if live_now {
                                    refreshed_dest.or(*to_dest_phys)
                                } else {
                                    None
                                };
                            } else if pre.was_minimized {
                                *to_dest_phys = None;
                            }
                        }
                    }
                } else if let Some(ref mut exit) = self.exit_anim {
                    if let ExitTransitionKind::CrossDesktopWindow { to_dest_phys, .. } =
                        &mut exit.kind
                    {
                        *to_dest_phys = None;
                    }
                }
            }
        }

        ready.store(true, Ordering::Release);
        cx.notify();
    }

    fn activate_window(&mut self, target_hwnd: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.exit_anim.is_some() {
            return;
        }
        let raw = HWND(target_hwnd as *mut std::ffi::c_void);
        if !unsafe { IsWindow(Some(raw)) }.as_bool() {
            self.clear_all_animations(false);
            self.refresh_snapshot();
            self.thumbs.borrow_mut().sync_targets(&self.flat_hwnds);
            cx.notify();
            return;
        }

        if !self.config.switch_transition_enabled() {
            self.clear_all_animations(false);
            self.thumbs.borrow_mut().unregister_all();
            self.last_thumb_bounds.borrow_mut().clear();
            hide_layered_overlay(self.gpui_hwnd, self.thumb_host.hwnd());
            self.visible = false;
            self.shown_at = None;
            self.hold_active = false;
            self.hold_navigated = false;
            self.hovered_flat = None;
            self.sync_visibility_to_host();

            match switch_to_window(&self.vdm, target_hwnd, Some(self.gpui_hwnd.0 as isize)) {
                Ok(report) => {
                    append_log(&format!(
                        "SWITCH hwnd=0x{:X} desktop_switched={} desktop_ok={} fg_ok={} iconic_after={} zoomed_after={} self_hidden={:?} elapsed={:.2}ms",
                        report.target_hwnd,
                        report.desktop_switch_issued,
                        report.desktop_matched,
                        report.foreground_matched,
                        report.is_iconic_after,
                        report.is_zoomed_after,
                        report.self_hidden_after,
                        report.elapsed_ms,
                    ));
                }
                Err(e) => {
                    append_log(&format!("SWITCH ERROR hwnd=0x{:X}: {e:#}", target_hwnd));
                    hide_layered_overlay(self.gpui_hwnd, self.thumb_host.hwnd());
                }
            }
            cx.notify();
            return;
        }

        // Animated Scene C exit path: preempt Scene A/B cleanly, keep overlay visible as curtain.
        self.tab_anim = None;
        self.selection_anim = None;
        self.scroll_y = self.scroll_target_y;
        self.scroll_last_tick = None;
        self.disarm_hold_mode();
        self.armed_delete_desktop = None;
        self.hovered_flat = None;

        let mut target_info = self
            .all_windows
            .iter()
            .find(|w| w.hwnd == target_hwnd)
            .cloned();
        let current_desktop_id = self
            .desktops
            .iter()
            .find(|d| d.is_current)
            .map(|d| d.id);
        let (target_desktop_id, is_cross_desktop) = match &target_info {
            Some(info) => {
                let need_switch = !info.is_pinned
                    && info.desktop_id.is_some()
                    && info.desktop_id != current_desktop_id;
                (info.desktop_id, need_switch)
            }
            None => (None, false),
        };

        let desktop_ready = Arc::new(AtomicBool::new(!is_cross_desktop));
        let desktop_switch_error = Arc::new(Mutex::new(None));

        let scale = window.scale_factor().max(self.overlay_scale).max(1.0);
        let from_slot_phys = self
            .last_thumb_bounds
            .borrow()
            .get(&target_hwnd)
            .copied()
            .or_else(|| {
                let target_flat = self.flat_hwnds.iter().position(|&h| h == target_hwnd)?;
                let (cols, preview_w, preview_h, tile_outer_w, grid_total_w) =
                    self.fixed_grid_geometry();
                let (total_h, _) = self.compute_layout_metrics(cols, preview_h);
                let view_h = self.viewport_height_px.get().max(100.0);
                let max_scroll = (total_h - view_h).max(0.0);
                let optical_top_pad = Self::compute_optical_top_pad(max_scroll, total_h, view_h);
                let vp_origin_y = self
                    .viewport_clip
                    .borrow()
                    .map(|b| f32::from(b.origin.y))
                    .unwrap_or(TOP_SPACES_BAR_H);
                let layouts = self.compute_card_layout_rects(
                    cols,
                    preview_w,
                    preview_h,
                    tile_outer_w,
                    grid_total_w,
                    optical_top_pad,
                    self.scroll_y,
                    vp_origin_y,
                );
                layouts.get(&target_flat).map(|r| r.thumb_window_bounds)
            })
            .map(|b| bounds_to_phys_rect(b, scale));
        let mut has_live_surface = self.thumbs.borrow().source_size(target_hwnd).is_some();
        let profile = TransitionProfile::from_effect(self.config.transition_effect);
        let mut dwm_transition_guard: Option<DwmTransitionSuppressGuard> = None;
        let mut should_pre_restore_after_desktop_switch = false;

        let (exit_duration, kind) = if is_cross_desktop {
            if let Some(target_did) = target_desktop_id {
                let cur_idx = self.desktops.iter().position(|d| d.is_current).unwrap_or(0);
                let dst_idx = self
                    .desktops
                    .iter()
                    .position(|d| d.id == target_did)
                    .unwrap_or(cur_idx);
                let desktop_dir = if dst_idx < cur_idx { -1.0 } else { 1.0 };
                let target_desktop_label = SharedString::from(
                    self.desktops
                        .get(dst_idx)
                        .and_then(|d| d.custom_name.clone())
                        .unwrap_or_else(|| format!("桌面 {}", dst_idx + 1)),
                );
                let candidate_dest_phys = target_info.as_ref().and_then(|info| {
                    resolve_exit_target_phys_rect(info, self.overlay_phys_rect, &self.monitors)
                });
                should_pre_restore_after_desktop_switch =
                    target_info.as_ref().is_some_and(|i| i.is_minimized)
                        && candidate_dest_phys.is_some();
                let to_dest_phys = if has_live_surface {
                    candidate_dest_phys
                } else {
                    None
                };
                let (fallback_icon, fallback_title) = self
                    .groups
                    .iter()
                    .flat_map(|g| g.cards.iter())
                    .find(|c| c.info.hwnd == target_hwnd)
                    .map(|c| {
                        let title = if !c.info.title.trim().is_empty() {
                            c.info.title.clone()
                        } else {
                            c.info.process_name.clone()
                        };
                        (c.icon.clone(), SharedString::from(title))
                    })
                    .unwrap_or_else(|| {
                        let title = target_info
                            .as_ref()
                            .map(|i| i.title.clone())
                            .unwrap_or_default();
                        (None, SharedString::from(title))
                    });

                let pan_duration = profile.cross_desktop_pan_duration;
                let pop_duration = profile.cross_desktop_pop_duration;
                (
                    pan_duration + pop_duration,
                    ExitTransitionKind::CrossDesktopWindow {
                        target_hwnd,
                        target_desktop_id: target_did,
                        target_desktop_label,
                        desktop_dir,
                        pan_duration,
                        pop_duration,
                        to_dest_phys,
                        has_live_surface,
                        fallback_icon,
                        fallback_title,
                    },
                )
            } else {
                (
                    profile.exit_zoom_duration,
                    ExitTransitionKind::GentleFadeWindow {
                        target_hwnd,
                        from_slot_phys,
                        preview_fit: self.config.preview_fit,
                        target_desktop_id,
                        is_cross_desktop,
                    },
                )
            }
        } else if profile.use_zoom_to_window {
            let mut resolved_dest = target_info.as_ref().and_then(|info| {
                resolve_exit_target_phys_rect(info, self.overlay_phys_rect, &self.monitors)
            });
            if target_info.as_ref().is_some_and(|i| i.is_minimized) && resolved_dest.is_some() {
                let pre = self.pre_restore_behind_curtain(target_hwnd);
                if pre.restored {
                    dwm_transition_guard = pre.dwm_guard;
                    if let Some(ref mut info) = target_info {
                        if refresh_window_geometry(info, &self.monitors) {
                            resolved_dest = resolve_exit_target_phys_rect(
                                info,
                                self.overlay_phys_rect,
                                &self.monitors,
                            );
                        }
                    }
                    has_live_surface = self
                        .thumbs
                        .borrow_mut()
                        .refresh_source_size(target_hwnd)
                        .is_some();
                } else if pre.was_minimized {
                    // Window stayed iconic (e.g. hung or elevated UIPI): degrade cleanly to GentleFadeWindow.
                    resolved_dest = None;
                }
            }
            if has_live_surface {
                if let (Some(from_phys), Some(to_phys)) = (from_slot_phys, resolved_dest) {
                    (
                        profile.exit_zoom_duration,
                        ExitTransitionKind::ZoomWindow {
                            target_hwnd,
                            from_slot_phys: from_phys,
                            to_dest_phys: to_phys,
                            preview_fit: self.config.preview_fit,
                            target_desktop_id,
                            is_cross_desktop,
                        },
                    )
                } else {
                    (
                        profile.exit_zoom_duration,
                        ExitTransitionKind::GentleFadeWindow {
                            target_hwnd,
                            from_slot_phys,
                            preview_fit: self.config.preview_fit,
                            target_desktop_id,
                            is_cross_desktop,
                        },
                    )
                }
            } else {
                (
                    profile.exit_zoom_duration,
                    ExitTransitionKind::GentleFadeWindow {
                        target_hwnd,
                        from_slot_phys,
                        preview_fit: self.config.preview_fit,
                        target_desktop_id,
                        is_cross_desktop,
                    },
                )
            }
        } else {
            (
                profile.exit_zoom_duration,
                ExitTransitionKind::GentleFadeWindow {
                    target_hwnd,
                    from_slot_phys,
                    preview_fit: self.config.preview_fit,
                    target_desktop_id,
                    is_cross_desktop,
                },
            )
        };

        self.exit_anim = Some(ExitTransition {
            started_at: Instant::now(),
            duration: exit_duration,
            use_spring: profile.use_spring,
            finishing: false,
            desktop_ready: desktop_ready.clone(),
            desktop_switch_error: desktop_switch_error.clone(),
            dwm_transition_guard,
            kind,
        });

        if is_cross_desktop {
            if let Some(target_did) = target_desktop_id {
                let vdm = self.vdm.clone();
                let ready = desktop_ready;
                let err_slot = desktop_switch_error;
                let info_for_restore = target_info;
                cx.spawn(async move |this, cx| {
                    let switch_res = cx
                        .background_executor()
                        .spawn(async move {
                            switch_desktop_and_wait(&vdm, target_did, Duration::from_millis(200))
                        })
                        .await;
                    let updated = this
                        .update(cx, |view, cx| {
                            view.on_cross_desktop_switch_ready(
                                target_hwnd,
                                info_for_restore,
                                should_pre_restore_after_desktop_switch,
                                switch_res,
                                &ready,
                                &err_slot,
                                cx,
                            );
                        })
                        .is_ok();
                    if !updated {
                        ready.store(true, Ordering::Release);
                    }
                })
                .detach();
            }
        }
        cx.notify();
    }

    fn switch_to_desktop_only(
        &mut self,
        desktop_id: DesktopId,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.exit_anim.is_some() {
            return;
        }
        // Locate frontmost non-minimized window on destination desktop to prevent UWP late-activation pull-back
        let dest_front_hwnd = self
            .all_windows
            .iter()
            .find(|w| w.desktop_id == Some(desktop_id) && !w.is_minimized)
            .map(|w| w.hwnd);

        if !self.config.switch_transition_enabled() {
            self.clear_all_animations(false);
            self.thumbs.borrow_mut().unregister_all();
            self.last_thumb_bounds.borrow_mut().clear();
            self.visible = false;
            self.shown_at = None;
            self.hold_active = false;
            self.hold_navigated = false;
            self.hovered_flat = None;
            self.sync_visibility_to_host();

            if let Err(e) = self.vdm.switch_to_desktop(desktop_id) {
                append_log(&format!("DESKTOP SWITCH ERROR {}: {e:#}", desktop_id));
                hide_layered_overlay(self.gpui_hwnd, self.thumb_host.hwnd());
                cx.notify();
                return;
            }

            // Hand off foreground to the destination desktop (or Shell desktop if empty) while we still own foreground
            unsafe {
                let target = dest_front_hwnd
                    .map(|raw| HWND(raw as *mut std::ffi::c_void))
                    .filter(|&h| IsWindow(Some(h)).as_bool())
                    .unwrap_or_else(|| GetShellWindow());
                if !target.is_invalid() {
                    let _ = SetForegroundWindow(target);
                }
            }
            hide_layered_overlay(self.gpui_hwnd, self.thumb_host.hwnd());
            append_log(&format!("DESKTOP_SWITCH target={}", desktop_id));
            cx.notify();
            return;
        }

        // Animated Scene C desktop switch: immediately preempt any click-1 TabTransition
        self.tab_anim = None;
        self.selection_anim = None;
        self.scroll_y = self.scroll_target_y;
        self.scroll_last_tick = None;
        self.disarm_hold_mode();
        self.armed_delete_desktop = None;
        self.hovered_flat = None;

        let cur_idx = self.desktops.iter().position(|d| d.is_current).unwrap_or(0);
        let dst_idx = self
            .desktops
            .iter()
            .position(|d| d.id == desktop_id)
            .unwrap_or(cur_idx);
        let slide_dir = if dst_idx < cur_idx { -1.0 } else { 1.0 };
        let target_desktop_label = SharedString::from(
            self.desktops
                .get(dst_idx)
                .and_then(|d| d.custom_name.clone())
                .unwrap_or_else(|| format!("桌面 {}", dst_idx + 1)),
        );
        let already_current = self
            .desktops
            .iter()
            .any(|d| d.id == desktop_id && d.is_current);

        let desktop_ready = Arc::new(AtomicBool::new(already_current));
        let desktop_switch_error = Arc::new(Mutex::new(None));
        if !already_current {
            let vdm = self.vdm.clone();
            let ready = desktop_ready.clone();
            let err_slot = desktop_switch_error.clone();
            cx.background_executor()
                .spawn(async move {
                    if let Err(e) =
                        switch_desktop_and_wait(&vdm, desktop_id, Duration::from_millis(200))
                    {
                        if let Ok(mut guard) = err_slot.lock() {
                            *guard = Some(format!("{e:#}"));
                        }
                    }
                    ready.store(true, Ordering::Release);
                })
                .detach();
        }

        let profile = TransitionProfile::from_effect(self.config.transition_effect);
        self.exit_anim = Some(ExitTransition {
            started_at: Instant::now(),
            duration: profile.exit_desktop_duration,
            use_spring: profile.use_spring,
            finishing: false,
            desktop_ready,
            desktop_switch_error,
            dwm_transition_guard: None,
            kind: ExitTransitionKind::DesktopSwitch {
                target_desktop_id: desktop_id,
                target_desktop_label,
                dest_front_hwnd,
                slide_dir,
            },
        });
        cx.notify();
    }

    fn finish_exit_transition(&mut self, cx: &mut Context<Self>) {
        let Some(exit) = self.exit_anim.take() else {
            return;
        };
        let dwm_guard = exit.dwm_transition_guard;
        let elapsed_ms = exit.started_at.elapsed().as_secs_f64() * 1000.0;
        let desktop_err = exit
            .desktop_switch_error
            .lock()
            .ok()
            .and_then(|mut g| g.take());

        // Clear visible flag BEFORE foreground handoff so hide_on_blur cannot re-enter mid-handoff.
        self.tab_anim = None;
        self.selection_anim = None;
        self.scroll_last_tick = None;
        self.visible = false;
        self.shown_at = None;
        self.hold_active = false;
        self.hold_navigated = false;
        self.hovered_flat = None;
        self.armed_delete_desktop = None;
        self.sync_visibility_to_host();

        match exit.kind {
            ExitTransitionKind::ZoomWindow {
                target_hwnd,
                target_desktop_id,
                is_cross_desktop,
                ..
            }
            | ExitTransitionKind::GentleFadeWindow {
                target_hwnd,
                target_desktop_id,
                is_cross_desktop,
                ..
            } => {
                if let Some(err) = desktop_err {
                    append_log(&format!(
                        "SWITCH DESKTOP ERROR hwnd=0x{:X} target_desktop={:?}: {err}",
                        target_hwnd, target_desktop_id
                    ));
                }
                match activate_window_foreground(target_hwnd) {
                    Ok(res) => {
                        append_log(&format!(
                            "SWITCH(anim) hwnd=0x{:X} desktop_switched={} fg_ok={} iconic_after={} zoomed_after={} elapsed={:.2}ms",
                            target_hwnd,
                            is_cross_desktop,
                            res.foreground_matched,
                            res.is_iconic_after,
                            res.is_zoomed_after,
                            elapsed_ms,
                        ));
                    }
                    Err(e) => {
                        append_log(&format!("SWITCH(anim) ERROR hwnd=0x{:X}: {e:#}", target_hwnd));
                    }
                }
            }
            ExitTransitionKind::CrossDesktopWindow {
                target_hwnd,
                target_desktop_id,
                desktop_dir,
                ..
            } => {
                if let Some(err) = desktop_err {
                    append_log(&format!(
                        "SWITCH(cross-desktop) DESKTOP ERROR hwnd=0x{:X} target_desktop={target_desktop_id}: {err}",
                        target_hwnd
                    ));
                }
                match activate_window_foreground(target_hwnd) {
                    Ok(res) => {
                        append_log(&format!(
                            "SWITCH(cross-desktop-anim) hwnd=0x{:X} dir={} desktop={} fg_ok={} iconic_after={} zoomed_after={} elapsed={:.2}ms",
                            target_hwnd,
                            if desktop_dir < 0.0 { "left" } else { "right" },
                            target_desktop_id,
                            res.foreground_matched,
                            res.is_iconic_after,
                            res.is_zoomed_after,
                            elapsed_ms,
                        ));
                    }
                    Err(e) => {
                        append_log(&format!(
                            "SWITCH(cross-desktop-anim) ERROR hwnd=0x{:X}: {e:#}",
                            target_hwnd
                        ));
                    }
                }
            }
            ExitTransitionKind::DesktopSwitch {
                target_desktop_id,
                dest_front_hwnd,
                ..
            } => {
                if let Some(err) = desktop_err {
                    append_log(&format!("DESKTOP SWITCH(anim) ERROR {target_desktop_id}: {err}"));
                } else {
                    unsafe {
                        let target = dest_front_hwnd
                            .map(|raw| HWND(raw as *mut std::ffi::c_void))
                            .filter(|&h| IsWindow(Some(h)).as_bool())
                            .unwrap_or_else(|| GetShellWindow());
                        if !target.is_invalid() {
                            let _ = SetForegroundWindow(target);
                        }
                    }
                    append_log(&format!(
                        "DESKTOP_SWITCH(anim) target={} elapsed={:.2}ms",
                        target_desktop_id, elapsed_ms
                    ));
                }
            }
        }

        self.thumbs.borrow_mut().unregister_all();
        self.last_thumb_bounds.borrow_mut().clear();
        hide_layered_overlay(self.gpui_hwnd, self.thumb_host.hwnd());
        drop(dwm_guard);
        cx.notify();
    }

    fn force_quit_target_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let target_idx = self.hovered_flat.unwrap_or(self.selected_flat);
        let Some(&target_hwnd) = self.flat_hwnds.get(target_idx) else {
            return;
        };

        let (title, pid, proc_name) = self
            .all_windows
            .iter()
            .find(|w| w.hwnd == target_hwnd)
            .map(|w| (w.title.clone(), w.pid, w.process_name.clone()))
            .unwrap_or_else(|| (String::new(), 0, String::new()));

        let same_pid_count = if pid != 0 {
            self.all_windows.iter().filter(|w| w.pid == pid).count()
        } else {
            1
        };

        // Cancel hold-to-jump so releasing the launch modifier afterwards does not jump to a neighbor window
        self.disarm_hold_mode();
        self.armed_delete_desktop = None;

        match force_quit_window(target_hwnd, same_pid_count) {
            Ok(report) => {
                let display = if !title.trim().is_empty() {
                    let short: String = title.chars().take(24).collect();
                    short
                } else if !report.process_name.is_empty() {
                    report.process_name.clone()
                } else {
                    proc_name
                };

                self.hovered_flat = None;
                self.refresh_snapshot();
                if self.flat_hwnds.is_empty() {
                    self.selected_flat = 0;
                } else {
                    self.selected_flat = target_idx.min(self.flat_hwnds.len() - 1);
                }
                self.thumbs.borrow_mut().sync_targets(&self.flat_hwnds);

                let toast = match report.outcome {
                    ForceQuitOutcome::ProcessTerminated { .. } => {
                        format!("已强制退出 {display}")
                    }
                    ForceQuitOutcome::WindowClosedSafely => {
                        format!("已关闭 {display}")
                    }
                };
                self.transient_toast = Some((SharedString::from(toast), Instant::now()));
                append_log(&format!(
                    "FORCE_QUIT hwnd=0x{:X} pid={} proc='{}' outcome={:?}",
                    report.target_hwnd, report.pid, report.process_name, report.outcome
                ));
                self.clamp_scroll();
                window.refresh();
                cx.notify();
            }
            Err(e) => {
                self.transient_toast = Some((
                    SharedString::from(format!("强制退出失败：{e}")),
                    Instant::now(),
                ));
                append_log(&format!("FORCE_QUIT ERROR hwnd=0x{:X}: {e:#}", target_hwnd));
                cx.notify();
            }
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.visible {
            return;
        }
        let key = event.keystroke.key.as_str();
        let mods = &event.keystroke.modifiers;
        let shift = mods.shift;

        // When Update confirmation dialog is open, route keys exclusively to it
        if self.update_dialog_open {
            cx.stop_propagation();
            if key == "escape" {
                self.dismiss_update_dialog(window, cx);
            } else if key == "enter" {
                self.start_self_update(cx);
            }
            return;
        }

        // When Settings view is open, route keys exclusively to Settings / shortcut capture
        if self.settings_open {
            cx.stop_propagation();
            if self.recording_hotkey_letter {
                if key == "escape" {
                    self.recording_hotkey_letter = false;
                    cx.notify();
                } else if key.len() == 1 && key.as_bytes()[0].is_ascii_alphabetic() {
                    let letter = key.as_bytes()[0] as char;
                    let primary = self.config.parsed_hotkey().primary;
                    self.apply_hotkey_change(primary, letter, cx);
                }
            } else if self.recording_force_quit_key {
                if key == "escape" {
                    self.recording_force_quit_key = false;
                    cx.notify();
                } else if let Some(sc) = crate::config::LocalShortcut::from_keystroke(
                    key,
                    mods.control,
                    mods.alt,
                    mods.shift,
                ) {
                    self.apply_force_quit_key_change(sc, cx);
                }
            } else if key == "escape" {
                self.close_settings(window, cx);
            }
            return;
        }

        if self.exit_anim.is_some() {
            if key == "escape" {
                cx.stop_propagation();
                self.finish_exit_transition(cx);
            }
            return;
        }

        let (cols, _, _, _, _) = self.fixed_grid_geometry();
        let sizes = self.group_sizes();

        if key == "escape" {
            cx.stop_propagation();
            if self.dismiss_armed_delete(cx) {
                return;
            }
            self.hide(cx);
            return;
        }
        if self.armed_delete_desktop.take().is_some() {
            cx.notify();
        }

        // Configurable force-quit shortcut on hovered/selected window
        let held_primary = self.hold_active.then(|| self.config.parsed_hotkey().primary);
        if self.config.parsed_force_quit_key().matches(
            key,
            mods.control,
            mods.alt,
            mods.shift,
            held_primary,
        ) {
            cx.stop_propagation();
            self.force_quit_target_window(window, cx);
            return;
        }

        match key {
            "enter" | "space" => {
                cx.stop_propagation();
                if let Some(&hwnd) = self.flat_hwnds.get(self.selected_flat) {
                    self.activate_window(hwnd, window, cx);
                } else if let Some(did) = self.desktop_filter {
                    self.switch_to_desktop_only(did, window, cx);
                }
            }
            "right" => {
                cx.stop_propagation();
                let next = navigate(
                    &sizes,
                    cols,
                    self.selected_flat,
                    NavDirection::Right,
                    self.config.cross_desktop_nav,
                );
                self.move_selection(next, window, cx);
            }
            "left" => {
                cx.stop_propagation();
                let next = navigate(
                    &sizes,
                    cols,
                    self.selected_flat,
                    NavDirection::Left,
                    self.config.cross_desktop_nav,
                );
                self.move_selection(next, window, cx);
            }
            "down" => {
                cx.stop_propagation();
                let next = navigate(
                    &sizes,
                    cols,
                    self.selected_flat,
                    NavDirection::Down,
                    self.config.cross_desktop_nav,
                );
                self.move_selection(next, window, cx);
            }
            "up" => {
                cx.stop_propagation();
                let next = navigate(
                    &sizes,
                    cols,
                    self.selected_flat,
                    NavDirection::Up,
                    self.config.cross_desktop_nav,
                );
                self.move_selection(next, window, cx);
            }
            "tab" => {
                cx.stop_propagation();
                let dir = if shift {
                    NavDirection::Left
                } else {
                    NavDirection::Right
                };
                let next = navigate(&sizes, cols, self.selected_flat, dir, true);
                self.move_selection(next, window, cx);
            }
            "home" => {
                cx.stop_propagation();
                let next = navigate(&sizes, cols, self.selected_flat, NavDirection::Home, true);
                self.move_selection(next, window, cx);
            }
            "end" => {
                cx.stop_propagation();
                let next = navigate(&sizes, cols, self.selected_flat, NavDirection::End, true);
                self.move_selection(next, window, cx);
            }
            // Press L to live-cycle label placement: bottom_right -> bottom_left -> center -> below
            "l" => {
                cx.stop_propagation();
                self.config.label_position = self.config.label_position.next();
                self.persist_config();
                self.transient_toast = Some((
                    SharedString::from(format!(
                        "标签位置：{}（已自动保存）",
                        self.config.label_position.display_label()
                    )),
                    Instant::now(),
                ));
                self.ensure_selected_visible();
                window.refresh();
                cx.notify();
            }
            // Press F to live-toggle fixed box fit mode: cover <-> contain
            "f" => {
                cx.stop_propagation();
                self.config.preview_fit = self.config.preview_fit.toggle();
                self.persist_config();
                self.transient_toast = Some((
                    SharedString::from(format!(
                        "预览适配：{}（已自动保存）",
                        self.config.preview_fit.display_label()
                    )),
                    Instant::now(),
                ));
                window.refresh();
                cx.notify();
            }
            // Press - / = to live-tune Gaussian blur transparency
            "-" | "_" => {
                cx.stop_propagation();
                self.config.blur_opacity = (self.config.blur_opacity - 0.05).clamp(0.05, 0.85);
                self.thumb_host.configure(
                    self.config.immersive,
                    self.config.background,
                    self.config.blur_opacity,
                );
                self.persist_config();
                self.transient_toast = Some((
                    SharedString::from(format!(
                        "毛玻璃暗度：{:.0}%（已自动保存）",
                        self.config.blur_opacity * 100.0
                    )),
                    Instant::now(),
                ));
                cx.notify();
            }
            "=" | "+" => {
                cx.stop_propagation();
                self.config.blur_opacity = (self.config.blur_opacity + 0.05).clamp(0.05, 0.85);
                self.thumb_host.configure(
                    self.config.immersive,
                    self.config.background,
                    self.config.blur_opacity,
                );
                self.persist_config();
                self.transient_toast = Some((
                    SharedString::from(format!(
                        "毛玻璃暗度：{:.0}%（已自动保存）",
                        self.config.blur_opacity * 100.0
                    )),
                    Instant::now(),
                ));
                cx.notify();
            }
            "f5" | "r" => {
                self.refresh_snapshot();
                self.thumbs.borrow_mut().sync_targets(&self.flat_hwnds);
                self.clamp_scroll();
                window.refresh();
                cx.notify();
            }
            // Number keys: 0 = 全部, 1..9 = filter Desktop 1..9
            "0" => {
                self.set_desktop_filter(None, window, cx);
            }
            "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" => {
                if let Ok(n) = key.parse::<usize>() {
                    if let Some(d) = self.desktops.get(n - 1) {
                        let did = d.id;
                        self.set_desktop_filter(Some(did), window, cx);
                    }
                }
            }
            _ => {}
        }
    }

    fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.exit_anim.is_some() {
            return;
        }
        let delta_y = match event.delta {
            ScrollDelta::Lines(pt) => -pt.y * 64.0,
            ScrollDelta::Pixels(pt) => -f32::from(pt.y),
        };
        self.scroll_target_y += delta_y;
        self.clamp_scroll();
        if !self.config.selection_transition_enabled() {
            self.scroll_y = self.scroll_target_y;
            self.scroll_last_tick = None;
        } else if (self.scroll_target_y - self.scroll_y).abs() >= 0.45 {
            self.scroll_last_tick.get_or_insert_with(Instant::now);
        }
        cx.notify();
    }

    pub fn run_self_benchmark(&mut self, window: &mut Window, cx: &mut Context<Self>) -> String {
        use win_platform::capture_process_resources;

        self.show_and_refresh(window, cx);
        let ((win_w, win_h), (cli_w, cli_h)) = query_window_and_client_sizes(self.gpui_hwnd);
        let ((host_w, host_h), _) = query_window_and_client_sizes(self.thumb_host.hwnd());
        self.hide(cx);

        for _ in 0..10 {
            self.show_and_refresh(window, cx);
            self.hide(cx);
        }

        let before = capture_process_resources();
        let mut open_ms = Vec::with_capacity(50);

        for _ in 0..50 {
            let t0 = Instant::now();
            self.show_and_refresh(window, cx);
            open_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
            self.hide(cx);
        }

        let after = capture_process_resources();
        open_ms.sort_by(|a, b| a.total_cmp(b));
        let p50 = open_ms[open_ms.len() / 2];
        let p95 = open_ms[(open_ms.len() * 95) / 100];
        let max = *open_ms.last().unwrap_or(&0.0);

        let r_idle0 = capture_process_resources();
        let t_idle = Instant::now();
        std::thread::sleep(Duration::from_millis(2000));
        let r_idle1 = capture_process_resources();
        let wall_ms = t_idle.elapsed().as_secs_f64() * 1000.0;
        let cpu_ms = r_idle1.cpu_time_ms - r_idle0.cpu_time_ms;

        format!(
            "BENCH 50-cycles: p50={:.2}ms p95={:.2}ms max={:.2}ms | Layered gpui={}x{} (insets={}x{}) host={}x{} | FixedPreview={}x{} ({}) | GDI {}->{} (d={:+}) USER {}->{} (d={:+}) Handles {}->{} (d={:+}) | Hidden 2s CPU={:.2}ms ({:.2}% of 1 core)",
            p50,
            p95,
            max,
            win_w,
            win_h,
            win_w - cli_w,
            win_h - cli_h,
            host_w,
            host_h,
            self.config.preview_width as i32,
            self.config.preview_height as i32,
            self.config.preview_fit.as_config_str(),
            before.gdi_objects,
            after.gdi_objects,
            after.gdi_objects as i64 - before.gdi_objects as i64,
            before.user_objects,
            after.user_objects,
            after.user_objects as i64 - before.user_objects as i64,
            before.kernel_handles,
            after.kernel_handles,
            after.kernel_handles as i64 - before.kernel_handles as i64,
            cpu_ms,
            (cpu_ms / wall_ms) * 100.0,
        )
    }
}

impl Render for SwitcherView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let now = Instant::now();

        // Schedule Scene C exit completion outside WM_PAINT / Render::render once visual + desktop switch finish.
        if let Some(ref mut exit) = self.exit_anim {
            if exit.is_ready_to_finish(now) && !exit.finishing {
                exit.finishing = true;
                cx.spawn(async move |this, cx| {
                    let _ = this.update(cx, |view, cx| {
                        view.finish_exit_transition(cx);
                    });
                })
                .detach();
            }
        }

        let viewport_sz = window.viewport_size();
        let logical_w = f32::from(viewport_sz.width);
        if logical_w > 100.0 {
            self.viewport_width_px.set(logical_w);
        }

        // Complete Scene A tab transition once elapsed and prune leaving DWM thumbnails.
        if self.tab_anim.as_ref().is_some_and(|ta| ta.is_finished(now)) {
            self.tab_anim = None;
            self.thumbs.borrow_mut().retain_targets(&self.flat_hwnds);
        }

        let (cols, preview_w, preview_h, tile_outer_w, grid_total_w) = self.fixed_grid_geometry();
        let item_total_h = preview_h + self.extra_item_h();
        let label_pos = self.config.label_position;
        let preview_fit = self.config.preview_fit;

        let (total_content_h, _) = self.compute_layout_metrics(cols, preview_h);
        let view_h = self.viewport_height_px.get().max(100.0);
        let max_scroll = (total_content_h - view_h).max(0.0);
        self.scroll_target_y = self.scroll_target_y.clamp(0.0, max_scroll);
        if self.scroll_y > max_scroll {
            self.scroll_y = max_scroll;
        }

        // Advance Scene B smooth scroll via frame-rate-independent exponential damping.
        let scroll_animating = if self.visible
            && !self.settings_open
            && !self.update_dialog_open
            && self.exit_anim.is_none()
            && self.config.selection_transition_enabled()
            && (self.scroll_target_y - self.scroll_y).abs() >= 0.45
        {
            let dt = self
                .scroll_last_tick
                .map(|last| now.saturating_duration_since(last).as_secs_f32())
                .unwrap_or(1.0 / 60.0);
            self.scroll_last_tick = Some(now);
            let speed =
                TransitionProfile::from_effect(self.config.transition_effect).scroll_damp_speed;
            self.scroll_y = exp_damp(self.scroll_y, self.scroll_target_y, speed, dt);
            (self.scroll_target_y - self.scroll_y).abs() >= 0.45
        } else {
            self.scroll_y = self.scroll_target_y;
            self.scroll_last_tick = None;
            false
        };
        let scroll_y = self.scroll_y;

        let optical_top_pad = Self::compute_optical_top_pad(max_scroll, total_content_h, view_h);
        let vp_origin_y = self
            .viewport_clip
            .borrow()
            .map(|b| f32::from(b.origin.y))
            .unwrap_or(TOP_SPACES_BAR_H);

        let card_layouts = if self.tab_anim.is_some() || self.selection_anim.is_some() {
            Some(self.compute_card_layout_rects(
                cols,
                preview_w,
                preview_h,
                tile_outer_w,
                grid_total_w,
                optical_top_pad,
                scroll_y,
                vp_origin_y,
            ))
        } else {
            None
        };

        // Advance Scene B floating focus ring.
        let floating_focus_rect: Option<Bounds<Pixels>> = if let Some(sel) = self.selection_anim {
            if let Some(to_rect) = card_layouts
                .as_ref()
                .and_then(|m| m.get(&sel.to_flat))
                .map(|c| c.ring_content_bounds)
            {
                let (rect, done) = sel.sample(to_rect, now);
                if done {
                    self.selection_anim = None;
                    None
                } else {
                    Some(rect)
                }
            } else {
                self.selection_anim = None;
                None
            }
        } else {
            None
        };

        let host_size_phys = (
            (self.overlay_phys_rect.right - self.overlay_phys_rect.left).max(1),
            (self.overlay_phys_rect.bottom - self.overlay_phys_rect.top).max(1),
        );
        let viewport_w_logical = self.viewport_width_px.get().max(640.0);

        // Compute per-frame Scene C exit parameters (if active).
        #[derive(Clone, Copy)]
        enum ExitFrameState {
            ZoomWindow {
                target_hwnd: isize,
                from_slot_phys: RECT,
                to_dest_phys: RECT,
                fit: PreviewFit,
                eased_pos: f32,
                eased_uncrop: f32,
                other_thumb_opacity: u8,
            },
            CrossDesktopWindow {
                target_hwnd: isize,
                sample: CrossDesktopSample,
                has_live_surface: bool,
            },
            GentleFadeWindow {
                target_hwnd: isize,
                from_slot_phys: Option<RECT>,
                fit: PreviewFit,
                target_opacity: f32,
                target_scale: f32,
                other_thumb_opacity: u8,
            },
            DesktopSwitch {
                slide_dx: f32,
                fade: f32,
                fade_u8: u8,
            },
        }

        let mut spaces_bar_opacity: f32 = 1.0;
        let mut override_active_desktop: Option<DesktopId> = None;
        let mut directional_badge: Option<(SharedString, f32, f32, f32)> = None;
        let mut cross_desktop_fallback: Option<(Option<Arc<RenderImage>>, SharedString, f32, f32, f32)> =
            None;

        let (exit_frame, chrome_opacity) = if let Some(ref exit) = self.exit_anim {
            let t = exit.raw_progress(now);
            let mut chrome = exit.chrome_opacity(t);
            spaces_bar_opacity = chrome;
            let other_u8 = exit.other_thumbs_opacity(t);
            let state = match &exit.kind {
                ExitTransitionKind::ZoomWindow {
                    target_hwnd,
                    from_slot_phys,
                    to_dest_phys,
                    preview_fit,
                    ..
                } => ExitFrameState::ZoomWindow {
                    target_hwnd: *target_hwnd,
                    from_slot_phys: *from_slot_phys,
                    to_dest_phys: *to_dest_phys,
                    fit: *preview_fit,
                    eased_pos: exit.eased_zoom_pos(t),
                    eased_uncrop: exit.eased_uncrop(t),
                    other_thumb_opacity: other_u8,
                },
                ExitTransitionKind::CrossDesktopWindow {
                    target_hwnd,
                    target_desktop_id,
                    target_desktop_label,
                    desktop_dir,
                    pan_duration,
                    pop_duration,
                    to_dest_phys,
                    has_live_surface,
                    fallback_icon,
                    fallback_title,
                } => {
                    let elapsed = now.saturating_duration_since(exit.started_at);
                    let sample = sample_cross_desktop_window(
                        elapsed,
                        *pan_duration,
                        *pop_duration,
                        *desktop_dir,
                        exit.use_spring,
                        viewport_w_logical,
                        host_size_phys,
                        if *has_live_surface { *to_dest_phys } else { None },
                    );
                    chrome = sample.outgoing_opacity;
                    // Keep the top Spaces bar subtly visible during Phase 1 so the active desktop pill visibly shifts left/right,
                    // then fade it out cleanly as Phase 2 pops the target window forward.
                    spaces_bar_opacity = if sample.pop_t > 0.0 {
                        (1.0 - ease_out_cubic((sample.pop_t / 0.35).clamp(0.0, 1.0))).clamp(0.0, 1.0)
                    } else {
                        0.92
                    };
                    if sample.pan_t >= 0.12 {
                        override_active_desktop = Some(*target_desktop_id);
                    }
                    if sample.badge_opacity > 0.01 {
                        directional_badge = Some((
                            target_desktop_label.clone(),
                            *desktop_dir,
                            sample.badge_dx,
                            sample.badge_opacity,
                        ));
                    }
                    if sample.target_dest_phys.is_none() && sample.target_opacity > 0.01 {
                        cross_desktop_fallback = Some((
                            fallback_icon.clone(),
                            fallback_title.clone(),
                            sample.fallback_dx,
                            sample.fallback_scale,
                            sample.target_opacity,
                        ));
                    }
                    ExitFrameState::CrossDesktopWindow {
                        target_hwnd: *target_hwnd,
                        sample,
                        has_live_surface: *has_live_surface,
                    }
                }
                ExitTransitionKind::GentleFadeWindow {
                    target_hwnd,
                    from_slot_phys,
                    preview_fit,
                    ..
                } => {
                    let eased = ease_out_cubic(t);
                    ExitFrameState::GentleFadeWindow {
                        target_hwnd: *target_hwnd,
                        from_slot_phys: *from_slot_phys,
                        fit: *preview_fit,
                        target_opacity: (1.0 - eased).clamp(0.0, 1.0),
                        target_scale: 1.0 + 0.055 * eased,
                        other_thumb_opacity: other_u8,
                    }
                }
                ExitTransitionKind::DesktopSwitch {
                    target_desktop_id,
                    target_desktop_label,
                    slide_dir,
                    ..
                } => {
                    let eased_pos = TransitionProfile::ease_position(exit.use_spring, t);
                    let fade = (1.0 - ease_out_cubic(t)).clamp(0.0, 1.0);
                    let pan_dist = (viewport_w_logical * 0.14).clamp(110.0, 260.0);
                    override_active_desktop = Some(*target_desktop_id);
                    spaces_bar_opacity = fade;
                    let badge_op = (1.0 - ease_out_cubic(((t - 0.35) / 0.65).clamp(0.0, 1.0)))
                        .clamp(0.0, 1.0);
                    if badge_op > 0.01 {
                        let badge_dx = *slide_dir * 22.0 * (1.0 - ease_out_cubic(t));
                        directional_badge = Some((
                            target_desktop_label.clone(),
                            *slide_dir,
                            badge_dx,
                            badge_op,
                        ));
                    }
                    ExitFrameState::DesktopSwitch {
                        slide_dx: -*slide_dir * pan_dist * eased_pos,
                        fade,
                        fade_u8: (fade * 255.0).round() as u8,
                    }
                }
            };
            (Some(state), chrome)
        } else {
            (None, 1.0)
        };

        // Compute per-frame Scene A tab transition parameters (if active).
        let (tab_eased, tab_enter_dx, tab_enter_opacity, tab_leave_dx, tab_leave_opacity) =
            if let Some(ref ta) = self.tab_anim {
                let t = ta.raw_progress(now);
                let eased = ta.eased_pos(t);
                (
                    eased,
                    ta.slide_dir * ta.slide_px * (1.0 - eased),
                    ta.entering_opacity(t),
                    -ta.slide_dir * ta.slide_px * eased,
                    ta.leaving_opacity(t),
                )
            } else {
                (1.0, 0.0, 1.0, 0.0, 0.0)
            };

        let leaving_live_cards: Vec<(isize, Bounds<Pixels>)> = self
            .tab_anim
            .as_ref()
            .map(|ta| {
                ta.leaving_cards
                    .iter()
                    .filter(|c| c.has_live_surface)
                    .map(|c| {
                        let shifted = Bounds {
                            origin: point(
                                c.thumb_bounds.origin.x + px(tab_leave_dx),
                                c.thumb_bounds.origin.y,
                            ),
                            size: c.thumb_bounds.size,
                        };
                        (c.hwnd, shifted)
                    })
                    .collect()
            })
            .unwrap_or_default();

        let leaving_fallback_cards: Vec<LeavingCardSnapshot> = self
            .tab_anim
            .as_ref()
            .map(|ta| {
                ta.leaving_cards
                    .iter()
                    .filter(|c| !c.has_live_surface)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();

        let viewport_clip_rc = self.viewport_clip.clone();
        let viewport_h_cell = self.viewport_height_px.clone();
        let viewport_w_cell = self.viewport_width_px.clone();
        let last_thumb_bounds_rc = self.last_thumb_bounds.clone();
        let thumbs_rc = self.thumbs.clone();
        let is_visible = self.visible;
        let is_exiting = self.exit_anim.is_some();
        let selected_flat = self.selected_flat;
        let hovered_flat = self.hovered_flat;

        // Request next vsync frame ONLY while an animation is actively running (zero idle CPU).
        let exit_still_running = self.exit_anim.as_ref().is_some_and(|e| !e.finishing);
        let any_anim_active = self.visible
            && !self.settings_open
            && !self.update_dialog_open
            && (self.tab_anim.is_some()
                || self.selection_anim.is_some()
                || exit_still_running
                || scroll_animating);
        if any_anim_active {
            Self::schedule_next_frame(window, cx);
        }

        // === TOP FLOATING APPLE SPACES BAR: [ 全部 ] [ 桌面 1 ] [ 桌面 2 ] ... [ + ] ===
        let mut space_pills = Vec::with_capacity(self.desktops.len() + 3);
        let all_selected = override_active_desktop.is_none() && self.desktop_filter.is_none();

        // 1. [ 全部 ] pill
        space_pills.push(
            div()
                .id("space-pill-all")
                .h(px(24.0))
                .px(px(14.0))
                .rounded_full()
                .cursor_pointer()
                .flex()
                .flex_row()
                .items_center()
                .justify_center()
                .bg(if all_selected {
                    rgba(0xFFFFFF2E)
                } else {
                    rgba(0x00000000)
                })
                .hover(|s| {
                    s.bg(if all_selected {
                        rgba(0xFFFFFF38)
                    } else {
                        rgba(0xFFFFFF18)
                    })
                })
                .on_click(cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    if this.dismiss_armed_delete(cx) {
                        return;
                    }
                    this.set_desktop_filter(None, window, cx);
                }))
                .child(
                    div()
                        .text_xs()
                        .font_weight(if all_selected {
                            gpui::FontWeight::SEMIBOLD
                        } else {
                            gpui::FontWeight::MEDIUM
                        })
                        .text_color(if all_selected {
                            rgba(0xFFFFFFFA)
                        } else {
                            rgba(0xFFFFFFA0)
                        })
                        .child("全部"),
                ),
        );

        // 2. [ 桌面 1 ] [ 桌面 2 ] ... pills
        //    Left click = filter, Left double-click = switch directly
        //    Right click = morph pill into red trash button; Left click trash = close; click anywhere else = cancel
        for (d_idx, d_info) in self.desktops.iter().enumerate() {
            let did = d_info.id;
            let is_cur = match override_active_desktop {
                Some(over_did) => over_did == did,
                None => d_info.is_current,
            };
            let is_filtered = match override_active_desktop {
                Some(over_did) => over_did == did,
                None => self.desktop_filter == Some(did),
            };
            let is_armed_delete = self.armed_delete_desktop == Some(did);
            let label = d_info
                .custom_name
                .clone()
                .unwrap_or_else(|| format!("桌面 {}", d_idx + 1));

            let pill = div()
                .id(("space-pill", d_idx))
                .h(px(24.0))
                .min_w(px(58.0))
                .px(px(14.0))
                .rounded_full()
                .cursor_pointer()
                .flex()
                .flex_row()
                .items_center()
                .justify_center()
                .gap(px(5.0))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        if this.desktops.len() <= 1 {
                            this.armed_delete_desktop = None;
                            this.transient_toast = Some((
                                SharedString::from("至少需要保留 1 个虚拟桌面"),
                                Instant::now(),
                            ));
                        } else if this.armed_delete_desktop == Some(did) {
                            this.armed_delete_desktop = None;
                        } else {
                            this.armed_delete_desktop = Some(did);
                        }
                        cx.notify();
                    }),
                )
                .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                    cx.stop_propagation();
                    if this.armed_delete_desktop == Some(did) {
                        this.remove_desktop_by_id(did, window, cx);
                    } else if this.dismiss_armed_delete(cx) {
                        // Clicking any other pill cancels the armed trash state
                    } else if event.click_count() >= 2 {
                        this.switch_to_desktop_only(did, window, cx);
                    } else {
                        this.set_desktop_filter(Some(did), window, cx);
                    }
                }));

            let pill = if is_armed_delete {
                pill.bg(rgba(0xFF453AD8))
                    .border_1()
                    .border_color(rgba(0xFFFFFF38))
                    .hover(|s| s.bg(rgba(0xFF453AF5)).border_color(rgba(0xFFFFFF66)))
                    .child(trash_icon())
            } else {
                pill.bg(if is_filtered {
                    rgba(0xFFFFFF2E)
                } else {
                    rgba(0x00000000)
                })
                .hover(|s| {
                    s.bg(if is_filtered {
                        rgba(0xFFFFFF38)
                    } else {
                        rgba(0xFFFFFF18)
                    })
                })
                .when(is_cur, |el| {
                    el.child(div().size(px(5.0)).rounded_full().bg(rgb(0x0A84FF)))
                })
                .child(
                    div()
                        .text_xs()
                        .font_weight(if is_filtered || is_cur {
                            gpui::FontWeight::SEMIBOLD
                        } else {
                            gpui::FontWeight::MEDIUM
                        })
                        .text_color(if is_filtered {
                            rgba(0xFFFFFFFA)
                        } else if is_cur {
                            rgba(0xFFFFFFDC)
                        } else {
                            rgba(0xFFFFFF99)
                        })
                        .child(label),
                )
            };

            space_pills.push(pill);
        }

        // Subtle hairline divider + minimalist [ + ] create-desktop button
        space_pills.push(
            div()
                .id("space-bar-sep")
                .w(px(1.0))
                .h(px(14.0))
                .mx(px(2.0))
                .bg(rgba(0xFFFFFF24)),
        );
        space_pills.push(
            div()
                .id("space-pill-add")
                .size(px(24.0))
                .rounded_full()
                .cursor_pointer()
                .flex()
                .items_center()
                .justify_center()
                .bg(rgba(0xFFFFFF10))
                .hover(|s| s.bg(rgba(0xFFFFFF28)))
                .on_click(cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    if this.dismiss_armed_delete(cx) {
                        return;
                    }
                    this.create_new_desktop(window, cx);
                }))
                .child(
                    div()
                        .text_sm()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(rgba(0xFFFFFFE8))
                        .child("+"),
                ),
        );

        // === FIXED-SIZE UNIFORM MISSION CONTROL GROUPS ===
        let show_group_labels = self.groups.len() > 1;
        let mut group_elements = Vec::with_capacity(self.groups.len());

        for (g_idx, group) in self.groups.iter().enumerate() {
            let desktop_id = group.desktop_id;

            let header = div()
                .id(("desktop-label", g_idx))
                .h(px(GROUP_HEADER_H))
                .flex()
                .flex_row()
                .items_center()
                .px(px(TOTAL_RING_PAD))
                .when_some(desktop_id, |el, did| {
                    el.cursor_pointer()
                        .hover(|s| s.opacity(1.0))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            if this.dismiss_armed_delete(cx) {
                                return;
                            }
                            this.switch_to_desktop_only(did, window, cx);
                        }))
                })
                .when(chrome_opacity < 0.999, |el| el.opacity(chrome_opacity))
                .when(show_group_labels, |el| {
                    el.child(
                        div()
                            .text_xs()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(if group.is_current {
                                rgba(0xFFFFFFDC)
                            } else {
                                rgba(0xFFFFFF78)
                            })
                            .child(if group.is_current {
                                format!("{} · 当前", group.title)
                            } else {
                                group.title.to_string()
                            }),
                    )
                });

            let mut section = div()
                .w_full()
                .flex()
                .flex_col()
                .when(g_idx > 0, |el| el.mt(px(GROUP_TOP_GAP)))
                .child(header);

            if group.cards.is_empty() {
                section = section.child(
                    div()
                        .id(("empty-space", g_idx))
                        .h(px(EMPTY_GROUP_H))
                        .flex()
                        .items_center()
                        .px_3()
                        .rounded_full()
                        .bg(rgba(0xFFFFFF0D))
                        .text_xs()
                        .text_color(rgba(0xFFFFFF66))
                        .when_some(desktop_id, |el, did| {
                            el.cursor_pointer()
                                .hover(|s| s.bg(rgba(0xFFFFFF1A)).text_color(rgba(0xFFFFFFD0)))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    if this.dismiss_armed_delete(cx) {
                                        return;
                                    }
                                    this.switch_to_desktop_only(did, window, cx);
                                }))
                        })
                        .when(chrome_opacity < 0.999, |el| el.opacity(chrome_opacity))
                        .child(if self.desktop_filter.is_some() {
                            "空白桌面 · 点击或按 Enter 切换到此桌面"
                        } else {
                            "空白桌面"
                        }),
                );
            } else {
                let mut rows_col = div().flex().flex_col().gap(px(ROW_GAP));

                for row_chunk in group.cards.chunks(cols) {
                    let mut row_el = div().flex().flex_row().gap(px(COL_GAP)).w_full();

                    for card in row_chunk {
                        let card_flat_index = card.flat_index;
                        let is_selected = card_flat_index == selected_flat;
                        let is_hovered = hovered_flat == Some(card_flat_index);
                        let target_hwnd = card.info.hwnd;

                        let has_live_surface = thumbs_rc.borrow().source_size(target_hwnd).is_some();

                        let (
                            card_dx,
                            card_dy,
                            gpui_card_opacity,
                            thumb_opacity_u8,
                            skip_normal_thumb_update,
                        ) = match exit_frame {
                            Some(ExitFrameState::ZoomWindow {
                                target_hwnd: zoom_hwnd,
                                other_thumb_opacity,
                                ..
                            }) => {
                                if target_hwnd == zoom_hwnd {
                                    (0.0, 0.0, chrome_opacity, 255u8, true)
                                } else {
                                    (0.0, 0.0, chrome_opacity, other_thumb_opacity, false)
                                }
                            }
                            Some(ExitFrameState::CrossDesktopWindow {
                                target_hwnd: cd_hwnd,
                                sample,
                                ..
                            }) => {
                                if target_hwnd == cd_hwnd {
                                    if sample.pan_t < 0.25 {
                                        let quick_out =
                                            (1.0 - sample.pan_t / 0.22).clamp(0.0, 1.0);
                                        let u8_op = (quick_out * 255.0).round() as u8;
                                        (sample.outgoing_grid_dx, 0.0, quick_out, u8_op, false)
                                    } else {
                                        (0.0, 0.0, 0.0, 0u8, true)
                                    }
                                } else {
                                    let out_u8 = (sample.outgoing_opacity * 255.0).round() as u8;
                                    (
                                        sample.outgoing_grid_dx,
                                        0.0,
                                        sample.outgoing_opacity,
                                        out_u8,
                                        false,
                                    )
                                }
                            }
                            Some(ExitFrameState::GentleFadeWindow {
                                target_hwnd: fade_hwnd,
                                from_slot_phys,
                                target_opacity,
                                other_thumb_opacity,
                                ..
                            }) => {
                                if target_hwnd == fade_hwnd {
                                    if has_live_surface && from_slot_phys.is_some() {
                                        (0.0, 0.0, chrome_opacity, 255u8, true)
                                    } else {
                                        (0.0, 0.0, target_opacity, 0u8, false)
                                    }
                                } else {
                                    (0.0, 0.0, chrome_opacity, other_thumb_opacity, false)
                                }
                            }
                            Some(ExitFrameState::DesktopSwitch {
                                slide_dx,
                                fade,
                                fade_u8,
                            }) => (slide_dx, 0.0, fade, fade_u8, false),
                            None => {
                                if let Some(ref ta) = self.tab_anim {
                                    if let Some(&from_b) = ta.shared_from_bounds.get(&target_hwnd) {
                                        if let Some(nat) = card_layouts
                                            .as_ref()
                                            .and_then(|m| m.get(&card_flat_index))
                                        {
                                            let cur = lerp_bounds(
                                                from_b,
                                                nat.thumb_window_bounds,
                                                tab_eased,
                                            );
                                            let dx = f32::from(
                                                cur.origin.x - nat.thumb_window_bounds.origin.x,
                                            );
                                            let dy = f32::from(
                                                cur.origin.y - nat.thumb_window_bounds.origin.y,
                                            );
                                            (dx, dy, 1.0, 255u8, false)
                                        } else {
                                            (0.0, 0.0, 1.0, 255u8, false)
                                        }
                                    } else if ta.entering_hwnds.contains(&target_hwnd) {
                                        let u8_op = (tab_enter_opacity * 255.0).round() as u8;
                                        (tab_enter_dx, 0.0, tab_enter_opacity, u8_op, false)
                                    } else {
                                        (0.0, 0.0, 1.0, 255u8, false)
                                    }
                                } else {
                                    (0.0, 0.0, 1.0, 255u8, false)
                                }
                            }
                        };

                        let thumbs_for_card = thumbs_rc.clone();
                        let clip_for_card = viewport_clip_rc.clone();
                        let last_bounds_for_card = last_thumb_bounds_rc.clone();

                        let icon_px = if label_pos == LabelPosition::Center {
                            18.0
                        } else {
                            16.0
                        };
                        let icon_el = if let Some(ref render_img) = card.icon {
                            div()
                                .size(px(icon_px))
                                .flex_shrink_0()
                                .child(img(render_img.clone()).size(px(icon_px)).rounded(px(3.0)))
                        } else {
                            div()
                                .size(px(icon_px))
                                .flex_shrink_0()
                                .rounded(px(4.0))
                                .bg(rgb(card.accent_color))
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_xs()
                                .font_weight(gpui::FontWeight::BOLD)
                                .text_color(rgb(0xFFFFFF))
                                .child(card.monogram.clone())
                        };

                        let title_text = if card.info.is_minimized {
                            format!("{} · 最小化", card.info.title)
                        } else {
                            card.info.title.clone()
                        };

                        let pill_max_w = if label_pos.is_embedded() {
                            (preview_w - 18.0).clamp(96.0, 270.0)
                        } else {
                            preview_w.max(120.0)
                        };
                        let label_pill = div()
                            .max_w(px(pill_max_w))
                            .h(px(if label_pos == LabelPosition::Center {
                                28.0
                            } else {
                                BELOW_LABEL_H
                            }))
                            .px(px(10.0))
                            .rounded_full()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.0))
                            .bg(if is_selected {
                                rgba(0x0A84FFEC)
                            } else if is_hovered {
                                rgba(0x111622DC)
                            } else if label_pos.is_embedded() {
                                rgba(0x0B0F18C8)
                            } else {
                                rgba(0x0A0D1478)
                            })
                            .when(label_pos.is_embedded(), |el| {
                                el.border_1().border_color(if is_selected {
                                    rgba(0xFFFFFF44)
                                } else {
                                    rgba(0xFFFFFF22)
                                })
                            })
                            .child(icon_el)
                            .child(
                                div()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .text_xs()
                                    .font_weight(if is_selected {
                                        gpui::FontWeight::SEMIBOLD
                                    } else {
                                        gpui::FontWeight::MEDIUM
                                    })
                                    .text_color(if is_selected || is_hovered {
                                        rgb(0xFFFFFF)
                                    } else {
                                        rgba(0xFFFFFFEC)
                                    })
                                    .child(title_text),
                            );

                        // Multi-ring transparent-interior Apple blue outer glow + soft ambient depth
                        // When Scene B floating focus ring is active, suppress static blue ring on selected card.
                        let show_static_selected_ring = is_selected && floating_focus_rect.is_none();
                        let (outer_glow, mid_glow, inner_ring) = if show_static_selected_ring {
                            (rgba(0x0A84FF24), rgba(0x0A84FF5A), rgb(0x0A84FF).into())
                        } else if is_hovered {
                            (rgba(0xFFFFFF18), rgba(0xFFFFFF36), rgba(0xFFFFFF8C))
                        } else if preview_fit == PreviewFit::Contain {
                            (rgba(0x00000010), rgba(0x0000001E), rgba(0xFFFFFF14))
                        } else {
                            (rgba(0x00000014), rgba(0x00000028), rgba(0x00000000))
                        };

                        let thumb_pane = div()
                            .relative()
                            .w(px(preview_w))
                            .h(px(preview_h))
                            .rounded(px(4.0))
                            .bg(if has_live_surface {
                                rgba(0x00000000)
                            } else {
                                rgba(0xFFFFFF16)
                            })
                            .flex()
                            .items_center()
                            .justify_center()
                            .when(!has_live_surface, |el| {
                                el.child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .items_center()
                                        .gap_2()
                                        .when_some(card.icon.clone(), |inner, r_img| {
                                            inner.child(
                                                img(r_img).size(px(36.0)).rounded(px(8.0)),
                                            )
                                        })
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(rgba(0xFFFFFF88))
                                                .child(if card.info.process_name.is_empty() {
                                                    "已最小化".to_string()
                                                } else {
                                                    card.info.process_name.clone()
                                                }),
                                        ),
                                )
                            })
                            .child(
                                canvas(
                                    |bounds, _, _| bounds,
                                    move |bounds, _, window, _| {
                                        if !is_visible {
                                            return;
                                        }
                                        if !is_exiting {
                                            last_bounds_for_card
                                                .borrow_mut()
                                                .insert(target_hwnd, bounds);
                                        }
                                        if skip_normal_thumb_update {
                                            return;
                                        }
                                        let sf = window.scale_factor();
                                        let clip = clip_for_card.borrow().unwrap_or(bounds);
                                        thumbs_for_card.borrow_mut().update_slot(
                                            target_hwnd,
                                            bounds,
                                            clip,
                                            sf,
                                            preview_fit,
                                            thumb_opacity_u8,
                                        );
                                    },
                                )
                                .absolute()
                                .inset_0()
                                .size_full(),
                            );

                        let (thumb_pane, below_label) = match label_pos {
                            LabelPosition::BottomRight => (
                                thumb_pane.child(
                                    div()
                                        .absolute()
                                        .bottom(px(8.0))
                                        .right(px(8.0))
                                        .child(label_pill),
                                ),
                                None,
                            ),
                            LabelPosition::BottomLeft => (
                                thumb_pane.child(
                                    div()
                                        .absolute()
                                        .bottom(px(8.0))
                                        .left(px(8.0))
                                        .child(label_pill),
                                ),
                                None,
                            ),
                            LabelPosition::Center => (
                                thumb_pane.child(
                                    div()
                                        .absolute()
                                        .inset_0()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .px_2()
                                        .child(label_pill),
                                ),
                                None,
                            ),
                            LabelPosition::Below => (thumb_pane, Some(label_pill)),
                        };

                        let mut item_el = div()
                            .id(("mission-item", card_flat_index))
                            .when(card_dx != 0.0 || card_dy != 0.0, |el| {
                                el.relative().left(px(card_dx)).top(px(card_dy))
                            })
                            .when(gpui_card_opacity < 0.999, |el| el.opacity(gpui_card_opacity))
                            .w(px(tile_outer_w))
                            .flex_shrink_0()
                            .h(px(item_total_h))
                            .flex()
                            .flex_col()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                                if this.exit_anim.is_some() {
                                    return;
                                }
                                if *hovered {
                                    this.hovered_flat = Some(card_flat_index);
                                } else if this.hovered_flat == Some(card_flat_index) {
                                    this.hovered_flat = None;
                                }
                                cx.notify();
                            }))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                if this.exit_anim.is_some() {
                                    return;
                                }
                                if this.dismiss_armed_delete(cx) {
                                    return;
                                }
                                this.selected_flat = card_flat_index;
                                this.activate_window(target_hwnd, window, cx);
                            }))
                            .child(
                                div()
                                    .w(px(tile_outer_w))
                                    .h(px(preview_h + TOTAL_RING_PAD * 2.0))
                                    .rounded(px(12.0))
                                    .border(px(OUTER_GLOW_PAD))
                                    .border_color(outer_glow)
                                    .bg(rgba(0x00000000))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        div()
                                            .w(px(preview_w + (MID_GLOW_PAD + INNER_RING_PAD) * 2.0))
                                            .h(px(preview_h + (MID_GLOW_PAD + INNER_RING_PAD) * 2.0))
                                            .rounded(px(9.0))
                                            .border(px(MID_GLOW_PAD))
                                            .border_color(mid_glow)
                                            .bg(rgba(0x00000000))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .child(
                                                div()
                                                    .w(px(preview_w + INNER_RING_PAD * 2.0))
                                                    .h(px(preview_h + INNER_RING_PAD * 2.0))
                                                    .rounded(px(6.0))
                                                    .border(px(INNER_RING_PAD))
                                                    .border_color(inner_ring)
                                                    .bg(rgba(0x00000000))
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .child(thumb_pane),
                                            ),
                                    ),
                            );

                        if let Some(pill) = below_label {
                            item_el = item_el.child(div().mt(px(BELOW_LABEL_GAP)).child(pill));
                        }

                        row_el = row_el.child(item_el);
                    }

                    rows_col = rows_col.child(row_el);
                }

                section = section.child(rows_col);
            }

            group_elements.push(section);
        }

        let show_scrollbar = max_scroll > 1.0;
        let thumb_ratio = (view_h / total_content_h.max(1.0)).clamp(0.10, 1.0);
        let thumb_h = (view_h * thumb_ratio).max(32.0);
        let thumb_top = if max_scroll > 0.0 {
            (scroll_y / max_scroll) * (view_h - thumb_h).max(0.0)
        } else {
            0.0
        };

        let active_toast = self
            .transient_toast
            .as_ref()
            .and_then(|(msg, at)| (at.elapsed() < Duration::from_secs(3)).then(|| msg.clone()))
            .or_else(|| {
                (self.hold_active && self.hold_navigated).then(|| {
                    let mod_name = self.config.parsed_hotkey().primary.display_name();
                    SharedString::from(format!("松开 {mod_name} 立即跳转 · Esc 取消"))
                })
            });

        let root = div()
            .id("windeck-mission-control")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            .size_full()
            .flex()
            .flex_col()
            .bg(rgba(0x00000001))
            .text_color(rgb(0xFFFFFF));

        let root = if self.update_dialog_open {
            root.child(self.render_update_dialog(cx))
        } else if self.settings_open {
            root.child(self.render_settings_view(cx))
        } else {
            root.on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _, _, cx| {
                    this.dismiss_armed_delete(cx);
                }),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                if this.exit_anim.is_some() {
                    return;
                }
                if this.dismiss_armed_delete(cx) {
                    return;
                }
                this.hide(cx);
            }))
            // === TOP FLOATING APPLE SPACES BAR ===
            .child(
                div()
                    .h(px(TOP_SPACES_BAR_H))
                    .flex_shrink_0()
                    .w_full()
                    .pt(px(18.0))
                    .pb(px(8.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_center()
                    .when(spaces_bar_opacity < 0.999, |el| {
                        el.opacity(spaces_bar_opacity)
                    })
                    .child(
                        div()
                            .id("space-bar-capsule")
                            .px(px(6.0))
                            .py(px(4.0))
                            .rounded_full()
                            .bg(rgba(0x0B0F1866))
                            .border_1()
                            .border_color(rgba(0xFFFFFF18))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(4.0))
                            .on_click(cx.listener(|this, _, _, cx| {
                                cx.stop_propagation();
                                this.dismiss_armed_delete(cx);
                            }))
                            .children(space_pills),
                    ),
            )
            // === FIXED-SIZE MISSION CONTROL CANVAS ===
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_hidden()
                    .child(
                        canvas(
                            move |bounds, _, _| {
                                let w = f32::from(bounds.size.width);
                                let h = f32::from(bounds.size.height);
                                if w > 100.0 && h > 100.0 {
                                    *viewport_clip_rc.borrow_mut() = Some(bounds);
                                    viewport_w_cell.set(w);
                                    viewport_h_cell.set(h);
                                }
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .inset_0()
                        .size_full(),
                    )
                    // Scene A leaving non-live (minimized) fallback cards fading out smoothly
                    .children(leaving_fallback_cards.into_iter().map(move |card| {
                        let left_x = f32::from(card.thumb_bounds.origin.x) + tab_leave_dx;
                        let top_y = f32::from(card.thumb_bounds.origin.y) - vp_origin_y;
                        div()
                            .absolute()
                            .left(px(left_x))
                            .top(px(top_y))
                            .w(card.thumb_bounds.size.width)
                            .h(card.thumb_bounds.size.height)
                            .rounded(px(4.0))
                            .bg(rgba(0xFFFFFF16))
                            .opacity(tab_leave_opacity)
                            .flex()
                            .flex_col()
                            .items_center()
                            .justify_center()
                            .gap_2()
                            .when_some(card.icon, |inner, r_img| {
                                inner.child(img(r_img).size(px(36.0)).rounded(px(8.0)))
                            })
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgba(0xFFFFFF88))
                                    .child(card.fallback_label),
                            )
                    }))
                    .child(
                        div()
                            .relative()
                            .top(px(optical_top_pad - scroll_y))
                            .w_full()
                            .flex()
                            .flex_col()
                            .items_center()
                            .px(px(CANVAS_PAD_X))
                            .pt(px(CONTENT_PAD_TOP))
                            .pb(px(CONTENT_PAD_BOTTOM))
                            .child(
                                div()
                                    .w(px(grid_total_w))
                                    .flex()
                                    .flex_col()
                                    .children(group_elements),
                            ),
                    )
                    // Scene B smooth floating 3-ring focus frame
                    .when_some(floating_focus_rect, |el, rect| {
                        let ring_x = f32::from(rect.origin.x);
                        let ring_y = f32::from(rect.origin.y) - scroll_y;
                        let ring_w = f32::from(rect.size.width);
                        let ring_h = f32::from(rect.size.height);
                        let mid_w = (ring_w - OUTER_GLOW_PAD * 2.0).max(1.0);
                        let mid_h = (ring_h - OUTER_GLOW_PAD * 2.0).max(1.0);
                        let inner_w = (mid_w - MID_GLOW_PAD * 2.0).max(1.0);
                        let inner_h = (mid_h - MID_GLOW_PAD * 2.0).max(1.0);
                        el.child(
                            div()
                                .absolute()
                                .left(px(ring_x))
                                .top(px(ring_y))
                                .w(px(ring_w))
                                .h(px(ring_h))
                                .rounded(px(12.0))
                                .border(px(OUTER_GLOW_PAD))
                                .border_color(rgba(0x0A84FF24))
                                .bg(rgba(0x00000000))
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(
                                    div()
                                        .w(px(mid_w))
                                        .h(px(mid_h))
                                        .rounded(px(9.0))
                                        .border(px(MID_GLOW_PAD))
                                        .border_color(rgba(0x0A84FF5A))
                                        .bg(rgba(0x00000000))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .child(
                                            div()
                                                .w(px(inner_w))
                                                .h(px(inner_h))
                                                .rounded(px(6.0))
                                                .border(px(INNER_RING_PAD))
                                                .border_color(rgb(0x0A84FF))
                                                .bg(rgba(0x00000000)),
                                        ),
                                ),
                        )
                    })
                    // Scene C cross-desktop directional badge ("‹ 桌面 1" / "桌面 3 ›")
                    .when_some(directional_badge, |el, (label, dir, dx, opacity)| {
                        let going_left = dir < 0.0;
                        el.child(
                            div()
                                .absolute()
                                .top(px(28.0))
                                .left_0()
                                .right_0()
                                .flex()
                                .justify_center()
                                .opacity(opacity)
                                .child(
                                    div()
                                        .relative()
                                        .left(px(dx))
                                        .h(px(38.0))
                                        .px(px(18.0))
                                        .rounded_full()
                                        .bg(rgba(0x0B0F18CC))
                                        .border_1()
                                        .border_color(rgba(0xFFFFFF2A))
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .gap(px(10.0))
                                        .when(going_left, |pill| {
                                            pill.child(
                                                div()
                                                    .text_sm()
                                                    .font_weight(gpui::FontWeight::BOLD)
                                                    .text_color(rgb(0x0A84FF))
                                                    .child("‹"),
                                            )
                                        })
                                        .child(
                                            div()
                                                .text_sm()
                                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                                .text_color(rgba(0xFFFFFFFA))
                                                .child(label),
                                        )
                                        .when(!going_left, |pill| {
                                            pill.child(
                                                div()
                                                    .text_sm()
                                                    .font_weight(gpui::FontWeight::BOLD)
                                                    .text_color(rgb(0x0A84FF))
                                                    .child("›"),
                                            )
                                        }),
                                ),
                        )
                    })
                    // Scene C cross-desktop fallback stage card (for minimized or other-monitor target windows)
                    .when_some(
                        cross_desktop_fallback,
                        |el, (icon, title, fb_dx, fb_scale, fb_opacity)| {
                            let card_w = (preview_w * fb_scale).max(160.0);
                            let card_h = (preview_h * fb_scale).max(100.0);
                            el.child(
                                div()
                                    .absolute()
                                    .inset_0()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .opacity(fb_opacity)
                                    .child(
                                        div()
                                            .relative()
                                            .left(px(fb_dx))
                                            .w(px(card_w))
                                            .h(px(card_h))
                                            .rounded(px(12.0))
                                            .bg(rgba(0x0B0F18BA))
                                            .border_1()
                                            .border_color(rgba(0x0A84FF99))
                                            .flex()
                                            .flex_col()
                                            .items_center()
                                            .justify_center()
                                            .gap(px(10.0))
                                            .px(px(16.0))
                                            .when_some(icon, |inner, r_img| {
                                                inner.child(
                                                    img(r_img).size(px(42.0)).rounded(px(10.0)),
                                                )
                                            })
                                            .child(
                                                div()
                                                    .max_w_full()
                                                    .overflow_hidden()
                                                    .whitespace_nowrap()
                                                    .text_ellipsis()
                                                    .text_xs()
                                                    .font_weight(gpui::FontWeight::MEDIUM)
                                                    .text_color(rgba(0xFFFFFFF0))
                                                    .child(title),
                                            ),
                                    ),
                            )
                        },
                    )
                    // Post-paint DWM choreographer for Scene A leaving live thumbnails & Scene C zoom/fade targets
                    .child({
                        let thumbs_post = thumbs_rc.clone();
                        let clip_post = self.viewport_clip.clone();
                        let leaving_u8 = (tab_leave_opacity * 255.0).round() as u8;
                        canvas(
                            |_, _, _| {},
                            move |_, _, window, _| {
                                if !is_visible {
                                    return;
                                }
                                let sf = window.scale_factor();
                                let mut thumbs = thumbs_post.borrow_mut();

                                if !leaving_live_cards.is_empty() {
                                    if let Some(clip) = *clip_post.borrow() {
                                        for &(hwnd, shifted_bounds) in &leaving_live_cards {
                                            thumbs.update_slot(
                                                hwnd,
                                                shifted_bounds,
                                                clip,
                                                sf,
                                                preview_fit,
                                                leaving_u8,
                                            );
                                        }
                                    }
                                }

                                match exit_frame {
                                    Some(ExitFrameState::ZoomWindow {
                                        target_hwnd,
                                        from_slot_phys,
                                        to_dest_phys,
                                        fit,
                                        eased_pos,
                                        eased_uncrop,
                                        other_thumb_opacity,
                                    }) => {
                                        if other_thumb_opacity == 0 {
                                            thumbs.hide_all_except(Some(target_hwnd));
                                        }
                                        thumbs.update_zoom_to_rect(
                                            target_hwnd,
                                            from_slot_phys,
                                            to_dest_phys,
                                            host_size_phys,
                                            fit,
                                            eased_pos,
                                            eased_uncrop,
                                            255,
                                        );
                                    }
                                    Some(ExitFrameState::CrossDesktopWindow {
                                        target_hwnd,
                                        sample,
                                        has_live_surface,
                                    }) => {
                                        if sample.outgoing_opacity <= 0.001 {
                                            thumbs.hide_all_except(Some(target_hwnd));
                                        }
                                        if sample.pan_t >= 0.25 && has_live_surface {
                                            if let Some(dest_phys) = sample.target_dest_phys {
                                                let op_u8 =
                                                    (sample.target_opacity * 255.0).round() as u8;
                                                thumbs.update_full_source_rect(
                                                    target_hwnd,
                                                    dest_phys,
                                                    host_size_phys,
                                                    op_u8,
                                                );
                                            }
                                        }
                                    }
                                    Some(ExitFrameState::GentleFadeWindow {
                                        target_hwnd,
                                        from_slot_phys: Some(from_phys),
                                        fit,
                                        target_opacity,
                                        target_scale,
                                        other_thumb_opacity,
                                    }) => {
                                        if other_thumb_opacity == 0 {
                                            thumbs.hide_all_except(Some(target_hwnd));
                                        }
                                        let scaled =
                                            scale_rect_around_center(from_phys, target_scale);
                                        let op_u8 = (target_opacity * 255.0).round() as u8;
                                        thumbs.update_zoom_to_rect(
                                            target_hwnd,
                                            scaled,
                                            scaled,
                                            host_size_phys,
                                            fit,
                                            0.0,
                                            0.0,
                                            op_u8,
                                        );
                                    }
                                    _ => {}
                                }
                            },
                        )
                        .absolute()
                        .w(px(0.0))
                        .h(px(0.0))
                    })
                    .when(show_scrollbar, |el| {
                        el.child(
                            div()
                                .absolute()
                                .top(px(thumb_top + 6.0))
                                .right(px(5.0))
                                .w(px(4.0))
                                .h(px((thumb_h - 12.0).max(22.0)))
                                .rounded_full()
                                .bg(rgba(0xFFFFFF38))
                                .when(chrome_opacity < 0.999, |s| s.opacity(chrome_opacity)),
                        )
                    }),
            )
            // === BOTTOM-RIGHT QUIET FLOATING SETTINGS ICON ===
            .child(
                self.render_settings_trigger(cx)
                    .when(chrome_opacity < 0.999, |el| el.opacity(chrome_opacity)),
            )
        };

        root
            .when_some(active_toast, |el, msg| {
                el.child(
                    div()
                        .absolute()
                        .bottom(px(16.0))
                        .left_0()
                        .right_0()
                        .flex()
                        .justify_center()
                        .child(
                            div()
                                .px_4()
                                .py(px(6.0))
                                .rounded_full()
                                .bg(rgba(0x0B0F18DC))
                                .border_1()
                                .border_color(rgba(0xFFFFFF28))
                                .text_xs()
                                .text_color(rgba(0xFFFFFFF2))
                                .child(msg),
                        ),
                )
            })
            .when_some(self.hotkey_warning.clone(), |el, warn| {
                el.child(
                    div()
                        .absolute()
                        .bottom(px(16.0))
                        .left_0()
                        .right_0()
                        .flex()
                        .justify_center()
                        .child(
                            div()
                                .px_4()
                                .py(px(6.0))
                                .rounded_full()
                                .bg(rgba(0xEF444455))
                                .text_xs()
                                .text_color(rgba(0xFFFFFFEB))
                                .child(warn),
                        ),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use windows::Win32::UI::WindowsAndMessaging::{GetDesktopWindow, GetWindowThreadProcessId};

    use super::*;

    #[test]
    fn alive_with_pid_check_validates_real_win32_hwnd_and_pid() {
        assert!(!is_window_alive_with_pid(0, 0));
        assert!(!is_window_alive_with_pid(0, 1234));

        let desktop_hwnd = unsafe { GetDesktopWindow() };
        let raw = desktop_hwnd.0 as isize;
        let mut real_pid = 0u32;
        unsafe { GetWindowThreadProcessId(desktop_hwnd, Some(&mut real_pid)) };

        assert!(is_window_alive_with_pid(raw, 0));
        if real_pid != 0 {
            assert!(is_window_alive_with_pid(raw, real_pid));
            assert!(!is_window_alive_with_pid(raw, real_pid.wrapping_add(1)));
        }
    }
}
