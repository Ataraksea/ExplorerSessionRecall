//! Helpers shared by the tab-folding spikes F1–F4 (design.md, "Tab folding",
//! "Spike outcomes F1…F4"; Req 14).
//!
//! * [`HookThread`]: an out-of-context WinEvent hook
//!   (`EVENT_OBJECT_CREATE..EVENT_OBJECT_SHOW`, other processes only) on its
//!   own message-loop thread. It logs every create/destroy/show of a
//!   top-level `CabinetWClass` and of a `ShellTabWindowClass`, captures the
//!   key state (`GetAsyncKeyState`) and the virtual desktop when a new
//!   `CabinetWClass` is created, and applies a [`Hide`] method to it.
//! * [`Sampler`]: polls every new top-level `CabinetWClass` about once per
//!   millisecond and records the intervals in which it is *shown* (visible,
//!   not cloaked, not minimized, not layered with alpha 0, and intersecting
//!   the virtual screen), the method `esr-app/tests/restore_timing.rs` uses.
//! * Launchers (another process: `explorer.exe <args>`; this process:
//!   `ShellExecuteExW`, `SHOpenFolderAndSelectItems`), and small Shell helpers.

use std::collections::{HashMap, HashSet};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::explorer_guard::{ExplorerGuard, pump_once, top_level_cabinets};
use esr_win::shell_util::{
    StaGuard, current_folder_pidl, enumerate_all_explorer_windows, is_cabinet_window,
    pidl_to_bytes, window_class_name, window_process_id,
};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWMWA_CLOAK, DWMWA_CLOAKED, DwmGetWindowAttribute, DwmSetWindowAttribute,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_CONTROL, VK_LBUTTON, VK_SHIFT,
};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    ILCreateFromPathW, ILFindLastID, ILFree, ILIsEqual, IVirtualDesktopManager, IWebBrowser2,
    SEE_MASK_FLAG_NO_UI, SEE_MASK_IDLIST, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, SHOpenFolderAndSelectItems,
    ShellExecuteExW, VirtualDesktopManager,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CHILDID_SELF, DispatchMessageW, EVENT_OBJECT_CREATE, EVENT_OBJECT_DESTROY,
    EVENT_OBJECT_NAMECHANGE, EVENT_OBJECT_SHOW, EVENT_SYSTEM_FOREGROUND,
    GA_ROOT, GWL_EXSTYLE, GetAncestor, GetLayeredWindowAttributes, GetMessageW, GetShellWindow,
    GetSystemMetrics, GetWindowLongPtrW, GetWindowRect, IsIconic, IsWindow, IsWindowVisible,
    LAYERED_WINDOW_ATTRIBUTES_FLAGS, LWA_ALPHA, MSG, OBJID_WINDOW, PostThreadMessageW,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SW_HIDE,
    SW_SHOWNORMAL, SWP_ASYNCWINDOWPOS, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOZORDER, SetLayeredWindowAttributes, SetWindowLongPtrW, SetWindowPos, ShowWindowAsync,
    TranslateMessage, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_QUIT, WS_EX_LAYERED,
};
use windows::core::{GUID, HSTRING, PCWSTR};

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

pub fn hwnd(raw: isize) -> HWND {
    HWND(raw as *mut _)
}

pub fn raw(h: HWND) -> isize {
    h.0 as isize
}

pub fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

pub fn ms_between(a: Instant, b: Instant) -> f64 {
    if b >= a { ms(b - a) } else { -ms(a - b) }
}

/// Polls `f` (pumping messages) until it returns true; returns the elapsed time.
pub fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> Option<Duration> {
    let start = Instant::now();
    loop {
        pump_once();
        if f() {
            return Some(start.elapsed());
        }
        if start.elapsed() > timeout {
            return None;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// "n=… min … med … p90 … max …" (ms).
pub fn stats(values: &[f64]) -> String {
    if values.is_empty() {
        return "n=0".into();
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pick = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
    format!(
        "n={} min {:.0} med {:.0} p90 {:.0} max {:.0} ms",
        v.len(),
        v[0],
        pick(0.5),
        pick(0.9),
        v[v.len() - 1]
    )
}

pub fn is_window(raw_hwnd: isize) -> bool {
    // SAFETY: IsWindow tolerates stale handles.
    unsafe { IsWindow(Some(hwnd(raw_hwnd))) }.as_bool()
}

pub fn window_rect(h: HWND) -> RECT {
    let mut r = RECT::default();
    // SAFETY: valid out pointer; stale HWNDs just fail.
    let _ = unsafe { GetWindowRect(h, &mut r) };
    r
}

/// PID of the shell (`GetShellWindow`), to detect an Explorer restart.
pub fn shell_pid() -> u32 {
    // SAFETY: plain call.
    let shell = unsafe { GetShellWindow() };
    if shell.is_invalid() { 0 } else { window_process_id(shell) }
}

pub fn pidl_of_path(path: &Path) -> Result<Vec<u8>, String> {
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

pub fn same_pidl(a: &[u8], b: &[u8]) -> bool {
    // SAFETY: both buffers are terminated PIDLs.
    unsafe { ILIsEqual(a.as_ptr().cast(), b.as_ptr().cast()) }.as_bool()
}

/// ShellWindows tabs (browsers) of the window `top`, in collection order.
pub fn browsers_of(top: isize) -> Vec<(Option<isize>, IWebBrowser2)> {
    enumerate_all_explorer_windows()
        .ok()
        .and_then(|groups| groups.into_iter().find(|g| raw(g.top_hwnd) == top))
        .map(|g| {
            g.tabs
                .into_iter()
                .map(|t| (t.tab_hwnd.map(raw), t.browser))
                .collect()
        })
        .unwrap_or_default()
}

/// Whether some tab of `top` shows `target`.
pub fn window_shows(top: isize, target: &[u8]) -> bool {
    browsers_of(top).iter().any(|(_, b)| {
        current_folder_pidl(b).is_ok_and(|p| same_pidl(&p, target))
    })
}

/// New top-level `CabinetWClass` windows (not in `pre`).
pub fn new_cabinets(pre: &HashSet<isize>) -> Vec<isize> {
    top_level_cabinets().into_iter().filter(|h| !pre.contains(h)).collect()
}

// ---------------------------------------------------------------------------
// Launchers
// ---------------------------------------------------------------------------

/// `explorer.exe <raw args>` as a separate process (the way another
/// application or a shortcut asks for a folder). The child exits at once
/// after handing the request to the running Explorer.
pub fn spawn_explorer(raw_args: &str) -> Result<(), String> {
    std::process::Command::new(r"C:\Windows\explorer.exe")
        .raw_arg(raw_args)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("spawn explorer.exe {raw_args}: {e}"))
}

/// `ShellExecuteExW(verb, path, SW_SHOWNORMAL)` on its own STA thread (verb
/// `None` = the default verb, as a desktop double-click). Returns the call's
/// duration in ms.
pub fn shell_execute(path: &Path, verb: Option<&'static str>) -> JoinHandle<Result<f64, String>> {
    let path = path.to_path_buf();
    std::thread::spawn(move || {
        let _sta = StaGuard::init().map_err(|e| e.to_string())?;
        let verb_h = verb.map(HSTRING::from);
        let pidl = pidl_of_path(&path)?;
        let t = Instant::now();
        let mut sei = SHELLEXECUTEINFOW {
            cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_IDLIST | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
            lpVerb: verb_h.as_ref().map_or(PCWSTR::null(), |v| PCWSTR(v.as_ptr())),
            lpIDList: pidl.as_ptr() as *mut _,
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };
        // SAFETY: the strings and the PIDL outlive the call.
        unsafe { ShellExecuteExW(&mut sei) }.map_err(|e| format!("ShellExecuteExW: {e}"))?;
        Ok(ms(t.elapsed()))
    })
}

/// `SHOpenFolderAndSelectItems(dir, [file])` on its own STA thread (what
/// "Show in folder" of most applications calls).
pub fn open_folder_and_select(dir: &Path, file: &Path) -> JoinHandle<Result<f64, String>> {
    let (dir, file) = (dir.to_path_buf(), file.to_path_buf());
    std::thread::spawn(move || {
        let _sta = StaGuard::init().map_err(|e| e.to_string())?;
        let t = Instant::now();
        // SAFETY: the PIDLs are created and freed here; the child ID points into `item`.
        unsafe {
            let folder = ILCreateFromPathW(&HSTRING::from(dir.as_os_str()));
            let item = ILCreateFromPathW(&HSTRING::from(file.as_os_str()));
            if folder.is_null() || item.is_null() {
                return Err("ILCreateFromPathW failed".into());
            }
            let child = ILFindLastID(item) as *const ITEMIDLIST;
            let r = SHOpenFolderAndSelectItems(folder, Some(&[child]), 0);
            ILFree(Some(folder));
            ILFree(Some(item));
            r.map_err(|e| format!("SHOpenFolderAndSelectItems: {e}"))?;
        }
        Ok(ms(t.elapsed()))
    })
}

/// Opens a visible window on `dir` (`opennewwindow`, `SW_SHOWNORMAL`), adopts
/// it and waits until it shows `dir` and has settled.
pub fn open_primary(guard: &ExplorerGuard, dir: &Path) -> Result<isize, String> {
    let pre: HashSet<isize> = top_level_cabinets().into_iter().collect();
    let target = pidl_of_path(dir)?;
    let launcher = shell_execute(dir, Some("opennewwindow"));
    let mut found = None;
    wait_until(Duration::from_secs(10), || {
        for h in new_cabinets(&pre) {
            guard.adopt(hwnd(h));
            if window_shows(h, &target) {
                found = Some(h);
                return true;
            }
        }
        false
    });
    let _ = launcher.join();
    let top = found.ok_or_else(|| format!("no window showing {}", dir.display()))?;
    // Let Explorer finish building the window (XAML, view) before it is used.
    super::explorer_guard::pump_for(Duration::from_millis(1200));
    Ok(top)
}

// ---------------------------------------------------------------------------
// Hiding methods (F1)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hide {
    /// Baseline: nothing is done.
    None,
    /// `ShowWindowAsync(SW_HIDE)` at `EVENT_OBJECT_CREATE` and `EVENT_OBJECT_SHOW`.
    AsyncShowWindow,
    /// `SetWindowPos(SWP_HIDEWINDOW | SWP_ASYNCWINDOWPOS)` at create and show.
    SwpHideWindow,
    /// `SetWindowPos(-32000, -32000, SWP_ASYNCWINDOWPOS)` at create and show.
    Offscreen,
    /// `WS_EX_LAYERED` + alpha 0 at create (R1's method).
    AlphaAtCreate,
    /// `WS_EX_LAYERED` + alpha 0 at show (ExplorerTabUtility's default).
    AlphaAtShow,
    /// `DwmSetWindowAttribute(DWMWA_CLOAK)` at create (expected to fail cross-process).
    Cloak,
    /// Alpha 0 at create plus `ShowWindowAsync(SW_HIDE)` at create and show.
    AlphaAndShowAsync,
}

impl Hide {
    pub const ALL: [Hide; 8] = [
        Hide::None,
        Hide::AsyncShowWindow,
        Hide::SwpHideWindow,
        Hide::Offscreen,
        Hide::AlphaAtCreate,
        Hide::AlphaAtShow,
        Hide::Cloak,
        Hide::AlphaAndShowAsync,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Hide::None => "none",
            Hide::AsyncShowWindow => "showasync-hide",
            Hide::SwpHideWindow => "swp-hide",
            Hide::Offscreen => "offscreen",
            Hide::AlphaAtCreate => "alpha-create",
            Hide::AlphaAtShow => "alpha-show",
            Hide::Cloak => "cloak",
            Hide::AlphaAndShowAsync => "alpha+hide",
        }
    }

    pub fn from_label(s: &str) -> Option<Hide> {
        Hide::ALL.into_iter().find(|h| h.label() == s)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Create,
    Show,
}

fn set_alpha0(h: HWND) -> String {
    // SAFETY: style and layered-attribute calls on a live top-level window of
    // the same integrity level.
    unsafe {
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
        let _ = SetWindowLongPtrW(h, GWL_EXSTYLE, ex | WS_EX_LAYERED.0 as isize);
        match SetLayeredWindowAttributes(h, COLORREF(0), 0, LWA_ALPHA) {
            Ok(()) => "alpha0 ok".into(),
            Err(e) => format!("alpha0 {:?}", e.code()),
        }
    }
}

fn show_async_hide(h: HWND) -> String {
    // SAFETY: plain asynchronous call on a window handle.
    let r = unsafe { ShowWindowAsync(h, SW_HIDE) };
    format!("ShowWindowAsync(SW_HIDE)={}", r.as_bool())
}

fn apply_hide(hide: Hide, h: HWND, stage: Stage) -> Option<String> {
    let swp = |x: i32, y: i32, flags| {
        // SAFETY: asynchronous SetWindowPos on a window handle.
        unsafe { SetWindowPos(h, None, x, y, 0, 0, flags) }
            .map_or_else(|e| format!("SetWindowPos {:?}", e.code()), |_| "SetWindowPos ok".into())
    };
    let common = SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS;
    match (hide, stage) {
        (Hide::None, _) => None,
        (Hide::AsyncShowWindow, _) => Some(show_async_hide(h)),
        (Hide::SwpHideWindow, _) => Some(swp(0, 0, common | SWP_NOMOVE | SWP_HIDEWINDOW)),
        (Hide::Offscreen, _) => Some(swp(-32000, -32000, common)),
        (Hide::AlphaAtCreate, Stage::Create) | (Hide::AlphaAtShow, Stage::Show) => {
            Some(set_alpha0(h))
        }
        (Hide::AlphaAtCreate, Stage::Show) | (Hide::AlphaAtShow, Stage::Create) => None,
        (Hide::Cloak, Stage::Create) => {
            let on: i32 = 1;
            // SAFETY: a BOOL-sized buffer for DWMWA_CLOAK.
            let r = unsafe {
                DwmSetWindowAttribute(h, DWMWA_CLOAK, (&on as *const i32).cast(), 4)
            };
            Some(match r {
                Ok(()) => "DWMWA_CLOAK ok".into(),
                Err(e) => format!("DWMWA_CLOAK {:?}", e.code()),
            })
        }
        (Hide::Cloak, Stage::Show) => None,
        (Hide::AlphaAndShowAsync, Stage::Create) => {
            Some(format!("{}; {}", set_alpha0(h), show_async_hide(h)))
        }
        (Hide::AlphaAndShowAsync, Stage::Show) => Some(show_async_hide(h)),
    }
}

/// Undoes a hide so the guard can close the window cleanly and Explorer does
/// not remember an off-screen position: alpha 0 is kept while the window is
/// moved back (callers close the window right after).
pub fn unhide_for_close(h: HWND, original: RECT) {
    // SAFETY: calls on a window created during the test.
    unsafe {
        let _ = SetWindowPos(
            h,
            None,
            original.left,
            original.top,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

// ---------------------------------------------------------------------------
// WinEvent hook thread
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct HookEvent {
    pub t: Instant,
    pub event: u32,
    pub hwnd: isize,
    /// "cabinet" or "tab".
    pub kind: &'static str,
    /// Root window (for tabs: the `CabinetWClass` that holds it at create time).
    pub root: isize,
}

impl HookEvent {
    pub fn name(&self) -> &'static str {
        match self.event {
            e if e == EVENT_OBJECT_CREATE => "CREATE",
            e if e == EVENT_OBJECT_DESTROY => "DESTROY",
            e if e == EVENT_OBJECT_SHOW => "SHOW",
            _ => "?",
        }
    }
}

/// A new top-level `CabinetWClass` seen by the hook.
#[derive(Clone, Debug)]
pub struct Created {
    pub hwnd: isize,
    /// When the hook first saw any event of the window or of one of its
    /// child windows ("first signal"); the hide is applied then.
    pub t_create: Instant,
    /// Which event that was, e.g. `child CREATE ShellTabWindowClass`.
    pub first_signal: String,
    /// When the window's own `EVENT_OBJECT_CREATE` arrived.
    pub t_own_create: Option<Instant>,
    /// `GetTickCount` at the callback minus the event's `dwmsEventTime`
    /// for the first signal: how late the event was delivered (ms).
    pub create_delivery_ms: u32,
    pub t_show: Option<Instant>,
    pub ctrl: bool,
    pub shift: bool,
    pub lbutton: bool,
    /// `GetForegroundWindow` when the first signal arrived (Req 14.22: the
    /// Foreground_Explorer_Window).
    pub foreground: isize,
    /// The latest foreground window other than this one, from the
    /// EVENT_SYSTEM_FOREGROUND events delivered by then.
    pub fg_before: isize,
    /// Whether this window's own foreground event had been delivered by then.
    pub fg_event_seen: bool,
    /// Visible (`WS_VISIBLE`) when the create callback ran.
    pub visible_at_create: bool,
    pub rect_at_create: RECT,
    pub desktop_at_create: Option<GUID>,
    /// Cost of `GetWindowDesktopId` in the callback, ms.
    pub desktop_ms: f64,
    pub apply: Vec<String>,
}

struct HookShared {
    hide: Hide,
    pre: HashSet<isize>,
    created: Vec<Created>,
    events: Vec<HookEvent>,
    tabs: HashSet<isize>,
    vdm: Option<IVirtualDesktopManager>,
    /// Windows that became foreground (EVENT_SYSTEM_FOREGROUND), oldest first.
    fg_history: Vec<isize>,
}

// SAFETY: only used on the hook thread (the interface is an MTA object
// created there); the Mutex is for reading results from the test thread.
unsafe impl Send for HookShared {}

static HOOK: Mutex<Option<HookShared>> = Mutex::new(None);

fn key_down(vk: u16) -> bool {
    // SAFETY: plain call.
    (unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000) != 0
}

unsafe extern "system" fn on_hook(
    _hook: HWINEVENTHOOK,
    event: u32,
    h: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    event_time: u32,
) {
    if id_object != OBJID_WINDOW.0 || id_child != CHILDID_SELF as i32 || h.is_invalid() {
        return;
    }
    let now = Instant::now();
    if event == EVENT_SYSTEM_FOREGROUND {
        if let Some(st) = HOOK.lock().unwrap().as_mut() {
            st.fg_history.push(raw(h));
        }
        return;
    }
    // Key state first: it is what a product would read at this moment.
    let keys = (key_down(VK_CONTROL.0), key_down(VK_SHIFT.0), key_down(VK_LBUTTON.0));
    let key = raw(h);
    let mut guard = HOOK.lock().unwrap();
    let Some(st) = guard.as_mut() else { return };
    if event == EVENT_OBJECT_DESTROY {
        if st.tabs.contains(&key) || st.created.iter().any(|c| c.hwnd == key) {
            let kind = if st.tabs.contains(&key) { "tab" } else { "cabinet" };
            st.events.push(HookEvent { t: now, event, hwnd: key, kind, root: 0 });
        }
        return;
    }
    let class = window_class_name(h);
    // SAFETY: plain call.
    let root = raw(unsafe { GetAncestor(h, GA_ROOT) });
    if class == "ShellTabWindowClass" && (event == EVENT_OBJECT_CREATE || event == EVENT_OBJECT_SHOW) {
        if event == EVENT_OBJECT_CREATE {
            st.tabs.insert(key);
        }
        st.events.push(HookEvent { t: now, event, hwnd: key, kind: "tab", root });
    }
    if root == 0 || st.pre.contains(&root) || !is_cabinet_window(hwnd(root)) {
        return;
    }
    let own = root == key;
    if own && (event == EVENT_OBJECT_CREATE || event == EVENT_OBJECT_SHOW) {
        st.events.push(HookEvent { t: now, event, hwnd: key, kind: "cabinet", root: key });
    }
    let hide = st.hide;
    let rh = hwnd(root);
    if !st.created.iter().any(|c| c.hwnd == root) {
        // First signal of a new top-level CabinetWClass: hide it now.
        // SAFETY: plain reads.
        let visible = unsafe { IsWindowVisible(rh) }.as_bool();
        let rect = window_rect(rh);
        let t_d = Instant::now();
        // SAFETY: plain COM call on the hook thread's MTA object.
        let desktop = st.vdm.as_ref().and_then(|v| unsafe { v.GetWindowDesktopId(rh) }.ok());
        let desktop_ms = ms(t_d.elapsed());
        let apply = apply_hide(hide, rh, Stage::Create).into_iter().collect();
        // SAFETY: plain call.
        let tick = unsafe { windows::Win32::System::SystemInformation::GetTickCount() };
        let name = match event {
            e if e == EVENT_OBJECT_CREATE => "CREATE".to_string(),
            e if e == EVENT_OBJECT_SHOW => "SHOW".to_string(),
            e => format!("0x{e:04x}"),
        };
        st.created.push(Created {
            hwnd: root,
            t_create: now,
            first_signal: if own { name } else { format!("child {name} {class}") },
            t_own_create: None,
            create_delivery_ms: tick.wrapping_sub(event_time),
            t_show: None,
            ctrl: keys.0,
            shift: keys.1,
            lbutton: keys.2,
            foreground: foreground(),
            fg_before: st.fg_history.iter().rev().copied().find(|w| *w != root).unwrap_or(0),
            fg_event_seen: st.fg_history.contains(&root),
            visible_at_create: visible,
            rect_at_create: rect,
            desktop_at_create: desktop,
            desktop_ms,
            apply,
        });
    }
    let Some(c) = st.created.iter_mut().find(|c| c.hwnd == root) else { return };
    if own && event == EVENT_OBJECT_CREATE && c.t_own_create.is_none() {
        c.t_own_create = Some(now);
    }
    if own && event == EVENT_OBJECT_SHOW {
        if c.t_show.is_none() {
            c.t_show = Some(now);
        }
        if let Some(a) = apply_hide(hide, rh, Stage::Show) {
            c.apply.push(format!("show: {a}"));
        }
    }
}

/// Out-of-context WinEvent hook on its own thread (see the module docs).
pub struct HookThread {
    thread_id: u32,
    join: Option<JoinHandle<()>>,
}

impl HookThread {
    pub fn start(hide: Hide, pre: HashSet<isize>) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let join = std::thread::spawn(move || {
            // SAFETY: balanced with CoUninitialize below.
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            // SAFETY: valid CLSID.
            let vdm: Option<IVirtualDesktopManager> =
                unsafe { CoCreateInstance(&VirtualDesktopManager, None, CLSCTX_ALL) }.ok();
            *HOOK.lock().unwrap() = Some(HookShared {
                hide,
                pre,
                created: Vec::new(),
                events: Vec::new(),
                tabs: HashSet::new(),
                vdm,
                // As a long-running hook would know it: the window that is foreground now.
                fg_history: vec![foreground()],
            });
            // SAFETY: the callback is a plain function; the hooks are removed below.
            let hook = unsafe {
                SetWinEventHook(
                    EVENT_OBJECT_CREATE,
                    EVENT_OBJECT_SHOW,
                    None,
                    Some(on_hook),
                    0,
                    0,
                    WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
                )
            };
            // Explorer's own later events (REORDER..NAMECHANGE, including
            // LOCATIONCHANGE), scoped to the shell process as the product's
            // per-PID hook is: they can precede the window's own CREATE.
            let shell = shell_pid();
            // SAFETY: as above.
            let hook2 = unsafe {
                SetWinEventHook(
                    EVENT_OBJECT_SHOW + 1,
                    EVENT_OBJECT_NAMECHANGE,
                    None,
                    Some(on_hook),
                    shell,
                    0,
                    WINEVENT_OUTOFCONTEXT,
                )
            };
            // The foreground history (Req 14.22), as the product's global
            // EVENT_SYSTEM_FOREGROUND hook sees it.
            // SAFETY: as above.
            let hook3 = unsafe {
                SetWinEventHook(
                    EVENT_SYSTEM_FOREGROUND,
                    EVENT_SYSTEM_FOREGROUND,
                    None,
                    Some(on_hook),
                    0,
                    0,
                    WINEVENT_OUTOFCONTEXT,
                )
            };
            // SAFETY: plain call.
            let _ = tx.send(unsafe { GetCurrentThreadId() });
            let mut msg = MSG::default();
            // SAFETY: standard message loop; out-of-context callbacks run inside it.
            unsafe {
                while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                let _ = UnhookWinEvent(hook);
                let _ = UnhookWinEvent(hook2);
                let _ = UnhookWinEvent(hook3);
            }
            if let Some(st) = HOOK.lock().unwrap().as_mut() {
                st.vdm = None;
            }
            // SAFETY: balanced with CoInitializeEx above.
            unsafe { CoUninitialize() };
        });
        let thread_id = rx.recv().expect("hook thread id");
        Self { thread_id, join: Some(join) }
    }

    pub fn created(&self) -> Vec<Created> {
        HOOK.lock().unwrap().as_ref().map(|s| s.created.clone()).unwrap_or_default()
    }

    pub fn events(&self) -> Vec<HookEvent> {
        HOOK.lock().unwrap().as_ref().map(|s| s.events.clone()).unwrap_or_default()
    }

    /// Also logs `DESTROY` of these (pre-existing) tab windows.
    pub fn watch_tabs(&self, tabs: impl IntoIterator<Item = isize>) {
        if let Some(s) = HOOK.lock().unwrap().as_mut() {
            s.tabs.extend(tabs);
        }
    }

    /// Stops the hook; returns what it saw.
    pub fn stop(mut self) -> (Vec<Created>, Vec<HookEvent>) {
        self.shutdown();
        let st = HOOK.lock().unwrap().take();
        st.map(|s| (s.created, s.events)).unwrap_or_default()
    }

    fn shutdown(&mut self) {
        if let Some(j) = self.join.take() {
            // SAFETY: posting WM_QUIT to the hook thread's queue.
            unsafe {
                let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
            }
            let _ = j.join();
        }
    }
}

impl Drop for HookThread {
    fn drop(&mut self) {
        self.shutdown();
        HOOK.lock().unwrap().take();
    }
}

// ---------------------------------------------------------------------------
// Visibility sampler
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct Track {
    pub first_seen: Option<Instant>,
    /// Closed or open intervals in which the window was shown.
    pub shown: Vec<(Instant, Option<Instant>)>,
    pub samples: u64,
    pub max_gap: Duration,
    last: Option<Instant>,
    /// State changes, "t_ms state" relative to `first_seen`.
    pub changes: Vec<String>,
    last_state: String,
}

impl Track {
    /// Total time shown, counting an open interval up to `end`.
    pub fn shown_ms(&self, end: Instant) -> f64 {
        self.shown
            .iter()
            .map(|(a, b)| ms(b.unwrap_or(end).saturating_duration_since(*a)))
            .sum()
    }
}

fn virtual_screen() -> RECT {
    // SAFETY: plain calls.
    unsafe {
        let x = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let y = GetSystemMetrics(SM_YVIRTUALSCREEN);
        RECT {
            left: x,
            top: y,
            right: x + GetSystemMetrics(SM_CXVIRTUALSCREEN),
            bottom: y + GetSystemMetrics(SM_CYVIRTUALSCREEN),
        }
    }
}

fn sample_state(h: HWND, screen: &RECT) -> (bool, String) {
    // SAFETY: tolerant reads; valid out pointers.
    unsafe {
        let visible = IsWindowVisible(h).as_bool();
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(h, DWMWA_CLOAKED, (&mut cloaked as *mut u32).cast(), 4);
        let layered = GetWindowLongPtrW(h, GWL_EXSTYLE) & WS_EX_LAYERED.0 as isize != 0;
        let alpha = if layered {
            let mut a = 255u8;
            let mut flags = LAYERED_WINDOW_ATTRIBUTES_FLAGS(0);
            match GetLayeredWindowAttributes(h, None, Some(&mut a), Some(&mut flags)) {
                Ok(()) if flags.0 & LWA_ALPHA.0 != 0 => Some(a),
                Ok(()) => Some(255),
                Err(_) => Some(0),
            }
        } else {
            None
        };
        let iconic = IsIconic(h).as_bool();
        let r = window_rect(h);
        let on_screen =
            r.right > screen.left && r.left < screen.right && r.bottom > screen.top && r.top < screen.bottom;
        let shown = visible && cloaked == 0 && !iconic && alpha != Some(0) && on_screen;
        let state = format!(
            "vis={} cloak={cloaked} alpha={alpha:?} iconic={iconic} rect=({},{})-({},{})",
            visible as u8, r.left, r.top, r.right, r.bottom
        );
        (shown, state)
    }
}

/// Samples every new top-level `CabinetWClass` (see the module docs).
pub struct Sampler {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<HashMap<isize, Track>>>,
}

impl Sampler {
    pub fn start(pre: HashSet<isize>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let join = std::thread::spawn(move || {
            let screen = virtual_screen();
            let mut tracks: HashMap<isize, Track> = HashMap::new();
            while !flag.load(Ordering::SeqCst) {
                let now = Instant::now();
                let mut live: Vec<isize> = new_cabinets(&pre);
                for k in tracks.keys() {
                    if !live.contains(k) {
                        live.push(*k);
                    }
                }
                for k in live {
                    let t = tracks.entry(k).or_default();
                    let alive = is_window(k);
                    let (shown, state) =
                        if alive { sample_state(hwnd(k), &screen) } else { (false, "gone".into()) };
                    let first = *t.first_seen.get_or_insert(now);
                    if let Some(last) = t.last {
                        t.max_gap = t.max_gap.max(now - last);
                    }
                    t.last = Some(now);
                    t.samples += 1;
                    let open = t.shown.last().is_some_and(|(_, b)| b.is_none());
                    if shown && !open {
                        t.shown.push((now, None));
                    } else if !shown && open {
                        t.shown.last_mut().unwrap().1 = Some(now);
                    }
                    if state != t.last_state && t.changes.len() < 40 && t.last_state != "gone" {
                        t.changes.push(format!("{:7.1} ms {state}", ms(now - first)));
                        t.last_state = state;
                    }
                }
                std::thread::sleep(Duration::from_micros(500));
            }
            tracks
        });
        Self { stop, join: Some(join) }
    }

    pub fn stop(mut self) -> HashMap<isize, Track> {
        self.stop.store(true, Ordering::SeqCst);
        self.join.take().map(|j| j.join().unwrap_or_default()).unwrap_or_default()
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Virtual desktops
// ---------------------------------------------------------------------------

/// `IVirtualDesktopManager` for the calling (COM-initialized) thread.
pub fn desktop_manager() -> Result<IVirtualDesktopManager, String> {
    // SAFETY: valid CLSID.
    unsafe { CoCreateInstance(&VirtualDesktopManager, None, CLSCTX_ALL) }
        .map_err(|e| format!("VirtualDesktopManager: {e}"))
}

pub fn fmt_guid(g: &GUID) -> String {
    format!("{:08x}-…-{:012x}", g.data1, u64::from_be_bytes(g.data4) & 0xffff_ffff_ffff)
}

// ---------------------------------------------------------------------------
// Fold mechanics (F2): new tab, navigation, selection, UIA, foreground
// ---------------------------------------------------------------------------

/// The undocumented "new tab" command (R2).
pub const CMD_NEW_TAB: usize = 0xA21B;

pub fn post_new_tab(top: isize) -> Result<(), String> {
    use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_COMMAND};
    // SAFETY: posting a plain message to a window handle.
    unsafe { PostMessageW(Some(hwnd(top)), WM_COMMAND, WPARAM(CMD_NEW_TAB), LPARAM(0)) }
        .map_err(|e| e.to_string())
}

pub fn post_sc_close(top: isize) {
    use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, SC_CLOSE, WM_SYSCOMMAND};
    // SAFETY: posting a plain message to a window handle.
    unsafe {
        let _ = PostMessageW(Some(hwnd(top)), WM_SYSCOMMAND, WPARAM(SC_CLOSE as usize), LPARAM(0));
    }
}

/// `Navigate2(pidl, navNoHistory)`; the PIDL SAFEARRAY is not destroyed
/// (R2: destroying it corrupted the heap).
pub fn navigate(browser: &IWebBrowser2, pidl: &[u8]) -> Result<(), String> {
    use std::mem::ManuallyDrop;
    use windows::Win32::System::Ole::{
        SafeArrayAccessData, SafeArrayCreateVector, SafeArrayUnaccessData,
    };
    use windows::Win32::System::Variant::{
        VARENUM, VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_ARRAY, VT_I4, VT_UI1,
    };
    let mk = |vt, a: VARIANT_0_0_0| VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: a,
            }),
        },
    };
    // SAFETY: the SAFEARRAY is created and filled here and handed to Explorer.
    unsafe {
        let psa = SafeArrayCreateVector(VT_UI1, 0, pidl.len() as u32);
        if psa.is_null() {
            return Err("SafeArrayCreateVector failed".into());
        }
        let mut data: *mut core::ffi::c_void = std::ptr::null_mut();
        SafeArrayAccessData(psa, &mut data).map_err(|e| e.to_string())?;
        std::ptr::copy_nonoverlapping(pidl.as_ptr(), data.cast::<u8>(), pidl.len());
        SafeArrayUnaccessData(psa).map_err(|e| e.to_string())?;
        let url = mk(VARENUM(VT_ARRAY.0 | VT_UI1.0), VARIANT_0_0_0 { parray: psa });
        let flags = mk(VT_I4, VARIANT_0_0_0 { lVal: 0x2 });
        let empty = VARIANT::default();
        browser
            .Navigate2(&url, Some(&flags), Some(&empty), Some(&empty), Some(&empty))
            .map_err(|e| format!("Navigate2: {e}"))
    }
}

/// Selects exactly `items` (absolute PIDLs) in the tab's view, as
/// `ShellNavigator::select_items` does.
pub fn select_items(browser: &IWebBrowser2, items: &[Vec<u8>]) -> Result<(), String> {
    use esr_win::navigator::select_flags;
    use esr_win::shell_util::shell_browser;
    let sb = shell_browser(browser).map_err(|e| e.to_string())?;
    // SAFETY: plain COM calls on live interfaces; each child ID points into its item.
    unsafe {
        let view = sb.QueryActiveShellView().map_err(|e| e.to_string())?;
        for (i, item) in items.iter().enumerate() {
            let child = ILFindLastID(item.as_ptr().cast::<ITEMIDLIST>());
            view.SelectItem(child, select_flags(i)).map_err(|e| format!("SelectItem: {e}"))?;
        }
    }
    Ok(())
}

pub fn selection(browser: &IWebBrowser2) -> Vec<Vec<u8>> {
    esr_win::shell_windows_source::selection_of(browser).into_iter().map(|i| i.0).collect()
}

/// UIA access to a window's tab strip through the product's own code.
///
/// Names, the selected index and selection go through
/// [`esr_win::tab_strip::TabStripReader`] (its own MTA workers; the strip
/// search of `esr_win::uia_strip::find_strip` with its fallback for builds
/// whose strip is not at the 22621 place, such as 26300). [`Uia::items`], for
/// the bounding rectangles of the F3 drag, finds the strip with the same
/// `find_strip` and lists the same `ListViewItem` tab items. Positions are
/// those of the items the strip exposes, as in the product's reads. Names are
/// display names on 22621 and parsing names on 26200/26300.
pub struct Uia {
    pub automation: windows::Win32::UI::Accessibility::IUIAutomation,
    reader: esr_win::tab_strip::TabStripReader,
    keys: esr_win::keys::SharedKeys<esr_core::engine::WinKey>,
    walker: windows::Win32::UI::Accessibility::IUIAutomationTreeWalker,
    tab_items: windows::Win32::UI::Accessibility::IUIAutomationCondition,
    strip_item: windows::Win32::UI::Accessibility::IUIAutomationCondition,
    strips: std::cell::RefCell<HashMap<isize, windows::Win32::UI::Accessibility::IUIAutomationElement>>,
}

impl Uia {
    pub fn new() -> Result<Self, String> {
        use esr_win::tab_strip::{ChangeCallback, TAB_ITEM_CLASS, TabStripConfig, TabStripReader};
        use windows::Win32::System::Com::CLSCTX_INPROC_SERVER;
        use windows::Win32::UI::Accessibility::{
            CUIAutomation, UIA_ClassNamePropertyId, UIA_ControlTypePropertyId, UIA_TabItemControlTypeId,
        };
        use windows::Win32::System::Variant::VARIANT;
        use windows::core::BSTR;
        let e = |e: windows::core::Error| e.to_string();
        // SAFETY: valid CLSID; in-process server; plain UIA calls on live interfaces.
        let (automation, walker, tab_items, strip_item) = unsafe {
            let automation: windows::Win32::UI::Accessibility::IUIAutomation =
                CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).map_err(e)?;
            let tab_items = automation
                .CreatePropertyCondition(
                    UIA_ControlTypePropertyId,
                    &VARIANT::from(UIA_TabItemControlTypeId.0),
                )
                .map_err(e)?;
            let class = automation
                .CreatePropertyCondition(UIA_ClassNamePropertyId, &VARIANT::from(BSTR::from(TAB_ITEM_CLASS)))
                .map_err(e)?;
            let strip_item = automation.CreateAndCondition(&tab_items, &class).map_err(e)?;
            let walker = automation.RawViewWalker().map_err(e)?;
            (automation, walker, tab_items, strip_item)
        };
        // The first read of a window searches the strip (up to ~2.4 s while
        // Explorer builds it): longer timeouts than the product's 500 ms.
        let config = TabStripConfig {
            read_timeout: Duration::from_secs(4),
            select_timeout: Duration::from_secs(4),
        };
        let on_change: ChangeCallback = Arc::new(|_| {});
        let reader = TabStripReader::with_config(config, on_change)
            .map_err(|err| format!("TabStripReader: {err:?}"))?;
        Ok(Self {
            automation,
            reader,
            keys: esr_win::keys::SharedKeys::new(),
            walker,
            tab_items,
            strip_item,
            strips: std::cell::RefCell::new(HashMap::new()),
        })
    }

    fn key(&self, top: isize) -> esr_core::engine::WinKey {
        self.keys.key_of(top as usize as u64)
    }

    /// The tab strip element of `top` (cached; searched with the product's
    /// `find_strip`).
    fn strip(&self, top: isize, fresh: bool) -> Option<windows::Win32::UI::Accessibility::IUIAutomationElement> {
        let cached = if fresh { None } else { self.strips.borrow().get(&top).cloned() };
        if cached.is_some() {
            return cached;
        }
        let strip =
            esr_win::uia_strip::find_strip(&self.automation, &self.walker, &self.strip_item, hwnd(top)).ok()?;
        self.strips.borrow_mut().insert(top, strip.clone());
        Some(strip)
    }

    fn children(&self, top: isize, fresh: bool) -> Vec<windows::Win32::UI::Accessibility::IUIAutomationElement> {
        use windows::Win32::UI::Accessibility::TreeScope_Children;
        let Some(strip) = self.strip(top, fresh) else { return Vec::new() };
        // SAFETY: plain UIA calls on live interfaces.
        unsafe {
            let Ok(all) = strip.FindAll(TreeScope_Children, &self.tab_items) else { return Vec::new() };
            (0..all.Length().unwrap_or(0))
                .filter_map(|i| all.GetElement(i).ok())
                .filter(|el| {
                    el.CurrentClassName()
                        .is_ok_and(|c| esr_win::tab_strip::is_tab_item_class(&c.to_string()))
                })
                .collect()
        }
    }

    /// The exposed tab items (`ListViewItem`) left to right; empty when the
    /// strip cannot be found yet.
    pub fn items(&self, top: isize) -> Vec<windows::Win32::UI::Accessibility::IUIAutomationElement> {
        let items = self.children(top, false);
        if items.is_empty() {
            // A cached strip can be stale (window rebuilt): search again.
            self.strips.borrow_mut().remove(&top);
            return self.children(top, true);
        }
        items
    }

    pub fn names(&self, top: isize) -> Vec<String> {
        self.reader
            .read(self.key(top))
            .map(|s| s.names.iter().map(|n| String::from_utf16_lossy(n.units())).collect())
            .unwrap_or_default()
    }

    pub fn selected(&self, top: isize) -> Option<usize> {
        self.reader.read(self.key(top)).ok().and_then(|s| s.selected)
    }

    pub fn select(&self, top: isize, index: usize) -> Result<(), String> {
        self.reader.select(self.key(top), index).map_err(|e| e.message)
    }
}

/// The product's foreground technique (`ShellNavigator::focus`): zero-effect
/// `SendInput`, then `SetForegroundWindow`. Returns whether `top` is the
/// foreground window afterwards.
pub fn bring_to_foreground(top: isize) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{INPUT, INPUT_MOUSE, SendInput};
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, SetForegroundWindow};
    let input = [INPUT { r#type: INPUT_MOUSE, ..Default::default() }];
    // SAFETY: one zero mouse INPUT; plain calls.
    unsafe {
        SendInput(&input, size_of::<INPUT>() as i32);
        let _ = SetForegroundWindow(hwnd(top));
        raw(GetForegroundWindow()) == top
    }
}

pub fn foreground() -> isize {
    // SAFETY: plain call.
    raw(unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() })
}

/// `ShellTabWindowClass` children of `top` (HWND values).
pub fn tab_children(top: isize) -> Vec<isize> {
    esr_win::navigator::tab_children(hwnd(top)).into_iter().map(|v| v as isize).collect()
}

/// The browser of tab window `tab` of `top`, once it registered.
pub fn browser_of_tab(top: isize, tab: isize) -> Option<IWebBrowser2> {
    browsers_of(top).into_iter().find(|(t, _)| *t == Some(tab)).map(|(_, b)| b)
}
