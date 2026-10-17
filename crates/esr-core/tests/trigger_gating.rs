//! Trigger detection and gating in the `Engine` (task 11.5), driven against
//! the simulated Explorer with `InitialLocationKnown` announcements
//! (`SimConfig::for_restoration`).
//!
//! Requirements 4.1, 4.2, 4.3, 4.8, 4.11, 10.4.

use esr_core::engine::{Command, Engine, Environment, ExplorerOptionValues, Millis, SessionFileRead, StartupInput, WinKey};
use esr_core::logfmt::entry::LogEntry;
use esr_core::model::{
    DisplayRef, LocationId, Placement, Rect, SaveCause, SessionDoc, ShowState, Snapshot, Status, TabRec, Timestamp,
    WindowId, WindowRec, MAX_FAILED_ATTEMPTS,
};
use esr_core::settings::Settings;
use esr_sim::explorer::{default_displays, default_locations, CloseOrder, Op, Pick, SimConfig, SimExplorer};
use esr_sim::restore::{home_location, CommandKind, Driver, Launch, Origin, Run, Stop, UserAction};

const DOCUMENTS: usize = 2;
const DOWNLOADS: usize = 3;
const PHOTOS: usize = 4;
const BACKUP: usize = 5;
const THIS_PC: usize = 9;
const RECYCLE: usize = 11;

fn loc(i: usize) -> LocationId {
    default_locations()[i].clone()
}

fn locs(ids: &[usize]) -> Vec<LocationId> {
    ids.iter().map(|&i| loc(i)).collect()
}

fn rect(left: i32, top: i32) -> Rect {
    Rect { left, top, right: left + 800, bottom: top + 600 }
}

fn window(id: u32, tabs: &[usize], left: i32) -> WindowRec {
    let d = &default_displays()[usize::from(left >= 1920)];
    WindowRec {
        id: WindowId(id),
        tabs: tabs.iter().map(|&i| TabRec { location: loc(i) }).collect(),
        active: 0,
        placement: Placement {
            state: ShowState::Normal,
            restored: rect(left, 40),
            display: DisplayRef { device: d.device.clone(), monitor: d.monitor },
        },
    }
}

fn snapshot(windows: Vec<WindowRec>) -> Snapshot {
    let activation = windows.iter().map(|w| w.id).collect();
    Snapshot { windows, activation }
}

fn pending(snap: Snapshot, failed_attempts: u8) -> SessionDoc {
    SessionDoc {
        version: esr_core::format::FORMAT_VERSION,
        status: Status::Pending,
        failed_attempts,
        saved_at: Timestamp { year: 2025, month: 3, day: 4, hour: 5, minute: 6, second: 7, millis: 8, offset_minutes: 0 },
        cause: SaveCause::LastWindowClosed,
        snapshot: snap,
    }
}

fn launch(location: LocationId, left: i32) -> Launch {
    Launch { location, selection: vec![], rect: rect(left, 300), maximized: false }
}

#[derive(Clone, Default)]
struct Case {
    settings: Settings,
    file: Option<SessionDoc>,
    /// Builtin_Restore_Option logon restore conditions hold (Req 10.4).
    builtin: bool,
}

enum Step {
    Launch(Launch),
    Op(Op),
}

/// Runs an Engine against the simulator, playing `steps` at their times.
fn play(case: Case, steps: Vec<(u64, Step)>) -> (Engine, Run) {
    let mut startup = StartupInput {
        session_file: case
            .file
            .map_or(SessionFileRead::Absent, |d| SessionFileRead::Content(esr_core::format::serialize(&d).into_bytes())),
        environment: Environment { displays: default_displays(), ..Environment::default() },
        background_logon: false,
        logon_at: None,
        shell_ready_at: Some(Millis(0)),
    };
    if case.builtin {
        startup.environment.options = ExplorerOptionValues { persist_browsers: Some(true), ..Default::default() };
        startup.background_logon = true;
        startup.logon_at = Some(Millis(0));
    }
    let (mut engine, commands) = Engine::new(case.settings, startup);
    let mut driver = Driver::new(SimExplorer::new(SimConfig::for_restoration()));
    driver.submit(commands);
    for (at, step) in steps {
        let action = match step {
            Step::Launch(l) => UserAction::Launch(l),
            Step::Op(op) => UserAction::Op(op),
        };
        driver.schedule(Millis(at), action);
    }
    let stop = driver.run(|now, ev| engine.handle(now, ev));
    assert_eq!(stop, Stop::Quiescent);
    (engine, driver.finish(stop))
}

fn user_windows(run: &Run) -> Vec<WinKey> {
    run.sim.window_records().iter().filter(|r| matches!(r.origin, Origin::User)).map(|r| r.key).collect()
}

fn shown(run: &Run, win: WinKey) -> Vec<LocationId> {
    run.sim.window(win).expect("open").tabs.iter().map(|t| t.last_known.clone().expect("navigated")).collect()
}

fn writes(run: &Run) -> Vec<SessionDoc> {
    run.commands
        .iter()
        .filter_map(|c| match &c.command {
            Command::WriteSessionFile { text, .. } => Some(esr_core::format::parse(text.as_bytes()).unwrap()),
            _ => None,
        })
        .collect()
}

fn attempts(run: &Run) -> usize {
    writes(run).iter().filter(|d| d.status == Status::Pending && d.failed_attempts > 0).count()
}

fn consumed(run: &Run) -> usize {
    writes(run).iter().filter(|d| d.status == Status::Consumed).count()
}

fn logs(run: &Run) -> Vec<&LogEntry> {
    run.sim.log_entries().iter().map(|(_, e)| e).collect()
}

fn restores(run: &Run) -> Vec<&LogEntry> {
    logs(run).into_iter().filter(|e| matches!(e, LogEntry::Restore { .. })).collect()
}

/// Restoration commands that act on `win` or one of its tabs.
fn commands_for(run: &Run, win: WinKey) -> Vec<&Command> {
    let tabs: Vec<_> = run.sim.window(win).map_or_else(Vec::new, |w| w.tabs.iter().map(|t| t.key).collect());
    run.commands
        .iter()
        .map(|c| &c.command)
        .filter(|c| match c {
            Command::ApplyPlacement { win: w, .. }
            | Command::CreateTab { win: w, .. }
            | Command::ActivateTab { win: w, .. }
            | Command::Reveal { win: w }
            | Command::Focus(w) => *w == win,
            Command::SetZOrder(order) => order.contains(&win),
            Command::Navigate { tab, .. } | Command::CloseTab { tab, .. } | Command::SelectItems { tab, .. } => {
                tabs.contains(tab)
            }
            _ => false,
        })
        .collect()
}

const WINDOW_COMMANDS: [CommandKind; 7] = [
    CommandKind::OpenWindow,
    CommandKind::ApplyPlacement,
    CommandKind::CreateTab,
    CommandKind::Navigate,
    CommandKind::CloseTab,
    CommandKind::ActivateTab,
    CommandKind::SelectItems,
];

fn no_window_commands(run: &Run) -> bool {
    WINDOW_COMMANDS.iter().all(|k| run.sim.commands_of(*k).is_empty())
}

#[test]
fn a_default_launch_restores_the_pending_snapshot_once() {
    let snap = snapshot(vec![window(1, &[DOCUMENTS, DOWNLOADS], 100), window(2, &[PHOTOS], 2000)]);
    let case = Case { file: Some(pending(snap, 0)), ..Case::default() };
    // A second window after completion is not a trigger (Req 4.2).
    let (engine, run) = play(case, vec![(1_000, Step::Launch(launch(home_location(), 300))), (8_000, Step::Launch(launch(loc(RECYCLE), 600)))]);
    let users = user_windows(&run);
    let (trigger, later) = (users[0], users[1]);
    let session = engine.restore_session().expect("restored");
    assert_eq!(session.trigger(), trigger);
    assert!(session.is_finished());
    assert_eq!(shown(&run, trigger), locs(&[DOCUMENTS, DOWNLOADS]), "the default tab is replaced (Req 6.1)");
    assert_eq!(restores(&run).len(), 1);
    assert!(matches!(restores(&run)[0], LogEntry::Restore { targeted: false, windows: 2, tabs: 3, .. }));
    // One counted attempt, one consumption (Req 4.1, 4.4).
    assert_eq!((attempts(&run), consumed(&run)), (1, 1));
    assert!(commands_for(&run, later).is_empty());
    assert_eq!(shown(&run, later), locs(&[RECYCLE]));
    assert_eq!(engine.tracker().window_count(), 3);
}

#[test]
fn a_targeted_launch_keeps_the_requested_folder_as_the_last_tab() {
    let snap = snapshot(vec![window(1, &[DOCUMENTS, DOWNLOADS], 100)]);
    let case = Case { file: Some(pending(snap, 0)), ..Case::default() };
    let (_, run) = play(case, vec![(1_000, Step::Launch(launch(loc(PHOTOS), 300)))]);
    let trigger = user_windows(&run)[0];
    assert_eq!(shown(&run, trigger), locs(&[DOCUMENTS, DOWNLOADS, PHOTOS]));
    assert!(matches!(restores(&run)[..], [LogEntry::Restore { targeted: true, windows: 1, tabs: 2, .. }]));
}

#[test]
fn reopening_explorer_after_the_last_window_closed_restores_it() {
    let close = Op::CloseWindow {
        win: Pick(0),
        order: CloseOrder { revoke_before: 0, revoke_after: false, gap_ms: 0, rotate: Pick(0) },
    };
    let (engine, run) = play(
        Case::default(),
        vec![
            // No Pending_Snapshot: nothing (Req 4.3).
            (1_000, Step::Launch(launch(loc(DOCUMENTS), 300))),
            (3_000, Step::Op(close)),
            (5_000, Step::Launch(launch(home_location(), 300))),
        ],
    );
    let users = user_windows(&run);
    assert_eq!(engine.restore_session().unwrap().trigger(), users[1]);
    assert!(commands_for(&run, users[0]).is_empty());
    assert_eq!(shown(&run, users[1]), locs(&[DOCUMENTS]));
    assert_eq!(restores(&run).len(), 1);
    assert_eq!((attempts(&run), consumed(&run)), (1, 1));
}

#[test]
fn without_a_pending_snapshot_the_window_is_left_alone() {
    let consumed_doc = SessionDoc { status: Status::Consumed, ..pending(snapshot(vec![window(1, &[PHOTOS], 100)]), 0) };
    for file in [None, Some(consumed_doc)] {
        let (engine, run) = play(Case { file, ..Case::default() }, vec![(1_000, Step::Launch(launch(home_location(), 300)))]);
        assert!(engine.restore_session().is_none());
        assert!(no_window_commands(&run));
        assert_eq!(shown(&run, user_windows(&run)[0]), vec![home_location()]);
        assert_eq!((attempts(&run), consumed(&run)), (0, 0));
    }
}

#[test]
fn disabled_restoration_consumes_the_pending_snapshot_and_opens_nothing() {
    let case = Case {
        settings: Settings { restoration_enabled: false, ..Settings::default() },
        file: Some(pending(snapshot(vec![window(1, &[DOCUMENTS], 100), window(2, &[PHOTOS], 2000)]), 0)),
        ..Case::default()
    };
    let (engine, run) = play(case, vec![(1_000, Step::Launch(launch(home_location(), 300)))]);
    assert!(engine.restore_session().is_none());
    assert!(no_window_commands(&run));
    assert_eq!(shown(&run, user_windows(&run)[0]), vec![home_location()]);
    assert_eq!((attempts(&run), consumed(&run)), (0, 1));
    assert!(logs(&run).is_empty(), "{:?}", logs(&run));
    assert!(engine.store().durable().doc().is_some_and(|d| d.status != Status::Pending || d.cause == SaveCause::Checkpoint));
}

#[test]
fn builtin_restore_at_logon_consumes_logs_and_opens_nothing() {
    let snap = snapshot(vec![window(1, &[DOCUMENTS], 100), window(2, &[PHOTOS], 2000)]);
    let case = Case { file: Some(pending(snap, 0)), builtin: true, ..Case::default() };
    // Explorer reopens two windows at logon.
    let (engine, run) = play(
        case,
        vec![(1_000, Step::Launch(launch(loc(DOCUMENTS), 100))), (1_050, Step::Launch(launch(loc(PHOTOS), 2000)))],
    );
    assert!(engine.restore_session().is_none());
    assert!(no_window_commands(&run));
    assert_eq!(logs(&run), vec![&LogEntry::BuiltinRestoreDetected]);
    assert_eq!((attempts(&run), consumed(&run)), (0, 1));
    let users = user_windows(&run);
    assert_eq!(shown(&run, users[0]), locs(&[DOCUMENTS]));
    assert_eq!(shown(&run, users[1]), locs(&[PHOTOS]));
}

#[test]
fn an_exhausted_snapshot_is_abandoned_once_and_not_restored() {
    let case = Case { file: Some(pending(snapshot(vec![window(1, &[DOCUMENTS], 100)]), MAX_FAILED_ATTEMPTS)), ..Case::default() };
    let (engine, run) = play(case, vec![(1_000, Step::Launch(launch(home_location(), 300)))]);
    assert!(engine.restore_session().is_none());
    assert!(no_window_commands(&run));
    assert_eq!(logs(&run).iter().filter(|e| matches!(e, LogEntry::Abandoned)).count(), 1);
    assert_eq!(attempts(&run), 0);

    // Two failed attempts: the third is made.
    let case = Case { file: Some(pending(snapshot(vec![window(1, &[DOCUMENTS], 100)]), 2)), ..Case::default() };
    let (engine, run) = play(case, vec![(1_000, Step::Launch(launch(home_location(), 300)))]);
    assert!(engine.restore_session().is_some_and(|s| s.is_finished()));
    assert!(writes(&run).iter().any(|d| d.failed_attempts == 3));
    assert_eq!(restores(&run).len(), 1);
}

#[test]
fn windows_the_user_opens_during_a_restoration_get_no_restore_command() {
    let snap = snapshot(vec![window(1, &[DOCUMENTS, DOWNLOADS, PHOTOS], 100), window(2, &[BACKUP, THIS_PC], 2000)]);
    let case = Case { file: Some(pending(snap, 0)), ..Case::default() };
    let (engine, run) = play(
        case,
        vec![(1_000, Step::Launch(launch(home_location(), 300))), (1_150, Step::Launch(launch(loc(RECYCLE), 600)))],
    );
    let users = user_windows(&run);
    let (trigger, user) = (users[0], users[1]);
    let session = engine.restore_session().unwrap();
    assert_eq!(session.trigger(), trigger);
    assert!(!session.owns_window(user));
    // The user window opened while the restoration ran.
    let user_at = run.sim.window_record(user).unwrap().created_at;
    let focus_at = run.sim.commands_of(CommandKind::Focus)[0].at;
    assert!(user_at < focus_at);
    assert!(commands_for(&run, user).is_empty(), "{:?}", commands_for(&run, user));
    assert_eq!(shown(&run, user), locs(&[RECYCLE]));
    assert_eq!(run.sim.window(user).unwrap().placement.restored, rect(600, 300));
    // One restoration; the Live_Session includes the user's window (Req 4.5).
    assert_eq!(restores(&run).len(), 1);
    assert_eq!(attempts(&run), 1);
    assert!(engine.tracker().window(user).is_some());
    let live = engine.tracker().capture().0;
    assert_eq!((live.windows.len(), live.tab_count()), (3, 6));
}
