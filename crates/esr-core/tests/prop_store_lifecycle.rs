// Feature: explorer-session-restore, Property 10: Store lifecycle matches the protection model
//! Property 10 (task 8.6): a model-based state-machine test of `StoreLogic`
//! against an abstract model of design.md "Failed-restoration protection".
//!
//! - **Model** ([`ReferenceStateMachine`]): what the Session_File durably
//!   holds and what it will hold once the newest write lands (status,
//!   `failed_attempts`, cause, snapshot identity), the state of the newest
//!   write (landed, in flight, retry pending, retries exhausted), the
//!   Live_Session (no window, windows with an empty capture, or a snapshot
//!   identity), whether a restoration runs and an attempt is counted, and
//!   whether a checkpoint is scheduled. A Pending snapshot with
//!   `failed_attempts ≥ 1` is protected while no restoration runs.
//! - **System under test** ([`StateMachineTest`]): `StoreLogic` with a
//!   simulated disk. A write lands on the disk when it completes `Ok` and is
//!   newer than what the disk holds (the file store reports an older queued
//!   write as `Ok` without writing it). Writes still in flight are lost when
//!   the process ends.
//! - **Transitions**: Live_Session changes; time passing (checkpoint and
//!   retry timers); write completion (in issue order or reversed) and write
//!   failure; Session_Ends of every user cause and of File Explorer
//!   termination; a Trigger_Window (`begin_restoration`,
//!   `abandon_if_exhausted`, `begin_attempt`, or a trigger that does not
//!   restore); the restoration opening windows; restoration completion and
//!   in-process failure; a process end and restart, which re-runs `startup`
//!   from the bytes on the simulated disk.
//!
//! After every transition the store's durable view, its desired view, and
//! the disk match the model exactly, as do protection, the restoration and
//! attempt flags, the scheduled checkpoint, and the retry state. The `SAVE`
//! causes and the number of `ABANDONED` entries of the step match the model
//! (`ABANDONED` exactly once per exhausted snapshot; again only after a
//! process end before its Consumed write landed, since the file then still
//! holds the exhausted Pending snapshot). Every checkpoint write is issued
//! while the snapshot is neither protected nor being restored. An attempt
//! write holds the unchanged Pending snapshot with the count increased by
//! one, is never issued for a count of 3, and the restoration opens windows
//! only once that write is on the disk (increment-before-attempt).
//!
//! **Validates: Requirements 3.10, 4.6, 4.7, 4.10, 4.11**

use std::collections::{BTreeMap, BTreeSet};

use esr_core::engine::{Command, Event, IoErrorKind, Millis, SessionFileRead, TimerId, TimerOwner};
use esr_core::format::{parse, serialize, FORMAT_VERSION};
use esr_core::logfmt::LogEntry;
use esr_core::model::{
    DisplayRef, LocationId, LocationKind, Placement, Rect, SaveCause, SessionDoc, ShowState, Snapshot, Status,
    TabRec, Volume, WindowId, WindowRec, MAX_FAILED_ATTEMPTS,
};
use esr_core::store::{FileView, LiveView, StoreLogic, MAX_RETRIES, UNKNOWN_TIME};
use proptest::prelude::*;
use proptest::sample::select;
use proptest::test_runner::Config;
use proptest_state_machine::{prop_state_machine, ReferenceStateMachine, StateMachineTest};

// ---------------------------------------------------------------------------
// Snapshots and the Live_Session
// ---------------------------------------------------------------------------

/// The snapshot with identity `id`: one window with 1–3 tabs.
fn snapshot(id: u32) -> Snapshot {
    let r = Rect { left: 0, top: 0, right: 800, bottom: 600 };
    let tabs = 1 + (id % 3) as usize;
    Snapshot {
        windows: vec![WindowRec {
            id: WindowId(0),
            tabs: (0..tabs)
                .map(|k| TabRec {
                    location: LocationId {
                        case_sensitive: false,
                        kind: LocationKind::FileSystem(Volume::Fixed),
                        parsing_name: format!(r"C:\s{id}\t{k}").as_str().into(),
                        display_name: format!("t{k}").as_str().into(),
                        pidl: vec![1, 2, 3],
                        search: None,
                    },
                })
                .collect(),
            active: 0,
            placement: Placement {
                state: ShowState::Normal,
                restored: r,
                display: DisplayRef { device: r"\\.\DISPLAY1".into(), monitor: r },
            },
        }],
        activation: vec![WindowId(0)],
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Live {
    /// No Explorer window is open.
    Closed,
    /// Windows are open but no tab has a Location yet (empty capture).
    Empty,
    /// Windows are open; the capture is `snapshot(id)`.
    Snap(u32),
}

impl LiveView for Live {
    fn has_windows(&self) -> bool {
        *self != Live::Closed
    }
    fn capture_snapshot(&self) -> Snapshot {
        match *self {
            Live::Snap(id) => snapshot(id),
            _ => Snapshot::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

/// The Session_File content, abstractly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MDoc {
    status: Status,
    failed_attempts: u8,
    cause: SaveCause,
    snap: u32,
}

/// The concrete document for `m` (no wall clock: `saved-at` is unknown).
fn to_doc(m: &MDoc) -> SessionDoc {
    SessionDoc {
        version: FORMAT_VERSION,
        status: m.status,
        failed_attempts: m.failed_attempts,
        saved_at: UNKNOWN_TIME,
        cause: m.cause,
        snapshot: snapshot(m.snap),
    }
}

fn to_view(m: &Option<MDoc>) -> FileView {
    m.as_ref().map_or(FileView::Absent, |d| FileView::Doc(to_doc(d)))
}

/// The newest issued session write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WriteState {
    /// It landed (or nothing was written since startup).
    Done,
    /// In flight; `retries` of the original write it is.
    InFlight(u8),
    /// It failed; retry number `n` is scheduled.
    RetryPending(u8),
    /// Every retry failed; written again at the next Live_Session change.
    Exhausted,
}

/// What a step logs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct StepLog {
    saves: Vec<SaveCause>,
    abandoned: u32,
}

const USER_CAUSES: [SaveCause; 6] = [
    SaveCause::LastWindowClosed,
    SaveCause::LastTabClosed,
    SaveCause::SignOut,
    SaveCause::Shutdown,
    SaveCause::Restart,
    SaveCause::ExitThroughSettings,
];

const ALL_CAUSES: [SaveCause; 8] = [
    SaveCause::Checkpoint,
    SaveCause::LastWindowClosed,
    SaveCause::LastTabClosed,
    SaveCause::ExplorerTerminated,
    SaveCause::SignOut,
    SaveCause::Shutdown,
    SaveCause::Restart,
    SaveCause::ExitThroughSettings,
];

#[derive(Clone, Debug)]
struct Model {
    /// The Session_File before the first startup.
    initial_file: Option<MDoc>,
    durable: Option<MDoc>,
    desired: Option<MDoc>,
    write: WriteState,
    live: Live,
    restoring: bool,
    attempting: bool,
    /// A change was held back by a restoration.
    dirty: bool,
    checkpoint: bool,
    /// The document the running attempt's write holds.
    attempt_doc: Option<MDoc>,
    next_id: u32,
    /// What the last step (or the startup) logs.
    step: StepLog,
    abandoned_total: u32,
}

impl Model {
    fn pending(&self) -> Option<MDoc> {
        self.desired.filter(|d| d.status == Status::Pending)
    }

    fn holds_retry_snapshot(&self) -> bool {
        self.pending().is_some_and(|d| d.failed_attempts >= 1)
    }

    fn protected(&self) -> bool {
        !self.restoring && self.holds_retry_snapshot()
    }

    fn checkpoints_allowed(&self) -> bool {
        !self.restoring && !self.protected() && self.live != Live::Closed
    }

    fn issue(&mut self, d: MDoc) {
        self.desired = Some(d);
        self.write = WriteState::InFlight(0);
    }

    fn live_changed(&mut self) {
        if self.write == WriteState::Exhausted {
            self.write = WriteState::InFlight(0);
        }
        if self.checkpoints_allowed() {
            self.checkpoint = true;
        } else if self.restoring {
            self.dirty = true;
        }
    }

    fn consume(&mut self) -> bool {
        match self.pending() {
            Some(mut d) => {
                d.status = Status::Consumed;
                self.issue(d);
                true
            }
            None => false,
        }
    }

    /// Req 4.11: the third failed attempt abandons the snapshot.
    fn abandon_if_exhausted(&mut self) -> bool {
        if self.attempting || !self.pending().is_some_and(|d| d.failed_attempts >= MAX_FAILED_ATTEMPTS) {
            return false;
        }
        self.consume();
        self.step.abandoned += 1;
        self.abandoned_total += 1;
        true
    }

    fn begin_restoration(&mut self) {
        self.restoring = true;
        if std::mem::take(&mut self.checkpoint) {
            self.dirty = true;
        }
    }

    fn end_restoration(&mut self) {
        self.restoring = false;
        self.attempting = false;
        self.attempt_doc = None;
        if std::mem::take(&mut self.dirty) {
            self.live_changed();
        }
    }

    /// Increment-before-attempt (Req 4.6, 4.7).
    fn begin_attempt(&mut self) -> bool {
        if self.attempting {
            return false;
        }
        let Some(mut d) = self.pending() else { return false };
        if d.failed_attempts >= MAX_FAILED_ATTEMPTS {
            return false;
        }
        d.failed_attempts += 1;
        self.begin_restoration();
        self.attempting = true;
        self.attempt_doc = Some(d);
        self.issue(d);
        true
    }

    /// Req 3.10 and the protection: a user-caused Session_End replaces the
    /// file with the new snapshot at count 0; a termination keeps a retry
    /// snapshot.
    fn session_end(&mut self, cause: SaveCause, snap: u32) {
        self.checkpoint = false;
        self.dirty = false;
        if cause == SaveCause::ExplorerTerminated && self.holds_retry_snapshot() {
            return;
        }
        self.attempting = false;
        self.attempt_doc = None;
        self.issue(MDoc { status: Status::Pending, failed_attempts: 0, cause, snap });
        self.step.saves.push(cause);
    }

    /// The process (re)starts with the file on disk.
    fn startup(&mut self) {
        self.desired = self.durable;
        self.write = WriteState::Done;
        self.restoring = false;
        self.attempting = false;
        self.dirty = false;
        self.checkpoint = false;
        self.attempt_doc = None;
        if let Some(d) = self.durable {
            // Abrupt end or power loss (Req 3.8), not again for a retry snapshot.
            if d.status == Status::Pending && d.cause == SaveCause::Checkpoint && d.failed_attempts == 0 {
                self.step.saves.push(SaveCause::Checkpoint);
            }
        }
        self.abandon_if_exhausted();
        // The tracker starts with the open windows (Req 2.9).
        if self.live != Live::Closed {
            self.live_changed();
        }
    }

    /// Time passes: the scheduled checkpoint and a retry fire.
    fn tick(&mut self) {
        if std::mem::take(&mut self.checkpoint)
            && self.checkpoints_allowed()
            && let Live::Snap(id) = self.live
        {
            let d = MDoc { status: Status::Pending, failed_attempts: 0, cause: SaveCause::Checkpoint, snap: id };
            self.issue(d);
            return;
        }
        if let WriteState::RetryPending(n) = self.write {
            self.write = WriteState::InFlight(n);
        }
    }

    fn flush(&mut self) {
        if let WriteState::InFlight(_) = self.write {
            self.durable = self.desired;
            self.write = WriteState::Done;
        }
    }

    fn fail_writes(&mut self) {
        if let WriteState::InFlight(r) = self.write {
            self.write = if r < MAX_RETRIES { WriteState::RetryPending(r + 1) } else { WriteState::Exhausted };
        }
    }

    fn attempt_durable(&self) -> bool {
        self.attempting && self.attempt_doc.is_some() && self.durable == self.attempt_doc
    }

    fn fresh(&self) -> u32 {
        self.next_id
    }
}

#[derive(Clone, Debug)]
enum Transition {
    LiveChange(Live),
    Tick,
    Flush { reverse: bool },
    FailWrites,
    SessionEnd { cause: SaveCause, snap: u32 },
    Trigger { snap: u32, restore: bool },
    OpenWindows,
    Completed,
    Failed,
    Crash,
}

fn preconditions(m: &Model, t: &Transition) -> bool {
    match t {
        Transition::LiveChange(_) => m.live != Live::Closed || m.restoring,
        Transition::SessionEnd { .. } => m.live != Live::Closed,
        Transition::Trigger { .. } => m.live == Live::Closed && !m.restoring,
        Transition::OpenWindows => m.attempt_durable(),
        Transition::Completed => m.restoring && (!m.attempting || m.attempt_durable()),
        Transition::Failed => m.restoring,
        Transition::Tick | Transition::Flush { .. } | Transition::FailWrites | Transition::Crash => true,
    }
}

struct StoreModel;

impl ReferenceStateMachine for StoreModel {
    type State = Model;
    type Transition = Transition;

    fn init_state() -> BoxedStrategy<Model> {
        let doc = (
            prop_oneof![3 => Just(Status::Pending), 1 => Just(Status::Consumed)],
            0..=MAX_FAILED_ATTEMPTS,
            select(ALL_CAUSES.to_vec()),
        )
            .prop_map(|(status, failed_attempts, cause)| MDoc { status, failed_attempts, cause, snap: 1 });
        let file = prop_oneof![1 => Just(None), 4 => doc.prop_map(Some)];
        let live = prop_oneof![2 => Just(Live::Closed), 1 => Just(Live::Empty), 2 => Just(Live::Snap(2))];
        (file, live)
            .prop_map(|(file, live)| {
                let mut m = Model {
                    initial_file: file,
                    durable: file,
                    desired: file,
                    write: WriteState::Done,
                    live,
                    restoring: false,
                    attempting: false,
                    dirty: false,
                    checkpoint: false,
                    attempt_doc: None,
                    next_id: 3,
                    step: StepLog::default(),
                    abandoned_total: 0,
                };
                m.startup();
                m
            })
            .boxed()
    }

    fn transitions(m: &Model) -> BoxedStrategy<Transition> {
        let id = m.fresh();
        let mut options: Vec<(u32, BoxedStrategy<Transition>)> = vec![
            (4, Just(Transition::Tick).boxed()),
            (5, any::<bool>().prop_map(|reverse| Transition::Flush { reverse }).boxed()),
            (1, Just(Transition::FailWrites).boxed()),
            (2, Just(Transition::Crash).boxed()),
        ];
        if m.live != Live::Closed || m.restoring {
            let live = prop_oneof![3 => Just(Live::Snap(id)), 1 => Just(Live::Empty)];
            options.push((3, live.prop_map(Transition::LiveChange).boxed()));
        }
        if m.live != Live::Closed {
            options.push((
                2,
                select(USER_CAUSES.to_vec()).prop_map(move |cause| Transition::SessionEnd { cause, snap: id }).boxed(),
            ));
            options.push((
                3,
                Just(Transition::SessionEnd { cause: SaveCause::ExplorerTerminated, snap: id }).boxed(),
            ));
        }
        if m.live == Live::Closed && !m.restoring {
            options.push((
                4,
                prop::bool::weighted(0.85).prop_map(move |restore| Transition::Trigger { snap: id, restore }).boxed(),
            ));
        }
        if m.attempt_durable() {
            options.push((4, Just(Transition::OpenWindows).boxed()));
        }
        if m.restoring && (!m.attempting || m.attempt_durable()) {
            options.push((1, Just(Transition::Completed).boxed()));
        }
        if m.restoring {
            options.push((3, Just(Transition::Failed).boxed()));
        }
        proptest::strategy::Union::new_weighted(options).boxed()
    }

    fn preconditions(m: &Model, t: &Transition) -> bool {
        preconditions(m, t)
    }

    fn apply(mut m: Model, t: &Transition) -> Model {
        m.step = StepLog::default();
        match *t {
            Transition::LiveChange(live) => {
                if let Live::Snap(id) = live {
                    m.next_id = m.next_id.max(id + 1);
                }
                m.live = live;
                m.live_changed();
            }
            Transition::Tick => m.tick(),
            Transition::Flush { .. } => m.flush(),
            Transition::FailWrites => m.fail_writes(),
            Transition::SessionEnd { cause, snap } => {
                m.next_id = m.next_id.max(snap + 1);
                m.session_end(cause, snap);
                if matches!(
                    cause,
                    SaveCause::LastWindowClosed | SaveCause::LastTabClosed | SaveCause::ExplorerTerminated
                ) {
                    m.live = Live::Closed;
                }
            }
            Transition::Trigger { snap, restore } => {
                m.next_id = m.next_id.max(snap + 1);
                m.begin_restoration();
                m.live = Live::Snap(snap);
                m.live_changed();
                if restore {
                    m.abandon_if_exhausted();
                    m.begin_attempt();
                }
                if !m.attempting {
                    m.end_restoration();
                }
            }
            Transition::OpenWindows => {
                let d = m.attempt_doc.expect("precondition");
                m.live = Live::Snap(d.snap);
                m.live_changed();
            }
            Transition::Completed => {
                if std::mem::take(&mut m.attempting) {
                    m.consume();
                }
                m.dirty = true;
                m.end_restoration();
            }
            Transition::Failed => {
                m.attempting = false;
                if m.abandon_if_exhausted() {
                    m.dirty = true;
                }
                m.end_restoration();
            }
            Transition::Crash => m.startup(),
        }
        m
    }
}

// ---------------------------------------------------------------------------
// System under test
// ---------------------------------------------------------------------------

struct Sut {
    store: StoreLogic,
    now: Millis,
    /// Store timers: (due, seq).
    timers: BTreeSet<(Millis, u64)>,
    /// Issued session writes not completed yet.
    inflight: BTreeMap<u64, String>,
    /// What the disk holds, and the `seq` of the write it came from (0: from
    /// before this process started).
    disk: Option<String>,
    disk_seq: u64,
    live: Live,
}

impl Sut {
    fn start(disk: Option<String>, live: Live, now: Millis) -> (Sut, StepLog) {
        let read = match &disk {
            Some(text) => SessionFileRead::Content(text.clone().into_bytes()),
            None => SessionFileRead::Absent,
        };
        let (store, cmds) = StoreLogic::startup(&read);
        let mut sut = Sut {
            store,
            now,
            timers: BTreeSet::new(),
            inflight: BTreeMap::new(),
            disk,
            disk_seq: 0,
            live,
        };
        let mut log = StepLog::default();
        sut.absorb(cmds, &mut log);
        if live != Live::Closed {
            let cmds = sut.store.live_changed(sut.now, &live);
            sut.absorb(cmds, &mut log);
        }
        (sut, log)
    }

    /// Takes the store's commands: records writes and timers, collects the
    /// `SAVE` and `ABANDONED` entries, and checks every checkpoint write.
    fn absorb(&mut self, cmds: Vec<Command>, log: &mut StepLog) {
        let protected = self.store.is_protected();
        for c in cmds {
            match c {
                Command::WriteSessionFile { seq, text, urgent } => {
                    let doc = parse(text.as_bytes()).unwrap_or_else(|e| panic!("{e:?} in\n{text}"));
                    assert!(doc.failed_attempts <= MAX_FAILED_ATTEMPTS, "{doc:?}");
                    if !urgent {
                        // Checkpoints (and their retries) are the only
                        // non-urgent writes; never while protected.
                        assert_eq!(
                            (doc.status, doc.failed_attempts, doc.cause),
                            (Status::Pending, 0, SaveCause::Checkpoint)
                        );
                        assert!(!protected, "checkpoint written while protected: {doc:?}");
                    }
                    self.inflight.insert(seq, text);
                }
                Command::SetTimer { id: TimerId { owner: TimerOwner::Store, seq }, at } => {
                    assert!(at > self.now, "store timer set in the past");
                    self.timers.insert((at, seq));
                }
                Command::AppendLog(LogEntry::Save { cause, .. }) => log.saves.push(cause),
                Command::AppendLog(LogEntry::Abandoned) => log.abandoned += 1,
                Command::PreserveBackup { .. } => panic!("no malformed file in this test"),
                _ => {}
            }
        }
    }

    fn complete(&mut self, seq: u64, result: Result<(), IoErrorKind>, log: &mut StepLog) {
        let text = self.inflight.remove(&seq).expect("in flight");
        if result.is_ok() && seq > self.disk_seq {
            self.disk = Some(text);
            self.disk_seq = seq;
        }
        let cmds = self.store.handle(self.now, None, &Event::WriteCompleted { seq, result }, &self.live);
        self.absorb(cmds, log);
    }

    fn step(&mut self, m: &Model, t: Transition) -> StepLog {
        let mut log = StepLog::default();
        match t {
            Transition::LiveChange(live) => {
                self.live = live;
                let cmds = self.store.live_changed(self.now, &self.live);
                self.absorb(cmds, &mut log);
            }
            Transition::Tick => {
                self.now = self.now + 2_000;
                while let Some(&(at, seq)) = self.timers.first().filter(|(at, _)| *at <= self.now) {
                    self.timers.remove(&(at, seq));
                    let event = Event::Timer(TimerId { owner: TimerOwner::Store, seq });
                    let cmds = self.store.handle(at, None, &event, &self.live);
                    self.absorb(cmds, &mut log);
                }
            }
            Transition::Flush { reverse } => {
                let mut seqs: Vec<u64> = self.inflight.keys().copied().collect();
                if reverse {
                    seqs.reverse();
                }
                for seq in seqs {
                    self.complete(seq, Ok(()), &mut log);
                }
            }
            Transition::FailWrites => {
                let seqs: Vec<u64> = self.inflight.keys().copied().collect();
                for seq in seqs {
                    self.complete(seq, Err(IoErrorKind::DiskFull), &mut log);
                }
            }
            Transition::SessionEnd { cause, snap } => {
                let cmds = self.store.session_end(self.now, None, cause, snapshot(snap));
                self.absorb(cmds, &mut log);
                self.live = m.live;
            }
            Transition::Trigger { snap, restore } => {
                self.store.begin_restoration();
                self.live = Live::Snap(snap);
                let cmds = self.store.live_changed(self.now, &self.live);
                self.absorb(cmds, &mut log);
                if restore {
                    let cmds = self.store.abandon_if_exhausted();
                    self.absorb(cmds, &mut log);
                    let before = self.store.pending_snapshot().cloned();
                    if let Some((seq, cmd)) = self.store.begin_attempt() {
                        let before = before.expect("an attempt needs a Pending_Snapshot");
                        assert!(before.failed_attempts < MAX_FAILED_ATTEMPTS, "attempt for {before:?}");
                        let Command::WriteSessionFile { seq: s, text, urgent } = &cmd else {
                            panic!("attempt command {cmd:?}")
                        };
                        assert_eq!(*s, seq);
                        assert!(*urgent, "the attempt write is urgent");
                        // Only the counter changes (Req 4.6).
                        let mut expected = before.clone();
                        expected.failed_attempts += 1;
                        assert_eq!(parse(text.as_bytes()).expect("parses"), expected);
                        self.absorb(vec![cmd], &mut log);
                    }
                }
                if !self.store.is_attempting() {
                    let cmds = self.store.end_restoration(self.now, &self.live);
                    self.absorb(cmds, &mut log);
                }
            }
            Transition::OpenWindows => {
                // Increment-before-attempt: the counted attempt is on disk
                // before any window-affecting command.
                let attempt = to_doc(&m.attempt_doc.expect("precondition"));
                assert_eq!(self.store.durable(), &FileView::Doc(attempt.clone()), "attempt not durable");
                let disk = self.disk.as_ref().expect("attempt on disk");
                assert_eq!(parse(disk.as_bytes()).expect("parses"), attempt, "attempt not on disk");
                self.live = m.live;
                let cmds = self.store.live_changed(self.now, &self.live);
                self.absorb(cmds, &mut log);
            }
            Transition::Completed => {
                let cmds = self.store.restoration_completed(self.now, &self.live);
                self.absorb(cmds, &mut log);
            }
            Transition::Failed => {
                let cmds = self.store.restoration_failed(self.now, &self.live);
                self.absorb(cmds, &mut log);
            }
            Transition::Crash => {
                // Writes in flight are lost; the next process reads the disk.
                let (sut, startup_log) = Sut::start(self.disk.take(), self.live, self.now + 1);
                *self = sut;
                log = startup_log;
            }
        }
        log
    }
}

struct StoreLifecycleTest;

impl StateMachineTest for StoreLifecycleTest {
    type SystemUnderTest = Sut;
    type Reference = StoreModel;

    fn init_test(m: &Model) -> Sut {
        let disk = m.initial_file.as_ref().map(|d| serialize(&to_doc(d)));
        let (sut, log) = Sut::start(disk, m.live, Millis(0));
        assert_eq!(log, m.step, "startup logs");
        sut
    }

    fn apply(mut sut: Sut, m: &Model, t: Transition) -> Sut {
        let log = sut.step(m, t.clone());
        assert_eq!(log, m.step, "logs of {t:?}");
        sut
    }

    fn check_invariants(sut: &Sut, m: &Model) {
        let s = &sut.store;
        assert_eq!(s.durable(), &to_view(&m.durable), "durable view");
        assert_eq!(s.desired(), &to_view(&m.desired), "desired view");
        let disk = sut.disk.as_ref().map(|t| parse(t.as_bytes()).expect("the disk parses"));
        assert_eq!(disk, m.durable.as_ref().map(to_doc), "disk content");
        assert_eq!(s.is_protected(), m.protected(), "protection");
        assert_eq!(s.is_restoring(), m.restoring, "restoring");
        assert_eq!(s.is_attempting(), m.attempting, "attempting");
        assert_eq!(s.checkpoint_due().is_some(), m.checkpoint, "scheduled checkpoint");
        match m.write {
            WriteState::Done => {
                assert_eq!(s.writes_in_flight(), 0);
                assert_eq!(s.retry_due(), None);
                assert!(!s.awaiting_change_to_retry());
            }
            WriteState::InFlight(_) => {
                assert!(s.writes_in_flight() > 0);
                assert_eq!(s.retry_due(), None);
                assert!(!s.awaiting_change_to_retry());
            }
            WriteState::RetryPending(n) => {
                assert_eq!(s.writes_in_flight(), 0);
                assert_eq!(s.retry_due().map(|(_, r)| r), Some(n));
            }
            WriteState::Exhausted => {
                assert_eq!(s.writes_in_flight(), 0);
                assert!(s.awaiting_change_to_retry());
            }
        }
        // While an attempt runs, its snapshot stays Pending and unchanged.
        if let Some(d) = m.attempt_doc {
            assert_eq!(s.pending_snapshot(), Some(&to_doc(&d)), "snapshot changed during the attempt");
        }
    }
}

prop_state_machine! {
    #![proptest_config(Config { cases: 256, ..Config::default() })]

    #[test]
    fn store_lifecycle_matches_the_protection_model(sequential 1..=80 => StoreLifecycleTest);
}

// ---------------------------------------------------------------------------
// Scripted runs
// ---------------------------------------------------------------------------

fn initial(file: Option<MDoc>, live: Live) -> Model {
    let mut m = Model {
        initial_file: file,
        durable: file,
        desired: file,
        write: WriteState::Done,
        live,
        restoring: false,
        attempting: false,
        dirty: false,
        checkpoint: false,
        attempt_doc: None,
        next_id: 3,
        step: StepLog::default(),
        abandoned_total: 0,
    };
    m.startup();
    m
}

/// Runs `ts` through the model (checking preconditions) and the test.
fn run(m: Model, ts: Vec<Transition>) -> Model {
    let mut end = m.clone();
    for t in &ts {
        assert!(preconditions(&end, t), "{t:?} not allowed in {end:?}");
        end = StoreModel::apply(end, t);
    }
    StoreLifecycleTest::test_sequential(Config::default(), m, ts, None);
    end
}

fn pending(cause: SaveCause, failed_attempts: u8) -> MDoc {
    MDoc { status: Status::Pending, failed_attempts, cause, snap: 1 }
}

/// Three in-process failures, File Explorer terminated after each of the
/// first two: the retry snapshot is kept and counted, the third failure
/// abandons it once, and checkpointing resumes.
#[test]
fn three_in_process_failures_abandon_once() {
    use Transition::*;
    let m = initial(Some(pending(SaveCause::LastWindowClosed, 0)), Live::Closed);
    let attempt = |snap| vec![Trigger { snap, restore: true }, Flush { reverse: false }, OpenWindows, Failed];
    let mut ts = Vec::new();
    for snap in [10, 11, 12] {
        ts.extend(attempt(snap));
        ts.extend([Tick, Flush { reverse: false }]);
        if snap != 12 {
            ts.push(SessionEnd { cause: SaveCause::ExplorerTerminated, snap: snap + 100 });
        }
    }
    ts.extend([LiveChange(Live::Snap(20)), Tick, Flush { reverse: false }]);
    let end = run(m, ts);
    assert_eq!(end.abandoned_total, 1);
    assert_eq!(end.durable, Some(MDoc { status: Status::Pending, failed_attempts: 0, cause: SaveCause::Checkpoint, snap: 20 }));
}

/// The process ends during each of three attempts: each end is counted
/// without any action at crash time, and the next start abandons the
/// snapshot.
#[test]
fn three_crashes_mid_attempt_abandon_at_the_next_start() {
    use Transition::*;
    let m = initial(Some(pending(SaveCause::Shutdown, 0)), Live::Closed);
    let mut ts = Vec::new();
    for snap in [10, 11, 12] {
        ts.extend([Trigger { snap, restore: true }, Flush { reverse: true }, OpenWindows, Crash]);
        ts.extend([Tick, Flush { reverse: false }]);
        if snap != 12 {
            ts.push(SessionEnd { cause: SaveCause::ExplorerTerminated, snap: snap + 100 });
        }
    }
    let end = run(m, ts);
    assert_eq!(end.abandoned_total, 1);
    // Checkpointing resumed with the open windows after the abandonment.
    assert_eq!(end.durable, Some(MDoc { status: Status::Pending, failed_attempts: 0, cause: SaveCause::Checkpoint, snap: 1 }));
}

/// A user-caused Session_End during a protected retry replaces it with the
/// Live_Session at count 0; a crash before the attempt write lands leaves
/// the count unchanged.
#[test]
fn user_session_end_ends_protection_and_unlanded_attempts_do_not_count() {
    use Transition::*;
    let m = initial(Some(pending(SaveCause::LastTabClosed, 1)), Live::Closed);
    let ts = vec![
        Trigger { snap: 10, restore: true },
        Crash,
        SessionEnd { cause: SaveCause::ExplorerTerminated, snap: 20 },
        Trigger { snap: 11, restore: true },
        Flush { reverse: false },
        OpenWindows,
        Failed,
        SessionEnd { cause: SaveCause::LastWindowClosed, snap: 30 },
        Flush { reverse: false },
    ];
    let end = run(m, ts);
    assert_eq!(end.durable, Some(MDoc { status: Status::Pending, failed_attempts: 0, cause: SaveCause::LastWindowClosed, snap: 30 }));
    assert!(!end.protected());
}
