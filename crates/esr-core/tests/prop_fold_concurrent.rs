//! Property test for concurrent Folding_Candidates, end to end: the `Engine`
//! (with its `FoldingEngine`) against the simulated Explorer, and the expected
//! outcome from the independent `FoldReference` model.
//!
// Feature: explorer-session-restore, Property 39: Concurrent candidates are handled once, in creation order
//!
//! **Property 39: Concurrent candidates are handled once, in creation order**
//!
//! *For any* burst of candidates (arbitrary creation order, arrival order of
//! their Locations, and Location equalities):
//!
//! - each candidate is folded, switched or left open exactly once: it gets one
//!   `CloseWindow`, it is gone at the end, and no tab is closed;
//! - at most one `CreateTab` is outstanding per Fold_Target: no `CreateTab` is
//!   issued while an earlier one of the same window has not registered its
//!   tab (`TabCreated`), and there is one `CreateTab` per Folded_Tab;
//! - Folded_Tabs that share a Fold_Target are left to right in creation order;
//! - a candidate whose Location equals an earlier candidate's pending
//!   Folded_Tab switches to it (no second tab with that Location, unless the
//!   candidate's Foreground_Explorer_Window is the one that shows it, Req
//!   14.22);
//! - the tab of the last-created candidate of each Fold_Target ends active.
//!
//! The scenarios are one or two windows of one to four tabs with different
//! Locations, then a burst from `fold_burst()`: 1–6 candidates, their first
//! signals spaced by the generated gaps, their Locations known after the
//! generated first-navigation delays (so a later candidate's Location often
//! arrives before an earlier one's), several with equal Locations. The
//! simulator registers a Folded_Tab only after a lag, so that tabs are
//! pending while later candidates choose their targets. No fault is injected.
//! Selections are not part of this property and are left out.
//!
//! The expected tabs and Active_Tabs come from `FoldReference::candidate`,
//! fed in creation order; the windows may complete their folds in any order,
//! so the foreground window is not compared.
//!
//! **Validates: Requirements 14.19, 14.20**

mod common;

use common::strategies::{fold_burst, FoldBurst};
use esr_core::engine::{
    Engine, Environment, Event, ExplorerOptionValues, Command, Millis, SessionFileRead, StartupInput, TimerOwner, WinKey,
};
use esr_core::settings::Settings;
use esr_sim::explorer::{default_displays, NavOutcome, Op, Pick, ProcessChoice, SimConfig, SimExplorer};
use esr_sim::fold::FoldConfig;
use esr_sim::reference::{FoldReference, RefCandidate, RefFoldEnv, RefFoldOutcome};
use esr_sim::restore::{CommandKind, Driver, Stop};
use esr_core::model::Rect;
use proptest::collection::vec;
use proptest::prelude::*;
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

/// One window of the tracked session.
#[derive(Clone, Debug)]
struct WinSpec {
    /// The Locations of its tabs (picks into the simulator's pool), all
    /// different.
    tabs: Vec<u16>,
    /// Pick of the Active_Tab.
    active: u16,
}

#[derive(Clone, Debug)]
struct Scenario {
    windows: Vec<WinSpec>,
    burst: FoldBurst,
    /// Time from a `CreateTab` to the tab registering.
    registration_lag_ms: u64,
}

/// The burst's Locations come from the first four pool entries (mostly); six
/// are available to the windows.
const LOCATIONS: u16 = 6;

fn win_spec() -> impl Strategy<Value = WinSpec> {
    (proptest::sample::subsequence((0..LOCATIONS).collect::<Vec<_>>(), 1..=4).prop_shuffle(), any::<u16>())
        .prop_map(|(tabs, active)| WinSpec { tabs, active })
}

fn scenario() -> impl Strategy<Value = Scenario> {
    (vec(win_spec(), 1..=2), fold_burst(), prop_oneof![Just(0u64), Just(250), Just(800)]).prop_map(
        |(windows, mut burst, registration_lag_ms)| {
            // Selections are not part of Property 39.
            burst.candidates.iter_mut().for_each(|c| c.selection.clear());
            // Neither is the foreground exception (Req 14.22, Property 41):
            // whether a Matching_Tab is the Active_Tab of the foreground
            // window depends on how far earlier folds into that window have
            // come, which the burst leaves open.
            burst.candidates.iter_mut().for_each(|c| c.foreground = None);
            Scenario { windows, burst, registration_lag_ms }
        },
    )
}

/// The operations that build the tracked session.
fn setup_ops(windows: &[WinSpec]) -> Vec<Op> {
    let mut ops = Vec::new();
    for (i, w) in windows.iter().enumerate() {
        let n = i as i32;
        ops.push(Op::OpenWindow {
            process: ProcessChoice::Main,
            location: Pick(w.tabs[0]),
            nav: NavOutcome::Complete,
            rect: Rect { left: 100 + 60 * n, top: 80 + 40 * n, right: 900 + 60 * n, bottom: 680 + 40 * n },
            maximized: false,
        });
        for &location in &w.tabs[1..] {
            ops.push(Op::OpenTab { win: Pick(i as u16), location: Pick(location), nav: NavOutcome::Complete });
        }
        ops.push(Op::ActivateTab { win: Pick(i as u16), tab: Pick(w.active) });
    }
    ops
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

/// What a run exercised.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Tally {
    folded: usize,
    switched: usize,
    /// The Locations arrived in another order than the creation order.
    reordered: bool,
    /// Some Fold_Target received more than one Folded_Tab.
    shared_target: bool,
    /// A candidate switched to a Folded_Tab of the same burst.
    switched_to_pending: bool,
    /// More than one window received a candidate.
    several_targets: bool,
}

fn run(sc: &Scenario) -> Result<Tally, TestCaseError> {
    let mut sim = SimExplorer::new(SimConfig {
        fold: FoldConfig {
            report_tab_started: true,
            registration_lag_ms: sc.registration_lag_ms,
            ..FoldConfig::default()
        },
        // Room for the windows, six candidates and their Folded_Tabs.
        max_windows: 12,
        max_tabs_per_window: 16,
        ..SimConfig::default()
    });
    let mut events = Vec::new();
    for op in setup_ops(&sc.windows) {
        events.extend(sim.apply(&op));
    }
    events.extend(sim.settle());

    // The Engine tracks the session; folding is on (the default setting).
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
    prop_assert_eq!(driver.run(|now, event| engine.handle(now, event)), Stop::Quiescent);
    driver.deliver_now(Event::ShellAvailable);
    prop_assert_eq!(driver.run(|now, event| engine.handle(now, event)), Stop::Quiescent);

    let mut reference = FoldReference::of(driver.sim());
    prop_assert_eq!(engine.folding().opening().to_vec(), reference.opening_order());
    let initial: BTreeMap<WinKey, usize> =
        reference.targets().iter().map(|t| (t.win, t.tabs.len())).collect();
    let base_events = driver.events().len();

    // The burst: each candidate opens at its scheduled moment while the
    // earlier ones are still being folded. The model takes them in creation
    // order (Req 14.19).
    let ops = sc.burst.sim_ops();
    let signals = sc.burst.first_signals();
    let t0 = driver.sim().now();
    let mut candidates: Vec<RefCandidate> = Vec::new();
    let mut outcomes: Vec<RefFoldOutcome> = Vec::new();
    for (i, chunk) in ops.chunks(3).enumerate() {
        let at = t0 + signals[i];
        driver.set_deadline(at);
        let stop = driver.run(|now, event| engine.handle(now, event));
        prop_assert!(matches!(stop, Stop::Quiescent | Stop::Deadline), "{:?}", stop);
        let gap = at.since(driver.sim().now());
        let events = driver.sim_mut().tick(gap);
        driver.deliver(events);

        // chunk = [first-navigation delay, the candidate, the gap (played by
        // the schedule above)].
        let before = driver.sim().candidates().len();
        let mut events = driver.sim_mut().apply(&chunk[0]);
        events.extend(driver.sim_mut().apply(&chunk[1]));
        prop_assert_eq!(driver.sim().candidates().len(), before + 1, "the candidate opened");
        let record = driver.sim().candidates().last().cloned().expect("a candidate");
        let cand: WinKey = record.win;
        let foreground = events
            .iter()
            .find_map(|e| match &e.event {
                Event::WindowAppeared { win, foreground, .. } if *win == cand => Some(*foreground),
                _ => None,
            })
            .expect("the candidate's first signal");
        let wanted = RefCandidate { win: cand, location: record.location, selection: record.selection, foreground };

        let env = RefFoldEnv::of(driver.sim(), cand);
        reference.candidate_appeared(cand);
        let outcome = reference.candidate(&wanted, &env);
        prop_assert!(outcome != RefFoldOutcome::LeftOpen, "the model has no Fold_Target for {:?}", wanted);
        candidates.push(wanted);
        outcomes.push(outcome);
        driver.deliver(events);
    }
    driver.set_deadline(driver.sim().now() + 600_000);
    prop_assert_eq!(driver.run(|now, event| engine.handle(now, event)), Stop::Quiescent);

    let sim = driver.sim();

    // Each candidate is handled exactly once: one CloseWindow, closed, no tab
    // closed, no Session_End (Req 14.1, 14.19).
    let closes: Vec<WinKey> = sim
        .commands_of(CommandKind::CloseWindow)
        .iter()
        .filter_map(|r| match &r.command {
            Command::CloseWindow { win } => Some(*win),
            _ => None,
        })
        .collect();
    let mut asked = closes.clone();
    asked.sort();
    let mut expected: Vec<WinKey> = candidates.iter().map(|c| c.win).collect();
    expected.sort();
    prop_assert_eq!(asked, expected, "one CloseWindow per candidate");
    for c in &candidates {
        prop_assert!(sim.window(c.win).is_none(), "the candidate {:?} is still open", c.win);
        prop_assert!(!engine.folding().opening().contains(c.win), "a folded candidate is not ranked");
    }
    prop_assert_eq!(engine.folding().candidates_in_progress(), 0);
    prop_assert!(sim.commands_of(CommandKind::CloseTab).is_empty(), "no tab is closed by a fold");
    prop_assert!(sim.reference().session_ends().is_empty());

    // At most one CreateTab outstanding per Fold_Target, and one per
    // Folded_Tab (Req 14.19).
    prop_assert!(sim.pipelining_violations().is_empty(), "pipelined CreateTab: {:?}", sim.pipelining_violations());
    let folded: Vec<&RefFoldOutcome> = outcomes.iter().filter(|o| matches!(o, RefFoldOutcome::Folded { .. })).collect();
    let creates = sim.commands_of(CommandKind::CreateTab);
    prop_assert_eq!(creates.len(), folded.len(), "one CreateTab per Folded_Tab");
    let mut registered: BTreeMap<WinKey, Vec<Millis>> = BTreeMap::new();
    for e in &driver.events()[base_events..] {
        if let Event::TabCreated { win, .. } = &e.event {
            registered.entry(*win).or_default().push(e.at);
        }
    }
    let mut issued: BTreeMap<WinKey, Vec<Millis>> = BTreeMap::new();
    for r in &creates {
        if let Command::CreateTab { win, .. } = &r.command {
            issued.entry(*win).or_default().push(r.at);
        }
    }
    for (win, times) in &issued {
        for (k, at) in times.iter().enumerate() {
            let earlier_registered = k == 0 || registered.get(win).and_then(|r| r.get(k - 1)).is_some_and(|r| r <= at);
            prop_assert!(earlier_registered, "{:?}: CreateTab #{} at {:?} while an earlier one is outstanding", win, k, at);
        }
    }

    // The tabs and the Active_Tab of every Fold_Target are what the model
    // expects, with the Folded_Tabs left to right in creation order, and the
    // last-created candidate's tab active (Req 14.19, 14.20).
    let mut per_target: BTreeMap<WinKey, Vec<(usize, RefFoldOutcome)>> = BTreeMap::new();
    for (i, o) in outcomes.iter().enumerate() {
        let (RefFoldOutcome::Folded { win, .. } | RefFoldOutcome::Switched { win, .. }) = o else { continue };
        per_target.entry(*win).or_default().push((i, *o));
    }
    for t in reference.targets() {
        let w = sim.window(t.win).expect("a Fold_Target is open");
        prop_assert_eq!(w.tabs.len(), t.tabs.len(), "tabs of {:?}", t.win);
        for (i, (seen, want)) in w.tabs.iter().zip(&t.tabs).enumerate() {
            let want = want.location.as_ref().map(|l| l.fold_key());
            prop_assert_eq!(seen.last_known.as_ref().map(|l| l.fold_key()), want, "tab {} of {:?}", i, t.win);
        }
        prop_assert_eq!(w.active, t.active, "Active_Tab of {:?}", t.win);
    }
    for (win, mine) in &per_target {
        let w = sim.window(*win).expect("a Fold_Target is open");
        // Left to right in creation order.
        let added: Vec<_> = w.tabs[initial[win]..].iter().map(|t| t.last_known.as_ref().map(|l| l.fold_key())).collect();
        let created: Vec<_> = mine
            .iter()
            .filter(|(_, o)| matches!(o, RefFoldOutcome::Folded { .. }))
            .map(|(i, _)| Some(candidates[*i].location.fold_key()))
            .collect();
        prop_assert_eq!(added, created, "Folded_Tabs of {:?} in creation order", win);
        // The last-created candidate's tab is active.
        let (last, outcome) = mine.last().expect("a candidate");
        let index = match outcome {
            RefFoldOutcome::Folded { index, .. } | RefFoldOutcome::Switched { index, .. } => *index,
            RefFoldOutcome::LeftOpen => unreachable!("filtered"),
        };
        prop_assert_eq!(w.active, index, "the last candidate's tab of {:?} is active", win);
        prop_assert_eq!(
            w.tabs[w.active].last_known.as_ref().map(|l| l.fold_key()),
            Some(candidates[*last].location.fold_key())
        );
    }

    // A repeated Location switches: a Location gets one new tab, plus one
    // for each candidate of that Location whose Foreground_Explorer_Window
    // may have made it a Folded_Tab (Req 14.22).
    let mut keys: Vec<_> = candidates.iter().map(|c| c.location.fold_key()).collect();
    keys.sort();
    keys.dedup();
    for key in &keys {
        let added = sim
            .windows()
            .iter()
            .filter(|w| initial.contains_key(&w.key))
            .map(|w| {
                w.tabs[initial[&w.key]..].iter().filter(|t| t.last_known.as_ref().map(|l| l.fold_key()).as_ref() == Some(key)).count()
            })
            .sum::<usize>();
        let with_foreground = candidates.iter().filter(|c| &c.location.fold_key() == key && c.foreground.is_some()).count();
        prop_assert!(added <= 1 + with_foreground, "{} new tabs for one Location", added);
    }

    let mut tally = Tally {
        folded: folded.len(),
        switched: outcomes.len() - folded.len(),
        reordered: sc.burst.arrival_order() != (0..sc.burst.len()).collect::<Vec<_>>(),
        shared_target: per_target.values().any(|m| m.iter().filter(|(_, o)| matches!(o, RefFoldOutcome::Folded { .. })).count() > 1),
        several_targets: per_target.len() > 1,
        ..Tally::default()
    };
    // A Switched candidate whose tab is an added one switched to a pending
    // Folded_Tab.
    for o in &outcomes {
        if let RefFoldOutcome::Switched { win, index } = o
            && *index >= initial[win]
        {
            tally.switched_to_pending = true;
        }
    }
    Ok(tally)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn concurrent_candidates_are_handled_once_in_creation_order(sc in scenario()) {
        run(&sc)?;
    }
}

// ---------------------------------------------------------------------------
// Examples
// ---------------------------------------------------------------------------

/// The generated scenarios reach every kind of burst: Folded_Tabs sharing a
/// target, switches to pending Folded_Tabs, Locations that arrive out of
/// order, and several Fold_Targets.
#[test]
fn the_scenarios_cover_the_interesting_bursts() {
    use proptest::strategy::{Strategy, ValueTree};
    use proptest::test_runner::TestRunner;

    let mut runner = TestRunner::deterministic();
    let mut folded = 0;
    let mut switched = 0;
    let mut shared = 0;
    let mut pending = 0;
    let mut reordered = 0;
    let mut several = 0;
    for _ in 0..200 {
        let sc = scenario().new_tree(&mut runner).expect("a scenario").current();
        let t = run(&sc).expect("every fold succeeds");
        folded += t.folded;
        switched += t.switched;
        shared += usize::from(t.shared_target);
        pending += usize::from(t.switched_to_pending);
        reordered += usize::from(t.reordered);
        several += usize::from(t.several_targets);
    }
    assert!(
        folded >= 100 && switched >= 50 && shared >= 30 && pending >= 20 && reordered >= 30 && several >= 10,
        "folded {folded}, switched {switched}, shared {shared}, pending {pending}, reordered {reordered}, several {several}"
    );
}

/// Three candidates at once for one window: two different Locations and a
/// repeat of the first, whose Location is known before the first one's.
#[test]
fn three_candidates_at_once_fold_in_creation_order_and_the_repeat_switches() {
    use common::strategies::BurstCandidate;
    let candidate = |slot: usize, first_navigation_ms: u64| BurstCandidate {
        slot,
        spelling: 0,
        selection: vec![],
        foreground: None,
        gap_ms: 0,
        first_navigation_ms,
    };
    let sc = Scenario {
        windows: vec![WinSpec { tabs: vec![5], active: 0 }],
        // The third candidate (a repeat of the first) knows its Location
        // first; the second is the slowest.
        burst: FoldBurst { candidates: vec![candidate(0, 300), candidate(1, 500), candidate(0, 100)] },
        registration_lag_ms: 800,
    };
    let tally = run(&sc).expect("every fold succeeds");
    assert_eq!(tally.folded, 2);
    assert_eq!(tally.switched, 1);
    assert!(tally.reordered && tally.shared_target && tally.switched_to_pending);
}
