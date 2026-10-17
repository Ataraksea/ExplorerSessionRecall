//! Spike F1: hide a Folding_Candidate that another process asked for, as
//! early as possible (Req 14.16).
//!
//! ```text
//! cargo test -p esr-win --test spike_f1_hide_candidate -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Optional: `ESR_F1_METHODS=none,showasync-hide,…` (see `Hide::label`),
//! `ESR_F1_RUNS=<n>` (runs per method and scenario, default 3).
//!
//! Unlike R1 (where the product opened the window itself and could pass
//! `SW_HIDE`), a candidate is created by Explorer for someone else
//! (`explorer.exe <path>`, `ShellExecute("open")`), so it can only be hidden
//! from the outside, after the fact. Method:
//!
//! 1. A primary window is opened on a temp folder (folding only happens
//!    while one is open).
//! 2. For each [`Hide`] method and scenario (`explorer.exe <dir>` from a
//!    child process; `ShellExecuteExW("open", dir)` from this process; three
//!    `explorer.exe <dir>` at once = load), a [`HookThread`] applies the
//!    method at `EVENT_OBJECT_CREATE` / `EVENT_OBJECT_SHOW`, and a
//!    [`Sampler`] records for how long each new window is *shown* (visible,
//!    not cloaked, alpha ≠ 0, on screen; ~1 ms sampling).
//! 3. The test waits until each candidate shows its folder (its first
//!    navigation completed while hidden), reads its selection
//!    (`IFolderView2::GetSelection`, must not fail), keeps it hidden for
//!    another 700 ms (a late `SHOW` must not reveal it), then closes it with
//!    `WM_SYSCOMMAND SC_CLOSE` (off-screen windows are moved back first so
//!    Explorer does not remember an off-screen position).
//! 4. At the end a plain window is opened with no hook to check that the
//!    hidden-and-closed candidates did not change how Explorer opens the
//!    next window (visible, on screen), and the shell PID is compared.
//!
//! Output: per method and scenario the shown time per candidate (stats), the
//! create-event delivery delay, the time to the first navigation, and any
//! failure. Only windows created during the test are touched (ExplorerGuard).

#![cfg(windows)]

mod common;

use std::collections::HashSet;
use std::time::{Duration, Instant};

use common::explorer_guard::{ExplorerGuard, pump_for, top_level_cabinets};
use common::fold_spike::{
    Hide, HookThread, Sampler, browsers_of, hwnd, ms, ms_between, new_cabinets, open_primary,
    pidl_of_path, shell_execute, shell_pid, spawn_explorer, stats, unhide_for_close, wait_until,
    window_shows,
};
use esr_win::shell_util::StaGuard;
use esr_win::shell_windows_source::selection_of;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};

#[derive(Clone, Copy, Debug)]
enum Scenario {
    ExplorerExe,
    ShellOpen,
    Load3,
}

impl Scenario {
    fn label(self) -> &'static str {
        match self {
            Scenario::ExplorerExe => "explorer.exe <dir>",
            Scenario::ShellOpen => "ShellExecute open",
            Scenario::Load3 => "3 x explorer.exe at once",
        }
    }
    fn count(self) -> usize {
        if matches!(self, Scenario::Load3) { 3 } else { 1 }
    }
}

#[derive(Default)]
struct Row {
    shown: Vec<f64>,
    delivery: Vec<f64>,
    create_after_launch: Vec<f64>,
    show_after_create: Vec<f64>,
    first_nav: Vec<f64>,
    own_create_after_first: Vec<f64>,
    signals: std::collections::BTreeMap<String, u32>,
    visible_at_create: u32,
    failures: Vec<String>,
    notes: Vec<String>,
}

fn run_once(
    guard: &ExplorerGuard,
    hide: Hide,
    scenario: Scenario,
    run: usize,
    row: &mut Row,
) -> Result<(), String> {
    let dirs: Vec<_> = (0..scenario.count()).map(|i| guard.dir(&format!("c{i}"))).collect();
    let targets: Vec<Vec<u8>> = dirs.iter().map(|d| pidl_of_path(d)).collect::<Result<_, _>>()?;
    let pre: HashSet<isize> = top_level_cabinets().into_iter().collect();
    let hook = HookThread::start(hide, pre.clone());
    let sampler = Sampler::start(pre.clone());
    std::thread::sleep(Duration::from_millis(50));

    let t_launch = Instant::now();
    match scenario {
        Scenario::ExplorerExe | Scenario::Load3 => {
            for d in &dirs {
                spawn_explorer(&format!("\"{}\"", d.display()))?;
            }
        }
        Scenario::ShellOpen => {
            let _ = shell_execute(&dirs[0], Some("open"));
        }
    }

    // Wait for every candidate to exist and show its folder.
    let mut found: Vec<Option<(isize, Instant)>> = vec![None; dirs.len()];
    let ok = wait_until(Duration::from_secs(10), || {
        for h in new_cabinets(&pre) {
            guard.adopt(hwnd(h));
            for (i, t) in targets.iter().enumerate() {
                if found[i].is_none() && !found.iter().flatten().any(|(f, _)| *f == h) && window_shows(h, t) {
                    found[i] = Some((h, Instant::now()));
                }
            }
        }
        found.iter().all(Option::is_some)
    });
    if ok.is_none() {
        row.failures.push(format!(
            "run {run}: not every candidate showed its folder within 10 s ({} of {})",
            found.iter().flatten().count(),
            dirs.len()
        ));
    }
    // Selection read while hidden.
    for (h, _) in found.iter().flatten() {
        for (_, b) in browsers_of(*h) {
            let _ = selection_of(&b);
        }
    }
    pump_for(Duration::from_millis(700));
    let t_end = Instant::now();
    let tracks = sampler.stop();
    let (created, _events) = hook.stop();

    for (h, t_nav) in found.iter().flatten() {
        let c = created.iter().find(|c| c.hwnd == *h);
        let shown = tracks.get(h).map_or(0.0, |t| t.shown_ms(t_end));
        row.shown.push(shown);
        if let Some(c) = c {
            row.delivery.push(c.create_delivery_ms as f64);
            row.create_after_launch.push(ms_between(t_launch, c.t_create));
            if let Some(s) = c.t_show {
                row.show_after_create.push(ms_between(c.t_create, s));
            }
            row.first_nav.push(ms_between(c.t_create, *t_nav));
            if let Some(o) = c.t_own_create {
                row.own_create_after_first.push(ms_between(c.t_create, o));
            }
            *row.signals.entry(c.first_signal.clone()).or_default() += 1;
            if c.visible_at_create {
                row.visible_at_create += 1;
            }
            if run == 0 && !c.apply.is_empty() && row.notes.len() < 2 {
                row.notes.push(format!("apply: {}", c.apply.join(" | ")));
            }
        } else {
            row.failures.push(format!("run {run}: window {h:#x} not seen by the hook"));
        }
        if shown > 0.0
            && row.notes.len() < 6
            && let Some(t) = tracks.get(h)
        {
            row.notes.push(format!(
                "run {run} {h:#x}: shown {shown:.1} ms, samples {}, max gap {:.1} ms; states: {}",
                t.samples,
                ms(t.max_gap),
                t.changes.iter().take(8).cloned().collect::<Vec<_>>().join(" / ")
            ));
        }
        // Still hidden 700 ms after the first navigation?
        if !matches!(hide, Hide::None)
            && tracks.get(h).is_some_and(|t| t.shown.last().is_some_and(|(_, e)| e.is_none()))
        {
            row.failures.push(format!("run {run}: {h:#x} visible again at the end"));
        }
    }
    // Close (move off-screen windows back first).
    for (h, _) in found.iter().flatten() {
        if matches!(hide, Hide::Offscreen)
            && let Some(c) = created.iter().find(|c| c.hwnd == *h)
        {
            unhide_for_close(hwnd(*h), c.rect_at_create);
        }
        let t = Instant::now();
        if !guard.close(hwnd(*h)) {
            row.failures.push(format!("run {run}: {h:#x} did not close"));
        } else if ms(t.elapsed()) > 1500.0 {
            row.notes.push(format!("run {run}: close took {:.0} ms", ms(t.elapsed())));
        }
    }
    // Any other new window (not one of the candidates) is closed too.
    for h in new_cabinets(&pre) {
        if !found.iter().flatten().any(|(f, _)| *f == h) {
            guard.adopt(hwnd(h));
            row.failures.push(format!("run {run}: unexpected extra window {h:#x}"));
            guard.close(hwnd(h));
        }
    }
    pump_for(Duration::from_millis(300));
    Ok(())
}

#[test]
#[ignore = "spike: opens, hides and closes File Explorer windows on the interactive desktop"]
fn spike_f1_hide_candidate_window() {
    // SAFETY: plain call; failure (already set) is fine.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let _sta = StaGuard::init().expect("STA");
    let guard = ExplorerGuard::with_stable_root("f1", "f1_hide");
    let shell_before = shell_pid();
    let methods: Vec<Hide> = std::env::var("ESR_F1_METHODS")
        .ok()
        .map(|s| s.split(',').filter_map(|m| Hide::from_label(m.trim())).collect())
        .unwrap_or_else(|| Hide::ALL.to_vec());
    let runs: usize = std::env::var("ESR_F1_RUNS").ok().and_then(|s| s.parse().ok()).unwrap_or(3);

    let primary = open_primary(&guard, &guard.dir("primary")).expect("primary window");
    println!("[f1] primary {primary:#x}, shell pid {shell_before}");

    let mut report = Vec::new();
    for &hide in &methods {
        for scenario in [Scenario::ExplorerExe, Scenario::ShellOpen, Scenario::Load3] {
            let mut row = Row::default();
            for run in 0..runs {
                if let Err(e) = run_once(&guard, hide, scenario, run, &mut row) {
                    row.failures.push(format!("run {run}: {e}"));
                }
            }
            let line = format!(
                "[f1] {:<15} {:<26} shown: {} | first signal after launch: {} | delivery delay: {} | own CREATE after first signal: {} | SHOW after first signal: {} | first nav after first signal: {} | visible at first signal: {} | first signals: {:?}",
                hide.label(),
                scenario.label(),
                stats(&row.shown),
                stats(&row.create_after_launch),
                stats(&row.delivery),
                stats(&row.own_create_after_first),
                stats(&row.show_after_create),
                stats(&row.first_nav),
                row.visible_at_create,
                row.signals,
            );
            println!("{line}");
            for n in &row.notes {
                println!("       note: {n}");
            }
            for f in &row.failures {
                println!("       FAIL: {f}");
            }
            report.push(line);
        }
    }

    // Does Explorer still open a normal, visible window afterwards?
    let pre: HashSet<isize> = top_level_cabinets().into_iter().collect();
    let after_dir = guard.dir("after");
    let target = pidl_of_path(&after_dir).unwrap();
    let sampler = Sampler::start(pre.clone());
    spawn_explorer(&format!("\"{}\"", after_dir.display())).unwrap();
    let mut after = None;
    wait_until(Duration::from_secs(10), || {
        for h in new_cabinets(&pre) {
            guard.adopt(hwnd(h));
            if window_shows(h, &target) {
                after = Some(h);
                return true;
            }
        }
        false
    });
    pump_for(Duration::from_millis(800));
    let tracks = sampler.stop();
    match after {
        Some(h) => {
            let t = tracks.get(&h).cloned().unwrap_or_default();
            println!(
                "[f1] plain window afterwards: shown {:.0} ms of the last {:.0} ms; states: {}",
                t.shown_ms(Instant::now()),
                t.first_seen.map_or(0.0, |f| ms(f.elapsed())),
                t.changes.join(" / ")
            );
            guard.close(hwnd(h));
        }
        None => println!("[f1] FAIL: plain window afterwards did not show its folder"),
    }
    let shell_after = shell_pid();
    println!(
        "[f1] shell pid before {shell_before}, after {shell_after}{}",
        if shell_before == shell_after { " (no Explorer restart)" } else { " (EXPLORER RESTARTED)" }
    );
    println!("\n[f1] summary:\n{}", report.join("\n"));
    guard.finish().expect("cleanup");
}
