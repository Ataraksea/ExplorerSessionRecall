//! Property test for the failure chain of a fold, end to end: the `Engine`
//! (with its `FoldingEngine`) against the simulated Explorer, with generated
//! failure schedules.
//!
// Feature: explorer-session-restore, Property 38: The failure chain ends with the candidate folded or left open
//!
//! **Property 38: The failure chain ends with the candidate folded or left open**
//!
//! *For any* candidate and *any* injected failure schedule (no tab or a late
//! tab, unresponsive or closing windows, Fold_Target closed at any step,
//! Location not shown within 3 s, Tab_Switch not confirmed, Matching_Tab or its
//! window closed, a fold into the Foreground_Explorer_Window failing, first
//! navigation timeout at 8 s, folding turned off), the candidate ends in
//! exactly one of:
//!
//! - **folded**: closed by the fold, with a Folded_Tab or Matching_Tab active
//!   in a window on its desktop (one `FOLD` entry whose outcome is not
//!   `not-folded`);
//! - **left open**: revealed, with its own single tab, Location, selection and
//!   Window_Placement (one `FOLD` entry `not-folded`; no entry only when its
//!   first navigation never completed in time, so that its Location is not
//!   known).
//!
//! In both cases no tab the fold created is left behind: when the candidate is
//! left open, every tab created for it (including one appearing up to 10 s
//! late) is closed; when it is folded, the Folded_Tab is the only new tab.
//! The engine never closes a tab that existed before the candidate (so no
//! Matching_Tab) and never closes a window other than the candidate. After a
//! failed Tab_Switch no new Matching_Tab is searched (no second Tab_Switch) and
//! the next target is the Matching_Tab's window, unless that window has
//! been disturbed; no window is made Fold_Target twice. Every path ends within
//! the sum of its step timers.
//!
//! # Scenarios
//!
//! 1–3 windows of 1–4 tabs (some maximized, some minimized), then 1–2
//! candidates, one after the other (each ends before the next opens, so a
//! later candidate meets what an earlier one left): a candidate opened from
//! another application or from a window that is in the foreground, or Ctrl+N
//! on the foreground window. The simulator gets a failure schedule from
//! `esr_sim::fold::failure_schedule` (failed tab creations, unconfirmed
//! activations, windows that close while they are a target, candidates that
//! ignore `CloseWindow` once), the first-navigation delay (up to 9 s, past the
//! limit of 8 s) and the registration lag. Registrations of the schedule that
//! are slow are made slower still, so that a Location is not shown within 3 s.
//! Each candidate also gets, at generated moments while its fold is in
//! progress, disturbances by the user or the system: a window begins closing,
//! the Matching_Tab of a started Tab_Switch is closed, a window hangs for a
//! while, folding is turned off; and some windows hang before it opens.
//!
//! # Findings
//!
//! - **BUG-1** (production, fixed): a Fold_Target that begins closing while
//!   its fold finishes was not noticed, and the candidate was closed with its
//!   folder lost. `FoldingEngine::tab_revoked` now leaves a candidate open
//!   (`fold-target-closed`) when its Folded_Tab is revoked; the strict example
//!   is `bug_1_a_fold_target_that_begins_closing_loses_the_folded_tab`.
//! - Simulator artifacts the generators stay clear of: a `Reveal` within 60 ms
//!   of the first signal overtakes the pending `HideWindow` (disturbances and
//!   first navigations start at 100 ms); `ActivateMode::Late` selects the
//!   Matching_Tab after the fold's own, later `ActivateTab` (made
//!   `Unconfirmed`); `Op::CloseTab` of a window's last tab runs the simulator
//!   ahead of the engine (not applied).
//!
//! `ignore_close` ordinals are left out of the schedule: a candidate that
//! ignores `CloseWindow` twice is handed over after its fold completed
//! (`ERROR operation=fold-close`), which is an error path of Req 14.1, not of
//! this chain.
//!
//! **Validates: Requirements 14.13, 14.14, 14.22, 14.23, 14.24**

use std::collections::BTreeSet;

use esr_core::engine::{
    Command, Engine, Environment, Event, ExplorerOptionValues, ItemId, Millis, SessionFileRead, StartupInput, TabKey,
    TimerOwner, WinKey,
};
use esr_core::fold::{
    FoldOutcome, FOLD_ACTIVATE_MS, FOLD_ACTIVATE_RETRIES, FOLD_CLOSE_RETRY_MS, FOLD_FIRST_NAV_MS, FOLD_SWITCH_MS,
    FOLD_TAB_CREATE_MS, FOLD_TAB_SHOW_MS,
};
use esr_core::logfmt::entry::{fold_outcome_token, LogEntry};
use esr_core::model::Rect;
use esr_core::settings::Settings;
use esr_sim::explorer::{default_displays, NavOutcome, Op, Pick, ProcessChoice, SimConfig, SimExplorer};
use esr_sim::fold::{
    failure_schedule, selection_strategy, ActivateMode, CreateTabFault, DesktopSpec, FoldConfig, FoldFaults,
    TabWindowFault,
};
use esr_sim::restore::{CommandKind, Driver, Stop};
use proptest::collection::vec;
use proptest::prelude::*;

/// Allowance for the simulator's own latencies (tab window, registration,
/// navigation, scans, `CloseWindow`) on top of the step timers.
const SLACK_MS: u64 = 600;

/// The `FoldContext` probe of the candidate.
const PROBE_MS: u64 = 100;

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

/// One window of the tracked session.
#[derive(Clone, Debug)]
struct WinSpec {
    tabs: Vec<u16>,
    active: u16,
    maximized: bool,
    minimized: bool,
}

/// How a candidate opens.
#[derive(Clone, Debug)]
enum Step {
    Open { location: u16, selection: Vec<ItemId>, foreground: Option<u16> },
    CtrlN,
}

/// What happens to the windows while a fold is in progress.
#[derive(Clone, Debug)]
enum Kind {
    /// The window begins closing (`WindowDestroyed` follows 250 ms later).
    Closing,
    /// The Matching_Tab of a Tab_Switch that has started closes (nothing
    /// happens if no Tab_Switch is under way).
    CloseMatching,
    /// The window stops answering `WM_NULL` for a while.
    Hang { for_ms: u64 },
    /// Folding is turned off in Settings.
    FoldingOff,
}

#[derive(Clone, Debug)]
struct Disturbance {
    /// Time after the candidate's first signal.
    at_ms: u64,
    /// Pick among the windows open before the candidate.
    win: u16,
    kind: Kind,
}

#[derive(Clone, Debug)]
struct Round {
    step: Step,
    /// Picks of windows that hang before the candidate opens.
    hung: Vec<u16>,
    disturbances: Vec<Disturbance>,
}

#[derive(Clone, Debug)]
struct Scenario {
    windows: Vec<WinSpec>,
    rounds: Vec<Round>,
    faults: FoldFaults,
    first_navigation_ms: u64,
    registration_lag_ms: u64,
}

const LOCATIONS: u16 = 6;

fn win_spec() -> impl Strategy<Value = WinSpec> {
    (vec(0..LOCATIONS, 1..=4), any::<u16>(), prop::bool::weighted(0.2), prop::bool::weighted(0.25))
        .prop_map(|(tabs, active, maximized, minimized)| WinSpec { tabs, active, maximized, minimized })
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        5 => (0..LOCATIONS, selection_strategy(), proptest::option::weighted(0.6, any::<u16>()))
            .prop_map(|(location, selection, foreground)| Step::Open { location, selection, foreground }),
        2 => Just(Step::CtrlN),
    ]
}

fn disturbance() -> impl Strategy<Value = Disturbance> {
    let kind = prop_oneof![
        3 => Just(Kind::Closing),
        3 => Just(Kind::CloseMatching),
        3 => (200u64..=4_000).prop_map(|for_ms| Kind::Hang { for_ms }),
        1 => Just(Kind::FoldingOff),
    ];
    // Not before 100 ms: the simulator's `HideWindow` takes effect 60 ms after
    // the command whereas a `Reveal` takes effect sooner, so a candidate
    // revealed within 60 ms of its first signal would end hidden. (The real
    // CandidateHider runs both on one thread, in order.)
    (100u64..=2_500, any::<u16>(), kind).prop_map(|(at_ms, win, kind)| Disturbance { at_ms, win, kind })
}

fn round() -> impl Strategy<Value = Round> {
    (
        step(),
        prop_oneof![3 => Just(Vec::new()), 1 => vec(any::<u16>(), 1..=2)],
        prop_oneof![2 => Just(Vec::new()), 3 => vec(disturbance(), 1..=2)],
    )
        .prop_map(|(step, hung, disturbances)| Round { step, hung, disturbances })
}

/// A failure schedule with the ordinals of the first 8 commands, without
/// `ignore_close` and with the slow registrations made slower (beyond the 3 s
/// a Folded_Tab may take to show its Location).
fn faults() -> impl Strategy<Value = FoldFaults> {
    prop_oneof![1 => Just(FoldFaults::none()), 4 => failure_schedule(8)].prop_map(|mut faults| {
        faults.ignore_close.clear();
        // A late activation takes effect after the fold's own, later
        // `ActivateTab` and selects the Matching_Tab again; Explorer's window
        // thread handles the two selections in order, so this is left out.
        for fault in faults.activate_tab.values_mut() {
            if matches!(fault.mode, ActivateMode::Late { .. }) {
                fault.mode = ActivateMode::Unconfirmed;
            }
        }
        for fault in faults.create_tab.values_mut() {
            if fault.registration_delay_ms >= 1_500 {
                fault.registration_delay_ms += 2_000;
            }
        }
        faults
    })
}

fn scenario() -> impl Strategy<Value = Scenario> {
    (
        vec(win_spec(), 1..=3),
        vec(round(), 1..=2),
        faults(),
        // (Not below 100 ms, for the reason given at `disturbance`.)
        prop_oneof![5 => Just(150u64), 2 => 100u64..=1_500, 1 => Just(9_000u64)],
        prop_oneof![3 => Just(0u64), 1 => 0u64..400],
    )
        .prop_map(|(windows, rounds, faults, first_navigation_ms, registration_lag_ms)| Scenario {
            windows,
            rounds,
            faults,
            first_navigation_ms,
            registration_lag_ms,
        })
}

fn setup_ops(windows: &[WinSpec]) -> Vec<Op> {
    let mut ops = Vec::new();
    for (i, w) in windows.iter().enumerate() {
        let n = i as i32;
        ops.push(Op::OpenWindow {
            process: ProcessChoice::Main,
            location: Pick(w.tabs[0]),
            nav: NavOutcome::Complete,
            rect: Rect { left: 100 + 60 * n, top: 80 + 40 * n, right: 900 + 60 * n, bottom: 680 + 40 * n },
            maximized: w.maximized,
        });
        for &location in &w.tabs[1..] {
            ops.push(Op::OpenTab { win: Pick(i as u16), location: Pick(location), nav: NavOutcome::Complete });
        }
        ops.push(Op::ActivateTab { win: Pick(i as u16), tab: Pick(w.active) });
    }
    for (i, w) in windows.iter().enumerate() {
        if w.minimized {
            ops.push(Op::Minimize { win: Pick(i as u16) });
        }
    }
    ops
}

fn candidate_op(step: &Step) -> Op {
    match step {
        Step::Open { location, selection, foreground } => Op::OpenCandidate {
            location: Pick(*location),
            selection: selection.clone(),
            bypass_keys: false,
            desktop: DesktopSpec::Current,
            foreground: foreground.map(Pick),
            process: ProcessChoice::Main,
        },
        Step::CtrlN => Op::NewWindow { bypass_keys: false },
    }
}

fn sorted(items: &[ItemId]) -> Vec<ItemId> {
    let mut v = items.to_vec();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

/// What the run saw, for the coverage example.
#[derive(Clone, Debug, Default)]
struct Seen {
    /// The `outcome=` token of every `FOLD` entry (`not-folded:<reason>` for
    /// a candidate that was left open).
    outcomes: Vec<String>,
    /// Candidates that ended without a `FOLD` entry (first navigation too late).
    unlogged: usize,
}

fn token(outcome: &FoldOutcome) -> String {
    match outcome {
        FoldOutcome::NotFolded { reason, switch_reason } => format!("not-folded:{reason:?}:{}", switch_reason.is_some()),
        other => fold_outcome_token(other).to_string(),
    }
}

fn drive(driver: &mut Driver, engine: &mut Engine) -> Stop {
    driver.run(|now, event| engine.handle(now, event))
}

/// Runs to `until`, with the simulator's clock at `until` afterwards (if the
/// run has not ended before).
fn drive_to(driver: &mut Driver, engine: &mut Engine, until: Millis) -> Result<Stop, TestCaseError> {
    driver.set_deadline(until);
    let mut stop = drive(driver, engine);
    prop_assert_ne!(stop, Stop::EventLimit);
    let now = driver.sim().now();
    if stop == Stop::Deadline && now < until {
        let events = driver.sim_mut().tick(until.since(now));
        driver.deliver(events);
        stop = drive(driver, engine);
        prop_assert_ne!(stop, Stop::EventLimit);
    }
    Ok(stop)
}

fn run(sc: &Scenario) -> Result<Seen, TestCaseError> {
    let mut sim = SimExplorer::new(SimConfig {
        fold: FoldConfig {
            report_tab_started: true,
            first_navigation_ms: Some(sc.first_navigation_ms),
            registration_lag_ms: sc.registration_lag_ms,
            faults: sc.faults.clone(),
            ..FoldConfig::default()
        },
        ..SimConfig::default()
    });
    let mut events = Vec::new();
    for op in setup_ops(&sc.windows) {
        events.extend(sim.apply(&op));
    }
    events.extend(sim.settle());

    let startup = StartupInput {
        session_file: SessionFileRead::Absent,
        environment: Environment {
            displays: default_displays(),
            options: ExplorerOptionValues { persist_browsers: Some(false), ..ExplorerOptionValues::default() },
            ..Environment::default()
        },
        background_logon: false,
        logon_at: None,
        shell_ready_at: Some(Millis(0)),
    };
    let (mut engine, commands) = Engine::new(Settings::default(), startup);
    let mut driver = Driver::new(sim);
    for owner in [TimerOwner::Store, TimerOwner::Control] {
        driver.quiet_timers(owner);
    }
    driver.submit(commands);
    driver.deliver(events);
    prop_assert_eq!(drive(&mut driver, &mut engine), Stop::Quiescent);
    driver.deliver_now(Event::ShellAvailable);
    prop_assert_eq!(drive(&mut driver, &mut engine), Stop::Quiescent);

    let mut seen = Seen::default();
    for r in &sc.rounds {
        round_checks(sc, r, &mut driver, &mut engine, &mut seen)?;
    }
    Ok(seen)
}

/// A disturbance as it is played: the expansion of a `Hang` has an end.
#[derive(Clone, Debug)]
enum Act {
    Closing(WinKey),
    CloseMatching,
    Hang(WinKey),
    Unhang(WinKey),
    FoldingOff,
}

fn round_checks(
    sc: &Scenario,
    round: &Round,
    driver: &mut Driver,
    engine: &mut Engine,
    seen: &mut Seen,
) -> Result<(), TestCaseError> {
    // With no window left (a schedule that closed them all after a fold) the
    // next window is a Trigger_Window, not a Folding_Candidate (Req 14.3 a).
    if driver.sim().windows().is_empty() {
        return Ok(());
    }
    // What exists before the candidate.
    let initial_windows: Vec<WinKey> = driver.sim().windows().iter().map(|w| w.key).collect();
    let initial_tabs: BTreeSet<TabKey> =
        driver.sim().windows().iter().flat_map(|w| w.tabs.iter().map(|t| t.key)).collect();
    let cmd_start = driver.sim().command_log().len();
    let log_start = driver.sim().log_entries().len();
    let pick_window = |pick: u16| Pick(pick).of(initial_windows.len()).map(|i| initial_windows[i]);

    let mut touched: BTreeSet<WinKey> = BTreeSet::new();
    for &pick in &round.hung {
        if let Some(win) = pick_window(pick) {
            driver.sim_mut().set_unresponsive(win, true);
            touched.insert(win);
        }
    }

    // The candidate opens.
    let before = driver.sim().candidates().len();
    let events = driver.sim_mut().apply(&candidate_op(&round.step));
    if driver.sim().candidates().len() == before {
        // Ctrl+N with an application in the foreground: nothing opened.
        for win in &initial_windows {
            driver.sim_mut().set_unresponsive(*win, false);
        }
        prop_assert_eq!(drive(driver, engine), Stop::Quiescent);
        return Ok(());
    }
    let record = driver.sim().candidates().last().cloned().expect("a candidate");
    let cand = record.win;
    let placement0 = driver.sim().window(cand).expect("the candidate is open").placement.clone();
    let desktop0 = driver.sim().desktop_of(cand);
    let first_signal = record.first_signal;
    driver.deliver(events);

    // The disturbances, in time order.
    let mut acts: Vec<(u64, Act)> = Vec::new();
    for d in &round.disturbances {
        let Some(win) = pick_window(d.win) else { continue };
        match &d.kind {
            Kind::Closing => {
                touched.insert(win);
                acts.push((d.at_ms, Act::Closing(win)));
            }
            Kind::CloseMatching => acts.push((d.at_ms, Act::CloseMatching)),
            Kind::Hang { for_ms } => {
                touched.insert(win);
                acts.push((d.at_ms, Act::Hang(win)));
                acts.push((d.at_ms + for_ms, Act::Unhang(win)));
            }
            Kind::FoldingOff => acts.push((d.at_ms, Act::FoldingOff)),
        }
    }
    acts.sort_by_key(|(at, _)| *at);

    let mut folding_off = false;
    for (at, act) in acts {
        let until = first_signal + at;
        let stop = drive_to(driver, engine, until)?;
        // The fold is over, or its candidate has been told to close: nobody
        // disturbs it any more.
        let close_requested = driver
            .sim()
            .command_log()[cmd_start..]
            .iter()
            .any(|r| matches!(&r.command, Command::CloseWindow { win } if *win == cand));
        if stop == Stop::Quiescent || close_requested {
            break;
        }
        match act {
            Act::Closing(win) => {
                if let Some(i) = driver.sim().windows().iter().position(|w| w.key == win) {
                    let events = driver.sim_mut().apply(&Op::BeginClosing { win: Pick(i as u16) });
                    driver.deliver(events);
                }
            }
            Act::CloseMatching => {
                // The user closes the Matching_Tab of a Tab_Switch that has
                // started (Req 14.23). (Closing another tab of a Fold_Target
                // moves the indices of the others: not part of this chain.)
                let switch = driver.sim().command_log()[cmd_start..]
                    .iter()
                    .map_while(|r| match &r.command {
                        Command::CreateTab { .. } => None,
                        Command::ActivateTab { win, index, .. } => Some(Some((*win, *index))),
                        _ => Some(None),
                    })
                    .flatten()
                    .next();
                if let Some((win, index)) = switch
                    && let Some(i) = driver.sim().windows().iter().position(|w| w.key == win)
                    && driver.sim().windows()[i].tabs.len() > 1
                    && index < driver.sim().windows()[i].tabs.len()
                    && initial_tabs.contains(&driver.sim().windows()[i].tabs[index].key)
                {
                    // Closing a window's only tab is `Closing`: the simulator
                    // would run ahead of the engine until it is idle.
                    let op = Op::CloseTab { win: Pick(i as u16), tab: Pick(index as u16), destroy_gap_ms: 40 };
                    let events = driver.sim_mut().apply(&op);
                    driver.deliver(events);
                }
            }
            Act::Hang(win) => driver.sim_mut().set_unresponsive(win, true),
            Act::Unhang(win) => driver.sim_mut().set_unresponsive(win, false),
            Act::FoldingOff => {
                folding_off = true;
                driver.deliver_now(Event::SettingsChanged(Settings {
                    tab_folding_enabled: false,
                    ..Settings::default()
                }));
            }
        }
    }

    // Everything runs out; windows that still hang answer again and what they
    // held back happens (a late tab is closed if it appears up to 10 s late).
    driver.set_deadline(driver.sim().now() + 600_000);
    prop_assert_eq!(drive(driver, engine), Stop::Quiescent);
    let open: Vec<WinKey> = driver.sim().windows().iter().map(|w| w.key).collect();
    for win in open {
        driver.sim_mut().set_unresponsive(win, false);
    }
    prop_assert_eq!(drive(driver, engine), Stop::Quiescent);
    if folding_off {
        driver.deliver_now(Event::SettingsChanged(Settings::default()));
        prop_assert_eq!(drive(driver, engine), Stop::Quiescent);
    }

    // -----------------------------------------------------------------------
    // The checks
    // -----------------------------------------------------------------------
    let closes_targets = sc.faults.create_tab.values().any(|f| f.target_closes_after_ms.is_some())
        || sc.faults.activate_tab.values().any(|f| f.target_closes_after_ms.is_some());    let sim = driver.sim();
    prop_assert_eq!(engine.folding().candidates_in_progress(), 0, "a fold is still in progress");
    let commands = &sim.command_log()[cmd_start..];

    // The FOLD entry.
    let folds: Vec<&(Millis, LogEntry)> =
        sim.log_entries()[log_start..].iter().filter(|(_, e)| matches!(e, LogEntry::Fold { .. })).collect();
    prop_assert!(folds.len() <= 1, "more than one FOLD entry: {folds:?}");
    let outcome = match folds.first() {
        Some((_, LogEntry::Fold { location, outcome })) => {
            prop_assert_eq!(location.fold_key(), record.location.fold_key(), "the FOLD entry names another Location");
            Some(outcome.clone())
        }
        _ => None,
    };

    // Exactly one of folded and left open.
    let closed = sim.window(cand).is_none();
    let left_open = !closed;

    // When it ended (a closed candidate: its destruction; a candidate left
    // open: the `Reveal`).
    let end = if closed {
        sim.window_record(cand).and_then(|r| r.closed_at).expect("the candidate closed")
    } else {
        let revealed = commands.iter().find(|r| matches!(&r.command, Command::Reveal { win } if *win == cand));
        let Some(revealed) = revealed else {
            let kinds: Vec<String> = commands
                .iter()
                .map(|r| format!("{:?}@{}", CommandKind::of(&r.command), r.at.since(first_signal)))
                .collect();
            return Err(TestCaseError::fail(format!(
                "candidate {} is open but was never revealed; outcome {outcome:?}; folds {folds:?}; commands {kinds:?}",
                cand.seq
            )));
        };
        revealed.at
    };
    let took = end.since(first_signal);

    // What happened, for failure messages.
    let story = || {
        let list: Vec<String> = commands
            .iter()
            .filter(|r| !matches!(r.command, Command::SetTimer { .. } | Command::WriteSessionFile { .. }))
            .map(|r| {
                let detail = match &r.command {
                    Command::CreateTab { win, .. } | Command::ActivateTab { win, .. } => format!("{}", win.seq),
                    Command::CloseTab { tab, .. } => format!("tab{}", tab.seq),
                    Command::Reveal { win } | Command::CloseWindow { win } | Command::Focus(win) => {
                        format!("{}", win.seq)
                    }
                    _ => String::new(),
                };
                format!("{:?}({detail})@{}", CommandKind::of(&r.command), r.at.since(first_signal))
            })
            .collect();
        format!("candidate {} ended after {took} ms as {outcome:?}; commands {list:?}", cand.seq)
    };

    // A candidate left open before its first navigation completed (too late,
    // or folding turned off first) has no Location to log.
    if outcome.is_none() {
        prop_assert!(
            left_open && took <= sc.first_navigation_ms,
            "no FOLD entry although the Location was known (ended after {took} ms)"
        );
    }
    if let Some(outcome) = &outcome {
        prop_assert_eq!(
            matches!(outcome, FoldOutcome::NotFolded { .. }),
            left_open,
            "the log says {:?} but the candidate is {}",
            outcome,
            if closed { "closed" } else { "open" }
        );
        seen.outcomes.push(token(outcome));
    } else {
        prop_assert!(left_open, "a candidate without a FOLD entry was closed");
        seen.unlogged += 1;
    }

    // Tabs the fold created that are still there.
    let new_tabs: Vec<(WinKey, TabKey)> = sim
        .windows()
        .iter()
        .filter(|w| w.key != cand)
        .flat_map(|w| w.tabs.iter().filter(|t| !initial_tabs.contains(&t.key)).map(move |t| (w.key, t.key)))
        .collect();

    if left_open {
        // Left open and revealed with its own tab, Location, selection and
        // Window_Placement; every tab created for it is closed (Req 14.14).
        prop_assert!(new_tabs.is_empty(), "tabs created for the candidate are left open: {new_tabs:?}; {}", story());
        let window = sim.window(cand).expect("open");
        prop_assert_eq!(window.tabs.len(), 1, "the candidate has another tab count");
        prop_assert_eq!(
            window.tabs[0].last_known.as_ref().map(|l| l.fold_key()),
            Some(record.location.fold_key()),
            "the candidate shows another Location"
        );
        prop_assert_eq!(sorted(sim.selection(window.tabs[0].key)), sorted(&record.selection));
        prop_assert_eq!(&window.placement.state, &placement0.state);
        prop_assert_eq!(&window.placement.restored, &placement0.restored);
        prop_assert!(sim.is_visible(cand), "the candidate is not revealed");
        prop_assert!(sim.window_record(cand).is_some_and(|r| r.closed_at.is_none()));
        let revealed = commands.iter().any(|r| matches!(&r.command, Command::Reveal { win } if *win == cand));
        prop_assert!(revealed, "the candidate was never revealed");
    } else {
        // Closed by the fold: a Folded_Tab (the only new tab) or the
        // Matching_Tab is the Active_Tab of a window on its desktop.
        // A schedule with a window that closes `target_closes_after_ms` after
        // a command can close the Fold_Target after the fold completed (the
        // candidate is closed already); the Folded_Tab and the Active_Tab are
        // then gone with it. Such a window is missing from the final state.
        //
        // (A Fold_Target that *begins closing* while the fold finishes is
        // noticed at the revocation of the Folded_Tab and leaves the candidate
        // open, so it is not among these; see BUG-1 below.)
        let target_closed_later = closes_targets && initial_windows.iter().any(|w| sim.window(*w).is_none());
        let switched = matches!(outcome, Some(FoldOutcome::Switched { .. }));
        if switched {
            prop_assert!(new_tabs.is_empty(), "a Tab_Switch left new tabs: {new_tabs:?}; {}", story());
        } else if !(target_closed_later && new_tabs.is_empty()) {
            prop_assert_eq!(new_tabs.len(), 1, "a fold must leave exactly one new tab: {:?}; {}", new_tabs, story());
        }
        let shows = |w: &esr_sim::explorer::SimWindow| {
            w.tabs.get(w.active).and_then(|t| t.last_known.as_ref()).map(|l| l.fold_key())
                == Some(record.location.fold_key())
        };
        let targets: Vec<&esr_sim::explorer::SimWindow> =
            sim.windows().iter().filter(|w| w.key != cand && shows(w)).collect();
        if !(target_closed_later && targets.is_empty()) {
            prop_assert!(!targets.is_empty(), "no window has the Location as its Active_Tab; {}", story());
        }
        if let Some(&(win, tab)) = new_tabs.first() {
            let w = sim.window(win).expect("open");
            prop_assert_eq!(w.tabs[w.active].key, tab, "the Folded_Tab is not the Active_Tab; {}", story());
            prop_assert_eq!(w.tabs.last().map(|t| t.key), Some(tab), "the Folded_Tab is not the rightmost");
            if !record.selection.is_empty() {
                prop_assert_eq!(sorted(sim.selection(tab)), sorted(&record.selection));
            }
        }
        let on_desktop = |win: WinKey| {
            use esr_sim::fold::DesktopPlace::AllDesktops;
            let place = sim.desktop_of(win);
            place == desktop0 || place == Some(AllDesktops) || desktop0 == Some(AllDesktops)
        };
        prop_assert!(
            targets.is_empty() || targets.iter().any(|w| on_desktop(w.key)),
            "no such window is on the candidate's desktop"
        );
    }

    // No Matching_Tab (no tab that existed) and no other window is closed.
    for r in commands {
        match &r.command {
            Command::CloseTab { tab, .. } => {
                prop_assert!(!initial_tabs.contains(tab), "the engine closed a tab that existed: {:?}", tab);
            }
            Command::CloseWindow { win } => prop_assert_eq!(*win, cand, "the engine closed another window"),
            _ => {}
        }
    }

    // The chain of targets.
    let mut activates_before_create = 0;
    let mut switch_win: Option<WinKey> = None;
    let mut creates: Vec<WinKey> = Vec::new();
    for r in commands {
        match &r.command {
            Command::ActivateTab { win, .. } if creates.is_empty() => {
                activates_before_create += 1;
                switch_win.get_or_insert(*win);
            }
            Command::CreateTab { win, .. } => creates.push(*win),
            _ => {}
        }
    }
    // A failed Tab_Switch searches no new Matching_Tab (Req 14.23).
    prop_assert!(activates_before_create <= 1, "more than one Tab_Switch: {commands:?}");
    // No window is Fold_Target twice (Req 14.13, 14.24).
    let distinct: BTreeSet<&WinKey> = creates.iter().collect();
    prop_assert_eq!(distinct.len(), creates.len(), "a window was tried twice: {:?}", creates);
    // After a failed Tab_Switch the next target is the Matching_Tab's window,
    // unless it was disturbed or the schedule closes targets (Req 14.23).

    if let (Some(holder), Some(&next)) = (switch_win, creates.first())
        && !touched.contains(&holder)
        && !closes_targets
    {
        prop_assert_eq!(next, holder, "after a failed Tab_Switch the next target is not the Matching_Tab's window");
    }

    // Every path ends within the sum of its step timers.
    let bound = if sc.first_navigation_ms > FOLD_FIRST_NAV_MS {
        FOLD_FIRST_NAV_MS + SLACK_MS
    } else {
        let creating = creates.len() as u64;
        sc.first_navigation_ms
            + PROBE_MS
            + if switch_win.is_some() { FOLD_SWITCH_MS } else { 0 }
            + creating * FOLD_TAB_CREATE_MS
            + if creating > 0 { FOLD_TAB_SHOW_MS } else { 0 }
            + FOLD_ACTIVATE_MS * u64::from(FOLD_ACTIVATE_RETRIES + 1)
            + FOLD_CLOSE_RETRY_MS
            + SLACK_MS
    };
    // A folding turned off, or a window that closes, ends the fold earlier;
    // none of them extends it.
    prop_assert!(
        took <= bound,
        "the fold took {took} ms, more than the {bound} ms the step timers allow; outcome {outcome:?}, creates {creates:?}, switch {switch_win:?}, commands {:#?}",
        driver.events().iter().filter(|e| !matches!(e.event, Event::WindowScanned(_))).map(|e| (e.at, format!("{:?}", e.event))).collect::<Vec<_>>()
    );

    // Windows that hung answer again for the next round.
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn a_failed_fold_ends_folded_or_left_open(sc in scenario()) {
        run(&sc)?;
    }
}

// ---------------------------------------------------------------------------
// Examples
// ---------------------------------------------------------------------------

fn two_windows() -> Vec<WinSpec> {
    // Windows open in order and each is activated last when it opens: the
    // second one is the Primary_Window, the first the fallback.
    vec![
        WinSpec { tabs: vec![0, 1], active: 0, maximized: false, minimized: false },
        WinSpec { tabs: vec![2, 3], active: 0, maximized: false, minimized: false },
    ]
}

fn open(location: u16) -> Round {
    Round {
        step: Step::Open { location, selection: vec![ItemId(vec![1, 7])], foreground: None },
        hung: Vec::new(),
        disturbances: Vec::new(),
    }
}

fn never() -> CreateTabFault {
    CreateTabFault { tab_window: TabWindowFault::Never, ..CreateTabFault::default() }
}

fn scenario_of(rounds: Vec<Round>, faults: FoldFaults) -> Scenario {
    Scenario { windows: two_windows(), rounds, faults, first_navigation_ms: 150, registration_lag_ms: 0 }
}

/// The Primary_Window creates no tab: the fold goes on in the fallback window.
#[test]
fn a_primary_that_creates_no_tab_is_passed_over() {
    let mut faults = FoldFaults::none();
    faults.create_tab.insert(0, never());
    let seen = run(&scenario_of(vec![open(5)], faults)).expect("the chain ends");
    assert_eq!(seen.outcomes, vec!["folded-fallback".to_string()]);
}

/// Neither window creates a tab: the candidate is left open.
#[test]
fn no_window_creating_a_tab_leaves_the_candidate_open() {
    let mut faults = FoldFaults::none();
    faults.create_tab.insert(0, never());
    faults.create_tab.insert(1, never());
    let seen = run(&scenario_of(vec![open(5)], faults)).expect("the chain ends");
    assert_eq!(seen.outcomes, vec!["not-folded:TabNotCreated:false".to_string()]);
}

/// A Tab_Switch that is not confirmed is followed by a Folded_Tab in the
/// Matching_Tab's window, which is not the Primary_Window here.
#[test]
fn a_switch_that_is_not_confirmed_folds_into_the_matching_window() {
    use esr_sim::fold::{ActivateFault, ActivateMode};
    let mut faults = FoldFaults::none();
    faults.activate_tab.insert(0, ActivateFault { mode: ActivateMode::Unconfirmed, target_closes_after_ms: None });
    // Location 0 is the Active_Tab of the first window.
    let seen = run(&scenario_of(vec![open(0)], faults)).expect("the chain ends");
    assert_eq!(seen.outcomes, vec!["switch-failed-folded".to_string()]);
}

/// A Folded_Tab that registers after 3 s leaves the candidate open and the
/// tab, which appears later still, is closed.
#[test]
fn a_location_not_shown_within_3_s_leaves_the_candidate_open() {
    let mut faults = FoldFaults::none();
    faults.create_tab.insert(0, CreateTabFault { registration_delay_ms: 3_500, ..CreateTabFault::default() });
    let seen = run(&scenario_of(vec![open(5)], faults)).expect("the chain ends");
    assert_eq!(seen.outcomes, vec!["not-folded:LocationNotShown:false".to_string()]);
}

/// A first navigation that completes after 8 s leaves the candidate open.
#[test]
fn a_first_navigation_after_8_s_leaves_the_candidate_open() {
    let mut sc = scenario_of(vec![open(5)], FoldFaults::none());
    sc.first_navigation_ms = 9_000;
    let seen = run(&sc).expect("the chain ends");
    assert_eq!(seen.unlogged, 1, "{seen:?}");
}

/// The generated scenarios reach the failure chain's outcomes.
#[test]
fn the_scenarios_cover_the_outcomes_of_the_chain() {
    use proptest::strategy::{Strategy, ValueTree};
    use proptest::test_runner::TestRunner;

    let mut runner = TestRunner::deterministic();
    let mut all: Vec<String> = Vec::new();
    let mut unlogged = 0;
    for _ in 0..300 {
        let sc = scenario().new_tree(&mut runner).expect("a scenario").current();
        let seen = run(&sc).unwrap_or_else(|e| panic!("{e:?}\n{sc:#?}"));
        all.extend(seen.outcomes);
        unlogged += seen.unlogged;
    }
    let count = |prefix: &str| all.iter().filter(|t| t.starts_with(prefix)).count();
    assert!(
        count("folded-") + count("switched") >= 30
            && count("not-folded") >= 30
            && count("switch-failed-folded") >= 5
            && count("folded-fallback") >= 5
            && unlogged >= 1,
        "{} folded, {} not folded, {} switch failures, {} fallbacks, {unlogged} unlogged: {all:?}",
        count("folded-") + count("switched"),
        count("not-folded"),
        count("switch-failed-folded"),
        count("folded-fallback"),
    );
}

/// BUG-1 (fixed): the Fold_Target begins closing while the unconfirmed
/// activation of the Folded_Tab is retried; the fold used to complete between
/// the revocation of the Folded_Tab and the destruction of the window, close
/// the candidate, and lose the folder. The candidate is now left open.
#[test]
fn bug_1_a_fold_target_that_begins_closing_loses_the_folded_tab() {
    use esr_sim::fold::{ActivateFault, ActivateMode};
    let mut faults = FoldFaults::none();
    for ordinal in [0, 1] {
        faults.activate_tab.insert(ordinal, ActivateFault { mode: ActivateMode::Unconfirmed, target_closes_after_ms: None });
    }
    let one_tab = || WinSpec { tabs: vec![0], active: 0, maximized: false, minimized: false };
    let sc = Scenario {
        windows: vec![one_tab(), one_tab(), one_tab()],
        rounds: vec![Round {
            step: Step::Open { location: 1, selection: vec![], foreground: None },
            hung: Vec::new(),
            // The third window is the Primary_Window, the Fold_Target.
            disturbances: vec![Disturbance { at_ms: 2_056, win: 2, kind: Kind::Closing }],
        }],
        faults,
        first_navigation_ms: 150,
        registration_lag_ms: 0,
    };
    run(&sc).expect("the fold ends folded or left open");
}
