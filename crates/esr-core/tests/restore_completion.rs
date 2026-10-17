//! Restoration completion and failure handling (task 11.4): `RestoreSession`
//! and the Engine hook, driven against the simulated Explorer.
//!
//! Requirements 4.4, 4.5, 4.10, 5.4, 5.7, 5.10, 5.11, 7.4, 9.4, 9.7, 9.8,
//! 13.2, 13.3.

use esr_core::engine::{
    Command, Engine, Environment, Millis, SessionFileRead, StartupInput, TabKey, TimerOwner, WinKey,
};
use esr_core::logfmt::entry::LogEntry;
use esr_core::model::{
    DisplayRef, LocationId, Placement, Rect, SaveCause, SessionDoc, ShowState, Snapshot, Status, TabRec, Timestamp,
    WindowId, WindowRec,
};
use esr_core::plan::fallback::{ancestors, AncestorProbe, OmitReason, ProbeOutcome};
use esr_core::plan::LaunchInfo;
use esr_core::restore::{
    Completion, Failure, OpKind, Phase, RestoreOutcome, RestoreParams, RestoreSession, TAB_CREATE_TIMEOUT_MS,
};
use esr_core::settings::Settings;
use esr_sim::explorer::{default_displays, default_locations, SimConfig, SimExplorer};
use esr_sim::restore::{
    home_location, CommandKind, Driver, FailWhen, FailureRule, Launch, Origin, RecordedCommand, Run, Stop, TabLoss,
};

const SEQ: u64 = 91;

const DOCUMENTS: usize = 2;
const DOWNLOADS: usize = 3;
const PHOTOS: usize = 4;
const BACKUP: usize = 5;
const MAPPED: usize = 6;
const UNC: usize = 7;
const ZIP: usize = 8;
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

fn window_in(id: u32, tabs: &[usize], active: usize, left: i32, state: ShowState) -> WindowRec {
    let d = &default_displays()[usize::from(left >= 1920)];
    WindowRec {
        id: WindowId(id),
        tabs: tabs.iter().map(|&i| TabRec { location: loc(i) }).collect(),
        active,
        placement: Placement {
            state,
            restored: rect(left, 40),
            display: DisplayRef { device: d.device.clone(), monitor: d.monitor },
        },
    }
}

fn window(id: u32, tabs: &[usize], active: usize, left: i32) -> WindowRec {
    window_in(id, tabs, active, left, ShowState::Normal)
}

fn snapshot(windows: Vec<WindowRec>) -> Snapshot {
    let activation = windows.iter().map(|w| w.id).collect();
    Snapshot { windows, activation }
}

/// A sim with a settled trigger on Home (a Default_Launch).
fn sim_with_trigger(config: SimConfig) -> (SimExplorer, WinKey, TabKey) {
    let mut sim = SimExplorer::new(config);
    sim.launch(&Launch { location: home_location(), selection: vec![], rect: rect(300, 300), maximized: false });
    sim.settle();
    let w = &sim.windows()[0];
    let (win, tab) = (w.key, w.tabs[0].key);
    (sim, win, tab)
}

fn params(snap: Snapshot, trigger: WinKey, tab: TabKey) -> RestoreParams {
    RestoreParams {
        snapshot: snap,
        launch: LaunchInfo::Default,
        displays: default_displays(),
        trigger,
        trigger_tab: Some(tab),
        trigger_location: Some(home_location()),
        requested_selection: vec![],
        attempt_write_seq: SEQ,
        first_id: 3000,
    }
}

struct Outcome {
    session: RestoreSession,
    outcome: Option<RestoreOutcome>,
    run: Run,
    trigger: WinKey,
    started: Millis,
}

/// Runs a session (no Engine) over `snap`; collects its outcome.
fn restore_cfg(config: SimConfig, snap: Snapshot, setup: impl FnOnce(&mut SimExplorer)) -> Outcome {
    let (mut sim, trigger, tab) = sim_with_trigger(config);
    setup(&mut sim);
    let started = sim.now();
    let mut session = RestoreSession::new(started, params(snap, trigger, tab));
    let mut driver = Driver::new(sim);
    driver.submit([Command::WriteSessionFile { seq: SEQ, text: "failed-attempts 1".into(), urgent: true }]);
    let mut outcome = None;
    let stop = driver.run(|now, ev| {
        let out = session.handle(now, &ev);
        if let Some(o) = session.take_outcome() {
            assert!(outcome.is_none(), "the outcome is reported once");
            outcome = Some(o);
        }
        out
    });
    let run = driver.finish(stop);
    assert_eq!(run.stop, Stop::Quiescent);
    Outcome { session, outcome, run, trigger, started }
}

fn restore(snap: Snapshot, setup: impl FnOnce(&mut SimExplorer)) -> Outcome {
    restore_cfg(SimConfig::default(), snap, setup)
}

fn completion(o: &Outcome) -> &Completion {
    match &o.outcome {
        Some(RestoreOutcome::Completed(c)) => c,
        other => panic!("not completed: {other:?} / {:?}", o.session.phase()),
    }
}

fn shown(sim: &SimExplorer, win: WinKey) -> Vec<LocationId> {
    sim.window(win).expect("open").tabs.iter().map(|t| t.last_known.clone().expect("navigated")).collect()
}

fn opened(sim: &SimExplorer) -> Vec<WinKey> {
    sim.window_records()
        .iter()
        .filter(|r| matches!(r.origin, Origin::Request(_)) && r.closed_at.is_none())
        .map(|r| r.key)
        .collect()
}

fn logs(sim: &SimExplorer) -> Vec<&LogEntry> {
    sim.log_entries().iter().map(|(_, e)| e).collect()
}

fn position(log: &[RecordedCommand], pred: impl Fn(&Command) -> bool) -> Option<usize> {
    log.iter().position(|c| pred(&c.command))
}

#[test]
fn completion_activates_then_reveals_orders_focuses_and_logs() {
    let snap = snapshot(vec![window(1, &[DOCUMENTS, DOWNLOADS, PHOTOS], 1, 100), window(2, &[THIS_PC, RECYCLE], 1, 2000)]);
    let o = restore(snap.clone(), |_| {});
    let sim = &o.run.sim;
    let c = completion(&o);
    assert_eq!(o.session.phase(), &Phase::Completed);
    let others = opened(sim);
    assert_eq!(others.len(), 1);
    let other = others[0];

    // Saved Active_Tabs (Req 5.4).
    assert_eq!(sim.window(o.trigger).unwrap().active, 1);
    assert_eq!(sim.window(other).unwrap().active, 1);
    assert_eq!(shown(sim, other), locs(&[THIS_PC, RECYCLE]));

    // Activate, then Reveal (opened windows only), SetZOrder, Focus.
    let log = sim.command_log();
    let last_activate = log.iter().rposition(|c| matches!(c.command, Command::ActivateTab { .. })).unwrap();
    let reveal = position(log, |c| matches!(c, Command::Reveal { .. })).unwrap();
    let z = position(log, |c| matches!(c, Command::SetZOrder(_))).unwrap();
    let focus = position(log, |c| matches!(c, Command::Focus(_))).unwrap();
    assert!(last_activate < reveal && reveal < z && z < focus, "{log:#?}");
    assert_eq!(sim.commands_of(CommandKind::Reveal).len(), 1);
    assert!(matches!(log[reveal].command, Command::Reveal { win } if win == other));
    assert!(matches!(&log[z].command, Command::SetZOrder(order) if *order == vec![o.trigger, other]));
    // No restoration command after completion.
    assert!(log[focus + 1..].iter().all(|c| matches!(
        c.command,
        Command::AppendLog(_) | Command::Notify { .. } | Command::SetTimer { .. } | Command::RescanWindow(_)
    )));

    // The restored window first became visible at its final placement and
    // never moved (Req 7.2); z-order and focus (Req 5.7, 5.10).
    let rec = sim.window_record(other).unwrap();
    let (_, first) = rec.first_visible.clone().expect("revealed");
    assert_eq!(first.restored, snap.windows[1].placement.restored);
    assert_eq!(rec.visible_placement_changes, 0);
    assert_eq!(&sim.z_order()[..2], &[o.trigger, other]);
    assert_eq!(sim.focused(), Some(o.trigger));

    // One RESTORE entry (Req 13.2), nothing omitted, no toast (Req 7.4).
    let done_at = log[focus].at;
    assert_eq!(
        logs(sim),
        vec![&LogEntry::Restore {
            targeted: false,
            windows: 2,
            tabs: 5,
            omitted: 0,
            redirected: 0,
            duration_ms: done_at.since(o.started),
        }]
    );
    assert!(sim.notifications().is_empty());
    assert_eq!(
        (c.windows, c.tabs, c.omitted, c.redirected, c.z_order.clone(), c.focus),
        (2, 5, 0, 0, vec![o.trigger, other], Some(o.trigger))
    );
    assert_eq!(o.session.outstanding_ops().count(), 0);
}

#[test]
fn omitted_and_redirected_tabs_are_logged_and_one_toast_counts_omitted_windows() {
    // Window 1: Documents, ZIP (missing → its .zip), UNC (no response).
    // Window 2: Mapped only (access denied) → omitted window.
    let snap = snapshot(vec![window(1, &[DOCUMENTS, ZIP, UNC], 2, 100), window(2, &[MAPPED], 0, 2000)]);
    let zip_ancestors = ancestors(&loc(ZIP));
    let probes = zip_ancestors.iter().map(|a| AncestorProbe { location: a.clone(), available: true }).collect();
    let o = restore(snap, |s| {
        s.set_probe(loc(ZIP), ProbeOutcome::Missing { ancestors: probes }, 20);
        s.set_probe(loc(UNC), ProbeOutcome::Timeout, 1500);
        s.set_probe(loc(MAPPED), ProbeOutcome::AccessDenied, 30);
    });
    let sim = &o.run.sim;
    let c = completion(&o);
    assert_eq!(shown(sim, o.trigger), vec![loc(DOCUMENTS), zip_ancestors[0].clone()]);
    assert!(opened(sim).is_empty());
    // The saved Active_Tab (UNC) was omitted: the nearest tab to its left (Req 9.4).
    assert_eq!(sim.window(o.trigger).unwrap().active, 1);

    let entries = logs(sim);
    assert!(matches!(entries[0], LogEntry::Restore { windows: 1, tabs: 2, omitted: 2, redirected: 1, .. }));
    assert_eq!(
        entries[1..],
        [
            &LogEntry::TabRedirected { location: loc(ZIP), reason: OmitReason::NotExist, ancestor: zip_ancestors[0].clone() },
            &LogEntry::TabOmitted { location: loc(UNC), reason: OmitReason::Timeout },
            &LogEntry::TabOmitted { location: loc(MAPPED), reason: OmitReason::Inaccessible },
        ]
    );
    // Exactly one toast with every omitted tab, at completion (Req 9.8).
    let focus_at = sim.commands_of(CommandKind::Focus)[0].at;
    assert_eq!(sim.notifications(), &[(focus_at, 2)]);
    assert_eq!((c.omitted, c.redirected), (2, 1));
}

#[test]
fn redirected_tabs_alone_show_no_notification() {
    let snap = snapshot(vec![window(1, &[ZIP, BACKUP], 0, 100)]);
    let zip_ancestors = ancestors(&loc(ZIP));
    let probes = zip_ancestors.iter().map(|a| AncestorProbe { location: a.clone(), available: true }).collect();
    let o = restore(snap, |s| s.set_probe(loc(ZIP), ProbeOutcome::Missing { ancestors: probes }, 20));
    let sim = &o.run.sim;
    completion(&o);
    assert!(sim.notifications().is_empty(), "Req 7.4");
    assert_eq!(logs(sim).iter().filter(|e| matches!(e, LogEntry::TabRedirected { .. })).count(), 1);
}

#[test]
fn focus_goes_to_the_first_non_minimized_window() {
    let minimized = ShowState::Minimized { restore_maximized: false };
    let snap = snapshot(vec![
        window_in(1, &[DOCUMENTS], 0, 100, minimized),
        window(2, &[PHOTOS], 0, 2000),
        window(3, &[BACKUP], 0, 200),
    ]);
    let o = restore(snap, |_| {});
    let sim = &o.run.sim;
    let c = completion(&o);
    let others = opened(sim);
    assert_eq!(others.len(), 2);
    let second = o.session.window_for(WindowId(2)).unwrap();
    let third = o.session.window_for(WindowId(3)).unwrap();
    // Z-order is the saved Activation_Order (Req 5.7); focus skips the
    // minimized first window (Req 5.10).
    assert_eq!(c.z_order, vec![o.trigger, second, third]);
    assert_eq!(c.focus, Some(second));
    assert_eq!(sim.focused(), Some(second));
    assert!(matches!(sim.window(o.trigger).unwrap().placement.state, ShowState::Minimized { .. }));
}

#[test]
fn all_minimized_shows_and_focuses_the_first_window() {
    let minimized = ShowState::Minimized { restore_maximized: false };
    let snap = snapshot(vec![window_in(1, &[DOCUMENTS], 0, 100, minimized), window_in(2, &[PHOTOS], 0, 2000, minimized)]);
    let o = restore(snap, |_| {});
    let sim = &o.run.sim;
    let c = completion(&o);
    assert_eq!(c.focus, Some(o.trigger));
    assert_eq!(sim.focused(), Some(o.trigger));
    assert_eq!(sim.window(o.trigger).unwrap().placement.state, ShowState::Normal, "Req 5.11");
    let other = o.session.window_for(WindowId(2)).unwrap();
    assert!(matches!(sim.window(other).unwrap().placement.state, ShowState::Minimized { .. }));
}

#[test]
fn an_operation_failure_stops_logs_and_reveals_the_opened_windows() {
    // The second window needs a new tab; creating it fails.
    let snap = snapshot(vec![window(1, &[DOCUMENTS], 0, 100), window(2, &[PHOTOS, BACKUP], 0, 2000)]);
    let o = restore(snap, |s| s.fail(FailureRule::always(FailWhen::Kind(CommandKind::CreateTab))));
    let sim = &o.run.sim;
    assert!(matches!(
        &o.outcome,
        Some(RestoreOutcome::Failed(Failure::OperationFailed { kind: OpKind::CreateTab, .. }))
    ));
    // Stopped: nothing completes, and the opened window stays open, now visible.
    let others = opened(sim);
    assert_eq!(others.len(), 1);
    assert!(sim.is_visible(others[0]));
    assert_eq!(shown(sim, others[0]), locs(&[PHOTOS]));
    assert_eq!(sim.commands_of(CommandKind::CreateTab).len(), 1, "no retry of a failed operation");
    assert!(sim.commands_of(CommandKind::SetZOrder).is_empty() && sim.commands_of(CommandKind::Focus).is_empty());
    assert!(sim.notifications().is_empty());
    // One ERROR entry with the operation and a description (Req 13.3).
    let entries = logs(sim);
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert!(matches!(entries[0], LogEntry::Error { operation, description }
        if operation == "restore-create-tab" && description.contains("injected failure")));
}

#[test]
fn a_tab_that_never_appears_times_out() {
    // Every post is lost and nobody retries: the tab never registers.
    let mut config = SimConfig::default();
    config.restore.lost_tabs = TabLoss::EveryNth(1);
    config.restore.navigator_repost = false;
    let o = restore_cfg(config, snapshot(vec![window(1, &[DOCUMENTS, DOWNLOADS], 0, 100)]), |_| {});
    let sim = &o.run.sim;
    let Some(RestoreOutcome::Failed(Failure::Timeout { kind: OpKind::CreateTab, after_ms, .. })) = &o.outcome else {
        panic!("{:?}", o.outcome)
    };
    assert_eq!(*after_ms, TAB_CREATE_TIMEOUT_MS);
    let created = sim.commands_of(CommandKind::CreateTab)[0].at;
    let (at, entry) = sim.log_entries().last().unwrap();
    assert_eq!(*at, created + TAB_CREATE_TIMEOUT_MS, "failed when the Restore timer fired");
    assert!(matches!(entry, LogEntry::Error { operation, .. } if operation == "restore-create-tab"));
    let timers = sim.commands_of(CommandKind::SetTimer);
    assert!(timers.iter().all(|c| matches!(c.command, Command::SetTimer { id, .. } if id.owner == TimerOwner::Restore)));
    // The trigger keeps what it has (Req 4.10).
    assert_eq!(shown(sim, o.trigger), locs(&[DOCUMENTS]));
}

#[test]
fn a_lost_post_retried_by_the_navigator_still_completes() {
    let mut config = SimConfig::default();
    config.restore.lost_tabs = TabLoss::Posts([0].into());
    let o = restore_cfg(config, snapshot(vec![window(1, &[DOCUMENTS, DOWNLOADS, PHOTOS], 2, 100)]), |_| {});
    let c = completion(&o);
    assert_eq!(shown(&o.run.sim, o.trigger), locs(&[DOCUMENTS, DOWNLOADS, PHOTOS]));
    assert_eq!(o.run.sim.commands_of(CommandKind::CreateTab).len(), 2, "the core never re-posts");
    assert!(c.duration_ms > 1500);
}

#[test]
fn a_window_arriving_after_a_failure_is_revealed() {
    // The trigger's navigation fails at once while the second window is
    // still opening (≈430 ms).
    let snap = snapshot(vec![window(1, &[DOCUMENTS], 0, 100), window(2, &[PHOTOS], 0, 2000)]);
    let o = restore(snap, |s| s.fail(FailureRule::always(FailWhen::Kind(CommandKind::Navigate))));
    let sim = &o.run.sim;
    assert!(matches!(&o.outcome, Some(RestoreOutcome::Failed(Failure::OperationFailed { kind: OpKind::Navigate, .. }))));
    let failed_at = sim.log_entries()[0].0;
    let others = opened(sim);
    assert_eq!(others.len(), 1);
    let reveal = &sim.commands_of(CommandKind::Reveal)[0];
    assert!(matches!(reveal.command, Command::Reveal { win } if win == others[0]));
    assert!(reveal.at > failed_at);
    assert!(sim.is_visible(others[0]));
    assert!(o.session.owns_window(others[0]));
}

// ---------------------------------------------------------------------------
// Engine hook
// ---------------------------------------------------------------------------

fn pending_doc(snap: &Snapshot) -> SessionDoc {
    SessionDoc {
        version: esr_core::format::FORMAT_VERSION,
        status: Status::Pending,
        failed_attempts: 0,
        saved_at: Timestamp { year: 2025, month: 3, day: 4, hour: 5, minute: 6, second: 7, millis: 8, offset_minutes: 0 },
        cause: SaveCause::LastWindowClosed,
        snapshot: snap.clone(),
    }
}

/// An Engine with `snap` pending, a tracked trigger, and a restoration run.
fn engine_run(snap: Snapshot, setup: impl FnOnce(&mut SimExplorer)) -> (Engine, Run, WinKey) {
    let (mut sim, trigger, tab) = sim_with_trigger(SimConfig::default());
    setup(&mut sim);
    let text = esr_core::format::serialize(&pending_doc(&snap));
    let startup = StartupInput {
        session_file: SessionFileRead::Content(text.into_bytes()),
        environment: Environment { displays: default_displays(), ..Environment::default() },
        background_logon: false,
        logon_at: None,
        shell_ready_at: Some(Millis(0)),
    };
    let (mut engine, commands) = Engine::new(Settings::default(), startup);
    assert!(engine.store().pending_snapshot().is_some());
    let mut driver = Driver::new(sim);
    for owner in [TimerOwner::Tracker, TimerOwner::Store, TimerOwner::Control] {
        driver.quiet_timers(owner);
    }
    driver.submit(commands);
    // The tracker learns the trigger.
    driver.sim_mut().request_rescan(trigger);
    assert_eq!(driver.run(|now, ev| engine.handle(now, ev)), Stop::Quiescent);
    assert_eq!(engine.tracker().window_count(), 1);

    // Task 11.5 calls this once its trigger checks pass.
    let now = driver.sim().now();
    let cmds = engine.start_restoration(now, params(snap, trigger, tab));
    assert!(matches!(cmds.as_slice(), [Command::WriteSessionFile { urgent: true, .. }]), "{cmds:?}");
    assert!(engine.store().is_restoring());
    driver.submit(cmds);
    let stop = driver.run(|now, ev| engine.handle(now, ev));
    assert_eq!(stop, Stop::Quiescent);
    (engine, driver.finish(stop), trigger)
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

#[test]
fn engine_consumes_the_snapshot_and_rebuilds_the_live_session_on_completion() {
    let snap = snapshot(vec![window(1, &[DOCUMENTS, DOWNLOADS], 0, 100), window(2, &[PHOTOS], 0, 2000)]);
    let (engine, run, trigger) = engine_run(snap.clone(), |_| {});
    let session = engine.restore_session().unwrap();
    assert_eq!(session.phase(), &Phase::Completed);
    let other = session.window_for(WindowId(2)).unwrap();

    // Attempt counted first (failed_attempts 1), then Consumed at completion
    // (Req 4.4), keeping the snapshot content.
    let w = writes(&run);
    assert_eq!((w[0].1.status, w[0].1.failed_attempts), (Status::Pending, 1));
    let focus_at = run.sim.commands_of(CommandKind::Focus)[0].at;
    let consumed: Vec<_> = w.iter().filter(|(_, d)| d.status == Status::Consumed).collect();
    assert_eq!(consumed.len(), 1);
    assert_eq!(consumed[0].0, focus_at);
    assert_eq!(consumed[0].1.snapshot, snap);
    assert!(!engine.store().is_restoring());
    assert!(engine.store().pending_snapshot().is_none_or(|d| d.cause == SaveCause::Checkpoint));

    // A full rescan follows completion; the Live_Session holds both windows
    // in the planned Activation_Order (Req 4.5).
    let log = &run.commands;
    let focus = log.iter().position(|c| matches!(c.command, Command::Focus(_))).unwrap();
    let rescan_all = log.iter().position(|c| matches!(c.command, Command::RescanAll)).unwrap();
    assert!(focus < rescan_all);
    assert_eq!(engine.tracker().activation_order(), vec![trigger, other]);
    assert_eq!(engine.tracker().capture().0.tab_count(), 3);
    assert!(run.sim.log_entries().iter().any(|(_, e)| matches!(e, LogEntry::Restore { windows: 2, tabs: 3, .. })));
}

#[test]
fn engine_keeps_the_counted_attempt_and_protection_on_failure() {
    let snap = snapshot(vec![window(1, &[DOCUMENTS], 0, 100), window(2, &[PHOTOS, BACKUP], 0, 2000)]);
    let (engine, run, _) = engine_run(snap.clone(), |s| s.fail(FailureRule::always(FailWhen::Kind(CommandKind::CreateTab))));
    assert!(matches!(engine.restore_session().unwrap().phase(), Phase::Failed(_)));
    // Only the attempt write: the snapshot stays Pending with one failed
    // attempt (Req 4.10), protected from checkpoints.
    let w = writes(&run);
    assert_eq!(w.len(), 1, "{w:?}");
    assert_eq!((w[0].1.status, w[0].1.failed_attempts), (Status::Pending, 1));
    assert_eq!(w[0].1.snapshot, snap);
    assert!(engine.store().is_protected());
    assert!(!engine.store().is_restoring());
    assert!(run.sim.log_entries().iter().any(|(_, e)| matches!(e, LogEntry::Error { operation, .. } if operation == "restore-create-tab")));
    assert!(!run.commands.iter().any(|c| matches!(c.command, Command::RescanAll)));
}
