//! Unit tests for Tab_Folding examples (task 23.7): the `Engine` (with its
//! `FoldingEngine`) against the simulated Explorer.
//!
//! - a duplicated tab produces no fold command (Req 14.5);
//! - a candidate opened while only another candidate is open on its
//!   Virtual_Desktop stays a separate window (Req 14.10);
//! - a candidate in a separate File Explorer process folds like any other,
//!   and folds into a window of a separate process (Req 14.29);
//! - turning folding off while a fold is in progress leaves the candidate
//!   open (Req 12.10, 14.14);
//! - Ctrl+N on the Active_Tab of the foreground window adds a tab there
//!   (Req 14.22), while the same Location opened from another application
//!   switches to the existing tab (Req 14.21).
//!
//! **Validates: Requirements 14.5, 14.10, 14.21, 14.22, 14.29**

use esr_core::engine::{
    Command, Engine, Environment, Event, ExplorerOptionValues, Millis, SessionFileRead, StartupInput, TimerOwner, WinKey,
};
use esr_core::logfmt::entry::{format_entry, LogEntry};
use esr_core::model::{LocationId, Rect, Timestamp};
use esr_core::settings::Settings;
use esr_sim::explorer::{
    default_displays, default_locations, NavOutcome, Op, Pick, ProcessChoice, SimConfig, SimExplorer,
};
use esr_sim::fold::{DesktopPlace, DesktopSpec, FoldConfig};
use esr_sim::reference::canonical;
use esr_sim::restore::{CommandKind, Driver, Stop};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// The Engine and the simulated Explorer it tracks.
struct Harness {
    engine: Engine,
    driver: Driver,
    /// The windows of the setup, in creation order.
    wins: Vec<WinKey>,
}

fn rect(n: i32) -> Rect {
    Rect { left: 100 + 60 * n, top: 80 + 40 * n, right: 900 + 60 * n, bottom: 680 + 40 * n }
}

/// A new window `n` showing pool Location `location`.
fn open(process: ProcessChoice, location: u16, n: i32) -> Op {
    Op::OpenWindow { process, location: Pick(location), nav: NavOutcome::Complete, rect: rect(n), maximized: false }
}

/// A new tab in window `win` showing pool Location `location`.
fn tab(win: u16, location: u16) -> Op {
    Op::OpenTab { win: Pick(win), location: Pick(location), nav: NavOutcome::Complete }
}

/// A candidate on pool Location `location`, opened from another application,
/// in the shell's process.
fn candidate_from_app(location: u16) -> Op {
    Op::OpenCandidate {
        location: Pick(location),
        selection: vec![],
        bypass_keys: false,
        desktop: DesktopSpec::Current,
        foreground: None,
        process: ProcessChoice::Main,
    }
}

impl Harness {
    /// Builds the session with `setup`, tracks it (folding on, the default),
    /// and settles.
    fn start(fold: FoldConfig, setup: &[Op]) -> Self {
        let mut sim = SimExplorer::new(SimConfig { fold, ..SimConfig::default() });
        let mut events = Vec::new();
        for op in setup {
            events.extend(sim.apply(op));
        }
        events.extend(sim.settle());
        let wins: Vec<WinKey> = sim.windows().iter().map(|w| w.key).collect();

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
        let (engine, commands) = Engine::new(Settings::default(), startup);
        let mut driver = Driver::new(sim);
        for owner in [TimerOwner::Store, TimerOwner::Control] {
            driver.quiet_timers(owner);
        }
        driver.submit(commands);
        driver.deliver(events);
        let mut h = Harness { engine, driver, wins };
        h.run();
        // The windows already open are ranked in their Activation_Order.
        h.driver.deliver_now(Event::ShellAvailable);
        h.run();
        h
    }

    fn sim(&self) -> &SimExplorer {
        self.driver.sim()
    }

    fn now(&self) -> u64 {
        self.sim().now().0
    }

    /// Runs until nothing is scheduled any more.
    fn run(&mut self) {
        let Harness { engine, driver, .. } = self;
        driver.set_deadline(Millis(driver.sim().now().0 + 600_000));
        assert_eq!(driver.run(|now, event| engine.handle(now, event)), Stop::Quiescent);
    }

    /// Runs until logical time `until`.
    fn run_until(&mut self, until: u64) {
        let Harness { engine, driver, .. } = self;
        driver.set_deadline(Millis(until));
        let stop = driver.run(|now, event| engine.handle(now, event));
        assert!(matches!(stop, Stop::Quiescent | Stop::Deadline), "{stop:?}");
    }

    /// Applies `op` and queues its events; returns the window of the
    /// candidate it opened, if any. Nothing runs yet.
    fn apply(&mut self, op: &Op) -> Option<WinKey> {
        let before = self.sim().candidates().len();
        let events = self.driver.sim_mut().apply(op);
        self.driver.deliver(events);
        (self.sim().candidates().len() > before).then(|| self.sim().candidates().last().expect("a candidate").win)
    }

    /// Applies `op` and runs until nothing is scheduled.
    fn act(&mut self, op: &Op) -> Option<WinKey> {
        let cand = self.apply(op);
        self.run();
        cand
    }

    fn settings_changed(&mut self, folding: bool) {
        let settings = Settings { tab_folding_enabled: folding, ..Settings::default() };
        self.driver.deliver_now(Event::SettingsChanged(settings));
    }

    /// The kinds of the commands the simulator received after the first `n`.
    fn kinds_since(&self, n: usize) -> Vec<CommandKind> {
        self.sim().command_log()[n..].iter().map(|c| CommandKind::of(&c.command)).collect()
    }

    /// The tracker agrees with the simulated Explorer.
    fn assert_tracked_as_open(&self) {
        let tracked = canonical(&self.engine.tracker().capture().0);
        let expected = canonical(&self.sim().reference().snapshot());
        assert_eq!(tracked, expected);
    }
}

fn name(l: &LocationId) -> String {
    l.parsing_name.to_string_lossy().to_string()
}

/// The parsing name of pool Location `n`.
fn pool(n: usize) -> String {
    name(&default_locations()[n])
}

/// The Locations the tabs of `win` show, left to right.
fn shown(sim: &SimExplorer, win: WinKey) -> Vec<Option<String>> {
    sim.window(win).expect("the window is open").tabs.iter().map(|t| t.last_known.as_ref().map(name)).collect()
}

/// The expected `shown` of tabs on the pool Locations `ns`.
fn tabs(ns: &[usize]) -> Vec<Option<String>> {
    ns.iter().map(|&n| Some(pool(n))).collect()
}

const TS: Timestamp = Timestamp { year: 2025, month: 1, day: 31, hour: 9, minute: 15, second: 2, millis: 123, offset_minutes: 0 };

/// The `outcome=…` part of every `FOLD` entry written, in order.
fn fold_outcomes(sim: &SimExplorer) -> Vec<String> {
    sim.log_entries()
        .iter()
        .filter(|(_, entry)| matches!(entry, LogEntry::Fold { .. }))
        .map(|(_, entry)| {
            let line = format_entry(&TS, entry);
            let event = line.splitn(4, ' ').nth(3).expect("a timestamp and an event").to_owned();
            assert!(event.starts_with("FOLD location="), "{event}");
            event.split_once(" outcome=").expect("an outcome").1.to_owned()
        })
        .collect()
}

fn count(kinds: &[CommandKind], kind: CommandKind) -> usize {
    kinds.iter().filter(|&&k| k == kind).count()
}

// ---------------------------------------------------------------------------
// Req 14.5: a duplicated tab
// ---------------------------------------------------------------------------

#[test]
fn a_duplicated_tab_produces_no_fold_command() {
    // Two windows; the first is the one in front, with its first tab active.
    let mut h = Harness::start(
        FoldConfig::default(),
        &[
            open(ProcessChoice::Main, 2, 1),
            open(ProcessChoice::Main, 4, 0),
            tab(1, 3),
            Op::ActivateTab { win: Pick(1), tab: Pick(0) },
            Op::ActivateWindow { win: Pick(1) },
        ],
    );
    let (first, second) = (h.wins[0], h.wins[1]);
    let opening = h.engine.folding().opening().to_vec();
    assert_eq!(opening.len(), 2);
    let log_before = h.sim().command_log().len();

    // "Duplicate tab" on the Active_Tab of the window in front: a copy of the
    // tab, which shows the Location of an open tab, and no window.
    h.act(&Op::DuplicateTab { win: Pick(1) });

    let sim = h.sim();
    assert_eq!(sim.windows().len(), 2, "a duplicated tab opens no window");
    assert_eq!(shown(sim, second).len(), 3, "the copy stays in the window where File Explorer put it");
    assert_eq!(shown(sim, first), tabs(&[2]));
    assert_eq!(shown(sim, second).iter().filter(|s| **s == Some(pool(4))).count(), 2);

    // The FoldingEngine did nothing: no candidate, no command that hides,
    // creates, selects, activates or closes anything, no FOLD entry.
    let kinds = h.kinds_since(log_before);
    for kind in [
        CommandKind::HideWindow,
        CommandKind::ProbeFoldContext,
        CommandKind::CreateTab,
        CommandKind::Navigate,
        CommandKind::ActivateTab,
        CommandKind::SelectItems,
        CommandKind::Reveal,
        CommandKind::Focus,
        CommandKind::CloseTab,
        CommandKind::CloseWindow,
    ] {
        assert_eq!(count(&kinds, kind), 0, "{kind:?} in {kinds:?}");
    }
    assert!(h.sim().candidates().is_empty());
    assert_eq!(h.engine.folding().candidates_in_progress(), 0);
    assert_eq!(h.engine.folding().opening().to_vec(), opening);
    assert!(fold_outcomes(h.sim()).is_empty());

    // The tab is tracked where File Explorer put it, with its Active_Tab.
    h.assert_tracked_as_open();
}

// ---------------------------------------------------------------------------
// Req 14.10: only another candidate is open on the desktop
// ---------------------------------------------------------------------------

#[test]
fn a_candidate_opened_while_only_another_candidate_is_open_stays_separate() {
    let mut h = Harness::start(FoldConfig::default(), &[open(ProcessChoice::Main, 0, 0)]);
    let a = h.wins[0];

    // C1 is a candidate: the window `a` is on its desktop. Its first
    // navigation never completes, so it stays in progress.
    h.act(&Op::SetFirstNavigation { ms: None });
    let c1 = h.apply(&candidate_from_app(2)).expect("the first candidate");
    let t = h.now();
    h.run_until(t + 500);
    assert!(h.engine.folding().owns(c1), "the first window is a Folding_Candidate");
    assert_eq!(h.engine.folding().candidates_in_progress(), 1);

    // `a` leaves the desktop: on it only the candidate C1 remains.
    // (Nothing runs between these steps, so C1 stays in progress.)
    h.apply(&Op::MoveToDesktop { win: Pick(0), to: DesktopPlace::On(1) });
    h.apply(&Op::SetFirstNavigation { ms: Some(150) });
    let c2 = h.apply(&candidate_from_app(4)).expect("the second window");
    let t = h.now();
    h.run_until(t + 1_000);

    // C2 is not a candidate: left as File Explorer opened it, tracked, ranked.
    assert!(h.engine.folding().is_excluded(c2));
    assert!(!h.engine.folding().owns(c2));
    assert!(h.engine.folding().opening().contains(c2));
    assert!(h.engine.tracker().window(c2).is_some(), "an excluded window is tracked (Req 2)");
    assert!(h.sim().is_visible(c2));
    assert_eq!(shown(h.sim(), c2), tabs(&[4]));
    assert_eq!(h.sim().windows().len(), 3);
    // C1 is still a candidate in progress and was not folded into C2.
    assert!(h.engine.folding().owns(c1));
    assert_eq!(h.engine.folding().candidates_in_progress(), 1);

    // It was never hidden, folded into or closed.
    let log = h.sim().command_log();
    let hidden: Vec<WinKey> = log
        .iter()
        .filter_map(|c| match &c.command {
            Command::HideWindow { win } => Some(*win),
            _ => None,
        })
        .collect();
    assert_eq!(hidden, vec![c1]);
    assert_eq!(count(&h.kinds_since(0), CommandKind::CreateTab), 0);
    assert_eq!(count(&h.kinds_since(0), CommandKind::CloseWindow), 0);
    // Not a Trigger_Window either (Req 14.10, 4.2).
    assert_eq!(count(&h.kinds_since(0), CommandKind::OpenWindow), 0);

    // C1 times out after 8 s and is left open too: three separate windows.
    h.run();
    assert_eq!(h.engine.folding().candidates_in_progress(), 0);
    assert!(h.sim().is_visible(c1) && h.sim().is_visible(c2) && h.sim().is_visible(a));
    assert_eq!(h.sim().windows().len(), 3);
    assert!(fold_outcomes(h.sim()).is_empty());
}

// ---------------------------------------------------------------------------
// Req 14.29: separate File Explorer processes
// ---------------------------------------------------------------------------

/// A window in the shell's process (`wins[0]`, Location 0) and one in a
/// separate process (`wins[1]`, Locations 1 and 2, the first tab active); the
/// window activated last is the Primary_Window.
fn two_processes(separate_is_primary: bool) -> Harness {
    let last = if separate_is_primary { 1 } else { 0 };
    let h = Harness::start(
        FoldConfig::default(),
        &[
            open(ProcessChoice::Main, 0, 0),
            open(ProcessChoice::New, 1, 1),
            tab(1, 2),
            Op::ActivateTab { win: Pick(1), tab: Pick(0) },
            Op::ActivateWindow { win: Pick(1 - last) },
            Op::ActivateWindow { win: Pick(last) },
        ],
    );
    assert_ne!(h.sim().window(h.wins[0]).unwrap().pid, h.sim().window(h.wins[1]).unwrap().pid);
    h
}

#[test]
fn a_candidate_of_a_separate_process_folds_into_the_primary_window() {
    let mut h = two_processes(false);
    let (main, separate) = (h.wins[0], h.wins[1]);

    let cand = h
        .apply(&Op::OpenCandidate {
            location: Pick(5),
            selection: vec![],
            bypass_keys: false,
            desktop: DesktopSpec::Current,
            foreground: None,
            process: ProcessChoice::New,
        })
        .expect("a candidate");
    let pid = |h: &Harness, win: WinKey| h.sim().window(win).expect("open").pid;
    assert_ne!(pid(&h, cand), pid(&h, main), "the candidate is hosted by a separate process");
    h.run();

    let sim = h.sim();
    assert!(sim.window(cand).is_none(), "the candidate is closed");
    assert_eq!(shown(sim, main), tabs(&[0, 5]));
    assert_eq!(sim.window(main).unwrap().active, 1);
    assert_eq!(shown(sim, separate), tabs(&[1, 2]));
    assert_eq!(sim.foreground(), Some(main));
    assert_eq!(fold_outcomes(sim), vec!["folded-primary".to_owned()]);
    assert_eq!(h.engine.folding().candidates_in_progress(), 0);
}

#[test]
fn a_candidate_folds_into_a_primary_window_of_a_separate_process() {
    let mut h = two_processes(true);
    let (main, separate) = (h.wins[0], h.wins[1]);

    let cand = h.act(&candidate_from_app(5)).expect("a candidate");
    let sim = h.sim();
    assert!(sim.window(cand).is_none(), "the candidate is closed");
    assert_eq!(shown(sim, separate), tabs(&[1, 2, 5]));
    assert_eq!(sim.window(separate).unwrap().active, 2);
    assert_eq!(shown(sim, main), tabs(&[0]));
    assert_eq!(sim.foreground(), Some(separate));
    assert_eq!(fold_outcomes(sim), vec!["folded-primary".to_owned()]);
}

#[test]
fn a_tab_switch_reaches_a_matching_tab_in_a_window_of_a_separate_process() {
    // The shell's process holds the Primary_Window; the Matching_Tab is the
    // second tab of the window of the separate process.
    let mut h = two_processes(false);
    let (main, separate) = (h.wins[0], h.wins[1]);

    let cand = h.act(&candidate_from_app(2)).expect("a candidate");
    let sim = h.sim();
    assert!(sim.window(cand).is_none(), "the candidate is closed");
    assert_eq!(shown(sim, separate), tabs(&[1, 2]), "no tab is added");
    assert_eq!(sim.window(separate).unwrap().active, 1, "the Matching_Tab is the Active_Tab");
    assert_eq!(shown(sim, main), tabs(&[0]));
    assert_eq!(sim.foreground(), Some(separate));
    assert_eq!(fold_outcomes(sim), vec!["switched matching=other".to_owned()]);
    assert_eq!(count(&h.kinds_since(0), CommandKind::CreateTab), 0);
}

// ---------------------------------------------------------------------------
// Req 12.10, 14.14: folding turned off while a fold is in progress
// ---------------------------------------------------------------------------

#[test]
fn turning_folding_off_after_the_folded_tab_was_created_leaves_the_candidate_open() {
    let fold = FoldConfig { report_tab_started: true, registration_lag_ms: 600, ..FoldConfig::default() };
    let mut h = Harness::start(fold, &[open(ProcessChoice::Main, 0, 0)]);
    let a = h.wins[0];

    let cand = h.apply(&candidate_from_app(5)).expect("a candidate");
    // Run until the Folded_Tab exists in the Primary_Window.
    let mut t = h.now();
    while h.sim().window(a).unwrap().tabs.len() < 2 {
        t += 10;
        assert!(t < h.now() + 5_000 && t < 20_000, "no Folded_Tab was ever created");
        h.run_until(t);
    }
    assert_eq!(h.engine.folding().candidates_in_progress(), 1, "the fold is in progress");
    assert!(h.engine.folding().owns(cand));

    // The user turns folding off in Settings.
    h.settings_changed(false);
    h.run();

    let sim = h.sim();
    assert_eq!(h.engine.folding().candidates_in_progress(), 0);
    assert!(h.engine.folding().is_excluded(cand) && !h.engine.folding().owns(cand));
    assert!(h.engine.folding().opening().contains(cand));
    // The candidate is open and visible, as File Explorer opened it.
    assert!(sim.is_visible(cand));
    assert_eq!(shown(sim, cand), tabs(&[5]));
    assert_eq!(count(&h.kinds_since(0), CommandKind::CloseWindow), 0);
    // The tab created for it is closed again.
    assert_eq!(shown(sim, a), tabs(&[0]));
    assert_eq!(sim.window(a).unwrap().active, 0);
    assert_eq!(fold_outcomes(sim), vec!["not-folded reason=folding-disabled".to_owned()]);
    h.assert_tracked_as_open();

    // Windows that open from now on are not candidates.
    let later = h.act(&candidate_from_app(7)).expect("a window");
    assert!(h.engine.folding().is_excluded(later));
    assert!(h.sim().is_visible(later));
    assert_eq!(h.sim().windows().len(), 3);
    assert_eq!(fold_outcomes(h.sim()).len(), 1);
}

#[test]
fn turning_folding_off_before_the_first_navigation_reveals_the_candidate() {
    let mut h = Harness::start(FoldConfig::default(), &[open(ProcessChoice::Main, 0, 0)]);
    let a = h.wins[0];

    h.act(&Op::SetFirstNavigation { ms: None });
    let cand = h.apply(&candidate_from_app(5)).expect("a candidate");
    let t = h.now();
    h.run_until(t + 500);
    assert!(h.engine.folding().owns(cand), "hidden and waiting for its first navigation");
    assert!(!h.sim().is_visible(cand));

    h.settings_changed(false);
    h.run();

    // Open and visible at once; no Location was known, so no FOLD entry.
    assert!(h.sim().is_visible(cand));
    assert!(h.engine.folding().is_excluded(cand) && !h.engine.folding().owns(cand));
    assert!(h.engine.tracker().window(cand).is_some());
    assert_eq!(h.sim().windows().len(), 2);
    assert_eq!(shown(h.sim(), a), tabs(&[0]));
    assert!(fold_outcomes(h.sim()).is_empty());
    assert_eq!(count(&h.kinds_since(0), CommandKind::CloseWindow), 0);
}

// ---------------------------------------------------------------------------
// Req 14.21, 14.22: Ctrl+N on the foreground window, Tab_Switch
// ---------------------------------------------------------------------------

/// One window showing Locations 0 and 3 with the tab `active` selected, in
/// front.
fn one_window(active: u16) -> Harness {
    Harness::start(
        FoldConfig::default(),
        &[
            open(ProcessChoice::Main, 0, 0),
            tab(0, 3),
            Op::ActivateTab { win: Pick(0), tab: Pick(active) },
            Op::ActivateWindow { win: Pick(0) },
        ],
    )
}

#[test]
fn ctrl_n_on_the_active_tab_of_the_foreground_window_adds_a_tab_there() {
    let mut h = one_window(0);
    let a = h.wins[0];
    assert_eq!(h.sim().foreground(), Some(a));
    assert_eq!(shown(h.sim(), a), tabs(&[0, 3]));
    let log_before = h.sim().command_log().len();

    // The new window shows the Location of the Active_Tab, which is the
    // Matching_Tab and the Active_Tab of the foreground window.
    let cand = h.act(&Op::NewWindow { bypass_keys: false }).expect("a candidate");

    let sim = h.sim();
    assert!(sim.window(cand).is_none(), "the candidate is closed");
    assert_eq!(shown(sim, a), tabs(&[0, 3, 0]), "a Folded_Tab, though a tab shows the Location already");
    assert_eq!(sim.window(a).unwrap().active, 2);
    assert_eq!(sim.foreground(), Some(a));
    assert_eq!(fold_outcomes(sim), vec!["folded-foreground into=foreground passed=".to_owned()]);
    let kinds = h.kinds_since(log_before);
    assert_eq!(count(&kinds, CommandKind::CreateTab), 1, "{kinds:?}");
    assert_eq!(count(&kinds, CommandKind::CloseTab), 0, "{kinds:?}");
    let activations: Vec<usize> = h.sim().command_log()[log_before..]
        .iter()
        .filter_map(|c| match &c.command {
            Command::ActivateTab { index, .. } => Some(*index),
            _ => None,
        })
        .collect();
    assert_eq!(activations, vec![2], "only the Folded_Tab is activated");
    assert_eq!(h.engine.folding().candidates_in_progress(), 0);
    h.assert_tracked_as_open();
}

#[test]
fn the_same_location_opened_from_another_application_switches_to_the_existing_tab() {
    // Whether the Matching_Tab is the Active_Tab or another tab, it becomes
    // the Active_Tab and no tab is added.
    for active in [0u16, 1] {
        let mut h = one_window(active);
        let a = h.wins[0];
        let log_before = h.sim().command_log().len();

        // "Show in folder": an application is in the foreground.
        h.act(&Op::FocusApplication);
        assert_eq!(h.sim().foreground(), None);
        let cand = h.act(&candidate_from_app(0)).expect("a candidate");

        let sim = h.sim();
        assert!(sim.window(cand).is_none(), "the candidate is closed (active={active})");
        assert_eq!(shown(sim, a), tabs(&[0, 3]), "no tab is added (active={active})");
        assert_eq!(sim.window(a).unwrap().active, 0, "the Matching_Tab is active (active={active})");
        assert_eq!(sim.foreground(), Some(a));
        assert_eq!(fold_outcomes(sim), vec!["switched matching=primary".to_owned()], "active={active}");
        let kinds = h.kinds_since(log_before);
        assert_eq!(count(&kinds, CommandKind::CreateTab), 0, "{kinds:?}");
        assert_eq!(count(&kinds, CommandKind::CloseTab), 0, "{kinds:?}");
        assert_eq!(h.engine.folding().candidates_in_progress(), 0);
        h.assert_tracked_as_open();
    }
}
