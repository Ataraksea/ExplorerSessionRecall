// Feature: explorer-session-restore, Property 1: Live session tracking matches the reference model
//! Property 1 (task 7.8): a model-based state-machine test of the
//! `LiveSessionTracker` against the esr-sim reference model.
//!
//! - **Model** ([`ReferenceStateMachine`]): a [`SimExplorer`] that only
//!   applies the operations. Its [`Reference`](esr_sim::reference::Reference)
//!   is the expected Live_Session (tab order, Locations of completed
//!   navigations, active tabs, placements, Activation_Order, and the
//!   Session_Ends a closing of the last window records).
//! - **System under test** ([`StateMachineTest`]): the tracker, fed by a
//!   second, lock-step `SimExplorer` that also carries out the tracker's
//!   commands (`RescanAll`) and whose tracker timers (tombstones) are
//!   delivered at their time.
//! - **Transitions**: the sim's operations (`OpsConfig::tracking()`: open or
//!   close windows and tabs, navigate with or without completion, activate,
//!   reorder, drag out/in, move/resize/minimize/maximize/restore, separate
//!   processes, shell loss and restoration, time passing).
//! - **Initial state**: a random set of windows opened either while the
//!   tracker is already running or before it starts, in which case it starts
//!   with those windows already open (Req 2.9), from the adapters' startup
//!   scans and activations.
//!
//! After every transition a copy of the SUT is settled (debounce and
//! tombstone timers expired, 1 s of logical time; the shell is restored first
//! if it is lost, since the tracker cannot see anything meanwhile). Then
//! `capture()` must equal the model's snapshot (compared with `canonical`;
//! `LocationId` equality is by canonical Location key). The Session_Ends the
//! tracker emitted must equal the model's (cause and pre-close snapshot), as
//! far as they are exact: those recorded before the tracker started are not
//! comparable, and from the first shell loss on the tracker cannot see
//! windows or tabs the model knows (as in `tracker_smoke`).
//!
//! **Validates: Requirements 2.1, 2.2, 2.3, 2.4, 2.5, 2.7, 2.8, 2.9, 8.1**

use std::collections::BTreeSet;
use std::fmt;

use esr_core::engine::{Command, Event, Millis, TimerId};
use esr_core::model::{SaveCause, Snapshot};
use esr_core::tracker::{LiveSessionTracker, SessionEnd};
use esr_sim::explorer::{op_strategy, ops_strategy, Op, OpsConfig, SimConfig, SimExplorer, TimedEvent, UiaNaming};
use esr_sim::reference::canonical;
use proptest::prelude::*;
use proptest::test_runner::Config;
use proptest_state_machine::{prop_state_machine, ReferenceStateMachine, StateMachineTest};

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

/// The simulator configurations the tracker must match the model under:
/// display-name strips (22621), full-path strips (26200), and adapters that
/// report the active tab's key without HWND reuse.
fn sim_config() -> impl Strategy<Value = (&'static str, SimConfig)> {
    prop_oneof![
        Just(("display-names", SimConfig::default())),
        Just(("full-paths", SimConfig { uia_naming: UiaNaming::FullPaths, ..SimConfig::default() })),
        Just((
            "active-key",
            SimConfig { report_active_key: true, reuse_hwnds: false, ..SimConfig::default() }
        )),
    ]
}

#[derive(Clone)]
struct ModelState {
    variant: &'static str,
    config: SimConfig,
    /// Operations that open the initial set of windows.
    initial: Vec<Op>,
    /// Whether the tracker already runs during `initial` (otherwise it starts
    /// right after, with those windows open).
    tracker_from_start: bool,
    /// The model: a simulator that applies the operations, nothing else.
    sim: SimExplorer,
}

impl fmt::Debug for ModelState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelState")
            .field("variant", &self.variant)
            .field("tracker_from_start", &self.tracker_from_start)
            .field("initial", &self.initial)
            .field("expected", &canonical(&self.sim.reference().snapshot()))
            .finish()
    }
}

struct LiveSessionModel;

impl ReferenceStateMachine for LiveSessionModel {
    type State = ModelState;
    type Transition = Op;

    fn init_state() -> BoxedStrategy<ModelState> {
        (sim_config(), ops_strategy(&OpsConfig::tracking(), 0..6), prop::bool::weighted(0.5))
            .prop_map(|((variant, config), mut initial, tracker_from_start)| {
                let mut sim = SimExplorer::new(config.clone());
                sim.apply_all(&initial);
                if !tracker_from_start && sim.shell_lost() {
                    // The tracker starts once the shell is ready (Engine
                    // startup waits for it); the startup scans and
                    // activations need it.
                    initial.push(Op::RestoreShell);
                    sim.apply(&Op::RestoreShell);
                }
                ModelState { variant, config, initial, tracker_from_start, sim }
            })
            .boxed()
    }

    /// Property 1's operation mix. Shell loss is drawn depending on the
    /// state: rarely while the shell is available (about half the cases
    /// lose it at some point, so the other half compare every Session_End),
    /// and restoration is likely soon after a loss.
    fn transitions(state: &ModelState) -> BoxedStrategy<Op> {
        let ops = op_strategy(&OpsConfig { shell_loss: false, ..OpsConfig::tracking() });
        if state.sim.shell_lost() {
            prop_oneof![6 => ops, 1 => Just(Op::RestoreShell)].boxed()
        } else {
            prop_oneof![150 => ops, 1 => Just(Op::LoseShell)].boxed()
        }
    }

    fn apply(mut state: ModelState, op: &Op) -> ModelState {
        state.sim.apply(op);
        state
    }
}

// ---------------------------------------------------------------------------
// System under test
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Sut {
    tracker: LiveSessionTracker,
    /// The simulator feeding the tracker (lock-step with the model's).
    sim: SimExplorer,
    /// Tracker timers, delivered at their time.
    timers: BTreeSet<(Millis, TimerId)>,
    ends: Vec<SessionEnd>,
    now: Millis,
    /// Model Session_Ends recorded before the tracker started.
    ends_offset: usize,
    /// Number of model Session_Ends (from `ends_offset`) recorded before the
    /// first shell loss; `None` while the shell has never been lost.
    exact_ends: Option<usize>,
}

impl Sut {
    fn deliver(&mut self, at: Millis, event: &Event) {
        self.now = self.now.max(at);
        let out = self.tracker.handle(self.now, event);
        self.ends.extend(out.session_end);
        for c in out.commands {
            match c {
                Command::SetTimer { id, at } => {
                    self.timers.insert((at, id));
                }
                Command::AppendLog(_) => {}
                c => {
                    self.sim.handle_command(&c);
                }
            }
        }
    }

    /// Fires every tracker timer due before `until`.
    fn fire_until(&mut self, until: Millis) {
        while let Some(&(at, id)) = self.timers.first().filter(|(at, _)| *at < until) {
            self.timers.remove(&(at, id));
            self.deliver(at, &Event::Timer(id));
        }
    }

    fn feed(&mut self, events: Vec<TimedEvent>) {
        for e in events {
            self.fire_until(e.at);
            self.deliver(e.at, &e.event);
        }
    }

    fn model_ends(&self) -> usize {
        self.sim.reference().session_ends().len()
    }

    fn note_shell_loss(&mut self) {
        if self.sim.shell_lost() && self.exact_ends.is_none() {
            self.exact_ends = Some(self.model_ends() - self.ends_offset);
        }
    }

    /// A copy of `self` with every timer expired: 1 s of logical time after
    /// the last operation (the shell restored first if it is lost), repeated
    /// until the tracker's commands cause no further events.
    fn settled(&self) -> Sut {
        let mut s = self.clone();
        let mut events = if s.sim.shell_lost() { s.sim.finish() } else { s.sim.settle() };
        for _ in 0..8 {
            let quiet = events.is_empty();
            s.feed(events);
            s.fire_until(Millis(u64::MAX));
            if quiet {
                return s;
            }
            events = s.sim.settle();
        }
        panic!("the simulator does not settle");
    }
}

struct LiveSessionTest;

impl StateMachineTest for LiveSessionTest {
    type SystemUnderTest = Sut;
    type Reference = LiveSessionModel;

    fn init_test(model: &ModelState) -> Sut {
        let mut sut = Sut {
            tracker: LiveSessionTracker::new(),
            sim: SimExplorer::new(model.config.clone()),
            timers: BTreeSet::new(),
            ends: Vec::new(),
            now: Millis(0),
            ends_offset: 0,
            exact_ends: None,
        };
        if model.tracker_from_start {
            for op in &model.initial {
                let events = sut.sim.apply(op);
                sut.feed(events);
                sut.note_shell_loss();
            }
        } else {
            // The windows open before the tracker runs; it starts with them
            // already open (Req 2.9).
            sut.sim.apply_all(&model.initial);
            sut.ends_offset = sut.model_ends();
            let events = sut.sim.startup_events();
            sut.now = sut.sim.now();
            sut.feed(events);
            sut.note_shell_loss();
        }
        sut
    }

    fn apply(mut sut: Sut, _model: &ModelState, op: Op) -> Sut {
        let events = sut.sim.apply(&op);
        sut.feed(events);
        sut.note_shell_loss();
        sut
    }

    fn check_invariants(sut: &Sut, model: &ModelState) {
        // The tracker's commands never change what Explorer shows.
        assert_eq!(sut.sim.reference(), model.sim.reference(), "simulators out of lock-step");

        let settled = sut.settled();
        let (actual, _) = settled.tracker.capture();
        let expected = model.sim.reference().snapshot();
        assert_eq!(canonical(&actual), canonical(&expected), "settled capture differs from the model");

        let expected_ends: Vec<(SaveCause, Snapshot)> = model.sim.reference().session_ends()[sut.ends_offset..]
            .iter()
            .map(|e| (e.cause, canonical(&e.snapshot)))
            .collect();
        let actual_ends: Vec<(SaveCause, Snapshot)> =
            settled.ends.iter().map(|e| (e.cause, canonical(&e.snapshot))).collect();
        match sut.exact_ends {
            None => assert_eq!(actual_ends, expected_ends, "Session_Ends differ from the model"),
            Some(k) => assert_eq!(
                actual_ends.get(..k),
                Some(&expected_ends[..k]),
                "Session_Ends before the shell loss differ from the model"
            ),
        }
    }
}

prop_state_machine! {
    #![proptest_config(Config { cases: 100, ..Config::default() })]

    #[test]
    fn live_session_tracking_matches_the_reference_model(sequential 1..=200 => LiveSessionTest);
}

/// The shrunk counterexample of the first run: windows opened while the
/// shell is lost are activated but not scanned, and a separate process
/// exiting meanwhile (its only window closed) must not cost them their
/// Activation_Order position once the shell is back.
#[test]
fn unscanned_activations_survive_another_process_exiting() {
    use esr_core::model::Rect;
    use esr_sim::explorer::{CloseOrder, NavOutcome, Pick, ProcessChoice};

    let open = |process, bottom| Op::OpenWindow {
        process,
        location: Pick(0),
        nav: NavOutcome::Complete,
        rect: Rect { left: 0, top: 0, right: 300, bottom },
        maximized: false,
    };
    let initial = vec![open(ProcessChoice::Main, 201)];
    let config = SimConfig::default();
    let mut sim = SimExplorer::new(config.clone());
    sim.apply_all(&initial);
    let state = ModelState { variant: "display-names", config, initial, tracker_from_start: false, sim };
    let transitions = vec![
        open(ProcessChoice::New, 200),
        open(ProcessChoice::New, 200),
        open(ProcessChoice::Main, 200),
        Op::LoseShell,
        open(ProcessChoice::Main, 200),
        // The second window (a separate process) closes; its process exits.
        Op::CloseWindow {
            win: Pick(1),
            order: CloseOrder { revoke_before: 0, revoke_after: false, gap_ms: 0, rotate: Pick(0) },
        },
        Op::Tick { ms: 100 },
    ];
    LiveSessionTest::test_sequential(Config::default(), state, transitions, None);
}

/// Shrunk counterexample: a window opens a few ms before a separate
/// process's `ProcessExited` (its only window closed ~200 ms earlier), then
/// the other window's last tab is dragged into the new one before its first
/// scan. The new window was activated after that process ended, so it still
/// counts as open and the emptied source closing is not a Session_End.
#[test]
fn window_activated_after_another_process_ended_stays_open() {
    use esr_core::model::Rect;
    use esr_sim::explorer::{NavOutcome, Pick, ProcessChoice};

    let open = |process, location, right| Op::OpenWindow {
        process,
        location: Pick(location),
        nav: NavOutcome::Complete,
        rect: Rect { left: 0, top: 0, right, bottom: 200 },
        maximized: false,
    };
    let config = SimConfig::default();
    let state = ModelState {
        variant: "display-names",
        config: config.clone(),
        initial: Vec::new(),
        tracker_from_start: true,
        sim: SimExplorer::new(config),
    };
    let transitions = vec![
        open(ProcessChoice::Main, 0, 300),
        open(ProcessChoice::New, 0, 400),
        // The separate process's window closes; its process exits 200 ms later.
        Op::CloseTab { win: Pick(1), tab: Pick(0), destroy_gap_ms: 7 },
        Op::Tick { ms: 196 },
        open(ProcessChoice::Main, 4, 500),
        // The first window's only tab goes into the new window.
        Op::DragIn { from: Pick(0), tab: Pick(0), to: Pick(0), position: Pick(0) },
    ];
    LiveSessionTest::test_sequential(Config::default(), state, transitions, None);
}
