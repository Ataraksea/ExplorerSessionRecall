//! Spike R5: tab identity across drag-out and drag-in (Req 2.4).
//!
//! ```text
//! cargo test -p esr-win --test spike_r5_drag -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Assumption under test (design.md, "Verification spikes", R5): when a tab is
//! dragged out of an Explorer window into a new window, and then into another
//! window, its `ShellTabWindowClass` HWND either moves with it or is recreated,
//! and a rescan (ShellWindows + UIA tab order) of the windows involved sees the
//! tab under its new window at its new position.
//!
//! Method:
//!
//! 1. Open `W1` with tabs `r5_a1, r5_a2, r5_a3` and `W2` with tab `r5_b1`
//!    (temp folders under the guard's root; `opennewwindow`, `WM_COMMAND
//!    0xA21B`, `Navigate2(PIDL, navNoHistory)`), placed side by side at the top
//!    of the primary work area (topmost, so the drags only ever hit them).
//! 2. A plain window owned by this test (the "drop pad", no drop target) covers
//!    the area below `W1`. Drag-out: `SendInput` drags `r5_a2`'s UIA tab item
//!    from `W1`'s strip onto the pad and releases. The new window `W3` that
//!    shows `r5_a2` is adopted at once.
//! 3. Drag-in: the pad is removed, `W3` is placed below `W2`, and `r5_a2`'s tab
//!    is dragged into `W2`'s strip, over the right half of `r5_b1`.
//! 4. After each move the tab HWNDs (ShellWindows → `IShellBrowser::GetWindow`)
//!    are compared with the ones before, and every involved window is rescanned:
//!    UIA order joined with ShellWindows tabs by folder parsing name.
//!
//! Mouse safety: the cursor position is saved first and restored afterwards;
//! each drag is short (< 1 s, a few hundred pixels); `LEFTUP` is always sent at
//! the end of a drag and again from a `Drop` guard if the button is still down.
//! Before pressing, the point under the cursor must belong to the expected
//! test window, and the drop point to the pad / target window.
//!
//! Window safety: `ExplorerGuard` (`tests/common`) adopts `W1`, `W2` and `W3`,
//! sweeps any other new window under the temp root, and closes them all (every
//! tab) on success, failure or panic. Run this test alone.
//!
//! Findings (Windows 11 22H2, build 22621.3880, 1 monitor 2500x1572 at 175 %,
//! Hyper-V VM; this test passes):
//!
//! * `SendInput` drags (absolute, virtual-desk coordinates, per-monitor-DPI-v2
//!   process, 2 px steps past the drag threshold, then 30 steps of 15 ms) are
//!   accepted by the XAML tab strip, both for drag-out and drag-in. No explicit
//!   foreground activation is needed; the press activates the window.
//! * Drag-out onto a plain window with no drop target creates a new
//!   `CabinetWClass` window (same Explorer process) that registers in
//!   ShellWindows ~200 ms after the release; rescans of both windows are
//!   consistent ~400 ms after it.
//! * The dragged tab's `ShellTabWindowClass` HWND does **not** survive: on
//!   drag-out and again on drag-in the old HWND is destroyed and a new one is
//!   created in the target window. Tabs that were not dragged keep their HWNDs.
//!   So the tracker must not key a moved tab by its HWND; a moved tab shows up
//!   as a removed tab key in the source plus a new tab key (same location) in
//!   the target, which location-based reconciliation handles.
//! * Drag-in: dropping just past the target's last tab inserts the tab
//!   rightmost; dropping on the right part of the last tab inserted it at
//!   index 0 (TabView opens the insertion gap at the hovered tab). The emptied
//!   source window closes by itself. The dragged tab becomes the selected tab of
//!   the target window; the source window selects its left neighbour.
//! * On 22H2 the UIA tab name is the folder's display name (`r5_a1`), not the
//!   full path as on build 26200; the TabItem class is still `ListViewItem`.

#![cfg(windows)]

mod common;

use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::explorer_guard::{ExplorerGuard, parsing_name, pump_for, pump_once};
use esr_win::shell_util::{
    SHELL_TAB_WINDOW_CLASS, StaGuard, current_folder_pidl, enumerate_all_explorer_windows,
    is_cabinet_window, pidl_to_bytes, window_class_name, window_process_id,
};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{GRAY_BRUSH, GetStockObject, HBRUSH, UpdateWindow};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::{
    SafeArrayAccessData, SafeArrayCreateVector, SafeArrayUnaccessData,
};
use windows::Win32::System::Variant::{
    VARENUM, VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_ARRAY, VT_UI1,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationSelectionItemPattern,
    TreeScope_Descendants, UIA_ControlTypePropertyId, UIA_SelectionItemPatternId,
    UIA_TabItemControlTypeId,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
    SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_MOUSE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_VIRTUALDESK, MOUSEINPUT, SendInput,
};
use windows::Win32::UI::Shell::{
    ILCreateFromPathW, ILFree, IWebBrowser2, SEE_MASK_FLAG_NO_UI, SEE_MASK_IDLIST,
    SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GA_ROOT, GA_ROOTOWNER, GetAncestor,
    GetCursorPos, GetSystemMetrics, GetWindowTextW, HWND_TOPMOST, IsWindow, PostMessageW,
    RegisterClassW, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    SPI_GETWORKAREA, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_SHOWWINDOW,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SetCursorPos, SetWindowPos, SystemParametersInfoW,
    UnregisterClassW, WM_COMMAND, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP, WS_VISIBLE, WindowFromPoint,
};
use windows::core::{HSTRING, w};

/// The undocumented "new tab" command of `ShellTabWindowClass`.
const CMD_NEW_TAB: usize = 0xA21B;
/// `BrowserNavConstants::navNoHistory`.
const NAV_NO_HISTORY: i32 = 0x2;
/// `UIA_E_ELEMENTNOTAVAILABLE`, seen transiently from `ElementFromHandle`.
const UIA_E_ELEMENTNOTAVAILABLE: u32 = 0x8004_0201;
/// How long a move may take to show up in ShellWindows / UIA.
const MOVE_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn hwnd(raw: isize) -> HWND {
    HWND(raw as *mut _)
}

fn raw(h: HWND) -> isize {
    h.0 as isize
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
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn ms(d: Duration) -> u128 {
    d.as_millis()
}

fn same_path(a: &str, b: &str) -> bool {
    a.trim_end_matches('\\')
        .eq_ignore_ascii_case(b.trim_end_matches('\\'))
}

/// Whether a UIA tab name denotes `path`. On build 26200 the name is the full
/// path; on 22H2 (22621) it is the folder's display name (the leaf for these
/// uniquely named temp folders).
fn uia_matches(uia_name: &str, path: &str) -> bool {
    same_path(uia_name, path)
        || Path::new(path.trim_end_matches('\\'))
            .file_name()
            .is_some_and(|leaf| leaf.to_string_lossy().eq_ignore_ascii_case(uia_name))
}

fn root_of(h: HWND) -> isize {
    // SAFETY: GetAncestor tolerates stale HWNDs.
    raw(unsafe { GetAncestor(h, GA_ROOT) })
}

fn is_window(raw_hwnd: isize) -> bool {
    // SAFETY: IsWindow tolerates stale HWNDs.
    unsafe { IsWindow(Some(hwnd(raw_hwnd))) }.as_bool()
}

/// Top-level window under a screen point.
fn root_at(p: POINT) -> isize {
    // SAFETY: plain query.
    root_of(unsafe { WindowFromPoint(p) })
}

/// Whether the point is over `top` itself or over a top-level window owned by
/// it (Explorer's WinUI content can sit in owned popup windows).
fn over(p: POINT, top: isize) -> bool {
    let r = root_at(p);
    // SAFETY: GetAncestor tolerates stale HWNDs.
    r == top || raw(unsafe { GetAncestor(hwnd(r), GA_ROOTOWNER) }) == top
}

/// Diagnostics for the window under a point.
fn describe_at(p: POINT) -> String {
    // SAFETY: plain queries; stale HWNDs just fail.
    unsafe {
        let child = WindowFromPoint(p);
        let r = hwnd(root_of(child));
        let owner = GetAncestor(r, GA_ROOTOWNER);
        let mut title = [0u16; 128];
        let n = GetWindowTextW(r, &mut title).max(0) as usize;
        format!(
            "root {:#x} class {:?} pid {} title {:?} (child {:#x} class {:?}; root owner {:#x})",
            raw(r),
            window_class_name(r),
            window_process_id(r),
            String::from_utf16_lossy(&title[..n]),
            raw(child),
            window_class_name(child),
            raw(owner)
        )
    }
}

fn work_area() -> RECT {
    let mut r = RECT::default();
    // SAFETY: `r` is a valid RECT out buffer for SPI_GETWORKAREA.
    unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut r as *mut RECT as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    }
    .expect("SPI_GETWORKAREA");
    r
}

/// Moves a test window to `r` and makes it topmost (so drags only hit it).
fn place(top: isize, r: RECT) {
    // SAFETY: called only on windows adopted by this test.
    let _ = unsafe {
        SetWindowPos(
            hwnd(top),
            Some(HWND_TOPMOST),
            r.left,
            r.top,
            r.right - r.left,
            r.bottom - r.top,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        )
    };
}

fn rect(x: i32, y: i32, w: i32, h: i32) -> RECT {
    RECT {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    }
}

// ---------------------------------------------------------------------------
// Shell helpers
// ---------------------------------------------------------------------------

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

fn folder_of(browser: &IWebBrowser2) -> Option<String> {
    current_folder_pidl(browser)
        .ok()
        .and_then(|p| parsing_name(&p))
}

/// One ShellWindows tab: folder parsing name, tab HWND and browser.
struct ShellTabInfo {
    name: String,
    tab: isize,
    browser: IWebBrowser2,
}

/// ShellWindows tabs of `top` (collection order).
fn shell_tabs(top: isize) -> Vec<ShellTabInfo> {
    let Ok(groups) = enumerate_all_explorer_windows() else {
        return Vec::new();
    };
    groups
        .into_iter()
        .filter(|g| raw(g.top_hwnd) == top)
        .flat_map(|g| g.tabs)
        .map(|t| ShellTabInfo {
            name: folder_of(&t.browser).unwrap_or_default(),
            tab: t.tab_hwnd.map_or(0, raw),
            browser: t.browser,
        })
        .collect()
}

/// `(top, tab HWND)` of every ShellWindows tab showing `path`.
fn locate(path: &str) -> Vec<(isize, isize)> {
    let Ok(groups) = enumerate_all_explorer_windows() else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for g in groups {
        for t in g.tabs {
            if folder_of(&t.browser).is_some_and(|n| same_path(&n, path)) {
                found.push((raw(g.top_hwnd), t.tab_hwnd.map_or(0, raw)));
            }
        }
    }
    found
}

/// Runs `f` with a `VT_ARRAY | VT_UI1` VARIANT holding the PIDL bytes.
fn with_pidl_variant<R>(pidl: &[u8], f: impl FnOnce(&VARIANT) -> R) -> Result<R, String> {
    // SAFETY: the SAFEARRAY is created and filled here and passed to `f`.
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
        // Finding from R2: SafeArrayDestroy(psa) after Navigate2 corrupts the heap,
        // so the array is intentionally not destroyed (and `variant` is never dropped).
        Ok(f(&variant))
    }
}

fn navigate_no_history(browser: &IWebBrowser2, dir: &Path) -> Result<(), String> {
    let pidl = pidl_of_path(dir)?;
    let flags = VARIANT::from(NAV_NO_HISTORY);
    // Optional [in] VARIANT* parameters must be empty VARIANTs, never None.
    let empty = VARIANT::default();
    with_pidl_variant(&pidl, |url| {
        // SAFETY: valid VARIANT pointers for the duration of the call.
        unsafe { browser.Navigate2(url, Some(&flags), Some(&empty), Some(&empty), Some(&empty)) }
    })?
    .map_err(|e| format!("Navigate2: {e}"))
}

// ---------------------------------------------------------------------------
// UIA
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct UiaTab {
    name: String,
    selected: bool,
    rect: RECT,
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

    /// `ElementFromHandle`, retried while it reports `UIA_E_ELEMENTNOTAVAILABLE`.
    fn element(&self, top: isize) -> windows::core::Result<IUIAutomationElement> {
        let mut attempt = 0;
        loop {
            // SAFETY: plain UIA call; stale HWNDs just fail.
            match unsafe { self.automation.ElementFromHandle(hwnd(top)) } {
                Ok(e) => return Ok(e),
                Err(e) if e.code().0 as u32 == UIA_E_ELEMENTNOTAVAILABLE && attempt < 10 => {
                    attempt += 1;
                    pump_for(Duration::from_millis(100));
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// The tab strip's items (`ListViewItem` TabItems, not `NavigationViewItem`)
    /// in UIA tree order (= visual left-to-right order).
    fn tabs(&self, top: isize) -> Vec<UiaTab> {
        self.try_tabs(top).unwrap_or_default()
    }

    fn try_tabs(&self, top: isize) -> windows::core::Result<Vec<UiaTab>> {
        let root = self.element(top)?;
        // SAFETY: plain UIA calls on live interfaces.
        unsafe {
            let cond = self.automation.CreatePropertyCondition(
                UIA_ControlTypePropertyId,
                &VARIANT::from(UIA_TabItemControlTypeId.0),
            )?;
            let all = root.FindAll(TreeScope_Descendants, &cond)?;
            let mut tabs = Vec::new();
            for i in 0..all.Length()? {
                let el = all.GetElement(i)?;
                let class = el
                    .CurrentClassName()
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                if !class.contains("ListViewItem") || class.contains("NavigationViewItem") {
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
                    rect: el.CurrentBoundingRectangle().unwrap_or_default(),
                });
            }
            Ok(tabs)
        }
    }

    fn names(&self, top: isize) -> Vec<String> {
        self.tabs(top).into_iter().map(|t| t.name).collect()
    }

    /// Diagnostics: every UIA TabItem under `top` as `(class, name)`.
    fn dump_tab_items(&self, top: isize) -> Vec<(String, String)> {
        let Ok(root) = self.element(top) else {
            return Vec::new();
        };
        // SAFETY: plain UIA calls on live interfaces.
        unsafe {
            let Ok(cond) = self.automation.CreatePropertyCondition(
                UIA_ControlTypePropertyId,
                &VARIANT::from(UIA_TabItemControlTypeId.0),
            ) else {
                return Vec::new();
            };
            let Ok(all) = root.FindAll(TreeScope_Descendants, &cond) else {
                return Vec::new();
            };
            (0..all.Length().unwrap_or(0))
                .filter_map(|i| all.GetElement(i).ok())
                .map(|el| {
                    (
                        el.CurrentClassName()
                            .map(|s| s.to_string())
                            .unwrap_or_default(),
                        el.CurrentName().map(|s| s.to_string()).unwrap_or_default(),
                    )
                })
                .collect()
        }
    }

    fn tab_named(&self, top: isize, path: &str) -> Option<UiaTab> {
        self.tabs(top)
            .into_iter()
            .find(|t| uia_matches(&t.name, path))
    }
}

fn names_eq(actual: &[String], expected: &[&str]) -> bool {
    actual.len() == expected.len() && actual.iter().zip(expected).all(|(a, e)| uia_matches(a, e))
}

// ---------------------------------------------------------------------------
// Rescan: UIA order joined with ShellWindows tabs (what the tracker will do)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct ScannedTab {
    name: String,
    tab: isize,
    /// UIA-selected (the window's active tab).
    selected: bool,
}

/// Rescans `top`: every UIA tab, in order, must match exactly one ShellWindows
/// tab of `top` by folder, whose HWND is a `ShellTabWindowClass` under `top`.
fn rescan(uia: &Uia, top: isize) -> Result<Vec<ScannedTab>, String> {
    let ui = uia.tabs(top);
    let sh = shell_tabs(top);
    if ui.len() != sh.len() {
        return Err(format!(
            "window {top:#x}: UIA has {} tab(s), ShellWindows {}",
            ui.len(),
            sh.len()
        ));
    }
    if ui.windows(2).any(|w| w[0].rect.left >= w[1].rect.left) {
        return Err(format!(
            "window {top:#x}: UIA tab rects are not left-to-right: {:?}",
            ui.iter()
                .map(|t| (t.name.as_str(), t.rect.left, t.rect.right))
                .collect::<Vec<_>>()
        ));
    }
    let mut out = Vec::new();
    for t in &ui {
        let matches: Vec<&ShellTabInfo> = sh
            .iter()
            .filter(|s| uia_matches(&t.name, &s.name))
            .collect();
        let [m] = matches.as_slice() else {
            return Err(format!(
                "window {top:#x}: UIA tab {:?} matches {} ShellWindows tab(s)",
                t.name,
                matches.len()
            ));
        };
        if window_class_name(hwnd(m.tab)) != SHELL_TAB_WINDOW_CLASS || root_of(hwnd(m.tab)) != top {
            return Err(format!(
                "window {top:#x}: tab HWND {:#x} of {:?} is not a ShellTabWindowClass under it",
                m.tab, t.name
            ));
        }
        out.push(ScannedTab {
            // The full folder path (from ShellWindows), whatever form UIA uses.
            name: m.name.clone(),
            tab: m.tab,
            selected: t.selected,
        });
    }
    Ok(out)
}

fn scan_names(scan: &[ScannedTab]) -> Vec<String> {
    scan.iter().map(|t| t.name.clone()).collect()
}

/// Rescans until the UIA names equal `expected` and the rescan is consistent.
fn rescan_expect(
    uia: &Uia,
    top: isize,
    expected: &[&str],
) -> (Option<Duration>, Result<Vec<ScannedTab>, String>) {
    let mut last = Err("not scanned".to_string());
    let t = wait_until(MOVE_TIMEOUT, || {
        last = rescan(uia, top);
        last.as_ref()
            .is_ok_and(|s| names_eq(&scan_names(s), expected))
    });
    (t, last)
}

// ---------------------------------------------------------------------------
// Opening windows and tabs
// ---------------------------------------------------------------------------

/// Opens a new window on `dir`, adopts it, and returns it once UIA shows it.
fn open_window(guard: &ExplorerGuard, uia: &Uia, dir: &Path) -> Result<isize, String> {
    let path = dir.to_string_lossy().to_string();
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
    let mut found = None;
    let mut seen: Vec<isize> = Vec::new();
    wait_until(Duration::from_secs(15), || {
        for (top, _) in locate(&path) {
            if guard.is_pre_existing(hwnd(top)) {
                continue;
            }
            // New and showing our temp folder: adopt at once.
            guard.adopt(hwnd(top));
            if !seen.contains(&top) {
                seen.push(top);
            }
            if shell_tabs(top).len() == 1 && names_eq(&uia.names(top), &[path.as_str()]) {
                found = Some(top);
                return true;
            }
        }
        false
    });
    found.ok_or_else(|| {
        let diag: Vec<String> = seen
            .iter()
            .map(|&t| {
                format!(
                    "{t:#x}: {} ShellWindows tab(s) {:?}; UIA TabItems (class, name) {:?}",
                    shell_tabs(t).len(),
                    shell_tabs(t)
                        .iter()
                        .map(|s| s.name.clone())
                        .collect::<Vec<_>>(),
                    uia.dump_tab_items(t)
                )
            })
            .collect();
        format!("window on {path} did not appear as expected; new windows: {diag:?}")
    })
}

/// Appends a tab to `top` and navigates it to `dir`.
fn add_tab(uia: &Uia, top: isize, dir: &Path) -> Result<(), String> {
    let path = dir.to_string_lossy().to_string();
    let before: Vec<isize> = shell_tabs(top).iter().map(|t| t.tab).collect();
    let post_to = *before.first().ok_or("window has no tab")?;
    // SAFETY: posting a message to a tab of an adopted test window.
    unsafe {
        PostMessageW(
            Some(hwnd(post_to)),
            WM_COMMAND,
            WPARAM(CMD_NEW_TAB),
            LPARAM(0),
        )
    }
    .map_err(|e| e.to_string())?;
    let mut browser = None;
    wait_until(MOVE_TIMEOUT, || {
        browser = shell_tabs(top)
            .into_iter()
            .find(|t| t.tab != 0 && !before.contains(&t.tab))
            .map(|t| t.browser);
        browser.is_some()
    });
    let browser = browser.ok_or("no new tab appeared")?;
    pump_for(Duration::from_millis(250));
    navigate_no_history(&browser, dir)?;
    wait_until(MOVE_TIMEOUT, || {
        folder_of(&browser).is_some_and(|n| same_path(&n, &path))
            && uia.names(top).iter().any(|n| uia_matches(n, &path))
    })
    .map(|_| ())
    .ok_or_else(|| format!("new tab did not reach {path}"))
}

// ---------------------------------------------------------------------------
// Drop pad: a plain window with no drop target, so a dropped tab tears out
// ---------------------------------------------------------------------------

unsafe extern "system" fn pad_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: forwarding the original arguments.
    unsafe { DefWindowProcW(h, msg, wp, lp) }
}

struct Pad {
    hwnd: Option<HWND>,
    registered: bool,
}

impl Pad {
    fn create(r: RECT) -> Result<Self, String> {
        let mut pad = Pad {
            hwnd: None,
            registered: false,
        };
        // SAFETY: standard class registration and window creation on this thread.
        unsafe {
            let hinst = GetModuleHandleW(None).map_err(|e| e.to_string())?;
            let wc = WNDCLASSW {
                lpfnWndProc: Some(pad_proc),
                hInstance: hinst.into(),
                hbrBackground: HBRUSH(GetStockObject(GRAY_BRUSH).0),
                lpszClassName: w!("EsrSpikeR5Pad"),
                ..Default::default()
            };
            pad.registered = RegisterClassW(&wc) != 0;
            let h = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
                w!("EsrSpikeR5Pad"),
                w!("ESR spike R5 drop pad"),
                WS_POPUP | WS_VISIBLE,
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                None,
                None,
                Some(hinst.into()),
                None,
            )
            .map_err(|e| e.to_string())?;
            let _ = UpdateWindow(h);
            pad.hwnd = Some(h);
        }
        Ok(pad)
    }

    fn raw(&self) -> isize {
        self.hwnd.map_or(0, raw)
    }
}

impl Drop for Pad {
    fn drop(&mut self) {
        if let Some(h) = self.hwnd.take() {
            // SAFETY: window created by this thread.
            let _ = unsafe { DestroyWindow(h) };
        }
        if self.registered {
            // SAFETY: class registered by this test; its window is destroyed.
            unsafe {
                let hinst = GetModuleHandleW(None).unwrap_or_default();
                let _ = UnregisterClassW(w!("EsrSpikeR5Pad"), Some(hinst.into()));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Mouse: SendInput drags; cursor restored and button released on drop
// ---------------------------------------------------------------------------

struct Mouse {
    saved: POINT,
    down: bool,
}

impl Mouse {
    fn new() -> Self {
        let mut saved = POINT::default();
        // SAFETY: valid out pointer.
        let _ = unsafe { GetCursorPos(&mut saved) };
        Self { saved, down: false }
    }

    fn send(flags: MOUSE_EVENT_FLAGS, p: POINT) {
        // SAFETY: plain metric reads.
        let (vx, vy, vw, vh) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN) as i64,
                GetSystemMetrics(SM_YVIRTUALSCREEN) as i64,
                GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2) as i64,
                GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2) as i64,
            )
        };
        let dx = ((p.x as i64 - vx) * 65535 / (vw - 1)) as i32;
        let dy = ((p.y as i64 - vy) * 65535 / (vh - 1)) as i32;
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: 0,
                    dwFlags: flags
                        | MOUSEEVENTF_MOVE
                        | MOUSEEVENTF_ABSOLUTE
                        | MOUSEEVENTF_VIRTUALDESK,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        // SAFETY: one fully initialized INPUT.
        unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
    }

    fn move_to(&self, p: POINT) {
        Self::send(MOUSE_EVENT_FLAGS(0), p);
    }

    fn press(&mut self, p: POINT) {
        self.down = true;
        Self::send(MOUSEEVENTF_LEFTDOWN, p);
    }

    fn release(&mut self, p: POINT) {
        Self::send(MOUSEEVENTF_LEFTUP, p);
        self.down = false;
    }

    /// A short drag from `from` to `to`; the button is always released.
    fn drag(&mut self, from: POINT, to: POINT) {
        self.move_to(from);
        pump_for(Duration::from_millis(150));
        self.press(from);
        pump_for(Duration::from_millis(150));
        // Leave the drag threshold slowly, then move in steps.
        for i in 1..=5 {
            self.move_to(POINT {
                x: from.x,
                y: from.y + 2 * i,
            });
            pump_for(Duration::from_millis(25));
        }
        let start = POINT {
            x: from.x,
            y: from.y + 10,
        };
        const STEPS: i32 = 30;
        for i in 1..=STEPS {
            self.move_to(POINT {
                x: start.x + (to.x - start.x) * i / STEPS,
                y: start.y + (to.y - start.y) * i / STEPS,
            });
            pump_for(Duration::from_millis(15));
        }
        // Hover so the target registers the drag-over, then drop.
        self.move_to(POINT {
            x: to.x + 3,
            y: to.y,
        });
        pump_for(Duration::from_millis(150));
        self.move_to(to);
        pump_for(Duration::from_millis(400));
        self.release(to);
        pump_for(Duration::from_millis(200));
    }
}

impl Drop for Mouse {
    fn drop(&mut self) {
        if self.down {
            let mut p = POINT::default();
            // SAFETY: valid out pointer.
            let _ = unsafe { GetCursorPos(&mut p) };
            self.release(p);
        }
        // SAFETY: plain call.
        let _ = unsafe { SetCursorPos(self.saved.x, self.saved.y) };
    }
}

fn press_point(t: &UiaTab) -> POINT {
    // Left part of the tab (away from its close button), vertical centre.
    let w = t.rect.right - t.rect.left;
    POINT {
        x: t.rect.left + (w / 3).min(40),
        y: (t.rect.top + t.rect.bottom) / 2,
    }
}

// ---------------------------------------------------------------------------
// The spike
// ---------------------------------------------------------------------------

#[derive(Default, Debug)]
struct Outcome {
    w1: isize,
    w2: isize,
    w3: Option<isize>,
    a2_before: isize,
    other_before: Vec<(String, isize)>,
    out_error: Option<String>,
    out_registered_ms: Option<u128>,
    out_rescan_ms: Option<u128>,
    a2_after_out: Option<isize>,
    w1_after_out: Option<Result<Vec<ScannedTab>, String>>,
    w3_after_out: Option<Result<Vec<ScannedTab>, String>>,
    in_error: Option<String>,
    in_registered_ms: Option<u128>,
    in_rescan_ms: Option<u128>,
    a2_after_in: Option<isize>,
    w1_after_in: Option<Result<Vec<ScannedTab>, String>>,
    w2_after_in: Option<Result<Vec<ScannedTab>, String>>,
    w2_predicted: bool,
    w3_closed_after_in: Option<bool>,
    others_stable: Option<bool>,
}

fn describe_hwnd(h: isize) -> String {
    if is_window(h) {
        format!(
            "{h:#x} (alive, class {}, root {:#x})",
            window_class_name(hwnd(h)),
            root_of(hwnd(h))
        )
    } else {
        format!("{h:#x} (destroyed)")
    }
}

fn run(guard: &ExplorerGuard, uia: &Uia, out: &mut Outcome) -> Result<(), String> {
    let dir = |n: &str| -> PathBuf { guard.dir(n) };
    let (a1, a2, a3, b1) = (dir("r5_a1"), dir("r5_a2"), dir("r5_a3"), dir("r5_b1"));
    let s = |p: &Path| p.to_string_lossy().to_string();
    let (sa1, sa2, sa3, sb1) = (s(&a1), s(&a2), s(&a3), s(&b1));

    // Layout on the primary work area (physical pixels).
    let wa = work_area();
    let m = 40;
    let ww = wa.right - wa.left;
    let wh = wa.bottom - wa.top;
    // Side by side (2 x 2) on a wide work area. On a narrow one (e.g. a portrait
    // monitor at high DPI) the tab strip would overflow and scroll tabs out of
    // view, so the windows are stacked in one column instead: W1, pad, W2, W3.
    let (r_w1, r_w2, r_pad, r_w3) = if (ww - 3 * m) / 2 >= 1000 {
        let col = 1000;
        let h = ((wh - 3 * m) / 2).min(360);
        (
            rect(wa.left + m, wa.top + m, col, h),
            rect(wa.left + 2 * m + col, wa.top + m, col, h),
            rect(wa.left + m, wa.top + 2 * m + h, col, h),
            rect(wa.left + 2 * m + col, wa.top + 2 * m + h, col, h),
        )
    } else {
        let col = (ww - 2 * m).min(1400);
        let h = ((wh - 5 * m) / 4).min(420);
        let row = |i: i32| rect(wa.left + m, wa.top + m + i * (h + m), col, h);
        (row(0), row(2), row(1), row(3))
    };
    println!("layout: W1 {r_w1:?} W2 {r_w2:?} pad {r_pad:?} W3 {r_w3:?}");

    // W1: a1, a2, a3. W2: b1.
    let w1 = open_window(guard, uia, &a1)?;
    out.w1 = w1;
    add_tab(uia, w1, &a2)?;
    add_tab(uia, w1, &a3)?;
    let w2 = open_window(guard, uia, &b1)?;
    out.w2 = w2;
    place(w1, r_w1);
    place(w2, r_w2);
    pump_for(Duration::from_millis(800));

    let (_, scan1) = rescan_expect(uia, w1, &[&sa1, &sa2, &sa3]);
    let scan1 = scan1.map_err(|e| format!("initial W1 rescan: {e}"))?;
    let (_, scan2) = rescan_expect(uia, w2, &[&sb1]);
    let scan2 = scan2.map_err(|e| format!("initial W2 rescan: {e}"))?;
    println!("initial W1 {w1:#x}: {scan1:?}");
    println!("initial W2 {w2:#x}: {scan2:?}");
    out.a2_before = scan1[1].tab;
    out.other_before = scan1
        .iter()
        .chain(&scan2)
        .filter(|t| !same_path(&t.name, &sa2))
        .map(|t| (t.name.clone(), t.tab))
        .collect();

    let mut mouse = Mouse::new();

    // ---------------- drag-out ----------------
    println!("== drag-out: {sa2} from W1 onto the drop pad");
    let pad = Pad::create(r_pad)?;
    pump_for(Duration::from_millis(400));
    let tab = uia
        .tab_named(w1, &sa2)
        .ok_or("UIA tab r5_a2 not found in W1")?;
    let from = press_point(&tab);
    let to = POINT {
        x: (r_pad.left + r_pad.right) / 2,
        y: (r_pad.top + r_pad.bottom) / 2,
    };
    println!("  press point: {}", describe_at(from));
    if !over(from, w1) {
        return Err(format!(
            "drag-out aborted: the point {from:?} is not over W1: {}",
            describe_at(from)
        ));
    }
    if root_at(to) != pad.raw() {
        return Err(format!(
            "drag-out aborted: the drop point {to:?} is not over the pad: {}",
            describe_at(to)
        ));
    }
    mouse.drag(from, to);
    let released = Instant::now();
    // The new window: not pre-existing, not W1/W2, shows r5_a2.
    let mut w3 = None;
    let reg = wait_until(MOVE_TIMEOUT, || {
        for (top, _) in locate(&sa2) {
            if top != w1
                && top != w2
                && !guard.is_pre_existing(hwnd(top))
                && is_cabinet_window(hwnd(top))
            {
                guard.adopt(hwnd(top));
                w3 = Some(top);
            }
        }
        w3.is_some() && !locate(&sa2).iter().any(|(t, _)| *t == w1)
    });
    drop(pad);
    out.out_registered_ms = reg.map(|_| ms(released.elapsed()));
    let Some(w3) = w3 else {
        out.out_error = Some("no new window showing r5_a2 appeared after the drag-out".into());
        println!("  W1 UIA now: {:?}", uia.names(w1));
        return Ok(());
    };
    out.w3 = Some(w3);
    let (t1, s1) = rescan_expect(uia, w1, &[&sa1, &sa3]);
    let (t3, s3) = rescan_expect(uia, w3, &[&sa2]);
    out.out_rescan_ms = t1.zip(t3).map(|_| ms(released.elapsed()));
    out.a2_after_out = s3.as_ref().ok().and_then(|s| s.first()).map(|t| t.tab);
    println!(
        "  W3 {w3:#x} registered after {:?} ms",
        out.out_registered_ms
    );
    println!("  W1 rescan: {s1:?}");
    println!("  W3 rescan: {s3:?}");
    out.w1_after_out = Some(s1);
    out.w3_after_out = Some(s3.clone());
    if s3.is_err() {
        out.out_error = Some("W3 rescan did not show exactly r5_a2".into());
        return Ok(());
    }

    // ---------------- drag-in ----------------
    println!("== drag-in: {sa2} from W3 into W2 after {sb1}");
    place(w3, r_w3);
    pump_for(Duration::from_millis(800));
    let tab = uia
        .tab_named(w3, &sa2)
        .ok_or("UIA tab r5_a2 not found in W3")?;
    let target = uia
        .tab_named(w2, &sb1)
        .ok_or("UIA tab r5_b1 not found in W2")?;
    let from = press_point(&tab);
    // Just past the right edge of W2's last tab (still on its tab strip, before
    // the "+" button). Variant 1 dropped at 3/4 of `r5_b1` and the tab landed
    // *before* it: while hovering, TabView opens the insertion gap at the hovered
    // tab and shifts `r5_b1` right, so the pointer ends up in the gap at index 0.
    let to = POINT {
        x: target.rect.right + ((target.rect.bottom - target.rect.top) / 4).max(6),
        y: (target.rect.top + target.rect.bottom) / 2,
    };
    println!("  press point: {}", describe_at(from));
    println!("  drop point: {}", describe_at(to));
    if !over(from, w3) {
        return Err(format!(
            "drag-in aborted: the point {from:?} is not over W3: {}",
            describe_at(from)
        ));
    }
    if !over(to, w2) {
        return Err(format!(
            "drag-in aborted: the drop point {to:?} is not over W2: {}",
            describe_at(to)
        ));
    }
    mouse.drag(from, to);
    let released = Instant::now();
    let reg = wait_until(MOVE_TIMEOUT, || {
        let l = locate(&sa2);
        l.len() == 1 && l[0].0 == w2
    });
    out.in_registered_ms = reg.map(|_| ms(released.elapsed()));
    if reg.is_none() {
        out.in_error = Some(format!(
            "r5_a2 is not (only) in W2 after the drag-in: {:?}",
            locate(&sa2)
        ));
    }
    // Wait for W2 to hold both tabs, then take the order UIA reports.
    let mut s2 = Err("not scanned".to_string());
    let t2 = wait_until(MOVE_TIMEOUT, || {
        s2 = rescan(uia, w2);
        s2.as_ref().is_ok_and(|s| {
            s.len() == 2
                && s.iter().any(|t| same_path(&t.name, &sa2))
                && s.iter().any(|t| same_path(&t.name, &sb1))
        })
    });
    out.in_rescan_ms = t2.map(|_| ms(released.elapsed()));
    // A second, independent rescan after settling must agree.
    pump_for(Duration::from_millis(700));
    let s2_again = rescan(uia, w2);
    if let (Ok(a), Ok(b)) = (&s2, &s2_again)
        && scan_names(a) != scan_names(b)
    {
        out.in_error = Some(format!("W2 rescans disagree: {a:?} vs {b:?}"));
    }
    out.w2_predicted = s2
        .as_ref()
        .is_ok_and(|s| names_eq(&scan_names(s), &[&sb1, &sa2]));
    out.a2_after_in = s2
        .as_ref()
        .ok()
        .and_then(|s| s.iter().find(|t| same_path(&t.name, &sa2)))
        .map(|t| t.tab);
    let (_, s1) = rescan_expect(uia, w1, &[&sa1, &sa3]);
    out.w3_closed_after_in =
        Some(wait_until(Duration::from_secs(2), || !is_cabinet_window(hwnd(w3))).is_some());
    println!("  W2 rescan: {s2:?}");
    println!("  W1 rescan: {s1:?}");
    // Tabs that did not move keep their HWNDs.
    let now: Vec<(String, isize)> = s1
        .iter()
        .chain(s2.iter())
        .flatten()
        .map(|t| (t.name.clone(), t.tab))
        .collect();
    out.others_stable = Some(
        out.other_before
            .iter()
            .all(|(n, h)| now.iter().any(|(n2, h2)| same_path(n, n2) && h == h2)),
    );
    out.w1_after_in = Some(s1);
    out.w2_after_in = Some(s2);
    Ok(())
}

#[test]
#[ignore = "interactive spike: drags tabs with the real mouse; opens and closes its own Explorer windows"]
fn spike_r5_tab_identity_across_drag_out_and_in() {
    // Physical pixels everywhere (UIA rects, SendInput, window placement).
    // SAFETY: plain calls; failure (already set) is fine.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let _sta = StaGuard::init().expect("STA init");
    let uia = Uia::new().expect("create UIA");
    let guard = ExplorerGuard::new("r5");

    let mut out = Outcome::default();
    let result = run(&guard, &uia, &mut out);
    println!(
        "  after run: W1 {:#x} alive={} W2 {:#x} alive={}",
        out.w1,
        is_cabinet_window(hwnd(out.w1)),
        out.w2,
        is_cabinet_window(hwnd(out.w2))
    );
    // `run` dropped the mouse guard (cursor restored, button up) and the pad.

    let ok_scan = |r: &Option<Result<Vec<ScannedTab>, String>>, expected: usize| {
        r.as_ref()
            .is_some_and(|r| r.as_ref().is_ok_and(|s| s.len() == expected))
    };
    println!("\n== SUMMARY (R5)");
    println!("  setup/abort error: {:?}", result.as_ref().err());
    println!("  r5_a2 tab HWND before: {}", describe_hwnd(out.a2_before));
    println!(
        "  drag-out: error={:?} new window={:?} registered after {:?} ms, rescans consistent after {:?} ms",
        out.out_error, out.w3, out.out_registered_ms, out.out_rescan_ms
    );
    if let Some(h) = out.a2_after_out {
        println!(
            "  drag-out: r5_a2 tab HWND after = {h:#x} -> {}",
            if h == out.a2_before {
                "SURVIVES (same HWND, re-parented)"
            } else {
                "RECREATED (new HWND)"
            }
        );
        println!("            old HWND now: {}", describe_hwnd(out.a2_before));
    }
    println!(
        "  drag-in: error={:?} registered after {:?} ms, W2 rescan settled after {:?} ms, \
         order as predicted [b1, a2]={}, W3 closed={:?}",
        out.in_error,
        out.in_registered_ms,
        out.in_rescan_ms,
        out.w2_predicted,
        out.w3_closed_after_in
    );
    if let (Some(h), Some(prev)) = (out.a2_after_in, out.a2_after_out) {
        println!(
            "  drag-in: r5_a2 tab HWND after = {h:#x} -> {}",
            if h == prev {
                "SURVIVES (same HWND, re-parented)"
            } else {
                "RECREATED (new HWND)"
            }
        );
        println!("            previous HWND now: {}", describe_hwnd(prev));
    }
    println!("  unmoved tabs keep their HWNDs: {:?}", out.others_stable);
    let selected = |r: &Option<Result<Vec<ScannedTab>, String>>| -> Option<String> {
        r.as_ref()?
            .as_ref()
            .ok()?
            .iter()
            .find(|t| t.selected)
            .map(|t| t.name.clone())
    };
    println!(
        "  active tab after drag-in: W1 {:?}, W2 {:?}",
        selected(&out.w1_after_in),
        selected(&out.w2_after_in)
    );
    println!("  W1 after drag-in: {:?}", out.w1_after_in);
    println!("  W2 after drag-in: {:?}", out.w2_after_in);

    let cleanup = guard.finish();
    if let Err(e) = &cleanup {
        println!("  cleanup: {e}");
    }

    result.expect("R5 setup failed or a drag was aborted");
    assert!(out.out_error.is_none(), "drag-out: {:?}", out.out_error);
    assert!(
        ok_scan(&out.w1_after_out, 2) && ok_scan(&out.w3_after_out, 1),
        "drag-out: rescans of W1 and the new window do not show [a1, a3] and [a2]"
    );
    assert!(out.in_error.is_none(), "drag-in: {:?}", out.in_error);
    assert!(
        ok_scan(&out.w1_after_in, 2) && ok_scan(&out.w2_after_in, 2),
        "drag-in: rescans of W1 and W2 do not show [a1, a3] and both of W2's tabs"
    );
    assert!(
        out.w2_predicted,
        "drag-in: W2's rescan does not show the tab at its drop position [b1, a2]: {:?}",
        out.w2_after_in
    );
    assert_eq!(
        out.w3_closed_after_in,
        Some(true),
        "drag-in: the emptied source window did not close"
    );
    assert_eq!(
        out.others_stable,
        Some(true),
        "tabs that were not dragged changed their HWND"
    );
    cleanup.expect("test windows left open");
}
