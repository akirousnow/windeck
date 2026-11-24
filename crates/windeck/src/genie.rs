use std::f32::consts::PI;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{Bounds, Pixels, RenderImage, point, px, size};
use smallvec::SmallVec;
use win_platform::{CapturedBitmap, DesktopId};

use crate::animation::{ease_out_back_subtle, ease_out_cubic, lerp_f32};
use crate::virtual_spaces::SpaceId;

/// Default horizontal strip count for GPU-instanced pixel-level Genie funnel warping.
/// 64 strips balance silky smooth S-curve edges (~3-4px/strip) with tiny single-draw-call overhead.
pub const GENIE_STRIP_COUNT: usize = 64;

/// Maximum pixel width for captured card textures (keeps GPU atlas footprint tiny even on 4K HiDPI).
pub const GENIE_CAPTURE_MAX_W: u32 = 420;

/// Logical pixel rectangle helper for Genie geometry calculations.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RectF {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl RectF {
    pub fn from_bounds(b: Bounds<Pixels>) -> Self {
        let left = f32::from(b.origin.x);
        let top = f32::from(b.origin.y);
        let w = f32::from(b.size.width).max(1.0);
        let h = f32::from(b.size.height).max(1.0);
        Self {
            left,
            top,
            right: left + w,
            bottom: top + h,
        }
    }

    pub fn width(self) -> f32 {
        (self.right - self.left).max(0.0)
    }

    pub fn height(self) -> f32 {
        (self.bottom - self.top).max(0.0)
    }

    pub fn center_x(self) -> f32 {
        (self.left + self.right) * 0.5
    }

    pub fn center_y(self) -> f32 {
        (self.top + self.bottom) * 0.5
    }

    pub fn lerp(self, other: Self, t: f32) -> Self {
        Self {
            left: lerp_f32(self.left, other.left, t),
            top: lerp_f32(self.top, other.top, t),
            right: lerp_f32(self.right, other.right, t),
            bottom: lerp_f32(self.bottom, other.bottom, t),
        }
    }
}

/// Narrow funnel portal mouth (in GPUI logical window coordinates) where pixels suck in or emerge out.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FunnelMouth {
    pub left: f32,
    pub right: f32,
    pub y: f32,
}

impl FunnelMouth {
    pub fn from_top_pill(pill_bounds: Bounds<Pixels>) -> Self {
        let r = RectF::from_bounds(pill_bounds);
        let cx = r.center_x();
        let half_w = (r.width() * 0.28).clamp(14.0, 24.0);
        Self {
            left: cx - half_w,
            right: cx + half_w,
            y: r.bottom - 3.0,
        }
    }

    /// Computes a clean portal mouth on `preview_box`'s top or bottom rim that is guaranteed
    /// to sit outside `drag_rect`'s vertical span whenever `drag_rect.height() < preview_box.height()`,
    /// ensuring the Suck-In convergence point, portal glow, and Suck-Out emergence point match pixel-for-pixel.
    pub fn for_group_preview_box(drag_rect: RectF, preview_box: RectF) -> Self {
        let cx = preview_box.center_x();
        let half_w = (preview_box.width() * 0.065).clamp(14.0, 24.0);
        let y = if drag_rect.bottom <= preview_box.top + 4.0 {
            // Dragged card sits above preview box -> stream downward into top rim
            preview_box.top
        } else if drag_rect.top >= preview_box.bottom - 4.0 {
            // Dragged card sits below preview box -> stream upward into bottom rim
            preview_box.bottom
        } else if drag_rect.top >= preview_box.top {
            // Overlapping lower/middle of preview box -> suck upward into top rim, bloom downward
            preview_box.top
        } else {
            // Overlapping top edge of preview box -> drag_rect.bottom < preview_box.bottom -> bottom rim
            preview_box.bottom
        };
        Self {
            left: cx - half_w,
            right: cx + half_w,
            y,
        }
    }

    pub fn center_x(self) -> f32 {
        (self.left + self.right) * 0.5
    }

    pub fn width(self) -> f32 {
        (self.right - self.left).max(1.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenieDirection {
    /// Pulls a rectangular card into `mouth` (`t: 0 -> 1`).
    SuckIn,
    /// Blooms a card out of `mouth` into its exact rectangular destination (`t: 0 -> 1`).
    SuckOut,
}

/// A single horizontal strip quad sampled for the current Genie animation frame.
#[derive(Debug, Clone, Copy)]
pub struct GenieStripQuad {
    pub strip_index: usize,
    pub bounds: Bounds<Pixels>,
}

/// Sampled per-frame render payload for a Genie funnel animation.
#[derive(Debug, Clone)]
pub struct GenieFrameSample {
    pub strips: Vec<GenieStripQuad>,
    pub opacity: f32,
    /// Intensity (`0.0..=1.0`) for the luminous portal glow right at `mouth`.
    pub portal_glow: f32,
}

fn smoothstep(t: f32) -> f32 {
    let x = t.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

fn ease_in_pull(t: f32) -> f32 {
    let x = t.clamp(0.0, 1.0);
    0.30 * x * x + 0.70 * x * x * x
}

/// Samples disjoint, pixel-snapped horizontal strip quads forming a genuine macOS-style
/// cosine S-curve Genie funnel distortion.
///
/// - At `SuckIn(t = 0.0)` and `SuckOut(t = 1.0)`, the strips reconstruct `card` cleanly with
///   zero horizontal distortion (`pinch_t = 0`) and exact physical pixel coverage.
/// - During mid-motion, pixels stream through the fixed S-curve funnel envelope strip-by-strip
///   and clip cleanly at `mouth.y`.
pub fn sample_genie_strips(
    card: RectF,
    mouth: FunnelMouth,
    direction: GenieDirection,
    raw_t: f32,
    num_strips: usize,
    scale_factor: f32,
) -> GenieFrameSample {
    let sf = scale_factor.max(0.5);
    let t = raw_t.clamp(0.0, 1.0);
    let p = match direction {
        GenieDirection::SuckIn => t,
        GenieDirection::SuckOut => 1.0 - t,
    };

    // Portal glow peaks as the card enters or emerges from the narrow mouth.
    let portal_glow = match direction {
        GenieDirection::SuckIn => {
            let rise = smoothstep(((t - 0.15) / 0.55).clamp(0.0, 1.0));
            let fall = 1.0 - smoothstep(((t - 0.82) / 0.18).clamp(0.0, 1.0));
            (rise * fall).clamp(0.0, 1.0)
        }
        GenieDirection::SuckOut => {
            let rise = smoothstep((t / 0.18).clamp(0.0, 1.0));
            let fall = 1.0 - smoothstep(((t - 0.25) / 0.55).clamp(0.0, 1.0));
            (rise * fall).clamp(0.0, 1.0)
        }
    };

    // Soft dissolve only on the final sliver entering/leaving the mouth so strips never pop abruptly.
    let opacity = if p > 0.88 {
        (1.0 - (p - 0.88) / 0.12).clamp(0.0, 1.0)
    } else {
        1.0
    };

    if num_strips == 0 || opacity <= 0.001 || card.width() < 1.0 || card.height() < 1.0 {
        return GenieFrameSample {
            strips: Vec::new(),
            opacity: 0.0,
            portal_glow,
        };
    }

    let mouth_above = mouth.y <= card.center_y();
    let card_h = card.height().max(1.0);

    // Phase A: S-curve funnel walls pinch smoothly from rectangle into cosine hourglass.
    let pinch_t = smoothstep((p / 0.42).clamp(0.0, 1.0));
    // Phase B: Leading edge reaches out to connect to mouth; trailing edge accelerates into mouth.
    let lead_t = ease_out_cubic((p / 0.44).clamp(0.0, 1.0));
    let slide_t = ease_in_pull(((p - 0.14) / 0.86).clamp(0.0, 1.0));
    let apparent_h = lerp_f32(card_h, card_h * 0.56, slide_t);

    let (my, span, virt_top, virt_bottom) = if mouth_above {
        let my = mouth.y.min(card.top);
        let far_y = card.bottom.max(my + 4.0);
        let span = (far_y - my).max(1.0);
        let stretched_top = lerp_f32(card.top, my, lead_t);
        let cur_bottom = lerp_f32(card.bottom, my, slide_t);
        let sliding_top = cur_bottom - apparent_h;
        let virt_top = stretched_top.min(sliding_top);
        (my, span, virt_top, cur_bottom)
    } else {
        let my = mouth.y.max(card.bottom);
        let far_y = card.top.min(my - 4.0);
        let span = (my - far_y).max(1.0);
        let stretched_bottom = lerp_f32(card.bottom, my, lead_t);
        let cur_top = lerp_f32(card.top, my, slide_t);
        let sliding_bottom = cur_top + apparent_h;
        let virt_bottom = stretched_bottom.max(sliding_bottom);
        (my, span, cur_top, virt_bottom)
    };

    let vis_top = if mouth_above {
        virt_top.max(my)
    } else {
        virt_top
    };
    let vis_bottom = if mouth_above {
        virt_bottom
    } else {
        virt_bottom.min(my)
    };

    if vis_bottom - vis_top < 0.75 / sf {
        return GenieFrameSample {
            strips: Vec::new(),
            opacity,
            portal_glow,
        };
    }

    let mut strips = Vec::with_capacity(num_strips);
    let mut last_y1_phys: Option<i32> = None;

    for i in 0..num_strips {
        let v0 = i as f32 / num_strips as f32;
        let v1 = (i + 1) as f32 / num_strips as f32;
        let raw_y0 = lerp_f32(virt_top, virt_bottom, v0);
        let raw_y1 = lerp_f32(virt_top, virt_bottom, v1);

        if mouth_above {
            if raw_y1 <= my {
                continue;
            }
        } else if raw_y0 >= my {
            continue;
        }

        let y0_log = if mouth_above { raw_y0.max(my) } else { raw_y0 };
        let y1_log = if mouth_above { raw_y1 } else { raw_y1.min(my) };

        let y0_phys = last_y1_phys.unwrap_or_else(|| (y0_log * sf).round() as i32);
        let y1_phys = (y1_log * sf).round() as i32;
        if y1_phys <= y0_phys {
            continue;
        }
        last_y1_phys = Some(y1_phys);

        let y_mid_log = ((y0_phys + y1_phys) as f32 * 0.5) / sf;
        let u = if mouth_above {
            ((y_mid_log - my) / span).clamp(0.0, 1.0)
        } else {
            ((my - y_mid_log) / span).clamp(0.0, 1.0)
        };
        // Iconic macOS Genie smooth cosine S-curve (zero slope at mouth and far base).
        let s = 0.5 - 0.5 * (PI * u).cos();

        let target_left = lerp_f32(mouth.left, card.left, s);
        let target_right = lerp_f32(mouth.right, card.right, s);
        let left_x = lerp_f32(card.left, target_left, pinch_t);
        let right_x = lerp_f32(card.right, target_right, pinch_t).max(left_x + 1.0 / sf);

        // Snap Y strictly onto exact physical pixel row boundaries [y0_phys, y1_phys)
        // accounting for GPUI's `origin.floor()` + `size.ceil()` so adjacent strips have 0 gaps & 0 overlap.
        let origin_y = (y0_phys as f32 + 0.001) / sf;
        let height_log = (((y1_phys - y0_phys) as f32) - 0.002).max(0.01) / sf;

        strips.push(GenieStripQuad {
            strip_index: i,
            bounds: Bounds {
                origin: point(px(left_x), px(origin_y)),
                size: size(px((right_x - left_x).max(0.5)), px(height_log)),
            },
        });
    }

    GenieFrameSample {
        strips,
        opacity,
        portal_glow,
    }
}

/// Converts a top-down BGRA8 bitmap into a single-frame GPUI `RenderImage` (used for the floating drag ghost).
pub fn bitmap_to_full_render_image(bmp: &CapturedBitmap) -> Option<Arc<RenderImage>> {
    let img = image::RgbaImage::from_raw(bmp.width, bmp.height, bmp.bgra.clone())?;
    let mut frames: SmallVec<[image::Frame; 1]> = SmallVec::with_capacity(1);
    frames.push(image::Frame::new(img));
    Some(Arc::new(RenderImage::new(frames)))
}

/// Slices a top-down BGRA8 bitmap into `num_strips` horizontal frames packed into a single GPUI `RenderImage`.
/// Uploaded to DirectX once on frame 0; every subsequent Genie frame renders via zero-upload instanced quads.
pub fn bitmap_to_strip_render_image(
    bmp: &CapturedBitmap,
    desired_strips: usize,
) -> Option<(Arc<RenderImage>, usize)> {
    if bmp.width < 2 || bmp.height < 4 {
        return None;
    }
    let max_strips = (bmp.height as usize / 2).max(1);
    let count = desired_strips.clamp(8.min(max_strips), max_strips);
    let row_stride = (bmp.width as usize) * 4;
    let mut frames: SmallVec<[image::Frame; 1]> = SmallVec::with_capacity(count);

    for i in 0..count {
        let y0 = (i * bmp.height as usize) / count;
        let y1 = (((i + 1) * bmp.height as usize) / count).max(y0 + 1).min(bmp.height as usize);
        let strip_h = (y1 - y0) as u32;
        let byte_start = y0 * row_stride;
        let byte_end = y1 * row_stride;
        let slice = bmp.bgra.get(byte_start..byte_end)?;
        let img = image::RgbaImage::from_raw(bmp.width, strip_h, slice.to_vec())?;
        frames.push(image::Frame::new(img));
    }

    let actual = frames.len();
    Some((Arc::new(RenderImage::new(frames)), actual))
}

/// Pure-Rust fallback card bitmap when desktop screen capture is unavailable (e.g. offscreen or headless).
/// Produces a sleek frosted slate card tinted with `accent_rgb` so Genie distortion always has crisp pixels.
pub fn synthesize_fallback_card_bitmap(width: u32, height: u32, accent_rgb: u32) -> CapturedBitmap {
    let w = width.clamp(64, GENIE_CAPTURE_MAX_W);
    let h = height.clamp(40, 320);
    let mut bgra = vec![0u8; (w as usize) * (h as usize) * 4];

    let ar = ((accent_rgb >> 16) & 0xFF) as f32;
    let ag = ((accent_rgb >> 8) & 0xFF) as f32;
    let ab = (accent_rgb & 0xFF) as f32;

    for y in 0..h {
        let fy = y as f32 / (h.saturating_sub(1).max(1)) as f32;
        for x in 0..w {
            let fx = x as f32 / (w.saturating_sub(1).max(1)) as f32;
            let border = x < 2 || y < 2 || x + 2 >= w || y + 2 >= h;
            let (r, g, b) = if border {
                (
                    lerp_f32(72.0, ar, 0.35),
                    lerp_f32(84.0, ag, 0.35),
                    lerp_f32(108.0, ab, 0.35),
                )
            } else {
                let glow = (1.0 - ((fx - 0.5).hypot(fy - 0.5) * 1.45).clamp(0.0, 1.0)).powi(2);
                (
                    lerp_f32(22.0, ar, glow * 0.28),
                    lerp_f32(28.0, ag, glow * 0.28),
                    lerp_f32(40.0, ab, glow * 0.28),
                )
            };
            let idx = ((y * w + x) * 4) as usize;
            bgra[idx] = b.clamp(0.0, 255.0) as u8;
            bgra[idx + 1] = g.clamp(0.0, 255.0) as u8;
            bgra[idx + 2] = r.clamp(0.0, 255.0) as u8;
            bgra[idx + 3] = 255;
        }
    }

    CapturedBitmap {
        width: w,
        height: h,
        bgra,
    }
}

/// Active phase of a post-drop Genie transfer or cancel return animation.
pub enum GeniePhase {
    /// Stage 1: Pixel-level Genie funnel pulling the released card INTO `mouth`.
    SuckIn {
        from_rect: RectF,
        mouth: FunnelMouth,
        duration: Duration,
    },
    /// Stage 2: Pixel-level Genie funnel blooming OUT from `mouth` into `to_rect`.
    SuckOut {
        mouth: FunnelMouth,
        to_rect: RectF,
        duration: Duration,
    },
    /// Smooth spring-back when a drag is cancelled or dropped onto its own desktop.
    CancelReturn {
        from_rect: RectF,
        to_rect: RectF,
        duration: Duration,
    },
}

/// State machine driving the full post-drop Genie animation (Suck-In -> optional filter switch -> Suck-Out)
/// or smooth cancel return, while keeping GPU atlas textures tracked for deterministic cleanup.
pub struct GenieTransferAnim {
    pub hwnd: isize,
    pub target_desktop_id: Option<DesktopId>,
    /// When `true` and currently in single-desktop filter mode, SwitcherView switches `desktop_filter`
    /// to `target_desktop_id` right after Stage 1 (`SuckIn`) finishes and plays Stage 2 (`SuckOut`).
    pub auto_switch_filter: bool,
    /// Whether Stage 2 (`SuckOut`) should play once Stage 1 (`SuckIn`) completes.
    pub play_suck_out: bool,
    /// If `true`, Stage 2 emerges from the target top desktop pill; if `false`, emerges from the
    /// destination slot's preview-box edge portal.
    pub suck_out_from_pill: bool,
    pub phase: GeniePhase,
    pub phase_started_at: Instant,
    pub full_image: Arc<RenderImage>,
    pub strip_image: Option<Arc<RenderImage>>,
    pub num_strips: usize,
    pub pill_pulse_desktop: Option<DesktopId>,
    pub pill_pulse_space: Option<SpaceId>,
    pub is_virtual_space_transfer: bool,
    pub keep_source_slot_visible: bool,
}

impl GenieTransferAnim {
    pub fn phase_progress(&self, now: Instant) -> f32 {
        let dur = match self.phase {
            GeniePhase::SuckIn { duration, .. }
            | GeniePhase::SuckOut { duration, .. }
            | GeniePhase::CancelReturn { duration, .. } => duration.as_secs_f32(),
        };
        if dur <= 0.0001 {
            return 1.0;
        }
        (now.saturating_duration_since(self.phase_started_at).as_secs_f32() / dur).clamp(0.0, 1.0)
    }

    pub fn is_phase_finished(&self, now: Instant) -> bool {
        self.phase_progress(now) >= 1.0
    }

    pub fn sample_cancel_return_rect(&self, now: Instant) -> Option<RectF> {
        let GeniePhase::CancelReturn {
            from_rect, to_rect, ..
        } = self.phase
        else {
            return None;
        };
        let t = ease_out_back_subtle(self.phase_progress(now));
        Some(from_rect.lerp(to_rect, t))
    }

    /// Returns a `0.0..=1.0` pulse factor for highlighting the target top pill as it swallows/emits the tab.
    pub fn pill_pulse_strength(&self, desktop_id: DesktopId, now: Instant) -> f32 {
        if self.pill_pulse_desktop != Some(desktop_id) {
            return 0.0;
        }
        self.pulse_strength_inner(now)
    }

    pub fn space_pill_pulse_strength(&self, space_id: SpaceId, now: Instant) -> f32 {
        if self.pill_pulse_space != Some(space_id) {
            return 0.0;
        }
        self.pulse_strength_inner(now)
    }

    fn pulse_strength_inner(&self, now: Instant) -> f32 {
        let t = self.phase_progress(now);
        match self.phase {
            GeniePhase::SuckIn { .. } => {
                // Builds up as the card streams into the pill and peaks right as it finishes swallowing
                smoothstep(((t - 0.25) / 0.75).clamp(0.0, 1.0))
            }
            GeniePhase::SuckOut { .. } => {
                // Decays smoothly as the card emerges out of the pill
                1.0 - smoothstep((t / 0.65).clamp(0.0, 1.0))
            }
            GeniePhase::CancelReturn { .. } => 0.0,
        }
    }

    /// Extracts owned `Arc<RenderImage>` handles so the caller can drop them from GPUI's GPU sprite atlas.
    pub fn into_images(self) -> Vec<Arc<RenderImage>> {
        let mut out = Vec::with_capacity(2);
        out.push(self.full_image);
        if let Some(s) = self.strip_image {
            out.push(s);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_card() -> RectF {
        RectF {
            left: 200.0,
            top: 400.0,
            right: 580.0,
            bottom: 626.0,
        }
    }

    fn top_pill_mouth() -> FunnelMouth {
        FunnelMouth {
            left: 610.0,
            right: 646.0,
            y: 44.0,
        }
    }

    #[test]
    fn suck_in_starts_at_exact_card_rect_and_converges_into_mouth() {
        let card = test_card();
        let mouth = top_pill_mouth();

        // t = 0.0: exact un-warped card rectangle with zero horizontal distortion
        let s0 = sample_genie_strips(card, mouth, GenieDirection::SuckIn, 0.0, 64, 1.5);
        assert!(!s0.strips.is_empty());
        assert!((s0.opacity - 1.0).abs() < 1e-4);
        for strip in &s0.strips {
            let l = f32::from(strip.bounds.origin.x);
            let r = l + f32::from(strip.bounds.size.width);
            assert!((l - card.left).abs() < 0.2, "expected left={}, got {l}", card.left);
            assert!((r - card.right).abs() < 0.2, "expected right={}, got {r}", card.right);
        }

        // t = 0.50: genuine funnel narrowing — top strips near mouth are strictly narrower than bottom strips
        let s_mid = sample_genie_strips(card, mouth, GenieDirection::SuckIn, 0.50, 64, 1.5);
        assert!(s_mid.strips.len() >= 10);
        let top_w = f32::from(s_mid.strips.first().unwrap().bounds.size.width);
        let bot_w = f32::from(s_mid.strips.last().unwrap().bounds.size.width);
        assert!(
            top_w < bot_w * 0.65,
            "funnel top ({top_w}) should pinch much narrower than bottom ({bot_w})"
        );
        // Leading strips pull diagonally toward the mouth X
        let top_cx = f32::from(s_mid.strips.first().unwrap().bounds.origin.x) + top_w * 0.5;
        let bot_cx = f32::from(s_mid.strips.last().unwrap().bounds.origin.x) + bot_w * 0.5;
        assert!(
            (top_cx - mouth.center_x()).abs() < (bot_cx - mouth.center_x()).abs(),
            "top strip center ({top_cx}) should pull closer to mouth ({}) than bottom ({bot_cx})",
            mouth.center_x()
        );

        // t = 1.0: completely sucked into mouth
        let s1 = sample_genie_strips(card, mouth, GenieDirection::SuckIn, 1.0, 64, 1.5);
        assert!(s1.strips.is_empty());
    }

    #[test]
    fn suck_out_is_exact_reverse_and_lands_pixel_perfect_on_destination() {
        let dest = test_card();
        let mouth = top_pill_mouth();

        let s0 = sample_genie_strips(dest, mouth, GenieDirection::SuckOut, 0.0, 64, 1.25);
        assert!(s0.strips.is_empty());

        let s_mid = sample_genie_strips(dest, mouth, GenieDirection::SuckOut, 0.50, 64, 1.25);
        assert!(s_mid.strips.len() >= 10);
        let top_w = f32::from(s_mid.strips.first().unwrap().bounds.size.width);
        let bot_w = f32::from(s_mid.strips.last().unwrap().bounds.size.width);
        assert!(top_w < bot_w * 0.65);

        let s1 = sample_genie_strips(dest, mouth, GenieDirection::SuckOut, 1.0, 64, 1.25);
        assert!(!s1.strips.is_empty());
        assert!((s1.opacity - 1.0).abs() < 1e-4);
        let first_top = f32::from(s1.strips.first().unwrap().bounds.origin.y);
        let last_strip = s1.strips.last().unwrap().bounds;
        let last_bot = f32::from(last_strip.origin.y) + f32::from(last_strip.size.height);
        assert!((first_top - dest.top).abs() < 1.2);
        assert!((last_bot - dest.bottom).abs() < 1.2);
        for strip in &s1.strips {
            let l = f32::from(strip.bounds.origin.x);
            let r = l + f32::from(strip.bounds.size.width);
            assert!((l - dest.left).abs() < 0.2);
            assert!((r - dest.right).abs() < 0.2);
        }
    }

    #[test]
    fn downward_mouth_pinches_bottom_narrower_than_top_and_strips_never_overlap_or_gap() {
        let card = RectF {
            left: 120.0,
            top: 140.0,
            right: 500.0,
            bottom: 366.0,
        };
        let lower_mouth = FunnelMouth {
            left: 340.0,
            right: 380.0,
            y: 680.0,
        };

        let sf = 1.5_f32;
        let mid = sample_genie_strips(card, lower_mouth, GenieDirection::SuckIn, 0.45, 64, sf);
        assert!(mid.strips.len() >= 10);
        let top_w = f32::from(mid.strips.first().unwrap().bounds.size.width);
        let bot_w = f32::from(mid.strips.last().unwrap().bounds.size.width);
        assert!(
            bot_w < top_w * 0.65,
            "downward funnel bottom ({bot_w}) should pinch narrower than top ({top_w})"
        );

        // Verify zero physical pixel gap and zero overlap between every consecutive emitted strip
        for pair in mid.strips.windows(2) {
            let a_y0 = (f32::from(pair[0].bounds.origin.y) * sf).floor() as i32;
            let a_h = (f32::from(pair[0].bounds.size.height) * sf).ceil() as i32;
            let b_y0 = (f32::from(pair[1].bounds.origin.y) * sf).floor() as i32;
            assert_eq!(
                a_y0 + a_h,
                b_y0,
                "consecutive strips must tile physical scanlines seamlessly without gaps or overlaps"
            );
        }
    }

    #[test]
    fn fallback_bitmap_and_strip_slicing_cover_all_rows() {
        let bmp = synthesize_fallback_card_bitmap(380, 226, 0x0A84FF);
        assert_eq!(bmp.width, 380);
        assert_eq!(bmp.height, 226);
        assert!(bmp.bgra.chunks_exact(4).all(|px| px[3] == 255));

        let (strips, count) = bitmap_to_strip_render_image(&bmp, GENIE_STRIP_COUNT)
            .expect("strip slicing should succeed");
        assert_eq!(count, GENIE_STRIP_COUNT);
        assert_eq!(strips.frame_count(), GENIE_STRIP_COUNT);
        let total_h: i32 = (0..count).map(|i| strips.size(i).height.0).sum();
        assert_eq!(total_h, 226);
    }

    #[test]
    fn group_preview_box_mouth_sits_outside_drag_rect_across_all_four_positions() {
        let preview = RectF {
            left: 500.0,
            top: 380.0,
            right: 880.0,
            bottom: 606.0,
        };
        let drag_w = 380.0 * 0.78;
        let drag_h = 226.0 * 0.78;

        let cases = [
            // 1. Clearly above preview box -> top rim
            (
                RectF {
                    left: 520.0,
                    top: 100.0,
                    right: 520.0 + drag_w,
                    bottom: 100.0 + drag_h,
                },
                preview.top,
            ),
            // 2. Clearly below preview box -> bottom rim
            (
                RectF {
                    left: 520.0,
                    top: 660.0,
                    right: 520.0 + drag_w,
                    bottom: 660.0 + drag_h,
                },
                preview.bottom,
            ),
            // 3. Overlapping lower/middle of preview box -> top rim
            (
                RectF {
                    left: 520.0,
                    top: 405.0,
                    right: 520.0 + drag_w,
                    bottom: 405.0 + drag_h,
                },
                preview.top,
            ),
            // 4. Overlapping top edge of preview box -> bottom rim (strictly below drag_rect.bottom!)
            (
                RectF {
                    left: 520.0,
                    top: 340.0,
                    right: 520.0 + drag_w,
                    bottom: 340.0 + drag_h,
                },
                preview.bottom,
            ),
        ];

        for (drag_rect, expected_y) in cases {
            let mouth = FunnelMouth::for_group_preview_box(drag_rect, preview);
            assert_eq!(mouth.y, expected_y);
            assert!(
                mouth.y <= drag_rect.top + 4.0 || mouth.y >= drag_rect.bottom - 4.0,
                "portal y={} must not fall inside drag_rect [{}..{}]",
                mouth.y,
                drag_rect.top,
                drag_rect.bottom
            );

            // Verify both Suck-In and Suck-Out produce seamless strips across DPI scales
            for sf in [1.0_f32, 1.25, 1.5, 2.0] {
                let s_in = sample_genie_strips(drag_rect, mouth, GenieDirection::SuckIn, 0.48, 64, sf);
                assert!(!s_in.strips.is_empty());
                for pair in s_in.strips.windows(2) {
                    let a_y0 = (f32::from(pair[0].bounds.origin.y) * sf).floor() as i32;
                    let a_h = (f32::from(pair[0].bounds.size.height) * sf).ceil() as i32;
                    let b_y0 = (f32::from(pair[1].bounds.origin.y) * sf).floor() as i32;
                    assert_eq!(a_y0 + a_h, b_y0);
                }

                let s_out = sample_genie_strips(preview, mouth, GenieDirection::SuckOut, 0.52, 64, sf);
                assert!(!s_out.strips.is_empty());
                for pair in s_out.strips.windows(2) {
                    let a_y0 = (f32::from(pair[0].bounds.origin.y) * sf).floor() as i32;
                    let a_h = (f32::from(pair[0].bounds.size.height) * sf).ceil() as i32;
                    let b_y0 = (f32::from(pair[1].bounds.origin.y) * sf).floor() as i32;
                    assert_eq!(a_y0 + a_h, b_y0);
                }
            }
        }
    }
}
