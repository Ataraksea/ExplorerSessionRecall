//! UI thread: Settings dialog, Builtin_Restore_Option prompt, and messages
//! (Requirements 10.1, 10.3, 10.5, 10.6, 10.7, 12.1–12.5, 12.7).
//!
//! # Threads
//!
//! [`UiThread::spawn`] starts a dedicated STA thread (`esr-ui`) with its own
//! message loop and a message-only window (`ESR.UI`). The Engine thread talks
//! to it only through a [`UiHandle`]: [`UiHandle::send`] queues a
//! [`UiCommand`] on a channel and posts `WM_UI_COMMAND` to wake the loop.
//! Results go back as `esr_core::engine::Event`s through the
//! [`SharedSink`] given at spawn:
//! `SettingsChanged`, `ExitRequested`, and `PromptAnswered`.
//!
//! * The modeless **Settings** dialog lives on the UI thread; the loop routes
//!   its messages through `IsDialogMessageW` (keyboard navigation).
//! * The **prompt** and **messages** are modal (`TaskDialogIndirect`), so each
//!   runs on its own short-lived STA thread (`esr-ui-modal`) that reports its
//!   result back to the UI thread with `WM_MODAL_DONE`. Neither the Engine
//!   (Req 10.7) nor the Settings dialog is ever blocked by a displayed prompt,
//!   and no modal loop runs nested inside the UI thread's loop.
//!
//! Which command leads to which window operation is decided by the pure
//! [`UiController`]; the Windows side only carries out [`UiAction`]s.
//!
//! The UI thread and modal threads set per-monitor-v2 DPI awareness for
//! themselves, so they behave the same whether or not the process already set
//! it (esr.exe declares it in its manifest).

pub mod message;
pub mod prompt;
pub mod settings;

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::thread::{self, JoinHandle};

use esr_core::engine::{Event, MessageKind, PromptAnswer};
use esr_core::settings::Settings;
use windows::Win32::Foundation::{E_FAIL, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX, InitCommonControlsEx,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, HWND_MESSAGE,
    IsDialogMessageW, MSG, PostMessageW, PostQuitMessage, RegisterClassW, TranslateMessage,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CLOSE, WM_DESTROY, WNDCLASSW,
};
use windows::core::{HSTRING, PCWSTR};

use crate::ports::SharedSink;
use prompt::ModalList;

/// Class name of the UI thread's message-only window.
pub const UI_WINDOW_CLASS: &str = "ESR.UI";
/// Wakes the UI thread to drain the command channel.
const WM_UI_COMMAND: u32 = WM_APP + 0x10;
/// A modal thread finished; `wParam`/`lParam` encode a [`ModalResult`].
const WM_MODAL_DONE: u32 = WM_APP + 0x11;

// ---------------------------------------------------------------------------
// Pure part
// ---------------------------------------------------------------------------

/// Requests from the Engine thread to the UI thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiCommand {
    /// Open Settings showing these values, or bring the open dialog to the
    /// foreground (Req 12.7). Sent for `Command::ShowSettings` with the
    /// Engine's current Settings.
    ShowSettings(Settings),
    /// Settings changed somewhere other than the dialog; refresh an open
    /// dialog (ignored when none is open).
    SettingsUpdated(Settings),
    /// Show the Builtin_Restore_Option prompt (`Command::ShowPrompt`). Ignored
    /// while a prompt is already displayed (Req 10.1: exactly one prompt).
    ShowPrompt,
    /// Show a message (`Command::ShowMessage`). Ignored while the same
    /// message is already displayed.
    ShowMessage(MessageKind),
    /// Close every UI window and end the UI thread.
    Shutdown,
}

/// A window operation decided by [`UiController`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiAction {
    OpenSettings(Settings),
    FocusSettings,
    RefreshSettings(Settings),
    /// The prompt was declined while Settings is open: record it there too,
    /// so a later toggle does not post a stale `builtin_prompt_declined`.
    MarkPromptDeclined,
    StartPrompt,
    StartMessage(MessageKind),
    Post(Event),
    /// Close everything and quit the UI thread.
    Stop,
}

/// A finished modal window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModalResult {
    Prompt(PromptAnswer),
    Message(MessageKind),
}

/// Encodes a [`ModalResult`] as `(wParam, lParam)` of `WM_MODAL_DONE`.
pub fn encode_modal(result: ModalResult) -> (usize, isize) {
    match result {
        ModalResult::Prompt(a) => (
            0,
            match a {
                PromptAnswer::Accepted => 0,
                PromptAnswer::Declined => 1,
                PromptAnswer::Dismissed => 2,
            },
        ),
        ModalResult::Message(MessageKind::BuiltinRestoreDisableFailed) => (1, 0),
    }
}

/// Inverse of [`encode_modal`].
pub fn decode_modal(wparam: usize, lparam: isize) -> Option<ModalResult> {
    match (wparam, lparam) {
        (0, 0) => Some(ModalResult::Prompt(PromptAnswer::Accepted)),
        (0, 1) => Some(ModalResult::Prompt(PromptAnswer::Declined)),
        (0, 2) => Some(ModalResult::Prompt(PromptAnswer::Dismissed)),
        (1, 0) => Some(ModalResult::Message(
            MessageKind::BuiltinRestoreDisableFailed,
        )),
        _ => None,
    }
}

/// Decides what the UI thread does for each command and completion.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UiController {
    settings_open: bool,
    prompt_open: bool,
    messages_open: Vec<MessageKind>,
    stopped: bool,
}

impl UiController {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn settings_open(&self) -> bool {
        self.settings_open
    }

    pub fn prompt_open(&self) -> bool {
        self.prompt_open
    }

    /// A command from the Engine. Nothing happens after `Shutdown`.
    pub fn command(&mut self, cmd: UiCommand) -> Vec<UiAction> {
        if self.stopped {
            return Vec::new();
        }
        match cmd {
            UiCommand::ShowSettings(s) => {
                if self.settings_open {
                    vec![UiAction::FocusSettings]
                } else {
                    self.settings_open = true;
                    vec![UiAction::OpenSettings(s)]
                }
            }
            UiCommand::SettingsUpdated(s) => {
                if self.settings_open {
                    vec![UiAction::RefreshSettings(s)]
                } else {
                    Vec::new()
                }
            }
            UiCommand::ShowPrompt => {
                if self.prompt_open {
                    Vec::new()
                } else {
                    self.prompt_open = true;
                    vec![UiAction::StartPrompt]
                }
            }
            UiCommand::ShowMessage(kind) => {
                if self.messages_open.contains(&kind) {
                    Vec::new()
                } else {
                    self.messages_open.push(kind);
                    vec![UiAction::StartMessage(kind)]
                }
            }
            UiCommand::Shutdown => {
                self.stopped = true;
                vec![UiAction::Stop]
            }
        }
    }

    /// The Settings dialog was destroyed (Close, Exit, X) or failed to open.
    pub fn settings_closed(&mut self) {
        self.settings_open = false;
    }

    /// A modal window finished. The prompt answer is posted to the Engine
    /// (unless the UI is stopping).
    pub fn modal_finished(&mut self, result: ModalResult) -> Vec<UiAction> {
        match result {
            ModalResult::Prompt(answer) => {
                self.prompt_open = false;
                if self.stopped {
                    return Vec::new();
                }
                let mut actions = vec![UiAction::Post(Event::PromptAnswered(answer))];
                if answer == PromptAnswer::Declined && self.settings_open {
                    actions.push(UiAction::MarkPromptDeclined);
                }
                actions
            }
            ModalResult::Message(kind) => {
                self.messages_open.retain(|&k| k != kind);
                Vec::new()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Handle and thread
// ---------------------------------------------------------------------------

/// Cheap, cloneable sender to the UI thread (usable from any thread).
#[derive(Clone, Debug)]
pub struct UiHandle {
    tx: Sender<UiCommand>,
    /// The UI thread's message-only window.
    hwnd: usize,
}

impl UiHandle {
    /// Queues `cmd` and wakes the UI thread. Returns `false` once the UI
    /// thread has ended.
    pub fn send(&self, cmd: UiCommand) -> bool {
        if self.tx.send(cmd).is_err() {
            return false;
        }
        // SAFETY: posting to a window of another thread; fails harmlessly
        // once it is destroyed.
        unsafe {
            PostMessageW(
                Some(HWND(self.hwnd as *mut _)),
                WM_UI_COMMAND,
                WPARAM(0),
                LPARAM(0),
            )
            .is_ok()
        }
    }
}

/// The running UI thread. Dropping it sends `Shutdown` and joins the thread.
#[derive(Debug)]
pub struct UiThread {
    handle: UiHandle,
    join: Option<JoinHandle<()>>,
}

impl UiThread {
    /// Starts the UI thread; returns once its message window exists.
    pub fn spawn(sink: SharedSink) -> windows::core::Result<UiThread> {
        let (tx, rx) = mpsc::channel::<UiCommand>();
        let (ready_tx, ready_rx) = mpsc::sync_channel::<windows::core::Result<usize>>(1);
        let join = thread::Builder::new()
            .name("esr-ui".into())
            .spawn(move || ui_thread_main(sink, rx, ready_tx))
            .map_err(|_| windows::core::Error::from(E_FAIL))?;
        match ready_rx.recv() {
            Ok(Ok(hwnd)) => Ok(UiThread {
                handle: UiHandle { tx, hwnd },
                join: Some(join),
            }),
            Ok(Err(e)) => {
                let _ = join.join();
                Err(e)
            }
            Err(_) => {
                let _ = join.join();
                Err(windows::core::Error::from(E_FAIL))
            }
        }
    }

    pub fn handle(&self) -> UiHandle {
        self.handle.clone()
    }

    /// Sends `Shutdown` and waits for the UI thread to end. Displayed modal
    /// windows are asked to close; their threads are not waited for.
    pub fn shutdown(mut self) {
        self.stop_and_join();
    }

    fn stop_and_join(&mut self) {
        if let Some(join) = self.join.take() {
            self.handle.send(UiCommand::Shutdown);
            let _ = join.join();
        }
    }
}

impl Drop for UiThread {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

/// State owned by the UI thread.
struct UiState {
    controller: UiController,
    sink: SharedSink,
    rx: Receiver<UiCommand>,
    hwnd: HWND,
    settings_hwnd: Option<HWND>,
    modals: Arc<ModalList>,
}

thread_local! {
    static STATE: RefCell<Option<UiState>> = const { RefCell::new(None) };
}

/// Runs `f` on the UI state. Never called while another borrow is active:
/// window calls are made only outside of `f`.
fn with_state<R>(f: impl FnOnce(&mut UiState) -> R) -> Option<R> {
    STATE.with(|cell| {
        cell.try_borrow_mut()
            .ok()
            .and_then(|mut s| s.as_mut().map(f))
    })
}

/// Per-monitor-v2 DPI awareness and an STA for the current thread.
/// Returns whether `CoUninitialize` must be called.
fn init_ui_thread() -> bool {
    // SAFETY: per-thread settings with no preconditions.
    unsafe {
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok()
    }
}

fn ui_thread_main(
    sink: SharedSink,
    rx: Receiver<UiCommand>,
    ready: SyncSender<windows::core::Result<usize>>,
) {
    let com = init_ui_thread();
    let icc = INITCOMMONCONTROLSEX {
        dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
        dwICC: ICC_STANDARD_CLASSES,
    };
    // SAFETY: valid INITCOMMONCONTROLSEX.
    unsafe {
        let _ = InitCommonControlsEx(&icc);
    }
    match create_message_window() {
        Ok(hwnd) => {
            STATE.with(|cell| {
                *cell.borrow_mut() = Some(UiState {
                    controller: UiController::new(),
                    sink,
                    rx,
                    hwnd,
                    settings_hwnd: None,
                    modals: Arc::new(ModalList::default()),
                })
            });
            let _ = ready.send(Ok(hwnd.0 as usize));
            run_message_loop();
            STATE.with(|cell| cell.borrow_mut().take());
        }
        Err(e) => {
            let _ = ready.send(Err(e));
        }
    }
    if com {
        // SAFETY: balances the successful CoInitializeEx above.
        unsafe { CoUninitialize() };
    }
}

fn create_message_window() -> windows::core::Result<HWND> {
    let class = HSTRING::from(UI_WINDOW_CLASS);
    // SAFETY: registering (idempotently) and creating a message-only window
    // owned by this thread.
    unsafe {
        let hinstance = GetModuleHandleW(None)?;
        let wc = WNDCLASSW {
            lpfnWndProc: Some(ui_window_proc),
            hInstance: hinstance.into(),
            lpszClassName: PCWSTR(class.as_ptr()),
            ..Default::default()
        };
        // Fails with ERROR_CLASS_ALREADY_EXISTS for later UI threads; the
        // existing registration is identical.
        RegisterClassW(&wc);
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            &class,
            &class,
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(hinstance.into()),
            None,
        )
    }
}

fn run_message_loop() {
    let mut msg = MSG::default();
    // SAFETY: standard message loop on the thread that owns the windows.
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            if let Some(Some(dialog)) = with_state(|s| s.settings_hwnd)
                && IsDialogMessageW(dialog, &msg).as_bool()
            {
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn ui_window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_UI_COMMAND => {
            while let Some(Some(cmd)) = with_state(|s| s.rx.try_recv().ok()) {
                let actions = with_state(|s| s.controller.command(cmd)).unwrap_or_default();
                run_actions(actions);
            }
            LRESULT(0)
        }
        WM_MODAL_DONE => {
            if let Some(result) = decode_modal(wparam.0, lparam.0) {
                let actions =
                    with_state(|s| s.controller.modal_finished(result)).unwrap_or_default();
                run_actions(actions);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: ends this thread's message loop.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // SAFETY: default handling for everything else.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Called by the Settings dialog from `WM_DESTROY`.
fn on_settings_destroyed() {
    with_state(|s| {
        s.settings_hwnd = None;
        s.controller.settings_closed();
    });
}

fn run_actions(actions: Vec<UiAction>) {
    for action in actions {
        run_action(action);
    }
}

fn run_action(action: UiAction) {
    let settings_hwnd = with_state(|s| s.settings_hwnd).flatten();
    match action {
        UiAction::OpenSettings(values) => {
            let Some(sink) = with_state(|s| s.sink.clone()) else {
                return;
            };
            match settings::open(values, sink, on_settings_destroyed) {
                Ok(dialog) => {
                    with_state(|s| s.settings_hwnd = Some(dialog));
                }
                Err(_) => {
                    with_state(|s| s.controller.settings_closed());
                }
            }
        }
        UiAction::FocusSettings => {
            if let Some(dialog) = settings_hwnd {
                settings::bring_to_front(dialog);
            }
        }
        UiAction::RefreshSettings(values) => {
            if let Some(dialog) = settings_hwnd {
                settings::refresh(dialog, values);
            }
        }
        UiAction::MarkPromptDeclined => {
            if let Some(dialog) = settings_hwnd {
                settings::mark_prompt_declined(dialog);
            }
        }
        UiAction::StartPrompt => {
            start_modal(
                |open| ModalResult::Prompt(prompt::show_prompt(open)),
                ModalResult::Prompt(PromptAnswer::Dismissed),
            );
        }
        UiAction::StartMessage(kind) => {
            start_modal(
                move |open| {
                    message::show_message(kind, open);
                    ModalResult::Message(kind)
                },
                ModalResult::Message(kind),
            );
        }
        UiAction::Post(event) => {
            if let Some(sink) = with_state(|s| s.sink.clone()) {
                sink.post(event);
            }
        }
        UiAction::Stop => {
            if let Some(dialog) = settings_hwnd {
                settings::close(dialog);
            }
            if let Some(modals) = with_state(|s| s.modals.clone()) {
                close_modals(&modals);
            }
            if let Some(hwnd) = with_state(|s| s.hwnd) {
                // SAFETY: destroying this thread's own window (WM_DESTROY quits the loop).
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
            }
        }
    }
}

/// Runs `show` on a new modal thread and reports its result to the UI
/// window. If the thread cannot start, the modal counts as finished at once
/// (a prompt then counts as dismissed, Req 10.6).
fn start_modal(
    show: impl FnOnce(&ModalList) -> ModalResult + Send + 'static,
    if_not_started: ModalResult,
) {
    let Some((hwnd, modals)) = with_state(|s| (s.hwnd.0 as usize, s.modals.clone())) else {
        return;
    };
    let spawned = thread::Builder::new()
        .name("esr-ui-modal".into())
        .spawn(move || {
            let com = init_ui_thread();
            let result = show(&modals);
            if com {
                // SAFETY: balances CoInitializeEx in init_ui_thread.
                unsafe { CoUninitialize() };
            }
            let (w, l) = encode_modal(result);
            // SAFETY: fails harmlessly once the UI window is gone.
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(hwnd as *mut _)),
                    WM_MODAL_DONE,
                    WPARAM(w),
                    LPARAM(l),
                );
            }
        });
    if spawned.is_err() {
        let actions =
            with_state(|s| s.controller.modal_finished(if_not_started)).unwrap_or_default();
        run_actions(actions);
    }
}

/// Asks every displayed task dialog to close (as if Esc was pressed).
fn close_modals(modals: &ModalList) {
    let open: Vec<usize> = modals.lock().map(|v| v.clone()).unwrap_or_default();
    for h in open {
        // SAFETY: posting to another thread's window; fails harmlessly if gone.
        unsafe {
            let _ = PostMessageW(Some(HWND(h as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Condvar, Mutex};
    use std::time::{Duration, Instant};

    use esr_core::engine::{Event, WinKey};

    use crate::ports::EventSink;

    /// Records posted events and lets a test wait for them.
    #[derive(Default)]
    pub struct Recorder {
        events: Mutex<Vec<Event>>,
        cv: Condvar,
    }

    impl Recorder {
        pub fn events(&self) -> Vec<Event> {
            self.events.lock().unwrap().clone()
        }

        /// Waits until `pred` holds for the recorded events or `timeout` passes.
        pub fn wait_for(&self, timeout: Duration, pred: impl Fn(&[Event]) -> bool) -> Vec<Event> {
            let deadline = Instant::now() + timeout;
            let mut events = self.events.lock().unwrap();
            while !pred(&events) {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                events = self.cv.wait_timeout(events, deadline - now).unwrap().0;
            }
            events.clone()
        }
    }

    impl EventSink for Recorder {
        fn post(&self, event: Event) {
            self.events.lock().unwrap().push(event);
            self.cv.notify_all();
        }
        fn window_changed(&self, _win: WinKey) {}
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::Recorder;
    use super::*;
    use std::time::{Duration, Instant};

    fn declined() -> Settings {
        Settings {
            builtin_prompt_declined: true,
            ..Settings::default()
        }
    }

    #[test]
    fn only_one_settings_dialog() {
        let mut c = UiController::new();
        let s = Settings::default();
        assert_eq!(
            c.command(UiCommand::ShowSettings(s.clone())),
            vec![UiAction::OpenSettings(s.clone())]
        );
        assert!(c.settings_open());
        // Req 12.7: a second request focuses the open dialog.
        assert_eq!(
            c.command(UiCommand::ShowSettings(declined())),
            vec![UiAction::FocusSettings]
        );
        c.settings_closed();
        assert_eq!(
            c.command(UiCommand::ShowSettings(declined())),
            vec![UiAction::OpenSettings(declined())]
        );
    }

    #[test]
    fn settings_updates_only_reach_an_open_dialog() {
        let mut c = UiController::new();
        assert!(c.command(UiCommand::SettingsUpdated(declined())).is_empty());
        c.command(UiCommand::ShowSettings(Settings::default()));
        assert_eq!(
            c.command(UiCommand::SettingsUpdated(declined())),
            vec![UiAction::RefreshSettings(declined())]
        );
    }

    #[test]
    fn one_prompt_at_a_time_and_answers_are_posted() {
        let mut c = UiController::new();
        assert_eq!(
            c.command(UiCommand::ShowPrompt),
            vec![UiAction::StartPrompt]
        );
        assert!(c.command(UiCommand::ShowPrompt).is_empty());
        for answer in [PromptAnswer::Accepted, PromptAnswer::Dismissed] {
            assert_eq!(
                c.modal_finished(ModalResult::Prompt(answer)),
                vec![UiAction::Post(Event::PromptAnswered(answer))]
            );
            assert!(!c.prompt_open());
            assert_eq!(
                c.command(UiCommand::ShowPrompt),
                vec![UiAction::StartPrompt]
            );
        }
    }

    #[test]
    fn declined_prompt_updates_an_open_settings_dialog() {
        let mut c = UiController::new();
        c.command(UiCommand::ShowPrompt);
        c.command(UiCommand::ShowSettings(Settings::default()));
        assert_eq!(
            c.modal_finished(ModalResult::Prompt(PromptAnswer::Declined)),
            vec![
                UiAction::Post(Event::PromptAnswered(PromptAnswer::Declined)),
                UiAction::MarkPromptDeclined
            ]
        );
        // Without an open dialog only the answer is posted.
        let mut c = UiController::new();
        c.command(UiCommand::ShowPrompt);
        assert_eq!(
            c.modal_finished(ModalResult::Prompt(PromptAnswer::Declined)),
            vec![UiAction::Post(Event::PromptAnswered(
                PromptAnswer::Declined
            ))]
        );
    }

    #[test]
    fn messages_are_not_duplicated_while_shown() {
        let kind = MessageKind::BuiltinRestoreDisableFailed;
        let mut c = UiController::new();
        assert_eq!(
            c.command(UiCommand::ShowMessage(kind)),
            vec![UiAction::StartMessage(kind)]
        );
        assert!(c.command(UiCommand::ShowMessage(kind)).is_empty());
        assert!(c.modal_finished(ModalResult::Message(kind)).is_empty());
        assert_eq!(
            c.command(UiCommand::ShowMessage(kind)),
            vec![UiAction::StartMessage(kind)]
        );
    }

    #[test]
    fn shutdown_stops_everything() {
        let mut c = UiController::new();
        c.command(UiCommand::ShowPrompt);
        assert_eq!(c.command(UiCommand::Shutdown), vec![UiAction::Stop]);
        assert!(
            c.command(UiCommand::ShowSettings(Settings::default()))
                .is_empty()
        );
        assert!(c.command(UiCommand::ShowPrompt).is_empty());
        assert!(c.command(UiCommand::Shutdown).is_empty());
        // A prompt closed by the shutdown posts no answer.
        assert!(
            c.modal_finished(ModalResult::Prompt(PromptAnswer::Dismissed))
                .is_empty()
        );
    }

    #[test]
    fn modal_results_round_trip() {
        let all = [
            ModalResult::Prompt(PromptAnswer::Accepted),
            ModalResult::Prompt(PromptAnswer::Declined),
            ModalResult::Prompt(PromptAnswer::Dismissed),
            ModalResult::Message(MessageKind::BuiltinRestoreDisableFailed),
        ];
        for r in all {
            let (w, l) = encode_modal(r);
            assert_eq!(decode_modal(w, l), Some(r));
        }
        assert_eq!(decode_modal(0, 3), None);
        assert_eq!(decode_modal(2, 0), None);
    }

    /// Starts and stops the real UI thread. Only the message-only window is
    /// created, so nothing appears on screen.
    #[test]
    fn ui_thread_starts_and_shuts_down() {
        let rec = Arc::new(Recorder::default());
        let ui = UiThread::spawn(rec.clone()).expect("UI thread");
        let handle = ui.handle();
        assert!(handle.send(UiCommand::SettingsUpdated(Settings::default())));
        let start = Instant::now();
        ui.shutdown();
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(
            !handle.send(UiCommand::ShowPrompt),
            "no UI thread after shutdown"
        );
        assert!(rec.events().is_empty());
    }

    /// Manual: opens Settings, then (after 3 s) asks again to check that the
    /// same window comes to the foreground. Toggle options and watch the
    /// printed events; close the dialog or press Exit to end (60 s maximum).
    #[test]
    #[ignore = "manual: shows the Settings dialog"]
    fn manual_settings_dialog() {
        let rec = Arc::new(Recorder::default());
        let ui = UiThread::spawn(rec.clone()).expect("UI thread");
        let handle = ui.handle();
        handle.send(UiCommand::ShowSettings(declined()));
        rec.wait_for(Duration::from_secs(3), |_| false);
        handle.send(UiCommand::ShowSettings(Settings::default()));
        let events = rec.wait_for(Duration::from_secs(60), |e| {
            e.contains(&Event::ExitRequested)
        });
        for e in &events {
            println!("{e:?}");
        }
        ui.shutdown();
    }

    /// Manual: the prompt through the UI thread; answer it and see the event.
    #[test]
    #[ignore = "manual: shows the prompt"]
    fn manual_prompt_via_ui_thread() {
        let rec = Arc::new(Recorder::default());
        let ui = UiThread::spawn(rec.clone()).expect("UI thread");
        ui.handle().send(UiCommand::ShowPrompt);
        ui.handle().send(UiCommand::ShowPrompt); // ignored: one prompt only
        let events = rec.wait_for(Duration::from_secs(120), |e| !e.is_empty());
        println!("{events:?}");
        ui.shutdown();
    }
}
