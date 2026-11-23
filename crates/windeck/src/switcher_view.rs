use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    App, Bounds, BoxShadow, Context, FocusHandle, Focusable, KeyDownEvent, Pixels, RenderImage,
    ScrollDelta, ScrollWheelEvent, SharedString, Window, canvas, div, hsla, img, point, prelude::*,
    px, rgb, rgba,
};
use smallvec::smallvec;
use win_platform::{
    DesktopId, IconBgra, VirtualDesktopClient, VirtualDesktopInfo, WindowInfo, enumerate_monitors,
    enumerate_windows, extract_window_icon, hide_overlay, show_and_focus_overlay, switch_to_window,
};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, IsWindow};

use crate::config::{AppConfig, append_log};
use crate::nav::{NavDirection, navigate};
use crate::thumb_manager::ThumbnailManager;

const GROUP_HEADER_H: f32 = 34.0;
const GROUP_TOP_GAP: f32 = 20.0;
const CARD_HEADER_H: f32 = 36.0;
const CARD_THUMB_H: f32 = 178.0;
const CARD_TOTAL_H: f32 = CARD_HEADER_H + CARD_THUMB_H + 10.0; // header + well + vertical borders/padding
const ROW_GAP: f32 = 14.0;
const EMPTY_GROUP_H: f32 = 42.0;
const CONTENT_PAD_TOP: f32 = 14.0;
const CONTENT_PAD_BOTTOM: f32 = 24.0;

#[derive(Clone)]
struct WindowCard {
    flat_index: usize,
    info: WindowInfo,
    desktop_label: SharedString,
    icon: Option<Arc<RenderImage>>,
    monogram: SharedString,
    accent_color: u32,
}

#[derive(Clone)]
struct DesktopGroup {
    title: SharedString,
    desktop_id: Option<DesktopId>,
    is_current: bool,
    is_pinned_group: bool,
    cards: Vec<WindowCard>,
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
        0x3B82F6, 0x10B981, 0xF59E0B, 0xEC4899, 0x8B5CF6, 0x06B6D4, 0xEF4444, 0x14B8A6,
    ];
    let hash: usize = source
        .bytes()
        .fold(0usize, |acc, b| acc.wrapping_mul(31).wrapping_add(b as usize));
    (SharedString::from(ch), palette[hash % palette.len()])
}

pub struct SwitcherView {
    focus_handle: FocusHandle,
    gpui_hwnd: HWND,
    vdm: Arc<VirtualDesktopClient>,
    config: AppConfig,
    visible: bool,
    shown_at: Option<Instant>,
    prev_foreground_hwnd: isize,
    desktops: Vec<VirtualDesktopInfo>,
    groups: Vec<DesktopGroup>,
    flat_hwnds: Vec<isize>,
    selected_flat: usize,
    scroll_y: f32,
    viewport_clip: Rc<RefCell<Option<Bounds<Pixels>>>>,
    viewport_width_px: Rc<Cell<f32>>,
    viewport_height_px: Rc<Cell<f32>>,
    icon_cache: HashMap<String, Option<Arc<RenderImage>>>,
    thumbs: Rc<RefCell<ThumbnailManager>>,
    last_open_ms: f64,
    last_enum_ms: f64,
    total_window_count: usize,
    status_notice: Option<SharedString>,
    hotkey_warning: Option<SharedString>,
}

impl Focusable for SwitcherView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl SwitcherView {
    pub fn new(
        gpui_hwnd: HWND,
        vdm: Arc<VirtualDesktopClient>,
        config: AppConfig,
        hotkey_warning: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle);

        Self {
            focus_handle,
            gpui_hwnd,
            vdm,
            config,
            visible: false,
            shown_at: None,
            prev_foreground_hwnd: 0,
            desktops: Vec::new(),
            groups: Vec::new(),
            flat_hwnds: Vec::new(),
            selected_flat: 0,
            scroll_y: 0.0,
            viewport_clip: Rc::new(RefCell::new(None)),
            viewport_width_px: Rc::new(Cell::new(1400.0)),
            viewport_height_px: Rc::new(Cell::new(720.0)),
            icon_cache: HashMap::new(),
            thumbs: Rc::new(RefCell::new(ThumbnailManager::new(gpui_hwnd))),
            last_open_ms: 0.0,
            last_enum_ms: 0.0,
            total_window_count: 0,
            status_notice: None,
            hotkey_warning: hotkey_warning.map(SharedString::from),
        }
    }

    pub fn update_config(&mut self, new_config: AppConfig, window: &mut Window, cx: &mut Context<Self>) {
        window.set_background_appearance(new_config.background);
        self.config = new_config;
        self.status_notice = Some(SharedString::from(format!(
            "配置已重载（快捷键：{}）",
            self.config.hotkey
        )));
        cx.notify();
    }

    pub fn handle_explorer_restarted(&mut self) {
        let res = self.vdm.reconnect();
        append_log(&format!("Explorer restarted -> VDM reconnect: {:?}", res));
    }

    pub fn handle_window_activation_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.visible || !self.config.hide_on_blur {
            return;
        }
        if !window.is_window_active() {
            // Grace period right after showing so initial focus transition does not immediately hide.
            if let Some(shown_at) = self.shown_at {
                if shown_at.elapsed() < Duration::from_millis(180) {
                    return;
                }
            }
            self.hide(cx);
        }
    }

    pub fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.visible {
            self.hide(cx);
        } else {
            self.show_and_refresh(window, cx);
        }
    }

    pub fn hide(&mut self, cx: &mut Context<Self>) {
        self.thumbs.borrow_mut().unregister_all();
        hide_overlay(self.gpui_hwnd);
        self.visible = false;
        self.shown_at = None;
        cx.notify();
    }

    pub fn show_and_refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let t_start = Instant::now();
        self.prev_foreground_hwnd = unsafe { GetForegroundWindow() }.0 as isize;
        self.status_notice = None;
        self.scroll_y = 0.0;

        // 1. Refresh desktops + windows + cached icons
        self.refresh_snapshot();

        // 2. Register DWM live thumbnails and mark visible BEFORE ShowWindow so the very first WM_SHOWWINDOW frame is fresh (zero stale flash)
        self.thumbs.borrow_mut().sync_targets(&self.flat_hwnds);
        self.visible = true;
        self.shown_at = Some(Instant::now());
        window.set_background_appearance(self.config.background);

        // 3. Position & reveal overlay window on chosen monitor
        if let Ok((phys_rc, scale)) =
            show_and_focus_overlay(self.gpui_hwnd, self.config.monitor, 0.86, 0.84)
        {
            let logical_w = ((phys_rc.right - phys_rc.left) as f32 / scale).max(640.0);
            let logical_h = ((phys_rc.bottom - phys_rc.top) as f32 / scale).max(420.0);
            self.viewport_width_px.set((logical_w - 48.0).max(400.0));
            self.viewport_height_px.set((logical_h - 92.0).max(260.0));
        }
        window.focus(&self.focus_handle);
        self.ensure_selected_visible();
        window.refresh();

        self.last_open_ms = t_start.elapsed().as_secs_f64() * 1000.0;
        append_log(&format!(
            "OPEN total={:.2}ms enum={:.2}ms desktops={} windows={} thumbs={}",
            self.last_open_ms,
            self.last_enum_ms,
            self.desktops.len(),
            self.total_window_count,
            self.thumbs.borrow().active_count(),
        ));
        cx.notify();
    }

    pub fn refresh_snapshot(&mut self) {
        let t_enum = Instant::now();
        let desktops = self.vdm.get_desktops().unwrap_or_default();
        let monitors = enumerate_monitors();
        let (windows, _) = enumerate_windows(
            &self.vdm,
            &desktops,
            &monitors,
            Some(self.gpui_hwnd.0 as isize),
            false,
        )
        .unwrap_or_default();
        self.last_enum_ms = t_enum.elapsed().as_secs_f64() * 1000.0;

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

        if !pinned_windows.is_empty() {
            let mut cards = Vec::with_capacity(pinned_windows.len());
            for info in pinned_windows {
                let icon = self.cached_icon(&info);
                let (monogram, accent_color) = monogram_and_color(&info.process_name, &info.title);
                flat_hwnds.push(info.hwnd);
                cards.push(WindowCard {
                    flat_index: flat_idx,
                    info,
                    desktop_label: SharedString::from("所有桌面"),
                    icon,
                    monogram,
                    accent_color,
                });
                flat_idx += 1;
            }
            groups.push(DesktopGroup {
                title: SharedString::from("固定到所有桌面"),
                desktop_id: None,
                is_current: false,
                is_pinned_group: true,
                cards,
            });
        }

        for (d_idx, d_info) in desktops.iter().enumerate() {
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
                    desktop_label: label_shared.clone(),
                    icon,
                    monogram,
                    accent_color,
                });
                flat_idx += 1;
            }
            groups.push(DesktopGroup {
                title: label_shared,
                desktop_id: Some(d_info.id),
                is_current: d_info.is_current,
                is_pinned_group: false,
                cards,
            });
        }

        if !unassigned.is_empty() {
            let mut cards = Vec::with_capacity(unassigned.len());
            for info in unassigned {
                let icon = self.cached_icon(&info);
                let (monogram, accent_color) = monogram_and_color(&info.process_name, &info.title);
                flat_hwnds.push(info.hwnd);
                cards.push(WindowCard {
                    flat_index: flat_idx,
                    info,
                    desktop_label: SharedString::from("未分配桌面"),
                    icon,
                    monogram,
                    accent_color,
                });
                flat_idx += 1;
            }
            groups.push(DesktopGroup {
                title: SharedString::from("其他窗口"),
                desktop_id: None,
                is_current: false,
                is_pinned_group: false,
                cards,
            });
        }

        self.desktops = desktops;
        self.total_window_count = flat_hwnds.len();
        self.groups = groups;
        self.flat_hwnds = flat_hwnds;

        // Smart initial selection: prefer first non-current-foreground window on current desktop, else 0.
        self.selected_flat = self.pick_initial_selection();
    }

    fn pick_initial_selection(&self) -> usize {
        if self.flat_hwnds.is_empty() {
            return 0;
        }
        if let Some(cur_group) = self.groups.iter().find(|g| g.is_current && !g.cards.is_empty()) {
            if let Some(card) = cur_group
                .cards
                .iter()
                .find(|c| c.info.hwnd != self.prev_foreground_hwnd)
            {
                return card.flat_index;
            }
            return cur_group.cards[0].flat_index;
        }
        0
    }

    fn cached_icon(&mut self, info: &WindowInfo) -> Option<Arc<RenderImage>> {
        // Cache by exe_path for regular apps; by HWND for UWP ApplicationFrameHost so each UWP app keeps its own icon!
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
        let decoded = extract_window_icon(info.hwnd, &info.exe_path)
            .and_then(icon_bgra_to_render_image);
        self.icon_cache.insert(cache_key, decoded.clone());
        decoded
    }

    fn group_sizes(&self) -> Vec<usize> {
        self.groups.iter().map(|g| g.cards.len()).collect()
    }

    fn current_cols(&self) -> usize {
        self.config.resolve_columns(self.viewport_width_px.get())
    }

    /// Computes `(total_content_height, selected_card_y_range)` deterministically.
    fn compute_layout_metrics(&self, cols: usize) -> (f32, Option<(f32, f32)>) {
        let cols = cols.max(1);
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
                let row_bottom = row_top + CARD_TOTAL_H;
                let start = r * cols;
                let end = (start + cols).min(group.cards.len());
                for card in &group.cards[start..end] {
                    if card.flat_index == self.selected_flat {
                        // Include group header when on row 0 so jumping to a new desktop reveals its title too.
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

    fn clamp_scroll(&mut self, cols: usize) {
        let (total_h, _) = self.compute_layout_metrics(cols);
        let view_h = self.viewport_height_px.get().max(100.0);
        let max_scroll = (total_h - view_h).max(0.0);
        self.scroll_y = self.scroll_y.clamp(0.0, max_scroll);
    }

    fn ensure_selected_visible(&mut self) {
        let cols = self.current_cols();
        let (total_h, selected_range) = self.compute_layout_metrics(cols);
        let view_h = self.viewport_height_px.get().max(100.0);
        let max_scroll = (total_h - view_h).max(0.0);

        if let Some((top, bottom)) = selected_range {
            let margin = 12.0;
            if top - margin < self.scroll_y {
                self.scroll_y = (top - margin).max(0.0);
            } else if bottom + margin > self.scroll_y + view_h {
                self.scroll_y = (bottom + margin - view_h).min(max_scroll);
            }
        }
        self.scroll_y = self.scroll_y.clamp(0.0, max_scroll);
    }

    fn activate_window(&mut self, target_hwnd: isize, cx: &mut Context<Self>) {
        // Guard against a window that closed while the overlay was open
        let raw = HWND(target_hwnd as *mut std::ffi::c_void);
        if !unsafe { IsWindow(Some(raw)) }.as_bool() {
            self.status_notice = Some(SharedString::from("该窗口已关闭，已刷新列表"));
            self.refresh_snapshot();
            self.thumbs.borrow_mut().sync_targets(&self.flat_hwnds);
            cx.notify();
            return;
        }

        // Release thumbnails first before hiding & switching
        self.thumbs.borrow_mut().unregister_all();
        self.visible = false;
        self.shown_at = None;

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
                hide_overlay(self.gpui_hwnd);
            }
        }
        cx.notify();
    }

    fn switch_to_desktop_only(&mut self, desktop_id: DesktopId, cx: &mut Context<Self>) {
        self.hide(cx);
        if let Err(e) = self.vdm.switch_to_desktop(desktop_id) {
            append_log(&format!("DESKTOP SWITCH ERROR {}: {e:#}", desktop_id));
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.visible {
            return;
        }
        let key = event.keystroke.key.as_str();
        let shift = event.keystroke.modifiers.shift;
        let cols = self.current_cols();
        let sizes = self.group_sizes();

        match key {
            "escape" => {
                self.hide(cx);
            }
            "enter" | "space" => {
                if let Some(&hwnd) = self.flat_hwnds.get(self.selected_flat) {
                    self.activate_window(hwnd, cx);
                }
            }
            "right" => {
                self.selected_flat = navigate(
                    &sizes,
                    cols,
                    self.selected_flat,
                    NavDirection::Right,
                    self.config.cross_desktop_nav,
                );
                self.ensure_selected_visible();
                cx.notify();
            }
            "left" => {
                self.selected_flat = navigate(
                    &sizes,
                    cols,
                    self.selected_flat,
                    NavDirection::Left,
                    self.config.cross_desktop_nav,
                );
                self.ensure_selected_visible();
                cx.notify();
            }
            "down" => {
                self.selected_flat = navigate(
                    &sizes,
                    cols,
                    self.selected_flat,
                    NavDirection::Down,
                    self.config.cross_desktop_nav,
                );
                self.ensure_selected_visible();
                cx.notify();
            }
            "up" => {
                self.selected_flat = navigate(
                    &sizes,
                    cols,
                    self.selected_flat,
                    NavDirection::Up,
                    self.config.cross_desktop_nav,
                );
                self.ensure_selected_visible();
                cx.notify();
            }
            "tab" => {
                let dir = if shift {
                    NavDirection::Left
                } else {
                    NavDirection::Right
                };
                self.selected_flat = navigate(&sizes, cols, self.selected_flat, dir, true);
                self.ensure_selected_visible();
                cx.notify();
            }
            "home" => {
                self.selected_flat =
                    navigate(&sizes, cols, self.selected_flat, NavDirection::Home, true);
                self.ensure_selected_visible();
                cx.notify();
            }
            "end" => {
                self.selected_flat =
                    navigate(&sizes, cols, self.selected_flat, NavDirection::End, true);
                self.ensure_selected_visible();
                cx.notify();
            }
            "f5" | "r" => {
                self.refresh_snapshot();
                self.thumbs.borrow_mut().sync_targets(&self.flat_hwnds);
                self.clamp_scroll(cols);
                self.status_notice = Some(SharedString::from("已刷新窗口列表"));
                window.refresh();
                cx.notify();
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
        let delta_y = match event.delta {
            ScrollDelta::Lines(pt) => -pt.y * 56.0,
            ScrollDelta::Pixels(pt) => -f32::from(pt.y),
        };
        let cols = self.current_cols();
        self.scroll_y += delta_y;
        self.clamp_scroll(cols);
        cx.notify();
    }

    fn selected_card(&self) -> Option<&WindowCard> {
        self.groups
            .iter()
            .flat_map(|g| g.cards.iter())
            .find(|c| c.flat_index == self.selected_flat)
    }

    /// Runs automated 50-cycle hide/show + enumerate + DWM register/unregister benchmark.
    pub fn run_self_benchmark(&mut self, window: &mut Window, cx: &mut Context<Self>) -> String {
        use win_platform::capture_process_resources;

        // Warmup 10 cycles so lazy Windows caches stabilize
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

        // Hidden idle 2s check
        let r_idle0 = capture_process_resources();
        let t_idle = Instant::now();
        std::thread::sleep(Duration::from_millis(2000));
        let r_idle1 = capture_process_resources();
        let wall_ms = t_idle.elapsed().as_secs_f64() * 1000.0;
        let cpu_ms = r_idle1.cpu_time_ms - r_idle0.cpu_time_ms;

        format!(
            "BENCH 50-cycles: p50={:.2}ms p95={:.2}ms max={:.2}ms | GDI {}->{} (d={:+}) USER {}->{} (d={:+}) Handles {}->{} (d={:+}) | Hidden 2s CPU={:.2}ms ({:.2}% of 1 core)",
            p50,
            p95,
            max,
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
        let viewport_sz = window.viewport_size();
        let logical_w = f32::from(viewport_sz.width);
        if logical_w > 100.0 {
            self.viewport_width_px.set((logical_w - 48.0).max(320.0));
        }
        let cols = self.current_cols();
        let (total_content_h, _) = self.compute_layout_metrics(cols);
        let view_h = self.viewport_height_px.get().max(100.0);
        let max_scroll = (total_content_h - view_h).max(0.0);
        if self.scroll_y > max_scroll {
            self.scroll_y = max_scroll;
        }
        let scroll_y = self.scroll_y;

        let selected_summary = self
            .selected_card()
            .map(|c| {
                format!(
                    "{} · {} · {}",
                    c.desktop_label,
                    if c.info.process_name.is_empty() {
                        "应用"
                    } else {
                        c.info.process_name.as_str()
                    },
                    c.info.title
                )
            })
            .unwrap_or_else(|| "当前没有可切换的窗口".to_string());

        let viewport_clip_rc = self.viewport_clip.clone();
        let viewport_h_cell = self.viewport_height_px.clone();
        let viewport_w_cell = self.viewport_width_px.clone();
        let thumbs_rc = self.thumbs.clone();
        let is_visible = self.visible;
        let selected_flat = self.selected_flat;

        // Build all desktop group sections inside one unified scrollable list
        let mut group_elements = Vec::with_capacity(self.groups.len());
        for (g_idx, group) in self.groups.iter().enumerate() {
            let desktop_id = group.desktop_id;
            let count = group.cards.len();
            let dot_color = if group.is_pinned_group {
                rgb(0xA855F7)
            } else if group.is_current {
                rgb(0x3B82F6)
            } else {
                rgb(0x64748B)
            };

            let header = div()
                .id(("desktop-header", g_idx))
                .h(px(GROUP_HEADER_H))
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_1()
                .when_some(desktop_id, |el, did| {
                    el.cursor_pointer()
                        .hover(|s| s.opacity(0.85))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.switch_to_desktop_only(did, cx);
                        }))
                })
                .child(div().size(px(8.0)).rounded_full().bg(dot_color))
                .child(
                    div()
                        .text_sm()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(if group.is_current {
                            rgb(0xF8FAFC)
                        } else {
                            rgb(0xCBD5E1)
                        })
                        .child(group.title.clone()),
                )
                .when(group.is_current, |el| {
                    el.child(
                        div()
                            .px_2()
                            .py(px(1.0))
                            .rounded(px(4.0))
                            .bg(rgba(0x3B82F633))
                            .border_1()
                            .border_color(rgba(0x3B82F680))
                            .text_xs()
                            .text_color(rgb(0x93C5FD))
                            .child("当前桌面"),
                    )
                })
                .child(
                    div()
                        .px(px(6.0))
                        .py(px(1.0))
                        .rounded(px(4.0))
                        .bg(rgba(0xFFFFFF12))
                        .text_xs()
                        .text_color(rgb(0x94A3B8))
                        .child(format!("{count}")),
                )
                .child(div().flex_1().h(px(1.0)).ml_2().bg(rgba(0xFFFFFF14)));

            let mut section = div()
                .flex()
                .flex_col()
                .when(g_idx > 0, |el| el.mt(px(GROUP_TOP_GAP)))
                .child(header);

            if group.cards.is_empty() {
                section = section.child(
                    div()
                        .id(("empty-desktop", g_idx))
                        .h(px(EMPTY_GROUP_H))
                        .flex()
                        .items_center()
                        .px_3()
                        .rounded_md()
                        .bg(rgba(0xFFFFFF08))
                        .border_1()
                        .border_color(rgba(0xFFFFFF10))
                        .text_xs()
                        .text_color(rgb(0x64748B))
                        .when_some(desktop_id, |el, did| {
                            el.cursor_pointer()
                                .hover(|s| s.bg(rgba(0xFFFFFF12)).text_color(rgb(0xCBD5E1)))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.switch_to_desktop_only(did, cx);
                                }))
                        })
                        .child("该桌面当前无窗口（点击可直接切换到此桌面）"),
                );
            } else {
                let mut rows_col = div().flex().flex_col().gap(px(ROW_GAP));

                for row_chunk in group.cards.chunks(cols) {
                    let mut row_el = div().flex().flex_row().gap(px(14.0)).w_full();

                    for card in row_chunk {
                        let card_flat_index = card.flat_index;
                        let is_selected = card_flat_index == selected_flat;
                        let target_hwnd = card.info.hwnd;
                        let has_live_surface = thumbs_rc.borrow().has_live_surface(target_hwnd);

                        let thumbs_for_card = thumbs_rc.clone();
                        let clip_for_card = viewport_clip_rc.clone();

                        let icon_el = if let Some(ref render_img) = card.icon {
                            div()
                                .size(px(18.0))
                                .flex_shrink_0()
                                .child(img(render_img.clone()).size(px(18.0)).rounded(px(3.0)))
                        } else {
                            div()
                                .size(px(18.0))
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

                        let card_el = div()
                            .id(("win-card", card.flat_index))
                            .flex_1()
                            .min_w_0()
                            .h(px(CARD_TOTAL_H))
                            .flex()
                            .flex_col()
                            .rounded(px(10.0))
                            .overflow_hidden()
                            .cursor_pointer()
                            .bg(if is_selected {
                                rgb(0x1D2B44)
                            } else {
                                rgba(0x161B26E8)
                            })
                            .border(px(if is_selected { 2.0 } else { 1.0 }))
                            .border_color(if is_selected {
                                rgb(0x3B82F6)
                            } else {
                                rgba(0xFFFFFF18)
                            })
                            .when(is_selected, |el| {
                                el.shadow(vec![BoxShadow {
                                    color: hsla(0.60, 0.90, 0.56, 0.45),
                                    blur_radius: px(14.0),
                                    spread_radius: px(1.0),
                                    offset: point(px(0.0), px(0.0)),
                                }])
                            })
                            .hover(|s| {
                                if is_selected {
                                    s.border_color(rgb(0x60A5FA))
                                } else {
                                    s.bg(rgba(0x1E2636F2)).border_color(rgba(0xFFFFFF38))
                                }
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.selected_flat = card_flat_index;
                                this.activate_window(target_hwnd, cx);
                            }))
                            // Dedicated header ABOVE thumbnail (never overlapped by DWM thumbnail)
                            .child(
                                div()
                                    .h(px(CARD_HEADER_H))
                                    .px(px(10.0))
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_2()
                                    .bg(if is_selected {
                                        rgba(0x2563EB30)
                                    } else {
                                        rgba(0xFFFFFF06)
                                    })
                                    .border_b_1()
                                    .border_color(rgba(0xFFFFFF10))
                                    .child(icon_el)
                                    .child(
                                        div()
                                            .flex_1()
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
                                            .text_color(if is_selected {
                                                rgb(0xFFFFFF)
                                            } else {
                                                rgb(0xE2E8F0)
                                            })
                                            .child(card.info.title.clone()),
                                    )
                                    .when(card.info.is_minimized, |el| {
                                        el.child(
                                            div()
                                                .flex_shrink_0()
                                                .px(px(5.0))
                                                .py(px(1.0))
                                                .rounded(px(3.0))
                                                .bg(rgba(0xF59E0B28))
                                                .text_xs()
                                                .text_color(rgb(0xFCD34D))
                                                .child("最小化"),
                                        )
                                    })
                                    .when(card.info.is_elevated == Some(true), |el| {
                                        el.child(
                                            div()
                                                .flex_shrink_0()
                                                .px(px(5.0))
                                                .py(px(1.0))
                                                .rounded(px(3.0))
                                                .bg(rgba(0xEF444428))
                                                .text_xs()
                                                .text_color(rgb(0xFCA5A5))
                                                .child("管理员"),
                                        )
                                    }),
                            )
                            // Dedicated dark recessed preview well for DWM live thumbnail
                            .child(
                                div()
                                    .relative()
                                    .h(px(CARD_THUMB_H))
                                    .w_full()
                                    .bg(rgb(0x0B0F17))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .when(!has_live_surface, |el| {
                                        el.child(
                                            div()
                                                .flex()
                                                .flex_col()
                                                .items_center()
                                                .gap_1()
                                                .text_xs()
                                                .text_color(rgb(0x64748B))
                                                .child(if card.info.process_name.is_empty() {
                                                    "暂无预览".to_string()
                                                } else {
                                                    card.info.process_name.clone()
                                                }),
                                        )
                                    })
                                    .child(
                                        canvas(
                                            |bounds, _, _| bounds,
                                            move |bounds, _, window, _| {
                                                if !is_visible {
                                                    return;
                                                }
                                                let sf = window.scale_factor();
                                                let clip = clip_for_card
                                                    .borrow()
                                                    .unwrap_or(bounds);
                                                thumbs_for_card.borrow_mut().update_slot(
                                                    target_hwnd,
                                                    bounds,
                                                    clip,
                                                    sf,
                                                );
                                            },
                                        )
                                        .absolute()
                                        .inset_0()
                                        .size_full(),
                                    ),
                            );

                        row_el = row_el.child(card_el);
                    }

                    // Fill trailing empty grid slots so partial rows keep identical card width!
                    for _ in row_chunk.len()..cols {
                        row_el = row_el.child(div().flex_1().min_w_0());
                    }

                    rows_col = rows_col.child(row_el);
                }

                section = section.child(rows_col);
            }

            group_elements.push(section);
        }

        // Scrollbar geometry when content overflows viewport
        let show_scrollbar = max_scroll > 1.0;
        let thumb_ratio = (view_h / total_content_h.max(1.0)).clamp(0.12, 1.0);
        let thumb_h = (view_h * thumb_ratio).max(32.0);
        let thumb_top = if max_scroll > 0.0 {
            (scroll_y / max_scroll) * (view_h - thumb_h).max(0.0)
        } else {
            0.0
        };

        let root_bg = match self.config.background {
            gpui::WindowBackgroundAppearance::Opaque => rgb(0x11151E).into(),
            gpui::WindowBackgroundAppearance::Transparent => rgba(0x0E121AE0),
            gpui::WindowBackgroundAppearance::Blurred => rgba(0x0E121BD4),
        };

        div()
            .id("windeck-root")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            .size_full()
            .flex()
            .flex_col()
            .bg(root_bg)
            .border_1()
            .border_color(rgba(0xFFFFFF22))
            .text_color(rgb(0xF1F5F9))
            // === TOP HEADER BAR ===
            .child(
                div()
                    .h(px(50.0))
                    .flex_shrink_0()
                    .px_5()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(rgba(0xFFFFFF14))
                    .bg(rgba(0x0B0F1788))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .text_base()
                                    .font_weight(gpui::FontWeight::BOLD)
                                    .text_color(rgb(0xF8FAFC))
                                    .child("WinDeck"),
                            )
                            .child(
                                div()
                                    .px_2()
                                    .py(px(2.0))
                                    .rounded(px(5.0))
                                    .bg(rgba(0x3B82F626))
                                    .border_1()
                                    .border_color(rgba(0x3B82F655))
                                    .text_xs()
                                    .text_color(rgb(0x93C5FD))
                                    .child(format!(
                                        "{} 个桌面 · {} 个窗口",
                                        self.desktops.len(),
                                        self.total_window_count
                                    )),
                            )
                            .when_some(self.hotkey_warning.clone(), |el, warn| {
                                el.child(
                                    div()
                                        .px_2()
                                        .py(px(2.0))
                                        .rounded(px(4.0))
                                        .bg(rgba(0xEF444433))
                                        .text_xs()
                                        .text_color(rgb(0xFCA5A5))
                                        .child(warn),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_3()
                            .text_xs()
                            .text_color(rgb(0x94A3B8))
                            .child(format!("热键 {}", self.config.hotkey))
                            .child("·")
                            .child("↑↓←→ / Tab 选择")
                            .child("·")
                            .child("Enter / 点击 跳转")
                            .child("·")
                            .child("Esc 收起"),
                    ),
            )
            // === SCROLLABLE UNIFIED VIEWPORT (ALL DESKTOPS + ALL APPS) ===
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_hidden()
                    // Capture exact viewport clip bounds in prepaint BEFORE card canvases paint
                    .child(
                        canvas(
                            move |bounds, _, _| {
                                *viewport_clip_rc.borrow_mut() = Some(bounds);
                                let w = f32::from(bounds.size.width);
                                let h = f32::from(bounds.size.height);
                                if w > 100.0 {
                                    viewport_w_cell.set(w - 40.0);
                                }
                                if h > 100.0 {
                                    viewport_h_cell.set(h);
                                }
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .inset_0()
                        .size_full(),
                    )
                    .child(
                        div()
                            .relative()
                            .top(px(-scroll_y))
                            .w_full()
                            .px_5()
                            .pt(px(CONTENT_PAD_TOP))
                            .pb(px(CONTENT_PAD_BOTTOM))
                            .flex()
                            .flex_col()
                            .children(group_elements),
                    )
                    .when(show_scrollbar, |el| {
                        el.child(
                            div()
                                .absolute()
                                .top(px(thumb_top + 4.0))
                                .right(px(5.0))
                                .w(px(5.0))
                                .h(px((thumb_h - 8.0).max(20.0)))
                                .rounded_full()
                                .bg(rgba(0xFFFFFF44)),
                        )
                    }),
            )
            // === BOTTOM STATUS BAR ===
            .child(
                div()
                    .h(px(32.0))
                    .flex_shrink_0()
                    .px_5()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .border_t_1()
                    .border_color(rgba(0xFFFFFF12))
                    .bg(rgba(0x0B0F1799))
                    .text_xs()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_color(rgb(0xCBD5E1))
                            .child(
                                self.status_notice
                                    .clone()
                                    .unwrap_or_else(|| SharedString::from(selected_summary)),
                            ),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .ml_4()
                            .text_color(rgb(0x64748B))
                            .child(format!(
                                "呼出 {:.1}ms · 枚举 {:.1}ms · 滚轮翻页 · F5 刷新",
                                self.last_open_ms, self.last_enum_ms
                            )),
                    ),
            )
    }
}
