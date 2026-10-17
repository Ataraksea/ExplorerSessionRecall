//! Adapter port traits (`TabSource`, `WindowEvents`, `TabStrip`, `Navigator`,
//! `Prober`, `FileStore`, `LogWriter`, `Displays`, `Options`, `Notifier`,
//! `Startup`, and for tab folding `Hider` and `FoldProbe`), implemented for the `esr-win` adapters (in this crate, because
//! `esr-win` does not depend on `esr-app`) and by `esr-sim`.
//!
//! The traits use only `esr-core` types and plain ids, never `windows` crate
//! types. Ports that report asynchronous results receive an [`EventSink`] when
//! they are constructed; the runtime stamps each posted event with the current
//! [`Millis`](esr_core::engine::Millis) before handing it to the `Engine`.
//!
//! Requirements 2.1, 4.1, 14.2, 14.9, 14.13, 14.16.

use std::sync::Arc;

use esr_core::engine::{
    Event, ExplorerOptionValues, FoldProbeId, IoErrorKind, ItemId, ObservedLocation, OpId,
    ProbeId, RequestId, SessionFileRead, ShellError, TabKey, TabStripState, WinKey,
};
use esr_core::model::{Display, LocationId, Placement};
use esr_core::plan::placement::FinalPlacement;

/// Where adapters deliver what they observe.
pub trait EventSink {
    /// Deliver an event to the Engine.
    fn post(&self, event: Event);

    /// Something about `win` changed (tab registered, title or placement
    /// changed, tab strip structure changed, …). The scanner collapses these
    /// per window and emits one `WindowScanned` after its debounce (100 ms).
    fn window_changed(&self, win: WinKey);
}

/// A sink that adapters on other threads (UIA worker, probe pool, writer) can share.
pub type SharedSink = Arc<dyn EventSink + Send + Sync>;

/// The tabs `TabSource::tabs_of` found in one window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowTabs {
    pub pid: u32,
    /// `(tab, location)` in ShellWindows enumeration order.
    pub tabs: Vec<(TabKey, ObservedLocation)>,
    /// The active tab if this source can tell (e.g. `QueryActiveShellView`).
    pub active: Option<TabKey>,
}

/// How start-at-sign-in is registered (Req 1.1, 1.9).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StartupMechanism {
    /// Task Scheduler logon task (meets the 10 s target).
    LogonTask,
    /// `HKCU\...\Run` fallback (startup may be delayed by Windows).
    RunKey,
}

/// `IShellWindows` and per-tab browser events (`ShellWindowsSource`).
///
/// Posts `TabRevoked`, `NavigationCompleted`, `InitialLocationKnown`,
/// `WindowOpened` (claims of outstanding `OpenWindow` requests), `TabCreated`,
/// `ShellLost`, and `ShellAvailable`, and calls `window_changed` on tab
/// registration.
pub trait TabSource {
    /// Connect to the shell's window collection and attach event sinks.
    fn attach(&mut self) -> Result<(), ShellError>;
    /// Disconnect all sinks.
    fn detach(&mut self);
    /// Every open top-level Explorer window.
    fn windows(&mut self) -> Result<Vec<WinKey>, ShellError>;
    /// The tabs currently hosted by `win`.
    fn tabs_of(&mut self, win: WinKey) -> Result<WindowTabs, ShellError>;
}

/// WinEvent hooks and process-exit waits (`WindowEventSource`).
///
/// Posts `WindowDestroyed`, `WindowActivated`, and `ProcessExited` (200 ms
/// after the process handle is signalled), and calls `window_changed` on
/// create, show, location, minimize/restore, move/size end, and name changes.
///
/// Tab folding (Req 14): the source also reports a new top-level Explorer
/// window at its first signal as `WindowAppeared { win, pid, bypass_keys,
/// desktop, foreground }` (Req 14.2, 14.4, 14.22), and the destruction of a
/// known window's tab window as `TabDetached { from }` (Req 14.3 b). Both are
/// delivered through the [`EventSink`] given at construction, so the trait
/// needs no extra methods for them.
pub trait WindowEvents {
    /// Install the global hooks.
    fn start(&mut self) -> Result<(), ShellError>;
    /// Install the per-process hooks and the exit wait for an Explorer PID.
    fn watch_process(&mut self, pid: u32) -> Result<(), ShellError>;
    /// Remove every hook and wait.
    fn stop(&mut self);
    /// Current Window_Placement of `win` in screen coordinates.
    fn placement(&mut self, win: WinKey) -> Result<Placement, ShellError>;
}

/// The XAML tab strip through UI Automation (`TabStripReader`).
pub trait TabStrip {
    /// `TabItem` names left to right, and the selected index.
    fn read(&mut self, win: WinKey) -> Result<TabStripState, ShellError>;
    /// `ActivateTab`: select the tab at visual position `index`
    /// (`SelectionItemPattern.Select`) without blocking. A later failure is
    /// posted as `OperationFailed { op }`, a success (the tab seen selected
    /// afterwards) as `ActivationConfirmed { op }` plus `window_changed(win)`.
    /// `Err` only if it could not be queued.
    fn activate(&mut self, op: OpId, win: WinKey, index: usize) -> Result<(), ShellError>;
    /// Subscribe to `StructureChanged`; changes are reported via `window_changed`.
    fn watch(&mut self, win: WinKey) -> Result<(), ShellError>;
    fn unwatch(&mut self, win: WinKey);
}

/// Window and tab operations used by restoration (`ShellNavigator`).
///
/// Results that Explorer reports later (`WindowOpened`, `TabCreated`,
/// `NavigationCompleted`) arrive through the sources; an `Err` here becomes
/// `OperationFailed` for the command's op.
pub trait Navigator {
    /// Open a hidden window on `location` that is placed at `placement` before
    /// its first frame. The window is claimed by `request`.
    fn open_window(
        &mut self,
        request: RequestId,
        location: &LocationId,
        placement: &FinalPlacement,
    ) -> Result<(), ShellError>;
    fn apply_placement(&mut self, win: WinKey, placement: &FinalPlacement) -> Result<(), ShellError>;
    /// Append one tab to `win`.
    fn create_tab(&mut self, win: WinKey) -> Result<(), ShellError>;
    /// Navigate `tab` without adding history.
    fn navigate(&mut self, tab: TabKey, location: &LocationId) -> Result<(), ShellError>;
    fn close_tab(&mut self, tab: TabKey) -> Result<(), ShellError>;
    /// Select exactly `items` in `tab`, focusing the first.
    fn select_items(&mut self, tab: TabKey, items: &[ItemId]) -> Result<(), ShellError>;
    /// Make a hidden window visible.
    fn reveal(&mut self, win: WinKey) -> Result<(), ShellError>;
    /// Stack windows so that `order[0]` is topmost and each lies under its predecessor.
    fn set_z_order(&mut self, order: &[WinKey]) -> Result<(), ShellError>;
    /// Give `win` keyboard focus.
    fn focus(&mut self, win: WinKey) -> Result<(), ShellError>;
    /// Close the whole window `win` (`WM_SYSCOMMAND SC_CLOSE`), used for a
    /// folded candidate (Req 14.1). Its destruction arrives as
    /// `WindowDestroyed`. The default reports the operation as unsupported.
    fn close_window(&mut self, _win: WinKey) -> Result<(), ShellError> {
        Err(ShellError { code: 0, message: "close_window is not supported by this navigator".to_owned() })
    }
    /// Make every window the navigator still hides visible (after a failed
    /// restoration, Req 4.10).
    fn reveal_all(&mut self) {}
    /// `win` was destroyed: forget it.
    fn window_destroyed(&mut self, _win: WinKey) {}
}

/// Folding_Candidate visibility (`CandidateHider`, own thread).
///
/// Setting the style of another thread's window can wait for that thread, so
/// implementations queue the work and never run it on the Engine thread.
pub trait Hider {
    /// Hide `win` with layered alpha 0 (Req 14.2). `Err` only if it could not
    /// be queued.
    fn hide(&mut self, win: WinKey) -> Result<(), ShellError>;
    /// Undo [`hide`](Hider::hide) for a window this hider hid: alpha 255 and
    /// `WS_EX_LAYERED` removed (Req 14.14). A window it did not hide is left
    /// alone, so a window the Restore_Engine hid is revealed by
    /// [`Navigator::reveal`].
    fn reveal(&mut self, win: WinKey) -> Result<(), ShellError>;
    /// Reveal every Explorer window that is layered with alpha 0, recovering a
    /// candidate left invisible by a monitor crash (Req 14.16). Returns how
    /// many windows were revealed.
    fn sweep(&mut self) -> u32;
}

/// Tab_Folding context probes (`FoldContextProbe`, on the probe pool).
pub trait FoldProbe {
    /// Read the open windows on `candidate`'s Virtual_Desktop and test which
    /// windows answer `WM_NULL` within the deadline. Posts
    /// `FoldContext { probe, .. }` (Req 14.9, 14.12, 14.13).
    fn probe_fold_context(&mut self, probe: FoldProbeId, candidate: WinKey);
}

/// Location availability probes (`LocationProber`).
pub trait Prober {
    /// Start a probe; `ProbeCompleted` is posted within the 5 s deadline.
    fn probe(&mut self, probe: ProbeId, location: &LocationId);
}

/// Session_File, backup copy, and settings storage (`DurableFileStore`).
pub trait FileStore {
    /// Read the Session_File at startup (after removing a leftover temp file).
    fn read_session(&mut self) -> SessionFileRead;
    /// Atomically replace the Session_File; `WriteCompleted { seq }` is posted.
    fn write_session(&mut self, seq: u64, text: &str, urgent: bool);
    /// Replace the backup copy with `content`; `WriteCompleted { seq }` is posted.
    fn preserve_backup(&mut self, seq: u64, content: &[u8]);
    /// Block until every queued write has completed or `timeout_ms` passed.
    /// Returns `true` when the queue drained (used at session end).
    fn flush(&mut self, timeout_ms: u32) -> bool;
    /// Settings text, or `None` when the file is absent.
    fn read_settings(&mut self) -> Result<Option<String>, IoErrorKind>;
    fn write_settings(&mut self, text: &str) -> Result<(), IoErrorKind>;
}

/// Activity_Log appends (`LogSink`). Rotation and write errors are handled
/// inside the writer and never reported (Req 13.7).
pub trait LogWriter {
    /// Append one already formatted line (without the line terminator).
    fn append(&mut self, line: &str);
}

/// Connected displays (`DisplayInfo`).
pub trait Displays {
    fn displays(&mut self) -> Vec<Display>;
}

/// File Explorer options (`ExplorerOptions`).
pub trait Options {
    fn read(&mut self) -> ExplorerOptionValues;
    /// Write `PersistBrowsers` only, then broadcast the setting change.
    fn set_persist_browsers(&mut self, enabled: bool) -> Result<(), IoErrorKind>;
}

/// Non-modal notification (`ToastNotifier`).
pub trait Notifier {
    fn notify_omitted(&mut self, omitted_tabs: u32) -> Result<(), ShellError>;
}

/// Start-at-sign-in registration (`StartupRegistration`).
pub trait Startup {
    /// Register (if needed) and enable or disable start at sign-in.
    fn set_enabled(&mut self, enabled: bool) -> Result<StartupMechanism, ShellError>;
    /// Remove the registration entirely (uninstall).
    fn unregister(&mut self) -> Result<(), ShellError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Every port must stay usable as a trait object so the runtime can hold
    /// either the Windows adapters or the simulator.
    #[allow(dead_code)]
    struct AllPorts {
        tabs: Box<dyn TabSource>,
        events: Box<dyn WindowEvents>,
        strip: Box<dyn TabStrip>,
        nav: Box<dyn Navigator>,
        prober: Box<dyn Prober>,
        hider: Box<dyn Hider>,
        fold_probe: Box<dyn FoldProbe>,
        files: Box<dyn FileStore>,
        log: Box<dyn LogWriter>,
        displays: Box<dyn Displays>,
        options: Box<dyn Options>,
        notifier: Box<dyn Notifier>,
        startup: Box<dyn Startup>,
        sink: SharedSink,
    }

    #[derive(Default)]
    struct Recorder {
        events: Mutex<Vec<Event>>,
        changed: Mutex<Vec<WinKey>>,
    }

    impl EventSink for Recorder {
        fn post(&self, event: Event) {
            self.events.lock().unwrap().push(event);
        }
        fn window_changed(&self, win: WinKey) {
            self.changed.lock().unwrap().push(win);
        }
    }

    #[test]
    fn shared_sink_crosses_threads() {
        let rec = Arc::new(Recorder::default());
        let sink: SharedSink = rec.clone();
        let win = WinKey { hwnd: 1, seq: 1 };
        std::thread::spawn(move || {
            sink.post(Event::ShellLost);
            sink.window_changed(win);
        })
        .join()
        .unwrap();
        assert_eq!(*rec.events.lock().unwrap(), vec![Event::ShellLost]);
        assert_eq!(*rec.changed.lock().unwrap(), vec![win]);
    }
}
