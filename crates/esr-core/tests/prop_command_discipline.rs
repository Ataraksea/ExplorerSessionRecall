// Feature: explorer-session-restore, Property 19: Restoration command discipline
//! Property test for the window commands of `RestoreSession`, driven against
//! the simulated Explorer (`esr-sim`).
//!
//! **Property 19: Restoration command discipline.** For any simulated
//! restoration:
//! - the trigger receives exactly one `ApplyPlacement` (none for an empty
//!   plan, Req 9.6), carrying its final placement, issued only once its role
//!   and placement are decided (Req 7.7);
//! - every non-trigger window is created with its final placement (hidden),
//!   first becomes visible at it, and receives no placement change after
//!   its `Reveal` (Req 7.2);
//! - the number of `OpenWindow` commands equals the number of planned
//!   non-trigger windows; each is issued only for a decided, known-restorable
//!   window, on one of its restorable Locations (Req 7.3);
//! - no window is closed;
//! - no window other than the trigger, the planned windows, and user-opened
//!   windows exists at completion (Req 7.3).
//!
//! Also checked: nothing is issued before the attempt-counter write
//! completes, and at most one `CreateTab` is outstanding per window (no
//! pipelining).
//!
//! Inputs: random snapshots over the simulator's location pool (1–4 windows,
//! 1–5 tabs, random placements on and off screen, every show state), random
//! probe outcomes and latencies (network Locations up to 5 s, so their tabs
//! are placeholders for a while; omissions of every kind; redirects to random
//! ancestors), random Explorer latencies (window open and contention, tab
//! registration, navigation, close, writes), Default and Targeted launches,
//! and user windows opened during the restoration.
//!
//! **Validates: Requirements 7.2, 7.3, 7.7**

use std::collections::BTreeSet;

use esr_core::engine::{Command, Event, ItemId, Millis, WinKey};
use esr_core::model::{
    DisplayRef, LocationId, Placement, Rect, ShowState, Snapshot, TabRec, WindowId, WindowRec,
};
use esr_core::plan::fallback::{ancestors, AncestorProbe, ProbeOutcome};
use esr_core::plan::placement::FinalPlacement;
use esr_core::plan::{LaunchInfo, RestorePlan, Slot};
use esr_core::restore::{RestoreOutcome, RestoreParams, RestoreSession};
use esr_sim::explorer::{default_displays, default_locations, SimConfig, SimExplorer};
use esr_sim::restore::{home_location, CommandKind, Driver, Launch, Origin, Stop, UserAction};
use proptest::collection::vec;
use proptest::prelude::*;

const SEQ: u64 = 19;

/// Pool indices usable in snapshots and as requested Locations. The last
/// pool entry (Recycle Bin) is reserved for user-opened windows, so they can
/// never claim an `OpenWindow` request (the documented ambiguity of Req 4.2,
/// covered by other tests).
const SNAPSHOT_POOL: usize = 11;
const USER_LOCATION: usize = 11;
/// Network Locations of the pool (mapped drive, UNC share).
const NETWORK: [usize; 2] = [6, 7];

fn loc(i: usize) -> LocationId {
    default_locations()[i].clone()
}

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn show_state() -> impl Strategy<Value = ShowState> {
    prop_oneof![
        5 => Just(ShowState::Normal),
        2 => Just(ShowState::Maximized),
        1 => Just(ShowState::Minimized { restore_maximized: false }),
        1 => Just(ShowState::Minimized { restore_maximized: true }),
    ]
}

/// Saved placement: bounds on either display or (partly) off screen.
fn placement() -> impl Strategy<Value = Placement> {
    (show_state(), -600i32..3600, -400i32..1300, 200i32..2200, 150i32..1300, 0usize..2).prop_map(
        |(state, left, top, w, h, d)| {
            let display = &default_displays()[d];
            Placement {
                state,
                restored: Rect { left, top, right: left + w, bottom: top + h },
                display: DisplayRef { device: display.device.clone(), monitor: display.monitor },
            }
        },
    )
}

/// One saved window: tabs as pool indices, the active index, a placement.
fn window() -> impl Strategy<Value = (Vec<usize>, prop::sample::Index, Placement)> {
    (vec(0..SNAPSHOT_POOL, 1..=5), any::<prop::sample::Index>(), placement())
}

/// A valid snapshot with 1–4 windows and a random Activation_Order.
fn snapshot() -> impl Strategy<Value = Snapshot> {
    vec(window(), 1..=4)
        .prop_flat_map(|ws| {
            let n = ws.len();
            (Just(ws), Just((1..=n as u32).collect::<Vec<_>>()).prop_shuffle())
        })
        .prop_map(|(ws, order)| {
            let windows = ws
                .into_iter()
                .enumerate()
                .map(|(i, (tabs, active, placement))| WindowRec {
                    id: WindowId(i as u32 + 1),
                    active: active.index(tabs.len()),
                    tabs: tabs.into_iter().map(|t| TabRec { location: loc(t) }).collect(),
                    placement,
                })
                .collect();
            Snapshot { windows, activation: order.into_iter().map(WindowId).collect() }
        })
}

/// Test-side probe outcome kind.
#[derive(Clone, Copy, Debug)]
enum Kind {
    Available,
    Missing,
    AccessDenied,
    Timeout,
    Unresolvable,
    ShellUnavailable,
}

fn kind() -> impl Strategy<Value = Kind> {
    prop_oneof![
        8 => Just(Kind::Available),
        3 => Just(Kind::Missing),
        1 => Just(Kind::AccessDenied),
        1 => Just(Kind::Timeout),
        1 => Just(Kind::Unresolvable),
        1 => Just(Kind::ShellUnavailable),
    ]
}

/// Programmed probe of one pool Location: outcome kind, latency, and the
/// availability of each ancestor (used when the outcome is `Missing`).
#[derive(Clone, Debug)]
struct Probe {
    kind: Kind,
    latency_ms: u64,
    ancestors: Vec<bool>,
}

fn probes() -> impl Strategy<Value = Vec<Probe>> {
    let local = (kind(), 1u64..=300, vec(any::<bool>(), 4));
    let network = (kind(), 1u64..=5000, vec(any::<bool>(), 4));
    (vec(local, SNAPSHOT_POOL), vec(network, NETWORK.len())).prop_map(|(local, network)| {
        let mut out: Vec<Probe> =
            local.into_iter().map(|(kind, latency_ms, ancestors)| Probe { kind, latency_ms, ancestors }).collect();
        for (&i, (kind, latency_ms, ancestors)) in NETWORK.iter().zip(network) {
            out[i] = Probe { kind, latency_ms, ancestors };
        }
        out
    })
}

fn outcome_for(location: &LocationId, p: &Probe) -> ProbeOutcome {
    match p.kind {
        Kind::Available => ProbeOutcome::Available,
        Kind::Missing => ProbeOutcome::Missing {
            ancestors: ancestors(location)
                .into_iter()
                .enumerate()
                .map(|(i, a)| AncestorProbe { location: a, available: p.ancestors.get(i).copied().unwrap_or(true) })
                .collect(),
        },
        Kind::AccessDenied => ProbeOutcome::AccessDenied,
        Kind::Timeout => ProbeOutcome::Timeout,
        Kind::Unresolvable => ProbeOutcome::Unresolvable,
        Kind::ShellUnavailable => ProbeOutcome::ShellUnavailable,
    }
}

/// Explorer latencies.
#[derive(Clone, Debug)]
struct Latencies {
    window_open_ms: u64,
    window_open_contention_ms: u64,
    tab_register_ms: u64,
    navigate_ms: u64,
    close_tab_ms: u64,
    write_ms: u64,
}

fn latencies() -> impl Strategy<Value = Latencies> {
    (50u64..=1500, 0u64..=800, 20u64..=150, 10u64..=200, 50u64..=400, 1u64..=200).prop_map(
        |(window_open_ms, window_open_contention_ms, tab_register_ms, navigate_ms, close_tab_ms, write_ms)| {
            Latencies { window_open_ms, window_open_contention_ms, tab_register_ms, navigate_ms, close_tab_ms, write_ms }
        },
    )
}

/// How the trigger was opened.
#[derive(Clone, Debug)]
enum LaunchSpec {
    Default,
    Targeted { requested: usize, selection: Vec<ItemId> },
}

fn launch_spec() -> impl Strategy<Value = LaunchSpec> {
    prop_oneof![
        1 => Just(LaunchSpec::Default),
        1 => (0..SNAPSHOT_POOL, vec(vec(any::<u8>(), 1..6).prop_map(ItemId), 0..3))
            .prop_map(|(requested, selection)| LaunchSpec::Targeted { requested, selection }),
    ]
}

#[derive(Clone, Debug)]
struct Case {
    snapshot: Snapshot,
    probes: Vec<Probe>,
    latencies: Latencies,
    launch: LaunchSpec,
    trigger_rect: Rect,
    trigger_maximized: bool,
    /// Offsets (ms after the restoration starts) of user window opens.
    user_opens: Vec<u64>,
}

fn case() -> impl Strategy<Value = Case> {
    (
        snapshot(),
        probes(),
        latencies(),
        launch_spec(),
        (0i32..1500, 0i32..500, any::<bool>()),
        vec(0u64..4000, 0..=1),
    )
        .prop_map(|(snapshot, probes, latencies, launch, (x, y, trigger_maximized), user_opens)| Case {
            snapshot,
            probes,
            latencies,
            launch,
            trigger_rect: Rect { left: x, top: y, right: x + 900, bottom: y + 650 },
            trigger_maximized,
            user_opens,
        })
}

// ---------------------------------------------------------------------------
// Running a restoration
// ---------------------------------------------------------------------------

fn config(l: &Latencies) -> SimConfig {
    let mut c = SimConfig::default();
    c.restore.window_open_ms = l.window_open_ms;
    c.restore.window_open_contention_ms = l.window_open_contention_ms;
    c.restore.tab_register_ms = l.tab_register_ms;
    c.restore.navigate_ms = l.navigate_ms;
    c.restore.close_tab_ms = l.close_tab_ms;
    c.restore.write_ms = l.write_ms;
    c
}

fn same_place(p: &Placement, fp: &FinalPlacement) -> bool {
    p.state == fp.state && p.restored == fp.restored
}

/// The restorable Locations of a planned window.
fn restore_locations(plan: &RestorePlan, i: usize) -> Vec<&LocationId> {
    plan.windows[i]
        .slots
        .iter()
        .filter_map(|s| match s {
            Slot::Restore { location, .. } => Some(location),
            _ => None,
        })
        .collect()
}

/// Runs the restoration of `case` and returns every violation found.
fn check(case: &Case) -> Vec<String> {
    let mut errors: Vec<String> = Vec::new();

    // The trigger: a settled window opened by the user.
    let mut sim = SimExplorer::new(config(&case.latencies));
    let (launch_info, trigger_location, selection) = match &case.launch {
        LaunchSpec::Default => (LaunchInfo::Default, home_location(), vec![]),
        LaunchSpec::Targeted { requested, selection } => {
            (LaunchInfo::Targeted { requested: loc(*requested) }, loc(*requested), selection.clone())
        }
    };
    sim.launch(&Launch {
        location: trigger_location.clone(),
        selection: selection.clone(),
        rect: case.trigger_rect,
        maximized: case.trigger_maximized,
    });
    sim.settle();
    let (trigger, trigger_tab) = {
        let w = &sim.windows()[0];
        (w.key, w.tabs[0].key)
    };
    let trigger_placements_before = sim.window_record(trigger).map_or(0, |r| r.placements.len());
    for (i, p) in case.probes.iter().enumerate() {
        let location = loc(i);
        let outcome = outcome_for(&location, p);
        sim.set_probe(location, outcome, p.latency_ms);
    }

    let started = sim.now();
    let mut session = RestoreSession::new(
        started,
        RestoreParams {
            snapshot: case.snapshot.clone(),
            launch: launch_info,
            displays: default_displays(),
            trigger,
            trigger_tab: Some(trigger_tab),
            trigger_location: Some(trigger_location),
            requested_selection: selection,
            attempt_write_seq: SEQ,
            first_id: 7000,
        },
    );
    let mut driver = Driver::new(sim);
    for &at in &case.user_opens {
        driver.schedule(
            started + at,
            UserAction::Launch(Launch {
                location: loc(USER_LOCATION),
                selection: vec![],
                rect: Rect { left: 400, top: 200, right: 1100, bottom: 700 },
                maximized: false,
            }),
        );
    }
    driver.submit([Command::WriteSessionFile { seq: SEQ, text: "failed-attempts 1".into(), urgent: true }]);

    let mut outcome: Option<RestoreOutcome> = None;
    let mut written: Option<Millis> = None;
    let mut opens_issued = 0usize;
    let stop = driver.run(|now, ev| {
        if matches!(&ev, Event::WriteCompleted { seq, result: Ok(()) } if *seq == SEQ) {
            written.get_or_insert(now);
        }
        let out = session.handle(now, &ev);
        if let Some(o) = session.take_outcome() {
            if outcome.is_some() {
                errors.push("outcome reported twice".into());
            }
            outcome = Some(o);
        }
        if written.is_none() && !out.is_empty() {
            errors.push(format!("{now:?}: commands before the attempt write completed: {out:?}"));
        }
        // Checked against the plan as it stands when the command is issued.
        let plan = session.plan();
        for c in &out {
            match c {
                Command::ApplyPlacement { win, placement } => {
                    let ok = *win == trigger
                        && plan.windows.first().is_some_and(|w| w.is_trigger && w.decided && w.placement == *placement);
                    if !ok {
                        errors.push(format!("{now:?}: ApplyPlacement {win:?} {placement:?} not the decided trigger's: {plan:?}"));
                    }
                }
                Command::OpenWindow { location, placement, .. } => {
                    opens_issued += 1;
                    let eligible: Vec<usize> = (1..plan.windows.len())
                        .filter(|&i| plan.windows[i].decided && plan.windows[i].known_restorable())
                        .collect();
                    let matches = eligible.iter().any(|&i| {
                        plan.windows[i].placement == *placement && restore_locations(plan, i).contains(&location)
                    });
                    if !matches {
                        errors.push(format!(
                            "{now:?}: OpenWindow on {location:?} at {placement:?} matches no decided, restorable window: {plan:?}"
                        ));
                    }
                    if opens_issued > eligible.len() {
                        errors.push(format!("{now:?}: {opens_issued} OpenWindow for {} eligible windows", eligible.len()));
                    }
                }
                _ => {}
            }
        }
        out
    });
    let run = driver.finish(stop);
    let sim = &run.sim;
    if run.stop != Stop::Quiescent {
        errors.push(format!("run stopped: {:?}", run.stop));
    }
    match &outcome {
        Some(RestoreOutcome::Completed(_)) => {}
        other => {
            errors.push(format!("not completed: {other:?} / {:?}", session.phase()));
            return errors;
        }
    }

    // Nothing before the attempt write completed (commands carry their time).
    let Some(written) = written else {
        errors.push("attempt write never completed".into());
        return errors;
    };
    let log = sim.command_log();
    if let Some(c) = log[1..].iter().find(|c| c.at < written && !matches!(c.command, Command::SetTimer { .. })) {
        // User launches issue no commands; every recorded command is the session's.
        errors.push(format!("command at {:?} before the attempt write completed at {written:?}: {c:?}", c.at));
    }

    let plan = session.plan().clone();

    // Req 7.7: the trigger is placed exactly once, at its final placement.
    let applies = sim.commands_of(CommandKind::ApplyPlacement);
    let trigger_rec = sim.window_record(trigger).expect("trigger record");
    if plan.is_empty() {
        if !applies.is_empty() {
            errors.push(format!("empty plan but ApplyPlacement issued: {applies:?}"));
        }
        if trigger_rec.placements.len() != trigger_placements_before {
            errors.push(format!("empty plan but the trigger moved: {:?}", trigger_rec.placements));
        }
    } else {
        let fp = plan.windows[0].placement;
        let ok = applies.len() == 1
            && matches!(&applies[0].command, Command::ApplyPlacement { win, placement } if *win == trigger && *placement == fp);
        if !ok {
            errors.push(format!("trigger placement: expected one ApplyPlacement with {fp:?}, got {applies:?}"));
        }
        if trigger_rec.placements.len() > trigger_placements_before + 1 || trigger_rec.visible_placement_changes > 1 {
            errors.push(format!("trigger changed placement more than once: {:?}", trigger_rec.placements));
        }
        match sim.window(trigger) {
            Some(w) if same_place(&w.placement, &fp) => {}
            other => errors.push(format!("trigger not at its final placement {fp:?}: {:?}", other.map(|w| &w.placement))),
        }
    }

    // Req 7.3: one OpenWindow per planned non-trigger window.
    let opens = sim.commands_of(CommandKind::OpenWindow);
    let expected_opens = plan.windows.len().saturating_sub(1);
    if opens.len() != expected_opens {
        errors.push(format!("{} OpenWindow for {expected_opens} planned non-trigger windows", opens.len()));
    }

    // Req 7.2: each opened window created hidden at its final placement,
    // first visible there, and never moved.
    let mut planned_keys: BTreeSet<WinKey> = BTreeSet::from([trigger]);
    for (i, pw) in plan.windows.iter().enumerate().skip(1) {
        let Some(win) = session.window_for(pw.source) else {
            errors.push(format!("planned window {:?} has no window", pw.source));
            continue;
        };
        if !planned_keys.insert(win) {
            errors.push(format!("window {win:?} hosts two planned windows"));
        }
        let Some(rec) = sim.window_record(win) else {
            errors.push(format!("no record for {win:?}"));
            continue;
        };
        let Origin::Request(request) = rec.origin else {
            errors.push(format!("planned window {:?} is a user window {win:?}", pw.source));
            continue;
        };
        let open = opens.iter().find(|c| matches!(c.command, Command::OpenWindow { request: r, .. } if r == request));
        match open.map(|c| &c.command) {
            Some(Command::OpenWindow { placement, location, .. }) => {
                if *placement != pw.placement {
                    errors.push(format!("{win:?} requested at {placement:?}, final {:?}", pw.placement));
                }
                if !restore_locations(&plan, i).contains(&location) {
                    errors.push(format!("{win:?} opened on {location:?}, not one of its restored Locations"));
                }
            }
            _ => errors.push(format!("{win:?}: no OpenWindow for {request:?}")),
        }
        if rec.placements.len() != 1 || !same_place(&rec.placements[0].1, &pw.placement) {
            errors.push(format!("{win:?} placements {:?}, final {:?}", rec.placements, pw.placement));
        }
        match &rec.first_visible {
            Some((_, p)) if same_place(p, &pw.placement) => {}
            other => errors.push(format!("{win:?} first visible {other:?}, final {:?}", pw.placement)),
        }
        if rec.visible_placement_changes != 0 {
            errors.push(format!("{win:?} moved {} times while visible", rec.visible_placement_changes));
        }
    }

    // No window closed; at completion only the trigger, the planned windows,
    // and user windows exist.
    for r in sim.window_records() {
        if r.closed_at.is_some() {
            errors.push(format!("window {:?} ({:?}) was closed", r.key, r.origin));
        }
    }
    let user: BTreeSet<WinKey> = sim
        .window_records()
        .iter()
        .filter(|r| r.origin == Origin::User && r.key != trigger)
        .map(|r| r.key)
        .collect();
    if user.len() != case.user_opens.len() {
        errors.push(format!("{} user windows for {} user opens", user.len(), case.user_opens.len()));
    }
    let live: BTreeSet<WinKey> = sim.windows().iter().map(|w| w.key).collect();
    let allowed: BTreeSet<WinKey> = planned_keys.union(&user).copied().collect();
    if live != allowed {
        errors.push(format!("live windows {live:?}, expected {allowed:?}"));
    }

    // At most one CreateTab outstanding per window.
    if sim.pipelining_violated() {
        errors.push(format!("pipelined tab creation: {:?}", sim.pipelining_violations()));
    }

    errors
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

    /// Property 19: Restoration command discipline.
    ///
    /// **Validates: Requirements 7.2, 7.3, 7.7**
    #[test]
    fn restoration_command_discipline(case in case()) {
        let errors = check(&case);
        prop_assert!(errors.is_empty(), "{}", errors.join("\n"));
    }
}

/// A fixed mix: network placeholders, a redirect, an omitted window, user
/// window, Default_Launch.
#[test]
fn mixed_default_launch_example() {
    let p = |state, left| Placement {
        state,
        restored: Rect { left, top: 40, right: left + 800, bottom: 640 },
        display: DisplayRef {
            device: default_displays()[usize::from(left >= 1920)].device.clone(),
            monitor: default_displays()[usize::from(left >= 1920)].monitor,
        },
    };
    let w = |id, tabs: &[usize], active, placement| WindowRec {
        id: WindowId(id),
        tabs: tabs.iter().map(|&t| TabRec { location: loc(t) }).collect(),
        active,
        placement,
    };
    let snapshot = Snapshot {
        windows: vec![
            w(1, &[7], 0, p(ShowState::Normal, 100)),
            w(2, &[2, 6, 3], 2, p(ShowState::Maximized, 2000)),
            w(3, &[8, 9], 0, p(ShowState::Minimized { restore_maximized: false }, 300)),
        ],
        activation: vec![WindowId(1), WindowId(2), WindowId(3)],
    };
    let available = |latency_ms| Probe { kind: Kind::Available, latency_ms, ancestors: vec![true; 4] };
    let mut probes: Vec<Probe> = (0..SNAPSHOT_POOL).map(|_| available(5)).collect();
    probes[7] = Probe { kind: Kind::Timeout, latency_ms: 2500, ancestors: vec![] };
    probes[6] = available(3000);
    probes[8] = Probe { kind: Kind::Missing, latency_ms: 30, ancestors: vec![false, true, true, true] };
    let case = Case {
        snapshot,
        probes,
        latencies: Latencies {
            window_open_ms: 430,
            window_open_contention_ms: 335,
            tab_register_ms: 85,
            navigate_ms: 60,
            close_tab_ms: 300,
            write_ms: 10,
        },
        launch: LaunchSpec::Default,
        trigger_rect: Rect { left: 300, top: 300, right: 1100, bottom: 900 },
        trigger_maximized: false,
        user_opens: vec![1000],
    };
    let errors = check(&case);
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}
