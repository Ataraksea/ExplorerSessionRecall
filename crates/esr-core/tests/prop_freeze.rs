// Feature: explorer-session-restore, Property 5: Session-end freeze
//! Property 5 (task 7.11), checked end to end through the `Engine`.
//!
//! A random operation sequence (Property 1's mix plus process kills) builds a
//! Live_Session in the simulated Explorer, optionally ending with every
//! window closed. An `EndSessionQuery` (sign-out, shutdown, or restart) is
//! then delivered, either after quiescence or right after the events of a
//! last operation (so in the middle of a close, before its tombstones
//! expire). What the sign-out does next follows: random operations weighted
//! towards window and tab closes and process kills, optionally every
//! remaining window closed, and optionally a repeated query.
//!
//! - If a window was tracked at the query, every query yields exactly one
//!   urgent `WriteSessionFile`: a Pending document with `failed-attempts` 0,
//!   the query's cause, and (field-exact) the capture at the query. Nothing
//!   else is written, so the closes after the query never change what is
//!   saved; the Session_File ends up holding that document, and the frozen
//!   Live_Session still captures the same snapshot (Req 3.5).
//! - If no window was tracked, nothing is written from the query on and the
//!   Session_File (as the simulator and the store see it) holds what it held
//!   at the query (Req 3.11).
//!
//! **Validates: Requirements 3.5, 3.11**

use esr_core::engine::{
    Command, EndKind, Engine, Environment, Event, Millis, SessionFileRead, StartupInput, TimerId, TimerOwner,
};
use esr_core::format::parse;
use esr_core::logfmt::LogEntry;
use esr_core::model::{Rect, SaveCause, Snapshot, Status};
use esr_core::settings::Settings;
use esr_core::tracker::LiveSessionTracker;
use esr_sim::explorer::{
    op_strategy, ops_strategy, CloseOrder, NavOutcome, Op, OpsConfig, Pick, ProcessChoice,
    SimConfig, SimExplorer, UiaNaming,
};
use esr_sim::restore::{Driver, Stop};
use proptest::prelude::*;

#[derive(Clone, Debug)]
struct Scenario {
    config: SimConfig,
    setup: Vec<Op>,
    /// Close every window before the query (the empty-session case).
    close_all_first: bool,
    /// Deliver the query right after this operation's events, before the
    /// run reaches quiescence (`None`: after quiescence).
    query_mid_op: Option<Op>,
    kind: EndKind,
    /// What happens after the query (the sign-out closing windows, and more).
    during: Vec<Op>,
    /// Close every remaining window at the end, as the sign-out does.
    close_rest: Option<CloseOrder>,
    /// A repeated query at the end.
    repeat: Option<EndKind>,
}

fn sim_config() -> impl Strategy<Value = SimConfig> {
    prop_oneof![
        Just(SimConfig::default()),
        Just(SimConfig { uia_naming: UiaNaming::FullPaths, ..SimConfig::default() }),
        Just(SimConfig { report_active_key: true, reuse_hwnds: false, ..SimConfig::default() }),
    ]
}

fn end_kind() -> impl Strategy<Value = EndKind> {
    prop_oneof![Just(EndKind::SignOut), Just(EndKind::Shutdown), Just(EndKind::Restart)]
}

fn close_order() -> impl Strategy<Value = CloseOrder> {
    (any::<u8>(), any::<bool>(), 0u16..=120, any::<u16>()).prop_map(|(revoke_before, revoke_after, gap_ms, r)| {
        CloseOrder { revoke_before, revoke_after, gap_ms, rotate: Pick(r) }
    })
}

/// Operations after the query: mostly closes and kills, plus anything else.
fn during_op(ops: &OpsConfig) -> BoxedStrategy<Op> {
    prop_oneof![
        3 => (any::<u16>(), close_order()).prop_map(|(w, order)| Op::CloseWindow { win: Pick(w), order }),
        2 => (any::<u16>(), any::<u16>(), 0u16..=250)
            .prop_map(|(w, t, destroy_gap_ms)| Op::CloseTab { win: Pick(w), tab: Pick(t), destroy_gap_ms }),
        1 => any::<u16>().prop_map(|p| Op::KillProcess { pid: Pick(p) }),
        2 => op_strategy(ops),
    ]
    .boxed()
}

fn scenario() -> impl Strategy<Value = Scenario> {
    let ops = OpsConfig::all();
    (
        sim_config(),
        ops_strategy(&ops, 0..30),
        prop::bool::weighted(0.2),
        prop::option::weighted(0.4, during_op(&ops)),
        end_kind(),
        prop::collection::vec(during_op(&ops), 0..15),
        prop::option::weighted(0.6, close_order()),
        prop::option::weighted(0.3, end_kind()),
    )
        .prop_map(|(config, setup, close_all_first, query_mid_op, kind, during, close_rest, repeat)| Scenario {
            config,
            setup,
            close_all_first,
            query_mid_op,
            kind,
            during,
            close_rest,
            repeat,
        })
}

/// One `EndSessionQuery` as the Engine saw it.
#[derive(Debug)]
struct Query {
    kind: EndKind,
    /// A window (open or orphaned) was tracked.
    tracked: bool,
    /// The capture at the query (tombstones due by then expired).
    capture: Snapshot,
    /// Commands issued while handling the query.
    commands: Vec<Command>,
}

/// The Engine, driven by the simulated Explorer.
struct Harness {
    engine: Engine,
    driver: Driver,
    /// Every query delivered so far.
    queries: Vec<Query>,
}

/// The Live_Session as captured at `now`: a probe event lets tombstones due
/// at `now` expire, as the query itself does before capturing.
fn capture_at(tracker: &LiveSessionTracker, now: Millis) -> Snapshot {
    let mut probe = tracker.clone();
    probe.handle(now, &Event::Timer(TimerId { owner: TimerOwner::Tracker, seq: u64::MAX }));
    probe.capture().0
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
        Harness { engine, driver, queries: Vec::new() }
    }

    fn sim(&self) -> &SimExplorer {
        self.driver.sim()
    }

    /// Plays `op` (if any), then delivers `query` (if any) right after the
    /// op's events, and runs until quiescence. Returns the commands issued
    /// before the first query of this run, and those issued from it on.
    fn play_split(&mut self, op: Option<&Op>, query: Option<EndKind>) -> (Vec<Command>, Vec<Command>) {
        if let Some(op) = op {
            let events = self.driver.sim_mut().apply(op);
            self.driver.deliver(events);
        }
        if let Some(kind) = query {
            self.driver.deliver_now(Event::EndSessionQuery(kind));
        }
        let engine = &mut self.engine;
        let queries = &mut self.queries;
        let (mut before, mut after) = (Vec::new(), Vec::new());
        let mut seen_query = false;
        let stop = self.driver.run(|now, e| {
            let query = match &e {
                Event::EndSessionQuery(kind) => Some(Query {
                    kind: *kind,
                    tracked: !engine.tracker().is_empty(),
                    capture: capture_at(engine.tracker(), now),
                    commands: Vec::new(),
                }),
                _ => None,
            };
            seen_query |= query.is_some();
            let commands = engine.handle(now, e);
            if seen_query { &mut after } else { &mut before }.extend(commands.iter().cloned());
            if let Some(mut q) = query {
                q.commands = commands.clone();
                queries.push(q);
            }
            commands
        });
        assert_eq!(stop, Stop::Quiescent, "after {op:?} / {query:?}");
        (before, after)
    }

    /// [`Harness::play_split`], every command issued.
    fn play(&mut self, op: Option<&Op>, query: Option<EndKind>) -> Vec<Command> {
        let (mut before, after) = self.play_split(op, query);
        before.extend(after);
        before
    }

    /// Closes every window the simulator shows.
    fn close_all(&mut self, order: CloseOrder) -> Vec<Command> {
        let mut issued = Vec::new();
        while !self.sim().windows().is_empty() {
            issued.extend(self.play(Some(&Op::CloseWindow { win: Pick(0), order }), None));
        }
        issued
    }
}

fn cause_of(kind: EndKind) -> SaveCause {
    match kind {
        EndKind::SignOut => SaveCause::SignOut,
        EndKind::Shutdown => SaveCause::Shutdown,
        EndKind::Restart => SaveCause::Restart,
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

fn writes(commands: &[Command]) -> Vec<(&String, bool)> {
    commands
        .iter()
        .filter_map(|c| match c {
            Command::WriteSessionFile { text, urgent, .. } => Some((text, *urgent)),
            _ => None,
        })
        .collect()
}

fn saves(commands: &[Command]) -> Vec<&LogEntry> {
    commands.iter().filter_map(|c| match c {
        Command::AppendLog(e @ LogEntry::Save { .. }) => Some(e),
        _ => None,
    })
    .collect()
}

fn check(s: &Scenario) {
    let mut h = Harness::new(s.config.clone());
    for op in &s.setup {
        h.play(Some(op), None);
    }
    if s.close_all_first {
        // The shell is needed to see the closes.
        h.play(Some(&Op::RestoreShell), None);
        let order = CloseOrder { revoke_before: 1, revoke_after: true, gap_ms: 10, rotate: Pick(0) };
        h.close_all(order);
        assert!(h.engine.tracker().is_empty(), "every window closed");
    }

    // What the Session_File holds before the query (a write issued just
    // before a mid-operation query completes after it).
    let file_before = h.sim().session_file().map(str::to_owned);
    let durable_before = h.engine.store().durable().clone();

    // The query, and everything the sign-out does next.
    let (pre_query, mut after) = h.play_split(s.query_mid_op.as_ref(), Some(s.kind));
    let pre_query_write = writes(&pre_query).last().map(|(text, _)| (*text).clone());
    assert_eq!(h.queries.len(), 1);
    assert!(h.engine.tracker().is_frozen());
    for op in &s.during {
        after.extend(h.play(Some(op), None));
    }
    if let Some(order) = s.close_rest {
        after.extend(h.close_all(order));
    }
    if let Some(kind) = s.repeat {
        after.extend(h.play(None, Some(kind)));
    }

    let first = &h.queries[0];
    let expected = first.capture.clone();
    let tracked = first.tracked;
    assert!(h.queries.iter().all(|q| q.tracked == tracked), "the frozen session keeps its windows");

    if !tracked {
        // Req 3.11: no write, no SAVE, the Session_File unchanged.
        assert!(expected.windows.is_empty());
        assert_eq!(writes(&after), vec![], "{after:#?}");
        assert_eq!(saves(&after), Vec::<&LogEntry>::new());
        match pre_query_write {
            None => {
                assert_eq!(h.sim().session_file().map(str::to_owned), file_before);
                assert_eq!(h.engine.store().durable(), &durable_before);
            }
            Some(text) => {
                assert_eq!(h.sim().session_file(), Some(text.as_str()));
                let doc = parse(text.as_bytes()).expect("parses");
                assert_eq!(h.engine.store().durable().doc(), Some(&doc));
            }
        }
        return;
    }

    // Req 3.5: each query writes the capture at the first query, urgently,
    // and nothing else is written.
    let total_writes = writes(&after).len();
    assert_eq!(total_writes, h.queries.len(), "{after:#?}");
    let mut last_doc = None;
    for q in &h.queries {
        let w = writes(&q.commands);
        assert_eq!(w.len(), 1, "{:#?}", q.commands);
        let (text, urgent) = w[0];
        assert!(urgent, "the Session_End write is urgent");
        let doc = parse(text.as_bytes()).unwrap_or_else(|e| panic!("{e:?} in\n{text}"));
        assert_eq!(doc.status, Status::Pending);
        assert_eq!(doc.failed_attempts, 0);
        assert_eq!(doc.cause, cause_of(q.kind));
        assert_identical(&doc.snapshot, &expected);
        assert_identical(&q.capture, &expected);
        assert_eq!(
            saves(&q.commands),
            vec![&LogEntry::Save {
                cause: cause_of(q.kind),
                windows: expected.windows.len() as u32,
                tabs: expected.tab_count() as u32,
            }]
        );
        last_doc = Some(doc);
    }
    let doc = last_doc.expect("one query at least");

    // The write became durable, and the closes did not touch it.
    let file = h.sim().session_file().expect("written");
    let on_disk = parse(file.as_bytes()).expect("parses");
    assert_eq!(on_disk, doc);
    assert_identical(&on_disk.snapshot, &expected);
    assert_eq!(h.engine.store().durable().doc(), Some(&doc));
    assert_eq!(h.engine.store().pending_snapshot(), Some(&doc));

    // The frozen Live_Session keeps every window the sign-out closed.
    assert!(h.engine.tracker().is_frozen());
    assert_identical(&h.engine.tracker().capture().0, &expected);
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

    #[test]
    fn session_end_query_saves_the_capture_at_the_query(s in scenario()) {
        check(&s);
    }
}

fn open(location: u16, left: i32) -> Op {
    Op::OpenWindow {
        process: ProcessChoice::Main,
        location: Pick(location),
        nav: NavOutcome::Complete,
        rect: Rect { left, top: 0, right: left + 800, bottom: 600 },
        maximized: false,
    }
}

/// Two windows: sign-out saves both while they close; restart with no window
/// writes nothing.
#[test]
fn freeze_examples() {
    let order = CloseOrder { revoke_before: 2, revoke_after: false, gap_ms: 30, rotate: Pick(0) };
    let tab = Op::OpenTab { win: Pick(0), location: Pick(3), nav: NavOutcome::Complete };
    let base = Scenario {
        config: SimConfig::default(),
        setup: vec![open(0, 0), tab, open(1, 900)],
        close_all_first: false,
        query_mid_op: None,
        kind: EndKind::SignOut,
        during: vec![Op::CloseTab { win: Pick(0), tab: Pick(0), destroy_gap_ms: 0 }],
        close_rest: Some(order),
        repeat: Some(EndKind::SignOut),
    };
    check(&base);
    // The query lands in the middle of a window close.
    check(&Scenario {
        query_mid_op: Some(Op::CloseWindow { win: Pick(1), order }),
        kind: EndKind::Shutdown,
        ..base.clone()
    });
    // No window: nothing is written, before or after a previous save.
    check(&Scenario { setup: vec![], kind: EndKind::Restart, ..base.clone() });
    check(&Scenario { close_all_first: true, kind: EndKind::Restart, ..base });
}
