//! `RestoreSession` (task 11.2) driven against the simulated Explorer.
//!
//! Requirements 5.1, 5.3, 6.1, 7.2, 7.3, 7.5, 7.7, 7.9, 8.3, 9.1, 9.2.

use esr_core::engine::{Command, Event, Millis, TabKey, WinKey};
use esr_core::model::{DisplayRef, LocationId, Placement, Rect, ShowState, Snapshot, TabRec, WindowId, WindowRec};
use esr_core::plan::fallback::{ancestors, AncestorProbe, ProbeOutcome};
use esr_core::plan::LaunchInfo;
use esr_core::restore::{Failure, Phase, RestoreParams, RestoreSession};
use esr_sim::explorer::{default_displays, default_locations, SimConfig, SimExplorer};
use esr_sim::restore::{home_location, CommandKind, Driver, FailWhen, FailureRule, Launch, Origin, Run, Stop};

const SEQ: u64 = 77;

fn loc(i: usize) -> LocationId {
    default_locations()[i].clone()
}

const PROJECTS: usize = 0;
const DOCUMENTS: usize = 2;
const DOWNLOADS: usize = 3;
const PHOTOS: usize = 4;
const BACKUP: usize = 5;
const MAPPED: usize = 6;
const UNC: usize = 7;
const ZIP: usize = 8;
const THIS_PC: usize = 9;
const RECYCLE: usize = 11;

fn rect(left: i32, top: i32) -> Rect {
    Rect { left, top, right: left + 800, bottom: top + 600 }
}

fn window(id: u32, tabs: &[usize], active: usize, left: i32) -> WindowRec {
    let d = &default_displays()[usize::from(left >= 1920)];
    WindowRec {
        id: WindowId(id),
        tabs: tabs.iter().map(|&i| TabRec { location: loc(i) }).collect(),
        active,
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

/// A sim with a settled trigger on Home (a Default_Launch).
fn sim_with_trigger(config: SimConfig) -> (SimExplorer, WinKey, TabKey) {
    let mut sim = SimExplorer::new(config);
    sim.launch(&Launch { location: home_location(), selection: vec![], rect: rect(300, 300), maximized: false });
    sim.settle();
    let w = &sim.windows()[0];
    let (win, tab) = (w.key, w.tabs[0].key);
    (sim, win, tab)
}

struct Outcome {
    session: RestoreSession,
    run: Run,
    trigger: WinKey,
    started: Millis,
}

fn restore_with(mut sim: SimExplorer, trigger: WinKey, tab: TabKey, snap: Snapshot, setup: impl FnOnce(&mut SimExplorer)) -> Outcome {
    setup(&mut sim);
    let started = sim.now();
    let mut session = RestoreSession::new(
        started,
        RestoreParams {
            snapshot: snap,
            launch: LaunchInfo::Default,
            displays: default_displays(),
            trigger,
            trigger_tab: Some(tab),
            trigger_location: Some(home_location()),
            requested_selection: vec![],
            attempt_write_seq: SEQ,
            first_id: 1000,
        },
    );
    let mut driver = Driver::new(sim);
    driver.submit([Command::WriteSessionFile { seq: SEQ, text: "failed-attempts 1".into(), urgent: true }]);
    let stop = driver.run(|now, ev| session.handle(now, &ev));
    let run = driver.finish(stop);
    assert_eq!(run.stop, Stop::Quiescent);
    Outcome { session, run, trigger, started }
}

fn restore(snap: Snapshot, setup: impl FnOnce(&mut SimExplorer)) -> Outcome {
    let (sim, trigger, tab) = sim_with_trigger(SimConfig::default());
    restore_with(sim, trigger, tab, snap, setup)
}

fn shown(sim: &SimExplorer, win: WinKey) -> Vec<LocationId> {
    sim.window(win).expect("open").tabs.iter().map(|t| t.last_known.clone().expect("navigated")).collect()
}

fn locs(ids: &[usize]) -> Vec<LocationId> {
    ids.iter().map(|&i| loc(i)).collect()
}

/// Windows the session opened, in creation order.
fn opened(sim: &SimExplorer) -> Vec<WinKey> {
    sim.window_records()
        .iter()
        .filter(|r| matches!(r.origin, Origin::Request(_)) && r.closed_at.is_none())
        .map(|r| r.key)
        .collect()
}

fn count(run: &Run, kind: CommandKind) -> usize {
    run.sim.commands_of(kind).len()
}

fn is_window_command(c: &Command) -> bool {
    matches!(
        c,
        Command::OpenWindow { .. }
            | Command::ApplyPlacement { .. }
            | Command::CreateTab { .. }
            | Command::Navigate { .. }
            | Command::CloseTab { .. }
            | Command::ActivateTab { .. }
            | Command::SelectItems { .. }
            | Command::Reveal { .. }
            | Command::SetZOrder(_)
            | Command::Focus(_)
    )
}

fn event_time(run: &Run, pred: impl Fn(&Event) -> bool) -> Millis {
    run.events.iter().find(|e| pred(&e.event)).expect("event delivered").at
}

#[test]
fn all_local_default_launch_restores_every_window_and_tab() {
    let snap = snapshot(vec![window(1, &[DOCUMENTS, DOWNLOADS, PHOTOS], 1, 100), window(2, &[THIS_PC, RECYCLE], 0, 2000)]);
    let o = restore(snap.clone(), |_| {});
    let sim = &o.run.sim;
    assert!(o.session.is_settled(), "{:?}", o.session.phase());

    // The trigger's initial tab was replaced by the first window's tabs (Req 6.1).
    assert_eq!(shown(sim, o.trigger), locs(&[DOCUMENTS, DOWNLOADS, PHOTOS]));
    // Exactly one more window, with the second window's tabs (Req 5.1, 5.3).
    let others = opened(sim);
    assert_eq!(others.len(), 1);
    assert_eq!(shown(sim, others[0]), locs(&[THIS_PC, RECYCLE]));
    assert_eq!(o.session.window_for(WindowId(2)), Some(others[0]));
    assert_eq!(count(&o.run, CommandKind::OpenWindow), 1);

    // Trigger placed once (Req 7.7); the other window created at its final
    // placement, hidden, and never moved (Req 7.2).
    let place: Vec<_> = sim.commands_of(CommandKind::ApplyPlacement);
    assert_eq!(place.len(), 1);
    let plan = o.session.plan();
    assert!(matches!(&place[0].command, Command::ApplyPlacement { win, placement }
        if *win == o.trigger && *placement == plan.windows[0].placement));
    let rec = sim.window_record(others[0]).unwrap();
    assert_eq!(rec.placements.len(), 1);
    assert_eq!(rec.placements[0].1.restored, plan.windows[1].placement.restored);
    // Revealed at completion (task 11.4), first visible at its final placement.
    let (_, first) = rec.first_visible.clone().expect("revealed at completion");
    assert_eq!(first.restored, plan.windows[1].placement.restored);
    assert_eq!(rec.visible_placement_changes, 0);
    assert_eq!(sim.window(o.trigger).unwrap().placement.restored, snap.windows[0].placement.restored);

    // Sequential creation, nothing closed, one probe per tab.
    assert!(!sim.pipelining_violated());
    assert_eq!(count(&o.run, CommandKind::CloseTab), 0);
    assert_eq!(count(&o.run, CommandKind::Probe), 5);
    // Every navigation was needed once: 3 trigger slots + 1 new tab of window 2.
    assert_eq!(count(&o.run, CommandKind::Navigate), 4);
    // Completion (task 11.4) activates each window's saved Active_Tab once;
    // nothing is selected in a Default_Launch.
    assert_eq!(count(&o.run, CommandKind::ActivateTab), 2);
    assert_eq!(sim.window(o.trigger).unwrap().active, 1);
    assert_eq!(sim.window(others[0]).unwrap().active, 0);
    assert_eq!(count(&o.run, CommandKind::SelectItems), 0);
}

#[test]
fn nothing_is_issued_before_the_attempt_counter_write_completes() {
    let snap = snapshot(vec![window(1, &[DOCUMENTS, DOWNLOADS], 0, 100), window(2, &[PHOTOS], 0, 2000)]);
    let o = restore(snap, |_| {});
    let written = event_time(&o.run, |e| matches!(e, Event::WriteCompleted { seq, .. } if *seq == SEQ));
    assert!(written > o.started);
    let log = o.run.sim.command_log();
    assert!(matches!(log[0].command, Command::WriteSessionFile { seq: SEQ, .. }));
    assert!(log[1..].iter().all(|c| c.at >= written), "a command before the attempt write completed");
    // Probes go out first, then window commands.
    let first_window_cmd = log.iter().position(|c| is_window_command(&c.command)).unwrap();
    let last_probe = log.iter().rposition(|c| matches!(c.command, Command::Probe { .. })).unwrap();
    assert!(last_probe < first_window_cmd);
}

#[test]
fn failed_attempt_write_touches_no_window() {
    let snap = snapshot(vec![window(1, &[DOCUMENTS], 0, 100), window(2, &[PHOTOS], 0, 2000)]);
    let o = restore(snap, |s| s.fail(FailureRule::always(FailWhen::Write(SEQ))));
    assert!(matches!(o.session.phase(), Phase::Failed(Failure::AttemptWriteFailed(_))));
    assert_eq!(o.run.sim.command_log().len(), 1);
    assert_eq!(shown(&o.run.sim, o.trigger), [home_location()]);
}

#[test]
fn network_placeholder_keeps_its_position_and_local_tabs_do_not_wait() {
    // Trigger window: Documents, UNC (3 s probe), Downloads. Second window: Mapped (2 s), Photos.
    let snap = snapshot(vec![window(1, &[DOCUMENTS, UNC, DOWNLOADS], 2, 100), window(2, &[MAPPED, PHOTOS], 1, 2000)]);
    let o = restore(snap, |s| {
        s.set_probe(loc(UNC), ProbeOutcome::Available, 3000);
        s.set_probe(loc(MAPPED), ProbeOutcome::Available, 2000);
    });
    let sim = &o.run.sim;
    assert!(o.session.is_settled());
    assert_eq!(shown(sim, o.trigger), locs(&[DOCUMENTS, UNC, DOWNLOADS]), "Req 7.9");
    let others = opened(sim);
    assert_eq!(others.len(), 1);
    assert_eq!(shown(sim, others[0]), locs(&[MAPPED, PHOTOS]));

    // Every local navigation went out before the first network probe finished (Req 7.5).
    let network_done = o.started + 2000;
    let local_navs: Vec<_> = sim
        .commands_of(CommandKind::Navigate)
        .into_iter()
        .filter(|c| matches!(&c.command, Command::Navigate { location, .. } if *location != loc(UNC) && *location != loc(MAPPED)))
        .collect();
    assert!(!local_navs.is_empty());
    assert!(local_navs.iter().all(|c| c.at < network_done), "{local_navs:?}");
    // The window was opened on its first known-restorable Location (Photos);
    // its placeholder tab 0 was navigated to Mapped when the probe returned.
    let open = &sim.commands_of(CommandKind::OpenWindow)[0].command;
    assert!(matches!(open, Command::OpenWindow { location, .. } if *location == loc(PHOTOS)));
    assert!(sim.commands_of(CommandKind::OpenWindow)[0].at < network_done);
    assert!(!sim.pipelining_violated());
    assert_eq!(count(&o.run, CommandKind::CloseTab), 0);
}

#[test]
fn failed_placeholder_is_closed_and_the_rest_keep_their_order() {
    let snap = snapshot(vec![window(1, &[DOCUMENTS, UNC, DOWNLOADS], 0, 100)]);
    let o = restore(snap, |s| s.set_probe(loc(UNC), ProbeOutcome::Timeout, 3000));
    let sim = &o.run.sim;
    assert!(o.session.is_settled());
    assert_eq!(shown(sim, o.trigger), locs(&[DOCUMENTS, DOWNLOADS]), "Req 9.2");
    assert_eq!(count(&o.run, CommandKind::CloseTab), 1);
    assert!(opened(sim).is_empty());
    // The close happened after the probe failed, not before.
    assert!(sim.commands_of(CommandKind::CloseTab)[0].at >= o.started + 3000);
}

#[test]
fn missing_folder_is_redirected_to_its_nearest_existing_ancestor() {
    let snap = snapshot(vec![window(1, &[ZIP, BACKUP], 0, 100)]);
    let zip_ancestors = ancestors(&loc(ZIP));
    // C:\Archives\data.zip exists (and C:\Archives, C:\), the inner folder does not.
    let probes: Vec<AncestorProbe> =
        zip_ancestors.iter().map(|a| AncestorProbe { location: a.clone(), available: true }).collect();
    let o = restore(snap, |s| s.set_probe(loc(ZIP), ProbeOutcome::Missing { ancestors: probes }, 20));
    assert!(o.session.is_settled());
    assert_eq!(shown(&o.run.sim, o.trigger), vec![zip_ancestors[0].clone(), loc(BACKUP)], "Req 9.1");
}

#[test]
fn undecided_trigger_mapping_delays_only_trigger_candidates() {
    // Window 1 has only a network tab that times out after 2 s; window 2 is
    // local (a trigger candidate); window 3 is local and goes ahead at once.
    let snap = snapshot(vec![window(1, &[UNC], 0, 100), window(2, &[DOCUMENTS, DOWNLOADS], 1, 200), window(3, &[PHOTOS], 0, 2000)]);
    let o = restore(snap, |s| s.set_probe(loc(UNC), ProbeOutcome::Timeout, 2000));
    let sim = &o.run.sim;
    assert!(o.session.is_settled());
    let decided_at = o.started + 2000;

    // Window 1 omitted: the trigger hosts window 2 (Req 5.2, 9.5).
    assert_eq!(shown(sim, o.trigger), locs(&[DOCUMENTS, DOWNLOADS]));
    assert_eq!(o.session.window_for(WindowId(2)), Some(o.trigger));
    let others = opened(sim);
    assert_eq!(others.len(), 1, "no window was opened for windows 1 or 2");
    assert_eq!(shown(sim, others[0]), locs(&[PHOTOS]));
    // Window 3 did not wait; the trigger did.
    assert!(sim.commands_of(CommandKind::OpenWindow)[0].at < decided_at);
    let trigger_cmds: Vec<_> = sim
        .command_log()
        .iter()
        .filter(|c| match &c.command {
            Command::ApplyPlacement { win, .. } | Command::CreateTab { win, .. } => *win == o.trigger,
            Command::Navigate { tab, .. } => sim.window(o.trigger).unwrap().tabs.iter().any(|t| t.key == *tab),
            _ => false,
        })
        .collect();
    assert!(!trigger_cmds.is_empty());
    assert!(trigger_cmds.iter().all(|c| c.at >= decided_at));
    assert_eq!(count(&o.run, CommandKind::ApplyPlacement), 1);
}

#[test]
fn all_omitted_leaves_the_trigger_untouched() {
    let snap = snapshot(vec![window(1, &[UNC], 0, 100), window(2, &[MAPPED], 0, 2000)]);
    let o = restore(snap, |s| {
        s.set_probe(loc(UNC), ProbeOutcome::Timeout, 500);
        s.set_probe(loc(MAPPED), ProbeOutcome::AccessDenied, 50);
    });
    assert!(o.session.is_settled());
    assert!(o.session.plan().is_empty());
    assert_eq!(shown(&o.run.sim, o.trigger), [home_location()], "Req 9.6");
    assert!(o.run.sim.command_log().iter().all(|c| !is_window_command(&c.command)));
}

#[test]
fn trigger_already_showing_slot_zero_is_not_navigated() {
    // The trigger opened on Home, which is also the saved first tab.
    let snap = snapshot(vec![window(1, &[10, PROJECTS], 0, 100)]);
    let o = restore(snap, |_| {});
    assert!(o.session.is_settled());
    assert_eq!(shown(&o.run.sim, o.trigger), locs(&[10, PROJECTS]));
    assert_eq!(count(&o.run, CommandKind::Navigate), 1);
}

#[test]
fn overflowing_strip_does_not_cause_renavigation() {
    let config = SimConfig { strip_capacity: Some(3), max_tabs_per_window: 16, ..SimConfig::default() };
    let (sim, trigger, tab) = sim_with_trigger(config);
    let tabs = [PROJECTS, DOCUMENTS, DOWNLOADS, PHOTOS, BACKUP, THIS_PC, RECYCLE];
    let o = restore_with(sim, trigger, tab, snapshot(vec![window(1, &tabs, 6, 100)]), |_| {});
    assert!(o.session.is_settled());
    assert_eq!(shown(&o.run.sim, o.trigger), locs(&tabs));
    assert_eq!(count(&o.run, CommandKind::Navigate), tabs.len(), "one navigation per slot, no re-navigation");
}

#[test]
fn full_path_uia_names_verify_too() {
    let config = SimConfig { uia_naming: esr_sim::explorer::UiaNaming::FullPaths, ..SimConfig::default() };
    let (sim, trigger, tab) = sim_with_trigger(config);
    let o = restore_with(sim, trigger, tab, snapshot(vec![window(1, &[DOCUMENTS, ZIP, THIS_PC], 0, 100)]), |_| {});
    assert!(o.session.is_settled());
    assert_eq!(shown(&o.run.sim, o.trigger), locs(&[DOCUMENTS, ZIP, THIS_PC]));
    assert_eq!(count(&o.run, CommandKind::Navigate), 3);
}

#[test]
fn opened_window_failure_stops_the_session() {
    let snap = snapshot(vec![window(1, &[DOCUMENTS], 0, 100), window(2, &[PHOTOS, BACKUP], 0, 2000)]);
    let o = restore(snap, |s| s.fail(FailureRule::always(FailWhen::Kind(CommandKind::OpenWindow))));
    assert!(matches!(
        o.session.phase(),
        Phase::Failed(Failure::OperationFailed { kind: esr_core::restore::OpKind::OpenWindow, .. })
    ));
    assert!(!o.session.is_settled());
}
