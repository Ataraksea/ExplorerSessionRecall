//! Spike F3: classification signals for Folding_Candidates (Req 14.2–14.5).
//!
//! ```text
//! cargo test -p esr-win --test spike_f3_signals -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Every case runs the F1 [`HookThread`] (no hiding) and reports what the
//! product could see at a new window's *first signal* (the first WinEvent of
//! the window or of one of its child windows, see F1):
//!
//! 1. **Drag-out** (Req 14.3 b): a tab of a two-tab window is dragged with
//!    `SendInput` onto a plain pad window (no drop target, as in R5). Reported:
//!    the order and timing of the source tab's `ShellTabWindowClass`
//!    `DESTROY`, the new window's first signal, its own `CREATE`, the new
//!    tab child's `CREATE`, and the mouse-button / Location state at the
//!    first signal.
//! 2. **Tab context menu** (Req 14.3 b, 14.5): right-click a tab, list the
//!    menu items through UIA ("Move tab to new window"? "Duplicate tab"?),
//!    then Escape.
//! 3. **Ctrl+Shift** (Req 14.4): keys pressed with `SendInput` before
//!    `ShellExecute("open")`, (a) held until the window exists, (b) released
//!    50 ms after the call; `GetAsyncKeyState` at the first signal.
//! 4. **Ctrl+N** vs `ShellExecute`: Ctrl+N sent to the foreground primary;
//!    reported: process, initial Location, tab count, key state at the first
//!    signal.
//!
//! Selection reads ("Show in folder") are measured in F2. Only windows the
//! test creates are touched; keys and the mouse button are always released.

#![cfg(windows)]

mod common;

use std::collections::HashSet;
use std::time::{Duration, Instant};

use common::explorer_guard::{ExplorerGuard, pump_for, top_level_cabinets};
use common::fold_spike::{
    Hide, HookThread, Uia, bring_to_foreground, browser_of_tab, browsers_of, hwnd, ms_between,
    navigate, new_cabinets, open_primary, pidl_of_path, post_new_tab, shell_execute, shell_pid,
    tab_children, wait_until, window_shows,
};
use esr_win::shell_util::{StaGuard, current_folder_pidl, window_process_id};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{GRAY_BRUSH, GetStockObject, HBRUSH, UpdateWindow};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Accessibility::{
    TreeScope_Descendants, UIA_ControlTypePropertyId, UIA_MenuItemControlTypeId,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP,
    MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
    MOUSEINPUT, SendInput, VIRTUAL_KEY, VK_CONTROL, VK_ESCAPE, VK_N, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetCursorPos, GetSystemMetrics, RegisterClassW,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SWP_NOACTIVATE,
    SWP_NOZORDER, SetCursorPos, SetWindowPos, UnregisterClassW, WNDCLASSW, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};
use windows::core::w;

// ---------------------------------------------------------------------------
// Input with guaranteed release
// ---------------------------------------------------------------------------

struct Input {
    saved: POINT,
    keys_down: Vec<VIRTUAL_KEY>,
    left_down: bool,
}

impl Input {
    fn new() -> Self {
        let mut saved = POINT::default();
        // SAFETY: valid out pointer.
        let _ = unsafe { GetCursorPos(&mut saved) };
        Self { saved, keys_down: Vec::new(), left_down: false }
    }

    fn key(vk: VIRTUAL_KEY, up: bool) {
        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        // SAFETY: one fully initialized INPUT.
        unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
    }

    fn press(&mut self, vk: VIRTUAL_KEY) {
        Self::key(vk, false);
        self.keys_down.push(vk);
    }

    fn release_all(&mut self) {
        while let Some(vk) = self.keys_down.pop() {
            Self::key(vk, true);
        }
    }

    fn mouse(flags: MOUSE_EVENT_FLAGS, p: POINT) {
        // SAFETY: plain metric reads.
        let (vx, vy, vw, vh) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN) as i64,
                GetSystemMetrics(SM_YVIRTUALSCREEN) as i64,
                GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2) as i64,
                GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2) as i64,
            )
        };
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: ((p.x as i64 - vx) * 65535 / (vw - 1)) as i32,
                    dy: ((p.y as i64 - vy) * 65535 / (vh - 1)) as i32,
                    mouseData: 0,
                    dwFlags: flags | MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        // SAFETY: one fully initialized INPUT.
        unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
    }

    fn drag(&mut self, from: POINT, to: POINT) {
        Self::mouse(MOUSE_EVENT_FLAGS(0), from);
        pump_for(Duration::from_millis(150));
        self.left_down = true;
        Self::mouse(MOUSEEVENTF_LEFTDOWN, from);
        pump_for(Duration::from_millis(150));
        for i in 1..=5 {
            Self::mouse(MOUSE_EVENT_FLAGS(0), POINT { x: from.x, y: from.y + 2 * i });
            pump_for(Duration::from_millis(25));
        }
        let start = POINT { x: from.x, y: from.y + 10 };
        for i in 1..=30 {
            Self::mouse(
                MOUSE_EVENT_FLAGS(0),
                POINT { x: start.x + (to.x - start.x) * i / 30, y: start.y + (to.y - start.y) * i / 30 },
            );
            pump_for(Duration::from_millis(15));
        }
        Self::mouse(MOUSE_EVENT_FLAGS(0), POINT { x: to.x + 3, y: to.y });
        pump_for(Duration::from_millis(150));
        Self::mouse(MOUSE_EVENT_FLAGS(0), to);
        pump_for(Duration::from_millis(400));
        Self::mouse(MOUSEEVENTF_LEFTUP, to);
        self.left_down = false;
    }

    fn right_click(&self, p: POINT) {
        Self::mouse(MOUSE_EVENT_FLAGS(0), p);
        pump_for(Duration::from_millis(100));
        Self::mouse(MOUSEEVENTF_RIGHTDOWN, p);
        pump_for(Duration::from_millis(50));
        Self::mouse(MOUSEEVENTF_RIGHTUP, p);
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        self.release_all();
        if self.left_down {
            let mut p = POINT::default();
            // SAFETY: valid out pointer.
            let _ = unsafe { GetCursorPos(&mut p) };
            Self::mouse(MOUSEEVENTF_LEFTUP, p);
        }
        // SAFETY: plain call.
        let _ = unsafe { SetCursorPos(self.saved.x, self.saved.y) };
    }
}

// ---------------------------------------------------------------------------
// Drop pad (as in R5)
// ---------------------------------------------------------------------------

unsafe extern "system" fn pad_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: forwarding the original arguments.
    unsafe { DefWindowProcW(h, msg, wp, lp) }
}

struct Pad(Option<HWND>, bool);

impl Pad {
    fn create(r: RECT) -> Result<Self, String> {
        // SAFETY: standard class registration and window creation on this thread.
        unsafe {
            let hinst = GetModuleHandleW(None).map_err(|e| e.to_string())?;
            let wc = WNDCLASSW {
                lpfnWndProc: Some(pad_proc),
                hInstance: hinst.into(),
                hbrBackground: HBRUSH(GetStockObject(GRAY_BRUSH).0),
                lpszClassName: w!("EsrSpikeF3Pad"),
                ..Default::default()
            };
            let registered = RegisterClassW(&wc) != 0;
            let h = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
                w!("EsrSpikeF3Pad"),
                w!("ESR spike F3 drop pad"),
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
            Ok(Pad(Some(h), registered))
        }
    }
}

impl Drop for Pad {
    fn drop(&mut self) {
        // SAFETY: window and class created by this test.
        unsafe {
            if let Some(h) = self.0.take() {
                let _ = DestroyWindow(h);
            }
            if self.1 {
                let hinst = GetModuleHandleW(None).unwrap_or_default();
                let _ = UnregisterClassW(w!("EsrSpikeF3Pad"), Some(hinst.into()));
            }
        }
    }
}

fn tab_rects(uia: &Uia, top: isize) -> Vec<RECT> {
    uia.items(top)
        .iter()
        // SAFETY: plain UIA property read.
        .map(|i| unsafe { i.CurrentBoundingRectangle() }.unwrap_or_default())
        .collect()
}

/// Adds a tab to `top` showing `dir`.
fn add_tab(top: isize, dir: &std::path::Path) -> Result<(), String> {
    let pidl = pidl_of_path(dir)?;
    let base: HashSet<isize> = tab_children(top).into_iter().collect();
    post_new_tab(top)?;
    let mut nt = None;
    wait_until(Duration::from_secs(4), || {
        nt = tab_children(top).into_iter().find(|c| !base.contains(c));
        nt.is_some_and(|t| browser_of_tab(top, t).is_some())
    })
    .ok_or("new tab did not register")?;
    let b = browser_of_tab(top, nt.unwrap()).unwrap();
    navigate(&b, &pidl)?;
    wait_until(Duration::from_secs(4), || window_shows(top, &pidl)).ok_or("tab did not navigate")?;
    pump_for(Duration::from_millis(500));
    Ok(())
}

fn case_drag_out(guard: &ExplorerGuard, uia: &Uia, input: &mut Input) -> Result<(), String> {
    let src = open_primary(guard, &guard.dir("drag_a"))?;
    // Place the source window and the pad side by side.
    // SAFETY: plain call on the test's own window.
    unsafe {
        let _ = SetWindowPos(hwnd(src), None, 40, 40, 900, 600, SWP_NOZORDER | SWP_NOACTIVATE);
    }
    add_tab(src, &guard.dir("drag_b"))?;
    let rects = tab_rects(uia, src);
    let Some(r) = rects.get(1).copied() else {
        return Err(format!("expected 2 UIA tab items, got {}", rects.len()));
    };
    let _pad = Pad::create(RECT { left: 1000, top: 100, right: 1500, bottom: 600 })?;
    pump_for(Duration::from_millis(300));
    let src_tabs: HashSet<isize> = tab_children(src).into_iter().collect();
    let pre: HashSet<isize> = top_level_cabinets().into_iter().collect();
    let hook = HookThread::start(Hide::None, pre.clone());
    hook.watch_tabs(src_tabs.iter().copied());
    std::thread::sleep(Duration::from_millis(50));
    let from = POINT { x: r.left + ((r.right - r.left) / 3).min(40), y: (r.top + r.bottom) / 2 };
    let t0 = Instant::now();
    input.drag(from, POINT { x: 1250, y: 350 });
    let t_drop = Instant::now();
    let mut new_win = None;
    wait_until(Duration::from_secs(4), || {
        new_win = new_cabinets(&pre).into_iter().next();
        new_win.is_some_and(|w| !browsers_of(w).is_empty())
    });
    pump_for(Duration::from_millis(400));
    let (created, events) = hook.stop();
    let Some(w) = new_win else {
        return Err("no drag-out window appeared".into());
    };
    guard.adopt(hwnd(w));
    let rel = |t: Instant| ms_between(t_drop, t);
    println!("[f3] drag-out: drag started {:.0} ms before the drop (button up)", ms_between(t0, t_drop));
    for e in &events {
        let src_tab = src_tabs.contains(&e.hwnd);
        if e.kind == "tab" || e.hwnd == w {
            println!(
                "[f3]   {:+8.1} ms {:<7} {:<7} {:#x}{}{}",
                rel(e.t),
                e.name(),
                e.kind,
                e.hwnd,
                if src_tab { " (source window's dragged tab)" } else { "" },
                if e.kind == "tab" && e.root == w { " (in the new window)" } else { "" }
            );
        }
    }
    if let Some(c) = created.iter().find(|c| c.hwnd == w) {
        println!(
            "[f3]   new window first signal {:+.1} ms ({}); own CREATE {:+.1} ms; at first signal: lbutton={} ctrl={} shift={}",
            rel(c.t_create),
            c.first_signal,
            c.t_own_create.map_or(f64::NAN, rel),
            c.lbutton,
            c.ctrl,
            c.shift
        );
    }
    let loc = browsers_of(w).first().and_then(|(_, b)| current_folder_pidl(b).ok());
    let b_pidl = pidl_of_path(&guard.dir("drag_b"))?;
    println!(
        "[f3]   new window shows the dragged tab's folder: {}; source tabs now {} (was 2); pids src {} new {}",
        loc.is_some_and(|l| common::fold_spike::same_pidl(&l, &b_pidl)),
        tab_children(src).len(),
        window_process_id(hwnd(src)),
        window_process_id(hwnd(w))
    );
    guard.close(hwnd(w));
    guard.close(hwnd(src));
    Ok(())
}

fn case_context_menu(guard: &ExplorerGuard, uia: &Uia, input: &mut Input) -> Result<(), String> {
    let top = open_primary(guard, &guard.dir("menu_a"))?;
    let rects = tab_rects(uia, top);
    let r = *rects.first().ok_or("no UIA tab item")?;
    let pre: HashSet<isize> = top_level_cabinets().into_iter().collect();
    let hook = HookThread::start(Hide::None, pre.clone());
    let mut duplicate = None;
    bring_to_foreground(top);
    input.right_click(POINT { x: r.left + ((r.right - r.left) / 3).min(40), y: (r.top + r.bottom) / 2 });
    pump_for(Duration::from_millis(800));
    // SAFETY: plain UIA calls.
    let names = unsafe {
        let root = uia.automation.GetRootElement().map_err(|e| e.to_string())?;
        let v = windows::Win32::System::Variant::VARIANT::from(UIA_MenuItemControlTypeId.0);
        let cond = uia
            .automation
            .CreatePropertyCondition(UIA_ControlTypePropertyId, &v)
            .map_err(|e| e.to_string())?;
        let all = root.FindAll(TreeScope_Descendants, &cond).map_err(|e| e.to_string())?;
        let mut names = Vec::new();
        for i in 0..all.Length().unwrap_or(0) {
            if let Ok(el) = all.GetElement(i) {
                let pid = el.CurrentProcessId().unwrap_or(0) as u32;
                if pid == window_process_id(hwnd(top)) {
                    let name = el.CurrentName().map(|s| s.to_string()).unwrap_or_default();
                    if name == "Duplicate tab" {
                        duplicate = Some(el.clone());
                    }
                    names.push(name);
                }
            }
        }
        names
    };
    println!("[f3] tab context menu items (UIA MenuItem, Explorer process): {names:?}");
    if let Some(item) = duplicate {
        // Req 14.5: does "Duplicate tab" create a window (a first signal) or only a tab?
        let before = tab_children(top).len();
        let t0 = Instant::now();
        // SAFETY: plain UIA calls on a live element.
        let invoked = unsafe {
            item.GetCurrentPatternAs::<windows::Win32::UI::Accessibility::IUIAutomationInvokePattern>(
                windows::Win32::UI::Accessibility::UIA_InvokePatternId,
            )
            .and_then(|p| p.Invoke())
        };
        pump_for(Duration::from_millis(1500));
        let (created, _) = hook.stop();
        let windows_now = new_cabinets(&pre);
        println!(
            "[f3] \"Duplicate tab\" invoked ({invoked:?}): tabs in the window {before} -> {}; new top-level Explorer windows: {} (first signals seen: {}, first {:?} ms after the click); same process tabs: {}",
            tab_children(top).len(),
            windows_now.len(),
            created.len(),
            created.first().map(|c| ms_between(t0, c.t_create).round()),
            browsers_of(top).len()
        );
        for w in windows_now {
            guard.adopt(hwnd(w));
            guard.close(hwnd(w));
        }
    } else {
        Input::key(VK_ESCAPE, false);
        Input::key(VK_ESCAPE, true);
        pump_for(Duration::from_millis(300));
        let _ = hook.stop();
    }
    Input::key(VK_ESCAPE, false);
    Input::key(VK_ESCAPE, true);
    pump_for(Duration::from_millis(300));
    guard.close(hwnd(top));
    Ok(())
}

fn case_ctrl_shift(guard: &ExplorerGuard, input: &mut Input, release_after: Option<Duration>) -> Result<(), String> {
    let dir = guard.dir(if release_after.is_some() { "keys_quick" } else { "keys_held" });
    let pre: HashSet<isize> = top_level_cabinets().into_iter().collect();
    let hook = HookThread::start(Hide::None, pre.clone());
    std::thread::sleep(Duration::from_millis(50));
    input.press(VK_CONTROL);
    input.press(VK_SHIFT);
    pump_for(Duration::from_millis(30));
    let t0 = Instant::now();
    let launcher = shell_execute(&dir, Some("open"));
    if let Some(d) = release_after {
        std::thread::sleep(d);
        input.release_all();
    }
    let ok = wait_until(Duration::from_secs(5), || !hook.created().is_empty());
    let t_rel = Instant::now();
    input.release_all();
    let _ = launcher.join();
    let target = pidl_of_path(&dir)?;
    let mut win = None;
    wait_until(Duration::from_secs(5), || {
        win = new_cabinets(&pre).into_iter().find(|h| window_shows(*h, &target));
        win.is_some()
    });
    let (created, _) = hook.stop();
    if let Some(w) = win {
        guard.adopt(hwnd(w));
    }
    match (ok, created.first()) {
        (Some(_), Some(c)) => println!(
            "[f3] Ctrl+Shift {}: first signal {:.0} ms after ShellExecute ({}); at first signal ctrl={} shift={}; keys released {:.0} ms after ShellExecute",
            if release_after.is_some() { "released 50 ms after the request" } else { "held until the window exists" },
            ms_between(t0, c.t_create),
            c.first_signal,
            c.ctrl,
            c.shift,
            ms_between(t0, t_rel).min(release_after.map_or(f64::MAX, |d| d.as_secs_f64() * 1000.0)),
        ),
        _ => println!("[f3] Ctrl+Shift: no window seen"),
    }
    if let Some(w) = win {
        guard.close(hwnd(w));
    }
    Ok(())
}

fn case_ctrl_n(guard: &ExplorerGuard, input: &mut Input) -> Result<(), String> {
    let pdir = guard.dir("ctrln_primary");
    let primary = open_primary(guard, &pdir)?;
    let pre: HashSet<isize> = top_level_cabinets().into_iter().collect();
    let hook = HookThread::start(Hide::None, pre.clone());
    std::thread::sleep(Duration::from_millis(50));
    let fg = bring_to_foreground(primary);
    pump_for(Duration::from_millis(200));
    let t0 = Instant::now();
    input.press(VK_CONTROL);
    Input::key(VK_N, false);
    Input::key(VK_N, true);
    pump_for(Duration::from_millis(60));
    input.release_all();
    let mut win = None;
    wait_until(Duration::from_secs(5), || {
        win = new_cabinets(&pre).into_iter().next();
        win.is_some_and(|w| !browsers_of(w).is_empty())
    });
    pump_for(Duration::from_millis(600));
    let (created, _) = hook.stop();
    let Some(w) = win else {
        println!("[f3] Ctrl+N: no new window (primary foreground: {fg})");
        return Ok(());
    };
    guard.adopt(hwnd(w));
    let tabs = browsers_of(w);
    let loc = tabs.first().and_then(|(_, b)| current_folder_pidl(b).ok());
    let shows_same = loc.is_some_and(|l| common::fold_spike::same_pidl(&l, &pidl_of_path(&pdir).unwrap()));
    if let Some(c) = created.iter().find(|c| c.hwnd == w) {
        println!(
            "[f3] Ctrl+N: first signal {:.0} ms after the keys ({}); ctrl at first signal={} shift={}; tabs {}; same folder as the primary: {shows_same}; foreground at first signal is the primary: {} (foreground {:#x} = the new window: {}; previous foreground from the events {:#x} = the primary: {}; the new window's own foreground event already delivered: {}; primary {primary:#x}, primary foreground before the keys: {fg}); pid primary {} new {}",
            ms_between(t0, c.t_create),
            c.first_signal,
            c.ctrl,
            c.shift,
            tabs.len(),
            c.foreground == primary,
            c.foreground,
            c.foreground == w,
            c.fg_before,
            c.fg_before == primary,
            c.fg_event_seen,
            window_process_id(hwnd(primary)),
            window_process_id(hwnd(w)),
        );
    }
    // ShellExecute comparison.
    let pre2: HashSet<isize> = top_level_cabinets().into_iter().collect();
    let hook2 = HookThread::start(Hide::None, pre2.clone());
    std::thread::sleep(Duration::from_millis(50));
    let sdir = guard.dir("ctrln_shellexec");
    let t1 = Instant::now();
    let _ = shell_execute(&sdir, Some("open")).join();
    let target = pidl_of_path(&sdir)?;
    let mut win2 = None;
    wait_until(Duration::from_secs(5), || {
        win2 = new_cabinets(&pre2).into_iter().find(|h| window_shows(*h, &target));
        win2.is_some()
    });
    let (created2, _) = hook2.stop();
    if let Some(w2) = win2 {
        guard.adopt(hwnd(w2));
        if let Some(c) = created2.iter().find(|c| c.hwnd == w2) {
            println!(
                "[f3] ShellExecute open: first signal {:.0} ms after the call ({}); foreground at first signal is an Explorer window: {}; tabs {}; pid {}",
                ms_between(t1, c.t_create),
                c.first_signal,
                top_level_cabinets().contains(&c.foreground),
                browsers_of(w2).len(),
                window_process_id(hwnd(w2)),
            );
        }
        guard.close(hwnd(w2));
    }
    guard.close(hwnd(w));
    guard.close(hwnd(primary));
    Ok(())
}

#[test]
#[ignore = "spike: opens and closes File Explorer windows and injects mouse/keyboard input"]
fn spike_f3_classification_signals() {
    // SAFETY: plain call; failure (already set) is fine.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let _sta = StaGuard::init().expect("STA");
    let guard = ExplorerGuard::with_stable_root("f3", "f3_signals");
    let uia = Uia::new().expect("UIA");
    let mut input = Input::new();
    let shell_before = shell_pid();
    for (name, r) in [
        ("drag-out 1", case_drag_out(&guard, &uia, &mut input)),
        ("drag-out 2", case_drag_out(&guard, &uia, &mut input)),
        ("drag-out 3", case_drag_out(&guard, &uia, &mut input)),
        ("context menu", case_context_menu(&guard, &uia, &mut input)),
        ("ctrl+shift held", case_ctrl_shift(&guard, &mut input, None)),
        ("ctrl+shift quick", case_ctrl_shift(&guard, &mut input, Some(Duration::from_millis(50)))),
        ("ctrl+n", case_ctrl_n(&guard, &mut input)),
    ] {
        if let Err(e) = r {
            println!("[f3] {name}: FAIL {e}");
        }
    }
    drop(input);
    let shell_after = shell_pid();
    println!(
        "[f3] shell pid before {shell_before}, after {shell_after}{}",
        if shell_before == shell_after { " (no Explorer restart)" } else { " (EXPLORER RESTARTED)" }
    );
    guard.finish().expect("cleanup");
}
