//! Targeted_Launch composition of `RestoreSession` (task 11.3), driven
//! against the simulated Explorer.
//!
//! Requirements 6.2, 6.3, 6.4, 6.5.

use esr_core::engine::{Command, ItemId, TabKey, WinKey};
use esr_core::model::{DisplayRef, LocationId, Placement, Rect, ShowState, Snapshot, TabRec, WindowId, WindowRec};
use esr_core::plan::LaunchInfo;
use esr_core::restore::{Phase, RestoreParams, RestoreSession};
use esr_sim::explorer::{default_displays, default_locations, SimConfig, SimExplorer};
use esr_sim::restore::{CommandKind, Driver, FailWhen, FailureRule, Launch, Origin, Run, Stop};

const SEQ: u64 = 41;

const DOCUMENTS: usize = 2;
const DOWNLOADS: usize = 3;
const PHOTOS: usize = 4;
const BACKUP: usize = 5;
const THIS_PC: usize = 9;

fn loc(i: usize) -> LocationId {
    default_locations()[i].clone()
}

fn locs(ids: &[usize]) -> Vec<LocationId> {
    ids.iter().map(|&i| loc(i)).collect()
}

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

fn items() -> Vec<ItemId> {
    vec![ItemId(vec![1, 2, 3, 0, 0]), ItemId(vec![4, 5, 0, 0])]
}

struct Outcome {
    session: RestoreSession,
    run: Run,
    trigger: WinKey,
    /// The trigger's original tab.
    original: TabKey,
}

/// A Targeted_Launch on `requested` with `selection`, restoring `snap`.
fn restore(snap: Snapshot, requested: usize, selection: Vec<ItemId>, setup: impl FnOnce(&mut SimExplorer)) -> Outcome {
    let mut sim = SimExplorer::new(SimConfig::default());
    sim.launch(&Launch { location: loc(requested), selection: selection.clone(), rect: rect(300, 300), maximized: false });
    sim.settle();
    let (trigger, original) = {
        let w = &sim.windows()[0];
        (w.key, w.tabs[0].key)
    };
    assert_eq!(sim.selection(original), &selection[..]);
    setup(&mut sim);
    let mut session = RestoreSession::new(
        sim.now(),
        RestoreParams {
            snapshot: snap,
            launch: LaunchInfo::Targeted { requested: loc(requested) },
            displays: default_displays(),
            trigger,
            trigger_tab: Some(original),
            trigger_location: Some(loc(requested)),
            requested_selection: selection,
            attempt_write_seq: SEQ,
            first_id: 500,
        },
    );
    let mut driver = Driver::new(sim);
    driver.submit([Command::WriteSessionFile { seq: SEQ, text: "failed-attempts 1".into(), urgent: true }]);
    let stop = driver.run(|now, ev| session.handle(now, &ev));
    let run = driver.finish(stop);
    assert_eq!(run.stop, Stop::Quiescent);
    Outcome { session, run, trigger, original }
}

fn shown(sim: &SimExplorer, win: WinKey) -> Vec<LocationId> {
    sim.window(win).expect("open").tabs.iter().map(|t| t.last_known.clone().expect("navigated")).collect()
}

fn tab_keys(sim: &SimExplorer, win: WinKey) -> Vec<TabKey> {
    sim.window(win).expect("open").tabs.iter().map(|t| t.key).collect()
}

fn opened(sim: &SimExplorer) -> Vec<WinKey> {
    sim.window_records()
        .iter()
        .filter(|r| matches!(r.origin, Origin::Request(_)) && r.closed_at.is_none())
        .map(|r| r.key)
        .collect()
}

#[test]
fn requested_tab_is_appended_last_activated_and_selected() {
    let snap = snapshot(vec![window(1, &[DOCUMENTS, DOWNLOADS], 0, 100), window(2, &[PHOTOS, THIS_PC], 1, 2000)]);
    let o = restore(snap, BACKUP, items(), |_| {});
    let sim = &o.run.sim;
    assert!(o.session.is_settled(), "{:?}", o.session.phase());

    // Restored tabs plus one, the requested Location rightmost (Req 6.2).
    assert_eq!(shown(sim, o.trigger), locs(&[DOCUMENTS, DOWNLOADS, BACKUP]));
    let keys = tab_keys(sim, o.trigger);
    // The original tab became restored tab 0; the requested tab is new.
    assert_eq!(keys[0], o.original);
    assert_eq!(o.session.requested_tab(), Some(keys[2]));
    assert_ne!(keys[2], o.original);

    // Active (Req 6.3) with exactly the captured items selected (Req 6.4).
    assert_eq!(sim.window(o.trigger).unwrap().active, 2);
    assert_eq!(sim.selection(keys[2]), &items()[..]);
    assert!(sim.selection(keys[0]).is_empty(), "navigating the original tab cleared its selection");
    assert_eq!(o.session.requested_selection(), &items()[..]);

    // ActivateTab, then SelectItems, after the requested tab's navigation.
    // The other window's saved Active_Tab is activated too (task 11.4).
    let activations: Vec<_> = sim
        .commands_of(CommandKind::ActivateTab)
        .into_iter()
        .filter(|c| matches!(c.command, Command::ActivateTab { win, .. } if win == o.trigger))
        .collect();
    let selects = sim.commands_of(CommandKind::SelectItems);
    assert_eq!(activations.len(), 1);
    assert_eq!(sim.commands_of(CommandKind::ActivateTab).len(), 2);
    assert_eq!(selects.len(), 1);
    assert!(matches!(activations[0].command, Command::ActivateTab { win, index: 2, .. } if win == o.trigger));
    assert!(matches!(&selects[0].command, Command::SelectItems { tab, items: i, .. } if *tab == keys[2] && *i == items()));
    let log = sim.command_log();
    let pos = |pred: &dyn Fn(&Command) -> bool| log.iter().rposition(|c| pred(&c.command)).unwrap();
    let last_nav = pos(&|c| matches!(c, Command::Navigate { tab, .. } if *tab == keys[2]));
    let act = pos(&|c| matches!(c, Command::ActivateTab { win, .. } if *win == o.trigger));
    let sel = pos(&|c| matches!(c, Command::SelectItems { .. }));
    assert!(last_nav < act && act < sel);

    // The other window keeps its saved tabs and its saved Active_Tab.
    let others = opened(sim);
    assert_eq!(others.len(), 1);
    assert_eq!(shown(sim, others[0]), locs(&[PHOTOS, THIS_PC]));
    assert_eq!(o.session.active_tabs(), vec![(o.trigger, 2), (others[0], 1)]);
    assert_eq!(sim.window(others[0]).unwrap().active, 1);
    // Focus and z-order go to the trigger first (Req 6.3).
    assert_eq!(sim.focused(), Some(o.trigger));
    assert_eq!(&sim.z_order()[..2], &[o.trigger, others[0]]);
}

#[test]
fn duplicate_location_stays_a_separate_tab() {
    // A restored tab shows the requested Location (Req 6.5).
    let snap = snapshot(vec![window(1, &[DOCUMENTS, BACKUP, DOWNLOADS], 2, 100)]);
    let o = restore(snap, BACKUP, items(), |_| {});
    let sim = &o.run.sim;
    assert!(o.session.is_settled());
    assert_eq!(shown(sim, o.trigger), locs(&[DOCUMENTS, BACKUP, DOWNLOADS, BACKUP]));
    let keys = tab_keys(sim, o.trigger);
    assert_eq!(sim.window(o.trigger).unwrap().active, 3);
    assert_eq!(o.session.requested_tab(), Some(keys[3]));
    assert_eq!(sim.selection(keys[3]), &items()[..]);
    assert!(sim.selection(keys[1]).is_empty(), "the restored duplicate is not selected into");
}

#[test]
fn requested_location_equal_to_restored_tab_zero_still_gets_its_own_tab() {
    // The original tab already shows restored tab 0, so it is not navigated.
    let snap = snapshot(vec![window(1, &[BACKUP, DOCUMENTS], 0, 100)]);
    let o = restore(snap, BACKUP, items(), |_| {});
    let sim = &o.run.sim;
    assert!(o.session.is_settled());
    assert_eq!(shown(sim, o.trigger), locs(&[BACKUP, DOCUMENTS, BACKUP]));
    let keys = tab_keys(sim, o.trigger);
    assert_eq!(keys[0], o.original);
    assert_eq!(sim.window(o.trigger).unwrap().active, 2);
    assert_eq!(sim.selection(keys[2]), &items()[..]);
    assert!(
        !sim.commands_of(CommandKind::Navigate)
            .iter()
            .any(|c| matches!(c.command, Command::Navigate { tab, .. } if tab == o.original)),
        "slot 0 already showed restored tab 0"
    );
}

#[test]
fn no_selection_issues_no_select_items() {
    let snap = snapshot(vec![window(1, &[DOCUMENTS], 0, 100)]);
    let o = restore(snap, BACKUP, vec![], |_| {});
    let sim = &o.run.sim;
    assert!(o.session.is_settled());
    assert_eq!(shown(sim, o.trigger), locs(&[DOCUMENTS, BACKUP]));
    assert_eq!(sim.window(o.trigger).unwrap().active, 1);
    assert_eq!(sim.commands_of(CommandKind::ActivateTab).len(), 1);
    assert!(sim.commands_of(CommandKind::SelectItems).is_empty());
}

#[test]
fn failed_activation_is_retried_once() {
    let snap = snapshot(vec![window(1, &[DOCUMENTS, DOWNLOADS], 0, 100)]);
    let o = restore(snap, BACKUP, items(), |s| s.fail(FailureRule::times(FailWhen::Kind(CommandKind::ActivateTab), 1)));
    let sim = &o.run.sim;
    assert!(o.session.is_settled(), "{:?}", o.session.phase());
    assert_eq!(sim.commands_of(CommandKind::ActivateTab).len(), 2);
    assert_eq!(sim.window(o.trigger).unwrap().active, 2);
    assert_eq!(sim.selection(tab_keys(sim, o.trigger)[2]), &items()[..]);

    // A second failure gives the activation up: the restoration still
    // completes, with whatever tab Explorer shows (design.md, "Error Handling").
    let snap = snapshot(vec![window(1, &[DOCUMENTS, DOWNLOADS], 0, 100)]);
    let o = restore(snap, BACKUP, items(), |s| s.fail(FailureRule::always(FailWhen::Kind(CommandKind::ActivateTab))));
    assert_eq!(o.session.phase(), &Phase::Completed, "{:?}", o.session.phase());
    assert_eq!(o.run.sim.commands_of(CommandKind::ActivateTab).len(), 2);
    assert!(o.session.is_settled());
    let log = o.run.sim.log_entries();
    assert!(
        log.iter().any(|(_, e)| matches!(e, esr_core::logfmt::entry::LogEntry::Error { operation, .. }
            if operation == "restore-activate-tab")),
        "the given-up activation is logged"
    );
}

#[test]
fn failed_selection_is_retried_once_then_given_up() {
    let snap = snapshot(vec![window(1, &[DOCUMENTS], 0, 100)]);
    let o = restore(snap, BACKUP, items(), |s| s.fail(FailureRule::always(FailWhen::Kind(CommandKind::SelectItems))));
    let sim = &o.run.sim;
    assert_eq!(sim.commands_of(CommandKind::SelectItems).len(), 2);
    assert!(o.session.selection_error().is_some());
    // Not a restoration failure: the tabs are restored and the tab active.
    assert!(o.session.is_settled(), "{:?}", o.session.phase());
    assert_eq!(shown(sim, o.trigger), locs(&[DOCUMENTS, BACKUP]));
    assert_eq!(sim.window(o.trigger).unwrap().active, 1);
}
