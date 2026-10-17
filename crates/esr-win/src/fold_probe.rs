//! `FoldContextProbe`: the fold context of a Folding_Candidate, answered as
//! `FoldContext { probe, desktop, unresponsive }` (tab folding, Req 14.9,
//! 14.11-14.13; design.md, "Tab folding" and "Probe pool").
//!
//! For `ProbeFoldContext { probe, candidate }` a pool worker (MTA, like the
//! Location prober's) does, in this order:
//!
//! 1. Enumerates the open Explorer windows: every top-level `CabinetWClass`
//!    window, keyed through the shared [`WindowKeys`] so that the keys agree
//!    with the ones the other adapters hand to the core.
//! 2. Computes a **fresh** [`DesktopView`] of the candidate with
//!    [`crate::desktops`] (never cached: Req 14.12). When the manager cannot be
//!    created the view is `Unknown`.
//! 3. Sends `WM_NULL` with `SendMessageTimeoutW(SMTO_ABORTIFHUNG, 1000 ms)` to
//!    every window on that desktop **in parallel** (every window of the
//!    enumeration for an `Unknown` view; the candidate itself is not pinged),
//!    so the probe takes a few ms normally and about 1 s at most, however many
//!    windows hang. A window that does not answer is an Unresponsive_Window
//!    (Req 14.13).
//! 4. Reports `Event::FoldContext` through the callback given at construction.
//!    Exactly one event is reported per probe, also when the probe panics (the
//!    view is then `Unknown` and nothing is unresponsive), so the core's fold
//!    never waits for an answer that does not come.
//!
//! A failed send is not always "unresponsive" ([`ping_is_unresponsive`]): a
//! window that was destroyed meanwhile (`ERROR_INVALID_WINDOW_HANDLE`) is not
//! open any more, and a window the process may not message (`ERROR_ACCESS_DENIED`,
//! an elevated Explorer window under UIPI) cannot be tested, so neither blocks
//! a fold.
//!
//! The pure parts ([`ping_targets`], [`unresponsive_among`], [`fold_context`],
//! [`ping_is_unresponsive`]) take the desktop answers and the ping as
//! closures, so they are unit-tested without Windows.

use std::collections::BTreeSet;
use std::marker::PhantomData;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;

use esr_core::engine::{DesktopView, Event, FoldProbeId, WinKey};
use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_INVALID_WINDOW_HANDLE, GetLastError, HWND, LPARAM, SetLastError,
    WIN32_ERROR, WPARAM,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_NULL,
};
use windows::core::BOOL;

use crate::desktops::{VirtualDesktops, desktop_view};
use crate::keys::{WindowKeys, hwnd_of, hwnd_value};
use crate::shell_util::{is_cabinet_class, window_class_name};

/// How long a window has to answer `WM_NULL` (Req 14.13, design.md).
pub const PING_TIMEOUT_MS: u32 = 1000;
/// Workers in the pool. Probes are rare (one per Folding_Candidate) and each
/// pings its windows in parallel on short-lived threads, so two are plenty.
pub const DEFAULT_THREADS: usize = 2;

/// Receives each answer, on a pool thread (the application posts it into the
/// Engine mailbox through its `EventSink`). The event is always an
/// [`Event::FoldContext`].
pub type EventCallback = Arc<dyn Fn(Event) + Send + Sync>;

/// Computes the answer to one probe; [`windows_context`] is the real one.
pub type ContextFn = Arc<dyn Fn(FoldProbeId, WinKey) -> Event + Send + Sync>;

// ---------------------------------------------------------------------------
// Pure logic
// ---------------------------------------------------------------------------

/// The windows to send `WM_NULL` to: the members of a `Known` view, or every
/// open window for `Unknown` (the core then treats them all as on the
/// candidate's desktop). Never the candidate itself. Sorted and unique.
pub fn ping_targets(view: &DesktopView, open: &[WinKey], candidate: WinKey) -> Vec<WinKey> {
    let all: BTreeSet<WinKey> = match view {
        DesktopView::Known(set) => set.clone(),
        DesktopView::Unknown => open.iter().copied().collect(),
    };
    all.into_iter().filter(|w| w.hwnd != candidate.hwnd).collect()
}

/// Whether a `SendMessageTimeoutW(WM_NULL)` result means the window is an
/// Unresponsive_Window. `delivered` is "the call returned non-zero";
/// `last_error` is `GetLastError` right after a failed call.
pub fn ping_is_unresponsive(delivered: bool, last_error: u32) -> bool {
    !delivered
        && last_error != ERROR_INVALID_WINDOW_HANDLE.0
        && last_error != ERROR_ACCESS_DENIED.0
}

/// The `targets` for which `unresponsive(target)` is true, asking all of them
/// in parallel (one scoped thread each), so the total wait is the slowest
/// ping, not the sum. A ping that panics counts as responsive.
pub fn unresponsive_among(
    targets: &[WinKey],
    unresponsive: impl Fn(WinKey) -> bool + Sync,
) -> BTreeSet<WinKey> {
    let ask = &unresponsive;
    thread::scope(|scope| {
        let pending: Vec<_> = targets
            .iter()
            .map(|&target| {
                let spawned = thread::Builder::new()
                    .name("esr-fold-ping".into())
                    .spawn_scoped(scope, move || ask(target));
                (target, spawned)
            })
            .collect();
        pending
            .into_iter()
            .filter_map(|(target, spawned)| {
                let hung = match spawned {
                    Ok(handle) => handle.join().unwrap_or(false),
                    // Out of threads: ask inline rather than lose the answer.
                    Err(_) => catch_unwind(AssertUnwindSafe(|| ask(target))).unwrap_or(false),
                };
                hung.then_some(target)
            })
            .collect()
    })
}

/// The `FoldContext` event for `candidate`: computes the view with `view`
/// (given the candidate and the open windows), then pings
/// [`ping_targets`] with `unresponsive`.
pub fn fold_context(
    probe: FoldProbeId,
    candidate: WinKey,
    open: &[WinKey],
    view: impl FnOnce(WinKey, &[WinKey]) -> DesktopView,
    unresponsive: impl Fn(WinKey) -> bool + Sync,
) -> Event {
    let desktop = view(candidate, open);
    let targets = ping_targets(&desktop, open, candidate);
    let unresponsive = unresponsive_among(&targets, unresponsive);
    Event::FoldContext { probe, desktop, unresponsive }
}

/// The event sent when a probe could not be computed at all.
fn unknown_context(probe: FoldProbeId) -> Event {
    Event::FoldContext { probe, desktop: DesktopView::Unknown, unresponsive: BTreeSet::new() }
}

// ---------------------------------------------------------------------------
// The pool
// ---------------------------------------------------------------------------

/// Fold-context probes on a small MTA thread pool.
pub struct FoldContextProbe {
    jobs: Option<Sender<(FoldProbeId, WinKey)>>,
    closed: Arc<AtomicBool>,
}

impl FoldContextProbe {
    /// A pool of [`DEFAULT_THREADS`] workers computing real contexts
    /// ([`windows_context`]). `keys` is the registry shared with the other
    /// adapters; `on_context` receives each `Event::FoldContext`.
    pub fn new(keys: WindowKeys, on_context: EventCallback) -> Self {
        Self::with_context(DEFAULT_THREADS, windows_context(keys), on_context)
    }

    /// Like [`new`](Self::new) with `threads` workers (at least 1) and a custom
    /// computation (tests).
    pub fn with_context(threads: usize, context: ContextFn, on_context: EventCallback) -> Self {
        let (jobs, inbox) = channel();
        let inbox = Arc::new(Mutex::new(inbox));
        let closed = Arc::new(AtomicBool::new(false));
        for id in 0..threads.max(1) {
            let (inbox, context, on_context, closed) =
                (Arc::clone(&inbox), Arc::clone(&context), Arc::clone(&on_context), Arc::clone(&closed));
            // A failed spawn leaves a smaller pool; `probe` reports Unknown if there is none.
            let _ = thread::Builder::new()
                .name(format!("esr-fold-probe-{id}"))
                .spawn(move || worker_main(&inbox, &context, &on_context, &closed));
        }
        Self { jobs: Some(jobs), closed }
    }

    /// Starts a probe; `on_context` is called once with its `FoldContext`,
    /// normally within a few ms and at most about [`PING_TIMEOUT_MS`] later
    /// (plus the time of earlier probes queued on the same worker).
    pub fn probe(&self, probe: FoldProbeId, candidate: WinKey) {
        let sent = self.jobs.as_ref().is_some_and(|jobs| jobs.send((probe, candidate)).is_ok());
        debug_assert!(sent || self.closed.load(Ordering::SeqCst), "fold probe pool has no workers");
    }
}

impl Drop for FoldContextProbe {
    fn drop(&mut self) {
        // No further callbacks; workers leave when the queue is closed. They
        // are not joined: one may be in a 1 s ping.
        self.closed.store(true, Ordering::SeqCst);
        self.jobs = None;
    }
}

/// MTA membership for the worker thread (the `IVirtualDesktopManager` is
/// created per probe on it).
struct MtaGuard {
    joined: bool,
    _not_send: PhantomData<*const ()>,
}

impl MtaGuard {
    fn init() -> Self {
        // SAFETY: no reserved pointer; valid flags.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        Self { joined: hr.is_ok(), _not_send: PhantomData }
    }
}

impl Drop for MtaGuard {
    fn drop(&mut self) {
        if self.joined {
            // SAFETY: balances the successful CoInitializeEx on this thread.
            unsafe { CoUninitialize() };
        }
    }
}

type Inbox = Arc<Mutex<Receiver<(FoldProbeId, WinKey)>>>;

fn worker_main(inbox: &Inbox, context: &ContextFn, on_context: &EventCallback, closed: &AtomicBool) {
    let _com = MtaGuard::init();
    loop {
        // Hold the queue lock only while waiting for a job, not while working.
        let next = inbox.lock().unwrap_or_else(PoisonError::into_inner).recv();
        let Ok((probe, candidate)) = next else { return };
        if closed.load(Ordering::SeqCst) {
            return;
        }
        let event = catch_unwind(AssertUnwindSafe(|| context(probe, candidate)))
            .unwrap_or_else(|_| unknown_context(probe));
        if closed.load(Ordering::SeqCst) {
            return;
        }
        on_context(event);
    }
}

// ---------------------------------------------------------------------------
// Windows glue
// ---------------------------------------------------------------------------

/// The real computation: enumerate the Explorer windows, compute the view with
/// `IVirtualDesktopManager`, ping the windows on the desktop. Run it on a
/// COM-initialized thread (the pool's workers are).
pub fn windows_context(keys: WindowKeys) -> ContextFn {
    Arc::new(move |probe, candidate| {
        let open: Vec<WinKey> =
            cabinet_windows().into_iter().map(|hwnd| keys.key_of(hwnd)).collect();
        fold_context(
            probe,
            candidate,
            &open,
            |candidate, open| match VirtualDesktops::new() {
                Ok(manager) => desktop_view(&manager, candidate, open),
                Err(_) => DesktopView::Unknown,
            },
            ping_unresponsive,
        )
    })
}

/// [`desktop_view`] over the real manager, for callers that only have `open`.
pub fn view_of(candidate: WinKey, open: &[WinKey]) -> DesktopView {
    match VirtualDesktops::new() {
        Ok(manager) => manager.view(candidate, open),
        Err(_) => DesktopView::Unknown,
    }
}

/// Sends `WM_NULL` to the window and reports whether it is an
/// Unresponsive_Window (see [`ping_is_unresponsive`]).
pub fn ping_unresponsive(target: WinKey) -> bool {
    let hwnd = hwnd_of(target.hwnd);
    // SAFETY: plain message send to a window handle; a stale handle just fails.
    // The error is cleared first so a stale value is never mistaken for this call's.
    let delivered = unsafe {
        SetLastError(WIN32_ERROR(0));
        SendMessageTimeoutW(hwnd, WM_NULL, WPARAM(0), LPARAM(0), SMTO_ABORTIFHUNG, PING_TIMEOUT_MS, None)
    }
    .0 != 0;
    // SAFETY: reads this thread's last error, set by the call above.
    let error = if delivered { 0 } else { unsafe { GetLastError() }.0 };
    ping_is_unresponsive(delivered, error)
}

/// The HWND values of every top-level `CabinetWClass` window (visible or not:
/// a window opened hidden by the Restore_Engine is open too).
pub fn cabinet_windows() -> Vec<u64> {
    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: `lparam` is the `&mut Vec<u64>` passed by `cabinet_windows`,
        // alive for the whole synchronous EnumWindows call.
        let found = unsafe { &mut *(lparam.0 as *mut Vec<u64>) };
        if is_cabinet_class(&window_class_name(hwnd)) {
            found.push(hwnd_value(hwnd));
        }
        BOOL(1)
    }
    let mut found: Vec<u64> = Vec::new();
    // SAFETY: the callback only uses `lparam` as above. A failed enumeration
    // leaves what was found so far.
    let _ = unsafe { EnumWindows(Some(collect), LPARAM((&raw mut found) as isize)) };
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktops::{DesktopId, DesktopQuery, QueryFailed};
    use std::collections::HashMap;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn w(n: u64) -> WinKey {
        WinKey { hwnd: n, seq: n }
    }

    fn known(ns: &[u64]) -> DesktopView {
        DesktopView::Known(ns.iter().map(|&n| w(n)).collect())
    }

    fn set(ns: &[u64]) -> BTreeSet<WinKey> {
        ns.iter().map(|&n| w(n)).collect()
    }

    #[test]
    fn targets_are_the_known_view_without_the_candidate() {
        let view = known(&[9, 3, 1]);
        assert_eq!(ping_targets(&view, &[w(1), w(2), w(3)], w(9)), vec![w(1), w(3)]);
    }

    #[test]
    fn targets_of_an_unknown_view_are_every_open_window() {
        let open = [w(5), w(1), w(9), w(5)];
        assert_eq!(ping_targets(&DesktopView::Unknown, &open, w(9)), vec![w(1), w(5)]);
    }

    #[test]
    fn ping_classification() {
        assert!(!ping_is_unresponsive(true, 0));
        assert!(ping_is_unresponsive(false, 1460), "ERROR_TIMEOUT");
        assert!(ping_is_unresponsive(false, 0), "a hung window may leave no error");
        assert!(!ping_is_unresponsive(false, ERROR_INVALID_WINDOW_HANDLE.0), "destroyed meanwhile");
        assert!(!ping_is_unresponsive(false, ERROR_ACCESS_DENIED.0), "cannot be tested");
    }

    #[test]
    fn pings_run_in_parallel_and_collect_the_unresponsive() {
        let targets: Vec<WinKey> = (1..=8).map(w).collect();
        let started = Instant::now();
        let hung = unresponsive_among(&targets, |t| {
            thread::sleep(Duration::from_millis(200));
            t.hwnd % 3 == 0
        });
        assert_eq!(hung, set(&[3, 6]));
        // 8 sequential pings would take 1.6 s.
        assert!(started.elapsed() < Duration::from_millis(1000), "{:?}", started.elapsed());
        assert!(unresponsive_among(&[], |_| true).is_empty());
    }

    #[test]
    fn a_panicking_ping_counts_as_responsive() {
        let hung = unresponsive_among(&[w(1), w(2)], |t| {
            assert!(t.hwnd != 1, "boom");
            true
        });
        assert_eq!(hung, set(&[2]));
    }

    #[derive(Default)]
    struct Fake(HashMap<u64, (Option<bool>, Option<DesktopId>)>);

    impl DesktopQuery for Fake {
        fn on_current_desktop(&self, hwnd: u64) -> Result<bool, QueryFailed> {
            self.0.get(&hwnd).and_then(|e| e.0).ok_or(QueryFailed)
        }
        fn desktop_id(&self, hwnd: u64) -> Result<DesktopId, QueryFailed> {
            self.0.get(&hwnd).and_then(|e| e.1).ok_or(QueryFailed)
        }
    }

    #[test]
    fn context_pings_only_windows_on_the_candidates_desktop() {
        // Candidate 9 unreadable id; 1 and 3 on the current desktop, 2 elsewhere.
        let mut q = Fake::default();
        q.0.insert(9, (None, None));
        q.0.insert(1, (Some(true), None));
        q.0.insert(2, (Some(false), Some(20)));
        q.0.insert(3, (Some(true), Some(10)));
        let pinged = Mutex::new(Vec::new());
        let event = fold_context(
            FoldProbeId(7),
            w(9),
            &[w(1), w(2), w(3)],
            |c, open| desktop_view(&q, c, open),
            |t| {
                pinged.lock().unwrap().push(t.hwnd);
                t.hwnd == 3
            },
        );
        let mut pinged = pinged.into_inner().unwrap();
        pinged.sort_unstable();
        assert_eq!(pinged, vec![1, 3], "the other desktop's window and the candidate are not pinged");
        assert_eq!(
            event,
            Event::FoldContext { probe: FoldProbeId(7), desktop: known(&[9, 1, 3]), unresponsive: set(&[3]) }
        );
    }

    #[test]
    fn context_with_an_unknown_view_pings_every_open_window() {
        let event = fold_context(
            FoldProbeId(1),
            w(9),
            &[w(1), w(2)],
            |_, _| DesktopView::Unknown,
            |t| t.hwnd == 2,
        );
        assert_eq!(
            event,
            Event::FoldContext { probe: FoldProbeId(1), desktop: DesktopView::Unknown, unresponsive: set(&[2]) }
        );
    }

    fn collecting() -> (EventCallback, mpsc::Receiver<Event>) {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        (Arc::new(move |e| drop(tx.lock().unwrap().send(e))), rx)
    }

    #[test]
    fn the_pool_answers_every_probe_once_and_survives_a_panic() {
        let context: ContextFn = Arc::new(|probe, candidate| {
            assert!(probe != FoldProbeId(2), "boom");
            Event::FoldContext {
                probe,
                desktop: DesktopView::Known([candidate].into_iter().collect()),
                unresponsive: BTreeSet::new(),
            }
        });
        let (callback, rx) = collecting();
        let pool = FoldContextProbe::with_context(2, context, callback);
        for n in 1..=4 {
            pool.probe(FoldProbeId(n), w(n));
        }
        let mut answers: Vec<Event> =
            (0..4).map(|_| rx.recv_timeout(Duration::from_secs(5)).expect("an answer")).collect();
        answers.sort_by_key(|e| match e {
            Event::FoldContext { probe, .. } => probe.0,
            other => panic!("{other:?}"),
        });
        for (i, event) in answers.iter().enumerate() {
            let n = i as u64 + 1;
            let desktop = if n == 2 { DesktopView::Unknown } else { known(&[n]) };
            assert_eq!(
                *event,
                Event::FoldContext { probe: FoldProbeId(n), desktop, unresponsive: BTreeSet::new() }
            );
        }
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err(), "exactly one answer per probe");
    }

    #[test]
    fn a_dropped_pool_posts_nothing_more() {
        let context: ContextFn = Arc::new(|probe, _| {
            thread::sleep(Duration::from_millis(200));
            unknown_context(probe)
        });
        let (callback, rx) = collecting();
        let pool = FoldContextProbe::with_context(1, context, callback);
        pool.probe(FoldProbeId(1), w(1));
        drop(pool);
        assert!(rx.recv_timeout(Duration::from_millis(600)).is_err());
    }

    // -- Windows ------------------------------------------------------------

    #[test]
    fn a_responsive_window_is_not_unresponsive_and_a_gone_one_is_not_either() {
        use windows::Win32::UI::WindowsAndMessaging::GetShellWindow;
        // SAFETY: plain query.
        let shell = unsafe { GetShellWindow() };
        if !shell.0.is_null() {
            assert!(!ping_unresponsive(WinKey { hwnd: hwnd_value(shell), seq: 1 }));
        }
        assert!(!ping_unresponsive(WinKey { hwnd: 0x0000_0001_0000_0001, seq: 1 }));
    }

    #[test]
    fn a_window_whose_thread_does_not_pump_is_unresponsive_within_the_deadline() {
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WS_POPUP,
        };
        use windows::core::w;

        let (tx, rx) = mpsc::channel();
        let owner = thread::spawn(move || {
            // SAFETY: a system class window of this thread, destroyed below.
            let hwnd = unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("STATIC"),
                    w!("esr-fold-probe-test"),
                    WS_POPUP,
                    0,
                    0,
                    1,
                    1,
                    None,
                    None,
                    None,
                    None,
                )
            }
            .expect("test window");
            tx.send(hwnd_value(hwnd)).unwrap();
            // Never pumps messages: the ping cannot be answered.
            thread::sleep(Duration::from_millis(2500));
            // SAFETY: created on this thread.
            unsafe { DestroyWindow(hwnd) }.ok();
        });
        let hwnd = rx.recv().unwrap();
        let started = Instant::now();
        assert!(ping_unresponsive(WinKey { hwnd, seq: 1 }));
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(900) && took < Duration::from_millis(2000), "{took:?}");
        owner.join().unwrap();
    }

    #[test]
    fn the_real_context_always_includes_the_candidate_in_a_known_view() {
        // COM may be unavailable on this thread; the answer is then Unknown.
        let _mta = MtaGuard::init();
        let keys = WindowKeys::new();
        let candidate = keys.key_of(0x7FFF_0001);
        let event = windows_context(keys)(FoldProbeId(1), candidate);
        match event {
            Event::FoldContext { probe, desktop, unresponsive } => {
                assert_eq!(probe, FoldProbeId(1));
                if let DesktopView::Known(set) = desktop {
                    assert!(set.contains(&candidate));
                }
                assert!(!unresponsive.contains(&candidate));
            }
            other => panic!("{other:?}"),
        }
    }
}
