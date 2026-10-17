//! Reference model of the Live_Session for model-based tracking tests.
//!
//! [`Reference`] is the "obviously correct" expected state that the
//! `LiveSessionTracker` must converge to once its debounce and tombstone
//! timers have expired (Property 1), and it records the Session_Ends the
//! tracker must emit (Properties 3 and 4). It is driven by
//! [`SimExplorer`](crate::explorer::SimExplorer) with the *ground truth* of each
//! operation (which window now holds which tabs in which visual order), never
//! with the core `Event`s, so it is independent of how events are translated.
//!
//! Semantics (design.md, "Live session tracking" and "Data Models"):
//!
//! - Windows are kept in Activation_Order, most recent first. A window the
//!   model has not seen before is appended (least recent) until activated.
//! - Each tab carries its most recent *identified* Location: navigations that
//!   have not completed, and completed navigations with no Location_Identifier
//!   (Req 8.8), leave it unchanged. Tabs that never had one are left out of the
//!   snapshot; a window left with no tabs is left out entirely.
//! - If a window's Active_Tab is left out, the nearest captured tab to its left
//!   becomes active, or the leftmost captured tab if there is none. (This is
//!   the same rule `apply_caps` uses for a cut Active_Tab. The tracker can
//!   apply it with `Reconciled::uia_tabs` and the UIA selection.)
//! - A process termination keeps the process's windows as `Orphaned`
//!   (Req 3.3). When no `Open` window remains, a Session_End with cause
//!   `ExplorerTerminated` and the pre-termination snapshot is recorded
//!   (Req 3.4).
//! - Closing the last `Open` window records a Session_End with the snapshot
//!   taken just before the close began (Req 3.1, 3.2); the caller supplies the
//!   cause (`LastTabClosed` or `LastWindowClosed`, decided by the event
//!   ordering, see design.md "Close coalescing").
//! - Every Session_End drops all windows, including orphans, so the
//!   Live_Session is empty afterwards. A completed restoration drops orphans.
//! - Snapshot window ids are assigned by Activation_Order position, and the
//!   caps are applied last ([`apply_caps`]).
//!
//! Compare snapshots from different sources with [`canonical`], because file
//! window ids are arbitrary.
//!
//! # Tab folding
//!
//! [`RefOpeningOrder`] and [`FoldReference`] model the expected outcome of
//! Tab_Folding (Req 14.8, 14.19, 14.20), written independently of
//! `esr_core::fold`:
//!
//! - **Opening_Order** ([`RefOpeningOrder`]): windows in the order they
//!   opened; windows already open at start or resumption in their
//!   Activation_Order; at Restoration_Completion the trigger and the restored
//!   windows move in front of the windows opened meanwhile; a candidate holds
//!   a reserved position that counts only if it is left open (Req 14.15).
//! - **Fold_Targets** ([`RefFoldTarget`]): the expected tabs, Active_Tab,
//!   selection and Window_Placement. A Folded_Tab is added at the right end
//!   and becomes the Active_Tab; a Tab_Switch makes the Matching_Tab the
//!   Active_Tab; either shows a minimized target in its pre-minimize state and
//!   brings it to the foreground, and leaves everything else as it was
//!   (Req 14.6, 14.7, 14.21).
//! - **Bursts** ([`FoldReference::candidate`]): candidates are handled once
//!   each in creation order. The tabs a burst adds count as open tabs for
//!   later candidates, at the right end of their target, in creation order
//!   (Req 14.19), but are not an Active_Tab as far as the foreground exception
//!   of Req 14.22 and the "Active_Tab matches" preference of Req 14.20 are
//!   concerned: those look at the target as it was when the burst began
//!   ([`FoldReference::settle`] ends a burst). The last candidate to touch a
//!   target leaves its tab active.
//!
//! The model covers folds in which no fault interferes: a window that is
//! unresponsive or closing (given in [`RefFoldEnv`]) is passed over exactly as
//! Req 14.13 says, and a candidate with no target is left open. Failures in
//! the middle of a fold (Req 14.23, 14.24) are not modelled.
//! [`FoldReference::check`] compares the expected targets with the simulator.
//!
//! Requirements 2.1, 2.5, 3.3, 14.8, 14.19, 14.20.

use std::collections::BTreeSet;

use esr_core::engine::{DesktopView, ItemId, TabKey, WinKey};
use esr_core::model::{
    LocationId, Placement, SaveCause, ShowState, Snapshot, TabRec, WindowId, WindowRec,
};
use esr_core::tracker::{apply_caps, Excluded};

use crate::explorer::SimExplorer;

/// Whether a window is still open or survived its process's termination.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WindowStatus {
    Open,
    /// Its process terminated without an orderly close (Req 3.3).
    Orphaned,
}

/// One tab of a reference window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefTab {
    /// The tab's current key (it changes when the tab is dragged).
    pub key: TabKey,
    /// Most recent identified Location; `None` if the tab never had one.
    pub location: Option<LocationId>,
}

/// One window of the reference Live_Session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefWindow {
    pub key: WinKey,
    pub pid: u32,
    /// Left-to-right (visual) order, including tabs with no Location yet.
    pub tabs: Vec<RefTab>,
    /// Index into `tabs`.
    pub active: usize,
    pub placement: Placement,
    pub status: WindowStatus,
}

/// A Session_End the tracker is expected to emit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefSessionEnd {
    pub cause: SaveCause,
    /// The snapshot to save (after caps).
    pub snapshot: Snapshot,
}

/// The expected Live_Session.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reference {
    /// Activation_Order, most recent first.
    windows: Vec<RefWindow>,
    session_ends: Vec<RefSessionEnd>,
}

impl Reference {
    pub fn new() -> Self {
        Self::default()
    }

    /// Windows in Activation_Order, most recent first.
    pub fn windows(&self) -> &[RefWindow] {
        &self.windows
    }

    pub fn window(&self, key: WinKey) -> Option<&RefWindow> {
        self.windows.iter().find(|w| w.key == key)
    }

    /// Number of windows that are not orphaned.
    pub fn open_count(&self) -> usize {
        self.windows.iter().filter(|w| w.status == WindowStatus::Open).count()
    }

    /// Every Session_End recorded so far, oldest first.
    pub fn session_ends(&self) -> &[RefSessionEnd] {
        &self.session_ends
    }

    /// Insert or replace a window's state. A new window is appended at the end
    /// of the Activation_Order; an existing one keeps its position.
    pub fn update_window(&mut self, window: RefWindow) {
        match self.windows.iter_mut().find(|w| w.key == window.key) {
            Some(w) => *w = window,
            None => self.windows.push(window),
        }
    }

    /// Move `key` to the front of the Activation_Order.
    pub fn activate(&mut self, key: WinKey) {
        if let Some(i) = self.windows.iter().position(|w| w.key == key) {
            let w = self.windows.remove(i);
            self.windows.insert(0, w);
        }
    }

    /// The user closed `key` (or it closed itself after its last tab was
    /// dragged away). If it was the last open window, a Session_End with
    /// `cause_if_last` and the pre-close snapshot is recorded and the session
    /// is cleared.
    pub fn close_window(&mut self, key: WinKey, cause_if_last: SaveCause) {
        let Some(i) = self.windows.iter().position(|w| w.key == key) else {
            return;
        };
        if self.windows[i].status == WindowStatus::Open && self.open_count() == 1 {
            self.end_session(cause_if_last);
        } else {
            self.windows.remove(i);
        }
    }

    /// Process `pid` terminated without an orderly close of its windows.
    pub fn process_killed(&mut self, pid: u32) {
        let mut orphaned = false;
        for w in self.windows.iter_mut().filter(|w| w.pid == pid && w.status == WindowStatus::Open) {
            w.status = WindowStatus::Orphaned;
            orphaned = true;
        }
        if orphaned && self.open_count() == 0 {
            self.end_session(SaveCause::ExplorerTerminated);
        }
    }

    /// A restoration completed: orphaned windows are dropped (Req 3.3).
    pub fn restoration_completed(&mut self) {
        self.windows.retain(|w| w.status == WindowStatus::Open);
    }

    /// The expected `capture()`: snapshot after caps, plus what the caps excluded.
    pub fn capture(&self) -> (Snapshot, Excluded) {
        let mut windows = Vec::new();
        let mut activation = Vec::new();
        for w in &self.windows {
            let located: Vec<(usize, &LocationId)> = w
                .tabs
                .iter()
                .enumerate()
                .filter_map(|(i, t)| t.location.as_ref().map(|l| (i, l)))
                .collect();
            if located.is_empty() {
                continue;
            }
            let active = located.iter().rposition(|&(i, _)| i <= w.active).unwrap_or(0);
            let id = WindowId(windows.len() as u32);
            windows.push(WindowRec {
                id,
                tabs: located.iter().map(|&(_, l)| TabRec { location: l.clone() }).collect(),
                active,
                placement: w.placement.clone(),
            });
            activation.push(id);
        }
        apply_caps(Snapshot { windows, activation })
    }

    /// The expected snapshot (after caps).
    pub fn snapshot(&self) -> Snapshot {
        self.capture().0
    }

    fn end_session(&mut self, cause: SaveCause) {
        let snapshot = self.snapshot();
        self.session_ends.push(RefSessionEnd { cause, snapshot });
        self.windows.clear();
    }
}

// ---------------------------------------------------------------------------
// Tab folding
// ---------------------------------------------------------------------------

/// The expected Opening_Order (Req 14.8).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RefOpeningOrder {
    /// `(window, ranked)`: a candidate's position is held with `ranked ==
    /// false` until it is left open.
    entries: Vec<(WinKey, bool)>,
}

impl RefOpeningOrder {
    pub fn new() -> Self {
        Self::default()
    }

    /// The ranked windows, earliest first.
    pub fn order(&self) -> Vec<WinKey> {
        self.entries.iter().filter(|e| e.1).map(|e| e.0).collect()
    }

    fn position(&self, win: WinKey) -> Option<usize> {
        self.entries.iter().position(|e| e.0 == win)
    }

    /// A window that is not a candidate opened (or an excluded window's first
    /// signal arrived): it ranks last.
    pub fn opened(&mut self, win: WinKey) {
        if self.position(win).is_none() {
            self.entries.push((win, true));
        }
    }

    /// A candidate's first signal: its position is held, not ranked.
    pub fn candidate_appeared(&mut self, win: WinKey) {
        if self.position(win).is_none() {
            self.entries.push((win, false));
        }
    }

    /// A candidate was left open: it ranks at its first-signal position
    /// (last, if it never had one).
    pub fn candidate_left_open(&mut self, win: WinKey) {
        match self.position(win) {
            Some(i) => self.entries[i].1 = true,
            None => self.entries.push((win, true)),
        }
    }

    /// A window was destroyed, or a candidate was folded away.
    pub fn removed(&mut self, win: WinKey) {
        self.entries.retain(|e| e.0 != win);
    }

    /// Tracking starts or resumes with these windows open, most recently
    /// activated first.
    pub fn tracking_started(&mut self, activation: &[WinKey]) {
        self.entries.clear();
        for &win in activation {
            self.opened(win);
        }
    }

    /// A restoration completed: the trigger, then the restored windows
    /// (saved Activation_Order), rank in front of the windows the user opened
    /// meanwhile; windows that ranked before all of them keep their place.
    pub fn restoration_completed(&mut self, trigger: Option<WinKey>, restored: &[WinKey]) {
        let mut moved: Vec<WinKey> = Vec::new();
        for win in trigger.into_iter().chain(restored.iter().copied()) {
            let ranked = self.entries.iter().any(|e| e.0 == win && e.1);
            if ranked && !moved.contains(&win) {
                moved.push(win);
            }
        }
        let Some(first) = self.entries.iter().position(|e| e.1 && moved.contains(&e.0)) else {
            return;
        };
        let (before, after) = self.entries.split_at(first);
        let mut entries = before.to_vec();
        entries.extend(moved.iter().map(|&w| (w, true)));
        entries.extend(after.iter().filter(|e| !(e.1 && moved.contains(&e.0))).copied());
        self.entries = entries;
    }

    /// The Primary_Window of a desktop: the first ranked window on it.
    pub fn primary(&self, desktop: &DesktopView) -> Option<WinKey> {
        self.order().into_iter().find(|w| match desktop {
            DesktopView::Known(on) => on.contains(w),
            DesktopView::Unknown => true,
        })
    }
}

/// One tab of an expected Fold_Target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefFoldTab {
    /// The Location the tab shows; `None` if it has none that is known (such a
    /// tab matches nothing and is not compared).
    pub location: Option<LocationId>,
    /// The items that must be selected in the tab; `None` when no selection is
    /// required of it.
    pub selection: Option<Vec<ItemId>>,
}

/// The expected state of one Fold_Target: its tabs, Active_Tab, selection and
/// Window_Placement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefFoldTarget {
    pub win: WinKey,
    /// Left to right. Folded_Tabs that are still being created are included.
    pub tabs: Vec<RefFoldTab>,
    /// Index into `tabs`.
    pub active: usize,
    pub placement: Placement,
    /// The number of tabs and the Active_Tab when the current burst began.
    settled_tabs: usize,
    settled_active: usize,
}

impl RefFoldTarget {
    /// A target with these tabs (`None`: a tab with no known Location).
    pub fn new(win: WinKey, tabs: Vec<Option<LocationId>>, active: usize, placement: Placement) -> Self {
        let tabs: Vec<RefFoldTab> = tabs.into_iter().map(|location| RefFoldTab { location, selection: None }).collect();
        let active = active.min(tabs.len().saturating_sub(1));
        RefFoldTarget { win, settled_tabs: tabs.len(), settled_active: active, tabs, active, placement }
    }

    /// The target as `sim` has the window now.
    pub fn of(sim: &SimExplorer, win: WinKey) -> Option<Self> {
        let w = sim.window(win)?;
        let tabs = w.tabs.iter().map(|t| t.last_known.clone()).collect();
        Some(Self::new(win, tabs, w.active, w.placement.clone()))
    }

    /// The target's Active_Tab as of the start of the current burst.
    pub fn settled_active(&self) -> usize {
        self.settled_active
    }

    /// The target is as it is now: the next burst starts from here.
    pub fn settle(&mut self) {
        self.settled_tabs = self.tabs.len();
        self.settled_active = self.active;
    }

    /// The tab Req 14.20 picks in this window for `key`: the Active_Tab of the
    /// start of the burst if it matches, otherwise the leftmost match
    /// (Folded_Tabs of the burst are tabs at the right end).
    fn matching(&self, key: &LocationId) -> Option<usize> {
        let matches = |i: usize| {
            self.tabs[i].location.as_ref().is_some_and(|l| l.fold_key() == key.fold_key())
        };
        if self.settled_active < self.settled_tabs && matches(self.settled_active) {
            return Some(self.settled_active);
        }
        (0..self.tabs.len()).find(|&i| matches(i))
    }

    /// Show a minimized target in its pre-minimize state (Req 14.7).
    fn unminimize(&mut self) {
        if let ShowState::Minimized { restore_maximized } = self.placement.state {
            self.placement.state = if restore_maximized { ShowState::Maximized } else { ShowState::Normal };
        }
    }

    fn require(tab: &mut RefFoldTab, selection: &[ItemId]) {
        if !selection.is_empty() {
            tab.selection = Some(selection.to_vec());
        }
    }

    /// A Folded_Tab showing `location` is added at the right end and becomes
    /// the Active_Tab (Req 14.6, 14.7, 14.19).
    pub fn fold_in(&mut self, location: &LocationId, selection: &[ItemId]) {
        let mut tab = RefFoldTab { location: Some(location.clone()), selection: None };
        Self::require(&mut tab, selection);
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.unminimize();
    }

    /// The tab at `index` becomes the Active_Tab (Req 14.21).
    pub fn switch_to(&mut self, index: usize, selection: &[ItemId]) {
        if let Some(tab) = self.tabs.get_mut(index) {
            Self::require(tab, selection);
            self.active = index;
        }
        self.unminimize();
    }
}

/// A Folding_Candidate as the model sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefCandidate {
    pub win: WinKey,
    /// The Location of its first navigation.
    pub location: LocationId,
    /// The items it was opened to select; empty if none.
    pub selection: Vec<ItemId>,
    /// Its Foreground_Explorer_Window (Req 14.22).
    pub foreground: Option<WinKey>,
}

/// What is true of the windows when a candidate chooses its Fold_Target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefFoldEnv {
    /// The windows on the candidate's Virtual_Desktop (Req 14.11, 14.12).
    pub desktop: DesktopView,
    /// Every tracked window, most recently activated first.
    pub activation: Vec<WinKey>,
    /// Windows that do not answer `WM_NULL` in time (Req 14.13).
    pub unresponsive: BTreeSet<WinKey>,
    /// Windows that have begun closing (Req 14.13).
    pub closing: BTreeSet<WinKey>,
}

impl Default for RefFoldEnv {
    fn default() -> Self {
        RefFoldEnv {
            desktop: DesktopView::Unknown,
            activation: Vec::new(),
            unresponsive: BTreeSet::new(),
            closing: BTreeSet::new(),
        }
    }
}

impl RefFoldEnv {
    /// The environment of `candidate` in `sim`: its desktop view, the
    /// Activation_Order of the reference Live_Session and the unresponsive
    /// windows. No window is closing (a closing window has left `sim`).
    pub fn of(sim: &SimExplorer, candidate: WinKey) -> Self {
        RefFoldEnv {
            desktop: sim.desktop_view_of(candidate),
            activation: sim.reference().windows().iter().map(|w| w.key).collect(),
            unresponsive: sim.windows().iter().map(|w| w.key).filter(|w| sim.is_unresponsive(*w)).collect(),
            closing: BTreeSet::new(),
        }
    }
}

/// What the model expects to become of a candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RefFoldOutcome {
    /// Tab_Switch: the tab at `index` of `win` became the Active_Tab.
    Switched { win: WinKey, index: usize },
    /// A Folded_Tab was created at `index` of `win`.
    Folded { win: WinKey, index: usize },
    /// There was no Fold_Target: the candidate stays open (Req 14.14).
    LeftOpen,
}

/// The expected result of Tab_Folding: Fold_Targets and the Opening_Order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FoldReference {
    opening: RefOpeningOrder,
    /// Tracked windows, in the order they were added.
    targets: Vec<RefFoldTarget>,
    /// The window the last fold or switch brought to the foreground.
    foreground: Option<WinKey>,
}

impl FoldReference {
    pub fn new() -> Self {
        Self::default()
    }

    /// The open windows of `sim` that are not candidates, ranked in the
    /// Activation_Order of its reference Live_Session (tracking has just
    /// started).
    pub fn of(sim: &SimExplorer) -> Self {
        let open: Vec<WinKey> =
            sim.windows().iter().map(|w| w.key).filter(|w| sim.candidate(*w).is_none()).collect();
        let mut activation: Vec<WinKey> =
            sim.reference().windows().iter().map(|w| w.key).filter(|w| open.contains(w)).collect();
        let unlisted: Vec<WinKey> = open.iter().copied().filter(|w| !activation.contains(w)).collect();
        activation.extend(unlisted);
        let mut reference = FoldReference::new();
        reference.opening.tracking_started(&activation);
        reference.targets = open.iter().filter_map(|&w| RefFoldTarget::of(sim, w)).collect();
        reference
    }

    pub fn opening(&self) -> &RefOpeningOrder {
        &self.opening
    }

    pub fn opening_mut(&mut self) -> &mut RefOpeningOrder {
        &mut self.opening
    }

    /// The expected Opening_Order, earliest first.
    pub fn opening_order(&self) -> Vec<WinKey> {
        self.opening.order()
    }

    pub fn targets(&self) -> &[RefFoldTarget] {
        &self.targets
    }

    pub fn target(&self, win: WinKey) -> Option<&RefFoldTarget> {
        self.targets.iter().find(|t| t.win == win)
    }

    /// A tracked window opened that is not a candidate.
    pub fn window_opened(&mut self, target: RefFoldTarget) {
        self.opening.opened(target.win);
        self.targets.push(target);
    }

    /// A tracked window was destroyed.
    pub fn window_closed(&mut self, win: WinKey) {
        self.opening.removed(win);
        self.targets.retain(|t| t.win != win);
    }

    /// The window the last fold or switch brought to the foreground.
    pub fn foreground(&self) -> Option<WinKey> {
        self.foreground
    }

    /// The burst is over: every target is as it is now.
    pub fn settle(&mut self) {
        self.targets.iter_mut().for_each(RefFoldTarget::settle);
    }

    /// A candidate's first signal.
    pub fn candidate_appeared(&mut self, win: WinKey) {
        self.opening.candidate_appeared(win);
    }

    /// Choose the Fold_Target of `c` as Req 14.9, 14.13, 14.20–14.22 say and
    /// apply the result. Candidates must be passed in creation order.
    pub fn candidate(&mut self, c: &RefCandidate, env: &RefFoldEnv) -> RefFoldOutcome {
        let outcome = self.choose(c, env);
        match outcome {
            RefFoldOutcome::LeftOpen => self.opening.candidate_left_open(c.win),
            RefFoldOutcome::Switched { win, index } => {
                self.opening.removed(c.win);
                self.apply(win, |t| t.switch_to(index, &c.selection));
            }
            RefFoldOutcome::Folded { win, .. } => {
                self.opening.removed(c.win);
                self.apply(win, |t| t.fold_in(&c.location, &c.selection));
            }
        }
        outcome
    }

    fn apply(&mut self, win: WinKey, f: impl FnOnce(&mut RefFoldTarget)) {
        if let Some(t) = self.targets.iter_mut().find(|t| t.win == win) {
            f(t);
            self.foreground = Some(win);
        }
    }

    fn choose(&self, c: &RefCandidate, env: &RefFoldEnv) -> RefFoldOutcome {
        let order = self.opening.order();
        let rank = |list: &[WinKey], w: WinKey| list.iter().position(|&x| x == w).unwrap_or(usize::MAX);
        let on_desktop = |w: WinKey| match &env.desktop {
            DesktopView::Known(set) => set.contains(&w),
            DesktopView::Unknown => true,
        };
        let usable = |w: WinKey| !env.closing.contains(&w) && !env.unresponsive.contains(&w);

        let mut eligible: Vec<&RefFoldTarget> = self.targets.iter().filter(|t| on_desktop(t.win)).collect();
        eligible.sort_by_key(|t| rank(&order, t.win));
        let Some(&primary) = eligible.first() else { return RefFoldOutcome::LeftOpen };
        let mut fallbacks: Vec<&RefFoldTarget> =
            eligible.iter().copied().filter(|t| t.win != primary.win && usable(t.win)).collect();
        fallbacks.sort_by_key(|t| rank(&env.activation, t.win));

        let searched: Vec<&RefFoldTarget> =
            usable(primary.win).then_some(primary).into_iter().chain(fallbacks.iter().copied()).collect();
        for t in &searched {
            if let Some(index) = t.matching(&c.location) {
                let foreground_active =
                    c.foreground == Some(t.win) && index == t.settled_active && index < t.settled_tabs;
                return if foreground_active {
                    RefFoldOutcome::Folded { win: t.win, index: t.tabs.len() }
                } else {
                    RefFoldOutcome::Switched { win: t.win, index }
                };
            }
        }
        match searched.first() {
            Some(t) => RefFoldOutcome::Folded { win: t.win, index: t.tabs.len() },
            None => RefFoldOutcome::LeftOpen,
        }
    }

    /// Compare the expected targets with the windows of `sim`: tabs (by
    /// `fold_key`), Active_Tab, required selections, show state and restored
    /// bounds, and the foreground window. Call it when the folds have
    /// completed and nothing else has changed the windows since.
    pub fn check(&self, sim: &SimExplorer) -> Result<(), String> {
        for t in &self.targets {
            let w = sim.window(t.win).ok_or_else(|| format!("{:?} is not open", t.win))?;
            if w.tabs.len() != t.tabs.len() {
                return Err(format!("{:?}: {} tabs, expected {}", t.win, w.tabs.len(), t.tabs.len()));
            }
            for (i, (seen, want)) in w.tabs.iter().zip(&t.tabs).enumerate() {
                if let Some(want_location) = &want.location {
                    let same = seen.last_known.as_ref().is_some_and(|l| l.fold_key() == want_location.fold_key());
                    if !same {
                        return Err(format!("{:?}: tab {i} shows {:?}, expected {want_location:?}", t.win, seen.last_known));
                    }
                }
                if let Some(items) = &want.selection {
                    let sorted = |v: &[ItemId]| {
                        let mut v = v.to_vec();
                        v.sort_by(|a, b| a.0.cmp(&b.0));
                        v
                    };
                    if sorted(sim.selection(seen.key)) != sorted(items) {
                        return Err(format!("{:?}: tab {i} selects {:?}, expected {items:?}", t.win, sim.selection(seen.key)));
                    }
                }
            }
            if w.active != t.active {
                return Err(format!("{:?}: Active_Tab {}, expected {}", t.win, w.active, t.active));
            }
            if w.placement.state != t.placement.state || w.placement.restored != t.placement.restored {
                return Err(format!("{:?}: placement {:?}, expected {:?}", t.win, w.placement, t.placement));
            }
        }
        if let Some(front) = self.foreground
            && sim.foreground() != Some(front)
        {
            return Err(format!("foreground is {:?}, expected {front:?}", sim.foreground()));
        }
        Ok(())
    }
}

/// `snapshot` with its windows listed in Activation_Order and renumbered
/// `0, 1, …` in that order, so snapshots whose window ids were assigned
/// differently compare equal. Windows missing from `activation` follow, in
/// their original order.
pub fn canonical(snapshot: &Snapshot) -> Snapshot {
    let mut order: Vec<usize> = Vec::with_capacity(snapshot.windows.len());
    for id in &snapshot.activation {
        if let Some(i) = snapshot.windows.iter().position(|w| w.id == *id)
            && !order.contains(&i)
        {
            order.push(i);
        }
    }
    let unlisted: Vec<usize> = (0..snapshot.windows.len()).filter(|i| !order.contains(i)).collect();
    order.extend(unlisted);
    let windows: Vec<WindowRec> = order
        .iter()
        .enumerate()
        .map(|(n, &i)| WindowRec { id: WindowId(n as u32), ..snapshot.windows[i].clone() })
        .collect();
    let listed = order.iter().filter(|&&i| snapshot.activation.contains(&snapshot.windows[i].id)).count();
    let activation = (0..listed as u32).map(WindowId).collect();
    Snapshot { windows, activation }
}

#[cfg(test)]
mod tests {
    use super::*;
    use esr_core::model::{validate, DisplayRef, LocationKind, Rect, ShowState, Volume};

    fn loc(name: &str) -> LocationId {
        LocationId {
            case_sensitive: false,
            kind: LocationKind::FileSystem(Volume::Fixed),
            parsing_name: format!(r"C:\{name}").as_str().into(),
            display_name: name.into(),
            pidl: vec![],
            search: None,
        }
    }

    fn placement() -> Placement {
        let r = Rect { left: 0, top: 0, right: 800, bottom: 600 };
        Placement { state: ShowState::Normal, restored: r, display: DisplayRef { device: "D1".into(), monitor: r } }
    }

    fn win(n: u64, pid: u32, tabs: &[Option<&str>], active: usize) -> RefWindow {
        RefWindow {
            key: WinKey { hwnd: n, seq: n },
            pid,
            tabs: tabs
                .iter()
                .enumerate()
                .map(|(i, l)| RefTab { key: TabKey { hwnd: 100 * n + i as u64, seq: i as u64 }, location: l.map(loc) })
                .collect(),
            active,
            placement: placement(),
            status: WindowStatus::Open,
        }
    }

    fn names(s: &Snapshot) -> Vec<Vec<String>> {
        canonical(s)
            .windows
            .iter()
            .map(|w| w.tabs.iter().map(|t| t.location.display_name.to_string_lossy()).collect())
            .collect()
    }

    #[test]
    fn snapshot_follows_activation_and_leaves_out_unlocated_tabs() {
        let mut r = Reference::new();
        r.update_window(win(1, 7, &[Some("a"), None, Some("b")], 2));
        r.update_window(win(2, 7, &[None], 0));
        r.update_window(win(3, 7, &[Some("c")], 0));
        r.activate(WinKey { hwnd: 3, seq: 3 });
        let s = r.snapshot();
        assert_eq!(validate(&s), Ok(()));
        // Window 2 has no located tab and is absent.
        assert_eq!(names(&s), vec![vec!["c".to_string()], vec!["a".into(), "b".into()]]);
        assert_eq!(canonical(&s).windows[1].active, 1);
    }

    #[test]
    fn unlocated_active_tab_falls_back_left_then_leftmost() {
        let mut r = Reference::new();
        r.update_window(win(1, 7, &[Some("a"), Some("b"), None, Some("c")], 2));
        r.update_window(win(2, 7, &[None, Some("x"), Some("y")], 0));
        let s = canonical(&r.snapshot());
        assert_eq!(s.windows[0].active, 1, "nearest located tab to the left");
        assert_eq!(s.windows[1].active, 0, "leftmost located tab");
    }

    #[test]
    fn last_window_close_records_pre_close_session_end() {
        let mut r = Reference::new();
        r.update_window(win(1, 7, &[Some("a")], 0));
        r.update_window(win(2, 7, &[Some("b"), Some("c")], 1));
        r.close_window(WinKey { hwnd: 1, seq: 1 }, SaveCause::LastTabClosed);
        assert!(r.session_ends().is_empty());
        let before = r.snapshot();
        r.close_window(WinKey { hwnd: 2, seq: 2 }, SaveCause::LastWindowClosed);
        assert_eq!(r.session_ends(), &[RefSessionEnd { cause: SaveCause::LastWindowClosed, snapshot: before }]);
        assert!(r.windows().is_empty());
    }

    #[test]
    fn process_kill_orphans_and_ends_when_nothing_open_remains() {
        let mut r = Reference::new();
        r.update_window(win(1, 7, &[Some("a")], 0));
        r.update_window(win(2, 8, &[Some("b")], 0));
        let before = r.snapshot();
        r.process_killed(7);
        assert_eq!(r.window(WinKey { hwnd: 1, seq: 1 }).unwrap().status, WindowStatus::Orphaned);
        assert_eq!(r.snapshot(), before, "orphans stay in the snapshot");
        assert!(r.session_ends().is_empty());
        r.process_killed(8);
        assert_eq!(r.session_ends(), &[RefSessionEnd { cause: SaveCause::ExplorerTerminated, snapshot: before }]);
        assert_eq!(r.snapshot(), Snapshot::default());
    }

    #[test]
    fn closing_last_open_window_includes_orphans_and_drops_them() {
        let mut r = Reference::new();
        r.update_window(win(1, 7, &[Some("a")], 0));
        r.update_window(win(2, 8, &[Some("b")], 0));
        r.process_killed(7);
        r.close_window(WinKey { hwnd: 2, seq: 2 }, SaveCause::LastTabClosed);
        assert_eq!(r.session_ends().len(), 1);
        assert_eq!(r.session_ends()[0].snapshot.windows.len(), 2);
        assert!(r.windows().is_empty());
    }

    #[test]
    fn restoration_drops_orphans() {
        let mut r = Reference::new();
        r.update_window(win(1, 7, &[Some("a")], 0));
        r.update_window(win(2, 8, &[Some("b")], 0));
        r.process_killed(7);
        r.restoration_completed();
        assert_eq!(r.windows().len(), 1);
    }

    #[test]
    fn canonical_renumbers_by_activation() {
        let s = Snapshot {
            windows: vec![
                WindowRec { id: WindowId(9), tabs: vec![TabRec { location: loc("a") }], active: 0, placement: placement() },
                WindowRec { id: WindowId(4), tabs: vec![TabRec { location: loc("b") }], active: 0, placement: placement() },
            ],
            activation: vec![WindowId(4), WindowId(9)],
        };
        let c = canonical(&s);
        assert_eq!(c.activation, vec![WindowId(0), WindowId(1)]);
        assert_eq!(names(&c), vec![vec!["b".to_string()], vec!["a".to_string()]]);
        assert_eq!(canonical(&c), c);
    }

    // --- Tab folding -----------------------------------------------------

    fn wk(n: u64) -> WinKey {
        WinKey { hwnd: n, seq: n }
    }

    fn target(n: u64, tabs: &[&str], active: usize) -> RefFoldTarget {
        RefFoldTarget::new(wk(n), tabs.iter().map(|t| Some(loc(t))).collect(), active, placement())
    }

    fn candidate(n: u64, name: &str, foreground: Option<u64>) -> RefCandidate {
        RefCandidate { win: wk(n), location: loc(name), selection: vec![], foreground: foreground.map(wk) }
    }

    fn reference_of(targets: Vec<RefFoldTarget>) -> FoldReference {
        let mut r = FoldReference::new();
        for t in targets {
            r.window_opened(t);
        }
        r
    }

    fn keys(ns: &[u64]) -> Vec<WinKey> {
        ns.iter().map(|&n| wk(n)).collect()
    }

    #[test]
    fn opening_order_ranks_by_opening_and_moves_restored_windows_forward() {
        let mut o = RefOpeningOrder::new();
        o.tracking_started(&keys(&[3, 1]));
        o.opened(wk(5));
        o.candidate_appeared(wk(6));
        o.opened(wk(7));
        assert_eq!(o.order(), keys(&[3, 1, 5, 7]), "a candidate holds a place but is not ranked");
        o.candidate_left_open(wk(6));
        assert_eq!(o.order(), keys(&[3, 1, 5, 6, 7]), "left open: at its first-signal position");
        o.removed(wk(1));
        o.restoration_completed(Some(wk(7)), &keys(&[6]));
        assert_eq!(o.order(), keys(&[3, 5, 7, 6]), "moved windows take the place of the earliest of them");
        let again = o.clone();
        o.restoration_completed(Some(wk(7)), &keys(&[6]));
        assert_eq!(o, again);
    }

    #[test]
    fn primary_window_is_the_first_ranked_window_on_the_desktop() {
        let mut o = RefOpeningOrder::new();
        o.tracking_started(&keys(&[4, 2, 9]));
        let on = |ns: &[u64]| DesktopView::Known(ns.iter().map(|&n| wk(n)).collect());
        assert_eq!(o.primary(&on(&[9, 2])), Some(wk(2)));
        assert_eq!(o.primary(&on(&[7])), None);
        assert_eq!(o.primary(&DesktopView::Unknown), Some(wk(4)));
    }

    #[test]
    fn a_burst_folds_in_creation_order_and_the_last_candidate_leaves_its_tab_active() {
        let mut r = reference_of(vec![target(1, &["a", "b"], 0), target(2, &["c"], 0)]);
        let env = RefFoldEnv { activation: keys(&[2, 1]), ..RefFoldEnv::default() };
        let cands = [candidate(10, "x", None), candidate(11, "y", None), candidate(12, "X", None), candidate(13, "b", None)];
        for c in &cands {
            r.candidate_appeared(c.win);
        }
        let got: Vec<_> = cands.iter().map(|c| r.candidate(c, &env)).collect();
        assert_eq!(
            got,
            vec![
                RefFoldOutcome::Folded { win: wk(1), index: 2 },
                RefFoldOutcome::Folded { win: wk(1), index: 3 },
                RefFoldOutcome::Switched { win: wk(1), index: 2 }, // equal to the pending tab of 10
                RefFoldOutcome::Switched { win: wk(1), index: 1 },
            ]
        );
        let t = r.target(wk(1)).unwrap();
        assert_eq!(t.tabs.len(), 4);
        assert_eq!(t.active, 1, "the last-created candidate's tab is the Active_Tab");
        assert_eq!(r.opening_order(), keys(&[1, 2]), "folded candidates are not in the order");
        assert_eq!(r.foreground(), Some(wk(1)));
        r.settle();
        assert_eq!(r.target(wk(1)).unwrap().settled_active(), 1);
    }

    #[test]
    fn selection_is_required_of_the_tab_that_shows_the_location() {
        let mut r = reference_of(vec![target(1, &["a", "b"], 0)]);
        let items = vec![ItemId(vec![2]), ItemId(vec![1])];
        let c = RefCandidate { selection: items.clone(), ..candidate(10, "b", None) };
        assert_eq!(r.candidate(&c, &RefFoldEnv::default()), RefFoldOutcome::Switched { win: wk(1), index: 1 });
        let t = r.target(wk(1)).unwrap();
        assert_eq!((t.tabs[1].selection.clone(), t.tabs[0].selection.clone()), (Some(items), None));
    }

    #[test]
    fn matching_prefers_the_active_tab_then_the_leftmost() {
        let mut r = reference_of(vec![target(1, &["a", "b", "a"], 2)]);
        let env = RefFoldEnv::default();
        assert_eq!(r.candidate(&candidate(10, "a", None), &env), RefFoldOutcome::Switched { win: wk(1), index: 2 });
        r.settle();
        r.targets[0].active = 1;
        r.settle();
        assert_eq!(r.candidate(&candidate(11, "A", None), &env), RefFoldOutcome::Switched { win: wk(1), index: 0 });
    }

    #[test]
    fn the_foreground_window_folds_instead_of_switching_to_its_own_active_tab() {
        let mut r = reference_of(vec![target(1, &["a", "b"], 0), target(2, &["c"], 0)]);
        let env = RefFoldEnv { activation: keys(&[2, 1]), ..RefFoldEnv::default() };
        // "a" is the Active_Tab of the foreground window 1.
        let got = r.candidate(&candidate(10, "a", Some(1)), &env);
        assert_eq!(got, RefFoldOutcome::Folded { win: wk(1), index: 2 });
        assert_eq!(r.target(wk(1)).unwrap().active, 2);
        // "b" is a Matching_Tab but not the Active_Tab of the foreground window.
        let got = r.candidate(&candidate(11, "b", Some(1)), &env);
        assert_eq!(got, RefFoldOutcome::Switched { win: wk(1), index: 1 });
        // The tab the burst folded in is not an Active_Tab as far as the exception goes.
        let got = r.candidate(&candidate(12, "a", Some(1)), &env);
        assert_eq!(got, RefFoldOutcome::Folded { win: wk(1), index: 3 });
    }

    #[test]
    fn unresponsive_and_closing_windows_are_passed_over() {
        let mut r = reference_of(vec![target(1, &["a"], 0), target(2, &["b"], 0), target(3, &["c"], 0)]);
        let mut env = RefFoldEnv { activation: keys(&[3, 2, 1]), ..RefFoldEnv::default() };
        env.unresponsive.insert(wk(1));
        // The Primary_Window is not searched: "a" has no Matching_Tab.
        let got = r.candidate(&candidate(10, "a", None), &env);
        assert_eq!(got, RefFoldOutcome::Folded { win: wk(3), index: 1 }, "first fallback in Activation_Order");
        env.closing.insert(wk(3));
        assert_eq!(r.candidate(&candidate(11, "d", None), &env), RefFoldOutcome::Folded { win: wk(2), index: 1 });
        env.closing.insert(wk(2));
        assert_eq!(r.candidate(&candidate(12, "e", None), &env), RefFoldOutcome::LeftOpen);
        assert_eq!(r.opening_order(), keys(&[1, 2, 3, 12]), "left open: ranked after the others");
    }

    #[test]
    fn only_windows_on_the_candidates_desktop_are_targets() {
        let mut r = reference_of(vec![target(1, &["a"], 0), target(2, &["b"], 0)]);
        let env = RefFoldEnv { desktop: DesktopView::Known([wk(2)].into()), activation: keys(&[1, 2]), ..RefFoldEnv::default() };
        assert_eq!(r.candidate(&candidate(10, "a", None), &env), RefFoldOutcome::Folded { win: wk(2), index: 1 });
        let none = RefFoldEnv { desktop: DesktopView::Known(BTreeSet::new()), ..env };
        assert_eq!(r.candidate(&candidate(11, "z", None), &none), RefFoldOutcome::LeftOpen);
    }

    #[test]
    fn a_minimized_target_is_shown_in_its_pre_minimize_state() {
        for (restore_maximized, expected) in [(true, ShowState::Maximized), (false, ShowState::Normal)] {
            let mut t = target(1, &["a"], 0);
            t.placement.state = ShowState::Minimized { restore_maximized };
            let mut folded = t.clone();
            folded.fold_in(&loc("b"), &[]);
            assert_eq!(folded.placement.state, expected);
            assert_eq!(folded.placement.restored, t.placement.restored);
            t.switch_to(0, &[]);
            assert_eq!(t.placement.state, expected);
        }
        let mut shown = target(1, &["a"], 0);
        shown.placement.state = ShowState::Maximized;
        shown.fold_in(&loc("b"), &[]);
        assert_eq!(shown.placement.state, ShowState::Maximized, "unchanged");
    }

    #[test]
    fn check_compares_the_model_with_the_simulator() {
        use crate::explorer::{NavOutcome, Op, Pick, ProcessChoice, SimConfig};
        let mut sim = SimExplorer::new(SimConfig::default());
        for n in 0..2u16 {
            sim.apply(&Op::OpenWindow {
                process: ProcessChoice::Main,
                location: Pick(n),
                nav: NavOutcome::Complete,
                rect: esr_core::model::Rect { left: 100, top: 100, right: 900, bottom: 700 },
                maximized: false,
            });
        }
        sim.settle();
        let mut r = FoldReference::of(&sim);
        assert_eq!(r.targets().len(), 2);
        assert_eq!(r.opening_order().len(), 2);
        assert_eq!(r.check(&sim), Ok(()));
        let first = r.targets()[0].win;
        r.targets[0].fold_in(&loc("elsewhere"), &[]);
        assert!(r.check(&sim).unwrap_err().contains("tabs"));
        r.targets[0].tabs.pop();
        r.targets[0].active = 0;
        assert_eq!(r.check(&sim), Ok(()));
        r.foreground = Some(first);
        assert!(r.check(&sim).is_ok() == (sim.foreground() == Some(first)));
    }
}
