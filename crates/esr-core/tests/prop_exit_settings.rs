// Feature: explorer-session-restore, Property 27: Exit through Settings
//! Property 27 (task 11.13), checked end to end through the `Engine` driven
//! by the simulated Explorer.
//!
//! A Live_Session is built in one of two ways:
//!
//! - **Tracking:** a random operation sequence (Property 1's mix plus process
//!   kills, shell loss), optionally ending with every window closed, from an
//!   absent, Consumed, or Pending Session_File. The request lands after
//!   quiescence or right after the events of one more operation (in the
//!   middle of a close, before its tombstones expire).
//! - **Mid-restoration:** a Pending_Snapshot restored by a user launch
//!   (`SimConfig::for_restoration`); the request lands a random delay after
//!   the launch, before, during, or after the restoration.
//!
//! The simulated file store has a random write latency (up to beyond the
//! exit deadline) and fails a random number of the session writes issued
//! from the request on. The user keeps working after the request. (Window
//! closes and process kills in the simulator first run everything scheduled,
//! which makes the exit timing unfaithful; the timing checks are skipped
//! when the user does one of those after the request.)
//!
//! - Exactly one `Exit` is emitted, within 5 s of the request, and nothing
//!   after it.
//! - From the request on, only `WriteSessionFile`, `AppendLog`, `SetTimer`,
//!   and `Exit` are emitted: no window-affecting command (Explorer windows
//!   stay open and, when nothing else touches them, unchanged).
//! - With a window tracked (open, with a Location), the request emits one
//!   urgent write of a Pending document with `failed-attempts` 0, cause
//!   `ExitThroughSettings`, and (field-exact) the capture at the request,
//!   plus its `SAVE` entry; every later write repeats that content (no
//!   checkpoint replaces it). If that write (or a retry) completed before
//!   `Exit`, the store holds it durably at `Exit` and no `exit` ERROR is
//!   logged; otherwise exactly one `exit` ERROR precedes `Exit`. Whenever
//!   the write and its retries fit in the 4 s exit deadline, it completes
//!   before `Exit`.
//! - With no window open, `Exit` is the only command of the request,
//!   nothing is written from it on, and the Session_File keeps what it held.
//!
//! **Validates: Requirements 12.5, 12.8, 12.9**

use std::collections::BTreeSet;

use esr_core::engine::{
    Command, Engine, Environment, Event, Millis, SessionFileRead, StartupInput, EXIT_WRITE_TIMEOUT_MS, OP_EXIT,
};
use esr_core::format::{parse, serialize, FORMAT_VERSION};
use esr_core::logfmt::LogEntry;
use esr_core::model::{
    DisplayRef, LocationId, Placement, Rect, SaveCause, SessionDoc, ShowState, Snapshot, Status, TabRec, Timestamp,
    WindowId, WindowRec,
};
use esr_core::settings::Settings;
use esr_core::store::{FileView, RETRY_INTERVAL_MS};
use esr_sim::explorer::{
    default_displays, default_locations, op_strategy, ops_strategy, CloseOrder, NavOutcome, Op, OpsConfig, Pick,
    ProcessChoice, SimConfig, SimExplorer, SimWindow, UiaNaming,
};
use esr_sim::restore::{home_location, CommandKind, Driver, FailWhen, FailureRule, Launch, Stop, UserAction};
use proptest::prelude::*;
use proptest::sample::select;

/// Req 12.5: the Session_Monitor stops within 5 s of the exit request.
const EXIT_LIMIT_MS: u64 = 5_000;
/// Locations of saved tabs: always available, never search results.
const POOL: [usize; 6] = [2, 3, 4, 9, 10, 11];
/// Targets of a Targeted_Launch (`POOL` minus Home).
const TARGETS: [usize; 5] = [2, 3, 4, 9, 11];
/// Far enough never to stop a run.
const FAR: Millis = Millis(u64::MAX / 2);

#[derive(Clone, Debug)]
// One scenario value per test case; boxing the large variant buys nothing.
#[allow(clippy::large_enum_variant)]
enum Mode {
    Tracking {
        config: SimConfig,
        setup: Vec<Op>,
        /// Close every window before the request (the empty-session case).
        close_all_first: bool,
        /// Deliver the request right after this operation's events.
        mid_op: Option<Op>,
    },
    /// The user launches File Explorer (`target: None` = Default_Launch on
    /// Home) with a Pending_Snapshot; the request lands `delay` ms later.
    Restoring { target: Option<usize>, delay: u64 },
}

#[derive(Clone, Debug)]
struct Scenario {
    mode: Mode,
    /// The Session_File at startup (always Pending when restoring).
    initial: Option<SessionDoc>,
    /// What the user does after the request (150 ms apart).
    during: Vec<Op>,
    write_ms: u64,
    /// Session writes failed from the request on.
    failures: u32,
}

fn loc(i: usize) -> LocationId {
    default_locations()[i].clone()
}

fn rect(left: i32, top: i32) -> Rect {
    Rect { left, top, right: left + 800, bottom: top + 600 }
}

fn doc(status: Status, failed_attempts: u8, windows: &[Vec<usize>]) -> SessionDoc {
    let windows: Vec<WindowRec> = windows
        .iter()
        .enumerate()
        .map(|(i, tabs)| {
            let left = 100 + 2000 * (i as i32 % 2);
            let d = &default_displays()[usize::from(left >= 1920)];
            WindowRec {
                id: WindowId(i as u32 + 1),
                tabs: tabs.iter().map(|&l| TabRec { location: loc(l) }).collect(),
                active: 0,
                placement: Placement {
                    state: ShowState::Normal,
                    restored: rect(left, 40 + 30 * i as i32),
                    display: DisplayRef { device: d.device.clone(), monitor: d.monitor },
                },
            }
        })
        .collect();
    let activation = windows.iter().map(|w| w.id).collect();
    SessionDoc {
        version: FORMAT_VERSION,
        status,
        failed_attempts,
        saved_at: Timestamp { year: 2025, month: 3, day: 4, hour: 5, minute: 6, second: 7, millis: 8, offset_minutes: 0 },
        cause: SaveCause::LastWindowClosed,
        snapshot: Snapshot { windows, activation },
    }
}

fn windows_strategy() -> impl Strategy<Value = Vec<Vec<usize>>> {
    prop::collection::vec(prop::collection::vec(select(POOL.to_vec()), 1..=3), 1..=3)
}

fn sim_config() -> impl Strategy<Value = SimConfig> {
    prop_oneof![
        Just(SimConfig::default()),
        Just(SimConfig { uia_naming: UiaNaming::FullPaths, ..SimConfig::default() }),
        Just(SimConfig { report_active_key: true, reuse_hwnds: false, ..SimConfig::default() }),
    ]
}

/// Write latencies from instant to beyond the exit deadline.
fn write_ms() -> impl Strategy<Value = u64> {
    prop_oneof![Just(1u64), Just(10), Just(300), Just(900), Just(1_500), Just(3_000), Just(4_500)]
}

fn failures() -> impl Strategy<Value = u32> {
    prop_oneof![4 => Just(0u32), 2 => Just(1), 1 => Just(2), 1 => Just(3)]
}

/// An operation after the request. `Op::Tick` is left out (it adds nothing
/// the script's spacing does not).
fn during_op(ops: &OpsConfig) -> BoxedStrategy<Op> {
    op_strategy(ops).prop_filter("no Tick", |op| !matches!(op, Op::Tick { .. })).boxed()
}

/// `SimExplorer::apply` carries out window closes and process kills only
/// after running everything scheduled in the simulator (`quiesce`), in-flight
/// writes and restoration steps included. The driver then delivers what that
/// caused (e.g. the exit write's completion) before an Engine timer that is
/// due earlier (the exit deadline, a write retry), so with such an operation
/// after the request the timing of the exit is not faithful and only the
/// order-independent checks apply.
fn jumps_the_clock(op: &Op) -> bool {
    matches!(op, Op::CloseWindow { .. } | Op::CloseTab { .. } | Op::KillProcess { .. } | Op::Tick { .. })
}

/// What the user does after the request: either operations that never
/// advance the simulator's clock (the exit timing stays faithful), or any.
fn during(ops: &OpsConfig) -> BoxedStrategy<Vec<Op>> {
    let instant = during_op(ops).prop_filter("instant", |op| !jumps_the_clock(op));
    prop_oneof![
        prop::collection::vec(instant, 0..8),
        prop::collection::vec(during_op(ops), 0..6),
    ]
    .boxed()
}

fn scenario() -> impl Strategy<Value = Scenario> {
    let ops = OpsConfig::all();
    let initial = prop_oneof![
        Just(None),
        windows_strategy().prop_map(|w| Some(doc(Status::Consumed, 0, &w))),
        (0u8..=2, windows_strategy()).prop_map(|(f, w)| Some(doc(Status::Pending, f, &w))),
    ];
    let tracking = (
        sim_config(),
        ops_strategy(&ops, 0..30),
        prop::bool::weighted(0.15),
        prop::option::weighted(0.3, op_strategy(&ops)),
        initial,
    )
        .prop_map(|(config, setup, close_all_first, mid_op, initial)| {
            (Mode::Tracking { config, setup, close_all_first, mid_op }, initial)
        });
    let target = prop_oneof![Just(None), select(TARGETS.to_vec()).prop_map(Some)];
    let restoring = (target, 0u64..2_500, 0u8..=1, windows_strategy()).prop_map(|(target, delay, f, w)| {
        (Mode::Restoring { target, delay }, Some(doc(Status::Pending, f, &w)))
    });
    (
        prop_oneof![3 => tracking, 2 => restoring],
        during(&ops),
        write_ms(),
        failures(),
    )
        .prop_map(|((mode, initial), during, write_ms, failures)| Scenario { mode, initial, during, write_ms, failures })
}

/// The `Exit` command as the Engine emitted it.
#[derive(Debug)]
struct ExitSeen {
    at: Millis,
    /// A write of the exit content completed successfully before it.
    landed: bool,
    /// What the store holds durably at that moment.
    durable: FileView,
}

/// What happened from the request on.
#[derive(Debug, Default)]
struct Observed {
    request_at: Option<Millis>,
    open_at_request: usize,
    capture: Option<Snapshot>,
    durable_at_request: Option<FileView>,
    restoring_at_request: bool,
    /// Commands of the request itself.
    request_commands: Vec<Command>,
    /// Every command from the request on (the request's included).
    after: Vec<(Millis, Command)>,
    /// `seq`s of the session writes from the request on.
    exit_seqs: BTreeSet<u64>,
    landed: bool,
    exits: Vec<ExitSeen>,
}

/// The Engine, driven by the simulated Explorer.
struct Harness {
    engine: Engine,
    driver: Driver,
}

impl Harness {
    fn new(config: SimConfig, file: Option<&SessionDoc>) -> Self {
        let startup = StartupInput {
            session_file: file.map_or(SessionFileRead::Absent, |d| SessionFileRead::Content(serialize(d).into_bytes())),
            environment: Environment { displays: default_displays(), ..Environment::default() },
            background_logon: false,
            logon_at: None,
            shell_ready_at: Some(Millis(0)),
        };
        let (engine, commands) = Engine::new(Settings::default(), startup);
        let mut driver = Driver::new(SimExplorer::new(config));
        driver.submit(commands);
        let mut h = Harness { engine, driver };
        assert_eq!(h.run(FAR), Stop::Quiescent);
        h
    }

    fn sim(&self) -> &SimExplorer {
        self.driver.sim()
    }

    fn run(&mut self, deadline: Millis) -> Stop {
        self.driver.set_deadline(deadline);
        let engine = &mut self.engine;
        self.driver.run(|now, e| engine.handle(now, e))
    }

    fn play(&mut self, op: &Op) {
        let events = self.driver.sim_mut().apply(op);
        self.driver.deliver(events);
        assert_eq!(self.run(FAR), Stop::Quiescent, "after {op:?}");
    }

    /// Delivers `ExitRequested` (right after `mid_op`'s events, if any),
    /// schedules `during` after it, and runs until quiescence.
    fn request_exit(&mut self, mid_op: Option<&Op>, during: &[Op]) -> Observed {
        if let Some(op) = mid_op {
            let events = self.driver.sim_mut().apply(op);
            self.driver.deliver(events);
        }
        self.driver.deliver_now(Event::ExitRequested);
        let base = self.sim().now();
        for (i, op) in during.iter().enumerate() {
            self.driver.schedule(base + 150 * (i as u64 + 1), UserAction::Op(op.clone()));
        }
        self.driver.set_deadline(FAR);
        let engine = &mut self.engine;
        let mut obs = Observed::default();
        let stop = self.driver.run(|now, e| {
            let is_request = matches!(e, Event::ExitRequested);
            if is_request {
                assert!(obs.request_at.is_none(), "one request");
                obs.request_at = Some(now);
                obs.open_at_request = engine.tracker().open_window_count();
                obs.capture = Some(engine.tracker().capture().0);
                obs.durable_at_request = Some(engine.store().durable().clone());
                obs.restoring_at_request = engine.store().is_restoring();
            }
            if let Event::WriteCompleted { seq, result: Ok(()) } = &e
                && obs.exit_seqs.contains(seq)
            {
                obs.landed = true;
            }
            let commands = engine.handle(now, e);
            if obs.request_at.is_some() {
                for c in &commands {
                    if let Command::WriteSessionFile { seq, .. } = c {
                        obs.exit_seqs.insert(*seq);
                    }
                    if *c == Command::Exit {
                        obs.exits.push(ExitSeen { at: now, landed: obs.landed, durable: engine.store().durable().clone() });
                    }
                    obs.after.push((now, c.clone()));
                }
                if is_request {
                    obs.request_commands = commands.clone();
                }
            }
            commands
        });
        assert_eq!(stop, Stop::Quiescent);
        obs
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

fn writes(commands: &[(Millis, Command)]) -> Vec<(&String, bool)> {
    commands
        .iter()
        .filter_map(|(_, c)| match c {
            Command::WriteSessionFile { text, urgent, .. } => Some((text, *urgent)),
            _ => None,
        })
        .collect()
}

fn exit_errors(commands: &[(Millis, Command)]) -> usize {
    commands
        .iter()
        .filter(|(_, c)| matches!(c, Command::AppendLog(LogEntry::Error { operation, .. }) if operation == OP_EXIT))
        .count()
}

/// Builds the Live_Session and requests the exit; returns the harness, the
/// observation, and the simulator's windows just before the request (when
/// nothing else touched them after it: no mid-operation request, no
/// restoration in flight, no user operation).
fn run_scenario(s: &Scenario) -> (Harness, Observed, Option<Vec<SimWindow>>) {
    let (mut h, mid_op) = match &s.mode {
        Mode::Tracking { config, setup, close_all_first, mid_op } => {
            let mut config = config.clone();
            config.restore.write_ms = s.write_ms;
            let mut h = Harness::new(config, s.initial.as_ref());
            for op in setup {
                h.play(op);
            }
            if *close_all_first {
                // The shell is needed to see the closes.
                h.play(&Op::RestoreShell);
                let order = CloseOrder { revoke_before: 1, revoke_after: true, gap_ms: 10, rotate: Pick(0) };
                while !h.sim().windows().is_empty() {
                    h.play(&Op::CloseWindow { win: Pick(0), order });
                }
                assert!(h.engine.tracker().is_empty(), "every window closed");
            }
            (h, mid_op.as_ref())
        }
        Mode::Restoring { target, delay } => {
            let mut config = SimConfig::for_restoration();
            config.restore.write_ms = s.write_ms;
            config.restore.claim_user_windows = false;
            let mut h = Harness::new(config, s.initial.as_ref());
            let at = h.sim().now() + 1_000;
            let launch = Launch {
                location: target.map_or_else(home_location, loc),
                selection: vec![],
                rect: rect(300, 300),
                maximized: false,
            };
            h.driver.schedule(at, UserAction::Launch(launch));
            let stop = h.run(at + *delay);
            assert_ne!(stop, Stop::EventLimit);
            (h, None)
        }
    };
    let untouched = mid_op.is_none() && s.during.is_empty() && matches!(s.mode, Mode::Tracking { .. });
    let before = untouched.then(|| h.sim().windows().to_vec());
    if s.failures > 0 {
        h.driver.sim_mut().fail(FailureRule::times(FailWhen::Kind(CommandKind::WriteSessionFile), s.failures));
    }
    let obs = h.request_exit(mid_op, &s.during);
    (h, obs, before)
}

fn check(s: &Scenario) -> Observed {
    let (h, obs, before) = run_scenario(s);
    let request_at = obs.request_at.expect("the request was delivered");

    // Exactly one Exit, within 5 s, and nothing after it.
    assert_eq!(obs.exits.len(), 1, "{:#?}", obs.after);
    assert_eq!(h.sim().commands_of(CommandKind::Exit).len(), 1);
    let exit = &obs.exits[0];
    let faithful = !s.during.iter().any(jumps_the_clock);
    if faithful {
        assert!(exit.at.since(request_at) <= EXIT_LIMIT_MS, "Exit {} ms after the request", exit.at.since(request_at));
    }
    assert_eq!(obs.after.last().map(|(_, c)| c), Some(&Command::Exit), "nothing after Exit: {:#?}", obs.after);
    assert!(h.engine.is_exiting() && h.engine.has_exited());

    // No window-affecting command from the request on (Req 12.5).
    for (at, c) in &obs.after {
        assert!(
            matches!(
                c,
                Command::WriteSessionFile { .. } | Command::AppendLog(_) | Command::SetTimer { .. } | Command::Exit
            ),
            "{c:?} at {at:?} after the exit request"
        );
    }
    // Nothing else touched the windows: they are open and unchanged.
    if let Some(before) = before {
        assert_eq!(h.sim().windows(), &before[..]);
    }

    let capture = obs.capture.clone().expect("captured");
    let tracked = obs.open_at_request > 0 && !capture.windows.is_empty();
    if !tracked {
        // Req 12.9: Exit at once, nothing written, the Session_File unchanged.
        assert_eq!(obs.request_commands, vec![Command::Exit]);
        assert_eq!(exit.at, request_at);
        assert_eq!(writes(&obs.after), vec![]);
        assert_eq!(Some(h.engine.store().durable()), obs.durable_at_request.as_ref());
        // Every write in the simulator's log was issued before the request,
        // so the file holds the last one that succeeded (writes complete in
        // order), whatever the exit did.
        let last_ok = h.sim().command_log().iter().rev().find_map(|r| match &r.command {
            Command::WriteSessionFile { text, .. } if !r.failed => Some(text.as_str()),
            _ => None,
        });
        assert_eq!(h.sim().session_file(), last_ok);
        return obs;
    }

    // Req 12.8: the capture at the request, urgently, as the Pending_Snapshot.
    let request_writes: Vec<(&String, bool)> = obs
        .request_commands
        .iter()
        .filter_map(|c| match c {
            Command::WriteSessionFile { text, urgent, .. } => Some((text, *urgent)),
            _ => None,
        })
        .collect();
    assert_eq!(request_writes.len(), 1, "{:#?}", obs.request_commands);
    let (text, urgent) = request_writes[0];
    assert!(urgent, "the exit write is urgent");
    let doc = parse(text.as_bytes()).unwrap_or_else(|e| panic!("{e:?} in\n{text}"));
    assert_eq!(doc.status, Status::Pending);
    assert_eq!(doc.failed_attempts, 0);
    assert_eq!(doc.cause, SaveCause::ExitThroughSettings);
    assert_identical(&doc.snapshot, &capture);
    let saves: Vec<&LogEntry> = obs
        .request_commands
        .iter()
        .filter_map(|c| match c {
            Command::AppendLog(e @ LogEntry::Save { .. }) => Some(e),
            _ => None,
        })
        .collect();
    assert_eq!(
        saves,
        vec![&LogEntry::Save {
            cause: SaveCause::ExitThroughSettings,
            windows: capture.windows.len() as u32,
            tabs: capture.tab_count() as u32,
        }]
    );
    assert!(!obs.request_commands.contains(&Command::Exit), "Exit waits for the write");
    // Retries repeat the exit content; no checkpoint replaces it.
    for (t, _) in writes(&obs.after) {
        assert_eq!(t, text, "a write after the exit request is not the exit capture");
    }

    if exit.landed {
        // Durable before Exit; no exit ERROR.
        assert_eq!(exit_errors(&obs.after), 0, "{:#?}", obs.after);
        assert_eq!(exit.durable.doc(), Some(&doc));
        let file = h.sim().session_file().expect("written");
        assert_eq!(parse(file.as_bytes()).expect("parses"), doc);
    } else {
        // Not durable in time: one ERROR, then Exit.
        assert_eq!(exit_errors(&obs.after), 1, "{:#?}", obs.after);
        assert_ne!(exit.durable.doc(), Some(&doc));
    }

    // Liveness: when the write and its retries fit in the exit deadline, it
    // lands before Exit; a first attempt slower than the deadline cannot.
    let f = u64::from(s.failures);
    let worst = (f + 1) * s.write_ms + f * RETRY_INTERVAL_MS;
    if faithful && worst < EXIT_WRITE_TIMEOUT_MS {
        assert!(exit.landed, "the write fits in {EXIT_WRITE_TIMEOUT_MS} ms (worst case {worst} ms)");
    }
    if faithful && s.write_ms > EXIT_WRITE_TIMEOUT_MS {
        assert!(!exit.landed);
        assert_eq!(exit.at.since(request_at), EXIT_WRITE_TIMEOUT_MS);
    }
    obs
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 160, ..ProptestConfig::default() })]

    #[test]
    fn exit_through_settings_saves_the_capture_and_touches_no_window(s in scenario()) {
        check(&s);
    }
}

fn open(location: u16, left: i32) -> Op {
    Op::OpenWindow {
        process: ProcessChoice::Main,
        location: Pick(location),
        nav: NavOutcome::Complete,
        rect: rect(left, 0),
        maximized: false,
    }
}

fn tracking(setup: Vec<Op>, close_all_first: bool) -> Mode {
    Mode::Tracking { config: SimConfig::default(), setup, close_all_first, mid_op: None }
}

/// No window: Exit at once, nothing written, before or after a save.
#[test]
fn exit_with_no_window_writes_nothing() {
    let pending = doc(Status::Pending, 1, &[vec![2, 3]]);
    for (mode, initial) in [
        (tracking(vec![], false), None),
        (tracking(vec![], false), Some(pending.clone())),
        (tracking(vec![open(2, 0), open(3, 900)], true), Some(pending)),
    ] {
        let obs = check(&Scenario { mode, initial, during: vec![], write_ms: 10, failures: 0 });
        assert_eq!(obs.request_commands, vec![Command::Exit]);
    }
}

/// Two windows: the exit waits for the write; failures are retried; a write
/// that never lands ends in an ERROR at the deadline.
#[test]
fn exit_waits_for_the_write_or_the_deadline() {
    let tab = Op::OpenTab { win: Pick(0), location: Pick(4), nav: NavOutcome::Complete };
    let base = Scenario {
        mode: tracking(vec![open(2, 0), tab, open(3, 900)], false),
        initial: None,
        during: vec![],
        write_ms: 300,
        failures: 0,
    };
    let obs = check(&base);
    let request_at = obs.request_at.unwrap();
    assert_eq!(obs.exits[0].at, request_at + 300);
    // One failure: the retry 2 s later lands.
    let obs = check(&Scenario { failures: 1, ..base.clone() });
    assert!(obs.exits[0].landed);
    assert_eq!(obs.exits[0].at, obs.request_at.unwrap() + 300 + RETRY_INTERVAL_MS + 300);
    // Every write fails: ERROR and Exit at the deadline.
    let obs = check(&Scenario { failures: 100, ..base.clone() });
    assert!(!obs.exits[0].landed);
    assert_eq!(obs.exits[0].at, obs.request_at.unwrap() + EXIT_WRITE_TIMEOUT_MS);
    // The user closes every window meanwhile: still the capture at the request.
    let order = CloseOrder { revoke_before: 1, revoke_after: true, gap_ms: 10, rotate: Pick(0) };
    let close = Op::CloseWindow { win: Pick(0), order };
    check(&Scenario { during: vec![close.clone(), close], write_ms: 1_500, ..base });
}

/// The request lands while a restoration is opening windows.
#[test]
fn exit_mid_restoration_stops_restoring() {
    let initial = Some(doc(Status::Pending, 0, &[vec![2, 3], vec![4, 9, 11], vec![10]]));
    let obs = check(&Scenario {
        mode: Mode::Restoring { target: None, delay: 200 },
        initial,
        during: vec![],
        write_ms: 10,
        failures: 0,
    });
    assert!(obs.restoring_at_request, "the request landed mid-restoration");
    assert!(obs.open_at_request > 0);
}
