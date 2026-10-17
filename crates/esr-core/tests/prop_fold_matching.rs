//! Property test for the Matching_Tab selection of `fold::select`.
//!
//! **Property 35: Matching_Tab selection**
//!
//! **Validates: Requirements 14.20, 14.21**

mod common;

use common::strategies::*;
use esr_core::engine::{DesktopView, WinKey};
use esr_core::fold::{select, FoldPlan};
use proptest::prelude::*;

/// Position of `win` in `order`; windows missing from it rank last.
fn rank(order: &[WinKey], win: WinKey) -> usize {
    order.iter().position(|&w| w == win).unwrap_or(usize::MAX)
}

/// The tab of `w` that matches the candidate: the Active_Tab when it matches,
/// otherwise the leftmost matching tab, otherwise the first matching pending
/// Folded_Tab (counted after the real tabs).
fn matching_tab(sc: &FoldScenario, w: &FoldWindowData) -> Option<usize> {
    let key = sc.candidate.fold_key();
    let matches = |i: usize| w.tabs[i].location.as_ref().is_some_and(|l| l.fold_key() == key);
    if let Some(a) = w.active.filter(|&a| a < w.tabs.len() && matches(a)) {
        return Some(a);
    }
    if let Some(i) = (0..w.tabs.len()).find(|&i| matches(i)) {
        return Some(i);
    }
    sc.pending_folds
        .iter()
        .filter(|(target, _)| *target == w.win)
        .position(|(_, l)| l.fold_key() == key)
        .map(|n| w.tabs.len() + n)
}

/// The reference search: the Primary_Window first (unless unresponsive or
/// closing), then the other usable windows in Activation_Order.
fn reference(sc: &FoldScenario) -> Option<(WinKey, usize)> {
    let on_desktop = |win: WinKey| match &sc.desktop {
        DesktopView::Known(set) => set.contains(&win),
        DesktopView::Unknown => true,
    };
    let mut eligible: Vec<&FoldWindowData> = sc.windows.iter().filter(|w| on_desktop(w.win)).collect();
    eligible.sort_by_key(|w| rank(&sc.opening, w.win));
    let primary = *eligible.first()?;
    let usable = |w: &FoldWindowData| !w.closing && !sc.unresponsive.contains(&w.win);

    let mut order: Vec<&FoldWindowData> = Vec::new();
    if usable(primary) {
        order.push(primary);
    }
    let mut fallbacks: Vec<&FoldWindowData> =
        eligible.iter().copied().filter(|w| w.win != primary.win && usable(w)).collect();
    fallbacks.sort_by_key(|w| rank(&sc.activation, w.win));
    order.extend(fallbacks);

    order.into_iter().find_map(|w| matching_tab(sc, w).map(|i| (w.win, i)))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// With no Foreground_Explorer_Window and no `exclude`, `select` gives
    /// `Switch` to the reference Matching_Tab exactly when there is one.
    #[test]
    fn switch_exactly_when_the_reference_search_finds_a_tab(mut sc in fold_scenario()) {
        sc.foreground = None;
        sc.exclude = None;
        let plan = sc.with_ctx(select);
        match reference(&sc) {
            Some((win, index)) => prop_assert_eq!(plan, FoldPlan::Switch { win, index }),
            None => prop_assert!(
                !matches!(plan, FoldPlan::Switch { .. }),
                "no Matching_Tab, yet {:?}",
                plan
            ),
        }
    }
}

#[test]
fn a_matching_pending_folded_tab_counts_after_the_real_tabs() {
    // Window 0 shows pool entry 1 only; an earlier candidate folds entry 2
    // into it. A candidate for entry 2 switches to index 1 of window 0.
    let win = fold_win_key(0);
    let sc = FoldScenario {
        candidate: pool_location(2, 0),
        foreground: None,
        opening: vec![win],
        activation: vec![win],
        windows: vec![FoldWindowData {
            win,
            tabs: vec![FoldTabData { location: Some(pool_location(1, 0)), deferred: false }],
            active: Some(0),
            closing: false,
        }],
        pending_folds: vec![(win, pool_location(2, 1))],
        desktop: DesktopView::Unknown,
        unresponsive: Default::default(),
        exclude: None,
    };
    assert_eq!(sc.with_ctx(select), FoldPlan::Switch { win, index: 1 });
}
