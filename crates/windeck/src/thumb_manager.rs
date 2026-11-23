use std::collections::{HashMap, HashSet};
use std::ffi::c_void;

use gpui::{Bounds, Pixels};
use win_platform::DwmThumbnail;
use windows::Win32::Foundation::{HWND, RECT};

struct ThumbEntry {
    thumb: DwmThumbnail,
    source_size: Option<(i32, i32)>,
}

/// Manages RAII DWM live thumbnails attached to the GPUI overlay HWND.
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

    /// Re-registers thumbnails for the exact active target window set.
    /// Any previously registered windows not in `target_hwnds` are immediately dropped (`DwmUnregisterThumbnail`).
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
                // Keep hidden until first layout pass supplies exact bounds.
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

    pub fn has_live_surface(&self, hwnd: isize) -> bool {
        self.entries
            .get(&hwnd)
            .is_some_and(|e| e.source_size.is_some())
    }

    /// Immediately drops every DWM thumbnail relationship (called whenever the overlay hides).
    pub fn unregister_all(&mut self) {
        self.entries.clear();
    }

    pub fn active_count(&self) -> usize {
        self.entries.len()
    }

    /// Positions, aspect-fits, and viewport-clips a thumbnail inside `slot_bounds` (GPUI logical px).
    pub fn update_slot(
        &mut self,
        hwnd: isize,
        slot_bounds: Bounds<Pixels>,
        clip_bounds: Bounds<Pixels>,
        scale: f32,
    ) {
        let Some(entry) = self.entries.get_mut(&hwnd) else {
            return;
        };

        // Refresh source size lazily if previously unavailable
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

        // 3 logical pixels inner inset so GPUI card border/selection ring stays razor-clean.
        let inset = 3.0;
        let box_x = f32::from(slot_bounds.origin.x) + inset;
        let box_y = f32::from(slot_bounds.origin.y) + inset;
        let box_w_log = (f32::from(slot_bounds.size.width) - inset * 2.0).max(1.0);
        let box_h_log = (f32::from(slot_bounds.size.height) - inset * 2.0).max(1.0);

        let mut dst_left = (box_x * scale).round() as i32;
        let mut dst_top = (box_y * scale).round() as i32;
        let mut dst_right = ((box_x + box_w_log) * scale).round() as i32;
        let mut dst_bottom = ((box_y + box_h_log) * scale).round() as i32;

        // Aspect-fit source inside destination container
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

        // Strict vertical & horizontal clipping against the scroll viewport
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

        let orig_h = (dst_bottom - dst_top).max(1) as f32;
        let orig_w = (dst_right - dst_left).max(1) as f32;
        let mut src_rect = RECT {
            left: 0,
            top: 0,
            right: src_w,
            bottom: src_h,
        };

        if dst_top < clip_top {
            let ratio = (clip_top - dst_top) as f32 / orig_h;
            src_rect.top = (ratio * src_h as f32).round().clamp(0.0, (src_h - 1) as f32) as i32;
            dst_top = clip_top;
        }
        if dst_bottom > clip_bottom {
            let ratio = (dst_bottom - clip_bottom) as f32 / orig_h;
            src_rect.bottom =
                src_h - (ratio * src_h as f32).round().clamp(0.0, (src_h - 1) as f32) as i32;
            dst_bottom = clip_bottom;
        }
        if dst_left < clip_left {
            let ratio = (clip_left - dst_left) as f32 / orig_w;
            src_rect.left = (ratio * src_w as f32).round().clamp(0.0, (src_w - 1) as f32) as i32;
            dst_left = clip_left;
        }
        if dst_right > clip_right {
            let ratio = (dst_right - clip_right) as f32 / orig_w;
            src_rect.right =
                src_w - (ratio * src_w as f32).round().clamp(0.0, (src_w - 1) as f32) as i32;
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
