//! `CandidateHider`: hides and reveals Folding_Candidates with layered alpha 0
//! (tab folding, Req 14.14, 14.16; spike F1).
//!
//! # Why a thread of its own
//!
//! Setting the style of another thread's window waits until that thread
//! handles `WM_STYLECHANGING` (31–94 ms in F1), so the work never runs on the
//! Engine thread. [`CandidateHider`] owns one worker thread; [`hide`],
//! [`reveal`] and [`sweep`] only queue a job for it.
//!
//! # Behaviour
//!
//! - `hide`: adds `WS_EX_LAYERED` (if the window lacks it) and sets alpha 0
//!   with `SetLayeredWindowAttributes`. The window is remembered as hidden by
//!   this hider, together with whether *it* added the style. If the style
//!   change fails (for example an elevated Explorer window), the style is put
//!   back and [`Event::HideFailed`] is posted through the poster given at
//!   construction; the core logs `ERROR operation=fold-hide` and the fold goes
//!   on (Req 14.16).
//! - `reveal`: for a window this hider hid, restores alpha 255 and removes
//!   `WS_EX_LAYERED` again if the hider added it. A window this hider did not
//!   hide is left alone: the Restore_Engine's hidden windows are revealed by
//!   `Navigator::reveal`, which keeps its current meaning.
//! - `sweep`: reveals every top-level `CabinetWClass` that is layered with
//!   alpha 0, which recovers a candidate left invisible when the monitor
//!   crashed (Req 14.14). The runtime runs it once before the Engine starts.
//! - Dropping the hider reveals every window it still hides, so that stopping
//!   never leaves an invisible window behind, and lets the thread end.
//!
//! A window that no longer exists, or whose HWND was reused by a newer window
//! (its [`WinKey`] is no longer the current one in the shared registry), is
//! skipped silently: a candidate that closed itself is not an error.
//!
//! # Wiring (task 26.1)
//!
//! `esr-win` does not depend on `esr-app`, so the poster is a plain closure:
//!
//! ```ignore
//! let sink: SharedSink = ...;
//! let hider = CandidateHider::start(windows.clone(), Arc::new(move |e| sink.post(e)))?;
//! // `impl Hider for CandidateHider` in esr-app forwards to
//! // `hider.hide(win)`, `hider.reveal(win)` and `hider.sweep()`.
//! ```
//!
//! [`hide`]: CandidateHider::hide
//! [`reveal`]: CandidateHider::reveal
//! [`sweep`]: CandidateHider::sweep

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use esr_core::engine::{Event, ShellError, WinKey};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM};
use windows::Win32::Graphics::Gdi::{
    RDW_ALLCHILDREN, RDW_ERASE, RDW_FRAME, RDW_INVALIDATE, RedrawWindow,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GWL_EXSTYLE, GetWindowLongPtrW, IsWindow, LWA_ALPHA, SetLayeredWindowAttributes,
    SetWindowLongPtrW, WS_EX_LAYERED,
};
use windows::core::BOOL;

use crate::keys::{WindowKeys, hwnd_of, hwnd_value};
use crate::navigator::layered_alpha;
use crate::shell_util::{is_cabinet_class, window_class_name};

/// How long [`CandidateHider::sweep`] waits for the worker. A hung Explorer
/// window can hold up a style change; the sweep then reports what it has.
pub const SWEEP_TIMEOUT: Duration = Duration::from_secs(5);

/// Delivers an event to the Engine (the runtime maps it onto `EventSink::post`).
pub type EventPoster = Arc<dyn Fn(Event) + Send + Sync>;

// ---------------------------------------------------------------------------
// Pure decisions (no Windows calls)
// ---------------------------------------------------------------------------

/// `style` with `WS_EX_LAYERED` set.
pub fn style_with_layered(style: isize) -> isize {
    style | WS_EX_LAYERED.0 as isize
}

/// `style` with `WS_EX_LAYERED` cleared.
pub fn style_without_layered(style: isize) -> isize {
    style & !(WS_EX_LAYERED.0 as isize)
}

/// Whether `style` has `WS_EX_LAYERED`.
pub fn style_is_layered(style: isize) -> bool {
    style & WS_EX_LAYERED.0 as isize != 0
}

/// Whether the sweep reveals a window: a `CabinetWClass` that is layered
/// with alpha 0 (`alpha` is [`layered_alpha`]'s answer).
pub fn is_sweep_target(class: &str, alpha: Option<u8>) -> bool {
    is_cabinet_class(class) && alpha == Some(0)
}

/// The windows this hider hid, and whether it added `WS_EX_LAYERED` to each
/// (so [`reveal`](Self::take) removes it again only then).
#[derive(Debug, Default)]
pub struct HideLedger {
    hidden: HashMap<WinKey, bool>,
}

impl HideLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn contains(&self, win: WinKey) -> bool {
        self.hidden.contains_key(&win)
    }

    pub fn len(&self) -> usize {
        self.hidden.len()
    }

    pub fn is_empty(&self) -> bool {
        self.hidden.is_empty()
    }

    /// Records a hidden window. Returns false (and keeps the first record)
    /// if it is already recorded.
    pub fn record(&mut self, win: WinKey, added_layered: bool) -> bool {
        if self.hidden.contains_key(&win) {
            return false;
        }
        self.hidden.insert(win, added_layered);
        true
    }

    /// Forgets `win`. Returns `Some(added_layered)` if this hider hid it,
    /// `None` if it did not (then nothing is revealed).
    pub fn take(&mut self, win: WinKey) -> Option<bool> {
        self.hidden.remove(&win)
    }

    /// Forgets the windows for which `alive` is false.
    pub fn prune(&mut self, mut alive: impl FnMut(WinKey) -> bool) {
        self.hidden.retain(|win, _| alive(*win));
    }

    /// Forgets every window and returns them with their `added_layered` flag.
    pub fn drain(&mut self) -> Vec<(WinKey, bool)> {
        self.hidden.drain().collect()
    }
}

// ---------------------------------------------------------------------------
// The worker, over an abstract window backend
// ---------------------------------------------------------------------------

/// The window operations the worker needs. [`Win32Backend`] is the real one.
pub trait HiderBackend {
    /// Whether `win` still names a live window (not a gone or reused HWND).
    fn is_current(&self, win: WinKey) -> bool;
    /// Layered alpha 0. `Ok(true)` if the style was added, `Ok(false)` if the
    /// window was already layered. On `Err` the window is left as it was.
    fn make_transparent(&self, win: WinKey) -> Result<bool, ShellError>;
    /// Alpha 255, and `WS_EX_LAYERED` removed if `remove_layered`.
    fn make_opaque(&self, win: WinKey, remove_layered: bool) -> Result<(), ShellError>;
    /// Top-level `CabinetWClass` windows that are layered with alpha 0,
    /// as HWND values.
    fn sweep_targets(&self) -> Vec<u64>;
    /// Alpha 255 and `WS_EX_LAYERED` removed for a swept HWND.
    fn make_opaque_hwnd(&self, hwnd: u64) -> Result<(), ShellError>;
}

/// The result of a sweep.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Windows made visible again.
    pub revealed: u32,
    /// Windows that could not be revealed.
    pub failed: u32,
}

/// The hider's logic: a ledger over a backend. Runs on the hider thread.
pub struct HiderWorker<B> {
    backend: B,
    ledger: HideLedger,
    post: EventPoster,
}

impl<B: HiderBackend> HiderWorker<B> {
    pub fn new(backend: B, post: EventPoster) -> Self {
        Self { backend, ledger: HideLedger::new(), post }
    }

    pub fn ledger(&self) -> &HideLedger {
        &self.ledger
    }

    /// Hides `win`; posts `HideFailed` when the style change fails.
    pub fn hide(&mut self, win: WinKey) {
        let backend = &self.backend;
        self.ledger.prune(|w| backend.is_current(w));
        if self.ledger.contains(win) || !self.backend.is_current(win) {
            return;
        }
        match self.backend.make_transparent(win) {
            Ok(added) => {
                self.ledger.record(win, added);
            }
            Err(error) => (self.post)(Event::HideFailed { win, error }),
        }
    }

    /// Reveals `win` if this hider hid it; any other window is left alone.
    pub fn reveal(&mut self, win: WinKey) {
        let Some(added) = self.ledger.take(win) else { return };
        if self.backend.is_current(win) {
            // Best effort: nothing in the core waits on a reveal.
            let _ = self.backend.make_opaque(win, added);
        }
    }

    /// Reveals every `CabinetWClass` that is layered with alpha 0.
    pub fn sweep(&mut self) -> SweepReport {
        let mut report = SweepReport::default();
        for hwnd in self.backend.sweep_targets() {
            match self.backend.make_opaque_hwnd(hwnd) {
                Ok(()) => report.revealed += 1,
                Err(_) => report.failed += 1,
            }
        }
        // Whatever the sweep revealed is no longer hidden by this hider.
        let backend = &self.backend;
        self.ledger.prune(|w| backend.is_current(w));
        report
    }

    /// Reveals every window this hider still hides.
    pub fn reveal_all(&mut self) {
        for (win, added) in self.ledger.drain() {
            if self.backend.is_current(win) {
                let _ = self.backend.make_opaque(win, added);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The real backend
// ---------------------------------------------------------------------------

/// Win32 implementation of [`HiderBackend`].
pub struct Win32Backend {
    windows: WindowKeys,
}

impl Win32Backend {
    pub fn new(windows: WindowKeys) -> Self {
        Self { windows }
    }
}

impl HiderBackend for Win32Backend {
    fn is_current(&self, win: WinKey) -> bool {
        // SAFETY: IsWindow tolerates stale HWNDs.
        if !unsafe { IsWindow(Some(hwnd_of(win.hwnd))) }.as_bool() {
            return false;
        }
        // A newer window that reuses the HWND has another key. An HWND the
        // registry does not know yet is taken as the candidate itself.
        self.windows.get(win.hwnd).is_none_or(|current| current == win)
    }

    fn make_transparent(&self, win: WinKey) -> Result<bool, ShellError> {
        set_transparent(hwnd_of(win.hwnd))
    }

    fn make_opaque(&self, win: WinKey, remove_layered: bool) -> Result<(), ShellError> {
        set_opaque(hwnd_of(win.hwnd), remove_layered)
    }

    fn sweep_targets(&self) -> Vec<u64> {
        let mut found: Vec<u64> = Vec::new();
        // SAFETY: the callback only reads `found` through the LPARAM, which
        // outlives the synchronous EnumWindows call.
        let _ = unsafe { EnumWindows(Some(collect_sweep_target), LPARAM(&mut found as *mut Vec<u64> as isize)) };
        found
    }

    fn make_opaque_hwnd(&self, hwnd: u64) -> Result<(), ShellError> {
        set_opaque(hwnd_of(hwnd), true)
    }
}

unsafe extern "system" fn collect_sweep_target(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: `lparam` is the `&mut Vec<u64>` passed by `sweep_targets`.
    let found = unsafe { &mut *(lparam.0 as *mut Vec<u64>) };
    if is_sweep_target(&window_class_name(hwnd), layered_alpha(hwnd)) {
        found.push(hwnd_value(hwnd));
    }
    BOOL(1)
}

fn error(code: i32, message: String) -> ShellError {
    ShellError { code, message: format!("candidate hider: {message}") }
}

fn win_error(step: &str, e: &windows::core::Error) -> ShellError {
    error(e.code().0, format!("{step}: {}", e.message()))
}

/// Adds `WS_EX_LAYERED` if missing and sets alpha 0. Returns whether the style
/// was added. On failure the style is put back.
fn set_transparent(hwnd: HWND) -> Result<bool, ShellError> {
    // SAFETY: style and layered-attribute calls on a window handle; stale
    // HWNDs and windows of a higher integrity level just fail.
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let added = !style_is_layered(ex);
        if added {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style_with_layered(ex));
        }
        match SetLayeredWindowAttributes(hwnd, COLORREF(0), 0, LWA_ALPHA) {
            Ok(()) => Ok(added),
            Err(e) => {
                if added {
                    SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex);
                }
                Err(win_error("SetLayeredWindowAttributes(alpha 0)", &e))
            }
        }
    }
}

/// Alpha 255, then `WS_EX_LAYERED` removed if `remove_layered`. Succeeds when
/// the window ends up opaque.
fn set_opaque(hwnd: HWND, remove_layered: bool) -> Result<(), ShellError> {
    // SAFETY: as in `set_transparent`; RedrawWindow repaints after the style change.
    unsafe {
        let attrs = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA);
        if !remove_layered {
            return attrs.map_err(|e| win_error("SetLayeredWindowAttributes(alpha 255)", &e));
        }
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style_without_layered(ex));
        let _ = RedrawWindow(Some(hwnd), None, None, RDW_ERASE | RDW_INVALIDATE | RDW_FRAME | RDW_ALLCHILDREN);
        if style_is_layered(GetWindowLongPtrW(hwnd, GWL_EXSTYLE)) {
            return match attrs {
                Err(e) => Err(win_error("remove WS_EX_LAYERED", &e)),
                Ok(()) => Err(error(0, "remove WS_EX_LAYERED: the style stayed".into())),
            };
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The adapter
// ---------------------------------------------------------------------------

enum Job {
    Hide(WinKey),
    Reveal(WinKey),
    Sweep(Sender<SweepReport>),
    Stop,
}

/// The `CandidateHider` adapter (backs the `Hider` port in `esr-app`).
/// All methods only queue work for the hider thread and return at once,
/// except [`sweep`](Self::sweep), which waits for the result.
pub struct CandidateHider {
    jobs: Sender<Job>,
    thread: Option<JoinHandle<()>>,
}

impl CandidateHider {
    /// Starts the hider thread. `windows` is the registry shared with the
    /// other adapters; `post` receives `Event::HideFailed` (it is called on
    /// the hider thread).
    pub fn start(windows: WindowKeys, post: EventPoster) -> Result<Self, ShellError> {
        Self::start_with(Win32Backend::new(windows), post)
    }

    /// As [`start`](Self::start), over another backend.
    pub fn start_with<B: HiderBackend + Send + 'static>(backend: B, post: EventPoster) -> Result<Self, ShellError> {
        let (jobs, inbox) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("esr-candidate-hider".into())
            .spawn(move || run(HiderWorker::new(backend, post), inbox))
            .map_err(|e| error(e.raw_os_error().unwrap_or(0), format!("spawn failed: {e}")))?;
        Ok(Self { jobs, thread: Some(thread) })
    }

    fn queue(&self, job: Job) -> Result<(), ShellError> {
        self.jobs.send(job).map_err(|_| error(0, "the hider thread has stopped".into()))
    }

    /// Queues "hide `win`" (layered alpha 0). `Err` only if it could not be
    /// queued; a failure to hide is posted as `Event::HideFailed`.
    pub fn hide(&self, win: WinKey) -> Result<(), ShellError> {
        self.queue(Job::Hide(win))
    }

    /// Queues "reveal `win`" if this hider hid it (alpha 255, and
    /// `WS_EX_LAYERED` removed if the hider added it); other windows are left
    /// alone. Jobs run in the order queued, so a reveal after a hide always
    /// finds the window hidden.
    pub fn reveal(&self, win: WinKey) -> Result<(), ShellError> {
        self.queue(Job::Reveal(win))
    }

    /// Reveals every top-level `CabinetWClass` that is layered with alpha 0
    /// and waits for the result (at most [`SWEEP_TIMEOUT`]; `default` counts
    /// as nothing revealed).
    pub fn sweep_report(&self) -> SweepReport {
        let (reply, answer) = mpsc::channel();
        if self.queue(Job::Sweep(reply)).is_err() {
            return SweepReport::default();
        }
        answer.recv_timeout(SWEEP_TIMEOUT).unwrap_or_default()
    }

    /// [`sweep_report`](Self::sweep_report), as the number of windows revealed.
    pub fn sweep(&self) -> u32 {
        self.sweep_report().revealed
    }
}

impl Drop for CandidateHider {
    fn drop(&mut self) {
        let _ = self.jobs.send(Job::Stop);
        // Not joined: the thread may be waiting on a hung Explorer window and
        // must not hold up shutdown. It ends on its own after revealing.
        drop(self.thread.take());
    }
}

fn run<B: HiderBackend>(mut worker: HiderWorker<B>, inbox: Receiver<Job>) {
    while let Ok(job) = inbox.recv() {
        match job {
            Job::Hide(win) => worker.hide(win),
            Job::Reveal(win) => worker.reveal(win),
            Job::Sweep(reply) => {
                let _ = reply.send(worker.sweep());
            }
            Job::Stop => break,
        }
    }
    // Stopped, or every sender is gone: leave no invisible window behind.
    worker.reveal_all();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet};
    use std::rc::Rc;
    use std::sync::Mutex;

    fn w(hwnd: u64) -> WinKey {
        WinKey { hwnd, seq: 1 }
    }

    #[derive(Default)]
    struct FakeState {
        alive: BTreeSet<u64>,
        /// hwnd -> (layered, alpha)
        style: BTreeMap<u64, (bool, u8)>,
        /// HWNDs for which making transparent fails.
        refuse: BTreeSet<u64>,
        sweep_hwnds: Vec<u64>,
        opaque_calls: Vec<(u64, bool)>,
    }

    #[derive(Clone, Default)]
    struct Fake(Rc<RefCell<FakeState>>);

    impl Fake {
        fn window(&self, hwnd: u64, layered: bool) {
            let mut s = self.0.borrow_mut();
            s.alive.insert(hwnd);
            s.style.insert(hwnd, (layered, 255));
        }
    }

    impl HiderBackend for Fake {
        fn is_current(&self, win: WinKey) -> bool {
            self.0.borrow().alive.contains(&win.hwnd)
        }
        fn make_transparent(&self, win: WinKey) -> Result<bool, ShellError> {
            let mut s = self.0.borrow_mut();
            if s.refuse.contains(&win.hwnd) {
                return Err(ShellError { code: 5, message: "access denied".into() });
            }
            let st = s.style.get_mut(&win.hwnd).unwrap();
            let added = !st.0;
            *st = (true, 0);
            Ok(added)
        }
        fn make_opaque(&self, win: WinKey, remove_layered: bool) -> Result<(), ShellError> {
            let mut s = self.0.borrow_mut();
            s.opaque_calls.push((win.hwnd, remove_layered));
            let st = s.style.get_mut(&win.hwnd).unwrap();
            st.1 = 255;
            if remove_layered {
                st.0 = false;
            }
            Ok(())
        }
        fn sweep_targets(&self) -> Vec<u64> {
            self.0.borrow().sweep_hwnds.clone()
        }
        fn make_opaque_hwnd(&self, hwnd: u64) -> Result<(), ShellError> {
            let mut s = self.0.borrow_mut();
            if s.refuse.contains(&hwnd) {
                return Err(ShellError { code: 5, message: "access denied".into() });
            }
            s.style.insert(hwnd, (false, 255));
            Ok(())
        }
    }

    fn worker(fake: &Fake) -> (HiderWorker<Fake>, Arc<Mutex<Vec<Event>>>) {
        let posted = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&posted);
        let post: EventPoster = Arc::new(move |e| sink.lock().unwrap().push(e));
        (HiderWorker::new(fake.clone(), post), posted)
    }

    #[test]
    fn style_bits_toggle_only_the_layered_flag() {
        let base = 0x0004_0100isize;
        assert!(!style_is_layered(base));
        let on = style_with_layered(base);
        assert!(style_is_layered(on));
        assert_eq!(style_without_layered(on), base);
        assert_eq!(style_with_layered(on), on);
        assert_eq!(style_without_layered(base), base);
    }

    #[test]
    fn sweep_targets_are_layered_alpha_zero_cabinets() {
        assert!(is_sweep_target("CabinetWClass", Some(0)));
        assert!(!is_sweep_target("CabinetWClass", Some(255)));
        assert!(!is_sweep_target("CabinetWClass", Some(1)));
        assert!(!is_sweep_target("CabinetWClass", None));
        assert!(!is_sweep_target("#32770", Some(0)));
        assert!(!is_sweep_target("ShellTabWindowClass", Some(0)));
    }

    #[test]
    fn hide_then_reveal_restores_a_window_the_hider_made_layered() {
        let fake = Fake::default();
        fake.window(10, false);
        let (mut w1, posted) = worker(&fake);
        w1.hide(w(10));
        assert_eq!(fake.0.borrow().style[&10], (true, 0));
        assert!(w1.ledger().contains(w(10)));
        w1.reveal(w(10));
        assert_eq!(fake.0.borrow().style[&10], (false, 255));
        assert!(w1.ledger().is_empty());
        assert!(posted.lock().unwrap().is_empty());
    }

    #[test]
    fn reveal_keeps_the_layered_style_it_did_not_add() {
        let fake = Fake::default();
        fake.window(10, true);
        let (mut w1, _) = worker(&fake);
        w1.hide(w(10));
        w1.reveal(w(10));
        assert_eq!(fake.0.borrow().style[&10], (true, 255));
        assert_eq!(fake.0.borrow().opaque_calls, vec![(10, false)]);
    }

    #[test]
    fn reveal_of_a_window_the_hider_did_not_hide_does_nothing() {
        let fake = Fake::default();
        fake.window(10, true);
        fake.0.borrow_mut().style.insert(10, (true, 0)); // hidden by the Restore_Engine
        let (mut w1, _) = worker(&fake);
        w1.reveal(w(10));
        assert_eq!(fake.0.borrow().style[&10], (true, 0));
        assert!(fake.0.borrow().opaque_calls.is_empty());
    }

    #[test]
    fn failed_hide_posts_hide_failed_and_records_nothing() {
        let fake = Fake::default();
        fake.window(10, false);
        fake.0.borrow_mut().refuse.insert(10);
        let (mut w1, posted) = worker(&fake);
        w1.hide(w(10));
        assert!(w1.ledger().is_empty());
        let events = posted.lock().unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            Event::HideFailed { win, error } => {
                assert_eq!(*win, w(10));
                assert_eq!(error.code, 5);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_gone_window_is_skipped_without_an_error() {
        let fake = Fake::default();
        let (mut w1, posted) = worker(&fake);
        w1.hide(w(99));
        assert!(w1.ledger().is_empty());
        assert!(posted.lock().unwrap().is_empty());
    }

    #[test]
    fn hiding_twice_keeps_the_first_added_flag() {
        let fake = Fake::default();
        fake.window(10, false);
        let (mut w1, _) = worker(&fake);
        w1.hide(w(10));
        w1.hide(w(10)); // already layered now, but the first record stands
        w1.reveal(w(10));
        assert_eq!(fake.0.borrow().style[&10], (false, 255));
    }

    #[test]
    fn destroyed_windows_leave_the_ledger() {
        let fake = Fake::default();
        fake.window(10, false);
        fake.window(11, false);
        let (mut w1, _) = worker(&fake);
        w1.hide(w(10));
        fake.0.borrow_mut().alive.remove(&10);
        w1.hide(w(11));
        assert!(!w1.ledger().contains(w(10)));
        assert!(w1.ledger().contains(w(11)));
    }

    #[test]
    fn sweep_counts_revealed_and_failed_windows() {
        let fake = Fake::default();
        {
            let mut s = fake.0.borrow_mut();
            s.sweep_hwnds = vec![1, 2, 3];
            s.refuse.insert(2);
        }
        let (mut w1, _) = worker(&fake);
        assert_eq!(w1.sweep(), SweepReport { revealed: 2, failed: 1 });
    }

    #[test]
    fn reveal_all_reveals_what_is_still_hidden() {
        let fake = Fake::default();
        fake.window(10, false);
        fake.window(11, false);
        let (mut w1, _) = worker(&fake);
        w1.hide(w(10));
        w1.hide(w(11));
        w1.reveal_all();
        assert_eq!(fake.0.borrow().style[&10], (false, 255));
        assert_eq!(fake.0.borrow().style[&11], (false, 255));
        assert!(w1.ledger().is_empty());
    }

    /// A thread-safe backend for the threaded adapter.
    #[derive(Clone, Default)]
    struct SyncFake(Arc<Mutex<BTreeMap<u64, (bool, u8)>>>);

    impl HiderBackend for SyncFake {
        fn is_current(&self, win: WinKey) -> bool {
            self.0.lock().unwrap().contains_key(&win.hwnd)
        }
        fn make_transparent(&self, win: WinKey) -> Result<bool, ShellError> {
            if win.hwnd == 666 {
                return Err(ShellError { code: 5, message: "denied".into() });
            }
            let mut m = self.0.lock().unwrap();
            let st = m.get_mut(&win.hwnd).unwrap();
            let added = !st.0;
            *st = (true, 0);
            Ok(added)
        }
        fn make_opaque(&self, win: WinKey, remove_layered: bool) -> Result<(), ShellError> {
            let mut m = self.0.lock().unwrap();
            let st = m.get_mut(&win.hwnd).unwrap();
            st.1 = 255;
            if remove_layered {
                st.0 = false;
            }
            Ok(())
        }
        fn sweep_targets(&self) -> Vec<u64> {
            let m = self.0.lock().unwrap();
            m.iter().filter(|(_, st)| **st == (true, 0)).map(|(h, _)| *h).collect()
        }
        fn make_opaque_hwnd(&self, hwnd: u64) -> Result<(), ShellError> {
            self.0.lock().unwrap().insert(hwnd, (false, 255));
            Ok(())
        }
    }

    #[test]
    fn the_thread_runs_jobs_in_order_and_posts_failures() {
        let fake = SyncFake::default();
        {
            let mut m = fake.0.lock().unwrap();
            m.insert(10, (false, 255));
            m.insert(666, (false, 255));
        }
        let posted = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&posted);
        let hider = CandidateHider::start_with(fake.clone(), Arc::new(move |e| sink.lock().unwrap().push(e))).unwrap();
        hider.hide(w(10)).unwrap();
        hider.hide(w(666)).unwrap();
        // The sweep runs after the hide jobs and finds the hidden window.
        assert_eq!(hider.sweep(), 1);
        assert_eq!(fake.0.lock().unwrap()[&10], (false, 255));
        let events = posted.lock().unwrap();
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(matches!(events[0], Event::HideFailed { win, .. } if win == w(666)));
    }

    #[test]
    fn dropping_the_hider_reveals_what_it_still_hides() {
        let fake = SyncFake::default();
        fake.0.lock().unwrap().insert(10, (false, 255));
        let hider = CandidateHider::start_with(fake.clone(), Arc::new(|_| {})).unwrap();
        hider.hide(w(10)).unwrap();
        drop(hider);
        for _ in 0..200 {
            if fake.0.lock().unwrap()[&10] == (false, 255) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("window still hidden: {:?}", fake.0.lock().unwrap());
    }

    #[test]
    fn win32_transitions_on_a_plain_window() {
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WS_POPUP,
        };
        use windows::core::w;

        // A never-shown popup of the predefined STATIC class, owned by this thread.
        // SAFETY: valid predefined class name; no parent, menu, or params.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("esr-hider-test"),
                WS_POPUP,
                10,
                10,
                100,
                100,
                None,
                None,
                None,
                None,
            )
        }
        .expect("create window");

        assert_eq!(layered_alpha(hwnd), None);
        assert!(set_transparent(hwnd).unwrap());
        assert_eq!(layered_alpha(hwnd), Some(0));
        // Already layered: the second hide does not claim to have added the style.
        assert!(!set_transparent(hwnd).unwrap());
        set_opaque(hwnd, false).unwrap();
        assert_eq!(layered_alpha(hwnd), Some(255));
        set_transparent(hwnd).unwrap();
        set_opaque(hwnd, true).unwrap();
        assert_eq!(layered_alpha(hwnd), None);

        // SAFETY: we own the window and destroy it on the creating thread.
        unsafe { DestroyWindow(hwnd) }.expect("destroy window");
        // A destroyed window is not current and cannot be hidden.
        let backend = Win32Backend::new(WindowKeys::new());
        assert!(!backend.is_current(WinKey { hwnd: hwnd_value(hwnd), seq: 1 }));
        assert!(set_transparent(hwnd).is_err());
    }

    /// Creates a never-shown popup of the predefined STATIC class.
    fn plain_window() -> HWND {
        use windows::Win32::UI::WindowsAndMessaging::{CreateWindowExW, WINDOW_EX_STYLE, WS_POPUP};
        use windows::core::w;
        // SAFETY: valid predefined class name; no parent, menu, or params.
        unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("esr-hider-test"),
                WS_POPUP,
                10,
                10,
                100,
                100,
                None,
                None,
                None,
                None,
            )
        }
        .expect("create window")
    }

    #[test]
    fn win32_backend_does_not_touch_a_window_whose_hwnd_was_reused() {
        use windows::Win32::UI::WindowsAndMessaging::DestroyWindow;

        let hwnd = plain_window();
        let value = hwnd_value(hwnd);
        let keys = WindowKeys::new();
        let old = keys.key_of(value);
        let backend = Win32Backend::new(keys.clone());
        assert!(backend.is_current(old));

        // The registry saw the HWND destroyed and a new window take its value.
        keys.destroyed(value);
        let newer = keys.key_of(value);
        assert_ne!(old, newer);
        assert!(!backend.is_current(old), "the old key no longer names the window");
        assert!(backend.is_current(newer));

        // An HWND the registry does not know yet is taken as the candidate itself.
        let unknown = WinKey { hwnd: value, seq: 99 };
        keys.destroyed(value);
        assert!(backend.is_current(unknown));

        // A worker over the real backend skips the stale key silently.
        let (post, posted) = {
            let posted = Arc::new(Mutex::new(Vec::new()));
            let sink = Arc::clone(&posted);
            let post: EventPoster = Arc::new(move |e| sink.lock().unwrap().push(e));
            (post, posted)
        };
        keys.key_of(value);
        let mut worker = HiderWorker::new(Win32Backend::new(keys), post);
        worker.hide(old);
        assert!(worker.ledger().is_empty());
        assert_eq!(layered_alpha(hwnd), None, "the window was left alone");
        assert!(posted.lock().unwrap().is_empty());

        // SAFETY: we own the window and destroy it on the creating thread.
        unsafe { DestroyWindow(hwnd) }.expect("destroy window");
    }

    /// The hider thread changes the style of a window owned by another thread
    /// (which must pump messages, as an Explorer window does): hide sets alpha 0,
    /// reveal restores the window, and dropping the hider reveals what is left.
    #[test]
    fn the_hider_thread_hides_and_reveals_a_window_of_another_thread() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use windows::Win32::UI::WindowsAndMessaging::{
            DestroyWindow, DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
        };

        fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
            for _ in 0..500 {
                if done() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("timed out waiting for {what}");
        }

        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel();
        let owner = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let hwnd = plain_window();
                ready_tx.send(hwnd_value(hwnd)).unwrap();
                while !stop.load(Ordering::SeqCst) {
                    let mut msg = MSG::default();
                    // SAFETY: standard message pump on the window's own thread.
                    unsafe {
                        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                            let _ = TranslateMessage(&msg);
                            DispatchMessageW(&msg);
                        }
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                // SAFETY: our window, destroyed on its thread.
                let _ = unsafe { DestroyWindow(hwnd) };
            })
        };
        let value = ready_rx.recv_timeout(Duration::from_secs(5)).expect("window created");
        let hwnd = hwnd_of(value);

        let keys = WindowKeys::new();
        let win = keys.key_of(value);
        let posted = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&posted);
        let hider = CandidateHider::start(keys, Arc::new(move |e| sink.lock().unwrap().push(e))).unwrap();

        assert_eq!(layered_alpha(hwnd), None);
        hider.hide(win).unwrap();
        wait_for("alpha 0", || layered_alpha(hwnd) == Some(0));
        hider.reveal(win).unwrap();
        wait_for("the style to be removed", || layered_alpha(hwnd).is_none());

        // Hide again, then drop: the hider leaves no invisible window behind.
        hider.hide(win).unwrap();
        wait_for("alpha 0 again", || layered_alpha(hwnd) == Some(0));
        drop(hider);
        wait_for("the reveal on drop", || layered_alpha(hwnd).is_none());
        assert!(posted.lock().unwrap().is_empty(), "{:?}", posted.lock().unwrap());

        stop.store(true, Ordering::SeqCst);
        owner.join().unwrap();
    }
}
