//! Integration tests for session ends (task 16.2; Requirements 3.1–3.6).
//!
//! Every test is `#[ignore]`: it runs the real runtime in-process against real
//! File Explorer and opens Explorer windows through the shared
//! [`ExplorerGuard`] (which serialises Explorer-opening tests across processes
//! and closes every window the test opened, also on failure). Run alone:
//!
//! ```text
//! cargo test -p esr-app --test session_ends -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Each test starts [`run`] on its own thread with a temporary data directory,
//! no UI thread, no panic guard, no Session_File at start, restoration turned
//! off in the passed settings (Session_Ends are still saved, Req 4.9; this
//! makes sure no test ever restores windows), and the built-in restore prompt
//! marked declined (so nothing is prompted). Nothing touches the real data
//! directory, the startup registration, or the real settings. The runtime is
//! stopped with `ESR_EXIT`, sent to the `ESR.Engine` window of this process
//! only.
//!
//! "Next start" is checked the way a fresh start reads the file: `Storage`
//! on the data directory → `read_session` → `StoreLogic::startup` (what
//! `Engine::new` runs), which must yield the expected Pending_Snapshot and no
//! startup command (no `ERROR`, no backup, no abrupt-end `SAVE`).
//!
//! # Cases
//!
//! | Test | How | Checks |
//! |---|---|---|
//! | `last_window_closed_*` | Real window with 2 tabs, closed with `SC_CLOSE` after a second window was closed. Needs no non-test Explorer window (else skipped). | Cause `last-window-closed`, the pre-close snapshot, written ≤ 1 s after the close (Req 3.1); next start. |
//! | `last_tab_closed_*` | Real window with 2 tabs; one `WM_CLOSE` closes one tab, the second closes the last tab. Same precondition. | Cause `last-tab-closed`, that tab included, ≤ 1 s (Req 3.2); next start. |
//! | `sign_out_*`, `shutdown_*`, `restart_*` | Simulated: `WM_QUERYENDSESSION` / `WM_ENDSESSION` sent to this process's Engine window only (never a real session end). | Return `TRUE`, no shutdown block reason, urgent write done when the ≤ 1.5 s block returns (Req 3.6), cause, every open window, windows closed afterwards stay saved (Req 3.5); next start from the bytes on disk at `WM_ENDSESSION` (the moment the process would end). |
//! | `separate_process_termination_*` | A window in its own Explorer process (`explorer.exe /separate`), killed by PID after verifying that the process is new, is `explorer.exe`, is not the shell, and hosts only the test's window. Skipped if any check fails. | No other window open: cause `explorer-terminated` ≤ 1 s (Req 3.4). Otherwise the killed window stays in the Live_Session (Req 3.3), checked through a simulated sign-out capture. Next start. |
//! | `taskkill_of_explorer_*`, `explorer_restart_*` | **Destructive** (kill the shell). Gated by `ESR_ALLOW_DESTRUCTIVE=1` in addition to `#[ignore]`; they restart `explorer.exe` if it is not running at the end. | Cause `explorer-terminated` ≤ 1 s (Req 3.3, 3.4); after the restart the runtime tracks a new window again. Next start. |
//!
//! Restart and shutdown deliver the same `WM_QUERYENDSESSION` (`lParam` 0);
//! the runtime tells them apart by the newest System-log event 1074, which
//! only a real restart writes. The restart test therefore expects the cause
//! that the event-log derivation yields now and reports when that is not
//! `restart`; the shutdown test uses `EndKindSource::AssumeShutdown`.

#![cfg(windows)]

#[path = "../../esr-win/tests/common/explorer_guard.rs"]
#[allow(dead_code)]
mod explorer_guard;

use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use esr_app::instance::{ENGINE_WINDOW_CLASS, InstanceMessages};
use esr_app::runtime::{RuntimeOptions, Storage, run, startup_input};
use esr_core::engine::{EndKind, SessionFileRead};
use esr_core::format::parse;
use esr_core::model::{SaveCause, SessionDoc, Status, WindowRec};
use esr_core::settings::Settings;
use esr_core::store::StoreLogic;
use esr_win::session_end::SessionEndListener;
use esr_win::shell_util::{
    ShellTab, StaGuard, current_folder_pidl, enumerate_all_explorer_windows, is_cabinet_class,
    is_cabinet_window, pidl_to_bytes, window_class_name, window_process_id,
};
use esr_win::strategy::{EndKindSource, NEW_TAB_COMMAND, Strategies};
use explorer_guard::{ExplorerGuard, parsing_name, pump_for, pump_once};
use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, HWND, LPARAM, WAIT_OBJECT_0, WPARAM};
use windows::Win32::System::Ole::{SafeArrayAccessData, SafeArrayCreateVector, SafeArrayUnaccessData};
use windows::Win32::System::Shutdown::ShutdownBlockReasonQuery;
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, QueryFullProcessImageNameW, TerminateProcess,
    WaitForSingleObject,
};
use windows::Win32::System::Variant::{
    VARENUM, VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_ARRAY, VT_UI1,
};
use windows::Win32::UI::Shell::{
    ILCreateFromPathW, ILFree, IWebBrowser2, SEE_MASK_FLAG_NO_UI, SEE_MASK_IDLIST,
    SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    ENDSESSION_LOGOFF, EnumWindows, FindWindowExW, FindWindowW, GetShellWindow,
    GetWindowThreadProcessId, IsWindow, IsWindowVisible, PostMessageW, SC_CLOSE, SMTO_NORMAL,
    SW_SHOWNOACTIVATE, SendMessageTimeoutW, WM_CLOSE, WM_COMMAND, WM_ENDSESSION,
    WM_QUERYENDSESSION, WM_SYSCOMMAND,
};
use windows::core::{BOOL, HSTRING, PCWSTR, PWSTR, w};

/// Environment variable that allows the tests that kill the shell.
const ENV_DESTRUCTIVE: &str = "ESR_ALLOW_DESTRUCTIVE";
/// Req 3.1, 3.2, 3.4: the Session_End write lands within 1 s.
const SESSION_END_WRITE_BOUND: Duration = Duration::from_secs(1);
/// Req 3.6: written within 2 s of the session end beginning (the runtime
/// blocks `WM_QUERYENDSESSION` for at most 1.5 s).
const END_SESSION_WRITE_BOUND: Duration = Duration::from_secs(2);
/// `BrowserNavConstants::navNoHistory`.
const NAV_NO_HISTORY: i32 = 0x2;

// ---------------------------------------------------------------------------
// General helpers
// ---------------------------------------------------------------------------

/// Pumps messages until `f` yields or `timeout` passes.
fn wait<T>(timeout: Duration, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let end = Instant::now() + timeout;
    loop {
        pump_once();
        if let Some(v) = f() {
            return Some(v);
        }
        if Instant::now() >= end {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Lower-case path without a `\\?\` prefix or a trailing separator.
fn norm(p: &str) -> String {
    p.strip_prefix(r"\\?\").unwrap_or(p).trim_end_matches('\\').to_lowercase()
}

fn key(p: &Path) -> String {
    norm(&p.to_string_lossy())
}

fn hwnd_of(raw: isize) -> HWND {
    HWND(raw as *mut c_void)
}

fn raw(h: HWND) -> isize {
    h.0 as isize
}

/// Whether `hwnd` is still a top-level Explorer window.
fn alive(hwnd: HWND) -> bool {
    // SAFETY: IsWindow tolerates stale handles.
    unsafe { IsWindow(Some(hwnd)) }.as_bool() && is_cabinet_window(hwnd)
}

/// Result of a test that may be skipped for safety or precondition reasons.
enum Outcome {
    Ran,
    Skipped(String),
}

fn report(test: &str, outcome: &Outcome) {
    match outcome {
        Outcome::Ran => println!("[session_ends] {test}: RAN"),
        Outcome::Skipped(why) => println!("[session_ends] {test}: SKIPPED: {why}"),
    }
}

fn test_settings() -> Settings {
    Settings {
        // Never restore anything from a test (Session_Ends still save, Req 4.9).
        restoration_enabled: false,
        start_at_sign_in: true,
        builtin_prompt_declined: true,
        tab_folding_enabled: true,
    }
}

// ---------------------------------------------------------------------------
// The in-process runtime
// ---------------------------------------------------------------------------

/// The `ESR.Engine` windows created by this process.
fn own_engine_windows() -> Vec<isize> {
    let class = HSTRING::from(ENGINE_WINDOW_CLASS);
    let mut out = Vec::new();
    let mut after: Option<HWND> = None;
    loop {
        // SAFETY: plain window enumeration by class.
        let Ok(hwnd) = (unsafe { FindWindowExW(None, after, &class, PCWSTR::null()) }) else {
            return out;
        };
        if hwnd.is_invalid() {
            return out;
        }
        let mut pid = 0;
        // SAFETY: valid out pointer.
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        if pid == std::process::id() {
            out.push(raw(hwnd));
        }
        after = Some(hwnd);
    }
}

/// The runtime on its own thread, with a temporary data directory. Dropping
/// it stops the runtime (`ESR_EXIT` to its own Engine window) and deletes
/// the directory.
struct Runtime {
    data_dir: PathBuf,
    engine: Option<HWND>,
    thread: Option<JoinHandle<i32>>,
}

impl Runtime {
    fn start(name: &str, strategies: Strategies) -> Result<Self, String> {
        let data_dir =
            std::env::temp_dir().join(format!("esr_session_ends_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_dir);
        let before = own_engine_windows();
        let thread = {
            let data_dir = data_dir.clone();
            std::thread::Builder::new()
                .name(format!("esr-engine-{name}"))
                .spawn(move || {
                    let storage = Storage::open(&data_dir);
                    let input = startup_input(SessionFileRead::Absent, false);
                    let options = RuntimeOptions {
                        strategies,
                        ui: false,
                        panic_guard: false,
                        ..RuntimeOptions::default()
                    };
                    run(test_settings(), input, storage, options)
                })
                .map_err(|e| format!("spawn the runtime: {e}"))?
        };
        let mut rt = Runtime { data_dir, engine: None, thread: Some(thread) };
        rt.engine = wait(Duration::from_secs(10), || {
            own_engine_windows().into_iter().find(|h| !before.contains(h))
        })
        .map(hwnd_of);
        if rt.engine.is_none() {
            return Err("the runtime created no ESR.Engine window".into());
        }
        // Let it attach to the shell and scan the open windows.
        pump_for(Duration::from_millis(500));
        Ok(rt)
    }

    fn engine(&self) -> HWND {
        self.engine.expect("engine window")
    }

    fn dir(&self) -> &Path {
        &self.data_dir
    }

    /// `ESR_EXIT`, then waits for the runtime thread to end with exit code 0.
    fn stop(&mut self) -> Result<(), String> {
        let Some(thread) = self.thread.take() else { return Ok(()) };
        if let Some(engine) = self.engine {
            let exit = InstanceMessages::register().exit;
            // SAFETY: posting a registered message to this process's own window.
            let _ = unsafe { PostMessageW(Some(engine), exit, WPARAM(0), LPARAM(0)) };
        }
        if wait(Duration::from_secs(10), || thread.is_finished().then_some(())).is_none() {
            return Err("the runtime did not exit after ESR_EXIT".into());
        }
        match thread.join() {
            Ok(0) => Ok(()),
            Ok(code) => Err(format!("the runtime exited with {code}")),
            Err(_) => Err("the runtime thread panicked".into()),
        }
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        if let Err(e) = self.stop() {
            eprintln!("[session_ends] stopping the runtime: {e}");
        }
        let _ = std::fs::remove_dir_all(&self.data_dir);
    }
}

// ---------------------------------------------------------------------------
// Session_File helpers
// ---------------------------------------------------------------------------

fn read_bytes(dir: &Path) -> Option<Vec<u8>> {
    std::fs::read(dir.join("session.esr")).ok()
}

fn read_doc(dir: &Path) -> Option<(SessionDoc, Vec<u8>)> {
    let bytes = read_bytes(dir)?;
    let doc = parse(&bytes).ok()?;
    Some((doc, bytes))
}

/// Waits for a parseable Session_File satisfying `pred`.
fn wait_doc(
    dir: &Path,
    timeout: Duration,
    mut pred: impl FnMut(&SessionDoc) -> bool,
) -> Option<(SessionDoc, Vec<u8>)> {
    wait(timeout, || read_doc(dir).filter(|(d, _)| pred(d)))
}

fn tab_names(w: &WindowRec) -> Vec<String> {
    w.tabs.iter().map(|t| norm(&t.location.parsing_name.to_string_lossy())).collect()
}

/// The saved window whose tabs are exactly `folders`, left to right.
fn find_window<'a>(doc: &'a SessionDoc, folders: &[&Path]) -> Option<&'a WindowRec> {
    let want: Vec<String> = folders.iter().map(|f| key(f)).collect();
    doc.snapshot.windows.iter().find(|w| tab_names(w) == want)
}

fn lists(doc: &SessionDoc, folder: &Path) -> bool {
    let k = key(folder);
    doc.snapshot.windows.iter().any(|w| tab_names(w).contains(&k))
}

fn describe(doc: &SessionDoc) -> String {
    // Folder names only (the temp root is the same for every tab).
    let short = |n: &String| n.rsplit('\\').next().unwrap_or(n).to_string();
    let windows: Vec<String> = doc
        .snapshot
        .windows
        .iter()
        .map(|w| {
            let names: Vec<String> = tab_names(w).iter().map(short).collect();
            format!("[{}] active={}", names.join(", "), w.active)
        })
        .collect();
    format!(
        "cause={:?} status={:?} failed_attempts={} windows={{{}}}",
        doc.cause,
        doc.status,
        doc.failed_attempts,
        windows.join("; ")
    )
}

/// Checks a Session_End document: cause, Pending, `failed_attempts = 0`.
fn check_end_doc(doc: &SessionDoc, cause: SaveCause) -> Result<(), String> {
    if doc.cause != cause || doc.status != Status::Pending || doc.failed_attempts != 0 {
        return Err(format!("expected a Pending {cause:?} save, found {}", describe(doc)));
    }
    Ok(())
}

/// "Next start": what a fresh Session_Monitor start finds in `dir`
/// (`Storage::read_session` + `StoreLogic::startup`, as `Engine::new`).
fn next_start(dir: &Path) -> Result<SessionDoc, String> {
    let storage = Storage::open(dir);
    let read = storage.read_session();
    drop(storage);
    if !matches!(read, SessionFileRead::Content(_)) {
        return Err(format!("next start: the Session_File reads as {read:?}"));
    }
    let (store, commands) = StoreLogic::startup(&read);
    if !commands.is_empty() {
        return Err(format!("next start issued startup commands {commands:?}"));
    }
    store
        .pending_snapshot()
        .cloned()
        .ok_or_else(|| "next start: no Pending_Snapshot".to_string())
}

/// Next start from `bytes` (the file as it was on disk when the process
/// would have ended), in a fresh temp data directory.
fn next_start_from(bytes: &[u8], name: &str) -> Result<SessionDoc, String> {
    let dir = std::env::temp_dir()
        .join(format!("esr_session_ends_next_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let result = std::fs::write(dir.join("session.esr"), bytes)
        .map_err(|e| e.to_string())
        .and_then(|()| next_start(&dir));
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn check_next_start(found: &SessionDoc, saved: &SessionDoc) -> Result<(), String> {
    if found != saved {
        return Err(format!(
            "next start found a different snapshot:\n  saved: {}\n  found: {}",
            describe(saved),
            describe(found)
        ));
    }
    println!("  next start: Pending_Snapshot {}", describe(found));
    Ok(())
}

// ---------------------------------------------------------------------------
// Explorer helpers (all windows go through the guard)
// ---------------------------------------------------------------------------

fn pidl_of(path: &Path) -> Result<Vec<u8>, String> {
    // SAFETY: valid path string; the PIDL is copied, then freed.
    unsafe {
        let pidl = ILCreateFromPathW(&HSTRING::from(path.as_os_str()));
        if pidl.is_null() {
            return Err(format!("ILCreateFromPathW failed for {}", path.display()));
        }
        let bytes = pidl_to_bytes(pidl);
        ILFree(Some(pidl));
        Ok(bytes)
    }
}

/// Opens a new Explorer window on `dir` with the `opennewwindow` verb (from
/// a launcher STA thread).
fn request_open(dir: PathBuf) -> Result<(), String> {
    std::thread::spawn(move || {
        let _sta = StaGuard::init().map_err(|e| e.to_string())?;
        // SAFETY: valid path string; the PIDL is freed after the call.
        unsafe {
            let pidl = ILCreateFromPathW(&HSTRING::from(dir.as_os_str()));
            if pidl.is_null() {
                return Err("ILCreateFromPathW failed".to_string());
            }
            let mut sei = SHELLEXECUTEINFOW {
                cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
                fMask: SEE_MASK_IDLIST | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
                lpVerb: w!("opennewwindow"),
                lpIDList: pidl.cast(),
                nShow: SW_SHOWNOACTIVATE.0,
                ..Default::default()
            };
            let r = ShellExecuteExW(&mut sei);
            ILFree(Some(pidl));
            r.map_err(|e| format!("ShellExecuteExW: {e}"))
        }
    })
    .join()
    .map_err(|_| "launcher panicked".to_string())?
}

/// Opens `dir` in a window of its own Explorer process
/// (`explorer.exe /separate,<dir>`).
fn request_open_separate(dir: PathBuf) -> Result<(), String> {
    std::thread::spawn(move || {
        let _sta = StaGuard::init().map_err(|e| e.to_string())?;
        let file = HSTRING::from("explorer.exe");
        let params = HSTRING::from(format!("/separate,\"{}\"", dir.display()));
        let mut sei = SHELLEXECUTEINFOW {
            cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
            lpFile: PCWSTR(file.as_ptr()),
            lpParameters: PCWSTR(params.as_ptr()),
            nShow: SW_SHOWNOACTIVATE.0,
            ..Default::default()
        };
        // SAFETY: the strings outlive the call.
        unsafe { ShellExecuteExW(&mut sei) }.map_err(|e| format!("ShellExecuteExW: {e}"))
    })
    .join()
    .map_err(|_| "launcher panicked".to_string())?
}

fn folder_of(browser: &IWebBrowser2) -> Option<String> {
    current_folder_pidl(browser).ok().and_then(|p| parsing_name(&p)).map(|n| norm(&n))
}

/// ShellWindows tabs of the window `top`.
fn tabs_of(top: HWND) -> Vec<ShellTab> {
    enumerate_all_explorer_windows()
        .ok()
        .and_then(|groups| groups.into_iter().find(|g| g.top_hwnd == top))
        .map(|g| g.tabs)
        .unwrap_or_default()
}

/// The new window (not pre-existing) showing `dir` in its single tab,
/// adopted by the guard as soon as it appears.
fn wait_for_window(guard: &ExplorerGuard, dir: &Path) -> Option<HWND> {
    let target = key(dir);
    let hwnd = wait(Duration::from_secs(20), || {
        enumerate_all_explorer_windows()
            .unwrap_or_default()
            .into_iter()
            .filter(|g| !guard.is_pre_existing(g.top_hwnd))
            .find(|g| g.tabs.iter().any(|t| folder_of(&t.browser).as_deref() == Some(&target)))
            .map(|g| {
                guard.adopt(g.top_hwnd);
                g.top_hwnd
            })
    })?;
    // Explorer may replace a freshly created window; let it settle.
    pump_for(Duration::from_millis(500));
    (alive(hwnd) && tabs_of(hwnd).len() == 1).then_some(hwnd)
}

fn open_window(guard: &ExplorerGuard, dir: &Path) -> Result<HWND, String> {
    request_open(dir.to_path_buf())?;
    wait_for_window(guard, dir).ok_or_else(|| format!("no window opened on {}", dir.display()))
}

/// `Navigate2(PIDL as VT_ARRAY|VT_UI1, navNoHistory)`, as the navigator
/// does. The SAFEARRAY is leaked on purpose (R2: destroying it after
/// `Navigate2` corrupted the heap).
fn navigate_pidl(browser: &IWebBrowser2, pidl: &[u8]) -> Result<(), String> {
    let flags = VARIANT::from(NAV_NO_HISTORY);
    let empty = VARIANT::default();
    // SAFETY: the SAFEARRAY is created, filled, and passed to Navigate2.
    unsafe {
        let psa = SafeArrayCreateVector(VT_UI1, 0, pidl.len() as u32);
        if psa.is_null() {
            return Err("SafeArrayCreateVector failed".into());
        }
        let mut data: *mut c_void = std::ptr::null_mut();
        SafeArrayAccessData(psa, &mut data).map_err(|e| e.to_string())?;
        std::ptr::copy_nonoverlapping(pidl.as_ptr(), data.cast::<u8>(), pidl.len());
        SafeArrayUnaccessData(psa).map_err(|e| e.to_string())?;
        let url = ManuallyDrop::new(VARIANT {
            Anonymous: VARIANT_0 {
                Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                    vt: VARENUM(VT_ARRAY.0 | VT_UI1.0),
                    wReserved1: 0,
                    wReserved2: 0,
                    wReserved3: 0,
                    Anonymous: VARIANT_0_0_0 { parray: psa },
                }),
            },
        });
        browser
            .Navigate2(&*url, Some(&flags), Some(&empty), Some(&empty), Some(&empty))
            .map_err(|e| format!("Navigate2: {e}"))
    }
}

/// Appends a tab to `top` (`WM_COMMAND 0xA21B`) and navigates it to `dir`.
fn add_tab(top: HWND, dir: &Path) -> Result<(), String> {
    let before = tabs_of(top);
    let known: Vec<isize> = before.iter().filter_map(|t| t.tab_hwnd.map(raw)).collect();
    let post_to = before.first().and_then(|t| t.tab_hwnd).ok_or("no tab HWND to post to")?;
    // SAFETY: posting a plain message to the test's own window's tab.
    unsafe { PostMessageW(Some(post_to), WM_COMMAND, WPARAM(NEW_TAB_COMMAND), LPARAM(0)) }
        .map_err(|e| format!("post the new-tab command: {e}"))?;
    let tab = wait(Duration::from_secs(5), || {
        tabs_of(top)
            .into_iter()
            .find(|t| t.tab_hwnd.is_some_and(|h| !known.contains(&raw(h))))
    })
    .ok_or("the new tab did not register")?;
    pump_for(Duration::from_millis(250));
    navigate_pidl(&tab.browser, &pidl_of(dir)?)?;
    let target = key(dir);
    wait(Duration::from_secs(5), || (folder_of(&tab.browser).as_deref() == Some(&target)).then_some(()))
        .ok_or_else(|| format!("the new tab did not reach {}", dir.display()))?;
    if tabs_of(top).len() != before.len() + 1 {
        return Err(format!("expected {} tabs, found {}", before.len() + 1, tabs_of(top).len()));
    }
    Ok(())
}

/// Waits until `hwnd` is gone; returns when that was seen.
fn wait_gone(hwnd: HWND, timeout: Duration) -> Option<Instant> {
    let end = Instant::now() + timeout;
    loop {
        if !alive(hwnd) {
            return Some(Instant::now());
        }
        if Instant::now() >= end {
            return None;
        }
        pump_once();
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Explorer windows that the test did not open (pre-existing or opened by
/// someone else meanwhile).
fn foreign_cabinets(guard: &ExplorerGuard) -> Vec<isize> {
    explorer_guard::top_level_cabinets()
        .into_iter()
        .filter(|&h| !guard.is_adopted(hwnd_of(h)))
        .collect()
}

// ---------------------------------------------------------------------------
// Simulated session end (this process's Engine window only)
// ---------------------------------------------------------------------------

struct Reply {
    delivered: bool,
    result: usize,
    elapsed: Duration,
}

fn send_to_engine(engine: HWND, msg: u32, wparam: usize, lparam: isize) -> Reply {
    // Safety net: only ever this process's own Engine window.
    assert!(own_engine_windows().contains(&raw(engine)), "not this process's Engine window");
    let mut result = 0usize;
    let start = Instant::now();
    // SAFETY: plain send without pointers in wParam/lParam.
    let ok = unsafe {
        SendMessageTimeoutW(
            engine,
            msg,
            WPARAM(wparam),
            LPARAM(lparam),
            SMTO_NORMAL,
            5_000,
            Some(&mut result),
        )
    };
    Reply { delivered: ok.0 != 0, result, elapsed: start.elapsed() }
}

/// `WM_QUERYENDSESSION`: must be answered `TRUE` (allow) within the 1.5 s
/// block with no shutdown block reason set (Req 3.6).
fn query_end_session(engine: HWND, lparam: isize) -> Result<Duration, String> {
    let reply = send_to_engine(engine, WM_QUERYENDSESSION, 0, lparam);
    if !reply.delivered {
        return Err("WM_QUERYENDSESSION was not answered within 5 s".into());
    }
    if reply.result != 1 {
        return Err(format!("WM_QUERYENDSESSION returned {} (expected TRUE)", reply.result));
    }
    let mut len = 0u32;
    // SAFETY: size query only (no buffer); fails when no reason is set.
    if unsafe { ShutdownBlockReasonQuery(engine, None, &mut len) }.is_ok() {
        return Err("a shutdown block reason is set on the Engine window".into());
    }
    println!("  WM_QUERYENDSESSION(lParam={lparam:#x}) answered TRUE after {:?}", reply.elapsed);
    Ok(reply.elapsed)
}

fn end_session(engine: HWND, ending: bool, lparam: isize) -> Result<(), String> {
    let reply = send_to_engine(engine, WM_ENDSESSION, usize::from(ending), lparam);
    if !reply.delivered {
        return Err("WM_ENDSESSION was not handled within 5 s".into());
    }
    println!("  WM_ENDSESSION(ending={ending}) handled after {:?}", reply.elapsed);
    Ok(())
}

fn cause_of(kind: EndKind) -> SaveCause {
    match kind {
        EndKind::SignOut => SaveCause::SignOut,
        EndKind::Shutdown => SaveCause::Shutdown,
        EndKind::Restart => SaveCause::Restart,
    }
}

// ---------------------------------------------------------------------------
// Last window / last tab closed (Req 3.1, 3.2)
// ---------------------------------------------------------------------------

/// Waits for the Session_End write after the last close (seen at `gone`)
/// and checks cause, timing, and that it equals the pre-close checkpoint.
fn check_close_save(
    rt: &Runtime,
    gone: Instant,
    cause: SaveCause,
    pre: &SessionDoc,
) -> Result<SessionDoc, String> {
    let (doc, _) = wait_doc(rt.dir(), Duration::from_secs(5), |d| d.cause != SaveCause::Checkpoint)
        .ok_or("no Session_End write after the last close")?;
    let delay = gone.elapsed();
    // `wait_doc` returned as soon as the file changed; measure that moment.
    println!("  Session_End write seen {delay:?} after the close: {}", describe(&doc));
    check_end_doc(&doc, cause)?;
    if doc.snapshot != pre.snapshot {
        return Err(format!(
            "the Session_End snapshot differs from the pre-close session:\n  pre:  {}\n  save: {}",
            describe(pre),
            describe(&doc)
        ));
    }
    if delay > SESSION_END_WRITE_BOUND {
        return Err(format!("the Session_End write took {delay:?} (> 1 s)"));
    }
    Ok(doc)
}

/// Waits for a checkpoint satisfying `pred`, then for the file to stay
/// unchanged for 1 s (so it is the session right before the next action).
fn settled_checkpoint(
    rt: &Runtime,
    what: &str,
    pred: impl Fn(&SessionDoc) -> bool,
) -> Result<SessionDoc, String> {
    wait_doc(rt.dir(), Duration::from_secs(10), |d| d.cause == SaveCause::Checkpoint && pred(d))
        .ok_or_else(|| {
            let now = read_doc(rt.dir()).map(|(d, _)| describe(&d));
            format!("no checkpoint with {what}; file: {now:?}")
        })?;
    let mut last = read_bytes(rt.dir());
    let mut stable_since = Instant::now();
    let end = Instant::now() + Duration::from_secs(10);
    while stable_since.elapsed() < Duration::from_secs(1) {
        if Instant::now() > end {
            return Err(format!("the Session_File kept changing ({what})"));
        }
        pump_for(Duration::from_millis(50));
        let now = read_bytes(rt.dir());
        if now != last {
            last = now;
            stable_since = Instant::now();
        }
    }
    let (doc, _) = read_doc(rt.dir()).ok_or("the Session_File is unreadable")?;
    if doc.cause != SaveCause::Checkpoint || !pred(&doc) {
        return Err(format!("the settled file no longer has {what}: {}", describe(&doc)));
    }
    println!("  pre-close checkpoint: {}", describe(&doc));
    Ok(doc)
}

#[test]
#[ignore = "opens File Explorer windows; run alone with --ignored --test-threads=1"]
fn last_window_closed_saves_the_pre_close_session() {
    const TEST: &str = "last_window_closed_saves_the_pre_close_session";
    let _sta = StaGuard::init().expect("STA init");
    let guard = ExplorerGuard::new("session_ends_last_window");
    let foreign = foreign_cabinets(&guard);
    if !foreign.is_empty() {
        let o = Outcome::Skipped(format!(
            "{} non-test Explorer window(s) are open, so the test's window would not be the \
             last one (they are never closed by tests)",
            foreign.len()
        ));
        report(TEST, &o);
        guard.finish().unwrap();
        return;
    }
    let (a, b, c) = (guard.dir("lw_a"), guard.dir("lw_b"), guard.dir("lw_c"));
    let mut rt = Runtime::start("last_window", Strategies::DEFAULT).expect("start the runtime");

    let result = (|| -> Result<SessionDoc, String> {
        let w1 = open_window(&guard, &a)?;
        add_tab(w1, &b)?;
        let w2 = open_window(&guard, &c)?;
        settled_checkpoint(&rt, "both windows", |d| {
            find_window(d, &[&a, &b]).is_some() && find_window(d, &[&c]).is_some()
        })?;
        // Closing a window while another stays open is no Session_End.
        if !guard.close(w2) {
            return Err("could not close the second window".into());
        }
        let pre = settled_checkpoint(&rt, "only the two-tab window", |d| {
            d.snapshot.windows.len() == 1 && find_window(d, &[&a, &b]).is_some()
        })?;
        // Close the whole two-tab window (as its title-bar close button).
        // SAFETY: posting to the test's own (adopted) window.
        unsafe { PostMessageW(Some(w1), WM_SYSCOMMAND, WPARAM(SC_CLOSE as usize), LPARAM(0)) }
            .map_err(|e| e.to_string())?;
        let gone = wait_gone(w1, Duration::from_secs(5)).ok_or_else(|| {
            format!("SC_CLOSE did not close the whole window ({} tab(s) left)", tabs_of(w1).len())
        })?;
        if !foreign_cabinets(&guard).is_empty() {
            return Err("a non-test Explorer window appeared during the test".into());
        }
        check_close_save(&rt, gone, SaveCause::LastWindowClosed, &pre)
    })();

    let stopped = rt.stop();
    let next = result.as_ref().ok().map(|doc| next_start(rt.dir()).and_then(|n| check_next_start(&n, doc)));
    drop(rt);
    let cleanup = guard.finish();
    result.unwrap();
    stopped.unwrap();
    next.unwrap().unwrap();
    cleanup.unwrap();
    report(TEST, &Outcome::Ran);
}

#[test]
#[ignore = "opens File Explorer windows; run alone with --ignored --test-threads=1"]
fn last_tab_closed_saves_the_session_with_that_tab() {
    const TEST: &str = "last_tab_closed_saves_the_session_with_that_tab";
    let _sta = StaGuard::init().expect("STA init");
    let guard = ExplorerGuard::new("session_ends_last_tab");
    let foreign = foreign_cabinets(&guard);
    if !foreign.is_empty() {
        let o = Outcome::Skipped(format!(
            "{} non-test Explorer window(s) are open, so the test's tab would not be the \
             last one (they are never closed by tests)",
            foreign.len()
        ));
        report(TEST, &o);
        guard.finish().unwrap();
        return;
    }
    let (a, b) = (guard.dir("lt_a"), guard.dir("lt_b"));
    let mut rt = Runtime::start("last_tab", Strategies::DEFAULT).expect("start the runtime");

    let result = (|| -> Result<SessionDoc, String> {
        let w = open_window(&guard, &a)?;
        add_tab(w, &b)?;
        settled_checkpoint(&rt, "both tabs", |d| find_window(d, &[&a, &b]).is_some())?;
        // One WM_CLOSE closes one tab (the selected one) of a multi-tab window.
        // SAFETY: posting to the test's own (adopted) window.
        unsafe { PostMessageW(Some(w), WM_CLOSE, WPARAM(0), LPARAM(0)) }.map_err(|e| e.to_string())?;
        wait(Duration::from_secs(5), || (tabs_of(w).len() == 1).then_some(()))
            .ok_or("WM_CLOSE did not close exactly one tab")?;
        if !alive(w) {
            return Err("the window closed with its first tab".into());
        }
        let remaining = tabs_of(w)
            .first()
            .and_then(|t| folder_of(&t.browser))
            .ok_or("cannot read the remaining tab's folder")?;
        let remaining = if remaining == key(&a) { a.clone() } else { b.clone() };
        let pre = settled_checkpoint(&rt, "only the remaining tab", |d| {
            d.snapshot.windows.len() == 1 && find_window(d, &[&remaining]).is_some()
        })?;
        // Close the last tab.
        // SAFETY: as above.
        unsafe { PostMessageW(Some(w), WM_CLOSE, WPARAM(0), LPARAM(0)) }.map_err(|e| e.to_string())?;
        let gone = wait_gone(w, Duration::from_secs(5)).ok_or("closing the last tab left the window open")?;
        if !foreign_cabinets(&guard).is_empty() {
            return Err("a non-test Explorer window appeared during the test".into());
        }
        let doc = check_close_save(&rt, gone, SaveCause::LastTabClosed, &pre)?;
        if find_window(&doc, &[&remaining]).is_none() {
            return Err(format!("the closed last tab is missing: {}", describe(&doc)));
        }
        Ok(doc)
    })();

    let stopped = rt.stop();
    let next = result.as_ref().ok().map(|doc| next_start(rt.dir()).and_then(|n| check_next_start(&n, doc)));
    drop(rt);
    let cleanup = guard.finish();
    result.unwrap();
    stopped.unwrap();
    next.unwrap().unwrap();
    cleanup.unwrap();
    report(TEST, &Outcome::Ran);
}

// ---------------------------------------------------------------------------
// Sign-out, shutdown, restart (Req 3.5, 3.6), simulated
// ---------------------------------------------------------------------------

/// Opens a two-tab window and a one-tab window, sends `WM_QUERYENDSESSION`
/// with `lparam` to the runtime's own Engine window, and checks the urgent
/// write. Then closes the one-tab window (as the session end would) and
/// checks that it stays saved, sends `WM_ENDSESSION(TRUE)`, and checks the
/// file at the next start.
fn end_session_case(name: &str, strategies: Strategies, lparam: isize, cause: SaveCause) {
    let _sta = StaGuard::init().expect("STA init");
    let guard = ExplorerGuard::new(&format!("session_ends_{name}"));
    let (a, b, c) = (guard.dir("es_a"), guard.dir("es_b"), guard.dir("es_c"));
    let mut rt = Runtime::start(name, strategies).expect("start the runtime");

    let result = (|| -> Result<(SessionDoc, Vec<u8>), String> {
        let w1 = open_window(&guard, &a)?;
        add_tab(w1, &b)?;
        let w2 = open_window(&guard, &c)?;
        settled_checkpoint(&rt, "both windows", |d| {
            find_window(d, &[&a, &b]).is_some() && find_window(d, &[&c]).is_some()
        })?;
        let open_now = explorer_guard::top_level_cabinets().len();

        let elapsed = query_end_session(rt.engine(), lparam)?;
        // The urgent write is complete when the (≤ 1.5 s) block returns.
        let (doc, _) = read_doc(rt.dir()).ok_or("no readable Session_File after WM_QUERYENDSESSION")?;
        println!("  after WM_QUERYENDSESSION: {}", describe(&doc));
        check_end_doc(&doc, cause)?;
        if find_window(&doc, &[&a, &b]).is_none() || find_window(&doc, &[&c]).is_none() {
            return Err(format!("not every test window was saved: {}", describe(&doc)));
        }
        if elapsed > END_SESSION_WRITE_BOUND {
            return Err(format!("WM_QUERYENDSESSION blocked {elapsed:?} (> 2 s)"));
        }
        println!(
            "  {open_now} top-level Explorer window(s) open, {} saved",
            doc.snapshot.windows.len()
        );

        // The session end closes windows: they stay in the saved session.
        if !guard.close(w2) {
            return Err("could not close the second window".into());
        }
        pump_for(Duration::from_millis(1_500)); // past a checkpoint delay
        let (after, _) = read_doc(rt.dir()).ok_or("the Session_File is unreadable")?;
        if after != doc {
            return Err(format!("a window closed by the session end changed the file: {}", describe(&after)));
        }
        end_session(rt.engine(), true, lparam)?;
        // The process would end now: this is what the next start finds.
        let (last, bytes) = read_doc(rt.dir()).ok_or("the Session_File is unreadable")?;
        if last != doc {
            return Err(format!("WM_ENDSESSION changed the saved session: {}", describe(&last)));
        }
        Ok((doc, bytes))
    })();

    let next = result
        .as_ref()
        .ok()
        .map(|(doc, bytes)| next_start_from(bytes, name).and_then(|n| check_next_start(&n, doc)));
    let stopped = rt.stop();
    drop(rt);
    let cleanup = guard.finish();
    result.unwrap();
    next.unwrap().unwrap();
    stopped.unwrap();
    cleanup.unwrap();
}

#[test]
#[ignore = "opens File Explorer windows; run alone with --ignored --test-threads=1"]
fn sign_out_with_windows_open_saves_every_window() {
    end_session_case("sign_out", Strategies::DEFAULT, ENDSESSION_LOGOFF as isize, SaveCause::SignOut);
    report("sign_out_with_windows_open_saves_every_window", &Outcome::Ran);
}

#[test]
#[ignore = "opens File Explorer windows; run alone with --ignored --test-threads=1"]
fn shutdown_with_windows_open_saves_every_window() {
    let strategies = Strategies { end_kind: EndKindSource::AssumeShutdown, ..Strategies::DEFAULT };
    end_session_case("shutdown", strategies, 0, SaveCause::Shutdown);
    report("shutdown_with_windows_open_saves_every_window", &Outcome::Ran);
}

#[test]
#[ignore = "opens File Explorer windows; run alone with --ignored --test-threads=1"]
fn restart_with_windows_open_saves_every_window() {
    // Restart and shutdown send the same message; the kind comes from the
    // newest System-log event 1074, exactly as the runtime derives it.
    let kind = SessionEndListener::new(EndKindSource::Event1074).classify(0);
    let cause = cause_of(kind);
    if kind != EndKind::Restart {
        println!(
            "  note: no fresh 'restart' event 1074 in the System log (only a real restart \
             writes one), so the runtime derives {kind:?}; the test checks that derivation"
        );
    }
    end_session_case("restart", Strategies::DEFAULT, 0, cause);
    report("restart_with_windows_open_saves_every_window", &Outcome::Ran);
}

// ---------------------------------------------------------------------------
// Termination of a separate Explorer process (Req 3.3, 3.4)
// ---------------------------------------------------------------------------

/// An owned process handle.
struct Process(HANDLE);

impl Process {
    fn open(pid: u32) -> Result<Self, String> {
        // SAFETY: plain handle-returning call; closed in Drop.
        unsafe {
            OpenProcess(
                PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                false,
                pid,
            )
        }
        .map(Self)
        .map_err(|e| format!("OpenProcess({pid}): {e}"))
    }

    fn image(&self) -> Option<String> {
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        // SAFETY: the buffer is valid for `len` units.
        unsafe { QueryFullProcessImageNameW(self.0, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len) }
            .ok()?;
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }

    fn created(&self) -> Option<SystemTime> {
        let (mut c, mut e, mut k, mut u) =
            (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        // SAFETY: valid out pointers; the handle has query access.
        unsafe { GetProcessTimes(self.0, &mut c, &mut e, &mut k, &mut u) }.ok()?;
        // FILETIME: 100 ns since 1601-01-01; the Unix epoch is 11644473600 s later.
        let ticks = (u64::from(c.dwHighDateTime) << 32) | u64::from(c.dwLowDateTime);
        let unix_100ns = ticks.checked_sub(116_444_736_000_000_000)?;
        Some(UNIX_EPOCH + Duration::from_nanos(unix_100ns * 100))
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // SAFETY: the handle is owned.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn pid_of(hwnd: HWND) -> u32 {
    window_process_id(hwnd)
}

/// PIDs of the shell (desktop window and taskbar).
fn shell_pids() -> Vec<u32> {
    let mut pids = Vec::new();
    // SAFETY: plain reads.
    let shell = unsafe { GetShellWindow() };
    if !shell.is_invalid() {
        pids.push(pid_of(shell));
    }
    // SAFETY: plain window lookup.
    if let Ok(tray) = unsafe { FindWindowW(w!("Shell_TrayWnd"), PCWSTR::null()) }
        && !tray.is_invalid()
    {
        pids.push(pid_of(tray));
    }
    pids.retain(|&p| p != 0);
    pids
}

/// Top-level windows of `pid`: (HWND, visible, class).
fn top_level_windows_of(pid: u32) -> Vec<(isize, bool, String)> {
    struct Ctx {
        pid: u32,
        out: Vec<(isize, bool, String)>,
    }
    unsafe extern "system" fn cb(hwnd: HWND, data: LPARAM) -> BOOL {
        // SAFETY: `data` is the `&mut Ctx` passed below.
        let ctx = unsafe { &mut *(data.0 as *mut Ctx) };
        if window_process_id(hwnd) == ctx.pid {
            // SAFETY: plain read.
            let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
            ctx.out.push((raw(hwnd), visible, window_class_name(hwnd)));
        }
        true.into()
    }
    let mut ctx = Ctx { pid, out: Vec::new() };
    // SAFETY: the callback only runs during this call; `ctx` outlives it.
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut ctx as *mut Ctx as isize));
    }
    ctx.out
}

/// Every safety check before killing `pid`: it hosts `hwnd`, is
/// `explorer.exe`, is not the shell, was created after `launched_at`, and
/// has no top-level Explorer window or visible window other than `hwnd`.
/// `Err` is a reason to skip (nothing is killed).
fn verify_killable(proc: &Process, pid: u32, hwnd: HWND, launched_at: SystemTime) -> Result<(), String> {
    if pid == 0 || pid_of(hwnd) != pid {
        return Err("the test window is no longer hosted by the checked process".into());
    }
    if shell_pids().contains(&pid) {
        return Err(format!("the /separate window is hosted by the shell process {pid}"));
    }
    let image = proc.image().ok_or("cannot read the process image")?;
    if !image.to_lowercase().ends_with("\\explorer.exe") {
        return Err(format!("process {pid} is {image}, not explorer.exe"));
    }
    let created = proc.created().ok_or("cannot read the process creation time")?;
    if created < launched_at {
        return Err(format!(
            "process {pid} existed before the test opened its window (it may host other windows)"
        ));
    }
    let others: Vec<String> = top_level_windows_of(pid)
        .into_iter()
        .filter(|(h, visible, class)| {
            *h != raw(hwnd) && (*visible || is_cabinet_class(class))
        })
        .map(|(h, visible, class)| format!("{:?} {class} visible={visible}", hwnd_of(h)))
        .collect();
    if !others.is_empty() {
        return Err(format!("process {pid} also hosts {others:?}"));
    }
    Ok(())
}

#[test]
#[ignore = "opens a File Explorer window in its own process and kills only that process"]
fn separate_process_termination_saves_the_session() {
    const TEST: &str = "separate_process_termination_saves_the_session";
    let _sta = StaGuard::init().expect("STA init");
    let guard = ExplorerGuard::new("session_ends_separate");
    let folder = guard.dir("sp_a");
    let mut rt = Runtime::start("separate", Strategies::DEFAULT).expect("start the runtime");

    // Ok((outcome, saved doc and bytes, whether the file stays as is to the next start)).
    type Saved = Option<(SessionDoc, Vec<u8>, bool)>;
    let result = (|| -> Result<(Outcome, Saved), String> {
        let launched_at = SystemTime::now();
        request_open_separate(folder.clone())?;
        let hwnd = wait_for_window(&guard, &folder).ok_or("the /separate window did not open")?;
        let pid = pid_of(hwnd);
        let proc = Process::open(pid)?;
        if let Err(why) = verify_killable(&proc, pid, hwnd, launched_at) {
            return Ok((Outcome::Skipped(format!("not killing anything: {why}")), None));
        }
        println!(
            "  test window {hwnd:?} runs in its own explorer.exe (pid {pid}; shell pid(s) {:?})",
            shell_pids()
        );
        wait_doc(rt.dir(), Duration::from_secs(10), |d| lists(d, &folder))
            .ok_or("no checkpoint lists the test window")?;
        pump_for(Duration::from_millis(500));
        let others_open = !foreign_cabinets(&guard).is_empty();
        // Re-check right before the kill.
        if let Err(why) = verify_killable(&proc, pid, hwnd, launched_at) {
            return Ok((Outcome::Skipped(format!("not killing anything: {why}")), None));
        }
        // SAFETY: the verified handle of the test's own Explorer process.
        unsafe { TerminateProcess(proc.0, 1) }.map_err(|e| format!("TerminateProcess: {e}"))?;
        let killed_at = Instant::now();
        // SAFETY: valid handle with SYNCHRONIZE access.
        if unsafe { WaitForSingleObject(proc.0, 5_000) } != WAIT_OBJECT_0 {
            return Err("the killed process did not end".into());
        }
        println!("  killed pid {pid}");

        if !others_open {
            // Req 3.4: the termination is a Session_End.
            let (doc, bytes) =
                wait_doc(rt.dir(), Duration::from_secs(5), |d| d.cause != SaveCause::Checkpoint)
                    .ok_or("no Session_End write after the termination")?;
            let delay = killed_at.elapsed();
            println!("  Session_End write seen {delay:?} after the kill: {}", describe(&doc));
            check_end_doc(&doc, SaveCause::ExplorerTerminated)?;
            if find_window(&doc, &[&folder]).is_none() {
                return Err(format!("the killed window is missing: {}", describe(&doc)));
            }
            if delay > SESSION_END_WRITE_BOUND {
                return Err(format!("the Session_End write took {delay:?} (> 1 s)"));
            }
            Ok((Outcome::Ran, Some((doc, bytes, true))))
        } else {
            // Req 3.3: other windows are open, so no Session_End; the killed
            // window stays in the Live_Session. A (simulated) sign-out
            // capture shows the Live_Session.
            pump_for(Duration::from_millis(1_500));
            if let Some((d, _)) = read_doc(rt.dir())
                && d.cause == SaveCause::ExplorerTerminated
            {
                return Err("a Session_End was saved although other windows are open".into());
            }
            query_end_session(rt.engine(), ENDSESSION_LOGOFF as isize)?;
            let (doc, bytes) = read_doc(rt.dir()).ok_or("no readable Session_File")?;
            println!("  Live_Session capture (simulated sign-out): {}", describe(&doc));
            check_end_doc(&doc, SaveCause::SignOut)?;
            if find_window(&doc, &[&folder]).is_none() {
                return Err(format!("the killed window was dropped from the Live_Session: {}", describe(&doc)));
            }
            end_session(rt.engine(), false, ENDSESSION_LOGOFF as isize)?; // cancelled
            println!("  (other Explorer windows are open: checked Req 3.3, not the Req 3.4 Session_End)");
            Ok((Outcome::Ran, Some((doc, bytes, false))))
        }
    })();

    let stopped = rt.stop();
    let next = match &result {
        Ok((_, Some((doc, bytes, true)))) => Some(
            // The runtime exited with no window open: the file is unchanged.
            (if read_bytes(rt.dir()).as_ref() == Some(bytes) {
                Ok(())
            } else {
                Err("the Session_File changed after the termination save".to_string())
            })
            .and_then(|()| next_start(rt.dir()))
            .and_then(|n| check_next_start(&n, doc)),
        ),
        Ok((_, Some((doc, bytes, false)))) => {
            Some(next_start_from(bytes, "separate").and_then(|n| check_next_start(&n, doc)))
        }
        _ => None,
    };
    drop(rt);
    let cleanup = guard.finish();
    let (outcome, _) = result.unwrap();
    stopped.unwrap();
    if let Some(next) = next {
        next.unwrap();
    }
    cleanup.unwrap();
    report(TEST, &outcome);
}

// ---------------------------------------------------------------------------
// Destructive: taskkill of Explorer, Explorer restart (Req 3.3, 3.4)
// ---------------------------------------------------------------------------

fn destructive_allowed(test: &str) -> bool {
    if std::env::var(ENV_DESTRUCTIVE).as_deref() == Ok("1") {
        return true;
    }
    report(
        test,
        &Outcome::Skipped(format!(
            "kills the shell explorer.exe (closes every Explorer window and the taskbar); \
             set {ENV_DESTRUCTIVE}=1 to run"
        )),
    );
    false
}

fn shell_running() -> bool {
    // SAFETY: plain window lookup.
    unsafe { FindWindowW(w!("Shell_TrayWnd"), PCWSTR::null()) }.is_ok_and(|h| !h.is_invalid())
}

/// Waits for the shell to come back (Winlogon restarts it after a crash);
/// starts `explorer.exe` if it does not.
fn ensure_shell_running() {
    if wait(Duration::from_secs(15), || shell_running().then_some(())).is_some() {
        return;
    }
    let windir = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    let exe = Path::new(&windir).join("explorer.exe");
    println!("  the shell is not running: starting {}", exe.display());
    if let Err(e) = std::process::Command::new(&exe).spawn() {
        eprintln!("  could not start explorer.exe: {e}");
    }
    if wait(Duration::from_secs(30), || shell_running().then_some(())).is_none() {
        eprintln!("  WARNING: the shell is still not running; start explorer.exe manually");
    }
}

/// Restarts the shell if it is not running when dropped (also on failure).
struct ShellRestorer;

impl Drop for ShellRestorer {
    fn drop(&mut self) {
        ensure_shell_running();
    }
}

fn kill_all_explorer() -> Result<Instant, String> {
    let status = std::process::Command::new("taskkill")
        .args(["/F", "/IM", "explorer.exe"])
        .status()
        .map_err(|e| format!("taskkill: {e}"))?;
    let at = Instant::now();
    if !status.success() {
        return Err(format!("taskkill exited with {status}"));
    }
    Ok(at)
}

/// Waits for the `explorer-terminated` Session_End and checks it.
fn check_termination_save(rt: &Runtime, killed_at: Instant, folder: &Path) -> Result<(SessionDoc, Vec<u8>), String> {
    let (doc, bytes) = wait_doc(rt.dir(), Duration::from_secs(5), |d| d.cause != SaveCause::Checkpoint)
        .ok_or("no Session_End write after the termination")?;
    let delay = killed_at.elapsed();
    println!("  Session_End write seen {delay:?} after the kill: {}", describe(&doc));
    check_end_doc(&doc, SaveCause::ExplorerTerminated)?;
    if find_window(&doc, &[folder]).is_none() {
        return Err(format!("the killed window is missing: {}", describe(&doc)));
    }
    if delay > SESSION_END_WRITE_BOUND {
        return Err(format!("the Session_End write took {delay:?} (> 1 s)"));
    }
    Ok((doc, bytes))
}

#[test]
#[ignore = "DESTRUCTIVE: kills explorer.exe; also needs ESR_ALLOW_DESTRUCTIVE=1"]
fn taskkill_of_explorer_saves_the_session() {
    const TEST: &str = "taskkill_of_explorer_saves_the_session";
    if !destructive_allowed(TEST) {
        return;
    }
    let _sta = StaGuard::init().expect("STA init");
    let guard = ExplorerGuard::new("session_ends_taskkill");
    let _shell = ShellRestorer; // dropped before the guard cleans up
    let folder = guard.dir("tk_a");
    let mut rt = Runtime::start("taskkill", Strategies::DEFAULT).expect("start the runtime");

    let result = (|| -> Result<(SessionDoc, Vec<u8>), String> {
        open_window(&guard, &folder)?;
        wait_doc(rt.dir(), Duration::from_secs(10), |d| lists(d, &folder))
            .ok_or("no checkpoint lists the test window")?;
        pump_for(Duration::from_millis(500));
        let killed_at = kill_all_explorer()?;
        check_termination_save(&rt, killed_at, &folder)
    })();
    ensure_shell_running();

    let next = result
        .as_ref()
        .ok()
        .map(|(doc, bytes)| next_start_from(bytes, "taskkill").and_then(|n| check_next_start(&n, doc)));
    let stopped = rt.stop();
    drop(rt);
    drop(_shell);
    let cleanup = guard.finish();
    result.unwrap();
    next.unwrap().unwrap();
    stopped.unwrap();
    cleanup.unwrap();
    report(TEST, &Outcome::Ran);
}

#[test]
#[ignore = "DESTRUCTIVE: restarts explorer.exe; also needs ESR_ALLOW_DESTRUCTIVE=1"]
fn explorer_restart_saves_the_session_and_tracking_resumes() {
    const TEST: &str = "explorer_restart_saves_the_session_and_tracking_resumes";
    if !destructive_allowed(TEST) {
        return;
    }
    let _sta = StaGuard::init().expect("STA init");
    let guard = ExplorerGuard::new("session_ends_restart_shell");
    let _shell = ShellRestorer;
    let (a, b) = (guard.dir("rs_a"), guard.dir("rs_b"));
    let mut rt = Runtime::start("restart_shell", Strategies::DEFAULT).expect("start the runtime");

    let result = (|| -> Result<(SessionDoc, Vec<u8>), String> {
        open_window(&guard, &a)?;
        wait_doc(rt.dir(), Duration::from_secs(10), |d| lists(d, &a))
            .ok_or("no checkpoint lists the test window")?;
        pump_for(Duration::from_millis(500));
        // Restart: terminate Explorer, then bring the shell back (as Task
        // Manager's "Restart" does; Winlogon may restart it first).
        let killed_at = kill_all_explorer()?;
        let saved = check_termination_save(&rt, killed_at, &a)?;
        ensure_shell_running();
        if !shell_running() {
            return Err("the shell did not come back".into());
        }
        pump_for(Duration::from_secs(3));
        // Tracking resumes after the restart (Req 1.7, 1.10).
        let w = open_window(&guard, &b)?;
        wait_doc(rt.dir(), Duration::from_secs(15), |d| {
            d.cause == SaveCause::Checkpoint && lists(d, &b)
        })
        .ok_or("the runtime did not track a window opened after the restart")?;
        if !guard.close(w) {
            return Err("could not close the window opened after the restart".into());
        }
        Ok(saved)
    })();
    ensure_shell_running();

    let next = result
        .as_ref()
        .ok()
        .map(|(doc, bytes)| next_start_from(bytes, "restart_shell").and_then(|n| check_next_start(&n, doc)));
    let stopped = rt.stop();
    drop(rt);
    drop(_shell);
    let cleanup = guard.finish();
    result.unwrap();
    next.unwrap().unwrap();
    stopped.unwrap();
    cleanup.unwrap();
    report(TEST, &Outcome::Ran);
}
