// Feature: explorer-session-restore, Property 4: Explorer termination and shell loss preserve tracked windows
//! Property 4 (task 7.10), checked on the `LiveSessionTracker` driven by the
//! simulated Explorer (`esr_sim::restore::Driver`).
//!
//! A random operation sequence (Property 1's mix plus process kills and shell
//! loss) builds a Live_Session spread over the shell's process and separate
//! Explorer processes; the shell is then restored and everything settles, so
//! the tracker knows every open window. Its `capture()` at that point is the
//! pre-termination capture. A random subset of the live processes is then
//! killed in random order (no orderly close), each kill optionally preceded
//! by `ShellLost`, with time passing in between; or only the shell is lost.
//!
//! After every kill, as long as an `Open` window remains, `capture()` must
//! equal the pre-termination capture and every tracked window must keep its
//! tabs, Locations, Active_Tab, placement, and Activation_Order position, the
//! killed process's windows becoming `Orphaned`. The kill that leaves no
//! `Open` window must emit exactly one Session_End with cause
//! `ExplorerTerminated` and the pre-termination capture, and empty the
//! Live_Session. Restoring the shell (rescan of every window) changes
//! nothing. Finally the orphans must survive until the next Session_End
//! (closing the remaining windows: the last close saves them and drops them)
//! or a completed restoration (`drop_orphans`, which keeps the open windows).
//!
//! **Validates: Requirements 1.10, 3.3, 3.4**

use std::collections::BTreeSet;

use esr_core::engine::{Millis, TabKey, WinKey};
use esr_core::model::{LocationId, Placement, Rect, SaveCause, Snapshot};
use esr_core::tracker::{LiveSessionTracker, LiveWindow, SessionEnd, WindowStatus};
use esr_sim::explorer::{
    ops_strategy, rect_strategy, CloseOrder, NavOutcome, Op, OpsConfig, Pick, ProcessChoice, SimConfig,
    SimExplorer, UiaNaming,
};
use esr_sim::restore::{Driver, Stop};
use proptest::prelude::*;

/// One process termination of the termination phase.
#[derive(Clone, Debug)]
struct Kill {
    /// Index into the live processes at the time of the kill.
    pid: Pick,
    /// `ShellLost` just before the kill.
    lose_shell_first: bool,
    /// Time passing after the kill.
    tick_ms: u16,
}

/// What happens after the termination phase, if the session did not end.
#[derive(Clone, Copy, Debug)]
enum Epilogue {
    Nothing,
    /// The user closes every remaining window: the last close is the next
    /// Session_End.
    CloseAll,
    /// A restoration completes (`drop_orphans`).
    RestorationCompleted,
}

#[derive(Clone, Debug)]
struct Scenario {
    config: SimConfig,
    setup: Vec<Op>,
    /// Windows opened in separate processes after the setup.
    extra: Vec<Op>,
    /// `ShellLost` before the first kill (also covers shell loss alone).
    lose_shell_at_start: bool,
    kills: Vec<Kill>,
    restore_shell_after: bool,
    epilogue: Epilogue,
}

fn sim_config() -> impl Strategy<Value = SimConfig> {
    prop_oneof![
        Just(SimConfig::default()),
        Just(SimConfig { uia_naming: UiaNaming::FullPaths, ..SimConfig::default() }),
        Just(SimConfig { report_active_key: true, reuse_hwnds: false, ..SimConfig::default() }),
    ]
}

fn extra_window() -> impl Strategy<Value = Op> {
    let process = prop_oneof![
        2 => Just(ProcessChoice::New),
        1 => any::<u16>().prop_map(|p| ProcessChoice::Existing(Pick(p))),
        1 => Just(ProcessChoice::Main),
    ];
    (process, any::<u16>(), rect_strategy(), prop::bool::weighted(0.2)).prop_map(
        |(process, location, rect, maximized)| Op::OpenWindow {
            process,
            location: Pick(location),
            nav: NavOutcome::Complete,
            rect,
            maximized,
        },
    )
}

fn kill() -> impl Strategy<Value = Kill> {
    (any::<u16>(), prop::bool::weighted(0.3), 0u16..=400)
        .prop_map(|(pid, lose_shell_first, tick_ms)| Kill { pid: Pick(pid), lose_shell_first, tick_ms })
}

fn epilogue() -> impl Strategy<Value = Epilogue> {
    prop_oneof![
        1 => Just(Epilogue::Nothing),
        2 => Just(Epilogue::CloseAll),
        1 => Just(Epilogue::RestorationCompleted),
    ]
}

fn scenario() -> impl Strategy<Value = Scenario> {
    (
        sim_config(),
        ops_strategy(&OpsConfig::all(), 0..30),
        prop::collection::vec(extra_window(), 0..4),
        prop::bool::weighted(0.3),
        prop::collection::vec(kill(), 0..=4),
        any::<bool>(),
        epilogue(),
    )
        .prop_map(|(config, setup, extra, lose_shell_at_start, kills, restore_shell_after, epilogue)| Scenario {
            config,
            setup,
            extra,
            lose_shell_at_start,
            kills,
            restore_shell_after,
            epilogue,
        })
}

/// The tracker, driven by the simulated Explorer, plus the Session_Ends it
/// emitted.
struct Harness {
    tracker: LiveSessionTracker,
    driver: Driver,
    ends: Vec<SessionEnd>,
}

impl Harness {
    fn new(config: SimConfig) -> Self {
        let mut driver = Driver::new(SimExplorer::new(config));
        driver.set_deadline(Millis(u64::MAX / 2));
        Harness { tracker: LiveSessionTracker::new(), driver, ends: Vec::new() }
    }

    fn sim(&self) -> &SimExplorer {
        self.driver.sim()
    }

    /// Plays `op` and runs until quiescence (tracker timers included).
    fn play(&mut self, op: &Op) {
        let events = self.driver.sim_mut().apply(op);
        self.driver.deliver(events);
        let tracker = &mut self.tracker;
        let ends = &mut self.ends;
        let stop = self.driver.run(|now, e| {
            let out = tracker.handle(now, &e);
            ends.extend(out.session_end);
            out.commands
        });
        assert_eq!(stop, Stop::Quiescent, "after {op:?}");
    }

    fn capture(&self) -> Snapshot {
        self.tracker.capture().0
    }
}

/// Everything the tracker records about a window.
#[derive(Clone, Debug, PartialEq)]
struct WinFp {
    key: WinKey,
    pid: u32,
    status: WindowStatus,
    active: Option<TabKey>,
    placement: Placement,
    tabs: Vec<(TabKey, Option<LocationId>, bool)>,
}

fn fp(w: &LiveWindow) -> WinFp {
    WinFp {
        key: w.key(),
        pid: w.pid(),
        status: w.status(),
        active: w.active(),
        placement: w.placement().clone(),
        tabs: w.tabs().iter().map(|t| (t.key(), t.location().cloned(), t.is_tombstoned())).collect(),
    }
}

/// The tracked windows in Activation_Order.
fn fingerprint(t: &LiveSessionTracker) -> Vec<WinFp> {
    t.windows().map(fp).collect()
}

/// Field-exact snapshot equality (`LocationId`'s `==` ignores PIDL bytes and
/// display names).
fn assert_identical(actual: &Snapshot, expected: &Snapshot, context: &str) {
    assert_eq!(actual, expected, "{context}");
    for (a, e) in actual.windows.iter().zip(&expected.windows) {
        for (ta, te) in a.tabs.iter().zip(&e.tabs) {
            assert!(ta.location.identical(&te.location), "{context}: {:?} vs {:?}", ta.location, te.location);
        }
    }
}

fn check(s: &Scenario) {
    let mut h = Harness::new(s.config.clone());
    for op in &s.setup {
        h.play(op);
    }
    // Bring the shell back (rescanning every window) so the tracker knows
    // every open window, then spread windows over separate processes.
    h.play(&Op::RestoreShell);
    for op in &s.extra {
        h.play(op);
    }

    // The tracker's open windows are exactly the open windows.
    let open: BTreeSet<WinKey> =
        h.tracker.windows().filter(|w| w.status() == WindowStatus::Open).map(|w| w.key()).collect();
    let truth: BTreeSet<WinKey> = h.sim().windows().iter().map(|w| w.key).collect();
    assert_eq!(open, truth, "tracker out of sync before the termination phase");

    // The pre-termination state.
    let before = h.capture();
    let mut expected = fingerprint(&h.tracker);
    let mut ends_seen = h.ends.len();
    let mut ended = false;

    if s.lose_shell_at_start {
        h.play(&Op::LoseShell);
        assert_eq!(h.ends.len(), ends_seen, "ShellLost ends nothing");
        assert_identical(&h.capture(), &before, "after ShellLost");
        assert_eq!(fingerprint(&h.tracker), expected, "after ShellLost");
    }

    for (n, k) in s.kills.iter().enumerate() {
        let context = format!("kill {n} ({k:?})");
        let pids: Vec<u32> = h.sim().processes().iter().copied().collect();
        let Some(i) = k.pid.of(pids.len()) else { break };
        let pid = pids[i];
        if k.lose_shell_first {
            h.play(&Op::LoseShell);
        }
        let had_open = expected.iter().any(|w| w.pid == pid && w.status == WindowStatus::Open);
        let others_open = expected.iter().any(|w| w.pid != pid && w.status == WindowStatus::Open);

        h.play(&Op::KillProcess { pid: Pick(i as u16) });
        h.play(&Op::Tick { ms: u64::from(k.tick_ms) });
        assert!(!h.sim().processes().contains(&pid));

        let new_ends = &h.ends[ends_seen..];
        if ended {
            assert!(new_ends.is_empty(), "{context}: {new_ends:#?}");
            assert!(h.tracker.is_empty(), "{context}");
        } else if had_open && !others_open {
            // No Open window remains: Session_End with the pre-termination capture.
            assert_eq!(new_ends.len(), 1, "{context}: {new_ends:#?}");
            assert_eq!(new_ends[0].cause, SaveCause::ExplorerTerminated, "{context}");
            assert_identical(&new_ends[0].snapshot, &before, &context);
            assert!(h.tracker.is_empty(), "{context}: the Session_End empties the Live_Session");
            ended = true;
        } else {
            assert!(new_ends.is_empty(), "{context}: {new_ends:#?}");
            for w in expected.iter_mut().filter(|w| w.pid == pid) {
                w.status = WindowStatus::Orphaned;
            }
            assert_identical(&h.capture(), &before, &context);
            assert_eq!(fingerprint(&h.tracker), expected, "{context}");
        }
        ends_seen = h.ends.len();
    }

    if s.restore_shell_after || matches!(s.epilogue, Epilogue::CloseAll) {
        h.play(&Op::RestoreShell);
        assert_eq!(h.ends.len(), ends_seen, "ShellAvailable ends nothing");
        if ended {
            assert!(h.tracker.is_empty());
        } else {
            assert_identical(&h.capture(), &before, "after ShellAvailable");
            assert_eq!(fingerprint(&h.tracker), expected, "after ShellAvailable");
        }
    }
    if ended {
        return;
    }

    let orphans: Vec<WinFp> = expected.iter().filter(|w| w.status == WindowStatus::Orphaned).cloned().collect();
    let orphans_kept = |h: &Harness, context: &str| {
        for o in &orphans {
            let w = h.tracker.window(o.key).unwrap_or_else(|| panic!("{context}: orphan {:?} dropped", o.key));
            assert_eq!(&fp(w), o, "{context}");
        }
    };
    match s.epilogue {
        Epilogue::Nothing => {}
        Epilogue::RestorationCompleted => {
            h.tracker.drop_orphans();
            let open: Vec<WinFp> = expected.into_iter().filter(|w| w.status == WindowStatus::Open).collect();
            assert_eq!(fingerprint(&h.tracker), open, "drop_orphans keeps exactly the open windows");
        }
        Epilogue::CloseAll => {
            let order = CloseOrder { revoke_before: u8::MAX, revoke_after: false, gap_ms: 10, rotate: Pick(0) };
            while !h.sim().windows().is_empty() {
                let last = h.sim().windows().len() == 1;
                orphans_kept(&h, "before a close");
                let pre = h.capture();
                // Revoke every tab, then destroy the window.
                let tabs = h.sim().windows()[0].tabs.len();
                let order = CloseOrder { revoke_before: (tabs % 256) as u8, ..order };
                h.play(&Op::CloseWindow { win: Pick(0), order });
                let new_ends = &h.ends[ends_seen..];
                if last {
                    assert_eq!(new_ends.len(), 1, "last close: {new_ends:#?}");
                    assert!(
                        matches!(new_ends[0].cause, SaveCause::LastWindowClosed | SaveCause::LastTabClosed),
                        "{:?}",
                        new_ends[0].cause
                    );
                    // The next Session_End saves the orphans, then drops them.
                    assert_identical(&new_ends[0].snapshot, &pre, "last close");
                    assert!(h.tracker.is_empty(), "orphans dropped at the Session_End");
                } else {
                    assert!(new_ends.is_empty(), "{new_ends:#?}");
                }
                ends_seen = h.ends.len();
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 160, ..ProptestConfig::default() })]

    #[test]
    fn termination_and_shell_loss_preserve_tracked_windows(s in scenario()) {
        check(&s);
    }
}

fn open(process: ProcessChoice, location: u16, left: i32) -> Op {
    Op::OpenWindow {
        process,
        location: Pick(location),
        nav: NavOutcome::Complete,
        rect: Rect { left, top: 50, right: left + 800, bottom: 650 },
        maximized: false,
    }
}

/// A window in the shell's process and one in a separate process: killing
/// the separate process orphans its window; losing the shell, then killing
/// the shell's process ends the session with both windows.
#[test]
fn killing_every_process_saves_both_windows() {
    let mut h = Harness::new(SimConfig::default());
    h.play(&open(ProcessChoice::Main, 0, 0));
    h.play(&Op::OpenTab { win: Pick(0), location: Pick(3), nav: NavOutcome::Complete });
    h.play(&open(ProcessChoice::New, 4, 900));
    let before = h.capture();
    assert_eq!(before.windows.len(), 2);
    assert_eq!(before.tab_count(), 3);

    let main = h.sim().main_pid().expect("shell running");
    let separate = *h.sim().processes().iter().find(|p| **p != main).expect("separate process");
    let index = |h: &Harness, pid: u32| h.sim().processes().iter().position(|p| *p == pid).unwrap() as u16;

    h.play(&Op::KillProcess { pid: Pick(index(&h, separate)) });
    assert!(h.ends.is_empty());
    assert_eq!(h.tracker.open_window_count(), 1);
    assert_eq!(h.tracker.window_count(), 2);
    assert_identical(&h.capture(), &before, "orphaned");

    h.play(&Op::LoseShell);
    h.play(&Op::KillProcess { pid: Pick(index(&h, main)) });
    assert_eq!(h.ends.len(), 1);
    assert_eq!(h.ends[0].cause, SaveCause::ExplorerTerminated);
    assert_identical(&h.ends[0].snapshot, &before, "ExplorerTerminated");
    assert!(h.tracker.is_empty());
}

/// Shell loss alone keeps the Live_Session; the shell's return rescans and
/// changes nothing.
#[test]
fn shell_loss_alone_keeps_the_session() {
    let mut h = Harness::new(SimConfig::default());
    h.play(&open(ProcessChoice::Main, 1, 0));
    h.play(&open(ProcessChoice::Main, 5, 900));
    let before = h.capture();
    let fp_before = fingerprint(&h.tracker);
    h.play(&Op::LoseShell);
    h.play(&Op::Tick { ms: 5000 });
    assert_identical(&h.capture(), &before, "shell lost");
    h.play(&Op::RestoreShell);
    assert_identical(&h.capture(), &before, "shell back");
    assert_eq!(fingerprint(&h.tracker), fp_before);
    assert!(h.ends.is_empty());
}
