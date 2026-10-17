//! Spike F2: fold a Folding_Candidate into the primary window, and Tab_Switch
//! (Req 14.1, 14.6, 14.7, 14.16–14.22).
//!
//! ```text
//! cargo test -p esr-win --test spike_f2_fold -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Method: a primary window is opened on a temp folder. Each case asks for a
//! folder from another process or this one; the candidate is hidden at its
//! first signal with layered alpha 0 (the F1 choice) and then folded:
//!
//! 1. wait until the candidate shows its folder (first navigation), read its
//!    selection (`IFolderView2::GetSelection`; for "Show in folder" cases
//!    polled until non-empty, up to 1.5 s);
//! 2. `WM_COMMAND 0xA21B` to the primary, wait for the new tab to register,
//!    `Navigate2(pidl, navNoHistory)`, wait until it shows the folder;
//! 3. `IShellView::SelectItem` for the selection (verified by reading it back);
//! 4. check through UIA that the new (rightmost) tab is selected, else select it;
//! 5. zero-effect `SendInput` + `SetForegroundWindow` (restore first if the
//!    primary is minimized);
//! 6. `WM_SYSCOMMAND SC_CLOSE` to the candidate, wait until it is gone.
//!
//! Tab_Switch case: the candidate asks for a folder already open in the
//! primary's first tab; that tab is selected through UIA instead of 2–3.
//! Load case: three candidates at once, folded in creation order with one
//! new-tab command outstanding at a time.
//!
//! Reported per case: time from the candidate's first signal to (a) its first
//! navigation, (b) the folded tab showing the folder, (c) fold complete
//! (candidate closed), plus the steps, the candidate's shown time, and the
//! shell PID before/after (no Explorer crash).

#![cfg(windows)]

mod common;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::explorer_guard::{ExplorerGuard, pump_for, top_level_cabinets};
use common::fold_spike::{
    Hide, HookThread, Sampler, Uia, bring_to_foreground, browser_of_tab, browsers_of, foreground,
    hwnd, is_window, ms, ms_between, navigate, new_cabinets, open_folder_and_select, open_primary,
    pidl_of_path, post_new_tab, post_sc_close, same_pidl, select_items, selection, shell_pid,
    spawn_explorer, stats, tab_children, wait_until,
};
use esr_win::shell_util::{StaGuard, current_folder_pidl};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{IsIconic, SW_MINIMIZE, SW_RESTORE, ShowWindow};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Case {
    Plain,
    Select,
    ShOpen,
    Switch,
    Minimized,
    Load3,
}

impl Case {
    fn label(self) -> &'static str {
        match self {
            Case::Plain => "explorer.exe <dir>",
            Case::Select => "explorer.exe /select,<file>",
            Case::ShOpen => "SHOpenFolderAndSelectItems",
            Case::Switch => "Tab_Switch (folder open in primary)",
            Case::Minimized => "primary minimized",
            Case::Load3 => "3 candidates at once",
        }
    }
}

#[derive(Default)]
struct Times {
    registered: Vec<f64>,
    nav: Vec<f64>,
    sel_read: Vec<f64>,
    tab_create: Vec<f64>,
    tab_child: Vec<f64>,
    after_nav: Vec<f64>,
    tab_nav: Vec<f64>,
    shown_at: Vec<f64>,
    select_apply: Vec<f64>,
    activate: Vec<f64>,
    foreground: Vec<f64>,
    close: Vec<f64>,
    total: Vec<f64>,
    /// Fold complete counted from the candidate's first navigation (Req 14.17).
    total_nav: Vec<f64>,
    /// The same per position in a burst (0 = first created).
    total_nav_pos: [Vec<f64>; 3],
    /// Candidates whose first `SC_CLOSE` was not enough (re-posted after 1.5 s).
    close_reposts: u32,
    cand_shown: Vec<f64>,
    fg_ok: u32,
    fg_tries: u32,
    selection_ok: u32,
    selection_cases: u32,
    failures: Vec<String>,
}

struct Ctx<'a> {
    guard: &'a ExplorerGuard,
    uia: Uia,
    primary: isize,
    primary_dir: PathBuf,
    /// The candidates of the running case and the moment each was first seen
    /// showing its folder (observed while an earlier candidate is folded too).
    cands: RefCell<Vec<isize>>,
    navs: RefCell<HashMap<isize, Instant>>,
}

/// State of a window that did not close, for the log.
fn describe(h: isize) -> String {
    use windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;
    use windows::Win32::UI::WindowsAndMessaging::IsWindowVisible;
    let w = hwnd(h);
    // SAFETY: plain reads; stale HWNDs just report false.
    let (visible, enabled, iconic) = unsafe {
        (IsWindowVisible(w).as_bool(), IsWindowEnabled(w).as_bool(), IsIconic(w).as_bool())
    };
    format!(
        "alive={} visible={visible} enabled={enabled} iconic={iconic} tabs={} shellwindows-tabs={} foreground={:#x}",
        is_window(h),
        tab_children(h).len(),
        browsers_of(h).len(),
        foreground()
    )
}

impl Ctx<'_> {
    /// Notes the first navigation of every candidate that has one by now.
    fn observe(&self, pidls: &[Vec<u8>]) {
        for cand in self.cands.borrow().iter().copied() {
            if self.navs.borrow().contains_key(&cand) {
                continue;
            }
            let shown = browsers_of(cand).into_iter().next().is_some_and(|(_, b)| {
                current_folder_pidl(&b).is_ok_and(|c| pidls.iter().any(|p| same_pidl(p, &c)))
            });
            if shown {
                self.navs.borrow_mut().insert(cand, Instant::now());
            }
        }
    }

    /// `wait_until`, observing all candidates on every poll.
    fn wait(&self, pidls: &[Vec<u8>], timeout: Duration, mut f: impl FnMut() -> bool) -> Option<Duration> {
        wait_until(timeout, || {
            self.observe(pidls);
            f()
        })
    }
}

/// Folds one candidate whose first signal was at `t_sig`. Returns the
/// fold-complete time.
#[allow(clippy::too_many_arguments)]
fn fold_one(
    ctx: &Ctx,
    cand: isize,
    t_sig: Instant,
    pidls: &[Vec<u8>],
    expect_selection: bool,
    switch_index: Option<usize>,
    pos: usize,
    t: &mut Times,
) -> Result<(), String> {
    // 1. candidate's first navigation and selection
    let mut cb = None;
    ctx.wait(pidls, Duration::from_secs(8), || {
        cb = browsers_of(cand).into_iter().next().map(|x| x.1);
        cb.is_some()
    })
    .ok_or("candidate never registered in ShellWindows")?;
    let cb = cb.unwrap();
    t.registered.push(ms(t_sig.elapsed()));
    let mut dir_pidl: Option<&Vec<u8>> = None;
    ctx.wait(pidls, Duration::from_secs(8), || {
        let cur = current_folder_pidl(&cb).ok();
        dir_pidl = cur.and_then(|c| pidls.iter().find(|p| same_pidl(p, &c)));
        dir_pidl.is_some()
    })
    .ok_or("candidate never showed its folder")?;
    let dir_pidl: &[u8] = dir_pidl.unwrap();
    // The first navigation as observed (a burst's later candidates showed their
    // folder while the earlier ones were folded).
    let t_nav = ctx.navs.borrow().get(&cand).copied().unwrap_or_else(Instant::now);
    t.nav.push(ms_between(t_sig, t_nav));
    let mut sel = selection(&cb);
    if expect_selection {
        t.selection_cases += 1;
        wait_until(Duration::from_millis(1500), || {
            sel = selection(&cb);
            !sel.is_empty()
        });
        if !sel.is_empty() {
            t.selection_ok += 1;
        }
    }
    t.sel_read.push(ms(t_nav.elapsed()));

    if ctx.primary_is_minimized() {
        // SAFETY: plain call on the test's own primary window.
        unsafe {
            let _ = ShowWindow(hwnd(ctx.primary), SW_RESTORE);
        }
    }

    let tab_window: isize;
    if let Some(index) = switch_index {
        // Tab_Switch: activate the existing tab.
        let t0 = Instant::now();
        ctx.uia.select(ctx.primary, index)?;
        ctx.wait(pidls, Duration::from_secs(2), || ctx.uia.selected(ctx.primary) == Some(index))
            .ok_or("matching tab not selected within 2 s")?;
        t.activate.push(ms(t0.elapsed()));
        t.shown_at.push(ms(t_sig.elapsed()));
        t.after_nav.push(ms(t_nav.elapsed()));
        tab_window = 0;
    } else {
        // 2. new tab + navigation
        let baseline: HashSet<isize> = tab_children(ctx.primary).into_iter().collect();
        let t_req = Instant::now();
        post_new_tab(ctx.primary)?;
        let mut new_tab = None;
        let mut t_child = None;
        ctx.wait(pidls, Duration::from_secs(3), || {
            new_tab = tab_children(ctx.primary).into_iter().find(|c| !baseline.contains(c));
            if new_tab.is_some() && t_child.is_none() {
                t_child = Some(t_req.elapsed());
            }
            new_tab.is_some_and(|nt| browser_of_tab(ctx.primary, nt).is_some())
        })
        .ok_or("new tab not registered within 3 s")?;
        let nt = new_tab.unwrap();
        t.tab_create.push(ms(t_req.elapsed()));
        if let Some(c) = t_child {
            t.tab_child.push(ms(c));
        }
        let browser = browser_of_tab(ctx.primary, nt).unwrap();
        let t_n = Instant::now();
        navigate(&browser, dir_pidl)?;
        ctx.wait(pidls, Duration::from_secs(5), || {
            current_folder_pidl(&browser).is_ok_and(|p| same_pidl(&p, dir_pidl))
        })
        .ok_or("folded tab did not show the folder within 5 s")?;
        t.tab_nav.push(ms(t_n.elapsed()));
        t.shown_at.push(ms(t_sig.elapsed()));
        t.after_nav.push(ms(t_nav.elapsed()));
        // 3. selection
        if !sel.is_empty() {
            let t_s = Instant::now();
            let mut applied = false;
            wait_until(Duration::from_secs(2), || {
                if select_items(&browser, &sel).is_ok() {
                    let now = selection(&browser);
                    applied = now.len() == sel.len()
                        && now.iter().zip(sel.iter()).all(|(a, b)| same_pidl(a, b));
                }
                applied
            });
            if !applied {
                t.failures.push("selection not applied to the folded tab".into());
            }
            t.select_apply.push(ms(t_s.elapsed()));
        }
        // 4. active tab: the new tab should already be selected (R2).
        let t_a = Instant::now();
        let n = ctx.uia.items(ctx.primary).len();
        let ok = wait_until(Duration::from_millis(1500), || {
            ctx.uia.selected(ctx.primary).is_some_and(|s| s + 1 == ctx.uia.items(ctx.primary).len())
        });
        if ok.is_none() {
            t.failures.push(format!(
                "new tab not the selected (rightmost) UIA item: selected {:?} of {n}",
                ctx.uia.selected(ctx.primary)
            ));
        }
        t.activate.push(ms(t_a.elapsed()));
        tab_window = nt;
    }
    let _ = tab_window;
    // 5. foreground
    let t_f = Instant::now();
    t.fg_tries += 1;
    if bring_to_foreground(ctx.primary) {
        t.fg_ok += 1;
    } else {
        t.failures.push(format!("foreground refused (foreground is {:#x})", foreground()));
    }
    t.foreground.push(ms(t_f.elapsed()));
    // 6. close the candidate
    let t_c = Instant::now();
    post_sc_close(cand);
    if ctx.wait(pidls, Duration::from_millis(1500), || !is_window(cand)).is_none() {
        // The product posts it once more; record that it was needed.
        t.close_reposts += 1;
        println!("[f2] candidate {cand:#x} still open 1.5 s after SC_CLOSE: {}; posting again", describe(cand));
        post_sc_close(cand);
    }
    ctx.wait(pidls, Duration::from_secs(3), || !is_window(cand))
        .ok_or_else(|| format!("candidate did not close: {}", describe(cand)))?;
    t.close.push(ms(t_c.elapsed()));
    t.total.push(ms(t_sig.elapsed()));
    let from_nav = ms(t_nav.elapsed());
    t.total_nav.push(from_nav);
    if let Some(v) = t.total_nav_pos.get_mut(pos) {
        v.push(from_nav);
    }
    Ok(())
}

impl Ctx<'_> {
    fn primary_is_minimized(&self) -> bool {
        // SAFETY: plain read.
        unsafe { IsIconic(hwnd(self.primary)) }.as_bool()
    }
}

fn run_case(ctx: &Ctx, case: Case, run: usize, t: &mut Times) -> Result<(), String> {
    let n = if case == Case::Load3 { 3 } else { 1 };
    let dirs: Vec<PathBuf> = (0..n)
        .map(|i| match case {
            Case::Switch => ctx.primary_dir.clone(),
            _ => ctx.guard.dir(&format!("{case:?}_{run}_{i}")),
        })
        .collect();
    let files: Vec<PathBuf> = dirs
        .iter()
        .map(|d| {
            let f = d.join("pick-me.txt");
            let _ = std::fs::write(&f, b"x");
            f
        })
        .collect();
    let pidls: Vec<Vec<u8>> = dirs.iter().map(|d| pidl_of_path(d)).collect::<Result<_, _>>()?;
    if case == Case::Minimized {
        // SAFETY: plain call on the test's own primary window.
        unsafe {
            let _ = ShowWindow(hwnd(ctx.primary), SW_MINIMIZE);
        }
        pump_for(Duration::from_millis(400));
    }
    let pre: HashSet<isize> = top_level_cabinets().into_iter().collect();
    let hook = HookThread::start(Hide::AlphaAtCreate, pre.clone());
    let sampler = Sampler::start(pre.clone());
    std::thread::sleep(Duration::from_millis(50));
    let t_launch = Instant::now();
    let mut launcher = None;
    for (d, f) in dirs.iter().zip(files.iter()) {
        match case {
            Case::Select => spawn_explorer(&format!("/select,\"{}\"", f.display()))?,
            Case::ShOpen => launcher = Some(open_folder_and_select(d, f)),
            _ => spawn_explorer(&format!("\"{}\"", d.display()))?,
        }
    }
    // Candidates in creation (first-signal) order.
    let ok = wait_until(Duration::from_secs(8), || hook.created().len() >= n);
    if ok.is_none() {
        return Err(format!("only {} of {n} candidates seen", hook.created().len()));
    }
    let mut created = hook.created();
    created.sort_by_key(|c| c.t_create);
    for c in &created {
        ctx.guard.adopt(hwnd(c.hwnd));
    }
    *ctx.cands.borrow_mut() = created.iter().take(n).map(|c| c.hwnd).collect();
    ctx.navs.borrow_mut().clear();
    let mut result = Ok(());
    for (pos, c) in created.iter().take(n).enumerate() {
        let expect_sel = matches!(case, Case::Select | Case::ShOpen);
        let switch = (case == Case::Switch).then_some(0);
        if let Err(e) = fold_one(ctx, c.hwnd, c.t_create, &pidls, expect_sel, switch, pos, t) {
            result = Err(e);
            break;
        }
    }
    ctx.cands.borrow_mut().clear();
    if let Some(l) = launcher {
        let _ = l.join();
    }
    pump_for(Duration::from_millis(200));
    let tracks = sampler.stop();
    let _ = hook.stop();
    for c in created.iter().take(n) {
        if let Some(tr) = tracks.get(&c.hwnd) {
            t.cand_shown.push(tr.shown_ms(Instant::now()));
        }
    }
    let _ = t_launch;
    // Anything still open from this case (a failed fold) is closed.
    for h in new_cabinets(&pre) {
        ctx.guard.adopt(hwnd(h));
        ctx.guard.close(hwnd(h));
    }
    let _ = ms_between(t_launch, Instant::now());
    result
}

#[test]
#[ignore = "spike: opens, folds and closes File Explorer windows on the interactive desktop"]
fn spike_f2_fold_into_primary() {
    // SAFETY: plain call; failure (already set) is fine.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let _sta = StaGuard::init().expect("STA");
    let guard = ExplorerGuard::with_stable_root("f2", "f2_fold");
    let shell_before = shell_pid();
    let primary_dir = guard.dir("primary");
    let primary = open_primary(&guard, &primary_dir).expect("primary window");
    let ctx = Ctx {
        guard: &guard,
        uia: Uia::new().expect("UIA"),
        primary,
        primary_dir,
        cands: RefCell::new(Vec::new()),
        navs: RefCell::new(HashMap::new()),
    };
    println!("[f2] primary {primary:#x}, shell pid {shell_before}");
    let enum_ms: Vec<f64> = (0..3)
        .map(|_| {
            let t = Instant::now();
            let _ = browsers_of(primary);
            ms(t.elapsed())
        })
        .collect();
    println!("[f2] one ShellWindows enumeration (the polls below): {}", stats(&enum_ms));

    let plan = [
        (Case::Plain, 5),
        (Case::Select, 3),
        (Case::ShOpen, 3),
        (Case::Switch, 3),
        (Case::Minimized, 2),
        (Case::Load3, 2),
    ];
    let mut summary = Vec::new();
    let only = std::env::var("ESR_F2_ONLY").ok();
    for (case, runs) in plan {
        if only.as_deref().is_some_and(|o| !format!("{case:?}").eq_ignore_ascii_case(o)) {
            continue;
        }
        let mut t = Times::default();
        for run in 0..runs {
            if let Err(e) = run_case(&ctx, case, run, &mut t) {
                t.failures.push(format!("run {run}: {e}"));
            }
            pump_for(Duration::from_millis(500));
        }
        let lines = [
            format!("[f2] {}", case.label()),
            format!("       candidate in ShellWindows after first signal: {}", stats(&t.registered)),
            format!("       candidate first nav after first signal: {}", stats(&t.nav)),
            format!("       selection read after first nav: {} (non-empty {}/{})", stats(&t.sel_read), t.selection_ok, t.selection_cases),
            format!("       new tab child window after 0xA21B: {}", stats(&t.tab_child)),
            format!("       new tab registered (incl. ShellWindows poll) after 0xA21B: {}", stats(&t.tab_create)),
            format!("       folded tab navigation: {}", stats(&t.tab_nav)),
            format!("       folded/matching tab shows folder, after first signal: {}", stats(&t.shown_at)),
            format!("       folded/matching tab shows folder, after candidate's first nav: {}", stats(&t.after_nav)),
            format!("       selection applied: {}", stats(&t.select_apply)),
            format!("       activation check / Tab_Switch select: {}", stats(&t.activate)),
            format!("       foreground: {} ({} of {} granted)", stats(&t.foreground), t.fg_ok, t.fg_tries),
            format!("       candidate close: {}", stats(&t.close)),
            format!("       FOLD COMPLETE after first signal: {}", stats(&t.total)),
            format!("       FOLD COMPLETE after candidate's first nav (Req 14.17): {}", stats(&t.total_nav)),
            format!("       ... by position in the burst: 1st {} | 2nd {} | 3rd {}", stats(&t.total_nav_pos[0]), stats(&t.total_nav_pos[1]), stats(&t.total_nav_pos[2])),
            format!("       SC_CLOSE re-posted after 1.5 s: {}", t.close_reposts),
            format!("       candidate shown: {}", stats(&t.cand_shown)),
        ];
        for l in &lines {
            println!("{l}");
        }
        for f in &t.failures {
            println!("       FAIL: {f}");
        }
        summary.extend(lines);
        summary.extend(t.failures.iter().map(|f| format!("       FAIL: {f}")));
    }
    println!("[f2] primary tabs at the end (UIA): {}", ctx.uia.names(primary).len());
    let shell_after = shell_pid();
    println!(
        "[f2] shell pid before {shell_before}, after {shell_after}{}",
        if shell_before == shell_after { " (no Explorer restart)" } else { " (EXPLORER RESTARTED)" }
    );
    println!("\n[f2] summary:\n{}", summary.join("\n"));
    drop(ctx);
    guard.finish().expect("cleanup");
}
