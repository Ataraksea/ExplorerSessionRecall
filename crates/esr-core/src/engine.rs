//! `Engine`, the `Event`/`Command` vocabulary, trigger detection and gating,
//! and control flows (exit, settings, Builtin_Restore_Option prompt).
//!
//! The `Engine` is the functional core: the shell feeds it [`Event`]s with a
//! monotonic [`Millis`] timestamp and carries out the [`Command`]s it returns.
//! It owns no clock, thread, or handle.
//!
//! Task 1.3 defines the vocabulary and the skeleton: [`route`] decides which
//! sub-component (tracker, store, restore session, control) sees an event, and
//! the sub-component hooks are stubs that later tasks fill (7.x tracker, 8.x
//! store, 11.2-11.4 restore session, 11.5-11.6 control).
//!
//! Requirements 2.1, 4.1-4.3, 4.8, 4.9, 10.1-10.7, 12.3-12.5, 12.8, 12.9.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Add;

use crate::fold::{FirstSignalEnv, FoldStep, FoldingEngine, Released, FOLD_OP_BASE};
use crate::launch::{is_builtin_restore, is_default_launch, BuiltinContext, LaunchToContext};
use crate::logfmt::entry::LogEntry;
use crate::model::{Display, LocationId, Placement, SaveCause, ShowState, WideString, MAX_FAILED_ATTEMPTS};
use crate::plan::defer::{DeferralConfig, IMMEDIATE_FOLDER_BUDGET};
use crate::plan::fallback::ProbeOutcome;
use crate::plan::placement::{resolve_placement, FinalPlacement};
use crate::plan::LaunchInfo;
use crate::restore::{RestoreOutcome, RestoreParams, RestoreSession};
use crate::settings::Settings;
use crate::store::logic::io_description;
use crate::store::{StoreLogic, WallClock};
use crate::tracker::LiveSessionTracker;

// ---------------------------------------------------------------------------
// Time and identifiers
// ---------------------------------------------------------------------------

/// Monotonic time in milliseconds, supplied by the shell (e.g. `GetTickCount64`).
/// The core never reads a clock itself.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Millis(pub u64);

impl Millis {
    /// Milliseconds elapsed from `earlier` to `self`, or 0 if `earlier` is later.
    pub fn since(self, earlier: Millis) -> u64 {
        self.0.saturating_sub(earlier.0)
    }
}

impl Add<u64> for Millis {
    type Output = Millis;

    fn add(self, ms: u64) -> Millis {
        Millis(self.0.saturating_add(ms))
    }
}

/// Identity of a live Explorer window: the top-level `CabinetWClass` HWND value
/// plus a creation sequence number assigned by the adapters, so that a reused
/// HWND never aliases an earlier window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WinKey {
    pub hwnd: u64,
    pub seq: u64,
}

/// Identity of a live tab: its `ShellTabWindowClass` HWND value plus a creation
/// sequence number (see [`WinKey`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TabKey {
    pub hwnd: u64,
    pub seq: u64,
}

/// Correlates an `OpenWindow` command with the `WindowOpened` event that claims it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestId(pub u64);

/// Correlates a window/tab operation command with its `OperationFailed` event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OpId(pub u64);

/// Correlates a `Probe` command with its `ProbeCompleted` event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProbeId(pub u64);

/// Correlates a `ProbeFoldContext` command with its `FoldContext` event
/// (tab folding, Req 14.9, 14.12, 14.13).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FoldProbeId(pub u64);

/// The open windows on a Folding_Candidate's Virtual_Desktop, as computed by
/// the adapter (Req 14.9, 14.11, 14.12).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DesktopView {
    /// The open windows on the candidate's Virtual_Desktop; windows shown on
    /// all desktops are included.
    Known(BTreeSet<WinKey>),
    /// Desktop membership could not be read: every open window counts as on
    /// the candidate's desktop (Req 14.12).
    Unknown,
}

/// The sub-component that owns a timer; [`route`] delivers `Timer` events to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TimerOwner {
    Tracker,
    Store,
    Restore,
    Control,
    /// The `FoldingEngine` (tab folding, Req 14): first-navigation (14.14),
    /// tab-creation and tab-show (14.13), Tab_Switch (14.23) and close-retry
    /// (14.1) timers. The `seq` is chosen by the folding code.
    Fold,
}

/// A timer set with `Command::SetTimer`. `seq` is chosen by the owner; setting
/// a timer with an id that is already pending replaces it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TimerId {
    pub owner: TimerOwner,
    pub seq: u64,
}

/// One selected item of a Targeted_Launch, as absolute ITEMIDLIST bytes
/// (captured with `IFolderView2::GetSelection`, Req 6.4).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ItemId(pub Vec<u8>);

// ---------------------------------------------------------------------------
// Event payloads
// ---------------------------------------------------------------------------

/// The Location a scanned tab currently shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObservedLocation {
    /// No navigation has completed in this tab yet (Req 2.2, 8.1).
    NotNavigated,
    /// The tab shows this Location.
    Known(LocationId),
    /// Navigation completed but no Location_Identifier could be obtained (Req 8.8).
    Unidentifiable,
}

/// One tab found by a window rescan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabObservation {
    pub tab: TabKey,
    pub location: ObservedLocation,
}

/// The tab strip as read through UI Automation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabStripState {
    /// `TabItem` names in left-to-right order.
    pub names: Vec<WideString>,
    /// Index of the selected `TabItem`, if UIA reported one.
    pub selected: Option<usize>,
}

/// Result of a full rescan of one Explorer window.
///
/// The active tab is carried explicitly in `active`. Which signal it comes from
/// (UIA selection, `QueryActiveShellView`, â€¦) is the adapter's choice; tab
/// visibility is not a reliable source on current Windows 11 builds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowObservation {
    pub win: WinKey,
    /// Process hosting the window (Req 2.5, 3.3).
    pub pid: u32,
    /// Tabs in ShellWindows enumeration order (not display order).
    pub tabs: Vec<TabObservation>,
    /// The Active_Tab, if the adapter could determine it.
    pub active: Option<TabKey>,
    /// UIA tab strip, or `None` when UIA was unavailable; the reconciler then
    /// keeps the previous order and appends new tabs.
    pub strip: Option<TabStripState>,
    /// Window_Placement in screen coordinates.
    pub placement: Placement,
}

/// What kind of Windows session end began (from `WM_QUERYENDSESSION`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EndKind {
    SignOut,
    Shutdown,
    Restart,
}

/// The user's answer to the Builtin_Restore_Option prompt (Req 10.2, 10.3, 10.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PromptAnswer {
    Accepted,
    Declined,
    Dismissed,
}

/// A failed Shell/Win32 operation, as reported by an adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellError {
    /// HRESULT or Win32 error code (0 when not applicable, e.g. a timeout).
    pub code: i32,
    /// Human-readable description for the Activity_Log.
    pub message: String,
}

/// A failed file operation, as reported by the file store.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IoErrorKind {
    NotFound,
    AccessDenied,
    SharingViolation,
    DiskFull,
    /// Any other Win32 error code.
    Other(u32),
}

/// The values of File Explorer's own options that the core needs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ExplorerOptionValues {
    /// `Advanced\PersistBrowsers` (Builtin_Restore_Option); `None` if absent or unreadable.
    pub persist_browsers: Option<bool>,
    /// `Advanced\LaunchTo` raw DWORD; `None` if absent or unreadable.
    pub launch_to: Option<u32>,
    /// `Advanced\SeparateProcess`; `None` if absent or unreadable.
    pub separate_process: Option<bool>,
}

/// Environment readings the pure core cannot obtain itself.
///
/// Not part of design.md's event list: the planner needs the current displays
/// (`plan_restore(.., &[Display])`), launch classification needs `LaunchTo`,
/// and the BuiltinRestore heuristic needs `PersistBrowsers` and the last-input
/// time. The shell sends `EnvironmentChanged` at startup, on display or option
/// changes, and with fresh readings right before it reports a window that may
/// become a Trigger_Window.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Environment {
    pub displays: Vec<Display>,
    pub options: ExplorerOptionValues,
    /// Time of the last user input (`GetLastInputInfo`), on the [`Millis`] clock.
    pub last_input_at: Option<Millis>,
    /// Local wall-clock time at a point of the [`Millis`] clock, from which
    /// the store derives `saved-at` (the core reads no clock itself).
    pub wall_clock: Option<WallClock>,
    /// Build and known-folder paths for resolving `LaunchTo` (Default_Launch
    /// classification; `esr-win::explorer_options::launch_to_context`).
    pub launch_to_context: LaunchToContext,
    /// Directory for the placeholders of deferred tabs (Req 5.13), an
    /// absolute path; `None` turns deferral off (every tab is opened at
    /// once). Addition for task 16.3.
    pub placeholder_dir: Option<WideString>,
}

/// The Session_File as read at startup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionFileRead {
    Absent,
    /// The file exists but could not be read (Req 11.10).
    Unreadable(IoErrorKind),
    /// Raw bytes, not yet parsed.
    Content(Vec<u8>),
}

/// Everything `Engine::new` needs besides `Settings`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartupInput {
    pub session_file: SessionFileRead,
    pub environment: Environment,
    /// Started with `--background` by the logon task.
    pub background_logon: bool,
    /// Sign-in time on the [`Millis`] clock, if known (BuiltinRestore heuristic).
    pub logon_at: Option<Millis>,
    /// When the shell became ready, if it already was at startup.
    pub shell_ready_at: Option<Millis>,
}

/// Messages shown on the UI thread (the unsupported-build message is shown
/// before the Engine exists and is not listed here).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MessageKind {
    /// Disabling the Builtin_Restore_Option failed (Req 10.5).
    BuiltinRestoreDisableFailed,
}

// ---------------------------------------------------------------------------
// Event / Command
// ---------------------------------------------------------------------------

/// Every input to the core.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    // tracking
    WindowScanned(WindowObservation),
    TabRevoked { win: WinKey, tab: TabKey },
    WindowDestroyed { win: WinKey },
    WindowActivated { win: WinKey },
    ProcessExited { pid: u32 },
    ShellLost,
    ShellAvailable,
    // session end / control
    EndSessionQuery(EndKind),
    EndSessionCancelled,
    ExitRequested,
    SettingsChanged(Settings),
    PromptAnswered(PromptAnswer),
    /// Addition to design.md: the shell carried out
    /// `Command::SetBuiltinRestore(enabled)` with this result (the command has
    /// no op id). A failure after an accepted prompt is reported to the user
    /// and logged (Req 10.5).
    BuiltinRestoreSet { enabled: bool, result: Result<(), IoErrorKind> },
    /// Addition to design.md; see [`Environment`].
    EnvironmentChanged(Environment),
    // restore feedback
    /// `tab`: the window's initial tab, when the adapter knows it (saves the
    /// scan that would otherwise learn it).
    InitialLocationKnown { win: WinKey, tab: Option<TabKey>, location: LocationId, selection: Vec<ItemId> },
    /// `tab`: the claimed window's initial tab, when the adapter knows it.
    WindowOpened { request: RequestId, win: WinKey, tab: Option<TabKey> },
    TabCreated { win: WinKey, tab: TabKey },
    NavigationCompleted { tab: TabKey, location: LocationId },
    ProbeCompleted { probe: ProbeId, outcome: ProbeOutcome },
    OperationFailed { op: OpId, error: ShellError },
    /// `ActivateTab { op }` was carried out and the adapter saw the tab still
    /// selected afterwards (task 16.3). Confirms the activation; a scan that
    /// cannot tell which tab is selected no longer does.
    ActivationConfirmed { op: OpId },
    // tab folding (Req 14); handled by the `FoldingEngine` (see `Engine`,
    // "Tab folding").
    /// First signal of an Explorer window (Req 14.2). `bypass_keys`: Ctrl and
    /// Shift both down when the adapter first detected it (Req 14.4);
    /// `desktop`: the windows on its Virtual_Desktop (Req 14.9, 14.12);
    /// `foreground`: the Explorer window that was in the foreground before
    /// the new window took it (Req 14.22).
    WindowAppeared { win: WinKey, pid: u32, bypass_keys: bool, desktop: DesktopView, foreground: Option<WinKey> },
    /// A known window's tab window was destroyed: evidence of a tab dragged
    /// out of `from` (Req 14.3 b).
    TabDetached { from: WinKey },
    /// The tab window of a `CreateTab { op }` appeared (Req 14.12, 14.13).
    TabStarted { op: OpId, tab: TabKey },
    /// Answer to `ProbeFoldContext { probe, .. }`: the candidate's desktop
    /// and the windows that did not answer `WM_NULL` in time (Req 14.9,
    /// 14.12, 14.13).
    FoldContext { probe: FoldProbeId, desktop: DesktopView, unresponsive: BTreeSet<WinKey> },
    /// Addition to design.md: `HideWindow { win }` failed (for example an
    /// elevated Explorer window; the command carries no op id, so this is not
    /// an `OperationFailed`). The fold goes on and `ERROR operation=fold-hide`
    /// is logged (Req 14.16). Reported by the `CandidateHider` (task 25.4).
    HideFailed { win: WinKey, error: ShellError },
    // persistence
    /// Completion of `WriteSessionFile` or `PreserveBackup` with the same `seq`.
    WriteCompleted { seq: u64, result: Result<(), IoErrorKind> },
    Timer(TimerId),
}

/// Every output of the core.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    RescanWindow(WinKey),
    /// Rescan every open Explorer window, reporting windows the tracker may
    /// not know yet (after `ShellAvailable` and `EndSessionCancelled`).
    RescanAll,
    WriteSessionFile { seq: u64, text: String, urgent: bool },
    /// Preserve unreadable Session_File content as the single backup copy,
    /// replacing any previous backup (Req 11.7). Completion is reported by
    /// `WriteCompleted` with the same `seq`.
    PreserveBackup { seq: u64, content: Vec<u8> },
    AppendLog(LogEntry),
    OpenWindow { request: RequestId, location: LocationId, placement: FinalPlacement },
    ApplyPlacement { win: WinKey, placement: FinalPlacement },
    CreateTab { op: OpId, win: WinKey },
    Navigate { op: OpId, tab: TabKey, location: LocationId },
    CloseTab { op: OpId, tab: TabKey },
    ActivateTab { op: OpId, win: WinKey, index: usize },
    SelectItems { op: OpId, tab: TabKey, items: Vec<ItemId> },
    Reveal { win: WinKey },
    SetZOrder(Vec<WinKey>),
    Focus(WinKey),
    Probe { probe: ProbeId, location: LocationId },
    Notify { omitted_tabs: u32 },
    // tab folding (Req 14); the folding code also uses CreateTab, Navigate,
    // SelectItems, ActivateTab, CloseTab, ApplyPlacement, Focus, Reveal,
    // RescanWindow and AppendLog.
    /// Hide a Folding_Candidate at its first signal: layered, alpha 0 (Req 14.14, 14.16).
    HideWindow { win: WinKey },
    /// Read the candidate's Virtual_Desktop windows and test which windows
    /// answer `WM_NULL`; answered by `Event::FoldContext` (Req 14.9, 14.12, 14.13).
    ProbeFoldContext { probe: FoldProbeId, candidate: WinKey },
    /// Close a whole window (`WM_SYSCOMMAND SC_CLOSE`), used for a folded
    /// candidate (Req 14.1).
    CloseWindow { win: WinKey },
    ShowPrompt,
    ShowMessage(MessageKind),
    ShowSettings,
    SetBuiltinRestore(bool),
    SetStartAtSignIn(bool),
    PersistSettings(Settings),
    SetTimer { id: TimerId, at: Millis },
    Exit,
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

/// Which sub-components an event is delivered to. `Engine::handle` calls them
/// in the fixed order tracker â†’ store â†’ restore â†’ control.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Route {
    pub tracker: bool,
    pub store: bool,
    pub restore: bool,
    pub control: bool,
    /// The folding sub-component (Req 14). It is offered the events that can
    /// concern a Folding_Candidate or a Fold_Target first: an event about a
    /// candidate goes to it only, to none of the other components (Req 2.6,
    /// 14.27, 14.28); any other such event goes to the flagged components and
    /// then to it as well. See [`Engine::handle`].
    pub fold: bool,
}

impl Route {
    const NONE: Route = Route { tracker: false, store: false, restore: false, control: false, fold: false };

    fn only(owner: TimerOwner) -> Route {
        let mut r = Route::NONE;
        match owner {
            TimerOwner::Tracker => r.tracker = true,
            TimerOwner::Store => r.store = true,
            TimerOwner::Restore => r.restore = true,
            TimerOwner::Control => r.control = true,
            TimerOwner::Fold => r.fold = true,
        }
        r
    }
}

/// Delivery table for every event.
pub fn route(event: &Event) -> Route {
    let r = |tracker, store, restore, control| Route { tracker, store, restore, control, fold: false };
    // Events that can concern a Folding_Candidate or a Fold_Target as well.
    let rf = |tracker, store, restore, control| Route { tracker, store, restore, control, fold: true };
    match event {
        // Scans feed the Live_Session, restoration slot verification, and trigger detection.
        Event::WindowScanned(_) => rf(true, false, true, true),
        // Closes also end a pending Trigger_Window candidate (control).
        Event::WindowDestroyed { .. } => rf(true, false, true, true),
        Event::TabRevoked { .. } => rf(true, false, true, false),
        // A window first seen through its activation while another window is
        // open is never a Trigger_Window (control).
        Event::WindowActivated { .. } => rf(true, false, false, true),
        Event::ProcessExited { .. } => r(true, false, false, false),
        // A lost shell stops a running restoration (`Failure::ShellLost`).
        Event::ShellLost => r(true, false, true, true),
        Event::ShellAvailable => r(true, false, false, true),
        Event::EndSessionQuery(_) | Event::EndSessionCancelled => r(true, true, false, false),
        Event::ExitRequested => r(false, true, false, true),
        // Turning folding off leaves the candidates in progress open (fold).
        Event::SettingsChanged(_) => rf(false, false, false, true),
        Event::PromptAnswered(_) | Event::BuiltinRestoreSet { .. } | Event::EnvironmentChanged(_) => {
            r(false, false, false, true)
        }
        // Trigger classification (control) and Targeted_Launch composition (restore).
        Event::InitialLocationKnown { .. } => rf(false, false, true, true),
        Event::NavigationCompleted { .. } => rf(true, false, true, false),
        // A claimed window is never a Trigger_Window (control, step 1).
        Event::WindowOpened { .. } => r(false, false, true, true),
        // The folding sub-component also needs the registration of a Folded_Tab
        // and the confirmation of its activation (ops of the `FoldingEngine`
        // go to it only).
        Event::TabCreated { .. } | Event::ActivationConfirmed { .. } => rf(false, false, true, false),
        Event::ProbeCompleted { .. } | Event::OperationFailed { .. } => r(false, false, true, false),
        // The restore session waits for the durable attempt-counter write,
        // the exit flow (control) for the exit write.
        Event::WriteCompleted { .. } => r(false, true, true, true),
        // Tab folding: the `FoldingEngine` alone.
        Event::WindowAppeared { .. }
        | Event::TabDetached { .. }
        | Event::TabStarted { .. }
        | Event::FoldContext { .. }
        | Event::HideFailed { .. } => Route { fold: true, ..Route::NONE },
        Event::Timer(id) => Route::only(id.owner),
    }
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

/// Functional core: every input is an [`Event`], every output a [`Command`].
///
/// # Trigger detection (task 11.5; design.md "Trigger detection and launch
/// classification", Req 4.1-4.3, 4.8, 4.11, 10.4)
///
/// A window is evaluated once, at its *first sighting*: the first
/// `WindowScanned` the tracker accepts for it, or its `InitialLocationKnown`
/// if that comes first. A window claimed by `WindowOpened` is never
/// evaluated. It is a Trigger_Window candidate only if no other Explorer
/// window is open ([`LiveSessionTracker::has_other_open_window`], or another
/// candidate is pending) and no Windows session end froze the Live_Session.
/// A window whose activation comes before its first sighting while another
/// window is open is never evaluated either: it appeared while the session
/// was running (design.md: the check applies when the window appears).
/// Such windows, and windows first sighted while another is open, still get
/// check 3 (Builtin_Restore_Option logon restore, which opens all saved
/// windows at once, so that none of them may be a candidate): when it holds
/// and a Pending_Snapshot exists, the snapshot is consumed and
/// `BUILTIN-RESTORE-DETECTED` logged (task 16.6, Req 10.4).
/// The checks then run in design order, each ending the evaluation:
///
/// 1. the restore session still has unclaimed `OpenWindow` requests (the
///    window may be one of them; claims arrive as `WindowOpened`);
/// 2. a restoration is in progress;
/// 3. Builtin_Restore_Option logon restore ([`is_builtin_restore`]): the
///    Pending_Snapshot is consumed and `BUILTIN-RESTORE-DETECTED` logged;
/// 4. no Pending_Snapshot: nothing;
/// 5. restoration disabled in Settings: the Pending_Snapshot is consumed;
/// 6. `failed_attempts â‰¥ 3`: consumed and `ABANDONED` logged;
/// 7. otherwise the window becomes the candidate: checkpoints are held back
///    ([`StoreLogic::begin_restoration`], so no checkpoint replaces the
///    Pending_Snapshot meanwhile) and a `Control` timer is set for
///    [`INITIAL_LOCATION_WAIT_MS`]. When the candidate's
///    `InitialLocationKnown` arrives, steps 4-6 are checked again, the
///    launch is classified ([`is_default_launch`] with
///    [`Environment::launch_to_context`]), and the restoration starts
///    ([`Engine::start_restoration`]) with the initial Location, the
///    selection, and the trigger's tab if already scanned.
///
/// If the candidate closes, is claimed, or its timer fires first, it is not
/// a trigger and checkpointing resumes. The adapters announce
/// `InitialLocationKnown` for every window that opens with an identifiable
/// initial Location; windows that were already open when the monitor
/// attached or the shell reconnected are never announced. Restoring
/// without it could therefore duplicate windows that were already open
/// (Req 4.2) and could not tell a Targeted_Launch from a Default_Launch.
///
/// The restore session only ever commands the trigger and the windows it
/// opened, so windows the user opens during a restoration get no
/// restoration command; the tracker tracks them as usual (Req 4.2, 4.5).
///
/// # Control flows (task 11.6; Req 4.9, 10.1-10.3, 10.5-10.7, 12.3-12.5,
/// 12.8, 12.9)
///
/// - **Builtin_Restore_Option prompt.** Decided once per start: at
///   [`Engine::new`] when `PersistBrowsers` is known, otherwise at the first
///   `EnvironmentChanged`. `ShowPrompt` is emitted when `PersistBrowsers` is
///   enabled and no decline is recorded in Settings (Req 10.1), never twice
///   in one run. The prompt runs on the UI thread and blocks nothing here:
///   tracking and saving go on while it is shown (Req 10.7). The answer:
///   - *accepted:* `SetBuiltinRestore(false)` (Req 10.2). The shell reports
///     the result as [`Event::BuiltinRestoreSet`]. On failure the option is
///     unchanged, `ShowMessage(BuiltinRestoreDisableFailed)` and an `ERROR`
///     entry are emitted, and nothing is recorded in Settings, so the prompt
///     is shown again at the next start (Req 10.5). On success the
///     Engine's copy of `PersistBrowsers` becomes `false`;
///   - *declined:* `builtin_prompt_declined` is set and persisted
///     (`PersistSettings`), so no later start prompts (Req 10.3);
///   - *dismissed:* nothing is recorded; the next start prompts again
///     (Req 10.6).
/// - **Settings.** `SettingsChanged` applies at once (the next trigger
///   decision uses it) and is persisted with `PersistSettings`;
///   `SetStartAtSignIn` follows when start-at-sign-in changed (Req 12.4).
///   Re-enabling the prompt is a `SettingsChanged` that clears
///   `builtin_prompt_declined`; the prompt then shows at the next start at
///   which `PersistBrowsers` is enabled (Req 12.3). Disabled restoration
///   only affects trigger step 5: Session_Ends are saved as usual (Req 4.9).
/// - **Exit through Settings.** `ExitRequested` with at least one open
///   Explorer window writes the capture at that moment urgently as the
///   Pending_Snapshot (`SaveCause::ExitThroughSettings`, Req 12.8) and emits
///   `Exit` once that write (or its retry) is durable, or after
///   [`EXIT_WRITE_TIMEOUT_MS`] (an `ERROR` entry is logged then), within the
///   5 s of Req 12.5. With no window open the Session_File is left unchanged
///   and `Exit` is emitted at once (Req 12.9). From the request on, the
///   Engine only completes that write: every other event is ignored, so no
///   checkpoint replaces the exit capture and no window-affecting command is
///   emitted (Explorer windows stay as they are, Req 12.5).
///
/// # Deferred tabs (Req 5.13, task 16.3)
///
/// With [`Environment::placeholder_dir`] set, a restoration opens at most
/// [`IMMEDIATE_FOLDER_BUDGET`] distinct saved Locations at once and restores
/// the other tabs as deferred tabs showing placeholders
/// ([`crate::plan::defer`]). Two things make them behave like their saved
/// folders:
///
/// - **Tracking.** The tracker's copy of every `WindowScanned` and
///   `NavigationCompleted` reports a tab that shows a placeholder as showing
///   the Location the placeholder stands for
///   ([`RestoreSession::deferred_target`]), so the Live_Session and every
///   Pending_Snapshot saved from it hold the saved Locations. The restore
///   session itself sees the placeholders.
/// - **Activation.** Once the restoration has ended, a scan in which the
///   window's Active_Tab (per the tracker) still shows a placeholder makes
///   the Engine navigate that tab to the Location it stands for (its own
///   `Navigate` ops, numbered from [`DEFERRED_OP_BASE`]), at most
///   [`DEFERRED_NAV_ATTEMPTS`] times per tab and again only after
///   [`DEFERRED_RENAVIGATE_MS`]. A failure is logged (`deferred-navigate`).
///
/// # Tab folding (Req 14; task 22.2)
///
/// The [`FoldingEngine`] is the fourth sub-component. It owns a
/// Folding_Candidate from its first signal until the candidate is folded,
/// switched, or left open, and while it does, the tracker, the store, trigger
/// detection, and the restore session never see the window (Req 2.6, 14.27,
/// 14.28):
///
/// - `WindowAppeared` is classified by the `FoldingEngine` with the Engine's
///   inputs ([`FirstSignalEnv`]): the tracked, open windows, whether a
///   restoration is in progress, a Trigger_Window candidate is waiting, or
///   `OpenWindow` requests are unclaimed, and the Builtin_Restore_Option
///   heuristic. A window it excludes continues on the existing path
///   unchanged; a candidate is hidden at once.
/// - `TabDetached` (of a tracked window), `TabStarted`, `FoldContext`, and
///   the `ActivationConfirmed` of an op the `FoldingEngine` issued go to it
///   only. The scans, `InitialLocationKnown`, revocations, activation,
///   destruction, and navigations of a candidate, and the tabs it reports
///   (`TabCreated`), go to it only as well. A tab belongs to a candidate once
///   a candidate's scan, `InitialLocationKnown`, or `TabCreated` named it.
/// - Events about other windows also reach the `FoldingEngine` after the
///   other components, where it needs them: `TabCreated`, `NavigationCompleted`
///   (the Folded_Tab of a Fold_Target), `TabRevoked` (the Matching_Tab of a
///   Tab_Switch), and `WindowDestroyed` (a Fold_Target or Matching_Tab's
///   window; the window also leaves the Opening_Order).
/// - Its timers are `Fold` timers on the common `SetTimer`/`Timer` mechanism.
/// - `SettingsChanged` reaches it after the control flow: turning folding off
///   leaves every candidate in progress open (`not-folded`,
///   `folding-disabled`), turning it on affects only later windows (Req
///   12.10, 14.3, 14.14).
/// - `ProcessExited` and `ShellLost` reach it *before* the tracker (which
///   learns the number of candidates in progress with each event): the
///   candidates of that process, or all of them, are dropped with `ERROR
///   operation=fold-shell`, and nothing is replayed to the tracker (Req
///   14.14). A candidate revealed because the shell was lost is never a
///   Trigger_Window.
/// - `HideWindow` has no op id, so its failure is not an `OperationFailed`:
///   the adapter reports it with [`Event::HideFailed`], which logs `ERROR
///   operation=fold-hide` and lets the fold go on (Req 14.16). An
///   `OperationFailed` of a fold op is not routed to the `FoldingEngine`; the
///   fold's timers end an attempt whose command failed.
/// - A window it gives up (left open, drag-out, several tabs) comes back as a
///   [`Released`]: the Engine replays the buffered events to the other
///   components as if the window had never been owned, marks it as seen by
///   trigger detection, and rescans it.
///
/// What the `FoldingEngine` reads from the Engine: the tracker's fold view
/// (`fold_windows`, `open_window_keys`), a tab's key at an index of a window,
/// and the pre-minimize placement of a minimized Fold_Target.
#[derive(Debug)]
pub struct Engine {
    settings: Settings,
    environment: Environment,
    background_logon: bool,
    logon_at: Option<Millis>,
    shell_ready_at: Option<Millis>,
    /// Startup Session_File read, handed to the store (task 8.3).
    session_file: SessionFileRead,
    /// The Live_Session (tasks 7.4-7.6).
    tracker: LiveSessionTracker,
    /// The Session_Store (tasks 8.3, 8.4).
    store: StoreLogic,
    /// The current or last restoration (tasks 11.2-11.4); created by
    /// [`Engine::start_restoration`].
    restore: Option<RestoreSession>,
    /// Trigger detection (task 11.5).
    trigger: TriggerState,
    /// Prompt and exit flows (task 11.6).
    control: ControlState,
    /// Deferred tabs of the last restoration (Req 5.13).
    deferred: DeferredTabs,
    /// Tab folding (task 22.2).
    fold: FoldingEngine,
    /// The tabs of the Folding_Candidates the `FoldingEngine` owns, so that
    /// their navigations stay away from the tracker (Req 2.6, 14.27).
    candidate_tabs: BTreeMap<TabKey, WinKey>,
    /// The process of every Folding_Candidate in progress (from its
    /// `WindowAppeared`), for `ProcessExited` (Req 14.14).
    candidate_pids: BTreeMap<WinKey, u32>,
}

/// First id of the Engine's own `Navigate` commands for deferred tabs, far
/// above any id a restore session allocates (they count up from 1).
pub const DEFERRED_OP_BASE: u64 = 1 << 62;

/// A deferred tab is navigated again when it is seen active and still
/// showing its placeholder this long after the last attempt.
pub const DEFERRED_RENAVIGATE_MS: u64 = 5_000;

/// Most `Navigate` attempts per deferred tab.
pub const DEFERRED_NAV_ATTEMPTS: u32 = 2;

/// `ERROR` entry operation name of a deferred tab's failed navigation.
pub const OP_DEFERRED_NAVIGATE: &str = "deferred-navigate";

/// Deferred tabs (Req 5.13; see the `Engine` docs, "Deferred tabs"). Which
/// Location a placeholder stands for is answered by the last restore
/// session ([`RestoreSession::deferred_target`]).
#[derive(Clone, Debug, Default)]
struct DeferredTabs {
    /// Navigations of activated deferred tabs: tab â†’ (op, placeholder,
    /// issued at).
    navigating: BTreeMap<TabKey, (OpId, LocationId, Millis)>,
    /// `Navigate` attempts per placeholder.
    attempts: HashMap<LocationId, u32>,
    next_op: u64,
}

/// How long `ExitRequested` waits for its Session_File write to become
/// durable before exiting anyway (one retry fits: Req 3.12 retries after
/// 2 s; Req 12.5 allows 5 s for the whole exit).
pub const EXIT_WRITE_TIMEOUT_MS: u64 = 4_000;

/// `ERROR` entry operation names of the control flows.
pub const OP_DISABLE_BUILTIN_RESTORE: &str = "disable-builtin-restore";
pub const OP_EXIT: &str = "exit";

/// The Builtin_Restore_Option prompt within one run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PromptState {
    /// `PersistBrowsers` was unknown at startup; decided at the first
    /// `EnvironmentChanged`.
    #[default]
    Undecided,
    /// `ShowPrompt` was emitted and no answer has arrived yet.
    Shown,
    /// Answered, or not needed in this run.
    Done,
}

/// The exit flow after `ExitRequested`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExitState {
    /// Waiting for the exit write `seq` to become durable, until `timer`.
    Writing { seq: u64, timer: TimerId },
    /// `Exit` was emitted.
    Exited,
}

#[derive(Clone, Debug, Default)]
struct ControlState {
    prompt: PromptState,
    exit: Option<ExitState>,
}

/// How long a Trigger_Window candidate waits for `InitialLocationKnown`
/// (design.md "Trigger detection", step 7).
pub const INITIAL_LOCATION_WAIT_MS: u64 = 2_000;

/// A Trigger_Window candidate that passed steps 1-6 and waits for its
/// initial Location (step 7). Checkpoints are held back meanwhile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Candidate {
    win: WinKey,
    timer: TimerId,
}

/// Trigger detection state (see [`Engine`], "Trigger detection").
#[derive(Clone, Debug, Default)]
struct TriggerState {
    /// Windows already seen by trigger detection: each window is evaluated
    /// once, at its first sighting. Removed at `WindowDestroyed`.
    seen: BTreeSet<WinKey>,
    candidate: Option<Candidate>,
    /// The initial tab reported with the last `InitialLocationKnown`.
    initial_tab: Option<(WinKey, TabKey)>,
    /// `seq` of the last `Control` timer set.
    timer_seq: u64,
}

/// Result of trigger steps 4-6.
enum Gate {
    /// Do not restore; carry out these commands.
    Stop(Vec<Command>),
    Proceed,
}

impl Engine {
    pub fn new(settings: Settings, startup: StartupInput) -> (Self, Vec<Command>) {
        let (store, store_commands) = StoreLogic::startup(&startup.session_file);
        let fold = FoldingEngine::new(settings.tab_folding_enabled);
        let mut engine = Engine {
            settings,
            environment: startup.environment,
            background_logon: startup.background_logon,
            logon_at: startup.logon_at,
            shell_ready_at: startup.shell_ready_at,
            session_file: startup.session_file,
            tracker: LiveSessionTracker::new(),
            store,
            restore: None,
            trigger: TriggerState::default(),
            control: ControlState::default(),
            deferred: DeferredTabs::default(),
            fold,
            candidate_tabs: BTreeMap::new(),
            candidate_pids: BTreeMap::new(),
        };
        // Session_File handling (task 8.3), then the Builtin_Restore_Option
        // prompt if `PersistBrowsers` is already known (Req 10.1).
        let mut out = store_commands;
        if engine.environment.options.persist_browsers.is_some() {
            out.extend(engine.decide_prompt());
        }
        // Windows already open when tracking starts rank in their
        // Activation_Order (Req 14.8). The tracker is empty here; the
        // windows it will learn from the startup rescan are ranked at
        // `ShellAvailable`.
        engine.seed_opening_order();
        (engine, out)
    }

    /// Ranks the open windows the tracker knows in their Activation_Order at
    /// this moment, most recent first (Req 14.8): the Opening_Order of
    /// windows already open when tracking starts or resumes. Every earlier
    /// position is dropped, so a window that is then open and later reported
    /// by `WindowAppeared` is already known and is never a Folding_Candidate
    /// (Req 14.3).
    fn seed_opening_order(&mut self) {
        let open = self.tracker.open_window_keys();
        self.fold.opening_mut().reset_to_activation(&open);
    }

    /// `now` is a monotonic millisecond timestamp supplied by the shell.
    pub fn handle(&mut self, now: Millis, event: Event) -> Vec<Command> {
        if self.control.exit.is_some() {
            return self.exiting_handle(now, &event);
        }
        let route = route(&event);
        // An event about a Folding_Candidate goes to the `FoldingEngine` only.
        if route.fold
            && let Some(commands) = self.fold_owned(now, &event)
        {
            return commands;
        }
        // The candidates of an exited process or a lost shell are dropped
        // before the tracker sees the event, which needs their number to
        // decide on a Session_End (Req 14.14, 14.15).
        let mut out = self.fold_shell_gone(now, &event);
        out.extend(self.components(now, route, &event));
        if route.fold {
            out.extend(self.fold_shared(now, &event));
        }
        // The shell is back and the tracker has rescanned what is open:
        // those windows rank in their Activation_Order at this moment
        // (Req 14.8).
        if matches!(event, Event::ShellAvailable) {
            self.seed_opening_order();
        }
        out
    }

    /// The tracker, store, restore session, and control for `event`, in that
    /// order, as routed by `route`, then the deferred-tab navigation.
    fn components(&mut self, now: Millis, route: Route, event: &Event) -> Vec<Command> {
        let mut out = Vec::new();
        if route.tracker {
            // The Live_Session records what a deferred tab stands for, not
            // its placeholder (Req 5.13).
            let tracked = self.translate_deferred(event);
            out.extend(self.tracker_handle(now, &tracked));
        }
        if route.store {
            out.extend(self.store_handle(now, event));
        }
        if route.restore {
            out.extend(self.restore_handle(now, event));
        }
        if route.control {
            out.extend(self.control_handle(now, event));
        }
        out.extend(self.deferred_handle(now, event));
        out
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn environment(&self) -> &Environment {
        &self.environment
    }

    pub fn background_logon(&self) -> bool {
        self.background_logon
    }

    pub fn logon_at(&self) -> Option<Millis> {
        self.logon_at
    }

    pub fn shell_ready_at(&self) -> Option<Millis> {
        self.shell_ready_at
    }

    pub fn startup_session_file(&self) -> &SessionFileRead {
        &self.session_file
    }

    /// The Live_Session tracker.
    pub fn tracker(&self) -> &LiveSessionTracker {
        &self.tracker
    }

    // --- sub-component stubs -------------------------------------------------

    /// `LiveSessionTracker` (tasks 7.4-7.6). Its `session_end` and `changed`
    /// outputs go to the store (task 8.3).
    fn tracker_handle(&mut self, now: Millis, event: &Event) -> Vec<Command> {
        // The candidates in progress count as open windows for Session_End
        // detection (Req 14.15, 14.28, 2.7).
        self.tracker.set_candidates_in_progress(self.fold.candidates_in_progress());
        let out = self.tracker.handle(now, event);
        let mut commands = out.commands;
        let wall = self.environment.wall_clock;
        if let Some(end) = out.session_end {
            commands.extend(self.store.session_end(now, wall, end.cause, end.snapshot));
        } else if out.changed {
            commands.extend(self.store.live_changed(now, &self.tracker));
        }
        commands
    }

    /// `StoreLogic` (tasks 8.3, 8.4).
    fn store_handle(&mut self, now: Millis, event: &Event) -> Vec<Command> {
        self.store.handle(now, self.environment.wall_clock, event, &self.tracker)
    }

    /// The Session_Store.
    pub fn store(&self) -> &StoreLogic {
        &self.store
    }

    /// The current or last restoration session.
    pub fn restore_session(&self) -> Option<&RestoreSession> {
        self.restore.as_ref()
    }

    /// Starts restoring the Pending_Snapshot (the hook trigger detection,
    /// task 11.5, calls once its checks pass): counts the attempt with
    /// [`StoreLogic::begin_attempt`] (increment-before-attempt, Req 4.6, 4.7)
    /// and creates the [`RestoreSession`], which waits for that write. The
    /// Engine fills in `params.attempt_write_seq` and `params.first_id`
    /// (ids continue from the previous session). Returns the attempt write,
    /// or nothing (and no session) when the store has no Pending_Snapshot to
    /// attempt (none, an attempt runs, or it already failed 3 times).
    pub fn start_restoration(&mut self, now: Millis, mut params: RestoreParams) -> Vec<Command> {
        // Whatever candidate waited is decided now.
        self.trigger.candidate = None;
        let Some((seq, write)) = self.store.begin_attempt() else { return Vec::new() };
        params.attempt_write_seq = seq;
        params.first_id = self.restore.as_ref().map_or(1, RestoreSession::next_id);
        let mut session = RestoreSession::new(now, params);
        // A restoration starts only while no Explorer window is open, so no
        // tab shows an earlier restoration's placeholder any more.
        self.deferred = DeferredTabs { next_op: self.deferred.next_op, ..DeferredTabs::default() };
        if let Some(dir) = &self.environment.placeholder_dir {
            let config = DeferralConfig { budget: IMMEDIATE_FOLDER_BUDGET, placeholder_dir: dir.clone() };
            session = session.with_deferral(&config);
        }
        self.restore = Some(session);
        vec![write]
    }

    /// `RestoreSession` (tasks 11.2-11.4). When the session ends:
    ///
    /// - **Completed:** the planned Activation_Order is applied to the
    ///   tracker (the restored windows were never activated by the user;
    ///   Req 4.5), orphans are dropped (Req 3.3), the store marks the
    ///   snapshot Consumed and resumes checkpointing (Req 4.4), and a full
    ///   rescan rebuilds the Live_Session from what is open (Req 4.5).
    /// - **Failed:** the store keeps the counted attempt and protection, or
    ///   abandons the snapshot at the third failure (Req 4.10, 4.11).
    fn restore_handle(&mut self, now: Millis, event: &Event) -> Vec<Command> {
        let Some(session) = self.restore.as_mut() else { return Vec::new() };
        let mut out = session.handle(now, event);
        let trigger = session.trigger();
        match session.take_outcome() {
            Some(RestoreOutcome::Completed(done)) => {
                // The Trigger_Window, then the restored windows in their saved
                // Activation_Order, rank in front of every window the user
                // opened during the restoration (Req 14.8).
                self.fold.opening_mut().restoration_completed(Some(trigger), &done.z_order);
                // Last to first, so the first planned window ends up most
                // recently activated. Tracker commands (none are expected for
                // activations) are kept.
                for win in done.z_order.iter().rev() {
                    out.extend(self.tracker.handle(now, &Event::WindowActivated { win: *win }).commands);
                }
                self.tracker.drop_orphans();
                out.extend(self.store.restoration_completed(now, &self.tracker));
                out.push(Command::RescanAll);
            }
            Some(RestoreOutcome::Failed(_)) => out.extend(self.store.restoration_failed(now, &self.tracker)),
            None => {}
        }
        out
    }

    /// The tracker's copy of `event`: a tab showing a placeholder of a
    /// deferred tab (Req 5.13) is reported showing the Location the
    /// placeholder stands for, so the Live_Session, and every snapshot saved
    /// from it, keeps the saved Location of a tab that was never activated.
    fn translate_deferred<'e>(&self, event: &'e Event) -> Cow<'e, Event> {
        let Some(session) = self.restore.as_ref().filter(|s| s.has_deferred()) else { return Cow::Borrowed(event) };
        let target = |l: &LocationId| session.deferred_target(l);
        match event {
            Event::WindowScanned(obs) => {
                let shows = |t: &TabObservation| matches!(&t.location, ObservedLocation::Known(l) if target(l).is_some());
                if !obs.tabs.iter().any(shows) {
                    return Cow::Borrowed(event);
                }
                let mut obs = obs.clone();
                // The UIA strip item of a deferred tab is named after its
                // placeholder (by parsing name on newer builds, by display
                // name on older ones), so it is renamed like the tab's
                // Location; otherwise the tracker could not tell which tab a
                // selected item is (found on build 26300).
                let mut renames: Vec<(WideString, WideString)> = Vec::new();
                for t in &mut obs.tabs {
                    if let ObservedLocation::Known(l) = &t.location
                        && let Some(real) = target(l)
                    {
                        renames.push((l.parsing_name.clone(), real.parsing_name.clone()));
                        renames.push((l.display_name.clone(), real.display_name.clone()));
                        t.location = ObservedLocation::Known(real);
                    }
                }
                if let Some(strip) = &mut obs.strip {
                    crate::plan::defer::rename_strip_names(&mut strip.names, &renames);
                }
                Cow::Owned(Event::WindowScanned(obs))
            }
            Event::NavigationCompleted { tab, location } => match target(location) {
                Some(real) => Cow::Owned(Event::NavigationCompleted { tab: *tab, location: real }),
                None => Cow::Borrowed(event),
            },
            _ => Cow::Borrowed(event),
        }
    }

    /// Deferred tabs after the restoration (Req 5.13; see the `Engine` docs,
    /// "Deferred tabs"): a scan whose window's Active_Tab (per the tracker,
    /// which has just seen the scan) still shows a placeholder navigates
    /// that tab to the Location the placeholder stands for, at most
    /// [`DEFERRED_NAV_ATTEMPTS`] times, and again only
    /// [`DEFERRED_RENAVIGATE_MS`] after the last attempt.
    fn deferred_handle(&mut self, now: Millis, event: &Event) -> Vec<Command> {
        let Some(session) = self.restore.as_ref().filter(|s| s.is_finished() && s.has_deferred()) else {
            return Vec::new();
        };
        let d = &mut self.deferred;
        match event {
            Event::WindowScanned(obs) => {
                let Some(w) = self.tracker.window(obs.win) else { return Vec::new() };
                let Some(active) = w.active().or_else(|| w.tabs().first().map(|t| t.key())) else {
                    return Vec::new();
                };
                let Some(ObservedLocation::Known(shown)) = obs.tabs.iter().find(|t| t.tab == active).map(|t| &t.location)
                else {
                    return Vec::new();
                };
                let Some(target) = session.deferred_target(shown) else { return Vec::new() };
                if d.navigating.get(&active).is_some_and(|(_, _, at)| now.since(*at) < DEFERRED_RENAVIGATE_MS) {
                    return Vec::new();
                }
                let attempts = d.attempts.entry(shown.clone()).or_insert(0);
                if *attempts >= DEFERRED_NAV_ATTEMPTS {
                    return Vec::new();
                }
                *attempts += 1;
                let op = OpId(DEFERRED_OP_BASE + d.next_op);
                d.next_op += 1;
                d.navigating.insert(active, (op, shown.clone(), now));
                vec![Command::Navigate { op, tab: active, location: target }]
            }
            Event::NavigationCompleted { tab, location } => {
                if d.navigating.get(tab).is_some_and(|(_, p, _)| p != location) {
                    d.navigating.remove(tab);
                }
                Vec::new()
            }
            Event::OperationFailed { op, error } => {
                let Some(tab) = d.navigating.iter().find(|(_, (o, ..))| o == op).map(|(t, _)| *t) else {
                    return Vec::new();
                };
                d.navigating.remove(&tab);
                vec![Command::AppendLog(LogEntry::Error {
                    operation: OP_DEFERRED_NAVIGATE.to_owned(),
                    description: format!("{} (0x{:08X})", error.message, error.code as u32),
                })]
            }
            Event::TabRevoked { tab, .. } => {
                d.navigating.remove(tab);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Trigger detection (task 11.5) and control flows (task 11.6; see the
    /// `Engine` docs, "Control flows").
    fn control_handle(&mut self, now: Millis, event: &Event) -> Vec<Command> {
        match event {
            Event::SettingsChanged(s) => return self.settings_changed(s),
            Event::EnvironmentChanged(env) => {
                self.environment = env.clone();
                if self.control.prompt == PromptState::Undecided {
                    return self.decide_prompt();
                }
            }
            Event::PromptAnswered(answer) => return self.prompt_answered(*answer),
            Event::BuiltinRestoreSet { enabled, result } => return self.builtin_restore_set(*enabled, *result),
            Event::ExitRequested => return self.exit_requested(now),
            Event::ShellAvailable => self.shell_ready_at = Some(now),
            // Only windows the tracker accepted (not a stale scan of a
            // window that already closed).
            Event::WindowScanned(obs) if self.tracker.window(obs.win).is_some() => {
                return self.window_sighted(now, obs.win, None);
            }
            Event::InitialLocationKnown { win, tab, location, selection } => {
                if let Some(t) = tab {
                    self.trigger.initial_tab = Some((*win, *t));
                }
                return self.window_sighted(now, *win, Some((location, selection)));
            }
            Event::WindowOpened { win, .. } => {
                // Claimed by an outstanding request: never a trigger (step 1).
                self.trigger.seen.insert(*win);
                return self.drop_candidate(now, *win);
            }
            Event::WindowActivated { win } => return self.window_activated(now, *win),
            Event::WindowDestroyed { win } => {
                self.trigger.seen.remove(win);
                return self.drop_candidate(now, *win);
            }
            Event::Timer(id) if self.trigger.candidate.is_some_and(|c| c.timer == *id) => {
                // No initial Location within 2 s: not a trigger (see the
                // `Engine` docs, "Trigger detection").
                self.trigger.candidate = None;
                return self.store.end_restoration(now, &self.tracker);
            }
            _ => {}
        }
        Vec::new()
    }

    /// The pending Trigger_Window candidate, if any (step 7).
    pub fn trigger_candidate(&self) -> Option<WinKey> {
        self.trigger.candidate.map(|c| c.win)
    }

    /// A window's activation can precede its first scan (a new or drag-out
    /// window whose rescan is still debounced). If it is the first sign of
    /// the window and another Explorer window is open at that moment, the
    /// window appeared while the session was running and is never a
    /// Trigger_Window (Req 4.1, 4.2), even if the other windows close before
    /// its first scan. It is marked as evaluated (and checked for Explorer's
    /// logon restore, see [`Engine::builtin_restore_seen`]); otherwise the
    /// evaluation waits for its first scan or `InitialLocationKnown` as usual.
    fn window_activated(&mut self, now: Millis, win: WinKey) -> Vec<Command> {
        if self.trigger.seen.contains(&win) || self.tracker.window(win).is_some() || self.tracker.is_frozen() {
            return Vec::new();
        }
        if self.tracker.has_other_open_window(win) || self.trigger.candidate.is_some() {
            self.trigger.seen.insert(win);
            return self.builtin_restore_seen(now);
        }
        Vec::new()
    }

    /// Trigger step 3 for a window that is not a Trigger_Window candidate
    /// because another Explorer window was already open when it was first
    /// seen. Explorer's logon restore (Builtin_Restore_Option) opens all
    /// saved windows within a fraction of a second, so the windows activate
    /// one another before their first scans and none of them may be a
    /// candidate; the snapshot must be consumed all the same (Req 10.4).
    /// When the BuiltinRestore heuristic holds, a Pending_Snapshot exists,
    /// and no restoration runs, the snapshot is consumed and
    /// `BUILTIN-RESTORE-DETECTED` is logged; afterwards there is no
    /// Pending_Snapshot left, so this happens at most once per snapshot.
    fn builtin_restore_seen(&mut self, now: Millis) -> Vec<Command> {
        let restoring = self
            .restore
            .as_ref()
            .is_some_and(|s| s.has_pending_requests() || !s.is_finished());
        if restoring || self.store.pending_snapshot().is_none() || !is_builtin_restore(&self.builtin_context(), now) {
            return Vec::new();
        }
        let mut out = self.store.consume_pending();
        out.push(Command::AppendLog(LogEntry::BuiltinRestoreDetected));
        out
    }

    /// `win` was scanned (`initial` = `None`) or announced with its initial
    /// Location. The first sighting runs steps 1-6; the candidate's initial
    /// Location runs step 7.
    fn window_sighted(&mut self, now: Millis, win: WinKey, initial: Option<(&LocationId, &[ItemId])>) -> Vec<Command> {
        if self.trigger.seen.insert(win) {
            return self.first_sighting(now, win, initial);
        }
        match initial {
            Some((location, selection)) if self.trigger.candidate.is_some_and(|c| c.win == win) => {
                self.decide(now, win, location, selection)
            }
            _ => Vec::new(),
        }
    }

    /// Steps 1-6 for a window seen for the first time. When they all pass,
    /// step 7 runs at once if the initial Location is already known, else
    /// the window waits for it as the candidate (holding checkpoints back).
    fn first_sighting(&mut self, now: Millis, win: WinKey, initial: Option<(&LocationId, &[ItemId])>) -> Vec<Command> {
        let other_open = self.tracker.has_other_open_window(win) || self.trigger.candidate.is_some();
        if self.tracker.is_frozen() {
            // During a Windows session end.
            return Vec::new();
        }
        if other_open {
            // Opened while another window is open: not a trigger (Req 4.2).
            // Explorer's logon restore opens all saved windows at once,
            // though, so it may be detected here (Req 10.4).
            return self.builtin_restore_seen(now);
        }
        // No other window is open: earlier windows are gone or orphaned.
        let tracker = &self.tracker;
        self.trigger.seen.retain(|k| *k == win || tracker.window(*k).is_some());
        // 1. Claims are reported by `WindowOpened`; a window first seen in a
        //    scan while requests are unclaimed may be one of them.
        // 2. A restoration is in progress (Req 4.2).
        if let Some(session) = &self.restore
            && (session.has_pending_requests() || !session.is_finished())
        {
            return Vec::new();
        }
        // 3. Explorer's own logon restore opened the window (Req 10.4).
        if is_builtin_restore(&self.builtin_context(), now) {
            let mut out = self.store.consume_pending();
            out.push(Command::AppendLog(LogEntry::BuiltinRestoreDetected));
            return out;
        }
        if let Gate::Stop(out) = self.gate_snapshot() {
            return out;
        }
        // 7. Classify now, or wait for the initial Location.
        if let Some((location, selection)) = initial {
            return self.decide(now, win, location, selection);
        }
        self.store.begin_restoration();
        self.trigger.timer_seq += 1;
        let timer = TimerId { owner: TimerOwner::Control, seq: self.trigger.timer_seq };
        self.trigger.candidate = Some(Candidate { win, timer });
        vec![Command::SetTimer { id: timer, at: now + INITIAL_LOCATION_WAIT_MS }]
    }

    /// Steps 4-6 (also re-checked when the candidate's initial Location
    /// arrives, in case the settings or the snapshot changed meanwhile).
    fn gate_snapshot(&mut self) -> Gate {
        // 4. Nothing to restore (Req 4.3).
        let Some(doc) = self.store.pending_snapshot() else { return Gate::Stop(Vec::new()) };
        let exhausted = doc.failed_attempts >= MAX_FAILED_ATTEMPTS;
        // 5. Restoration disabled: consume, open nothing (Req 4.8).
        if !self.settings.restoration_enabled {
            return Gate::Stop(self.store.consume_pending());
        }
        // 6. Abandoned after 3 failed attempts (Req 4.11).
        if exhausted {
            return Gate::Stop(self.store.abandon_if_exhausted());
        }
        Gate::Proceed
    }

    /// Step 7 for the candidate `win`: classify the launch and start the
    /// restoration (Req 4.1, 6.1, 6.2).
    fn decide(&mut self, now: Millis, win: WinKey, location: &LocationId, selection: &[ItemId]) -> Vec<Command> {
        self.trigger.candidate = None;
        if let Gate::Stop(mut out) = self.gate_snapshot() {
            out.extend(self.store.end_restoration(now, &self.tracker));
            return out;
        }
        let Some(doc) = self.store.pending_snapshot() else { unreachable!("checked by gate_snapshot") };
        let env = &self.environment;
        let launch = if is_default_launch(location, env.options.launch_to, &env.launch_to_context) {
            LaunchInfo::Default
        } else {
            LaunchInfo::Targeted { requested: location.clone() }
        };
        let params = RestoreParams {
            snapshot: doc.snapshot.clone(),
            launch,
            displays: env.displays.clone(),
            trigger: win,
            trigger_tab: self
                .tracker
                .window(win)
                .and_then(|w| w.tabs().first())
                .map(|t| t.key())
                .or_else(|| self.trigger.initial_tab.filter(|(w, _)| *w == win).map(|(_, t)| t)),
            trigger_location: Some(location.clone()),
            requested_selection: selection.to_vec(),
            attempt_write_seq: 0,
            first_id: 0,
        };
        let out = self.start_restoration(now, params);
        if out.is_empty() {
            // The store refused the attempt (cannot happen after the gates).
            return self.store.end_restoration(now, &self.tracker);
        }
        out
    }

    /// Ends the candidate `win` without restoring (claimed or closed).
    fn drop_candidate(&mut self, now: Millis, win: WinKey) -> Vec<Command> {
        if self.trigger.candidate.is_some_and(|c| c.win == win) {
            self.trigger.candidate = None;
            return self.store.end_restoration(now, &self.tracker);
        }
        Vec::new()
    }

    fn builtin_context(&self) -> BuiltinContext {
        BuiltinContext {
            persist_browsers: self.environment.options.persist_browsers,
            background_logon: self.background_logon,
            logon_at: self.logon_at,
            last_input_at: self.environment.last_input_at,
            shell_ready_at: self.shell_ready_at,
        }
    }

    // --- control flows (task 11.6) --------------------------------------------

    /// The Builtin_Restore_Option prompt state of this run.
    pub fn prompt_state(&self) -> PromptState {
        self.control.prompt
    }

    /// `ExitRequested` was handled: the Engine only finishes the exit write
    /// (if any) and ignores everything else.
    pub fn is_exiting(&self) -> bool {
        self.control.exit.is_some()
    }

    /// `Exit` was emitted.
    pub fn has_exited(&self) -> bool {
        self.control.exit == Some(ExitState::Exited)
    }

    /// Decides the startup prompt once (Req 10.1, 10.3, 12.3).
    fn decide_prompt(&mut self) -> Vec<Command> {
        if self.control.prompt != PromptState::Undecided {
            return Vec::new();
        }
        if self.environment.options.persist_browsers == Some(true) && !self.settings.builtin_prompt_declined {
            self.control.prompt = PromptState::Shown;
            vec![Command::ShowPrompt]
        } else {
            self.control.prompt = PromptState::Done;
            Vec::new()
        }
    }

    /// Applies and persists a Settings change (Req 12.4). Clearing
    /// `builtin_prompt_declined` re-enables the prompt for the next start
    /// (Req 12.3).
    fn settings_changed(&mut self, settings: &Settings) -> Vec<Command> {
        let old = std::mem::replace(&mut self.settings, settings.clone());
        let mut out = vec![Command::PersistSettings(settings.clone())];
        if old.start_at_sign_in != settings.start_at_sign_in {
            out.push(Command::SetStartAtSignIn(settings.start_at_sign_in));
        }
        out
    }

    /// The user answered the prompt (Req 10.2, 10.3, 10.6).
    fn prompt_answered(&mut self, answer: PromptAnswer) -> Vec<Command> {
        self.control.prompt = PromptState::Done;
        match answer {
            PromptAnswer::Accepted => vec![Command::SetBuiltinRestore(false)],
            PromptAnswer::Declined => {
                self.settings.builtin_prompt_declined = true;
                vec![Command::PersistSettings(self.settings.clone())]
            }
            PromptAnswer::Dismissed => Vec::new(),
        }
    }

    /// The shell carried out `SetBuiltinRestore(enabled)` (Req 10.5).
    fn builtin_restore_set(&mut self, enabled: bool, result: Result<(), IoErrorKind>) -> Vec<Command> {
        match result {
            Ok(()) => {
                self.environment.options.persist_browsers = Some(enabled);
                Vec::new()
            }
            // Only disabling is ever commanded. Nothing is recorded in
            // Settings, so the next start prompts again.
            Err(kind) if !enabled => vec![
                Command::ShowMessage(MessageKind::BuiltinRestoreDisableFailed),
                Command::AppendLog(LogEntry::Error {
                    operation: OP_DISABLE_BUILTIN_RESTORE.to_owned(),
                    description: format!(
                        "{}; the option was left unchanged and the prompt is shown again at the next start",
                        io_description(kind)
                    ),
                }),
            ],
            Err(kind) => vec![Command::AppendLog(LogEntry::Error {
                operation: "enable-builtin-restore".to_owned(),
                description: io_description(kind),
            })],
        }
    }

    /// Exit through Settings (Req 12.5, 12.8, 12.9).
    fn exit_requested(&mut self, now: Millis) -> Vec<Command> {
        let (snapshot, cap_log) = self.tracker.capture();
        if self.tracker.open_window_count() == 0 || snapshot.windows.is_empty() {
            // No window open (or none with a Location yet): the
            // Session_File keeps what it holds.
            self.control.exit = Some(ExitState::Exited);
            return vec![Command::Exit];
        }
        self.trigger.candidate = None;
        let mut out: Vec<Command> = cap_log.map(Command::AppendLog).into_iter().collect();
        let wall = self.environment.wall_clock;
        let save = self.store.session_end(now, wall, SaveCause::ExitThroughSettings, snapshot);
        let seq = save.iter().find_map(|c| match c {
            Command::WriteSessionFile { seq, .. } => Some(*seq),
            _ => None,
        });
        out.extend(save);
        let Some(seq) = seq else {
            // Not expected: the store writes every user-caused Session_End.
            self.control.exit = Some(ExitState::Exited);
            out.push(Command::Exit);
            return out;
        };
        self.trigger.timer_seq += 1;
        let timer = TimerId { owner: TimerOwner::Control, seq: self.trigger.timer_seq };
        self.control.exit = Some(ExitState::Writing { seq, timer });
        out.push(Command::SetTimer { id: timer, at: now + EXIT_WRITE_TIMEOUT_MS });
        out
    }

    /// After `ExitRequested`: only the exit write's completion, its store
    /// retries, and the exit deadline are processed.
    fn exiting_handle(&mut self, now: Millis, event: &Event) -> Vec<Command> {
        let Some(ExitState::Writing { seq, timer }) = self.control.exit else { return Vec::new() };
        match event {
            Event::WriteCompleted { .. } => {
                let mut out = self.store_handle(now, event);
                if self.store.is_durable(seq) {
                    out.push(self.exit_now());
                } else if self.store.retry_due().is_none() && self.store.awaiting_change_to_retry() {
                    out.push(exit_error("every retry of the Session_File write failed; exiting anyway"));
                    out.push(self.exit_now());
                }
                out
            }
            Event::Timer(TimerId { owner: TimerOwner::Store, .. }) => self.store_handle(now, event),
            Event::Timer(id) if *id == timer => vec![
                exit_error(&format!(
                    "the Session_File write was not confirmed within {} s; exiting anyway",
                    EXIT_WRITE_TIMEOUT_MS / 1_000
                )),
                self.exit_now(),
            ],
            _ => Vec::new(),
        }
    }

    fn exit_now(&mut self) -> Command {
        self.control.exit = Some(ExitState::Exited);
        Command::Exit
    }
}

// --- tab folding (task 22.2; see the `Engine` docs, "Tab folding") ----------

/// Whether `op` was issued by the `FoldingEngine` (its ids start at
/// [`FOLD_OP_BASE`]; the restore session counts up from 1 and the deferred
/// tabs from [`DEFERRED_OP_BASE`], below it).
fn is_fold_op(op: OpId) -> bool {
    op.0 >= FOLD_OP_BASE
}

/// The placement a minimized Fold_Target is shown in: its pre-minimize state at
/// the bounds the tracker recorded (Req 14.7, 14.21). `None` for a window that
/// is not minimized or not tracked.
fn pre_minimize_placement(tracker: &LiveSessionTracker, displays: &[Display], win: WinKey) -> Option<FinalPlacement> {
    let placement = tracker.window(win)?.placement();
    let ShowState::Minimized { restore_maximized } = placement.state else { return None };
    let mut shown = resolve_placement(placement, displays, false);
    shown.state = if restore_maximized { ShowState::Maximized } else { ShowState::Normal };
    Some(shown)
}

impl Engine {
    /// The Opening_Order and Folding_Candidates (read-only, for tests and
    /// diagnostics).
    pub fn folding(&self) -> &FoldingEngine {
        &self.fold
    }

    /// Events the `FoldingEngine` takes for itself: the first signal, the
    /// fold's own feedback, and everything that concerns a Folding_Candidate
    /// (Req 2.6, 14.27, 14.28). `None` leaves the event to the other
    /// components (and to [`Engine::fold_shared`]).
    fn fold_owned(&mut self, now: Millis, event: &Event) -> Option<Vec<Command>> {
        let step = match event {
            Event::WindowAppeared { win, pid, bypass_keys, desktop, foreground } => {
                return Some(self.first_signal(now, *win, *pid, *bypass_keys, desktop, *foreground));
            }
            Event::HideFailed { win, error } => self.fold.hide_failed(*win, error),
            // Only a tracked window can be the source of a dragged-out tab.
            Event::TabDetached { from } if self.tracker.window(*from).is_some() => self.fold.tab_detached(now, *from),
            Event::TabDetached { .. } => return Some(Vec::new()),
            Event::TabStarted { op, tab } => self.fold.tab_started(now, *op, *tab),
            Event::FoldContext { probe, desktop, unresponsive } => {
                let tracker = &self.tracker;
                let windows = tracker.fold_windows();
                let tab_of = |win: WinKey, index: usize| tracker.window(win)?.tabs().get(index).map(|t| t.key());
                self.fold.fold_context(now, *probe, desktop.clone(), unresponsive, &windows, &tab_of)
            }
            Event::WindowScanned(obs) => {
                if self.fold.owns(obs.win) {
                    self.candidate_tabs.extend(obs.tabs.iter().map(|t| (t.tab, obs.win)));
                }
                self.fold.window_scanned(obs)?
            }
            Event::InitialLocationKnown { win, tab, location, selection } => {
                if let (true, Some(tab)) = (self.fold.owns(*win), tab) {
                    self.candidate_tabs.insert(*tab, *win);
                }
                self.fold.initial_location_known(*win, location.clone(), selection.clone())?
            }
            Event::TabCreated { win, tab } if self.fold.owns(*win) => {
                self.candidate_tabs.insert(*tab, *win);
                return Some(Vec::new());
            }
            // The candidate's own tab never reaches the tracker, and its
            // activation only if it is left open (replayed at hand-over).
            Event::TabRevoked { win, .. } if self.fold.owns(*win) => {
                return Some(Vec::new());
            }
            Event::WindowActivated { win } if self.fold.owns(*win) => {
                self.fold.window_activated(*win);
                return Some(Vec::new());
            }
            Event::NavigationCompleted { tab, .. } if self.candidate_tabs.contains_key(tab) => {
                return Some(Vec::new());
            }
            Event::WindowDestroyed { win } => self.fold.window_destroyed(now, *win)?,
            Event::ActivationConfirmed { op } if is_fold_op(*op) => {
                let (tracker, displays) = (&self.tracker, &self.environment.displays);
                let placement_of = |win| pre_minimize_placement(tracker, displays, win);
                self.fold.activation_confirmed(now, *op, &placement_of)
            }
            Event::Timer(id) if id.owner == TimerOwner::Fold => {
                let (tracker, displays) = (&self.tracker, &self.environment.displays);
                let placement_of = |win| pre_minimize_placement(tracker, displays, win);
                self.fold.timer(now, *id, &placement_of)
            }
            _ => return None,
        };
        Some(self.apply_fold_step(now, step))
    }

    /// Events that other components handled and the `FoldingEngine` needs as
    /// well, because they concern a Fold_Target or a Folded_Tab: the
    /// registration and navigation of a Folded_Tab, the revocation of a
    /// Matching_Tab, and the destruction of a Fold_Target or the window that
    /// held a Matching_Tab. A destroyed window also leaves the Opening_Order
    /// (Req 14.8).
    fn fold_shared(&mut self, now: Millis, event: &Event) -> Vec<Command> {
        let step = match event {
            Event::TabCreated { win, tab } => self.fold.tab_created(now, *win, *tab),
            Event::NavigationCompleted { tab, location } => {
                let (tracker, displays) = (&self.tracker, &self.environment.displays);
                let placement_of = |win| pre_minimize_placement(tracker, displays, win);
                self.fold.navigation_completed(now, *tab, location, &placement_of)
            }
            Event::TabRevoked { win, tab } => self.fold.tab_revoked(now, *win, *tab),
            Event::WindowDestroyed { win } => {
                self.fold.opening_mut().remove(*win);
                self.fold.target_destroyed(now, *win)
            }
            // Turning folding off leaves every candidate open; turning it on
            // affects only later windows (Req 12.10, 14.3, 14.14).
            Event::SettingsChanged(settings) => self.fold.settings_changed(now, settings.tab_folding_enabled),
            // A candidate that is left open later still ranks behind a window
            // activated after it (see `hand_over`).
            Event::WindowActivated { win } => {
                self.fold.window_activated(*win);
                return Vec::new();
            }
            _ => return Vec::new(),
        };
        self.apply_fold_step(now, step)
    }

    /// `ProcessExited` drops the candidates of that process, `ShellLost` all
    /// of them, each with `ERROR operation=fold-shell`; nothing is replayed to
    /// the tracker (Req 14.14; see [`FoldingEngine::process_exited`] and
    /// [`FoldingEngine::shell_lost`]). Runs before the other components see
    /// the event, so that the tracker is told how many candidates remain
    /// before it decides on a Session_End (Req 14.15, 2.7). A candidate
    /// revealed because the shell was lost is marked as seen by trigger
    /// detection: it is never a Trigger_Window (Req 14.10, 4.2).
    fn fold_shell_gone(&mut self, now: Millis, event: &Event) -> Vec<Command> {
        let (step, revealed) = match event {
            Event::ProcessExited { pid } => {
                let wins: Vec<WinKey> =
                    self.candidate_pids.iter().filter(|(_, p)| *p == pid).map(|(&win, _)| win).collect();
                (self.fold.process_exited(now, &wins), Vec::new())
            }
            Event::ShellLost => {
                let before: Vec<WinKey> = self.candidate_pids.keys().copied().collect();
                (self.fold.shell_lost(), before)
            }
            _ => return Vec::new(),
        };
        for win in revealed {
            if !self.fold.owns(win) {
                self.trigger.seen.insert(win);
            }
        }
        self.apply_fold_step(now, step)
    }

    /// The first signal of a window (Req 14.2-14.4, 14.10): the
    /// `FoldingEngine` classifies it with what only the Engine knows.
    fn first_signal(
        &mut self,
        now: Millis,
        win: WinKey,
        pid: u32,
        bypass_keys: bool,
        desktop: &DesktopView,
        foreground: Option<WinKey>,
    ) -> Vec<Command> {
        let tracked = self.tracker.open_window_keys();
        // A restoration is in progress, a Trigger_Window candidate waits for
        // its restoration to start, or `OpenWindow` requests are unclaimed
        // (Req 14.3 c, d).
        let restoration_active = self.trigger.candidate.is_some()
            || self.restore.as_ref().is_some_and(|s| s.has_pending_requests() || !s.is_finished());
        let env = FirstSignalEnv {
            tracked: &tracked,
            restoration_active,
            builtin_restore: is_builtin_restore(&self.builtin_context(), now),
        };
        let commands = self.fold.window_appeared(now, win, bypass_keys, desktop.clone(), foreground, &env).commands;
        if self.fold.owns(win) {
            self.candidate_pids.insert(win, pid);
        }
        commands
    }

    /// Carries out a step of the `FoldingEngine`: its commands, then the
    /// hand-over of every window it gave up.
    fn apply_fold_step(&mut self, now: Millis, step: FoldStep) -> Vec<Command> {
        let mut out = step.commands;
        for released in step.released {
            out.extend(self.hand_over(now, released));
        }
        let fold = &self.fold;
        self.candidate_tabs.retain(|_, win| fold.owns(*win));
        self.candidate_pids.retain(|win, _| fold.owns(*win));
        out
    }

    /// A window the `FoldingEngine` gave up becomes an ordinary window: it is
    /// marked as seen by trigger detection first (never a Trigger_Window,
    /// Req 14.10, 4.2), its buffered last scan, `InitialLocationKnown` and
    /// activation (with the later activations of tracked windows, so that the
    /// Activation_Order is what Explorer's was) go to the other components as
    /// if they had just arrived, and a rescan
    /// makes sure the Live_Session holds it (Req 14.15).
    fn hand_over(&mut self, now: Millis, released: Released) -> Vec<Command> {
        self.trigger.seen.insert(released.win);
        let mut out = Vec::new();
        for event in &released.buffered {
            // After the candidate's own activation come those of the windows
            // activated since, which keep their rank ahead of it. A candidate
            // still in progress is left out: the tracker must not know it
            // (Req 2.6); its own activation is replayed when it is handed over.
            if let Event::WindowActivated { win } = event
                && self.fold.owns(*win)
            {
                continue;
            }
            out.extend(self.components(now, route(event), event));
        }
        out.push(Command::RescanWindow(released.win));
        out
    }
}

fn exit_error(description: &str) -> Command {
    Command::AppendLog(LogEntry::Error { operation: OP_EXIT.to_owned(), description: description.to_owned() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DisplayRef, LocationKind, Rect, ShowState, Volume};

    fn win(n: u64) -> WinKey {
        WinKey { hwnd: n, seq: n }
    }

    fn tab(n: u64) -> TabKey {
        TabKey { hwnd: 100 + n, seq: n }
    }

    fn loc() -> LocationId {
        LocationId {
            case_sensitive: false,
            kind: LocationKind::FileSystem(Volume::Fixed),
            parsing_name: r"C:\".into(),
            display_name: "C:".into(),
            pidl: vec![],
            search: None,
        }
    }

    fn placement() -> Placement {
        let r = Rect { left: 0, top: 0, right: 800, bottom: 600 };
        Placement {
            state: ShowState::Normal,
            restored: r,
            display: DisplayRef { device: r"\\.\DISPLAY1".into(), monitor: r },
        }
    }

    fn startup() -> StartupInput {
        StartupInput {
            session_file: SessionFileRead::Absent,
            environment: Environment::default(),
            background_logon: true,
            logon_at: Some(Millis(5)),
            shell_ready_at: None,
        }
    }

    /// One event of every variant.
    fn all_events() -> Vec<Event> {
        let timer = |owner| Event::Timer(TimerId { owner, seq: 1 });
        vec![
            Event::WindowScanned(WindowObservation {
                win: win(1),
                pid: 42,
                tabs: vec![TabObservation { tab: tab(1), location: ObservedLocation::Known(loc()) }],
                active: Some(tab(1)),
                strip: None,
                placement: placement(),
            }),
            Event::TabRevoked { win: win(1), tab: tab(1) },
            Event::WindowDestroyed { win: win(1) },
            Event::WindowActivated { win: win(1) },
            Event::ProcessExited { pid: 42 },
            Event::ShellLost,
            Event::ShellAvailable,
            Event::EndSessionQuery(EndKind::Restart),
            Event::EndSessionCancelled,
            Event::ExitRequested,
            Event::SettingsChanged(Settings::default()),
            Event::PromptAnswered(PromptAnswer::Dismissed),
            Event::BuiltinRestoreSet { enabled: false, result: Ok(()) },
            Event::EnvironmentChanged(Environment::default()),
            Event::InitialLocationKnown { win: win(1), tab: None, location: loc(), selection: vec![] },
            Event::WindowOpened { request: RequestId(1), win: win(2), tab: None },
            Event::TabCreated { win: win(2), tab: tab(2) },
            Event::NavigationCompleted { tab: tab(2), location: loc() },
            Event::ProbeCompleted { probe: ProbeId(1), outcome: ProbeOutcome::Available },
            Event::OperationFailed {
                op: OpId(1),
                error: ShellError { code: -1, message: "x".into() },
            },
            Event::ActivationConfirmed { op: OpId(1) },
            Event::WindowAppeared {
                win: win(3),
                pid: 42,
                bypass_keys: false,
                desktop: DesktopView::Known(BTreeSet::from([win(1), win(2)])),
                foreground: Some(win(1)),
            },
            Event::WindowAppeared {
                win: win(3),
                pid: 42,
                bypass_keys: true,
                desktop: DesktopView::Unknown,
                foreground: None,
            },
            Event::TabDetached { from: win(1) },
            Event::TabStarted { op: OpId(2), tab: tab(3) },
            Event::FoldContext {
                probe: FoldProbeId(1),
                desktop: DesktopView::Unknown,
                unresponsive: BTreeSet::from([win(2)]),
            },
            Event::HideFailed { win: win(3), error: ShellError { code: -2, message: "y".into() } },
            Event::WriteCompleted { seq: 1, result: Err(IoErrorKind::DiskFull) },
            timer(TimerOwner::Tracker),
            timer(TimerOwner::Store),
            timer(TimerOwner::Restore),
            timer(TimerOwner::Control),
            timer(TimerOwner::Fold),
        ]
    }

    #[test]
    fn millis_arithmetic_saturates() {
        assert_eq!(Millis(10) + 5, Millis(15));
        assert_eq!(Millis(u64::MAX) + 1, Millis(u64::MAX));
        assert_eq!(Millis(15).since(Millis(10)), 5);
        assert_eq!(Millis(10).since(Millis(15)), 0);
    }

    #[test]
    fn every_event_reaches_at_least_one_component() {
        for e in all_events() {
            assert_ne!(route(&e), Route::default(), "{e:?} is routed nowhere");
        }
    }

    #[test]
    fn timers_go_only_to_their_owner() {
        let only = |owner| route(&Event::Timer(TimerId { owner, seq: 7 }));
        assert_eq!(only(TimerOwner::Tracker), Route { tracker: true, ..Route::default() });
        assert_eq!(only(TimerOwner::Store), Route { store: true, ..Route::default() });
        assert_eq!(only(TimerOwner::Restore), Route { restore: true, ..Route::default() });
        assert_eq!(only(TimerOwner::Control), Route { control: true, ..Route::default() });
        assert_eq!(only(TimerOwner::Fold), Route { fold: true, ..Route::default() });
    }

    #[test]
    fn key_routes() {
        let scan = route(&all_events()[0]);
        assert!(scan.tracker && scan.restore && scan.control && !scan.store);
        let end = route(&Event::EndSessionQuery(EndKind::SignOut));
        assert!(end.tracker && end.store);
        let write = route(&Event::WriteCompleted { seq: 0, result: Ok(()) });
        assert!(write.store && write.restore && write.control && !write.tracker);
        let set = route(&Event::BuiltinRestoreSet { enabled: false, result: Ok(()) });
        assert_eq!(set, Route { control: true, ..Route::default() });
        let nav = route(&Event::NavigationCompleted { tab: tab(1), location: loc() });
        assert!(nav.tracker && nav.restore);
    }

    #[test]
    fn skeleton_emits_no_commands() {
        let (mut engine, cmds) = Engine::new(Settings::default(), startup());
        assert!(cmds.is_empty());
        assert!(engine.background_logon());
        assert_eq!(engine.logon_at(), Some(Millis(5)));
        assert_eq!(engine.startup_session_file(), &SessionFileRead::Absent);
        // Only the tracker's tombstone timer (task 7.5), full rescans
        // (task 7.6), and the store's checkpoint and retry timers, Session_End
        // write, SAVE entry, and write-failure ERROR entry (task 8.3), and the
        // attempt write of a restoration that `InitialLocationKnown` starts
        // after the Session_End (task 11.5), and the `SettingsChanged`
        // persistence (task 11.6) so far. `ExitRequested` ends the
        // run and is tested separately (task 11.6).
        let events = all_events().into_iter().filter(|e| *e != Event::ExitRequested);
        for (i, e) in events.enumerate() {
            let cmds = engine.handle(Millis(i as u64), e);
            assert!(
                cmds.iter().all(|c| matches!(c, Command::RescanAll)
                    || matches!(c, Command::PersistSettings(_))
                    || matches!(c, Command::WriteSessionFile { .. })
                    || matches!(c, Command::AppendLog(LogEntry::Save { .. } | LogEntry::Error { .. }))
                    || matches!(c, Command::SetTimer { id, .. }
                        if id.owner == TimerOwner::Tracker || id.owner == TimerOwner::Store)),
                "{cmds:?}"
            );
        }
    }

    #[test]
    fn tracker_receives_tracking_events() {
        let (mut engine, _) = Engine::new(Settings::default(), startup());
        let scan = all_events().remove(0);
        let cmds = engine.handle(Millis(1), scan);
        // The change schedules a checkpoint.
        assert!(
            matches!(cmds.as_slice(), [Command::SetTimer { id, at: Millis(501) }] if id.owner == TimerOwner::Store),
            "{cmds:?}"
        );
        assert_eq!(engine.tracker().window_count(), 1);
        assert_eq!(engine.tracker().capture().0.tab_count(), 1);
        engine.handle(Millis(2), Event::WindowDestroyed { win: win(1) });
        assert!(engine.tracker().is_empty());
    }

    #[test]
    fn store_checkpoints_and_saves_tracker_output() {
        let wall = WallClock {
            at: Millis(0),
            local: crate::model::Timestamp {
                year: 2025,
                month: 1,
                day: 31,
                hour: 9,
                minute: 0,
                second: 0,
                millis: 0,
                offset_minutes: 60,
            },
        };
        let mut start = startup();
        start.environment.wall_clock = Some(wall);
        let (mut engine, _) = Engine::new(Settings::default(), start);
        let cmds = engine.handle(Millis(1), all_events().remove(0));
        let Some(Command::SetTimer { id, at }) = cmds.into_iter().next() else { panic!() };
        let cmds = engine.handle(at, Event::Timer(id));
        let Some(Command::WriteSessionFile { seq, text, urgent: false }) = cmds.into_iter().next() else {
            panic!()
        };
        let doc = crate::format::parse(text.as_bytes()).unwrap();
        assert_eq!(doc.cause, crate::model::SaveCause::Checkpoint);
        assert_eq!(doc.saved_at, wall.local_at(at));
        engine.handle(Millis(600), Event::WriteCompleted { seq, result: Ok(()) });
        assert_eq!(engine.store().durable().doc(), Some(&doc));

        // Closing the last window saves it urgently and logs SAVE.
        let cmds = engine.handle(Millis(700), Event::WindowDestroyed { win: win(1) });
        assert!(cmds.iter().any(|c| matches!(c, Command::WriteSessionFile { urgent: true, .. })), "{cmds:?}");
        assert!(cmds.contains(&Command::AppendLog(LogEntry::Save {
            cause: crate::model::SaveCause::LastWindowClosed,
            windows: 1,
            tabs: 1,
        })));
        let pending = engine.store().pending_snapshot().unwrap();
        assert_eq!(pending.cause, crate::model::SaveCause::LastWindowClosed);
        assert_eq!(pending.failed_attempts, 0);
    }

    #[test]
    fn control_tracks_settings_environment_and_shell_ready() {
        let (mut engine, _) = Engine::new(Settings::default(), startup());
        let s = Settings { restoration_enabled: false, ..Settings::default() };
        engine.handle(Millis(1), Event::SettingsChanged(s.clone()));
        assert_eq!(engine.settings(), &s);

        let env = Environment {
            displays: vec![],
            options: ExplorerOptionValues {
                persist_browsers: Some(true),
                launch_to: Some(1),
                separate_process: None,
            },
            last_input_at: Some(Millis(3)),
            wall_clock: None,
            launch_to_context: LaunchToContext::default(),
            placeholder_dir: None,
        };
        engine.handle(Millis(2), Event::EnvironmentChanged(env.clone()));
        assert_eq!(engine.environment(), &env);

        assert_eq!(engine.shell_ready_at(), None);
        engine.handle(Millis(9), Event::ShellAvailable);
        assert_eq!(engine.shell_ready_at(), Some(Millis(9)));
    }

    // --- trigger detection (task 11.5) ----------------------------------------

    fn pending_startup(failed_attempts: u8) -> StartupInput {
        use crate::model::{SaveCause, SessionDoc, Snapshot, Status, TabRec, Timestamp, WindowId, WindowRec};
        let snapshot = Snapshot {
            windows: vec![WindowRec { id: WindowId(0), tabs: vec![TabRec { location: loc() }], active: 0, placement: placement() }],
            activation: vec![WindowId(0)],
        };
        let doc = SessionDoc {
            version: crate::format::FORMAT_VERSION,
            status: Status::Pending,
            failed_attempts,
            saved_at: Timestamp { year: 2025, month: 1, day: 2, hour: 3, minute: 4, second: 5, millis: 6, offset_minutes: 0 },
            cause: SaveCause::LastWindowClosed,
            snapshot,
        };
        StartupInput {
            session_file: SessionFileRead::Content(crate::format::serialize(&doc).into_bytes()),
            background_logon: false,
            ..startup()
        }
    }

    fn scan(n: u64) -> Event {
        Event::WindowScanned(WindowObservation {
            win: win(n),
            pid: 42,
            tabs: vec![TabObservation { tab: tab(n), location: ObservedLocation::Known(loc()) }],
            active: Some(tab(n)),
            strip: None,
            placement: placement(),
        })
    }

    fn initial(n: u64) -> Event {
        Event::InitialLocationKnown { win: win(n), tab: None, location: loc(), selection: vec![] }
    }

    fn attempt_writes(cmds: &[Command]) -> usize {
        cmds.iter()
            .filter(|c| matches!(c, Command::WriteSessionFile { text, urgent: true, .. } if text.contains("failed-attempts 1")))
            .count()
    }

    fn control_timer(cmds: &[Command]) -> Option<(TimerId, Millis)> {
        cmds.iter().find_map(|c| match c {
            Command::SetTimer { id, at } if id.owner == TimerOwner::Control => Some((*id, *at)),
            _ => None,
        })
    }

    #[test]
    fn scanned_candidate_holds_checkpoints_until_its_initial_location_starts_the_restoration() {
        let (mut engine, _) = Engine::new(Settings::default(), pending_startup(0));
        let cmds = engine.handle(Millis(100), scan(1));
        // Step 7: wait â‰¤ 2 s, checkpoints held back.
        assert_eq!(control_timer(&cmds).map(|t| t.1), Some(Millis(100 + INITIAL_LOCATION_WAIT_MS)), "{cmds:?}");
        assert_eq!(engine.trigger_candidate(), Some(win(1)));
        assert!(engine.store().is_restoring());
        assert_eq!(engine.store().checkpoint_due(), None);
        assert!(engine.restore_session().is_none());

        // A rescan is not a second sighting.
        assert!(control_timer(&engine.handle(Millis(500), scan(1))).is_none());

        let cmds = engine.handle(Millis(1_100), initial(1));
        assert_eq!(attempt_writes(&cmds), 1, "{cmds:?}");
        assert_eq!(engine.trigger_candidate(), None);
        let session = engine.restore_session().expect("started");
        assert_eq!(session.trigger(), win(1));
        // Later events for the trigger start nothing new.
        let cmds = engine.handle(Millis(1_200), initial(1));
        assert_eq!(attempt_writes(&cmds), 0);
        assert!(engine.store().is_attempting());
    }

    #[test]
    fn candidate_without_initial_location_is_not_a_trigger_after_2_s() {
        let (mut engine, _) = Engine::new(Settings::default(), pending_startup(0));
        let (id, at) = control_timer(&engine.handle(Millis(100), scan(1))).unwrap();
        let cmds = engine.handle(at, Event::Timer(id));
        assert_eq!(engine.trigger_candidate(), None);
        assert!(!engine.store().is_restoring());
        // Checkpointing resumes with the held-back change.
        assert!(cmds.iter().any(|c| matches!(c, Command::SetTimer { id, .. } if id.owner == TimerOwner::Store)), "{cmds:?}");
        // A late initial Location starts nothing.
        let cmds = engine.handle(at + 10, initial(1));
        assert_eq!(attempt_writes(&cmds), 0);
        assert!(engine.restore_session().is_none());
        assert!(engine.store().pending_snapshot().is_some());
    }

    #[test]
    fn closed_or_claimed_candidates_end_without_restoring() {
        let (mut engine, _) = Engine::new(Settings::default(), pending_startup(0));
        engine.handle(Millis(1), scan(1));
        engine.handle(Millis(2), Event::WindowDestroyed { win: win(1) });
        assert_eq!(engine.trigger_candidate(), None);
        assert!(!engine.store().is_restoring());

        engine.handle(Millis(3), scan(2));
        assert_eq!(engine.trigger_candidate(), Some(win(2)));
        engine.handle(Millis(4), Event::WindowOpened { request: RequestId(9), win: win(2), tab: None });
        assert_eq!(engine.trigger_candidate(), None);
        assert_eq!(attempt_writes(&engine.handle(Millis(5), initial(2))), 0);
        assert!(engine.restore_session().is_none());
    }

    #[test]
    fn window_opening_while_another_is_open_is_not_a_trigger() {
        let (mut engine, _) = Engine::new(Settings::default(), pending_startup(0));
        let (id, at) = control_timer(&engine.handle(Millis(1), scan(1))).unwrap();
        // Window 2 opens while window 1 (the candidate) is open.
        let cmds = engine.handle(Millis(2), initial(2));
        assert!(cmds.is_empty(), "{cmds:?}");
        assert_eq!(engine.trigger_candidate(), Some(win(1)));
        assert!(engine.handle(Millis(3), scan(2)).iter().all(|c| control_timer(std::slice::from_ref(c)).is_none()));
        // Window 1 times out; window 3 opens while windows 1 and 2 are open.
        engine.handle(at, Event::Timer(id));
        let cmds = engine.handle(at + 1, initial(3));
        assert!(cmds.is_empty(), "{cmds:?}");
        // A window activated but not scanned yet also counts as open.
        let (mut engine, _) = Engine::new(Settings::default(), pending_startup(0));
        engine.handle(Millis(1), Event::WindowActivated { win: win(1) });
        assert!(engine.handle(Millis(2), initial(2)).is_empty());
        assert!(engine.restore_session().is_none());
        assert!(engine.store().pending_snapshot().is_some_and(|d| d.failed_attempts == 0));
    }

    #[test]
    fn restoration_in_progress_blocks_a_new_trigger() {
        let (mut engine, _) = Engine::new(Settings::default(), pending_startup(0));
        // Announced before any scan: the restoration starts at once.
        let cmds = engine.handle(Millis(1), initial(1));
        assert_eq!(attempt_writes(&cmds), 1);
        assert!(control_timer(&cmds).is_none());
        // The tracker knows no window yet, but a restoration is in progress.
        let cmds = engine.handle(Millis(2), initial(2));
        assert!(cmds.is_empty(), "{cmds:?}");
        assert_eq!(engine.restore_session().unwrap().trigger(), win(1));
    }

    #[test]
    fn gates_run_in_design_order() {
        let builtin_env = Environment {
            options: ExplorerOptionValues { persist_browsers: Some(true), ..ExplorerOptionValues::default() },
            ..Environment::default()
        };
        let builtin = |mut s: StartupInput| {
            s.environment = builtin_env.clone();
            s.background_logon = true;
            s.logon_at = Some(Millis(0));
            s.shell_ready_at = Some(Millis(0));
            s
        };
        let consumed = |cmds: &[Command]| {
            cmds.iter().any(|c| matches!(c, Command::WriteSessionFile { text, urgent: true, .. } if text.contains("status consumed")))
        };
        let disabled = Settings { restoration_enabled: false, ..Settings::default() };

        // 3 before 4: BuiltinRestore is logged without a Pending_Snapshot.
        let (mut e, _) = Engine::new(Settings::default(), builtin(startup()));
        let cmds = e.handle(Millis(10), initial(1));
        assert_eq!(cmds, vec![Command::AppendLog(LogEntry::BuiltinRestoreDetected)]);

        // 3 before 5: consumed and logged once, not restored.
        let (mut e, _) = Engine::new(disabled.clone(), builtin(pending_startup(0)));
        let cmds = e.handle(Millis(10), initial(1));
        assert!(consumed(&cmds) && cmds.contains(&Command::AppendLog(LogEntry::BuiltinRestoreDetected)), "{cmds:?}");
        assert!(e.store().pending_snapshot().is_none() && e.restore_session().is_none());
        e.handle(Millis(11), scan(1));
        assert!(e.handle(Millis(12), initial(2)).is_empty());

        // 4: nothing.
        let (mut e, _) = Engine::new(disabled.clone(), startup());
        assert!(e.handle(Millis(10), initial(1)).is_empty());

        // 5: disabled consumes without a log entry.
        let (mut e, _) = Engine::new(disabled, pending_startup(2));
        let cmds = e.handle(Millis(10), scan(1));
        assert!(consumed(&cmds), "{cmds:?}");
        assert!(!cmds.iter().any(|c| matches!(c, Command::AppendLog(_) | Command::SetTimer { id: TimerId { owner: TimerOwner::Control, .. }, .. })));
        assert!(e.store().pending_snapshot().is_none() && e.restore_session().is_none());

        // 6 is normally reached with nothing left to do: the store abandons
        // an exhausted snapshot at startup (and at the third failure).
        let (mut e, cmds) = Engine::new(Settings::default(), pending_startup(MAX_FAILED_ATTEMPTS));
        assert!(cmds.contains(&Command::AppendLog(LogEntry::Abandoned)));
        assert!(e.handle(Millis(10), initial(1)).is_empty());

        // A protected retry snapshot (2 failed attempts) is restored.
        let (mut e, _) = Engine::new(Settings::default(), pending_startup(2));
        let cmds = e.handle(Millis(10), initial(1));
        assert!(cmds.iter().any(|c| matches!(c, Command::WriteSessionFile { text, .. } if text.contains("failed-attempts 3"))));
        assert!(e.restore_session().is_some());
    }

    /// Task 16.6 regression (Req 10.4). Explorer's logon restore opens its
    /// windows within a fraction of a second. On Windows 11 22621 the
    /// windows activated one another before either was scanned (A
    /// activated, B activated, A activated again, then both
    /// `InitialLocationKnown`), so neither was a Trigger_Window candidate and
    /// the Pending_Snapshot was never consumed; a checkpoint replaced it.
    #[test]
    fn builtin_restore_of_windows_seen_together_consumes_once() {
        let builtin = StartupInput {
            environment: persist_browsers(Some(true)),
            background_logon: true,
            logon_at: Some(Millis(0)),
            shell_ready_at: Some(Millis(0)),
            ..pending_startup(0)
        };
        let activated = |n| Event::WindowActivated { win: win(n) };
        let consumed = |cmds: &[Command]| {
            cmds.iter()
                .filter(|c| matches!(c, Command::WriteSessionFile { text, urgent: true, .. } if text.contains("status consumed")))
                .count()
        };
        let detected = |cmds: &[Command]| {
            cmds.iter().filter(|c| **c == Command::AppendLog(LogEntry::BuiltinRestoreDetected)).count()
        };
        let play = |startup: &StartupInput, events: &[Event]| {
            let (mut e, _) = Engine::new(declined(), startup.clone());
            let mut cmds = Vec::new();
            for (i, ev) in events.iter().enumerate() {
                cmds.extend(e.handle(Millis(100 + i as u64 * 50), ev.clone()));
            }
            (e, cmds)
        };
        let sequences: [Vec<Event>; 3] = [
            // As observed: activations interleave before the first sightings.
            vec![activated(1), activated(2), activated(1), initial(1), initial(2), scan(1), scan(2)],
            // The second window activates before the first one's scan.
            vec![activated(2), scan(1), scan(2), initial(2)],
            // Both already open when the monitor attaches (scans only).
            vec![scan(1), scan(2), scan(1)],
        ];
        for events in sequences {
            let (e, cmds) = play(&builtin, &events);
            assert_eq!((consumed(&cmds), detected(&cmds)), (1, 1), "{events:?}: {cmds:?}");
            assert!(e.store().pending_snapshot().is_none(), "{events:?}");
            assert!(e.restore_session().is_none() && e.trigger_candidate().is_none(), "{events:?}");
            assert_eq!(attempt_writes(&cmds), 0, "{events:?}");
        }

        // Without the logon-restore conditions, windows that activate one
        // another are simply not triggers (Req 4.2): nothing is consumed.
        let (e, cmds) = play(&pending_startup(0), &[activated(1), activated(2), activated(1), initial(1), initial(2)]);
        assert_eq!((consumed(&cmds), detected(&cmds)), (0, 0), "{cmds:?}");
        assert!(e.store().pending_snapshot().is_some() && e.restore_session().is_none());
    }

    // --- control flows (task 11.6) --------------------------------------------

    fn persist_browsers(value: Option<bool>) -> Environment {
        Environment {
            options: ExplorerOptionValues { persist_browsers: value, ..ExplorerOptionValues::default() },
            ..Environment::default()
        }
    }

    fn with_env(env: Environment) -> StartupInput {
        StartupInput { environment: env, ..startup() }
    }

    fn declined() -> Settings {
        Settings { builtin_prompt_declined: true, ..Settings::default() }
    }

    fn exit_write(cmds: &[Command]) -> Option<(u64, crate::model::SessionDoc)> {
        cmds.iter().find_map(|c| match c {
            Command::WriteSessionFile { seq, text, urgent: true } => {
                Some((*seq, crate::format::parse(text.as_bytes()).unwrap()))
            }
            _ => None,
        })
    }

    #[test]
    fn startup_prompt_shows_once_when_persist_browsers_is_on_and_not_declined() {
        let (e, cmds) = Engine::new(Settings::default(), with_env(persist_browsers(Some(true))));
        assert_eq!(cmds, vec![Command::ShowPrompt]);
        assert_eq!(e.prompt_state(), PromptState::Shown);

        for (settings, value) in [(declined(), Some(true)), (Settings::default(), Some(false))] {
            let (e, cmds) = Engine::new(settings, with_env(persist_browsers(value)));
            assert!(!cmds.contains(&Command::ShowPrompt), "{cmds:?}");
            assert_eq!(e.prompt_state(), PromptState::Done);
        }

        // Unknown at startup: decided at the first EnvironmentChanged, once.
        let (mut e, cmds) = Engine::new(Settings::default(), startup());
        assert!(cmds.is_empty());
        assert_eq!(e.prompt_state(), PromptState::Undecided);
        let env = persist_browsers(Some(true));
        assert_eq!(e.handle(Millis(1), Event::EnvironmentChanged(env.clone())), vec![Command::ShowPrompt]);
        assert!(e.handle(Millis(2), Event::EnvironmentChanged(env.clone())).is_empty());
        // Answered: still no second prompt in this run.
        e.handle(Millis(3), Event::PromptAnswered(PromptAnswer::Dismissed));
        assert!(e.handle(Millis(4), Event::EnvironmentChanged(env)).is_empty());
    }

    #[test]
    fn accepted_prompt_disables_the_builtin_option() {
        let (mut e, _) = Engine::new(Settings::default(), with_env(persist_browsers(Some(true))));
        let cmds = e.handle(Millis(1), Event::PromptAnswered(PromptAnswer::Accepted));
        assert_eq!(cmds, vec![Command::SetBuiltinRestore(false)]);
        assert_eq!(e.prompt_state(), PromptState::Done);
        let cmds = e.handle(Millis(2), Event::BuiltinRestoreSet { enabled: false, result: Ok(()) });
        assert!(cmds.is_empty(), "{cmds:?}");
        assert_eq!(e.environment().options.persist_browsers, Some(false));
        assert_eq!(e.settings(), &Settings::default());
    }

    #[test]
    fn failed_disable_shows_a_message_logs_and_records_nothing() {
        let (mut e, _) = Engine::new(Settings::default(), with_env(persist_browsers(Some(true))));
        e.handle(Millis(1), Event::PromptAnswered(PromptAnswer::Accepted));
        let cmds =
            e.handle(Millis(2), Event::BuiltinRestoreSet { enabled: false, result: Err(IoErrorKind::AccessDenied) });
        assert_eq!(cmds.len(), 2, "{cmds:?}");
        assert_eq!(cmds[0], Command::ShowMessage(MessageKind::BuiltinRestoreDisableFailed));
        assert!(
            matches!(&cmds[1], Command::AppendLog(LogEntry::Error { operation, .. }) if operation == OP_DISABLE_BUILTIN_RESTORE)
        );
        // The option stays as it was and no decision is recorded, so the
        // next start prompts again (Req 10.5).
        assert_eq!(e.environment().options.persist_browsers, Some(true));
        assert_eq!(e.settings(), &Settings::default());
        let (_, cmds) = Engine::new(e.settings().clone(), with_env(persist_browsers(Some(true))));
        assert_eq!(cmds, vec![Command::ShowPrompt]);
    }

    #[test]
    fn declined_prompt_is_recorded_and_dismissed_prompt_is_not() {
        let (mut e, _) = Engine::new(Settings::default(), with_env(persist_browsers(Some(true))));
        let cmds = e.handle(Millis(1), Event::PromptAnswered(PromptAnswer::Declined));
        assert_eq!(cmds, vec![Command::PersistSettings(declined())]);
        assert_eq!(e.settings(), &declined());
        let (_, cmds) = Engine::new(e.settings().clone(), with_env(persist_browsers(Some(true))));
        assert!(cmds.is_empty(), "no prompt after a decline: {cmds:?}");

        let (mut e, _) = Engine::new(Settings::default(), with_env(persist_browsers(Some(true))));
        assert!(e.handle(Millis(1), Event::PromptAnswered(PromptAnswer::Dismissed)).is_empty());
        assert_eq!(e.settings(), &Settings::default());
        assert_eq!(e.environment().options.persist_browsers, Some(true));
        let (_, cmds) = Engine::new(e.settings().clone(), with_env(persist_browsers(Some(true))));
        assert_eq!(cmds, vec![Command::ShowPrompt]);
    }

    #[test]
    fn settings_changes_apply_and_persist_at_once() {
        let (mut e, _) = Engine::new(Settings::default(), startup());
        let off = Settings { start_at_sign_in: false, ..Settings::default() };
        assert_eq!(
            e.handle(Millis(1), Event::SettingsChanged(off.clone())),
            vec![Command::PersistSettings(off.clone()), Command::SetStartAtSignIn(false)]
        );
        assert_eq!(e.settings(), &off);
        // Unchanged start-at-sign-in: persisted only.
        let both_off = Settings { restoration_enabled: false, ..off };
        assert_eq!(
            e.handle(Millis(2), Event::SettingsChanged(both_off.clone())),
            vec![Command::PersistSettings(both_off.clone())]
        );
        assert_eq!(e.settings(), &both_off);
    }

    #[test]
    fn re_enabling_the_prompt_clears_the_decline_for_the_next_start() {
        let (mut e, cmds) = Engine::new(declined(), with_env(persist_browsers(Some(true))));
        assert!(cmds.is_empty());
        let cmds = e.handle(Millis(1), Event::SettingsChanged(Settings::default()));
        // Not shown again in this run (Req 12.3: at the next start).
        assert_eq!(cmds, vec![Command::PersistSettings(Settings::default())]);
        assert!(!e.settings().builtin_prompt_declined);
        let (_, cmds) = Engine::new(e.settings().clone(), with_env(persist_browsers(Some(true))));
        assert_eq!(cmds, vec![Command::ShowPrompt]);
    }

    #[test]
    fn tracking_and_saving_continue_while_the_prompt_is_shown() {
        let (mut e, cmds) = Engine::new(Settings::default(), with_env(persist_browsers(Some(true))));
        assert_eq!(cmds, vec![Command::ShowPrompt]);
        let cmds = e.handle(Millis(1), scan(1));
        assert!(cmds.iter().any(|c| matches!(c, Command::SetTimer { id, .. } if id.owner == TimerOwner::Store)));
        assert_eq!(e.tracker().window_count(), 1);
        let cmds = e.handle(Millis(2), Event::WindowDestroyed { win: win(1) });
        assert!(exit_write(&cmds).is_some_and(|(_, d)| d.cause == SaveCause::LastWindowClosed), "{cmds:?}");
        assert_eq!(e.prompt_state(), PromptState::Shown);
    }

    #[test]
    fn session_ends_are_saved_while_restoration_is_disabled() {
        let disabled = Settings { restoration_enabled: false, ..Settings::default() };
        let (mut e, _) = Engine::new(disabled, startup());
        e.handle(Millis(1), scan(1));
        let cmds = e.handle(Millis(2), Event::WindowDestroyed { win: win(1) });
        let (_, doc) = exit_write(&cmds).expect("Session_End written");
        assert_eq!(doc.cause, SaveCause::LastWindowClosed);
        assert_eq!(doc.status, crate::model::Status::Pending);
        assert!(e.store().pending_snapshot().is_some());
    }

    #[test]
    fn exit_without_open_windows_exits_at_once_without_writing() {
        let (mut e, _) = Engine::new(Settings::default(), pending_startup(0));
        let before = e.store().pending_snapshot().cloned();
        assert_eq!(e.handle(Millis(1), Event::ExitRequested), vec![Command::Exit]);
        assert!(e.has_exited());
        assert_eq!(e.store().pending_snapshot().cloned(), before);
        assert!(e.handle(Millis(2), scan(1)).is_empty());
    }

    #[test]
    fn exit_with_open_windows_writes_the_capture_then_exits() {
        let (mut e, _) = Engine::new(Settings::default(), startup());
        e.handle(Millis(1), scan(1));
        let cmds = e.handle(Millis(10), Event::ExitRequested);
        let (seq, doc) = exit_write(&cmds).expect("exit write");
        assert_eq!(doc.cause, SaveCause::ExitThroughSettings);
        assert_eq!(doc.status, crate::model::Status::Pending);
        assert_eq!(doc.snapshot.tab_count(), 1);
        assert!(!cmds.contains(&Command::Exit));
        assert_eq!(control_timer(&cmds).map(|t| t.1), Some(Millis(10 + EXIT_WRITE_TIMEOUT_MS)));
        assert!(e.is_exiting() && !e.has_exited());

        // Everything else is ignored: the windows stay as they are.
        assert!(e.handle(Millis(11), Event::WindowDestroyed { win: win(1) }).is_empty());
        assert!(e.handle(Millis(12), initial(2)).is_empty());

        let cmds = e.handle(Millis(20), Event::WriteCompleted { seq, result: Ok(()) });
        assert_eq!(cmds, vec![Command::Exit]);
        assert!(e.has_exited());
        assert!(e.store().is_durable(seq));
    }

    #[test]
    fn exit_retries_a_failed_write_and_never_hangs() {
        // A failed write is retried; the retry's completion exits.
        let (mut e, _) = Engine::new(Settings::default(), startup());
        e.handle(Millis(1), scan(1));
        let (seq, _) = exit_write(&e.handle(Millis(10), Event::ExitRequested)).unwrap();
        let cmds = e.handle(Millis(20), Event::WriteCompleted { seq, result: Err(IoErrorKind::SharingViolation) });
        assert!(!cmds.contains(&Command::Exit), "{cmds:?}");
        let (id, at) = cmds
            .iter()
            .find_map(|c| match c {
                Command::SetTimer { id, at } if id.owner == TimerOwner::Store => Some((*id, *at)),
                _ => None,
            })
            .expect("retry timer");
        let cmds = e.handle(at, Event::Timer(id));
        let retry = cmds
            .iter()
            .find_map(|c| match c {
                Command::WriteSessionFile { seq, .. } => Some(*seq),
                _ => None,
            })
            .expect("retry write");
        let cmds = e.handle(at + 5, Event::WriteCompleted { seq: retry, result: Ok(()) });
        assert!(cmds.ends_with(&[Command::Exit]), "{cmds:?}");

        // No completion at all: the deadline exits with an ERROR entry.
        let (mut e, _) = Engine::new(Settings::default(), startup());
        e.handle(Millis(1), scan(1));
        let (id, at) = control_timer(&e.handle(Millis(10), Event::ExitRequested)).unwrap();
        let cmds = e.handle(at, Event::Timer(id));
        assert!(
            matches!(cmds.as_slice(), [Command::AppendLog(LogEntry::Error { operation, .. }), Command::Exit] if operation == OP_EXIT),
            "{cmds:?}"
        );
        assert!(e.has_exited());
        assert!(e.handle(at + 1, Event::Timer(id)).is_empty());
    }

    // --- tab folding (task 22.2) ----------------------------------------------

    fn folder(name: &str) -> LocationId {
        LocationId { parsing_name: format!(r"C:\{name}").as_str().into(), display_name: name.into(), ..loc() }
    }

    fn scan_of(n: u64, location: LocationId) -> Event {
        Event::WindowScanned(WindowObservation {
            win: win(n),
            pid: 42,
            tabs: vec![TabObservation { tab: tab(n), location: ObservedLocation::Known(location) }],
            active: Some(tab(n)),
            strip: None,
            placement: placement(),
        })
    }

    fn appeared(n: u64, bypass_keys: bool) -> Event {
        Event::WindowAppeared { win: win(n), pid: 42, bypass_keys, desktop: DesktopView::Unknown, foreground: None }
    }

    fn fold_timer_of(cmds: &[Command]) -> TimerId {
        cmds.iter()
            .find_map(|c| match c {
                Command::SetTimer { id, .. } if id.owner == TimerOwner::Fold => Some(*id),
                _ => None,
            })
            .expect("a fold timer")
    }

    fn writes(cmds: &[Command]) -> usize {
        cmds.iter().filter(|c| matches!(c, Command::WriteSessionFile { .. })).count()
    }

    #[test]
    fn fold_events_are_offered_to_the_folding_engine() {
        let events = all_events();
        let fold_flagged = events.iter().filter(|e| route(e).fold).count();
        // Scan, revocation, destruction, activation, InitialLocationKnown,
        // TabCreated, NavigationCompleted, ActivationConfirmed, SettingsChanged,
        // the folding events (`all_events` has two `WindowAppeared`), the
        // failed hide, and the Fold timer.
        assert_eq!(fold_flagged, 16);
        assert!(!route(&Event::ProbeCompleted { probe: ProbeId(1), outcome: ProbeOutcome::Available }).fold);
        assert!(!route(&Event::OperationFailed { op: OpId(1), error: ShellError { code: 0, message: String::new() } }).fold);
        assert_eq!(route(&appeared(2, false)), Route { fold: true, ..Route::default() });
    }

    #[test]
    fn a_candidate_is_hidden_and_stays_away_from_the_other_components() {
        let (mut e, _) = Engine::new(Settings::default(), startup());
        e.handle(Millis(1), scan_of(1, folder("A")));
        let cmds = e.handle(Millis(10), appeared(2, false));
        assert!(cmds.contains(&Command::HideWindow { win: win(2) }), "{cmds:?}");
        fold_timer_of(&cmds);
        assert_eq!(e.folding().candidates_in_progress(), 1);

        // Scan, activation, navigation, and tab of the candidate reach nobody else.
        assert!(e.handle(Millis(11), scan_of(2, folder("B"))).is_empty());
        assert!(e.handle(Millis(12), Event::WindowActivated { win: win(2) }).is_empty());
        assert!(e.handle(Millis(13), Event::NavigationCompleted { tab: tab(2), location: folder("B") }).is_empty());
        assert!(e.handle(Millis(14), Event::TabRevoked { win: win(2), tab: tab(7) }).is_empty());
        assert_eq!(e.tracker().activation_order(), vec![win(1)]);
        assert_eq!(e.tracker().window_count(), 1);
        assert_eq!(e.trigger_candidate(), None);
        assert!(!e.trigger.seen.contains(&win(2)));

        // Closing it is no Session_End and saves nothing.
        let cmds = e.handle(Millis(15), Event::WindowDestroyed { win: win(2) });
        assert!(cmds.is_empty(), "{cmds:?}");
        assert_eq!(e.folding().candidates_in_progress(), 0);
        assert_eq!(e.tracker().window_count(), 1);
        assert!(!e.tracker().is_frozen());
    }

    #[test]
    fn an_excluded_window_continues_on_the_existing_path() {
        let (mut e, _) = Engine::new(Settings::default(), startup());
        e.handle(Millis(1), scan_of(1, folder("A")));
        // Fold_Bypass_Keys (Req 14.4).
        assert!(e.handle(Millis(10), appeared(2, true)).is_empty());
        assert!(e.folding().is_excluded(win(2)) && e.folding().opening().contains(win(2)));
        e.handle(Millis(11), scan_of(2, folder("B")));
        assert_eq!(e.tracker().window_count(), 2);
        // No tracked window at all: the first window may be a Trigger_Window.
        let (mut e, _) = Engine::new(Settings::default(), startup());
        assert!(e.handle(Millis(10), appeared(1, false)).is_empty());
        assert!(e.folding().is_excluded(win(1)));
        // With folding off nothing is hidden.
        let off = Settings { tab_folding_enabled: false, ..Settings::default() };
        let (mut e, _) = Engine::new(off, startup());
        e.handle(Millis(1), scan_of(1, folder("A")));
        assert!(e.handle(Millis(10), appeared(2, false)).is_empty());
    }

    #[test]
    fn a_waiting_trigger_or_a_restoration_excludes_new_windows() {
        let (mut e, _) = Engine::new(Settings::default(), pending_startup(0));
        e.handle(Millis(100), scan(1));
        assert_eq!(e.trigger_candidate(), Some(win(1)));
        // The Trigger_Window waits for its restoration (Req 14.3 c, d).
        assert!(e.handle(Millis(110), appeared(2, false)).is_empty());
        assert!(e.folding().is_excluded(win(2)));
        // The restoration runs: still excluded.
        e.handle(Millis(120), initial(1));
        assert!(e.restore_session().is_some());
        assert!(e.handle(Millis(130), appeared(3, false)).is_empty());
        assert!(e.folding().is_excluded(win(3)));
    }

    #[test]
    fn a_drag_out_is_revealed_and_handed_to_the_tracker() {
        let (mut e, _) = Engine::new(Settings::default(), startup());
        e.handle(Millis(1), scan_of(1, folder("A")));
        // A tab of a window the tracker does not know proves nothing.
        assert!(e.handle(Millis(5), Event::TabDetached { from: win(9) }).is_empty());
        e.handle(Millis(100), appeared(2, false));
        e.handle(Millis(101), scan_of(2, folder("B")));
        let cmds = e.handle(Millis(150), Event::TabDetached { from: win(1) });
        assert!(cmds.contains(&Command::Reveal { win: win(2) }), "{cmds:?}");
        assert!(cmds.contains(&Command::RescanWindow(win(2))), "{cmds:?}");
        assert!(e.folding().is_excluded(win(2)));
        // The buffered scan reached the tracker, and the window is seen by trigger detection.
        assert!(e.tracker().window(win(2)).is_some());
        assert!(e.trigger.seen.contains(&win(2)));
        assert_eq!(e.folding().candidates_in_progress(), 0);
    }

    #[test]
    fn a_candidate_left_open_joins_the_live_session_at_once() {
        let (mut e, _) = Engine::new(Settings::default(), startup());
        e.handle(Millis(1), scan_of(1, folder("A")));
        let cmds = e.handle(Millis(10), appeared(2, false));
        let timer = fold_timer_of(&cmds);
        e.handle(Millis(11), scan_of(2, folder("B")));
        assert!(e.tracker().window(win(2)).is_none());
        // No first navigation within 8 s (Req 14.14).
        let cmds = e.handle(Millis(10 + crate::fold::FOLD_FIRST_NAV_MS), Event::Timer(timer));
        assert!(cmds.contains(&Command::Reveal { win: win(2) }), "{cmds:?}");
        assert!(cmds.contains(&Command::RescanWindow(win(2))), "{cmds:?}");
        assert!(e.tracker().window(win(2)).is_some());
        assert!(e.trigger.seen.contains(&win(2)));
        assert!(e.folding().opening().contains(win(2)));
        assert_eq!(e.folding().candidates_in_progress(), 0);
        // From now on it is an ordinary window.
        e.handle(Millis(9_000), Event::WindowActivated { win: win(2) });
        assert_eq!(e.tracker().activation_order()[0], win(2));
    }

    // --- hand-over of a left-open candidate (task 22.3, Req 14.10, 14.15, 4.2) --

    /// A candidate (window 2, scanned, Location announced) that is left open
    /// because no Fold_Target is available (Req 14.14: the only other window
    /// is unresponsive). Returns the engine, the commands of the hand-over,
    /// and the logical time of the decision.
    fn left_open_candidate() -> (Engine, Vec<Command>, Millis) {
        let (mut e, _) = Engine::new(Settings::default(), startup());
        e.handle(Millis(1), scan_of(1, folder("A")));
        e.fold.opening_mut().append(win(1));
        e.handle(Millis(10), appeared(2, false));
        e.handle(Millis(11), scan_of(2, folder("B")));
        let cmds = e.handle(
            Millis(12),
            Event::InitialLocationKnown { win: win(2), tab: None, location: folder("B"), selection: vec![] },
        );
        let Some(Command::ProbeFoldContext { probe, .. }) = cmds.first().cloned() else { panic!("{cmds:?}") };
        assert!(e.tracker().window(win(2)).is_none(), "a candidate is not in the Live_Session");
        let at = Millis(150);
        let context = Event::FoldContext { probe, desktop: DesktopView::Unknown, unresponsive: BTreeSet::from([win(1)]) };
        let cmds = e.handle(at, context);
        (e, cmds, at)
    }

    #[test]
    fn a_left_open_candidate_is_in_the_live_session_at_the_moment_it_is_given_up() {
        let (e, cmds, _) = left_open_candidate();
        // Within the 1 s of Req 14.15 (here: in the same step): the buffered
        // scan reached the tracker, so the window and its Location are in the
        // Live_Session, and a rescan confirms it.
        let live = e.tracker().window(win(2)).expect("tracked");
        assert_eq!(live.tabs().len(), 1);
        assert_eq!(live.tabs()[0].location(), Some(&folder("B")));
        assert!(cmds.contains(&Command::Reveal { win: win(2) }), "{cmds:?}");
        assert!(cmds.contains(&Command::RescanWindow(win(2))), "{cmds:?}");
        // The rescan comes after the replayed events.
        let rescan = cmds.iter().position(|c| *c == Command::RescanWindow(win(2))).unwrap();
        assert_eq!(rescan, cmds.len() - 1, "{cmds:?}");
        assert_eq!(e.folding().candidates_in_progress(), 0);
        assert!(e.folding().is_excluded(win(2)));
    }

    #[test]
    fn a_left_open_candidate_counts_as_open_for_a_session_end() {
        let (mut e, _, _) = left_open_candidate();
        let end = e.handle(Millis(9_000), Event::WindowDestroyed { win: win(1) });
        assert_eq!(writes(&end), 0, "win(2) is still open: {end:?}");
        assert!(!e.tracker().is_frozen());
        assert_eq!(e.tracker().open_window_keys(), vec![win(2)]);
    }

    // --- Session_End rules while candidates are in progress (task 22.4) ---------

    /// The checkpoint timers (`Store`) among `cmds`.
    fn store_timers(cmds: &[Command]) -> Vec<(TimerId, Millis)> {
        cmds.iter()
            .filter_map(|c| match c {
                Command::SetTimer { id, at } if id.owner == TimerOwner::Store => Some((*id, *at)),
                _ => None,
            })
            .collect()
    }

    /// Window 1 (pid 42) is tracked and window 2 (pid 43) is a hidden,
    /// scanned candidate. Returns the engine and the first-navigation timer
    /// of the candidate.
    fn tracked_window_and_hidden_candidate() -> (Engine, TimerId) {
        let (mut e, _) = Engine::new(Settings::default(), startup());
        e.handle(Millis(1), scan_of(1, folder("A")));
        let event = Event::WindowAppeared { win: win(2), pid: 43, bypass_keys: false, desktop: DesktopView::Unknown, foreground: None };
        let timer = fold_timer_of(&e.handle(Millis(10), event));
        e.handle(Millis(11), scan_of(2, folder("B")));
        assert_eq!(e.folding().candidates_in_progress(), 1);
        (e, timer)
    }

    #[test]
    fn closing_the_last_tracked_window_while_a_candidate_is_hidden_is_no_session_end() {
        let (mut e, timer) = tracked_window_and_hidden_candidate();
        let cmds = e.handle(Millis(100), Event::WindowDestroyed { win: win(1) });
        assert_eq!(writes(&cmds), 0, "an ordinary window close (Req 2.7): {cmds:?}");
        assert_eq!(e.tracker().window_count(), 0);
        assert!(!e.tracker().is_frozen());

        // The checkpoint the close schedules finds an empty Live_Session and writes nothing.
        let mut all = cmds.clone();
        for (id, at) in store_timers(&cmds) {
            all.extend(e.handle(at, Event::Timer(id)));
        }
        assert_eq!(writes(&all), 0, "{all:?}");

        // The candidate is left open (no first navigation in 8 s): it is the
        // only window of the Live_Session now, and closing it ends the session.
        let cmds = e.handle(Millis(10 + crate::fold::FOLD_FIRST_NAV_MS), Event::Timer(timer));
        assert!(cmds.contains(&Command::RescanWindow(win(2))), "{cmds:?}");
        assert_eq!(e.tracker().open_window_keys(), vec![win(2)]);
        assert_eq!(e.tracker().candidates_in_progress(), 0);
        let end = e.handle(Millis(20_000), Event::WindowDestroyed { win: win(2) });
        assert_eq!(writes(&end), 1, "{end:?}");
        assert!(e.tracker().is_empty());
    }

    #[test]
    fn a_candidate_that_goes_away_after_the_last_tracked_window_closed_saves_nothing() {
        let (mut e, _) = tracked_window_and_hidden_candidate();
        let mut all = e.handle(Millis(100), Event::WindowDestroyed { win: win(1) });
        // Closed by Explorer or the user before its fold ended.
        all.extend(e.handle(Millis(110), Event::WindowDestroyed { win: win(2) }));
        assert_eq!(e.folding().candidates_in_progress(), 0);
        assert_eq!(writes(&all), 0, "no Session_End for a candidate (Req 14.28): {all:?}");
        assert!(e.tracker().is_empty() && !e.tracker().is_frozen());
        // Later windows are ordinary again.
        let cmds = e.handle(Millis(200), scan_of(3, folder("C")));
        assert_eq!(writes(&cmds), 0);
        let end = e.handle(Millis(300), Event::WindowDestroyed { win: win(3) });
        assert_eq!(writes(&end), 1, "{end:?}");
    }

    #[test]
    fn a_candidate_folded_after_the_last_tracked_window_closed_causes_no_session_end() {
        let (mut e, _) = Engine::new(Settings::default(), startup());
        e.handle(Millis(1), scan_of(1, folder("A")));
        e.fold.opening_mut().append(win(1));
        e.handle(Millis(100), appeared(2, false));
        e.handle(Millis(101), scan_of(2, folder("B")));
        let cmds = e.handle(
            Millis(102),
            Event::InitialLocationKnown { win: win(2), tab: Some(tab(2)), location: folder("B"), selection: vec![] },
        );
        let Some(Command::ProbeFoldContext { probe, .. }) = cmds.first().cloned() else { panic!("{cmds:?}") };
        let cmds = e.handle(
            Millis(110),
            Event::FoldContext { probe, desktop: DesktopView::Unknown, unresponsive: BTreeSet::new() },
        );
        let Some(Command::CreateTab { op, .. }) = cmds.first().cloned() else { panic!("{cmds:?}") };
        e.handle(Millis(120), Event::TabStarted { op, tab: tab(9) });
        e.handle(Millis(400), Event::TabCreated { win: win(1), tab: tab(9) });
        let cmds = e.handle(Millis(500), Event::NavigationCompleted { tab: tab(9), location: folder("B") });
        let Some(Command::ActivateTab { op: activate, .. }) =
            cmds.iter().find(|c| matches!(c, Command::ActivateTab { .. })).cloned()
        else {
            panic!("{cmds:?}")
        };
        let cmds = e.handle(Millis(510), Event::ActivationConfirmed { op: activate });
        assert!(cmds.contains(&Command::CloseWindow { win: win(2) }), "{cmds:?}");

        // The candidate is folded and its close is posted, but it still exists
        // when the user closes the Fold_Target, the last tracked window.
        let mut all = e.handle(Millis(520), Event::WindowDestroyed { win: win(1) });
        assert_eq!(writes(&all), 0, "{all:?}");
        assert!(e.tracker().is_empty());
        // Then the candidate is gone: still no Session_End, no write.
        all.extend(e.handle(Millis(530), Event::WindowDestroyed { win: win(2) }));
        assert_eq!(writes(&all), 0, "{all:?}");
        assert!(e.tracker().is_empty() && e.tracker().window(win(2)).is_none());
        assert_eq!(e.folding().candidates_in_progress(), 0);
    }

    #[test]
    fn a_fold_target_closing_the_last_tracked_window_leaves_the_candidate_open_in_the_session() {
        let (mut e, _) = Engine::new(Settings::default(), startup());
        e.handle(Millis(1), scan_of(1, folder("A")));
        e.fold.opening_mut().append(win(1));
        e.handle(Millis(100), appeared(2, false));
        e.handle(Millis(101), scan_of(2, folder("B")));
        let cmds = e.handle(
            Millis(102),
            Event::InitialLocationKnown { win: win(2), tab: Some(tab(2)), location: folder("B"), selection: vec![] },
        );
        let Some(Command::ProbeFoldContext { probe, .. }) = cmds.first().cloned() else { panic!("{cmds:?}") };
        e.handle(
            Millis(110),
            Event::FoldContext { probe, desktop: DesktopView::Unknown, unresponsive: BTreeSet::new() },
        );
        // The Fold_Target closes before the Folded_Tab exists: no Session_End.
        // The candidate looks for another target, finds none, and is left open
        // and joins the Live_Session (Req 14.14, 14.15).
        let cmds = e.handle(Millis(200), Event::WindowDestroyed { win: win(1) });
        assert_eq!(writes(&cmds), 0, "{cmds:?}");
        assert!(e.tracker().is_empty());
        let Some(Command::ProbeFoldContext { probe, .. }) = cmds.first().cloned() else { panic!("{cmds:?}") };
        let cmds = e.handle(
            Millis(210),
            Event::FoldContext { probe, desktop: DesktopView::Unknown, unresponsive: BTreeSet::new() },
        );
        assert_eq!(writes(&cmds), 0, "{cmds:?}");
        assert!(cmds.contains(&Command::Reveal { win: win(2) }), "{cmds:?}");
        assert_eq!(e.tracker().open_window_keys(), vec![win(2)]);
        assert_eq!(e.folding().candidates_in_progress(), 0);
    }

    #[test]
    fn a_session_end_query_with_a_candidate_in_progress_saves_the_tracked_windows_and_freezes() {
        let (mut e, _) = tracked_window_and_hidden_candidate();
        let cmds = e.handle(Millis(100), Event::EndSessionQuery(EndKind::Shutdown));
        assert_eq!(writes(&cmds), 1, "{cmds:?}");
        assert!(e.tracker().is_frozen());
        // The user's close of the tracked window is deferred, not a second Session_End.
        let cmds = e.handle(Millis(110), Event::WindowDestroyed { win: win(1) });
        assert_eq!(writes(&cmds), 0, "{cmds:?}");
        // The candidate going away while frozen causes no save.
        let cmds = e.handle(Millis(120), Event::WindowDestroyed { win: win(2) });
        assert_eq!(writes(&cmds), 0, "{cmds:?}");
        let cmds = e.handle(Millis(200), Event::EndSessionCancelled);
        assert_eq!(writes(&cmds), 0, "{cmds:?}");
        assert!(!e.tracker().is_frozen() && e.tracker().is_empty());
    }

    #[test]
    fn a_session_end_query_with_only_a_candidate_saves_nothing() {
        let (mut e, _) = tracked_window_and_hidden_candidate();
        e.handle(Millis(100), Event::WindowDestroyed { win: win(1) });
        let cmds = e.handle(Millis(110), Event::EndSessionQuery(EndKind::SignOut));
        assert_eq!(writes(&cmds), 0, "the candidate is not in the Live_Session (Req 3.11): {cmds:?}");
    }

    #[test]
    fn a_process_exit_with_a_candidate_in_progress_is_no_session_end() {
        let (mut e, timer) = tracked_window_and_hidden_candidate();
        // The process hosting window 1 (pid 42) ends; the candidate (pid 43) is
        // an open window, so the orphan is kept and nothing is saved.
        let cmds = e.handle(Millis(100), Event::ProcessExited { pid: 42 });
        assert_eq!(writes(&cmds), 0, "{cmds:?}");
        assert_eq!(e.tracker().window(win(1)).unwrap().status(), crate::tracker::WindowStatus::Orphaned);
        assert_eq!(e.folding().candidates_in_progress(), 1);

        // The candidate is left open: closing it is the Session_End, orphan included.
        e.handle(Millis(10 + crate::fold::FOLD_FIRST_NAV_MS), Event::Timer(timer));
        assert!(e.tracker().window(win(2)).is_some());
        let end = e.handle(Millis(20_000), Event::WindowDestroyed { win: win(2) });
        assert_eq!(writes(&end), 1, "{end:?}");
        assert!(e.tracker().is_empty());
    }

    #[test]
    fn a_left_open_candidate_never_becomes_a_trigger_window() {
        let (mut e, _) = Engine::new(Settings::default(), pending_startup(0));
        // Window 1 is no Trigger_Window: its Location never arrives.
        let cmds = e.handle(Millis(100), scan(1));
        let timer = cmds
            .iter()
            .find_map(|c| match c {
                Command::SetTimer { id, .. } if id.owner == TimerOwner::Control => Some(*id),
                _ => None,
            })
            .expect("a candidate waits for its Location");
        e.handle(Millis(100 + INITIAL_LOCATION_WAIT_MS), Event::Timer(timer));
        assert_eq!(e.trigger_candidate(), None);
        assert!(e.tracker().window(win(1)).is_some());

        // Window 2 opens while window 1 is open: a candidate, left open by the timeout.
        let cmds = e.handle(Millis(5_000), appeared(2, false));
        let timer = fold_timer_of(&cmds);
        e.handle(Millis(5_001), scan_of(2, folder("B")));
        let cmds = e.handle(Millis(5_000 + crate::fold::FOLD_FIRST_NAV_MS), Event::Timer(timer));
        assert!(cmds.contains(&Command::RescanWindow(win(2))), "{cmds:?}");
        assert!(e.trigger.seen.contains(&win(2)));
        assert_eq!(e.trigger_candidate(), None, "the replay is not a first sighting");
        assert!(e.restore_session().is_none());

        // The other window is gone: the rescan, and any later sign of window 2,
        // still do not make it a Trigger_Window (Req 14.10, 4.2).
        let later = Millis(20_000);
        e.handle(later, Event::WindowDestroyed { win: win(1) });
        e.handle(Millis(20_001), scan_of(2, folder("B")));
        e.handle(
            Millis(20_002),
            Event::InitialLocationKnown { win: win(2), tab: None, location: folder("B"), selection: vec![] },
        );
        e.handle(Millis(20_003), Event::WindowActivated { win: win(2) });
        assert_eq!(e.trigger_candidate(), None);
        assert!(e.restore_session().is_none());
        assert_eq!(e.tracker().open_window_keys(), vec![win(2)]);
    }

    #[test]
    fn a_left_open_candidate_takes_its_first_signal_position_in_the_opening_order() {
        let (mut e, _) = Engine::new(Settings::default(), startup());
        e.handle(Millis(1), scan_of(1, folder("A")));
        e.fold.opening_mut().append(win(1));
        let cmds = e.handle(Millis(10), appeared(2, false));
        let timer = fold_timer_of(&cmds);
        // Window 3 appears after window 2 (Fold_Bypass_Keys: excluded, appended).
        e.handle(Millis(11), appeared(3, true));
        e.handle(Millis(12), scan_of(2, folder("B")));
        // Window 2 is a candidate: not in the order while the fold owns it.
        assert_eq!(e.folding().opening().to_vec(), vec![win(1), win(3)]);
        let cmds = e.handle(Millis(10 + crate::fold::FOLD_FIRST_NAV_MS), Event::Timer(timer));
        assert!(cmds.contains(&Command::RescanWindow(win(2))), "{cmds:?}");
        assert_eq!(e.folding().opening().to_vec(), vec![win(1), win(2), win(3)]);
    }

    #[test]
    fn a_left_open_candidate_without_a_scan_waits_for_its_rescan() {
        let (mut e, _) = Engine::new(Settings::default(), startup());
        e.handle(Millis(1), scan_of(1, folder("A")));
        let cmds = e.handle(Millis(10), appeared(2, false));
        let timer = fold_timer_of(&cmds);
        let cmds = e.handle(Millis(10 + crate::fold::FOLD_FIRST_NAV_MS), Event::Timer(timer));
        // Nothing buffered: only the rescan can bring it into the Live_Session.
        assert!(cmds.contains(&Command::RescanWindow(win(2))), "{cmds:?}");
        assert!(e.tracker().window(win(2)).is_none());
        assert!(e.trigger.seen.contains(&win(2)));
        e.handle(Millis(9_100), scan_of(2, folder("B")));
        assert!(e.tracker().window(win(2)).is_some());
        assert_eq!(e.trigger_candidate(), None);
    }

    #[test]
    fn a_candidate_folds_into_the_primary_window_without_touching_the_live_session() {
        let (mut e, _) = Engine::new(Settings::default(), startup());
        e.handle(Millis(1), scan_of(1, folder("A")));
        e.fold.opening_mut().append(win(1));
        let mut all = e.handle(Millis(100), appeared(2, false));
        all.extend(e.handle(Millis(101), scan_of(2, folder("B"))));

        let cmds = e.handle(
            Millis(102),
            Event::InitialLocationKnown { win: win(2), tab: Some(tab(2)), location: folder("B"), selection: vec![] },
        );
        let Some(Command::ProbeFoldContext { probe, candidate }) = cmds.first().cloned() else { panic!("{cmds:?}") };
        assert_eq!(candidate, win(2));
        all.extend(cmds);

        let cmds = e.handle(
            Millis(110),
            Event::FoldContext { probe, desktop: DesktopView::Unknown, unresponsive: BTreeSet::new() },
        );
        let Some(Command::CreateTab { op, win: target }) = cmds.first().cloned() else { panic!("{cmds:?}") };
        assert_eq!(target, win(1));
        assert!(is_fold_op(op));
        all.extend(cmds);

        all.extend(e.handle(Millis(120), Event::TabStarted { op, tab: tab(9) }));
        let cmds = e.handle(Millis(400), Event::TabCreated { win: win(1), tab: tab(9) });
        let Some(Command::Navigate { op: nav, tab: navigated, location }) = cmds.first().cloned() else {
            panic!("{cmds:?}")
        };
        assert_eq!((navigated, location), (tab(9), folder("B")));
        assert!(is_fold_op(nav));
        all.extend(cmds);

        // The navigation of the Folded_Tab reaches the fold and the tracker; the
        // candidate's own tab never does.
        let cmds = e.handle(Millis(500), Event::NavigationCompleted { tab: tab(9), location: folder("B") });
        let Some(Command::ActivateTab { op: activate, win: target, index }) =
            cmds.iter().find(|c| matches!(c, Command::ActivateTab { .. })).cloned()
        else {
            panic!("{cmds:?}")
        };
        assert_eq!((target, index), (win(1), 1));
        all.extend(cmds);

        // The confirmation of a fold op goes to the fold alone.
        let cmds = e.handle(Millis(510), Event::ActivationConfirmed { op: activate });
        assert!(cmds.contains(&Command::Focus(win(1))), "{cmds:?}");
        assert!(cmds.contains(&Command::CloseWindow { win: win(2) }), "{cmds:?}");
        all.extend(cmds);

        let cmds = e.handle(Millis(600), Event::WindowDestroyed { win: win(2) });
        assert!(cmds.iter().any(|c| matches!(c, Command::AppendLog(LogEntry::Fold { .. }))), "{cmds:?}");
        assert!(cmds.contains(&Command::RescanWindow(win(1))), "{cmds:?}");
        all.extend(cmds);

        // The candidate was never tracked, never activated, and caused no save.
        assert!(e.tracker().window(win(2)).is_none());
        assert_eq!(e.tracker().activation_order(), vec![win(1)]);
        assert_eq!(writes(&all), 0, "{all:?}");
        assert!(!e.folding().opening().contains(win(2)));
        assert_eq!(e.folding().candidates_in_progress(), 0);
    }

    #[test]
    fn resuming_ranks_the_open_windows_in_their_activation_order() {
        let (mut e, _) = Engine::new(Settings::default(), startup());
        assert!(e.folding().opening().is_empty());
        for n in 1..=3 {
            e.handle(Millis(n), scan_of(n, folder("A")));
        }
        e.handle(Millis(10), Event::WindowActivated { win: win(2) });
        e.handle(Millis(11), Event::WindowActivated { win: win(3) });
        assert_eq!(e.tracker().open_window_keys(), vec![win(3), win(2), win(1)]);
        assert!(e.folding().opening().is_empty(), "scans alone rank nothing");

        e.handle(Millis(20), Event::ShellLost);
        e.handle(Millis(30), Event::ShellAvailable);
        assert_eq!(e.folding().opening().to_vec(), vec![win(3), win(2), win(1)]);

        // A window already open is never a candidate, even if its first signal arrives late.
        assert!(e.handle(Millis(40), appeared(1, false)).is_empty());
        assert_eq!(e.folding().candidates_in_progress(), 0);
        // A window opened later is ranked after them.
        e.handle(Millis(50), appeared(4, true));
        assert_eq!(e.folding().opening().to_vec(), vec![win(3), win(2), win(1), win(4)]);

        // Resuming again re-ranks by the Activation_Order at that moment (4 is not tracked yet).
        e.handle(Millis(60), Event::WindowActivated { win: win(1) });
        e.handle(Millis(70), Event::ShellAvailable);
        assert_eq!(e.folding().opening().to_vec(), vec![win(1), win(3), win(2)]);
        e.handle(Millis(80), Event::WindowDestroyed { win: win(3) });
        assert_eq!(e.folding().opening().to_vec(), vec![win(1), win(2)]);
    }

    #[test]
    fn a_destroyed_window_leaves_the_opening_order() {
        let (mut e, _) = Engine::new(Settings::default(), startup());
        e.handle(Millis(1), scan_of(1, folder("A")));
        e.handle(Millis(2), scan_of(3, folder("C")));
        e.fold.opening_mut().append(win(1));
        e.fold.opening_mut().append(win(3));
        e.handle(Millis(3), Event::WindowDestroyed { win: win(1) });
        assert_eq!(e.folding().opening().to_vec(), vec![win(3)]);
    }

    #[test]
    fn a_minimized_target_is_shown_in_its_pre_minimize_state() {
        let (mut e, _) = Engine::new(Settings::default(), startup());
        let mut scan = scan_of(1, folder("A"));
        let Event::WindowScanned(obs) = &mut scan else { unreachable!() };
        obs.placement.state = ShowState::Minimized { restore_maximized: true };
        e.handle(Millis(1), scan);
        e.handle(Millis(2), scan_of(2, folder("B")));
        let shown = pre_minimize_placement(e.tracker(), &[], win(1)).expect("minimized");
        assert_eq!(shown.state, ShowState::Maximized);
        assert_eq!(pre_minimize_placement(e.tracker(), &[], win(2)), None);
        assert_eq!(pre_minimize_placement(e.tracker(), &[], win(9)), None);
    }

    // --- settings, process exit, shell loss (task 22.6) ------------------------

    fn appeared_in(n: u64, pid: u32) -> Event {
        Event::WindowAppeared { win: win(n), pid, bypass_keys: false, desktop: DesktopView::Unknown, foreground: None }
    }

    fn settings_with_folding(enabled: bool) -> Settings {
        Settings { tab_folding_enabled: enabled, ..Settings::default() }
    }

    fn errors_of<'c>(cmds: &'c [Command], operation: &str) -> Vec<&'c Command> {
        cmds.iter()
            .filter(|c| matches!(c, Command::AppendLog(LogEntry::Error { operation: o, .. }) if o == operation))
            .collect()
    }

    #[test]
    fn turning_folding_off_leaves_every_candidate_open() {
        let (mut e, _) = Engine::new(Settings::default(), startup());
        e.handle(Millis(1), scan_of(1, folder("A")));
        e.handle(Millis(10), appeared_in(2, 43));
        e.handle(Millis(11), scan_of(2, folder("B")));
        e.handle(Millis(12), appeared_in(3, 44));
        e.handle(Millis(13), scan_of(3, folder("C")));
        assert_eq!(e.folding().candidates_in_progress(), 2);

        let cmds = e.handle(Millis(100), Event::SettingsChanged(settings_with_folding(false)));
        assert!(cmds.iter().any(|c| matches!(c, Command::PersistSettings(_))), "{cmds:?}");
        for n in [2, 3] {
            assert!(cmds.contains(&Command::Reveal { win: win(n) }), "{cmds:?}");
            assert!(cmds.contains(&Command::RescanWindow(win(n))), "{cmds:?}");
            // Handed over: in the Live_Session, seen by trigger detection, excluded.
            assert!(e.tracker().window(win(n)).is_some());
            assert!(e.trigger.seen.contains(&win(n)));
            assert!(e.folding().is_excluded(win(n)) && e.folding().opening().contains(win(n)));
        }
        let reasons: Vec<_> = cmds
            .iter()
            .filter_map(|c| match c {
                Command::AppendLog(LogEntry::Fold { outcome, .. }) => Some(outcome.clone()),
                _ => None,
            })
            .collect();
        let disabled = crate::fold::FoldOutcome::NotFolded {
            reason: crate::fold::NotFoldedReason::FoldingDisabled,
            switch_reason: None,
        };
        assert_eq!(reasons, vec![disabled.clone(), disabled], "{cmds:?}");
        assert_eq!(e.folding().candidates_in_progress(), 0);
        assert_eq!(e.tracker().candidates_in_progress(), 0);
        // With folding off a new window is no candidate.
        assert!(e.handle(Millis(200), appeared_in(4, 45)).is_empty());
    }

    #[test]
    fn turning_folding_on_affects_only_later_windows() {
        let (mut e, _) = Engine::new(settings_with_folding(false), startup());
        e.handle(Millis(1), scan_of(1, folder("A")));
        // Opened while folding was off: excluded for good.
        assert!(e.handle(Millis(10), appeared_in(2, 43)).is_empty());
        e.handle(Millis(11), scan_of(2, folder("B")));

        let cmds = e.handle(Millis(100), Event::SettingsChanged(settings_with_folding(true)));
        assert!(!cmds.iter().any(|c| matches!(c, Command::Reveal { .. } | Command::HideWindow { .. })), "{cmds:?}");
        assert_eq!(e.folding().candidates_in_progress(), 0);
        assert!(e.folding().is_excluded(win(2)));
        // A repeated first signal of window 2 is still ignored; window 3 is hidden.
        assert!(e.handle(Millis(110), appeared_in(2, 43)).is_empty());
        let cmds = e.handle(Millis(120), appeared_in(3, 43));
        assert!(cmds.contains(&Command::HideWindow { win: win(3) }), "{cmds:?}");
        assert_eq!(e.folding().candidates_in_progress(), 1);
    }

    #[test]
    fn a_failed_hide_is_logged_and_the_fold_goes_on() {
        let (mut e, _) = tracked_window_and_hidden_candidate();
        let error = ShellError { code: 5, message: "access denied".into() };
        let cmds = e.handle(Millis(20), Event::HideFailed { win: win(2), error: error.clone() });
        assert_eq!(errors_of(&cmds, "fold-hide").len(), 1, "{cmds:?}");
        assert_eq!(cmds.len(), 1, "{cmds:?}");
        assert_eq!(e.folding().candidates_in_progress(), 1);
        assert!(e.tracker().window(win(2)).is_none());
        // A window that is not a candidate is ignored.
        assert!(e.handle(Millis(21), Event::HideFailed { win: win(1), error }).is_empty());
    }

    #[test]
    fn an_exited_process_drops_its_candidates_and_replays_nothing() {
        let (mut e, _) = tracked_window_and_hidden_candidate();
        // A second candidate in another process survives.
        e.handle(Millis(12), appeared_in(3, 44));
        e.handle(Millis(13), scan_of(3, folder("C")));
        assert_eq!(e.folding().candidates_in_progress(), 2);

        let cmds = e.handle(Millis(100), Event::ProcessExited { pid: 43 });
        assert_eq!(errors_of(&cmds, "fold-shell").len(), 1, "{cmds:?}");
        assert!(!cmds.iter().any(|c| matches!(c, Command::Reveal { .. } | Command::RescanWindow(_))), "{cmds:?}");
        assert_eq!(writes(&cmds), 0, "{cmds:?}");
        assert!(!e.folding().owns(win(2)) && e.folding().owns(win(3)));
        assert_eq!(e.folding().candidates_in_progress(), 1);
        assert!(!e.folding().opening().contains(win(2)) && !e.folding().is_excluded(win(2)));
        // Nothing reached the tracker.
        assert!(e.tracker().window(win(2)).is_none());
        assert_eq!(e.tracker().window_count(), 1);
        assert!(!e.trigger.seen.contains(&win(2)));

        // The surviving candidate is still in progress; a process nobody has exited changes nothing.
        let cmds = e.handle(Millis(110), Event::ProcessExited { pid: 99 });
        assert!(errors_of(&cmds, "fold-shell").is_empty(), "{cmds:?}");
        assert_eq!(e.folding().candidates_in_progress(), 1);
    }

    #[test]
    fn the_candidate_count_is_current_when_the_tracker_handles_a_process_exit() {
        // Window 1 is the only tracked window and shares the process of the
        // candidate (window 2), which has the same pid.
        let (mut e, _) = Engine::new(Settings::default(), startup());
        e.handle(Millis(1), scan_of(1, folder("A")));
        e.handle(Millis(10), appeared_in(2, 42));
        e.handle(Millis(11), scan_of(2, folder("B")));
        assert_eq!(e.folding().candidates_in_progress(), 1);
        let cmds = e.handle(Millis(100), Event::ProcessExited { pid: 42 });
        // The candidate is dropped first, so the last tracked window's process
        // ending is an Explorer termination (Req 3.4), not an ordinary close.
        assert_eq!(e.folding().candidates_in_progress(), 0);
        assert_eq!(e.tracker().candidates_in_progress(), 0);
        assert_eq!(errors_of(&cmds, "fold-shell").len(), 1, "{cmds:?}");
        assert_eq!(writes(&cmds), 1, "{cmds:?}");
        assert!(e.tracker().window(win(2)).is_none());
    }

    #[test]
    fn a_lost_shell_drops_every_candidate_and_reveals_it() {
        let (mut e, _) = tracked_window_and_hidden_candidate();
        e.handle(Millis(12), appeared_in(3, 44));
        let before = e.tracker().window_count();

        let cmds = e.handle(Millis(100), Event::ShellLost);
        assert_eq!(errors_of(&cmds, "fold-shell").len(), 2, "{cmds:?}");
        // Windows of surviving processes must not stay invisible; nothing needs the shell.
        for n in [2, 3] {
            assert!(cmds.contains(&Command::Reveal { win: win(n) }), "{cmds:?}");
            assert!(e.trigger.seen.contains(&win(n)));
            assert!(e.folding().is_excluded(win(n)) && e.folding().opening().contains(win(n)));
        }
        assert!(!cmds.iter().any(|c| matches!(
            c,
            Command::RescanWindow(_) | Command::CloseTab { .. } | Command::CreateTab { .. } | Command::ProbeFoldContext { .. }
        )));
        assert_eq!(e.folding().candidates_in_progress(), 0);
        assert_eq!(e.tracker().candidates_in_progress(), 0);
        // Nothing was replayed: the buffered scan of window 2 did not reach the tracker.
        assert_eq!(e.tracker().window_count(), before);
        assert!(e.tracker().window(win(2)).is_none());

        // The shell is back: the rescan reports window 2 as an ordinary window.
        e.handle(Millis(200), Event::ShellAvailable);
        e.handle(Millis(210), scan_of(2, folder("B")));
        assert!(e.tracker().window(win(2)).is_some());
        assert_eq!(e.trigger_candidate(), None);
    }

    #[test]
    fn a_candidate_that_is_closing_is_finished_by_its_process_and_survives_a_lost_shell() {
        // A candidate whose fold completed and whose `CloseWindow` is out.
        let closing = || {
            let (mut e, _) = Engine::new(Settings::default(), startup());
            e.handle(Millis(1), scan_of(1, folder("A")));
            e.fold.opening_mut().append(win(1));
            e.handle(Millis(100), appeared_in(2, 43));
            e.handle(Millis(101), scan_of(2, folder("B")));
            let cmds = e.handle(
                Millis(102),
                Event::InitialLocationKnown { win: win(2), tab: Some(tab(2)), location: folder("B"), selection: vec![] },
            );
            let Some(Command::ProbeFoldContext { probe, .. }) = cmds.first().cloned() else { panic!("{cmds:?}") };
            let cmds = e.handle(
                Millis(110),
                Event::FoldContext { probe, desktop: DesktopView::Unknown, unresponsive: BTreeSet::new() },
            );
            let Some(Command::CreateTab { op, .. }) = cmds.first().cloned() else { panic!("{cmds:?}") };
            e.handle(Millis(120), Event::TabStarted { op, tab: tab(9) });
            e.handle(Millis(400), Event::TabCreated { win: win(1), tab: tab(9) });
            let cmds = e.handle(Millis(500), Event::NavigationCompleted { tab: tab(9), location: folder("B") });
            let Some(Command::ActivateTab { op: activate, .. }) =
                cmds.iter().find(|c| matches!(c, Command::ActivateTab { .. })).cloned()
            else {
                panic!("{cmds:?}")
            };
            let cmds = e.handle(Millis(510), Event::ActivationConfirmed { op: activate });
            assert!(cmds.contains(&Command::CloseWindow { win: win(2) }), "{cmds:?}");
            e
        };

        // Its process ends: that is the close it waited for; no error.
        let mut e = closing();
        let cmds = e.handle(Millis(600), Event::ProcessExited { pid: 43 });
        assert!(errors_of(&cmds, "fold-shell").is_empty(), "{cmds:?}");
        assert!(cmds.iter().any(|c| matches!(c, Command::AppendLog(LogEntry::Fold { .. }))), "{cmds:?}");
        assert_eq!(e.folding().candidates_in_progress(), 0);

        // The shell is lost: the close is an HWND operation and goes on.
        let mut e = closing();
        let cmds = e.handle(Millis(600), Event::ShellLost);
        assert!(errors_of(&cmds, "fold-shell").is_empty(), "{cmds:?}");
        assert_eq!(e.folding().candidates_in_progress(), 1);
    }
}