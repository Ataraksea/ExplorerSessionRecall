//! Spike R2/R4: append a tab with `WM_COMMAND 0xA21B` and navigate it without
//! history (Req 5.3, 6.2, 7.1).
//!
//! ```text
//! cargo test -p esr-win --test spike_r2_new_tab -- --ignored --nocapture
//! ```
//!
//! Assumptions under test (design.md, "Verification spikes"):
//!
//! * R2: posting `WM_COMMAND 0xA21B` to a `ShellTabWindowClass` window opens a
//!   new tab in that Explorer window; the tab registers in ShellWindows and is
//!   appended rightmost in UIA order. The fallback is invoking the tab strip's
//!   `AddButton` through UIA `InvokePattern`.
//! * R4: `IWebBrowser2::Navigate2` with `navNoHistory` (0x2) on the new tab
//!   reaches the target folder and leaves exactly one tab per slot.
//!
//! Method: the test opens its own Explorer window on a temp folder with
//! `ShellExecuteExW("opennewwindow", SEE_MASK_IDLIST)` (the window is identified
//! by HWND, absent from the pre-test list, and showing the temp folder), then:
//!
//! 1. posts `0xA21B` to the active tab's HWND;
//! 2. posts `0xA21B` to a non-active tab's HWND;
//! 3. posts `0xA21B` to the top-level `CabinetWClass` HWND (alternative);
//! 4. invokes the UIA `AddButton`;
//! 5. posts `0xA21B` to the active tab three more times (timing).
//!
//! After every creation the new tab is navigated with `Navigate2(PIDL,
//! navNoHistory)` to its own temp folder. Registration time, UIA order and the
//! tab count (ShellWindows and UIA) are recorded. Only the window created by
//! this test is closed (by `ExplorerGuard`, see `tests/common`); the user's
//! windows and tabs are not touched. Run alone with `--test-threads=1`.

#![cfg(windows)]

mod common;

use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::path::Path;
use std::time::{Duration, Instant};

use common::explorer_guard::ExplorerGuard;
use esr_win::shell_util::{
    StaGuard, current_folder_pidl, enumerate_all_explorer_windows, is_window_visible,
    pidl_to_bytes, shell_browser, window_process_id,
};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::Ole::{
    SafeArrayAccessData, SafeArrayCreateVector, SafeArrayUnaccessData,
};
use windows::Win32::System::Variant::{
    VARENUM, VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_ARRAY, VT_BSTR, VT_I4, VT_UI1,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationInvokePattern, IUIAutomationSelectionItemPattern,
    TreeScope_Descendants, UIA_AutomationIdPropertyId, UIA_ButtonControlTypeId,
    UIA_ControlTypePropertyId, UIA_InvokePatternId, UIA_SelectionItemPatternId,
    UIA_TabItemControlTypeId,
};
use windows::Win32::UI::Shell::{
    ILCreateFromPathW, ILFree, ILIsEqual, IWebBrowser2, SEE_MASK_FLAG_NO_UI, SEE_MASK_IDLIST,
    SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GW_HWNDFIRST, GetWindow, MSG, PM_REMOVE, PeekMessageW, PostMessageW,
    SW_SHOWNOACTIVATE, TranslateMessage, WM_COMMAND,
};
use windows::core::{BSTR, HSTRING, w};

/// The undocumented "new tab" command of `ShellTabWindowClass`.
const CMD_NEW_TAB: usize = 0xA21B;
/// `BrowserNavConstants::navNoHistory`.
const NAV_NO_HISTORY: i32 = 0x2;
const STEP_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Message pumping and polling
// ---------------------------------------------------------------------------

fn pump_once() {
    let mut msg = MSG::default();
    // SAFETY: standard message pump on the STA thread.
    unsafe {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn pump_for(d: Duration) {
    let end = Instant::now() + d;
    while Instant::now() < end {
        pump_once();
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Polls `f` (pumping messages) until it returns true; returns the elapsed time.
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
        std::thread::sleep(Duration::from_millis(3));
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

// ---------------------------------------------------------------------------
// VARIANT helpers
// ---------------------------------------------------------------------------

fn variant_i4(value: i32) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_I4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { lVal: value },
            }),
        },
    }
}

/// A `VT_BSTR` VARIANT. The BSTR is leaked on purpose (spike code, tiny).
fn variant_bstr(value: &str) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_BSTR,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 {
                    bstrVal: ManuallyDrop::new(BSTR::from(value)),
                },
            }),
        },
    }
}

/// Runs `f` with a `VT_ARRAY | VT_UI1` VARIANT holding the PIDL bytes (the form
/// `Navigate2` accepts for a PIDL).
fn with_pidl_variant<R>(pidl: &[u8], f: impl FnOnce(&VARIANT) -> R) -> Result<R, String> {
    // SAFETY: the SAFEARRAY is created, filled, used and destroyed in this scope.
    unsafe {
        let psa = SafeArrayCreateVector(VT_UI1, 0, pidl.len() as u32);
        if psa.is_null() {
            return Err("SafeArrayCreateVector failed".into());
        }
        let mut data: *mut c_void = std::ptr::null_mut();
        SafeArrayAccessData(psa, &mut data).map_err(|e| e.to_string())?;
        std::ptr::copy_nonoverlapping(pidl.as_ptr(), data.cast::<u8>(), pidl.len());
        SafeArrayUnaccessData(psa).map_err(|e| e.to_string())?;
        let variant = VARIANT {
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
        let result = f(&variant);
        // Finding (this machine): calling SafeArrayDestroy(psa) here corrupts the heap
        // (STATUS_HEAP_CORRUPTION on the first Navigate2), so Explorer's side evidently
        // takes ownership of the array. The array is therefore intentionally not freed.
        let _ = psa;
        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// Shell helpers
// ---------------------------------------------------------------------------

/// Current ShellWindows tabs of the window `top`, in collection order.
fn tabs_of(top: HWND) -> Vec<esr_win::shell_util::ShellTab> {
    enumerate_all_explorer_windows()
        .ok()
        .and_then(|groups| groups.into_iter().find(|g| g.top_hwnd == top))
        .map(|g| g.tabs)
        .unwrap_or_default()
}

fn location_name(browser: &IWebBrowser2) -> String {
    // SAFETY: plain COM property read.
    unsafe { browser.LocationName() }
        .map(|s| s.to_string())
        .unwrap_or_default()
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

fn post_new_tab(target: HWND) -> Result<(), String> {
    // SAFETY: posting a plain message to a window handle.
    unsafe { PostMessageW(Some(target), WM_COMMAND, WPARAM(CMD_NEW_TAB), LPARAM(0)) }
        .map_err(|e| e.to_string())
}

/// `Navigate2(pidl, navNoHistory)` and wait until the tab shows `target`.
fn navigate_no_history(browser: &IWebBrowser2, target: &[u8]) -> Result<f64, String> {
    let flags = variant_i4(NAV_NO_HISTORY);
    // The optional [in] VARIANT* parameters must point to VT_EMPTY variants, not null.
    let empty = VARIANT::default();
    let start = Instant::now();
    println!(
        "    navigate: calling Navigate2 ({} PIDL bytes)",
        target.len()
    );
    with_pidl_variant(target, |url| {
        // SAFETY: valid VARIANT pointers for the duration of the call.
        unsafe { browser.Navigate2(url, Some(&flags), Some(&empty), Some(&empty), Some(&empty)) }
    })?
    .map_err(|e| format!("Navigate2: {e}"))?;
    wait_until(STEP_TIMEOUT, || at_folder(browser, target))
        .map(ms)
        .map(|_| ms(start.elapsed()))
        .ok_or_else(|| "timeout waiting for the tab to reach the target folder".to_string())
}

// ---------------------------------------------------------------------------
// UIA
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
struct UiaTab {
    name: String,
    selected: bool,
    left: i32,
    offscreen: bool,
    id: String,
    class: String,
}

struct Uia {
    automation: IUIAutomation,
}

impl Uia {
    fn new() -> windows::core::Result<Self> {
        // SAFETY: valid CLSID; in-process server.
        let automation = unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)? };
        Ok(Self { automation })
    }

    /// TabItems of the window in UIA tree order.
    fn tabs(&self, top: HWND) -> Vec<UiaTab> {
        self.try_tabs(top).unwrap_or_default()
    }

    fn try_tabs(&self, top: HWND) -> windows::core::Result<Vec<UiaTab>> {
        // SAFETY: plain UIA calls on live interfaces.
        unsafe {
            let root = self.automation.ElementFromHandle(top)?;
            let cond = self.automation.CreatePropertyCondition(
                UIA_ControlTypePropertyId,
                &variant_i4(UIA_TabItemControlTypeId.0),
            )?;
            let all = root.FindAll(TreeScope_Descendants, &cond)?;
            let mut tabs = Vec::new();
            for i in 0..all.Length()? {
                let el = all.GetElement(i)?;
                let class = el
                    .CurrentClassName()
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                // The Home page also exposes NavigationViewItem TabItems (Recent,
                // Favorites, ...); only the tab strip's items are tabs.
                if class.contains("NavigationViewItem") {
                    continue;
                }
                let selected = el
                    .GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(
                        UIA_SelectionItemPatternId,
                    )
                    .and_then(|p| p.CurrentIsSelected())
                    .map(|b| b.as_bool())
                    .unwrap_or(false);
                tabs.push(UiaTab {
                    name: el.CurrentName().map(|s| s.to_string()).unwrap_or_default(),
                    selected,
                    left: el.CurrentBoundingRectangle().map(|r| r.left).unwrap_or(0),
                    // Scrolled out of an overflowing tab strip (seen on 22H2 at
                    // 175 %): such items report a degenerate rect (left 0).
                    offscreen: el.CurrentIsOffscreen().is_ok_and(|b| b.as_bool()),
                    id: el
                        .CurrentAutomationId()
                        .map(|s| s.to_string())
                        .unwrap_or_default(),
                    class,
                });
            }
            Ok(tabs)
        }
    }

    /// Invokes the tab strip's "add new tab" button. Returns a description.
    fn invoke_add_button(&self, top: HWND) -> Result<String, String> {
        // SAFETY: plain UIA calls on live interfaces.
        unsafe {
            let root = self
                .automation
                .ElementFromHandle(top)
                .map_err(|e| e.to_string())?;
            let by_id = self
                .automation
                .CreatePropertyCondition(UIA_AutomationIdPropertyId, &variant_bstr("AddButton"))
                .map_err(|e| e.to_string())?;
            let element = match root.FindFirst(TreeScope_Descendants, &by_id) {
                Ok(el) => el,
                Err(e) => {
                    self.dump_buttons(top);
                    return Err(format!("AddButton not found by AutomationId: {e}"));
                }
            };
            let name = element
                .CurrentName()
                .map(|s| s.to_string())
                .unwrap_or_default();
            let invoke = element
                .GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
                .map_err(|e| format!("AddButton has no InvokePattern: {e}"))?;
            invoke.Invoke().map_err(|e| format!("Invoke: {e}"))?;
            Ok(format!("AddButton (name {name:?}) invoked"))
        }
    }

    /// Diagnostics: lists Button elements when `AddButton` cannot be found.
    fn dump_buttons(&self, top: HWND) {
        // SAFETY: plain UIA calls on live interfaces.
        let result = (|| unsafe {
            let root = self.automation.ElementFromHandle(top)?;
            let cond = self.automation.CreatePropertyCondition(
                UIA_ControlTypePropertyId,
                &variant_i4(UIA_ButtonControlTypeId.0),
            )?;
            let all = root.FindAll(TreeScope_Descendants, &cond)?;
            for i in 0..all.Length()? {
                let el = all.GetElement(i)?;
                println!(
                    "    button: name={:?} id={:?}",
                    el.CurrentName().map(|s| s.to_string()).unwrap_or_default(),
                    el.CurrentAutomationId()
                        .map(|s| s.to_string())
                        .unwrap_or_default()
                );
            }
            windows::core::Result::Ok(())
        })();
        if let Err(e) = result {
            println!("    button dump failed: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Own Explorer window (tracked and closed by ExplorerGuard)
// ---------------------------------------------------------------------------

/// Opens a new Explorer window on `dir` and returns its top-level HWND.
fn open_own_window(dir: &Path, guard: &ExplorerGuard, uia: &Uia) -> Result<HWND, String> {
    let target = pidl_of_path(dir)?;
    let dir_name = dir.file_name().unwrap().to_string_lossy().to_string();
    let launch_dir = dir.to_path_buf();
    let launcher = std::thread::spawn(move || -> Result<(), String> {
        let _sta = StaGuard::init().map_err(|e| e.to_string())?;
        // SAFETY: valid path string; the PIDL is freed below.
        unsafe {
            let pidl = ILCreateFromPathW(&HSTRING::from(launch_dir.as_os_str()));
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
    });
    let mut found: Option<HWND> = None;
    wait_until(Duration::from_secs(20), || {
        let Ok(groups) = enumerate_all_explorer_windows() else {
            return false;
        };
        for g in groups {
            if guard.is_pre_existing(g.top_hwnd) {
                continue;
            }
            if g.tabs
                .iter()
                .any(|t| location_name(&t.browser) == dir_name && at_folder(&t.browser, &target))
            {
                // Ours (new, showing our temp folder): adopt it at once so it is
                // closed even if Explorer replaces it or the test fails.
                guard.adopt(g.top_hwnd);
                // Explorer may replace a freshly created window: only accept it
                // once the UIA tab strip is populated too.
                if g.tabs.len() == 1 && !uia.tabs(g.top_hwnd).is_empty() {
                    found = Some(g.top_hwnd);
                    return true;
                }
            }
        }
        false
    });
    match launcher.join() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err("launcher thread panicked".into()),
    }
    found.ok_or_else(|| "the spike window did not appear in ShellWindows".to_string())
}

// ---------------------------------------------------------------------------
// One tab creation + navigation step
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Step {
    label: String,
    action_error: Option<String>,
    registered_ms: Option<f64>,
    uia_ms: Option<f64>,
    exactly_one_new: bool,
    appended_rightmost: bool,
    new_is_selected: Option<bool>,
    nav_ms: Option<f64>,
    nav_error: Option<String>,
    tab_hwnd_stable: bool,
    tab_count_after_nav: usize,
    uia_count_after_nav: usize,
    expected_count: usize,
    created: bool,
}

impl Step {
    fn creation_ok(&self) -> bool {
        self.created && self.exactly_one_new && self.appended_rightmost
    }
    fn nav_ok(&self) -> bool {
        self.nav_ms.is_some()
            && self.tab_hwnd_stable
            && self.tab_count_after_nav == self.expected_count
            && self.uia_count_after_nav == self.expected_count
    }
}

/// Runs `action` to create a tab, observes it, then navigates the new tab to
/// `dir` with `navNoHistory`. Returns the step result and the new browser.
fn create_and_navigate(
    label: &str,
    top: HWND,
    uia: &Uia,
    dir: &Path,
    action: &dyn Fn() -> Result<(), String>,
) -> (Step, Option<IWebBrowser2>) {
    let mut step = Step {
        label: label.to_string(),
        ..Default::default()
    };
    let before = tabs_of(top);
    let before_hwnds: Vec<isize> = before
        .iter()
        .filter_map(|t| t.tab_hwnd.map(|h| h.0 as isize))
        .collect();
    let before_uia = uia.tabs(top);
    let n = before.len();
    step.expected_count = n + 1;

    let t0 = Instant::now();
    if let Err(e) = action() {
        step.action_error = Some(e);
        return (step, None);
    }
    step.registered_ms =
        wait_until(STEP_TIMEOUT, || tabs_of(top).len() > n).map(|_| ms(t0.elapsed()));
    step.uia_ms = wait_until(STEP_TIMEOUT, || uia.tabs(top).len() > before_uia.len())
        .map(|_| ms(t0.elapsed()));
    if step.registered_ms.is_none() {
        return (step, None);
    }
    pump_for(Duration::from_millis(250)); // let the tab settle (and any duplicate appear)

    let after = tabs_of(top);
    let after_uia = uia.tabs(top);
    let new_tabs: Vec<_> = after
        .iter()
        .filter(|t| {
            !t.tab_hwnd
                .is_some_and(|h| before_hwnds.contains(&(h.0 as isize)))
        })
        .collect();
    step.exactly_one_new = after.len() == n + 1 && new_tabs.len() == 1;
    let prefix_same = after_uia.len() == before_uia.len() + 1
        && after_uia[..before_uia.len()]
            .iter()
            .map(|t| &t.name)
            .eq(before_uia.iter().map(|t| &t.name));
    // UIA tree order is the tab order; on-screen items must also be laid out
    // left to right (scrolled-out items of an overflowing strip are skipped).
    let on_screen: Vec<&UiaTab> = after_uia.iter().filter(|t| !t.offscreen).collect();
    let lefts_increase = on_screen.windows(2).all(|w| w[0].left < w[1].left);
    if on_screen.len() < after_uia.len() {
        println!(
            "  [{label}] tab strip overflows: {} of {} TabItems offscreen",
            after_uia.len() - on_screen.len(),
            after_uia.len()
        );
    }
    step.appended_rightmost = prefix_same && lefts_increase;
    step.new_is_selected = after_uia.last().map(|t| t.selected);
    println!(
        "  [{label}] after create: ShellWindows tabs={} (new={}), UIA = {:?}",
        after.len(),
        new_tabs.len(),
        after_uia
    );
    step.created = true;
    let Some(new_tab) = new_tabs.first() else {
        return (step, None);
    };
    let browser = new_tab.browser.clone();
    let hwnd_before = new_tab.tab_hwnd;

    match pidl_of_path(dir).and_then(|pidl| navigate_no_history(&browser, &pidl)) {
        Ok(t) => step.nav_ms = Some(t),
        Err(e) => step.nav_error = Some(e),
    }
    pump_for(Duration::from_millis(400)); // a stray extra tab would show up here
    let final_tabs = tabs_of(top);
    step.tab_count_after_nav = final_tabs.len();
    step.uia_count_after_nav = uia.tabs(top).len();
    let hwnd_after = esr_win::shell_util::tab_hwnd(&browser).ok();
    step.tab_hwnd_stable = hwnd_before.is_some() && hwnd_before == hwnd_after;
    (step, Some(browser))
}

/// Which tab does each notion of "active" point at? (matters for task 13.7)
fn report_active(top: HWND, uia: &Uia, tag: &str) {
    let uia_tabs = uia.tabs(top);
    let selected: Vec<&str> = uia_tabs
        .iter()
        .filter(|t| t.selected)
        .map(|t| t.name.as_str())
        .collect();
    println!("  [{tag}] UIA selected tab(s): {selected:?}");
    for (i, tab) in tabs_of(top).iter().enumerate() {
        let name = location_name(&tab.browser);
        let (view_hwnd, view_visible) = shell_browser(&tab.browser)
            .ok()
            .and_then(|sb| unsafe { sb.QueryActiveShellView() }.ok())
            .and_then(|v| unsafe { v.GetWindow() }.ok())
            .map(|h| (Some(h), is_window_visible(h)))
            .unwrap_or((None, false));
        let (tab_visible, first_sibling) = match tab.tab_hwnd {
            Some(h) => (
                is_window_visible(h),
                // SAFETY: GetWindow tolerates stale HWNDs.
                unsafe { GetWindow(h, GW_HWNDFIRST) }.ok() == Some(h),
            ),
            None => (false, false),
        };
        let uia_selected = uia_tabs
            .iter()
            .any(|t| t.selected && t.name.ends_with(name.as_str()));
        println!(
            "    shellwindows[{i}] name={name:?} tab_hwnd={:?} tab_visible={tab_visible} \
             zorder_first={first_sibling} view_hwnd={view_hwnd:?} view_visible={view_visible} \
             uia_selected={uia_selected}",
            tab.tab_hwnd
        );
    }
}

/// The browser whose folder name equals the UIA-selected tab (or the first).
fn active_tab_hwnd(top: HWND, uia: &Uia) -> Option<HWND> {
    let tabs = tabs_of(top);
    let selected = uia.tabs(top).into_iter().find(|t| t.selected)?;
    tabs.iter()
        .find(|t| selected.name.ends_with(location_name(&t.browser).as_str()))
        .or(tabs.first())
        .and_then(|t| t.tab_hwnd)
}

fn inactive_tab_hwnd(top: HWND, uia: &Uia) -> Option<HWND> {
    let tabs = tabs_of(top);
    let selected = uia.tabs(top).into_iter().find(|t| t.selected)?.name;
    tabs.iter()
        .find(|t| !selected.ends_with(location_name(&t.browser).as_str()))
        .and_then(|t| t.tab_hwnd)
}

// ---------------------------------------------------------------------------
// The spike
// ---------------------------------------------------------------------------

#[test]
#[ignore = "interactive spike: opens and closes its own Explorer window"]
fn spike_r2_new_tab_append_and_navigate() {
    let _sta = StaGuard::init().expect("STA init");
    let uia = Uia::new().expect("create UIA");
    // Records the pre-test windows (left untouched) and owns the temp root.
    let guard = ExplorerGuard::new("r2");
    let home = guard.dir("r2_home");
    let top = open_own_window(&home, &guard, &uia).expect("open the spike window");
    // The window is usable once both ShellWindows and the UIA tab strip show it.
    let ready = wait_until(Duration::from_secs(15), || {
        tabs_of(top).len() == 1 && !uia.tabs(top).is_empty()
    });
    println!(
        "spike window {:?} (pid {}), ready after {:?} ms, initial UIA tabs: {:?}",
        top,
        window_process_id(top),
        ready.map(|d| ms(d).round()),
        uia.tabs(top)
    );
    assert_eq!(
        tabs_of(top).len(),
        1,
        "fresh window should have exactly one tab"
    );
    pump_for(Duration::from_millis(500));
    report_active(top, &uia, "initial");

    let mut steps: Vec<Step> = Vec::new();
    let mut expected_names = vec!["r2_home".to_string()];
    let mut navigated: Vec<(IWebBrowser2, Vec<u8>)> = Vec::new();

    let mut run = |label: &str,
                   dir_name: &str,
                   action: &dyn Fn() -> Result<(), String>,
                   steps: &mut Vec<Step>| {
        let dir = guard.dir(dir_name);
        let (step, browser) = create_and_navigate(label, top, &uia, &dir, action);
        if step.creation_ok() {
            expected_names.push(dir_name.to_string());
        }
        if let (Some(b), Some(_)) = (browser, step.nav_ms)
            && let Ok(p) = pidl_of_path(&dir)
        {
            navigated.push((b, p));
        }
        println!(
            "  [{label}] action_error={:?} registered_ms={:?} uia_ms={:?} created={} one_new={} \
             appended={} new_selected={:?} nav_ms={:?} nav_error={:?} hwnd_stable={} \
             counts after nav: shellwindows={} uia={} (expected {})",
            step.action_error,
            step.registered_ms.map(|v| v.round()),
            step.uia_ms.map(|v| v.round()),
            step.created,
            step.exactly_one_new,
            step.appended_rightmost,
            step.new_is_selected,
            step.nav_ms.map(|v| v.round()),
            step.nav_error,
            step.tab_hwnd_stable,
            step.tab_count_after_nav,
            step.uia_count_after_nav,
            step.expected_count
        );
        steps.push(step);
    };

    // 1. WM_COMMAND to the active tab's ShellTabWindowClass.
    println!("== 1: WM_COMMAND 0xA21B -> active tab HWND");
    let active = active_tab_hwnd(top, &uia).expect("active tab");
    println!("  posting to {active:?}");
    run("wm-active", "r2_t1", &|| post_new_tab(active), &mut steps);
    report_active(top, &uia, "after wm-active");

    // 2. WM_COMMAND to a non-active tab.
    println!("== 2: WM_COMMAND 0xA21B -> non-active tab HWND");
    match inactive_tab_hwnd(top, &uia) {
        Some(inactive) => {
            println!("  posting to {inactive:?}");
            run(
                "wm-inactive",
                "r2_t2",
                &|| post_new_tab(inactive),
                &mut steps,
            );
        }
        None => println!("  no non-active tab available (skipped)"),
    }
    report_active(top, &uia, "after wm-inactive");

    // 3. WM_COMMAND to the top-level CabinetWClass window.
    println!("== 3: WM_COMMAND 0xA21B -> top-level window HWND");
    run("wm-top", "r2_t3", &|| post_new_tab(top), &mut steps);

    // 4. UIA AddButton (R2 fallback).
    println!("== 4: UIA AddButton InvokePattern");
    run(
        "uia-add-button",
        "r2_t4",
        &|| uia.invoke_add_button(top).map(|d| println!("  {d}")),
        &mut steps,
    );
    report_active(top, &uia, "after uia-add-button");

    // 5. Timing: three more WM_COMMANDs on the currently active tab.
    println!("== 5: WM_COMMAND 0xA21B -> active tab, x3 (timing)");
    for i in 1..=3 {
        let hwnd = active_tab_hwnd(top, &uia).unwrap_or(active);
        run(
            &format!("wm-active-timing-{i}"),
            &format!("r2_t5_{i}"),
            &|| post_new_tab(hwnd),
            &mut steps,
        );
    }

    // Final state: UIA order must equal creation order; each tab shows its folder.
    pump_for(Duration::from_millis(500));
    let final_uia: Vec<String> = uia.tabs(top).into_iter().map(|t| t.name).collect();
    let order_ok = final_uia.len() == expected_names.len()
        && final_uia
            .iter()
            .zip(&expected_names)
            .all(|(ui, exp)| ui.ends_with(exp.as_str()));
    let arrival_ok = navigated.iter().all(|(b, p)| at_folder(b, p));
    println!("== final");
    println!("  UIA order  : {final_uia:?}");
    println!("  expected   : {expected_names:?}");
    println!("  order_ok={order_ok} all_navigated_tabs_still_at_target={arrival_ok}");
    println!(
        "  ShellWindows tabs={} UIA tabs={}",
        tabs_of(top).len(),
        final_uia.len()
    );
    report_active(top, &uia, "final");

    // Summary.
    let by = |label: &str| steps.iter().find(|s| s.label == label);
    let ok = |s: Option<&Step>| s.is_some_and(|s| s.creation_ok());
    let wm_timing: Vec<f64> = steps
        .iter()
        .filter(|s| s.label.starts_with("wm-active") && s.creation_ok())
        .filter_map(|s| s.registered_ms)
        .collect();
    let wm_avg = wm_timing.iter().sum::<f64>() / wm_timing.len().max(1) as f64;
    println!("== SUMMARY (R2/R4)");
    println!(
        "  WM_COMMAND 0xA21B -> active tab HWND : works={}",
        ok(by("wm-active"))
    );
    println!(
        "  WM_COMMAND 0xA21B -> non-active tab  : works={}",
        ok(by("wm-inactive"))
    );
    println!(
        "  WM_COMMAND 0xA21B -> top-level HWND  : works={}",
        ok(by("wm-top"))
    );
    println!(
        "  UIA AddButton Invoke                 : works={}",
        ok(by("uia-add-button"))
    );
    println!(
        "  append rightmost (all created tabs)  : {}",
        steps
            .iter()
            .filter(|s| s.created)
            .all(|s| s.appended_rightmost)
    );
    println!(
        "  Navigate2 + navNoHistory reached target, 1 tab/slot : {}",
        steps.iter().filter(|s| s.created).all(|s| s.nav_ok())
    );
    println!(
        "  time per tab via WM_COMMAND (post -> ShellWindows registration), avg of {}: {:.0} ms; \
         UIA AddButton: {:?} ms",
        wm_timing.len(),
        wm_avg,
        by("uia-add-button")
            .and_then(|s| s.registered_ms)
            .map(|v| v.round())
    );

    // Close every window this test created and check that none is left.
    drop(navigated);
    let cleanup = guard.finish();
    if let Err(e) = &cleanup {
        println!("  cleanup: {e}");
    }

    assert!(
        ok(by("wm-active")),
        "R2: WM_COMMAND 0xA21B on the active tab did not append a tab"
    );
    assert!(
        ok(by("uia-add-button")),
        "R2 fallback: UIA AddButton did not append a tab"
    );
    assert!(
        steps.iter().filter(|s| s.created).all(|s| s.nav_ok()),
        "R4: Navigate2(navNoHistory) did not reach the target with one tab per slot"
    );
    assert!(order_ok, "UIA order does not match creation order");
    assert!(
        arrival_ok,
        "a navigated tab is no longer at its target folder"
    );
    cleanup.expect("test windows left open");
}
