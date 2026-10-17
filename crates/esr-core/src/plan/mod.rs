//! Restore planning: [`plan_restore`], [`focus_target`], and [`z_order`]
//! (Requirements 5.1–5.3, 5.7, 5.10, 5.11, 6.1–6.3, 9.5, 9.6; design.md,
//! "Restore pipeline" → "Planning").
//!
//! `plan_restore` is pure and is called again every time a probe result
//! arrives. The `RestoreSession` state machine compares successive plans and
//! applies the difference, so every plan type is `Clone + Eq`.
//!
//! # Windows and trigger mapping
//!
//! Each saved tab has a [`TabResolution`]: `Pending` while its probe is
//! outstanding, or `Resolved(Fallback)`. A tab is *kept* when it resolved to
//! `Keep` or `Redirected`, *omitted* when it resolved to `Omitted`, and
//! *pending* otherwise. Saved windows are visited in saved Activation_Order:
//!
//! - A window whose tabs are all omitted is dropped (Req 9.5).
//! - Every other window is planned: it has at least one kept tab
//!   (*known restorable*, see [`PlannedWindow::known_restorable`]) or at least
//!   one pending tab. With every probe resolved, the planned windows are
//!   exactly the windows that keep at least one tab (Req 5.1).
//! - The Trigger_Window hosts the first planned window (`windows[0]`,
//!   `is_trigger`) (Req 5.2, 6.1).
//! - **Trigger delay.** When windows with only pending tabs come before the
//!   first known-restorable window, the mapping is not decided: each of them
//!   could still be omitted. Those windows and the first known-restorable
//!   window are the *trigger candidates* and are planned with
//!   `decided: false`. Windows after the first known-restorable window can
//!   never become the trigger and are `decided: true`, so they go ahead at
//!   once. With no known-restorable window at all, every planned window is a
//!   candidate.
//! - If every window is omitted, or the launch is
//!   [`LaunchKind::BuiltinRestore`] (Req 10.4), the plan is empty and the
//!   trigger stays as it opened (Req 9.6).
//!
//! # Slots
//!
//! A window's slots are its non-omitted saved tabs in saved order: a
//! [`Slot::Restore`] for a kept tab (showing its saved Location or the chosen
//! ancestor; redirected duplicates are kept, Req 9.1), a
//! [`Slot::Placeholder`] for a pending tab, so a late result stays in its
//! saved position (Req 7.9). For a Targeted_Launch the trigger gets one more
//! slot, [`Slot::Requested`], last (Req 6.2, 6.5).
//!
//! # Active slot
//!
//! [`active_after_omission`] over the mask "not omitted": **a placeholder
//! counts as kept** for position purposes, so the planned active slot can be a
//! placeholder. If that probe later fails, the next plan moves the active slot
//! by the same rule. For a Targeted trigger the requested slot is active
//! (Req 6.3).
//!
//! # Placement, z-order and focus
//!
//! - Placement is [`resolve_placement`] with `targeted_trigger` set for the
//!   trigger of a Targeted_Launch (never minimized, Req 6.6).
//! - Z-order ([`z_order`]): planned windows in saved Activation_Order, first
//!   topmost; the trigger first (Req 5.7, 6.3). The trigger is `windows[0]`, so
//!   both rules give `0, 1, …, n−1`.
//! - Focus ([`focus_target`]): the trigger for a Targeted_Launch (Req 6.3);
//!   otherwise the first window in z-order that is not minimized (Req 5.10).
//! - **All minimized (Req 5.11).** For a Default_Launch where every planned
//!   window's final state is minimized, the first window is planned in its
//!   pre-minimize state (`Maximized` if `restore_maximized`, else `Normal`);
//!   all other windows stay minimized. Focus then goes to it.
//!
//! # `decided`
//!
//! `decided` means the window's role (trigger or not) and placement can no
//! longer change with later probe results; its slots still can, as
//! placeholders resolve. Besides trigger candidates, the first window is also
//! undecided when its saved state is minimized, some planned window is not
//! minimized, and every such window has only pending tabs: if those are all
//! omitted, Req 5.11 un-minimizes the first window.

pub mod active;
pub mod defer;
pub mod fallback;
pub mod placement;

use std::collections::BTreeMap;

use crate::launch::LaunchKind;
use crate::model::{Display, LocationId, ShowState, Snapshot, WindowId};
use active::active_after_omission;
use fallback::Fallback;
use placement::{resolve_placement, FinalPlacement};

/// One saved tab: window id and index into its saved tabs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TabRef {
    pub window: WindowId,
    pub tab: usize,
}

/// What is known about one saved tab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TabResolution {
    /// The probe has not returned yet.
    Pending,
    /// The probe returned and [`fallback::choose_fallback`] decided.
    Resolved(Fallback),
}

/// Resolution of each saved tab. Tabs without an entry are pending.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Availability {
    resolved: BTreeMap<TabRef, Fallback>,
}

impl Availability {
    /// Every tab pending.
    pub fn new() -> Self {
        Self::default()
    }

    /// Every tab of `snapshot` resolved as `Keep`.
    pub fn all_kept(snapshot: &Snapshot) -> Self {
        Self::from_fn(snapshot, |_, _| TabResolution::Resolved(Fallback::Keep))
    }

    /// Builds the map by asking `f` about every tab of `snapshot`.
    pub fn from_fn(
        snapshot: &Snapshot,
        mut f: impl FnMut(TabRef, &LocationId) -> TabResolution,
    ) -> Self {
        let mut out = Self::new();
        for w in &snapshot.windows {
            for (i, t) in w.tabs.iter().enumerate() {
                let tab = TabRef { window: w.id, tab: i };
                if let TabResolution::Resolved(fb) = f(tab, &t.location) {
                    out.resolved.insert(tab, fb);
                }
            }
        }
        out
    }

    /// Records a probe result; returns the previous one, if any.
    pub fn resolve(&mut self, tab: TabRef, fallback: Fallback) -> Option<Fallback> {
        self.resolved.insert(tab, fallback)
    }

    pub fn get(&self, tab: TabRef) -> TabResolution {
        match self.resolved.get(&tab) {
            Some(fb) => TabResolution::Resolved(fb.clone()),
            None => TabResolution::Pending,
        }
    }
}

/// How the trigger was opened, as needed for planning.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaunchInfo {
    Default,
    /// `requested` is the Location the Trigger_Window opened with.
    Targeted { requested: LocationId },
    /// Explorer's own logon restore: nothing is planned (Req 10.4).
    BuiltinRestore,
}

impl LaunchInfo {
    pub fn kind(&self) -> LaunchKind {
        match self {
            LaunchInfo::Default => LaunchKind::Default,
            LaunchInfo::Targeted { .. } => LaunchKind::Targeted,
            LaunchInfo::BuiltinRestore => LaunchKind::BuiltinRestore,
        }
    }
}

/// One tab position of a planned window, left to right.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Slot {
    /// Saved tab `tab_index` showing `location`: the saved Location, or the
    /// ancestor chosen by the fallback.
    Restore { tab_index: usize, location: LocationId },
    /// Saved tab `tab_index` whose probe is still pending.
    Placeholder { tab_index: usize },
    /// The requested tab of a Targeted_Launch (last slot of the trigger).
    Requested(LocationId),
}

/// One window to restore.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedWindow {
    /// The saved window this restores.
    pub source: WindowId,
    /// Hosted by the Trigger_Window. Only `windows[0]` is the trigger.
    pub is_trigger: bool,
    /// Never empty.
    pub slots: Vec<Slot>,
    /// Index into `slots`.
    pub active: usize,
    /// Final placement, including the Req 5.11 un-minimize adjustment.
    pub placement: FinalPlacement,
    /// Role and placement are final (see the module docs).
    pub decided: bool,
}

impl PlannedWindow {
    /// At least one saved tab is known to be restorable.
    pub fn known_restorable(&self) -> bool {
        self.slots.iter().any(|s| matches!(s, Slot::Restore { .. }))
    }
}

/// Result of [`plan_restore`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RestorePlan {
    /// In saved Activation_Order; `windows[0]` is the trigger.
    pub windows: Vec<PlannedWindow>,
    /// Indices into `windows`, topmost first ([`z_order`]).
    pub z_order: Vec<usize>,
    /// Index into `windows` ([`focus_target`]); `None` for an empty plan.
    pub focus: Option<usize>,
    /// The trigger mapping can no longer change.
    pub trigger_decided: bool,
    /// Planned for a Targeted_Launch.
    pub targeted: bool,
}

impl RestorePlan {
    /// Nothing to restore; the trigger stays as it opened (Req 9.6).
    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }

    /// Index of the trigger window, if any.
    pub fn trigger(&self) -> Option<usize> {
        self.windows.iter().position(|w| w.is_trigger)
    }
}

/// Plans the restoration of `snapshot` (see the module docs).
///
/// Window ids in `snapshot.activation` that do not name a window are skipped,
/// and windows not listed there are not planned; valid snapshots have neither.
pub fn plan_restore(
    snapshot: &Snapshot,
    launch: &LaunchInfo,
    availability: &Availability,
    displays: &[Display],
) -> RestorePlan {
    let targeted = matches!(launch, LaunchInfo::Targeted { .. });
    if matches!(launch, LaunchInfo::BuiltinRestore) {
        return RestorePlan { targeted, ..RestorePlan::default() };
    }

    let mut windows: Vec<PlannedWindow> = Vec::new();
    for w in snapshot.activation.iter().filter_map(|&id| snapshot.window(id)) {
        let mut slots = Vec::with_capacity(w.tabs.len() + 1);
        let mut not_omitted = Vec::with_capacity(w.tabs.len());
        for (i, t) in w.tabs.iter().enumerate() {
            let slot = match availability.get(TabRef { window: w.id, tab: i }) {
                TabResolution::Pending => Some(Slot::Placeholder { tab_index: i }),
                TabResolution::Resolved(Fallback::Keep) => {
                    Some(Slot::Restore { tab_index: i, location: t.location.clone() })
                }
                TabResolution::Resolved(Fallback::Redirected(a)) => {
                    Some(Slot::Restore { tab_index: i, location: a })
                }
                TabResolution::Resolved(Fallback::Omitted(_)) => None,
            };
            not_omitted.push(slot.is_some());
            slots.extend(slot);
        }
        if slots.is_empty() {
            continue; // every tab omitted (Req 9.5)
        }
        let is_trigger = windows.is_empty();
        let mut active = active_after_omission(&not_omitted, w.active).unwrap_or(0);
        if let (true, LaunchInfo::Targeted { requested }) = (is_trigger, launch) {
            slots.push(Slot::Requested(requested.clone()));
            active = slots.len() - 1;
        }
        let placement = resolve_placement(&w.placement, displays, targeted && is_trigger);
        windows.push(PlannedWindow { source: w.id, is_trigger, slots, active, placement, decided: true });
    }
    if windows.is_empty() {
        return RestorePlan { targeted, ..RestorePlan::default() };
    }

    // Trigger mapping: candidates are the windows up to and including the
    // first known-restorable one (all of them if there is none).
    let first_known = windows.iter().position(PlannedWindow::known_restorable);
    let trigger_decided = first_known == Some(0);
    if !trigger_decided {
        let last_candidate = first_known.unwrap_or(windows.len() - 1);
        for w in &mut windows[..=last_candidate] {
            w.decided = false;
        }
    }

    // Req 5.11: all minimized → the first window shows its pre-minimize state.
    // (A Targeted trigger is never minimized, so this is Default only.)
    let minimized = |w: &PlannedWindow| matches!(w.placement.state, ShowState::Minimized { .. });
    if let ShowState::Minimized { restore_maximized } = windows[0].placement.state {
        if windows.iter().all(minimized) {
            windows[0].placement.state =
                if restore_maximized { ShowState::Maximized } else { ShowState::Normal };
        } else if windows.iter().filter(|w| !minimized(w)).all(|w| !w.known_restorable()) {
            // Every non-minimized window could still be omitted.
            windows[0].decided = false;
        }
    }

    let mut plan = RestorePlan { z_order: z_order(&windows), windows, focus: None, trigger_decided, targeted };
    plan.focus = focus_target(&plan);
    plan
}

/// Z-order, topmost first, as indices into `windows`: saved Activation_Order
/// (the order of `windows`) with the trigger moved first (Req 5.7, 6.3).
pub fn z_order(windows: &[PlannedWindow]) -> Vec<usize> {
    let trigger = windows.iter().position(|w| w.is_trigger);
    trigger.into_iter().chain((0..windows.len()).filter(|&i| Some(i) != trigger)).collect()
}

/// The window that receives keyboard focus, as an index into `plan.windows`:
/// the trigger for a Targeted_Launch (Req 6.3); otherwise the first window in
/// `plan.z_order` whose final state is not minimized (Req 5.10). After the
/// Req 5.11 adjustment of [`plan_restore`] such a window always exists; if
/// not (a hand-built plan), the first in z-order. `None` for an empty plan.
pub fn focus_target(plan: &RestorePlan) -> Option<usize> {
    if plan.targeted
        && let Some(t) = plan.trigger()
    {
        return Some(t);
    }
    let shown = |&&i: &&usize| {
        plan.windows
            .get(i)
            .is_some_and(|w| !matches!(w.placement.state, ShowState::Minimized { .. }))
    };
    plan.z_order.iter().find(shown).or(plan.z_order.first()).copied()
}

#[cfg(test)]
mod tests {
    use super::fallback::OmitReason;
    use super::*;
    use crate::model::{DisplayRef, LocationKind, Placement, Rect, TabRec, Volume, WindowRec};

    const MIN_RM: ShowState = ShowState::Minimized { restore_maximized: true };
    const MIN_N: ShowState = ShowState::Minimized { restore_maximized: false };
    const OMIT: Fallback = Fallback::Omitted(OmitReason::NotExist);

    fn fs(path: &str) -> LocationId {
        LocationId {
            case_sensitive: false,
            kind: LocationKind::FileSystem(Volume::Fixed),
            parsing_name: path.into(),
            display_name: path.into(),
            pidl: vec![],
            search: None,
        }
    }

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> Rect {
        Rect { left, top, right, bottom }
    }

    fn displays() -> Vec<Display> {
        vec![Display {
            device: "P".into(),
            monitor: rect(0, 0, 1920, 1080),
            work: rect(0, 0, 1920, 1040),
            primary: true,
        }]
    }

    /// Window `id` with tabs `C:\w{id}\t{i}` and `state`.
    fn window(id: u32, tabs: usize, active: usize, state: ShowState) -> WindowRec {
        WindowRec {
            id: WindowId(id),
            tabs: (0..tabs).map(|i| TabRec { location: fs(&format!(r"C:\w{id}\t{i}")) }).collect(),
            active,
            placement: Placement {
                state,
                restored: rect(100, 100, 900, 700),
                display: DisplayRef { device: "P".into(), monitor: rect(0, 0, 1920, 1080) },
            },
        }
    }

    fn snap(windows: Vec<WindowRec>, activation: &[u32]) -> Snapshot {
        Snapshot { windows, activation: activation.iter().map(|&i| WindowId(i)).collect() }
    }

    fn tab(window: u32, tab: usize) -> TabRef {
        TabRef { window: WindowId(window), tab }
    }

    /// Everything kept except the listed overrides.
    fn avail(s: &Snapshot, overrides: &[(u32, usize, Option<Fallback>)]) -> Availability {
        Availability::from_fn(s, |t, _| {
            match overrides.iter().find(|(w, i, _)| tab(*w, *i) == t) {
                Some((_, _, None)) => TabResolution::Pending,
                Some((_, _, Some(fb))) => TabResolution::Resolved(fb.clone()),
                None => TabResolution::Resolved(Fallback::Keep),
            }
        })
    }

    fn plan(s: &Snapshot, launch: &LaunchInfo, a: &Availability) -> RestorePlan {
        plan_restore(s, launch, a, &displays())
    }

    fn sources(p: &RestorePlan) -> Vec<u32> {
        p.windows.iter().map(|w| w.source.0).collect()
    }

    fn decided(p: &RestorePlan) -> Vec<bool> {
        p.windows.iter().map(|w| w.decided).collect()
    }

    /// `R{tab}` for restore, `P{tab}` for placeholder, `Q` for requested.
    fn shape(w: &PlannedWindow) -> Vec<String> {
        w.slots
            .iter()
            .map(|s| match s {
                Slot::Restore { tab_index, .. } => format!("R{tab_index}"),
                Slot::Placeholder { tab_index } => format!("P{tab_index}"),
                Slot::Requested(_) => "Q".into(),
            })
            .collect()
    }

    fn targeted(path: &str) -> LaunchInfo {
        LaunchInfo::Targeted { requested: fs(path) }
    }

    #[test]
    fn all_available_default_launch() {
        let s = snap(
            vec![window(1, 2, 1, ShowState::Normal), window(2, 3, 2, ShowState::Maximized), window(3, 1, 0, MIN_N)],
            &[2, 3, 1],
        );
        let p = plan(&s, &LaunchInfo::Default, &Availability::all_kept(&s));
        assert_eq!(sources(&p), [2, 3, 1]);
        assert_eq!(p.windows.iter().map(|w| w.is_trigger).collect::<Vec<_>>(), [true, false, false]);
        assert!(p.trigger_decided && p.windows.iter().all(|w| w.decided && w.known_restorable()));
        let w2 = &p.windows[0];
        assert_eq!(shape(w2), ["R0", "R1", "R2"]);
        assert_eq!(w2.active, 2);
        for (i, slot) in w2.slots.iter().enumerate() {
            assert_eq!(slot, &Slot::Restore { tab_index: i, location: s.windows[1].tabs[i].location.clone() });
        }
        assert_eq!(p.windows[1].placement.state, MIN_N);
        assert_eq!(p.z_order, [0, 1, 2]);
        assert_eq!(p.focus, Some(0));
        assert_eq!(p.trigger(), Some(0));
    }

    #[test]
    fn placement_comes_from_resolve_placement() {
        let mut w = window(1, 1, 0, ShowState::Normal);
        w.placement.restored = rect(9000, 9000, 9800, 9600);
        let s = snap(vec![w.clone()], &[1]);
        let p = plan(&s, &LaunchInfo::Default, &Availability::all_kept(&s));
        assert_eq!(p.windows[0].placement, resolve_placement(&w.placement, &displays(), false));
        assert_eq!(p.windows[0].placement.restored, rect(560, 220, 1360, 820));
    }

    #[test]
    fn omitted_tabs_leave_an_order_preserving_subsequence() {
        // Tabs 0..5, active 2 omitted → nearest kept to the left (tab 1).
        let s = snap(vec![window(1, 5, 2, ShowState::Normal)], &[1]);
        let a = avail(&s, &[(1, 0, Some(OMIT)), (1, 2, Some(OMIT)), (1, 4, Some(OMIT))]);
        let p = plan(&s, &LaunchInfo::Default, &a);
        assert_eq!(shape(&p.windows[0]), ["R1", "R3"]);
        assert_eq!(p.windows[0].active, 0);

        // Nothing kept left of the active tab → nearest to the right.
        let a = avail(&s, &[(1, 0, Some(OMIT)), (1, 1, Some(OMIT)), (1, 2, Some(OMIT))]);
        let p = plan(&s, &LaunchInfo::Default, &a);
        assert_eq!(shape(&p.windows[0]), ["R3", "R4"]);
        assert_eq!(p.windows[0].active, 0);
    }

    #[test]
    fn redirected_duplicates_are_kept() {
        let mut w = window(1, 3, 0, ShowState::Normal);
        w.tabs[0].location = fs(r"C:\a");
        let s = snap(vec![w], &[1]);
        let a = avail(
            &s,
            &[(1, 1, Some(Fallback::Redirected(fs(r"C:\a")))), (1, 2, Some(Fallback::Redirected(fs(r"C:\a"))))],
        );
        let p = plan(&s, &LaunchInfo::Default, &a);
        let locs: Vec<_> = p.windows[0]
            .slots
            .iter()
            .map(|s| match s {
                Slot::Restore { location, .. } => location.parsing_name.to_string_lossy(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(locs, [r"C:\a", r"C:\a", r"C:\a"]);
    }

    #[test]
    fn fully_omitted_window_is_dropped_and_trigger_moves_on() {
        let s = snap(
            vec![window(1, 2, 0, ShowState::Normal), window(2, 1, 0, ShowState::Normal), window(3, 1, 0, ShowState::Normal)],
            &[1, 2, 3],
        );
        let a = avail(&s, &[(1, 0, Some(OMIT)), (1, 1, Some(Fallback::Omitted(OmitReason::Timeout)))]);
        let p = plan(&s, &LaunchInfo::Default, &a);
        assert_eq!(sources(&p), [2, 3]);
        assert!(p.windows[0].is_trigger && !p.windows[1].is_trigger);
        assert!(p.trigger_decided);
        assert_eq!(p.z_order, [0, 1]);
    }

    #[test]
    fn all_omitted_gives_empty_plan() {
        let s = snap(vec![window(1, 2, 0, ShowState::Normal), window(2, 1, 0, MIN_N)], &[2, 1]);
        let a = Availability::from_fn(&s, |_, _| TabResolution::Resolved(OMIT));
        for launch in [LaunchInfo::Default, targeted(r"D:\req")] {
            let p = plan(&s, &launch, &a);
            assert!(p.is_empty());
            assert!(p.z_order.is_empty());
            assert_eq!(p.focus, None);
            assert_eq!(p.trigger(), None);
        }
        let empty = Snapshot::default();
        assert!(plan(&empty, &LaunchInfo::Default, &Availability::new()).is_empty());
    }

    #[test]
    fn builtin_restore_plans_nothing() {
        let s = snap(vec![window(1, 1, 0, ShowState::Normal)], &[1]);
        assert!(plan(&s, &LaunchInfo::BuiltinRestore, &Availability::all_kept(&s)).is_empty());
    }

    #[test]
    fn targeted_trigger_gets_requested_slot_active() {
        let mut w1 = window(1, 2, 0, MIN_RM);
        w1.tabs[1].location = fs(r"D:\req");
        let s = snap(vec![w1, window(2, 3, 1, ShowState::Normal)], &[1, 2]);
        let p = plan(&s, &targeted(r"D:\req"), &Availability::all_kept(&s));
        assert!(p.targeted);
        let t = &p.windows[0];
        assert!(t.is_trigger);
        // The requested Location stays separate even though tab 1 shows it (Req 6.5).
        assert_eq!(shape(t), ["R0", "R1", "Q"]);
        assert_eq!(t.slots[2], Slot::Requested(fs(r"D:\req")));
        assert_eq!(t.active, 2);
        // A Targeted trigger is never minimized (Req 6.6).
        assert_eq!(t.placement.state, ShowState::Normal);
        // Other windows keep their saved active tab and get no requested slot.
        assert_eq!(shape(&p.windows[1]), ["R0", "R1", "R2"]);
        assert_eq!(p.windows[1].active, 1);
        assert_eq!(p.z_order, [0, 1]);
        assert_eq!(p.focus, Some(0));
    }

    #[test]
    fn targeted_focus_is_trigger_even_when_others_are_normal() {
        let s = snap(vec![window(1, 1, 0, ShowState::Normal), window(2, 1, 0, ShowState::Maximized)], &[1, 2]);
        let p = plan(&s, &targeted(r"D:\req"), &Availability::all_kept(&s));
        assert_eq!(focus_target(&p), Some(0));
        assert!(p.windows[0].is_trigger);
    }

    #[test]
    fn placeholders_hold_saved_positions_and_count_for_active() {
        let s = snap(vec![window(1, 4, 2, ShowState::Normal)], &[1]);
        let a = avail(&s, &[(1, 1, Some(OMIT)), (1, 2, None)]);
        let p = plan(&s, &LaunchInfo::Default, &a);
        assert_eq!(shape(&p.windows[0]), ["R0", "P2", "R3"]);
        assert_eq!(p.windows[0].active, 1);
        // The pending active tab is then omitted: nearest kept to the left.
        let a = avail(&s, &[(1, 1, Some(OMIT)), (1, 2, Some(OMIT))]);
        let p2 = plan(&s, &LaunchInfo::Default, &a);
        assert_eq!(shape(&p2.windows[0]), ["R0", "R3"]);
        assert_eq!(p2.windows[0].active, 0);
        // Only slots changed; the window was decided throughout.
        assert!(p.windows[0].decided && p2.windows[0].decided);
    }

    #[test]
    fn pending_window_ahead_delays_only_trigger_candidates() {
        // U (all pending), K (kept), L (kept), M (all pending).
        let s = snap(
            vec![
                window(10, 2, 0, ShowState::Normal),
                window(20, 1, 0, ShowState::Normal),
                window(30, 1, 0, ShowState::Normal),
                window(40, 1, 0, ShowState::Normal),
            ],
            &[10, 20, 30, 40],
        );
        let pending = [(10, 0, None), (10, 1, None), (40, 0, None)];
        let p = plan(&s, &LaunchInfo::Default, &avail(&s, &pending));
        assert_eq!(sources(&p), [10, 20, 30, 40]);
        assert!(!p.trigger_decided);
        assert!(p.windows[0].is_trigger && !p.windows[0].known_restorable());
        assert_eq!(shape(&p.windows[0]), ["P0", "P1"]);
        assert_eq!(decided(&p), [false, false, true, true]);
        assert!(!p.windows[3].known_restorable());

        // U turns out to be fully omitted: K hosts the trigger, now decided.
        let a = avail(&s, &[(10, 0, Some(OMIT)), (10, 1, Some(OMIT)), (40, 0, None)]);
        let p = plan(&s, &LaunchInfo::Default, &a);
        assert_eq!(sources(&p), [20, 30, 40]);
        assert!(p.trigger_decided && p.windows[0].is_trigger);
        assert_eq!(decided(&p), [true, true, true]);

        // One of U's tabs resolves kept: U hosts the trigger; K is decided.
        let a = avail(&s, &[(10, 1, None), (40, 0, None)]);
        let p = plan(&s, &LaunchInfo::Default, &a);
        assert_eq!(sources(&p), [10, 20, 30, 40]);
        assert!(p.trigger_decided && p.windows[0].is_trigger);
        assert_eq!(shape(&p.windows[0]), ["R0", "P1"]);
        assert_eq!(decided(&p), [true, true, true, true]);
    }

    #[test]
    fn targeted_trigger_delay_moves_requested_slot_with_mapping() {
        let s = snap(vec![window(1, 1, 0, ShowState::Normal), window(2, 1, 0, ShowState::Normal)], &[1, 2]);
        let p = plan(&s, &targeted(r"D:\req"), &avail(&s, &[(1, 0, None)]));
        assert_eq!(shape(&p.windows[0]), ["P0", "Q"]);
        assert_eq!(shape(&p.windows[1]), ["R0"]);
        assert_eq!(decided(&p), [false, false]);

        let p = plan(&s, &targeted(r"D:\req"), &avail(&s, &[(1, 0, Some(OMIT))]));
        assert_eq!(sources(&p), [2]);
        assert_eq!(shape(&p.windows[0]), ["R0", "Q"]);
        assert_eq!(p.windows[0].active, 1);
        assert_eq!(decided(&p), [true]);
    }

    #[test]
    fn everything_pending_makes_every_window_a_candidate() {
        let s = snap(vec![window(1, 1, 0, ShowState::Normal), window(2, 2, 1, ShowState::Normal)], &[2, 1]);
        let p = plan(&s, &LaunchInfo::Default, &Availability::new());
        assert_eq!(sources(&p), [2, 1]);
        assert!(!p.trigger_decided);
        assert_eq!(decided(&p), [false, false]);
        assert_eq!(shape(&p.windows[0]), ["P0", "P1"]);
        assert_eq!(p.windows[0].active, 1);
    }

    #[test]
    fn default_focus_is_first_non_minimized() {
        let s = snap(
            vec![window(1, 1, 0, MIN_RM), window(2, 1, 0, ShowState::Normal), window(3, 1, 0, ShowState::Maximized)],
            &[1, 2, 3],
        );
        let p = plan(&s, &LaunchInfo::Default, &Availability::all_kept(&s));
        assert_eq!(p.focus, Some(1));
        assert_eq!(focus_target(&p), Some(1));
        // Not every window is minimized, so the first stays minimized.
        assert_eq!(p.windows[0].placement.state, MIN_RM);
    }

    #[test]
    fn all_minimized_shows_first_in_pre_minimize_state() {
        for (first, shown) in [(MIN_RM, ShowState::Maximized), (MIN_N, ShowState::Normal)] {
            let s = snap(vec![window(1, 1, 0, first), window(2, 1, 0, MIN_RM), window(3, 1, 0, MIN_N)], &[1, 2, 3]);
            let p = plan(&s, &LaunchInfo::Default, &Availability::all_kept(&s));
            assert_eq!(p.windows[0].placement.state, shown);
            assert_eq!(p.windows[1].placement.state, MIN_RM);
            assert_eq!(p.windows[2].placement.state, MIN_N);
            assert_eq!(p.focus, Some(0));
            assert!(p.windows.iter().all(|w| w.decided));
        }
    }

    #[test]
    fn first_window_waits_when_only_pending_windows_are_not_minimized() {
        // A (kept, minimized), B (pending only, normal), C (kept, minimized).
        let s = snap(
            vec![window(1, 1, 0, MIN_N), window(2, 1, 0, ShowState::Normal), window(3, 1, 0, MIN_N)],
            &[1, 2, 3],
        );
        let p = plan(&s, &LaunchInfo::Default, &avail(&s, &[(2, 0, None)]));
        assert!(p.trigger_decided);
        assert_eq!(decided(&p), [false, true, true]);
        assert_eq!(p.windows[0].placement.state, MIN_N);
        assert_eq!(p.focus, Some(1));

        // B is omitted: every window is minimized, A is shown and focused.
        let p = plan(&s, &LaunchInfo::Default, &avail(&s, &[(2, 0, Some(OMIT))]));
        assert_eq!(sources(&p), [1, 3]);
        assert_eq!(decided(&p), [true, true]);
        assert_eq!(p.windows[0].placement.state, ShowState::Normal);
        assert_eq!(p.focus, Some(0));

        // B is kept: A stays minimized, B gets focus.
        let p = plan(&s, &LaunchInfo::Default, &Availability::all_kept(&s));
        assert_eq!(decided(&p), [true, true, true]);
        assert_eq!(p.windows[0].placement.state, MIN_N);
        assert_eq!(p.focus, Some(1));
    }

    #[test]
    fn z_order_puts_trigger_first() {
        let s = snap(vec![window(1, 1, 0, ShowState::Normal), window(2, 1, 0, ShowState::Normal), window(3, 1, 0, ShowState::Normal)], &[1, 2, 3]);
        let mut p = plan(&s, &LaunchInfo::Default, &Availability::all_kept(&s));
        assert_eq!(z_order(&p.windows), [0, 1, 2]);
        // A hand-built plan whose trigger is not first.
        p.windows[0].is_trigger = false;
        p.windows[2].is_trigger = true;
        assert_eq!(z_order(&p.windows), [2, 0, 1]);
        p.z_order = z_order(&p.windows);
        p.targeted = true;
        assert_eq!(focus_target(&p), Some(2));
        assert_eq!(z_order(&[]), Vec::<usize>::new());
    }

    #[test]
    fn availability_defaults_to_pending() {
        let mut a = Availability::new();
        assert_eq!(a.get(tab(1, 0)), TabResolution::Pending);
        assert_eq!(a.resolve(tab(1, 0), Fallback::Keep), None);
        assert_eq!(a.get(tab(1, 0)), TabResolution::Resolved(Fallback::Keep));
        assert_eq!(a.resolve(tab(1, 0), OMIT), Some(Fallback::Keep));
    }
}
