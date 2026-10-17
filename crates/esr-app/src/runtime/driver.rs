//! The Windows-free half of the runtime: the Engine driver.
//!
//! [`RuntimeCore`] owns the pure [`Engine`] and the adapter ports. It takes
//! [`Inbound`] items (adapter output, collected by the thread-safe
//! [`Mailbox`]), turns them into Engine events, and carries out the
//! Engine's [`Command`]s on the [`Ports`]. Everything here runs on the
//! Engine thread and is driven with explicit [`Millis`] timestamps, so it is
//! unit-tested with fake ports (see the tests at the end).
//!
//! What it adds between the adapters and the Engine:
//!
//! * **Timers.** `Command::SetTimer { id, at }` goes into a [`TimerWheel`]
//!   (setting a pending id replaces it); [`RuntimeCore::tick`] delivers due
//!   timers as `Event::Timer(id)` in deadline order. The Windows side arms
//!   one Win32 timer for [`RuntimeCore::next_deadline`].
//! * **Op ids for navigator signals** ([`OpMap`]). The `Navigator` port has
//!   no op ids, so the core remembers the `OpId` of each outstanding
//!   `CreateTab` (per window) and `CloseTab` (per tab) and maps
//!   `TabCreateFailed` / `CloseFailed` back to `OperationFailed`.
//!   `OpenFailed` maps to `OpId(request.0)`: the restore session issues each
//!   `OpenWindow` with `RequestId(op.0)`. `TabCreateRetried` is progress,
//!   not a failure, and is dropped. A synchronous `Err` from any port call
//!   that carries an op becomes `OperationFailed` for that op; an `Err` from
//!   one without (placement, reveal, z-order, focus, toast, start-up
//!   registration, settings) is logged as an `ERROR` entry.
//! * **Destroy suppression** ([`DestroyFilter`], the adapter rule in
//!   `esr_core::tracker::session`): a `WindowDestroyed` without a recent
//!   ShellWindows revocation of one of the window's tabs is held for
//!   [`DESTROY_HOLD_MS`]. If the window's process signals meanwhile (its
//!   `ProcessExited`, sent 200 ms after the signal, arrives) the destroy is
//!   dropped, so a killed Explorer's windows are orphaned instead of closed
//!   one by one (Req 3.3, 3.4). A revocation releases a held destroy at once.
//! * **Process watches.** The first sighting of an Explorer PID (a scan or a
//!   window signal) installs its per-process hooks and exit wait (Req 2.5).
//! * **Environment.** Fresh readings (`EnvironmentChanged`) go to the Engine
//!   right before the first scan of a window, before every activation of a
//!   window not scanned yet, and before every `InitialLocationKnown`, i.e.
//!   before anything that may become a Trigger_Window or be checked for
//!   Explorer's logon restore (BuiltinRestore heuristic, saved-at clock,
//!   LaunchTo).
//! * **Tab folding** (Req 14). `HideWindow` goes to the [`Hider`] (a hide
//!   that cannot be queued becomes `HideFailed`), `Reveal` to the hider and
//!   the navigator (each reveals only the windows it hid), `ProbeFoldContext`
//!   to the [`FoldProbe`] (the answer comes back as `FoldContext` through the
//!   mailbox), and `CloseWindow` to the navigator. `TabStarted` navigator
//!   signals become `Event::TabStarted` for the outstanding `CreateTab`.
//! * **Failed restoration.** When the restore session enters
//!   `Phase::Failed`, `Navigator::reveal_all` reveals or releases every
//!   window the navigator still hides (Req 4.10).
//! * **Session end** ([`RuntimeCore::end_session`]): `EndSessionQuery`, then
//!   blocking on the urgent write for at most the given budget (1.5 s,
//!   Req 3.5, 3.6).

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

use esr_core::engine::{
    Command, EndKind, Engine, Environment, Event, Millis, OpId, ShellError, StartupInput,
    TabKey, TimerId, WinKey,
};
use esr_core::logfmt::entry::{LogEntry, format_entry};
use esr_core::model::Timestamp;
use esr_core::restore::Phase;
use esr_core::settings::Settings;
use esr_win::navigator::NavigatorSignal;
use esr_win::window_event_source::{PROCESS_EXIT_DELAY_MS, WindowSignal};

use crate::ports::{
    EventSink, FileStore, FoldProbe, Hider, LogWriter, Navigator, Notifier, Options, Prober,
    Startup, StartupMechanism, TabStrip,
};
use crate::ui::UiCommand;

/// How long a `WindowDestroyed` without a prior revocation is held while
/// waiting for its process's `ProcessExited`: the 200 ms exit delay plus a
/// margin for destroys reported slightly before the process handle signals.
pub const DESTROY_HOLD_MS: u64 = PROCESS_EXIT_DELAY_MS as u64 + 150;

/// A revocation counts as part of a window close when it came at most this
/// long before the window's destroy (a tab closed long ago does not make a
/// later kill look orderly).
pub const REVOKE_RECENT_MS: u64 = 1_000;

/// Retry delay for a `RescanAll` that found the shell window source busy.
pub const RESCAN_ALL_RETRY_MS: u64 = 50;

/// How long `WM_QUERYENDSESSION` may block for the urgent write (design.md
/// "Sign-out, shutdown, restart").
pub const END_SESSION_BLOCK_MS: u64 = 1_500;

/// `ERROR` entry operation names of the runtime.
pub const OP_APPLY_PLACEMENT: &str = "apply-placement";
pub const OP_REVEAL: &str = "reveal-window";
/// `CloseWindow` could not be posted (the window is gone or not an Explorer
/// window). Distinct from the Engine's own `fold-close`, which ends an
/// attempt after the retry timers (Req 14.1).
pub const OP_CLOSE_WINDOW: &str = "close-window";
pub const OP_Z_ORDER: &str = "set-z-order";
pub const OP_FOCUS: &str = "focus-window";
pub const OP_NOTIFY: &str = "notify";
pub const OP_START_AT_SIGN_IN: &str = "start-at-sign-in";
pub const OP_PERSIST_SETTINGS: &str = "persist-settings";
pub const OP_WATCH_PROCESS: &str = "watch-process";
pub const OP_PANIC: &str = "panic";

// ---------------------------------------------------------------------------
// Inbound items and the mailbox
// ---------------------------------------------------------------------------

/// Everything that reaches the Engine thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Inbound {
    /// An Engine event from an adapter (before runtime filtering).
    Event(Event),
    /// `EventSink::window_changed`: schedule a debounced rescan.
    Changed(WinKey),
    /// A `ShellNavigator` signal (mapped to `OperationFailed` by op id).
    Navigator(NavigatorSignal),
    /// A `WindowEventSource` signal.
    Window(WindowSignal),
    /// `ESR_SHOW_SETTINGS` from a second instance (Req 12.7).
    ShowSettings,
    /// Displays or Explorer options may have changed (`WM_DISPLAYCHANGE`,
    /// `WM_SETTINGCHANGE`): send fresh readings.
    EnvironmentStale,
}

/// Thread-safe queue of [`Inbound`] items for the Engine thread. Adapters on
/// any thread post into it (it is the [`EventSink`] of every port); the
/// waker (set once the Engine window exists) wakes the Engine thread at most
/// once per batch.
#[derive(Default)]
pub struct Mailbox {
    queue: Mutex<VecDeque<Inbound>>,
    wake_posted: AtomicBool,
    waker: OnceLock<Box<dyn Fn() + Send + Sync>>,
}

impl std::fmt::Debug for Mailbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mailbox").field("len", &self.len()).finish()
    }
}

impl Mailbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the waker (once) and wakes for items already queued.
    pub fn set_waker(&self, waker: Box<dyn Fn() + Send + Sync>) {
        let _ = self.waker.set(waker);
        if !self.is_empty() {
            self.wake();
        }
    }

    pub fn push(&self, item: Inbound) {
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(item);
        self.wake();
    }

    /// The next item. Call [`begin_drain`](Self::begin_drain) first.
    pub fn pop(&self) -> Option<Inbound> {
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
    }

    /// Items posted from now on wake the Engine thread again.
    pub fn begin_drain(&self) {
        self.wake_posted.store(false, Ordering::SeqCst);
    }

    pub fn len(&self) -> usize {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn wake(&self) {
        if !self.wake_posted.swap(true, Ordering::SeqCst) {
            match self.waker.get() {
                Some(w) => w(),
                None => self.wake_posted.store(false, Ordering::SeqCst),
            }
        }
    }
}

impl EventSink for Mailbox {
    fn post(&self, event: Event) {
        self.push(Inbound::Event(event));
    }

    fn window_changed(&self, win: WinKey) {
        self.push(Inbound::Changed(win));
    }
}

// ---------------------------------------------------------------------------
// Timers
// ---------------------------------------------------------------------------

/// Pending Engine timers (`Command::SetTimer`). Setting a pending id replaces it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TimerWheel {
    at: BTreeMap<TimerId, Millis>,
}

impl TimerWheel {
    pub fn set(&mut self, id: TimerId, at: Millis) {
        self.at.insert(id, at);
    }

    /// The earliest deadline.
    pub fn next(&self) -> Option<Millis> {
        self.at.values().min().copied()
    }

    /// Removes and returns every timer due at `now`, earliest first (ties
    /// by id).
    pub fn take_due(&mut self, now: Millis) -> Vec<TimerId> {
        let mut due: Vec<(Millis, TimerId)> = self
            .at
            .iter()
            .filter(|(_, at)| **at <= now)
            .map(|(id, at)| (*at, *id))
            .collect();
        due.sort_unstable();
        for (_, id) in &due {
            self.at.remove(id);
        }
        due.into_iter().map(|(_, id)| id).collect()
    }

    pub fn len(&self) -> usize {
        self.at.len()
    }

    pub fn is_empty(&self) -> bool {
        self.at.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Op ids for navigator signals
// ---------------------------------------------------------------------------

/// Outstanding navigator operations by the key their signals carry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpMap {
    creates: HashMap<WinKey, OpId>,
    closes: HashMap<TabKey, OpId>,
    /// `Navigate` ops by tab, until the tab's navigation completes (a
    /// browser worker reports a failed `Navigate2` asynchronously).
    navigations: HashMap<TabKey, OpId>,
}

impl OpMap {
    /// Records the op of a `CreateTab`, `CloseTab` or `Navigate` before it
    /// is carried out.
    pub fn issued(&mut self, command: &Command) {
        match command {
            Command::CreateTab { op, win } => {
                self.creates.insert(*win, *op);
            }
            Command::CloseTab { op, tab } => {
                self.closes.insert(*tab, *op);
            }
            Command::Navigate { op, tab, .. } => {
                self.navigations.insert(*tab, *op);
            }
            _ => {}
        }
    }

    /// A tab registered in `win`: its new-tab command is settled.
    pub fn tab_created(&mut self, win: WinKey) {
        self.creates.remove(&win);
    }

    /// A navigation of `tab` completed: its `Navigate` is settled.
    pub fn navigated(&mut self, tab: TabKey) {
        self.navigations.remove(&tab);
    }

    /// `tab` is gone: its close (and navigation) is settled.
    pub fn tab_revoked(&mut self, tab: TabKey) {
        self.closes.remove(&tab);
        self.navigations.remove(&tab);
    }

    pub fn window_destroyed(&mut self, win: WinKey) {
        self.creates.remove(&win);
    }

    /// Forgets the op of a command whose port call failed synchronously.
    pub fn failed(&mut self, command: &Command) {
        match command {
            Command::CreateTab { win, .. } => {
                self.creates.remove(win);
            }
            Command::CloseTab { tab, .. } => {
                self.closes.remove(tab);
            }
            Command::Navigate { tab, .. } => {
                self.navigations.remove(tab);
            }
            _ => {}
        }
    }

    /// The Engine event for a navigator signal: the failure of an operation
    /// the Engine still waits for, or the start of a `CreateTab`'s tab.
    pub fn signal(&mut self, signal: NavigatorSignal) -> Option<Event> {
        match signal {
            NavigatorSignal::OpenFailed { request, error } => {
                Some(Event::OperationFailed { op: OpId(request.0), error })
            }
            NavigatorSignal::TabCreateFailed { win, error } => {
                let op = self.creates.remove(&win)?;
                Some(Event::OperationFailed { op, error })
            }
            NavigatorSignal::CloseFailed { tab, error } => {
                let op = self.closes.remove(&tab)?;
                Some(Event::OperationFailed { op, error })
            }
            NavigatorSignal::NavigateFailed { tab, error } => {
                let op = self.navigations.remove(&tab)?;
                Some(Event::OperationFailed { op, error })
            }
            // The new tab's window appeared (Req 14.12, 14.13). The command
            // stays outstanding (`get`, not `remove`): its `TabCreated`
            // follows and settles it, and a failure may still be reported.
            // A tab nobody asked for through `CreateTab` reports nothing.
            NavigatorSignal::TabStarted { win, tab } => {
                let op = *self.creates.get(&win)?;
                Some(Event::TabStarted { op, tab })
            }
            NavigatorSignal::TabCreateRetried { .. } | NavigatorSignal::ZOrderFailed { .. } => None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.creates.is_empty() && self.closes.is_empty() && self.navigations.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Destroy suppression
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HeldDestroy {
    win: WinKey,
    pid: Option<u32>,
    until: Millis,
}

/// Implements the adapter rule of `esr_core::tracker::session`: no
/// `WindowDestroyed` for a window whose process has signalled (or signals
/// within its `ProcessExited` delay) unless ShellWindows revoked one of the
/// window's tabs first. See the module docs.
#[derive(Clone, Debug, Default)]
pub struct DestroyFilter {
    pids: HashMap<WinKey, u32>,
    /// Last revocation of a tab of the window.
    revoked: HashMap<WinKey, Millis>,
    /// PIDs whose `ProcessExited` arrived.
    exited: HashSet<u32>,
    held: Vec<HeldDestroy>,
}

impl DestroyFilter {
    /// `win` belongs to `pid` (from a scan or a window signal).
    pub fn note_pid(&mut self, win: WinKey, pid: u32) {
        if pid == 0 {
            return;
        }
        self.pids.insert(win, pid);
        // A live window under this PID: a new process reuses the number.
        self.exited.remove(&pid);
    }

    /// A tab of `win` was revoked at `now`. Returns a held destroy of `win`,
    /// which is released (to be delivered after the revocation).
    pub fn tab_revoked(&mut self, win: WinKey, now: Millis) -> Option<Event> {
        if let Some(i) = self.held.iter().position(|h| h.win == win) {
            self.held.remove(i);
            self.pids.remove(&win);
            self.revoked.remove(&win);
            return Some(Event::WindowDestroyed { win });
        }
        self.revoked.insert(win, now);
        None
    }

    /// `win` was destroyed at `now`: the event to deliver now, or `None` when
    /// it is held or suppressed.
    pub fn destroyed(&mut self, win: WinKey, now: Millis) -> Option<Event> {
        let pid = self.pids.get(&win).copied();
        let recent = self
            .revoked
            .remove(&win)
            .is_some_and(|at| now.since(at) <= REVOKE_RECENT_MS);
        if recent {
            self.pids.remove(&win);
            return Some(Event::WindowDestroyed { win });
        }
        if pid.is_some_and(|p| self.exited.contains(&p)) {
            self.pids.remove(&win);
            return None;
        }
        if !self.held.iter().any(|h| h.win == win) {
            self.held.push(HeldDestroy { win, pid, until: now + DESTROY_HOLD_MS });
        }
        None
    }

    /// `ProcessExited(pid)` arrived: held destroys of its windows are dropped,
    /// and later destroys of its windows are suppressed.
    pub fn process_exited(&mut self, pid: u32) {
        let dropped: Vec<WinKey> =
            self.held.iter().filter(|h| h.pid == Some(pid)).map(|h| h.win).collect();
        self.held.retain(|h| h.pid != Some(pid));
        for win in dropped {
            self.pids.remove(&win);
        }
        self.exited.insert(pid);
    }

    /// Held destroys whose hold expired at `now`, in hold order.
    pub fn poll(&mut self, now: Millis) -> Vec<Event> {
        let mut out = Vec::new();
        let pids = &mut self.pids;
        self.held.retain(|h| {
            if h.until <= now {
                pids.remove(&h.win);
                out.push(Event::WindowDestroyed { win: h.win });
                false
            } else {
                true
            }
        });
        // Old revocations no longer count; forget them.
        self.revoked.retain(|_, at| now.since(*at) <= REVOKE_RECENT_MS);
        out
    }

    pub fn next_deadline(&self) -> Option<Millis> {
        self.held.iter().map(|h| h.until).min()
    }

    pub fn is_holding(&self, win: WinKey) -> bool {
        self.held.iter().any(|h| h.win == win)
    }
}

// ---------------------------------------------------------------------------
// Ports
// ---------------------------------------------------------------------------

/// Engine-thread services that are not one of the adapter ports.
pub trait Host {
    /// Debounced rescan of `win` (the window scanner).
    fn rescan_window(&mut self, win: WinKey);
    /// Rescan of `win` without the debounce quiet time (the Engine's
    /// `RescanWindow`, a barrier it waits for).
    fn scan_now(&mut self, win: WinKey) {
        self.rescan_window(win);
    }
    /// Signals a rescan of every open Explorer window. `false` when the
    /// shell window source is busy (re-entrant call); retried shortly.
    fn rescan_all(&mut self) -> bool;
    /// Per-process hooks and exit wait for an Explorer PID.
    fn watch_process(&mut self, pid: u32) -> Result<(), ShellError>;
    /// `win` was destroyed: drop scanner state for it.
    fn forget_window(&mut self, win: WinKey);
    /// Drop a pending or running scan of `win` (its result is ignored); the
    /// caller rescans it later.
    fn cancel_scan(&mut self, _win: WinKey) {}
    /// Turn the watch of `win`'s tab strip (UIA `StructureChanged`) off or
    /// back on; a restoration turns it off for the windows it builds.
    fn watch_strip(&mut self, _win: WinKey, _on: bool) {}
    /// A running restoration builds `win` (`on`), or no longer does: the
    /// Locations its navigations reported are then current, so scans need
    /// not re-read each tab's folder from Explorer (task 16.3).
    fn restoration_builds(&mut self, _win: WinKey, _on: bool) {}
    /// Pause (or resume) the per-PID location/name WinEvent hooks while a
    /// restoration runs. `false` when the hooks are busy (retried at the
    /// next pass).
    fn pause_location_hooks(&mut self, _paused: bool) -> bool {
        true
    }
    /// Fresh environment readings at `now`.
    fn environment(&mut self, now: Millis) -> Environment;
    /// The local wall-clock time now (Activity_Log timestamps).
    fn local_now(&mut self) -> Timestamp;
    /// A request for the UI thread.
    fn ui(&mut self, command: UiCommand);
}

/// Everything the Engine's commands are carried out on.
pub struct Ports {
    pub navigator: Box<dyn Navigator>,
    pub tab_strip: Box<dyn TabStrip>,
    pub prober: Box<dyn Prober>,
    /// Folding_Candidate visibility (`HideWindow`, the fold's `Reveal`).
    pub hider: Box<dyn Hider>,
    /// Tab_Folding context probes (`ProbeFoldContext`).
    pub fold_probe: Box<dyn FoldProbe>,
    pub files: Box<dyn FileStore>,
    pub log: Box<dyn LogWriter>,
    pub options: Box<dyn Options>,
    pub notifier: Box<dyn Notifier>,
    pub startup: Box<dyn Startup>,
    pub host: Box<dyn Host>,
}

// ---------------------------------------------------------------------------
// The core
// ---------------------------------------------------------------------------

/// The Engine with its ports; see the module docs.
pub struct RuntimeCore {
    engine: Engine,
    ports: Ports,
    timers: TimerWheel,
    ops: OpMap,
    destroys: DestroyFilter,
    /// Events produced while carrying out commands (synchronous failures,
    /// `BuiltinRestoreSet`), delivered before the next inbound item.
    immediate: VecDeque<Event>,
    watched: HashSet<u32>,
    watch_logged: HashSet<u32>,
    /// Windows already seen in a scan (fresh environment before the first).
    seen: HashSet<WinKey>,
    rescan_all_retry: Option<Millis>,
    /// `timer_id` of the failed restore session already handled.
    failure_handled: Option<TimerId>,
    /// `seq` of urgent Session_File writes not completed yet.
    urgent_writes: HashSet<u64>,
    /// Change-driven rescans deferred while a restoration builds the window
    /// (see `window_changed`).
    deferred_scans: HashSet<WinKey>,
    /// Windows of a running restoration whose tab strip is not watched
    /// (see `after_engine`).
    unwatched: HashSet<WinKey>,
    /// The per-PID location hooks are paused (see `after_engine`).
    location_paused: bool,
    exit: bool,
}

impl RuntimeCore {
    /// Creates the Engine and carries out its startup commands.
    pub fn new(settings: Settings, startup: StartupInput, ports: Ports, now: Millis) -> Self {
        let (engine, commands) = Engine::new(settings, startup);
        let mut core = Self {
            engine,
            ports,
            timers: TimerWheel::default(),
            ops: OpMap::default(),
            destroys: DestroyFilter::default(),
            immediate: VecDeque::new(),
            watched: HashSet::new(),
            watch_logged: HashSet::new(),
            seen: HashSet::new(),
            rescan_all_retry: None,
            failure_handled: None,
            urgent_writes: HashSet::new(),
            deferred_scans: HashSet::new(),
            unwatched: HashSet::new(),
            location_paused: false,
            exit: false,
        };
        for command in commands {
            core.dispatch(now, command);
        }
        core.run_immediate(now);
        core
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    pub fn ports_mut(&mut self) -> &mut Ports {
        &mut self.ports
    }

    /// `Command::Exit` was carried out.
    pub fn exit_requested(&self) -> bool {
        self.exit
    }

    /// The earliest time [`tick`](Self::tick) has work.
    pub fn next_deadline(&self) -> Option<Millis> {
        [self.timers.next(), self.destroys.next_deadline(), self.rescan_all_retry]
            .into_iter()
            .flatten()
            .min()
    }

    /// Whether an urgent Session_File write is still outstanding.
    pub fn urgent_write_pending(&self) -> bool {
        !self.urgent_writes.is_empty()
    }

    /// Delivers due timers, expired destroy holds, and retries.
    pub fn tick(&mut self, now: Millis) {
        for id in self.timers.take_due(now) {
            self.feed(now, Event::Timer(id));
        }
        for event in self.destroys.poll(now) {
            self.feed(now, event);
        }
        if self.rescan_all_retry.is_some_and(|at| at <= now) {
            self.rescan_all_retry = None;
            self.rescan_all(now);
        }
        self.run_immediate(now);
    }

    /// Handles everything queued in `inbox`, stamping each item with `clock()`.
    /// Loops until the queue is empty after re-arming its wake-up, so an
    /// item whose wake-up message was consumed by a nested (ignored) pump
    /// is never stranded.
    pub fn drain(&mut self, inbox: &Mailbox, clock: &mut dyn FnMut() -> Millis) {
        loop {
            inbox.begin_drain();
            while let Some(item) = inbox.pop() {
                self.handle(clock(), item);
            }
            if inbox.is_empty() {
                break;
            }
        }
    }

    /// `WM_QUERYENDSESSION` (Req 3.5, 3.6): delivers what is queued, then
    /// `EndSessionQuery(kind)`, and blocks on the urgent write for at most
    /// `budget_ms` from the call. Returns whether no urgent write is still
    /// outstanding.
    pub fn end_session(
        &mut self,
        kind: EndKind,
        inbox: &Mailbox,
        clock: &mut dyn FnMut() -> Millis,
        budget_ms: u64,
    ) -> bool {
        let start = clock();
        self.drain(inbox, clock);
        self.handle(clock(), Inbound::Event(Event::EndSessionQuery(kind)));
        self.drain(inbox, clock);
        self.flush_urgent(inbox, clock, start, budget_ms)
    }

    /// Waits (ÃƒÆ’Ã‚Â¢ÃƒÂ¢Ã¢â€šÂ¬Ã‚Â°Ãƒâ€šÃ‚Â¤ `budget_ms` after `start`) for outstanding urgent writes and
    /// delivers their completions.
    pub fn flush_urgent(
        &mut self,
        inbox: &Mailbox,
        clock: &mut dyn FnMut() -> Millis,
        start: Millis,
        budget_ms: u64,
    ) -> bool {
        if self.urgent_write_pending() {
            let left = budget_ms.saturating_sub(clock().since(start));
            let _ = self.ports.files.flush(left.min(u64::from(u32::MAX)) as u32);
            self.drain(inbox, clock);
        }
        !self.urgent_write_pending()
    }

    /// Handles one inbound item at `now`.
    pub fn handle(&mut self, now: Millis, item: Inbound) {
        if esr_win::diag::tracing() {
            let mut name = format!("{item:?}");
            name.truncate(110);
            let start = std::time::Instant::now();
            self.handle_inner(now, item);
            let took = start.elapsed();
            esr_win::diag::note(|| format!("in  {:.1} ms: {name}", took.as_secs_f64() * 1000.0));
        } else {
            self.handle_inner(now, item);
        }
    }

    fn handle_inner(&mut self, now: Millis, item: Inbound) {
        match item {
            Inbound::Event(event) => self.event(now, event),
            Inbound::Changed(win) => self.window_changed(win),
            Inbound::Navigator(NavigatorSignal::ZOrderFailed { error }) => {
                self.log_error(now, OP_Z_ORDER, &error.message);
            }
            Inbound::Navigator(signal) => {
                if let Some(event) = self.ops.signal(signal) {
                    self.feed(now, event);
                }
            }
            Inbound::Window(signal) => self.window_signal(now, signal),
            Inbound::ShowSettings => {
                let settings = self.engine.settings().clone();
                self.ports.host.ui(UiCommand::ShowSettings(settings));
            }
            Inbound::EnvironmentStale => self.refresh_environment(now),
        }
        self.run_immediate(now);
    }

    fn window_signal(&mut self, now: Millis, signal: WindowSignal) {
        match signal {
            WindowSignal::Changed { win, pid, .. } => {
                self.note_window(now, win, pid);
                self.window_changed(win);
            }
            WindowSignal::Activated { win, pid } => {
                self.note_window(now, win, pid);
                if !self.seen.contains(&win) {
                    // Not scanned yet: the Engine may evaluate it for
                    // Explorer's logon restore now (Req 10.4).
                    self.refresh_environment(now);
                }
                self.feed(now, Event::WindowActivated { win });
            }
            WindowSignal::Destroyed { win } => {
                self.ports.host.forget_window(win);
                self.ports.navigator.window_destroyed(win);
                self.ops.window_destroyed(win);
                self.seen.remove(&win);
                if let Some(event) = self.destroys.destroyed(win, now) {
                    self.feed(now, event);
                }
            }
            WindowSignal::ProcessExited { pid } => {
                self.destroys.process_exited(pid);
                // The source removed the PID's hooks and wait.
                self.watched.remove(&pid);
                self.feed(now, Event::ProcessExited { pid });
            }
        }
    }

    fn event(&mut self, now: Millis, event: Event) {
        match &event {
            Event::WindowScanned(obs) => {
                self.note_window(now, obs.win, obs.pid);
                if self.seen.insert(obs.win) {
                    self.refresh_environment(now);
                }
            }
            Event::InitialLocationKnown { .. } => self.refresh_environment(now),
            Event::TabRevoked { win, tab } => {
                let (win, tab) = (*win, *tab);
                self.ops.tab_revoked(tab);
                let released = self.destroys.tab_revoked(win, now);
                self.feed(now, event);
                if let Some(destroy) = released {
                    self.feed(now, destroy);
                }
                return;
            }
            Event::TabCreated { win, .. } => self.ops.tab_created(*win),
            Event::NavigationCompleted { tab, .. } => self.ops.navigated(*tab),
            Event::WriteCompleted { seq, .. } => {
                self.urgent_writes.remove(seq);
            }
            Event::WindowOpened { win, .. } => {
                let win = *win;
                self.feed(now, event);
                // A scan signalled before the claim (the window was not the
                // session's yet) would compete with Explorer building the
                // window's tabs; it is dropped and done at the end instead.
                if let Some(session) = self.engine.restore_session()
                    && !session.is_finished()
                    && session.owns_window(win)
                    && !session.needs_scan(win)
                {
                    self.ports.host.cancel_scan(win);
                    self.deferred_scans.insert(win);
                }
                if self.engine.restore_session().is_some_and(|s| !s.is_finished() && s.owns_window(win))
                    && self.unwatched.insert(win)
                {
                    self.ports.host.watch_strip(win, false);
                    self.ports.host.restoration_builds(win, true);
                }
                return;
            }
            _ => {}
        }
        self.feed(now, event);
    }

    fn note_window(&mut self, now: Millis, win: WinKey, pid: u32) {
        if pid == 0 {
            return;
        }
        self.destroys.note_pid(win, pid);
        if self.watched.contains(&pid) {
            return;
        }
        match self.ports.host.watch_process(pid) {
            Ok(()) => {
                self.watched.insert(pid);
            }
            // Retried at the next sighting; logged once per PID.
            Err(e) => {
                if self.watch_logged.insert(pid) {
                    self.log_error(now, OP_WATCH_PROCESS, &format!("PID {pid}: {}", e.message));
                }
            }
        }
    }

    fn refresh_environment(&mut self, now: Millis) {
        let environment = self.ports.host.environment(now);
        self.feed(now, Event::EnvironmentChanged(environment));
    }

    /// Delivers `event` to the Engine and carries out its commands.
    fn feed(&mut self, now: Millis, event: Event) {
        let commands = self.engine.handle(now, event);
        for command in commands {
            self.dispatch(now, command);
        }
        self.after_engine();
    }

    fn run_immediate(&mut self, now: Millis) {
        while let Some(event) = self.immediate.pop_front() {
            self.event(now, event);
        }
    }

    /// A change signal for `win`: a debounced rescan, unless the window is
    /// being built by a running restoration that does not need the scan
    /// ([`RestoreSession::needs_scan`]); the scan is then deferred until the
    /// session asks for one or finishes.
    ///
    /// [`RestoreSession::needs_scan`]: esr_core::restore::RestoreSession::needs_scan
    fn window_changed(&mut self, win: WinKey) {
        if let Some(session) = self.engine.restore_session()
            && !session.is_finished()
            && session.owns_window(win)
            && !session.needs_scan(win)
        {
            self.deferred_scans.insert(win);
            return;
        }
        esr_win::diag::note(|| format!("core: rescan {:#x} (not deferred)", win.hwnd));
        self.deferred_scans.remove(&win);
        self.ports.host.rescan_window(win);
    }

    fn after_engine(&mut self) {
        let finished = self.engine.restore_session().is_none_or(|s| s.is_finished());
        // The tab strips of the windows a restoration builds are not watched
        // meanwhile: Explorer raises UIA StructureChanged events for every
        // tab added, which slowed the restoration down by ~2 s for 3 x 10
        // tabs (task 16.3). The rescans at completion watch them again.
        if finished {
            for win in std::mem::take(&mut self.unwatched) {
                self.ports.host.watch_strip(win, true);
                self.ports.host.restoration_builds(win, false);
            }
        } else if let Some(session) = self.engine.restore_session() {
            let trigger = session.trigger();
            let needs_scan = session.needs_scan(trigger);
            if self.unwatched.insert(trigger) {
                self.ports.host.watch_strip(trigger, false);
                self.ports.host.restoration_builds(trigger, true);
                // A tracking scan still scheduled from before the restoration
                // would read the trigger's tab strip while Explorer builds
                // its tabs: its first strip search then took up to 10 s and
                // held up the window's UIA worker, and with it the
                // activation (task 16.3). It is dropped and done at the end.
                if !needs_scan {
                    self.ports.host.cancel_scan(trigger);
                    self.deferred_scans.insert(trigger);
                }
            }
        }
        // Explorer's location and name changes are not hooked meanwhile
        // either (~9,500 events for 3 x 10 tabs, task 16.3); every window is
        // rescanned once the restoration has finished.
        if finished == self.location_paused && self.ports.host.pause_location_hooks(!finished) {
            self.location_paused = !finished;
            if finished {
                self.ports.host.rescan_all();
            }
        }
        if finished && !self.deferred_scans.is_empty() {
            for win in std::mem::take(&mut self.deferred_scans) {
                self.ports.host.rescan_window(win);
            }
        }
        let Some(session) = self.engine.restore_session() else { return };
        if matches!(session.phase(), Phase::Failed(_)) {
            let id = session.timer_id();
            if self.failure_handled != Some(id) {
                self.failure_handled = Some(id);
                self.ports.navigator.reveal_all();
            }
        }
    }

    fn rescan_all(&mut self, now: Millis) {
        if !self.ports.host.rescan_all() {
            self.rescan_all_retry = Some(now + RESCAN_ALL_RETRY_MS);
        }
    }

    fn log_error(&mut self, _now: Millis, operation: &str, description: &str) {
        let entry = LogEntry::Error {
            operation: operation.to_owned(),
            description: description.to_owned(),
        };
        let ts = self.ports.host.local_now();
        self.ports.log.append(&format_entry(&ts, &entry));
    }

    fn op_failed(&mut self, op: OpId, error: ShellError) {
        self.immediate.push_back(Event::OperationFailed { op, error });
    }

    /// Carries out one Engine command.
    pub fn dispatch(&mut self, now: Millis, command: Command) {
        if !esr_win::diag::tracing() {
            return self.dispatch_inner(now, command);
        }
        let mut name = format!("{command:?}");
        name.truncate(110);
        let start = std::time::Instant::now();
        self.dispatch_inner(now, command);
        let took = start.elapsed();
        esr_win::diag::note(|| format!("cmd {:.1} ms: {name}", took.as_secs_f64() * 1000.0));
    }

    fn dispatch_inner(&mut self, now: Millis, command: Command) {
        self.ops.issued(&command);
        let p = &mut self.ports;
        match &command {
            Command::RescanWindow(win) => {
                self.deferred_scans.remove(win);
                p.host.scan_now(*win);
            }
            Command::RescanAll => self.rescan_all(now),
            Command::WriteSessionFile { seq, text, urgent } => {
                if *urgent {
                    self.urgent_writes.insert(*seq);
                }
                p.files.write_session(*seq, text, *urgent);
            }
            Command::PreserveBackup { seq, content } => p.files.preserve_backup(*seq, content),
            Command::AppendLog(entry) => {
                let ts = p.host.local_now();
                p.log.append(&format_entry(&ts, entry));
            }
            Command::OpenWindow { request, location, placement } => {
                if let Err(error) = p.navigator.open_window(*request, location, placement) {
                    self.op_failed(OpId(request.0), error);
                }
            }
            Command::ApplyPlacement { win, placement } => {
                if let Err(e) = p.navigator.apply_placement(*win, placement) {
                    self.log_error(now, OP_APPLY_PLACEMENT, &e.message);
                }
            }
            Command::CreateTab { op, win } => {
                if let Err(error) = p.navigator.create_tab(*win) {
                    self.ops.failed(&command);
                    self.op_failed(*op, error);
                }
            }
            Command::Navigate { op, tab, location } => {
                if let Err(error) = p.navigator.navigate(*tab, location) {
                    self.ops.failed(&command);
                    self.op_failed(*op, error);
                }
            }
            Command::CloseTab { op, tab } => {
                if let Err(error) = p.navigator.close_tab(*tab) {
                    self.ops.failed(&command);
                    self.op_failed(*op, error);
                }
            }
            Command::ActivateTab { op, win, index } => {
                // Asynchronous: a later failure arrives as `OperationFailed`.
                if let Err(error) = p.tab_strip.activate(*op, *win, *index) {
                    self.op_failed(*op, error);
                }
            }
            Command::SelectItems { op, tab, items } => {
                if let Err(error) = p.navigator.select_items(*tab, items) {
                    self.op_failed(*op, error);
                }
            }
            // Both ports: the hider reveals a Folding_Candidate it hid and
            // leaves any other window alone; the navigator reveals a window
            // the Restore_Engine opened hidden and leaves a visible one alone
            // (Req 14.14).
            Command::Reveal { win } => {
                let hider = p.hider.reveal(*win);
                let navigator = p.navigator.reveal(*win);
                for e in [hider, navigator].into_iter().filter_map(Result::err) {
                    self.log_error(now, OP_REVEAL, &e.message);
                }
            }
            Command::SetZOrder(order) => {
                if let Err(e) = p.navigator.set_z_order(order) {
                    self.log_error(now, OP_Z_ORDER, &e.message);
                }
            }
            Command::Focus(win) => {
                if let Err(e) = p.navigator.focus(*win) {
                    self.log_error(now, OP_FOCUS, &e.message);
                }
            }
            Command::Probe { probe, location } => p.prober.probe(*probe, location),
            Command::Notify { omitted_tabs } => {
                if let Err(e) = p.notifier.notify_omitted(*omitted_tabs) {
                    self.log_error(now, OP_NOTIFY, &e.message);
                }
            }
            Command::ShowPrompt => p.host.ui(UiCommand::ShowPrompt),
            Command::ShowMessage(kind) => p.host.ui(UiCommand::ShowMessage(*kind)),
            Command::ShowSettings => {
                let settings = self.engine.settings().clone();
                p.host.ui(UiCommand::ShowSettings(settings));
            }
            Command::SetBuiltinRestore(enabled) => {
                let result = p.options.set_persist_browsers(*enabled);
                self.immediate
                    .push_back(Event::BuiltinRestoreSet { enabled: *enabled, result });
            }
            Command::SetStartAtSignIn(enabled) => match p.startup.set_enabled(*enabled) {
                Ok(StartupMechanism::RunKey) if *enabled => self.log_error(
                    now,
                    OP_START_AT_SIGN_IN,
                    "logon task unavailable; registered in the Run key, start may be delayed",
                ),
                Ok(_) => {}
                Err(e) => self.log_error(now, OP_START_AT_SIGN_IN, &e.message),
            },
            Command::PersistSettings(settings) => {
                if let Err(kind) = p.files.write_settings(&settings.serialize()) {
                    self.log_error(now, OP_PERSIST_SETTINGS, &format!("{kind:?}"));
                }
            }
            // Tab folding (Req 14). The command has no op id: a hide that
            // cannot even be queued is reported like one that failed, so the
            // fold goes on and `ERROR operation=fold-hide` is logged (14.16).
            Command::HideWindow { win } => {
                if let Err(error) = p.hider.hide(*win) {
                    self.immediate.push_back(Event::HideFailed { win: *win, error });
                }
            }
            // The answer arrives as `Event::FoldContext` through the mailbox.
            Command::ProbeFoldContext { probe, candidate } => {
                p.fold_probe.probe_fold_context(*probe, *candidate);
            }
            Command::CloseWindow { win } => {
                if let Err(e) = p.navigator.close_window(*win) {
                    self.log_error(now, OP_CLOSE_WINDOW, &e.message);
                }
            }
            Command::SetTimer { id, at } => self.timers.set(*id, *at),
            Command::Exit => self.exit = true,
        }
    }
}

/// The `ERROR` entry the panic guard logs.
pub fn panic_entry(description: &str) -> LogEntry {
    LogEntry::Error { operation: OP_PANIC.to_owned(), description: description.to_owned() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Arc;

    use esr_core::engine::{
        DesktopView, ExplorerOptionValues, FoldProbeId, IoErrorKind, ItemId, ObservedLocation, ProbeId, RequestId,
        SessionFileRead, TabObservation, TimerOwner, WindowObservation,
    };
    use esr_core::model::{DisplayRef, LocationId, LocationKind, Placement, Rect, ShowState, Volume};
    use esr_core::plan::placement::FinalPlacement;
    use esr_win::window_event_source::ChangeCause;
    use proptest::prelude::*;

    // -- fakes --------------------------------------------------------------

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        Rescan(WinKey),
        RescanAll,
        Watch(u32),
        Forget(WinKey),
        Ui(UiCommand),
        Write { seq: u64, urgent: bool },
        Flush(u32),
        Log(String),
        Open(RequestId),
        CreateTab(WinKey),
        CloseTab(TabKey),
        Select(WinKey, usize),
        Reveal(WinKey),
        RevealAll,
        NavDestroyed(WinKey),
        Probe(ProbeId),
        HiderHide(WinKey),
        HiderReveal(WinKey),
        FoldProbe(FoldProbeId, WinKey),
        CloseWindow(WinKey),
        SetPersist(bool),
        Startup(bool),
        Settings(String),
    }

    #[derive(Default)]
    struct World {
        calls: Vec<Call>,
        fail_nav: bool,
        fail_hider: bool,
        /// `TabStrip::activate` succeeds (it fails by default).
        strip_ok: bool,
        /// The ops of the `ActivateTab` commands carried out, in order.
        activations: Vec<OpId>,
        busy_source: bool,
        /// Complete every write on flush through the mailbox.
        inbox: Option<Arc<Mailbox>>,
        written: Vec<(u64, String)>,
    }

    type Shared = Rc<RefCell<World>>;

    fn err(msg: &str) -> ShellError {
        ShellError { code: 5, message: msg.into() }
    }

    struct FakeNav(Shared);
    impl Navigator for FakeNav {
        fn open_window(&mut self, r: RequestId, _: &LocationId, _: &FinalPlacement) -> Result<(), ShellError> {
            let mut w = self.0.borrow_mut();
            w.calls.push(Call::Open(r));
            if w.fail_nav { Err(err("open")) } else { Ok(()) }
        }
        fn apply_placement(&mut self, _: WinKey, _: &FinalPlacement) -> Result<(), ShellError> {
            Ok(())
        }
        fn create_tab(&mut self, win: WinKey) -> Result<(), ShellError> {
            let mut w = self.0.borrow_mut();
            w.calls.push(Call::CreateTab(win));
            if w.fail_nav { Err(err("create")) } else { Ok(()) }
        }
        fn navigate(&mut self, _: TabKey, _: &LocationId) -> Result<(), ShellError> {
            Ok(())
        }
        fn close_tab(&mut self, tab: TabKey) -> Result<(), ShellError> {
            self.0.borrow_mut().calls.push(Call::CloseTab(tab));
            Ok(())
        }
        fn select_items(&mut self, _: TabKey, _: &[ItemId]) -> Result<(), ShellError> {
            Ok(())
        }
        fn reveal(&mut self, win: WinKey) -> Result<(), ShellError> {
            let mut w = self.0.borrow_mut();
            w.calls.push(Call::Reveal(win));
            if w.fail_nav { Err(err("reveal")) } else { Ok(()) }
        }
        fn set_z_order(&mut self, _: &[WinKey]) -> Result<(), ShellError> {
            Ok(())
        }
        fn focus(&mut self, _: WinKey) -> Result<(), ShellError> {
            Ok(())
        }
        fn close_window(&mut self, win: WinKey) -> Result<(), ShellError> {
            let mut w = self.0.borrow_mut();
            w.calls.push(Call::CloseWindow(win));
            if w.fail_nav { Err(err("close")) } else { Ok(()) }
        }
        fn reveal_all(&mut self) {
            self.0.borrow_mut().calls.push(Call::RevealAll);
        }
        fn window_destroyed(&mut self, win: WinKey) {
            self.0.borrow_mut().calls.push(Call::NavDestroyed(win));
        }
    }

    struct FakeHider(Shared);
    impl Hider for FakeHider {
        fn hide(&mut self, win: WinKey) -> Result<(), ShellError> {
            let mut w = self.0.borrow_mut();
            w.calls.push(Call::HiderHide(win));
            if w.fail_hider { Err(err("hide")) } else { Ok(()) }
        }
        fn reveal(&mut self, win: WinKey) -> Result<(), ShellError> {
            self.0.borrow_mut().calls.push(Call::HiderReveal(win));
            Ok(())
        }
        fn sweep(&mut self) -> u32 {
            0
        }
    }

    struct FakeFoldProbe(Shared);
    impl FoldProbe for FakeFoldProbe {
        fn probe_fold_context(&mut self, probe: FoldProbeId, candidate: WinKey) {
            self.0.borrow_mut().calls.push(Call::FoldProbe(probe, candidate));
        }
    }

    struct FakeStrip(Shared);
    impl TabStrip for FakeStrip {
        fn read(&mut self, _: WinKey) -> Result<esr_core::engine::TabStripState, ShellError> {
            Err(err("read"))
        }
        fn activate(&mut self, op: OpId, win: WinKey, index: usize) -> Result<(), ShellError> {
            let mut w = self.0.borrow_mut();
            w.calls.push(Call::Select(win, index));
            w.activations.push(op);
            if w.strip_ok { Ok(()) } else { Err(err("UIA worker stopped")) }
        }
        fn watch(&mut self, _: WinKey) -> Result<(), ShellError> {
            Ok(())
        }
        fn unwatch(&mut self, _: WinKey) {}
    }

    struct FakeProber(Shared);
    impl Prober for FakeProber {
        fn probe(&mut self, probe: ProbeId, _: &LocationId) {
            self.0.borrow_mut().calls.push(Call::Probe(probe));
        }
    }

    struct FakeFiles(Shared);
    impl FileStore for FakeFiles {
        fn read_session(&mut self) -> SessionFileRead {
            SessionFileRead::Absent
        }
        fn write_session(&mut self, seq: u64, text: &str, urgent: bool) {
            let mut w = self.0.borrow_mut();
            w.calls.push(Call::Write { seq, urgent });
            w.written.push((seq, text.to_owned()));
        }
        fn preserve_backup(&mut self, _: u64, _: &[u8]) {}
        fn flush(&mut self, timeout_ms: u32) -> bool {
            let mut w = self.0.borrow_mut();
            w.calls.push(Call::Flush(timeout_ms));
            let seqs: Vec<u64> = w.written.iter().map(|(s, _)| *s).collect();
            if let Some(inbox) = w.inbox.clone() {
                for seq in seqs {
                    inbox.post(Event::WriteCompleted { seq, result: Ok(()) });
                }
            }
            true
        }
        fn read_settings(&mut self) -> Result<Option<String>, IoErrorKind> {
            Ok(None)
        }
        fn write_settings(&mut self, text: &str) -> Result<(), IoErrorKind> {
            self.0.borrow_mut().calls.push(Call::Settings(text.to_owned()));
            Err(IoErrorKind::AccessDenied)
        }
    }

    struct FakeLog(Shared);
    impl LogWriter for FakeLog {
        fn append(&mut self, line: &str) {
            self.0.borrow_mut().calls.push(Call::Log(line.to_owned()));
        }
    }

    struct FakeOptions(Shared);
    impl Options for FakeOptions {
        fn read(&mut self) -> ExplorerOptionValues {
            ExplorerOptionValues::default()
        }
        fn set_persist_browsers(&mut self, enabled: bool) -> Result<(), IoErrorKind> {
            self.0.borrow_mut().calls.push(Call::SetPersist(enabled));
            Err(IoErrorKind::AccessDenied)
        }
    }

    struct FakeNotifier;
    impl Notifier for FakeNotifier {
        fn notify_omitted(&mut self, _: u32) -> Result<(), ShellError> {
            Err(err("no toast"))
        }
    }

    struct FakeStartup(Shared);
    impl Startup for FakeStartup {
        fn set_enabled(&mut self, enabled: bool) -> Result<StartupMechanism, ShellError> {
            self.0.borrow_mut().calls.push(Call::Startup(enabled));
            Ok(StartupMechanism::RunKey)
        }
        fn unregister(&mut self) -> Result<(), ShellError> {
            Ok(())
        }
    }

    struct FakeHost(Shared);
    impl Host for FakeHost {
        fn rescan_window(&mut self, win: WinKey) {
            self.0.borrow_mut().calls.push(Call::Rescan(win));
        }
        fn rescan_all(&mut self) -> bool {
            let mut w = self.0.borrow_mut();
            w.calls.push(Call::RescanAll);
            !w.busy_source
        }
        fn watch_process(&mut self, pid: u32) -> Result<(), ShellError> {
            self.0.borrow_mut().calls.push(Call::Watch(pid));
            Ok(())
        }
        fn forget_window(&mut self, win: WinKey) {
            self.0.borrow_mut().calls.push(Call::Forget(win));
        }
        fn environment(&mut self, _: Millis) -> Environment {
            Environment::default()
        }
        fn local_now(&mut self) -> Timestamp {
            Timestamp { year: 2025, month: 1, day: 31, hour: 9, minute: 15, second: 2, millis: 123, offset_minutes: 60 }
        }
        fn ui(&mut self, command: UiCommand) {
            self.0.borrow_mut().calls.push(Call::Ui(command));
        }
    }

    fn ports(world: &Shared) -> Ports {
        Ports {
            navigator: Box::new(FakeNav(world.clone())),
            tab_strip: Box::new(FakeStrip(world.clone())),
            prober: Box::new(FakeProber(world.clone())),
            hider: Box::new(FakeHider(world.clone())),
            fold_probe: Box::new(FakeFoldProbe(world.clone())),
            files: Box::new(FakeFiles(world.clone())),
            log: Box::new(FakeLog(world.clone())),
            options: Box::new(FakeOptions(world.clone())),
            notifier: Box::new(FakeNotifier),
            startup: Box::new(FakeStartup(world.clone())),
            host: Box::new(FakeHost(world.clone())),
        }
    }

    fn startup() -> StartupInput {
        StartupInput {
            session_file: SessionFileRead::Absent,
            environment: Environment::default(),
            background_logon: false,
            logon_at: None,
            shell_ready_at: None,
        }
    }

    fn core(world: &Shared) -> RuntimeCore {
        RuntimeCore::new(Settings::default(), startup(), ports(world), Millis(0))
    }

    fn calls(world: &Shared) -> Vec<Call> {
        std::mem::take(&mut world.borrow_mut().calls)
    }

    const W1: WinKey = WinKey { hwnd: 0x10, seq: 1 };
    const W2: WinKey = WinKey { hwnd: 0x20, seq: 2 };
    const T1: TabKey = TabKey { hwnd: 0x11, seq: 1 };
    const T2: TabKey = TabKey { hwnd: 0x21, seq: 2 };
    const T9: TabKey = TabKey { hwnd: 0x91, seq: 9 };

    fn location(path: &str) -> LocationId {
        LocationId {
            case_sensitive: false,
            kind: LocationKind::FileSystem(Volume::Fixed),
            parsing_name: path.into(),
            display_name: path.rsplit('\\').next().unwrap_or(path).into(),
            pidl: Vec::new(),
            search: None,
        }
    }

    fn scan(win: WinKey, tab: TabKey, pid: u32) -> Event {
        let rect = Rect { left: 0, top: 0, right: 800, bottom: 600 };
        Event::WindowScanned(WindowObservation {
            win,
            pid,
            tabs: vec![TabObservation { tab, location: ObservedLocation::Known(location(r"C:\Data")) }],
            active: Some(tab),
            strip: None,
            placement: Placement {
                state: ShowState::Normal,
                restored: rect,
                display: DisplayRef { device: r"\\.\DISPLAY1".into(), monitor: Rect { left: 0, top: 0, right: 1920, bottom: 1080 } },
            },
        })
    }

    // -- timers -------------------------------------------------------------

    #[test]
    fn timer_wheel_replaces_and_delivers_in_deadline_order() {
        let a = TimerId { owner: TimerOwner::Store, seq: 1 };
        let b = TimerId { owner: TimerOwner::Tracker, seq: 7 };
        let mut t = TimerWheel::default();
        t.set(a, Millis(500));
        t.set(b, Millis(300));
        t.set(a, Millis(200)); // replaces
        assert_eq!(t.len(), 2);
        assert_eq!(t.next(), Some(Millis(200)));
        assert!(t.take_due(Millis(199)).is_empty());
        assert_eq!(t.take_due(Millis(1_000)), vec![a, b]);
        assert!(t.is_empty());
    }

    proptest! {
        /// Every set timer fires exactly once, at or after its last deadline,
        /// and timers fire in deadline order.
        #[test]
        fn timer_wheel_fires_each_timer_once(sets in proptest::collection::vec((0u64..6, 0u64..2_000), 0..40),
                                             steps in proptest::collection::vec(1u64..300, 1..30)) {
            let mut t = TimerWheel::default();
            let mut last: BTreeMap<TimerId, Millis> = BTreeMap::new();
            for (seq, at) in &sets {
                let id = TimerId { owner: TimerOwner::Restore, seq: *seq };
                t.set(id, Millis(*at));
                last.insert(id, Millis(*at));
            }
            let mut now = 0;
            let mut fired: Vec<(TimerId, Millis)> = Vec::new();
            for s in steps.iter().chain(std::iter::once(&5_000)) {
                now += s;
                for id in t.take_due(Millis(now)) {
                    prop_assert!(last[&id] <= Millis(now));
                    fired.push((id, last[&id]));
                }
            }
            prop_assert_eq!(fired.len(), last.len());
            prop_assert!(fired.windows(2).all(|w| w[0].1 <= w[1].1));
        }
    }

    #[test]
    fn engine_timers_are_scheduled_and_delivered() {
        let world = Shared::default();
        let mut c = core(&world);
        c.handle(Millis(1_000), Inbound::Event(scan(W1, T1, 7)));
        // The store schedules a checkpoint 500 ms after the change (Req 3.7).
        let deadline = c.next_deadline().expect("checkpoint timer");
        assert!(deadline > Millis(1_000) && deadline <= Millis(1_500), "{deadline:?}");
        calls(&world);
        c.tick(Millis(deadline.0 - 1));
        assert!(!calls(&world).iter().any(|c| matches!(c, Call::Write { .. })));
        c.tick(deadline);
        assert!(calls(&world).iter().any(|c| matches!(c, Call::Write { urgent: false, .. })));
    }

    // -- op ids -------------------------------------------------------------

    #[test]
    fn tab_started_maps_to_the_outstanding_create_without_settling_it() {
        let mut m = OpMap::default();
        let started = NavigatorSignal::TabStarted { win: W1, tab: T1 };
        // A tab nobody asked for reports nothing.
        assert_eq!(m.signal(started.clone()), None);
        m.issued(&Command::CreateTab { op: OpId(4), win: W1 });
        let expected = Some(Event::TabStarted { op: OpId(4), tab: T1 });
        assert_eq!(m.signal(started.clone()), expected);
        // Still outstanding: a later failure maps to the same op.
        assert_eq!(m.signal(started), expected);
        m.tab_created(W1);
        assert!(m.is_empty());
    }

    #[test]
    fn fold_commands_reach_their_ports() {
        let world = Shared::default();
        let mut c = core(&world);
        c.dispatch(Millis(1), Command::HideWindow { win: W1 });
        c.dispatch(Millis(1), Command::ProbeFoldContext { probe: FoldProbeId(3), candidate: W1 });
        c.dispatch(Millis(1), Command::CloseWindow { win: W2 });
        c.dispatch(Millis(1), Command::Reveal { win: W1 });
        assert_eq!(
            calls(&world),
            vec![
                Call::HiderHide(W1),
                Call::FoldProbe(FoldProbeId(3), W1),
                Call::CloseWindow(W2),
                Call::HiderReveal(W1),
                Call::Reveal(W1),
            ]
        );
    }

    #[test]
    fn a_hide_that_cannot_be_queued_becomes_hide_failed() {
        let world = Shared::default();
        world.borrow_mut().fail_hider = true;
        let mut c = core(&world);
        c.dispatch(Millis(1), Command::HideWindow { win: W1 });
        assert_eq!(c.immediate.pop_front(), Some(Event::HideFailed { win: W1, error: err("hide") }));
    }

    #[test]
    fn a_close_window_that_cannot_be_posted_is_logged_not_failed() {
        let world = Shared::default();
        world.borrow_mut().fail_nav = true;
        let mut c = core(&world);
        calls(&world);
        c.dispatch(Millis(1), Command::CloseWindow { win: W2 });
        let log = calls(&world);
        assert_eq!(log[0], Call::CloseWindow(W2));
        let Call::Log(line) = &log[1] else { panic!("{log:?}") };
        assert!(
            line.starts_with("2025-01-31 09:15:02.123 +01:00 ERROR operation=close-window description=close"),
            "{line}"
        );
        assert_eq!(log.len(), 2, "{log:?}");
        // No op id, so nothing goes back to the Engine.
        assert!(c.immediate.is_empty());
    }

    // -- adapter events reach the Engine ---------------------------------------

    fn appeared(win: WinKey, pid: u32) -> Inbound {
        Inbound::Event(Event::WindowAppeared {
            win,
            pid,
            bypass_keys: false,
            desktop: DesktopView::Unknown,
            foreground: None,
        })
    }

    /// Posts `items` into a mailbox and drains it into the core, the way the
    /// Engine thread does (every item stamped one millisecond after the last).
    fn run(c: &mut RuntimeCore, inbox: &Mailbox, from_ms: u64, items: Vec<Inbound>) {
        for item in items {
            inbox.push(item);
        }
        let mut t = from_ms;
        c.drain(inbox, &mut || {
            t += 1;
            Millis(t)
        });
        assert!(inbox.is_empty());
    }

    /// The tab-folding port calls since the last look, and the folding
    /// entries of the Activity_Log.
    fn fold_calls(world: &Shared) -> Vec<Call> {
        calls(world)
            .into_iter()
            .filter(|c| match c {
                Call::HiderHide(_)
                | Call::HiderReveal(_)
                | Call::Reveal(_)
                | Call::FoldProbe(..)
                | Call::CreateTab(_)
                | Call::Select(..)
                | Call::CloseWindow(_) => true,
                Call::Log(l) => l.contains("operation=fold-hide") || l.contains("not-folded"),
                _ => false,
            })
            .collect()
    }

    #[test]
    fn fold_events_posted_through_the_mailbox_reach_the_engine_in_order() {
        let world = Shared::default();
        world.borrow_mut().strip_ok = true;
        let inbox = Mailbox::new();
        let mut c = core(&world);
        // W2 shows a folder W1 does not (else the Engine would switch to W1's
        // Matching_Tab instead of creating a Folded_Tab).
        let place = location(r"C:\Other");
        let mut scan_w2 = scan(W2, T2, 8);
        if let Event::WindowScanned(obs) = &mut scan_w2 {
            obs.tabs[0].location = ObservedLocation::Known(place.clone());
        }
        calls(&world);

        // W1 is the Fold_Target; W2 appears, fails to hide (the failure only
        // counts once W2 is a candidate), is scanned and announces its
        // Location: the Engine asks for the fold context.
        run(
            &mut c,
            &inbox,
            1_000,
            vec![
                appeared(W1, 7),
                Inbound::Event(scan(W1, T1, 7)),
                appeared(W2, 8),
                Inbound::Event(Event::HideFailed { win: W2, error: err("hide") }),
                Inbound::Event(scan_w2),
                Inbound::Event(Event::InitialLocationKnown {
                    win: W2,
                    tab: Some(T2),
                    location: place.clone(),
                    selection: Vec::new(),
                }),
            ],
        );
        let log = fold_calls(&world);
        let [Call::HiderHide(hidden), Call::Log(entry), Call::FoldProbe(probe, candidate)] = log.as_slice() else {
            panic!("{log:?}")
        };
        assert_eq!((*hidden, *candidate), (W2, W2));
        assert!(entry.contains("ERROR operation=fold-hide"), "{entry}");
        assert_eq!(c.engine().folding().candidates_in_progress(), 1);

        // The answer: the Engine creates the Folded_Tab in the Fold_Target.
        let desktop = DesktopView::Unknown;
        run(
            &mut c,
            &inbox,
            2_000,
            vec![Inbound::Event(Event::FoldContext {
                probe: *probe,
                desktop,
                unresponsive: std::collections::BTreeSet::new(),
            })],
        );
        assert_eq!(fold_calls(&world), vec![Call::CreateTab(W1)]);

        // The navigator reports the new tab's window, the tab registers and
        // its navigation completes: the Engine activates it.
        run(
            &mut c,
            &inbox,
            3_000,
            vec![
                Inbound::Navigator(NavigatorSignal::TabStarted { win: W1, tab: T9 }),
                Inbound::Event(Event::TabCreated { win: W1, tab: T9 }),
                Inbound::Event(Event::NavigationCompleted { tab: T9, location: place }),
            ],
        );
        let log = fold_calls(&world);
        assert!(matches!(log.as_slice(), [Call::Select(W1, _)]), "{log:?}");

        // Its confirmation ends the fold: the candidate is closed.
        let op = *world.borrow().activations.last().expect("an activation");
        run(&mut c, &inbox, 4_000, vec![Inbound::Event(Event::ActivationConfirmed { op })]);
        assert_eq!(fold_calls(&world), vec![Call::CloseWindow(W2)]);
    }

    #[test]
    fn a_tab_detached_after_the_appearance_reveals_the_candidate() {
        let world = Shared::default();
        let inbox = Mailbox::new();
        let mut c = core(&world);
        calls(&world);
        run(
            &mut c,
            &inbox,
            1_000,
            vec![
                Inbound::Event(scan(W1, T1, 7)),
                appeared(W2, 8),
                Inbound::Event(scan(W2, T2, 8)),
                Inbound::Event(Event::TabDetached { from: W1 }),
            ],
        );
        // Hidden first, then revealed by both ports and rescanned: the drag-out
        // proved it is not a new window of the user's own.
        let log = calls(&world);
        let at = |call: Call| log.iter().position(|c| *c == call).unwrap_or_else(|| panic!("{call:?} in {log:?}"));
        assert!(at(Call::HiderHide(W2)) < at(Call::HiderReveal(W2)));
        assert!(at(Call::HiderReveal(W2)) < at(Call::Reveal(W2)));
        assert!(at(Call::Reveal(W2)) < at(Call::Rescan(W2)));
        assert_eq!(c.engine().folding().candidates_in_progress(), 0);
        assert!(c.engine().folding().is_excluded(W2));
    }

    #[test]
    fn turning_folding_off_reaches_the_folding_engine() {
        let world = Shared::default();
        let inbox = Mailbox::new();
        let mut c = core(&world);
        run(
            &mut c,
            &inbox,
            1_000,
            vec![appeared(W1, 7), Inbound::Event(scan(W1, T1, 7)), appeared(W2, 8), Inbound::Event(scan(W2, T2, 8))],
        );
        assert!(c.engine().folding().enabled());
        assert_eq!(c.engine().folding().candidates_in_progress(), 1);
        assert!(calls(&world).contains(&Call::HiderHide(W2)));

        let off = Settings { tab_folding_enabled: false, ..Settings::default() };
        run(&mut c, &inbox, 2_000, vec![Inbound::Event(Event::SettingsChanged(off))]);
        // The candidate in progress is left open: revealed through both
        // ports, rescanned, and logged as `not-folded`.
        let log = calls(&world);
        assert!(!c.engine().folding().enabled());
        assert_eq!(c.engine().folding().candidates_in_progress(), 0);
        for call in [Call::HiderReveal(W2), Call::Reveal(W2), Call::Rescan(W2)] {
            assert!(log.contains(&call), "{call:?} in {log:?}");
        }
        assert!(
            log.iter().any(|c| matches!(c, Call::Log(l) if l.contains("not-folded") && l.contains("folding-disabled"))),
            "{log:?}"
        );

        // Later windows are not hidden.
        run(&mut c, &inbox, 3_000, vec![appeared(WinKey { hwnd: 0x30, seq: 3 }, 9)]);
        assert!(!calls(&world).iter().any(|c| matches!(c, Call::HiderHide(_))));
    }

    #[test]
    fn navigator_signals_map_to_the_outstanding_op() {
        let mut m = OpMap::default();
        m.issued(&Command::CreateTab { op: OpId(4), win: W1 });
        m.issued(&Command::CloseTab { op: OpId(9), tab: T1 });
        let e = err("x");
        assert_eq!(
            m.signal(NavigatorSignal::TabCreateRetried {
                win: W1,
                attempt: 2,
                strategy: esr_win::strategy::TabCreateStrategy::WmCommand
            }),
            None
        );
        assert_eq!(
            m.signal(NavigatorSignal::TabCreateFailed { win: W1, error: e.clone() }),
            Some(Event::OperationFailed { op: OpId(4), error: e.clone() })
        );
        // Reported once.
        assert_eq!(m.signal(NavigatorSignal::TabCreateFailed { win: W1, error: e.clone() }), None);
        assert_eq!(
            m.signal(NavigatorSignal::CloseFailed { tab: T1, error: e.clone() }),
            Some(Event::OperationFailed { op: OpId(9), error: e.clone() })
        );
        assert_eq!(
            m.signal(NavigatorSignal::OpenFailed { request: RequestId(12), error: e.clone() }),
            Some(Event::OperationFailed { op: OpId(12), error: e.clone() })
        );
        // Settled operations no longer fail.
        m.issued(&Command::CreateTab { op: OpId(5), win: W2 });
        m.tab_created(W2);
        m.issued(&Command::CloseTab { op: OpId(6), tab: T1 });
        m.tab_revoked(T1);
        assert_eq!(m.signal(NavigatorSignal::TabCreateFailed { win: W2, error: e.clone() }), None);
        assert_eq!(m.signal(NavigatorSignal::CloseFailed { tab: T1, error: e.clone() }), None);
        assert!(m.is_empty());

        // A browser worker's failed Navigate2 fails the tab's latest Navigate,
        // until the tab's navigation completed or the tab is gone.
        let to = location("C:\\x");
        m.issued(&Command::Navigate { op: OpId(20), tab: T1, location: to.clone() });
        m.issued(&Command::Navigate { op: OpId(21), tab: T1, location: to.clone() });
        assert_eq!(
            m.signal(NavigatorSignal::NavigateFailed { tab: T1, error: e.clone() }),
            Some(Event::OperationFailed { op: OpId(21), error: e.clone() })
        );
        assert_eq!(m.signal(NavigatorSignal::NavigateFailed { tab: T1, error: e.clone() }), None);
        m.issued(&Command::Navigate { op: OpId(22), tab: T1, location: to.clone() });
        m.navigated(T1);
        assert_eq!(m.signal(NavigatorSignal::NavigateFailed { tab: T1, error: e.clone() }), None);
        m.issued(&Command::Navigate { op: OpId(23), tab: T1, location: to });
        m.tab_revoked(T1);
        assert_eq!(m.signal(NavigatorSignal::NavigateFailed { tab: T1, error: e }), None);
        assert!(m.is_empty());
    }

    #[test]
    fn synchronous_port_failures_become_operation_failed_or_errors() {
        let world = Shared::default();
        world.borrow_mut().fail_nav = true;
        let mut c = core(&world);
        c.dispatch(Millis(1), Command::CreateTab { op: OpId(3), win: W1 });
        assert_eq!(c.immediate.pop_front(), Some(Event::OperationFailed { op: OpId(3), error: err("create") }));
        assert!(c.ops.is_empty(), "a failed create is not outstanding");
        c.dispatch(Millis(1), Command::ActivateTab { op: OpId(8), win: W1, index: 2 });
        assert_eq!(
            c.immediate.pop_front(),
            Some(Event::OperationFailed { op: OpId(8), error: err("UIA worker stopped") })
        );
        calls(&world);
        // No op: logged as an ERROR entry with the local timestamp.
        c.dispatch(Millis(1), Command::Reveal { win: W1 });
        let log = calls(&world);
        assert_eq!(log[..2], [Call::HiderReveal(W1), Call::Reveal(W1)]);
        let Call::Log(line) = &log[2] else { panic!("{log:?}") };
        assert!(line.starts_with("2025-01-31 09:15:02.123 +01:00 ERROR operation=reveal-window"), "{line}");
        assert!(c.immediate.is_empty());
    }

    // -- destroy suppression ---------------------------------------------------

    #[test]
    fn destroy_of_a_killed_process_window_is_suppressed() {
        let mut f = DestroyFilter::default();
        f.note_pid(W1, 7);
        assert_eq!(f.destroyed(W1, Millis(1_000)), None);
        assert!(f.is_holding(W1));
        // ProcessExited (200 ms after the signal) within the hold.
        f.process_exited(7);
        assert!(f.poll(Millis(5_000)).is_empty());
        // A later destroy of another window of the dead process is dropped too.
        f.note_pid(W2, 7);
        f.process_exited(7);
        assert_eq!(f.destroyed(W2, Millis(5_001)), None);
        assert!(!f.is_holding(W2));
    }

    #[test]
    fn orderly_close_destroy_is_delivered() {
        let mut f = DestroyFilter::default();
        f.note_pid(W1, 7);
        // Revocation first: delivered at once.
        assert_eq!(f.tab_revoked(W1, Millis(100)), None);
        assert_eq!(f.destroyed(W1, Millis(150)), Some(Event::WindowDestroyed { win: W1 }));
        // Revocation after the destroy releases it.
        f.note_pid(W2, 7);
        assert_eq!(f.destroyed(W2, Millis(200)), None);
        assert_eq!(f.tab_revoked(W2, Millis(220)), Some(Event::WindowDestroyed { win: W2 }));
        assert_eq!(f.next_deadline(), None);
        // No revocation and no exit: released when the hold ends.
        let w3 = WinKey { hwnd: 0x30, seq: 3 };
        f.note_pid(w3, 8);
        assert_eq!(f.destroyed(w3, Millis(1_000)), None);
        assert_eq!(f.next_deadline(), Some(Millis(1_000 + DESTROY_HOLD_MS)));
        assert!(f.poll(Millis(1_000 + DESTROY_HOLD_MS - 1)).is_empty());
        assert_eq!(f.poll(Millis(1_000 + DESTROY_HOLD_MS)), vec![Event::WindowDestroyed { win: w3 }]);
        // The exit of another process does not drop it.
        let w4 = WinKey { hwnd: 0x40, seq: 4 };
        f.note_pid(w4, 8);
        assert_eq!(f.destroyed(w4, Millis(2_000)), None);
        f.process_exited(9);
        assert_eq!(f.poll(Millis(3_000)), vec![Event::WindowDestroyed { win: w4 }]);
    }

    #[test]
    fn an_old_revocation_does_not_make_a_kill_look_orderly() {
        let mut f = DestroyFilter::default();
        f.note_pid(W1, 7);
        f.tab_revoked(W1, Millis(0)); // the user closed a tab long ago
        assert_eq!(f.destroyed(W1, Millis(REVOKE_RECENT_MS + 1)), None);
        f.process_exited(7);
        assert!(f.poll(Millis(60_000)).is_empty());
    }

    #[test]
    fn a_reused_pid_is_not_treated_as_exited() {
        let mut f = DestroyFilter::default();
        f.note_pid(W1, 7);
        f.process_exited(7);
        f.note_pid(W2, 7); // new process with the same PID
        assert_eq!(f.destroyed(W2, Millis(0)), None);
        assert_eq!(f.poll(Millis(DESTROY_HOLD_MS)), vec![Event::WindowDestroyed { win: W2 }]);
    }

    #[test]
    fn killed_explorer_orphans_its_window_through_the_core() {
        // Scan W1 (PID 7), destroy without revocation, then ProcessExited:
        // the Engine sees no WindowDestroyed, so the Session_End is
        // ExplorerTerminated, not LastWindowClosed (Req 3.3, 3.4).
        let world = Shared::default();
        let mut c = core(&world);
        c.handle(Millis(1_000), Inbound::Event(scan(W1, T1, 7)));
        assert!(calls(&world).contains(&Call::Watch(7)));
        c.handle(Millis(2_000), Inbound::Window(WindowSignal::Destroyed { win: W1 }));
        let after_destroy = calls(&world);
        assert!(after_destroy.contains(&Call::Forget(W1)));
        assert!(after_destroy.contains(&Call::NavDestroyed(W1)));
        assert!(!after_destroy.iter().any(|c| matches!(c, Call::Write { urgent: true, .. })));
        c.handle(Millis(2_250), Inbound::Window(WindowSignal::ProcessExited { pid: 7 }));
        c.tick(Millis(10_000));
        let written = world.borrow().written.clone();
        let (_, text) = written.last().expect("Session_End write");
        assert!(text.contains("cause explorer-terminated"), "{text}");
    }

    #[test]
    fn orderly_last_window_close_saves_last_window_closed() {
        let world = Shared::default();
        let mut c = core(&world);
        c.handle(Millis(1_000), Inbound::Event(scan(W1, T1, 7)));
        c.handle(Millis(2_000), Inbound::Window(WindowSignal::Destroyed { win: W1 }));
        c.handle(Millis(2_050), Inbound::Event(Event::TabRevoked { win: W1, tab: T1 }));
        let written = world.borrow().written.clone();
        let (_, text) = written.last().expect("Session_End write");
        assert!(text.contains("cause last-"), "{text}");
    }

    // -- dispatch -------------------------------------------------------------

    #[test]
    fn window_signals_install_watches_once_and_rescan() {
        let world = Shared::default();
        let mut c = core(&world);
        calls(&world);
        let changed = |pid| Inbound::Window(WindowSignal::Changed { win: W1, pid, cause: ChangeCause::Created });
        c.handle(Millis(5), changed(7));
        c.handle(Millis(6), changed(7));
        assert_eq!(calls(&world), vec![Call::Watch(7), Call::Rescan(W1), Call::Rescan(W1)]);
        // The source drops the watch at exit; the next sighting re-installs it.
        c.handle(Millis(7), Inbound::Window(WindowSignal::ProcessExited { pid: 7 }));
        calls(&world);
        c.handle(Millis(8), changed(7));
        assert_eq!(calls(&world), vec![Call::Watch(7), Call::Rescan(W1)]);
    }

    #[test]
    fn ui_and_settings_commands() {
        let world = Shared::default();
        let mut c = core(&world);
        calls(&world);
        c.handle(Millis(1), Inbound::ShowSettings);
        c.dispatch(Millis(1), Command::ShowPrompt);
        assert_eq!(
            calls(&world),
            vec![Call::Ui(UiCommand::ShowSettings(Settings::default())), Call::Ui(UiCommand::ShowPrompt)]
        );
        // SetBuiltinRestore reports its result back to the Engine.
        c.dispatch(Millis(2), Command::SetBuiltinRestore(false));
        assert_eq!(
            c.immediate.pop_front(),
            Some(Event::BuiltinRestoreSet { enabled: false, result: Err(IoErrorKind::AccessDenied) })
        );
        // Run-key fallback and failed settings writes are logged.
        c.dispatch(Millis(3), Command::SetStartAtSignIn(true));
        c.dispatch(Millis(3), Command::PersistSettings(Settings::default()));
        let log = calls(&world);
        assert_eq!(log[0], Call::SetPersist(false));
        assert_eq!(log[1], Call::Startup(true));
        assert!(matches!(&log[2], Call::Log(l) if l.contains("operation=start-at-sign-in")));
        assert!(matches!(&log[3], Call::Settings(_)));
        assert!(matches!(&log[4], Call::Log(l) if l.contains("operation=persist-settings")));
        c.dispatch(Millis(4), Command::Exit);
        assert!(c.exit_requested());
    }

    #[test]
    fn busy_rescan_all_is_retried() {
        let world = Shared::default();
        world.borrow_mut().busy_source = true;
        let mut c = core(&world);
        calls(&world);
        c.dispatch(Millis(100), Command::RescanAll);
        assert_eq!(c.next_deadline(), Some(Millis(100 + RESCAN_ALL_RETRY_MS)));
        world.borrow_mut().busy_source = false;
        c.tick(Millis(100 + RESCAN_ALL_RETRY_MS));
        assert_eq!(calls(&world), vec![Call::RescanAll, Call::RescanAll]);
        assert_eq!(c.next_deadline(), None);
    }

    #[test]
    fn end_session_blocks_for_the_urgent_write() {
        let world = Shared::default();
        let inbox = Arc::new(Mailbox::new());
        world.borrow_mut().inbox = Some(inbox.clone());
        let mut c = core(&world);
        c.handle(Millis(1_000), Inbound::Event(scan(W1, T1, 7)));
        calls(&world);
        let mut t = 2_000;
        let mut clock = || {
            t += 10;
            Millis(t)
        };
        assert!(c.end_session(EndKind::Shutdown, &inbox, &mut clock, END_SESSION_BLOCK_MS));
        let log = calls(&world);
        let write = log.iter().position(|c| matches!(c, Call::Write { urgent: true, .. })).expect("urgent write");
        let Some(Call::Flush(ms)) = log[write..].iter().find(|c| matches!(c, Call::Flush(_))) else {
            panic!("{log:?}")
        };
        assert!(u64::from(*ms) <= END_SESSION_BLOCK_MS && *ms > 1_000, "{ms}");
        assert!(!c.urgent_write_pending());
        let (_, text) = world.borrow().written.last().cloned().unwrap();
        assert!(text.contains("cause shutdown"), "{text}");
    }

    #[test]
    fn end_session_without_windows_does_not_block() {
        let world = Shared::default();
        let inbox = Arc::new(Mailbox::new());
        let mut c = core(&world);
        calls(&world);
        let mut clock = || Millis(10);
        assert!(c.end_session(EndKind::SignOut, &inbox, &mut clock, END_SESSION_BLOCK_MS));
        assert!(!calls(&world).iter().any(|c| matches!(c, Call::Write { .. } | Call::Flush(_))));
    }

    #[test]
    fn mailbox_wakes_once_per_batch() {
        let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let inbox = Mailbox::new();
        inbox.post(Event::ShellLost); // before the waker: queued, no wake
        let w = wakes.clone();
        inbox.set_waker(Box::new(move || {
            w.fetch_add(1, Ordering::SeqCst);
        }));
        assert_eq!(wakes.load(Ordering::SeqCst), 1);
        inbox.post(Event::ShellAvailable);
        inbox.window_changed(W1);
        assert_eq!(wakes.load(Ordering::SeqCst), 1);
        inbox.begin_drain();
        assert_eq!(inbox.pop(), Some(Inbound::Event(Event::ShellLost)));
        inbox.post(Event::ShellLost);
        assert_eq!(wakes.load(Ordering::SeqCst), 2);
        assert_eq!(inbox.len(), 3);
    }

    #[test]
    fn panic_entry_is_an_error_line() {
        let ts = Timestamp { year: 2025, month: 1, day: 31, hour: 9, minute: 15, second: 2, millis: 123, offset_minutes: 0 };
        let line = format_entry(&ts, &panic_entry("boom at x.rs:1"));
        assert_eq!(line, "2025-01-31 09:15:02.123 +00:00 ERROR operation=panic description=boom%20at%20x.rs:1");
    }
}
