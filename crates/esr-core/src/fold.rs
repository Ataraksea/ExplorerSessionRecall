//! Tab folding (Req 14): the `FoldingEngine` state machine, its state types,
//! the pure Fold_Target selection [`select`], and the limits of the folding
//! timers.
//!
//! The `FoldingEngine` owns each Folding_Candidate from its first signal
//! (`Event::WindowAppeared`) until the candidate is folded, switched, or left
//! open. While it owns a window, the tracker, the store, trigger detection and
//! the restore session never see it (Req 2.6, 14.27, 14.28).
//!
//! Task 20.1 defines the types and constants with stub bodies; later tasks fill
//! them in: 20.4 `OpeningOrder`, 20.6 [`select`], 20.11 classification, hiding
//! and drag-out detection, 20.12 the fold context step, 20.13 folding into a
//! Folded_Tab, 20.14 Tab_Switch and its failure chain, 20.15 leaving a
//! candidate open. The log entry carrying a [`FoldOutcome`] is added by 20.2.
//!
//! Requirements 2.6, 12.10, 14.1–14.30 (design.md, "Tab folding", "Folding
//! state").

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::engine::{
    Command, DesktopView, Event, FoldProbeId, ItemId, Millis, OpId, TabKey, TimerId, TimerOwner, WinKey,
    WindowObservation,
};
use crate::logfmt::LogEntry;
use crate::model::{LocationId, LocationKey};
use crate::plan::placement::FinalPlacement;

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

/// A Folding_Candidate whose first navigation has not completed after this
/// long is left open (Req 14.14).
pub const FOLD_FIRST_NAV_MS: u64 = 8_000;

/// A Fold_Target that creates no tab within this long (counted from the
/// command, or from the last fallback) is passed over (Req 14.13, 14.24).
pub const FOLD_TAB_CREATE_MS: u64 = 1_000;

/// A Folded_Tab that does not show the candidate's Location within this long of
/// being created leaves the candidate open (Req 14.14).
pub const FOLD_TAB_SHOW_MS: u64 = 3_000;

/// A Tab_Switch whose activation is not confirmed within this long fails
/// (Req 14.23).
pub const FOLD_SWITCH_MS: u64 = 1_000;

/// A window that does not answer `WM_NULL` within this long is an
/// Unresponsive_Window (Req 14.13).
pub const UNRESPONSIVE_MS: u64 = 1_000;

/// A `TabDetached` for a tracked window marks a candidate as a drag-out when it
/// arrives between 100 ms before and 300 ms after the candidate's first signal,
/// as `(before, after)` offsets in milliseconds from the first signal
/// (Req 14.3 b).
pub const DRAG_OUT_WINDOW_MS: (i64, i64) = (-100, 300);

/// [`DRAG_OUT_WINDOW_MS`] as unsigned spans: how long before the first signal
/// a `TabDetached` still counts, and how long after it.
const DRAG_OUT_BEFORE_MS: u64 = DRAG_OUT_WINDOW_MS.0.unsigned_abs();
const DRAG_OUT_AFTER_MS: u64 = DRAG_OUT_WINDOW_MS.1.unsigned_abs();

/// A `TabStarted` for the op of a stopped fold is still closed if it arrives up
/// to this long after the fold stopped (the "late tab" of Req 14.13).
pub const LATE_TAB_MS: u64 = 10_000;

/// A Folded_Tab's `ActivateTab` that is not confirmed within this long is
/// issued again (like the restore session's activation, Req 14.17, 14.19).
pub const FOLD_ACTIVATE_MS: u64 = 1_000;

/// How often an unconfirmed `ActivateTab` of a Folded_Tab is issued again
/// before the fold goes on without the confirmation (the restore session's
/// rule: retried once, then given up).
pub const FOLD_ACTIVATE_RETRIES: u32 = 1;

/// A candidate that has not closed this long after `CloseWindow` gets the
/// command once more; if it has not closed this long after the second post
/// either, it is revealed and handed to the tracker with
/// `ERROR operation=fold-close` (Req 14.1, design.md "Error Handling").
pub const FOLD_CLOSE_RETRY_MS: u64 = 2_000;

/// How often `CloseWindow` is posted for one candidate: the first time and
/// once more after [`FOLD_CLOSE_RETRY_MS`].
pub const FOLD_CLOSE_POSTS: u32 = 2;

/// First [`OpId`] of the `FoldingEngine`'s own commands. The restore session
/// counts up from 1 and the Engine's deferred-tab navigations from
/// `1 << 62`; this base lies above both.
pub const FOLD_OP_BASE: u64 = (1 << 62) + (1 << 61);

/// The placement a Fold_Target is shown in when it is minimized: `Some` for a
/// minimized window (its pre-minimize state, normal at its restored bounds or
/// maximized), `None` for any other window (Req 14.7, 14.21). The Engine
/// answers it from the tracker's recorded placement and the displays.
pub type PlacementOf<'a> = &'a dyn Fn(WinKey) -> Option<FinalPlacement>;

/// The tab the tracker has at `index` of a window, left to right: the key of a
/// Matching_Tab, which `SelectItems` of a Tab_Switch needs (Req 14.6, 14.21).
/// `None` for an index the tracker does not know (the Matching_Tab is then
/// switched to without a selection).
pub type TabOf<'a> = &'a dyn Fn(WinKey, usize) -> Option<TabKey>;

// ---------------------------------------------------------------------------
// Opening_Order (Req 14.8, 14.9, 14.11, 14.12)
// ---------------------------------------------------------------------------

/// One position of the [`OpeningOrder`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Slot {
    win: WinKey,
    /// A Folding_Candidate's reserved first-signal position: not part of the
    /// order until the candidate is left open (Req 14.15).
    reserved: bool,
}

/// The ranking of all open Explorer_Windows, on every Virtual_Desktop, by the
/// moment each opened, earliest first (Req 14.8). Never persisted.
///
/// - A window is appended at its `WindowAppeared`, excluded windows too
///   ([`append`](Self::append)).
/// - A Folding_Candidate holds a hidden position from its first signal
///   ([`reserve`](Self::reserve)). The position is not visible in the order
///   (a candidate is never in the Opening_Order while the `FoldingEngine` owns
///   it, Req 2.6) and becomes a real one only if the candidate is left open
///   ([`reveal`](Self::reveal)); a folded or closed candidate gives it up
///   ([`remove`](Self::remove)).
/// - Windows already open when tracking starts or resumes are ranked in their
///   Activation_Order at that moment, most recent first
///   ([`reset_to_activation`](Self::reset_to_activation)).
/// - At Restoration_Completion the Trigger_Window and the restored windows (in
///   saved Activation_Order) move in front of every window opened during the
///   restoration ([`restoration_completed`](Self::restoration_completed)).
/// - The Primary_Window of a Virtual_Desktop is derived from the order and a
///   [`DesktopView`] when needed ([`primary`](Self::primary)), so a closed or
///   moved Primary_Window is replaced by the next window automatically.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpeningOrder {
    slots: Vec<Slot>,
}

impl OpeningOrder {
    /// An empty order.
    pub fn new() -> OpeningOrder {
        OpeningOrder::default()
    }

    /// The windows of the order, earliest first. Reserved candidate positions
    /// are not included.
    pub fn windows(&self) -> impl Iterator<Item = WinKey> + '_ {
        self.slots.iter().filter(|s| !s.reserved).map(|s| s.win)
    }

    /// The windows of the order, earliest first, as a vector (the shape of
    /// `FoldCtx::opening`).
    pub fn to_vec(&self) -> Vec<WinKey> {
        self.windows().collect()
    }

    /// Whether `win` is in the order (a reserved candidate position does not
    /// count).
    pub fn contains(&self, win: WinKey) -> bool {
        self.slots.iter().any(|s| !s.reserved && s.win == win)
    }

    /// The number of windows in the order.
    pub fn len(&self) -> usize {
        self.windows().count()
    }

    /// Whether the order has no windows.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Appends `win` at the end, at its `WindowAppeared` (Req 14.8). A window
    /// that already has a position, real or reserved, keeps it.
    pub fn append(&mut self, win: WinKey) {
        if !self.slots.iter().any(|s| s.win == win) {
            self.slots.push(Slot { win, reserved: false });
        }
    }

    /// Reserves the first-signal position of a Folding_Candidate at the end,
    /// without making it part of the order (Req 14.15).
    pub fn reserve(&mut self, win: WinKey) {
        if !self.slots.iter().any(|s| s.win == win) {
            self.slots.push(Slot { win, reserved: true });
        }
    }

    /// Makes a left-open candidate part of the order at its first-signal
    /// position (Req 14.15). A window without a reserved position is appended.
    pub fn reveal(&mut self, win: WinKey) {
        match self.slots.iter_mut().find(|s| s.win == win) {
            Some(slot) => slot.reserved = false,
            None => self.slots.push(Slot { win, reserved: false }),
        }
    }

    /// Removes a destroyed window, or gives up a candidate's reserved position
    /// (Req 14.9). An unknown window is ignored.
    pub fn remove(&mut self, win: WinKey) {
        self.slots.retain(|s| s.win != win);
    }

    /// Ranks the windows already open when tracking starts or resumes in their
    /// Activation_Order at that moment, most recent first (Req 14.8). Every
    /// earlier position, reserved ones included, is dropped; `activation` lists
    /// tracked windows only, never candidates.
    pub fn reset_to_activation(&mut self, activation: &[WinKey]) {
        self.slots.clear();
        for &win in activation {
            self.append(win);
        }
    }

    /// Applies Restoration_Completion (Req 14.8): the Trigger_Window and then
    /// the restored windows in saved Activation_Order (`restored`, most recent
    /// first) rank in front of every window opened during the restoration.
    ///
    /// Windows that are not in the order are skipped. Windows that ranked before
    /// all of these windows (opened before the restoration began) keep their
    /// place; the moved windows take the place of the earliest of them, and the
    /// remaining windows follow in their old order.
    pub fn restoration_completed(&mut self, trigger: Option<WinKey>, restored: &[WinKey]) {
        let mut moved: Vec<WinKey> = Vec::new();
        for win in trigger.into_iter().chain(restored.iter().copied()) {
            if self.contains(win) && !moved.contains(&win) {
                moved.push(win);
            }
        }
        let Some(anchor) = self.slots.iter().position(|s| !s.reserved && moved.contains(&s.win)) else {
            return;
        };
        let mut reordered: Vec<Slot> = self.slots[..anchor].to_vec();
        reordered.extend(moved.iter().map(|&win| Slot { win, reserved: false }));
        reordered.extend(
            self.slots[anchor..]
                .iter()
                .filter(|s| s.reserved || !moved.contains(&s.win))
                .copied(),
        );
        self.slots = reordered;
    }

    /// The windows of the order that are on the Virtual_Desktop `view`
    /// describes, earliest first. A window shown on all desktops is in a
    /// `Known` view of every desktop (Req 14.11); with `Unknown` every window
    /// counts as on the desktop (Req 14.12).
    pub fn on_desktop<'a>(&'a self, view: &'a DesktopView) -> impl Iterator<Item = WinKey> + 'a {
        self.windows().filter(move |win| match view {
            DesktopView::Known(on_desktop) => on_desktop.contains(win),
            DesktopView::Unknown => true,
        })
    }

    /// The Primary_Window of the desktop `view` describes: the first window of
    /// the order that is on it (Req 14.9). Candidates are not in the order.
    pub fn primary(&self, view: &DesktopView) -> Option<WinKey> {
        self.on_desktop(view).next()
    }
}

// ---------------------------------------------------------------------------
// Engine state
// ---------------------------------------------------------------------------

/// Where a Folding_Candidate is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Phase {
    /// Hidden at its first signal, waiting for its Location (Req 14.14, 14.16).
    Hidden,
    /// `ProbeFoldContext` issued, waiting for `FoldContext` (Req 14.9).
    Context,
    /// Tab_Switch under way: waiting for `ActivationConfirmed` (Req 14.21, 14.23).
    Switching,
    /// A Folded_Tab is being created and navigated in the Fold_Target
    /// (Req 14.1, 14.13).
    Creating,
    /// The Folded_Tab shows the Location; selection, activation and focus are
    /// being applied (Req 14.6, 14.7).
    Finishing,
    /// `CloseWindow` issued for the candidate; waiting for its `WindowDestroyed`
    /// (Req 14.1).
    Closing,
}

/// Why a window was passed over as Fold_Target (Req 14.24, 14.30).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PassReason {
    /// The window was closed.
    Closed,
    /// Every tab of the window is closing.
    Closing,
    /// The window did not answer `WM_NULL` in time.
    Unresponsive,
    /// The window created no tab within [`FOLD_TAB_CREATE_MS`].
    TabNotCreated,
}

/// Why a Tab_Switch failed (Req 14.23, 14.30).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SwitchFailure {
    /// No `ActivationConfirmed` within [`FOLD_SWITCH_MS`].
    NotActivated,
    /// The Matching_Tab closed first (`TabRevoked`), or the Folded_Tab of an
    /// earlier candidate that a waiting switch was to land on never came to
    /// exist.
    MatchingTabClosed,
    /// The Explorer_Window that held the Matching_Tab closed first
    /// (`WindowDestroyed`).
    FoldTargetClosed,
}

/// A Folding_Candidate in progress (Req 14).
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)] // filled in by tasks 20.11–20.15
pub struct Candidate {
    /// Creation sequence: the order of first signals (Req 14.19).
    seq: u64,
    /// When the first signal arrived.
    appeared_at: Millis,
    /// The windows on the candidate's Virtual_Desktop, as of its first signal.
    desktop: DesktopView,
    /// The candidate's Foreground_Explorer_Window (Req 14.22), from
    /// `WindowAppeared`.
    foreground: Option<WinKey>,
    /// The candidate's Location, once `InitialLocationKnown` arrived.
    location: Option<LocationId>,
    /// The items selected in the candidate (Req 14.6).
    selection: Vec<ItemId>,
    phase: Phase,
    /// The current Fold_Target (or the window holding the Matching_Tab).
    target: Option<WinKey>,
    /// The outstanding `ProbeFoldContext` while in [`Phase::Context`].
    probe: Option<FoldProbeId>,
    /// What [`select`] chose at the last `FoldContext` (a `Switch` or a `Fold`,
    /// never `NoTarget`). A `Fold` plan makes the candidate's Folded_Tab count
    /// as an open tab at the right end of its target for later candidates
    /// (Req 14.19). Tasks 20.13 and 20.14 carry the plan out and replace it
    /// when a Tab_Switch fails.
    plan: Option<FoldPlan>,
    /// The window `select` must not take as Fold_Target: the one that held the
    /// Matching_Tab of a failed Tab_Switch or fold (Req 14.24). Set by 20.14.
    exclude: Option<WinKey>,
    /// Tabs created for this candidate (Req 14.14: closed when it stays open).
    created_tabs: Vec<TabKey>,
    /// Operations issued for this candidate (`CreateTab`, `Navigate`,
    /// `SelectItems`, `ActivateTab`), so a late `ActivationConfirmed` of an
    /// earlier attempt is still recognised.
    pending_ops: Vec<OpId>,
    /// Index its Folded_Tab has in the Fold_Target: the target's tab count at
    /// `FoldContext` plus the Folded_Tabs that earlier candidates were still
    /// creating in it (the same count `select` uses for a pending tab,
    /// Req 14.19). This is the "rightmost" index of `ActivateTab`.
    slot: usize,
    /// For a `Switch` plan onto an earlier candidate's pending Folded_Tab: that
    /// candidate. The switch can only start once its tab is shown and active
    /// (Req 14.19).
    waits_on: Option<WinKey>,
    /// The outstanding or last `CreateTab` for the Folded_Tab.
    create_op: Option<OpId>,
    /// The Folded_Tab, from `TabStarted` (or from `TabCreated` when the
    /// adapter reports no `TabStarted`).
    tab: Option<TabKey>,
    /// `TabCreated`: the Folded_Tab is registered and `Navigate` was issued.
    registered: bool,
    /// `NavigationCompleted` with the candidate's `fold_key` was seen.
    shown: bool,
    /// Number of `ActivateTab` commands issued for the Folded_Tab.
    activate_tries: u32,
    /// The Folded_Tab's activation was confirmed (or given up).
    activated: bool,
    /// The fold followed the foreground exception (Req 14.22): the log's
    /// `folded-foreground`.
    foreground_exception: bool,
    /// Which window the fold goes into, for the `FOLD` entry (Req 14.30).
    fold_into: FoldInto,
    /// Windows passed over as Fold_Target, in order, with the reason (Req 14.30).
    passed_over: Vec<(WinKey, PassReason)>,
    /// Set when a Tab_Switch failed and the fold continues from it (Req 14.23).
    switch_failure: Option<SwitchFailure>,
    /// After a failed Tab_Switch: the window that held the Matching_Tab, which
    /// the fresh `FoldContext` tries first as Fold_Target (Req 14.23) before
    /// the chain of Req 14.24 takes over.
    retry_holder: Option<WinKey>,
    /// The current Fold_Target is a fallback window selected under Req 14.13
    /// (including as applied by 14.24): if it creates no tab within 1 s the
    /// candidate is left open (Req 14.14).
    fallback_used: bool,
    /// The Matching_Tab of a `Switch` plan, once known: from the tracker at
    /// `FoldContext`, or the Folded_Tab of the earlier candidate it waited on.
    switch_tab: Option<TabKey>,
    /// `ActivateTab` of the Tab_Switch was issued.
    switch_started: bool,
    /// When the Tab_Switch fails if `ActivationConfirmed` has not arrived.
    switch_deadline: Option<Millis>,
    /// When the Fold_Target is passed over if no tab has appeared.
    create_deadline: Option<Millis>,
    /// When the candidate is left open if the Folded_Tab does not show the
    /// Location.
    show_deadline: Option<Millis>,
    /// For a `Switch` plan: the Matching_Tab is in the Primary_Window.
    matching_primary: bool,
    /// The last scan and similar events, replayed to the tracker if the
    /// candidate is left open (Req 14.15).
    buffered: Vec<Event>,
    /// The windows activated since the candidate's own latest activation
    /// (`buffered` holds one), oldest first: they rank ahead of the candidate
    /// in the Activation_Order when it is handed over (Req 2.1, 14.15).
    activated_after: Vec<WinKey>,
    /// Whether the candidate's first scan has been seen (Req 14.2: a first
    /// scan with more than one tab excludes the window).
    first_scan_seen: bool,
    /// Number of `CloseWindow` commands posted for the candidate (1 when it
    /// enters [`Phase::Closing`], [`FOLD_CLOSE_POSTS`] at most).
    close_posts: u32,
    /// When the next step of the `CloseWindow` retry is due.
    close_deadline: Option<Millis>,
}

impl Candidate {
    /// Forgets everything about the attempt (Tab_Switch or Folded_Tab) that
    /// just failed, so that late events of its ops are ignored and the next
    /// attempt starts clean (Req 14.13, 14.23, 14.24). The tabs created for
    /// the candidate (`created_tabs`), the passed-over windows and the failure
    /// chain's state are kept.
    fn reset_attempt(&mut self) {
        self.target = None;
        self.probe = None;
        self.plan = None;
        self.pending_ops.clear();
        self.slot = 0;
        self.waits_on = None;
        self.create_op = None;
        self.tab = None;
        self.registered = false;
        self.shown = false;
        self.activate_tries = 0;
        self.activated = false;
        self.switch_tab = None;
        self.switch_started = false;
        self.switch_deadline = None;
        self.create_deadline = None;
        self.show_deadline = None;
    }
}

/// The pure folding state machine (Req 14).
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)] // filled in by tasks 20.4, 20.11–20.15
pub struct FoldingEngine {
    /// `Settings::tab_folding_enabled` (Req 12.10).
    enabled: bool,
    /// Opening_Order, earliest first (Req 14.8).
    opening: OpeningOrder,
    /// Candidates in progress, keyed and ordered by first signal.
    candidates: BTreeMap<WinKey, Candidate>,
    /// Windows classified once and never candidates (Req 14.3).
    excluded: BTreeSet<WinKey>,
    /// Per Fold_Target, the candidates folding into it in creation order; one
    /// `CreateTab` is outstanding per target (Req 14.19).
    target_queues: BTreeMap<WinKey, VecDeque<WinKey>>,
    /// Ops of stopped folds whose tab is closed if it still appears, with the
    /// time they were stopped (Req 14.13, [`LATE_TAB_MS`]).
    late_ops: BTreeMap<OpId, (WinKey, Millis)>,
    /// Tabs of stopped folds that were closed when they appeared late; a
    /// `TabCreated` for one of them is not the Folded_Tab of any candidate.
    late_tabs: BTreeSet<TabKey>,
    /// Creation sequence of the next candidate (Req 14.19).
    next_seq: u64,
    /// Id of the next `ProbeFoldContext`.
    next_probe: u64,
    /// `TabDetached` events not yet matched with a first signal, with their
    /// arrival times; only those up to `-DRAG_OUT_WINDOW_MS.0` ms old are kept
    /// (Req 14.3 b: a drag-out's `TabDetached` can precede its first signal).
    recent_detached: Vec<(WinKey, Millis)>,
    /// Id of the next command the `FoldingEngine` issues with an [`OpId`]
    /// (counts up from [`FOLD_OP_BASE`]).
    next_op: u64,
}

// ---------------------------------------------------------------------------
// First signal: classification, hiding, drag-out (Req 14.2-14.4, 14.10, 14.16)
// ---------------------------------------------------------------------------

/// Kind of a `Fold`-owned timer; the `seq` of its [`TimerId`] is
/// `(kind << 56) | candidate seq` (see [`fold_timer`]). Later tasks add kinds.
const TIMER_FIRST_NAV: u64 = 1;
/// A Fold_Target must create the Folded_Tab within [`FOLD_TAB_CREATE_MS`] of
/// the `CreateTab` command (Req 14.13, 14.24); set with the command.
const TIMER_TAB_CREATE: u64 = 2;
/// The Folded_Tab must show the Location within [`FOLD_TAB_SHOW_MS`] of its
/// creation (`TabStarted`, Req 14.14).
const TIMER_TAB_SHOW: u64 = 3;
/// A Folded_Tab's `ActivateTab` is issued again after [`FOLD_ACTIVATE_MS`].
const TIMER_ACTIVATE: u64 = 4;
/// A Tab_Switch fails if `ActivationConfirmed` has not arrived within
/// [`FOLD_SWITCH_MS`] of its `ActivateTab` (Req 14.23).
const TIMER_SWITCH: u64 = 5;
/// A candidate that ignores `CloseWindow` gets it once more after
/// [`FOLD_CLOSE_RETRY_MS`], then is revealed and handed over after another
/// [`FOLD_CLOSE_RETRY_MS`] (Req 14.1).
const TIMER_CLOSE: u64 = 6;

/// Splits a `Fold` timer into its kind and candidate sequence number (the
/// inverse of [`fold_timer`]).
fn timer_parts(id: TimerId) -> (u64, u64) {
    (id.seq >> 56, id.seq & ((1 << 56) - 1))
}

/// The `Fold` timer of `kind` for the candidate with creation sequence `seq`.
fn fold_timer(kind: u64, seq: u64) -> TimerId {
    TimerId { owner: TimerOwner::Fold, seq: (kind << 56) | (seq & ((1 << 56) - 1)) }
}

/// Takes the next [`OpId`] of the `FoldingEngine`.
fn alloc_op(counter: &mut u64) -> OpId {
    let op = OpId(*counter);
    *counter += 1;
    op
}

/// `ERROR operation=fold-shell`: a candidate was dropped because its process
/// exited or the shell was lost (Req 14.14).
fn fold_shell_error(description: &str) -> Command {
    Command::AppendLog(LogEntry::Error { operation: "fold-shell".into(), description: description.into() })
}

/// Moves a candidate to [`Phase::Closing`] at the moment its `CloseWindow` is
/// issued, and returns the timer of the retry (Req 14.1).
fn start_closing(candidate: &mut Candidate, now: Millis) -> Command {
    candidate.phase = Phase::Closing;
    candidate.close_posts = 1;
    candidate.close_deadline = Some(now + FOLD_CLOSE_RETRY_MS);
    Command::SetTimer { id: fold_timer(TIMER_CLOSE, candidate.seq), at: now + FOLD_CLOSE_RETRY_MS }
}

/// The Location a `FOLD` entry names for a candidate (Req 14.30): the one
/// `InitialLocationKnown` announced, or else the one the candidate's last scan
/// shows. `None` while the first navigation has not completed.
fn log_location(c: &Candidate) -> Option<LocationId> {
    c.location.clone().or_else(|| {
        c.buffered.iter().find_map(|e| match e {
            Event::WindowScanned(obs) => obs.tabs.iter().find_map(|t| match &t.location {
                crate::engine::ObservedLocation::Known(l) => Some(l.clone()),
                _ => None,
            }),
            _ => None,
        })
    })
}

/// The `FOLD` outcome of a candidate whose fold completed (Req 14.30), from
/// the plan it carried out: `folded-primary`, `folded-fallback` (the
/// Primary_Window unresponsive or closing), `folded-foreground`, or
/// `switch-failed-folded` after a failed Tab_Switch. Switched candidates and
/// the fallback chain of Req 14.24 fill in `passed_over`, `switch_failure` and
/// `fold_into` (task 20.14).
fn fold_outcome(c: &Candidate) -> FoldOutcome {
    let passed: Vec<PassReason> = c.passed_over.iter().map(|&(_, reason)| reason).collect();
    if let Some(switch_reason) = c.switch_failure {
        FoldOutcome::SwitchFailedFolded { switch_reason, into: c.fold_into, passed }
    } else if let Some(FoldPlan::Switch { .. }) = c.plan {
        let matching = if c.matching_primary { MatchingWindow::Primary } else { MatchingWindow::Other };
        FoldOutcome::Switched { matching }
    } else if c.foreground_exception {
        FoldOutcome::FoldedForeground { into: c.fold_into, passed }
    } else if c.fold_into == FoldInto::Fallback {
        let reason = match c.plan {
            Some(FoldPlan::Fold { reason: Some(FoldReason::PrimaryUnresponsive), .. }) => {
                FallbackReason::PrimaryUnresponsive
            }
            Some(FoldPlan::Fold { reason: Some(FoldReason::PrimaryClosing), .. }) => FallbackReason::PrimaryClosing,
            _ => FallbackReason::TabNotCreated,
        };
        FoldOutcome::FoldedFallback { reason }
    } else {
        FoldOutcome::FoldedPrimary
    }
}

/// What the Engine knows about the rest of the system at a first signal and
/// the `FoldingEngine` cannot know itself (design.md, "First signal and
/// classification").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FirstSignalEnv<'a> {
    /// The windows the tracker tracks (candidates are never among them).
    pub tracked: &'a [WinKey],
    /// A restoration is in progress, a Trigger_Window candidate is waiting for
    /// its restoration to start, or the restore session has unclaimed
    /// `OpenWindow` requests (Req 14.3 c, d).
    pub restoration_active: bool,
    /// The Builtin_Restore_Option logon heuristic holds (Req 14.3 e).
    pub builtin_restore: bool,
}

/// Why a window was excluded from Tab_Folding for good (Req 14.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExclusionReason {
    /// The tab-folding option is off (Req 12.10).
    FoldingDisabled,
    /// No tracked window is open on the window's desktop (Req 14.3 a, h; 14.10).
    NoWindowOnDesktop,
    /// A restoration is in progress or about to start (Req 14.3 c, d).
    RestorationActive,
    /// Windows opened it through the Builtin_Restore_Option (Req 14.3 e).
    BuiltinRestore,
    /// The Fold_Bypass_Keys were held down (Req 14.3 g, 14.4).
    BypassKeys,
    /// A tab was dragged out of a tracked window (Req 14.3 b).
    DragOut,
    /// The first scan showed more than one tab (Req 14.2).
    MultipleTabs,
}

/// The classification of a window at its first signal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FirstSignalVerdict {
    /// A Folding_Candidate: hidden, owned by the `FoldingEngine`.
    Candidate,
    /// Excluded for good; the tracker and trigger detection handle it as usual.
    Excluded(ExclusionReason),
    /// The window was already known; nothing happened.
    AlreadyKnown,
}

/// The result of [`FoldingEngine::window_appeared`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FirstSignalOutcome {
    pub verdict: FirstSignalVerdict,
    /// `HideWindow` and the first-navigation timer for a candidate; empty
    /// otherwise.
    pub commands: Vec<Command>,
}

/// A window the `FoldingEngine` gives up: excluded after a drag-out or a
/// multi-tab first scan. The Engine hands it to the tracker and trigger
/// detection, replaying `buffered` (the candidate's last scan) first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Released {
    pub win: WinKey,
    pub buffered: Vec<Event>,
}

/// Commands to carry out and windows released to the tracker.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FoldStep {
    pub commands: Vec<Command>,
    pub released: Vec<Released>,
}

impl FoldingEngine {
    /// An engine with no windows and no candidates. `enabled` is
    /// `Settings::tab_folding_enabled` (Req 12.10).
    pub fn new(enabled: bool) -> FoldingEngine {
        FoldingEngine {
            enabled,
            opening: OpeningOrder::new(),
            candidates: BTreeMap::new(),
            excluded: BTreeSet::new(),
            target_queues: BTreeMap::new(),
            late_ops: BTreeMap::new(),
            late_tabs: BTreeSet::new(),
            next_seq: 1,
            next_probe: 1,
            recent_detached: Vec::new(),
            next_op: FOLD_OP_BASE,
        }
    }

    /// Whether Tab_Folding is turned on (Req 12.10).
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// The Opening_Order (Req 14.8).
    pub fn opening(&self) -> &OpeningOrder {
        &self.opening
    }

    /// The Opening_Order, for the engine's event handling (Req 14.8).
    pub fn opening_mut(&mut self) -> &mut OpeningOrder {
        &mut self.opening
    }

    /// The number of Folding_Candidates in progress. The Engine tells the
    /// tracker, so that closing the last tracked window while it is above zero
    /// is not a Session_End (Req 14.15, 14.28).
    pub fn candidates_in_progress(&self) -> usize {
        self.candidates.len()
    }

    /// Whether the `FoldingEngine` owns `win` as a Folding_Candidate. The
    /// Engine delivers a candidate's events to the `FoldingEngine` only
    /// (Req 2.6, 14.27).
    pub fn owns(&self, win: WinKey) -> bool {
        self.candidates.contains_key(&win)
    }

    /// Whether `win` was classified as excluded from Tab_Folding (Req 14.3).
    pub fn is_excluded(&self, win: WinKey) -> bool {
        self.excluded.contains(&win)
    }

    /// Classifies a new window once, at its first signal
    /// (`Event::WindowAppeared`; Req 14.2-14.4, 14.10, 12.10), in the design
    /// order:
    ///
    /// 1. folding is off; 2. no tracked window is open on the window's
    ///    desktop; 3. a restoration is in progress, a trigger is waiting, or
    ///    `OpenWindow` requests are unclaimed; 4. the Builtin_Restore_Option
    ///    heuristic holds; 5. `bypass_keys`; then a drag-out whose
    ///    `TabDetached` arrived up to 100 ms before the first signal.
    ///
    /// Every window is appended to the Opening_Order. An excluded window is
    /// recorded in `excluded` and left to the tracker; a candidate gets a
    /// reserved Opening_Order position, `HideWindow` and the 8 s
    /// first-navigation timer in the same call (Req 14.14, 14.16). `foreground`
    /// is kept only if it is a tracked, non-candidate window (Req 14.22).
    /// A window the `FoldingEngine` already knows is ignored.
    ///
    /// The Engine passes `tracked`, `restoration_active` and `builtin_restore`
    /// in [`FirstSignalEnv`] (task 22.x wiring).
    pub fn window_appeared(
        &mut self,
        now: Millis,
        win: WinKey,
        bypass_keys: bool,
        desktop: DesktopView,
        foreground: Option<WinKey>,
        env: &FirstSignalEnv<'_>,
    ) -> FirstSignalOutcome {
        let verdict_only = |verdict| FirstSignalOutcome { verdict, commands: Vec::new() };
        if self.candidates.contains_key(&win) || self.excluded.contains(&win) || self.opening.contains(win) {
            return verdict_only(FirstSignalVerdict::AlreadyKnown);
        }
        self.recent_detached.retain(|&(_, at)| now.since(at) <= DRAG_OUT_BEFORE_MS);

        let tracked_on_desktop = env.tracked.iter().any(|t| {
            !self.candidates.contains_key(t)
                && match &desktop {
                    DesktopView::Known(on_desktop) => on_desktop.contains(t),
                    DesktopView::Unknown => true,
                }
        });
        let exclusion = if !self.enabled {
            Some(ExclusionReason::FoldingDisabled)
        } else if !tracked_on_desktop {
            Some(ExclusionReason::NoWindowOnDesktop)
        } else if env.restoration_active {
            Some(ExclusionReason::RestorationActive)
        } else if env.builtin_restore {
            Some(ExclusionReason::BuiltinRestore)
        } else if bypass_keys {
            Some(ExclusionReason::BypassKeys)
        } else if let Some(n) = self.recent_detached.iter().rposition(|(from, _)| env.tracked.contains(from)) {
            // The drag-out's source tab was destroyed just before the new
            // window's first signal: the window was never hidden.
            self.recent_detached.remove(n);
            Some(ExclusionReason::DragOut)
        } else {
            None
        };

        if let Some(reason) = exclusion {
            self.opening.append(win);
            self.excluded.insert(win);
            return verdict_only(FirstSignalVerdict::Excluded(reason));
        }

        let seq = self.next_seq;
        self.next_seq += 1;
        self.opening.reserve(win);
        let foreground =
            foreground.filter(|f| *f != win && env.tracked.contains(f) && !self.candidates.contains_key(f));
        self.candidates.insert(
            win,
            Candidate {
                seq,
                appeared_at: now,
                desktop,
                foreground,
                location: None,
                selection: Vec::new(),
                phase: Phase::Hidden,
                target: None,
                probe: None,
                plan: None,
                exclude: None,
                created_tabs: Vec::new(),
                pending_ops: Vec::new(),
                slot: 0,
                waits_on: None,
                create_op: None,
                tab: None,
                registered: false,
                shown: false,
                activate_tries: 0,
                activated: false,
                foreground_exception: false,
                fold_into: FoldInto::Primary,
                passed_over: Vec::new(),
                switch_failure: None,
                retry_holder: None,
                fallback_used: false,
                switch_tab: None,
                switch_started: false,
                switch_deadline: None,
                create_deadline: None,
                show_deadline: None,
                matching_primary: false,
                buffered: Vec::new(),
                activated_after: Vec::new(),
                first_scan_seen: false,
                close_posts: 0,
                close_deadline: None,
            },
        );
        FirstSignalOutcome {
            verdict: FirstSignalVerdict::Candidate,
            commands: vec![
                Command::HideWindow { win },
                Command::SetTimer { id: fold_timer(TIMER_FIRST_NAV, seq), at: now + FOLD_FIRST_NAV_MS },
            ],
        }
    }

    /// Handles `Event::TabDetached { from }` for a tracked window (the Engine
    /// passes tracked windows only; Req 14.3 b).
    ///
    /// If a hidden candidate's first signal was at most 300 ms ago, the
    /// candidate is a drag-out: `Reveal`, excluded, released to the tracker.
    /// With several candidates the one with the latest first signal is taken.
    /// Otherwise the event is remembered for 100 ms, in case the dragged-out
    /// window's first signal arrives after it.
    pub fn tab_detached(&mut self, now: Millis, from: WinKey) -> FoldStep {
        if self.candidates.contains_key(&from) {
            return FoldStep::default();
        }
        self.recent_detached.retain(|&(_, at)| now.since(at) <= DRAG_OUT_BEFORE_MS);
        let dragged = self
            .candidates
            .iter()
            .filter(|(_, c)| c.phase == Phase::Hidden && now.since(c.appeared_at) <= DRAG_OUT_AFTER_MS)
            .max_by_key(|(_, c)| c.seq)
            .map(|(&win, _)| win);
        match dragged {
            Some(win) => {
                let step = self.release(win);
                self.then_start_ready(step)
            }
            None => {
                self.recent_detached.push((from, now));
                FoldStep::default()
            }
        }
    }

    /// Handles a scan of a window the `FoldingEngine` owns (`Event::WindowScanned`).
    /// Returns `None` when `obs.win` is not a candidate: the Engine then
    /// delivers the scan to the tracker as usual.
    ///
    /// The scan is buffered (the last one is replayed to the tracker if the
    /// candidate is left open, Req 14.15). A candidate whose first scan shows
    /// more than one tab is excluded: `Reveal`, released (Req 14.2).
    pub fn window_scanned(&mut self, obs: &WindowObservation) -> Option<FoldStep> {
        let candidate = self.candidates.get_mut(&obs.win)?;
        // The scan comes first, so the tracker knows the window when the
        // `InitialLocationKnown` that follows it is replayed.
        candidate.buffered.retain(|e| !matches!(e, Event::WindowScanned(_)));
        candidate.buffered.insert(0, Event::WindowScanned(obs.clone()));
        let first = !candidate.first_scan_seen;
        candidate.first_scan_seen = true;
        let early = matches!(candidate.phase, Phase::Hidden | Phase::Context);
        if first && early && obs.tabs.len() > 1 {
            let step = self.release(obs.win);
            return Some(self.then_start_ready(step));
        }
        Some(FoldStep::default())
    }

    /// Handles `Event::InitialLocationKnown` of a window the `FoldingEngine`
    /// owns (Req 14.9, 14.19). Returns `None` when `win` is not a candidate:
    /// the Engine then handles the event as usual.
    ///
    /// The Location and selection are kept (Req 14.6). Candidates are taken in
    /// first-signal order: this candidate's `ProbeFoldContext` is issued now
    /// only if every earlier candidate has already chosen its target;
    /// otherwise it waits and is probed when the last earlier one has chosen,
    /// or has gone (Req 14.19). Only the first announcement counts.
    pub fn initial_location_known(
        &mut self,
        win: WinKey,
        location: LocationId,
        selection: Vec<ItemId>,
    ) -> Option<FoldStep> {
        let candidate = self.candidates.get_mut(&win)?;
        if candidate.location.is_none() && candidate.phase == Phase::Hidden {
            // Kept for the tracker: a candidate that is left open replays it
            // after its last scan (Req 14.15).
            candidate.buffered.push(Event::InitialLocationKnown {
                win,
                tab: None,
                location: location.clone(),
                selection: selection.clone(),
            });
            candidate.location = Some(location);
            candidate.selection = selection;
        }
        Some(FoldStep { commands: self.start_ready(), released: Vec::new() })
    }

    /// Handles `Event::FoldContext` (Req 14.9, 14.12, 14.13, 14.19, 14.20): the
    /// answer to the `ProbeFoldContext` of one candidate. Returns an empty step
    /// for a probe no candidate is waiting for.
    ///
    /// `windows` is the tracker's fold view (`Tracker::fold_windows`): the open
    /// tracked windows, never a candidate, in Activation_Order, which is also
    /// the `activation` list handed to [`select`]. The Opening_Order is the
    /// engine's own, and the Folded_Tabs that earlier candidates are creating
    /// are added as pending tabs (Req 14.19).
    ///
    /// `tab_of` names the tab the tracker has at an index of a window (the
    /// Matching_Tab of a `Switch` needs its key for `SelectItems`).
    ///
    /// The same event answers the fresh `ProbeFoldContext` of a failure chain
    /// (a candidate that went back to [`Phase::Context`] after a failed
    /// Tab_Switch or a Fold_Target that was passed over, Req 14.23, 14.24):
    /// the window that held the Matching_Tab of a failed Tab_Switch takes the
    /// Folded_Tab if it is in `windows`, not closing and not unresponsive;
    /// otherwise it is passed over with its reason. [`select`] then runs with
    /// `exclude` set, so there is no new Matching_Tab search, and with every
    /// window already passed over treated as unavailable.
    ///
    /// The candidate then moves to [`Phase::Switching`] for a `Switch` plan
    /// (`ActivateTab` with the 1 s timer is issued at once, or when the
    /// earlier candidate whose pending Folded_Tab it lands on has activated it,
    /// see [`switch_waits_on`](Self::switch_waits_on)) and to
    /// [`Phase::Creating`] for a `Fold` plan. A `Fold` plan joins the queue of
    /// its Fold_Target and, when no `CreateTab` is outstanding there and every
    /// earlier candidate of the queue has issued its own, emits `CreateTab`
    /// with the 1 s timer of Req 14.13 (later ones follow as the earlier tabs
    /// register, see [`tab_created`](Self::tab_created)). With no Fold_Target the
    /// candidate is left open with the reason `no-fold-target` (Req 14.14).
    /// Either way the next waiting candidate is probed.
    ///
    /// `now` is the time of the event.
    pub fn fold_context(
        &mut self,
        now: Millis,
        probe: FoldProbeId,
        desktop: DesktopView,
        unresponsive: &BTreeSet<WinKey>,
        windows: &[FoldWindow<'_>],
        tab_of: TabOf<'_>,
    ) -> FoldStep {
        let Some(win) = self
            .candidates
            .iter()
            .find(|(_, c)| c.phase == Phase::Context && c.probe == Some(probe))
            .map(|(&win, _)| win)
        else {
            return FoldStep::default();
        };
        let activation: Vec<WinKey> = windows.iter().map(|w| w.win).collect();
        let opening = self.opening.to_vec();
        let primary = primary_of(windows, &opening, &desktop);
        // The plan, the index a Folded_Tab would get in its target, the
        // earlier candidate whose pending Folded_Tab a Switch would land on,
        // whether the plan is the retry into the window that held the
        // Matching_Tab of a failed Tab_Switch (Req 14.23), the windows passed
        // over on the way, and the window `select` must not take.
        let (plan, slot, waits_on, via_holder, passes, exclude) = {
            let candidate = &self.candidates[&win];
            let Some(location) = candidate.location.as_ref() else {
                return FoldStep::default();
            };
            // After a failed Tab_Switch the window that held the Matching_Tab
            // takes the Folded_Tab if it is open, not closing and responsive;
            // otherwise it is passed over with its reason (Req 14.23, 14.24).
            let mut passes: Vec<(WinKey, PassReason)> = Vec::new();
            let mut exclude = candidate.exclude;
            let mut holder_plan = None;
            if let Some(holder) = candidate.retry_holder {
                let state = windows.iter().find(|w| w.win == holder);
                let reason = match state {
                    None => Some(PassReason::Closed),
                    Some(w) if w.closing => Some(PassReason::Closing),
                    Some(_) if unresponsive.contains(&holder) => Some(PassReason::Unresponsive),
                    Some(_) => None,
                };
                match reason {
                    None => holder_plan = Some(FoldPlan::Fold { target: holder, reason: None }),
                    Some(reason) => {
                        passes.push((holder, reason));
                        exclude = Some(holder);
                    }
                }
            }
            // Windows passed over earlier on the chain are not taken again.
            let mut blocked = unresponsive.clone();
            blocked.extend(candidate.passed_over.iter().map(|&(w, _)| w));
            blocked.extend(passes.iter().map(|&(w, _)| w));
            let via_holder = holder_plan.is_some();
            // The Folded_Tabs earlier candidates are still creating, in
            // creation order (Req 14.19).
            let mut earlier: Vec<(u64, WinKey, WinKey, &LocationId)> = self
                .candidates
                .iter()
                .filter(|&(&other, _)| other != win)
                .filter_map(|(&other, c)| match (c.plan, c.location.as_ref()) {
                    (Some(FoldPlan::Fold { target, .. }), Some(l)) => Some((c.seq, other, target, l)),
                    _ => None,
                })
                .collect();
            earlier.sort_by_key(|&(seq, ..)| seq);
            // A Folded_Tab the tracker already lists (registered, with no
            // Location yet, with the new tab's start page, or with its
            // Location) is one of the window's tabs: counting it as pending
            // as well would count it twice. Its tab then carries the
            // candidate's Location for the search, and a Tab_Switch onto it
            // still waits for that candidate (Req 14.19).
            let tracker_index = |other: WinKey, target: WinKey| {
                let tab = self.candidates[&other].tab?;
                let count = windows.iter().find(|x| x.win == target)?.tabs.len();
                (0..count).find(|&i| tab_of(target, i) == Some(tab))
            };
            let mut seen: Vec<FoldWindow<'_>> = windows.to_vec();
            let mut listed: Vec<(WinKey, WinKey, usize)> = Vec::new();
            for &(_, other, target, l) in &earlier {
                let Some(index) = tracker_index(other, target) else { continue };
                listed.push((other, target, index));
                // Whatever the tracker shows there (no Location yet, or the
                // new tab's start page) is only on its way to the Location.
                if let Some(w) = seen.iter_mut().find(|x| x.win == target) {
                    w.tabs[index] = Some(l.fold_key());
                }
            }
            let earlier: Vec<(u64, WinKey, WinKey, &LocationId)> =
                earlier.into_iter().filter(|&(_, other, ..)| !listed.iter().any(|&(o, ..)| o == other)).collect();
            let pending: Vec<(WinKey, LocationKey<'_>)> =
                earlier.iter().map(|&(_, _, target, l)| (target, l.fold_key())).collect();
            let plan = holder_plan.unwrap_or_else(|| {
                select(&FoldCtx {
                    candidate_key: location.fold_key(),
                    foreground: candidate.foreground,
                    opening: &opening,
                    activation: &activation,
                    windows: &seen,
                    pending_folds: &pending,
                    desktop: &desktop,
                    unresponsive: &blocked,
                    exclude,
                })
            });
            let tabs_of = |w: WinKey| windows.iter().find(|x| x.win == w).map_or(0, |x| x.tabs.len());
            let pending_in = |w: WinKey| earlier.iter().filter(move |&&(_, _, target, _)| target == w);
            match plan {
                FoldPlan::Fold { target, .. } => {
                    (plan, tabs_of(target) + pending_in(target).count(), None, via_holder, passes, exclude)
                }
                FoldPlan::Switch { win: holder, index } => {
                    // An index past the window's real tabs is a pending
                    // Folded_Tab, counted in creation order (`select`); one
                    // the tracker lists is found by its place.
                    let waits_on = listed
                        .iter()
                        .find(|&&(_, target, at)| target == holder && at == index)
                        .map(|&(other, ..)| other)
                        .or_else(|| {
                            index
                                .checked_sub(tabs_of(holder))
                                .and_then(|n| pending_in(holder).nth(n))
                                .map(|&(_, other, ..)| other)
                        });
                    (plan, 0, waits_on, false, passes, exclude)
                }
                FoldPlan::NoTarget => (plan, 0, None, false, passes, exclude),
            }
        };

        let candidate = self.candidates.get_mut(&win).expect("candidate found above");
        candidate.desktop = desktop;
        candidate.probe = None;
        candidate.retry_holder = None;
        candidate.passed_over.extend(passes);
        candidate.exclude = exclude;
        let mut step = match plan {
            FoldPlan::Switch { win: holder, index } => {
                candidate.phase = Phase::Switching;
                candidate.target = Some(holder);
                candidate.plan = Some(plan);
                candidate.waits_on = waits_on;
                candidate.matching_primary = primary == Some(holder);
                // A Matching_Tab that exists now is known to the tracker; a
                // pending Folded_Tab is known once its candidate created it
                // (`start_switches`).
                candidate.switch_tab = if waits_on.is_none() { tab_of(holder, index) } else { None };
                FoldStep::default()
            }
            FoldPlan::Fold { target, reason } => {
                let chained = !candidate.passed_over.is_empty() || candidate.switch_failure.is_some();
                // After a window was passed over for a plain fold, the entry's
                // reason is that the Folded_Tab was not created (Req 14.30).
                let reason = if chained && candidate.switch_failure.is_none() && !candidate.foreground_exception {
                    None
                } else {
                    reason
                };
                candidate.phase = Phase::Creating;
                candidate.target = Some(target);
                candidate.plan = Some(FoldPlan::Fold { target, reason });
                candidate.slot = slot;
                if via_holder {
                    candidate.fold_into = FoldInto::MatchingWindow;
                    candidate.fallback_used = false;
                } else if reason == Some(FoldReason::Foreground) {
                    candidate.foreground_exception = true;
                    candidate.fold_into = FoldInto::Foreground;
                    candidate.fallback_used = false;
                } else {
                    let is_primary = primary == Some(target);
                    candidate.fold_into = if is_primary { FoldInto::Primary } else { FoldInto::Fallback };
                    candidate.fallback_used = !is_primary;
                }
                self.enqueue(win, target, !chained);
                FoldStep { commands: self.pump_creates(now, target), released: Vec::new() }
            }
            FoldPlan::NoTarget => self.leave_open(now, win, NotFoldedReason::NoFoldTarget),
        };
        step.commands.extend(self.start_switches(now));
        step.commands.extend(self.start_ready());
        step
    }

    // -----------------------------------------------------------------------
    // Folding into a Folded_Tab (Req 14.1, 14.6, 14.7, 14.17, 14.19, 14.22, 14.26)
    // -----------------------------------------------------------------------

    /// Handles `Event::WindowActivated { win }`: Explorer made `win` the
    /// foreground window. The Engine calls it for every activation.
    ///
    /// The tracker does not see the activation of a candidate while it is in
    /// progress; it is buffered, after the scan, and replayed with the rest at
    /// hand-over, so that a candidate that is left open ranks in the
    /// Activation_Order where Explorer put it (Req 2.1, 2.3, 14.15). Only the
    /// latest activation of a candidate is kept. Every window activated after
    /// it, whether another candidate or an ordinary window, is remembered too
    /// (latest once), so that at hand-over those windows are activated again
    /// in their order and still rank ahead of the candidate. Any other window
    /// is of no concern to a candidate that was never activated.
    pub fn window_activated(&mut self, win: WinKey) {
        for (&cand, candidate) in &mut self.candidates {
            if cand == win {
                candidate.buffered.retain(|e| !matches!(e, Event::WindowActivated { .. }));
                candidate.buffered.push(Event::WindowActivated { win });
                candidate.activated_after.clear();
            } else if candidate.buffered.iter().any(|e| matches!(e, Event::WindowActivated { .. })) {
                candidate.activated_after.retain(|w| *w != win);
                candidate.activated_after.push(win);
            }
        }
    }

    /// A destroyed window is no longer one of the activations to replay at a
    /// hand-over.
    fn forget_activation(&mut self, win: WinKey) {
        for candidate in self.candidates.values_mut() {
            candidate.activated_after.retain(|w| *w != win);
        }
    }

    /// Handles `Event::TabStarted { op, tab }`: the tab window of a `CreateTab`
    /// appeared. This is when the Folded_Tab counts as created (Req 14.13,
    /// 14.14): the 3 s timer for showing the Location starts (the 1 s limit of
    /// `CreateTab` is met; its timer finds the tab and does nothing). An op no
    /// candidate waits for is ignored, unless it is the late tab of a stopped
    /// fold, which is closed (Req 14.13).
    pub fn tab_started(&mut self, now: Millis, op: OpId, tab: TabKey) -> FoldStep {
        let Some(candidate) = self
            .candidates
            .values_mut()
            .find(|c| c.phase == Phase::Creating && c.create_op == Some(op) && c.tab.is_none())
        else {
            return self.close_late_tab(now, op, tab);
        };
        candidate.tab = Some(tab);
        candidate.created_tabs.push(tab);
        candidate.show_deadline = Some(now + FOLD_TAB_SHOW_MS);
        let timer = Command::SetTimer { id: fold_timer(TIMER_TAB_SHOW, candidate.seq), at: now + FOLD_TAB_SHOW_MS };
        FoldStep { commands: vec![timer], released: Vec::new() }
    }

    /// The tab of a `CreateTab` whose fold was stopped (the Fold_Target was
    /// passed over because it created no tab within 1 s, Req 14.13, 14.24)
    /// appears after all: it is closed, up to [`LATE_TAB_MS`] after the fold
    /// stopped. Any other op is ignored.
    fn close_late_tab(&mut self, now: Millis, op: OpId, tab: TabKey) -> FoldStep {
        match self.late_ops.remove(&op) {
            Some((_, stopped_at)) if now.since(stopped_at) <= LATE_TAB_MS => {
                self.late_tabs.insert(tab);
                let close = alloc_op(&mut self.next_op);
                FoldStep { commands: vec![Command::CloseTab { op: close, tab }], released: Vec::new() }
            }
            _ => FoldStep::default(),
        }
    }

    /// Handles `Event::TabCreated { win, tab }`: a tab registered in `win`. If
    /// it is the Folded_Tab a candidate is creating in `win`, the candidate
    /// issues `Navigate` to its Location (Req 14.1), and the next candidate of
    /// `win`'s queue may issue its `CreateTab` (at most one is outstanding per
    /// Fold_Target, Req 14.19). Any other tab is ignored.
    ///
    /// The tab is the candidate's when it is the tab `TabStarted` named, or,
    /// when no `TabStarted` came (an adapter that does not report it), the one
    /// the candidate waits for in `win`; in that case the tab counts as created
    /// now and the 3 s timer starts.
    pub fn tab_created(&mut self, now: Millis, win: WinKey, tab: TabKey) -> FoldStep {
        if self.late_tabs.remove(&tab) {
            // The late tab of a stopped fold, already closed: nobody's Folded_Tab.
            return FoldStep::default();
        }
        let waiting = |c: &Candidate| {
            c.phase == Phase::Creating && c.target == Some(win) && c.create_op.is_some() && !c.registered
        };
        let found = self
            .candidates
            .iter()
            .find(|(_, c)| waiting(c) && c.tab == Some(tab))
            .or_else(|| self.candidates.iter().filter(|(_, c)| waiting(c) && c.tab.is_none()).min_by_key(|(_, c)| c.seq))
            .map(|(&cand, _)| cand);
        let Some(cand) = found else {
            return FoldStep::default();
        };
        let op = alloc_op(&mut self.next_op);
        let candidate = self.candidates.get_mut(&cand).expect("candidate found above");
        let mut commands = Vec::new();
        if candidate.tab.is_none() {
            candidate.tab = Some(tab);
            candidate.created_tabs.push(tab);
            candidate.show_deadline = Some(now + FOLD_TAB_SHOW_MS);
            commands.push(Command::SetTimer { id: fold_timer(TIMER_TAB_SHOW, candidate.seq), at: now + FOLD_TAB_SHOW_MS });
        }
        candidate.registered = true;
        candidate.pending_ops.push(op);
        let location = candidate.location.clone().expect("a folding candidate has its Location");
        commands.push(Command::Navigate { op, tab, location });
        commands.extend(self.pump_creates(now, win));
        FoldStep { commands, released: Vec::new() }
    }

    /// Handles `Event::NavigationCompleted { tab, location }`: when `tab` is a
    /// candidate's Folded_Tab and `location` has the candidate's `fold_key`,
    /// the Folded_Tab shows the Location (Req 14.14, 14.17). Other completions
    /// (the new tab's first navigation to the start location) are ignored.
    ///
    /// The candidate then finishes in creation order among those folding into
    /// the same Fold_Target (Req 14.19): when every earlier one has activated
    /// its tab, it emits `SelectItems` (if it has a selection, Req 14.6) and
    /// `ActivateTab` on its rightmost index. Once that is confirmed
    /// ([`activation_confirmed`](Self::activation_confirmed)) it emits
    /// `ApplyPlacement` (a minimized target, per `placement_of`), `Focus` and
    /// `CloseWindow` for the candidate (Req 14.7).
    pub fn navigation_completed(
        &mut self,
        now: Millis,
        tab: TabKey,
        location: &LocationId,
        placement_of: PlacementOf<'_>,
    ) -> FoldStep {
        let Some((target, candidate)) = self.candidates.values_mut().find_map(|c| {
            (c.phase == Phase::Creating && c.registered && !c.shown && c.tab == Some(tab))
                .then_some(c)
                .and_then(|c| c.target.map(|t| (t, c)))
        }) else {
            return FoldStep::default();
        };
        if candidate.location.as_ref().map(LocationId::fold_key) != Some(location.fold_key()) {
            return FoldStep::default();
        }
        candidate.shown = true;
        let mut commands = self.advance(now, target, Some(placement_of));
        commands.extend(self.start_switches(now));
        FoldStep { commands, released: Vec::new() }
    }

    /// Handles `Event::ActivationConfirmed { op }`: the adapter saw the Folded_Tab
    /// selected after `ActivateTab { op }`. The candidate continues with
    /// `ApplyPlacement` (a minimized target), `Focus` and `CloseWindow`, and the
    /// next candidate of the queue may activate its tab (Req 14.7, 14.19). An
    /// op that is not an activation of a Folded_Tab is ignored.
    pub fn activation_confirmed(&mut self, now: Millis, op: OpId, placement_of: PlacementOf<'_>) -> FoldStep {
        if let Some(step) = self.switch_confirmed(now, op, placement_of) {
            return step;
        }
        let Some(candidate) = self
            .candidates
            .values_mut()
            .find(|c| c.phase == Phase::Finishing && !c.activated && c.pending_ops.contains(&op))
        else {
            return FoldStep::default();
        };
        candidate.activated = true;
        let target = candidate.target.expect("a finishing candidate has a target");
        let mut commands = self.advance(now, target, Some(placement_of));
        // A candidate that waited for this tab to be active may switch to it now.
        commands.extend(self.start_switches(now));
        FoldStep { commands, released: Vec::new() }
    }

    /// `ActivationConfirmed` for the `ActivateTab` of a Tab_Switch (Req 14.21):
    /// the Matching_Tab is the Active_Tab of its window. The candidate then
    /// selects the requested items in it (Req 14.6), shows a minimized window
    /// in its pre-minimize state, takes the focus, and has its candidate
    /// closed. `None` when `op` is not a Tab_Switch activation that is still
    /// awaited (a confirmation after the 1 s limit is ignored, Req 14.23).
    fn switch_confirmed(&mut self, now: Millis, op: OpId, placement_of: PlacementOf<'_>) -> Option<FoldStep> {
        let win = self
            .candidates
            .iter()
            .find(|(_, c)| c.phase == Phase::Switching && c.switch_started && c.pending_ops.contains(&op))
            .map(|(&win, _)| win)?;
        let select_op = alloc_op(&mut self.next_op);
        let candidate = self.candidates.get_mut(&win).expect("candidate found above");
        let close_timer = start_closing(candidate, now);
        let holder = candidate.target.expect("a switching candidate has a target");
        let mut commands = Vec::new();
        if let (false, Some(tab)) = (candidate.selection.is_empty(), candidate.switch_tab) {
            candidate.pending_ops.push(select_op);
            commands.push(Command::SelectItems { op: select_op, tab, items: candidate.selection.clone() });
        }
        commands.extend(placement_of(holder).map(|placement| Command::ApplyPlacement { win: holder, placement }));
        commands.push(Command::Focus(holder));
        commands.push(Command::CloseWindow { win });
        commands.push(close_timer);
        commands.extend(self.start_switches(now));
        Some(FoldStep { commands, released: Vec::new() })
    }

    /// Issues `ActivateTab` with the 1 s timer of Req 14.23 for every
    /// Tab_Switch whose Matching_Tab exists and is the Active_Tab of its
    /// window: a Matching_Tab that is a real tab starts at once; one that is
    /// the pending Folded_Tab of an earlier candidate starts when that
    /// candidate has shown and activated it (Req 14.19), and its key is the
    /// earlier candidate's [`folded_tab`](Self::folded_tab). If the earlier
    /// candidate gave up without making the tab, the Matching_Tab is closed
    /// and the switch fails.
    ///
    /// A Tab_Switch also starts only after every earlier candidate that is
    /// folding into, or still has a Tab_Switch to start in, the same window
    /// has finished (Req 14.19): its own `ActivateTab` is the last one in that
    /// window, so the tab of the last-created candidate ends active even
    /// though the switch needs no Folded_Tab of its own.
    fn start_switches(&mut self, now: Millis) -> Vec<Command> {
        // With folding turned off the candidates are being left open.
        if !self.enabled {
            return Vec::new();
        }
        let mut order: Vec<(u64, WinKey)> = self
            .candidates
            .iter()
            .filter(|(_, c)| c.phase == Phase::Switching && !c.switch_started)
            .map(|(&win, c)| (c.seq, win))
            .collect();
        order.sort_unstable();
        let mut commands = Vec::new();
        for (seq, win) in order {
            let candidate = &self.candidates[&win];
            let Some(FoldPlan::Switch { win: holder, index }) = candidate.plan else {
                continue;
            };
            let (tab, resolved) = match candidate.waits_on {
                None => (candidate.switch_tab, false),
                Some(earlier) => match self.candidates.get(&earlier) {
                    Some(c) if c.phase == Phase::Closing => (c.tab, true),
                    Some(_) => continue,
                    None => {
                        commands.extend(self.fail_switch(now, win, SwitchFailure::MatchingTabClosed));
                        continue;
                    }
                },
            };
            if self.held_back_by_earlier(win, holder) {
                if resolved {
                    // The tab it lands on is shown and active; the candidate
                    // that made it may be gone by the time the switch starts.
                    let candidate = self.candidates.get_mut(&win).expect("listed above");
                    candidate.switch_tab = tab;
                    candidate.waits_on = None;
                }
                continue;
            }
            let op = alloc_op(&mut self.next_op);
            let candidate = self.candidates.get_mut(&win).expect("listed above");
            candidate.switch_started = true;
            candidate.switch_tab = tab;
            candidate.switch_deadline = Some(now + FOLD_SWITCH_MS);
            candidate.pending_ops.push(op);
            commands.push(Command::ActivateTab { op, win: holder, index });
            commands.push(Command::SetTimer { id: fold_timer(TIMER_SWITCH, seq), at: now + FOLD_SWITCH_MS });
        }
        commands
    }

    /// Whether the Tab_Switch of `win` onto `holder` must wait for an earlier
    /// candidate: one that is folding into `holder` (queued there, not yet
    /// closing) or whose own Tab_Switch onto `holder` has not started
    /// (Req 14.19). Only earlier candidates hold a later one back, so the
    /// waits cannot form a cycle.
    fn held_back_by_earlier(&self, win: WinKey, holder: WinKey) -> bool {
        let seq = self.candidates[&win].seq;
        let earlier = |other: &WinKey| *other != win && self.candidates.get(other).is_some_and(|c| c.seq < seq);
        let queued = self.target_queues.get(&holder).is_some_and(|q| q.iter().any(earlier));
        queued
            || self.candidates.iter().any(|(other, c)| {
                earlier(other) && c.phase == Phase::Switching && !c.switch_started && c.target == Some(holder)
            })
    }

    /// Handles `Event::TabRevoked { win, tab }`: when `tab` is the Matching_Tab
    /// of a Tab_Switch in progress in `win`, the switch fails with
    /// [`SwitchFailure::MatchingTabClosed`] and the fold continues in the
    /// window that held it, without a new Matching_Tab search (Req 14.23).
    ///
    /// When `tab` is the Folded_Tab of a candidate still creating or finishing
    /// it in `win`, the candidate is left open with
    /// [`NotFoldedReason::FoldTargetClosed`] (Req 14.14): the tab is gone,
    /// usually because `win` began closing (its tabs are revoked before its
    /// `WindowDestroyed`), and a fold that completed now would close the
    /// candidate and lose the folder with the window. The candidates behind it
    /// go on. Any other tab is ignored; the Engine delivers the event to the
    /// tracker as well.
    pub fn tab_revoked(&mut self, now: Millis, win: WinKey, tab: TabKey) -> FoldStep {
        let failing: Vec<WinKey> = self
            .candidates
            .iter()
            .filter(|(_, c)| {
                c.phase == Phase::Switching && c.target == Some(win) && c.switch_started && c.switch_tab == Some(tab)
            })
            .map(|(&cand, _)| cand)
            .collect();
        let mut step = FoldStep::default();
        for cand in failing {
            step.commands.extend(self.fail_switch(now, cand, SwitchFailure::MatchingTabClosed));
        }
        let lost: Vec<WinKey> = self
            .candidates
            .iter()
            .filter(|(_, c)| {
                matches!(c.phase, Phase::Creating | Phase::Finishing) && c.target == Some(win) && c.tab == Some(tab)
            })
            .map(|(&cand, _)| cand)
            .collect();
        for cand in lost {
            let more = self.leave_open(now, cand, NotFoldedReason::FoldTargetClosed);
            step.commands.extend(more.commands);
            step.released.extend(more.released);
            // The candidate behind it, whose Folded_Tab may show its Location
            // already, goes on (Req 14.19).
            step.commands.extend(self.advance(now, win, None));
        }
        step
    }

    /// Handles the destruction of a window that is not a Folding_Candidate
    /// (Req 14.23, 14.24). The Engine calls it for every `WindowDestroyed`, in
    /// addition to the tracker (and to [`window_destroyed`](Self::window_destroyed)
    /// for a candidate):
    ///
    /// - a Tab_Switch whose Matching_Tab was in `win` fails with
    ///   [`SwitchFailure::FoldTargetClosed`] and the fold continues, without a
    ///   new Matching_Tab search, in another window (Req 14.23);
    /// - a candidate whose Fold_Target `win` was, and that has not created its
    ///   Folded_Tab yet, passes `win` over as [`PassReason::Closed`] and takes
    ///   the next Fold_Target (Req 14.13, 14.24);
    /// - one that had created its tab is left open, `fold-target-closed`
    ///   (Req 14.14).
    pub fn target_destroyed(&mut self, now: Millis, win: WinKey) -> FoldStep {
        self.forget_activation(win);
        let mut step = FoldStep::default();
        let mut affected: Vec<(WinKey, Phase, bool)> = self
            .candidates
            .iter()
            .filter(|(_, c)| c.target == Some(win))
            .map(|(&cand, c)| (cand, c.phase, c.tab.is_some()))
            .collect();
        // The Tab_Switches fail first: a candidate left open before them must
        // not start a switch that waited for it, onto `win`.
        affected.sort_by_key(|&(_, phase, _)| phase != Phase::Switching);
        for (cand, phase, has_tab) in affected {
            let more = match phase {
                Phase::Switching => FoldStep {
                    commands: self.fail_switch(now, cand, SwitchFailure::FoldTargetClosed),
                    released: Vec::new(),
                },
                Phase::Creating | Phase::Finishing if !has_tab => self.pass_over(now, cand, win, PassReason::Closed),
                Phase::Creating | Phase::Finishing => self.leave_open(now, cand, NotFoldedReason::FoldTargetClosed),
                // `CloseWindow` was issued: the fold is complete and the
                // candidate is on its way out; `win` closing later is no
                // concern of it (Req 14.1, 14.28). A Fold_Target that began
                // closing earlier was noticed at the revocation of the
                // Folded_Tab ([`tab_revoked`](Self::tab_revoked)).
                Phase::Hidden | Phase::Context | Phase::Closing => FoldStep::default(),
            };
            step.commands.extend(more.commands);
            step.released.extend(more.released);
        }
        step
    }

    /// A Tab_Switch of `win` failed (Req 14.23): the 1 s limit passed, or its
    /// Matching_Tab or the window holding it closed. The candidate asks for a
    /// fresh `FoldContext`; with it the fold goes into the window that held
    /// the Matching_Tab, or, if that window cannot take it, on the chain of
    /// Req 14.24, without a new Matching_Tab search.
    fn fail_switch(&mut self, now: Millis, win: WinKey, failure: SwitchFailure) -> Vec<Command> {
        let Some(candidate) = self.candidates.get_mut(&win) else {
            return Vec::new();
        };
        candidate.switch_failure.get_or_insert(failure);
        candidate.retry_holder = candidate.target;
        self.reprobe(now, win)
    }

    /// Handles `Event::Timer` of a `Fold` timer:
    ///
    /// - activation: an `ActivateTab` of a Folded_Tab that is not confirmed
    ///   within [`FOLD_ACTIVATE_MS`] is issued again, once
    ///   ([`FOLD_ACTIVATE_RETRIES`]); after that the fold goes on without the
    ///   confirmation and logs `ERROR operation=fold-activate-tab` (Explorer
    ///   already selects a new tab, so the tab is usually active);
    /// - tab creation: a Fold_Target that created no tab within
    ///   [`FOLD_TAB_CREATE_MS`] of the `CreateTab` is passed over and the next
    ///   Fold_Target is taken (Req 14.13, 14.24), or, after a fallback, the
    ///   candidate is left open;
    /// - tab show: a Folded_Tab that does not show the Location within
    ///   [`FOLD_TAB_SHOW_MS`] of its creation leaves the candidate open
    ///   (Req 14.14);
    /// - switch: a Tab_Switch not confirmed within [`FOLD_SWITCH_MS`] fails
    ///   (Req 14.23);
    /// - first navigation: a candidate whose Location is still unknown
    ///   [`FOLD_FIRST_NAV_MS`] after its first signal is left open
    ///   (Req 14.14, `first-nav-timeout-8s`);
    /// - close: a candidate that has not been destroyed [`FOLD_CLOSE_RETRY_MS`]
    ///   after `CloseWindow` gets the command once more; after another
    ///   [`FOLD_CLOSE_RETRY_MS`] it is revealed and handed over with
    ///   `ERROR operation=fold-close`, the Folded_Tab staying (Req 14.1);
    /// - a stale timer of any kind does nothing.
    pub fn timer(&mut self, now: Millis, id: TimerId, placement_of: PlacementOf<'_>) -> FoldStep {
        if id.owner != TimerOwner::Fold {
            return FoldStep::default();
        }
        let (kind, seq) = timer_parts(id);
        let Some(cand) = self.candidates.iter().find(|(_, c)| c.seq == seq).map(|(&win, _)| win) else {
            return FoldStep::default();
        };
        let reached = |deadline: Option<Millis>| deadline.is_some_and(|d| now >= d);
        let c = &self.candidates[&cand];
        match kind {
            TIMER_ACTIVATE => {}
            // The Fold_Target created no tab within 1 s (Req 14.13, 14.24): it
            // is passed over. A timer of an earlier attempt, or of a tab that
            // appeared in time, finds another state and does nothing.
            TIMER_TAB_CREATE => {
                let Some(target) = c.target else {
                    return FoldStep::default();
                };
                let stale = c.phase != Phase::Creating
                    || c.create_op.is_none()
                    || c.tab.is_some()
                    || c.registered
                    || !reached(c.create_deadline);
                return if stale { FoldStep::default() } else { self.pass_over(now, cand, target, PassReason::TabNotCreated) };
            }
            // The Folded_Tab does not show the Location within 3 s of its
            // creation (Req 14.14).
            TIMER_TAB_SHOW => {
                let stale = c.phase != Phase::Creating || c.tab.is_none() || c.shown || !reached(c.show_deadline);
                return if stale {
                    FoldStep::default()
                } else {
                    self.leave_open_advancing(now, cand, NotFoldedReason::LocationNotShown, placement_of)
                };
            }
            // No `ActivationConfirmed` within 1 s: the Tab_Switch failed (Req 14.23).
            TIMER_SWITCH => {
                let stale = c.phase != Phase::Switching || !c.switch_started || !reached(c.switch_deadline);
                return if stale {
                    FoldStep::default()
                } else {
                    FoldStep { commands: self.fail_switch(now, cand, SwitchFailure::NotActivated), released: Vec::new() }
                };
            }
            // The first navigation did not complete within 8 s of the first
            // signal (Req 14.14). Once the Location is known the navigation
            // has completed and the timer is stale.
            TIMER_FIRST_NAV => {
                let stale = c.phase != Phase::Hidden
                    || c.location.is_some()
                    || !reached(Some(c.appeared_at + FOLD_FIRST_NAV_MS));
                if stale {
                    return FoldStep::default();
                }
                // Later candidates that waited for this one's target go on.
                let step = self.leave_open(now, cand, NotFoldedReason::FirstNavTimeout);
                return self.then_start_ready(step);
            }
            // The candidate ignores `CloseWindow` (Req 14.1).
            TIMER_CLOSE => return self.close_timer(now, cand),
            _ => return FoldStep::default(),
        }
        let op = alloc_op(&mut self.next_op);
        let candidate = self.candidates.get_mut(&cand).expect("candidate found above");
        if candidate.phase != Phase::Finishing || candidate.activated {
            return FoldStep::default();
        }
        let target = candidate.target.expect("a finishing candidate has a target");
        if candidate.activate_tries <= FOLD_ACTIVATE_RETRIES {
            candidate.activate_tries += 1;
            candidate.pending_ops.push(op);
            let commands = vec![
                Command::ActivateTab { op, win: target, index: candidate.slot },
                Command::SetTimer { id: fold_timer(TIMER_ACTIVATE, candidate.seq), at: now + FOLD_ACTIVATE_MS },
            ];
            return FoldStep { commands, released: Vec::new() };
        }
        candidate.activated = true;
        let mut commands = vec![Command::AppendLog(LogEntry::Error {
            operation: "fold-activate-tab".into(),
            description: format!("the Folded_Tab's activation was not confirmed within {FOLD_ACTIVATE_MS} ms"),
        })];
        commands.extend(self.advance(now, target, Some(placement_of)));
        commands.extend(self.start_switches(now));
        FoldStep { commands, released: Vec::new() }
    }

    /// Handles `Event::WindowDestroyed` of a window the `FoldingEngine` owns.
    /// For a candidate that was closed by its fold (`CloseWindow` issued) the
    /// fold is complete (Req 14.1, 14.26): the `FOLD` entry is logged and
    /// `RescanWindow(target)` lets the tracker record the Folded_Tab, its
    /// position and Active_Tab within 1 s. The candidate gives up its reserved
    /// Opening_Order position and the next waiting candidate is probed.
    ///
    /// Returns `None` when `win` is not a candidate (the Engine then handles
    /// the event as usual; this includes a candidate that was left open or
    /// handed over after it ignored `CloseWindow`).
    ///
    /// A candidate destroyed in any other phase (Explorer or the user closed
    /// it before its fold ended) is abandoned: it gives up its Opening_Order
    /// position and its place in the Fold_Target queue, a `CloseTab` is
    /// issued for every tab created for it (Req 14.14), a tab it still has
    /// outstanding is closed if it appears late (Req 14.13), a Tab_Switch that
    /// waited for its Folded_Tab fails, and the next waiting candidate is
    /// probed. Nothing is revealed, logged or handed to the tracker: the
    /// window is gone and the tracker never knew it (Req 14.27, 14.28).
    pub fn window_destroyed(&mut self, now: Millis, win: WinKey) -> Option<FoldStep> {
        self.forget_activation(win);
        let candidate = self.candidates.get(&win)?;
        if candidate.phase != Phase::Closing {
            return Some(self.abandon(now, win));
        }
        let candidate = self.candidates.remove(&win).expect("candidate found above");
        self.forget(win);
        self.opening.remove(win);
        let mut commands = Vec::new();
        if let Some(location) = candidate.location.clone() {
            commands.push(Command::AppendLog(LogEntry::Fold { location, outcome: fold_outcome(&candidate) }));
        }
        if let Some(target) = candidate.target {
            commands.push(Command::RescanWindow(target));
        }
        Some(self.then_start_ready(FoldStep { commands, released: Vec::new() }))
    }

    /// The earlier candidate whose pending Folded_Tab the Tab_Switch of `win`
    /// lands on (Req 14.19, 14.20), while that candidate has not yet shown and
    /// activated the tab. The switch can start once this is `None`: the
    /// Matching_Tab then exists and is the Active_Tab; its key is
    /// [`folded_tab`](Self::folded_tab).
    pub fn switch_waits_on(&self, win: WinKey) -> Option<WinKey> {
        let earlier = self.candidates.get(&win)?.waits_on?;
        let c = self.candidates.get(&earlier)?;
        (c.phase != Phase::Closing).then_some(earlier)
    }

    /// The Folded_Tab a candidate is creating or has created, once known.
    pub fn folded_tab(&self, win: WinKey) -> Option<TabKey> {
        self.candidates.get(&win)?.tab
    }

    /// Adds a candidate to the queue of its Fold_Target: in creation order
    /// (Req 14.19) when `in_order`, otherwise at the end. A candidate that
    /// comes back to the queues after a failed attempt is appended: its
    /// `slot` (computed at the fresh `FoldContext`) counts every Folded_Tab
    /// already planned in the target.
    fn enqueue(&mut self, win: WinKey, target: WinKey, in_order: bool) {
        let seq = self.candidates[&win].seq;
        let candidates = &self.candidates;
        let queue = self.target_queues.entry(target).or_default();
        let at = if in_order {
            queue.iter().position(|w| candidates.get(w).is_some_and(|c| c.seq > seq)).unwrap_or(queue.len())
        } else {
            queue.len()
        };
        queue.insert(at, win);
    }

    /// Takes a candidate out of its Fold_Target queue because it gives up the
    /// target (Req 14.13, 14.14, 14.24). The candidates behind it move one
    /// tab to the left: their `slot` is one lower.
    fn leave_queue(&mut self, win: WinKey) {
        for queue in self.target_queues.values_mut() {
            let Some(at) = queue.iter().position(|w| *w == win) else {
                continue;
            };
            queue.remove(at);
            for later in queue.iter().skip(at) {
                if let Some(c) = self.candidates.get_mut(later) {
                    c.slot = c.slot.saturating_sub(1);
                }
            }
        }
        self.target_queues.retain(|_, q| !q.is_empty());
    }

    /// Removes a candidate from every Fold_Target queue.
    fn forget(&mut self, win: WinKey) {
        for queue in self.target_queues.values_mut() {
            queue.retain(|w| *w != win);
        }
        self.target_queues.retain(|_, q| !q.is_empty());
    }

    /// Issues the next `CreateTab` of `target`'s queue: for the first candidate
    /// that has not issued one, provided no earlier candidate's tab is still
    /// being created (issued but not registered). The command comes with the
    /// 1 s timer of Req 14.13 (`FOLD_TAB_CREATE_MS`).
    fn pump_creates(&mut self, now: Millis, target: WinKey) -> Vec<Command> {
        // With folding turned off no further Folded_Tab is requested (Req 12.10).
        let Some(queue) = self.target_queues.get(&target).filter(|_| self.enabled) else {
            return Vec::new();
        };
        let mut next = None;
        for &win in queue {
            let Some(c) = self.candidates.get(&win) else { continue };
            match c.create_op {
                Some(_) if !c.registered => return Vec::new(),
                Some(_) => {}
                None => {
                    next = Some(win);
                    break;
                }
            }
        }
        let Some(win) = next else {
            return Vec::new();
        };
        let op = alloc_op(&mut self.next_op);
        let candidate = self.candidates.get_mut(&win).expect("listed above");
        candidate.create_op = Some(op);
        candidate.create_deadline = Some(now + FOLD_TAB_CREATE_MS);
        candidate.pending_ops.push(op);
        vec![
            Command::CreateTab { op, win: target },
            Command::SetTimer { id: fold_timer(TIMER_TAB_CREATE, candidate.seq), at: now + FOLD_TAB_CREATE_MS },
        ]
    }

    /// Moves the candidates of `target`'s queue forward, in creation order
    /// (Req 14.19): the head, once its Folded_Tab shows the Location, selects
    /// its items and activates the tab (phase [`Phase::Finishing`]); once the
    /// activation is confirmed it applies the placement of a minimized target,
    /// takes the focus and has its candidate closed (phase [`Phase::Closing`]),
    /// leaves the queue, and the next one proceeds.
    ///
    /// `placement_of` is `None` where a candidate left the queue and the
    /// Engine's placements are not at hand (a destroyed candidate): the new
    /// head may start (select and activate), but one whose activation is
    /// already confirmed, which cannot be behind another, would wait.
    fn advance(&mut self, now: Millis, target: WinKey, placement_of: Option<PlacementOf<'_>>) -> Vec<Command> {
        let mut commands = Vec::new();
        while let Some(&head) = self.target_queues.get(&target).and_then(VecDeque::front) {
            let Some(candidate) = self.candidates.get_mut(&head) else {
                self.forget(head);
                continue;
            };
            match candidate.phase {
                Phase::Creating if candidate.shown => {
                    let tab = candidate.tab.expect("a shown Folded_Tab is known");
                    if !candidate.selection.is_empty() {
                        let op = alloc_op(&mut self.next_op);
                        candidate.pending_ops.push(op);
                        commands.push(Command::SelectItems { op, tab, items: candidate.selection.clone() });
                    }
                    let op = alloc_op(&mut self.next_op);
                    candidate.pending_ops.push(op);
                    candidate.activate_tries = 1;
                    candidate.phase = Phase::Finishing;
                    commands.push(Command::ActivateTab { op, win: target, index: candidate.slot });
                    commands.push(Command::SetTimer {
                        id: fold_timer(TIMER_ACTIVATE, candidate.seq),
                        at: now + FOLD_ACTIVATE_MS,
                    });
                    break;
                }
                Phase::Finishing if candidate.activated => {
                    // Showing a minimized target needs the placement; without
                    // it the candidate waits for the next call that has it.
                    let Some(placement_of) = placement_of else {
                        break;
                    };
                    let close_timer = start_closing(candidate, now);
                    commands.extend(placement_of(target).map(|placement| Command::ApplyPlacement { win: target, placement }));
                    commands.push(Command::Focus(target));
                    commands.push(Command::CloseWindow { win: head });
                    commands.push(close_timer);
                    if let Some(queue) = self.target_queues.get_mut(&target) {
                        queue.pop_front();
                        if queue.is_empty() {
                            self.target_queues.remove(&target);
                        }
                    }
                }
                _ => break,
            }
        }
        commands
    }

    /// Appends the `ProbeFoldContext` of the next candidate that may start to
    /// `step` (Req 14.19).
    fn then_start_ready(&mut self, mut step: FoldStep) -> FoldStep {
        step.commands.extend(self.start_ready());
        step
    }

    /// Issues `ProbeFoldContext` for the earliest candidate (by first signal)
    /// whose Location is known, provided every earlier candidate has chosen its
    /// target, that is, is past [`Phase::Context`]. An earlier candidate whose
    /// Location is not known yet, or that waits for its context, holds the
    /// later ones back (Req 14.19). At most one candidate is in
    /// [`Phase::Context`] at a time.
    fn start_ready(&mut self) -> Vec<Command> {
        let mut order: Vec<(u64, WinKey)> = self.candidates.iter().map(|(&win, c)| (c.seq, win)).collect();
        order.sort_unstable();
        for (_, win) in order {
            let candidate = self.candidates.get_mut(&win).expect("listed above");
            match candidate.phase {
                Phase::Hidden if candidate.location.is_some() => {
                    let probe = FoldProbeId(self.next_probe);
                    self.next_probe += 1;
                    candidate.phase = Phase::Context;
                    candidate.probe = Some(probe);
                    return vec![Command::ProbeFoldContext { probe, candidate: win }];
                }
                Phase::Hidden | Phase::Context => return Vec::new(),
                Phase::Switching | Phase::Creating | Phase::Finishing | Phase::Closing => {}
            }
        }
        Vec::new()
    }

    /// Leaves a candidate open (Req 14.14, 14.15): the tabs created for it are
    /// closed, it is revealed, handed to the tracker with its buffered events,
    /// and a `not-folded` entry is logged (with the Tab_Switch failure reason
    /// when a failed Tab_Switch led here). The candidate leaves its
    /// Fold_Target's queue, the next candidate of that queue may create its
    /// tab, and a Tab_Switch that was waiting for this candidate's Folded_Tab
    /// fails. A `CreateTab` still outstanding is remembered, so its tab is
    /// closed if it appears late.
    ///
    /// The hand-over is the returned [`Released`] (the candidate's buffered
    /// last scan, `InitialLocationKnown` and latest `WindowActivated`, in that
    /// order); the Engine
    /// replays them to the tracker, issues `RescanWindow`, and marks the window
    /// as seen by trigger detection (task 22.3). The window joins the
    /// Opening_Order at its first-signal position and is excluded from
    /// Tab_Folding for good (Req 14.15).
    ///
    /// The `FOLD` entry names the candidate's Location; while the first
    /// navigation has not completed (`first-nav-timeout-8s`, or folding turned
    /// off before the Location was known) the candidate has none unless its
    /// last scan shows one, and no entry is written.
    ///
    /// The next candidate of the queue is not advanced here (see
    /// [`leave_open_advancing`](Self::leave_open_advancing)), which is right
    /// where it cannot be ahead of the head: a candidate that never created
    /// its tab (the ones behind it have not created theirs either, Req 14.19),
    /// a destroyed Fold_Target (every candidate of its queue is left open in
    /// turn) and folding turned off (all are left open).
    fn leave_open(&mut self, now: Millis, win: WinKey, reason: NotFoldedReason) -> FoldStep {
        let Some(candidate) = self.candidates.get(&win) else {
            return FoldStep::default();
        };
        let log = log_location(candidate).map(|location| {
            Command::AppendLog(LogEntry::Fold {
                location,
                outcome: FoldOutcome::NotFolded { reason, switch_reason: candidate.switch_failure },
            })
        });
        let target = candidate.target;
        let created = candidate.created_tabs.clone();
        let stopped_op = candidate.create_op.filter(|_| candidate.tab.is_none());
        let mut commands = Vec::new();
        for tab in created {
            let op = alloc_op(&mut self.next_op);
            commands.push(Command::CloseTab { op, tab });
        }
        if let (Some(op), Some(target)) = (stopped_op, target) {
            self.remember_late_op(now, op, target);
        }
        self.leave_queue(win);
        let mut step = self.release(win);
        commands.append(&mut step.commands);
        commands.extend(log);
        if let Some(target) = target {
            commands.extend(self.pump_creates(now, target));
        }
        commands.extend(self.fail_waiters(now, win));
        // A Tab_Switch that waited for this candidate to finish may start.
        commands.extend(self.start_switches(now));
        step.commands = commands;
        step
    }

    /// [`leave_open`](Self::leave_open) for the head of a queue whose
    /// Folded_Tab was created and which is not given up together with the
    /// whole queue (its Location was not shown in time, Req 14.14). The
    /// candidate behind it, whose Folded_Tab may already show its Location,
    /// has no timer pending that would move it on, so the queue is advanced:
    /// the new head selects its items and activates its tab (Req 14.19).
    fn leave_open_advancing(
        &mut self,
        now: Millis,
        win: WinKey,
        reason: NotFoldedReason,
        placement_of: PlacementOf<'_>,
    ) -> FoldStep {
        let target = self.candidates.get(&win).and_then(|c| c.target);
        let mut step = self.leave_open(now, win, reason);
        if let Some(target) = target {
            step.commands.extend(self.advance(now, target, Some(placement_of)));
        }
        step.commands.extend(self.start_switches(now));
        step
    }

    /// Handles `SettingsChanged` for Tab_Folding (Req 12.10, 14.14). Turning
    /// the option off leaves every candidate in progress open, in first-signal
    /// order, with `not-folded reason=folding-disabled`; a candidate whose
    /// `CloseWindow` was already issued is not stopped (its fold has
    /// completed and it is on its way out). New windows are excluded from then
    /// on, and no further `CreateTab` or `ProbeFoldContext` is issued. Turning
    /// the option on affects only windows that appear later (Req 14.3 f). A
    /// call that leaves the option unchanged does nothing.
    pub fn settings_changed(&mut self, now: Millis, enabled: bool) -> FoldStep {
        let was = self.enabled;
        self.enabled = enabled;
        if enabled || !was {
            return FoldStep::default();
        }
        let mut order: Vec<(u64, WinKey)> = self
            .candidates
            .iter()
            .filter(|(_, c)| c.phase != Phase::Closing)
            .map(|(&win, c)| (c.seq, win))
            .collect();
        order.sort_unstable();
        let mut step = FoldStep::default();
        for (_, win) in order {
            let more = self.leave_open(now, win, NotFoldedReason::FoldingDisabled);
            step.commands.extend(more.commands);
            step.released.extend(more.released);
        }
        step
    }

    /// `HideWindow { win }` failed (for example an elevated Explorer window,
    /// Req 14.16): `ERROR operation=fold-hide` is logged and the fold goes on;
    /// the window may be visible briefly. A window that is not a candidate is
    /// ignored.
    pub fn hide_failed(&mut self, win: WinKey, error: &crate::engine::ShellError) -> FoldStep {
        if !self.candidates.contains_key(&win) {
            return FoldStep::default();
        }
        FoldStep {
            commands: vec![Command::AppendLog(LogEntry::Error {
                operation: "fold-hide".into(),
                description: format!("hiding the Folding_Candidate failed ({:#x}): {}", error.code, error.message),
            })],
            released: Vec::new(),
        }
    }

    /// The Explorer process of the candidates `wins` exited (`ProcessExited`,
    /// Req 14.14): each of those still in progress is dropped with `ERROR
    /// operation=fold-shell`. The window is gone, so nothing is revealed and
    /// nothing is replayed to the tracker, which never knew it; it gives up
    /// its Opening_Order position and its place in the Fold_Target queue, a
    /// `CloseTab` is issued for every tab created for it (the Fold_Target may
    /// live in another process), a Tab_Switch that waited for its Folded_Tab
    /// fails, and the next waiting candidate is probed, as for a destroyed
    /// window ([`window_destroyed`](Self::window_destroyed)).
    ///
    /// A candidate whose `CloseWindow` was already issued has completed its
    /// fold; its process ending is the close it was waiting for, so it is
    /// finished as a destroyed window, without an error. A window that is not
    /// a candidate is ignored. The Engine calls this before the tracker
    /// handles the `ProcessExited`, so the number of candidates in progress
    /// the tracker is told is current.
    ///
    /// Candidates are dropped latest first, so that none is still waiting on
    /// or queued behind one that is dropped in the same call.
    pub fn process_exited(&mut self, now: Millis, wins: &[WinKey]) -> FoldStep {
        let mut order: Vec<(u64, WinKey)> =
            wins.iter().filter_map(|win| Some((self.candidates.get(win)?.seq, *win))).collect();
        order.sort_unstable_by(|a, b| b.cmp(a));
        let mut step = FoldStep::default();
        for (_, win) in order {
            let more = if self.candidates[&win].phase == Phase::Closing {
                self.window_destroyed(now, win).unwrap_or_default()
            } else {
                let mut more = self.abandon(now, win);
                more.commands.insert(0, fold_shell_error("the Explorer process of the Folding_Candidate exited"));
                more
            };
            step.commands.extend(more.commands);
            step.released.extend(more.released);
        }
        step
    }

    /// The shell was lost (`ShellLost`, Req 1.10, 14.14): every candidate in
    /// progress is dropped with `ERROR operation=fold-shell`, in first-signal
    /// order. No command that needs the shell is issued (no `CloseTab`, no
    /// `CreateTab`, no probe) and nothing is replayed to the tracker, which
    /// rescans every open window at `ShellAvailable`. A candidate may live in
    /// a process that survives the shell, and it is hidden, so it is
    /// revealed; it stays excluded from Tab_Folding, with a real
    /// Opening_Order position, so that the rescan treats it as an ordinary
    /// window. The Folded_Tab it already created, if any, stays in its
    /// Fold_Target. A candidate whose `CloseWindow` was already issued has
    /// completed its fold and is left to finish closing (an HWND operation).
    pub fn shell_lost(&mut self) -> FoldStep {
        let mut order: Vec<(u64, WinKey)> =
            self.candidates.iter().filter(|(_, c)| c.phase != Phase::Closing).map(|(&win, c)| (c.seq, win)).collect();
        order.sort_unstable();
        let mut step = FoldStep::default();
        for (_, win) in order {
            self.candidates.remove(&win);
            self.forget(win);
            self.excluded.insert(win);
            self.opening.reveal(win);
            step.commands.push(fold_shell_error("the shell was lost while the Folding_Candidate was in progress"));
            step.commands.push(Command::Reveal { win });
        }
        step
    }

    /// A candidate that was told to close did not go away (Req 14.1). The
    /// first time [`FOLD_CLOSE_RETRY_MS`] after the command, `CloseWindow` is
    /// posted once more. The second time, the candidate is revealed and handed
    /// over so that no invisible window remains: `ERROR operation=fold-close`,
    /// the `FOLD` entry of the fold that did complete, and `RescanWindow` of the
    /// Fold_Target, whose Folded_Tab (or Matching_Tab) stays. A timer of a
    /// candidate that is not closing, or not due yet, does nothing.
    fn close_timer(&mut self, now: Millis, win: WinKey) -> FoldStep {
        let Some(candidate) = self.candidates.get_mut(&win) else {
            return FoldStep::default();
        };
        if candidate.phase != Phase::Closing || !candidate.close_deadline.is_some_and(|d| now >= d) {
            return FoldStep::default();
        }
        if candidate.close_posts < FOLD_CLOSE_POSTS {
            candidate.close_posts += 1;
            let at = now + FOLD_CLOSE_RETRY_MS;
            candidate.close_deadline = Some(at);
            let id = fold_timer(TIMER_CLOSE, candidate.seq);
            return FoldStep {
                commands: vec![Command::CloseWindow { win }, Command::SetTimer { id, at }],
                released: Vec::new(),
            };
        }
        let log = log_location(candidate)
            .map(|location| Command::AppendLog(LogEntry::Fold { location, outcome: fold_outcome(candidate) }));
        let target = candidate.target;
        let posts = candidate.close_posts;
        let mut step = self.release(win);
        step.commands.push(Command::AppendLog(LogEntry::Error {
            operation: "fold-close".into(),
            description: format!("the Folding_Candidate did not close after {posts} requests"),
        }));
        step.commands.extend(log);
        step.commands.extend(target.map(Command::RescanWindow));
        self.then_start_ready(step)
    }

    /// A candidate was destroyed before its fold ended (see
    /// [`window_destroyed`](Self::window_destroyed)).
    fn abandon(&mut self, now: Millis, win: WinKey) -> FoldStep {
        let Some(candidate) = self.candidates.remove(&win) else {
            return FoldStep::default();
        };
        self.forget_activation(win);
        let mut commands = Vec::new();
        for &tab in &candidate.created_tabs {
            let op = alloc_op(&mut self.next_op);
            commands.push(Command::CloseTab { op, tab });
        }
        if let (Some(op), Some(target)) = (candidate.create_op.filter(|_| candidate.tab.is_none()), candidate.target) {
            self.remember_late_op(now, op, target);
        }
        self.leave_queue(win);
        self.forget(win);
        self.opening.remove(win);
        if let Some(target) = candidate.target {
            commands.extend(self.pump_creates(now, target));
            // The candidate behind it, whose Folded_Tab may show its Location
            // already, goes on (Req 14.19).
            commands.extend(self.advance(now, target, None));
        }
        commands.extend(self.fail_waiters(now, win));
        commands.extend(self.start_switches(now));
        self.then_start_ready(FoldStep { commands, released: Vec::new() })
    }

    /// Remembers the `CreateTab` of a Fold_Target that was passed over, so that
    /// a tab it creates after all is closed (Req 14.13, [`LATE_TAB_MS`]).
    fn remember_late_op(&mut self, now: Millis, op: OpId, target: WinKey) {
        self.late_ops.retain(|_, &mut (_, at)| now.since(at) <= LATE_TAB_MS);
        self.late_ops.insert(op, (target, now));
    }

    /// A Fold_Target attempt of `win` failed before its Folded_Tab was created
    /// (Req 14.13, 14.24): `failed` created no tab within 1 s, is closed, closing
    /// or unresponsive. `failed` is recorded as passed over with its reason and
    /// excluded from the next choice (so no new Matching_Tab search happens).
    /// The next candidate of its queue may create its tab.
    ///
    /// Then the candidate asks for a fresh `FoldContext` to take the next
    /// Fold_Target: the Primary_Window as of that moment, or the first fallback
    /// window (Req 14.13, 14.24). If `failed` was itself a fallback window the
    /// candidate is left open instead: `tab-not-created-1s` (Req 14.14).
    fn pass_over(&mut self, now: Millis, win: WinKey, failed: WinKey, reason: PassReason) -> FoldStep {
        let Some(candidate) = self.candidates.get_mut(&win) else {
            return FoldStep::default();
        };
        candidate.passed_over.push((failed, reason));
        candidate.exclude.get_or_insert(failed);
        let last_fallback = candidate.fallback_used;
        let stopped_op = candidate.create_op.filter(|_| candidate.tab.is_none());
        if let Some(op) = stopped_op {
            self.remember_late_op(now, op, failed);
        }
        self.leave_queue(win);
        let mut commands = self.pump_creates(now, failed);
        if last_fallback {
            let mut step = self.leave_open(now, win, NotFoldedReason::TabNotCreated);
            commands.append(&mut step.commands);
            step.commands = commands;
            return step;
        }
        commands.extend(self.reprobe(now, win));
        // A Tab_Switch that waited for this candidate to finish may start.
        commands.extend(self.start_switches(now));
        FoldStep { commands, released: Vec::new() }
    }

    /// Takes a candidate back to [`Phase::Context`] with a fresh
    /// `ProbeFoldContext` after a failed attempt (Req 14.23, 14.24). Its old
    /// plan no longer counts as a pending Folded_Tab, and a Tab_Switch that
    /// waited for its Folded_Tab fails.
    fn reprobe(&mut self, now: Millis, win: WinKey) -> Vec<Command> {
        let probe = FoldProbeId(self.next_probe);
        self.next_probe += 1;
        let Some(candidate) = self.candidates.get_mut(&win) else {
            return Vec::new();
        };
        candidate.reset_attempt();
        candidate.phase = Phase::Context;
        candidate.probe = Some(probe);
        let mut commands = vec![Command::ProbeFoldContext { probe, candidate: win }];
        commands.extend(self.fail_waiters(now, win));
        commands
    }

    /// The Tab_Switch of every candidate that waits for the Folded_Tab of
    /// `earlier` fails: that tab does not come to exist where it was expected
    /// (Req 14.19, 14.23).
    fn fail_waiters(&mut self, now: Millis, earlier: WinKey) -> Vec<Command> {
        // With folding turned off every waiting switch is left open itself.
        if !self.enabled {
            return Vec::new();
        }
        let waiting: Vec<WinKey> = self
            .candidates
            .iter()
            .filter(|(_, c)| c.phase == Phase::Switching && !c.switch_started && c.waits_on == Some(earlier))
            .map(|(&win, _)| win)
            .collect();
        let mut commands = Vec::new();
        for win in waiting {
            commands.extend(self.fail_switch(now, win, SwitchFailure::MatchingTabClosed));
        }
        commands
    }

    /// Gives up a candidate as a drag-out or multi-tab window: it becomes
    /// excluded and a real Opening_Order entry, is revealed, and its buffered
    /// events go to the tracker.
    fn release(&mut self, win: WinKey) -> FoldStep {
        let Some(candidate) = self.candidates.remove(&win) else {
            return FoldStep::default();
        };
        self.forget(win);
        self.excluded.insert(win);
        self.opening.reveal(win);
        // The candidate's activation, then the windows activated after it.
        let mut buffered = candidate.buffered;
        buffered.extend(candidate.activated_after.into_iter().map(|win| Event::WindowActivated { win }));
        FoldStep {
            commands: vec![Command::Reveal { win }],
            released: vec![Released { win, buffered }],
        }
    }
}

// ---------------------------------------------------------------------------
// Selection (pure)
// ---------------------------------------------------------------------------

/// One tracked window as the pure selection sees it (Req 14.9, 14.20).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoldWindow<'a> {
    pub win: WinKey,
    /// The `fold_key` of every tab's Location, left to right; `None` for a tab
    /// whose Location is not known (it matches nothing). A Deferred_Tab has
    /// the key of the Location the tracker records for it (Req 5.13).
    pub tabs: Vec<Option<LocationKey<'a>>>,
    /// Index of the Active_Tab in `tabs`.
    pub active: Option<usize>,
    /// Every tab of the window is tombstoned, or the window was destroyed:
    /// a Closing_Explorer_Window (Req 14.9, 14.24).
    pub closing: bool,
}

/// Input of the pure [`select`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoldCtx<'a> {
    /// `fold_key` of the candidate's Location.
    pub candidate_key: LocationKey<'a>,
    /// The candidate's Foreground_Explorer_Window, if tracked and not a
    /// candidate (Req 14.22).
    pub foreground: Option<WinKey>,
    /// Opening_Order, earliest first (Req 14.8).
    pub opening: &'a [WinKey],
    /// Activation_Order, most recent first.
    pub activation: &'a [WinKey],
    /// The tracked windows.
    pub windows: &'a [FoldWindow<'a>],
    /// Folded_Tabs that earlier candidates are still creating: each counts as
    /// an open tab at the right end of its Fold_Target (Req 14.19).
    pub pending_folds: &'a [(WinKey, LocationKey<'a>)],
    /// The windows on the candidate's Virtual_Desktop (Req 14.11, 14.12).
    pub desktop: &'a DesktopView,
    /// The windows that did not answer `WM_NULL` in time (Req 14.13).
    pub unresponsive: &'a BTreeSet<WinKey>,
    /// The window that held the Matching_Tab of a failed Tab_Switch or fold,
    /// which the next target must not be (Req 14.24).
    pub exclude: Option<WinKey>,
}

/// Why a Fold_Target other than the Primary_Window was taken (Req 14.13, 14.22).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FoldReason {
    /// The Primary_Window did not answer in time.
    PrimaryUnresponsive,
    /// The Primary_Window is closing.
    PrimaryClosing,
    /// The Matching_Tab is the Active_Tab of the Foreground_Explorer_Window
    /// (the log's `folded-foreground`).
    Foreground,
}

/// The result of the pure [`select`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FoldPlan {
    /// Activate the Matching_Tab at `index` of `win` (Req 14.21).
    Switch { win: WinKey, index: usize },
    /// Create a Folded_Tab in `target`; `reason` is why it is not the
    /// Primary_Window, if it is not.
    Fold { target: WinKey, reason: Option<FoldReason> },
    /// There is no Fold_Target (Req 14.14).
    NoTarget,
}

/// Chooses between Tab_Switch and Tab_Fold for a candidate and names the window
/// (Req 14.9, 14.13, 14.20–14.22, 14.24).
///
/// - The *candidate windows* are the entries of `ctx.windows` (tracked, open,
///   never a Folding_Candidate) on the candidate's desktop. `ctx.opening` may
///   name windows that are not in `ctx.windows` (excluded or orphaned windows);
///   they are never chosen.
/// - The Primary_Window is the first candidate window in the Opening_Order
///   (Req 14.9). A tracked window missing from the Opening_Order ranks after
///   every window in it. The Primary_Window may be unresponsive or closing; it
///   is then not searched and not the target (Req 14.13).
/// - The fallback list holds the other candidate windows that are neither
///   closing nor unresponsive, in Activation_Order (windows missing from it
///   last, in `ctx.windows` order).
/// - The Matching_Tab is searched in the Primary_Window (unless unresponsive or
///   closing) and then in the fallback list: within a window its Active_Tab if
///   it matches, otherwise the leftmost match, with the pending Folded_Tabs of
///   earlier candidates after the window's real tabs, in `pending_folds` order
///   (Req 14.19, 14.20).
/// - A Matching_Tab gives [`FoldPlan::Switch`], except that the Active_Tab of
///   the Foreground_Explorer_Window gives `Fold { target: foreground, reason:
///   Some(Foreground) }` (Req 14.21, 14.22).
/// - Without a Matching_Tab the target is the Primary_Window, or the first
///   fallback window with the reason `PrimaryUnresponsive` or `PrimaryClosing`
///   (unresponsive wins when both hold); with neither, [`FoldPlan::NoTarget`].
///
/// `exclude` is the window that held the Matching_Tab of a failed Tab_Switch or
/// fold (Req 14.23, 14.24). The search then runs without a new Matching_Tab
/// search, so the plan is `Fold` or `NoTarget`, never `Switch`, and the target
/// is never `exclude`: the Primary_Window as of now, or the first fallback
/// window other than `exclude` when the Primary_Window is `exclude` or fails
/// 14.13. The `reason` is that of the Primary_Window's condition and is `None`
/// when the Primary_Window is `exclude` and otherwise usable.
pub fn select(ctx: &FoldCtx<'_>) -> FoldPlan {
    let on_desktop = |win: WinKey| match ctx.desktop {
        DesktopView::Known(set) => set.contains(&win),
        DesktopView::Unknown => true,
    };
    let rank = |order: &[WinKey], win: WinKey| order.iter().position(|&w| w == win).unwrap_or(usize::MAX);

    // Windows on the desktop, ranked by Opening_Order (stable: ties keep the
    // list order).
    let mut by_opening: Vec<&FoldWindow<'_>> = ctx.windows.iter().filter(|w| on_desktop(w.win)).collect();
    by_opening.sort_by_key(|w| rank(ctx.opening, w.win));
    let Some(&primary) = by_opening.first() else {
        return FoldPlan::NoTarget;
    };

    let primary_unresponsive = ctx.unresponsive.contains(&primary.win);
    let primary_usable = !primary_unresponsive && !primary.closing;
    let primary_reason = if primary_unresponsive {
        Some(FoldReason::PrimaryUnresponsive)
    } else if primary.closing {
        Some(FoldReason::PrimaryClosing)
    } else {
        None
    };

    let mut fallbacks: Vec<&FoldWindow<'_>> = by_opening
        .iter()
        .copied()
        .filter(|w| w.win != primary.win && !w.closing && !ctx.unresponsive.contains(&w.win))
        .collect();
    fallbacks.sort_by_key(|w| rank(ctx.activation, w.win));

    // The Matching_Tab (Req 14.20); not searched again after a failure.
    if ctx.exclude.is_none() {
        let searched = primary_usable.then_some(primary).into_iter().chain(fallbacks.iter().copied());
        for w in searched {
            if let Some(index) = matching_index(w, ctx) {
                let is_foreground_active = ctx.foreground == Some(w.win) && w.active == Some(index);
                return if is_foreground_active {
                    FoldPlan::Fold { target: w.win, reason: Some(FoldReason::Foreground) }
                } else {
                    FoldPlan::Switch { win: w.win, index }
                };
            }
        }
    }

    // The Fold_Target (Req 14.9, 14.13, 14.24).
    if primary_usable && ctx.exclude != Some(primary.win) {
        return FoldPlan::Fold { target: primary.win, reason: None };
    }
    match fallbacks.iter().find(|w| ctx.exclude != Some(w.win)) {
        Some(w) => FoldPlan::Fold { target: w.win, reason: primary_reason },
        None => FoldPlan::NoTarget,
    }
}

/// The Primary_Window [`select`] works with: the first window of `windows` on
/// the desktop in the Opening_Order (windows missing from it rank last).
fn primary_of(windows: &[FoldWindow<'_>], opening: &[WinKey], desktop: &DesktopView) -> Option<WinKey> {
    let on_desktop = |win: WinKey| match desktop {
        DesktopView::Known(set) => set.contains(&win),
        DesktopView::Unknown => true,
    };
    let rank = |win: WinKey| opening.iter().position(|&w| w == win).unwrap_or(usize::MAX);
    windows.iter().filter(|w| on_desktop(w.win)).min_by_key(|w| rank(w.win)).map(|w| w.win)
}

/// The index of the Matching_Tab in `w`: its Active_Tab if it matches, else the
/// leftmost matching tab, else the first matching pending Folded_Tab (counted
/// after the real tabs).
fn matching_index(w: &FoldWindow<'_>, ctx: &FoldCtx<'_>) -> Option<usize> {
    let matches = |tab: &Option<LocationKey<'_>>| tab.as_ref() == Some(&ctx.candidate_key);
    if let Some(active) = w.active.filter(|&a| w.tabs.get(a).is_some_and(matches)) {
        return Some(active);
    }
    if let Some(index) = w.tabs.iter().position(matches) {
        return Some(index);
    }
    ctx.pending_folds
        .iter()
        .filter(|(target, _)| *target == w.win)
        .position(|(_, key)| *key == ctx.candidate_key)
        .map(|n| w.tabs.len() + n)
}

// ---------------------------------------------------------------------------
// Outcomes (the `FOLD` log entry, Req 14.30)
// ---------------------------------------------------------------------------

/// Why a fold into a fallback window was taken (Req 14.13, 14.30).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FallbackReason {
    PrimaryUnresponsive,
    PrimaryClosing,
    /// The Primary_Window created no tab within [`FOLD_TAB_CREATE_MS`].
    TabNotCreated,
}

/// Which window a fold that followed a foreground exception or a failed
/// Tab_Switch finally went into (Req 14.24, 14.30).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FoldInto {
    /// The Foreground_Explorer_Window (`folded-foreground` only).
    Foreground,
    /// The window that held the Matching_Tab (`switch-failed-folded` only).
    MatchingWindow,
    /// The Primary_Window.
    Primary,
    /// A fallback window.
    Fallback,
}

/// Which window held the Matching_Tab of a Tab_Switch (Req 14.30).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MatchingWindow {
    Primary,
    Other,
}

/// Why a candidate was left open (Req 14.14, 14.30).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NotFoldedReason {
    NoFoldTarget,
    TabNotCreated,
    LocationNotShown,
    FoldTargetClosed,
    FirstNavTimeout,
    FoldingDisabled,
}

/// How a candidate's fold ended; the payload of `LogEntry::Fold` (task 20.2).
/// Each field has exactly one value (design.md, "Activity_Log").
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FoldOutcome {
    /// `folded-primary`.
    FoldedPrimary,
    /// `folded-fallback`.
    FoldedFallback { reason: FallbackReason },
    /// `folded-foreground`: `into` is [`FoldInto::Foreground`],
    /// [`FoldInto::Primary`] or [`FoldInto::Fallback`]; `passed` lists the
    /// windows passed over, in order (empty when none).
    FoldedForeground { into: FoldInto, passed: Vec<PassReason> },
    /// `switched`.
    Switched { matching: MatchingWindow },
    /// `switch-failed-folded`: `into` is [`FoldInto::MatchingWindow`],
    /// [`FoldInto::Primary`] or [`FoldInto::Fallback`].
    SwitchFailedFolded { switch_reason: SwitchFailure, into: FoldInto, passed: Vec<PassReason> },
    /// `not-folded`, with the `switch-reason` when a failed Tab_Switch led there.
    NotFolded { reason: NotFoldedReason, switch_reason: Option<SwitchFailure> },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_match_the_requirements() {
        assert_eq!(FOLD_FIRST_NAV_MS, 8_000);
        assert_eq!(FOLD_TAB_CREATE_MS, 1_000);
        assert_eq!(FOLD_TAB_SHOW_MS, 3_000);
        assert_eq!(FOLD_SWITCH_MS, 1_000);
        assert_eq!(UNRESPONSIVE_MS, 1_000);
        assert_eq!(DRAG_OUT_WINDOW_MS, (-100, 300));
        assert_eq!(LATE_TAB_MS, 10_000);
    }

    // ---- select ----

    use crate::model::KindTag;
    use std::borrow::Cow;

    fn w(n: u64) -> WinKey {
        WinKey { hwnd: n, seq: n }
    }

    fn key(n: u16) -> LocationKey<'static> {
        LocationKey::Plain { kind: KindTag::FileSystem, parsing_name: Cow::Owned(vec![n]) }
    }

    fn win(n: u64, tabs: &[Option<u16>], active: Option<usize>, closing: bool) -> FoldWindow<'static> {
        FoldWindow { win: w(n), tabs: tabs.iter().map(|t| t.map(key)).collect(), active, closing }
    }

    struct Fixture {
        opening: Vec<WinKey>,
        activation: Vec<WinKey>,
        windows: Vec<FoldWindow<'static>>,
        pending: Vec<(WinKey, LocationKey<'static>)>,
        desktop: DesktopView,
        unresponsive: BTreeSet<WinKey>,
        foreground: Option<WinKey>,
        exclude: Option<WinKey>,
    }

    impl Fixture {
        /// Windows 1..=n in both orders, each showing only location 0.
        fn new(windows: Vec<FoldWindow<'static>>) -> Fixture {
            let order: Vec<WinKey> = windows.iter().map(|x| x.win).collect();
            Fixture {
                opening: order.clone(),
                activation: order,
                windows,
                pending: Vec::new(),
                desktop: DesktopView::Unknown,
                unresponsive: BTreeSet::new(),
                foreground: None,
                exclude: None,
            }
        }

        fn select(&self, candidate: u16) -> FoldPlan {
            select(&FoldCtx {
                candidate_key: key(candidate),
                foreground: self.foreground,
                opening: &self.opening,
                activation: &self.activation,
                windows: &self.windows,
                pending_folds: &self.pending,
                desktop: &self.desktop,
                unresponsive: &self.unresponsive,
                exclude: self.exclude,
            })
        }
    }

    fn fold(n: u64, reason: Option<FoldReason>) -> FoldPlan {
        FoldPlan::Fold { target: w(n), reason }
    }

    #[test]
    fn select_without_windows_has_no_target() {
        assert_eq!(Fixture::new(vec![]).select(9), FoldPlan::NoTarget);
    }

    #[test]
    fn select_folds_into_the_primary_window() {
        let mut f = Fixture::new(vec![win(1, &[Some(0)], Some(0), false), win(2, &[Some(0)], Some(0), false)]);
        f.opening = vec![w(2), w(1)];
        assert_eq!(f.select(9), fold(2, None));
    }

    #[test]
    fn select_ignores_opening_entries_that_are_not_windows() {
        let mut f = Fixture::new(vec![win(2, &[Some(0)], Some(0), false)]);
        f.opening = vec![w(7), w(2)];
        assert_eq!(f.select(9), fold(2, None));
    }

    #[test]
    fn select_falls_back_in_activation_order() {
        let mut f = Fixture::new(vec![
            win(1, &[Some(0)], Some(0), false),
            win(2, &[Some(0)], Some(0), false),
            win(3, &[Some(0)], Some(0), false),
        ]);
        f.activation = vec![w(3), w(2), w(1)];
        f.unresponsive.insert(w(1));
        assert_eq!(f.select(9), fold(3, Some(FoldReason::PrimaryUnresponsive)));
        f.unresponsive.clear();
        f.windows[0].closing = true;
        assert_eq!(f.select(9), fold(3, Some(FoldReason::PrimaryClosing)));
        f.windows[2].closing = true;
        assert_eq!(f.select(9), fold(2, Some(FoldReason::PrimaryClosing)));
        f.windows[1].closing = true;
        assert_eq!(f.select(9), FoldPlan::NoTarget);
    }

    #[test]
    fn select_only_uses_windows_on_the_desktop() {
        let mut f = Fixture::new(vec![win(1, &[Some(5)], Some(0), false), win(2, &[Some(0)], Some(0), false)]);
        f.desktop = DesktopView::Known([w(2)].into_iter().collect());
        assert_eq!(f.select(5), fold(2, None));
        f.desktop = DesktopView::Known(BTreeSet::new());
        assert_eq!(f.select(5), FoldPlan::NoTarget);
    }

    #[test]
    fn select_prefers_the_active_tab_then_the_leftmost() {
        let f = Fixture::new(vec![win(1, &[Some(5), Some(0), Some(5)], Some(2), false)]);
        assert_eq!(f.select(5), FoldPlan::Switch { win: w(1), index: 2 });
        let f = Fixture::new(vec![win(1, &[Some(0), Some(5), Some(5)], Some(0), false)]);
        assert_eq!(f.select(5), FoldPlan::Switch { win: w(1), index: 1 });
    }

    #[test]
    fn select_searches_the_primary_before_the_fallbacks() {
        let mut f = Fixture::new(vec![win(1, &[Some(5)], Some(0), false), win(2, &[Some(5)], Some(0), false)]);
        f.activation = vec![w(2), w(1)];
        assert_eq!(f.select(5), FoldPlan::Switch { win: w(1), index: 0 });
        f.unresponsive.insert(w(1));
        assert_eq!(f.select(5), FoldPlan::Switch { win: w(2), index: 0 });
    }

    #[test]
    fn select_does_not_search_closing_windows_and_skips_unknown_locations() {
        let mut f = Fixture::new(vec![win(1, &[None, Some(5)], Some(0), true), win(2, &[None], Some(0), false)]);
        assert_eq!(f.select(5), fold(2, Some(FoldReason::PrimaryClosing)));
        f.windows[0].closing = false;
        assert_eq!(f.select(5), FoldPlan::Switch { win: w(1), index: 1 });
    }

    #[test]
    fn select_counts_pending_folded_tabs_at_the_right_end() {
        let mut f = Fixture::new(vec![win(1, &[Some(0), Some(1)], Some(0), false)]);
        f.pending = vec![(w(1), key(4)), (w(1), key(5))];
        assert_eq!(f.select(5), FoldPlan::Switch { win: w(1), index: 3 });
        // A real tab is to the left of any pending one.
        f.windows[0].tabs.push(Some(key(5)));
        assert_eq!(f.select(5), FoldPlan::Switch { win: w(1), index: 2 });
        // A pending tab of a window that is not tracked counts for nothing.
        f.pending = vec![(w(9), key(7))];
        assert_eq!(f.select(7), fold(1, None));
    }

    #[test]
    fn select_applies_the_foreground_exception_to_the_active_tab_only() {
        let mut f = Fixture::new(vec![win(1, &[Some(0)], Some(0), false), win(2, &[Some(5), Some(0)], Some(0), false)]);
        f.foreground = Some(w(2));
        assert_eq!(f.select(5), fold(2, Some(FoldReason::Foreground)));
        // The Matching_Tab is not the foreground window's Active_Tab.
        f.windows[1].active = Some(1);
        assert_eq!(f.select(5), FoldPlan::Switch { win: w(2), index: 0 });
        // Another window holds the Matching_Tab: Tab_Switch.
        f.windows[0].tabs = vec![Some(key(5))];
        assert_eq!(f.select(5), FoldPlan::Switch { win: w(1), index: 0 });
        f.windows[1].active = Some(0);
        assert_eq!(f.select(5), FoldPlan::Switch { win: w(1), index: 0 });
    }

    #[test]
    fn select_with_exclude_folds_elsewhere_without_a_new_search() {
        let mut f = Fixture::new(vec![
            win(1, &[Some(5)], Some(0), false),
            win(2, &[Some(5)], Some(0), false),
            win(3, &[Some(0)], Some(0), false),
        ]);
        f.exclude = Some(w(1));
        assert_eq!(f.select(5), fold(2, None));
        f.exclude = Some(w(2));
        assert_eq!(f.select(5), fold(1, None));
        f.exclude = Some(w(1));
        f.unresponsive.insert(w(2));
        f.unresponsive.insert(w(3));
        assert_eq!(f.select(5), FoldPlan::NoTarget);
    }

    #[test]
    fn select_with_exclude_skips_the_excluded_fallback() {
        let mut f = Fixture::new(vec![win(1, &[Some(0)], Some(0), false), win(2, &[Some(0)], Some(0), false), win(3, &[Some(0)], Some(0), false)]);
        f.unresponsive.insert(w(1));
        f.exclude = Some(w(2));
        assert_eq!(f.select(5), fold(3, Some(FoldReason::PrimaryUnresponsive)));
    }

    // ---- first signal ----

    use crate::engine::{ObservedLocation, TabObservation};
    use crate::model::{DisplayRef, Placement, Rect, ShowState};

    fn env(tracked: &[WinKey]) -> FirstSignalEnv<'_> {
        FirstSignalEnv { tracked, restoration_active: false, builtin_restore: false }
    }

    fn appear(e: &mut FoldingEngine, at: u64, n: u64, bypass: bool, tracked: &[WinKey]) -> FirstSignalOutcome {
        e.window_appeared(Millis(at), w(n), bypass, DesktopView::Unknown, Some(w(1)), &env(tracked))
    }

    fn excluded(reason: ExclusionReason) -> FirstSignalVerdict {
        FirstSignalVerdict::Excluded(reason)
    }

    fn scan(n: u64, tabs: u64) -> WindowObservation {
        WindowObservation {
            win: w(n),
            pid: 1,
            tabs: (0..tabs)
                .map(|t| TabObservation { tab: TabKey { hwnd: t, seq: t }, location: ObservedLocation::NotNavigated })
                .collect(),
            active: None,
            strip: None,
            placement: Placement {
                state: ShowState::Normal,
                restored: Rect::default(),
                display: DisplayRef { device: String::new(), monitor: Rect::default() },
            },
        }
    }

    #[test]
    fn a_new_window_with_a_tracked_window_is_hidden_with_a_timer() {
        let mut e = FoldingEngine::new(true);
        let out = appear(&mut e, 1_000, 2, false, &[w(1)]);
        assert_eq!(out.verdict, FirstSignalVerdict::Candidate);
        assert_eq!(out.commands.len(), 2);
        assert_eq!(out.commands[0], Command::HideWindow { win: w(2) });
        match &out.commands[1] {
            Command::SetTimer { id, at } => {
                assert_eq!(id.owner, TimerOwner::Fold);
                assert_eq!(*at, Millis(1_000 + FOLD_FIRST_NAV_MS));
            }
            other => panic!("expected a timer, got {other:?}"),
        }
        assert!(e.owns(w(2)));
        assert_eq!(e.candidates_in_progress(), 1);
        // Reserved: not visible in the Opening_Order while owned (Req 2.6, 14.27).
        assert!(!e.opening().contains(w(2)));
        let cand = &e.candidates[&w(2)];
        assert_eq!(cand.phase, Phase::Hidden);
        assert_eq!(cand.foreground, Some(w(1)));
    }

    #[test]
    fn exclusions_follow_the_design_order() {
        let tracked = [w(1)];
        let all = FirstSignalEnv { tracked: &tracked, restoration_active: true, builtin_restore: true };
        let sig = |e: &mut FoldingEngine, n, env: &FirstSignalEnv<'_>| {
            e.window_appeared(Millis(0), w(n), true, DesktopView::Unknown, None, env).verdict
        };
        let mut e = FoldingEngine::new(false);
        assert_eq!(sig(&mut e, 2, &all), excluded(ExclusionReason::FoldingDisabled));
        let mut e = FoldingEngine::new(true);
        let none = FirstSignalEnv { tracked: &[], ..all };
        assert_eq!(sig(&mut e, 2, &none), excluded(ExclusionReason::NoWindowOnDesktop));
        assert_eq!(sig(&mut e, 3, &all), excluded(ExclusionReason::RestorationActive));
        let no_restore = FirstSignalEnv { restoration_active: false, ..all };
        assert_eq!(sig(&mut e, 4, &no_restore), excluded(ExclusionReason::BuiltinRestore));
        let plain = FirstSignalEnv { builtin_restore: false, restoration_active: false, ..all };
        assert_eq!(sig(&mut e, 5, &plain), excluded(ExclusionReason::BypassKeys));
        assert_eq!(e.candidates_in_progress(), 0);
    }

    #[test]
    fn an_excluded_window_is_never_a_candidate_later() {
        let mut e = FoldingEngine::new(true);
        let out = appear(&mut e, 0, 2, true, &[w(1)]);
        assert_eq!(out.verdict, excluded(ExclusionReason::BypassKeys));
        assert!(out.commands.is_empty());
        assert!(e.is_excluded(w(2)));
        assert!(e.opening().contains(w(2)));
        // A repeated signal changes nothing, and a scan is not ours.
        assert_eq!(appear(&mut e, 5, 2, false, &[w(1)]).verdict, FirstSignalVerdict::AlreadyKnown);
        assert!(e.window_scanned(&scan(2, 1)).is_none());
    }

    #[test]
    fn only_tracked_windows_on_the_desktop_count() {
        let mut e = FoldingEngine::new(true);
        let other_desktop = DesktopView::Known([w(9)].into_iter().collect());
        let out = e.window_appeared(Millis(0), w(2), false, other_desktop, None, &env(&[w(1)]));
        assert_eq!(out.verdict, excluded(ExclusionReason::NoWindowOnDesktop));
        let same = DesktopView::Known([w(1), w(3)].into_iter().collect());
        let out = e.window_appeared(Millis(0), w(3), false, same, None, &env(&[w(1)]));
        assert_eq!(out.verdict, FirstSignalVerdict::Candidate);
        // A candidate is not a window that counts, even if the caller lists it.
        let out = e.window_appeared(Millis(1), w(4), false, DesktopView::Unknown, None, &env(&[w(3)]));
        assert_eq!(out.verdict, excluded(ExclusionReason::NoWindowOnDesktop));
    }

    #[test]
    fn the_foreground_must_be_tracked_and_not_a_candidate() {
        let mut e = FoldingEngine::new(true);
        e.window_appeared(Millis(0), w(2), false, DesktopView::Unknown, Some(w(7)), &env(&[w(1)]));
        assert_eq!(e.candidates[&w(2)].foreground, None);
        e.window_appeared(Millis(1), w(3), false, DesktopView::Unknown, Some(w(2)), &env(&[w(1), w(2)]));
        assert_eq!(e.candidates[&w(3)].foreground, None);
    }

    #[test]
    fn candidates_get_increasing_sequence_numbers() {
        let mut e = FoldingEngine::new(true);
        appear(&mut e, 0, 2, false, &[w(1)]);
        appear(&mut e, 1, 3, false, &[w(1)]);
        assert!(e.candidates[&w(2)].seq < e.candidates[&w(3)].seq);
    }

    #[test]
    fn a_tab_detached_just_after_the_first_signal_is_a_drag_out() {
        let mut e = FoldingEngine::new(true);
        appear(&mut e, 1_000, 2, false, &[w(1)]);
        e.window_scanned(&scan(2, 1));
        let step = e.tab_detached(Millis(1_020), w(1));
        assert_eq!(step.commands, vec![Command::Reveal { win: w(2) }]);
        assert_eq!(step.released.len(), 1);
        assert_eq!(step.released[0].win, w(2));
        assert_eq!(step.released[0].buffered, vec![Event::WindowScanned(scan(2, 1))]);
        assert!(!e.owns(w(2)));
        assert!(e.is_excluded(w(2)));
        assert!(e.opening().contains(w(2)));
        assert_eq!(e.candidates_in_progress(), 0);
    }

    #[test]
    fn a_tab_detached_too_late_is_not_a_drag_out() {
        let mut e = FoldingEngine::new(true);
        appear(&mut e, 1_000, 2, false, &[w(1)]);
        let step = e.tab_detached(Millis(1_301), w(1));
        assert_eq!(step, FoldStep::default());
        assert!(e.owns(w(2)));
        // At exactly 300 ms it still counts.
        let mut e = FoldingEngine::new(true);
        appear(&mut e, 1_000, 2, false, &[w(1)]);
        assert_eq!(e.tab_detached(Millis(1_300), w(1)).commands, vec![Command::Reveal { win: w(2) }]);
    }

    #[test]
    fn a_tab_detached_up_to_100_ms_before_the_first_signal_is_a_drag_out() {
        let mut e = FoldingEngine::new(true);
        assert_eq!(e.tab_detached(Millis(1_000), w(1)), FoldStep::default());
        let out = appear(&mut e, 1_100, 2, false, &[w(1)]);
        assert_eq!(out.verdict, excluded(ExclusionReason::DragOut));
        assert!(out.commands.is_empty());
        assert!(e.opening().contains(w(2)));
        // The detach was used up: the next window is a candidate.
        assert_eq!(appear(&mut e, 1_105, 3, false, &[w(1)]).verdict, FirstSignalVerdict::Candidate);

        let mut e = FoldingEngine::new(true);
        e.tab_detached(Millis(1_000), w(1));
        assert_eq!(appear(&mut e, 1_101, 2, false, &[w(1)]).verdict, FirstSignalVerdict::Candidate);
    }

    #[test]
    fn a_detach_marks_the_latest_hidden_candidate_only() {
        let mut e = FoldingEngine::new(true);
        appear(&mut e, 1_000, 2, false, &[w(1)]);
        appear(&mut e, 1_010, 3, false, &[w(1)]);
        let step = e.tab_detached(Millis(1_020), w(1));
        assert_eq!(step.commands, vec![Command::Reveal { win: w(3) }]);
        assert!(e.owns(w(2)));
        assert_eq!(e.tab_detached(Millis(1_021), w(1)).commands, vec![Command::Reveal { win: w(2) }]);
    }

    #[test]
    fn a_first_scan_with_several_tabs_excludes_the_window() {
        let mut e = FoldingEngine::new(true);
        appear(&mut e, 0, 2, false, &[w(1)]);
        let step = e.window_scanned(&scan(2, 2)).unwrap();
        assert_eq!(step.commands, vec![Command::Reveal { win: w(2) }]);
        assert_eq!(step.released[0].buffered, vec![Event::WindowScanned(scan(2, 2))]);
        assert!(e.is_excluded(w(2)));
        assert!(e.opening().contains(w(2)));
    }

    #[test]
    fn only_the_latest_activation_of_a_candidate_is_buffered_after_its_scan() {
        let mut e = FoldingEngine::new(true);
        appear(&mut e, 0, 2, false, &[w(1)]);
        // An activation before the first scan, and a repeated one.
        e.window_activated(w(2));
        e.window_scanned(&scan(2, 1));
        e.window_activated(w(2));
        // A window that is not a candidate is ignored.
        e.window_activated(w(7));
        assert_eq!(
            e.candidates[&w(2)].buffered,
            vec![Event::WindowScanned(scan(2, 1)), Event::WindowActivated { win: w(2) }]
        );
    }

    #[test]
    fn windows_activated_after_a_candidate_are_handed_over_behind_its_activation() {
        let mut e = FoldingEngine::new(true);
        appear(&mut e, 0, 2, false, &[w(1)]);
        // Not activated yet: the other window is of no concern to it.
        e.window_activated(w(1));
        e.window_activated(w(2));
        e.window_activated(w(1));
        e.window_activated(w(7));
        e.window_activated(w(1));
        // Excluded by a first scan of several tabs: released with everything.
        let step = e.window_scanned(&scan(2, 3)).unwrap();
        assert_eq!(
            step.released[0].buffered,
            vec![
                Event::WindowScanned(scan(2, 3)),
                Event::WindowActivated { win: w(2) },
                Event::WindowActivated { win: w(7) },
                Event::WindowActivated { win: w(1) },
            ]
        );
    }

    #[test]
    fn a_single_tab_scan_is_buffered_and_keeps_the_candidate() {
        let mut e = FoldingEngine::new(true);
        appear(&mut e, 0, 2, false, &[w(1)]);
        assert_eq!(e.window_scanned(&scan(2, 1)), Some(FoldStep::default()));
        // Only the first scan decides.
        assert_eq!(e.window_scanned(&scan(2, 3)), Some(FoldStep::default()));
        assert!(e.owns(w(2)));
        assert_eq!(e.candidates[&w(2)].buffered, vec![Event::WindowScanned(scan(2, 3))]);
    }

    // ---- fold context ----

    use crate::model::{LocationKind, Volume};

    fn location(n: u16) -> LocationId {
        let name = format!("{n}");
        LocationId {
            case_sensitive: false,
            kind: LocationKind::FileSystem(Volume::Fixed),
            parsing_name: name.as_str().into(),
            display_name: name.as_str().into(),
            pidl: vec![],
            search: None,
        }
    }

    /// A window whose tabs show the numbered Locations; `key(n)` of
    /// `location(n)` is only equal for `n` made of one UTF-16 unit, so the
    /// helpers build windows from `location(..).fold_key()`.
    fn fwin(n: u64, tabs: &[u16]) -> FoldWindow<'static> {
        FoldWindow {
            win: w(n),
            tabs: tabs.iter().map(|&t| Some(location_key(t))).collect(),
            active: if tabs.is_empty() { None } else { Some(0) },
            closing: false,
        }
    }

    fn location_key(n: u16) -> LocationKey<'static> {
        match location(n).fold_key() {
            LocationKey::Plain { kind, parsing_name } => {
                LocationKey::Plain { kind, parsing_name: std::borrow::Cow::Owned(parsing_name.into_owned()) }
            }
            LocationKey::Search { .. } => unreachable!("test locations are plain"),
        }
    }

    fn probe_of(cmds: &[Command]) -> FoldProbeId {
        match cmds {
            [Command::ProbeFoldContext { probe, .. }] => *probe,
            other => panic!("expected one probe, got {other:?}"),
        }
    }

    /// Time of the fold context answer in the tests.
    const CTX_AT: u64 = 1_000;

    fn answer(e: &mut FoldingEngine, probe: FoldProbeId, windows: &[FoldWindow<'_>]) -> FoldStep {
        answer_at(e, CTX_AT, probe, windows)
    }

    /// [`answer`] at time `at`, for the later steps of a failure chain.
    fn answer_at(e: &mut FoldingEngine, at: u64, probe: FoldProbeId, windows: &[FoldWindow<'_>]) -> FoldStep {
        e.fold_context(Millis(at), probe, DesktopView::Unknown, &BTreeSet::new(), windows, &real_tab)
    }

    /// The tab the tracker has at `index` of `win` in these tests.
    fn real_tab(win: WinKey, index: usize) -> Option<TabKey> {
        let n = 1_000 + win.hwnd * 10 + index as u64;
        Some(TabKey { hwnd: n, seq: n })
    }

    /// The `ProbeFoldContext` among other commands.
    fn probe_in(cmds: &[Command]) -> FoldProbeId {
        let probes: Vec<FoldProbeId> = cmds
            .iter()
            .filter_map(|c| match c {
                Command::ProbeFoldContext { probe, .. } => Some(*probe),
                _ => None,
            })
            .collect();
        match probes.as_slice() {
            [probe] => *probe,
            other => panic!("expected one probe in {cmds:?}, got {other:?}"),
        }
    }

    #[test]
    fn a_known_location_starts_the_probe_and_keeps_the_selection() {
        let mut e = FoldingEngine::new(true);
        appear(&mut e, 0, 2, false, &[w(1)]);
        assert!(e.initial_location_known(w(9), location(5), vec![]).is_none());
        let items = vec![ItemId(vec![1, 2])];
        let step = e.initial_location_known(w(2), location(5), items.clone()).unwrap();
        assert!(matches!(step.commands.as_slice(), [Command::ProbeFoldContext { candidate, .. }] if *candidate == w(2)));
        let c = &e.candidates[&w(2)];
        assert_eq!((c.phase, c.selection.clone()), (Phase::Context, items));
        // A repeated announcement does not probe again.
        let again = e.initial_location_known(w(2), location(6), vec![]).unwrap();
        assert!(again.commands.is_empty());
        assert_eq!(e.candidates[&w(2)].location, Some(location(5)));
    }

    #[test]
    fn a_later_candidate_waits_for_every_earlier_one() {
        let mut e = FoldingEngine::new(true);
        appear(&mut e, 0, 2, false, &[w(1)]);
        appear(&mut e, 1, 3, false, &[w(1)]);
        // The later candidate's Location is known first: it waits.
        let step = e.initial_location_known(w(3), location(7), vec![]).unwrap();
        assert!(step.commands.is_empty());
        assert_eq!(e.candidates[&w(3)].phase, Phase::Hidden);
        // The earlier one is probed, and still holds the later one back.
        let first = probe_of(&e.initial_location_known(w(2), location(6), vec![]).unwrap().commands);
        assert_eq!(e.candidates[&w(3)].phase, Phase::Hidden);
        // Once the earlier one has chosen, the later one is probed.
        let step = answer(&mut e, first, &[fwin(1, &[0])]);
        assert_eq!(e.candidates[&w(2)].phase, Phase::Creating);
        assert!(matches!(
            step.commands.as_slice(),
            [Command::CreateTab { .. }, Command::SetTimer { .. }, Command::ProbeFoldContext { candidate, .. }]
                if *candidate == w(3)
        ));
        assert_eq!(e.candidates[&w(3)].phase, Phase::Context);
    }

    #[test]
    fn a_released_earlier_candidate_no_longer_holds_the_later_one_back() {
        let mut e = FoldingEngine::new(true);
        appear(&mut e, 0, 2, false, &[w(1)]);
        appear(&mut e, 1, 3, false, &[w(1)]);
        e.initial_location_known(w(3), location(7), vec![]);
        // A multi-tab first scan excludes candidate 2.
        let step = e.window_scanned(&scan(2, 2)).unwrap();
        assert_eq!(step.commands.len(), 2);
        assert_eq!(step.commands[0], Command::Reveal { win: w(2) });
        assert!(matches!(&step.commands[1], Command::ProbeFoldContext { candidate, .. } if *candidate == w(3)));
    }

    #[test]
    fn a_fold_plan_creates_in_the_primary_and_a_switch_plan_switches() {
        let mut e = FoldingEngine::new(true);
        e.opening_mut().append(w(1));
        appear(&mut e, 0, 2, false, &[w(1)]);
        let p = probe_of(&e.initial_location_known(w(2), location(5), vec![]).unwrap().commands);
        let step = answer(&mut e, p, &[fwin(1, &[0])]);
        // The Folded_Tab is requested at once, with the 1 s limit of Req 14.13.
        let op = create_op_of(&step.commands);
        assert_eq!(step.commands[0], Command::CreateTab { op, win: w(1) });
        assert_eq!(
            step.commands[1],
            Command::SetTimer { id: fold_timer(TIMER_TAB_CREATE, 1), at: Millis(CTX_AT + FOLD_TAB_CREATE_MS) }
        );
        assert!(step.released.is_empty());
        let c = &e.candidates[&w(2)];
        assert_eq!((c.phase, c.target), (Phase::Creating, Some(w(1))));
        assert_eq!(c.plan, Some(FoldPlan::Fold { target: w(1), reason: None }));

        let mut e = FoldingEngine::new(true);
        e.opening_mut().append(w(1));
        appear(&mut e, 0, 2, false, &[w(1)]);
        let p = probe_of(&e.initial_location_known(w(2), location(5), vec![]).unwrap().commands);
        answer(&mut e, p, &[fwin(1, &[0, 5])]);
        let c = &e.candidates[&w(2)];
        assert_eq!((c.phase, c.target), (Phase::Switching, Some(w(1))));
        assert_eq!(c.plan, Some(FoldPlan::Switch { win: w(1), index: 1 }));
    }

    #[test]
    fn the_context_decides_with_its_desktop_and_unresponsive_windows() {
        let mut e = FoldingEngine::new(true);
        e.opening_mut().append(w(1));
        e.opening_mut().append(w(3));
        appear(&mut e, 0, 2, false, &[w(1), w(3)]);
        let p = probe_of(&e.initial_location_known(w(2), location(5), vec![]).unwrap().commands);
        let windows = [fwin(1, &[0]), fwin(3, &[0])];
        let unresponsive = BTreeSet::from([w(1)]);
        e.fold_context(Millis(CTX_AT), p, DesktopView::Known(BTreeSet::from([w(1), w(3)])), &unresponsive, &windows, &real_tab);
        let c = &e.candidates[&w(2)];
        assert_eq!(c.plan, Some(FoldPlan::Fold { target: w(3), reason: Some(FoldReason::PrimaryUnresponsive) }));
        assert_eq!(c.desktop, DesktopView::Known(BTreeSet::from([w(1), w(3)])));
    }

    #[test]
    fn an_earlier_candidates_folded_tab_is_a_matching_tab_for_a_later_one() {
        let mut e = FoldingEngine::new(true);
        e.opening_mut().append(w(1));
        appear(&mut e, 0, 2, false, &[w(1)]);
        appear(&mut e, 1, 3, false, &[w(1)]);
        let windows = [fwin(1, &[0])];
        e.initial_location_known(w(3), location(5), vec![]);
        let p = probe_of(&e.initial_location_known(w(2), location(5), vec![]).unwrap().commands);
        let step = answer(&mut e, p, &windows);
        assert_eq!(e.candidates[&w(2)].plan, Some(FoldPlan::Fold { target: w(1), reason: None }));
        // Candidate 3 shows the same Location as candidate 2's Folded_Tab: it
        // switches to that tab, which lies after the window's one real tab.
        let p = probe_in(&step.commands);
        let step = answer(&mut e, p, &windows);
        assert_eq!(e.candidates[&w(3)].plan, Some(FoldPlan::Switch { win: w(1), index: 1 }));
        // The switch waits for candidate 2's tab to be shown and active.
        assert_eq!(e.candidates[&w(3)].waits_on, Some(w(2)));
        assert_eq!(e.switch_waits_on(w(3)), Some(w(2)));
        assert!(step.commands.is_empty());
    }

    #[test]
    fn no_fold_target_leaves_the_candidate_open() {
        let mut e = FoldingEngine::new(true);
        appear(&mut e, 0, 2, false, &[w(1)]);
        e.window_scanned(&scan(2, 1));
        let p = probe_of(&e.initial_location_known(w(2), location(5), vec![]).unwrap().commands);
        let step = answer(&mut e, p, &[]);
        assert_eq!(
            step.commands,
            vec![
                Command::Reveal { win: w(2) },
                Command::AppendLog(LogEntry::Fold {
                    location: location(5),
                    outcome: FoldOutcome::NotFolded { reason: NotFoldedReason::NoFoldTarget, switch_reason: None },
                }),
            ]
        );
        assert_eq!(step.released.len(), 1);
        // The tracker gets the last scan, then the Location (Req 14.15).
        assert_eq!(
            step.released[0].buffered,
            vec![
                Event::WindowScanned(scan(2, 1)),
                Event::InitialLocationKnown { win: w(2), tab: None, location: location(5), selection: vec![] },
            ]
        );
        assert!(!e.owns(w(2)));
        assert!(e.opening().contains(w(2)));
    }

    #[test]
    fn an_unknown_probe_is_ignored() {
        let mut e = FoldingEngine::new(true);
        appear(&mut e, 0, 2, false, &[w(1)]);
        e.initial_location_known(w(2), location(5), vec![]);
        assert_eq!(answer(&mut e, FoldProbeId(99), &[fwin(1, &[0])]), FoldStep::default());
        assert_eq!(e.candidates[&w(2)].phase, Phase::Context);
    }

    // ---- folding into a Folded_Tab ----

    fn tk(n: u64) -> TabKey {
        TabKey { hwnd: n, seq: n }
    }

    fn create_op_of(cmds: &[Command]) -> OpId {
        cmds.iter()
            .find_map(|c| match c {
                Command::CreateTab { op, .. } => Some(*op),
                _ => None,
            })
            .unwrap_or_else(|| panic!("expected a CreateTab in {cmds:?}"))
    }

    fn activate_op_of(cmds: &[Command]) -> OpId {
        cmds.iter()
            .find_map(|c| match c {
                Command::ActivateTab { op, .. } => Some(*op),
                _ => None,
            })
            .unwrap_or_else(|| panic!("expected an ActivateTab in {cmds:?}"))
    }

    fn not_minimized(_: WinKey) -> Option<FinalPlacement> {
        None
    }

    fn minimized_placement() -> FinalPlacement {
        FinalPlacement {
            state: ShowState::Maximized,
            restored: Rect { left: 10, top: 20, right: 300, bottom: 400 },
            monitor: Rect::default(),
            work: Rect::default(),
        }
    }

    /// Candidate window `n` appears at `at`, announces Location `loc` with
    /// `items`, and its fold context is answered with `windows`.
    fn start_fold(
        e: &mut FoldingEngine,
        at: u64,
        n: u64,
        loc: u16,
        items: Vec<ItemId>,
        windows: &[FoldWindow<'_>],
    ) -> FoldStep {
        appear(e, at, n, false, &[w(1)]);
        let p = probe_of(&e.initial_location_known(w(n), location(loc), items).unwrap().commands);
        answer(e, p, windows)
    }

    /// Drives a started fold to its end: tab `tab` appears and registers in
    /// the target, shows the Location, and the activation is confirmed;
    /// returns the step of the candidate's `WindowDestroyed`.
    fn complete_fold(e: &mut FoldingEngine, create: OpId, cand: u64, tab: u64, loc: u16) -> FoldStep {
        e.tab_started(Millis(1_040), create, tk(tab));
        let target = e.candidates[&w(cand)].target.unwrap();
        e.tab_created(Millis(1_300), target, tk(tab));
        let step = e.navigation_completed(Millis(1_400), tk(tab), &location(loc), &not_minimized);
        e.activation_confirmed(Millis(1_450), activate_op_of(&step.commands), &not_minimized);
        e.window_destroyed(Millis(5_000), w(cand)).expect("a candidate")
    }

    /// The retry timer that comes with CloseWindow of candidate seq.
    fn close_timer(seq: u64, at: u64) -> Command {
        Command::SetTimer { id: fold_timer(TIMER_CLOSE, seq), at: Millis(at + FOLD_CLOSE_RETRY_MS) }
    }

    fn folded(loc: u16, outcome: FoldOutcome) -> Command {
        Command::AppendLog(LogEntry::Fold { location: location(loc), outcome })
    }

    #[test]
    fn a_candidate_is_folded_into_the_primary_window_step_by_step() {
        let mut e = FoldingEngine::new(true);
        e.opening_mut().append(w(1));
        let items = vec![ItemId(vec![7])];
        let step = start_fold(&mut e, 0, 2, 5, items.clone(), &[fwin(1, &[0, 1])]);
        let create = create_op_of(&step.commands);
        assert_eq!(e.candidates[&w(2)].slot, 2);

        // The tab window appears: the Folded_Tab counts as created and the
        // 3 s limit for showing the Location starts (Req 14.14).
        let step = e.tab_started(Millis(1_040), create, tk(10));
        assert_eq!(
            step.commands,
            vec![Command::SetTimer { id: fold_timer(TIMER_TAB_SHOW, 1), at: Millis(1_040 + FOLD_TAB_SHOW_MS) }]
        );
        assert_eq!(e.folded_tab(w(2)), Some(tk(10)));

        // The tab registers: navigate it to the candidate's Location.
        let step = e.tab_created(Millis(1_300), w(1), tk(10));
        let [Command::Navigate { tab, location: shown, .. }] = step.commands.as_slice() else {
            panic!("expected one Navigate, got {:?}", step.commands);
        };
        assert_eq!((*tab, shown), (tk(10), &location(5)));

        // The new tab's own navigation to the start location is not the Location.
        let step = e.navigation_completed(Millis(1_310), tk(10), &location(0), &not_minimized);
        assert_eq!(step, FoldStep::default());
        assert_eq!(e.candidates[&w(2)].phase, Phase::Creating);

        // The Location is shown: select the items, activate the rightmost tab.
        let step = e.navigation_completed(Millis(1_400), tk(10), &location(5), &not_minimized);
        let [
            Command::SelectItems { tab, items: got, .. },
            Command::ActivateTab { op: activate, win, index },
            Command::SetTimer { id, at },
        ] = step.commands.as_slice()
        else {
            panic!("expected select, activate and a timer, got {:?}", step.commands);
        };
        assert_eq!((*tab, got, *win, *index), (tk(10), &items, w(1), 2));
        assert_eq!((*id, *at), (fold_timer(TIMER_ACTIVATE, 1), Millis(1_400 + FOLD_ACTIVATE_MS)));
        assert_eq!(e.candidates[&w(2)].phase, Phase::Finishing);

        // An unrelated confirmation changes nothing; ours closes the candidate.
        assert_eq!(e.activation_confirmed(Millis(1_410), OpId(5), &not_minimized), FoldStep::default());
        let step = e.activation_confirmed(Millis(1_450), *activate, &not_minimized);
        assert_eq!(step.commands, vec![Command::Focus(w(1)), Command::CloseWindow { win: w(2) }, close_timer(1, 1_450)]);
        assert_eq!(e.candidates[&w(2)].phase, Phase::Closing);
        assert!(e.owns(w(2)));

        // Its destruction completes the fold (Req 14.26).
        let step = e.window_destroyed(Millis(5_000), w(2)).unwrap();
        assert_eq!(step.commands, vec![folded(5, FoldOutcome::FoldedPrimary), Command::RescanWindow(w(1))]);
        assert!(!e.owns(w(2)));
        assert!(!e.opening().contains(w(2)));
        assert!(e.target_queues.is_empty());
        assert_eq!(e.candidates_in_progress(), 0);
        assert!(e.window_destroyed(Millis(5_000), w(2)).is_none());
    }

    #[test]
    fn a_candidate_without_a_selection_selects_nothing() {
        let mut e = FoldingEngine::new(true);
        e.opening_mut().append(w(1));
        let create = create_op_of(&start_fold(&mut e, 0, 2, 5, vec![], &[fwin(1, &[0])]).commands);
        e.tab_started(Millis(1_040), create, tk(10));
        e.tab_created(Millis(1_300), w(1), tk(10));
        let step = e.navigation_completed(Millis(1_400), tk(10), &location(5), &not_minimized);
        assert!(matches!(step.commands.as_slice(), [Command::ActivateTab { index: 1, .. }, Command::SetTimer { .. }]));
    }

    #[test]
    fn a_minimized_target_is_shown_before_the_focus_moves() {
        let mut e = FoldingEngine::new(true);
        e.opening_mut().append(w(1));
        let create = create_op_of(&start_fold(&mut e, 0, 2, 5, vec![], &[fwin(1, &[0])]).commands);
        e.tab_started(Millis(1_040), create, tk(10));
        e.tab_created(Millis(1_300), w(1), tk(10));
        let minimized = |win: WinKey| (win == w(1)).then(minimized_placement);
        let step = e.navigation_completed(Millis(1_400), tk(10), &location(5), &minimized);
        let step = e.activation_confirmed(Millis(1_450), activate_op_of(&step.commands), &minimized);
        assert_eq!(
            step.commands,
            vec![
                Command::ApplyPlacement { win: w(1), placement: minimized_placement() },
                Command::Focus(w(1)),
                Command::CloseWindow { win: w(2) },
                close_timer(1, 1_450),
            ]
        );
    }

    #[test]
    fn a_tab_that_registers_without_a_tab_started_is_adopted() {
        let mut e = FoldingEngine::new(true);
        e.opening_mut().append(w(1));
        let create = create_op_of(&start_fold(&mut e, 0, 2, 5, vec![], &[fwin(1, &[0])]).commands);
        // A tab the user opened in another window, or one the candidate does
        // not wait for, is not its Folded_Tab.
        assert_eq!(e.tab_created(Millis(1_100), w(9), tk(99)), FoldStep::default());
        let step = e.tab_created(Millis(1_300), w(1), tk(10));
        assert!(matches!(
            step.commands.as_slice(),
            [Command::SetTimer { at, .. }, Command::Navigate { tab, .. }]
                if *at == Millis(1_300 + FOLD_TAB_SHOW_MS) && *tab == tk(10)
        ));
        assert_eq!(e.candidates[&w(2)].created_tabs, vec![tk(10)]);
        // The late TabStarted of the same op names a tab it already has.
        assert_eq!(e.tab_started(Millis(1_310), create, tk(10)), FoldStep::default());
    }

    #[test]
    fn a_foreign_tab_is_not_the_folded_tab_once_tab_started_named_it() {
        let mut e = FoldingEngine::new(true);
        e.opening_mut().append(w(1));
        let create = create_op_of(&start_fold(&mut e, 0, 2, 5, vec![], &[fwin(1, &[0])]).commands);
        e.tab_started(Millis(1_040), create, tk(10));
        assert_eq!(e.tab_created(Millis(1_100), w(1), tk(99)), FoldStep::default());
        assert!(!e.candidates[&w(2)].registered);
        // A completion for a tab the candidate does not own is ignored too.
        assert_eq!(e.navigation_completed(Millis(1_200), tk(99), &location(5), &not_minimized), FoldStep::default());
        assert!(!e.tab_created(Millis(1_300), w(1), tk(10)).commands.is_empty());
    }

    #[test]
    fn candidates_of_one_target_create_one_tab_at_a_time_and_activate_in_order() {
        let mut e = FoldingEngine::new(true);
        e.opening_mut().append(w(1));
        let windows = [fwin(1, &[0])];
        appear(&mut e, 0, 2, false, &[w(1)]);
        appear(&mut e, 1, 3, false, &[w(1)]);
        e.initial_location_known(w(3), location(6), vec![]);
        let p2 = probe_of(&e.initial_location_known(w(2), location(5), vec![]).unwrap().commands);
        let step = answer(&mut e, p2, &windows);
        let create2 = create_op_of(&step.commands);
        let p3 = probe_in(&step.commands);
        // Candidate 3 is behind candidate 2's tab, which is not registered yet.
        let step = answer(&mut e, p3, &windows);
        assert!(step.commands.is_empty(), "{:?}", step.commands);
        assert_eq!((e.candidates[&w(2)].slot, e.candidates[&w(3)].slot), (1, 2));

        // The first tab registers: the second may be requested now.
        e.tab_started(Millis(1_040), create2, tk(10));
        let step = e.tab_created(Millis(1_300), w(1), tk(10));
        assert!(matches!(step.commands[0], Command::Navigate { .. }));
        let create3 = create_op_of(&step.commands);
        assert_eq!(
            step.commands[2],
            Command::SetTimer { id: fold_timer(TIMER_TAB_CREATE, 2), at: Millis(1_300 + FOLD_TAB_CREATE_MS) }
        );
        e.tab_started(Millis(1_340), create3, tk(11));
        e.tab_created(Millis(1_600), w(1), tk(11));

        // Candidate 3's tab shows its Location first: it waits for candidate 2.
        let step = e.navigation_completed(Millis(1_650), tk(11), &location(6), &not_minimized);
        assert_eq!(step, FoldStep::default());
        let step = e.navigation_completed(Millis(1_700), tk(10), &location(5), &not_minimized);
        assert!(matches!(step.commands.as_slice(), [Command::ActivateTab { index: 1, .. }, Command::SetTimer { .. }]));
        let act2 = activate_op_of(&step.commands);

        // Candidate 2 done: candidate 3 activates its (rightmost) tab, last.
        let step = e.activation_confirmed(Millis(1_750), act2, &not_minimized);
        assert!(matches!(
            step.commands.as_slice(),
            [
                Command::Focus(_),
                Command::CloseWindow { win: closed },
                Command::SetTimer { id: close_id, .. },
                Command::ActivateTab { win: target, index: 2, .. },
                Command::SetTimer { .. },
            ] if *closed == w(2) && *target == w(1) && *close_id == fold_timer(TIMER_CLOSE, 1)
        ));
        let step = e.activation_confirmed(Millis(1_800), activate_op_of(&step.commands), &not_minimized);
        assert_eq!(step.commands, vec![Command::Focus(w(1)), Command::CloseWindow { win: w(3) }, close_timer(2, 1_800)]);
        assert!(e.target_queues.is_empty());
    }

    #[test]
    fn folds_into_different_targets_do_not_wait_for_each_other() {
        let mut e = FoldingEngine::new(true);
        e.opening_mut().append(w(1));
        e.opening_mut().append(w(3));
        let windows = [fwin(1, &[0]), fwin(3, &[0])];
        appear(&mut e, 0, 2, false, &[w(1), w(3)]);
        appear(&mut e, 1, 4, false, &[w(1), w(3)]);
        e.initial_location_known(w(4), location(6), vec![]);
        let p2 = probe_of(&e.initial_location_known(w(2), location(5), vec![]).unwrap().commands);
        // Candidate 2 goes to the Primary_Window 1; candidate 4 finds window 1
        // unresponsive and goes to window 3.
        let step = answer(&mut e, p2, &windows);
        let p4 = probe_in(&step.commands);
        let step = e.fold_context(
            Millis(CTX_AT),
            p4,
            DesktopView::Unknown,
            &BTreeSet::from([w(1)]),
            &windows,
            &real_tab,
        );
        assert!(matches!(step.commands.as_slice(), [Command::CreateTab { win, .. }, Command::SetTimer { .. }] if *win == w(3)));
        assert_eq!(e.candidates[&w(4)].slot, 1);
    }

    #[test]
    fn an_unconfirmed_activation_is_issued_again_once_then_given_up() {
        let mut e = FoldingEngine::new(true);
        e.opening_mut().append(w(1));
        let create = create_op_of(&start_fold(&mut e, 0, 2, 5, vec![], &[fwin(1, &[0])]).commands);
        e.tab_started(Millis(1_040), create, tk(10));
        e.tab_created(Millis(1_300), w(1), tk(10));
        let step = e.navigation_completed(Millis(1_400), tk(10), &location(5), &not_minimized);
        let first = activate_op_of(&step.commands);
        let timer = fold_timer(TIMER_ACTIVATE, 1);

        let step = e.timer(Millis(2_400), timer, &not_minimized);
        let second = activate_op_of(&step.commands);
        assert_ne!(first, second);
        assert!(matches!(step.commands[0], Command::ActivateTab { win, index: 1, .. } if win == w(1)));
        assert_eq!(step.commands[1], Command::SetTimer { id: timer, at: Millis(2_400 + FOLD_ACTIVATE_MS) });

        let step = e.timer(Millis(3_400), timer, &not_minimized);
        assert!(matches!(
            step.commands.as_slice(),
            [
                Command::AppendLog(LogEntry::Error { operation, .. }),
                Command::Focus(_),
                Command::CloseWindow { win },
                Command::SetTimer { .. },
            ] if operation == "fold-activate-tab" && *win == w(2)
        ));
        // Stale timers, timers of other kinds and other owners do nothing.
        assert_eq!(e.timer(Millis(4_400), timer, &not_minimized), FoldStep::default());
        let other = fold_timer(TIMER_FIRST_NAV, 1);
        assert_eq!(e.timer(Millis(4_400), other, &not_minimized), FoldStep::default());
        let foreign = TimerId { owner: TimerOwner::Tracker, seq: timer.seq };
        assert_eq!(e.timer(Millis(4_400), foreign, &not_minimized), FoldStep::default());
    }

    #[test]
    fn a_late_confirmation_of_an_earlier_activation_attempt_counts() {
        let mut e = FoldingEngine::new(true);
        e.opening_mut().append(w(1));
        let create = create_op_of(&start_fold(&mut e, 0, 2, 5, vec![], &[fwin(1, &[0])]).commands);
        e.tab_started(Millis(1_040), create, tk(10));
        e.tab_created(Millis(1_300), w(1), tk(10));
        let step = e.navigation_completed(Millis(1_400), tk(10), &location(5), &not_minimized);
        let first = activate_op_of(&step.commands);
        e.timer(Millis(2_400), fold_timer(TIMER_ACTIVATE, 1), &not_minimized);
        let step = e.activation_confirmed(Millis(2_450), first, &not_minimized);
        assert!(step.commands.contains(&Command::CloseWindow { win: w(2) }));
    }

    #[test]
    fn the_outcome_names_the_fold_target_that_was_used() {
        // Primary_Window: folded-primary.
        let mut e = FoldingEngine::new(true);
        e.opening_mut().append(w(1));
        let create = create_op_of(&start_fold(&mut e, 0, 2, 5, vec![], &[fwin(1, &[0])]).commands);
        let step = complete_fold(&mut e, create, 2, 10, 5);
        assert_eq!(step.commands[0], folded(5, FoldOutcome::FoldedPrimary));

        // Primary_Window unresponsive: folded-fallback, into the next window.
        let mut e = FoldingEngine::new(true);
        e.opening_mut().append(w(1));
        e.opening_mut().append(w(3));
        appear(&mut e, 0, 2, false, &[w(1), w(3)]);
        let p = probe_of(&e.initial_location_known(w(2), location(5), vec![]).unwrap().commands);
        let step = e.fold_context(
            Millis(CTX_AT),
            p,
            DesktopView::Unknown,
            &BTreeSet::from([w(1)]),
            &[fwin(1, &[0]), fwin(3, &[0])],
            &real_tab,
        );
        let create = create_op_of(&step.commands);
        let step = complete_fold(&mut e, create, 2, 10, 5);
        assert_eq!(
            step.commands,
            vec![
                folded(5, FoldOutcome::FoldedFallback { reason: FallbackReason::PrimaryUnresponsive }),
                Command::RescanWindow(w(3)),
            ]
        );

        // The Matching_Tab is the foreground window's Active_Tab: folded-foreground.
        let mut e = FoldingEngine::new(true);
        e.opening_mut().append(w(1));
        let step = start_fold(&mut e, 0, 2, 0, vec![], &[fwin(1, &[0])]);
        assert_eq!(
            e.candidates[&w(2)].plan,
            Some(FoldPlan::Fold { target: w(1), reason: Some(FoldReason::Foreground) })
        );
        let step = complete_fold(&mut e, create_op_of(&step.commands), 2, 10, 0);
        assert_eq!(
            step.commands[0],
            folded(0, FoldOutcome::FoldedForeground { into: FoldInto::Foreground, passed: vec![] })
        );
    }

    #[test]
    fn a_candidate_destroyed_before_its_fold_ends_is_abandoned_and_its_tab_closed() {
        let mut e = FoldingEngine::new(true);
        e.opening_mut().append(w(1));
        let create = create_op_of(&start_fold(&mut e, 0, 2, 5, vec![], &[fwin(1, &[0])]).commands);
        e.tab_started(Millis(1_040), create, tk(10));
        let step = e.window_destroyed(Millis(1_100), w(2)).unwrap();
        // Not revealed, not logged, not handed to the tracker (Req 14.27, 14.28).
        assert!(matches!(step.commands.as_slice(), [Command::CloseTab { tab, .. }] if *tab == tk(10)));
        assert!(step.released.is_empty());
        assert!(!e.owns(w(2)));
        assert!(!e.is_excluded(w(2)));
        assert!(!e.opening().contains(w(2)));
        assert!(e.target_queues.is_empty());
        assert_eq!(e.candidates_in_progress(), 0);
        assert!(e.window_destroyed(Millis(5_000), w(7)).is_none());
        assert!(e.window_destroyed(Millis(5_000), w(2)).is_none());
    }

    #[test]
    fn a_hidden_candidate_destroyed_closes_nothing_and_releases_the_candidates_behind_it() {
        let mut e = FoldingEngine::new(true);
        appear(&mut e, 0, 2, false, &[w(1)]);
        appear(&mut e, 1, 3, false, &[w(1)]);
        // Candidate 3 waits for candidate 2, whose Location never came.
        e.initial_location_known(w(3), location(7), vec![]);
        let step = e.window_destroyed(Millis(100), w(2)).unwrap();
        assert!(matches!(step.commands.as_slice(), [Command::ProbeFoldContext { candidate, .. }] if *candidate == w(3)));
        assert_eq!(e.candidates[&w(3)].phase, Phase::Context);
    }

    #[test]
    fn a_candidate_destroyed_while_creating_closes_its_late_tab_and_lets_the_next_one_create() {
        let mut e = FoldingEngine::new(true);
        e.opening_mut().append(w(1));
        let windows = [fwin(1, &[0])];
        appear(&mut e, 0, 2, false, &[w(1)]);
        appear(&mut e, 1, 3, false, &[w(1)]);
        e.initial_location_known(w(3), location(6), vec![]);
        let p2 = probe_of(&e.initial_location_known(w(2), location(5), vec![]).unwrap().commands);
        let step = answer(&mut e, p2, &windows);
        let create2 = create_op_of(&step.commands);
        answer(&mut e, probe_in(&step.commands), &windows);
        assert_eq!(e.candidates[&w(3)].slot, 2);

        // Candidate 2 goes before its tab appeared: candidate 3 creates its
        // own, one place further left, and 2's tab is closed if it shows up.
        let step = e.window_destroyed(Millis(1_100), w(2)).unwrap();
        let create3 = create_op_of(&step.commands);
        assert!(matches!(step.commands.as_slice(), [Command::CreateTab { win, .. }, Command::SetTimer { .. }] if *win == w(1)));
        assert_eq!(e.candidates[&w(3)].slot, 1);
        let late = e.tab_started(Millis(5_000), create2, tk(77));
        assert!(matches!(late.commands.as_slice(), [Command::CloseTab { tab, .. }] if *tab == tk(77)));
        assert_ne!(create2, create3);
    }

    #[test]
    fn a_switch_waiting_for_a_destroyed_candidates_tab_fails() {
        let (mut e, _) = folding_with_a_waiting_switch(vec![]);
        let step = e.window_destroyed(Millis(1_100), w(2)).unwrap();
        assert!(matches!(step.commands.as_slice(), [Command::ProbeFoldContext { candidate, .. }] if *candidate == w(3)));
        assert_eq!(e.candidates[&w(3)].switch_failure, Some(SwitchFailure::MatchingTabClosed));
    }

    #[test]
    fn the_engine_numbers_its_ops_above_the_other_components() {
        let mut e = FoldingEngine::new(true);
        e.opening_mut().append(w(1));
        let create = create_op_of(&start_fold(&mut e, 0, 2, 5, vec![], &[fwin(1, &[0])]).commands);
        assert!(create.0 >= FOLD_OP_BASE);
        assert!(create.0 > crate::engine::DEFERRED_OP_BASE + (1 << 40));
    }

    // ---- Tab_Switch and its failure chain ----

    fn switch_timer(seq: u64) -> TimerId {
        fold_timer(TIMER_SWITCH, seq)
    }

    fn create_timer(seq: u64) -> TimerId {
        fold_timer(TIMER_TAB_CREATE, seq)
    }

    /// The `SetTimer` of kind `id` among `cmds`: its due time.
    fn timer_due(cmds: &[Command], id: TimerId) -> Millis {
        cmds.iter()
            .find_map(|c| match c {
                Command::SetTimer { id: i, at } if *i == id => Some(*at),
                _ => None,
            })
            .unwrap_or_else(|| panic!("expected a timer {id:?} in {cmds:?}"))
    }

    /// Candidate 2 (Location 5) over `windows`, opening order 1, 3: the
    /// Tab_Switch it plans is in progress.
    fn start_switch(items: Vec<ItemId>, windows: &[FoldWindow<'_>]) -> (FoldingEngine, FoldStep) {
        let mut e = FoldingEngine::new(true);
        e.opening_mut().append(w(1));
        e.opening_mut().append(w(3));
        let step = start_fold(&mut e, 0, 2, 5, items, windows);
        (e, step)
    }

    /// Fails the Tab_Switch of candidate 2 by its 1 s limit and returns the
    /// fresh probe.
    fn fail_switch_by_timer(e: &mut FoldingEngine) -> FoldProbeId {
        let step = e.timer(Millis(CTX_AT + FOLD_SWITCH_MS), switch_timer(1), &not_minimized);
        probe_of(&step.commands)
    }

    #[test]
    fn a_tab_switch_activates_selects_focuses_and_closes_the_candidate() {
        let items = vec![ItemId(vec![7])];
        let (mut e, step) = start_switch(items.clone(), &[fwin(1, &[0, 5])]);
        let activate = activate_op_of(&step.commands);
        assert_eq!(
            step.commands,
            vec![
                Command::ActivateTab { op: activate, win: w(1), index: 1 },
                Command::SetTimer { id: switch_timer(1), at: Millis(CTX_AT + FOLD_SWITCH_MS) },
            ]
        );
        assert_eq!(e.candidates[&w(2)].phase, Phase::Switching);

        // An unrelated confirmation changes nothing; ours selects, focuses, closes.
        assert_eq!(e.activation_confirmed(Millis(1_050), OpId(5), &not_minimized), FoldStep::default());
        let step = e.activation_confirmed(Millis(1_100), activate, &not_minimized);
        let [Command::SelectItems { tab, items: got, .. }, Command::Focus(focus), Command::CloseWindow { win: closed }, Command::SetTimer { .. }] =
            step.commands.as_slice()
        else {
            panic!("expected select, focus and close, got {:?}", step.commands);
        };
        assert_eq!((*tab, got, *focus, *closed), (real_tab(w(1), 1).unwrap(), &items, w(1), w(2)));
        assert_eq!(e.candidates[&w(2)].phase, Phase::Closing);

        // The 1 s timer finds the switch done; a repeated confirmation is ignored.
        assert_eq!(e.timer(Millis(CTX_AT + FOLD_SWITCH_MS), switch_timer(1), &not_minimized), FoldStep::default());
        assert_eq!(e.activation_confirmed(Millis(1_200), activate, &not_minimized), FoldStep::default());

        let step = e.window_destroyed(Millis(5_000), w(2)).unwrap();
        assert_eq!(
            step.commands,
            vec![folded(5, FoldOutcome::Switched { matching: MatchingWindow::Primary }), Command::RescanWindow(w(1))]
        );
        assert_eq!(e.candidates_in_progress(), 0);
    }

    #[test]
    fn a_tab_switch_into_another_window_shows_a_minimized_window_without_a_selection() {
        let (mut e, step) = start_switch(vec![], &[fwin(1, &[0]), fwin(3, &[5])]);
        let activate = activate_op_of(&step.commands);
        assert!(matches!(step.commands[0], Command::ActivateTab { win, index: 0, .. } if win == w(3)));
        let minimized = |win: WinKey| (win == w(3)).then(minimized_placement);
        let step = e.activation_confirmed(Millis(1_100), activate, &minimized);
        assert_eq!(
            step.commands,
            vec![
                Command::ApplyPlacement { win: w(3), placement: minimized_placement() },
                Command::Focus(w(3)),
                Command::CloseWindow { win: w(2) },
                close_timer(1, 1_100),
            ]
        );
        let step = e.window_destroyed(Millis(5_000), w(2)).unwrap();
        assert_eq!(
            step.commands,
            vec![folded(5, FoldOutcome::Switched { matching: MatchingWindow::Other }), Command::RescanWindow(w(3))]
        );
    }

    #[test]
    fn a_tab_switch_that_is_not_confirmed_in_1_s_folds_into_the_window_that_held_the_tab() {
        let windows = [fwin(1, &[0, 5])];
        let (mut e, step) = start_switch(vec![], &windows);
        let first = activate_op_of(&step.commands);
        // Not before the limit.
        assert_eq!(e.timer(Millis(CTX_AT + FOLD_SWITCH_MS - 1), switch_timer(1), &not_minimized), FoldStep::default());
        let probe = fail_switch_by_timer(&mut e);
        let c = &e.candidates[&w(2)];
        assert_eq!((c.phase, c.switch_failure, c.retry_holder), (Phase::Context, Some(SwitchFailure::NotActivated), Some(w(1))));
        // The late confirmation of the failed switch is not a confirmation.
        assert_eq!(e.activation_confirmed(Millis(2_100), first, &not_minimized), FoldStep::default());

        // The window that held the Matching_Tab takes the Folded_Tab (Req 14.23),
        // as its rightmost tab, without a new Matching_Tab search.
        let step = answer_at(&mut e, 2_100, probe, &windows);
        let create = create_op_of(&step.commands);
        assert_eq!(step.commands[0], Command::CreateTab { op: create, win: w(1) });
        let c = &e.candidates[&w(2)];
        assert_eq!((c.phase, c.target, c.slot, c.fold_into), (Phase::Creating, Some(w(1)), 2, FoldInto::MatchingWindow));

        let step = complete_fold(&mut e, create, 2, 10, 5);
        let outcome = FoldOutcome::SwitchFailedFolded {
            switch_reason: SwitchFailure::NotActivated,
            into: FoldInto::MatchingWindow,
            passed: vec![],
        };
        assert_eq!(step.commands, vec![folded(5, outcome), Command::RescanWindow(w(1))]);
    }

    #[test]
    fn a_revoked_matching_tab_fails_the_tab_switch() {
        let (mut e, _) = start_switch(vec![], &[fwin(1, &[0, 5])]);
        let matching = real_tab(w(1), 1).unwrap();
        // Another tab, another window: not the Matching_Tab.
        assert_eq!(e.tab_revoked(Millis(1_100), w(1), real_tab(w(1), 0).unwrap()), FoldStep::default());
        assert_eq!(e.tab_revoked(Millis(1_100), w(3), matching), FoldStep::default());
        let step = e.tab_revoked(Millis(1_100), w(1), matching);
        probe_of(&step.commands);
        assert_eq!(e.candidates[&w(2)].switch_failure, Some(SwitchFailure::MatchingTabClosed));
        assert_eq!(e.candidates[&w(2)].phase, Phase::Context);
    }

    #[test]
    fn a_destroyed_window_that_held_the_matching_tab_moves_the_fold_to_the_primary_window() {
        let (mut e, _) = start_switch(vec![], &[fwin(1, &[0]), fwin(3, &[5])]);
        assert_eq!(e.target_destroyed(Millis(1_100), w(1)), FoldStep::default());
        let step = e.target_destroyed(Millis(1_100), w(3));
        let probe = probe_of(&step.commands);
        assert_eq!(e.candidates[&w(2)].switch_failure, Some(SwitchFailure::FoldTargetClosed));

        // The window is gone: it is passed over as closed and the Primary_Window
        // as of now takes the Folded_Tab (Req 14.24).
        let step = answer_at(&mut e, 1_150, probe, &[fwin(1, &[0])]);
        let create = create_op_of(&step.commands);
        assert_eq!(step.commands[0], Command::CreateTab { op: create, win: w(1) });
        let c = &e.candidates[&w(2)];
        assert_eq!((c.fold_into, c.passed_over.clone()), (FoldInto::Primary, vec![(w(3), PassReason::Closed)]));
        let step = complete_fold(&mut e, create, 2, 10, 5);
        let outcome = FoldOutcome::SwitchFailedFolded {
            switch_reason: SwitchFailure::FoldTargetClosed,
            into: FoldInto::Primary,
            passed: vec![PassReason::Closed],
        };
        assert_eq!(step.commands, vec![folded(5, outcome), Command::RescanWindow(w(1))]);
    }

    #[test]
    fn a_closing_or_unresponsive_holder_is_passed_over_with_its_reason() {
        for (closing, unresponsive, reason) in
            [(true, false, PassReason::Closing), (false, true, PassReason::Unresponsive)]
        {
            let (mut e, _) = start_switch(vec![], &[fwin(1, &[0]), fwin(3, &[5])]);
            let probe = fail_switch_by_timer(&mut e);
            let mut holder = fwin(3, &[5]);
            holder.closing = closing;
            let unresponsive: BTreeSet<WinKey> = unresponsive.then_some(w(3)).into_iter().collect();
            let step = e.fold_context(
                Millis(2_100),
                probe,
                DesktopView::Unknown,
                &unresponsive,
                &[fwin(1, &[0]), holder],
                &real_tab,
            );
            assert!(matches!(step.commands[0], Command::CreateTab { win, .. } if win == w(1)));
            let c = &e.candidates[&w(2)];
            assert_eq!((c.fold_into, c.passed_over.clone()), (FoldInto::Primary, vec![(w(3), reason)]));
        }
    }

    #[test]
    fn a_holder_that_is_the_primary_window_hands_the_fold_to_a_fallback_window() {
        let (mut e, _) = start_switch(vec![], &[fwin(1, &[0, 5]), fwin(3, &[0])]);
        let probe = fail_switch_by_timer(&mut e);
        let mut holder = fwin(1, &[0, 5]);
        holder.closing = true;
        let step = answer_at(&mut e, 2_100, probe, &[holder, fwin(3, &[0])]);
        let create = create_op_of(&step.commands);
        assert_eq!(step.commands[0], Command::CreateTab { op: create, win: w(3) });
        let c = &e.candidates[&w(2)];
        assert_eq!((c.fold_into, c.fallback_used), (FoldInto::Fallback, true));
        let step = complete_fold(&mut e, create, 2, 10, 5);
        let outcome = FoldOutcome::SwitchFailedFolded {
            switch_reason: SwitchFailure::NotActivated,
            into: FoldInto::Fallback,
            passed: vec![PassReason::Closing],
        };
        assert_eq!(step.commands, vec![folded(5, outcome), Command::RescanWindow(w(3))]);
    }

    #[test]
    fn the_chain_ends_with_the_candidate_left_open_when_no_window_is_left() {
        // Matching_Tab in window 3; the Primary_Window is window 1.
        let windows = [fwin(1, &[0]), fwin(3, &[5])];
        let (mut e, _) = start_switch(vec![], &windows);
        let probe = fail_switch_by_timer(&mut e);

        // The holder takes the Folded_Tab but creates no tab within 1 s.
        let step = answer_at(&mut e, 2_000, probe, &windows);
        let due = timer_due(&step.commands, create_timer(1));
        assert_eq!(due, Millis(2_000 + FOLD_TAB_CREATE_MS));
        assert_eq!(e.timer(Millis(due.0 - 1), create_timer(1), &not_minimized), FoldStep::default());
        let probe = probe_of(&e.timer(due, create_timer(1), &not_minimized).commands);

        // The Primary_Window as of now takes it, and creates no tab either.
        let step = answer_at(&mut e, 3_000, probe, &windows);
        assert!(matches!(step.commands[0], Command::CreateTab { win, .. } if win == w(1)));
        assert_eq!(e.candidates[&w(2)].fold_into, FoldInto::Primary);
        let due = timer_due(&step.commands, create_timer(1));
        let probe = probe_of(&e.timer(due, create_timer(1), &not_minimized).commands);

        // Both windows are used up: the candidate stays open (Req 14.14).
        let step = answer_at(&mut e, 4_000, probe, &windows);
        assert_eq!(
            step.commands,
            vec![
                Command::Reveal { win: w(2) },
                Command::AppendLog(LogEntry::Fold {
                    location: location(5),
                    outcome: FoldOutcome::NotFolded {
                        reason: NotFoldedReason::NoFoldTarget,
                        switch_reason: Some(SwitchFailure::NotActivated),
                    },
                }),
            ]
        );
        assert_eq!(step.released.len(), 1);
        assert!(!e.owns(w(2)));
    }

    #[test]
    fn a_primary_window_that_creates_no_tab_in_1_s_hands_the_fold_to_one_fallback_window() {
        let windows = [fwin(1, &[0]), fwin(3, &[0])];
        let run = || {
            let mut e = FoldingEngine::new(true);
            e.opening_mut().append(w(1));
            e.opening_mut().append(w(3));
            let step = start_fold(&mut e, 0, 2, 5, vec![], &windows);
            let first = create_op_of(&step.commands);
            let due = timer_due(&step.commands, create_timer(1));
            let probe = probe_of(&e.timer(due, create_timer(1), &not_minimized).commands);
            // The Primary_Window stays unchanged for later candidates; this one
            // goes to the first fallback window.
            let step = answer_at(&mut e, 2_000, probe, &windows);
            let second = create_op_of(&step.commands);
            assert_eq!(step.commands[0], Command::CreateTab { op: second, win: w(3) });
            assert_eq!(e.candidates[&w(2)].passed_over, vec![(w(1), PassReason::TabNotCreated)]);
            (e, first, second, timer_due(&step.commands, create_timer(1)))
        };

        let (mut e, first, second, _) = run();
        // The tab of the first request appears after all: it is closed, and it
        // is not the second request's tab.
        let step = e.tab_started(Millis(2_500), first, tk(77));
        assert!(matches!(step.commands.as_slice(), [Command::CloseTab { tab, .. }] if *tab == tk(77)));
        assert_eq!(e.tab_started(Millis(2_501), first, tk(78)), FoldStep::default());
        assert_eq!(e.tab_created(Millis(2_600), w(3), tk(77)), FoldStep::default());
        assert!(e.candidates[&w(2)].tab.is_none());
        let step = complete_fold(&mut e, second, 2, 10, 5);
        assert_eq!(
            step.commands,
            vec![
                folded(5, FoldOutcome::FoldedFallback { reason: FallbackReason::TabNotCreated }),
                Command::RescanWindow(w(3)),
            ]
        );

        // A tab that comes more than 10 s late is left alone.
        let (mut e, first, _, _) = run();
        assert_eq!(e.tab_started(Millis(CTX_AT + FOLD_TAB_CREATE_MS + LATE_TAB_MS + 1), first, tk(77)), FoldStep::default());

        // A fallback window that creates no tab within 1 s ends the fold: the
        // candidate stays open and nothing else is tried (Req 14.14).
        let (mut e, _, _, due) = run();
        let step = e.timer(due, create_timer(1), &not_minimized);
        assert_eq!(
            step.commands,
            vec![
                Command::Reveal { win: w(2) },
                Command::AppendLog(LogEntry::Fold {
                    location: location(5),
                    outcome: FoldOutcome::NotFolded { reason: NotFoldedReason::TabNotCreated, switch_reason: None },
                }),
            ]
        );
        assert!(!e.owns(w(2)));
    }

    #[test]
    fn a_failed_fold_into_the_foreground_window_follows_the_same_chain() {
        let windows = [fwin(1, &[0]), fwin(3, &[7])];
        let mut e = FoldingEngine::new(true);
        e.opening_mut().append(w(1));
        e.opening_mut().append(w(3));
        // Location 0 is the Active_Tab of the foreground window 1.
        let step = start_fold(&mut e, 0, 2, 0, vec![], &windows);
        assert_eq!(e.candidates[&w(2)].fold_into, FoldInto::Foreground);
        let due = timer_due(&step.commands, create_timer(1));
        let probe = probe_of(&e.timer(due, create_timer(1), &not_minimized).commands);
        let step = answer_at(&mut e, 2_000, probe, &windows);
        let create = create_op_of(&step.commands);
        assert_eq!(step.commands[0], Command::CreateTab { op: create, win: w(3) });
        let step = complete_fold(&mut e, create, 2, 10, 0);
        let outcome = FoldOutcome::FoldedForeground { into: FoldInto::Fallback, passed: vec![PassReason::TabNotCreated] };
        assert_eq!(step.commands, vec![folded(0, outcome), Command::RescanWindow(w(3))]);
    }

    #[test]
    fn a_stale_creation_timer_does_nothing() {
        let mut e = FoldingEngine::new(true);
        e.opening_mut().append(w(1));
        let step = start_fold(&mut e, 0, 2, 5, vec![], &[fwin(1, &[0]), fwin(3, &[0])]);
        let create = create_op_of(&step.commands);
        let due = timer_due(&step.commands, create_timer(1));
        // The tab appeared in time.
        e.tab_started(Millis(1_040), create, tk(10));
        assert_eq!(e.timer(due, create_timer(1), &not_minimized), FoldStep::default());
        // A timer of a candidate that is gone, or of the wrong owner.
        assert_eq!(e.timer(due, create_timer(9), &not_minimized), FoldStep::default());
    }

    #[test]
    fn a_passed_over_target_lets_the_next_candidate_of_its_queue_create_its_tab() {
        let windows = [fwin(1, &[0]), fwin(3, &[0])];
        let mut e = FoldingEngine::new(true);
        e.opening_mut().append(w(1));
        e.opening_mut().append(w(3));
        appear(&mut e, 0, 2, false, &[w(1)]);
        appear(&mut e, 1, 4, false, &[w(1)]);
        e.initial_location_known(w(4), location(6), vec![]);
        let p2 = probe_of(&e.initial_location_known(w(2), location(5), vec![]).unwrap().commands);
        let step = answer(&mut e, p2, &windows);
        let due = timer_due(&step.commands, create_timer(1));
        let p4 = probe_in(&step.commands);
        answer(&mut e, p4, &windows);
        assert_eq!((e.candidates[&w(2)].slot, e.candidates[&w(4)].slot), (1, 2));

        // Window 1 does not answer for candidate 2: candidate 3 may create its
        // tab now, one place further left, and candidate 2 asks again.
        let step = e.timer(due, create_timer(1), &not_minimized);
        assert!(matches!(
            step.commands.as_slice(),
            [
                Command::CreateTab { win: target, .. },
                Command::SetTimer { id, at },
                Command::ProbeFoldContext { candidate, .. },
            ] if *target == w(1) && *id == create_timer(2) && *at == due + FOLD_TAB_CREATE_MS && *candidate == w(2)
        ));
        assert_eq!(e.candidates[&w(4)].slot, 1);
    }

    #[test]
    fn a_folded_tab_that_does_not_show_the_location_in_3_s_leaves_the_candidate_open() {
        let mut e = FoldingEngine::new(true);
        e.opening_mut().append(w(1));
        let create = create_op_of(&start_fold(&mut e, 0, 2, 5, vec![], &[fwin(1, &[0])]).commands);
        e.tab_started(Millis(1_040), create, tk(10));
        let id = fold_timer(TIMER_TAB_SHOW, 1);
        assert_eq!(e.timer(Millis(1_040 + FOLD_TAB_SHOW_MS - 1), id, &not_minimized), FoldStep::default());
        let step = e.timer(Millis(1_040 + FOLD_TAB_SHOW_MS), id, &not_minimized);
        assert!(matches!(
            step.commands.as_slice(),
            [
                Command::CloseTab { tab, .. },
                Command::Reveal { win },
                Command::AppendLog(LogEntry::Fold {
                    outcome: FoldOutcome::NotFolded { reason: NotFoldedReason::LocationNotShown, switch_reason: None },
                    ..
                }),
            ] if *tab == tk(10) && *win == w(2)
        ));
        assert!(!e.owns(w(2)));
        assert!(e.target_queues.is_empty());
    }

    #[test]
    fn a_target_that_closes_before_it_creates_the_tab_is_passed_over() {
        let mut e = FoldingEngine::new(true);
        e.opening_mut().append(w(1));
        e.opening_mut().append(w(3));
        let windows = [fwin(1, &[0]), fwin(3, &[0])];
        let step = start_fold(&mut e, 0, 2, 5, vec![], &windows);
        let first = create_op_of(&step.commands);
        let probe = probe_of(&e.target_destroyed(Millis(1_100), w(1)).commands);
        assert_eq!(e.target_destroyed(Millis(1_100), w(9)), FoldStep::default());
        let step = answer_at(&mut e, 1_150, probe, &[fwin(3, &[0])]);
        assert!(matches!(step.commands[0], Command::CreateTab { win, .. } if win == w(3)));
        let c = &e.candidates[&w(2)];
        assert_eq!((c.fold_into, c.passed_over.clone()), (FoldInto::Primary, vec![(w(1), PassReason::Closed)]));
        // The first request's tab, if it comes, is closed.
        assert!(matches!(e.tab_started(Millis(1_200), first, tk(77)).commands.as_slice(), [Command::CloseTab { .. }]));
    }

    #[test]
    fn a_target_that_closes_after_it_created_the_tab_leaves_the_candidate_open() {
        let mut e = FoldingEngine::new(true);
        e.opening_mut().append(w(1));
        let create = create_op_of(&start_fold(&mut e, 0, 2, 5, vec![], &[fwin(1, &[0])]).commands);
        e.tab_started(Millis(1_040), create, tk(10));
        let step = e.target_destroyed(Millis(1_100), w(1));
        assert!(matches!(
            step.commands.as_slice(),
            [
                Command::CloseTab { tab, .. },
                Command::Reveal { .. },
                Command::AppendLog(LogEntry::Fold {
                    outcome: FoldOutcome::NotFolded { reason: NotFoldedReason::FoldTargetClosed, .. },
                    ..
                }),
            ] if *tab == tk(10)
        ));
        assert!(!e.owns(w(2)));
    }

    /// Candidates 2 and 3 show Location 5; candidate 2 folds into window 1 and
    /// candidate 3 switches to candidate 2's Folded_Tab once it is active.
    fn folding_with_a_waiting_switch(items: Vec<ItemId>) -> (FoldingEngine, OpId) {
        let mut e = FoldingEngine::new(true);
        e.opening_mut().append(w(1));
        let windows = [fwin(1, &[0])];
        appear(&mut e, 0, 2, false, &[w(1)]);
        appear(&mut e, 1, 3, false, &[w(1)]);
        e.initial_location_known(w(3), location(5), items);
        let p2 = probe_of(&e.initial_location_known(w(2), location(5), vec![]).unwrap().commands);
        let step = answer(&mut e, p2, &windows);
        let create2 = create_op_of(&step.commands);
        let p3 = probe_in(&step.commands);
        let step = answer(&mut e, p3, &windows);
        assert!(step.commands.is_empty());
        assert_eq!(e.candidates[&w(3)].phase, Phase::Switching);
        assert_eq!(e.switch_waits_on(w(3)), Some(w(2)));
        (e, create2)
    }

    #[test]
    fn a_switch_onto_an_earlier_candidates_folded_tab_starts_when_that_tab_is_active() {
        let items = vec![ItemId(vec![9])];
        let (mut e, create2) = folding_with_a_waiting_switch(items.clone());
        e.tab_started(Millis(1_040), create2, tk(10));
        e.tab_created(Millis(1_300), w(1), tk(10));
        let step = e.navigation_completed(Millis(1_400), tk(10), &location(5), &not_minimized);
        // The tab is shown but not active yet: the switch still waits.
        assert_eq!(e.switch_waits_on(w(3)), Some(w(2)));
        let act2 = activate_op_of(&step.commands);

        let step = e.activation_confirmed(Millis(1_450), act2, &not_minimized);
        assert_eq!(e.switch_waits_on(w(3)), None);
        assert_eq!(e.folded_tab(w(2)), Some(tk(10)));
        let [
            Command::Focus(_),
            Command::CloseWindow { win: closed },
            Command::SetTimer { .. },
            Command::ActivateTab { op: act3, win: holder, index: 1 },
            Command::SetTimer { id, at },
        ] = step.commands.as_slice()
        else {
            panic!("expected candidate 2's close and candidate 3's activation, got {:?}", step.commands);
        };
        assert_eq!((*closed, *holder, *id, *at), (w(2), w(1), switch_timer(2), Millis(1_450 + FOLD_SWITCH_MS)));

        // The Matching_Tab is the Folded_Tab of candidate 2: its items are
        // selected there.
        let step = e.activation_confirmed(Millis(1_500), *act3, &not_minimized);
        assert!(matches!(
            step.commands.as_slice(),
            [Command::SelectItems { tab, items: got, .. }, Command::Focus(_), Command::CloseWindow { win }, Command::SetTimer { .. }]
                if *tab == tk(10) && got == &items && *win == w(3)
        ));
        let done2 = e.window_destroyed(Millis(5_000), w(2)).unwrap();
        assert_eq!(done2.commands, vec![folded(5, FoldOutcome::FoldedPrimary), Command::RescanWindow(w(1))]);
        let done3 = e.window_destroyed(Millis(5_000), w(3)).unwrap();
        assert_eq!(
            done3.commands,
            vec![folded(5, FoldOutcome::Switched { matching: MatchingWindow::Primary }), Command::RescanWindow(w(1))]
        );
    }

    #[test]
    fn a_switch_that_waits_for_a_folded_tab_fails_when_that_candidate_gives_up() {
        let (mut e, _) = folding_with_a_waiting_switch(vec![]);
        let due = Millis(CTX_AT + FOLD_TAB_CREATE_MS);
        let step = e.timer(due, create_timer(1), &not_minimized);
        // Candidate 2 asks again, and candidate 3's Matching_Tab will not come.
        assert!(matches!(
            step.commands.as_slice(),
            [
                Command::ProbeFoldContext { candidate: first, probe: p2 },
                Command::ProbeFoldContext { candidate: second, probe: p3 },
            ] if *first == w(2) && *second == w(3) && p2 != p3
        ));
        let c = &e.candidates[&w(3)];
        assert_eq!(
            (c.phase, c.switch_failure, c.retry_holder, c.waits_on),
            (Phase::Context, Some(SwitchFailure::MatchingTabClosed), Some(w(1)), None)
        );
        assert_eq!(e.switch_waits_on(w(3)), None);
    }

    #[test]
    fn a_candidate_left_open_fails_the_switch_that_waited_for_its_tab() {
        let (mut e, _) = folding_with_a_waiting_switch(vec![]);
        // Candidate 2 has no other window to go to: it stays open.
        let due = Millis(CTX_AT + FOLD_TAB_CREATE_MS);
        let step = e.timer(due, create_timer(1), &not_minimized);
        let (p2, p3) = match step.commands.as_slice() {
            [Command::ProbeFoldContext { probe: a, .. }, Command::ProbeFoldContext { probe: b, .. }] => (*a, *b),
            other => panic!("expected two probes, got {other:?}"),
        };
        let step = answer_at(&mut e, 2_100, p2, &[fwin(1, &[0])]);
        assert_eq!(step.commands[0], Command::Reveal { win: w(2) });
        // Candidate 3 now folds into window 1 (it held the Matching_Tab): one
        // real tab, none pending.
        let step = answer_at(&mut e, 2_100, p3, &[fwin(1, &[0])]);
        assert!(matches!(step.commands[0], Command::CreateTab { win, .. } if win == w(1)));
        assert_eq!(e.candidates[&w(3)].slot, 1);
    }

    // ---- leaving a candidate open (Req 14.14, 14.15) ----

    fn first_nav_timer() -> TimerId {
        fold_timer(TIMER_FIRST_NAV, 1)
    }

    #[test]
    fn a_first_navigation_that_takes_8_s_leaves_the_candidate_open() {
        let mut e = FoldingEngine::new(true);
        appear(&mut e, 1_000, 2, false, &[w(1)]);
        e.window_scanned(&scan(2, 1));
        let due = 1_000 + FOLD_FIRST_NAV_MS;
        assert_eq!(e.timer(Millis(due - 1), first_nav_timer(), &not_minimized), FoldStep::default());
        // The Location is unknown, so there is no Location to name in a FOLD entry.
        let step = e.timer(Millis(due), first_nav_timer(), &not_minimized);
        assert_eq!(step.commands, vec![Command::Reveal { win: w(2) }]);
        assert_eq!(step.released, vec![Released { win: w(2), buffered: vec![Event::WindowScanned(scan(2, 1))] }]);
        assert!(!e.owns(w(2)));
        assert!(e.is_excluded(w(2)));
        assert!(e.opening().contains(w(2)));
        // The timer fires once.
        assert_eq!(e.timer(Millis(due + 1), first_nav_timer(), &not_minimized), FoldStep::default());
    }

    #[test]
    fn a_first_navigation_timeout_logs_the_location_the_scan_shows() {
        let mut e = FoldingEngine::new(true);
        appear(&mut e, 0, 2, false, &[w(1)]);
        let mut obs = scan(2, 1);
        obs.tabs[0].location = crate::engine::ObservedLocation::Known(location(5));
        e.window_scanned(&obs);
        let step = e.timer(Millis(FOLD_FIRST_NAV_MS), first_nav_timer(), &not_minimized);
        assert_eq!(
            step.commands,
            vec![
                Command::Reveal { win: w(2) },
                Command::AppendLog(LogEntry::Fold {
                    location: location(5),
                    outcome: FoldOutcome::NotFolded { reason: NotFoldedReason::FirstNavTimeout, switch_reason: None },
                }),
            ]
        );
    }

    #[test]
    fn the_first_navigation_timer_is_stale_once_the_location_is_known() {
        let mut e = FoldingEngine::new(true);
        e.opening_mut().append(w(1));
        start_fold(&mut e, 0, 2, 5, vec![], &[fwin(1, &[0])]);
        assert_eq!(e.timer(Millis(FOLD_FIRST_NAV_MS), first_nav_timer(), &not_minimized), FoldStep::default());
        assert!(e.owns(w(2)));
    }

    #[test]
    fn a_candidate_that_timed_out_no_longer_holds_the_later_ones_back() {
        let mut e = FoldingEngine::new(true);
        appear(&mut e, 0, 2, false, &[w(1)]);
        appear(&mut e, 1, 3, false, &[w(1)]);
        e.initial_location_known(w(3), location(7), vec![]);
        let step = e.timer(Millis(FOLD_FIRST_NAV_MS), first_nav_timer(), &not_minimized);
        assert_eq!(step.commands[0], Command::Reveal { win: w(2) });
        assert!(matches!(step.commands[1], Command::ProbeFoldContext { candidate, .. } if candidate == w(3)));
        assert_eq!(step.commands.len(), 2);
    }

    #[test]
    fn a_failed_hide_is_logged_and_the_fold_goes_on() {
        let mut e = FoldingEngine::new(true);
        appear(&mut e, 0, 2, false, &[w(1)]);
        let error = crate::engine::ShellError { code: 5, message: "access denied".into() };
        let step = e.hide_failed(w(2), &error);
        assert!(matches!(
            step.commands.as_slice(),
            [Command::AppendLog(LogEntry::Error { operation, description })]
                if operation == "fold-hide" && description.contains("access denied")
        ));
        assert!(step.released.is_empty());
        assert!(e.owns(w(2)));
        assert_eq!(e.candidates[&w(2)].phase, Phase::Hidden);
        // Not a candidate: nothing to log.
        assert_eq!(e.hide_failed(w(9), &error), FoldStep::default());
    }

    /// Candidate 2 (Location 5) folded into window 1 up to its `CloseWindow`
    /// at 1 450 ms.
    fn closing_candidate() -> FoldingEngine {
        let mut e = FoldingEngine::new(true);
        e.opening_mut().append(w(1));
        let create = create_op_of(&start_fold(&mut e, 0, 2, 5, vec![], &[fwin(1, &[0])]).commands);
        e.tab_started(Millis(1_040), create, tk(10));
        e.tab_created(Millis(1_300), w(1), tk(10));
        let step = e.navigation_completed(Millis(1_400), tk(10), &location(5), &not_minimized);
        e.activation_confirmed(Millis(1_450), activate_op_of(&step.commands), &not_minimized);
        assert_eq!(e.candidates[&w(2)].phase, Phase::Closing);
        e
    }

    #[test]
    fn a_candidate_that_ignores_close_window_gets_it_once_more_after_2_s() {
        let mut e = closing_candidate();
        let id = fold_timer(TIMER_CLOSE, 1);
        let due = 1_450 + FOLD_CLOSE_RETRY_MS;
        assert_eq!(e.timer(Millis(due - 1), id, &not_minimized), FoldStep::default());
        let step = e.timer(Millis(due), id, &not_minimized);
        assert_eq!(
            step.commands,
            vec![Command::CloseWindow { win: w(2) }, Command::SetTimer { id, at: Millis(due + FOLD_CLOSE_RETRY_MS) }]
        );
        assert!(step.released.is_empty());
        assert!(e.owns(w(2)));
        // The second post closed it: the fold completes as usual, and the
        // pending timer finds no candidate.
        let step = e.window_destroyed(Millis(due + 100), w(2)).unwrap();
        assert_eq!(step.commands, vec![folded(5, FoldOutcome::FoldedPrimary), Command::RescanWindow(w(1))]);
        assert_eq!(e.timer(Millis(due + FOLD_CLOSE_RETRY_MS), id, &not_minimized), FoldStep::default());
    }

    #[test]
    fn a_candidate_that_closes_in_time_is_not_asked_again() {
        let mut e = closing_candidate();
        e.window_destroyed(Millis(1_800), w(2)).unwrap();
        let id = fold_timer(TIMER_CLOSE, 1);
        assert_eq!(e.timer(Millis(1_450 + FOLD_CLOSE_RETRY_MS), id, &not_minimized), FoldStep::default());
    }

    #[test]
    fn a_candidate_that_ignores_both_posts_is_revealed_and_handed_over() {
        let mut e = closing_candidate();
        e.window_scanned(&scan(2, 1));
        let id = fold_timer(TIMER_CLOSE, 1);
        let first = 1_450 + FOLD_CLOSE_RETRY_MS;
        e.timer(Millis(first), id, &not_minimized);
        let step = e.timer(Millis(first + FOLD_CLOSE_RETRY_MS), id, &not_minimized);
        // Reveal first, so no invisible window remains; the Folded_Tab stays and
        // is rescanned; the fold that did complete is logged.
        assert!(matches!(step.commands[0], Command::Reveal { win } if win == w(2)));
        assert!(matches!(
            &step.commands[1],
            Command::AppendLog(LogEntry::Error { operation, .. }) if operation == "fold-close"
        ));
        assert_eq!(step.commands[2], folded(5, FoldOutcome::FoldedPrimary));
        assert_eq!(step.commands[3], Command::RescanWindow(w(1)));
        assert_eq!(step.commands.len(), 4);
        assert_eq!(step.released.len(), 1);
        assert_eq!(step.released[0].win, w(2));
        assert!(!e.owns(w(2)));
        assert!(e.is_excluded(w(2)));
        assert!(e.opening().contains(w(2)));
        assert_eq!(e.candidates_in_progress(), 0);
        // Its destruction later is an ordinary window close for the tracker.
        assert!(e.window_destroyed(Millis(20_000), w(2)).is_none());
    }

    #[test]
    fn a_tab_switch_candidate_that_ignores_close_window_is_handed_over_too() {
        let (mut e, step) = start_switch(vec![], &[fwin(1, &[0, 5])]);
        let step = e.activation_confirmed(Millis(1_100), activate_op_of(&step.commands), &not_minimized);
        assert!(step.commands.contains(&close_timer(1, 1_100)));
        let id = fold_timer(TIMER_CLOSE, 1);
        e.timer(Millis(1_100 + FOLD_CLOSE_RETRY_MS), id, &not_minimized);
        let step = e.timer(Millis(1_100 + 2 * FOLD_CLOSE_RETRY_MS), id, &not_minimized);
        assert_eq!(step.commands[0], Command::Reveal { win: w(2) });
        assert_eq!(step.commands[2], folded(5, FoldOutcome::Switched { matching: MatchingWindow::Primary }));
        assert_eq!(step.commands[3], Command::RescanWindow(w(1)));
    }

    #[test]
    fn turning_folding_off_leaves_every_candidate_in_progress_open() {
        let mut e = FoldingEngine::new(true);
        e.opening_mut().append(w(1));
        let windows = [fwin(1, &[0])];
        appear(&mut e, 0, 2, false, &[w(1)]);
        appear(&mut e, 1, 3, false, &[w(1)]);
        appear(&mut e, 2, 4, false, &[w(1)]);
        e.initial_location_known(w(3), location(6), vec![]);
        let p2 = probe_of(&e.initial_location_known(w(2), location(5), vec![]).unwrap().commands);
        let step = answer(&mut e, p2, &windows);
        let create2 = create_op_of(&step.commands);
        answer(&mut e, probe_in(&step.commands), &windows);
        // Candidate 2 is closing, candidate 3 has created its tab, candidate 4
        // is still waiting for its Location.
        e.tab_started(Millis(1_040), create2, tk(10));
        let step = e.tab_created(Millis(1_300), w(1), tk(10));
        let create3 = create_op_of(&step.commands);
        let step = e.navigation_completed(Millis(1_400), tk(10), &location(5), &not_minimized);
        e.activation_confirmed(Millis(1_450), activate_op_of(&step.commands), &not_minimized);
        e.tab_started(Millis(1_500), create3, tk(11));
        assert_eq!(e.candidates[&w(2)].phase, Phase::Closing);

        // Turning it on again, or off twice, changes nothing.
        assert_eq!(e.settings_changed(Millis(1_600), true), FoldStep::default());
        let step = e.settings_changed(Millis(1_600), false);
        let off = FoldOutcome::NotFolded { reason: NotFoldedReason::FoldingDisabled, switch_reason: None };
        assert_eq!(
            step.commands.len(),
            4,
            "CloseTab, Reveal and the entry of candidate 3, Reveal of candidate 4: {:?}",
            step.commands
        );
        assert!(matches!(step.commands[0], Command::CloseTab { tab, .. } if tab == tk(11)));
        assert_eq!(step.commands[1], Command::Reveal { win: w(3) });
        assert_eq!(step.commands[2], folded(6, off));
        assert_eq!(step.commands[3], Command::Reveal { win: w(4) });
        let released: Vec<WinKey> = step.released.iter().map(|r| r.win).collect();
        assert_eq!(released, vec![w(3), w(4)]);
        // The closing candidate completes its fold.
        assert!(!e.enabled());
        assert!(e.owns(w(2)));
        let done = e.window_destroyed(Millis(1_700), w(2)).unwrap();
        assert_eq!(done.commands, vec![folded(5, FoldOutcome::FoldedPrimary), Command::RescanWindow(w(1))]);
        // New windows are excluded from then on; turning it on only affects
        // windows that appear later (Req 14.3 f).
        assert_eq!(appear(&mut e, 2_000, 5, false, &[w(1)]).verdict, excluded(ExclusionReason::FoldingDisabled));
        e.settings_changed(Millis(2_100), true);
        assert!(e.enabled());
        assert!(e.is_excluded(w(5)));
        assert_eq!(appear(&mut e, 2_200, 6, false, &[w(1)]).verdict, FirstSignalVerdict::Candidate);
    }

    #[test]
    fn turning_folding_off_issues_no_new_commands_for_the_candidates_it_stops() {
        // Candidate 3 waits to switch to candidate 2's Folded_Tab; both are stopped.
        let (mut e, _) = folding_with_a_waiting_switch(vec![]);
        let step = e.settings_changed(Millis(1_100), false);
        assert_eq!(step.released.len(), 2);
        assert!(
            step.commands
                .iter()
                .all(|c| matches!(c, Command::Reveal { .. } | Command::AppendLog(_) | Command::CloseTab { .. })),
            "{:?}",
            step.commands
        );
        assert_eq!(e.candidates_in_progress(), 0);
        assert!(e.target_queues.is_empty());
    }

    #[test]
    fn a_tab_of_a_stopped_fold_is_closed_up_to_10_s_late_and_not_after() {
        let mut e = FoldingEngine::new(true);
        e.opening_mut().append(w(1));
        let first = create_op_of(&start_fold(&mut e, 0, 2, 5, vec![], &[fwin(1, &[0])]).commands);
        // The Fold_Target creates no tab in 1 s and is the only window: the
        // candidate stays open.
        let due = Millis(CTX_AT + FOLD_TAB_CREATE_MS);
        let probe = probe_in(&e.timer(due, create_timer(1), &not_minimized).commands);
        let step = answer_at(&mut e, due.0, probe, &[fwin(1, &[0])]);
        assert_eq!(step.commands[0], Command::Reveal { win: w(2) });
        assert!(!e.owns(w(2)));

        let mut in_time = e.clone();
        let step = in_time.tab_started(Millis(due.0 + LATE_TAB_MS), first, tk(9));
        assert!(matches!(step.commands.as_slice(), [Command::CloseTab { tab, .. }] if *tab == tk(9)));
        // Its registration is not a Folded_Tab of anyone.
        assert_eq!(in_time.tab_created(Millis(due.0 + LATE_TAB_MS + 50), w(1), tk(9)), FoldStep::default());
        let mut too_late = e.clone();
        assert_eq!(too_late.tab_started(Millis(due.0 + LATE_TAB_MS + 1), first, tk(9)), FoldStep::default());
        // Another op is not ours.
        assert_eq!(e.tab_started(Millis(due.0 + 1), OpId(3), tk(9)), FoldStep::default());
    }

    #[test]
    fn a_pending_create_tab_of_a_candidate_left_open_is_remembered() {
        let mut e = FoldingEngine::new(true);
        e.opening_mut().append(w(1));
        let create = create_op_of(&start_fold(&mut e, 0, 2, 5, vec![], &[fwin(1, &[0])]).commands);
        let step = e.target_destroyed(Millis(1_100), w(1));
        // The only window is gone; the candidate is left open at the fresh context.
        let probe = probe_of(&step.commands);
        let step = answer_at(&mut e, 1_150, probe, &[]);
        assert_eq!(step.commands[0], Command::Reveal { win: w(2) });
        let late = e.tab_started(Millis(2_000), create, tk(9));
        assert!(matches!(late.commands.as_slice(), [Command::CloseTab { tab, .. }] if *tab == tk(9)));
    }

    #[test]
    fn a_new_engine_has_no_candidates() {
        let e = FoldingEngine::new(true);
        assert!(e.enabled());
        assert_eq!(e.candidates_in_progress(), 0);
        assert!(!FoldingEngine::new(false).enabled());
    }

    // ---- concurrent candidates: ordering, double counting, a closing target ----

    /// Candidates 2 (Location `loc2`) and 3 (Location `loc3`) are hidden and
    /// know their Locations; candidate 2's context is answered with
    /// `windows`. Returns the engine, candidate 2's `CreateTab` and candidate
    /// 3's probe.
    fn two_candidates(loc2: u16, loc3: u16, windows: &[FoldWindow<'_>]) -> (FoldingEngine, OpId, FoldProbeId) {
        let mut e = FoldingEngine::new(true);
        e.opening_mut().append(w(1));
        appear(&mut e, 0, 2, false, &[w(1)]);
        appear(&mut e, 1, 3, false, &[w(1)]);
        e.initial_location_known(w(3), location(loc3), vec![]);
        let p2 = probe_of(&e.initial_location_known(w(2), location(loc2), vec![]).unwrap().commands);
        let step = answer(&mut e, p2, windows);
        (e, create_op_of(&step.commands), probe_in(&step.commands))
    }

    #[test]
    fn a_tab_switch_activates_its_tab_after_the_earlier_candidates_folding_into_the_same_window() {
        // Window 1 shows Locations 0 and 7; candidate 2 folds Location 5 into
        // it, candidate 3 switches to the tab with Location 7.
        let windows = [fwin(1, &[0, 7])];
        let (mut e, create2, p3) = two_candidates(5, 7, &windows);
        let step = answer(&mut e, p3, &windows);
        let c3 = &e.candidates[&w(3)];
        assert_eq!((c3.phase, c3.plan), (Phase::Switching, Some(FoldPlan::Switch { win: w(1), index: 1 })));
        // The Matching_Tab exists, but the switch must not run ahead of the
        // earlier candidate's activation of its Folded_Tab (Req 14.19).
        assert!(step.commands.is_empty(), "{:?}", step.commands);
        assert!(!c3.switch_started);

        e.tab_started(Millis(1_040), create2, tk(10));
        e.tab_created(Millis(1_300), w(1), tk(10));
        let step = e.navigation_completed(Millis(1_400), tk(10), &location(5), &not_minimized);
        assert!(!step.commands.iter().any(|c| matches!(c, Command::CloseWindow { .. })));
        let step = e.activation_confirmed(Millis(1_450), activate_op_of(&step.commands), &not_minimized);
        // Candidate 2 closes; only then does candidate 3 activate its tab, the
        // last `ActivateTab` of window 1, so its tab ends active.
        let [
            Command::Focus(_),
            Command::CloseWindow { win: closed },
            Command::SetTimer { .. },
            Command::ActivateTab { win: holder, index: 1, .. },
            Command::SetTimer { .. },
        ] = step.commands.as_slice()
        else {
            panic!("expected candidate 2's close, then candidate 3's activation, got {:?}", step.commands);
        };
        assert_eq!((*closed, *holder), (w(2), w(1)));
    }

    #[test]
    fn a_switch_held_back_by_an_earlier_candidate_still_lands_on_a_folded_tab_whose_candidate_is_gone() {
        // Candidate 3 switches to candidate 2's Folded_Tab, and candidate 4,
        // folding Location 6 into the same window before it, is still busy
        // when candidate 2 is destroyed.
        let windows = [fwin(1, &[0])];
        let mut e = FoldingEngine::new(true);
        e.opening_mut().append(w(1));
        for (at, n) in [(0, 2), (1, 4), (2, 3)] {
            appear(&mut e, at, n, false, &[w(1)]);
        }
        e.initial_location_known(w(3), location(5), vec![]);
        e.initial_location_known(w(4), location(6), vec![]);
        let p2 = probe_of(&e.initial_location_known(w(2), location(5), vec![]).unwrap().commands);
        let step = answer(&mut e, p2, &windows);
        let create2 = create_op_of(&step.commands);
        let step = answer(&mut e, probe_in(&step.commands), &windows);
        answer(&mut e, probe_in(&step.commands), &windows);
        assert_eq!(e.switch_waits_on(w(3)), Some(w(2)));
        // Candidate 2 is shown and active; candidate 4 (earlier than 3) still
        // has to create its tab, so the switch waits, and survives candidate
        // 2 being destroyed.
        e.tab_started(Millis(1_040), create2, tk(10));
        let step = e.tab_created(Millis(1_300), w(1), tk(10));
        let create4 = create_op_of(&step.commands);
        let step = e.navigation_completed(Millis(1_400), tk(10), &location(5), &not_minimized);
        e.activation_confirmed(Millis(1_450), activate_op_of(&step.commands), &not_minimized);
        assert!(!e.candidates[&w(3)].switch_started);
        e.window_destroyed(Millis(1_500), w(2)).expect("a candidate");
        assert_eq!(e.candidates[&w(3)].phase, Phase::Switching, "the switch has not failed");
        e.tab_started(Millis(1_540), create4, tk(11));
        e.tab_created(Millis(1_600), w(1), tk(11));
        let step = e.navigation_completed(Millis(1_700), tk(11), &location(6), &not_minimized);
        let step = e.activation_confirmed(Millis(1_750), activate_op_of(&step.commands), &not_minimized);
        let tab_of_3 = step.commands.iter().find_map(|c| match c {
            Command::ActivateTab { index, .. } => Some(*index),
            _ => None,
        });
        assert_eq!(tab_of_3, Some(1), "candidate 3 activates candidate 2's Folded_Tab: {:?}", step.commands);
        assert_eq!(e.candidates[&w(3)].switch_tab, Some(tk(10)));
    }

    #[test]
    fn a_registered_folded_tab_the_tracker_lists_is_not_counted_twice() {
        // Candidate 2's Folded_Tab is registered and the tracker lists it, as
        // the second tab of window 1, without a Location yet.
        let windows = [fwin(1, &[0])];
        let (mut e, create2, p3) = two_candidates(5, 6, &windows);
        let tab2 = real_tab(w(1), 1).unwrap();
        e.tab_started(Millis(1_040), create2, tab2);
        e.tab_created(Millis(1_300), w(1), tab2);
        let mut listed = fwin(1, &[0, 0]);
        listed.tabs[1] = None;
        let step = answer(&mut e, p3, &[listed]);
        assert!(matches!(step.commands.first(), Some(Command::CreateTab { .. })), "{:?}", step.commands);
        assert_eq!(e.candidates[&w(3)].slot, 2, "its tab is the third: the listed one and its own");
    }

    #[test]
    fn a_switch_onto_a_folded_tab_the_tracker_lists_waits_for_its_candidate_and_finds_the_right_index() {
        // Candidates 2 and 3 show the same Location; the tracker lists
        // candidate 2's registered tab without its Location. Counting it as
        // pending as well would send the switch to a tab that does not exist.
        let windows = [fwin(1, &[0])];
        let (mut e, create2, p3) = two_candidates(5, 5, &windows);
        let tab2 = real_tab(w(1), 1).unwrap();
        e.tab_started(Millis(1_040), create2, tab2);
        e.tab_created(Millis(1_300), w(1), tab2);
        let mut listed = fwin(1, &[0, 0]);
        listed.tabs[1] = None;
        let step = answer(&mut e, p3, &[listed]);
        assert!(step.commands.is_empty(), "{:?}", step.commands);
        let c3 = &e.candidates[&w(3)];
        assert_eq!(c3.plan, Some(FoldPlan::Switch { win: w(1), index: 1 }));
        assert_eq!(e.switch_waits_on(w(3)), Some(w(2)));

        // Listed with its Location, the tab is found the same way.
        let (mut e, create2, p3) = two_candidates(5, 5, &windows);
        e.tab_started(Millis(1_040), create2, tab2);
        e.tab_created(Millis(1_300), w(1), tab2);
        answer(&mut e, p3, &[fwin(1, &[0, 5])]);
        assert_eq!(e.candidates[&w(3)].plan, Some(FoldPlan::Switch { win: w(1), index: 1 }));
        assert_eq!(e.switch_waits_on(w(3)), Some(w(2)));

        // Listed showing the new tab's start page (another Location), it is
        // still candidate 2's Folded_Tab and not a third tab to create.
        let (mut e, create2, p3) = two_candidates(5, 5, &windows);
        e.tab_started(Millis(1_040), create2, tab2);
        e.tab_created(Millis(1_300), w(1), tab2);
        answer(&mut e, p3, &[fwin(1, &[0, 9])]);
        assert_eq!(e.candidates[&w(3)].plan, Some(FoldPlan::Switch { win: w(1), index: 1 }));
        assert_eq!(e.switch_waits_on(w(3)), Some(w(2)));
    }

    #[test]
    fn a_fold_target_that_begins_closing_leaves_the_candidate_open_when_its_folded_tab_is_revoked() {
        let mut e = FoldingEngine::new(true);
        e.opening_mut().append(w(1));
        let create = create_op_of(&start_fold(&mut e, 0, 2, 5, vec![], &[fwin(1, &[0])]).commands);
        e.tab_started(Millis(1_040), create, tk(10));
        e.tab_created(Millis(1_300), w(1), tk(10));
        let step = e.navigation_completed(Millis(1_400), tk(10), &location(5), &not_minimized);
        let activate = activate_op_of(&step.commands);
        assert_eq!(e.candidates[&w(2)].phase, Phase::Finishing);

        // Another tab of the window closing is no concern of the fold.
        assert_eq!(e.tab_revoked(Millis(1_410), w(1), tk(99)), FoldStep::default());
        assert_eq!(e.candidates[&w(2)].phase, Phase::Finishing);

        // The Folded_Tab is revoked: the window is closing, `WindowDestroyed`
        // follows. The candidate stays open instead of being closed with the
        // folder (Req 14.14).
        let step = e.tab_revoked(Millis(1_420), w(1), tk(10));
        assert!(matches!(
            step.commands.as_slice(),
            [
                Command::CloseTab { tab, .. },
                Command::Reveal { win },
                Command::AppendLog(LogEntry::Fold {
                    outcome: FoldOutcome::NotFolded { reason: NotFoldedReason::FoldTargetClosed, .. },
                    ..
                }),
            ] if *tab == tk(10) && *win == w(2)
        ));
        assert_eq!(step.released.len(), 1);
        assert!(!e.owns(w(2)));
        // The unconfirmed activation and the activation timer no longer close it.
        assert_eq!(e.activation_confirmed(Millis(1_450), activate, &not_minimized), FoldStep::default());
        let retry = e.timer(Millis(1_400 + FOLD_ACTIVATE_MS), fold_timer(TIMER_ACTIVATE, 1), &not_minimized);
        assert_eq!(retry, FoldStep::default());
        assert!(e.target_queues.is_empty());
    }

    #[test]
    fn a_revoked_folded_tab_lets_the_candidate_behind_it_go_on() {
        let windows = [fwin(1, &[0])];
        let (mut e, create2, p3) = two_candidates(5, 6, &windows);
        let step = answer(&mut e, p3, &windows);
        assert!(step.commands.is_empty());
        e.tab_started(Millis(1_040), create2, tk(10));
        let step = e.tab_created(Millis(1_300), w(1), tk(10));
        let create3 = create_op_of(&step.commands);
        assert_ne!(create2, create3);
        // The head's tab is revoked before it showed the Location.
        let step = e.tab_revoked(Millis(1_350), w(1), tk(10));
        assert!(step.commands.contains(&Command::Reveal { win: w(2) }));
        assert!(e.owns(w(3)));
        assert_eq!(e.target_queues[&w(1)], VecDeque::from([w(3)]));
    }
}
