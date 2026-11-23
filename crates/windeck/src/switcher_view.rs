use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    App, Bounds, ClickEvent, Context, FocusHandle, Focusable, KeyDownEvent, MouseButton, Pixels,
    RenderImage, ScrollDelta, ScrollWheelEvent, SharedString, Window, WindowBackgroundAppearance,
    canvas, div, img, prelude::*, px, rgb, rgba,
};
use smallvec::smallvec;
use win_platform::{
    DesktopId, IconBgra, ThumbnailHostWindow, VirtualDesktopClient, VirtualDesktopInfo, WindowInfo,
    configure_borderless_immersive, enumerate_monitors, enumerate_windows, extract_window_icon,
    hide_layered_overlay, query_window_and_client_sizes, show_and_focus_layered_overlay,
    switch_to_window,
};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetShellWindow, IsWindow, SetForegroundWindow,
};

use crate::config::{AppConfig, LabelPosition, PreviewFit, append_log};
use crate::nav::{NavDirection, navigate};
use crate::thumb_manager::ThumbnailManager;

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
    focus_handle: FocusHandle,
    gpui_hwnd: HWND,
    thumb_host: ThumbnailHostWindow,
    vdm: Arc<VirtualDesktopClient>,
    config: AppConfig,
    visible: bool,
    shown_at: Option<Instant>,
    prev_foreground_hwnd: isize,
    desktops: Vec<VirtualDesktopInfo>,
    all_windows: Vec<WindowInfo>,
    desktop_filter: Option<DesktopId>,
    groups: Vec<DesktopGroup>,
    flat_hwnds: Vec<isize>,
    selected_flat: usize,
    hovered_flat: Option<usize>,
    armed_delete_desktop: Option<DesktopId>,
    scroll_y: f32,
    viewport_clip: Rc<RefCell<Option<Bounds<Pixels>>>>,
    viewport_width_px: Rc<Cell<f32>>,
    viewport_height_px: Rc<Cell<f32>>,
    icon_cache: HashMap<String, Option<Arc<RenderImage>>>,
    thumbs: Rc<RefCell<ThumbnailManager>>,
    last_open_ms: f64,
    last_enum_ms: f64,
    total_window_count: usize,
    hotkey_warning: Option<SharedString>,
    transient_toast: Option<(SharedString, Instant)>,
}

impl Focusable for SwitcherView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl SwitcherView {
    pub fn new(
        gpui_hwnd: HWND,
        thumb_host: ThumbnailHostWindow,
        vdm: Arc<VirtualDesktopClient>,
        config: AppConfig,
        hotkey_warning: Option<String>,
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
            config,
            visible: false,
            shown_at: None,
            prev_foreground_hwnd: 0,
            desktops: Vec::new(),
            all_windows: Vec::new(),
            desktop_filter: None,
            groups: Vec::new(),
            flat_hwnds: Vec::new(),
            selected_flat: 0,
            hovered_flat: None,
            armed_delete_desktop: None,
            scroll_y: 0.0,
            viewport_clip: Rc::new(RefCell::new(None)),
            viewport_width_px: Rc::new(Cell::new(2560.0)),
            viewport_height_px: Rc::new(Cell::new(1380.0)),
            icon_cache: HashMap::new(),
            thumbs: Rc::new(RefCell::new(thumbs)),
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

    pub fn handle_window_activation_changed(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.visible || !self.config.hide_on_blur {
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

    pub fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.visible {
            self.hide(cx);
        } else {
            self.show_and_refresh(window, cx);
        }
    }

    pub fn hide(&mut self, cx: &mut Context<Self>) {
        self.thumbs.borrow_mut().unregister_all();
        hide_layered_overlay(self.gpui_hwnd, self.thumb_host.hwnd());
        self.visible = false;
        self.shown_at = None;
        self.hovered_flat = None;
        self.armed_delete_desktop = None;
        self.transient_toast = None;
        cx.notify();
    }

    pub fn show_and_refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let t_start = Instant::now();
        self.prev_foreground_hwnd = unsafe { GetForegroundWindow() }.0 as isize;
        self.hovered_flat = None;
        self.armed_delete_desktop = None;
        self.scroll_y = 0.0;
        // Always open in full panorama ("全部") so all desktops' windows are visible at once
        self.desktop_filter = None;

        self.refresh_snapshot();

        self.thumbs.borrow_mut().sync_targets(&self.flat_hwnds);
        self.visible = true;
        self.shown_at = Some(Instant::now());
        window.set_background_appearance(WindowBackgroundAppearance::Transparent);
        self.thumb_host.configure(
            self.config.immersive,
            self.config.background,
            self.config.blur_opacity,
        );

        if let Ok((phys_rc, scale)) = show_and_focus_layered_overlay(
            self.gpui_hwnd,
            self.thumb_host.hwnd(),
            self.config.monitor,
            self.config.immersive,
        ) {
            let logical_w = ((phys_rc.right - phys_rc.left) as f32 / scale).max(640.0);
            let logical_h = ((phys_rc.bottom - phys_rc.top) as f32 / scale).max(420.0);
            self.viewport_width_px.set(logical_w);
            self.viewport_height_px
                .set((logical_h - TOP_SPACES_BAR_H).max(260.0));
        }
        window.focus(&self.focus_handle);
        self.ensure_selected_visible();
        window.refresh();

        self.last_open_ms = t_start.elapsed().as_secs_f64() * 1000.0;
        append_log(&format!(
            "OPEN total={:.2}ms enum={:.2}ms desktops={} windows={} thumbs={} preview={}x{} fit={} label={}",
            self.last_open_ms,
            self.last_enum_ms,
            self.desktops.len(),
            self.total_window_count,
            self.thumbs.borrow().active_count(),
            self.config.preview_width as i32,
            self.config.preview_height as i32,
            self.config.preview_fit.as_config_str(),
            self.config.label_position.as_config_str(),
        ));
        cx.notify();
    }

    pub fn refresh_snapshot(&mut self) {
        let t_enum = Instant::now();
        let desktops = self.vdm.get_desktops().unwrap_or_default();
        let monitors = enumerate_monitors();
        let exclude = [self.gpui_hwnd.0 as isize, self.thumb_host.hwnd().0 as isize];
        let (windows, _) =
            enumerate_windows(&self.vdm, &desktops, &monitors, &exclude, false).unwrap_or_default();
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
        self.all_windows = windows;
        self.rebuild_visible_groups(true);
    }

    /// Rebuilds visible desktop groups & flat HWND index list according to `self.desktop_filter`.
    fn rebuild_visible_groups(&mut self, pick_smart_initial: bool) {
        let desktops = self.desktops.clone();
        let windows = self.all_windows.clone();
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

    fn set_desktop_filter(
        &mut self,
        new_filter: Option<DesktopId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.desktop_filter == new_filter {
            return;
        }
        self.desktop_filter = new_filter;
        self.scroll_y = 0.0;
        self.hovered_flat = None;
        self.selected_flat = 0;
        self.rebuild_visible_groups(false);
        // Immediately synchronize DWM thumbnails so filtered-out desktops release their surfaces (zero ghosts)
        self.thumbs.borrow_mut().sync_targets(&self.flat_hwnds);
        window.refresh();
        cx.notify();
    }

    fn create_new_desktop(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
                }
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
        if let Some(cur_group) = self
            .groups
            .iter()
            .find(|g| g.is_current && !g.cards.is_empty())
        {
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

    fn clamp_scroll(&mut self) {
        let (cols, _, preview_h, _, _) = self.fixed_grid_geometry();
        let (total_h, _) = self.compute_layout_metrics(cols, preview_h);
        let view_h = self.viewport_height_px.get().max(100.0);
        let max_scroll = (total_h - view_h).max(0.0);
        self.scroll_y = self.scroll_y.clamp(0.0, max_scroll);
    }

    fn ensure_selected_visible(&mut self) {
        let (cols, _, preview_h, _, _) = self.fixed_grid_geometry();
        let (total_h, selected_range) = self.compute_layout_metrics(cols, preview_h);
        let view_h = self.viewport_height_px.get().max(100.0);
        let max_scroll = (total_h - view_h).max(0.0);

        if let Some((top, bottom)) = selected_range {
            let margin = 14.0;
            if top - margin < self.scroll_y {
                self.scroll_y = (top - margin).max(0.0);
            } else if bottom + margin > self.scroll_y + view_h {
                self.scroll_y = (bottom + margin - view_h).min(max_scroll);
            }
        }
        self.scroll_y = self.scroll_y.clamp(0.0, max_scroll);
    }

    fn activate_window(&mut self, target_hwnd: isize, cx: &mut Context<Self>) {
        let raw = HWND(target_hwnd as *mut std::ffi::c_void);
        if !unsafe { IsWindow(Some(raw)) }.as_bool() {
            self.refresh_snapshot();
            self.thumbs.borrow_mut().sync_targets(&self.flat_hwnds);
            cx.notify();
            return;
        }

        self.thumbs.borrow_mut().unregister_all();
        hide_layered_overlay(self.gpui_hwnd, self.thumb_host.hwnd());
        self.visible = false;
        self.shown_at = None;
        self.hovered_flat = None;

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
    }

    fn switch_to_desktop_only(&mut self, desktop_id: DesktopId, cx: &mut Context<Self>) {
        // Locate frontmost non-minimized window on destination desktop to prevent UWP late-activation pull-back
        let dest_front_hwnd = self
            .all_windows
            .iter()
            .find(|w| w.desktop_id == Some(desktop_id) && !w.is_minimized)
            .map(|w| HWND(w.hwnd as *mut std::ffi::c_void));

        self.thumbs.borrow_mut().unregister_all();
        self.visible = false;
        self.shown_at = None;
        self.hovered_flat = None;

        if let Err(e) = self.vdm.switch_to_desktop(desktop_id) {
            append_log(&format!("DESKTOP SWITCH ERROR {}: {e:#}", desktop_id));
            hide_layered_overlay(self.gpui_hwnd, self.thumb_host.hwnd());
            cx.notify();
            return;
        }

        // Hand off foreground to the destination desktop (or Shell desktop if empty) while we still own foreground
        unsafe {
            let target = dest_front_hwnd
                .filter(|&h| IsWindow(Some(h)).as_bool())
                .unwrap_or_else(|| GetShellWindow());
            if !target.is_invalid() {
                let _ = SetForegroundWindow(target);
            }
        }
        hide_layered_overlay(self.gpui_hwnd, self.thumb_host.hwnd());
        append_log(&format!("DESKTOP_SWITCH target={}", desktop_id));
        cx.notify();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.visible {
            return;
        }
        let key = event.keystroke.key.as_str();
        let shift = event.keystroke.modifiers.shift;
        let (cols, _, _, _, _) = self.fixed_grid_geometry();
        let sizes = self.group_sizes();

        if key == "escape" {
            if self.dismiss_armed_delete(cx) {
                return;
            }
            self.hide(cx);
            return;
        }
        if self.armed_delete_desktop.take().is_some() {
            cx.notify();
        }

        match key {
            "enter" | "space" => {
                if let Some(&hwnd) = self.flat_hwnds.get(self.selected_flat) {
                    self.activate_window(hwnd, cx);
                } else if let Some(did) = self.desktop_filter {
                    self.switch_to_desktop_only(did, cx);
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
            // Press L to live-cycle label placement: bottom_right -> bottom_left -> center -> below
            "l" => {
                self.config.label_position = self.config.label_position.next();
                self.transient_toast = Some((
                    SharedString::from(format!(
                        "标签位置：{}（config.toml: label_position = \"{}\"）",
                        self.config.label_position.display_label(),
                        self.config.label_position.as_config_str()
                    )),
                    Instant::now(),
                ));
                self.ensure_selected_visible();
                window.refresh();
                cx.notify();
            }
            // Press F to live-toggle fixed box fit mode: cover <-> contain
            "f" => {
                self.config.preview_fit = self.config.preview_fit.toggle();
                self.transient_toast = Some((
                    SharedString::from(format!(
                        "预览适配：{}（config.toml: preview_fit = \"{}\"）",
                        self.config.preview_fit.display_label(),
                        self.config.preview_fit.as_config_str()
                    )),
                    Instant::now(),
                ));
                window.refresh();
                cx.notify();
            }
            // Press - / = to live-tune Gaussian blur transparency
            "-" | "_" => {
                self.config.blur_opacity = (self.config.blur_opacity - 0.05).clamp(0.05, 0.85);
                self.thumb_host.configure(
                    self.config.immersive,
                    self.config.background,
                    self.config.blur_opacity,
                );
                self.transient_toast = Some((
                    SharedString::from(format!(
                        "毛玻璃暗度：{:.2}（越低越通透，config.toml: blur_opacity = {:.2}）",
                        self.config.blur_opacity, self.config.blur_opacity
                    )),
                    Instant::now(),
                ));
                cx.notify();
            }
            "=" | "+" => {
                self.config.blur_opacity = (self.config.blur_opacity + 0.05).clamp(0.05, 0.85);
                self.thumb_host.configure(
                    self.config.immersive,
                    self.config.background,
                    self.config.blur_opacity,
                );
                self.transient_toast = Some((
                    SharedString::from(format!(
                        "毛玻璃暗度：{:.2}（越低越通透，config.toml: blur_opacity = {:.2}）",
                        self.config.blur_opacity, self.config.blur_opacity
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
        let delta_y = match event.delta {
            ScrollDelta::Lines(pt) => -pt.y * 64.0,
            ScrollDelta::Pixels(pt) => -f32::from(pt.y),
        };
        self.scroll_y += delta_y;
        self.clamp_scroll();
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
        let viewport_sz = window.viewport_size();
        let logical_w = f32::from(viewport_sz.width);
        if logical_w > 100.0 {
            self.viewport_width_px.set(logical_w);
        }

        let (cols, preview_w, preview_h, tile_outer_w, grid_total_w) = self.fixed_grid_geometry();
        let item_total_h = preview_h + self.extra_item_h();
        let label_pos = self.config.label_position;
        let preview_fit = self.config.preview_fit;

        let (total_content_h, _) = self.compute_layout_metrics(cols, preview_h);
        let view_h = self.viewport_height_px.get().max(100.0);
        let max_scroll = (total_content_h - view_h).max(0.0);
        if self.scroll_y > max_scroll {
            self.scroll_y = max_scroll;
        }
        let scroll_y = self.scroll_y;

        let optical_top_pad = if max_scroll <= 1.0 && total_content_h < view_h {
            ((view_h - total_content_h) * 0.32).min(76.0)
        } else {
            0.0
        };

        let viewport_clip_rc = self.viewport_clip.clone();
        let viewport_h_cell = self.viewport_height_px.clone();
        let viewport_w_cell = self.viewport_width_px.clone();
        let thumbs_rc = self.thumbs.clone();
        let is_visible = self.visible;
        let selected_flat = self.selected_flat;
        let hovered_flat = self.hovered_flat;

        // === TOP FLOATING APPLE SPACES BAR: [ 全部 ] [ 桌面 1 ] [ 桌面 2 ] ... [ + ] ===
        let mut space_pills = Vec::with_capacity(self.desktops.len() + 3);
        let all_selected = self.desktop_filter.is_none();

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
            let is_cur = d_info.is_current;
            let is_filtered = self.desktop_filter == Some(did);
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
                        this.switch_to_desktop_only(did, cx);
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
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            if this.dismiss_armed_delete(cx) {
                                return;
                            }
                            this.switch_to_desktop_only(did, cx);
                        }))
                })
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
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    if this.dismiss_armed_delete(cx) {
                                        return;
                                    }
                                    this.switch_to_desktop_only(did, cx);
                                }))
                        })
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

                        let thumbs_for_card = thumbs_rc.clone();
                        let clip_for_card = viewport_clip_rc.clone();

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
                        let (outer_glow, mid_glow, inner_ring) = if is_selected {
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
                                        let sf = window.scale_factor();
                                        let clip = clip_for_card.borrow().unwrap_or(bounds);
                                        thumbs_for_card.borrow_mut().update_slot(
                                            target_hwnd,
                                            bounds,
                                            clip,
                                            sf,
                                            preview_fit,
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
                            .w(px(tile_outer_w))
                            .flex_shrink_0()
                            .h(px(item_total_h))
                            .flex()
                            .flex_col()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                                if *hovered {
                                    this.hovered_flat = Some(card_flat_index);
                                } else if this.hovered_flat == Some(card_flat_index) {
                                    this.hovered_flat = None;
                                }
                                cx.notify();
                            }))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                if this.dismiss_armed_delete(cx) {
                                    return;
                                }
                                this.selected_flat = card_flat_index;
                                this.activate_window(target_hwnd, cx);
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

        let active_toast = self.transient_toast.as_ref().and_then(|(msg, at)| {
            (at.elapsed() < Duration::from_secs(3)).then(|| msg.clone())
        });

        div()
            .id("windeck-mission-control")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _, _, cx| {
                    this.dismiss_armed_delete(cx);
                }),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                if this.dismiss_armed_delete(cx) {
                    return;
                }
                this.hide(cx);
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(rgba(0x00000001))
            .text_color(rgb(0xFFFFFF))
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
                                *viewport_clip_rc.borrow_mut() = Some(bounds);
                                let w = f32::from(bounds.size.width);
                                let h = f32::from(bounds.size.height);
                                if w > 100.0 {
                                    viewport_w_cell.set(w);
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
                    .when(show_scrollbar, |el| {
                        el.child(
                            div()
                                .absolute()
                                .top(px(thumb_top + 6.0))
                                .right(px(5.0))
                                .w(px(4.0))
                                .h(px((thumb_h - 12.0).max(22.0)))
                                .rounded_full()
                                .bg(rgba(0xFFFFFF38)),
                        )
                    }),
            )
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
