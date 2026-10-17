//! Spike R3: close exactly one Explorer tab with `IWebBrowser2::Quit` (Req 7.9, 9.2).
//!
//! ```text
//! cargo test -p esr-win --test spike_r3_close_tab -- --ignored --nocapture
//! ```
//!
//! Assumption under test (design.md, "Verification spikes", R3): calling
//! `IWebBrowser2::Quit` on one tab's browser object, from an STA thread that
//! pumps messages, closes only that tab; the window, its HWND and its other
//! tabs remain. The documented difference (a non-pumping STA does not work
//! reliably) is probed too, as is the UIA fallback (`InvokePattern` on the
//! tab's close button) and the behaviour of `Quit` on the last remaining tab.
//!
//! Scenarios (each builds its own window with 4 tabs `tabA`..`tabD` in a temp
//! folder this test creates, so no existing Explorer window or tab is touched):
//!
//! 1. `Quit` from a pumping STA on the middle tabs (`tabC`, then `tabB`).
//! 2. `Quit` from a non-pumping STA on the middle tab (informational).
//! 3. UIA close-button `InvokePattern` on the middle tab (fallback).
//! 4. `Quit` on the only tab of a one-tab window (window must close).
//!
//! The window is opened with `ShellExecuteExW("opennewwindow", SEE_MASK_IDLIST)`
//! (found by spike R1). Tabs are appended by posting `WM_COMMAND 0xA21B` to the
//! window's newest `ShellTabWindowClass` (falling back to the UIA `AddButton`),
//! then navigated with `Navigate2(navNoHistory)`. Identity of tabs is the
//! `ShellTabWindowClass` HWND (not visibility, which is unreliable); the
//! visual order and names come from UIA `TabItem`s.
//!
//! Safety: a window is identified by HWND (not in the pre-test list) and only
//! windows created here are closed. `ExplorerGuard` (`tests/common`) adopts each
//! window and closes all its tabs when the scenario's `TestWindow` drops, so at
//! most one test window exists at a time; it sweeps leftovers at the end.
//! Run alone with `--test-threads=1`.
//! Scenarios 1 and 3 are asserted; 2 and 4 are reported.

#![cfg(windows)]

mod common;

use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, Instant};

use common::explorer_guard::ExplorerGuard;
use esr_win::shell_util::{StaGuard, enumerate_all_explorer_windows};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationInvokePattern,
    TreeScope_Children, TreeScope_Descendants, UIA_AutomationIdPropertyId, UIA_ButtonControlTypeId,
    UIA_ControlTypePropertyId, UIA_InvokePatternId, UIA_TabItemControlTypeId,
};
use windows::Win32::UI::Shell::{
    ILCreateFromPathW, ILFree, SEE_MASK_FLAG_NO_UI, SEE_MASK_IDLIST, SEE_MASK_NOASYNC,
    SHELLEXECUTEINFOW, ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, IsWindow, MSG, PM_REMOVE, PeekMessageW, PostMessageW, SW_SHOWNOACTIVATE,
    TranslateMessage, WM_COMMAND,
};
use windows::core::{HSTRING, Interface, w};

/// `WM_COMMAND` id of "new tab" in the Explorer frame.
const CMD_NEW_TAB: usize = 0xA21B;
/// `navNoHistory`.
const NAV_NO_HISTORY: i32 = 2;
const TAB_NAMES: [&str; 4] = ["tabA", "tabB", "tabC", "tabD"];

// ---------------------------------------------------------------------------
// Generic helpers
// ---------------------------------------------------------------------------

fn pump_messages() {
    let mut msg = MSG::default();
    // SAFETY: standard message loop on the calling thread with a valid MSG.
    unsafe {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Polls `f` every 20 ms (pumping messages) until it returns `Some` or `timeout` passes.
fn poll<T>(timeout: Duration, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let end = Instant::now() + timeout;
    loop {
        pump_messages();
        if let Some(v) = f() {
            return Some(v);
        }
        if Instant::now() >= end {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn is_window(hwnd: isize) -> bool {
    // SAFETY: IsWindow tolerates stale handles.
    unsafe { IsWindow(Some(HWND(hwnd as *mut _))) }.as_bool()
}

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

// ---------------------------------------------------------------------------
// ShellWindows views
// ---------------------------------------------------------------------------

/// Top-level HWNDs of every Explorer window currently in ShellWindows.
fn all_tops() -> HashSet<isize> {
    enumerate_all_explorer_windows()
        .map(|g| g.iter().map(|w| w.top_hwnd.0 as isize).collect())
        .unwrap_or_default()
}

/// Tab HWNDs (ShellWindows order) of the window `top`, or `None` if it is gone.
fn tabs_of(top: isize) -> Option<Vec<isize>> {
    let groups = enumerate_all_explorer_windows().ok()?;
    let group = groups.iter().find(|g| g.top_hwnd.0 as isize == top)?;
    Some(
        group
            .tabs
            .iter()
            .filter_map(|t| t.tab_hwnd.map(|h| h.0 as isize))
            .collect(),
    )
}

/// The browser of the tab with the given HWND (COM object of the calling STA).
fn browser_for_tab(tab: isize) -> Option<windows::Win32::UI::Shell::IWebBrowser2> {
    let groups = enumerate_all_explorer_windows().ok()?;
    groups
        .into_iter()
        .flat_map(|g| g.tabs)
        .find(|t| t.tab_hwnd.is_some_and(|h| h.0 as isize == tab))
        .map(|t| t.browser)
}

// ---------------------------------------------------------------------------
// UIA (on a short-lived MTA thread)
// ---------------------------------------------------------------------------

fn on_mta<T: Send + 'static>(
    f: impl FnOnce(&IUIAutomation) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    std::thread::spawn(move || {
        // SAFETY: balanced with CoUninitialize below on this same thread.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .map_err(err)?;
        let result = (|| {
            // SAFETY: valid CLSID, in-proc server.
            let uia: IUIAutomation =
                unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
                    .map_err(err)?;
            f(&uia)
        })();
        // SAFETY: balanced with the CoInitializeEx above.
        unsafe { CoUninitialize() };
        result
    })
    .join()
    .map_err(|_| "UIA thread panicked".to_string())?
}

fn tab_items(uia: &IUIAutomation, top: isize) -> Result<Vec<IUIAutomationElement>, String> {
    // SAFETY: plain UIA calls on live interfaces.
    unsafe {
        let root = uia
            .ElementFromHandle(HWND(top as *mut _))
            .map_err(|e| format!("ElementFromHandle: {e}"))?;
        let cond = uia
            .CreatePropertyCondition(
                UIA_ControlTypePropertyId,
                &VARIANT::from(UIA_TabItemControlTypeId.0),
            )
            .map_err(err)?;
        let array = root
            .FindAll(TreeScope_Descendants, &cond)
            .map_err(|e| format!("FindAll: {e}"))?;
        let n = array.Length().map_err(err)?;
        (0..n).map(|i| array.GetElement(i).map_err(err)).collect()
    }
}

fn element_name(el: &IUIAutomationElement) -> String {
    // SAFETY: plain property read.
    unsafe { el.CurrentName() }
        .map(|b| b.to_string())
        .unwrap_or_default()
}

/// UIA `TabItem` names of window `top`, in visual (left-to-right) order.
fn uia_tab_names(top: isize) -> Vec<String> {
    // Explorer may show full paths as tab titles; keep the last path segment.
    on_mta(move |uia| {
        Ok(tab_items(uia, top)?
            .iter()
            .map(|el| {
                let name = element_name(el);
                name.rsplit('\\').next().unwrap_or(&name).to_string()
            })
            .collect())
    })
    .unwrap_or_else(|e| {
        static LAST: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
        if let Ok(mut last) = LAST.lock()
            && *last != e
        {
            eprintln!("      (UIA tab read failed: {e})");
            *last = e;
        }
        Vec::new()
    })
}

/// Invokes the close button of the `index`-th UIA `TabItem` of `top`.
fn uia_invoke_close(top: isize, index: usize) -> Result<String, String> {
    on_mta(move |uia| {
        let items = tab_items(uia, top)?;
        let item = items
            .get(index)
            .ok_or_else(|| format!("no TabItem at index {index} ({} items)", items.len()))?;
        // SAFETY: plain UIA calls on live interfaces.
        unsafe {
            let button_cond = uia
                .CreatePropertyCondition(
                    UIA_ControlTypePropertyId,
                    &VARIANT::from(UIA_ButtonControlTypeId.0),
                )
                .map_err(err)?;
            let button = match item.FindFirst(TreeScope_Descendants, &button_cond) {
                Ok(b) => b,
                Err(e) => {
                    let all = uia.CreateTrueCondition().map_err(err)?;
                    let kids = item.FindAll(TreeScope_Children, &all).map_err(err)?;
                    let names: Vec<String> = (0..kids.Length().unwrap_or(0))
                        .filter_map(|i| kids.GetElement(i).ok())
                        .map(|k| element_name(&k))
                        .collect();
                    return Err(format!("no close button found ({e}); children: {names:?}"));
                }
            };
            let name = element_name(&button);
            let pattern: IUIAutomationInvokePattern = button
                .GetCurrentPattern(UIA_InvokePatternId)
                .map_err(|e| format!("button '{name}' has no InvokePattern: {e}"))?
                .cast()
                .map_err(err)?;
            pattern.Invoke().map_err(err)?;
            Ok(name)
        }
    })
}

/// Invokes the UIA `AddButton` of window `top` (R2 fallback for tab creation).
fn uia_invoke_add(top: isize) -> Result<(), String> {
    on_mta(move |uia| {
        // SAFETY: plain UIA calls on live interfaces.
        unsafe {
            let root = uia.ElementFromHandle(HWND(top as *mut _)).map_err(err)?;
            let cond = uia
                .CreatePropertyCondition(UIA_AutomationIdPropertyId, &VARIANT::from("AddButton"))
                .map_err(err)?;
            let button = root.FindFirst(TreeScope_Descendants, &cond).map_err(err)?;
            let pattern: IUIAutomationInvokePattern = button
                .GetCurrentPattern(UIA_InvokePatternId)
                .map_err(err)?
                .cast()
                .map_err(err)?;
            pattern.Invoke().map_err(err)
        }
    })
}

// ---------------------------------------------------------------------------
// Test window
// ---------------------------------------------------------------------------

/// An Explorer window created by this test, adopted by the guard. Fully closed
/// on drop (every tab), so at most one test window exists at a time.
struct TestWindow<'g> {
    guard: &'g ExplorerGuard,
    top: isize,
    /// Tab HWNDs in creation order (= expected visual order).
    tabs: Vec<isize>,
    names: Vec<String>,
}

impl Drop for TestWindow<'_> {
    fn drop(&mut self) {
        if is_window(self.top) {
            self.guard.close(HWND(self.top as *mut _));
        }
    }
}

/// Opens a new Explorer window on `dir` and returns it once registered.
fn open_window<'g>(
    guard: &'g ExplorerGuard,
    dir: &Path,
    name: &str,
) -> Result<TestWindow<'g>, String> {
    let before = all_tops();
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
        r.map_err(|e| format!("ShellExecuteExW: {e}"))?;
    }
    let (top, tab) = poll(Duration::from_secs(12), || {
        let groups = enumerate_all_explorer_windows().ok()?;
        groups.iter().find_map(|g| {
            let top = g.top_hwnd.0 as isize;
            let tab = g.tabs.first()?.tab_hwnd?.0 as isize;
            let fresh = !before.contains(&top) && !guard.is_pre_existing(g.top_hwnd);
            fresh.then_some((top, tab))
        })
    })
    .ok_or("new window did not appear in ShellWindows")?;
    guard.adopt(HWND(top as *mut _));
    let mut win = TestWindow {
        guard,
        top,
        tabs: vec![tab],
        names: vec![name.to_string()],
    };
    wait_names(&mut win)?;
    Ok(win)
}

/// Waits until the UIA tab names equal the expected names.
fn wait_names(win: &mut TestWindow) -> Result<(), String> {
    let expected = win.names.clone();
    let top = win.top;
    poll(Duration::from_secs(8), || {
        (uia_tab_names(top) == expected).then_some(())
    })
    .ok_or_else(|| {
        format!(
            "UIA names {:?} never matched expected {:?}",
            uia_tab_names(top),
            expected
        )
    })
}

/// Appends a tab to `win` and navigates it to `dir`.
fn add_tab(win: &mut TestWindow, dir: &Path, name: &str) -> Result<&'static str, String> {
    let before = tabs_of(win.top).ok_or("window vanished")?;
    let newest = *win.tabs.last().ok_or("no tabs")?;
    // SAFETY: posting a message to a window of this test.
    unsafe {
        PostMessageW(
            Some(HWND(newest as *mut _)),
            WM_COMMAND,
            WPARAM(CMD_NEW_TAB),
            LPARAM(0),
        )
    }
    .map_err(err)?;
    let find_new = |top: isize, before: &[isize]| {
        tabs_of(top).and_then(|now| now.into_iter().find(|t| !before.contains(t)))
    };
    let mut method = "WM_COMMAND 0xA21B";
    let mut new_tab = poll(Duration::from_secs(5), || find_new(win.top, &before));
    if new_tab.is_none() {
        method = "UIA AddButton";
        uia_invoke_add(win.top)?;
        new_tab = poll(Duration::from_secs(5), || find_new(win.top, &before));
    }
    let new_tab = new_tab.ok_or("no new tab appeared")?;
    let browser = poll(Duration::from_secs(3), || browser_for_tab(new_tab))
        .ok_or("browser of new tab not found")?;
    let url = VARIANT::from(dir.to_string_lossy().as_ref());
    let flags = VARIANT::from(NAV_NO_HISTORY);
    let empty = VARIANT::default();
    // SAFETY: valid VARIANTs that outlive the call (marshalling needs non-null pointers).
    unsafe { browser.Navigate2(&url, Some(&flags), Some(&empty), Some(&empty), Some(&empty)) }
        .map_err(err)?;
    win.tabs.push(new_tab);
    win.names.push(name.to_string());
    wait_names(win)?;
    Ok(method)
}

/// Builds a window with the first `count` tabs `tabA`...
fn build_window(dirs: &ExplorerGuard, count: usize) -> Result<TestWindow<'_>, String> {
    let mut win = open_window(dirs, &dirs.dir(TAB_NAMES[0]), TAB_NAMES[0])?;
    for name in &TAB_NAMES[1..count] {
        let method = add_tab(&mut win, &dirs.dir(name), name)?;
        println!("    added {name} via {method}");
    }
    Ok(win)
}

// ---------------------------------------------------------------------------
// Closing
// ---------------------------------------------------------------------------

/// Calls `Quit` on the tab's browser from a fresh STA thread. With `pump`, the
/// thread then pumps messages for 1.5 s; otherwise it sleeps without pumping.
/// Returns the duration of the `Quit` call in ms.
fn quit_on_thread(tab: isize, pump: bool) -> std::thread::JoinHandle<Result<f64, String>> {
    std::thread::spawn(move || {
        let _sta = StaGuard::init().map_err(err)?;
        let browser = browser_for_tab(tab).ok_or("tab browser not found")?;
        let t = Instant::now();
        // SAFETY: plain COM call on a live interface.
        let result = unsafe { browser.Quit() };
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        let end = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < end {
            if pump {
                pump_messages();
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        result.map_err(|e| format!("Quit failed after {ms:.1} ms: {e}"))?;
        Ok(ms)
    })
}

struct Verdict {
    ok: bool,
    ms_to_gone: Option<f64>,
    notes: Vec<String>,
}

/// Waits for `closed` to leave ShellWindows, then checks that only it did.
fn verify_only_closed(win: &mut TestWindow, closed_index: usize, started: Instant) -> Verdict {
    let closed = win.tabs[closed_index];
    let closed_name = win.names[closed_index].clone();
    let top = win.top;
    let mut notes = Vec::new();
    let gone = poll(Duration::from_secs(5), || match tabs_of(top) {
        Some(t) if t.contains(&closed) => None,
        _ => Some(started.elapsed().as_secs_f64() * 1000.0),
    });
    let mut ok = true;
    if gone.is_none() {
        notes.push("target tab never left ShellWindows".to_string());
        ok = false;
    }
    let mut expected_tabs = win.tabs.clone();
    expected_tabs.remove(closed_index);
    let mut expected_names = win.names.clone();
    expected_names.remove(closed_index);
    // Let ShellWindows and UIA settle before comparing.
    let settled = poll(Duration::from_secs(4), || {
        let now = tabs_of(top)?;
        let same = now.len() == expected_tabs.len()
            && expected_tabs.iter().all(|t| now.contains(t))
            && uia_tab_names(top) == expected_names;
        same.then_some(())
    });
    let now_tabs = tabs_of(top);
    let now_names = uia_tab_names(top);
    notes.push(format!(
        "after close: window alive={} ShellWindows tabs={} UIA names={now_names:?}",
        is_window(top),
        now_tabs.as_ref().map_or(0, Vec::len)
    ));
    if settled.is_none() {
        ok = false;
        notes.push(format!(
            "expected {} tab(s) with names {expected_names:?} (closed '{closed_name}')",
            expected_tabs.len()
        ));
    }
    if !is_window(top) {
        ok = false;
    }
    if ok {
        win.tabs = expected_tabs;
        win.names = expected_names;
    }
    Verdict {
        ok,
        ms_to_gone: gone,
        notes,
    }
}

fn quit_scenario(
    dirs: &ExplorerGuard,
    pump: bool,
    closes: &[usize],
) -> Result<Vec<Verdict>, String> {
    let mut win = build_window(dirs, 4)?;
    let top = win.top;
    let mut verdicts = Vec::new();
    for &index in closes {
        let target = win.tabs[index];
        let started = Instant::now();
        let handle = quit_on_thread(target, pump);
        let mut verdict = verify_only_closed(&mut win, index, started);
        match handle.join() {
            Ok(Ok(ms)) => verdict
                .notes
                .push(format!("Quit call returned in {ms:.1} ms")),
            Ok(Err(e)) => {
                verdict.notes.push(e);
                verdict.ok = false;
            }
            Err(_) => verdict.ok = false,
        }
        verdict
            .notes
            .push(format!("window HWND unchanged: {}", is_window(top)));
        verdicts.push(verdict);
        if !is_window(top) {
            break;
        }
    }
    Ok(verdicts)
}

fn uia_scenario(dirs: &ExplorerGuard) -> Result<Verdict, String> {
    let mut win = build_window(dirs, 4)?;
    let index = 2;
    let started = Instant::now();
    let button = uia_invoke_close(win.top, index)?;
    let mut verdict = verify_only_closed(&mut win, index, started);
    verdict.notes.push(format!("invoked UIA button '{button}'"));
    Ok(verdict)
}

/// Returns (window closed, ms until it left ShellWindows).
fn last_tab_scenario(dirs: &ExplorerGuard) -> Result<(bool, Option<f64>, String), String> {
    let win = build_window(dirs, 1)?;
    let started = Instant::now();
    let handle = quit_on_thread(win.tabs[0], true);
    let top = win.top;
    let gone = poll(Duration::from_secs(6), || {
        (tabs_of(top).is_none()).then(|| started.elapsed().as_secs_f64() * 1000.0)
    });
    let call = match handle.join() {
        Ok(Ok(ms)) => format!("Quit call returned in {ms:.1} ms"),
        Ok(Err(e)) => e,
        Err(_) => "Quit thread panicked".into(),
    };
    let closed = gone.is_some() && !is_window(top);
    Ok((closed, gone, call))
}

/// Outcome of one `Quit` on tab `index` of a fresh 4-tab window.
fn quit_classify(dirs: &ExplorerGuard, index: usize) -> Result<String, String> {
    let mut win = build_window(dirs, 4)?;
    let top = win.top;
    let target = win.tabs[index];
    let before = win.tabs.clone();
    let handle = quit_on_thread(target, true);
    // Observe for 3 s; stop early once the state changed and settled.
    let started = Instant::now();
    poll(Duration::from_secs(3), || {
        let now = tabs_of(top);
        (now.as_ref() != Some(&before)).then_some(())
    });
    std::thread::sleep(Duration::from_millis(700));
    let _ = handle.join();
    let outcome = match tabs_of(top) {
        None => "WHOLE WINDOW closed".to_string(),
        Some(now) if now == before => "NO-OP (nothing closed)".to_string(),
        Some(now) if now.len() == before.len() - 1 && !now.contains(&target) => {
            win.tabs.remove(index);
            "ONLY TARGET closed".to_string()
        }
        Some(now) => format!(
            "OTHER: {} tab(s) left, target present={}",
            now.len(),
            now.contains(&target)
        ),
    };
    Ok(format!(
        "{outcome} (observed {:.0} ms)",
        started.elapsed().as_secs_f64() * 1000.0
    ))
}

fn report(label: &str, v: &Verdict) {
    println!(
        "  [{}] {label}: gone after {} ms",
        if v.ok { "PASS" } else { "FAIL" },
        v.ms_to_gone.map_or("-".into(), |m| format!("{m:.0}")),
    );
    for n in &v.notes {
        println!("      {n}");
    }
}

#[test]
#[ignore = "requires an interactive desktop; opens and closes its own Explorer window"]
fn spike_r3_close_tab() {
    let _sta = StaGuard::init().expect("STA init");
    // Owns the temp root (tab folders) and closes every window opened here.
    let dirs = ExplorerGuard::new("r3");
    for name in TAB_NAMES {
        dirs.dir(name);
    }
    println!("\nSpike R3: close one tab");

    println!("1. Quit from a pumping STA (middle tabs tabC, then tabB)");
    let pumped = quit_scenario(&dirs, true, &[2, 1]).expect("pumped scenario setup");
    for (i, v) in pumped.iter().enumerate() {
        report(&format!("pumped Quit #{}", i + 1), v);
    }

    println!("2. Quit from a NON-pumping STA (middle tab tabC; informational)");
    match quit_scenario(&dirs, false, &[2]) {
        Ok(v) => v.iter().for_each(|v| report("non-pumped Quit", v)),
        Err(e) => println!("  setup failed: {e}"),
    }

    println!("3. UIA close button InvokePattern (middle tab tabC)");
    let uia = uia_scenario(&dirs);
    match &uia {
        Ok(v) => report("UIA close button", v),
        Err(e) => println!("  [FAIL] {e}"),
    }

    println!("4. Quit on the last remaining tab");
    match last_tab_scenario(&dirs) {
        Ok((closed, ms, note)) => println!(
            "  window closed={closed} after {} ms; {note}",
            ms.map_or("-".into(), |m| format!("{m:.0}"))
        ),
        Err(e) => println!("  setup failed: {e}"),
    }

    println!("5. Quit outcome per tab position (fresh 4-tab window each; tabD is active)");
    let mut only_target = 0;
    let mut trials = 0;
    for round in 0..2 {
        for (index, tab_name) in TAB_NAMES.iter().enumerate() {
            match quit_classify(&dirs, index) {
                Ok(o) => {
                    trials += 1;
                    only_target += usize::from(o.starts_with("ONLY TARGET"));
                    println!("  round {round} Quit on {tab_name}: {o}");
                }
                Err(e) => println!("  round {round} Quit on {tab_name}: setup failed: {e}"),
            }
        }
    }

    let pumped_ok = pumped.len() == 2 && pumped.iter().all(|v| v.ok);
    let uia_ok = uia.as_ref().is_ok_and(|v| v.ok);
    println!(
        "\nRESULT: Quit-only-closes-one-tab={pumped_ok} (matrix: {only_target}/{trials} trials closed only the target); UIA-fallback-works={uia_ok}"
    );
    println!(
        "RECOMMENDATION: TabCloseStrategy::{}",
        if pumped_ok && only_target == trials && trials > 0 {
            "Quit"
        } else {
            "UiaCloseButton"
        }
    );
    let cleanup = dirs.finish();
    if let Err(e) = &cleanup {
        println!("cleanup: {e}");
    }
    // `Quit` is reported, not asserted: the spike's job is to choose the strategy.
    // The fallback that the adapter will rely on must work.
    assert!(
        uia_ok,
        "UIA close-button fallback did not close exactly the target tab"
    );
    cleanup.expect("test windows left open");
}
