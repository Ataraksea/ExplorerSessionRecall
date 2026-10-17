//! Unit tests for the `FoldingEngine` and the `FOLD` log tokens (task 20.16),
//! through the public API only.
//!
//! The engine is driven with hand-made events, the way the `Engine` and the
//! adapters would drive it, and every `FOLD` entry it emits is formatted with
//! `format_entry`, so the tokens are checked as they appear in the
//! Activity_Log. Covered: each classification rule, the hide command, the
//! drag-out window at its edges, every timer (early and on time), one example
//! of every outcome and of every `reason`, `matching`, `switch-reason`, `into`
//! and `passed` token, and the late-tab close.
//!
//! Requirements 14.3, 14.4, 14.14, 14.30.

use std::borrow::Cow;
use std::collections::BTreeSet;

use esr_core::engine::{
    Command, DesktopView, Event, FoldProbeId, ItemId, Millis, ObservedLocation, OpId, ShellError, TabKey,
    TabObservation, TimerId, TimerOwner, WinKey, WindowObservation,
};
use esr_core::fold::{
    ExclusionReason, FirstSignalEnv, FirstSignalVerdict, FoldStep, FoldWindow, FoldingEngine, Released, DRAG_OUT_WINDOW_MS,
    FOLD_ACTIVATE_MS, FOLD_CLOSE_RETRY_MS, FOLD_FIRST_NAV_MS, FOLD_SWITCH_MS, FOLD_TAB_CREATE_MS, FOLD_TAB_SHOW_MS,
    LATE_TAB_MS,
};
use esr_core::logfmt::entry::{format_entry, LogEntry};
use esr_core::model::{
    DisplayRef, LocationId, LocationKey, LocationKind, Placement, Rect, ShowState, Timestamp, Volume,
};
use esr_core::plan::placement::FinalPlacement;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn w(n: u64) -> WinKey {
    WinKey { hwnd: n, seq: n }
}

fn tk(n: u64) -> TabKey {
    TabKey { hwnd: n, seq: n }
}

/// The Location `n`; its parsing name is the decimal number.
fn loc(n: u16) -> LocationId {
    let name = format!("{n}");
    LocationId {
        case_sensitive: false,
        kind: LocationKind::FileSystem(Volume::Fixed),
        parsing_name: name.as_str().into(),
        display_name: name.as_str().into(),
        pidl: vec![],
        search: None,
    }
}

fn owned_key(l: &LocationId) -> LocationKey<'static> {
    match l.fold_key() {
        LocationKey::Plain { kind, parsing_name } => {
            LocationKey::Plain { kind, parsing_name: Cow::Owned(parsing_name.into_owned()) }
        }
        LocationKey::Search { .. } => unreachable!("test locations are plain"),
    }
}

/// A window whose tabs show the numbered Locations; the first tab is active.
fn fwin(n: u64, tabs: &[u16]) -> FoldWindow<'static> {
    FoldWindow {
        win: w(n),
        tabs: tabs.iter().map(|&t| Some(owned_key(&loc(t)))).collect(),
        active: if tabs.is_empty() { None } else { Some(0) },
        closing: false,
    }
}

fn closing(mut window: FoldWindow<'static>) -> FoldWindow<'static> {
    window.closing = true;
    window
}

/// The tab the tracker has at `index` of `win`.
fn tab_of(win: WinKey, index: usize) -> Option<TabKey> {
    Some(tk(100 * win.hwnd + index as u64))
}

fn not_min(_: WinKey) -> Option<FinalPlacement> {
    None
}

fn minimized_placement() -> FinalPlacement {
    FinalPlacement {
        state: ShowState::Maximized,
        restored: Rect { left: 10, top: 20, right: 300, bottom: 400 },
        monitor: Rect::default(),
        work: Rect::default(),
    }
}

fn env(tracked: &[WinKey]) -> FirstSignalEnv<'_> {
    FirstSignalEnv { tracked, restoration_active: false, builtin_restore: false }
}

fn engine(opening: &[u64]) -> FoldingEngine {
    let mut e = FoldingEngine::new(true);
    for &n in opening {
        e.opening_mut().append(w(n));
    }
    e
}

fn appear(
    e: &mut FoldingEngine,
    at: u64,
    cand: u64,
    tracked: &[WinKey],
    foreground: Option<WinKey>,
) -> esr_core::fold::FirstSignalOutcome {
    e.window_appeared(Millis(at), w(cand), false, DesktopView::Unknown, foreground, &env(tracked))
}

fn scan(n: u64, tabs: u64) -> WindowObservation {
    WindowObservation {
        win: w(n),
        pid: 1,
        tabs: (0..tabs).map(|t| TabObservation { tab: tk(t), location: ObservedLocation::NotNavigated }).collect(),
        active: None,
        strip: None,
        placement: Placement {
            state: ShowState::Normal,
            restored: Rect::default(),
            display: DisplayRef { device: String::new(), monitor: Rect::default() },
        },
    }
}

/// A one-tab scan that shows Location `loc_n`.
fn scan_showing(n: u64, loc_n: u16) -> WindowObservation {
    let mut obs = scan(n, 1);
    obs.tabs[0].location = ObservedLocation::Known(loc(loc_n));
    obs
}

const TS: Timestamp = Timestamp { year: 2025, month: 1, day: 31, hour: 9, minute: 15, second: 2, millis: 123, offset_minutes: 0 };

/// A log entry as written to the Activity_Log, without its timestamp.
fn line_of(entry: &LogEntry) -> String {
    let line = format_entry(&TS, entry);
    assert!(!line.contains(['\r', '\n']), "{line:?}");
    line.splitn(4, ' ').nth(3).expect("a timestamp and an event").to_owned()
}

/// The lines of the `FOLD` entries among `cmds`.
fn fold_lines(cmds: &[Command]) -> Vec<String> {
    cmds.iter()
        .filter_map(|c| match c {
            Command::AppendLog(entry @ LogEntry::Fold { .. }) => Some(line_of(entry)),
            _ => None,
        })
        .collect()
}

fn one_fold_line(step: &FoldStep) -> String {
    let mut lines = fold_lines(&step.commands);
    assert_eq!(lines.len(), 1, "expected one FOLD entry in {:?}", step.commands);
    lines.remove(0)
}

fn error_lines(cmds: &[Command]) -> Vec<String> {
    cmds.iter()
        .filter_map(|c| match c {
            Command::AppendLog(entry @ LogEntry::Error { .. }) => Some(line_of(entry)),
            _ => None,
        })
        .collect()
}

fn probe_in(cmds: &[Command]) -> FoldProbeId {
    let probes: Vec<FoldProbeId> = cmds
        .iter()
        .filter_map(|c| match c {
            Command::ProbeFoldContext { probe, .. } => Some(*probe),
            _ => None,
        })
        .collect();
    match probes.as_slice() {
        [probe] => *probe,
        other => panic!("expected one probe in {cmds:?}, got {other:?}"),
    }
}

fn create_op(cmds: &[Command]) -> OpId {
    cmds.iter()
        .find_map(|c| match c {
            Command::CreateTab { op, .. } => Some(*op),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected a CreateTab in {cmds:?}"))
}

fn create_target(cmds: &[Command]) -> WinKey {
    cmds.iter()
        .find_map(|c| match c {
            Command::CreateTab { win, .. } => Some(*win),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected a CreateTab in {cmds:?}"))
}

fn activate_op(cmds: &[Command]) -> OpId {
    cmds.iter()
        .find_map(|c| match c {
            Command::ActivateTab { op, .. } => Some(*op),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected an ActivateTab in {cmds:?}"))
}

/// The only `SetTimer` among `cmds`: its id and due time.
fn only_timer(cmds: &[Command]) -> (TimerId, Millis) {
    let timers: Vec<(TimerId, Millis)> = cmds
        .iter()
        .filter_map(|c| match c {
            Command::SetTimer { id, at } => Some((*id, *at)),
            _ => None,
        })
        .collect();
    match timers.as_slice() {
        [timer] => *timer,
        other => panic!("expected one timer in {cmds:?}, got {other:?}"),
    }
}

/// Fires a timer one millisecond before it is due, which must change nothing,
/// and then on time.
fn fire_at(e: &mut FoldingEngine, (id, due): (TimerId, Millis)) -> FoldStep {
    assert_eq!(e.timer(Millis(due.0 - 1), id, &not_min), FoldStep::default(), "a timer fired early must do nothing");
    e.timer(due, id, &not_min)
}

const CTX: u64 = 1_000;

fn context(e: &mut FoldingEngine, now: u64, probe: FoldProbeId, windows: &[FoldWindow<'_>], unresponsive: &[u64]) -> FoldStep {
    let set: BTreeSet<WinKey> = unresponsive.iter().map(|&n| w(n)).collect();
    e.fold_context(Millis(now), probe, DesktopView::Unknown, &set, windows, &tab_of)
}

/// Candidate `cand` appears at 0 with `foreground` in front, announces
/// Location `loc_n` with `items`, and its fold context (at [`CTX`]) shows
/// `windows`. Returns the step of the context.
#[allow(clippy::too_many_arguments)]
fn begin(
    e: &mut FoldingEngine,
    cand: u64,
    loc_n: u16,
    items: Vec<ItemId>,
    tracked: &[WinKey],
    foreground: Option<WinKey>,
    windows: &[FoldWindow<'_>],
    unresponsive: &[u64],
) -> FoldStep {
    let out = appear(e, 0, cand, tracked, foreground);
    assert_eq!(out.verdict, FirstSignalVerdict::Candidate);
    let step = e.initial_location_known(w(cand), loc(loc_n), items).expect("an owned candidate");
    let probe = probe_in(&step.commands);
    context(e, CTX, probe, windows, unresponsive)
}

/// Drives the Folded_Tab `tab` of candidate `cand` in `target`, from its
/// `TabStarted` at `t0` to the candidate's `WindowDestroyed`, and returns the
/// step of that destruction.
fn finish_fold(e: &mut FoldingEngine, create: OpId, target: u64, cand: u64, tab: u64, loc_n: u16, t0: u64) -> FoldStep {
    let step = e.tab_started(Millis(t0), create, tk(tab));
    assert_eq!(only_timer(&step.commands).1, Millis(t0 + FOLD_TAB_SHOW_MS));
    let step = e.tab_created(Millis(t0 + 10), w(target), tk(tab));
    assert!(matches!(step.commands.as_slice(), [Command::Navigate { .. }]), "{:?}", step.commands);
    let step = e.navigation_completed(Millis(t0 + 20), tk(tab), &loc(loc_n), &not_min);
    let step = e.activation_confirmed(Millis(t0 + 30), activate_op(&step.commands), &not_min);
    assert!(step.commands.contains(&Command::Focus(w(target))), "{:?}", step.commands);
    assert!(step.commands.contains(&Command::CloseWindow { win: w(cand) }), "{:?}", step.commands);
    let done = e.window_destroyed(Millis(t0 + 40), w(cand)).expect("a candidate that was closed");
    assert!(done.commands.contains(&Command::RescanWindow(w(target))), "{:?}", done.commands);
    assert!(!e.owns(w(cand)));
    done
}

/// A candidate that left the tracker alone: revealed and handed over.
fn assert_left_open(step: &FoldStep, cand: u64) {
    assert!(step.commands.contains(&Command::Reveal { win: w(cand) }), "{:?}", step.commands);
    assert_eq!(step.released.len(), 1);
    assert_eq!(step.released[0].win, w(cand));
}

// ---------------------------------------------------------------------------
// Classification at the first signal (Req 14.3, 14.4)
// ---------------------------------------------------------------------------

/// Classifies window `n` at time 0 with the given conditions.
fn classify(
    e: &mut FoldingEngine,
    n: u64,
    bypass: bool,
    restoration: bool,
    builtin: bool,
    tracked: &[WinKey],
) -> esr_core::fold::FirstSignalOutcome {
    let env = FirstSignalEnv { tracked, restoration_active: restoration, builtin_restore: builtin };
    e.window_appeared(Millis(0), w(n), bypass, DesktopView::Unknown, None, &env)
}

#[test]
fn each_classification_rule_excludes_the_window_alone() {
    let tracked = [w(1)];
    // (name, bypass, restoration, builtin, tracked, expected reason)
    #[allow(clippy::type_complexity)]
    let cases: [(&str, bool, bool, bool, &[WinKey], ExclusionReason); 4] = [
        ("restoration", false, true, false, &tracked[..], ExclusionReason::RestorationActive),
        ("builtin", false, false, true, &tracked[..], ExclusionReason::BuiltinRestore),
        ("bypass", true, false, false, &tracked[..], ExclusionReason::BypassKeys),
        ("no window", false, false, false, &tracked[..0], ExclusionReason::NoWindowOnDesktop),
    ];
    for (name, bypass, restoration, builtin, tracked, reason) in cases {
        let mut e = FoldingEngine::new(true);
        let out = classify(&mut e, 2, bypass, restoration, builtin, tracked);
        assert_eq!(out.verdict, FirstSignalVerdict::Excluded(reason), "{name}");
        assert!(out.commands.is_empty(), "{name}: an excluded window is neither hidden nor timed");
        assert!(e.is_excluded(w(2)) && !e.owns(w(2)), "{name}");
        assert!(e.opening().contains(w(2)), "{name}: an excluded window is in the Opening_Order");
        assert_eq!(e.candidates_in_progress(), 0, "{name}");
    }

    let mut off = FoldingEngine::new(false);
    assert!(!off.enabled());
    let out = classify(&mut off, 2, false, false, false, &tracked);
    assert_eq!(out.verdict, FirstSignalVerdict::Excluded(ExclusionReason::FoldingDisabled));

    // A window none of the rules touches is a candidate.
    let mut e = FoldingEngine::new(true);
    assert_eq!(classify(&mut e, 2, false, false, false, &tracked).verdict, FirstSignalVerdict::Candidate);
}

#[test]
fn the_classification_rules_apply_in_the_design_order() {
    let tracked = [w(1)];
    let verdict = |e: &mut FoldingEngine, n, restoration, builtin, tracked: &[WinKey]| {
        classify(e, n, true, restoration, builtin, tracked).verdict
    };

    // Folding off wins over every other rule.
    let mut e = FoldingEngine::new(false);
    assert_eq!(verdict(&mut e, 2, true, true, &tracked[..0]), FirstSignalVerdict::Excluded(ExclusionReason::FoldingDisabled));
    // Then "no tracked window on the desktop", a restoration, the
    // Builtin_Restore_Option, and the bypass keys.
    let mut e = FoldingEngine::new(true);
    assert_eq!(verdict(&mut e, 2, true, true, &tracked[..0]), FirstSignalVerdict::Excluded(ExclusionReason::NoWindowOnDesktop));
    assert_eq!(verdict(&mut e, 3, true, true, &tracked), FirstSignalVerdict::Excluded(ExclusionReason::RestorationActive));
    assert_eq!(verdict(&mut e, 4, false, true, &tracked), FirstSignalVerdict::Excluded(ExclusionReason::BuiltinRestore));
    assert_eq!(verdict(&mut e, 5, false, false, &tracked), FirstSignalVerdict::Excluded(ExclusionReason::BypassKeys));
}
#[test]
fn only_tracked_windows_on_the_candidates_desktop_count() {
    let tracked = [w(1)];
    let mut e = FoldingEngine::new(true);
    let elsewhere = DesktopView::Known([w(9)].into_iter().collect());
    let out = e.window_appeared(Millis(0), w(2), false, elsewhere, None, &env(&tracked));
    assert_eq!(out.verdict, FirstSignalVerdict::Excluded(ExclusionReason::NoWindowOnDesktop));

    let here = DesktopView::Known([w(1), w(9)].into_iter().collect());
    let out = e.window_appeared(Millis(0), w(3), false, here, None, &env(&tracked));
    assert_eq!(out.verdict, FirstSignalVerdict::Candidate);
}

#[test]
fn a_known_window_is_not_classified_again() {
    let mut e = FoldingEngine::new(true);
    assert_eq!(appear(&mut e, 0, 2, &[w(1)], None).verdict, FirstSignalVerdict::Candidate);
    let again = appear(&mut e, 5, 2, &[w(1)], None);
    assert_eq!(again.verdict, FirstSignalVerdict::AlreadyKnown);
    assert!(again.commands.is_empty());
    assert_eq!(e.candidates_in_progress(), 1);
}

#[test]
fn a_candidate_is_hidden_in_the_same_call_and_timed() {
    let mut e = FoldingEngine::new(true);
    let out = appear(&mut e, 1_000, 2, &[w(1)], Some(w(1)));
    assert_eq!(out.verdict, FirstSignalVerdict::Candidate);
    let [Command::HideWindow { win }, Command::SetTimer { id, at }] = out.commands.as_slice() else {
        panic!("expected HideWindow and a timer, got {:?}", out.commands);
    };
    assert_eq!(*win, w(2));
    assert_eq!(id.owner, TimerOwner::Fold);
    assert_eq!(*at, Millis(1_000 + FOLD_FIRST_NAV_MS));
    assert!(e.owns(w(2)));
    assert!(!e.is_excluded(w(2)));
    // The candidate's position is reserved, but it is not part of the order.
    assert!(!e.opening().contains(w(2)));
}

#[test]
fn a_failed_hide_is_logged_and_the_fold_goes_on() {
    let mut e = engine(&[1]);
    appear(&mut e, 0, 2, &[w(1)], None);
    let error = ShellError { code: 5, message: "access denied".into() };
    let step = e.hide_failed(w(2), &error);
    let lines = error_lines(&step.commands);
    assert_eq!(lines.len(), 1, "{:?}", step.commands);
    assert!(lines[0].starts_with("ERROR operation=fold-hide "), "{}", lines[0]);
    assert!(step.released.is_empty());
    assert!(e.owns(w(2)), "the candidate stays under the FoldingEngine's control");
    // A window that is no candidate is ignored.
    assert_eq!(e.hide_failed(w(9), &error), FoldStep::default());

    // The fold itself still completes.
    let probe = probe_in(&e.initial_location_known(w(2), loc(5), vec![]).unwrap().commands);
    let step = context(&mut e, CTX, probe, &[fwin(1, &[0])], &[]);
    assert_eq!(create_target(&step.commands), w(1));
}

// ---------------------------------------------------------------------------
// Drag-out detection at the edges of its window (Req 14.3 b)
// ---------------------------------------------------------------------------

#[test]
fn the_drag_out_window_is_100_ms_before_to_300_ms_after() {
    assert_eq!(DRAG_OUT_WINDOW_MS, (-100, 300));
}

#[test]
fn a_detach_up_to_300_ms_after_the_first_signal_makes_a_drag_out() {
    let mut e = FoldingEngine::new(true);
    appear(&mut e, 1_000, 2, &[w(1)], None);
    e.window_scanned(&scan(2, 1));
    let step = e.tab_detached(Millis(1_300), w(1));
    assert_eq!(step.commands, vec![Command::Reveal { win: w(2) }]);
    assert_eq!(step.released, vec![Released { win: w(2), buffered: vec![Event::WindowScanned(scan(2, 1))] }]);
    assert!(!e.owns(w(2)));
    assert!(e.is_excluded(w(2)));
    assert!(e.opening().contains(w(2)));
    assert_eq!(e.candidates_in_progress(), 0);
}

#[test]
fn a_detach_301_ms_after_the_first_signal_is_not_a_drag_out() {
    let mut e = FoldingEngine::new(true);
    appear(&mut e, 1_000, 2, &[w(1)], None);
    assert_eq!(e.tab_detached(Millis(1_301), w(1)), FoldStep::default());
    assert!(e.owns(w(2)));
    assert!(!e.is_excluded(w(2)));
}

#[test]
fn a_detach_100_ms_before_the_first_signal_makes_a_drag_out() {
    let mut e = FoldingEngine::new(true);
    assert_eq!(e.tab_detached(Millis(1_000), w(1)), FoldStep::default());
    let out = appear(&mut e, 1_100, 2, &[w(1)], None);
    assert_eq!(out.verdict, FirstSignalVerdict::Excluded(ExclusionReason::DragOut));
    assert!(out.commands.is_empty(), "the dragged-out window was never hidden");
    assert!(e.is_excluded(w(2)) && !e.owns(w(2)));
}

#[test]
fn a_detach_101_ms_before_the_first_signal_is_not_a_drag_out() {
    let mut e = FoldingEngine::new(true);
    e.tab_detached(Millis(1_000), w(1));
    let out = appear(&mut e, 1_101, 2, &[w(1)], None);
    assert_eq!(out.verdict, FirstSignalVerdict::Candidate);
}

#[test]
fn a_detach_from_an_untracked_window_is_no_drag_out() {
    let mut e = FoldingEngine::new(true);
    e.tab_detached(Millis(1_000), w(9));
    assert_eq!(appear(&mut e, 1_050, 2, &[w(1)], None).verdict, FirstSignalVerdict::Candidate);
}

#[test]
fn a_detach_after_the_candidate_left_its_hidden_phase_is_no_drag_out() {
    let mut e = FoldingEngine::new(true);
    appear(&mut e, 1_000, 2, &[w(1)], None);
    e.initial_location_known(w(2), loc(5), vec![]).unwrap();
    assert_eq!(e.tab_detached(Millis(1_100), w(1)), FoldStep::default());
    assert!(e.owns(w(2)));
}

#[test]
fn a_first_scan_with_several_tabs_excludes_the_window_but_a_later_one_does_not() {
    let mut e = FoldingEngine::new(true);
    appear(&mut e, 0, 2, &[w(1)], None);
    assert!(e.window_scanned(&scan(9, 2)).is_none(), "a scan of another window is not for the FoldingEngine");
    assert_eq!(e.window_scanned(&scan(2, 1)), Some(FoldStep::default()));
    // Only the first scan decides (Req 14.2).
    assert_eq!(e.window_scanned(&scan(2, 3)), Some(FoldStep::default()));
    assert!(e.owns(w(2)));

    appear(&mut e, 10, 3, &[w(1)], None);
    let step = e.window_scanned(&scan(3, 2)).expect("a candidate's scan");
    assert_eq!(step.commands, vec![Command::Reveal { win: w(3) }]);
    assert_eq!(step.released.len(), 1);
    assert!(e.is_excluded(w(3)) && !e.owns(w(3)));
}

// ---------------------------------------------------------------------------
// The fold context and the timers (Req 14.13, 14.14, 14.23, 14.24)
// ---------------------------------------------------------------------------

#[test]
fn a_first_navigation_that_takes_8_s_leaves_the_candidate_open() {
    let mut e = FoldingEngine::new(true);
    let out = appear(&mut e, 1_000, 2, &[w(1)], None);
    let timer = only_timer(&out.commands);
    assert_eq!(timer.1, Millis(1_000 + FOLD_FIRST_NAV_MS));
    e.window_scanned(&scan_showing(2, 5));
    let step = fire_at(&mut e, timer);
    assert_left_open(&step, 2);
    assert_eq!(step.released[0].buffered, vec![Event::WindowScanned(scan_showing(2, 5))]);
    assert_eq!(one_fold_line(&step), "FOLD location=5 outcome=not-folded reason=first-nav-timeout-8s");
    assert!(e.is_excluded(w(2)) && e.opening().contains(w(2)));
    // A timer fires once.
    assert_eq!(e.timer(Millis(timer.1 .0 + 1), timer.0, &not_min), FoldStep::default());
}

#[test]
fn a_first_navigation_timeout_without_any_location_writes_no_fold_entry() {
    let mut e = FoldingEngine::new(true);
    let timer = only_timer(&appear(&mut e, 0, 2, &[w(1)], None).commands);
    let step = fire_at(&mut e, timer);
    assert_left_open(&step, 2);
    assert!(fold_lines(&step.commands).is_empty());
}

#[test]
fn the_first_navigation_timer_is_stale_once_the_location_is_known() {
    let mut e = engine(&[1]);
    let timer = only_timer(&appear(&mut e, 0, 2, &[w(1)], None).commands);
    e.initial_location_known(w(2), loc(5), vec![]).unwrap();
    assert_eq!(e.timer(timer.1, timer.0, &not_min), FoldStep::default());
    assert!(e.owns(w(2)));
}

#[test]
fn timers_of_other_owners_and_unknown_candidates_do_nothing() {
    let mut e = engine(&[1]);
    let timer = only_timer(&appear(&mut e, 0, 2, &[w(1)], None).commands);
    let tracker = TimerId { owner: TimerOwner::Tracker, seq: timer.0.seq };
    assert_eq!(e.timer(Millis(100_000), tracker, &not_min), FoldStep::default());
    let unknown = TimerId { owner: TimerOwner::Fold, seq: timer.0.seq + 77 };
    assert_eq!(e.timer(Millis(100_000), unknown, &not_min), FoldStep::default());
}

// ---- FOLD outcomes: folded-primary, folded-fallback ----

#[test]
fn a_fold_into_the_primary_window_logs_folded_primary() {
    let mut e = engine(&[1]);
    let items = vec![ItemId(vec![7])];
    let step = begin(&mut e, 2, 5, items.clone(), &[w(1)], None, &[fwin(1, &[0, 1])], &[]);
    // CreateTab with its 1 s timer.
    assert_eq!(create_target(&step.commands), w(1));
    assert_eq!(only_timer(&step.commands).1, Millis(CTX + FOLD_TAB_CREATE_MS));
    let create = create_op(&step.commands);

    e.tab_started(Millis(1_040), create, tk(10));
    e.tab_created(Millis(1_300), w(1), tk(10));
    // The new tab's own start navigation is not the Location.
    assert_eq!(e.navigation_completed(Millis(1_310), tk(10), &loc(0), &not_min), FoldStep::default());
    let step = e.navigation_completed(Millis(1_400), tk(10), &loc(5), &not_min);
    let [Command::SelectItems { tab, items: got, .. }, Command::ActivateTab { op, win, index }, Command::SetTimer { at, .. }] =
        step.commands.as_slice()
    else {
        panic!("expected select, activate and a timer, got {:?}", step.commands);
    };
    assert_eq!((*tab, got, *win, *index), (tk(10), &items, w(1), 2));
    assert_eq!(*at, Millis(1_400 + FOLD_ACTIVATE_MS));
    let step = e.activation_confirmed(Millis(1_450), *op, &not_min);
    assert!(matches!(
        step.commands.as_slice(),
        [Command::Focus(f), Command::CloseWindow { win: c }, Command::SetTimer { .. }] if *f == w(1) && *c == w(2)
    ));
    let done = e.window_destroyed(Millis(1_500), w(2)).unwrap();
    assert_eq!(one_fold_line(&done), "FOLD location=5 outcome=folded-primary");
    assert_eq!(done.commands.last(), Some(&Command::RescanWindow(w(1))));
    assert!(!e.opening().contains(w(2)), "a folded candidate leaves the Opening_Order");
}

#[test]
fn a_minimized_target_is_restored_before_the_focus_moves() {
    let mut e = engine(&[1]);
    let create = create_op(&begin(&mut e, 2, 5, vec![], &[w(1)], None, &[fwin(1, &[0])], &[]).commands);
    e.tab_started(Millis(1_040), create, tk(10));
    e.tab_created(Millis(1_300), w(1), tk(10));
    let minimized = |win: WinKey| (win == w(1)).then(minimized_placement);
    let step = e.navigation_completed(Millis(1_400), tk(10), &loc(5), &minimized);
    let step = e.activation_confirmed(Millis(1_450), activate_op(&step.commands), &minimized);
    assert!(matches!(
        step.commands.as_slice(),
        [
            Command::ApplyPlacement { win: a, .. },
            Command::Focus(f),
            Command::CloseWindow { win: c },
            Command::SetTimer { .. },
        ] if *a == w(1) && *f == w(1) && *c == w(2)
    ));
}

#[test]
fn an_unresponsive_primary_window_logs_folded_fallback_with_its_reason() {
    let mut e = engine(&[1, 3]);
    let windows = [fwin(1, &[0]), fwin(3, &[0])];
    let step = begin(&mut e, 2, 5, vec![], &[w(1), w(3)], None, &windows, &[1]);
    assert_eq!(create_target(&step.commands), w(3));
    let done = finish_fold(&mut e, create_op(&step.commands), 3, 2, 30, 5, 1_040);
    assert_eq!(one_fold_line(&done), "FOLD location=5 outcome=folded-fallback reason=primary-unresponsive");
}

#[test]
fn a_closing_primary_window_logs_folded_fallback_with_its_reason() {
    let mut e = engine(&[1, 3]);
    let windows = [closing(fwin(1, &[0])), fwin(3, &[0])];
    let step = begin(&mut e, 2, 5, vec![], &[w(1), w(3)], None, &windows, &[]);
    assert_eq!(create_target(&step.commands), w(3));
    let done = finish_fold(&mut e, create_op(&step.commands), 3, 2, 30, 5, 1_040);
    assert_eq!(one_fold_line(&done), "FOLD location=5 outcome=folded-fallback reason=primary-closing");
}

#[test]
fn a_primary_window_that_creates_no_tab_in_1_s_hands_the_fold_to_a_fallback_window() {
    let mut e = engine(&[1, 3]);
    let windows = [fwin(1, &[0]), fwin(3, &[0])];
    let step = begin(&mut e, 2, 5, vec![], &[w(1), w(3)], None, &windows, &[]);
    assert_eq!(create_target(&step.commands), w(1));
    let first_create = create_op(&step.commands);

    // Fired early the timer does nothing; at 1 s the Primary_Window is passed
    // over and a fresh fold context is asked for.
    let step = fire_at(&mut e, only_timer(&step.commands));
    let probe = probe_in(&step.commands);
    let step = context(&mut e, CTX + FOLD_TAB_CREATE_MS + 10, probe, &windows, &[]);
    assert_eq!(create_target(&step.commands), w(3));
    let second_create = create_op(&step.commands);
    assert_ne!(first_create, second_create);
    let done = finish_fold(&mut e, second_create, 3, 2, 30, 5, 2_100);
    assert_eq!(one_fold_line(&done), "FOLD location=5 outcome=folded-fallback reason=tab-not-created-1s");
}

#[test]
fn a_fallback_window_that_creates_no_tab_in_1_s_leaves_the_candidate_open() {
    let mut e = engine(&[1, 3]);
    let windows = [fwin(1, &[0]), fwin(3, &[0])];
    let step = begin(&mut e, 2, 5, vec![], &[w(1), w(3)], None, &windows, &[]);
    let step = fire_at(&mut e, only_timer(&step.commands));
    let probe = probe_in(&step.commands);
    let at = CTX + FOLD_TAB_CREATE_MS + 10;
    let step = context(&mut e, at, probe, &windows, &[]);
    assert_eq!(create_target(&step.commands), w(3));

    let step = fire_at(&mut e, only_timer(&step.commands));
    assert_left_open(&step, 2);
    assert_eq!(one_fold_line(&step), "FOLD location=5 outcome=not-folded reason=tab-not-created-1s");
    assert!(e.is_excluded(w(2)));
}

// ---- FOLD outcomes: folded-foreground ----

#[test]
fn the_foreground_exception_logs_folded_foreground_into_the_foreground_window() {
    // The Matching_Tab is the Active_Tab of the Foreground_Explorer_Window.
    let mut e = engine(&[1, 3]);
    let windows = [fwin(1, &[0]), fwin(3, &[5])];
    let step = begin(&mut e, 2, 5, vec![], &[w(1), w(3)], Some(w(3)), &windows, &[]);
    assert_eq!(create_target(&step.commands), w(3), "a Folded_Tab, not a Tab_Switch");
    assert!(!step.commands.iter().any(|c| matches!(c, Command::ActivateTab { .. })));
    let done = finish_fold(&mut e, create_op(&step.commands), 3, 2, 30, 5, 1_040);
    assert_eq!(one_fold_line(&done), "FOLD location=5 outcome=folded-foreground into=foreground passed=");
}

#[test]
fn a_failed_foreground_fold_follows_the_chain_and_logs_what_it_passed_over() {
    let mut e = engine(&[1, 3]);
    let windows = [fwin(1, &[0]), fwin(3, &[5])];
    let step = begin(&mut e, 2, 5, vec![], &[w(1), w(3)], Some(w(3)), &windows, &[]);
    assert_eq!(create_target(&step.commands), w(3));
    // The foreground window creates no tab in 1 s: it is passed over and the
    // Primary_Window takes the Folded_Tab.
    let step = fire_at(&mut e, only_timer(&step.commands));
    let probe = probe_in(&step.commands);
    let step = context(&mut e, 2_010, probe, &windows, &[]);
    assert_eq!(create_target(&step.commands), w(1));
    let done = finish_fold(&mut e, create_op(&step.commands), 1, 2, 30, 5, 2_100);
    assert_eq!(
        one_fold_line(&done),
        "FOLD location=5 outcome=folded-foreground into=primary passed=tab-not-created-1s"
    );
}

#[test]
fn a_failed_foreground_fold_can_end_in_a_fallback_window() {
    let mut e = engine(&[1, 3, 4]);
    let windows = [fwin(3, &[5]), fwin(1, &[0]), fwin(4, &[0])];
    let tracked = [w(1), w(3), w(4)];
    let step = begin(&mut e, 2, 5, vec![], &tracked, Some(w(3)), &windows, &[]);
    assert_eq!(create_target(&step.commands), w(3));
    let step = fire_at(&mut e, only_timer(&step.commands));
    let probe = probe_in(&step.commands);
    // The Primary_Window (1) is now closing, so the first fallback window
    // other than the foreground window takes the tab.
    let windows = [fwin(3, &[5]), closing(fwin(1, &[0])), fwin(4, &[0])];
    let step = context(&mut e, 2_010, probe, &windows, &[]);
    assert_eq!(create_target(&step.commands), w(4));
    let done = finish_fold(&mut e, create_op(&step.commands), 4, 2, 30, 5, 2_100);
    assert_eq!(
        one_fold_line(&done),
        "FOLD location=5 outcome=folded-foreground into=fallback passed=tab-not-created-1s"
    );
}

// ---- FOLD outcomes: switched ----

#[test]
fn a_tab_switch_in_the_primary_window_logs_switched_primary() {
    let mut e = engine(&[1]);
    let step = begin(&mut e, 2, 5, vec![], &[w(1)], None, &[fwin(1, &[0, 5])], &[]);
    let [Command::ActivateTab { op, win, index }, Command::SetTimer { at, .. }] = step.commands.as_slice() else {
        panic!("expected ActivateTab and a timer, got {:?}", step.commands);
    };
    assert_eq!((*win, *index), (w(1), 1));
    assert_eq!(*at, Millis(CTX + FOLD_SWITCH_MS));
    let step = e.activation_confirmed(Millis(1_100), *op, &not_min);
    assert!(matches!(
        step.commands.as_slice(),
        [Command::Focus(f), Command::CloseWindow { win: c }, Command::SetTimer { .. }] if *f == w(1) && *c == w(2)
    ));
    let done = e.window_destroyed(Millis(1_200), w(2)).unwrap();
    assert_eq!(one_fold_line(&done), "FOLD location=5 outcome=switched matching=primary");
    assert_eq!(done.commands.last(), Some(&Command::RescanWindow(w(1))));
}

#[test]
fn a_tab_switch_in_another_window_logs_switched_other_and_selects_then_shows_then_focuses() {
    let mut e = engine(&[1, 3]);
    let items = vec![ItemId(vec![9])];
    let windows = [fwin(1, &[0]), fwin(3, &[5])];
    let step = begin(&mut e, 2, 5, items.clone(), &[w(1), w(3)], None, &windows, &[]);
    let op = activate_op(&step.commands);
    let minimized = |win: WinKey| (win == w(3)).then(minimized_placement);
    let step = e.activation_confirmed(Millis(1_100), op, &minimized);
    assert!(matches!(
        step.commands.as_slice(),
        [
            Command::SelectItems { tab, items: got, .. },
            Command::ApplyPlacement { win: a, .. },
            Command::Focus(f),
            Command::CloseWindow { win: c },
            Command::SetTimer { .. },
        ] if *tab == tk(300) && got == &items && *a == w(3) && *f == w(3) && *c == w(2)
    ));
    let done = e.window_destroyed(Millis(1_200), w(2)).unwrap();
    assert_eq!(one_fold_line(&done), "FOLD location=5 outcome=switched matching=other");
}

// ---- FOLD outcomes: switch-failed-folded ----

/// Candidate 2 (Location 5) plans a Tab_Switch into the Matching_Tab at
/// `holder`'s tab; returns the engine and the step of the plan.
fn switching(holder: u64) -> (FoldingEngine, FoldStep) {
    let mut e = engine(&[1, 3]);
    let windows = if holder == 1 { [fwin(1, &[0, 5]), fwin(3, &[0])] } else { [fwin(1, &[0]), fwin(3, &[5])] };
    let step = begin(&mut e, 2, 5, vec![], &[w(1), w(3)], None, &windows, &[]);
    assert!(matches!(step.commands.first(), Some(Command::ActivateTab { win, .. }) if *win == w(holder)));
    (e, step)
}

#[test]
fn a_tab_switch_that_is_not_confirmed_in_1_s_folds_into_the_window_that_held_the_tab() {
    let (mut e, step) = switching(1);
    let step = fire_at(&mut e, only_timer(&step.commands));
    let probe = probe_in(&step.commands);
    let step = context(&mut e, 2_010, probe, &[fwin(1, &[0, 5]), fwin(3, &[0])], &[]);
    assert_eq!(create_target(&step.commands), w(1));
    let done = finish_fold(&mut e, create_op(&step.commands), 1, 2, 30, 5, 2_100);
    assert_eq!(
        one_fold_line(&done),
        "FOLD location=5 outcome=switch-failed-folded switch-reason=not-activated-1s into=matching-window passed="
    );
}

#[test]
fn a_late_confirmation_after_the_1_s_limit_is_ignored() {
    let (mut e, step) = switching(1);
    let op = activate_op(&step.commands);
    fire_at(&mut e, only_timer(&step.commands));
    assert_eq!(e.activation_confirmed(Millis(2_100), op, &not_min), FoldStep::default());
}

#[test]
fn a_revoked_matching_tab_logs_switch_reason_matching_tab_closed() {
    let (mut e, _) = switching(1);
    // A tab that is not the Matching_Tab changes nothing.
    assert_eq!(e.tab_revoked(Millis(1_100), w(1), tk(999)), FoldStep::default());
    let step = e.tab_revoked(Millis(1_100), w(1), tk(101));
    let probe = probe_in(&step.commands);
    let step = context(&mut e, 1_120, probe, &[fwin(1, &[0]), fwin(3, &[0])], &[]);
    assert_eq!(create_target(&step.commands), w(1));
    let done = finish_fold(&mut e, create_op(&step.commands), 1, 2, 30, 5, 1_200);
    assert_eq!(
        one_fold_line(&done),
        "FOLD location=5 outcome=switch-failed-folded switch-reason=matching-tab-closed into=matching-window passed="
    );
}

#[test]
fn a_destroyed_holder_logs_switch_reason_fold_target_closed_and_passes_over_closed() {
    let (mut e, _) = switching(3);
    let step = e.target_destroyed(Millis(1_100), w(3));
    let probe = probe_in(&step.commands);
    let step = context(&mut e, 1_120, probe, &[fwin(1, &[0])], &[]);
    assert_eq!(create_target(&step.commands), w(1));
    let done = finish_fold(&mut e, create_op(&step.commands), 1, 2, 30, 5, 1_200);
    assert_eq!(
        one_fold_line(&done),
        "FOLD location=5 outcome=switch-failed-folded switch-reason=fold-target-closed into=primary passed=closed"
    );
}

#[test]
fn an_unresponsive_or_closing_holder_is_passed_over_into_a_fallback_window() {
    for (unresponsive, closing_holder, passed) in [(true, false, "unresponsive"), (false, true, "closing")] {
        let (mut e, step) = switching(1);
        let step = fire_at(&mut e, only_timer(&step.commands));
        let probe = probe_in(&step.commands);
        let holder = fwin(1, &[0, 5]);
        let windows = [if closing_holder { closing(holder) } else { holder }, fwin(3, &[0])];
        let unresponsive_windows: &[u64] = if unresponsive { &[1] } else { &[] };
        let step = context(&mut e, 2_010, probe, &windows, unresponsive_windows);
        assert_eq!(create_target(&step.commands), w(3), "{passed}");
        let done = finish_fold(&mut e, create_op(&step.commands), 3, 2, 30, 5, 2_100);
        assert_eq!(
            one_fold_line(&done),
            format!(
                "FOLD location=5 outcome=switch-failed-folded switch-reason=not-activated-1s into=fallback passed={passed}"
            )
        );
    }
}

// ---- FOLD outcomes: not-folded ----

#[test]
fn no_fold_target_leaves_the_candidate_open() {
    let mut e = engine(&[]);
    let step = begin(&mut e, 2, 5, vec![], &[w(1)], None, &[], &[]);
    assert_left_open(&step, 2);
    assert_eq!(one_fold_line(&step), "FOLD location=5 outcome=not-folded reason=no-fold-target");
    assert!(e.is_excluded(w(2)) && !e.owns(w(2)));
}

#[test]
fn a_chain_that_runs_out_of_windows_logs_the_switch_reason_next_to_the_reason() {
    let (mut e, step) = switching(3);
    let step = fire_at(&mut e, only_timer(&step.commands));
    let probe = probe_in(&step.commands);
    let step = context(&mut e, 2_010, probe, &[], &[]);
    assert_left_open(&step, 2);
    assert_eq!(
        one_fold_line(&step),
        "FOLD location=5 outcome=not-folded reason=no-fold-target switch-reason=not-activated-1s"
    );
}

#[test]
fn a_folded_tab_that_does_not_show_the_location_in_3_s_leaves_the_candidate_open() {
    let mut e = engine(&[1]);
    let create = create_op(&begin(&mut e, 2, 5, vec![], &[w(1)], None, &[fwin(1, &[0])], &[]).commands);
    let shown = e.tab_started(Millis(1_040), create, tk(10));
    let timer = only_timer(&shown.commands);
    assert_eq!(timer.1, Millis(1_040 + FOLD_TAB_SHOW_MS));
    e.tab_created(Millis(1_300), w(1), tk(10));
    // Another Location in the tab does not count.
    assert_eq!(e.navigation_completed(Millis(1_400), tk(10), &loc(6), &not_min), FoldStep::default());

    let step = fire_at(&mut e, timer);
    assert_left_open(&step, 2);
    assert!(
        step.commands.iter().any(|c| matches!(c, Command::CloseTab { tab, .. } if *tab == tk(10))),
        "the tab created for the candidate is closed: {:?}",
        step.commands
    );
    assert_eq!(one_fold_line(&step), "FOLD location=5 outcome=not-folded reason=location-not-shown-3s");
}

#[test]
fn a_target_that_closes_after_it_created_the_tab_leaves_the_candidate_open() {
    let mut e = engine(&[1]);
    let create = create_op(&begin(&mut e, 2, 5, vec![], &[w(1)], None, &[fwin(1, &[0])], &[]).commands);
    e.tab_started(Millis(1_040), create, tk(10));
    e.tab_created(Millis(1_300), w(1), tk(10));
    let step = e.target_destroyed(Millis(1_400), w(1));
    assert_left_open(&step, 2);
    assert!(step.commands.iter().any(|c| matches!(c, Command::CloseTab { tab, .. } if *tab == tk(10))));
    assert_eq!(one_fold_line(&step), "FOLD location=5 outcome=not-folded reason=fold-target-closed");
}

#[test]
fn turning_folding_off_leaves_a_candidate_in_progress_open() {
    let mut e = engine(&[1]);
    begin(&mut e, 2, 5, vec![], &[w(1)], None, &[fwin(1, &[0])], &[]);
    // Turning it on again, or leaving it as it is, does nothing.
    assert_eq!(e.settings_changed(Millis(1_100), true), FoldStep::default());
    let step = e.settings_changed(Millis(1_100), false);
    assert_left_open(&step, 2);
    assert_eq!(one_fold_line(&step), "FOLD location=5 outcome=not-folded reason=folding-disabled");
    assert!(!e.enabled());
    assert_eq!(e.settings_changed(Millis(1_200), false), FoldStep::default());
    // Windows that appear later are excluded.
    let out = appear(&mut e, 2_000, 3, &[w(1)], None);
    assert_eq!(out.verdict, FirstSignalVerdict::Excluded(ExclusionReason::FoldingDisabled));
}

// ---- Tab_Switch and activation timers ----

#[test]
fn a_switch_timer_that_fires_after_the_confirmation_does_nothing() {
    let (mut e, step) = switching(1);
    let timer = only_timer(&step.commands);
    e.activation_confirmed(Millis(1_100), activate_op(&step.commands), &not_min);
    assert_eq!(e.timer(timer.1, timer.0, &not_min), FoldStep::default());
}

#[test]
fn an_unconfirmed_folded_tab_activation_is_repeated_once_and_then_given_up() {
    let mut e = engine(&[1]);
    let create = create_op(&begin(&mut e, 2, 5, vec![], &[w(1)], None, &[fwin(1, &[0])], &[]).commands);
    e.tab_started(Millis(1_040), create, tk(10));
    e.tab_created(Millis(1_300), w(1), tk(10));
    let step = e.navigation_completed(Millis(1_400), tk(10), &loc(5), &not_min);
    let first = activate_op(&step.commands);
    let timer = only_timer(&step.commands);
    assert_eq!(timer.1, Millis(1_400 + FOLD_ACTIVATE_MS));

    // One more try after 1 s ...
    let step = e.timer(timer.1, timer.0, &not_min);
    let second = activate_op(&step.commands);
    assert_ne!(first, second);
    let timer = only_timer(&step.commands);
    assert_eq!(timer.1, Millis(2_400 + FOLD_ACTIVATE_MS));

    // ... then the fold goes on without the confirmation, and says so.
    let step = e.timer(timer.1, timer.0, &not_min);
    let errors = error_lines(&step.commands);
    assert_eq!(errors.len(), 1, "{:?}", step.commands);
    assert!(errors[0].starts_with("ERROR operation=fold-activate-tab "), "{}", errors[0]);
    assert!(step.commands.contains(&Command::Focus(w(1))));
    assert!(step.commands.contains(&Command::CloseWindow { win: w(2) }));
}

#[test]
fn a_late_confirmation_of_the_first_activation_still_counts() {
    let mut e = engine(&[1]);
    let create = create_op(&begin(&mut e, 2, 5, vec![], &[w(1)], None, &[fwin(1, &[0])], &[]).commands);
    e.tab_started(Millis(1_040), create, tk(10));
    e.tab_created(Millis(1_300), w(1), tk(10));
    let step = e.navigation_completed(Millis(1_400), tk(10), &loc(5), &not_min);
    let first = activate_op(&step.commands);
    let timer = only_timer(&step.commands);
    e.timer(timer.1, timer.0, &not_min);
    let step = e.activation_confirmed(Millis(2_450), first, &not_min);
    assert!(step.commands.contains(&Command::CloseWindow { win: w(2) }), "{:?}", step.commands);
}

// ---- CloseWindow and its retry (Req 14.1) ----

/// Candidate 2 is folded into window 1 and has been told to close at 1 450;
/// returns the engine and the timer of the close retry.
fn closing_candidate() -> (FoldingEngine, (TimerId, Millis)) {
    let mut e = engine(&[1]);
    let create = create_op(&begin(&mut e, 2, 5, vec![], &[w(1)], None, &[fwin(1, &[0])], &[]).commands);
    e.tab_started(Millis(1_040), create, tk(10));
    e.tab_created(Millis(1_300), w(1), tk(10));
    let step = e.navigation_completed(Millis(1_400), tk(10), &loc(5), &not_min);
    let step = e.activation_confirmed(Millis(1_450), activate_op(&step.commands), &not_min);
    let timer = only_timer(&step.commands);
    assert_eq!(timer.1, Millis(1_450 + FOLD_CLOSE_RETRY_MS));
    (e, timer)
}

#[test]
fn a_candidate_that_ignores_close_window_gets_it_once_more_after_2_s() {
    let (mut e, timer) = closing_candidate();
    let step = fire_at(&mut e, timer);
    assert!(matches!(
        step.commands.as_slice(),
        [Command::CloseWindow { win }, Command::SetTimer { at, .. }]
            if *win == w(2) && *at == Millis(timer.1 .0 + FOLD_CLOSE_RETRY_MS)
    ));
    assert!(step.released.is_empty());
    assert!(e.owns(w(2)));
}

#[test]
fn a_candidate_that_closes_in_time_is_not_asked_again() {
    let (mut e, timer) = closing_candidate();
    let done = e.window_destroyed(Millis(1_600), w(2)).unwrap();
    assert_eq!(one_fold_line(&done), "FOLD location=5 outcome=folded-primary");
    assert_eq!(e.timer(timer.1, timer.0, &not_min), FoldStep::default());
}

#[test]
fn a_candidate_that_ignores_both_posts_is_revealed_and_handed_over() {
    let (mut e, timer) = closing_candidate();
    let step = fire_at(&mut e, timer);
    let second = only_timer(&step.commands);
    let step = fire_at(&mut e, second);
    assert_left_open(&step, 2);
    let errors = error_lines(&step.commands);
    assert_eq!(errors.len(), 1, "{:?}", step.commands);
    assert!(errors[0].starts_with("ERROR operation=fold-close "), "{}", errors[0]);
    // The fold that did complete is still logged, and the target is rescanned.
    assert_eq!(one_fold_line(&step), "FOLD location=5 outcome=folded-primary");
    assert!(step.commands.contains(&Command::RescanWindow(w(1))));
    assert!(!e.owns(w(2)));
    assert!(e.window_destroyed(Millis(9_000), w(2)).is_none());
}

// ---------------------------------------------------------------------------
// The late tab of a stopped fold (Req 14.13)
// ---------------------------------------------------------------------------

/// Candidate 2 passed window 1 over at 2 000 (no tab in 1 s); returns the
/// engine and the `CreateTab` op of the stopped attempt.
fn stopped_fold() -> (FoldingEngine, OpId) {
    let mut e = engine(&[1, 3]);
    let windows = [fwin(1, &[0]), fwin(3, &[0])];
    let step = begin(&mut e, 2, 5, vec![], &[w(1), w(3)], None, &windows, &[]);
    let op = create_op(&step.commands);
    let timer = only_timer(&step.commands);
    assert_eq!(timer.1, Millis(2_000));
    let step = fire_at(&mut e, timer);
    probe_in(&step.commands);
    (e, op)
}

#[test]
fn a_tab_that_appears_up_to_10_s_after_the_fold_stopped_is_closed() {
    assert_eq!(LATE_TAB_MS, 10_000);
    let (mut e, op) = stopped_fold();
    let step = e.tab_started(Millis(2_000 + LATE_TAB_MS), op, tk(50));
    assert!(matches!(step.commands.as_slice(), [Command::CloseTab { tab, .. }] if *tab == tk(50)), "{:?}", step.commands);
    // Its registration is nobody's Folded_Tab.
    assert_eq!(e.tab_created(Millis(12_100), w(1), tk(50)), FoldStep::default());
    // The op is spent: a second report changes nothing.
    assert_eq!(e.tab_started(Millis(12_200), op, tk(51)), FoldStep::default());
}

#[test]
fn a_tab_that_appears_more_than_10_s_after_the_fold_stopped_is_left_alone() {
    let (mut e, op) = stopped_fold();
    assert_eq!(e.tab_started(Millis(2_000 + LATE_TAB_MS + 1), op, tk(50)), FoldStep::default());
}

#[test]
fn a_tab_started_for_an_unknown_op_is_ignored() {
    let (mut e, op) = stopped_fold();
    assert_eq!(e.tab_started(Millis(2_100), OpId(op.0 + 1_000), tk(50)), FoldStep::default());
}

#[test]
fn a_pending_create_tab_of_a_candidate_left_open_is_remembered_too() {
    let mut e = engine(&[1]);
    let step = begin(&mut e, 2, 5, vec![], &[w(1)], None, &[fwin(1, &[0])], &[]);
    let op = create_op(&step.commands);
    // Folding is turned off while the CreateTab is outstanding.
    let off = e.settings_changed(Millis(1_100), false);
    assert_left_open(&off, 2);
    let late = e.tab_started(Millis(1_900), op, tk(50));
    assert!(matches!(late.commands.as_slice(), [Command::CloseTab { tab, .. }] if *tab == tk(50)), "{:?}", late.commands);
}

// ---------------------------------------------------------------------------
// The tokens of the FOLD entry, as written (Req 14.30)
// ---------------------------------------------------------------------------

#[test]
fn every_fold_entry_is_one_line_with_the_fold_event_and_the_location() {
    let mut e = engine(&[1]);
    let step = begin(&mut e, 2, 5, vec![], &[w(1)], None, &[fwin(1, &[0])], &[]);
    let done = finish_fold(&mut e, create_op(&step.commands), 1, 2, 30, 5, 1_040);
    let entries: Vec<&LogEntry> = done
        .commands
        .iter()
        .filter_map(|c| match c {
            Command::AppendLog(entry) => Some(entry),
            _ => None,
        })
        .collect();
    assert_eq!(entries.len(), 1);
    let line = format_entry(&TS, entries[0]);
    assert!(line.starts_with("2025-01-31 09:15:02.123 +00:00 FOLD location=5 outcome="), "{line}");
}
