//! `active_after_omission` (Requirements 5.4, 9.4).
//!
//! When a window is restored, some of its saved tabs may be omitted because
//! their Location is unavailable and no ancestor qualifies (Requirement 9).
//! Tabs that are restored or redirected to an ancestor are *kept*. This
//! module decides which kept tab becomes the window's Active_Tab.
//!
//! The Targeted_Launch case (the requested tab is the active tab of the
//! Trigger_Window, Requirement 6) is handled by the planner, which overrides
//! the result of this function for the Trigger_Window. This function only
//! knows about the saved tabs.

/// Returns the index, within the kept subsequence, of the tab that should be
/// active after omissions.
///
/// * `kept[i]` is `true` when saved tab `i` is restored or redirected, and
///   `false` when it is omitted.
/// * `saved_active` is the saved Active_Tab index into `kept`.
///
/// Rules:
/// 1. If the saved Active_Tab is kept, its position among the kept tabs.
/// 2. Otherwise the nearest kept tab to the left of `saved_active`.
/// 3. If none remains to the left, the nearest kept tab to the right.
/// 4. If no tab is kept, `None`.
///
/// An out-of-range `saved_active` (`>= kept.len()`) is clamped to the last
/// saved tab, so it resolves to the rightmost kept tab. This only matters for
/// defensive handling; valid snapshots always have `saved_active < len`.
pub fn active_after_omission(kept: &[bool], saved_active: usize) -> Option<usize> {
    if kept.is_empty() {
        return None;
    }
    let a = saved_active.min(kept.len() - 1);

    // Number of kept tabs strictly before position `i` = new index of tab `i`.
    let new_index = |i: usize| kept[..i].iter().filter(|&&k| k).count();

    if kept[a] {
        return Some(new_index(a));
    }
    if let Some(left) = (0..a).rev().find(|&i| kept[i]) {
        return Some(new_index(left));
    }
    // No kept tab to the left, so the nearest kept tab to the right is the
    // first kept tab overall: new index 0.
    (a + 1..kept.len()).find(|&i| kept[i]).map(|_| 0)
}

#[cfg(test)]
mod tests {
    use super::active_after_omission;

    #[test]
    fn active_kept_returns_its_new_position() {
        assert_eq!(active_after_omission(&[true, true, true], 1), Some(1));
        assert_eq!(active_after_omission(&[false, true, true], 2), Some(1));
        assert_eq!(active_after_omission(&[true, false, false, true], 3), Some(1));
    }

    #[test]
    fn active_omitted_prefers_nearest_left() {
        // Saved tabs: 0 kept, 1 kept, 2 omitted (active), 3 kept.
        assert_eq!(active_after_omission(&[true, true, false, true], 2), Some(1));
        // Nearest left skips over omitted tabs.
        assert_eq!(active_after_omission(&[true, false, false, true], 2), Some(0));
    }

    #[test]
    fn active_omitted_falls_back_to_nearest_right() {
        assert_eq!(active_after_omission(&[false, true, true], 0), Some(0));
        assert_eq!(active_after_omission(&[false, false, false, true], 1), Some(0));
    }

    #[test]
    fn nothing_kept_returns_none() {
        assert_eq!(active_after_omission(&[], 0), None);
        assert_eq!(active_after_omission(&[false], 0), None);
        assert_eq!(active_after_omission(&[false, false, false], 1), None);
    }

    #[test]
    fn out_of_range_active_is_clamped_to_last_tab() {
        assert_eq!(active_after_omission(&[true, true, true], 7), Some(2));
        assert_eq!(active_after_omission(&[true, true, false], 7), Some(1));
        assert_eq!(active_after_omission(&[false, false], 7), None);
    }

    #[test]
    fn single_tab() {
        assert_eq!(active_after_omission(&[true], 0), Some(0));
    }
}
