//! Integration test for `ShellNavigator` (task 13.8) against the real
//! Explorer.
//!
//! ```text
//! cargo test -p esr-win --test navigator_integration -- --ignored --nocapture --test-threads=1
//! ```
//!
//! One window is opened through the navigator on a temp folder `navA`. The
//! test checks that:
//!
//! * the window is opened with `SW_HIDE`, assigned at `EVENT_OBJECT_CREATE`,
//!   and stays invisible (`HideStrategy::LaunchHidden`);
//! * `claim` matches it to its request by initial Location;
//! * two tabs are appended with `0xA21B` (one outstanding at a time) and
//!   navigated with `Navigate2(navNoHistory)` to `navB` and `navC`;
//! * `select_items` selects a file in `navC`;
//! * `close_tab` closes the middle tab (UIA close button) while the window
//!   is still hidden;
//! * `reveal` makes the window visible at its final placement;
//! * z-order, focus, and a minimized placement with `WPF_RESTORETOMAXIMIZED`
//!   can be applied.
//!
//! Window safety: `ExplorerGuard` (`tests/common`) adopts the window as soon
//! as the navigator assigns it. It closes the window (all tabs) on success,
//! failure, or panic, and sweeps any window that shows a folder under its
//! temp root. Before closing, the window gets back the placement Explorer
//! gave it, so Explorer does not remember the test's size. Run this test
//! alone.

#![cfg(windows)]

mod common;

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

use common::explorer_guard::{ExplorerGuard, pump_once};
use esr_core::engine::{ItemId, ObservedLocation, RequestId, TabKey, WinKey};
use esr_core::model::{LocationId, Rect, ShowState, WideString};
use esr_core::plan::placement::FinalPlacement;
use esr_win::display_info::displays;
use esr_win::keys::{TabKeys, WindowKeys, hwnd_of, hwnd_value};
use esr_win::location::{capture_from_pidl, try_capture_from_parsing_name};
use esr_win::navigator::{NavigatorSignal, ShellNavigator, layered_alpha};
use esr_win::shell_util::{
    StaGuard, current_folder_pidl, enumerate_all_explorer_windows, is_window_visible, pidl_to_bytes,
    shell_browser,
};
use esr_win::strategy::Strategies;
use esr_win::window_event_source::window_placement;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CoTaskMemFree, IBindCtx};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    IFolderView, IWebBrowser2, SHParseDisplayName, SVGIO_ALLVIEW, SVGIO_SELECTION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowPlacement, IsIconic, SW_SHOWNOACTIVATE, SetWindowPlacement, WINDOWPLACEMENT,
    WPF_RESTORETOMAXIMIZED,
};
use windows::core::{Interface, PCWSTR};

const STEP: Duration = Duration::from_secs(10);

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

fn wide(s: &str) -> WideString {
    WideString(s.encode_utf16().collect())
}

fn location_of(path: &Path) -> LocationId {
    try_capture_from_parsing_name(&wide(&path.to_string_lossy()))
        .unwrap_or_else(|e| panic!("capture {}: {e}", path.display()))
}

fn pidl_of(path: &Path) -> Vec<u8> {
    let z: Vec<u16> = path
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut out: *mut ITEMIDLIST = std::ptr::null_mut();
    // SAFETY: NUL-terminated name, valid out pointer; the PIDL is copied and freed.
    unsafe {
        SHParseDisplayName(PCWSTR(z.as_ptr()), None::<&IBindCtx>, &mut out, 0, None)
            .expect("SHParseDisplayName");
        let bytes = pidl_to_bytes(out);
        CoTaskMemFree(Some(out.cast_const().cast()));
        bytes
    }
}

/// `(tab HWND, browser)` of each ShellWindows tab of `top`.
fn tabs_of(top: HWND) -> Vec<(HWND, IWebBrowser2)> {
    enumerate_all_explorer_windows()
        .unwrap_or_default()
        .into_iter()
        .filter(|g| g.top_hwnd == top)
        .flat_map(|g| g.tabs)
        .filter_map(|t| t.tab_hwnd.map(|h| (h, t.browser)))
        .collect()
}

fn shows(browser: &IWebBrowser2, location: &LocationId) -> bool {
    current_folder_pidl(browser).ok().is_some_and(
        |p| matches!(capture_from_pidl(&p), ObservedLocation::Known(l) if l == *location),
    )
}

fn folder_view(browser: &IWebBrowser2) -> Option<IFolderView> {
    let sb = shell_browser(browser).ok()?;
    // SAFETY: plain COM calls on live interfaces.
    unsafe { sb.QueryActiveShellView() }.ok()?.cast().ok()
}

fn item_count(browser: &IWebBrowser2, selection: bool) -> i32 {
    let flags = if selection {
        SVGIO_SELECTION
    } else {
        SVGIO_ALLVIEW
    };
    folder_view(browser)
        // SAFETY: plain COM call.
        .and_then(|v| unsafe { v.ItemCount(flags) }.ok())
        .unwrap_or(-1)
}

fn get_wp(hwnd: HWND) -> WINDOWPLACEMENT {
    let mut wp = WINDOWPLACEMENT {
        length: size_of::<WINDOWPLACEMENT>() as u32,
        ..Default::default()
    };
    // SAFETY: valid, correctly sized out structure.
    let _ = unsafe { GetWindowPlacement(hwnd, &mut wp) };
    wp
}

struct Ctx<'g> {
    guard: &'g ExplorerGuard,
    nav: ShellNavigator,
    tab_keys: TabKeys,
    signals: Rc<RefCell<Vec<NavigatorSignal>>>,
    win: Option<WinKey>,
}

impl Ctx<'_> {
    fn failures(&self) -> Vec<NavigatorSignal> {
        self.signals
            .borrow()
            .iter()
            .filter(|s| !matches!(s, NavigatorSignal::TabCreateRetried { .. }))
            .cloned()
            .collect()
    }

    /// Creates one tab, waits for it to register, and navigates it (without
    /// waiting for the navigation).
    fn add_tab(&mut self, win: WinKey, location: &LocationId) -> Result<TabKey, String> {
        let top = hwnd_of(win.hwnd);
        let before: Vec<HWND> = tabs_of(top).into_iter().map(|(h, _)| h).collect();
        self.nav
            .create_tab(win)
            .map_err(|e| format!("create_tab: {e:?}"))?;
        let t = Instant::now();
        let new = wait(STEP, || {
            tabs_of(top)
                .into_iter()
                .map(|(h, _)| h)
                .find(|h| !before.contains(h))
        })
        .ok_or_else(|| format!("no new tab registered; signals {:?}", self.signals.borrow()))?;
        let tab = self.tab_keys.key_of(hwnd_value(new));
        println!(
            "  tab {new:?} registered after {} ms",
            t.elapsed().as_millis()
        );
        self.nav.tab_registered(win, tab);
        self.nav
            .navigate(tab, location)
            .map_err(|e| format!("navigate: {e:?}"))?;
        Ok(tab)
    }
}

fn run(ctx: &mut Ctx, guard: &ExplorerGuard) -> Result<(), String> {
    let dir_a = guard.dir("navA");
    let dir_b = guard.dir("navB");
    let dir_c = guard.dir("navC");
    for f in ["x.txt", "y.txt"] {
        std::fs::write(dir_c.join(f), b"esr").map_err(|e| e.to_string())?;
    }
    let (loc_a, loc_b, loc_c) = (
        location_of(&dir_a),
        location_of(&dir_b),
        location_of(&dir_c),
    );

    let display = displays()
        .into_iter()
        .find(|d| d.primary)
        .ok_or("no primary display")?;
    let w = display.work;
    let restored = Rect {
        left: w.left + 80,
        top: w.top + 60,
        right: w.left + 80 + 900.min(w.right - w.left - 160),
        bottom: w.top + 60 + 600.min(w.bottom - w.top - 120),
    };
    let placement = FinalPlacement {
        state: ShowState::Normal,
        restored,
        monitor: display.monitor,
        work: display.work,
    };

    println!("1. open_window on navA (hidden, final placement {restored:?})");
    let t_open = Instant::now();
    ctx.nav
        .open_window(RequestId(1), &loc_a, &placement)
        .map_err(|e| format!("open_window: {e:?}"))?;
    let win = wait(STEP, || ctx.nav.pending_assignment(RequestId(1)))
        .ok_or_else(|| format!("no window was assigned; signals {:?}", ctx.signals.borrow()))?;
    let hwnd = hwnd_of(win.hwnd);
    ctx.win = Some(win);
    println!(
        "  assigned {hwnd:?} after {} ms; visible {}, alpha {:?}",
        t_open.elapsed().as_millis(),
        is_window_visible(hwnd),
        layered_alpha(hwnd)
    );
    if !ctx.nav.is_hidden(win) || is_window_visible(hwnd) {
        return Err("the new window is not hidden (launched with SW_HIDE)".into());
    }

    println!("2. claim by initial Location");
    let (tab0, _) = wait(STEP, || {
        let tabs = tabs_of(hwnd);
        (tabs.len() == 1 && shows(&tabs[0].1, &loc_a)).then(|| tabs[0].clone())
    })
    .ok_or("the window never showed navA")?;
    // Adopted only once it shows the test's folder, so a window the user
    // happens to open meanwhile is never adopted (the guard sweeps strays
    // under its temp root anyway).
    ctx.guard.adopt(hwnd);
    let initial = match capture_from_pidl(
        &current_folder_pidl(&tabs_of(hwnd)[0].1).map_err(|e| e.to_string())?,
    ) {
        ObservedLocation::Known(l) => l,
        other => return Err(format!("initial Location: {other:?}")),
    };
    let claimed = ctx.nav.claim(win, &initial);
    if claimed != Some(RequestId(1)) {
        return Err(format!("claim returned {claimed:?}"));
    }
    if ctx.nav.pending_opens() != 0 || is_window_visible(hwnd) {
        return Err("after claim: request still pending or window not hidden".into());
    }
    let tab0 = ctx.tab_keys.key_of(hwnd_value(tab0));

    println!("3. create + navigate two tabs (sequential, navigation overlapped)");
    let tab1 = ctx.add_tab(win, &loc_b)?;
    let tab2 = ctx.add_tab(win, &loc_c)?;
    let expected = [(tab0, &loc_a), (tab1, &loc_b), (tab2, &loc_c)];
    wait(STEP, || {
        let tabs = tabs_of(hwnd);
        expected
            .iter()
            .all(|(t, l)| {
                tabs.iter()
                    .any(|(h, b)| hwnd_value(*h) == t.hwnd && shows(b, l))
            })
            .then_some(())
    })
    .ok_or("the tabs did not reach navA, navB, navC")?;
    let order = ctx.nav.tab_order(win);
    if order != vec![tab0.hwnd, tab1.hwnd, tab2.hwnd] {
        return Err(format!("tracked tab order {order:x?}"));
    }
    if is_window_visible(hwnd) {
        return Err("the window became visible while tabs were built".into());
    }

    println!("4. select_items(y.txt) in navC");
    let browser2 = tabs_of(hwnd)
        .into_iter()
        .find(|(h, _)| hwnd_value(*h) == tab2.hwnd)
        .map(|(_, b)| b)
        .ok_or("tab 2 browser")?;
    wait(STEP, || (item_count(&browser2, false) >= 2).then_some(()))
        .ok_or("navC view never listed its files")?;
    let y = ItemId(pidl_of(&dir_c.join("y.txt")));
    ctx.nav
        .select_items(tab2, &[y])
        .map_err(|e| format!("select_items: {e:?}"))?;
    wait(Duration::from_secs(3), || {
        (item_count(&browser2, true) == 1).then_some(())
    })
    .ok_or_else(|| format!("selection count {}", item_count(&browser2, true)))?;

    println!("5. close_tab(navB) through UIA while hidden");
    let t_close = Instant::now();
    ctx.nav
        .close_tab(tab1)
        .map_err(|e| format!("close_tab: {e:?}"))?;
    wait(STEP, || {
        let tabs = tabs_of(hwnd);
        let gone = !tabs.iter().any(|(h, _)| hwnd_value(*h) == tab1.hwnd);
        (gone && tabs.len() == 2 || !ctx.failures().is_empty()).then_some(())
    })
    .ok_or("the tab did not close")?;
    if !ctx.failures().is_empty() {
        return Err(format!("signals: {:?}", ctx.failures()));
    }
    println!("  closed after {} ms", t_close.elapsed().as_millis());
    let left: Vec<u64> = tabs_of(hwnd).iter().map(|(h, _)| hwnd_value(*h)).collect();
    if !(left.contains(&tab0.hwnd) && left.contains(&tab2.hwnd)) {
        return Err(format!("wrong tabs left: {left:x?}"));
    }

    println!("6. reveal");
    ctx.nav.reveal(win).map_err(|e| format!("reveal: {e:?}"))?;
    if ctx.nav.is_hidden(win) {
        return Err("still hidden after reveal".into());
    }
    // The placement is applied by Explorer's window thread (asynchronously).
    let p = wait(Duration::from_secs(3), || {
        window_placement(hwnd)
            .ok()
            .filter(|p| is_window_visible(hwnd) && layered_alpha(hwnd).is_none() && p.restored == restored)
    })
    .ok_or("the window did not become visible at its final placement")?;
    println!("  placement {:?} {:?}", p.state, p.restored);
    if p.state != ShowState::Normal || p.restored != restored {
        return Err(format!("revealed at {p:?}, expected {restored:?}"));
    }

    println!("7. z-order and focus");
    ctx.nav
        .set_z_order(&[win])
        .map_err(|e| format!("set_z_order: {e:?}"))?;
    let outcome = ctx.nav.focus(win).map_err(|e| format!("focus: {e:?}"))?;
    println!("  focus outcome {outcome:?}");

    println!("8. minimized placement restoring to maximized");
    let min = FinalPlacement {
        state: ShowState::Minimized {
            restore_maximized: true,
        },
        ..placement
    };
    ctx.nav
        .apply_placement(win, &min)
        .map_err(|e| format!("apply_placement: {e:?}"))?;
    // The placement is applied asynchronously by Explorer's window thread
    // (`WPF_ASYNCWINDOWPLACEMENT`): the window can be iconic a few ms before
    // the restore-to-maximized flag reads back, so wait for both.
    let restores_maximized = || get_wp(hwnd).flags.0 & WPF_RESTORETOMAXIMIZED.0 != 0;
    wait(Duration::from_secs(3), || {
        // SAFETY: tolerant read.
        unsafe { IsIconic(hwnd) }.as_bool().then_some(())
    })
    .ok_or("the window did not minimize")?;
    if wait(Duration::from_secs(3), || restores_maximized().then_some(())).is_none() {
        return Err("WPF_RESTORETOMAXIMIZED not set".into());
    }
    if !ctx.failures().is_empty() {
        return Err(format!("signals: {:?}", ctx.failures()));
    }
    println!("  retries: {:?}", ctx.signals.borrow());
    Ok(())
}

#[test]
#[ignore = "requires an interactive desktop; opens and closes its own Explorer window"]
fn navigator_builds_a_hidden_window() {
    let _sta = StaGuard::init().expect("STA init");
    let guard = ExplorerGuard::new("nav");
    let signals: Rc<RefCell<Vec<NavigatorSignal>>> = Rc::default();
    let sink = signals.clone();
    let tab_keys = TabKeys::new();
    let mut nav = ShellNavigator::new(
        Strategies::DEFAULT,
        WindowKeys::new(),
        tab_keys.clone(),
        Rc::new(move |s: NavigatorSignal| {
            println!("  signal {s:?}");
            sink.borrow_mut().push(s);
        }),
    );
    nav.start().expect("start navigator");
    let mut ctx = Ctx {
        guard: &guard,
        nav,
        tab_keys,
        signals,
        win: None,
    };
    let result = run(&mut ctx, &guard);

    // Give the window back the placement Explorer chose, so Explorer does not
    // remember the test's size, then stop the navigator (reveals anything hidden).
    if let Some(win) = ctx.win
        && let Some(mut wp) = ctx.nav.original_placement(win)
    {
        wp.showCmd = SW_SHOWNOACTIVATE.0 as u32;
        // SAFETY: a WINDOWPLACEMENT read from this window.
        let _ = unsafe { SetWindowPlacement(hwnd_of(win.hwnd), &wp) };
    }
    drop(ctx);
    let cleanup = guard.finish();
    if let Err(e) = &result {
        println!("FAILED: {e}");
    }
    result.expect("navigator integration");
    cleanup.expect("test windows left open");
}
