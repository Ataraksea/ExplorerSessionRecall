//! `WindowEventSource`: out-of-context WinEvent hooks filtered to top-level
//! `CabinetWClass` windows, and Explorer process-exit waits.
//!
//! # Hooks
//!
//! | Hook | Scope | Signal |
//! |---|---|---|
//! | `EVENT_OBJECT_CREATE..EVENT_OBJECT_HIDE` (create, destroy, show, hide) | global | [`WindowSignal::Changed`] (`Created`/`Shown`/`Hidden`), [`WindowSignal::Destroyed`] |
//! | `EVENT_SYSTEM_FOREGROUND` | global | [`WindowSignal::Activated`] |
//! | `EVENT_SYSTEM_MOVESIZEEND` | per Explorer PID | `Changed` (`MoveSizeEnd`) |
//! | `EVENT_SYSTEM_MINIMIZESTART..END` | per Explorer PID | `Changed` (`MinimizeStart`/`MinimizeEnd`) |
//! | `EVENT_OBJECT_LOCATIONCHANGE..NAMECHANGE` | per Explorer PID | `Changed` (`Location`/`Name`) |
//!
//! Only events with `OBJID_WINDOW` / `CHILDID_SELF` for a top-level window of
//! the watched class (`CabinetWClass`, see [`WindowEventConfig`]) pass. The
//! check is `GetClassNameW` + `GetAncestor(GA_ROOT)`, which keeps the global
//! hooks cheap. A destroyed HWND can no longer be queried, so
//! `EVENT_OBJECT_DESTROY` passes exactly when the HWND has a live key in the
//! shared [`WindowKeys`] registry (every window that passed the filter got
//! one). Keys come from that registry, which the window scanner shares, so a
//! reused HWND value gets a new [`WinKey::seq`] after its destroy.
//!
//! `Changed` carries the window's PID; the caller calls
//! [`watch_process`](WindowEventSource::watch_process) for PIDs it has not seen
//! (separate-process Explorer windows, Req 2.5).
//!
//! # Process exit (Req 3.3)
//!
//! `watch_process(pid)` installs the per-PID hooks and registers a
//! `RegisterWaitForSingleObject` wait on an `OpenProcess(SYNCHRONIZE)` handle.
//! The wait callback runs on a thread-pool thread and only posts a message to
//! the source's message-only window. On that message the source starts a
//! timer, and after [`PROCESS_EXIT_DELAY_MS`] (so the WinEvents already queued
//! for that process are delivered first) it removes the PID's hooks and wait
//! and emits [`WindowSignal::ProcessExited`].
//!
//! # Threading
//!
//! Out-of-context WinEvent callbacks are delivered to the thread that
//! installed the hooks, while it retrieves messages. Hooks must also be
//! removed from that thread. The source is therefore bound to one thread (it
//! is `!Send`): create it, call [`start`](WindowEventSource::start),
//! `watch_process`, and [`stop`](WindowEventSource::stop) on the Engine thread,
//! which must pump messages. The callbacks reach the source through a
//! thread-local, so only one source can be started per thread. The handler is
//! called on that thread, from inside the message pump (never while the source
//! holds an internal borrow), so it may call back into the source.
//!
//! # Tab folding: first signals (Req 14.2, 14.4, 14.22)
//!
//! With an [event sink](WindowEventSource::set_event_sink) set, the source also
//! reports each new top-level `CabinetWClass` once, at its *first signal*: the
//! first WinEvent (any of the hooks above except `DESTROY`) whose window or
//! root window (`GetAncestor(GA_ROOT)`) is a watched-class window it has not
//! seen. That is a child's `CREATE` for `explorer.exe <path>` or the cabinet's
//! own `LOCATIONCHANGE` for `ShellExecute`; the window's own `CREATE` comes
//! 0.16-1.4 s later (F1). The sink receives
//! `Event::WindowAppeared { win, pid, bypass_keys, desktop, foreground }`
//! *before* the [`WindowSignal`] the same WinEvent produces.
//!
//! * `bypass_keys`: Ctrl and Shift both down (`GetAsyncKeyState`), read first.
//! * `desktop`: [`desktop_view`](crate::desktops::desktop_view) of the
//!   candidate among the live windows of the shared registry.
//! * `foreground`: the Explorer window that was foreground before the
//!   candidate took the foreground, from `GetForegroundWindow` and a short
//!   history of the global `EVENT_SYSTEM_FOREGROUND` events of any class
//!   ([`first_signal::foreground_before`]).
//!
//! Windows open when [`start`](WindowEventSource::start) runs are marked seen
//! and never reported. The pure decisions live in [`first_signal`] and are
//! tested without Windows.
//!
//! # Tab folding: detached tabs (Req 14.3 b)
//!
//! With an event sink set, the `DESTROY` of a `ShellTabWindowClass` window
//! that belongs to a known window is posted as `Event::TabDetached { from }`
//! (drag-out evidence: the dragged tab's window is destroyed and recreated in
//! the new window). The destroyed child can no longer be asked for its parent,
//! so the source keeps a [tab-to-window map](tab_detach::TabWindows), filled
//! by the window scanner from the tab children it sees on every scan
//! ([`WindowScanner::set_tab_windows`](crate::scanner::WindowScanner::set_tab_windows)
//! with [`WindowEventSource::tab_windows`]). The event is posted only if the
//! owning window still has a live key. The pure decision is
//! [`tab_detach::tab_detached_event`].
//!
//! Requirements 2.3, 2.5, 2.6, 3.3, 14.2, 14.3, 14.4, 14.22.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::rc::{Rc, Weak};

use esr_core::engine::{DesktopView, Event, ShellError, WinKey};
use esr_core::model::{Display, DisplayRef, Placement, Rect, ShowState};
use esr_core::plan::placement::workspace_to_screen;
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, HWND, INVALID_HANDLE_VALUE, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    INFINITE, OpenProcess, PROCESS_SYNCHRONIZE, RegisterWaitForSingleObject, UnregisterWaitEx,
    WT_EXECUTEONLYONCE,
};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CHILDID_SELF, CreateWindowExW, DefWindowProcW, DestroyWindow, EVENT_OBJECT_CREATE,
    EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE, EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_NAMECHANGE,
    EVENT_OBJECT_SHOW, EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND,
    EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MOVESIZEEND, GA_ROOT, GetAncestor, GetClassNameW,
    EnumWindows, GetForegroundWindow, GetWindowPlacement, GetWindowThreadProcessId, HWND_MESSAGE,
    IsWindow, KillTimer, OBJID_WINDOW, PostMessageW, RegisterClassW, SHOW_WINDOW_CMD, SW_FORCEMINIMIZE, SW_MINIMIZE, SW_SHOWMAXIMIZED,
    SW_SHOWMINIMIZED, SW_SHOWMINNOACTIVE, SetTimer, WINDOW_EX_STYLE, WINDOW_STYLE, WINDOWPLACEMENT,
    WINEVENT_OUTOFCONTEXT, WM_APP, WM_TIMER, WNDCLASSW, WPF_RESTORETOMAXIMIZED,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL, VK_SHIFT};
use windows::core::{BOOL, PCWSTR, w};

use crate::desktops::VirtualDesktops;
use crate::display_info::display_for_window;
use crate::keys::{WindowKeys, hwnd_of, hwnd_value};
use crate::shell_util::CABINET_WINDOW_CLASS;

/// Delay between the process handle being signalled and `ProcessExited`.
pub const PROCESS_EXIT_DELAY_MS: u32 = 200;

/// Global hook ranges: create/destroy/show/hide, and foreground.
const GLOBAL_HOOKS: [(u32, u32); 2] = [
    (EVENT_OBJECT_CREATE, EVENT_OBJECT_HIDE),
    (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
];

/// Per-PID hook ranges: move/size end, minimize start/end (and
/// [`LOCATION_HOOK`]).
const PROCESS_HOOKS: [(u32, u32); 2] = [
    (EVENT_SYSTEM_MOVESIZEEND, EVENT_SYSTEM_MOVESIZEEND),
    (EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND),
];

/// Per-PID location and name changes. Explorer raises these for every
/// element of every window (~9,500 for a 3 x 10 restoration), so the hook
/// can be paused while a restoration runs
/// ([`WindowEventSource::pause_location_hooks`]).
const LOCATION_HOOK: (u32, u32) = (EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_NAMECHANGE);

/// Posted by the wait callback: `wParam` = PID whose handle was signalled.
const WM_PROCESS_SIGNALLED: u32 = WM_APP + 0x51;

const MESSAGE_WINDOW_CLASS: PCWSTR = w!("ESR.WindowEventSource");

/// What changed about a watched window (all map to `EventSink::window_changed`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChangeCause {
    Created,
    Shown,
    Hidden,
    Location,
    MinimizeStart,
    MinimizeEnd,
    MoveSizeEnd,
    Name,
}

/// A filtered window signal (mapped onto the `WindowEvents` port in `esr-app`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WindowSignal {
    /// `window_changed(win)`. `pid` owns the window.
    Changed {
        win: WinKey,
        pid: u32,
        cause: ChangeCause,
    },
    /// `Event::WindowDestroyed`.
    Destroyed { win: WinKey },
    /// `Event::WindowActivated` (foreground). `pid` owns the window.
    Activated { win: WinKey, pid: u32 },
    /// `Event::ProcessExited`, [`PROCESS_EXIT_DELAY_MS`] after the exit.
    ProcessExited { pid: u32 },
}

/// Receives the source's signals on the source's thread.
pub trait WindowEventHandler {
    fn on_event(&self, signal: WindowSignal);
}

impl<F: Fn(WindowSignal)> WindowEventHandler for F {
    fn on_event(&self, signal: WindowSignal) {
        self(signal)
    }
}

/// Source configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowEventConfig {
    /// Class of the watched top-level windows (`CabinetWClass`; tests use their own).
    pub window_class: String,
    /// Delay before `ProcessExited` ([`PROCESS_EXIT_DELAY_MS`]).
    pub exit_delay_ms: u32,
}

impl Default for WindowEventConfig {
    fn default() -> Self {
        Self {
            window_class: CABINET_WINDOW_CLASS.to_owned(),
            exit_delay_ms: PROCESS_EXIT_DELAY_MS,
        }
    }
}

/// The `WindowEventSource` adapter (backs the `WindowEvents` port in `esr-app`).
/// Bound to the thread that created it; see the module docs.
pub struct WindowEventSource {
    inner: Rc<Inner>,
}

struct Inner {
    class: Vec<u16>,
    exit_delay_ms: u32,
    keys: WindowKeys,
    handler: Rc<dyn WindowEventHandler>,
    state: RefCell<State>,
    /// Tab folding (Req 14): see [`FoldState`].
    fold: FoldState,
}

#[derive(Default)]
struct State {
    started: bool,
    message_window: Option<HWND>,
    global_hooks: Vec<HWINEVENTHOOK>,
    processes: HashMap<u32, ProcessWatch>,
    /// [`LOCATION_HOOK`] is paused.
    location_paused: bool,
}

/// Hooks and exit wait of one Explorer PID.
struct ProcessWatch {
    hooks: Vec<HWINEVENTHOOK>,
    /// The [`LOCATION_HOOK`], unless paused.
    location: Option<HWINEVENTHOOK>,
    process: HANDLE,
    wait: HANDLE,
    /// When the delayed `ProcessExited` is due (set when the wait fires). A
    /// USER timer can fire up to one clock tick early, so the timer is
    /// re-armed for the rest until this instant has passed.
    exit_due: Option<std::time::Instant>,
    /// Read by the thread-pool wait callback; freed only after the wait is
    /// unregistered with completion (see [`ProcessWatch::release`]).
    _context: Box<WaitContext>,
}

struct WaitContext {
    window: HWND,
    pid: u32,
}

thread_local! {
    static ACTIVE: RefCell<Option<Weak<Inner>>> = const { RefCell::new(None) };
}

fn active_source() -> Option<Rc<Inner>> {
    ACTIVE
        .try_with(|a| {
            a.try_borrow()
                .ok()
                .and_then(|a| a.as_ref().and_then(Weak::upgrade))
        })
        .ok()
        .flatten()
}

fn set_active(inner: Option<&Rc<Inner>>) {
    let _ = ACTIVE.try_with(|a| *a.borrow_mut() = inner.map(Rc::downgrade));
}

impl WindowEventSource {
    pub fn new(
        config: WindowEventConfig,
        keys: WindowKeys,
        handler: Rc<dyn WindowEventHandler>,
    ) -> Self {
        Self {
            inner: Rc::new(Inner {
                class: config.window_class.encode_utf16().collect(),
                exit_delay_ms: config.exit_delay_ms,
                keys,
                handler,
                state: RefCell::new(State::default()),
                fold: FoldState::default(),
            }),
        }
    }

    /// The registry the source mints window keys from.
    pub fn keys(&self) -> &WindowKeys {
        &self.inner.keys
    }

    /// Sets where the tab-folding events (`WindowAppeared`) are posted
    /// (`EventSink::post` in `esr-app`). Called on the source's thread, from
    /// inside the message pump, never while the source holds a borrow. Without
    /// a sink the source reports no first signals.
    pub fn set_event_sink(&self, sink: EventPost) {
        *self.inner.fold.sink.borrow_mut() = Some(sink);
    }

    /// The tab-to-window map behind `TabDetached`. Hand it to the window
    /// scanner ([`WindowScanner::set_tab_windows`](crate::scanner::WindowScanner::set_tab_windows)),
    /// which refreshes it on every scan.
    pub fn tab_windows(&self) -> tab_detach::TabWindows {
        self.inner.fold.tabs.clone()
    }

    pub fn is_started(&self) -> bool {
        self.inner.state.borrow().started
    }

    /// Install the global hooks. Must be called on a thread that pumps
    /// messages; signals are delivered there. Idempotent. Fails if another
    /// source is started on this thread.
    pub fn start(&mut self) -> Result<(), ShellError> {
        if self.is_started() {
            return Ok(());
        }
        if active_source().is_some() {
            return Err(error(
                0,
                "another WindowEventSource is started on this thread".into(),
            ));
        }
        let window = create_message_window()?;
        {
            let mut st = self.inner.state.borrow_mut();
            st.started = true;
            st.message_window = Some(window);
        }
        set_active(Some(&self.inner));
        self.inner.fold_started();
        for (min, max) in GLOBAL_HOOKS {
            if crate::diag::disabled(crate::diag::Switch::GlobalHooks) {
                break;
            }
            match install_hook(min, max, 0) {
                Ok(hook) => self.inner.state.borrow_mut().global_hooks.push(hook),
                Err(e) => {
                    self.stop();
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    /// Install the per-PID hooks and the exit wait for an Explorer PID.
    /// Idempotent while the PID is watched. Requires [`start`](Self::start).
    pub fn watch_process(&mut self, pid: u32) -> Result<(), ShellError> {
        if pid == 0 {
            return Err(error(0, "watch_process: PID 0".into()));
        }
        let window = {
            let st = self.inner.state.borrow();
            if st.processes.contains_key(&pid) {
                return Ok(());
            }
            match (st.started, st.message_window) {
                (true, Some(window)) => window,
                _ => return Err(error(0, "watch_process: source not started".into())),
            }
        };
        // SAFETY: plain handle-returning call; the handle is owned by the watch.
        let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }
            .map_err(|e| win_error(&format!("OpenProcess({pid})"), &e))?;
        let mut watch = ProcessWatch {
            hooks: Vec::new(),
            location: None,
            process,
            wait: HANDLE::default(),
            exit_due: None,
            _context: Box::new(WaitContext { window, pid }),
        };
        for (min, max) in PROCESS_HOOKS {
            if crate::diag::disabled(crate::diag::Switch::PidHooks) {
                break;
            }
            match install_hook(min, max, pid) {
                Ok(hook) => watch.hooks.push(hook),
                Err(e) => {
                    watch.release();
                    return Err(e);
                }
            }
        }
        if !self.inner.state.borrow().location_paused && !crate::diag::disabled(crate::diag::Switch::PidHooks) {
            match install_hook(LOCATION_HOOK.0, LOCATION_HOOK.1, pid) {
                Ok(hook) => watch.location = Some(hook),
                Err(e) => {
                    watch.release();
                    return Err(e);
                }
            }
        }
        let context: *const WaitContext = &*watch._context;
        // SAFETY: `context` stays valid until `release` unregisters the wait
        // and waits for running callbacks; the callback only reads it.
        let registered = unsafe {
            RegisterWaitForSingleObject(
                &mut watch.wait,
                process,
                Some(process_signalled),
                Some(context.cast::<c_void>()),
                INFINITE,
                WT_EXECUTEONLYONCE,
            )
        };
        if let Err(e) = registered {
            watch.wait = HANDLE::default();
            watch.release();
            return Err(win_error("RegisterWaitForSingleObject", &e));
        }
        self.inner.state.borrow_mut().processes.insert(pid, watch);
        Ok(())
    }

    /// Pauses (or resumes) the per-PID `LOCATION_HOOK` of every watched
    /// process, and of processes watched later. A restoration pauses it
    /// (task 16.3: the events kept the Engine thread busy and slowed Explorer
    /// down); the windows it built are rescanned when it finishes, and the
    /// caller rescans every window on resume. A hook that cannot be
    /// reinstalled is retried at the next resume.
    pub fn pause_location_hooks(&mut self, paused: bool) {
        let mut st = self.inner.state.borrow_mut();
        if st.location_paused == paused {
            return;
        }
        st.location_paused = paused;
        for (&pid, watch) in st.processes.iter_mut() {
            match (paused, watch.location) {
                (true, Some(hook)) => {
                    // SAFETY: installed on this thread by this source.
                    let _ = unsafe { UnhookWinEvent(hook) };
                    watch.location = None;
                }
                (false, None) if !crate::diag::disabled(crate::diag::Switch::PidHooks) => {
                    watch.location = install_hook(LOCATION_HOOK.0, LOCATION_HOOK.1, pid).ok();
                }
                _ => {}
            }
        }
    }

    /// Whether `pid` has hooks and an exit wait installed.
    pub fn is_watching(&self, pid: u32) -> bool {
        self.inner.state.borrow().processes.contains_key(&pid)
    }

    /// Remove every hook and wait. Pending `ProcessExited` signals are
    /// dropped. Idempotent.
    pub fn stop(&mut self) {
        let (hooks, processes, window) = {
            let mut st = self.inner.state.borrow_mut();
            st.started = false;
            (
                std::mem::take(&mut st.global_hooks),
                std::mem::take(&mut st.processes),
                st.message_window.take(),
            )
        };
        if active_source().is_some_and(|a| Rc::ptr_eq(&a, &self.inner)) {
            set_active(None);
        }
        self.inner.fold_stopped();
        for hook in hooks {
            // SAFETY: installed by this source on this thread.
            let _ = unsafe { UnhookWinEvent(hook) };
        }
        for (_, watch) in processes {
            watch.release();
        }
        if let Some(window) = window {
            // SAFETY: created by this source on this thread; its timers go with it.
            let _ = unsafe { DestroyWindow(window) };
        }
    }

    /// Current Window_Placement of `win` in screen coordinates (physical px).
    /// Fails if `win` is not the live key of its HWND.
    pub fn placement(&self, win: WinKey) -> Result<Placement, ShellError> {
        if self.inner.keys.get(win.hwnd) != Some(win) {
            return Err(error(0, format!("placement: {win:?} is not a live window")));
        }
        window_placement(hwnd_of(win.hwnd))
    }
}

impl Drop for WindowEventSource {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Inner {
    fn owns_window(&self, window: HWND) -> bool {
        self.state.borrow().message_window == Some(window)
    }

    fn on_win_event(&self, event: u32, hwnd: HWND, id_object: i32, id_child: i32) {
        if self.fold.building.get() {
            // Reentered from a COM call made while building a `WindowAppeared`.
            self.fold.deferred.borrow_mut().push_back((event, hwnd, id_object, id_child));
            return;
        }
        self.dispatch_win_event(event, hwnd, id_object, id_child);
        loop {
            let next = self.fold.deferred.borrow_mut().pop_front();
            let Some((event, hwnd, id_object, id_child)) = next else { break };
            self.dispatch_win_event(event, hwnd, id_object, id_child);
        }
    }

    fn dispatch_win_event(&self, event: u32, hwnd: HWND, id_object: i32, id_child: i32) {
        // Tab folding: a first signal is posted before the signal of the same event.
        self.report_first_signal(event, hwnd, id_object, id_child);
        self.report_tab_detached(event, hwnd, id_object, id_child);
        if let Some(signal) = self.signal_for(event, hwnd, id_object, id_child) {
            self.handler.on_event(signal);
        }
    }

    /// The filter: `OBJID_WINDOW`/`CHILDID_SELF`, a known HWND for destroy, a
    /// top-level window of the watched class otherwise.
    fn signal_for(
        &self,
        event: u32,
        hwnd: HWND,
        id_object: i32,
        id_child: i32,
    ) -> Option<WindowSignal> {
        if id_object != OBJID_WINDOW.0 || id_child != CHILDID_SELF as i32 || hwnd.is_invalid() {
            return None;
        }
        if event == EVENT_OBJECT_DESTROY {
            return self
                .keys
                .destroyed(hwnd_value(hwnd))
                .map(|win| WindowSignal::Destroyed { win });
        }
        let cause = match event {
            EVENT_SYSTEM_FOREGROUND => None,
            EVENT_OBJECT_CREATE => Some(ChangeCause::Created),
            EVENT_OBJECT_SHOW => Some(ChangeCause::Shown),
            EVENT_OBJECT_HIDE => Some(ChangeCause::Hidden),
            EVENT_OBJECT_LOCATIONCHANGE => Some(ChangeCause::Location),
            EVENT_OBJECT_NAMECHANGE => Some(ChangeCause::Name),
            EVENT_SYSTEM_MINIMIZESTART => Some(ChangeCause::MinimizeStart),
            EVENT_SYSTEM_MINIMIZEEND => Some(ChangeCause::MinimizeEnd),
            EVENT_SYSTEM_MOVESIZEEND => Some(ChangeCause::MoveSizeEnd),
            _ => return None,
        };
        if !is_top_level_of_class(hwnd, &self.class) {
            return None;
        }
        let win = self.keys.key_of(hwnd_value(hwnd));
        let pid = window_process_id(hwnd);
        Some(match cause {
            Some(cause) => WindowSignal::Changed { win, pid, cause },
            None => WindowSignal::Activated { win, pid },
        })
    }

    /// The exit wait for `pid` fired: emit `ProcessExited` after the delay.
    fn process_signalled(&self, pid: u32) {
        let window = {
            let mut st = self.state.borrow_mut();
            let Some(watch) = st.processes.get_mut(&pid) else {
                return;
            };
            let delay = std::time::Duration::from_millis(u64::from(self.exit_delay_ms));
            watch.exit_due = Some(std::time::Instant::now() + delay);
            st.message_window
        };
        let Some(window) = window else {
            return self.exit_delay_elapsed(pid);
        };
        // SAFETY: the window belongs to this thread; timer id = PID (non-zero).
        let timer = unsafe { SetTimer(Some(window), pid as usize, self.exit_delay_ms, None) };
        if timer == 0 {
            self.exit_delay_elapsed(pid);
        }
    }

    /// The exit timer for `pid` fired on `window`: emit `ProcessExited`, or
    /// re-arm the timer if it fired before the full delay had passed.
    fn exit_timer_fired(&self, window: HWND, pid: u32) {
        let due = self.state.borrow().processes.get(&pid).and_then(|w| w.exit_due);
        let remaining = due.map_or(0, |d| {
            let left = d.saturating_duration_since(std::time::Instant::now());
            u32::try_from(left.as_micros().div_ceil(1000)).unwrap_or(u32::MAX)
        });
        if remaining > 0 {
            // SAFETY: the window belongs to this thread; timer id = PID (non-zero).
            let timer = unsafe { SetTimer(Some(window), pid as usize, remaining, None) };
            if timer != 0 {
                return;
            }
        }
        self.exit_delay_elapsed(pid);
    }

    fn exit_delay_elapsed(&self, pid: u32) {
        let watch = self.state.borrow_mut().processes.remove(&pid);
        if let Some(watch) = watch {
            watch.release();
            // Windows of the exited process may have been destroyed unseen.
            self.prune_seen_windows();
            self.handler.on_event(WindowSignal::ProcessExited { pid });
        }
    }
}

impl ProcessWatch {
    /// Unhook, unregister the wait (waiting for a running callback), close the handle.
    fn release(self) {
        for hook in self.hooks.iter().chain(&self.location) {
            // SAFETY: installed on this thread by `watch_process`.
            let _ = unsafe { UnhookWinEvent(*hook) };
        }
        if !self.wait.is_invalid() {
            // SAFETY: a registered wait; INVALID_HANDLE_VALUE blocks until any
            // running callback (which only posts a message) has returned.
            let _ = unsafe { UnregisterWaitEx(self.wait, Some(INVALID_HANDLE_VALUE)) };
        }
        // SAFETY: the handle is owned by this watch and closed once.
        let _ = unsafe { CloseHandle(self.process) };
    }
}

/// `GetWindowPlacement` of `hwnd` converted to screen coordinates of the
/// display the window is on (`MonitorFromWindow`, nearest).
pub fn window_placement(hwnd: HWND) -> Result<Placement, ShellError> {
    let _dpi = ThreadDpiScope::per_monitor_v2();
    let mut wp = WINDOWPLACEMENT {
        length: size_of::<WINDOWPLACEMENT>() as u32,
        ..Default::default()
    };
    // SAFETY: `wp` is writable and its length is set.
    unsafe { GetWindowPlacement(hwnd, &mut wp) }
        .map_err(|e| win_error("GetWindowPlacement", &e))?;
    let display =
        display_for_window(hwnd).ok_or_else(|| error(0, "placement: no monitor info".into()))?;
    placement_from(&wp, &display).ok_or_else(|| {
        error(
            0,
            format!("placement: unusable rect {:?}", wp.rcNormalPosition),
        )
    })
}

/// Builds a [`Placement`] from a `WINDOWPLACEMENT` (workspace coordinates)
/// and the display the window is on. `None` if the restored rect is empty or
/// out of range.
fn placement_from(wp: &WINDOWPLACEMENT, display: &Display) -> Option<Placement> {
    let r = wp.rcNormalPosition;
    let normal = Rect {
        left: r.left,
        top: r.top,
        right: r.right,
        bottom: r.bottom,
    };
    let restored = workspace_to_screen(normal, display.monitor, display.work)?;
    if !restored.is_positive() {
        return None;
    }
    let cmd = SHOW_WINDOW_CMD(wp.showCmd as i32);
    let state = if [
        SW_SHOWMINIMIZED,
        SW_MINIMIZE,
        SW_SHOWMINNOACTIVE,
        SW_FORCEMINIMIZE,
    ]
    .contains(&cmd)
    {
        ShowState::Minimized {
            restore_maximized: wp.flags.0 & WPF_RESTORETOMAXIMIZED.0 != 0,
        }
    } else if cmd == SW_SHOWMAXIMIZED {
        ShowState::Maximized
    } else {
        ShowState::Normal
    };
    Some(Placement {
        state,
        restored,
        display: DisplayRef {
            device: display.device.clone(),
            monitor: display.monitor,
        },
    })
}

/// `GetClassNameW(hwnd) == class` and `GetAncestor(hwnd, GA_ROOT) == hwnd`.
fn is_top_level_of_class(hwnd: HWND, class: &[u16]) -> bool {
    let mut buf = [0u16; 256];
    // SAFETY: the buffer is valid for its length; stale HWNDs return 0.
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    if len <= 0 || buf[..len as usize] != *class {
        return false;
    }
    // SAFETY: GetAncestor tolerates stale HWNDs.
    unsafe { GetAncestor(hwnd, GA_ROOT) == hwnd }
}

fn window_process_id(hwnd: HWND) -> u32 {
    let mut pid = 0u32;
    // SAFETY: `pid` is a valid out pointer; stale HWNDs leave it at 0.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid
}

fn install_hook(min: u32, max: u32, pid: u32) -> Result<HWINEVENTHOOK, ShellError> {
    // SAFETY: out-of-context hook with a 'static callback; removed by the
    // owner on this thread.
    let hook = unsafe {
        SetWinEventHook(
            min,
            max,
            None,
            Some(win_event_proc),
            pid,
            0,
            WINEVENT_OUTOFCONTEXT,
        )
    };
    if hook.is_invalid() {
        let e = windows::core::Error::from_thread();
        return Err(win_error(
            &format!("SetWinEventHook({min:#x}..{max:#x}, pid {pid})"),
            &e,
        ));
    }
    Ok(hook)
}

fn create_message_window() -> Result<HWND, ShellError> {
    // SAFETY: standard class registration (a repeat registration fails
    // harmlessly) and creation of a message-only window owned by this thread.
    unsafe {
        let module = GetModuleHandleW(None).map_err(|e| win_error("GetModuleHandleW", &e))?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(message_window_proc),
            hInstance: module.into(),
            lpszClassName: MESSAGE_WINDOW_CLASS,
            ..Default::default()
        };
        RegisterClassW(&class);
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            MESSAGE_WINDOW_CLASS,
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
        .map_err(|e| win_error("CreateWindowExW(message window)", &e))
    }
}

/// WinEvent callback (out-of-context, on the hooking thread).
unsafe extern "system" fn win_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    time: u32,
) {
    if id_object != OBJID_WINDOW.0 || id_child != CHILDID_SELF as i32 {
        return;
    }
    if crate::diag::tracing()
        && (EVENT_OBJECT_CREATE..=EVENT_OBJECT_HIDE).contains(&event)
        && crate::shell_util::is_cabinet_window(hwnd)
    {
        // SAFETY: no preconditions.
        let lag = unsafe { windows::Win32::System::SystemInformation::GetTickCount() }.wrapping_sub(time);
        crate::diag::note(|| {
            format!("wes: event {event:#x} {:#x} delivered {lag} ms after it fired", hwnd.0 as usize)
        });
    }
    if let Some(inner) = active_source() {
        inner.on_win_event(event, hwnd, id_object, id_child);
    }
}

/// Wait callback (thread-pool thread): hand the PID to the source's thread.
unsafe extern "system" fn process_signalled(context: *mut c_void, _timed_out: bool) {
    // SAFETY: `context` is the watch's `WaitContext`, alive until the wait is
    // unregistered with completion.
    let context = unsafe { &*(context as *const WaitContext) };
    // SAFETY: PostMessageW may target another thread's window; a destroyed
    // window just makes it fail.
    let _ = unsafe {
        PostMessageW(
            Some(context.window),
            WM_PROCESS_SIGNALLED,
            WPARAM(context.pid as usize),
            LPARAM(0),
        )
    };
}

unsafe extern "system" fn message_window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PROCESS_SIGNALLED => {
            if let Some(inner) = active_source().filter(|i| i.owns_window(hwnd)) {
                inner.process_signalled(wparam.0 as u32);
            }
            LRESULT(0)
        }
        WM_TIMER => {
            // SAFETY: our own window's timer.
            let _ = unsafe { KillTimer(Some(hwnd), wparam.0) };
            if let Some(inner) = active_source().filter(|i| i.owns_window(hwnd)) {
                inner.exit_timer_fired(hwnd, wparam.0 as u32);
            }
            LRESULT(0)
        }
        // SAFETY: default processing for our own window.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn error(code: i32, message: String) -> ShellError {
    ShellError {
        code,
        message: format!("window events: {message}"),
    }
}

fn win_error(step: &str, e: &windows::core::Error) -> ShellError {
    error(e.code().0, format!("{step}: {}", e.message()))
}

/// Switches the calling thread to per-monitor-aware v2 (physical pixels) and
/// restores the previous context on drop.
struct ThreadDpiScope {
    previous: DPI_AWARENESS_CONTEXT,
}

impl ThreadDpiScope {
    fn per_monitor_v2() -> Self {
        // SAFETY: valid predefined context; null on failure (nothing to restore).
        let previous =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        Self { previous }
    }
}

impl Drop for ThreadDpiScope {
    fn drop(&mut self) {
        if !self.previous.0.is_null() {
            // SAFETY: restores the context returned by the matching call.
            unsafe { SetThreadDpiAwarenessContext(self.previous) };
        }
    }
}

// ---------------------------------------------------------------------------
// Tab folding: first signals (task 25.2, Req 14.2, 14.4, 14.22)
// ---------------------------------------------------------------------------

/// Where the tab-folding events are posted (`EventSink::post` in `esr-app`).
/// Called on the source's thread from inside the message pump.
pub type EventPost = Rc<dyn Fn(Event)>;

/// Tab-folding state of the source (each field is its own `RefCell`; none is
/// borrowed across a call that can pump messages). Later tasks add their
/// state here.
#[derive(Default)]
struct FoldState {
    /// Set by [`WindowEventSource::set_event_sink`].
    sink: RefCell<Option<EventPost>>,
    /// Tab window to owning window, filled by the scanner (task 25.3).
    tabs: tab_detach::TabWindows,
    /// Seen windows and the foreground history.
    first: RefCell<first_signal::FirstSignals>,
    /// Created on first use (needs COM on the source's thread).
    desktops: RefCell<Option<VirtualDesktops>>,
    /// A `WindowAppeared` is being built. The `IVirtualDesktopManager` calls
    /// it makes are cross-process COM calls that can dispatch further WinEvent
    /// callbacks on this thread; those are queued in `deferred` and handled,
    /// in arrival order, after the report is posted, so that no signal of the
    /// new window overtakes its `WindowAppeared`.
    building: Cell<bool>,
    deferred: RefCell<VecDeque<(u32, HWND, i32, i32)>>,
}

impl Inner {
    /// `start`: windows that are open now are never first signals (Req 14.3 f).
    fn fold_started(&self) {
        let open = windows_of_class(&self.class);
        self.fold.first.borrow_mut().reset_to(open);
        if self.fold.sink.borrow().is_some() {
            // Best effort: creating it now keeps the cost off the first signal.
            let _ = self.desktop_manager();
        }
    }

    /// `stop`: forget what was seen; `start` seeds it again.
    fn fold_stopped(&self) {
        self.fold.first.borrow_mut().reset_to(Vec::new());
        self.fold.deferred.borrow_mut().clear();
    }

    /// Posts `TabDetached { from }` when `hwnd` is the destroyed tab window of
    /// a known window, and drops the map entries of a destroyed window. The
    /// map is read and updated for every `DESTROY`; no borrow or lock is held
    /// while the sink runs.
    fn report_tab_detached(&self, event: u32, hwnd: HWND, id_object: i32, id_child: i32) {
        if event != EVENT_OBJECT_DESTROY
            || id_object != OBJID_WINDOW.0
            || id_child != CHILDID_SELF as i32
            || hwnd.is_invalid()
        {
            return;
        }
        let value = hwnd_value(hwnd);
        let detached = tab_detach::tab_detached_event(&self.fold.tabs, value, |top| self.keys.get(top));
        match detached {
            Some(event) => {
                let sink = self.fold.sink.borrow().clone();
                if let Some(sink) = sink {
                    sink(event);
                }
            }
            // Not a tab window: it may be a window that owned some.
            None => self.fold.tabs.forget_window(value),
        }
    }

    /// Forgets seen windows that no longer exist (their `DESTROY` was missed,
    /// for example when their process was killed).
    fn prune_seen_windows(&self) {
        self.fold.first.borrow_mut().retain(window_exists);
    }

    fn desktop_manager(&self) -> Option<VirtualDesktops> {
        if let Some(manager) = self.fold.desktops.borrow().clone() {
            return Some(manager);
        }
        // No borrow is held while COM is called.
        let manager = VirtualDesktops::new().ok()?;
        *self.fold.desktops.borrow_mut() = Some(manager.clone());
        Some(manager)
    }

    /// Posts `WindowAppeared` if this WinEvent is the first signal of a new
    /// top-level window of the watched class; keeps the foreground history
    /// and the seen set up to date.
    fn report_first_signal(&self, event: u32, hwnd: HWND, id_object: i32, id_child: i32) {
        if id_object != OBJID_WINDOW.0 || id_child != CHILDID_SELF as i32 || hwnd.is_invalid() {
            return;
        }
        let value = hwnd_value(hwnd);
        if event == EVENT_OBJECT_DESTROY {
            // The HWND value may be reused by a new window.
            self.fold.first.borrow_mut().forget(value);
            return;
        }
        if event == EVENT_SYSTEM_FOREGROUND {
            self.fold.first.borrow_mut().record_foreground(value);
        }
        let Some(sink) = self.fold.sink.borrow().clone() else {
            return;
        };
        // SAFETY: GetAncestor tolerates stale HWNDs (NULL for them).
        let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
        if root.is_invalid() || !is_top_level_of_class(root, &self.class) {
            return;
        }
        let root = hwnd_value(root);
        let history = {
            let mut first = self.fold.first.borrow_mut();
            if !first.claim(root) {
                return;
            }
            first.history().clone()
        };
        self.fold.building.set(true);
        let facts = WinFacts {
            class: &self.class,
            desktops: self.desktop_manager(),
        };
        let event = first_signal::appeared_event(root, &self.keys, &history, &facts);
        self.fold.building.set(false);
        sink(event);
    }
}

/// The Windows answers behind [`first_signal::Facts`].
struct WinFacts<'a> {
    class: &'a [u16],
    desktops: Option<VirtualDesktops>,
}

impl first_signal::Facts for WinFacts<'_> {
    fn bypass_keys(&self) -> bool {
        // SAFETY: no preconditions.
        let (ctrl, shift) = unsafe {
            (
                GetAsyncKeyState(i32::from(VK_CONTROL.0)),
                GetAsyncKeyState(i32::from(VK_SHIFT.0)),
            )
        };
        first_signal::both_down(ctrl, shift)
    }

    fn foreground_window(&self) -> u64 {
        // SAFETY: no preconditions; NULL when there is no foreground window.
        hwnd_value(unsafe { GetForegroundWindow() })
    }

    fn is_explorer_window(&self, hwnd: u64) -> bool {
        is_top_level_of_class(hwnd_of(hwnd), self.class)
    }

    fn window_exists(&self, hwnd: u64) -> bool {
        window_exists(hwnd)
    }

    fn process_id(&self, hwnd: u64) -> u32 {
        window_process_id(hwnd_of(hwnd))
    }

    fn desktop_view(&self, candidate: WinKey, open: &[WinKey]) -> DesktopView {
        match &self.desktops {
            Some(desktops) => desktops.view(candidate, open),
            None => DesktopView::Unknown,
        }
    }
}

fn window_exists(hwnd: u64) -> bool {
    // SAFETY: IsWindow tolerates stale HWNDs.
    unsafe { IsWindow(Some(hwnd_of(hwnd))) }.as_bool()
}

/// The HWND values of the top-level windows of `class` that exist now.
fn windows_of_class(class: &[u16]) -> Vec<u64> {
    struct Search<'a> {
        class: &'a [u16],
        found: Vec<u64>,
    }
    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: `lparam` is the `&mut Search` passed below, alive for the
        // whole synchronous EnumWindows call.
        let search = unsafe { &mut *(lparam.0 as *mut Search<'_>) };
        if is_top_level_of_class(hwnd, search.class) {
            search.found.push(hwnd_value(hwnd));
        }
        BOOL(1)
    }
    let mut search = Search { class, found: Vec::new() };
    // SAFETY: as above. A failed enumeration leaves what was found so far.
    let _ = unsafe { EnumWindows(Some(collect), LPARAM((&raw mut search) as isize)) };
    search.found
}

/// Pure decisions of the first-signal report; the Windows reads sit behind
/// [`Facts`](first_signal::Facts), so everything here runs without Windows.
pub mod first_signal {
    use std::collections::{HashSet, VecDeque};

    use esr_core::engine::{DesktopView, Event, WinKey};

    use crate::keys::WindowKeys;

    /// How many distinct recent foreground windows are remembered.
    pub const FOREGROUND_HISTORY_LEN: usize = 8;

    /// The most recent windows reported by the global
    /// `EVENT_SYSTEM_FOREGROUND` hook, whatever their class (HWND values,
    /// newest last, each window once).
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    pub struct ForegroundHistory {
        recent: VecDeque<u64>,
    }

    impl ForegroundHistory {
        /// `hwnd` became the foreground window.
        pub fn record(&mut self, hwnd: u64) {
            if hwnd == 0 {
                return;
            }
            self.recent.retain(|&h| h != hwnd);
            self.recent.push_back(hwnd);
            while self.recent.len() > FOREGROUND_HISTORY_LEN {
                self.recent.pop_front();
            }
        }

        /// The latest foreground window other than `candidate`.
        pub fn latest_other_than(&self, candidate: u64) -> Option<u64> {
            self.recent.iter().rev().copied().find(|&h| h != candidate)
        }

        pub fn clear(&mut self) {
            self.recent.clear();
        }
    }

    /// The seen set (each window is reported once) and the foreground history.
    #[derive(Debug, Default)]
    pub struct FirstSignals {
        seen: HashSet<u64>,
        history: ForegroundHistory,
    }

    impl FirstSignals {
        /// Marks `hwnd` seen. `true` the first time (this is its first signal).
        pub fn claim(&mut self, hwnd: u64) -> bool {
            self.seen.insert(hwnd)
        }

        /// `hwnd` was destroyed; a new window may reuse the value.
        pub fn forget(&mut self, hwnd: u64) {
            self.seen.remove(&hwnd);
        }

        /// Keeps only the seen windows for which `exists` is true.
        pub fn retain(&mut self, mut exists: impl FnMut(u64) -> bool) {
            self.seen.retain(|&h| exists(h));
        }

        /// Replaces the seen set (the windows open now) and empties the history.
        pub fn reset_to(&mut self, open: impl IntoIterator<Item = u64>) {
            self.seen = open.into_iter().collect();
            self.history.clear();
        }

        pub fn is_seen(&self, hwnd: u64) -> bool {
            self.seen.contains(&hwnd)
        }

        pub fn record_foreground(&mut self, hwnd: u64) {
            self.history.record(hwnd);
        }

        pub fn history(&self) -> &ForegroundHistory {
            &self.history
        }
    }

    /// Ctrl and Shift both down, from two `GetAsyncKeyState` results (the high
    /// bit, so a negative `i16`, means down).
    pub fn both_down(ctrl: i16, shift: i16) -> bool {
        ctrl < 0 && shift < 0
    }

    /// The candidate's Foreground_Explorer_Window (Req 14.22): the window that
    /// was foreground *before the candidate took the foreground*.
    ///
    /// Explorer makes the new window foreground before its first signal is
    /// delivered, so `current` (`GetForegroundWindow`, 0 for none) usually is
    /// the candidate. Then the latest history entry other than the candidate
    /// is the answer. A `current` that is another window is the answer itself.
    /// Either way it counts only if `is_explorer` says it is an Explorer
    /// window: a candidate opened while another application was in the
    /// foreground has none.
    pub fn foreground_before(
        candidate: u64,
        current: u64,
        history: &ForegroundHistory,
        is_explorer: impl Fn(u64) -> bool,
    ) -> Option<u64> {
        let named = if current != 0 && current != candidate {
            current
        } else {
            history.latest_other_than(candidate)?
        };
        is_explorer(named).then_some(named)
    }

    /// What the Windows layer answers at a first signal.
    pub trait Facts {
        /// Ctrl and Shift are both down now.
        fn bypass_keys(&self) -> bool;
        /// `GetForegroundWindow`, 0 for none.
        fn foreground_window(&self) -> u64;
        /// A top-level window of the watched class.
        fn is_explorer_window(&self, hwnd: u64) -> bool;
        /// The HWND is a window now.
        fn window_exists(&self, hwnd: u64) -> bool;
        /// The owner process of the window.
        fn process_id(&self, hwnd: u64) -> u32;
        /// The `DesktopView` of `candidate` among the `open` windows.
        fn desktop_view(&self, candidate: WinKey, open: &[WinKey]) -> DesktopView;
    }

    /// `Event::WindowAppeared` for the new window `root`. The key state is
    /// read first, as close to the signal as possible (Req 14.4). `keys` is the
    /// registry shared with the other adapters; its live windows, those that
    /// still exist, are the open windows of the `DesktopView`, and the
    /// foreground must be one of them to be named.
    pub fn appeared_event(
        root: u64,
        keys: &WindowKeys,
        history: &ForegroundHistory,
        facts: &impl Facts,
    ) -> Event {
        let bypass_keys = facts.bypass_keys();
        let current = facts.foreground_window();
        let win = keys.key_of(root);
        let pid = facts.process_id(root);
        let foreground = foreground_before(root, current, history, |h| facts.is_explorer_window(h))
            .and_then(|hwnd| keys.get(hwnd))
            .filter(|&fg| fg != win);
        let open: Vec<WinKey> = keys
            .live()
            .into_iter()
            .filter(|w| w.hwnd != root && facts.window_exists(w.hwnd))
            .collect();
        let desktop = facts.desktop_view(win, &open);
        Event::WindowAppeared { win, pid, bypass_keys, desktop, foreground }
    }
}

/// Pure part of `TabDetached` (task 25.3): the tab-to-window map and the
/// decision for a destroyed window, tested without Windows.
pub mod tab_detach {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

    use esr_core::engine::{Event, WinKey};

    /// Tab window (`ShellTabWindowClass` HWND value) to the HWND value of the
    /// top-level window that holds it. Shared (cheap to clone) between the
    /// window scanner, which refreshes it, and the `WindowEventSource`, which
    /// reads it when a tab window is destroyed.
    #[derive(Clone, Debug, Default)]
    pub struct TabWindows(Arc<Mutex<HashMap<u64, u64>>>);

    impl TabWindows {
        fn lock(&self) -> MutexGuard<'_, HashMap<u64, u64>> {
            // The map stays consistent even if a holder panicked.
            self.0.lock().unwrap_or_else(PoisonError::into_inner)
        }

        /// Replaces what is known about `window`: exactly `tabs` belong to it
        /// now. A tab HWND that was mapped to another window moves to this one
        /// (a dragged tab's window is recreated, and HWND values are reused).
        pub fn refresh(&self, window: u64, tabs: impl IntoIterator<Item = u64>) {
            let mut map = self.lock();
            map.retain(|_, w| *w != window);
            map.extend(tabs.into_iter().map(|tab| (tab, window)));
        }

        /// Drops every tab of `window` (it was destroyed).
        pub fn forget_window(&self, window: u64) {
            self.lock().retain(|_, w| *w != window);
        }

        /// Removes `tab` and returns the window that held it.
        pub fn take(&self, tab: u64) -> Option<u64> {
            self.lock().remove(&tab)
        }

        /// The window that holds `tab`.
        pub fn window_of(&self, tab: u64) -> Option<u64> {
            self.lock().get(&tab).copied()
        }

        pub fn len(&self) -> usize {
            self.lock().len()
        }

        pub fn is_empty(&self) -> bool {
            self.lock().is_empty()
        }
    }

    /// `Event::TabDetached` for the destroyed window `destroyed`: it is a tab
    /// window of the map (which forgets it) and its window is known to the
    /// caller (`known` returns the window's live key, as the shared registry
    /// does). Anything else is not a detached tab.
    pub fn tab_detached_event(
        tabs: &TabWindows,
        destroyed: u64,
        known: impl Fn(u64) -> Option<WinKey>,
    ) -> Option<Event> {
        let window = tabs.take(destroyed)?;
        known(window).map(|from| Event::TabDetached { from })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetWindowRect, MSG, MsgWaitForMultipleObjects, OBJID_CLIENT, PM_REMOVE,
        PeekMessageW, QS_ALLINPUT, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
        SetWindowPos, SetWindowTextW, ShowWindow, TranslateMessage, WINDOW_STYLE,
        WPF_SETMINPOSITION, WS_CHILD, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_OVERLAPPEDWINDOW,
        WS_POPUP,
    };
    use windows::core::HSTRING;

    type Recorded = Rc<RefCell<Vec<WindowSignal>>>;

    fn recorder() -> (Recorded, Rc<dyn WindowEventHandler>) {
        let rec: Recorded = Rc::default();
        let sink = rec.clone();
        (
            rec,
            Rc::new(move |s: WindowSignal| sink.borrow_mut().push(s)),
        )
    }

    fn source_for(class: &str, delay: u32) -> (WindowEventSource, Recorded) {
        let (rec, handler) = recorder();
        let config = WindowEventConfig {
            window_class: class.to_owned(),
            exit_delay_ms: delay,
        };
        (
            WindowEventSource::new(config, WindowKeys::new(), handler),
            rec,
        )
    }

    /// Pumps this thread's messages until `done` yields or `timeout` passes.
    fn pump_until<T>(timeout: Duration, mut done: impl FnMut() -> Option<T>) -> Option<T> {
        let deadline = Instant::now() + timeout;
        loop {
            let mut msg = MSG::default();
            // SAFETY: standard message pump on this thread.
            unsafe {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            if let Some(v) = done() {
                return Some(v);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let ms = (deadline - now).as_millis().min(10) as u32;
            // SAFETY: no handles; waits for input or the timeout.
            unsafe { MsgWaitForMultipleObjects(None, false, ms, QS_ALLINPUT) };
        }
    }

    fn pump_for(duration: Duration) {
        let _ = pump_until::<()>(duration, || None);
    }

    unsafe extern "system" fn test_window_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        // SAFETY: default processing for the test window.
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    /// A test window, destroyed on drop.
    struct TestWindow(HWND);

    impl TestWindow {
        fn new(
            class: &str,
            ex: WINDOW_EX_STYLE,
            style: WINDOW_STYLE,
            rect: (i32, i32, i32, i32),
            parent: Option<HWND>,
        ) -> Self {
            let class_w = HSTRING::from(class);
            // SAFETY: registers a test class (repeat registration fails
            // harmlessly) and creates a window owned by this thread.
            let hwnd = unsafe {
                let module = GetModuleHandleW(None).unwrap();
                let wc = WNDCLASSW {
                    lpfnWndProc: Some(test_window_proc),
                    hInstance: module.into(),
                    lpszClassName: PCWSTR(class_w.as_ptr()),
                    ..Default::default()
                };
                RegisterClassW(&wc);
                CreateWindowExW(
                    ex,
                    &class_w,
                    w!("esr-test"),
                    style,
                    rect.0,
                    rect.1,
                    rect.2,
                    rect.3,
                    parent,
                    None,
                    Some(module.into()),
                    None,
                )
            }
            .expect("create test window");
            Self(hwnd)
        }

        /// A hidden, non-activating, off-screen popup.
        fn offscreen(class: &str) -> Self {
            Self::new(
                class,
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                WS_POPUP,
                (-20000, -20000, 120, 80),
                None,
            )
        }

        fn destroy(&mut self) {
            if !self.0.is_invalid() {
                // SAFETY: our window, destroyed on its thread.
                let _ = unsafe { DestroyWindow(self.0) };
                self.0 = HWND::default();
            }
        }
    }

    impl Drop for TestWindow {
        fn drop(&mut self) {
            self.destroy();
        }
    }

    /// A child process, killed on drop.
    struct TestChild(Child);

    impl Drop for TestChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn find_change(rec: &Recorded, hwnd: HWND, cause: ChangeCause) -> Option<WindowSignal> {
        rec.borrow()
            .iter()
            .copied()
            .find(|s| matches!(s, WindowSignal::Changed { win, cause: c, .. } if win.hwnd == hwnd_value(hwnd) && *c == cause))
    }

    fn signals_for(rec: &Recorded, hwnd: HWND) -> Vec<WindowSignal> {
        let v = hwnd_value(hwnd);
        rec.borrow()
            .iter()
            .copied()
            .filter(|s| match s {
                WindowSignal::Changed { win, .. }
                | WindowSignal::Destroyed { win }
                | WindowSignal::Activated { win, .. } => win.hwnd == v,
                WindowSignal::ProcessExited { .. } => false,
            })
            .collect()
    }

    const WAIT: Duration = Duration::from_secs(5);

    /// Task 16.3: while a restoration runs, the per-PID location/name hook
    /// is paused (no `Location` signals); resuming reinstalls it.
    #[test]
    fn paused_location_hooks_report_no_moves_until_resumed() {
        let class = "ESR.Test.WindowEvents.Pause";
        let (mut source, rec) = source_for(class, PROCESS_EXIT_DELAY_MS);
        source.start().expect("start");
        source.watch_process(std::process::id()).expect("watch own process");
        let win = TestWindow::offscreen(class);
        pump_until(WAIT, || find_change(&rec, win.0, ChangeCause::Created)).expect("Created");
        let move_to = |x: i32| {
            // SAFETY: our own window, on this thread.
            unsafe { SetWindowPos(win.0, None, x, -20050, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE) }
                .unwrap();
        };

        source.pause_location_hooks(true);
        // Pausing twice is a no-op.
        source.pause_location_hooks(true);
        rec.borrow_mut().clear();
        move_to(-20100);
        pump_for(Duration::from_millis(500));
        assert_eq!(find_change(&rec, win.0, ChangeCause::Location), None, "moved while paused");

        source.pause_location_hooks(false);
        move_to(-20200);
        pump_until(WAIT, || find_change(&rec, win.0, ChangeCause::Location)).expect("Location after resume");
        source.stop();
    }

    #[test]
    fn real_events_of_watched_class_produce_signals() {
        let class = "ESR.Test.WindowEvents.Real";
        let (mut source, rec) = source_for(class, PROCESS_EXIT_DELAY_MS);
        source.start().expect("start");
        let own_pid = std::process::id();
        source.watch_process(own_pid).expect("watch own process");

        let mut win = TestWindow::offscreen(class);
        let other = TestWindow::offscreen("ESR.Test.WindowEvents.Other");
        let child = TestWindow::new(
            class,
            WINDOW_EX_STYLE(0),
            WS_CHILD,
            (0, 0, 10, 10),
            Some(win.0),
        );

        let created =
            pump_until(WAIT, || find_change(&rec, win.0, ChangeCause::Created)).expect("Created");
        let WindowSignal::Changed { win: key, pid, .. } = created else {
            unreachable!()
        };
        assert_eq!(pid, own_pid);
        assert_eq!(source.keys().get(key.hwnd), Some(key));

        // SAFETY: our own windows, on this thread.
        unsafe {
            let _ = ShowWindow(win.0, SW_SHOWNOACTIVATE);
            let _ = ShowWindow(other.0, SW_SHOWNOACTIVATE);
        }
        pump_until(WAIT, || find_change(&rec, win.0, ChangeCause::Shown)).expect("Shown");

        // SAFETY: as above.
        unsafe {
            SetWindowPos(
                win.0,
                None,
                -20100,
                -20050,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            )
        }
        .unwrap();
        pump_until(WAIT, || find_change(&rec, win.0, ChangeCause::Location)).expect("Location");

        // SAFETY: as above.
        unsafe {
            SetWindowTextW(win.0, w!("esr-test renamed")).unwrap();
            SetWindowTextW(other.0, w!("other renamed")).unwrap();
        }
        pump_until(WAIT, || find_change(&rec, win.0, ChangeCause::Name)).expect("Name");

        win.destroy();
        let destroyed = WindowSignal::Destroyed { win: key };
        pump_until(WAIT, || rec.borrow().contains(&destroyed).then_some(())).expect("Destroyed");
        assert_eq!(source.keys().get(key.hwnd), None);

        // Every signal for the window used the same key.
        for s in signals_for(&rec, hwnd_of(key.hwnd)) {
            match s {
                WindowSignal::Changed { win, .. } | WindowSignal::Destroyed { win } => {
                    assert_eq!(win, key)
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        pump_for(Duration::from_millis(100));
        assert!(
            signals_for(&rec, other.0).is_empty(),
            "other class: {:?}",
            signals_for(&rec, other.0)
        );
        assert!(
            signals_for(&rec, child.0).is_empty(),
            "child window: {:?}",
            signals_for(&rec, child.0)
        );
        assert!(
            !rec.borrow()
                .iter()
                .any(|s| matches!(s, WindowSignal::ProcessExited { .. }))
        );

        source.stop();
        assert!(!source.is_started());
        assert!(!source.is_watching(own_pid));
    }

    #[test]
    fn filter_routes_each_event_kind() {
        let class = "ESR.Test.WindowEvents.Filter";
        let (source, rec) = source_for(class, PROCESS_EXIT_DELAY_MS);
        let win = TestWindow::offscreen(class);
        let other = TestWindow::offscreen("ESR.Test.WindowEvents.FilterOther");
        let child = TestWindow::new(
            class,
            WINDOW_EX_STYLE(0),
            WS_CHILD,
            (0, 0, 10, 10),
            Some(win.0),
        );
        let inner = &source.inner;
        let pid = std::process::id();
        let self_id = CHILDID_SELF as i32;

        let cases = [
            (EVENT_OBJECT_CREATE, ChangeCause::Created),
            (EVENT_OBJECT_SHOW, ChangeCause::Shown),
            (EVENT_OBJECT_HIDE, ChangeCause::Hidden),
            (EVENT_OBJECT_LOCATIONCHANGE, ChangeCause::Location),
            (EVENT_OBJECT_NAMECHANGE, ChangeCause::Name),
            (EVENT_SYSTEM_MINIMIZESTART, ChangeCause::MinimizeStart),
            (EVENT_SYSTEM_MINIMIZEEND, ChangeCause::MinimizeEnd),
            (EVENT_SYSTEM_MOVESIZEEND, ChangeCause::MoveSizeEnd),
        ];
        let key = source.keys().key_of(hwnd_value(win.0));
        for (event, cause) in cases {
            assert_eq!(
                inner.signal_for(event, win.0, OBJID_WINDOW.0, self_id),
                Some(WindowSignal::Changed {
                    win: key,
                    pid,
                    cause
                }),
                "{event:#x}"
            );
            // Not OBJID_WINDOW / CHILDID_SELF, wrong class, child window: filtered.
            assert_eq!(
                inner.signal_for(event, win.0, OBJID_CLIENT.0, self_id),
                None
            );
            assert_eq!(inner.signal_for(event, win.0, OBJID_WINDOW.0, 1), None);
            assert_eq!(
                inner.signal_for(event, other.0, OBJID_WINDOW.0, self_id),
                None
            );
            assert_eq!(
                inner.signal_for(event, child.0, OBJID_WINDOW.0, self_id),
                None
            );
        }
        assert_eq!(
            inner.signal_for(EVENT_SYSTEM_FOREGROUND, win.0, OBJID_WINDOW.0, self_id),
            Some(WindowSignal::Activated { win: key, pid })
        );
        assert_eq!(
            inner.signal_for(EVENT_SYSTEM_FOREGROUND, other.0, OBJID_WINDOW.0, self_id),
            None
        );
        assert_eq!(
            inner.signal_for(EVENT_SYSTEM_FOREGROUND, child.0, OBJID_WINDOW.0, self_id),
            None
        );
        // Unrelated events are ignored.
        assert_eq!(
            inner.signal_for(EVENT_SYSTEM_FOREGROUND + 1, win.0, OBJID_WINDOW.0, self_id),
            None
        );

        // Destroy: only known HWNDs; the HWND then gets a new seq.
        assert_eq!(
            inner.signal_for(EVENT_OBJECT_DESTROY, other.0, OBJID_WINDOW.0, self_id),
            None
        );
        assert_eq!(
            inner.signal_for(EVENT_OBJECT_DESTROY, win.0, OBJID_WINDOW.0, self_id),
            Some(WindowSignal::Destroyed { win: key })
        );
        assert_eq!(
            inner.signal_for(EVENT_OBJECT_DESTROY, win.0, OBJID_WINDOW.0, self_id),
            None
        );
        let Some(WindowSignal::Changed { win: reused, .. }) =
            inner.signal_for(EVENT_OBJECT_CREATE, win.0, OBJID_WINDOW.0, self_id)
        else {
            panic!("no Created for reused HWND");
        };
        assert_eq!(reused.hwnd, key.hwnd);
        assert!(reused.seq > key.seq);

        // Dispatch delivers to the handler.
        inner.on_win_event(EVENT_SYSTEM_FOREGROUND, win.0, OBJID_WINDOW.0, self_id);
        assert_eq!(
            *rec.borrow(),
            vec![WindowSignal::Activated { win: reused, pid }]
        );
    }

    /// The default filter (`CabinetWClass`) on real windows of the classes
    /// that must never be tracked (Req 2.6). The desktop and taskbar windows
    /// are only read (class name, ancestor), never modified.
    #[test]
    fn default_class_filter_rejects_non_explorer_windows() {
        use crate::shell_util::{is_cabinet_window, window_class_name};
        use windows::Win32::UI::WindowsAndMessaging::FindWindowW;

        assert_eq!(WindowEventConfig::default().window_class, "CabinetWClass");
        let (source, _rec) = source_for(CABINET_WINDOW_CLASS, PROCESS_EXIT_DELAY_MS);
        let inner = &source.inner;
        let self_id = CHILDID_SELF as i32;
        let rejected = |hwnd: HWND, what: &str| {
            assert!(!is_cabinet_window(hwnd), "{what}");
            for event in [
                EVENT_OBJECT_CREATE,
                EVENT_OBJECT_SHOW,
                EVENT_OBJECT_LOCATIONCHANGE,
                EVENT_SYSTEM_FOREGROUND,
            ] {
                assert_eq!(
                    inner.signal_for(event, hwnd, OBJID_WINDOW.0, self_id),
                    None,
                    "{what} {event:#x}"
                );
            }
        };

        // A dialog of this process (the system `#32770` class), hidden.
        let dialog = TestWindow::offscreen("#32770");
        assert_eq!(window_class_name(dialog.0), "#32770");
        rejected(dialog.0, "#32770");

        // The shell's own desktop and taskbar windows, where present.
        for class in ["Progman", "WorkerW", "Shell_TrayWnd"] {
            let name = HSTRING::from(class);
            // SAFETY: read-only lookup of a top-level window by class.
            let Ok(hwnd) = (unsafe { FindWindowW(&name, PCWSTR::null()) }) else {
                continue;
            };
            assert_eq!(window_class_name(hwnd), class);
            rejected(hwnd, class);
        }

        // A `CabinetWClass` window that is not top-level (a local class of
        // this process, created as a hidden child) is rejected as well.
        let parent = TestWindow::offscreen("ESR.Test.WindowEvents.CabinetParent");
        let child = TestWindow::new(
            CABINET_WINDOW_CLASS,
            WINDOW_EX_STYLE(0),
            WS_CHILD,
            (0, 0, 10, 10),
            Some(parent.0),
        );
        assert_eq!(window_class_name(child.0), CABINET_WINDOW_CLASS);
        rejected(child.0, "child CabinetWClass");
    }

    #[test]
    fn only_one_source_per_thread_and_restart_works() {
        let (mut a, _) = source_for("ESR.Test.WindowEvents.OneA", PROCESS_EXIT_DELAY_MS);
        let (mut b, _) = source_for("ESR.Test.WindowEvents.OneB", PROCESS_EXIT_DELAY_MS);
        a.start().expect("start a");
        assert!(b.start().is_err());
        assert!(
            b.watch_process(std::process::id()).is_err(),
            "watch before start"
        );
        a.stop();
        b.start().expect("start b after a stopped");
        drop(b);
        a.start().expect("restart a");
        a.stop();
    }

    fn spawn_waiting_child() -> TestChild {
        let child = Command::new("cmd.exe")
            .args(["/d", "/c", "pause"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn cmd");
        TestChild(child)
    }

    fn wait_exit(rec: &Recorded, pid: u32) -> Option<Instant> {
        let exited = WindowSignal::ProcessExited { pid };
        pump_until(WAIT, || rec.borrow().contains(&exited).then(Instant::now))
    }

    #[test]
    fn process_exit_is_reported_after_the_delay() {
        let (mut source, rec) = source_for("ESR.Test.WindowEvents.Exit", PROCESS_EXIT_DELAY_MS);
        source.start().expect("start");

        // Killed while watched.
        let mut child = spawn_waiting_child();
        let pid = child.0.id();
        source.watch_process(pid).expect("watch child");
        source.watch_process(pid).expect("watch is idempotent");
        assert!(source.is_watching(pid));
        pump_for(Duration::from_millis(50));
        assert!(rec.borrow().is_empty(), "{:?}", rec.borrow());
        let killed = Instant::now();
        child.0.kill().expect("kill");
        child.0.wait().expect("wait");
        let at = wait_exit(&rec, pid).expect("ProcessExited after kill");
        assert!(
            at - killed >= Duration::from_millis(u64::from(PROCESS_EXIT_DELAY_MS)),
            "{:?}",
            at - killed
        );
        assert!(!source.is_watching(pid));
        assert_eq!(
            rec.borrow()
                .iter()
                .filter(|s| **s == WindowSignal::ProcessExited { pid })
                .count(),
            1
        );

        // Already exited when watched (its handle is still open, so the PID is
        // not reused): the wait fires at once, the signal follows the delay.
        let mut done = TestChild(
            Command::new("cmd.exe")
                .args(["/d", "/c", "exit"])
                .stdin(Stdio::null())
                .spawn()
                .expect("spawn"),
        );
        done.0.wait().expect("wait");
        let pid2 = done.0.id();
        let watched = Instant::now();
        source.watch_process(pid2).expect("watch exited child");
        let at = wait_exit(&rec, pid2).expect("ProcessExited for exited child");
        let elapsed = at - watched;
        assert!(
            elapsed >= Duration::from_millis(u64::from(PROCESS_EXIT_DELAY_MS)),
            "{elapsed:?}"
        );
        assert!(!source.is_watching(pid2));
        source.stop();
    }

    #[test]
    fn stop_removes_pending_watches_and_signals() {
        let (mut source, rec) = source_for("ESR.Test.WindowEvents.Stop", PROCESS_EXIT_DELAY_MS);
        source.start().expect("start");
        let mut child = spawn_waiting_child();
        let pid = child.0.id();
        source.watch_process(pid).expect("watch");
        source.stop();
        assert!(!source.is_watching(pid));
        child.0.kill().expect("kill");
        child.0.wait().expect("wait");
        pump_for(Duration::from_millis(400));
        assert!(rec.borrow().is_empty(), "{:?}", rec.borrow());
    }

    #[test]
    fn placement_of_a_test_window_is_in_screen_coordinates() {
        let _dpi = ThreadDpiScope::per_monitor_v2();
        let class = "ESR.Test.WindowEvents.Placement";
        let (source, _) = source_for(class, PROCESS_EXIT_DELAY_MS);
        // Hidden normal (non-tool) window, so rcNormalPosition is in workspace coordinates.
        let mut win = TestWindow::new(
            class,
            WINDOW_EX_STYLE(0),
            WS_OVERLAPPEDWINDOW,
            (150, 120, 400, 300),
            None,
        );
        let key = source.keys().key_of(hwnd_value(win.0));
        let p = source.placement(key).expect("placement");
        let mut rect = RECT::default();
        // SAFETY: our window; `rect` is writable.
        unsafe { GetWindowRect(win.0, &mut rect) }.unwrap();
        assert_eq!(p.state, ShowState::Normal);
        assert_eq!(
            p.restored,
            Rect {
                left: rect.left,
                top: rect.top,
                right: rect.right,
                bottom: rect.bottom
            }
        );
        let display = display_for_window(win.0).expect("display");
        assert_eq!(
            p.display,
            DisplayRef {
                device: display.device,
                monitor: display.monitor
            }
        );

        // Stale or unknown keys are rejected.
        assert!(
            source
                .placement(WinKey {
                    hwnd: key.hwnd,
                    seq: key.seq + 1
                })
                .is_err()
        );
        win.destroy();
        assert!(source.placement(key).is_err(), "destroyed window");
    }

    #[test]
    fn placement_conversion_maps_state_and_workspace_offset() {
        // Taskbar on the left of a secondary monitor: work area starts 48 px right.
        let display = Display {
            device: r"\\.\DISPLAY2".into(),
            monitor: Rect {
                left: 1920,
                top: 0,
                right: 3840,
                bottom: 1080,
            },
            work: Rect {
                left: 1968,
                top: 0,
                right: 3840,
                bottom: 1080,
            },
            primary: false,
        };
        // Workspace coordinates differ from screen coordinates by the work
        // area's offset within its monitor.
        let wp = |cmd: SHOW_WINDOW_CMD, flags| WINDOWPLACEMENT {
            length: size_of::<WINDOWPLACEMENT>() as u32,
            flags,
            showCmd: cmd.0 as u32,
            rcNormalPosition: RECT {
                left: 1930,
                top: 20,
                right: 2730,
                bottom: 620,
            },
            ..Default::default()
        };
        let restored = Rect {
            left: 1978,
            top: 20,
            right: 2778,
            bottom: 620,
        };
        let display_ref = DisplayRef {
            device: display.device.clone(),
            monitor: display.monitor,
        };
        let none = windows::Win32::UI::WindowsAndMessaging::WINDOWPLACEMENT_FLAGS(0);
        let cases = [
            (wp(SHOW_WINDOW_CMD(1), none), ShowState::Normal),
            (wp(SW_SHOWMAXIMIZED, none), ShowState::Maximized),
            (
                wp(SW_SHOWMINIMIZED, none),
                ShowState::Minimized {
                    restore_maximized: false,
                },
            ),
            (
                wp(SW_SHOWMINIMIZED, WPF_RESTORETOMAXIMIZED),
                ShowState::Minimized {
                    restore_maximized: true,
                },
            ),
            (
                wp(
                    SW_SHOWMINNOACTIVE,
                    WPF_RESTORETOMAXIMIZED | WPF_SETMINPOSITION,
                ),
                ShowState::Minimized {
                    restore_maximized: true,
                },
            ),
            // The flag only matters while minimized.
            (
                wp(SW_SHOWMAXIMIZED, WPF_RESTORETOMAXIMIZED),
                ShowState::Maximized,
            ),
        ];
        for (wp, state) in cases {
            assert_eq!(
                placement_from(&wp, &display),
                Some(Placement {
                    state,
                    restored,
                    display: display_ref.clone()
                }),
                "{:?}",
                wp.showCmd
            );
        }
        let mut empty = wp(SHOW_WINDOW_CMD(1), none);
        empty.rcNormalPosition.right = empty.rcNormalPosition.left;
        assert_eq!(placement_from(&empty, &display), None);
    }
}

/// Tests of the tab-folding first-signal report (task 25.2).
#[cfg(test)]
mod first_signal_tests {
    use super::first_signal::*;
    use super::*;
    use std::collections::BTreeSet;
    use std::time::{Duration, Instant};
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
        SetWindowPos, TranslateMessage, WS_CHILD, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
    };
    use windows::core::HSTRING;

    // --- pure decisions -------------------------------------------------

    #[test]
    fn history_keeps_each_window_once_newest_last_and_is_capped() {
        let mut h = ForegroundHistory::default();
        for hwnd in [1, 2, 3, 2, 0] {
            h.record(hwnd);
        }
        assert_eq!(h.latest_other_than(99), Some(2), "the repeat moved to the end; 0 is ignored");
        assert_eq!(h.latest_other_than(2), Some(3));
        for hwnd in 10..10 + 2 * FOREGROUND_HISTORY_LEN as u64 {
            h.record(hwnd);
        }
        let newest = 10 + 2 * FOREGROUND_HISTORY_LEN as u64 - 1;
        assert_eq!(h.latest_other_than(newest), Some(newest - 1));
        assert_eq!(h.latest_other_than(0), Some(newest));
        h.clear();
        assert_eq!(h.latest_other_than(0), None);
    }

    #[test]
    fn both_keys_must_be_down() {
        assert!(both_down(i16::MIN, i16::MIN + 1));
        assert!(!both_down(i16::MIN, 0));
        assert!(!both_down(0, i16::MIN));
        // The low bit alone ("pressed since the last call") is not "down".
        assert!(!both_down(1, 1));
    }

    #[test]
    fn foreground_is_the_window_before_the_candidate_took_it() {
        let mut h = ForegroundHistory::default();
        h.record(5); // an Explorer window P
        let is_explorer = |hwnd: u64| hwnd == 5 || hwnd == 6 || hwnd == 9;

        // Ctrl+N: the candidate (9) is already foreground, P was before it.
        assert_eq!(foreground_before(9, 9, &h, is_explorer), Some(5));
        // Its own foreground event arrived first: still P.
        h.record(9);
        assert_eq!(foreground_before(9, 9, &h, is_explorer), Some(5));
        // No foreground window right now: the history decides.
        assert_eq!(foreground_before(9, 0, &h, is_explorer), Some(5));
        // The candidate is not foreground and another Explorer window is.
        assert_eq!(foreground_before(9, 6, &h, is_explorer), Some(6));
        // Another application is foreground: no Foreground_Explorer_Window,
        // however Explorer-heavy the history is.
        assert_eq!(foreground_before(9, 77, &h, is_explorer), None);
        // "Show in folder": the application was foreground before the candidate.
        h.record(77);
        h.record(9);
        assert_eq!(foreground_before(9, 9, &h, is_explorer), None);
        // Nothing known before the candidate.
        assert_eq!(foreground_before(9, 9, &ForegroundHistory::default(), is_explorer), None);
    }

    #[test]
    fn each_window_is_claimed_once_until_it_is_destroyed() {
        let mut f = FirstSignals::default();
        assert!(f.claim(1));
        assert!(!f.claim(1));
        f.forget(1);
        assert!(f.claim(1), "an HWND value reused after its destroy is a new window");

        f.record_foreground(4);
        f.reset_to([7, 8]);
        assert!(f.is_seen(7) && !f.claim(8), "windows open at start are never first signals");
        assert_eq!(f.history().latest_other_than(0), None);
        f.retain(|h| h == 7);
        assert!(f.is_seen(7) && !f.is_seen(8));
    }

    #[derive(Default)]
    struct FakeFacts {
        keys_down: bool,
        foreground: u64,
        explorer: BTreeSet<u64>,
        gone: BTreeSet<u64>,
    }

    impl Facts for FakeFacts {
        fn bypass_keys(&self) -> bool {
            self.keys_down
        }
        fn foreground_window(&self) -> u64 {
            self.foreground
        }
        fn is_explorer_window(&self, hwnd: u64) -> bool {
            self.explorer.contains(&hwnd)
        }
        fn window_exists(&self, hwnd: u64) -> bool {
            !self.gone.contains(&hwnd)
        }
        fn process_id(&self, hwnd: u64) -> u32 {
            hwnd as u32 + 1000
        }
        fn desktop_view(&self, candidate: WinKey, open: &[WinKey]) -> DesktopView {
            DesktopView::Known(open.iter().copied().chain([candidate]).collect())
        }
    }

    #[test]
    fn appeared_event_carries_keys_pid_desktop_and_foreground() {
        let keys = WindowKeys::new();
        let primary = keys.key_of(5);
        let other = keys.key_of(6);
        let stale = keys.key_of(7);
        let mut history = ForegroundHistory::default();
        history.record(5);
        let facts = FakeFacts {
            keys_down: true,
            foreground: 9,
            explorer: BTreeSet::from([5, 6, 9]),
            gone: BTreeSet::from([7]),
        };

        let Event::WindowAppeared { win, pid, bypass_keys, desktop, foreground } =
            appeared_event(9, &keys, &history, &facts)
        else {
            panic!("not a WindowAppeared");
        };
        assert_eq!(keys.get(9), Some(win), "the candidate's key comes from the shared registry");
        assert_eq!(pid, 1009);
        assert!(bypass_keys);
        assert_eq!(foreground, Some(primary));
        let DesktopView::Known(set) = desktop else { panic!("{desktop:?}") };
        assert_eq!(set, BTreeSet::from([primary, other, win]), "a destroyed window is not open: {stale:?}");
    }

    #[test]
    fn appeared_event_without_an_untracked_or_non_explorer_foreground() {
        let keys = WindowKeys::new();
        keys.key_of(6);
        // Explorer window 5 was foreground, but nothing tracks it (no key).
        let mut history = ForegroundHistory::default();
        history.record(5);
        let facts = FakeFacts { foreground: 9, explorer: BTreeSet::from([5, 9]), ..Default::default() };
        let Event::WindowAppeared { foreground, bypass_keys, .. } = appeared_event(9, &keys, &history, &facts) else {
            panic!("not a WindowAppeared");
        };
        assert_eq!(foreground, None);
        assert!(!bypass_keys);

        // Another application is foreground.
        let facts = FakeFacts { foreground: 77, explorer: BTreeSet::from([5, 6]), ..Default::default() };
        let Event::WindowAppeared { foreground, .. } = appeared_event(9, &keys, &history, &facts) else {
            panic!("not a WindowAppeared");
        };
        assert_eq!(foreground, None);
    }

    // --- Windows ----------------------------------------------------------

    #[derive(Clone, Debug)]
    pub(super) enum Logged {
        Appeared(Event),
        Signal(WindowSignal),
    }

    pub(super) type Log = Rc<RefCell<Vec<Logged>>>;

    pub(super) fn fold_source(class: &str) -> (WindowEventSource, Log) {
        let log: Log = Rc::default();
        let signals = log.clone();
        let handler: Rc<dyn WindowEventHandler> =
            Rc::new(move |s: WindowSignal| signals.borrow_mut().push(Logged::Signal(s)));
        let config = WindowEventConfig { window_class: class.to_owned(), exit_delay_ms: PROCESS_EXIT_DELAY_MS };
        let source = WindowEventSource::new(config, WindowKeys::new(), handler);
        let events = log.clone();
        source.set_event_sink(Rc::new(move |e| events.borrow_mut().push(Logged::Appeared(e))));
        (source, log)
    }

    fn appeared_for(log: &Log, hwnd: HWND) -> Vec<Event> {
        let v = hwnd_value(hwnd);
        log.borrow()
            .iter()
            .filter_map(|l| match l {
                Logged::Appeared(e @ Event::WindowAppeared { win, .. }) if win.hwnd == v => Some(e.clone()),
                _ => None,
            })
            .collect()
    }

    unsafe extern "system" fn proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        // SAFETY: default processing for the test window.
        unsafe { DefWindowProcW(hwnd, msg, w, l) }
    }

    pub(super) fn window(class: &str, parent: Option<HWND>) -> HWND {
        let class_w = HSTRING::from(class);
        let (ex, style, rect) = match parent {
            None => (WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE, WS_POPUP, (-20000, -20000, 120, 80)),
            Some(_) => (WINDOW_EX_STYLE(0), WS_CHILD, (0, 0, 10, 10)),
        };
        // SAFETY: registers a test class (a repeat fails harmlessly) and
        // creates a window owned by this thread.
        unsafe {
            let module = GetModuleHandleW(None).unwrap();
            let wc = WNDCLASSW {
                lpfnWndProc: Some(proc),
                hInstance: module.into(),
                lpszClassName: PCWSTR(class_w.as_ptr()),
                ..Default::default()
            };
            RegisterClassW(&wc);
            CreateWindowExW(ex, &class_w, w!("esr-test"), style, rect.0, rect.1, rect.2, rect.3, parent, None, Some(module.into()), None)
        }
        .expect("create test window")
    }

    pub(super) fn destroy(hwnd: HWND) {
        // SAFETY: our window, on its thread.
        let _ = unsafe { DestroyWindow(hwnd) };
    }

    pub(super) fn pump_for(duration: Duration) {
        let end = Instant::now() + duration;
        while Instant::now() < end {
            let mut msg = MSG::default();
            // SAFETY: standard message pump on this thread.
            unsafe {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    pub(super) const SELF_ID: i32 = CHILDID_SELF as i32;

    #[test]
    fn a_child_event_or_the_cabinets_own_event_is_the_first_signal_and_comes_first() {
        let class = "ESR.Test.FirstSignal.Filter";
        let (source, log) = fold_source(class);
        let inner = &source.inner;
        let own_pid = std::process::id();

        // explorer.exe <path>: the first event is a child's CREATE.
        let parent = window(class, None);
        let child = window(class, Some(parent));
        inner.on_win_event(EVENT_OBJECT_CREATE, child, OBJID_WINDOW.0, SELF_ID);
        let appeared = appeared_for(&log, parent);
        assert_eq!(appeared.len(), 1, "{:?}", log.borrow());
        let Event::WindowAppeared { win, pid, desktop, foreground, .. } = &appeared[0] else { unreachable!() };
        assert_eq!(source.keys().get(hwnd_value(parent)), Some(*win));
        assert_eq!(*pid, own_pid);
        assert_eq!(*foreground, None, "the test class is not an Explorer window");
        assert!(matches!(desktop, DesktopView::Known(set) if set.contains(win)) || *desktop == DesktopView::Unknown);
        assert!(log.borrow().len() == 1, "the child itself is not a watched window: {:?}", log.borrow());

        // Later events of the same window, its children or the window itself,
        // are not first signals.
        inner.on_win_event(EVENT_OBJECT_CREATE, child, OBJID_WINDOW.0, SELF_ID);
        inner.on_win_event(EVENT_OBJECT_CREATE, parent, OBJID_WINDOW.0, SELF_ID);
        assert_eq!(appeared_for(&log, parent).len(), 1);

        // ShellExecute: the first event is the cabinet's own LOCATIONCHANGE,
        // and the window signal follows the report.
        let second = window(class, None);
        inner.on_win_event(EVENT_OBJECT_LOCATIONCHANGE, second, OBJID_WINDOW.0, SELF_ID);
        let entries = log.borrow();
        let n = entries.len();
        assert!(matches!(&entries[n - 2], Logged::Appeared(Event::WindowAppeared { win, .. }) if win.hwnd == hwnd_value(second)), "{entries:?}");
        assert!(matches!(&entries[n - 1], Logged::Signal(WindowSignal::Changed { cause: ChangeCause::Location, .. })), "{entries:?}");
        drop(entries);

        // Other classes, other identifiers and a missing sink report nothing.
        let other = window("ESR.Test.FirstSignal.Other", None);
        let before = log.borrow().len();
        inner.on_win_event(EVENT_OBJECT_CREATE, other, OBJID_WINDOW.0, SELF_ID);
        inner.on_win_event(EVENT_OBJECT_CREATE, window(class, None), OBJID_CLIENT_ID, SELF_ID);
        inner.on_win_event(EVENT_OBJECT_CREATE, HWND::default(), OBJID_WINDOW.0, SELF_ID);
        assert_eq!(log.borrow().len(), before);
        let (quiet, quiet_log) = fold_source(class);
        *quiet.inner.fold.sink.borrow_mut() = None;
        quiet.inner.on_win_event(EVENT_OBJECT_CREATE, parent, OBJID_WINDOW.0, SELF_ID);
        assert!(appeared_for(&quiet_log, parent).is_empty());

        for hwnd in [child, parent, second, other] {
            destroy(hwnd);
        }
    }

    /// `OBJID_CLIENT` without importing it a second time.
    const OBJID_CLIENT_ID: i32 = -4;

    #[test]
    fn a_destroyed_windows_hwnd_can_be_reported_again() {
        let class = "ESR.Test.FirstSignal.Reuse";
        let (source, log) = fold_source(class);
        let inner = &source.inner;
        let win = window(class, None);
        inner.on_win_event(EVENT_OBJECT_SHOW, win, OBJID_WINDOW.0, SELF_ID);
        inner.on_win_event(EVENT_OBJECT_SHOW, win, OBJID_WINDOW.0, SELF_ID);
        assert_eq!(appeared_for(&log, win).len(), 1);
        // Its DESTROY event, then a new window with the same HWND value.
        inner.on_win_event(EVENT_OBJECT_DESTROY, win, OBJID_WINDOW.0, SELF_ID);
        inner.on_win_event(EVENT_OBJECT_SHOW, win, OBJID_WINDOW.0, SELF_ID);
        let again = appeared_for(&log, win);
        assert_eq!(again.len(), 2);
        let (Event::WindowAppeared { win: first, .. }, Event::WindowAppeared { win: second, .. }) = (&again[0], &again[1]) else {
            unreachable!()
        };
        assert!(second.seq > first.seq, "a reused HWND gets a new key");
        destroy(win);
    }

    #[test]
    fn windows_open_at_start_are_never_reported_and_new_ones_are_reported_once() {
        let class = "ESR.Test.FirstSignal.Live";
        let _sta = crate::shell_util::StaGuard::init();
        let (mut source, log) = fold_source(class);
        let old = window(class, None);
        source.start().expect("start");
        let own_pid = std::process::id();
        source.watch_process(own_pid).expect("watch own process");
        // A new window, with a child, then moved: one report, before its signals.
        let new = window(class, None);
        let child = window(class, Some(new));
        let reposition = |hwnd: HWND, x: i32| {
            // SAFETY: our own window, on this thread.
            unsafe { SetWindowPos(hwnd, None, x, -20050, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE) }.unwrap();
        };
        reposition(old, -20100);
        reposition(new, -20200);
        pump_for(Duration::from_millis(700));

        assert!(appeared_for(&log, old).is_empty(), "{:?}", log.borrow());
        let appeared = appeared_for(&log, new);
        assert_eq!(appeared.len(), 1, "{:?}", log.borrow());
        let entries = log.borrow();
        let first_for_new = entries
            .iter()
            .position(|l| match l {
                Logged::Appeared(Event::WindowAppeared { win, .. }) => win.hwnd == hwnd_value(new),
                Logged::Signal(WindowSignal::Changed { win, .. }) => win.hwnd == hwnd_value(new),
                _ => false,
            })
            .expect("something for the new window");
        assert!(matches!(entries[first_for_new], Logged::Appeared(_)), "{entries:?}");
        drop(entries);

        source.stop();
        for hwnd in [child, new, old] {
            destroy(hwnd);
        }
    }
}

/// Tests of the `TabDetached` report (task 25.3).
#[cfg(test)]
mod tab_detach_tests {
    use super::first_signal_tests::{Logged, SELF_ID, destroy, fold_source, pump_for, window};
    use super::tab_detach::*;
    use super::*;
    use std::time::Duration;

    // --- pure decisions -------------------------------------------------

    #[test]
    fn refresh_replaces_the_tabs_of_a_window_and_moves_a_reused_tab() {
        let map = TabWindows::default();
        map.refresh(1, [10, 11, 12]);
        map.refresh(2, [20]);
        assert_eq!((map.window_of(10), map.window_of(20), map.len()), (Some(1), Some(2), 4));

        // A rescan: 11 closed, 13 opened, 20 dragged into window 1.
        map.refresh(1, [10, 12, 13, 20]);
        assert_eq!(map.window_of(11), None);
        assert_eq!([13, 20].map(|t| map.window_of(t)), [Some(1), Some(1)]);
        // Window 2 lost its only tab in the move; its own rescan finds none.
        map.refresh(2, []);
        assert_eq!(map.window_of(20), Some(1));
        assert_eq!(map.len(), 4);

        map.forget_window(1);
        assert!(map.is_empty());
    }

    #[test]
    fn the_destroyed_tab_of_a_known_window_is_detached_once() {
        let keys = WindowKeys::new();
        let known = keys.key_of(1);
        let map = TabWindows::default();
        map.refresh(1, [10, 11]);
        map.refresh(2, [20]); // window 2 has no key: unknown to the source

        let of = |tab| tab_detached_event(&map, tab, |top| keys.get(top));
        assert_eq!(of(10), Some(Event::TabDetached { from: known }));
        assert_eq!(of(10), None, "the entry is consumed: reported once");
        assert_eq!(map.window_of(11), Some(1), "the other tab stays");
        assert_eq!(of(20), None, "a tab of an unknown window");
        assert_eq!(of(99), None, "not a tab window at all");
    }

    #[test]
    fn a_destroyed_window_is_no_longer_a_source() {
        let keys = WindowKeys::new();
        keys.key_of(1);
        let map = TabWindows::default();
        map.refresh(1, [10]);
        keys.destroyed(1);
        assert_eq!(tab_detached_event(&map, 10, |top| keys.get(top)), None);
    }

    #[test]
    fn scanned_tabs_and_children_are_both_tab_windows_of_the_scanned_window() {
        use crate::scanner::{TabChild, tab_window_hwnds};
        let children = [TabChild { hwnd: 5, visible: true }, TabChild { hwnd: 6, visible: false }];
        let tabs = [esr_core::engine::TabKey { hwnd: 6, seq: 1 }, esr_core::engine::TabKey { hwnd: 7, seq: 2 }];
        assert_eq!(tab_window_hwnds(&children, &tabs), vec![5, 6, 7]);
        assert!(tab_window_hwnds(&[], &[]).is_empty());
    }

    // --- Windows ----------------------------------------------------------

    fn detached(log: &Log) -> Vec<WinKey> {
        log.borrow()
            .iter()
            .filter_map(|l| match l {
                Logged::Appeared(Event::TabDetached { from }) => Some(*from),
                _ => None,
            })
            .collect()
    }

    type Log = super::first_signal_tests::Log;

    #[test]
    fn a_destroy_event_of_a_mapped_tab_posts_tab_detached_before_other_signals() {
        let class = "ESR.Test.TabDetach.Filter";
        let (source, log) = fold_source(class);
        let inner = &source.inner;
        let top = window(class, None);
        let tab = window("ESR.Test.TabDetach.Tab", Some(top));
        let other_tab = window("ESR.Test.TabDetach.Tab", Some(top));
        let from = source.keys().key_of(hwnd_value(top));
        source.tab_windows().refresh(hwnd_value(top), [hwnd_value(tab), hwnd_value(other_tab)]);

        // Not a window event of the right kind: nothing.
        inner.on_win_event(EVENT_OBJECT_DESTROY, tab, -4, SELF_ID);
        inner.on_win_event(EVENT_OBJECT_DESTROY, tab, OBJID_WINDOW.0, 1);
        inner.on_win_event(EVENT_OBJECT_HIDE, tab, OBJID_WINDOW.0, SELF_ID);
        assert!(detached(&log).is_empty(), "{:?}", log.borrow());

        inner.on_win_event(EVENT_OBJECT_DESTROY, tab, OBJID_WINDOW.0, SELF_ID);
        assert_eq!(detached(&log), vec![from]);
        // Once; and the other tab is still mapped.
        inner.on_win_event(EVENT_OBJECT_DESTROY, tab, OBJID_WINDOW.0, SELF_ID);
        assert_eq!(detached(&log), vec![from]);
        assert_eq!(source.tab_windows().window_of(hwnd_value(other_tab)), Some(hwnd_value(top)));

        // The window itself being destroyed forgets its tabs and reports nothing.
        inner.on_win_event(EVENT_OBJECT_DESTROY, top, OBJID_WINDOW.0, SELF_ID);
        assert!(source.tab_windows().is_empty());
        inner.on_win_event(EVENT_OBJECT_DESTROY, other_tab, OBJID_WINDOW.0, SELF_ID);
        assert_eq!(detached(&log), vec![from]);

        // Without a sink the map is still maintained and nothing is posted.
        let (quiet, quiet_log) = fold_source(class);
        *quiet.inner.fold.sink.borrow_mut() = None;
        quiet.keys().key_of(hwnd_value(top));
        quiet.tab_windows().refresh(hwnd_value(top), [hwnd_value(tab)]);
        quiet.inner.on_win_event(EVENT_OBJECT_DESTROY, tab, OBJID_WINDOW.0, SELF_ID);
        assert!(detached(&quiet_log).is_empty());
        assert!(quiet.tab_windows().is_empty());

        for hwnd in [other_tab, tab, top] {
            destroy(hwnd);
        }
    }

    #[test]
    fn destroying_a_mapped_child_window_for_real_reports_the_tab_detached() {
        let class = "ESR.Test.TabDetach.Live";
        let _sta = crate::shell_util::StaGuard::init();
        let (mut source, log) = fold_source(class);
        let top = window(class, None);
        let tab = window("ESR.Test.TabDetach.LiveTab", Some(top));
        source.start().expect("start");
        let from = source.keys().key_of(hwnd_value(top));
        source.tab_windows().refresh(hwnd_value(top), [hwnd_value(tab)]);

        destroy(tab);
        pump_for(Duration::from_millis(500));
        assert_eq!(detached(&log), vec![from], "{:?}", log.borrow());
        assert!(source.tab_windows().is_empty());

        source.stop();
        destroy(top);
    }
}
