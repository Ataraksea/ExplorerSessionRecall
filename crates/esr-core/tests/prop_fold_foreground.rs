//! Property test for the foreground exception of the pure `fold::select`
//! (the pure part of Property 41; the simulator part is task 23.6).
//!
// Feature: explorer-session-restore, Property 41: Foreground exception
//!
//! **Property 41: Foreground exception**
//!
//! *For any* `FoldCtx` in which the Matching_Tab selected under 14.20 is the
//! Active_Tab of the candidate's Foreground_Explorer_Window, `select` returns
//! `Fold { target: foreground, reason: Foreground }` and never `Switch`; for
//! any other `FoldCtx` with a Matching_Tab it returns `Switch`, whatever the
//! candidate's `foreground` is.
//!
//! The Matching_Tab is not searched when `exclude` is set (Req 14.24), so the
//! properties use `exclude: None` wherever a `Switch` or foreground outcome is
//! expected.
//!
//! - `forced_foreground_exception`: a scenario shaped so that the foreground
//!   window is the Primary_Window and its Active_Tab shows the candidate's
//!   Location, which must give the foreground `Fold`.
//! - `foreground_changes_only_the_matching_tab_outcome`: metamorphic. The plan
//!   with `foreground: None` is the reference. If it is a `Switch`, any
//!   `foreground` gives the foreground `Fold` exactly when it is the matching
//!   window and the match is that window's Active_Tab, and the same `Switch`
//!   otherwise. If it is not a `Switch` (no Matching_Tab), `foreground` has no
//!   effect.
//! - `a_failed_search_never_switches_or_folds_foreground`: with `exclude` set,
//!   the plan is never `Switch` and never has the reason `Foreground`.
//!
//! The simulator part runs the `Engine` against the simulated Explorer through
//! the `Driver`. The scenarios are 1–3 windows of 1–4 tabs; the window `F`
//! chosen as the foreground window is activated last, so that it is the
//! Primary_Window, and is not minimized. Its Active_Tab shows Location `L`.
//!
//! - `a_candidate_on_the_foreground_active_tab_adds_one_rightmost_tab`: 1–3
//!   candidates, each a Ctrl+N on `F` or a window opened on `L` with `F` as the
//!   Foreground_Explorer_Window, or (the control) a window opened on `L` whose
//!   foreground is another window or an application. The first kind ends as one
//!   new rightmost active tab in `F` showing `L` with the other tabs unchanged,
//!   one `CreateTab` for `F`, no `ActivateTab`, no tab closed and the single
//!   log entry `outcome=folded-foreground into=foreground passed=`; the control
//!   is a Tab_Switch to the Active_Tab of `F` (no new tab) with the entry
//!   `outcome=switched matching=primary`. The model's `FoldReference` predicts
//!   the same and checks every window.
//! - `a_failing_foreground_target_follows_the_failure_chain`: the first
//!   `CreateTab` (to `F`) never gets a tab window. With another window, the
//!   first window after `F` in the Activation_Order takes the Folded_Tab and
//!   the entry is `folded-foreground into=fallback passed=tab-not-created-1s`;
//!   with none the candidate is left open (`not-folded`). `F` is never changed
//!   and no other window gains a tab it should not have.
//!
//! **Validates: Requirements 14.21, 14.22**

mod common;

use common::strategies::*;
use esr_core::engine::{
    Command, Engine, Environment, Event, ExplorerOptionValues, ItemId, Millis, SessionFileRead, StartupInput, TimerOwner, WinKey,
};
use esr_core::fold::{select, FoldPlan, FoldReason};
use esr_core::engine::DesktopView;
use esr_core::logfmt::entry::{format_entry, LogEntry};
use esr_core::model::{Rect, ShowState, Timestamp};
use esr_core::settings::Settings;
use esr_sim::explorer::{default_displays, NavOutcome, Op, Pick, ProcessChoice, SimConfig, SimExplorer};
use esr_sim::fold::{selection_strategy, CreateTabFault, DesktopSpec, FoldConfig, TabWindowFault};
use esr_sim::reference::{FoldReference, RefCandidate, RefFoldEnv, RefFoldOutcome};
use esr_sim::restore::{CommandKind, Driver, Stop};
use proptest::collection::vec;
use proptest::prelude::*;
use proptest::sample::Index;

fn plan(s: &FoldScenario) -> FoldPlan {
    s.with_ctx(select)
}

/// `foreground` values to try: none, every tracked window, an untracked one.
fn foregrounds(s: &FoldScenario) -> Vec<Option<WinKey>> {
    let mut v = vec![None, Some(WinKey { hwnd: 0xdead, seq: 9999 })];
    v.extend(s.keys().into_iter().map(Some));
    v
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    #[test]
    fn forced_foreground_exception(mut s in fold_scenario(), pick in any::<Index>(), tab in any::<Index>()) {
        if s.windows.is_empty() {
            return Ok(());
        }
        let i = pick.index(s.windows.len());
        let win = s.windows[i].win;
        let t = tab.index(s.windows[i].tabs.len());

        // The foreground window is the usable Primary_Window and its
        // Active_Tab shows the candidate's Location.
        s.windows[i].closing = false;
        s.windows[i].active = Some(t);
        s.windows[i].tabs[t].location = Some(s.candidate.clone());
        s.unresponsive.remove(&win);
        if let DesktopView::Known(set) = &mut s.desktop {
            set.insert(win);
        }
        s.opening.retain(|&w| w != win);
        s.opening.insert(0, win);
        s.foreground = Some(win);
        s.exclude = None;

        prop_assert_eq!(
            plan(&s),
            FoldPlan::Fold { target: win, reason: Some(FoldReason::Foreground) }
        );
    }

    #[test]
    fn foreground_changes_only_the_matching_tab_outcome(mut s in fold_scenario()) {
        s.exclude = None;
        s.foreground = None;
        let reference = plan(&s);
        let windows = s.windows.clone();

        for fg in foregrounds(&s) {
            s.foreground = fg;
            let got = plan(&s);
            match reference {
                FoldPlan::Switch { win, index } => {
                    let active = windows.iter().find(|w| w.win == win).and_then(|w| w.active);
                    if fg == Some(win) && active == Some(index) {
                        prop_assert_eq!(
                            got,
                            FoldPlan::Fold { target: win, reason: Some(FoldReason::Foreground) },
                            "foreground {:?} holds the Active_Tab match", fg
                        );
                    } else {
                        prop_assert_eq!(got, reference, "foreground {:?}", fg);
                    }
                }
                _ => prop_assert_eq!(got, reference, "no Matching_Tab, foreground {:?}", fg),
            }
        }
    }

    #[test]
    fn a_failed_search_never_switches_or_folds_foreground(s in fold_scenario(), pick in any::<Index>()) {
        if s.windows.is_empty() {
            return Ok(());
        }
        let mut s = s;
        let keys = s.keys();
        s.exclude = Some(keys[pick.index(keys.len())]);
        let got = plan(&s);
        prop_assert!(!matches!(got, FoldPlan::Switch { .. }), "{:?}", got);
        if let FoldPlan::Fold { reason, .. } = got {
            prop_assert_ne!(reason, Some(FoldReason::Foreground));
        }
    }
}

// ---------------------------------------------------------------------------
// The simulator part
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

/// One candidate; its Location is always `L`, the Location of `F`'s Active_Tab.
#[derive(Clone, Debug)]
enum FgStep {
    /// The foreground exception: Ctrl+N on `F`, or a window opened on `L`
    /// with `F` as its Foreground_Explorer_Window.
    Exception { ctrl_n: bool, selection: Vec<ItemId> },
    /// The control: a window opened on `L` whose foreground is another window
    /// (`Some`, a pick among the others) or an application (`None`).
    Elsewhere { selection: Vec<ItemId>, foreground: Option<u16> },
}

#[derive(Clone, Debug)]
struct FgScenario {
    windows: Vec<WinSpec>,
    /// Pick of the foreground window `F`.
    f: u16,
    steps: Vec<FgStep>,
}

/// Mostly a handful of Locations, so that other windows often show `L` too.
const LOCATIONS: u16 = 6;

fn win_spec() -> impl Strategy<Value = WinSpec> {
    (vec(0..LOCATIONS, 1..=4), any::<u16>(), prop::bool::weighted(0.2), prop::bool::weighted(0.3))
        .prop_map(|(tabs, active, maximized, minimized)| WinSpec { tabs, active, maximized, minimized })
}

fn fg_step() -> impl Strategy<Value = FgStep> {
    prop_oneof![
        3 => (any::<bool>(), selection_strategy())
            .prop_map(|(ctrl_n, selection)| FgStep::Exception { ctrl_n, selection }),
        2 => (selection_strategy(), proptest::option::weighted(0.6, any::<u16>()))
            .prop_map(|(selection, foreground)| FgStep::Elsewhere { selection, foreground }),
    ]
}

fn fg_scenario() -> impl Strategy<Value = FgScenario> {
    (vec(win_spec(), 1..=3), any::<u16>(), vec(fg_step(), 1..=3)).prop_map(|(mut windows, f, steps)| {
        // The foreground window is shown.
        let i = usize::from(f) % windows.len();
        windows[i].minimized = false;
        FgScenario { windows, f: i as u16, steps }
    })
}

/// The operations that build the tracked session and leave `F` activated last.
fn setup_ops(windows: &[WinSpec], f: u16) -> Vec<Op> {
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
    ops.push(Op::ActivateWindow { win: Pick(f) });
    ops
}

fn sorted(items: &[ItemId]) -> Vec<ItemId> {
    let mut v = items.to_vec();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

/// The `fold_key`s of the tabs of `win`, as strings (`None`: no Location).
fn tab_keys(sim: &SimExplorer, win: WinKey) -> Vec<Option<String>> {
    let window = sim.window(win).expect("the window is open");
    window.tabs.iter().map(|t| t.last_known.as_ref().map(|l| format!("{:?}", l.fold_key()))).collect()
}

/// The window a `CreateTab` names.
fn create_target(command: Option<&Command>) -> Option<WinKey> {
    match command {
        Some(Command::CreateTab { win, .. }) => Some(*win),
        _ => None,
    }
}

const TS: Timestamp =
    Timestamp { year: 2025, month: 1, day: 31, hour: 9, minute: 15, second: 2, millis: 123, offset_minutes: 0 };

/// The `FOLD` entries logged so far, as they appear in the Activity_Log
/// (without the timestamp).
fn fold_lines(sim: &SimExplorer) -> Vec<String> {
    sim.log_entries()
        .iter()
        .filter(|(_, entry)| matches!(entry, LogEntry::Fold { .. }))
        .map(|(_, entry)| format_entry(&TS, entry).splitn(4, ' ').nth(3).expect("an event").to_owned())
        .collect()
}

/// What the run starts from: the Engine tracking the session, the driver, the
/// model, and the keys of `F` and `L`'s pick.
struct Start {
    engine: Engine,
    driver: Driver,
    reference: FoldReference,
    f: WinKey,
    l: u16,
}

fn start(windows: &[WinSpec], f: u16) -> Result<Start, TestCaseError> {
    let mut sim = SimExplorer::new(SimConfig {
        fold: FoldConfig { report_tab_started: true, ..FoldConfig::default() },
        ..SimConfig::default()
    });
    let mut events = Vec::new();
    for op in setup_ops(windows, f) {
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

    let reference = FoldReference::of(driver.sim());
    let f_key = driver.sim().windows()[usize::from(f)].key;
    prop_assert_eq!(engine.folding().opening().to_vec(), reference.opening_order());
    prop_assert_eq!(engine.folding().opening().to_vec().first().copied(), Some(f_key), "F is the Primary_Window");
    prop_assert_eq!(driver.sim().foreground(), Some(f_key));
    let spec = &windows[usize::from(f)];
    let l = spec.tabs[usize::from(spec.active) % spec.tabs.len()];
    Ok(Start { engine, driver, reference, f: f_key, l })
}

/// How many steps of each kind ran.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Tally {
    exceptions: usize,
    ctrl_n: usize,
    controls: usize,
}

fn run(sc: &FgScenario) -> Result<Tally, TestCaseError> {
    let Start { mut engine, mut driver, mut reference, f, l } = start(&sc.windows, sc.f)?;
    let n = sc.windows.len();
    let mut tally = Tally::default();

    for step in &sc.steps {
        // `F` is in the foreground and shows `L` in its Active_Tab.
        prop_assert_eq!(driver.sim().foreground(), Some(f));
        let before = tab_keys(driver.sim(), f);
        let active_before = driver.sim().window(f).expect("F").active;
        let l_key = before[active_before].clone();
        let creates_before = driver.sim().commands_of(CommandKind::CreateTab).len();
        let folds_before = fold_lines(driver.sim()).len();
        let others_before: Vec<_> = driver
            .sim()
            .windows()
            .iter()
            .filter(|w| w.key != f)
            .map(|w| (w.key, tab_keys(driver.sim(), w.key)))
            .collect();

        let op = match step {
            FgStep::Exception { ctrl_n: true, .. } => Op::NewWindow { bypass_keys: false },
            FgStep::Exception { ctrl_n: false, selection } => Op::OpenCandidate {
                location: Pick(l),
                selection: selection.clone(),
                bypass_keys: false,
                desktop: DesktopSpec::Current,
                foreground: Some(Pick(sc.f)),
                process: ProcessChoice::Main,
            },
            FgStep::Elsewhere { selection, foreground } => Op::OpenCandidate {
                location: Pick(l),
                selection: selection.clone(),
                bypass_keys: false,
                desktop: DesktopSpec::Current,
                // Another window than `F` (`None` when `F` is alone).
                foreground: foreground.filter(|_| n > 1).map(|k| Pick((sc.f + 1 + k % (n as u16 - 1)) % n as u16)),
                process: ProcessChoice::Main,
            },
        };
        let events = driver.sim_mut().apply(&op);
        let record = driver.sim().candidates().last().cloned().expect("a candidate");
        let cand = record.win;
        let foreground = events
            .iter()
            .find_map(|e| match &e.event {
                Event::WindowAppeared { win, foreground, .. } if *win == cand => Some(*foreground),
                _ => None,
            })
            .expect("the candidate's first signal");
        let exception = matches!(step, FgStep::Exception { .. });
        if exception {
            prop_assert_eq!(foreground, Some(f), "the exception's candidate names F as the foreground window");
        } else {
            prop_assert_ne!(foreground, Some(f));
        }
        prop_assert_eq!(format!("{:?}", record.location.fold_key()), l_key.clone().expect("L"));

        // What the model expects (Req 14.20-14.22).
        let wanted = RefCandidate {
            win: cand,
            location: record.location.clone(),
            selection: record.selection.clone(),
            foreground,
        };
        let env = RefFoldEnv::of(driver.sim(), cand);
        reference.candidate_appeared(cand);
        let outcome = reference.candidate(&wanted, &env);
        if exception {
            prop_assert_eq!(outcome, RefFoldOutcome::Folded { win: f, index: before.len() });
        } else {
            prop_assert_eq!(outcome, RefFoldOutcome::Switched { win: f, index: active_before });
        }

        // The Engine folds it.
        driver.deliver(events);
        prop_assert_eq!(driver.run(|now, event| engine.handle(now, event)), Stop::Quiescent);
        let sim = driver.sim();

        prop_assert_eq!(reference.check(sim), Ok(()), "after {:?}", wanted);
        let after = tab_keys(sim, f);
        let window = sim.window(f).expect("F is open");
        let log = fold_lines(sim);
        prop_assert_eq!(log.len(), folds_before + 1, "exactly one FOLD entry: {:?}", log);
        let line = log.last().expect("an entry");
        let creates = sim.commands_of(CommandKind::CreateTab);
        if exception {
            // One new rightmost active tab showing `L`, the others unchanged.
            let mut expected = before.clone();
            expected.push(l_key);
            prop_assert_eq!(&after, &expected);
            prop_assert_eq!(window.active, before.len());
            prop_assert_eq!(sorted(sim.selection(window.tabs[before.len()].key)), sorted(&record.selection));
            prop_assert_eq!(creates.len(), creates_before + 1);
            prop_assert!(create_target(creates.last().map(|c| &c.command)) == Some(f), "CreateTab into F");
            prop_assert!(line.ends_with(" outcome=folded-foreground into=foreground passed="), "{}", line);
            tally.exceptions += 1;
            tally.ctrl_n += usize::from(matches!(step, FgStep::Exception { ctrl_n: true, .. }));
        } else {
            // A Tab_Switch to the Active_Tab: no tab is created.
            prop_assert_eq!(&after, &before);
            prop_assert_eq!(window.active, active_before);
            prop_assert_eq!(creates.len(), creates_before);
            prop_assert!(line.ends_with(" outcome=switched matching=primary"), "{}", line);
            tally.controls += 1;
        }

        // Nothing else changed: the other windows have their tabs, F is in
        // front, the candidate is closed, no tab was closed, no Session_End.
        for (win, tabs) in &others_before {
            prop_assert_eq!(&tab_keys(sim, *win), tabs, "window {:?}", win);
        }
        prop_assert_eq!(sim.foreground(), Some(f));
        prop_assert!(sim.window(cand).is_none(), "the candidate is still open");
        prop_assert_eq!(engine.folding().candidates_in_progress(), 0);
        prop_assert!(sim.commands_of(CommandKind::CloseTab).is_empty());
        prop_assert!(sim.reference().session_ends().is_empty());
        let shown = !matches!(window.placement.state, ShowState::Minimized { .. });
        prop_assert!(shown, "F is shown");

        reference.settle();
    }
    Ok(tally)
}

/// A first `CreateTab` (to `F`) that never gets a tab window.
fn run_failing(windows: &[WinSpec], f_pick: u16, selection: &[ItemId]) -> Result<bool, TestCaseError> {
    let Start { mut engine, mut driver, f, l, .. } = start(windows, f_pick)?;
    let ordinal = driver.sim().create_tab_commands();
    driver.sim_mut().faults_mut().create_tab.insert(
        ordinal,
        CreateTabFault { tab_window: TabWindowFault::Never, registration_delay_ms: 0, target_closes_after_ms: None },
    );
    // The first window after `F` in the Activation_Order takes the tab.
    let next = driver.sim().reference().windows().iter().map(|w| w.key).find(|&w| w != f);
    let before: Vec<_> = driver
        .sim()
        .windows()
        .iter()
        .map(|w| (w.key, tab_keys(driver.sim(), w.key), w.active))
        .collect();
    let l_key = {
        let (_, tabs, active) = before.iter().find(|(w, ..)| *w == f).expect("F");
        tabs[*active].clone()
    };

    let events = driver.sim_mut().apply(&Op::OpenCandidate {
        location: Pick(l),
        selection: selection.to_vec(),
        bypass_keys: false,
        desktop: DesktopSpec::Current,
        foreground: Some(Pick(f_pick)),
        process: ProcessChoice::Main,
    });
    let cand = driver.sim().candidates().last().expect("a candidate").win;
    driver.deliver(events);
    prop_assert_eq!(driver.run(|now, event| engine.handle(now, event)), Stop::Quiescent);
    let sim = driver.sim();

    let log = fold_lines(sim);
    prop_assert_eq!(log.len(), 1, "exactly one FOLD entry: {:?}", log);
    let creates = sim.commands_of(CommandKind::CreateTab);
    prop_assert!(
        create_target(creates.first().map(|c| &c.command)) == Some(f),
        "the foreground window was tried first"
    );
    prop_assert_eq!(engine.folding().candidates_in_progress(), 0);
    prop_assert!(sim.commands_of(CommandKind::CloseTab).is_empty(), "no tab exists to close");
    prop_assert!(sim.reference().session_ends().is_empty());

    match next {
        Some(g) => {
            // Passed over F (no tab in 1 s); the next window takes the tab.
            prop_assert!(
                log[0].ends_with(" outcome=folded-foreground into=fallback passed=tab-not-created-1s"),
                "{}",
                log[0]
            );
            prop_assert_eq!(creates.len(), 2);
            prop_assert!(create_target(creates.last().map(|c| &c.command)) == Some(g), "CreateTab into G");
            prop_assert!(sim.window(cand).is_none(), "the candidate is folded");
            let window = sim.window(g).expect("G");
            let (_, g_before, _) = before.iter().find(|(w, ..)| *w == g).expect("G");
            let mut expected = g_before.clone();
            expected.push(l_key);
            prop_assert_eq!(&tab_keys(sim, g), &expected);
            prop_assert_eq!(window.active, g_before.len());
            prop_assert_eq!(sim.foreground(), Some(g));
            for (win, tabs, active) in &before {
                if *win != g {
                    prop_assert_eq!(&tab_keys(sim, *win), tabs, "window {:?}", win);
                    prop_assert_eq!(sim.window(*win).map(|w| w.active), Some(*active));
                }
            }
            Ok(true)
        }
        None => {
            // No other window: the candidate is left open.
            prop_assert!(log[0].contains(" outcome=not-folded"), "{}", log[0]);
            prop_assert!(sim.window(cand).is_some(), "the candidate is left open");
            let (_, f_before, _) = &before[0];
            prop_assert_eq!(&tab_keys(sim, f), f_before);
            prop_assert_eq!(creates.len(), 1);
            Ok(false)
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn a_candidate_on_the_foreground_active_tab_adds_one_rightmost_tab(sc in fg_scenario()) {
        run(&sc)?;
    }

    #[test]
    fn a_failing_foreground_target_follows_the_failure_chain(
        sc in fg_scenario(),
        selection in selection_strategy(),
    ) {
        run_failing(&sc.windows, sc.f, &selection)?;
    }
}

// ---------------------------------------------------------------------------
// Examples
// ---------------------------------------------------------------------------

/// The generated scenarios reach both kinds of exception, Ctrl+N included,
/// and the control.
#[test]
fn the_scenarios_cover_the_exception_ctrl_n_and_the_control() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;

    let mut runner = TestRunner::deterministic();
    let mut total = Tally::default();
    let mut folded_elsewhere = 0;
    let mut left_open = 0;
    for _ in 0..150 {
        let sc = fg_scenario().new_tree(&mut runner).expect("a scenario").current();
        let tally = run(&sc).expect("every fold succeeds");
        total.exceptions += tally.exceptions;
        total.ctrl_n += tally.ctrl_n;
        total.controls += tally.controls;
        if run_failing(&sc.windows, sc.f, &[]).expect("the failure chain") {
            folded_elsewhere += 1;
        } else {
            left_open += 1;
        }
    }
    assert!(total.exceptions >= 50 && total.ctrl_n >= 20 && total.controls >= 30, "{total:?}");
    assert!(folded_elsewhere >= 30 && left_open >= 10, "{folded_elsewhere} {left_open}");
}

/// Ctrl+N on a lone window, then the same Location from another application.
#[test]
fn ctrl_n_folds_into_the_foreground_window_and_an_application_switches_to_its_active_tab() {
    let sc = FgScenario {
        windows: vec![WinSpec { tabs: vec![1, 2], active: 1, maximized: false, minimized: false }],
        f: 0,
        steps: vec![
            FgStep::Exception { ctrl_n: true, selection: vec![] },
            FgStep::Elsewhere { selection: vec![], foreground: None },
        ],
    };
    assert_eq!(run(&sc).expect("the steps behave"), Tally { exceptions: 1, ctrl_n: 1, controls: 1 });
}

/// The foreground window cannot create the tab: with a second window the
/// Folded_Tab goes there, alone the candidate stays open.
#[test]
fn a_failing_foreground_window_passes_the_tab_on_or_leaves_the_candidate_open() {
    let one = vec![WinSpec { tabs: vec![3], active: 0, maximized: false, minimized: false }];
    assert!(!run_failing(&one, 0, &[]).expect("the candidate is left open"));
    let two = vec![
        WinSpec { tabs: vec![4, 5], active: 1, maximized: false, minimized: true },
        WinSpec { tabs: vec![3], active: 0, maximized: true, minimized: false },
    ];
    assert!(run_failing(&two, 1, &[ItemId(vec![9])]).expect("the next window takes the tab"));
}
