//! `apply_caps`: at most 100 windows and 500 tabs (Activation_Order prefix).
//!
//! Requirement 2.10; design.md, "Open questions" item 6 and Property 6.
//!
//! Windows are walked in Activation_Order (most recent first). A window is
//! kept while fewer than [`MAX_WINDOWS`] are kept and tab budget
//! ([`MAX_TABS_TOTAL`]) remains. A window that only partly fits keeps its
//! leftmost tabs up to the remaining budget; if its Active_Tab is cut, the
//! rightmost kept tab becomes active. Once a window is excluded (window cap
//! reached or no tab budget left), every later window is excluded too, so the
//! kept windows are always an Activation_Order prefix.

use std::collections::BTreeSet;

use crate::logfmt::LogEntry;
use crate::model::{Snapshot, WindowId, MAX_TABS_PER_WINDOW, MAX_TABS_TOTAL, MAX_WINDOWS};

/// What [`apply_caps`] left out.
///
/// `windows` counts excluded windows. `tabs` counts every dropped tab: the
/// tabs of excluded windows plus the tabs cut from the end of a partly kept
/// window. This matches [`LogEntry::CapExcluded`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Excluded {
    pub windows: u32,
    pub tabs: u32,
}

impl Excluded {
    /// True when nothing was excluded.
    pub fn is_empty(&self) -> bool {
        self.windows == 0 && self.tabs == 0
    }

    /// The `CAP-EXCLUDED` entry to log, or `None` when nothing was excluded.
    pub fn log_entry(&self) -> Option<LogEntry> {
        (!self.is_empty()).then_some(LogEntry::CapExcluded {
            windows: self.windows,
            tabs: self.tabs,
        })
    }
}

/// Applies the window and tab caps (Req 2.10).
///
/// The kept snapshot lists the kept windows in their original relative order
/// in `windows`, and in their original relative Activation_Order in
/// `activation`.
///
/// Invalid inputs are handled deterministically:
/// - windows not listed in `activation` rank after all listed windows, in
///   their `windows` order; unknown or repeated `activation` entries are
///   ignored;
/// - windows with no tabs are dropped without counting as excluded (there is
///   nothing to cap) and do not use a window slot;
/// - an out-of-range Active_Tab is clamped to the rightmost kept tab.
pub fn apply_caps(snapshot: Snapshot) -> (Snapshot, Excluded) {
    let Snapshot { windows, activation } = snapshot;

    // Rank window indices by Activation_Order, unlisted windows last.
    let mut assigned = vec![false; windows.len()];
    let mut order: Vec<usize> = Vec::with_capacity(windows.len());
    let mut seen_ids: BTreeSet<WindowId> = BTreeSet::new();
    for id in &activation {
        if !seen_ids.insert(*id) {
            continue;
        }
        if let Some(i) = (0..windows.len()).find(|&i| !assigned[i] && windows[i].id == *id) {
            assigned[i] = true;
            order.push(i);
        }
    }
    order.extend((0..windows.len()).filter(|&i| !assigned[i]));

    // Decide how many leftmost tabs each window keeps (0 = excluded).
    let mut keep_tabs = vec![0usize; windows.len()];
    let mut kept_order: Vec<usize> = Vec::new();
    let mut tab_budget = MAX_TABS_TOTAL;
    let mut excluded = Excluded::default();
    let mut closed = false; // once a window is excluded, all later ones are
    for &i in &order {
        let n = windows[i].tabs.len();
        if n == 0 {
            continue;
        }
        if closed || kept_order.len() >= MAX_WINDOWS || tab_budget == 0 {
            closed = true;
            excluded.windows += 1;
            excluded.tabs += n as u32;
            continue;
        }
        let k = n.min(tab_budget).min(MAX_TABS_PER_WINDOW);
        keep_tabs[i] = k;
        tab_budget -= k;
        excluded.tabs += (n - k) as u32;
        kept_order.push(i);
    }

    let kept_activation: Vec<WindowId> = kept_order.iter().map(|&i| windows[i].id).collect();
    let kept_windows = windows
        .into_iter()
        .zip(keep_tabs)
        .filter(|(_, k)| *k > 0)
        .map(|(mut w, k)| {
            w.tabs.truncate(k);
            if w.active >= k {
                w.active = k - 1;
            }
            w
        })
        .collect();

    (Snapshot { windows: kept_windows, activation: kept_activation }, excluded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        validate, DisplayRef, LocationId, LocationKind, Placement, Rect, ShowState, TabRec,
        Volume, WindowRec,
    };

    fn tab(i: usize) -> TabRec {
        let path = format!(r"C:\t{i}");
        TabRec {
            location: LocationId {
                case_sensitive: false,
                kind: LocationKind::FileSystem(Volume::Fixed),
                parsing_name: path.as_str().into(),
                display_name: path.as_str().into(),
                pidl: vec![],
                search: None,
            },
        }
    }

    fn window(id: u32, tabs: usize, active: usize) -> WindowRec {
        let r = Rect { left: 0, top: 0, right: 800, bottom: 600 };
        WindowRec {
            id: WindowId(id),
            tabs: (0..tabs).map(tab).collect(),
            active,
            placement: Placement {
                state: ShowState::Normal,
                restored: r,
                display: DisplayRef { device: r"\\.\DISPLAY1".into(), monitor: r },
            },
        }
    }

    fn snap(windows: Vec<WindowRec>, activation: &[u32]) -> Snapshot {
        Snapshot { windows, activation: activation.iter().map(|&i| WindowId(i)).collect() }
    }

    fn ids(s: &Snapshot) -> Vec<u32> {
        s.windows.iter().map(|w| w.id.0).collect()
    }

    fn act(s: &Snapshot) -> Vec<u32> {
        s.activation.iter().map(|w| w.0).collect()
    }

    #[test]
    fn empty_snapshot() {
        let (s, ex) = apply_caps(Snapshot::default());
        assert_eq!(s, Snapshot::default());
        assert!(ex.is_empty());
        assert_eq!(ex.log_entry(), None);
    }

    #[test]
    fn under_caps_is_identity() {
        let input = snap(vec![window(1, 3, 2), window(2, 1, 0), window(3, 4, 1)], &[3, 1, 2]);
        let (s, ex) = apply_caps(input.clone());
        assert_eq!(s, input);
        assert_eq!(ex, Excluded::default());
        assert_eq!(validate(&s), Ok(()));
    }

    #[test]
    fn exactly_at_caps_is_identity() {
        // 100 windows, 500 tabs.
        let windows: Vec<_> = (0..100).map(|i| window(i, 5, 4)).collect();
        let activation: Vec<u32> = (0..100).rev().collect();
        let input = snap(windows, &activation);
        let (s, ex) = apply_caps(input.clone());
        assert_eq!(s, input);
        assert!(ex.is_empty());
        assert_eq!(validate(&s), Ok(()));

        // One window holding all 500 tabs.
        let input = snap(vec![window(9, 500, 499)], &[9]);
        let (s, ex) = apply_caps(input.clone());
        assert_eq!(s, input);
        assert!(ex.is_empty());
    }

    #[test]
    fn over_window_cap_keeps_activation_prefix() {
        // 103 windows of 1 tab; activation is the reverse of `windows` order.
        let windows: Vec<_> = (0..103).map(|i| window(i, 1, 0)).collect();
        let activation: Vec<u32> = (0..103).rev().collect();
        let (s, ex) = apply_caps(snap(windows, &activation));
        assert_eq!(ex, Excluded { windows: 3, tabs: 3 });
        assert_eq!(ex.log_entry(), Some(LogEntry::CapExcluded { windows: 3, tabs: 3 }));
        // The three least recently activated (ids 0..3) are dropped.
        assert_eq!(act(&s), (3..103).rev().collect::<Vec<_>>());
        // `windows` keeps its original relative order.
        assert_eq!(ids(&s), (3..103).collect::<Vec<_>>());
        assert_eq!(validate(&s), Ok(()));
    }

    #[test]
    fn over_tab_cap_cuts_window_and_excludes_rest() {
        // Activation: 2 (300 tabs), 1 (250 tabs), 3 (10 tabs).
        let input = snap(vec![window(1, 250, 10), window(2, 300, 0), window(3, 10, 0)], &[2, 1, 3]);
        let (s, ex) = apply_caps(input);
        // Window 1 keeps its leftmost 200 tabs; window 3 has no budget left.
        assert_eq!(ex, Excluded { windows: 1, tabs: 50 + 10 });
        assert_eq!(ids(&s), vec![1, 2]);
        assert_eq!(act(&s), vec![2, 1]);
        let w1 = s.window(WindowId(1)).unwrap();
        assert_eq!(w1.tabs, (0..200).map(tab).collect::<Vec<_>>());
        assert_eq!(w1.active, 10, "active tab within the kept prefix is unchanged");
        assert_eq!(s.window(WindowId(2)).unwrap().tabs.len(), 300);
        assert_eq!(s.tab_count(), 500);
        assert_eq!(validate(&s), Ok(()));
    }

    #[test]
    fn exclusion_is_a_prefix_even_if_a_later_window_would_fit() {
        // Window cap reached after 100; the tab cap is not, but window 101 is
        // still excluded, and nothing after it is kept.
        let mut windows: Vec<_> = (0..100).map(|i| window(i, 1, 0)).collect();
        windows.push(window(100, 1, 0));
        let activation: Vec<u32> = (0..101).collect();
        let (s, ex) = apply_caps(snap(windows, &activation));
        assert_eq!(ex, Excluded { windows: 1, tabs: 1 });
        assert_eq!(act(&s), (0..100).collect::<Vec<_>>());
    }

    #[test]
    fn cut_active_tab_moves_to_rightmost_kept() {
        let input = snap(vec![window(1, 495, 0), window(2, 20, 15)], &[1, 2]);
        let (s, ex) = apply_caps(input);
        assert_eq!(ex, Excluded { windows: 0, tabs: 15 });
        let w2 = s.window(WindowId(2)).unwrap();
        assert_eq!(w2.tabs.len(), 5);
        assert_eq!(w2.active, 4);
        assert_eq!(validate(&s), Ok(()));
    }

    #[test]
    fn unlisted_windows_rank_last() {
        // Window 5 is missing from `activation`; window 9 is unknown.
        let input = snap(vec![window(5, 1, 0), window(1, 499, 0), window(2, 1, 0)], &[9, 1, 2, 1]);
        let (s, ex) = apply_caps(input);
        assert_eq!(ex, Excluded { windows: 1, tabs: 1 });
        assert_eq!(ids(&s), vec![1, 2]);
        assert_eq!(act(&s), vec![1, 2]);
        assert_eq!(validate(&s), Ok(()));
    }
}
