//! explorer.exe dying in the middle of a restoration (task 16.3 round 4c):
//! the Engine stops the restoration at once, logs it, keeps the counted
//! attempt and the protected retry snapshot, and retries at the next
//! Trigger_Window after a new shell process starts. Driven against the
//! simulated Explorer (`SimConfig::for_restoration`).
//!
//! Requirements 1.7, 3.4, 4.7, 4.10, 13.3.

use esr_core::engine::{
    Command, Engine, Environment, Event, Millis, SessionFileRead, StartupInput, TimerOwner, WinKey,
};
use esr_core::logfmt::entry::LogEntry;
use esr_core::model::{
    DisplayRef, LocationId, Placement, Rect, SaveCause, SessionDoc, ShowState, Snapshot, Status, TabRec, Timestamp,
    WindowId, WindowRec,
};
use esr_core::restore::{Failure, Phase};
use esr_core::settings::Settings;
use esr_sim::explorer::{default_displays, default_locations, Op, Pick, SimConfig, SimExplorer};
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

fn pending(snap: Snapshot) -> SessionDoc {
    SessionDoc {
        version: esr_core::format::FORMAT_VERSION,
        status: Status::Pending,
        failed_attempts: 0,
        saved_at: Timestamp { year: 2025, month: 3, day: 4, hour: 5, minute: 6, second: 7, millis: 8, offset_minutes: 0 },
        cause: SaveCause::LastWindowClosed,
        snapshot: snap,
    }
}

fn launch_home() -> UserAction {
    UserAction::Launch(Launch { location: home_location(), selection: vec![], rect: rect(300, 300), maximized: false })
}

/// The shell's process (the only one in these runs) dies without an orderly close.
fn kill_shell() -> UserAction {
    UserAction::Op(Op::KillProcess { pid: Pick(0) })
}

/// Runs an Engine with `snap` pending against the simulator, playing `script`.
fn play(snap: &Snapshot, script: Vec<(u64, UserAction)>) -> (Engine, Run) {
    let startup = StartupInput {
        session_file: SessionFileRead::Content(esr_core::format::serialize(&pending(snap.clone())).into_bytes()),
        environment: Environment { displays: default_displays(), ..Environment::default() },
        background_logon: false,
        logon_at: None,
        shell_ready_at: Some(Millis(0)),
    };
    let (mut engine, commands) = Engine::new(Settings::default(), startup);
    let mut driver = Driver::new(SimExplorer::new(SimConfig::for_restoration()));
    driver.submit(commands);
    for (at, action) in script {
        driver.schedule(Millis(at), action);
    }
    let stop = driver.run(|now, ev| engine.handle(now, ev));
    assert_eq!(stop, Stop::Quiescent);
    (engine, driver.finish(stop))
}

/// Every Session_File write, parsed, with its time.
fn writes(run: &Run) -> Vec<(Millis, SessionDoc)> {
    run.commands
        .iter()
        .filter_map(|c| match &c.command {
            Command::WriteSessionFile { text, .. } => Some((c.at, esr_core::format::parse(text.as_bytes()).unwrap())),
            _ => None,
        })
        .collect()
}

fn user_windows(run: &Run) -> Vec<WinKey> {
    run.sim.window_records().iter().filter(|r| matches!(r.origin, Origin::User)).map(|r| r.key).collect()
}

fn shown(run: &Run, win: WinKey) -> Vec<LocationId> {
    run.sim.window(win).expect("open").tabs.iter().map(|t| t.last_known.clone().expect("navigated")).collect()
}

fn time_of(run: &Run, pred: impl Fn(&Event) -> bool) -> Option<Millis> {
    run.events.iter().find(|e| pred(&e.event)).map(|e| e.at)
}

fn three_windows() -> Snapshot {
    snapshot(vec![
        window(1, &[DOCUMENTS, DOWNLOADS, PHOTOS], 100),
        window(2, &[THIS_PC, RECYCLE, BACKUP], 2000),
        window(3, &[PHOTOS, DOCUMENTS], 600),
    ])
}

/// When the shell dies while the restoration builds its windows, the
/// restoration fails at once (not at an operation timeout), the failure is
/// logged, the protected retry snapshot keeps its content and its one
/// counted attempt, and nothing overwrites it, not the Session_End of the
/// termination, nor any checkpoint.
#[test]
fn explorer_dying_mid_restoration_fails_it_at_once_and_keeps_the_protected_snapshot() {
    let snap = three_windows();
    // The simulated restoration of these 8 tabs ends about 1.1 s after the
    // trigger; every kill lands while it runs.
    for kill_after in [10, 150, 400, 700, 900] {
        let (engine, run) = play(&snap, vec![(1_000, launch_home()), (1_000 + kill_after, kill_shell())]);
        let session = engine.restore_session().expect("a restoration started");
        let lost_at = time_of(&run, |e| matches!(e, Event::ShellLost)).expect("ShellLost");
        assert!(
            matches!(session.phase(), Phase::Failed(Failure::ShellLost)),
            "kill +{kill_after}: {:?}",
            session.phase()
        );

        // One ERROR entry, when the shell was lost (Req 4.10, 13.3).
        let errors: Vec<_> = run
            .sim
            .log_entries()
            .iter()
            .filter(|(_, e)| matches!(e, LogEntry::Error { .. }))
            .collect();
        assert_eq!(errors.len(), 1, "kill +{kill_after}: {errors:?}");
        let (_, entry) = errors[0];
        assert!(
            matches!(entry, LogEntry::Error { operation, description }
                if operation == "restore-shell" && description.contains("explorer.exe")),
            "{entry:?}"
        );
        assert!(!run.sim.log_entries().iter().any(|(_, e)| matches!(e, LogEntry::Restore { .. })));
        // The session stopped when it saw ShellLost, not at an operation
        // timeout: no Restore timer had fired by then.
        let error_cmd = run
            .commands
            .iter()
            .position(|c| matches!(&c.command, Command::AppendLog(LogEntry::Error { operation, .. }) if operation == "restore-shell"))
            .unwrap();
        let restore_timer_fired_before =
            run.events.iter().any(|e| e.at <= lost_at && matches!(e.event, Event::Timer(id) if id.owner == TimerOwner::Restore));
        assert!(!restore_timer_fired_before, "kill +{kill_after}");

        // The attempt write only: Pending, one failed attempt, the snapshot
        // unchanged (Req 4.7, 4.10); protected, so the termination's
        // Session_End and checkpoints keep it.
        let w = writes(&run);
        assert_eq!(w.len(), 1, "kill +{kill_after}: {w:?}");
        assert_eq!((w[0].1.status, w[0].1.failed_attempts), (Status::Pending, 1));
        assert_eq!(w[0].1.snapshot, snap);
        assert!(engine.store().is_protected());
        assert!(!engine.store().is_restoring());
        let kept = engine.store().pending_snapshot().expect("still pending");
        assert_eq!((kept.failed_attempts, &kept.snapshot), (1, &snap));

        // No restoration command once the session stopped (only the reveal
        // of the windows it opened, which is harmless for a dead process and
        // right for any window that survived, Req 4.10).
        let after: Vec<_> = run.commands[error_cmd + 1..]
            .iter()
            .filter(|c| !matches!(c.command, Command::AppendLog(_) | Command::SetTimer { .. } | Command::Reveal { .. }))
            .collect();
        assert!(after.is_empty(), "kill +{kill_after}: {after:?}");
    }
}

/// The next Trigger_Window after a new shell process starts retries the
/// protected snapshot (second counted attempt) and completes it.
#[test]
fn the_next_explorer_start_retries_the_snapshot_and_completes() {
    let snap = three_windows();
    let (engine, run) = play(
        &snap,
        vec![
            (1_000, launch_home()),
            (1_700, kill_shell()),
            // A new shell process starts; the user then opens File Explorer.
            (15_000, UserAction::Op(Op::RestoreShell)),
            (20_000, launch_home()),
        ],
    );
    let w = writes(&run);
    // Up to the consumption (a checkpoint of the restored session follows it).
    let consumed = w.iter().position(|(_, d)| d.status == Status::Consumed).expect("consumed");
    let (consumed_at, _) = w[consumed];
    let w = &w[..consumed];
    let attempts: Vec<u8> = w.iter().map(|(_, d)| d.failed_attempts).collect();
    assert_eq!(attempts, vec![1, 2], "{w:?}");
    assert!(w.iter().all(|(_, d)| d.status == Status::Pending && d.snapshot == snap), "the snapshot never changes");
    assert!(consumed_at.0 > 20_000);

    let session = engine.restore_session().expect("restored");
    assert!(session.is_finished());
    assert_eq!(session.phase(), &Phase::Completed);
    let users = user_windows(&run);
    let retry_trigger = *users.last().unwrap();
    assert_eq!(session.trigger(), retry_trigger);
    assert_eq!(shown(&run, retry_trigger), locs(&[DOCUMENTS, DOWNLOADS, PHOTOS]));
    let restores = run.sim.log_entries().iter().filter(|(_, e)| matches!(e, LogEntry::Restore { .. })).count();
    assert_eq!(restores, 1);
    assert_eq!(run.sim.commands_of(CommandKind::Focus).len(), 1);
}
