// Feature: explorer-session-restore, Property 13: Plan structure under unavailable locations
//! Property test for the structure of `esr_core::plan::plan_restore`.
//!
//! **Property 13: Plan structure under unavailable locations**
//!
//! *For any* snapshot, availability map (Available, Redirected(ancestor), or
//! Omitted(reason) per tab), and launch kind, `plan_restore` returns:
//! - one planned window per saved window that keeps at least one tab, in
//!   saved Activation_Order, with the Trigger_Window mapped to the first of
//!   them;
//! - each window's tabs as the order-preserving subsequence of its
//!   non-omitted tabs;
//! - each tab showing its saved Location or its chosen ancestor, with
//!   redirected duplicates kept;
//! - if every window is omitted, an empty plan that leaves the trigger with
//!   its initial tab.
//!
//! **Validates: Requirements 5.1, 5.2, 5.3, 9.1, 9.2, 9.5, 9.6**
//!
//! Every tab is resolved (no pending probes). Redirect targets are drawn
//! from a small per-case pool so that several tabs often redirect to the same
//! ancestor (duplicates must be kept, Req 9.1). Windows are often fully
//! omitted, and whole cases often omit everything, to exercise Req 9.5/9.6.
//! `BuiltinRestore` launches plan nothing (Req 10.4).

mod common;

use common::strategies::{displays_and_snapshot, location_id, plain_location_id};
use esr_core::model::*;
use esr_core::plan::fallback::{Fallback, OmitReason};
use esr_core::plan::{plan_restore, Availability, LaunchInfo, Slot, TabResolution};
use proptest::collection::vec;
use proptest::prelude::*;
use proptest::sample::select;

const ALL_OMIT_REASONS: [OmitReason; 5] = [
    OmitReason::NotExist,
    OmitReason::Inaccessible,
    OmitReason::Timeout,
    OmitReason::Unresolvable,
    OmitReason::ShellUnavailable,
];

/// Test-side fate of one tab; `Redirect(i)` names an entry of the ancestor pool.
#[derive(Clone, Debug)]
enum Fate {
    Keep,
    Redirect(usize),
    Omit(OmitReason),
}

fn omit() -> impl Strategy<Value = Fate> {
    select(ALL_OMIT_REASONS.to_vec()).prop_map(Fate::Omit)
}

fn not_omitted(pool: usize) -> impl Strategy<Value = Fate> {
    prop_oneof![2 => Just(Fate::Keep), 1 => (0..pool).prop_map(Fate::Redirect)]
}

fn any_fate(pool: usize) -> impl Strategy<Value = Fate> {
    prop_oneof![3 => not_omitted(pool), 2 => omit()]
}

/// Fates for one window of `n` tabs: all omitted, none omitted, or mixed.
fn window_fates(n: usize, pool: usize) -> BoxedStrategy<Vec<Fate>> {
    prop_oneof![
        1 => vec(omit(), n),
        1 => vec(not_omitted(pool), n),
        3 => vec(any_fate(pool), n),
    ]
    .boxed()
}

#[derive(Clone, Debug)]
struct Case {
    displays: Vec<Display>,
    snapshot: Snapshot,
    /// Redirect targets, shared by all tabs of the case.
    pool: Vec<LocationId>,
    /// Parallel to `snapshot.windows[i].tabs`.
    fates: Vec<Vec<Fate>>,
    launch: LaunchInfo,
}

fn launch(snapshot: &Snapshot) -> BoxedStrategy<LaunchInfo> {
    let saved: Vec<LocationId> =
        snapshot.windows.iter().flat_map(|w| w.tabs.iter().map(|t| t.location.clone())).collect();
    let requested = if saved.is_empty() {
        location_id()
    } else {
        // Sometimes the requested Location equals a saved one (Req 6.5).
        prop_oneof![3 => location_id(), 1 => select(saved).boxed()].boxed()
    };
    prop_oneof![
        4 => Just(LaunchInfo::Default),
        4 => requested.prop_map(|requested| LaunchInfo::Targeted { requested }),
        1 => Just(LaunchInfo::BuiltinRestore),
    ]
    .boxed()
}

fn case() -> impl Strategy<Value = Case> {
    (displays_and_snapshot(), vec(plain_location_id(), 1..=3)).prop_flat_map(|((displays, snapshot), pool)| {
        let n = pool.len();
        let counts: Vec<usize> = snapshot.windows.iter().map(|w| w.tabs.len()).collect();
        let all_omitted = counts.iter().map(|&c| vec(omit(), c).boxed()).collect::<Vec<_>>();
        let per_window = counts.iter().map(|&c| window_fates(c, n)).collect::<Vec<_>>();
        let fates = prop_oneof![1 => all_omitted, 4 => per_window];
        (Just(displays), Just(snapshot.clone()), Just(pool), fates, launch(&snapshot)).prop_map(
            |(displays, snapshot, pool, fates, launch)| Case { displays, snapshot, pool, fates, launch },
        )
    })
}

fn fallback_of(fate: &Fate, pool: &[LocationId]) -> Fallback {
    match fate {
        Fate::Keep => Fallback::Keep,
        Fate::Redirect(i) => Fallback::Redirected(pool[*i].clone()),
        Fate::Omit(r) => Fallback::Omitted(*r),
    }
}

/// Expected planned window, computed independently of the planner.
struct Expected {
    source: WindowId,
    /// `(saved tab index, Location shown)` of every non-omitted tab, in order.
    tabs: Vec<(usize, LocationId)>,
}

fn expected(c: &Case) -> Vec<Expected> {
    if matches!(c.launch, LaunchInfo::BuiltinRestore) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for id in &c.snapshot.activation {
        let wi = c.snapshot.windows.iter().position(|w| w.id == *id).expect("valid snapshot");
        let w = &c.snapshot.windows[wi];
        let tabs: Vec<(usize, LocationId)> = w
            .tabs
            .iter()
            .zip(&c.fates[wi])
            .enumerate()
            .filter_map(|(i, (t, fate))| match fate {
                Fate::Keep => Some((i, t.location.clone())),
                Fate::Redirect(p) => Some((i, c.pool[*p].clone())),
                Fate::Omit(_) => None,
            })
            .collect();
        if !tabs.is_empty() {
            out.push(Expected { source: *id, tabs });
        }
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Property 13: plan structure under unavailable locations.
    #[test]
    fn plan_structure_under_unavailable_locations(c in case()) {
        let index_of: std::collections::HashMap<WindowId, usize> =
            c.snapshot.windows.iter().enumerate().map(|(i, w)| (w.id, i)).collect();
        let availability = Availability::from_fn(&c.snapshot, |t, _| {
            TabResolution::Resolved(fallback_of(&c.fates[index_of[&t.window]][t.tab], &c.pool))
        });
        let plan = plan_restore(&c.snapshot, &c.launch, &availability, &c.displays);
        let want = expected(&c);
        let targeted = matches!(c.launch, LaunchInfo::Targeted { .. });

        // One planned window per saved window keeping a tab, in saved
        // Activation_Order (Req 5.1, 9.5).
        let got_sources: Vec<WindowId> = plan.windows.iter().map(|w| w.source).collect();
        let want_sources: Vec<WindowId> = want.iter().map(|e| e.source).collect();
        prop_assert_eq!(&got_sources, &want_sources);

        if want.is_empty() {
            // Every window omitted (or Builtin_Restore): empty plan, the
            // trigger is left as it opened (Req 9.6).
            prop_assert!(plan.is_empty());
            prop_assert!(plan.z_order.is_empty());
            prop_assert_eq!(plan.focus, None);
            prop_assert_eq!(plan.trigger(), None);
            return Ok(());
        }

        // The Trigger_Window hosts the first of them, and only it (Req 5.2,
        // 9.5). With every probe resolved, the mapping is final.
        prop_assert_eq!(plan.trigger(), Some(0));
        prop_assert!(plan.windows.iter().skip(1).all(|w| !w.is_trigger));
        prop_assert!(plan.trigger_decided);

        for (w, e) in plan.windows.iter().zip(&want) {
            // Restored tabs: the order-preserving subsequence of the
            // non-omitted tabs, each showing its saved Location or chosen
            // ancestor; redirected duplicates are kept (Req 5.3, 9.1, 9.2).
            let mut slots = w.slots.as_slice();
            if targeted && w.is_trigger {
                // The requested tab is one extra slot, last (Req 6.2).
                let requested = match &c.launch {
                    LaunchInfo::Targeted { requested } => requested,
                    _ => unreachable!(),
                };
                let (last, rest) = slots.split_last().expect("trigger has the requested slot");
                prop_assert_eq!(last, &Slot::Requested(requested.clone()));
                slots = rest;
            }
            let got_tabs: Vec<(usize, LocationId)> = slots
                .iter()
                .map(|s| match s {
                    Slot::Restore { tab_index, location } => Ok((*tab_index, location.clone())),
                    other => Err(TestCaseError::fail(format!("unexpected slot {other:?}"))),
                })
                .collect::<Result<_, _>>()?;
            prop_assert_eq!(&got_tabs, &e.tabs);
            prop_assert!(w.active < w.slots.len());
        }
    }
}
