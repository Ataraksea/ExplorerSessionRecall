// Feature: explorer-session-restore, Property 8: Checkpoint latency and crash freshness
//! Property 8 (task 8.5), checked end to end through the `Engine`.
//!
//! A random operation sequence (every kind the simulator supports, including
//! process kills, shell loss, and closes that end the session) is played at
//! random times: bursts of changes a few ms apart, changes inside a pending
//! checkpoint's 500 ms, and quiet gaps of seconds. Every write takes the same
//! random latency `w` ≤ 400 ms (the simulator's `write_ms`). The Session_File
//! starts absent and restoration is disabled in Settings, so checkpointing is
//! never held back by a restoration, a Trigger_Window candidate waiting for
//! its initial Location (task 11.5), or a protected snapshot. A window
//! opening after a Session_End consumes that Pending_Snapshot instead
//! (Req 4.8; an urgent, non-checkpoint write).
//!
//! While the run goes, the harness records the Live_Session after every
//! event (the tracker's capture, and whether any window is tracked), every
//! session write issued, and every write that became durable (`WriteCompleted`
//! with `Ok`: what the simulated disk holds from then on).
//!
//! The simulator runs a window close (it quiesces first) and a process kill
//! to completion in one step, so a store timer due meanwhile reaches the
//! Engine late; the harness measures the largest such delay `lateness` (0
//! without those operations). A scripted `Tick` would do the same, so time
//! passes through the gaps instead. With
//! `B = CHECKPOINT_DELAY_MS + w + lateness` (asserted ≤ 2 s, the bound of
//! Req 3.7, 3.8), and calling a Live_Session state *checkpointable* when at
//! least one window is tracked and the capture has a window (an empty
//! capture is never written, by design):
//!
//! - Every checkpoint write is non-urgent, Pending, `failed-attempts` 0,
//!   issued by a store timer, and holds (field-exact) the capture at the
//!   moment it is issued.
//! - **Latency (Req 3.7).** After any change at time `t` to a checkpointable
//!   state that stays checkpointable until `t + 500 + lateness`, a
//!   checkpoint write is issued after the change by a store timer due no
//!   later than `t + 500` ms.
//! - **Crash freshness (Req 3.7, 3.8).** For any crash time `c` such that the
//!   Live_Session was checkpointable throughout `[c − B, c]`, the durable file
//!   at `c` is a Pending checkpoint whose snapshot equals the Live_Session as
//!   it was at some time in `[c − B, c]`. Crash times are probed at every
//!   change and completion, just before and after `change + B`, and beyond
//!   the end of the run.
//!
//! The simulator's file, the store's durable view, and the last recorded
//! durable write agree at the end.
//!
//! **Validates: Requirements 3.7, 3.8**

use std::collections::BTreeMap;

use esr_core::engine::{
    Command, Engine, Environment, Event, Millis, SessionFileRead, StartupInput, TimerId, TimerOwner,
};
use esr_core::format::parse;
use esr_core::model::{SaveCause, SessionDoc, Snapshot, Status};
use esr_core::settings::Settings;
use esr_core::store::CHECKPOINT_DELAY_MS;
use esr_sim::explorer::{
    op_strategy, rect_strategy, NavOutcome, Op, OpsConfig, Pick, ProcessChoice, SimConfig, SimExplorer, UiaNaming,
};
use esr_sim::restore::{Driver, Stop, UserAction};
use proptest::prelude::*;

#[derive(Clone, Debug)]
struct Scenario {
    config: SimConfig,
    /// `(gap before the op in ms, op)`, played at increasing times.
    steps: Vec<(u64, Op)>,
}

fn sim_config(write_ms: u64) -> impl Strategy<Value = SimConfig> {
    let with = move |mut c: SimConfig| {
        c.restore.write_ms = write_ms;
        c
    };
    prop_oneof![
        Just(with(SimConfig::default())),
        Just(with(SimConfig { uia_naming: UiaNaming::FullPaths, ..SimConfig::default() })),
        Just(with(SimConfig { report_active_key: true, reuse_hwnds: false, ..SimConfig::default() })),
    ]
}

/// Gaps between operations: bursts, changes inside a pending checkpoint's
/// delay, around the delay plus the write time, and quiet periods.
fn gap() -> impl Strategy<Value = u64> {
    prop_oneof![
        3 => 0u64..=40,
        3 => 40u64..=600,
        2 => 400u64..=1_000,
        1 => 1_000u64..=3_000,
    ]
}

fn open_window() -> impl Strategy<Value = Op> {
    (any::<u16>(), rect_strategy()).prop_map(|(l, rect)| Op::OpenWindow {
        process: ProcessChoice::Main,
        location: Pick(l),
        nav: NavOutcome::Complete,
        rect,
        maximized: false,
    })
}

fn scenario() -> impl Strategy<Value = Scenario> {
    // A scripted `Tick` would jump the simulator's clock past due timers
    // (the driver fires timers only between items); the gaps let time pass.
    let ops = OpsConfig { max_tick_ms: 0, ..OpsConfig::all() };
    (0u64..=400)
        .prop_flat_map(move |write_ms| {
            (
                sim_config(write_ms),
                (gap(), open_window()),
                prop::collection::vec((gap(), op_strategy(&ops)), 0..40),
            )
        })
        .prop_map(|(config, first, mut rest)| {
            rest.insert(0, first);
            Scenario { config, steps: rest }
        })
}

/// The Live_Session after one event.
#[derive(Clone, Debug)]
struct LiveState {
    at: Millis,
    /// Position of the event in delivery order.
    k: usize,
    /// A window is tracked and the capture has a window.
    checkpointable: bool,
    snap: Snapshot,
}

/// A session write, issued or durable.
#[derive(Clone, Debug)]
struct Write {
    at: Millis,
    k: usize,
    /// For a checkpoint: when the store timer that issued it was due.
    due: Millis,
    doc: SessionDoc,
}

#[derive(Debug, Default)]
struct Record {
    /// Live_Session states in order; the first is the empty one before any event.
    live: Vec<LiveState>,
    issued: Vec<Write>,
    durable: Vec<Write>,
    /// Largest delay of a store timer behind its due time (simulator clock
    /// jumps; see the module docs).
    lateness: u64,
}

/// Field-exact snapshot equality (`LocationId`'s `==` ignores PIDL bytes and
/// display names).
fn identical(a: &Snapshot, b: &Snapshot) -> bool {
    a == b
        && a.windows.iter().zip(&b.windows).all(|(wa, wb)| {
            wa.tabs.iter().zip(&wb.tabs).all(|(ta, tb)| ta.location.identical(&tb.location))
        })
}

/// Runs the scenario through the Engine and the simulator, recording the
/// Live_Session, issued writes, and durable writes.
fn run(s: &Scenario) -> (Record, Driver, Engine) {
    let startup = StartupInput {
        session_file: SessionFileRead::Absent,
        environment: Environment::default(),
        background_logon: false,
        logon_at: None,
        shell_ready_at: Some(Millis(0)),
    };
    let settings = Settings { restoration_enabled: false, ..Settings::default() };
    let (mut engine, commands) = Engine::new(settings, startup);
    assert!(commands.is_empty(), "absent file: nothing at startup");
    let mut driver = Driver::new(SimExplorer::new(s.config.clone()));
    let mut at = Millis(0);
    for (gap, op) in &s.steps {
        at = at + *gap;
        driver.schedule(at, UserAction::Op(op.clone()));
    }
    driver.set_deadline(at + 600_000);

    let mut rec = Record {
        live: vec![LiveState { at: Millis(0), k: 0, checkpointable: false, snap: Snapshot::default() }],
        ..Record::default()
    };
    let mut texts: BTreeMap<u64, SessionDoc> = BTreeMap::new();
    let mut timers: BTreeMap<TimerId, Millis> = BTreeMap::new();
    let mut k = 0usize;
    let stop = driver.run(|now, e| {
        k += 1;
        let completed = match &e {
            Event::WriteCompleted { seq, result: Ok(()) } => Some(*seq),
            _ => None,
        };
        let fired = match &e {
            Event::Timer(id) if id.owner == TimerOwner::Store => {
                let due = timers.remove(id).expect("a store timer the engine set");
                rec.lateness = rec.lateness.max(now.since(due));
                Some(due)
            }
            _ => None,
        };
        let commands = engine.handle(now, e);
        let tracker = engine.tracker();
        let snap = tracker.capture().0;
        let checkpointable = !tracker.is_empty() && !snap.windows.is_empty();

        if let Some(seq) = completed
            && let Some(doc) = texts.get(&seq)
        {
            rec.durable.push(Write { at: now, k, due: now, doc: doc.clone() });
        }
        for c in &commands {
            if let Command::SetTimer { id, at } = c {
                timers.insert(*id, *at);
            }
            if let Command::WriteSessionFile { seq, text, urgent } = c {
                let doc = parse(text.as_bytes()).unwrap_or_else(|err| panic!("{err:?} in\n{text}"));
                // A Pending checkpoint consumed by a window opening (Req 4.8,
                // restoration disabled) keeps its cause: an urgent write too.
                if doc.cause == SaveCause::Checkpoint && doc.status == Status::Pending {
                    // A checkpoint holds the capture as it is issued, from a
                    // store timer.
                    assert!(!urgent, "checkpoints are not urgent");
                    assert_eq!(doc.failed_attempts, 0);
                    assert!(checkpointable, "checkpoint while nothing to checkpoint: {doc:?}");
                    assert!(identical(&doc.snapshot, &snap), "checkpoint {:?}\nvs capture {snap:?}", doc.snapshot);
                    let due = fired.expect("checkpoints are issued by store timers");
                    rec.issued.push(Write { at: now, k, due, doc: doc.clone() });
                } else {
                    assert!(urgent, "only Session_End writes and consumptions are not checkpoints: {doc:?}");
                }
                texts.insert(*seq, doc);
            }
        }
        let last = rec.live.last().expect("initial state");
        if last.checkpointable != checkpointable || !identical(&last.snap, &snap) {
            rec.live.push(LiveState { at: now, k, checkpointable, snap });
        }
        commands
    });
    assert_eq!(stop, Stop::Quiescent);
    (rec, driver, engine)
}

/// Live_Session states in effect at some time in `[a, c]`: the one in effect
/// when `a` begins and every later one up to `c`.
fn states_in(live: &[LiveState], a: Millis, c: Millis) -> &[LiveState] {
    let start = live.iter().rposition(|s| s.at < a).unwrap_or(0);
    let end = live.iter().rposition(|s| s.at <= c).map_or(start + 1, |i| i + 1).max(start + 1);
    &live[start..end]
}

/// What the disk holds at `c` (after every completion at or before `c`).
fn durable_at(durable: &[Write], c: Millis) -> Option<&Write> {
    durable.iter().rev().find(|w| w.at <= c)
}

fn check(s: &Scenario) {
    let write_ms = s.config.restore.write_ms;
    let (rec, driver, engine) = run(s);
    let end = driver.sim().now();
    // The simulator's window closes and process kills run to completion in
    // one step, moving its clock past timers due meanwhile; those timers are
    // delivered late by at most `lateness`. The store's own bound is
    // `CHECKPOINT_DELAY_MS + write_ms`; the whole must stay within the 2 s of
    // Req 3.7, 3.8.
    let bound = CHECKPOINT_DELAY_MS + write_ms + rec.lateness;
    assert!(bound <= 2_000, "freshness bound {bound} ms (lateness {} ms)", rec.lateness);

    // Latency (Req 3.7): a change to a checkpointable state that stays
    // checkpointable for 500 ms is followed by a checkpoint from a timer due
    // within 500 ms (issued then, or as soon as a late timer is delivered,
    // so the state has to stay checkpointable until then).
    for (i, change) in rec.live.iter().enumerate().skip(1) {
        let t = change.at;
        let due = t + CHECKPOINT_DELAY_MS;
        let stays = rec.live[i..].iter().take_while(|l| l.at <= due + rec.lateness).all(|l| l.checkpointable);
        if !stays {
            continue;
        }
        assert!(
            rec.issued.iter().any(|w| w.k > change.k && w.due <= due && w.at <= due + rec.lateness),
            "no checkpoint by {due:?} for the change at {t:?}: {:?}\nissued (at, due): {:?}",
            change.snap,
            rec.issued.iter().map(|w| (w.at, w.due)).collect::<Vec<_>>()
        );
    }

    // Crash freshness (Req 3.7, 3.8).
    let mut crashes: Vec<Millis> = Vec::new();
    for l in &rec.live {
        crashes.extend([l.at, l.at + bound.saturating_sub(1), l.at + bound, l.at + bound + 1]);
    }
    for w in &rec.durable {
        crashes.extend([Millis(w.at.0.saturating_sub(1)), w.at]);
    }
    crashes.extend([end, end + bound, end + bound + 1, end + 2_000]);
    crashes.sort();
    crashes.dedup();
    for c in crashes {
        let a = Millis(c.0.saturating_sub(bound));
        let states = states_in(&rec.live, a, c);
        if !states.iter().all(|l| l.checkpointable) {
            continue;
        }
        let w = durable_at(&rec.durable, c)
            .unwrap_or_else(|| panic!("crash at {c:?}: nothing durable; Live_Session since {a:?}: {states:?}"));
        assert_eq!(w.doc.status, Status::Pending, "crash at {c:?}");
        assert_eq!(w.doc.failed_attempts, 0, "crash at {c:?}");
        assert_eq!(w.doc.cause, SaveCause::Checkpoint, "crash at {c:?}");
        assert!(
            states.iter().any(|l| identical(&l.snap, &w.doc.snapshot)),
            "crash at {c:?}: the file (written at {:?}) holds {:?}\nbut the Live_Session in [{a:?}, {c:?}] was {states:#?}",
            w.at,
            w.doc.snapshot
        );
    }

    // The simulated disk, the store's view, and the record agree.
    match rec.durable.last() {
        Some(w) => {
            let file = driver.sim().session_file().expect("written");
            assert_eq!(parse(file.as_bytes()).expect("parses"), w.doc);
            assert_eq!(engine.store().durable().doc(), Some(&w.doc));
        }
        None => assert_eq!(driver.sim().session_file(), None),
    }
    assert_eq!(engine.store().writes_in_flight(), 0);
    assert_eq!(engine.store().checkpoint_due(), None);
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

    #[test]
    fn checkpoints_follow_changes_and_survive_crashes(s in scenario()) {
        check(&s);
    }
}

fn open(location: u16, left: i32) -> Op {
    Op::OpenWindow {
        process: ProcessChoice::Main,
        location: Pick(location),
        nav: NavOutcome::Complete,
        rect: esr_core::model::Rect { left, top: 0, right: left + 800, bottom: 600 },
        maximized: false,
    }
}

/// Changes 100 ms apart coalesce into one checkpoint 500 ms after the first;
/// a change after a quiet period gets its own; the file then holds the
/// latest Live_Session.
#[test]
fn coalesced_checkpoints_reach_the_disk() {
    let nav = |l| Op::Navigate { win: Pick(0), location: Pick(l), nav: NavOutcome::Complete };
    let mut config = SimConfig::default();
    config.restore.write_ms = 400;
    let s = Scenario {
        config,
        steps: vec![(0, open(0, 0)), (100, nav(1)), (100, nav(2)), (100, open(3, 900)), (3_000, nav(4))],
    };
    check(&s);
    let (rec, driver, engine) = run(&s);
    // One checkpoint for the burst, one for the late navigation.
    assert_eq!(rec.issued.len(), 2, "{:?}", rec.issued.iter().map(|w| w.at).collect::<Vec<_>>());
    let first_change = rec.live[1].at;
    assert!(rec.issued[0].at <= first_change + CHECKPOINT_DELAY_MS);
    let file = parse(driver.sim().session_file().expect("written").as_bytes()).expect("parses");
    assert!(identical(&file.snapshot, &engine.tracker().capture().0));
    assert_eq!(file.snapshot.windows.len(), 2);
}

/// Closing the last window right after a change: the simulator runs the
/// close to completion in one step, so the pending checkpoint timer is
/// delivered late, after the Session_End, which cancelled it; the file
/// holds the Session_End write.
#[test]
fn late_timer_after_a_close_that_ends_the_session() {
    let mut config = SimConfig::default();
    config.restore.write_ms = 267;
    let nav = |l| Op::Navigate { win: Pick(0), location: Pick(l), nav: NavOutcome::Complete };
    let s = Scenario {
        config,
        steps: vec![
            (0, open(0, 0)),
            (263, nav(0)),
            (337, nav(1)),
            (0, Op::CloseTab { win: Pick(0), tab: Pick(0), destroy_gap_ms: 244 }),
        ],
    };
    check(&s);
    let (rec, driver, _) = run(&s);
    assert!(rec.lateness > 0, "the close moved the clock past the timer");
    let file = parse(driver.sim().session_file().expect("written").as_bytes()).expect("parses");
    assert_eq!(file.cause, SaveCause::LastTabClosed);
}

/// Shrunk counterexample: a tab is dragged out to a new window, and the
/// source's last tab is dragged into it before the new window's first scan.
/// The new window appeared while the source was open, so it is no
/// Trigger_Window: the pending checkpoint is not consumed (an urgent
/// `Consumed` write of a checkpoint), and checkpointing goes on.
#[test]
fn drag_out_window_outliving_its_source_is_no_trigger() {
    let mut config = SimConfig::default();
    config.restore.write_ms = 186;
    let s = Scenario {
        config,
        steps: vec![
            (0, open(0, 0)),
            (0, Op::OpenTab { win: Pick(0), location: Pick(0), nav: NavOutcome::Complete }),
            (537, Op::DragOut { win: Pick(0), tab: Pick(0), dx: 0, dy: 0 }),
            (34, Op::DragIn { from: Pick(0), tab: Pick(0), to: Pick(0), position: Pick(0) }),
        ],
    };
    check(&s);
    let (_, driver, engine) = run(&s);
    let file = parse(driver.sim().session_file().expect("written").as_bytes()).expect("parses");
    assert_eq!((file.status, file.cause), (Status::Pending, SaveCause::Checkpoint));
    assert!(identical(&file.snapshot, &engine.tracker().capture().0));
    assert_eq!(file.snapshot.tab_count(), 2);
}
