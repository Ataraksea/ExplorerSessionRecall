//! Integration test for the debounced `WindowScanner` (task 13.7) against
//! real File Explorer.
//!
//! ```text
//! cargo test -p esr-win --test scanner_integration -- --ignored --nocapture --test-threads=1
//! ```
//!
//! The scanner is wired as in the runtime: `ShellWindowsSource` lists tabs
//! and signals tab registrations, `TabStripReader` reads the UIA strip and
//! signals strip changes through a `ScanRequester`. One Explorer window is
//! opened on a temp folder (through `ExplorerGuard`, which closes it again).
//! The test checks that:
//!
//! * the new window is scanned with its tab, Location, UIA strip, pid, and
//!   placement (`GetWindowPlacement` in screen coordinates);
//! * appending a tab yields a scan with two tabs and the new tab active;
//! * after activating the first tab through UIA, a signal yields a scan
//!   within 1 s whose active hint (topmost visible `ShellTabWindowClass`)
//!   agrees with the UIA selection;
//! * a burst of signals collapses into one scan.
//!
//! Requirements 2.1, 2.2, 2.3, 2.4, 2.9.

#![cfg(windows)]

mod common;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use common::explorer_guard::{ExplorerGuard, parsing_name, pump_once};
use esr_core::engine::{Event, ObservedLocation, TabKey, WinKey, WindowObservation};
use esr_core::model::LocationId;
use esr_win::keys::{TabKeys, WindowKeys, hwnd_value};
use esr_win::location::capture_from_pidl;
use esr_win::scanner::{
    DebounceConfig, ScanHandler, ScanRequester, StripSource, TabEnumerator, WindowScanner,
};
use esr_win::shell_util::{StaGuard, current_folder_pidl, enumerate_all_explorer_windows};
use esr_win::shell_windows_source::{ShellSourceHandler, ShellWindowsSource};
use esr_win::tab_strip::{ChangeCallback, TabStripReader};
use esr_win::window_event_source::window_placement;
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
        std::thread::sleep(Duration::from_millis(5));
    }
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

/// Forwards the source's `window_changed` to the scanner (once it exists).
struct SourceHandler {
    requester: Arc<OnceLock<ScanRequester>>,
}

impl ShellSourceHandler for SourceHandler {
    fn post(&self, _event: Event) {}
    fn window_changed(&self, win: WinKey) {
        if let Some(r) = self.requester.get() {
            r.window_changed(win);
        }
    }
}

/// Records every observation with the time it arrived.
#[derive(Default)]
struct Recorder {
    seen: RefCell<Vec<(Instant, WindowObservation)>>,
}

impl ScanHandler for Recorder {
    fn post(&self, event: Event) {
        match event {
            Event::WindowScanned(obs) => self.seen.borrow_mut().push((Instant::now(), obs)),
            other => panic!("unexpected event {other:?}"),
        }
    }
}

impl Recorder {
    /// The first observation of `win` after `since` that satisfies `f`.
    fn find(
        &self,
        win: WinKey,
        since: Instant,
        f: impl Fn(&WindowObservation) -> bool,
    ) -> Option<(Instant, WindowObservation)> {
        self.seen
            .borrow()
            .iter()
            .find(|(at, o)| *at >= since && o.win == win && f(o))
            .cloned()
    }

    fn count(&self, win: WinKey, since: Instant) -> usize {
        self.seen
            .borrow()
            .iter()
            .filter(|(at, o)| *at >= since && o.win == win)
            .count()
    }
}

fn names(obs: &WindowObservation) -> Vec<String> {
    obs.strip
        .iter()
        .flat_map(|s| s.names.iter())
        .map(|n| String::from_utf16_lossy(n.units()))
        .collect()
}

fn shows(obs: &WindowObservation, tab: TabKey, location: &LocationId) -> bool {
    obs.tabs
        .iter()
        .any(|t| t.tab == tab && t.location == ObservedLocation::Known(location.clone()))
}

#[test]
#[ignore = "opens File Explorer windows; run alone"]
fn scanner_reports_tabs_strip_active_and_placement() {
    let guard = ExplorerGuard::new("scanner_integration");
    let result = run(&guard);
    let cleanup = guard.finish();
    result.unwrap();
    cleanup.unwrap();
}

fn run(guard: &ExplorerGuard) -> Result<(), String> {
    let _sta = StaGuard::init().map_err(|e| e.to_string())?;
    let dir = guard.dir("esr_scan_alpha");

    let windows = WindowKeys::new();
    let tabs = TabKeys::new();
    let requester: Arc<OnceLock<ScanRequester>> = Arc::new(OnceLock::new());

    let source = Rc::new(RefCell::new(ShellWindowsSource::new(
        windows.clone(),
        tabs.clone(),
        Rc::new(SourceHandler {
            requester: Arc::clone(&requester),
        }),
    )));
    source
        .borrow_mut()
        .attach()
        .map_err(|e| format!("attach: {e:?}"))?;

    let strip_requester = Arc::clone(&requester);
    let on_change: ChangeCallback = Arc::new(move |win| {
        if let Some(r) = strip_requester.get() {
            r.window_changed(win);
        }
    });
    let reader = Arc::new(TabStripReader::start(on_change).map_err(|e| format!("UIA: {e:?}"))?);

    let recorder = Rc::new(Recorder::default());
    let scanner = WindowScanner::new(
        DebounceConfig::default(),
        windows.clone(),
        Rc::clone(&source) as Rc<dyn TabEnumerator>,
        Some(Arc::clone(&reader) as Arc<dyn StripSource>),
        Rc::clone(&recorder) as Rc<dyn ScanHandler>,
    )
    .map_err(|e| format!("scanner: {e:?}"))?;
    let _ = requester.set(scanner.requester());

    // 1. A new window is scanned with its tab, Location, strip, and placement.
    let start = Instant::now();
    request_open(dir.clone())?;
    let top = wait_for_window(guard, &dir).ok_or("the test window did not appear")?;
    let win = windows.key_of(hwnd_value(top));
    let alpha = match current_folder_pidl(
        &enumerate_all_explorer_windows()
            .map_err(|e| e.to_string())?
            .into_iter()
            .find(|g| g.top_hwnd == top)
            .ok_or("window vanished")?
            .tabs[0]
            .browser,
    )
    .map(|p| capture_from_pidl(&p))
    {
        Ok(ObservedLocation::Known(l)) => l,
        other => return Err(format!("alpha location: {other:?}")),
    };
    let (at, first) = wait(Duration::from_secs(15), || {
        recorder.find(win, start, |o| {
            o.tabs.len() == 1
                && matches!(o.tabs[0].location, ObservedLocation::Known(ref l) if *l == alpha)
                && o.strip.as_ref().is_some_and(|s| s.names.len() == 1)
        })
    })
    .ok_or_else(|| {
        format!(
            "no complete scan of the new window; last: {:?}",
            recorder
                .seen
                .borrow()
                .iter()
                .rev()
                .find(|(_, o)| o.win == win)
        )
    })?;
    println!(
        "first full scan after {:?}: pid {} tabs {:?} strip {:?} active {:?} placement {:?}",
        at - start,
        first.pid,
        first.tabs.iter().map(|t| t.tab).collect::<Vec<_>>(),
        names(&first),
        first.active,
        first.placement
    );
    let tab_a = first.tabs[0].tab;
    if first.pid == 0 {
        return Err("pid 0".into());
    }
    if first.active != Some(tab_a) {
        return Err(format!("single tab not active: {:?}", first.active));
    }
    if first.strip.as_ref().and_then(|s| s.selected) != Some(0) {
        return Err(format!("strip selection: {:?}", first.strip));
    }
    let direct = window_placement(top).map_err(|e| format!("placement: {e:?}"))?;
    if first.placement != direct {
        return Err(format!("placement {:?} != {direct:?}", first.placement));
    }
    if !first.placement.restored.is_positive() {
        return Err(format!("empty placement {:?}", first.placement));
    }

    // 2. Appending a tab: two tabs, the new one selected and active.
    let since = Instant::now();
    // SAFETY: posting a plain message to the adopted test window.
    unsafe { PostMessageW(Some(top), WM_COMMAND, WPARAM(CMD_NEW_TAB), LPARAM(0)) }
        .map_err(|e| format!("PostMessageW: {e}"))?;
    let (at, two) = wait(Duration::from_secs(10), || {
        recorder.find(win, since, |o| {
            o.tabs.len() == 2
                && o.strip
                    .as_ref()
                    .is_some_and(|s| s.names.len() == 2 && s.selected == Some(1))
                && o.tabs
                    .iter()
                    .all(|t| !matches!(t.location, ObservedLocation::NotNavigated))
        })
    })
    .ok_or("no scan with the appended tab")?;
    let tab_b = two
        .tabs
        .iter()
        .map(|t| t.tab)
        .find(|t| *t != tab_a)
        .ok_or("the appended tab is missing")?;
    println!(
        "two-tab scan after {:?}: strip {:?} selected {:?} active {:?} (new tab {tab_b:?})",
        at - since,
        names(&two),
        two.strip.as_ref().and_then(|s| s.selected),
        two.active
    );
    if !shows(&two, tab_a, &alpha) {
        return Err("the first tab lost its Location".into());
    }
    if two.active != Some(tab_b) {
        return Err(format!(
            "active hint {:?} disagrees with the UIA selection (new tab {tab_b:?})",
            two.active
        ));
    }

    // 3. Activate the first tab through UIA, signal, and expect a scan within 1 s.
    reader
        .select(win, 0)
        .map_err(|e| format!("select(0): {e:?}"))?;
    wait(Duration::from_secs(2), || {
        reader
            .read(win)
            .ok()
            .filter(|s| s.selected == Some(0))
            .map(|_| ())
    })
    .ok_or("tab 0 did not become selected")?;
    let since = Instant::now();
    scanner.requester().window_changed(win);
    let (at, back) = wait(Duration::from_secs(3), || {
        recorder.find(win, since, |o| {
            o.strip.as_ref().is_some_and(|s| s.selected == Some(0))
        })
    })
    .ok_or("no scan after activating tab 0")?;
    let latency = at - since;
    println!(
        "scan after activation: {latency:?}, active {:?}",
        back.active
    );
    if latency > Duration::from_secs(1) {
        return Err(format!("scan took {latency:?}"));
    }
    if back.active != Some(tab_a) {
        return Err(format!(
            "active hint {:?} disagrees with the UIA selection (first tab {tab_a:?})",
            back.active
        ));
    }

    // 4. A burst of signals collapses into one scan.
    wait(Duration::from_millis(600), || None::<()>);
    let since = Instant::now();
    for _ in 0..10 {
        scanner.window_changed(win);
        std::thread::sleep(Duration::from_millis(5));
    }
    wait(Duration::from_millis(1200), || None::<()>);
    let scans = recorder.count(win, since);
    println!("10 signals within ~50 ms -> {scans} scan(s)");
    if scans != 1 {
        return Err(format!("expected one scan for the burst, got {scans}"));
    }
    if !scanner.is_idle() {
        return Err("scanner not idle after the burst".into());
    }

    drop(scanner);
    reader.unwatch(win);
    drop(reader);
    source.borrow_mut().detach();
    Ok(())
}
