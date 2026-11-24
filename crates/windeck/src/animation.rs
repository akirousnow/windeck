use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui::{Bounds, Pixels, RenderImage, SharedString, point, px, size};
use win_platform::{DesktopId, MonitorInfo, WindowInfo};
use windows::Win32::Foundation::RECT;

use crate::config::{PreviewFit, TransitionEffect};

pub fn ease_out_cubic(t: f32) -> f32 {
    let x = t.clamp(0.0, 1.0);
    1.0 - (1.0 - x).powi(3)
}

pub fn ease_out_quint(t: f32) -> f32 {
    let x = t.clamp(0.0, 1.0);
    1.0 - (1.0 - x).powi(5)
}

/// Subtle spring-like ease-out with restrained ~2.8% overshoot for `TransitionEffect::Spring`.
pub fn ease_out_back_subtle(t: f32) -> f32 {
    let x = t.clamp(0.0, 1.0);
    let c1 = 0.85_f32;
    let c3 = c1 + 1.0;
    1.0 + c3 * (x - 1.0).powi(3) + c1 * (x - 1.0).powi(2)
}

pub fn lerp_f32(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

pub fn lerp_rect(from: RECT, to: RECT, t: f32) -> RECT {
    RECT {
        left: lerp_f32(from.left as f32, to.left as f32, t).round() as i32,
        top: lerp_f32(from.top as f32, to.top as f32, t).round() as i32,
        right: lerp_f32(from.right as f32, to.right as f32, t).round() as i32,
        bottom: lerp_f32(from.bottom as f32, to.bottom as f32, t).round() as i32,
    }
}

pub fn lerp_bounds(from: Bounds<Pixels>, to: Bounds<Pixels>, t: f32) -> Bounds<Pixels> {
    Bounds {
        origin: point(
            px(lerp_f32(f32::from(from.origin.x), f32::from(to.origin.x), t)),
            px(lerp_f32(f32::from(from.origin.y), f32::from(to.origin.y), t)),
        ),
        size: size(
            px(lerp_f32(f32::from(from.size.width), f32::from(to.size.width), t).max(1.0)),
            px(lerp_f32(f32::from(from.size.height), f32::from(to.size.height), t).max(1.0)),
        ),
    }
}

pub fn bounds_to_phys_rect(bounds: Bounds<Pixels>, scale: f32) -> RECT {
    RECT {
        left: (f32::from(bounds.origin.x) * scale).round() as i32,
        top: (f32::from(bounds.origin.y) * scale).round() as i32,
        right: (f32::from(bounds.origin.x + bounds.size.width) * scale).round() as i32,
        bottom: (f32::from(bounds.origin.y + bounds.size.height) * scale).round() as i32,
    }
}

pub fn scale_rect_around_center(rc: RECT, factor: f32) -> RECT {
    let cx = (rc.left + rc.right) as f32 * 0.5;
    let cy = (rc.top + rc.bottom) as f32 * 0.5;
    let half_w = ((rc.right - rc.left) as f32 * 0.5 * factor).max(1.0);
    let half_h = ((rc.bottom - rc.top) as f32 * 0.5 * factor).max(1.0);
    RECT {
        left: (cx - half_w).round() as i32,
        top: (cy - half_h).round() as i32,
        right: (cx + half_w).round() as i32,
        bottom: (cy + half_h).round() as i32,
    }
}

/// Frame-rate-independent exponential smoothing for smooth scroll tracking.
pub fn exp_damp(current: f32, target: f32, speed: f32, dt_secs: f32) -> f32 {
    if (target - current).abs() < 0.45 {
        return target;
    }
    let dt = dt_secs.clamp(0.0, 0.05);
    let alpha = 1.0 - (-speed * dt).exp();
    let next = current + (target - current) * alpha;
    if (target - next).abs() < 0.45 {
        target
    } else {
        next
    }
}

#[derive(Debug, Clone, Copy)]
pub struct TransitionProfile {
    pub tab_duration: Duration,
    pub tab_slide_px: f32,
    pub selection_duration: Duration,
    pub scroll_damp_speed: f32,
    pub exit_zoom_duration: Duration,
    pub exit_desktop_duration: Duration,
    pub cross_desktop_pan_duration: Duration,
    pub cross_desktop_pop_duration: Duration,
    pub use_zoom_to_window: bool,
    pub use_spring: bool,
}

impl TransitionProfile {
    pub fn from_effect(effect: TransitionEffect) -> Self {
        match effect {
            TransitionEffect::Off => Self {
                tab_duration: Duration::ZERO,
                tab_slide_px: 0.0,
                selection_duration: Duration::ZERO,
                scroll_damp_speed: 0.0,
                exit_zoom_duration: Duration::ZERO,
                exit_desktop_duration: Duration::ZERO,
                cross_desktop_pan_duration: Duration::ZERO,
                cross_desktop_pop_duration: Duration::ZERO,
                use_zoom_to_window: false,
                use_spring: false,
            },
            TransitionEffect::Subtle => Self {
                tab_duration: Duration::from_millis(120),
                tab_slide_px: 10.0,
                selection_duration: Duration::from_millis(110),
                scroll_damp_speed: 24.0,
                exit_zoom_duration: Duration::from_millis(110),
                exit_desktop_duration: Duration::from_millis(110),
                cross_desktop_pan_duration: Duration::from_millis(115),
                cross_desktop_pop_duration: Duration::from_millis(100),
                use_zoom_to_window: false,
                use_spring: false,
            },
            TransitionEffect::Smooth => Self {
                tab_duration: Duration::from_millis(180),
                tab_slide_px: 28.0,
                selection_duration: Duration::from_millis(145),
                scroll_damp_speed: 18.0,
                exit_zoom_duration: Duration::from_millis(175),
                exit_desktop_duration: Duration::from_millis(155),
                cross_desktop_pan_duration: Duration::from_millis(175),
                cross_desktop_pop_duration: Duration::from_millis(165),
                use_zoom_to_window: true,
                use_spring: false,
            },
            TransitionEffect::Spring => Self {
                tab_duration: Duration::from_millis(220),
                tab_slide_px: 42.0,
                selection_duration: Duration::from_millis(185),
                scroll_damp_speed: 14.0,
                exit_zoom_duration: Duration::from_millis(210),
                exit_desktop_duration: Duration::from_millis(185),
                cross_desktop_pan_duration: Duration::from_millis(200),
                cross_desktop_pop_duration: Duration::from_millis(205),
                use_zoom_to_window: true,
                use_spring: true,
            },
        }
    }

    pub fn ease_position(use_spring: bool, t: f32) -> f32 {
        if use_spring {
            ease_out_back_subtle(t)
        } else {
            ease_out_quint(t)
        }
    }
}

/// Snapshot of a card leaving the grid during a Scene A desktop-tab transition.
#[derive(Clone)]
pub struct LeavingCardSnapshot {
    pub hwnd: isize,
    pub thumb_bounds: Bounds<Pixels>,
    pub has_live_surface: bool,
    pub icon: Option<Arc<RenderImage>>,
    pub fallback_label: SharedString,
}

/// Scene A: Top Spaces bar desktop-filter transition (FLIP for shared cards + crossfade/slide for leaving & entering).
pub struct TabTransition {
    pub started_at: Instant,
    pub duration: Duration,
    /// `+1.0` when navigating right (higher tab index), `-1.0` when navigating left.
    pub slide_dir: f32,
    pub slide_px: f32,
    pub use_spring: bool,
    pub shared_from_bounds: HashMap<isize, Bounds<Pixels>>,
    pub leaving_cards: Vec<LeavingCardSnapshot>,
    pub entering_hwnds: HashSet<isize>,
}

impl TabTransition {
    pub fn raw_progress(&self, now: Instant) -> f32 {
        let dur = self.duration.as_secs_f32();
        if dur <= 0.0001 {
            return 1.0;
        }
        (now.saturating_duration_since(self.started_at).as_secs_f32() / dur).clamp(0.0, 1.0)
    }

    pub fn is_finished(&self, now: Instant) -> bool {
        self.raw_progress(now) >= 1.0
    }

    pub fn eased_pos(&self, t: f32) -> f32 {
        TransitionProfile::ease_position(self.use_spring, t)
    }

    /// Leaving cards clear briskly over the first 72% so gliding shared cards remain clean and unobstructed.
    pub fn leaving_opacity(&self, t: f32) -> f32 {
        (1.0 - ease_out_cubic((t / 0.72).clamp(0.0, 1.0))).clamp(0.0, 1.0)
    }

    pub fn entering_opacity(&self, t: f32) -> f32 {
        ease_out_cubic(((t - 0.06) / 0.94).clamp(0.0, 1.0))
    }
}

/// Scene B: Smooth floating focus-ring transition in scroll-independent content coordinates.
#[derive(Clone, Copy)]
pub struct SelectionTransition {
    pub started_at: Instant,
    pub duration: Duration,
    pub use_spring: bool,
    pub from_content_rect: Bounds<Pixels>,
    pub to_flat: usize,
}

impl SelectionTransition {
    pub fn raw_progress(&self, now: Instant) -> f32 {
        let dur = self.duration.as_secs_f32();
        if dur <= 0.0001 {
            return 1.0;
        }
        (now.saturating_duration_since(self.started_at).as_secs_f32() / dur).clamp(0.0, 1.0)
    }

    pub fn sample(
        &self,
        to_content_rect: Bounds<Pixels>,
        now: Instant,
    ) -> (Bounds<Pixels>, bool) {
        let t = self.raw_progress(now);
        if t >= 1.0 {
            return (to_content_rect, true);
        }
        let eased = TransitionProfile::ease_position(self.use_spring, t);
        (lerp_bounds(self.from_content_rect, to_content_rect, eased), false)
    }
}

/// Scene C: Exit transition when activating a target window or switching directly to a virtual desktop.
pub enum ExitTransitionKind {
    ZoomWindow {
        target_hwnd: isize,
        from_slot_phys: RECT,
        to_dest_phys: RECT,
        preview_fit: PreviewFit,
        target_desktop_id: Option<DesktopId>,
        is_cross_desktop: bool,
    },
    /// Two-phase cross-desktop window transition:
    /// - Phase 1 (`0..pan_duration`): Pan toward target desktop (`desktop_dir = -1.0` left / `+1.0` right)
    ///   while outgoing grid sweeps out in the opposite direction and the target window sweeps in from the
    ///   target desktop's side into a poised launch rect.
    /// - Phase 2 (`pan_duration..total`): Pop/zoom the target window from the poised rect to its real
    ///   destination bounds (or gentle pop-fade for minimized/other-monitor windows).
    CrossDesktopWindow {
        target_hwnd: isize,
        target_desktop_id: DesktopId,
        target_desktop_label: SharedString,
        /// `-1.0` when target desktop is to the LEFT of current desktop, `+1.0` when to the RIGHT.
        desktop_dir: f32,
        pan_duration: Duration,
        pop_duration: Duration,
        to_dest_phys: Option<RECT>,
        has_live_surface: bool,
        fallback_icon: Option<Arc<RenderImage>>,
        fallback_title: SharedString,
    },
    GentleFadeWindow {
        target_hwnd: isize,
        from_slot_phys: Option<RECT>,
        preview_fit: PreviewFit,
        target_desktop_id: Option<DesktopId>,
        is_cross_desktop: bool,
    },
    DesktopSwitch {
        target_desktop_id: DesktopId,
        target_desktop_label: SharedString,
        dest_front_hwnd: Option<isize>,
        slide_dir: f32,
    },
}

/// Sampled per-frame visual state for `ExitTransitionKind::CrossDesktopWindow`.
#[derive(Debug, Clone, Copy)]
pub struct CrossDesktopSample {
    /// Phase 1 progress `0.0..=1.0` (desktop directional pan).
    pub pan_t: f32,
    /// Phase 2 progress `0.0..=1.0` (target window pop-out; `0.0` during Phase 1).
    pub pop_t: f32,
    /// Horizontal offset (logical px) applied to the outgoing overview grid during Phase 1.
    pub outgoing_grid_dx: f32,
    /// Opacity (`0.0..=1.0`) for outgoing overview cards & chrome during Phase 1.
    pub outgoing_opacity: f32,
    /// Opacity (`0.0..=1.0`) for the directional desktop indicator badge (`‹ 桌面 1` / `桌面 3 ›`).
    pub badge_opacity: f32,
    /// Horizontal offset (logical px) for the directional desktop indicator badge.
    pub badge_dx: f32,
    /// Target window physical rect on `thumb_host_hwnd` (when `to_dest_phys` is known).
    pub target_dest_phys: Option<RECT>,
    /// Target window opacity (`0.0..=1.0`) for live DWM thumbnail or fallback card.
    pub target_opacity: f32,
    /// Target fallback card horizontal offset (logical px) when `!has_live_surface` / `to_dest_phys.is_none()`.
    pub fallback_dx: f32,
    /// Target fallback card scale (`0.78..=1.04`) when `!has_live_surface` / `to_dest_phys.is_none()`.
    pub fallback_scale: f32,
}

pub fn shift_rect_x(rc: RECT, dx: i32) -> RECT {
    RECT {
        left: rc.left + dx,
        top: rc.top + 0,
        right: rc.right + dx,
        bottom: rc.bottom + 0,
    }
}

/// Computes all per-frame values for the two-phase cross-desktop window transition.
#[allow(clippy::too_many_arguments)]
pub fn sample_cross_desktop_window(
    elapsed: Duration,
    pan_duration: Duration,
    pop_duration: Duration,
    desktop_dir: f32,
    use_spring: bool,
    viewport_w_logical: f32,
    host_size_phys: (i32, i32),
    to_dest_phys: Option<RECT>,
) -> CrossDesktopSample {
    let pan_secs = pan_duration.as_secs_f32().max(0.001);
    let pop_secs = pop_duration.as_secs_f32().max(0.001);
    let elapsed_secs = elapsed.as_secs_f32();

    let pan_t = (elapsed_secs / pan_secs).clamp(0.0, 1.0);
    let pop_t = if elapsed_secs <= pan_secs {
        0.0
    } else {
        ((elapsed_secs - pan_secs) / pop_secs).clamp(0.0, 1.0)
    };

    // Phase 1: Outgoing overview sweeps in opposite direction of target desktop:
    // target desktop on LEFT (desktop_dir = -1) -> old desktop sweeps RIGHT (+dx), new desktop enters from LEFT (-dx -> 0).
    let pan_eased = ease_out_cubic(pan_t);
    let pan_dist_logical = (viewport_w_logical * 0.28).clamp(220.0, 520.0);
    let outgoing_grid_dx = -desktop_dir * pan_dist_logical * pan_eased;
    // Outgoing cards clear cleanly by 62% of Phase 1 so DWM thumbnails never clash with incoming target.
    let outgoing_opacity = (1.0 - ease_out_cubic((pan_t / 0.62).clamp(0.0, 1.0))).clamp(0.0, 1.0);

    // Directional desktop pill ("‹ 桌面 1" / "桌面 3 ›"): glides in during Phase 1, fades cleanly in early Phase 2.
    let badge_in = ease_out_cubic((pan_t / 0.45).clamp(0.0, 1.0));
    let badge_out = if pop_t > 0.0 {
        (1.0 - ease_out_cubic((pop_t / 0.42).clamp(0.0, 1.0))).clamp(0.0, 1.0)
    } else {
        1.0
    };
    let badge_opacity = (badge_in * badge_out).clamp(0.0, 1.0);
    let badge_dx = desktop_dir * 26.0 * (1.0 - badge_in) - desktop_dir * 12.0 * ease_out_cubic(pop_t);

    // Target window arrival (latter 72% of Phase 1) -> pop-out zoom (Phase 2).
    let entry_t = ((pan_t - 0.25) / 0.75).clamp(0.0, 1.0);
    let entry_eased = ease_out_quint(entry_t);
    let entry_opacity = ease_out_cubic((entry_t / 0.65).clamp(0.0, 1.0));

    let pop_eased = if use_spring {
        ease_out_back_subtle(pop_t)
    } else {
        ease_out_quint(pop_t)
    };

    let host_w = host_size_phys.0.max(1);
    let entry_offset_phys = ((host_w as f32) * 0.26).clamp(180.0, 640.0);

    let (target_dest_phys, target_opacity, fallback_dx, fallback_scale) =
        if let Some(dest) = to_dest_phys {
            // Poised stage rect: 78% of real target window centered on target window's center.
            let poised = scale_rect_around_center(dest, 0.78);
            let rect = if pop_t <= 0.0 {
                // Phase 1: sweep in from target desktop's side while scaling gently 0.70 -> 0.78
                let stage_scale = lerp_f32(0.70, 0.78, entry_eased);
                let scaled = scale_rect_around_center(dest, stage_scale);
                let dx_phys = (desktop_dir * entry_offset_phys * (1.0 - entry_eased)).round() as i32;
                shift_rect_x(scaled, dx_phys)
            } else {
                // Phase 2: crisp pop-out from 78% poised rect to 100% real window bounds!
                lerp_rect(poised, dest, pop_eased)
            };
            let op = if pop_t > 0.0 { 1.0 } else { entry_opacity };
            (Some(rect), op, 0.0, 1.0)
        } else {
            // Degraded cross-desktop path (minimized / other-monitor / no live surface):
            // Phase 1 sweeps a clean centered preview in from target desktop side; Phase 2 pops slightly and fades out.
            let fb_dx = desktop_dir * (pan_dist_logical * 0.65) * (1.0 - entry_eased);
            let (fb_scale, op) = if pop_t <= 0.0 {
                (lerp_f32(0.82, 0.94, entry_eased), entry_opacity)
            } else {
                let fade = (1.0 - ease_out_cubic(pop_t)).clamp(0.0, 1.0);
                (lerp_f32(0.94, 1.05, ease_out_cubic(pop_t)), fade)
            };
            (None, op, fb_dx, fb_scale)
        };

    CrossDesktopSample {
        pan_t,
        pop_t,
        outgoing_grid_dx,
        outgoing_opacity,
        badge_opacity,
        badge_dx,
        target_dest_phys,
        target_opacity,
        fallback_dx,
        fallback_scale,
    }
}

pub struct ExitTransition {
    pub started_at: Instant,
    pub duration: Duration,
    pub use_spring: bool,
    pub finishing: bool,
    pub desktop_ready: Arc<AtomicBool>,
    pub desktop_switch_error: Arc<Mutex<Option<String>>>,
    pub kind: ExitTransitionKind,
}

impl ExitTransition {
    /// Maximum extra grace time after visual completion while waiting on a slow Explorer desktop switch.
    const MAX_DESKTOP_GRACE_MS: u64 = 240;

    pub fn raw_progress(&self, now: Instant) -> f32 {
        let dur = self.duration.as_secs_f32();
        if dur <= 0.0001 {
            return 1.0;
        }
        (now.saturating_duration_since(self.started_at).as_secs_f32() / dur).clamp(0.0, 1.0)
    }

    pub fn is_ready_to_finish(&self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.started_at);
        if elapsed < self.duration {
            return false;
        }
        if self.desktop_ready.load(Ordering::Acquire) {
            return true;
        }
        elapsed >= self.duration + Duration::from_millis(Self::MAX_DESKTOP_GRACE_MS)
    }

    /// Chrome (top Spaces bar, group titles, borders, labels, settings trigger) fades quickly in first 40%
    /// so Layer 2 GPUI chrome never lags behind the fast-moving Layer 1 DWM thumbnail.
    pub fn chrome_opacity(&self, t: f32) -> f32 {
        (1.0 - ease_out_cubic((t / 0.40).clamp(0.0, 1.0))).clamp(0.0, 1.0)
    }

    /// Non-target thumbnails clear quickly in the first 30% so DWM registration Z-order never overlaps the zooming target.
    pub fn other_thumbs_opacity(&self, t: f32) -> u8 {
        let a = (1.0 - ease_out_cubic((t / 0.30).clamp(0.0, 1.0))).clamp(0.0, 1.0);
        (a * 255.0).round() as u8
    }

    pub fn eased_zoom_pos(&self, t: f32) -> f32 {
        if self.use_spring {
            ease_out_back_subtle(t)
        } else {
            ease_out_quint(t)
        }
    }

    pub fn eased_uncrop(&self, t: f32) -> f32 {
        ease_out_cubic(t)
    }
}

/// Resolves the target window's physical destination rect relative to `thumb_host_hwnd`'s client origin.
/// Returns `None` when a fly-to-window zoom would be invalid or visually jarring:
/// - minimized window (iconic `(-32000, -32000)` + Windows' own `SW_RESTORE` animation)
/// - degenerate zero/tiny rect
/// - target window sits predominantly on a different monitor than the overlay host (`< 50%` overlap)
pub fn resolve_exit_target_phys_rect(
    win: &WindowInfo,
    overlay_phys_rect: RECT,
    monitors: &[MonitorInfo],
) -> Option<RECT> {
    if win.is_minimized {
        return None;
    }

    let mut target = if win.visible_rect.right > win.visible_rect.left
        && win.visible_rect.bottom > win.visible_rect.top
    {
        win.visible_rect
    } else {
        win.rect
    };

    if win.is_maximized {
        if let Some(mon) = win.monitor_index.and_then(|idx| monitors.get(idx)) {
            target = RECT {
                left: target.left.max(mon.work_area.left),
                top: target.top.max(mon.work_area.top),
                right: target.right.min(mon.work_area.right),
                bottom: target.bottom.min(mon.work_area.bottom),
            };
        }
    }

    let w = target.right - target.left;
    let h = target.bottom - target.top;
    if w < 32 || h < 32 {
        return None;
    }

    let ov_w = overlay_phys_rect.right - overlay_phys_rect.left;
    let ov_h = overlay_phys_rect.bottom - overlay_phys_rect.top;
    if ov_w <= 0 || ov_h <= 0 {
        return None;
    }

    let inter_l = target.left.max(overlay_phys_rect.left);
    let inter_t = target.top.max(overlay_phys_rect.top);
    let inter_r = target.right.min(overlay_phys_rect.right);
    let inter_b = target.bottom.min(overlay_phys_rect.bottom);
    let inter_w = (inter_r - inter_l).max(0) as i64;
    let inter_h = (inter_b - inter_t).max(0) as i64;
    let target_area = (w as i64) * (h as i64);
    if inter_w * inter_h * 2 < target_area {
        return None;
    }

    Some(RECT {
        left: target.left - overlay_phys_rect.left,
        top: target.top - overlay_phys_rect.top,
        right: target.right - overlay_phys_rect.left,
        bottom: target.bottom - overlay_phys_rect.top,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_monitor(bounds: RECT, work_area: RECT) -> MonitorInfo {
        MonitorInfo {
            index: 0,
            handle: 1,
            device_name: "DISPLAY1".to_string(),
            is_primary: true,
            bounds,
            work_area,
            dpi_x: 96,
            dpi_y: 96,
            scale_factor: 1.0,
        }
    }

    fn test_window(rect: RECT, visible_rect: RECT, is_minimized: bool, is_maximized: bool) -> WindowInfo {
        WindowInfo {
            hwnd: 100,
            pid: 1,
            process_name: "app.exe".to_string(),
            exe_path: String::new(),
            class_name: "Wnd".to_string(),
            title: "Test".to_string(),
            desktop_id: None,
            desktop_index: Some(0),
            is_pinned: false,
            is_minimized,
            is_maximized,
            restore_to_maximized: false,
            is_elevated: Some(false),
            cloaked_flags: 0,
            monitor_index: Some(0),
            rect,
            visible_rect,
            normal_rect: rect,
        }
    }

    #[test]
    fn easing_curves_hit_exact_endpoints_and_subtle_overshoot() {
        assert!((ease_out_cubic(0.0) - 0.0).abs() < 1e-5);
        assert!((ease_out_cubic(1.0) - 1.0).abs() < 1e-5);
        assert!((ease_out_quint(0.0) - 0.0).abs() < 1e-5);
        assert!((ease_out_quint(1.0) - 1.0).abs() < 1e-5);
        assert!((ease_out_back_subtle(0.0) - 0.0).abs() < 1e-5);
        assert!((ease_out_back_subtle(1.0) - 1.0).abs() < 1e-5);

        let peak = (0..=100)
            .map(|i| ease_out_back_subtle(i as f32 / 100.0))
            .fold(0.0_f32, f32::max);
        assert!(peak > 1.005 && peak < 1.06, "expected subtle overshoot, got {peak}");
    }

    #[test]
    fn exp_damp_converges_cleanly_at_60hz_and_120hz() {
        let mut y60 = 0.0_f32;
        for _ in 0..30 {
            y60 = exp_damp(y60, 400.0, 18.0, 1.0 / 60.0);
        }
        let mut y120 = 0.0_f32;
        for _ in 0..60 {
            y120 = exp_damp(y120, 400.0, 18.0, 1.0 / 120.0);
        }
        assert_eq!(y60, 400.0);
        assert_eq!(y120, 400.0);
    }

    #[test]
    fn resolve_exit_target_handles_same_monitor_maximized_minimized_and_other_monitor() {
        let screen = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let work = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        let mons = vec![test_monitor(screen, work)];

        // Normal window uses visible_rect (no invisible 7px shadow border)
        let raw = RECT {
            left: 93,
            top: 100,
            right: 907,
            bottom: 707,
        };
        let vis = RECT {
            left: 100,
            top: 100,
            right: 900,
            bottom: 700,
        };
        let normal = test_window(raw, vis, false, false);
        let resolved = resolve_exit_target_phys_rect(&normal, screen, &mons).unwrap();
        assert_eq!(resolved.left, 100);
        assert_eq!(resolved.right, 900);
        assert_eq!(resolved.bottom, 700);

        // Maximized window clamped to monitor work area
        let overhang = RECT {
            left: -8,
            top: -8,
            right: 1928,
            bottom: 1048,
        };
        let max_win = test_window(overhang, overhang, false, true);
        let max_resolved = resolve_exit_target_phys_rect(&max_win, screen, &mons).unwrap();
        assert_eq!(max_resolved.left, 0);
        assert_eq!(max_resolved.top, 0);
        assert_eq!(max_resolved.right, 1920);
        assert_eq!(max_resolved.bottom, 1040);

        // Minimized window degrades to None
        let iconic = RECT {
            left: -32000,
            top: -32000,
            right: -31840,
            bottom: -31972,
        };
        let min_win = test_window(iconic, iconic, true, false);
        assert!(resolve_exit_target_phys_rect(&min_win, screen, &mons).is_none());

        // Window on secondary monitor degrades to None
        let second_mon_rc = RECT {
            left: 1920,
            top: 0,
            right: 3200,
            bottom: 900,
        };
        let other_win = test_window(second_mon_rc, second_mon_rc, false, false);
        assert!(resolve_exit_target_phys_rect(&other_win, screen, &mons).is_none());

        // Non-immersive offset host translates coordinates to host client-local origin
        let non_immersive_host = RECT {
            left: 100,
            top: 60,
            right: 1820,
            bottom: 980,
        };
        let translated =
            resolve_exit_target_phys_rect(&normal, non_immersive_host, &mons).unwrap();
        assert_eq!(translated.left, 0);
        assert_eq!(translated.top, 40);
        assert_eq!(translated.right, 800);
        assert_eq!(translated.bottom, 640);
    }

    #[test]
    fn cross_desktop_two_phase_pans_left_then_pops_to_exact_dest() {
        let dest = RECT {
            left: 160,
            top: 90,
            right: 1760,
            bottom: 990,
        };
        let pan = Duration::from_millis(175);
        let pop = Duration::from_millis(165);

        // Target desktop on LEFT (desktop_dir = -1.0):
        // Mid-Phase 1: outgoing grid sweeps RIGHT (+dx), target enters from LEFT (< dest.left)
        let mid_p1 = sample_cross_desktop_window(
            Duration::from_millis(85),
            pan,
            pop,
            -1.0,
            false,
            1920.0,
            (1920, 1080),
            Some(dest),
        );
        assert!(mid_p1.pan_t > 0.4 && mid_p1.pan_t < 0.6);
        assert_eq!(mid_p1.pop_t, 0.0);
        assert!(mid_p1.outgoing_grid_dx > 100.0, "old desktop should sweep right when panning left");
        let r_mid = mid_p1.target_dest_phys.unwrap();
        assert!(
            (r_mid.left + r_mid.right) < (dest.left + dest.right),
            "incoming target window should still be to the left of center during left pan"
        );

        // End of Phase 1: outgoing cleared, target poised at 78% centered on dest, pop_t = 0
        let end_p1 = sample_cross_desktop_window(
            pan,
            pan,
            pop,
            -1.0,
            false,
            1920.0,
            (1920, 1080),
            Some(dest),
        );
        assert_eq!(end_p1.pan_t, 1.0);
        assert_eq!(end_p1.pop_t, 0.0);
        assert_eq!(end_p1.outgoing_opacity, 0.0);
        assert_eq!(end_p1.target_opacity, 1.0);
        let r_poised = end_p1.target_dest_phys.unwrap();
        assert!(r_poised.right - r_poised.left < dest.right - dest.left);

        // End of Phase 2: lands pixel-for-pixel on exact dest rect
        let end_p2 = sample_cross_desktop_window(
            pan + pop,
            pan,
            pop,
            -1.0,
            false,
            1920.0,
            (1920, 1080),
            Some(dest),
        );
        assert_eq!(end_p2.pop_t, 1.0);
        let r_final = end_p2.target_dest_phys.unwrap();
        assert_eq!(r_final.left, dest.left);
        assert_eq!(r_final.top, dest.top);
        assert_eq!(r_final.right, dest.right);
        assert_eq!(r_final.bottom, dest.bottom);
    }
}
