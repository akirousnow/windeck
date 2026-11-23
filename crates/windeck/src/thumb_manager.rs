use std::collections::{HashMap, HashSet};
use std::ffi::c_void;

use gpui::{Bounds, Pixels};
use win_platform::DwmThumbnail;
use windows::Win32::Foundation::{HWND, RECT};

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

    pub fn sync_targets(&mut self, target_hwnds: &[isize]) {
        let keep: HashSet<isize> = target_hwnds.iter().copied().collect();
        self.entries.retain(|hwnd, _| keep.contains(hwnd));

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
                    255,
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

    pub fn source_size(&self, hwnd: isize) -> Option<(i32, i32)> {
        self.entries.get(&hwnd).and_then(|e| e.source_size)
    }

    pub fn unregister_all(&mut self) {
        self.entries.clear();
    }

    pub fn active_count(&self) -> usize {
        self.entries.len()
    }

    /// Positions and viewport-clips a thumbnail inside its fixed-size `slot_bounds` (GPUI logical px).
    pub fn update_slot(
        &mut self,
        hwnd: isize,
        slot_bounds: Bounds<Pixels>,
        clip_bounds: Bounds<Pixels>,
        scale: f32,
        fit: PreviewFit,
    ) {
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

        let mut dst_left = (f32::from(slot_bounds.origin.x) * scale).round() as i32;
        let mut dst_top = (f32::from(slot_bounds.origin.y) * scale).round() as i32;
        let mut dst_right =
            (f32::from(slot_bounds.origin.x + slot_bounds.size.width) * scale).round() as i32;
        let mut dst_bottom =
            (f32::from(slot_bounds.origin.y + slot_bounds.size.height) * scale).round() as i32;

        let base_src = match fit {
            PreviewFit::Cover => compute_cover_source_rect(
                src_w,
                src_h,
                (dst_right - dst_left).max(1),
                (dst_bottom - dst_top).max(1),
            ),
            PreviewFit::Contain => {
                let avail_w = (dst_right - dst_left).max(1) as f32;
                let avail_h = (dst_bottom - dst_top).max(1) as f32;
                let aspect_src = src_w as f32 / src_h as f32;
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
                    left: 0,
                    top: 0,
                    right: src_w,
                    bottom: src_h,
                }
            }
        };

        // Strict viewport scroll clipping (proportionally crops base_src and clamps dst)
        let clip_left = (f32::from(clip_bounds.origin.x) * scale).round() as i32;
        let clip_top = (f32::from(clip_bounds.origin.y) * scale).round() as i32;
        let clip_right =
            (f32::from(clip_bounds.origin.x + clip_bounds.size.width) * scale).round() as i32;
        let clip_bottom =
            (f32::from(clip_bounds.origin.y + clip_bounds.size.height) * scale).round() as i32;

        if dst_bottom <= clip_top
            || dst_top >= clip_bottom
            || dst_right <= clip_left
            || dst_left >= clip_right
        {
            let _ = entry.thumb.update(
                RECT {
                    left: clip_left,
                    top: clip_top,
                    right: clip_left + 1,
                    bottom: clip_top + 1,
                },
                None,
                255,
                false,
                false,
            );
            return;
        }

        let orig_dst_w = (dst_right - dst_left).max(1) as f32;
        let orig_dst_h = (dst_bottom - dst_top).max(1) as f32;
        let base_src_w = (base_src.right - base_src.left).max(1) as f32;
        let base_src_h = (base_src.bottom - base_src.top).max(1) as f32;
        let mut src_rect = base_src;

        if dst_top < clip_top {
            let ratio = (clip_top - dst_top) as f32 / orig_dst_h;
            src_rect.top = base_src.top
                + (ratio * base_src_h)
                    .round()
                    .clamp(0.0, base_src_h - 1.0) as i32;
            dst_top = clip_top;
        }
        if dst_bottom > clip_bottom {
            let ratio = (dst_bottom - clip_bottom) as f32 / orig_dst_h;
            src_rect.bottom = base_src.bottom
                - (ratio * base_src_h)
                    .round()
                    .clamp(0.0, base_src_h - 1.0) as i32;
            dst_bottom = clip_bottom;
        }
        if dst_left < clip_left {
            let ratio = (clip_left - dst_left) as f32 / orig_dst_w;
            src_rect.left = base_src.left
                + (ratio * base_src_w)
                    .round()
                    .clamp(0.0, base_src_w - 1.0) as i32;
            dst_left = clip_left;
        }
        if dst_right > clip_right {
            let ratio = (dst_right - clip_right) as f32 / orig_dst_w;
            src_rect.right = base_src.right
                - (ratio * base_src_w)
                    .round()
                    .clamp(0.0, base_src_w - 1.0) as i32;
            dst_right = clip_right;
        }

        let visible = dst_right > dst_left
            && dst_bottom > dst_top
            && src_rect.right > src_rect.left
            && src_rect.bottom > src_rect.top;

        let dest_rect = RECT {
            left: dst_left,
            top: dst_top,
            right: dst_right.max(dst_left + 1),
            bottom: dst_bottom.max(dst_top + 1),
        };
        let _ = entry
            .thumb
            .update(dest_rect, Some(src_rect), 255, visible, false);
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
}
