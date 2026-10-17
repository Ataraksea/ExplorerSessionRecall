//! Property tests for the classification of a new window at its first signal
//! (task 23.1).
//!
// Feature: explorer-session-restore, Property 31: Candidate classification at the first signal
//!
//! **Property 31: Candidate classification at the first signal**
//!
//! *For any* `WindowAppeared` context (folding on or off, tracked windows per
//! desktop including `DesktopView::Unknown`, restoration in progress or not,
//! the Builtin_Restore_Option heuristic, the bypass keys, any `foreground`
//! value) and *any* `TabDetached` timing, the window becomes a
//! Folding_Candidate exactly when folding is on, at least one tracked
//! non-candidate window is on its desktop, no restoration or trigger is
//! pending, the heuristic does not hold, the bypass keys are up and no
//! `TabDetached` of a tracked window lies in `[-100 ms, +300 ms]` around the
//! first signal. A candidate's first command is `HideWindow`, in the same
//! `handle` call. A window that is not a candidate, or is a drag-out, never
//! receives a command from the `FoldingEngine` other than one `Reveal`
//! (drag-out only), is never a candidate later, and a window first seen while
//! others were open is never a Trigger_Window.
//!
//! Three tests, from the `FoldingEngine` up to the whole `Engine`:
//!
//! - `classification_of_one_first_signal`: `FoldingEngine::window_appeared`
//!   with every input of [`FirstSignalEnv`] generated, against a model of the
//!   rules in the design order (including which `ExclusionReason` is
//!   reported), the `TabDetached` timing around the first signal at and
//!   around its edges, and the commands of the candidate and of the window
//!   that is not one.
//! - `the_engine_classifies_every_first_signal_of_a_sequence`: the real
//!   `Engine`, fed `WindowAppeared` for up to five windows in turn after a
//!   few windows are tracked (a pending Trigger_Window candidate for
//!   "restoration in progress", the `PersistBrowsers` logon heuristic for the
//!   Builtin_Restore_Option), each followed by its scan and its
//!   `InitialLocationKnown`. Earlier candidates do not count as tracked
//!   windows, excluded windows do once they are scanned, and none of the new
//!   windows becomes a Trigger_Window.
//! - `a_tab_detached_after_the_first_signal_reveals_the_hidden_candidate`:
//!   a drag-out whose `TabDetached` arrives after the first signal, through
//!   the `Engine`: one `Reveal`, the window handed to the tracker, never a
//!   candidate again.
//!
//! **Validates: Requirements 14.2, 14.3, 14.4, 14.10, 12.10**

mod common;

use common::strategies::{fold_win_key, pool_location};
use esr_core::engine::{
    Command, DesktopView, Engine, Environment, Event, ExplorerOptionValues, Millis, ObservedLocation, SessionFileRead,
    StartupInput, TabKey, TabObservation, WinKey, WindowObservation,
};
use esr_core::fold::{ExclusionReason, FirstSignalEnv, FirstSignalVerdict, FoldStep, FoldingEngine, DRAG_OUT_WINDOW_MS};
use esr_core::logfmt::entry::LogEntry;
use esr_core::model::{
    DisplayRef, Placement, Rect, SaveCause, SessionDoc, ShowState, Snapshot, Status, TabRec, Timestamp, WindowId,
    WindowRec,
};
use esr_core::settings::Settings;
use esr_sim::explorer::{default_displays, default_locations};
use proptest::collection::vec;
use proptest::prelude::*;
use proptest::sample::select;

/// How long before the first signal a `TabDetached` still marks a drag-out.
const BEFORE_MS: u64 = DRAG_OUT_WINDOW_MS.0.unsigned_abs();
/// How long after the first signal it still does.
const AFTER_MS: u64 = DRAG_OUT_WINDOW_MS.1.unsigned_abs();

/// The window that appears.
const NEW: WinKey = WinKey { hwnd: 0x3000, seq: 500 };

/// A window that is open but not tracked.
fn stray(n: usize) -> WinKey {
    WinKey { hwnd: 0x2000 + n as u64, seq: 100 + n as u64 }
}

fn arrival_key(k: usize) -> WinKey {
    WinKey { hwnd: 0x3000 + k as u64, seq: 500 + k as u64 }
}

/// A `TabDetached` offset: at, just inside and just outside the edges, and
/// anywhere.
fn before_offset() -> impl Strategy<Value = u64> {
    prop_oneof![
        1 => Just(0u64), 1 => Just(BEFORE_MS - 1), 2 => Just(BEFORE_MS), 2 => Just(BEFORE_MS + 1), 3 => 0u64..=150,
    ]
}

fn after_offset() -> impl Strategy<Value = u64> {
    prop_oneof![
        1 => Just(0u64), 1 => Just(AFTER_MS - 1), 2 => Just(AFTER_MS), 2 => Just(AFTER_MS + 1), 3 => 0u64..=450,
    ]
}

/// `Unknown`, or `Known` over a subset of `universe` (most of it).
fn desktop_over(universe: Vec<WinKey>) -> BoxedStrategy<DesktopView> {
    let n = universe.len();
    prop_oneof![
        1 => Just(DesktopView::Unknown),
        4 => vec(prop_oneof![3 => Just(true), 1 => Just(false)], n).prop_map(move |keep| {
            DesktopView::Known(universe.iter().zip(keep).filter(|(_, k)| *k).map(|(w, _)| *w).collect())
        }),
    ]
    .boxed()
}

/// What the rules say for a window at its first signal, in the design order.
#[allow(clippy::too_many_arguments)]
fn expected_exclusion(
    enabled: bool,
    tracked: &[WinKey],
    desktop: &DesktopView,
    restoration: bool,
    builtin: bool,
    bypass: bool,
    drag_out: bool,
) -> Option<ExclusionReason> {
    let on_desktop = match desktop {
        DesktopView::Unknown => !tracked.is_empty(),
        DesktopView::Known(set) => tracked.iter().any(|t| set.contains(t)),
    };
    if !enabled {
        Some(ExclusionReason::FoldingDisabled)
    } else if !on_desktop {
        Some(ExclusionReason::NoWindowOnDesktop)
    } else if restoration {
        Some(ExclusionReason::RestorationActive)
    } else if builtin {
        Some(ExclusionReason::BuiltinRestore)
    } else if bypass {
        Some(ExclusionReason::BypassKeys)
    } else if drag_out {
        Some(ExclusionReason::DragOut)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// The FoldingEngine alone
// ---------------------------------------------------------------------------

/// A `TabDetached` around the first signal.
#[derive(Clone, Copy, Debug)]
enum Detach {
    None,
    /// `dt` ms before the first signal, from a tracked window (`idx` picks it)
    /// or, when `idx` is odd and `stray_source`, from an untracked one.
    Before { dt: u64, idx: usize, stray_source: bool },
    /// `dt` ms after the first signal.
    After { dt: u64, idx: usize, stray_source: bool },
}

#[derive(Clone, Debug)]
struct OneSignal {
    enabled: bool,
    tracked: usize,
    desktop: DesktopView,
    restoration: bool,
    builtin: bool,
    bypass: bool,
    foreground: Option<WinKey>,
    detach: Detach,
}

fn one_signal() -> impl Strategy<Value = OneSignal> {
    (0usize..=4)
        .prop_flat_map(|tracked| {
            let mut universe: Vec<WinKey> = (0..tracked).map(fold_win_key).collect();
            universe.extend([stray(0), stray(1), NEW]);
            let foreground = prop_oneof![
                1 => Just(None),
                4 => select(universe.clone()).prop_map(Some),
            ];
            let detach = prop_oneof![
                3 => Just(Detach::None),
                2 => (before_offset(), 0usize..8, any::<bool>())
                    .prop_map(|(dt, idx, stray_source)| Detach::Before { dt, idx, stray_source }),
                2 => (after_offset(), 0usize..8, any::<bool>())
                    .prop_map(|(dt, idx, stray_source)| Detach::After { dt, idx, stray_source }),
            ];
            (
                prop_oneof![5 => Just(true), 1 => Just(false)],
                Just(tracked),
                desktop_over(universe),
                prop_oneof![1 => Just(true), 4 => Just(false)],
                prop_oneof![1 => Just(true), 4 => Just(false)],
                prop_oneof![1 => Just(true), 4 => Just(false)],
                foreground,
                detach,
            )
        })
        .prop_map(|(enabled, tracked, desktop, restoration, builtin, bypass, foreground, detach)| OneSignal {
            enabled,
            tracked,
            desktop,
            restoration,
            builtin,
            bypass,
            foreground,
            detach,
        })
}

/// The source window of a `TabDetached`.
fn source(tracked: &[WinKey], idx: usize, stray_source: bool) -> (WinKey, bool) {
    if tracked.is_empty() || (stray_source && idx % 2 == 1) {
        (stray(2), false)
    } else {
        (tracked[idx % tracked.len()], true)
    }
}

fn is_hide_or_reveal(c: &Command) -> bool {
    matches!(c, Command::HideWindow { .. } | Command::Reveal { .. })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 1024, ..ProptestConfig::default() })]

    #[test]
    fn classification_of_one_first_signal(s in one_signal()) {
        let tracked: Vec<WinKey> = (0..s.tracked).map(fold_win_key).collect();
        let env = FirstSignalEnv { tracked: &tracked, restoration_active: s.restoration, builtin_restore: s.builtin };
        let t0 = 1_000u64;
        let mut fold = FoldingEngine::new(s.enabled);

        let mut drag_before = false;
        if let Detach::Before { dt, idx, stray_source } = s.detach {
            let (from, is_tracked) = source(&tracked, idx, stray_source);
            let step = fold.tab_detached(Millis(t0 - dt), from);
            prop_assert_eq!(step, FoldStep::default(), "no candidate yet: nothing to reveal");
            drag_before = is_tracked && dt <= BEFORE_MS;
        }

        let out = fold.window_appeared(Millis(t0), NEW, s.bypass, s.desktop.clone(), s.foreground, &env);
        let expected = expected_exclusion(
            s.enabled, &tracked, &s.desktop, s.restoration, s.builtin, s.bypass, drag_before,
        );
        match expected {
            None => {
                prop_assert_eq!(out.verdict, FirstSignalVerdict::Candidate);
                // `HideWindow` first, in the same call, then the 8 s timer.
                prop_assert_eq!(out.commands.len(), 2, "{:?}", out.commands);
                prop_assert_eq!(&out.commands[0], &Command::HideWindow { win: NEW });
                prop_assert!(matches!(out.commands[1], Command::SetTimer { .. }), "{:?}", out.commands[1]);
                prop_assert!(fold.owns(NEW));
                prop_assert!(!fold.is_excluded(NEW));
                prop_assert_eq!(fold.candidates_in_progress(), 1);
                // A candidate holds a reserved position, not a real one.
                prop_assert!(!fold.opening().contains(NEW));
            }
            Some(reason) => {
                prop_assert_eq!(out.verdict, FirstSignalVerdict::Excluded(reason));
                prop_assert!(out.commands.is_empty(), "an excluded window gets no command: {:?}", out.commands);
                prop_assert!(!fold.owns(NEW));
                prop_assert!(fold.is_excluded(NEW));
                prop_assert_eq!(fold.candidates_in_progress(), 0);
                // Every window is in the Opening_Order from its first signal.
                prop_assert!(fold.opening().contains(NEW));
            }
        }

        // Classified once: the same window reported again changes nothing.
        let again = fold.window_appeared(Millis(t0 + 5), NEW, !s.bypass, DesktopView::Unknown, None, &env);
        prop_assert_eq!(again.verdict, FirstSignalVerdict::AlreadyKnown);
        prop_assert!(again.commands.is_empty());
        prop_assert_eq!(fold.owns(NEW), expected.is_none());
        prop_assert_eq!(fold.is_excluded(NEW), expected.is_some());

        // A `TabDetached` after the first signal reveals a hidden candidate
        // within 300 ms, whatever else; nothing else is ever revealed.
        if let Detach::After { dt, idx, stray_source } = s.detach {
            // The Engine passes tracked windows only (see the Engine test
            // below); the FoldingEngine decides by the time alone.
            let (from, _) = source(&tracked, idx, stray_source);
            let step = fold.tab_detached(Millis(t0 + dt), from);
            if expected.is_none() && dt <= AFTER_MS {
                prop_assert_eq!(&step.commands, &vec![Command::Reveal { win: NEW }]);
                prop_assert_eq!(step.released.len(), 1);
                prop_assert_eq!(step.released[0].win, NEW);
                prop_assert!(!fold.owns(NEW));
                prop_assert!(fold.is_excluded(NEW));
                prop_assert!(fold.opening().contains(NEW));
                prop_assert_eq!(fold.candidates_in_progress(), 0);
            } else {
                prop_assert!(step.commands.iter().all(|c| !is_hide_or_reveal(c)), "{:?}", step.commands);
                prop_assert!(step.released.is_empty());
                prop_assert_eq!(fold.owns(NEW), expected.is_none());
            }
            // Neither a window that was excluded nor a revealed one is a
            // candidate later.
            let later = fold.window_appeared(Millis(t0 + dt + 1), NEW, false, DesktopView::Unknown, None, &env);
            prop_assert_eq!(later.verdict, FirstSignalVerdict::AlreadyKnown);
            prop_assert!(later.commands.is_empty());
        }
    }
}

// ---------------------------------------------------------------------------
// The whole Engine
// ---------------------------------------------------------------------------

/// Why the Builtin_Restore_Option heuristic does or does not hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Heuristic {
    /// Persisted browsers, logon task, no input, shell ready a moment ago.
    Holds,
    Off,
    NoPersist,
    NotBackground,
    /// Everything else holds, but the shell was ready more than 60 s ago.
    ShellStale,
}

#[derive(Clone, Debug)]
struct Arrival {
    bypass: bool,
    /// `None`: membership unreadable. Otherwise which windows of the universe
    /// (tracked windows, earlier arrivals, two strays, the new window itself)
    /// are on its desktop.
    on_desktop: Option<Vec<bool>>,
    /// Selects the `foreground` value.
    foreground: (u8, usize),
    /// A `TabDetached` of a tracked window `dt` ms before the first signal.
    detach: Option<(u64, usize, bool)>,
    pid: u32,
}

#[derive(Clone, Debug)]
struct Scenario {
    enabled: bool,
    initial: usize,
    restoring: bool,
    heuristic: Heuristic,
    arrivals: Vec<Arrival>,
}

fn arrival() -> impl Strategy<Value = Arrival> {
    (
        prop_oneof![1 => Just(true), 4 => Just(false)],
        prop_oneof![
            1 => Just(None),
            5 => vec(prop_oneof![4 => Just(true), 1 => Just(false)], 16).prop_map(Some),
        ],
        (0u8..5, 0usize..8),
        prop_oneof![3 => Just(None), 1 => (before_offset(), 0usize..8, any::<bool>()).prop_map(Some)],
        1u32..=3,
    )
        .prop_map(|(bypass, on_desktop, foreground, detach, pid)| Arrival { bypass, on_desktop, foreground, detach, pid })
}

fn scenario() -> impl Strategy<Value = Scenario> {
    (
        prop_oneof![5 => Just(true), 1 => Just(false)],
        0usize..=3,
        prop_oneof![1 => Just(true), 2 => Just(false)],
        prop_oneof![
            1 => Just(Heuristic::Holds),
            1 => Just(Heuristic::NoPersist),
            1 => Just(Heuristic::NotBackground),
            1 => Just(Heuristic::ShellStale),
            4 => Just(Heuristic::Off),
        ],
        vec(arrival(), 1..=5),
    )
        .prop_map(|(enabled, initial, restoring, heuristic, arrivals)| {
            // A restoration needs a first window to be the Trigger_Window,
            // and the logon heuristic would consume the Pending_Snapshot.
            let restoring = restoring && initial >= 1;
            let heuristic = if restoring { Heuristic::Off } else { heuristic };
            Scenario { enabled, initial, restoring, heuristic, arrivals }
        })
}

fn placement() -> Placement {
    let r = Rect { left: 0, top: 0, right: 800, bottom: 600 };
    Placement { state: ShowState::Normal, restored: r, display: DisplayRef { device: r"\\.\DISPLAY1".into(), monitor: r } }
}

fn tab_of(win: WinKey) -> TabKey {
    TabKey { hwnd: win.hwnd * 16 + 1, seq: win.seq * 16 + 1 }
}

fn scan(win: WinKey, pid: u32, location: ObservedLocation) -> Event {
    Event::WindowScanned(WindowObservation {
        win,
        pid,
        tabs: vec![TabObservation { tab: tab_of(win), location }],
        active: Some(tab_of(win)),
        strip: None,
        placement: placement(),
    })
}

/// A Session_File with a Pending_Snapshot of one window.
fn pending_file() -> Vec<u8> {
    let d = &default_displays()[0];
    let window = WindowRec {
        id: WindowId(1),
        tabs: vec![TabRec { location: default_locations()[2].clone() }],
        active: 0,
        placement: Placement {
            state: ShowState::Normal,
            restored: Rect { left: 100, top: 40, right: 900, bottom: 640 },
            display: DisplayRef { device: d.device.clone(), monitor: d.monitor },
        },
    };
    esr_core::format::serialize(&SessionDoc {
        version: esr_core::format::FORMAT_VERSION,
        status: Status::Pending,
        failed_attempts: 0,
        saved_at: Timestamp { year: 2025, month: 3, day: 4, hour: 5, minute: 6, second: 7, millis: 8, offset_minutes: 0 },
        cause: SaveCause::LastWindowClosed,
        snapshot: Snapshot { windows: vec![window], activation: vec![WindowId(1)] },
    })
    .into_bytes()
}

/// A command only the folding code issues.
fn is_fold_command(c: &Command) -> bool {
    matches!(
        c,
        Command::HideWindow { .. }
            | Command::Reveal { .. }
            | Command::ProbeFoldContext { .. }
            | Command::CloseWindow { .. }
            | Command::CreateTab { .. }
            | Command::Navigate { .. }
            | Command::CloseTab { .. }
            | Command::ActivateTab { .. }
            | Command::SelectItems { .. }
            | Command::ApplyPlacement { .. }
            | Command::Focus(_)
            | Command::AppendLog(LogEntry::Fold { .. })
    )
}

fn count_of(cmds: &[Command], pred: impl Fn(&Command) -> bool) -> usize {
    cmds.iter().filter(|c| pred(c)).count()
}

fn run_scenario(s: &Scenario) -> Result<(), TestCaseError> {
    let (base, options, background_logon) = match s.heuristic {
        Heuristic::Holds => (1_000, Some(true), true),
        Heuristic::Off => (1_000, Some(false), true),
        Heuristic::NoPersist => (1_000, None, true),
        Heuristic::NotBackground => (1_000, Some(true), false),
        Heuristic::ShellStale => (100_000, Some(true), true),
    };
    let startup = StartupInput {
        session_file: if s.restoring { SessionFileRead::Content(pending_file()) } else { SessionFileRead::Absent },
        environment: Environment {
            options: ExplorerOptionValues { persist_browsers: options, ..ExplorerOptionValues::default() },
            displays: default_displays(),
            ..Environment::default()
        },
        background_logon,
        logon_at: Some(Millis(0)),
        shell_ready_at: Some(Millis(0)),
    };
    let (mut engine, _) =
        Engine::new(Settings { tab_folding_enabled: s.enabled, ..Settings::default() }, startup);

    // Windows that were open before the first arrival: never a first signal.
    let mut tracked: Vec<WinKey> = (0..s.initial).map(fold_win_key).collect();
    for (i, &win) in tracked.iter().enumerate() {
        let location = if s.restoring && i == 0 {
            // The first window waits for its initial Location: the
            // Trigger_Window candidate of a restoration about to start.
            ObservedLocation::NotNavigated
        } else {
            ObservedLocation::Known(pool_location(i, 0))
        };
        engine.handle(Millis(base - 500 + i as u64), scan(win, 1, location));
        prop_assert!(engine.tracker().window(win).is_some());
    }
    if s.restoring {
        prop_assert_eq!(engine.trigger_candidate(), Some(tracked[0]));
    }

    let mut candidates: Vec<WinKey> = Vec::new();
    for (k, a) in s.arrivals.iter().enumerate() {
        let win = arrival_key(k);
        let t = base + 1_000 * k as u64;

        // A `TabDetached` shortly before the first signal.
        let mut drag_out = false;
        if let Some((dt, idx, stray_source)) = a.detach {
            let (from, is_tracked) = source(&tracked, idx, stray_source);
            let cmds = engine.handle(Millis(t - dt), Event::TabDetached { from });
            prop_assert!(cmds.is_empty(), "no candidate is hidden yet: {:?}", cmds);
            drag_out = is_tracked && dt <= BEFORE_MS;
        }

        let mut universe = tracked.clone();
        universe.extend(candidates.iter().copied());
        universe.extend([stray(0), stray(1), win]);
        let desktop = match &a.on_desktop {
            None => DesktopView::Unknown,
            Some(bits) => {
                DesktopView::Known(universe.iter().enumerate().filter(|(i, _)| bits[*i % bits.len()]).map(|(_, w)| *w).collect())
            }
        };
        let foreground = match a.foreground {
            (0, _) => None,
            (1, i) => tracked.get(i % tracked.len().max(1)).copied(),
            (2, i) => Some(stray(i % 2)),
            (3, _) => Some(win),
            (_, i) => candidates.get(i % candidates.len().max(1)).copied(),
        };

        let restoration = s.restoring;
        let builtin = s.heuristic == Heuristic::Holds;
        let expected =
            expected_exclusion(s.enabled, &tracked, &desktop, restoration, builtin, a.bypass, drag_out);

        let before = candidates.len();
        let cmds = engine.handle(
            Millis(t),
            Event::WindowAppeared { win, pid: a.pid, bypass_keys: a.bypass, desktop: desktop.clone(), foreground },
        );
        match expected {
            None => {
                prop_assert_eq!(cmds.first(), Some(&Command::HideWindow { win }), "arrival {}: {:?}", k, cmds);
                prop_assert_eq!(count_of(&cmds, |c| matches!(c, Command::HideWindow { .. })), 1);
                prop_assert_eq!(count_of(&cmds, |c| matches!(c, Command::Reveal { .. })), 0);
                prop_assert!(engine.folding().owns(win));
                candidates.push(win);
            }
            Some(reason) => {
                prop_assert!(cmds.is_empty(), "arrival {} ({:?}) got commands {:?}", k, reason, cmds);
                prop_assert!(!engine.folding().owns(win));
                prop_assert!(engine.folding().is_excluded(win));
            }
        }
        prop_assert_eq!(engine.folding().candidates_in_progress(), candidates.len());
        prop_assert_eq!(candidates.len(), before + usize::from(expected.is_none()));

        // The window's scan, then its initial Location.
        let cmds = engine.handle(Millis(t + 10), scan(win, a.pid, ObservedLocation::Known(pool_location(k + 3, 0))));
        prop_assert_eq!(count_of(&cmds, is_hide_or_reveal), 0, "{:?}", cmds);
        if expected.is_none() {
            // A candidate is not in the Live_Session.
            prop_assert!(engine.tracker().window(win).is_none());
            prop_assert!(cmds.is_empty(), "a candidate's scan goes to the FoldingEngine only: {:?}", cmds);
        } else {
            prop_assert!(engine.tracker().window(win).is_some());
            prop_assert!(!cmds.iter().any(is_fold_command), "{:?}", cmds);
            tracked.push(win);
        }
        let cmds = engine.handle(
            Millis(t + 20),
            Event::InitialLocationKnown {
                win,
                tab: Some(tab_of(win)),
                location: pool_location(k + 3, 0),
                selection: vec![],
            },
        );
        prop_assert_eq!(count_of(&cmds, is_hide_or_reveal), 0, "{:?}", cmds);
        if expected.is_some() {
            prop_assert!(!cmds.iter().any(is_fold_command), "an excluded window gets no fold command: {:?}", cmds);
        }

        // A window first seen while others were open is never a
        // Trigger_Window, and no restoration starts for it.
        prop_assert!(engine.trigger_candidate() != Some(win));
        prop_assert!(engine.restore_session().is_none());

        // Reported again, it is known: no command, no change of kind.
        let cmds = engine.handle(
            Millis(t + 30),
            Event::WindowAppeared { win, pid: a.pid, bypass_keys: !a.bypass, desktop: DesktopView::Unknown, foreground: None },
        );
        prop_assert!(cmds.is_empty(), "{:?}", cmds);
        prop_assert_eq!(engine.folding().owns(win), expected.is_none());
        prop_assert_eq!(engine.folding().candidates_in_progress(), candidates.len());
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn the_engine_classifies_every_first_signal_of_a_sequence(s in scenario()) {
        run_scenario(&s)?;
    }

    #[test]
    fn a_tab_detached_after_the_first_signal_reveals_the_hidden_candidate(
        tracked in 1usize..=3,
        dt in after_offset(),
        idx in 0usize..8,
        stray_source in any::<bool>(),
        scanned in any::<bool>(),
    ) {
        let startup = StartupInput {
            session_file: SessionFileRead::Absent,
            environment: Environment::default(),
            background_logon: false,
            logon_at: None,
            shell_ready_at: None,
        };
        let (mut engine, _) = Engine::new(Settings::default(), startup);
        let tracked: Vec<WinKey> = (0..tracked).map(fold_win_key).collect();
        for (i, &win) in tracked.iter().enumerate() {
            engine.handle(Millis(100 + i as u64), scan(win, 1, ObservedLocation::Known(pool_location(i, 0))));
        }

        let t = 1_000u64;
        let cmds = engine.handle(
            Millis(t),
            Event::WindowAppeared { win: NEW, pid: 7, bypass_keys: false, desktop: DesktopView::Unknown, foreground: None },
        );
        prop_assert_eq!(cmds.first(), Some(&Command::HideWindow { win: NEW }));
        if scanned {
            engine.handle(Millis(t), scan(NEW, 7, ObservedLocation::NotNavigated));
        }

        let (from, is_tracked) = source(&tracked, idx, stray_source);
        let cmds = engine.handle(Millis(t + dt), Event::TabDetached { from });
        if is_tracked && dt <= AFTER_MS {
            // Exactly one `Reveal`; the window is the tracker's from now on.
            prop_assert_eq!(count_of(&cmds, |c| matches!(c, Command::Reveal { win } if *win == NEW)), 1, "{:?}", cmds);
            prop_assert_eq!(count_of(&cmds, is_hide_or_reveal), 1, "{:?}", cmds);
            prop_assert!(cmds.contains(&Command::RescanWindow(NEW)), "{:?}", cmds);
            prop_assert!(!engine.folding().owns(NEW));
            prop_assert!(engine.folding().is_excluded(NEW));
            prop_assert_eq!(engine.folding().candidates_in_progress(), 0);
            if scanned {
                prop_assert!(engine.tracker().window(NEW).is_some(), "the buffered scan reached the tracker");
            }
            // Never a Trigger_Window, never a candidate again, nothing more
            // to reveal.
            prop_assert!(engine.trigger_candidate() != Some(NEW));
            let again = engine.handle(Millis(t + dt + 1), Event::TabDetached { from });
            prop_assert_eq!(count_of(&again, is_hide_or_reveal), 0, "{:?}", again);
        } else {
            prop_assert!(cmds.is_empty(), "{:?}", cmds);
            prop_assert!(engine.folding().owns(NEW));
            prop_assert_eq!(engine.folding().candidates_in_progress(), 1);
        }
        let later = engine.handle(
            Millis(t + dt + 2),
            Event::WindowAppeared { win: NEW, pid: 7, bypass_keys: false, desktop: DesktopView::Unknown, foreground: None },
        );
        prop_assert!(later.is_empty(), "{:?}", later);
    }
}
