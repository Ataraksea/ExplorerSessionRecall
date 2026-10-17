//! `StoreLogic`: the pure Session_Store state machine (task 8.3 base
//! behaviour; task 8.4 adds attempt counting, protection, consumption, and
//! abandonment on top of the hooks marked below).
//!
//! # Startup (Req 11.7, 11.10, 11.11, 13.1, 13.3)
//!
//! [`StoreLogic::startup`] takes the Session_File as read by the shell:
//!
//! - absent: no Pending_Snapshot, nothing logged;
//! - unreadable (storage or access failure): the file is left as it is, an
//!   `ERROR` entry is logged, and there is no Pending_Snapshot;
//! - malformed or from a newer format version: the content is preserved as
//!   the single backup copy (`Command::PreserveBackup`), an `ERROR` entry
//!   gives the line, column, and kind of the first problem (with the declared
//!   and newest versions for a newer version), and there is no
//!   Pending_Snapshot;
//! - a pending `checkpoint` file is what an abrupt end or power loss leaves
//!   behind (Req 3.8): it is the Pending_Snapshot, and a `SAVE` entry with the
//!   cause "abrupt end or power loss detected at the next start" is logged
//!   (not again for a protected retry snapshot, whose abrupt end was
//!   reported when it was first found);
//! - a Pending snapshot with [`MAX_FAILED_ATTEMPTS`] failed attempts (the
//!   third attempt ended with the process) is abandoned: marked Consumed and
//!   logged as `ABANDONED` (Req 4.11).
//!
//! # Checkpoints (Req 3.7, 3.8)
//!
//! While at least one Explorer window is open, no restoration is running
//! ([`StoreLogic::begin_restoration`]/[`StoreLogic::end_restoration`]), and
//! the stored snapshot is not protected, a Live_Session change schedules a
//! checkpoint [`CHECKPOINT_DELAY_MS`] later. Further changes before it fires
//! are coalesced into that one write, so a change is on disk after at most
//! 500 ms plus the write time. A checkpoint is a non-urgent `pending`
//! `checkpoint` write of the capture at the time the timer fires, with
//! `failed_attempts = 0`. An empty capture (windows open but no tab has a
//! Location yet) is not written, so it never replaces a stored snapshot.
//!
//! # Session_End (Req 3.1, 3.2, 3.4, 3.5, 3.6, 3.10, 13.1)
//!
//! [`StoreLogic::session_end`] cancels any scheduled checkpoint, writes the
//! snapshot urgently as the new Pending_Snapshot with `failed_attempts = 0`,
//! replacing whatever the file held, and logs one `SAVE` entry with the
//! cause and the window and tab counts. A protected retry snapshot is kept
//! instead on `ExplorerTerminated` (see below).
//!
//! # Attempts, protection, consumption, abandonment (Req 4.4, 4.6, 4.7, 4.10, 4.11)
//!
//! design.md "Failed-restoration protection". The restoration flow (task
//! 11.5) drives it:
//!
//! 1. [`StoreLogic::begin_restoration`] when a Trigger_Window candidate is
//!    first seen (holds checkpoints back while the trigger is decided);
//! 2. [`StoreLogic::abandon_if_exhausted`] (trigger step 6; normally a no-op,
//!    because [`StoreLogic::startup`] and [`StoreLogic::restoration_failed`]
//!    already abandon an exhausted snapshot), or
//!    [`StoreLogic::consume_pending`] (steps 3 and 5: Builtin_Restore_Option
//!    logon restore, restoration disabled);
//! 3. [`StoreLogic::begin_attempt`] before any window-affecting command: it
//!    increments `failed_attempts` and issues an urgent write of the
//!    otherwise unchanged Pending document (increment-before-attempt), whose
//!    `seq` goes into `RestoreParams::attempt_write_seq`. A process end
//!    during the restoration (Req 4.7) is then already counted;
//! 4. [`StoreLogic::restoration_completed`] marks the snapshot Consumed
//!    (Req 4.4), or [`StoreLogic::restoration_failed`] leaves the counted
//!    attempt (Req 4.10) and, at [`MAX_FAILED_ATTEMPTS`], marks it Consumed
//!    and logs `ABANDONED` (Req 4.11). Either ends the restoration.
//!
//! A Pending snapshot with `failed_attempts ≥ 1` is a protected retry
//! snapshot: checkpoints are suspended while no restoration runs, and an
//! `ExplorerTerminated` Session_End keeps it and its count (no write, no
//! `SAVE`). A user-caused Session_End (last window or tab closed, sign-out,
//! shutdown, restart, exit through Settings) replaces it with the
//! Live_Session and `failed_attempts = 0`. Abandonment consumes the snapshot,
//! so `ABANDONED` is logged once; normal checkpointing then resumes.
//!
//! # Writes and failures (Req 3.12, 13.3)
//!
//! Every write gets a fresh, increasing `seq`. The file store may run an
//! urgent write before an older queued one and then reports the older one as
//! `Ok` without writing it, so a completion only updates the durable view
//! when its `seq` is newer than the last committed one. When the newest
//! session write fails, the file keeps its previous content, an `ERROR`
//! entry is logged, and the same content is written again up to
//! [`MAX_RETRIES`] times at [`RETRY_INTERVAL_MS`] intervals. If every retry
//! fails, it is written again at the next Live_Session change. Issuing any
//! newer write supersedes a pending retry. The Live_Session stays in memory
//! throughout, and [`StoreLogic::pending_snapshot`] reflects the newest
//! issued content, so a failed Session_End write is still restored by the
//! next Trigger_Window.

use std::collections::BTreeMap;

use crate::engine::{Command, Event, IoErrorKind, Millis, SessionFileRead, TimerId, TimerOwner};
use crate::format::{parse, serialize, FORMAT_VERSION};
use crate::logfmt::LogEntry;
use crate::model::{SaveCause, SessionDoc, Snapshot, Status, MAX_FAILED_ATTEMPTS};
use crate::store::atomic::BACKUP_FILE;
use crate::store::clock::{local_time, WallClock};
use crate::tracker::LiveSessionTracker;

/// Delay from a Live_Session change to its checkpoint write (design.md
/// "Save policy").
pub const CHECKPOINT_DELAY_MS: u64 = 500;
/// Interval between retries of a failed write (Req 3.12).
pub const RETRY_INTERVAL_MS: u64 = 2_000;
/// Retries of a failed write before waiting for the next change (Req 3.12).
pub const MAX_RETRIES: u8 = 5;

/// `ERROR` entry operation names.
pub const OP_READ: &str = "read-session-file";
pub const OP_PARSE: &str = "parse-session-file";
pub const OP_WRITE: &str = "write-session-file";
pub const OP_BACKUP: &str = "preserve-backup";

/// What the store needs from the Live_Session.
pub trait LiveView {
    /// At least one Explorer window is tracked (open or orphaned).
    fn has_windows(&self) -> bool;
    /// The Live_Session as a snapshot (after caps).
    fn capture_snapshot(&self) -> Snapshot;
}

impl LiveView for LiveSessionTracker {
    fn has_windows(&self) -> bool {
        !self.is_empty()
    }

    fn capture_snapshot(&self) -> Snapshot {
        // CAP-EXCLUDED is logged by the tracker with each Session_End;
        // logging it for every checkpoint would flood the Activity_Log.
        self.capture().0
    }
}

/// What the Session_File holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileView {
    Absent,
    /// It exists but could not be read at startup (left unchanged, Req 11.11).
    Unreadable(IoErrorKind),
    /// Its content did not parse at startup (preserved as the backup, Req 11.7).
    Malformed,
    Doc(SessionDoc),
}

impl FileView {
    pub fn doc(&self) -> Option<&SessionDoc> {
        match self {
            FileView::Doc(d) => Some(d),
            _ => None,
        }
    }
}

/// A Session_File write and its urgency.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SessionJob {
    doc: SessionDoc,
    urgent: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum InFlight {
    /// `retries` is how many retries of the original write this one is.
    Session { job: SessionJob, retries: u8 },
    Backup,
}

/// A scheduled retry of a failed write.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Retry {
    job: SessionJob,
    /// Retry number of the write the timer issues (1..=MAX_RETRIES).
    retries: u8,
    timer: u64,
    at: Millis,
}

/// The Session_Store state machine. Pure: events in, commands out.
#[derive(Clone, Debug)]
pub struct StoreLogic {
    /// What the Session_File durably holds.
    durable: FileView,
    /// What it will hold once the newest issued write lands.
    desired: FileView,
    /// `seq` of the write `durable` came from (0: from startup).
    committed_seq: u64,
    /// `seq` of the newest issued session write (0: none).
    last_session_seq: u64,
    inflight: BTreeMap<u64, InFlight>,
    next_seq: u64,
    /// A restoration is running (Req 3.7, 4.6).
    restoring: bool,
    /// A change arrived while checkpoints were held back by a restoration.
    dirty: bool,
    /// An attempt was counted by `begin_attempt` and has not ended (or been
    /// replaced by a Session_End) yet.
    attempting: bool,
    /// Scheduled checkpoint: timer seq and due time.
    checkpoint: Option<(u64, Millis)>,
    retry: Option<Retry>,
    /// A write whose retries all failed; written again at the next change.
    exhausted: Option<SessionJob>,
    timer_seq: u64,
}

impl StoreLogic {
    /// Loads the Session_File as read at startup. Returns the startup
    /// commands (`AppendLog`, `PreserveBackup`, and the abandonment write).
    pub fn startup(read: &SessionFileRead) -> (Self, Vec<Command>) {
        let mut store = StoreLogic {
            durable: FileView::Absent,
            desired: FileView::Absent,
            committed_seq: 0,
            last_session_seq: 0,
            inflight: BTreeMap::new(),
            next_seq: 1,
            restoring: false,
            dirty: false,
            attempting: false,
            checkpoint: None,
            retry: None,
            exhausted: None,
            timer_seq: 0,
        };
        let mut out = Vec::new();
        let view = match read {
            SessionFileRead::Absent => FileView::Absent,
            SessionFileRead::Unreadable(kind) => {
                out.push(error(OP_READ, format!("{}; the file was left unchanged", io_description(*kind))));
                FileView::Unreadable(*kind)
            }
            SessionFileRead::Content(bytes) => match parse(bytes) {
                Ok(doc) => {
                    // A protected snapshot's abrupt end was reported when it
                    // was first found (checkpoints are suspended meanwhile).
                    if doc.status == Status::Pending && doc.cause == SaveCause::Checkpoint && doc.failed_attempts == 0
                    {
                        out.push(save_entry(SaveCause::Checkpoint, &doc.snapshot));
                    }
                    FileView::Doc(doc)
                }
                Err(e) => {
                    out.push(error(OP_PARSE, format!("{e}; content preserved as {}", BACKUP_FILE.target)));
                    let seq = store.take_seq();
                    store.inflight.insert(seq, InFlight::Backup);
                    out.push(Command::PreserveBackup { seq, content: bytes.clone() });
                    FileView::Malformed
                }
            },
        };
        store.durable = view.clone();
        store.desired = view;
        // The third attempt ended with the process (Req 4.7, 4.11).
        out.extend(store.abandon_if_exhausted());
        (store, out)
    }

    // --- accessors -------------------------------------------------------------

    /// What the Session_File durably holds.
    pub fn durable(&self) -> &FileView {
        &self.durable
    }

    /// What the Session_File holds once the newest issued write lands.
    pub fn desired(&self) -> &FileView {
        &self.desired
    }

    /// The Pending_Snapshot, if any (from the newest issued content).
    pub fn pending_snapshot(&self) -> Option<&SessionDoc> {
        self.desired.doc().filter(|d| d.status == Status::Pending)
    }

    pub fn is_restoring(&self) -> bool {
        self.restoring
    }

    /// The stored snapshot is a protected retry snapshot (design.md
    /// "Failed-restoration protection"): Pending with `failed_attempts ≥ 1`
    /// while no restoration runs. Checkpoints are suspended meanwhile.
    pub fn is_protected(&self) -> bool {
        !self.restoring && self.holds_retry_snapshot()
    }

    /// An attempt counted by [`StoreLogic::begin_attempt`] is running.
    pub fn is_attempting(&self) -> bool {
        self.attempting
    }

    /// Due time of the scheduled checkpoint.
    pub fn checkpoint_due(&self) -> Option<Millis> {
        self.checkpoint.map(|(_, at)| at)
    }

    /// Due time and number of the scheduled retry.
    pub fn retry_due(&self) -> Option<(Millis, u8)> {
        self.retry.as_ref().map(|r| (r.at, r.retries))
    }

    /// All retries failed; the write is repeated at the next change.
    pub fn awaiting_change_to_retry(&self) -> bool {
        self.exhausted.is_some()
    }

    /// Number of writes issued and not completed yet.
    pub fn writes_in_flight(&self) -> usize {
        self.inflight.len()
    }

    /// The write `seq`, or a newer one, has landed: the Session_File durably
    /// holds that content or newer.
    pub fn is_durable(&self, seq: u64) -> bool {
        self.committed_seq >= seq
    }

    // --- inputs ----------------------------------------------------------------

    /// Store-routed events: `WriteCompleted` and store timers.
    pub fn handle(
        &mut self,
        now: Millis,
        wall: Option<WallClock>,
        event: &Event,
        live: &impl LiveView,
    ) -> Vec<Command> {
        match event {
            Event::WriteCompleted { seq, result } => self.write_completed(now, *seq, *result),
            Event::Timer(TimerId { owner: TimerOwner::Store, seq }) => self.timer(now, wall, *seq, live),
            // EndSessionQuery/Cancelled: the save arrives as the tracker's
            // Session_End. ExitRequested: the Engine's exit flow calls
            // `session_end`.
            _ => Vec::new(),
        }
    }

    /// The Live_Session changed (`TrackerOutput::changed` without a
    /// Session_End).
    pub fn live_changed(&mut self, now: Millis, live: &impl LiveView) -> Vec<Command> {
        let mut out = Vec::new();
        if let Some(job) = self.exhausted.take() {
            out.push(self.issue(job, 0));
        }
        if self.checkpoints_allowed(live) {
            if self.checkpoint.is_none() {
                let seq = self.take_timer_seq();
                let at = now + CHECKPOINT_DELAY_MS;
                self.checkpoint = Some((seq, at));
                out.push(set_timer(seq, at));
            }
        } else if self.restoring {
            self.dirty = true;
        }
        out
    }

    /// A Session_End: save `snapshot` as the new Pending_Snapshot, urgently,
    /// with `failed_attempts = 0`. An `ExplorerTerminated` Session_End keeps
    /// a stored retry snapshot (`failed_attempts ≥ 1`, also during its
    /// restoration) and its count instead: nothing is written or logged.
    pub fn session_end(
        &mut self,
        now: Millis,
        wall: Option<WallClock>,
        cause: SaveCause,
        snapshot: Snapshot,
    ) -> Vec<Command> {
        self.checkpoint = None;
        self.dirty = false;
        if cause == SaveCause::ExplorerTerminated && self.holds_retry_snapshot() {
            return Vec::new();
        }
        // The attempt's snapshot is replaced: its end must not consume or
        // abandon the new one.
        self.attempting = false;
        let log = save_entry(cause, &snapshot);
        let doc = new_doc(now, wall, cause, snapshot);
        vec![self.issue(SessionJob { doc, urgent: true }, 0), log]
    }

    /// Counts a restoration attempt before any window-affecting command
    /// (increment-before-attempt, Req 4.6, 4.7): issues an urgent write of
    /// the Pending document with `failed_attempts + 1` and nothing else
    /// changed, and holds checkpoints back as [`StoreLogic::begin_restoration`]
    /// does. Returns the write's `seq` (for `RestoreParams::attempt_write_seq`)
    /// and the command. `None` when there is no Pending_Snapshot, it already
    /// has [`MAX_FAILED_ATTEMPTS`] (abandon it instead), or an attempt runs.
    pub fn begin_attempt(&mut self) -> Option<(u64, Command)> {
        if self.attempting {
            return None;
        }
        let mut doc = self.pending_snapshot()?.clone();
        if doc.failed_attempts >= MAX_FAILED_ATTEMPTS {
            return None;
        }
        doc.failed_attempts += 1;
        self.begin_restoration();
        self.attempting = true;
        let cmd = self.issue(SessionJob { doc, urgent: true }, 0);
        let Command::WriteSessionFile { seq, .. } = &cmd else { unreachable!("issue writes the session file") };
        Some((*seq, cmd))
    }

    /// The restoration completed (Req 4.4): the snapshot is marked Consumed
    /// with an urgent write, the restoration ends, and checkpointing resumes
    /// with the Live_Session (a checkpoint is scheduled if windows are open).
    /// Nothing is consumed if a Session_End replaced the snapshot meanwhile.
    pub fn restoration_completed(&mut self, now: Millis, live: &impl LiveView) -> Vec<Command> {
        let mut out = Vec::new();
        if std::mem::take(&mut self.attempting) {
            out.extend(self.consume());
        }
        self.dirty = true;
        out.extend(self.end_restoration(now, live));
        out
    }

    /// The restoration stopped before completing while the monitor keeps
    /// running (Req 4.10). The counted attempt stays; the snapshot stays
    /// Pending and protected, unless this was attempt
    /// [`MAX_FAILED_ATTEMPTS`], which abandons it (Req 4.11) and resumes
    /// checkpointing. Also ends the restoration.
    pub fn restoration_failed(&mut self, now: Millis, live: &impl LiveView) -> Vec<Command> {
        self.attempting = false;
        let mut out = self.abandon_if_exhausted();
        if !out.is_empty() {
            self.dirty = true;
        }
        out.extend(self.end_restoration(now, live));
        out
    }

    /// If the Pending_Snapshot has [`MAX_FAILED_ATTEMPTS`] failed attempts,
    /// marks it Consumed (urgent write) and logs `ABANDONED` (Req 4.11).
    /// Once consumed it is no longer pending, so this happens once. Called
    /// at startup, at a failed restoration, and by the trigger flow.
    pub fn abandon_if_exhausted(&mut self) -> Vec<Command> {
        if self.attempting || !self.pending_snapshot().is_some_and(|d| d.failed_attempts >= MAX_FAILED_ATTEMPTS) {
            return Vec::new();
        }
        let mut out: Vec<Command> = self.consume().into_iter().collect();
        out.push(Command::AppendLog(LogEntry::Abandoned));
        out
    }

    /// Marks the Pending_Snapshot Consumed without restoring it (urgent
    /// write, content and count kept; nothing logged): trigger steps 3 and 5,
    /// Builtin_Restore_Option logon restore and restoration disabled
    /// (Req 10.4, 4.8). Nothing while an attempt runs or without a
    /// Pending_Snapshot.
    pub fn consume_pending(&mut self) -> Vec<Command> {
        if self.attempting {
            return Vec::new();
        }
        self.consume().into_iter().collect()
    }

    /// A restoration starts (or a Trigger_Window decision is pending):
    /// checkpoints are held back and a scheduled one is cancelled (Req 4.6).
    pub fn begin_restoration(&mut self) {
        self.restoring = true;
        if self.checkpoint.take().is_some() {
            self.dirty = true;
        }
    }

    /// The restoration ended (or the Trigger_Window decision was not to
    /// restore). Changes held back meanwhile are checkpointed unless the
    /// snapshot is protected. Prefer [`StoreLogic::restoration_completed`] or
    /// [`StoreLogic::restoration_failed`] after [`StoreLogic::begin_attempt`].
    pub fn end_restoration(&mut self, now: Millis, live: &impl LiveView) -> Vec<Command> {
        self.restoring = false;
        self.attempting = false;
        if std::mem::take(&mut self.dirty) {
            self.live_changed(now, live)
        } else {
            Vec::new()
        }
    }

    // --- internals -------------------------------------------------------------

    fn checkpoints_allowed(&self, live: &impl LiveView) -> bool {
        !self.restoring && !self.is_protected() && live.has_windows()
    }

    /// Pending with `failed_attempts ≥ 1`, whether or not a restoration runs.
    fn holds_retry_snapshot(&self) -> bool {
        self.pending_snapshot().is_some_and(|d| d.failed_attempts >= 1)
    }

    /// Marks the Pending_Snapshot Consumed with an urgent write, keeping its
    /// content and count.
    fn consume(&mut self) -> Option<Command> {
        let mut doc = self.pending_snapshot()?.clone();
        doc.status = Status::Consumed;
        Some(self.issue(SessionJob { doc, urgent: true }, 0))
    }

    fn take_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        seq
    }

    fn take_timer_seq(&mut self) -> u64 {
        self.timer_seq += 1;
        self.timer_seq
    }

    /// Issues a session write. Any newer write supersedes a pending retry.
    fn issue(&mut self, job: SessionJob, retries: u8) -> Command {
        let seq = self.take_seq();
        self.retry = None;
        self.exhausted = None;
        self.last_session_seq = seq;
        self.desired = FileView::Doc(job.doc.clone());
        let cmd = Command::WriteSessionFile { seq, text: serialize(&job.doc), urgent: job.urgent };
        self.inflight.insert(seq, InFlight::Session { job, retries });
        cmd
    }

    fn timer(&mut self, now: Millis, wall: Option<WallClock>, seq: u64, live: &impl LiveView) -> Vec<Command> {
        if self.checkpoint.is_some_and(|(s, _)| s == seq) {
            self.checkpoint = None;
            if !self.checkpoints_allowed(live) {
                return Vec::new();
            }
            let snapshot = live.capture_snapshot();
            if snapshot.windows.is_empty() {
                return Vec::new();
            }
            let doc = new_doc(now, wall, SaveCause::Checkpoint, snapshot);
            return vec![self.issue(SessionJob { doc, urgent: false }, 0)];
        }
        if self.retry.as_ref().is_some_and(|r| r.timer == seq) {
            let r = self.retry.take().expect("checked above");
            return vec![self.issue(r.job, r.retries)];
        }
        Vec::new()
    }

    fn write_completed(&mut self, now: Millis, seq: u64, result: Result<(), IoErrorKind>) -> Vec<Command> {
        let Some(job) = self.inflight.remove(&seq) else {
            return Vec::new();
        };
        match (job, result) {
            (InFlight::Backup, Ok(())) => Vec::new(),
            (InFlight::Backup, Err(kind)) => vec![error(OP_BACKUP, io_description(kind))],
            (InFlight::Session { job, .. }, Ok(())) => {
                if seq > self.committed_seq {
                    self.committed_seq = seq;
                    self.durable = FileView::Doc(job.doc);
                }
                Vec::new()
            }
            (InFlight::Session { job, retries }, Err(kind)) => {
                let io = io_description(kind);
                // A newer write (issued or committed) supersedes this one.
                if seq != self.last_session_seq {
                    return vec![error(OP_WRITE, format!("{io}; superseded by a newer write"))];
                }
                if retries < MAX_RETRIES {
                    let next = retries + 1;
                    let timer = self.take_timer_seq();
                    let at = now + RETRY_INTERVAL_MS;
                    self.retry = Some(Retry { job, retries: next, timer, at });
                    vec![
                        error(OP_WRITE, format!("{io}; retry {next} of {MAX_RETRIES} in 2 s")),
                        set_timer(timer, at),
                    ]
                } else {
                    self.exhausted = Some(job);
                    vec![error(
                        OP_WRITE,
                        format!("{io}; all {MAX_RETRIES} retries failed, writing again at the next change"),
                    )]
                }
            }
        }
    }
}

fn new_doc(now: Millis, wall: Option<WallClock>, cause: SaveCause, snapshot: Snapshot) -> SessionDoc {
    SessionDoc {
        version: FORMAT_VERSION,
        status: Status::Pending,
        failed_attempts: 0,
        saved_at: local_time(wall, now),
        cause,
        snapshot,
    }
}

fn set_timer(seq: u64, at: Millis) -> Command {
    Command::SetTimer { id: TimerId { owner: TimerOwner::Store, seq }, at }
}

fn error(operation: &str, description: String) -> Command {
    Command::AppendLog(LogEntry::Error { operation: operation.to_owned(), description })
}

fn save_entry(cause: SaveCause, snapshot: &Snapshot) -> Command {
    Command::AppendLog(LogEntry::Save {
        cause,
        windows: snapshot.windows.len() as u32,
        tabs: snapshot.tab_count() as u32,
    })
}

/// Human-readable description of a file error for the Activity_Log.
pub fn io_description(kind: IoErrorKind) -> String {
    match kind {
        IoErrorKind::NotFound => "file or folder not found".to_owned(),
        IoErrorKind::AccessDenied => "access denied".to_owned(),
        IoErrorKind::SharingViolation => "sharing violation".to_owned(),
        IoErrorKind::DiskFull => "disk full".to_owned(),
        IoErrorKind::Other(code) => format!("Win32 error {code}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::ParseErrorKind;
    use crate::model::{
        DisplayRef, LocationId, LocationKind, Placement, Rect, ShowState, TabRec, Timestamp, Volume,
        WindowId, WindowRec,
    };

    /// A Live_Session stand-in: `None` means no window is open.
    struct Live(Option<Snapshot>);

    impl LiveView for Live {
        fn has_windows(&self) -> bool {
            self.0.is_some()
        }
        fn capture_snapshot(&self) -> Snapshot {
            self.0.clone().unwrap_or_default()
        }
    }

    fn snapshot(tabs: usize) -> Snapshot {
        let r = Rect { left: 0, top: 0, right: 800, bottom: 600 };
        let tab = |i: usize| TabRec {
            location: LocationId {
                case_sensitive: false,
                kind: LocationKind::FileSystem(Volume::Fixed),
                parsing_name: format!(r"C:\dir{i}").as_str().into(),
                display_name: format!("dir{i}").as_str().into(),
                pidl: vec![1, 2, 3],
                search: None,
            },
        };
        Snapshot {
            windows: vec![WindowRec {
                id: WindowId(0),
                tabs: (0..tabs).map(tab).collect(),
                active: 0,
                placement: Placement {
                    state: ShowState::Normal,
                    restored: r,
                    display: DisplayRef { device: r"\\.\DISPLAY1".into(), monitor: r },
                },
            }],
            activation: vec![WindowId(0)],
        }
    }

    fn open(tabs: usize) -> Live {
        Live(Some(snapshot(tabs)))
    }

    const CLOSED: Live = Live(None);

    fn doc(status: Status, cause: SaveCause, failed_attempts: u8) -> SessionDoc {
        SessionDoc {
            version: FORMAT_VERSION,
            status,
            failed_attempts,
            saved_at: Timestamp {
                year: 2025,
                month: 1,
                day: 31,
                hour: 9,
                minute: 15,
                second: 2,
                millis: 123,
                offset_minutes: 60,
            },
            cause,
            snapshot: snapshot(2),
        }
    }

    fn content(d: &SessionDoc) -> SessionFileRead {
        SessionFileRead::Content(serialize(d).into_bytes())
    }

    fn fresh() -> StoreLogic {
        StoreLogic::startup(&SessionFileRead::Absent).0
    }

    fn store_timer(cmds: &[Command]) -> Option<(u64, Millis)> {
        cmds.iter().find_map(|c| match c {
            Command::SetTimer { id: TimerId { owner: TimerOwner::Store, seq }, at } => Some((*seq, *at)),
            _ => None,
        })
    }

    fn fire(s: &mut StoreLogic, at: Millis, seq: u64, live: &Live) -> Vec<Command> {
        s.handle(at, None, &Event::Timer(TimerId { owner: TimerOwner::Store, seq }), live)
    }

    fn complete(s: &mut StoreLogic, at: Millis, seq: u64, result: Result<(), IoErrorKind>) -> Vec<Command> {
        s.handle(at, None, &Event::WriteCompleted { seq, result }, &CLOSED)
    }

    /// The single session write in `cmds`: (seq, parsed doc, text, urgent).
    fn write(cmds: &[Command]) -> (u64, SessionDoc, String, bool) {
        let writes: Vec<_> = cmds
            .iter()
            .filter_map(|c| match c {
                Command::WriteSessionFile { seq, text, urgent } => Some((*seq, text.clone(), *urgent)),
                _ => None,
            })
            .collect();
        assert_eq!(writes.len(), 1, "{cmds:?}");
        let (seq, text, urgent) = writes[0].clone();
        (seq, parse(text.as_bytes()).expect("store writes parse"), text, urgent)
    }

    fn errors(cmds: &[Command]) -> Vec<(String, String)> {
        cmds.iter()
            .filter_map(|c| match c {
                Command::AppendLog(LogEntry::Error { operation, description }) => {
                    Some((operation.clone(), description.clone()))
                }
                _ => None,
            })
            .collect()
    }

    // --- startup ---------------------------------------------------------------

    #[test]
    fn absent_file_gives_no_snapshot_and_no_log() {
        let (s, cmds) = StoreLogic::startup(&SessionFileRead::Absent);
        assert!(cmds.is_empty());
        assert_eq!(s.durable(), &FileView::Absent);
        assert!(s.pending_snapshot().is_none());
    }

    #[test]
    fn unreadable_file_is_left_alone_and_logged() {
        let (s, cmds) = StoreLogic::startup(&SessionFileRead::Unreadable(IoErrorKind::AccessDenied));
        assert_eq!(cmds.len(), 1);
        let errs = errors(&cmds);
        assert_eq!(errs[0].0, OP_READ);
        assert!(errs[0].1.contains("access denied"), "{errs:?}");
        assert_eq!(s.durable(), &FileView::Unreadable(IoErrorKind::AccessDenied));
        assert!(s.pending_snapshot().is_none());
    }

    #[test]
    fn malformed_file_is_preserved_as_backup_and_logged_with_position() {
        let bytes = b"ESR-SESSION version=1\nstatus pending\nfailed-attempts 9\n".to_vec();
        let (mut s, cmds) = StoreLogic::startup(&SessionFileRead::Content(bytes.clone()));
        let backup: Vec<_> = cmds
            .iter()
            .filter_map(|c| match c {
                Command::PreserveBackup { seq, content } => Some((*seq, content.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(backup.len(), 1);
        assert_eq!(backup[0].1, bytes);
        let err = parse(&bytes).unwrap_err();
        let errs = errors(&cmds);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].0, OP_PARSE);
        assert!(errs[0].1.starts_with(&format!("line {}, column {}: {}", err.line, err.column, err.kind)));
        assert_eq!(s.durable(), &FileView::Malformed);
        assert!(s.pending_snapshot().is_none());

        // A failed backup is logged; a successful one is silent.
        assert!(complete(&mut s, Millis(1), backup[0].0, Ok(())).is_empty());
        let (mut s, cmds) = StoreLogic::startup(&SessionFileRead::Content(bytes));
        let seq = match cmds.iter().find(|c| matches!(c, Command::PreserveBackup { .. })) {
            Some(Command::PreserveBackup { seq, .. }) => *seq,
            _ => unreachable!(),
        };
        let errs = errors(&complete(&mut s, Millis(1), seq, Err(IoErrorKind::DiskFull)));
        assert_eq!(errs, vec![(OP_BACKUP.to_owned(), "disk full".to_owned())]);
    }

    #[test]
    fn newer_version_is_preserved_and_logged_with_both_versions() {
        let text = serialize(&doc(Status::Pending, SaveCause::LastWindowClosed, 0))
            .replacen("version=1", "version=7", 1);
        let (s, cmds) = StoreLogic::startup(&SessionFileRead::Content(text.into_bytes()));
        assert!(cmds.iter().any(|c| matches!(c, Command::PreserveBackup { .. })));
        let errs = errors(&cmds);
        let kind = ParseErrorKind::UnsupportedVersion { declared: 7, newest: FORMAT_VERSION };
        assert!(errs[0].1.contains(&kind.to_string()), "{errs:?}");
        assert!(errs[0].1.contains("version 7") && errs[0].1.contains("version 1"));
        assert!(s.pending_snapshot().is_none());
    }

    #[test]
    fn pending_checkpoint_is_logged_as_abrupt_end() {
        let d = doc(Status::Pending, SaveCause::Checkpoint, 0);
        let (s, cmds) = StoreLogic::startup(&content(&d));
        assert_eq!(
            cmds,
            vec![Command::AppendLog(LogEntry::Save { cause: SaveCause::Checkpoint, windows: 1, tabs: 2 })]
        );
        assert_eq!(s.pending_snapshot(), Some(&d));
        assert_eq!(s.durable(), &FileView::Doc(d));

        // Other causes and consumed checkpoints were logged when they were saved.
        for d in [
            doc(Status::Pending, SaveCause::LastTabClosed, 0),
            doc(Status::Consumed, SaveCause::Checkpoint, 0),
        ] {
            let (s, cmds) = StoreLogic::startup(&content(&d));
            assert!(cmds.is_empty());
            assert_eq!(s.durable(), &FileView::Doc(d));
        }
    }

    // --- checkpoints -----------------------------------------------------------

    #[test]
    fn changes_coalesce_into_one_checkpoint_500ms_after_the_first() {
        let mut s = fresh();
        let live = open(3);
        let (seq, at) = store_timer(&s.live_changed(Millis(1_000), &live)).unwrap();
        assert_eq!(at, Millis(1_500));
        assert!(s.live_changed(Millis(1_100), &live).is_empty());
        assert!(s.live_changed(Millis(1_499), &live).is_empty());
        assert_eq!(s.checkpoint_due(), Some(Millis(1_500)));

        let wall = WallClock { at: Millis(1_000), local: doc(Status::Pending, SaveCause::Checkpoint, 0).saved_at };
        let cmds = s.handle(Millis(1_500), Some(wall), &Event::Timer(TimerId { owner: TimerOwner::Store, seq }), &live);
        let (wseq, d, _, urgent) = write(&cmds);
        assert!(!urgent);
        assert_eq!((d.status, d.cause, d.failed_attempts), (Status::Pending, SaveCause::Checkpoint, 0));
        assert_eq!(d.snapshot, snapshot(3));
        assert_eq!(d.saved_at, wall.local_at(Millis(1_500)));
        assert_eq!(s.pending_snapshot(), Some(&d));
        assert_eq!(s.durable(), &FileView::Absent);

        assert!(complete(&mut s, Millis(1_600), wseq, Ok(())).is_empty());
        assert_eq!(s.durable(), &FileView::Doc(d));

        // The next change starts a new window.
        let (_, at) = store_timer(&s.live_changed(Millis(1_700), &live)).unwrap();
        assert_eq!(at, Millis(2_200));
    }

    #[test]
    fn no_checkpoint_without_windows_or_with_an_empty_capture() {
        let mut s = fresh();
        assert!(s.live_changed(Millis(0), &CLOSED).is_empty());

        let (seq, _) = store_timer(&s.live_changed(Millis(0), &open(1))).unwrap();
        // Every window closed before the timer fired.
        assert!(fire(&mut s, Millis(500), seq, &CLOSED).is_empty());

        let (seq, _) = store_timer(&s.live_changed(Millis(600), &open(1))).unwrap();
        // Windows are open, but no tab has a Location yet.
        assert!(fire(&mut s, Millis(1_100), seq, &Live(Some(Snapshot::default()))).is_empty());
        assert_eq!(s.desired(), &FileView::Absent);
    }

    #[test]
    fn stale_and_foreign_timers_are_ignored() {
        let mut s = fresh();
        let (seq, _) = store_timer(&s.live_changed(Millis(0), &open(1))).unwrap();
        assert!(fire(&mut s, Millis(500), seq + 100, &open(1)).is_empty());
        let foreign = Event::Timer(TimerId { owner: TimerOwner::Tracker, seq });
        assert!(s.handle(Millis(500), None, &foreign, &open(1)).is_empty());
        assert_eq!(write(&fire(&mut s, Millis(500), seq, &open(1))).1.snapshot, snapshot(1));
        // Firing twice writes once.
        assert!(fire(&mut s, Millis(501), seq, &open(1)).is_empty());
    }

    #[test]
    fn restoration_holds_back_checkpoints_until_it_ends() {
        let mut s = fresh();
        let (seq, _) = store_timer(&s.live_changed(Millis(0), &open(1))).unwrap();
        s.begin_restoration();
        assert!(s.is_restoring());
        assert_eq!(s.checkpoint_due(), None);
        assert!(fire(&mut s, Millis(500), seq, &open(1)).is_empty());
        assert!(s.live_changed(Millis(600), &open(2)).is_empty());

        let (seq, at) = store_timer(&s.end_restoration(Millis(1_000), &open(2))).unwrap();
        assert_eq!(at, Millis(1_500));
        assert_eq!(write(&fire(&mut s, at, seq, &open(2))).1.snapshot, snapshot(2));

        // Nothing held back: ending schedules nothing.
        s.begin_restoration();
        assert!(s.end_restoration(Millis(2_000), &open(2)).is_empty());
    }

    #[test]
    fn protected_snapshot_suspends_checkpoints() {
        let (mut s, _) = StoreLogic::startup(&content(&doc(Status::Pending, SaveCause::LastWindowClosed, 1)));
        assert!(s.is_protected());
        assert!(s.live_changed(Millis(0), &open(1)).is_empty());
        s.begin_restoration();
        assert!(!s.is_protected());
    }

    // --- Session_End -----------------------------------------------------------

    #[test]
    fn session_end_writes_urgently_with_zero_attempts_and_logs_save() {
        let (mut s, _) = StoreLogic::startup(&content(&doc(Status::Consumed, SaveCause::Restart, 2)));
        let (cp, _) = store_timer(&s.live_changed(Millis(0), &open(1))).unwrap();
        let cmds = s.session_end(Millis(100), None, SaveCause::LastTabClosed, snapshot(4));
        let (_, d, _, urgent) = write(&cmds);
        assert!(urgent);
        assert_eq!(
            (d.status, d.cause, d.failed_attempts, d.version),
            (Status::Pending, SaveCause::LastTabClosed, 0, FORMAT_VERSION)
        );
        assert_eq!(d.snapshot, snapshot(4));
        assert!(cmds.contains(&Command::AppendLog(LogEntry::Save {
            cause: SaveCause::LastTabClosed,
            windows: 1,
            tabs: 4,
        })));
        assert_eq!(s.pending_snapshot(), Some(&d));
        // The scheduled checkpoint was cancelled.
        assert_eq!(s.checkpoint_due(), None);
        assert!(fire(&mut s, Millis(500), cp, &open(1)).is_empty());
    }

    #[test]
    fn older_completion_never_regresses_the_durable_view() {
        let mut s = fresh();
        let (cp, _) = store_timer(&s.live_changed(Millis(0), &open(1))).unwrap();
        let (old, ..) = write(&fire(&mut s, Millis(500), cp, &open(1)));
        let (new, end, ..) = write(&s.session_end(Millis(510), None, SaveCause::SignOut, snapshot(2)));
        assert!(complete(&mut s, Millis(520), new, Ok(())).is_empty());
        // The writer skipped the older queued checkpoint and reported Ok.
        assert!(complete(&mut s, Millis(530), old, Ok(())).is_empty());
        assert_eq!(s.durable(), &FileView::Doc(end.clone()));
        // A late failure of the superseded write is logged but not retried.
        let (cp2, _) = store_timer(&s.live_changed(Millis(600), &open(1))).unwrap();
        let (w, ..) = write(&fire(&mut s, Millis(1_100), cp2, &open(1)));
        let (_, _end2, ..) = write(&s.session_end(Millis(1_110), None, SaveCause::LastWindowClosed, snapshot(1)));
        let cmds = complete(&mut s, Millis(1_120), w, Err(IoErrorKind::SharingViolation));
        assert_eq!(errors(&cmds).len(), 1);
        assert!(store_timer(&cmds).is_none());
        assert_eq!(s.durable(), &FileView::Doc(end));
    }

    // --- write failures --------------------------------------------------------

    #[test]
    fn failed_write_is_retried_five_times_then_at_the_next_change() {
        let (mut s, _) = StoreLogic::startup(&content(&doc(Status::Consumed, SaveCause::Restart, 0)));
        let before = s.durable().clone();
        let (mut seq, d, text, _) = write(&s.session_end(Millis(0), None, SaveCause::LastWindowClosed, snapshot(1)));
        let mut now = Millis(50);
        for n in 1..=MAX_RETRIES {
            let cmds = complete(&mut s, now, seq, Err(IoErrorKind::DiskFull));
            let errs = errors(&cmds);
            assert_eq!(errs.len(), 1);
            assert_eq!(errs[0].0, OP_WRITE);
            assert!(errs[0].1.contains(&format!("retry {n} of 5")), "{errs:?}");
            let (timer, at) = store_timer(&cmds).unwrap();
            assert_eq!(at, now + RETRY_INTERVAL_MS);
            assert_eq!(s.retry_due(), Some((at, n)));
            // The file keeps its previous content; the snapshot stays pending in memory.
            assert_eq!(s.durable(), &before);
            assert_eq!(s.pending_snapshot(), Some(&d));
            now = at;
            let (next, _, retried, urgent) = write(&fire(&mut s, now, timer, &CLOSED));
            assert_eq!(retried, text);
            assert!(urgent);
            seq = next;
            now = now + 50;
        }
        // The fifth retry fails too: wait for the next change.
        let cmds = complete(&mut s, now, seq, Err(IoErrorKind::DiskFull));
        assert_eq!(errors(&cmds).len(), 1);
        assert!(store_timer(&cmds).is_none());
        assert!(s.awaiting_change_to_retry());
        assert_eq!(s.retry_due(), None);

        let cmds = s.live_changed(now + 10_000, &CLOSED);
        let (seq, _, retried, _) = write(&cmds);
        assert_eq!(retried, text);
        assert!(!s.awaiting_change_to_retry());
        assert!(complete(&mut s, now + 10_100, seq, Ok(())).is_empty());
        assert_eq!(s.durable(), &FileView::Doc(d));
    }

    #[test]
    fn a_newer_write_supersedes_a_pending_retry() {
        let mut s = fresh();
        let (seq, ..) = write(&s.session_end(Millis(0), None, SaveCause::LastWindowClosed, snapshot(1)));
        let (timer, _) = store_timer(&complete(&mut s, Millis(10), seq, Err(IoErrorKind::Other(5)))).unwrap();
        let (seq2, d2, ..) = write(&s.session_end(Millis(20), None, SaveCause::Shutdown, snapshot(3)));
        assert_eq!(s.retry_due(), None);
        assert!(fire(&mut s, Millis(2_010), timer, &CLOSED).is_empty());
        assert!(complete(&mut s, Millis(30), seq2, Ok(())).is_empty());
        assert_eq!(s.durable(), &FileView::Doc(d2));
        assert_eq!(s.writes_in_flight(), 0);
    }

    #[test]
    fn unknown_completions_are_ignored() {
        let mut s = fresh();
        assert!(complete(&mut s, Millis(0), 42, Err(IoErrorKind::DiskFull)).is_empty());
        assert!(s.handle(Millis(0), None, &Event::ExitRequested, &open(1)).is_empty());
    }

    // --- attempts, protection, consumption, abandonment ------------------------

    fn abandoned(cmds: &[Command]) -> usize {
        cmds.iter().filter(|c| matches!(c, Command::AppendLog(LogEntry::Abandoned))).count()
    }

    fn pending(attempts: u8) -> StoreLogic {
        let (s, _) = StoreLogic::startup(&content(&doc(Status::Pending, SaveCause::LastWindowClosed, attempts)));
        s
    }

    #[test]
    fn begin_attempt_writes_the_incremented_count_urgently_and_nothing_else() {
        let mut s = pending(0);
        let before = doc(Status::Pending, SaveCause::LastWindowClosed, 0);
        let (seq, cmd) = s.begin_attempt().unwrap();
        let (wseq, d, _, urgent) = write(std::slice::from_ref(&cmd));
        assert_eq!(wseq, seq);
        assert!(urgent);
        assert_eq!(d, SessionDoc { failed_attempts: 1, ..before });
        assert!(s.is_restoring() && s.is_attempting());
        assert_eq!(s.pending_snapshot(), Some(&d));
        // Live_Session changes are held back; a second attempt is refused.
        assert!(s.live_changed(Millis(0), &open(3)).is_empty());
        assert!(s.begin_attempt().is_none());
        assert!(complete(&mut s, Millis(10), seq, Ok(())).is_empty());
        assert_eq!(s.durable(), &FileView::Doc(d));
    }

    #[test]
    fn no_attempt_without_a_pending_snapshot() {
        assert!(fresh().begin_attempt().is_none());
        let (mut s, _) = StoreLogic::startup(&content(&doc(Status::Consumed, SaveCause::Restart, 0)));
        assert!(s.begin_attempt().is_none());
        assert!(!s.is_restoring());
    }

    #[test]
    fn completion_consumes_the_snapshot_and_resumes_checkpoints() {
        let mut s = pending(1);
        let (seq, _) = s.begin_attempt().unwrap();
        assert!(complete(&mut s, Millis(5), seq, Ok(())).is_empty());
        let cmds = s.restoration_completed(Millis(100), &open(3));
        let (_, d, _, urgent) = write(&cmds);
        assert!(urgent);
        assert_eq!(d, SessionDoc { status: Status::Consumed, ..doc(Status::Pending, SaveCause::LastWindowClosed, 2) });
        assert!(s.pending_snapshot().is_none());
        assert!(!s.is_restoring() && !s.is_protected() && !s.is_attempting());
        let (cp, at) = store_timer(&cmds).unwrap();
        assert_eq!(at, Millis(600));
        let (_, d, ..) = write(&fire(&mut s, at, cp, &open(3)));
        assert_eq!((d.status, d.cause, d.failed_attempts), (Status::Pending, SaveCause::Checkpoint, 0));
    }

    #[test]
    fn failed_attempts_protect_the_snapshot_and_the_third_abandons_it_once() {
        let mut s = pending(0);
        for n in 1..MAX_FAILED_ATTEMPTS {
            let (seq, _) = s.begin_attempt().unwrap();
            assert!(complete(&mut s, Millis(5), seq, Ok(())).is_empty());
            // Partly restored windows are open, but the snapshot is kept.
            assert!(s.restoration_failed(Millis(100), &open(1)).is_empty());
            assert!(s.is_protected());
            assert_eq!(s.pending_snapshot().unwrap().failed_attempts, n);
            assert!(s.live_changed(Millis(200), &open(1)).is_empty());
        }
        let (seq, _) = s.begin_attempt().unwrap();
        assert!(complete(&mut s, Millis(5), seq, Ok(())).is_empty());
        let cmds = s.restoration_failed(Millis(100), &open(1));
        assert_eq!(abandoned(&cmds), 1);
        let (_, d, _, urgent) = write(&cmds);
        assert!(urgent);
        assert_eq!((d.status, d.failed_attempts), (Status::Consumed, MAX_FAILED_ATTEMPTS));
        assert_eq!(d.snapshot, snapshot(2));
        assert!(s.pending_snapshot().is_none() && !s.is_protected());
        // Logged once; normal checkpointing resumes.
        assert!(s.abandon_if_exhausted().is_empty());
        assert!(s.begin_attempt().is_none());
        assert!(store_timer(&cmds).is_some());
    }

    #[test]
    fn startup_abandons_a_snapshot_whose_third_attempt_crashed() {
        let d = doc(Status::Pending, SaveCause::Checkpoint, MAX_FAILED_ATTEMPTS);
        let (mut s, cmds) = StoreLogic::startup(&content(&d));
        assert_eq!(abandoned(&cmds), 1);
        // Its abrupt end was already reported when it was first found.
        assert!(!cmds.iter().any(|c| matches!(c, Command::AppendLog(LogEntry::Save { .. }))));
        let (_, w, ..) = write(&cmds);
        assert_eq!(w, SessionDoc { status: Status::Consumed, ..d });
        assert!(s.pending_snapshot().is_none());
        assert!(s.abandon_if_exhausted().is_empty());
    }

    #[test]
    fn restart_during_protection_stays_protected_without_a_save_log() {
        let (s, cmds) = StoreLogic::startup(&content(&doc(Status::Pending, SaveCause::Checkpoint, 1)));
        assert!(cmds.is_empty());
        assert!(s.is_protected());
    }

    #[test]
    fn explorer_termination_keeps_a_protected_snapshot() {
        // While protected, and during the restoration that counted it.
        let mut s = pending(1);
        let before = s.desired().clone();
        assert!(s.session_end(Millis(0), None, SaveCause::ExplorerTerminated, snapshot(4)).is_empty());
        assert_eq!(s.desired(), &before);
        let (seq, _) = s.begin_attempt().unwrap();
        let counted = s.desired().clone();
        assert!(s.session_end(Millis(10), None, SaveCause::ExplorerTerminated, snapshot(4)).is_empty());
        assert_eq!(s.desired(), &counted);
        assert!(complete(&mut s, Millis(20), seq, Ok(())).is_empty());
        assert!(s.restoration_failed(Millis(30), &CLOSED).is_empty());
        assert_eq!(s.pending_snapshot().unwrap().failed_attempts, 2);

        // An unprotected snapshot is replaced as usual.
        let mut s = pending(0);
        let (_, d, ..) = write(&s.session_end(Millis(0), None, SaveCause::ExplorerTerminated, snapshot(4)));
        assert_eq!((d.cause, d.failed_attempts), (SaveCause::ExplorerTerminated, 0));
    }

    #[test]
    fn user_caused_session_end_replaces_a_protected_snapshot() {
        for cause in [
            SaveCause::LastWindowClosed,
            SaveCause::LastTabClosed,
            SaveCause::SignOut,
            SaveCause::Shutdown,
            SaveCause::Restart,
            SaveCause::ExitThroughSettings,
        ] {
            let mut s = pending(2);
            let (_, d, _, urgent) = write(&s.session_end(Millis(0), None, cause, snapshot(4)));
            assert!(urgent);
            assert_eq!((d.status, d.cause, d.failed_attempts), (Status::Pending, cause, 0));
            assert_eq!(d.snapshot, snapshot(4));
            assert!(!s.is_protected());
        }
    }

    #[test]
    fn a_session_end_during_an_attempt_is_not_consumed_by_its_end() {
        let mut s = pending(2);
        let (seq, _) = s.begin_attempt().unwrap();
        assert!(complete(&mut s, Millis(5), seq, Ok(())).is_empty());
        let (_, end, ..) = write(&s.session_end(Millis(10), None, SaveCause::SignOut, snapshot(4)));
        assert!(!s.is_attempting());
        assert!(write_count(&s.restoration_completed(Millis(20), &CLOSED)) == 0);
        assert_eq!(s.pending_snapshot(), Some(&end));

        // Nor abandoned by a third failure.
        let mut s = pending(2);
        s.begin_attempt().unwrap();
        let (_, end, ..) = write(&s.session_end(Millis(10), None, SaveCause::LastWindowClosed, snapshot(1)));
        assert!(s.restoration_failed(Millis(20), &CLOSED).is_empty());
        assert_eq!(s.pending_snapshot(), Some(&end));
    }

    fn write_count(cmds: &[Command]) -> usize {
        cmds.iter().filter(|c| matches!(c, Command::WriteSessionFile { .. })).count()
    }
}
