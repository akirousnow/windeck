#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavDirection {
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FlatPos {
    group: usize,
    offset_in_group: usize,
    group_start: usize,
    group_len: usize,
}

fn locate(group_sizes: &[usize], flat_idx: usize) -> Option<FlatPos> {
    let mut start = 0usize;
    for (g, &len) in group_sizes.iter().enumerate() {
        if flat_idx < start + len {
            return Some(FlatPos {
                group: g,
                offset_in_group: flat_idx - start,
                group_start: start,
                group_len: len,
            });
        }
        start += len;
    }
    None
}

fn group_start_offset(group_sizes: &[usize], target_group: usize) -> usize {
    group_sizes.iter().take(target_group).sum()
}

/// Computes the next flat card index for 2D grid navigation across grouped virtual desktop sections.
pub fn navigate(
    group_sizes: &[usize],
    cols: usize,
    current: usize,
    dir: NavDirection,
    cross_desktop: bool,
) -> usize {
    let total: usize = group_sizes.iter().sum();
    if total == 0 {
        return 0;
    }
    let cols = cols.max(1);
    let current = current.min(total - 1);

    match dir {
        NavDirection::Home => return 0,
        NavDirection::End => return total - 1,
        _ => {}
    }

    let Some(pos) = locate(group_sizes, current) else {
        return 0;
    };

    match dir {
        NavDirection::Right => {
            if cross_desktop {
                (current + 1) % total
            } else {
                pos.group_start + (pos.offset_in_group + 1) % pos.group_len
            }
        }
        NavDirection::Left => {
            if cross_desktop {
                if current == 0 { total - 1 } else { current - 1 }
            } else if pos.offset_in_group == 0 {
                pos.group_start + pos.group_len - 1
            } else {
                current - 1
            }
        }
        NavDirection::Down => {
            let row = pos.offset_in_group / cols;
            let col = pos.offset_in_group % cols;
            let total_rows = pos.group_len.div_ceil(cols);

            if row + 1 < total_rows {
                let next_row_start = (row + 1) * cols;
                let next_row_len = (pos.group_len - next_row_start).min(cols);
                return pos.group_start + next_row_start + col.min(next_row_len - 1);
            }

            if !cross_desktop {
                let first_row_len = pos.group_len.min(cols);
                return pos.group_start + col.min(first_row_len - 1);
            }

            // Advance to next non-empty group (wrapping around)
            let n_groups = group_sizes.len();
            for step in 1..=n_groups {
                let next_g = (pos.group + step) % n_groups;
                let next_len = group_sizes[next_g];
                if next_len > 0 {
                    let first_row_len = next_len.min(cols);
                    return group_start_offset(group_sizes, next_g) + col.min(first_row_len - 1);
                }
            }
            current
        }
        NavDirection::Up => {
            let row = pos.offset_in_group / cols;
            let col = pos.offset_in_group % cols;

            if row > 0 {
                let prev_row_start = (row - 1) * cols;
                return pos.group_start + prev_row_start + col;
            }

            if !cross_desktop {
                let total_rows = pos.group_len.div_ceil(cols);
                let last_row_start = (total_rows - 1) * cols;
                let last_row_len = pos.group_len - last_row_start;
                return pos.group_start + last_row_start + col.min(last_row_len - 1);
            }

            // Retreat to previous non-empty group's last row (wrapping around)
            let n_groups = group_sizes.len();
            for step in 1..=n_groups {
                let prev_g = (pos.group + n_groups - step) % n_groups;
                let prev_len = group_sizes[prev_g];
                if prev_len > 0 {
                    let total_rows = prev_len.div_ceil(cols);
                    let last_row_start = (total_rows - 1) * cols;
                    let last_row_len = prev_len - last_row_start;
                    return group_start_offset(group_sizes, prev_g)
                        + last_row_start
                        + col.min(last_row_len - 1);
                }
            }
            current
        }
        NavDirection::Home | NavDirection::End => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn down_and_up_preserve_column_across_uneven_groups() {
        // Group 0: 5 items (cols=4 -> row0: [0,1,2,3], row1: [4])
        // Group 1: 0 items (empty desktop)
        // Group 2: 3 items (row0: [5,6,7])
        let groups = [5, 0, 3];

        // From col 3 (index 3), Down clamps to row1's only item (index 4)
        assert_eq!(navigate(&groups, 4, 3, NavDirection::Down, true), 4);
        // From index 4 (col 0, last row of G0), Down skips empty G1 -> G2 col 0 (index 5)
        assert_eq!(navigate(&groups, 4, 4, NavDirection::Down, true), 5);
        // From index 2 (col 2, row 0 of G0), Down -> index 4
        assert_eq!(navigate(&groups, 4, 2, NavDirection::Down, true), 4);
        // From index 7 (col 2, G2), Up skips empty G1 -> G0 last row (index 4)
        assert_eq!(navigate(&groups, 4, 7, NavDirection::Up, true), 4);
        // From index 1 (col 1, row 0 of G0), Up wraps to G2 last row col 1 (index 6)
        assert_eq!(navigate(&groups, 4, 1, NavDirection::Up, true), 6);
        // Right wraps from 7 -> 0, Left wraps from 0 -> 7
        assert_eq!(navigate(&groups, 4, 7, NavDirection::Right, true), 0);
        assert_eq!(navigate(&groups, 4, 0, NavDirection::Left, true), 7);
    }

    #[test]
    fn non_cross_desktop_stays_within_group() {
        let groups = [3, 2];
        assert_eq!(navigate(&groups, 4, 2, NavDirection::Right, false), 0);
        assert_eq!(navigate(&groups, 4, 0, NavDirection::Left, false), 2);
        assert_eq!(navigate(&groups, 4, 3, NavDirection::Down, false), 3);
        assert_eq!(navigate(&groups, 4, 4, NavDirection::Up, false), 4);
    }
}
