// Feature: explorer-session-restore, Property 3: Closing the last window or last tab saves the pre-close session
//! Property 3 (task 7.9), checked end to end through the `Engine`.
//!
//! A random operation sequence (Property 1's mix plus process kills) builds a
//! Live_Session in the simulated Explorer; every window but one is then
//! closed, the survivor is optionally reduced to a single tab, and the last
//! window or its last tab is closed with a random event ordering (tabs
//! revoked before and/or after `WindowDestroyed`, within the 300 ms
//! tombstone). The close must yield exactly one urgent `WriteSessionFile`
//! whose text parses to a Pending document with `failed-attempts` 0, the
//! right cause (`LastTabClosed` exactly when the window had one tab and it
//! was revoked before the destruction, else `LastWindowClosed`), and the
//! snapshot `capture()` returned just before the close, plus one SAVE entry.
//!
//! **Validates: Requirements 3.1, 3.2**

use esr_core::engine::{Command, Engine, Environment, Millis, SessionFileRead, StartupInput, WinKey};
use esr_core::format::parse;
use esr_core::logfmt::LogEntry;
use esr_core::model::{SaveCause, Snapshot, Status};
use esr_core::settings::Settings;
use esr_sim::explorer::{
    ops_strategy, rect_strategy, CloseOrder, NavOutcome, Op, OpsConfig, Pick, ProcessChoice, SimConfig,
    SimExplorer, UiaNaming,
};
use esr_sim::reference::canonical;
use esr_sim::restore::{Driver, Stop};
use proptest::prelude::*;

/// How the last window ends.
#[derive(Clone, Debug)]
enum LastClose {
    /// Close the window (`Op::CloseWindow`), after optionally closing tabs
    /// until one is left.
    Window { reduce_to_one: bool, order: CloseOrder },
    /// Close tabs until one is left, then close that last tab (`Op::CloseTab`:
    /// revocation, then destruction).
    Tab { destroy_gap_ms: u16 },
}

#[derive(Clone, Debug)]
struct Scenario {
    config: SimConfig,
    setup: Vec<Op>,
    /// Which window survives the closing of the others.
    survivor: Pick,
    /// Picks of the tabs closed while reducing the survivor to one tab.
    tab_picks: Vec<Pick>,
    last: LastClose,
    /// A fallback window, opened if the setup left none.
    fallback: Op,
}

fn sim_config() -> impl Strategy<Value = SimConfig> {
    prop_oneof![
        Just(SimConfig::default()),
        Just(SimConfig { uia_naming: UiaNaming::FullPaths, ..SimConfig::default() }),
        Just(SimConfig { report_active_key: true, reuse_hwnds: false, ..SimConfig::default() }),
    ]
}

fn close_order() -> impl Strategy<Value = CloseOrder> {
    (any::<u8>(), any::<bool>(), 0u16..=120, any::<u16>()).prop_map(|(revoke_before, revoke_after, gap_ms, r)| {
        CloseOrder { revoke_before, revoke_after, gap_ms, rotate: Pick(r) }
    })
}

fn last_close() -> impl Strategy<Value = LastClose> {
    prop_oneof![
        2 => (prop::bool::weighted(0.35), close_order())
            .prop_map(|(reduce_to_one, order)| LastClose::Window { reduce_to_one, order }),
        1 => (0u16..=250).prop_map(|destroy_gap_ms| LastClose::Tab { destroy_gap_ms }),
    ]
}

fn scenario() -> impl Strategy<Value = Scenario> {
    // Unidentifiable navigations are left out: their UIA names match nothing,
    // which makes the reference cross-check inexact (esr-sim docs).
    let ops = OpsConfig { unidentifiable: false, ..OpsConfig::all() };
    let fallback = (any::<u16>(), rect_strategy()).prop_map(|(l, rect)| Op::OpenWindow {
        process: ProcessChoice::Main,
        location: Pick(l),
        nav: NavOutcome::Complete,
        rect,
        maximized: false,
    });
    (
        sim_config(),
        ops_strategy(&ops, 0..40),
        any::<u16>(),
        prop::collection::vec(any::<u16>().prop_map(Pick), 8),
        last_close(),
        fallback,
    )
        .prop_map(|(config, setup, survivor, tab_picks, last, fallback)| Scenario {
            config,
            setup,
            survivor: Pick(survivor),
            tab_picks,
            last,
            fallback,
        })
}

/// The Engine, driven by the simulated Explorer.
struct Harness {
    engine: Engine,
    driver: Driver,
}

impl Harness {
    fn new(config: SimConfig) -> Self {
        let startup = StartupInput {
            session_file: SessionFileRead::Absent,
            environment: Environment::default(),
            background_logon: false,
            logon_at: None,
            shell_ready_at: Some(Millis(0)),
        };
        let (engine, commands) = Engine::new(Settings::default(), startup);
        let mut driver = Driver::new(SimExplorer::new(config));
        driver.set_deadline(Millis(u64::MAX / 2));
        driver.submit(commands);
        Harness { engine, driver }
    }

    fn sim(&self) -> &SimExplorer {
        self.driver.sim()
    }

    /// Plays `op` and runs until quiescence; returns every command the Engine
    /// issued meanwhile.
    fn play(&mut self, op: &Op) -> Vec<Command> {
        let events = self.driver.sim_mut().apply(op);
        self.driver.deliver(events);
        let engine = &mut self.engine;
        let mut issued = Vec::new();
        let stop = self.driver.run(|now, e| {
            let commands = engine.handle(now, e);
            issued.extend(commands.iter().cloned());
            commands
        });
        assert_eq!(stop, Stop::Quiescent, "after {op:?}");
        issued
    }

    fn index_of(&self, win: WinKey) -> Option<usize> {
        self.sim().windows().iter().position(|w| w.key == win)
    }
}

/// Field-exact snapshot equality (`LocationId`'s `==` ignores PIDL bytes and
/// display names).
fn assert_identical(actual: &Snapshot, expected: &Snapshot) {
    assert_eq!(actual, expected);
    for (a, e) in actual.windows.iter().zip(&expected.windows) {
        for (ta, te) in a.tabs.iter().zip(&e.tabs) {
            assert!(ta.location.identical(&te.location), "{:?} vs {:?}", ta.location, te.location);
        }
    }
}

fn check(s: &Scenario) {
    let mut h = Harness::new(s.config.clone());
    let mut shell_lost = false;
    for op in &s.setup {
        h.play(op);
        shell_lost |= h.sim().shell_lost();
    }
    // The shell is needed to see the close (and the shell's process may have
    // been killed): restore it, which also rescans every window.
    h.play(&Op::RestoreShell);
    if h.sim().windows().is_empty() {
        h.play(&s.fallback);
    }
    assert!(!h.sim().windows().is_empty(), "no window to close: {:?}", s.setup);

    // Close every window but the survivor.
    let n = h.sim().windows().len();
    let survivor = h.sim().windows()[s.survivor.of(n).expect("windows")].key;
    while let Some(i) = h.sim().windows().iter().position(|w| w.key != survivor) {
        let order = CloseOrder { revoke_before: 1, revoke_after: true, gap_ms: 10, rotate: Pick(0) };
        h.play(&Op::CloseWindow { win: Pick(i as u16), order });
    }

    // Optionally close tabs until one is left.
    let reduce = matches!(s.last, LastClose::Tab { .. } | LastClose::Window { reduce_to_one: true, .. });
    if reduce {
        let mut picks = s.tab_picks.iter().cycle();
        loop {
            let wi = h.index_of(survivor).expect("survivor open");
            if h.sim().windows()[wi].tabs.len() <= 1 {
                break;
            }
            let tab = *picks.next().expect("cycle");
            h.play(&Op::CloseTab { win: Pick(wi as u16), tab, destroy_gap_ms: 0 });
        }
    }

    let wi = h.index_of(survivor).expect("survivor open");
    let tabs = h.sim().windows()[wi].tabs.len();
    assert!(tabs >= 1);
    assert_eq!(h.engine.tracker().open_window_count(), 1, "only the survivor is open");
    let (op, expected_cause) = match &s.last {
        LastClose::Window { order, .. } => {
            // `revoke_before % (tabs + 1)` tabs are revoked before the destroy.
            let revoked_first = usize::from(order.revoke_before) % (tabs + 1);
            let cause = if tabs == 1 && revoked_first == 1 {
                SaveCause::LastTabClosed
            } else {
                SaveCause::LastWindowClosed
            };
            (Op::CloseWindow { win: Pick(wi as u16), order: *order }, cause)
        }
        LastClose::Tab { destroy_gap_ms } => {
            assert_eq!(tabs, 1);
            (
                Op::CloseTab { win: Pick(wi as u16), tab: Pick(0), destroy_gap_ms: *destroy_gap_ms },
                SaveCause::LastTabClosed,
            )
        }
    };

    // The pre-close capture, just before the first revocation.
    let (before, _) = h.engine.tracker().capture();
    let ref_ends = h.sim().reference().session_ends().len();

    let issued = h.play(&op);

    // Exactly one session write: urgent, Pending, no failed attempts, the
    // right cause, and the pre-close snapshot.
    let writes: Vec<(&String, bool)> = issued
        .iter()
        .filter_map(|c| match c {
            Command::WriteSessionFile { text, urgent, .. } => Some((text, *urgent)),
            _ => None,
        })
        .collect();
    assert_eq!(writes.len(), 1, "{issued:#?}");
    let (text, urgent) = writes[0];
    assert!(urgent, "the Session_End write is urgent");
    let doc = parse(text.as_bytes()).unwrap_or_else(|e| panic!("{e:?} in\n{text}"));
    assert_eq!(doc.status, Status::Pending);
    assert_eq!(doc.failed_attempts, 0);
    assert_eq!(doc.cause, expected_cause);
    assert_identical(&doc.snapshot, &before);

    // One SAVE entry with the snapshot's counts.
    let saves: Vec<&LogEntry> = issued
        .iter()
        .filter_map(|c| match c {
            Command::AppendLog(e @ LogEntry::Save { .. }) => Some(e),
            _ => None,
        })
        .collect();
    assert_eq!(
        saves,
        vec![&LogEntry::Save {
            cause: expected_cause,
            windows: before.windows.len() as u32,
            tabs: before.tab_count() as u32,
        }]
    );

    // The Session_End emptied the Live_Session and the write became durable.
    assert!(h.engine.tracker().is_empty());
    assert_eq!(h.engine.store().pending_snapshot(), Some(&doc));
    assert_eq!(h.engine.store().durable().doc(), Some(&doc));

    // Cross-check against the reference model: one Session_End, same cause
    // and snapshot. Windows opened or changed while the shell was lost (also
    // by killing the shell's process) in a process killed before the shell
    // came back are invisible to the tracker for good (an orphan cannot be
    // rescanned), so the snapshot is compared only if the shell was never
    // lost (as in `tracker_smoke`).
    let ends = h.sim().reference().session_ends();
    assert_eq!(ends.len(), ref_ends + 1);
    let end = ends.last().expect("one more");
    assert_eq!(end.cause, expected_cause);
    if !shell_lost {
        assert_eq!(canonical(&end.snapshot), canonical(&before));
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

    #[test]
    fn last_window_or_tab_close_saves_the_pre_close_session(s in scenario()) {
        check(&s);
    }
}

/// The two orderings of a single-tab window close, and a multi-tab close
/// with every tab revoked before the destroy.
#[test]
fn single_tab_orderings_decide_the_cause() {
    let open = Op::OpenWindow {
        process: ProcessChoice::Main,
        location: Pick(0),
        nav: NavOutcome::Complete,
        rect: esr_core::model::Rect { left: 0, top: 0, right: 800, bottom: 600 },
        maximized: false,
    };
    let base = Scenario {
        config: SimConfig::default(),
        setup: vec![open.clone()],
        survivor: Pick(0),
        tab_picks: vec![Pick(0)],
        last: LastClose::Tab { destroy_gap_ms: 40 },
        fallback: open.clone(),
    };
    // Revocation, then destruction: LastTabClosed.
    check(&base);
    // Destruction first, the tab revoked after (or never): LastWindowClosed.
    for revoke_after in [false, true] {
        let order = CloseOrder { revoke_before: 0, revoke_after, gap_ms: 30, rotate: Pick(0) };
        check(&Scenario { last: LastClose::Window { reduce_to_one: false, order }, ..base.clone() });
    }
    // Three tabs, all revoked before the destroy: LastWindowClosed.
    let tab = Op::OpenTab { win: Pick(0), location: Pick(2), nav: NavOutcome::Complete };
    let order = CloseOrder { revoke_before: 3, revoke_after: false, gap_ms: 50, rotate: Pick(1) };
    check(&Scenario {
        setup: vec![open, tab.clone(), tab],
        last: LastClose::Window { reduce_to_one: false, order },
        ..base
    });
}
