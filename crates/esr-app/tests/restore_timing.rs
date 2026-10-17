//! Restore timing and visual cleanliness against real File Explorer
//! (task 16.3; Requirements 7.1, 7.2, 7.7, 7.8). All tests are `#[ignore]`
//! because they open File Explorer windows. Run alone:
//!
//! ```text
//! cargo test -p esr-app --test restore_timing -- --ignored --nocapture --test-threads=1
//! ```
//!
//! # Method
//!
//! Each test runs the real runtime in-process (as `runtime_smoke` does): a
//! temporary data directory holding a Pending Session_File whose tabs are
//! folders under the `ExplorerGuard` temp root, no UI thread, no panic guard.
//! The test then "opens File Explorer" by launching `explorer.exe` without
//! arguments through `ShellExecuteExW`, which opens the configured "Open File
//! Explorer to" location (Home or This PC), so the new window is a
//! Default_Launch Trigger_Window. Explorer opens it at the placement of the
//! last Explorer window closed (maximized state included), so each case
//! first opens and closes a window on a temp folder at a known placement
//! ([`TriggerStart`]: normal at a rect no case uses as a final placement, or
//! maximized), which makes the trigger's initial placement independent of
//! the test that ran before (Req 7.7).
//!
//! Diagnostics: `ESR_RT_TRACE=1` prints the runtime's in-process trace
//! (`esr_win::diag`: Engine commands and inbound items with their durations,
//! ShellWindows syncs, scans, window creation events), relative to the
//! trigger becoming visible. `ESR_RT_DISABLE=pid,global,uia,uiawatch`
//! switches runtime subsystems off for a measurement (per-PID WinEvent
//! hooks, global WinEvent hooks, the UIA tab strip reader, only its
//! `StructureChanged` subscriptions); `ESR_RT_WORKERS=0` turns the
//! navigator's browser workers off (on by default); `ESR_RT_LATE_SINK=1`
//! attaches each new tab's sink after its navigation instead of before it
//! (`RuntimeOptions::late_sink`, an opt-in, off by default). `ESR_RT_REPEAT=n` runs a
//! case n times in one process; `ESR_RT_SKIP_STRIP=1` skips the test's own
//! UIA check of the tab strips (to measure without it).
//!
//! * **Timing (Req 7.1, 7.8).** A sampler thread records when the trigger
//!   window first becomes visible (visible, not cloaked, alpha > 0). A log
//!   watcher thread records when the `RESTORE` entry appears in the temp
//!   `activity.log`. The asserted time is the difference (it slightly
//!   overestimates, by the log thread's latency and the 5 ms poll). The
//!   entry's own `duration_ms` (which starts later, once the trigger has been
//!   detected) must also be within the bound. ShellWindows is polled every
//!   200 ms as a cross-check (`ESR_RT_SHELL_POLL_MS` changes the interval;
//!   `0` checks only once afterwards): the time at which every expected
//!   Location is open in the expected windows is reported, as is the time
//!   each restored window was first shown and any `restore-…` `ERROR` entry
//!   (which stops the restoration, Req 4.10).
//! * **Completion (Req 7.1).** After the `RESTORE` entry, ShellWindows must
//!   show exactly the expected Locations per window, UI Automation the
//!   expected tab order and Active_Tab, and every window its final placement
//!   (normal windows at the saved restored bounds, maximized windows zoomed).
//! * **Visual cleanliness (Req 7.2, 7.3, 7.7).** The sampler polls every new
//!   top-level `CabinetWClass` window about once per millisecond:
//!   `IsWindowVisible`, `DWMWA_CLOAKED`, `WS_EX_LAYERED` +
//!   `GetLayeredWindowAttributes` alpha, then `GetWindowPlacement` (state)
//!   and `GetWindowRect`. A window counts as *shown* when it is visible, not
//!   cloaked, not minimized, and not layered with alpha 0 (a layered window
//!   without attributes is not drawn either). Visibility is read before the
//!   placement, so a sample taken during a reveal never pairs "shown" with a
//!   stale rect. Asserted: a restored window is shown only at its final
//!   placement; the trigger is shown at most at its initial placement and
//!   then at its final one (one step, Req 7.7); no other Explorer window is
//!   shown (Req 7.3). Every recorded state change is printed.
//!
//!   One exception is reported as a `VISUAL note` instead of a violation: a
//!   maximized trigger read as *normal at its maximized rect* for less than
//!   a frame (16 ms) right before it shows at its final placement (seen in
//!   2–3 of 5–10 maxtrigger runs, 4–5 ms). This is the inside of the one
//!   `SetWindowPlacement` Explorer's thread carries out: win32k's restore
//!   clears `WS_MAXIMIZE` first and then moves the window, and for a
//!   window in the normal state `GetWindowPlacement` reports the current
//!   window rect as `rcNormalPosition`, so the reads agree with each other
//!   (it is not a torn pair of reads; re-reading until state and restored
//!   size agree never flagged it). The window's position and size do not
//!   change during it and its thread cannot repaint in between, so no
//!   intermediate position or size can be composed; at most one frame
//!   could show the normal-state frame decoration at the maximized bounds.
//!   The product cannot avoid it: every un-maximize goes through that
//!   win32k sequence.
//!
//!   Real frame capture is **not** used. `Windows.Graphics.Capture` works (spike
//!   R1 used it with a magenta backdrop to show that a single hidden window is
//!   never composed), but here tab contents legitimately change while windows
//!   are visible, so frames cannot tell a placement change from a navigation
//!   without per-window segmentation that would itself be heuristic. Sampling
//!   window state at ~1 ms is stricter than the ~16 ms frame cadence: it can
//!   flag a state DWM never composed, never miss one it did (unless shorter
//!   than the sampling gap, which is reported).
//!
//! # Safety
//!
//! * Every test holds an `ExplorerGuard` (its named mutex serialises against
//!   other Explorer-opening tests) for its whole duration. Every new
//!   `CabinetWClass` window that appears during the test (the trigger and the
//!   restored windows) is adopted, and the guard closes them, plus anything
//!   still showing a folder under its temp root, even on failure.
//! * A restore needs the trigger to be the only Explorer window, so a test
//!   is **skipped** (with a printed reason) when any Explorer window is open
//!   before it starts; such windows are never touched. It is also skipped when
//!   another process (a real Explorer Session Restore instance) owns an
//!   `ESR.Engine` window, because that instance would react to the test's
//!   windows.
//! * The runtime uses a temp data directory (deleted afterwards), no UI, the
//!   default settings in memory (start-at-sign-in is only registered on a
//!   settings change, which never happens here), and is stopped with
//!   `ESR_EXIT` posted to this process's own Engine window.
//! * The offline share is `\\esr-offline-host-test\share\x`, a host that does
//!   not exist; its probe ends as `Timeout` (no response, or a
//!   network-unreachable error) and the tab is omitted. The omission
//!   notification may fail silently when the toast AUMID is not installed.
//! * Every case uses the same folders, `%TEMP%\esr_spike_restore_timing\w<n>_t<nn>`
//!   (31 in total with `prime`), in every run. Explorer 10.0.22621.3880
//!   crashes (`SHELL32.dll` + `0x1ab1fa`, `CSyncIntegrationManager::_CleanupCache`)
//!   once its sync-status cache holds 128 entries, one per distinct folder a
//!   view showed in the last 5 minutes: eviction dereferences a null entry.
//!   With fresh paths every run, the 5th back-to-back 3x10 run crashed it;
//!   navigating one plain Explorer window (visible, hidden, or an inactive
//!   tab) through 130 new folders does the same without this app.
//! * **Deferred tabs (Req 5.13).** `restore_130_folders_defers_the_tabs_beyond_the_budget`
//!   restores 2 × 65 tabs on 130 distinct folders of the same root (100 of
//!   them are opened; restart explorer.exe before other cases if they must
//!   not share its 5-minute sync-cache window): it checks the
//!   `TABS-DEFERRED` entry, that explorer.exe is still the same process, the
//!   placeholders and their captions, and that a deferred tab selected
//!   through UIA navigates to its saved folder.

#[path = "../../esr-win/tests/common/explorer_guard.rs"]
#[allow(dead_code)]
mod explorer_guard;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use esr_app::instance::{ENGINE_WINDOW_CLASS, InstanceMessages};
use esr_app::runtime::{RuntimeOptions, Storage, init_process, run, startup_input};
use esr_core::engine::{TabStripState, WinKey};
use esr_core::format::{FORMAT_VERSION, parse, serialize};
use esr_core::logfmt::entry::parse_line;
use esr_core::model::{
    DisplayRef, LocationId, LocationKind, Placement, Rect, SaveCause, SessionDoc, ShowState,
    Snapshot, Status, TabRec, Timestamp, Volume, WideString, WindowId, WindowRec, validate_doc,
};
use esr_core::settings::Settings;
use esr_win::diag::Switch;
use esr_win::display_info::displays;
use esr_win::keys::{WindowKeys, hwnd_value};
use esr_win::location::try_capture_from_parsing_name;
use esr_win::shell_util::{StaGuard, current_folder_pidl, enumerate_all_explorer_windows};
use esr_win::tab_strip::TabStripReader;
use explorer_guard::{ExplorerGuard, parsing_name, pump_once, top_level_cabinets};
use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::UI::Shell::{
    SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, GWL_EXSTYLE, GetLayeredWindowAttributes, GetShellWindow, GetWindowLongPtrW,
    GetWindowPlacement, GetWindowRect, GetWindowThreadProcessId, IsWindowVisible,
    LAYERED_WINDOW_ATTRIBUTES_FLAGS, LWA_ALPHA, PostMessageW, SW_SHOWMAXIMIZED, SW_SHOWMINIMIZED,
    SW_SHOWNORMAL, SetWindowPlacement, WINDOWPLACEMENT, WS_EX_LAYERED,
};
use windows::core::{HSTRING, PCWSTR, w};

/// A UNC share on a host that does not exist (the probe cannot reach it).
const OFFLINE_SHARE: &str = r"\\esr-offline-host-test\share\x";
/// Req 7.1: per restored window (the trigger included).
const WINDOW_MS: u64 = 300;
/// Req 7.1: per restored tab (rebased to 0.27 s in task 16.3 round 4d).
const TAB_MS: u64 = 270;
/// Req 9.3 / 7.8: the network response limit added to the Req 7.1 bound.
const NETWORK_EXTRA_MS: u64 = 5_000;
/// How long the test keeps observing after the `RESTORE` entry.
const SETTLE: Duration = Duration::from_millis(1_500);
/// ShellWindows cross-check interval during the restore (default).
const SHELL_POLL: Duration = Duration::from_millis(200);

/// `ESR_RT_SHELL_POLL_MS` overrides [`SHELL_POLL`]; `0` disables polling
/// during the restore (one check afterwards), to rule out that the
/// cross-process ShellWindows calls slow Explorer down.
fn shell_poll_interval() -> Option<Duration> {
    match std::env::var("ESR_RT_SHELL_POLL_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
        Some(0) => None,
        Some(ms) => Some(Duration::from_millis(ms)),
        None => Some(SHELL_POLL),
    }
}

/// `ESR_RT_LATE_SINK=1` turns the runtime's post-navigation sink attach on
/// (`RuntimeOptions::late_sink`, an opt-in, off by default).
fn late_sink_requested() -> bool {
    std::env::var("ESR_RT_LATE_SINK").is_ok_and(|v| v.trim() == "1")
}

/// `ESR_RT_DISABLE` (comma-separated: `pid`, `global`, `uia`, `uiawatch`)
/// switches runtime subsystems off for a measurement (`esr_win::diag`);
/// `false` switches them all on again. `ESR_RT_WORKERS=0` turns the
/// navigator's browser workers off (`esr_win::browser_worker`, on by
/// default). Returns the switches that were set.
fn set_switches(on: bool) -> Vec<Switch> {
    esr_win::browser_worker::set_enabled(!(on && std::env::var("ESR_RT_WORKERS").is_ok_and(|v| v == "0")));
    let all = [
        ("pid", Switch::PidHooks),
        ("global", Switch::GlobalHooks),
        ("uia", Switch::UiaReader),
        ("uiawatch", Switch::UiaWatch),
    ];
    let wanted = std::env::var("ESR_RT_DISABLE").unwrap_or_default();
    let mut set = Vec::new();
    for (name, switch) in all {
        let disable = on && wanted.split(',').any(|w| w.trim() == name);
        esr_win::diag::set_disabled(switch, disable);
        if disable {
            set.push(switch);
        }
    }
    set
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn normalize(p: &str) -> String {
    let p = p.strip_prefix(r"\\?\").unwrap_or(p);
    p.trim_end_matches('\\').to_lowercase()
}

fn hwnd_of(raw: isize) -> HWND {
    HWND(raw as *mut _)
}

/// Signed milliseconds from `from` to `to`.
fn ms(from: Instant, to: Instant) -> i64 {
    if to >= from {
        (to - from).as_millis() as i64
    } else {
        -((from - to).as_millis() as i64)
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
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `ESR.Engine` windows as (hwnd, owning pid).
fn engine_windows() -> Vec<(isize, u32)> {
    let class = HSTRING::from(ENGINE_WINDOW_CLASS);
    let mut out = Vec::new();
    let mut after: Option<HWND> = None;
    loop {
        // SAFETY: plain window enumeration by class.
        let Ok(hwnd) = (unsafe { FindWindowExW(None, after, &class, PCWSTR::null()) }) else {
            return out;
        };
        if hwnd.is_invalid() {
            return out;
        }
        let mut pid = 0;
        // SAFETY: valid out pointer.
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        out.push((hwnd.0 as isize, pid));
        after = Some(hwnd);
    }
}

fn own_engine_window() -> Option<isize> {
    engine_windows()
        .into_iter()
        .find(|&(_, pid)| pid == std::process::id())
        .map(|(h, _)| h)
}

/// Handle count and private bytes (MB) of process `pid`.
fn process_usage(pid: u32) -> Option<(u32, f64)> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
    use windows::Win32::System::Threading::{GetProcessHandleCount, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    // SAFETY: a process handle opened and closed here; valid out pointers.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut handles = 0u32;
        let ok = GetProcessHandleCount(h, &mut handles).is_ok();
        let mut mem = PROCESS_MEMORY_COUNTERS_EX {
            cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
            ..Default::default()
        };
        let mem_ok =
            K32GetProcessMemoryInfo(h, (&mut mem as *mut PROCESS_MEMORY_COUNTERS_EX).cast(), mem.cb).as_bool();
        let _ = CloseHandle(h);
        (ok && mem_ok).then(|| (handles, mem.PrivateUsage as f64 / (1024.0 * 1024.0)))
    }
}

/// The shell's (explorer.exe's) process id.
fn shell_pid() -> Option<u32> {
    // SAFETY: plain reads.
    let shell = unsafe { GetShellWindow() };
    if shell.is_invalid() {
        return None;
    }
    let mut pid = 0;
    // SAFETY: valid out pointer.
    unsafe { GetWindowThreadProcessId(shell, Some(&mut pid)) };
    (pid != 0).then_some(pid)
}

fn usage_text(u: Option<(u32, f64)>) -> String {
    u.map_or("?".into(), |(h, m)| format!("{h} handles, {m:.1} MB private"))
}

/// "The user opens File Explorer": `explorer.exe` without arguments, which
/// opens the "Open File Explorer to" location (Home or This PC) in a new
/// window when no Explorer window is open (spike R6).
fn launch_explorer() -> Result<(), String> {
    std::thread::spawn(|| {
        let _sta = StaGuard::init().map_err(|e| e.to_string())?;
        let mut sei = SHELLEXECUTEINFOW {
            cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
            lpFile: w!("explorer.exe"),
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };
        // SAFETY: `sei` is fully initialised; the strings are static.
        unsafe { ShellExecuteExW(&mut sei) }.map_err(|e| format!("ShellExecuteExW(explorer.exe): {e}"))
    })
    .join()
    .map_err(|_| "launcher thread panicked".to_string())?
}

/// The trigger's initial placement for a case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TriggerStart {
    /// Normal, at a rect that differs from the final one.
    Normal,
    Maximized,
}

/// Explorer opens a new window at the placement of the last Explorer window
/// closed, maximized state included. So that the trigger's initial
/// placement does not depend on the test that ran before, a window on a temp
/// folder is opened, given `start` (normal at a rect inside the primary work
/// area that no case uses as a final placement, or maximized), and closed
/// again, before the runtime starts.
fn prime_initial_placement(guard: &ExplorerGuard, start: TriggerStart) -> Result<String, String> {
    let pre: HashSet<isize> = top_level_cabinets().into_iter().collect();
    let dir = guard.dir("prime");
    let dir_text = dir.to_str().ok_or("non-UTF-8 temp path")?.to_string();
    std::thread::spawn(move || {
        let _sta = StaGuard::init().map_err(|e| e.to_string())?;
        let file = HSTRING::from(dir_text.as_str());
        let mut sei = SHELLEXECUTEINFOW {
            cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
            lpVerb: w!("opennewwindow"),
            lpFile: PCWSTR(file.as_ptr()),
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };
        // SAFETY: `sei` is fully initialised; `file` outlives the call.
        unsafe { ShellExecuteExW(&mut sei) }.map_err(|e| format!("ShellExecuteExW(prime): {e}"))
    })
    .join()
    .map_err(|_| "prime launcher thread panicked".to_string())??;
    let raw = wait(Duration::from_secs(10), || {
        top_level_cabinets()
            .into_iter()
            .find(|h| !pre.contains(h) && unsafe { IsWindowVisible(hwnd_of(*h)) }.as_bool())
    })
    .ok_or("the priming window did not open")?;
    guard.adopt(hwnd_of(raw));
    wait(Duration::from_millis(600), || None::<()>);
    let all = displays();
    let work = all.iter().find(|d| d.primary).or(all.first()).ok_or("no display")?.work;
    let (w, h) = ((work.width() * 3 / 5) as i32, (work.height() * 3 / 5) as i32);
    let (l, t) = (work.left + work.width() as i32 / 4, work.top + work.height() as i32 / 5);
    let wp = WINDOWPLACEMENT {
        length: size_of::<WINDOWPLACEMENT>() as u32,
        showCmd: match start {
            TriggerStart::Normal => SW_SHOWNORMAL.0 as u32,
            TriggerStart::Maximized => SW_SHOWMAXIMIZED.0 as u32,
        },
        // Workspace coordinates: the primary work area starts at its origin.
        rcNormalPosition: RECT { left: l - work.left, top: t - work.top, right: l - work.left + w, bottom: t - work.top + h },
        ..Default::default()
    };
    // SAFETY: a fully initialised WINDOWPLACEMENT for a window of the test.
    unsafe { SetWindowPlacement(hwnd_of(raw), &wp) }.map_err(|e| format!("SetWindowPlacement(prime): {e}"))?;
    wait(Duration::from_millis(600), || None::<()>);
    let applied = sample(raw, Instant::now()).key;
    if !guard.close(hwnd_of(raw)) {
        return Err("the priming window did not close".into());
    }
    wait(Duration::from_millis(500), || None::<()>);
    Ok(format!("{start:?}: last closed window was {applied}"))
}

// ---------------------------------------------------------------------------
// Window state sampler
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WinState {
    Normal,
    Maximized,
    Minimized,
}

/// What the user sees of a window's placement: state and window rect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PlacementKey {
    state: WinState,
    rect: (i32, i32, i32, i32),
}

impl std::fmt::Display for PlacementKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (l, t, r, b) = self.rect;
        write!(f, "{:?} ({l},{t})-({r},{b}) {}x{}", self.state, r - l, b - t)
    }
}

#[derive(Clone, Copy, Debug)]
struct Sample {
    t: Instant,
    visible: bool,
    cloaked: u32,
    layered: bool,
    /// Layered alpha; `None` if not layered or no attributes are set.
    alpha: Option<u8>,
    key: PlacementKey,
}

impl Sample {
    /// Visible on screen: visible, not cloaked, not minimized, and not
    /// layered with alpha 0 / without attributes.
    fn shown(&self) -> bool {
        self.visible
            && self.cloaked == 0
            && self.key.state != WinState::Minimized
            && (!self.layered || self.alpha.is_some_and(|a| a > 0))
    }

    fn same_state(&self, o: &Sample) -> bool {
        self.visible == o.visible
            && self.cloaked == o.cloaked
            && self.layered == o.layered
            && self.alpha == o.alpha
            && self.key == o.key
    }
}

fn sample(raw: isize, t: Instant) -> Sample {
    let hwnd = hwnd_of(raw);
    // Visibility first, placement second (see the module docs).
    // SAFETY: these reads tolerate stale HWNDs; out pointers are valid.
    unsafe {
        let visible = IsWindowVisible(hwnd).as_bool();
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            (&mut cloaked as *mut u32).cast(),
            size_of::<u32>() as u32,
        );
        let layered = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & WS_EX_LAYERED.0 as isize != 0;
        let alpha = if layered {
            let mut a = 255u8;
            let mut flags = LAYERED_WINDOW_ATTRIBUTES_FLAGS(0);
            match GetLayeredWindowAttributes(hwnd, None, Some(&mut a), Some(&mut flags)) {
                Ok(()) if flags.0 & LWA_ALPHA.0 != 0 => Some(a),
                // Colour key only: drawn opaque.
                Ok(()) => Some(255),
                Err(_) => None,
            }
        } else {
            None
        };
        let mut wp = WINDOWPLACEMENT { length: size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
        let _ = GetWindowPlacement(hwnd, &mut wp);
        let state = if wp.showCmd == SW_SHOWMAXIMIZED.0 as u32 {
            WinState::Maximized
        } else if wp.showCmd == SW_SHOWMINIMIZED.0 as u32 {
            WinState::Minimized
        } else {
            WinState::Normal
        };
        let mut r = RECT::default();
        let _ = GetWindowRect(hwnd, &mut r);
        Sample {
            t,
            visible,
            cloaked,
            layered,
            alpha,
            key: PlacementKey { state, rect: (r.left, r.top, r.right, r.bottom) },
        }
    }
}

/// One frame at 60 Hz: a state-only change shorter than this inside the
/// trigger's placement change is not a visible step (see the module docs).
const FRAME: Duration = Duration::from_millis(16);

#[derive(Default)]
struct Trace {
    first_seen: Option<Instant>,
    /// State changes only (consecutive duplicates dropped).
    samples: Vec<Sample>,
}

#[derive(Default)]
struct Sampled {
    /// New windows in order of first appearance.
    order: Vec<isize>,
    traces: HashMap<isize, Trace>,
    iterations: u64,
    max_gap: Duration,
}

struct Sampler {
    stop: Arc<AtomicBool>,
    seen: Arc<Mutex<Vec<isize>>>,
    thread: Option<JoinHandle<Sampled>>,
}

impl Sampler {
    fn start(pre: HashSet<isize>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (s, n) = (stop.clone(), seen.clone());
        let thread = std::thread::Builder::new()
            .name("restore-timing-sampler".into())
            .spawn(move || {
                let mut out = Sampled::default();
                let mut last = Instant::now();
                while !s.load(Ordering::SeqCst) {
                    let now = Instant::now();
                    out.max_gap = out.max_gap.max(now - last);
                    last = now;
                    out.iterations += 1;
                    for raw in top_level_cabinets() {
                        if pre.contains(&raw) {
                            continue;
                        }
                        let smp = sample(raw, Instant::now());
                        let trace = out.traces.entry(raw).or_default();
                        if trace.first_seen.is_none() {
                            trace.first_seen = Some(now);
                            out.order.push(raw);
                            n.lock().unwrap().push(raw);
                        }
                        if trace.samples.last().is_none_or(|l| !l.same_state(&smp)) {
                            trace.samples.push(smp);
                        }
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                out
            })
            .expect("spawn the sampler");
        Self { stop, seen, thread: Some(thread) }
    }

    /// New windows seen so far, in order of appearance.
    fn seen(&self) -> Vec<isize> {
        self.seen.lock().unwrap().clone()
    }

    fn finish(&mut self) -> Sampled {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.take().map(|t| t.join().unwrap_or_default()).unwrap_or_default()
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Activity_Log watcher
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct RestoreEntry {
    seen_at: Instant,
    line: String,
    launch: String,
    windows: u32,
    tabs: u32,
    omitted: u32,
    redirected: u32,
    duration_ms: u64,
}

fn parse_restore(line: &str, seen_at: Instant) -> Option<RestoreEntry> {
    let p = parse_line(line)?;
    if p.event != "RESTORE" {
        return None;
    }
    let num = |k: &str| p.get_str(k).and_then(|v| v.parse::<u64>().ok());
    Some(RestoreEntry {
        seen_at,
        line: line.to_string(),
        launch: p.get_str("launch")?,
        windows: num("windows")? as u32,
        tabs: num("tabs")? as u32,
        omitted: num("omitted")? as u32,
        redirected: num("redirected")? as u32,
        duration_ms: num("duration_ms")?,
    })
}

struct LogWatcher {
    stop: Arc<AtomicBool>,
    found: Arc<Mutex<Option<RestoreEntry>>>,
    /// First `ERROR` entry of a restore operation (`operation=restore-…`),
    /// which stops the restoration (Req 4.10).
    error: Arc<Mutex<Option<(Instant, String)>>>,
    thread: Option<JoinHandle<()>>,
}

impl LogWatcher {
    fn start(path: PathBuf) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let found = Arc::new(Mutex::new(None));
        let error = Arc::new(Mutex::new(None));
        let (s, f, er) = (stop.clone(), found.clone(), error.clone());
        let thread = std::thread::spawn(move || {
            while !s.load(Ordering::SeqCst) {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    let at = Instant::now();
                    if let Some(e) = text.lines().find_map(|l| parse_restore(l, at)) {
                        *f.lock().unwrap() = Some(e);
                        return;
                    }
                    let mut slot = er.lock().unwrap();
                    if slot.is_none() {
                        *slot = text
                            .lines()
                            .find(|l| {
                                parse_line(l).is_some_and(|p| {
                                    p.event == "ERROR"
                                        && p.get_str("operation").is_some_and(|o| o.starts_with("restore"))
                                })
                            })
                            .map(|l| (at, l.to_string()));
                    }
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        Self { stop, found, error, thread: Some(thread) }
    }

    fn found(&self) -> Option<RestoreEntry> {
        self.found.lock().unwrap().clone()
    }

    fn error(&self) -> Option<(Instant, String)> {
        self.error.lock().unwrap().clone()
    }
}

impl Drop for LogWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ---------------------------------------------------------------------------
// In-process runtime
// ---------------------------------------------------------------------------

struct Runtime {
    thread: Option<JoinHandle<i32>>,
    engine: Option<isize>,
}

impl Runtime {
    fn start(data_dir: &Path) -> Self {
        let dir = data_dir.to_path_buf();
        let thread = std::thread::Builder::new()
            .name("esr-engine-test".into())
            .spawn(move || {
                let storage = Storage::open(&dir);
                let input = startup_input(storage.read_session(), false);
                let options = RuntimeOptions {
                    ui: false,
                    panic_guard: false,
                    late_sink: late_sink_requested(),
                    ..RuntimeOptions::default()
                };
                run(Settings::default(), input, storage, options)
            })
            .expect("spawn the runtime");
        let engine = wait(Duration::from_secs(10), own_engine_window);
        Self { thread: Some(thread), engine }
    }

    /// Posts `ESR_EXIT` to this process's Engine window and waits for the
    /// runtime to return.
    fn stop(&mut self) -> Result<(), String> {
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        if let Some(engine) = self.engine {
            let exit = InstanceMessages::register().exit;
            // SAFETY: posting a registered message to this process's own window.
            let _ = unsafe { PostMessageW(Some(hwnd_of(engine)), exit, WPARAM(0), LPARAM(0)) };
        }
        if wait(Duration::from_secs(15), || thread.is_finished().then_some(())).is_none() {
            return Err("the runtime did not exit after ESR_EXIT".into());
        }
        match thread.join() {
            Ok(0) => Ok(()),
            Ok(code) => Err(format!("the runtime exited with code {code}")),
            Err(_) => Err("the runtime thread panicked".into()),
        }
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

// ---------------------------------------------------------------------------
// Cases
// ---------------------------------------------------------------------------

enum TabSpec {
    /// A folder `root\<name>`.
    Local(String),
    /// [`OFFLINE_SHARE`].
    Offline,
}

struct WinSpec {
    tabs: Vec<TabSpec>,
    /// Saved Active_Tab (always a local tab here).
    active: usize,
    maximized: bool,
}

/// One expected restored window.
struct Expected {
    /// Saved window id.
    id: u32,
    /// Normalized parsing names of the restored tabs, left to right.
    paths: Vec<String>,
    /// Leaf names (the 22621 tab captions), left to right.
    leaves: Vec<String>,
    /// Restored Active_Tab index.
    active: usize,
    maximized: bool,
    restored: Rect,
}

fn local(window: usize, count: usize) -> Vec<TabSpec> {
    (0..count).map(|i| TabSpec::Local(format!("w{window}_t{i:02}"))).collect()
}

/// Builds the Pending Session_File and the expected outcome.
fn build(guard: &ExplorerGuard, specs: &[WinSpec]) -> Result<(SessionDoc, Vec<Expected>), String> {
    let all = displays();
    let primary = all.iter().find(|d| d.primary).or(all.first()).ok_or("no display")?.clone();
    let work = primary.work;
    let width = (work.width() - 240).clamp(400, 1500) as i32;
    let height = (work.height() - 160).clamp(300, 850) as i32;
    let mut windows = Vec::new();
    let mut expected = Vec::new();
    for (i, spec) in specs.iter().enumerate() {
        let (left, top) = (work.left + 40 + 60 * i as i32, work.top + 30 + 50 * i as i32);
        let restored = Rect { left, top, right: left + width, bottom: top + height };
        let mut tabs = Vec::new();
        let (mut paths, mut leaves) = (Vec::new(), Vec::new());
        let mut active = 0;
        for (j, tab) in spec.tabs.iter().enumerate() {
            let location = match tab {
                TabSpec::Local(name) => {
                    let dir = guard.dir(name);
                    let text = dir.to_str().ok_or("non-UTF-8 temp path")?;
                    let loc = try_capture_from_parsing_name(&WideString::from(text))
                        .map_err(|e| format!("capture {}: {e:?}", dir.display()))?;
                    if j == spec.active {
                        active = paths.len();
                    }
                    paths.push(normalize(&loc.parsing_name.to_string_lossy()));
                    leaves.push(name.clone());
                    loc
                }
                TabSpec::Offline => LocationId {
                    case_sensitive: false,
                    kind: LocationKind::FileSystem(Volume::Unc),
                    parsing_name: OFFLINE_SHARE.into(),
                    display_name: "x".into(),
                    pidl: Vec::new(),
                    search: None,
                },
            };
            tabs.push(TabRec { location });
        }
        let id = i as u32 + 1;
        windows.push(WindowRec {
            id: WindowId(id),
            tabs,
            active: spec.active,
            placement: Placement {
                state: if spec.maximized { ShowState::Maximized } else { ShowState::Normal },
                restored,
                display: DisplayRef { device: primary.device.clone(), monitor: primary.monitor },
            },
        });
        expected.push(Expected { id, paths, leaves, active, maximized: spec.maximized, restored });
    }
    let activation = windows.iter().map(|w| w.id).collect();
    let doc = SessionDoc {
        version: FORMAT_VERSION,
        status: Status::Pending,
        failed_attempts: 0,
        saved_at: Timestamp {
            year: 2025,
            month: 1,
            day: 31,
            hour: 9,
            minute: 15,
            second: 2,
            millis: 123,
            offset_minutes: 0,
        },
        cause: SaveCause::LastWindowClosed,
        snapshot: Snapshot { windows, activation },
    };
    validate_doc(&doc).map_err(|e| format!("invalid test snapshot: {e}"))?;
    Ok((doc, expected))
}

/// Normalized tab locations of every new Explorer window (ShellWindows order).
fn observe(pre: &HashSet<isize>) -> Vec<(isize, Vec<String>)> {
    enumerate_all_explorer_windows()
        .unwrap_or_default()
        .into_iter()
        .filter(|g| !pre.contains(&(g.top_hwnd.0 as isize)))
        .map(|g| {
            let names = g
                .tabs
                .iter()
                .map(|t| {
                    current_folder_pidl(&t.browser)
                        .ok()
                        .and_then(|p| parsing_name(&p))
                        .map(|n| normalize(&n))
                        .unwrap_or_else(|| "<unknown>".into())
                })
                .collect();
            (g.top_hwnd.0 as isize, names)
        })
        .collect()
}

/// expected index → hwnd, when every expected window is open with exactly
/// its Locations (as a multiset) and no other new window has tabs.
fn match_windows(obs: &[(isize, Vec<String>)], expected: &[Expected]) -> Option<Vec<isize>> {
    if obs.len() != expected.len() {
        return None;
    }
    let mut out = Vec::new();
    for e in expected {
        let mut want = e.paths.clone();
        want.sort();
        let hit = obs.iter().find(|(_, names)| {
            let mut have = names.clone();
            have.sort();
            have == want
        })?;
        out.push(hit.0);
    }
    let distinct: HashSet<_> = out.iter().collect();
    (distinct.len() == out.len()).then_some(out)
}

/// Checks the UIA order and selection against `e`. With an overflowing strip
/// only the exposed items are checked (in order, and the selection if exposed).
fn check_strip(state: &TabStripState, e: &Expected) -> Result<String, String> {
    let names: Vec<String> = state.names.iter().map(WideString::to_string_lossy).collect();
    let mut indices = Vec::new();
    for n in &names {
        let i = (0..e.paths.len())
            .find(|&i| n.eq_ignore_ascii_case(&e.leaves[i]) || normalize(n) == e.paths[i])
            .ok_or_else(|| format!("unexpected tab caption {n:?} in {names:?}"))?;
        indices.push(i);
    }
    if indices.windows(2).any(|p| p[0] >= p[1]) {
        return Err(format!("tab order {names:?} differs from the saved order {:?}", e.leaves));
    }
    if names.len() == e.paths.len() {
        if state.selected != Some(e.active) {
            return Err(format!(
                "active tab {:?} (strip {names:?}), expected {} ({})",
                state.selected, e.active, e.leaves[e.active]
            ));
        }
        Ok(format!("{} tabs in saved order, active {}", names.len(), e.active))
    } else {
        match state.selected {
            Some(s) if indices[s] != e.active => Err(format!(
                "active tab {} ({}), expected {} ({})",
                indices[s], names[s], e.active, e.leaves[e.active]
            )),
            Some(_) => Ok(format!(
                "strip overflows: {} of {} tabs exposed, in saved order, active tab correct",
                names.len(),
                e.paths.len()
            )),
            None => Ok(format!(
                "strip overflows: {} of {} tabs exposed, in saved order, active tab not exposed",
                names.len(),
                e.paths.len()
            )),
        }
    }
}

/// Visual cleanliness verdict (Req 7.2, 7.3, 7.7): (violations, notes).
/// Prints every trace.
fn analyze_visuals(sampled: &Sampled, trigger: isize, restored: &[isize], t0: Instant) -> (Vec<String>, Vec<String>) {
    let mut violations = Vec::new();
    let mut notes = Vec::new();
    for &raw in &sampled.order {
        let trace = &sampled.traces[&raw];
        let Some(last) = trace.samples.last() else { continue };
        let final_key = last.key;
        let role = if raw == trigger {
            "trigger"
        } else if restored.contains(&raw) {
            "restored"
        } else {
            "unexpected"
        };
        println!("  window {:?} ({role}), {} state changes:", hwnd_of(raw), trace.samples.len());
        for (i, s) in trace.samples.iter().enumerate() {
            let until = trace.samples.get(i + 1).map_or("end".to_string(), |n| format!("{} ms", ms(s.t, n.t)));
            if i < 60 {
                println!(
                    "    {:+6} ms  shown={} visible={} cloaked={} layered={} alpha={:?}  {}  (for {until})",
                    ms(t0, s.t),
                    s.shown(),
                    s.visible,
                    s.cloaked,
                    s.layered,
                    s.alpha,
                    s.key,
                );
            } else if i == 60 {
                println!("    ... ({} more)", trace.samples.len() - 60);
            }
        }
        let shown: Vec<(usize, &Sample)> =
            trace.samples.iter().enumerate().filter(|(_, s)| s.shown()).collect();
        match role {
            "unexpected" => {
                if let Some((_, s)) = shown.first() {
                    violations.push(format!(
                        "Req 7.3: Explorer window {:?} that is not a restored window was shown at {:+} ms ({})",
                        hwnd_of(raw),
                        ms(t0, s.t),
                        s.key
                    ));
                }
            }
            "trigger" => {
                // Distinct shown placements in order (Req 7.7: initial, then final).
                let mut runs: Vec<(PlacementKey, Instant)> = Vec::new();
                for (_, s) in &shown {
                    if runs.last().is_none_or(|(k, _)| *k != s.key) {
                        runs.push((s.key, s.t));
                    }
                }
                // A state-only change at the unchanged window rect, for less
                // than a frame, right before the next placement, is the inside
                // of one SetWindowPlacement (win32k clears WS_MAXIMIZE before
                // it moves the window; see the module docs): not a step.
                let mut i = 1;
                while i + 1 < runs.len() {
                    let (prev, cur, next) = (runs[i - 1], runs[i], runs[i + 1]);
                    if cur.0.rect == prev.0.rect && cur.0.state != prev.0.state && next.1 - cur.1 < FRAME {
                        notes.push(format!(
                            "trigger: {} -> {} at the same rect for {:.1} ms inside its placement change (win32k transition, not a visible step)",
                            prev.0,
                            cur.0,
                            (next.1 - cur.1).as_secs_f64() * 1000.0
                        ));
                        runs.remove(i);
                    } else {
                        i += 1;
                    }
                }
                let ok = runs.len() <= 2 && runs.last().is_none_or(|(k, _)| *k == final_key);
                if !ok {
                    let list: Vec<String> =
                        runs.iter().map(|(k, t)| format!("{:+} ms {k}", ms(t0, *t))).collect();
                    violations.push(format!(
                        "Req 7.7: the trigger was shown at {} placements (allowed: initial then final {final_key}): {}",
                        runs.len(),
                        list.join("; ")
                    ));
                }
            }
            _ => {
                for (i, s) in &shown {
                    if s.key != final_key {
                        let until = trace.samples.get(i + 1).map_or(0, |n| ms(s.t, n.t));
                        violations.push(format!(
                            "Req 7.2: restored window {:?} shown at {:+} ms at {} for {until} ms (final {final_key})",
                            hwnd_of(raw),
                            ms(t0, s.t),
                            s.key
                        ));
                    }
                }
            }
        }
    }
    (violations, notes)
}

/// Outcome of one case, printed and asserted after cleanup.
struct Outcome {
    skipped: Option<String>,
    report: Vec<String>,
    failures: Vec<String>,
}

fn run_case(name: &str, start: TriggerStart, specs: Vec<WinSpec>) {
    init_process(); // per-monitor DPI v2: physical pixels everywhere
    let _sta = StaGuard::init().expect("STA init");
    // One stable folder set for every case and run (see `with_stable_root`):
    // fresh paths per run made Explorer crash on the 5th back-to-back 3x10 run.
    let guard = ExplorerGuard::with_stable_root(&format!("rt_{name}"), "restore_timing");
    let data_dir = std::env::temp_dir().join(format!("esr_restore_timing_{name}_{}", std::process::id()));
    // `ESR_RT_REPEAT=n` runs the case n times in this process (a new runtime
    // each time), to watch this process's own resource use across runs.
    let repeat = std::env::var("ESR_RT_REPEAT").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(1).max(1);
    let mut outcome = Outcome { skipped: None, report: Vec::new(), failures: Vec::new() };
    for i in 0..repeat {
        let _ = std::fs::remove_dir_all(&data_dir);
        let mut o = execute(&guard, &data_dir, start, &specs);
        if repeat > 1 {
            o.report.insert(0, format!("--- iteration {}/{repeat}", i + 1));
        }
        outcome.skipped = outcome.skipped.or(o.skipped);
        outcome.report.extend(o.report);
        outcome.failures.extend(o.failures.into_iter().map(|f| if repeat > 1 { format!("[{}] {f}", i + 1) } else { f }));
        if outcome.skipped.is_some() {
            break;
        }
    }

    let cleanup = guard.finish();
    let _ = std::fs::remove_dir_all(&data_dir);
    let leftover_dir = data_dir.exists();
    let leftover_windows = top_level_cabinets();

    println!("\n=== restore_timing {name} ===");
    if let Some(reason) = &outcome.skipped {
        println!("SKIPPED: {reason}");
    }
    for line in &outcome.report {
        println!("{line}");
    }
    if !outcome.failures.is_empty() {
        println!("FAILURES:");
        for f in &outcome.failures {
            println!("  - {f}");
        }
    }
    println!(
        "cleanup: {}; Explorer windows open now: {}",
        match &cleanup {
            Ok(()) => "no test windows left".to_string(),
            Err(e) => e.clone(),
        },
        leftover_windows.len()
    );
    cleanup.unwrap();
    assert!(!leftover_dir, "temp data directory {} was not deleted", data_dir.display());
    assert!(outcome.failures.is_empty(), "{name}: {}", outcome.failures.join(" | "));
}

fn execute(guard: &ExplorerGuard, data_dir: &Path, start: TriggerStart, specs: &[WinSpec]) -> Outcome {
    let mut out = Outcome { skipped: None, report: Vec::new(), failures: Vec::new() };
    let skip = |mut o: Outcome, reason: String| {
        o.skipped = Some(reason);
        o
    };

    // Preconditions (never touch what is already open).
    let pre: HashSet<isize> = top_level_cabinets().into_iter().collect();
    if !pre.is_empty() {
        return skip(
            out,
            format!(
                "{} non-test Explorer window(s) are open; a restore needs the trigger to be the only one (they were not touched)",
                pre.len()
            ),
        );
    }
    let others: Vec<u32> =
        engine_windows().into_iter().map(|(_, p)| p).filter(|&p| p != std::process::id()).collect();
    if !others.is_empty() {
        return skip(out, format!("an Explorer Session Restore instance is running (pid {others:?})"));
    }
    // SAFETY: plain read.
    if unsafe { GetShellWindow() }.is_invalid() {
        return skip(out, "no shell (explorer.exe) is running".into());
    }

    // The trigger's initial placement (Explorer reuses the last closed one).
    match prime_initial_placement(guard, start) {
        Ok(msg) => out.report.push(format!("trigger start: {msg}")),
        Err(e) => {
            out.failures.push(format!("priming the trigger's initial placement: {e}"));
            return out;
        }
    }

    // Pending Session_File in the temp data directory.
    let (doc, expected) = match build(guard, specs) {
        Ok(v) => v,
        Err(e) => {
            out.failures.push(e);
            return out;
        }
    };
    let text = serialize(&doc);
    if let Err(e) = parse(text.as_bytes()) {
        out.failures.push(format!("the test Session_File does not parse: {e:?}"));
        return out;
    }
    if let Err(e) =
        std::fs::create_dir_all(data_dir).and_then(|()| std::fs::write(data_dir.join("session.esr"), &text))
    {
        out.failures.push(format!("writing the Session_File: {e}"));
        return out;
    }
    let offline = specs.iter().any(|s| s.tabs.iter().any(|t| matches!(t, TabSpec::Offline)));
    let w = expected.len() as u64;
    let t = expected.iter().map(|e| e.paths.len() as u64).sum::<u64>();
    let bound_ms = WINDOW_MS * w + TAB_MS * t + if offline { NETWORK_EXTRA_MS } else { 0 };
    out.report.push(format!(
        "snapshot: {w} window(s), {t} restored tab(s){}; bound {bound_ms} ms ({})",
        if offline { " + 1 offline UNC tab" } else { "" },
        if offline {
            "Req 7.8: 0.3 s x windows + 0.27 s x restored tabs + 5 s"
        } else {
            "Req 7.1: 0.3 s x windows + 0.27 s x tabs"
        }
    ));

    let switches = set_switches(true);
    if !switches.is_empty() {
        out.report.push(format!("diagnostics: disabled subsystems {switches:?} (ESR_RT_DISABLE)"));
    }
    if !esr_win::browser_worker::enabled() {
        out.report.push("diagnostics: browser workers off (ESR_RT_WORKERS=0)".into());
    }
    if late_sink_requested() {
        out.report.push("diagnostics: late sinks on (ESR_RT_LATE_SINK=1)".into());
    }
    let mut runtime = Runtime::start(data_dir);
    esr_win::diag::set_trace(std::env::var("ESR_RT_TRACE").is_ok_and(|v| v == "1"));
    if runtime.engine.is_none() {
        out.failures.push("the runtime created no ESR.Engine window".into());
        let _ = runtime.stop();
        return out;
    }
    // Let the runtime attach to the shell and install its hooks.
    wait(Duration::from_millis(500), || None::<()>);
    let own_before = process_usage(std::process::id());
    let explorer_pid = shell_pid();
    let explorer_before = explorer_pid.and_then(process_usage);

    let mut sampler = Sampler::start(pre.clone());
    let watcher = LogWatcher::start(data_dir.join("activity.log"));
    let launch_at = Instant::now();
    if let Err(e) = launch_explorer() {
        out.failures.push(e);
    }

    // Adopt every new window at once; poll ShellWindows; wait for RESTORE.
    let deadline = launch_at + Duration::from_millis(bound_ms + 20_000);
    let shell_poll = shell_poll_interval();
    let mut adopted: HashSet<isize> = HashSet::new();
    let mut shell_complete: Option<(Instant, Vec<isize>)> = None;
    let mut last_poll = launch_at;
    let mut restore: Option<RestoreEntry> = None;
    let mut settle_end: Option<Instant> = None;
    loop {
        pump_once();
        for raw in sampler.seen().into_iter().chain(top_level_cabinets()) {
            if !pre.contains(&raw) && adopted.insert(raw) {
                guard.adopt(hwnd_of(raw));
            }
        }
        let now = Instant::now();
        let poll_due = match shell_poll {
            Some(p) => now - last_poll >= p,
            // Polling disabled: one check once the restore has ended.
            None => settle_end.is_some_and(|e| now >= e),
        };
        if shell_complete.is_none() && poll_due {
            last_poll = now;
            let obs = observe(&pre);
            if let Some(m) = match_windows(&obs, &expected) {
                shell_complete = Some((Instant::now(), m));
            }
        }
        if restore.is_none() {
            restore = watcher.found();
            if restore.is_some() {
                settle_end = Some(Instant::now() + SETTLE);
            }
        }
        if settle_end.is_some_and(|e| Instant::now() >= e) && (shell_complete.is_some() || shell_poll.is_none()) {
            break;
        }
        // After RESTORE, give ShellWindows up to 5 s more for the cross-check.
        if settle_end.is_some_and(|e| Instant::now() >= e + Duration::from_secs(5)) {
            break;
        }
        // A restore error stops the restoration (Req 4.10): no RESTORE will follow.
        if restore.is_none() && watcher.error().is_some_and(|(at, _)| Instant::now() >= at + Duration::from_secs(3)) {
            break;
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let sampled = sampler.finish();
    // Re-enable everything before the checks (they use UIA themselves).
    set_switches(false);
    for &raw in &sampled.order {
        if adopted.insert(raw) {
            guard.adopt(hwnd_of(raw));
        }
    }
    if esr_win::diag::tracing() {
        let t0 = sampled
            .order
            .first()
            .and_then(|raw| sampled.traces[raw].samples.iter().find(|s| s.shown()))
            .map_or(launch_at, |s| s.t);
        println!("trace (ms relative to the trigger first becoming visible; launch at {:+}):", ms(t0, launch_at));
        for (at, note) in esr_win::diag::take_notes() {
            println!("  {:+7} {note}", ms(t0, at));
        }
        println!("  counts (DWebBrowserEvents2 DISPID -> calls): {:?}", esr_win::diag::take_counts());
    }

    // ---- Timing -----------------------------------------------------------
    let trigger = sampled.order.first().copied();
    let trigger_shown = trigger.and_then(|raw| {
        sampled.traces[&raw].samples.iter().find(|s| s.shown()).map(|s| s.t)
    });
    out.report.push(format!(
        "sampler: {} iterations, max gap {:.1} ms; {} new Explorer window(s)",
        sampled.iterations,
        sampled.max_gap.as_secs_f64() * 1000.0,
        sampled.order.len()
    ));
    match (trigger, trigger_shown) {
        (Some(raw), Some(t0)) => {
            let first_seen = sampled.traces[&raw].first_seen.unwrap_or(t0);
            out.report.push(format!(
                "trigger {:?}: created +{} ms, first visible +{} ms after the launch",
                hwnd_of(raw),
                ms(launch_at, first_seen),
                ms(launch_at, t0)
            ));
        }
        _ => out.failures.push("no trigger window became visible".into()),
    }
    match &restore {
        None => out.failures.push(format!(
            "no RESTORE entry (restoration did not complete; waited {} ms after the launch)",
            ms(launch_at, Instant::now())
        )),
        Some(r) => {
            out.report.push(format!("log: {}", r.line));
            if let Some(t0) = trigger_shown {
                let external = ms(t0, r.seen_at);
                out.report.push(format!(
                    "TIMING: trigger visible -> RESTORE logged = {external} ms (entry duration_ms = {}); bound {bound_ms} ms -> {}",
                    r.duration_ms,
                    if external <= bound_ms as i64 && r.duration_ms <= bound_ms { "within" } else { "EXCEEDED" }
                ));
                if external > bound_ms as i64 {
                    out.failures.push(format!(
                        "restoration took {external} ms from the trigger becoming visible (bound {bound_ms} ms)"
                    ));
                }
                if r.duration_ms > bound_ms {
                    out.failures.push(format!(
                        "RESTORE duration_ms = {} exceeds the bound of {bound_ms} ms",
                        r.duration_ms
                    ));
                }
            }
            let omitted = if offline { 1 } else { 0 };
            if r.launch != "default"
                || r.windows as u64 != w
                || r.tabs as u64 != t
                || r.omitted != omitted
                || r.redirected != 0
            {
                out.failures.push(format!(
                    "RESTORE entry {:?}: expected launch=default windows={w} tabs={t} omitted={omitted} redirected=0",
                    r.line
                ));
            }
        }
    }
    if let (Some(t0), Some((at, _))) = (trigger_shown, &shell_complete) {
        out.report.push(format!(
            "cross-check: ShellWindows showed every expected Location {} ms after the trigger became visible ({})",
            ms(t0, *at),
            match shell_poll {
                Some(p) => format!("polled every {} ms", p.as_millis()),
                None => "polling disabled; checked once after the restore".into(),
            }
        ));
    }
    if let Some(t0) = trigger_shown {
        // When each other new window was first shown (revealed).
        for &raw in sampled.order.iter().skip(1) {
            if let Some(s) = sampled.traces[&raw].samples.iter().find(|s| s.shown()) {
                out.report.push(format!("window {:?} first shown {} ms after the trigger", hwnd_of(raw), ms(t0, s.t)));
            }
        }
        if let Some((at, line)) = watcher.error() {
            out.report.push(format!("restore error logged ~{} ms after the trigger became visible: {line}", ms(t0, at)));
        }
    }
    let log_text = std::fs::read_to_string(data_dir.join("activity.log")).unwrap_or_default();
    if offline {
        let entry = log_text.lines().filter_map(parse_line).find(|p| {
            (p.event == "TAB-OMITTED" || p.event == "TAB-REDIRECTED")
                && p.get_str("location").as_deref() == Some(OFFLINE_SHARE)
        });
        match entry {
            Some(p) if p.event == "TAB-OMITTED" && p.get_str("reason").as_deref() == Some("no-response-within-5s") => {
                out.report.push("offline share: omitted, reason no-response-within-5s (Req 9.2, 9.3)".into());
            }
            Some(p) => out.failures.push(format!(
                "offline share logged as {} reason {:?}; expected TAB-OMITTED no-response-within-5s",
                p.event,
                p.get_str("reason")
            )),
            None => out.failures.push("no TAB-OMITTED entry for the offline share".into()),
        }
    }

    // ---- Completion state (Req 7.1) ----------------------------------------
    let mapping = match &shell_complete {
        Some((_, m)) => Some(m.clone()),
        None => {
            let obs = observe(&pre);
            let m = match_windows(&obs, &expected);
            if m.is_none() {
                out.failures.push(format!("ShellWindows does not show the expected windows/Locations: {obs:?}"));
            }
            m
        }
    };
    if let Some(mapping) = &mapping {
        if trigger.is_some_and(|t| t != mapping[0]) {
            out.failures.push("the trigger window does not host the first window in Activation_Order".into());
        }
        let reader = if std::env::var("ESR_RT_SKIP_STRIP").is_ok_and(|v| v == "1") {
            out.report.push("diagnostics: UIA tab strip check skipped (ESR_RT_SKIP_STRIP=1)".into());
            None
        } else {
            TabStripReader::start(Arc::new(|_| {})).ok()
        };
        let keys = WindowKeys::new();
        for (e, &raw) in expected.iter().zip(mapping) {
            // Final placement.
            let last = sampled.traces.get(&raw).and_then(|t| t.samples.last()).copied();
            let final_ok = last.is_some_and(|s| {
                s.shown()
                    && if e.maximized {
                        s.key.state == WinState::Maximized
                    } else {
                        s.key.state == WinState::Normal
                            && s.key.rect
                                == (e.restored.left, e.restored.top, e.restored.right, e.restored.bottom)
                    }
            });
            let want = if e.maximized {
                "Maximized".to_string()
            } else {
                format!(
                    "Normal ({},{})-({},{})",
                    e.restored.left, e.restored.top, e.restored.right, e.restored.bottom
                )
            };
            let got = last.map_or("<no sample>".into(), |s| format!("{} shown={}", s.key, s.shown()));
            out.report.push(format!("window {} -> {:?}: final placement {got} (expected {want})", e.id, hwnd_of(raw)));
            if !final_ok {
                out.failures.push(format!("window {}: final placement {got}, expected {want}", e.id));
            }
            // Tab order and Active_Tab through UIA.
            let Some(reader) = &reader else {
                if std::env::var("ESR_RT_SKIP_STRIP").is_ok_and(|v| v == "1") {
                    continue;
                }
                out.failures.push("could not start the UIA tab strip reader".into());
                continue;
            };
            let key: WinKey = keys.key_of(hwnd_value(hwnd_of(raw)));
            let _ = reader.watch(key);
            let state = wait(Duration::from_secs(10), || {
                reader.read(key).ok().filter(|s| !s.names.is_empty())
            });
            match state.map(|s| check_strip(&s, e)) {
                Some(Ok(msg)) => out.report.push(format!("window {}: {msg}", e.id)),
                Some(Err(msg)) => out.failures.push(format!("window {}: {msg}", e.id)),
                None => out.failures.push(format!("window {}: the tab strip could not be read", e.id)),
            }
            reader.unwatch(key);
        }
    }

    // ---- Visual cleanliness (Req 7.2, 7.3, 7.7) ----------------------------
    if let (Some(trig), Some(t0)) = (trigger, trigger_shown) {
        println!("window state changes (ms relative to the trigger first becoming visible):");
        let restored: Vec<isize> = mapping.clone().unwrap_or_default();
        let (violations, notes) = analyze_visuals(&sampled, trig, &restored, t0);
        out.report.extend(notes.into_iter().map(|n| format!("VISUAL note: {n}")));
        if violations.is_empty() {
            out.report.push(
                "VISUAL: no window was shown at a placement other than its final one (trigger: initial, then final)".into(),
            );
        } else {
            out.report.push(format!("VISUAL: {} violation(s)", violations.len()));
            out.failures.extend(violations);
        }
    }

    // Stop the runtime only after the resource check; the guard closes
    // whatever is left afterwards.
    drop(watcher);
    // Resource check (task 16.3): close the windows while the runtime still
    // runs, so whatever it held for them must be released, and compare this
    // process (the runtime) and explorer.exe with the state before the launch.
    let mut closed = 0;
    for &raw in &sampled.order {
        if guard.close(hwnd_of(raw)) {
            closed += 1;
        }
    }
    wait(Duration::from_millis(2_000), || None::<()>);
    let own_after = process_usage(std::process::id());
    let explorer_after = explorer_pid.and_then(process_usage);
    out.report.push(format!(
        "LEAK check ({closed} window(s) closed with the runtime running, then 2 s): this process {} -> {}; explorer.exe {} -> {}",
        usage_text(own_before),
        usage_text(own_after),
        usage_text(explorer_before),
        usage_text(explorer_after)
    ));
    if let Err(e) = runtime.stop() {
        out.failures.push(e);
    }
    let (first, late) = esr_win::tab_strip::reselect_counts();
    out.report.push(format!("activation reselects: at the confirming check {first}, at the late check {late}"));
    esr_win::diag::set_trace(false);
    if !out.failures.is_empty() {
        out.report.push(format!("activity.log:\n{log_text}"));
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Req 7.1, 7.2, 7.7: 1 window × 10 local tabs within 0.3 + 2.7 = 3.0 s.
#[test]
#[ignore = "opens File Explorer windows; run alone with --ignored --nocapture --test-threads=1"]
fn restore_1_window_10_local_tabs_within_bound_and_clean() {
    run_case("1x10", TriggerStart::Normal, vec![WinSpec { tabs: local(1, 10), active: 6, maximized: false }]);
}

/// Req 7.7 with a maximized trigger (Explorer remembered a maximized window):
/// it must change to its normal final placement in one step; 1 window × 10
/// local tabs within 3.0 s (Req 7.1).
#[test]
#[ignore = "opens File Explorer windows; run alone with --ignored --nocapture --test-threads=1"]
fn restore_from_a_maximized_trigger_changes_it_in_one_step() {
    run_case("maxtrigger", TriggerStart::Maximized, vec![WinSpec { tabs: local(1, 10), active: 2, maximized: false }]);
}

/// Req 7.1, 7.2, 7.7: 3 windows with 30 local tabs within 0.9 + 8.1 = 9.0 s;
/// the third window is maximized (state applied while hidden, Req 7.2).
#[test]
#[ignore = "opens File Explorer windows; run alone with --ignored --nocapture --test-threads=1"]
fn restore_3_windows_30_local_tabs_within_bound_and_clean() {
    run_case(
        "3x10",
        TriggerStart::Normal,
        vec![
            WinSpec { tabs: local(1, 10), active: 3, maximized: false },
            WinSpec { tabs: local(2, 10), active: 9, maximized: false },
            WinSpec { tabs: local(3, 10), active: 0, maximized: true },
        ],
    );
}

/// Req 7.8 (with 7.2, 7.7): 1 window with 10 local tabs and one tab on an
/// offline UNC share (saved at position 4) within 3.0 s + 5 s = 8.0 s; the
/// share tab is omitted (`no-response-within-5s`).
#[test]
#[ignore = "opens File Explorer windows; run alone with --ignored --nocapture --test-threads=1"]
fn restore_with_offline_share_within_bound_plus_5_s() {
    let mut tabs = local(1, 10);
    tabs.insert(4, TabSpec::Offline);
    // Saved active index 8 is local tab w1_t07 (restored index 7).
    run_case("offline", TriggerStart::Normal, vec![WinSpec { tabs, active: 8, maximized: false }]);
}

/// Req 5.13: 2 windows × 65 tabs = 130 distinct folders, more than Explorer
/// 22621's sync-status cache survives (it crashes at 128). The restoration
/// opens 100 of them (`IMMEDIATE_FOLDER_BUDGET`) and restores the last 30
/// tabs of window 2 as deferred tabs: placeholders named after the saved
/// folder. Checks the `TABS-DEFERRED` entry, that explorer.exe is still the
/// same process (no crash), the placeholders' tab captions, and that
/// selecting a deferred tab navigates it to its saved folder. No timing
/// bound applies (Req 7.1 covers up to 30 tabs).
#[test]
#[ignore = "opens File Explorer windows; run alone with --ignored --nocapture --test-threads=1"]
fn restore_130_folders_defers_the_tabs_beyond_the_budget() {
    init_process();
    let _sta = StaGuard::init().expect("STA init");
    let guard = ExplorerGuard::with_stable_root("rt_deferred", "restore_timing");
    let data_dir = std::env::temp_dir().join(format!("esr_restore_timing_deferred_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data_dir);
    let (report, failures) = deferred_case(&guard, &data_dir);
    let cleanup = guard.finish();
    for _ in 0..20 {
        if !data_dir.exists() || std::fs::remove_dir_all(&data_dir).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    println!("\n=== restore_timing deferred ===");
    for line in &report {
        println!("{line}");
    }
    for f in &failures {
        println!("  FAILURE: {f}");
    }
    println!("Explorer windows open now: {}", top_level_cabinets().len());
    cleanup.unwrap();
    assert!(failures.is_empty(), "deferred: {}", failures.join(" | "));
}

fn deferred_case(guard: &ExplorerGuard, data_dir: &Path) -> (Vec<String>, Vec<String>) {
    use esr_core::plan::defer::IMMEDIATE_FOLDER_BUDGET;
    let (mut report, mut failures) = (Vec::new(), Vec::new());
    let pre: HashSet<isize> = top_level_cabinets().into_iter().collect();
    if !pre.is_empty() || engine_windows().iter().any(|&(_, p)| p != std::process::id()) {
        report.push("SKIPPED: other Explorer windows or another instance are open".into());
        return (report, failures);
    }
    let specs = vec![
        WinSpec { tabs: local(1, 65), active: 0, maximized: false },
        WinSpec { tabs: local(2, 65), active: 0, maximized: false },
    ];
    let (doc, expected) = match build(guard, &specs) {
        Ok(v) => v,
        Err(e) => return (report, vec![e]),
    };
    std::fs::create_dir_all(data_dir).unwrap();
    std::fs::write(data_dir.join("session.esr"), serialize(&doc)).unwrap();
    // Actives (w1_t00, w2_t00), then w1_t01..t64, then w2_t01..t34: 100.
    let deferred: Vec<usize> = (35..65).collect();
    let explorer_before = shell_pid();

    let mut runtime = Runtime::start(data_dir);
    esr_win::diag::set_trace(std::env::var("ESR_RT_TRACE").is_ok_and(|v| v == "1"));
    wait(Duration::from_millis(500), || None::<()>);
    let watcher = LogWatcher::start(data_dir.join("activity.log"));
    let launch_at = Instant::now();
    if let Err(e) = launch_explorer() {
        failures.push(e);
    }
    let mut adopted: HashSet<isize> = HashSet::new();
    let entry = wait(Duration::from_secs(120), || {
        for raw in top_level_cabinets() {
            if !pre.contains(&raw) && adopted.insert(raw) {
                guard.adopt(hwnd_of(raw));
            }
        }
        watcher.found().or_else(|| watcher.error().map(|(at, line)| RestoreEntry {
            seen_at: at,
            line,
            launch: String::new(),
            windows: 0,
            tabs: 0,
            omitted: 0,
            redirected: 0,
            duration_ms: 0,
        }))
    });
    for raw in top_level_cabinets() {
        if !pre.contains(&raw) && adopted.insert(raw) {
            guard.adopt(hwnd_of(raw));
        }
    }
    let log_text = std::fs::read_to_string(data_dir.join("activity.log")).unwrap_or_default();
    match &entry {
        Some(r) => report.push(format!("{} ms after the launch: {}", ms(launch_at, r.seen_at), r.line)),
        None => failures.push("no RESTORE entry within 120 s".into()),
    }
    if entry.as_ref().is_some_and(|r| r.windows != 2 || r.tabs != 130 || r.omitted != 0) {
        failures.push("RESTORE entry: expected windows=2 tabs=130 omitted=0".into());
    }
    let deferral = log_text.lines().filter_map(parse_line).find(|p| p.event == "TABS-DEFERRED");
    match deferral {
        Some(p) => {
            let field = |k: &str| p.get_str(k).unwrap_or_default();
            report.push(format!(
                "TABS-DEFERRED distinct={} immediate={} deferred={} budget={}",
                field("distinct"),
                field("immediate"),
                field("deferred"),
                field("budget")
            ));
            if (field("distinct"), field("immediate"), field("deferred"))
                != ("130".into(), IMMEDIATE_FOLDER_BUDGET.to_string(), "30".into())
            {
                failures.push("TABS-DEFERRED counts differ from distinct=130 immediate=100 deferred=30".into());
            }
        }
        None => failures.push("no TABS-DEFERRED entry".into()),
    }
    let explorer_after = shell_pid();
    report.push(format!("explorer.exe pid before {explorer_before:?}, after {explorer_after:?}"));
    if explorer_before != explorer_after {
        failures.push("explorer.exe restarted (crashed?) during the restoration".into());
    }

    // ShellWindows: window 1 shows its 65 folders; window 2 its first 35
    // and 30 placeholders under <data dir>\deferred.
    let placeholder_root = normalize(&data_dir.join("deferred").to_string_lossy());
    let obs = observe(&pre);
    let find = |want: &str| obs.iter().find(|(_, names)| names.iter().any(|n| n == want)).map(|(h, n)| (*h, n.clone()));
    let w2 = find(&expected[1].paths[0]);
    match (&w2, find(&expected[0].paths[0])) {
        (Some((_, names2)), Some((_, names1))) => {
            let placeholders = names2.iter().filter(|n| n.starts_with(&placeholder_root)).count();
            let real: HashSet<&String> = names2.iter().filter(|n| !n.starts_with(&placeholder_root)).collect();
            let want_real: HashSet<&String> = expected[1].paths[..35].iter().collect();
            report.push(format!(
                "window 1: {} tabs; window 2: {} tabs, {placeholders} placeholders",
                names1.len(),
                names2.len()
            ));
            if names1.len() != 65 || placeholders != 30 || real != want_real {
                failures.push(format!("unexpected tabs: window 1 {names1:?}; window 2 {names2:?}"));
            }
        }
        _ => failures.push(format!("restored windows not found: {obs:?}")),
    }

    // Captions, then select deferred tab 50 (w2_t50) like a click would.
    if let Some((raw, _)) = w2 {
        let reader = TabStripReader::start(Arc::new(|_| {})).ok();
        let keys = WindowKeys::new();
        let key: WinKey = keys.key_of(hwnd_value(hwnd_of(raw)));
        if let Some(reader) = &reader {
            let _ = reader.watch(key);
            let state = wait(Duration::from_secs(10), || reader.read(key).ok().filter(|s| s.names.len() == 65));
            match state {
                Some(s) => {
                    let names: Vec<String> = s.names.iter().map(WideString::to_string_lossy).collect();
                    let ok = deferred.iter().all(|&i| names[i] == format!("w2_t{i:02}"));
                    report.push(format!("window 2 captions of deferred tabs: {:?} … {:?}", names[35], names[64]));
                    if !ok {
                        failures.push(format!("deferred tab captions differ from the saved folder names: {names:?}"));
                    }
                }
                None => report.push("window 2: the strip did not expose all 65 tabs (overflow); captions not checked".into()),
            }
            let selected_at = Instant::now();
            match reader.select(key, 50) {
                Ok(()) => {
                    let want = expected[1].paths[50].clone();
                    let done = wait(Duration::from_secs(5), || {
                        observe(&pre).into_iter().find(|(h, _)| *h == raw).filter(|(_, n)| n.contains(&want)).map(|_| ())
                    });
                    match done {
                        Some(()) => report.push(format!(
                            "selected deferred tab 50: it shows {want} {} ms later",
                            ms(selected_at, Instant::now())
                        )),
                        None => failures.push("the selected deferred tab did not navigate to its saved folder within 5 s".into()),
                    }
                }
                Err(e) => failures.push(format!("UIA select of tab 50: {}", e.message)),
            }
            reader.unwatch(key);
        }
    }
    if esr_win::diag::tracing() {
        let notes: Vec<_> = esr_win::diag::take_notes().into_iter().filter(|(_, n)| !n.starts_with("uia: read")).collect();
        let from = notes.len().saturating_sub(150);
        println!("trace (last {} notes):", notes.len() - from);
        for (_, note) in &notes[from..] {
            println!("  {note}");
        }
        esr_win::diag::set_trace(false);
    }

    drop(watcher);
    for &raw in &adopted {
        let _ = guard.close(hwnd_of(raw));
    }
    wait(Duration::from_millis(1_000), || None::<()>);
    if let Err(e) = runtime.stop() {
        failures.push(e);
    }
    if !failures.is_empty() {
        report.push(format!("activity.log:\n{log_text}"));
    }
    (report, failures)
}

// ---------------------------------------------------------------------------
// explorer.exe dying mid-restoration (task 16.3 round 4c)
// ---------------------------------------------------------------------------

fn shell_running() -> bool {
    // SAFETY: plain window lookup.
    unsafe { windows::Win32::UI::WindowsAndMessaging::FindWindowW(w!("Shell_TrayWnd"), PCWSTR::null()) }
        .is_ok_and(|h| !h.is_invalid())
}

/// Waits for the shell to come back (Winlogon restarts it after it ends
/// abnormally); starts `explorer.exe` if it does not.
fn ensure_shell_running() {
    if wait(Duration::from_secs(15), || shell_running().then_some(())).is_some() {
        return;
    }
    // Through ShellExecuteExW (no inherited handles: a child holding this
    // process's redirected output would keep the caller's pipe open). With
    // no shell running, explorer.exe becomes the shell.
    if let Err(e) = launch_explorer() {
        eprintln!("could not start explorer.exe: {e}");
    }
    if wait(Duration::from_secs(30), || shell_running().then_some(())).is_none() {
        eprintln!("WARNING: the shell is still not running; start explorer.exe manually");
    }
}

/// The Engine window answers a message within 2 s (the Engine thread is not hung).
fn engine_responds(engine: isize) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_NULL};
    let mut result = 0usize;
    // SAFETY: a no-op message to this process's own window, with a timeout.
    let r = unsafe {
        SendMessageTimeoutW(hwnd_of(engine), WM_NULL, WPARAM(0), LPARAM(0), SMTO_ABORTIFHUNG, 2_000, Some(&mut result))
    };
    r.0 != 0
}

/// Req 4.7, 4.10, 13.3 with the late sinks (task 16.3 round 4c): explorer.exe
/// is killed (`taskkill /F`, as a crash would end it) while a 3 × 10
/// restoration builds its windows. The runtime must neither crash nor hang:
/// it logs a restore `ERROR` (`restore-shell`, or the failed call into the
/// dead process), keeps the Session_File Pending with one failed attempt
/// and the snapshot unchanged, keeps answering messages, and once the shell
/// is back, the next Trigger_Window retries the restoration and completes it.
///
/// **Destructive** (kills the shell, so the taskbar restarts): gated by
/// `ESR_ALLOW_DESTRUCTIVE=1` in addition to `#[ignore]`; restarts
/// `explorer.exe` if Winlogon does not.
#[test]
#[ignore = "DESTRUCTIVE: kills explorer.exe mid-restoration; also needs ESR_ALLOW_DESTRUCTIVE=1"]
fn restore_survives_explorer_dying_mid_restoration() {
    if std::env::var("ESR_ALLOW_DESTRUCTIVE").as_deref() != Ok("1") {
        println!("SKIPPED: kills the shell explorer.exe; set ESR_ALLOW_DESTRUCTIVE=1 to run");
        return;
    }
    init_process();
    let _sta = StaGuard::init().expect("STA init");
    let guard = ExplorerGuard::with_stable_root("rt_3x10", "restore_timing");
    let data_dir = std::env::temp_dir().join(format!("esr_restore_timing_crash_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data_dir);
    let (report, failures) = crash_case(&guard, &data_dir);
    ensure_shell_running();
    let cleanup = guard.finish();
    let _ = std::fs::remove_dir_all(&data_dir);
    println!("\n=== restore_timing explorer_crash ===");
    for line in &report {
        println!("{line}");
    }
    for f in &failures {
        println!("  FAILURE: {f}");
    }
    println!("Explorer windows open now: {}", top_level_cabinets().len());
    cleanup.unwrap();
    assert!(failures.is_empty(), "explorer_crash: {}", failures.join(" | "));
}

fn crash_case(guard: &ExplorerGuard, data_dir: &Path) -> (Vec<String>, Vec<String>) {
    let (mut report, mut failures) = (Vec::new(), Vec::new());
    let pre: HashSet<isize> = top_level_cabinets().into_iter().collect();
    if !pre.is_empty() || engine_windows().iter().any(|&(_, p)| p != std::process::id()) || !shell_running() {
        report.push("SKIPPED: other Explorer windows or another instance are open, or no shell runs".into());
        return (report, failures);
    }
    let specs = vec![
        WinSpec { tabs: local(1, 10), active: 3, maximized: false },
        WinSpec { tabs: local(2, 10), active: 9, maximized: false },
        WinSpec { tabs: local(3, 10), active: 0, maximized: true },
    ];
    let (doc, _) = match build(guard, &specs) {
        Ok(v) => v,
        Err(e) => return (report, vec![e]),
    };
    std::fs::create_dir_all(data_dir).unwrap();
    std::fs::write(data_dir.join("session.esr"), serialize(&doc)).unwrap();
    let read_doc = || std::fs::read(data_dir.join("session.esr")).ok().and_then(|b| parse(&b).ok());
    let adopt_new = |adopted: &mut HashSet<isize>| {
        for raw in top_level_cabinets() {
            if !pre.contains(&raw) && adopted.insert(raw) {
                guard.adopt(hwnd_of(raw));
            }
        }
    };

    let mut runtime = Runtime::start(data_dir);
    let Some(engine) = runtime.engine else {
        let _ = runtime.stop();
        return (report, vec!["the runtime created no ESR.Engine window".into()]);
    };
    wait(Duration::from_millis(500), || None::<()>);
    let shell_before = shell_pid();
    let watcher = LogWatcher::start(data_dir.join("activity.log"));
    let mut adopted: HashSet<isize> = HashSet::new();
    let launch_at = Instant::now();
    if let Err(e) = launch_explorer() {
        failures.push(e);
    }
    // Mid-restoration: all three windows exist (the third ~1.5 s after the
    // trigger) and their tabs are being built.
    let mid = wait(Duration::from_secs(10), || {
        pump_once();
        adopt_new(&mut adopted);
        (adopted.len() >= 3 && launch_at.elapsed() >= Duration::from_millis(2_500)).then_some(())
    });
    if mid.is_none() {
        failures.push(format!("only {} window(s) appeared within 10 s", adopted.len()));
    }
    if watcher.found().is_some() {
        failures.push("the restoration completed before the kill; nothing was tested".into());
    }
    let tabs_before: usize = adopted.iter().map(|&raw| esr_win::navigator::tab_children(hwnd_of(raw)).len()).sum();
    let killed_at = Instant::now();
    let status = std::process::Command::new("taskkill").args(["/F", "/IM", "explorer.exe"]).output();
    report.push(format!(
        "killed explorer.exe (pid {shell_before:?}) {} ms after the launch, with {} window(s) and {tabs_before} tab(s) open: {:?}",
        ms(launch_at, killed_at),
        adopted.len(),
        status.as_ref().map(|o| o.status)
    ));

    // The runtime notices, logs, and stays responsive. Which restore ERROR
    // comes first depends on timing: `restore-shell` when the shell's exit
    // (ShellLost) arrives first, otherwise the failed call into the dead
    // process (for example `restore-navigate`, RPC_S_CALL_FAILED). Either
    // stops the restoration as a failed attempt.
    let logged = wait(Duration::from_secs(10), || watcher.error());
    match &logged {
        Some((at, line)) => report.push(format!("logged {} ms after the kill: {line}", ms(killed_at, *at))),
        None => failures.push("no restore ERROR entry within 10 s of the kill".into()),
    }
    let alive = runtime.thread.as_ref().is_some_and(|t| !t.is_finished());
    let responds = engine_responds(engine);
    report.push(format!("after the kill: runtime thread running {alive}, Engine window responds {responds}"));
    if !alive || !responds {
        failures.push("the runtime ended or hung after the kill".into());
    }
    match read_doc() {
        Some(d) if d.status == Status::Pending && d.failed_attempts == 1 && d.snapshot == doc.snapshot => {
            report.push("Session_File after the kill: Pending, failed-attempts 1, snapshot unchanged".into());
        }
        other => failures.push(format!(
            "Session_File after the kill: {:?}",
            other.map(|d| (d.status, d.failed_attempts, d.cause, d.snapshot == doc.snapshot))
        )),
    }

    // The shell comes back; the next Trigger_Window retries.
    ensure_shell_running();
    let back_at = Instant::now();
    report.push(format!("shell running again {} ms after the kill (pid {:?})", ms(killed_at, back_at), shell_pid()));
    // Req 1.7: tracking resumes within 5 s of the new shell; give it that.
    wait(Duration::from_secs(5), || None::<()>);
    let gone: Vec<isize> = adopted.iter().copied().filter(|&raw| !alive_window(raw)).collect();
    report.push(format!("{} of the {} killed windows are gone", gone.len(), adopted.len()));
    drop(watcher);
    let watcher = LogWatcher::start(data_dir.join("activity.log"));
    let retry_at = Instant::now();
    if let Err(e) = launch_explorer() {
        failures.push(e);
    }
    let restored = wait(Duration::from_secs(40), || {
        pump_once();
        adopt_new(&mut adopted);
        watcher.found()
    });
    match &restored {
        Some(r) => report.push(format!("retry: {} ({} ms after the second launch)", r.line, ms(retry_at, r.seen_at))),
        None => failures.push("the retry did not complete within 40 s".into()),
    }
    wait(Duration::from_millis(1_000), || None::<()>);
    adopt_new(&mut adopted);
    let attempts = std::fs::read_to_string(data_dir.join("activity.log")).unwrap_or_default();
    report.push(format!("activity.log:\n{attempts}"));
    match read_doc() {
        Some(d) if d.status == Status::Consumed && d.failed_attempts == 2 => {
            report.push("Session_File after the retry: Consumed with failed-attempts 2 (the retry was counted)".into());
        }
        Some(d) if d.cause == SaveCause::Checkpoint => {
            report.push("Session_File after the retry: a checkpoint of the restored session".into());
        }
        other => failures.push(format!(
            "Session_File after the retry: {:?}",
            other.map(|d| (d.status, d.failed_attempts, d.cause))
        )),
    }
    drop(watcher);
    for &raw in &adopted {
        guard.close(hwnd_of(raw));
    }
    wait(Duration::from_millis(1_000), || None::<()>);
    if let Err(e) = runtime.stop() {
        failures.push(e);
    }
    (report, failures)
}

fn alive_window(raw: isize) -> bool {
    // SAFETY: IsWindow tolerates stale handles.
    unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(hwnd_of(raw))) }.as_bool()
}
