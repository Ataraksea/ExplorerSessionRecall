//! Spike F4: virtual desktops for Tab_Folding (Req 14.2, 14.9–14.12).
//!
//! ```text
//! cargo test -p esr-win --test spike_f4_desktops -- --ignored --nocapture --test-threads=1
//! ```
//!
//! 1. Cost and result of `IVirtualDesktopManager::GetWindowDesktopId` and
//!    `IsWindowOnCurrentVirtualDesktop` for an Explorer window (another
//!    process), also for a candidate at its first signal (alpha 0, F1) and
//!    for a window hidden with `SW_HIDE`; `MoveWindowToDesktop` on Explorer's
//!    window (documented to work only for the caller's own windows).
//! 2. A second desktop is created with Win+Ctrl+D (`SendInput`); a candidate
//!    opened there must report the new desktop at its first signal, and the
//!    primary must report desktop 1 and not be on the current desktop. The
//!    candidate is closed and the desktop closed again with Win+Ctrl+F4.
//!
//! Pinned windows ("Show this window on all desktops") need the undocumented
//! `IVirtualDesktopPinnedApps`; they are not tested (see design.md, F4).

#![cfg(windows)]

mod common;

use std::collections::HashSet;
use std::time::{Duration, Instant};

use common::explorer_guard::{ExplorerGuard, pump_for, top_level_cabinets};
use common::fold_spike::{
    Hide, HookThread, desktop_manager, fmt_guid, hwnd, ms, new_cabinets, open_primary,
    pidl_of_path, shell_pid, spawn_explorer, stats, wait_until, window_shows,
};
use esr_win::shell_util::StaGuard;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
    VIRTUAL_KEY, VK_CONTROL, VK_D, VK_F4, VK_LWIN,
};
use windows::Win32::UI::Shell::IVirtualDesktopManager;
use windows::Win32::UI::WindowsAndMessaging::{SW_HIDE, SW_SHOWNOACTIVATE, ShowWindow};
use windows::core::GUID;

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

/// Win+Ctrl+<vk>, keys always released.
fn win_ctrl(vk: VIRTUAL_KEY) {
    key(VK_LWIN, false);
    key(VK_CONTROL, false);
    key(vk, false);
    std::thread::sleep(Duration::from_millis(40));
    key(vk, true);
    key(VK_CONTROL, true);
    key(VK_LWIN, true);
}

fn desktop_of(vdm: &IVirtualDesktopManager, h: isize) -> Result<GUID, String> {
    // SAFETY: plain COM call.
    unsafe { vdm.GetWindowDesktopId(hwnd(h)) }.map_err(|e| format!("{:?}", e.code()))
}

fn on_current(vdm: &IVirtualDesktopManager, h: isize) -> Result<bool, String> {
    // SAFETY: plain COM call.
    unsafe { vdm.IsWindowOnCurrentVirtualDesktop(hwnd(h)) }
        .map(|b| b.as_bool())
        .map_err(|e| format!("{:?}", e.code()))
}

/// Opens a candidate on `dir` with the F1 hook (alpha 0 at first signal);
/// returns (window, desktop id at first signal, cost of that call in ms).
fn candidate(guard: &ExplorerGuard, dir: &std::path::Path) -> Result<(isize, Option<GUID>, f64), String> {
    let pre: HashSet<isize> = top_level_cabinets().into_iter().collect();
    let hook = HookThread::start(Hide::AlphaAtCreate, pre.clone());
    std::thread::sleep(Duration::from_millis(50));
    spawn_explorer(&format!("\"{}\"", dir.display()))?;
    let target = pidl_of_path(dir)?;
    let mut w = None;
    wait_until(Duration::from_secs(8), || {
        w = new_cabinets(&pre).into_iter().find(|h| window_shows(*h, &target));
        w.is_some()
    });
    let (created, _) = hook.stop();
    let w = w.ok_or("candidate did not show its folder")?;
    guard.adopt(hwnd(w));
    let c = created.iter().find(|c| c.hwnd == w).ok_or("candidate not seen by the hook")?;
    Ok((w, c.desktop_at_create, c.desktop_ms))
}

#[test]
#[ignore = "spike: opens windows, creates and closes a virtual desktop (Win+Ctrl+D / Win+Ctrl+F4)"]
fn spike_f4_virtual_desktops() {
    let _sta = StaGuard::init().expect("STA");
    let guard = ExplorerGuard::with_stable_root("f4", "f4_desktops");
    let vdm = desktop_manager().expect("IVirtualDesktopManager");
    let shell_before = shell_pid();
    let primary = open_primary(&guard, &guard.dir("primary")).expect("primary");

    // 1. Cost and results on desktop 1.
    let d1 = desktop_of(&vdm, primary).expect("desktop of the primary");
    let mut cost_id = Vec::new();
    let mut cost_cur = Vec::new();
    for _ in 0..50 {
        let t = Instant::now();
        let _ = desktop_of(&vdm, primary);
        cost_id.push(ms(t.elapsed()));
        let t = Instant::now();
        let _ = on_current(&vdm, primary);
        cost_cur.push(ms(t.elapsed()));
    }
    println!("[f4] primary desktop {} (on current: {:?})", fmt_guid(&d1), on_current(&vdm, primary));
    println!("[f4] GetWindowDesktopId (cross-process): {}", stats(&cost_id));
    println!("[f4] IsWindowOnCurrentVirtualDesktop: {}", stats(&cost_cur));
    // SAFETY: plain COM call on another process's window.
    let mv = unsafe { vdm.MoveWindowToDesktop(hwnd(primary), &d1) };
    println!("[f4] MoveWindowToDesktop(Explorer window, same desktop): {:?}", mv.map_err(|e| e.code()));

    match candidate(&guard, &guard.dir("cand1")) {
        Ok((w, id, cost)) => {
            println!(
                "[f4] candidate on desktop 1: id at first signal {} ({cost:.2} ms), equals primary's: {}; now {:?}",
                id.as_ref().map_or("ERROR".into(), fmt_guid),
                id == Some(d1),
                desktop_of(&vdm, w).map(|g| fmt_guid(&g))
            );
            // SAFETY: plain calls on the test's own candidate window.
            unsafe {
                let _ = ShowWindow(hwnd(w), SW_HIDE);
            }
            pump_for(Duration::from_millis(300));
            println!(
                "[f4] SW_HIDE window: GetWindowDesktopId {:?}, on current {:?}",
                desktop_of(&vdm, w).map(|g| fmt_guid(&g)),
                on_current(&vdm, w)
            );
            // SAFETY: as above.
            unsafe {
                let _ = ShowWindow(hwnd(w), SW_SHOWNOACTIVATE);
            }
            guard.close(hwnd(w));
        }
        Err(e) => println!("[f4] candidate on desktop 1: FAIL {e}"),
    }

    // 2. A second desktop.
    win_ctrl(VK_D);
    pump_for(Duration::from_millis(1500));
    let switched = on_current(&vdm, primary);
    println!("[f4] after Win+Ctrl+D: primary on current desktop: {switched:?}");
    if switched == Ok(false) {
        match candidate(&guard, &guard.dir("cand2")) {
            Ok((w, id, cost)) => {
                let now = desktop_of(&vdm, w);
                println!(
                    "[f4] candidate on desktop 2: id at first signal {} ({cost:.2} ms), differs from desktop 1: {}; now {:?}; on current {:?}; primary still {:?}",
                    id.as_ref().map_or("ERROR".into(), fmt_guid),
                    id.is_some_and(|g| g != d1),
                    now.as_ref().map(fmt_guid),
                    on_current(&vdm, w),
                    desktop_of(&vdm, primary).map(|g| fmt_guid(&g)),
                );
                guard.close(hwnd(w));
            }
            Err(e) => println!("[f4] candidate on desktop 2: FAIL {e}"),
        }
        win_ctrl(VK_F4);
        pump_for(Duration::from_millis(1500));
        println!("[f4] after Win+Ctrl+F4: primary on current desktop: {:?}", on_current(&vdm, primary));
    } else {
        println!("[f4] second desktop not created (or not switched to); skipped");
    }
    let shell_after = shell_pid();
    println!(
        "[f4] shell pid before {shell_before}, after {shell_after}{}",
        if shell_before == shell_after { " (no Explorer restart)" } else { " (EXPLORER RESTARTED)" }
    );
    drop(vdm);
    guard.finish().expect("cleanup");
}
