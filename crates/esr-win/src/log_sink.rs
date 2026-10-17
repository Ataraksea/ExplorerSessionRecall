//! `LogSink`: appends Activity_Log lines and rotates via `LogRotationModel`.
//!
//! The Activity_Log ([`ACTIVITY_LOG_FILE_NAME`]) and the Previous_Log_File
//! ([`PREVIOUS_LOG_FILE_NAME`]) live in the data directory shared with
//! [`DurableFileStore`](crate::file_store::DurableFileStore), normally
//! `%LOCALAPPDATA%\ExplorerSessionRestore`
//! ([`default_data_dir`](crate::file_store::default_data_dir)). The path is
//! injected into [`LogSink::open`] so tests never touch the real one. Both
//! files inherit the directory's protected DACL (see `file_store`); if the
//! directory is missing when an entry is appended, it is (re-)created with
//! [`ensure_data_dir`] (Req 13.6). Nothing is ever sent over a network.
//!
//! # Appending
//!
//! Each entry is the formatted line followed by
//! [`LINE_TERMINATOR`], written with one
//! `write_all` at the end of the Activity_Log. The file is opened per append
//! (log entries are infrequent), so no handle blocks the rotation rename and
//! users can read or delete the log while the app runs. Before writing,
//! [`LogRotationModel::plan_append`] decides whether the current log must
//! first be renamed to the Previous_Log_File (replacing it) so the entry
//! starts a new, empty Activity_Log (Req 13.4). An entry is never split; an
//! entry larger than the cap is still written whole.
//!
//! # Errors (Req 13.7)
//!
//! Every error is swallowed: the entry is discarded, nothing is reported, and
//! the next entry is attempted normally. A failed rename leaves both files
//! unchanged. A failed write is rolled back to the previous end of file (best
//! effort), so a partial line is not left behind. After any failure the
//! model is re-synchronised with the sizes on disk.
//!
//! The `esr_app::ports::LogWriter` implementation that wraps this type is
//! written in esr-app (`esr-win` does not depend on `esr-app`).
//!
//! Requirements 13.4, 13.6, 13.7.

use std::fs::{self, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use esr_core::logfmt::{
    ACTIVITY_LOG_FILE_NAME, AppendOutcome, LINE_TERMINATOR, LogRotationModel,
    PREVIOUS_LOG_FILE_NAME, entry_len,
};

use crate::file_store::ensure_data_dir;

/// Appends Activity_Log entries in one data directory.
#[derive(Debug)]
pub struct LogSink {
    dir: PathBuf,
    model: LogRotationModel,
}

impl LogSink {
    /// Open the log in `dir`, reading the current file sizes. Never fails and
    /// creates nothing: the directory is created on the first append if it
    /// is missing.
    pub fn open(dir: impl Into<PathBuf>) -> Self {
        let mut sink = Self { dir: dir.into(), model: LogRotationModel::new() };
        sink.resync();
        sink
    }

    /// The data directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Path of the Activity_Log.
    pub fn activity_log_path(&self) -> PathBuf {
        self.dir.join(ACTIVITY_LOG_FILE_NAME)
    }

    /// Path of the Previous_Log_File.
    pub fn previous_log_path(&self) -> PathBuf {
        self.dir.join(PREVIOUS_LOG_FILE_NAME)
    }

    /// Append one already formatted line (without the terminator). Rotates
    /// first when needed. Errors are swallowed and the entry is discarded
    /// (Req 13.7).
    ///
    /// Formatted entries never contain CR or LF (values are escaped); any that
    /// slip through are replaced by spaces so the entry stays a single line.
    pub fn append(&mut self, line: &str) {
        let line = if line.contains(['\r', '\n']) {
            std::borrow::Cow::Owned(line.replace(['\r', '\n'], " "))
        } else {
            std::borrow::Cow::Borrowed(line)
        };
        let plan = self.model.plan_append(entry_len(&line));

        if !self.dir.is_dir() {
            // Deleted while running (or first use): re-create with the DACL.
            // A failure surfaces as the write error below.
            let _ = ensure_data_dir(&self.dir);
        }

        if plan.rotate_first
            && fs::rename(self.activity_log_path(), self.previous_log_path()).is_err()
        {
            // Both files unchanged; the next entry retries the rotation.
            self.model.apply(plan, AppendOutcome::Failed);
            self.resync();
            return;
        }

        match self.write_line(&line) {
            Ok(()) => self.model.apply(plan, AppendOutcome::Written),
            Err(_) => {
                let outcome = if plan.rotate_first {
                    AppendOutcome::WriteFailedAfterRotation
                } else {
                    AppendOutcome::Failed
                };
                self.model.apply(plan, outcome);
                self.resync();
            }
        }
    }

    /// Write `line` + terminator at the end of the Activity_Log in one call,
    /// truncating back to the previous end if the write fails part-way.
    fn write_line(&self, line: &str) -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.activity_log_path())?;
        let start = file.seek(SeekFrom::End(0))?;
        let mut buf = String::with_capacity(line.len() + LINE_TERMINATOR.len());
        buf.push_str(line);
        buf.push_str(LINE_TERMINATOR);
        if let Err(e) = file.write_all(buf.as_bytes()) {
            let _ = file.set_len(start);
            return Err(e);
        }
        Ok(())
    }

    /// Replace the model's state with the sizes on disk.
    fn resync(&mut self) {
        let current = fs::metadata(self.activity_log_path()).ok().map(|m| m.len());
        let has_previous = self.previous_log_path().is_file();
        self.model.resync(current, has_previous);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use esr_core::logfmt::MAX_LOG_BYTES;
    use std::os::windows::fs::OpenOptionsExt;

    /// `%TEMP%\esr_test_logsink_<pid>\<name>`, removed on drop.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("esr_test_logsink_{}", std::process::id()))
                .join(name);
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        /// The sink's data directory (not created yet).
        fn data(&self) -> PathBuf {
            self.0.join("data")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
            if let Some(parent) = self.0.parent() {
                let _ = fs::remove_dir(parent);
            }
        }
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap()
    }

    /// A file of `len` bytes made of `x` lines, each ending in LF.
    fn filler(len: u64) -> String {
        let mut s = "x".repeat(len as usize - 1);
        s.push('\n');
        s
    }

    /// Open `path` with no sharing, so every other open (and rename) fails.
    fn lock(path: &Path) -> fs::File {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .share_mode(0)
            .open(path)
            .unwrap()
    }

    #[test]
    fn appends_lines_with_lf_and_creates_directory() {
        let dir = TestDir::new("append");
        let mut sink = LogSink::open(dir.data());
        assert!(!dir.data().exists(), "open creates nothing");

        sink.append("2025-01-31 09:15:02.123 +01:00 SAVE windows=1 tabs=2");
        sink.append("2025-01-31 09:15:03.000 +01:00 RESTORE windows=1 tabs=2");

        assert!(dir.data().is_dir());
        assert_eq!(
            read(&sink.activity_log_path()),
            "2025-01-31 09:15:02.123 +01:00 SAVE windows=1 tabs=2\n\
             2025-01-31 09:15:03.000 +01:00 RESTORE windows=1 tabs=2\n"
        );
        assert!(!sink.previous_log_path().exists());
    }

    #[test]
    fn line_breaks_in_input_never_split_an_entry() {
        let dir = TestDir::new("newline");
        let mut sink = LogSink::open(dir.data());
        sink.append("a\r\nb\nc");
        assert_eq!(read(&sink.activity_log_path()), "a  b c\n");
    }

    #[test]
    fn reopening_continues_the_existing_log() {
        let dir = TestDir::new("reopen");
        LogSink::open(dir.data()).append("one");
        LogSink::open(dir.data()).append("two");
        assert_eq!(read(&dir.data().join(ACTIVITY_LOG_FILE_NAME)), "one\ntwo\n");
    }

    #[test]
    fn filling_exactly_to_the_cap_does_not_rotate() {
        let dir = TestDir::new("exact");
        fs::create_dir_all(dir.data()).unwrap();
        let old = filler(MAX_LOG_BYTES - 4);
        fs::write(dir.data().join(ACTIVITY_LOG_FILE_NAME), &old).unwrap();

        let mut sink = LogSink::open(dir.data());
        sink.append("abc"); // 4 bytes: exactly MAX_LOG_BYTES.

        let current = read(&sink.activity_log_path());
        assert_eq!(current.len() as u64, MAX_LOG_BYTES);
        assert!(current.ends_with("\nabc\n"));
        assert!(!sink.previous_log_path().exists());
    }

    #[test]
    fn exceeding_the_cap_rotates_and_replaces_previous() {
        let dir = TestDir::new("rotate");
        fs::create_dir_all(dir.data()).unwrap();
        let old = filler(MAX_LOG_BYTES - 4);
        fs::write(dir.data().join(ACTIVITY_LOG_FILE_NAME), &old).unwrap();
        fs::write(dir.data().join(PREVIOUS_LOG_FILE_NAME), "older\n").unwrap();

        let mut sink = LogSink::open(dir.data());
        sink.append("abcd"); // 5 bytes: one more than fits.

        assert_eq!(read(&sink.previous_log_path()), old);
        assert_eq!(read(&sink.activity_log_path()), "abcd\n");
        let names: Vec<_> = fs::read_dir(dir.data()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names.len(), 2, "at most two log files: {names:?}");
    }

    #[test]
    fn oversized_entry_is_written_whole_and_rotated_away_next() {
        let dir = TestDir::new("oversized");
        let mut sink = LogSink::open(dir.data());
        sink.append("first");
        let big = "y".repeat(MAX_LOG_BYTES as usize + 10);
        sink.append(&big);

        assert_eq!(read(&sink.previous_log_path()), "first\n");
        assert_eq!(read(&sink.activity_log_path()).len(), big.len() + 1);

        sink.append("next");
        assert_eq!(read(&sink.previous_log_path()).len(), big.len() + 1);
        assert_eq!(read(&sink.activity_log_path()), "next\n");
    }

    #[test]
    fn write_error_discards_entry_and_next_entry_is_written() {
        let dir = TestDir::new("write_error");
        let mut sink = LogSink::open(dir.data());
        sink.append("before");

        let guard = lock(&sink.activity_log_path());
        sink.append("discarded"); // Sharing violation: swallowed.
        drop(guard);

        sink.append("after");
        assert_eq!(read(&sink.activity_log_path()), "before\nafter\n");
    }

    #[test]
    fn failed_rotation_discards_entry_and_retries_on_next_entry() {
        let dir = TestDir::new("rotate_error");
        fs::create_dir_all(dir.data()).unwrap();
        let old = filler(MAX_LOG_BYTES);
        fs::write(dir.data().join(ACTIVITY_LOG_FILE_NAME), &old).unwrap();
        fs::write(dir.data().join(PREVIOUS_LOG_FILE_NAME), "older\n").unwrap();
        let mut sink = LogSink::open(dir.data());

        let guard = lock(&sink.activity_log_path());
        sink.append("discarded"); // Rename fails: both files unchanged.
        drop(guard);
        assert_eq!(read(&sink.previous_log_path()), "older\n");
        assert_eq!(read(&sink.activity_log_path()), old);

        sink.append("after");
        assert_eq!(read(&sink.previous_log_path()), old);
        assert_eq!(read(&sink.activity_log_path()), "after\n");
    }

    #[test]
    fn unusable_directory_is_swallowed() {
        let dir = TestDir::new("bad_dir");
        // The data "directory" is a regular file: nothing can be created.
        fs::write(dir.data(), "not a directory").unwrap();
        let mut sink = LogSink::open(dir.data());
        sink.append("discarded");
        sink.append("discarded too");
        assert_eq!(read(&dir.data()), "not a directory");
    }

    #[test]
    fn directory_deleted_while_running_is_recreated() {
        let dir = TestDir::new("recreate");
        let mut sink = LogSink::open(dir.data());
        sink.append("one");
        fs::remove_dir_all(dir.data()).unwrap();
        sink.append("two");
        assert_eq!(read(&sink.activity_log_path()), "two\n");
    }
}
