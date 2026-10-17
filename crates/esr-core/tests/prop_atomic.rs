//! Property 9: Atomic replacement survives interruption.
//!
//! *For any* previously durable Session_File content and *any* failure
//! injected at any step of the write sequence (temp write, flush, replace) in
//! the fake file system, the visible Session_File afterwards is either the
//! complete previous content or the complete new content, and it parses
//! without error.
//!
//! `atomic_replace` (`esr_core::store::atomic`) is driven against an in-memory
//! fake `FsOps` over random sequences of writes to the Session_File and, via
//! `preserve_backup`, to the backup copy. Each write gets random contents
//! (serialized valid session documents or arbitrary bytes), the target may or
//! may not exist, stale temp files may be present, and any call may fail with
//! a random `IoErrorKind` or crash (the process dies: the state freezes, with
//! a random prefix of a temp write on disk). Checked:
//!
//! 1. after every single call, the target holds the complete old content (or
//!    is absent if it was) or the complete new content, nothing else;
//! 2. `Ok` means the target holds exactly the new content and no temp remains;
//! 3. `Err` means the target is unchanged, the error names the failing step
//!    and its kind, and the temp is removed unless that removal itself failed;
//! 4. after a crash, `cleanup_all_leftover_temps` at the next start leaves no
//!    temp files and the target holds the old or the new content;
//! 5. only temp names are ever written or removed; the target changes only
//!    through replace or move, and a write never touches the other file;
//! 6. a sequence of successful writes ends with the last content of each file.
//!
//! "Parses without error": the visible content is always byte-identical to a
//! complete content that was handed to `atomic_replace`, and the session
//! contents are `serialize`d valid documents, so it is exactly what the
//! Snapshot_Parser receives for a completely written file; after every write
//! each visible serialized document is also run through `parse`.
//!
//! **Validates: Requirements 3.9**

mod common;

use std::collections::BTreeMap;

use common::strategies::session_doc;
use esr_core::engine::IoErrorKind;
use esr_core::format::parse::parse;
use esr_core::format::serialize::serialize;
use esr_core::store::atomic::{
    atomic_replace, cleanup_all_leftover_temps, preserve_backup, AtomicFile, AtomicWriteError,
    Step, ATOMIC_FILES, BACKUP_FILE, SESSION_FILE,
};
use esr_core::store::atomic::FsOps;
use proptest::collection::vec;
use proptest::option;
use proptest::prelude::*;
use proptest::sample::Index;

type Files = BTreeMap<String, Vec<u8>>;

/// The error every call returns once the process has "died".
const DEAD: IoErrorKind = IoErrorKind::Other(0xDEAD);

/// The most calls one `atomic_replace` makes (clear, write, flush, replace,
/// move, cleanup), plus one so some plans never fire.
const MAX_CALLS: usize = 7;

// ---------------------------------------------------------------------------
// Fake file system
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    Remove(String),
    WriteTemp(String),
    Flush(String),
    Replace { temp: String, target: String },
    Move { temp: String, target: String },
}

impl Call {
    fn step(&self) -> Step {
        match self {
            Call::Remove(_) => Step::ClearTemp,
            Call::WriteTemp(_) => Step::WriteTemp,
            Call::Flush(_) => Step::Flush,
            Call::Replace { .. } => Step::Replace,
            Call::Move { .. } => Step::Move,
        }
    }

    /// Names this call may change.
    fn may_change(&self) -> Vec<&str> {
        match self {
            Call::Remove(n) | Call::WriteTemp(n) => vec![n],
            Call::Flush(_) => vec![],
            Call::Replace { temp, target } | Call::Move { temp, target } => vec![temp, target],
        }
    }
}

/// One call, its result, and the whole file system right after it.
#[derive(Clone, Debug)]
struct Record {
    call: Call,
    result: Result<(), IoErrorKind>,
    after: Files,
}

#[derive(Clone, Copy, Debug)]
struct Crash {
    /// Call index (within one plan) at which the process dies.
    at: usize,
    /// For remove / replace / move: whether the effect lands before the death.
    after_effect: bool,
}

/// Faults for the calls of one operation, by call index.
#[derive(Clone, Debug, Default)]
struct Plan {
    fail: Vec<Option<IoErrorKind>>,
    crash: Option<Crash>,
    /// Length of the prefix a failed or crashed temp write leaves on disk.
    partial: Option<Index>,
}

enum Action {
    Normal,
    Fail(IoErrorKind),
    Crash { after_effect: bool },
    Dead,
}

struct FakeTemp(String);

#[derive(Default)]
struct FakeFs {
    files: Files,
    plan: Plan,
    calls: usize,
    crashed: bool,
    /// Index into `log` of the call at which the process died.
    crash_record: Option<usize>,
    log: Vec<Record>,
}

impl FakeFs {
    /// Start one operation with `plan`; clears the log.
    fn begin(&mut self, plan: Plan) {
        self.plan = plan;
        self.calls = 0;
        self.log.clear();
        self.crash_record = None;
    }

    /// The next process start after a crash.
    fn restart(&mut self) {
        self.crashed = false;
        self.begin(Plan::default());
    }

    fn action(&mut self) -> Action {
        let i = self.calls;
        self.calls += 1;
        if self.crashed {
            return Action::Dead;
        }
        if let Some(c) = self.plan.crash.filter(|c| c.at == i) {
            return Action::Crash { after_effect: c.after_effect };
        }
        match self.plan.fail.get(i).copied().flatten() {
            Some(kind) => Action::Fail(kind),
            None => Action::Normal,
        }
    }

    fn record(&mut self, call: Call, result: Result<(), IoErrorKind>, crashed_now: bool) {
        if crashed_now {
            self.crashed = true;
            self.crash_record = Some(self.log.len());
        }
        self.log.push(Record { call, result, after: self.files.clone() });
    }

    fn prefix(&self, content: &[u8]) -> Vec<u8> {
        let n = self.plan.partial.map_or(content.len() / 2, |i| i.index(content.len() + 1));
        content[..n].to_vec()
    }

    fn rename(&mut self, temp: &str, target: &str) -> Result<(), IoErrorKind> {
        let content = self.files.remove(temp).ok_or(IoErrorKind::NotFound)?;
        self.files.insert(target.into(), content);
        Ok(())
    }
}

impl FsOps for FakeFs {
    type Temp = FakeTemp;

    fn remove_if_exists(&mut self, name: &str) -> Result<(), IoErrorKind> {
        let (result, crashed_now) = match self.action() {
            Action::Dead => (Err(DEAD), false),
            Action::Fail(k) => (Err(k), false),
            Action::Crash { after_effect } => {
                if after_effect {
                    self.files.remove(name);
                }
                (Err(DEAD), true)
            }
            Action::Normal => {
                self.files.remove(name);
                (Ok(()), false)
            }
        };
        self.record(Call::Remove(name.into()), result, crashed_now);
        result
    }

    fn write_temp(&mut self, name: &str, content: &[u8]) -> Result<FakeTemp, IoErrorKind> {
        let (result, crashed_now) = match self.action() {
            Action::Dead => (Err(DEAD), false),
            Action::Fail(k) => {
                let partial = self.prefix(content);
                self.files.insert(name.into(), partial);
                (Err(k), false)
            }
            Action::Crash { .. } => {
                let partial = self.prefix(content);
                self.files.insert(name.into(), partial);
                (Err(DEAD), true)
            }
            Action::Normal => {
                self.files.insert(name.into(), content.to_vec());
                (Ok(()), false)
            }
        };
        self.record(Call::WriteTemp(name.into()), result, crashed_now);
        result.map(|()| FakeTemp(name.into()))
    }

    fn flush(&mut self, temp: FakeTemp) -> Result<(), IoErrorKind> {
        let (result, crashed_now) = match self.action() {
            Action::Dead => (Err(DEAD), false),
            Action::Fail(k) => (Err(k), false),
            Action::Crash { .. } => (Err(DEAD), true),
            Action::Normal => (Ok(()), false),
        };
        self.record(Call::Flush(temp.0), result, crashed_now);
        result
    }

    fn replace_existing(&mut self, temp: &str, target: &str) -> Result<(), IoErrorKind> {
        let (result, crashed_now) = match self.action() {
            Action::Dead => (Err(DEAD), false),
            Action::Fail(k) => (Err(k), false),
            Action::Crash { after_effect } => {
                if after_effect && self.files.contains_key(target) {
                    let _ = self.rename(temp, target);
                }
                (Err(DEAD), true)
            }
            Action::Normal if !self.files.contains_key(target) => (Err(IoErrorKind::NotFound), false),
            Action::Normal => (self.rename(temp, target), false),
        };
        let call = Call::Replace { temp: temp.into(), target: target.into() };
        self.record(call, result, crashed_now);
        result
    }

    fn move_into_place(&mut self, temp: &str, target: &str) -> Result<(), IoErrorKind> {
        let (result, crashed_now) = match self.action() {
            Action::Dead => (Err(DEAD), false),
            Action::Fail(k) => (Err(k), false),
            Action::Crash { after_effect } => {
                if after_effect {
                    let _ = self.rename(temp, target);
                }
                (Err(DEAD), true)
            }
            Action::Normal => (self.rename(temp, target), false),
        };
        let call = Call::Move { temp: temp.into(), target: target.into() };
        self.record(call, result, crashed_now);
        result
    }
}

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct WriteOp {
    /// `false`: Session_File via `atomic_replace`; `true`: `preserve_backup`.
    backup: bool,
    content: Vec<u8>,
    plan: Plan,
}

#[derive(Clone, Debug)]
struct Initial {
    session: Option<Vec<u8>>,
    backup: Option<Vec<u8>>,
    session_temp: Option<Vec<u8>>,
    backup_temp: Option<Vec<u8>>,
}

fn kind() -> impl Strategy<Value = IoErrorKind> {
    prop_oneof![
        Just(IoErrorKind::NotFound),
        Just(IoErrorKind::AccessDenied),
        Just(IoErrorKind::SharingViolation),
        Just(IoErrorKind::DiskFull),
        any::<u32>().prop_map(IoErrorKind::Other),
    ]
}

fn content() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        2 => session_doc().prop_map(|d| serialize(&d).into_bytes()),
        1 => vec(any::<u8>(), 0..48),
    ]
}

fn plan() -> impl Strategy<Value = Plan> {
    let crash = (0..MAX_CALLS, any::<bool>()).prop_map(|(at, after_effect)| Crash { at, after_effect });
    prop_oneof![
        // No fault at all.
        1 => Just(Plan::default()),
        // Exactly one failure, at any call.
        2 => (0..MAX_CALLS, kind(), any::<Index>()).prop_map(|(at, k, partial)| {
            let mut fail = vec![None; MAX_CALLS];
            fail[at] = Some(k);
            Plan { fail, crash: None, partial: Some(partial) }
        }),
        // Exactly one crash, at any call.
        2 => (crash.clone(), any::<Index>())
            .prop_map(|(c, partial)| Plan { fail: vec![], crash: Some(c), partial: Some(partial) }),
        // Random failures anywhere, possibly with a crash.
        2 => (
            vec(option::weighted(0.3, kind()), MAX_CALLS),
            option::weighted(0.3, crash),
            any::<Index>(),
        )
            .prop_map(|(fail, crash, partial)| Plan { fail, crash, partial: Some(partial) }),
    ]
}

fn write_op() -> impl Strategy<Value = WriteOp> {
    (prop::bool::weighted(0.3), content(), plan())
        .prop_map(|(backup, content, plan)| WriteOp { backup, content, plan })
}

fn initial() -> impl Strategy<Value = Initial> {
    let stale = option::weighted(0.3, vec(any::<u8>(), 0..16));
    (option::of(content()), option::of(content()), stale.clone(), stale).prop_map(
        |(session, backup, session_temp, backup_temp)| Initial {
            session,
            backup,
            session_temp,
            backup_temp,
        },
    )
}

impl Initial {
    fn files(&self) -> Files {
        let mut files = Files::new();
        let entries = [
            (SESSION_FILE.target, &self.session),
            (BACKUP_FILE.target, &self.backup),
            (SESSION_FILE.temp, &self.session_temp),
            (BACKUP_FILE.temp, &self.backup_temp),
        ];
        for (name, content) in entries {
            if let Some(c) = content {
                files.insert(name.into(), c.clone());
            }
        }
        files
    }
}

// ---------------------------------------------------------------------------
// Checks
// ---------------------------------------------------------------------------

fn file_of(op: &WriteOp) -> (AtomicFile, AtomicFile) {
    if op.backup {
        (BACKUP_FILE, SESSION_FILE)
    } else {
        (SESSION_FILE, BACKUP_FILE)
    }
}

fn run(fs: &mut FakeFs, op: &WriteOp) -> Result<(), AtomicWriteError> {
    fs.begin(op.plan.clone());
    if op.backup {
        preserve_backup(fs, &op.content)
    } else {
        atomic_replace(fs, &SESSION_FILE, &op.content)
    }
}

/// Properties 1 and 5 over the call log of one write of `file`.
fn check_calls(
    before: &Files,
    log: &[Record],
    crash_record: Option<usize>,
    file: &AtomicFile,
    old: &Option<Vec<u8>>,
    new: &[u8],
) -> Result<(), TestCaseError> {
    let mut prev = before;
    for (i, rec) in log.iter().enumerate() {
        // (5) The write only names its own temp, and the target only as the
        // destination of replace / move.
        match &rec.call {
            Call::Remove(n) | Call::WriteTemp(n) | Call::Flush(n) => {
                prop_assert_eq!(n.as_str(), file.temp, "call {:?}", rec.call)
            }
            Call::Replace { temp, target } | Call::Move { temp, target } => {
                prop_assert_eq!(temp.as_str(), file.temp);
                prop_assert_eq!(target.as_str(), file.target);
            }
        }
        // (5) Nothing but the names the call may change changes.
        let allowed = rec.call.may_change();
        for name in prev.keys().chain(rec.after.keys()) {
            if prev.get(name) != rec.after.get(name) {
                prop_assert!(
                    allowed.contains(&name.as_str()),
                    "{:?} changed {} (allowed {:?})",
                    rec.call,
                    name,
                    allowed
                );
            }
        }
        // (1) Old or new, complete, after every call.
        let visible = rec.after.get(file.target);
        prop_assert!(
            visible == old.as_ref() || visible.map(Vec::as_slice) == Some(new),
            "after {:?} the target holds {:?}; old {:?}, new {:?}",
            rec.call,
            visible,
            old,
            new
        );
        // A crash freezes the state.
        if let Some(c) = crash_record
            && i > c
        {
            prop_assert_eq!(&rec.after, &log[c].after, "state changed after the crash");
            prop_assert_eq!(rec.result, Err(DEAD));
        }
        prev = &rec.after;
    }
    Ok(())
}

/// "Parses without error": a visible file holding a serialized document
/// (the arbitrary-byte contents essentially never start with the header) is
/// read by the Snapshot_Parser without error.
fn check_visible_parses(files: &Files) -> Result<(), TestCaseError> {
    for file in &ATOMIC_FILES {
        if let Some(content) = files.get(file.target)
            && content.starts_with(b"ESR-SESSION ")
        {
            let parsed = parse(content);
            prop_assert!(parsed.is_ok(), "{} does not parse: {:?}", file.target, parsed.err());
        }
    }
    Ok(())
}

/// Runs `ops` from `init`, checking properties 1–5 for every write. Returns
/// the expected final targets.
fn run_sequence(init: &Initial, ops: &[WriteOp]) -> Result<(), TestCaseError> {
    let mut fs = FakeFs { files: init.files(), ..FakeFs::default() };
    let mut model: BTreeMap<&str, Option<Vec<u8>>> = BTreeMap::new();
    model.insert(SESSION_FILE.target, init.session.clone());
    model.insert(BACKUP_FILE.target, init.backup.clone());

    for op in ops {
        let (file, other) = file_of(op);
        let before = fs.files.clone();
        let old = model[file.target].clone();
        prop_assert_eq!(before.get(file.target), old.as_ref());

        let result = run(&mut fs, op);
        check_calls(&before, &fs.log, fs.crash_record, &file, &old, &op.content)?;

        if fs.crashed {
            // (4) The next start removes leftover temps before reading.
            prop_assert!(result.is_err(), "Ok after a crash");
            let visible = fs.files.get(file.target).cloned();
            fs.restart();
            prop_assert_eq!(cleanup_all_leftover_temps(&mut fs), Ok(()));
            for f in &ATOMIC_FILES {
                prop_assert!(!fs.files.contains_key(f.temp), "{} left after restart", f.temp);
            }
            prop_assert_eq!(fs.files.get(file.target), visible.as_ref());
            prop_assert!(visible == old || visible.as_deref() == Some(&op.content[..]));
            model.insert(file.target, visible);
        } else {
            match result {
                // (2)
                Ok(()) => {
                    prop_assert_eq!(fs.files.get(file.target), Some(&op.content));
                    prop_assert!(!fs.files.contains_key(file.temp), "temp left after Ok");
                    model.insert(file.target, Some(op.content.clone()));
                }
                // (3)
                Err(e) => {
                    prop_assert_eq!(fs.files.get(file.target), old.as_ref());
                    let log = &fs.log;
                    let failed = if e.step == Step::ClearTemp {
                        prop_assert_eq!(log.len(), 1, "work after a failed clear: {:?}", log);
                        &log[0]
                    } else {
                        prop_assert!(log.len() >= 2);
                        let cleanup = &log[log.len() - 1];
                        prop_assert_eq!(&cleanup.call, &Call::Remove(file.temp.into()));
                        if cleanup.result.is_ok() {
                            prop_assert!(!fs.files.contains_key(file.temp), "temp left after Err");
                        }
                        &log[log.len() - 2]
                    };
                    prop_assert_eq!(failed.call.step(), e.step);
                    prop_assert_eq!(failed.result, Err(e.kind));
                }
            }
        }
        // (5) The other file is never touched.
        prop_assert_eq!(fs.files.get(other.target), model[other.target].as_ref());
        check_visible_parses(&fs.files)?;
    }

    // At the end, a clean start sees exactly the model.
    fs.restart();
    prop_assert_eq!(cleanup_all_leftover_temps(&mut fs), Ok(()));
    let expected: Files = model
        .iter()
        .filter_map(|(name, c)| c.as_ref().map(|c| (name.to_string(), c.clone())))
        .collect();
    prop_assert_eq!(&fs.files, &expected);
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// **Property 9: Atomic replacement survives interruption** (checks 1–5).
    /// Random sequences of Session_File writes and `preserve_backup` calls
    /// with failures and crashes injected at random calls.
    ///
    /// **Validates: Requirements 3.9**
    #[test]
    fn atomic_replace_survives_failures_and_crashes(
        init in initial(),
        ops in vec(write_op(), 1..8),
    ) {
        run_sequence(&init, &ops)?;
    }

    /// **Property 9: Atomic replacement survives interruption** (check 6).
    /// Without faults, each file ends with the last content written to it.
    ///
    /// **Validates: Requirements 3.9**
    #[test]
    fn successful_writes_end_with_last_content(
        init in initial(),
        writes in vec((prop::bool::weighted(0.3), content()), 1..8),
    ) {
        let mut fs = FakeFs { files: init.files(), ..FakeFs::default() };
        let mut last_session = init.session.clone();
        let mut last_backup = init.backup.clone();
        for (backup, content) in &writes {
            let op = WriteOp { backup: *backup, content: content.clone(), plan: Plan::default() };
            prop_assert_eq!(run(&mut fs, &op), Ok(()));
            if *backup {
                last_backup = Some(content.clone());
            } else {
                last_session = Some(content.clone());
            }
        }
        prop_assert_eq!(fs.files.get(SESSION_FILE.target), last_session.as_ref());
        prop_assert_eq!(fs.files.get(BACKUP_FILE.target), last_backup.as_ref());
        // A stale temp of a file that was never written may remain (startup
        // cleanup removes it); a written file never leaves one.
        for (file, written) in [(SESSION_FILE, false), (BACKUP_FILE, true)] {
            if writes.iter().any(|(b, _)| *b == written) {
                prop_assert!(!fs.files.contains_key(file.temp), "{} left", file.temp);
            }
        }
    }
}

/// Every step, failure and crash, for both an existing and an absent target,
/// once for the Session_File and once for the backup.
#[test]
fn every_single_fault_position() {
    let old = b"old complete content".to_vec();
    let new = b"new complete content".to_vec();
    for backup in [false, true] {
        for target_exists in [true, false] {
            for at in 0..MAX_CALLS {
                let mut fail = vec![None; MAX_CALLS];
                fail[at] = Some(IoErrorKind::DiskFull);
                let plans = [
                    Plan { fail, crash: None, partial: None },
                    Plan { fail: vec![], crash: Some(Crash { at, after_effect: false }), partial: None },
                    Plan { fail: vec![], crash: Some(Crash { at, after_effect: true }), partial: None },
                ];
                for plan in plans {
                    let init = Initial {
                        session: (target_exists && !backup).then(|| old.clone()),
                        backup: (target_exists && backup).then(|| old.clone()),
                        session_temp: Some(b"stale".to_vec()),
                        backup_temp: None,
                    };
                    let op = WriteOp { backup, content: new.clone(), plan: plan.clone() };
                    run_sequence(&init, &[op]).unwrap_or_else(|e| {
                        panic!("backup {backup}, exists {target_exists}, plan {plan:?}: {e}")
                    });
                }
            }
        }
    }
}
