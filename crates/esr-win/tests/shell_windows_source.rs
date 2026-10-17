//! Integration test for `ShellWindowsSource` (task 13.4) against the real
//! Explorer.
//!
//! ```text
//! cargo test -p esr-win --test shell_windows_source -- --ignored --nocapture --test-threads=1
//! ```
//!
//! The test attaches the source and checks that:
//!
//! * a window opened on a temp folder (`opennewwindow`) is offered to
//!   `claim` with its initial Location and, unclaimed, announced with
//!   `InitialLocationKnown` (empty selection) after `NavigationCompleted`;
//! * a new tab (`WM_COMMAND 0xA21B`) is reported with `tab_registered` and
//!   `TabCreated`, and its navigations (`Navigate2`) with
//!   `NavigationCompleted`; `tabs_of` reports both tabs' Locations;
//! * closing that tab posts `TabRevoked` and retires its key;
//! * `SHOpenFolderAndSelectItems` (a Targeted_Launch with a selection) is
//!   announced with the selected file's PIDL;
//! * after `shell_disconnected` the source posts `ShellLost`, reconnects
//!   through its `GetShellWindow()` poll, posts `ShellAvailable`, rescans
//!   every window (same tab keys), and keeps tracking new tabs;
//! * closing the windows revokes their tabs.
//!
//! The shell itself is not restarted; the loss is simulated.
//!
//! Window safety: `ExplorerGuard` (`tests/common`) adopts each test window
//! as soon as it is announced and closes it on success, failure, or panic.
//! It also sweeps any window that shows a folder under its temp root. Run
//! this test alone.

#![cfg(windows)]

mod common;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use common::explorer_guard::{ExplorerGuard, parsing_name, pump_once};
use esr_core::engine::{Event, ObservedLocation, RequestId, TabKey, WinKey};
use esr_core::model::{LocationId, WideString};
use esr_win::keys::{TabKeys, WindowKeys, hwnd_of};
use esr_win::location::try_capture_from_parsing_name;
use esr_win::shell_util::{StaGuard, enumerate_all_explorer_windows};
use esr_win::shell_windows_source::{Connection, ShellSourceHandler, ShellWindowsSource};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Variant::{
    VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BSTR, VariantClear,
};
use windows::Win32::UI::Shell::{
    ILCreateFromPathW, ILFindLastID, ILFree, IWebBrowser2, SEE_MASK_FLAG_NO_UI, SEE_MASK_IDLIST,
    SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, SHOpenFolderAndSelectItems, ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    PostMessageW, SW_SHOWNOACTIVATE, WM_CLOSE, WM_COMMAND,
};
use windows::core::{BSTR, HSTRING, w};

const STEP: Duration = Duration::from_secs(15);
const CMD_NEW_TAB: usize = 0xA21B;

#[derive(Default)]
struct Recorder {
    start: Option<Instant>,
    events: RefCell<Vec<(Duration, Event)>>,
    changed: RefCell<Vec<(Duration, WinKey)>>,
    claims: RefCell<Vec<(WinKey, LocationId)>>,
    registered: RefCell<Vec<(WinKey, TabKey)>>,
}

impl Recorder {
    fn now(&self) -> Duration {
        self.start.map(|s| s.elapsed()).unwrap_or_default()
    }

    fn find<T>(&self, mut f: impl FnMut(&Event) -> Option<T>) -> Option<(Duration, T)> {
        self.events
            .borrow()
            .iter()
            .find_map(|(t, e)| f(e).map(|v| (*t, v)))
    }

    fn count(&self, mut f: impl FnMut(&Event) -> bool) -> usize {
        self.events.borrow().iter().filter(|(_, e)| f(e)).count()
    }

    fn dump(&self) {
        for (t, e) in self.events.borrow().iter() {
            println!("  {:>6} ms  {}", t.as_millis(), describe(e));
        }
    }
}

impl ShellSourceHandler for Recorder {
    fn post(&self, event: Event) {
        self.events.borrow_mut().push((self.now(), event));
    }

    fn window_changed(&self, win: WinKey) {
        self.changed.borrow_mut().push((self.now(), win));
    }

    fn tab_registered(&self, win: WinKey, tab: TabKey) {
        self.registered.borrow_mut().push((win, tab));
    }

    fn claim(&self, win: WinKey, initial: &LocationId) -> Option<RequestId> {
        self.claims.borrow_mut().push((win, initial.clone()));
        None
    }
}

fn name(l: &LocationId) -> String {
    String::from_utf16_lossy(l.parsing_name.units())
}

fn describe(e: &Event) -> String {
    match e {
        Event::NavigationCompleted { tab, location } => {
            format!("NavigationCompleted {tab:?} {}", name(location))
        }
        Event::InitialLocationKnown {
            win,
            location,
            selection,
            ..
        } => format!(
            "InitialLocationKnown {win:?} {} selection {}",
            name(location),
            selection.len()
        ),
        other => format!("{other:?}"),
    }
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
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn pump(d: Duration) {
    let _ = wait::<()>(d, || None);
}

fn normalize(p: &str) -> String {
    p.strip_prefix(r"\\?\")
        .unwrap_or(p)
        .trim_end_matches('\\')
        .to_lowercase()
}

fn location_of(path: &Path) -> LocationId {
    let wide = WideString(path.to_string_lossy().encode_utf16().collect());
    try_capture_from_parsing_name(&wide)
        .unwrap_or_else(|e| panic!("capture {}: {e}", path.display()))
}

/// Opens a new Explorer window on `dir` (from a launcher STA thread).
fn open_window(dir: PathBuf) -> Result<(), String> {
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

/// Opens `dir` with `file` selected, as a browser's "show in folder" does.
fn open_and_select(dir: PathBuf, file: PathBuf) -> Result<(), String> {
    std::thread::spawn(move || {
        let _sta = StaGuard::init().map_err(|e| e.to_string())?;
        // SAFETY: valid path strings; both PIDLs are freed after the call.
        unsafe {
            let folder = ILCreateFromPathW(&HSTRING::from(dir.as_os_str()));
            let item = ILCreateFromPathW(&HSTRING::from(file.as_os_str()));
            if folder.is_null() || item.is_null() {
                return Err("ILCreateFromPathW failed".to_string());
            }
            let child = ILFindLastID(item);
            let r = SHOpenFolderAndSelectItems(folder, Some(&[child.cast_const()]), 0);
            ILFree(Some(folder));
            ILFree(Some(item));
            r.map_err(|e| format!("SHOpenFolderAndSelectItems: {e}"))
        }
    })
    .join()
    .map_err(|_| "launcher panicked".to_string())?
}

/// The browser of tab window `tab` (ShellWindows lookup).
fn browser_of(tab: TabKey) -> Option<IWebBrowser2> {
    enumerate_all_explorer_windows()
        .ok()?
        .into_iter()
        .flat_map(|g| g.tabs)
        .find(|t| t.tab_hwnd.is_some_and(|h| h.0 as usize as u64 == tab.hwnd))
        .map(|t| t.browser)
}

/// `IWebBrowser2::Navigate2` to a path given as a `VT_BSTR`.
fn navigate(browser: &IWebBrowser2, path: &Path) -> Result<(), String> {
    let mut url = VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_BSTR,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 {
                    bstrVal: std::mem::ManuallyDrop::new(BSTR::from(
                        path.to_string_lossy().as_ref(),
                    )),
                },
            }),
        },
    };
    // The marshaled call rejects null optional arguments: pass VT_EMPTY ones.
    let empty = VARIANT::default();
    // SAFETY: `url` is a valid VT_BSTR VARIANT, cleared once after the call.
    let r = unsafe {
        browser.Navigate2(
            &url,
            Some(&empty),
            Some(&empty),
            Some(&empty),
            Some(&empty),
        )
    };
    let _ = unsafe { VariantClear(&mut url) };
    r.map_err(|e| format!("Navigate2: {e}"))
}

fn post(hwnd: HWND, msg: u32, wparam: usize) -> Result<(), String> {
    // SAFETY: posting a plain message to an adopted test window.
    unsafe { PostMessageW(Some(hwnd), msg, WPARAM(wparam), LPARAM(0)) }
        .map_err(|e| format!("PostMessageW: {e}"))
}

/// Waits for `InitialLocationKnown` on `location` and adopts its window.
fn wait_initial(
    guard: &ExplorerGuard,
    rec: &Recorder,
    location: &LocationId,
) -> Result<(Duration, WinKey, Vec<esr_core::engine::ItemId>), String> {
    let found = wait(STEP, || {
        rec.find(|e| match e {
            Event::InitialLocationKnown {
                win,
                location: l,
                selection,
                ..
            } if l == location => Some((*win, selection.clone())),
            _ => None,
        })
    });
    let Some((t, (win, selection))) = found else {
        rec.dump();
        return Err(format!("no InitialLocationKnown for {}", name(location)));
    };
    if !guard.is_under_root(&name(location)) {
        return Err(format!("{} is not under the temp root", name(location)));
    }
    guard.adopt(hwnd_of(win.hwnd));
    Ok((t, win, selection))
}

#[test]
#[ignore = "opens File Explorer windows; run alone"]
fn shell_windows_source_tracks_tabs_and_reconnects() {
    let guard = ExplorerGuard::new("shell_windows_source");
    let result = run(&guard);
    let cleanup = guard.finish();
    result.unwrap();
    cleanup.unwrap();
}

fn run(guard: &ExplorerGuard) -> Result<(), String> {
    let _sta = StaGuard::init().map_err(|e| e.to_string())?;
    let rec = Rc::new(Recorder {
        start: Some(Instant::now()),
        ..Default::default()
    });
    let tab_keys = TabKeys::new();
    let mut source = ShellWindowsSource::new(WindowKeys::new(), tab_keys.clone(), rec.clone());
    source.attach().map_err(|e| format!("attach: {e:?}"))?;
    if source.connection() != Connection::Attached {
        return Err(format!("connection {:?}", source.connection()));
    }
    let before = source.windows().map_err(|e| format!("windows: {e:?}"))?;
    println!("attached; {} window(s) open before the test", before.len());
    if !rec.events.borrow().is_empty() {
        return Err("attach posted events".into());
    }

    // 1. A new window on dirA: claim, NavigationCompleted, InitialLocationKnown.
    let dir_a = guard.dir("swsA");
    let dir_b = guard.dir("swsB");
    let dir_c = guard.dir("swsC");
    let loc_a = location_of(&dir_a);
    let loc_b = location_of(&dir_b);
    let loc_c = location_of(&dir_c);
    open_window(dir_a.clone())?;
    let (t_init, win, selection) = wait_initial(guard, &rec, &loc_a)?;
    let top = hwnd_of(win.hwnd);
    println!("window A {win:?}: InitialLocationKnown at {} ms", t_init.as_millis());
    if !selection.is_empty() {
        return Err(format!("unexpected selection {selection:?}"));
    }
    if !rec.claims.borrow().iter().any(|(w, l)| *w == win && *l == loc_a) {
        return Err(format!("claim not asked: {:?}", rec.claims.borrow()));
    }
    let (t_nav, tab1) = rec
        .find(|e| match e {
            Event::NavigationCompleted { tab, location } if *location == loc_a => Some(*tab),
            _ => None,
        })
        .ok_or("no NavigationCompleted for A")?;
    println!(
        "  NavigationCompleted {tab1:?} at {} ms ({} ms before the announcement)",
        t_nav.as_millis(),
        (t_init - t_nav).as_millis()
    );
    if rec.count(|e| matches!(e, Event::TabCreated { .. } | Event::WindowOpened { .. })) != 0 {
        return Err("a new window must not report TabCreated/WindowOpened".into());
    }
    if !rec.changed.borrow().iter().any(|(_, w)| *w == win) {
        return Err("window_changed not called for the new window".into());
    }
    let tabs = source.tabs_of(win).map_err(|e| format!("tabs_of: {e:?}"))?;
    if tabs.tabs != vec![(tab1, ObservedLocation::Known(loc_a.clone()))] || tabs.pid == 0 {
        return Err(format!("tabs_of(A) = {tabs:?}"));
    }

    // 2. A new tab: tab_registered + TabCreated, then its navigations.
    post(top, WM_COMMAND, CMD_NEW_TAB)?;
    let tab2 = wait(STEP, || {
        rec.find(|e| match e {
            Event::TabCreated { win: w, tab } if *w == win => Some(*tab),
            _ => None,
        })
    })
    .map(|(_, t)| t)
    .ok_or("no TabCreated for the new tab")?;
    if !rec.registered.borrow().contains(&(win, tab2)) {
        return Err("tab_registered not called".into());
    }
    let home = wait(STEP, || {
        rec.find(|e| match e {
            Event::NavigationCompleted { tab, location } if *tab == tab2 => {
                Some(location.clone())
            }
            _ => None,
        })
    })
    .ok_or("no NavigationCompleted for the new tab")?
    .1;
    println!("new tab {tab2:?} first showed {}", name(&home));
    let browser = browser_of(tab2).ok_or("new tab's browser not found")?;
    navigate(&browser, &dir_b)?;
    wait(STEP, || {
        rec.find(|e| match e {
            Event::NavigationCompleted { tab, location } if *tab == tab2 && *location == loc_b => {
                Some(())
            }
            _ => None,
        })
    })
    .ok_or("no NavigationCompleted for B")?;
    let tabs = source.tabs_of(win).map_err(|e| format!("tabs_of: {e:?}"))?;
    let want = vec![
        (tab1, ObservedLocation::Known(loc_a.clone())),
        (tab2, ObservedLocation::Known(loc_b.clone())),
    ];
    if tabs.tabs != want {
        return Err(format!("tabs_of(A) after navigation = {tabs:?}"));
    }
    if !source
        .windows()
        .map_err(|e| format!("windows: {e:?}"))?
        .contains(&win)
    {
        return Err("windows() misses window A".into());
    }

    // 3. Closing the selected tab revokes it and retires its key.
    post(top, WM_CLOSE, 0)?;
    wait(STEP, || {
        rec.find(|e| matches!(e, Event::TabRevoked { win: w, tab } if *w == win && *tab == tab2).then_some(()))
    })
    .ok_or("no TabRevoked for the closed tab")?;
    if tab_keys.get(tab2.hwnd) == Some(tab2) {
        return Err("the closed tab's key is still live".into());
    }
    let tabs = source.tabs_of(win).map_err(|e| format!("tabs_of: {e:?}"))?;
    if tabs.tabs.len() != 1 || tabs.tabs[0].0 != tab1 {
        return Err(format!("tabs_of(A) after close = {tabs:?}"));
    }

    // 4. A Targeted_Launch with a selected file.
    let file = dir_c.join("esr_pick.txt");
    std::fs::write(&file, b"esr").map_err(|e| format!("write {}: {e}", file.display()))?;
    open_and_select(dir_c.clone(), file.clone())?;
    let (t_sel, win_c, selection) = wait_initial(guard, &rec, &loc_c)?;
    if let Some((t_nav_c, _)) = rec.find(|e| {
        matches!(e, Event::NavigationCompleted { location, .. } if *location == loc_c).then_some(())
    }) {
        println!(
            "  C: NavigationCompleted {} ms before the announcement",
            (t_sel - t_nav_c).as_millis()
        );
    }
    let names: Vec<String> = selection
        .iter()
        .filter_map(|i| parsing_name(&i.0))
        .map(|n| normalize(&n))
        .collect();
    println!(
        "window C {win_c:?}: InitialLocationKnown at {} ms, selection {names:?}",
        t_sel.as_millis()
    );
    if names != vec![normalize(&file.to_string_lossy())] {
        return Err(format!("selection {names:?}, expected {}", file.display()));
    }
    let tabs_c = source.tabs_of(win_c).map_err(|e| format!("tabs_of(C): {e:?}"))?;
    if tabs_c.tabs.len() != 1 || tabs_c.tabs[0].1 != ObservedLocation::Known(loc_c.clone()) {
        return Err(format!("tabs_of(C) = {tabs_c:?}"));
    }
    let tab_c = tabs_c.tabs[0].0;

    // 5. Simulated shell loss: ShellLost, then the poll reconnects and rescans.
    rec.events.borrow_mut().clear();
    let mark = rec.now();
    source.shell_disconnected();
    if !matches!(source.connection(), Connection::Lost { reported: true }) {
        return Err(format!("connection after loss {:?}", source.connection()));
    }
    if rec.count(|e| *e == Event::ShellLost) != 1 {
        return Err("ShellLost not posted".into());
    }
    if source.tabs_of(win).is_ok() {
        return Err("tabs_of succeeded while lost".into());
    }
    let t0 = Instant::now();
    wait(Duration::from_secs(5), || {
        rec.find(|e| (*e == Event::ShellAvailable).then_some(()))
    })
    .ok_or("no ShellAvailable within 5 s")?;
    println!("reconnected after {:?}", t0.elapsed());
    if source.connection() != Connection::Attached {
        return Err(format!("connection after reconnect {:?}", source.connection()));
    }
    let rescanned: Vec<WinKey> = rec
        .changed
        .borrow()
        .iter()
        .filter(|(t, _)| *t >= mark)
        .map(|(_, w)| *w)
        .collect();
    if !rescanned.contains(&win) || !rescanned.contains(&win_c) {
        return Err(format!("reconnect rescanned {rescanned:?}"));
    }
    if rec.count(|e| !matches!(e, Event::ShellLost | Event::ShellAvailable)) != 0 {
        rec.dump();
        return Err("the reconnection posted tab events".into());
    }
    let tabs = source.tabs_of(win).map_err(|e| format!("tabs_of: {e:?}"))?;
    if tabs.tabs != vec![(tab1, ObservedLocation::Known(loc_a.clone()))] {
        return Err(format!("tabs_of(A) after reconnect = {tabs:?}"));
    }

    // 6. Sinks are attached again: a new tab is reported.
    post(top, WM_COMMAND, CMD_NEW_TAB)?;
    let tab3 = wait(STEP, || {
        rec.find(|e| match e {
            Event::TabCreated { win: w, tab } if *w == win => Some(*tab),
            _ => None,
        })
    })
    .map(|(_, t)| t)
    .ok_or("no TabCreated after reconnecting")?;
    wait(STEP, || {
        rec.find(|e| matches!(e, Event::NavigationCompleted { tab, .. } if *tab == tab3).then_some(()))
    })
    .ok_or("no NavigationCompleted after reconnecting")?;
    println!("events since the reconnection:");
    rec.dump();
    rec.events.borrow_mut().clear();

    // 7. Closing a window revokes exactly its tabs; the other window's tab
    //    is neither revoked nor re-created meanwhile.
    for (w, expected) in [(win, vec![tab1, tab3]), (win_c, vec![tab_c])] {
        let hwnd = hwnd_of(w.hwnd);
        if !guard.close(hwnd) {
            return Err(format!("window {w:?} did not close"));
        }
        pump(Duration::from_millis(500));
        let mut revoked: Vec<TabKey> = rec
            .events
            .borrow()
            .iter()
            .filter_map(|(_, e)| match e {
                Event::TabRevoked { tab, .. } => Some(*tab),
                _ => None,
            })
            .collect();
        let created = rec.count(|e| matches!(e, Event::TabCreated { .. }));
        rec.events.borrow_mut().clear();
        println!("closed {w:?}: revoked {revoked:?}");
        if created != 0 {
            return Err(format!("closing {w:?} re-created {created} tab(s)"));
        }
        revoked.sort();
        let mut expected = expected;
        expected.sort();
        if revoked != expected {
            return Err(format!("closing {w:?} revoked {revoked:?}, expected {expected:?}"));
        }
    }
    let left = source.windows().map_err(|e| format!("windows: {e:?}"))?;
    if left.contains(&win) || left.contains(&win_c) {
        return Err(format!("closed windows still listed: {left:?}"));
    }

    source.detach();
    if source.connection() != Connection::Detached || source.tab_count() != 0 {
        return Err("detach left state behind".into());
    }
    Ok(())
}
