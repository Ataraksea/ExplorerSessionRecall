//! [`RestoreSession`]: the restoration state machine (see the module docs of
//! [`crate::restore`]).

use std::collections::{BTreeMap, HashMap};

use crate::engine::{
    Command, Event, IoErrorKind, ItemId, Millis, OpId, ProbeId, RequestId, ShellError, TabKey, TimerId, TimerOwner,
    WinKey, WindowObservation,
};
use crate::logfmt::entry::LogEntry;
use crate::model::{Display, LocationId, Snapshot, WindowId};
use crate::plan::defer::{choose_deferred, placeholder_location, Deferral, DeferralConfig};
use crate::plan::fallback::{choose_fallback, Fallback, OmitReason};
use crate::plan::{
    plan_restore, Availability, LaunchInfo, PlannedWindow, RestorePlan, Slot, TabRef, TabResolution,
};

use super::window::{target, LiveWindow, PhysTab, SlotKey, Verification};
use super::{timeout_ms, UIA_RETRIES};

/// What the caller knows when it starts a restoration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreParams {
    /// The Pending_Snapshot being restored.
    pub snapshot: Snapshot,
    pub launch: LaunchInfo,
    /// Currently connected displays.
    pub displays: Vec<Display>,
    /// The Trigger_Window.
    pub trigger: WinKey,
    /// The trigger's initial tab, if known; otherwise it is learned from the
    /// trigger's next scan.
    pub trigger_tab: Option<TabKey>,
    /// The Location the trigger opened with, if known (its slot 0 is not
    /// navigated when it already shows the planned Location).
    pub trigger_location: Option<LocationId>,
    /// Items selected in the trigger's initial tab of a Targeted_Launch
    /// (`InitialLocationKnown { selection }`), applied to the requested tab
    /// (Req 6.4). Ignored for other launches.
    pub requested_selection: Vec<ItemId>,
    /// `seq` of the durable `WriteSessionFile` that incremented
    /// `failed_attempts`, already issued by the caller.
    pub attempt_write_seq: u64,
    /// First value for the session's request, op, and probe ids. They share
    /// one counter, so `OpId(request.0)` (how an `OpenWindow` failure is
    /// reported) never names another operation.
    pub first_id: u64,
}

/// Why a session stopped early.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The attempt-counter write failed; nothing was opened.
    AttemptWriteFailed(IoErrorKind),
    /// One of the session's operations failed (an `ActivateTab` never does:
    /// it is retried, then given up without failing the session).
    OperationFailed { op: OpId, kind: OpKind, error: ShellError },
    /// One of the session's operations did not complete within `after_ms`
    /// ([`crate::restore::timeout_ms`]; again not for `ActivateTab`).
    Timeout { op: OpId, kind: OpKind, after_ms: u64 },
    /// A window being restored (the trigger or one the session opened) closed.
    WindowClosed(WinKey),
    /// The shell's connection was lost (`ShellLost`: explorer.exe ended or
    /// crashed) while the session ran. Every outstanding operation goes
    /// into that process, so the session stops at once instead of at their
    /// timeouts (task 16.3 round 4c).
    ShellLost,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Waiting for `WriteCompleted { seq }`; no command issued yet.
    AwaitingAttemptWrite { seq: u64 },
    Running,
    /// Restoration_Completion reached; no further commands.
    Completed,
    /// Stopped; no further restoration commands (windows opened later for
    /// an outstanding request are still revealed).
    Failed(Failure),
}

/// Summary of a completed restoration (the `RESTORE` entry, Req 13.2, plus
/// what the Engine needs to rebuild the Live_Session).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Completion {
    pub targeted: bool,
    /// Restored Explorer_Windows, including the trigger (0 for an empty plan).
    pub windows: u32,
    /// Restored tabs, including redirected ones, excluding the requested tab
    /// of a Targeted_Launch.
    pub tabs: u32,
    /// Omitted saved tabs, including every tab of an omitted window.
    pub omitted: u32,
    /// Tabs redirected to an ancestor folder (also counted in `tabs`).
    pub redirected: u32,
    /// Tabs restored as deferred tabs (Req 5.13; also counted in `tabs`).
    pub deferred: u32,
    /// From the session's start (right after the trigger appeared, when the
    /// attempt-counter write was issued) to completion.
    pub duration_ms: u64,
    /// The restored windows, topmost first: the planned z-order, which is
    /// the restored Activation_Order (Req 5.7, 6.3).
    pub z_order: Vec<WinKey>,
    /// The window given keyboard focus (Req 5.10, 5.11, 6.3).
    pub focus: Option<WinKey>,
}

/// How a session ended, reported once by [`RestoreSession::take_outcome`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RestoreOutcome {
    Completed(Completion),
    Failed(Failure),
}

/// Kind of an operation (`SelectItems` has no completion event and is never
/// outstanding; `ActivateTab` is outstanding until a scan confirms it).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OpKind {
    OpenWindow,
    CreateTab,
    Navigate,
    CloseTab,
    ActivateTab,
    SelectItems,
}

/// An operation without its completion event yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OutstandingOp {
    pub kind: OpKind,
    pub issued_at: Millis,
}

/// Id allocation and the outstanding-operation registry.
#[derive(Clone, Debug, Default)]
pub(crate) struct Ops {
    next: u64,
    live: BTreeMap<OpId, OutstandingOp>,
}

impl Ops {
    fn id(&mut self) -> u64 {
        let id = self.next;
        self.next += 1;
        id
    }

    pub fn issue(&mut self, kind: OpKind, now: Millis) -> OpId {
        let op = OpId(self.id());
        self.live.insert(op, OutstandingOp { kind, issued_at: now });
        op
    }

    pub fn done(&mut self, op: OpId) {
        self.live.remove(&op);
    }

    /// Restarts the deadline of an outstanding `op` at `now`.
    pub fn restart(&mut self, op: OpId, now: Millis) {
        if let Some(o) = self.live.get_mut(&op) {
            o.issued_at = now;
        }
    }

    /// An id for a command without a completion event (not outstanding).
    pub fn untracked(&mut self) -> OpId {
        OpId(self.id())
    }
}

/// The restoration state machine.
#[derive(Clone, Debug)]
pub struct RestoreSession {
    snapshot: Snapshot,
    launch: LaunchInfo,
    displays: Vec<Display>,
    trigger_key: WinKey,
    phase: Phase,
    availability: Availability,
    probes: BTreeMap<ProbeId, TabRef>,
    plan: RestorePlan,
    /// The trigger; bound to a saved window once the mapping is decided.
    trigger: LiveWindow,
    trigger_source: Option<WindowId>,
    /// Windows the session opened (or requested), by saved window.
    opened: BTreeMap<WindowId, LiveWindow>,
    requests: BTreeMap<RequestId, WindowId>,
    ops: Ops,
    started_at: Millis,
    /// Targeted_Launch selection for the requested tab (Req 6.4).
    selection: Vec<ItemId>,
    select_failures: u32,
    /// Set when `SelectItems` failed more than [`UIA_RETRIES`] times; the
    /// selection is then given up (not a restoration failure).
    selection_error: Option<ShellError>,
    /// `seq` of the session's `Restore` timer (its `first_id`).
    timer_seq: u64,
    /// Due time of the armed timer.
    armed: Option<Millis>,
    /// The completion or failure was concluded (commands issued, outcome
    /// recorded).
    concluded: bool,
    outcome: Option<RestoreOutcome>,
    /// Deferred tabs (Req 5.13), set by [`Self::with_deferral`].
    deferral: Option<SessionDeferral>,
}

/// The deferred tabs of a session and their placeholders.
#[derive(Clone, Debug)]
struct SessionDeferral {
    budget: usize,
    chosen: Deferral,
    placeholders: BTreeMap<TabRef, LocationId>,
    by_placeholder: HashMap<LocationId, TabRef>,
}

impl RestoreSession {
    /// Starts a session. It waits for the attempt-counter write before
    /// issuing anything, so no commands are returned here.
    pub fn new(now: Millis, params: RestoreParams) -> Self {
        let RestoreParams {
            snapshot,
            launch,
            displays,
            trigger,
            trigger_tab,
            trigger_location,
            requested_selection,
            attempt_write_seq,
            first_id,
        } = params;
        let availability = Availability::new();
        let plan = plan_restore(&snapshot, &launch, &availability, &displays);
        let selection = if matches!(launch, LaunchInfo::Targeted { .. }) { requested_selection } else { Vec::new() };
        RestoreSession {
            snapshot,
            launch,
            displays,
            trigger_key: trigger,
            phase: Phase::AwaitingAttemptWrite { seq: attempt_write_seq },
            availability,
            probes: BTreeMap::new(),
            plan,
            trigger: LiveWindow::new(Some(trigger), PhysTab::new(trigger_tab, None, trigger_location)),
            trigger_source: None,
            opened: BTreeMap::new(),
            requests: BTreeMap::new(),
            ops: Ops { next: first_id, live: BTreeMap::new() },
            started_at: now,
            selection,
            select_failures: 0,
            selection_error: None,
            timer_seq: first_id,
            armed: None,
            concluded: false,
            outcome: None,
            deferral: None,
        }
    }

    /// Restores the tabs beyond `config.budget` distinct saved Locations as
    /// deferred tabs (Req 5.13; [`choose_deferred`]): their slots show a
    /// [`placeholder_location`] instead. Call before the attempt-counter
    /// write completes (nothing has been issued then). Nothing changes when
    /// the snapshot is within the budget.
    pub fn with_deferral(mut self, config: &DeferralConfig) -> Self {
        let chosen = choose_deferred(&self.snapshot, &self.launch, config.budget);
        if chosen.deferred.is_empty() {
            return self;
        }
        let mut placeholders = BTreeMap::new();
        let mut by_placeholder = HashMap::new();
        for (index, tab) in chosen.deferred.iter().enumerate() {
            let Some(saved) = self.saved_location(*tab) else { continue };
            let p = placeholder_location(&config.placeholder_dir, index, saved);
            by_placeholder.insert(p.clone(), *tab);
            placeholders.insert(*tab, p);
        }
        self.deferral = Some(SessionDeferral { budget: config.budget, chosen, placeholders, by_placeholder });
        self.plan = self.replan();
        self
    }

    /// `plan_restore` for the current availability, with the Location of
    /// every deferred tab's slot replaced by its placeholder.
    fn replan(&self) -> RestorePlan {
        let mut plan = plan_restore(&self.snapshot, &self.launch, &self.availability, &self.displays);
        if let Some(d) = &self.deferral {
            for pw in &mut plan.windows {
                for slot in &mut pw.slots {
                    if let Slot::Restore { tab_index, location } = slot
                        && let Some(p) = d.placeholders.get(&TabRef { window: pw.source, tab: *tab_index })
                    {
                        *location = p.clone();
                    }
                }
            }
        }
        plan
    }

    /// The Location a deferred tab showing `placeholder` stands for (its
    /// saved Location, or the ancestor it was redirected to), if
    /// `placeholder` is one of this session's placeholders.
    pub fn deferred_target(&self, placeholder: &LocationId) -> Option<LocationId> {
        let d = self.deferral.as_ref()?;
        let tab = *d.by_placeholder.get(placeholder)?;
        match self.availability.get(tab) {
            TabResolution::Resolved(Fallback::Redirected(a)) => Some(a),
            TabResolution::Resolved(Fallback::Omitted(_)) => None,
            _ => self.saved_location(tab).cloned(),
        }
    }

    /// Every (placeholder, target) pair of this session's deferred tabs
    /// that are not omitted (see [`Self::deferred_target`]).
    pub fn deferred_targets(&self) -> Vec<(LocationId, LocationId)> {
        let Some(d) = &self.deferral else { return Vec::new() };
        d.placeholders.values().filter_map(|p| Some((p.clone(), self.deferred_target(p)?))).collect()
    }

    /// The saved tabs restored as deferred tabs.
    pub fn deferred_tabs(&self) -> Vec<TabRef> {
        self.deferral.as_ref().map_or_else(Vec::new, |d| d.chosen.deferred.iter().copied().collect())
    }

    /// Some tab of this session is deferred.
    pub fn has_deferred(&self) -> bool {
        self.deferral.is_some()
    }

    pub fn phase(&self) -> &Phase {
        &self.phase
    }

    /// Completed or failed: the session issues no further restoration
    /// command (a restoration is in progress while a session is not finished).
    pub fn is_finished(&self) -> bool {
        matches!(self.phase, Phase::Completed | Phase::Failed(_))
    }

    /// How the session ended, once, right after the event that ended it
    /// (see the module docs, "Engine contract").
    pub fn take_outcome(&mut self) -> Option<RestoreOutcome> {
        self.outcome.take()
    }

    /// The id of the session's `Restore` timer.
    pub fn timer_id(&self) -> TimerId {
        TimerId { owner: TimerOwner::Restore, seq: self.timer_seq }
    }

    /// The current plan (re-planned on every probe result).
    pub fn plan(&self) -> &RestorePlan {
        &self.plan
    }

    pub fn availability(&self) -> &Availability {
        &self.availability
    }

    pub fn started_at(&self) -> Millis {
        self.started_at
    }

    pub fn trigger(&self) -> WinKey {
        self.trigger_key
    }

    /// The next unused id (for the caller's next session).
    pub fn next_id(&self) -> u64 {
        self.ops.next
    }

    /// Operations still waiting for their completion event (for timeouts).
    pub fn outstanding_ops(&self) -> impl Iterator<Item = (OpId, OutstandingOp)> + '_ {
        self.ops.live.iter().map(|(op, o)| (*op, *o))
    }

    /// Probes still outstanding.
    pub fn pending_probes(&self) -> usize {
        self.probes.len()
    }

    /// `win` is the trigger or a window this session opened.
    pub fn owns_window(&self, win: WinKey) -> bool {
        win == self.trigger_key || self.opened.values().any(|w| w.key == Some(win))
    }

    /// `request` is an `OpenWindow` of this session not claimed yet.
    pub fn is_pending_request(&self, request: RequestId) -> bool {
        self.requests.contains_key(&request)
    }

    /// Some `OpenWindow` of this session is not claimed yet (also after the
    /// session finished: such a window is still revealed when it arrives).
    pub fn has_pending_requests(&self) -> bool {
        !self.requests.is_empty()
    }

    /// Whether a scan of `win` that the session did not request
    /// (`RescanWindow`) can still matter to it: always once the session is
    /// finished or for a window it does not own; for its own windows only
    /// while a tab key is still unknown, a verification scan is awaited, or
    /// an activation is unconfirmed. The adapters may defer other tracking
    /// scans of a window under construction until then (they compete with
    /// Explorer building its tabs; the Live_Session is rebuilt by a full
    /// rescan at completion anyway).
    pub fn needs_scan(&self, win: WinKey) -> bool {
        if self.is_finished() {
            return true;
        }
        let w = if win == self.trigger_key {
            Some(&self.trigger)
        } else {
            self.opened.values().find(|w| w.key == Some(win))
        };
        w.is_none_or(|w| {
            w.tabs.iter().any(|t| t.key.is_none())
                || w.verification == Verification::AwaitingScan
                || (w.verification == Verification::Verified && w.activated.is_some_and(|_| !w.active_confirmed))
        })
    }

    /// The live window restoring saved window `source`, if any.
    pub fn window_for(&self, source: WindowId) -> Option<WinKey> {
        if self.trigger_source == Some(source) {
            return Some(self.trigger_key);
        }
        self.opened.get(&source).and_then(|w| w.key)
    }

    /// The tab keys of the window restoring `source`, left to right, as the
    /// session believes Explorer shows them.
    pub fn tab_keys(&self, source: WindowId) -> Vec<Option<TabKey>> {
        let w = if self.trigger_source == Some(source) { Some(&self.trigger) } else { self.opened.get(&source) };
        w.map_or_else(Vec::new, |w| w.tabs.iter().map(|t| t.key).collect())
    }

    /// Every probe resolved and every planned window built at its final slot
    /// count, navigated, and verified against UIA; for a Targeted_Launch the
    /// trigger's requested tab is also activated and has its selection
    /// applied (completion proper additionally needs every window's
    /// activation confirmed; see the module docs). Stays true once the
    /// session completed.
    pub fn is_settled(&self) -> bool {
        if !matches!(self.phase, Phase::Running | Phase::Completed) || !self.probes.is_empty() {
            return false;
        }
        self.plan.windows.iter().all(|pw| {
            let live = if pw.is_trigger {
                Some(&self.trigger).filter(|w| w.placement_applied)
            } else {
                self.opened.get(&pw.source)
            };
            live.is_some_and(|w| {
                if self.targeted_trigger(pw) {
                    w.is_finished_targeted(pw, &self.selection, self.selection_error.is_none())
                } else {
                    w.is_settled(pw)
                }
            })
        })
    }

    /// For every planned window that has a live window, its key and the
    /// visual index of its planned Active_Tab: the saved Active_Tab's
    /// position (Req 5.4, 9.4), or the requested tab, last, for the trigger
    /// of a Targeted_Launch (Req 6.3). In plan order.
    pub fn active_tabs(&self) -> Vec<(WinKey, usize)> {
        self.plan.windows.iter().filter_map(|pw| Some((self.window_for(pw.source)?, pw.active))).collect()
    }

    /// The trigger's tab showing the requested Location of a Targeted_Launch
    /// (bound to the last slot), once known.
    pub fn requested_tab(&self) -> Option<TabKey> {
        self.trigger.tab_for(SlotKey::Requested).and_then(|t| t.key)
    }

    /// The selection applied to the requested tab (empty unless Targeted).
    pub fn requested_selection(&self) -> &[ItemId] {
        &self.selection
    }

    /// The last `SelectItems` error once the selection was given up.
    pub fn selection_error(&self) -> Option<&ShellError> {
        self.selection_error.as_ref()
    }

    /// Feeds one event; returns the commands to carry out.
    pub fn handle(&mut self, now: Millis, event: &Event) -> Vec<Command> {
        let mut out = Vec::new();
        match self.phase {
            Phase::AwaitingAttemptWrite { .. } | Phase::Running => self.step(now, event, &mut out),
            Phase::Failed(_) => self.after_failure(event, &mut out),
            Phase::Completed => {}
        }
        self.conclude(now, &mut out);
        out
    }

    fn step(&mut self, now: Millis, event: &Event, out: &mut Vec<Command>) {
        if let Phase::AwaitingAttemptWrite { seq } = self.phase {
            if let Event::WriteCompleted { seq: done, result } = event
                && *done == seq
            {
                match result {
                    Ok(()) => {
                        self.phase = Phase::Running;
                        if let Some(d) = &self.deferral {
                            out.push(Command::AppendLog(LogEntry::TabsDeferred {
                                distinct: count_u32(d.chosen.distinct),
                                immediate: count_u32(d.chosen.immediate),
                                deferred: count_u32(d.chosen.deferred.len()),
                                budget: count_u32(d.budget),
                            }));
                        }
                        self.issue_probes(out);
                        self.drive(now, out);
                    }
                    Err(e) => self.phase = Phase::Failed(Failure::AttemptWriteFailed(*e)),
                }
            } else if matches!(event, Event::ShellLost) {
                // The attempt is counted (its write is under way); nothing
                // is opened in a shell that is gone.
                self.phase = Phase::Failed(Failure::ShellLost);
            }
            return;
        }
        match event {
            Event::Timer(id) => {
                if *id != self.timer_id() {
                    return;
                }
                self.armed = None;
                self.check_timeouts(now, out);
                // Falls through to the drive, which re-issues a timed-out
                // activation.
            }
            Event::ProbeCompleted { probe, outcome } => {
                let Some(tab) = self.probes.remove(probe) else { return };
                let Some(saved) = self.saved_location(tab) else { return };
                let fallback = choose_fallback(saved, outcome);
                self.availability.resolve(tab, fallback);
                self.plan = self.replan();
            }
            Event::WindowOpened { request, win, tab } => {
                let Some(source) = self.requests.remove(request) else { return };
                self.ops.done(OpId(request.0));
                if let Some(w) = self.opened.get_mut(&source) {
                    w.key = Some(*win);
                    match (tab, w.tabs.first_mut()) {
                        // The adapter reported the initial tab.
                        (Some(t), Some(first)) if first.key.is_none() => first.key = Some(*t),
                        // Learn the initial tab's key from a scan.
                        _ => out.push(Command::RescanWindow(*win)),
                    }
                }
            }
            Event::TabCreated { win, tab } => {
                let Some((source, w)) = Self::find_mut(&mut self.trigger, &mut self.opened, self.trigger_source, *win)
                else {
                    return;
                };
                let Some(op) = w.creating else { return }; // not ours
                self.ops.done(op);
                w.tab_created(*tab, source.and_then(|s| Self::planned(&self.plan, s)));
            }
            Event::NavigationCompleted { tab, location } => {
                let ops = &mut self.ops;
                let _ = std::iter::once(&mut self.trigger)
                    .chain(self.opened.values_mut())
                    .any(|w| w.navigation_completed(*tab, location, ops));
            }
            Event::WindowScanned(obs) => self.scanned(obs),
            Event::ActivationConfirmed { op } => {
                // The adapter saw the tab selected. A settled window (other
                // than a Targeted trigger) is confirmed at once (task 16.3:
                // the confirming scan's strip read took up to 1.5 s while
                // Explorer was still busy); otherwise the next scan of the
                // window confirms it, so ask for that scan.
                let plan = &self.plan;
                let ops = &mut self.ops;
                let trigger_pw = self.trigger_source.and_then(|s| Self::planned(plan, s)).filter(|_| !plan.targeted);
                let windows = std::iter::once((&mut self.trigger, trigger_pw))
                    .chain(self.opened.iter_mut().map(|(s, w)| (w, Self::planned(plan, *s))));
                for (w, pw) in windows {
                    if w.activation_confirmed(*op) {
                        match pw {
                            Some(pw) if w.is_settled(pw) => w.confirm_now(ops),
                            // Not settled yet (the activation was issued
                            // before the verification): the verification
                            // scan confirms it. Its deadline starts again,
                            // so that the window has time to settle.
                            Some(_) => ops.restart(*op, now),
                            // A Targeted trigger: the next scan confirms it.
                            None => out.extend(w.key.map(Command::RescanWindow)),
                        }
                        break;
                    }
                }
            }
            Event::TabRevoked { win, tab } => {
                let ops = &mut self.ops;
                if let Some((_, w)) = Self::find_mut(&mut self.trigger, &mut self.opened, self.trigger_source, *win) {
                    w.tab_revoked(*tab, ops);
                }
            }
            Event::WindowDestroyed { win } => {
                if self.owns_window(*win) {
                    self.phase = Phase::Failed(Failure::WindowClosed(*win));
                    return;
                }
            }
            Event::ShellLost => {
                self.phase = Phase::Failed(Failure::ShellLost);
                return;
            }
            Event::OperationFailed { op, error } => {
                // UIA commands first: their failures are retried, then given up.
                if self.uia_failed(*op, error, out) {
                    if self.phase != Phase::Running {
                        return;
                    }
                } else if let Some(o) = self.ops.live.get(op) {
                    self.phase = Phase::Failed(Failure::OperationFailed { op: *op, kind: o.kind, error: error.clone() });
                    return;
                } else {
                    return;
                }
            }
            _ => return,
        }
        self.drive(now, out);
    }

    /// After a failure only windows still arriving for an outstanding
    /// `OpenWindow` are handled: they are revealed (they are created hidden)
    /// and left open.
    fn after_failure(&mut self, event: &Event, out: &mut Vec<Command>) {
        if let Event::WindowOpened { request, win, .. } = event
            && let Some(source) = self.requests.remove(request)
        {
            if let Some(w) = self.opened.get_mut(&source) {
                w.key = Some(*win);
            }
            out.push(Command::Reveal { win: *win });
        }
    }

    /// Completes the session when it is ready, arms the timeout timer while
    /// it runs, and concludes a failure once.
    fn conclude(&mut self, now: Millis, out: &mut Vec<Command>) {
        if self.phase == Phase::Running && self.ready_to_complete() {
            self.complete(now, out);
        }
        if self.phase == Phase::Running {
            self.arm_timer(out);
        }
        if let Phase::Failed(failure) = &self.phase
            && !self.concluded
        {
            let failure = failure.clone();
            self.concluded = true;
            self.ops.live.clear();
            if let Some(entry) = error_entry(&failure) {
                out.push(Command::AppendLog(entry));
            }
            let closed = if let Failure::WindowClosed(w) = failure { Some(w) } else { None };
            for w in self.opened.values() {
                if let Some(win) = w.key.filter(|k| Some(*k) != closed) {
                    out.push(Command::Reveal { win });
                }
            }
            self.outcome = Some(RestoreOutcome::Failed(failure));
        }
    }

    /// Settled, and every planned window's activation confirmed.
    fn ready_to_complete(&self) -> bool {
        self.is_settled()
            && self.plan.windows.iter().all(|pw| self.live_for(pw).is_some_and(|w| w.is_active_confirmed(pw)))
    }

    /// Restoration_Completion (see the module docs, "Completion").
    fn complete(&mut self, now: Millis, out: &mut Vec<Command>) {
        let plan = &self.plan;
        let keys: Vec<Option<WinKey>> = plan.windows.iter().map(|pw| self.window_for(pw.source)).collect();
        for (pw, key) in plan.windows.iter().zip(&keys) {
            if let (false, Some(win)) = (pw.is_trigger, key) {
                out.push(Command::Reveal { win: *win });
            }
        }
        let z_order: Vec<WinKey> = plan.z_order.iter().filter_map(|&i| keys.get(i).copied().flatten()).collect();
        let focus = plan.focus.and_then(|i| keys.get(i).copied().flatten());
        if !z_order.is_empty() {
            out.push(Command::SetZOrder(z_order.clone()));
        }
        if let Some(win) = focus {
            out.push(Command::Focus(win));
        }

        let mut tab_entries = Vec::new();
        let (mut omitted, mut redirected) = (0u32, 0u32);
        for &id in &self.snapshot.activation {
            let Some(w) = self.snapshot.window(id) else { continue };
            for (i, t) in w.tabs.iter().enumerate() {
                match self.availability.get(TabRef { window: id, tab: i }) {
                    TabResolution::Resolved(Fallback::Omitted(reason)) => {
                        omitted += 1;
                        tab_entries.push(LogEntry::TabOmitted { location: t.location.clone(), reason });
                    }
                    TabResolution::Resolved(Fallback::Redirected(ancestor)) => {
                        redirected += 1;
                        tab_entries.push(LogEntry::TabRedirected {
                            location: t.location.clone(),
                            reason: OmitReason::NotExist,
                            ancestor,
                        });
                    }
                    _ => {}
                }
            }
        }
        let tabs = plan.windows.iter().flat_map(|pw| &pw.slots).filter(|s| matches!(s, Slot::Restore { .. })).count();
        let deferred = self.deferred_targets().len();
        let completion = Completion {
            targeted: plan.targeted,
            windows: count_u32(plan.windows.len()),
            tabs: count_u32(tabs),
            omitted,
            redirected,
            deferred: count_u32(deferred),
            duration_ms: now.since(self.started_at),
            z_order,
            focus,
        };
        out.push(Command::AppendLog(LogEntry::Restore {
            targeted: completion.targeted,
            windows: completion.windows,
            tabs: completion.tabs,
            omitted,
            redirected,
            duration_ms: completion.duration_ms,
        }));
        out.extend(tab_entries.into_iter().map(Command::AppendLog));
        if omitted > 0 {
            out.push(Command::Notify { omitted_tabs: omitted });
        }
        self.phase = Phase::Completed;
        self.concluded = true;
        self.outcome = Some(RestoreOutcome::Completed(completion));
    }

    /// Handles outstanding operations past their deadline: an `ActivateTab`
    /// not confirmed in time counts as a failed activation attempt
    /// ([`Self::uia_failed`]); any other operation fails the session (the
    /// earliest one is reported).
    fn check_timeouts(&mut self, now: Millis, out: &mut Vec<Command>) {
        let mut expired: Vec<(Millis, OpId, OpKind, u64)> = self
            .ops
            .live
            .iter()
            .filter_map(|(op, o)| timeout_ms(o.kind).map(|t| (o.issued_at + t, *op, o.kind, t)))
            .filter(|(due, ..)| *due <= now)
            .collect();
        expired.sort_by_key(|(due, op, ..)| (*due, *op));
        for &(_, op, kind, after_ms) in &expired {
            if kind == OpKind::ActivateTab {
                let error = ShellError { code: 0, message: format!("not confirmed within {after_ms} ms") };
                self.uia_failed(op, &error, out);
            }
        }
        if let Some(&(_, op, kind, after_ms)) = expired.iter().find(|(.., kind, _)| *kind != OpKind::ActivateTab) {
            self.phase = Phase::Failed(Failure::Timeout { op, kind, after_ms });
        }
    }

    /// Arms the timer for the earliest deadline unless an earlier one is
    /// already armed (an armed timer that turns out early just re-arms).
    fn arm_timer(&mut self, out: &mut Vec<Command>) {
        let next = self.ops.live.values().filter_map(|o| timeout_ms(o.kind).map(|t| o.issued_at + t)).min();
        if let Some(at) = next
            && self.armed.is_none_or(|a| at < a)
        {
            self.armed = Some(at);
            out.push(Command::SetTimer { id: self.timer_id(), at });
        }
    }

    // --- internals -----------------------------------------------------------

    /// The live window restoring `pw`, if any.
    fn live_for(&self, pw: &PlannedWindow) -> Option<&LiveWindow> {
        if pw.is_trigger { Some(&self.trigger) } else { self.opened.get(&pw.source) }
    }

    /// `pw` is the trigger of a Targeted_Launch (its last slot is requested).
    fn targeted_trigger(&self, pw: &PlannedWindow) -> bool {
        self.plan.targeted && pw.is_trigger
    }

    /// A failed `ActivateTab` or `SelectItems` of this session: retried
    /// [`UIA_RETRIES`] times (the next drive re-issues it). Then the
    /// activation is given up and logged, and the window completes with
    /// whatever tab Explorer shows (only the Active_Tab can be wrong, the
    /// restoration does not fail; design.md, "Error Handling"); the selection
    /// is given up ([`Self::selection_error`]). Returns `false` if `op` is not
    /// one of ours.
    fn uia_failed(&mut self, op: OpId, error: &ShellError, out: &mut Vec<Command>) -> bool {
        for w in std::iter::once(&mut self.trigger).chain(self.opened.values_mut()) {
            if let Some(abandoned) = w.activation_failed(op, &mut self.ops) {
                if abandoned {
                    out.push(Command::AppendLog(activation_abandoned_entry(error)));
                }
                return true;
            }
            if let Some(t) = w.tabs.iter_mut().find(|t| t.selected == Some(op)) {
                t.selected = None;
                self.select_failures += 1;
                if self.select_failures > UIA_RETRIES {
                    self.selection_error = Some(error.clone());
                }
                return true;
            }
        }
        false
    }
    fn saved_location(&self, tab: TabRef) -> Option<&LocationId> {
        self.snapshot.window(tab.window).and_then(|w| w.tabs.get(tab.tab)).map(|t| &t.location)
    }

    fn issue_probes(&mut self, out: &mut Vec<Command>) {
        for &id in &self.snapshot.activation {
            let Some(w) = self.snapshot.window(id) else { continue };
            for (i, t) in w.tabs.iter().enumerate() {
                let probe = ProbeId(self.ops.id());
                self.probes.insert(probe, TabRef { window: id, tab: i });
                out.push(Command::Probe { probe, location: t.location.clone() });
            }
        }
    }

    fn planned(plan: &RestorePlan, source: WindowId) -> Option<&PlannedWindow> {
        plan.windows.iter().find(|w| w.source == source)
    }

    /// The window with key `win` and the saved window it restores (`None`
    /// source for the trigger before its mapping is decided).
    fn find_mut<'a>(
        trigger: &'a mut LiveWindow,
        opened: &'a mut BTreeMap<WindowId, LiveWindow>,
        trigger_source: Option<WindowId>,
        win: WinKey,
    ) -> Option<(Option<WindowId>, &'a mut LiveWindow)> {
        if trigger.key == Some(win) {
            return Some((trigger_source, trigger));
        }
        opened.iter_mut().find(|(_, w)| w.key == Some(win)).map(|(s, w)| (Some(*s), w))
    }

    fn scanned(&mut self, obs: &WindowObservation) {
        let Some((source, w)) = Self::find_mut(&mut self.trigger, &mut self.opened, self.trigger_source, obs.win)
        else {
            return;
        };
        w.learn_keys(obs);
        let Some(pw) = source.and_then(|s| Self::planned(&self.plan, s)) else { return };
        w.verify(pw, obs);
        // The activation barrier. A scan showing another tab selected leaves
        // the activation outstanding: it runs asynchronously, and its success
        // triggers another scan; a failure report or its deadline counts as a
        // failed attempt.
        let _ = w.confirm_active(pw, obs, &mut self.ops);
    }

    /// One pass over every window towards the current plan.
    fn drive(&mut self, now: Millis, out: &mut Vec<Command>) {
        if self.phase != Phase::Running {
            return;
        }
        let plan = &self.plan;

        // Other windows: opened once decided and known restorable (Req 5.1,
        // 7.2, 7.3), all in parallel. Before the trigger's commands, so that
        // the trigger's synchronous first navigation does not hold up their
        // `ShellExecuteExW` (task 16.3: ~90 ms).
        for pw in plan.windows.iter().filter(|w| !w.is_trigger) {
            if let Some(w) = self.opened.get_mut(&pw.source) {
                w.drive(pw, &mut self.ops, now, out);
                w.finish(pw, &mut self.ops, now, out);
                continue;
            }
            if !pw.decided {
                continue;
            }
            let Some(location) = pw.slots.iter().find(|s| matches!(s, Slot::Restore { .. })).and_then(target) else {
                continue;
            };
            let request = RequestId(self.ops.issue(OpKind::OpenWindow, now).0);
            self.requests.insert(request, pw.source);
            // The initial tab is slot 0's tab and shows `location`.
            let first = PhysTab::new(None, Some(SlotKey::of(&pw.slots[0])), Some(location.clone()));
            self.opened.insert(pw.source, LiveWindow::new(None, first));
            out.push(Command::OpenWindow { request, location: location.clone(), placement: pw.placement });
        }

        // The trigger: built once the mapping is decided; placed once its
        // placement is final (Req 7.7).
        if plan.trigger_decided
            && let Some(pw) = plan.windows.first()
        {
            let source = *self.trigger_source.get_or_insert(pw.source);
            debug_assert_eq!(source, pw.source, "a decided trigger mapping never changes");
            if pw.decided && !self.trigger.placement_applied {
                self.trigger.placement_applied = true;
                out.push(Command::ApplyPlacement { win: self.trigger_key, placement: pw.placement });
            }
            self.trigger.drive(pw, &mut self.ops, now, out);
            // Activate the planned tab once the trigger is settled; for a
            // Targeted_Launch, the requested slot, then the captured
            // selection (Req 5.4, 6.3, 6.4).
            if plan.targeted {
                let select = self.selection_error.is_none();
                self.trigger.finish_targeted(pw, &self.selection, select, &mut self.ops, now, out);
            } else {
                self.trigger.finish(pw, &mut self.ops, now, out);
            }
        }
    }
}

fn count_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The `operation` token of an `ERROR` entry for a failed `kind`.
fn operation_name(kind: OpKind) -> &'static str {
    match kind {
        OpKind::OpenWindow => "restore-open-window",
        OpKind::CreateTab => "restore-create-tab",
        OpKind::Navigate => "restore-navigate",
        OpKind::CloseTab => "restore-close-tab",
        OpKind::ActivateTab => "restore-activate-tab",
        OpKind::SelectItems => "restore-select-items",
    }
}

/// The `ERROR` entry for an activation given up after its retries (Req
/// 13.3). The restoration continues; only that window's Active_Tab may differ
/// from the saved one.
fn activation_abandoned_entry(error: &ShellError) -> LogEntry {
    LogEntry::Error {
        operation: operation_name(OpKind::ActivateTab).to_string(),
        description: format!(
            "{} (0x{:08X}); the restoration continues without the saved active tab",
            error.message, error.code as u32
        ),
    }
}

/// The `ERROR` entry for a failure (Req 4.10, 13.3). A failed attempt-counter
/// write is logged by the store already, so it gets none here.
fn error_entry(failure: &Failure) -> Option<LogEntry> {
    let (operation, description) = match failure {
        Failure::AttemptWriteFailed(_) => return None,
        Failure::OperationFailed { kind, error, .. } => {
            (operation_name(*kind), format!("{} (0x{:08X})", error.message, error.code as u32))
        }
        Failure::Timeout { kind, after_ms, .. } => {
            (operation_name(*kind), format!("no completion within {after_ms} ms"))
        }
        Failure::WindowClosed(_) => ("restore-window", "a window being restored was closed".to_string()),
        Failure::ShellLost => (
            "restore-shell",
            "the connection to explorer.exe was lost (the shell process ended); the restoration is retried \
             at the next Trigger_Window"
                .to_string(),
        ),
    };
    Some(LogEntry::Error { operation: operation.to_string(), description })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{ObservedLocation, TabObservation, TabStripState};
    use crate::model::{DisplayRef, LocationKind, Placement, Rect, ShowState, TabRec, Volume, WindowRec};
    use crate::plan::fallback::ProbeOutcome;

    const TRIGGER: WinKey = WinKey { hwnd: 1, seq: 1 };

    fn fs(name: &str) -> LocationId {
        LocationId {
            case_sensitive: false,
            kind: LocationKind::FileSystem(Volume::Fixed),
            parsing_name: format!(r"C:\{name}").as_str().into(),
            display_name: name.into(),
            pidl: vec![],
            search: None,
        }
    }

    fn tab(n: u64) -> TabKey {
        TabKey { hwnd: 100 + n, seq: n }
    }

    fn placement() -> Placement {
        let r = Rect { left: 0, top: 0, right: 1920, bottom: 1080 };
        Placement {
            state: ShowState::Normal,
            restored: Rect { left: 100, top: 100, right: 900, bottom: 700 },
            display: DisplayRef { device: "P".into(), monitor: r },
        }
    }

    fn displays() -> Vec<Display> {
        let r = Rect { left: 0, top: 0, right: 1920, bottom: 1080 };
        vec![Display { device: "P".into(), monitor: r, work: r, primary: true }]
    }

    fn session(names: &[&str]) -> RestoreSession {
        let w = WindowRec {
            id: WindowId(1),
            tabs: names.iter().map(|n| TabRec { location: fs(n) }).collect(),
            active: 0,
            placement: placement(),
        };
        RestoreSession::new(
            Millis(0),
            RestoreParams {
                snapshot: Snapshot { windows: vec![w], activation: vec![WindowId(1)] },
                launch: LaunchInfo::Default,
                displays: displays(),
                trigger: TRIGGER,
                trigger_tab: Some(tab(0)),
                trigger_location: Some(fs("Home")),
                requested_selection: vec![],
                attempt_write_seq: 9,
                first_id: 1,
            },
        )
    }

    fn probes(cmds: &[Command]) -> Vec<ProbeId> {
        cmds.iter().filter_map(|c| if let Command::Probe { probe, .. } = c { Some(*probe) } else { None }).collect()
    }

    fn navigations(cmds: &[Command]) -> Vec<(TabKey, String)> {
        cmds.iter()
            .filter_map(|c| match c {
                Command::Navigate { tab, location, .. } => Some((*tab, location.display_name.to_string_lossy())),
                _ => None,
            })
            .collect()
    }

    fn scan(tabs: &[(TabKey, &str)], strip: &[&str]) -> Event {
        scan_sel(tabs, strip, 0)
    }

    /// A scan of the trigger with UIA item `selected` selected.
    fn scan_sel(tabs: &[(TabKey, &str)], strip: &[&str], selected: usize) -> Event {
        Event::WindowScanned(WindowObservation {
            win: TRIGGER,
            pid: 7,
            tabs: tabs
                .iter()
                .map(|(t, n)| TabObservation { tab: *t, location: ObservedLocation::Known(fs(n)) })
                .collect(),
            active: None,
            strip: Some(TabStripState { names: strip.iter().map(|&n| n.into()).collect(), selected: Some(selected) }),
            placement: placement(),
        })
    }

    fn is_activate(c: &Command, i: usize) -> bool {
        matches!(c, Command::ActivateTab { win: TRIGGER, index, .. } if *index == i)
    }

    /// `out` without its `SetTimer` commands (timeout timers).
    fn no_timers(mut out: Vec<Command>) -> Vec<Command> {
        out.retain(|c| !matches!(c, Command::SetTimer { .. }));
        out
    }

    /// Runs a session over `names` (all available) to the point where the
    /// trigger is built and a verification rescan was requested.
    fn built(names: &[&str]) -> (RestoreSession, Vec<TabKey>) {
        let mut s = session(names);
        assert!(s.handle(Millis(1), &Event::WriteCompleted { seq: 8, result: Ok(()) }).is_empty(), "other seq");
        let cmds = s.handle(Millis(2), &Event::WriteCompleted { seq: 9, result: Ok(()) });
        assert!(cmds.iter().all(|c| matches!(c, Command::Probe { .. })));
        let ps = probes(&cmds);
        assert_eq!(ps.len(), names.len());
        let mut keys = vec![tab(0)];
        let mut all = Vec::new();
        for p in ps {
            all.extend(s.handle(Millis(3), &Event::ProbeCompleted { probe: p, outcome: ProbeOutcome::Available }));
        }
        for i in 1..names.len() as u64 {
            assert!(all.iter().any(|c| matches!(c, Command::CreateTab { win: TRIGGER, .. })));
            all.retain(|c| !matches!(c, Command::CreateTab { .. }));
            keys.push(tab(i));
            all.extend(s.handle(Millis(4), &Event::TabCreated { win: TRIGGER, tab: tab(i) }));
        }
        assert_eq!(navigations(&all).len(), names.len());
        assert!(!all.iter().any(|c| matches!(c, Command::ActivateTab { .. })), "{all:?}");
        let mut out = Vec::new();
        for (i, n) in names.iter().enumerate() {
            out.extend(s.handle(Millis(5), &Event::NavigationCompleted { tab: keys[i], location: fs(n) }));
        }
        // Built: the activation goes out before the verification scan (task
        // 16.3), which can then confirm both.
        assert!(matches!(no_timers(out.clone()).as_slice(), [a, Command::RescanWindow(TRIGGER)] if is_activate(a, 0)), "{out:?}");
        assert!(!s.is_settled());
        (s, keys)
    }

    #[test]
    fn matching_uia_order_settles() {
        let (mut s, k) = built(&["A", "B", "C"]);
        let out = no_timers(s.handle(Millis(6), &scan(&[(k[0], "A"), (k[1], "B"), (k[2], "C")], &["A", "B", "C"])));
        // The verification scan settles the window and, showing the planned
        // tab selected, confirms the early activation: complete.
        assert!(out.contains(&Command::Focus(TRIGGER)), "{out:?}");
        assert!(!out.iter().any(|c| matches!(c, Command::ActivateTab { .. })), "{out:?}");
        assert!(s.is_settled());
        assert_eq!(s.phase(), &Phase::Completed);
        assert_eq!(s.tab_keys(WindowId(1)), k.iter().copied().map(Some).collect::<Vec<_>>());
    }

    #[test]
    fn mismatched_uia_order_is_fixed_by_renavigating() {
        let (mut s, k) = built(&["A", "B"]);
        // Explorer shows the tabs swapped.
        let out = s.handle(Millis(6), &scan(&[(k[0], "A"), (k[1], "B")], &["B", "A"]));
        let mut navs = navigations(&out);
        navs.sort();
        assert_eq!(navs, vec![(k[0], "B".to_string()), (k[1], "A".to_string())]);
        assert_eq!(s.tab_keys(WindowId(1)), vec![Some(k[1]), Some(k[0])]);
        let mut out = s.handle(Millis(7), &Event::NavigationCompleted { tab: k[0], location: fs("B") });
        out.extend(s.handle(Millis(7), &Event::NavigationCompleted { tab: k[1], location: fs("A") }));
        assert_eq!(out, vec![Command::RescanWindow(TRIGGER)]);
        let out = s.handle(Millis(8), &scan(&[(k[0], "B"), (k[1], "A")], &["A", "B"]));
        // The early activation (by position) stays valid across the
        // re-binding; this scan confirms it.
        assert!(!out.iter().any(|c| matches!(c, Command::ActivateTab { .. })), "{out:?}");
        assert!(out.contains(&Command::Focus(TRIGGER)), "{out:?}");
        assert!(s.is_settled());
    }

    #[test]
    fn unexposed_items_of_an_overflowing_strip_are_not_renavigated() {
        let (mut s, k) = built(&["A", "B", "C", "D"]);
        let tabs = [(k[0], "A"), (k[1], "B"), (k[2], "C"), (k[3], "D")];
        let out = s.handle(Millis(6), &scan(&tabs, &["C", "D"]));
        assert!(navigations(&out).is_empty(), "{out:?}");
        assert!(s.is_settled());
        // The partial strip cannot tell which tab is active: the activation
        // stays outstanding until the adapter confirms it (task 16.3).
        let out = s.handle(Millis(7), &scan_sel(&tabs, &["C", "D"], 1));
        assert!(!out.contains(&Command::Focus(TRIGGER)), "{out:?}");
        assert_eq!(s.phase(), &Phase::Running);
        let op = s.outstanding_ops().find(|(_, o)| o.kind == OpKind::ActivateTab).map(|(op, _)| op).unwrap();
        // Someone else's confirmation changes nothing.
        assert!(s.handle(Millis(8), &Event::ActivationConfirmed { op: OpId(op.0 + 1000) }).is_empty());
        // The adapter's confirmation of a settled window confirms it at once.
        let out = s.handle(Millis(9), &Event::ActivationConfirmed { op });
        assert!(out.contains(&Command::Focus(TRIGGER)), "{out:?}");
        assert_eq!(s.phase(), &Phase::Completed);
    }

    #[test]
    fn an_adapter_confirmation_before_the_verification_is_kept_for_it() {
        let (mut s, k) = built(&["A", "B"]);
        let tabs = [(k[0], "A"), (k[1], "B")];
        let op = s.outstanding_ops().find(|(_, o)| o.kind == OpKind::ActivateTab).map(|(op, _)| op).unwrap();
        // Confirmed while the verification scan is still awaited: nothing yet.
        assert!(no_timers(s.handle(Millis(6), &Event::ActivationConfirmed { op })).is_empty());
        assert_eq!(s.phase(), &Phase::Running);
        // A verification scan that cannot tell which tab is selected confirms it.
        let mut ev = scan(&tabs, &["A", "B"]);
        if let Event::WindowScanned(obs) = &mut ev {
            obs.strip.as_mut().unwrap().selected = None;
        }
        let out = s.handle(Millis(7), &ev);
        assert!(out.contains(&Command::Focus(TRIGGER)), "{out:?}");
        assert_eq!(s.phase(), &Phase::Completed);
    }

    #[test]
    fn completion_orders_focuses_logs_and_reports() {
        let (mut s, k) = built(&["A", "B"]);
        let tabs = [(k[0], "A"), (k[1], "B")];
        // Verified, but another tab is still selected.
        s.handle(Millis(6), &scan_sel(&tabs, &["A", "B"], 1));
        assert_eq!(s.outstanding_ops().filter(|(_, o)| o.kind == OpKind::ActivateTab).count(), 1);
        assert!(s.take_outcome().is_none());
        // The barrier scan shows the planned tab selected.
        let out = s.handle(Millis(9), &scan_sel(&tabs, &["A", "B"], 0));
        assert_eq!(
            out,
            vec![
                Command::SetZOrder(vec![TRIGGER]),
                Command::Focus(TRIGGER),
                Command::AppendLog(LogEntry::Restore {
                    targeted: false,
                    windows: 1,
                    tabs: 2,
                    omitted: 0,
                    redirected: 0,
                    duration_ms: 9,
                }),
            ],
            "the trigger is not revealed, nothing omitted: no Notify"
        );
        assert_eq!(s.phase(), &Phase::Completed);
        assert!(s.is_finished() && s.is_settled());
        assert_eq!(s.outstanding_ops().count(), 0);
        let Some(RestoreOutcome::Completed(c)) = s.take_outcome() else { panic!() };
        assert_eq!((c.z_order, c.focus), (vec![TRIGGER], Some(TRIGGER)));
        assert!(s.take_outcome().is_none(), "reported once");
        // Nothing more is issued.
        assert!(s.handle(Millis(10), &Event::NavigationCompleted { tab: k[0], location: fs("Home") }).is_empty());
    }

    #[test]
    fn an_unconfirmed_activation_is_retried_then_given_up_without_failing() {
        let (mut s, k) = built(&["A", "B"]);
        let tabs = [(k[0], "A"), (k[1], "B")];
        s.handle(Millis(6), &scan_sel(&tabs, &["A", "B"], 1));
        // A scan showing another tab leaves the (asynchronous) activation outstanding.
        let out = no_timers(s.handle(Millis(7), &scan_sel(&tabs, &["A", "B"], 1)));
        assert!(out.is_empty(), "{out:?}");
        assert_eq!(s.outstanding_ops().filter(|(_, o)| o.kind == OpKind::ActivateTab).count(), 1);
        // No confirmation by the deadline (from the activation, issued when
        // the window was built at 5): re-issued once.
        let id = s.timer_id();
        let due = Millis(5 + super::super::ACTIVATE_TAB_TIMEOUT_MS);
        let out = no_timers(s.handle(due, &Event::Timer(id)));
        assert!(matches!(out.as_slice(), [a, Command::RescanWindow(TRIGGER)] if is_activate(a, 0)), "{out:?}");
        assert_eq!(s.phase(), &Phase::Running);
        // The retry fails too: given up and logged, and the session completes
        // with the tab Explorer shows.
        let op = s.outstanding_ops().find(|(_, o)| o.kind == OpKind::ActivateTab).map(|(op, _)| op).unwrap();
        let error = ShellError { code: 0, message: "UIA select: the tab strip has 1 item(s) for 2 tab(s)".into() };
        let out = s.handle(Millis(due.0 + 1), &Event::OperationFailed { op, error });
        assert!(
            matches!(&out[0], Command::AppendLog(LogEntry::Error { operation, description })
                if operation == "restore-activate-tab" && description.contains("continues")),
            "{out:?}"
        );
        assert!(out.contains(&Command::Focus(TRIGGER)), "{out:?}");
        assert_eq!(s.phase(), &Phase::Completed);
        assert!(matches!(s.take_outcome(), Some(RestoreOutcome::Completed(_))));
    }

    #[test]
    fn a_late_scan_confirms_an_activation_after_a_stale_one() {
        let (mut s, k) = built(&["A", "B"]);
        let tabs = [(k[0], "A"), (k[1], "B")];
        s.handle(Millis(6), &scan_sel(&tabs, &["A", "B"], 1));
        assert!(no_timers(s.handle(Millis(7), &scan_sel(&tabs, &["A", "B"], 1))).is_empty());
        // The activation completes; its rescan shows the planned tab.
        let out = s.handle(Millis(60), &scan_sel(&tabs, &["A", "B"], 0));
        assert!(out.contains(&Command::Focus(TRIGGER)), "{out:?}");
        assert_eq!(s.phase(), &Phase::Completed);
        assert_eq!(count_activations(&s), 0);
    }

    fn count_activations(s: &RestoreSession) -> usize {
        s.outstanding_ops().filter(|(_, o)| o.kind == OpKind::ActivateTab).count()
    }

    #[test]
    fn tracking_scans_are_needed_only_when_the_session_waits_for_one() {
        let mut s = session(&["A", "B"]);
        let other = WinKey { hwnd: 77, seq: 1 };
        let ps = probes(&s.handle(Millis(1), &Event::WriteCompleted { seq: 9, result: Ok(()) }));
        for p in ps {
            s.handle(Millis(2), &Event::ProbeCompleted { probe: p, outcome: ProbeOutcome::Available });
        }
        // Building: tab keys known, nothing to verify or confirm.
        assert!(!s.needs_scan(TRIGGER));
        assert!(s.needs_scan(other), "not ours");
        s.handle(Millis(3), &Event::TabCreated { win: TRIGGER, tab: tab(1) });
        assert_eq!(count_activations(&s), 0);
        assert!(!s.needs_scan(TRIGGER));
        // Built: activated, and the verification scan is awaited.
        s.handle(Millis(4), &Event::NavigationCompleted { tab: tab(0), location: fs("A") });
        s.handle(Millis(4), &Event::NavigationCompleted { tab: tab(1), location: fs("B") });
        assert_eq!(count_activations(&s), 1);
        assert!(s.needs_scan(TRIGGER));
        // Verified, activation unconfirmed: the confirming scan is awaited.
        s.handle(Millis(5), &scan_sel(&[(tab(0), "A"), (tab(1), "B")], &["A", "B"], 1));
        assert_eq!(count_activations(&s), 1);
        assert!(s.needs_scan(TRIGGER));
        s.handle(Millis(6), &scan_sel(&[(tab(0), "A"), (tab(1), "B")], &["A", "B"], 0));
        assert!(s.is_finished() && s.needs_scan(TRIGGER), "finished: every scan counts again");
    }

    #[test]
    fn an_operation_past_its_deadline_fails_the_session() {
        let mut s = session(&["A"]);
        let ps = probes(&s.handle(Millis(1), &Event::WriteCompleted { seq: 9, result: Ok(()) }));
        let out = s.handle(Millis(2), &Event::ProbeCompleted { probe: ps[0], outcome: ProbeOutcome::Available });
        assert_eq!(navigations(&out), vec![(tab(0), "A".to_string())]);
        let id = s.timer_id();
        assert_eq!(id.owner, TimerOwner::Restore);
        assert!(out.contains(&Command::SetTimer { id, at: Millis(2 + super::super::NAVIGATION_TIMEOUT_MS) }));
        // Another session's timer is ignored; an early one re-arms.
        let other = TimerId { owner: TimerOwner::Restore, seq: id.seq + 1 };
        assert!(s.handle(Millis(3000), &Event::Timer(other)).is_empty());
        assert_eq!(s.handle(Millis(3000), &Event::Timer(id)), vec![Command::SetTimer { id, at: Millis(5002) }]);
        let out = s.handle(Millis(5002), &Event::Timer(id));
        assert!(matches!(
            s.phase(),
            Phase::Failed(Failure::Timeout { kind: OpKind::Navigate, after_ms: 5000, .. })
        ));
        assert_eq!(
            out,
            vec![Command::AppendLog(LogEntry::Error {
                operation: "restore-navigate".into(),
                description: "no completion within 5000 ms".into(),
            })]
        );
        assert_eq!(s.outstanding_ops().count(), 0);
        assert!(s.handle(Millis(5003), &Event::NavigationCompleted { tab: tab(0), location: fs("A") }).is_empty());
    }

    #[test]
    fn all_omitted_completes_without_window_commands() {
        let mut s = session(&["A", "B"]);
        let ps = probes(&s.handle(Millis(1), &Event::WriteCompleted { seq: 9, result: Ok(()) }));
        s.handle(Millis(2), &Event::ProbeCompleted { probe: ps[0], outcome: ProbeOutcome::AccessDenied });
        let out = s.handle(Millis(4), &Event::ProbeCompleted { probe: ps[1], outcome: ProbeOutcome::Timeout });
        let reasons: Vec<OmitReason> = out
            .iter()
            .filter_map(|c| match c {
                Command::AppendLog(LogEntry::TabOmitted { reason, .. }) => Some(*reason),
                _ => None,
            })
            .collect();
        assert_eq!(reasons, vec![OmitReason::Inaccessible, OmitReason::Timeout]);
        assert!(out.contains(&Command::AppendLog(LogEntry::Restore {
            targeted: false,
            windows: 0,
            tabs: 0,
            omitted: 2,
            redirected: 0,
            duration_ms: 4,
        })));
        assert_eq!(out.last(), Some(&Command::Notify { omitted_tabs: 2 }));
        assert!(!out.iter().any(|c| matches!(
            c,
            Command::Navigate { .. } | Command::SetZOrder(_) | Command::Focus(_) | Command::Reveal { .. }
        )));
        assert_eq!(s.phase(), &Phase::Completed);
    }

    #[test]
    fn a_tab_showing_the_wrong_location_is_renavigated() {
        let (mut s, k) = built(&["A", "B"]);
        // Tab 1's late first navigation (the start location) overrode ours.
        let out = s.handle(Millis(6), &scan(&[(k[0], "A"), (k[1], "Home")], &["A", "Home"]));
        assert_eq!(navigations(&out), vec![(k[1], "B".to_string())]);
        assert!(!s.is_settled());
    }

    #[test]
    fn new_tab_is_navigated_at_once_and_creation_is_sequential() {
        let mut s = session(&["A", "B", "C"]);
        let ps = probes(&s.handle(Millis(1), &Event::WriteCompleted { seq: 9, result: Ok(()) }));
        let mut out = Vec::new();
        for p in ps {
            out.extend(s.handle(Millis(2), &Event::ProbeCompleted { probe: p, outcome: ProbeOutcome::Available }));
        }
        let creates = |c: &[Command]| c.iter().filter(|c| matches!(c, Command::CreateTab { .. })).count();
        assert_eq!(creates(&out), 1, "one outstanding CreateTab per window");
        assert_eq!(out.iter().filter(|c| matches!(c, Command::ApplyPlacement { .. })).count(), 1);
        assert_eq!(navigations(&out), vec![(tab(0), "A".to_string())]);
        // The tab registers: navigate it and post the next CreateTab, without
        // waiting for the navigation.
        let out = s.handle(Millis(3), &Event::TabCreated { win: TRIGGER, tab: tab(1) });
        assert_eq!(navigations(&out), vec![(tab(1), "B".to_string())]);
        assert_eq!(creates(&out), 1);
        // The new tab's own first navigation does not complete ours.
        let out = s.handle(Millis(4), &Event::NavigationCompleted { tab: tab(1), location: fs("Home") });
        assert!(out.is_empty(), "{out:?}");
        let out = s.handle(Millis(5), &Event::TabCreated { win: TRIGGER, tab: tab(2) });
        assert_eq!(navigations(&out), vec![(tab(2), "C".to_string())]);
        assert_eq!(creates(&out), 0, "final slot count reached");
        // Tabs created by the user are not ours.
        assert!(s.handle(Millis(6), &Event::TabCreated { win: TRIGGER, tab: tab(9) }).is_empty());
        assert_eq!(s.outstanding_ops().filter(|(_, o)| o.kind == OpKind::Navigate).count(), 3);
    }

    #[test]
    fn only_tab_of_a_failed_placeholder_is_rebound_not_closed() {
        // [N, A]: A resolves first, so the trigger is decided with slots [P0, R1].
        let mut s = session(&["N", "A"]);
        let ps = probes(&s.handle(Millis(1), &Event::WriteCompleted { seq: 9, result: Ok(()) }));
        let out = s.handle(Millis(2), &Event::ProbeCompleted { probe: ps[1], outcome: ProbeOutcome::Available });
        assert!(navigations(&out).is_empty(), "tab 0 is a placeholder");
        assert!(out.iter().any(|c| matches!(c, Command::CreateTab { .. })));
        // N fails before the new tab registers: tab 0 takes slot A.
        let out = s.handle(Millis(3), &Event::ProbeCompleted { probe: ps[0], outcome: ProbeOutcome::Timeout });
        assert_eq!(navigations(&out), vec![(tab(0), "A".to_string())]);
        assert!(!out.iter().any(|c| matches!(c, Command::CloseTab { .. })));
        // The surplus new tab is closed once it registers.
        let out = s.handle(Millis(4), &Event::TabCreated { win: TRIGGER, tab: tab(1) });
        assert!(out.iter().any(|c| matches!(c, Command::CloseTab { tab: t, .. } if *t == tab(1))), "{out:?}");
        s.handle(Millis(5), &Event::NavigationCompleted { tab: tab(0), location: fs("A") });
        let out = no_timers(s.handle(Millis(6), &Event::TabRevoked { win: TRIGGER, tab: tab(1) }));
        // Built (verification scan) and down to its final tab (activation).
        assert_eq!(out.len(), 2, "{out:?}");
        assert!(out.contains(&Command::RescanWindow(TRIGGER)) && out.iter().any(|c| is_activate(c, 0)), "{out:?}");
        s.handle(Millis(7), &scan(&[(tab(0), "A")], &["A"]));
        assert!(s.is_settled());
    }

    #[test]
    fn targeted_selection_is_reapplied_after_a_renavigation() {
        let w = WindowRec { id: WindowId(1), tabs: vec![TabRec { location: fs("A") }], active: 0, placement: placement() };
        let items = vec![ItemId(vec![7, 0, 0])];
        let mut s = RestoreSession::new(
            Millis(0),
            RestoreParams {
                snapshot: Snapshot { windows: vec![w], activation: vec![WindowId(1)] },
                launch: LaunchInfo::Targeted { requested: fs("Q") },
                displays: displays(),
                trigger: TRIGGER,
                trigger_tab: Some(tab(0)),
                trigger_location: Some(fs("Q")),
                requested_selection: items.clone(),
                attempt_write_seq: 9,
                first_id: 1,
            },
        );
        let ps = probes(&s.handle(Millis(1), &Event::WriteCompleted { seq: 9, result: Ok(()) }));
        let out = s.handle(Millis(2), &Event::ProbeCompleted { probe: ps[0], outcome: ProbeOutcome::Available });
        // Slot 0 (the original tab, showing Q) goes to restored tab 0.
        assert_eq!(navigations(&out), vec![(tab(0), "A".to_string())]);
        let out = s.handle(Millis(3), &Event::TabCreated { win: TRIGGER, tab: tab(1) });
        assert_eq!(navigations(&out), vec![(tab(1), "Q".to_string())]);
        s.handle(Millis(4), &Event::NavigationCompleted { tab: tab(0), location: fs("A") });
        s.handle(Millis(4), &Event::NavigationCompleted { tab: tab(1), location: fs("Q") });
        assert_eq!(s.requested_tab(), Some(tab(1)));

        let finish = |out: &[Command]| -> (Vec<usize>, Vec<TabKey>) {
            let act = out.iter().filter_map(|c| if let Command::ActivateTab { index, .. } = c { Some(*index) } else { None });
            let sel = out.iter().filter_map(|c| match c {
                Command::SelectItems { tab, items: i, .. } if *i == items => Some(*tab),
                _ => None,
            });
            (act.collect(), sel.collect())
        };
        assert_eq!(finish(&[]), (vec![], vec![]));
        let tabs = [(tab(0), "A"), (tab(1), "Q")];
        let out = no_timers(s.handle(Millis(5), &scan_sel(&tabs, &["A", "Q"], 1)));
        assert_eq!(finish(&out), (vec![1], vec![tab(1)]));
        assert_eq!(out.last(), Some(&Command::RescanWindow(TRIGGER)), "the activation barrier follows the selection");
        assert!(s.is_settled());

        // A late navigation moves the requested tab away: it is re-navigated,
        // which clears its selection, so both are issued again once settled.
        let out = s.handle(Millis(7), &Event::NavigationCompleted { tab: tab(1), location: fs("Home") });
        assert_eq!(navigations(&out), vec![(tab(1), "Q".to_string())]);
        assert!(!s.is_settled());
        assert_eq!(s.outstanding_ops().filter(|(_, o)| o.kind == OpKind::ActivateTab).count(), 0, "forgotten");
        let out = s.handle(Millis(8), &Event::NavigationCompleted { tab: tab(1), location: fs("Q") });
        assert_eq!(out, vec![Command::RescanWindow(TRIGGER)]);
        let out = s.handle(Millis(9), &scan_sel(&tabs, &["A", "Q"], 1));
        assert_eq!(finish(&out), (vec![1], vec![tab(1)]));
        assert!(s.is_settled());
        assert_eq!(s.active_tabs(), vec![(TRIGGER, 1)]);
        // The barrier scan confirms the activation: the session completes.
        let out = s.handle(Millis(10), &scan_sel(&tabs, &["A", "Q"], 1));
        assert!(out.contains(&Command::SetZOrder(vec![TRIGGER])) && out.contains(&Command::Focus(TRIGGER)), "{out:?}");
        let Some(RestoreOutcome::Completed(c)) = s.take_outcome() else { panic!() };
        assert!(c.targeted);
        assert_eq!((c.windows, c.tabs), (1, 1), "the requested tab is not a restored tab");
        // SelectItems is never outstanding; the activation was confirmed.
        assert_eq!(s.outstanding_ops().count(), 0);
    }

    #[test]
    fn failures_stop_the_session() {
        let mut s = session(&["A"]);
        s.handle(Millis(1), &Event::WriteCompleted { seq: 9, result: Err(IoErrorKind::DiskFull) });
        assert_eq!(s.phase(), &Phase::Failed(Failure::AttemptWriteFailed(IoErrorKind::DiskFull)));
        assert!(s.handle(Millis(2), &Event::WriteCompleted { seq: 9, result: Ok(()) }).is_empty());

        let mut s = session(&["A"]);
        let ps = probes(&s.handle(Millis(1), &Event::WriteCompleted { seq: 9, result: Ok(()) }));
        let out = s.handle(Millis(2), &Event::ProbeCompleted { probe: ps[0], outcome: ProbeOutcome::Available });
        let Some(Command::Navigate { op, .. }) = out.iter().find(|c| matches!(c, Command::Navigate { .. })) else {
            panic!("{out:?}")
        };
        // Someone else's failure is ignored; ours stops the session.
        let err = ShellError { code: 1, message: "x".into() };
        assert!(s.handle(Millis(3), &Event::OperationFailed { op: OpId(999), error: err.clone() }).is_empty());
        assert_eq!(s.phase(), &Phase::Running);
        s.handle(Millis(3), &Event::OperationFailed { op: *op, error: err.clone() });
        assert_eq!(s.phase(), &Phase::Failed(Failure::OperationFailed { op: *op, kind: OpKind::Navigate, error: err }));
        assert!(s.handle(Millis(4), &Event::NavigationCompleted { tab: tab(0), location: fs("A") }).is_empty());
    }
}
