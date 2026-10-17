//! Fault injection for tab folding (task 21.3): the ways File Explorer lets a
//! fold down, schedulable per operation so that a generator can inject any
//! failure schedule (Req 14.13, 14.14, 14.16, 14.23, 14.24).
//!
//! # Schedules
//!
//! [`FoldFaults`] (in [`FoldConfig::faults`]; [`SimExplorer::faults_mut`]
//! changes it while a run is in progress) selects faults by the *ordinal* of
//! the command they hit, counted from 0 in the order the simulator receives
//! the commands:
//!
//! - the n-th `CreateTab` that reaches an existing window
//!   ([`FoldFaults::create_tab`], a [`CreateTabFault`]),
//! - the n-th `ActivateTab` that reaches an existing tab
//!   ([`FoldFaults::activate_tab`], an [`ActivateFault`]), and
//! - the n-th `CloseWindow` that reaches an existing window
//!   ([`FoldFaults::ignore_close`]); [`FoldFaults::ignore_first_close`] makes
//!   every candidate ignore the first `CloseWindow` it receives (spike F2: 1
//!   of 22 candidates did).
//!
//! The counters include the commands of a restoration. A command rejected by
//! a [`FailureRule`](crate::restore::FailureRule) is not counted.
//! [`SimExplorer::create_tab_commands`] and its siblings give the ordinal the
//! next command will get, so a test can inject a fault into the very next one.
//!
//! # Faults
//!
//! - **Tab window late or never** ([`TabWindowFault`]). The new
//!   `ShellTabWindowClass` child of a `CreateTab` appears `extra_ms` later than
//!   usual, or never: no tab, no event, and no re-post (the fold's own 1 s
//!   limit ends the wait, Req 14.13). A late tab is still created, which is
//!   the "late tab" the fold closes when `TabStarted` arrives up to 10 s after
//!   it gave up.
//! - **Delayed registration** ([`CreateTabFault::registration_delay_ms`]). The
//!   tab window appears on time (`TabStarted`) but registers in ShellWindows,
//!   and is reported by `TabCreated`, only that much later (on top of
//!   [`FoldConfig::registration_lag_ms`]). Until then a scan lists the tab's
//!   name in the strip but no registered tab.
//! - **Unconfirmed activation** ([`ActivateMode`]). The selection changes but
//!   no `ActivationConfirmed` ever follows (`Unconfirmed`), the selection
//!   never changes (`Ignored`), or both arrive late (`Late`): the Tab_Switch
//!   of Req 14.23 is not confirmed within 1 s.
//! - **A target that closes** (`target_closes_after_ms` of a create or
//!   activate fault). The window the command names closes that long after the
//!   command, whatever the command was doing: its registered tabs are revoked
//!   and `WindowDestroyed` follows at the same logical time (Req 14.23, 14.24:
//!   the Matching_Tab's window, or a Fold_Target, closes before the fold
//!   ends). A tab still to appear in it never does.
//! - **A candidate that ignores `CloseWindow`** ([`FoldFaults::ignore_close`]):
//!   the command is recorded and does nothing; a later `CloseWindow` works.
//!
//! # Window states
//!
//! These are operations, so a script or generator can place them anywhere in
//! logical time:
//!
//! - [`Op::SetUnresponsive`](crate::explorer::Op::SetUnresponsive): the
//!   window does not answer `WM_NULL` (a `FoldContext` lists it, Req 14.13).
//!   Its thread is hung, so what it would do while hung waits: a new tab does
//!   not appear, a tab is not selected, a `CloseWindow` and a `HideWindow` do
//!   not take effect, and all of it happens, in order, at the moment the
//!   window answers again.
//! - [`Op::BeginClosing`](crate::explorer::Op::BeginClosing): the window has
//!   begun closing, a Closing_Explorer_Window. Every registered tab is revoked
//!   at once and `WindowDestroyed` follows [`FoldConfig::closing_ms`] later.
//!
//! Requirements 14.13, 14.14, 14.16, 14.23, 14.24.

use std::collections::{BTreeMap, BTreeSet};

use esr_core::engine::{Event, OpId, TabKey, WinKey};
use esr_core::model::SaveCause;

use super::Action;
use crate::explorer::{Pick, Scheduled, SimExplorer};
use crate::restore::Action as RestoreAction;

// ---------------------------------------------------------------------------
// Schedule
// ---------------------------------------------------------------------------

/// When the tab window of a `CreateTab` appears.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum TabWindowFault {
    #[default]
    Normal,
    /// It appears `extra_ms` later than it would have.
    Late { extra_ms: u64 },
    /// It never appears: the command has no effect and no event follows.
    Never,
}

/// A fault of one `CreateTab` command.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct CreateTabFault {
    pub tab_window: TabWindowFault,
    /// Extra time from the tab window appearing (`TabStarted`) to the tab
    /// registering (`TabCreated`).
    pub registration_delay_ms: u64,
    /// The command's window closes this long after the command.
    pub target_closes_after_ms: Option<u64>,
}

/// What an `ActivateTab` does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ActivateMode {
    #[default]
    Normal,
    /// The selection changes and `ActivationConfirmed` follows `extra_ms`
    /// later than usual.
    Late { extra_ms: u64 },
    /// The selection changes, but `ActivationConfirmed` never follows.
    Unconfirmed,
    /// The selection does not change and nothing is reported.
    Ignored,
}

/// A fault of one `ActivateTab` command.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ActivateFault {
    pub mode: ActivateMode,
    /// The command's window closes this long after the command.
    pub target_closes_after_ms: Option<u64>,
}

/// Which commands fail, by ordinal (see the module docs).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FoldFaults {
    pub create_tab: BTreeMap<u64, CreateTabFault>,
    pub activate_tab: BTreeMap<u64, ActivateFault>,
    /// Ordinals of the `CloseWindow` commands that have no effect.
    pub ignore_close: BTreeSet<u64>,
    /// Every candidate ignores the first `CloseWindow` it receives.
    pub ignore_first_close: bool,
}

impl FoldFaults {
    /// No faults.
    pub fn none() -> Self {
        FoldFaults::default()
    }

    /// Every candidate ignores its first `CloseWindow`.
    pub fn stubborn_candidates() -> Self {
        FoldFaults { ignore_first_close: true, ..FoldFaults::default() }
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Counters and held-back work of the fault injection, part of [`FoldState`](super::FoldState).
#[derive(Clone, Debug, Default)]
pub(crate) struct FaultState {
    create_tab_seen: u64,
    activate_tab_seen: u64,
    close_seen: u64,
    /// `CloseWindow` commands received by each window.
    close_attempts: BTreeMap<WinKey, u32>,
    /// The fault of each `CreateTab` whose tab has not appeared yet.
    create_pending: BTreeMap<OpId, CreateTabFault>,
    /// Work of unresponsive windows, in the order it came due.
    stalled: Vec<(WinKey, Scheduled)>,
}

// ---------------------------------------------------------------------------
// SimExplorer
// ---------------------------------------------------------------------------

impl SimExplorer {
    /// The fault schedule in effect.
    pub fn faults(&self) -> &FoldFaults {
        &self.config.fold.faults
    }

    /// Change the fault schedule (it applies to the commands received from now on).
    pub fn faults_mut(&mut self) -> &mut FoldFaults {
        &mut self.config.fold.faults
    }

    /// The ordinal the next counted `CreateTab` will get.
    pub fn create_tab_commands(&self) -> u64 {
        self.fold.faults.create_tab_seen
    }

    /// The ordinal the next counted `ActivateTab` will get.
    pub fn activate_tab_commands(&self) -> u64 {
        self.fold.faults.activate_tab_seen
    }

    /// The ordinal the next counted `CloseWindow` will get.
    pub fn close_window_commands(&self) -> u64 {
        self.fold.faults.close_seen
    }

    /// Whether `win` does not answer `WM_NULL`.
    pub fn is_unresponsive(&self, win: WinKey) -> bool {
        self.fold.unresponsive.contains(&win)
    }

    /// A `CreateTab { op, win }` reached an existing window: take its ordinal
    /// and note its fault.
    pub(crate) fn fold_create_tab_command(&mut self, op: OpId, win: WinKey) {
        let n = self.fold.faults.create_tab_seen;
        self.fold.faults.create_tab_seen += 1;
        let Some(fault) = self.config.fold.faults.create_tab.get(&n).copied() else { return };
        if let Some(ms) = fault.target_closes_after_ms {
            self.after_fold(ms, Action::TargetCloses(win));
        }
        self.fold.faults.create_pending.insert(op, fault);
    }

    /// The fault of the tab `op` creates (none once its tab has appeared).
    pub(crate) fn create_tab_fault(&self, op: OpId) -> CreateTabFault {
        self.fold.faults.create_pending.get(&op).copied().unwrap_or_default()
    }

    /// The tab of `op` never appears or has appeared: forget its fault.
    pub(crate) fn take_create_tab_fault(&mut self, op: OpId) -> CreateTabFault {
        self.fold.faults.create_pending.remove(&op).unwrap_or_default()
    }

    /// An `ActivateTab` reached an existing tab of `win`: take its ordinal
    /// and return what it does.
    pub(crate) fn fold_activate_command(&mut self, win: WinKey) -> ActivateMode {
        let n = self.fold.faults.activate_tab_seen;
        self.fold.faults.activate_tab_seen += 1;
        let Some(fault) = self.config.fold.faults.activate_tab.get(&n).copied() else {
            return ActivateMode::Normal;
        };
        if let Some(ms) = fault.target_closes_after_ms {
            self.after_fold(ms, Action::TargetCloses(win));
        }
        fault.mode
    }

    /// Whether this `CloseWindow` of an existing window has no effect.
    pub(crate) fn close_is_ignored(&mut self, win: WinKey) -> bool {
        let n = self.fold.faults.close_seen;
        self.fold.faults.close_seen += 1;
        let attempts = self.fold.faults.close_attempts.entry(win).or_insert(0);
        let first = *attempts == 0;
        *attempts += 1;
        let faults = &self.config.fold.faults;
        faults.ignore_close.contains(&n) || (faults.ignore_first_close && first && self.candidate(win).is_some())
    }

    /// The tab of `op` registers in ShellWindows (and `TabCreated` is
    /// reported) after `ms`.
    pub(crate) fn register_tab_later(&mut self, win: WinKey, tab: TabKey, ms: u64) {
        self.window_changed(win);
        self.after_fold(ms, Action::RegisterTab { win, tab });
    }

    /// The tab registers: `TabCreated`, then the registration itself.
    pub(crate) fn register_tab_now(&mut self, win: WinKey, tab: TabKey) {
        self.tab_registered(win);
        let Some((wi, ti)) = self.find_tab(tab) else { return };
        if !self.shell_lost {
            self.emit(Event::TabCreated { win, tab });
        }
        self.register(wi, ti);
    }

    /// [`Op::BeginClosing`](crate::explorer::Op::BeginClosing).
    pub(crate) fn begin_closing(&mut self, win: Pick) {
        let Some(wi) = win.of(self.windows.len()) else { return };
        let key = self.windows[wi].key;
        self.start_closing(key, self.config.fold.closing_ms);
    }

    /// The window begins closing: its registered tabs are revoked now and
    /// `WindowDestroyed` follows after `destroy_after_ms`.
    pub(crate) fn start_closing(&mut self, win: WinKey, destroy_after_ms: u64) {
        let Some(wi) = self.wi(win) else { return };
        let w = &self.windows[wi];
        let pid = w.pid;
        let tabs: Vec<TabKey> = w.tabs.iter().filter(|t| t.registration.is_some()).map(|t| t.key).collect();
        self.reference.close_window(win, SaveCause::LastWindowClosed);
        self.remove_window(wi);
        for tab in tabs {
            self.revoke_now(win, tab);
        }
        if destroy_after_ms == 0 {
            self.destroyed(win, pid);
        } else {
            self.after_fold(destroy_after_ms, Action::Destroyed { win, pid });
        }
    }

    pub(crate) fn destroyed(&mut self, win: WinKey, pid: u32) {
        self.emit(Event::WindowDestroyed { win });
        self.exit_if_idle(pid);
    }

    /// Hold back `item` if it is work of an unresponsive window; hands it back
    /// otherwise.
    pub(crate) fn stall_if_unresponsive(&mut self, item: Scheduled) -> Option<Scheduled> {
        let win = match &item {
            Scheduled::Restore(
                RestoreAction::TabAppears { win, .. }
                | RestoreAction::RetryNewTab { win, .. }
                | RestoreAction::ActivateTab { win, .. },
            )
            | Scheduled::Fold(Action::Close(win) | Action::Hide(win)) => *win,
            _ => return Some(item),
        };
        if self.fold.unresponsive.contains(&win) && self.wi(win).is_some() {
            self.fold.faults.stalled.push((win, item));
            None
        } else {
            Some(item)
        }
    }

    /// `win` answers again: what it held back happens now, in order.
    pub(crate) fn release_stalled(&mut self, win: WinKey) {
        let (due, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.fold.faults.stalled).into_iter().partition(|s| s.0 == win);
        self.fold.faults.stalled = rest;
        for (_, item) in due {
            self.schedule(self.now, item);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explorer::{NavOutcome, Op, ProcessChoice, SimConfig, TimedEvent};
    use crate::fold::FoldConfig;
    use crate::restore::CommandKind;
    use esr_core::engine::{Command, Millis};
    use esr_core::model::Rect;
    use proptest::prelude::*;

    fn config(faults: FoldFaults) -> SimConfig {
        SimConfig {
            fold: FoldConfig { report_tab_started: true, faults, ..FoldConfig::default() },
            ..SimConfig::default()
        }
    }

    fn open_window(s: &mut SimExplorer) -> WinKey {
        s.apply(&Op::OpenWindow {
            process: ProcessChoice::Main,
            location: Pick(2),
            nav: NavOutcome::Complete,
            rect: Rect { left: 100, top: 100, right: 900, bottom: 700 },
            maximized: false,
        });
        s.settle();
        s.windows().last().unwrap().key
    }

    fn open_candidate(s: &mut SimExplorer) -> WinKey {
        s.apply(&Op::OpenCandidate {
            location: Pick(3),
            selection: vec![],
            bypass_keys: false,
            desktop: crate::fold::DesktopSpec::Current,
            foreground: None,
            process: ProcessChoice::Main,
        });
        s.windows().last().unwrap().key
    }

    fn create_tab(s: &mut SimExplorer, n: u64, win: WinKey) {
        s.handle_command(&Command::CreateTab { op: OpId(n), win });
    }

    /// The events of `kind` among `ev` with their times (scans left out).
    fn timeline(ev: &[TimedEvent]) -> Vec<(Millis, &Event)> {
        ev.iter().filter(|e| !matches!(e.event, Event::WindowScanned(_))).map(|e| (e.at, &e.event)).collect()
    }

    fn started_created(ev: &[TimedEvent]) -> (Vec<Millis>, Vec<Millis>) {
        let at = |f: fn(&Event) -> bool| -> Vec<Millis> {
            ev.iter().filter(|e| f(&e.event)).map(|e| e.at).collect()
        };
        (
            at(|e| matches!(e, Event::TabStarted { .. })),
            at(|e| matches!(e, Event::TabCreated { .. })),
        )
    }

    #[test]
    fn tab_started_is_opt_in_and_precedes_the_registration() {
        // Off by default: restoration event streams are unchanged.
        let mut s = SimExplorer::new(SimConfig::default());
        let win = open_window(&mut s);
        let t0 = s.now();
        create_tab(&mut s, 1, win);
        let ev = s.tick(500);
        assert_eq!(started_created(&ev), (vec![], vec![t0 + 85]));

        let mut s = SimExplorer::new(SimConfig {
            fold: FoldConfig { report_tab_started: true, registration_lag_ms: 300, ..FoldConfig::default() },
            ..SimConfig::default()
        });
        let win = open_window(&mut s);
        let t0 = s.now();
        create_tab(&mut s, 1, win);
        let ev = s.tick(500);
        let tab = s.window(win).unwrap().tabs[1].key;
        assert_eq!(started_created(&ev), (vec![t0 + 85], vec![t0 + 385]));
        assert!(timeline(&ev).contains(&(t0 + 85, &Event::TabStarted { op: OpId(1), tab })));
        assert_eq!(s.observe(win).unwrap().tabs.len(), 2);
    }

    #[test]
    fn faults_hit_the_command_with_their_ordinal_only() {
        let mut faults = FoldFaults::none();
        faults.create_tab.insert(1, CreateTabFault { tab_window: TabWindowFault::Never, ..CreateTabFault::default() });
        let mut s = SimExplorer::new(config(faults));
        let win = open_window(&mut s);
        assert_eq!(s.create_tab_commands(), 0);
        for n in 0..3 {
            create_tab(&mut s, n, win);
            s.tick(200);
        }
        assert_eq!(s.create_tab_commands(), 3);
        assert_eq!(s.window(win).unwrap().tabs.len(), 3, "the second command produced no tab");
    }

    #[test]
    fn a_tab_window_that_never_appears_leaves_no_trace() {
        let mut faults = FoldFaults::none();
        faults.create_tab.insert(0, CreateTabFault { tab_window: TabWindowFault::Never, ..CreateTabFault::default() });
        let mut s = SimExplorer::new(config(faults));
        let win = open_window(&mut s);
        create_tab(&mut s, 1, win);
        let ev = [s.tick(20_000), s.settle()].concat();
        assert!(ev.iter().all(|e| !matches!(e.event, Event::TabStarted { .. } | Event::TabCreated { .. } | Event::OperationFailed { .. })));
        assert_eq!(s.window(win).unwrap().tabs.len(), 1);
        assert_eq!(s.commands_of(CommandKind::CreateTab).len(), 1);
        assert!(!s.pipelining_violated());
    }

    #[test]
    fn a_late_tab_window_appears_late() {
        let mut faults = FoldFaults::none();
        faults.create_tab.insert(
            0,
            CreateTabFault { tab_window: TabWindowFault::Late { extra_ms: 1_500 }, ..CreateTabFault::default() },
        );
        let mut s = SimExplorer::new(config(faults));
        let win = open_window(&mut s);
        let t0 = s.now();
        create_tab(&mut s, 1, win);
        let ev = s.tick(1_584);
        assert!(started_created(&ev).0.is_empty(), "nothing within the fold's 1 s limit");
        assert_eq!(s.window(win).unwrap().tabs.len(), 1);
        let ev = s.tick(1);
        assert_eq!(started_created(&ev), (vec![t0 + 1_585], vec![t0 + 1_585]));
        assert_eq!(s.window(win).unwrap().tabs.len(), 2);
    }

    #[test]
    fn delayed_registration_shows_the_tab_in_the_strip_but_not_in_shell_windows() {
        let mut faults = FoldFaults::none();
        faults.create_tab.insert(0, CreateTabFault { registration_delay_ms: 900, ..CreateTabFault::default() });
        let mut s = SimExplorer::new(config(faults));
        let win = open_window(&mut s);
        let t0 = s.now();
        create_tab(&mut s, 1, win);
        let ev = s.tick(85);
        let tab = s.window(win).unwrap().tabs[1].key;
        assert_eq!(started_created(&ev), (vec![t0 + 85], vec![]));
        assert!(timeline(&ev).contains(&(t0 + 85, &Event::TabStarted { op: OpId(1), tab })));
        let seen = s.observe(win).unwrap();
        assert_eq!(seen.tabs.len(), 1, "not registered yet");
        assert_eq!(seen.strip.as_ref().unwrap().names.len(), 2, "but the strip shows the tab");
        let ev = s.tick(899);
        assert_eq!(started_created(&ev).1, vec![]);
        let ev = s.tick(1);
        assert_eq!(started_created(&ev).1, vec![t0 + 985]);
        assert_eq!(s.observe(win).unwrap().tabs.len(), 2);
    }

    #[test]
    fn activation_faults_change_what_is_confirmed() {
        let mut faults = FoldFaults::none();
        faults.activate_tab.insert(0, ActivateFault { mode: ActivateMode::Unconfirmed, ..ActivateFault::default() });
        faults.activate_tab.insert(1, ActivateFault { mode: ActivateMode::Ignored, ..ActivateFault::default() });
        faults.activate_tab.insert(
            2,
            ActivateFault { mode: ActivateMode::Late { extra_ms: 1_500 }, ..ActivateFault::default() },
        );
        let mut s = SimExplorer::new(config(faults));
        let win = open_window(&mut s);
        s.apply(&Op::OpenTab { win: Pick(0), location: Pick(4), nav: NavOutcome::Complete });
        s.apply(&Op::OpenTab { win: Pick(0), location: Pick(5), nav: NavOutcome::Complete });
        s.settle();
        let activate = |s: &mut SimExplorer, n: u64, index: usize| {
            s.handle_command(&Command::ActivateTab { op: OpId(n), win, index });
        };
        let confirmed = |ev: &[TimedEvent], n: u64| {
            ev.iter().any(|e| e.event == Event::ActivationConfirmed { op: OpId(n) })
        };

        activate(&mut s, 1, 0);
        let ev = s.tick(5_000);
        assert_eq!(s.window(win).unwrap().active, 0, "the selection changed");
        assert!(!confirmed(&ev, 1), "but nothing confirmed it");

        activate(&mut s, 2, 1);
        let ev = s.tick(5_000);
        assert_eq!(s.window(win).unwrap().active, 0, "ignored");
        assert!(!confirmed(&ev, 2));

        let t0 = s.now();
        activate(&mut s, 3, 2);
        let ev = s.tick(1_519);
        assert_eq!(s.window(win).unwrap().active, 0);
        assert!(!confirmed(&ev, 3));
        let ev = s.tick(1);
        assert_eq!(s.window(win).unwrap().active, 2);
        assert!(ev.iter().any(|e| e.at == t0 + 1_520 && e.event == Event::ActivationConfirmed { op: OpId(3) }));

        // Later commands are not affected.
        activate(&mut s, 4, 1);
        assert!(confirmed(&s.tick(20), 4));
        assert_eq!(s.activate_tab_commands(), 4);
    }

    #[test]
    fn a_target_that_closes_takes_its_pending_tab_with_it() {
        let mut faults = FoldFaults::none();
        faults.create_tab.insert(
            0,
            CreateTabFault { target_closes_after_ms: Some(30), ..CreateTabFault::default() },
        );
        let mut s = SimExplorer::new(config(faults));
        let _other = open_window(&mut s);
        let target = open_window(&mut s);
        let old_tab = s.window(target).unwrap().tabs[0].key;
        let t0 = s.now();
        create_tab(&mut s, 1, target);
        let ev = s.tick(500);
        let got = timeline(&ev);
        assert!(got.contains(&(t0 + 30, &Event::TabRevoked { win: target, tab: old_tab })));
        assert!(got.contains(&(t0 + 30, &Event::WindowDestroyed { win: target })));
        assert!(started_created(&ev).0.is_empty() && started_created(&ev).1.is_empty());
        assert!(s.window(target).is_none());
        assert!(s.reference().window(target).is_none());
    }

    #[test]
    fn a_target_that_closes_during_an_activation() {
        let mut faults = FoldFaults::none();
        faults.activate_tab.insert(
            0,
            ActivateFault { mode: ActivateMode::Late { extra_ms: 500 }, target_closes_after_ms: Some(100) },
        );
        let mut s = SimExplorer::new(config(faults));
        open_window(&mut s);
        let target = open_window(&mut s);
        let t0 = s.now();
        s.handle_command(&Command::ActivateTab { op: OpId(1), win: target, index: 0 });
        let ev = s.tick(1_000);
        assert!(ev.iter().any(|e| e.at == t0 + 100 && e.event == Event::WindowDestroyed { win: target }));
        assert!(!ev.iter().any(|e| matches!(e.event, Event::ActivationConfirmed { .. })));
    }

    #[test]
    fn a_candidate_can_ignore_its_first_close_window() {
        let mut s = SimExplorer::new(config(FoldFaults::stubborn_candidates()));
        open_window(&mut s);
        let cand = open_candidate(&mut s);
        s.tick(200);
        let t0 = s.now();
        s.handle_command(&Command::CloseWindow { win: cand });
        let ev = s.tick(2_000);
        assert!(ev.iter().all(|e| !matches!(e.event, Event::WindowDestroyed { .. })), "ignored");
        assert!(s.window(cand).is_some());
        s.handle_command(&Command::CloseWindow { win: cand });
        let ev = s.tick(50);
        assert!(ev.iter().any(|e| e.at == t0 + 2_050 && e.event == Event::WindowDestroyed { win: cand }));
        assert_eq!(s.commands_of(CommandKind::CloseWindow).len(), 2, "both commands are recorded");
    }

    #[test]
    fn a_close_window_is_ignored_by_ordinal_for_any_window() {
        let mut faults = FoldFaults::none();
        faults.ignore_close.insert(1);
        let mut s = SimExplorer::new(config(faults));
        open_window(&mut s);
        let a = open_candidate(&mut s);
        let b = open_candidate(&mut s);
        s.handle_command(&Command::CloseWindow { win: a });
        s.handle_command(&Command::CloseWindow { win: b });
        s.tick(100);
        assert!(s.window(a).is_none());
        assert!(s.window(b).is_some(), "the second command was the ignored one");
        assert_eq!(s.close_window_commands(), 2);
    }

    #[test]
    fn an_unresponsive_window_does_its_work_when_it_answers_again() {
        let mut s = SimExplorer::new(config(FoldFaults::none()));
        let win = open_window(&mut s);
        s.apply(&Op::OpenTab { win: Pick(0), location: Pick(4), nav: NavOutcome::Complete });
        s.settle();
        s.apply(&Op::SetUnresponsive { win: Pick(0), unresponsive: true });
        assert!(s.is_unresponsive(win));

        s.handle_command(&Command::CreateTab { op: OpId(1), win });
        s.handle_command(&Command::ActivateTab { op: OpId(2), win, index: 0 });
        s.handle_command(&Command::ProbeFoldContext { probe: esr_core::engine::FoldProbeId(1), candidate: win });
        let ev = s.tick(10_000);
        assert_eq!(s.window(win).unwrap().tabs.len(), 2, "no new tab while hung");
        assert_eq!(s.window(win).unwrap().active, 1);
        assert!(ev.iter().any(|e| matches!(&e.event, Event::FoldContext { unresponsive, .. } if unresponsive.contains(&win))));
        assert!(ev.iter().all(|e| !matches!(e.event, Event::TabStarted { .. } | Event::ActivationConfirmed { .. })));

        let t1 = s.now();
        s.apply(&Op::SetUnresponsive { win: Pick(0), unresponsive: false });
        let ev = s.tick(0);
        // The activation came due first; the new tab then became the selected one.
        assert_eq!(s.window(win).unwrap().active, 2);
        assert_eq!(s.window(win).unwrap().tabs.len(), 3);
        assert!(ev.iter().any(|e| e.event == Event::ActivationConfirmed { op: OpId(2) }));
        assert!(ev.iter().all(|e| e.at == t1));
        assert!(!s.is_unresponsive(win));
    }

    #[test]
    fn an_unresponsive_candidate_does_not_close_or_hide_until_it_answers() {
        let mut s = SimExplorer::new(config(FoldFaults::none()));
        open_window(&mut s);
        let cand = open_candidate(&mut s);
        s.apply(&Op::SetUnresponsive { win: Pick(1), unresponsive: true });
        s.handle_command(&Command::HideWindow { win: cand });
        s.handle_command(&Command::CloseWindow { win: cand });
        let ev = s.tick(5_000);
        assert!(s.window(cand).is_some());
        assert!(ev.iter().all(|e| !matches!(e.event, Event::WindowDestroyed { .. })));
        assert_eq!(s.candidate(cand).unwrap().hidden_at, None);
        s.apply(&Op::SetUnresponsive { win: Pick(1), unresponsive: false });
        let ev = s.tick(200);
        assert!(ev.iter().any(|e| e.event == Event::WindowDestroyed { win: cand }));
    }

    #[test]
    fn a_closing_window_revokes_its_tabs_now_and_is_destroyed_later() {
        let mut s = SimExplorer::new(config(FoldFaults::none()));
        open_window(&mut s);
        let win = open_window(&mut s);
        s.apply(&Op::OpenTab { win: Pick(1), location: Pick(4), nav: NavOutcome::Complete });
        s.settle();
        let tabs: Vec<TabKey> = s.window(win).unwrap().tabs.iter().map(|t| t.key).collect();
        let t0 = s.now();
        let ev = s.apply(&Op::BeginClosing { win: Pick(1) });
        let revoked: Vec<&Event> = ev.iter().map(|e| &e.event).collect();
        assert_eq!(revoked.len(), 2);
        for tab in &tabs {
            assert!(revoked.contains(&&Event::TabRevoked { win, tab: *tab }));
        }
        assert!(s.window(win).is_none());
        let ev = s.tick(249);
        assert!(ev.iter().all(|e| !matches!(e.event, Event::WindowDestroyed { .. })));
        let ev = s.tick(1);
        assert_eq!(ev.len(), 1);
        assert_eq!((ev[0].at, &ev[0].event), (t0 + 250, &Event::WindowDestroyed { win }));
        assert_eq!(s.windows().len(), 1);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]

        /// Whatever the schedule and whichever windows hang or close, the
        /// simulator settles and stays coherent: a `TabStarted` names a tab of
        /// a command that was issued, once, before its `TabCreated`; a tab
        /// that never appears is never reported; no window is destroyed twice.
        ///
        /// **Validates: Requirements 14.13, 14.14, 14.23, 14.24**
        #[test]
        fn any_fault_schedule_settles_coherently(
            creates in proptest::collection::btree_map(0u64..6, (0u8..3, 0u64..3_000, 0u64..2_000, proptest::option::of(0u64..400)), 0..6),
            activates in proptest::collection::btree_map(0u64..6, (0u8..4, 0u64..3_000, proptest::option::of(0u64..400)), 0..6),
            ignored in proptest::collection::btree_set(0u64..4, 0..3),
            stubborn in any::<bool>(),
            steps in proptest::collection::vec((0u8..6, any::<u16>(), 0u64..800), 1..30),
        ) {
            let mut faults = FoldFaults { ignore_close: ignored, ignore_first_close: stubborn, ..FoldFaults::none() };
            for (n, (kind, extra, delay, closes)) in creates {
                let tab_window = match kind {
                    0 => TabWindowFault::Normal,
                    1 => TabWindowFault::Late { extra_ms: extra },
                    _ => TabWindowFault::Never,
                };
                faults.create_tab.insert(n, CreateTabFault { tab_window, registration_delay_ms: delay, target_closes_after_ms: closes });
            }
            for (n, (kind, extra, closes)) in activates {
                let mode = match kind {
                    0 => ActivateMode::Normal,
                    1 => ActivateMode::Late { extra_ms: extra },
                    2 => ActivateMode::Unconfirmed,
                    _ => ActivateMode::Ignored,
                };
                faults.activate_tab.insert(n, ActivateFault { mode, target_closes_after_ms: closes });
            }
            let mut s = SimExplorer::new(config(faults));
            let mut events = Vec::new();
            for _ in 0..3 {
                s.apply(&Op::OpenWindow {
                    process: ProcessChoice::Main,
                    location: Pick(2),
                    nav: NavOutcome::Complete,
                    rect: Rect { left: 100, top: 100, right: 900, bottom: 700 },
                    maximized: false,
                });
            }
            events.extend(s.settle());
            let mut ops = 0u64;
            let mut candidates = 0;
            for (kind, pick, ms) in steps {
                let live: Vec<WinKey> = s.windows().iter().map(|w| w.key).collect();
                let Some(win) = Pick(pick).of(live.len()).map(|i| live[i]) else { break };
                ops += 1;
                match kind {
                    0 => { s.handle_command(&Command::CreateTab { op: OpId(ops), win }); }
                    1 => { s.handle_command(&Command::ActivateTab { op: OpId(ops), win, index: usize::from(pick) % 3 }); }
                    2 => events.extend(s.apply(&Op::SetUnresponsive { win: Pick(pick), unresponsive: ms % 2 == 0 })),
                    3 => events.extend(s.apply(&Op::BeginClosing { win: Pick(pick) })),
                    4 => {
                        if candidates < 2 {
                            candidates += 1;
                            events.extend(s.apply(&Op::OpenCandidate {
                                location: Pick(pick),
                                selection: vec![],
                                bypass_keys: false,
                                desktop: crate::fold::DesktopSpec::Current,
                                foreground: None,
                                process: ProcessChoice::Main,
                            }));
                        }
                    }
                    _ => { s.handle_command(&Command::CloseWindow { win }); }
                }
                events.extend(s.tick(ms));
            }
            // Everyone answers again, then everything runs out.
            for w in s.windows().iter().map(|w| w.key).collect::<Vec<_>>() {
                s.set_unresponsive(w, false);
            }
            events.extend(s.tick(0));
            events.extend(s.finish());

            let mut started = BTreeMap::new();
            let mut destroyed = BTreeSet::new();
            for e in &events {
                match &e.event {
                    Event::TabStarted { op, tab } => {
                        prop_assert!(op.0 >= 1 && op.0 <= ops, "unknown op {:?}", op);
                        prop_assert!(started.insert(*tab, e.at).is_none(), "TabStarted twice for {:?}", tab);
                    }
                    Event::TabCreated { tab, .. } => {
                        let at = started.get(tab);
                        prop_assert!(at.is_some_and(|t| *t <= e.at), "TabCreated without an earlier TabStarted");
                    }
                    Event::WindowDestroyed { win } => prop_assert!(destroyed.insert(*win), "destroyed twice"),
                    _ => {}
                }
            }
            prop_assert!(s.next_due().is_none(), "nothing left to run");
        }
    }
}
