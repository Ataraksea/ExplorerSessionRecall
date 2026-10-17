//! Single instance: per-user named mutex and signalling the first instance
//! (Requirements 1.4, 12.7).
//!
//! * The first instance creates the named mutex
//!   `Local\ExplorerSessionRecall.<user SID>` and keeps the returned
//!   [`InstanceGuard`] alive for the whole process lifetime (bind it in `main`
//!   and never drop it early). The kernel closes the handle when the process
//!   ends, including on abort, so a crashed instance never blocks the next one.
//! * A second instance (the mutex already exists) does not open the
//!   Session_File and shows no window. It finds the first instance's hidden
//!   top-level window of class [`ENGINE_WINDOW_CLASS`] and sends one
//!   registered message:
//!   * started by the user (no `--background`): `AllowSetForegroundWindow`
//!     for the first instance, then [`MSG_SHOW_SETTINGS`];
//!   * started with `--background` (logon task): [`MSG_PING`].
//!
//!   Then it exits. Finding the window is retried briefly (the first instance
//!   may still be starting) and the send uses `SMTO_ABORTIFHUNG` with a short
//!   timeout, so the whole signal takes at most about
//!   [`FIND_WINDOW_BUDGET`] + [`SEND_TIMEOUT`] (1.5 s), within the 2 s of
//!   Req 1.4. Any failure is silent.
//! * `esr.exe --uninstall` (task 14.5) sends [`MSG_EXIT`] the same way to
//!   stop the running instance (Req 12.6); the runtime treats it like Exit in
//!   Settings (`Event::ExitRequested`), leaving every Explorer window open.
//! * The runtime (task 14.6) calls [`InstanceMessages::register`] and uses
//!   [`InstanceMessages::recognise`] in the `ESR.Engine` window procedure.
//!
//! The decision logic ([`SecondaryAction::from_args`], [`decide`]) is pure;
//! the Windows calls are thin wrappers around it.

use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HLOCAL, HWND,
    LPARAM, LocalFree, WPARAM,
};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentProcess, OpenProcessToken};
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, FindWindowW, GetWindowThreadProcessId, RegisterWindowMessageW,
    SMTO_ABORTIFHUNG, SendMessageTimeoutW,
};
use windows::core::{HSTRING, PCWSTR, PWSTR};

/// Prefix of the per-user mutex name; the user's SID string follows.
pub const MUTEX_PREFIX: &str = r"Local\ExplorerSessionRecall.";
/// Window class of the first instance's hidden top-level Engine window.
pub const ENGINE_WINDOW_CLASS: &str = "ESR.Engine";
/// Registered message name: "open Settings or bring it to the foreground".
pub const MSG_SHOW_SETTINGS: &str = "ESR_SHOW_SETTINGS";
/// Registered message name: "another instance was started in the background".
pub const MSG_PING: &str = "ESR_PING";
/// Registered message name: "exit now" (sent by `--uninstall`, Req 12.6).
pub const MSG_EXIT: &str = "ESR_EXIT";
/// Command-line flag passed by the logon task.
pub const BACKGROUND_FLAG: &str = "--background";

/// How long a second instance keeps looking for the Engine window.
pub const FIND_WINDOW_BUDGET: Duration = Duration::from_millis(1000);
/// Interval between `FindWindowW` attempts.
pub const FIND_WINDOW_POLL: Duration = Duration::from_millis(50);
/// `SendMessageTimeoutW` timeout (with `SMTO_ABORTIFHUNG`).
pub const SEND_TIMEOUT: Duration = Duration::from_millis(500);

/// What a second instance asks the first instance to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecondaryAction {
    /// Started by the user: show Settings (Req 12.7).
    ShowSettings,
    /// Started with `--background`: only notify.
    Ping,
    /// `--uninstall`: stop the running instance (Req 12.6). Never chosen by
    /// [`from_args`](Self::from_args); sent by `crate::install`.
    Exit,
}

impl SecondaryAction {
    /// Chooses the action from the command line. Including the program name
    /// in `args` is harmless. The flag is matched exactly, as the logon task
    /// passes it.
    pub fn from_args<I, S>(args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        if args.into_iter().any(|a| a.as_ref() == BACKGROUND_FLAG) {
            Self::Ping
        } else {
            Self::ShowSettings
        }
    }

    /// Name of the registered window message for this action.
    pub fn message_name(self) -> &'static str {
        match self {
            Self::ShowSettings => MSG_SHOW_SETTINGS,
            Self::Ping => MSG_PING,
            Self::Exit => MSG_EXIT,
        }
    }
}

/// Result of trying to create the named mutex.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutexState {
    /// This process created the mutex.
    Created,
    /// The mutex already exists (another instance is running).
    AlreadyExists,
}

/// Pure decision: the role to take, without the guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Primary,
    Secondary { action: SecondaryAction },
}

/// Pure decision from the mutex state and the command line.
pub fn decide<I, S>(state: MutexState, args: I) -> Decision
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    match state {
        MutexState::Created => Decision::Primary,
        MutexState::AlreadyExists => Decision::Secondary {
            action: SecondaryAction::from_args(args),
        },
    }
}

/// The per-user mutex name for `sid`.
pub fn mutex_name(sid: &str) -> String {
    format!("{MUTEX_PREFIX}{sid}")
}

/// Mutex prefix of the old "Explorer Session Restore" builds (v0.1). Only
/// `--install` uses it, to stop a running old instance before migrating.
pub const LEGACY_MUTEX_PREFIX: &str = r"Local\ExplorerSessionRestore.";

/// The old (v0.1) per-user mutex name for `sid`.
pub fn legacy_mutex_name(sid: &str) -> String {
    format!("{LEGACY_MUTEX_PREFIX}{sid}")
}

/// Handle to the single-instance mutex. Holding it marks this process as the
/// first instance; it must live for the whole process lifetime. Dropping it
/// closes the handle (the mutex disappears once no process holds it).
#[derive(Debug)]
pub struct InstanceGuard {
    handle: HANDLE,
}

// SAFETY: a kernel handle is process-wide and may be closed from any thread.
unsafe impl Send for InstanceGuard {}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        // SAFETY: `handle` came from CreateMutexW and is closed exactly once.
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// This process's role.
#[derive(Debug)]
pub enum InstanceRole {
    /// First instance; keep the guard alive until the process exits.
    Primary(InstanceGuard),
    /// Another instance is running; signal it with `action` and exit.
    Secondary { action: SecondaryAction },
}

/// Acquires the per-user mutex `Local\ExplorerSessionRecall.<SID>`.
pub fn acquire<I, S>(args: I) -> windows::core::Result<InstanceRole>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let sid = current_user_sid()?;
    acquire_named(&mutex_name(&sid), args)
}

/// Acquires the mutex `name` (injectable for tests).
///
/// `ERROR_ACCESS_DENIED` is treated as "already exists": the same user's
/// mutex created by an instance at a higher integrity level cannot be opened
/// but still means an instance is running.
pub fn acquire_named<I, S>(name: &str, args: I) -> windows::core::Result<InstanceRole>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let wide = HSTRING::from(name);
    // SAFETY: plain Win32 call with a valid null-terminated name. The mutex
    // is not initially owned: only its existence matters.
    let created = unsafe { CreateMutexW(None, false, &wide) };
    let state = match created {
        Ok(handle) => {
            // Must be read right after CreateMutexW.
            // SAFETY: no preconditions.
            if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                // SAFETY: we own this handle and close it once.
                unsafe {
                    let _ = CloseHandle(handle);
                }
                MutexState::AlreadyExists
            } else {
                return Ok(InstanceRole::Primary(InstanceGuard { handle }));
            }
        }
        Err(e) if e.code() == ERROR_ACCESS_DENIED.to_hresult() => MutexState::AlreadyExists,
        Err(e) => return Err(e),
    };
    match decide(state, args) {
        Decision::Secondary { action } => Ok(InstanceRole::Secondary { action }),
        // `decide` maps AlreadyExists to Secondary.
        Decision::Primary => unreachable!("AlreadyExists always yields Secondary"),
    }
}

/// The current user's SID as a string (`S-1-5-21-…`).
pub fn current_user_sid() -> windows::core::Result<String> {
    // SAFETY: standard token query; buffers are sized from the first call and
    // the token handle and the SID string are released before returning.
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)?;
        let result = (|| {
            let mut len = 0u32;
            // Expected to fail with ERROR_INSUFFICIENT_BUFFER, filling `len`.
            let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
            // u64 elements keep the buffer aligned for TOKEN_USER.
            let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
            GetTokenInformation(
                token,
                TokenUser,
                Some(buf.as_mut_ptr().cast()),
                (buf.len() * 8) as u32,
                &mut len,
            )?;
            let user = &*(buf.as_ptr() as *const TOKEN_USER);
            let mut sid_str = PWSTR::null();
            ConvertSidToStringSidW(user.User.Sid, &mut sid_str)?;
            let s = sid_str.to_string();
            let _ = LocalFree(Some(HLOCAL(sid_str.0.cast())));
            s.map_err(|_| windows::core::Error::from(windows::Win32::Foundation::E_UNEXPECTED))
        })();
        let _ = CloseHandle(token);
        result
    }
}

/// Registered message ids, for the first instance's Engine window procedure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstanceMessages {
    pub show_settings: u32,
    pub ping: u32,
    pub exit: u32,
}

impl InstanceMessages {
    /// Registers (or looks up) the messages. An id of 0 means registration
    /// failed; such an id never matches in [`recognise`](Self::recognise).
    pub fn register() -> Self {
        Self {
            show_settings: register_message(MSG_SHOW_SETTINGS),
            ping: register_message(MSG_PING),
            exit: register_message(MSG_EXIT),
        }
    }

    /// Maps a window message to the single-instance request it carries.
    pub fn recognise(&self, msg: u32) -> Option<SecondaryAction> {
        if msg == 0 {
            None
        } else if msg == self.show_settings {
            Some(SecondaryAction::ShowSettings)
        } else if msg == self.ping {
            Some(SecondaryAction::Ping)
        } else if msg == self.exit {
            Some(SecondaryAction::Exit)
        } else {
            None
        }
    }
}

fn register_message(name: &str) -> u32 {
    // SAFETY: valid null-terminated string.
    unsafe { RegisterWindowMessageW(&HSTRING::from(name)) }
}

/// Outcome of signalling the first instance (informational; the second
/// instance exits either way).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalOutcome {
    /// The message was delivered to the window of process `pid`.
    Delivered { pid: u32 },
    /// No Engine window appeared within the budget.
    WindowNotFound,
    /// The window was found but the send failed or timed out.
    SendFailed,
}

/// Signals the running first instance with `action` (see module docs).
pub fn signal_primary(action: SecondaryAction) -> SignalOutcome {
    signal_window_class(ENGINE_WINDOW_CLASS, action, FIND_WINDOW_BUDGET)
}

/// Signals the top-level window of class `class` (injectable for tests).
/// `FindWindowW` is retried every [`FIND_WINDOW_POLL`] until `find_budget`
/// elapses; the send then waits at most [`SEND_TIMEOUT`].
pub fn signal_window_class(
    class: &str,
    action: SecondaryAction,
    find_budget: Duration,
) -> SignalOutcome {
    let Some(hwnd) = find_window_with_retry(class, find_budget) else {
        return SignalOutcome::WindowNotFound;
    };
    signal_window(hwnd, action)
}

/// Signals the already found Engine window `hwnd` with `action`
/// (`AllowSetForegroundWindow` for Settings, then a `SendMessageTimeoutW`
/// of at most [`SEND_TIMEOUT`]). Lets tests target one specific window,
/// such as the Engine window of their own process, instead of the first
/// window of the class.
pub fn signal_window(hwnd: HWND, action: SecondaryAction) -> SignalOutcome {
    let mut pid = 0u32;
    // SAFETY: `hwnd` may have been destroyed meanwhile; the calls then fail
    // harmlessly.
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if action == SecondaryAction::ShowSettings && pid != 0 {
            // Lets the first instance take the foreground for Settings.
            let _ = AllowSetForegroundWindow(pid);
        }
    }
    let msg = register_message(action.message_name());
    if msg == 0 {
        return SignalOutcome::SendFailed;
    }
    let mut result = 0usize;
    // SAFETY: plain send with no pointers in wParam/lParam.
    let ok = unsafe {
        SendMessageTimeoutW(
            hwnd,
            msg,
            WPARAM(0),
            LPARAM(0),
            SMTO_ABORTIFHUNG,
            SEND_TIMEOUT.as_millis() as u32,
            Some(&mut result),
        )
    };
    if ok.0 != 0 {
        SignalOutcome::Delivered { pid }
    } else {
        SignalOutcome::SendFailed
    }
}

fn find_window_with_retry(class: &str, budget: Duration) -> Option<HWND> {
    let wide = HSTRING::from(class);
    let start = Instant::now();
    loop {
        // SAFETY: valid class string; null window name matches any title.
        if let Ok(hwnd) = unsafe { FindWindowW(&wide, PCWSTR::null()) }
            && !hwnd.is_invalid()
        {
            return Some(hwnd);
        }
        let elapsed = start.elapsed();
        if elapsed >= budget {
            return None;
        }
        thread::sleep(FIND_WINDOW_POLL.min(budget - elapsed));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_start_shows_settings() {
        assert_eq!(
            SecondaryAction::from_args(["esr.exe"]),
            SecondaryAction::ShowSettings
        );
        assert_eq!(
            SecondaryAction::from_args(Vec::<String>::new()),
            SecondaryAction::ShowSettings
        );
        assert_eq!(
            SecondaryAction::from_args(["esr.exe", "--install"]),
            SecondaryAction::ShowSettings
        );
    }

    #[test]
    fn background_flag_pings() {
        assert_eq!(
            SecondaryAction::from_args(["esr.exe", "--background"]),
            SecondaryAction::Ping
        );
        assert_eq!(
            SecondaryAction::from_args(["--background".to_string()]),
            SecondaryAction::Ping
        );
        // Only the exact flag counts.
        assert_eq!(
            SecondaryAction::from_args(["esr.exe", "--backgroundx", "background"]),
            SecondaryAction::ShowSettings
        );
    }

    #[test]
    fn decide_maps_mutex_state() {
        assert_eq!(
            decide(MutexState::Created, ["--background"]),
            Decision::Primary
        );
        assert_eq!(
            decide(MutexState::AlreadyExists, ["esr.exe"]),
            Decision::Secondary {
                action: SecondaryAction::ShowSettings
            }
        );
        assert_eq!(
            decide(MutexState::AlreadyExists, ["esr.exe", "--background"]),
            Decision::Secondary {
                action: SecondaryAction::Ping
            }
        );
    }

    #[test]
    fn message_names_and_mutex_name() {
        assert_eq!(SecondaryAction::ShowSettings.message_name(), "ESR_SHOW_SETTINGS");
        assert_eq!(SecondaryAction::Ping.message_name(), "ESR_PING");
        assert_eq!(SecondaryAction::Exit.message_name(), "ESR_EXIT");
        assert_eq!(
            mutex_name("S-1-5-21-1-2-3-1001"),
            r"Local\ExplorerSessionRecall.S-1-5-21-1-2-3-1001"
        );
        assert_eq!(
            legacy_mutex_name("S-1-5-21-1-2-3-1001"),
            r"Local\ExplorerSessionRestore.S-1-5-21-1-2-3-1001"
        );
    }

    #[test]
    fn current_user_sid_is_a_sid_string() {
        let sid = current_user_sid().expect("SID must be readable");
        assert!(sid.starts_with("S-1-"), "{sid}");
    }

    fn unique(prefix: &str) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{prefix}.{}.{nanos}", std::process::id())
    }

    #[test]
    fn second_acquire_sees_existing_mutex_and_guard_lifetime() {
        let name = unique(r"Local\ESR.Test.Instance");

        let first = acquire_named(&name, ["esr.exe"]).unwrap();
        assert!(matches!(first, InstanceRole::Primary(_)));

        let second = acquire_named(&name, ["esr.exe"]).unwrap();
        assert!(matches!(
            second,
            InstanceRole::Secondary {
                action: SecondaryAction::ShowSettings
            }
        ));
        let third = acquire_named(&name, ["esr.exe", "--background"]).unwrap();
        assert!(matches!(
            third,
            InstanceRole::Secondary {
                action: SecondaryAction::Ping
            }
        ));

        // Releasing the guard frees the name (secondary handles were closed).
        drop(first);
        let again = acquire_named(&name, ["esr.exe"]).unwrap();
        assert!(matches!(again, InstanceRole::Primary(_)));
    }

    #[test]
    fn recognise_registered_messages() {
        let m = InstanceMessages::register();
        assert_ne!(m.show_settings, 0);
        assert_ne!(m.ping, 0);
        assert_ne!(m.exit, 0);
        assert_ne!(m.show_settings, m.ping);
        assert_ne!(m.exit, m.ping);
        assert_ne!(m.exit, m.show_settings);
        assert_eq!(m.recognise(m.show_settings), Some(SecondaryAction::ShowSettings));
        assert_eq!(m.recognise(m.ping), Some(SecondaryAction::Ping));
        assert_eq!(m.recognise(m.exit), Some(SecondaryAction::Exit));
        assert_eq!(m.recognise(0), None);
        assert_eq!(m.recognise(0x0010 /* WM_CLOSE */), None);
        // Registration is stable within the session.
        assert_eq!(InstanceMessages::register(), m);
    }

    #[test]
    fn missing_window_gives_up_within_budget() {
        let class = unique("ESR.Test.NoSuchWindow");
        let start = Instant::now();
        let outcome =
            signal_window_class(&class, SecondaryAction::Ping, Duration::from_millis(150));
        assert_eq!(outcome, SignalOutcome::WindowNotFound);
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    /// End to end against a hidden top-level window of a unique test class,
    /// pumped by a thread of this process (no Explorer windows, no esr.exe).
    #[test]
    fn signal_reaches_hidden_window() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::mpsc;
        use windows::Win32::Foundation::LRESULT;
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, MSG,
            PostMessageW, PostQuitMessage, RegisterClassW, UnregisterClassW, WINDOW_EX_STYLE,
            WM_APP, WM_DESTROY, WNDCLASSW, WS_OVERLAPPED,
        };

        static RECEIVED: AtomicU32 = AtomicU32::new(0);
        const WM_TEST_QUIT: u32 = WM_APP + 1;

        extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
            let m = InstanceMessages::register();
            if m.recognise(msg).is_some() {
                RECEIVED.store(msg, Ordering::SeqCst);
                return LRESULT(1);
            }
            unsafe {
                match msg {
                    WM_TEST_QUIT => {
                        let _ = DestroyWindow(hwnd);
                        LRESULT(0)
                    }
                    WM_DESTROY => {
                        PostQuitMessage(0);
                        LRESULT(0)
                    }
                    _ => DefWindowProcW(hwnd, msg, wp, lp),
                }
            }
        }

        let class = unique("ESR.Test.Engine");
        let class_w = HSTRING::from(class.as_str());
        let (tx, rx) = mpsc::channel::<usize>();
        let pump = {
            let class_w = class_w.clone();
            thread::spawn(move || unsafe {
                let hinstance = GetModuleHandleW(None).unwrap();
                let wc = WNDCLASSW {
                    lpfnWndProc: Some(wndproc),
                    hInstance: hinstance.into(),
                    lpszClassName: PCWSTR(class_w.as_ptr()),
                    ..Default::default()
                };
                assert_ne!(RegisterClassW(&wc), 0);
                // Not WS_VISIBLE: the window is never shown.
                let hwnd = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    &class_w,
                    &HSTRING::from("ESR test"),
                    WS_OVERLAPPED,
                    0,
                    0,
                    0,
                    0,
                    None,
                    None,
                    Some(hinstance.into()),
                    None,
                )
                .unwrap();
                tx.send(hwnd.0 as usize).unwrap();
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                    DispatchMessageW(&msg);
                }
                let _ = UnregisterClassW(&class_w, Some(hinstance.into()));
            })
        };
        let hwnd = HWND(rx.recv().unwrap() as *mut _);

        let messages = InstanceMessages::register();
        let me = std::process::id();

        let outcome =
            signal_window_class(&class, SecondaryAction::ShowSettings, FIND_WINDOW_BUDGET);
        assert_eq!(outcome, SignalOutcome::Delivered { pid: me });
        assert_eq!(RECEIVED.load(Ordering::SeqCst), messages.show_settings);

        let outcome = signal_window_class(&class, SecondaryAction::Ping, FIND_WINDOW_BUDGET);
        assert_eq!(outcome, SignalOutcome::Delivered { pid: me });
        assert_eq!(RECEIVED.load(Ordering::SeqCst), messages.ping);

        // The test window only records the message; it does not exit.
        let outcome = signal_window_class(&class, SecondaryAction::Exit, FIND_WINDOW_BUDGET);
        assert_eq!(outcome, SignalOutcome::Delivered { pid: me });
        assert_eq!(RECEIVED.load(Ordering::SeqCst), messages.exit);

        unsafe {
            PostMessageW(Some(hwnd), WM_TEST_QUIT, WPARAM(0), LPARAM(0)).unwrap();
        }
        pump.join().unwrap();
    }
}
