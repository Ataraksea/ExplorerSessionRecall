// Feature: explorer-session-restore, Property 20: Network probes do not delay or reorder local tabs
//! Property test for `RestoreSession` with slow network probes, driven
//! against the simulated Explorer (`esr-sim`).
//!
//! **Property 20: Network probes do not delay or reorder local tabs.** For any
//! snapshot mixing local and network Locations, with network probe latencies
//! up to 5 s and any completion order:
//! - every local tab in a window whose trigger mapping is decided is
//!   navigated before any network probe finishes (Req 7.5);
//! - the final tab order of every window equals the planned order, whatever
//!   order the probes completed in (Req 7.9).
//!
//! Each case is run twice on the same snapshot, outcomes and Explorer
//! latencies:
//! - **slow**: every network probe takes the full 5 s (the response limit of
//!   Req 9.3), so every network tab is pending for as long as it can be. Every
//!   local tab (a kept local saved tab, a local redirect, or the requested tab
//!   of a Targeted_Launch) of a window decided before the first network probe
//!   completes must be displayed (`Navigate` issued, or already shown by its
//!   new or initial tab) before that moment.
//! - **random**: network latencies anywhere in 1 ms – 5 s, so the network
//!   probes complete in any order, interleaved with local work. Up to its
//!   first network completion the run must be command-for-command identical
//!   to the slow run: a pending network probe, whatever its latency, never
//!   holds back (or otherwise changes) anything the session does.
//!
//! In both runs the final tabs of every window must be exactly its saved
//! tabs in saved order with omitted tabs removed and redirected tabs showing
//! their ancestor (plus the requested tab last for a Targeted trigger), as
//! computed independently from the probe outcomes with `choose_fallback`;
//! network tabs that turn out available therefore appear at their saved
//! positions. The final plan must say the same, with no placeholder left.
//!
//! The Explorer latencies are bounded so that all local work of a snapshot
//! (up to 3 windows of up to 5 tabs, tab creation serialized across windows)
//! fits well within 5 s.
//!
//! **Validates: Requirements 7.5, 7.9**

use std::collections::{BTreeMap, BTreeSet};

use esr_core::engine::{Command, Event, Millis, ProbeId, TabKey, WinKey};
use esr_core::model::{
    DisplayRef, LocationId, LocationKind, Placement, Rect, ShowState, Snapshot, TabRec, Volume, WindowId,
    WindowRec,
};
use esr_core::plan::fallback::{ancestors, choose_fallback, AncestorProbe, Fallback, ProbeOutcome};
use esr_core::plan::{LaunchInfo, Slot};
use esr_core::restore::{RestoreOutcome, RestoreParams, RestoreSession};
use esr_sim::explorer::{default_displays, default_locations, SimConfig, SimExplorer};
use esr_sim::restore::{home_location, Driver, Launch, Origin, Run, Stop};
use proptest::collection::vec;
use proptest::prelude::*;

const SEQ: u64 = 20;

/// Size of the simulator's location pool.
const POOL: usize = 12;
/// Network Locations of the pool (mapped drive, UNC share).
const NETWORK: [usize; 2] = [6, 7];
/// Local Locations of the pool.
const LOCAL: [usize; 10] = [0, 1, 2, 3, 4, 5, 8, 9, 10, 11];
/// The full response limit of a network probe (Req 9.3).
const NETWORK_LIMIT_MS: u64 = 5_000;

fn loc(i: usize) -> LocationId {
    default_locations()[i].clone()
}

fn pool_index(l: &LocationId) -> usize {
    default_locations().iter().position(|p| p == l).expect("pool location")
}

/// A network Location (a pool network Location or one of its ancestors).
fn is_network(l: &LocationId) -> bool {
    matches!(l.kind, LocationKind::FileSystem(Volume::Mapped) | LocationKind::FileSystem(Volume::Unc))
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

fn placement() -> impl Strategy<Value = Placement> {
    (show_state(), -300i32..3200, -200i32..900, 300i32..1600, 200i32..1000, 0usize..2).prop_map(
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

/// A saved tab: local more often than network.
fn tab() -> impl Strategy<Value = usize> {
    prop_oneof![
        3 => prop::sample::select(LOCAL.to_vec()),
        2 => prop::sample::select(NETWORK.to_vec()),
    ]
}

fn window() -> impl Strategy<Value = (Vec<usize>, prop::sample::Index, Placement)> {
    (vec(tab(), 1..=5), any::<prop::sample::Index>(), placement())
}

/// A valid snapshot with 1–3 windows, a random Activation_Order, and at least
/// one local and one network tab.
fn snapshot() -> impl Strategy<Value = Snapshot> {
    vec(window(), 1..=3)
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
        .prop_filter("mixes local and network tabs", |s| {
            let all = || s.windows.iter().flat_map(|w| &w.tabs);
            all().any(|t| is_network(&t.location)) && all().any(|t| !is_network(&t.location))
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
        2 => Just(Kind::Timeout),
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

/// One probe per pool Location: local ones answer within 300 ms, network
/// ones anywhere up to 5 s.
fn probes() -> impl Strategy<Value = Vec<Probe>> {
    let local = (kind(), 1u64..=300, vec(any::<bool>(), 4));
    let network = (kind(), 1u64..=NETWORK_LIMIT_MS, vec(any::<bool>(), 4));
    (vec(local, POOL), vec(network, NETWORK.len())).prop_map(|(local, network)| {
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

/// Explorer latencies, bounded so that local work fits well within 5 s.
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
    (50u64..=800, 0u64..=300, 20u64..=100, 10u64..=200, 50u64..=400, 1u64..=100).prop_map(
        |(window_open_ms, window_open_contention_ms, tab_register_ms, navigate_ms, close_tab_ms, write_ms)| {
            Latencies { window_open_ms, window_open_contention_ms, tab_register_ms, navigate_ms, close_tab_ms, write_ms }
        },
    )
}

#[derive(Clone, Debug)]
struct Case {
    snapshot: Snapshot,
    probes: Vec<Probe>,
    latencies: Latencies,
    /// `None`: Default_Launch on Home; `Some(i)`: Targeted_Launch on local
    /// pool Location `i`.
    targeted: Option<usize>,
}

fn case() -> impl Strategy<Value = Case> {
    (snapshot(), probes(), latencies(), prop::option::of(prop::sample::select(LOCAL.to_vec())))
        .prop_map(|(snapshot, probes, latencies, targeted)| Case { snapshot, probes, latencies, targeted })
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

/// One finished restoration.
struct Restored {
    session: RestoreSession,
    run: Run,
    outcome: Option<RestoreOutcome>,
    trigger: WinKey,
    trigger_tab: TabKey,
    trigger_location: LocationId,
    started: Millis,
    /// When the attempt-counter write completed.
    written: Option<Millis>,
    /// When each saved window was first planned as `decided`.
    decided_at: BTreeMap<WindowId, Millis>,
    /// When the first network probe completed.
    first_network: Option<Millis>,
}

/// Restores `case` with the network probes taking `network_latency(i)` ms
/// (`i` a network pool index); every other probe as programmed.
fn restore(case: &Case, network_latency: impl Fn(usize) -> u64) -> Restored {
    let mut sim = SimExplorer::new(config(&case.latencies));
    let (launch, trigger_location) = match case.targeted {
        None => (LaunchInfo::Default, home_location()),
        Some(i) => (LaunchInfo::Targeted { requested: loc(i) }, loc(i)),
    };
    sim.launch(&Launch {
        location: trigger_location.clone(),
        selection: vec![],
        rect: Rect { left: 200, top: 150, right: 1100, bottom: 800 },
        maximized: false,
    });
    sim.settle();
    let (trigger, trigger_tab) = {
        let w = &sim.windows()[0];
        (w.key, w.tabs[0].key)
    };
    for (i, p) in case.probes.iter().enumerate() {
        let location = loc(i);
        let latency = if NETWORK.contains(&i) { network_latency(i) } else { p.latency_ms };
        sim.set_probe(location.clone(), outcome_for(&location, p), latency);
    }

    let started = sim.now();
    let mut session = RestoreSession::new(
        started,
        RestoreParams {
            snapshot: case.snapshot.clone(),
            launch,
            displays: default_displays(),
            trigger,
            trigger_tab: Some(trigger_tab),
            trigger_location: Some(trigger_location.clone()),
            requested_selection: vec![],
            attempt_write_seq: SEQ,
            first_id: 9000,
        },
    );
    let mut decided_at: BTreeMap<WindowId, Millis> = BTreeMap::new();
    for pw in session.plan().windows.iter().filter(|w| w.decided) {
        decided_at.entry(pw.source).or_insert(started);
    }
    let mut driver = Driver::new(sim);
    driver.submit([Command::WriteSessionFile { seq: SEQ, text: "failed-attempts 1".into(), urgent: true }]);

    let mut outcome: Option<RestoreOutcome> = None;
    let mut written: Option<Millis> = None;
    let mut probe_locations: BTreeMap<ProbeId, LocationId> = BTreeMap::new();
    let mut first_network: Option<Millis> = None;
    let stop = driver.run(|now, ev| {
        match &ev {
            Event::WriteCompleted { seq, result: Ok(()) } if *seq == SEQ => {
                written.get_or_insert(now);
            }
            Event::ProbeCompleted { probe, .. } if probe_locations.get(probe).is_some_and(is_network) => {
                first_network.get_or_insert(now);
            }
            _ => {}
        }
        let out = session.handle(now, &ev);
        if let Some(o) = session.take_outcome() {
            outcome.get_or_insert(o);
        }
        for pw in session.plan().windows.iter().filter(|w| w.decided) {
            decided_at.entry(pw.source).or_insert(now);
        }
        for c in &out {
            if let Command::Probe { probe, location } = c {
                probe_locations.insert(*probe, location.clone());
            }
        }
        out
    });
    let run = driver.finish(stop);
    Restored {
        session,
        run,
        outcome,
        trigger,
        trigger_tab,
        trigger_location,
        started,
        written,
        decided_at,
        first_network,
    }
}

/// What each saved window must finally show, computed from the probe
/// outcomes alone: kept tabs in saved order (redirected ones as their
/// ancestor), plus the requested Location last on the trigger of a
/// Targeted_Launch. Also returns the saved window the trigger hosts.
fn expected(case: &Case) -> (BTreeMap<WindowId, Vec<LocationId>>, Option<WindowId>) {
    let mut out: BTreeMap<WindowId, Vec<LocationId>> = BTreeMap::new();
    for w in &case.snapshot.windows {
        let kept = w
            .tabs
            .iter()
            .filter_map(|t| {
                let p = &case.probes[pool_index(&t.location)];
                match choose_fallback(&t.location, &outcome_for(&t.location, p)) {
                    Fallback::Keep => Some(t.location.clone()),
                    Fallback::Redirected(a) => Some(a),
                    Fallback::Omitted(_) => None,
                }
            })
            .collect();
        out.insert(w.id, kept);
    }
    let trigger = case.snapshot.activation.iter().copied().find(|id| !out[id].is_empty());
    if let (Some(t), Some(i)) = (trigger, case.targeted) {
        out.get_mut(&t).expect("saved window").push(loc(i));
    }
    (out, trigger)
}

/// What the tabs of `win` show, left to right.
fn shown(r: &Restored, win: WinKey) -> Option<Vec<(TabKey, Option<LocationId>)>> {
    r.run.sim.window(win).map(|w| w.tabs.iter().map(|t| (t.key, t.last_known.clone())).collect())
}

/// Common checks of one run: it completed, and every window shows its
/// expected tabs in saved order (Req 7.9). Returns the violations.
fn check_completed_and_order(name: &str, case: &Case, r: &Restored) -> Vec<String> {
    let mut errors = Vec::new();
    if r.run.stop != Stop::Quiescent {
        errors.push(format!("{name}: run stopped: {:?}", r.run.stop));
    }
    if !matches!(r.outcome, Some(RestoreOutcome::Completed(_))) {
        errors.push(format!("{name}: not completed: {:?} / {:?}", r.outcome, r.session.phase()));
        return errors;
    }
    if r.written.is_none() {
        errors.push(format!("{name}: the attempt write never completed"));
    }
    if r.first_network.is_none() {
        errors.push(format!("{name}: no network probe completed"));
    }
    let (expected, trigger_source) = expected(case);
    let plan = r.session.plan();
    if trigger_source.is_none() {
        if !plan.windows.is_empty() {
            errors.push(format!("{name}: every window omitted, but planned {plan:?}"));
        }
        let got = shown(r, r.trigger).map(|ts| ts.into_iter().map(|t| t.1).collect::<Vec<_>>());
        if got != Some(vec![Some(r.trigger_location.clone())]) {
            errors.push(format!("{name}: empty plan, trigger shows {got:?}"));
        }
        return errors;
    }
    for w in &case.snapshot.windows {
        let want = &expected[&w.id];
        let win = r.session.window_for(w.id);
        if want.is_empty() {
            if plan.windows.iter().any(|pw| pw.source == w.id) {
                errors.push(format!("{name}: omitted window {:?} is planned", w.id));
            }
            continue;
        }
        let Some(win) = win else {
            errors.push(format!("{name}: window {:?} has no live window", w.id));
            continue;
        };
        if (Some(w.id) == trigger_source) != (win == r.trigger) {
            errors.push(format!("{name}: window {:?} on {win:?}, trigger {:?} hosts {trigger_source:?}", w.id, r.trigger));
        }
        let got: Option<Vec<Option<LocationId>>> = shown(r, win).map(|ts| ts.into_iter().map(|t| t.1).collect());
        let want_shown: Vec<Option<LocationId>> = want.iter().cloned().map(Some).collect();
        if got.as_ref() != Some(&want_shown) {
            errors.push(format!("{name}: window {:?} shows {got:?}, expected {want_shown:?}", w.id));
        }
        match plan.windows.iter().find(|pw| pw.source == w.id) {
            Some(pw) => {
                let targets: Vec<Option<&LocationId>> = pw
                    .slots
                    .iter()
                    .map(|s| match s {
                        Slot::Restore { location, .. } | Slot::Requested(location) => Some(location),
                        Slot::Placeholder { .. } => None,
                    })
                    .collect();
                let want_targets: Vec<Option<&LocationId>> = want.iter().map(Some).collect();
                if targets != want_targets {
                    errors.push(format!("{name}: window {:?} planned {:?}, expected {want:?}", w.id, pw.slots));
                }
                let indices: Vec<usize> = pw
                    .slots
                    .iter()
                    .filter_map(|s| match s {
                        Slot::Restore { tab_index, .. } | Slot::Placeholder { tab_index } => Some(*tab_index),
                        Slot::Requested(_) => None,
                    })
                    .collect();
                if !indices.windows(2).all(|p| p[0] < p[1]) {
                    errors.push(format!("{name}: window {:?} slots out of saved order: {:?}", w.id, pw.slots));
                }
            }
            None => errors.push(format!("{name}: window {:?} not in the final plan", w.id)),
        }
    }
    errors
}

/// When `tab` (a final tab of `win`) first showed `location`: its
/// `Navigate` to it was issued, a navigation to it completed (a new tab
/// showing Home), or it is the window's initial tab and the window opened
/// there.
fn first_display(r: &Restored, win: WinKey, tab: TabKey, location: &LocationId, created: &BTreeSet<TabKey>) -> Option<Millis> {
    let sim = &r.run.sim;
    let navigated = sim.command_log().iter().find_map(|c| match &c.command {
        Command::Navigate { tab: t, location: l, .. } if *t == tab && l == location => Some(c.at),
        _ => None,
    });
    let completed = r.run.events.iter().find_map(|e| match &e.event {
        Event::NavigationCompleted { tab: t, location: l } if *t == tab && l == location => Some(e.at),
        _ => None,
    });
    let initial = if win == r.trigger {
        (tab == r.trigger_tab && r.trigger_location == *location).then_some(r.started)
    } else if !created.contains(&tab) {
        match sim.window_record(win).map(|rec| rec.origin) {
            Some(Origin::Request(request)) => sim.command_log().iter().find_map(|c| match &c.command {
                Command::OpenWindow { request: q, location: l, .. } if *q == request && l == location => Some(c.at),
                _ => None,
            }),
            _ => None,
        }
    } else {
        None
    };
    [navigated, completed, initial].into_iter().flatten().min()
}

/// Req 7.5 on the slow run: every local tab of a window decided before the
/// first network probe completed was displayed before that moment.
fn check_local_tabs_do_not_wait(r: &Restored) -> Vec<String> {
    let mut errors = Vec::new();
    let Some(t_net) = r.first_network else { return errors };
    let created: BTreeSet<TabKey> = r
        .run
        .events
        .iter()
        .filter_map(|e| match &e.event {
            Event::TabCreated { tab, .. } => Some(*tab),
            _ => None,
        })
        .collect();
    for pw in &r.session.plan().windows {
        let Some(&decided) = r.decided_at.get(&pw.source) else {
            errors.push(format!("slow: planned window {:?} never decided", pw.source));
            continue;
        };
        if decided >= t_net {
            continue; // its trigger mapping waited for a network result
        }
        let Some(win) = r.session.window_for(pw.source) else { continue };
        let Some(tabs) = shown(r, win) else { continue };
        if tabs.len() != pw.slots.len() {
            continue; // reported by the order check
        }
        for (i, (slot, (tab, _))) in pw.slots.iter().zip(&tabs).enumerate() {
            let target = match slot {
                Slot::Restore { location, .. } | Slot::Requested(location) => location,
                Slot::Placeholder { .. } => continue,
            };
            if is_network(target) {
                continue;
            }
            match first_display(r, win, *tab, target, &created) {
                Some(at) if at < t_net => {}
                other => errors.push(format!(
                    "slow: window {:?} (decided at {decided:?}) slot {i} {:?}: local tab first shown at {other:?}, \
                     not before the first network probe completed at {t_net:?}",
                    pw.source,
                    target.display_name
                )),
            }
        }
    }
    errors
}

/// Runs both restorations of `case` and returns every violation found.
fn check(case: &Case) -> Vec<String> {
    let slow = restore(case, |_| NETWORK_LIMIT_MS);
    let random = restore(case, |i| case.probes[i].latency_ms);

    let mut errors = check_completed_and_order("slow", case, &slow);
    errors.extend(check_completed_and_order("random", case, &random));
    if !errors.is_empty() {
        return errors;
    }
    errors.extend(check_local_tabs_do_not_wait(&slow));

    // Up to the first network completion of the random run, both runs issue
    // exactly the same commands at the same times.
    let t_a = random.first_network.expect("checked");
    let before = |r: &Restored| -> Vec<(Millis, Command)> {
        r.run.sim.command_log().iter().filter(|c| c.at < t_a).map(|c| (c.at, c.command.clone())).collect()
    };
    let (a, s) = (before(&random), before(&slow));
    if a != s {
        let i = a.iter().zip(&s).position(|(x, y)| x != y).unwrap_or(a.len().min(s.len()));
        errors.push(format!(
            "random: before the first network completion at {t_a:?}, commands differ from the slow run at #{i}: \
             random {:?}, slow {:?}",
            a.get(i),
            s.get(i)
        ));
    }
    errors
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

    /// Property 20: Network probes do not delay or reorder local tabs.
    ///
    /// **Validates: Requirements 7.5, 7.9**
    #[test]
    fn network_probes_do_not_delay_or_reorder_local_tabs(case in case()) {
        let errors = check(&case);
        prop_assert!(errors.is_empty(), "{}", errors.join("\n"));
    }
}

/// A fixed mix: the first window starts with a network tab that resolves
/// last, the second has a network tab that times out between local tabs.
#[test]
fn interleaved_network_tabs_example() {
    let p = |left: i32| {
        let d = &default_displays()[usize::from(left >= 1920)];
        Placement {
            state: ShowState::Normal,
            restored: Rect { left, top: 40, right: left + 800, bottom: 640 },
            display: DisplayRef { device: d.device.clone(), monitor: d.monitor },
        }
    };
    let w = |id, tabs: &[usize], active, left| WindowRec {
        id: WindowId(id),
        tabs: tabs.iter().map(|&t| TabRec { location: loc(t) }).collect(),
        active,
        placement: p(left),
    };
    let snapshot = Snapshot {
        windows: vec![w(1, &[2, 7, 3, 6], 0, 100), w(2, &[4, 6, 9, 0], 2, 2000)],
        activation: vec![WindowId(1), WindowId(2)],
    };
    let available = |latency_ms| Probe { kind: Kind::Available, latency_ms, ancestors: vec![true; 4] };
    let mut probes: Vec<Probe> = (0..POOL).map(|_| available(20)).collect();
    probes[7] = available(4000);
    probes[6] = Probe { kind: Kind::Timeout, latency_ms: 1200, ancestors: vec![] };
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
        targeted: None,
    };
    let errors = check(&case);
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}
