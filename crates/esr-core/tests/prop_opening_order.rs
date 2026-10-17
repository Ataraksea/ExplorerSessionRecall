//! Model-based test of the Opening_Order and the Primary_Window per
//! Virtual_Desktop (task 22.8): the `Engine`'s `OpeningOrder`, driven through
//! window openings and closings, starts and resumptions with windows open,
//! completed restorations and desktop views, against the independent
//! reference model [`RefOpeningOrder`] of `esr-sim`.
//!
//! - **Model** ([`ReferenceStateMachine`]): the windows that exist (which
//!   desktop each is on, whether it is shown on all desktops, whether it is
//!   tracked, an in-progress Folding_Candidate, or an open window the tracker
//!   does not know since the shell was lost), the Activation_Order of the
//!   tracked ones, and a [`RefOpeningOrder`] that is told, in the words of
//!   Req 14.8, what happened: a window opened, a candidate's first signal, a
//!   candidate left open, a window or candidate destroyed, tracking started
//!   or resumed with these windows open, a restoration completed. Whether a
//!   new window is a candidate follows Req 14.3 / 14.10 (folding on, no bypass
//!   keys, a tracked window on its desktop).
//! - **System under test** ([`StateMachineTest`]): the `Engine`, fed the same
//!   events as a Windows adapter would send: `WindowAppeared` for a new window
//!   (followed by its scan and activation when it is an ordinary window), the
//!   first-navigation timeout of a candidate, `WindowDestroyed`,
//!   `ProcessExited`, `ShellLost` / `ShellAvailable` (after the startup
//!   rescan), `SettingsChanged`, and a restart (a new `Engine` that scans the
//!   windows that are open). A state may start from a completed restoration
//!   against the simulated Explorer (the trigger window, restored windows and
//!   a window the user opened meanwhile).
//!
//!   After every step the Opening_Order equals the model's, the candidates in
//!   progress and the windows the tracker knows are the model's, and for
//!   every desktop view (each of three desktops, windows shown on all
//!   desktops included, and `Unknown`) the Primary_Window is the first window
//!   of the order in the view ([`OpeningOrder::primary`]), and the
//!   Fold_Target that `select` takes for a candidate whose Location matches
//!   no open tab is the first window of the order that is open, tracked, not
//!   a candidate, and in the view (every window when it is `Unknown`).
//!
//! **Validates: Requirements 14.8, 14.9, 14.11, 14.12**

use std::collections::{BTreeMap, BTreeSet};

use esr_core::engine::{
    Command, DesktopView, Engine, Environment, Event, ExplorerOptionValues, Millis, SessionFileRead, StartupInput,
    TabKey, TabObservation, TimerId, TimerOwner, WinKey, WindowObservation,
};
use esr_core::engine::ObservedLocation;
use esr_core::fold::{select, FoldCtx, FoldPlan};
use esr_core::model::{
    DisplayRef, LocationId, Placement, Rect, SaveCause, SessionDoc, ShowState, Snapshot, Status, TabRec, Timestamp,
    WindowId, WindowRec,
};
use esr_core::settings::Settings;
use esr_sim::explorer::{default_displays, default_locations, SimConfig, SimExplorer};
use esr_sim::reference::RefOpeningOrder;
use esr_sim::restore::{home_location, Driver, Launch, Stop, UserAction};
use proptest::prelude::*;
use proptest::strategy::Union;
use proptest::test_runner::Config;
use proptest_state_machine::{prop_state_machine, ReferenceStateMachine, StateMachineTest};

// ---------------------------------------------------------------------------
// Windows, keys and events
// ---------------------------------------------------------------------------

/// The Virtual_Desktops of the model.
const DESKTOPS: u8 = 3;

/// Key of the first window the model opens (far from the simulator's keys).
const FIRST: u64 = 1 << 40;

/// A window the user opens while a restoration runs.
const OPENED_MEANWHILE: WinKey = WinKey { hwnd: 0xFFFF_0001, seq: 0xFFFF_0001 };

fn key(n: u64) -> WinKey {
    WinKey { hwnd: n, seq: n }
}

fn tab_of(win: WinKey) -> TabKey {
    TabKey { hwnd: win.hwnd | (1 << 41), seq: win.seq }
}

fn placement() -> Placement {
    let d = &default_displays()[0];
    Placement {
        state: ShowState::Normal,
        restored: Rect { left: 0, top: 0, right: 800, bottom: 600 },
        display: DisplayRef { device: d.device.clone(), monitor: d.monitor },
    }
}

/// The Location every window of the model shows.
fn shown() -> LocationId {
    default_locations()[2].clone()
}

/// A Location that no tab of any window shows.
fn nobody_shows() -> LocationId {
    LocationId {
        parsing_name: r"C:\Nobody\Shows\This".into(),
        display_name: "This".into(),
        ..default_locations()[2].clone()
    }
}

fn scan(win: WinKey, pid: u32) -> Event {
    let tab = tab_of(win);
    Event::WindowScanned(WindowObservation {
        win,
        pid,
        tabs: vec![TabObservation { tab, location: ObservedLocation::Known(shown()) }],
        active: Some(tab),
        strip: None,
        placement: placement(),
    })
}

fn engine_with(folding: bool) -> Engine {
    let startup = StartupInput {
        session_file: SessionFileRead::Absent,
        environment: Environment::default(),
        background_logon: false,
        logon_at: None,
        shell_ready_at: None,
    };
    Engine::new(Settings { tab_folding_enabled: folding, ..Settings::default() }, startup).0
}

// ---------------------------------------------------------------------------
// A completed restoration against the simulated Explorer
// ---------------------------------------------------------------------------

/// Left edges of the saved windows 1, 2 and 3.
const SAVED_LEFT: [i32; 3] = [100, 600, 1100];

fn rect(left: i32, top: i32) -> Rect {
    Rect { left, top, right: left + 800, bottom: top + 600 }
}

fn saved_window(id: u32) -> WindowRec {
    let left = SAVED_LEFT[id as usize - 1];
    let d = &default_displays()[0];
    WindowRec {
        id: WindowId(id),
        tabs: vec![TabRec { location: default_locations()[1 + id as usize].clone() }],
        active: 0,
        placement: Placement {
            state: ShowState::Normal,
            restored: rect(left, 40),
            display: DisplayRef { device: d.device.clone(), monitor: d.monitor },
        },
    }
}

/// A Pending_Snapshot of the saved windows `1..=count` whose Activation_Order
/// is `activation` (most recently activated first).
fn pending_file(count: u32, activation: &[u32]) -> String {
    esr_core::format::serialize(&SessionDoc {
        version: esr_core::format::FORMAT_VERSION,
        status: Status::Pending,
        failed_attempts: 0,
        saved_at: Timestamp { year: 2025, month: 3, day: 4, hour: 5, minute: 6, second: 7, millis: 8, offset_minutes: 0 },
        cause: SaveCause::LastWindowClosed,
        snapshot: Snapshot {
            windows: (1..=count).map(saved_window).collect(),
            activation: activation.iter().map(|&i| WindowId(i)).collect(),
        },
    })
}

/// What the simulator and the harness saw during a restoration.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Facts {
    /// The windows in the order their first signals were announced.
    announced: Vec<WinKey>,
    /// The Trigger_Window.
    trigger: WinKey,
    /// The restored windows in the saved Activation_Order (the Trigger_Window
    /// is the one the restoration reused for its saved window, if any).
    restored: Vec<WinKey>,
    /// The Activation_Order the tracker ends with (not under test here).
    activation: Vec<WinKey>,
}

/// Restores a Pending_Snapshot of `activation.len()` windows saved with
/// that Activation_Order, with the harness announcing every window just
/// before the first event that names it (the simulator reports no
/// `WindowAppeared` for ordinary windows) and the user opening one more
/// window right after the Trigger_Window. Returns the engine, the facts and
/// the time after the restoration.
fn run_restoration(activation: &[u32]) -> (Engine, Facts, Millis) {
    let startup = StartupInput {
        session_file: SessionFileRead::Content(pending_file(activation.len() as u32, activation).into_bytes()),
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
    let mut driver = Driver::new(SimExplorer::new(SimConfig::for_restoration()));
    driver.submit(commands);
    let mut announced: Vec<WinKey> = Vec::new();
    let mut destroyed = 0usize;
    let mut run = |driver: &mut Driver, engine: &mut Engine| {
        let stop = driver.run(|now, event| {
            let win = match &event {
                Event::WindowScanned(obs) => Some(obs.win),
                Event::WindowOpened { win, .. } | Event::InitialLocationKnown { win, .. } => Some(*win),
                _ => None,
            };
            let mut out = Vec::new();
            if let Some(win) = win.filter(|w| !announced.contains(w)) {
                announced.push(win);
                out.extend(engine.handle(
                    now,
                    Event::WindowAppeared {
                        win,
                        pid: 1,
                        bypass_keys: false,
                        desktop: DesktopView::Unknown,
                        foreground: None,
                    },
                ));
                if !announced.contains(&OPENED_MEANWHILE) {
                    announced.push(OPENED_MEANWHILE);
                    out.extend(engine.handle(
                        now,
                        Event::WindowAppeared {
                            win: OPENED_MEANWHILE,
                            pid: 1,
                            bypass_keys: false,
                            desktop: DesktopView::Unknown,
                            foreground: None,
                        },
                    ));
                }
            }
            if matches!(event, Event::WindowDestroyed { .. }) {
                destroyed += 1;
            }
            out.extend(engine.handle(now, event));
            out
        });
        assert_eq!(stop, Stop::Quiescent);
    };
    run(&mut driver, &mut engine);
    let at = driver.sim().now() + 1_000;
    driver.schedule(
        at,
        UserAction::Launch(Launch { location: home_location(), selection: vec![], rect: rect(300, 300), maximized: false }),
    );
    run(&mut driver, &mut engine);
    assert_eq!(destroyed, 0, "the scenario closes no window");

    let session = engine.restore_session().expect("a restoration ran");
    assert!(session.is_finished(), "the restoration completed");
    let trigger = session.trigger();
    // Saved window `id` is the window that ends up at its saved rectangle.
    let restored: Vec<WinKey> = activation
        .iter()
        .map(|&id| {
            let left = SAVED_LEFT[id as usize - 1];
            driver
                .sim()
                .window_records()
                .iter()
                .find(|r| r.closed_at.is_none() && r.placements.last().is_some_and(|p| p.1.restored.left == left))
                .unwrap_or_else(|| panic!("no window restored for saved window {id}"))
                .key
        })
        .collect();
    let now = driver.sim().now().0 + 10_000;
    // The window the user opened meanwhile is only known to the tracker now.
    engine.handle(Millis(now), scan(OPENED_MEANWHILE, 1));
    let now = now + 1;
    let tracked = engine.tracker().activation_order();
    assert_eq!(
        tracked.iter().copied().collect::<BTreeSet<_>>(),
        announced.iter().copied().collect::<BTreeSet<_>>(),
        "every announced window is tracked"
    );
    let facts = Facts { announced, trigger, restored, activation: tracked };
    (engine, facts, Millis(now))
}

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// An open window the tracker knows.
    Tracked,
    /// A Folding_Candidate in progress: hidden, owned by the `FoldingEngine`.
    Candidate,
    /// An open window the tracker does not know yet: a candidate that was
    /// revealed because the shell was lost, until the rescan at
    /// `ShellAvailable`.
    Unscanned,
}

#[derive(Clone, Debug)]
struct Win {
    key: WinKey,
    pid: u32,
    desk: u8,
    /// Shown on all desktops.
    pinned: bool,
    kind: Kind,
}

impl Win {
    fn on(&self, desk: u8) -> bool {
        self.pinned || self.desk == desk
    }
}

/// How a state starts.
#[derive(Clone, Debug)]
enum Start {
    /// The engine starts with these windows open, `(desktop, pinned)` each,
    /// in this Activation_Order (indices, most recent first).
    Windows { wins: Vec<(u8, bool)>, activation: Vec<usize> },
    /// A Pending_Snapshot of `activation.len()` windows saved with this
    /// Activation_Order is restored.
    Restoration { activation: Vec<u32> },
}

#[derive(Clone, Debug)]
struct Model {
    start: Start,
    folding: bool,
    shell_up: bool,
    next: u64,
    /// Every window that exists, in creation order.
    wins: Vec<Win>,
    /// The tracked windows, most recently activated first.
    activation: Vec<WinKey>,
    order: RefOpeningOrder,
}

#[derive(Clone, Debug)]
enum Tr {
    /// The user opens a window on `desk`, `pinned` to all desktops or not;
    /// its first signal carries the desktop view (`unknown_view`: could not
    /// be read) and the Fold_Bypass_Keys state.
    Open { desk: u8, pinned: bool, unknown_view: bool, bypass: bool, pid_group: u8 },
    /// A tracked window is activated.
    Activate(Pick),
    /// A window is destroyed.
    Close(Pick),
    /// A candidate's first-navigation timer expires: it is left open.
    Timeout(Pick),
    /// The Explorer process of some candidates exits.
    ProcessExit(Pick),
    /// The Tab_Folding option is switched.
    ToggleFolding,
    /// The shell is lost.
    ShellLost,
    /// The shell is back and the open windows are rescanned.
    ShellAvailable,
    /// The tool restarts with the windows that are open.
    Restart,
}

impl Model {
    fn start(start: Start) -> Model {
        match start {
            Start::Windows { ref wins, ref activation } => {
                let list: Vec<Win> = wins
                    .iter()
                    .enumerate()
                    .map(|(i, &(desk, pinned))| Win { key: key(FIRST + i as u64), pid: 1, desk, pinned, kind: Kind::Tracked })
                    .collect();
                let activation: Vec<WinKey> = activation.iter().map(|&i| list[i].key).collect();
                let mut order = RefOpeningOrder::new();
                order.tracking_started(&activation);
                Model {
                    next: FIRST + list.len() as u64,
                    folding: true,
                    shell_up: true,
                    wins: list,
                    activation,
                    order,
                    start,
                }
            }
            Start::Restoration { ref activation } => {
                let (_engine, facts, _) = run_restoration(activation);
                let mut order = RefOpeningOrder::new();
                // Nothing is open when the tool starts; the windows open one
                // by one and the restoration completes (Req 14.8).
                order.tracking_started(&[]);
                for &win in &facts.announced {
                    order.opened(win);
                }
                order.restoration_completed(Some(facts.trigger), &facts.restored);
                let list = facts
                    .announced
                    .iter()
                    .map(|&key| Win { key, pid: 1, desk: 0, pinned: false, kind: Kind::Tracked })
                    .collect();
                Model { next: FIRST, folding: true, shell_up: true, wins: list, activation: facts.activation, order, start }
            }
        }
    }

    fn tracked(&self) -> Vec<WinKey> {
        self.of_kind(Kind::Tracked)
    }

    fn candidates(&self) -> Vec<WinKey> {
        self.of_kind(Kind::Candidate)
    }

    fn of_kind(&self, kind: Kind) -> Vec<WinKey> {
        self.wins.iter().filter(|w| w.kind == kind).map(|w| w.key).collect()
    }

    fn win(&self, key: WinKey) -> &Win {
        self.wins.iter().find(|w| w.key == key).expect("a window of the model")
    }

    fn win_mut(&mut self, key: WinKey) -> &mut Win {
        self.wins.iter_mut().find(|w| w.key == key).expect("a window of the model")
    }

    /// The pids whose windows are all candidates.
    fn exitable_pids(&self) -> Vec<u32> {
        let mut pids: Vec<u32> = self
            .wins
            .iter()
            .filter(|w| w.kind == Kind::Candidate)
            .map(|w| w.pid)
            .filter(|pid| self.wins.iter().all(|w| w.pid != *pid || w.kind == Kind::Candidate))
            .collect();
        pids.sort_unstable();
        pids.dedup();
        pids
    }

    /// The desktop view of the first signal of a window on `desk`.
    fn view(&self, desk: u8, unknown: bool) -> DesktopView {
        if unknown {
            DesktopView::Unknown
        } else {
            DesktopView::Known(self.wins.iter().filter(|w| w.on(desk)).map(|w| w.key).collect())
        }
    }

    /// The window `Tr::Activate`, `Close`, `Timeout` or `ProcessExit` names.
    fn pick(&self, tr: &Tr) -> Option<WinKey> {
        match tr {
            Tr::Activate(i) => pick_of(&self.tracked(), i),
            Tr::Close(i) => pick_of(&self.closable(), i),
            Tr::Timeout(i) => pick_of(&self.candidates(), i),
            _ => None,
        }
    }

    fn pick_pid(&self, i: &Pick) -> Option<u32> {
        pick_of(&self.exitable_pids(), i)
    }

    /// Whether a new window is a Folding_Candidate (Req 14.2-14.4, 14.10).
    fn is_candidate(&self, desk: u8, unknown_view: bool, bypass: bool) -> bool {
        self.folding
            && !bypass
            && self.wins.iter().any(|w| w.kind == Kind::Tracked && (unknown_view || w.on(desk)))
    }

    /// A candidate is left open (timeout, folding switched off): it takes
    /// its first-signal position and, once scanned, is the most recently
    /// activated window.
    fn left_open(&mut self, win: WinKey) {
        self.order.candidate_left_open(win);
        self.win_mut(win).kind = Kind::Tracked;
        self.activation.insert(0, win);
    }

    fn apply(mut self, tr: &Tr) -> Model {
        match tr {
            Tr::Open { desk, pinned, unknown_view, bypass, pid_group } => {
                let win = key(self.next);
                self.next += 1;
                let candidate = self.is_candidate(*desk, *unknown_view, *bypass);
                if candidate {
                    self.order.candidate_appeared(win);
                } else {
                    self.order.opened(win);
                    self.activation.insert(0, win);
                }
                self.wins.push(Win {
                    key: win,
                    pid: 1000 + u32::from(*pid_group),
                    desk: *desk,
                    pinned: *pinned,
                    kind: if candidate { Kind::Candidate } else { Kind::Tracked },
                });
            }
            Tr::Activate(_) => {
                if let Some(win) = self.pick(tr) {
                    self.activation.retain(|w| *w != win);
                    self.activation.insert(0, win);
                }
            }
            Tr::Close(_) => {
                if let Some(win) = self.pick(tr) {
                    self.wins.retain(|w| w.key != win);
                    self.activation.retain(|w| *w != win);
                    self.order.removed(win);
                }
            }
            Tr::Timeout(_) => {
                if let Some(win) = self.pick(tr) {
                    self.left_open(win);
                }
            }
            Tr::ProcessExit(i) => {
                if let Some(pid) = self.pick_pid(i) {
                    let exiting: Vec<WinKey> =
                        self.candidates().into_iter().filter(|w| self.win(*w).pid == pid).collect();
                    for win in exiting {
                        self.wins.retain(|w| w.key != win);
                        self.order.removed(win);
                    }
                }
            }
            Tr::ToggleFolding => {
                if self.folding {
                    // Every candidate in progress is left open (Req 12.10).
                    for win in self.candidates() {
                        self.left_open(win);
                    }
                }
                self.folding = !self.folding;
            }
            Tr::ShellLost => {
                // The candidates are revealed and keep their place in the
                // order; the tracker does not know them.
                for win in self.candidates() {
                    self.order.candidate_left_open(win);
                    self.win_mut(win).kind = Kind::Unscanned;
                }
                self.shell_up = false;
            }
            Tr::ShellAvailable => {
                for win in self.of_kind(Kind::Unscanned) {
                    self.win_mut(win).kind = Kind::Tracked;
                    self.activation.insert(0, win);
                }
                self.order.tracking_started(&self.activation);
                self.shell_up = true;
            }
            Tr::Restart => {
                let mut activation = self.activation.clone();
                activation.extend(self.candidates());
                for w in &mut self.wins {
                    w.kind = Kind::Tracked;
                }
                self.order.tracking_started(&activation);
                self.activation = activation;
            }
        }
        self
    }

    fn precondition(&self, tr: &Tr) -> bool {
        match tr {
            Tr::ShellAvailable => !self.shell_up,
            _ if !self.shell_up => false,
            Tr::Open { .. } | Tr::ToggleFolding | Tr::ShellLost | Tr::Restart => true,
            Tr::Activate(_) => !self.tracked().is_empty(),
            Tr::Close(_) => self.can_close(),
            Tr::Timeout(_) => !self.candidates().is_empty(),
            Tr::ProcessExit(_) => self.can_exit(),
        }
    }

    /// The windows that may be closed. The last tracked window is never
    /// closed: the next window the tracker sees would be a Trigger_Window
    /// candidate (a Pending_Snapshot exists), and the windows opened while it
    /// waits are not Folding_Candidates (Req 14.3 c, d), which task 22.7 and
    /// the restoration tests cover.
    fn closable(&self) -> Vec<WinKey> {
        let only_tracked = self.tracked().len() == 1;
        self.wins.iter().filter(|w| !(only_tracked && w.kind == Kind::Tracked)).map(|w| w.key).collect()
    }

    fn can_close(&self) -> bool {
        !self.closable().is_empty()
    }

    /// Whether the Explorer process of some candidates may exit: a window
    /// stays open.
    fn can_exit(&self) -> bool {
        !self.exitable_pids().is_empty() && self.wins.iter().any(|w| w.kind != Kind::Candidate)
    }
}

/// A choice among the candidates a transition may name (taken modulo their
/// number).
#[derive(Clone, Copy, Debug)]
struct Pick(u16);

fn pick_of<T: Copy>(items: &[T], i: &Pick) -> Option<T> {
    if items.is_empty() { None } else { Some(items[usize::from(i.0) % items.len()]) }
}

struct OpeningOrderModel;

impl ReferenceStateMachine for OpeningOrderModel {
    type State = Model;
    type Transition = Tr;

    fn init_state() -> BoxedStrategy<Model> {
        let windows = (0usize..=4)
            .prop_flat_map(|n| {
                (
                    prop::collection::vec((0..DESKTOPS, prop::bool::weighted(0.25)), n),
                    Just((0..n).collect::<Vec<usize>>()).prop_shuffle(),
                )
            })
            .prop_map(|(wins, activation)| Start::Windows { wins, activation });
        let restoration = (1u32..=3)
            .prop_flat_map(|k| Just((1..=k).collect::<Vec<u32>>()).prop_shuffle())
            .prop_map(|activation| Start::Restoration { activation });
        prop_oneof![3 => windows, 2 => restoration].prop_map(Model::start).boxed()
    }

    fn transitions(state: &Model) -> BoxedStrategy<Tr> {
        if !state.shell_up {
            return Just(Tr::ShellAvailable).boxed();
        }
        let mut choices: Vec<(u32, BoxedStrategy<Tr>)> = vec![
            (
                8,
                (
                    0..DESKTOPS,
                    prop::bool::weighted(0.25),
                    prop::bool::weighted(0.2),
                    prop::bool::weighted(0.15),
                    0u8..3,
                )
                    .prop_map(|(desk, pinned, unknown_view, bypass, pid_group)| Tr::Open {
                        desk,
                        pinned,
                        unknown_view,
                        bypass,
                        pid_group,
                    })
                    .boxed(),
            ),
            (2, Just(Tr::ToggleFolding).boxed()),
            (2, Just(Tr::ShellLost).boxed()),
            (1, Just(Tr::Restart).boxed()),
        ];
        if !state.tracked().is_empty() {
            choices.push((3, (0u16..1000).prop_map(Pick).prop_map(Tr::Activate).boxed()));
        }
        if state.can_close() {
            choices.push((4, (0u16..1000).prop_map(Pick).prop_map(Tr::Close).boxed()));
        }
        if !state.candidates().is_empty() {
            choices.push((3, (0u16..1000).prop_map(Pick).prop_map(Tr::Timeout).boxed()));
        }
        if state.can_exit() {
            choices.push((2, (0u16..1000).prop_map(Pick).prop_map(Tr::ProcessExit).boxed()));
        }
        Union::new_weighted(choices).boxed()
    }

    fn preconditions(state: &Model, transition: &Tr) -> bool {
        state.precondition(transition)
    }

    fn apply(state: Model, transition: &Tr) -> Model {
        state.apply(transition)
    }
}

// ---------------------------------------------------------------------------
// System under test
// ---------------------------------------------------------------------------

struct Sut {
    engine: Engine,
    now: u64,
    /// The first-navigation timer of every candidate in progress.
    timers: BTreeMap<WinKey, (TimerId, Millis)>,
    /// The model before the transition that is being applied.
    before: Model,
}

impl Sut {
    fn tick(&mut self) -> Millis {
        self.now += 1;
        Millis(self.now)
    }

    fn send(&mut self, event: Event) -> Vec<Command> {
        let now = self.tick();
        self.engine.handle(now, event)
    }

    /// The adapter's report of an open window: its scan and its activation.
    fn scan_and_activate(&mut self, win: WinKey, pid: u32) {
        self.send(scan(win, pid));
        self.send(Event::WindowActivated { win });
    }

    fn pid_of(&self, win: WinKey) -> u32 {
        self.before.wins.iter().find(|w| w.key == win).map_or(1, |w| w.pid)
    }

    /// Starts an engine that scans `wins` and activates them so that
    /// `activation` is the Activation_Order, then reports the shell ready.
    fn start(&mut self, folding: bool, wins: &[(WinKey, u32)], activation: &[WinKey]) {
        self.engine = engine_with(folding);
        self.timers.clear();
        for &(win, pid) in wins {
            self.send(scan(win, pid));
        }
        for &win in activation.iter().rev() {
            self.send(Event::WindowActivated { win });
        }
        self.send(Event::ShellAvailable);
    }
}

fn expected_plan(model: &Model, view: &DesktopView) -> FoldPlan {
    let in_view = |win: WinKey| match view {
        DesktopView::Known(on) => on.contains(&win),
        DesktopView::Unknown => true,
    };
    let tracked = model.tracked();
    match model.order.order().into_iter().find(|w| tracked.contains(w) && in_view(*w)) {
        Some(target) => FoldPlan::Fold { target, reason: None },
        None => FoldPlan::NoTarget,
    }
}

struct OpeningOrderTest;

impl StateMachineTest for OpeningOrderTest {
    type SystemUnderTest = Sut;
    type Reference = OpeningOrderModel;

    fn init_test(model: &Model) -> Sut {
        let mut sut = match &model.start {
            Start::Windows { .. } => {
                let mut sut = Sut { engine: engine_with(true), now: 1_000, timers: BTreeMap::new(), before: model.clone() };
                let wins: Vec<(WinKey, u32)> = model.wins.iter().map(|w| (w.key, w.pid)).collect();
                sut.start(true, &wins, &model.activation);
                sut
            }
            Start::Restoration { activation } => {
                let (engine, facts, now) = run_restoration(activation);
                assert_eq!(
                    facts.announced,
                    model.wins.iter().map(|w| w.key).collect::<Vec<_>>(),
                    "the restoration is not deterministic"
                );
                Sut { engine, now: now.0, timers: BTreeMap::new(), before: model.clone() }
            }
        };
        sut.before = model.clone();
        sut
    }

    fn apply(mut sut: Sut, model: &Model, tr: Tr) -> Sut {
        let before = sut.before.clone();
        match &tr {
            Tr::Open { desk, unknown_view, bypass, .. } => {
                let new = model.wins.last().expect("the opened window");
                let view = before.view(*desk, *unknown_view);
                let commands = sut.send(Event::WindowAppeared {
                    win: new.key,
                    pid: new.pid,
                    bypass_keys: *bypass,
                    desktop: view,
                    foreground: None,
                });
                if new.kind == Kind::Candidate {
                    assert!(
                        commands.contains(&Command::HideWindow { win: new.key }),
                        "{tr:?} is a candidate for the model, not for the engine: {commands:?}; tracked {:?}, model {:?}",
                        sut.engine.tracker().open_window_keys(),
                        (
                            before.wins,
                            sut.engine.folding().is_excluded(new.key),
                            sut.engine.folding().opening().to_vec(),
                            sut.engine.folding().enabled(),
                            sut.engine.store().pending_snapshot().is_some(),
                            sut.engine.restore_session().map(|s| (s.is_finished(), s.has_pending_requests()))
                        )
                    );
                    let timer = commands.iter().find_map(|c| match c {
                        Command::SetTimer { id, at } if id.owner == TimerOwner::Fold => Some((*id, *at)),
                        _ => None,
                    });
                    sut.timers.insert(new.key, timer.expect("the first-navigation timer"));
                } else {
                    assert!(commands.is_empty(), "{commands:?}");
                    sut.scan_and_activate(new.key, new.pid);
                }
            }
            Tr::Activate(_) => {
                if let Some(win) = before.pick(&tr) {
                    sut.send(Event::WindowActivated { win });
                }
            }
            Tr::Close(_) => {
                if let Some(win) = before.pick(&tr) {
                    sut.timers.remove(&win);
                    sut.send(Event::WindowDestroyed { win });
                }
            }
            Tr::Timeout(_) => {
                if let Some(win) = before.pick(&tr) {
                    let (id, at) = sut.timers.remove(&win).expect("the candidate's timer");
                    sut.now = sut.now.max(at.0.saturating_sub(1));
                    let commands = sut.send(Event::Timer(id));
                    assert!(commands.contains(&Command::RescanWindow(win)), "{commands:?}");
                    let pid = sut.pid_of(win);
                    sut.scan_and_activate(win, pid);
                }
            }
            Tr::ProcessExit(i) => {
                if let Some(pid) = before.pick_pid(i) {
                    for win in before.candidates().into_iter().filter(|w| before.win(*w).pid == pid) {
                        sut.timers.remove(&win);
                    }
                    sut.send(Event::ProcessExited { pid });
                }
            }
            Tr::ToggleFolding => {
                let settings = Settings { tab_folding_enabled: !before.folding, ..Settings::default() };
                sut.send(Event::SettingsChanged(settings));
                if before.folding {
                    for win in before.candidates() {
                        sut.timers.remove(&win);
                        let pid = sut.pid_of(win);
                        sut.scan_and_activate(win, pid);
                    }
                }
            }
            Tr::ShellLost => {
                sut.timers.clear();
                sut.send(Event::ShellLost);
            }
            Tr::ShellAvailable => {
                for win in before.of_kind(Kind::Unscanned) {
                    let pid = sut.pid_of(win);
                    sut.scan_and_activate(win, pid);
                }
                sut.send(Event::ShellAvailable);
            }
            Tr::Restart => {
                let wins: Vec<(WinKey, u32)> = before.wins.iter().map(|w| (w.key, w.pid)).collect();
                sut.start(before.folding, &wins, &model.activation);
            }
        }
        sut.before = model.clone();
        sut
    }

    fn check_invariants(sut: &Sut, model: &Model) {
        let engine = &sut.engine;
        // Req 14.8: the Opening_Order is the reference's.
        assert_eq!(engine.folding().opening().to_vec(), model.order.order(), "Opening_Order");

        // Which windows are candidates and which the tracker knows.
        assert_eq!(engine.folding().candidates_in_progress(), model.candidates().len(), "candidates");
        for w in &model.wins {
            assert_eq!(engine.folding().owns(w.key), w.kind == Kind::Candidate, "{w:?} owned by the fold");
            assert_eq!(engine.tracker().window(w.key).is_some(), w.kind == Kind::Tracked, "{w:?} tracked");
        }
        assert_eq!(engine.tracker().activation_order(), model.activation, "Activation_Order");

        // Req 14.9, 14.11, 14.12: the Primary_Window of every desktop view.
        let views = (0..DESKTOPS)
            .map(|desk| model.view(desk, false))
            .chain([DesktopView::Unknown])
            .collect::<Vec<_>>();
        let nobody = nobody_shows();
        let opening = engine.folding().opening().to_vec();
        let activation = engine.tracker().activation_order();
        let windows = engine.tracker().fold_windows();
        let unresponsive = BTreeSet::new();
        for view in &views {
            assert_eq!(
                engine.folding().opening().primary(view),
                model.order.primary(view),
                "Primary_Window in {view:?}"
            );
            let plan = select(&FoldCtx {
                candidate_key: nobody.fold_key(),
                foreground: None,
                opening: &opening,
                activation: &activation,
                windows: &windows,
                pending_folds: &[],
                desktop: view,
                unresponsive: &unresponsive,
                exclude: None,
            });
            assert_eq!(plan, expected_plan(model, view), "Fold_Target in {view:?}");
        }
    }
}

prop_state_machine! {
    #![proptest_config(Config { cases: 200, ..Config::default() })]

    /// Property 33: Opening_Order and Primary_Window per Virtual_Desktop.
    #[test]
    fn opening_order_and_primary_window_match_the_reference_model(sequential 1..=60 => OpeningOrderTest);
}

/// A directed run: a restoration, candidates left open by their timeouts,
/// a shell loss with a candidate in progress, a restart, and a window opened
/// afterwards.
#[test]
fn a_restoration_candidates_a_shell_loss_and_a_restart_match_the_model() {
    let open = |desk, pinned| Tr::Open { desk, pinned, unknown_view: false, bypass: false, pid_group: 0 };
    let model = Model::start(Start::Restoration { activation: vec![2, 3, 1] });
    let transitions = vec![
        open(0, false),
        open(1, true),
        Tr::Timeout(Pick(0)),
        open(0, false),
        Tr::ShellLost,
        Tr::ShellAvailable,
        Tr::Close(Pick(0)),
        Tr::Restart,
        open(2, false),
        Tr::ProcessExit(Pick(0)),
    ];
    OpeningOrderTest::test_sequential(Config::default(), model, transitions, None);
}