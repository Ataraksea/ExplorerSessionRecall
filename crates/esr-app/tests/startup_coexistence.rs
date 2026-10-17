//! Integration tests for start at sign-in and coexistence with Windows'
//! built-in folder restore (task 16.6; Requirements 1.1, 1.9, 10.2, 10.4,
//! and along the way 10.1, 10.3, 12.4).
//!
//! Every test is `#[ignore]`. Run them alone:
//!
//! ```text
//! cargo test -p esr-app --test startup_coexistence -- --ignored --nocapture --test-threads=1
//! ```
//!
//! The three tests that change the real `PersistBrowsers` value (the
//! Builtin_Restore_Option) additionally need `ESR_ALLOW_DESTRUCTIVE=1`;
//! without it they print `SKIPPED` and pass.
//!
//! # How "sign-in" is simulated
//!
//! Signing out would end the test session, so no test signs in for real.
//! Instead each test installs a private copy of `esr.exe` and registers a
//! **real logon task** for it with the product's own
//! `esr_win::startup::StartupRegistration` (the code `--install` and the
//! Settings dialog use), under the test base name `ESRTest_<pid>_<tag>`
//! (task `\ESRTest_<pid>_<tag>\<SID>`; never `\ExplorerSessionRecall`).
//! "Sign-in" is `schtasks /Run` of that task: Task Scheduler starts the
//! task's action, `"<exe>" --background`, with the task's principal
//! (interactive token, no elevation) in the user's session, exactly as the
//! logon trigger would. The test measures from just before `schtasks /Run`.
//!
//! What this proves: the registered task starts the monitor in the user's
//! session with the logon command line; the time from "task started" to
//! "tracking" (≤ 10 s, Req 1.1); that a disabled registration is not run
//! (Req 1.9); and the Builtin_Restore_Option flows of a `--background`
//! start shortly after a "sign-in".
//!
//! What it does not prove: when Windows fires the logon trigger relative to
//! the desktop becoming available at a real sign-in (the task has a logon
//! trigger with no delay, which is asserted, and Task Scheduler starts
//! logon tasks as the session starts), contention at a real sign-in, that a
//! disabled task's *trigger* does not fire at a real sign-in (Task Scheduler
//! runs no trigger of a disabled task; the test shows the task is disabled,
//! keeps its trigger, refuses an on-demand run, and that no Run-key entry
//! exists), and that real `PersistBrowsers` logon restore opens windows the
//! way the test opens them (the test opens the saved folders with the
//! `opennewwindow` verb right after, or right before, the monitor starts,
//! without user input, which is what the BuiltinRestore heuristic sees at
//! sign-in).
//!
//! # Isolation and cleanup
//!
//! * The private copy lives in `%TEMP%\esr_startup_<tag>_<pid>\bin\esr.exe`.
//!   It is the test build of `esr.exe` (`CARGO_BIN_EXE_esr`) with its PE
//!   subsystem patched from console to GUI, because debug builds are console
//!   programs and a console window would appear (release builds are GUI
//!   programs already). Next to it, the test-only overrides file of
//!   `esr_app::dev_overrides` (read by debug builds only) points the data
//!   directory to `...\data` and the start-at-sign-in name to the test task,
//!   so the real data directory `%LOCALAPPDATA%\ExplorerSessionRecall` and
//!   the real registration are never touched. The tests refuse to run in
//!   release builds, which ignore the file.
//! * The copy takes the real single-instance mutex, as a real start does.
//!   Each test therefore first checks that no Explorer Session Recall
//!   instance runs and that the mutex is free, and fails otherwise.
//! * [`TestInstall`] stops every process running from the copy (`ESR_EXIT`,
//!   then `TerminateProcess` of a process whose image is the copy),
//!   unregisters the task, its folder and the Run value, and deletes the
//!   temp folder, in `finish` and again in `Drop` (also on panic).
//! * [`RegValueGuard`] records `PersistBrowsers` before it is set and puts
//!   it back (deleting it if it was absent) in `restore` and in `Drop`. The
//!   original value is printed first, so it can be restored by hand if the
//!   test process is killed.
//! * Explorer windows are opened through `ExplorerGuard` (serialised with
//!   every other Explorer-opening test), which closes each of them with
//!   `WM_SYSCOMMAND SC_CLOSE`.
//!
//! Helpers shared with `smoke.rs` (task 16.7) live in `tests/common`.

#![cfg(windows)]

#[path = "../../esr-win/tests/common/explorer_guard.rs"]
#[allow(dead_code)]
mod explorer_guard;
mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command as Process, Stdio};
use std::time::{Duration, Instant};

use common::{
    TaskState, instance_of, lists, make_gui_subsystem, normalize, open_window, query_task, read_doc, require_debug_build,
    require_no_running_instance, snapshot_folders, stop_instances_of, wait, window_alive, window_folders,
    window_pid,
};
use esr_app::dev_overrides::{self, DevOverrides};
use esr_app::install;
use esr_app::instance::{self, SecondaryAction};
use esr_app::ui::prompt::{CAPTION as PROMPT_CAPTION, ID_KEEP_ON, ID_TURN_OFF};
use esr_app::ui::settings::{BN_CLICKED, IDC_EXIT, IDC_START_AT_SIGN_IN, TITLE as SETTINGS_TITLE};
use esr_core::logfmt::ACTIVITY_LOG_FILE_NAME;
use esr_core::model::{SaveCause, Status};
use esr_core::settings::Settings;
use esr_win::explorer_options::{ADVANCED_SUBKEY, PERSIST_BROWSERS};
use esr_win::file_store::SETTINGS_FILE;
use esr_win::shell_util::{StaGuard, is_cabinet_window};
use esr_win::startup::{BACKGROUND_ARG, StartupConfig, StartupRegistration, quote_program_path};
use explorer_guard::{ExplorerGuard, pump_for, pump_once, top_level_cabinets};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, REG_DWORD, REG_ROUTINE_FLAGS, REG_VALUE_TYPE, RRF_NOEXPAND,
    RRF_RT_ANY, RegCloseKey, RegDeleteKeyValueW, RegEnumValueW, RegGetValueW, RegOpenKeyExW,
    RegQueryInfoKeyW, RegSetKeyValueW,
};
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::TaskScheduler::TASK_TRIGGER_LOGON;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::Win32::UI::WindowsAndMessaging::{
    BM_GETCHECK, BM_SETCHECK, EnumWindows, GetClassNameW, GetDlgItem, GetWindowRect, GetWindowTextW,
    IsWindowVisible, PostMessageW, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_COMMAND, WM_USER,
};
use windows::core::{BOOL, HSTRING, PWSTR};

/// Req 1.1: tracking within 10 s of the (simulated) sign-in.
const START_LIMIT: Duration = Duration::from_secs(10);
/// Req 10.2 (and 12.4): applied within 2 s.
const APPLY_LIMIT: Duration = Duration::from_secs(2);
/// `TDM_CLICK_BUTTON` (CommCtrl.h: `WM_USER + 102`).
const TDM_CLICK_BUTTON: u32 = WM_USER + 102;
/// `BST_UNCHECKED` / `BST_CHECKED`.
const BST_UNCHECKED: usize = 0;
const BST_CHECKED: isize = 1;

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn destructive_allowed(test: &str) -> bool {
    let allowed = std::env::var("ESR_ALLOW_DESTRUCTIVE").is_ok_and(|v| v.trim() == "1");
    if !allowed {
        println!("[startup] SKIPPED {test}: changes the real PersistBrowsers; set ESR_ALLOW_DESTRUCTIVE=1");
    }
    allowed
}

/// Milliseconds since the last user input (`GetLastInputInfo`).
fn last_input_age_ms() -> u32 {
    let mut info = LASTINPUTINFO { cbSize: size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    // SAFETY: valid, sized out structure; plain tick read.
    unsafe {
        if !GetLastInputInfo(&mut info).as_bool() {
            return u32::MAX;
        }
        GetTickCount().wrapping_sub(info.dwTime)
    }
}

fn window_text(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    // SAFETY: valid buffer.
    let n = unsafe { GetWindowTextW(hwnd, &mut buf) }.max(0) as usize;
    String::from_utf16_lossy(&buf[..n])
}

fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    // SAFETY: valid buffer.
    let n = unsafe { GetClassNameW(hwnd, &mut buf) }.max(0) as usize;
    String::from_utf16_lossy(&buf[..n])
}

fn window_rect(hwnd: HWND) -> Option<(i32, i32, i32, i32)> {
    let mut r = RECT::default();
    // SAFETY: valid out pointer.
    unsafe { GetWindowRect(hwnd, &mut r) }.ok()?;
    Some((r.left, r.top, r.right, r.bottom))
}

/// A visible top-level window.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Shown {
    hwnd: isize,
    class: String,
    title: String,
}

/// Visible top-level windows of process `pid`.
fn visible_windows_of(pid: u32) -> Vec<Shown> {
    unsafe extern "system" fn cb(hwnd: HWND, data: LPARAM) -> BOOL {
        // SAFETY: `data` is the `&mut Vec<isize>` passed below.
        let list = unsafe { &mut *(data.0 as *mut Vec<isize>) };
        // SAFETY: plain query.
        if unsafe { IsWindowVisible(hwnd) }.as_bool() {
            list.push(hwnd.0 as isize);
        }
        true.into()
    }
    let mut all: Vec<isize> = Vec::new();
    // SAFETY: the callback only runs during this call; `all` outlives it.
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut all as *mut _ as isize));
    }
    all.into_iter()
        .map(|raw| HWND(raw as *mut _))
        .filter(|&h| window_pid(h) == pid)
        .map(|h| Shown { hwnd: h.0 as isize, class: class_name(h), title: window_text(h) })
        .collect()
}

/// The visible dialog (`#32770`) of `pid` titled `title`.
fn dialog_of(pid: u32, title: &str) -> Option<HWND> {
    visible_windows_of(pid)
        .into_iter()
        .find(|w| w.class == "#32770" && w.title == title)
        .map(|w| HWND(w.hwnd as *mut _))
}

fn send(hwnd: HWND, msg: u32, wparam: usize, lparam: isize) -> Result<isize, String> {
    let mut result = 0usize;
    // SAFETY: plain cross-process message with scalar parameters.
    let ok = unsafe {
        SendMessageTimeoutW(
            hwnd,
            msg,
            WPARAM(wparam),
            LPARAM(lparam),
            SMTO_ABORTIFHUNG,
            5_000,
            Some(&mut result),
        )
    };
    if ok.0 == 0 {
        return Err(format!("SendMessageTimeoutW({msg:#x}) to {hwnd:?} failed or timed out"));
    }
    Ok(result as isize)
}

fn read_settings(data_dir: &Path) -> Option<Settings> {
    std::fs::read_to_string(data_dir.join(SETTINGS_FILE.target))
        .ok()
        .map(|t| Settings::parse_or_default(&t))
}

fn log_count(data_dir: &Path, needle: &str) -> usize {
    std::fs::read_to_string(data_dir.join(ACTIVITY_LOG_FILE_NAME))
        .map(|t| t.lines().filter(|l| l.contains(needle)).count())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Explorer windows
// ---------------------------------------------------------------------------

/// Top-level Explorer windows that are neither pre-existing nor in `known`.
fn other_cabinets(guard: &ExplorerGuard, known: &[HWND]) -> Vec<isize> {
    top_level_cabinets()
        .into_iter()
        .filter(|&raw| {
            let h = HWND(raw as *mut _);
            !guard.is_pre_existing(h) && !known.contains(&h) && is_cabinet_window(h)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// TestInstall
// ---------------------------------------------------------------------------

/// A private copy of `esr.exe` with its own data directory and logon task
/// (see the module documentation). Cleaned up in `finish` and in `Drop`.
struct TestInstall {
    tag: String,
    root: PathBuf,
    exe: PathBuf,
    data_dir: PathBuf,
    reg: StartupRegistration,
    done: bool,
}

impl TestInstall {
    fn new(tag: &str) -> Result<Self, String> {
        require_debug_build();
        let pid = std::process::id();
        let root = std::env::temp_dir().join(format!("esr_startup_{tag}_{pid}"));
        let _ = std::fs::remove_dir_all(&root);
        let bin = root.join("bin");
        let data_dir = root.join("data");
        std::fs::create_dir_all(&bin).map_err(|e| e.to_string())?;
        let exe = bin.join("esr.exe");
        std::fs::copy(env!("CARGO_BIN_EXE_esr"), &exe).map_err(|e| format!("copy esr.exe: {e}"))?;
        make_gui_subsystem(&exe)?;

        let base = format!("ESRTest_{pid}_{tag}");
        let overrides = format!("data_dir={}\nstartup_name={base}\n", data_dir.display());
        std::fs::write(exe.with_file_name(dev_overrides::FILE_NAME), &overrides).map_err(|e| e.to_string())?;
        // The copy must see exactly these overrides (never the defaults).
        let seen = DevOverrides::for_exe(&exe);
        if seen.data_dir.as_deref() != Some(data_dir.as_path()) || seen.startup_name.as_deref() != Some(&base) {
            return Err(format!("the overrides file is not read back as written: {seen:?}"));
        }
        let sid = instance::current_user_sid().map_err(|e| e.message())?;
        let config = StartupConfig::with_base_name(&base, &sid, &exe);
        assert_ne!(config.folder, esr_win::startup::APP_STARTUP_NAME);
        println!("[startup {tag}] private copy {} (data {})", exe.display(), data_dir.display());
        Ok(Self { tag: tag.into(), root, exe, data_dir, reg: StartupRegistration::new(config), done: false })
    }

    /// Registers start at sign-in for the copy, enabled or disabled, as
    /// `--install` does. Requires the logon task (not the Run-key fallback).
    fn register(&mut self, enabled: bool) -> Result<String, String> {
        let mechanism = self.reg.set_enabled(enabled).map_err(|e| e.message())?;
        let path = self
            .reg
            .task_path()
            .map(str::to_owned)
            .ok_or_else(|| format!("start at sign-in fell back to {mechanism:?}: {:?}", self.reg.last_task_error()))?;
        println!("[startup {}] registered logon task {path} (enabled={enabled})", self.tag);
        Ok(path)
    }

    fn task_path(&self) -> String {
        self.reg.task_path().map(str::to_owned).unwrap_or_else(|| self.reg.config().folder_task_path())
    }

    fn task(&self) -> Result<TaskState, String> {
        query_task(&self.task_path())?.ok_or_else(|| format!("task {} not found", self.task_path()))
    }

    /// "Sign-in": runs the logon task on demand. Returns the moment just
    /// before `schtasks /Run` started, and its exit code and output.
    fn run_task(&self) -> Result<(Instant, i32, String), String> {
        let schtasks = PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into()))
            .join("System32")
            .join("schtasks.exe");
        let path = self.task_path();
        let t0 = Instant::now();
        let out = Process::new(schtasks)
            .args(["/Run", "/TN", &path])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("schtasks /Run: {e}"))?;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
        let code = out.status.code().unwrap_or(-1);
        println!("[startup {}] schtasks /Run /TN {path}: exit {code} ({text}) after {:?}", self.tag, t0.elapsed());
        Ok((t0, code, text))
    }

    /// Starts the copy the way a user would (no arguments).
    fn start_manually(&self) -> Result<(), String> {
        Process::new(&self.exe)
            .current_dir(self.exe.parent().unwrap())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("start {}: {e}", self.exe.display()))
    }

    /// The running instance of the copy: its Engine window and pid.
    fn instance(&self) -> Option<(HWND, u32)> {
        instance_of(&self.exe)
    }

    fn wait_instance(&self, timeout: Duration) -> Result<(HWND, u32), String> {
        wait(timeout, || self.instance()).ok_or_else(|| format!("no instance of the copy within {timeout:?}"))
    }

    /// Posts `ESR_EXIT` to the copy's Engine window.
    fn post_exit(&self, engine: HWND) -> Result<(), String> {
        common::post_exit(engine)
    }

    /// Stops every running instance of the copy: `ESR_EXIT`, then
    /// `TerminateProcess` if it is still there after 10 s.
    fn stop_instances(&self) -> Vec<String> {
        stop_instances_of(&self.exe, &format!("startup {}", self.tag))
    }

    /// Stops the copy, unregisters the task and Run value, deletes the
    /// folder, and reports anything left behind.
    fn cleanup(&mut self) -> Vec<String> {
        self.done = true;
        let mut problems = self.stop_instances();
        if let Err(e) = self.reg.unregister() {
            problems.push(format!("unregister: {}", e.message()));
        }
        let config = self.reg.config().clone();
        for path in [config.folder_task_path(), config.root_task_path()] {
            match query_task(&path) {
                Ok(None) => {}
                Ok(Some(_)) => problems.push(format!("task {path} still exists")),
                Err(e) => problems.push(format!("query {path}: {e}")),
            }
        }
        if !matches!(self.reg.run_value(), Ok(None)) {
            problems.push(format!("Run value {} still exists", config.run_value_name));
        }
        for _ in 0..40 {
            if !self.root.exists() || std::fs::remove_dir_all(&self.root).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        if self.root.exists() {
            problems.push(format!("{} could not be deleted", self.root.display()));
        }
        if problems.is_empty() {
            println!("[startup {}] cleanup complete: task, Run value, and files removed", self.tag);
        }
        problems
    }

    fn finish(mut self) -> Result<(), String> {
        let problems = self.cleanup();
        if problems.is_empty() { Ok(()) } else { Err(problems.join("; ")) }
    }
}

impl Drop for TestInstall {
    fn drop(&mut self) {
        if !self.done {
            let problems = self.cleanup();
            if !problems.is_empty() {
                eprintln!("[startup {}] CLEANUP INCOMPLETE: {}", self.tag, problems.join("; "));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Registry (PersistBrowsers)
// ---------------------------------------------------------------------------

/// A raw registry value: type and bytes.
type RawValue = (u32, Vec<u8>);

fn read_raw(subkey: &str, name: &str) -> Result<Option<RawValue>, String> {
    let (subkey, name) = (HSTRING::from(subkey), HSTRING::from(name));
    let flags = REG_ROUTINE_FLAGS(RRF_RT_ANY.0 | RRF_NOEXPAND.0);
    let mut size = 0u32;
    // SAFETY: size query without a data buffer.
    let probe = unsafe { RegGetValueW(HKEY_CURRENT_USER, &subkey, &name, flags, None, None, Some(&mut size)) };
    if probe == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if probe != ERROR_SUCCESS {
        return Err(format!("RegGetValueW: {probe:?}"));
    }
    let mut data = vec![0u8; size as usize + 2];
    let mut len = data.len() as u32;
    let mut ty = REG_VALUE_TYPE::default();
    // SAFETY: `data` is valid for `len` bytes.
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            &subkey,
            &name,
            flags,
            Some(&mut ty),
            Some(data.as_mut_ptr().cast()),
            Some(&mut len),
        )
    };
    if r != ERROR_SUCCESS {
        return Err(format!("RegGetValueW: {r:?}"));
    }
    data.truncate(len as usize);
    Ok(Some((ty.0, data)))
}

fn write_raw(subkey: &str, name: &str, value: &RawValue) -> Result<(), String> {
    // SAFETY: the data pointer covers exactly `value.1.len()` bytes.
    let r = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            &HSTRING::from(subkey),
            &HSTRING::from(name),
            value.0,
            Some(value.1.as_ptr().cast()),
            value.1.len() as u32,
        )
    };
    if r == ERROR_SUCCESS { Ok(()) } else { Err(format!("RegSetKeyValueW: {r:?}")) }
}

fn delete_value(subkey: &str, name: &str) -> Result<(), String> {
    // SAFETY: plain registry call.
    let r = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, &HSTRING::from(subkey), &HSTRING::from(name)) };
    if r == ERROR_SUCCESS || r == ERROR_FILE_NOT_FOUND { Ok(()) } else { Err(format!("RegDeleteKeyValueW: {r:?}")) }
}

/// Every value of `HKCU\<subkey>` (name → type and bytes).
fn all_values(subkey: &str) -> Result<BTreeMap<String, RawValue>, String> {
    let mut key = HKEY::default();
    // SAFETY: valid strings and out pointers; the key is closed below.
    unsafe {
        let r = RegOpenKeyExW(HKEY_CURRENT_USER, &HSTRING::from(subkey), None, KEY_READ, &mut key);
        if r != ERROR_SUCCESS {
            return Err(format!("RegOpenKeyExW: {r:?}"));
        }
        let (mut count, mut max_name, mut max_data) = (0u32, 0u32, 0u32);
        let r = RegQueryInfoKeyW(
            key, None, None, None, None, None, None,
            Some(&mut count), Some(&mut max_name), Some(&mut max_data), None, None,
        );
        let mut out = BTreeMap::new();
        if r == ERROR_SUCCESS {
            for i in 0..count {
                let mut name = vec![0u16; max_name as usize + 1];
                let mut name_len = name.len() as u32;
                let mut data = vec![0u8; max_data as usize + 2];
                let mut data_len = data.len() as u32;
                let mut ty = 0u32;
                let r = RegEnumValueW(
                    key,
                    i,
                    Some(PWSTR(name.as_mut_ptr())),
                    &mut name_len,
                    None,
                    Some(&mut ty),
                    Some(data.as_mut_ptr()),
                    Some(&mut data_len),
                );
                if r == ERROR_SUCCESS {
                    data.truncate(data_len as usize);
                    out.insert(String::from_utf16_lossy(&name[..name_len as usize]), (ty, data));
                }
            }
        }
        let _ = RegCloseKey(key);
        if r != ERROR_SUCCESS {
            return Err(format!("RegQueryInfoKeyW: {r:?}"));
        }
        Ok(out)
    }
}

fn persist_browsers() -> Result<Option<RawValue>, String> {
    read_raw(ADVANCED_SUBKEY, PERSIST_BROWSERS)
}

fn dword(v: u32) -> RawValue {
    (REG_DWORD.0, v.to_le_bytes().to_vec())
}

/// Sets `PersistBrowsers` and puts the original back afterwards.
struct RegValueGuard {
    original: Option<RawValue>,
    restored: bool,
}

impl RegValueGuard {
    fn set_persist_browsers(on: bool) -> Result<Self, String> {
        let original = persist_browsers()?;
        println!(
            "[startup] original {ADVANCED_SUBKEY}\\{PERSIST_BROWSERS}: {} (restored after the test)",
            match &original {
                None => "absent".to_string(),
                Some((ty, data)) => format!("type {ty}, data {data:?}"),
            }
        );
        let guard = Self { original, restored: false };
        write_raw(ADVANCED_SUBKEY, PERSIST_BROWSERS, &dword(on.into()))?;
        Ok(guard)
    }

    fn restore(&mut self) -> Result<(), String> {
        self.restored = true;
        match &self.original {
            None => delete_value(ADVANCED_SUBKEY, PERSIST_BROWSERS)?,
            Some(v) => write_raw(ADVANCED_SUBKEY, PERSIST_BROWSERS, v)?,
        }
        let now = persist_browsers()?;
        if now != self.original {
            return Err(format!("PersistBrowsers is {now:?} after restoring {:?}", self.original));
        }
        println!("[startup] PersistBrowsers restored to its original state");
        Ok(())
    }
}

impl Drop for RegValueGuard {
    fn drop(&mut self) {
        if !self.restored
            && let Err(e) = self.restore()
        {
            eprintln!("[startup] COULD NOT RESTORE PersistBrowsers ({:?}): {e}", self.original);
        }
    }
}

// ---------------------------------------------------------------------------
// Req 1.1: start at sign-in through the logon task
// ---------------------------------------------------------------------------

/// The logon task starts the monitor (`--background`) in the user's session
/// and it tracks Explorer windows within 10 s, showing no window.
#[test]
#[ignore = "registers a test logon task, runs esr.exe from it, opens two File Explorer windows; run alone"]
fn logon_task_starts_tracking_within_10_seconds() {
    let _sta = StaGuard::init().expect("STA init");
    let guard = ExplorerGuard::new("startup_logon");
    let mut setup = TestInstall::new("logon").expect("test install");

    let result = (|| -> Result<(), String> {
        require_no_running_instance()?;
        let path = setup.register(true)?;

        // The registration is the logon task the product defines.
        let task = setup.task()?;
        println!("[startup logon] task: {task:?}");
        let quoted = quote_program_path(&setup.exe).map_err(|e| e.message())?;
        if !task.enabled
            || task.trigger_types != vec![TASK_TRIGGER_LOGON]
            || task.logon_delay.as_deref() != Some("")
            || task.exe != quoted
            || task.args != BACKGROUND_ARG
        {
            return Err(format!("unexpected task definition for {path}: {task:?}"));
        }

        // A folder window that is open when the user's session starts.
        let w1 = guard.dir("signin_open");
        let h1 = open_window(&guard, &w1)?;
        pump_for(Duration::from_millis(500));

        // "Sign-in".
        let (t0, code, text) = setup.run_task()?;
        if code != 0 {
            return Err(format!("schtasks /Run failed ({code}): {text}"));
        }
        let mut shown: Vec<Shown> = Vec::new();
        let (engine, pid) = wait(START_LIMIT + Duration::from_secs(5), || setup.instance()).ok_or_else(|| {
            format!("no instance of the copy {:?} after schtasks /Run", t0.elapsed())
        })?;
        let t_engine = t0.elapsed();
        let doc = wait(START_LIMIT + Duration::from_secs(10), || {
            shown.extend(visible_windows_of(pid));
            read_doc(&setup.data_dir).filter(|d| lists(d, &w1))
        })
        .ok_or("the open window was never recorded in the Session_File")?;
        let t_tracked = t0.elapsed();
        println!(
            "[startup logon] instance pid {pid}: Engine window after {t_engine:?}, open window \
             recorded ({:?}, {:?}) after {t_tracked:?} (Req 1.1: <= {START_LIMIT:?})",
            doc.status, doc.cause
        );
        if t_tracked > START_LIMIT {
            return Err(format!("tracking began {t_tracked:?} after the logon task ran (Req 1.1: <= 10 s)"));
        }
        if task.last_run == setup.task()?.last_run {
            return Err("Task Scheduler's LastRunTime did not change: the task did not start it".into());
        }

        // Tracking goes on: a window opened now is recorded too.
        let w2 = guard.dir("after_signin");
        let start = Instant::now();
        let _h2 = open_window(&guard, &w2)?;
        wait(Duration::from_secs(5), || {
            shown.extend(visible_windows_of(pid));
            read_doc(&setup.data_dir).filter(|d| lists(d, &w1) && lists(d, &w2))
        })
        .ok_or("a window opened after the start was not recorded")?;
        println!("[startup logon] window opened after the start recorded after {:?}", start.elapsed());

        // No window, dialog, or taskbar button. The hidden Engine window is
        // not visible. If the machine has PersistBrowsers on, the prompt of
        // Req 10.1 is the one allowed exception.
        let _ = wait(Duration::from_secs(1), || {
            shown.extend(visible_windows_of(pid));
            None::<()>
        });
        let pb_on = persist_browsers()?.is_some_and(|(_, d)| d.iter().any(|&b| b != 0));
        shown.retain(|w| !(pb_on && w.title == PROMPT_CAPTION));
        shown.dedup();
        if !shown.is_empty() {
            return Err(format!("the background start showed windows: {shown:?}"));
        }

        setup.post_exit(engine)?;
        if !install::wait_for_process_exit(pid, Duration::from_secs(5)) {
            return Err("the instance did not exit within 5 s of ESR_EXIT".into());
        }
        if !window_alive(h1) {
            return Err("the window open at sign-in was closed".into());
        }
        Ok(())
    })();

    let cleaned = setup.finish();
    let windows = guard.finish();
    result.unwrap();
    cleaned.unwrap();
    windows.unwrap();
}

// ---------------------------------------------------------------------------
// Req 1.9: no start when start at sign-in is disabled
// ---------------------------------------------------------------------------

/// Turning "Start when I sign in" off in Settings disables the logon task
/// (within 2 s, Req 12.4), and the next "sign-in" starts nothing.
#[test]
#[ignore = "registers a test logon task, runs esr.exe from it, shows Settings; run alone"]
fn disabled_start_at_sign_in_does_not_start() {
    let _sta = StaGuard::init().expect("STA init");
    let guard = ExplorerGuard::new("startup_disabled");
    let mut setup = TestInstall::new("disabled").expect("test install");

    let result = (|| -> Result<(), String> {
        require_no_running_instance()?;
        setup.register(true)?;

        // First sign-in with the option on (the default): it starts.
        let (_, code, text) = setup.run_task()?;
        if code != 0 {
            return Err(format!("schtasks /Run failed ({code}): {text}"));
        }
        let (engine, pid) = setup.wait_instance(START_LIMIT)?;

        // The user turns the option off in Settings.
        let outcome = instance::signal_window(engine, SecondaryAction::ShowSettings);
        println!("[startup disabled] ShowSettings: {outcome:?}");
        let dialog = wait(Duration::from_secs(3), || dialog_of(pid, SETTINGS_TITLE))
            .ok_or("Settings did not open")?;
        // SAFETY: a control of the copy's dialog; cross-process GetDlgItem.
        let checkbox = unsafe { GetDlgItem(Some(dialog), i32::from(IDC_START_AT_SIGN_IN)) }
            .map_err(|e| format!("start-at-sign-in checkbox: {e}"))?;
        if send(checkbox, BM_GETCHECK, 0, 0)? != BST_CHECKED {
            return Err("\"Start when I sign in\" is not checked by default".into());
        }
        let before = setup.task()?;
        send(checkbox, BM_SETCHECK, BST_UNCHECKED, 0)?;
        let start = Instant::now();
        let wparam = (usize::from(BN_CLICKED) << 16) | usize::from(IDC_START_AT_SIGN_IN);
        // SAFETY: the dialog's own BN_CLICKED notification.
        unsafe { PostMessageW(Some(dialog), WM_COMMAND, WPARAM(wparam), LPARAM(checkbox.0 as isize)) }
            .map_err(|e| format!("BN_CLICKED: {e}"))?;
        wait(APPLY_LIMIT, || setup.task().ok().filter(|t| !t.enabled))
            .ok_or("the logon task was not disabled within 2 s (Req 12.4)")?;
        println!("[startup disabled] logon task disabled {:?} after the click", start.elapsed());
        wait(APPLY_LIMIT, || read_settings(&setup.data_dir).filter(|s| !s.start_at_sign_in))
            .ok_or("start_at_sign_in=false was not saved")?;

        // Exit through Settings.
        // SAFETY: the dialog's Exit button notification.
        unsafe {
            let exit = GetDlgItem(Some(dialog), i32::from(IDC_EXIT)).unwrap_or_default();
            let wparam = (usize::from(BN_CLICKED) << 16) | usize::from(IDC_EXIT);
            PostMessageW(Some(dialog), WM_COMMAND, WPARAM(wparam), LPARAM(exit.0 as isize))
                .map_err(|e| format!("Exit: {e}"))?;
        }
        if !install::wait_for_process_exit(pid, Duration::from_secs(5)) {
            return Err("the instance did not exit within 5 s of Exit".into());
        }

        // The task is kept, disabled, with its logon trigger; no Run value.
        let task = setup.task()?;
        println!("[startup disabled] task after the change: {task:?}");
        if task.enabled || task.trigger_types != vec![TASK_TRIGGER_LOGON] || task.args != before.args {
            return Err(format!("unexpected disabled task: {task:?}"));
        }
        if !matches!(setup.reg.run_value(), Ok(None)) {
            return Err("a Run-key entry exists, which would start the app at sign-in".into());
        }

        // Next "sign-in": nothing starts.
        let (_, code, text) = setup.run_task()?;
        println!("[startup disabled] on-demand run of the disabled task: exit {code}: {text}");
        if let Some((_, pid)) = wait(Duration::from_secs(12), || setup.instance()) {
            return Err(format!("the disabled registration started an instance (pid {pid})"));
        }
        let after = setup.task()?;
        if after.last_run != task.last_run {
            return Err(format!("Task Scheduler ran the disabled task ({} -> {})", task.last_run, after.last_run));
        }
        println!("[startup disabled] no instance within 12 s; LastRunTime unchanged");
        Ok(())
    })();

    let cleaned = setup.finish();
    let windows = guard.finish();
    result.unwrap();
    cleaned.unwrap();
    windows.unwrap();
}

// ---------------------------------------------------------------------------
// Req 10.2 (and 10.1): accepting the prompt
// ---------------------------------------------------------------------------

/// With PersistBrowsers on and no decision recorded, the logon start shows
/// the prompt (Req 10.1); accepting turns PersistBrowsers off within 2 s and
/// changes no other File Explorer option (Req 10.2).
#[test]
#[ignore = "changes PersistBrowsers (restored); needs ESR_ALLOW_DESTRUCTIVE=1; run alone"]
fn accepting_the_prompt_turns_off_only_builtin_restore() {
    if !destructive_allowed("accepting_the_prompt_turns_off_only_builtin_restore") {
        return;
    }
    let _sta = StaGuard::init().expect("STA init");
    let guard = ExplorerGuard::new("startup_accept");
    let mut setup = TestInstall::new("accept").expect("test install");
    let mut pb: Option<RegValueGuard> = None;

    let result = (|| -> Result<(), String> {
        require_no_running_instance()?;
        setup.register(true)?;
        pb = Some(RegValueGuard::set_persist_browsers(true)?);
        let before = all_values(ADVANCED_SUBKEY)?;

        let (t0, code, text) = setup.run_task()?;
        if code != 0 {
            return Err(format!("schtasks /Run failed ({code}): {text}"));
        }
        let (engine, pid) = setup.wait_instance(START_LIMIT)?;
        let prompt = wait(START_LIMIT.saturating_sub(t0.elapsed()), || dialog_of(pid, PROMPT_CAPTION))
            .ok_or("no prompt within 10 s of the start (Req 10.1)")?;
        println!("[startup accept] prompt shown {:?} after the logon task ran", t0.elapsed());
        pump_for(Duration::from_millis(500));
        let prompts = visible_windows_of(pid).into_iter().filter(|w| w.title == PROMPT_CAPTION).count();
        if prompts != 1 {
            return Err(format!("{prompts} prompts are shown (Req 10.1: exactly one)"));
        }

        let start = Instant::now();
        send(prompt, TDM_CLICK_BUTTON, ID_TURN_OFF as usize, 0)?;
        wait(APPLY_LIMIT, || persist_browsers().ok().flatten().filter(|v| *v == dword(0)))
            .ok_or_else(|| format!("PersistBrowsers is {:?} 2 s after accepting (Req 10.2)", persist_browsers()))?;
        println!("[startup accept] PersistBrowsers = 0 {:?} after accepting", start.elapsed());

        let after = all_values(ADVANCED_SUBKEY)?;
        let changed: Vec<&String> = before
            .keys()
            .chain(after.keys())
            .filter(|k| k.as_str() != PERSIST_BROWSERS && before.get(*k) != after.get(*k))
            .collect();
        if !changed.is_empty() {
            return Err(format!("other File Explorer options changed: {changed:?}"));
        }
        println!("[startup accept] {} other Advanced values unchanged", after.len().saturating_sub(1));

        // No error message, no second prompt; nothing recorded as declined.
        pump_for(Duration::from_secs(1));
        let shown = visible_windows_of(pid);
        if !shown.is_empty() {
            return Err(format!("windows shown after accepting: {shown:?}"));
        }
        if read_settings(&setup.data_dir).is_some_and(|s| s.builtin_prompt_declined) {
            return Err("accepting recorded a declined decision".into());
        }
        setup.post_exit(engine)?;
        if !install::wait_for_process_exit(pid, Duration::from_secs(5)) {
            return Err("the instance did not exit within 5 s".into());
        }
        Ok(())
    })();

    let cleaned = setup.finish();
    let restored = pb.as_mut().map_or(Ok(()), RegValueGuard::restore);
    let windows = guard.finish();
    result.unwrap();
    cleaned.unwrap();
    restored.unwrap();
    windows.unwrap();
}

// ---------------------------------------------------------------------------
// Req 10.4 (and 10.3): built-in restore consumes the snapshot
// ---------------------------------------------------------------------------

/// When Windows' own logon restore opens the folder windows, relative to
/// the monitor's start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RestoreOrder {
    /// The windows open while the monitor is already running.
    AfterStart,
    /// The windows are already open when the monitor starts.
    BeforeStart,
}

fn builtin_restore_case(tag: &str, order: RestoreOrder) {
    let _sta = StaGuard::init().expect("STA init");
    let guard = ExplorerGuard::new(tag);
    if !top_level_cabinets().is_empty() {
        println!("[{tag}] SKIPPED: other File Explorer windows are open (the restore trigger needs none)");
        return;
    }
    let mut setup = TestInstall::new(tag).expect("test install");
    let mut pb: Option<RegValueGuard> = None;
    let a = guard.dir("restore_a");
    let b = guard.dir("restore_b");

    let result = (|| -> Result<(), String> {
        require_no_running_instance()?;
        setup.register(true)?;
        pb = Some(RegValueGuard::set_persist_browsers(true)?);

        // ---- Earlier session: the user starts the app, declines the
        // prompt, works with two windows, and exits through Settings.
        setup.start_manually()?;
        let (engine1, pid1) = setup.wait_instance(START_LIMIT)?;
        let prompt = wait(START_LIMIT, || dialog_of(pid1, PROMPT_CAPTION)).ok_or("no prompt at the manual start")?;
        send(prompt, TDM_CLICK_BUTTON, ID_KEEP_ON as usize, 0)?;
        wait(APPLY_LIMIT, || read_settings(&setup.data_dir).filter(|s| s.builtin_prompt_declined))
            .ok_or("the declined decision was not recorded (Req 10.3)")?;
        if persist_browsers()? != Some(dword(1)) {
            return Err("declining changed PersistBrowsers".into());
        }
        let ha = open_window(&guard, &a)?;
        let hb = open_window(&guard, &b)?;
        wait(Duration::from_secs(10), || read_doc(&setup.data_dir).filter(|d| lists(d, &a) && lists(d, &b)))
            .ok_or("no checkpoint lists both windows")?;
        setup.post_exit(engine1)?;
        if !install::wait_for_process_exit(pid1, Duration::from_secs(5)) {
            return Err("the first instance did not exit".into());
        }
        let saved = read_doc(&setup.data_dir).ok_or("no Session_File after exit")?;
        if saved.status != Status::Pending || saved.cause != SaveCause::ExitThroughSettings || !lists(&saved, &a) || !lists(&saved, &b) {
            return Err(format!("unexpected saved session: {:?} {:?} {:?}", saved.status, saved.cause, snapshot_folders(&saved.snapshot)));
        }
        println!("[{tag}] pending snapshot saved: {:?}", snapshot_folders(&saved.snapshot));
        for h in [ha, hb] {
            if !guard.close(h) {
                return Err(format!("could not close {h:?}"));
            }
        }
        if wait(Duration::from_secs(5), || top_level_cabinets().is_empty().then_some(())).is_none() {
            return Err("Explorer windows remain open before the sign-in".into());
        }
        let detected_before = log_count(&setup.data_dir, "BUILTIN-RESTORE-DETECTED");

        // ---- "Sign-in" with PersistBrowsers on: Windows reopens A and B.
        println!("[{tag}] last user input {} ms ago", last_input_age_ms());
        let mut restored_by_windows = Vec::new();
        if order == RestoreOrder::BeforeStart {
            restored_by_windows.push(open_window(&guard, &a)?);
            restored_by_windows.push(open_window(&guard, &b)?);
        }
        let (t0, code, text) = setup.run_task()?;
        if code != 0 {
            return Err(format!("schtasks /Run failed ({code}): {text}"));
        }
        let (engine2, pid2) = setup.wait_instance(START_LIMIT)?;
        println!("[{tag}] instance {pid2} up {:?} after the logon task ran", t0.elapsed());
        if order == RestoreOrder::AfterStart {
            restored_by_windows.push(open_window(&guard, &a)?);
            restored_by_windows.push(open_window(&guard, &b)?);
        }

        // Watch: the snapshot is consumed, nothing else opens, nothing changes.
        let mut consumed = false;
        let mut attempted: Option<u8> = None;
        let mut extra: Vec<isize> = Vec::new();
        let mut shown: Vec<Shown> = Vec::new();
        /// Per restored window: its rect and its tab folders.
        type WindowState = (Option<(i32, i32, i32, i32)>, Option<Vec<String>>);
        let mut baseline: Option<Vec<WindowState>> = None;
        let watch_start = Instant::now();
        while watch_start.elapsed() < Duration::from_secs(10) {
            pump_once();
            if let Some(doc) = read_doc(&setup.data_dir) {
                if doc.status == Status::Consumed && doc.snapshot == saved.snapshot {
                    if !consumed {
                        println!("[{tag}] snapshot consumed {:?} after the start", t0.elapsed());
                    }
                    consumed = true;
                }
                if doc.failed_attempts > 0 {
                    attempted = Some(doc.failed_attempts);
                }
            }
            for raw in other_cabinets(&guard, &restored_by_windows) {
                if !extra.contains(&raw) {
                    extra.push(raw);
                    guard.adopt(HWND(raw as *mut _));
                }
            }
            for w in visible_windows_of(pid2) {
                if !shown.contains(&w) {
                    shown.push(w);
                }
            }
            if baseline.is_none() && watch_start.elapsed() >= Duration::from_millis(1500) {
                baseline = Some(restored_by_windows.iter().map(|&h| (window_rect(h), window_folders(h))).collect());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let detected = log_count(&setup.data_dir, "BUILTIN-RESTORE-DETECTED") - detected_before;
        let after: Vec<_> = restored_by_windows.iter().map(|&h| (window_rect(h), window_folders(h))).collect();
        println!(
            "[{tag}] Activity_Log:\n{}",
            std::fs::read_to_string(setup.data_dir.join(ACTIVITY_LOG_FILE_NAME)).unwrap_or_default().trim_end()
        );
        if let Some(doc) = read_doc(&setup.data_dir) {
            println!(
                "[{tag}] Session_File now: {:?} {:?} attempts={} {:?}",
                doc.status,
                doc.cause,
                doc.failed_attempts,
                snapshot_folders(&doc.snapshot)
            );
        }
        println!(
            "[{tag}] consumed={consumed} detections logged={detected} restore attempt={attempted:?} \
             extra windows={extra:?} shown={shown:?} last input {} ms ago",
            last_input_age_ms()
        );
        if !extra.is_empty() {
            return Err(format!("{} additional Explorer window(s) opened (duplicates)", extra.len()));
        }
        if let Some(n) = attempted {
            return Err(format!("a restoration was attempted (failed_attempts={n})"));
        }
        if !shown.is_empty() {
            return Err(format!("windows shown at the start (the declined prompt must not return, Req 10.3): {shown:?}"));
        }
        let expected = [vec![normalize(&a.to_string_lossy())], vec![normalize(&b.to_string_lossy())]];
        for (i, (rect, folders)) in after.iter().enumerate() {
            if folders.as_ref() != Some(&expected[i]) {
                return Err(format!("window {i} tabs changed: {folders:?} (expected {:?})", expected[i]));
            }
            if baseline.as_ref().map(|b| &b[i]) != Some(&(*rect, folders.clone())) {
                return Err(format!("window {i} changed: {:?} -> {rect:?}", baseline.as_ref().map(|b| &b[i])));
            }
        }
        if !consumed || detected != 1 {
            return Err(format!(
                "the Pending_Snapshot was not consumed as a built-in restore (consumed={consumed}, \
                 BUILTIN-RESTORE-DETECTED entries={detected}); last input {} ms ago",
                last_input_age_ms()
            ));
        }
        setup.post_exit(engine2)?;
        if !install::wait_for_process_exit(pid2, Duration::from_secs(5)) {
            return Err("the instance did not exit".into());
        }
        Ok(())
    })();

    let cleaned = setup.finish();
    let restored = pb.as_mut().map_or(Ok(()), RegValueGuard::restore);
    let windows = guard.finish();
    result.unwrap();
    cleaned.unwrap();
    restored.unwrap();
    windows.unwrap();
}

/// Windows reopens the saved windows after the monitor started.
#[test]
#[ignore = "changes PersistBrowsers (restored); needs ESR_ALLOW_DESTRUCTIVE=1; opens File Explorer windows; run alone"]
fn builtin_restore_after_start_consumes_without_duplicates() {
    if destructive_allowed("builtin_restore_after_start_consumes_without_duplicates") {
        builtin_restore_case("builtin_after", RestoreOrder::AfterStart);
    }
}

/// Windows reopened the saved windows before the monitor started.
#[test]
#[ignore = "changes PersistBrowsers (restored); needs ESR_ALLOW_DESTRUCTIVE=1; opens File Explorer windows; run alone"]
fn builtin_restore_before_start_consumes_without_duplicates() {
    if destructive_allowed("builtin_restore_before_start_consumes_without_duplicates") {
        builtin_restore_case("builtin_before", RestoreOrder::BeforeStart);
    }
}
