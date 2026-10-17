//! Runtime: the Engine STA thread with its hidden `ESR.Engine` window,
//! timers, and command dispatch, plus the UIA worker, probe pool, writer
//! thread, log thread, UI thread, and the top-level panic guard
//! (Requirements 1.1, 1.5, 1.7, 3.6, 10.7, 12.5; design.md "Threads").
//!
//! # Threads
//!
//! | Thread | Who | Role |
//! |---|---|---|
//! | Engine (the caller of [`run`]) | STA, message loop, `ESR.Engine` | [`RuntimeCore`]: the pure `Engine`, timers, command dispatch. Hosts the WinEvent hooks, the ShellWindows sinks, the window scanner, and the `ShellNavigator`. |
//! | UIA worker | `TabStripReader` (and the navigator's own) | Tab strip reads, `ActivateTab`, `StructureChanged`. |
//! | Probe pool | `LocationProber` | 4 threads, 5 s deadline. |
//! | Writer | `DurableFileStore` | Atomic Session_File and backup writes. |
//! | Log | [`win::LogThread`] | Activity_Log appends. |
//! | UI | `ui::UiThread` | Settings, prompt, messages; never blocks tracking (Req 10.7). |
//!
//! Every adapter reports into one [`Mailbox`]; the mailbox wakes the Engine
//! window (`WM_ESR_WAKE`), whose window procedure drains it into the
//! [`RuntimeCore`] (see `driver` for the event filtering and dispatch, which
//! is unit-tested without Explorer). The Engine's timers are one Win32
//! timer on the Engine window, armed for the core's next deadline. The
//! [`Millis`](esr_core::engine::Millis) clock is `GetTickCount64`.
//!
//! The Engine window also handles `WM_QUERYENDSESSION` (freeze, urgent
//! write, block â‰¤ 1.5 s, return `TRUE`; `ShutdownBlockReasonCreate` is never
//! called), `WM_ENDSESSION`, `TaskbarCreated` (forwarded to the shell window
//! source, Req 1.7), `WM_DISPLAYCHANGE`/`WM_SETTINGCHANGE` (fresh environment
//! readings), and the single-instance messages: `ESR_SHOW_SETTINGS` opens
//! Settings (Req 12.7), `ESR_EXIT` is `ExitRequested` (Req 12.5, 12.6),
//! `ESR_PING` does nothing.
//!
//! A message pumped while the core is busy further up the stack (during a
//! COM call made by a command) never re-enters it: wake-ups and timers are
//! picked up by the outer drain or retried, and a nested
//! `WM_QUERYENDSESSION` is queued with its write flushed at `WM_ENDSESSION`.
//!
//! # Process setup
//!
//! [`init_process`]: per-monitor DPI awareness v2 (all coordinates are
//! physical pixels, matching `Get/SetWindowPlacement` on Explorer's windows)
//! and `SetProcessShutdownParameters(0x3FF, SHUTDOWN_NORETRY)`, so this
//! process gets `WM_QUERYENDSESSION` before Explorer closes its windows.
//! [`install_panic_guard`]: a panic on any thread is logged as an `ERROR`
//! entry (`operation=panic`) and the process aborts (release builds use
//! `panic = "abort"` anyway); the persisted state already covers abrupt
//! termination (Req 3.8, 4.7).

mod driver;
mod win;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use esr_core::engine::{Event, SessionFileRead, StartupInput};
use esr_core::logfmt::entry::{LogEntry, format_entry};
use esr_core::settings::Settings;
use esr_win::file_store::DurableFileStore;
use esr_win::fold_probe::FoldContextProbe;
use esr_win::hider::CandidateHider;
use esr_win::log_sink::LogSink;
use esr_win::navigator::ShellNavigator;
use esr_win::prober::{DEFAULT_DEADLINE, DEFAULT_THREADS, LocationProber};
use esr_win::scanner::{DebounceConfig, ScanHandler, WindowScanner};
use esr_win::session_end::{SessionEndListener, configure_shutdown_priority};
use esr_win::shell_util::StaGuard;
use esr_win::shell_windows_source::ShellWindowsSource;
use esr_win::strategy::Strategies;
use esr_win::tab_strip::TabStripReader;
use esr_win::toast::ToastNotifier;
use esr_win::window_event_source::{WindowEventConfig, WindowEventSource, WindowSignal};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::GetShellWindow;

pub use driver::{
    DESTROY_HOLD_MS, DestroyFilter, END_SESSION_BLOCK_MS, Host, Inbound, Mailbox, OpMap, Ports,
    REVOKE_RECENT_MS, RuntimeCore, TimerWheel, panic_entry,
};
pub use win::{
    LogThread, LogThreadWriter, SourceHandler, WM_ESR_WAKE, WinHost, last_input_at, local_now,
    now, read_environment,
};

use crate::instance::InstanceMessages;
use crate::ports::{EventSink, SharedSink};
use crate::ui::UiThread;

/// Exit code of a normal exit.
pub const EXIT_OK: i32 = 0;
/// Exit code when the Engine thread could not be set up.
pub const EXIT_SETUP_FAILED: i32 = 1;

/// Subdirectory of the data directory holding deferred tabs' placeholders.
pub const PLACEHOLDER_SUBDIR: &str = "deferred";

/// Per-monitor DPI awareness v2 and the shutdown priority. Idempotent;
/// failures are ignored (the manifest may already have set the awareness).
pub fn init_process() {
    // SAFETY: process-wide setting with no other preconditions; fails with
    // ACCESS_DENIED when already set (manifest), which is fine.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let _ = configure_shutdown_priority();
}

/// Installs the panic hook: the panic is appended to the Activity_Log in
/// `data_dir` as an `ERROR` entry, then the process aborts.
pub fn install_panic_guard(data_dir: PathBuf) {
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let description =
            format!("thread {} {info}", thread.name().unwrap_or("<unnamed>")).replace(['\r', '\n'], " ");
        let line = format_entry(&local_now(), &panic_entry(&description));
        LogSink::open(&data_dir).append(&line);
        std::process::abort();
    }));
}

/// The Session_File store and the mailbox its completions go to. Opened
/// before the runtime starts so that settings and the Session_File can be
/// loaded first (task 14.7: "load settings and Session_File â†’ start runtime").
pub struct Storage {
    inbox: Arc<Mailbox>,
    files: DurableFileStore,
}

impl Storage {
    /// Opens the store in `data_dir` (creating it with its protected DACL
    /// and deleting leftover temp files). Write completions are queued in
    /// the runtime's mailbox. Deferred tabs get their placeholders in
    /// `data_dir\deferred` ([`PLACEHOLDER_SUBDIR`], Req 5.13).
    pub fn open(data_dir: impl Into<PathBuf>) -> Self {
        let inbox = Arc::new(Mailbox::new());
        let sink = inbox.clone();
        let files = DurableFileStore::open(
            data_dir,
            Box::new(move |seq, result| sink.post(Event::WriteCompleted { seq, result })),
        );
        esr_win::placeholder::set_dir(Some(files.dir().join(PLACEHOLDER_SUBDIR)));
        Self { inbox, files }
    }

    pub fn data_dir(&self) -> &Path {
        self.files.dir()
    }

    /// The settings, with defaults for anything missing or unreadable.
    pub fn settings(&self) -> Settings {
        match self.files.read_settings() {
            Ok(Some(text)) => Settings::parse_or_default(&text),
            Ok(None) | Err(_) => Settings::default(),
        }
    }

    pub fn read_session(&self) -> SessionFileRead {
        self.files.read_session()
    }
}

/// The `StartupInput` for a start now. With `background` (started by the
/// logon task, which has no delay) the start time stands in for the sign-in
/// time of the BuiltinRestore heuristic; the shell counts as ready now if
/// it already has a shell window.
pub fn startup_input(session_file: SessionFileRead, background: bool) -> StartupInput {
    let t = now();
    // SAFETY: plain read.
    let shell_ready = !unsafe { GetShellWindow() }.is_invalid();
    StartupInput {
        session_file,
        environment: read_environment(t),
        background_logon: background,
        logon_at: background.then_some(t),
        shell_ready_at: shell_ready.then_some(t),
    }
}

/// Runtime configuration.
#[derive(Clone, Debug)]
pub struct RuntimeOptions {
    /// Executable registered for start at sign-in (`None`: this executable).
    pub exe_path: Option<PathBuf>,
    /// Base name of the start-at-sign-in registration that a Settings change
    /// updates (`None`: the production `ExplorerSessionRecall`). Only set by
    /// tests ([`crate::dev_overrides`]).
    pub startup_name: Option<String>,
    pub strategies: Strategies,
    /// Start the UI thread (Settings, prompt, messages).
    pub ui: bool,
    /// Install the top-level panic guard (logs, then aborts). Tests that run
    /// the runtime in-process turn it off so a failing assertion stays a
    /// test failure.
    pub panic_guard: bool,
    /// Attach each new tab's event sink after its navigation, overlapping
    /// the window's next new-tab command ([`esr_win::browser_worker::late_sink`]).
    /// Opt-in, off by default (the user's decision in task 16.3 round 4d):
    /// it showed no measurable gain in 54 runs and keeps a small risk of an
    /// explorer.exe crash, which a restoration survives as a failed attempt
    /// (design.md "Timing budget"). [`LATE_SINK_ENV`]`=1` in the environment
    /// turns it on.
    pub late_sink: bool,
}

/// Environment variable read by [`RuntimeOptions::default`]: `1` turns
/// [`RuntimeOptions::late_sink`] on.
pub const LATE_SINK_ENV: &str = "ESR_LATE_SINK";

impl Default for RuntimeOptions {
    fn default() -> Self {
        let late_sink = std::env::var(LATE_SINK_ENV).is_ok_and(|v| v.trim() == "1");
        Self {
            exe_path: None,
            startup_name: None,
            strategies: Strategies::DEFAULT,
            ui: true,
            panic_guard: true,
            late_sink,
        }
    }
}

/// Runs Explorer Session Recall on the calling thread (which becomes the
/// Engine STA thread) until the Engine exits. Returns the process exit code.
pub fn run(
    settings: Settings,
    startup: StartupInput,
    storage: Storage,
    options: RuntimeOptions,
) -> i32 {
    let data_dir = storage.data_dir().to_path_buf();
    if options.panic_guard {
        install_panic_guard(data_dir.clone());
    }
    init_process();
    // Read when the navigator starts its browser workers.
    esr_win::browser_worker::set_late_sink(options.late_sink);
    let log_thread = LogThread::spawn(data_dir.clone());
    let mut early_log = log_thread.writer();
    let log_setup_error = |log: &mut LogThreadWriter, operation: &str, description: String| {
        let entry = LogEntry::Error { operation: operation.into(), description };
        crate::ports::LogWriter::append(log, &format_entry(&local_now(), &entry));
    };

    let _sta = match StaGuard::init() {
        Ok(g) => g,
        Err(e) => {
            log_setup_error(&mut early_log, "runtime-start", format!("COM STA: {}", e.message()));
            return EXIT_SETUP_FAILED;
        }
    };
    let hwnd = match win::create_engine_window() {
        Ok(h) => h,
        Err(e) => {
            log_setup_error(&mut early_log, "runtime-start", e.message);
            return EXIT_SETUP_FAILED;
        }
    };
    let messages = InstanceMessages::register();
    let taskbar_created = win::register_messages(hwnd, &messages);

    let Storage { inbox, files } = storage;
    if let Some(kind) = files.setup_error() {
        log_setup_error(&mut early_log, "data-directory", format!("{kind:?}"));
    }
    let sink: SharedSink = inbox.clone();

    // Shared key registries, so every adapter agrees on WinKey/TabKey.
    let windows = esr_win::keys::WindowKeys::new();
    let tabs = esr_win::keys::TabKeys::new();

    // UIA worker (tab strip).
    let strip = {
        let sink = sink.clone();
        match TabStripReader::start(Arc::new(move |win| sink.window_changed(win))) {
            Ok(s) => Some(Arc::new(s)),
            Err(e) => {
                log_setup_error(&mut early_log, "uia-worker", e.message);
                None
            }
        }
    };

    // Navigator (Engine thread, its own UIA worker).
    let mut navigator = {
        let inbox = inbox.clone();
        ShellNavigator::new(
            options.strategies,
            windows.clone(),
            tabs.clone(),
            Rc::new(move |signal| inbox.push(Inbound::Navigator(signal))),
        )
    };
    if let Err(e) = navigator.start() {
        log_setup_error(&mut early_log, "navigator", e.message);
    }

    // ShellWindows source and the window scanner.
    let building: win::Building = Rc::default();
    let source = Rc::new(RefCell::new(ShellWindowsSource::new(
        windows.clone(),
        tabs.clone(),
        Rc::new(SourceHandler { inbox: inbox.clone(), navigator: navigator.handle(), building: building.clone() }),
    )));
    let scanner = {
        let sink = sink.clone();
        let handler: Rc<dyn ScanHandler> = Rc::new(move |event: Event| sink.post(event));
        let strip_source = strip
            .clone()
            .map(|s| s as Arc<dyn esr_win::scanner::StripSource>);
        match WindowScanner::new(
            DebounceConfig::default(),
            windows.clone(),
            source.clone(),
            strip_source,
            handler,
        ) {
            Ok(s) => Rc::new(s),
            Err(e) => {
                log_setup_error(&mut early_log, "runtime-start", e.message);
                win::destroy_window(hwnd);
                return EXIT_SETUP_FAILED;
            }
        }
    };

    // WinEvent hooks and process-exit waits.
    let events = {
        let inbox = inbox.clone();
        Rc::new(RefCell::new(WindowEventSource::new(
            WindowEventConfig::default(),
            windows.clone(),
            Rc::new(move |signal: WindowSignal| inbox.push(Inbound::Window(signal))),
        )))
    };

    // Tab folding (Req 14). The first signals and `TabDetached` reach the
    // mailbox through the source's event sink, and the scanner hands the
    // source its tab-window map so a destroyed tab window can be named.
    {
        let sink = sink.clone();
        events.borrow().set_event_sink(Rc::new(move |event: Event| sink.post(event)));
        scanner.set_tab_windows(events.borrow().tab_windows());
    }

    // The hider reveals any candidate a crashed run left at alpha 0, before
    // the Engine starts (Req 14.14, 14.16); `HideFailed` goes to the mailbox.
    let mut hider: Box<dyn crate::ports::Hider> = {
        let sink = sink.clone();
        match CandidateHider::start(windows.clone(), Arc::new(move |event: Event| sink.post(event))) {
            Ok(hider) => Box::new(hider),
            Err(e) => {
                log_setup_error(&mut early_log, "candidate-hider", e.message.clone());
                Box::new(win::NoHider(e))
            }
        }
    };
    hider.sweep();

    // Fold-context probes (probe pool); the answer is `Event::FoldContext`.
    let fold_probe = {
        let sink = sink.clone();
        FoldContextProbe::new(windows.clone(), Arc::new(move |event: Event| sink.post(event)))
    };

    // Probe pool.
    let prober = {
        let sink = sink.clone();
        LocationProber::new(
            DEFAULT_THREADS,
            DEFAULT_DEADLINE,
            Box::new(move |probe, outcome| sink.post(Event::ProbeCompleted { probe, outcome })),
        )
    };

    // UI thread.
    let ui = if options.ui {
        match UiThread::spawn(sink.clone()) {
            Ok(ui) => Some(ui),
            Err(e) => {
                log_setup_error(&mut early_log, "ui-thread", e.message().to_string());
                None
            }
        }
    } else {
        None
    };

    let tab_strip: Box<dyn crate::ports::TabStrip> = match strip.clone() {
        Some(reader) => Box::new(win::UiaTabStrip { reader, sink: sink.clone() }),
        None => Box::new(NoTabStrip),
    };
    let ports = Ports {
        navigator: Box::new(navigator),
        tab_strip,
        prober: Box::new(prober),
        hider,
        fold_probe: Box::new(fold_probe),
        files: Box::new(files),
        log: Box::new(log_thread.writer()),
        options: Box::new(esr_win::explorer_options::ExplorerOptions::new()),
        notifier: Box::new(ToastNotifier::new()),
        startup: win::startup_port(options.exe_path.clone(), options.startup_name.as_deref()),
        host: Box::new(WinHost {
            scanner: scanner.clone(),
            source: source.clone(),
            events: events.clone(),
            ui: ui.as_ref().map(UiThread::handle),
            building,
        }),
    };
    let core = RuntimeCore::new(settings, startup, ports, now());
    let live = win::Live::new(
        core,
        inbox.clone(),
        hwnd,
        messages,
        taskbar_created,
        SessionEndListener::new(options.strategies.end_kind),
        source.clone(),
    );
    live.install();
    inbox.set_waker(win::waker(hwnd));

    // Attach to the shell and install the hooks; both report into the
    // mailbox (the source signals every open window for a scan).
    if let Err(e) = events.borrow_mut().start() {
        log_setup_error(&mut early_log, "window-events", e.message);
    }
    if let Err(e) = source.borrow_mut().attach() {
        // The source keeps polling for a shell and posts ShellAvailable.
        log_setup_error(&mut early_log, "shell-windows", e.message);
    }
    live.pump();

    win::message_loop();

    // Teardown: stop sources first, then the navigator (which reveals any
    // window it still hides), the pools, and the UI.
    win::Live::uninstall();
    events.borrow_mut().stop();
    source.borrow_mut().detach();
    drop(scanner);
    if let Ok(core) = Rc::try_unwrap(live).map(|l| l.core.into_inner()) {
        let mut core = core;
        let _ = core.ports_mut().files.flush(2_000);
        drop(core);
    }
    drop(ui);
    drop(early_log);
    drop(log_thread);
    win::destroy_window(hwnd);
    EXIT_OK
}

/// `ActivateTab` when no UIA worker could be started.
struct NoTabStrip;

impl crate::ports::TabStrip for NoTabStrip {
    fn read(
        &mut self,
        _: esr_core::engine::WinKey,
    ) -> Result<esr_core::engine::TabStripState, esr_core::engine::ShellError> {
        Err(no_uia())
    }
    fn activate(
        &mut self,
        _: esr_core::engine::OpId,
        _: esr_core::engine::WinKey,
        _: usize,
    ) -> Result<(), esr_core::engine::ShellError> {
        Err(no_uia())
    }
    fn watch(&mut self, _: esr_core::engine::WinKey) -> Result<(), esr_core::engine::ShellError> {
        Err(no_uia())
    }
    fn unwatch(&mut self, _: esr_core::engine::WinKey) {}
}

fn no_uia() -> esr_core::engine::ShellError {
    esr_core::engine::ShellError { code: 0, message: "UI Automation worker unavailable".into() }
}
