//! `rotation_needed` and the pure `LogRotationModel`.
//!
//! Requirement 13.4 (design.md, "Activity_Log entry"; Property 29).
//!
//! Before each append the writer asks whether `size + entry_len` would exceed
//! [`MAX_LOG_BYTES`]. If so, the current Activity_Log ([`ACTIVITY_LOG_FILE_NAME`])
//! is renamed to the Previous_Log_File ([`PREVIOUS_LOG_FILE_NAME`]), replacing
//! any existing one, and the entry is written to a new, empty Activity_Log. At
//! most two log files therefore exist and an entry is never split.
//!
//! Decisions made here (and followed by `LogSink`, task 13.2):
//!
//! - **Line terminator.** Every entry is written as the line from
//!   [`format_entry`](super::format_entry) followed by a single LF
//!   ([`LINE_TERMINATOR`]), as in the session file. `entry_len` counts the
//!   UTF-8 bytes of the line plus the terminator ([`entry_len`]).
//! - **Oversized entries.** An entry larger than [`MAX_LOG_BYTES`] on its own
//!   is still written whole (entries are never split or truncated). If the
//!   current log is non-empty it is rotated first, so the oversized entry is
//!   the only entry in the new Activity_Log, which then exceeds the cap; the
//!   next append rotates again.
//! - **Empty current log.** No rotation is performed when the current log is
//!   empty (or absent): it already is the "new, empty Activity_Log", and
//!   rotating it would only replace the Previous_Log_File with an empty file
//!   and lose history. [`rotation_needed`] is the raw size predicate; the
//!   empty-log rule lives in [`LogRotationModel::plan_append`].
//! - **Failures (Req 13.7).** The model is plan/apply: the writer asks for an
//!   [`AppendPlan`], performs it, and reports an [`AppendOutcome`]. If the
//!   rename fails, the entry is discarded rather than appended past the cap,
//!   and the state is unchanged so the next entry retries the rotation. If
//!   the rename succeeds but the write fails, the entry is discarded and the
//!   state reflects the completed rotation. The writer may call
//!   [`LogRotationModel::resync`] with sizes read from disk at any time (at
//!   startup, or after a write failed part-way).

/// Size limit of the Activity_Log in bytes (Req 13.4).
pub const MAX_LOG_BYTES: u64 = 1_048_576;

/// File name of the Activity_Log in the application data folder.
pub const ACTIVITY_LOG_FILE_NAME: &str = "activity.log";

/// File name of the Previous_Log_File in the application data folder.
pub const PREVIOUS_LOG_FILE_NAME: &str = "activity.previous.log";

/// Terminator written after every Activity_Log line (LF, as in the session file).
pub const LINE_TERMINATOR: &str = "\n";

/// Number of bytes appended for `line` (a formatted entry without terminator):
/// its UTF-8 length plus [`LINE_TERMINATOR`].
pub fn entry_len(line: &str) -> u64 {
    (line.len() + LINE_TERMINATOR.len()) as u64
}

/// True when appending `entry_len` bytes to a log of `size` bytes would make
/// it larger than [`MAX_LOG_BYTES`] (Req 13.4). Saturates instead of
/// overflowing.
pub fn rotation_needed(size: u64, entry_len: u64) -> bool {
    size.saturating_add(entry_len) > MAX_LOG_BYTES
}

/// What the writer must do to append one entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppendPlan {
    /// Rename the Activity_Log to the Previous_Log_File (replacing it) before
    /// writing the entry to a new, empty Activity_Log.
    pub rotate_first: bool,
    /// Bytes the entry occupies, terminator included.
    pub entry_len: u64,
}

/// Result of carrying out an [`AppendPlan`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppendOutcome {
    /// The planned rotation (if any) and the write both succeeded.
    Written,
    /// The planned rotation succeeded, but writing the entry failed; the entry
    /// is discarded (Req 13.7).
    WriteFailedAfterRotation,
    /// Nothing changed on disk: the planned rotation failed, or no rotation
    /// was planned and the write failed. The entry is discarded (Req 13.7).
    Failed,
}

/// Pure model of the Activity_Log / Previous_Log_File pair.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LogRotationModel {
    current_size: u64,
    has_previous: bool,
}

impl LogRotationModel {
    /// No log files yet.
    pub const fn new() -> Self {
        LogRotationModel { current_size: 0, has_previous: false }
    }

    /// State read from disk: the Activity_Log size (`None` when it does not
    /// exist) and whether a Previous_Log_File exists.
    pub fn from_disk(current_size: Option<u64>, has_previous: bool) -> Self {
        LogRotationModel { current_size: current_size.unwrap_or(0), has_previous }
    }

    /// Replace the tracked state with what is on disk (see [`Self::from_disk`]).
    pub fn resync(&mut self, current_size: Option<u64>, has_previous: bool) {
        *self = Self::from_disk(current_size, has_previous);
    }

    /// Size of the Activity_Log in bytes (0 when it is empty or absent).
    pub fn current_size(&self) -> u64 {
        self.current_size
    }

    /// Whether a Previous_Log_File exists.
    pub fn has_previous(&self) -> bool {
        self.has_previous
    }

    /// Plan the append of an entry of `entry_len` bytes (terminator included).
    /// Rotates first when [`rotation_needed`] and the current log is non-empty.
    pub fn plan_append(&self, entry_len: u64) -> AppendPlan {
        AppendPlan {
            rotate_first: self.current_size > 0 && rotation_needed(self.current_size, entry_len),
            entry_len,
        }
    }

    /// Update the state after the writer carried out `plan` with `outcome`.
    pub fn apply(&mut self, plan: AppendPlan, outcome: AppendOutcome) {
        match outcome {
            AppendOutcome::Written => {
                if plan.rotate_first {
                    self.rotate();
                }
                self.current_size = self.current_size.saturating_add(plan.entry_len);
            }
            AppendOutcome::WriteFailedAfterRotation => {
                if plan.rotate_first {
                    self.rotate();
                }
            }
            AppendOutcome::Failed => {}
        }
    }

    /// Plan and apply a successful append; returns the plan that was applied.
    pub fn append(&mut self, entry_len: u64) -> AppendPlan {
        let plan = self.plan_append(entry_len);
        self.apply(plan, AppendOutcome::Written);
        plan
    }

    /// Record a failed write that changed nothing on disk (state unchanged).
    pub fn record_failed(&mut self, plan: AppendPlan) {
        self.apply(plan, AppendOutcome::Failed);
    }

    fn rotate(&mut self) {
        self.has_previous = true;
        self.current_size = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_needed_only_when_cap_exceeded() {
        assert!(!rotation_needed(0, MAX_LOG_BYTES));
        assert!(!rotation_needed(MAX_LOG_BYTES - 10, 10));
        assert!(rotation_needed(MAX_LOG_BYTES - 10, 11));
        assert!(rotation_needed(MAX_LOG_BYTES, 1));
        assert!(rotation_needed(0, MAX_LOG_BYTES + 1));
        assert!(rotation_needed(u64::MAX, u64::MAX));
    }

    #[test]
    fn entry_len_counts_utf8_bytes_and_lf() {
        assert_eq!(entry_len(""), 1);
        assert_eq!(entry_len("SAVE"), 5);
        assert_eq!(entry_len("é"), 3);
    }

    #[test]
    fn file_names() {
        assert_eq!(ACTIVITY_LOG_FILE_NAME, "activity.log");
        assert_eq!(PREVIOUS_LOG_FILE_NAME, "activity.previous.log");
    }

    #[test]
    fn appends_accumulate_until_exactly_full() {
        let mut m = LogRotationModel::new();
        assert!(!m.append(MAX_LOG_BYTES - 100).rotate_first);
        assert!(!m.append(100).rotate_first);
        assert_eq!(m.current_size(), MAX_LOG_BYTES);
        assert!(!m.has_previous());
    }

    #[test]
    fn exceeding_cap_rotates_and_starts_new_log_with_entry() {
        let mut m = LogRotationModel::new();
        m.append(MAX_LOG_BYTES - 5);
        let plan = m.append(6);
        assert_eq!(plan, AppendPlan { rotate_first: true, entry_len: 6 });
        assert_eq!(m.current_size(), 6);
        assert!(m.has_previous());

        // A second rotation replaces the previous file; still two files.
        m.append(MAX_LOG_BYTES);
        assert_eq!(m.current_size(), MAX_LOG_BYTES);
        assert!(m.has_previous());
    }

    #[test]
    fn oversized_entry_is_written_whole_after_rotation() {
        let mut m = LogRotationModel::new();
        m.append(10);
        let plan = m.append(MAX_LOG_BYTES + 1);
        assert!(plan.rotate_first);
        assert_eq!(m.current_size(), MAX_LOG_BYTES + 1);
        // The next entry rotates the oversized log away.
        assert!(m.append(1).rotate_first);
        assert_eq!(m.current_size(), 1);
    }

    #[test]
    fn empty_log_is_never_rotated() {
        let mut m = LogRotationModel::from_disk(None, true);
        let plan = m.append(MAX_LOG_BYTES + 1);
        assert!(!plan.rotate_first);
        assert_eq!(m.current_size(), MAX_LOG_BYTES + 1);
        assert!(m.has_previous());
    }

    #[test]
    fn failed_write_leaves_state_unchanged() {
        let mut m = LogRotationModel::new();
        m.append(MAX_LOG_BYTES);
        let before = m;
        let plan = m.plan_append(1);
        assert!(plan.rotate_first);
        m.record_failed(plan);
        assert_eq!(m, before);
        m.apply(plan, AppendOutcome::Failed);
        assert_eq!(m, before);
        // The next entry retries the rotation.
        assert!(m.plan_append(1).rotate_first);
    }

    #[test]
    fn write_failure_after_rotation_keeps_rotation() {
        let mut m = LogRotationModel::new();
        m.append(MAX_LOG_BYTES);
        let plan = m.plan_append(10);
        m.apply(plan, AppendOutcome::WriteFailedAfterRotation);
        assert_eq!(m.current_size(), 0);
        assert!(m.has_previous());
        // Without a planned rotation this outcome changes nothing.
        let plan = m.plan_append(10);
        assert!(!plan.rotate_first);
        m.apply(plan, AppendOutcome::WriteFailedAfterRotation);
        assert_eq!(m.current_size(), 0);
    }

    #[test]
    fn resync_from_disk() {
        let mut m = LogRotationModel::new();
        m.resync(Some(MAX_LOG_BYTES + 7), false);
        assert_eq!(m.current_size(), MAX_LOG_BYTES + 7);
        assert!(!m.has_previous());
        assert!(m.plan_append(1).rotate_first);
        m.resync(None, true);
        assert_eq!(m, LogRotationModel::from_disk(Some(0), true));
    }
}
