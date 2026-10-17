//! The Windows half of the runtime: port implementations for the `esr-win`
//! adapters, the [`WinHost`], the hidden `ESR.Engine` window, and the Engine
//! thread's message loop.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, Sender};
use std::thread::JoinHandle;

use esr_core::engine::{
    Environment, Event, ExplorerOptionValues, FoldProbeId, IoErrorKind, ItemId, Millis, OpId,
    ProbeId, RequestId, SessionFileRead, ShellError, TabKey, TabStripState, WinKey,
};
use esr_core::model::{Display, LocationId, Placement, Timestamp};
use esr_core::plan::placement::FinalPlacement;
use esr_core::store::clock::add_millis;
use esr_core::store::{UNKNOWN_TIME, WallClock};
use esr_win::display_info::DisplayInfo;
use esr_win::explorer_options::{ExplorerOptions, launch_to_context};
use esr_win::file_store::DurableFileStore;
use esr_win::fold_probe::FoldContextProbe;
use esr_win::hider::CandidateHider;
use esr_win::log_sink::LogSink;
use esr_win::navigator::{NavigatorHandle, ShellNavigator};
use esr_win::prober::LocationProber;
use esr_win::scanner::WindowScanner;
use esr_win::session_end::{EndSessionMessage, QUERY_END_SESSION_ALLOW, SessionEndListener};
use esr_win::shell_windows_source::{ShellSourceHandler, ShellWindowsSource};
use esr_win::startup::{self as win_startup, StartupRegistration};
use esr_win::tab_strip::TabStripReader;
use esr_win::toast::ToastNotifier;
use esr_win::window_event_source::WindowEventSource;
use windows::Win32::Foundation::{FILETIME, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Storage::FileSystem::FileTimeToLocalFileTime;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::{
    GetSystemTimeAsFileTime, GetTickCount, GetTickCount64,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::Win32::UI::WindowsAndMessaging::{
    ChangeWindowMessageFilterEx, CreateWindowExW, DefWindowProcW, DestroyWindow,
    DispatchMessageW, GetMessageW, KillTimer, MSG, MSGFLT_ALLOW, PostMessageW, PostQuitMessage,
    RegisterClassW, RegisterWindowMessageW, SetTimer, TranslateMessage, WM_APP, WM_CLOSE,
    WM_DISPLAYCHANGE, WM_ENDSESSION, WM_QUERYENDSESSION, WM_SETTINGCHANGE, WM_TIMER, WNDCLASSW,
    WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::{HSTRING, PCWSTR, w};

use super::driver::{END_SESSION_BLOCK_MS, Host, Inbound, Mailbox, RuntimeCore};
use crate::instance::{ENGINE_WINDOW_CLASS, InstanceMessages, SecondaryAction};
use crate::ports::{
    Displays, FileStore, FoldProbe, Hider, LogWriter, Navigator, Notifier, Options, Prober, SharedSink, Startup,
    StartupMechanism, TabSource, TabStrip, WindowEvents, WindowTabs,
};
use crate::ui::{UiCommand, UiHandle};

// ---------------------------------------------------------------------------
// Clocks
// ---------------------------------------------------------------------------

/// The monotonic [`Millis`] clock (`GetTickCount64`).
pub fn now() -> Millis {
    // SAFETY: no preconditions.
    Millis(unsafe { GetTickCount64() })
}

/// FILETIME ticks (100 ns) between 1601-01-01 and 1970-01-01.
const UNIX_EPOCH_FILETIME: i64 = 116_444_736_000_000_000;

fn filetime_ticks(ft: FILETIME) -> i64 {
    ((u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime)) as i64
}

/// The local time from a UTC and a local FILETIME reading.
pub fn timestamp_from_filetimes(utc_ticks: i64, local_ticks: i64) -> Timestamp {
    let offset_minutes = ((local_ticks - utc_ticks) / 600_000_000).clamp(-1_440, 1_440) as i16;
    let local_ms = (local_ticks - UNIX_EPOCH_FILETIME).div_euclid(10_000);
    add_millis(Timestamp { offset_minutes, ..UNKNOWN_TIME }, local_ms)
}

/// The local wall-clock time now, with its UTC offset.
pub fn local_now() -> Timestamp {
    // SAFETY: plain reads into owned values.
    let utc = unsafe { GetSystemTimeAsFileTime() };
    let mut local = FILETIME::default();
    // SAFETY: valid pointers to owned values.
    let local = match unsafe { FileTimeToLocalFileTime(&utc, &mut local) } {
        Ok(()) => local,
        Err(_) => utc,
    };
    timestamp_from_filetimes(filetime_ticks(utc), filetime_ticks(local))
}

/// The time of the last user input on the [`Millis`] clock, given `now`.
pub fn last_input_at(now: Millis) -> Option<Millis> {
    let mut info = LASTINPUTINFO { cbSize: size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    // SAFETY: valid, sized out structure.
    if !unsafe { GetLastInputInfo(&mut info) }.as_bool() {
        return None;
    }
    // SAFETY: no preconditions. Both values are 32-bit tick counts.
    let ago = unsafe { GetTickCount() }.wrapping_sub(info.dwTime);
    Some(Millis(now.0.saturating_sub(u64::from(ago))))
}

/// Fresh environment readings for the Engine at `now`.
pub fn read_environment(now: Millis) -> Environment {
    Environment {
        displays: DisplayInfo::new().displays(),
        options: ExplorerOptions::new().read(),
        last_input_at: last_input_at(now),
        wall_clock: Some(WallClock { at: now, local: local_now() }),
        launch_to_context: launch_to_context(),
        placeholder_dir: esr_win::placeholder::dir_wide(),
    }
}

// ---------------------------------------------------------------------------
// Port implementations
// ---------------------------------------------------------------------------

fn shell_error(context: &str, e: &windows::core::Error) -> ShellError {
    ShellError { code: e.code().0, message: format!("{context}: {}", e.message()) }
}

impl Navigator for ShellNavigator {
    fn open_window(
        &mut self,
        request: RequestId,
        location: &LocationId,
        placement: &FinalPlacement,
    ) -> Result<(), ShellError> {
        ShellNavigator::open_window(self, request, location, placement)
    }
    fn apply_placement(&mut self, win: WinKey, placement: &FinalPlacement) -> Result<(), ShellError> {
        ShellNavigator::apply_placement(self, win, placement)
    }
    fn create_tab(&mut self, win: WinKey) -> Result<(), ShellError> {
        ShellNavigator::create_tab(self, win)
    }
    fn navigate(&mut self, tab: TabKey, location: &LocationId) -> Result<(), ShellError> {
        ShellNavigator::navigate(self, tab, location)
    }
    fn close_tab(&mut self, tab: TabKey) -> Result<(), ShellError> {
        ShellNavigator::close_tab(self, tab)
    }
    fn select_items(&mut self, tab: TabKey, items: &[ItemId]) -> Result<(), ShellError> {
        ShellNavigator::select_items(self, tab, items)
    }
    fn reveal(&mut self, win: WinKey) -> Result<(), ShellError> {
        ShellNavigator::reveal(self, win)
    }
    fn set_z_order(&mut self, order: &[WinKey]) -> Result<(), ShellError> {
        ShellNavigator::set_z_order(self, order)
    }
    /// Falling back to z-order only (no foreground) is not an error.
    fn focus(&mut self, win: WinKey) -> Result<(), ShellError> {
        ShellNavigator::focus(self, win).map(|_| ())
    }
    fn close_window(&mut self, win: WinKey) -> Result<(), ShellError> {
        ShellNavigator::close_window(self, win)
    }
    fn reveal_all(&mut self) {
        ShellNavigator::reveal_all(self);
    }
    fn window_destroyed(&mut self, win: WinKey) {
        ShellNavigator::window_destroyed(self, win);
    }
}

impl Hider for CandidateHider {
    fn hide(&mut self, win: WinKey) -> Result<(), ShellError> {
        CandidateHider::hide(self, win)
    }
    fn reveal(&mut self, win: WinKey) -> Result<(), ShellError> {
        CandidateHider::reveal(self, win)
    }
    fn sweep(&mut self) -> u32 {
        CandidateHider::sweep(self)
    }
}

/// The hider when its thread could not be started: nothing is ever hidden,
/// so a hide reports why (the fold goes on with `ERROR operation=fold-hide`,
/// Req 14.16) and a reveal has nothing to undo.
pub struct NoHider(pub ShellError);

impl Hider for NoHider {
    fn hide(&mut self, _: WinKey) -> Result<(), ShellError> {
        Err(self.0.clone())
    }
    fn reveal(&mut self, _: WinKey) -> Result<(), ShellError> {
        Ok(())
    }
    fn sweep(&mut self) -> u32 {
        0
    }
}

impl FoldProbe for FoldContextProbe {
    fn probe_fold_context(&mut self, probe: FoldProbeId, candidate: WinKey) {
        FoldContextProbe::probe(self, probe, candidate);
    }
}

/// The `TabStrip` port: the `TabStripReader`, plus the sink that receives the
/// outcome of an asynchronous `ActivateTab`.
pub struct UiaTabStrip {
    pub reader: Arc<TabStripReader>,
    pub sink: SharedSink,
}

impl TabStrip for UiaTabStrip {
    fn read(&mut self, win: WinKey) -> Result<TabStripState, ShellError> {
        TabStripReader::read(&self.reader, win)
    }
    /// Queued on the UIA worker, so the Engine thread never waits for UIA
    /// (the activation waits there for the strip to catch up with the tab
    /// count). A failure arrives as `OperationFailed`; a success (the tab
    /// was seen still selected after the `Select`) as `ActivationConfirmed`,
    /// and the reader also rescans the window.
    fn activate(&mut self, op: OpId, win: WinKey, index: usize) -> Result<(), ShellError> {
        let sink = self.sink.clone();
        TabStripReader::activate(
            &self.reader,
            win,
            index,
            Box::new(move |result| {
                let event = match result {
                    Ok(()) => Event::ActivationConfirmed { op },
                    Err(error) => Event::OperationFailed { op, error },
                };
                crate::ports::EventSink::post(&*sink, event);
            }),
        )
    }
    fn watch(&mut self, win: WinKey) -> Result<(), ShellError> {
        TabStripReader::watch(&self.reader, win)
    }
    fn unwatch(&mut self, win: WinKey) {
        TabStripReader::unwatch(&self.reader, win);
    }
}

impl Prober for LocationProber {
    fn probe(&mut self, probe: ProbeId, location: &LocationId) {
        LocationProber::probe(self, probe, location.clone());
    }
}

impl FileStore for DurableFileStore {
    fn read_session(&mut self) -> SessionFileRead {
        DurableFileStore::read_session(self)
    }
    fn write_session(&mut self, seq: u64, text: &str, urgent: bool) {
        DurableFileStore::write_session(self, seq, text, urgent);
    }
    fn preserve_backup(&mut self, seq: u64, content: &[u8]) {
        DurableFileStore::preserve_backup(self, seq, content);
    }
    fn flush(&mut self, timeout_ms: u32) -> bool {
        DurableFileStore::flush(self, timeout_ms)
    }
    fn read_settings(&mut self) -> Result<Option<String>, IoErrorKind> {
        DurableFileStore::read_settings(self)
    }
    fn write_settings(&mut self, text: &str) -> Result<(), IoErrorKind> {
        DurableFileStore::write_settings(self, text)
    }
}

impl LogWriter for LogSink {
    fn append(&mut self, line: &str) {
        LogSink::append(self, line);
    }
}

impl Displays for DisplayInfo {
    fn displays(&mut self) -> Vec<Display> {
        DisplayInfo::displays(self)
    }
}

impl Options for ExplorerOptions {
    fn read(&mut self) -> ExplorerOptionValues {
        ExplorerOptions::read(self)
    }
    fn set_persist_browsers(&mut self, enabled: bool) -> Result<(), IoErrorKind> {
        ExplorerOptions::set_persist_browsers(self, enabled)
    }
}

impl Notifier for ToastNotifier {
    fn notify_omitted(&mut self, omitted_tabs: u32) -> Result<(), ShellError> {
        ToastNotifier::notify_omitted(self, omitted_tabs)
    }
}

impl Startup for StartupRegistration {
    fn set_enabled(&mut self, enabled: bool) -> Result<StartupMechanism, ShellError> {
        match StartupRegistration::set_enabled(self, enabled) {
            Ok(win_startup::StartupMechanism::LogonTask) => Ok(StartupMechanism::LogonTask),
            Ok(win_startup::StartupMechanism::RunKey) => Ok(StartupMechanism::RunKey),
            Err(e) => Err(shell_error("start at sign-in", &e)),
        }
    }
    fn unregister(&mut self) -> Result<(), ShellError> {
        StartupRegistration::unregister(self).map_err(|e| shell_error("unregister start at sign-in", &e))
    }
}

/// Start-at-sign-in when the registration could not be configured.
struct NoStartup(ShellError);

impl Startup for NoStartup {
    fn set_enabled(&mut self, _: bool) -> Result<StartupMechanism, ShellError> {
        Err(self.0.clone())
    }
    fn unregister(&mut self) -> Result<(), ShellError> {
        Err(self.0.clone())
    }
}

impl TabSource for ShellWindowsSource {
    fn attach(&mut self) -> Result<(), ShellError> {
        ShellWindowsSource::attach(self)
    }
    fn detach(&mut self) {
        ShellWindowsSource::detach(self);
    }
    fn windows(&mut self) -> Result<Vec<WinKey>, ShellError> {
        ShellWindowsSource::windows(self)
    }
    fn tabs_of(&mut self, win: WinKey) -> Result<WindowTabs, ShellError> {
        let t = ShellWindowsSource::tabs_of(self, win)?;
        Ok(WindowTabs { pid: t.pid, tabs: t.tabs, active: t.active })
    }
}

impl WindowEvents for WindowEventSource {
    fn start(&mut self) -> Result<(), ShellError> {
        WindowEventSource::start(self)
    }
    fn watch_process(&mut self, pid: u32) -> Result<(), ShellError> {
        WindowEventSource::watch_process(self, pid)
    }
    fn stop(&mut self) {
        WindowEventSource::stop(self);
    }
    fn placement(&mut self, win: WinKey) -> Result<Placement, ShellError> {
        WindowEventSource::placement(self, win)
    }
}

// ---------------------------------------------------------------------------
// Log thread
// ---------------------------------------------------------------------------

/// Activity_Log appends on their own thread, so a slow disk never blocks
/// the Engine (design.md "Threads": Writer). Dropping it writes what is
/// queued and joins the thread.
pub struct LogThread {
    tx: Option<Sender<String>>,
    join: Option<JoinHandle<()>>,
}

impl LogThread {
    pub fn spawn(dir: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel::<String>();
        let join = std::thread::Builder::new()
            .name("esr-log".into())
            .spawn(move || {
                let mut sink = LogSink::open(dir);
                for line in rx {
                    sink.append(&line);
                }
            })
            .ok();
        Self { tx: Some(tx), join }
    }

    pub fn writer(&self) -> LogThreadWriter {
        LogThreadWriter(self.tx.clone())
    }
}

impl Drop for LogThread {
    fn drop(&mut self) {
        self.tx = None;
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// A [`LogWriter`] that queues lines for the [`LogThread`].
pub struct LogThreadWriter(Option<Sender<String>>);

impl LogWriter for LogThreadWriter {
    fn append(&mut self, line: &str) {
        if let Some(tx) = &self.0 {
            let _ = tx.send(line.to_owned());
        }
    }
}

// ---------------------------------------------------------------------------
// Host
// ---------------------------------------------------------------------------

/// Routes the shell window source's output: events to the mailbox, claims
/// and tab registrations to the navigator (through its handle, which is
/// safe to use while the navigator itself is mid-call).
pub struct SourceHandler {
    pub inbox: Arc<Mailbox>,
    pub navigator: NavigatorHandle,
    /// Windows a running restoration builds ([`Host::restoration_builds`]).
    pub building: Building,
}

/// The windows a running restoration builds, shared by [`WinHost`] and the
/// [`SourceHandler`].
pub type Building = Rc<RefCell<HashSet<WinKey>>>;

impl ShellSourceHandler for SourceHandler {
    fn post(&self, event: esr_core::engine::Event) {
        crate::ports::EventSink::post(&*self.inbox, event);
    }
    fn window_changed(&self, win: WinKey) {
        crate::ports::EventSink::window_changed(&*self.inbox, win);
    }
    fn tab_registered(&self, win: WinKey, tab: TabKey) {
        self.navigator.tab_registered(win, tab);
    }
    fn tab_browser(&self, tab: TabKey, browser: &windows::Win32::UI::Shell::IWebBrowser2) {
        self.navigator.tab_browser(tab, browser);
    }
    fn attach_remote(
        &self,
        win: WinKey,
        tab: TabKey,
        browser: &windows::Win32::UI::Shell::IWebBrowser2,
        sink: esr_win::browser_worker::SinkFactory,
        failed: esr_win::browser_worker::OnFailure,
    ) -> bool {
        self.navigator.attach_browser(win, tab, browser, Some(sink), failed)
    }
    fn detach_remote(&self, tab: TabKey) {
        self.navigator.detach_browser(tab);
    }
    fn claim(&self, win: WinKey, initial: &LocationId) -> Option<RequestId> {
        self.navigator.claim(win, initial)
    }
    fn expected_location(&self, tab: TabKey, parsing_name: &[u16]) -> Option<LocationId> {
        self.navigator.expected_location(tab, parsing_name)
    }
    fn creating_tab(&self, win: WinKey) -> bool {
        self.navigator.creating_tab(win)
    }
    fn navigation_expected(&self, tab: TabKey) -> bool {
        self.navigator.navigation_expected(tab)
    }
    fn cached_locations_current(&self, win: WinKey) -> bool {
        self.building.try_borrow().is_ok_and(|b| b.contains(&win))
    }
    /// Drains the mailbox into the core now (a no-op while the core is busy
    /// further up the stack).
    fn flush(&self) {
        if let Some(live) = live() {
            live.pump();
        }
    }
}

/// [`Host`] on the Engine thread.
pub struct WinHost {
    pub scanner: Rc<WindowScanner>,
    pub source: Rc<RefCell<ShellWindowsSource>>,
    pub events: Rc<RefCell<WindowEventSource>>,
    pub ui: Option<UiHandle>,
    pub building: Building,
}

impl Host for WinHost {
    fn rescan_window(&mut self, win: WinKey) {
        self.scanner.window_changed(win);
    }

    fn scan_now(&mut self, win: WinKey) {
        self.scanner.scan_now(win);
    }

    fn rescan_all(&mut self) -> bool {
        let Ok(mut source) = self.source.try_borrow_mut() else { return false };
        let windows = source.windows();
        drop(source);
        // While the shell is lost the source rescans every window itself
        // when it reconnects (`ShellAvailable`).
        for win in windows.unwrap_or_default() {
            self.scanner.window_changed(win);
        }
        true
    }

    fn watch_process(&mut self, pid: u32) -> Result<(), ShellError> {
        match self.events.try_borrow_mut() {
            Ok(mut events) => events.watch_process(pid),
            Err(_) => Err(ShellError { code: 0, message: "window event source busy".into() }),
        }
    }

    fn forget_window(&mut self, win: WinKey) {
        self.scanner.forget(win);
    }

    fn cancel_scan(&mut self, win: WinKey) {
        self.scanner.cancel(win);
    }

    fn watch_strip(&mut self, win: WinKey, on: bool) {
        self.scanner.set_strip_watch(win, on);
    }

    fn pause_location_hooks(&mut self, paused: bool) -> bool {
        let Ok(mut events) = self.events.try_borrow_mut() else { return false };
        events.pause_location_hooks(paused);
        true
    }

    fn restoration_builds(&mut self, win: WinKey, on: bool) {
        let mut building = self.building.borrow_mut();
        if on {
            building.insert(win);
        } else {
            building.remove(&win);
        }
    }

    fn environment(&mut self, now: Millis) -> Environment {
        read_environment(now)
    }

    fn local_now(&mut self) -> Timestamp {
        local_now()
    }

    fn ui(&mut self, command: UiCommand) {
        if let Some(ui) = &self.ui {
            ui.send(command);
        }
    }
}

/// Start-at-sign-in registration for `exe` (or a port that reports why
/// there is none), under the base name `name` (`None`: the production
/// `ExplorerSessionRecall`; see `RuntimeOptions::startup_name`).
pub fn startup_port(exe: Option<PathBuf>, name: Option<&str>) -> Box<dyn Startup> {
    let exe = match exe.map(Ok).unwrap_or_else(std::env::current_exe) {
        Ok(exe) => exe,
        Err(e) => {
            return Box::new(NoStartup(ShellError {
                code: e.raw_os_error().unwrap_or(0),
                message: format!("executable path: {e}"),
            }));
        }
    };
    let config = match name {
        None => win_startup::StartupConfig::for_current_user(exe),
        Some(name) => esr_win::file_store::current_user_sid()
            .map(|sid| win_startup::StartupConfig::with_base_name(name, &sid, exe)),
    };
    match config {
        Ok(config) => Box::new(StartupRegistration::new(config)),
        Err(e) => Box::new(NoStartup(shell_error("start at sign-in", &e))),
    }
}

// ---------------------------------------------------------------------------
// The Engine window and message loop
// ---------------------------------------------------------------------------

/// Posted to `ESR.Engine` when the mailbox has items.
pub const WM_ESR_WAKE: u32 = WM_APP + 0x01;
const TIMER_ENGINE: usize = 1;
/// Retry delay for a tick that arrived while the core was busy (nested).
const BUSY_RETRY_MS: u32 = 10;

/// The Engine thread's state, reachable from the window procedure.
pub struct Live {
    pub core: RefCell<RuntimeCore>,
    pub inbox: Arc<Mailbox>,
    pub hwnd: HWND,
    pub messages: InstanceMessages,
    pub taskbar_created: u32,
    pub listener: SessionEndListener,
    pub source: Rc<RefCell<ShellWindowsSource>>,
    quit_posted: Cell<bool>,
}

thread_local! {
    static LIVE: RefCell<Option<Rc<Live>>> = const { RefCell::new(None) };
}

fn live() -> Option<Rc<Live>> {
    LIVE.try_with(|l| l.try_borrow().ok().and_then(|l| l.clone())).ok().flatten()
}

impl Live {
    pub fn new(
        core: RuntimeCore,
        inbox: Arc<Mailbox>,
        hwnd: HWND,
        messages: InstanceMessages,
        taskbar_created: u32,
        listener: SessionEndListener,
        source: Rc<RefCell<ShellWindowsSource>>,
    ) -> Rc<Self> {
        Rc::new(Self {
            core: RefCell::new(core),
            inbox,
            hwnd,
            messages,
            taskbar_created,
            listener,
            source,
            quit_posted: Cell::new(false),
        })
    }

    /// Makes `self` the window procedure's target on this thread.
    pub fn install(self: &Rc<Self>) {
        let _ = LIVE.try_with(|l| *l.borrow_mut() = Some(self.clone()));
    }

    pub fn uninstall() {
        let _ = LIVE.try_with(|l| l.borrow_mut().take());
    }

    /// Drains the mailbox unless the core is already busy further up the
    /// stack (a message pumped during a COM call); that drain picks the
    /// items up.
    pub fn pump(&self) {
        let Ok(mut core) = self.core.try_borrow_mut() else { return };
        core.drain(&self.inbox, &mut now);
        drop(core);
        self.after();
    }

    fn tick(&self) {
        // SAFETY: our own window and timer.
        let _ = unsafe { KillTimer(Some(self.hwnd), TIMER_ENGINE) };
        let Ok(mut core) = self.core.try_borrow_mut() else {
            // SAFETY: as above.
            unsafe { SetTimer(Some(self.hwnd), TIMER_ENGINE, BUSY_RETRY_MS, None) };
            return;
        };
        core.tick(now());
        core.drain(&self.inbox, &mut now);
        drop(core);
        self.after();
    }

    /// Re-arms the timer for the next deadline and quits after `Exit`.
    fn after(&self) {
        let Ok(core) = self.core.try_borrow() else { return };
        if core.exit_requested() {
            if !self.quit_posted.replace(true) {
                // SAFETY: no preconditions.
                unsafe { PostQuitMessage(0) };
            }
            return;
        }
        if let Some(at) = core.next_deadline() {
            let delay = at.since(now()).clamp(1, u64::from(u32::MAX >> 1)) as u32;
            // SAFETY: our own window; replaces a pending timer with the same id.
            unsafe { SetTimer(Some(self.hwnd), TIMER_ENGINE, delay, None) };
        } else {
            // SAFETY: as above.
            let _ = unsafe { KillTimer(Some(self.hwnd), TIMER_ENGINE) };
        }
    }

    fn query_end_session(&self, lparam: isize) -> LRESULT {
        let Some(EndSessionMessage::Query(kind)) =
            self.listener.on_message(WM_QUERYENDSESSION, 0, lparam)
        else {
            return QUERY_END_SESSION_ALLOW;
        };
        match self.core.try_borrow_mut() {
            Ok(mut core) => {
                core.end_session(kind, &self.inbox, &mut now, END_SESSION_BLOCK_MS);
                drop(core);
                self.after();
            }
            // Re-entrant: the outer drain delivers the query; the write is
            // flushed at WM_ENDSESSION.
            Err(_) => self.inbox.push(Inbound::Event(esr_core::engine::Event::EndSessionQuery(kind))),
        }
        // Never ShutdownBlockReasonCreate; always allow the session end.
        QUERY_END_SESSION_ALLOW
    }

    fn end_session(&self, ending: bool) {
        if ending {
            if let Ok(mut core) = self.core.try_borrow_mut() {
                let start = now();
                core.drain(&self.inbox, &mut now);
                core.flush_urgent(&self.inbox, &mut now, start, END_SESSION_BLOCK_MS);
            }
        } else {
            self.inbox.push(Inbound::Event(esr_core::engine::Event::EndSessionCancelled));
        }
    }

    fn on_message(&self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_ESR_WAKE => {
                self.pump();
                Some(LRESULT(0))
            }
            WM_TIMER if wparam.0 == TIMER_ENGINE => {
                self.tick();
                Some(LRESULT(0))
            }
            WM_QUERYENDSESSION => Some(self.query_end_session(lparam.0)),
            WM_ENDSESSION => {
                self.end_session(wparam.0 != 0);
                Some(LRESULT(0))
            }
            WM_DISPLAYCHANGE | WM_SETTINGCHANGE => {
                self.inbox.push(Inbound::EnvironmentStale);
                None
            }
            // The Engine window is never closed from outside; exit goes
            // through ExitRequested (Settings or ESR_EXIT).
            WM_CLOSE => Some(LRESULT(0)),
            m if m != 0 && m == self.taskbar_created => {
                // The source's own window receives it too; forwarding makes
                // the reconnect independent of that (Req 1.7).
                if let Ok(source) = self.source.try_borrow() {
                    source.on_taskbar_created();
                }
                self.inbox.push(Inbound::EnvironmentStale);
                Some(LRESULT(0))
            }
            m => match self.messages.recognise(m)? {
                SecondaryAction::ShowSettings => {
                    self.inbox.push(Inbound::ShowSettings);
                    Some(LRESULT(1))
                }
                SecondaryAction::Ping => Some(LRESULT(1)),
                SecondaryAction::Exit => {
                    self.inbox.push(Inbound::Event(esr_core::engine::Event::ExitRequested));
                    Some(LRESULT(1))
                }
            },
        }
    }
}

extern "system" fn engine_window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if let Some(live) = live().filter(|l| l.hwnd == hwnd)
        && let Some(result) = live.on_message(msg, wparam, lparam)
    {
        return result;
    }
    // SAFETY: default processing for our own window.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Creates the hidden top-level `ESR.Engine` window (top-level, not
/// message-only: it must receive `WM_QUERYENDSESSION`, `TaskbarCreated`, and
/// be found by `FindWindowW` from a second instance).
pub fn create_engine_window() -> Result<HWND, ShellError> {
    let class = HSTRING::from(ENGINE_WINDOW_CLASS);
    // SAFETY: plain calls; the class name outlives them.
    unsafe {
        let instance = GetModuleHandleW(None).map_err(|e| shell_error("GetModuleHandleW", &e))?;
        let wc = WNDCLASSW {
            lpfnWndProc: Some(engine_window_proc),
            hInstance: instance.into(),
            lpszClassName: PCWSTR(class.as_ptr()),
            ..Default::default()
        };
        // Fails harmlessly when already registered (a second run in-process).
        RegisterClassW(&wc);
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            PCWSTR(class.as_ptr()),
            w!("Explorer Session Recall"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .map_err(|e| shell_error("CreateWindowExW(ESR.Engine)", &e))
    }
}

/// Registers `TaskbarCreated` and lets it and the single-instance messages
/// through UIPI (an instance at another integrity level may send them).
pub fn register_messages(hwnd: HWND, messages: &InstanceMessages) -> u32 {
    // SAFETY: static string.
    let taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
    for msg in [taskbar_created, messages.show_settings, messages.ping, messages.exit] {
        if msg != 0 {
            // SAFETY: our own window.
            let _ = unsafe { ChangeWindowMessageFilterEx(hwnd, msg, MSGFLT_ALLOW, None) };
        }
    }
    taskbar_created
}

/// The mailbox waker: posts [`WM_ESR_WAKE`] to the Engine window.
pub fn waker(hwnd: HWND) -> Box<dyn Fn() + Send + Sync> {
    let raw = hwnd.0 as isize;
    Box::new(move || {
        // SAFETY: posting to a window of (possibly) another thread; fails
        // harmlessly once it is destroyed.
        let _ = unsafe {
            PostMessageW(Some(HWND(raw as *mut _)), WM_ESR_WAKE, WPARAM(0), LPARAM(0))
        };
    })
}

/// Runs the message loop until `WM_QUIT`.
pub fn message_loop() {
    let mut msg = MSG::default();
    // SAFETY: standard message loop on this thread.
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

pub fn destroy_window(hwnd: HWND) {
    // SAFETY: our own window, destroyed on its thread.
    let _ = unsafe { DestroyWindow(hwnd) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_time_from_filetimes() {
        // 2025-01-31 08:15:02.123 UTC, local = UTC+01:00.
        let utc_ms: i64 = 1_738_311_302_123;
        let utc = UNIX_EPOCH_FILETIME + utc_ms * 10_000;
        let local = utc + 60 * 600_000_000;
        let ts = timestamp_from_filetimes(utc, local);
        assert_eq!(
            ts,
            Timestamp { year: 2025, month: 1, day: 31, hour: 9, minute: 15, second: 2, millis: 123, offset_minutes: 60 }
        );
        // Negative offsets.
        let ts = timestamp_from_filetimes(utc, utc - 330 * 600_000_000);
        assert_eq!((ts.hour, ts.minute, ts.offset_minutes), (2, 45, -330));
        assert!(local_now().is_valid());
    }

    #[test]
    fn last_input_is_not_in_the_future() {
        let n = now();
        if let Some(at) = last_input_at(n) {
            assert!(at <= n);
        }
    }
}
