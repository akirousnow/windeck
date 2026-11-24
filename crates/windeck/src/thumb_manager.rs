use std::collections::{HashMap, HashSet};
use std::ffi::c_void;

use gpui::{Bounds, Pixels};
use win_platform::DwmThumbnail;
use windows::Win32::Foundation::{HWND, RECT};

use crate::animation::lerp_rect;
use crate::config::PreviewFit;

struct ThumbEntry {
    thumb: DwmThumbnail,
    source_size: Option<(i32, i32)>,
}

/// Computes the unclipped `rcSource` crop for `PreviewFit::Cover` so the live thumbnail
/// fills 100% of the fixed destination box with zero stretching:
/// - Tall windows (like chat apps): top-anchored crop so the window header & top content remain visible.
/// - Ultra-wide windows: symmetric center crop horizontally.
fn compute_cover_source_rect(src_w: i32, src_h: i32, dst_w: i32, dst_h: i32) -> RECT {
    let aspect_src = src_w as f32 / src_h.max(1) as f32;
    let aspect_dst = dst_w as f32 / dst_h.max(1) as f32;

    if (aspect_src - aspect_dst).abs() <= 0.01 {
        return RECT {
            left: 0,
            top: 0,
            right: src_w,
            bottom: src_h,
        };
    }

    if aspect_src > aspect_dst {
        // Source is wider than target -> crop left & right symmetrically
        let keep_w = ((src_h as f32) * aspect_dst)
            .round()
            .clamp(1.0, src_w as f32) as i32;
        let pad_x = (src_w - keep_w) / 2;
        RECT {
            left: pad_x,
            top: 0,
            right: pad_x + keep_w,
            bottom: src_h,
        }
    } else {
        // Source is taller than target -> keep top area so app title/toolbar/top chats stay visible
        let keep_h = ((src_w as f32) / aspect_dst)
            .round()
            .clamp(1.0, src_h as f32) as i32;
        RECT {
            left: 0,
            top: 0,
            right: src_w,
            bottom: keep_h,
        }
    }
}

fn compute_contain_dest_rect(src_w: i32, src_h: i32, slot: RECT) -> RECT {
    let mut dst_left = slot.left;
    let mut dst_top = slot.top;
    let mut dst_right = slot.right;
    let mut dst_bottom = slot.bottom;
    let avail_w = (dst_right - dst_left).max(1) as f32;
    let avail_h = (dst_bottom - dst_top).max(1) as f32;
    let aspect_src = src_w as f32 / src_h.max(1) as f32;
    let aspect_box = avail_w / avail_h;

    if aspect_src > aspect_box {
        let fit_h = (avail_w / aspect_src).round() as i32;
        let pad = ((dst_bottom - dst_top) - fit_h) / 2;
        dst_top += pad;
        dst_bottom = dst_top + fit_h;
    } else {
        let fit_w = (avail_h * aspect_src).round() as i32;
        let pad = ((dst_right - dst_left) - fit_w) / 2;
        dst_left += pad;
        dst_right = dst_left + fit_w;
    }

    RECT {
        left: dst_left,
        top: dst_top,
        right: dst_right.max(dst_left + 1),
        bottom: dst_bottom.max(dst_top + 1),
    }
}

fn clip_dest_and_source(dst: RECT, base_src: RECT, clip: RECT) -> Option<(RECT, RECT)> {
    let mut dst_left = dst.left;
    let mut dst_top = dst.top;
    let mut dst_right = dst.right;
    let mut dst_bottom = dst.bottom;

    if dst_bottom <= clip.top
        || dst_top >= clip.bottom
        || dst_right <= clip.left
        || dst_left >= clip.right
    {
        return None;
    }

    let orig_dst_w = (dst_right - dst_left).max(1) as f32;
    let orig_dst_h = (dst_bottom - dst_top).max(1) as f32;
    let base_src_w = (base_src.right - base_src.left).max(1) as f32;
    let base_src_h = (base_src.bottom - base_src.top).max(1) as f32;
    let mut src_rect = base_src;

    if dst_top < clip.top {
        let ratio = (clip.top - dst_top) as f32 / orig_dst_h;
        src_rect.top = base_src.top
            + (ratio * base_src_h)
                .round()
                .clamp(0.0, base_src_h - 1.0) as i32;
        dst_top = clip.top;
    }
    if dst_bottom > clip.bottom {
        let ratio = (dst_bottom - clip.bottom) as f32 / orig_dst_h;
        src_rect.bottom = base_src.bottom
            - (ratio * base_src_h)
                .round()
                .clamp(0.0, base_src_h - 1.0) as i32;
        dst_bottom = clip.bottom;
    }
    if dst_left < clip.left {
        let ratio = (clip.left - dst_left) as f32 / orig_dst_w;
        src_rect.left = base_src.left
            + (ratio * base_src_w)
                .round()
                .clamp(0.0, base_src_w - 1.0) as i32;
        dst_left = clip.left;
    }
    if dst_right > clip.right {
        let ratio = (dst_right - clip.right) as f32 / orig_dst_w;
        src_rect.right = base_src.right
            - (ratio * base_src_w)
                .round()
                .clamp(0.0, base_src_w - 1.0) as i32;
        dst_right = clip.right;
    }

    if dst_right <= dst_left
        || dst_bottom <= dst_top
        || src_rect.right <= src_rect.left
        || src_rect.bottom <= src_rect.top
    {
        return None;
    }

    Some((
        RECT {
            left: dst_left,
            top: dst_top,
            right: dst_right,
            bottom: dst_bottom,
        },
        src_rect,
    ))
}

/// Computes clipped `(dest_rect, src_rect)` for a Scene C fly-to-window zoom frame,
/// smoothly uncropping `Cover` thumbnails to full source bounds as they expand to the real window.
#[allow(clippy::too_many_arguments)]
fn compute_zoom_rects(
    src_w: i32,
    src_h: i32,
    from_slot_phys: RECT,
    to_dest_phys: RECT,
    host_w: i32,
    host_h: i32,
    fit: PreviewFit,
    t_pos: f32,
    t_uncrop: f32,
) -> Option<(RECT, RECT)> {
    let full_src = RECT {
        left: 0,
        top: 0,
        right: src_w,
        bottom: src_h,
    };
    let (start_dst, start_src) = match fit {
        PreviewFit::Cover => (
            from_slot_phys,
            compute_cover_source_rect(
                src_w,
                src_h,
                (from_slot_phys.right - from_slot_phys.left).max(1),
                (from_slot_phys.bottom - from_slot_phys.top).max(1),
            ),
        ),
        PreviewFit::Contain => (compute_contain_dest_rect(src_w, src_h, from_slot_phys), full_src),
    };

    let interp_dst = lerp_rect(start_dst, to_dest_phys, t_pos);
    let interp_src = lerp_rect(start_src, full_src, t_uncrop.clamp(0.0, 1.0));
    let host_clip = RECT {
        left: 0,
        top: 0,
        right: host_w.max(1),
        bottom: host_h.max(1),
    };
    clip_dest_and_source(interp_dst, interp_src, host_clip)
}

/// Manages RAII DWM live thumbnails attached to the bottom `ThumbnailHostWindow` HWND.
pub struct ThumbnailManager {
    dest_hwnd: HWND,
    entries: HashMap<isize, ThumbEntry>,
}

impl ThumbnailManager {
    pub fn new(dest_hwnd: HWND) -> Self {
        Self {
            dest_hwnd,
            entries: HashMap::new(),
        }
    }

    /// Registers any new target HWNDs without dropping existing entries (used while leaving cards fade out).
    pub fn ensure_targets(&mut self, target_hwnds: &[isize]) {
        for &src_raw in target_hwnds {
            if self.entries.contains_key(&src_raw) {
                continue;
            }
            let src_hwnd = HWND(src_raw as *mut c_void);
            if let Ok(thumb) = DwmThumbnail::register(self.dest_hwnd, src_hwnd) {
                let source_size = thumb.query_source_size().ok().filter(|&(w, h)| w > 0 && h > 0);
                let _ = thumb.update(
                    RECT {
                        left: 0,
                        top: 0,
                        right: 1,
                        bottom: 1,
                    },
                    None,
                    0,
                    false,
                    false,
                );
                self.entries.insert(
                    src_raw,
                    ThumbEntry {
                        thumb,
                        source_size,
                    },
                );
            }
        }
    }

    /// Releases thumbnails not present in `keep_hwnds`.
    pub fn retain_targets(&mut self, keep_hwnds: &[isize]) {
        let keep: HashSet<isize> = keep_hwnds.iter().copied().collect();
        self.entries.retain(|hwnd, _| keep.contains(hwnd));
    }

    pub fn sync_targets(&mut self, target_hwnds: &[isize]) {
        self.retain_targets(target_hwnds);
        self.ensure_targets(target_hwnds);
    }

    pub fn source_size(&self, hwnd: isize) -> Option<(i32, i32)> {
        self.entries.get(&hwnd).and_then(|e| e.source_size)
    }

    /// Re-queries `DwmQueryThumbnailSourceSize` for an already-registered thumbnail
    /// (e.g. right after pre-restoring a minimized window behind the overlay curtain),
    /// preserving any existing cached size if DWM transiently reports zero.
    pub fn refresh_source_size(&mut self, hwnd: isize) -> Option<(i32, i32)> {
        let entry = self.entries.get_mut(&hwnd)?;
        if let Ok((w, h)) = entry.thumb.query_source_size()
            && w > 0
            && h > 0
        {
            entry.source_size = Some((w, h));
        }
        entry.source_size
    }

    pub fn unregister_all(&mut self) {
        self.entries.clear();
    }

    pub fn active_count(&self) -> usize {
        self.entries.len()
    }

    pub fn hide_thumbnail(&mut self, hwnd: isize) {
        if let Some(entry) = self.entries.get_mut(&hwnd) {
            let _ = entry.thumb.update(
                RECT {
                    left: 0,
                    top: 0,
                    right: 1,
                    bottom: 1,
                },
                None,
                0,
                false,
                false,
            );
        }
    }

    pub fn hide_all_except(&mut self, keep_hwnd: Option<isize>) {
        for (&hwnd, entry) in &mut self.entries {
            if Some(hwnd) == keep_hwnd {
                continue;
            }
            let _ = entry.thumb.update(
                RECT {
                    left: 0,
                    top: 0,
                    right: 1,
                    bottom: 1,
                },
                None,
                0,
                false,
                false,
            );
        }
    }

    /// Positions and viewport-clips a thumbnail inside its fixed-size `slot_bounds` (GPUI logical px)
    /// with per-thumbnail `opacity` (`0..=255`).
    pub fn update_slot(
        &mut self,
        hwnd: isize,
        slot_bounds: Bounds<Pixels>,
        clip_bounds: Bounds<Pixels>,
        scale: f32,
        fit: PreviewFit,
        opacity: u8,
    ) {
        if opacity == 0 {
            self.hide_thumbnail(hwnd);
            return;
        }
        let Some(entry) = self.entries.get_mut(&hwnd) else {
            return;
        };

        let (src_w, src_h) = match entry.source_size {
            Some(sz) => sz,
            None => match entry.thumb.query_source_size() {
                Ok((w, h)) if w > 0 && h > 0 => {
                    entry.source_size = Some((w, h));
                    (w, h)
                }
                _ => return,
            },
        };

        let slot_rect = RECT {
            left: (f32::from(slot_bounds.origin.x) * scale).round() as i32,
            top: (f32::from(slot_bounds.origin.y) * scale).round() as i32,
            right: (f32::from(slot_bounds.origin.x + slot_bounds.size.width) * scale).round()
                as i32,
            bottom: (f32::from(slot_bounds.origin.y + slot_bounds.size.height) * scale).round()
                as i32,
        };

        let (raw_dst, base_src) = match fit {
            PreviewFit::Cover => (
                slot_rect,
                compute_cover_source_rect(
                    src_w,
                    src_h,
                    (slot_rect.right - slot_rect.left).max(1),
                    (slot_rect.bottom - slot_rect.top).max(1),
                ),
            ),
            PreviewFit::Contain => (
                compute_contain_dest_rect(src_w, src_h, slot_rect),
                RECT {
                    left: 0,
                    top: 0,
                    right: src_w,
                    bottom: src_h,
                },
            ),
        };

        let clip_rect = RECT {
            left: (f32::from(clip_bounds.origin.x) * scale).round() as i32,
            top: (f32::from(clip_bounds.origin.y) * scale).round() as i32,
            right: (f32::from(clip_bounds.origin.x + clip_bounds.size.width) * scale).round()
                as i32,
            bottom: (f32::from(clip_bounds.origin.y + clip_bounds.size.height) * scale).round()
                as i32,
        };

        match clip_dest_and_source(raw_dst, base_src, clip_rect) {
            Some((dest_rect, src_rect)) => {
                let _ = entry
                    .thumb
                    .update(dest_rect, Some(src_rect), opacity, true, false);
            }
            None => {
                let _ = entry.thumb.update(
                    RECT {
                        left: clip_rect.left,
                        top: clip_rect.top,
                        right: clip_rect.left + 1,
                        bottom: clip_rect.top + 1,
                    },
                    None,
                    0,
                    false,
                    false,
                );
            }
        }
    }

    /// Updates a single thumbnail during a Scene C fly-to-window zoom animation.
    #[allow(clippy::too_many_arguments)]
    pub fn update_zoom_to_rect(
        &mut self,
        hwnd: isize,
        from_slot_phys: RECT,
        to_dest_phys: RECT,
        host_size_phys: (i32, i32),
        fit: PreviewFit,
        t_pos: f32,
        t_uncrop: f32,
        opacity: u8,
    ) {
        if opacity == 0 {
            self.hide_thumbnail(hwnd);
            return;
        }
        let Some(entry) = self.entries.get_mut(&hwnd) else {
            return;
        };
        let (src_w, src_h) = match entry.source_size {
            Some(sz) => sz,
            None => match entry.thumb.query_source_size() {
                Ok((w, h)) if w > 0 && h > 0 => {
                    entry.source_size = Some((w, h));
                    (w, h)
                }
                _ => return,
            },
        };

        match compute_zoom_rects(
            src_w,
            src_h,
            from_slot_phys,
            to_dest_phys,
            host_size_phys.0,
            host_size_phys.1,
            fit,
            t_pos,
            t_uncrop,
        ) {
            Some((dest_rect, src_rect)) => {
                let _ = entry
                    .thumb
                    .update(dest_rect, Some(src_rect), opacity, true, false);
            }
            None => {
                let _ = entry.thumb.update(
                    RECT {
                        left: 0,
                        top: 0,
                        right: 1,
                        bottom: 1,
                    },
                    None,
                    0,
                    false,
                    false,
                );
            }
        }
    }

    /// Updates a thumbnail using its full uncropped window surface placed at `dest_phys`
    /// (clipped proportionally to `host_size_phys` so windows can sweep smoothly in from off-edge).
    pub fn update_full_source_rect(
        &mut self,
        hwnd: isize,
        dest_phys: RECT,
        host_size_phys: (i32, i32),
        opacity: u8,
    ) {
        if opacity == 0 {
            self.hide_thumbnail(hwnd);
            return;
        }
        let Some(entry) = self.entries.get_mut(&hwnd) else {
            return;
        };
        let (src_w, src_h) = match entry.source_size {
            Some(sz) => sz,
            None => match entry.thumb.query_source_size() {
                Ok((w, h)) if w > 0 && h > 0 => {
                    entry.source_size = Some((w, h));
                    (w, h)
                }
                _ => return,
            },
        };
        let full_src = RECT {
            left: 0,
            top: 0,
            right: src_w,
            bottom: src_h,
        };
        let host_clip = RECT {
            left: 0,
            top: 0,
            right: host_size_phys.0.max(1),
            bottom: host_size_phys.1.max(1),
        };
        match clip_dest_and_source(dest_phys, full_src, host_clip) {
            Some((dest_rect, src_rect)) => {
                let _ = entry
                    .thumb
                    .update(dest_rect, Some(src_rect), opacity, true, false);
            }
            None => {
                let _ = entry.thumb.update(
                    RECT {
                        left: 0,
                        top: 0,
                        right: 1,
                        bottom: 1,
                    },
                    None,
                    0,
                    false,
                    false,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cover_crop_keeps_top_for_tall_windows_and_centers_wide_windows() {
        // Tall chat window 800x1000 inside 16:10 box (380x226)
        let tall = compute_cover_source_rect(800, 1000, 380, 226);
        assert_eq!(tall.left, 0);
        assert_eq!(tall.right, 800);
        assert_eq!(tall.top, 0);
        assert!(tall.bottom > 450 && tall.bottom < 500);

        // Ultrawide window 2560x800 inside 380x226
        let wide = compute_cover_source_rect(2560, 800, 380, 226);
        assert_eq!(wide.top, 0);
        assert_eq!(wide.bottom, 800);
        assert!(wide.left > 500);
        assert!((wide.left - (2560 - wide.right)).abs() <= 1);
    }

    #[test]
    fn zoom_rects_uncrop_cover_and_clip_to_host_bounds() {
        let from = RECT {
            left: 200,
            top: 200,
            right: 580,
            bottom: 426,
        };
        let to = RECT {
            left: -40,
            top: 0,
            right: 1960,
            bottom: 1040,
        };
        // At t=0, Cover crop matches compute_cover_source_rect
        let (d0, s0) =
            compute_zoom_rects(800, 1000, from, to, 1920, 1080, PreviewFit::Cover, 0.0, 0.0)
                .unwrap();
        assert_eq!(d0.left, 200);
        assert_eq!(d0.right, 580);
        assert_eq!(s0.top, 0);
        assert!(s0.bottom < 550);

        // At t=1, destination is clipped cleanly to host (0..1920) and source expands proportionally
        let (d1, s1) =
            compute_zoom_rects(800, 1000, from, to, 1920, 1080, PreviewFit::Cover, 1.0, 1.0)
                .unwrap();
        assert_eq!(d1.left, 0);
        assert_eq!(d1.right, 1920);
        assert_eq!(d1.bottom, 1040);
        assert!(s1.left > 0 && s1.right < 800);
        assert_eq!(s1.top, 0);
        assert_eq!(s1.bottom, 1000);
    }
}
