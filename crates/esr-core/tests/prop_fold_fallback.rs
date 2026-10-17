//! Property test for the fallback Fold_Target of the pure `fold::select`.
//!
//! For fold contexts without a Matching_Tab, `select` returns the
//! Primary_Window when it is neither unresponsive nor closing; otherwise the
//! first window in Activation_Order that is on the desktop, not the
//! Primary_Window, not unresponsive, not closing and not `exclude`; otherwise
//! `NoTarget`. The returned window is always on the candidate's desktop.
//!
//! A context has no Matching_Tab in two ways, both covered here:
//! - no tab of any window (and no pending Folded_Tab) has the candidate's
//!   `fold_key` (the matching tabs of a generated scenario are made unknown);
//! - `exclude` is `Some`, which skips the Matching_Tab search (Req 14.23,
//!   14.24), so even a scenario full of matching tabs yields a fold plan.
//!
//! The expected value comes from a reference written separately from
//! `select`: it ranks with explicit keys, not with `select`'s sort-and-filter
//! steps.
//!
//! **Property 34: Fallback Fold_Target selection**
//!
//! **Validates: Requirements 14.13, 14.24**

mod common;

use common::strategies::*;
use esr_core::engine::{DesktopView, WinKey};
use esr_core::fold::{select, FoldPlan, FoldReason};
use esr_core::model::LocationId;
use proptest::prelude::*;

// Feature: explorer-session-restore, Property 34: Fallback Fold_Target selection

fn on_desktop(view: &DesktopView, win: WinKey) -> bool {
    match view {
        DesktopView::Known(set) => set.contains(&win),
        DesktopView::Unknown => true,
    }
}

/// Position in `order`; windows missing from it rank after every listed one.
fn rank(order: &[WinKey], win: WinKey) -> usize {
    order.iter().position(|w| *w == win).unwrap_or(order.len())
}

/// The window `select` must return, and the reason it must give, for a
/// scenario without a Matching_Tab.
fn reference(s: &FoldScenario) -> (FoldPlan, Option<WinKey>) {
    // Candidate windows: tracked windows on the desktop.
    let mut on_desk: Vec<(usize, &FoldWindowData)> = s
        .windows
        .iter()
        .enumerate()
        .filter(|(_, w)| on_desktop(&s.desktop, w.win))
        .collect();
    // Primary_Window: smallest (Opening_Order rank, list position).
    on_desk.sort_by_key(|(i, w)| (rank(&s.opening, w.win), *i));
    let Some(&(_, primary)) = on_desk.first() else {
        return (FoldPlan::NoTarget, None);
    };
    let unresponsive = s.unresponsive.contains(&primary.win);
    let reason = if unresponsive {
        Some(FoldReason::PrimaryUnresponsive)
    } else if primary.closing {
        Some(FoldReason::PrimaryClosing)
    } else {
        None
    };
    if reason.is_none() && s.exclude != Some(primary.win) {
        return (FoldPlan::Fold { target: primary.win, reason: None }, Some(primary.win));
    }
    // First fallback in Activation_Order (ties by list position).
    let mut fallbacks: Vec<(usize, &FoldWindowData)> = on_desk
        .iter()
        .skip(1)
        .copied()
        .filter(|(_, w)| !w.closing && !s.unresponsive.contains(&w.win) && s.exclude != Some(w.win))
        .collect();
    fallbacks.sort_by_key(|(i, w)| (rank(&s.activation, w.win), *i));
    match fallbacks.first() {
        Some(&(_, w)) => (FoldPlan::Fold { target: w.win, reason }, Some(w.win)),
        None => (FoldPlan::NoTarget, None),
    }
}

/// Makes every tab and pending Folded_Tab with the candidate's `fold_key` an
/// unknown tab (which matches nothing), so that no Matching_Tab exists.
fn without_matching_tabs(mut s: FoldScenario) -> FoldScenario {
    let key = s.candidate.fold_key();
    for w in &mut s.windows {
        for tab in &mut w.tabs {
            if tab.location.as_ref().is_some_and(|l: &LocationId| l.fold_key() == key) {
                tab.location = None;
                tab.deferred = false;
            }
        }
    }
    s.pending_folds.retain(|(_, l)| l.fold_key() != key);
    s
}

fn check(s: &FoldScenario) -> Result<(), TestCaseError> {
    let plan = s.with_ctx(select);
    let (expected, target) = reference(s);
    prop_assert_eq!(plan, expected, "scenario: {:?}", s);
    prop_assert!(!matches!(plan, FoldPlan::Switch { .. }), "no Matching_Tab, yet {:?}", plan);
    if let FoldPlan::Fold { target: got, .. } = plan {
        prop_assert!(on_desktop(&s.desktop, got), "target {:?} is off the desktop", got);
        prop_assert!(s.windows.iter().any(|w| w.win == got), "target {:?} is not a tracked window", got);
        prop_assert_eq!(Some(got), target);
        prop_assert_ne!(Some(got), s.exclude);
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// No tab has the candidate's key: the target follows the fallback rule,
    /// with `foreground` and `exclude` as generated.
    #[test]
    fn fallback_target_without_a_matching_tab(s in fold_scenario().prop_map(without_matching_tabs)) {
        check(&s)?;
    }

    /// `exclude` is set: the search for a Matching_Tab is skipped, whatever
    /// the tabs show, and the target follows the same rule.
    #[test]
    fn fallback_target_when_exclude_is_set(
        s in (fold_scenario(), any::<prop::sample::Index>()).prop_map(|(mut s, pick)| {
            if s.exclude.is_none() {
                let keys = s.keys();
                s.exclude = Some(if keys.is_empty() { fold_win_key(0) } else { keys[pick.index(keys.len())] });
            }
            s
        })
    ) {
        check(&s)?;
    }
}

#[test]
fn unresponsive_primary_gives_first_fallback_in_activation_order() {
    use esr_core::fold::FoldWindow;
    use std::collections::BTreeSet;

    let (a, b, c) = (fold_win_key(0), fold_win_key(1), fold_win_key(2));
    let windows: Vec<FoldWindow<'_>> = [a, b, c]
        .into_iter()
        .map(|win| FoldWindow { win, tabs: vec![None], active: Some(0), closing: false })
        .collect();
    let key = pool_location(0, 0);
    let unresponsive: BTreeSet<WinKey> = [a].into_iter().collect();
    let desktop = DesktopView::Unknown;
    let (opening, activation) = ([a, b, c], [c, b, a]);
    let plan = select(&esr_core::fold::FoldCtx {
        candidate_key: key.fold_key(),
        foreground: None,
        opening: &opening,
        activation: &activation,
        windows: &windows,
        pending_folds: &[],
        desktop: &desktop,
        unresponsive: &unresponsive,
        exclude: None,
    });
    assert_eq!(plan, FoldPlan::Fold { target: c, reason: Some(FoldReason::PrimaryUnresponsive) });
}
