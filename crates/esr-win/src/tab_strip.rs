//! `TabStripReader`: UIA tab order, selection, and `StructureChanged` on the
//! MTA worker.
//!
//! The XAML tab strip of a File Explorer window is reachable only through UI
//! Automation. Its tabs are the `TabItem` children with class `ListViewItem`
//! of one element (the strip); the Home page also exposes `TabItem`s with
//! class `NavigationViewItem`, which are not tabs. UIA is the only source of
//! the visual tab order and of the active tab (`SelectionItemPattern`), and the
//! only practical way to activate a tab (design.md, "Spike outcomes").
//!
//! # Threading
//!
//! Every UIA call runs on a dedicated MTA worker thread (UIA client calls
//! are never made from a UI thread); there are [`UIA_WORKERS`] of them, and
//! all jobs of one window run on the same one, in order. [`TabStripReader`]
//! is a handle that queues jobs for the workers; it is `Send + Sync` and may
//! be used from any thread.
//!
//! * [`read`](TabStripReader::read) and [`select`](TabStripReader::select)
//!   wait for their result for at most [`READ_TIMEOUT`] / [`SELECT_TIMEOUT`]
//!   (500 ms, design.md "Error Handling"). A timed-out `select` that the worker
//!   has not started yet is skipped, so a stale activation never happens
//!   later. A timed-out `read` still runs (it warms the strip cache); if it
//!   then succeeds, the change callback is called for the window so the
//!   scanner rescans it with the now cached strip.
//! * [`activate`](TabStripReader::activate) is the non-blocking `select` the
//!   restoration uses (`ActivateTab`): its outcome is handed to a callback,
//!   and a success also calls the change callback (the confirming rescan).
//!   A success is reported only after the tab was found still selected
//!   [`ACTIVATE_VERIFY_DELAY`] after the `Select`, and the worker checks once
//!   more [`ACTIVATE_LATE_CHECK`] later; a tab that lost its selection is
//!   selected again (up to [`ACTIVATE_RESELECTS`] times). Explorer selects a
//!   tab it has just created from a deferred XAML callback, so an activation
//!   made right after a window's last tab appeared was sometimes overridden
//!   by that tab (task 16.3: 4 of ~55 3x10 runs ended with the last created
//!   tab active). The checks run between the worker's other jobs (a pending
//!   check does not hold up reads).
//! * Selecting waits for the strip to catch up with Explorer: the strip lags
//!   behind tab creation, so the tab at visual position `index` is selected
//!   only once the strip has as many items as the window has tabs
//!   (re-read with backoff for up to [`ACTIVATE_CATCH_UP`]). Items are taken
//!   from the strip's `ItemContainerPattern`, which also covers the items an
//!   overflowing strip virtualises; such an item is realized and scrolled
//!   into view first ([`crate::uia_strip`]).
//! * [`watch`](TabStripReader::watch) and [`unwatch`](TabStripReader::unwatch)
//!   only queue the job. A successful subscription calls the change callback
//!   once (the strip has become readable). If the strip cannot be found yet
//!   (a window that is still being created), the subscription is made the next
//!   time the strip is found by a `read` or `select`.
//!
//! # Strip cache
//!
//! Finding the strip is a descendant search. From `ElementFromHandle(cabinet)`
//! it walks into every tab's file view and costs 0.8-2.4 s (spike R9); it is
//! therefore made under the window's XAML island child first, where it takes
//! about 1 ms ([`crate::uia_strip::search_roots`]). Reading the cached
//! strip's children costs 2-7 ms. The strip element is found once per window
//! and cached. A cached strip that fails or returns no
//! tab items is dropped and searched again (re-subscribing `StructureChanged`
//! on the new element). `ElementFromHandle` is retried while it reports the
//! transient `UIA_E_ELEMENTNOTAVAILABLE` (`0x80040201`). Entries of windows
//! that no longer exist, or whose HWND was reused by a newer [`WinKey`], are
//! pruned whenever a strip is searched.
//!
//! # Overflowing strips
//!
//! A strip with more tabs than fit virtualises its items: UIA then exposes
//! only some of them (18-24 of 30 in R9). [`TabStripState::names`] holds
//! exactly the exposed items, in order, and `selected` indexes into that list
//! (`None` when the selected tab is not exposed). A strip is therefore
//! incomplete exactly when `names.len()` is smaller than the window's
//! ShellWindows tab count; the `TabOrderReconciler` keeps the previous
//! relative order of tabs without an exposed item. The `index` of
//! [`select`](TabStripReader::select) is a visual position among all tabs
//! (see above), not a position in the exposed list.
//!
//! # `StructureChanged`
//!
//! The handler is registered on the strip element with scope element +
//! children, which covers tabs being added (`ChildAdded` is raised on the new
//! item), removed and reordered (raised on the strip), including drag-in and
//! drag-out (Req 2.4). UIA calls the handler on its own threads; the handler
//! only calls the change callback (`EventSink::window_changed`, which the
//! scanner debounces).
//!
//! Requirements 2.1, 2.2, 2.4, 5.4.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use esr_core::engine::{ShellError, TabStripState, WinKey};
use esr_core::model::WideString;
use windows::Win32::Foundation::E_INVALIDARG;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
    SAFEARRAY,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::{
    CUIAutomation, CUIAutomation8, IUIAutomation, IUIAutomation2, IUIAutomationCacheRequest,
    IUIAutomationCondition, IUIAutomationElement,
    IUIAutomationStructureChangedEventHandler, IUIAutomationStructureChangedEventHandler_Impl,
    IUIAutomationTreeWalker, StructureChangeType, TreeScope, TreeScope_Children,
    TreeScope_Element, UIA_ClassNamePropertyId, UIA_ControlTypePropertyId,
    UIA_NamePropertyId, UIA_SelectionItemIsSelectedPropertyId, UIA_TabItemControlTypeId,
};
use windows::Win32::UI::WindowsAndMessaging::IsWindow;
use windows::core::{BSTR, Interface, Ref, implement};

use crate::keys::hwnd_of;
use crate::uia_strip;

/// How long [`TabStripReader::read`] waits for the worker (design.md, Error Handling).
pub const READ_TIMEOUT: Duration = Duration::from_millis(500);
/// How long [`TabStripReader::select`] waits for the worker: the strip
/// catch-up wait plus the 500 ms UIA budget of design.md.
pub const SELECT_TIMEOUT: Duration = Duration::from_millis(2000);
/// UIA transaction timeout of the worker's client: bounds how long one UIA
/// call can block the worker when Explorer hangs (the UIA default is 20 s).
/// It must exceed the slowest strip search (about 2.4 s for 30 tabs).
pub const UIA_TRANSACTION_TIMEOUT_MS: u32 = 5000;
/// UIA class name of a tab strip item.
pub const TAB_ITEM_CLASS: &str = "ListViewItem";
/// Class name of the Home page's navigation items, which are `TabItem`s too.
pub const NAVIGATION_ITEM_CLASS: &str = "NavigationViewItem";
/// How long an activation waits for the strip to catch up with Explorer's
/// tab count (it lags behind tab creation, see [`crate::uia_strip`]) before
/// it fails.
pub const ACTIVATE_CATCH_UP: Duration = Duration::from_millis(1500);
/// How long after an activation's `Select` the worker checks that the tab is
/// still selected before it reports success (see [`TabStripReader::activate`]).
pub const ACTIVATE_VERIFY_DELAY: Duration = Duration::from_millis(100);
/// How long after the confirmed check the worker checks once more (and
/// re-selects if needed): Explorer can override a selection a little later.
pub const ACTIVATE_LATE_CHECK: Duration = Duration::from_millis(500);
/// How often one activation re-selects a tab that lost its selection.
pub const ACTIVATE_RESELECTS: u32 = 3;

/// Re-selections so far in this process: (at the first check, at a later
/// check). Diagnostics for the timing tests.
static RESELECTS: [std::sync::atomic::AtomicU32; 2] =
    [std::sync::atomic::AtomicU32::new(0), std::sync::atomic::AtomicU32::new(0)];

/// Re-selections made by activations in this process so far: (at the
/// confirming check, at a later check).
pub fn reselect_counts() -> (u32, u32) {
    use std::sync::atomic::Ordering::Relaxed;
    (RESELECTS[0].load(Relaxed), RESELECTS[1].load(Relaxed))
}

/// Called with a window whose tab strip changed (or became readable). Runs on
/// the UIA worker or on a UIA event thread, so it must be cheap and must not
/// call back into the reader synchronously.
pub type ChangeCallback = Arc<dyn Fn(WinKey) + Send + Sync>;

/// Reader timeouts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TabStripConfig {
    pub read_timeout: Duration,
    pub select_timeout: Duration,
}

impl Default for TabStripConfig {
    fn default() -> Self {
        Self {
            read_timeout: READ_TIMEOUT,
            select_timeout: SELECT_TIMEOUT,
        }
    }
}

/// Whether a `TabItem`'s UIA class name denotes a tab strip item.
pub fn is_tab_item_class(class: &str) -> bool {
    class == TAB_ITEM_CLASS
}

/// UIA workers of a reader started with [`TabStripReader::start`]. Each
/// window's jobs go to one worker (assigned round-robin when the window is
/// first seen), so jobs of one window stay in order while the windows of a
/// restoration are read and activated in parallel: a UIA call waits while
/// the window's thread is busy, ~0.1-0.5 s per read while Explorer builds
/// tabs, and one worker made the windows wait for each other (task 16.3).
pub const UIA_WORKERS: usize = 3;

/// The `TabStripReader` adapter (backs the `TabStrip` port in `esr-app`).
pub struct TabStripReader {
    workers: Vec<Worker>,
    /// Worker index per window HWND value.
    route: Mutex<Route>,
    config: TabStripConfig,
}

struct Worker {
    jobs: Sender<Job>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct Route {
    by_window: HashMap<u64, usize>,
    next: usize,
}

/// Entries of the route table above which those of destroyed windows are
/// dropped.
const ROUTE_PRUNE_AT: usize = 64;

impl TabStripReader {
    /// Starts the UIA worker with the default timeouts. `on_change` receives
    /// `StructureChanged` notifications and late read completions.
    pub fn start(on_change: ChangeCallback) -> Result<Self, ShellError> {
        Self::with_config(TabStripConfig::default(), on_change)
    }

    pub fn with_config(
        config: TabStripConfig,
        on_change: ChangeCallback,
    ) -> Result<Self, ShellError> {
        let mut workers = Vec::with_capacity(UIA_WORKERS);
        for _ in 0..UIA_WORKERS {
            // Workers already started stop when `workers` is dropped.
            workers.push(Self::spawn_worker(on_change.clone(), UiaBackend::new)?);
        }
        Ok(Self { workers, route: Mutex::default(), config })
    }

    /// Starts a reader with one worker whose backend is created on the
    /// worker thread.
    #[cfg(test)]
    fn spawn<B, F>(
        config: TabStripConfig,
        on_change: ChangeCallback,
        make: F,
    ) -> Result<Self, ShellError>
    where
        B: Backend,
        F: FnOnce(ChangeCallback) -> Result<B, ShellError> + Send + 'static,
    {
        let worker = Self::spawn_worker(on_change, make)?;
        Ok(Self { workers: vec![worker], route: Mutex::default(), config })
    }

    /// Starts one worker thread whose backend is created on that thread.
    fn spawn_worker<B, F>(on_change: ChangeCallback, make: F) -> Result<Worker, ShellError>
    where
        B: Backend,
        F: FnOnce(ChangeCallback) -> Result<B, ShellError> + Send + 'static,
    {
        let (jobs, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("esr-uia".into())
            .spawn(move || {
                let _mta = MtaGuard::init();
                match make(on_change.clone()) {
                    Ok(backend) => {
                        let _ = ready_tx.send(Ok(()));
                        run_worker(backend, &rx, &on_change);
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                }
            })
            .map_err(|e| ShellError {
                code: e.raw_os_error().unwrap_or(0),
                message: format!("UIA worker: spawn failed: {e}"),
            })?;
        let ready = ready_rx.recv().unwrap_or_else(|_| {
            Err(worker_error("the UIA worker stopped during start-up"))
        });
        match ready {
            Ok(()) => Ok(Worker {
                jobs,
                thread: Some(worker),
            }),
            Err(e) => {
                let _ = worker.join();
                Err(e)
            }
        }
    }

    /// `TabItem` names left to right (the exposed items), and the selected index.
    pub fn read(&self, win: WinKey) -> Result<TabStripState, ShellError> {
        if crate::diag::disabled(crate::diag::Switch::UiaReader) {
            return Err(worker_error("UIA reads disabled (diagnostics)"));
        }
        self.request(win, "read", self.config.read_timeout, |slot| Job::Read { win, slot })
    }

    /// Selects the tab at visual position `index` (`SelectionItemPattern.Select`
    /// on every item, realized or not), waiting up to [`ACTIVATE_CATCH_UP`]
    /// for the strip to show Explorer's tab count. Blocks the caller for at
    /// most `select_timeout`; prefer [`activate`](Self::activate).
    pub fn select(&self, win: WinKey, index: usize) -> Result<(), ShellError> {
        self.request(win, "select", self.config.select_timeout, |slot| Job::Select {
            win,
            index,
            slot,
        })
    }

    /// Like [`select`](Self::select), without blocking: `done` is called on
    /// the UIA worker with the outcome. After a successful activation the
    /// change callback is called for `win` too (the scan that confirms it).
    /// Fails only if the worker has stopped.
    pub fn activate(
        &self,
        win: WinKey,
        index: usize,
        done: Box<dyn FnOnce(Result<(), ShellError>) + Send>,
    ) -> Result<(), ShellError> {
        self.send(win, Job::Activate { win, index, done })
    }

    /// Subscribes to `StructureChanged` on `win`'s tab strip (asynchronously;
    /// see the module docs). Fails only if the worker has stopped.
    pub fn watch(&self, win: WinKey) -> Result<(), ShellError> {
        if crate::diag::disabled(crate::diag::Switch::UiaReader)
            || crate::diag::disabled(crate::diag::Switch::UiaWatch)
        {
            return Ok(());
        }
        self.send(win, Job::Watch { win })
    }

    /// Removes the subscription and forgets `win`'s cached strip.
    pub fn unwatch(&self, win: WinKey) {
        let _ = self.send(win, Job::Unwatch { win });
        self.route.lock().unwrap_or_else(PoisonError::into_inner).by_window.remove(&win.hwnd);
    }

    /// Removes the subscription but keeps `win`'s cached strip (a later
    /// [`watch`](Self::watch) subscribes again). The `StructureChanged`
    /// subscription makes Explorer raise UIA events for every change of the
    /// strip, which slowed the building of restored windows down (task 16.3).
    pub fn pause(&self, win: WinKey) {
        let _ = self.send(win, Job::Pause { win });
    }

    /// The worker that runs `win`'s jobs.
    fn worker_of(&self, win: WinKey) -> &Worker {
        let n = self.workers.len();
        let index = if n == 1 {
            0
        } else {
            let mut route = self.route.lock().unwrap_or_else(PoisonError::into_inner);
            if route.by_window.len() >= ROUTE_PRUNE_AT {
                route.by_window.retain(|&hwnd, _| crate::shell_util::is_cabinet_window(hwnd_of(hwnd)));
            }
            match route.by_window.get(&win.hwnd) {
                Some(&i) => i,
                None => {
                    let i = route.next % n;
                    route.next = route.next.wrapping_add(1);
                    route.by_window.insert(win.hwnd, i);
                    i
                }
            }
        };
        &self.workers[index]
    }

    fn send(&self, win: WinKey, job: Job) -> Result<(), ShellError> {
        self.worker_of(win)
            .jobs
            .send(job)
            .map_err(|_| worker_error("the UIA worker has stopped"))
    }

    fn request<T>(
        &self,
        win: WinKey,
        op: &str,
        timeout: Duration,
        job: impl FnOnce(Arc<Slot<Result<T, ShellError>>>) -> Job,
    ) -> Result<T, ShellError> {
        let slot = Arc::new(Slot::new());
        self.send(win, job(Arc::clone(&slot)))?;
        slot.wait(timeout).unwrap_or_else(|| {
            Err(ShellError {
                code: 0,
                message: format!("UIA {op}: timed out after {} ms", timeout.as_millis()),
            })
        })
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.jobs.send(Job::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn worker_error(message: &str) -> ShellError {
    ShellError {
        code: 0,
        message: format!("UIA worker: {message}"),
    }
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

type ReadSlot = Arc<Slot<Result<TabStripState, ShellError>>>;
type SelectSlot = Arc<Slot<Result<(), ShellError>>>;

enum Job {
    Read { win: WinKey, slot: ReadSlot },
    Select { win: WinKey, index: usize, slot: SelectSlot },
    Activate { win: WinKey, index: usize, done: ActivateDone },
    Watch { win: WinKey },
    Unwatch { win: WinKey },
    Pause { win: WinKey },
    Shutdown,
}

type ActivateDone = Box<dyn FnOnce(Result<(), ShellError>) + Send>;

/// The UIA operations, behind a trait so the worker's queueing and timeout
/// rules can be tested without Explorer.
trait Backend {
    fn read(&mut self, win: WinKey) -> Result<TabStripState, ShellError>;
    fn select(&mut self, win: WinKey, index: usize) -> Result<(), ShellError>;
    /// Whether the tab at visual position `index` is selected.
    fn is_selected(&mut self, win: WinKey, index: usize) -> Result<bool, ShellError>;
    /// `Ok(true)` when a new subscription was made, `Ok(false)` when one
    /// already existed.
    fn watch(&mut self, win: WinKey) -> Result<bool, ShellError>;
    fn unwatch(&mut self, win: WinKey);
    /// Removes the subscription but keeps the cached strip.
    fn pause(&mut self, win: WinKey) {
        self.unwatch(win);
    }
    fn shutdown(&mut self);
}

/// An activation whose selection is checked again later (see
/// [`TabStripReader::activate`]).
struct Verify {
    due: Instant,
    win: WinKey,
    index: usize,
    reselects: u32,
    /// Still to be called: the first (confirming) check has not passed yet.
    done: Option<ActivateDone>,
}

impl Verify {
    /// Reports the activation (once) and asks for the confirming rescan.
    fn report(&mut self, on_change: &ChangeCallback, result: Result<(), ShellError>) {
        if let Some(done) = self.done.take() {
            if result.is_ok() {
                on_change(self.win);
            }
            done(result);
        }
    }
}

/// Runs a due check; returns it again when it must be repeated.
fn run_check<B: Backend>(backend: &mut B, mut v: Verify, on_change: &ChangeCallback) -> Option<Verify> {
    let selected = backend.is_selected(v.win, v.index);
    crate::diag::note(|| format!("uia: check {:#x} tab {} -> {:?}", v.win.hwnd, v.index, selected.as_ref().map_err(|e| &e.message)));
    match selected {
        Ok(true) if v.done.is_some() => {
            v.report(on_change, Ok(()));
            v.due = Instant::now() + ACTIVATE_LATE_CHECK;
            Some(v)
        }
        // The late check passed too.
        Ok(true) => None,
        Ok(false) if v.reselects < ACTIVATE_RESELECTS => {
            let late = v.done.is_none();
            crate::diag::note(|| {
                format!("uia: reselect {:#x} tab {} ({} check)", v.win.hwnd, v.index, if late { "late" } else { "first" })
            });
            v.reselects += 1;
            RESELECTS[usize::from(late)].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            match backend.select(v.win, v.index) {
                Ok(()) => {
                    if late {
                        // Completed already: let the scanner see the change.
                        on_change(v.win);
                    }
                    v.due = Instant::now() + ACTIVATE_VERIFY_DELAY;
                    Some(v)
                }
                Err(e) => {
                    v.report(on_change, Err(e));
                    None
                }
            }
        }
        // The strip could not be read: check again a little later.
        Err(_) if v.reselects < ACTIVATE_RESELECTS => {
            v.reselects += 1;
            v.due = Instant::now() + ACTIVATE_VERIFY_DELAY;
            Some(v)
        }
        // Still not selected after the re-selections, or the strip cannot be
        // read: a failed activation attempt (the Engine retries it, then
        // gives it up, design.md "Error Handling").
        Ok(false) => {
            v.report(on_change, Err(ShellError {
                code: 0,
                message: format!("UIA select: tab {} did not stay selected", v.index),
            }));
            None
        }
        Err(e) => {
            v.report(on_change, Err(e));
            None
        }
    }
}

fn run_worker<B: Backend>(mut backend: B, jobs: &Receiver<Job>, on_change: &ChangeCallback) {
    let mut checks: Vec<Verify> = Vec::new();
    loop {
        let next = checks.iter().map(|v| v.due).min();
        let job = match next {
            None => match jobs.recv() {
                Ok(job) => Some(job),
                Err(_) => break,
            },
            Some(due) => match jobs.recv_timeout(due.saturating_duration_since(Instant::now())) {
                Ok(job) => Some(job),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            },
        };
        if let Some(job) = job {
            match job {
                Job::Read { win, slot } => {
                    // Runs even if the caller gave up: it warms the strip cache.
                    let t = Instant::now();
                    let result = backend.read(win);
                    crate::diag::note(|| {
                        format!("uia: read {:#x} {:.1} ms ok={}", win.hwnd, t.elapsed().as_secs_f64() * 1000.0, result.is_ok())
                    });
                    let ok = result.is_ok();
                    if !slot.complete(result) && ok {
                        // The caller timed out; let the scanner read again.
                        on_change(win);
                    }
                }
                Job::Select { win, index, slot } => {
                    // A select the caller gave up on must not happen later.
                    if slot.begin() {
                        slot.complete(backend.select(win, index));
                    }
                }
                Job::Activate { win, index, done } => {
                    // A newer activation of the window replaces a pending check.
                    if let Some(i) = checks.iter().position(|v| v.win == win) {
                        let mut old = checks.swap_remove(i);
                        old.report(on_change, Err(worker_error("superseded by a newer activation")));
                    }
                    let t = Instant::now();
                    match backend.select(win, index) {
                        Ok(()) => {
                            crate::diag::note(|| {
                                format!("uia: select {:#x} tab {index} {:.1} ms", win.hwnd, t.elapsed().as_secs_f64() * 1000.0)
                            });
                            checks.push(Verify {
                                due: Instant::now() + ACTIVATE_VERIFY_DELAY,
                                win,
                                index,
                                reselects: 0,
                                done: Some(done),
                            })
                        }
                        Err(e) => done(Err(e)),
                    }
                }
                Job::Watch { win } => {
                    if let Ok(true) = backend.watch(win) {
                        on_change(win);
                    }
                }
                Job::Unwatch { win } => backend.unwatch(win),
                Job::Pause { win } => backend.pause(win),
                Job::Shutdown => break,
            }
        }
        let now = Instant::now();
        let (due, later): (Vec<Verify>, Vec<Verify>) = std::mem::take(&mut checks).into_iter().partition(|v| v.due <= now);
        checks = later;
        for v in due {
            if let Some(again) = run_check(&mut backend, v, on_change) {
                checks.push(again);
            }
        }
    }
    for mut v in checks {
        v.report(on_change, Err(worker_error("the UIA worker stopped")));
    }
    backend.shutdown();
}

/// A one-shot result handed from the worker to a waiting caller, who may give
/// up after a timeout.
struct Slot<T> {
    state: Mutex<SlotState<T>>,
    done: Condvar,
}

enum SlotState<T> {
    Pending,
    Running,
    Done(T),
    Taken,
    Abandoned,
}

impl<T> Slot<T> {
    fn new() -> Self {
        Self {
            state: Mutex::new(SlotState::Pending),
            done: Condvar::new(),
        }
    }

    /// Worker: marks the job as started. `false` if the caller already gave up.
    fn begin(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        match *state {
            SlotState::Pending => {
                *state = SlotState::Running;
                true
            }
            SlotState::Abandoned => false,
            _ => true,
        }
    }

    /// Worker: delivers the result. `false` if the caller already gave up.
    fn complete(&self, value: T) -> bool {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if matches!(*state, SlotState::Abandoned) {
            return false;
        }
        *state = SlotState::Done(value);
        self.done.notify_all();
        true
    }

    /// Caller: waits up to `timeout`. On timeout the slot is abandoned and
    /// `None` is returned.
    fn wait(&self, timeout: Duration) -> Option<T> {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let (mut state, _) = self
            .done
            .wait_timeout_while(state, timeout, |s| {
                matches!(s, SlotState::Pending | SlotState::Running)
            })
            .unwrap_or_else(PoisonError::into_inner);
        match std::mem::replace(&mut *state, SlotState::Taken) {
            SlotState::Done(value) => Some(value),
            _ => {
                *state = SlotState::Abandoned;
                None
            }
        }
    }
}

/// MTA membership for the worker thread.
struct MtaGuard(bool);

impl MtaGuard {
    fn init() -> Self {
        // SAFETY: no reserved pointer; valid flags.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        Self(hr.is_ok())
    }
}

impl Drop for MtaGuard {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: balanced with the successful CoInitializeEx on this thread.
            unsafe { CoUninitialize() };
        }
    }
}

// ---------------------------------------------------------------------------
// UIA backend
// ---------------------------------------------------------------------------

fn uia_error(op: &str, e: &windows::core::Error) -> ShellError {
    ShellError {
        code: e.code().0,
        message: format!("UIA {op}: {}", e.message()),
    }
}

/// Forwards `StructureChanged` on one window's strip to the change callback.
#[implement(IUIAutomationStructureChangedEventHandler)]
struct StructureHandler {
    win: WinKey,
    on_change: ChangeCallback,
}

impl IUIAutomationStructureChangedEventHandler_Impl for StructureHandler_Impl {
    fn HandleStructureChangedEvent(
        &self,
        _sender: Ref<IUIAutomationElement>,
        _change: StructureChangeType,
        _runtime_id: *const SAFEARRAY,
    ) -> windows::core::Result<()> {
        (self.on_change)(self.win);
        Ok(())
    }
}

/// A `StructureChanged` registration: the element it is on and the handler.
struct Subscription {
    strip: IUIAutomationElement,
    handler: IUIAutomationStructureChangedEventHandler,
}

#[derive(Default)]
struct WindowStrip {
    strip: Option<IUIAutomationElement>,
    /// `watch` was requested (and `unwatch` was not).
    watched: bool,
    subscription: Option<Subscription>,
}

/// One exposed tab item.
struct Item {
    element: IUIAutomationElement,
    name: Vec<u16>,
    selected: bool,
}

struct UiaBackend {
    automation: IUIAutomation,
    /// `TabItem` control type.
    tab_items: IUIAutomationCondition,
    /// `TabItem` with class `ListViewItem` (finds the strip's first item).
    strip_item: IUIAutomationCondition,
    walker: IUIAutomationTreeWalker,
    /// Name, class, and selection state of the strip's items.
    cache: IUIAutomationCacheRequest,
    windows: HashMap<WinKey, WindowStrip>,
    on_change: ChangeCallback,
}

impl UiaBackend {
    fn new(on_change: ChangeCallback) -> Result<Self, ShellError> {
        fn e(op: &'static str) -> impl Fn(windows::core::Error) -> ShellError {
            move |err| uia_error(op, &err)
        }
        // SAFETY: valid CLSIDs, in-process server, plain UIA calls on live
        // interfaces created here.
        unsafe {
            let automation: IUIAutomation =
                CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)
                    .or_else(|_| CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER))
                    .map_err(e("create client"))?;
            if let Ok(a2) = automation.cast::<IUIAutomation2>() {
                let _ = a2.SetTransactionTimeout(UIA_TRANSACTION_TIMEOUT_MS);
            }
            let tab_items = automation
                .CreatePropertyCondition(
                    UIA_ControlTypePropertyId,
                    &VARIANT::from(UIA_TabItemControlTypeId.0),
                )
                .map_err(e("create condition"))?;
            let class = automation
                .CreatePropertyCondition(
                    UIA_ClassNamePropertyId,
                    &VARIANT::from(BSTR::from(TAB_ITEM_CLASS)),
                )
                .map_err(e("create condition"))?;
            let strip_item = automation
                .CreateAndCondition(&tab_items, &class)
                .map_err(e("create condition"))?;
            let walker = automation.RawViewWalker().map_err(e("tree walker"))?;
            let cache = automation.CreateCacheRequest().map_err(e("cache request"))?;
            cache
                .AddProperty(UIA_NamePropertyId)
                .and_then(|()| cache.AddProperty(UIA_ClassNamePropertyId))
                .and_then(|()| cache.AddProperty(UIA_SelectionItemIsSelectedPropertyId))
                .map_err(e("cache request"))?;
            Ok(Self {
                automation,
                tab_items,
                strip_item,
                walker,
                cache,
                windows: HashMap::new(),
                on_change,
            })
        }
    }

    /// Drops entries of destroyed windows and of HWNDs reused by a newer key.
    fn prune(&mut self, current: WinKey) {
        let stale: Vec<WinKey> = self
            .windows
            .keys()
            .copied()
            .filter(|k| {
                (k.hwnd == current.hwnd && k.seq != current.seq)
                    // SAFETY: IsWindow tolerates stale HWNDs.
                    || !unsafe { IsWindow(Some(hwnd_of(k.hwnd))) }.as_bool()
            })
            .collect();
        for key in stale {
            self.unwatch(key);
        }
    }

    /// Searches the strip (the parent of the first tab strip item, under the
    /// XAML island first, see [`crate::uia_strip`]), caches it and, if the
    /// window is watched, (re)subscribes on it.
    fn find_strip(&mut self, win: WinKey) -> Result<IUIAutomationElement, ShellError> {
        self.prune(win);
        let t = Instant::now();
        let strip = uia_strip::find_strip(&self.automation, &self.walker, &self.strip_item, hwnd_of(win.hwnd))
            .map_err(|e| uia_error("find tab strip", &e))?;
        crate::diag::note(|| format!("uia: find strip {:#x} {:.1} ms", win.hwnd, t.elapsed().as_secs_f64() * 1000.0));
        let entry = self.windows.entry(win).or_default();
        entry.strip = Some(strip.clone());
        if entry.watched {
            let current = entry.subscription.as_ref().is_some_and(|s| {
                // SAFETY: plain UIA call on live interfaces.
                unsafe { self.automation.CompareElements(&s.strip, &strip) }
                    .is_ok_and(|same| same.as_bool())
            });
            if !current {
                self.unsubscribe(win);
                // A failure leaves the window unsubscribed; the next search retries.
                let t = Instant::now();
                let _ = self.subscribe(win, &strip);
                crate::diag::note(|| format!("uia: subscribe {:#x} {:.1} ms", win.hwnd, t.elapsed().as_secs_f64() * 1000.0));
            }
        }
        Ok(strip)
    }

    /// The strip's exposed tab items, in order.
    fn items_of(&self, strip: &IUIAutomationElement) -> windows::core::Result<Vec<Item>> {
        // SAFETY: plain UIA calls on live interfaces.
        unsafe {
            let all = strip.FindAllBuildCache(TreeScope_Children, &self.tab_items, &self.cache)?;
            let mut items = Vec::new();
            for i in 0..all.Length()? {
                let element = all.GetElement(i)?;
                let class = element.CachedClassName().unwrap_or_default();
                if !is_tab_item_class(&class.to_string()) {
                    continue;
                }
                let name = element
                    .CachedName()
                    .map(|n| n.to_vec())
                    .unwrap_or_default();
                let selected = element
                    .GetCachedPropertyValue(UIA_SelectionItemIsSelectedPropertyId)
                    .ok()
                    .and_then(|v| bool::try_from(&v).ok())
                    .unwrap_or(false);
                items.push(Item {
                    element,
                    name,
                    selected,
                });
            }
            Ok(items)
        }
    }

    /// The exposed items of `win`'s strip, from the cached strip when it still
    /// answers with items, otherwise after a new search.
    fn items(&mut self, win: WinKey) -> Result<Vec<Item>, ShellError> {
        let cached = self.windows.get(&win).and_then(|w| w.strip.clone());
        if let Some(strip) = cached {
            match self.items_of(&strip) {
                Ok(items) if !items.is_empty() => return Ok(items),
                _ => {
                    if let Some(entry) = self.windows.get_mut(&win) {
                        entry.strip = None;
                    }
                }
            }
        }
        let strip = self.find_strip(win)?;
        let items = self
            .items_of(&strip)
            .map_err(|e| uia_error("read tab items", &e))?;
        if items.is_empty() {
            if let Some(entry) = self.windows.get_mut(&win) {
                entry.strip = None;
            }
            return Err(ShellError {
                code: 0,
                message: "UIA read tab items: the tab strip exposes no tab".into(),
            });
        }
        Ok(items)
    }

    /// The cached strip of `win`, or a newly found one.
    fn strip(&mut self, win: WinKey) -> Result<IUIAutomationElement, ShellError> {
        match self.windows.get(&win).and_then(|w| w.strip.clone()) {
            Some(strip) => Ok(strip),
            None => self.find_strip(win),
        }
    }

    fn drop_strip(&mut self, win: WinKey) {
        if let Some(entry) = self.windows.get_mut(&win) {
            entry.strip = None;
        }
    }

    /// One activation attempt: selects item `index` of every item of the
    /// strip (realized or not, [`uia_strip::all_items`]) once the strip has
    /// exactly `tabs` items (Explorer's tab count). Without the
    /// `ItemContainerPattern`, the exposed child items are used, which must
    /// then number `tabs` too.
    fn try_select(&mut self, win: WinKey, index: usize, tabs: usize) -> Result<(), ShellError> {
        let strip = self.strip(win)?;
        let items = match uia_strip::all_items(&strip) {
            Some(items) if !items.is_empty() => items,
            Some(_) => {
                // A stale cached strip answers with nothing: search again.
                self.drop_strip(win);
                return Err(ShellError {
                    code: 0,
                    message: "UIA select: the tab strip has no item".into(),
                });
            }
            None => self.items(win)?.into_iter().map(|i| i.element).collect(),
        };
        if items.len() != tabs {
            return Err(ShellError {
                code: 0,
                message: format!(
                    "UIA select: the tab strip has {} item(s) for {tabs} tab(s)",
                    items.len()
                ),
            });
        }
        let Some(item) = items.get(index) else {
            return Err(ShellError {
                code: E_INVALIDARG.0,
                message: format!("UIA select: no tab {index} ({tabs} tabs)"),
            });
        };
        uia_strip::realize(item);
        uia_strip::select_item(item).map_err(|e| uia_error("select", &e))
    }

    fn subscribe(&mut self, win: WinKey, strip: &IUIAutomationElement) -> Result<(), ShellError> {
        let handler: IUIAutomationStructureChangedEventHandler = StructureHandler {
            win,
            on_change: Arc::clone(&self.on_change),
        }
        .into();
        let scope = TreeScope(TreeScope_Element.0 | TreeScope_Children.0);
        // SAFETY: plain UIA call on live interfaces, from the MTA worker.
        unsafe {
            self.automation.AddStructureChangedEventHandler(
                strip,
                scope,
                None::<&IUIAutomationCacheRequest>,
                &handler,
            )
        }
        .map_err(|e| uia_error("subscribe StructureChanged", &e))?;
        if let Some(entry) = self.windows.get_mut(&win) {
            entry.subscription = Some(Subscription {
                strip: strip.clone(),
                handler,
            });
        }
        Ok(())
    }

    fn unsubscribe(&mut self, win: WinKey) {
        if let Some(sub) = self.windows.get_mut(&win).and_then(|w| w.subscription.take()) {
            // SAFETY: plain UIA call on live interfaces, from the MTA worker
            // (never from inside a handler).
            let _ = unsafe {
                self.automation
                    .RemoveStructureChangedEventHandler(&sub.strip, &sub.handler)
            };
        }
    }
}

impl Backend for UiaBackend {
    fn read(&mut self, win: WinKey) -> Result<TabStripState, ShellError> {
        let items = self.items(win)?;
        Ok(TabStripState {
            selected: items.iter().position(|i| i.selected),
            names: items.into_iter().map(|i| WideString::new(i.name)).collect(),
        })
    }

    fn select(&mut self, win: WinKey, index: usize) -> Result<(), ShellError> {
        let top = hwnd_of(win.hwnd);
        let deadline = Instant::now() + ACTIVATE_CATCH_UP;
        let mut attempt = 0;
        loop {
            // SAFETY: IsWindow tolerates stale HWNDs.
            if !unsafe { IsWindow(Some(top)) }.as_bool() {
                return Err(ShellError {
                    code: 0,
                    message: "UIA select: the window no longer exists".into(),
                });
            }
            match self.try_select(win, index, uia_strip::tab_window_count(top)) {
                Ok(()) => return Ok(()),
                Err(e) if Instant::now() >= deadline => return Err(e),
                // The strip lags behind tab creation: read it again.
                Err(_) => {
                    std::thread::sleep(uia_strip::backoff(attempt));
                    attempt += 1;
                }
            }
        }
    }

    fn is_selected(&mut self, win: WinKey, index: usize) -> Result<bool, ShellError> {
        let strip = self.strip(win)?;
        let items = match uia_strip::all_items(&strip) {
            Some(items) if !items.is_empty() => items,
            _ => self.items(win)?.into_iter().map(|i| i.element).collect(),
        };
        let item = items.get(index).ok_or_else(|| ShellError {
            code: E_INVALIDARG.0,
            message: format!("UIA is selected: no tab {index} ({} items)", items.len()),
        })?;
        uia_strip::is_selected(item).map_err(|e| uia_error("is selected", &e))
    }

    fn watch(&mut self, win: WinKey) -> Result<bool, ShellError> {
        let entry = self.windows.entry(win).or_default();
        entry.watched = true;
        if entry.subscription.is_some() {
            return Ok(false);
        }
        let cached = entry.strip.clone();
        match cached {
            Some(strip) => self.subscribe(win, &strip)?,
            // `find_strip` subscribes a watched window.
            None => {
                self.find_strip(win)?;
            }
        }
        match self.windows.get(&win) {
            Some(w) if w.subscription.is_some() => Ok(true),
            _ => Err(ShellError {
                code: 0,
                message: "UIA subscribe StructureChanged: failed".into(),
            }),
        }
    }

    fn unwatch(&mut self, win: WinKey) {
        self.unsubscribe(win);
        self.windows.remove(&win);
    }

    fn pause(&mut self, win: WinKey) {
        self.unsubscribe(win);
        if let Some(entry) = self.windows.get_mut(&win) {
            entry.watched = false;
        }
    }

    fn shutdown(&mut self) {
        let keys: Vec<WinKey> = self.windows.keys().copied().collect();
        for win in keys {
            self.unsubscribe(win);
        }
        self.windows.clear();
        // SAFETY: plain UIA call from the MTA worker.
        let _ = unsafe { self.automation.RemoveAllEventHandlers() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const WIN: WinKey = WinKey { hwnd: 0x10, seq: 1 };

    #[test]
    fn tab_item_class_filter() {
        assert!(is_tab_item_class(TAB_ITEM_CLASS));
        assert!(!is_tab_item_class(NAVIGATION_ITEM_CLASS));
        assert!(!is_tab_item_class(""));
        assert!(!is_tab_item_class("ListViewItemPresenter"));
    }

    #[test]
    fn slot_delivers_a_result_completed_in_time() {
        let slot = Slot::new();
        assert!(slot.begin());
        assert!(slot.complete(7));
        assert_eq!(slot.wait(Duration::from_millis(10)), Some(7));
    }

    #[test]
    fn slot_abandoned_after_timeout_rejects_late_work() {
        let slot = Slot::<u32>::new();
        assert_eq!(slot.wait(Duration::from_millis(10)), None);
        assert!(!slot.begin());
        assert!(!slot.complete(1));
    }

    #[test]
    fn slot_wakes_a_waiting_caller() {
        let slot = Arc::new(Slot::new());
        let worker = Arc::clone(&slot);
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            worker.complete("done")
        });
        assert_eq!(slot.wait(Duration::from_secs(5)), Some("done"));
        assert!(t.join().unwrap());
    }

    /// Records calls; each operation sleeps for `delay`.
    struct Scripted {
        delay: Duration,
        selects: Arc<AtomicUsize>,
        watch_ok: bool,
        /// Results of the next `is_selected` calls (then `true`).
        checks: Arc<Mutex<std::collections::VecDeque<bool>>>,
    }

    impl Backend for Scripted {
        fn is_selected(&mut self, _win: WinKey, _index: usize) -> Result<bool, ShellError> {
            Ok(self.checks.lock().unwrap().pop_front().unwrap_or(true))
        }
        fn read(&mut self, _win: WinKey) -> Result<TabStripState, ShellError> {
            std::thread::sleep(self.delay);
            Ok(TabStripState {
                names: vec![WideString::new("a".encode_utf16().collect())],
                selected: Some(0),
            })
        }
        fn select(&mut self, _win: WinKey, _index: usize) -> Result<(), ShellError> {
            self.selects.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(self.delay);
            Ok(())
        }
        fn watch(&mut self, _win: WinKey) -> Result<bool, ShellError> {
            if self.watch_ok {
                Ok(true)
            } else {
                Err(worker_error("no strip"))
            }
        }
        fn unwatch(&mut self, _win: WinKey) {}
        fn shutdown(&mut self) {}
    }

    struct Harness {
        reader: TabStripReader,
        changed: Arc<Mutex<Vec<WinKey>>>,
        selects: Arc<AtomicUsize>,
    }

    fn harness(delay_ms: u64, timeout_ms: u64, watch_ok: bool) -> Harness {
        harness_with_checks(delay_ms, timeout_ms, watch_ok, &[])
    }

    fn harness_with_checks(delay_ms: u64, timeout_ms: u64, watch_ok: bool, checks: &[bool]) -> Harness {
        let changed = Arc::new(Mutex::new(Vec::new()));
        let selects = Arc::new(AtomicUsize::new(0));
        let sink = Arc::clone(&changed);
        let on_change: ChangeCallback = Arc::new(move |w| sink.lock().unwrap().push(w));
        let counter = Arc::clone(&selects);
        let checks = Arc::new(Mutex::new(checks.iter().copied().collect()));
        let config = TabStripConfig {
            read_timeout: Duration::from_millis(timeout_ms),
            select_timeout: Duration::from_millis(timeout_ms),
        };
        let reader = TabStripReader::spawn(config, on_change, move |_| {
            Ok(Scripted {
                delay: Duration::from_millis(delay_ms),
                selects: counter,
                watch_ok,
                checks,
            })
        })
        .unwrap();
        Harness {
            reader,
            changed,
            selects,
        }
    }

    #[test]
    fn fast_read_and_select_return_results_without_notifications() {
        let h = harness(0, 2000, true);
        let state = h.reader.read(WIN).unwrap();
        assert_eq!(state.selected, Some(0));
        assert_eq!(state.names.len(), 1);
        h.reader.select(WIN, 0).unwrap();
        drop(h.reader);
        assert!(h.changed.lock().unwrap().is_empty());
        assert_eq!(h.selects.load(Ordering::SeqCst), 1);
    }

    /// Task 16.3: windows are spread round-robin over [`UIA_WORKERS`]
    /// workers and keep theirs, so a slow window does not hold up the
    /// others, while one window's jobs stay in order on one worker.
    #[test]
    fn windows_are_routed_round_robin_to_workers_that_run_in_parallel() {
        let delay = Duration::from_millis(300);
        let on_change: ChangeCallback = Arc::new(|_| {});
        let config = TabStripConfig {
            read_timeout: Duration::from_secs(5),
            select_timeout: Duration::from_secs(5),
        };
        let mut workers = Vec::new();
        for _ in 0..UIA_WORKERS {
            let selects = Arc::new(AtomicUsize::new(0));
            workers.push(
                TabStripReader::spawn_worker(on_change.clone(), move |_| {
                    Ok(Scripted { delay, selects, watch_ok: true, checks: Arc::default() })
                })
                .unwrap(),
            );
        }
        let reader = Arc::new(TabStripReader { workers, route: Mutex::default(), config });
        let w = |i: u64| WinKey { hwnd: 0x100 + i, seq: 1 };
        let index_of = |win: WinKey| {
            let worker = reader.worker_of(win) as *const Worker;
            reader.workers.iter().position(|x| std::ptr::eq(x, worker)).unwrap()
        };
        assert_eq!([index_of(w(1)), index_of(w(2)), index_of(w(3))], [0, 1, 2]);
        assert_eq!(index_of(w(2)), 1, "sticky");
        assert_eq!(index_of(w(4)), 0, "round robin");

        // Two windows on different workers: their reads overlap.
        let timed = |a: WinKey, b: WinKey| {
            let start = Instant::now();
            let r = Arc::clone(&reader);
            let t = std::thread::spawn(move || r.read(a).map(|_| ()));
            reader.read(b).unwrap();
            t.join().unwrap().unwrap();
            start.elapsed()
        };
        let parallel = timed(w(1), w(2));
        assert!(parallel < delay * 2 - Duration::from_millis(50), "{parallel:?}");
        // The same window twice: one worker, one after the other.
        let serial = timed(w(3), w(3));
        assert!(serial >= delay * 2 - Duration::from_millis(10), "{serial:?}");
    }

    #[test]
    fn slow_read_times_out_and_notifies_on_late_success() {
        let h = harness(150, 30, true);
        let err = h.reader.read(WIN).unwrap_err();
        assert!(err.message.contains("timed out"), "{err:?}");
        drop(h.reader); // joins the worker after the late read finished
        assert_eq!(*h.changed.lock().unwrap(), vec![WIN]);
    }

    #[test]
    fn select_abandoned_before_it_starts_is_skipped() {
        let h = harness(150, 30, true);
        // The read occupies the worker past the select's deadline.
        assert!(h.reader.read(WIN).is_err());
        assert!(h.reader.select(WIN, 0).is_err());
        drop(h.reader);
        assert_eq!(h.selects.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn successful_watch_notifies_once_and_failed_watch_does_not() {
        let ok = harness(0, 2000, true);
        ok.reader.watch(WIN).unwrap();
        drop(ok.reader);
        assert_eq!(*ok.changed.lock().unwrap(), vec![WIN]);

        let failing = harness(0, 2000, false);
        failing.reader.watch(WIN).unwrap();
        drop(failing.reader);
        assert!(failing.changed.lock().unwrap().is_empty());
    }

    #[test]
    fn activate_reports_through_its_callback_and_notifies_on_success() {
        let h = harness(0, 2000, true);
        let (tx, rx) = mpsc::channel();
        h.reader
            .activate(WIN, 0, Box::new(move |r| tx.send(r).unwrap()))
            .unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), Ok(()));
        drop(h.reader);
        assert_eq!(h.selects.load(Ordering::SeqCst), 1);
        assert_eq!(*h.changed.lock().unwrap(), vec![WIN], "the confirming rescan");
    }

    /// Task 16.3: Explorer's own selection of a tab it has just created can
    /// override an activation; the worker re-selects before reporting.
    #[test]
    fn activate_reselects_a_tab_that_lost_its_selection_before_reporting() {
        let h = harness_with_checks(0, 2000, true, &[false]);
        let (tx, rx) = mpsc::channel();
        let start = Instant::now();
        h.reader.activate(WIN, 3, Box::new(move |r| tx.send(r).unwrap())).unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), Ok(()));
        assert!(start.elapsed() >= ACTIVATE_VERIFY_DELAY * 2, "reported after the second check");
        assert_eq!(h.selects.load(Ordering::SeqCst), 2);
        assert_eq!(*h.changed.lock().unwrap(), vec![WIN]);
        drop(h.reader);
    }

    #[test]
    fn a_late_override_is_reselected_and_rescanned() {
        let h = harness_with_checks(0, 2000, true, &[true, false]);
        let (tx, rx) = mpsc::channel();
        h.reader.activate(WIN, 1, Box::new(move |r| tx.send(r).unwrap())).unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), Ok(()));
        std::thread::sleep(ACTIVATE_LATE_CHECK + ACTIVATE_VERIFY_DELAY * 3);
        assert_eq!(h.selects.load(Ordering::SeqCst), 2, "re-selected at the late check");
        assert_eq!(*h.changed.lock().unwrap(), vec![WIN, WIN], "confirming rescan, then the late one");
        drop(h.reader);
    }

    #[test]
    fn a_tab_that_never_keeps_its_selection_is_reported_after_the_reselects() {
        let h = harness_with_checks(0, 2000, true, &[false; 10]);
        let (tx, rx) = mpsc::channel();
        h.reader.activate(WIN, 1, Box::new(move |r| tx.send(r).unwrap())).unwrap();
        assert!(rx.recv_timeout(Duration::from_secs(5)).unwrap().is_err(), "a failed attempt");
        assert_eq!(h.selects.load(Ordering::SeqCst), 1 + ACTIVATE_RESELECTS as usize);
        drop(h.reader);
    }

    #[test]
    fn start_failure_is_reported() {
        let on_change: ChangeCallback = Arc::new(|_| {});
        let r = TabStripReader::spawn::<Scripted, _>(TabStripConfig::default(), on_change, |_| {
            Err(worker_error("boom"))
        });
        assert!(r.is_err());
    }
}
