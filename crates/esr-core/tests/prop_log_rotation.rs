//! Property test for Activity_Log rotation.
//!
//! **Property 29: Log rotation** — *for any* sequence of entry lengths
//! appended through the log model:
//! - at most two files exist;
//! - the current file never exceeds 1,048,576 bytes unless it holds a single
//!   entry that is itself larger;
//! - no entry is split across files;
//! - the previous file followed by the current file is a contiguous,
//!   chronologically ordered suffix of all appended entries.
//!
//! The model only tracks sizes, so the test drives a test-side simulation of
//! the two files (each a list of whole entries) by the plans the model
//! returns, exactly as `LogSink` does on disk, and checks the property on the
//! simulated files and the model's agreement with them after every step.
//! Write failures (Req 13.7) are mixed in: a discarded entry is not "appended"
//! and must not appear in either file.
//!
//! **Validates: Requirements 13.4**

// Feature: explorer-session-restore, Property 29: Log rotation

use esr_core::logfmt::rotation::{
    rotation_needed, AppendOutcome, AppendPlan, LogRotationModel, MAX_LOG_BYTES,
};
use proptest::collection::vec;
use proptest::prelude::*;

/// One entry in a simulated file: its sequence number (chronological order of
/// successful appends) and its length in bytes. Entries are stored whole, so
/// an entry can never be split across files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Entry {
    seq: usize,
    len: u64,
}

/// Test-side simulation of the Activity_Log and Previous_Log_File.
#[derive(Debug, Default)]
struct Files {
    current: Option<Vec<Entry>>,
    previous: Option<Vec<Entry>>,
}

impl Files {
    fn size(file: &Option<Vec<Entry>>) -> u64 {
        file.as_ref().map_or(0, |f| f.iter().map(|e| e.len).sum())
    }

    fn file_count(&self) -> usize {
        self.current.is_some() as usize + self.previous.is_some() as usize
    }

    /// Rename the current log to the previous log, replacing it.
    fn rotate(&mut self) {
        self.previous = Some(self.current.take().unwrap_or_default());
    }

    fn write(&mut self, entry: Entry) {
        self.current.get_or_insert_with(Vec::new).push(entry);
    }
}

/// Outcome the simulated writer reports for one append.
#[derive(Clone, Copy, Debug)]
enum WriterResult {
    Ok,
    /// Rename (if planned) succeeded but the write failed.
    WriteFails,
    /// Rename (if planned) failed, or the write failed with no rename.
    Fails,
}

fn entry_len_strategy() -> impl Strategy<Value = u64> {
    prop_oneof![
        // Typical entries; with these alone a file holds hundreds of entries.
        3 => 1u64..=4_096,
        // Large entries so that rotations happen often within a sequence.
        4 => 1u64..=400_000,
        // Near the cap, on both sides of it.
        2 => (MAX_LOG_BYTES - 64)..=(MAX_LOG_BYTES + 64),
        // Oversized entries (written whole).
        1 => (MAX_LOG_BYTES + 1)..=(3 * MAX_LOG_BYTES),
    ]
}

fn op_strategy() -> impl Strategy<Value = (u64, WriterResult)> {
    let outcome = prop_oneof![
        8 => Just(WriterResult::Ok),
        1 => Just(WriterResult::WriteFails),
        1 => Just(WriterResult::Fails),
    ];
    (entry_len_strategy(), outcome)
}

fn check_invariants(files: &Files, model: &LogRotationModel, written: &[Entry]) -> Result<(), TestCaseError> {
    // At most two files exist.
    prop_assert!(files.file_count() <= 2);

    // The model agrees with the simulated files.
    prop_assert_eq!(model.current_size(), Files::size(&files.current));
    prop_assert_eq!(model.has_previous(), files.previous.is_some());

    // The current file never exceeds the cap unless it holds a single entry
    // that is itself larger.
    if let Some(cur) = &files.current {
        let size = Files::size(&files.current);
        if size > MAX_LOG_BYTES {
            prop_assert_eq!(cur.len(), 1, "over-cap log holds {} entries ({} bytes)", cur.len(), size);
            prop_assert!(cur[0].len > MAX_LOG_BYTES);
        }
    }

    // Previous followed by current is a contiguous, chronologically ordered
    // suffix of all successfully appended entries (entries are whole, so none
    // is split).
    let combined: Vec<Entry> = files
        .previous
        .iter()
        .chain(files.current.iter())
        .flatten()
        .copied()
        .collect();
    prop_assert!(combined.len() <= written.len());
    let suffix = &written[written.len() - combined.len()..];
    prop_assert_eq!(&combined[..], suffix);
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    // Feature: explorer-session-restore, Property 29: Log rotation
    #[test]
    fn prop_log_rotation(ops in vec(op_strategy(), 0..80)) {
        let mut model = LogRotationModel::new();
        let mut files = Files::default();
        let mut written: Vec<Entry> = Vec::new();

        for (len, result) in ops {
            let size_before = model.current_size();
            let plan: AppendPlan = model.plan_append(len);
            prop_assert_eq!(plan.entry_len, len);

            // Rotation happens exactly when appending would exceed the cap
            // and there is something to rotate.
            prop_assert_eq!(
                plan.rotate_first,
                size_before > 0 && rotation_needed(size_before, len)
            );
            prop_assert_eq!(rotation_needed(size_before, len), size_before + len > MAX_LOG_BYTES);

            let outcome = match result {
                WriterResult::Ok => {
                    if plan.rotate_first {
                        files.rotate();
                    }
                    let entry = Entry { seq: written.len(), len };
                    files.write(entry);
                    written.push(entry);
                    AppendOutcome::Written
                }
                WriterResult::WriteFails if plan.rotate_first => {
                    files.rotate();
                    AppendOutcome::WriteFailedAfterRotation
                }
                WriterResult::WriteFails | WriterResult::Fails => AppendOutcome::Failed,
            };
            model.apply(plan, outcome);

            check_invariants(&files, &model, &written)?;
        }
    }

    /// Only successful appends through `append` (the common path): the same
    /// invariants, the newest entry is always in the current file, and no
    /// rotation happens early.
    // Feature: explorer-session-restore, Property 29: Log rotation
    #[test]
    fn prop_log_rotation_append_only(lens in vec(entry_len_strategy(), 1..80)) {
        let mut model = LogRotationModel::new();
        let mut files = Files::default();
        let mut written: Vec<Entry> = Vec::new();

        for len in lens {
            let plan = model.append(len);
            if plan.rotate_first {
                files.rotate();
            }
            let entry = Entry { seq: written.len(), len };
            files.write(entry);
            written.push(entry);

            check_invariants(&files, &model, &written)?;

            // The entry just appended is the last one in the current file.
            prop_assert_eq!(files.current.as_ref().and_then(|c| c.last()), Some(&entry));

            // The previous file (if any) is non-empty and, together with the
            // new entry, would have exceeded the cap: nothing is rotated early.
            if plan.rotate_first {
                let prev = Files::size(&files.previous);
                prop_assert!(prev > 0);
                prop_assert!(prev + len > MAX_LOG_BYTES);
            }
        }
    }
}
