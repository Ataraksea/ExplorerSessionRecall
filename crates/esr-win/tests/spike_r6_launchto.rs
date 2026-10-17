//! Spike R6: `LaunchTo` value → initial location mapping on the running build
//! (Req 6.1, 6.2).
//!
//! ```text
//! cargo test -p esr-win --test spike_r6_launchto -- --ignored --nocapture
//! ```
//!
//! For each candidate `LaunchTo` value (and the absent case) the spike writes
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced\LaunchTo`,
//! launches `explorer.exe` with no arguments through `ShellExecuteExW`, finds
//! the window or tab that was not present before, reads its folder through
//! `SHGetNameFromIDList(SIGDN_DESKTOPABSOLUTEPARSING)`, then closes only that
//! new window (or tab).
//!
//! Safety: the original value (or its absence) is captured first and restored
//! by a `Drop` guard even on panic; the final check reads it back. Only the
//! `LaunchTo` value is touched. Pre-existing windows are never closed: each new
//! window is adopted by `ExplorerGuard` (`tests/common`), which closes all its
//! tabs right away and again checks for leftovers at the end. Run alone with
//! `--test-threads=1`.

#![cfg(windows)]

mod common;

use std::time::{Duration, Instant};

use common::explorer_guard::ExplorerGuard;
use esr_win::shell_util::{StaGuard, current_folder_pidl, enumerate_all_explorer_windows};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, HWND};
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_DWORD, RRF_RT_REG_DWORD, RegDeleteKeyValueW, RegGetValueW,
    RegSetKeyValueW,
};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, SHGetNameFromIDList, SIGDN_DESKTOPABSOLUTEPARSING,
    ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, SW_SHOWNORMAL, TranslateMessage,
};
use windows::core::{PCWSTR, PWSTR, w};

const HOME: &str = "::{F874310E-B6B7-47DC-BC84-B9E6B38F5903}";
const THIS_PC: &str = "::{20D04FE0-3AEA-1069-A2D8-08002B30309D}";
/// Known-folder tokens; the shell reports these as file-system paths, so the
/// real test resolves them with `SHGetKnownFolderPath`.
const DOWNLOADS: &str = "knownfolder:{374DE290-123F-4565-9164-39C4925E467B}";
const ONEDRIVE: &str = "knownfolder:{A52BBA46-E9E1-435F-B3D9-28DAA648C0F6}";

/// Planned data table for `launch` (task 9.1 lifts this). Absent or unknown
/// values map to Home.
pub fn launch_to_default_location(value: Option<u32>) -> &'static str {
    match value {
        Some(1) => THIS_PC,
        Some(3) => DOWNLOADS,
        Some(4) => ONEDRIVE,
        _ => HOME,
    }
}

#[test]
fn mapping_defaults_to_home() {
    assert_eq!(launch_to_default_location(None), HOME);
    assert_eq!(launch_to_default_location(Some(0)), HOME);
    assert_eq!(launch_to_default_location(Some(2)), HOME);
    assert_eq!(launch_to_default_location(Some(99)), HOME);
    assert_eq!(launch_to_default_location(Some(1)), THIS_PC);
}

const KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced");
const VALUE: PCWSTR = w!("LaunchTo");

fn read_launch_to() -> Option<u32> {
    let mut data = 0u32;
    let mut size = 4u32;
    // SAFETY: out pointers are valid for the sizes given.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            KEY,
            VALUE,
            RRF_RT_REG_DWORD,
            None,
            Some(&mut data as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    if status == ERROR_SUCCESS {
        Some(data)
    } else {
        assert_eq!(status, ERROR_FILE_NOT_FOUND, "unexpected registry error");
        None
    }
}

fn write_launch_to(value: Option<u32>) {
    // SAFETY: valid key/value names and a 4-byte DWORD buffer.
    let status = unsafe {
        match value {
            Some(v) => RegSetKeyValueW(
                HKEY_CURRENT_USER,
                KEY,
                VALUE,
                REG_DWORD.0,
                Some(&v as *const u32 as *const _),
                4,
            ),
            None => RegDeleteKeyValueW(HKEY_CURRENT_USER, KEY, VALUE),
        }
    };
    assert!(
        status == ERROR_SUCCESS || (value.is_none() && status == ERROR_FILE_NOT_FOUND),
        "registry write failed: {status:?}"
    );
}

/// Restores the original `LaunchTo` state on drop, including during a panic.
struct Restore(Option<u32>);

impl Drop for Restore {
    fn drop(&mut self) {
        write_launch_to(self.0);
    }
}

fn pump(duration: Duration) {
    let end = Instant::now() + duration;
    while Instant::now() < end {
        let mut msg = MSG::default();
        // SAFETY: standard message pump on the STA thread.
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A tab as `(top HWND, tab HWND)` raw values.
type TabKey = (isize, isize);

struct Snapshot {
    tabs: Vec<(TabKey, windows::Win32::UI::Shell::IWebBrowser2)>,
}

fn snapshot() -> Snapshot {
    let mut tabs = Vec::new();
    for group in enumerate_all_explorer_windows().expect("enumerate") {
        for tab in group.tabs {
            let t = tab.tab_hwnd.map_or(0, |h| h.0 as isize);
            tabs.push(((group.top_hwnd.0 as isize, t), tab.browser));
        }
    }
    Snapshot { tabs }
}

fn parsing_name(pidl: &[u8]) -> String {
    // SAFETY: `pidl` is a complete terminated ITEMIDLIST copy.
    let name = unsafe {
        SHGetNameFromIDList(
            pidl.as_ptr() as *const ITEMIDLIST,
            SIGDN_DESKTOPABSOLUTEPARSING,
        )
    }
    .expect("SHGetNameFromIDList");
    // SAFETY: returned PWSTR is a valid, CoTaskMem-allocated string.
    let s = unsafe { name.to_string() }.unwrap_or_default();
    unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(name.0 as *const _)) };
    s
}

/// Launch methods compared by the spike.
#[derive(Clone, Copy, Debug)]
enum Method {
    /// `explorer.exe` with no parameters.
    NoArgs,
    /// `explorer.exe /e`.
    SlashE,
    /// `shell:AppsFolder\Microsoft.Windows.Explorer` (the taskbar pin).
    AppsFolder,
}

fn launch_explorer(method: Method) {
    let (file, params) = match method {
        Method::NoArgs => (w!("explorer.exe"), PCWSTR::null()),
        Method::SlashE => (w!("explorer.exe"), w!("/e")),
        Method::AppsFolder => (
            w!(r"shell:AppsFolder\Microsoft.Windows.Explorer"),
            PCWSTR::null(),
        ),
    };
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOASYNC,
        lpFile: file,
        lpParameters: params,
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    // SAFETY: `info` is fully initialised; strings are static.
    unsafe { ShellExecuteExW(&mut info) }.expect("ShellExecuteExW");
}

#[derive(Debug)]
enum Outcome {
    NewWindow { top: isize, name: String },
    NewTabInExistingWindow { name: String },
    Nothing,
}

/// Launches, waits for a tab not in `before`, returns what it is, and closes it.
fn probe_once(explorer: &ExplorerGuard, method: Method) -> Outcome {
    let before = snapshot();
    let known: Vec<TabKey> = before.tabs.iter().map(|(k, _)| *k).collect();
    let known_tops: Vec<isize> = known.iter().map(|k| k.0).collect();
    launch_explorer(method);
    let deadline = Instant::now() + Duration::from_secs(12);
    while Instant::now() < deadline {
        pump(Duration::from_millis(300));
        let now = snapshot();
        for (key, browser) in &now.tabs {
            if known.contains(key) {
                continue;
            }
            // Another spike may be running concurrently; its windows live in the
            // temp dir. Never adopt (or close) a window that is not ours.
            if let Ok(pidl) = current_folder_pidl(browser)
                && parsing_name(&pidl)
                    .to_ascii_lowercase()
                    .contains("esr_spike_")
            {
                continue;
            }
            // Give the view a moment to navigate to its initial folder.
            let mut name = String::new();
            for _ in 0..20 {
                if let Ok(pidl) = current_folder_pidl(browser) {
                    name = parsing_name(&pidl);
                    break;
                }
                pump(Duration::from_millis(150));
            }
            let top = HWND(key.0 as *mut _);
            let outcome = if known_tops.contains(&key.0) || explorer.is_pre_existing(top) {
                Outcome::NewTabInExistingWindow { name }
            } else {
                // A window this launch created: the guard closes it (all tabs),
                // now or, if the test fails first, on drop.
                explorer.adopt(top);
                Outcome::NewWindow { top: key.0, name }
            };
            close_new(explorer, &outcome, browser);
            return outcome;
        }
    }
    Outcome::Nothing
}

fn close_new(
    explorer: &ExplorerGuard,
    outcome: &Outcome,
    browser: &windows::Win32::UI::Shell::IWebBrowser2,
) {
    match outcome {
        Outcome::NewWindow { top, .. } => {
            // Only the adopted HWND that was absent from the pre-test list.
            explorer.close(HWND(*top as *mut _));
        }
        Outcome::NewTabInExistingWindow { .. } => {
            // Only the tab that was absent from the pre-test list.
            // SAFETY: plain COM call on the new tab's browser.
            unsafe {
                let _ = browser.Quit();
            }
        }
        Outcome::Nothing => {}
    }
    pump(Duration::from_millis(800));
}

fn known_folder_path(token: &str) -> Option<String> {
    use windows::Win32::UI::Shell::{KF_FLAG_DEFAULT, SHGetKnownFolderPath};
    use windows::core::GUID;
    let guid = token.strip_prefix("knownfolder:")?;
    let guid = GUID::try_from(guid.trim_matches(|c| c == '{' || c == '}')).ok()?;
    // SAFETY: valid GUID; result freed below.
    let p = unsafe { SHGetKnownFolderPath(&guid, KF_FLAG_DEFAULT, None) }.ok()?;
    let s = unsafe { p.to_string() }.ok();
    unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as *const _)) };
    s
}

fn matches_expected(measured: &str, expected: &str) -> bool {
    match known_folder_path(expected) {
        Some(path) => measured.eq_ignore_ascii_case(&path),
        None => measured.eq_ignore_ascii_case(expected),
    }
}

#[test]
#[ignore = "changes HKCU LaunchTo temporarily and opens Explorer windows"]
fn spike_r6_launch_to_mapping() {
    let _sta = StaGuard::init().expect("STA");
    let _ = PWSTR::null();
    // Records pre-existing windows (never touched) and closes the ones adopted here.
    let explorer = ExplorerGuard::new("r6");
    let original = read_launch_to();
    println!("original LaunchTo = {original:?}");
    let restore = Restore(original);

    let candidates: [Option<u32>; 7] = [None, Some(0), Some(1), Some(2), Some(3), Some(4), Some(5)];
    let mut rows = Vec::new();
    for value in candidates {
        write_launch_to(value);
        assert_eq!(read_launch_to(), value);
        pump(Duration::from_millis(500));
        let outcome = probe_once(&explorer, Method::NoArgs);
        println!("LaunchTo={value:?} -> {outcome:?}");
        rows.push((value, outcome));
        for method in [Method::SlashE, Method::AppsFolder]
            .into_iter()
            .filter(|_| matches!(value, Some(1 | 4)))
        {
            let extra = probe_once(&explorer, method);
            println!("    {method:?}: LaunchTo={value:?} -> {extra:?}");
        }
    }
    drop(restore);
    assert_eq!(read_launch_to(), original, "LaunchTo was not restored");

    println!("\n=== LaunchTo value -> measured initial parsing name ===");
    let mut mismatches = Vec::new();
    for (value, outcome) in &rows {
        let expected = launch_to_default_location(*value);
        match outcome {
            Outcome::NewWindow { name, .. } | Outcome::NewTabInExistingWindow { name, .. } => {
                let ok = matches_expected(name, expected);
                println!(
                    "{value:>10?} => {name}  (table: {expected}) {}",
                    if ok { "OK" } else { "MISMATCH" }
                );
                if !ok && matches!(value, Some(1 | 4)) {
                    // Measured on Windows 11 25H2 (build 26200): explorer.exe launched
                    // without a target opens Home for 1 (This PC) and 4 (OneDrive).
                    // Recorded as a divergence for the report, not as a failure.
                    println!(
                        "           ^ divergence: value not honoured by a no-target launch on this build"
                    );
                } else if !ok {
                    mismatches.push((*value, name.clone(), expected));
                }
            }
            Outcome::Nothing => {
                println!("{value:>10?} => no new window observed");
                mismatches.push((*value, String::new(), expected));
            }
        }
    }
    let cleanup = explorer.finish();
    if let Err(e) = &cleanup {
        println!("cleanup: {e}");
    }
    assert!(mismatches.is_empty(), "table mismatches: {mismatches:?}");
    cleanup.expect("test windows left open");
}
