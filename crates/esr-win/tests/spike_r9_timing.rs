//! Spike R9: time to create and navigate 30 local tabs (Req 7.1).
//!
//! ```text
//! cargo test -p esr-win --test spike_r9_timing -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Assumption under test (design.md, "Verification spikes", R9, and the timing
//! budget for 30 local tabs): the windows and tabs of a 30-tab session can be
//! created and navigated within 3 s, with tab creation pipelined per window
//! (post every `WM_COMMAND 0xA21B`, then navigate the new tabs by position).
//!
//! Method: 30 temp folders `r9_00..r9_29` are created under the guard's root.
//! Each run opens its own windows with `ShellExecuteExW("opennewwindow",
//! SEE_MASK_IDLIST, SW_SHOWNOACTIVATE)` (one launcher thread per window) on the
//! first folder of the window. A window is ready when ShellWindows shows it
//! with one tab on that folder and the UIA tab strip shows the folder's name.
//! Tabs are appended with `WM_COMMAND 0xA21B` posted to the top-level window and
//! navigated with `Navigate2(PIDL, navNoHistory)`; arrival is
//! `ILIsEqual(current folder, target)`.
//!
//! Variants:
//!
//! * `sequential`: windows one after another; per tab post `0xA21B`, wait for
//!   the tab to register in ShellWindows, navigate it, wait for arrival.
//! * `pipelined`: windows one after another; per window post every `0xA21B`
//!   back to back, wait until all new tabs register, then navigate the new tabs
//!   by position (initial tab = slot 0, new tabs in registration order). Each
//!   new tab is navigated once its first view exists.
//! * `pipelined-parallel`: every window requested at once, then all `0xA21B`
//!   for all windows posted, then all tabs navigated (round-robin over windows).
//! * `overlapped-parallel` (added after the first runs): every window requested
//!   at once; per window one outstanding `0xA21B` at a time, each new tab
//!   navigated as soon as it registers (no wait for arrival) and the next
//!   `0xA21B` posted right away; all arrivals awaited at the end.
//!
//! Layouts: 3 windows x 10 tabs (the budget layout) and 1 window x 30 tabs
//! (extra datapoint). The timer runs from the first window-open request until
//! every tab shows its folder (`LocationURL`, confirmed by PIDL afterwards);
//! the breakdown is window open (request -> ShellWindows and UIA show the
//! window), tab creation (post -> ShellWindows registration) and navigation
//! (Navigate2 -> arrival). After the timer the UIA order is verified per
//! window. If it is a permutation of the expected order, the slots are
//! re-navigated by UIA position (what the restore engine does) and that time
//! is added to the run's "effective" time. Finally a 500 ms settle checks that
//! no tab reverted and the order still holds.
//!
//! Polling is kept cheap so it does not slow Explorer down: new tabs are found
//! from `IShellWindows::Count` plus the newly appended indices (`NewTabs`),
//! arrival from `LocationURL`, and UIA reads use a cached tab-strip element.
//!
//! Every configuration runs `ESR_R9_RUNS` times (default 3), interleaved
//! (`ESR_R9_CONFIGS=0,5` selects a subset by index, `ESR_R9_DIAG=1` adds
//! header diagnostics on failure). `ESR_R9_HIDE=1` opens the windows with
//! `SW_HIDE` and `ESR_R9_SINKS=1` attaches a no-op `DWebBrowserEvents2` sink
//! to every tab, as the product does (task 16.3: on 22621, 3x10
//! overlapped-parallel took 5.8/6.8 s visible, 5.0/5.4 s hidden, and
//! 5.0/5.4 s hidden with sinks; create 4.5 / 3.8-4.0 / 3.8 s). The
//! assertion: the best 3 x 10 variant
//! (lowest median effective time among variants whose runs all succeeded with
//! the correct final order) has a median effective time of at most 3 s.
//!
//! Findings (Windows 11 22H2, build 22621.3880, Hyper-V VM with 12 logical
//! processors, 1 monitor 2500x1572 at 175 %; 3 runs each; median (min) ms):
//!
//! | variant | layout | effective | open | create | nav | order |
//! |---|---|---|---|---|---|---|
//! | sequential | 3x10 | 5878 (5385) | 1913 | 2260 | 1674 | ok 3/3 |
//! | pipelined | 3x10 | 4044 (4004), headers broken | 1827 | 1241 | 918 | fail 3/3 |
//! | pipelined-parallel | 3x10 | 3776 (3560), headers broken | 1113 | 1423 | 1231 | fail 3/3 |
//! | overlapped-parallel | 3x10 | 5311 (4857) | 1162 | 4029 (incl. nav) | 163 | ok 3/3 |
//! | sequential | 1x30 | 4205 (4175) | 434 | 2160 | 1604 | ok 3/3 |
//! | pipelined | 1x30 | 3699 (2566), headers broken | 1246 | 1574 | 858 | fail 3/3 |
//! | overlapped-parallel | 1x30 | 4026 (3529) | 801 | 3102 (incl. nav) | 81 | ok 3/3 |
//!
//! ("headers broken" rows: time until every tab showed its folder; the runs
//! failed verification. "nav" for overlapped is the tail after the last tab
//! registered.)
//!
//! * **The 3 s budget is not met** by any correct variant: ~5.3 s for 3x10,
//!   ~4.0 s for 1x30. One `0xA21B` takes ~84 ms to register and one
//!   navigation ~62 ms; a window opens in ~430-640 ms.
//! * **Explorer does not parallelise across windows**: three windows opened at
//!   once take ~1.1 s (vs ~0.43 s for one), and overlapped creation in three
//!   windows at once needs ~150 ms per tab overall, the same as one window
//!   after another. The only gain of parallel windows is the window opening.
//! * **Back-to-back `0xA21B` posts break tab headers**: every tab but the first
//!   new one keeps the header (UIA name) "Home" for good, although the tab
//!   shows its folder (`LocationName`, `LocationURL`, PIDL and the window title
//!   are right). Waiting for each tab's first view before navigating, waiting
//!   longer, selecting each tab, and navigating again do not repair it. With
//!   one outstanding `0xA21B` per window (sequential or overlapped) headers are
//!   correct. So `Pipelined` is not usable.
//! * No variant ever reordered tabs: UIA order always equalled creation order.
//! * One `0xA21B` in ~200 sequential creations produced no tab; re-posting
//!   after 1.5 s is needed (the variants do this and count it).
//! * UIA: a descendant search for the tab strip costs ~0.8 s for a 10-tab
//!   window and ~2.4 s for a 30-tab window (it walks the file view); reading
//!   the children of a cached strip element costs 2-7 ms. With 30 tabs the
//!   overflowing strip virtualizes items: only 18-24 TabItems are exposed, so a
//!   full order check via UIA is not possible there. A cached strip element can
//!   briefly return a short list right after tab creation (~350 ms here).
//! * `IWebBrowser2::Quit` on tabs from the test thread did not close them;
//!   `WM_CLOSE` closes one tab per message.
//!
//! Window safety: `ExplorerGuard` (`tests/common`) adopts every window as soon
//! as it shows one of the test's folders. Each run's windows are closed (all
//! tabs) before the next run starts, so at most 3 test windows exist at a time;
//! the guard sweeps and closes anything left on success, failure or panic.
//! Windows that existed before the test are never touched. Run this test alone.

#![cfg(windows)]

mod common;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use common::explorer_guard::{ExplorerGuard, parsing_name, pump_for, pump_once};
use esr_win::shell_util::{
    StaGuard, current_folder_pidl, enumerate_explorer_windows, pidl_to_bytes, shell_browser,
    shell_windows, tab_hwnd, top_level_hwnd,
};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::Ole::{
    SafeArrayAccessData, SafeArrayCreateVector, SafeArrayUnaccessData,
};
use windows::Win32::System::Variant::{
    VARENUM, VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_ARRAY, VT_UI1,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationCacheRequest, IUIAutomationCondition,
    IUIAutomationElement, IUIAutomationSelectionItemPattern, IUIAutomationTreeWalker,
    TreeScope_Children, TreeScope_Descendants, UIA_ClassNamePropertyId, UIA_ControlTypePropertyId,
    UIA_NamePropertyId, UIA_SelectionItemPatternId, UIA_TabItemControlTypeId,
};
use windows::Win32::UI::Shell::{
    ILCreateFromPathW, ILFree, ILIsEqual, IShellWindows, IWebBrowser2, SEE_MASK_FLAG_NO_UI,
    SEE_MASK_IDLIST, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EVENT_OBJECT_CREATE, EVENT_OBJECT_HIDE, EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_NAMECHANGE,
    EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART,
    EVENT_SYSTEM_MOVESIZEEND, GetShellWindow, GetWindowTextW, GetWindowThreadProcessId, IsWindow,
    PostMessageW, SW_SHOWNOACTIVATE, WINEVENT_OUTOFCONTEXT,
    WM_COMMAND,
};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::core::{BSTR, HSTRING, Interface, w};
use windows::Win32::Foundation::E_NOTIMPL;
use windows::Win32::System::Com::{
    DISPATCH_FLAGS, DISPPARAMS, EXCEPINFO, IConnectionPoint, IConnectionPointContainer,
    IDispatch_Impl, ITypeInfo,
};
use windows::Win32::UI::Shell::{DWebBrowserEvents2, DWebBrowserEvents2_Impl};

/// The undocumented "new tab" command of the Explorer frame.
const CMD_NEW_TAB: usize = 0xA21B;
/// `BrowserNavConstants::navNoHistory`.
const NAV_NO_HISTORY: i32 = 0x2;
/// `UIA_E_ELEMENTNOTAVAILABLE`, seen transiently from `ElementFromHandle`.
const UIA_E_ELEMENTNOTAVAILABLE: u32 = 0x8004_0201;
/// Req 7.1 for the 3 x 10 layout: 0.3 s x 3 windows + 0.27 s x 30 tabs
/// (re-based in task 16.3, last in round 4d; the spike's original target
/// was 3 s).
const BUDGET_MS: f64 = 9000.0;
const FOLDER_COUNT: usize = 30;
const OPEN_TIMEOUT: Duration = Duration::from_secs(20);
const STEP_TIMEOUT: Duration = Duration::from_secs(5);
/// A `0xA21B` that has not produced a tab after this long is posted again.
const REPOST_AFTER: Duration = Duration::from_millis(1500);
const BULK_TIMEOUT: Duration = Duration::from_secs(15);
/// Poll interval: short enough for timing, long enough not to flood Explorer's
/// UI threads with cross-process calls.
const POLL: Duration = Duration::from_millis(5);

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn hwnd(raw: isize) -> HWND {
    HWND(raw as *mut _)
}

fn raw(h: HWND) -> isize {
    h.0 as isize
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn is_window(top: isize) -> bool {
    // SAFETY: IsWindow tolerates stale HWNDs.
    unsafe { IsWindow(Some(hwnd(top))) }.as_bool()
}

/// Polls `f` (pumping messages) until true; returns the elapsed time.
fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> Option<Duration> {
    let start = Instant::now();
    loop {
        pump_once();
        if f() {
            return Some(start.elapsed());
        }
        if start.elapsed() > timeout {
            return None;
        }
        std::thread::sleep(POLL);
    }
}

fn median(values: &[f64]) -> f64 {
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.total_cmp(b));
    match v.len() {
        0 => f64::NAN,
        n if n % 2 == 1 => v[n / 2],
        n => (v[n / 2 - 1] + v[n / 2]) / 2.0,
    }
}

fn min(values: &[f64]) -> f64 {
    values.iter().copied().fold(f64::INFINITY, f64::min)
}

// ---------------------------------------------------------------------------
// Folders, PIDLs, navigation
// ---------------------------------------------------------------------------

struct Folder {
    path: PathBuf,
    pidl: Vec<u8>,
    /// Leaf name (= the UIA tab name on 22H2).
    name: String,
}

fn pidl_of_path(path: &Path) -> Result<Vec<u8>, String> {
    // SAFETY: valid path string; the PIDL is freed after copying.
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

fn at_folder(browser: &IWebBrowser2, target: &[u8]) -> bool {
    match current_folder_pidl(browser) {
        // SAFETY: both buffers are terminated PIDLs.
        Ok(cur) => unsafe { ILIsEqual(cur.as_ptr().cast(), target.as_ptr().cast()) }.as_bool(),
        Err(_) => false,
    }
}

fn location_url(browser: &IWebBrowser2) -> String {
    // SAFETY: plain COM property read.
    unsafe { browser.LocationURL() }
        .map(|s| s.to_string())
        .unwrap_or_default()
}

fn location_name(browser: &IWebBrowser2) -> String {
    // SAFETY: plain COM property read.
    unsafe { browser.LocationName() }
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// Cheap arrival check for polling (one cross-process call instead of the four
/// of `at_folder`): the tab's `LocationURL` ends with the folder's unique leaf
/// name. Final states are confirmed with `at_folder`.
fn shows(browser: &IWebBrowser2, f: &Folder) -> bool {
    let url = location_url(browser).to_lowercase();
    url.trim_end_matches('/')
        .ends_with(&format!("/{}", f.name.to_lowercase()))
}

/// Whether a new tab has finished creating its first view (its initial
/// navigation to the default location), cheaply.
fn has_view(browser: &IWebBrowser2) -> bool {
    shell_browser(browser)
        // SAFETY: plain COM call on a live interface.
        .and_then(|sb| unsafe { sb.QueryActiveShellView() })
        .is_ok()
}

/// `Navigate2(PIDL as VT_ARRAY|VT_UI1, navNoHistory)`; does not wait.
fn navigate(browser: &IWebBrowser2, pidl: &[u8]) -> Result<(), String> {
    let flags = VARIANT::from(NAV_NO_HISTORY);
    // Optional [in] VARIANT* parameters must be empty VARIANTs, never None.
    let empty = VARIANT::default();
    // SAFETY: the SAFEARRAY is created and filled here and passed to Navigate2.
    unsafe {
        let psa = SafeArrayCreateVector(VT_UI1, 0, pidl.len() as u32);
        if psa.is_null() {
            return Err("SafeArrayCreateVector failed".into());
        }
        let mut data: *mut c_void = std::ptr::null_mut();
        SafeArrayAccessData(psa, &mut data).map_err(|e| e.to_string())?;
        std::ptr::copy_nonoverlapping(pidl.as_ptr(), data.cast::<u8>(), pidl.len());
        SafeArrayUnaccessData(psa).map_err(|e| e.to_string())?;
        let url = VARIANT {
            Anonymous: VARIANT_0 {
                Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                    vt: VARENUM(VT_ARRAY.0 | VT_UI1.0),
                    wReserved1: 0,
                    wReserved2: 0,
                    wReserved3: 0,
                    Anonymous: VARIANT_0_0_0 { parray: psa },
                }),
            },
        };
        // Finding from R2: SafeArrayDestroy(psa) after Navigate2 corrupts the heap,
        // so the array is intentionally not destroyed (and `url` is never dropped).
        browser
            .Navigate2(&url, Some(&flags), Some(&empty), Some(&empty), Some(&empty))
            .map_err(|e| format!("Navigate2: {e}"))
    }
}

fn post_new_tab(top: isize) -> Result<(), String> {
    // SAFETY: posting a plain message to an adopted test window.
    unsafe { PostMessageW(Some(hwnd(top)), WM_COMMAND, WPARAM(CMD_NEW_TAB), LPARAM(0)) }
        .map_err(|e| format!("PostMessageW(0xA21B): {e}"))
}

thread_local! {
    /// One ShellWindows collection for the test thread (created once).
    static SHELL_WINDOWS: IShellWindows = shell_windows().expect("create ShellWindows");
}

/// Number of ShellWindows entries: one call to the ShellWindows server, which
/// does not touch the Explorer windows' UI threads (cheap to poll).
fn shell_count() -> i32 {
    // SAFETY: plain COM property read.
    SHELL_WINDOWS.with(|sw| unsafe { sw.Count() }.unwrap_or(-1))
}

fn enumerate() -> Vec<esr_win::shell_util::ShellWindowGroup> {
    SHELL_WINDOWS.with(|sw| enumerate_explorer_windows(sw).unwrap_or_default())
}

/// ShellWindows tabs per top-level window: `(top, [(tab HWND, browser)])`.
/// Tabs whose HWND cannot be resolved yet are left out.
fn tabs_by_top() -> Vec<(isize, Vec<(isize, IWebBrowser2)>)> {
    enumerate()
        .into_iter()
        .map(|g| {
            let tabs = g
                .tabs
                .into_iter()
                .filter_map(|t| t.tab_hwnd.map(|h| (raw(h), t.browser)))
                .collect();
            (raw(g.top_hwnd), tabs)
        })
        .collect()
}

fn describe_browsers(w: &WinState) -> Vec<(String, String)> {
    w.browsers
        .iter()
        .map(|b| (location_name(b), location_url(b)))
        .collect()
}

/// The browser of ShellWindows entry `index`.
fn shell_item(index: i32) -> Option<IWebBrowser2> {
    SHELL_WINDOWS.with(|sw| {
        // SAFETY: `VARIANT::from(i32)` is a VT_I4 VARIANT owning no resources.
        unsafe { sw.Item(&VARIANT::from(index)) }
            .ok()
            .and_then(|d| d.cast::<IWebBrowser2>().ok())
    })
}

/// Detects newly registered tabs cheaply: new ShellWindows entries are
/// appended, so after `Count` grows only the new indices are read (three
/// calls each) instead of enumerating every tab of every window. If an entry
/// at a new index is a tab seen before (the collection was compacted or
/// reordered), it falls back to one full enumeration.
struct NewTabs {
    next: i32,
    seen: HashSet<isize>,
}

impl NewTabs {
    fn start() -> Self {
        let seen = tabs_by_top()
            .into_iter()
            .flat_map(|(_, tabs)| tabs.into_iter().map(|(h, _)| h))
            .collect();
        Self {
            next: shell_count(),
            seen,
        }
    }

    /// New tabs since the last call, in registration order: `(top, tab, browser)`.
    fn poll(&mut self) -> Vec<(isize, isize, IWebBrowser2)> {
        let count = shell_count();
        let mut out = Vec::new();
        while self.next < count {
            let Some(b) = shell_item(self.next) else {
                break;
            };
            let (Some(top), Ok(tab)) = (top_level_hwnd(&b), tab_hwnd(&b)) else {
                break; // not fully registered yet; retry on the next poll
            };
            if self.seen.contains(&raw(tab)) {
                return self.full_scan(out, count);
            }
            self.seen.insert(raw(tab));
            out.push((raw(top), raw(tab), b));
            self.next += 1;
        }
        out
    }

    fn full_scan(
        &mut self,
        mut out: Vec<(isize, isize, IWebBrowser2)>,
        count: i32,
    ) -> Vec<(isize, isize, IWebBrowser2)> {
        for (top, tabs) in tabs_by_top() {
            for (h, b) in tabs {
                if self.seen.insert(h) {
                    out.push((top, h, b));
                }
            }
        }
        self.next = count;
        out
    }
}

fn shell_tabs(top: isize) -> Vec<(isize, IWebBrowser2)> {
    tabs_by_top()
        .into_iter()
        .find(|(t, _)| *t == top)
        .map(|(_, tabs)| tabs)
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// UIA (tab strip order and names)
// ---------------------------------------------------------------------------

struct Uia {
    automation: IUIAutomation,
    tab_items: IUIAutomationCondition,
    /// TabItem with class `ListViewItem` (a tab strip item).
    strip_item: IUIAutomationCondition,
    walker: IUIAutomationTreeWalker,
    cache: IUIAutomationCacheRequest,
    /// Tab strip element per top-level window (see `try_names`).
    strips: RefCell<HashMap<isize, IUIAutomationElement>>,
}

impl Uia {
    fn new() -> windows::core::Result<Self> {
        // SAFETY: valid CLSID; in-process server; plain UIA calls.
        unsafe {
            let automation: IUIAutomation =
                CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
            let tab_items = automation.CreatePropertyCondition(
                UIA_ControlTypePropertyId,
                &VARIANT::from(UIA_TabItemControlTypeId.0),
            )?;
            let list_view_item = automation.CreatePropertyCondition(
                UIA_ClassNamePropertyId,
                &VARIANT::from(BSTR::from("ListViewItem")),
            )?;
            let strip_item = automation.CreateAndCondition(&tab_items, &list_view_item)?;
            let walker = automation.RawViewWalker()?;
            let cache = automation.CreateCacheRequest()?;
            cache.AddProperty(UIA_NamePropertyId)?;
            cache.AddProperty(UIA_ClassNamePropertyId)?;
            Ok(Self {
                automation,
                tab_items,
                strip_item,
                walker,
                cache,
                strips: RefCell::new(HashMap::new()),
            })
        }
    }

    /// `ElementFromHandle`, retried while it reports `UIA_E_ELEMENTNOTAVAILABLE`.
    fn element(&self, top: isize) -> windows::core::Result<IUIAutomationElement> {
        let mut attempt = 0;
        loop {
            // SAFETY: plain UIA call; stale HWNDs just fail.
            match unsafe { self.automation.ElementFromHandle(hwnd(top)) } {
                Ok(e) => return Ok(e),
                Err(e) if e.code().0 as u32 == UIA_E_ELEMENTNOTAVAILABLE && attempt < 10 => {
                    attempt += 1;
                    pump_for(Duration::from_millis(50));
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Diagnostics: selects the `index`-th tab strip item (UIA order).
    fn select(&self, top: isize, index: usize) -> windows::core::Result<()> {
        let root = self.element(top)?;
        // SAFETY: plain UIA calls on live interfaces.
        unsafe {
            let all = root.FindAll(TreeScope_Descendants, &self.tab_items)?;
            let mut n = 0;
            for i in 0..all.Length()? {
                let el = all.GetElement(i)?;
                let class = el
                    .CurrentClassName()
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                if !class.contains("ListViewItem") || class.contains("NavigationViewItem") {
                    continue;
                }
                if n == index {
                    return el
                        .GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(
                            UIA_SelectionItemPatternId,
                        )?
                        .Select();
                }
                n += 1;
            }
            Ok(())
        }
    }

    /// Diagnostics: duration of each step of `try_names` for one window.
    fn step_times(&self, top: isize) -> String {
        let t = Instant::now();
        let root = self.element(top);
        let t_root = ms(t.elapsed());
        let Ok(root) = root else {
            return format!("ElementFromHandle failed after {t_root:.0} ms");
        };
        let t = Instant::now();
        // SAFETY: plain UIA calls on live interfaces.
        let first = unsafe { root.FindFirst(TreeScope_Descendants, &self.strip_item) };
        let t_first = ms(t.elapsed());
        let t = Instant::now();
        // SAFETY: as above.
        let strip = first
            .as_ref()
            .ok()
            .and_then(|f| unsafe { self.walker.GetParentElement(f) }.ok());
        let t_parent = ms(t.elapsed());
        let t = Instant::now();
        // SAFETY: as above.
        let n = strip.as_ref().and_then(|s| {
            unsafe { s.FindAllBuildCache(TreeScope_Children, &self.tab_items, &self.cache) }
                .ok()
                .and_then(|a| unsafe { a.Length() }.ok())
        });
        let t_all = ms(t.elapsed());
        format!(
            "ElementFromHandle {t_root:.0} ms, FindFirst {t_first:.0} ms ({}), parent {t_parent:.0} \
             ms, FindAll children {t_all:.0} ms ({n:?} items)",
            if first.is_ok() { "found" } else { "not found" }
        )
    }

    /// Names of the tab strip's items (`ListViewItem` TabItems) in UIA order.
    fn names(&self, top: isize) -> Vec<String> {
        self.try_names(top).unwrap_or_default()
    }

    fn try_names(&self, top: isize) -> windows::core::Result<Vec<String>> {
        // Finding the strip costs ~0.8 s per window here (a descendant search
        // walks into the file view), reading its children ~2 ms. So the strip
        // element (parent of the first `ListViewItem` TabItem) is found once
        // per window and cached; a stale cache entry is dropped and re-found.
        let cached = self.strips.borrow().get(&top).cloned();
        if let Some(strip) = cached {
            match self.strip_names(&strip) {
                Ok(names) if !names.is_empty() => return Ok(names),
                _ => {
                    self.strips.borrow_mut().remove(&top);
                }
            }
        }
        let root = self.element(top)?;
        // SAFETY: plain UIA calls on live interfaces.
        let strip = unsafe {
            let first = root.FindFirst(TreeScope_Descendants, &self.strip_item)?;
            self.walker.GetParentElement(&first)?
        };
        let names = self.strip_names(&strip)?;
        self.strips.borrow_mut().insert(top, strip);
        Ok(names)
    }

    fn strip_names(&self, strip: &IUIAutomationElement) -> windows::core::Result<Vec<String>> {
        // SAFETY: plain UIA calls on live interfaces.
        unsafe {
            let all = strip.FindAllBuildCache(TreeScope_Children, &self.tab_items, &self.cache)?;
            let mut names = Vec::new();
            for i in 0..all.Length()? {
                let el = all.GetElement(i)?;
                let class = el
                    .CachedClassName()
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                // The Home page also exposes NavigationViewItem TabItems.
                if !class.contains("ListViewItem") || class.contains("NavigationViewItem") {
                    continue;
                }
                names.push(el.CachedName().map(|s| s.to_string()).unwrap_or_default());
            }
            Ok(names)
        }
    }
}

/// Whether a UIA tab name denotes `f` (leaf name on 22H2, full path on newer builds).
fn uia_matches(name: &str, f: &Folder) -> bool {
    name.eq_ignore_ascii_case(&f.name)
        || name
            .trim_end_matches('\\')
            .eq_ignore_ascii_case(f.path.to_string_lossy().trim_end_matches('\\'))
}

/// The slot shown at each UIA position, if every exposed TabItem shows a
/// distinct slot of the window. The tab strip virtualizes items when it
/// overflows, so UIA may expose fewer TabItems than there are tabs; `full`
/// tells whether all were exposed.
struct UiaOrder {
    seen: Vec<usize>,
    full: bool,
}

impl UiaOrder {
    /// Full view: exactly the creation order. Partial view: a contiguous,
    /// increasing run of slots (the best that can be checked).
    fn in_order(&self) -> bool {
        if self.full {
            self.seen.iter().enumerate().all(|(i, &s)| i == s)
        } else {
            self.seen.windows(2).all(|w| w[1] == w[0] + 1)
        }
    }
}

fn uia_order(names: &[String], slots: &[usize], folders: &[Folder]) -> Option<UiaOrder> {
    if names.is_empty() || names.len() > slots.len() {
        return None;
    }
    let mut used = vec![false; slots.len()];
    let mut seen = Vec::with_capacity(names.len());
    for n in names {
        let s = (0..slots.len()).find(|&s| !used[s] && uia_matches(n, &folders[slots[s]]))?;
        used[s] = true;
        seen.push(s);
    }
    Some(UiaOrder {
        seen,
        full: names.len() == slots.len(),
    })
}

fn uia_in_order(uia: &Uia, w: &WinState, folders: &[Folder]) -> bool {
    uia_order(&uia.names(w.top), &w.slots, folders).is_some_and(|o| o.in_order())
}

// ---------------------------------------------------------------------------
// Windows of one run
// ---------------------------------------------------------------------------

struct WinState {
    top: isize,
    /// Folder index per slot (slot 0 = initial tab).
    slots: Vec<usize>,
    /// Browser per slot, filled as tabs are created.
    browsers: Vec<IWebBrowser2>,
    /// Tab HWNDs known so far.
    known: HashSet<isize>,
}

/// Requests a new Explorer window on `dir` from a launcher thread.
fn request_open(dir: PathBuf, index: usize) -> JoinHandle<Result<(), String>> {
    // ESR_R9_HIDE=1: open hidden like the product (task 16.3); `2`: the first
    // window visible and the others hidden, like a restoration whose trigger
    // window is visible.
    let hide = match std::env::var("ESR_R9_HIDE").as_deref() {
        Ok("1") => true,
        Ok("2") => index > 0,
        _ => false,
    };
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
                nShow: if hide {
                    windows::Win32::UI::WindowsAndMessaging::SW_HIDE.0
                } else {
                    SW_SHOWNOACTIVATE.0
                },
                ..Default::default()
            };
            let r = ShellExecuteExW(&mut sei);
            ILFree(Some(pidl));
            r.map_err(|e| format!("ShellExecuteExW: {e}"))
        }
    })
}

/// Opens one window per entry of `assign` (all requested at once), adopting
/// each as soon as it shows its folder; returns them once they are ready.
fn open_windows(
    guard: &ExplorerGuard,
    uia: &Uia,
    folders: &[Folder],
    assign: &[Vec<usize>],
    opened: &mut Vec<isize>,
) -> Result<Vec<WinState>, String> {
    let launchers: Vec<_> = assign
        .iter()
        .enumerate()
        .map(|(i, a)| request_open(folders[a[0]].path.clone(), i))
        .collect();
    let mut found: Vec<Option<(isize, isize, IWebBrowser2)>> = vec![None; assign.len()];
    wait_until(OPEN_TIMEOUT, || {
        for g in enumerate() {
            if guard.is_pre_existing(g.top_hwnd) {
                continue;
            }
            let top = raw(g.top_hwnd);
            if found.iter().flatten().any(|(t, ..)| *t == top) {
                continue;
            }
            for (i, a) in assign.iter().enumerate() {
                if found[i].is_some() {
                    continue;
                }
                let f = &folders[a[0]];
                let Some(tab) = g.tabs.iter().find(|t| at_folder(&t.browser, &f.pidl)) else {
                    continue;
                };
                // New and showing our temp folder: adopt at once.
                if !guard.is_adopted(g.top_hwnd) {
                    guard.adopt(g.top_hwnd);
                    opened.push(top);
                }
                // Explorer may replace a fresh window: accept it once the UIA
                // tab strip shows the folder too.
                if g.tabs.len() == 1
                    && let Some(h) = tab.tab_hwnd
                    && uia_order(&uia.names(top), &a[..1], folders).is_some()
                {
                    found[i] = Some((top, raw(h), tab.browser.clone()));
                }
                break;
            }
        }
        found.iter().all(Option::is_some)
    });
    for l in launchers {
        match l.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err("launcher thread panicked".into()),
        }
    }
    found
        .into_iter()
        .zip(assign)
        .map(|(f, a)| {
            let (top, tab, browser) =
                f.ok_or_else(|| format!("window on {} did not become ready", folders[a[0]].name))?;
            Ok(WinState {
                top,
                slots: a.clone(),
                browsers: vec![browser],
                known: HashSet::from([tab]),
            })
        })
        .collect()
}

/// Variant A: one tab at a time (post, wait for registration, navigate, wait).
/// Returns (creation ms, navigation ms).
fn create_sequential(
    w: &mut WinState,
    folders: &[Folder],
    repeated: &mut usize,
) -> Result<(f64, f64), String> {
    let (mut create, mut nav) = (0.0, 0.0);
    let mut scanner = NewTabs::start();
    for slot in 1..w.slots.len() {
        let t0 = Instant::now();
        post_new_tab(w.top)?;
        let mut new = None;
        let mut find = |new: &mut Option<(isize, IWebBrowser2)>| {
            *new = scanner
                .poll()
                .into_iter()
                .find(|(top, h, _)| *top == w.top && !w.known.contains(h))
                .map(|(_, h, b)| (h, b));
            new.is_some()
        };
        if wait_until(REPOST_AFTER, || find(&mut new)).is_none() {
            // Seen once in ~200 sequential creations: a post that never
            // produced a tab. Post again (an extra tab is caught by verify).
            println!("    slot {slot}: no tab after {REPOST_AFTER:?}, posting 0xA21B again");
            *repeated += 1;
            post_new_tab(w.top)?;
            wait_until(STEP_TIMEOUT, || find(&mut new))
                .ok_or_else(|| format!("slot {slot}: the new tab did not register"))?;
        }
        create += ms(t0.elapsed());
        let (h, browser) = new.expect("checked above");
        w.known.insert(h);
        let f = &folders[w.slots[slot]];
        let t1 = Instant::now();
        navigate(&browser, &f.pidl)?;
        wait_until(STEP_TIMEOUT, || shows(&browser, f))
            .ok_or_else(|| format!("slot {slot}: the tab did not reach {}", f.name))?;
        nav += ms(t1.elapsed());
        w.browsers.push(browser);
    }
    Ok((create, nav))
}

struct PipeTimes {
    /// First post -> every new tab registered in ShellWindows.
    create: f64,
    /// Registered -> every tab at its folder.
    nav: f64,
    /// Registered -> the last Navigate2 issued (waiting for each new tab's
    /// first view before navigating it).
    issued: f64,
    repeated: usize,
}

/// Variants B/C: post every `0xA21B` for every window, wait until all register,
/// then navigate the new tabs by position (ShellWindows registration order).
/// Each new tab is navigated as soon as its first view exists (that gate was
/// added to rule out a navigation race; it does not prevent the stale "Home"
/// headers that back-to-back posts cause, see the module findings).
fn create_pipelined(wins: &mut [WinState], folders: &[Folder]) -> Result<PipeTimes, String> {
    let mut scanner = NewTabs::start();
    let t0 = Instant::now();
    for w in wins.iter() {
        for _ in 1..w.slots.len() {
            post_new_tab(w.top)?;
        }
    }
    let mut regs: Vec<Vec<(isize, IWebBrowser2)>> = vec![Vec::new(); wins.len()];
    let mut extra = String::new();
    let mut collect = |regs: &mut Vec<Vec<(isize, IWebBrowser2)>>, extra: &mut String| {
        // New tabs in registration order.
        for (top, h, b) in scanner.poll() {
            if let Some(i) = wins.iter().position(|w| w.top == top)
                && !wins[i].known.contains(&h)
            {
                regs[i].push((h, b));
            }
        }
        let mut done = true;
        for (i, w) in wins.iter().enumerate() {
            let need = w.slots.len() - 1;
            if regs[i].len() > need {
                *extra = format!(
                    "window {:#x}: {} new tabs, expected {need}",
                    w.top,
                    regs[i].len()
                );
            }
            if regs[i].len() < need {
                done = false;
            }
        }
        done
    };
    let mut repeated = 0;
    let first = wait_until(STEP_TIMEOUT, || collect(&mut regs, &mut extra));
    if first.is_none() {
        collect(&mut regs, &mut extra);
        for (i, w) in wins.iter().enumerate() {
            let missing = (w.slots.len() - 1).saturating_sub(regs[i].len());
            repeated += missing;
            for _ in 0..missing {
                post_new_tab(w.top)?;
            }
        }
        wait_until(BULK_TIMEOUT, || collect(&mut regs, &mut extra)).ok_or_else(|| {
            format!(
                "new tabs did not all register: {:?}",
                regs.iter().map(Vec::len).collect::<Vec<_>>()
            )
        })?;
    }
    if !extra.is_empty() {
        return Err(extra);
    }
    let create = ms(t0.elapsed());

    let t1 = Instant::now();
    // Position order, round-robin over windows.
    let rounds = wins.iter().map(|w| w.slots.len() - 1).max().unwrap_or(0);
    let mut to_nav: Vec<(IWebBrowser2, usize)> = Vec::new();
    for k in 0..rounds {
        for (i, w) in wins.iter().enumerate() {
            if let Some((_, b)) = regs[i].get(k) {
                to_nav.push((b.clone(), w.slots[k + 1]));
            }
        }
    }
    let mut pending: Vec<(IWebBrowser2, usize)> = Vec::new();
    let mut issued = None;
    let mut nav_error = None;
    wait_until(BULK_TIMEOUT, || {
        let mut i = 0;
        while i < to_nav.len() {
            if has_view(&to_nav[i].0) {
                let (b, f) = to_nav.remove(i);
                if let Err(e) = navigate(&b, &folders[f].pidl) {
                    nav_error = Some(e);
                    return true;
                }
                pending.push((b, f));
            } else {
                i += 1;
            }
        }
        if to_nav.is_empty() && issued.is_none() {
            issued = Some(ms(t1.elapsed()));
        }
        pending.retain(|(b, f)| !shows(b, &folders[*f]));
        to_nav.is_empty() && pending.is_empty()
    })
    .ok_or_else(|| {
        format!(
            "{} tab(s) never got a view, {} did not reach their folder",
            to_nav.len(),
            pending.len()
        )
    })?;
    if let Some(e) = nav_error {
        return Err(e);
    }
    let nav = ms(t1.elapsed());
    for (w, r) in wins.iter_mut().zip(regs) {
        for (h, b) in r {
            w.known.insert(h);
            w.browsers.push(b);
        }
    }
    Ok(PipeTimes {
        create,
        nav,
        issued: issued.unwrap_or(nav),
        repeated,
    })
}

/// Variant D: chained creation with overlapped navigation, every window at
/// once. Per window: post `0xA21B`, wait until that tab registers (and, with
/// `gate_view`, has its first view), navigate it without waiting for arrival,
/// post the next one. Arrival of all tabs is awaited at the end.
fn create_overlapped(
    wins: &mut [WinState],
    folders: &[Folder],
    gate_view: bool,
) -> Result<PipeTimes, String> {
    let mut scanner = NewTabs::start();
    let mut sinks: Vec<(IConnectionPoint, u32)> = Vec::new();
    if sinks_on() {
        for w in wins.iter() {
            for b in &w.browsers {
                advise_nop_sink(b, &mut sinks);
            }
        }
    }
    let t0 = Instant::now();
    let mut next: Vec<usize> = vec![1; wins.len()];
    let mut waiting: Vec<Option<Option<(isize, IWebBrowser2)>>> = vec![None; wins.len()];
    let mut pending: Vec<(IWebBrowser2, usize)> = Vec::new();
    let mut last_registered = t0;
    let mut issued_at = t0;
    let mut error = None;
    let mut posted_at: Vec<Instant> = vec![t0; wins.len()];
    let mut repeated = 0;
    let done = wait_until(Duration::from_secs(30), || {
        // Post where nothing is outstanding (again if a post produced no tab).
        for (i, w) in wins.iter().enumerate() {
            let lost = matches!(waiting[i], Some(None)) && posted_at[i].elapsed() > REPOST_AFTER;
            if lost || (waiting[i].is_none() && next[i] < w.slots.len()) {
                if let Err(e) = post_new_tab(w.top) {
                    error = Some(e);
                    return true;
                }
                if lost {
                    repeated += 1;
                }
                posted_at[i] = Instant::now();
                waiting[i] = Some(None);
            }
        }
        // Registration.
        for (top, h, b) in scanner.poll() {
            if let Some(i) = wins.iter().position(|w| w.top == top)
                && matches!(waiting[i], Some(None))
                && !wins[i].known.contains(&h)
            {
                waiting[i] = Some(Some((h, b)));
                last_registered = Instant::now();
            }
        }
        // Navigate registered tabs (optionally once their first view exists).
        for (i, w) in wins.iter_mut().enumerate() {
            let Some(Some((h, b))) = &waiting[i] else {
                continue;
            };
            if gate_view && !has_view(b) {
                continue;
            }
            let (h, b) = (*h, b.clone());
            if sinks_on() {
                advise_nop_sink(&b, &mut sinks);
            }
            let f = w.slots[next[i]];
            if let Err(e) = navigate(&b, &folders[f].pidl) {
                error = Some(e);
                return true;
            }
            issued_at = Instant::now();
            w.known.insert(h);
            w.browsers.push(b.clone());
            pending.push((b, f));
            waiting[i] = None;
            next[i] += 1;
        }
        pending.retain(|(b, f)| !shows(b, &folders[*f]));
        pending.is_empty()
            && waiting.iter().all(Option::is_none)
            && wins.iter().zip(&next).all(|(w, &n)| n >= w.slots.len())
    });
    if let Some(e) = error {
        return Err(e);
    }
    done.ok_or_else(|| {
        format!(
            "overlapped creation incomplete: next slots {next:?}, {} tab(s) not arrived",
            pending.len()
        )
    })?;
    for (point, cookie) in sinks {
        // SAFETY: the cookie Advise returned.
        let _ = unsafe { point.Unadvise(cookie) };
    }
    let total = ms(t0.elapsed());
    let create = ms(last_registered - t0);
    Ok(PipeTimes {
        create,
        nav: total - create,
        issued: ms(issued_at - t0),
        repeated,
    })
}

/// `ESR_R9_SINKS=1`: attach a no-op `DWebBrowserEvents2` sink to every tab,
/// as the product's shell window source does (task 16.3).
fn sinks_on() -> bool {
    std::env::var("ESR_R9_SINKS").is_ok_and(|v| v == "1")
}

/// `ESR_R9_HOOKS=global,pid`: install the product's out-of-context WinEvent
/// hooks with a counting no-op callback for the whole test (task 16.3),
/// `global` (create..hide, foreground) and/or `pid` (move/size end,
/// minimize, location..name change on the shell's Explorer process).
struct Hooks(Vec<HWINEVENTHOOK>);

static HOOK_EVENTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

unsafe extern "system" fn count_event(_: HWINEVENTHOOK, _: u32, _: HWND, _: i32, _: i32, _: u32, _: u32) {
    HOOK_EVENTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

impl Hooks {
    fn from_env() -> Self {
        let wanted = std::env::var("ESR_R9_HOOKS").unwrap_or_default();
        let has = |k: &str| wanted.split(',').any(|w| w.trim() == k);
        let mut ranges: Vec<(u32, u32, u32)> = Vec::new();
        if has("global") {
            ranges.push((EVENT_OBJECT_CREATE, EVENT_OBJECT_HIDE, 0));
            ranges.push((EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, 0));
        }
        if has("pid") {
            let mut pid = 0;
            // SAFETY: valid out pointer.
            unsafe { GetWindowThreadProcessId(GetShellWindow(), Some(&mut pid)) };
            ranges.push((EVENT_SYSTEM_MOVESIZEEND, EVENT_SYSTEM_MOVESIZEEND, pid));
            ranges.push((EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND, pid));
            ranges.push((EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_NAMECHANGE, pid));
        }
        let hooks = ranges
            .into_iter()
            .map(|(min, max, pid)| {
                // SAFETY: out-of-context hook; the callback only counts.
                unsafe { SetWinEventHook(min, max, None, Some(count_event), pid, 0, WINEVENT_OUTOFCONTEXT) }
            })
            .filter(|h| !h.is_invalid())
            .collect::<Vec<_>>();
        if !hooks.is_empty() {
            println!("R9: {} WinEvent hook(s) installed ({wanted})", hooks.len());
        }
        Self(hooks)
    }
}

impl Drop for Hooks {
    fn drop(&mut self) {
        for h in self.0.drain(..) {
            // SAFETY: a hook this value installed.
            let _ = unsafe { UnhookWinEvent(h) };
        }
        let n = HOOK_EVENTS.load(std::sync::atomic::Ordering::Relaxed);
        if n > 0 {
            println!("R9: hooks received {n} event(s)");
        }
    }
}

#[windows::core::implement(DWebBrowserEvents2)]
struct NopSink;

impl DWebBrowserEvents2_Impl for NopSink_Impl {}

impl IDispatch_Impl for NopSink_Impl {
    fn GetTypeInfoCount(&self) -> windows::core::Result<u32> {
        Ok(0)
    }
    fn GetTypeInfo(&self, _: u32, _: u32) -> windows::core::Result<ITypeInfo> {
        Err(E_NOTIMPL.into())
    }
    fn GetIDsOfNames(
        &self,
        _: *const windows::core::GUID,
        _: *const windows::core::PCWSTR,
        _: u32,
        _: u32,
        _: *mut i32,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn Invoke(
        &self,
        _: i32,
        _: *const windows::core::GUID,
        _: u32,
        _: DISPATCH_FLAGS,
        _: *const DISPPARAMS,
        _: *mut VARIANT,
        _: *mut EXCEPINFO,
        _: *mut u32,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

fn advise_nop_sink(browser: &IWebBrowser2, out: &mut Vec<(IConnectionPoint, u32)>) {
    let sink: DWebBrowserEvents2 = NopSink.into();
    let Ok(container) = browser.cast::<IConnectionPointContainer>() else { return };
    // SAFETY: plain COM calls on live interfaces.
    unsafe {
        if let Ok(point) = container.FindConnectionPoint(&DWebBrowserEvents2::IID)
            && let Ok(cookie) = point.Advise(&sink)
        {
            out.push((point, cookie));
        }
    }
}

// ---------------------------------------------------------------------------
// Runs
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Variant {
    Sequential,
    Pipelined,
    PipelinedParallel,
    /// Chained creation, navigation overlapped with the next creation, all
    /// windows at once (see `create_overlapped`).
    OverlappedParallel,
}

impl Variant {
    fn label(self) -> &'static str {
        match self {
            Variant::Sequential => "sequential",
            Variant::Pipelined => "pipelined",
            Variant::PipelinedParallel => "pipelined-parallel",
            Variant::OverlappedParallel => "overlapped-parallel",
        }
    }

    fn pipelining(self) -> &'static str {
        match self {
            Variant::Sequential | Variant::OverlappedParallel => "Sequential",
            Variant::Pipelined | Variant::PipelinedParallel => "Pipelined",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Layout {
    windows: usize,
    tabs: usize,
}

#[derive(Clone, Debug, Default)]
struct RunResult {
    open_ms: f64,
    create_ms: f64,
    nav_ms: f64,
    /// Pipelined only: registered -> last Navigate2 issued (view gate).
    issued_ms: f64,
    /// First open request -> every tab at its folder.
    total_ms: f64,
    /// Timer stop -> UIA confirms the order (including any fix-up).
    uia_confirm_ms: Option<f64>,
    /// UIA order matched the creation order without a fix-up.
    order_ok: bool,
    fixed: bool,
    fix_ms: f64,
    /// `total_ms`, plus the time to confirm the repaired order when a fix-up
    /// was needed.
    effective_ms: f64,
    stable: bool,
    repeated_posts: usize,
    /// Fewest TabItems UIA exposed for one window, relative to its tab count
    /// (`(seen, tabs)`); less than the tab count means the strip virtualized.
    uia_seen: Option<(usize, usize)>,
    error: Option<String>,
}

impl RunResult {
    fn good(&self) -> bool {
        self.error.is_none() && self.stable && (self.order_ok || self.fixed)
    }
}

/// UIA order check after the timer, fix-up by position, settle check.
fn verify(
    uia: &Uia,
    wins: &mut [WinState],
    folders: &[Folder],
    r: &mut RunResult,
) -> Result<(), String> {
    let t0 = Instant::now();
    let mut names: Vec<Vec<String>> = Vec::new();
    let mut polls = 0;
    let mut slowest_poll = 0.0f64;
    let settled = wait_until(STEP_TIMEOUT, || {
        let tp = Instant::now();
        names = wins.iter().map(|w| uia.names(w.top)).collect();
        polls += 1;
        slowest_poll = slowest_poll.max(ms(tp.elapsed()));
        // A partial (virtualized) view is only accepted after 1 s, so that a
        // momentarily short read of a small window is not taken as final.
        let allow_partial = t0.elapsed() > Duration::from_secs(1);
        wins.iter()
            .zip(&names)
            .all(|(w, n)| uia_order(n, &w.slots, folders).is_some_and(|o| o.full || allow_partial))
    });
    println!(
        "    UIA: {polls} poll(s) until settled (slowest poll {slowest_poll:.0} ms), last names {:?}",
        names
            .iter()
            .map(|n| n.iter().filter(|s| !s.starts_with("r9_")).count())
            .collect::<Vec<_>>()
    );
    if settled.is_none() {
        let browsers: Vec<_> = wins.iter().map(describe_browsers).collect();
        return Err(format!(
            "UIA tab names did not settle: {names:?}; browsers (LocationName, LocationURL): \
             {browsers:?}"
        ));
    }
    for w in wins.iter() {
        let n = shell_tabs(w.top).len();
        if n != w.slots.len() {
            return Err(format!(
                "window {:#x}: {n} ShellWindows tabs, expected {}",
                w.top,
                w.slots.len()
            ));
        }
    }
    let orders: Vec<UiaOrder> = wins
        .iter()
        .zip(&names)
        .map(|(w, n)| uia_order(n, &w.slots, folders).expect("settled"))
        .collect();
    r.uia_seen = wins
        .iter()
        .zip(&orders)
        .map(|(w, o)| (o.seen.len(), w.slots.len()))
        .min_by_key(|(seen, tabs)| (*seen as f64 / *tabs as f64 * 1000.0) as usize);
    r.order_ok = orders.iter().all(UiaOrder::in_order);
    if !r.order_ok {
        if orders.iter().any(|o| !o.full) {
            return Err(format!(
                "UIA order is wrong in a virtualized (partial) tab strip; no fix-up attempted: \
                 {names:?}"
            ));
        }
        println!("    UIA order differs from creation order: {names:?}; re-navigating by position");
        let tf = Instant::now();
        let mut pending: Vec<(IWebBrowser2, usize)> = Vec::new();
        for (w, o) in wins.iter_mut().zip(&orders) {
            let old = w.browsers.clone();
            for (pos, &slot) in o.seen.iter().enumerate() {
                // The tab at UIA position `pos` is the browser of `slot`.
                let b = old[slot].clone();
                if slot != pos {
                    navigate(&b, &folders[w.slots[pos]].pidl)?;
                    pending.push((b.clone(), w.slots[pos]));
                }
                w.browsers[pos] = b;
            }
        }
        wait_until(BULK_TIMEOUT, || {
            pending.retain(|(b, f)| !shows(b, &folders[*f]));
            pending.is_empty()
        })
        .ok_or("fix-up navigation did not arrive")?;
        wait_until(STEP_TIMEOUT, || {
            wins.iter().all(|w| uia_in_order(uia, w, folders))
        })
        .ok_or("UIA order still wrong after the fix-up")?;
        r.fixed = true;
        r.fix_ms = ms(tf.elapsed());
    }
    let confirm = ms(t0.elapsed());
    r.uia_confirm_ms = Some(confirm);
    r.effective_ms = r.total_ms + if r.fixed { confirm } else { 0.0 };

    // Settle: every tab still at its folder (exact PIDL check), order intact,
    // no extra tab.
    pump_for(Duration::from_millis(500));
    let ts = Instant::now();
    let again: Vec<_> = wins.iter().map(|w| uia.names(w.top)).collect();
    println!(
        "    UIA: second read of all windows took {:.0} ms ({} TabItems)",
        ms(ts.elapsed()),
        again.iter().map(Vec::len).sum::<usize>()
    );
    for w in wins.iter().take(1) {
        println!(
            "    UIA uncached steps {:#x}: {}",
            w.top,
            uia.step_times(w.top)
        );
    }
    r.stable = wins.iter().all(|w| {
        w.browsers
            .iter()
            .zip(&w.slots)
            .all(|(b, &f)| at_folder(b, &folders[f].pidl))
            && uia_in_order(uia, w, folders)
            && shell_tabs(w.top).len() == w.slots.len()
    });
    if !r.stable {
        let browsers: Vec<_> = wins.iter().map(describe_browsers).collect();
        let names: Vec<_> = wins.iter().map(|w| uia.names(w.top)).collect();
        println!("    NOT STABLE after 500 ms: UIA {names:?}; browsers {browsers:?}");
    }
    Ok(())
}

fn execute(
    guard: &ExplorerGuard,
    uia: &Uia,
    folders: &[Folder],
    variant: Variant,
    layout: Layout,
    opened: &mut Vec<isize>,
    r: &mut RunResult,
) -> Result<(), String> {
    let assign: Vec<Vec<usize>> = (0..layout.windows)
        .map(|w| (w * layout.tabs..(w + 1) * layout.tabs).collect())
        .collect();
    let start = Instant::now();
    let mut wins: Vec<WinState> = Vec::new();
    let add = |r: &mut RunResult, p: PipeTimes| {
        r.create_ms += p.create;
        r.nav_ms += p.nav;
        r.issued_ms += p.issued;
        r.repeated_posts += p.repeated;
    };
    match variant {
        Variant::Sequential | Variant::Pipelined => {
            for a in &assign {
                let t = Instant::now();
                let mut ws = open_windows(guard, uia, folders, std::slice::from_ref(a), opened)?;
                r.open_ms += ms(t.elapsed());
                if variant == Variant::Sequential {
                    let (c, n) = create_sequential(&mut ws[0], folders, &mut r.repeated_posts)?;
                    r.create_ms += c;
                    r.nav_ms += n;
                } else {
                    add(r, create_pipelined(&mut ws, folders)?);
                }
                wins.extend(ws);
            }
        }
        Variant::PipelinedParallel | Variant::OverlappedParallel => {
            let t = Instant::now();
            wins = open_windows(guard, uia, folders, &assign, opened)?;
            r.open_ms = ms(t.elapsed());
            if variant == Variant::PipelinedParallel {
                add(r, create_pipelined(&mut wins, folders)?);
            } else {
                let gate_view = std::env::var("ESR_R9_GATE").is_ok_and(|v| v == "view");
                add(r, create_overlapped(&mut wins, folders, gate_view)?);
            }
        }
    }
    r.total_ms = ms(start.elapsed());
    verify(uia, &mut wins, folders, r)
}

/// Closes one adopted window with all of its tabs through the guard
/// (`WM_SYSCOMMAND SC_CLOSE`). Closing the tabs one by one with `WM_CLOSE`
/// as fast as the tab count dropped crashed Explorer 22621 in every run
/// (`FileExplorerExtensions.dll` + `0x103f7a`, `TabView_GotFocus` on a null
/// `XamlRoot`; task 16.3).
fn close_window(guard: &ExplorerGuard, top: isize) -> bool {
    !is_window(top) || (0..3).any(|_| guard.close(hwnd(top)))
}

/// Closes this run's windows and any new window showing a folder under the
/// temp root (never a pre-existing window).
fn close_run(guard: &ExplorerGuard, opened: &mut Vec<isize>) -> Result<(), String> {
    let mut failed = Vec::new();
    for &top in opened.iter() {
        if !close_window(guard, top) {
            failed.push(top);
        }
    }
    opened.clear();
    pump_for(Duration::from_millis(300));
    for (top, tabs) in tabs_by_top() {
        if guard.is_pre_existing(hwnd(top)) {
            continue;
        }
        let ours = tabs.iter().any(|(_, b)| {
            current_folder_pidl(b)
                .ok()
                .and_then(|p| parsing_name(&p))
                .is_some_and(|n| guard.is_under_root(&n))
        });
        if ours {
            println!("    stray test window {top:#x}: closing");
            guard.adopt(hwnd(top));
            if !close_window(guard, top) {
                failed.push(top);
            }
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!("windows did not close: {failed:x?}"))
    }
}

/// Diagnostics for stale tab headers: a fresh UIA client, the window title,
/// a longer wait, and selecting each tab in turn.
fn diagnose_headers(uia: &Uia, opened: &[isize]) {
    for &top in opened.iter().take(1) {
        let mut title = [0u16; 256];
        // SAFETY: plain query on an adopted test window.
        let n = unsafe { GetWindowTextW(hwnd(top), &mut title) }.max(0) as usize;
        println!(
            "    DIAG {top:#x}: title {:?}",
            String::from_utf16_lossy(&title[..n])
        );
        if let Ok(fresh) = Uia::new() {
            println!("    DIAG fresh UIA client: {:?}", fresh.names(top));
        }
        pump_for(Duration::from_secs(3));
        println!("    DIAG after 3 s more: {:?}", uia.names(top));
        let count = uia.names(top).len();
        for i in 0..count {
            let r = uia.select(top, i);
            pump_for(Duration::from_millis(400));
            println!(
                "    DIAG after selecting #{i} ({r:?}): {:?}",
                uia.names(top)
            );
        }
        // Does a later navigation repair a stale header? Re-navigate every tab
        // to its own folder, then one tab to the parent folder.
        let tabs = shell_tabs(top);
        for (_, b) in &tabs {
            if let Ok(p) = current_folder_pidl(b) {
                let _ = navigate(b, &p);
            }
        }
        pump_for(Duration::from_secs(1));
        println!(
            "    DIAG after re-navigating each tab to its folder: {:?}",
            uia.names(top)
        );
        if let Some((_, b)) = tabs.get(3)
            && let Some(parent) = current_folder_pidl(b)
                .ok()
                .and_then(|p| parsing_name(&p))
                .and_then(|n| Path::new(&n).parent().map(Path::to_path_buf))
            && let Ok(pp) = pidl_of_path(&parent)
        {
            let _ = navigate(b, &pp);
            pump_for(Duration::from_secs(1));
            println!(
                "    DIAG after navigating ShellWindows tab #3 to the parent: {:?}",
                uia.names(top)
            );
        }
    }
}

fn run_once(
    guard: &ExplorerGuard,
    uia: &Uia,
    folders: &[Folder],
    variant: Variant,
    layout: Layout,
) -> RunResult {
    let mut r = RunResult::default();
    let mut opened = Vec::new();
    if let Err(e) = execute(guard, uia, folders, variant, layout, &mut opened, &mut r) {
        let diag: Vec<(String, usize, Vec<String>)> = opened
            .iter()
            .map(|&t| (format!("{t:#x}"), shell_tabs(t).len(), uia.names(t)))
            .collect();
        println!("    ERROR: {e}; windows (hwnd, ShellWindows tabs, UIA names): {diag:?}");
        if std::env::var("ESR_R9_DIAG").is_ok_and(|v| v == "1") {
            diagnose_headers(uia, &opened);
        }
        r.error = Some(e);
    }
    if let Err(e) = close_run(guard, &mut opened) {
        // Leave it to the guard's final cleanup, but make the failure visible.
        println!("    CLOSE FAILED: {e}");
        r.error.get_or_insert(e);
    }
    pump_for(Duration::from_millis(500));
    r
}

// ---------------------------------------------------------------------------
// The spike
// ---------------------------------------------------------------------------

#[test]
#[ignore = "interactive spike: opens and closes its own Explorer windows"]
fn spike_r9_thirty_tab_timing() {
    let _sta = StaGuard::init().expect("STA init");
    let uia = Uia::new().expect("create UIA");
    // Records the pre-test windows (left untouched) and owns the temp root.
    let guard = ExplorerGuard::new("r9");
    let _hooks = Hooks::from_env();
    let folders: Vec<Folder> = (0..FOLDER_COUNT)
        .map(|i| {
            let name = format!("r9_{i:02}");
            let path = guard.dir(&name);
            let pidl = pidl_of_path(&path).expect("PIDL of a temp folder");
            Folder { path, pidl, name }
        })
        .collect();
    let runs: usize = std::env::var("ESR_R9_RUNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3)
        .max(1);

    let three = Layout {
        windows: 3,
        tabs: 10,
    };
    let one = Layout {
        windows: 1,
        tabs: 30,
    };
    let configs = [
        (Variant::Sequential, three),
        (Variant::Pipelined, three),
        (Variant::PipelinedParallel, three),
        (Variant::Sequential, one),
        (Variant::Pipelined, one),
        (Variant::OverlappedParallel, three),
        (Variant::OverlappedParallel, one),
    ];
    // Optional subset for diagnosis, e.g. ESR_R9_CONFIGS=1,2 (indices above).
    let selected: Vec<usize> = std::env::var("ESR_R9_CONFIGS")
        .ok()
        .map(|v| v.split(',').filter_map(|s| s.trim().parse().ok()).collect())
        .unwrap_or_else(|| (0..configs.len()).collect());
    let mut results: Vec<Vec<RunResult>> = vec![Vec::new(); configs.len()];
    for round in 1..=runs {
        for (ci, &(variant, layout)) in configs.iter().enumerate() {
            if !selected.contains(&ci) {
                continue;
            }
            println!(
                "== round {round}/{runs}: {} {}x{}",
                variant.label(),
                layout.windows,
                layout.tabs
            );
            let r = run_once(&guard, &uia, &folders, variant, layout);
            println!(
                "    total={:.0} ms (open={:.0} create={:.0} nav={:.0}, navigations issued after \
                 {:.0}) uia_confirm=+{:?} ms order_ok={} fixed={} (fix {:.0} ms) effective={:.0} ms \
                 stable={} uia_seen={:?} repeated_posts={} error={:?}",
                r.total_ms,
                r.open_ms,
                r.create_ms,
                r.nav_ms,
                r.issued_ms,
                r.uia_confirm_ms.map(f64::round),
                r.order_ok,
                r.fixed,
                r.fix_ms,
                r.effective_ms,
                r.stable,
                r.uia_seen,
                r.repeated_posts,
                r.error
            );
            results[ci].push(r);
        }
    }

    println!("== SUMMARY (R9, {runs} runs per configuration; times in ms)");
    let mut best: Option<(usize, f64)> = None;
    for (ci, &(variant, layout)) in configs.iter().enumerate() {
        let rs = &results[ci];
        if rs.is_empty() {
            continue;
        }
        let ok: Vec<&RunResult> = rs.iter().filter(|r| r.error.is_none()).collect();
        let col = |f: fn(&RunResult) -> f64| ok.iter().map(|r| f(r)).collect::<Vec<f64>>();
        let eff = col(|r| r.effective_ms);
        let all_good = rs.iter().all(RunResult::good);
        println!(
            "  {:<19} {}x{:<2}: effective min={:.0} median={:.0} | total min={:.0} median={:.0} | \
             median open={:.0} create={:.0} nav={:.0} (issued {:.0}) uia_confirm=+{:.0} | \
             order ok {}/{} (fixed {}), stable {}/{}, errors {}, repeated posts {}, \
             min UIA TabItems seen {:?}",
            variant.label(),
            layout.windows,
            layout.tabs,
            min(&eff),
            median(&eff),
            min(&col(|r| r.total_ms)),
            median(&col(|r| r.total_ms)),
            median(&col(|r| r.open_ms)),
            median(&col(|r| r.create_ms)),
            median(&col(|r| r.nav_ms)),
            median(&col(|r| r.issued_ms)),
            median(&col(|r| r.uia_confirm_ms.unwrap_or(f64::NAN))),
            rs.iter().filter(|r| r.order_ok).count(),
            rs.len(),
            rs.iter().filter(|r| r.fixed).count(),
            rs.iter().filter(|r| r.stable).count(),
            rs.len(),
            rs.len() - ok.len(),
            rs.iter().map(|r| r.repeated_posts).sum::<usize>(),
            rs.iter().filter_map(|r| r.uia_seen).min_by_key(|(s, _)| *s)
        );
        if layout == three && all_good {
            let m = median(&eff);
            if best.is_none_or(|(_, b)| m < b) {
                best = Some((ci, m));
            }
        }
    }
    match best {
        Some((ci, m)) => {
            let variant = configs[ci].0;
            println!(
                "  chosen 3x10 variant: {} (median effective {m:.0} ms, budget {BUDGET_MS:.0} ms: {})",
                variant.label(),
                if m <= BUDGET_MS { "MET" } else { "NOT MET" }
            );
            println!(
                "  recommendation: TabCreationPipelining::{}{}",
                variant.pipelining(),
                match variant {
                    Variant::PipelinedParallel => {
                        " (open all windows at once, then pipeline 0xA21B per window)"
                    }
                    Variant::OverlappedParallel => {
                        " (one outstanding 0xA21B per window; navigate each tab as soon as it \
                         registers, without waiting for arrival; all windows at once). \
                         Back-to-back 0xA21B posts (Pipelined) leave tab headers at \"Home\"."
                    }
                    _ => "",
                }
            );
        }
        None => println!("  no 3x10 variant succeeded in every run with the correct order"),
    }

    // Close every window this test created and check that none is left.
    let cleanup = guard.finish();
    if let Err(e) = &cleanup {
        println!("  cleanup: {e}");
    }
    cleanup.expect("test windows left open");

    let (ci, m) = best.expect("R9: no 3x10 variant succeeded in every run with the correct order");
    assert!(
        m <= BUDGET_MS,
        "R9: best variant {} needs a median of {m:.0} ms for 30 tabs in 3x10 windows, over the \
         {BUDGET_MS:.0} ms budget (Req 7.1); see the summary above",
        configs[ci].0.label()
    );
}
