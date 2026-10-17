//! End-to-end smoke test of the runtime against real File Explorer
//! (`#[ignore]`; opens one Explorer window). Run alone:
//!
//! ```text
//! cargo test -p esr-app --test runtime_smoke -- --ignored --test-threads=1
//! ```
//!
//! The runtime runs on its own thread with a temporary data directory, no UI
//! thread, no panic guard, and no Session_File at start (so it never
//! restores anything; it only tracks). The test opens one window on a temp
//! folder through `ExplorerGuard` (which closes it again even on failure),
//! waits for the checkpoint that lists the folder, closes the window, and
//! stops the runtime with `ESR_EXIT`, sent to the Engine window of this
//! process only (never to a real instance).

#[path = "../../esr-win/tests/common/explorer_guard.rs"]
#[allow(dead_code)]
mod explorer_guard;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use esr_app::instance::{ENGINE_WINDOW_CLASS, InstanceMessages};
use esr_app::runtime::{RuntimeOptions, Storage, run, startup_input};
use esr_core::engine::SessionFileRead;
use esr_core::format::parse;
use esr_core::model::{SaveCause, SessionDoc};
use esr_core::settings::Settings;
use esr_win::shell_util::{StaGuard, current_folder_pidl, enumerate_all_explorer_windows};
use explorer_guard::{ExplorerGuard, parsing_name, pump_once};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Shell::{
    ILCreateFromPathW, ILFree, SEE_MASK_FLAG_NO_UI, SEE_MASK_IDLIST, SEE_MASK_NOASYNC,
    SHELLEXECUTEINFOW, ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, GetWindowThreadProcessId, PostMessageW, SW_SHOWNOACTIVATE,
};
use windows::core::{HSTRING, PCWSTR, w};

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

/// The new window showing `dir`, adopted by the guard as soon as it appears.
fn wait_for_window(guard: &ExplorerGuard, dir: &Path) -> Option<HWND> {
    let target = normalize(&dir.to_string_lossy());
    wait(Duration::from_secs(20), || {
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
        let mut pid = 0;
        // SAFETY: valid out pointer.
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        if pid == std::process::id() {
            return Some(hwnd);
        }
        after = Some(hwnd);
    }
}

fn read_doc(dir: &Path) -> Option<SessionDoc> {
    parse(&std::fs::read(dir.join("session.esr")).ok()?).ok()
}

fn lists(doc: &SessionDoc, folder: &Path) -> bool {
    let target = normalize(&folder.to_string_lossy());
    doc.snapshot.windows.iter().flat_map(|w| &w.tabs).any(|t| {
        normalize(&t.location.parsing_name.to_string_lossy()) == target
    })
}

#[test]
#[ignore = "opens a File Explorer window; run alone with --test-threads=1"]
fn runtime_tracks_a_window_and_writes_the_session_file() {
    let _sta = StaGuard::init().expect("STA init");
    let guard = ExplorerGuard::new("runtime_smoke");
    let data_dir = std::env::temp_dir().join(format!("esr_runtime_smoke_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data_dir);
    let folder = guard.dir("smoke");
    let others_open = !explorer_guard::top_level_cabinets().is_empty();

    let runtime = {
        let data_dir = data_dir.clone();
        std::thread::Builder::new()
            .name("esr-engine-test".into())
            .spawn(move || {
                let storage = Storage::open(&data_dir);
                let input = startup_input(SessionFileRead::Absent, false);
                let options = RuntimeOptions { ui: false, panic_guard: false, ..RuntimeOptions::default() };
                run(Settings::default(), input, storage, options)
            })
            .expect("spawn the runtime")
    };

    let engine = wait(Duration::from_secs(10), own_engine_window);
    let result = (|| -> Result<(), String> {
        engine.ok_or("no ESR.Engine window")?;
        request_open(folder.clone())?;
        let hwnd = wait_for_window(&guard, &folder).ok_or("the test window did not open")?;
        // A checkpoint lists the window within ~0.5 s of its scan (Req 3.7).
        wait(Duration::from_secs(10), || read_doc(&data_dir).filter(|d| lists(d, &folder)))
            .ok_or("no Session_File listing the window")?;
        if !guard.close(hwnd) {
            return Err("could not close the test window".into());
        }
        if !others_open {
            // It was the last window: the Session_End write keeps it.
            let doc = wait(Duration::from_secs(10), || {
                read_doc(&data_dir).filter(|d| {
                    matches!(d.cause, SaveCause::LastWindowClosed | SaveCause::LastTabClosed)
                })
            })
            .ok_or("no Session_End write after the last window closed")?;
            if !lists(&doc, &folder) {
                return Err(format!("Session_End snapshot misses the window: {doc:?}"));
            }
        }
        Ok(())
    })();

    // Stop the runtime in every case (ESR_EXIT, as `--uninstall` sends it).
    if let Some(engine) = engine {
        let exit = InstanceMessages::register().exit;
        // SAFETY: posting a registered message to this process's own window.
        let _ = unsafe { PostMessageW(Some(engine), exit, WPARAM(0), LPARAM(0)) };
    }

    let finished = wait(Duration::from_secs(10), || runtime.is_finished().then_some(()));
    let cleanup = guard.finish();
    let _ = std::fs::remove_dir_all(&data_dir);
    result.unwrap();
    assert!(finished.is_some(), "the runtime did not exit after ESR_EXIT");
    assert_eq!(runtime.join().expect("runtime thread"), 0);
    cleanup.unwrap();
}
