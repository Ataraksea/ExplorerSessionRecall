//! `TabOrderReconciler::reconcile` (UIA name order onto tab keys).
//!
//! A window rescan yields the tabs in ShellWindows enumeration order, which is
//! not display order, plus (when UI Automation works) the tab strip's
//! `TabItem` names left to right. The reconciler combines both with the
//! window's previous tab order (design.md, "Window rescan"):
//!
//! - A UIA name matches a tab when it equals one of the tab's *match names*:
//!   its display name or its parsing name. UIA names are display names on
//!   build 22621 but full paths on 26200.
//! - With unique match names the UIA order is authoritative.
//! - Tabs sharing a name keep their previous relative order, and newly seen
//!   tabs with an ambiguous name go after previously known tabs of that name.
//! - An overflowing strip exposes only some items. Tabs with no exposed item
//!   keep their previous relative order, each staying after the tab that
//!   preceded it before; new tabs with no exposed item are appended.
//! - Without UIA the previous order is kept and new tabs are appended.
//! - The output is always a permutation of the scanned tab set.
//!
//! Requirements 2.1, 2.2, 2.4.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::engine::{TabKey, TabStripState};
use crate::model::{LocationId, WideString};

/// One tab found by a rescan, with the names a UIA `TabItem` may carry for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScannedTab {
    pub tab: TabKey,
    /// Display name and/or parsing name. A tab with no known Location yet
    /// (e.g. "Home" before its first navigation) may only have a display name;
    /// a tab with no names is never matched to a UIA item.
    pub match_names: Vec<WideString>,
}

impl ScannedTab {
    pub fn new(tab: TabKey, match_names: Vec<WideString>) -> Self {
        Self { tab, match_names }
    }

    /// Match names taken from a Location: display name and parsing name.
    pub fn from_location(tab: TabKey, location: &LocationId) -> Self {
        let mut names = vec![location.display_name.clone()];
        if location.parsing_name != location.display_name {
            names.push(location.parsing_name.clone());
        }
        Self {
            tab,
            match_names: names,
        }
    }
}

/// Result of [`TabOrderReconciler::reconcile`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reconciled {
    /// The scanned tabs in left-to-right order (a permutation of the scanned set).
    pub order: Vec<TabKey>,
    /// For each UIA `TabItem` (same indices as `TabStripState::names`), the tab
    /// it was matched to; `None` for items matching no tab. Empty without UIA.
    pub uia_tabs: Vec<Option<TabKey>>,
    /// The tab of the selected UIA item, if UIA reported a selection that
    /// matched a tab.
    pub selected: Option<TabKey>,
}

/// Pure mapping of UIA tab-strip order onto tab keys.
#[derive(Clone, Copy, Debug, Default)]
pub struct TabOrderReconciler;

impl TabOrderReconciler {
    /// Orders the `scanned` tabs of one window.
    ///
    /// `prev` is the window's previous left-to-right order (keys not scanned
    /// are ignored); `scanned` is in enumeration order (duplicate keys count
    /// once); `strip` is the UIA tab strip, or `None` when UIA was unavailable.
    pub fn reconcile(
        prev: &[TabKey],
        scanned: &[ScannedTab],
        strip: Option<&TabStripState>,
    ) -> Reconciled {
        // Base order: previously known tabs in their previous order, then new
        // tabs in enumeration order. Indices below refer to `base`.
        let mut by_key: HashMap<TabKey, &ScannedTab> = HashMap::new();
        for s in scanned {
            by_key.entry(s.tab).or_insert(s);
        }
        let mut seen: HashSet<TabKey> = HashSet::new();
        let mut base: Vec<&ScannedTab> = Vec::with_capacity(by_key.len());
        for key in prev {
            if let Some(s) = by_key.get(key)
                && seen.insert(*key)
            {
                base.push(s);
            }
        }
        let known = base.len();
        for s in scanned {
            if seen.insert(s.tab) {
                base.push(s);
            }
        }

        let Some(strip) = strip else {
            return Reconciled {
                order: base.iter().map(|s| s.tab).collect(),
                uia_tabs: Vec::new(),
                selected: None,
            };
        };

        let tab_of_item = match_items(&base, &strip.names);

        // Matched tabs in UIA order.
        let matched: Vec<usize> = tab_of_item.iter().flatten().copied().collect();
        let mut pos_in_matched: Vec<Option<usize>> = vec![None; base.len()];
        for (p, &t) in matched.iter().enumerate() {
            pos_in_matched[t] = Some(p);
        }

        // Unmatched tabs in base order, each with the number of matched tabs
        // it must follow. A known tab follows every matched tab that precedes
        // it in base order (so it stays after its previous predecessor, and
        // same-name tabs keep their relative order); a new tab is appended.
        // The running maximum keeps the unmatched tabs in their base relative
        // order.
        let mut unmatched: Vec<(usize, usize)> = Vec::new();
        let mut anchor = 0;
        let mut floor = 0;
        for (t, &pos) in pos_in_matched.iter().enumerate() {
            match pos {
                Some(p) => anchor = anchor.max(p + 1),
                None => {
                    let after = if t < known { anchor } else { matched.len() };
                    floor = floor.max(after);
                    unmatched.push((floor, t));
                }
            }
        }

        // Merge both sequences.
        let mut order = Vec::with_capacity(base.len());
        let mut u = unmatched.iter().peekable();
        for p in 0..=matched.len() {
            while let Some(&&(after, t)) = u.peek() {
                if after != p {
                    break;
                }
                order.push(base[t].tab);
                u.next();
            }
            if let Some(&t) = matched.get(p) {
                order.push(base[t].tab);
            }
        }

        let uia_tabs: Vec<Option<TabKey>> =
            tab_of_item.iter().map(|t| t.map(|t| base[t].tab)).collect();
        let selected = strip
            .selected
            .and_then(|i| uia_tabs.get(i).copied().flatten());
        Reconciled {
            order,
            uia_tabs,
            selected,
        }
    }
}

/// Assigns UIA items to base tabs, each tab to at most one item.
///
/// Items with a single remaining candidate are assigned first (unit
/// propagation). Otherwise the leftmost ambiguous item takes its first
/// unassigned candidate in base order, so tabs sharing a name keep their base
/// relative order and new tabs go after known ones. Empty names never match.
fn match_items(base: &[&ScannedTab], names: &[WideString]) -> Vec<Option<usize>> {
    let mut tabs_by_name: HashMap<&WideString, Vec<usize>> = HashMap::new();
    for (t, s) in base.iter().enumerate() {
        for name in s.match_names.iter().filter(|n| !n.is_empty()) {
            // Tabs are visited in ascending order, so a repeat is always last.
            let list = tabs_by_name.entry(name).or_default();
            if list.last() != Some(&t) {
                list.push(t);
            }
        }
    }
    // Candidate tabs of each item, ascending base index.
    let cands: Vec<&[usize]> = names
        .iter()
        .map(|n| tabs_by_name.get(n).map_or(&[][..], |v| v.as_slice()))
        .collect();
    let mut items_of_tab: Vec<Vec<usize>> = vec![Vec::new(); base.len()];
    for (i, c) in cands.iter().enumerate() {
        for &t in c.iter() {
            items_of_tab[t].push(i);
        }
    }

    let mut m = Matching {
        tab_of_item: vec![None; names.len()],
        tab_taken: vec![false; base.len()],
        // Unassigned candidate tabs per item.
        live: cands.iter().map(|c| c.len()).collect(),
        units: VecDeque::new(),
    };
    m.units.extend((0..names.len()).filter(|&i| m.live[i] == 1));

    let mut next = 0;
    loop {
        while let Some(i) = m.units.pop_front() {
            if m.tab_of_item[i].is_none() && m.live[i] > 0 {
                m.assign_first_free(i, cands[i], &items_of_tab);
            }
        }
        // `live` only decreases, and an unassigned item with one candidate
        // left was just handled, so skipped items never become eligible again.
        while next < names.len() && (m.tab_of_item[next].is_some() || m.live[next] < 2) {
            next += 1;
        }
        if next == names.len() {
            break;
        }
        m.assign_first_free(next, cands[next], &items_of_tab);
    }
    m.tab_of_item
}

/// Working state of [`match_items`].
struct Matching {
    tab_of_item: Vec<Option<usize>>,
    tab_taken: Vec<bool>,
    live: Vec<usize>,
    units: VecDeque<usize>,
}

impl Matching {
    /// Assigns item `i` to its first untaken candidate (in base order) and
    /// queues items left with a single candidate.
    fn assign_first_free(&mut self, i: usize, cands: &[usize], items_of_tab: &[Vec<usize>]) {
        let Some(t) = cands.iter().copied().find(|&t| !self.tab_taken[t]) else {
            return;
        };
        self.tab_of_item[i] = Some(t);
        self.tab_taken[t] = true;
        for &j in &items_of_tab[t] {
            self.live[j] -= 1;
            if self.live[j] == 1 && self.tab_of_item[j].is_none() {
                self.units.push_back(j);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(n: u64) -> TabKey {
        TabKey {
            hwnd: 1000 + n,
            seq: n,
        }
    }

    fn w(s: &str) -> WideString {
        s.into()
    }

    /// A tab whose display name is `name` and parsing name `C:\<name>`.
    fn tab(n: u64, name: &str) -> ScannedTab {
        ScannedTab::new(k(n), vec![w(name), w(&format!(r"C:\{name}"))])
    }

    fn strip(names: &[&str], selected: Option<usize>) -> TabStripState {
        TabStripState {
            names: names.iter().map(|n| w(n)).collect(),
            selected,
        }
    }

    fn keys(ns: &[u64]) -> Vec<TabKey> {
        ns.iter().map(|&n| k(n)).collect()
    }

    #[test]
    fn unique_names_follow_uia_order() {
        let scanned = [tab(1, "A"), tab(2, "B"), tab(3, "C")];
        let s = strip(&["C", "A", "B"], Some(1));
        let r = TabOrderReconciler::reconcile(&keys(&[1, 2, 3]), &scanned, Some(&s));
        assert_eq!(r.order, keys(&[3, 1, 2]));
        assert_eq!(r.uia_tabs, vec![Some(k(3)), Some(k(1)), Some(k(2))]);
        assert_eq!(r.selected, Some(k(1)));
    }

    #[test]
    fn duplicate_names_keep_previous_relative_order() {
        // Enumeration order differs from previous order on purpose.
        let scanned = [tab(2, "Docs"), tab(3, "X"), tab(1, "Docs")];
        let s = strip(&["Docs", "X", "Docs"], Some(2));
        let r = TabOrderReconciler::reconcile(&keys(&[1, 2, 3]), &scanned, Some(&s));
        assert_eq!(r.order, keys(&[1, 3, 2]));
        assert_eq!(r.selected, Some(k(2)));
    }

    #[test]
    fn new_duplicate_goes_after_known_ones() {
        let scanned = [tab(9, "Docs"), tab(1, "Docs"), tab(2, "Docs")];
        let s = strip(&["Docs", "Docs", "Docs"], Some(0));
        let r = TabOrderReconciler::reconcile(&keys(&[2, 1]), &scanned, Some(&s));
        assert_eq!(r.order, keys(&[2, 1, 9]));
        assert_eq!(r.selected, Some(k(2)));
    }

    #[test]
    fn without_uia_keeps_previous_order_and_appends() {
        let scanned = [tab(5, "N1"), tab(3, "C"), tab(6, "N2"), tab(1, "A")];
        // Tab 2 was closed; 5 and 6 are new.
        let r = TabOrderReconciler::reconcile(&keys(&[3, 2, 1]), &scanned, None);
        assert_eq!(r.order, keys(&[3, 1, 5, 6]));
        assert!(r.uia_tabs.is_empty());
        assert_eq!(r.selected, None);
    }

    #[test]
    fn partial_exposure_keeps_unexposed_tabs_in_place() {
        let scanned: Vec<ScannedTab> = ["A", "B", "C", "D", "E"]
            .iter()
            .zip(1..)
            .map(|(n, i)| tab(i, n))
            .collect();
        // Only B, D, and E are exposed, and D was moved before B.
        let s = strip(&["D", "B", "E"], None);
        let r = TabOrderReconciler::reconcile(&keys(&[1, 2, 3, 4, 5]), &scanned, Some(&s));
        // A precedes everything matched; C stays after B, its previous predecessor.
        assert_eq!(r.order, keys(&[1, 4, 2, 3, 5]));
        assert_eq!(r.selected, None);
    }

    #[test]
    fn partial_exposure_appends_unexposed_new_tabs() {
        let scanned = [tab(1, "A"), tab(2, "B"), tab(7, "New")];
        let s = strip(&["B", "A"], Some(0));
        let r = TabOrderReconciler::reconcile(&keys(&[1, 2]), &scanned, Some(&s));
        assert_eq!(r.order, keys(&[2, 1, 7]));
        assert_eq!(r.selected, Some(k(2)));
    }

    #[test]
    fn matches_parsing_names_and_display_names() {
        // 26200-style full paths.
        let scanned = [tab(1, "A"), tab(2, "B")];
        let s = strip(&[r"C:\B", r"C:\A"], Some(1));
        let r = TabOrderReconciler::reconcile(&keys(&[1, 2]), &scanned, Some(&s));
        assert_eq!(r.order, keys(&[2, 1]));
        assert_eq!(r.selected, Some(k(1)));

        // A not-yet-navigated tab with only a display name, plus a
        // from_location tab matched by display name (22621 style).
        let loc = LocationId {
            case_sensitive: false,
            kind: crate::model::LocationKind::FileSystem(crate::model::Volume::Fixed),
            parsing_name: w(r"D:\Work"),
            display_name: w("Work"),
            pidl: vec![],
            search: None,
        };
        let scanned = [
            ScannedTab::from_location(k(1), &loc),
            ScannedTab::new(k(2), vec![w("Home")]),
        ];
        let s = strip(&["Home", "Work"], Some(0));
        let r = TabOrderReconciler::reconcile(&keys(&[1, 2]), &scanned, Some(&s));
        assert_eq!(r.order, keys(&[2, 1]));
        assert_eq!(r.selected, Some(k(2)));
    }

    #[test]
    fn unknown_uia_names_are_ignored() {
        let scanned = [tab(1, "A"), tab(2, "B")];
        let s = strip(&["Ghost", "B", "", "A", "B"], Some(0));
        let r = TabOrderReconciler::reconcile(&keys(&[1, 2]), &scanned, Some(&s));
        assert_eq!(r.order, keys(&[2, 1]));
        assert_eq!(r.uia_tabs, vec![None, Some(k(2)), None, Some(k(1)), None]);
        // The selected item matches no tab.
        assert_eq!(r.selected, None);
    }

    #[test]
    fn single_candidate_items_are_matched_first() {
        // Tab 1 is reachable by "Docs" and "C:\Docs"; tab 2 only by "Docs".
        let scanned = [
            ScannedTab::new(k(1), vec![w("Docs"), w(r"C:\Docs")]),
            ScannedTab::new(k(2), vec![w("Docs")]),
        ];
        let s = strip(&["Docs", r"C:\Docs"], None);
        let r = TabOrderReconciler::reconcile(&keys(&[1, 2]), &scanned, Some(&s));
        assert_eq!(r.uia_tabs, vec![Some(k(2)), Some(k(1))]);
        assert_eq!(r.order, keys(&[2, 1]));
    }

    #[test]
    fn empty_input() {
        let r = TabOrderReconciler::reconcile(&[], &[], None);
        assert_eq!(r, Reconciled::default());
        let s = strip(&["A"], Some(0));
        let r = TabOrderReconciler::reconcile(&keys(&[1]), &[], Some(&s));
        assert!(r.order.is_empty());
        assert_eq!(r.uia_tabs, vec![None]);
        assert_eq!(r.selected, None);
        let r = TabOrderReconciler::reconcile(&[], &[tab(1, "A")], Some(&strip(&[], None)));
        assert_eq!(r.order, keys(&[1]));
    }

    #[test]
    fn duplicate_scanned_and_previous_keys_count_once() {
        let scanned = [tab(1, "A"), tab(2, "B"), tab(1, "A")];
        let r = TabOrderReconciler::reconcile(&keys(&[2, 2, 1]), &scanned, None);
        assert_eq!(r.order, keys(&[2, 1]));
    }
}
