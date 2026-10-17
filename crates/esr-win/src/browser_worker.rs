//! Browser workers: MTA threads that make a restoration's per-tab
//! cross-process `IWebBrowser2` calls off the Engine thread (task 16.3).
//!
//! While a restoration builds a window, Explorer's thread for that window is
//! busy creating tabs, so every call into one of its tabs waits for it:
//! attaching the `DWebBrowserEvents2` sink of a new tab took ~40–80 ms and
//! its `Navigate2` ~30–70 ms. Made on the Engine thread, these waits added
//! up across all windows (the windows' threads are independent, the Engine
//! thread is one). A [`BrowserWorkers`] pool runs them instead, so the
//! Engine thread can serve the other windows meanwhile.
//!
//! * The Engine thread marshals a tab's browser (`IWebBrowser2`, a proxy in
//!   its STA) into a stream ([`BrowserWorkers::attach`]); the worker
//!   unmarshals it into its MTA, attaches the sink there (the sink object is
//!   created on the worker and is thread-safe: it only posts messages), and
//!   keeps the browser for the tab's navigations and the sink's `Unadvise`.
//! * Every window is routed to one worker ([`Router`]), so the jobs of one
//!   window run in order: navigations of one window are issued in command
//!   order, each before the window's next new-tab command is posted.
//! * Late sinks ([`late_sink`], opt-in, off by default since task 16.3
//!   round 4d): a new tab's sink is attached right *after* its `Navigate2`
//!   returned, by the worker's sink thread, while the worker posts the
//!   window's next new-tab command. Off, it is attached before the
//!   navigation. With late sinks on, a sink still pending when the window's
//!   next command is due, or after
//!   `PENDING_SINK_WAIT` without work, is attached by the worker itself.
//! * Failures come back through the callbacks given with the jobs (they run
//!   on the worker or sink thread and post to the owner's thread).

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, channel};

use esr_core::engine::{ShellError, TabKey};
use windows::Win32::System::Com::Marshal::CoMarshalInterThreadInterfaceInStream;
use windows::Win32::System::Com::StructuredStorage::CoGetInterfaceAndReleaseStream;
use windows::Win32::System::Com::{
    COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize, IConnectionPoint,
    IConnectionPointContainer, IStream,
};
use windows::Win32::UI::Shell::{DWebBrowserEvents2, IWebBrowser2};
use windows::core::{IUnknown, Interface};

/// Number of worker threads (windows are spread over them).
pub const BROWSER_WORKERS: usize = 3;

/// Whether navigators started from now on use browser workers. On by
/// default since task 16.3 round 4a: 15 consecutive 3x10 release runs with
/// workers showed no Explorer crash and no wrong result (the single heap
/// corruption, `0xc0000374`, seen in round 3 before new-tab commands were
/// queued behind the worker's navigation did not recur). The timing tests
/// turn them off with `ESR_RT_WORKERS=0`.
static ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Turns browser workers on or off for navigators started afterwards.
pub fn set_enabled(on: bool) {
    ENABLED.store(on, std::sync::atomic::Ordering::SeqCst);
}

/// Whether browser workers are on ([`set_enabled`]).
pub fn enabled() -> bool {
    ENABLED.load(std::sync::atomic::Ordering::SeqCst)
}

/// Whether worker pools started from now on attach a new tab's sink *after*
/// its navigation (on a sink thread, while the window builds its next tab)
/// rather than before it. Off by default, by the user's decision in task
/// 16.3 round 4d: it was on in round 4c, but 54 release runs showed no
/// measurable gain for 3 x 10 tabs, and the risk of an explorer.exe heap
/// corruption (`0xc0000374`, once in about 35 runs in round 4b) when the
/// `Advise` overlaps the next tab's creation remains. The code is kept as
/// an opt-in. A crash during a restoration is handled as a failed attempt
/// either way (`ShellLost`, see design.md "Timing budget"). The runtime
/// sets it from `RuntimeOptions::late_sink` (default off; `ESR_LATE_SINK=1`
/// in the environment turns it on); the timing tests use
/// `ESR_RT_LATE_SINK=1`.
static LATE_SINK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Turns the post-navigation sink attach on or off for pools started afterwards.
pub fn set_late_sink(on: bool) {
    LATE_SINK.store(on, std::sync::atomic::Ordering::SeqCst);
}

/// Whether the post-navigation sink attach is on ([`set_late_sink`]).
pub fn late_sink() -> bool {
    LATE_SINK.load(std::sync::atomic::Ordering::SeqCst)
}

/// Creates a tab's event sink on the worker thread.
pub type SinkFactory = Box<dyn FnOnce() -> IUnknown + Send>;
/// Called on the worker thread when a job failed.
pub type OnFailure = Box<dyn FnOnce(ShellError) + Send>;

/// Assigns windows (top-level HWND values) to workers: a new window goes to
/// the worker with the fewest windows (the lowest index on a tie) and stays
/// there until [`release`](Self::release)d, so its jobs keep their order.
#[derive(Debug, Clone)]
pub struct Router {
    assigned: HashMap<u64, usize>,
    load: Vec<usize>,
}

impl Router {
    pub fn new(workers: usize) -> Self {
        assert!(workers > 0, "at least one worker");
        Self { assigned: HashMap::new(), load: vec![0; workers] }
    }

    /// The worker of `window`, assigning one if needed.
    pub fn route(&mut self, window: u64) -> usize {
        if let Some(&w) = self.assigned.get(&window) {
            return w;
        }
        let (w, _) = self
            .load
            .iter()
            .enumerate()
            .min_by_key(|(i, n)| (**n, *i))
            .expect("at least one worker");
        self.load[w] += 1;
        self.assigned.insert(window, w);
        w
    }

    /// The worker of `window`, if it has one.
    pub fn worker_of(&self, window: u64) -> Option<usize> {
        self.assigned.get(&window).copied()
    }

    /// Forgets `window` (destroyed).
    pub fn release(&mut self, window: u64) {
        if let Some(w) = self.assigned.remove(&window) {
            self.load[w] -= 1;
        }
    }
}

/// A marshaled interface on its way to a worker.
struct Marshaled(IStream);

// SAFETY: a stream from `CoMarshalInterThreadInterfaceInStream` exists to be
// handed to another thread, which unmarshals (and releases) it.
unsafe impl Send for Marshaled {}

enum Job {
    Attach { window: u64, tab: TabKey, browser: Marshaled, sink: Option<SinkFactory>, failed: OnFailure },
    Navigate { tab: TabKey, pidl: Vec<u8>, failed: OnFailure },
    Detach { tab: TabKey },
    /// `PostMessageW(window, WM_COMMAND, command)`, after the jobs queued
    /// before it (a new-tab command is posted only once the previous tab's
    /// `Navigate2` has returned, and, without late sinks, its sink is
    /// attached, as when both ran on the Engine thread).
    PostCommand { window: u64, command: usize },
}

/// The worker threads; dropping the pool ends them (each detaches its tabs'
/// sinks first).
pub struct BrowserWorkers {
    senders: Vec<Sender<Job>>,
    router: Router,
    /// Tab → worker, for the tabs attached to a worker.
    tabs: HashMap<TabKey, usize>,
}

impl BrowserWorkers {
    /// Starts [`BROWSER_WORKERS`] threads (with sink threads if
    /// [`late_sink`] is on).
    pub fn start() -> Result<Self, ShellError> {
        let late = late_sink();
        let mut senders = Vec::with_capacity(BROWSER_WORKERS);
        for i in 0..BROWSER_WORKERS {
            let (tx, rx) = channel::<Job>();
            std::thread::Builder::new()
                .name(format!("esr-browser-{i}"))
                .spawn(move || worker(i, rx, late))
                .map_err(|e| error(format!("browser worker thread: {e}")))?;
            senders.push(tx);
        }
        Ok(Self { senders, router: Router::new(BROWSER_WORKERS), tabs: HashMap::new() })
    }

    /// Whether `tab` is attached to a worker.
    pub fn has(&self, tab: TabKey) -> bool {
        self.tabs.contains_key(&tab)
    }

    /// Hands `tab` (of top-level window `window`) to its window's worker:
    /// `browser` is marshaled here (the calling thread's apartment), and the
    /// worker attaches `sink`, if any, to it. A failure to attach is reported
    /// through `failed`; the browser is kept for navigations either way.
    pub fn attach(
        &mut self,
        window: u64,
        tab: TabKey,
        browser: &IWebBrowser2,
        sink: Option<SinkFactory>,
        failed: OnFailure,
    ) -> Result<(), ShellError> {
        // SAFETY: plain COM call; marshaling a proxy hands out references it
        // already holds (no call into Explorer).
        let stream = unsafe { CoMarshalInterThreadInterfaceInStream(&IWebBrowser2::IID, browser) }
            .map_err(|e| error(format!("CoMarshalInterThreadInterfaceInStream: {}", e.message())))?;
        let w = self.router.route(window);
        if let Some(old) = self.tabs.insert(tab, w)
            && old != w
        {
            let _ = self.senders[old].send(Job::Detach { tab });
        }
        self.senders[w]
            .send(Job::Attach { window, tab, browser: Marshaled(stream), sink, failed })
            .map_err(|_| {
                self.tabs.remove(&tab);
                error("browser worker stopped".into())
            })
    }

    /// Issues `Navigate2(pidl)` on the worker that holds `tab`. Returns
    /// false if no worker holds it. A failure is reported through `failed`.
    pub fn navigate(&self, tab: TabKey, pidl: Vec<u8>, failed: OnFailure) -> bool {
        match self.tabs.get(&tab) {
            Some(&w) => self.senders[w].send(Job::Navigate { tab, pidl, failed }).is_ok(),
            None => false,
        }
    }

    /// Detaches `tab`'s sink and drops its browser on its worker.
    pub fn detach(&mut self, tab: TabKey) {
        if let Some(w) = self.tabs.remove(&tab) {
            let _ = self.senders[w].send(Job::Detach { tab });
        }
    }

    /// Posts `WM_COMMAND command` to top-level `window` from its worker,
    /// after the jobs already queued for the window. Returns false if the
    /// workers stopped.
    pub fn post_command(&mut self, window: u64, command: usize) -> bool {
        let w = self.router.route(window);
        self.senders[w].send(Job::PostCommand { window, command }).is_ok()
    }

    /// Forgets top-level window `window` (destroyed) and its tabs.
    pub fn window_destroyed(&mut self, window: u64) {
        let gone: Vec<TabKey> = self
            .tabs
            .keys()
            .copied()
            .filter(|t| !is_alive(t.hwnd))
            .collect();
        for tab in gone {
            self.detach(tab);
        }
        self.router.release(window);
    }
}

/// What a worker holds for one tab.
struct Held {
    browser: IWebBrowser2,
    /// Top-level window (HWND value) of the tab.
    window: u64,
    /// The sink attached by this worker.
    advise: Option<(IConnectionPoint, u32)>,
    /// Late sinks: the sink still to attach, after the tab's navigation.
    pending: Option<(SinkFactory, OnFailure)>,
    /// Late sinks: the sink was handed to the sink thread, which holds its
    /// connection (and detaches it).
    late: bool,
}

impl Held {
    /// Detaches the tab's sink, wherever it is attached.
    fn release(mut self, sinks: Option<&Sender<SinkJob>>, tab: TabKey) {
        if let Some((point, cookie)) = self.advise.take() {
            // SAFETY: plain COM call with the cookie Advise returned; a dead
            // server just fails.
            let _ = unsafe { point.Unadvise(cookie) };
        }
        if self.late
            && let Some(tx) = sinks
        {
            let _ = tx.send(SinkJob::Detach { tab });
        }
    }

    /// Attaches the pending sink here, on the worker (before anything else
    /// is sent to the tab's window): the order without late sinks.
    fn attach_pending_now(&mut self, tab: TabKey) {
        if let Some((make, failed)) = self.pending.take() {
            let start = std::time::Instant::now();
            match advise(&self.browser, &make()) {
                Ok(a) => self.advise = Some(a),
                Err(e) => failed(error(format!("attaching the tab sink: {}", e.message()))),
            }
            crate::diag::note(|| {
                format!("worker: attach {:#x} {:.1} ms (pending, now)", tab.hwnd, start.elapsed().as_secs_f64() * 1000.0)
            });
        }
    }
}

/// An MTA interface pointer moved between two MTA threads of this process.
struct Agile<T>(T);

// SAFETY: only used for interfaces unmarshaled into the MTA (proxies), which
// every MTA thread of the process may call; both ends are MTA threads.
unsafe impl<T> Send for Agile<T> {}

/// Work for a worker's sink thread (late sinks).
enum SinkJob {
    Advise { tab: TabKey, browser: Agile<IWebBrowser2>, make: SinkFactory, failed: OnFailure },
    Detach { tab: TabKey },
}

/// How long a worker keeps a late sink pending while it has nothing else to
/// do: a tab that is not navigated meanwhile gets its sink attached anyway.
const PENDING_SINK_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

/// A worker's sink thread: attaches late sinks in the order the worker hands
/// them over, while the worker goes on with the window's next new-tab
/// command, and detaches them again. Ends (detaching everything) when the
/// worker drops its sender.
fn sink_thread(rx: Receiver<SinkJob>) {
    // SAFETY: balanced with CoUninitialize below, on this thread.
    let init = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let mut advised: HashMap<TabKey, (IConnectionPoint, u32)> = HashMap::new();
    let unadvise = |(point, cookie): (IConnectionPoint, u32)| {
        // SAFETY: plain COM call with the cookie Advise returned; a dead
        // server just fails.
        let _ = unsafe { point.Unadvise(cookie) };
    };
    for job in rx {
        match job {
            SinkJob::Advise { tab, browser, make, failed } => {
                let start = std::time::Instant::now();
                match advise(&browser.0, &make()) {
                    Ok(a) => {
                        if let Some(old) = advised.insert(tab, a) {
                            unadvise(old);
                        }
                    }
                    // Also when explorer.exe died meanwhile: the source then
                    // tries itself, which fails just as fast.
                    Err(e) => failed(error(format!("attaching the tab sink (late): {}", e.message()))),
                }
                crate::diag::note(|| {
                    format!("sink: attach {:#x} {:.1} ms (late)", tab.hwnd, start.elapsed().as_secs_f64() * 1000.0)
                });
            }
            SinkJob::Detach { tab } => {
                if let Some(a) = advised.remove(&tab) {
                    unadvise(a);
                }
            }
        }
    }
    for (_, a) in advised.drain() {
        unadvise(a);
    }
    if init.is_ok() {
        // SAFETY: balances the successful CoInitializeEx above.
        unsafe { CoUninitialize() };
    }
}

fn worker(index: usize, rx: Receiver<Job>, late: bool) {
    use std::sync::mpsc::RecvTimeoutError;
    // SAFETY: balanced with CoUninitialize below, on this thread.
    let init = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    // Late sinks need a sink thread; without one, sinks are attached first.
    let sinks: Option<Sender<SinkJob>> = late
        .then(|| {
            let (tx, rx) = channel::<SinkJob>();
            std::thread::Builder::new()
                .name(format!("esr-sink-{index}"))
                .spawn(move || sink_thread(rx))
                .ok()
                .map(|_| tx)
        })
        .flatten();
    let mut held: HashMap<TabKey, Held> = HashMap::new();
    loop {
        let job = if held.values().any(|h| h.pending.is_some()) {
            match rx.recv_timeout(PENDING_SINK_WAIT) {
                Ok(job) => job,
                Err(RecvTimeoutError::Timeout) => {
                    for (tab, h) in held.iter_mut() {
                        h.attach_pending_now(*tab);
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match rx.recv() {
                Ok(job) => job,
                Err(_) => break,
            }
        };
        match job {
            Job::Attach { window, tab, browser, sink, failed } => {
                // The call releases the stream, whether it succeeds or not.
                let stream = std::mem::ManuallyDrop::new(browser.0);
                // SAFETY: the stream was made for this purpose and is consumed here.
                let browser: IWebBrowser2 = match unsafe { CoGetInterfaceAndReleaseStream(&*stream) } {
                    Ok(b) => b,
                    Err(e) => {
                        failed(error(format!("CoGetInterfaceAndReleaseStream: {}", e.message())));
                        continue;
                    }
                };
                if let Some(old) = held.remove(&tab) {
                    old.release(sinks.as_ref(), tab);
                }
                let mut h = Held { browser, window, advise: None, pending: None, late: false };
                match sink {
                    // Late sinks (task 16.3 round 4c, the user's decision):
                    // the sink is attached once the tab's navigation is
                    // issued, on the sink thread, while this worker posts the
                    // window's next new-tab command. 0.3–0.5 s faster for
                    // 3 x 10 tabs; the risk is an explorer.exe heap
                    // corruption (`0xc0000374`, once in about 35 runs in
                    // round 4b, as in round 3 when a new-tab command
                    // overlapped a call into the previous tab), handled as a
                    // failed restoration attempt.
                    Some(make) if sinks.is_some() => h.pending = Some((make, failed)),
                    // Otherwise it is attached before the tab's navigation is
                    // issued and before the window's next new-tab command.
                    Some(make) => {
                        h.pending = Some((make, failed));
                        h.attach_pending_now(tab);
                    }
                    None => {}
                }
                held.insert(tab, h);
            }
            Job::Navigate { tab, pidl, failed } => {
                let Some(h) = held.get_mut(&tab) else {
                    failed(error(format!("no browser for {tab:?}")));
                    continue;
                };
                let start = std::time::Instant::now();
                let r = crate::navigator::navigate_pidl(&h.browser, &pidl);
                crate::diag::note(|| {
                    format!("worker: navigate {:#x} {:.1} ms", tab.hwnd, start.elapsed().as_secs_f64() * 1000.0)
                });
                if let Err(e) = r {
                    failed(e);
                }
                // Hand the late sink over (also after a failed navigation:
                // the tab is tracked either way).
                if let (Some((make, sink_failed)), Some(tx)) = (h.pending.take(), sinks.as_ref()) {
                    let job = SinkJob::Advise { tab, browser: Agile(h.browser.clone()), make, failed: sink_failed };
                    match tx.send(job) {
                        Ok(()) => h.late = true,
                        // The sink thread is gone: attach it here.
                        Err(std::sync::mpsc::SendError(SinkJob::Advise { make, failed, .. })) => {
                            h.pending = Some((make, failed));
                            h.attach_pending_now(tab);
                        }
                        Err(_) => {}
                    }
                }
            }
            Job::Detach { tab } => {
                if let Some(h) = held.remove(&tab) {
                    h.release(sinks.as_ref(), tab);
                }
            }
            Job::PostCommand { window, command } => {
                use windows::Win32::Foundation::{LPARAM, WPARAM};
                use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_COMMAND};
                // A tab of this window whose sink is still pending (it was
                // not navigated) gets it first, as without late sinks.
                for (tab, h) in held.iter_mut().filter(|(_, h)| h.window == window) {
                    h.attach_pending_now(*tab);
                }
                // SAFETY: posting a plain message; a destroyed window just fails
                // (the navigator's re-post and timeout handle a lost command).
                let r = unsafe {
                    PostMessageW(Some(crate::keys::hwnd_of(window)), WM_COMMAND, WPARAM(command), LPARAM(0))
                };
                crate::diag::note(|| format!("worker: post {command:#x} to {window:#x} ok={}", r.is_ok()));
            }
        }
    }
    for (tab, h) in held.drain() {
        h.release(sinks.as_ref(), tab);
    }
    // The sink thread detaches its sinks and ends.
    drop(sinks);
    if init.is_ok() {
        // SAFETY: balances the successful CoInitializeEx above.
        unsafe { CoUninitialize() };
    }
}

fn advise(browser: &IWebBrowser2, sink: &IUnknown) -> windows::core::Result<(IConnectionPoint, u32)> {
    let container: IConnectionPointContainer = browser.cast()?;
    // SAFETY: plain COM calls on live interfaces; the cookie is kept for Unadvise.
    unsafe {
        let point = container.FindConnectionPoint(&DWebBrowserEvents2::IID)?;
        let cookie = point.Advise(sink)?;
        Ok((point, cookie))
    }
}

fn is_alive(hwnd: u64) -> bool {
    // SAFETY: IsWindow tolerates stale HWNDs.
    unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(crate::keys::hwnd_of(hwnd))) }.as_bool()
}

fn error(message: String) -> ShellError {
    ShellError { code: 0, message: format!("browser worker: {message}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_are_spread_over_the_workers_and_keep_theirs() {
        let mut r = Router::new(3);
        assert_eq!([r.route(10), r.route(20), r.route(30)], [0, 1, 2]);
        // Sticky: a window keeps its worker.
        assert_eq!(r.route(20), 1);
        assert_eq!(r.worker_of(30), Some(2));
        // The fourth window goes to the least loaded worker (lowest index on a tie).
        assert_eq!(r.route(40), 0);
        // A released window frees its slot for the next new window.
        r.release(20);
        assert_eq!(r.worker_of(20), None);
        assert_eq!(r.route(50), 1);
        // Releasing an unknown window changes nothing.
        r.release(999);
        assert_eq!(r.route(60), 1);
    }

    #[test]
    fn a_single_worker_takes_every_window() {
        let mut r = Router::new(1);
        assert_eq!((r.route(1), r.route(2)), (0, 0));
        r.release(1);
        r.release(2);
        assert_eq!(r.route(3), 0);
    }
}
