//! Deferred tabs (Requirement 5.13, task 16.3): a restoration of more than
//! `IMMEDIATE_FOLDER_BUDGET` distinct folders opens the rest as placeholders
//! named after the saved folder, the Live_Session keeps their saved
//! Locations, and activating one navigates it there. Engine + simulated
//! Explorer.

use std::collections::HashSet;

use esr_core::engine::{
    Command, DEFERRED_OP_BASE, Engine, Environment, Millis, SessionFileRead, StartupInput, TimerOwner, WinKey,
};
use esr_core::logfmt::entry::LogEntry;
use esr_core::model::{
    DisplayRef, LocationId, LocationKind, Placement, Rect, SaveCause, SessionDoc, ShellKind, ShowState, Snapshot,
    Status, TabRec, Timestamp, Volume, WindowId, WindowRec,
};
use esr_core::plan::LaunchInfo;
use esr_core::plan::defer::IMMEDIATE_FOLDER_BUDGET;
use esr_core::restore::{Phase, RestoreParams};
use esr_core::settings::Settings;
use esr_sim::explorer::{Op, Pick, SimConfig, SimExplorer, Shown, default_displays};
use esr_sim::restore::{CommandKind, Driver, Launch, Stop, home_location};

const DIR: &str = r"C:\Users\User\AppData\Local\ExplorerSessionRestore\deferred";

fn folder(w: u32, i: usize) -> LocationId {
    LocationId {
        case_sensitive: false,
        kind: LocationKind::FileSystem(Volume::Fixed),
        parsing_name: format!(r"C:\esr\w{w}\t{i:03}").as_str().into(),
        display_name: format!("w{w}t{i:03}").as_str().into(),
        pidl: vec![1, 0x14, 0x1F, 7],
        search: None,
    }
}

fn window(id: u32, tabs: usize, active: usize, left: i32) -> WindowRec {
    let d = &default_displays()[0];
    WindowRec {
        id: WindowId(id),
        tabs: (0..tabs).map(|i| TabRec { location: folder(id, i) }).collect(),
        active,
        placement: Placement {
            state: ShowState::Normal,
            restored: Rect { left, top: 40, right: left + 800, bottom: 640 },
            display: DisplayRef { device: d.device.clone(), monitor: d.monitor },
        },
    }
}

fn doc(snap: &Snapshot) -> SessionDoc {
    SessionDoc {
        version: esr_core::format::FORMAT_VERSION,
        status: Status::Pending,
        failed_attempts: 0,
        saved_at: Timestamp { year: 2025, month: 3, day: 4, hour: 5, minute: 6, second: 7, millis: 8, offset_minutes: 0 },
        cause: SaveCause::LastWindowClosed,
        snapshot: snap.clone(),
    }
}

/// Runs a Default_Launch restoration of `snap` through the Engine with
/// placeholders in [`DIR`]; returns the Engine, the driver (still running)
/// and the trigger.
fn restore(snap: &Snapshot) -> (Engine, Driver, WinKey) {
    let mut sim = SimExplorer::new(SimConfig::default());
    sim.launch(&Launch {
        location: home_location(),
        selection: vec![],
        rect: Rect { left: 300, top: 300, right: 1100, bottom: 900 },
        maximized: false,
    });
    sim.settle();
    let (trigger, tab) = (sim.windows()[0].key, sim.windows()[0].tabs[0].key);
    let startup = StartupInput {
        session_file: SessionFileRead::Content(esr_core::format::serialize(&doc(snap)).into_bytes()),
        environment: Environment {
            displays: default_displays(),
            placeholder_dir: Some(DIR.into()),
            ..Environment::default()
        },
        background_logon: false,
        logon_at: None,
        shell_ready_at: Some(Millis(0)),
    };
    let (mut engine, commands) = Engine::new(Settings::default(), startup);
    let mut driver = Driver::new(sim);
    for owner in [TimerOwner::Tracker, TimerOwner::Store, TimerOwner::Control] {
        driver.quiet_timers(owner);
    }
    driver.submit(commands);
    driver.sim_mut().request_rescan(trigger);
    assert_eq!(driver.run(|now, ev| engine.handle(now, ev)), Stop::Quiescent);
    let params = RestoreParams {
        snapshot: snap.clone(),
        launch: LaunchInfo::Default,
        displays: default_displays(),
        trigger,
        trigger_tab: Some(tab),
        trigger_location: Some(home_location()),
        requested_selection: vec![],
        attempt_write_seq: 0,
        first_id: 0,
    };
    let now = driver.sim().now();
    let cmds = engine.start_restoration(now, params);
    driver.submit(cmds);
    assert_eq!(driver.run(|now, ev| engine.handle(now, ev)), Stop::Quiescent);
    (engine, driver, trigger)
}

fn is_placeholder(l: &LocationId) -> bool {
    l.kind == LocationKind::Shell(ShellKind::Library) && l.parsing_name.to_string_lossy().starts_with(DIR)
}

fn shown(sim: &SimExplorer, win: WinKey) -> Vec<LocationId> {
    sim.window(win)
        .unwrap()
        .tabs
        .iter()
        .map(|t| match &t.shown {
            Shown::Known(l) => l.clone(),
            other => panic!("{other:?}"),
        })
        .collect()
}

#[test]
fn tabs_beyond_the_budget_are_deferred_tracked_by_saved_location_and_navigated_when_activated() {
    // 60 + 50 distinct folders: actives first (w1 t000, w2 t000), then w1
    // left to right, then w2 up to the budget; w2 t040..t049 are deferred.
    let snap = Snapshot { windows: vec![window(1, 60, 0, 100), window(2, 50, 0, 1000)], activation: vec![WindowId(1), WindowId(2)] };
    let (engine, mut driver, trigger) = restore(&snap);
    let session = engine.restore_session().unwrap();
    assert_eq!(session.phase(), &Phase::Completed);
    let other = session.window_for(WindowId(2)).unwrap();
    let deferred: Vec<usize> = (40..50).collect();
    assert_eq!(session.deferred_tabs().iter().map(|t| (t.window.0, t.tab)).collect::<Vec<_>>(),
        deferred.iter().map(|&i| (2, i)).collect::<Vec<_>>());

    // Logged once with the counts; the RESTORE entry counts deferred tabs as restored.
    let sim = driver.sim();
    let logs: Vec<&LogEntry> = sim.log_entries().iter().map(|(_, e)| e).collect();
    let deferral: Vec<_> = logs.iter().filter(|e| matches!(e, LogEntry::TabsDeferred { .. })).collect();
    assert_eq!(
        deferral,
        [&&LogEntry::TabsDeferred { distinct: 110, immediate: IMMEDIATE_FOLDER_BUDGET as u32, deferred: 10, budget: 100 }]
    );
    assert!(logs.iter().any(|e| matches!(e, LogEntry::Restore { windows: 2, tabs: 110, omitted: 0, .. })));

    // Every tab is in place: saved Locations, and placeholders named after
    // the saved folder for the deferred ones (in their saved positions).
    assert_eq!(shown(sim, trigger), snap.windows[0].tabs.iter().map(|t| t.location.clone()).collect::<Vec<_>>());
    let w2 = shown(sim, other);
    for (i, l) in w2.iter().enumerate() {
        if deferred.contains(&i) {
            assert!(is_placeholder(l), "{i}: {l:?}");
            assert_eq!(l.display_name.to_string_lossy(), format!("w2t{i:03}"));
        } else {
            assert_eq!(l, &folder(2, i));
        }
    }
    let names: Vec<String> = sim.tab_names(other).iter().map(|n| n.to_string_lossy()).collect();
    assert_eq!(names[45], "w2t045");

    // No more than the budget of distinct real folders was opened.
    let opened: HashSet<String> = sim
        .command_log()
        .iter()
        .filter_map(|c| match &c.command {
            Command::Navigate { location, .. } | Command::OpenWindow { location, .. } => Some(location),
            _ => None,
        })
        .filter(|l| !is_placeholder(l))
        .map(|l| l.parsing_name.to_string_lossy())
        .collect();
    assert_eq!(opened.len(), IMMEDIATE_FOLDER_BUDGET);

    // The Live_Session holds the saved Locations, never a placeholder.
    let (live, _) = engine.tracker().capture();
    let mut live_locs: Vec<String> =
        live.windows.iter().flat_map(|w| &w.tabs).map(|t| t.location.parsing_name.to_string_lossy()).collect();
    let mut saved: Vec<String> =
        snap.windows.iter().flat_map(|w| &w.tabs).map(|t| t.location.parsing_name.to_string_lossy()).collect();
    live_locs.sort();
    saved.sort();
    assert_eq!(live_locs, saved);

    // The user activates deferred tab 45: the Engine navigates it to its
    // saved Location; the other deferred tabs keep their placeholders.
    let mut engine = engine;
    let pick = driver.sim().windows().iter().position(|w| w.key == other).unwrap();
    let events = driver.sim_mut().apply(&Op::ActivateTab { win: Pick(pick as u16), tab: Pick(45) });
    driver.deliver(events);
    assert_eq!(driver.run(|now, ev| engine.handle(now, ev)), Stop::Quiescent);
    let sim = driver.sim();
    let navs: Vec<_> = sim
        .commands_of(CommandKind::Navigate)
        .into_iter()
        .filter_map(|c| match &c.command {
            Command::Navigate { op, location, .. } if op.0 >= DEFERRED_OP_BASE => Some(location.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(navs, [folder(2, 45)]);
    let w2 = shown(sim, other);
    assert_eq!(w2[45], folder(2, 45));
    assert!(deferred.iter().filter(|&&i| i != 45).all(|&i| is_placeholder(&w2[i])));
    assert!(engine.tracker().capture().0.windows.iter().flat_map(|w| &w.tabs).all(|t| !is_placeholder(&t.location)));

    // Activating it again (it now shows its folder) navigates nothing more.
    let events = driver.sim_mut().apply(&Op::ActivateTab { win: Pick(pick as u16), tab: Pick(0) });
    driver.deliver(events);
    let events = driver.sim_mut().apply(&Op::ActivateTab { win: Pick(pick as u16), tab: Pick(45) });
    driver.deliver(events);
    assert_eq!(driver.run(|now, ev| engine.handle(now, ev)), Stop::Quiescent);
    let deferred_navs = driver
        .sim()
        .commands_of(CommandKind::Navigate)
        .into_iter()
        .filter(|c| matches!(&c.command, Command::Navigate { op, .. } if op.0 >= DEFERRED_OP_BASE))
        .count();
    assert_eq!(deferred_navs, 1);
}

#[test]
fn within_the_budget_nothing_is_deferred() {
    let snap = Snapshot { windows: vec![window(1, 30, 3, 100), window(2, 30, 0, 1000)], activation: vec![WindowId(2), WindowId(1)] };
    let (engine, driver, _) = restore(&snap);
    let session = engine.restore_session().unwrap();
    assert_eq!(session.phase(), &Phase::Completed);
    assert!(!session.has_deferred());
    assert!(!driver.sim().log_entries().iter().any(|(_, e)| matches!(e, LogEntry::TabsDeferred { .. })));
    assert!(driver.sim().command_log().iter().all(|c| match &c.command {
        Command::Navigate { location, .. } | Command::OpenWindow { location, .. } => !is_placeholder(location),
        _ => true,
    }));
}
