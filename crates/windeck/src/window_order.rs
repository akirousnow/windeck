use std::collections::{HashMap, HashSet};

use win_platform::{DesktopId, VirtualDesktopInfo, WindowInfo};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WindowBucket {
    Pinned,
    Desktop(DesktopId),
    Unassigned,
}

pub fn window_bucket(window: &WindowInfo, desktops: &[VirtualDesktopInfo]) -> WindowBucket {
    if window.is_pinned {
        WindowBucket::Pinned
    } else if let Some(idx) = window.desktop_index {
        if let Some(desktop) = desktops.get(idx) {
            WindowBucket::Desktop(desktop.id)
        } else {
            WindowBucket::Unassigned
        }
    } else {
        WindowBucket::Unassigned
    }
}

#[derive(Debug, Clone, Copy)]
struct TrackedSlot {
    pid: u32,
    bucket: WindowBucket,
    seq: u64,
}

/// Maintains a stable per-bucket display order across Win32 Z-order changes.
///
/// Win32 `EnumWindows` enumerates top-level windows in Z-order (most recently focused first),
/// so selecting a window moves it to the front on the next open. `StableWindowOrder` remembers
/// each live window's slot sequence per bucket (pinned / virtual desktop / unassigned) so
/// existing windows stay in the exact same arrangement across selections, while newly opened
/// or moved-in windows append cleanly to the end of their destination bucket.
#[derive(Debug, Default)]
pub struct StableWindowOrder {
    slots: HashMap<isize, TrackedSlot>,
    next_seq: u64,
}

impl StableWindowOrder {
    pub fn new() -> Self {
        Self::default()
    }

    fn alloc_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        seq
    }

    /// Synchronizes remembered slots with the latest enumerated snapshot (in Win32 Z-order).
    ///
    /// - Windows remaining in the same bucket keep their exact sequence key.
    /// - Windows whose bucket changed (e.g., moved to another virtual desktop, pinned/unpinned,
    ///   or migrated when a desktop is closed) are appended to the end of their new bucket
    ///   while preserving their previous relative order.
    /// - Newly discovered windows are appended to the end of their bucket in snapshot order.
    /// - Windows absent from the snapshot stay remembered while `is_alive(hwnd, pid)` is true
    ///   (e.g., minimized to tray or temporarily obscured by a modal dialog) and are pruned once destroyed.
    pub fn sync<F>(
        &mut self,
        windows: &[WindowInfo],
        desktops: &[VirtualDesktopInfo],
        mut is_alive: F,
    ) where
        F: FnMut(isize, u32) -> bool,
    {
        let live_pids: HashMap<isize, u32> = windows.iter().map(|w| (w.hwnd, w.pid)).collect();

        self.slots.retain(|&hwnd, slot| match live_pids.get(&hwnd) {
            Some(&pid) => slot.pid == pid || slot.pid == 0 || pid == 0,
            None => is_alive(hwnd, slot.pid),
        });

        let mut seen_in_pass: HashSet<isize> = HashSet::with_capacity(windows.len());
        let mut moved: Vec<(u64, isize, u32, WindowBucket)> = Vec::new();
        let mut added: Vec<(isize, u32, WindowBucket)> = Vec::new();

        for w in windows {
            if !seen_in_pass.insert(w.hwnd) {
                continue;
            }
            let bucket = window_bucket(w, desktops);
            match self.slots.get_mut(&w.hwnd) {
                Some(existing) if existing.bucket == bucket => {
                    if existing.pid == 0 && w.pid != 0 {
                        existing.pid = w.pid;
                    }
                }
                Some(existing) => {
                    moved.push((existing.seq, w.hwnd, w.pid, bucket));
                }
                None => {
                    added.push((w.hwnd, w.pid, bucket));
                }
            }
        }

        moved.sort_by_key(|&(old_seq, _, _, _)| old_seq);
        for (_, hwnd, pid, bucket) in moved {
            let seq = self.alloc_seq();
            self.slots.insert(hwnd, TrackedSlot { pid, bucket, seq });
        }

        for (hwnd, pid, bucket) in added {
            let seq = self.alloc_seq();
            self.slots.insert(hwnd, TrackedSlot { pid, bucket, seq });
        }
    }

    pub fn sort_key(&self, hwnd: isize) -> u64 {
        self.slots.get(&hwnd).map_or(u64::MAX, |s| s.seq)
    }

    pub fn ordered(&self, windows: &[WindowInfo]) -> Vec<WindowInfo> {
        let mut out = windows.to_vec();
        out.sort_by_key(|w| self.sort_key(w.hwnd));
        out
    }
}

/// Selects the initial highlighted card index when opening WinDeck:
/// picks the most recently active window (by `z_order_windows`) inside the current
/// virtual desktop group other than `prev_foreground_hwnd`, falling back to the
/// first card of the current desktop group, or `0`.
pub fn pick_initial_selection_index(
    z_order_windows: &[WindowInfo],
    current_group_cards: &[(isize, usize)],
    prev_foreground_hwnd: isize,
) -> usize {
    if current_group_cards.is_empty() {
        return 0;
    }
    for win in z_order_windows {
        if win.hwnd == prev_foreground_hwnd {
            continue;
        }
        if let Some(&(_, flat_idx)) = current_group_cards
            .iter()
            .find(|&&(hwnd, _)| hwnd == win.hwnd)
        {
            return flat_idx;
        }
    }
    current_group_cards[0].1
}

#[cfg(test)]
mod tests {
    use windows::Win32::Foundation::RECT;

    use super::*;

    fn desktop_id(n: u128) -> DesktopId {
        DesktopId::from_le_bytes(&n.to_le_bytes()).expect("16-byte desktop id")
    }

    fn make_desktop(index: usize, id: DesktopId, is_current: bool) -> VirtualDesktopInfo {
        VirtualDesktopInfo {
            index,
            id,
            custom_name: None,
            display_name: format!("Desktop {}", index + 1),
            is_current,
            source: "test",
        }
    }

    fn make_win(
        hwnd: isize,
        pid: u32,
        desktop: Option<(usize, DesktopId)>,
        is_pinned: bool,
    ) -> WindowInfo {
        WindowInfo {
            hwnd,
            pid,
            process_name: format!("app_{pid}.exe"),
            exe_path: String::new(),
            class_name: "TestWnd".to_string(),
            title: format!("Window {hwnd}"),
            desktop_id: desktop.map(|(_, id)| id),
            desktop_index: desktop.map(|(idx, _)| idx),
            is_pinned,
            is_minimized: false,
            is_maximized: false,
            restore_to_maximized: false,
            is_elevated: Some(false),
            cloaked_flags: 0,
            monitor_index: Some(0),
            rect: RECT::default(),
        }
    }

    fn hwnds(list: &[WindowInfo]) -> Vec<isize> {
        list.iter().map(|w| w.hwnd).collect()
    }

    #[test]
    fn selecting_window_and_reopening_keeps_arrangement_unchanged() {
        let d1 = desktop_id(1);
        let desktops = vec![make_desktop(0, d1, true)];
        let mut order = StableWindowOrder::new();

        // First open: Win32 Z-order is [10, 20, 30, 40]
        let snap1 = vec![
            make_win(10, 100, Some((0, d1)), false),
            make_win(20, 200, Some((0, d1)), false),
            make_win(30, 300, Some((0, d1)), false),
            make_win(40, 400, Some((0, d1)), false),
        ];
        order.sync(&snap1, &desktops, |_, _| true);
        assert_eq!(hwnds(&order.ordered(&snap1)), vec![10, 20, 30, 40]);

        // User selects window 30 -> Win32 moves 30 to top of Z-order: [30, 10, 20, 40]
        let snap2 = vec![
            make_win(30, 300, Some((0, d1)), false),
            make_win(10, 100, Some((0, d1)), false),
            make_win(20, 200, Some((0, d1)), false),
            make_win(40, 400, Some((0, d1)), false),
        ];
        order.sync(&snap2, &desktops, |_, _| true);
        assert_eq!(hwnds(&order.ordered(&snap2)), vec![10, 20, 30, 40]);

        // User selects window 40 -> Win32 Z-order becomes [40, 30, 10, 20]
        let snap3 = vec![
            make_win(40, 400, Some((0, d1)), false),
            make_win(30, 300, Some((0, d1)), false),
            make_win(10, 100, Some((0, d1)), false),
            make_win(20, 200, Some((0, d1)), false),
        ];
        order.sync(&snap3, &desktops, |_, _| true);
        assert_eq!(hwnds(&order.ordered(&snap3)), vec![10, 20, 30, 40]);
    }

    #[test]
    fn new_windows_append_to_end_and_closed_windows_preserve_relative_order() {
        let d1 = desktop_id(1);
        let desktops = vec![make_desktop(0, d1, true)];
        let mut order = StableWindowOrder::new();

        let snap1 = vec![
            make_win(10, 100, Some((0, d1)), false),
            make_win(20, 200, Some((0, d1)), false),
            make_win(30, 300, Some((0, d1)), false),
        ];
        order.sync(&snap1, &desktops, |_, _| true);

        // New window 50 opens (at top of Z-order!), window 20 is closed (destroyed)
        let snap2 = vec![
            make_win(50, 500, Some((0, d1)), false),
            make_win(30, 300, Some((0, d1)), false),
            make_win(10, 100, Some((0, d1)), false),
        ];
        order.sync(&snap2, &desktops, |hwnd, _| hwnd != 20);
        assert_eq!(hwnds(&order.ordered(&snap2)), vec![10, 30, 50]);
    }

    #[test]
    fn temporarily_hidden_window_returns_to_original_slot_when_shown_again() {
        let d1 = desktop_id(1);
        let desktops = vec![make_desktop(0, d1, true)];
        let mut order = StableWindowOrder::new();

        let snap1 = vec![
            make_win(10, 100, Some((0, d1)), false),
            make_win(20, 200, Some((0, d1)), false),
            make_win(30, 300, Some((0, d1)), false),
        ];
        order.sync(&snap1, &desktops, |_, _| true);

        // Window 20 hides to tray (still alive in OS)
        let snap2 = vec![
            make_win(30, 300, Some((0, d1)), false),
            make_win(10, 100, Some((0, d1)), false),
        ];
        order.sync(&snap2, &desktops, |_, _| true);
        assert_eq!(hwnds(&order.ordered(&snap2)), vec![10, 30]);

        // Window 20 restores from tray at top of Z-order -> returns to its original slot between 10 and 30
        let snap3 = vec![
            make_win(20, 200, Some((0, d1)), false),
            make_win(30, 300, Some((0, d1)), false),
            make_win(10, 100, Some((0, d1)), false),
        ];
        order.sync(&snap3, &desktops, |_, _| true);
        assert_eq!(hwnds(&order.ordered(&snap3)), vec![10, 20, 30]);
    }

    #[test]
    fn recycled_hwnd_for_different_process_appends_as_new_window() {
        let d1 = desktop_id(1);
        let desktops = vec![make_desktop(0, d1, true)];
        let mut order = StableWindowOrder::new();

        let snap1 = vec![
            make_win(10, 100, Some((0, d1)), false),
            make_win(20, 200, Some((0, d1)), false),
            make_win(30, 300, Some((0, d1)), false),
        ];
        order.sync(&snap1, &desktops, |_, _| true);

        // HWND 10 was destroyed and reused by PID 999 between two refreshes
        let snap2 = vec![
            make_win(10, 999, Some((0, d1)), false),
            make_win(30, 300, Some((0, d1)), false),
            make_win(20, 200, Some((0, d1)), false),
        ];
        order.sync(&snap2, &desktops, |_, _| false);
        assert_eq!(hwnds(&order.ordered(&snap2)), vec![20, 30, 10]);
    }

    #[test]
    fn moving_windows_across_desktops_appends_without_shifting_destination_and_keeps_relative_order()
    {
        let d1 = desktop_id(1);
        let d2 = desktop_id(2);
        let desktops_both = vec![make_desktop(0, d1, true), make_desktop(1, d2, false)];
        let mut order = StableWindowOrder::new();

        // Desktop 1: [10, 20], Desktop 2: [30, 40]
        let snap1 = vec![
            make_win(10, 100, Some((0, d1)), false),
            make_win(20, 200, Some((0, d1)), false),
            make_win(30, 300, Some((1, d2)), false),
            make_win(40, 400, Some((1, d2)), false),
        ];
        order.sync(&snap1, &desktops_both, |_, _| true);

        // Desktop 1 is closed -> its windows [10, 20] migrate into Desktop 2 (even if Z-order reversed!)
        let desktops_d2_only = vec![make_desktop(0, d2, true)];
        let snap2 = vec![
            make_win(20, 200, Some((0, d2)), false),
            make_win(10, 100, Some((0, d2)), false),
            make_win(40, 400, Some((0, d2)), false),
            make_win(30, 300, Some((0, d2)), false),
        ];
        order.sync(&snap2, &desktops_d2_only, |_, _| true);
        // Existing Desktop 2 windows [30, 40] stay in front; migrated [10, 20] append in their prior order
        assert_eq!(hwnds(&order.ordered(&snap2)), vec![30, 40, 10, 20]);
    }

    #[test]
    fn switching_to_window_on_other_desktop_preserves_order_on_all_desktops_and_pinned() {
        let d1 = desktop_id(1);
        let d2 = desktop_id(2);
        let desktops1 = vec![make_desktop(0, d1, true), make_desktop(1, d2, false)];
        let mut order = StableWindowOrder::new();

        let snap1 = vec![
            make_win(1, 10, Some((0, d1)), true),  // pinned #1
            make_win(2, 20, Some((0, d1)), true),  // pinned #2
            make_win(10, 100, Some((0, d1)), false),
            make_win(20, 200, Some((0, d1)), false),
            make_win(30, 300, Some((1, d2)), false),
            make_win(40, 400, Some((1, d2)), false),
        ];
        order.sync(&snap1, &desktops1, |_, _| true);

        // User switches to window 40 on Desktop 2 (Desktop 2 becomes current; pinned windows now report d2 as desktop_id)
        let desktops2 = vec![make_desktop(0, d1, false), make_desktop(1, d2, true)];
        let snap2 = vec![
            make_win(40, 400, Some((1, d2)), false),
            make_win(2, 20, Some((1, d2)), true),
            make_win(1, 10, Some((1, d2)), true),
            make_win(30, 300, Some((1, d2)), false),
            make_win(20, 200, Some((0, d1)), false),
            make_win(10, 100, Some((0, d1)), false),
        ];
        order.sync(&snap2, &desktops2, |_, _| true);

        let ordered = order.ordered(&snap2);
        let pinned: Vec<isize> = ordered
            .iter()
            .filter(|w| w.is_pinned)
            .map(|w| w.hwnd)
            .collect();
        let d1_wins: Vec<isize> = ordered
            .iter()
            .filter(|w| !w.is_pinned && w.desktop_id == Some(d1))
            .map(|w| w.hwnd)
            .collect();
        let d2_wins: Vec<isize> = ordered
            .iter()
            .filter(|w| !w.is_pinned && w.desktop_id == Some(d2))
            .map(|w| w.hwnd)
            .collect();

        assert_eq!(pinned, vec![1, 2]);
        assert_eq!(d1_wins, vec![10, 20]);
        assert_eq!(d2_wins, vec![30, 40]);
    }

    #[test]
    fn initial_selection_picks_mru_non_foreground_card_in_stable_grid() {
        let d1 = desktop_id(1);
        // Stable card layout in current desktop: [(10, flat=0), (20, flat=1), (30, flat=2), (40, flat=3)]
        let current_cards = [(10, 0), (20, 1), (30, 2), (40, 3)];
        // Win32 Z-order: 40 (active right before open), 30 (previous active), 10, 20
        let z_order = vec![
            make_win(40, 400, Some((0, d1)), false),
            make_win(30, 300, Some((0, d1)), false),
            make_win(10, 100, Some((0, d1)), false),
            make_win(20, 200, Some((0, d1)), false),
        ];

        assert_eq!(pick_initial_selection_index(&z_order, &current_cards, 40), 2);
        // Single window fallback selects that window
        assert_eq!(pick_initial_selection_index(&z_order, &[(40, 2)], 40), 2);
    }

    #[test]
    fn transient_empty_desktops_hiccup_recovers_original_order() {
        let d1 = desktop_id(1);
        let d2 = desktop_id(2);
        let desktops = vec![make_desktop(0, d1, true), make_desktop(1, d2, false)];
        let mut order = StableWindowOrder::new();

        let snap1 = vec![
            make_win(10, 100, Some((0, d1)), false),
            make_win(20, 200, Some((0, d1)), false),
            make_win(30, 300, Some((1, d2)), false),
            make_win(40, 400, Some((1, d2)), false),
        ];
        order.sync(&snap1, &desktops, |_, _| true);

        // Explorer restart momentarily yields empty desktops (all windows fall into Unassigned)
        let snap_hiccup = vec![
            make_win(40, 400, None, false),
            make_win(20, 200, None, false),
            make_win(30, 300, None, false),
            make_win(10, 100, None, false),
        ];
        order.sync(&snap_hiccup, &[], |_, _| true);

        // Explorer recovers with shuffled Z-order
        let snap_recovered = vec![
            make_win(20, 200, Some((0, d1)), false),
            make_win(40, 400, Some((1, d2)), false),
            make_win(10, 100, Some((0, d1)), false),
            make_win(30, 300, Some((1, d2)), false),
        ];
        order.sync(&snap_recovered, &desktops, |_, _| true);
        assert_eq!(hwnds(&order.ordered(&snap_recovered)), vec![10, 20, 30, 40]);
    }
}
