//! `RestoreSession` state machine: executes restore-plan diffs, tracks slots,
//! verifies tab order, and (tasks 11.3, 11.4) composes Targeted_Launches and
//! handles completion, timeouts, and failure.
//!
//! Requirements 4.4, 4.5, 4.10, 5.1, 5.3, 5.4, 5.7, 5.10, 5.11, 6.1–6.5,
//! 7.2–7.7, 7.9, 8.3, 8.6, 9.1, 9.2, 9.4, 9.7, 9.8, 13.2, 13.3 (design.md,
//! "Restore pipeline", "Error Handling").
//!
//! # Lifecycle
//!
//! 1. **Attempt counter.** The caller (the Engine with the store, task 11.5)
//!    issues the durable `WriteSessionFile` that increments `failed_attempts`
//!    and passes its `seq` in [`RestoreParams::attempt_write_seq`]. The
//!    session issues nothing until `WriteCompleted { seq }` arrives
//!    successfully (increment-before-attempt). A failed write stops the
//!    session ([`Failure::AttemptWriteFailed`]) before any window is touched.
//! 2. **Probes.** Then one `Probe` per saved tab, all at once.
//! 3. **Planning.** [`plan_restore`](crate::plan::plan_restore) runs at start
//!    (every tab pending) and again on every probe result; the session then
//!    drives the live windows towards the new plan (the "diff"). Driving is
//!    idempotent: every event ends with one pass over all windows that
//!    issues whatever the current plan still needs. With deferral
//!    ([`RestoreSession::with_deferral`], Req 5.13), the slots of deferred
//!    tabs show their placeholders instead of their saved Locations.
//! 4. **Trigger.** Once the trigger mapping is decided, the trigger's slots
//!    are built; its final placement is applied exactly once, as soon as
//!    `plan.windows[0].decided` (Req 7.7).
//! 5. **Other windows** are opened only when `decided` and known restorable,
//!    on their first known-restorable Location, with their final placement
//!    (Req 5.1, 7.2, 7.3). Each window is opened once.
//! 6. **Slots** (see [`window`]): physical tabs are bound to planned slots
//!    left to right. New tabs are always appended rightmost (R2), with at
//!    most one structural command (`CreateTab` or `CloseTab`) outstanding per
//!    window (`TabCreationPipelining::Sequential`, the `esr-win::strategy`
//!    default). A tab is navigated to its slot's Location as soon as it
//!    registers, and the next `CreateTab` goes out at the same time, without
//!    waiting for that navigation (`OVERLAP_NAVIGATION_WITH_CREATION`).
//!    Windows run in parallel. Placeholder slots get a tab at their saved
//!    position (Req 7.9); a placeholder whose probe fails is closed, or its
//!    tab is re-bound to a free slot when that keeps the order.
//! 7. **Verification.** When a window has its final slot count and every
//!    navigation completed, a rescan is requested; its UIA order is
//!    reconciled onto the tab keys ([`TabOrderReconciler`]) and mismatched
//!    slots are re-navigated (Req 5.3, 8.3). Tabs an overflowing strip does
//!    not expose keep their assumed position and are not re-navigated.
//! 8. **Targeted_Launch** (Req 6.2–6.5): the trigger's slot 0 is navigated
//!    to restored tab 0 and the requested Location gets a final, rightmost
//!    slot of its own (also when a restored tab shows the same Location).
//!    Once the trigger is settled, its requested slot is activated
//!    (`ActivateTab`) and then the selection captured at launch
//!    ([`RestoreParams::requested_selection`]) is applied to its tab
//!    (`SelectItems`). Both are re-issued if the trigger changes afterwards
//!    (a re-navigation clears the selection). Neither has a completion event
//!    (the activation is confirmed by a scan, see 10.);
//!    a failure is retried [`UIA_RETRIES`] times, after which the activation
//!    or the selection is given up ([`RestoreSession::selection_error`]);
//!    neither fails the restoration.
//! 9. **Settled** ([`RestoreSession::is_settled`]): every probe resolved,
//!    every planned window built and verified, and for a Targeted_Launch the
//!    trigger's activation and selection issued.
//! 10. **Activation** (Req 5.4, 9.4, 6.3): as soon as a window is built
//!     (every planned slot has its tab and shows its Location; task 16.3)
//!     its planned Active_Tab ([`RestoreSession::active_tabs`]) is activated
//!     with `ActivateTab`, issued before the verification `RescanWindow`, so
//!     that the verification scan's strip read, queued behind the `Select`,
//!     can confirm both (a settled window gets a `RescanWindow` barrier
//!     after it; a Targeted trigger is still activated once settled, after
//!     the `SelectItems`). The activation is by visual position and is kept
//!     while the window keeps its final tab structure. `ActivateTab` has no
//!     completion event and the adapter carries it out asynchronously (it
//!     waits for the tab strip to catch up with the tab count, then rescans
//!     the window).
//!     The activation is outstanding ([`OpKind::ActivateTab`]) until the
//!     adapter reports `ActivationConfirmed` (it saw the tab still selected
//!     after the `Select`; a settled window is then confirmed at once, an
//!     unsettled one by its verification scan, with the deadline restarted),
//!     or a scan of the settled window shows the planned
//!     tab selected (by the UIA selection when the strip exposes every tab,
//!     else by `WindowObservation::active`). A scan that can tell neither, or
//!     that shows another tab selected, leaves it outstanding (task 16.3:
//!     strip reads of windows Explorer is still busy with time out, and
//!     accepting such scans completed restorations before the activation
//!     had happened).
//!     An `OperationFailed` for it, or no confirmation within
//!     [`ACTIVATE_TAB_TIMEOUT_MS`], is a failed attempt: it is re-issued
//!     [`UIA_RETRIES`] times, then given up with an `ERROR` entry, and the
//!     window completes with whatever tab Explorer shows. Only that window's
//!     Active_Tab can then differ from the saved one; the restoration does not
//!     fail (design.md, "Error Handling"). If the window changes afterwards
//!     and is no longer settled, the activation is issued again once it is.
//! 11. **Completion** (Restoration_Completion, Req 4.4, 5.7, 5.10, 5.11,
//!     7.4, 9.7, 9.8, 13.2): once every probe is resolved and every planned
//!     window is settled with its activation confirmed, the session issues in
//!     one batch: `Reveal` for every window it opened (the trigger is already
//!     visible), one `SetZOrder` with the planned z-order, `Focus` on the
//!     planned focus target, the `RESTORE` summary, one `TAB-OMITTED` or
//!     `TAB-REDIRECTED` entry per omitted or redirected saved tab (saved
//!     Activation_Order, then tab order; tabs of omitted windows included),
//!     and one `Notify { omitted_tabs }` when at least one tab was omitted.
//!     It then enters [`Phase::Completed`] and reports
//!     [`RestoreOutcome::Completed`]. An empty plan (every window omitted,
//!     Req 9.6) completes as soon as every probe is resolved, with no window
//!     command. Marking the snapshot Consumed, rebuilding the Live_Session,
//!     and the full rescan belong to the Engine, which owns the store and the
//!     tracker (see "Engine contract" below).
//! 12. **Failure** (Req 4.10, 13.3): an `OperationFailed` for an outstanding
//!     operation other than `ActivateTab`, a timeout of one (below), a
//!     restored window closing, `ShellLost` (explorer.exe ended or crashed:
//!     [`Failure::ShellLost`], at once rather than at the operations'
//!     timeouts), or a failed
//!     attempt-counter write stops the session ([`Phase::Failed`]). It issues
//!     no further restoration command, leaves every window open, and logs one
//!     `ERROR` (operation and description; a failed attempt-counter write is
//!     already logged by the store, so the session adds none). Windows it
//!     opened are created hidden (alpha 0), so they are revealed rather than
//!     left invisible; a window whose `OpenWindow` was still outstanding is
//!     revealed when it arrives. It then reports [`RestoreOutcome::Failed`].
//!
//! # Timeouts
//!
//! The core is pure: the session sets one `Restore` timer
//! (`TimerId { owner: TimerOwner::Restore, seq: first_id }`) for the earliest
//! deadline among its [outstanding operations](RestoreSession::outstanding_ops)
//! (issue time + the operation's timeout) and re-arms it when it fires. When
//! `Event::Timer` arrives for it, any operation past its deadline fails the
//! restoration with [`Failure::Timeout`]. A timer that fires after the
//! session finished is ignored. Timeouts:
//!
//! - `OpenWindow`: [`OPEN_WINDOW_TIMEOUT_MS`] (5 s);
//! - `CreateTab`: [`TAB_CREATE_TIMEOUT_MS`] (2 s after the navigator's last
//!   attempt);
//! - `Navigate`: [`NAVIGATION_TIMEOUT_MS`] (5 s). Design.md names a *local*
//!   navigation timeout; network navigations get the same 5 s because they
//!   are only issued after a successful probe, which already showed that the
//!   Location responds within 5 s (Req 9.3);
//! - `CloseTab`: [`CLOSE_TAB_TIMEOUT_MS`] (5 s; not in design.md, which lists
//!   no close timeout, but a close that never completes would otherwise hold
//!   the restoration, and with it checkpoints, forever);
//! - `ActivateTab` until confirmed: [`ACTIVATE_TAB_TIMEOUT_MS`] (3 s: the
//!   adapter's 1.5 s wait for the strip to catch up, the 500 ms UIA budget
//!   of design.md, and the confirming scan, with margin). Expiry is a failed
//!   activation attempt, not a failure of the session.
//!
//! Probes need no timer: the prober reports `Timeout` itself after 5 s.
//!
//! # Engine contract
//!
//! The Engine ([`crate::engine::Engine`]) holds at most one session, created
//! by `Engine::start_restoration` (used by trigger detection, task 11.5).
//! After every `RestoreSession::handle` it calls
//! [`RestoreSession::take_outcome`]:
//!
//! - `Completed`: the planned Activation_Order is applied to the tracker
//!   (Req 4.5), orphans are dropped (`LiveSessionTracker::drop_orphans`,
//!   Req 3.3), `StoreLogic::restoration_completed` marks the snapshot
//!   Consumed (Req 4.4), and `RescanAll` rebuilds the Live_Session.
//! - `Failed`: `StoreLogic::restoration_failed` keeps the counted attempt and
//!   protection (or abandons the snapshot at the third failure, Req 4.11).
//!
//! # Hooks for task 11.5
//!
//! Trigger detection creates the session (`Engine::start_restoration`) and
//! asks [`RestoreSession::owns_window`] / [`RestoreSession::is_pending_request`]
//! / [`RestoreSession::has_pending_requests`]
//! and [`RestoreSession::is_finished`] (a restoration is in progress while a
//! session exists and is not finished).
//!
//! [`TabOrderReconciler`]: crate::tracker::TabOrderReconciler

mod session;
pub mod window;

pub use session::{
    Completion, Failure, OpKind, OutstandingOp, Phase, RestoreOutcome, RestoreParams, RestoreSession,
};
pub use window::SlotKey;

/// Window open timeout (design.md, "Error Handling"), from `OpenWindow`.
pub const OPEN_WINDOW_TIMEOUT_MS: u64 = 5_000;

/// The navigator's re-post delay for a new-tab command that produced no tab
/// (`esr-win::strategy::NEW_TAB_REPOST_AFTER_MS`).
pub const NEW_TAB_REPOST_AFTER_MS: u64 = 1_500;

/// Tab creation timeout, from `CreateTab`. The navigator retries on its own
/// (re-post after 1.5 s, then the UIA `AddButton` after another 1.5 s), and
/// the design's 2 s tab timeout counts from that last attempt, so the core
/// must not give up before 2 × 1.5 s + 2 s.
pub const TAB_CREATE_TIMEOUT_MS: u64 = 2 * NEW_TAB_REPOST_AFTER_MS + 2_000;

/// Navigation timeout, from `Navigate` (local and network alike; see the
/// module docs, "Timeouts").
pub const NAVIGATION_TIMEOUT_MS: u64 = 5_000;

/// Tab close timeout, from `CloseTab` (an addition; see "Timeouts").
pub const CLOSE_TAB_TIMEOUT_MS: u64 = 5_000;

/// Time for an `ActivateTab` to be confirmed by a scan (see "Timeouts").
pub const ACTIVATE_TAB_TIMEOUT_MS: u64 = 3_000;

/// The timeout of an outstanding operation of `kind`, if it has one.
pub fn timeout_ms(kind: OpKind) -> Option<u64> {
    match kind {
        OpKind::OpenWindow => Some(OPEN_WINDOW_TIMEOUT_MS),
        OpKind::CreateTab => Some(TAB_CREATE_TIMEOUT_MS),
        OpKind::Navigate => Some(NAVIGATION_TIMEOUT_MS),
        OpKind::CloseTab => Some(CLOSE_TAB_TIMEOUT_MS),
        OpKind::ActivateTab => Some(ACTIVATE_TAB_TIMEOUT_MS),
        // Never outstanding (no completion event, nothing to confirm).
        OpKind::SelectItems => None,
    }
}

/// Most `Navigate` commands per tab (initial + re-navigations), so that a
/// Location Explorer never reports back cannot loop.
pub const MAX_NAV_ATTEMPTS: u32 = 3;

/// Most UIA verification rounds per window before its order is accepted.
pub const MAX_VERIFY_ROUNDS: u32 = 3;

/// Retries of a failed `ActivateTab` or `SelectItems` (design.md, "Error
/// Handling": a failed activation is retried once, then given up).
pub const UIA_RETRIES: u32 = 1;
