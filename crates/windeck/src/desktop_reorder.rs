use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{Bounds, Pixels, Point, RenderImage, SharedString};
use win_platform::DesktopId;

use crate::animation::{TransitionProfile, exp_damp};
use crate::virtual_spaces::SpaceId;

pub const CAPSULE_PILL_GAP: f32 = 4.0;
pub const PILL_SHIFT_DAMP_SPEED: f32 = 26.0;
pub const PILL_SETTLE_DURATION_MS: u64 = 155;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopPressAction {
    NativePill(DesktopId),
    VirtualSpacePill(SpaceId),
    GroupHeader(DesktopId),
    GroupEmptyBar(DesktopId),
    GroupBackground(DesktopId),
}

impl DesktopPressAction {
    pub fn reorder_source(self) -> DesktopReorderSource {
        match self {
            DesktopPressAction::NativePill(did) => DesktopReorderSource::NativeTopPill(did),
            DesktopPressAction::VirtualSpacePill(sid) => {
                DesktopReorderSource::VirtualSpaceTopPill(sid)
            }
            DesktopPressAction::GroupHeader(did)
            | DesktopPressAction::GroupEmptyBar(did)
            | DesktopPressAction::GroupBackground(did) => {
                DesktopReorderSource::NativeContentGroup(did)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopReorderSource {
    /// Dragging a Windows native virtual desktop pill inside the top-left capsule.
    NativeTopPill(DesktopId),
    /// Dragging a Windows native virtual desktop section inside the scrollable content canvas.
    NativeContentGroup(DesktopId),
    /// Dragging a WinDeck logical virtual space pill inside the top-right capsule.
    VirtualSpaceTopPill(SpaceId),
}

impl DesktopReorderSource {
    pub fn is_native(self) -> bool {
        matches!(
            self,
            DesktopReorderSource::NativeTopPill(_) | DesktopReorderSource::NativeContentGroup(_)
        )
    }

    pub fn is_content_group(self) -> bool {
        matches!(self, DesktopReorderSource::NativeContentGroup(_))
    }

    pub fn is_top_pill(self) -> bool {
        matches!(
            self,
            DesktopReorderSource::NativeTopPill(_) | DesktopReorderSource::VirtualSpaceTopPill(_)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReorderPillKey {
    Native(DesktopId),
    VirtualSpace(SpaceId),
}

#[derive(Debug, Clone, Copy)]
pub struct PillSlotSnapshot {
    pub key: ReorderPillKey,
    pub left: f32,
    pub width: f32,
}

impl PillSlotSnapshot {
    pub fn right(&self) -> f32 {
        self.left + self.width
    }

    pub fn mid_x(&self) -> f32 {
        self.left + self.width * 0.5
    }
}

#[derive(Debug, Clone, Copy)]
pub struct GroupSlotLayout {
    /// Top Y in scroll-independent content coordinates (including `optical_top_pad`).
    pub content_top: f32,
    /// Bottom Y in scroll-independent content coordinates.
    pub content_bottom: f32,
}

impl GroupSlotLayout {
    pub fn content_mid_y(&self) -> f32 {
        (self.content_top + self.content_bottom) * 0.5
    }
}

pub struct PendingDesktopReorder {
    pub action: DesktopPressAction,
    pub source: DesktopReorderSource,
    pub from_index: usize,
    pub can_drag: bool,
    pub press_pos: Point<Pixels>,
    pub origin_bounds: Bounds<Pixels>,
    pub grab_offset: Point<Pixels>,
}

#[derive(Clone)]
pub struct FloatingDesktopBadge {
    pub label: SharedString,
    pub is_current: bool,
    pub live_count: usize,
    pub app_icons: Vec<Arc<RenderImage>>,
    pub total_windows: usize,
}

pub struct ActiveDesktopReorder {
    pub source: DesktopReorderSource,
    pub from_index: usize,
    pub target_index: usize,
    pub origin_bounds: Bounds<Pixels>,
    pub grab_offset: Point<Pixels>,
    pub cursor_pos: Point<Pixels>,
    pub badge: FloatingDesktopBadge,
    /// Rest-position snapshots of every reorderable pill in the active capsule,
    /// captured once at drag start so hit-testing never oscillates.
    pub rest_pill_slots: Vec<PillSlotSnapshot>,
    pub pill_gap: f32,
    /// Smoothed horizontal visual offset (logical px) per original index in `rest_pill_slots`.
    pub pill_visual_dx: Vec<f32>,
    pub last_anim_tick: Option<Instant>,
}

impl ActiveDesktopReorder {
    /// Computes `(left, top, width, height)` for the lifted floating pill during a top-capsule drag.
    pub fn floating_pill_rect(&self) -> Option<(f32, f32, f32, f32)> {
        if !self.source.is_top_pill() {
            return None;
        }
        let width = f32::from(self.origin_bounds.size.width).max(54.0);
        let height = f32::from(self.origin_bounds.size.height).max(24.0);
        let raw_left = f32::from(self.cursor_pos.x - self.grab_offset.x);
        let clamped_left = if let (Some(first), Some(last)) =
            (self.rest_pill_slots.first(), self.rest_pill_slots.last())
        {
            let min_left = first.left - 8.0;
            let max_left = (last.right() - width).max(first.left) + 8.0;
            raw_left.clamp(min_left, max_left)
        } else {
            raw_left
        };
        let top = f32::from(self.origin_bounds.origin.y) - 1.5;
        Some((clamped_left, top, width, height))
    }

    pub fn visual_dx_for_key(&self, key: ReorderPillKey) -> f32 {
        self.rest_pill_slots
            .iter()
            .position(|s| s.key == key)
            .and_then(|idx| self.pill_visual_dx.get(idx).copied())
            .unwrap_or(0.0)
    }

    /// Advances frame-rate-independent sibling pill offsets toward their live target shift.
    /// Returns `true` if any pill is still gliding and requires another vsync frame.
    pub fn advance_pill_offsets(&mut self, now: Instant, animate: bool) -> bool {
        if self.rest_pill_slots.is_empty() {
            self.last_anim_tick = None;
            return false;
        }
        if self.pill_visual_dx.len() != self.rest_pill_slots.len() {
            self.pill_visual_dx.resize(self.rest_pill_slots.len(), 0.0);
        }
        let dt = self
            .last_anim_tick
            .map(|last| now.saturating_duration_since(last).as_secs_f32())
            .unwrap_or(1.0 / 60.0);

        let mut still_moving = false;
        for i in 0..self.rest_pill_slots.len() {
            let target = target_pill_dx_for_slot(
                &self.rest_pill_slots,
                self.from_index,
                self.target_index,
                self.pill_gap,
                i,
            );
            if animate {
                let next = exp_damp(self.pill_visual_dx[i], target, PILL_SHIFT_DAMP_SPEED, dt);
                self.pill_visual_dx[i] = next;
                if (target - next).abs() >= 0.45 {
                    still_moving = true;
                }
            } else {
                self.pill_visual_dx[i] = target;
            }
        }
        self.last_anim_tick = if still_moving { Some(now) } else { None };
        still_moving
    }

    /// Builds a short FLIP settle animation so pills glide smoothly from their release positions
    /// into their exact final (or original, on cancel) layout slots without teleporting.
    pub fn build_settle_anim(
        &self,
        committed: bool,
        duration: Duration,
        use_spring: bool,
    ) -> Option<PillSettleAnim> {
        if self.rest_pill_slots.is_empty() || duration.is_zero() {
            return None;
        }
        let rail_left = self.rest_pill_slots[0].left;
        let mut final_order: Vec<usize> = (0..self.rest_pill_slots.len()).collect();
        if committed && self.from_index != self.target_index {
            reorder_vec_indices(&mut final_order, self.from_index, self.target_index);
        }

        let floating_left = self.floating_pill_rect().map(|(l, _, _, _)| l);
        let mut cursor_x = rail_left;
        let mut initial_dx = HashMap::new();
        for &orig_idx in &final_order {
            let slot = &self.rest_pill_slots[orig_idx];
            let current_visual_left = if orig_idx == self.from_index && self.source.is_top_pill() {
                floating_left.unwrap_or(slot.left)
            } else {
                slot.left + self.pill_visual_dx.get(orig_idx).copied().unwrap_or(0.0)
            };
            let dx = current_visual_left - cursor_x;
            if dx.abs() >= 0.5 {
                initial_dx.insert(slot.key, dx);
            }
            cursor_x += slot.width + self.pill_gap;
        }

        if initial_dx.is_empty() {
            return None;
        }
        Some(PillSettleAnim {
            started_at: Instant::now(),
            duration,
            use_spring,
            initial_dx,
        })
    }
}

pub struct PillSettleAnim {
    pub started_at: Instant,
    pub duration: Duration,
    pub use_spring: bool,
    pub initial_dx: HashMap<ReorderPillKey, f32>,
}

impl PillSettleAnim {
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

    pub fn dx_for(&self, key: ReorderPillKey, now: Instant) -> f32 {
        let Some(&start_dx) = self.initial_dx.get(&key) else {
            return 0.0;
        };
        let t = self.raw_progress(now);
        if t >= 1.0 {
            return 0.0;
        }
        let eased = TransitionProfile::ease_position(self.use_spring, t);
        start_dx * (1.0 - eased)
    }
}

/// Computes the exact horizontal offset (logical px) for pill `item_index` when item
/// `from_index` previews moving to `target_index`, supporting arbitrary variable pill widths.
pub fn target_pill_dx_for_slot(
    slots: &[PillSlotSnapshot],
    from_index: usize,
    target_index: usize,
    gap: f32,
    item_index: usize,
) -> f32 {
    if from_index >= slots.len()
        || target_index >= slots.len()
        || item_index >= slots.len()
        || from_index == target_index
    {
        return 0.0;
    }
    let dragged_stride = slots[from_index].width + gap;
    if target_index > from_index {
        if item_index > from_index && item_index <= target_index {
            -dragged_stride
        } else if item_index == from_index {
            slots[(from_index + 1)..=target_index]
                .iter()
                .map(|s| s.width + gap)
                .sum()
        } else {
            0.0
        }
    } else if item_index >= target_index && item_index < from_index {
        dragged_stride
    } else if item_index == from_index {
        -slots[target_index..from_index]
            .iter()
            .map(|s| s.width + gap)
            .sum::<f32>()
    } else {
        0.0
    }
}

/// Resolves the final index (`0..slots.len()`) when dragging item `from_index` horizontally
/// across immutable rest-position `slots` using pointer `cursor_x`.
pub fn resolve_horizontal_target_index(
    slots: &[PillSlotSnapshot],
    from_index: usize,
    cursor_x: f32,
) -> usize {
    if slots.len() <= 1 {
        return 0;
    }
    let mut target = 0usize;
    for (idx, slot) in slots.iter().enumerate() {
        if idx == from_index {
            continue;
        }
        if cursor_x >= slot.mid_x() {
            target += 1;
        }
    }
    target.min(slots.len() - 1)
}

/// Resolves the final desktop index (`0..groups.len()`) when dragging desktop `from_index`
/// vertically in scroll-independent content coordinates (`cursor_content_y`).
pub fn resolve_vertical_target_index(
    groups: &[GroupSlotLayout],
    from_index: usize,
    cursor_content_y: f32,
) -> usize {
    if groups.len() <= 1 {
        return 0;
    }
    let mut target = 0usize;
    for (idx, group) in groups.iter().enumerate() {
        if idx == from_index {
            continue;
        }
        if cursor_content_y >= group.content_mid_y() {
            target += 1;
        }
    }
    target.min(groups.len() - 1)
}

/// Computes the horizontal insertion bar's Y coordinate (in scroll-independent content space)
/// for moving native desktop group `from_index` to `target_index`.
/// Returns `None` when `target_index == from_index` (no-op drop).
pub fn compute_insertion_line_content_y(
    groups: &[GroupSlotLayout],
    from_index: usize,
    target_index: usize,
    group_gap: f32,
) -> Option<f32> {
    if groups.len() <= 1 || from_index >= groups.len() || target_index >= groups.len() {
        return None;
    }
    if target_index == from_index {
        return None;
    }
    if target_index < from_index {
        let anchor = &groups[target_index];
        if target_index == 0 {
            Some((anchor.content_top - group_gap * 0.45).max(4.0))
        } else {
            let prev = &groups[target_index - 1];
            Some((prev.content_bottom + anchor.content_top) * 0.5)
        }
    } else {
        let anchor = &groups[target_index];
        if target_index + 1 < groups.len() {
            let next = &groups[target_index + 1];
            Some((anchor.content_bottom + next.content_top) * 0.5)
        } else {
            Some(anchor.content_bottom + group_gap * 0.45)
        }
    }
}

/// Moves element at `from_index` to `target_index` in-place. Returns `true` if the slice changed.
pub fn reorder_vec_indices<T>(items: &mut Vec<T>, from_index: usize, target_index: usize) -> bool {
    if items.len() <= 1 || from_index >= items.len() {
        return false;
    }
    let clamped_target = target_index.min(items.len() - 1);
    if from_index == clamped_target {
        return false;
    }
    let item = items.remove(from_index);
    items.insert(clamped_target, item);
    true
}

#[cfg(test)]
mod tests {
    use gpui::{point, px, size};

    use super::*;

    fn did(n: u128) -> DesktopId {
        DesktopId::from_le_bytes(&n.to_le_bytes()).expect("16-byte desktop id")
    }

    fn make_slots(widths: &[f32], gap: f32) -> Vec<PillSlotSnapshot> {
        let mut x = 100.0;
        widths
            .iter()
            .enumerate()
            .map(|(i, &w)| {
                let slot = PillSlotSnapshot {
                    key: ReorderPillKey::Native(did((i + 1) as u128)),
                    left: x,
                    width: w,
                };
                x += w + gap;
                slot
            })
            .collect()
    }

    #[test]
    fn horizontal_target_index_handles_unequal_widths_monotonically() {
        // Wide pill P0 (180px: 100..280), narrow P1 (60px: 284..344, mid=314), medium P2 (90px: 348..438, mid=393)
        let slots = make_slots(&[180.0, 60.0, 90.0], 4.0);

        // Dragging wide P0 rightward across narrow P1 and medium P2
        assert_eq!(resolve_horizontal_target_index(&slots, 0, 150.0), 0);
        assert_eq!(resolve_horizontal_target_index(&slots, 0, 313.9), 0);
        assert_eq!(resolve_horizontal_target_index(&slots, 0, 314.0), 1);
        assert_eq!(resolve_horizontal_target_index(&slots, 0, 392.9), 1);
        assert_eq!(resolve_horizontal_target_index(&slots, 0, 393.0), 2);

        // Dragging narrow P1 leftward across wide P0 (mid=190) and rightward across P2 (mid=393)
        assert_eq!(resolve_horizontal_target_index(&slots, 1, 189.9), 0);
        assert_eq!(resolve_horizontal_target_index(&slots, 1, 190.0), 1);
        assert_eq!(resolve_horizontal_target_index(&slots, 1, 392.9), 1);
        assert_eq!(resolve_horizontal_target_index(&slots, 1, 393.0), 2);

        // Dragging P2 leftward across P1 (mid=314) and P0 (mid=190)
        assert_eq!(resolve_horizontal_target_index(&slots, 2, 314.0), 2);
        assert_eq!(resolve_horizontal_target_index(&slots, 2, 313.9), 1);
        assert_eq!(resolve_horizontal_target_index(&slots, 2, 189.9), 0);
    }

    #[test]
    fn variable_width_pill_shifts_pack_without_overlap_or_gaps() {
        let gap = 4.0;
        let slots = make_slots(&[100.0, 60.0, 80.0, 120.0], gap);
        let orig_right = slots.last().unwrap().right();

        for from in 0..slots.len() {
            for target in 0..slots.len() {
                let mut shifted: Vec<(f32, f32)> = slots
                    .iter()
                    .enumerate()
                    .map(|(i, s)| {
                        let dx = target_pill_dx_for_slot(&slots, from, target, gap, i);
                        (s.left + dx, s.width)
                    })
                    .collect();
                shifted.sort_by(|a, b| a.0.total_cmp(&b.0));

                assert!((shifted[0].0 - slots[0].left).abs() < 1e-3);
                for pair in shifted.windows(2) {
                    let prev_right = pair[0].0 + pair[0].1;
                    assert!(
                        (pair[1].0 - (prev_right + gap)).abs() < 1e-3,
                        "from={from} target={target}: expected exact gap {gap}, got {}",
                        pair[1].0 - prev_right
                    );
                }
                let final_right = shifted.last().unwrap().0 + shifted.last().unwrap().1;
                assert!((final_right - orig_right).abs() < 1e-3);
            }
        }
    }

    #[test]
    fn vertical_target_and_insertion_line_handle_variable_height_desktop_groups() {
        let gap = 28.0;
        // Group 0: tall (14..414, mid=214)
        // Group 1: empty/short (442..508, mid=475)
        // Group 2: medium (536..786, mid=661)
        let groups = vec![
            GroupSlotLayout {
                content_top: 14.0,
                content_bottom: 414.0,
            },
            GroupSlotLayout {
                content_top: 442.0,
                content_bottom: 508.0,
            },
            GroupSlotLayout {
                content_top: 536.0,
                content_bottom: 786.0,
            },
        ];

        // Dragging Group 0 downward
        assert_eq!(resolve_vertical_target_index(&groups, 0, 474.0), 0);
        assert_eq!(compute_insertion_line_content_y(&groups, 0, 0, gap), None);

        assert_eq!(resolve_vertical_target_index(&groups, 0, 476.0), 1);
        let y_between_1_2 = compute_insertion_line_content_y(&groups, 0, 1, gap).unwrap();
        assert!((y_between_1_2 - 522.0).abs() < 1e-3);

        assert_eq!(resolve_vertical_target_index(&groups, 0, 662.0), 2);
        let y_after_2 = compute_insertion_line_content_y(&groups, 0, 2, gap).unwrap();
        assert!(y_after_2 > 786.0);

        // Dragging Group 2 upward
        assert_eq!(resolve_vertical_target_index(&groups, 2, 476.0), 2);
        assert_eq!(compute_insertion_line_content_y(&groups, 2, 2, gap), None);

        assert_eq!(resolve_vertical_target_index(&groups, 2, 474.0), 1);
        let y_between_0_1 = compute_insertion_line_content_y(&groups, 2, 1, gap).unwrap();
        assert!((y_between_0_1 - 428.0).abs() < 1e-3);

        assert_eq!(resolve_vertical_target_index(&groups, 2, 213.0), 0);
        let y_before_0 = compute_insertion_line_content_y(&groups, 2, 0, gap).unwrap();
        assert!((4.0..14.0).contains(&y_before_0));
    }

    #[test]
    fn pill_settle_anim_computes_exact_delta_for_commit_and_cancel() {
        let gap = 4.0;
        let slots = make_slots(&[100.0, 60.0, 80.0], gap);
        let d1 = did(1);
        let d2 = did(2);
        let d3 = did(3);

        let reorder = ActiveDesktopReorder {
            source: DesktopReorderSource::NativeTopPill(d1),
            from_index: 0,
            target_index: 2,
            origin_bounds: Bounds {
                origin: point(px(100.0), px(22.0)),
                size: size(px(100.0), px(24.0)),
            },
            grab_offset: point(px(50.0), px(12.0)),
            cursor_pos: point(px(310.0), px(22.0)), // raw_left = 260, max_left = (348 - 100) + 8 = 256
            badge: FloatingDesktopBadge {
                label: SharedString::from("桌面 1"),
                is_current: true,
                live_count: 0,
                app_icons: Vec::new(),
                total_windows: 2,
            },
            rest_pill_slots: slots.clone(),
            pill_gap: gap,
            pill_visual_dx: vec![0.0, -104.0, -104.0],
            last_anim_tick: None,
        };

        // Committed move [d1, d2, d3] -> [d2(100..160), d3(164..244), d1(248..348)]:
        // d2 & d3 are already at exact final rest positions; d1 floats at 256 -> settles +8 -> 0!
        let settle_commit = reorder
            .build_settle_anim(true, Duration::from_millis(150), false)
            .expect("d1 should settle");
        assert!(!settle_commit.initial_dx.contains_key(&ReorderPillKey::Native(d2)));
        assert!(!settle_commit.initial_dx.contains_key(&ReorderPillKey::Native(d3)));
        assert!((settle_commit.initial_dx[&ReorderPillKey::Native(d1)] - 8.0).abs() < 1e-3);

        // Cancelled move stays [d1(100), d2(204), d3(268)]:
        // d1 glides back from 256 -> 100 (+156), d2 & d3 glide back from -104 -> 0!
        let settle_cancel = reorder
            .build_settle_anim(false, Duration::from_millis(150), false)
            .expect("all three should settle back");
        assert!((settle_cancel.initial_dx[&ReorderPillKey::Native(d1)] - 156.0).abs() < 1e-3);
        assert!((settle_cancel.initial_dx[&ReorderPillKey::Native(d2)] - (-104.0)).abs() < 1e-3);
        assert!((settle_cancel.initial_dx[&ReorderPillKey::Native(d3)] - (-104.0)).abs() < 1e-3);
    }
}
