// Feature: explorer-session-restore, Property 21: Omission reporting
//! Property test for the omission reporting of `RestoreSession` at
//! Restoration_Completion, driven against the simulated Explorer (`esr-sim`).
//!
//! **Property 21: Omission reporting.** For any restoration outcome:
//! - exactly one `TAB-OMITTED` or `TAB-REDIRECTED` log entry is emitted per
//!   omitted or redirected tab, each with a timestamp, saved Location, and
//!   exactly one reason, and redirected entries also give the ancestor;
//! - exactly one `Notify { omitted_tabs: n }` is emitted when *n* > 0, where
//!   *n* counts omitted tabs including those of omitted windows, and none is
//!   emitted when *n* = 0.
//!
//! The expected entries are computed independently from the snapshot and the
//! programmed probe outcomes with `choose_fallback`: one per saved tab whose
//! fate is `Omitted(reason)` or `Redirected(ancestor)`, in saved
//! Activation_Order and then tab order (tabs of omitted windows included).
//! Also checked: the `RESTORE` summary (exactly one) carries the matching
//! window, tab, omitted, and redirected counts; the entries and the
//! notification are issued in the completion batch and carry its time
//! (nothing is reported before completion, nothing twice); and the reported
//! `Completion` agrees.
//!
//! Inputs: random snapshots over the simulator's location pool (1–4 windows,
//! 1–5 tabs, duplicates allowed, random Activation_Order), random probe
//! outcomes of every kind (missing Locations with random ancestor
//! availability, so both redirects and `NotExist` omissions occur; missing
//! Shell and search Locations), random probe latencies (network Locations up
//! to 5 s), and Default and Targeted launches.
//!
//! **Validates: Requirements 7.4, 9.7, 9.8**

use esr_core::engine::{Command, Millis};
use esr_core::logfmt::entry::LogEntry;
use esr_core::model::{DisplayRef, LocationId, Placement, Rect, ShowState, Snapshot, TabRec, WindowId, WindowRec};
use esr_core::plan::fallback::{ancestors, choose_fallback, AncestorProbe, Fallback, OmitReason, ProbeOutcome};
use esr_core::plan::LaunchInfo;
use esr_core::restore::{RestoreOutcome, RestoreParams, RestoreSession};
use esr_sim::explorer::{default_displays, default_locations, SimConfig, SimExplorer};
use esr_sim::restore::{home_location, Driver, Launch, Stop};
use proptest::collection::vec;
use proptest::prelude::*;

const SEQ: u64 = 21;
/// Every pool Location is usable (no user windows here).
const POOL: usize = 12;
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
    ]
}

fn placement() -> impl Strategy<Value = Placement> {
    (show_state(), -200i32..3000, 0i32..700, 400i32..1400, 300i32..900, 0usize..2).prop_map(
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

/// A valid snapshot with 1–4 windows of 1–5 tabs and a random
/// Activation_Order.
fn snapshot() -> impl Strategy<Value = Snapshot> {
    vec((vec(0..POOL, 1..=5), any::<prop::sample::Index>(), placement()), 1..=4)
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

#[derive(Clone, Copy, Debug)]
enum Kind {
    Available,
    Missing,
    AccessDenied,
    Timeout,
    Unresolvable,
    ShellUnavailable,
}

/// Omissions are frequent, so whole windows (and whole snapshots) are often
/// omitted.
fn kind() -> impl Strategy<Value = Kind> {
    prop_oneof![
        4 => Just(Kind::Available),
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

#[derive(Clone, Debug)]
struct Case {
    snapshot: Snapshot,
    probes: Vec<Probe>,
    /// `None`: Default_Launch; `Some(i)`: Targeted_Launch of pool entry `i`.
    targeted: Option<usize>,
}

fn case() -> impl Strategy<Value = Case> {
    (snapshot(), probes(), prop::option::of(0..POOL))
        .prop_map(|(snapshot, probes, targeted)| Case { snapshot, probes, targeted })
}

// ---------------------------------------------------------------------------
// Expected report
// ---------------------------------------------------------------------------

struct Expected {
    /// `TAB-OMITTED` / `TAB-REDIRECTED` entries in the documented order.
    entries: Vec<LogEntry>,
    omitted: u32,
    redirected: u32,
    /// Saved windows with at least one tab not omitted.
    windows: u32,
    /// Saved tabs not omitted (restored as saved or redirected).
    tabs: u32,
}

fn expected(case: &Case) -> Expected {
    let fate = |l: &LocationId| {
        let i = default_locations().iter().position(|p| p == l).expect("pool location");
        choose_fallback(l, &outcome_for(l, &case.probes[i]))
    };
    let mut e = Expected { entries: vec![], omitted: 0, redirected: 0, windows: 0, tabs: 0 };
    for id in &case.snapshot.activation {
        let w = case.snapshot.windows.iter().find(|w| w.id == *id).expect("window of the order");
        let mut kept = 0;
        for t in &w.tabs {
            match fate(&t.location) {
                Fallback::Keep => kept += 1,
                Fallback::Redirected(ancestor) => {
                    kept += 1;
                    e.redirected += 1;
                    e.entries.push(LogEntry::TabRedirected {
                        location: t.location.clone(),
                        reason: OmitReason::NotExist,
                        ancestor,
                    });
                }
                Fallback::Omitted(reason) => {
                    e.omitted += 1;
                    e.entries.push(LogEntry::TabOmitted { location: t.location.clone(), reason });
                }
            }
        }
        e.tabs += kept;
        e.windows += u32::from(kept > 0);
    }
    e
}

fn is_report(c: &Command) -> bool {
    matches!(
        c,
        Command::Notify { .. }
            | Command::AppendLog(LogEntry::Restore { .. } | LogEntry::TabOmitted { .. } | LogEntry::TabRedirected { .. })
    )
}

// ---------------------------------------------------------------------------
// Running a restoration
// ---------------------------------------------------------------------------

/// Runs the restoration of `case` and returns every violation found.
fn check(case: &Case) -> Vec<String> {
    let mut errors: Vec<String> = Vec::new();

    let mut sim = SimExplorer::new(SimConfig::default());
    let (launch, trigger_location) = match case.targeted {
        None => (LaunchInfo::Default, home_location()),
        Some(i) => (LaunchInfo::Targeted { requested: loc(i) }, loc(i)),
    };
    sim.launch(&Launch {
        location: trigger_location.clone(),
        selection: vec![],
        rect: Rect { left: 300, top: 300, right: 1100, bottom: 900 },
        maximized: false,
    });
    sim.settle();
    let (trigger, trigger_tab) = {
        let w = &sim.windows()[0];
        (w.key, w.tabs[0].key)
    };
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
            launch,
            displays: default_displays(),
            trigger,
            trigger_tab: Some(trigger_tab),
            trigger_location: Some(trigger_location),
            requested_selection: vec![],
            attempt_write_seq: SEQ,
            first_id: 9000,
        },
    );
    let mut driver = Driver::new(sim);
    driver.submit([Command::WriteSessionFile { seq: SEQ, text: "failed-attempts 1".into(), urgent: true }]);

    let mut outcome: Option<RestoreOutcome> = None;
    let mut done_at: Option<Millis> = None;
    let mut reports: Vec<Command> = Vec::new();
    let stop = driver.run(|now, ev| {
        let out = session.handle(now, &ev);
        let completed_now = match session.take_outcome() {
            Some(o) => {
                if outcome.is_some() {
                    errors.push("outcome reported twice".into());
                }
                let completed = matches!(o, RestoreOutcome::Completed(_));
                if completed {
                    done_at = Some(now);
                }
                outcome = Some(o);
                completed
            }
            None => false,
        };
        let batch: Vec<Command> = out.iter().filter(|c| is_report(c)).cloned().collect();
        if !batch.is_empty() {
            if !completed_now {
                errors.push(format!("{now:?}: omission report outside the completion batch: {batch:?}"));
            }
            reports.extend(batch);
        }
        out
    });
    let run = driver.finish(stop);
    let sim = &run.sim;
    if run.stop != Stop::Quiescent {
        errors.push(format!("run stopped: {:?}", run.stop));
    }
    let completion = match &outcome {
        Some(RestoreOutcome::Completed(c)) => c.clone(),
        other => {
            errors.push(format!("not completed: {other:?} / {:?}", session.phase()));
            return errors;
        }
    };
    let done_at = done_at.expect("completion time");

    let exp = expected(case);

    // Req 9.7: one entry per omitted or redirected saved tab, in order, with
    // the right Location, reason, and ancestor; each carries a timestamp
    // (the completion time).
    let tab_entries: Vec<&(Millis, LogEntry)> = sim
        .log_entries()
        .iter()
        .filter(|(_, e)| matches!(e, LogEntry::TabOmitted { .. } | LogEntry::TabRedirected { .. }))
        .collect();
    let got: Vec<&LogEntry> = tab_entries.iter().map(|(_, e)| e).collect();
    let want: Vec<&LogEntry> = exp.entries.iter().collect();
    if got != want {
        errors.push(format!("tab entries\n  got  {got:?}\n  want {want:?}"));
    }
    if let Some((at, e)) = tab_entries.iter().find(|(at, _)| *at != done_at) {
        errors.push(format!("entry {e:?} at {at:?}, completion at {done_at:?}"));
    }

    // Exactly one RESTORE summary, with matching counts (Req 13.2).
    let restores: Vec<&(Millis, LogEntry)> =
        sim.log_entries().iter().filter(|(_, e)| matches!(e, LogEntry::Restore { .. })).collect();
    match restores.as_slice() {
        [(at, LogEntry::Restore { targeted, windows, tabs, omitted, redirected, .. })] => {
            let got = (*targeted, *windows, *tabs, *omitted, *redirected);
            let want = (case.targeted.is_some(), exp.windows, exp.tabs, exp.omitted, exp.redirected);
            if got != want {
                errors.push(format!("RESTORE (targeted, windows, tabs, omitted, redirected) {got:?}, want {want:?}"));
            }
            if *at != done_at {
                errors.push(format!("RESTORE at {at:?}, completion at {done_at:?}"));
            }
        }
        other => errors.push(format!("{} RESTORE entries: {other:?}", other.len())),
    }
    if let Some(e) = sim.log_entries().iter().find(|(_, e)| matches!(e, LogEntry::Error { .. })) {
        errors.push(format!("unexpected ERROR entry {e:?}"));
    }
    if (completion.omitted, completion.redirected, completion.windows, completion.tabs)
        != (exp.omitted, exp.redirected, exp.windows, exp.tabs)
    {
        errors.push(format!(
            "Completion (omitted, redirected, windows, tabs) {:?}, want {:?}",
            (completion.omitted, completion.redirected, completion.windows, completion.tabs),
            (exp.omitted, exp.redirected, exp.windows, exp.tabs)
        ));
    }

    // Req 9.8 / 7.4: one notification counting every omitted tab, at
    // completion, only when something was omitted.
    let want_notes: Vec<(Millis, u32)> = if exp.omitted > 0 { vec![(done_at, exp.omitted)] } else { vec![] };
    if sim.notifications() != want_notes.as_slice() {
        errors.push(format!("notifications {:?}, want {want_notes:?}", sim.notifications()));
    }

    // The session issued each report once (what the sim recorded is what
    // the completion batch held).
    let logged = sim.log_entries().len();
    let batch_logs = reports.iter().filter(|c| matches!(c, Command::AppendLog(_))).count();
    if batch_logs != 1 + exp.entries.len() || logged != batch_logs {
        errors.push(format!(
            "{batch_logs} log commands in the completion batch, {logged} recorded, want {}",
            1 + exp.entries.len()
        ));
    }

    errors
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

    /// Property 21: Omission reporting.
    ///
    /// **Validates: Requirements 7.4, 9.7, 9.8**
    #[test]
    fn omission_reporting(case in case()) {
        let errors = check(&case);
        prop_assert!(errors.is_empty(), "{}", errors.join("\n"));
    }
}

/// Every saved tab unavailable: every window is omitted, every tab is
/// reported (Req 9.5, 9.8), and the one notification counts them all.
#[test]
fn everything_omitted_reports_every_tab() {
    let unavailable = |kind| Probe { kind, latency_ms: 20, ancestors: vec![false; 4] };
    let mut probes: Vec<Probe> = (0..POOL).map(|_| unavailable(Kind::AccessDenied)).collect();
    probes[2] = unavailable(Kind::Missing);
    probes[7] = Probe { kind: Kind::Timeout, latency_ms: 4800, ancestors: vec![] };
    probes[9] = unavailable(Kind::ShellUnavailable);
    let p = Placement {
        state: ShowState::Normal,
        restored: Rect { left: 100, top: 40, right: 900, bottom: 640 },
        display: DisplayRef { device: default_displays()[0].device.clone(), monitor: default_displays()[0].monitor },
    };
    let w = |id, tabs: &[usize]| WindowRec {
        id: WindowId(id),
        tabs: tabs.iter().map(|&t| TabRec { location: loc(t) }).collect(),
        active: 0,
        placement: p.clone(),
    };
    let case = Case {
        snapshot: Snapshot { windows: vec![w(1, &[2, 7]), w(2, &[9, 2, 3])], activation: vec![WindowId(2), WindowId(1)] },
        probes,
        targeted: None,
    };
    assert_eq!(expected(&case).omitted, 5);
    assert_eq!(expected(&case).windows, 0);
    let errors = check(&case);
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}
