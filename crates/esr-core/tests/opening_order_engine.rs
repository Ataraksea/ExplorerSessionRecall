//! The `Engine` keeps the Opening_Order in step with a completed restoration
//! (task 22.5), end to end against the simulated Explorer. The simulator
//! reports no `WindowAppeared` for windows that are not Folding_Candidates, so
//! the harness announces every window just before the first event that names
//! it, as the Windows adapter's first signal does.
//!
//! Requirements 14.8, 14.9.

use std::collections::BTreeSet;

use esr_core::engine::{
    Engine, Environment, Event, ExplorerOptionValues, Millis, SessionFileRead, StartupInput, WinKey,
};
use esr_core::model::{
    DisplayRef, Placement, Rect, SaveCause, SessionDoc, ShowState, Snapshot, Status, TabRec, Timestamp, WindowId,
    WindowRec,
};
use esr_core::settings::Settings;
use esr_core::engine::DesktopView;
use esr_sim::explorer::{default_displays, default_locations, SimConfig, SimExplorer};
use esr_sim::restore::{home_location, Driver, Launch, Origin, Stop, UserAction};

/// A window the user opens during the restoration.
const OPENED_MEANWHILE: WinKey = WinKey { hwnd: 0xFFFF_0001, seq: 0xFFFF_0001 };

fn appeared(win: WinKey) -> Event {
    Event::WindowAppeared { win, pid: 1, bypass_keys: false, desktop: DesktopView::Unknown, foreground: None }
}

fn rect(left: i32, top: i32) -> Rect {
    Rect { left, top, right: left + 800, bottom: top + 600 }
}

fn window(id: u32, location: usize, left: i32) -> WindowRec {
    let d = &default_displays()[usize::from(left >= 1920)];
    WindowRec {
        id: WindowId(id),
        tabs: vec![TabRec { location: default_locations()[location].clone() }],
        active: 0,
        placement: Placement {
            state: ShowState::Normal,
            restored: rect(left, 40),
            display: DisplayRef { device: d.device.clone(), monitor: d.monitor },
        },
    }
}

/// A Pending_Snapshot of three windows whose saved Activation_Order is
/// `activation` (most recently activated first).
fn pending_file(activation: &[u32]) -> String {
    let windows = vec![window(1, 2, 100), window(2, 3, 600), window(3, 4, 1100)];
    esr_core::format::serialize(&SessionDoc {
        version: esr_core::format::FORMAT_VERSION,
        status: Status::Pending,
        failed_attempts: 0,
        saved_at: Timestamp { year: 2025, month: 3, day: 4, hour: 5, minute: 6, second: 7, millis: 8, offset_minutes: 0 },
        cause: SaveCause::LastWindowClosed,
        snapshot: Snapshot { windows, activation: activation.iter().map(|&i| WindowId(i)).collect() },
    })
}

/// Runs one restoration with the harness announcing each window first, and
/// returns the engine and the Trigger_Window.
fn restore(activation: &[u32]) -> (Engine, WinKey) {
    let startup = StartupInput {
        session_file: SessionFileRead::Content(pending_file(activation).into_bytes()),
        environment: Environment {
            displays: default_displays(),
            options: ExplorerOptionValues { persist_browsers: Some(false), ..ExplorerOptionValues::default() },
            ..Environment::default()
        },
        background_logon: false,
        logon_at: None,
        shell_ready_at: Some(Millis(0)),
    };
    let (mut engine, commands) = Engine::new(Settings::default(), startup);
    let mut driver = Driver::new(SimExplorer::new(SimConfig::for_restoration()));
    driver.submit(commands);
    let mut announced: BTreeSet<WinKey> = BTreeSet::new();
    let mut run = |driver: &mut Driver, engine: &mut Engine| {
        let stop = driver.run(|now, event| {
            let win = match &event {
                Event::WindowScanned(obs) => Some(obs.win),
                Event::WindowOpened { win, .. } | Event::InitialLocationKnown { win, .. } => Some(*win),
                _ => None,
            };
            let mut out = Vec::new();
            if let Some(win) = win.filter(|w| announced.insert(*w)) {
                out.extend(engine.handle(now, appeared(win)));
                // The user opens another window right after the Trigger_Window,
                // while the restoration runs.
                if announced.insert(OPENED_MEANWHILE) {
                    out.extend(engine.handle(now, appeared(OPENED_MEANWHILE)));
                }
            }
            out.extend(engine.handle(now, event));
            out
        });
        assert_eq!(stop, Stop::Quiescent);
    };
    run(&mut driver, &mut engine);
    let at = driver.sim().now() + 1_000;
    driver.schedule(
        at,
        UserAction::Launch(Launch { location: home_location(), selection: vec![], rect: rect(300, 300), maximized: false }),
    );
    run(&mut driver, &mut engine);
    let records = driver.sim().window_records();
    let user: Vec<WinKey> = records.iter().filter(|r| matches!(r.origin, Origin::User)).map(|r| r.key).collect();
    (engine, user[0])
}

#[test]
fn a_completed_restoration_ranks_the_trigger_then_the_restored_windows_in_saved_activation_order() {
    for activation in [[1, 2, 3], [3, 2, 1], [2, 3, 1]] {
        let (engine, trigger) = restore(&activation);
        assert!(engine.restore_session().is_some_and(|s| s.is_finished() && s.trigger() == trigger));
        let order = engine.folding().opening().to_vec();
        let restored = engine.tracker().activation_order();
        assert_eq!(restored.len(), 3, "{activation:?}");
        // The restored windows' Activation_Order is the saved one; the order
        // is the trigger first, then the others in that Activation_Order.
        let mut expected = vec![trigger];
        expected.extend(restored.iter().copied().filter(|w| *w != trigger));
        // The window opened meanwhile ranks behind all of them.
        expected.push(OPENED_MEANWHILE);
        assert_eq!(order, expected, "saved activation {activation:?}");
    }
}
