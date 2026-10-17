//! Activity_Log entry formatting and rotation model.
//!
//! Requirements 9.7, 13.1–13.5.

pub mod entry;
pub mod rotation;

pub use entry::{format_entry, parse_line, LogEntry, ParsedLine};
pub use rotation::{
    entry_len, rotation_needed, AppendOutcome, AppendPlan, LogRotationModel, ACTIVITY_LOG_FILE_NAME,
    LINE_TERMINATOR, MAX_LOG_BYTES, PREVIOUS_LOG_FILE_NAME,
};
