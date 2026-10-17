//! Live_Session tracking: reconciliation, caps, and the `LiveSessionTracker`.
//!
//! Requirements 1.7, 1.10, 2.1–2.10, 3.1–3.5, 3.11, 8.1, 8.8.

pub mod caps;
pub mod reconcile;
pub mod session;

pub use caps::{apply_caps, Excluded};
pub use reconcile::{Reconciled, ScannedTab, TabOrderReconciler};
pub use session::{
    LiveSession, LiveSessionTracker, LiveTab, LiveWindow, SessionEnd, TrackerOutput, WindowStatus,
    TOMBSTONE_MS,
};
