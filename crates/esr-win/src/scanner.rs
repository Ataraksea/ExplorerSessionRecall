//! Debounced per-window rescan producing `WindowScanned` observations.
//!
//! Every signal about an Explorer window (`window_changed`: a tab registered,
//! a title, placement, or tab strip change, a reconnection) schedules a full
//! rescan of that window. The pure core diffs the result against the
//! Live_Session, so lost or duplicated signals cannot corrupt it (design.md,
//! "Window rescan").
//!
//! # Debounce
//!
//! Signals are collapsed per window ([`ScanScheduler`], pure, logical time):
//! a window is scanned [`SCAN_DEBOUNCE_MS`] after its *last* signal, but no
//! later than [`SCAN_MAX_DELAY_MS`] after the first signal of the burst, so a
//! continuous stream of signals (a window being dragged) still yields a scan
//! well within the 1 s budget of Req 2.2–2.4.
//!
//! # One scan
//!
//! 1. The tab strip is read through UIA ([`StripSource::read`], the
//!    `TabStripReader`, whose worker makes the UIA calls). The read runs on a
//!    short-lived helper thread so the owning thread never waits on UIA; the
//!    result comes back as a message. At most one read per window is in
//!    flight; signals that arrive meanwhile start another scan as soon as it
//!    completes. A failed or timed-out read gives `strip: None` (the
//!    reconciler then keeps the previous order and appends new tabs); a
//!    timed-out read that later succeeds is reported by the reader's change
//!    callback, which rescans the window with the now cached strip.
//! 2. Back on the owning thread: the window's tabs and their Locations
//!    ([`TabEnumerator`], the `ShellWindowsSource`, built on `shell_util`).
//! 3. The active tab hint `obs.active` (see below).
//! 4. `GetWindowPlacement`, converted from workspace to screen coordinates
//!    ([`window_placement`]).
//! 5. `WindowScanned(WindowObservation)` is posted to the [`ScanHandler`].
//!
//! A window whose key is no longer live (destroyed, or its HWND reused) is
//! forgotten and produces nothing; `WindowDestroyed` comes from the
//! `WindowEventSource`. The first scan of a window subscribes to its strip's
//! `StructureChanged` ([`StripSource::watch`]); [`WindowScanner::forget`]
//! unsubscribes.
//!
//! # Active tab
//!
//! The UIA selection is the authoritative Active_Tab (the tracker prefers
//! `strip.selected`). `obs.active` is a fallback for when UIA is unavailable.
//! On 22621 every `ShellTabWindowClass` child reports visible with identical
//! rects and no cloaking, but Explorer raises the active tab's child to the
//! top of its siblings' z-order (checked against real Explorer in
//! `tests/scanner_integration.rs`). [`pick_active`] therefore takes the
//! topmost *visible* `ShellTabWindowClass` child (which is also the only
//! visible one on builds that hide inactive tabs). If that child is not one
//! of the scanned tabs (a tab still registering or being dragged), or there
//! is none, `active` is `None`.
//!
//! # Threading
//!
//! [`WindowScanner`] is bound to the thread that created it (`!Send`),
//! normally the Engine STA, which must pump messages: the debounce timer and
//! the read results are messages to the scanner's message-only window. The
//! [`TabEnumerator`] and the handler are called on that thread, never while
//! the scanner holds an internal borrow, and re-entrant calls (a COM call
//! pumping messages during a scan) are deferred until the current pass ends.
//! Other threads (the UIA worker's change callback, UIA event threads) signal
//! through a [`ScanRequester`], which posts a message.
//!
//! # Tab-to-window map
//!
//! With [`WindowScanner::set_tab_windows`], every scan also refreshes the
//! `WindowEventSource`'s map from tab window to owning window with the
//! window's `ShellTabWindowClass` children ([`tab_window_hwnds`]), so that the
//! source can report `TabDetached` for a destroyed tab window (Req 14.3 b).
//!
//! Requirements 2.1, 2.2, 2.3, 2.4, 2.9, 14.3.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashSet};
use std::ffi::c_void;
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex, PoisonError};

use esr_core::engine::{
    Event, ShellError, TabKey, TabObservation, TabStripState, WinKey, WindowObservation,
};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GW_CHILD, GW_HWNDNEXT, GWLP_USERDATA,
    GetWindow, GetWindowLongPtrW, HWND_MESSAGE, KillTimer, PostMessageW, RegisterClassW, SetTimer,
    SetWindowLongPtrW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_TIMER, WNDCLASSW,
};
use windows::core::{PCWSTR, w};

use crate::keys::{WindowKeys, hwnd_of, hwnd_value};
use crate::shell_util::{
    SHELL_TAB_WINDOW_CLASS, is_cabinet_window, is_window_visible, window_class_name,
};
use crate::shell_windows_source::{ShellWindowTabs, ShellWindowsSource};
use crate::tab_strip::{ChangeCallback, TabStripReader};
use crate::window_event_source::tab_detach::TabWindows;
use crate::window_event_source::window_placement;

// `ScanRequester` carries both halves of a `WinKey` in `WPARAM`/`LPARAM`.
const _: () = assert!(usize::BITS == 64, "the scanner assumes a 64-bit target");

/// Quiet time after a window's last signal before it is scanned (design.md).
pub const SCAN_DEBOUNCE_MS: u64 = 100;
/// Upper bound on the delay between a window's first pending signal and its
/// scan, so continuous signalling cannot postpone a scan indefinitely.
pub const SCAN_MAX_DELAY_MS: u64 = 400;
/// Stack size of the short-lived strip read threads (they only wait).
const READ_THREAD_STACK: usize = 64 * 1024;
/// Upper bound on the children examined when looking for the active tab.
const MAX_CHILDREN: usize = 4096;

/// A signal for the window in `wParam`/`lParam` (`WinKey::hwnd`/`seq`).
const WM_SCAN_SIGNAL: u32 = WM_APP + 0x91;
/// A strip read finished (the result is in the shared result queue).
const WM_STRIP_DONE: u32 = WM_APP + 0x92;
const TIMER_DEBOUNCE: usize = 1;

const WINDOW_CLASS: PCWSTR = w!("ESR.WindowScanner");

// ---------------------------------------------------------------------------
// Pure scheduling (logical time)
// ---------------------------------------------------------------------------

/// Debounce timing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DebounceConfig {
    /// Quiet time after the last signal ([`SCAN_DEBOUNCE_MS`]).
    pub quiet_ms: u64,
    /// Maximum delay after the first signal of a burst ([`SCAN_MAX_DELAY_MS`]).
    /// Values below `quiet_ms` are treated as `quiet_ms`.
    pub max_delay_ms: u64,
}

impl Default for DebounceConfig {
    fn default() -> Self {
        Self {
            quiet_ms: SCAN_DEBOUNCE_MS,
            max_delay_ms: SCAN_MAX_DELAY_MS,
        }
    }
}

/// Identifies one started scan of a window, so a late result of a scan that
/// was superseded (the window was forgotten meanwhile) is not mistaken for
/// the current one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ScanTicket(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Pending {
    first: u64,
    due: u64,
    /// An explicit request: later signals do not delay it.
    requested: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Flight {
    ticket: ScanTicket,
    /// Signals arrived after the scan started: scan again when it completes.
    deferred: bool,
}

/// What to do when a scan's strip read completed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadDone {
    /// Finish the scan and post the observation.
    pub emit: bool,
    /// Start another scan of the window at once (signals arrived meanwhile).
    pub restart: Option<ScanTicket>,
}

/// Per-window debounce and in-flight bookkeeping. Pure: time is passed in
/// (milliseconds on any monotonic clock).
#[derive(Clone, Debug)]
pub struct ScanScheduler {
    config: DebounceConfig,
    pending: BTreeMap<WinKey, Pending>,
    in_flight: BTreeMap<WinKey, Flight>,
    last_ticket: u64,
}

impl Default for ScanScheduler {
    fn default() -> Self {
        Self::new(DebounceConfig::default())
    }
}

impl ScanScheduler {
    pub fn new(config: DebounceConfig) -> Self {
        Self {
            config: DebounceConfig {
                quiet_ms: config.quiet_ms,
                max_delay_ms: config.max_delay_ms.max(config.quiet_ms),
            },
            pending: BTreeMap::new(),
            in_flight: BTreeMap::new(),
            last_ticket: 0,
        }
    }

    /// A signal for `win` at `now`: (re)starts its quiet time, bounded by the
    /// maximum delay after the burst's first signal.
    pub fn signal(&mut self, win: WinKey, now: u64) {
        let DebounceConfig {
            quiet_ms,
            max_delay_ms,
        } = self.config;
        let entry = self.pending.entry(win).or_insert(Pending {
            first: now,
            due: now.saturating_add(quiet_ms),
            requested: false,
        });
        if entry.requested {
            return;
        }
        let due = now
            .saturating_add(quiet_ms)
            .min(entry.first.saturating_add(max_delay_ms));
        // Never earlier than already scheduled (clocks that step back).
        entry.due = entry.due.max(due);
    }

    /// The earliest pending deadline.
    pub fn next_deadline(&self) -> Option<u64> {
        self.pending.values().map(|p| p.due).min()
    }

    /// An explicit request for `win` at `now` (the Engine's `RescanWindow`):
    /// due at once, without the quiet time. A scan in flight is followed by
    /// another one when it completes, as for a signal.
    pub fn request(&mut self, win: WinKey, now: u64) {
        let entry = self.pending.entry(win).or_insert(Pending { first: now, due: now, requested: true });
        entry.due = entry.due.min(now);
        entry.requested = true;
    }

    /// Windows whose deadline has passed, in deadline order: a scan starts for
    /// each one returned. A due window whose previous scan is still in flight
    /// is not returned; it is scanned again when that scan completes.
    pub fn poll(&mut self, now: u64) -> Vec<(WinKey, ScanTicket)> {
        let mut due: Vec<(u64, WinKey)> = self
            .pending
            .iter()
            .filter(|(_, p)| p.due <= now)
            .map(|(w, p)| (p.due, *w))
            .collect();
        due.sort_unstable();
        let mut started = Vec::with_capacity(due.len());
        for (_, win) in due {
            self.pending.remove(&win);
            match self.in_flight.get_mut(&win) {
                Some(flight) => flight.deferred = true,
                None => started.push((win, self.start(win))),
            }
        }
        started
    }

    fn start(&mut self, win: WinKey) -> ScanTicket {
        self.last_ticket += 1;
        let ticket = ScanTicket(self.last_ticket);
        self.in_flight.insert(
            win,
            Flight {
                ticket,
                deferred: false,
            },
        );
        ticket
    }

    /// The strip read of scan `ticket` of `win` completed.
    pub fn read_done(&mut self, win: WinKey, ticket: ScanTicket) -> ReadDone {
        match self.in_flight.get(&win) {
            Some(flight) if flight.ticket == ticket => {
                let deferred = flight.deferred;
                self.in_flight.remove(&win);
                ReadDone {
                    emit: true,
                    restart: deferred.then(|| self.start(win)),
                }
            }
            // Superseded or forgotten.
            _ => ReadDone {
                emit: false,
                restart: None,
            },
        }
    }

    /// Drops everything about `win` (destroyed); a result still in flight
    /// for it is ignored.
    pub fn forget(&mut self, win: WinKey) {
        self.pending.remove(&win);
        self.in_flight.remove(&win);
    }

    /// Whether `win` has a signal waiting or a scan in flight.
    pub fn is_busy(&self, win: WinKey) -> bool {
        self.pending.contains_key(&win) || self.in_flight.contains_key(&win)
    }

    /// No signal waiting and no scan in flight.
    pub fn is_idle(&self) -> bool {
        self.pending.is_empty() && self.in_flight.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Active tab (pure part)
// ---------------------------------------------------------------------------

/// One `ShellTabWindowClass` child of an Explorer window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TabChild {
    /// HWND value ([`hwnd_value`]).
    pub hwnd: u64,
    pub visible: bool,
}

/// The active tab hint: the topmost visible tab child (`children` in z-order,
/// topmost first), if it is one of the scanned `tabs`; `None` otherwise.
pub fn pick_active(children: &[TabChild], tabs: &[TabKey]) -> Option<TabKey> {
    let top = children.iter().find(|c| c.visible)?;
    tabs.iter().copied().find(|t| t.hwnd == top.hwnd)
}

/// The `ShellTabWindowClass` children of `top`, in z-order (topmost first).
pub fn tab_children(top: HWND) -> Vec<TabChild> {
    let mut out = Vec::new();
    // SAFETY: GetWindow tolerates stale HWNDs (it then fails and ends the walk).
    let mut child = unsafe { GetWindow(top, GW_CHILD) }.ok();
    let mut seen = 0;
    while let Some(c) = child.filter(|c| !c.is_invalid()) {
        seen += 1;
        if seen > MAX_CHILDREN {
            break;
        }
        if window_class_name(c) == SHELL_TAB_WINDOW_CLASS {
            out.push(TabChild {
                hwnd: hwnd_value(c),
                visible: is_window_visible(c),
            });
        }
        // SAFETY: as above.
        child = unsafe { GetWindow(c, GW_HWNDNEXT) }.ok();
    }
    out
}

/// The tab windows that belong to a window at a scan: its
/// `ShellTabWindowClass` children (which include a tab still registering) and
/// the windows of its scanned tabs.
pub fn tab_window_hwnds(children: &[TabChild], tabs: &[TabKey]) -> Vec<u64> {
    let mut all: Vec<u64> = children.iter().map(|c| c.hwnd).collect();
    for tab in tabs {
        if !all.contains(&tab.hwnd) {
            all.push(tab.hwnd);
        }
    }
    all
}

/// The active tab hint for `win` among `tabs` (see the module docs).
pub fn active_tab(win: WinKey, tabs: &[TabKey]) -> Option<TabKey> {
    pick_active(&tab_children(hwnd_of(win.hwnd)), tabs)
}

// ---------------------------------------------------------------------------
// Ports
// ---------------------------------------------------------------------------

/// The tabs of one window (backed by the `ShellWindowsSource`).
pub trait TabEnumerator {
    fn tabs_of(&self, win: WinKey) -> Result<ShellWindowTabs, ShellError>;
}

impl TabEnumerator for RefCell<ShellWindowsSource> {
    fn tabs_of(&self, win: WinKey) -> Result<ShellWindowTabs, ShellError> {
        self.try_borrow_mut()
            .map_err(|_| scan_error("the ShellWindowsSource is busy (re-entrant scan)"))?
            .tabs_of(win)
    }
}

/// The UIA tab strip (backed by the `TabStripReader`). Called from helper
/// threads, so it must be `Send + Sync`.
pub trait StripSource: Send + Sync {
    /// Tab names left to right and the selected index (may block ≤ 500 ms).
    fn read(&self, win: WinKey) -> Result<TabStripState, ShellError>;
    /// Subscribe to `StructureChanged` (must not block).
    fn watch(&self, _win: WinKey) {}
    fn unwatch(&self, _win: WinKey) {}
    /// Unsubscribe, keeping the cached strip (must not block).
    fn pause(&self, win: WinKey) {
        self.unwatch(win);
    }
}

impl StripSource for TabStripReader {
    fn read(&self, win: WinKey) -> Result<TabStripState, ShellError> {
        TabStripReader::read(self, win)
    }
    fn watch(&self, win: WinKey) {
        let _ = TabStripReader::watch(self, win);
    }
    fn unwatch(&self, win: WinKey) {
        TabStripReader::unwatch(self, win);
    }
    fn pause(&self, win: WinKey) {
        TabStripReader::pause(self, win);
    }
}

/// Receives `WindowScanned` events on the scanner's thread.
pub trait ScanHandler {
    fn post(&self, event: Event);
}

impl<F: Fn(Event)> ScanHandler for F {
    fn post(&self, event: Event) {
        self(event)
    }
}

// ---------------------------------------------------------------------------
// Requester (any thread)
// ---------------------------------------------------------------------------

/// Signals a [`WindowScanner`] from any thread (posts a message to it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanRequester {
    window: isize,
}

impl ScanRequester {
    /// Schedules a debounced rescan of `win`. `false` if the scanner is gone.
    pub fn window_changed(&self, win: WinKey) -> bool {
        // SAFETY: posting to a window of another thread; a destroyed window
        // just makes the call fail.
        unsafe {
            PostMessageW(
                Some(HWND(self.window as *mut c_void)),
                WM_SCAN_SIGNAL,
                WPARAM(win.hwnd as usize),
                LPARAM(win.seq as isize),
            )
        }
        .is_ok()
    }

    /// A `TabStripReader` change callback that rescans the window.
    pub fn change_callback(&self) -> ChangeCallback {
        let requester = *self;
        Arc::new(move |win| {
            requester.window_changed(win);
        })
    }
}

// ---------------------------------------------------------------------------
// The scanner
// ---------------------------------------------------------------------------

/// The window scanner. Bound to the thread that created it; see the module docs.
pub struct WindowScanner {
    inner: Rc<Inner>,
    window: HWND,
    /// `Box<Weak<Inner>>` stored in the window's `GWLP_USERDATA`.
    userdata: *mut Weak<Inner>,
}

struct Inner {
    keys: WindowKeys,
    tabs: Rc<dyn TabEnumerator>,
    strip: Option<Arc<dyn StripSource>>,
    handler: Rc<dyn ScanHandler>,
    /// Refreshed on every scan (task 25.3); see [`WindowScanner::set_tab_windows`].
    tab_windows: RefCell<Option<TabWindows>>,
    window: Cell<isize>,
    state: RefCell<State>,
    results: Arc<Mutex<Vec<StripResult>>>,
    /// A pass is running (re-entrant calls only set `again`).
    busy: Cell<bool>,
    again: Cell<bool>,
}

struct State {
    sched: ScanScheduler,
    /// Windows whose strip is watched.
    watched: HashSet<WinKey>,
    /// Windows whose strip must not be watched for now
    /// ([`WindowScanner::set_strip_watch`]).
    unwatched: HashSet<WinKey>,
    /// Deadline the timer is armed for.
    armed: Option<u64>,
}

struct StripResult {
    win: WinKey,
    ticket: ScanTicket,
    strip: Result<TabStripState, ShellError>,
}

impl WindowScanner {
    /// Creates the scanner's message-only window on the calling thread.
    ///
    /// `keys` is the shared window key registry; `tabs` lists a window's tabs
    /// (the `ShellWindowsSource`); `strip` reads the UIA tab strip (`None`
    /// scans without UIA); `handler` receives `WindowScanned`.
    pub fn new(
        config: DebounceConfig,
        keys: WindowKeys,
        tabs: Rc<dyn TabEnumerator>,
        strip: Option<Arc<dyn StripSource>>,
        handler: Rc<dyn ScanHandler>,
    ) -> Result<Self, ShellError> {
        let inner = Rc::new(Inner {
            keys,
            tabs,
            strip,
            handler,
            tab_windows: RefCell::new(None),
            window: Cell::new(0),
            state: RefCell::new(State {
                sched: ScanScheduler::new(config),
                watched: HashSet::new(),
                unwatched: HashSet::new(),
                armed: None,
            }),
            results: Arc::new(Mutex::new(Vec::new())),
            busy: Cell::new(false),
            again: Cell::new(false),
        });
        let window = create_window()?;
        let userdata = Box::into_raw(Box::new(Rc::downgrade(&inner)));
        // SAFETY: our own window; the pointer stays valid until `Drop` clears
        // it before freeing.
        unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, userdata as isize) };
        inner.window.set(window.0 as isize);
        Ok(Self {
            inner,
            window,
            userdata,
        })
    }

    /// The tab-to-window map to keep current (the `WindowEventSource`'s, from
    /// [`tab_windows`](crate::window_event_source::WindowEventSource::tab_windows)):
    /// every scan replaces the entries of the scanned window with its
    /// `ShellTabWindowClass` children, and a forgotten window loses them. The
    /// source needs it to name the window of a destroyed tab (`TabDetached`,
    /// Req 14.3 b), because the parent of a destroyed child cannot be read.
    pub fn set_tab_windows(&self, tabs: TabWindows) {
        *self.inner.tab_windows.borrow_mut() = Some(tabs);
    }

    /// A handle that signals this scanner from any thread.
    pub fn requester(&self) -> ScanRequester {
        ScanRequester {
            window: self.window.0 as isize,
        }
    }

    /// Schedules a debounced rescan of `win` (on the scanner's thread).
    pub fn window_changed(&self, win: WinKey) {
        self.inner.window_changed(win);
    }

    /// Scans `win` as soon as possible, without the quiet time (an explicit
    /// request rather than a change signal).
    pub fn scan_now(&self, win: WinKey) {
        crate::diag::note(|| format!("scan: request {:#x}", win.hwnd));
        self.inner.state.borrow_mut().sched.request(win, now_ms());
        self.inner.run();
    }

    /// Forgets `win` (destroyed): drops its pending scan and unwatches its strip.
    pub fn forget(&self, win: WinKey) {
        self.inner.forget(win);
    }

    /// Drops `win`'s pending scan; the result of a read already in flight is
    /// ignored. Unlike [`forget`](Self::forget) the strip stays watched.
    pub fn cancel(&self, win: WinKey) {
        crate::diag::note(|| format!("scan: cancel {:#x}", win.hwnd));
        self.inner.state.borrow_mut().sched.forget(win);
    }

    /// `false`: stop watching `win`'s strip (`StructureChanged`) and do not
    /// watch it at its next scans (the strip stays cached); `true`: watch it
    /// again from its next scan. A restoration turns the watch off for the
    /// windows it builds (task 16.3).
    pub fn set_strip_watch(&self, win: WinKey, on: bool) {
        crate::diag::note(|| format!("scan: strip watch {:#x} {on}", win.hwnd));
        let pause = {
            let mut st = self.inner.state.borrow_mut();
            if on {
                st.unwatched.remove(&win);
                false
            } else {
                st.unwatched.insert(win);
                st.watched.remove(&win)
            }
        };
        if pause && let Some(strip) = &self.inner.strip {
            strip.pause(win);
        }
    }

    /// Whether `win` has a scan waiting or in flight.
    pub fn is_busy(&self, win: WinKey) -> bool {
        self.inner.state.borrow().sched.is_busy(win)
    }

    /// No scan waiting or in flight.
    pub fn is_idle(&self) -> bool {
        self.inner.state.borrow().sched.is_idle()
    }
}

impl Drop for WindowScanner {
    fn drop(&mut self) {
        // SAFETY: our own window, destroyed on its thread; the user data is
        // cleared first so no message reaches the freed pointer.
        unsafe {
            SetWindowLongPtrW(self.window, GWLP_USERDATA, 0);
            let _ = DestroyWindow(self.window);
            drop(Box::from_raw(self.userdata));
        }
        let watched: Vec<WinKey> = self.inner.state.borrow_mut().watched.drain().collect();
        if let Some(strip) = &self.inner.strip {
            for win in watched {
                strip.unwatch(win);
            }
        }
    }
}

impl Inner {
    fn hwnd(&self) -> HWND {
        HWND(self.window.get() as *mut c_void)
    }

    fn window_changed(&self, win: WinKey) {
        crate::diag::note(|| format!("scan: signal {:#x}", win.hwnd));
        self.state.borrow_mut().sched.signal(win, now_ms());
        self.run();
    }

    fn forget(&self, win: WinKey) {
        if let Some(tabs) = self.tab_windows.borrow().as_ref() {
            tabs.forget_window(win.hwnd);
        }
        let watched = {
            let mut st = self.state.borrow_mut();
            st.sched.forget(win);
            st.unwatched.remove(&win);
            st.watched.remove(&win)
        };
        if let Some(strip) = self.strip.as_ref().filter(|_| watched) {
            strip.unwatch(win);
        }
    }

    /// Processes completed reads and due windows until nothing is left, then
    /// re-arms the timer. Re-entrant calls are folded into the running pass.
    fn run(&self) {
        if self.busy.replace(true) {
            self.again.set(true);
            return;
        }
        loop {
            self.again.set(false);
            self.process_results();
            self.process_due();
            if !self.again.get() {
                break;
            }
        }
        self.arm_timer();
        self.busy.set(false);
    }

    fn process_due(&self) {
        let started = self.state.borrow_mut().sched.poll(now_ms());
        for (win, ticket) in started {
            crate::diag::note(|| format!("scan: start {:#x}", win.hwnd));
            self.start_scan(win, ticket);
        }
    }

    fn start_scan(&self, win: WinKey, ticket: ScanTicket) {
        let Some(strip) = self.strip.clone() else {
            return self.push_result(StripResult {
                win,
                ticket,
                strip: Err(scan_error("no tab strip reader")),
            });
        };
        let first = {
            let mut st = self.state.borrow_mut();
            !st.unwatched.contains(&win) && st.watched.insert(win)
        };
        if first {
            strip.watch(win);
        }
        let results = Arc::clone(&self.results);
        let window = self.window.get();
        let spawned = std::thread::Builder::new()
            .name("esr-scan-strip".into())
            .stack_size(READ_THREAD_STACK)
            .spawn(move || {
                let t = std::time::Instant::now();
                let result = strip.read(win);
                crate::diag::note(|| {
                    format!(
                        "scan: strip read {:#x} {:.1} ms {}",
                        win.hwnd,
                        t.elapsed().as_secs_f64() * 1000.0,
                        match &result {
                            Ok(s) => format!(
                                "ok {} names, selected {:?}{}",
                                s.names.len(),
                                s.selected,
                                if s.names.len() <= 12 {
                                    format!(" {:?}", s.names.iter().map(|n| n.to_string_lossy()).collect::<Vec<_>>())
                                } else {
                                    String::new()
                                }
                            ),
                            Err(e) => format!("error {}", e.message),
                        }
                    )
                });
                deliver(
                    &results,
                    window,
                    StripResult {
                        win,
                        ticket,
                        strip: result,
                    },
                );
            });
        if let Err(e) = spawned {
            self.push_result(StripResult {
                win,
                ticket,
                strip: Err(scan_error(&format!("strip read thread: {e}"))),
            });
        }
    }

    /// Queues a result produced on this thread for the current pass.
    fn push_result(&self, result: StripResult) {
        self.results
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(result);
        self.again.set(true);
    }

    fn process_results(&self) {
        let results: Vec<StripResult> =
            std::mem::take(&mut *self.results.lock().unwrap_or_else(PoisonError::into_inner));
        for StripResult { win, ticket, strip } in results {
            let done = self.state.borrow_mut().sched.read_done(win, ticket);
            if done.emit {
                self.scan(win, strip.ok());
            }
            if let Some(ticket) = done.restart {
                self.start_scan(win, ticket);
            }
        }
    }

    /// Steps 2–5 of a scan (see the module docs).
    fn scan(&self, win: WinKey, strip: Option<TabStripState>) {
        let start = std::time::Instant::now();
        self.scan_inner(win, strip);
        crate::diag::note(|| format!("scan: done {:#x} {:.1} ms", win.hwnd, start.elapsed().as_secs_f64() * 1000.0));
    }

    fn scan_inner(&self, win: WinKey, strip: Option<TabStripState>) {
        let hwnd = hwnd_of(win.hwnd);
        if self.keys.get(win.hwnd) != Some(win) || !is_cabinet_window(hwnd) {
            self.forget(win);
            return;
        }
        let Ok(found) = self.tabs.tabs_of(win) else {
            return;
        };
        let Ok(placement) = window_placement(hwnd) else {
            return;
        };
        let keys: Vec<TabKey> = found.tabs.iter().map(|(t, _)| *t).collect();
        let children = tab_children(hwnd);
        let tab_windows = self.tab_windows.borrow().clone();
        if let Some(tabs) = tab_windows {
            tabs.refresh(win.hwnd, tab_window_hwnds(&children, &keys));
        }
        let active = found
            .active
            .filter(|a| keys.contains(a))
            .or_else(|| pick_active(&children, &keys));
        let obs = WindowObservation {
            win,
            pid: found.pid,
            tabs: found
                .tabs
                .into_iter()
                .map(|(tab, location)| TabObservation { tab, location })
                .collect(),
            active,
            strip,
            placement,
        };
        self.handler.post(Event::WindowScanned(obs));
    }

    fn arm_timer(&self) {
        let window = self.hwnd();
        let mut st = self.state.borrow_mut();
        let next = st.sched.next_deadline();
        if next == st.armed {
            return;
        }
        st.armed = next;
        drop(st);
        match next {
            Some(due) => {
                let delay = due.saturating_sub(now_ms()).clamp(1, u64::from(u32::MAX));
                // SAFETY: our own window; re-setting the same id replaces the timer.
                let id = unsafe { SetTimer(Some(window), TIMER_DEBOUNCE, delay as u32, None) };
                if id == 0 {
                    // No timer: try again at the next signal or result.
                    self.state.borrow_mut().armed = None;
                }
            }
            None => {
                // SAFETY: our own window's timer (absent is fine).
                let _ = unsafe { KillTimer(Some(window), TIMER_DEBOUNCE) };
            }
        }
    }

    fn timer_fired(&self) {
        self.state.borrow_mut().armed = None;
        self.run();
    }
}

/// Hands a helper thread's result to the scanner's thread.
fn deliver(results: &Mutex<Vec<StripResult>>, window: isize, result: StripResult) {
    results
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(result);
    // SAFETY: posting to the scanner's window; a destroyed window just fails.
    let _ = unsafe {
        PostMessageW(
            Some(HWND(window as *mut c_void)),
            WM_STRIP_DONE,
            WPARAM(0),
            LPARAM(0),
        )
    };
}

fn now_ms() -> u64 {
    // SAFETY: no preconditions.
    unsafe { GetTickCount64() }
}

fn scan_error(message: &str) -> ShellError {
    ShellError {
        code: 0,
        message: format!("window scanner: {message}"),
    }
}

fn create_window() -> Result<HWND, ShellError> {
    // SAFETY: standard class registration (a repeat registration fails
    // harmlessly) and creation of a message-only window owned by this thread.
    unsafe {
        let module = GetModuleHandleW(None).map_err(|e| ShellError {
            code: e.code().0,
            message: format!("window scanner: GetModuleHandleW: {}", e.message()),
        })?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: module.into(),
            lpszClassName: WINDOW_CLASS,
            ..Default::default()
        };
        RegisterClassW(&class);
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            WINDOW_CLASS,
            PCWSTR::null(),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(module.into()),
            None,
        )
        .map_err(|e| ShellError {
            code: e.code().0,
            message: format!("window scanner: CreateWindowExW: {}", e.message()),
        })
    }
}

/// The scanner owning `hwnd`, if it is still alive.
fn scanner_of(hwnd: HWND) -> Option<Rc<Inner>> {
    // SAFETY: our own window; the user data is null or a live `Box<Weak<Inner>>`.
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const Weak<Inner>;
    // SAFETY: see above.
    unsafe { ptr.as_ref() }.and_then(Weak::upgrade)
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_SCAN_SIGNAL => {
            if let Some(inner) = scanner_of(hwnd) {
                inner.window_changed(WinKey {
                    hwnd: wparam.0 as u64,
                    seq: lparam.0 as u64,
                });
            }
            LRESULT(0)
        }
        WM_STRIP_DONE => {
            if let Some(inner) = scanner_of(hwnd) {
                inner.run();
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_DEBOUNCE => {
            // SAFETY: our own window's timer; re-armed by the pass if needed.
            let _ = unsafe { KillTimer(Some(hwnd), TIMER_DEBOUNCE) };
            if let Some(inner) = scanner_of(hwnd) {
                inner.timer_fired();
            }
            LRESULT(0)
        }
        // SAFETY: default processing for our own window.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(n: u64) -> WinKey {
        WinKey { hwnd: n, seq: 1 }
    }

    fn sched() -> ScanScheduler {
        ScanScheduler::new(DebounceConfig {
            quiet_ms: 100,
            max_delay_ms: 400,
        })
    }

    fn wins(started: &[(WinKey, ScanTicket)]) -> Vec<WinKey> {
        started.iter().map(|(w, _)| *w).collect()
    }

    #[test]
    fn a_single_signal_is_scanned_after_the_quiet_time() {
        let mut s = sched();
        s.signal(win(1), 1000);
        assert_eq!(s.next_deadline(), Some(1100));
        assert!(s.poll(1099).is_empty());
        assert_eq!(wins(&s.poll(1100)), vec![win(1)]);
        assert_eq!(s.next_deadline(), None);
        assert!(s.is_busy(win(1)), "in flight until the read completes");
    }

    #[test]
    fn a_request_is_due_at_once_and_follows_a_scan_in_flight() {
        let mut s = sched();
        // Pulls a pending signal's deadline forward.
        s.signal(win(1), 1000);
        s.request(win(1), 1010);
        assert_eq!(s.next_deadline(), Some(1010));
        let started = s.poll(1010);
        assert_eq!(wins(&started), vec![win(1)]);
        // While in flight: scanned again once the read completes.
        s.request(win(1), 1020);
        assert!(s.poll(1020).is_empty());
        let done = s.read_done(win(1), started[0].1);
        assert!(done.emit && done.restart.is_some());
        // A later signal never delays a pending request.
        s.request(win(2), 2000);
        s.signal(win(2), 2001);
        assert_eq!(s.next_deadline(), Some(2000));
    }

    #[test]
    fn signals_within_the_quiet_time_collapse_into_one_scan() {
        let mut s = sched();
        for t in [1000, 1030, 1060, 1090] {
            s.signal(win(1), t);
        }
        assert_eq!(s.next_deadline(), Some(1190));
        assert!(s.poll(1189).is_empty());
        let started = s.poll(1190);
        assert_eq!(wins(&started), vec![win(1)]);
        assert!(s.poll(5000).is_empty());
    }

    #[test]
    fn continuous_signals_are_scanned_by_the_maximum_delay() {
        let mut s = sched();
        let mut t = 1000;
        let mut scans = Vec::new();
        while t <= 2000 {
            s.signal(win(1), t);
            for (w, ticket) in s.poll(t) {
                scans.push(t);
                assert!(s.read_done(w, ticket).emit);
            }
            t += 20;
        }
        // Every 400 ms despite a signal every 20 ms.
        assert_eq!(scans, vec![1400, 1820]);
    }

    #[test]
    fn windows_are_debounced_independently_and_started_in_deadline_order() {
        let mut s = sched();
        s.signal(win(2), 1000);
        s.signal(win(1), 1050);
        s.signal(win(2), 1080);
        assert_eq!(s.next_deadline(), Some(1150));
        assert_eq!(wins(&s.poll(1150)), vec![win(1)]);
        assert_eq!(s.next_deadline(), Some(1180));
        s.signal(win(3), 1160);
        assert_eq!(wins(&s.poll(1300)), vec![win(2), win(3)]);
    }

    #[test]
    fn signals_during_a_scan_rescan_once_it_completes() {
        let mut s = sched();
        s.signal(win(1), 0);
        let [(w, first)] = s.poll(100)[..] else {
            panic!()
        };
        // Signals while the read is in flight: debounced, then deferred.
        s.signal(win(1), 120);
        s.signal(win(1), 150);
        assert!(s.poll(250).is_empty());
        assert_eq!(s.next_deadline(), None);
        let done = s.read_done(w, first);
        assert!(done.emit);
        let second = done.restart.expect("deferred signals restart the scan");
        assert_ne!(second, first);
        let done = s.read_done(w, second);
        assert_eq!(
            done,
            ReadDone {
                emit: true,
                restart: None
            }
        );
        assert!(s.is_idle());
    }

    #[test]
    fn a_signal_before_the_deadline_of_an_in_flight_rescan_waits_for_it() {
        let mut s = sched();
        s.signal(win(1), 0);
        let [(w, t1)] = s.poll(100)[..] else { panic!() };
        s.signal(win(1), 110);
        // Read completes before the new signal is due: no restart, the new
        // signal scans at its own deadline.
        let done = s.read_done(w, t1);
        assert_eq!(done.restart, None);
        assert_eq!(s.next_deadline(), Some(210));
        assert_eq!(wins(&s.poll(210)), vec![win(1)]);
    }

    #[test]
    fn a_forgotten_window_ignores_its_late_result() {
        let mut s = sched();
        s.signal(win(1), 0);
        let [(w, t1)] = s.poll(100)[..] else { panic!() };
        s.forget(w);
        assert!(s.is_idle());
        // A new burst for the same key starts a new scan; the stale result
        // of the first does not complete it.
        s.signal(w, 200);
        let [(_, t2)] = s.poll(300)[..] else { panic!() };
        assert_eq!(
            s.read_done(w, t1),
            ReadDone {
                emit: false,
                restart: None
            }
        );
        assert!(s.is_busy(w));
        assert!(s.read_done(w, t2).emit);
        assert!(s.is_idle());
    }

    #[test]
    fn forgetting_drops_a_pending_signal() {
        let mut s = sched();
        s.signal(win(1), 0);
        s.signal(win(2), 0);
        s.forget(win(1));
        assert_eq!(wins(&s.poll(100)), vec![win(2)]);
    }

    #[test]
    fn a_clock_step_back_never_moves_a_deadline_earlier() {
        let mut s = sched();
        s.signal(win(1), 1000);
        s.signal(win(1), 900);
        assert_eq!(s.next_deadline(), Some(1100));
    }

    #[test]
    fn a_maximum_delay_below_the_quiet_time_is_the_quiet_time() {
        let mut s = ScanScheduler::new(DebounceConfig {
            quiet_ms: 100,
            max_delay_ms: 10,
        });
        s.signal(win(1), 0);
        s.signal(win(1), 50);
        assert_eq!(s.next_deadline(), Some(100));
    }

    fn key(hwnd: u64) -> TabKey {
        TabKey { hwnd, seq: 7 }
    }

    fn child(hwnd: u64, visible: bool) -> TabChild {
        TabChild { hwnd, visible }
    }

    #[test]
    fn the_topmost_visible_tab_child_is_active() {
        let tabs = [key(1), key(2), key(3)];
        // 22621: every child visible, the active one raised to the top.
        let all_visible = [child(2, true), child(3, true), child(1, true)];
        assert_eq!(pick_active(&all_visible, &tabs), Some(key(2)));
        // Builds that hide inactive tabs: the only visible one.
        let one_visible = [child(1, false), child(3, true), child(2, false)];
        assert_eq!(pick_active(&one_visible, &tabs), Some(key(3)));
    }

    #[test]
    fn no_active_hint_when_ambiguous() {
        let tabs = [key(1), key(2)];
        assert_eq!(pick_active(&[], &tabs), None);
        assert_eq!(
            pick_active(&[child(1, false), child(2, false)], &tabs),
            None
        );
        // The top child is not a scanned tab (still registering, being dragged).
        assert_eq!(pick_active(&[child(9, true), child(1, true)], &tabs), None);
        assert_eq!(pick_active(&[child(1, true)], &[]), None);
    }

    #[test]
    fn requester_signal_round_trips_a_win_key() {
        let k = WinKey {
            hwnd: 0x7FFF_1234,
            seq: u64::MAX / 3,
        };
        let (w, l) = (WPARAM(k.hwnd as usize), LPARAM(k.seq as isize));
        assert_eq!(
            WinKey {
                hwnd: w.0 as u64,
                seq: l.0 as u64
            },
            k
        );
    }
}
