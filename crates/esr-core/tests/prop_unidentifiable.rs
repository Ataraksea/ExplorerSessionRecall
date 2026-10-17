// Feature: explorer-session-restore, Property 7: Unidentifiable locations keep the last known identifier
//! Property test for unidentifiable Locations in the `LiveSessionTracker`.
//!
//! **Property 7: Unidentifiable locations keep the last known identifier**
//!
//! *For any* tab history in which some navigations report no
//! Location_Identifier, the captured location of each tab is the most recent
//! identified location. Tabs that never had one are absent from the capture.
//! Each unidentified navigation produces one `UNIDENTIFIABLE-LOCATION` log
//! entry.
//!
//! **Validates: Requirements 8.8**
//!
//! Two tests:
//!
//! - `event_histories_*` drives the tracker directly with the events the
//!   adapters post (`NavigationCompleted`, `WindowScanned`, `TabRevoked`,
//!   tracker timers) for per-tab navigation histories, and checks the capture
//!   (exact order) after every step and the `UNIDENTIFIABLE-LOCATION` entries.
//!   An unidentified navigation is reported only by the window rescan that
//!   follows it (`ObservedLocation::Unidentifiable`, esr-win
//!   `shell_windows_source`), so two unidentified navigations in a row in the
//!   same tab look the same as one: the entry is expected once per transition
//!   of a tab into the unidentified state, with `kept_previous` telling
//!   whether the tab had an identifier to keep.
//! - `simulated_sessions_*` plays esr-sim operation sequences with
//!   unidentifiable and pending navigations, drags, reorders, separate
//!   processes and closes, and checks that each window's captured Locations
//!   (and those of every Session_End) are its tabs' most recent identified
//!   Locations. Tab order is not compared: an unidentified tab's UIA name
//!   matches nothing, so its position after a reorder is not exact (esr-sim
//!   `explorer.rs`), which is Property 1's concern, not this one.
//!
//! Restriction: in the simulated sessions a dragged tab never shows an
//! unidentifiable Location when its new key registers: a tab that currently
//! shows one is never dragged, and a dragged tab is not navigated to one
//! before its new key registers. Explorer recreates a dragged tab's HWND, and
//! the only signal linking the new tab to the old one is the Location its new
//! browser reports. For an unidentifiable Location there is none: the new key
//! reports no Location_Identifier, its UIA name matches no recorded Location,
//! and its first scan arrives about 300 ms after the old key's revocation
//! (registration ~200 ms plus the scan debounce), when the tombstone has
//! expired. So the kept identifier cannot follow the tab. This is the known
//! gap recorded in `tracker/session.rs`; without the restriction this test
//! fails on it.

use std::collections::BTreeSet;

use esr_core::engine::{
    Command, Event, Millis, ObservedLocation, TabKey, TabObservation, TabStripState, TimerId, WinKey,
    WindowObservation,
};
use esr_core::logfmt::LogEntry;
use esr_core::model::{DisplayRef, LocationId, Placement, Rect, ShowState, Snapshot, WideString};
use esr_core::tracker::{LiveSessionTracker, SessionEnd, TOMBSTONE_MS};
use esr_sim::explorer::{
    default_locations, ops_strategy, NavOutcome, Op, OpsConfig, Pick, Shown, SimConfig, SimExplorer, SimTab, TimedEvent,
};
use esr_sim::reference::canonical;
use proptest::collection::vec;
use proptest::prelude::*;

/// Per window (in Activation_Order), the captured tabs' Locations as
/// `(parsing name, display name)`.
fn located(snapshot: &Snapshot) -> Vec<Vec<(String, String)>> {
    canonical(snapshot)
        .windows
        .iter()
        .map(|w| {
            w.tabs
                .iter()
                .map(|t| (t.location.parsing_name.to_string_lossy(), t.location.display_name.to_string_lossy()))
                .collect()
        })
        .collect()
}

/// [`located`] with each window's tabs sorted (order-insensitive).
fn located_sorted(snapshot: &Snapshot) -> Vec<Vec<(String, String)>> {
    let mut windows = located(snapshot);
    for w in &mut windows {
        w.sort();
    }
    windows
}

fn placement() -> Placement {
    let r = Rect { left: 100, top: 100, right: 900, bottom: 700 };
    Placement { state: ShowState::Normal, restored: r, display: DisplayRef { device: r"\\.\DISPLAY1".into(), monitor: r } }
}

// ---------------------------------------------------------------------------
// Event-level histories
// ---------------------------------------------------------------------------

const WINDOWS: usize = 3;

/// How a navigation completes.
#[derive(Clone, Copy, Debug)]
enum Nav {
    /// With a Location_Identifier (index into the location pool).
    Known(usize),
    /// Without one.
    Unidentified,
}

#[derive(Clone, Copy, Debug)]
enum Step {
    /// A new tab in window `win % WINDOWS` (opening the window if needed).
    /// `nav: None` leaves it unnavigated. With `nav_before_scan`, an identified
    /// first navigation is reported before the tab's first scan.
    Open { win: usize, nav: Option<Nav>, nav_before_scan: bool },
    /// Navigation of the tab `tab % tab_count` (any window).
    Navigate { tab: usize, nav: Nav },
    /// Closes the tab `tab % n` among tabs whose window keeps another tab.
    Close { tab: usize },
}

fn nav() -> impl Strategy<Value = Nav> {
    prop_oneof![
        3 => (0usize..64).prop_map(Nav::Known),
        2 => Just(Nav::Unidentified),
    ]
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        2 => (0..WINDOWS, prop::option::weighted(0.8, nav()), any::<bool>())
            .prop_map(|(win, nav, nav_before_scan)| Step::Open { win, nav, nav_before_scan }),
        5 => (any::<usize>(), nav()).prop_map(|(tab, nav)| Step::Navigate { tab, nav }),
        1 => any::<usize>().prop_map(|tab| Step::Close { tab }),
    ]
}

fn history() -> impl Strategy<Value = Vec<Step>> {
    (0..WINDOWS, nav(), vec(step(), 1..40)).prop_map(|(win, first, mut rest)| {
        rest.insert(0, Step::Open { win, nav: Some(first), nav_before_scan: false });
        rest
    })
}

/// What a model tab displays.
#[derive(Clone, Debug)]
enum ModelShown {
    NotNavigated,
    Known(LocationId),
    /// The value makes the UIA name unique.
    Unidentified(u32),
}

#[derive(Clone, Debug)]
struct ModelTab {
    key: TabKey,
    shown: ModelShown,
    /// Most recent identified Location.
    last_known: Option<LocationId>,
}

#[derive(Clone, Debug)]
struct ModelWindow {
    key: WinKey,
    /// Left to right; new tabs are appended.
    tabs: Vec<ModelTab>,
}

/// The tracker under test, the expected state, and the expected log entries.
struct Model {
    tracker: LiveSessionTracker,
    pool: Vec<LocationId>,
    /// Indexed by `Step::Open::win`; `None` until opened.
    slots: [Option<usize>; WINDOWS],
    /// In order of first scan (the Activation_Order with no activations).
    windows: Vec<ModelWindow>,
    timers: BTreeSet<(Millis, TimerId)>,
    now: Millis,
    next_key: u64,
    next_unidentified: u32,
    /// `kept_previous` of every `UNIDENTIFIABLE-LOCATION` entry, in order.
    expected_log: Vec<bool>,
    actual_log: Vec<bool>,
}

impl Model {
    fn new() -> Self {
        Model {
            tracker: LiveSessionTracker::new(),
            pool: default_locations(),
            slots: [None; WINDOWS],
            windows: Vec::new(),
            timers: BTreeSet::new(),
            now: Millis(0),
            next_key: 1,
            next_unidentified: 0,
            expected_log: Vec::new(),
            actual_log: Vec::new(),
        }
    }

    fn deliver(&mut self, event: &Event) {
        let out = self.tracker.handle(self.now, event);
        assert!(out.session_end.is_none(), "no window closes here: {event:?}");
        for c in out.commands {
            match c {
                Command::SetTimer { id, at } => {
                    self.timers.insert((at, id));
                }
                Command::AppendLog(LogEntry::UnidentifiableLocation { kept_previous }) => {
                    self.actual_log.push(kept_previous);
                }
                c => panic!("unexpected command {c:?}"),
            }
        }
    }

    /// The rescan of window `wi` the scanner would post, with `selected` as
    /// the UIA selection.
    fn scan(&mut self, wi: usize, selected: usize) {
        let w = &self.windows[wi];
        let tabs = w
            .tabs
            .iter()
            .map(|t| TabObservation {
                tab: t.key,
                location: match &t.shown {
                    ModelShown::NotNavigated => ObservedLocation::NotNavigated,
                    ModelShown::Known(l) => ObservedLocation::Known(l.clone()),
                    ModelShown::Unidentified(_) => ObservedLocation::Unidentifiable,
                },
            })
            .collect();
        let names = w
            .tabs
            .iter()
            .map(|t| match &t.shown {
                ModelShown::NotNavigated => WideString::from("New tab"),
                ModelShown::Known(l) => l.display_name.clone(),
                ModelShown::Unidentified(n) => format!("Unidentified {n}").as_str().into(),
            })
            .collect();
        let selected = (!w.tabs.is_empty()).then(|| selected.min(w.tabs.len() - 1));
        let obs = WindowObservation {
            win: w.key,
            pid: 10,
            tabs,
            active: None,
            strip: Some(TabStripState { names, selected }),
            placement: placement(),
        };
        self.deliver(&Event::WindowScanned(obs));
    }

    /// Completes a navigation of tab `ti` in window `wi` and reports it as
    /// the adapters do. `registered`: the tab has been scanned before.
    fn navigate(&mut self, wi: usize, ti: usize, nav: Nav, registered: bool) {
        let t = &mut self.windows[wi].tabs[ti];
        match nav {
            Nav::Known(i) => {
                let l = self.pool[i % self.pool.len()].clone();
                t.shown = ModelShown::Known(l.clone());
                t.last_known = Some(l.clone());
                let key = t.key;
                if registered {
                    self.deliver(&Event::NavigationCompleted { tab: key, location: l });
                }
            }
            Nav::Unidentified => {
                if !matches!(t.shown, ModelShown::Unidentified(_)) {
                    self.expected_log.push(t.last_known.is_some());
                }
                self.next_unidentified += 1;
                t.shown = ModelShown::Unidentified(self.next_unidentified);
            }
        }
    }

    fn apply(&mut self, step: Step) {
        match step {
            Step::Open { win, nav, nav_before_scan } => {
                let wi = match self.slots[win] {
                    Some(wi) => wi,
                    None => {
                        let key = WinKey { hwnd: 0x100 + win as u64, seq: win as u64 + 1 };
                        self.windows.push(ModelWindow { key, tabs: Vec::new() });
                        self.slots[win] = Some(self.windows.len() - 1);
                        self.windows.len() - 1
                    }
                };
                let key = TabKey { hwnd: 0x1000 + self.next_key, seq: self.next_key };
                self.next_key += 1;
                let w = &mut self.windows[wi];
                w.tabs.push(ModelTab { key, shown: ModelShown::NotNavigated, last_known: None });
                let ti = w.tabs.len() - 1;
                if let Some(nav) = nav {
                    // `NavigationCompleted` may arrive before the first scan
                    // lists the tab; otherwise the scan reports the Location.
                    self.navigate(wi, ti, nav, nav_before_scan);
                }
                self.scan(wi, ti);
            }
            Step::Navigate { tab, nav } => {
                let all: Vec<(usize, usize)> = self
                    .windows
                    .iter()
                    .enumerate()
                    .flat_map(|(wi, w)| (0..w.tabs.len()).map(move |ti| (wi, ti)))
                    .collect();
                if all.is_empty() {
                    return;
                }
                let (wi, ti) = all[tab % all.len()];
                self.navigate(wi, ti, nav, true);
                self.scan(wi, ti);
            }
            Step::Close { tab } => {
                let closable: Vec<(usize, usize)> = self
                    .windows
                    .iter()
                    .enumerate()
                    .filter(|(_, w)| w.tabs.len() >= 2)
                    .flat_map(|(wi, w)| (0..w.tabs.len()).map(move |ti| (wi, ti)))
                    .collect();
                if closable.is_empty() {
                    return;
                }
                let (wi, ti) = closable[tab % closable.len()];
                let removed = self.windows[wi].tabs.remove(ti);
                let win = self.windows[wi].key;
                self.deliver(&Event::TabRevoked { win, tab: removed.key });
                self.scan(wi, ti);
            }
        }
        // Let the tombstones expire, then move on.
        let next = self.now + TOMBSTONE_MS * 3;
        while let Some(&(at, id)) = self.timers.first().filter(|(at, _)| *at < next) {
            self.timers.remove(&(at, id));
            self.now = self.now.max(at);
            self.deliver(&Event::Timer(id));
        }
        self.now = next;
    }

    /// Per window, the most recent identified Location of each tab that has
    /// one, left to right; windows with none are absent.
    fn expected(&self) -> Vec<Vec<(String, String)>> {
        self.windows
            .iter()
            .map(|w| {
                w.tabs
                    .iter()
                    .filter_map(|t| t.last_known.as_ref())
                    .map(|l| (l.parsing_name.to_string_lossy(), l.display_name.to_string_lossy()))
                    .collect::<Vec<_>>()
            })
            .filter(|w| !w.is_empty())
            .collect()
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// **Validates: Requirements 8.8**
    #[test]
    fn event_histories_keep_the_last_identifier_and_log_each_transition(steps in history()) {
        let mut m = Model::new();
        for (n, &step) in steps.iter().enumerate() {
            m.apply(step);
            let (snapshot, excluded) = m.tracker.capture();
            prop_assert!(excluded.is_none());
            prop_assert_eq!(located(&snapshot), m.expected(), "after step {}: {:?}", n, step);
        }
        prop_assert_eq!(&m.actual_log, &m.expected_log);
    }
}

#[test]
fn unidentified_navigations_keep_or_exclude_and_log() {
    // Tab 1 is identified then unidentified twice in a row, then identified and
    // unidentified again; tab 2 is unidentified from the start.
    let steps = [
        Step::Open { win: 0, nav: Some(Nav::Known(0)), nav_before_scan: false },
        Step::Navigate { tab: 0, nav: Nav::Unidentified },
        Step::Navigate { tab: 0, nav: Nav::Unidentified },
        Step::Open { win: 0, nav: Some(Nav::Unidentified), nav_before_scan: false },
        Step::Navigate { tab: 0, nav: Nav::Known(2) },
        Step::Navigate { tab: 0, nav: Nav::Unidentified },
    ];
    let mut m = Model::new();
    for step in steps {
        m.apply(step);
    }
    let (snapshot, _) = m.tracker.capture();
    // Tab 1 keeps Documents (index 2); tab 2 is excluded.
    assert_eq!(located(&snapshot), vec![vec![(r"C:\Users\User\Documents".to_string(), "Documents".to_string())]]);
    assert_eq!(m.actual_log, vec![true, false, true]);
    assert_eq!(m.actual_log, m.expected_log);
}

// ---------------------------------------------------------------------------
// Simulated sessions
// ---------------------------------------------------------------------------

/// The tracker plus the tracker timers it set, delivered at their time
/// (as in `tracker_smoke.rs`).
#[derive(Default)]
struct Harness {
    tracker: LiveSessionTracker,
    timers: BTreeSet<(Millis, TimerId)>,
    ends: Vec<SessionEnd>,
    now: Millis,
}

impl Harness {
    fn deliver(&mut self, sim: &mut SimExplorer, at: Millis, event: &Event) {
        self.now = self.now.max(at);
        let out = self.tracker.handle(self.now, event);
        self.ends.extend(out.session_end);
        for c in out.commands {
            match c {
                Command::SetTimer { id, at } => {
                    self.timers.insert((at, id));
                }
                Command::AppendLog(_) => {}
                c => {
                    sim.handle_command(&c);
                }
            }
        }
    }

    fn fire_until(&mut self, sim: &mut SimExplorer, until: Millis) {
        while let Some(&(at, id)) = self.timers.first().filter(|(at, _)| *at < until) {
            self.timers.remove(&(at, id));
            self.deliver(sim, at, &Event::Timer(id));
        }
    }

    fn feed(&mut self, sim: &mut SimExplorer, events: Vec<TimedEvent>) {
        for e in events {
            self.fire_until(sim, e.at);
            self.deliver(sim, e.at, &e.event);
        }
    }
}

/// `op` would leave a dragged tab showing an unidentifiable Location when its
/// new key registers (see the restriction in the module docs): it drags a
/// tab that currently shows one, or makes a dragged tab whose new key has not
/// registered yet unidentified. Mirrors the picks of `SimExplorer`. (Tabs the
/// user opens register at once, so only dragged tabs are unregistered.)
fn hides_dragged_identifier(sim: &SimExplorer, op: &Op) -> bool {
    let windows = sim.windows();
    let tab_at = |win: Pick, tab: Option<Pick>| {
        let w = &windows[win.of(windows.len())?];
        let ti = match tab {
            Some(tab) => tab.of(w.tabs.len()).unwrap_or(0),
            None => w.active,
        };
        w.tabs.get(ti)
    };
    let unregistered = |t: &SimTab| t.registration.is_none() && t.last_known.is_some();
    match *op {
        Op::DragOut { win, tab, .. } | Op::DragIn { from: win, tab, .. } => {
            tab_at(win, Some(tab)).is_some_and(|t| matches!(t.shown, Shown::Unidentifiable(_)))
        }
        Op::Navigate { win, nav: NavOutcome::Unidentifiable, .. } => tab_at(win, None).is_some_and(unregistered),
        Op::CompleteNavigation { pick, identified: false } => {
            let pending: Vec<&SimTab> =
                windows.iter().flat_map(|w| w.tabs.iter().filter(|t| t.pending.is_some())).collect();
            pick.of(pending.len()).is_some_and(|i| unregistered(pending[i]))
        }
        _ => false,
    }
}

fn ops_config() -> OpsConfig {
    // Process kills and shell loss are other properties' concern (3.3, 1.10).
    OpsConfig { unidentifiable: true, pending_navigation: true, process_kill: false, shell_loss: false, ..OpsConfig::all() }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// **Validates: Requirements 8.8**
    #[test]
    fn simulated_sessions_capture_the_most_recent_identified_locations(
        ops in ops_strategy(&ops_config(), 1..60)
    ) {
        let mut sim = SimExplorer::new(SimConfig::default());
        let mut h = Harness::default();
        for op in &ops {
            if hides_dragged_identifier(&sim, op) {
                continue;
            }
            let events = sim.apply(op);
            h.feed(&mut sim, events);
        }
        let events = sim.finish();
        h.feed(&mut sim, events);
        h.fire_until(&mut sim, Millis(u64::MAX));

        let (actual, _) = h.tracker.capture();
        prop_assert_eq!(located_sorted(&actual), located_sorted(&sim.reference().snapshot()));

        let actual_ends: Vec<_> = h.ends.iter().map(|e| located_sorted(&e.snapshot)).collect();
        let expected_ends: Vec<_> =
            sim.reference().session_ends().iter().map(|e| located_sorted(&e.snapshot)).collect();
        prop_assert_eq!(actual_ends, expected_ends);
    }
}
