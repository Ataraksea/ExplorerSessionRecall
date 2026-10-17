//! Simulated File Explorer for tracking tests: windows, tabs, PIDs,
//! navigation with or without completion, activation, reorder, drag-out/in,
//! placement changes, process termination, and shell loss, on logical time.
//!
//! [`SimExplorer`] holds the ground truth of what Explorer shows. Each
//! [`Op`] changes that truth at the current logical time and returns the core
//! [`Event`]s the real adapters would deliver, stamped with their [`Millis`]
//! ([`TimedEvent`]). It also keeps the [`Reference`] model up to date with the
//! truth, so a test can compare the tracker's `capture()` against
//! [`Reference::snapshot`] once the timers have expired.
//!
//! # Faithfulness (spikes R3, R5, UIA; see `esr-win::strategy`)
//!
//! - Every tab registers in ShellWindows on its own. A rescan lists tabs in
//!   *registration* (enumeration) order, not visual order; the visual order
//!   comes only from the UIA tab strip, whose names are display names
//!   ([`UiaNaming::DisplayNames`], build 22621) or full paths for file-system
//!   locations ([`UiaNaming::FullPaths`], build 26200).
//! - The Active_Tab is reported only through the UIA selection
//!   (`WindowObservation::active` is `None` unless
//!   [`SimConfig::report_active_key`] is set).
//! - Changes are reported as the scanner would: a change marks the window, and
//!   one `WindowScanned` follows [`SimConfig::scan_debounce_ms`] after the
//!   first mark, reading the state at that time. A scan finds no tabs that
//!   have not registered yet, but the UIA strip already shows them.
//! - A dragged tab's HWND is destroyed and recreated in the target window, so
//!   it gets a new `TabKey`: the old key is revoked at once and the new one
//!   registers [`SimConfig::drag_register_delay_ms`] later. A source window
//!   left with no tabs closes itself (revocation, then `WindowDestroyed`).
//! - Closing the last tab of a window revokes the tab, then destroys the
//!   window. Closing a window revokes some of its tabs before and optionally
//!   the rest after `WindowDestroyed` ([`CloseOrder`]); the whole burst stays
//!   within the 300 ms tombstone.
//! - A killed process's windows vanish with no revocations and no
//!   `WindowDestroyed`; `ProcessExited` follows
//!   [`SimConfig::process_exit_delay_ms`] later. Killing the shell's process
//!   also loses the shell. A separate process exits normally (with
//!   `ProcessExited`) when its last window closes.
//! - While the shell is lost, nothing that comes from `IShellWindows` is
//!   delivered (`TabRevoked`, `NavigationCompleted`, `WindowScanned`), but
//!   WinEvents are (`WindowDestroyed`, `WindowActivated`, `ProcessExited`).
//!   Restoring it emits `ShellAvailable` and rescans every open window.
//! - An overflowing strip exposes only [`SimConfig::strip_capacity`] items
//!   around the selected one.
//! - HWND values are reused (most recently freed first) once a window or tab
//!   is destroyed, and window and tab HWNDs share one value space; sequence
//!   numbers keep every key unique.
//!
//! # Simplifications that keep the reference exact
//!
//! - Operations that end windows (closing a window or its last tab, killing a
//!   process) first let pending scans, registrations, and tombstones settle,
//!   so the Live_Session the tracker holds equals the truth when the close
//!   begins. Their own event bursts are played out within the operation.
//! - Foreground changes happen only through user interaction with a window
//!   (opening, activating, clicking a tab, navigating, reordering, moving,
//!   maximizing, restoring, drag targets). Closing or minimizing the
//!   foreground window does not activate another Explorer window.
//! - Navigation by the user happens in the active tab. A started navigation
//!   that has not completed leaves the tab's Location unchanged; a dragged tab
//!   loses its pending navigation.
//! - Tabs can be dragged only between windows of the same process.
//!
//! Exactness of the tracker against the reference additionally needs match
//! names that identify locations (the default pool's names are unique), UIA
//! available, no overflow, and no unidentifiable navigations followed by
//! reorders (an unidentified tab's UIA name matches nothing).
//!
//! Restoration commands (`OpenWindow`, `CreateTab`, `Navigate`, probes,
//! writes, …) are carried out by [`SimExplorer::handle_command`]; their
//! simulation lives in [`crate::restore`].
//!
//! Requirements 2.1, 2.5, 3.3.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::RangeInclusive;

use esr_core::engine::{
    Command, Event, ItemId, Millis, ObservedLocation, TabKey, TabObservation, TabStripState, WinKey,
    WindowObservation,
};
use esr_core::model::{
    Display, DisplayRef, LocationId, LocationKind, Placement, Rect, SaveCause, SearchSpec,
    ShellKind, ShowState, Volume, WideString,
};
use proptest::collection::{vec, SizeRange};
use proptest::prelude::*;
use proptest::strategy::Union;

use crate::fold::{DesktopPlace, DesktopSpec, FoldConfig, FoldState};
use crate::reference::{RefTab, RefWindow, Reference, WindowStatus};
use crate::restore::{Action, Origin, RestoreConfig, RestoreState};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// What UIA `TabItem` names contain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UiaNaming {
    /// Display names (build 22621).
    DisplayNames,
    /// Full parsing paths for file-system and ZIP locations, display names
    /// otherwise (build 26200).
    FullPaths,
}

/// Behaviour knobs of the simulator.
#[derive(Clone, Debug)]
pub struct SimConfig {
    pub uia_naming: UiaNaming,
    /// When `false`, scans carry no tab strip (`strip: None`).
    pub uia_available: bool,
    /// Most `TabItem`s an overflowing strip exposes (`None` = all).
    pub strip_capacity: Option<usize>,
    /// Also report the active tab's key in `WindowObservation::active`
    /// (the real adapters cannot, so the default is `false`).
    pub report_active_key: bool,
    /// Reuse freed HWND values (most recently freed first).
    pub reuse_hwnds: bool,
    /// Scanner debounce (design: 100 ms).
    pub scan_debounce_ms: u64,
    /// The tracker's close-coalescing tombstone (design: 300 ms); close
    /// bursts stay inside it, and window-ending operations wait it out.
    pub tombstone_ms: u64,
    /// Delay between a process dying and `ProcessExited` (design: 200 ms).
    pub process_exit_delay_ms: u64,
    /// Delay until a dragged tab's new HWND registers (spike R5: ~200 ms).
    pub drag_register_delay_ms: u64,
    /// Delay between the last revocation of a drag-emptied source window and
    /// its `WindowDestroyed`.
    pub self_close_gap_ms: u64,
    /// UIA name of a tab whose first navigation has not completed.
    pub unnavigated_tab_name: WideString,
    /// Monitors, used to fill `Placement::display`.
    pub displays: Vec<Display>,
    /// Locations that operations pick from (must not be empty).
    pub locations: Vec<LocationId>,
    pub max_windows: usize,
    pub max_tabs_per_window: usize,
    /// Most live Explorer processes, including the shell's.
    pub max_processes: usize,
    /// Restoration behaviour (latencies, tab loss, claiming); see
    /// [`crate::restore`].
    pub restore: RestoreConfig,
    /// Candidate windows (tab folding); see [`crate::fold`].
    pub fold: FoldConfig,
}

impl SimConfig {
    /// The default configuration with the adapters' trigger reporting on:
    /// every user-opened window that claims no request is announced with
    /// `InitialLocationKnown` (see [`RestoreConfig::report_initial_location`]).
    pub fn for_restoration() -> Self {
        SimConfig {
            restore: RestoreConfig { report_initial_location: true, ..RestoreConfig::default() },
            ..SimConfig::default()
        }
    }
}

impl Default for SimConfig {
    fn default() -> Self {
        SimConfig {
            uia_naming: UiaNaming::DisplayNames,
            uia_available: true,
            strip_capacity: None,
            report_active_key: false,
            reuse_hwnds: true,
            scan_debounce_ms: 100,
            tombstone_ms: 300,
            process_exit_delay_ms: 200,
            drag_register_delay_ms: 200,
            self_close_gap_ms: 30,
            unnavigated_tab_name: "New tab".into(),
            displays: default_displays(),
            locations: default_locations(),
            max_windows: 6,
            max_tabs_per_window: 8,
            max_processes: 3,
            restore: RestoreConfig::default(),
            fold: FoldConfig::default(),
        }
    }
}

/// Two side-by-side 1920×1080 monitors, the left one primary.
pub fn default_displays() -> Vec<Display> {
    let mon = |left: i32| Rect { left, top: 0, right: left + 1920, bottom: 1080 };
    let work = |left: i32| Rect { left, top: 0, right: left + 1920, bottom: 1032 };
    vec![
        Display { device: r"\\.\DISPLAY1".into(), monitor: mon(0), work: work(0), primary: true },
        Display { device: r"\\.\DISPLAY2".into(), monitor: mon(1920), work: work(1920), primary: false },
    ]
}

/// A pool covering every location kind. Display names and parsing names are
/// pairwise distinct, so UIA names identify locations in both namings.
pub fn default_locations() -> Vec<LocationId> {
    fn l(kind: LocationKind, parse: &str, display: &str, n: u8) -> LocationId {
        LocationId {
            case_sensitive: false,
            kind,
            parsing_name: parse.into(),
            display_name: display.into(),
            pidl: vec![n, 0x14, 0x1F, n.wrapping_mul(7)],
            search: None,
        }
    }
    use LocationKind::*;
    let projects = l(FileSystem(Volume::Fixed), r"C:\Projects", "Projects", 1);
    let mut search = l(
        Search,
        r"search-ms:crumb=System.Generic.String%3Areport&crumb=location:C%3A%5CProjects",
        "Search Results in Projects",
        2,
    );
    search.search = Some(SearchSpec { query: "report".into(), scope: Box::new(projects.clone()) });
    vec![
        projects,
        search,
        l(FileSystem(Volume::Fixed), r"C:\Users\User\Documents", "Documents", 3),
        l(FileSystem(Volume::Fixed), r"C:\Users\User\Downloads", "Downloads", 4),
        l(FileSystem(Volume::Fixed), r"D:\Photos\2024", "2024", 5),
        l(FileSystem(Volume::Removable), r"E:\Backup", "Backup", 6),
        l(FileSystem(Volume::Mapped), r"Z:\Shared", "Shared", 7),
        l(FileSystem(Volume::Unc), r"\\server\share\Reports", "Reports", 8),
        l(ZipFolder, r"C:\Archives\data.zip\inner", "inner", 9),
        l(Shell(ShellKind::ThisPc), "::{20D04FE0-3AEA-1069-A2D8-08002B30309D}", "This PC", 10),
        l(Shell(ShellKind::Home), "::{F874310E-B6B7-47DC-BC84-B9E6B38F5903}", "Home", 11),
        l(Shell(ShellKind::RecycleBin), "::{645FF040-5081-101B-9F08-00AA002F954E}", "Recycle Bin", 12),
    ]
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

/// Picks an element of a collection whose size is known only when the
/// operation is applied: `self.0 % len`. Operations whose pick finds an empty
/// collection do nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Pick(pub u16);

impl Pick {
    pub fn of(self, len: usize) -> Option<usize> {
        (len > 0).then(|| usize::from(self.0) % len)
    }
}

/// Which process hosts a new window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProcessChoice {
    /// The shell's process (no-op while it is dead).
    Main,
    /// An existing separate process (no-op if there is none).
    Existing(Pick),
    /// A new separate process (Req 2.5).
    New,
}

/// How a navigation ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NavOutcome {
    /// Completes at once (`NavigationCompleted`).
    Complete,
    /// Starts but does not complete until [`Op::CompleteNavigation`].
    Pending,
    /// Completes, but no Location_Identifier can be obtained (Req 8.8).
    Unidentifiable,
}

/// Event ordering of a window close (tombstone-relevant).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CloseOrder {
    /// How many tabs are revoked before `WindowDestroyed` (`% (tabs + 1)`).
    pub revoke_before: u8,
    /// Whether the remaining tabs are revoked after `WindowDestroyed`
    /// (otherwise they are never revoked).
    pub revoke_after: bool,
    /// Gap between consecutive events, clamped so the burst fits the tombstone.
    pub gap_ms: u16,
    /// Rotation of the visual tab order giving the revocation order.
    pub rotate: Pick,
}

/// One user action or environment change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    OpenWindow { process: ProcessChoice, location: Pick, nav: NavOutcome, rect: Rect, maximized: bool },
    /// Append a selected tab (Ctrl+T, then navigate).
    OpenTab { win: Pick, location: Pick, nav: NavOutcome },
    /// Close a tab; closing a window's only tab closes the window.
    CloseTab { win: Pick, tab: Pick, destroy_gap_ms: u16 },
    CloseWindow { win: Pick, order: CloseOrder },
    /// Navigate the window's active tab.
    Navigate { win: Pick, location: Pick, nav: NavOutcome },
    /// Complete one pending navigation (any window, any tab).
    CompleteNavigation { pick: Pick, identified: bool },
    ActivateTab { win: Pick, tab: Pick },
    ActivateWindow { win: Pick },
    ReorderTab { win: Pick, from: Pick, to: Pick },
    /// Drag a tab out to form a new window (source keeps ≥ 1 tab).
    DragOut { win: Pick, tab: Pick, dx: i32, dy: i32 },
    /// Drag a tab into another window of the same process.
    DragIn { from: Pick, tab: Pick, to: Pick, position: Pick },
    MoveResize { win: Pick, rect: Rect },
    Maximize { win: Pick },
    Minimize { win: Pick },
    Restore { win: Pick },
    /// The user opens a window that File Explorer shows only after its first
    /// signal (a Folding_Candidate, Req 14.2): emits `WindowAppeared` and,
    /// after `SimConfig::fold.first_navigation_ms` (if ever), `InitialLocationKnown`.
    /// `foreground` picks the Explorer window that was in the foreground
    /// (`None`: an application was). See [`crate::fold`].
    OpenCandidate {
        location: Pick,
        selection: Vec<ItemId>,
        bypass_keys: bool,
        desktop: DesktopSpec,
        foreground: Option<Pick>,
        process: ProcessChoice,
    },
    /// Ctrl+N or "New window" in the foreground Explorer window: a candidate
    /// on the folder of that window's Active_Tab, in its process, with that
    /// window as `foreground` (no-op when the foreground is an application or
    /// the Active_Tab shows no known Location). See [`crate::fold`].
    NewWindow { bypass_keys: bool },
    /// "Duplicate tab": a copy of a window's Active_Tab next to it. Creates a
    /// tab and no window: no first signal, no `TabDetached` (Req 14.5).
    DuplicateTab { win: Pick },
    /// Drag a tab out of a window on the current Virtual_Desktop to form a
    /// new window that the adapters announce: `WindowAppeared`, and
    /// `TabDetached` (with the tab's revocation) shortly after (Req 14.3 b).
    /// Like `DragOut` otherwise.
    DragOutCandidate { win: Pick, tab: Pick, dx: i32, dy: i32 },
    /// The user switches to another Virtual_Desktop (`desktop % desktops`).
    /// The foreground window becomes an application when it is not on it.
    SwitchDesktop { desktop: u8 },
    /// Move a window to a Virtual_Desktop or show it on all desktops.
    MoveToDesktop { win: Pick, to: DesktopPlace },
    /// Whether the adapters can read Virtual_Desktop membership; unreadable
    /// membership gives `DesktopView::Unknown` (Req 14.12).
    SetDesktopMembership { readable: bool },
    /// An application other than Explorer takes the foreground.
    FocusApplication,
    /// A window stops (or starts again) answering `WM_NULL`: a `FoldContext`
    /// lists it, and what its hung thread would do waits until it answers
    /// (Req 14.13). See [`crate::fold`].
    SetUnresponsive { win: Pick, unresponsive: bool },
    /// A window has begun closing: its registered tabs are revoked at once and
    /// `WindowDestroyed` follows [`FoldConfig::closing_ms`] later (Req 14.13).
    BeginClosing { win: Pick },
    /// Change [`FoldConfig::first_navigation_ms`](crate::fold::FoldConfig) for
    /// the candidates opened from now on, so that a burst's Locations can
    /// arrive in an order other than the creation order (`None`: never).
    SetFirstNavigation { ms: Option<u64> },
    /// Terminate an Explorer process (shell's or separate) without an orderly close.
    KillProcess { pid: Pick },
    /// The shell connection breaks (`RPC_E_DISCONNECTED`).
    LoseShell,
    /// The shell is back (`TaskbarCreated`); restarts the shell's process if it died.
    RestoreShell,
    /// Let logical time pass.
    Tick { ms: u64 },
}

/// A core event and the logical time it reaches the Engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimedEvent {
    pub at: Millis,
    pub event: Event,
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// What a tab currently displays.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shown {
    /// The first navigation has not completed.
    NotNavigated,
    Known(LocationId),
    /// A completed navigation without Location_Identifier; the value is the
    /// UIA name of what is shown.
    Unidentifiable(WideString),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimTab {
    pub key: TabKey,
    /// Position in ShellWindows enumeration order; `None` until registered.
    pub registration: Option<u64>,
    pub shown: Shown,
    /// Target of a started, not yet completed navigation.
    pub pending: Option<LocationId>,
    /// Most recent identified Location (what the tracker should record).
    pub last_known: Option<LocationId>,
    /// The UIA header is stuck at [`RestoreConfig::stuck_header_name`]
    /// whatever the tab shows: the tab was created while another new-tab
    /// command was outstanding in its window (spike R9, `Pipelined`).
    pub stuck_header: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimWindow {
    pub key: WinKey,
    pub pid: u32,
    /// Visual (left-to-right) order.
    pub tabs: Vec<SimTab>,
    /// Index of the selected tab.
    pub active: usize,
    pub placement: Placement,
}

#[derive(Clone, Debug)]
pub(crate) enum Scheduled {
    Emit(Event),
    Revoke { win: WinKey, tab: TabKey },
    Scan(WinKey),
    Register(TabKey),
    /// The effect of a restoration command (see [`crate::restore`]).
    Restore(Action),
    /// The effect of a candidate command or timer (see [`crate::fold`]).
    Fold(crate::fold::Action),
}

/// Deterministic simulated Explorer.
#[derive(Clone, Debug)]
pub struct SimExplorer {
    pub(crate) config: SimConfig,
    pub(crate) now: Millis,
    /// Live windows in creation order.
    pub(crate) windows: Vec<SimWindow>,
    processes: BTreeSet<u32>,
    pub(crate) main_pid: Option<u32>,
    pub(crate) shell_lost: bool,
    pub(crate) foreground: Option<WinKey>,
    queue: BTreeMap<(Millis, u64), Scheduled>,
    queue_seq: u64,
    scan_pending: BTreeSet<WinKey>,
    last_revoke: Option<Millis>,
    next_hwnd: u64,
    free_hwnds: Vec<u64>,
    next_seq: u64,
    next_registration: u64,
    next_pid: u32,
    next_unidentified: u32,
    out: Vec<TimedEvent>,
    pub(crate) reference: Reference,
    /// Restoration bookkeeping (command log, requests, records, …).
    pub(crate) restore: RestoreState,
    /// Candidate-window bookkeeping.
    pub(crate) fold: FoldState,
}

impl SimExplorer {
    /// An Explorer with a running shell process and no windows, at time 0.
    pub fn new(config: SimConfig) -> Self {
        assert!(!config.locations.is_empty(), "SimConfig::locations must not be empty");
        let mut sim = SimExplorer {
            config,
            now: Millis(0),
            windows: Vec::new(),
            processes: BTreeSet::new(),
            main_pid: None,
            shell_lost: false,
            foreground: None,
            queue: BTreeMap::new(),
            queue_seq: 0,
            scan_pending: BTreeSet::new(),
            last_revoke: None,
            next_hwnd: 0x1_0010,
            free_hwnds: Vec::new(),
            next_seq: 1,
            next_registration: 0,
            next_pid: 4000,
            next_unidentified: 0,
            out: Vec::new(),
            reference: Reference::new(),
            restore: RestoreState::default(),
            fold: FoldState::default(),
        };
        sim.main_pid = Some(sim.spawn_process());
        sim
    }

    pub fn config(&self) -> &SimConfig {
        &self.config
    }

    pub fn now(&self) -> Millis {
        self.now
    }

    /// Live windows in creation order.
    pub fn windows(&self) -> &[SimWindow] {
        &self.windows
    }

    pub fn window(&self, key: WinKey) -> Option<&SimWindow> {
        self.windows.iter().find(|w| w.key == key)
    }

    /// Live Explorer processes.
    pub fn processes(&self) -> &BTreeSet<u32> {
        &self.processes
    }

    /// The shell's process, if it is running.
    pub fn main_pid(&self) -> Option<u32> {
        self.main_pid
    }

    pub fn shell_lost(&self) -> bool {
        self.shell_lost
    }

    pub fn foreground(&self) -> Option<WinKey> {
        self.foreground
    }

    /// The expected Live_Session.
    pub fn reference(&self) -> &Reference {
        &self.reference
    }

    /// Apply one operation at the current time. Returns the events it causes
    /// up to the (possibly advanced) current time, in delivery order.
    pub fn apply(&mut self, op: &Op) -> Vec<TimedEvent> {
        match *op {
            Op::OpenWindow { process, location, nav, rect, maximized } => {
                self.open_window(process, location, nav, rect, maximized)
            }
            Op::OpenCandidate { location, ref selection, bypass_keys, ref desktop, foreground, process } => {
                self.open_candidate(process, location, selection, bypass_keys, desktop, foreground)
            }
            Op::OpenTab { win, location, nav } => self.open_tab(win, location, nav),
            Op::CloseTab { win, tab, destroy_gap_ms } => self.close_tab(win, tab, destroy_gap_ms),
            Op::CloseWindow { win, order } => {
                if let Some(wi) = win.of(self.windows.len()) {
                    let key = self.windows[wi].key;
                    self.close_window(key, order);
                }
            }
            Op::Navigate { win, location, nav } => self.navigate(win, location, nav),
            Op::CompleteNavigation { pick, identified } => self.complete_pending(pick, identified),
            Op::ActivateTab { win, tab } => self.activate_tab(win, tab),
            Op::ActivateWindow { win } => {
                if let Some(wi) = win.of(self.windows.len()) {
                    self.activate(self.windows[wi].key);
                }
            }
            Op::ReorderTab { win, from, to } => self.reorder(win, from, to),
            Op::DragOut { win, tab, dx, dy } => self.drag_out_with(win, tab, dx, dy, false),
            Op::DragIn { from, tab, to, position } => self.drag_in(from, tab, to, position),
            Op::MoveResize { win, rect } => self.move_resize(win, rect),
            Op::Maximize { win } => self.set_state(win, |s| match s {
                ShowState::Maximized => None,
                _ => Some(ShowState::Maximized),
            }),
            Op::Minimize { win } => self.set_state(win, |s| match s {
                ShowState::Minimized { .. } => None,
                ShowState::Maximized => Some(ShowState::Minimized { restore_maximized: true }),
                ShowState::Normal => Some(ShowState::Minimized { restore_maximized: false }),
            }),
            Op::Restore { win } => self.set_state(win, |s| match s {
                ShowState::Normal => None,
                ShowState::Maximized => Some(ShowState::Normal),
                ShowState::Minimized { restore_maximized: true } => Some(ShowState::Maximized),
                ShowState::Minimized { restore_maximized: false } => Some(ShowState::Normal),
            }),
            Op::NewWindow { bypass_keys } => self.new_window(bypass_keys),
            Op::DuplicateTab { win } => self.duplicate_tab(win),
            Op::DragOutCandidate { win, tab, dx, dy } => self.drag_out_with(win, tab, dx, dy, true),
            Op::SwitchDesktop { desktop } => self.switch_desktop(desktop),
            Op::MoveToDesktop { win, to } => self.move_to_desktop(win, to),
            Op::SetDesktopMembership { readable } => self.fold.set_membership_readable(readable),
            Op::FocusApplication => self.set_foreground(None),
            Op::SetUnresponsive { win, unresponsive } => {
                if let Some(wi) = win.of(self.windows.len()) {
                    let key = self.windows[wi].key;
                    self.set_unresponsive(key, unresponsive);
                }
            }
            Op::BeginClosing { win } => self.begin_closing(win),
            Op::SetFirstNavigation { ms } => self.config.fold.first_navigation_ms = ms,
            Op::KillProcess { pid } => self.kill(pid),
            Op::LoseShell => {
                if !self.shell_lost {
                    self.shell_lost = true;
                    self.emit(Event::ShellLost);
                }
            }
            Op::RestoreShell => self.restore_shell(),
            Op::Tick { ms } => self.advance_to(self.now + ms),
        }
        self.take_events()
    }

    /// Apply every operation in turn and concatenate their events.
    pub fn apply_all<'a>(&mut self, ops: impl IntoIterator<Item = &'a Op>) -> Vec<TimedEvent> {
        ops.into_iter().flat_map(|op| self.apply(op)).collect()
    }

    /// Let `ms` of logical time pass.
    pub fn tick(&mut self, ms: u64) -> Vec<TimedEvent> {
        self.apply(&Op::Tick { ms })
    }

    /// Let every pending scan, registration, delayed event, and tombstone
    /// expire: afterwards a correct tracker's `capture()` equals
    /// [`Reference::snapshot`] (unless the shell is still lost).
    pub fn settle(&mut self) -> Vec<TimedEvent> {
        self.quiesce();
        let until = self.now + 1000;
        self.advance_to(until);
        self.take_events()
    }

    /// Restore the shell if it is lost, then [`settle`](Self::settle).
    pub fn finish(&mut self) -> Vec<TimedEvent> {
        self.restore_shell();
        let mut events = self.take_events();
        events.extend(self.settle());
        events
    }

    /// What the adapters report when the tracker starts with windows already
    /// open (Req 2.9): a scan of every window, then `WindowActivated` from
    /// the least to the most recently activated window, all at the current
    /// time. Empty while the shell is lost.
    pub fn startup_events(&mut self) -> Vec<TimedEvent> {
        if self.shell_lost {
            return Vec::new();
        }
        let keys: Vec<WinKey> = self.windows.iter().map(|w| w.key).collect();
        for key in &keys {
            if let Some(obs) = self.observe(*key) {
                self.emit(Event::WindowScanned(obs));
            }
        }
        let order: Vec<WinKey> = self
            .reference
            .windows()
            .iter()
            .filter(|w| w.status == WindowStatus::Open && keys.contains(&w.key))
            .map(|w| w.key)
            .collect();
        for key in order.into_iter().rev() {
            self.emit(Event::WindowActivated { win: key });
        }
        self.take_events()
    }

    /// Carry out an Engine command at the current time and record it with
    /// its logical timestamp ([`command_log`](Self::command_log)). Every
    /// command is accepted (returns `true`); the effects are scheduled and
    /// their events arrive with later ticks or [`step`](Self::step)s, even
    /// those with no latency. `SetTimer` is only recorded: timers belong to
    /// the driver ([`crate::restore::Driver`]). See [`crate::restore`].
    pub fn handle_command(&mut self, command: &Command) -> bool {
        self.carry_out(command);
        true
    }

    /// Schedule a (debounced) rescan of `win`, if it exists.
    pub fn request_rescan(&mut self, win: WinKey) {
        self.window_changed(win);
    }

    /// When the next scheduled item is due (never earlier than now), or
    /// `None` if nothing is scheduled.
    pub fn next_due(&self) -> Option<Millis> {
        self.queue.first_key_value().map(|(&(at, _), _)| at.max(self.now))
    }

    /// Run exactly the earliest scheduled item (advancing the clock to its
    /// time) and return the events it caused. Empty when nothing is scheduled.
    pub fn step(&mut self) -> Vec<TimedEvent> {
        if let Some((&(at, seq), _)) = self.queue.first_key_value() {
            let item = self.queue.remove(&(at, seq)).expect("present");
            self.now = self.now.max(at);
            self.execute(item);
        }
        self.take_events()
    }

    /// The observation a rescan of `win` would produce now.
    pub fn observe(&self, win: WinKey) -> Option<WindowObservation> {
        let w = self.window(win)?;
        let mut registered: Vec<&SimTab> = w.tabs.iter().filter(|t| t.registration.is_some()).collect();
        registered.sort_by_key(|t| t.registration);
        let tabs = registered
            .iter()
            .map(|t| TabObservation {
                tab: t.key,
                location: match &t.shown {
                    Shown::NotNavigated => ObservedLocation::NotNavigated,
                    Shown::Known(l) => ObservedLocation::Known(l.clone()),
                    Shown::Unidentifiable(_) => ObservedLocation::Unidentifiable,
                },
            })
            .collect();
        let active_tab = w.tabs.get(w.active);
        let active = if self.config.report_active_key {
            active_tab.filter(|t| t.registration.is_some()).map(|t| t.key)
        } else {
            None
        };
        Some(WindowObservation {
            win,
            pid: w.pid,
            tabs,
            active,
            strip: self.config.uia_available.then(|| self.strip(w)),
            placement: w.placement.clone(),
        })
    }

    // --- operations --------------------------------------------------------

    /// The process that hosts a new window, or None if there is none (the
    /// shell's process is dead, no separate process exists, or the limit is
    /// reached). `ProcessChoice::New` starts a process.
    pub(crate) fn pick_process(&mut self, process: ProcessChoice) -> Option<u32> {
        match process {
            ProcessChoice::Main => self.main_pid,
            ProcessChoice::Existing(pick) => {
                let separate: Vec<u32> =
                    self.processes.iter().copied().filter(|p| Some(*p) != self.main_pid).collect();
                pick.of(separate.len()).map(|i| separate[i])
            }
            ProcessChoice::New => {
                if self.processes.len() >= self.config.max_processes {
                    return None;
                }
                Some(self.spawn_process())
            }
        }
    }
    fn open_window(&mut self, process: ProcessChoice, location: Pick, nav: NavOutcome, rect: Rect, maximized: bool) {
        if self.windows.len() >= self.config.max_windows {
            return;
        }
        let Some(pid) = self.pick_process(process) else { return };
        let key = WinKey { hwnd: self.alloc_hwnd(), seq: self.alloc_seq() };
        let initial = self.location(location);
        let tab = self.new_tab(initial.clone(), nav);
        let state = if maximized { ShowState::Maximized } else { ShowState::Normal };
        let placement = self.placement(rect, state);
        self.windows.push(SimWindow { key, pid, tabs: vec![tab], active: 0, placement });
        self.track_window(key, Origin::User, true);
        let wi = self.windows.len() - 1;
        self.sync(wi);
        self.activate(key);
        self.window_changed(key);
        self.register(wi, 0);
        // The adapters learn the initial Location only when it is shown.
        let shown = (nav == NavOutcome::Complete).then_some(initial);
        self.announce_user_window(key, shown, Vec::new());
    }

    fn open_tab(&mut self, win: Pick, location: Pick, nav: NavOutcome) {
        let Some(wi) = win.of(self.windows.len()) else { return };
        if self.windows[wi].tabs.len() >= self.config.max_tabs_per_window {
            return;
        }
        let tab = self.new_tab(self.location(location), nav);
        let w = &mut self.windows[wi];
        w.tabs.push(tab);
        w.active = w.tabs.len() - 1;
        let (key, ti) = (w.key, w.active);
        self.sync(wi);
        self.activate(key);
        self.window_changed(key);
        self.register(wi, ti);
    }

    fn close_tab(&mut self, win: Pick, tab: Pick, destroy_gap_ms: u16) {
        let Some(wi) = win.of(self.windows.len()) else { return };
        let key = self.windows[wi].key;
        let n = self.windows[wi].tabs.len();
        if n == 1 {
            let order = CloseOrder { revoke_before: 1, revoke_after: false, gap_ms: destroy_gap_ms, rotate: Pick(0) };
            self.close_window(key, order);
            return;
        }
        let ti = tab.of(n).unwrap_or(0);
        let t = self.remove_tab(wi, ti);
        if t.registration.is_some() {
            self.revoke_now(key, t.key);
        }
        self.sync(wi);
        self.activate(key);
        self.window_changed(key);
    }

    fn close_window(&mut self, key: WinKey, order: CloseOrder) {
        self.quiesce();
        let Some(wi) = self.wi(key) else { return };
        let w = &self.windows[wi];
        let (pid, n) = (w.pid, w.tabs.len());
        let k = usize::from(order.revoke_before) % (n + 1);
        let rot = order.rotate.of(n).unwrap_or(0);
        let revocation: Vec<(TabKey, bool)> =
            (0..n).map(|i| &w.tabs[(i + rot) % n]).map(|t| (t.key, t.registration.is_some())).collect();
        let delivered_before = if self.shell_lost { 0 } else { revocation[..k].iter().filter(|t| t.1).count() };
        let cause = if n == 1 && delivered_before == 1 { SaveCause::LastTabClosed } else { SaveCause::LastWindowClosed };
        self.reference.close_window(key, cause);
        self.remove_window(wi);

        let gap = u64::from(order.gap_ms).min(self.config.tombstone_ms.saturating_sub(50) / n.max(1) as u64);
        let mut at = self.now;
        for &(tab, registered) in &revocation[..k] {
            if registered {
                self.schedule(at, Scheduled::Revoke { win: key, tab });
                at = at + gap;
            }
        }
        self.schedule(at, Scheduled::Emit(Event::WindowDestroyed { win: key }));
        if order.revoke_after {
            for &(tab, registered) in &revocation[k..] {
                if registered {
                    at = at + gap;
                    self.schedule(at, Scheduled::Revoke { win: key, tab });
                }
            }
        }
        self.advance_to(at);
        self.exit_if_idle(pid);
    }

    fn navigate(&mut self, win: Pick, location: Pick, nav: NavOutcome) {
        let Some(wi) = win.of(self.windows.len()) else { return };
        let ti = self.windows[wi].active;
        let loc = self.location(location);
        match nav {
            NavOutcome::Complete => self.complete_navigation(wi, ti, Some(loc)),
            NavOutcome::Unidentifiable => self.complete_navigation(wi, ti, None),
            NavOutcome::Pending => self.windows[wi].tabs[ti].pending = Some(loc),
        }
        self.activate(self.windows[wi].key);
    }

    fn complete_pending(&mut self, pick: Pick, identified: bool) {
        let pending: Vec<(usize, usize)> = self
            .windows
            .iter()
            .enumerate()
            .flat_map(|(wi, w)| w.tabs.iter().enumerate().filter(|(_, t)| t.pending.is_some()).map(move |(ti, _)| (wi, ti)))
            .collect();
        let Some(i) = pick.of(pending.len()) else { return };
        let (wi, ti) = pending[i];
        let loc = self.windows[wi].tabs[ti].pending.take();
        self.complete_navigation(wi, ti, loc.filter(|_| identified));
    }

    fn activate_tab(&mut self, win: Pick, tab: Pick) {
        let Some(wi) = win.of(self.windows.len()) else { return };
        let ti = tab.of(self.windows[wi].tabs.len()).unwrap_or(0);
        let key = self.windows[wi].key;
        if self.windows[wi].active != ti {
            self.windows[wi].active = ti;
            self.sync(wi);
            self.window_changed(key);
        }
        self.activate(key);
    }

    fn reorder(&mut self, win: Pick, from: Pick, to: Pick) {
        let Some(wi) = win.of(self.windows.len()) else { return };
        let w = &mut self.windows[wi];
        let n = w.tabs.len();
        let (f, t) = (from.of(n).unwrap_or(0), to.of(n).unwrap_or(0));
        let key = w.key;
        if f != t {
            let active = w.tabs[w.active].key;
            let tab = w.tabs.remove(f);
            w.tabs.insert(t, tab);
            w.active = w.tabs.iter().position(|x| x.key == active).unwrap_or(0);
            self.sync(wi);
            self.window_changed(key);
        }
        self.activate(key);
    }

    /// Drag a tab out into a new window. With `announce`, the adapters report
    /// the new window as a Folding_Candidate (`WindowAppeared`) and the
    /// destruction of the source tab window (`TabDetached`) after
    /// [`FoldConfig::detach_ms`]; the source must be on the current
    /// Virtual_Desktop and the tab must show a known Location.
    fn drag_out_with(&mut self, win: Pick, tab: Pick, dx: i32, dy: i32, announce: bool) {
        let Some(si) = win.of(self.windows.len()) else { return };
        let n = self.windows[si].tabs.len();
        if n < 2 || self.windows.len() >= self.config.max_windows {
            return;
        }
        let ti = tab.of(n).unwrap_or(0);
        let (src, pid) = (self.windows[si].key, self.windows[si].pid);
        let location = self.windows[si].tabs[ti].last_known.clone();
        if announce && (location.is_none() || !self.on_current_desktop(src)) {
            return;
        }
        if announce {
            // The user holds the source window while dragging.
            self.activate(src);
        }
        let old = self.remove_tab(si, ti);
        if old.registration.is_some() {
            if announce {
                let at = self.now + self.config.fold.detach_ms;
                self.schedule(at, Scheduled::Emit(Event::TabDetached { from: src }));
                self.schedule(at, Scheduled::Revoke { win: src, tab: old.key });
            } else {
                self.revoke_now(src, old.key);
            }
        } else if announce {
            let at = self.now + self.config.fold.detach_ms;
            self.schedule(at, Scheduled::Emit(Event::TabDetached { from: src }));
        }
        self.sync(si);
        self.window_changed(src);

        // The tab's HWND is recreated first, so it may get the old value back.
        let moved = self.recreate(old);
        let moved_key = moved.key;
        let new_key = WinKey { hwnd: self.alloc_hwnd(), seq: self.alloc_seq() };
        let base = self.windows[si].placement.restored;
        let rect = Rect { left: base.left + dx, top: base.top + dy, right: base.right + dx, bottom: base.bottom + dy };
        let placement = self.placement(rect, ShowState::Normal);
        self.windows.push(SimWindow { key: new_key, pid, tabs: vec![moved], active: 0, placement });
        self.track_window(new_key, Origin::User, !announce);
        let ni = self.windows.len() - 1;
        self.sync(ni);
        if let (true, Some(location)) = (announce, location) {
            self.announce_drag_out(new_key, moved_key, src, location);
        }
        self.activate(new_key);
        self.window_changed(new_key);
        self.schedule(self.now + self.config.drag_register_delay_ms, Scheduled::Register(moved_key));
        if announce {
            self.after_drag_registration(new_key);
        }
    }

    fn drag_in(&mut self, from: Pick, tab: Pick, to: Pick, position: Pick) {
        let Some(si) = from.of(self.windows.len()) else { return };
        let (src, pid) = (self.windows[si].key, self.windows[si].pid);
        let targets: Vec<WinKey> =
            self.windows.iter().filter(|w| w.pid == pid && w.key != src).map(|w| w.key).collect();
        let Some(i) = to.of(targets.len()) else { return };
        let dst = targets[i];
        let di = self.wi(dst).expect("target exists");
        if self.windows[di].tabs.len() >= self.config.max_tabs_per_window {
            return;
        }
        let ti = tab.of(self.windows[si].tabs.len()).unwrap_or(0);
        let old = self.remove_tab(si, ti);
        if old.registration.is_some() {
            self.revoke_now(src, old.key);
        }
        let moved = self.recreate(old);
        let moved_key = moved.key;
        let w = &mut self.windows[di];
        let pos = position.of(w.tabs.len() + 1).unwrap_or(0);
        w.tabs.insert(pos, moved);
        w.active = pos;
        self.sync(di);
        self.window_changed(dst);
        self.schedule(self.now + self.config.drag_register_delay_ms, Scheduled::Register(moved_key));

        let si = self.wi(src).expect("source exists");
        if self.windows[si].tabs.is_empty() {
            // The emptied source closes itself; the target remains, so this
            // is never the last window.
            self.reference.close_window(src, SaveCause::LastWindowClosed);
            self.remove_window(si);
            self.schedule(self.now + self.config.self_close_gap_ms, Scheduled::Emit(Event::WindowDestroyed { win: src }));
        } else {
            self.sync(si);
            self.window_changed(src);
        }
        self.activate(dst);
    }

    fn move_resize(&mut self, win: Pick, rect: Rect) {
        let Some(wi) = win.of(self.windows.len()) else { return };
        if matches!(self.windows[wi].placement.state, ShowState::Minimized { .. }) {
            return;
        }
        let key = self.windows[wi].key;
        self.windows[wi].placement = self.placement(rect, ShowState::Normal);
        self.sync(wi);
        self.window_changed(key);
        self.activate(key);
    }

    fn set_state(&mut self, win: Pick, next: impl Fn(ShowState) -> Option<ShowState>) {
        let Some(wi) = win.of(self.windows.len()) else { return };
        let Some(state) = next(self.windows[wi].placement.state) else { return };
        let key = self.windows[wi].key;
        self.windows[wi].placement.state = state;
        self.sync(wi);
        self.window_changed(key);
        if matches!(state, ShowState::Minimized { .. }) {
            if self.foreground == Some(key) {
                self.set_foreground(None);
            }
        } else {
            self.activate(key);
        }
    }

    fn kill(&mut self, pid: Pick) {
        let pids: Vec<u32> = self.processes.iter().copied().collect();
        let Some(i) = pid.of(pids.len()) else { return };
        let pid = pids[i];
        self.quiesce();
        let mut had_windows = false;
        while let Some(wi) = self.windows.iter().position(|w| w.pid == pid) {
            self.remove_window(wi);
            had_windows = true;
        }
        self.reference.process_killed(pid);
        self.processes.remove(&pid);
        if self.main_pid == Some(pid) {
            self.main_pid = None;
            if !self.shell_lost {
                self.shell_lost = true;
                self.emit(Event::ShellLost);
            }
        }
        if had_windows {
            let at = self.now + self.config.process_exit_delay_ms;
            self.schedule(at, Scheduled::Emit(Event::ProcessExited { pid }));
            self.advance_to(at);
        }
    }

    fn restore_shell(&mut self) {
        if !self.shell_lost {
            return;
        }
        if self.main_pid.is_none() {
            self.main_pid = Some(self.spawn_process());
        }
        self.shell_lost = false;
        self.emit(Event::ShellAvailable);
        let keys: Vec<WinKey> = self.windows.iter().map(|w| w.key).collect();
        for key in keys {
            self.window_changed(key);
        }
    }

    // --- helpers -----------------------------------------------------------

    pub(crate) fn emit(&mut self, event: Event) {
        self.out.push(TimedEvent { at: self.now, event });
    }

    pub(crate) fn take_events(&mut self) -> Vec<TimedEvent> {
        std::mem::take(&mut self.out)
    }

    pub(crate) fn schedule(&mut self, at: Millis, item: Scheduled) {
        self.queue.insert((at, self.queue_seq), item);
        self.queue_seq += 1;
    }

    /// Run every scheduled item due at or before `t`, then set the clock to `t`.
    fn advance_to(&mut self, t: Millis) {
        while let Some((&(at, seq), _)) = self.queue.first_key_value() {
            if at > t {
                break;
            }
            let item = self.queue.remove(&(at, seq)).expect("present");
            self.now = self.now.max(at);
            self.execute(item);
        }
        self.now = self.now.max(t);
    }

    /// Advance until nothing is scheduled and no tombstone is live.
    fn quiesce(&mut self) {
        loop {
            let mut target = self.now;
            if let Some(t) = self.last_revoke {
                target = target.max(t + self.config.tombstone_ms + 1);
            }
            if let Some((&(at, _), _)) = self.queue.first_key_value() {
                target = target.max(at);
            } else if target == self.now {
                break;
            }
            self.advance_to(target);
        }
    }

    fn execute(&mut self, item: Scheduled) {
        // The work of a hung window waits until it answers again.
        let Some(item) = self.stall_if_unresponsive(item) else { return };
        match item {
            Scheduled::Emit(event) => self.emit(event),
            Scheduled::Revoke { win, tab } => self.revoke_now(win, tab),
            Scheduled::Scan(win) => {
                self.scan_pending.remove(&win);
                if !self.shell_lost
                    && let Some(obs) = self.observe(win)
                {
                    self.emit(Event::WindowScanned(obs));
                }
            }
            Scheduled::Register(tab) => {
                if let Some((wi, ti)) = self.find_tab(tab) {
                    self.register(wi, ti);
                }
            }
            Scheduled::Restore(action) => self.run_action(action),
            Scheduled::Fold(action) => self.run_fold_action(action),
        }
    }

    pub(crate) fn window_changed(&mut self, win: WinKey) {
        if self.wi(win).is_some() && self.scan_pending.insert(win) {
            self.schedule(self.now + self.config.scan_debounce_ms, Scheduled::Scan(win));
        }
    }

    pub(crate) fn revoke_now(&mut self, win: WinKey, tab: TabKey) {
        if !self.shell_lost {
            self.emit(Event::TabRevoked { win, tab });
            self.last_revoke = Some(self.now);
        }
    }

    /// The tab appears in ShellWindows; its browser sink reports a completed
    /// navigation if it shows a known Location.
    pub(crate) fn register(&mut self, wi: usize, ti: usize) {
        let reg = self.next_registration;
        let w = &mut self.windows[wi];
        let t = &mut w.tabs[ti];
        if t.registration.is_some() {
            return;
        }
        t.registration = Some(reg);
        self.next_registration += 1;
        let (win, tab, shown) = (w.key, t.key, t.shown.clone());
        if let (Shown::Known(location), false) = (shown, self.shell_lost) {
            self.emit(Event::NavigationCompleted { tab, location });
        }
        self.window_changed(win);
    }

    pub(crate) fn complete_navigation(&mut self, wi: usize, ti: usize, location: Option<LocationId>) {
        let shown = match &location {
            Some(l) => Shown::Known(l.clone()),
            None => Shown::Unidentifiable(self.unidentified_name()),
        };
        let w = &mut self.windows[wi];
        let t = &mut w.tabs[ti];
        t.pending = None;
        t.shown = shown;
        if let Some(l) = &location {
            t.last_known = Some(l.clone());
        }
        let (win, tab, registered) = (w.key, t.key, t.registration.is_some());
        if let (Some(location), true, false) = (location, registered, self.shell_lost) {
            self.emit(Event::NavigationCompleted { tab, location });
        }
        self.sync(wi);
        self.window_changed(win);
    }

    /// Make `key` the foreground window.
    pub(crate) fn activate(&mut self, key: WinKey) {
        // Activating a window on another Virtual_Desktop switches to it.
        self.follow_to_desktop(key);
        if self.foreground != Some(key) {
            self.set_foreground(Some(key));
            self.emit(Event::WindowActivated { win: key });
        }
        self.reference.activate(key);
        self.restore.raise(key);
    }

    /// Mirror window `wi` into the reference model (and record placement
    /// changes for the restoration checks).
    pub(crate) fn sync(&mut self, wi: usize) {
        let w = &self.windows[wi];
        self.restore.note_placement(w.key, self.now, &w.placement);
        self.reference.update_window(RefWindow {
            key: w.key,
            pid: w.pid,
            tabs: w.tabs.iter().map(|t| RefTab { key: t.key, location: t.last_known.clone() }).collect(),
            active: w.active,
            placement: w.placement.clone(),
            status: WindowStatus::Open,
        });
    }

    pub(crate) fn remove_tab(&mut self, wi: usize, ti: usize) -> SimTab {
        let w = &mut self.windows[wi];
        let t = w.tabs.remove(ti);
        if w.tabs.is_empty() {
            w.active = 0;
        } else if ti < w.active {
            w.active -= 1;
        } else if ti == w.active {
            // Explorer shows the right neighbour, or the left one at the end.
            w.active = ti.min(w.tabs.len() - 1);
        }
        self.free_hwnd(t.key.hwnd);
        t
    }

    pub(crate) fn remove_window(&mut self, wi: usize) -> SimWindow {
        let w = self.windows.remove(wi);
        for t in &w.tabs {
            self.free_hwnd(t.key.hwnd);
        }
        self.free_hwnd(w.key.hwnd);
        if self.foreground == Some(w.key) {
            self.set_foreground(None);
        }
        self.restore.window_removed(w.key, self.now, &w.tabs);
        w
    }

    /// A separate process exits once its last window is gone.
    pub(crate) fn exit_if_idle(&mut self, pid: u32) {
        if Some(pid) != self.main_pid && self.processes.contains(&pid) && !self.windows.iter().any(|w| w.pid == pid) {
            self.processes.remove(&pid);
            self.schedule(self.now + self.config.process_exit_delay_ms, Scheduled::Emit(Event::ProcessExited { pid }));
        }
    }

    /// The dragged tab with a new HWND (and key), not yet registered.
    fn recreate(&mut self, old: SimTab) -> SimTab {
        SimTab {
            key: TabKey { hwnd: self.alloc_hwnd(), seq: self.alloc_seq() },
            registration: None,
            pending: None,
            ..old
        }
    }

    pub(crate) fn new_tab(&mut self, location: LocationId, nav: NavOutcome) -> SimTab {
        let key = TabKey { hwnd: self.alloc_hwnd(), seq: self.alloc_seq() };
        let (shown, pending, last_known) = match nav {
            NavOutcome::Complete => (Shown::Known(location.clone()), None, Some(location)),
            NavOutcome::Pending => (Shown::NotNavigated, Some(location), None),
            NavOutcome::Unidentifiable => (Shown::Unidentifiable(self.unidentified_name()), None, None),
        };
        SimTab { key, registration: None, shown, pending, last_known, stuck_header: false }
    }

    fn strip(&self, w: &SimWindow) -> TabStripState {
        let names: Vec<WideString> = w.tabs.iter().map(|t| self.uia_name(t)).collect();
        let n = names.len();
        match self.config.strip_capacity {
            Some(cap) if cap < n => {
                let cap = cap.max(1);
                let start = w.active.saturating_sub(cap / 2).min(n - cap);
                TabStripState { names: names[start..start + cap].to_vec(), selected: Some(w.active - start) }
            }
            _ => TabStripState { names, selected: (n > 0).then_some(w.active) },
        }
    }

    pub(crate) fn uia_name(&self, t: &SimTab) -> WideString {
        if t.stuck_header {
            return self.config.restore.stuck_header_name.clone();
        }
        match &t.shown {
            Shown::NotNavigated => self.config.unnavigated_tab_name.clone(),
            Shown::Unidentifiable(name) => name.clone(),
            Shown::Known(l) => match (self.config.uia_naming, l.kind) {
                (UiaNaming::FullPaths, LocationKind::FileSystem(_) | LocationKind::ZipFolder) => l.parsing_name.clone(),
                _ => l.display_name.clone(),
            },
        }
    }

    pub(crate) fn placement(&self, rect: Rect, state: ShowState) -> Placement {
        let restored = if rect.is_positive() {
            rect
        } else {
            Rect { left: rect.left, top: rect.top, right: rect.left + 800, bottom: rect.top + 600 }
        };
        let display = self
            .config
            .displays
            .iter()
            .enumerate()
            .max_by_key(|(i, d)| {
                (restored.intersect(&d.monitor).map_or(0, |r| r.area()), d.primary, std::cmp::Reverse(*i))
            })
            .map(|(_, d)| DisplayRef { device: d.device.clone(), monitor: d.monitor })
            .unwrap_or(DisplayRef { device: r"\\.\DISPLAY1".into(), monitor: restored });
        Placement { state, restored, display }
    }

    pub(crate) fn location(&self, pick: Pick) -> LocationId {
        let pool = &self.config.locations;
        pool[pick.of(pool.len()).expect("non-empty pool")].clone()
    }

    fn unidentified_name(&mut self) -> WideString {
        self.next_unidentified += 1;
        format!("Unidentified {}", self.next_unidentified).as_str().into()
    }

    pub(crate) fn wi(&self, key: WinKey) -> Option<usize> {
        self.windows.iter().position(|w| w.key == key)
    }

    pub(crate) fn find_tab(&self, key: TabKey) -> Option<(usize, usize)> {
        self.windows
            .iter()
            .enumerate()
            .find_map(|(wi, w)| w.tabs.iter().position(|t| t.key == key).map(|ti| (wi, ti)))
    }

    pub(crate) fn alloc_hwnd(&mut self) -> u64 {
        if self.config.reuse_hwnds
            && let Some(h) = self.free_hwnds.pop()
        {
            return h;
        }
        let h = self.next_hwnd;
        self.next_hwnd += 0x10;
        h
    }

    fn free_hwnd(&mut self, hwnd: u64) {
        if self.config.reuse_hwnds {
            self.free_hwnds.push(hwnd);
        }
    }

    pub(crate) fn alloc_seq(&mut self) -> u64 {
        let s = self.next_seq;
        self.next_seq += 1;
        s
    }

    fn spawn_process(&mut self) -> u32 {
        let pid = self.next_pid;
        self.next_pid += 4;
        self.processes.insert(pid);
        pid
    }
}

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

/// Which kinds of operations a generated sequence contains.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpsConfig {
    /// Windows in separate processes (Req 2.5).
    pub separate_processes: bool,
    /// Navigations that start now and complete later (or never).
    pub pending_navigation: bool,
    /// Navigations without Location_Identifier (Req 8.8).
    pub unidentifiable: bool,
    /// Process termination (Req 3.3).
    pub process_kill: bool,
    /// Shell loss and restoration (Req 1.10, 2.9).
    pub shell_loss: bool,
    /// Candidate windows and the other operations of tab folding
    /// ([`fold_op_strategy`](crate::fold::fold_op_strategy)). Off in
    /// [`all`](Self::all) and [`tracking`](Self::tracking): the tracker's own
    /// properties do not involve candidates.
    pub folding: bool,
    /// Largest `Tick`.
    pub max_tick_ms: u64,
}

impl OpsConfig {
    /// Everything the simulator supports.
    pub fn all() -> Self {
        OpsConfig {
            separate_processes: true,
            pending_navigation: true,
            unidentifiable: true,
            process_kill: true,
            shell_loss: true,
            folding: false,
            max_tick_ms: 400,
        }
    }

    /// Tracking operations mixed with the operations of tab folding.
    pub fn folding() -> Self {
        OpsConfig { folding: true, ..Self::tracking() }
    }

    /// The operations of Property 1 (no process kill, no unidentifiable
    /// navigation).
    pub fn tracking() -> Self {
        OpsConfig { unidentifiable: false, process_kill: false, ..Self::all() }
    }
}

impl Default for OpsConfig {
    fn default() -> Self {
        Self::all()
    }
}

/// Window rects across (and partly beyond) the default displays.
pub fn rect_strategy() -> impl Strategy<Value = Rect> {
    (-300i32..3400, -200i32..800, 300i32..1400, 200i32..900)
        .prop_map(|(left, top, w, h)| Rect { left, top, right: left + w, bottom: top + h })
}

fn pick() -> impl Strategy<Value = Pick> {
    any::<u16>().prop_map(Pick)
}

fn nav_strategy(cfg: &OpsConfig) -> BoxedStrategy<NavOutcome> {
    let mut arms: Vec<(u32, BoxedStrategy<NavOutcome>)> = vec![(6, Just(NavOutcome::Complete).boxed())];
    if cfg.pending_navigation {
        arms.push((2, Just(NavOutcome::Pending).boxed()));
    }
    if cfg.unidentifiable {
        arms.push((1, Just(NavOutcome::Unidentifiable).boxed()));
    }
    Union::new_weighted(arms).boxed()
}

/// One operation.
pub fn op_strategy(cfg: &OpsConfig) -> BoxedStrategy<Op> {
    let nav = nav_strategy(cfg);
    let process: BoxedStrategy<ProcessChoice> = if cfg.separate_processes {
        prop_oneof![
            4 => Just(ProcessChoice::Main),
            1 => pick().prop_map(ProcessChoice::Existing),
            1 => Just(ProcessChoice::New),
        ]
        .boxed()
    } else {
        Just(ProcessChoice::Main).boxed()
    };
    let close_order = (any::<u8>(), any::<bool>(), 0u16..=120, pick())
        .prop_map(|(revoke_before, revoke_after, gap_ms, rotate)| CloseOrder { revoke_before, revoke_after, gap_ms, rotate });
    let ticks: RangeInclusive<u64> = 0..=cfg.max_tick_ms;

    let mut arms: Vec<(u32, BoxedStrategy<Op>)> = vec![
        (
            3,
            (process, pick(), nav.clone(), rect_strategy(), prop::bool::weighted(0.2))
                .prop_map(|(process, location, nav, rect, maximized)| Op::OpenWindow { process, location, nav, rect, maximized })
                .boxed(),
        ),
        (5, (pick(), pick(), nav.clone()).prop_map(|(win, location, nav)| Op::OpenTab { win, location, nav }).boxed()),
        (
            3,
            (pick(), pick(), 0u16..=250)
                .prop_map(|(win, tab, destroy_gap_ms)| Op::CloseTab { win, tab, destroy_gap_ms })
                .boxed(),
        ),
        (2, (pick(), close_order).prop_map(|(win, order)| Op::CloseWindow { win, order }).boxed()),
        (5, (pick(), pick(), nav).prop_map(|(win, location, nav)| Op::Navigate { win, location, nav }).boxed()),
        (4, (pick(), pick()).prop_map(|(win, tab)| Op::ActivateTab { win, tab }).boxed()),
        (3, pick().prop_map(|win| Op::ActivateWindow { win }).boxed()),
        (3, (pick(), pick(), pick()).prop_map(|(win, from, to)| Op::ReorderTab { win, from, to }).boxed()),
        (
            2,
            (pick(), pick(), -200i32..=200, -200i32..=200)
                .prop_map(|(win, tab, dx, dy)| Op::DragOut { win, tab, dx, dy })
                .boxed(),
        ),
        (
            2,
            (pick(), pick(), pick(), pick())
                .prop_map(|(from, tab, to, position)| Op::DragIn { from, tab, to, position })
                .boxed(),
        ),
        (2, (pick(), rect_strategy()).prop_map(|(win, rect)| Op::MoveResize { win, rect }).boxed()),
        (1, pick().prop_map(|win| Op::Maximize { win }).boxed()),
        (1, pick().prop_map(|win| Op::Minimize { win }).boxed()),
        (1, pick().prop_map(|win| Op::Restore { win }).boxed()),
        (4, ticks.prop_map(|ms| Op::Tick { ms }).boxed()),
    ];
    if cfg.pending_navigation {
        arms.push((
            2,
            (pick(), prop::bool::weighted(if cfg.unidentifiable { 0.8 } else { 1.0 }))
                .prop_map(|(pick, identified)| Op::CompleteNavigation { pick, identified })
                .boxed(),
        ));
    }
    if cfg.process_kill {
        arms.push((1, pick().prop_map(|pid| Op::KillProcess { pid }).boxed()));
    }
    if cfg.shell_loss {
        arms.push((1, Just(Op::LoseShell).boxed()));
        arms.push((1, Just(Op::RestoreShell).boxed()));
    }
    if cfg.folding {
        arms.push((8, crate::fold::fold_op_strategy()));
    }
    Union::new_weighted(arms).boxed()
}

/// A sequence of operations. It starts with an `OpenWindow` in the shell's
/// process, so most sequences have something to act on.
pub fn ops_strategy(cfg: &OpsConfig, len: impl Into<SizeRange>) -> BoxedStrategy<Vec<Op>> {
    let first = (pick(), rect_strategy()).prop_map(|(location, rect)| Op::OpenWindow {
        process: ProcessChoice::Main,
        location,
        nav: NavOutcome::Complete,
        rect,
        maximized: false,
    });
    (first, vec(op_strategy(cfg), len))
        .prop_map(|(first, mut rest)| {
            rest.insert(0, first);
            rest
        })
        .boxed()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::*;
    use esr_core::model::validate;
    use proptest::test_runner::Config;

    fn sim() -> SimExplorer {
        SimExplorer::new(SimConfig::default())
    }

    fn rect() -> Rect {
        Rect { left: 100, top: 100, right: 900, bottom: 700 }
    }

    fn open(location: u16) -> Op {
        Op::OpenWindow { process: ProcessChoice::Main, location: Pick(location), nav: NavOutcome::Complete, rect: rect(), maximized: false }
    }

    fn tab(win: u16, location: u16) -> Op {
        Op::OpenTab { win: Pick(win), location: Pick(location), nav: NavOutcome::Complete }
    }

    fn events(ev: &[TimedEvent]) -> Vec<&Event> {
        ev.iter().map(|e| &e.event).collect()
    }

    fn scans(ev: &[TimedEvent]) -> Vec<&WindowObservation> {
        ev.iter()
            .filter_map(|e| match &e.event {
                Event::WindowScanned(o) => Some(o),
                _ => None,
            })
            .collect()
    }

    fn display(l: &ObservedLocation) -> String {
        match l {
            ObservedLocation::Known(l) => l.display_name.to_string_lossy(),
            other => format!("{other:?}"),
        }
    }

    fn snapshot_names(sim: &SimExplorer) -> Vec<Vec<String>> {
        crate::reference::canonical(&sim.reference().snapshot())
            .windows
            .iter()
            .map(|w| w.tabs.iter().map(|t| t.location.display_name.to_string_lossy()).collect())
            .collect()
    }

    #[test]
    fn open_window_activates_then_scans_after_debounce() {
        let mut s = sim();
        let ev = s.apply(&open(2));
        let win = s.windows()[0].key;
        let tab = s.windows()[0].tabs[0].key;
        assert_eq!(
            events(&ev),
            vec![
                &Event::WindowActivated { win },
                &Event::NavigationCompleted { tab, location: default_locations()[2].clone() }
            ]
        );
        assert!(ev.iter().all(|e| e.at == Millis(0)));
        let ev = s.tick(100);
        let obs = scans(&ev);
        assert_eq!(obs.len(), 1);
        assert_eq!(ev[0].at, Millis(100));
        assert_eq!(obs[0].win, win);
        assert_eq!(obs[0].tabs.len(), 1);
        assert_eq!(obs[0].active, None, "active tab only via UIA selection");
        assert_eq!(obs[0].strip.as_ref().unwrap().selected, Some(0));
        assert_eq!(snapshot_names(&s), vec![vec!["Documents".to_string()]]);
    }

    #[test]
    fn scan_lists_enumeration_order_and_strip_lists_visual_order() {
        let mut s = sim();
        s.apply_all(&[open(2), tab(0, 3), tab(0, 4)]);
        s.apply(&Op::ReorderTab { win: Pick(0), from: Pick(2), to: Pick(0) });
        let ev = s.settle();
        let obs = *scans(&ev).last().unwrap();
        let enumerated: Vec<String> = obs.tabs.iter().map(|t| display(&t.location)).collect();
        assert_eq!(enumerated, ["Documents", "Downloads", "2024"]);
        let strip = obs.strip.as_ref().unwrap();
        let visual: Vec<String> = strip.names.iter().map(WideString::to_string_lossy).collect();
        assert_eq!(visual, ["2024", "Documents", "Downloads"]);
        assert_eq!(strip.selected, Some(0), "the active tab moved with the reorder");
        assert_eq!(snapshot_names(&s), vec![vec!["2024".to_string(), "Documents".into(), "Downloads".into()]]);
    }

    #[test]
    fn full_path_naming_and_overflowing_strip() {
        let config = SimConfig { uia_naming: UiaNaming::FullPaths, strip_capacity: Some(2), ..SimConfig::default() };
        let mut s = SimExplorer::new(config);
        s.apply_all(&[open(2), tab(0, 3), tab(0, 10), tab(0, 4)]);
        s.apply(&Op::ActivateTab { win: Pick(0), tab: Pick(2) });
        let ev = s.settle();
        let strip = scans(&ev).last().unwrap().strip.clone().unwrap();
        let names: Vec<String> = strip.names.iter().map(WideString::to_string_lossy).collect();
        // Four tabs, two exposed around the selected "Home" (a shell location keeps its display name).
        assert_eq!(names, [r"C:\Users\User\Downloads", "Home"]);
        assert_eq!(strip.selected, Some(1));
    }

    #[test]
    fn drag_out_revokes_old_key_and_registers_a_new_one_later() {
        let mut s = sim();
        s.apply_all(&[open(2), tab(0, 3)]);
        s.settle();
        let src = s.windows()[0].key;
        let old = s.windows()[0].tabs[1].key;
        let t0 = s.now();
        let ev = s.apply(&Op::DragOut { win: Pick(0), tab: Pick(1), dx: 40, dy: 40 });
        let new_win = s.windows()[1].key;
        let new_tab = s.windows()[1].tabs[0].key;
        assert_ne!(new_tab, old);
        assert_eq!(new_tab.hwnd, old.hwnd, "the freed HWND value is reused, the key is not");
        assert_eq!(events(&ev), vec![&Event::TabRevoked { win: src, tab: old }, &Event::WindowActivated { win: new_win }]);

        let ev = s.settle();
        let first_new_scan = scans(&ev).into_iter().find(|o| o.win == new_win).unwrap();
        assert!(first_new_scan.tabs.is_empty(), "not registered yet at the first scan");
        assert_eq!(first_new_scan.strip.as_ref().unwrap().names.len(), 1, "but the strip shows it");
        let registered = ev
            .iter()
            .find(|e| matches!(&e.event, Event::NavigationCompleted { tab, .. } if *tab == new_tab))
            .unwrap();
        assert_eq!(registered.at, t0 + s.config().drag_register_delay_ms);
        let last_new = scans(&ev).into_iter().rev().find(|o| o.win == new_win).unwrap();
        assert_eq!(last_new.tabs.iter().map(|t| t.tab).collect::<Vec<_>>(), vec![new_tab]);
        assert!(scans(&ev).iter().all(|o| o.tabs.iter().all(|t| t.tab != old)));
        assert_eq!(snapshot_names(&s), vec![vec!["Downloads".to_string()], vec!["Documents".to_string()]]);
    }

    #[test]
    fn drag_in_of_only_tab_closes_the_source() {
        let mut s = sim();
        s.apply_all(&[open(2), tab(0, 3), open(4)]);
        s.settle();
        let src = s.windows()[1].key;
        let old = s.windows()[1].tabs[0].key;
        let dst = s.windows()[0].key;
        let mut ev = s.apply(&Op::DragIn { from: Pick(1), tab: Pick(0), to: Pick(0), position: Pick(1) });
        ev.extend(s.settle());
        let revoke = ev.iter().position(|e| e.event == Event::TabRevoked { win: src, tab: old }).unwrap();
        let destroy = ev.iter().position(|e| e.event == Event::WindowDestroyed { win: src }).unwrap();
        assert!(revoke < destroy);
        assert!(ev[destroy].at.since(ev[revoke].at) < s.config().tombstone_ms);
        assert_eq!(s.windows().len(), 1);
        assert_eq!(s.windows()[0].key, dst);
        assert!(s.reference().session_ends().is_empty());
        assert_eq!(snapshot_names(&s), vec![vec!["Documents".to_string(), "2024".into(), "Downloads".into()]]);
        assert_eq!(crate::reference::canonical(&s.reference().snapshot()).windows[0].active, 1);
    }

    #[test]
    fn closing_the_last_tab_revokes_then_destroys_and_ends_the_session() {
        let mut s = sim();
        s.apply(&open(2));
        s.settle();
        let before = s.reference().snapshot();
        let win = s.windows()[0].key;
        let tab = s.windows()[0].tabs[0].key;
        let ev = s.apply(&Op::CloseTab { win: Pick(0), tab: Pick(0), destroy_gap_ms: 40 });
        assert_eq!(events(&ev), vec![&Event::TabRevoked { win, tab }, &Event::WindowDestroyed { win }]);
        assert_eq!(ev[1].at.since(ev[0].at), 40);
        let ends = s.reference().session_ends();
        assert_eq!(ends.len(), 1);
        assert_eq!(ends[0].cause, SaveCause::LastTabClosed);
        assert_eq!(ends[0].snapshot, before);
        assert_eq!(s.reference().snapshot(), Default::default());
    }

    #[test]
    fn close_window_orderings() {
        let mut s = sim();
        s.apply_all(&[open(2), tab(0, 3), tab(0, 4)]);
        s.settle();
        let win = s.windows()[0].key;
        let order = CloseOrder { revoke_before: 1, revoke_after: true, gap_ms: 1000, rotate: Pick(1) };
        let ev = s.apply(&Op::CloseWindow { win: Pick(0), order });
        let kinds: Vec<&str> = ev
            .iter()
            .map(|e| match e.event {
                Event::TabRevoked { win: w, .. } if w == win => "revoke",
                Event::WindowDestroyed { win: w } if w == win => "destroy",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["revoke", "destroy", "revoke", "revoke"]);
        let span = ev.last().unwrap().at.since(ev[0].at);
        assert!(span < s.config().tombstone_ms, "burst of {span} ms fits the tombstone");
        assert_eq!(s.reference().session_ends()[0].cause, SaveCause::LastWindowClosed);

        // A one-tab window destroyed before its tab is revoked is a window close.
        let mut s = sim();
        s.apply(&open(2));
        let order = CloseOrder { revoke_before: 0, revoke_after: true, gap_ms: 10, rotate: Pick(0) };
        let ev = s.apply(&Op::CloseWindow { win: Pick(0), order });
        assert!(matches!(ev.last().unwrap().event, Event::TabRevoked { .. }));
        assert_eq!(s.reference().session_ends()[0].cause, SaveCause::LastWindowClosed);
    }

    #[test]
    fn process_kill_produces_no_revocations_and_keeps_orphans() {
        let mut s = sim();
        s.apply_all(&[
            open(2),
            tab(0, 3),
            Op::OpenWindow { process: ProcessChoice::New, location: Pick(4), nav: NavOutcome::Complete, rect: rect(), maximized: false },
        ]);
        s.settle();
        let before = s.reference().snapshot();
        let main = s.main_pid().unwrap();
        let start = s.now();
        let ev = s.apply(&Op::KillProcess { pid: Pick(0) });
        assert_eq!(events(&ev), vec![&Event::ShellLost, &Event::ProcessExited { pid: main }]);
        assert_eq!(ev[1].at.since(start), s.config().process_exit_delay_ms);
        assert_eq!(s.windows().len(), 1);
        assert_eq!(s.reference().snapshot(), before, "orphaned windows stay");
        assert!(s.reference().session_ends().is_empty(), "a separate-process window is still open");

        // While the shell is lost nothing from ShellWindows arrives.
        let ev = s.apply_all(&[tab(0, 5), Op::Tick { ms: 500 }]);
        assert!(ev.iter().all(|e| matches!(e.event, Event::WindowActivated { .. })), "{ev:?}");
        let ev = s.apply(&Op::RestoreShell);
        assert_eq!(events(&ev), vec![&Event::ShellAvailable]);
        assert_ne!(s.main_pid(), Some(main), "the shell restarted in a new process");
        let ev = s.settle();
        assert_eq!(scans(&ev).len(), 1);
        assert_eq!(scans(&ev)[0].tabs.len(), 2);

        let other = *s.processes().iter().find(|p| Some(**p) != s.main_pid()).unwrap();
        let separate = s.processes().iter().position(|p| *p == other).unwrap() as u16;
        let ev = s.apply(&Op::KillProcess { pid: Pick(separate) });
        assert_eq!(events(&ev), vec![&Event::ProcessExited { pid: other }]);
        let ends = s.reference().session_ends();
        assert_eq!(ends.len(), 1);
        assert_eq!(ends[0].cause, SaveCause::ExplorerTerminated);
        assert_eq!(ends[0].snapshot.windows.len(), 2);
    }

    #[test]
    fn navigation_is_recorded_only_when_completed() {
        let mut s = sim();
        s.apply(&open(2));
        s.settle();
        s.apply(&Op::Navigate { win: Pick(0), location: Pick(3), nav: NavOutcome::Pending });
        let ev = s.settle();
        assert!(ev.is_empty(), "a started navigation reports nothing");
        assert_eq!(snapshot_names(&s), vec![vec!["Documents".to_string()]]);

        s.apply(&Op::CompleteNavigation { pick: Pick(0), identified: true });
        let ev = s.settle();
        assert_eq!(display(&scans(&ev)[0].tabs[0].location), "Downloads");
        assert_eq!(snapshot_names(&s), vec![vec!["Downloads".to_string()]]);

        let ev = s.apply(&Op::Navigate { win: Pick(0), location: Pick(4), nav: NavOutcome::Unidentifiable });
        assert!(ev.is_empty());
        let ev = s.settle();
        assert_eq!(scans(&ev)[0].tabs[0].location, ObservedLocation::Unidentifiable);
        assert_eq!(snapshot_names(&s), vec![vec!["Downloads".to_string()]], "last identified location kept");

        // A tab that never navigated is not captured.
        s.apply(&Op::OpenTab { win: Pick(0), location: Pick(5), nav: NavOutcome::Pending });
        let ev = s.settle();
        assert_eq!(scans(&ev)[0].tabs[1].location, ObservedLocation::NotNavigated);
        assert_eq!(snapshot_names(&s), vec![vec!["Downloads".to_string()]]);
    }

    #[test]
    fn hwnd_values_are_reused_but_keys_stay_unique() {
        let mut s = sim();
        s.apply(&open(2));
        let first = s.windows()[0].key;
        s.apply(&Op::CloseWindow { win: Pick(0), order: CloseOrder { revoke_before: 1, revoke_after: false, gap_ms: 0, rotate: Pick(0) } });
        s.apply(&open(3));
        let second = s.windows()[0].key;
        assert_eq!(first.hwnd, second.hwnd);
        assert_ne!(first, second);
    }

    #[test]
    fn startup_events_scan_then_activate_least_recent_first() {
        let mut s = sim();
        s.apply_all(&[open(2), open(3), open(4), Op::ActivateWindow { win: Pick(0) }]);
        let keys: Vec<WinKey> = s.windows().iter().map(|w| w.key).collect();
        let ev = s.startup_events();
        assert_eq!(scans(&ev).len(), 3);
        let activated: Vec<WinKey> = ev
            .iter()
            .filter_map(|e| match e.event {
                Event::WindowActivated { win } => Some(win),
                _ => None,
            })
            .collect();
        assert_eq!(activated, vec![keys[1], keys[2], keys[0]]);
    }

    #[test]
    fn rescan_command_is_served() {
        let mut s = sim();
        s.apply(&open(2));
        s.settle();
        let win = s.windows()[0].key;
        assert!(s.handle_command(&Command::RescanWindow(win)));
        // Focusing the foreground window changes nothing observable.
        assert!(s.handle_command(&Command::Focus(win)));
        assert_eq!(scans(&s.tick(100)).len(), 1);
    }

    /// Checks invariants every event stream must satisfy.
    #[derive(Default)]
    struct Checker {
        last_at: Millis,
        destroyed: HashSet<WinKey>,
        revoked: HashSet<TabKey>,
        tab_owner: HashMap<TabKey, WinKey>,
        shell_lost: bool,
    }

    impl Checker {
        fn check(&mut self, events: &[TimedEvent]) -> Result<(), TestCaseError> {
            for e in events {
                prop_assert!(e.at >= self.last_at, "time went backwards: {:?}", e);
                self.last_at = e.at;
                match &e.event {
                    Event::WindowScanned(o) => {
                        prop_assert!(!self.shell_lost, "scan while the shell is lost");
                        prop_assert!(!self.destroyed.contains(&o.win), "scan after destroy");
                        let keys: HashSet<TabKey> = o.tabs.iter().map(|t| t.tab).collect();
                        prop_assert_eq!(keys.len(), o.tabs.len(), "duplicate tab key in a scan");
                        for t in &o.tabs {
                            prop_assert!(!self.revoked.contains(&t.tab), "revoked tab scanned again");
                            let owner = *self.tab_owner.entry(t.tab).or_insert(o.win);
                            prop_assert_eq!(owner, o.win, "a tab key moved between windows");
                        }
                        prop_assert!(o.placement.restored.is_positive());
                        let strip = o.strip.as_ref().unwrap();
                        prop_assert!(strip.names.len() >= o.tabs.len());
                        prop_assert!(strip.selected.is_some_and(|i| i < strip.names.len()));
                    }
                    Event::TabRevoked { win, tab } => {
                        prop_assert!(!self.shell_lost, "revocation while the shell is lost");
                        prop_assert!(self.revoked.insert(*tab), "tab revoked twice");
                        if let Some(owner) = self.tab_owner.get(tab) {
                            prop_assert_eq!(owner, win);
                        }
                    }
                    Event::NavigationCompleted { tab, .. } => {
                        prop_assert!(!self.shell_lost, "navigation event while the shell is lost");
                        prop_assert!(!self.revoked.contains(tab));
                    }
                    Event::WindowDestroyed { win } => {
                        prop_assert!(self.destroyed.insert(*win), "window destroyed twice");
                    }
                    Event::WindowActivated { win } => {
                        prop_assert!(!self.destroyed.contains(win), "destroyed window activated");
                    }
                    Event::ShellLost => {
                        prop_assert!(!self.shell_lost);
                        self.shell_lost = true;
                    }
                    Event::ShellAvailable => {
                        prop_assert!(self.shell_lost);
                        self.shell_lost = false;
                    }
                    Event::ProcessExited { .. } => {}
                    other => prop_assert!(false, "unexpected event {:?}", other),
                }
            }
            Ok(())
        }
    }

    fn check_state(s: &SimExplorer) -> Result<(), TestCaseError> {
        // Keys of everything alive are unique.
        let mut wins = HashSet::new();
        let mut tabs = HashSet::new();
        for w in s.windows() {
            prop_assert!(wins.insert(w.key));
            prop_assert!(!w.tabs.is_empty() && w.active < w.tabs.len());
            prop_assert!(s.processes().contains(&w.pid));
            for t in &w.tabs {
                prop_assert!(tabs.insert(t.key));
            }
        }
        // The reference's open windows are exactly the live windows, with the same tabs.
        let open: HashSet<WinKey> = s
            .reference()
            .windows()
            .iter()
            .filter(|w| w.status == WindowStatus::Open)
            .map(|w| w.key)
            .collect();
        prop_assert_eq!(&open, &wins);
        for w in s.windows() {
            let r = s.reference().window(w.key).unwrap();
            prop_assert_eq!(r.tabs.iter().map(|t| t.key).collect::<Vec<_>>(), w.tabs.iter().map(|t| t.key).collect::<Vec<_>>());
            prop_assert_eq!(r.active, w.active);
            prop_assert_eq!(&r.placement, &w.placement);
        }
        if let Some(fg) = s.foreground() {
            prop_assert_eq!(s.reference().windows()[0].key, fg, "foreground leads the Activation_Order");
        }
        prop_assert_eq!(validate(&s.reference().snapshot()), Ok(()));
        Ok(())
    }

    proptest! {
        #![proptest_config(Config { cases: 64, ..Config::default() })]

        #[test]
        fn operation_sequences_produce_well_formed_events(ops in ops_strategy(&OpsConfig::all(), 0..120)) {
            let mut s = sim();
            let mut checker = Checker::default();
            for op in &ops {
                // Windows a kill will take down.
                let doomed: HashSet<WinKey> = match op {
                    Op::KillProcess { pid } => {
                        let pids: Vec<u32> = s.processes().iter().copied().collect();
                        let pid = pid.of(pids.len()).map(|i| pids[i]);
                        s.windows().iter().filter(|w| Some(w.pid) == pid).map(|w| w.key).collect()
                    }
                    _ => HashSet::new(),
                };
                let ev = s.apply(op);
                checker.check(&ev)?;
                check_state(&s)?;
                // A killed window vanishes without revocations or destroy.
                let orderly = ev.iter().any(|e| match e.event {
                    Event::TabRevoked { win, .. } | Event::WindowDestroyed { win } => doomed.contains(&win),
                    _ => false,
                });
                prop_assert!(!orderly, "a kill produced an orderly close: {:?}", ev);
            }
            checker.check(&s.finish())?;
            check_state(&s)?;
            prop_assert!(!s.shell_lost());
            // After settling, the last scan of every window matches the truth.
            for w in s.windows() {
                let obs = s.observe(w.key).unwrap();
                prop_assert_eq!(obs.tabs.len(), w.tabs.len(), "every tab registered after settling");
            }
        }
    }
}
