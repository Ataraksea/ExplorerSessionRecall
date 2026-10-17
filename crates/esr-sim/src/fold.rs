//! Tab-folding simulation, candidate windows: a window the user opens that
//! File Explorer shows only after the adapters have reported its first
//! signal, so the `FoldingEngine` can hide it (Req 14.2, 14.16, 14.27).
//!
//! [`Op::OpenCandidate`](crate::explorer::Op::OpenCandidate) opens such a
//! window in a chosen process and emits, at the same logical time:
//!
//! 1. `WindowAppeared { win, pid, bypass_keys, desktop, foreground }` (the
//!    first signal; the window is not visible and not in ShellWindows), then
//! 2. `WindowActivated` (Explorer made the new window the foreground window
//!    before the first signal reached the core; `foreground` names the window
//!    that was in the foreground before).
//!
//! # Faithfulness (spikes F1, F2; design.md "First signal and classification")
//!
//! - **First navigation.** [`FoldConfig::first_navigation_ms`] after the first
//!   signal the window's only tab shows the requested Location and registers
//!   in ShellWindows: `NavigationCompleted`, then `InitialLocationKnown` with
//!   the tab and the selection (F2: readable 21–170 ms after the first
//!   navigation). With `None` the first navigation never happens: the tab
//!   stays not navigated and unregistered, and no `InitialLocationKnown`
//!   follows (the 8 s first-navigation timer of Req 14.14 is for this case).
//!   Like any user-opened window, one whose Location matches an outstanding
//!   `OpenWindow` request is claimed by it instead
//!   ([`RestoreConfig::claim_user_windows`](crate::restore::RestoreConfig)).
//! - **Visibility.** Explorer shows the window [`FoldConfig::show_after_ms`]
//!   after the first signal (F1: 0.25–1.4 s before the window's own
//!   `EVENT_OBJECT_CREATE`). It is *visible* (alpha 255) from then on, unless
//!   `HideWindow` made it transparent. `HideWindow` takes effect
//!   [`FoldConfig::hide_ms`] after the command (F1: 31–94 ms: setting the
//!   style of another thread's window waits for that thread). `Reveal` takes
//!   effect after [`RestoreConfig::window_op_ms`](crate::restore::RestoreConfig).
//!   A window that becomes visible has a
//!   [`first_visible`](crate::restore::WindowRecord::first_visible) moment;
//!   a candidate that was hidden in time has none until it is revealed
//!   (Req 14.16).
//! - **Closing.** `CloseWindow` closes the window [`FoldConfig::close_ms`]
//!   after the command (`WM_SYSCOMMAND SC_CLOSE`): its registered tabs are
//!   revoked, then `WindowDestroyed` follows, at the same logical time.
//!
//! Every command is recorded with its logical timestamp
//! ([`SimExplorer::command_log`](crate::explorer::SimExplorer::command_log)).
//! Candidates are ordinary [`SimWindow`](crate::explorer::SimWindow)s, so
//! scans, rescans, and the reference model treat them like other windows
//! (a tab that has not navigated has no Location and is left out).
//!
//! # Virtual desktops, foreground, and drag-out (task 21.2)
//!
//! - **Virtual_Desktops** (Req 14.9, 14.11, 14.12). [`FoldConfig::desktops`]
//!   desktops exist; every window is on one ([`DesktopPlace::On`]) or on all
//!   ([`DesktopPlace::AllDesktops`]). A new window (candidate or not) is on the
//!   desktop that is current when it is created. [`Op::SwitchDesktop`],
//!   [`Op::MoveToDesktop`] and [`Op::SetDesktopMembership`] (unreadable
//!   membership: `DesktopView::Unknown`) change them. Activating a window on
//!   another desktop switches to it. The view of a candidate
//!   ([`SimExplorer::desktop_view_of`]) holds the open windows on its desktop
//!   and the all-desktop windows.
//! - **Fold context.** `ProbeFoldContext { probe, candidate }` is answered
//!   [`FoldConfig::context_ms`] later with a fresh `FoldContext`: the desktop
//!   view at that moment (`Unknown` when membership is unreadable or the
//!   candidate is gone) and the windows marked
//!   [unresponsive](SimExplorer::set_unresponsive).
//! - **Foreground.** The foreground window is an Explorer window or an
//!   application (`None`); [`SimExplorer::foreground_history`] lists the
//!   recent changes. [`Op::NewWindow`] (Ctrl+N, "New window") is an
//!   `OpenCandidate` on the folder of the foreground window's Active_Tab with
//!   that window as `foreground`; [`Op::FocusApplication`] followed by an
//!   `OpenCandidate` with `foreground: None` is "Show in folder". Switching
//!   to a desktop that does not hold the foreground window, or moving it away,
//!   leaves an application in the foreground.
//! - **Duplicate tab** ([`Op::DuplicateTab`]) adds a tab next to the Active_Tab
//!   and no window: no first signal and no `TabDetached` (Req 14.5).
//! - **Drag-out** ([`Op::DragOutCandidate`]): the new window's `WindowAppeared`
//!   comes first (the source window is the foreground window), and
//!   [`FoldConfig::detach_ms`] later the source tab window is destroyed:
//!   `TabDetached { from }` and the tab's revocation (spike F3: 5–24 ms).
//!   Explorer shows the window [`FoldConfig::drag_show_gap_ms`] after that
//!   (170–260 ms), and the moved tab registers
//!   [`SimConfig::drag_register_delay_ms`](crate::explorer::SimConfig) after
//!   the drag, when `InitialLocationKnown` follows.
//!
//! # Folded tabs and fault injection (task 21.3)
//!
//! `CreateTab` for a Fold_Target can report `TabStarted { op, tab }` when the
//! new tab window appears ([`FoldConfig::report_tab_started`]; off by default,
//! so restoration event streams stay as they are) and registers the tab
//! [`FoldConfig::registration_lag_ms`] later (F2: `TabStarted` 3–53 ms after
//! the command, `TabCreated` 0.27–0.86 s). The module [`faults`] injects the
//! failures a fold must survive, per operation: unresponsive and closing
//! windows, a tab window that appears late or never, a delayed registration,
//! an `ActivateTab` that stays unconfirmed, a window that closes while it is a
//! target, and a candidate that ignores its first `CloseWindow`.
//!
//! # Generators and the reference model (task 21.4)
//!
//! [`fold_op_strategy`] and [`fold_faults_strategy`] (module `generate`)
//! produce folding operations and failure schedules;
//! [`Op::SetFirstNavigation`] changes the first-navigation delay of the
//! candidates opened after it, so a burst can have its Locations arrive in any
//! order. [`FoldReference`](crate::reference::FoldReference) holds the
//! expected Fold_Targets and Opening_Order.
//!
//! Requirements 14.2, 14.3, 14.4, 14.5, 14.8, 14.9, 14.11, 14.12, 14.13, 14.14,
//! 14.16, 14.19, 14.20, 14.22, 14.23, 14.24, 14.27.

mod faults;
mod generate;

use std::collections::{BTreeMap, BTreeSet};

use esr_core::engine::{DesktopView, Event, FoldProbeId, ItemId, Millis, TabKey, WinKey};
use esr_core::model::{LocationId, Rect, ShowState};

pub use faults::{ActivateFault, ActivateMode, CreateTabFault, FoldFaults, TabWindowFault};
pub use generate::{
    desktop_spec_strategy, failure_schedule, fold_faults_strategy, fold_op_strategy, selection_strategy,
};

use crate::explorer::{NavOutcome, Pick, ProcessChoice, Scheduled, SimExplorer, SimWindow};
use crate::restore::Origin;

/// Where a new candidate opens (Explorer cascades new windows).
pub const CANDIDATE_RECT: Rect = Rect { left: 120, top: 80, right: 1020, bottom: 680 };

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Candidate-window knobs of the simulator (part of
/// [`SimConfig`](crate::explorer::SimConfig)).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoldConfig {
    /// Time from the first signal to the first navigation completing, or
    /// `None` if it never does.
    pub first_navigation_ms: Option<u64>,
    /// Time from the first signal until Explorer shows the window.
    pub show_after_ms: u64,
    /// Time from `HideWindow` to the window being transparent.
    pub hide_ms: u64,
    /// Time from `CloseWindow` to the window being closed.
    pub close_ms: u64,
    /// Number of Virtual_Desktops (at least 1).
    pub desktops: u8,
    /// Time from `ProbeFoldContext` to its `FoldContext`.
    pub context_ms: u64,
    /// Drag-out: time from the new window's first signal to the destruction
    /// of the source tab window (`TabDetached`; spike F3: 5–24 ms).
    pub detach_ms: u64,
    /// Drag-out: time from `TabDetached` to Explorer showing the new window
    /// (spike F3: 170–260 ms).
    pub drag_show_gap_ms: u64,
    /// Report `TabStarted` when the tab window of a `CreateTab` appears.
    pub report_tab_started: bool,
    /// Time from the tab window appearing to the tab registering and being
    /// reported by `TabCreated` (F2: about 0.25–0.8 s).
    pub registration_lag_ms: u64,
    /// Time from [`Op::BeginClosing`](crate::explorer::Op::BeginClosing) to
    /// `WindowDestroyed`.
    pub closing_ms: u64,
    /// Injected failures, by command ordinal (see [`faults`]).
    pub faults: FoldFaults,
}

impl Default for FoldConfig {
    fn default() -> Self {
        FoldConfig {
            first_navigation_ms: Some(150),
            show_after_ms: 400,
            hide_ms: 60,
            close_ms: 50,
            desktops: 3,
            context_ms: 10,
            detach_ms: 20,
            drag_show_gap_ms: 215,
            report_tab_started: false,
            registration_lag_ms: 0,
            closing_ms: 250,
            faults: FoldFaults::none(),
        }
    }
}

// ---------------------------------------------------------------------------
// Operation payloads
// ---------------------------------------------------------------------------

/// The `desktop` of a candidate's `WindowAppeared`: which windows are on its
/// Virtual_Desktop. The candidate itself is always on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DesktopSpec {
    /// Membership could not be read (`DesktopView::Unknown`, Req 14.12).
    Unknown,
    /// Every open window.
    All,
    /// The candidate and the open windows these picks choose (each `Pick`
    /// indexes the windows open before the candidate appeared).
    Windows(Vec<Pick>),
    /// What the adapters read from the simulated Virtual_Desktops: the
    /// candidate, the open windows on the current desktop, and the windows
    /// shown on all desktops; `Unknown` while membership is unreadable.
    Current,
}

/// Where a window is: on one Virtual_Desktop or on all of them (Req 14.11).
/// `On(n)` means desktop `n % FoldConfig::desktops`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DesktopPlace {
    On(u8),
    AllDesktops,
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Life of one candidate window, kept after it closes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CandidateRecord {
    pub win: WinKey,
    /// The window's only tab.
    pub tab: TabKey,
    /// What the first navigation shows.
    pub location: LocationId,
    pub selection: Vec<ItemId>,
    /// When `WindowAppeared` was emitted.
    pub first_signal: Millis,
    /// Explorer has shown the window (it is visible unless `hidden`).
    pub shown: bool,
    /// Alpha 0 is applied.
    pub hidden: bool,
    /// When the first `HideWindow` took effect.
    pub hidden_at: Option<Millis>,
    /// When the last `Reveal` took effect.
    pub revealed_at: Option<Millis>,
    /// When the first navigation completed.
    pub navigated_at: Option<Millis>,
    /// For a dragged-out tab: the window it came from.
    pub detached_from: Option<WinKey>,
}

/// Candidate bookkeeping held by [`SimExplorer`].
#[derive(Clone, Debug, Default)]
pub(crate) struct FoldState {
    /// Every candidate ever opened, in order.
    candidates: Vec<CandidateRecord>,
    /// The current Virtual_Desktop.
    current_desktop: u8,
    /// Where each window was last put; a window without an entry is on desktop 0.
    places: BTreeMap<WinKey, DesktopPlace>,
    /// Virtual_Desktop membership cannot be read (`DesktopView::Unknown`).
    membership_unreadable: bool,
    /// Windows that do not answer `WM_NULL`.
    unresponsive: BTreeSet<WinKey>,
    /// Recent foreground changes (`None`: an application), oldest first.
    foreground_history: Vec<(Millis, Option<WinKey>)>,
    /// Fault-injection counters and held-back work.
    faults: faults::FaultState,
}

/// Foreground changes kept in [`SimExplorer::foreground_history`].
const FOREGROUND_HISTORY: usize = 32;

impl FoldState {
    pub(crate) fn set_membership_readable(&mut self, readable: bool) {
        self.membership_unreadable = !readable;
    }

    pub(crate) fn note_foreground(&mut self, at: Millis, foreground: Option<WinKey>) {
        self.foreground_history.push((at, foreground));
        if self.foreground_history.len() > FOREGROUND_HISTORY {
            self.foreground_history.remove(0);
        }
    }

    fn record_mut(&mut self, win: WinKey) -> Option<&mut CandidateRecord> {
        self.candidates.iter_mut().rev().find(|c| c.win == win)
    }
}

/// The effect of a candidate-related command or timer, run when it is due.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Action {
    /// Explorer shows the window.
    Show(WinKey),
    /// The first navigation completes.
    FirstNavigation(WinKey),
    /// `HideWindow` takes effect.
    Hide(WinKey),
    /// `CloseWindow` takes effect.
    Close(WinKey),
    /// The `FoldContext` of a probe is due.
    Context { probe: FoldProbeId, candidate: WinKey },
    /// A dragged-out window's tab registered and showed its Location.
    DragLocation(WinKey),
    /// A window that was the target of a command closes (fault injection).
    TargetCloses(WinKey),
    /// A closing window is destroyed (fault injection).
    Destroyed { win: WinKey, pid: u32 },
    /// A tab whose registration was delayed registers (fault injection).
    RegisterTab { win: WinKey, tab: TabKey },
}

// ---------------------------------------------------------------------------
// SimExplorer: virtual desktops, foreground
// ---------------------------------------------------------------------------

impl SimExplorer {
    /// The current Virtual_Desktop.
    pub fn current_desktop(&self) -> u8 {
        self.fold.current_desktop
    }

    /// Where `win` is (windows created before any desktop operation are on
    /// desktop 0, the initial current one). `None` for an unknown window.
    pub fn desktop_of(&self, win: WinKey) -> Option<DesktopPlace> {
        self.wi(win)?;
        Some(self.fold.places.get(&win).copied().unwrap_or(DesktopPlace::On(0)))
    }

    /// Whether `win` is open and on the current Virtual_Desktop (a window on
    /// all desktops is).
    pub fn on_current_desktop(&self, win: WinKey) -> bool {
        match self.desktop_of(win) {
            Some(DesktopPlace::AllDesktops) => true,
            Some(DesktopPlace::On(d)) => self.normal_desktop(d) == self.fold.current_desktop,
            None => false,
        }
    }

    /// Whether Virtual_Desktop membership can be read.
    pub fn desktop_membership_readable(&self) -> bool {
        !self.fold.membership_unreadable
    }

    /// The recent foreground changes (`None`: an application took the
    /// foreground), oldest first, with the time of each.
    pub fn foreground_history(&self) -> &[(Millis, Option<WinKey>)] {
        &self.fold.foreground_history
    }

    /// Make `win` (not) answer `WM_NULL` in time. What a hung window would
    /// have done (see [`faults`]) happens when it answers again.
    pub fn set_unresponsive(&mut self, win: WinKey, unresponsive: bool) {
        if unresponsive {
            self.fold.unresponsive.insert(win);
        } else if self.fold.unresponsive.remove(&win) {
            self.release_stalled(win);
        }
    }

    /// The open windows the adapters report for the Virtual_Desktop of
    /// `candidate`: the windows on its desktop and those on all desktops.
    /// `Unknown` when membership is unreadable or the candidate is gone.
    pub fn desktop_view_of(&self, candidate: WinKey) -> DesktopView {
        let Some(place) = self.desktop_of(candidate) else { return DesktopView::Unknown };
        if self.fold.membership_unreadable {
            return DesktopView::Unknown;
        }
        let place = self.normal_place(place);
        DesktopView::Known(
            self.windows
                .iter()
                .map(|w| w.key)
                .filter(|w| {
                    let other = self.normal_place(self.desktop_of(*w).unwrap_or(DesktopPlace::On(0)));
                    place == DesktopPlace::AllDesktops || other == DesktopPlace::AllDesktops || other == place
                })
                .collect(),
        )
    }

    fn normal_desktop(&self, d: u8) -> u8 {
        d % self.config.fold.desktops.max(1)
    }

    fn normal_place(&self, place: DesktopPlace) -> DesktopPlace {
        match place {
            DesktopPlace::On(d) => DesktopPlace::On(self.normal_desktop(d)),
            DesktopPlace::AllDesktops => DesktopPlace::AllDesktops,
        }
    }

    /// A new window is on the current desktop.
    pub(crate) fn fold_window_created(&mut self, win: WinKey) {
        self.fold.places.insert(win, DesktopPlace::On(self.fold.current_desktop));
    }

    /// Make `front` the foreground window (`None`: an application), noting it
    /// in the history.
    pub(crate) fn set_foreground(&mut self, front: Option<WinKey>) {
        if self.foreground != front {
            self.foreground = front;
            let now = self.now;
            self.fold.note_foreground(now, front);
        }
    }

    /// Activating a window on another desktop switches to that desktop.
    pub(crate) fn follow_to_desktop(&mut self, win: WinKey) {
        if let Some(DesktopPlace::On(d)) = self.desktop_of(win) {
            self.fold.current_desktop = self.normal_desktop(d);
        }
    }

    /// An application holds the foreground unless the foreground window is
    /// still on the current desktop.
    fn drop_foreground_off_desktop(&mut self) {
        if let Some(front) = self.foreground
            && !self.on_current_desktop(front)
        {
            self.set_foreground(None);
        }
    }

    pub(crate) fn switch_desktop(&mut self, desktop: u8) {
        self.fold.current_desktop = self.normal_desktop(desktop);
        self.drop_foreground_off_desktop();
    }

    pub(crate) fn move_to_desktop(&mut self, win: Pick, to: DesktopPlace) {
        let Some(wi) = win.of(self.windows.len()) else { return };
        let key = self.windows[wi].key;
        let to = self.normal_place(to);
        self.fold.places.insert(key, to);
        self.drop_foreground_off_desktop();
    }

    /// The `DesktopView` of a candidate that is about to appear.
    fn desktop_spec_view(&self, spec: &DesktopSpec, open: &[WinKey], candidate: WinKey) -> DesktopView {
        match spec {
            DesktopSpec::Unknown => DesktopView::Unknown,
            DesktopSpec::All => DesktopView::Known(open.iter().copied().chain([candidate]).collect()),
            DesktopSpec::Windows(picks) => DesktopView::Known(
                picks.iter().filter_map(|p| p.of(open.len()).map(|i| open[i])).chain([candidate]).collect(),
            ),
            DesktopSpec::Current if self.fold.membership_unreadable => DesktopView::Unknown,
            DesktopSpec::Current => DesktopView::Known(
                open.iter().copied().filter(|w| self.on_current_desktop(*w)).chain([candidate]).collect(),
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// SimExplorer: candidates
// ---------------------------------------------------------------------------

impl SimExplorer {
    /// Every candidate window ever opened, in order.
    pub fn candidates(&self) -> &[CandidateRecord] {
        &self.fold.candidates
    }

    pub fn candidate(&self, win: WinKey) -> Option<&CandidateRecord> {
        self.fold.candidates.iter().rev().find(|c| c.win == win)
    }

    pub(crate) fn open_candidate(
        &mut self,
        process: ProcessChoice,
        location: Pick,
        selection: &[ItemId],
        bypass_keys: bool,
        desktop: &DesktopSpec,
        foreground: Option<Pick>,
    ) {
        if self.windows.len() >= self.config.max_windows {
            return;
        }
        let Some(pid) = self.pick_process(process) else { return };
        let location = self.location(location);
        let open = self.windows.len();
        let foreground = foreground.and_then(|p| p.of(open).map(|i| self.windows[i].key));
        self.spawn_candidate(pid, location, selection, bypass_keys, desktop, foreground);
    }

    /// Ctrl+N or "New window": a candidate on the folder of the foreground
    /// Explorer window''s Active_Tab, in that window''s process.
    pub(crate) fn new_window(&mut self, bypass_keys: bool) {
        let Some(front) = self.foreground else { return };
        if self.windows.len() >= self.config.max_windows {
            return;
        }
        let Some(w) = self.window(front) else { return };
        let (pid, location) = (w.pid, w.tabs.get(w.active).and_then(|t| t.last_known.clone()));
        let Some(location) = location else { return };
        self.spawn_candidate(pid, location, &[], bypass_keys, &DesktopSpec::Current, Some(front));
    }

    fn spawn_candidate(
        &mut self,
        pid: u32,
        location: LocationId,
        selection: &[ItemId],
        bypass_keys: bool,
        desktop: &DesktopSpec,
        foreground: Option<WinKey>,
    ) {
        let key = WinKey { hwnd: self.alloc_hwnd(), seq: self.alloc_seq() };

        // The tab has not navigated: it shows nothing and is not registered.
        let mut tab = self.new_tab(location.clone(), NavOutcome::Pending);
        tab.pending = None;
        let tab_key = tab.key;
        let placement = self.placement(CANDIDATE_RECT, ShowState::Normal);

        let open: Vec<WinKey> = self.windows.iter().map(|w| w.key).collect();
        let desktop = self.desktop_spec_view(desktop, &open, key);

        self.windows.push(SimWindow { key, pid, tabs: vec![tab], active: 0, placement });
        // Not visible yet: the window was never visible at its first signal.
        self.track_window(key, Origin::User, false);
        let wi = self.windows.len() - 1;
        self.sync(wi);

        let first_signal = self.now;
        self.fold.candidates.push(CandidateRecord {
            win: key,
            tab: tab_key,
            location,
            selection: selection.to_vec(),
            first_signal,
            shown: false,
            hidden: false,
            hidden_at: None,
            revealed_at: None,
            navigated_at: None,
            detached_from: None,
        });
        self.emit(Event::WindowAppeared { win: key, pid, bypass_keys, desktop, foreground });
        self.activate(key);

        self.after_fold(self.config.fold.show_after_ms, Action::Show(key));
        if let Some(ms) = self.config.fold.first_navigation_ms {
            self.after_fold(ms, Action::FirstNavigation(key));
        }
    }

    /// A tab was dragged out of `from` into the new window `win` (already
    /// created, not visible): announce it as a candidate. The source tab
    /// window''s destruction (`TabDetached`) was scheduled by the caller.
    pub(crate) fn announce_drag_out(&mut self, win: WinKey, tab: TabKey, from: WinKey, location: LocationId) {
        let Some(pid) = self.window(win).map(|w| w.pid) else { return };
        let open: Vec<WinKey> = self.windows.iter().map(|w| w.key).collect();
        let desktop = self.desktop_spec_view(&DesktopSpec::Current, &open, win);
        self.fold.candidates.push(CandidateRecord {
            win,
            tab,
            location,
            selection: Vec::new(),
            first_signal: self.now,
            shown: false,
            hidden: false,
            hidden_at: None,
            revealed_at: None,
            navigated_at: None,
            detached_from: Some(from),
        });
        self.emit(Event::WindowAppeared { win, pid, bypass_keys: false, desktop, foreground: Some(from) });
        let ms = self.config.fold.detach_ms + self.config.fold.drag_show_gap_ms;
        self.after_fold(ms, Action::Show(win));
    }

    /// The dragged-out tab registers (its `Register` was scheduled first) and
    /// the adapters learn the window''s Location.
    pub(crate) fn after_drag_registration(&mut self, win: WinKey) {
        self.after_fold(self.config.drag_register_delay_ms, Action::DragLocation(win));
    }

    /// "Duplicate tab": a copy of the Active_Tab, next to it; no window.
    pub(crate) fn duplicate_tab(&mut self, win: Pick) {
        let Some(wi) = win.of(self.windows.len()) else { return };
        let w = &self.windows[wi];
        if w.tabs.len() >= self.config.max_tabs_per_window {
            return;
        }
        let Some(location) = w.tabs.get(w.active).and_then(|t| t.last_known.clone()) else { return };
        let tab = self.new_tab(location, NavOutcome::Complete);
        let w = &mut self.windows[wi];
        let pos = w.active + 1;
        w.tabs.insert(pos, tab);
        w.active = pos;
        let key = w.key;
        self.sync(wi);
        self.activate(key);
        self.window_changed(key);
        self.register(wi, pos);
    }
    fn after_fold(&mut self, ms: u64, action: Action) {
        self.schedule(self.now + ms, Scheduled::Fold(action));
    }

    /// `HideWindow { win }` was received.
    pub(crate) fn fold_hide_command(&mut self, win: WinKey) {
        if self.candidate(win).is_some() && self.wi(win).is_some() {
            self.after_fold(self.config.fold.hide_ms, Action::Hide(win));
        }
    }

    /// `ProbeFoldContext { probe, candidate }` was received: answered with a
    /// fresh view after [`FoldConfig::context_ms`].
    pub(crate) fn fold_probe_command(&mut self, probe: FoldProbeId, candidate: WinKey) {
        self.after_fold(self.config.fold.context_ms, Action::Context { probe, candidate });
    }

    /// `CloseWindow { win }` was received. A candidate (or any window) that
    /// ignores it (fault injection) stays open.
    pub(crate) fn fold_close_command(&mut self, win: WinKey) {
        if self.wi(win).is_some() && !self.close_is_ignored(win) {
            self.after_fold(self.config.fold.close_ms, Action::Close(win));
        }
    }

    /// A due `Reveal`: for a candidate, remove alpha 0 (the window becomes
    /// visible only if Explorer has shown it). Returns `false` for any other
    /// window.
    pub(crate) fn fold_reveal(&mut self, win: WinKey) -> bool {
        if self.candidate(win).is_none() {
            return false;
        }
        if self.wi(win).is_some() {
            let now = self.now;
            if let Some(c) = self.fold.record_mut(win) {
                c.hidden = false;
                c.revealed_at = Some(now);
            }
            self.refresh_visibility(win);
        }
        true
    }

    /// Run a due candidate action.
    pub(crate) fn run_fold_action(&mut self, action: Action) {
        let now = self.now;
        match action {
            Action::Show(win) => {
                if self.wi(win).is_none() {
                    return;
                }
                if let Some(c) = self.fold.record_mut(win) {
                    c.shown = true;
                }
                self.refresh_visibility(win);
            }
            Action::Hide(win) => {
                if self.wi(win).is_none() {
                    return;
                }
                if let Some(c) = self.fold.record_mut(win) {
                    c.hidden = true;
                    c.hidden_at.get_or_insert(now);
                }
                self.refresh_visibility(win);
            }
            Action::FirstNavigation(win) => self.first_navigation(win),
            Action::Close(win) => self.close_window_now(win),
            Action::Context { probe, candidate } => {
                let desktop = self.desktop_view_of(candidate);
                let unresponsive = self.fold.unresponsive.iter().copied().filter(|w| self.wi(*w).is_some()).collect();
                self.emit(Event::FoldContext { probe, desktop, unresponsive });
            }
            Action::DragLocation(win) => self.drag_location(win),
            Action::TargetCloses(win) => self.start_closing(win, 0),
            Action::Destroyed { win, pid } => self.destroyed(win, pid),
            Action::RegisterTab { win, tab } => self.register_tab_now(win, tab),
        }
    }

    fn drag_location(&mut self, win: WinKey) {
        let Some(c) = self.candidate(win).cloned() else { return };
        if self.wi(win).is_none() {
            return;
        }
        let now = self.now;
        if let Some(r) = self.fold.record_mut(win) {
            r.navigated_at = Some(now);
        }
        if !self.shell_lost {
            self.emit(Event::InitialLocationKnown { win, tab: Some(c.tab), location: c.location, selection: Vec::new() });
        }
    }

    /// Visible means shown by Explorer and not transparent.
    fn refresh_visibility(&mut self, win: WinKey) {
        let Some(placement) = self.window(win).map(|w| w.placement.clone()) else { return };
        let Some(visible) = self.candidate(win).map(|c| c.shown && !c.hidden) else { return };
        let now = self.now;
        if let Some(r) = self.restore.record_mut(win) {
            r.visible = visible;
            if visible {
                r.first_visible.get_or_insert((now, placement));
            }
        }
    }

    fn first_navigation(&mut self, win: WinKey) {
        let Some(c) = self.candidate(win).cloned() else { return };
        let Some((wi, ti)) = self.find_tab(c.tab) else { return };
        let now = self.now;
        if let Some(r) = self.fold.record_mut(win) {
            r.navigated_at = Some(now);
        }
        // Shows the Location, then registers the tab in ShellWindows, which
        // reports the completed navigation.
        self.complete_navigation(wi, ti, Some(c.location.clone()));
        self.register(wi, ti);
        if !c.selection.is_empty() {
            self.restore.set_selection(c.tab, c.selection.clone());
        }
        // A window that matches an outstanding request is claimed by it, like
        // any user-opened window; otherwise the adapters announce it.
        if self.config.restore.claim_user_windows
            && let Some(request) = self.restore.claim(&c.location)
        {
            self.claimed(win, request);
        } else if !self.shell_lost {
            self.emit(Event::InitialLocationKnown {
                win,
                tab: Some(c.tab),
                location: c.location,
                selection: c.selection,
            });
        }
    }

    /// `WM_SYSCOMMAND SC_CLOSE` takes effect: the registered tabs are revoked,
    /// then the window is destroyed.
    fn close_window_now(&mut self, win: WinKey) {
        self.start_closing(win, 0);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explorer::{default_locations, Op, SimConfig, TimedEvent};
    use crate::restore::CommandKind;
    use esr_core::engine::Command;
    use proptest::prelude::*;

    fn sim() -> SimExplorer {
        SimExplorer::new(SimConfig::default())
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
        s.windows()[0].key
    }

    fn candidate_op(location: u16, selection: Vec<ItemId>, foreground: Option<Pick>) -> Op {
        Op::OpenCandidate {
            location: Pick(location),
            selection,
            bypass_keys: false,
            desktop: DesktopSpec::All,
            foreground,
            process: ProcessChoice::Main,
        }
    }

    fn events(ev: &[TimedEvent]) -> Vec<&Event> {
        ev.iter().map(|e| &e.event).collect()
    }

    fn kinds(s: &SimExplorer, kind: CommandKind) -> Vec<Millis> {
        s.commands_of(kind).iter().map(|c| c.at).collect()
    }

    #[test]
    fn first_signal_then_activation_then_delayed_first_navigation() {
        let mut s = sim();
        let primary = open_window(&mut s);
        let t0 = s.now();
        let item = ItemId(vec![1, 2, 3]);
        let ev = s.apply(&candidate_op(3, vec![item.clone()], Some(Pick(0))));
        let cand = s.windows()[1].key;
        let pid = s.windows()[1].pid;
        assert_eq!(
            events(&ev),
            vec![
                &Event::WindowAppeared {
                    win: cand,
                    pid,
                    bypass_keys: false,
                    desktop: DesktopView::Known([primary, cand].into_iter().collect()),
                    foreground: Some(primary),
                },
                &Event::WindowActivated { win: cand },
            ]
        );
        assert!(ev.iter().all(|e| e.at == t0));
        assert!(!s.is_visible(cand));
        assert!(s.observe(cand).unwrap().tabs.is_empty(), "not in ShellWindows yet");

        let tab = s.window(cand).unwrap().tabs[0].key;
        let ev = s.tick(150);
        let loc = default_locations()[3].clone();
        let expected = [
            Event::NavigationCompleted { tab, location: loc.clone() },
            Event::InitialLocationKnown { win: cand, tab: Some(tab), location: loc, selection: vec![item.clone()] },
        ];
        let got: Vec<&Event> = events(&ev).into_iter().filter(|e| !matches!(e, Event::WindowScanned(_))).collect();
        assert_eq!(got, expected.iter().collect::<Vec<_>>());
        assert!(ev.iter().take(2).all(|e| e.at == t0 + 150));
        assert_eq!(s.selection(tab), &[item]);
    }

    #[test]
    fn no_first_navigation_means_no_initial_location() {
        let mut s = SimExplorer::new(SimConfig {
            fold: FoldConfig { first_navigation_ms: None, ..FoldConfig::default() },
            ..SimConfig::default()
        });
        open_window(&mut s);
        s.apply(&candidate_op(3, vec![], None));
        let ev = s.tick(5_000);
        assert!(ev.iter().all(|e| !matches!(e.event, Event::InitialLocationKnown { .. } | Event::NavigationCompleted { .. })));
        assert!(s.observe(s.windows()[1].key).unwrap().tabs.is_empty());
    }

    #[test]
    fn unhidden_candidate_becomes_visible_when_explorer_shows_it() {
        let mut s = sim();
        open_window(&mut s);
        s.apply(&candidate_op(3, vec![], None));
        let cand = s.windows()[1].key;
        let t0 = s.now();
        s.tick(399);
        assert!(!s.is_visible(cand));
        s.tick(1);
        assert!(s.is_visible(cand));
        assert_eq!(s.window_record(cand).unwrap().first_visible.as_ref().map(|f| f.0), Some(t0 + 400));
    }

    #[test]
    fn hidden_candidate_stays_invisible_until_revealed() {
        let mut s = sim();
        open_window(&mut s);
        s.apply(&candidate_op(3, vec![], None));
        let cand = s.windows()[1].key;
        let t0 = s.now();
        assert!(s.handle_command(&Command::HideWindow { win: cand }));
        s.tick(1_000);
        assert!(!s.is_visible(cand));
        assert_eq!(s.window_record(cand).unwrap().first_visible, None, "never flashed");
        assert_eq!(s.candidate(cand).unwrap().hidden_at, Some(t0 + 60));

        s.handle_command(&Command::Reveal { win: cand });
        s.tick(1);
        assert!(s.is_visible(cand));
        assert_eq!(s.candidate(cand).unwrap().revealed_at, Some(t0 + 1_001));
        assert_eq!(kinds(&s, CommandKind::HideWindow), vec![t0]);
        assert_eq!(kinds(&s, CommandKind::Reveal), vec![t0 + 1_000]);
    }

    #[test]
    fn hide_that_arrives_after_the_window_is_shown_records_the_flash() {
        let mut s = sim();
        open_window(&mut s);
        s.apply(&candidate_op(3, vec![], None));
        let cand = s.windows()[1].key;
        s.tick(450);
        assert!(s.is_visible(cand));
        s.handle_command(&Command::HideWindow { win: cand });
        s.tick(60);
        assert!(!s.is_visible(cand));
        assert!(s.window_record(cand).unwrap().first_visible.is_some());
    }

    #[test]
    fn close_window_revokes_registered_tab_and_destroys_the_window() {
        let mut s = sim();
        let primary = open_window(&mut s);
        s.apply(&candidate_op(3, vec![], None));
        let cand = s.windows()[1].key;
        let tab = s.window(cand).unwrap().tabs[0].key;
        s.tick(200);
        s.handle_command(&Command::HideWindow { win: cand });
        let t_close = s.now();
        s.handle_command(&Command::CloseWindow { win: cand });
        let ev = s.tick(50);
        let got = events(&ev);
        assert!(got.contains(&&Event::TabRevoked { win: cand, tab }));
        assert!(got.contains(&&Event::WindowDestroyed { win: cand }));
        assert!(ev.iter().all(|e| e.at == t_close + 50));
        assert!(s.window(cand).is_none());
        assert_eq!(s.window_record(cand).unwrap().closed_at, Some(t_close + 50));
        assert_eq!(kinds(&s, CommandKind::CloseWindow), vec![t_close]);
        assert!(s.reference().window(cand).is_none());
        assert!(s.reference().window(primary).is_some());
        assert!(s.reference().session_ends().is_empty(), "a closed candidate is no Session_End");
    }

    #[test]
    fn candidate_matching_an_outstanding_request_is_claimed() {
        use esr_core::engine::RequestId;
        use esr_core::plan::placement::FinalPlacement;
        let mut s = sim();
        open_window(&mut s);
        let loc = default_locations()[3].clone();
        let r = Rect { left: 0, top: 0, right: 800, bottom: 600 };
        s.handle_command(&Command::OpenWindow {
            request: RequestId(7),
            location: loc,
            placement: FinalPlacement { state: ShowState::Normal, restored: r, monitor: r, work: r },
        });
        s.apply(&candidate_op(3, vec![], None));
        let cand = s.windows()[1].key;
        let ev = s.tick(150);
        assert!(events(&ev).iter().any(|e| matches!(e, Event::WindowOpened { request, win, .. } if *request == RequestId(7) && *win == cand)));
        assert!(events(&ev).iter().all(|e| !matches!(e, Event::InitialLocationKnown { win, .. } if *win == cand)));
    }

    fn current_candidate(location: u16) -> Op {
        Op::OpenCandidate {
            location: Pick(location),
            selection: vec![],
            bypass_keys: false,
            desktop: DesktopSpec::Current,
            foreground: None,
            process: ProcessChoice::Main,
        }
    }

    fn appeared(ev: &[TimedEvent]) -> (DesktopView, Option<WinKey>) {
        ev.iter()
            .find_map(|e| match &e.event {
                Event::WindowAppeared { desktop, foreground, .. } => Some((desktop.clone(), *foreground)),
                _ => None,
            })
            .expect("a WindowAppeared")
    }

    fn open_on(s: &mut SimExplorer, place: DesktopPlace) -> WinKey {
        let n = s.windows().len();
        s.apply(&Op::OpenWindow {
            process: ProcessChoice::Main,
            location: Pick(2),
            nav: NavOutcome::Complete,
            rect: Rect { left: 100, top: 100, right: 900, bottom: 700 },
            maximized: false,
        });
        s.settle();
        let win = s.windows()[n].key;
        s.apply(&Op::MoveToDesktop { win: Pick(n as u16), to: place });
        win
    }

    #[test]
    fn candidate_view_holds_the_current_desktop_and_all_desktop_windows() {
        let mut s = sim();
        let a = open_on(&mut s, DesktopPlace::On(0));
        let b = open_on(&mut s, DesktopPlace::On(1));
        let c = open_on(&mut s, DesktopPlace::AllDesktops);
        s.apply(&Op::SwitchDesktop { desktop: 1 });
        assert_eq!(s.current_desktop(), 1);
        let ev = s.apply(&current_candidate(3));
        let cand = s.windows()[3].key;
        assert_eq!(appeared(&ev).0, DesktopView::Known([b, c, cand].into_iter().collect()));
        assert_eq!(s.desktop_of(cand), Some(DesktopPlace::On(1)), "created on the current desktop");

        s.apply(&Op::SwitchDesktop { desktop: 0 });
        let ev = s.apply(&current_candidate(3));
        let cand2 = s.windows()[4].key;
        assert_eq!(appeared(&ev).0, DesktopView::Known([a, c, cand2].into_iter().collect()));
    }

    #[test]
    fn unreadable_membership_gives_unknown_views() {
        let mut s = sim();
        open_window(&mut s);
        s.apply(&Op::SetDesktopMembership { readable: false });
        assert!(!s.desktop_membership_readable());
        let ev = s.apply(&current_candidate(3));
        assert_eq!(appeared(&ev).0, DesktopView::Unknown);
        let cand = s.windows()[1].key;
        s.handle_command(&Command::ProbeFoldContext { probe: FoldProbeId(1), candidate: cand });
        let ev = s.tick(10);
        assert!(events(&ev).contains(&&Event::FoldContext {
            probe: FoldProbeId(1),
            desktop: DesktopView::Unknown,
            unresponsive: BTreeSet::new(),
        }));
    }

    #[test]
    fn fold_context_is_fresh_and_delayed() {
        let mut s = sim();
        let a = open_on(&mut s, DesktopPlace::On(0));
        let b = open_on(&mut s, DesktopPlace::On(1));
        s.apply(&current_candidate(3));
        let cand = s.windows()[2].key;
        s.settle();
        let t0 = s.now();
        let probe = FoldProbeId(4);
        assert!(s.handle_command(&Command::ProbeFoldContext { probe, candidate: cand }));
        // The desktop of `b` changes while the probe is in flight.
        s.apply(&Op::MoveToDesktop { win: Pick(1), to: DesktopPlace::On(0) });
        s.set_unresponsive(a, true);
        assert!(s.tick(9).is_empty());
        let ev = s.tick(1);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].at, t0 + 10);
        assert_eq!(
            ev[0].event,
            Event::FoldContext {
                probe,
                desktop: DesktopView::Known([a, b, cand].into_iter().collect()),
                unresponsive: [a].into_iter().collect(),
            }
        );
        assert_eq!(kinds(&s, CommandKind::ProbeFoldContext), vec![t0]);
    }

    #[test]
    fn switching_away_from_the_foreground_window_leaves_an_application_in_front() {
        let mut s = sim();
        let a = open_on(&mut s, DesktopPlace::On(0));
        assert_eq!(s.foreground(), Some(a));
        s.apply(&Op::SwitchDesktop { desktop: 2 });
        assert_eq!(s.foreground(), None);
        // Activating a window on another desktop switches to it.
        s.apply(&Op::ActivateWindow { win: Pick(0) });
        assert_eq!((s.current_desktop(), s.foreground()), (0, Some(a)));
        // An all-desktop window stays in front on every desktop.
        s.apply(&Op::MoveToDesktop { win: Pick(0), to: DesktopPlace::AllDesktops });
        s.apply(&Op::SwitchDesktop { desktop: 1 });
        assert_eq!(s.foreground(), Some(a));
        s.apply(&Op::FocusApplication);
        let history: Vec<Option<WinKey>> = s.foreground_history().iter().map(|h| h.1).collect();
        assert_eq!(history, vec![Some(a), None, Some(a), None]);
    }

    #[test]
    fn new_window_opens_the_foreground_active_tab_folder_with_the_foreground_window() {
        let mut s = sim();
        let primary = open_window(&mut s);
        s.apply(&Op::OpenTab { win: Pick(0), location: Pick(4), nav: NavOutcome::Complete });
        s.settle();
        let ev = s.apply(&Op::NewWindow { bypass_keys: true });
        let cand = s.windows()[1].key;
        let (desktop, foreground) = appeared(&ev);
        assert_eq!(foreground, Some(primary));
        assert_eq!(desktop, DesktopView::Known([primary, cand].into_iter().collect()));
        assert!(matches!(&ev[0].event, Event::WindowAppeared { bypass_keys: true, pid, .. } if *pid == s.windows()[0].pid));
        assert_eq!(s.candidate(cand).unwrap().location, default_locations()[4], "the active tab's folder");

        // An application in the foreground: no Ctrl+N; "Show in folder" has no foreground.
        s.apply(&Op::FocusApplication);
        assert!(s.apply(&Op::NewWindow { bypass_keys: false }).is_empty());
        let ev = s.apply(&current_candidate(5));
        assert_eq!(appeared(&ev).1, None);
    }

    #[test]
    fn duplicate_tab_adds_a_tab_and_no_window() {
        let mut s = sim();
        let win = open_window(&mut s);
        s.apply(&Op::OpenTab { win: Pick(0), location: Pick(4), nav: NavOutcome::Complete });
        s.apply(&Op::ActivateTab { win: Pick(0), tab: Pick(0) });
        s.settle();
        let ev = s.apply(&Op::DuplicateTab { win: Pick(0) });
        let ev = [ev, s.settle()].concat();
        assert!(ev.iter().all(|e| !matches!(e.event, Event::WindowAppeared { .. } | Event::TabDetached { .. })));
        let w = s.window(win).unwrap();
        assert_eq!(w.tabs.len(), 3);
        assert_eq!(w.active, 1, "next to the original, selected");
        assert_eq!(w.tabs[1].last_known, w.tabs[0].last_known);
        assert_eq!(s.windows().len(), 1);
        assert!(s.candidates().is_empty());
    }

    #[test]
    fn drag_out_announces_the_window_then_detaches_the_source_tab() {
        let mut s = sim();
        let src = open_window(&mut s);
        s.apply(&Op::OpenTab { win: Pick(0), location: Pick(4), nav: NavOutcome::Complete });
        s.settle();
        let t0 = s.now();
        let dragged = s.windows()[0].tabs[1].key;
        let ev = s.apply(&Op::DragOutCandidate { win: Pick(0), tab: Pick(1), dx: 40, dy: 40 });
        let cand = s.windows()[1].key;
        let (desktop, foreground) = appeared(&ev);
        assert_eq!((foreground, desktop), (Some(src), DesktopView::Known([src, cand].into_iter().collect())));
        let first = events(&ev).iter().position(|e| matches!(e, Event::WindowAppeared { .. })).unwrap();
        assert!(!events(&ev)[..first].iter().any(|e| matches!(e, Event::TabDetached { .. } | Event::TabRevoked { .. })));
        assert!(!s.is_visible(cand));

        // 20 ms later the source tab window is destroyed, next to the first signal.
        let ev = s.tick(20);
        assert_eq!(
            events(&ev).into_iter().filter(|e| !matches!(e, Event::WindowScanned(_))).collect::<Vec<_>>(),
            vec![&Event::TabDetached { from: src }, &Event::TabRevoked { win: src, tab: dragged }]
        );
        assert!(ev.iter().take(2).all(|e| e.at == t0 + 20));
        assert_eq!(s.candidate(cand).unwrap().detached_from, Some(src));

        // Explorer shows it 215 ms after that; the moved tab registers at +200.
        let ev = s.tick(180);
        let loc = default_locations()[4].clone();
        let new_tab = s.window(cand).unwrap().tabs[0].key;
        assert!(events(&ev).contains(&&Event::NavigationCompleted { tab: new_tab, location: loc.clone() }));
        assert!(events(&ev).contains(&&Event::InitialLocationKnown {
            win: cand,
            tab: Some(new_tab),
            location: loc,
            selection: vec![],
        }));
        s.tick(34);
        assert!(!s.is_visible(cand));
        s.tick(1);
        assert!(s.is_visible(cand));
        assert_eq!(s.window_record(cand).unwrap().first_visible.as_ref().map(|f| f.0), Some(t0 + 235));
    }

    #[test]
    fn drag_out_candidate_hidden_in_time_never_flashes() {
        let mut s = sim();
        open_window(&mut s);
        s.apply(&Op::OpenTab { win: Pick(0), location: Pick(4), nav: NavOutcome::Complete });
        s.apply(&Op::DragOutCandidate { win: Pick(0), tab: Pick(0), dx: 0, dy: 0 });
        let cand = s.windows()[1].key;
        s.handle_command(&Command::HideWindow { win: cand });
        s.tick(2_000);
        assert!(!s.is_visible(cand));
        assert_eq!(s.window_record(cand).unwrap().first_visible, None);
        s.handle_command(&Command::Reveal { win: cand });
        s.tick(1);
        assert!(s.is_visible(cand));
    }

    #[test]
    fn drag_out_candidate_needs_a_known_location_and_the_current_desktop() {
        let mut s = sim();
        open_window(&mut s);
        s.apply(&Op::OpenTab { win: Pick(0), location: Pick(4), nav: NavOutcome::Unidentifiable });
        s.settle();
        assert!(s.apply(&Op::DragOutCandidate { win: Pick(0), tab: Pick(1), dx: 0, dy: 0 }).is_empty());
        s.apply(&Op::OpenTab { win: Pick(0), location: Pick(4), nav: NavOutcome::Complete });
        s.apply(&Op::MoveToDesktop { win: Pick(0), to: DesktopPlace::On(1) });
        s.apply(&Op::SwitchDesktop { desktop: 0 });
        assert!(s.apply(&Op::DragOutCandidate { win: Pick(0), tab: Pick(2), dx: 0, dy: 0 }).is_empty());
        assert_eq!(s.windows().len(), 1);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        // After any sequence of desktop operations, a candidate's view is
        // exactly the open windows on its desktop plus the all-desktop ones,
        // and always names the candidate.
        #[test]
        fn desktop_view_follows_the_model(
            steps in proptest::collection::vec((0u8..3, any::<u16>(), 0u8..4), 1..24),
            location in any::<u16>(),
        ) {
            let mut s = sim();
            for i in 0..4u16 {
                s.apply(&Op::OpenWindow {
                    process: ProcessChoice::Main,
                    location: Pick(i),
                    nav: NavOutcome::Complete,
                    rect: Rect { left: 100, top: 100, right: 900, bottom: 700 },
                    maximized: false,
                });
            }
            for (kind, pick, d) in steps {
                let op = match kind {
                    0 => Op::SwitchDesktop { desktop: d },
                    1 => Op::MoveToDesktop { win: Pick(pick), to: DesktopPlace::On(d) },
                    _ => Op::MoveToDesktop { win: Pick(pick), to: DesktopPlace::AllDesktops },
                };
                s.apply(&op);
            }
            let ev = s.apply(&current_candidate(location));
            let Some(cand) = s.windows().get(4).map(|w| w.key) else { return Ok(()) };
            let (view, _) = appeared(&ev);
            let DesktopView::Known(set) = view else { return Err(TestCaseError::fail("unreadable")) };
            prop_assert!(set.contains(&cand));
            for w in s.windows() {
                let expected = w.key == cand
                    || s.desktop_of(w.key) == Some(DesktopPlace::AllDesktops)
                    || s.desktop_of(w.key) == s.desktop_of(cand);
                prop_assert_eq!(set.contains(&w.key), expected);
            }
            prop_assert_eq!(s.desktop_view_of(cand), DesktopView::Known(set));
        }

        // Hiding before Explorer shows the window keeps it invisible at any
        // moment, whatever the hide latency below the show delay.
        #[test]
        fn candidate_hidden_in_time_is_never_visible(
            hide_ms in 0u64..400,
            first_nav in proptest::option::of(0u64..2_000),
            location in any::<u16>(),
            ticks in proptest::collection::vec(1u64..120, 1..20),
        ) {
            let mut s = SimExplorer::new(SimConfig {
                fold: FoldConfig { hide_ms, first_navigation_ms: first_nav, ..FoldConfig::default() },
                ..SimConfig::default()
            });
            open_window(&mut s);
            s.apply(&candidate_op(location, vec![], None));
            let cand = s.windows()[1].key;
            s.handle_command(&Command::HideWindow { win: cand });
            for ms in ticks {
                s.tick(ms);
                prop_assert!(!s.is_visible(cand));
            }
            prop_assert_eq!(s.window_record(cand).unwrap().first_visible.clone(), None);
        }
    }
}
