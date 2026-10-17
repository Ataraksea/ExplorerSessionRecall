//! Integration test for `TabStripReader` against real File Explorer (task 13.6).
//!
//! ```text
//! cargo test -p esr-win --test tab_strip_reader -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Opens one Explorer window on a temp folder (through `ExplorerGuard`, which
//! closes it afterwards), then checks through the reader:
//!
//! * `watch` subscribes and reports the window once its strip is readable;
//! * `read` returns the single tab's name (display name on 22621, full path on
//!   newer builds) as selected;
//! * appending a tab (`WM_COMMAND 0xA21B`) raises `StructureChanged`, and
//!   `read` then shows two tabs with the new one selected;
//! * `select(0)` activates the first tab within the 500 ms timeout, and an
//!   index past the exposed items fails;
//! * cached reads are fast.
//!
//! Requirements 2.1, 2.2, 2.4, 5.4.

#![cfg(windows)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::explorer_guard::{ExplorerGuard, parsing_name, pump_for};
use esr_core::engine::{TabStripState, WinKey};
use esr_win::keys::{WindowKeys, hwnd_value};
use esr_win::shell_util::{StaGuard, current_folder_pidl, enumerate_all_explorer_windows};
use esr_win::tab_strip::{ChangeCallback, TabStripReader};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Shell::{
    ILCreateFromPathW, ILFree, SEE_MASK_FLAG_NO_UI, SEE_MASK_IDLIST, SEE_MASK_NOASYNC,
    SHELLEXECUTEINFOW, ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, SW_SHOWNOACTIVATE, WM_COMMAND};
use windows::core::{HSTRING, w};

const CMD_NEW_TAB: usize = 0xA21B;

fn normalize(p: &str) -> String {
    p.trim_end_matches('\\').to_lowercase()
}

/// Opens a new Explorer window on `dir` (from a launcher STA thread).
fn request_open(dir: PathBuf) -> std::thread::JoinHandle<Result<(), String>> {
    std::thread::spawn(move || {
        let _sta = StaGuard::init().map_err(|e| e.to_string())?;
        // SAFETY: valid path string; the PIDL is freed after the call.
        unsafe {
            let pidl = ILCreateFromPathW(&HSTRING::from(dir.as_os_str()));
            if pidl.is_null() {
                return Err("ILCreateFromPathW failed".into());
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
}

/// The new window showing `dir`, adopted by the guard as soon as it appears.
fn wait_for_window(guard: &ExplorerGuard, dir: &Path, timeout: Duration) -> Option<HWND> {
    let target = normalize(&dir.to_string_lossy());
    let end = Instant::now() + timeout;
    while Instant::now() < end {
        for g in enumerate_all_explorer_windows().unwrap_or_default() {
            if guard.is_pre_existing(g.top_hwnd) {
                continue;
            }
            let shows = g.tabs.iter().any(|t| {
                current_folder_pidl(&t.browser)
                    .ok()
                    .and_then(|p| parsing_name(&p))
                    .is_some_and(|n| normalize(&n) == target)
            });
            if shows {
                guard.adopt(g.top_hwnd);
                return Some(g.top_hwnd);
            }
        }
        pump_for(Duration::from_millis(50));
    }
    None
}

fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + timeout;
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        pump_for(Duration::from_millis(30));
    }
}

fn names(state: &TabStripState) -> Vec<String> {
    state
        .names
        .iter()
        .map(|n| String::from_utf16_lossy(n.units()))
        .collect()
}

#[test]
#[ignore = "opens File Explorer windows; run alone"]
fn tab_strip_reader_reads_selects_and_watches() {
    let guard = ExplorerGuard::new("tab_strip_reader");
    let result = run(&guard);
    let cleanup = guard.finish();
    result.unwrap();
    cleanup.unwrap();
}

fn run(guard: &ExplorerGuard) -> Result<(), String> {
    let _sta = StaGuard::init().map_err(|e| e.to_string())?;
    let dir = guard.dir("esr_tsr_alpha");
    let leaf = "esr_tsr_alpha";

    let changed: Arc<Mutex<Vec<WinKey>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&changed);
    let on_change: ChangeCallback = Arc::new(move |w| sink.lock().unwrap().push(w));
    let reader = TabStripReader::start(on_change).map_err(|e| format!("start: {e:?}"))?;

    request_open(dir.clone())
        .join()
        .map_err(|_| "launcher panicked".to_string())??;
    let top = wait_for_window(guard, &dir, Duration::from_secs(20))
        .ok_or("the test window did not appear")?;
    let keys = WindowKeys::new();
    let win = keys.key_of(hwnd_value(top));
    println!("window {top:?} -> {win:?}");

    // watch: subscribes (searching the strip) and reports the window.
    let t = Instant::now();
    let mut subscribed = false;
    for _ in 0..20 {
        reader.watch(win).map_err(|e| format!("watch: {e:?}"))?;
        if wait_until(Duration::from_millis(1500), || {
            changed.lock().unwrap().contains(&win)
        }) {
            subscribed = true;
            break;
        }
    }
    if !subscribed {
        return Err("watch never reported the window".into());
    }
    println!("watch reported the window after {:?}", t.elapsed());

    // read: one tab, named after the folder, selected.
    let mut state = None;
    wait_until(Duration::from_secs(5), || {
        state = reader.read(win).ok().filter(|s| !s.names.is_empty());
        state.is_some()
    });
    let state = state.ok_or("read failed")?;
    let first = names(&state);
    println!("initial strip: {first:?}, selected {:?}", state.selected);
    if first.len() != 1 {
        return Err(format!("expected one tab, got {first:?}"));
    }
    let name_ok = first[0].eq_ignore_ascii_case(leaf)
        || normalize(&first[0]) == normalize(&dir.to_string_lossy());
    if !name_ok {
        return Err(format!("unexpected tab name {:?}", first[0]));
    }
    if state.selected != Some(0) {
        return Err(format!("expected tab 0 selected, got {:?}", state.selected));
    }

    // Cached reads are fast.
    let t = Instant::now();
    for _ in 0..10 {
        reader.read(win).map_err(|e| format!("cached read: {e:?}"))?;
    }
    let per_read = t.elapsed() / 10;
    println!("cached read: {per_read:?} each");
    if per_read > Duration::from_millis(200) {
        return Err(format!("cached reads too slow: {per_read:?}"));
    }

    // Appending a tab raises StructureChanged; the new tab is rightmost and selected.
    changed.lock().unwrap().clear();
    // SAFETY: posting a plain message to the adopted test window.
    unsafe { PostMessageW(Some(top), WM_COMMAND, WPARAM(CMD_NEW_TAB), LPARAM(0)) }
        .map_err(|e| format!("PostMessageW: {e}"))?;
    if !wait_until(Duration::from_secs(5), || {
        changed.lock().unwrap().contains(&win)
    }) {
        return Err("no StructureChanged after appending a tab".into());
    }
    let mut two = None;
    wait_until(Duration::from_secs(5), || {
        two = reader
            .read(win)
            .ok()
            .filter(|s| s.names.len() == 2 && s.selected == Some(1));
        two.is_some()
    });
    let two = two.ok_or("read never showed two tabs with the new one selected")?;
    println!("after new tab: {:?}, selected {:?}", names(&two), two.selected);
    if names(&two)[0] != first[0] {
        return Err(format!("first tab changed: {:?}", names(&two)));
    }

    // select(0) activates the first tab within the timeout.
    let t = Instant::now();
    reader.select(win, 0).map_err(|e| format!("select(0): {e:?}"))?;
    println!("select(0) took {:?}", t.elapsed());
    if !wait_until(Duration::from_secs(2), || {
        reader.read(win).is_ok_and(|s| s.selected == Some(0))
    }) {
        return Err("tab 0 did not become selected".into());
    }
    reader.select(win, 1).map_err(|e| format!("select(1): {e:?}"))?;
    if !wait_until(Duration::from_secs(2), || {
        reader.read(win).is_ok_and(|s| s.selected == Some(1))
    }) {
        return Err("tab 1 did not become selected".into());
    }

    // An index past the exposed items fails.
    match reader.select(win, 5) {
        Err(e) => println!("select(5) failed as expected: {}", e.message),
        Ok(()) => return Err("select(5) succeeded".into()),
    }

    reader.unwatch(win);
    drop(reader);
    Ok(())
}
