// Feature: explorer-session-restore, Property 17: Z-order and focus
//! Property test for the z-order and focus decisions of
//! `esr_core::plan::plan_restore`.
//!
//! **Property 17: Z-order and focus.** For any restore plan:
//! - **Z-order:** the planned windows in saved Activation_Order, first
//!   topmost. For a Targeted_Launch the trigger comes first and the others
//!   keep their relative order.
//! - **Default_Launch focus:** the first window in that order whose final
//!   state is not minimized. If every window is minimized, the first window is
//!   shown in its pre-minimize state, receives focus, and every other window
//!   stays minimized.
//! - **Targeted_Launch focus:** the trigger.
//!
//! The plan is checked for any mix of kept, redirected, omitted and pending
//! tabs. While probes are pending, a first window marked `decided` must keep
//! its role and final state (the Req 5.11 un-minimize included) however the
//! pending probes resolve; a trigger saved as minimized may therefore be
//! undecided.
//!
//! **Validates: Requirements 5.7, 5.10, 5.11, 6.3**

mod common;

use common::strategies::*;
use esr_core::model::*;
use esr_core::plan::fallback::{Fallback, OmitReason};
use esr_core::plan::*;
use proptest::collection::vec;
use proptest::prelude::*;
use proptest::sample::select;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

/// Test-side resolution of one saved tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Res {
    Pending,
    Keep,
    Redirect,
    Omit(OmitReason),
}

const ALL_OMIT_REASONS: [OmitReason; 5] = [
    OmitReason::NotExist,
    OmitReason::Inaccessible,
    OmitReason::Timeout,
    OmitReason::Unresolvable,
    OmitReason::ShellUnavailable,
];

fn omit() -> impl Strategy<Value = Res> {
    select(ALL_OMIT_REASONS.to_vec()).prop_map(Res::Omit)
}

/// A final (non-pending) resolution.
fn resolved() -> BoxedStrategy<Res> {
    prop_oneof![3 => Just(Res::Keep), 1 => Just(Res::Redirect), 2 => omit()].boxed()
}

fn any_res() -> BoxedStrategy<Res> {
    prop_oneof![3 => resolved(), 1 => Just(Res::Pending)].boxed()
}

/// Resolutions for a window of `n` tabs; whole-window patterns are weighted
/// up so windows are often dropped (Req 9.5) or wholly pending.
fn window_res(n: usize) -> BoxedStrategy<Vec<Res>> {
    prop_oneof![
        3 => vec(any_res(), n),
        1 => Just(vec![Res::Keep; n]),
        1 => omit().prop_map(move |r| vec![r; n]),
        1 => Just(vec![Res::Pending; n]),
    ]
    .boxed()
}

fn minimized() -> impl Strategy<Value = ShowState> {
    any::<bool>().prop_map(|restore_maximized| ShowState::Minimized { restore_maximized })
}

/// Optional saved-state override: all minimized, or mostly minimized, so the
/// Req 5.11 case is common.
fn state_override(n: usize) -> impl Strategy<Value = Option<Vec<ShowState>>> {
    prop_oneof![
        2 => Just(None),
        1 => vec(minimized(), n).prop_map(Some),
        1 => vec(prop_oneof![3 => minimized().boxed(), 1 => show_state().boxed()], n).prop_map(Some),
    ]
}

fn launch() -> impl Strategy<Value = LaunchInfo> {
    prop_oneof![
        5 => Just(LaunchInfo::Default),
        4 => location_id().prop_map(|requested| LaunchInfo::Targeted { requested }),
        1 => Just(LaunchInfo::BuiltinRestore),
    ]
}

#[derive(Clone, Debug)]
struct Scenario {
    displays: Vec<Display>,
    snapshot: Snapshot,
    launch: LaunchInfo,
    /// Per saved window (same order as `snapshot.windows`), per tab.
    now: Vec<Vec<Res>>,
    /// How each tab eventually resolves (used only for pending tabs).
    later: Vec<Vec<Res>>,
}

fn scenario() -> impl Strategy<Value = Scenario> {
    (displays_and_snapshot(), launch())
        .prop_flat_map(|((displays, snapshot), launch)| {
            let n = snapshot.windows.len();
            let now: Vec<_> = snapshot.windows.iter().map(|w| window_res(w.tabs.len())).collect();
            let later: Vec<_> = snapshot.windows.iter().map(|w| vec(resolved(), w.tabs.len())).collect();
            (Just(displays), Just(snapshot), Just(launch), state_override(n), now, later)
        })
        .prop_map(|(displays, mut snapshot, launch, states, now, later)| {
            if let Some(states) = states {
                for (w, s) in snapshot.windows.iter_mut().zip(states) {
                    w.placement.state = s;
                }
            }
            Scenario { displays, snapshot, launch, now, later }
        })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn redirect_target() -> LocationId {
    LocationId {
        case_sensitive: false,
        kind: LocationKind::FileSystem(Volume::Fixed),
        parsing_name: WideString::from(r"C:\fallback"),
        display_name: WideString::from("fallback"),
        pidl: vec![],
        search: None,
    }
}

fn availability(s: &Snapshot, res: &[Vec<Res>]) -> Availability {
    let mut a = Availability::new();
    for (w, tabs) in s.windows.iter().zip(res) {
        for (i, r) in tabs.iter().enumerate() {
            let fb = match *r {
                Res::Pending => continue,
                Res::Keep => Fallback::Keep,
                Res::Redirect => Fallback::Redirected(redirect_target()),
                Res::Omit(reason) => Fallback::Omitted(reason),
            };
            a.resolve(TabRef { window: w.id, tab: i }, fb);
        }
    }
    a
}

/// `now` with every pending tab replaced by `f(window, tab)`.
fn complete(now: &[Vec<Res>], f: impl Fn(usize, usize) -> Res) -> Vec<Vec<Res>> {
    now.iter()
        .enumerate()
        .map(|(w, tabs)| {
            tabs.iter().enumerate().map(|(t, &r)| if r == Res::Pending { f(w, t) } else { r }).collect()
        })
        .collect()
}

fn is_min(s: ShowState) -> bool {
    matches!(s, ShowState::Minimized { .. })
}

fn pre_minimize(s: ShowState) -> ShowState {
    match s {
        ShowState::Minimized { restore_maximized: true } => ShowState::Maximized,
        ShowState::Minimized { restore_maximized: false } => ShowState::Normal,
        other => other,
    }
}

/// Independent oracle for Property 17 on one plan.
fn check(sc: &Scenario, res: &[Vec<Res>], plan: &RestorePlan) -> Result<(), TestCaseError> {
    let s = &sc.snapshot;
    prop_assert_eq!(plan.focus, focus_target(plan));
    prop_assert_eq!(&plan.z_order, &z_order(&plan.windows));

    // Expected planned windows, in saved Activation_Order: those with at
    // least one tab not omitted (pending counts as not omitted).
    let expected: Vec<(WindowId, ShowState)> = s
        .activation
        .iter()
        .filter_map(|&id| s.windows.iter().position(|w| w.id == id))
        .filter(|&i| res[i].iter().any(|r| !matches!(r, Res::Omit(_))))
        .map(|i| (s.windows[i].id, s.windows[i].placement.state))
        .collect();

    if matches!(sc.launch, LaunchInfo::BuiltinRestore) || expected.is_empty() {
        prop_assert!(plan.is_empty());
        prop_assert!(plan.z_order.is_empty());
        prop_assert_eq!(plan.focus, None);
        return Ok(());
    }

    let sources: Vec<WindowId> = plan.windows.iter().map(|w| w.source).collect();
    let expected_sources: Vec<WindowId> = expected.iter().map(|e| e.0).collect();
    prop_assert_eq!(&sources, &expected_sources);
    let trigger = plan.trigger();
    prop_assert_eq!(trigger, Some(0));
    prop_assert!(plan.windows.iter().skip(1).all(|w| !w.is_trigger));

    // Z-order: a permutation of the planned windows, the trigger first, then
    // the others in saved Activation_Order (Req 5.7, 6.3).
    let mut sorted = plan.z_order.clone();
    sorted.sort_unstable();
    prop_assert_eq!(sorted, (0..plan.windows.len()).collect::<Vec<_>>());
    prop_assert_eq!(plan.z_order.first().copied(), trigger);
    let z_sources: Vec<WindowId> = plan.z_order.iter().map(|&i| plan.windows[i].source).collect();
    // The trigger is the first planned window, so both rules give the saved
    // Activation_Order of the planned windows.
    prop_assert_eq!(&z_sources, &expected_sources);

    let states: Vec<ShowState> = plan.windows.iter().map(|w| w.placement.state).collect();
    let saved: Vec<ShowState> = expected.iter().map(|e| e.1).collect();

    match &sc.launch {
        LaunchInfo::Targeted { .. } => {
            // Trigger never minimized (Req 6.6), focused (Req 6.3); others as saved.
            let t = trigger.unwrap();
            prop_assert_eq!(states[t], if is_min(saved[t]) { ShowState::Normal } else { saved[t] });
            prop_assert_eq!(&states[1..], &saved[1..]);
            prop_assert_eq!(plan.focus, trigger);
        }
        LaunchInfo::Default => {
            if saved.iter().all(|&st| is_min(st)) {
                // Req 5.11: first shown in its pre-minimize state and focused.
                prop_assert_eq!(states[0], pre_minimize(saved[0]));
                prop_assert_eq!(&states[1..], &saved[1..]);
                prop_assert_eq!(plan.focus, Some(plan.z_order[0]));
            } else {
                // Req 5.10: first non-minimized in z-order.
                prop_assert_eq!(&states, &saved);
                let first_shown = plan.z_order.iter().copied().find(|&i| !is_min(saved[i]));
                prop_assert!(first_shown.is_some());
                prop_assert_eq!(plan.focus, first_shown);
            }
            let f = plan.focus.unwrap();
            prop_assert!(!is_min(states[f]), "focus target {} is minimized", f);
        }
        LaunchInfo::BuiltinRestore => unreachable!(),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// Property 17 on the current plan and on fully resolved plans; a decided
    /// first window keeps its role and final state under any resolution of
    /// the pending probes.
    #[test]
    fn z_order_and_focus(sc in scenario()) {
        let plan_for = |res: &[Vec<Res>]| {
            plan_restore(&sc.snapshot, &sc.launch, &availability(&sc.snapshot, res), &sc.displays)
        };

        let p0 = plan_for(&sc.now);
        check(&sc, &sc.now, &p0)?;

        let completions = [
            complete(&sc.now, |w, t| sc.later[w][t]),
            complete(&sc.now, |_, _| Res::Omit(OmitReason::Timeout)),
            complete(&sc.now, |_, _| Res::Keep),
        ];
        for res in &completions {
            let p1 = plan_for(res);
            check(&sc, res, &p1)?;
            // Fully resolved: every planned window is decided.
            prop_assert!(p1.windows.iter().all(|w| w.decided));

            if let Some(w0) = p0.windows.first().filter(|w| w.decided) {
                let w1 = &p1.windows[0];
                prop_assert_eq!(w1.source, w0.source);
                prop_assert!(w1.is_trigger);
                prop_assert_eq!(&w1.placement, &w0.placement);
            }
        }
    }
}

/// Req 5.11 with a pending, non-minimized window behind a minimized trigger:
/// the trigger waits, and is shown once that window is omitted.
#[test]
fn minimized_trigger_waits_for_pending_non_minimized_window() {
    let d = Display {
        device: r"\\.\DISPLAY1".into(),
        monitor: Rect { left: 0, top: 0, right: 1920, bottom: 1080 },
        work: Rect { left: 0, top: 0, right: 1920, bottom: 1040 },
        primary: true,
    };
    let window = |id: u32, state: ShowState| WindowRec {
        id: WindowId(id),
        tabs: vec![TabRec { location: redirect_target() }],
        active: 0,
        placement: Placement {
            state,
            restored: Rect { left: 100, top: 100, right: 900, bottom: 700 },
            display: DisplayRef { device: d.device.clone(), monitor: d.monitor },
        },
    };
    let s = Snapshot {
        windows: vec![window(1, ShowState::Minimized { restore_maximized: true }), window(2, ShowState::Normal)],
        activation: vec![WindowId(1), WindowId(2)],
    };
    let sc = Scenario {
        displays: vec![d],
        snapshot: s,
        launch: LaunchInfo::Default,
        now: vec![vec![Res::Keep], vec![Res::Pending]],
        later: vec![vec![Res::Keep], vec![Res::Omit(OmitReason::Timeout)]],
    };
    let p0 = plan_restore(&sc.snapshot, &sc.launch, &availability(&sc.snapshot, &sc.now), &sc.displays);
    check(&sc, &sc.now, &p0).unwrap();
    assert!(!p0.windows[0].decided);
    assert_eq!(p0.focus, Some(1));

    let p1 = plan_restore(&sc.snapshot, &sc.launch, &availability(&sc.snapshot, &sc.later), &sc.displays);
    check(&sc, &sc.later, &p1).unwrap();
    assert_eq!(p1.windows.len(), 1);
    assert_eq!(p1.windows[0].placement.state, ShowState::Maximized);
    assert_eq!(p1.focus, Some(0));
}
