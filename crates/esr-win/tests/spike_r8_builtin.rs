//! Spike R8: built-in restore detection signals (Req 10.4).
//!
//! Run right after sign-in (before touching mouse or keyboard if you want to
//! see the "logon restore" case):
//!
//! ```text
//! cargo test -p esr-win --test spike_r8_builtin -- --ignored --nocapture
//! ```
//!
//! design.md, trigger detection step 3, classifies a new Explorer window as
//! `BuiltinRestore` when all of these hold:
//!
//! 1. `PersistBrowsers = 1`
//! 2. the process was started with `--background` at logon
//! 3. no user input since sign-in (`GetLastInputInfo` earlier than logon tick + 2 s)
//! 4. less than 60 s since the shell became ready
//!
//! How each input is obtained as a standard user (all read-only):
//!
//! * PersistBrowsers: `RegGetValueW(HKCU\Software\Microsoft\Windows\CurrentVersion\
//!   Explorer\Advanced, "PersistBrowsers", RRF_RT_REG_DWORD)`. Absent means 0.
//! * Logon tick: `WTSQuerySessionInformationW(WTS_CURRENT_SERVER_HANDLE,
//!   WTS_CURRENT_SESSION, WTSSessionInfo)` → `WTSINFOW::LogonTime` (UTC FILETIME).
//!   It is converted to the tick time base as
//!   `ms_since_logon = (GetSystemTimeAsFileTime() - LogonTime) / 10_000`.
//!   Chosen over the start time of the user's explorer.exe / userinit.exe because
//!   it does not change when Explorer is restarted, needs no process enumeration
//!   (userinit exits after a few seconds and may already be gone), and is
//!   available to the session's own user without elevation.
//!   Caveat: the conversion goes through wall-clock time, so a clock change
//!   between logon and the check skews it; at logon time this window is small.
//! * Last input: `GetLastInputInfo` → `dwTime` (32-bit `GetTickCount` base).
//!   `ms_since_input = GetTickCount().wrapping_sub(dwTime)`, which stays correct
//!   across the 49.7-day wrap. Condition 3 becomes
//!   `ms_since_input >= ms_since_logon - 2000`.
//! * Shell ready: `GetShellWindow` → `GetWindowThreadProcessId` → `OpenProcess(
//!   PROCESS_QUERY_LIMITED_INFORMATION)` → `GetProcessTimes` creation time.
//!   `GetShellWindow` returns null until the desktop window exists, so a
//!   non-null result plus the creation time of its process is a pull-based
//!   signal that can be read at any time. A `TaskbarCreated` timestamp is only
//!   observable by a process that was already running with a window when the
//!   broadcast happened, so it cannot be read after the fact; it remains useful
//!   only as an optional refinement. Process creation is a few seconds earlier
//!   than "ready", which makes the 60 s window slightly conservative.
//!   The shell process is also restarted on Explorer crash; then the window
//!   restarts too, which is acceptable because condition 3 still requires no
//!   input since logon.
//! * `--background` at logon: `std::env::args()` contains `--background` and
//!   this process's own creation time (`GetProcessTimes(GetCurrentProcess())`)
//!   is after the logon time. The test binary is never started that way, so the
//!   spike only reports the inputs.

#![cfg(windows)]

use windows::Win32::Foundation::{CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, FILETIME};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::System::RemoteDesktop::{
    WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION, WTSFreeMemory, WTSINFOW,
    WTSQuerySessionInformationW, WTSSessionInfo,
};
use windows::Win32::System::SystemInformation::{GetSystemTimeAsFileTime, GetTickCount};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::Win32::UI::WindowsAndMessaging::{GetShellWindow, GetWindowThreadProcessId};
use windows::core::{PWSTR, w};

const INPUT_GRACE_MS: u64 = 2_000;
const SHELL_READY_WINDOW_MS: u64 = 60_000;

fn ft_to_u64(ft: FILETIME) -> u64 {
    (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime)
}

/// `Ok(Some(v))` if set, `Ok(None)` if absent (treated as 0).
fn read_persist_browsers() -> Result<Option<u32>, String> {
    let mut data: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced"),
            w!("PersistBrowsers"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut data as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    match rc {
        ERROR_SUCCESS => Ok(Some(data)),
        ERROR_FILE_NOT_FOUND => Ok(None),
        other => Err(format!("RegGetValueW failed: {other:?}")),
    }
}

/// Logon time of the current session as a UTC FILETIME value.
fn read_logon_filetime() -> Result<u64, String> {
    let mut buf = PWSTR::null();
    let mut bytes = 0u32;
    unsafe {
        WTSQuerySessionInformationW(
            Some(WTS_CURRENT_SERVER_HANDLE),
            WTS_CURRENT_SESSION,
            WTSSessionInfo,
            &mut buf,
            &mut bytes,
        )
    }
    .map_err(|e| format!("WTSQuerySessionInformationW(WTSSessionInfo): {e}"))?;
    if buf.is_null() || (bytes as usize) < std::mem::size_of::<WTSINFOW>() {
        return Err(format!("unexpected WTSINFOW buffer ({bytes} bytes)"));
    }
    // SAFETY: the buffer holds a WTSINFOW of at least size_of::<WTSINFOW>() bytes.
    let info = unsafe { std::ptr::read_unaligned(buf.0 as *const WTSINFOW) };
    unsafe { WTSFreeMemory(buf.0 as *mut _) };
    println!("  session id            : {}", info.SessionId);
    if info.LogonTime <= 0 {
        return Err(format!("LogonTime not set ({})", info.LogonTime));
    }
    Ok(info.LogonTime as u64)
}

/// Milliseconds since the last input event, in the 32-bit tick time base.
fn read_ms_since_input() -> Result<u64, String> {
    let mut lii = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    if !unsafe { GetLastInputInfo(&mut lii) }.as_bool() {
        return Err(format!(
            "GetLastInputInfo: {}",
            windows::core::Error::from_thread()
        ));
    }
    let now = unsafe { GetTickCount() };
    println!("  GetTickCount / dwTime : {now} / {}", lii.dwTime);
    Ok(u64::from(now.wrapping_sub(lii.dwTime)))
}

/// Creation time (UTC FILETIME) and PID of the process owning the shell window.
fn read_shell_start() -> Result<(u32, u64), String> {
    let hwnd = unsafe { GetShellWindow() };
    if hwnd.0.is_null() {
        return Err("GetShellWindow returned null (shell not ready)".into());
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 {
        return Err("GetWindowThreadProcessId returned PID 0".into());
    }
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
        .map_err(|e| format!("OpenProcess({pid}): {e}"))?;
    let created = process_creation(process);
    unsafe {
        let _ = CloseHandle(process);
    }
    Ok((pid, created?))
}

fn process_creation(process: windows::Win32::Foundation::HANDLE) -> Result<u64, String> {
    let (mut c, mut e, mut k, mut u) = Default::default();
    unsafe { GetProcessTimes(process, &mut c, &mut e, &mut k, &mut u) }
        .map_err(|err| format!("GetProcessTimes: {err}"))?;
    Ok(ft_to_u64(c))
}

#[test]
#[ignore = "manual spike; run right after sign-in with --ignored --nocapture"]
fn spike_r8_builtin_restore_signals() {
    println!("== Spike R8: built-in restore detection signals ==");
    let now_ft = ft_to_u64(unsafe { GetSystemTimeAsFileTime() });

    // 1. PersistBrowsers
    let persist = read_persist_browsers().expect("PersistBrowsers must be readable");
    let persist_on = persist == Some(1);
    println!("PersistBrowsers         : {persist:?} (enabled = {persist_on})");

    // Logon tick
    println!("Logon time:");
    let logon_ft = read_logon_filetime().expect("session LogonTime must be readable");
    assert!(logon_ft <= now_ft, "LogonTime is in the future");
    let ms_since_logon = (now_ft - logon_ft) / 10_000;
    println!(
        "  ms since logon        : {ms_since_logon} ({:.1} min)",
        ms_since_logon as f64 / 60_000.0
    );

    // 3. Last input vs logon tick + 2 s
    println!("Last input:");
    let ms_since_input = read_ms_since_input().expect("GetLastInputInfo must succeed");
    println!("  ms since last input   : {ms_since_input}");
    let no_input_since_logon = ms_since_input + INPUT_GRACE_MS >= ms_since_logon;
    println!("  input before logon+2s : {no_input_since_logon}");

    // 4. Shell ready
    println!("Shell:");
    let (shell_pid, shell_ft) = read_shell_start().expect("shell process start must be readable");
    let ms_since_shell = now_ft.saturating_sub(shell_ft) / 10_000;
    let shell_after_logon_ms = shell_ft as i64 / 10_000 - logon_ft as i64 / 10_000;
    println!("  shell PID             : {shell_pid}");
    println!("  ms since shell start  : {ms_since_shell}");
    println!("  shell start - logon   : {shell_after_logon_ms} ms");
    let shell_recent = ms_since_shell < SHELL_READY_WINDOW_MS;
    println!("  < 60 s since shell    : {shell_recent}");

    // 2. --background at logon
    println!("Own process:");
    let background_flag = std::env::args().any(|a| a == "--background");
    let own_ft = process_creation(unsafe { GetCurrentProcess() })
        .expect("own process creation time must be readable");
    let own_after_logon_ms = own_ft as i64 / 10_000 - logon_ft as i64 / 10_000;
    println!("  --background in args  : {background_flag}");
    println!("  own start - logon     : {own_after_logon_ms} ms");
    let started_background_at_logon = background_flag && own_after_logon_ms >= 0;

    let builtin_restore =
        persist_on && started_background_at_logon && no_input_since_logon && shell_recent;
    println!("Heuristic:");
    println!("  PersistBrowsers = 1        : {persist_on}");
    println!("  --background at logon      : {started_background_at_logon}");
    println!("  no input since logon + 2 s : {no_input_since_logon}");
    println!("  < 60 s since shell ready   : {shell_recent}");
    println!("  => BuiltinRestore          : {builtin_restore}");
}
