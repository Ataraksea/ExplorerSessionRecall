//! Property test for the outcome of a successful fold, end to end: the
//! `Engine` (with its `FoldingEngine`) against the simulated Explorer, and the
//! expected outcome from the independent `FoldReference` model.
//!
// Feature: explorer-session-restore, Property 37: Successful fold outcome
//!
//! **Property 37: Successful fold outcome**
//!
//! *For any* tracked session and candidate (Location, selection, minimized or
//! not Fold_Target) whose fold succeeds in the simulator:
//!
//! - Folded_Tab: the Fold_Target's tabs are its previous tabs in the same
//!   order followed by one tab showing the candidate's Location; that tab is
//!   active and holds exactly the requested selection;
//! - Tab_Switch: the Fold_Target's tabs and order are unchanged and the
//!   Matching_Tab is active with exactly the requested selection;
//! - in both cases the Fold_Target's Window_Placement is unchanged except that
//!   a minimized Fold_Target is in its pre-minimize state, it is the
//!   foreground window, the candidate is closed, and within 1 s `capture()`
//!   shows that tab as the Active_Tab with the Fold_Target first in the
//!   Activation_Order.
//!
//! The scenarios are 1–3 windows of 1–4 tabs (some maximized, some minimized),
//! then 1–4 candidates one after the other (each fold completes before the
//! next candidate opens, so later candidates meet the Folded_Tabs of earlier
//! ones): a candidate opened from another application, from a window that is
//! in the foreground when it opens, or by Ctrl+N on the Active_Tab of the
//! foreground window. No fault is injected, so every fold succeeds.
//!
//! The expected tabs, Active_Tab, selection, Window_Placement and foreground
//! window come from `FoldReference::candidate`/`check`; what is added here is
//! the exact selection of a Folded_Tab, the closed candidate with no tab
//! closed and no Session_End, and the Live_Session the tracker holds 1 s
//! after the candidate closed.
//!
//! **Validates: Requirements 14.1, 14.6, 14.7, 14.21, 14.26**

use esr_core::engine::{
    Engine, Environment, Event, ExplorerOptionValues, ItemId, Millis, SessionFileRead, StartupInput, TimerOwner, WinKey,
};
use esr_core::model::{Rect, ShowState, Snapshot};
use esr_core::settings::Settings;
use esr_sim::explorer::{default_displays, NavOutcome, Op, Pick, ProcessChoice, SimConfig, SimExplorer};
use esr_sim::fold::{selection_strategy, DesktopSpec, FoldConfig};
use esr_sim::reference::{canonical, FoldReference, RefCandidate, RefFoldEnv, RefFoldOutcome};
use esr_sim::restore::{CommandKind, Driver, Stop};
use proptest::collection::vec;
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

/// One window of the tracked session.
#[derive(Clone, Debug)]
struct WinSpec {
    /// The Locations of its tabs (picks into the simulator's pool).
    tabs: Vec<u16>,
    /// Pick of the Active_Tab.
    active: u16,
    maximized: bool,
    minimized: bool,
}

/// One candidate.
#[derive(Clone, Debug)]
enum Step {
    /// A window that opens on `location`; `foreground` picks the Explorer
    /// window that was in the foreground (`None`: another application was).
    Open { location: u16, selection: Vec<ItemId>, foreground: Option<u16> },
    /// Ctrl+N on the foreground window (nothing happens if an application is
    /// in the foreground).
    CtrlN,
}

#[derive(Clone, Debug)]
struct Scenario {
    windows: Vec<WinSpec>,
    steps: Vec<Step>,
}

/// Mostly a handful of Locations, so that a candidate often meets a tab that
/// shows its Location (a Tab_Switch) and often does not (a Folded_Tab).
const LOCATIONS: u16 = 6;

fn win_spec() -> impl Strategy<Value = WinSpec> {
    (vec(0..LOCATIONS, 1..=4), any::<u16>(), prop::bool::weighted(0.2), prop::bool::weighted(0.3))
        .prop_map(|(tabs, active, maximized, minimized)| WinSpec { tabs, active, maximized, minimized })
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        5 => (0..LOCATIONS, selection_strategy(), proptest::option::weighted(0.6, any::<u16>()))
            .prop_map(|(location, selection, foreground)| Step::Open { location, selection, foreground }),
        1 => Just(Step::CtrlN),
    ]
}

fn scenario() -> impl Strategy<Value = Scenario> {
    (vec(win_spec(), 1..=3), vec(step(), 1..=4)).prop_map(|(windows, steps)| Scenario { windows, steps })
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

/// How many folds ended as a Folded_Tab and as a Tab_Switch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Tally {
    folded: usize,
    switched: usize,
    minimized_target: usize,
}

fn run(sc: &Scenario) -> Result<Tally, TestCaseError> {
    let mut sim = SimExplorer::new(SimConfig {
        fold: FoldConfig { report_tab_started: true, ..FoldConfig::default() },
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
    // Windows already open are ranked in their Activation_Order (Req 14.8).
    driver.deliver_now(Event::ShellAvailable);
    prop_assert_eq!(driver.run(|now, event| engine.handle(now, event)), Stop::Quiescent);

    let mut reference = FoldReference::of(driver.sim());
    prop_assert_eq!(engine.folding().opening().to_vec(), reference.opening_order());

    let mut tally = Tally::default();
    for step in &sc.steps {
        let before = driver.sim().candidates().len();
        let events = driver.sim_mut().apply(&candidate_op(step));
        if driver.sim().candidates().len() == before {
            continue; // Ctrl+N with an application in the foreground
        }
        let record = driver.sim().candidates().last().cloned().expect("a candidate");
        let cand: WinKey = record.win;
        let foreground = events
            .iter()
            .find_map(|e| match &e.event {
                Event::WindowAppeared { win, foreground, .. } if *win == cand => Some(*foreground),
                _ => None,
            })
            .expect("the candidate's first signal");
        let wanted = RefCandidate {
            win: cand,
            location: record.location.clone(),
            selection: record.selection.clone(),
            foreground,
        };

        // What the model expects (Req 14.1, 14.20-14.22).
        let env = RefFoldEnv::of(driver.sim(), cand);
        reference.candidate_appeared(cand);
        let outcome = reference.candidate(&wanted, &env);
        let (target, index, folded) = match outcome {
            RefFoldOutcome::Folded { win, index } => (win, index, true),
            RefFoldOutcome::Switched { win, index } => (win, index, false),
            RefFoldOutcome::LeftOpen => {
                return Err(TestCaseError::fail(format!("the model has no Fold_Target for {wanted:?}")));
            }
        };
        let minimized =
            matches!(driver.sim().window(target).map(|w| w.placement.state), Some(ShowState::Minimized { .. }));

        // The Engine folds it.
        driver.deliver(events);
        let mut done_at: Option<Millis> = None;
        let mut at_deadline: Option<Snapshot> = None;
        let stop = driver.run(|now, event| {
            // The state 1 s after the candidate closed is the one before the
            // first event that is later than that.
            if let Some(done) = done_at
                && at_deadline.is_none()
                && now.0 > done.0 + 1_000
            {
                at_deadline = Some(engine.tracker().capture().0);
            }
            if matches!(&event, Event::WindowDestroyed { win } if *win == cand) {
                done_at = Some(now);
            }
            engine.handle(now, event)
        });
        prop_assert_eq!(stop, Stop::Quiescent);
        prop_assert!(done_at.is_some(), "the candidate {:?} was never closed", cand);
        let at_deadline = at_deadline.unwrap_or_else(|| engine.tracker().capture().0);

        let sim = driver.sim();

        // Tabs and order, the active tab, the selection, the Window_Placement
        // and the foreground window, of every window (Req 14.6, 14.7, 14.21).
        prop_assert_eq!(reference.check(sim), Ok(()), "after {:?}", wanted);
        let window = sim.window(target).expect("the Fold_Target is open");
        prop_assert_eq!(window.active, index);
        let tab = &window.tabs[index];
        prop_assert_eq!(
            tab.last_known.as_ref().map(|l| l.fold_key()),
            Some(wanted.location.fold_key()),
            "the active tab shows the candidate's Location"
        );
        if folded {
            // A Folded_Tab holds exactly the requested items (none if none).
            prop_assert_eq!(index + 1, window.tabs.len(), "the Folded_Tab is the rightmost tab");
            prop_assert_eq!(sorted(sim.selection(tab.key)), sorted(&wanted.selection));
        }
        prop_assert_eq!(sim.foreground(), Some(target));
        let still_minimized = matches!(window.placement.state, ShowState::Minimized { .. });
        prop_assert!(!still_minimized, "the Fold_Target is shown, not minimized");

        // The candidate is closed by the fold, with no tab closed and no
        // Session_End (Req 14.1, 14.28).
        prop_assert!(sim.window(cand).is_none(), "the candidate is still open");
        prop_assert!(sim.window_record(cand).is_some_and(|r| r.closed_at.is_some()));
        prop_assert_eq!(engine.folding().candidates_in_progress(), 0);
        prop_assert!(!engine.folding().opening().contains(cand), "a folded candidate is not ranked");
        prop_assert!(engine.tracker().window(cand).is_none(), "a folded candidate is not tracked");
        prop_assert!(sim.commands_of(CommandKind::CloseTab).is_empty(), "no tab is closed by a successful fold");
        prop_assert!(sim.reference().session_ends().is_empty());

        // Within 1 s the Live_Session shows the tab as the Active_Tab of the
        // Fold_Target, which is first in the Activation_Order (Req 14.26).
        prop_assert_eq!(sim.reference().windows().first().map(|w| w.key), Some(target));
        let expected = sim.reference().snapshot();
        prop_assert_eq!(canonical(&at_deadline), canonical(&expected));
        let first = canonical(&at_deadline).windows.into_iter().next().expect("a window");
        prop_assert_eq!(first.active, index);
        prop_assert_eq!(first.tabs.len(), window.tabs.len());
        prop_assert_eq!(first.tabs[index].location.fold_key(), wanted.location.fold_key());

        reference.settle();
        if folded {
            tally.folded += 1;
        } else {
            tally.switched += 1;
        }
        tally.minimized_target += usize::from(minimized);
    }
    Ok(tally)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn a_successful_fold_leaves_the_target_as_the_model_expects(sc in scenario()) {
        run(&sc)?;
    }
}

// ---------------------------------------------------------------------------
// Examples
// ---------------------------------------------------------------------------

/// The generated scenarios reach every kind of outcome: Folded_Tabs,
/// Tab_Switches and minimized Fold_Targets.
#[test]
fn the_scenarios_cover_folds_switches_and_minimized_targets() {
    use proptest::strategy::{Strategy, ValueTree};
    use proptest::test_runner::TestRunner;

    let mut runner = TestRunner::deterministic();
    let mut total = Tally::default();
    for _ in 0..200 {
        let sc = scenario().new_tree(&mut runner).expect("a scenario").current();
        let tally = run(&sc).expect("every fold succeeds");
        total.folded += tally.folded;
        total.switched += tally.switched;
        total.minimized_target += tally.minimized_target;
    }
    assert!(total.folded >= 50 && total.switched >= 50 && total.minimized_target >= 10, "{total:?}");
}


/// A Folded_Tab with a selection into a minimized window, then a Tab_Switch
/// to that tab and a Folded_Tab by Ctrl+N on the foreground window.
#[test]
fn a_folded_tab_a_tab_switch_and_ctrl_n_on_a_minimized_primary() {
    let sc = Scenario {
        // The second window was activated last, so it is the Primary_Window.
        windows: vec![
            WinSpec { tabs: vec![4], active: 0, maximized: false, minimized: false },
            WinSpec { tabs: vec![2, 3], active: 0, maximized: true, minimized: true },
        ],
        steps: vec![
            // Not open anywhere: a Folded_Tab with one item selected, shown in
            // the minimized Primary_Window's pre-minimize state.
            Step::Open { location: 5, selection: vec![ItemId(vec![1, 7])], foreground: None },
            // Open in the Primary_Window's second tab: a Tab_Switch.
            Step::Open { location: 3, selection: vec![], foreground: None },
            // Ctrl+N on the foreground window (the Primary_Window): its
            // Active_Tab is the Matching_Tab, so it folds a tab there.
            Step::CtrlN,
        ],
    };
    let tally = run(&sc).expect("every fold succeeds");
    assert_eq!(tally, Tally { folded: 2, switched: 1, minimized_target: 1 });
}
