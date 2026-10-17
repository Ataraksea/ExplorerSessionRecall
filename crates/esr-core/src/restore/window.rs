//! One window under restoration: physical tabs bound to planned slots.
//!
//! # Binding
//!
//! Each physical tab (left to right, as the session believes Explorer shows
//! them) is bound to at most one planned slot ([`SlotKey`]). Bindings are
//! strictly increasing from left to right, so the tab order is the planned
//! order. Plans only ever drop slots or turn a placeholder into a restore
//! slot, never insert one, so bindings stay valid as probes resolve.
//!
//! - A **new tab** (always appended rightmost) binds to the first planned slot
//!   after the last bound one, or to nothing if every slot is taken (it is
//!   then closed).
//! - A tab whose slot was **dropped** (an omitted placeholder) is re-bound to
//!   a free planned slot between its bound neighbours if there is one, else
//!   closed. A window's only tab is therefore never closed: with no bound
//!   neighbours, any planned slot fits.
//! - **Navigation:** a bound tab whose slot has a Location it does not show
//!   is navigated to it, at once.
//! - **Structure:** at most one `CreateTab` or `CloseTab` is outstanding per
//!   window (`TabCreationPipelining::Sequential`; UIA closes also need a
//!   stable strip). Closes go first.
//!
//! # Verification
//!
//! When the window is *built* (every planned slot bound, every navigation
//! completed, no structural command outstanding), a rescan is requested. The
//! scan's UIA names are reconciled onto the tab keys with the tracker's
//! [`TabOrderReconciler`] (match names = display and parsing name of what
//! each tab shows). If the visual order differs, the tabs are re-bound by
//! position; every tab whose observed Location differs from its slot is then
//! re-navigated. Items an overflowing strip does not expose leave their tabs
//! in their assumed position, so they are never re-navigated for that.
//!
//! # Activation and Targeted_Launch (Req 5.4, 6.2–6.5, 9.4)
//!
//! Once a window is settled, `LiveWindow::finish` activates its planned
//! active slot and requests the barrier scan that confirms it
//! (`LiveWindow::confirm_active`); a failed activation is retried, then
//! given up (`LiveWindow::activation_failed`). The trigger of a Targeted_Launch has
//! slots `[restored tabs] + [requested]`: its original tab (slot 0) is bound
//! to restored tab 0 and navigated there, and the requested Location gets a
//! new, rightmost tab like any other slot, even when a restored tab shows the
//! same Location. Once the trigger is settled, `LiveWindow::finish_targeted`
//! activates the requested slot and then applies the captured selection to
//! its tab. Both are issued again if the window changes afterwards (a
//! re-navigation clears the selection).

use std::collections::HashMap;

use crate::engine::{Command, ItemId, Millis, ObservedLocation, OpId, TabKey, WinKey, WindowObservation};
use crate::model::LocationId;
use crate::plan::{PlannedWindow, Slot};
use crate::tracker::{ScannedTab, TabOrderReconciler};

use super::session::{OpKind, Ops};
use super::{MAX_NAV_ATTEMPTS, MAX_VERIFY_ROUNDS};

/// Identity of a planned slot that survives re-planning: the saved tab it
/// restores, or the requested tab of a Targeted_Launch (always last).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SlotKey {
    Saved(usize),
    Requested,
}

impl SlotKey {
    pub fn of(slot: &Slot) -> SlotKey {
        match slot {
            Slot::Restore { tab_index, .. } | Slot::Placeholder { tab_index } => SlotKey::Saved(*tab_index),
            Slot::Requested(_) => SlotKey::Requested,
        }
    }
}

/// The Location a slot's tab must show (`None` for a placeholder).
pub(crate) fn target(slot: &Slot) -> Option<&LocationId> {
    match slot {
        Slot::Restore { location, .. } | Slot::Requested(location) => Some(location),
        Slot::Placeholder { .. } => None,
    }
}

fn slot_target(pw: &PlannedWindow, key: SlotKey) -> Option<&LocationId> {
    pw.slots.iter().find(|s| SlotKey::of(s) == key).and_then(target)
}

/// One physical tab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PhysTab {
    /// `None` until learned (the initial tab of an opened window, or of the
    /// trigger when the caller did not know it).
    pub key: Option<TabKey>,
    pub slot: Option<SlotKey>,
    /// What the tab shows, as last reported.
    pub shown: Option<LocationId>,
    /// Outstanding `Navigate`.
    pub nav: Option<(OpId, LocationId)>,
    pub nav_attempts: u32,
    /// `SelectItems` issued since the window last settled (the requested tab
    /// of a Targeted_Launch only).
    pub selected: Option<OpId>,
}

impl PhysTab {
    pub fn new(key: Option<TabKey>, slot: Option<SlotKey>, shown: Option<LocationId>) -> Self {
        PhysTab { key, slot, shown, nav: None, nav_attempts: 0, selected: None }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Verification {
    /// Not built, or changed since the last check.
    Pending,
    /// Built; waiting for a rescan to check the UIA order.
    AwaitingScan,
    Verified,
}

/// A window under restoration (the trigger or one the session opened).
#[derive(Clone, Debug)]
pub(crate) struct LiveWindow {
    /// `None` until `WindowOpened` (never `None` for the trigger).
    pub key: Option<WinKey>,
    pub placement_applied: bool,
    /// Left to right.
    pub tabs: Vec<PhysTab>,
    pub creating: Option<OpId>,
    pub closing: Option<(OpId, TabKey)>,
    pub verification: Verification,
    pub verify_rounds: u32,
    /// `ActivateTab { index }` issued since the window last settled.
    pub activated: Option<(usize, OpId)>,
    /// A scan confirmed `activated` (see the module docs of
    /// [`crate::restore`], "Activation").
    pub active_confirmed: bool,
    /// Failed (or unconfirmed) `ActivateTab` commands of this window.
    pub activate_failures: u32,
    /// The activation was given up after [`super::UIA_RETRIES`] retries: the
    /// window completes with whatever tab Explorer shows (logged).
    pub activation_abandoned: bool,
    /// The adapter reported this activation op as carried out
    /// (`ActivationConfirmed`); the next settled scan confirms it.
    pub adapter_confirmed: Option<OpId>,
}

impl LiveWindow {
    pub fn new(key: Option<WinKey>, first: PhysTab) -> Self {
        LiveWindow {
            key,
            placement_applied: false,
            tabs: vec![first],
            creating: None,
            closing: None,
            verification: Verification::Pending,
            verify_rounds: 0,
            activated: None,
            active_confirmed: false,
            activate_failures: 0,
            activation_abandoned: false,
            adapter_confirmed: None,
        }
    }

    /// The tab bound to `slot`.
    pub fn tab_for(&self, slot: SlotKey) -> Option<&PhysTab> {
        self.tabs.iter().find(|t| t.slot == Some(slot))
    }

    /// Forgets the activation and selection issued for this window, so that
    /// they are issued again once it settles again (a change may have moved
    /// the active tab, and a navigation clears the item selection).
    pub fn reset_finish(&mut self, ops: &mut Ops) {
        if let Some((_, op)) = self.activated.take() {
            ops.done(op);
        }
        self.active_confirmed = false;
        for t in &mut self.tabs {
            t.selected = None;
        }
    }

    /// Issues `ActivateTab { index }` unless it was already issued since the
    /// window last settled (or the activation was given up); returns whether
    /// it did. The op stays outstanding until a scan confirms it
    /// ([`Self::confirm_active`]); the caller follows it with a
    /// `RescanWindow` as the barrier.
    pub fn ensure_active(&mut self, index: usize, ops: &mut Ops, now: Millis, out: &mut Vec<Command>) -> bool {
        let Some(win) = self.key else { return false };
        if self.activation_abandoned || self.activated.is_some_and(|(i, _)| i == index) {
            return false;
        }
        if let Some((_, old)) = self.activated.take() {
            ops.done(old);
        }
        let op = ops.issue(OpKind::ActivateTab, now);
        self.activated = Some((index, op));
        self.active_confirmed = false;
        out.push(Command::ActivateTab { op, win, index });
        true
    }

    /// Activation for a window other than a Targeted trigger (Req 5.4, 9.4):
    /// as soon as the window is built ([`Self::is_built`]: every planned
    /// slot has its tab and shows its Location), before the verification
    /// scan, its planned active slot is activated (task 16.3: activating
    /// only once the window was verified queued the `Select` behind the
    /// verification's strip read, which takes up to 1.5 s while Explorer is
    /// still busy; activating as soon as the last tab registered made the
    /// strip search compete with the window's last navigations). The
    /// `ActivateTab` goes before the verification's `RescanWindow`, so the
    /// scan's strip read can confirm both. The activation is by visual
    /// position, which neither a navigation nor a verification re-binding
    /// changes, so it is kept while the window keeps its final tab structure
    /// ([`Self::is_structured`]) and forgotten otherwise. A `RescanWindow`
    /// barrier follows when the window is already settled.
    pub fn finish(&mut self, pw: &PlannedWindow, ops: &mut Ops, now: Millis, out: &mut Vec<Command>) {
        if !self.is_structured(pw) {
            self.reset_finish(ops);
            return;
        }
        if !self.is_built(pw) && self.activated.is_none() {
            return;
        }
        let Some(win) = self.key else { return };
        // The verification scan `drive` just requested goes after the
        // activation, so that its strip read (queued behind the `Select` on
        // the window's UIA worker) can confirm both.
        let scan = out.iter().rposition(|c| *c == Command::RescanWindow(win));
        let at = out.len();
        if self.ensure_active(pw.active, ops, now, out) {
            if let Some(i) = scan {
                let activate = out.remove(at);
                out.insert(i, activate);
            }
            if self.is_settled(pw) {
                out.push(Command::RescanWindow(win));
            }
        }
    }

    /// Every planned slot (no placeholder left) has its tab, in order, and
    /// no `CreateTab` or `CloseTab` is outstanding.
    pub fn is_structured(&self, pw: &PlannedWindow) -> bool {
        self.key.is_some()
            && self.creating.is_none()
            && self.closing.is_none()
            && self.tabs.len() == pw.slots.len()
            && pw.slots.iter().all(|s| !matches!(s, Slot::Placeholder { .. }))
            && self.tabs.iter().zip(&pw.slots).all(|(t, s)| t.key.is_some() && t.slot == Some(SlotKey::of(s)))
    }

    /// Targeted_Launch composition for the trigger (Req 6.2–6.4): once the
    /// window is settled, activates its planned active slot (the requested
    /// slot) and, unless `selection` is empty or `select` is off, applies it
    /// to the requested slot's tab with `SelectItems`, after the activation,
    /// followed by the barrier scan when the activation was (re-)issued.
    /// While the window is not settled, the previous ones are forgotten.
    pub fn finish_targeted(
        &mut self,
        pw: &PlannedWindow,
        selection: &[ItemId],
        select: bool,
        ops: &mut Ops,
        now: Millis,
        out: &mut Vec<Command>,
    ) {
        if !self.is_settled(pw) {
            self.reset_finish(ops);
            return;
        }
        let activated = self.ensure_active(pw.active, ops, now, out);
        if select
            && !selection.is_empty()
            && let Some(t) = self.tabs.iter_mut().find(|t| t.slot == Some(SlotKey::Requested))
            && t.selected.is_none()
            && let Some(tab) = t.key
        {
            let op = ops.untracked();
            t.selected = Some(op);
            out.push(Command::SelectItems { op, tab, items: selection.to_vec() });
        }
        if activated && let Some(win) = self.key {
            out.push(Command::RescanWindow(win));
        }
    }

    /// [`Self::finish_targeted`] has issued everything for the current state.
    pub fn is_finished_targeted(&self, pw: &PlannedWindow, selection: &[ItemId], select: bool) -> bool {
        self.is_settled(pw)
            && (self.activation_abandoned || self.activated.is_some_and(|(i, _)| i == pw.active))
            && (!select
                || selection.is_empty()
                || self.tab_for(SlotKey::Requested).is_some_and(|t| t.selected.is_some()))
    }

    /// Settled with the planned active slot activated and confirmed, or with
    /// the activation given up.
    pub fn is_active_confirmed(&self, pw: &PlannedWindow) -> bool {
        self.is_settled(pw)
            && (self.activation_abandoned
                || (self.active_confirmed && self.activated.is_some_and(|(i, _)| i == pw.active)))
    }

    /// A failed or unconfirmed `ActivateTab` `op` of this window: it is
    /// forgotten (the next drive re-issues it) and counted; once more than
    /// [`super::UIA_RETRIES`] retries failed the activation is given up and
    /// `true` is returned. `None` if `op` is not this window's activation.
    pub fn activation_failed(&mut self, op: OpId, ops: &mut Ops) -> Option<bool> {
        if !self.activated.is_some_and(|(_, o)| o == op) {
            return None;
        }
        ops.done(op);
        self.activated = None;
        self.active_confirmed = false;
        self.activate_failures += 1;
        if self.activate_failures > super::UIA_RETRIES && !self.activation_abandoned {
            self.activation_abandoned = true;
            return Some(true);
        }
        Some(false)
    }

    /// Checks an unconfirmed activation against a scan of this (settled)
    /// window: `Some(true)` confirms it (its op is done), `Some(false)` means
    /// the scan shows another tab selected (the activation may still be in
    /// flight: it is carried out asynchronously), `None` that there is
    /// nothing to check or the scan cannot tell. The UIA selection decides
    /// when the strip exposes every tab, else the scan's active-tab hint. A
    /// scan that can tell neither leaves the activation outstanding until the
    /// adapter confirms it ([`Self::activation_confirmed`]; task 16.3: with
    /// Explorer busy, strip reads time out, and accepting such scans
    /// completed restorations before the tab was selected).
    pub fn confirm_active(&mut self, pw: &PlannedWindow, obs: &WindowObservation, ops: &mut Ops) -> Option<bool> {
        let (index, op) = self.activated?;
        if self.active_confirmed || !self.is_settled(pw) {
            return None;
        }
        let planned = self.tabs.get(index).and_then(|t| t.key);
        let shown = match (&obs.strip, obs.active) {
            (Some(strip), _) if strip.names.len() == self.tabs.len() && strip.selected.is_some() => {
                strip.selected.map(|i| i == index)
            }
            (_, Some(active)) => Some(planned == Some(active)),
            _ => None,
        };
        if shown == Some(true) || (shown.is_none() && self.adapter_confirmed == Some(op)) {
            ops.done(op);
            self.active_confirmed = true;
        }
        shown
    }

    /// `ActivationConfirmed { op }`: the adapter carried out this window's
    /// outstanding activation and saw the tab selected. The next scan that
    /// finds the window still settled confirms it, also when it cannot tell
    /// which tab is selected (a scan showing another one does not). Returns
    /// whether `op` was this window's activation.
    pub fn activation_confirmed(&mut self, op: OpId) -> bool {
        let Some((_, mine)) = self.activated else { return false };
        if mine != op {
            return false;
        }
        self.adapter_confirmed = Some(op);
        true
    }

    /// Confirms the outstanding activation now (the adapter confirmed it and
    /// the window is settled).
    pub fn confirm_now(&mut self, ops: &mut Ops) {
        if let Some((_, op)) = self.activated {
            ops.done(op);
            self.active_confirmed = true;
        }
    }

    pub fn has_tab(&self, tab: TabKey) -> bool {
        self.tabs.iter().any(|t| t.key == Some(tab)) || self.closing.is_some_and(|(_, t)| t == tab)
    }

    fn last_bound(&self) -> Option<SlotKey> {
        self.tabs.iter().rev().find_map(|t| t.slot)
    }

    /// First planned slot after the last bound one (the next tab to create).
    fn next_free(&self, planned: &[SlotKey]) -> Option<SlotKey> {
        let last = self.last_bound();
        planned.iter().copied().find(|&s| last.is_none_or(|l| s > l))
    }

    /// Unbinds dropped slots and re-binds unbound tabs to free slots between
    /// their bound neighbours.
    fn rebind(&mut self, planned: &[SlotKey]) {
        for t in &mut self.tabs {
            if t.slot.is_some_and(|s| !planned.contains(&s)) {
                t.slot = None;
            }
        }
        let mut lo: Option<SlotKey> = None;
        for i in 0..self.tabs.len() {
            if let Some(s) = self.tabs[i].slot {
                lo = Some(s);
                continue;
            }
            let hi = self.tabs[i + 1..].iter().find_map(|t| t.slot);
            let free = planned.iter().copied().find(|&s| lo.is_none_or(|l| s > l) && hi.is_none_or(|h| s < h));
            if let Some(s) = free {
                self.tabs[i].slot = Some(s);
                lo = Some(s);
            }
        }
    }

    /// Every planned slot bound, every navigation done, nothing structural
    /// outstanding.
    pub fn is_built(&self, pw: &PlannedWindow) -> bool {
        self.key.is_some()
            && self.creating.is_none()
            && self.closing.is_none()
            && self.tabs.len() == pw.slots.len()
            && self.tabs.iter().zip(&pw.slots).all(|(t, s)| {
                t.key.is_some()
                    && t.slot == Some(SlotKey::of(s))
                    && t.nav.is_none()
                    && target(s).is_none_or(|l| t.shown.as_ref() == Some(l))
            })
    }

    /// Built, verified, and no placeholder left.
    pub fn is_settled(&self, pw: &PlannedWindow) -> bool {
        self.verification == Verification::Verified
            && self.is_built(pw)
            && pw.slots.iter().all(|s| !matches!(s, Slot::Placeholder { .. }))
    }

    /// Issues whatever this window still needs for `pw`.
    pub fn drive(&mut self, pw: &PlannedWindow, ops: &mut Ops, now: Millis, out: &mut Vec<Command>) {
        let Some(win) = self.key else { return };
        let planned: Vec<SlotKey> = pw.slots.iter().map(SlotKey::of).collect();
        self.rebind(&planned);

        // Close one unbound tab (never the only one; rebind covers that).
        let idle = self.creating.is_none() && self.closing.is_none();
        if idle
            && self.tabs.len() > 1
            && let Some(i) = self.tabs.iter().position(|t| t.slot.is_none() && t.key.is_some())
        {
            let t = self.tabs.remove(i);
            let tab = t.key.expect("checked");
            if let Some((op, _)) = t.nav {
                ops.done(op);
            }
            let op = ops.issue(OpKind::CloseTab, now);
            self.closing = Some((op, tab));
            self.verification = Verification::Pending;
            out.push(Command::CloseTab { op, tab });
        }

        // Navigate every bound tab that does not show its slot's Location.
        for t in &mut self.tabs {
            let (Some(tab), Some(slot)) = (t.key, t.slot) else { continue };
            let Some(location) = slot_target(pw, slot) else { continue };
            if t.nav.is_none() && t.shown.as_ref() != Some(location) && t.nav_attempts < MAX_NAV_ATTEMPTS {
                let op = ops.issue(OpKind::Navigate, now);
                t.nav = Some((op, location.clone()));
                t.nav_attempts += 1;
                self.verification = Verification::Pending;
                out.push(Command::Navigate { op, tab, location: location.clone() });
            }
        }

        // Create the next tab, unless a close is still due.
        let idle = self.creating.is_none() && self.closing.is_none();
        let close_due = self.tabs.len() > 1 && self.tabs.iter().any(|t| t.slot.is_none() && t.key.is_some());
        if idle && !close_due && self.next_free(&planned).is_some() {
            let op = ops.issue(OpKind::CreateTab, now);
            self.creating = Some(op);
            self.verification = Verification::Pending;
            out.push(Command::CreateTab { op, win });
        }

        if self.verification == Verification::Pending && self.is_built(pw) {
            self.verification = Verification::AwaitingScan;
            out.push(Command::RescanWindow(win));
        }
    }

    /// The outstanding `CreateTab` produced `tab` (appended rightmost).
    pub fn tab_created(&mut self, tab: TabKey, pw: Option<&PlannedWindow>) {
        let slot = pw.and_then(|pw| {
            let planned: Vec<SlotKey> = pw.slots.iter().map(SlotKey::of).collect();
            self.next_free(&planned)
        });
        self.creating = None;
        self.tabs.push(PhysTab::new(Some(tab), slot, None));
        self.verification = Verification::Pending;
    }

    /// A navigation of `tab` completed. Returns `false` if it is not ours.
    pub fn navigation_completed(&mut self, tab: TabKey, location: &LocationId, ops: &mut Ops) -> bool {
        let Some(t) = self.tabs.iter_mut().find(|t| t.key == Some(tab)) else { return false };
        if t.shown.as_ref() != Some(location) {
            t.shown = Some(location.clone());
            self.verification = Verification::Pending;
        }
        // An unrelated completion (e.g. a new tab's own first navigation to
        // the start location) does not finish our navigation.
        if let Some((op, target)) = &t.nav
            && target == location
        {
            ops.done(*op);
            t.nav = None;
        } else if t.selected.take().is_some() {
            // A completion nobody is waiting for after the selection was
            // issued: when the new tab's own first navigation to the start
            // location matched our target, it finished our navigation early
            // and this is the real one, which clears the item selection. The
            // selection is issued again once the window is settled.
        }
        true
    }

    /// `tab` was revoked. Returns `false` if it is not ours.
    pub fn tab_revoked(&mut self, tab: TabKey, ops: &mut Ops) -> bool {
        if let Some((op, t)) = self.closing
            && t == tab
        {
            ops.done(op);
            self.closing = None;
            self.verification = Verification::Pending;
            return true;
        }
        // Closed by someone else: forget it.
        let Some(i) = self.tabs.iter().position(|t| t.key == Some(tab)) else { return false };
        if let Some((op, _)) = self.tabs.remove(i).nav {
            ops.done(op);
        }
        self.verification = Verification::Pending;
        true
    }

    /// Learns the key of a tab not known yet (normally the initial tab) from
    /// a scan: the only scanned tab that is not ours, or else the leftmost
    /// such tab in UIA order.
    pub fn learn_keys(&mut self, obs: &WindowObservation) {
        let Some(i) = self.tabs.iter().position(|t| t.key.is_none()) else { return };
        let candidates: Vec<&crate::engine::TabObservation> =
            obs.tabs.iter().filter(|o| !self.has_tab(o.tab)).collect();
        let found = match candidates.as_slice() {
            [] => return,
            [one] => *one,
            many => {
                let scanned: Vec<ScannedTab> = obs.tabs.iter().map(|o| scanned_tab(o.tab, &o.location)).collect();
                let order = TabOrderReconciler::reconcile(&[], &scanned, obs.strip.as_ref()).order;
                let Some(first) = order.iter().find_map(|k| many.iter().find(|o| o.tab == *k)) else { return };
                *first
            }
        };
        let t = &mut self.tabs[i];
        t.key = Some(found.tab);
        if let (None, ObservedLocation::Known(l)) = (&t.nav, &found.location) {
            t.shown = Some(l.clone());
        }
        self.verification = Verification::Pending;
    }

    /// Checks the UIA order of a built window against a fresh scan.
    pub fn verify(&mut self, pw: &PlannedWindow, obs: &WindowObservation) {
        if self.verification != Verification::AwaitingScan || !self.is_built(pw) {
            return;
        }
        let observed: HashMap<TabKey, &ObservedLocation> = obs.tabs.iter().map(|o| (o.tab, &o.location)).collect();
        let keys: Vec<TabKey> = self.tabs.iter().filter_map(|t| t.key).collect();
        if keys.iter().any(|k| !observed.contains_key(k)) {
            return; // not every tab registered in this scan; wait for the next
        }
        let scanned: Vec<ScannedTab> = keys.iter().map(|k| scanned_tab(*k, observed[k])).collect();
        let order = TabOrderReconciler::reconcile(&keys, &scanned, obs.strip.as_ref()).order;

        let mut changed = false;
        if order != keys {
            // Explorer shows the tabs in another order: slot i is whatever
            // tab is at position i.
            let mut by_key: HashMap<TabKey, PhysTab> =
                self.tabs.drain(..).map(|t| (t.key.expect("built"), t)).collect();
            self.tabs = order.iter().filter_map(|k| by_key.remove(k)).collect();
            for (t, s) in self.tabs.iter_mut().zip(&pw.slots) {
                t.slot = Some(SlotKey::of(s));
            }
            changed = true;
        }
        for t in &mut self.tabs {
            if let Some(ObservedLocation::Known(l)) = t.key.and_then(|k| observed.get(&k).copied())
                && t.shown.as_ref() != Some(l)
            {
                t.shown = Some(l.clone());
                changed = true;
            }
        }
        self.verify_rounds += 1;
        self.verification = if changed && self.verify_rounds < MAX_VERIFY_ROUNDS {
            Verification::Pending
        } else {
            Verification::Verified
        };
    }
}

fn scanned_tab(tab: TabKey, location: &ObservedLocation) -> ScannedTab {
    match location {
        ObservedLocation::Known(l) => ScannedTab::from_location(tab, l),
        _ => ScannedTab::new(tab, Vec::new()),
    }
}
