// Feature: explorer-session-restore, Property 12: Restore round trip
//! Property test for the restore round trip, run end to end: an `Engine`
//! starts with a Pending_Snapshot in its Session_File, the user opens File
//! Explorer on its default start location (a Default_Launch), and the Engine
//! restores the snapshot against the simulated Explorer
//! (`SimConfig::for_restoration`, same displays as at save time, every
//! Location available).
//!
//! **Property 12: Restore round trip.** For any valid Pending_Snapshot whose
//! Locations are all available, restored through a Default_Launch with the
//! same displays as at save time: after Restoration_Completion the
//! simulator's windows give a capture equal to the snapshot on window count,
//! per-window tab order, canonical Location keys, active tabs, window states,
//! restored bounds (and display), and Activation_Order, except that the
//! Activation_Order is the saved one with the focus target (Req 5.10, 5.11)
//! moved to the front, since focusing a window is an activation. The trigger
//! shows no default-start tab unless one was saved, the file is marked
//! Consumed, and the Live_Session equals the capture.
//!
//! Generated snapshots use the simulator's location pool (every kind,
//! including Home, the default start location) and restored bounds that fit
//! a display's work area (Req 5.5), with the display a capture would record
//! for them. Besides the Activation_Order adjustment, the one deliberate
//! difference from the saved snapshot is Req 5.11: when every saved window
//! is minimized, the first window in the Activation_Order is shown in its
//! pre-minimize state (and is the focus target, so the order is unchanged).
//!
//! **Validates: Requirements 4.4, 4.5, 5.1, 5.3, 5.4, 5.9, 6.1, 7.6**

use std::cmp::Reverse;

use esr_core::engine::{Command, Engine, Environment, Millis, SessionFileRead, StartupInput, WinKey};
use esr_core::logfmt::entry::LogEntry;
use esr_core::model::{
    DisplayRef, Placement, Rect, SaveCause, SessionDoc, ShowState, Snapshot, Status, TabRec, Timestamp,
    WindowId, WindowRec,
};
use esr_core::restore::Phase;
use esr_core::settings::Settings;
use esr_sim::explorer::{default_displays, default_locations, Shown, SimConfig, SimExplorer};
use esr_sim::reference::canonical;
use esr_sim::restore::{home_location, CommandKind, Driver, Launch, Origin, Run, Stop, UserAction};
use proptest::collection::{btree_set, vec};
use proptest::prelude::*;

/// When the user opens File Explorer.
const LAUNCH_AT: u64 = 1_000;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn overlap(a: &Rect, b: &Rect) -> i64 {
    let w = (i64::from(a.right.min(b.right)) - i64::from(a.left.max(b.left))).max(0);
    let h = (i64::from(a.bottom.min(b.bottom)) - i64::from(a.top.max(b.top))).max(0);
    w * h
}

fn area(r: &Rect) -> i64 {
    (i64::from(r.right) - i64::from(r.left)) * (i64::from(r.bottom) - i64::from(r.top))
}

/// At least 50% of `r` lies in one display's work area (Req 5.5).
fn fits(r: &Rect) -> bool {
    let best = default_displays().iter().map(|d| overlap(r, &d.work)).max().unwrap_or(0);
    area(r) > 0 && 2 * best >= area(r)
}

/// The display a capture records for restored bounds `r`: the largest
/// monitor overlap, ties to the primary, then the lowest index.
fn display_of(r: &Rect) -> DisplayRef {
    let displays = default_displays();
    let (_, d) = displays
        .iter()
        .enumerate()
        .max_by_key(|(i, d)| (overlap(r, &d.monitor), d.primary, Reverse(*i)))
        .expect("displays");
    DisplayRef { device: d.device.clone(), monitor: d.monitor }
}

/// A rect inside one display's work area.
fn rect_within_work_area() -> impl Strategy<Value = Rect> {
    (0..default_displays().len())
        .prop_flat_map(|i| {
            let work = default_displays()[i].work;
            let (w, h) = (work.right - work.left, work.bottom - work.top);
            (Just(work), 200i32..=w, 150i32..=h)
        })
        .prop_flat_map(|(work, w, h)| (Just((w, h)), work.left..=work.right - w, work.top..=work.bottom - h))
        .prop_map(|((w, h), left, top)| Rect { left, top, right: left + w, bottom: top + h })
}

/// Any rect (straddling displays or partly off-screen) that still fits.
fn rect_fitting() -> impl Strategy<Value = Rect> {
    (-600i32..3600, -400i32..1000, 200i32..=2400, 150i32..=1400)
        .prop_map(|(left, top, w, h)| Rect { left, top, right: left + w, bottom: top + h })
        .prop_filter("fits a work area", fits)
}

fn show_state() -> impl Strategy<Value = ShowState> {
    prop_oneof![
        4 => Just(ShowState::Normal),
        2 => Just(ShowState::Maximized),
        2 => any::<bool>().prop_map(|restore_maximized| ShowState::Minimized { restore_maximized }),
    ]
}

fn placement() -> impl Strategy<Value = Placement> {
    let restored = prop_oneof![3 => rect_within_work_area().boxed(), 1 => rect_fitting().boxed()];
    (show_state(), restored).prop_map(|(state, restored)| Placement { state, restored, display: display_of(&restored) })
}

/// Tabs (indices into the location pool, duplicates allowed), the active
/// tab, and the placement of one saved window.
fn saved_window() -> impl Strategy<Value = (Vec<usize>, usize, Placement)> {
    vec(0..default_locations().len(), 1..=5)
        .prop_flat_map(|tabs| {
            let n = tabs.len();
            (Just(tabs), 0..n, placement())
        })
}

/// A valid snapshot of 1–4 windows with arbitrary ids and Activation_Order.
fn snapshot() -> impl Strategy<Value = Snapshot> {
    (1usize..=4)
        .prop_flat_map(|n| (vec(saved_window(), n), btree_set(any::<u32>(), n)))
        .prop_flat_map(|(windows, ids)| {
            let ids: Vec<WindowId> = ids.into_iter().map(WindowId).collect();
            (Just(windows), Just(ids.clone()), Just(ids).prop_shuffle())
        })
        .prop_map(|(windows, ids, activation)| {
            let pool = default_locations();
            Snapshot {
                windows: ids
                    .into_iter()
                    .zip(windows)
                    .map(|(id, (tabs, active, placement))| WindowRec {
                        id,
                        tabs: tabs.iter().map(|&i| TabRec { location: pool[i].clone() }).collect(),
                        active,
                        placement,
                    })
                    .collect(),
                activation,
            }
        })
}

/// The trigger's initial rect and maximized state (before restoration).
fn trigger_launch() -> impl Strategy<Value = Launch> {
    (rect_within_work_area(), any::<bool>()).prop_map(|(rect, maximized)| Launch {
        location: home_location(),
        selection: vec![],
        rect,
        maximized,
    })
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn pending(snap: &Snapshot) -> SessionDoc {
    SessionDoc {
        version: esr_core::format::FORMAT_VERSION,
        status: Status::Pending,
        failed_attempts: 0,
        saved_at: Timestamp { year: 2025, month: 3, day: 4, hour: 5, minute: 6, second: 7, millis: 8, offset_minutes: 0 },
        cause: SaveCause::LastWindowClosed,
        snapshot: snap.clone(),
    }
}

/// An Engine with `snap` pending (read from the Session_File at startup); the
/// user opens Explorer at [`LAUNCH_AT`]. Runs until quiescence.
fn restore(snap: &Snapshot, launch: Launch) -> (Engine, Run) {
    let text = esr_core::format::serialize(&pending(snap));
    let startup = StartupInput {
        session_file: SessionFileRead::Content(text.into_bytes()),
        environment: Environment { displays: default_displays(), ..Environment::default() },
        background_logon: false,
        logon_at: None,
        shell_ready_at: Some(Millis(0)),
    };
    let (mut engine, commands) = Engine::new(Settings::default(), startup);
    let mut driver = Driver::new(SimExplorer::new(SimConfig::for_restoration()));
    driver.submit(commands);
    driver.schedule(Millis(LAUNCH_AT), UserAction::Launch(launch));
    let stop = driver.run(|now, ev| engine.handle(now, ev));
    (engine, driver.finish(stop))
}

fn is_minimized(s: ShowState) -> bool {
    matches!(s, ShowState::Minimized { .. })
}

/// The snapshot a capture after restoration should equal, and the saved
/// window that receives keyboard focus. It is the saved snapshot, except:
/// - with every window minimized, the first window in the Activation_Order
///   is shown in its pre-minimize state (Req 5.11);
/// - the focus target, the first window in saved Activation_Order that is
///   not minimized after that adjustment (Req 5.10, 5.11), becomes the most
///   recently activated window: the Activation_Order is the saved one with
///   the focus target moved to the front (Req 5.9). When the focus target is
///   the first window (always so in the Req 5.11 case) the order is unchanged.
fn expected(snap: &Snapshot) -> (Snapshot, WindowId) {
    let mut e = snap.clone();
    if e.windows.iter().all(|w| is_minimized(w.placement.state)) {
        let first = e.windows.iter_mut().find(|w| w.id == snap.activation[0]).expect("listed");
        first.placement.state = match first.placement.state {
            ShowState::Minimized { restore_maximized: true } => ShowState::Maximized,
            _ => ShowState::Normal,
        };
    }
    let state = |id: WindowId| e.windows.iter().find(|w| w.id == id).expect("listed").placement.state;
    let focus = *snap.activation.iter().find(|&&id| !is_minimized(state(id))).expect("a shown window");
    e.activation.retain(|&id| id != focus);
    e.activation.insert(0, focus);
    (e, focus)
}

/// Every Session_File write, parsed, with its time.
fn writes(run: &Run) -> Vec<(Millis, SessionDoc)> {
    run.commands
        .iter()
        .filter_map(|c| match &c.command {
            Command::WriteSessionFile { text, .. } => {
                Some((c.at, esr_core::format::parse(text.as_bytes()).expect("the Engine writes valid files")))
            }
            _ => None,
        })
        .collect()
}

/// What the simulator's open windows show, in `order`, as a snapshot (window
/// ids by position). `None` if a tab shows anything but an identified,
/// settled Location.
fn truth(run: &Run, order: &[WinKey]) -> Option<Snapshot> {
    let mut windows = Vec::new();
    for (n, key) in order.iter().enumerate() {
        let w = run.sim.window(*key)?;
        let mut tabs = Vec::new();
        for t in &w.tabs {
            match (&t.shown, &t.last_known, &t.pending) {
                (Shown::Known(shown), Some(known), None) if shown == known => {
                    tabs.push(TabRec { location: shown.clone() })
                }
                _ => return None,
            }
        }
        windows.push(WindowRec { id: WindowId(n as u32), tabs, active: w.active, placement: w.placement.clone() });
    }
    let activation = (0..order.len() as u32).map(WindowId).collect();
    Some(Snapshot { windows, activation })
}

// ---------------------------------------------------------------------------
// Property
// ---------------------------------------------------------------------------

fn check(snap: &Snapshot, launch: Launch) -> Result<(), TestCaseError> {
    let (engine, run) = restore(snap, launch);
    prop_assert_eq!(run.stop, Stop::Quiescent);
    let (expected, focus_id) = expected(snap);

    // The restoration ran and completed.
    let session = engine.restore_session();
    prop_assert!(session.is_some(), "no restoration started");
    let session = session.unwrap();
    prop_assert_eq!(session.phase(), &Phase::Completed);
    let trigger = session.trigger();

    // Exactly one window per saved window (Req 5.1): the trigger (the only
    // user window) plus one opened per other saved window.
    let open: Vec<WinKey> = run.sim.windows().iter().map(|w| w.key).collect();
    prop_assert_eq!(open.len(), snap.windows.len(), "open windows {:?}", open);
    let users: Vec<WinKey> = run
        .sim
        .window_records()
        .iter()
        .filter(|r| r.origin == Origin::User)
        .map(|r| r.key)
        .collect();
    prop_assert_eq!(&users, &vec![trigger]);

    // Saved windows in Activation_Order → live windows; the trigger restores
    // the first (Req 5.2, 6.1).
    let mut mapped = Vec::new();
    for id in &snap.activation {
        let key = session.window_for(*id);
        prop_assert!(key.is_some_and(|k| open.contains(&k)), "saved window {:?} not restored", id);
        mapped.push(key.unwrap());
    }
    prop_assert_eq!(mapped[0], trigger);

    // The live Activation_Order: the saved one with the focus target moved
    // to the front (Req 5.9, 5.10, 5.11).
    let live: Vec<WinKey> = expected.activation.iter().map(|id| session.window_for(*id).unwrap()).collect();
    let focused = session.window_for(focus_id).unwrap();
    prop_assert_eq!(live[0], focused);

    // Every tab displays its saved Location (Req 5.3, 7.6), the saved Active_Tab
    // is active (Req 5.4), the placement is the saved one (Req 5.9): the
    // simulator's ground truth, in the expected Activation_Order, equals the
    // expected snapshot.
    let shown = truth(&run, &live);
    prop_assert!(shown.is_some(), "a tab shows an unsettled or unidentified Location");
    let shown = shown.unwrap();
    prop_assert_eq!(&shown, &canonical(&expected));

    // The trigger shows the default start location only if it was saved there
    // (Req 6.1); its tabs equal the first saved window's, as checked above.
    let home = home_location();
    let first_saved = snap.windows.iter().find(|w| w.id == snap.activation[0]).unwrap();
    let homes = |tabs: &[TabRec]| tabs.iter().filter(|t| t.location == home).count();
    let at = live.iter().position(|k| *k == trigger).unwrap();
    prop_assert_eq!(homes(&shown.windows[at].tabs), homes(&first_saved.tabs));

    // The Live_Session holds exactly the open windows (Req 4.5), in the
    // expected Activation_Order, and its capture equals the snapshot (Req 5.9).
    let order = engine.tracker().activation_order();
    prop_assert_eq!(&order, &live, "Activation_Order of the Live_Session");
    let (capture, excluded) = engine.tracker().capture();
    prop_assert_eq!(excluded, None, "nothing is cut by the caps");
    prop_assert_eq!(canonical(&capture), canonical(&expected));
    prop_assert_eq!(canonical(&capture), shown);

    // The attempt was counted, then the snapshot was marked Consumed with its
    // content kept, within 2 s of completion (Req 4.4).
    let w = writes(&run);
    prop_assert!(!w.is_empty());
    prop_assert_eq!((w[0].1.status, w[0].1.failed_attempts), (Status::Pending, 1));
    prop_assert_eq!(&w[0].1.snapshot, snap);
    let consumed: Vec<_> = w.iter().filter(|(_, d)| d.status == Status::Consumed).collect();
    prop_assert_eq!(consumed.len(), 1, "Consumed writes");
    prop_assert_eq!(&consumed[0].1.snapshot, snap);
    let focus = run.sim.commands_of(CommandKind::Focus);
    prop_assert_eq!(focus.len(), 1);
    prop_assert_eq!(&focus[0].command, &Command::Focus(focused), "focus goes to the planned target");
    let done_at = focus[0].at;
    prop_assert!(consumed[0].0 >= done_at && consumed[0].0.since(done_at) <= 2_000);
    let durable = engine.store().durable().doc();
    prop_assert!(
        durable.is_some_and(|d| d.status == Status::Consumed || d.cause == SaveCause::Checkpoint),
        "durable document {:?}",
        durable
    );

    // Nothing omitted or redirected: one RESTORE entry, no notification.
    let total_tabs: usize = snap.windows.iter().map(|w| w.tabs.len()).sum();
    let restores: Vec<&LogEntry> =
        run.sim.log_entries().iter().map(|(_, e)| e).filter(|e| matches!(e, LogEntry::Restore { .. })).collect();
    prop_assert_eq!(restores.len(), 1);
    let ok = matches!(restores[0], LogEntry::Restore { targeted: false, windows, tabs, omitted: 0, redirected: 0, .. }
        if *windows as usize == snap.windows.len() && *tabs as usize == total_tabs);
    prop_assert!(ok, "{:?}", restores[0]);
    prop_assert!(run.sim.notifications().is_empty());
    Ok(())
}

proptest! {
    // 128 cases by default; `PROPTEST_CASES` overrides.
    #![proptest_config(ProptestConfig {
        cases: std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(128),
        ..ProptestConfig::default()
    })]

    #[test]
    fn restore_round_trip(snap in snapshot(), launch in trigger_launch()) {
        check(&snap, launch)?;
    }
}

/// A saved window for the fixed cases: tabs as pool indices, an 800×600
/// restored rect at `left`.
fn example_window(id: u32, tabs: &[usize], active: usize, left: i32, state: ShowState) -> WindowRec {
    let pool = default_locations();
    let rect = example_rect(left);
    WindowRec {
        id: WindowId(id),
        tabs: tabs.iter().map(|&i| TabRec { location: pool[i].clone() }).collect(),
        active,
        placement: Placement { state, restored: rect, display: display_of(&rect) },
    }
}

fn example_rect(left: i32) -> Rect {
    Rect { left, top: 40, right: left + 800, bottom: 640 }
}

fn example_launch() -> Launch {
    Launch { location: home_location(), selection: vec![], rect: example_rect(300), maximized: false }
}

/// A fixed case: two windows, Home saved in the trigger's window, the second
/// window maximized on the right display.
#[test]
fn round_trip_example() {
    let snap = Snapshot {
        windows: vec![
            example_window(7, &[2, 10, 3], 2, 100, ShowState::Normal),
            example_window(3, &[9, 11], 0, 2000, ShowState::Maximized),
        ],
        activation: vec![WindowId(7), WindowId(3)],
    };
    check(&snap, example_launch()).unwrap();
}

/// The first saved window is minimized, the second is not: the second gets
/// focus (Req 5.10) and becomes the most recently activated (Req 5.9).
#[test]
fn round_trip_focus_moves_first_shown_window_to_front() {
    let snap = Snapshot {
        windows: vec![
            example_window(1, &[2, 3], 1, 100, ShowState::Minimized { restore_maximized: false }),
            example_window(2, &[9], 0, 2000, ShowState::Normal),
            example_window(3, &[10], 0, 400, ShowState::Maximized),
        ],
        activation: vec![WindowId(1), WindowId(2), WindowId(3)],
    };
    let (e, focus) = expected(&snap);
    assert_eq!(focus, WindowId(2));
    assert_eq!(e.activation, vec![WindowId(2), WindowId(1), WindowId(3)]);
    check(&snap, example_launch()).unwrap();
}

/// Every saved window minimized: the first is shown in its pre-minimize
/// state and focused (Req 5.11), so the Activation_Order is unchanged.
#[test]
fn round_trip_all_minimized() {
    let snap = Snapshot {
        windows: vec![
            example_window(5, &[9], 0, 2000, ShowState::Minimized { restore_maximized: false }),
            example_window(4, &[2, 3], 0, 100, ShowState::Minimized { restore_maximized: true }),
        ],
        activation: vec![WindowId(4), WindowId(5)],
    };
    let (e, focus) = expected(&snap);
    assert_eq!(focus, WindowId(4));
    assert_eq!(e.activation, snap.activation);
    assert_eq!(e.windows[1].placement.state, ShowState::Maximized);
    assert_eq!(e.windows[0].placement.state, ShowState::Minimized { restore_maximized: false });
    check(&snap, example_launch()).unwrap();
}
