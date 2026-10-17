//! Session_Store logic: `StoreLogic` state machine (startup, checkpoints,
//! Session_End saves, retry, attempt counting, protection, consumption).
//!
//! Task 8.3 implements the base behaviour in [`logic`] (startup, checkpoints,
//! Session_End saves, write failure and retry); task 8.4 adds attempt
//! counting, protection, consumption, and abandonment (Requirements 3.1–3.12,
//! 4.4, 4.6, 4.7, 4.10, 4.11, 11.7, 11.10, 11.11, 13.1, 13.3).

pub mod atomic;
pub mod clock;
pub mod logic;

pub use atomic::{
    atomic_replace, cleanup_all_leftover_temps, cleanup_leftover_temp, preserve_backup,
    AtomicFile, AtomicWriteError, FsOps, Step, ATOMIC_FILES, BACKUP_FILE, SESSION_FILE,
};
pub use clock::{local_time, WallClock, UNKNOWN_TIME};
pub use logic::{
    FileView, LiveView, StoreLogic, CHECKPOINT_DELAY_MS, MAX_RETRIES, RETRY_INTERVAL_MS,
};
