//! Integration tests for the tab-folding adapters (task 25.10) against the
//! real Explorer: the first-signal report of `WindowEventSource`, the
//! `CandidateHider`, and `ShellNavigator::close_window`.
//!
//! ```text
//! cargo test -p esr-win --test fold_adapters -- --ignored --nocapture --test-threads=1
//! ```
//!
//! | Test | Checks | Req |
//! |---|---|---|
//! | `first_signal_explorer_exe_dir` | `explorer.exe <dir>`: one `WindowAppeared` per new window, before its other signals, with the owner PID and the Fold_Bypass_Keys state (not held, then Ctrl+Shift held) | 14.2, 14.4 |
//! | `first_signal_shell_execute` | the same for `ShellExecute` (the way a desktop double-click opens a folder) | 14.2, 14.4 |
//! | `hidden_candidate_never_becomes_visible` | a candidate hidden at its first signal stays at layered alpha 0, is not shown by the sampler after the hide, and is shown for at most 100 ms in total | 14.16 |
//! | `startup_sweep_reveals_a_layered_alpha_zero_window` | the sweep removes the layered alpha-0 state a crashed monitor left | 14.14 |
//! | `close_window_closes_a_hidden_candidate` | `close_window` closes a hidden candidate (all tabs) without it showing | 14.2, 14.16 |
//!
//! Window safety: every Explorer window a test opens is adopted by the
//! `ExplorerGuard` (`tests/common`), which closes it, also on failure or panic,
//! and sweeps any window that shows a folder under its temp root. Windows
//! that were open before the test are never touched or reported. Guarded
//! tests serialise on a session-wide mutex; still run this binary alone and
//! single-threaded. The Ctrl+Shift cases inject those keys with `SendInput`
//! and always release them; do not touch the keyboard meanwhile.
//!
//! The pure decisions are unit-tested in `src/window_event_source.rs` and
//! `src/hider.rs`.

#![cfg(windows)]

mod common;

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::explorer_guard::{ExplorerGuard, pump_for, top_level_cabinets};
use common::fold_spike::{
    Sampler, hwnd, is_window, open_primary, pidl_of_path, shell_execute,
    spawn_explorer, wait_until, window_shows,
};
use esr_core::engine::{DesktopView, Event, WinKey};
use esr_win::hider::{CandidateHider, EventPoster};
use esr_win::keys::{TabKeys, WindowKeys, hwnd_of, hwnd_value};
use esr_win::navigator::{NavigatorSignal, ShellNavigator, layered_alpha};
use esr_win::shell_util::{StaGuard, window_process_id};
use esr_win::strategy::Strategies;
use esr_win::window_event_source::{
    EventPost, WindowEventConfig, WindowEventHandler, WindowEventSource, WindowSignal,
};
use windows::Win32::Foundation::{COLORREF, HWND};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
    VIRTUAL_KEY, VK_CONTROL, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_EXSTYLE, GetWindowLongPtrW, IsWindowVisible, LWA_ALPHA, SetLayeredWindowAttributes,
    SetWindowLongPtrW, WS_EX_LAYERED,
};

/// How long a launched window has to be reported and to show its folder.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Everything the source delivered, in order.
#[derive(Clone, Debug)]
enum Seen {
    Event(Event),
    Signal(WindowSignal),
}

type Log = Rc<RefCell<Vec<Seen>>>;

/// A started `WindowEventSource` with a recording sink. Stops on drop.
struct Rig {
    _source: WindowEventSource,
    log: Log,
}

/// Starts a source over `keys` whose event sink records everything and calls
/// `on_appeared` for each `WindowAppeared` (before recording it).
fn start_rig(keys: WindowKeys, on_appeared: impl Fn(WinKey) + 'static) -> Result<Rig, String> {
    let log: Log = Rc::default();
    let signals = log.clone();
    let handler: Rc<dyn WindowEventHandler> =
        Rc::new(move |s: WindowSignal| signals.borrow_mut().push(Seen::Signal(s)));
    let mut source = WindowEventSource::new(WindowEventConfig::default(), keys, handler);
    let events = log.clone();
    let sink: EventPost = Rc::new(move |e: Event| {
        if let Event::WindowAppeared { win, .. } = &e {
            on_appeared(*win);
        }
        events.borrow_mut().push(Seen::Event(e));
    });
    source.set_event_sink(sink);
    source.start().map_err(|e| format!("start source: {e:?}"))?;
    Ok(Rig { _source: source, log })
}

/// A `WindowAppeared` event, with its position in the log.
#[derive(Clone, Debug)]
struct Appeared {
    index: usize,
    win: WinKey,
    pid: u32,
    bypass_keys: bool,
    desktop: DesktopView,
    foreground: Option<WinKey>,
}

/// The `WindowAppeared` events at or after log position `from`.
fn appeared_since(log: &Log, from: usize) -> Vec<Appeared> {
    log.borrow()
        .iter()
        .enumerate()
        .skip(from)
        .filter_map(|(index, seen)| match seen {
            Seen::Event(Event::WindowAppeared { win, pid, bypass_keys, desktop, foreground }) => {
                Some(Appeared {
                    index,
                    win: *win,
                    pid: *pid,
                    bypass_keys: *bypass_keys,
                    desktop: desktop.clone(),
                    foreground: *foreground,
                })
            }
            _ => None,
        })
        .collect()
}

fn signal_window(signal: &WindowSignal) -> Option<WinKey> {
    match signal {
        WindowSignal::Changed { win, .. }
        | WindowSignal::Destroyed { win }
        | WindowSignal::Activated { win, .. } => Some(*win),
        WindowSignal::ProcessExited { .. } => None,
    }
}

/// Waits (pumping messages) for a reported window that shows `target`.
fn wait_for_candidate(log: &Log, from: usize, target: &[u8]) -> Result<Appeared, String> {
    let mut found = None;
    wait_until(LAUNCH_TIMEOUT, || {
        found = appeared_since(log, from)
            .into_iter()
            .find(|a| window_shows(a.win.hwnd as usize as isize, target));
        found.is_some()
    });
    found.ok_or_else(|| {
        format!(
            "no reported window showed the folder; reported: {:?}",
            appeared_since(log, from)
        )
    })
}

/// Ctrl and Shift pressed with `SendInput` (the Fold_Bypass_Keys). Released
/// by [`release`](Self::release) or on drop, so they are never left down.
struct BypassKeys {
    down: bool,
}

impl BypassKeys {
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

    fn press() -> Self {
        Self::key(VK_CONTROL, false);
        Self::key(VK_SHIFT, false);
        Self { down: true }
    }

    fn release(&mut self) {
        if self.down {
            self.down = false;
            Self::key(VK_SHIFT, true);
            Self::key(VK_CONTROL, true);
        }
    }
}

impl Drop for BypassKeys {
    fn drop(&mut self) {
        self.release();
    }
}

/// Layered alpha 0 set directly, as a hider that crashed left the window.
fn force_alpha_zero(h: HWND) -> Result<(), String> {
    // SAFETY: style and layered-attribute calls on a window the test created.
    unsafe {
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
        SetWindowLongPtrW(h, GWL_EXSTYLE, ex | WS_EX_LAYERED.0 as isize);
        SetLayeredWindowAttributes(h, COLORREF(0), 0, LWA_ALPHA)
            .map_err(|e| format!("SetLayeredWindowAttributes: {e}"))
    }
}

fn visible(h: HWND) -> bool {
    // SAFETY: IsWindowVisible tolerates stale HWNDs.
    unsafe { IsWindowVisible(h) }.as_bool()
}

/// Finishes a test: cleanup always runs, then the first error is raised.
fn conclude(name: &str, result: Result<(), String>, cleanup: Result<(), String>) {
    if let Err(e) = &result {
        println!("FAILED: {e}");
    }
    result.unwrap_or_else(|e| panic!("{name}: {e}"));
    cleanup.expect("test windows left open");
}

fn poster(sink: &Arc<Mutex<Vec<Event>>>) -> EventPoster {
    let sink = Arc::clone(sink);
    Arc::new(move |e| sink.lock().unwrap().push(e))
}

// ---------------------------------------------------------------------------
// First signal (Req 14.2, 14.4)
// ---------------------------------------------------------------------------

/// Opens `name` with `launch` while the source runs and checks the first
/// signal of the new window: reported once, before any other signal of the
/// window, with the owner PID, and with `bypass` as the Fold_Bypass_Keys state.
fn check_first_signal(
    guard: &ExplorerGuard,
    rig: &Rig,
    pre: &HashSet<isize>,
    name: &str,
    bypass: bool,
    launch: &dyn Fn(&Path) -> Result<(), String>,
) -> Result<(), String> {
    let dir = guard.dir(&format!("{name}_{}", if bypass { "keys" } else { "plain" }));
    let target = pidl_of_path(&dir)?;
    let from = rig.log.borrow().len();

    let mut keys = bypass.then(BypassKeys::press);
    if keys.is_some() {
        pump_for(Duration::from_millis(30));
    }
    launch(&dir)?;
    // Hold the keys until the first signal was read (Req 14.4: "at the moment
    // the Session_Monitor first detects" the window).
    wait_until(LAUNCH_TIMEOUT, || !appeared_since(&rig.log, from).is_empty());
    if let Some(keys) = keys.as_mut() {
        keys.release();
    }
    let appeared = wait_for_candidate(&rig.log, from, &target)?;
    let top = hwnd_of(appeared.win.hwnd);
    guard.adopt(top);
    println!(
        "  [{name} bypass={bypass}] {:?}: pid {}, bypass_keys {}, desktop {:?}, foreground {:?}",
        appeared.win, appeared.pid, appeared.bypass_keys, appeared.desktop, appeared.foreground
    );

    if appeared.bypass_keys != bypass {
        return Err(format!(
            "bypass_keys is {} with the keys {}held",
            appeared.bypass_keys,
            if bypass { "" } else { "not " }
        ));
    }
    if appeared.pid == 0 || appeared.pid != window_process_id(top) {
        return Err(format!(
            "pid {} is not the owner of {top:?} ({})",
            appeared.pid,
            window_process_id(top)
        ));
    }
    if pre.contains(&(appeared.win.hwnd as usize as isize)) {
        return Err("a window that was open before the source started was reported".into());
    }

    // The window's later signals (its own CREATE, SHOW, LOCATIONCHANGE ...)
    // arrive after the report, and the report is made once.
    pump_for(Duration::from_millis(1500));
    let reports = appeared_since(&rig.log, from)
        .into_iter()
        .filter(|a| a.win == appeared.win)
        .count();
    if reports != 1 {
        return Err(format!("{:?} was reported {reports} times", appeared.win));
    }
    let log = rig.log.borrow();
    let first_other = log.iter().position(|seen| {
        matches!(seen, Seen::Signal(s) if signal_window(s) == Some(appeared.win))
    });
    if let Some(i) = first_other
        && i < appeared.index
    {
        return Err(format!(
            "a signal of {:?} ({:?}) came before its WindowAppeared",
            appeared.win, log[i]
        ));
    }
    drop(log);

    if !guard.close(top) {
        return Err(format!("{top:?} did not close"));
    }
    Ok(())
}

fn run_first_signal(
    guard: &ExplorerGuard,
    name: &str,
    launch: &dyn Fn(&Path) -> Result<(), String>,
) -> Result<(), String> {
    let pre: HashSet<isize> = top_level_cabinets().into_iter().collect();
    let rig = start_rig(WindowKeys::new(), |_| {})?;
    for bypass in [false, true] {
        println!("{name}: Fold_Bypass_Keys held: {bypass}");
        check_first_signal(guard, &rig, &pre, name, bypass, launch)?;
    }
    Ok(())
}

#[test]
#[ignore = "requires an interactive desktop; opens and closes its own Explorer windows and presses Ctrl+Shift"]
fn first_signal_explorer_exe_dir() {
    let _sta = StaGuard::init().expect("STA init");
    let guard = ExplorerGuard::new("fold_first_exe");
    let result = run_first_signal(&guard, "exe", &|dir| {
        spawn_explorer(&format!("\"{}\"", dir.display()))
    });
    conclude("first_signal_explorer_exe_dir", result, guard.finish());
}

#[test]
#[ignore = "requires an interactive desktop; opens and closes its own Explorer windows and presses Ctrl+Shift"]
fn first_signal_shell_execute() {
    let _sta = StaGuard::init().expect("STA init");
    let guard = ExplorerGuard::new("fold_first_shex");
    let result = run_first_signal(&guard, "shex", &|dir| {
        // Its own STA thread; the result is not needed, the report is.
        drop(shell_execute(dir, Some("open")));
        Ok(())
    });
    conclude("first_signal_shell_execute", result, guard.finish());
}

// ---------------------------------------------------------------------------
// Hidden candidate (Req 14.16)
// ---------------------------------------------------------------------------

fn run_hidden_candidate(guard: &ExplorerGuard) -> Result<(), String> {
    let dir = guard.dir("hidden");
    let target = pidl_of_path(&dir)?;
    let keys = WindowKeys::new();
    let failures = Arc::new(Mutex::new(Vec::new()));
    let hider = Rc::new(
        CandidateHider::start(keys.clone(), poster(&failures)).map_err(|e| format!("{e:?}"))?,
    );
    // What the FoldingEngine's `HideWindow` does at a first signal.
    let hiding = hider.clone();
    let rig = start_rig(keys, move |win| {
        let _ = hiding.hide(win);
    })?;

    let pre: HashSet<isize> = top_level_cabinets().into_iter().collect();
    let sampler = Sampler::start(pre.clone());
    spawn_explorer(&format!("\"{}\"", dir.display()))?;
    let appeared = wait_for_candidate(&rig.log, 0, &target)?;
    let top = hwnd_of(appeared.win.hwnd);
    guard.adopt(top);

    let mut hidden_at = None;
    wait_until(Duration::from_secs(5), || {
        if layered_alpha(top) == Some(0) {
            hidden_at = Some(Instant::now());
            return true;
        }
        false
    });
    let hidden_at = hidden_at.ok_or("the candidate was never made layered alpha 0")?;

    // Explorer builds and shows the window meanwhile (its own SHOW and
    // LOCATIONCHANGE signals); it must stay invisible.
    let observe_until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < observe_until {
        pump_for(Duration::from_millis(20));
        if layered_alpha(top) != Some(0) {
            return Err(format!("the hidden candidate became visible: alpha {:?}", layered_alpha(top)));
        }
    }
    if let Some(failed) = failures.lock().unwrap().first() {
        return Err(format!("the hider reported {failed:?}"));
    }

    let end = Instant::now();
    let tracks = sampler.stop();
    let track = tracks
        .get(&(appeared.win.hwnd as usize as isize))
        .ok_or("the sampler never saw the candidate")?;
    println!(
        "  hidden after the first signal; shown {:.0} ms in total, intervals {} (samples {}, max gap {:?})",
        track.shown_ms(end),
        track.shown.len(),
        track.samples,
        track.max_gap
    );
    for change in &track.changes {
        println!("    {change}");
    }
    let grace = Duration::from_millis(100);
    for (from, to) in &track.shown {
        if *from > hidden_at || to.is_none_or(|to| to > hidden_at + grace) {
            return Err(format!("the candidate was shown after it was hidden: {track:?}"));
        }
    }
    if track.shown_ms(end) > 100.0 {
        return Err(format!("shown {:.0} ms in total (limit 100 ms)", track.shown_ms(end)));
    }

    // Revealing puts the window back the way Explorer made it.
    hider.reveal(appeared.win).map_err(|e| format!("{e:?}"))?;
    wait_until(Duration::from_secs(3), || layered_alpha(top).is_none())
        .ok_or_else(|| format!("reveal left alpha {:?}", layered_alpha(top)))?;
    if !guard.close(top) {
        return Err(format!("{top:?} did not close"));
    }
    Ok(())
}

#[test]
#[ignore = "requires an interactive desktop; opens and closes its own Explorer window"]
fn hidden_candidate_never_becomes_visible() {
    let _sta = StaGuard::init().expect("STA init");
    let guard = ExplorerGuard::new("fold_hidden");
    let result = run_hidden_candidate(&guard);
    conclude("hidden_candidate_never_becomes_visible", result, guard.finish());
}

// ---------------------------------------------------------------------------
// Startup sweep (Req 14.14)
// ---------------------------------------------------------------------------

fn run_sweep(guard: &ExplorerGuard) -> Result<(), String> {
    let dir = guard.dir("sweep");
    let top_raw = open_primary(guard, &dir)?;
    let top = hwnd(top_raw);
    if !visible(top) || layered_alpha(top).is_some() {
        return Err("the new window is not an ordinary visible window".into());
    }

    // The state a monitor that crashed while a candidate was hidden leaves.
    force_alpha_zero(top)?;
    if layered_alpha(top) != Some(0) {
        return Err(format!("alpha is {:?}, expected Some(0)", layered_alpha(top)));
    }

    // The runtime starts the hider and sweeps once before the Engine starts.
    let hider = CandidateHider::start(WindowKeys::new(), Arc::new(|_| {}))
        .map_err(|e| format!("{e:?}"))?;
    let report = hider.sweep_report();
    println!("  sweep: {report:?}");
    if report.revealed == 0 {
        return Err(format!("the sweep revealed nothing: {report:?}"));
    }
    wait_until(Duration::from_secs(3), || layered_alpha(top).is_none() && visible(top))
        .ok_or_else(|| {
            format!("after the sweep: alpha {:?}, visible {}", layered_alpha(top), visible(top))
        })?;
    // A second sweep finds nothing of ours to reveal.
    let again = hider.sweep();
    println!("  second sweep revealed {again}");
    drop(hider);
    if !guard.close(top) {
        return Err(format!("{top:?} did not close"));
    }
    Ok(())
}

#[test]
#[ignore = "requires an interactive desktop; opens and closes its own Explorer window"]
fn startup_sweep_reveals_a_layered_alpha_zero_window() {
    let _sta = StaGuard::init().expect("STA init");
    let guard = ExplorerGuard::new("fold_sweep");
    let result = run_sweep(&guard);
    conclude("startup_sweep_reveals_a_layered_alpha_zero_window", result, guard.finish());
}

// ---------------------------------------------------------------------------
// CloseWindow on a hidden candidate (Req 14.2, 14.16)
// ---------------------------------------------------------------------------

fn run_close_hidden(guard: &ExplorerGuard) -> Result<(), String> {
    let dir = guard.dir("closing");
    let windows = WindowKeys::new();
    let hider = CandidateHider::start(windows.clone(), Arc::new(|_| {}))
        .map_err(|e| format!("{e:?}"))?;
    // `close_window` needs no running navigator, only the shared window keys.
    let mut navigator = ShellNavigator::new(
        Strategies::DEFAULT,
        windows.clone(),
        TabKeys::new(),
        Rc::new(|_: NavigatorSignal| {}),
    );

    let top_raw = open_primary(guard, &dir)?;
    let top = hwnd(top_raw);
    let win = windows.key_of(hwnd_value(top));
    hider.hide(win).map_err(|e| format!("{e:?}"))?;
    wait_until(Duration::from_secs(5), || layered_alpha(top) == Some(0))
        .ok_or("the candidate was never made layered alpha 0")?;

    navigator.close_window(win).map_err(|e| format!("close_window: {e:?}"))?;
    // The window closes (SC_CLOSE closes all of its tabs) and never shows.
    let mut shown = None;
    let closed = wait_until(Duration::from_secs(10), || {
        if is_window(top_raw) && layered_alpha(top) != Some(0) && shown.is_none() {
            shown = Some(layered_alpha(top));
        }
        !is_window(top_raw)
    });
    if closed.is_none() {
        return Err("close_window did not close the hidden candidate".into());
    }
    if let Some(alpha) = shown {
        return Err(format!("the candidate became visible while closing: alpha {alpha:?}"));
    }

    // A window that is gone is an error, not a post to a stale handle.
    if navigator.close_window(win).is_ok() {
        return Err("close_window succeeded for a window that is gone".into());
    }
    drop(hider);
    Ok(())
}

#[test]
#[ignore = "requires an interactive desktop; opens and closes its own Explorer window"]
fn close_window_closes_a_hidden_candidate() {
    let _sta = StaGuard::init().expect("STA init");
    let guard = ExplorerGuard::new("fold_close");
    let result = run_close_hidden(&guard);
    conclude("close_window_closes_a_hidden_candidate", result, guard.finish());
}
