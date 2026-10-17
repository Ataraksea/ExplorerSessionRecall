//! Integration tests for process behaviour (task 16.5; Requirements 1.2,
//! 1.4, 2.5, 12.5, 12.6, 12.7). All tests are `#[ignore]`: they run the
//! runtime in-process against the live Explorer session. Run them alone:
//!
//! ```text
//! cargo test -p esr-app --test process_behaviour -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Safety:
//!
//! * The runtime runs on a thread of the test process with a data directory
//!   under the `ExplorerGuard` temp root, no panic guard, and no Session_File
//!   at start (so it never restores anything). Every message goes to the
//!   `ESR.Engine` window of *this* process (found by PID), never to a real
//!   instance.
//! * Single instance uses a unique test mutex, never
//!   `Local\ExplorerSessionRecall.<SID>`.
//! * Every Explorer window is opened through `ExplorerGuard`, which closes it
//!   again even on failure, and every test holds the guard's mutex.
//! * Uninstall runs the real install and uninstall *plans* against temp roots
//!   and a throwaway `HKCU\Software\ESRTest_<pid>_process` key. Start-at-sign-in
//!   registration and unregistration are mocked (no Task Scheduler task, no
//!   Run value), and so are the two process launches (`StartInstalled` starts
//!   the in-process runtime instead of `esr.exe --background`; the hand-off
//!   copy is not started, its finish plan runs in-process).

#[path = "../../esr-win/tests/common/explorer_guard.rs"]
#[allow(dead_code)]
mod explorer_guard;

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use esr_app::install::{self, InstallInputs, Layout, Step};
use esr_app::instance::{
    self, ENGINE_WINDOW_CLASS, InstanceMessages, InstanceRole, SecondaryAction, SignalOutcome,
};
use esr_app::runtime::{RuntimeOptions, Storage, run, startup_input};
use esr_app::ui::settings::{BN_CLICKED, IDC_EXIT, TITLE as SETTINGS_TITLE};
use esr_app::version;
use esr_core::format::parse;
use esr_core::model::{SaveCause, SessionDoc};
use esr_core::settings::Settings;
use esr_win::shell_util::{
    StaGuard, current_folder_pidl, enumerate_all_explorer_windows, is_cabinet_window,
};
use explorer_guard::{ExplorerGuard, parsing_name, pump_for, pump_once, top_level_cabinets};
use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, RegCloseKey, RegOpenKeyExW,
};
use windows::Win32::UI::Shell::{
    ILCreateFromPathW, ILFree, SEE_MASK_FLAG_NO_UI, SEE_MASK_IDLIST, SEE_MASK_NOASYNC,
    SHELLEXECUTEINFOW, ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowExW, GetDlgItem, GetShellWindow, GetWindowRect, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, PostMessageW, SW_MINIMIZE,
    SW_SHOWNOACTIVATE, ShowWindow, WM_COMMAND,
};
use windows::core::{BOOL, HSTRING, PCWSTR, w};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn normalize(p: &str) -> String {
    p.trim_end_matches('\\').to_lowercase()
}

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
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Opens a new Explorer window on `dir` (from a launcher STA thread).
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

/// Parsing names of the tabs of the Explorer window `hwnd`, if it is in
/// ShellWindows.
fn window_folders(hwnd: HWND) -> Option<Vec<String>> {
    enumerate_all_explorer_windows()
        .ok()?
        .into_iter()
        .find(|g| g.top_hwnd == hwnd)
        .map(|g| {
            g.tabs
                .iter()
                .map(|t| {
                    current_folder_pidl(&t.browser)
                        .ok()
                        .and_then(|p| parsing_name(&p))
                        .map(|n| normalize(&n))
                        .unwrap_or_default()
                })
                .collect()
        })
}

/// The new window showing `dir`, adopted by the guard as soon as it appears.
fn wait_for_window(guard: &ExplorerGuard, dir: &Path, timeout: Duration) -> Option<HWND> {
    let target = normalize(&dir.to_string_lossy());
    wait(timeout, || {
        enumerate_all_explorer_windows()
            .unwrap_or_default()
            .into_iter()
            .filter(|g| !guard.is_pre_existing(g.top_hwnd))
            .find(|g| {
                g.tabs.iter().any(|t| {
                    current_folder_pidl(&t.browser)
                        .ok()
                        .and_then(|p| parsing_name(&p))
                        .is_some_and(|n| normalize(&n) == target)
                })
            })
            .map(|g| {
                guard.adopt(g.top_hwnd);
                g.top_hwnd
            })
    })
}

fn window_pid(hwnd: HWND) -> u32 {
    let mut pid = 0;
    // SAFETY: valid out pointer; a stale HWND yields 0.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid
}

/// PID of the process hosting the desktop (the main Explorer process).
fn shell_pid() -> u32 {
    // SAFETY: plain read.
    let shell = unsafe { GetShellWindow() };
    if shell.is_invalid() { 0 } else { window_pid(shell) }
}

fn alive(hwnd: HWND) -> bool {
    // SAFETY: IsWindow tolerates stale HWNDs.
    unsafe { IsWindow(Some(hwnd)) }.as_bool() && is_cabinet_window(hwnd)
}

fn window_rect(hwnd: HWND) -> Option<(i32, i32, i32, i32)> {
    let mut r = RECT::default();
    // SAFETY: valid out pointer.
    unsafe { GetWindowRect(hwnd, &mut r) }.ok()?;
    Some((r.left, r.top, r.right, r.bottom))
}

/// The `ESR.Engine` window created by this process.
fn own_engine_window() -> Option<HWND> {
    let class = HSTRING::from(ENGINE_WINDOW_CLASS);
    let mut after: Option<HWND> = None;
    loop {
        // SAFETY: plain window enumeration by class.
        let hwnd = unsafe { FindWindowExW(None, after, &class, PCWSTR::null()) }.ok()?;
        if hwnd.is_invalid() {
            return None;
        }
        if window_pid(hwnd) == std::process::id() {
            return Some(hwnd);
        }
        after = Some(hwnd);
    }
}

/// Visible Settings dialogs of this process.
fn settings_dialogs() -> Vec<HWND> {
    unsafe extern "system" fn cb(hwnd: HWND, data: LPARAM) -> BOOL {
        // SAFETY: `data` is the `&mut Vec<isize>` passed below.
        let list = unsafe { &mut *(data.0 as *mut Vec<isize>) };
        if window_pid(hwnd) == std::process::id() && unsafe { IsWindowVisible(hwnd) }.as_bool() {
            let mut buf = [0u16; 128];
            // SAFETY: valid buffer; the window belongs to this process.
            let n = unsafe { GetWindowTextW(hwnd, &mut buf) };
            if n > 0 && String::from_utf16_lossy(&buf[..n as usize]) == SETTINGS_TITLE {
                list.push(hwnd.0 as isize);
            }
        }
        true.into()
    }
    let mut list: Vec<isize> = Vec::new();
    // SAFETY: the callback only runs during this call; `list` outlives it.
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut list as *mut _ as isize));
    }
    list.into_iter().map(|raw| HWND(raw as *mut _)).collect()
}

fn read_doc(dir: &Path) -> Option<SessionDoc> {
    parse(&std::fs::read(dir.join("session.esr")).ok()?).ok()
}

fn lists(doc: &SessionDoc, folder: &Path) -> bool {
    let target = normalize(&folder.to_string_lossy());
    doc.snapshot
        .windows
        .iter()
        .flat_map(|w| &w.tabs)
        .any(|t| normalize(&t.location.parsing_name.to_string_lossy()) == target)
}

fn unique_mutex_name(tag: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!(r"Local\ESR.Test.ProcessBehaviour.{tag}.{}.{nanos}", std::process::id())
}

fn reg_key_exists(subkey: &str) -> bool {
    let mut key = HKEY::default();
    // SAFETY: valid strings and out pointer; the key is closed when opened.
    unsafe {
        let r = RegOpenKeyExW(HKEY_CURRENT_USER, &HSTRING::from(subkey), None, KEY_READ, &mut key);
        if r.is_ok() {
            let _ = RegCloseKey(key);
            true
        } else {
            false
        }
    }
}

/// A second instance: acquire the (test) mutex, then signal the Engine window
/// `engine` like `main` does. Returns the action, the outcome, and how long
/// the second instance took.
fn second_instance(
    mutex: &str,
    args: &[&str],
    engine: HWND,
) -> Result<(SecondaryAction, SignalOutcome, Duration), String> {
    let start = Instant::now();
    match instance::acquire_named(mutex, args.iter().copied()).map_err(|e| e.message())? {
        InstanceRole::Primary(_) => Err("the second acquisition became Primary".into()),
        InstanceRole::Secondary { action } => {
            let outcome = instance::signal_window(engine, action);
            Ok((action, outcome, start.elapsed()))
        }
    }
}

/// The runtime on its own thread of this process. Dropping it stops it
/// (`ESR_EXIT` to its own Engine window) if it still runs.
struct TestRuntime {
    join: Option<JoinHandle<i32>>,
    engine: Option<isize>,
}

impl TestRuntime {
    fn start(data_dir: &Path, ui: bool, background: bool) -> Self {
        assert!(own_engine_window().is_none(), "an ESR.Engine window of this process already exists");
        let dir = data_dir.to_path_buf();
        let join = std::thread::Builder::new()
            .name("esr-engine-test".into())
            .spawn(move || {
                let storage = Storage::open(&dir);
                let input = startup_input(storage.read_session(), background);
                // A declined prompt: the Builtin_Restore_Option prompt (which
                // could change the real PersistBrowsers) is never shown.
                let settings = Settings { builtin_prompt_declined: true, ..storage.settings() };
                let options = RuntimeOptions { ui, panic_guard: false, ..RuntimeOptions::default() };
                run(settings, input, storage, options)
            })
            .expect("spawn the runtime");
        let engine = wait(Duration::from_secs(10), own_engine_window).map(|h| h.0 as isize);
        Self { join: Some(join), engine }
    }

    fn engine(&self) -> Result<HWND, String> {
        self.engine
            .map(|raw| HWND(raw as *mut _))
            .ok_or_else(|| "no ESR.Engine window".to_string())
    }

    fn is_finished(&self) -> bool {
        self.join.as_ref().is_none_or(|j| j.is_finished())
    }

    /// Posts `ESR_EXIT` (as `--uninstall` sends it; handled like Exit in
    /// Settings) to this process's Engine window.
    fn post_exit(&self) -> Result<(), String> {
        let engine = self.engine()?;
        let exit = InstanceMessages::register().exit;
        // SAFETY: posting a registered message to this process's own window.
        unsafe { PostMessageW(Some(engine), exit, WPARAM(0), LPARAM(0)) }
            .map_err(|e| format!("PostMessageW(ESR_EXIT): {e}"))
    }

    /// Time until the runtime thread returned, if within `timeout`.
    fn wait_finished(&self, timeout: Duration) -> Option<Duration> {
        let start = Instant::now();
        wait(timeout, || self.is_finished().then(|| start.elapsed()))
    }

    /// Stops the runtime if needed and returns its exit code.
    fn stop(&mut self) -> Option<i32> {
        let join = self.join.take()?;
        if !join.is_finished() {
            let _ = self.post_exit();
            let start = Instant::now();
            wait(Duration::from_secs(10), || join.is_finished().then_some(()));
            if !join.is_finished() {
                eprintln!("[process] runtime still running {:?} after ESR_EXIT", start.elapsed());
                return None;
            }
        }
        join.join().ok()
    }
}

impl Drop for TestRuntime {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------
// Req 1.2: the test machine's build passes the gate
// ---------------------------------------------------------------------------

/// The pure gate is table-tested in `version_gate.rs`; this checks the gate
/// on the build the integration tests run on (they need a supported build).
#[test]
#[ignore = "integration: run on the Windows 11 test VMs"]
fn this_build_passes_the_version_gate() {
    let v = version::read_os_version().expect("read the OS version");
    println!("[process] OS {}.{}.{}.{:?}", v.major, v.minor, v.build, v.ubr);
    assert!(v.is_supported(), "the test machine must run a supported build: {v:?}");
}

// ---------------------------------------------------------------------------
// Req 1.4, 12.7 (and 12.5 through the Settings Exit button)
// ---------------------------------------------------------------------------

#[test]
#[ignore = "runs the runtime and shows the Settings dialog; run alone with --test-threads=1"]
fn second_instance_signals_and_relaunch_shows_settings() {
    let _sta = StaGuard::init().expect("STA init");
    let guard = ExplorerGuard::new("process_single_instance");
    let data_dir = guard.root().join("esr-data");
    let mutex = unique_mutex_name("single");
    let me = std::process::id();

    // The first instance holds the (test) mutex for its whole lifetime.
    let first = instance::acquire_named(&mutex, ["esr.exe", "--background"]).expect("test mutex");
    assert!(matches!(first, InstanceRole::Primary(_)), "{first:?}");
    let mut runtime = TestRuntime::start(&data_dir, true, true);

    let result = (|| -> Result<(), String> {
        let engine = runtime.engine()?;
        pump_for(Duration::from_millis(500));
        if !settings_dialogs().is_empty() {
            return Err("Settings is shown before any relaunch".into());
        }

        // Logon-task start (`--background`): Ping, no Settings (Req 1.4).
        let (action, outcome, took) = second_instance(&mutex, &["esr.exe", "--background"], engine)?;
        println!("[process] background second instance: {action:?} {outcome:?} in {took:?}");
        if action != SecondaryAction::Ping || outcome != (SignalOutcome::Delivered { pid: me }) {
            return Err(format!("background relaunch: {action:?} {outcome:?}"));
        }
        if took >= Duration::from_secs(2) {
            return Err(format!("the second instance took {took:?} (Req 1.4: < 2 s)"));
        }
        pump_for(Duration::from_millis(700));
        if !settings_dialogs().is_empty() {
            return Err("a background relaunch showed Settings".into());
        }
        if runtime.is_finished() {
            return Err("the first instance stopped after a ping".into());
        }

        // User relaunch: Settings within 2 s (Req 12.7).
        let start = Instant::now();
        let (action, outcome, took) = second_instance(&mutex, &["esr.exe"], engine)?;
        println!("[process] user second instance: {action:?} {outcome:?} in {took:?}");
        if action != SecondaryAction::ShowSettings || outcome != (SignalOutcome::Delivered { pid: me }) {
            return Err(format!("user relaunch: {action:?} {outcome:?}"));
        }
        if took >= Duration::from_secs(2) {
            return Err(format!("the second instance took {took:?} (Req 1.4: < 2 s)"));
        }
        let dialog = wait(Duration::from_secs(2).saturating_sub(start.elapsed()), || {
            let d = settings_dialogs();
            (d.len() == 1).then(|| d[0])
        })
        .ok_or("Settings was not shown within 2 s of the relaunch")?;
        println!("[process] Settings shown {:?} after the relaunch", start.elapsed());

        // Relaunch while it is displayed (minimized): the same dialog comes
        // back, no second one (Req 12.7).
        // SAFETY: a window of this process; ShowWindow is synchronous.
        unsafe {
            let _ = ShowWindow(dialog, SW_MINIMIZE);
        }
        wait(Duration::from_secs(2), || unsafe { IsIconic(dialog) }.as_bool().then_some(()))
            .ok_or("could not minimize Settings")?;
        let start = Instant::now();
        let (action, outcome, _) = second_instance(&mutex, &["esr.exe"], engine)?;
        if action != SecondaryAction::ShowSettings || outcome != (SignalOutcome::Delivered { pid: me }) {
            return Err(format!("second user relaunch: {action:?} {outcome:?}"));
        }
        wait(Duration::from_secs(2), || {
            (!unsafe { IsIconic(dialog) }.as_bool()).then_some(())
        })
        .ok_or("the displayed Settings was not brought back within 2 s")?;
        pump_for(Duration::from_millis(300));
        let dialogs = settings_dialogs();
        if dialogs != vec![dialog] {
            return Err(format!("expected only the first Settings dialog, found {dialogs:?}"));
        }
        println!("[process] displayed Settings restored {:?} after the relaunch", start.elapsed());
        if runtime.is_finished() {
            return Err("the first instance stopped after showing Settings".into());
        }

        // Exit through the Settings Exit button: stops within 5 s (Req 12.5).
        // SAFETY: a dialog of this process; BN_CLICKED from its Exit button.
        let start = Instant::now();
        unsafe {
            let button = GetDlgItem(Some(dialog), i32::from(IDC_EXIT)).unwrap_or_default();
            let wparam = (usize::from(BN_CLICKED) << 16) | usize::from(IDC_EXIT);
            PostMessageW(Some(dialog), WM_COMMAND, WPARAM(wparam), LPARAM(button.0 as isize))
                .map_err(|e| format!("PostMessageW(Exit): {e}"))?;
        }
        let took = runtime
            .wait_finished(Duration::from_secs(5))
            .ok_or("the runtime did not stop within 5 s of Exit in Settings")?;
        println!("[process] Exit in Settings: runtime stopped after {took:?} ({:?})", start.elapsed());
        if !settings_dialogs().is_empty() {
            return Err("Settings is still shown after Exit".into());
        }
        Ok(())
    })();

    let code = runtime.stop();
    drop(first);
    let cleanup = guard.finish();
    result.unwrap();
    assert_eq!(code, Some(0), "runtime exit code");
    cleanup.unwrap();
}

// ---------------------------------------------------------------------------
// Req 2.5: windows in a separate Explorer process
// ---------------------------------------------------------------------------

fn explorer_exe() -> PathBuf {
    let windir = std::env::var_os("WINDIR").unwrap_or_else(|| r"C:\Windows".into());
    PathBuf::from(windir).join("explorer.exe")
}

#[test]
#[ignore = "opens two File Explorer windows (one in a separate process); run alone with --test-threads=1"]
fn windows_in_a_separate_explorer_process_are_tracked() {
    let _sta = StaGuard::init().expect("STA init");
    let guard = ExplorerGuard::new("process_separate");
    let data_dir = guard.root().join("esr-data");
    let normal = guard.dir("normal");
    let separate = guard.dir("separate");
    let mut runtime = TestRuntime::start(&data_dir, false, false);
    let mut child: Option<Child> = None;
    let mut separate_pid = 0u32;
    let mut skipped: Option<String> = None;

    let result = (|| -> Result<(), String> {
        runtime.engine()?;
        request_open(normal.clone())?;
        let normal_hwnd = wait_for_window(&guard, &normal, Duration::from_secs(20))
            .ok_or("the normal test window did not open")?;

        let path = separate.to_string_lossy().into_owned();
        if path.contains([' ', ',', '"']) {
            return Err(format!("temp path {path} cannot be passed to /separate unquoted"));
        }
        let c = Process::new(explorer_exe())
            .raw_arg(format!("/separate,{path}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("explorer.exe /separate: {e}"))?;
        let launched = c.id();
        child = Some(c);
        let separate_hwnd = match wait_for_window(&guard, &separate, Duration::from_secs(20)) {
            Some(h) => h,
            None => {
                // Not in ShellWindows: adopt any new window of the launched
                // process so the guard closes it, then fail.
                for raw in top_level_cabinets() {
                    let h = HWND(raw as *mut _);
                    if !guard.is_pre_existing(h) && window_pid(h) == launched {
                        guard.adopt(h);
                    }
                }
                return Err("the /separate window did not appear in ShellWindows".into());
            }
        };

        let (pid_normal, shell) = (window_pid(normal_hwnd), shell_pid());
        separate_pid = window_pid(separate_hwnd);
        println!(
            "[process] shell pid {shell}, normal window pid {pid_normal}, separate window pid \
             {separate_pid}, launched explorer.exe pid {launched}"
        );
        if separate_pid == pid_normal {
            skipped = Some(format!(
                "explorer.exe /separate did not create a distinct process (both windows in pid {pid_normal})"
            ));
            return Ok(());
        }

        // Both are tracked: a checkpoint lists both (Req 2.5, 3.7).
        let doc = wait(Duration::from_secs(15), || {
            read_doc(&data_dir).filter(|d| lists(d, &normal) && lists(d, &separate))
        })
        .ok_or_else(|| {
            format!(
                "no checkpoint lists both windows; last Session_File: {:?}",
                read_doc(&data_dir).map(|d| d
                    .snapshot
                    .windows
                    .iter()
                    .flat_map(|w| w.tabs.iter().map(|t| t.location.parsing_name.to_string_lossy()))
                    .collect::<Vec<_>>())
            )
        })?;
        println!("[process] checkpoint ({:?}) lists both windows", doc.cause);
        Ok(())
    })();

    let code = runtime.stop();
    let cleanup = guard.finish();

    // The separate process we launched: give it time to exit by itself once
    // its window is closed. Kill it only if it is verifiably ours (the
    // launched PID), not the shell, and hosts no Explorer window any more.
    if let Some(mut c) = child {
        let pid = c.id();
        let exited = wait(Duration::from_secs(10), || c.try_wait().ok().flatten().map(|_| ()));
        if exited.is_some() {
            println!("[process] launched explorer.exe {pid} has exited");
        } else {
            let hosts_window = top_level_cabinets()
                .into_iter()
                .any(|raw| window_pid(HWND(raw as *mut _)) == pid);
            if pid != shell_pid() && !hosts_window {
                let _ = c.kill();
                let _ = c.wait();
                println!("[process] killed the launched separate explorer.exe {pid} (no windows left)");
            } else {
                println!("[process] launched explorer.exe {pid} left running (hosts windows or is the shell)");
            }
        }
        // The launcher usually hands the window to a DCOM-started
        // `explorer.exe /factory,{…} -Embedding` process, which ends by
        // itself some time after its last window closed. It is not killed.
        if separate_pid != 0 && separate_pid != pid && separate_pid != shell_pid() {
            let ended = install::wait_for_process_exit(separate_pid, Duration::from_secs(90));
            println!(
                "[process] separate host process {separate_pid} {}",
                if ended { "has exited" } else { "is still running after 90 s (left alone)" }
            );
        }
    }

    result.unwrap();
    assert_eq!(code, Some(0), "runtime exit code");
    cleanup.unwrap();
    if let Some(why) = skipped {
        println!("[process] SKIPPED Req 2.5 check: {why}");
    }
}

// ---------------------------------------------------------------------------
// Req 12.5, 12.8: exit keeps every window open and saves the session
// ---------------------------------------------------------------------------

#[test]
#[ignore = "opens two File Explorer windows; run alone with --test-threads=1"]
fn exit_request_keeps_windows_open_and_saves_the_session() {
    let _sta = StaGuard::init().expect("STA init");
    let guard = ExplorerGuard::new("process_exit");
    let data_dir = guard.root().join("esr-data");
    let a = guard.dir("exit_a");
    let b = guard.dir("exit_b");
    let mut runtime = TestRuntime::start(&data_dir, false, false);

    let result = (|| -> Result<(), String> {
        runtime.engine()?;
        request_open(a.clone())?;
        let ha = wait_for_window(&guard, &a, Duration::from_secs(20)).ok_or("window A did not open")?;
        request_open(b.clone())?;
        let hb = wait_for_window(&guard, &b, Duration::from_secs(20)).ok_or("window B did not open")?;
        wait(Duration::from_secs(10), || read_doc(&data_dir).filter(|d| lists(d, &a) && lists(d, &b)))
            .ok_or("no checkpoint lists both windows")?;
        pump_for(Duration::from_millis(500));
        let before: Vec<_> = [ha, hb].iter().map(|&h| (window_rect(h), window_folders(h))).collect();

        let start = Instant::now();
        runtime.post_exit()?;
        let took = runtime
            .wait_finished(Duration::from_secs(5))
            .ok_or("the runtime did not stop within 5 s of the exit request")?;
        println!("[process] runtime stopped {took:?} after ESR_EXIT ({:?})", start.elapsed());

        let doc = read_doc(&data_dir).ok_or("no readable Session_File after exit")?;
        if doc.cause != SaveCause::ExitThroughSettings {
            return Err(format!("Session_File cause {:?}, expected ExitThroughSettings", doc.cause));
        }
        if !lists(&doc, &a) || !lists(&doc, &b) {
            return Err(format!("the exit capture misses a test window: {doc:?}"));
        }

        pump_for(Duration::from_millis(500));
        for (i, &h) in [ha, hb].iter().enumerate() {
            if !alive(h) {
                return Err(format!("test window {h:?} was closed by the exit"));
            }
            let after = (window_rect(h), window_folders(h));
            if after != before[i] {
                return Err(format!("test window {h:?} changed: {:?} -> {after:?}", before[i]));
            }
        }
        println!("[process] both test windows are still open and unchanged");
        Ok(())
    })();

    let code = runtime.stop();
    let cleanup = guard.finish();
    result.unwrap();
    assert_eq!(code, Some(0), "runtime exit code");
    cleanup.unwrap();
}

// ---------------------------------------------------------------------------
// Req 12.6: uninstall removes every artifact and leaves windows open
// ---------------------------------------------------------------------------

#[test]
#[ignore = "runs the runtime and opens one File Explorer window; run alone with --test-threads=1"]
fn uninstall_removes_every_artifact_and_leaves_windows_open() {
    let _sta = StaGuard::init().expect("STA init");
    let guard = ExplorerGuard::new("process_uninstall");
    let root = guard.dir("uninstall");
    let reg_root = format!(r"Software\ESRTest_{}_process", std::process::id());
    let mut layout = Layout::from_roots(
        &root.join("Local"),
        &root.join("Start Menu").join("Programs"),
        &root.join("Temp"),
    );
    layout.uninstall_key = format!(r"{reg_root}\Uninstall\ExplorerSessionRecall");
    assert!(!layout.uninstall_key.contains(r"CurrentVersion\Uninstall"));
    // No legacy (v0.1) migration steps: they would name the real startup
    // registration and Uninstall key of the old build.
    layout.legacy = None;
    std::fs::create_dir_all(&layout.temp_dir).unwrap();
    let download = root.join("Downloads").join("esr.exe");
    std::fs::create_dir_all(download.parent().unwrap()).unwrap();
    std::fs::write(&download, b"MZ test program").unwrap();
    let window_dir = guard.dir("uninstall_window");

    let mut runtime: Option<TestRuntime> = None;
    // Mocked start-at-sign-in registration: (exe, enabled).
    let mut startup: Option<(PathBuf, bool)> = None;

    let result = (|| -> Result<(), String> {
        // ---- Install (real plan; system-wide steps mocked) ----
        let inputs = InstallInputs {
            current_exe: download.clone(),
            exe_size: std::fs::metadata(&download).map_err(|e| e.to_string())?.len(),
            start_at_sign_in: true,
            version: "0.0.0-test".into(),
        };
        let plan = install::install_plan(&layout, &inputs)?;
        let outcome = install::execute(&plan, |step| match step {
            Step::StopRunningInstance => match own_engine_window() {
                None => Ok(()),
                Some(_) => Err("an instance is already running in this process".into()),
            },
            Step::CopyExe { from, to } => install::copy_exe(from, to).map_err(|e| e.to_string()),
            Step::CreateShortcut(spec) => install::create_shortcut(spec).map_err(|e| e.message()),
            Step::RegisterStartup { exe, enabled, .. } => {
                startup = Some((exe.clone(), *enabled));
                Ok(())
            }
            Step::WriteUninstallEntry { key, values } if *key == layout.uninstall_key => {
                install::write_registry_values(key, values).map_err(|e| e.message())
            }
            Step::StartInstalled { .. } => {
                runtime = Some(TestRuntime::start(&layout.data_dir, false, true));
                Ok(())
            }
            other => Err(format!("unexpected install step {other:?}")),
        });
        if !outcome.succeeded() {
            return Err(format!("install failed: {outcome:?}"));
        }
        for (what, ok) in [
            ("program", layout.installed_exe().is_file()),
            ("shortcut", layout.shortcut.is_file()),
            ("uninstall key", reg_key_exists(&layout.uninstall_key)),
            ("start at sign-in", startup == Some((layout.installed_exe(), true))),
        ] {
            if !ok {
                return Err(format!("install did not create the {what}"));
            }
        }
        runtime.as_ref().ok_or("no runtime")?.engine()?;

        // A window is open during the uninstall; the runtime checkpoints it.
        request_open(window_dir.clone())?;
        let hwnd = wait_for_window(&guard, &window_dir, Duration::from_secs(20))
            .ok_or("the test window did not open")?;
        wait(Duration::from_secs(10), || {
            read_doc(&layout.data_dir).filter(|d| lists(d, &window_dir))
        })
        .ok_or("no checkpoint in the installed data directory")?;
        pump_for(Duration::from_millis(500));
        let before = (window_rect(hwnd), window_folders(hwnd));

        // ---- Uninstall from the installed program (hand-off path) ----
        let plan = install::uninstall_plan(&layout, &layout.installed_exe(), std::process::id());
        let mut stop_cause = None;
        let mut created_by_runtime = Vec::new();
        let mut seeded = Vec::new();
        let mut handoff: Option<PathBuf> = None;
        let mut removing_started = false;
        let outcome = install::execute(&plan, |step| match step {
            Step::StopRunningInstance => {
                if removing_started {
                    return Err("stop after removal started".into());
                }
                let rt = runtime.as_ref().ok_or("no running instance")?;
                rt.post_exit()?;
                let took = rt
                    .wait_finished(install::STOP_TIMEOUT)
                    .ok_or("the running instance did not stop")?;
                println!("[process] uninstall stopped the instance in {took:?}");
                stop_cause = read_doc(&layout.data_dir).map(|d| d.cause);
                // Every data file the runtime did not write itself stands
                // for a leftover (backup, settings, previous log, temp files).
                for file in layout.data_files() {
                    let name = file.file_name().unwrap().to_string_lossy().into_owned();
                    if file.exists() {
                        created_by_runtime.push(name);
                    } else {
                        std::fs::write(&file, b"test leftover").map_err(|e| e.to_string())?;
                        seeded.push(name);
                    }
                }
                Ok(())
            }
            Step::UnregisterStartup { exe, .. } => {
                removing_started = true;
                match &startup {
                    Some((registered, _)) if registered == exe => {
                        startup = None;
                        Ok(())
                    }
                    other => Err(format!("unregister of {exe:?}, registered: {other:?}")),
                }
            }
            Step::RemoveFile(p) => {
                removing_started = true;
                install::remove_file(p).map_err(|e| e.to_string())
            }
            Step::RemoveDirIfEmpty(p) => install::remove_dir_if_empty(p).map_err(|e| e.to_string()),
            Step::RemovePlaceholders(p) => install::remove_placeholders(p).map_err(|e| e.to_string()),
            Step::RemoveRegistryKey(key) if *key == layout.uninstall_key => {
                install::remove_registry_key(key).map_err(|e| e.message())
            }
            Step::HandOff { from, to, .. } => {
                if !install::is_inside(to, &layout.temp_dir) {
                    return Err(format!("hand-off copy outside %TEMP%: {to:?}"));
                }
                install::copy_exe(from, to).map_err(|e| e.to_string())?;
                handoff = Some(to.clone());
                // Instead of starting the copy, run its finish plan here.
                let finish = install::finish_plan(&layout, to);
                let o = install::execute(&finish, |s| match s {
                    Step::RemoveFile(p) => install::remove_file(p).map_err(|e| e.to_string()),
                    Step::RemoveDirIfEmpty(p) => {
                        install::remove_dir_if_empty(p).map_err(|e| e.to_string())
                    }
                    // Stands in for the delayed `cmd.exe` (tested in install.rs).
                    Step::ScheduleSelfDelete { exe, dir, .. } => install::remove_file(exe)
                        .and_then(|()| install::remove_dir_if_empty(dir))
                        .map_err(|e| e.to_string()),
                    other => Err(format!("unexpected finish step {other:?}")),
                });
                if o.succeeded() { Ok(()) } else { Err(format!("finish plan: {o:?}")) }
            }
            other => Err(format!("unexpected uninstall step {other:?}")),
        });
        if !outcome.succeeded() {
            return Err(format!("uninstall failed: {outcome:?}"));
        }
        println!(
            "[process] data files written by the runtime: {created_by_runtime:?}; seeded: {seeded:?}"
        );

        if stop_cause != Some(SaveCause::ExitThroughSettings) {
            return Err(format!("the stopped instance's last write: {stop_cause:?}"));
        }
        let mut left = Vec::new();
        for file in layout.data_files() {
            if file.exists() {
                left.push(file.display().to_string());
            }
        }
        for (what, gone) in [
            ("data directory", !layout.data_dir.exists()),
            ("program", !layout.installed_exe().exists()),
            ("program directory", !layout.install_dir.exists()),
            ("shortcut", !layout.shortcut.exists()),
            ("uninstall key", !reg_key_exists(&layout.uninstall_key)),
            ("start at sign-in", startup.is_none()),
            ("hand-off copy", handoff.as_ref().is_some_and(|h| !h.exists() && !h.parent().unwrap().exists())),
        ] {
            if !gone {
                left.push(what.to_string());
            }
        }
        if !left.is_empty() {
            return Err(format!("uninstall left: {left:?}"));
        }

        // The Explorer window is untouched.
        if !alive(hwnd) {
            return Err("the test window was closed by the uninstall".into());
        }
        let after = (window_rect(hwnd), window_folders(hwnd));
        if after != before {
            return Err(format!("the test window changed: {before:?} -> {after:?}"));
        }
        println!("[process] uninstall removed every artifact; the test window is unchanged");
        Ok(())
    })();

    let code = runtime.as_mut().and_then(TestRuntime::stop);
    let _ = install::remove_registry_key(&reg_root);
    let reg_left = reg_key_exists(&reg_root);
    let cleanup = guard.finish();
    result.unwrap();
    assert_eq!(code, Some(0), "runtime exit code");
    assert!(!reg_left, "test registry key {reg_root} was not removed");
    cleanup.unwrap();
}
