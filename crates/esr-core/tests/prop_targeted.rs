// Feature: explorer-session-restore, Property 18: Targeted launch composition
//! Property test for the Targeted_Launch composition of `RestoreSession`
//! (task 11.3), driven against the simulated Explorer.
//!
//! **Property 18: Targeted launch composition.** For any snapshot, requested
//! Location (including one equal to a restored Location), and requested
//! selection set, after Restoration_Completion in the simulator:
//! - the trigger's tabs are its restored tabs in saved order followed by
//!   exactly one tab showing the requested Location;
//! - that last tab is active and holds exactly the requested selection;
//! - every other window keeps its planned active tab.
//!
//! The trigger is also focused and first in z-order, the other windows are
//! restored as saved (tabs, order, window state), and no extra window exists.
//!
//! **Validates: Requirements 6.2, 6.3, 6.4, 6.5**

use esr_core::engine::{Command, ItemId, WinKey};
use esr_core::model::{DisplayRef, LocationId, Placement, Rect, ShowState, Snapshot, TabRec, WindowId, WindowRec};
use esr_core::plan::LaunchInfo;
use esr_core::restore::{RestoreParams, RestoreSession};
use esr_sim::explorer::{default_displays, default_locations, SimConfig, SimExplorer};
use esr_sim::restore::{Driver, Launch, Origin, Stop};
use proptest::collection::{hash_set, vec};
use proptest::prelude::*;

const SEQ: u64 = 41;

fn rect(left: i32, top: i32) -> Rect {
    Rect { left, top, right: left + 800, bottom: top + 600 }
}

#[derive(Clone, Debug)]
struct Scenario {
    /// Per saved window: tab location indices, active tab, state, on display 2.
    windows: Vec<(Vec<usize>, usize, ShowState, bool)>,
    /// Saved Activation_Order, as a permutation of window indices.
    activation: Vec<usize>,
    requested: usize,
    selection: Vec<ItemId>,
}

fn show_state() -> impl Strategy<Value = ShowState> {
    prop_oneof![
        4 => Just(ShowState::Normal),
        2 => Just(ShowState::Maximized),
        2 => any::<bool>().prop_map(|restore_maximized| ShowState::Minimized { restore_maximized }),
    ]
}

fn scenario() -> impl Strategy<Value = Scenario> {
    let n_loc = default_locations().len();
    let window = (1..=4usize).prop_flat_map(move |n| {
        (vec(0..n_loc, n), 0..n, show_state(), any::<bool>())
    });
    (vec(window, 1..=3usize), 0..n_loc, hash_set(vec(any::<u8>(), 1..6), 0..4)).prop_flat_map(
        |(windows, requested, items)| {
            let n = windows.len();
            let selection: Vec<ItemId> = items.into_iter().map(ItemId).collect();
            (Just(windows), Just(requested), Just(selection), Just((0..n).collect::<Vec<_>>()).prop_shuffle())
        },
    )
    .prop_map(|(windows, requested, selection, activation)| Scenario { windows, activation, requested, selection })
}

fn snapshot(sc: &Scenario) -> Snapshot {
    let locs = default_locations();
    let displays = default_displays();
    let windows: Vec<WindowRec> = sc
        .windows
        .iter()
        .enumerate()
        .map(|(i, (tabs, active, state, second))| {
            let d = &displays[usize::from(*second)];
            WindowRec {
                id: WindowId(i as u32 + 1),
                tabs: tabs.iter().map(|&t| TabRec { location: locs[t].clone() }).collect(),
                active: *active,
                placement: Placement {
                    state: *state,
                    restored: rect(d.work.left + 60 + 40 * i as i32, 40 + 30 * i as i32),
                    display: DisplayRef { device: d.device.clone(), monitor: d.monitor },
                },
            }
        })
        .collect();
    let activation = sc.activation.iter().map(|&i| windows[i].id).collect();
    Snapshot { windows, activation }
}

/// What a window shows: its tab Locations left to right and its active index.
type View = (Vec<LocationId>, usize);

fn view(sim: &SimExplorer, win: WinKey) -> View {
    let w = sim.window(win).expect("window is open");
    (w.tabs.iter().map(|t| t.last_known.clone().expect("navigated")).collect(), w.active)
}

fn check(sc: &Scenario) -> Result<(), TestCaseError> {
    let locs = default_locations();
    let snap = snapshot(sc);
    let requested = locs[sc.requested].clone();

    let mut sim = SimExplorer::new(SimConfig::default());
    sim.launch(&Launch {
        location: requested.clone(),
        selection: sc.selection.clone(),
        rect: rect(300, 300),
        maximized: false,
    });
    sim.settle();
    let (trigger, original) = {
        let w = &sim.windows()[0];
        (w.key, w.tabs[0].key)
    };

    let mut session = RestoreSession::new(
        sim.now(),
        RestoreParams {
            snapshot: snap.clone(),
            launch: LaunchInfo::Targeted { requested: requested.clone() },
            displays: default_displays(),
            trigger,
            trigger_tab: Some(original),
            trigger_location: Some(requested.clone()),
            requested_selection: sc.selection.clone(),
            attempt_write_seq: SEQ,
            first_id: 500,
        },
    );
    let mut driver = Driver::new(sim);
    driver.submit([Command::WriteSessionFile { seq: SEQ, text: "failed-attempts 1".into(), urgent: true }]);
    let stop = driver.run(|now, ev| session.handle(now, &ev));
    let run = driver.finish(stop);
    prop_assert_eq!(run.stop, Stop::Quiescent);
    prop_assert!(session.is_settled(), "{:?}", session.phase());
    let sim = &run.sim;

    // Saved window 0 (first in Activation_Order) is restored in the trigger.
    let first = snap.windows.iter().find(|w| w.id == snap.activation[0]).unwrap();
    let saved_tabs: Vec<LocationId> = first.tabs.iter().map(|t| t.location.clone()).collect();

    // Req 6.2, 6.5: restored tabs in saved order, then exactly one more tab
    // showing the requested Location (also when a restored tab shows it).
    let (tabs, active) = view(sim, trigger);
    let mut expected_tabs = saved_tabs;
    expected_tabs.push(requested);
    prop_assert_eq!(&tabs, &expected_tabs);

    // Req 6.3, 6.4: that last tab is active and holds exactly the selection.
    let last = tabs.len() - 1;
    prop_assert_eq!(active, last);
    let keys: Vec<_> = sim.window(trigger).unwrap().tabs.iter().map(|t| t.key).collect();
    prop_assert_eq!(sim.selection(keys[last]), &sc.selection[..]);
    prop_assert_eq!(session.requested_tab(), Some(keys[last]));
    // Only the requested tab is required to hold the selection; tab 0 may
    // keep the launch selection when it already showed the requested Location.
    prop_assert!(keys[..last].iter().skip(1).all(|&k| sim.selection(k).is_empty()), "a restored tab holds a selection");
    // The trigger is never minimized (Req 6.6).
    let trigger_minimized = matches!(sim.window(trigger).unwrap().placement.state, ShowState::Minimized { .. });
    prop_assert!(!trigger_minimized, "the trigger is minimized");

    // Every other window is restored as saved, keeping its saved Active_Tab.
    let others: Vec<WinKey> = sim
        .window_records()
        .iter()
        .filter(|r| matches!(r.origin, Origin::Request(_)) && r.closed_at.is_none())
        .map(|r| r.key)
        .collect();
    prop_assert_eq!(sim.windows().len(), snap.windows.len(), "exactly the planned windows exist");
    prop_assert_eq!(others.len(), snap.windows.len() - 1);
    let saved_others: Vec<(View, ShowState)> = snap
        .activation
        .iter()
        .skip(1)
        .map(|id| {
            let w = snap.windows.iter().find(|w| w.id == *id).unwrap();
            ((w.tabs.iter().map(|t| t.location.clone()).collect(), w.active), w.placement.state)
        })
        .collect();
    let live_state = |k: WinKey| sim.window(k).unwrap().placement.state;
    let mut got: Vec<String> = others.iter().map(|&k| format!("{:?}", (view(sim, k), live_state(k)))).collect();
    let mut want: Vec<String> = saved_others.iter().map(|e| format!("{e:?}")).collect();
    got.sort();
    want.sort();
    prop_assert_eq!(got, want);

    // Focus and z-order: the trigger first, the others in saved order.
    prop_assert_eq!(sim.focused(), Some(trigger));
    let z = sim.z_order();
    prop_assert_eq!(z[0], trigger);
    let z_others: Vec<String> =
        z[1..].iter().map(|&k| format!("{:?}", (view(sim, k), live_state(k)))).collect();
    let want_z: Vec<String> = saved_others.iter().map(|e| format!("{e:?}")).collect();
    prop_assert_eq!(z_others, want_z);
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

    /// Property 18 over random snapshots, requested Locations (often equal to
    /// a saved Location, as the pool is small) and selections.
    #[test]
    fn targeted_launch_composition(sc in scenario()) {
        check(&sc)?;
    }
}
