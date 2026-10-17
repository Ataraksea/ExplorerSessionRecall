// Feature: explorer-session-restore, Property 11: Restoration starts only for a real trigger, at most once
//! Model-based test of restoration triggering (task 11.7): the `Engine`
//! driven against the simulated Explorer (`SimConfig::for_restoration`)
//! through sequences of user launches (default and targeted, with an optional
//! second launch while the first is being handled), window closes, settings
//! changes, Windows restarts (a new Engine reading the Session_File the last
//! one wrote), and cancelled sign-outs with a launch while frozen.
//!
//! - **Model** ([`ReferenceStateMachine`]): the open window count, what the
//!   Session_File holds while no window is open (absent, Consumed, or Pending
//!   with its `failed_attempts` and window count), restoration enabled, and
//!   the expected totals of restorations and consumptions. A launch is a
//!   trigger exactly when no window is open; it restores exactly when a
//!   Pending_Snapshot exists, restoration is enabled, and it is not exhausted
//!   (no restoration is ever in progress at a launch: every step settles).
//! - **System under test** ([`StateMachineTest`]): every step plays its user
//!   actions and runs the driver until quiescence, then checks the attempt
//!   writes (`Pending` with `failed_attempts > 0`), the Consumed writes, the
//!   `RESTORE` entries, the trigger of the restore session, the open window
//!   counts, and that no user window other than a trigger ever received a
//!   `Navigate`, `ApplyPlacement`, `CreateTab`, `CloseTab`, or `ActivateTab`.
//!
//! With Tab_Folding (task 22.7) the user also opens File Explorer windows as
//! Folding_Candidates (`Op::OpenCandidate`: a first signal, hidden, shown
//! after the first navigation) and switches the folding option on and off.
//! The harness announces every other window with `WindowAppeared` (Fold
//! bypass keys held, so it is an ordinary window and never a candidate) just
//! before the first event that names it, as the adapters' first signal does.
//! A candidate opened while another window is open is folded away when
//! folding is on and the bypass keys are not held (the open window count is
//! unchanged), and is an ordinary, left-open window otherwise. Either way it
//! is never a Trigger_Window; opened while no window is open it is the
//! trigger of a launch. With folding off, no window that is neither a
//! Trigger_Window nor created by the Engine gets any command at all.
//!
//! **Validates: Requirements 4.1, 4.2, 4.3**

use std::collections::BTreeSet;

use esr_core::engine::{
    Command, DesktopView, EndKind, Engine, Environment, Event, Millis, SessionFileRead,
    StartupInput, TabKey, WinKey,
};
use esr_core::logfmt::entry::LogEntry;
use esr_core::model::{
    DisplayRef, LocationId, MAX_FAILED_ATTEMPTS, Placement, Rect, SaveCause, SessionDoc, ShowState,
    Snapshot, Status, TabRec, Timestamp, WindowId, WindowRec,
};
use esr_core::settings::Settings;
use esr_sim::explorer::{
    CloseOrder, Op, Pick, ProcessChoice, SimConfig, SimExplorer, default_displays,
    default_locations,
};
use esr_sim::fold::DesktopSpec;
use esr_sim::restore::{Driver, Launch, Origin, Stop, UserAction, home_location};
use proptest::prelude::*;
use proptest::sample::select;
use proptest::test_runner::Config;
use proptest_state_machine::{ReferenceStateMachine, StateMachineTest, prop_state_machine};

/// Locations every window opens on: always available, never search results.
const POOL: [usize; 6] = [2, 3, 4, 9, 10, 11];
/// Targets of a Targeted_Launch (`POOL` minus Home).
const TARGETS: [usize; 5] = [2, 3, 4, 9, 11];
/// Most windows a step may leave open.
const MAX_OPEN: usize = 8;
/// Most candidates folded while the same windows stay open (a window holds at
/// most 8 tabs; a restored one starts with at most 3).
const MAX_FOLDS: usize = 4;

fn loc(i: usize) -> LocationId {
    default_locations()[i].clone()
}

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

/// The Session_File while no window is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum File {
    Absent,
    Consumed,
    Pending { failed: u8, windows: usize },
}

/// An initial Session_File.
#[derive(Clone, Debug)]
enum InitialFile {
    Absent,
    Consumed(Vec<Vec<usize>>),
    Pending {
        failed: u8,
        windows: Vec<Vec<usize>>,
    },
}

#[derive(Clone, Debug)]
struct Model {
    initial: InitialFile,
    initial_enabled: bool,
    initial_folding: bool,
    /// Meaningful only while `open == 0`.
    file: File,
    enabled: bool,
    /// Tab_Folding is on (Req 12.10).
    folding: bool,
    open: usize,
    /// Candidates folded into the windows open now.
    folds: usize,
    /// Expected totals over the whole run.
    restorations: usize,
    consumptions: usize,
    /// What the last step must have done.
    last: Expect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Expect {
    Nothing,
    /// The step's first new user window is the Trigger_Window of a
    /// restoration that completed.
    Restored,
}

#[derive(Clone, Debug)]
enum Transition {
    /// The user opens File Explorer (`target: None` = Default_Launch on
    /// Home), optionally followed `delay` ms later by another launch.
    Launch {
        target: Option<usize>,
        second: Option<(u64, usize)>,
    },
    /// The user closes one window.
    Close {
        pick: u16,
    },
    SetEnabled(bool),
    SetFolding(bool),
    /// The user opens a window File Explorer shows only after its first
    /// signal (`foreground` picks the window that was in the foreground).
    Candidate {
        target: usize,
        bypass_keys: bool,
        foreground: Option<u16>,
    },
    /// Windows restarts: a new Engine starts from the Session_File.
    Reboot,
    /// A sign-out starts and is cancelled; the user launches Explorer meanwhile.
    FrozenLaunch {
        target: usize,
    },
}

impl Model {
    fn restorable(&self) -> bool {
        self.open == 0
            && self.enabled
            && matches!(self.file, File::Pending { failed, .. } if failed < MAX_FAILED_ATTEMPTS)
    }

    /// The store abandons an exhausted Pending_Snapshot at startup.
    fn startup(&mut self) {
        if let File::Pending { failed, .. } = self.file
            && failed >= MAX_FAILED_ATTEMPTS
        {
            self.file = File::Consumed;
            self.consumptions += 1;
        }
    }

    /// Open windows after launching `launches` windows from this state.
    fn open_after(&self, launches: usize) -> usize {
        match self.file {
            File::Pending { windows, .. } if self.restorable() => windows + launches - 1,
            _ => self.open + launches,
        }
    }

    /// A candidate opened now is folded away: other windows are open, folding
    /// is on, and the bypass keys are not held (Req 14.2, 14.3).
    fn folds(&self, bypass_keys: bool) -> bool {
        self.open > 0 && self.folding && !bypass_keys
    }

    /// `launches` windows open while none is: the first is a Trigger_Window
    /// (Req 4.1, 4.3, 4.8, 4.11).
    fn launched(&mut self, launches: usize) {
        if self.open == 0 {
            match self.file {
                File::Pending { .. } if self.restorable() => {
                    self.restorations += 1;
                    self.consumptions += 1;
                    self.last = Expect::Restored;
                }
                File::Pending { .. } => self.consumptions += 1,
                File::Absent | File::Consumed => {}
            }
        }
        self.open = self.open_after(launches);
    }
}

fn preconditions(m: &Model, t: &Transition) -> bool {
    match t {
        Transition::Launch { second, .. } => {
            m.open_after(1 + usize::from(second.is_some())) <= MAX_OPEN
        }
        Transition::Close { .. } => m.open > 0,
        Transition::FrozenLaunch { .. } => m.open < MAX_OPEN,
        Transition::Candidate {
            bypass_keys,
            foreground,
            ..
        } => {
            // The foreground pick is only meaningful with a window open.
            (foreground.is_none() || m.open > 0)
                && if m.folds(*bypass_keys) {
                    m.folds < MAX_FOLDS
                } else {
                    m.open_after(1) <= MAX_OPEN
                }
        }
        Transition::SetEnabled(_) | Transition::SetFolding(_) | Transition::Reboot => true,
    }
}

fn snapshot_strategy() -> impl Strategy<Value = Vec<Vec<usize>>> {
    prop::collection::vec(prop::collection::vec(select(POOL.to_vec()), 1..=3), 1..=3)
}

struct TriggerModel;

impl ReferenceStateMachine for TriggerModel {
    type State = Model;
    type Transition = Transition;

    fn init_state() -> BoxedStrategy<Model> {
        let file = prop_oneof![
            1 => Just(InitialFile::Absent),
            1 => snapshot_strategy().prop_map(InitialFile::Consumed),
            4 => (0..=MAX_FAILED_ATTEMPTS, snapshot_strategy())
                .prop_map(|(failed, windows)| InitialFile::Pending { failed, windows }),
        ];
        (file, prop::bool::weighted(0.8), prop::bool::weighted(0.6))
            .prop_map(|(initial, enabled, folding)| {
                let file = match &initial {
                    InitialFile::Absent => File::Absent,
                    InitialFile::Consumed(_) => File::Consumed,
                    InitialFile::Pending { failed, windows } => File::Pending {
                        failed: *failed,
                        windows: windows.len(),
                    },
                };
                let mut m = Model {
                    initial,
                    initial_enabled: enabled,
                    initial_folding: folding,
                    file,
                    enabled,
                    folding,
                    open: 0,
                    folds: 0,
                    restorations: 0,
                    consumptions: 0,
                    last: Expect::Nothing,
                };
                m.startup();
                m
            })
            .boxed()
    }

    fn transitions(_: &Model) -> BoxedStrategy<Transition> {
        let target = prop_oneof![1 => Just(None), 1 => select(TARGETS.to_vec()).prop_map(Some)];
        let second = prop::option::weighted(0.3, (20..1_500u64, select(TARGETS.to_vec())));
        prop_oneof![
            6 => (target, second).prop_map(|(target, second)| Transition::Launch { target, second }),
            5 => any::<u16>().prop_map(|pick| Transition::Close { pick }),
            5 => (
                select(POOL.to_vec()),
                prop::bool::weighted(0.2),
                prop::option::weighted(0.5, any::<u16>()),
            )
                .prop_map(|(target, bypass_keys, foreground)| Transition::Candidate {
                    target,
                    bypass_keys,
                    foreground,
                }),
            1 => any::<bool>().prop_map(Transition::SetEnabled),
            2 => any::<bool>().prop_map(Transition::SetFolding),
            1 => Just(Transition::Reboot),
            1 => select(TARGETS.to_vec()).prop_map(|target| Transition::FrozenLaunch { target }),
        ]
        .boxed()
    }

    fn preconditions(m: &Model, t: &Transition) -> bool {
        preconditions(m, t)
    }

    fn apply(mut m: Model, t: &Transition) -> Model {
        m.last = Expect::Nothing;
        match t {
            Transition::Launch { second, .. } => {
                if m.open == 0 {
                    m.folds = 0;
                }
                m.launched(1 + usize::from(second.is_some()));
            }
            Transition::Candidate { bypass_keys, .. } => {
                if m.folds(*bypass_keys) {
                    // Folded into an open window: it never joins the
                    // Live_Session (Req 2.6, 14.15).
                    m.folds += 1;
                } else {
                    // Left open, or the first window while none is open.
                    if m.open == 0 {
                        m.folds = 0;
                    }
                    m.launched(1);
                }
            }
            Transition::Close { .. } => {
                m.open -= 1;
                if m.open == 0 {
                    // Last window closed: its Pending_Snapshot (Req 3.1, 4.9).
                    m.file = File::Pending {
                        failed: 0,
                        windows: 1,
                    };
                    m.folds = 0;
                }
            }
            Transition::SetEnabled(enabled) => m.enabled = *enabled,
            Transition::SetFolding(folding) => m.folding = *folding,
            Transition::Reboot => {
                if m.open > 0 {
                    m.file = File::Pending {
                        failed: 0,
                        windows: m.open,
                    };
                }
                m.open = 0;
                m.folds = 0;
                m.startup();
            }
            Transition::FrozenLaunch { .. } => {
                // Not a trigger while frozen.
                if m.open == 0 {
                    m.folds = 0;
                }
                m.open += 1;
            }
        }
        m
    }
}

// ---------------------------------------------------------------------------
// System under test
// ---------------------------------------------------------------------------

fn rect(left: i32, top: i32) -> Rect {
    Rect {
        left,
        top,
        right: left + 800,
        bottom: top + 600,
    }
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
                    display: DisplayRef {
                        device: d.device.clone(),
                        monitor: d.monitor,
                    },
                },
            }
        })
        .collect();
    let activation = windows.iter().map(|w| w.id).collect();
    SessionDoc {
        version: esr_core::format::FORMAT_VERSION,
        status,
        failed_attempts,
        saved_at: Timestamp {
            year: 2025,
            month: 3,
            day: 4,
            hour: 5,
            minute: 6,
            second: 7,
            millis: 8,
            offset_minutes: 0,
        },
        cause: SaveCause::LastWindowClosed,
        snapshot: Snapshot {
            windows,
            activation,
        },
    }
}

fn launch_of(target: Option<usize>, left: i32) -> Launch {
    Launch {
        location: target.map_or_else(home_location, loc),
        selection: vec![],
        rect: rect(left, 300),
        maximized: false,
    }
}

fn sim() -> SimExplorer {
    let mut config = SimConfig::for_restoration();
    config.max_windows = MAX_OPEN + 4;
    // The windows a user opens during a restoration never claim its requests
    // (the documented ambiguity of Req 4.2 is not what is tested here).
    config.restore.claim_user_windows = false;
    SimExplorer::new(config)
}

/// Counts from the command logs of the Engines before the current one.
#[derive(Clone, Copy, Debug, Default)]
struct Totals {
    attempts: usize,
    consumed: usize,
    restores: usize,
}

impl Totals {
    fn of(sim: &SimExplorer) -> Totals {
        let docs: Vec<SessionDoc> = sim
            .command_log()
            .iter()
            .filter_map(|c| match &c.command {
                Command::WriteSessionFile { text, .. } => {
                    Some(esr_core::format::parse(text.as_bytes()).expect("parses"))
                }
                _ => None,
            })
            .collect();
        Totals {
            attempts: docs
                .iter()
                .filter(|d| d.status == Status::Pending && d.failed_attempts > 0)
                .count(),
            consumed: docs.iter().filter(|d| d.status == Status::Consumed).count(),
            restores: sim
                .log_entries()
                .iter()
                .filter(|(_, e)| matches!(e, LogEntry::Restore { .. }))
                .count(),
        }
    }

    fn add(self, o: Totals) -> Totals {
        Totals {
            attempts: self.attempts + o.attempts,
            consumed: self.consumed + o.consumed,
            restores: self.restores + o.restores,
        }
    }
}

/// The window the harness announces with `WindowAppeared` before the first
/// event that names it. The Fold_Bypass_Keys are held, so it is an ordinary
/// window and never a candidate (the simulator announces its candidates
/// itself).
fn appeared(win: WinKey) -> Event {
    Event::WindowAppeared {
        win,
        pid: 1,
        bypass_keys: true,
        desktop: DesktopView::Unknown,
        foreground: None,
    }
}

fn settings_of(enabled: bool, folding: bool) -> Settings {
    Settings {
        restoration_enabled: enabled,
        tab_folding_enabled: folding,
        ..Settings::default()
    }
}

struct Sut {
    engine: Engine,
    driver: Driver,
    /// The Session_File this Engine started from.
    disk: Option<Vec<u8>>,
    enabled: bool,
    folding: bool,
    past: Totals,
    /// Windows the Engine has been told about with `WindowAppeared`.
    announced: BTreeSet<WinKey>,
    /// Trigger_Windows of this Engine's restorations.
    triggers: BTreeSet<WinKey>,
    /// Candidates first seen while another window was open.
    late_candidates: BTreeSet<WinKey>,
    /// Tabs ever seen in user windows that are not Trigger_Windows.
    untouchable_tabs: BTreeSet<TabKey>,
    /// Number of the sim's window records before the current step.
    records_before: usize,
    /// Commands of the simulator's log already checked.
    commands_checked: usize,
    /// The step may legitimately command windows to fold a candidate.
    fold_expected: bool,
    /// Tab_Folding was on at the start of the step.
    folding_before: bool,
}

impl Sut {
    fn boot(disk: Option<Vec<u8>>, enabled: bool, folding: bool, past: Totals) -> Sut {
        let startup = StartupInput {
            session_file: disk
                .clone()
                .map_or(SessionFileRead::Absent, SessionFileRead::Content),
            environment: Environment {
                displays: default_displays(),
                ..Environment::default()
            },
            background_logon: false,
            logon_at: None,
            shell_ready_at: Some(Millis(0)),
        };
        let (engine, commands) = Engine::new(settings_of(enabled, folding), startup);
        let mut driver = Driver::new(sim());
        driver.submit(commands);
        let mut sut = Sut {
            engine,
            driver,
            disk,
            enabled,
            folding,
            past,
            announced: BTreeSet::new(),
            triggers: BTreeSet::new(),
            late_candidates: BTreeSet::new(),
            untouchable_tabs: BTreeSet::new(),
            records_before: 0,
            commands_checked: 0,
            fold_expected: false,
            folding_before: folding,
        };
        sut.settle();
        sut
    }

    fn settle(&mut self) {
        let Sut {
            engine,
            driver,
            announced,
            ..
        } = self;
        driver.set_deadline(driver.sim().now() + 600_000);
        let stop = driver.run(|now, event| {
            let first_event_of = match &event {
                Event::WindowAppeared { win, .. } => {
                    announced.insert(*win);
                    None
                }
                Event::WindowScanned(obs) => Some(obs.win),
                Event::WindowOpened { win, .. } | Event::InitialLocationKnown { win, .. } => {
                    Some(*win)
                }
                _ => None,
            };
            let mut out = Vec::new();
            if let Some(win) = first_event_of.filter(|w| announced.insert(*w)) {
                out.extend(engine.handle(now, appeared(win)));
            }
            out.extend(engine.handle(now, event));
            out
        });
        assert_eq!(stop, Stop::Quiescent);
    }

    fn later(&self, ms: u64) -> Millis {
        self.driver.sim().now() + ms
    }

    fn user_windows_since(&self, from: usize) -> Vec<WinKey> {
        self.driver.sim().window_records()[from..]
            .iter()
            .filter(|r| matches!(r.origin, Origin::User))
            .map(|r| r.key)
            .collect()
    }

    fn step(&mut self, m: &Model, t: Transition) {
        self.records_before = self.driver.sim().window_records().len();
        self.folding_before = self.folding;
        self.fold_expected = false;
        match t {
            Transition::Launch { target, second } => {
                let at = self.later(1_000);
                self.driver
                    .schedule(at, UserAction::Launch(launch_of(target, 300)));
                if let Some((delay, l)) = second {
                    self.driver
                        .schedule(at + delay, UserAction::Launch(launch_of(Some(l), 600)));
                }
                self.settle();
            }
            Transition::Candidate {
                target,
                bypass_keys,
                foreground,
            } => {
                let open_before = self.driver.sim().windows().len();
                let candidates_before = self.driver.sim().candidates().len();
                self.fold_expected = open_before > 0 && self.folding && !bypass_keys;
                let at = self.later(1_000);
                self.driver.schedule(
                    at,
                    UserAction::Op(Op::OpenCandidate {
                        location: Pick(target as u16),
                        selection: vec![],
                        bypass_keys,
                        desktop: DesktopSpec::All,
                        foreground: foreground.map(Pick),
                        process: ProcessChoice::Main,
                    }),
                );
                self.settle();
                if open_before > 0 {
                    let sim = self.driver.sim();
                    let late: Vec<WinKey> = sim.candidates()[candidates_before..]
                        .iter()
                        .map(|c| c.win)
                        .collect();
                    assert_eq!(late.len(), 1, "the candidate was opened");
                    self.late_candidates.extend(late);
                }
            }
            Transition::Close { pick } => {
                let order = CloseOrder {
                    revoke_before: 0,
                    revoke_after: true,
                    gap_ms: 0,
                    rotate: Pick(0),
                };
                let at = self.later(1_000);
                self.driver.schedule(
                    at,
                    UserAction::Op(Op::CloseWindow {
                        win: Pick(pick),
                        order,
                    }),
                );
                self.settle();
            }
            Transition::SetEnabled(enabled) => {
                self.enabled = enabled;
                let settings = settings_of(self.enabled, self.folding);
                self.driver.deliver_now(Event::SettingsChanged(settings));
                self.settle();
            }
            Transition::SetFolding(folding) => {
                self.folding = folding;
                let settings = settings_of(self.enabled, self.folding);
                self.driver.deliver_now(Event::SettingsChanged(settings));
                self.settle();
            }
            Transition::Reboot => {
                self.driver
                    .deliver_now(Event::EndSessionQuery(EndKind::Restart));
                self.settle();
                let sim = self.driver.sim();
                // The simulator holds the last write of this Engine only.
                let disk = sim
                    .session_file()
                    .map(|t| t.as_bytes().to_vec())
                    .or_else(|| self.disk.clone());
                let past = self.past.add(Totals::of(sim));
                *self = Sut::boot(disk, self.enabled, self.folding, past);
            }
            Transition::FrozenLaunch { target } => {
                self.driver
                    .deliver_now(Event::EndSessionQuery(EndKind::SignOut));
                self.settle();
                let at = self.later(500);
                self.driver
                    .schedule(at, UserAction::Launch(launch_of(Some(target), 300)));
                self.settle();
                self.driver.deliver_now(Event::EndSessionCancelled);
                self.settle();
            }
        }
        self.check_step(m);
    }

    /// The trigger of this step, and that no other user window was touched.
    fn check_step(&mut self, m: &Model) {
        let new_users = self.user_windows_since(
            self.records_before
                .min(self.driver.sim().window_records().len()),
        );
        if m.last == Expect::Restored {
            let session = self
                .engine
                .restore_session()
                .expect("a restoration started");
            assert_eq!(
                Some(&session.trigger()),
                new_users.first(),
                "the launched window is the Trigger_Window"
            );
            assert!(session.is_finished());
            for w in &new_users[1..] {
                assert!(
                    !session.owns_window(*w),
                    "a window opened during the restoration is not restored into"
                );
            }
            self.triggers.insert(session.trigger());
        }
        // A window first seen while another was open is never a trigger.
        if let Some(session) = self.engine.restore_session() {
            assert!(
                !self.late_candidates.contains(&session.trigger()),
                "a candidate opened while another window was open is a Trigger_Window"
            );
        }
        let sim = self.driver.sim();
        for w in sim.windows() {
            let user = sim
                .window_record(w.key)
                .is_some_and(|r| matches!(r.origin, Origin::User));
            if user && !self.triggers.contains(&w.key) {
                self.untouchable_tabs.extend(w.tabs.iter().map(|t| t.key));
            }
        }
        let untouchable_wins: BTreeSet<WinKey> = sim
            .window_records()
            .iter()
            .filter(|r| matches!(r.origin, Origin::User) && !self.triggers.contains(&r.key))
            .map(|r| r.key)
            .collect();
        // Folding off for the whole step: no fold command at all.
        let folding_off = !self.folding_before && !self.folding;
        let log = sim.command_log();
        let from = self.commands_checked.min(log.len());
        for c in &log[from..] {
            // Folding a candidate commands the windows it folds into
            // (Req 14.1); every other step commands none of the user's.
            let touched = !self.fold_expected
                && match &c.command {
                    Command::ApplyPlacement { win, .. }
                    | Command::CreateTab { win, .. }
                    | Command::ActivateTab { win, .. } => untouchable_wins.contains(win),
                    Command::Navigate { tab, .. } | Command::CloseTab { tab, .. } => {
                        self.untouchable_tabs.contains(tab)
                    }
                    _ => false,
                };
            assert!(
                !touched,
                "a window that is not a Trigger_Window got {:?} at {:?}",
                c.command, c.at
            );
            if folding_off {
                assert!(
                    !matches!(
                        c.command,
                        Command::HideWindow { .. }
                            | Command::CloseWindow { .. }
                            | Command::ProbeFoldContext { .. }
                    ),
                    "folding is off but {:?} was commanded at {:?}",
                    c.command,
                    c.at
                );
            }
        }
        self.commands_checked = log.len();
    }
}

struct RestoreTriggerTest;

impl StateMachineTest for RestoreTriggerTest {
    type SystemUnderTest = Sut;
    type Reference = TriggerModel;

    fn init_test(m: &Model) -> Sut {
        let disk = match &m.initial {
            InitialFile::Absent => None,
            InitialFile::Consumed(w) => Some(doc(Status::Consumed, 0, w)),
            InitialFile::Pending { failed, windows } => {
                Some(doc(Status::Pending, *failed, windows))
            }
        };
        let disk = disk.map(|d| esr_core::format::serialize(&d).into_bytes());
        Sut::boot(
            disk,
            m.initial_enabled,
            m.initial_folding,
            Totals::default(),
        )
    }

    fn apply(mut sut: Sut, m: &Model, t: Transition) -> Sut {
        sut.step(m, t);
        sut
    }

    fn check_invariants(sut: &Sut, m: &Model) {
        let totals = sut.past.add(Totals::of(sut.driver.sim()));
        // One counted attempt per restoration, none without a real trigger
        // (Req 4.1-4.3); each Pending_Snapshot is consumed once.
        assert_eq!(totals.attempts, m.restorations, "restoration attempts");
        assert_eq!(totals.restores, m.restorations, "completed restorations");
        assert_eq!(totals.consumed, m.consumptions, "consumptions");
        assert_eq!(
            sut.driver.sim().windows().len(),
            m.open,
            "open windows (simulator)"
        );
        assert_eq!(
            sut.engine.tracker().window_count(),
            m.open,
            "open windows (Live_Session)"
        );
        assert!(
            sut.engine.trigger_candidate().is_none(),
            "no candidate after settling"
        );
        assert!(
            !sut.engine.store().is_restoring(),
            "no restoration in progress after settling"
        );
        assert!(sut.engine.restore_session().is_none_or(|s| s.is_finished()));
    }
}

prop_state_machine! {
    #![proptest_config(Config { cases: 256, ..Config::default() })]

    #[test]
    fn restoration_starts_only_for_a_real_trigger_at_most_once(sequential 1..=30 => RestoreTriggerTest);
}
