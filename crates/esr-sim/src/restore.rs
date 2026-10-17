//! Restoration simulation: request-driven window opens, tab creation,
//! navigation, tab closing, UIA activation and selection, reveal, z-order,
//! focus, probes, file writes, failure injection, and command recording, on
//! the same logical clock as the tracking simulation in [`crate::explorer`].
//!
//! [`SimExplorer::handle_command`] carries out every Engine [`Command`]. Each
//! command is recorded with its logical timestamp ([`RecordedCommand`]); its
//! effect is scheduled after a configurable latency ([`RestoreConfig`]) and
//! reported with the feedback events the real adapters post
//! (`WindowOpened`, `TabCreated`, `NavigationCompleted`, `ProbeCompleted`,
//! `OperationFailed`, `WriteCompleted`) plus the tracking events the change
//! causes (scans, revocations, `WindowDestroyed`, `WindowActivated`).
//! [`Driver`] runs an Engine (or any `FnMut(Millis, Event) -> Vec<Command>`)
//! against the simulator until quiescence.
//!
//! # Faithfulness (spikes R1–R4, R9; see `esr-win::strategy`)
//!
//! - **Windows.** `OpenWindow` opens a window in the shell's process after
//!   [`RestoreConfig::window_open_ms`] plus
//!   [`RestoreConfig::window_open_contention_ms`] for every other open still
//!   in flight (430 ms alone, about 1.1 s for the third of three). It is
//!   created hidden (layered, alpha 0) at the requested final placement, does
//!   not become foreground (`SW_SHOWNOACTIVATE`), and shows the requested
//!   Location in its only tab.
//! - **Claiming.** The adapters claim a new window FIFO by (outstanding
//!   request, initial Location): the oldest outstanding request whose
//!   Location equals the window's initial Location (canonical key) gets
//!   `WindowOpened`. A user-opened window whose initial Location matches an
//!   outstanding request is claimed too (the documented ambiguity of Req 4.2;
//!   [`RestoreConfig::claim_user_windows`]). A request-driven window that
//!   finds no outstanding request stays hidden and unclaimed. An unclaimed
//!   window is announced with `InitialLocationKnown` when
//!   [`RestoreConfig::report_initial_location`] is set.
//! - **New tabs.** `CreateTab` appends a tab rightmost and selects it after
//!   [`RestoreConfig::tab_register_ms`] (about 85 ms), reported as
//!   `TabCreated` followed by the new tab's own `NavigationCompleted` to
//!   [`RestoreConfig::new_tab_location`] (Home). Explorer builds tabs one at
//!   a time across all windows ([`RestoreConfig::serialize_tab_creation`]).
//!   Posts selected by [`RestoreConfig::lost_tabs`] produce no tab and no
//!   event (about 1 in 200 in R9). Like the real navigator
//!   (`esr-win::navigator`), a lost post is re-posted after
//!   [`NEW_TAB_REPOST_AFTER_MS`] (that post can be lost too), and if that is
//!   also lost the UIA `AddButton` is invoked after another
//!   [`NEW_TAB_REPOST_AFTER_MS`], which always produces a tab after
//!   [`RestoreConfig::uia_add_tab_ms`]. With
//!   [`RestoreConfig::navigator_repost`] off, a lost post is simply lost.
//! - **Pipelining.** A `CreateTab` for a window that still has a created but
//!   unregistered tab is the `Pipelined` variant of R9: the tab is created,
//!   but its UIA header stays [`RestoreConfig::stuck_header_name`] ("Home")
//!   whatever it shows. Each such command is recorded as a
//!   [`PipeliningViolation`] ([`SimExplorer::pipelining_violations`]) and the
//!   tab has [`SimTab::stuck_header`] set. Lost posts are not outstanding.
//! - **Folded tabs.** With [`FoldConfig::report_tab_started`](crate::fold::FoldConfig)
//!   the new tab window's appearance is reported as `TabStarted`, and the
//!   registration (`TabCreated`) may lag behind it; a tab being created counts
//!   as outstanding until it registers. `ActivateTab` and `CreateTab` can be
//!   made to fail per command (late, never, unconfirmed): see
//!   [`crate::fold`].
//! - **Navigation** completes after [`RestoreConfig::navigate_ms`] (about
//!   60 ms) with `NavigationCompleted`, never moves the tab, and clears the
//!   tab's item selection. A newer `Navigate` of the same tab supersedes an
//!   unfinished one.
//! - **Closing.** `CloseTab` closes exactly that tab after
//!   [`RestoreConfig::close_tab_ms`] (the UIA close button, 220–370 ms);
//!   closing a window's only tab closes the window. With
//!   [`TabCloseModel::Quit`] a tab that is not active (in a window with
//!   several tabs) is silently left open, as `IWebBrowser2::Quit` does.
//! - **UIA.** `ActivateTab` selects the tab at a visual index, `SelectItems`
//!   replaces the tab's item selection ([`SimExplorer::selection`]).
//! - **Reveal / z-order / focus.** `Reveal` makes a window visible; every
//!   window's first visible moment and placement are recorded
//!   ([`WindowRecord::first_visible`]), as are later placement changes while
//!   visible. `SetZOrder` stacks the listed windows on top, first topmost;
//!   `Focus` makes the window foreground (`WindowActivated`). User activation
//!   also raises a window ([`SimExplorer::z_order`]).
//! - **Probes** complete with the outcome and latency programmed for the
//!   Location ([`SimExplorer::set_probe`]), else `Available` after
//!   [`RestoreConfig::probe_ms`].
//! - **Files.** `WriteSessionFile` and `PreserveBackup` complete after
//!   [`RestoreConfig::write_ms`] with `WriteCompleted`; a successful write
//!   replaces [`SimExplorer::session_file`] / [`SimExplorer::backup`].
//!   `AppendLog` and `Notify` are collected.
//! - While the shell is lost, `WindowOpened`, `TabCreated` and
//!   `NavigationCompleted` (which come from `IShellWindows`) are not
//!   delivered; the changes still happen.
//!
//! # Failure injection
//!
//! [`SimExplorer::fail`] adds a [`FailureRule`] that matches commands by
//! kind, op, request, probe, or write sequence number. A failing command is
//! recorded with `failed: true` and has no effect; what is reported instead:
//!
//! - commands with an [`OpId`] → `OperationFailed { op }` at once;
//! - `OpenWindow` → `OperationFailed { op: OpId(request.0) }` at once (the
//!   command has no op; this is the simulator's convention);
//! - `WriteSessionFile` / `PreserveBackup` → `WriteCompleted` with the rule's
//!   [`IoErrorKind`] after the usual latency;
//! - `SetBuiltinRestore` → `BuiltinRestoreSet` with the rule's
//!   [`IoErrorKind`] at once (it reports `Ok(())` at once otherwise);
//! - every other command (including `Probe`) → nothing.
//!
//! Independently of rules, a command naming a window or tab that does not
//! exist fails with `OperationFailed` if it has an op (or for `OpenWindow`
//! while the shell's process is dead), and is ignored otherwise.
//!
//! Requirements 4.2, 7.3, 7.5.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use esr_core::engine::{
    Command, Event, IoErrorKind, ItemId, Millis, OpId, ProbeId, RequestId, ShellError, TabKey,
    TimerId, TimerOwner, WinKey,
};
use esr_core::logfmt::entry::LogEntry;
use esr_core::model::{DisplayRef, LocationId, Placement, Rect, SaveCause, ShowState, WideString};
use esr_core::plan::fallback::ProbeOutcome;
use esr_core::plan::placement::FinalPlacement;
use esr_core::restore::NEW_TAB_REPOST_AFTER_MS;

use crate::explorer::{
    default_locations, NavOutcome, Op, Scheduled, Shown, SimExplorer, SimTab, SimWindow, TimedEvent,
};
use crate::fold::{ActivateMode, TabWindowFault};

/// `E_FAIL`.
pub const E_FAIL: i32 = 0x8000_4005_u32 as i32;
/// `HRESULT_FROM_WIN32(ERROR_INVALID_WINDOW_HANDLE)`: the target is gone.
pub const E_INVALID_HANDLE: i32 = 0x8007_0578_u32 as i32;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Which `CreateTab` posts produce no tab. Posts are numbered from 0 in the
/// order the simulator receives them (across all windows), counting only
/// posts to existing windows that no failure rule rejected.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum TabLoss {
    #[default]
    Never,
    /// Every `n`-th post (posts `n-1`, `2n-1`, …); R9 measured about 1 in 200.
    EveryNth(u64),
    /// Exactly these posts.
    Posts(BTreeSet<u64>),
}

impl TabLoss {
    pub fn loses(&self, post: u64) -> bool {
        match self {
            TabLoss::Never => false,
            TabLoss::EveryNth(n) => *n > 0 && (post + 1).is_multiple_of(*n),
            TabLoss::Posts(set) => set.contains(&post),
        }
    }
}

/// How `CloseTab` is carried out (R3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum TabCloseModel {
    /// The tab's UIA "Close Tab" button: closes exactly that tab.
    #[default]
    UiaCloseButton,
    /// `IWebBrowser2::Quit`: does nothing unless the tab is active or the
    /// window's only tab.
    Quit,
}

/// Restoration knobs of the simulator (part of [`SimConfig`](crate::explorer::SimConfig)).
#[derive(Clone, Debug)]
pub struct RestoreConfig {
    /// Time from `OpenWindow` to the window registering, alone (R9: ~430 ms).
    pub window_open_ms: u64,
    /// Extra time per other `OpenWindow` still in flight (R9: three at once
    /// take ~1.1 s, so 335 ms).
    pub window_open_contention_ms: u64,
    /// Time from `CreateTab` to the tab registering (R2/R4: 84–99 ms).
    pub tab_register_ms: u64,
    /// Explorer creates tabs one at a time across all windows: a tab
    /// registers no earlier than `tab_register_ms` after the previous one.
    pub serialize_tab_creation: bool,
    pub lost_tabs: TabLoss,
    /// The navigator retries a lost post on its own: one re-post, then the
    /// UIA `AddButton` (see the module docs).
    pub navigator_repost: bool,
    /// Time from invoking the UIA `AddButton` to the tab registering (R2:
    /// about 430 ms).
    pub uia_add_tab_ms: u64,
    /// What a new tab shows before it is navigated (LaunchTo: Home); `None`
    /// leaves it not navigated.
    pub new_tab_location: Option<LocationId>,
    /// UIA name of a tab whose header is stuck (pipelined creation).
    pub stuck_header_name: WideString,
    /// Time from `Navigate` to `NavigationCompleted`.
    pub navigate_ms: u64,
    pub close_model: TabCloseModel,
    /// Time from `CloseTab` to the tab closing (R3: 220–370 ms).
    pub close_tab_ms: u64,
    /// Time from `ActivateTab` to the selection changing.
    pub activate_tab_ms: u64,
    /// Time from `SelectItems` to the items being selected.
    pub select_items_ms: u64,
    /// Latency of `ApplyPlacement`, `Reveal`, `SetZOrder`, and `Focus`.
    pub window_op_ms: u64,
    /// Latency of a probe with no programmed outcome (which is `Available`).
    pub probe_ms: u64,
    /// Latency of `WriteSessionFile` and `PreserveBackup`.
    pub write_ms: u64,
    /// User-opened windows can claim outstanding requests with the same
    /// initial Location (the adapters cannot tell them apart).
    pub claim_user_windows: bool,
    /// Announce every unclaimed new window whose initial navigation completed
    /// with `InitialLocationKnown` (off by default so that tracking-only
    /// event streams stay as they are; see `SimConfig::for_restoration`).
    pub report_initial_location: bool,
}

impl Default for RestoreConfig {
    fn default() -> Self {
        RestoreConfig {
            window_open_ms: 430,
            window_open_contention_ms: 335,
            tab_register_ms: 85,
            serialize_tab_creation: true,
            lost_tabs: TabLoss::Never,
            navigator_repost: true,
            uia_add_tab_ms: 430,
            new_tab_location: Some(home_location()),
            stuck_header_name: "Home".into(),
            navigate_ms: 60,
            close_model: TabCloseModel::UiaCloseButton,
            close_tab_ms: 300,
            activate_tab_ms: 20,
            select_items_ms: 10,
            window_op_ms: 1,
            probe_ms: 5,
            write_ms: 10,
            claim_user_windows: true,
            report_initial_location: false,
        }
    }
}

/// The Home Shell_Location of the default pool.
pub fn home_location() -> LocationId {
    default_locations()
        .into_iter()
        .find(|l| l.display_name == WideString::from("Home"))
        .expect("the default pool has Home")
}

// ---------------------------------------------------------------------------
// Records
// ---------------------------------------------------------------------------

/// A command as the simulator received it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedCommand {
    pub at: Millis,
    pub command: Command,
    /// A [`FailureRule`] rejected it.
    pub failed: bool,
}

/// A `CreateTab` issued while another tab of the same window was still being
/// created (an engine bug: the tab's header stays "Home").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PipeliningViolation {
    pub at: Millis,
    pub win: WinKey,
    pub op: OpId,
}

/// How a window came to exist.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Origin {
    /// The user opened it (or dragged a tab out to form it).
    User,
    /// Opened for this `OpenWindow` request (whichever request claimed it).
    Request(RequestId),
}

/// Life of one window, kept after it closes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowRecord {
    pub key: WinKey,
    pub origin: Origin,
    /// The request whose `WindowOpened` named this window.
    pub claimed_by: Option<RequestId>,
    pub created_at: Millis,
    /// Currently visible (alpha 255).
    pub visible: bool,
    /// When the window first became visible, and its placement then.
    pub first_visible: Option<(Millis, Placement)>,
    /// Every placement the window had, with the time it took effect.
    pub placements: Vec<(Millis, Placement)>,
    /// Placement changes that happened while the window was visible.
    pub visible_placement_changes: u32,
    pub closed_at: Option<Millis>,
}

/// Which commands a [`FailureRule`] rejects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FailWhen {
    Kind(CommandKind),
    Op(OpId),
    Request(RequestId),
    Probe(ProbeId),
    /// `WriteSessionFile` or `PreserveBackup` with this `seq`.
    Write(u64),
}

/// An injected failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailureRule {
    pub when: FailWhen,
    /// How many more commands it rejects (`None` = all).
    pub remaining: Option<u32>,
    /// Reported in `OperationFailed`.
    pub error: ShellError,
    /// Reported in `WriteCompleted`.
    pub io_error: IoErrorKind,
}

impl FailureRule {
    /// Rejects every matching command with `E_FAIL` / `Other(31)`.
    pub fn always(when: FailWhen) -> Self {
        FailureRule {
            when,
            remaining: None,
            error: ShellError { code: E_FAIL, message: "injected failure".into() },
            io_error: IoErrorKind::Other(31),
        }
    }

    /// Rejects the next `n` matching commands.
    pub fn times(when: FailWhen, n: u32) -> Self {
        FailureRule { remaining: Some(n), ..Self::always(when) }
    }

    fn matches(&self, command: &Command) -> bool {
        match &self.when {
            FailWhen::Kind(k) => CommandKind::of(command) == *k,
            FailWhen::Op(op) => command_op(command) == Some(*op),
            FailWhen::Request(r) => matches!(command, Command::OpenWindow { request, .. } if request == r),
            FailWhen::Probe(p) => matches!(command, Command::Probe { probe, .. } if probe == p),
            FailWhen::Write(s) => matches!(
                command,
                Command::WriteSessionFile { seq, .. } | Command::PreserveBackup { seq, .. } if seq == s
            ),
        }
    }
}

/// The variant of a [`Command`], for failure rules and log filtering.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CommandKind {
    RescanWindow,
    RescanAll,
    WriteSessionFile,
    PreserveBackup,
    AppendLog,
    OpenWindow,
    ApplyPlacement,
    CreateTab,
    Navigate,
    CloseTab,
    ActivateTab,
    SelectItems,
    Reveal,
    SetZOrder,
    Focus,
    Probe,
    Notify,
    HideWindow,
    ProbeFoldContext,
    CloseWindow,
    ShowPrompt,
    ShowMessage,
    ShowSettings,
    SetBuiltinRestore,
    SetStartAtSignIn,
    PersistSettings,
    SetTimer,
    Exit,
}

impl CommandKind {
    pub fn of(command: &Command) -> CommandKind {
        use CommandKind as K;
        match command {
            Command::RescanWindow(_) => K::RescanWindow,
            Command::RescanAll => K::RescanAll,
            Command::WriteSessionFile { .. } => K::WriteSessionFile,
            Command::PreserveBackup { .. } => K::PreserveBackup,
            Command::AppendLog(_) => K::AppendLog,
            Command::OpenWindow { .. } => K::OpenWindow,
            Command::ApplyPlacement { .. } => K::ApplyPlacement,
            Command::CreateTab { .. } => K::CreateTab,
            Command::Navigate { .. } => K::Navigate,
            Command::CloseTab { .. } => K::CloseTab,
            Command::ActivateTab { .. } => K::ActivateTab,
            Command::SelectItems { .. } => K::SelectItems,
            Command::Reveal { .. } => K::Reveal,
            Command::SetZOrder(_) => K::SetZOrder,
            Command::Focus(_) => K::Focus,
            Command::Probe { .. } => K::Probe,
            Command::Notify { .. } => K::Notify,
            Command::HideWindow { .. } => K::HideWindow,
            Command::ProbeFoldContext { .. } => K::ProbeFoldContext,
            Command::CloseWindow { .. } => K::CloseWindow,
            Command::ShowPrompt => K::ShowPrompt,
            Command::ShowMessage(_) => K::ShowMessage,
            Command::ShowSettings => K::ShowSettings,
            Command::SetBuiltinRestore(_) => K::SetBuiltinRestore,
            Command::SetStartAtSignIn(_) => K::SetStartAtSignIn,
            Command::PersistSettings(_) => K::PersistSettings,
            Command::SetTimer { .. } => K::SetTimer,
            Command::Exit => K::Exit,
        }
    }
}

/// The op of a window/tab operation command.
pub fn command_op(command: &Command) -> Option<OpId> {
    match command {
        Command::CreateTab { op, .. }
        | Command::Navigate { op, .. }
        | Command::CloseTab { op, .. }
        | Command::ActivateTab { op, .. }
        | Command::SelectItems { op, .. } => Some(*op),
        _ => None,
    }
}

/// A user opening File Explorer on a given Location, optionally with items
/// selected (e.g. "Open file location"; the trigger of a Targeted_Launch).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    pub location: LocationId,
    pub selection: Vec<ItemId>,
    pub rect: Rect,
    pub maximized: bool,
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// The effect of a restoration command, run when it is due.
#[derive(Clone, Debug)]
pub(crate) enum Action {
    WindowAppears { request: RequestId, location: LocationId, placement: FinalPlacement },
    TabAppears { op: OpId, win: WinKey, stuck: bool },
    /// The navigator's next attempt for a lost new-tab post (1: re-post,
    /// 2: UIA `AddButton`).
    RetryNewTab { op: OpId, win: WinKey, attempt: u8 },
    NavigationDone { tab: TabKey, location: LocationId, generation: u64 },
    CloseTab { op: OpId, tab: TabKey },
    /// `confirm`: the adapter reports the selection (`ActivationConfirmed`).
    ActivateTab { op: OpId, win: WinKey, index: usize, confirm: bool },
    SelectItems { op: OpId, tab: TabKey, items: Vec<ItemId> },
    ApplyPlacement { win: WinKey, placement: FinalPlacement },
    Reveal(WinKey),
    ZOrder(Vec<WinKey>),
    Focus(WinKey),
    WriteDone { seq: u64, content: Written, result: Result<(), IoErrorKind> },
}

#[derive(Clone, Debug)]
pub(crate) enum Written {
    Session(String),
    Backup(Vec<u8>),
}

/// Restoration bookkeeping held by [`SimExplorer`].
#[derive(Clone, Debug, Default)]
pub(crate) struct RestoreState {
    commands: Vec<RecordedCommand>,
    /// Outstanding `OpenWindow` requests, oldest first.
    requests: Vec<(RequestId, LocationId)>,
    opens_in_flight: u64,
    /// When the (serialized) tab-creation lane is free again.
    tab_lane_free: Millis,
    tab_posts: u64,
    /// Created but unregistered tabs per window.
    outstanding: BTreeMap<WinKey, u32>,
    violations: Vec<PipeliningViolation>,
    nav_generation: BTreeMap<TabKey, u64>,
    next_generation: u64,
    probes: HashMap<LocationId, (ProbeOutcome, u64)>,
    failures: Vec<FailureRule>,
    /// Every window ever created, in creation order.
    records: Vec<WindowRecord>,
    /// Z-order of windows that have been raised, topmost first.
    z: Vec<WinKey>,
    focused: Option<WinKey>,
    selections: BTreeMap<TabKey, Vec<ItemId>>,
    session_file: Option<String>,
    backup: Option<Vec<u8>>,
    log: Vec<(Millis, LogEntry)>,
    notifications: Vec<(Millis, u32)>,
}

impl RestoreState {
    pub(crate) fn record_mut(&mut self, key: WinKey) -> Option<&mut WindowRecord> {
        self.records.iter_mut().rev().find(|r| r.key == key)
    }

    /// The items selected in `tab` are `items`.
    pub(crate) fn set_selection(&mut self, tab: TabKey, items: Vec<ItemId>) {
        self.selections.insert(tab, items);
    }

    /// Put `key` on top of the z-order.
    pub(crate) fn raise(&mut self, key: WinKey) {
        self.z.retain(|k| *k != key);
        self.z.insert(0, key);
    }

    /// The placement of `key` is now `placement` (called on every sync).
    pub(crate) fn note_placement(&mut self, key: WinKey, now: Millis, placement: &Placement) {
        let Some(r) = self.record_mut(key) else { return };
        if r.placements.last().is_some_and(|(_, p)| p == placement) {
            return;
        }
        if r.visible && !r.placements.is_empty() {
            r.visible_placement_changes += 1;
        }
        r.placements.push((now, placement.clone()));
    }

    pub(crate) fn window_removed(&mut self, key: WinKey, now: Millis, tabs: &[SimTab]) {
        if let Some(r) = self.record_mut(key) {
            r.closed_at = Some(now);
            r.visible = false;
        }
        self.z.retain(|k| *k != key);
        self.outstanding.remove(&key);
        if self.focused == Some(key) {
            self.focused = None;
        }
        for t in tabs {
            self.forget_tab(t.key);
        }
    }

    fn forget_tab(&mut self, tab: TabKey) {
        self.selections.remove(&tab);
        self.nav_generation.remove(&tab);
    }

    /// The oldest outstanding request for `location`, removed.
    pub(crate) fn claim(&mut self, location: &LocationId) -> Option<RequestId> {
        let i = self.requests.iter().position(|(_, l)| l == location)?;
        Some(self.requests.remove(i).0)
    }

    fn take_failure(&mut self, command: &Command) -> Option<FailureRule> {
        let i = self.failures.iter().position(|f| f.matches(command) && f.remaining != Some(0))?;
        let rule = &mut self.failures[i];
        if let Some(n) = &mut rule.remaining {
            *n -= 1;
        }
        Some(rule.clone())
    }
}

fn missing(what: &str) -> ShellError {
    ShellError { code: E_INVALID_HANDLE, message: format!("no such {what}") }
}

// ---------------------------------------------------------------------------
// SimExplorer: restoration
// ---------------------------------------------------------------------------

impl SimExplorer {
    // --- programming -------------------------------------------------------

    /// Probes of `location` complete with `outcome` after `latency_ms`.
    pub fn set_probe(&mut self, location: LocationId, outcome: ProbeOutcome, latency_ms: u64) {
        self.restore.probes.insert(location, (outcome, latency_ms));
    }

    /// Add a failure rule; earlier rules are tried first.
    pub fn fail(&mut self, rule: FailureRule) {
        self.restore.failures.push(rule);
    }

    // --- observation -------------------------------------------------------

    /// Every command received, in order, with its logical timestamp.
    pub fn command_log(&self) -> &[RecordedCommand] {
        &self.restore.commands
    }

    /// The recorded commands of one kind.
    pub fn commands_of(&self, kind: CommandKind) -> Vec<&RecordedCommand> {
        self.restore.commands.iter().filter(|c| CommandKind::of(&c.command) == kind).collect()
    }

    /// Every `CreateTab` issued while its window had another tab being created.
    pub fn pipelining_violations(&self) -> &[PipeliningViolation] {
        &self.restore.violations
    }

    /// Whether the engine ever pipelined tab creation (see the module docs).
    pub fn pipelining_violated(&self) -> bool {
        !self.restore.violations.is_empty()
    }

    /// Every window ever created (including closed ones), in creation order.
    pub fn window_records(&self) -> &[WindowRecord] {
        &self.restore.records
    }

    pub fn window_record(&self, key: WinKey) -> Option<&WindowRecord> {
        self.restore.records.iter().rev().find(|r| r.key == key)
    }

    /// Whether `win` is open and visible.
    pub fn is_visible(&self, win: WinKey) -> bool {
        self.window(win).is_some() && self.window_record(win).is_none_or(|r| r.visible)
    }

    /// Outstanding (unclaimed) `OpenWindow` requests, oldest first.
    pub fn outstanding_requests(&self) -> &[(RequestId, LocationId)] {
        &self.restore.requests
    }

    /// Live windows, topmost first. Windows never raised (by activation,
    /// `SetZOrder`, or creation) follow in creation order.
    pub fn z_order(&self) -> Vec<WinKey> {
        let live: Vec<WinKey> = self.windows.iter().map(|w| w.key).collect();
        let mut order: Vec<WinKey> = self.restore.z.iter().copied().filter(|k| live.contains(k)).collect();
        order.extend(live.into_iter().filter(|k| !self.restore.z.contains(k)));
        order
    }

    /// The target of the last applied `Focus` command, while it is open.
    pub fn focused(&self) -> Option<WinKey> {
        self.restore.focused
    }

    /// The items selected in `tab` (by `SelectItems` or a [`Launch`]).
    pub fn selection(&self, tab: TabKey) -> &[ItemId] {
        self.restore.selections.get(&tab).map_or(&[], Vec::as_slice)
    }

    /// The content of the last successful `WriteSessionFile`.
    pub fn session_file(&self) -> Option<&str> {
        self.restore.session_file.as_deref()
    }

    /// The content of the last successful `PreserveBackup`.
    pub fn backup(&self) -> Option<&[u8]> {
        self.restore.backup.as_deref()
    }

    /// Every `AppendLog` entry with its timestamp.
    pub fn log_entries(&self) -> &[(Millis, LogEntry)] {
        &self.restore.log
    }

    /// Every `Notify { omitted_tabs }` with its timestamp.
    pub fn notifications(&self) -> &[(Millis, u32)] {
        &self.restore.notifications
    }

    /// UIA names of `win`'s tabs, left to right (what the tab strip shows).
    pub fn tab_names(&self, win: WinKey) -> Vec<WideString> {
        self.window(win).map_or_else(Vec::new, |w| w.tabs.iter().map(|t| self.uia_name(t)).collect())
    }

    // --- user actions ------------------------------------------------------

    /// The user opens File Explorer on `launch.location` in the shell's
    /// process (no-op while it is dead). Like [`Op::OpenWindow`] with a
    /// completed navigation, plus an initial item selection.
    pub fn launch(&mut self, launch: &Launch) -> Vec<TimedEvent> {
        if let Some(pid) = self.main_pid
            && self.windows.len() < self.config.max_windows
        {
            let key = WinKey { hwnd: self.alloc_hwnd(), seq: self.alloc_seq() };
            let tab = self.new_tab(launch.location.clone(), NavOutcome::Complete);
            let tab_key = tab.key;
            let state = if launch.maximized { ShowState::Maximized } else { ShowState::Normal };
            let placement = self.placement(launch.rect, state);
            self.windows.push(SimWindow { key, pid, tabs: vec![tab], active: 0, placement });
            self.track_window(key, Origin::User, true);
            let wi = self.windows.len() - 1;
            self.sync(wi);
            self.activate(key);
            self.window_changed(key);
            self.register(wi, 0);
            if !launch.selection.is_empty() {
                self.restore.selections.insert(tab_key, launch.selection.clone());
            }
            self.announce_user_window(key, Some(launch.location.clone()), launch.selection.clone());
        }
        self.take_events()
    }

    // --- hooks used by explorer.rs -------------------------------------------

    /// Start the record of a new window (before its first sync).
    pub(crate) fn track_window(&mut self, key: WinKey, origin: Origin, visible: bool) {
        let now = self.now;
        let first_visible = visible.then(|| self.window(key).map(|w| (now, w.placement.clone()))).flatten();
        self.restore.records.push(WindowRecord {
            key,
            origin,
            claimed_by: None,
            created_at: now,
            visible,
            first_visible,
            placements: Vec::new(),
            visible_placement_changes: 0,
            closed_at: None,
        });
        self.restore.raise(key);
        self.fold_window_created(key);
    }

    /// A user-opened window registered: claim a matching request, or announce
    /// it as a possible Trigger_Window.
    pub(crate) fn announce_user_window(&mut self, key: WinKey, initial: Option<LocationId>, selection: Vec<ItemId>) {
        let Some(location) = initial else { return };
        if self.config.restore.claim_user_windows
            && let Some(request) = self.restore.claim(&location)
        {
            self.claimed(key, request);
            return;
        }
        if self.config.restore.report_initial_location && !self.shell_lost {
            let tab = self.window(key).and_then(|w| w.tabs.first()).map(|t| t.key);
            self.emit(Event::InitialLocationKnown { win: key, tab, location, selection });
        }
    }

    pub(crate) fn claimed(&mut self, win: WinKey, request: RequestId) {
        if let Some(r) = self.restore.record_mut(win) {
            r.claimed_by = Some(request);
        }
        if !self.shell_lost {
            let tab = self.window(win).and_then(|w| w.tabs.first()).map(|t| t.key);
            self.emit(Event::WindowOpened { request, win, tab });
        }
    }

    fn fail_op(&mut self, op: OpId, error: ShellError) {
        self.schedule(self.now, Scheduled::Emit(Event::OperationFailed { op, error }));
    }

    fn after(&mut self, ms: u64, action: Action) {
        self.schedule(self.now + ms, Scheduled::Restore(action));
    }

    /// Record `command` and schedule its effect.
    pub(crate) fn carry_out(&mut self, command: &Command) {
        let failure = self.restore.take_failure(command);
        self.restore.commands.push(RecordedCommand { at: self.now, command: command.clone(), failed: failure.is_some() });
        let cfg = &self.config.restore;
        let (open_ms, contention_ms) = (cfg.window_open_ms, cfg.window_open_contention_ms);
        let (navigate_ms, close_ms, activate_ms, select_ms, window_ms, probe_ms, write_ms) = (
            cfg.navigate_ms,
            cfg.close_tab_ms,
            cfg.activate_tab_ms,
            cfg.select_items_ms,
            cfg.window_op_ms,
            cfg.probe_ms,
            cfg.write_ms,
        );

        if let Some(rule) = failure {
            match command {
                Command::OpenWindow { request, .. } => self.fail_op(OpId(request.0), rule.error),
                Command::WriteSessionFile { seq, text, .. } => self.after(
                    write_ms,
                    Action::WriteDone { seq: *seq, content: Written::Session(text.clone()), result: Err(rule.io_error) },
                ),
                Command::PreserveBackup { seq, content } => self.after(
                    write_ms,
                    Action::WriteDone { seq: *seq, content: Written::Backup(content.clone()), result: Err(rule.io_error) },
                ),
                Command::SetBuiltinRestore(enabled) => self.schedule(
                    self.now,
                    Scheduled::Emit(Event::BuiltinRestoreSet { enabled: *enabled, result: Err(rule.io_error) }),
                ),
                other => {
                    if let Some(op) = command_op(other) {
                        self.fail_op(op, rule.error);
                    }
                }
            }
            return;
        }

        match command {
            Command::RescanWindow(win) => self.request_rescan(*win),
            Command::RescanAll => {
                let keys: Vec<WinKey> = self.windows().iter().map(|w| w.key).collect();
                for win in keys {
                    self.request_rescan(win);
                }
            }
            Command::WriteSessionFile { seq, text, .. } => self.after(
                write_ms,
                Action::WriteDone { seq: *seq, content: Written::Session(text.clone()), result: Ok(()) },
            ),
            Command::PreserveBackup { seq, content } => self.after(
                write_ms,
                Action::WriteDone { seq: *seq, content: Written::Backup(content.clone()), result: Ok(()) },
            ),
            Command::AppendLog(entry) => self.restore.log.push((self.now, entry.clone())),
            Command::OpenWindow { request, location, placement } => {
                if self.main_pid.is_none() {
                    self.fail_op(OpId(request.0), missing("shell process"));
                    return;
                }
                let latency = open_ms + contention_ms * self.restore.opens_in_flight;
                self.restore.opens_in_flight += 1;
                self.restore.requests.push((*request, location.clone()));
                self.after(
                    latency,
                    Action::WindowAppears { request: *request, location: location.clone(), placement: *placement },
                );
            }
            Command::ApplyPlacement { win, placement } => {
                self.after(window_ms, Action::ApplyPlacement { win: *win, placement: *placement })
            }
            Command::CreateTab { op, win } => {
                if self.wi(*win).is_none() {
                    self.fail_op(*op, missing("window"));
                    return;
                }
                self.fold_create_tab_command(*op, *win);
                self.new_tab_attempt(*op, *win, 0);
            }
            Command::Navigate { op, tab, location } => {
                if self.find_tab(*tab).is_none() {
                    self.fail_op(*op, missing("tab"));
                    return;
                }
                self.restore.next_generation += 1;
                let generation = self.restore.next_generation;
                self.restore.nav_generation.insert(*tab, generation);
                self.after(navigate_ms, Action::NavigationDone { tab: *tab, location: location.clone(), generation });
            }
            Command::CloseTab { op, tab } => {
                if self.find_tab(*tab).is_none() {
                    self.fail_op(*op, missing("tab"));
                    return;
                }
                self.after(close_ms, Action::CloseTab { op: *op, tab: *tab });
            }
            Command::ActivateTab { op, win, index } => match self.window(*win) {
                Some(w) if *index < w.tabs.len() => {
                    // Fault injection (Req 14.23): late, unconfirmed or ignored.
                    let (extra_ms, confirm) = match self.fold_activate_command(*win) {
                        ActivateMode::Normal => (0, true),
                        ActivateMode::Late { extra_ms } => (extra_ms, true),
                        ActivateMode::Unconfirmed => (0, false),
                        ActivateMode::Ignored => return,
                    };
                    self.after(
                        activate_ms + extra_ms,
                        Action::ActivateTab { op: *op, win: *win, index: *index, confirm },
                    )
                }
                Some(_) => self.fail_op(*op, missing("tab index")),
                None => self.fail_op(*op, missing("window")),
            },
            Command::SelectItems { op, tab, items } => {
                if self.find_tab(*tab).is_none() {
                    self.fail_op(*op, missing("tab"));
                    return;
                }
                self.after(select_ms, Action::SelectItems { op: *op, tab: *tab, items: items.clone() });
            }
            Command::Reveal { win } => self.after(window_ms, Action::Reveal(*win)),
            Command::SetZOrder(order) => self.after(window_ms, Action::ZOrder(order.clone())),
            Command::Focus(win) => self.after(window_ms, Action::Focus(*win)),
            Command::Probe { probe, location } => {
                let (outcome, latency) =
                    self.restore.probes.get(location).cloned().unwrap_or((ProbeOutcome::Available, probe_ms));
                self.schedule(self.now + latency, Scheduled::Emit(Event::ProbeCompleted { probe: *probe, outcome }));
            }
            Command::Notify { omitted_tabs } => self.restore.notifications.push((self.now, *omitted_tabs)),
            // The option write has no effect on open windows; its result is
            // reported like the shell does (the command has no op id).
            Command::SetBuiltinRestore(enabled) => self.schedule(
                self.now,
                Scheduled::Emit(Event::BuiltinRestoreSet { enabled: *enabled, result: Ok(()) }),
            ),
            // Timers belong to the driver; the rest has no effect on Explorer.
            // Candidate windows (Req 14.16, 14.27); see `crate::fold`.
            Command::HideWindow { win } => self.fold_hide_command(*win),
            Command::CloseWindow { win } => self.fold_close_command(*win),
            Command::ProbeFoldContext { probe, candidate } => self.fold_probe_command(*probe, *candidate),
            Command::SetTimer { .. }
            | Command::ShowPrompt
            | Command::ShowMessage(_)
            | Command::ShowSettings
            | Command::SetStartAtSignIn(_)
            | Command::PersistSettings(_)
            | Command::Exit => {}
        }
    }

    /// Run a due restoration action.
    pub(crate) fn run_action(&mut self, action: Action) {
        match action {
            Action::WindowAppears { request, location, placement } => {
                self.restore.opens_in_flight = self.restore.opens_in_flight.saturating_sub(1);
                self.window_appears(request, location, &placement);
            }
            Action::TabAppears { op, win, stuck } => self.tab_appears(op, win, stuck),
            Action::RetryNewTab { op, win, attempt } => {
                if self.wi(win).is_none() {
                    return self.fail_op(op, missing("window"));
                }
                self.new_tab_attempt(op, win, attempt);
            }
            Action::NavigationDone { tab, location, generation } => {
                if self.restore.nav_generation.get(&tab) != Some(&generation) {
                    return;
                }
                self.restore.nav_generation.remove(&tab);
                let Some((wi, ti)) = self.find_tab(tab) else { return };
                self.restore.selections.remove(&tab);
                self.complete_navigation(wi, ti, Some(location));
            }
            Action::CloseTab { op, tab } => self.close_tab_now(op, tab),
            Action::ActivateTab { op, win, index, confirm } => {
                let Some(wi) = self.wi(win) else { return self.fail_op(op, missing("window")) };
                if index >= self.windows[wi].tabs.len() {
                    return self.fail_op(op, missing("tab index"));
                }
                if self.windows[wi].active != index {
                    self.windows[wi].active = index;
                    self.sync(wi);
                    self.window_changed(win);
                }
                // The adapter reports a selection it saw take effect.
                if confirm {
                    self.schedule(self.now, Scheduled::Emit(Event::ActivationConfirmed { op }));
                }
            }
            Action::SelectItems { op, tab, items } => {
                if self.find_tab(tab).is_none() {
                    return self.fail_op(op, missing("tab"));
                }
                self.restore.selections.insert(tab, items);
            }
            Action::ApplyPlacement { win, placement } => {
                let Some(wi) = self.wi(win) else { return };
                self.windows[wi].placement = self.final_placement(&placement);
                self.sync(wi);
                self.window_changed(win);
            }
            Action::Reveal(win) => {
                if self.fold_reveal(win) {
                    return;
                }
                let Some(placement) = self.window(win).map(|w| w.placement.clone()) else { return };
                let now = self.now;
                if let Some(r) = self.restore.record_mut(win) {
                    r.visible = true;
                    r.first_visible.get_or_insert((now, placement));
                }
            }
            Action::ZOrder(order) => {
                for key in order.into_iter().rev() {
                    if self.wi(key).is_some() {
                        self.restore.raise(key);
                    }
                }
            }
            Action::Focus(win) => {
                if self.wi(win).is_some() {
                    self.activate(win);
                    self.restore.focused = Some(win);
                }
            }
            Action::WriteDone { seq, content, result } => {
                if result.is_ok() {
                    match content {
                        Written::Session(text) => self.restore.session_file = Some(text),
                        Written::Backup(bytes) => self.restore.backup = Some(bytes),
                    }
                }
                self.emit(Event::WriteCompleted { seq, result });
            }
        }
    }

    /// The `Placement` a window has after `SetWindowPlacement(final)`.
    fn final_placement(&self, fp: &FinalPlacement) -> Placement {
        let display = match self.config.displays.iter().find(|d| d.monitor == fp.monitor) {
            Some(d) => DisplayRef { device: d.device.clone(), monitor: fp.monitor },
            None => self.placement(fp.restored, fp.state).display,
        };
        Placement { state: fp.state, restored: fp.restored, display }
    }

    fn window_appears(&mut self, request: RequestId, location: LocationId, fp: &FinalPlacement) {
        let Some(pid) = self.main_pid else { return };
        let key = WinKey { hwnd: self.alloc_hwnd(), seq: self.alloc_seq() };
        let tab = self.new_tab(location.clone(), NavOutcome::Complete);
        let placement = self.final_placement(fp);
        self.windows.push(SimWindow { key, pid, tabs: vec![tab], active: 0, placement });
        // Hidden (alpha 0) and placed at EVENT_OBJECT_CREATE, before any frame.
        self.track_window(key, Origin::Request(request), false);
        let wi = self.windows.len() - 1;
        self.sync(wi);
        self.window_changed(key);
        match self.restore.claim(&location) {
            Some(claimer) => self.claimed(key, claimer),
            None => {
                if self.config.restore.report_initial_location && !self.shell_lost {
                    let tab = self.window(key).and_then(|w| w.tabs.first()).map(|t| t.key);
                    self.emit(Event::InitialLocationKnown { win: key, tab, location, selection: Vec::new() });
                }
            }
        }
        self.register(wi, 0);
    }

    /// One attempt of the navigator's new-tab sequence for `CreateTab { op }`:
    /// attempts 0 and 1 post the command (and may be lost), attempt 2 invokes
    /// the UIA `AddButton` (never lost).
    fn new_tab_attempt(&mut self, op: OpId, win: WinKey, attempt: u8) {
        // Fault injection (Req 14.13): the tab window appears late or never.
        let extra_ms = match self.create_tab_fault(op).tab_window {
            TabWindowFault::Normal => 0,
            TabWindowFault::Late { extra_ms } => extra_ms,
            TabWindowFault::Never => {
                self.take_create_tab_fault(op);
                return;
            }
        };
        let cfg = &self.config.restore;
        let (serialize, uia) = (cfg.serialize_tab_creation, attempt >= 2);
        let register_ms = if uia { cfg.uia_add_tab_ms } else { cfg.tab_register_ms };
        if !uia {
            let post = self.restore.tab_posts;
            self.restore.tab_posts += 1;
            if self.config.restore.lost_tabs.loses(post) {
                if self.config.restore.navigator_repost {
                    self.after(NEW_TAB_REPOST_AFTER_MS, Action::RetryNewTab { op, win, attempt: attempt + 1 });
                }
                return;
            }
        }
        let outstanding = self.restore.outstanding.entry(win).or_insert(0);
        let stuck = *outstanding > 0;
        *outstanding += 1;
        if stuck {
            self.restore.violations.push(PipeliningViolation { at: self.now, win, op });
        }
        let start = if serialize { self.now.max(self.restore.tab_lane_free) } else { self.now };
        let at = start + register_ms + extra_ms;
        if serialize {
            self.restore.tab_lane_free = at;
        }
        self.schedule(at, Scheduled::Restore(Action::TabAppears { op, win, stuck }));
    }

    /// The new tab window appears: `TabStarted` (if reported), then the tab
    /// registers and is reported by `TabCreated` right away or, with a
    /// registration lag or fault, later. A tab being created counts as
    /// outstanding until it registers.
    fn tab_appears(&mut self, op: OpId, win: WinKey, stuck: bool) {
        let fault = self.take_create_tab_fault(op);
        let lag_ms = self.config.fold.registration_lag_ms + fault.registration_delay_ms;
        if lag_ms == 0 {
            self.tab_registered(win);
        }
        let Some(wi) = self.wi(win) else { return };
        let key = TabKey { hwnd: self.alloc_hwnd(), seq: self.alloc_seq() };
        let initial = self.config.restore.new_tab_location.clone();
        let tab = SimTab {
            key,
            registration: None,
            shown: initial.clone().map_or(Shown::NotNavigated, Shown::Known),
            pending: None,
            last_known: initial,
            stuck_header: stuck,
        };
        let w = &mut self.windows[wi];
        w.tabs.push(tab);
        w.active = w.tabs.len() - 1;
        let ti = w.active;
        self.sync(wi);
        if self.config.fold.report_tab_started {
            self.emit(Event::TabStarted { op, tab: key });
        }
        if lag_ms == 0 {
            if !self.shell_lost {
                self.emit(Event::TabCreated { win, tab: key });
            }
            self.register(wi, ti);
        } else {
            self.register_tab_later(win, key, lag_ms);
        }
    }

    /// The tab being created in `win` registered (or its window is gone).
    pub(crate) fn tab_registered(&mut self, win: WinKey) {
        if let Some(n) = self.restore.outstanding.get_mut(&win) {
            *n = n.saturating_sub(1);
        }
    }

    fn close_tab_now(&mut self, op: OpId, tab: TabKey) {
        let Some((wi, ti)) = self.find_tab(tab) else { return self.fail_op(op, missing("tab")) };
        let w = &self.windows[wi];
        let (win, pid, n, registered) = (w.key, w.pid, w.tabs.len(), w.tabs[ti].registration.is_some());
        if self.config.restore.close_model == TabCloseModel::Quit && n > 1 && w.active != ti {
            return;
        }
        if n == 1 {
            let cause = if registered && !self.shell_lost { SaveCause::LastTabClosed } else { SaveCause::LastWindowClosed };
            self.reference.close_window(win, cause);
            self.remove_window(wi);
            if registered {
                self.revoke_now(win, tab);
            }
            self.emit(Event::WindowDestroyed { win });
            self.exit_if_idle(pid);
        } else {
            let t = self.remove_tab(wi, ti);
            self.restore.forget_tab(tab);
            if t.registration.is_some() {
                self.revoke_now(win, tab);
            }
            self.sync(wi);
            self.window_changed(win);
        }
    }
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

/// Something the user does at a scripted time while the driver runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UserAction {
    Op(Op),
    Launch(Launch),
}

/// Why [`Driver::run`] returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Stop {
    /// Nothing is scheduled any more (apart from timers of quiet owners).
    Quiescent,
    /// The next thing to happen lies beyond the deadline.
    Deadline,
    /// The event limit was reached.
    EventLimit,
}

/// The result of [`run_until_quiescent`].
#[derive(Debug)]
pub struct Run {
    pub stop: Stop,
    /// Every event delivered to the handler, in delivery order.
    pub events: Vec<TimedEvent>,
    /// Every command the handler issued, with its timestamp.
    pub commands: Vec<RecordedCommand>,
    /// Timers still pending.
    pub timers: BTreeMap<TimerId, Millis>,
    /// Final state.
    pub sim: SimExplorer,
}

/// Runs a handler (an Engine) against a [`SimExplorer`]: delivers every event
/// at its logical time, carries out every returned command (recording it),
/// fires `SetTimer` timers, and plays scripted user actions.
///
/// Same-time ordering: simulator items first, then timers, then user actions.
#[derive(Debug)]
pub struct Driver {
    sim: SimExplorer,
    pending: VecDeque<TimedEvent>,
    /// `(at, insertion seq)` → action.
    script: BTreeMap<(Millis, u64), UserAction>,
    script_seq: u64,
    timers: BTreeMap<TimerId, Millis>,
    events: Vec<TimedEvent>,
    deadline: Millis,
    max_events: usize,
    quiet_owners: BTreeSet<TimerOwner>,
}

impl Driver {
    /// A driver with a 10-minute deadline (from the sim's current time) and
    /// a limit of 200,000 events.
    pub fn new(sim: SimExplorer) -> Self {
        let deadline = sim.now() + 600_000;
        Driver {
            sim,
            pending: VecDeque::new(),
            script: BTreeMap::new(),
            script_seq: 0,
            timers: BTreeMap::new(),
            events: Vec::new(),
            deadline,
            max_events: 200_000,
            quiet_owners: BTreeSet::new(),
        }
    }

    pub fn sim(&self) -> &SimExplorer {
        &self.sim
    }

    pub fn sim_mut(&mut self) -> &mut SimExplorer {
        &mut self.sim
    }

    pub fn set_deadline(&mut self, deadline: Millis) {
        self.deadline = deadline;
    }

    pub fn set_max_events(&mut self, max: usize) {
        self.max_events = max;
    }

    /// Timers of `owner` do not keep the run alive (e.g. periodic checkpoints).
    pub fn quiet_timers(&mut self, owner: TimerOwner) {
        self.quiet_owners.insert(owner);
    }

    /// Queue events for delivery (e.g. those of operations applied before).
    pub fn deliver(&mut self, events: impl IntoIterator<Item = TimedEvent>) {
        self.pending.extend(events);
    }

    /// Queue one event stamped with the current time.
    pub fn deliver_now(&mut self, event: Event) {
        let at = self.sim.now();
        self.pending.push_back(TimedEvent { at, event });
    }

    /// Carry out commands at the current time (e.g. those of `Engine::new`).
    pub fn submit(&mut self, commands: impl IntoIterator<Item = Command>) {
        for command in commands {
            if let Command::SetTimer { id, at } = &command {
                self.timers.insert(*id, (*at).max(self.sim.now()));
            }
            self.sim.handle_command(&command);
        }
    }

    /// Play `action` at `at` (or at once if `at` has passed).
    pub fn schedule(&mut self, at: Millis, action: UserAction) {
        self.script.insert((at, self.script_seq), action);
        self.script_seq += 1;
    }

    /// Pending timers.
    pub fn timers(&self) -> &BTreeMap<TimerId, Millis> {
        &self.timers
    }

    /// Events delivered so far.
    pub fn events(&self) -> &[TimedEvent] {
        &self.events
    }

    /// Run until quiescence, the deadline, or the event limit.
    pub fn run(&mut self, mut handler: impl FnMut(Millis, Event) -> Vec<Command>) -> Stop {
        loop {
            while let Some(ev) = self.pending.pop_front() {
                if self.events.len() >= self.max_events {
                    self.pending.push_front(ev);
                    return Stop::EventLimit;
                }
                self.events.push(ev.clone());
                let commands = handler(ev.at, ev.event);
                self.submit(commands);
            }
            let next_sim = self.sim.next_due();
            let next_timer = self.timers.iter().map(|(id, at)| (*at, *id)).min();
            let next_script = self.script.keys().next().map(|&(at, _)| at.max(self.sim.now()));
            let loud_timer = self.timers.iter().any(|(id, _)| !self.quiet_owners.contains(&id.owner));
            if next_sim.is_none() && next_script.is_none() && !loud_timer {
                return Stop::Quiescent;
            }
            let next = [next_sim, next_timer.map(|t| t.0), next_script].into_iter().flatten().min().expect("something");
            if next > self.deadline {
                return Stop::Deadline;
            }
            if next_sim == Some(next) {
                let events = self.sim.step();
                self.pending.extend(events);
            } else if let Some((at, id)) = next_timer.filter(|t| t.0 == next) {
                self.timers.remove(&id);
                let events = self.sim.tick(at.since(self.sim.now()));
                self.pending.extend(events);
                self.pending.push_back(TimedEvent { at: at.max(self.sim.now()), event: Event::Timer(id) });
            } else {
                let (&key, _) = self.script.iter().next().expect("script step");
                let action = self.script.remove(&key).expect("present");
                let events = self.sim.tick(next.since(self.sim.now()));
                self.pending.extend(events);
                let events = match &action {
                    UserAction::Op(op) => self.sim.apply(op),
                    UserAction::Launch(launch) => self.sim.launch(launch),
                };
                self.pending.extend(events);
            }
        }
    }

    pub fn finish(self, stop: Stop) -> Run {
        Run {
            stop,
            events: self.events,
            commands: self.sim.command_log().to_vec(),
            timers: self.timers,
            sim: self.sim,
        }
    }
}

/// Deliver `events` (e.g. the trigger window's) to `handler` and run the
/// simulator until quiescence (or the default deadline).
pub fn run_until_quiescent(
    sim: SimExplorer,
    events: impl IntoIterator<Item = TimedEvent>,
    handler: impl FnMut(Millis, Event) -> Vec<Command>,
) -> Run {
    let mut driver = Driver::new(sim);
    driver.deliver(events);
    let stop = driver.run(handler);
    driver.finish(stop)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explorer::{Pick, ProcessChoice, SimConfig};
    use esr_core::engine::{Engine, Environment, SessionFileRead, StartupInput};
    use esr_core::settings::Settings;

    fn loc(i: usize) -> LocationId {
        default_locations()[i].clone()
    }

    fn names(s: &SimExplorer, win: WinKey) -> Vec<String> {
        s.tab_names(win).iter().map(WideString::to_string_lossy).collect()
    }

    /// A Normal placement 800×600 at (`left`, 50), on the display it lies on.
    fn fp(left: i32) -> FinalPlacement {
        let d = &crate::explorer::default_displays()[usize::from(left >= 1920)];
        FinalPlacement {
            state: ShowState::Normal,
            restored: Rect { left, top: 50, right: left + 800, bottom: 650 },
            monitor: d.monitor,
            work: d.work,
        }
    }

    fn open_op(l: u16) -> Op {
        Op::OpenWindow {
            process: ProcessChoice::Main,
            location: Pick(l),
            nav: NavOutcome::Complete,
            rect: Rect { left: 100, top: 100, right: 900, bottom: 700 },
            maximized: false,
        }
    }

    fn open_cmd(request: u64, l: usize, left: i32) -> Command {
        Command::OpenWindow { request: RequestId(request), location: loc(l), placement: fp(left) }
    }

    fn evs(ev: &[TimedEvent]) -> Vec<&Event> {
        ev.iter().filter(|e| !matches!(e.event, Event::WindowScanned(_))).map(|e| &e.event).collect()
    }

    /// A sim with a settled trigger window on Documents; returns it and the trigger.
    fn with_trigger(config: SimConfig) -> (SimExplorer, WinKey) {
        let mut s = SimExplorer::new(config);
        s.apply(&open_op(2));
        s.settle();
        let trigger = s.windows()[0].key;
        (s, trigger)
    }

    #[test]
    fn open_window_is_hidden_placed_claimed_and_not_activated() {
        let (mut s, trigger) = with_trigger(SimConfig::default());
        let t0 = s.now();
        assert!(s.handle_command(&open_cmd(1, 3, 1000)));
        assert_eq!(s.outstanding_requests().len(), 1);
        assert!(evs(&s.tick(429)).is_empty());
        let ev = s.tick(1);
        let w = &s.windows()[1];
        let (win, tab) = (w.key, w.tabs[0].key);
        assert_eq!(
            evs(&ev),
            vec![
                &Event::WindowOpened { request: RequestId(1), win, tab: Some(tab) },
                &Event::NavigationCompleted { tab, location: loc(3) }
            ]
        );
        assert!(ev.iter().all(|e| e.at == t0 + 430));
        assert_eq!(w.placement.restored, fp(1000).restored);
        assert_eq!(w.placement.display.device, r"\\.\DISPLAY1");
        assert!(!s.is_visible(win));
        assert!(s.is_visible(trigger));
        assert_eq!(s.foreground(), Some(trigger), "SW_SHOWNOACTIVATE");
        let r = s.window_record(win).unwrap();
        assert_eq!((r.origin, r.claimed_by, r.first_visible.clone()), (Origin::Request(RequestId(1)), Some(RequestId(1)), None));
        assert!(s.outstanding_requests().is_empty());
        // The tracker sees the hidden window like any other.
        assert!(s.tick(100).iter().any(|e| matches!(&e.event, Event::WindowScanned(o) if o.win == win)));
    }

    #[test]
    fn parallel_opens_contend_and_claim_fifo_by_location() {
        let (mut s, _) = with_trigger(SimConfig::for_restoration());
        let t0 = s.now();
        for c in [open_cmd(1, 3, 100), open_cmd(2, 3, 200), open_cmd(3, 4, 300)] {
            s.handle_command(&c);
        }
        let mut ev = s.tick(100);
        // A user window on Downloads claims the oldest Downloads request…
        ev.extend(s.apply(&open_op(3)));
        let user = s.windows()[1].key;
        ev.extend(s.tick(100));
        // …and one on Backup matches none and is announced.
        ev.extend(s.apply(&open_op(5)));
        let other = s.windows()[2].key;
        ev.extend(s.tick(2000));

        let opened: Vec<(Millis, RequestId, WinKey)> = ev
            .iter()
            .filter_map(|e| match e.event {
                Event::WindowOpened { request, win, .. } => Some((e.at, request, win)),
                _ => None,
            })
            .collect();
        let req_wins: Vec<WinKey> = s
            .window_records()
            .iter()
            .filter(|r| matches!(r.origin, Origin::Request(_)))
            .map(|r| r.key)
            .collect();
        assert_eq!(req_wins.len(), 3);
        assert_eq!(
            opened,
            vec![
                (t0 + 100, RequestId(1), user),
                (t0 + 430, RequestId(2), req_wins[0]),
                (t0 + 1100, RequestId(3), req_wins[2]),
            ]
        );
        let announced: Vec<(Millis, WinKey)> = ev
            .iter()
            .filter_map(|e| match e.event {
                Event::InitialLocationKnown { win, .. } => Some((e.at, win)),
                _ => None,
            })
            .collect();
        // The second Downloads window found no request left: unclaimed, still hidden.
        assert_eq!(announced, vec![(t0 + 200, other), (t0 + 765, req_wins[1])]);
        assert_eq!(s.window_record(req_wins[1]).unwrap().claimed_by, None);
        assert!(!s.is_visible(req_wins[1]));
        assert!(s.is_visible(user) && s.is_visible(other));
    }

    #[test]
    fn new_tab_appends_selected_and_navigation_keeps_its_position() {
        let (mut s, win) = with_trigger(SimConfig::default());
        let t0 = s.now();
        s.handle_command(&Command::CreateTab { op: OpId(1), win });
        assert!(evs(&s.tick(84)).is_empty());
        let ev = s.tick(1);
        let tab = s.window(win).unwrap().tabs[1].key;
        assert_eq!(
            evs(&ev),
            vec![&Event::TabCreated { win, tab }, &Event::NavigationCompleted { tab, location: home_location() }]
        );
        assert_eq!(ev[0].at, t0 + 85);
        assert_eq!(s.window(win).unwrap().active, 1);
        assert_eq!(names(&s, win), ["Documents", "Home"]);

        s.handle_command(&Command::Navigate { op: OpId(2), tab, location: loc(4) });
        assert!(evs(&s.tick(59)).is_empty());
        let ev = s.tick(1);
        assert_eq!(evs(&ev), vec![&Event::NavigationCompleted { tab, location: loc(4) }]);
        assert_eq!(s.window(win).unwrap().tabs[1].key, tab);
        assert_eq!(names(&s, win), ["Documents", "2024"]);
        assert!(!s.pipelining_violated());
        // A second, sequential creation is fine; tabs are built one at a time.
        s.handle_command(&Command::CreateTab { op: OpId(3), win });
        s.tick(100);
        assert!(!s.pipelining_violated());
        assert_eq!(s.window(win).unwrap().tabs.len(), 3);
    }

    #[test]
    fn pipelined_creation_is_flagged_and_sticks_the_header() {
        let (mut s, win) = with_trigger(SimConfig::default());
        let t0 = s.now();
        s.handle_command(&Command::CreateTab { op: OpId(1), win });
        s.handle_command(&Command::CreateTab { op: OpId(2), win });
        let ev = s.tick(300);
        let created: Vec<(Millis, TabKey)> = ev
            .iter()
            .filter_map(|e| match e.event {
                Event::TabCreated { tab, .. } => Some((e.at, tab)),
                _ => None,
            })
            .collect();
        assert_eq!(created.iter().map(|c| c.0).collect::<Vec<_>>(), [t0 + 85, t0 + 170], "serialized");
        assert_eq!(s.pipelining_violations(), &[PipeliningViolation { at: t0, win, op: OpId(2) }]);
        for (i, (_, tab)) in created.iter().enumerate() {
            s.handle_command(&Command::Navigate { op: OpId(10 + i as u64), tab: *tab, location: loc(3 + i) });
        }
        s.tick(100);
        // Contents are right, but the second new tab's header stays "Home".
        assert_eq!(names(&s, win), ["Documents", "Downloads", "Home"]);
        assert_eq!(s.window(win).unwrap().tabs[2].last_known, Some(loc(4)));
        assert!(s.window(win).unwrap().tabs[2].stuck_header);
    }

    #[test]
    fn lost_posts_create_nothing_and_a_repost_is_not_pipelining() {
        let mut config = SimConfig::default();
        config.restore.lost_tabs = TabLoss::EveryNth(2);
        // The engine re-posts itself here (no navigator retry).
        config.restore.navigator_repost = false;
        let (mut s, win) = with_trigger(config);
        s.handle_command(&Command::CreateTab { op: OpId(1), win });
        assert_eq!(s.tick(200).iter().filter(|e| matches!(e.event, Event::TabCreated { .. })).count(), 1);
        s.handle_command(&Command::CreateTab { op: OpId(2), win });
        let ev = s.tick(1500);
        assert!(ev.iter().all(|e| !matches!(e.event, Event::TabCreated { .. } | Event::OperationFailed { .. })));
        s.handle_command(&Command::CreateTab { op: OpId(3), win });
        assert_eq!(s.tick(200).iter().filter(|e| matches!(e.event, Event::TabCreated { .. })).count(), 1);
        assert_eq!(s.window(win).unwrap().tabs.len(), 3);
        assert!(!s.pipelining_violated());
        assert!(TabLoss::EveryNth(200).loses(199) && !TabLoss::EveryNth(200).loses(198));
    }

    #[test]
    fn the_navigator_reposts_a_lost_post_then_uses_the_add_button() {
        // Post 0 lost, its re-post (post 1) arrives 1.5 s later.
        let mut config = SimConfig::default();
        config.restore.lost_tabs = TabLoss::Posts([0].into());
        let (mut s, win) = with_trigger(config);
        s.handle_command(&Command::CreateTab { op: OpId(1), win });
        assert!(s.tick(1500 + 84).iter().all(|e| !matches!(e.event, Event::TabCreated { .. })));
        assert_eq!(s.tick(1).iter().filter(|e| matches!(e.event, Event::TabCreated { .. })).count(), 1);

        // Both posts lost: the UIA AddButton produces the tab 3 s + 430 ms later.
        let mut config = SimConfig::default();
        config.restore.lost_tabs = TabLoss::Posts([0, 1].into());
        let (mut s, win) = with_trigger(config);
        s.handle_command(&Command::CreateTab { op: OpId(1), win });
        assert!(s.tick(3000 + 429).iter().all(|e| !matches!(e.event, Event::TabCreated { .. })));
        assert_eq!(s.tick(1).iter().filter(|e| matches!(e.event, Event::TabCreated { .. })).count(), 1);
        assert_eq!(s.window(win).unwrap().tabs.len(), 2);
        assert!(!s.pipelining_violated());
    }

    /// A trigger with tabs [Documents, Downloads, 2024], the last one active.
    fn three_tabs(config: SimConfig) -> (SimExplorer, WinKey, Vec<TabKey>) {
        let (mut s, win) = with_trigger(config);
        for (i, l) in [3, 4].into_iter().enumerate() {
            s.handle_command(&Command::CreateTab { op: OpId(100 + i as u64), win });
            s.tick(100);
            let tab = s.window(win).unwrap().tabs.last().unwrap().key;
            s.handle_command(&Command::Navigate { op: OpId(200 + i as u64), tab, location: loc(l) });
            s.tick(100);
        }
        let tabs = s.window(win).unwrap().tabs.iter().map(|t| t.key).collect();
        (s, win, tabs)
    }

    #[test]
    fn close_tab_closes_exactly_that_tab() {
        let (mut s, win, tabs) = three_tabs(SimConfig::default());
        s.handle_command(&Command::CloseTab { op: OpId(1), tab: tabs[1] });
        assert!(evs(&s.tick(299)).is_empty());
        assert_eq!(evs(&s.tick(1)), vec![&Event::TabRevoked { win, tab: tabs[1] }]);
        assert_eq!(names(&s, win), ["Documents", "2024"]);
        assert_eq!(s.window(win).unwrap().active, 1, "the active tab stays active");

        // With Quit, a tab that is not active is left open.
        let mut config = SimConfig::default();
        config.restore.close_model = TabCloseModel::Quit;
        let (mut s, win, tabs) = three_tabs(config);
        s.handle_command(&Command::CloseTab { op: OpId(1), tab: tabs[0] });
        assert!(evs(&s.tick(400)).is_empty());
        assert_eq!(s.window(win).unwrap().tabs.len(), 3);
        s.handle_command(&Command::CloseTab { op: OpId(2), tab: tabs[2] });
        s.tick(400);
        assert_eq!(names(&s, win), ["Documents", "Downloads"]);
    }

    #[test]
    fn closing_the_only_tab_closes_the_window() {
        let (mut s, _) = with_trigger(SimConfig::default());
        s.handle_command(&open_cmd(1, 3, 100));
        s.tick(500);
        let win = s.windows()[1].key;
        let tab = s.windows()[1].tabs[0].key;
        s.handle_command(&Command::CloseTab { op: OpId(1), tab });
        let ev = s.tick(300);
        assert_eq!(evs(&ev), vec![&Event::TabRevoked { win, tab }, &Event::WindowDestroyed { win }]);
        assert!(s.window(win).is_none());
        assert_eq!(s.window_record(win).unwrap().closed_at, Some(s.now()));
    }

    #[test]
    fn activation_selection_reveal_z_order_and_focus() {
        let (mut s, trigger) = with_trigger(SimConfig::default());
        s.handle_command(&open_cmd(1, 3, 2000));
        s.tick(500);
        let win = s.windows()[1].key;
        s.handle_command(&Command::CreateTab { op: OpId(1), win });
        s.tick(100);
        assert_eq!(s.window(win).unwrap().active, 1);
        s.handle_command(&Command::ActivateTab { op: OpId(2), win, index: 0 });
        s.handle_command(&Command::ActivateTab { op: OpId(3), win, index: 5 });
        let ev = s.tick(20);
        let got = evs(&ev);
        assert_eq!(got.len(), 2, "{got:?}");
        assert!(got.iter().any(|e| matches!(e, Event::OperationFailed { op: OpId(3), .. })), "{got:?}");
        assert!(got.iter().any(|e| matches!(e, Event::ActivationConfirmed { op: OpId(2) })), "{got:?}");
        assert_eq!(s.window(win).unwrap().active, 0);

        let tab = s.window(win).unwrap().tabs[0].key;
        let items = vec![ItemId(vec![1, 2]), ItemId(vec![3])];
        s.handle_command(&Command::SelectItems { op: OpId(4), tab, items: items.clone() });
        s.tick(10);
        assert_eq!(s.selection(tab), &items[..]);

        // Placement changes while hidden are invisible; the first visible
        // placement is the one at Reveal.
        let moved = FinalPlacement { state: ShowState::Maximized, ..fp(2100) };
        s.handle_command(&Command::ApplyPlacement { win, placement: moved });
        s.tick(5);
        s.handle_command(&Command::Reveal { win });
        s.tick(5);
        let r = s.window_record(win).unwrap();
        let (at, shown) = r.first_visible.clone().unwrap();
        assert_eq!(at, Millis(s.now().0 - 4));
        assert_eq!((shown.state, shown.restored), (ShowState::Maximized, moved.restored));
        assert_eq!(shown.display.device, r"\\.\DISPLAY2");
        assert_eq!((r.placements.len(), r.visible_placement_changes), (2, 0));
        s.handle_command(&Command::ApplyPlacement { win, placement: fp(2000) });
        s.tick(5);
        assert_eq!(s.window_record(win).unwrap().visible_placement_changes, 1);

        assert_eq!(s.z_order(), vec![win, trigger], "a new window is created on top");
        s.handle_command(&Command::SetZOrder(vec![trigger, win]));
        s.tick(5);
        assert_eq!(s.z_order(), vec![trigger, win]);
        s.handle_command(&Command::Focus(win));
        let ev = s.tick(5);
        assert_eq!(evs(&ev), vec![&Event::WindowActivated { win }]);
        assert_eq!((s.foreground(), s.focused()), (Some(win), Some(win)));
        assert_eq!(s.z_order(), vec![win, trigger]);

        // A navigation resets the selection.
        s.handle_command(&Command::Navigate { op: OpId(5), tab, location: loc(4) });
        s.tick(60);
        assert!(s.selection(tab).is_empty());
    }

    #[test]
    fn probes_follow_the_programmed_outcomes_and_latencies() {
        let mut s = SimExplorer::new(SimConfig::default());
        s.set_probe(loc(7), ProbeOutcome::Timeout, 5000);
        s.handle_command(&Command::Probe { probe: ProbeId(1), location: loc(7) });
        s.handle_command(&Command::Probe { probe: ProbeId(2), location: loc(3) });
        let ev = s.tick(6000);
        let done: Vec<(Millis, ProbeId, ProbeOutcome)> = ev
            .iter()
            .filter_map(|e| match &e.event {
                Event::ProbeCompleted { probe, outcome } => Some((e.at, *probe, outcome.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            done,
            vec![(Millis(5), ProbeId(2), ProbeOutcome::Available), (Millis(5000), ProbeId(1), ProbeOutcome::Timeout)]
        );
    }

    #[test]
    fn injected_failures() {
        let (mut s, win) = with_trigger(SimConfig::default());
        let tab = s.window(win).unwrap().tabs[0].key;
        s.fail(FailureRule::times(FailWhen::Kind(CommandKind::Navigate), 1));
        s.fail(FailureRule::always(FailWhen::Op(OpId(9))));
        s.fail(FailureRule::always(FailWhen::Write(4)));
        s.fail(FailureRule::always(FailWhen::Request(RequestId(7))));
        s.fail(FailureRule::always(FailWhen::Kind(CommandKind::Probe)));

        s.handle_command(&Command::Navigate { op: OpId(1), tab, location: loc(3) });
        s.handle_command(&Command::CreateTab { op: OpId(9), win });
        s.handle_command(&open_cmd(7, 3, 100));
        s.handle_command(&Command::Probe { probe: ProbeId(1), location: loc(3) });
        let ev = s.step();
        let failed: Vec<OpId> = s
            .step()
            .into_iter()
            .chain(s.step())
            .chain(ev)
            .filter_map(|e| match e.event {
                Event::OperationFailed { op, error } => Some((op, error.code)),
                _ => None,
            })
            .inspect(|(_, code)| assert_eq!(*code, E_FAIL))
            .map(|(op, _)| op)
            .collect();
        assert_eq!(failed.len(), 3);
        assert!([OpId(1), OpId(9), OpId(7)].iter().all(|op| failed.contains(op)));
        assert!(s.outstanding_requests().is_empty());

        // The rule for Navigate is used up; a missing tab fails on its own.
        s.handle_command(&Command::Navigate { op: OpId(2), tab, location: loc(3) });
        s.handle_command(&Command::Navigate { op: OpId(3), tab: TabKey { hwnd: 1, seq: 999 }, location: loc(3) });
        s.handle_command(&Command::WriteSessionFile { seq: 4, text: "a".into(), urgent: true });
        s.handle_command(&Command::WriteSessionFile { seq: 5, text: "b".into(), urgent: false });
        let ev = s.tick(1000);
        let events = evs(&ev);
        assert!(events.contains(&&Event::NavigationCompleted { tab, location: loc(3) }));
        assert!(events.iter().any(|e| matches!(e, Event::OperationFailed { op: OpId(3), error } if error.code == E_INVALID_HANDLE)));
        assert!(events.contains(&&Event::WriteCompleted { seq: 4, result: Err(IoErrorKind::Other(31)) }));
        assert!(events.contains(&&Event::WriteCompleted { seq: 5, result: Ok(()) }));
        assert!(!events.iter().any(|e| matches!(e, Event::ProbeCompleted { .. } | Event::TabCreated { .. })));
        assert_eq!(s.session_file(), Some("b"));
        assert_eq!(s.windows().len(), 1);

        let failed: Vec<bool> = s.command_log().iter().map(|c| c.failed).collect();
        assert_eq!(failed, [true, true, true, true, false, false, true, false]);
    }

    #[test]
    fn every_command_is_recorded_with_its_time() {
        let (mut s, win) = with_trigger(SimConfig::default());
        let t0 = s.now();
        let first = vec![Command::RescanWindow(win), Command::Notify { omitted_tabs: 2 }];
        for c in &first {
            s.handle_command(c);
        }
        s.tick(50);
        let timer = Command::SetTimer { id: TimerId { owner: TimerOwner::Restore, seq: 1 }, at: t0 + 900 };
        s.handle_command(&timer);
        s.handle_command(&Command::Exit);
        let log: Vec<(Millis, &Command)> = s.command_log().iter().map(|c| (c.at, &c.command)).collect();
        assert_eq!(
            log,
            vec![(t0, &first[0]), (t0, &first[1]), (t0 + 50, &timer), (t0 + 50, &Command::Exit)]
        );
        assert_eq!(s.notifications(), &[(t0, 2)]);
        assert_eq!(s.commands_of(CommandKind::Exit).len(), 1);
    }

    /// A hand-written restoration: the trigger gets [Downloads, 2024] after
    /// its own tab and one window opens with [Backup, Shared, Reports].
    #[derive(Default)]
    struct Hand {
        pipelined: bool,
        trigger: Option<WinKey>,
        restored: Option<WinKey>,
        to_create: BTreeMap<WinKey, VecDeque<LocationId>>,
        creating: BTreeMap<WinKey, VecDeque<LocationId>>,
        navigating: BTreeMap<TabKey, LocationId>,
        next_op: u64,
        finished: bool,
    }

    impl Hand {
        fn op(&mut self) -> OpId {
            self.next_op += 1;
            OpId(self.next_op)
        }

        fn create(&mut self, win: WinKey, out: &mut Vec<Command>) {
            loop {
                let busy = self.creating.get(&win).is_some_and(|q| !q.is_empty());
                if busy && !self.pipelined {
                    return;
                }
                let Some(l) = self.to_create.get_mut(&win).and_then(VecDeque::pop_front) else { return };
                self.creating.entry(win).or_default().push_back(l);
                let op = self.op();
                out.push(Command::CreateTab { op, win });
            }
        }

        fn handle(&mut self, _now: Millis, event: Event) -> Vec<Command> {
            let mut out = Vec::new();
            match event {
                Event::InitialLocationKnown { win, .. } if self.trigger.is_none() => {
                    self.trigger = Some(win);
                    self.to_create.insert(win, [loc(3), loc(4)].into());
                    out.push(Command::ApplyPlacement { win, placement: fp(100) });
                    out.push(Command::OpenWindow { request: RequestId(1), location: loc(5), placement: fp(2000) });
                    self.create(win, &mut out);
                }
                Event::WindowOpened { request: RequestId(1), win, .. } => {
                    self.restored = Some(win);
                    self.to_create.insert(win, [loc(6), loc(7)].into());
                    self.create(win, &mut out);
                }
                Event::TabCreated { win, tab } => {
                    let l = self.creating.get_mut(&win).and_then(VecDeque::pop_front).expect("expected tab");
                    self.navigating.insert(tab, l.clone());
                    let op = self.op();
                    out.push(Command::Navigate { op, tab, location: l });
                    self.create(win, &mut out);
                }
                Event::NavigationCompleted { tab, location } if self.navigating.get(&tab) == Some(&location) => {
                    self.navigating.remove(&tab);
                }
                _ => {}
            }
            let idle = self.to_create.values().all(VecDeque::is_empty)
                && self.creating.values().all(VecDeque::is_empty)
                && self.navigating.is_empty();
            if let (Some(trigger), Some(restored), true, false) = (self.trigger, self.restored, idle, self.finished) {
                self.finished = true;
                out.push(Command::Reveal { win: restored });
                out.push(Command::SetZOrder(vec![trigger, restored]));
                out.push(Command::Focus(trigger));
                out.push(Command::WriteSessionFile { seq: 1, text: "consumed".into(), urgent: true });
            }
            out
        }
    }

    fn hand_run(pipelined: bool) -> Run {
        let mut s = SimExplorer::new(SimConfig::for_restoration());
        let trigger_events = s.apply(&open_op(2));
        let t0 = s.now();
        let mut driver = Driver::new(s);
        driver.deliver(trigger_events);
        // The user opens another window while restoration runs (Req 4.2).
        driver.schedule(t0 + 200, UserAction::Op(open_op(0)));
        let mut hand = Hand { pipelined, ..Hand::default() };
        let stop = driver.run(|now, ev| hand.handle(now, ev));
        assert!(hand.finished);
        driver.finish(stop)
    }

    #[test]
    fn driver_runs_a_sequential_restoration_to_quiescence() {
        let run = hand_run(false);
        let s = &run.sim;
        assert_eq!(run.stop, Stop::Quiescent);
        assert!(!s.pipelining_violated());
        let [trigger, user, restored] = [0, 1, 2].map(|i| s.windows()[i].key);
        assert_eq!(names(s, trigger), ["Documents", "Downloads", "2024"]);
        assert_eq!(names(s, restored), ["Backup", "Shared", "Reports"]);
        assert_eq!(names(s, user), ["Projects"]);

        // Property 19's ingredients: one OpenWindow, one ApplyPlacement, the
        // restored window first shown at its final placement, nothing closed.
        assert_eq!(s.commands_of(CommandKind::OpenWindow).len(), 1);
        assert_eq!(s.commands_of(CommandKind::ApplyPlacement).len(), 1);
        let r = s.window_record(restored).unwrap();
        let (_, shown) = r.first_visible.clone().unwrap();
        assert_eq!(shown.restored, fp(2000).restored);
        assert_eq!(r.visible_placement_changes, 0);
        assert!(s.window_records().iter().all(|r| r.closed_at.is_none()));
        assert_eq!(s.window(trigger).unwrap().placement.restored, fp(100).restored);
        // The user's window received no command.
        let names_user = |c: &Command| format!("{c:?}").contains(&format!("{user:?}"));
        assert!(!run.commands.iter().any(|c| names_user(&c.command)));

        assert_eq!(s.session_file(), Some("consumed"));
        assert_eq!(s.focused(), Some(trigger));
        assert_eq!(s.z_order(), vec![trigger, restored, user]);
        // Timeline: the window opens at ~430 ms; tabs register 85 ms apart.
        let last = run.commands.last().unwrap().at;
        assert!(last < Millis(1500), "finished at {last:?}");
        assert!(run.events.windows(2).all(|w| w[0].at <= w[1].at), "time never goes backwards");
    }

    #[test]
    fn driver_exposes_a_pipelining_engine_bug() {
        let run = hand_run(true);
        assert_eq!(run.stop, Stop::Quiescent);
        assert_eq!(run.sim.pipelining_violations().len(), 2);
        let restored = run.sim.windows()[2].key;
        assert_eq!(names(&run.sim, restored), ["Backup", "Shared", "Home"]);
    }

    #[test]
    fn driver_fires_timers_so_a_lost_post_can_be_reposted() {
        let mut config = SimConfig::default();
        config.restore.lost_tabs = TabLoss::Posts([0].into());
        config.restore.navigator_repost = false;
        let (s, win) = with_trigger(config);
        let t0 = s.now();
        let id = TimerId { owner: TimerOwner::Restore, seq: 1 };
        let mut driver = Driver::new(s);
        // Set, then replace, the re-post timer.
        driver.submit([
            Command::CreateTab { op: OpId(1), win },
            Command::SetTimer { id, at: t0 + 100 },
            Command::SetTimer { id, at: t0 + 1500 },
        ]);
        let mut created = None;
        let stop = driver.run(|now, ev| match ev {
            Event::Timer(t) if t == id => vec![Command::CreateTab { op: OpId(2), win }],
            Event::TabCreated { .. } => {
                created = Some(now);
                vec![]
            }
            _ => vec![],
        });
        assert_eq!(stop, Stop::Quiescent);
        assert_eq!(created, Some(t0 + 1585));
        let run = driver.finish(stop);
        assert_eq!(run.events.iter().filter(|e| e.event == Event::Timer(id)).count(), 1);
        assert_eq!(run.sim.commands_of(CommandKind::CreateTab).len(), 2);
    }

    #[test]
    fn engine_runs_against_the_simulator() {
        let mut s = SimExplorer::new(SimConfig::for_restoration());
        let events = s.apply(&open_op(2));
        let startup = StartupInput {
            session_file: SessionFileRead::Absent,
            environment: Environment { displays: crate::explorer::default_displays(), ..Environment::default() },
            background_logon: false,
            logon_at: None,
            shell_ready_at: Some(Millis(0)),
        };
        let (mut engine, commands) = Engine::new(Settings::default(), startup);
        let mut driver = Driver::new(s);
        for owner in [TimerOwner::Tracker, TimerOwner::Store, TimerOwner::Control] {
            driver.quiet_timers(owner);
        }
        driver.submit(commands);
        driver.deliver(events);
        let stop = driver.run(|now, ev| engine.handle(now, ev));
        assert_eq!(stop, Stop::Quiescent);
        let run = driver.finish(stop);
        assert!(!run.sim.pipelining_violated());
        assert_eq!(run.sim.windows().len(), 1);
    }
}
