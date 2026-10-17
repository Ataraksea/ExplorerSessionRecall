//! `LiveSession`, `LiveWindow`, `LiveTab`, and the `LiveSessionTracker`
//! (close coalescing, session end detection, orphaning, freeze, `capture`).
//!
//! Task 7.4 implements the core: window rescans through the
//! [`TabOrderReconciler`], drag-out/drag-in, Activation_Order, window and tab
//! close while other windows or tabs remain, completed-navigation-only
//! Locations, unidentifiable Locations, and [`LiveSessionTracker::capture`].
//! Task 7.5 adds close coalescing ([`TOMBSTONE_MS`] tombstones) and
//! Session_End detection for the last window or tab closed
//! ([`TrackerOutput::session_end`]). Task 7.6 adds process termination
//! (orphaning), shell loss, and the session-end freeze.
//!
//! # Rules (design.md "Live session tracking", esr-sim `reference.rs`)
//!
//! - Windows and tabs are keyed by [`WinKey`]/[`TabKey`], whose sequence
//!   numbers keep a reused HWND from aliasing an earlier window or tab.
//! - A `WindowScanned` is authoritative for the window's tab set: scanned tabs
//!   are ordered by the reconciler, tabs not listed are dropped. A scan with no
//!   tabs at all (a drag-out window before its tab registers) never removes
//!   the window or its tabs; it only updates PID and placement.
//! - A tab key listed by a window other than the one holding it moved there.
//!   A dragged tab whose HWND is recreated arrives as a new key, the old key
//!   being revoked. A source window left with no tabs is no longer captured
//!   (Req 2.4) and is removed at its `WindowDestroyed`: until then it may still
//!   hold tabs that registered after its last scan.
//! - The Active_Tab is the tab of the UIA selection (`Reconciled::selected`).
//!   When the selected item matches no tab, the nearest matched tab to its
//!   left stands in (or "leftmost" if there is none). Without a UIA selection
//!   it is `WindowObservation::active` if set, else the previous Active_Tab.
//! - A Location is recorded only from a completed navigation: an
//!   `ObservedLocation::Known` scan or `NavigationCompleted` (Req 2.2, 8.1).
//!   An unidentifiable Location keeps the last identifier, or leaves the tab
//!   out if it never had one, and logs `UNIDENTIFIABLE-LOCATION` once per
//!   transition into the unidentified state (Req 8.8).
//! - `WindowActivated` moves the window to the front of the Activation_Order;
//!   it may arrive before the window's first scan and is remembered. A newly
//!   scanned window that was never activated goes last.
//! - `TabRevoked` does not remove the tab at once: it holds a tombstone for
//!   [`TOMBSTONE_MS`] (design.md "Close coalescing") and sets a tracker timer
//!   for its expiry. A tombstoned tab stays in the Live_Session and in
//!   `capture()`, keeps its position across rescans that no longer list it,
//!   and ignores later navigations. When the tombstone expires (at the timer,
//!   or lazily at the first event at or after the deadline) the tab is removed
//!   and the tab Explorer shows next becomes active if it was active
//!   (Req 2.8).
//! - `WindowDestroyed` removes the window with all its tabs, tombstoned ones
//!   included (Req 2.7). If it was the last `Open` window, a Session_End is
//!   emitted instead, with the capture taken before the removal (so including
//!   the tombstoned tabs, Req 3.1, 3.2). The cause is `LastTabClosed` when
//!   the window had exactly one tab and exactly one tab was tombstoned,
//!   otherwise `LastWindowClosed`. While Folding_Candidates are in progress
//!   ([`LiveSessionTracker::set_candidates_in_progress`]) the candidate is an
//!   open window the tracker does not know, so closing the last tracked
//!   window, or the termination of the process hosting the last open ones, is
//!   an ordinary close or orphaning and no Session_End (Req 14.15, 14.28).
//!   Every Session_End drops all windows,
//!   orphans included (esr-sim `reference.rs`). A window activated but not
//!   scanned yet counts as a remaining open window.
//! - Known limitation of the coalescing: a tab closed by the user less than
//!   300 ms before its window closes counts as part of the window close.
//! - `ProcessExited(pid)` (sent 200 ms after the process handle signals, so
//!   the WinEvents of an orderly close come first) ends the windows of `pid`
//!   that are still `Open`. A window whose tabs were all revoked, or that has
//!   no tab, had an orderly close and is closed as by `WindowDestroyed`. Any
//!   other window becomes `Orphaned`: it keeps its tabs, Locations,
//!   Active_Tab, placement, and Activation_Order position and stays in
//!   `capture()` (Req 3.3). If no `Open` window remains (an activated but
//!   unscanned window counts as open, as above), a Session_End with cause
//!   `ExplorerTerminated` and the pre-termination capture is emitted
//!   (Req 3.4). Orphans are dropped at the next Session_End or by
//!   [`LiveSessionTracker::drop_orphans`] at a completed restoration.
//!   Every `ProcessExited` also stops counting the windows activated but
//!   never scanned as open: their PID is unknown, and one whose process is
//!   gone would otherwise count as open forever (no scan, no
//!   `WindowDestroyed`). They keep their Activation_Order position for a
//!   later first scan (a window opened while the shell was lost). A window
//!   activated within [`PROCESS_EXIT_DELAY_MS`] before the `ProcessExited`
//!   was activated after that process ended and keeps counting as open.
//! - An orphan's window is already gone, so a later `WindowDestroyed` or
//!   `TabRevoked` for it is an artefact of the termination and is ignored.
//! - Adapter rule (task 14.6, not decidable here): the system may report a
//!   killed process's windows with WinEvent DESTROYs, which would look like
//!   orderly closes and remove those windows one by one before
//!   `ProcessExited`. A destroy arrives with no revocation and before
//!   `ProcessExited`, which is exactly what an orderly close whose tabs are
//!   revoked after the destroy looks like, so the tracker cannot tell them
//!   apart without delaying every close. The window event source must
//!   therefore not report `WindowDestroyed` for a window whose process has
//!   signalled (or signals within its 200 ms `ProcessExited` delay) unless
//!   ShellWindows revoked one of the window's tabs first.
//! - `ShellLost` leaves the Live_Session unchanged (Req 1.10); WinEvents still
//!   apply while the shell is lost. `ShellAvailable` emits
//!   [`Command::RescanAll`] so every open window, including windows opened
//!   meanwhile, is rescanned (Req 1.7, 2.9).
//! - `EndSessionQuery(kind)` freezes the Live_Session and, if any window is
//!   tracked, emits a Session_End with cause `SignOut`, `Shutdown`, or
//!   `Restart` and the current capture (Req 3.5); with no window tracked
//!   nothing is emitted (Req 3.11). Unlike the other Session_Ends, the
//!   Live_Session is kept. While frozen every event is ignored (a repeated
//!   query re-emits the frozen capture) except `WindowDestroyed` and
//!   `ProcessExited`, which are deferred. `EndSessionCancelled` unfreezes,
//!   expires due tombstones, drops the windows the aborted session end
//!   closed (destroyed, or of an exited process) without a new Session_End
//!   (the query's already saved them), drops the orphans too if no `Open`
//!   window remains, and emits [`Command::RescanAll`] to catch up with the
//!   changes ignored meanwhile.
//! - [`LiveSessionTracker::capture`] lists windows by Activation_Order with
//!   ids `0, 1, …`, leaves out tabs with no Location and windows with no such
//!   tab, moves an unlocated Active_Tab to the nearest located tab on its left
//!   (else the leftmost), and applies the caps last (Req 2.10).
//!
//! Known gap (for task 7.13): a dragged tab that currently shows an
//! unidentifiable Location arrives under a new key with no UIA-matchable
//! name, so it is not paired with its revoked predecessor and loses the kept
//! identifier.
//!
//! Requirements 1.7, 1.10, 2.1–2.5, 2.7–2.10, 3.1–3.5, 3.11, 8.1, 8.8.

use std::collections::{BTreeMap, VecDeque};

use crate::engine::{
    Command, EndKind, Event, Millis, ObservedLocation, TabKey, TabObservation, TimerId, TimerOwner,
    WinKey, WindowObservation,
};
use crate::fold::FoldWindow;
use crate::logfmt::LogEntry;
use crate::model::{LocationId, Placement, SaveCause, Snapshot, TabRec, WindowId, WindowRec};
use crate::tracker::caps::apply_caps;
use crate::tracker::reconcile::{ScannedTab, TabOrderReconciler};

/// How long a revoked tab is held as a tombstone (design.md "Close coalescing").
pub const TOMBSTONE_MS: u64 = 300;

/// Delay between a process ending and its `ProcessExited` (adapter contract:
/// `esr-win::window_event_source::PROCESS_EXIT_DELAY_MS`). A window activated
/// less than this long before a `ProcessExited` was activated after that
/// process had ended, so it cannot be one of its windows.
pub const PROCESS_EXIT_DELAY_MS: u64 = 200;

/// Most `NavigationCompleted` events remembered for tabs not yet scanned.
const MAX_PENDING_NAVIGATIONS: usize = 256;
/// Most recently closed windows remembered, so late scans do not resurrect them.
const MAX_RECENTLY_CLOSED: usize = 64;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Whether a window is still open or survived its process's termination.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum WindowStatus {
    #[default]
    Open,
    /// Its process terminated without an orderly close (Req 3.3, task 7.6).
    Orphaned,
}

/// One tab of the Live_Session.
#[derive(Clone, Debug)]
pub struct LiveTab {
    key: TabKey,
    /// Most recent identified Location; `None` until the first completed,
    /// identified navigation (excluded from capture, Req 8.8).
    location: Option<LocationId>,
    /// The latest completed navigation had no Location_Identifier.
    unidentified: bool,
    /// Close-coalescing tombstone (task 7.5); `None` while the tab is live.
    tombstone_until: Option<Millis>,
}

impl LiveTab {
    fn new(key: TabKey) -> Self {
        LiveTab { key, location: None, unidentified: false, tombstone_until: None }
    }

    pub fn key(&self) -> TabKey {
        self.key
    }

    pub fn location(&self) -> Option<&LocationId> {
        self.location.as_ref()
    }

    /// The tab's latest completed navigation could not be identified.
    pub fn unidentified(&self) -> bool {
        self.unidentified
    }

    pub fn tombstone_until(&self) -> Option<Millis> {
        self.tombstone_until
    }

    /// The tab was revoked and is held until its tombstone expires.
    pub fn is_tombstoned(&self) -> bool {
        self.tombstone_until.is_some()
    }

    /// Field-by-field equality (`LocationId`'s `==` ignores PIDL bytes and
    /// display names, which still matter for what is saved).
    fn same_as(&self, other: &LiveTab) -> bool {
        self.key == other.key
            && self.unidentified == other.unidentified
            && self.tombstone_until == other.tombstone_until
            && match (&self.location, &other.location) {
                (None, None) => true,
                (Some(a), Some(b)) => a.identical(b),
                _ => false,
            }
    }

    fn set_location(&mut self, location: &LocationId) {
        self.location = Some(location.clone());
        self.unidentified = false;
    }
}

/// One Explorer window of the Live_Session.
#[derive(Clone, Debug)]
pub struct LiveWindow {
    key: WinKey,
    pid: u32,
    /// Reconciled left-to-right order.
    tabs: Vec<LiveTab>,
    /// `None` means "the leftmost tab".
    active: Option<TabKey>,
    placement: Placement,
    status: WindowStatus,
}

impl LiveWindow {
    pub fn key(&self) -> WinKey {
        self.key
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Tabs left to right, including tabs with no Location yet.
    pub fn tabs(&self) -> &[LiveTab] {
        &self.tabs
    }

    /// The Active_Tab; `None` stands for the leftmost tab.
    pub fn active(&self) -> Option<TabKey> {
        self.active
    }

    pub fn placement(&self) -> &Placement {
        &self.placement
    }

    pub fn status(&self) -> WindowStatus {
        self.status
    }

    fn tab_index(&self, tab: TabKey) -> Option<usize> {
        self.tabs.iter().position(|t| t.key == tab)
    }

    /// Removes the tab at `i`. If it was active, the tab Explorer shows next
    /// (the right neighbour, or the left one at the end) becomes active.
    fn remove_tab_at(&mut self, i: usize) -> LiveTab {
        let tab = self.tabs.remove(i);
        if self.active == Some(tab.key) {
            self.active = self.tabs.get(i.min(self.tabs.len().saturating_sub(1))).map(|t| t.key);
        }
        tab
    }

    fn same_as(&self, other: &LiveWindow) -> bool {
        self.key == other.key
            && self.pid == other.pid
            && self.active == other.active
            && self.placement == other.placement
            && self.status == other.status
            && self.tabs.len() == other.tabs.len()
            && self.tabs.iter().zip(&other.tabs).all(|(a, b)| a.same_as(b))
    }
}

/// The Live_Session itself.
#[derive(Clone, Debug, Default)]
pub struct LiveSession {
    windows: BTreeMap<WinKey, LiveWindow>,
    /// Activation_Order, most recent first. Holds every key in `windows` plus
    /// windows activated before their first scan.
    activation: Vec<WinKey>,
    /// Set at `WM_QUERYENDSESSION` (task 7.6): later window closes are ignored.
    frozen: bool,
    /// `WindowDestroyed`/`ProcessExited` received while frozen, oldest first,
    /// applied at `EndSessionCancelled`. Holds only events for tracked
    /// windows or PIDs, each once, so it stays bounded.
    deferred: Vec<Event>,
    /// Activation_Order entries of windows never scanned that were pending
    /// at a `ProcessExited` (oldest first, at most [`MAX_RECENTLY_CLOSED`]):
    /// their process may be gone, so they no longer count as open, but they
    /// keep their position for a later first scan.
    dormant: Vec<WinKey>,
    /// Time of the latest activation of each Activation_Order entry not
    /// scanned yet (a subset of `activation`).
    unscanned_activated_at: BTreeMap<WinKey, Millis>,
}

impl LiveSession {
    pub fn frozen(&self) -> bool {
        self.frozen
    }

    /// `k` is an Activation_Order entry of a window activated but not
    /// scanned yet, still counted as an open window.
    fn unscanned_open(&self, k: &WinKey) -> bool {
        !self.windows.contains_key(k) && !self.dormant.contains(k)
    }
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// A detected Session_End.
///
/// For `SignOut`, `Shutdown`, and `Restart` (from `EndSessionQuery`) the
/// Live_Session is kept, frozen; every other Session_End empties it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionEnd {
    pub cause: SaveCause,
    /// The snapshot to save (after caps).
    pub snapshot: Snapshot,
}

/// What [`LiveSessionTracker::handle`] produced.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrackerOutput {
    /// Commands for the shell (`AppendLog`, `SetTimer` for tombstones,
    /// `RescanAll` after shell loss or a cancelled session end).
    pub commands: Vec<Command>,
    /// The Live_Session changed (the store schedules a checkpoint). Also set
    /// with a Session_End that empties the Live_Session.
    pub changed: bool,
    pub session_end: Option<SessionEnd>,
}

// ---------------------------------------------------------------------------
// Tracker
// ---------------------------------------------------------------------------

/// Pure Live_Session tracker: events in, commands and captures out.
#[derive(Clone, Debug, Default)]
pub struct LiveSessionTracker {
    session: LiveSession,
    /// `NavigationCompleted` for tabs no scan has listed yet, oldest first.
    pending_navigations: VecDeque<(TabKey, LocationId)>,
    /// Windows removed recently; scans for them are stale and ignored.
    recently_closed: VecDeque<WinKey>,
    /// `seq` of the last tombstone timer set (`TimerOwner::Tracker`).
    timer_seq: u64,
    /// Folding_Candidates in progress, as told by the Engine. A candidate is
    /// an open Explorer_Window the tracker does not know; it may be folded
    /// away or left open (Req 14.15, 14.28). While this is above zero the
    /// Live_Session cannot run out of open windows: closing the last tracked
    /// window, or the termination of the process hosting the last open
    /// tracked windows, is an ordinary close and no Session_End.
    candidates_in_progress: usize,
}

impl LiveSessionTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies one event. `now` is the Engine's monotonic clock; it must not
    /// go backwards between calls. Tombstones due at `now` expire before the
    /// event is applied, so a `WindowDestroyed` at or after a tombstone's
    /// deadline no longer counts that tab.
    pub fn handle(&mut self, now: Millis, event: &Event) -> TrackerOutput {
        let mut out = TrackerOutput::default();
        if self.session.frozen {
            self.handle_frozen(now, event, &mut out);
            return out;
        }
        self.expire_tombstones(now, &mut out);
        self.apply(now, event, &mut out);
        out
    }

    /// Applies one event to the unfrozen Live_Session.
    fn apply(&mut self, now: Millis, event: &Event, out: &mut TrackerOutput) {
        match event {
            Event::WindowScanned(obs) => self.apply_scan(obs, out),
            Event::TabRevoked { win, tab } => self.tab_revoked(now, *win, *tab, out),
            Event::WindowDestroyed { win } => self.window_destroyed(*win, out),
            Event::WindowActivated { win } => self.activate(now, *win, out),
            Event::NavigationCompleted { tab, location } => {
                self.navigation_completed(*tab, location, out)
            }
            Event::ProcessExited { pid } => self.process_exited(now, *pid, out),
            // The Live_Session stays as it was (Req 1.10).
            Event::ShellLost => {}
            // Resume: rescan every open window (Req 1.7, 2.9).
            Event::ShellAvailable => out.commands.push(Command::RescanAll),
            Event::EndSessionQuery(kind) => self.end_session_query(*kind, out),
            // Not frozen: nothing to undo, but catch up anyway.
            Event::EndSessionCancelled => out.commands.push(Command::RescanAll),
            // Tombstone timers: the expiry in `handle` did the work.
            Event::Timer(TimerId { owner: TimerOwner::Tracker, .. }) => {}
            _ => {}
        }
    }

    /// While frozen (Req 3.5): window closes and process exits are deferred,
    /// everything else is ignored until `EndSessionCancelled`.
    fn handle_frozen(&mut self, now: Millis, event: &Event, out: &mut TrackerOutput) {
        match event {
            Event::WindowDestroyed { win } => {
                let s = &self.session;
                if (s.windows.contains_key(win) || s.activation.contains(win)) && !s.deferred.contains(event) {
                    self.session.deferred.push(event.clone());
                }
            }
            Event::ProcessExited { pid } => {
                if self.session.windows.values().any(|w| w.pid == *pid) && !self.session.deferred.contains(event) {
                    self.session.deferred.push(event.clone());
                }
            }
            Event::EndSessionQuery(kind) => self.end_session_query(*kind, out),
            Event::EndSessionCancelled => {
                self.session.frozen = false;
                self.expire_tombstones(now, out);
                self.drop_closed_while_frozen(now, out);
                out.commands.push(Command::RescanAll);
            }
            _ => {}
        }
    }

    /// At `EndSessionCancelled`: the windows destroyed while frozen, and those
    /// of processes that exited meanwhile, were closed by the aborted session
    /// end, whose Session_End already saved them. They are dropped without a
    /// new Session_End (not replayed as user closes, which would save only the
    /// last of them). If no `Open` window remains, the session is over and
    /// the orphans go too.
    fn drop_closed_while_frozen(&mut self, now: Millis, out: &mut TrackerOutput) {
        let deferred = std::mem::take(&mut self.session.deferred);
        if deferred.is_empty() {
            return;
        }
        for e in deferred {
            match e {
                Event::WindowDestroyed { win } => out.changed |= self.remove_window_now(win),
                Event::ProcessExited { pid } => {
                    self.forget_unscanned_activations(now);
                    let keys: Vec<WinKey> =
                        self.session.windows.values().filter(|w| w.pid == pid).map(|w| w.key).collect();
                    for k in keys {
                        out.changed |= self.remove_window_now(k);
                    }
                }
                _ => {}
            }
        }
        let s = &self.session;
        let open_remains = s.windows.values().any(|w| w.status == WindowStatus::Open)
            || s.activation.iter().any(|k| s.unscanned_open(k));
        if !open_remains {
            out.changed |= self.drop_orphans();
        }
    }

    // --- accessors -----------------------------------------------------------

    /// Tells the tracker how many Folding_Candidates are in progress (Req
    /// 14.15, 14.28, 2.7). The Engine sets it before every event it
    /// delivers. While it is above zero, closing the last tracked window is
    /// an ordinary window close (Req 2.7) and the process hosting the last
    /// open windows ending is an ordinary orphaning: neither is a
    /// Session_End. A candidate that is left open reaches the tracker as a
    /// scan and then counts as a window like any other. `EndSessionQuery`
    /// is not affected: a candidate is not in the Live_Session, so it has
    /// nothing to add to the snapshot (Req 3.5, 3.11).
    pub fn set_candidates_in_progress(&mut self, count: usize) {
        self.candidates_in_progress = count;
    }

    /// The number of Folding_Candidates in progress, as last told.
    pub fn candidates_in_progress(&self) -> usize {
        self.candidates_in_progress
    }

    pub fn session(&self) -> &LiveSession {
        &self.session
    }

    /// Tracked windows in Activation_Order, most recent first.
    pub fn windows(&self) -> impl Iterator<Item = &LiveWindow> + '_ {
        self.session.activation.iter().filter_map(|k| self.session.windows.get(k))
    }

    pub fn window(&self, win: WinKey) -> Option<&LiveWindow> {
        self.session.windows.get(&win)
    }

    /// Keys of the tracked windows in Activation_Order.
    pub fn activation_order(&self) -> Vec<WinKey> {
        self.windows().map(|w| w.key).collect()
    }

    /// Number of tracked windows (open or orphaned).
    pub fn window_count(&self) -> usize {
        self.session.windows.len()
    }

    /// Number of tracked windows that are not orphaned.
    pub fn open_window_count(&self) -> usize {
        self.session.windows.values().filter(|w| w.status == WindowStatus::Open).count()
    }

    /// Keys of the windows that are tracked and open (not orphaned), in
    /// Activation_Order, most recent first (Req 14.9, 14.13). An orphan's
    /// window is gone with its process and is no Fold_Target.
    pub fn open_window_keys(&self) -> Vec<WinKey> {
        self.windows().filter(|w| w.status == WindowStatus::Open).map(|w| w.key).collect()
    }

    /// The fold view (Req 14.9, 14.13, 14.20, 5.13): every tracked window that
    /// is open (not orphaned), in Activation_Order, as the pure
    /// [`fold::select`](crate::fold::select) sees it.
    ///
    /// - `tabs`: the `fold_key` of each tab's recorded Location, left to right
    ///   in the reconciled order. A Deferred_Tab carries the Location the
    ///   tracker records for it (its saved Location, Req 5.13), not its
    ///   placeholder. A tab with no recorded Location, and a tombstoned tab
    ///   (its close is under way), has `None` and matches nothing; both keep
    ///   their position, so indices stay the tracker's tab order.
    /// - `active`: index of the Active_Tab (the leftmost tab when none is
    ///   recorded); `None` for a window without tabs.
    /// - `closing`: the window has tabs and every one is tombstoned. A destroyed
    ///   window is no longer tracked, so it does not appear at all.
    pub fn fold_windows(&self) -> Vec<FoldWindow<'_>> {
        self.windows()
            .filter(|w| w.status == WindowStatus::Open)
            .map(|w| FoldWindow {
                win: w.key,
                tabs: w
                    .tabs
                    .iter()
                    .map(|t| match (&t.location, t.is_tombstoned()) {
                        (Some(l), false) => Some(l.fold_key()),
                        _ => None,
                    })
                    .collect(),
                active: if w.tabs.is_empty() {
                    None
                } else {
                    Some(w.active.and_then(|k| w.tab_index(k)).unwrap_or(0))
                },
                closing: !w.tabs.is_empty() && w.tabs.iter().all(LiveTab::is_tombstoned),
            })
            .collect()
    }

    /// An Explorer window other than `win` is open: tracked and not
    /// orphaned, or activated but not scanned yet (as for Session_End
    /// detection). Used by trigger detection (Req 4.1, 4.2).
    pub fn has_other_open_window(&self, win: WinKey) -> bool {
        let s = &self.session;
        s.windows.values().any(|w| w.key != win && w.status == WindowStatus::Open)
            || s.activation.iter().any(|k| *k != win && s.unscanned_open(k))
    }

    /// Number of tracked tabs, including tabs with no Location yet.
    pub fn tab_count(&self) -> usize {
        self.session.windows.values().map(|w| w.tabs.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.session.windows.is_empty()
    }

    pub fn is_frozen(&self) -> bool {
        self.session.frozen
    }

    /// Drops every orphaned window; called at Restoration_Completion
    /// (Req 3.3). Returns whether the Live_Session changed.
    pub fn drop_orphans(&mut self) -> bool {
        let orphans: Vec<WinKey> = self
            .session
            .windows
            .values()
            .filter(|w| w.status == WindowStatus::Orphaned)
            .map(|w| w.key)
            .collect();
        let mut changed = false;
        for k in orphans {
            changed |= self.remove_window_now(k);
        }
        changed
    }

    /// The Live_Session as a snapshot, after caps, plus the `CAP-EXCLUDED`
    /// entry to log if the caps left anything out (Req 2.10).
    pub fn capture(&self) -> (Snapshot, Option<LogEntry>) {
        let mut windows = Vec::new();
        let mut activation = Vec::new();
        for w in self.windows() {
            let located: Vec<(usize, &LocationId)> = w
                .tabs
                .iter()
                .enumerate()
                .filter_map(|(i, t)| t.location.as_ref().map(|l| (i, l)))
                .collect();
            if located.is_empty() {
                continue;
            }
            let active = w
                .active
                .and_then(|k| w.tab_index(k))
                .and_then(|a| located.iter().rposition(|&(i, _)| i <= a))
                .unwrap_or(0);
            let id = WindowId(windows.len() as u32);
            windows.push(WindowRec {
                id,
                tabs: located.iter().map(|&(_, l)| TabRec { location: l.clone() }).collect(),
                active,
                placement: w.placement.clone(),
            });
            activation.push(id);
        }
        let (snapshot, excluded) = apply_caps(Snapshot { windows, activation });
        (snapshot, excluded.log_entry())
    }

    // --- event handlers --------------------------------------------------------

    fn apply_scan(&mut self, obs: &WindowObservation, out: &mut TrackerOutput) {
        let win = obs.win;
        if self.recently_closed.contains(&win) {
            return;
        }

        // Scanned tabs, duplicates counted once.
        let mut scanned: Vec<&TabObservation> = Vec::with_capacity(obs.tabs.len());
        for t in &obs.tabs {
            if !scanned.iter().any(|s| s.tab == t.tab) {
                scanned.push(t);
            }
        }

        let prev = self.session.windows.remove(&win);
        if scanned.is_empty() {
            // Never remove a window (or its tabs) on an empty scan.
            let mut w = prev.clone().unwrap_or_else(|| LiveWindow {
                key: win,
                pid: obs.pid,
                tabs: Vec::new(),
                active: None,
                placement: obs.placement.clone(),
                status: WindowStatus::Open,
            });
            w.pid = obs.pid;
            w.placement = obs.placement.clone();
            self.store_window(w, prev.as_ref(), out);
            return;
        }

        // Tabs that moved here from another window (drag with a surviving key).
        // An emptied source is left out of the capture and removed at its
        // `WindowDestroyed` (see `remove_tab_now`).
        let mut moved: BTreeMap<TabKey, LiveTab> = BTreeMap::new();
        for t in &scanned {
            if let Some(owner) = self.owner_of(t.tab).filter(|&o| o != win) {
                let source = self.session.windows.get_mut(&owner).expect("owner exists");
                let i = source.tab_index(t.tab).expect("owner holds tab");
                moved.insert(t.tab, source.remove_tab_at(i));
                out.changed = true;
            }
        }

        // Build the scanned tabs with their recorded state.
        let mut prev_tabs: BTreeMap<TabKey, LiveTab> = BTreeMap::new();
        let mut prev_order = Vec::new();
        if let Some(p) = &prev {
            for t in &p.tabs {
                prev_order.push(t.key);
                prev_tabs.insert(t.key, t.clone());
            }
        }
        let mut tabs: BTreeMap<TabKey, LiveTab> = BTreeMap::new();
        let mut match_input = Vec::with_capacity(scanned.len());
        for t in &scanned {
            let mut tab = prev_tabs
                .remove(&t.tab)
                .or_else(|| moved.remove(&t.tab))
                .unwrap_or_else(|| LiveTab::new(t.tab));
            if let Some(i) = self.pending_navigations.iter().position(|(k, _)| *k == t.tab) {
                let (_, location) = self.pending_navigations.remove(i).expect("index valid");
                tab.set_location(&location);
            }
            let names = match &t.location {
                ObservedLocation::Known(l) => {
                    tab.set_location(l);
                    ScannedTab::from_location(t.tab, l)
                }
                ObservedLocation::Unidentifiable => {
                    if !tab.unidentified {
                        tab.unidentified = true;
                        out.commands.push(Command::AppendLog(LogEntry::UnidentifiableLocation {
                            kept_previous: tab.location.is_some(),
                        }));
                    }
                    // The UIA name shows the unidentified Location, not the kept one.
                    ScannedTab::new(t.tab, Vec::new())
                }
                // The observation carries no display name for an unnavigated tab.
                ObservedLocation::NotNavigated => match &tab.location {
                    Some(l) => ScannedTab::from_location(t.tab, l),
                    None => ScannedTab::new(t.tab, Vec::new()),
                },
            };
            match_input.push(names);
            tabs.insert(t.tab, tab);
        }

        let rec = TabOrderReconciler::reconcile(&prev_order, &match_input, obs.strip.as_ref());
        let mut ordered: Vec<LiveTab> =
            rec.order.iter().map(|k| tabs.remove(k).expect("reconcile permutes")).collect();

        // Tombstoned tabs the scan no longer lists stay where they were, after
        // their nearest previous left neighbour, until the tombstone expires.
        let mut anchor: Option<usize> = None;
        for k in &prev_order {
            if let Some(i) = ordered.iter().position(|t| t.key == *k) {
                anchor = Some(i);
            } else if let Some(t) = prev_tabs.remove(k).filter(LiveTab::is_tombstoned) {
                let at = anchor.map_or(0, |i| i + 1);
                ordered.insert(at, t);
                anchor = Some(at);
            }
        }

        let present = |k: &TabKey| ordered.iter().any(|t| t.key == *k);
        let prev_active = prev.as_ref().and_then(|p| p.active).filter(present);
        // The UIA selection comes first: it is positionally consistent with the
        // reconciled order even when tabs share a name.
        let strip_selected = obs.strip.as_ref().and_then(|s| s.selected);
        let active = match (rec.selected, strip_selected) {
            (Some(sel), _) => Some(sel),
            // The selected item matches no tab: its nearest matched left
            // neighbour, or the leftmost tab.
            (None, Some(i)) => rec.uia_tabs.iter().take(i).rev().flatten().next().copied(),
            (None, None) => obs.active.filter(present).or(prev_active),
        };

        let w = LiveWindow {
            key: win,
            pid: obs.pid,
            tabs: ordered,
            active,
            placement: obs.placement.clone(),
            status: prev.as_ref().map_or(WindowStatus::Open, |p| p.status),
        };
        self.store_window(w, prev.as_ref(), out);
    }

    /// Puts `w` back into the session (appending a new window to the
    /// Activation_Order) and records whether anything changed.
    fn store_window(&mut self, w: LiveWindow, prev: Option<&LiveWindow>, out: &mut TrackerOutput) {
        if !prev.is_some_and(|p| p.same_as(&w)) {
            out.changed = true;
        }
        if !self.session.activation.contains(&w.key) {
            self.session.activation.push(w.key);
        }
        self.session.dormant.retain(|k| *k != w.key);
        self.session.unscanned_activated_at.remove(&w.key);
        self.session.windows.insert(w.key, w);
    }

    fn tab_revoked(&mut self, now: Millis, win: WinKey, tab: TabKey, out: &mut TrackerOutput) {
        self.pending_navigations.retain(|(k, _)| *k != tab);
        let owner = if self.window(win).is_some_and(|w| w.tab_index(tab).is_some()) {
            Some(win)
        } else {
            self.owner_of(tab)
        };
        let Some(owner) = owner else { return };
        let w = self.session.windows.get_mut(&owner).expect("owner exists");
        if w.status == WindowStatus::Orphaned {
            // An artefact of the termination, not a tab close.
            return;
        }
        let i = w.tab_index(tab).expect("owner holds tab");
        let t = &mut w.tabs[i];
        if t.is_tombstoned() {
            // A repeated revocation keeps the first deadline.
            return;
        }
        // The tab stays in the Live_Session (and the capture) until then; the
        // capture is unchanged, so `changed` is reported at expiry.
        let until = now + TOMBSTONE_MS;
        t.tombstone_until = Some(until);
        self.timer_seq += 1;
        out.commands.push(Command::SetTimer {
            id: TimerId { owner: TimerOwner::Tracker, seq: self.timer_seq },
            at: until,
        });
    }

    /// Removes every tombstoned tab whose deadline is at or before `now`
    /// (Req 2.8). A window left with no tabs stays until its
    /// `WindowDestroyed`, because it may hold tabs no scan has listed yet;
    /// `capture` leaves it out (Req 2.4).
    fn expire_tombstones(&mut self, now: Millis, out: &mut TrackerOutput) {
        for w in self.session.windows.values_mut() {
            while let Some(i) = w.tabs.iter().position(|t| t.tombstone_until.is_some_and(|u| u <= now)) {
                w.remove_tab_at(i);
                out.changed = true;
            }
        }
    }

    /// `WindowDestroyed`: the window closes with all its tabs, tombstoned
    /// ones included. Closing the last `Open` window is a Session_End
    /// (Req 3.1, 3.2); otherwise the window is just removed (Req 2.7).
    ///
    /// A window activated but not scanned yet (a new or drag-out window
    /// whose first rescan is still debounced) counts as an open window with
    /// no captured tabs. Its destruction ends the session only if orphans
    /// remain to be saved; with nothing tracked there is nothing to save.
    fn window_destroyed(&mut self, win: WinKey, out: &mut TrackerOutput) {
        if self.window(win).is_some_and(|w| w.status == WindowStatus::Orphaned) {
            // Already gone with its process; keep it until the Session_End.
            return;
        }
        let s = &self.session;
        let others_open = s.windows.values().any(|w| w.key != win && w.status == WindowStatus::Open)
            || s.activation.iter().any(|k| *k != win && s.unscanned_open(k));
        // A Folding_Candidate is an open window too (Req 14.15, 14.28).
        let cause = match s.windows.get(&win) {
            _ if others_open || self.candidates_in_progress > 0 => None,
            Some(w) if w.status == WindowStatus::Open => {
                let tombstoned = w.tabs.iter().filter(|t| t.is_tombstoned()).count();
                Some(if w.tabs.len() == 1 && tombstoned == 1 {
                    SaveCause::LastTabClosed
                } else {
                    SaveCause::LastWindowClosed
                })
            }
            None if s.activation.contains(&win) && !s.windows.is_empty() => Some(SaveCause::LastWindowClosed),
            _ => None,
        };
        if let Some(cause) = cause {
            self.end_session(cause, out);
        }
        out.changed |= self.remove_window_now(win);
    }

    /// Emits a Session_End with the current capture (logging `CAP-EXCLUDED`
    /// if the caps left anything out) and drops every tracked window,
    /// orphans included (the caller removes the closing window's own
    /// Activation_Order entry if it was never scanned). Task 7.6 calls this
    /// for `ExplorerTerminated`.
    fn end_session(&mut self, cause: SaveCause, out: &mut TrackerOutput) {
        let (snapshot, excluded) = self.capture();
        if let Some(entry) = excluded {
            out.commands.push(Command::AppendLog(entry));
        }
        out.session_end = Some(SessionEnd { cause, snapshot });
        let keys: Vec<WinKey> = self.session.windows.keys().copied().collect();
        for k in keys {
            self.remove_window_now(k);
        }
        out.changed = true;
    }

    /// `ProcessExited(pid)`: orderly-closed windows of `pid` close, the
    /// others become orphans (Req 3.3); with no `Open` window left, the
    /// session ends as `ExplorerTerminated` (Req 3.4).
    fn process_exited(&mut self, now: Millis, pid: u32, out: &mut TrackerOutput) {
        self.forget_unscanned_activations(now);
        let keys: Vec<WinKey> = self
            .session
            .windows
            .values()
            .filter(|w| w.pid == pid && w.status == WindowStatus::Open)
            .map(|w| w.key)
            .collect();
        // Orderly closes first: the other windows of `pid` are still `Open`,
        // so none of these can end the session while orphans are pending.
        let mut orphans = Vec::new();
        for k in keys {
            let Some(w) = self.session.windows.get(&k) else { continue };
            if w.tabs.iter().all(LiveTab::is_tombstoned) {
                // Every tab revoked (or none left) while the process was alive.
                self.window_destroyed(k, out);
            } else {
                orphans.push(k);
            }
        }
        let mut orphaned = false;
        for k in orphans {
            if let Some(w) = self.session.windows.get_mut(&k) {
                w.status = WindowStatus::Orphaned;
                orphaned = true;
            }
        }
        if !orphaned {
            return;
        }
        // Orphaning leaves the capture as it was: `changed` is not set.
        let s = &self.session;
        let open_remains = s.windows.values().any(|w| w.status == WindowStatus::Open)
            || s.activation.iter().any(|k| s.unscanned_open(k));
        // The orphans stay in the Live_Session while a candidate is in
        // progress, as they do while any other window is open.
        if !open_remains && self.candidates_in_progress == 0 {
            self.end_session(SaveCause::ExplorerTerminated, out);
        }
    }

    /// At `ProcessExited`: makes the Activation_Order entries of windows
    /// activated but never scanned dormant. Their PID is unknown, and one
    /// whose process is gone gets neither a scan nor a `WindowDestroyed`, so
    /// it would count as open forever and keep every later last-window close
    /// from ending the session. A dormant entry no longer counts as open but
    /// keeps its position, so a window of a live process (for example one
    /// opened while the shell was lost) keeps its activation at its first
    /// scan. At most [`MAX_RECENTLY_CLOSED`] are kept; the oldest go first.
    ///
    /// An entry activated within [`PROCESS_EXIT_DELAY_MS`] before `now` was
    /// activated after the exited process had ended, so it is a window of a
    /// live process (typically a new window whose first scan is still
    /// debounced) and keeps counting as open; a later `ProcessExited` makes
    /// it dormant if it is still unscanned by then.
    fn forget_unscanned_activations(&mut self, now: Millis) {
        let ended_by = Millis(now.0.saturating_sub(PROCESS_EXIT_DELAY_MS));
        let s = &mut self.session;
        for k in &s.activation {
            let recent = s.unscanned_activated_at.get(k).is_some_and(|at| *at > ended_by);
            if !s.windows.contains_key(k) && !s.dormant.contains(k) && !recent {
                s.dormant.push(*k);
            }
        }
        while s.dormant.len() > MAX_RECENTLY_CLOSED {
            let k = s.dormant.remove(0);
            s.activation.retain(|a| *a != k);
            s.unscanned_activated_at.remove(&k);
        }
    }

    /// `EndSessionQuery`: freezes the Live_Session and emits its capture as
    /// the Session_End of a sign-out, shutdown, or restart, keeping every
    /// window (Req 3.5). Nothing is emitted when no window is tracked
    /// (Req 3.11).
    fn end_session_query(&mut self, kind: EndKind, out: &mut TrackerOutput) {
        self.session.frozen = true;
        if self.session.windows.is_empty() {
            return;
        }
        let cause = match kind {
            EndKind::SignOut => SaveCause::SignOut,
            EndKind::Shutdown => SaveCause::Shutdown,
            EndKind::Restart => SaveCause::Restart,
        };
        let (snapshot, excluded) = self.capture();
        if let Some(entry) = excluded {
            out.commands.push(Command::AppendLog(entry));
        }
        out.session_end = Some(SessionEnd { cause, snapshot });
    }

    /// Removes `win` from the session and the Activation_Order (Req 2.7).
    /// Returns whether the Live_Session changed.
    fn remove_window_now(&mut self, win: WinKey) -> bool {
        // Dropping an unscanned window's activation does not change the capture.
        let removed = self.session.windows.remove(&win).is_some();
        self.session.activation.retain(|k| *k != win);
        self.session.dormant.retain(|k| *k != win);
        self.session.unscanned_activated_at.remove(&win);
        if !self.recently_closed.contains(&win) {
            if self.recently_closed.len() == MAX_RECENTLY_CLOSED {
                self.recently_closed.pop_front();
            }
            self.recently_closed.push_back(win);
        }
        removed
    }

    fn activate(&mut self, now: Millis, win: WinKey, out: &mut TrackerOutput) {
        if self.recently_closed.contains(&win) {
            return;
        }
        // Activated, so it exists: it counts as open again.
        self.session.dormant.retain(|k| *k != win);
        if !self.session.windows.contains_key(&win) {
            self.session.unscanned_activated_at.insert(win, now);
        }
        let order = &mut self.session.activation;
        match order.iter().position(|k| *k == win) {
            Some(0) => {}
            Some(i) => {
                order.remove(i);
                order.insert(0, win);
                out.changed |= self.session.windows.contains_key(&win);
            }
            // Not scanned yet: remember the activation for its first scan.
            None => order.insert(0, win),
        }
    }

    fn navigation_completed(&mut self, tab: TabKey, location: &LocationId, out: &mut TrackerOutput) {
        if let Some(owner) = self.owner_of(tab) {
            let w = self.session.windows.get_mut(&owner).expect("owner exists");
            let i = w.tab_index(tab).expect("owner holds tab");
            let t = &mut w.tabs[i];
            if t.is_tombstoned() {
                // Not a change made before the tab began closing.
                return;
            }
            let before = t.clone();
            t.set_location(location);
            out.changed |= !before.same_as(t);
            return;
        }
        self.pending_navigations.retain(|(k, _)| *k != tab);
        if self.pending_navigations.len() == MAX_PENDING_NAVIGATIONS {
            self.pending_navigations.pop_front();
        }
        self.pending_navigations.push_back((tab, location.clone()));
    }

    fn owner_of(&self, tab: TabKey) -> Option<WinKey> {
        self.session.windows.values().find(|w| w.tab_index(tab).is_some()).map(|w| w.key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{TabObservation, TabStripState};
    use crate::model::{validate, DisplayRef, LocationKind, Rect, ShowState, Volume, WideString};

    fn wk(hwnd: u64, seq: u64) -> WinKey {
        WinKey { hwnd, seq }
    }

    fn tk(hwnd: u64, seq: u64) -> TabKey {
        TabKey { hwnd, seq }
    }

    fn loc(name: &str) -> LocationId {
        LocationId {
            case_sensitive: false,
            kind: LocationKind::FileSystem(Volume::Fixed),
            parsing_name: format!(r"C:\{name}").as_str().into(),
            display_name: name.into(),
            pidl: vec![1],
            search: None,
        }
    }

    fn placement(left: i32) -> Placement {
        let r = Rect { left, top: 0, right: left + 800, bottom: 600 };
        Placement {
            state: ShowState::Normal,
            restored: r,
            display: DisplayRef { device: r"\\.\DISPLAY1".into(), monitor: r },
        }
    }

    fn known(tab: TabKey, name: &str) -> TabObservation {
        TabObservation { tab, location: ObservedLocation::Known(loc(name)) }
    }

    fn obs_of(tab: TabKey, location: ObservedLocation) -> TabObservation {
        TabObservation { tab, location }
    }

    /// A scan whose UIA strip shows `strip` (display names) with `selected`.
    fn scan(win: WinKey, tabs: Vec<TabObservation>, strip: &[&str], selected: Option<usize>) -> Event {
        Event::WindowScanned(WindowObservation {
            win,
            pid: 10,
            tabs,
            active: None,
            strip: Some(TabStripState {
                names: strip.iter().map(|n| WideString::from(*n)).collect(),
                selected,
            }),
            placement: placement(0),
        })
    }

    fn feed(t: &mut LiveSessionTracker, events: &[Event]) -> Vec<TrackerOutput> {
        feed_at(t, 0, events)
    }

    fn feed_at(t: &mut LiveSessionTracker, at: u64, events: &[Event]) -> Vec<TrackerOutput> {
        events.iter().map(|e| t.handle(Millis(at), e)).collect()
    }

    /// Delivers a tracker timer at `at` (any seq: expiry is by deadline).
    fn timer_at(t: &mut LiveSessionTracker, at: u64) -> TrackerOutput {
        t.handle(Millis(at), &Event::Timer(TimerId { owner: TimerOwner::Tracker, seq: 0 }))
    }

    /// Captured windows in Activation_Order as (tab display names, active index).
    fn shape(t: &LiveSessionTracker) -> Vec<(Vec<String>, usize)> {
        let (s, log) = t.capture();
        assert_eq!(validate(&s), Ok(()));
        assert_eq!(log, None);
        s.activation
            .iter()
            .map(|id| {
                let w = s.window(*id).unwrap();
                (w.tabs.iter().map(|t| t.location.display_name.to_string_lossy()).collect(), w.active)
            })
            .collect()
    }

    fn names(ns: &[&str]) -> Vec<String> {
        ns.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn scan_orders_by_uia_and_takes_the_selected_tab() {
        let mut t = LiveSessionTracker::new();
        let w = wk(1, 1);
        // Enumeration order A, B, C; visual order C, A, B; B selected.
        let out = feed(
            &mut t,
            &[scan(w, vec![known(tk(2, 2), "A"), known(tk(3, 3), "B"), known(tk(4, 4), "C")], &["C", "A", "B"], Some(2))],
        );
        assert!(out[0].changed);
        assert!(out[0].commands.is_empty());
        assert_eq!(shape(&t), vec![(names(&["C", "A", "B"]), 2)]);
        assert_eq!(t.window(w).unwrap().active(), Some(tk(3, 3)));
        assert_eq!(t.window(w).unwrap().pid(), 10);
        assert_eq!(t.window_count(), 1);
        assert_eq!(t.open_window_count(), 1);
        assert_eq!(t.tab_count(), 3);

        // The same scan again changes nothing.
        let out = feed(
            &mut t,
            &[scan(w, vec![known(tk(2, 2), "A"), known(tk(3, 3), "B"), known(tk(4, 4), "C")], &["C", "A", "B"], Some(2))],
        );
        assert!(!out[0].changed);
    }

    #[test]
    fn uia_selection_wins_and_active_key_is_the_fallback() {
        let mut t = LiveSessionTracker::new();
        let w = wk(1, 1);
        let Event::WindowScanned(mut o) = scan(w, vec![known(tk(2, 2), "A"), known(tk(3, 3), "B")], &["A", "B"], Some(0))
        else {
            unreachable!()
        };
        o.active = Some(tk(3, 3));
        t.handle(Millis(0), &Event::WindowScanned(o.clone()));
        assert_eq!(shape(&t), vec![(names(&["A", "B"]), 0)]);

        // Without UIA, the explicit key is used; without either, the previous
        // Active_Tab is kept.
        o.strip = None;
        t.handle(Millis(0), &Event::WindowScanned(o.clone()));
        assert_eq!(shape(&t), vec![(names(&["A", "B"]), 1)]);
        o.active = None;
        t.handle(Millis(0), &Event::WindowScanned(o));
        assert_eq!(t.window(w).unwrap().active(), Some(tk(3, 3)));
    }

    #[test]
    fn reused_hwnds_do_not_alias() {
        let mut t = LiveSessionTracker::new();
        // Window 7 (seq 1) is destroyed; a new window reuses HWND 7 with seq 2,
        // and its tab reuses the old tab's HWND.
        feed(&mut t, &[scan(wk(7, 1), vec![known(tk(8, 1), "Old")], &["Old"], Some(0)), Event::WindowDestroyed { win: wk(7, 1) }]);
        assert!(t.is_empty());
        feed(&mut t, &[scan(wk(7, 2), vec![known(tk(8, 2), "New")], &["New"], Some(0))]);
        assert_eq!(shape(&t), vec![(names(&["New"]), 0)]);
        // A late scan of the destroyed window is ignored.
        feed(&mut t, &[scan(wk(7, 1), vec![known(tk(8, 1), "Old")], &["Old"], Some(0))]);
        assert_eq!(t.window_count(), 1);
        // Revoking the old tab key does not touch the new tab.
        feed(&mut t, &[Event::TabRevoked { win: wk(7, 2), tab: tk(8, 1) }]);
        assert_eq!(t.tab_count(), 1);
    }

    #[test]
    fn activation_order_and_early_activation() {
        let mut t = LiveSessionTracker::new();
        let (a, b, c) = (wk(1, 1), wk(2, 2), wk(3, 3));
        feed(
            &mut t,
            &[
                scan(a, vec![known(tk(11, 11), "A")], &["A"], Some(0)),
                // `c` is activated before its first scan.
                Event::WindowActivated { win: c },
                scan(b, vec![known(tk(12, 12), "B")], &["B"], Some(0)),
                scan(c, vec![known(tk(13, 13), "C")], &["C"], Some(0)),
            ],
        );
        // c (activated), then a and b in scan order.
        assert_eq!(t.activation_order(), vec![c, a, b]);
        let out = feed(&mut t, &[Event::WindowActivated { win: b }]);
        assert!(out[0].changed);
        assert_eq!(t.activation_order(), vec![b, c, a]);
        let out = feed(&mut t, &[Event::WindowActivated { win: b }]);
        assert!(!out[0].changed);

        let (s, _) = t.capture();
        assert_eq!(s.activation, vec![WindowId(0), WindowId(1), WindowId(2)]);
        assert_eq!(shape(&t).into_iter().map(|(n, _)| n).collect::<Vec<_>>(), vec![names(&["B"]), names(&["C"]), names(&["A"])]);
    }

    #[test]
    fn window_close_with_others_open_removes_it_and_its_activation() {
        let mut t = LiveSessionTracker::new();
        let (a, b) = (wk(1, 1), wk(2, 2));
        feed(
            &mut t,
            &[
                scan(a, vec![known(tk(11, 11), "A"), known(tk(12, 12), "A2")], &["A", "A2"], Some(0)),
                scan(b, vec![known(tk(13, 13), "B")], &["B"], Some(0)),
                Event::WindowActivated { win: a },
            ],
        );
        let out = feed(&mut t, &[Event::WindowDestroyed { win: a }]);
        assert!(out[0].changed);
        assert_eq!(t.activation_order(), vec![b]);
        assert_eq!(shape(&t), vec![(names(&["B"]), 0)]);
        // Destroying an unknown or unscanned window is harmless.
        let out = feed(&mut t, &[Event::WindowActivated { win: wk(9, 9) }, Event::WindowDestroyed { win: wk(9, 9) }]);
        assert!(!out[1].changed);
        assert_eq!(t.session().activation, vec![b]);
    }

    #[test]
    fn tab_close_activates_the_tab_explorer_shows_next() {
        let mut t = LiveSessionTracker::new();
        let w = wk(1, 1);
        let tabs = || vec![known(tk(2, 2), "A"), known(tk(3, 3), "B"), known(tk(4, 4), "C")];
        feed(&mut t, &[scan(w, tabs(), &["A", "B", "C"], Some(1))]);
        // Closing the active middle tab activates its right neighbour once
        // the tombstone expires.
        let out = feed(&mut t, &[Event::TabRevoked { win: w, tab: tk(3, 3) }]);
        assert!(!out[0].changed, "the tombstoned tab is still captured");
        assert_eq!(shape(&t), vec![(names(&["A", "B", "C"]), 1)]);
        assert!(timer_at(&mut t, TOMBSTONE_MS).changed);
        assert_eq!(shape(&t), vec![(names(&["A", "C"]), 1)]);
        // Closing the active rightmost tab activates its left neighbour.
        feed_at(&mut t, 400, &[Event::TabRevoked { win: w, tab: tk(4, 4) }]);
        timer_at(&mut t, 700);
        assert_eq!(shape(&t), vec![(names(&["A"]), 0)]);
        assert_eq!(t.window(w).unwrap().active(), Some(tk(2, 2)));
        // Closing an inactive tab keeps the Active_Tab.
        let mut t = LiveSessionTracker::new();
        feed(&mut t, &[scan(w, tabs(), &["A", "B", "C"], Some(2)), Event::TabRevoked { win: w, tab: tk(2, 2) }]);
        timer_at(&mut t, 1000);
        assert_eq!(shape(&t), vec![(names(&["B", "C"]), 1)]);
        // A revocation of an unknown tab changes nothing.
        let out = feed_at(&mut t, 1000, &[Event::TabRevoked { win: w, tab: tk(99, 99) }]);
        assert_eq!(out[0], TrackerOutput::default());
    }

    #[test]
    fn scan_drops_unlisted_tabs_and_keeps_order_without_uia() {
        let mut t = LiveSessionTracker::new();
        let w = wk(1, 1);
        feed(&mut t, &[scan(w, vec![known(tk(2, 2), "A"), known(tk(3, 3), "B"), known(tk(4, 4), "C")], &["C", "B", "A"], Some(0))]);
        let Event::WindowScanned(mut o) = scan(w, vec![known(tk(4, 4), "C"), known(tk(2, 2), "A"), known(tk(5, 5), "D")], &[], None)
        else {
            unreachable!()
        };
        o.strip = None;
        o.placement = placement(300);
        t.handle(Millis(0), &Event::WindowScanned(o));
        // B is gone; C, A keep their order and D is appended; C stays active.
        assert_eq!(shape(&t), vec![(names(&["C", "A", "D"]), 0)]);
        assert_eq!(t.window(w).unwrap().placement(), &placement(300));
    }

    #[test]
    fn empty_scan_never_removes_a_window() {
        let mut t = LiveSessionTracker::new();
        let w = wk(1, 1);
        feed(&mut t, &[scan(w, vec![known(tk(2, 2), "A")], &["A"], Some(0))]);
        let out = feed(&mut t, &[scan(w, vec![], &["A"], Some(0))]);
        assert!(!out[0].changed);
        assert_eq!(shape(&t), vec![(names(&["A"]), 0)]);
    }

    #[test]
    fn drag_out_with_a_new_key() {
        let mut t = LiveSessionTracker::new();
        let (src, dst) = (wk(1, 1), wk(2, 5));
        feed(&mut t, &[scan(src, vec![known(tk(2, 2), "A"), known(tk(3, 3), "B")], &["A", "B"], Some(1))]);
        // The dragged tab's old key is revoked, the new window is activated
        // and first scanned before the recreated tab registers.
        feed(
            &mut t,
            &[
                Event::TabRevoked { win: src, tab: tk(3, 3) },
                Event::WindowActivated { win: dst },
                scan(dst, vec![], &["B"], Some(0)),
            ],
        );
        assert_eq!(t.window_count(), 2);
        assert_eq!(t.activation_order(), vec![dst, src]);
        assert_eq!(
            shape(&t),
            vec![(names(&["A", "B"]), 1)],
            "the empty new window is not captured; B is still tombstoned in the source"
        );
        // After the tombstone expired, the new key registers (HWND value
        // reused) and completes its navigation.
        feed_at(
            &mut t,
            400,
            &[
                Event::NavigationCompleted { tab: tk(3, 6), location: loc("B") },
                scan(src, vec![known(tk(2, 2), "A")], &["A"], Some(0)),
                scan(dst, vec![known(tk(3, 6), "B")], &["B"], Some(0)),
            ],
        );
        assert_eq!(shape(&t), vec![(names(&["B"]), 0), (names(&["A"]), 0)]);
    }

    #[test]
    fn drag_in_with_a_surviving_key_removes_the_emptied_source() {
        let mut t = LiveSessionTracker::new();
        let (a, b) = (wk(1, 1), wk(2, 2));
        feed(
            &mut t,
            &[
                scan(a, vec![known(tk(11, 11), "A1"), known(tk(12, 12), "A2")], &["A1", "A2"], Some(0)),
                scan(b, vec![known(tk(13, 13), "B")], &["B"], Some(0)),
                Event::WindowActivated { win: b },
            ],
        );
        // Tab A2 moves into b at position 0 (key unchanged): a keeps A1.
        let out = feed(&mut t, &[scan(b, vec![known(tk(13, 13), "B"), known(tk(12, 12), "A2")], &["A2", "B"], Some(0))]);
        assert!(out[0].changed);
        assert_eq!(shape(&t), vec![(names(&["A2", "B"]), 0), (names(&["A1"]), 0)]);
        // A1 moves into b too: a is left with no tab and is no longer captured.
        feed(
            &mut t,
            &[scan(b, vec![known(tk(13, 13), "B"), known(tk(12, 12), "A2"), known(tk(11, 11), "A1")], &["A2", "B", "A1"], Some(2))],
        );
        assert_eq!(shape(&t), vec![(names(&["A2", "B", "A1"]), 2)]);
        // Its destruction removes it; stale scans afterwards are no-ops.
        let out = feed(&mut t, &[Event::WindowDestroyed { win: a }, scan(a, vec![], &[], None)]);
        assert!(out[0].changed && !out[1].changed);
        assert_eq!(t.activation_order(), vec![b]);
        assert_eq!(t.window_count(), 1);
    }

    #[test]
    fn drag_in_with_a_new_key_and_self_closing_source() {
        let mut t = LiveSessionTracker::new();
        let (a, b) = (wk(1, 1), wk(2, 2));
        feed(
            &mut t,
            &[
                scan(a, vec![known(tk(11, 11), "A")], &["A"], Some(0)),
                scan(b, vec![known(tk(13, 13), "B")], &["B"], Some(0)),
            ],
        );
        feed(
            &mut t,
            &[
                Event::TabRevoked { win: a, tab: tk(11, 11) },
                Event::WindowActivated { win: b },
                Event::WindowDestroyed { win: a },
                Event::NavigationCompleted { tab: tk(11, 20), location: loc("A") },
                scan(b, vec![known(tk(13, 13), "B"), known(tk(11, 20), "A")], &["A", "B"], Some(0)),
            ],
        );
        assert_eq!(t.activation_order(), vec![b]);
        assert_eq!(shape(&t), vec![(names(&["A", "B"]), 0)]);
    }

    #[test]
    fn locations_only_from_completed_navigation() {
        let mut t = LiveSessionTracker::new();
        let w = wk(1, 1);
        // A new tab with no completed navigation is excluded; a window with no
        // located tab is left out entirely.
        feed(&mut t, &[scan(w, vec![obs_of(tk(2, 2), ObservedLocation::NotNavigated)], &["New tab"], Some(0))]);
        assert_eq!(t.window_count(), 1);
        assert_eq!(shape(&t), vec![]);
        // NavigationCompleted on a tracked tab records the Location.
        let out = feed(&mut t, &[Event::NavigationCompleted { tab: tk(2, 2), location: loc("A") }]);
        assert!(out[0].changed);
        assert_eq!(shape(&t), vec![(names(&["A"]), 0)]);
        // A later scan that still reports NotNavigated keeps it.
        feed(&mut t, &[scan(w, vec![obs_of(tk(2, 2), ObservedLocation::NotNavigated)], &["A"], Some(0))]);
        assert_eq!(shape(&t), vec![(names(&["A"]), 0)]);
        // A navigation to the same Location with a new PIDL is still a change.
        let mut l = loc("A");
        l.pidl = vec![9];
        let out = feed(&mut t, &[Event::NavigationCompleted { tab: tk(2, 2), location: l.clone() }]);
        assert!(out[0].changed);
        assert!(t.window(w).unwrap().tabs()[0].location().unwrap().identical(&l));
    }

    #[test]
    fn unlocated_active_tab_falls_back_left_then_leftmost() {
        let mut t = LiveSessionTracker::new();
        let (a, b) = (wk(1, 1), wk(2, 2));
        let nn = |k| obs_of(k, ObservedLocation::NotNavigated);
        feed(
            &mut t,
            &[
                // A, B, <new>, C with <new> selected → B.
                scan(a, vec![known(tk(11, 11), "A"), known(tk(12, 12), "B"), nn(tk(13, 13)), known(tk(14, 14), "C")], &["A", "B", "New tab", "C"], Some(2)),
                // <new>, X, Y with <new> selected → X (leftmost).
                scan(b, vec![nn(tk(21, 21)), known(tk(22, 22), "X"), known(tk(23, 23), "Y")], &["New tab", "X", "Y"], Some(0)),
            ],
        );
        assert_eq!(shape(&t), vec![(names(&["A", "B", "C"]), 1), (names(&["X", "Y"]), 0)]);
    }

    #[test]
    fn unidentifiable_keeps_previous_or_excludes_and_logs_once() {
        let mut t = LiveSessionTracker::new();
        let w = wk(1, 1);
        let un = |k| obs_of(k, ObservedLocation::Unidentifiable);
        let log = |kept| vec![Command::AppendLog(LogEntry::UnidentifiableLocation { kept_previous: kept })];
        feed(&mut t, &[scan(w, vec![known(tk(2, 2), "A")], &["A"], Some(0))]);

        // Tab 2 navigates somewhere unidentifiable: A is kept.
        let out = feed(&mut t, &[scan(w, vec![un(tk(2, 2))], &["Unidentified 1"], Some(0))]);
        assert_eq!(out[0].commands, log(true));
        assert_eq!(shape(&t), vec![(names(&["A"]), 0)]);
        // Rescans of the same state do not log again.
        let out = feed(&mut t, &[scan(w, vec![un(tk(2, 2))], &["Unidentified 1"], Some(0))]);
        assert!(out[0].commands.is_empty());

        // A new tab that never had an identifier is excluded.
        let out = feed(&mut t, &[scan(w, vec![un(tk(2, 2)), un(tk(3, 3))], &["Unidentified 1", "Unidentified 2"], Some(1))]);
        assert_eq!(out[0].commands, log(false));
        assert_eq!(shape(&t), vec![(names(&["A"]), 0)]);

        // An identified navigation clears the state; the next unidentified one logs again.
        feed(&mut t, &[Event::NavigationCompleted { tab: tk(2, 2), location: loc("B") }]);
        assert!(!t.window(w).unwrap().tabs().iter().find(|x| x.key() == tk(2, 2)).unwrap().unidentified());
        let out = feed(&mut t, &[scan(w, vec![un(tk(2, 2)), un(tk(3, 3))], &["Unidentified 3", "Unidentified 2"], Some(0))]);
        assert_eq!(out[0].commands, log(true));
        assert_eq!(shape(&t), vec![(names(&["B"]), 0)]);
    }

    #[test]
    fn navigation_before_first_scan_is_remembered() {
        let mut t = LiveSessionTracker::new();
        let w = wk(1, 1);
        let out = feed(
            &mut t,
            &[
                Event::NavigationCompleted { tab: tk(2, 2), location: loc("A") },
                scan(w, vec![obs_of(tk(2, 2), ObservedLocation::NotNavigated)], &["A"], Some(0)),
            ],
        );
        assert!(!out[0].changed, "nothing captured changes before the scan");
        assert_eq!(shape(&t), vec![(names(&["A"]), 0)]);
        // A revoked tab's remembered navigation is forgotten.
        feed(
            &mut t,
            &[
                Event::NavigationCompleted { tab: tk(5, 5), location: loc("Gone") },
                Event::TabRevoked { win: w, tab: tk(5, 5) },
                scan(w, vec![known(tk(2, 2), "A"), obs_of(tk(5, 5), ObservedLocation::NotNavigated)], &["A"], Some(0)),
            ],
        );
        assert_eq!(shape(&t), vec![(names(&["A"]), 0)]);
    }

    #[test]
    fn separate_processes_are_tracked_alike() {
        let mut t = LiveSessionTracker::new();
        let Event::WindowScanned(mut o) = scan(wk(1, 1), vec![known(tk(2, 2), "A")], &["A"], Some(0)) else {
            unreachable!()
        };
        o.pid = 77;
        t.handle(Millis(0), &Event::WindowScanned(o));
        feed(&mut t, &[scan(wk(3, 3), vec![known(tk(4, 4), "B")], &["B"], Some(0))]);
        assert_eq!(t.window(wk(1, 1)).unwrap().pid(), 77);
        assert_eq!(t.window(wk(3, 3)).unwrap().pid(), 10);
        assert_eq!(t.window(wk(1, 1)).unwrap().status(), WindowStatus::Open);
        assert_eq!(shape(&t).len(), 2);
    }

    #[test]
    fn capture_applies_caps_and_reports_exclusion() {
        let mut t = LiveSessionTracker::new();
        for i in 0..101u64 {
            let name = format!("L{i}");
            feed(&mut t, &[scan(wk(1000 + i, i), vec![known(tk(5000 + i, i), &name)], &[&name], Some(0))]);
        }
        // Never-activated windows rank in scan order, so the last one scanned
        // (L100) is excluded.
        let first = wk(1000, 0);
        let (s, log) = t.capture();
        assert_eq!(validate(&s), Ok(()));
        assert_eq!(s.windows.len(), 100);
        assert_eq!(log, Some(LogEntry::CapExcluded { windows: 1, tabs: 1 }));
        assert_eq!(s.windows[0].tabs[0].location, loc("L0"));
        // Activating the last window moves the exclusion to it.
        feed(&mut t, &[Event::WindowActivated { win: wk(1100, 100) }, Event::WindowActivated { win: first }]);
        let (s, _) = t.capture();
        assert_eq!(s.window(s.activation[0]).unwrap().tabs[0].location, loc("L0"));
        assert_eq!(s.window(s.activation[1]).unwrap().tabs[0].location, loc("L100"));
        assert!(s.windows.iter().all(|w| w.tabs[0].location != loc("L99")));
    }

    #[test]
    fn untracked_events_are_ignored() {
        let mut t = LiveSessionTracker::new();
        feed(&mut t, &[scan(wk(1, 1), vec![known(tk(2, 2), "A")], &["A"], Some(0))]);
        // Another PID's exit, shell loss, and non-tracking events change nothing.
        for e in [Event::ProcessExited { pid: 99 }, Event::ShellLost, Event::ExitRequested] {
            let out = t.handle(Millis(5), &e);
            assert_eq!(out, TrackerOutput::default());
        }
        assert!(!t.is_frozen());
        assert_eq!(shape(&t), vec![(names(&["A"]), 0)]);
    }
}

// ---------------------------------------------------------------------------
// Task 7.5: close coalescing and Session_End detection
// ---------------------------------------------------------------------------

#[cfg(test)]
mod close_tests {
    use super::*;
    use crate::engine::TabStripState;
    use crate::model::{validate, DisplayRef, LocationKind, Rect, ShowState, Volume, WideString};

    fn wk(n: u64) -> WinKey {
        WinKey { hwnd: n, seq: n }
    }

    fn tk(n: u64) -> TabKey {
        TabKey { hwnd: 100 + n, seq: n }
    }

    fn loc(name: &str) -> LocationId {
        LocationId {
            case_sensitive: false,
            kind: LocationKind::FileSystem(Volume::Fixed),
            parsing_name: format!(r"C:\{name}").as_str().into(),
            display_name: name.into(),
            pidl: vec![1],
            search: None,
        }
    }

    fn placement() -> Placement {
        let r = Rect { left: 0, top: 0, right: 800, bottom: 600 };
        Placement { state: ShowState::Normal, restored: r, display: DisplayRef { device: r"\\.\DISPLAY1".into(), monitor: r } }
    }

    /// A scan of `win` listing `tabs` (key, name) in visual order, `selected` active.
    fn scan(win: WinKey, tabs: &[(TabKey, &str)], selected: usize) -> Event {
        Event::WindowScanned(WindowObservation {
            win,
            pid: 10,
            tabs: tabs
                .iter()
                .map(|(k, n)| TabObservation { tab: *k, location: ObservedLocation::Known(loc(n)) })
                .collect(),
            active: None,
            strip: Some(TabStripState {
                names: tabs.iter().map(|(_, n)| WideString::from(*n)).collect(),
                selected: Some(selected),
            }),
            placement: placement(),
        })
    }

    fn at(t: &mut LiveSessionTracker, ms: u64, e: Event) -> TrackerOutput {
        t.handle(Millis(ms), &e)
    }

    fn revoke(win: WinKey, tab: TabKey) -> Event {
        Event::TabRevoked { win, tab }
    }

    fn destroy(win: WinKey) -> Event {
        Event::WindowDestroyed { win }
    }

    /// (tab names, active) per captured window in Activation_Order.
    fn shape_of(s: &Snapshot) -> Vec<(Vec<String>, usize)> {
        assert_eq!(validate(s), Ok(()));
        s.activation
            .iter()
            .map(|id| {
                let w = s.window(*id).unwrap();
                (w.tabs.iter().map(|t| t.location.display_name.to_string_lossy()).collect(), w.active)
            })
            .collect()
    }

    fn names(ns: &[&str]) -> Vec<String> {
        ns.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn revocation_sets_a_300_ms_timer_and_keeps_the_tab_until_then() {
        let mut t = LiveSessionTracker::new();
        let w = wk(1);
        at(&mut t, 0, scan(w, &[(tk(1), "A"), (tk(2), "B")], 1));
        let out = at(&mut t, 50, revoke(w, tk(2)));
        let id = TimerId { owner: TimerOwner::Tracker, seq: 1 };
        assert_eq!(out.commands, vec![Command::SetTimer { id, at: Millis(350) }]);
        assert!(!out.changed);
        assert!(t.window(w).unwrap().tabs()[1].is_tombstoned());
        assert_eq!(t.window(w).unwrap().tabs()[1].tombstone_until(), Some(Millis(350)));
        // A repeated revocation keeps the first deadline and sets no timer.
        assert_eq!(at(&mut t, 100, revoke(w, tk(2))), TrackerOutput::default());
        // Just before the deadline the tab is still there.
        assert!(!at(&mut t, 349, Event::WindowActivated { win: w }).changed);
        assert_eq!(shape_of(&t.capture().0), vec![(names(&["A", "B"]), 1)]);
        // The timer removes it and activates the tab Explorer shows next.
        let out = at(&mut t, 350, Event::Timer(id));
        assert!(out.changed && out.commands.is_empty() && out.session_end.is_none());
        assert_eq!(shape_of(&t.capture().0), vec![(names(&["A"]), 0)]);
        // A late timer is harmless.
        assert_eq!(at(&mut t, 400, Event::Timer(id)), TrackerOutput::default());
    }

    #[test]
    fn expiry_is_also_applied_lazily_by_later_events() {
        let mut t = LiveSessionTracker::new();
        let w = wk(1);
        at(&mut t, 0, scan(w, &[(tk(1), "A"), (tk(2), "B")], 0));
        at(&mut t, 0, revoke(w, tk(2)));
        // No timer delivered; an unrelated event after the deadline expires it.
        let out = at(&mut t, 300, Event::WindowActivated { win: wk(9) });
        assert!(out.changed);
        assert_eq!(t.tab_count(), 1);
    }

    #[test]
    fn navigation_of_a_tombstoned_tab_is_ignored() {
        let mut t = LiveSessionTracker::new();
        let w = wk(1);
        at(&mut t, 0, scan(w, &[(tk(1), "A")], 0));
        at(&mut t, 0, revoke(w, tk(1)));
        let out = at(&mut t, 10, Event::NavigationCompleted { tab: tk(1), location: loc("Z") });
        assert!(!out.changed);
        let end = at(&mut t, 20, destroy(w)).session_end.unwrap();
        assert_eq!(shape_of(&end.snapshot), vec![(names(&["A"]), 0)]);
    }

    #[test]
    fn closing_the_last_tab_of_the_last_window_saves_it_as_last_tab_closed() {
        let mut t = LiveSessionTracker::new();
        let w = wk(1);
        at(&mut t, 0, scan(w, &[(tk(1), "A")], 0));
        let before = t.capture().0;
        at(&mut t, 1000, revoke(w, tk(1)));
        // The scan triggered by the revocation finds no tabs: nothing changes.
        assert!(!at(&mut t, 1100, scan(w, &[], 0)).changed);
        let out = at(&mut t, 1200, destroy(w));
        assert_eq!(out.session_end, Some(SessionEnd { cause: SaveCause::LastTabClosed, snapshot: before }));
        assert!(out.changed);
        assert!(t.is_empty());
        assert!(t.activation_order().is_empty());
        // Late revocations and scans of the closed window are ignored.
        assert_eq!(at(&mut t, 1250, revoke(w, tk(1))), TrackerOutput::default());
        assert!(!at(&mut t, 1260, scan(w, &[(tk(1), "A")], 0)).changed);
        assert!(t.is_empty());
    }

    #[test]
    fn window_destroyed_before_its_only_tab_is_revoked_is_last_window_closed() {
        let mut t = LiveSessionTracker::new();
        let w = wk(1);
        at(&mut t, 0, scan(w, &[(tk(1), "A")], 0));
        let end = at(&mut t, 10, destroy(w)).session_end.unwrap();
        assert_eq!(end.cause, SaveCause::LastWindowClosed);
        assert_eq!(shape_of(&end.snapshot), vec![(names(&["A"]), 0)]);
        assert!(at(&mut t, 20, revoke(w, tk(1))).session_end.is_none());
    }

    #[test]
    fn multi_tab_window_close_keeps_every_tab_and_the_pre_close_active_tab() {
        let mut t = LiveSessionTracker::new();
        let w = wk(1);
        at(&mut t, 0, scan(w, &[(tk(1), "A"), (tk(2), "B"), (tk(3), "C")], 1));
        let before = t.capture().0;
        // B and A are revoked; a scan in between lists only C (selected).
        at(&mut t, 1000, revoke(w, tk(2)));
        at(&mut t, 1040, revoke(w, tk(1)));
        at(&mut t, 1060, scan(w, &[(tk(3), "C")], 0));
        // The tombstoned tabs kept their positions.
        let keys: Vec<TabKey> = t.window(w).unwrap().tabs().iter().map(|x| x.key()).collect();
        assert_eq!(keys, vec![tk(1), tk(2), tk(3)]);
        let end = at(&mut t, 1080, destroy(w)).session_end.unwrap();
        assert_eq!(end.cause, SaveCause::LastWindowClosed);
        assert_eq!(shape_of(&end.snapshot).into_iter().map(|s| s.0).collect::<Vec<_>>(), vec![names(&["A", "B", "C"])]);
        // Without the intermediate scan the snapshot is exactly the pre-close one.
        let mut t = LiveSessionTracker::new();
        at(&mut t, 0, scan(w, &[(tk(1), "A"), (tk(2), "B"), (tk(3), "C")], 1));
        at(&mut t, 1000, revoke(w, tk(2)));
        let end = at(&mut t, 1040, destroy(w)).session_end.unwrap();
        assert_eq!(end, SessionEnd { cause: SaveCause::LastWindowClosed, snapshot: before });
    }

    #[test]
    fn one_tombstone_in_a_multi_tab_window_is_not_last_tab_closed() {
        let mut t = LiveSessionTracker::new();
        let w = wk(1);
        at(&mut t, 0, scan(w, &[(tk(1), "A"), (tk(2), "B")], 0));
        at(&mut t, 1000, revoke(w, tk(1)));
        let end = at(&mut t, 1100, destroy(w)).session_end.unwrap();
        assert_eq!(end.cause, SaveCause::LastWindowClosed);
        assert_eq!(shape_of(&end.snapshot), vec![(names(&["A", "B"]), 0)]);
    }

    #[test]
    fn a_tab_closed_before_the_tombstone_window_is_not_saved() {
        let mut t = LiveSessionTracker::new();
        let w = wk(1);
        at(&mut t, 0, scan(w, &[(tk(1), "A"), (tk(2), "B")], 1));
        at(&mut t, 0, revoke(w, tk(2)));
        // The window closes 300 ms later: B's tombstone has expired.
        let end = at(&mut t, 300, destroy(w)).session_end.unwrap();
        assert_eq!(end.cause, SaveCause::LastWindowClosed);
        assert_eq!(shape_of(&end.snapshot), vec![(names(&["A"]), 0)]);
    }

    #[test]
    fn closing_a_window_while_others_remain_is_no_session_end() {
        let mut t = LiveSessionTracker::new();
        let (a, b) = (wk(1), wk(2));
        at(&mut t, 0, scan(a, &[(tk(1), "A")], 0));
        at(&mut t, 0, scan(b, &[(tk(2), "B"), (tk(3), "C")], 0));
        at(&mut t, 0, Event::WindowActivated { win: b });
        at(&mut t, 1000, revoke(a, tk(1)));
        let out = at(&mut t, 1030, destroy(a));
        assert!(out.changed && out.session_end.is_none());
        assert_eq!(t.activation_order(), vec![b]);
        // The tombstone went with the window; its timer finds nothing.
        assert_eq!(t.tab_count(), 2);
        assert!(!at(&mut t, 1300, Event::Timer(TimerId { owner: TimerOwner::Tracker, seq: 1 })).changed);
        // The last window: everything before it began closing is saved.
        let before = t.capture().0;
        let end = at(&mut t, 2000, destroy(b)).session_end.unwrap();
        assert_eq!(end, SessionEnd { cause: SaveCause::LastWindowClosed, snapshot: before });
    }

    #[test]
    fn a_remaining_window_without_captured_tabs_still_counts_as_open() {
        let mut t = LiveSessionTracker::new();
        let (a, b) = (wk(1), wk(2));
        at(&mut t, 0, scan(a, &[(tk(1), "A")], 0));
        // A drag-out window scanned before its tab registered.
        at(&mut t, 0, scan(b, &[], 0));
        assert!(at(&mut t, 10, destroy(a)).session_end.is_none());
        assert_eq!(t.window_count(), 1);
    }

    #[test]
    fn session_end_includes_and_drops_orphans() {
        let mut t = LiveSessionTracker::new();
        let (a, b) = (wk(1), wk(2));
        at(&mut t, 0, scan(a, &[(tk(1), "A")], 0));
        at(&mut t, 0, scan(b, &[(tk(2), "B")], 0));
        // `a` survived its process (task 7.6 sets this through ProcessExited).
        t.session.windows.get_mut(&a).unwrap().status = WindowStatus::Orphaned;
        let before = t.capture().0;
        let end = at(&mut t, 10, destroy(b)).session_end.unwrap();
        assert_eq!(end, SessionEnd { cause: SaveCause::LastWindowClosed, snapshot: before });
        assert_eq!(end.snapshot.windows.len(), 2);
        assert!(t.is_empty());
        assert!(t.session().activation.is_empty());
        // An orphan's own destruction is ignored: it stays until the Session_End.
        let mut t = LiveSessionTracker::new();
        at(&mut t, 0, scan(a, &[(tk(1), "A")], 0));
        t.session.windows.get_mut(&a).unwrap().status = WindowStatus::Orphaned;
        let out = at(&mut t, 10, destroy(a));
        assert_eq!(out, TrackerOutput::default());
        assert_eq!(t.window_count(), 1);
    }

    #[test]
    fn activated_but_unscanned_windows_count_as_open() {
        let mut t = LiveSessionTracker::new();
        let (a, b) = (wk(1), wk(2));
        at(&mut t, 0, scan(a, &[(tk(1), "A")], 0));
        // `b` opened; its first rescan is still debounced.
        at(&mut t, 0, Event::WindowActivated { win: b });
        let out = at(&mut t, 10, destroy(a));
        assert!(out.changed && out.session_end.is_none());
        // `b` closes before any scan: nothing is tracked, nothing to save.
        let out = at(&mut t, 20, destroy(b));
        assert_eq!(out, TrackerOutput::default());
        assert!(t.session().activation.is_empty());

        // With an orphan tracked, closing the unscanned last window saves it.
        let mut t = LiveSessionTracker::new();
        at(&mut t, 0, scan(a, &[(tk(1), "A")], 0));
        t.session.windows.get_mut(&a).unwrap().status = WindowStatus::Orphaned;
        at(&mut t, 0, Event::WindowActivated { win: b });
        let before = t.capture().0;
        let out = at(&mut t, 10, destroy(b));
        assert_eq!(out.session_end, Some(SessionEnd { cause: SaveCause::LastWindowClosed, snapshot: before }));
        assert!(t.is_empty() && t.session().activation.is_empty());
    }

    #[test]
    fn session_end_logs_cap_exclusion() {
        // 101 windows are tracked when the last Open one closes (the others
        // are orphans), so the capture leaves one out and logs it.
        let mut t = LiveSessionTracker::new();
        for i in 0..101u64 {
            let name = format!("L{i}");
            at(&mut t, 0, scan(wk(1000 + i), &[(tk(1000 + i), &name)], 0));
        }
        for i in 1..101u64 {
            t.session.windows.get_mut(&wk(1000 + i)).unwrap().status = WindowStatus::Orphaned;
        }
        let out = at(&mut t, 20, destroy(wk(1000)));
        assert_eq!(out.commands, vec![Command::AppendLog(LogEntry::CapExcluded { windows: 1, tabs: 1 })]);
        assert_eq!(out.session_end.unwrap().snapshot.windows.len(), 100);
    }
}

// ---------------------------------------------------------------------------
// Task 7.6: process termination, shell loss, and the session-end freeze
// ---------------------------------------------------------------------------

#[cfg(test)]
mod termination_tests {
    use super::*;
    use crate::engine::TabStripState;
    use crate::model::{DisplayRef, LocationKind, Rect, ShowState, Volume, WideString};

    fn wk(n: u64) -> WinKey {
        WinKey { hwnd: n, seq: n }
    }

    fn tk(n: u64) -> TabKey {
        TabKey { hwnd: 100 + n, seq: n }
    }

    fn loc(name: &str) -> LocationId {
        LocationId {
            case_sensitive: false,
            kind: LocationKind::FileSystem(Volume::Fixed),
            parsing_name: format!(r"C:\{name}").as_str().into(),
            display_name: name.into(),
            pidl: vec![1],
            search: None,
        }
    }

    /// A scan of `win` in process `pid` listing `tabs` (key, name), first tab active.
    fn scan(win: WinKey, pid: u32, tabs: &[(TabKey, &str)]) -> Event {
        let r = Rect { left: 0, top: 0, right: 800, bottom: 600 };
        Event::WindowScanned(WindowObservation {
            win,
            pid,
            tabs: tabs
                .iter()
                .map(|(k, n)| TabObservation { tab: *k, location: ObservedLocation::Known(loc(n)) })
                .collect(),
            active: None,
            strip: Some(TabStripState {
                names: tabs.iter().map(|(_, n)| WideString::from(*n)).collect(),
                selected: Some(0),
            }),
            placement: Placement {
                state: ShowState::Normal,
                restored: r,
                display: DisplayRef { device: r"\\.\DISPLAY1".into(), monitor: r },
            },
        })
    }

    fn at(t: &mut LiveSessionTracker, ms: u64, e: Event) -> TrackerOutput {
        t.handle(Millis(ms), &e)
    }

    fn exited(pid: u32) -> Event {
        Event::ProcessExited { pid }
    }

    fn destroy(win: WinKey) -> Event {
        Event::WindowDestroyed { win }
    }

    /// Windows `a` (pid 1, tabs A, B) and `b` (pid 2, tab C), `b` most recent.
    fn two_processes() -> LiveSessionTracker {
        let mut t = LiveSessionTracker::new();
        at(&mut t, 0, scan(wk(1), 1, &[(tk(1), "A"), (tk(2), "B")]));
        at(&mut t, 0, scan(wk(2), 2, &[(tk(3), "C")]));
        at(&mut t, 0, Event::WindowActivated { win: wk(2) });
        t
    }

    #[test]
    fn a_killed_process_leaves_orphans_that_stay_in_the_capture() {
        let mut t = two_processes();
        let before = t.capture().0;
        let out = at(&mut t, 1000, exited(1));
        assert_eq!(out, TrackerOutput::default());
        assert_eq!(t.window(wk(1)).unwrap().status(), WindowStatus::Orphaned);
        assert_eq!(t.open_window_count(), 1);
        assert_eq!(t.capture().0, before);
        // Late artefacts of the termination do not remove anything.
        assert_eq!(at(&mut t, 1010, Event::TabRevoked { win: wk(1), tab: tk(1) }), TrackerOutput::default());
        assert_eq!(at(&mut t, 1020, destroy(wk(1))), TrackerOutput::default());
        assert_eq!(at(&mut t, 2000, exited(1)), TrackerOutput::default());
        assert_eq!(t.capture().0, before);
        // The last process ends: Session_End with the pre-termination capture.
        let out = at(&mut t, 3000, exited(2));
        assert_eq!(out.session_end, Some(SessionEnd { cause: SaveCause::ExplorerTerminated, snapshot: before }));
        assert!(out.changed);
        assert!(t.is_empty() && t.activation_order().is_empty());
    }

    #[test]
    fn killing_the_only_process_ends_the_session() {
        let mut t = LiveSessionTracker::new();
        at(&mut t, 0, scan(wk(1), 7, &[(tk(1), "A")]));
        at(&mut t, 0, scan(wk(2), 7, &[(tk(2), "B"), (tk(3), "C")]));
        let before = t.capture().0;
        let end = at(&mut t, 500, exited(7)).session_end.unwrap();
        assert_eq!(end, SessionEnd { cause: SaveCause::ExplorerTerminated, snapshot: before });
        assert_eq!(end.snapshot.windows.len(), 2);
        assert!(t.is_empty());
    }

    #[test]
    fn closing_the_last_window_with_a_candidate_in_progress_is_an_ordinary_close() {
        let mut t = LiveSessionTracker::new();
        at(&mut t, 0, scan(wk(1), 7, &[(tk(1), "A")]));
        t.set_candidates_in_progress(1);
        assert_eq!(t.candidates_in_progress(), 1);
        let out = at(&mut t, 10, destroy(wk(1)));
        assert!(out.session_end.is_none() && out.changed, "{out:?}");
        assert!(t.is_empty() && t.activation_order().is_empty());

        // The same close with no candidate left is a Session_End.
        t.set_candidates_in_progress(0);
        at(&mut t, 20, scan(wk(2), 7, &[(tk(2), "B")]));
        let end = at(&mut t, 30, destroy(wk(2))).session_end.unwrap();
        assert_eq!(end.cause, SaveCause::LastWindowClosed);
    }

    #[test]
    fn closing_the_last_tab_with_a_candidate_in_progress_is_not_a_session_end_either() {
        let mut t = LiveSessionTracker::new();
        at(&mut t, 0, scan(wk(1), 7, &[(tk(1), "A")]));
        t.set_candidates_in_progress(2);
        at(&mut t, 10, Event::TabRevoked { win: wk(1), tab: tk(1) });
        let out = at(&mut t, 20, destroy(wk(1)));
        assert!(out.session_end.is_none() && t.is_empty(), "{out:?}");
    }

    #[test]
    fn a_process_exit_with_a_candidate_in_progress_keeps_the_orphans_without_a_session_end() {
        let mut t = LiveSessionTracker::new();
        at(&mut t, 0, scan(wk(1), 7, &[(tk(1), "A")]));
        let before = t.capture().0;
        t.set_candidates_in_progress(1);
        let out = at(&mut t, 500, exited(7));
        assert_eq!(out, TrackerOutput::default());
        assert_eq!(t.window(wk(1)).unwrap().status(), WindowStatus::Orphaned);
        assert_eq!(t.capture().0, before);

        // The candidate is gone and another window closes: the orphans are
        // saved with the Session_End.
        t.set_candidates_in_progress(0);
        at(&mut t, 600, scan(wk(2), 8, &[(tk(2), "B")]));
        let end = at(&mut t, 700, destroy(wk(2))).session_end.unwrap();
        assert_eq!(end.cause, SaveCause::LastWindowClosed);
        assert_eq!(end.snapshot.windows.len(), 2);
    }

    #[test]
    fn a_session_end_query_does_not_depend_on_candidates() {
        // The candidate is not in the Live_Session: the windows tracked are saved (Req 3.5).
        let mut t = LiveSessionTracker::new();
        at(&mut t, 0, scan(wk(1), 7, &[(tk(1), "A")]));
        t.set_candidates_in_progress(1);
        let end = at(&mut t, 10, Event::EndSessionQuery(EndKind::Shutdown)).session_end.unwrap();
        assert_eq!(end.cause, SaveCause::Shutdown);
        // A close while frozen is deferred and, at the cancel, no Session_End.
        assert_eq!(at(&mut t, 20, destroy(wk(1))), TrackerOutput::default());
        let out = at(&mut t, 30, Event::EndSessionCancelled);
        assert!(out.session_end.is_none() && t.is_empty());

        // Nothing tracked: nothing to save, candidate or not (Req 3.11).
        let out = at(&mut t, 40, Event::EndSessionQuery(EndKind::SignOut));
        assert!(out.session_end.is_none() && t.is_frozen());
    }

    #[test]
    fn a_process_exit_forgets_windows_activated_but_never_scanned() {
        // `b` was activated but never scanned (e.g. opened while the shell
        // was lost) and its process was killed: it gets no scan and no
        // destroy, so it must not keep counting as open.
        let mut t = LiveSessionTracker::new();
        at(&mut t, 0, scan(wk(1), 7, &[(tk(1), "A")]));
        at(&mut t, 0, Event::WindowActivated { win: wk(2) });
        let before = t.capture().0;
        let end = at(&mut t, 500, exited(7)).session_end.unwrap();
        assert_eq!(end, SessionEnd { cause: SaveCause::ExplorerTerminated, snapshot: before });
        assert!(t.is_empty());
        // `b` keeps its Activation_Order entry (for a later first scan) but
        // no longer counts as open.
        assert!(t.session().activation.iter().all(|k| !t.session().unscanned_open(k)));
        // A later session's last window close is saved again.
        at(&mut t, 1000, scan(wk(3), 8, &[(tk(3), "C")]));
        let end = at(&mut t, 1100, destroy(wk(3))).session_end.unwrap();
        assert_eq!(end.cause, SaveCause::LastWindowClosed);
    }

    #[test]
    fn closing_the_last_open_window_saves_and_drops_orphans() {
        let mut t = two_processes();
        at(&mut t, 100, exited(1));
        let before = t.capture().0;
        let end = at(&mut t, 200, destroy(wk(2))).session_end.unwrap();
        assert_eq!(end, SessionEnd { cause: SaveCause::LastWindowClosed, snapshot: before });
        assert!(t.is_empty());
    }

    #[test]
    fn orderly_closes_are_not_orphaned() {
        // Window `a` was closed (destroyed) before its process exited.
        let mut t = two_processes();
        at(&mut t, 100, destroy(wk(1)));
        assert_eq!(at(&mut t, 300, exited(1)), TrackerOutput::default());
        assert_eq!(t.window_count(), 1);

        // Every tab of `a` revoked, no destroy seen, process exits within the
        // tombstone: an orderly close, so `a` goes and nothing is orphaned.
        let mut t = two_processes();
        at(&mut t, 100, Event::TabRevoked { win: wk(1), tab: tk(1) });
        at(&mut t, 110, Event::TabRevoked { win: wk(1), tab: tk(2) });
        let out = at(&mut t, 300, exited(1));
        assert!(out.changed && out.session_end.is_none());
        assert_eq!(t.activation_order(), vec![wk(2)]);
        assert_eq!(t.open_window_count(), 1);

        // The same for the only window: an ordinary last-window close.
        let mut t = LiveSessionTracker::new();
        at(&mut t, 0, scan(wk(1), 1, &[(tk(1), "A")]));
        let before = t.capture().0;
        at(&mut t, 100, Event::TabRevoked { win: wk(1), tab: tk(1) });
        let end = at(&mut t, 300, exited(1)).session_end.unwrap();
        assert_eq!(end, SessionEnd { cause: SaveCause::LastTabClosed, snapshot: before });
    }

    #[test]
    fn drop_orphans_removes_only_orphans() {
        let mut t = two_processes();
        at(&mut t, 100, exited(1));
        assert!(t.drop_orphans());
        assert_eq!(t.activation_order(), vec![wk(2)]);
        assert_eq!(t.capture().0.tab_count(), 1);
        assert!(!t.drop_orphans());
    }

    #[test]
    fn shell_loss_keeps_the_session_and_resume_requests_a_full_rescan() {
        let mut t = two_processes();
        let before = t.capture().0;
        assert_eq!(at(&mut t, 100, Event::ShellLost), TrackerOutput::default());
        assert_eq!(t.capture().0, before);
        let out = at(&mut t, 5000, Event::ShellAvailable);
        assert_eq!(out, TrackerOutput { commands: vec![Command::RescanAll], ..TrackerOutput::default() });
        assert_eq!(t.capture().0, before);
    }

    #[test]
    fn end_session_query_freezes_and_emits_the_capture() {
        let mut t = two_processes();
        at(&mut t, 50, Event::TabRevoked { win: wk(1), tab: tk(2) });
        let before = t.capture().0;
        let out = at(&mut t, 100, Event::EndSessionQuery(EndKind::Restart));
        assert_eq!(out.session_end, Some(SessionEnd { cause: SaveCause::Restart, snapshot: before.clone() }));
        assert!(!out.changed && out.commands.is_empty());
        assert!(t.is_frozen() && t.session().frozen());
        // Whatever the sign-out closes, the Live_Session stays as it was.
        for e in [
            destroy(wk(2)),
            Event::TabRevoked { win: wk(1), tab: tk(1) },
            scan(wk(1), 1, &[(tk(9), "Z")]),
            Event::WindowActivated { win: wk(1) },
            exited(1),
            Event::ShellLost,
            Event::Timer(TimerId { owner: TimerOwner::Tracker, seq: 1 }),
        ] {
            assert_eq!(at(&mut t, 600, e), TrackerOutput::default());
        }
        assert_eq!(t.capture().0, before);
        // A repeated query re-emits the frozen capture.
        let out = at(&mut t, 700, Event::EndSessionQuery(EndKind::SignOut));
        assert_eq!(out.session_end, Some(SessionEnd { cause: SaveCause::SignOut, snapshot: before }));
    }

    #[test]
    fn end_session_query_with_no_window_emits_nothing() {
        let mut t = LiveSessionTracker::new();
        assert_eq!(at(&mut t, 0, Event::EndSessionQuery(EndKind::Shutdown)), TrackerOutput::default());
        assert!(t.is_frozen());
        assert_eq!(at(&mut t, 10, scan(wk(1), 1, &[(tk(1), "A")])), TrackerOutput::default());
        assert!(t.is_empty());
        let out = at(&mut t, 20, Event::EndSessionCancelled);
        assert_eq!(out.commands, vec![Command::RescanAll]);
        assert!(!t.is_frozen());
    }

    #[test]
    fn cancellation_unfreezes_drops_what_the_end_closed_and_rescans() {
        let mut t = two_processes();
        at(&mut t, 0, scan(wk(3), 3, &[(tk(4), "D")]));
        at(&mut t, 50, Event::TabRevoked { win: wk(1), tab: tk(2) });
        at(&mut t, 100, Event::EndSessionQuery(EndKind::Shutdown));
        at(&mut t, 200, destroy(wk(2)));
        at(&mut t, 210, exited(3));
        let out = at(&mut t, 1000, Event::EndSessionCancelled);
        assert!(out.changed && out.session_end.is_none());
        assert_eq!(out.commands, vec![Command::RescanAll]);
        assert!(!t.is_frozen());
        // `b` (destroyed) and `c` (its process exited) are gone; B's
        // tombstone expired; tracking resumes.
        assert_eq!(t.activation_order(), vec![wk(1)]);
        assert_eq!(t.tab_count(), 1);
        assert!(at(&mut t, 1100, scan(wk(1), 1, &[(tk(1), "A"), (tk(5), "E")])).changed);
        assert_eq!(t.tab_count(), 2);
    }

    #[test]
    fn cancellation_after_every_window_closed_empties_the_session_without_a_new_end() {
        let mut t = two_processes();
        at(&mut t, 0, scan(wk(3), 3, &[(tk(4), "D")]));
        at(&mut t, 10, exited(3));
        assert_eq!(t.window(wk(3)).unwrap().status(), WindowStatus::Orphaned);
        at(&mut t, 100, Event::EndSessionQuery(EndKind::SignOut));
        at(&mut t, 200, destroy(wk(1)));
        at(&mut t, 210, destroy(wk(2)));
        let out = at(&mut t, 1000, Event::EndSessionCancelled);
        assert!(out.changed && out.session_end.is_none());
        assert!(t.is_empty() && t.activation_order().is_empty());
    }
}

#[cfg(test)]
mod fold_view_tests {
    use super::*;
    use crate::engine::TabStripState;
    use crate::model::{DisplayRef, LocationKind, Rect, ShowState, Volume, WideString};

    fn wk(n: u64) -> WinKey {
        WinKey { hwnd: n, seq: n }
    }

    fn tk(n: u64) -> TabKey {
        TabKey { hwnd: 100 + n, seq: n }
    }

    fn loc(name: &str) -> LocationId {
        LocationId {
            case_sensitive: false,
            kind: LocationKind::FileSystem(Volume::Fixed),
            parsing_name: format!(r"C:\{name}").as_str().into(),
            display_name: name.into(),
            pidl: vec![1],
            search: None,
        }
    }

    /// A scan of `win` (pid `pid`) listing `tabs` (key, name), `selected` active.
    fn scan(win: WinKey, pid: u32, tabs: &[(TabKey, &str)], selected: usize) -> Event {
        let r = Rect { left: 0, top: 0, right: 800, bottom: 600 };
        Event::WindowScanned(WindowObservation {
            win,
            pid,
            tabs: tabs
                .iter()
                .map(|(k, n)| TabObservation { tab: *k, location: ObservedLocation::Known(loc(n)) })
                .collect(),
            active: None,
            strip: Some(TabStripState {
                names: tabs.iter().map(|(_, n)| WideString::from(*n)).collect(),
                selected: Some(selected),
            }),
            placement: Placement {
                state: ShowState::Normal,
                restored: r,
                display: DisplayRef { device: r"\\.\DISPLAY1".into(), monitor: r },
            },
        })
    }

    fn at(t: &mut LiveSessionTracker, ms: u64, e: Event) -> TrackerOutput {
        t.handle(Millis(ms), &e)
    }

    #[test]
    fn windows_come_in_activation_order_with_keys_and_active_tab() {
        let mut t = LiveSessionTracker::new();
        at(&mut t, 0, scan(wk(1), 1, &[(tk(1), "A"), (tk(2), "B")], 1));
        at(&mut t, 0, scan(wk(2), 1, &[(tk(3), "c")], 0));
        at(&mut t, 0, Event::WindowActivated { win: wk(1) });

        let (a, b, c) = (loc("A"), loc("B"), loc("C"));
        let view = t.fold_windows();
        assert_eq!(view.iter().map(|w| w.win).collect::<Vec<_>>(), vec![wk(1), wk(2)]);
        assert_eq!(t.open_window_keys(), vec![wk(1), wk(2)]);
        assert_eq!(view[0].tabs, vec![Some(a.fold_key()), Some(b.fold_key())]);
        assert_eq!((view[0].active, view[0].closing), (Some(1), false));
        // Keys are the folded ones: letter case does not matter.
        assert_eq!(view[1].tabs, vec![Some(c.fold_key())]);
        assert_eq!((view[1].active, view[1].closing), (Some(0), false));
    }

    #[test]
    fn a_tombstoned_tab_matches_nothing_and_a_fully_tombstoned_window_is_closing() {
        let (a, mut t) = (loc("A"), LiveSessionTracker::new());
        at(&mut t, 0, scan(wk(1), 1, &[(tk(1), "A"), (tk(2), "B")], 0));
        at(&mut t, 10, Event::TabRevoked { win: wk(1), tab: tk(2) });
        let view = t.fold_windows();
        assert_eq!(view[0].tabs, vec![Some(a.fold_key()), None]);
        assert!(!view[0].closing);

        at(&mut t, 20, Event::TabRevoked { win: wk(1), tab: tk(1) });
        let view = t.fold_windows();
        assert_eq!(view[0].tabs, vec![None, None]);
        assert!(view[0].closing);

        // Once the tombstones expire the window is gone with its last tab
        // only at its `WindowDestroyed`; an empty window is not closing.
        at(&mut t, 400, Event::WindowActivated { win: wk(1) });
        let view = t.fold_windows();
        assert_eq!((view[0].tabs.len(), view[0].active, view[0].closing), (0, None, false));
    }

    #[test]
    fn a_tab_without_a_location_matches_nothing_and_orphans_and_destroyed_windows_are_left_out() {
        let mut t = LiveSessionTracker::new();
        let unlocated = Event::WindowScanned(WindowObservation {
            tabs: vec![TabObservation { tab: tk(9), location: ObservedLocation::NotNavigated }],
            ..match scan(wk(3), 3, &[], 0) {
                Event::WindowScanned(o) => o,
                _ => unreachable!(),
            }
        });
        at(&mut t, 0, scan(wk(1), 1, &[(tk(1), "A")], 0));
        at(&mut t, 0, scan(wk(2), 2, &[(tk(2), "B")], 0));
        at(&mut t, 0, unlocated);
        assert_eq!(t.fold_windows().iter().find(|w| w.win == wk(3)).unwrap().tabs, vec![None]);

        at(&mut t, 10, Event::ProcessExited { pid: 1 });
        assert_eq!(t.window_count(), 3);
        assert_eq!(t.open_window_keys(), vec![wk(2), wk(3)]);
        let wins: Vec<_> = t.fold_windows().iter().map(|w| w.win).collect();
        assert_eq!(wins, vec![wk(2), wk(3)]);

        at(&mut t, 20, Event::WindowDestroyed { win: wk(2) });
        assert_eq!(t.open_window_keys(), vec![wk(3)]);
    }
}
