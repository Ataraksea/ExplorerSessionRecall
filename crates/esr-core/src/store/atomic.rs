//! The `FsOps` trait and the generic `atomic_replace` algorithm (Req 3.9).
//!
//! # Sequence
//!
//! [`atomic_replace`] replaces the content of a target file (for example
//! `session.esr`) without ever opening the target for writing:
//!
//! 1. [`Step::ClearTemp`]: remove a leftover temp file (`session.esr.tmp`) if one
//!    exists.
//! 2. [`Step::WriteTemp`]: create the temp file and write the complete new
//!    content to it.
//! 3. [`Step::Flush`]: flush the temp file to the disk (`FlushFileBuffers`) and
//!    close it.
//! 4. [`Step::Replace`]: swap the temp file onto the target (`ReplaceFileW`).
//!    If the target does not exist, [`FsOps::replace_existing`] reports
//!    [`IoErrorKind::NotFound`] and the algorithm falls back to
//!    [`Step::Move`]: `MoveFileExW(MOVEFILE_REPLACE_EXISTING |
//!    MOVEFILE_WRITE_THROUGH)`.
//!
//! If any step after step 1 fails, the temp file is removed (best effort) and
//! the error is returned. The target is left as it was.
//!
//! # Invariant
//!
//! At every point of the sequence, including after a failure or a crash
//! (process killed or power lost) at any step, the target path holds either
//! its complete previous content (or is absent, if it was absent before) or
//! the complete new content. It never holds a partial or mixed file. Only the
//! temp path is ever written, and the target changes only through the single
//! rename-based commit operation in step 4, which the file system performs
//! atomically. A crash may leave a temp file, possibly partial, behind. The
//! next start removes it with [`cleanup_leftover_temp`] before the target is
//! read, and the next write removes it in step 1. A leftover temp file is
//! never read as session content.
//!
//! Preserving an unreadable Session_File as the single backup copy
//! (Req 11.7, `Command::PreserveBackup`) uses the same sequence with
//! [`BACKUP_FILE`] as the target, so a new backup replaces any previous one
//! with the same guarantee and never touches the Session_File.
//!
//! The trait is generic and object-free. Task 13.1 implements it with Win32
//! calls in `esr-win`, and the property tests use an in-memory fake that
//! injects a failure or a crash at every step.

use crate::engine::IoErrorKind;

/// A target file and the temp file used to replace it, as names relative to
/// the data directory (`%LOCALAPPDATA%\ExplorerSessionRecall`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AtomicFile {
    /// The visible file that readers open.
    pub target: &'static str,
    /// The temp file the new content is written to before the commit.
    pub temp: &'static str,
}

impl AtomicFile {
    pub const fn new(target: &'static str, temp: &'static str) -> Self {
        Self { target, temp }
    }
}

/// The Session_File (Req 3.9).
pub const SESSION_FILE: AtomicFile = AtomicFile::new("session.esr", "session.esr.tmp");

/// The single backup copy of an unreadable Session_File (Req 11.7).
pub const BACKUP_FILE: AtomicFile =
    AtomicFile::new("session.esr.unreadable", "session.esr.unreadable.tmp");

/// Every file written with [`atomic_replace`], for startup cleanup.
pub const ATOMIC_FILES: [AtomicFile; 2] = [SESSION_FILE, BACKUP_FILE];

/// The primitive file operations [`atomic_replace`] is built from.
///
/// Names are relative to the data directory. Every method reports failure as
/// an [`IoErrorKind`]. None of these methods may write to an existing file in
/// place except [`FsOps::write_temp`], and [`atomic_replace`] only calls that
/// method with a temp name.
pub trait FsOps {
    /// An open, written temp file that has not been flushed yet.
    type Temp;

    /// Remove `name` if it exists. An absent file is success.
    fn remove_if_exists(&mut self, name: &str) -> Result<(), IoErrorKind>;

    /// Create `name` (truncating any existing file) and write all of
    /// `content` to it. On failure the implementation closes any handle it
    /// opened. The file may then be absent or partially written.
    fn write_temp(&mut self, name: &str, content: &[u8]) -> Result<Self::Temp, IoErrorKind>;

    /// Flush the temp file to the disk (`FlushFileBuffers`) and close it.
    /// The handle is consumed and closed whether or not the flush succeeds.
    fn flush(&mut self, temp: Self::Temp) -> Result<(), IoErrorKind>;

    /// Atomically replace the existing file `target` with `temp`
    /// (`ReplaceFileW` without a backup name). On success `temp` no longer
    /// exists. Returns [`IoErrorKind::NotFound`] without changing anything
    /// when `target` does not exist. On any failure `target` keeps its
    /// previous content.
    fn replace_existing(&mut self, temp: &str, target: &str) -> Result<(), IoErrorKind>;

    /// Rename `temp` to `target`, replacing `target` if it exists, and return
    /// only after the rename is durable (`MoveFileExW` with
    /// `MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH`). On failure
    /// `target` is unchanged.
    fn move_into_place(&mut self, temp: &str, target: &str) -> Result<(), IoErrorKind>;
}

/// A step of the [`atomic_replace`] sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Step {
    ClearTemp,
    WriteTemp,
    Flush,
    Replace,
    Move,
}

/// The step that failed and why. The target is unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AtomicWriteError {
    pub step: Step,
    pub kind: IoErrorKind,
}

impl AtomicWriteError {
    fn at(step: Step) -> impl FnOnce(IoErrorKind) -> Self {
        move |kind| Self { step, kind }
    }
}

/// Replace the content of `file.target` with `content` without writing the
/// target in place. See the module documentation for the sequence and the
/// invariant.
///
/// On error the target holds its previous content and the temp file has been
/// removed, unless that removal also failed (the next start or write removes
/// it then).
pub fn atomic_replace<F: FsOps>(
    fs: &mut F,
    file: &AtomicFile,
    content: &[u8],
) -> Result<(), AtomicWriteError> {
    fs.remove_if_exists(file.temp).map_err(AtomicWriteError::at(Step::ClearTemp))?;
    let result = write_and_commit(fs, file, content);
    if result.is_err() {
        // Best effort: the target is intact either way, and a temp file that
        // cannot be removed now is removed at the next start or write.
        let _ = fs.remove_if_exists(file.temp);
    }
    result
}

fn write_and_commit<F: FsOps>(
    fs: &mut F,
    file: &AtomicFile,
    content: &[u8],
) -> Result<(), AtomicWriteError> {
    let temp = fs.write_temp(file.temp, content).map_err(AtomicWriteError::at(Step::WriteTemp))?;
    fs.flush(temp).map_err(AtomicWriteError::at(Step::Flush))?;
    match fs.replace_existing(file.temp, file.target) {
        Ok(()) => Ok(()),
        Err(IoErrorKind::NotFound) => {
            fs.move_into_place(file.temp, file.target).map_err(AtomicWriteError::at(Step::Move))
        }
        Err(kind) => Err(AtomicWriteError { step: Step::Replace, kind }),
    }
}

/// Preserve unreadable Session_File content as the single backup copy,
/// replacing any previous backup (Req 11.7). The Session_File is not touched.
pub fn preserve_backup<F: FsOps>(fs: &mut F, content: &[u8]) -> Result<(), AtomicWriteError> {
    atomic_replace(fs, &BACKUP_FILE, content)
}

/// Remove a temp file left behind by an interrupted write. Called at startup
/// before the target is read.
pub fn cleanup_leftover_temp<F: FsOps>(fs: &mut F, file: &AtomicFile) -> Result<(), IoErrorKind> {
    fs.remove_if_exists(file.temp)
}

/// [`cleanup_leftover_temp`] for every file in [`ATOMIC_FILES`]. Every file is
/// attempted; the first error, if any, is returned.
pub fn cleanup_all_leftover_temps<F: FsOps>(fs: &mut F) -> Result<(), IoErrorKind> {
    let mut first_error = None;
    for file in &ATOMIC_FILES {
        if let Err(e) = cleanup_leftover_temp(fs, file) {
            first_error.get_or_insert(e);
        }
    }
    first_error.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// What the fake does when an injected fault fires.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Fault {
        /// The operation returns this error (and, for `WriteTemp`, leaves a
        /// partial temp file).
        Fail(IoErrorKind),
        /// The process dies before the operation takes effect: this and every
        /// later operation does nothing and reports an error. Unflushed temp
        /// content is lost at the next "start" (see `restart`).
        Crash,
    }

    /// One recorded call, for checking what the algorithm touched.
    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Op {
        Remove(String),
        WriteTemp(String),
        Flush(String),
        Replace { temp: String, target: String },
        Move { temp: String, target: String },
    }

    /// In-memory file system with one optional fault.
    #[derive(Default)]
    struct MemFs {
        files: BTreeMap<String, Vec<u8>>,
        fault: Option<(Step, Fault)>,
        crashed: bool,
        ops: Vec<Op>,
    }

    struct MemTemp(String);

    impl MemFs {
        fn with(files: &[(&str, &[u8])]) -> Self {
            Self {
                files: files.iter().map(|(n, c)| (n.to_string(), c.to_vec())).collect(),
                ..Self::default()
            }
        }

        fn inject(mut self, step: Step, fault: Fault) -> Self {
            self.fault = Some((step, fault));
            self
        }

        /// `Some(err)` if the operation for `step` must not take effect.
        fn check(&mut self, step: Step) -> Option<IoErrorKind> {
            if self.crashed {
                return Some(IoErrorKind::Other(0));
            }
            match self.fault {
                Some((s, Fault::Fail(kind))) if s == step => Some(kind),
                Some((s, Fault::Crash)) if s == step => {
                    self.crashed = true;
                    Some(IoErrorKind::Other(0))
                }
                _ => None,
            }
        }

        fn get(&self, name: &str) -> Option<&[u8]> {
            self.files.get(name).map(Vec::as_slice)
        }

        /// The next process start after a crash.
        fn restart(&mut self) {
            self.crashed = false;
            self.fault = None;
        }
    }

    impl FsOps for MemFs {
        type Temp = MemTemp;

        fn remove_if_exists(&mut self, name: &str) -> Result<(), IoErrorKind> {
            self.ops.push(Op::Remove(name.into()));
            if let Some(e) = self.check(Step::ClearTemp) {
                return Err(e);
            }
            self.files.remove(name);
            Ok(())
        }

        fn write_temp(&mut self, name: &str, content: &[u8]) -> Result<MemTemp, IoErrorKind> {
            self.ops.push(Op::WriteTemp(name.into()));
            if let Some(e) = self.check(Step::WriteTemp) {
                if !self.crashed {
                    // A failed write leaves a partial file behind.
                    self.files.insert(name.into(), content[..content.len() / 2].to_vec());
                }
                return Err(e);
            }
            self.files.insert(name.into(), content.to_vec());
            Ok(MemTemp(name.into()))
        }

        fn flush(&mut self, temp: MemTemp) -> Result<(), IoErrorKind> {
            self.ops.push(Op::Flush(temp.0));
            self.check(Step::Flush).map_or(Ok(()), Err)
        }

        fn replace_existing(&mut self, temp: &str, target: &str) -> Result<(), IoErrorKind> {
            self.ops.push(Op::Replace { temp: temp.into(), target: target.into() });
            if let Some(e) = self.check(Step::Replace) {
                return Err(e);
            }
            if !self.files.contains_key(target) {
                return Err(IoErrorKind::NotFound);
            }
            let content = self.files.remove(temp).ok_or(IoErrorKind::NotFound)?;
            self.files.insert(target.into(), content);
            Ok(())
        }

        fn move_into_place(&mut self, temp: &str, target: &str) -> Result<(), IoErrorKind> {
            self.ops.push(Op::Move { temp: temp.into(), target: target.into() });
            if let Some(e) = self.check(Step::Move) {
                return Err(e);
            }
            let content = self.files.remove(temp).ok_or(IoErrorKind::NotFound)?;
            self.files.insert(target.into(), content);
            Ok(())
        }
    }

    const OLD: &[u8] = b"old complete content";
    const NEW: &[u8] = b"new complete content";
    const ALL_STEPS: [Step; 5] =
        [Step::ClearTemp, Step::WriteTemp, Step::Flush, Step::Replace, Step::Move];

    #[test]
    fn replaces_existing_target_via_replace() {
        let mut fs = MemFs::with(&[("session.esr", OLD)]);
        atomic_replace(&mut fs, &SESSION_FILE, NEW).unwrap();
        assert_eq!(fs.get("session.esr"), Some(NEW));
        assert_eq!(fs.get("session.esr.tmp"), None);
        assert!(!fs.ops.iter().any(|op| matches!(op, Op::Move { .. })));
    }

    #[test]
    fn creates_absent_target_via_move() {
        let mut fs = MemFs::default();
        atomic_replace(&mut fs, &SESSION_FILE, NEW).unwrap();
        assert_eq!(fs.get("session.esr"), Some(NEW));
        assert_eq!(fs.get("session.esr.tmp"), None);
        assert!(fs.ops.contains(&Op::Move {
            temp: "session.esr.tmp".into(),
            target: "session.esr".into()
        }));
    }

    #[test]
    fn never_writes_the_target_in_place() {
        let mut fs = MemFs::with(&[("session.esr", OLD)]);
        atomic_replace(&mut fs, &SESSION_FILE, NEW).unwrap();
        let mut fs2 = MemFs::default();
        atomic_replace(&mut fs2, &SESSION_FILE, NEW).unwrap();
        for op in fs.ops.iter().chain(&fs2.ops) {
            if let Op::WriteTemp(name) = op {
                assert_eq!(name, "session.esr.tmp");
            }
        }
    }

    #[test]
    fn removes_leftover_temp_before_writing() {
        let mut fs = MemFs::with(&[("session.esr", OLD), ("session.esr.tmp", b"stale")]);
        atomic_replace(&mut fs, &SESSION_FILE, NEW).unwrap();
        assert_eq!(fs.ops.first(), Some(&Op::Remove("session.esr.tmp".into())));
        assert_eq!(fs.get("session.esr"), Some(NEW));
    }

    #[test]
    fn failure_at_any_step_keeps_old_target_and_removes_temp() {
        for target_exists in [true, false] {
            for step in ALL_STEPS {
                let initial: &[(&str, &[u8])] =
                    if target_exists { &[("session.esr", OLD)] } else { &[] };
                let mut fs = MemFs::with(initial).inject(step, Fault::Fail(IoErrorKind::DiskFull));
                let result = atomic_replace(&mut fs, &SESSION_FILE, NEW);
                // Move is only reached when the target is absent.
                let reached = step != Step::Move || !target_exists;
                if reached {
                    assert_eq!(
                        result,
                        Err(AtomicWriteError { step, kind: IoErrorKind::DiskFull }),
                        "step {step:?}, target_exists {target_exists}"
                    );
                    assert_eq!(fs.get("session.esr"), target_exists.then_some(OLD));
                } else {
                    assert_eq!(result, Ok(()));
                    assert_eq!(fs.get("session.esr"), Some(NEW));
                }
                // ClearTemp failing also fails the cleanup, but there was no temp.
                assert_eq!(fs.get("session.esr.tmp"), None, "step {step:?}");
            }
        }
    }

    #[test]
    fn crash_at_any_step_leaves_old_or_new_and_restart_cleans_temp() {
        for target_exists in [true, false] {
            for step in ALL_STEPS {
                let initial: &[(&str, &[u8])] =
                    if target_exists { &[("session.esr", OLD)] } else { &[] };
                let mut fs = MemFs::with(initial).inject(step, Fault::Crash);
                let _ = atomic_replace(&mut fs, &SESSION_FILE, NEW);
                let visible = fs.get("session.esr").map(<[u8]>::to_vec);
                let old = target_exists.then(|| OLD.to_vec());
                assert!(visible == old || visible.as_deref() == Some(NEW), "step {step:?}");

                fs.restart();
                cleanup_all_leftover_temps(&mut fs).unwrap();
                assert_eq!(fs.get("session.esr.tmp"), None);
                assert_eq!(fs.get("session.esr"), visible.as_deref());
            }
        }
    }

    #[test]
    fn preserve_backup_replaces_previous_backup_and_leaves_session_file() {
        let mut fs = MemFs::with(&[("session.esr", b"garbage"), ("session.esr.unreadable", OLD)]);
        preserve_backup(&mut fs, b"garbage").unwrap();
        assert_eq!(fs.get("session.esr.unreadable"), Some(&b"garbage"[..]));
        assert_eq!(fs.get("session.esr"), Some(&b"garbage"[..]));
        assert_eq!(fs.get("session.esr.unreadable.tmp"), None);
    }

    #[test]
    fn cleanup_attempts_every_file_and_reports_first_error() {
        let mut fs = MemFs::with(&[("session.esr.tmp", b"x"), ("session.esr.unreadable.tmp", b"y")]);
        cleanup_all_leftover_temps(&mut fs).unwrap();
        assert!(fs.files.is_empty());

        let mut fs = MemFs::with(&[("session.esr.tmp", b"x")])
            .inject(Step::ClearTemp, Fault::Fail(IoErrorKind::AccessDenied));
        assert_eq!(cleanup_all_leftover_temps(&mut fs), Err(IoErrorKind::AccessDenied));
        assert_eq!(
            fs.ops,
            vec![Op::Remove("session.esr.tmp".into()), Op::Remove("session.esr.unreadable.tmp".into())]
        );
    }
}
