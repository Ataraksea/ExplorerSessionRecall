// Unit tests for Session_Store startup and write retry (task 8.7).
//
// Startup: the Session_File is absent, unreadable (every `IoErrorKind`),
// malformed (one input per parse error kind), from a newer format version, a
// pending checkpoint left by an abrupt end, or a consumed snapshot.
//
// Retry: a failed write is retried 5 times at 2-second intervals, each
// failure logged, and then written again at the next Live_Session change; a
// success stops the retries; a newer write supersedes a pending retry; a
// failed backup write is logged.
//
// Everything goes through the public `StoreLogic` API, and the startup and
// retry paths are also driven once through the `Engine`.
//
// Requirements: 3.12, 11.7, 11.9, 11.10, 11.11

use esr_core::engine::{
    Command, Engine, Environment, Event, IoErrorKind, Millis, ObservedLocation, SessionFileRead,
    StartupInput, TabKey, TabObservation, TimerId, TimerOwner, WinKey, WindowObservation,
};
use esr_core::format::{parse, serialize, ParseErrorKind, FORMAT_VERSION};
use esr_core::logfmt::{format_entry, LogEntry};
use esr_core::model::*;
use esr_core::settings::Settings;
use esr_core::store::logic::{io_description, OP_BACKUP, OP_PARSE, OP_READ, OP_WRITE};
use esr_core::store::{FileView, LiveView, StoreLogic, MAX_RETRIES, RETRY_INTERVAL_MS};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const ALL_IO_KINDS: [IoErrorKind; 5] = [
    IoErrorKind::NotFound,
    IoErrorKind::AccessDenied,
    IoErrorKind::SharingViolation,
    IoErrorKind::DiskFull,
    IoErrorKind::Other(1392),
];

/// A Live_Session stand-in: `None` means no Explorer window is open.
struct Live(Option<Snapshot>);

impl LiveView for Live {
    fn has_windows(&self) -> bool {
        self.0.is_some()
    }
    fn capture_snapshot(&self) -> Snapshot {
        self.0.clone().unwrap_or_default()
    }
}

const CLOSED: Live = Live(None);

fn open(tabs: &[usize]) -> Live {
    Live(Some(snapshot(tabs)))
}

fn rect(right: i32, bottom: i32) -> Rect {
    Rect { left: 0, top: 0, right, bottom }
}

fn placement() -> Placement {
    Placement {
        state: ShowState::Normal,
        restored: rect(800, 600),
        display: DisplayRef { device: r"\\.\DISPLAY1".into(), monitor: rect(1920, 1080) },
    }
}

fn location(i: usize) -> LocationId {
    LocationId {
        case_sensitive: false,
        kind: LocationKind::FileSystem(Volume::Fixed),
        parsing_name: format!(r"C:\dir{i}").as_str().into(),
        display_name: format!("dir{i}").as_str().into(),
        pidl: vec![1, 2, 3],
        search: None,
    }
}

/// One window per entry of `tabs`, holding that many tabs.
fn snapshot(tabs: &[usize]) -> Snapshot {
    let windows: Vec<WindowRec> = tabs
        .iter()
        .enumerate()
        .map(|(w, &n)| WindowRec {
            id: WindowId(w as u32),
            tabs: (0..n).map(|t| TabRec { location: location(w * 100 + t) }).collect(),
            active: 0,
            placement: placement(),
        })
        .collect();
    let activation = windows.iter().map(|w| w.id).collect();
    Snapshot { windows, activation }
}

fn doc(status: Status, cause: SaveCause, failed_attempts: u8, tabs: &[usize]) -> SessionDoc {
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
        snapshot: snapshot(tabs),
    }
}

fn content(d: &SessionDoc) -> SessionFileRead {
    SessionFileRead::Content(serialize(d).into_bytes())
}

/// `text` with `from` replaced by `to` once (`from` must occur).
fn edit(text: &str, from: &str, to: &str) -> String {
    assert!(text.contains(from), "{from:?} not in {text}");
    text.replacen(from, to, 1)
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

fn backups(cmds: &[Command]) -> Vec<(u64, Vec<u8>)> {
    cmds.iter()
        .filter_map(|c| match c {
            Command::PreserveBackup { seq, content } => Some((*seq, content.clone())),
            _ => None,
        })
        .collect()
}

/// Every session write in `cmds`: (seq, text, urgent).
fn writes(cmds: &[Command]) -> Vec<(u64, String, bool)> {
    cmds.iter()
        .filter_map(|c| match c {
            Command::WriteSessionFile { seq, text, urgent } => Some((*seq, text.clone(), *urgent)),
            _ => None,
        })
        .collect()
}

/// The single session write in `cmds`.
fn write(cmds: &[Command]) -> (u64, String, bool) {
    let w = writes(cmds);
    assert_eq!(w.len(), 1, "{cmds:?}");
    w[0].clone()
}

/// The single store timer set by `cmds`, if any.
fn store_timer(cmds: &[Command]) -> Option<(u64, Millis)> {
    let timers: Vec<_> = cmds
        .iter()
        .filter_map(|c| match c {
            Command::SetTimer { id: TimerId { owner: TimerOwner::Store, seq }, at } => Some((*seq, *at)),
            _ => None,
        })
        .collect();
    assert!(timers.len() <= 1, "{cmds:?}");
    timers.first().copied()
}

fn fire(s: &mut StoreLogic, at: Millis, seq: u64, live: &Live) -> Vec<Command> {
    s.handle(at, None, &Event::Timer(TimerId { owner: TimerOwner::Store, seq }), live)
}

fn complete(s: &mut StoreLogic, at: Millis, seq: u64, result: Result<(), IoErrorKind>) -> Vec<Command> {
    s.handle(at, None, &Event::WriteCompleted { seq, result }, &CLOSED)
}

/// Asserts the store issued nothing that touches the Session_File.
fn assert_no_file_writes(cmds: &[Command]) {
    assert!(writes(cmds).is_empty(), "{cmds:?}");
}

fn engine_startup(session_file: SessionFileRead) -> (Engine, Vec<Command>) {
    Engine::new(
        Settings::default(),
        StartupInput {
            session_file,
            environment: Environment::default(),
            background_logon: true,
            logon_at: None,
            shell_ready_at: Some(Millis(0)),
        },
    )
}

// ---------------------------------------------------------------------------
// Startup: absent (Req 11.10)
// ---------------------------------------------------------------------------

#[test]
fn absent_file_proceeds_without_snapshot_log_or_write() {
    let (mut s, cmds) = StoreLogic::startup(&SessionFileRead::Absent);
    assert!(cmds.is_empty(), "{cmds:?}");
    assert_eq!(s.durable(), &FileView::Absent);
    assert_eq!(s.desired(), &FileView::Absent);
    assert!(s.pending_snapshot().is_none());
    assert_eq!(s.writes_in_flight(), 0);
    assert!(s.begin_attempt().is_none(), "no restoration without a Pending_Snapshot");

    // The store works normally afterwards: the first Session_End creates the file.
    let (seq, text, urgent) = write(&s.session_end(Millis(0), None, SaveCause::LastWindowClosed, snapshot(&[2])));
    assert!(urgent);
    assert!(complete(&mut s, Millis(5), seq, Ok(())).is_empty());
    assert_eq!(s.durable(), &FileView::Doc(parse(text.as_bytes()).unwrap()));

    let (engine, cmds) = engine_startup(SessionFileRead::Absent);
    assert!(cmds.is_empty(), "{cmds:?}");
    assert!(engine.store().pending_snapshot().is_none());
}

// ---------------------------------------------------------------------------
// Startup: unreadable (Req 11.11)
// ---------------------------------------------------------------------------

#[test]
fn unreadable_file_is_left_unchanged_logged_and_gives_no_snapshot() {
    for kind in ALL_IO_KINDS {
        let (s, cmds) = StoreLogic::startup(&SessionFileRead::Unreadable(kind));
        // Exactly one ERROR entry and nothing that touches the file.
        assert_eq!(cmds.len(), 1, "{kind:?}: {cmds:?}");
        assert!(backups(&cmds).is_empty());
        assert_no_file_writes(&cmds);
        let errs = errors(&cmds);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].0, OP_READ);
        assert!(errs[0].1.contains(&io_description(kind)), "{kind:?}: {errs:?}");
        assert!(errs[0].1.contains("left unchanged"), "{errs:?}");

        assert_eq!(s.durable(), &FileView::Unreadable(kind));
        assert!(s.pending_snapshot().is_none());
        assert_eq!(s.writes_in_flight(), 0);

        let (engine, ecmds) = engine_startup(SessionFileRead::Unreadable(kind));
        assert_eq!(ecmds, cmds);
        assert!(engine.store().pending_snapshot().is_none());
    }
    // Each kind is described distinctly.
    let descriptions: std::collections::BTreeSet<_> = ALL_IO_KINDS.iter().map(|k| io_description(*k)).collect();
    assert_eq!(descriptions.len(), ALL_IO_KINDS.len());
    assert!(io_description(IoErrorKind::Other(1392)).contains("1392"));
}

// ---------------------------------------------------------------------------
// Startup: malformed (Req 11.7)
// ---------------------------------------------------------------------------

/// One malformed Session_File per parse error kind (plus an empty file and
/// a binary blob), all derived from a valid document.
fn malformed_inputs() -> Vec<(&'static str, Vec<u8>, ParseErrorKind)> {
    let text = serialize(&doc(Status::Pending, SaveCause::Shutdown, 0, &[2]));
    let mut bad_utf8 = text.clone().into_bytes();
    let i = text.find("display=dir0").unwrap() + "display=dir".len();
    bad_utf8[i] = 0xFF;
    vec![
        ("invalid UTF-8", bad_utf8, ParseErrorKind::InvalidUtf8),
        ("binary blob", vec![0x00, 0xFE, 0xFF, 0x80, 0x0A], ParseErrorKind::InvalidUtf8),
        ("unknown field", edit(&text, " monitor=", " monitxr=").into_bytes(), ParseErrorKind::Syntax),
        ("truncated", edit(&text, "end-session\n", "").into_bytes(), ParseErrorKind::MissingElement),
        ("empty", Vec::new(), ParseErrorKind::MissingElement),
        (
            "attempts out of range",
            edit(&text, "failed-attempts 0", "failed-attempts 9").into_bytes(),
            ParseErrorKind::OutOfRange,
        ),
        (
            "activation of an unknown window",
            edit(&text, "activation 0", "activation 5").into_bytes(),
            ParseErrorKind::InvalidSnapshot,
        ),
    ]
}

#[test]
fn malformed_file_is_preserved_byte_exact_and_logged_with_position_and_kind() {
    for (name, bytes, expected_kind) in malformed_inputs() {
        let err = parse(&bytes).expect_err(name);
        assert_eq!(err.kind, expected_kind, "{name}");

        let (mut s, cmds) = StoreLogic::startup(&SessionFileRead::Content(bytes.clone()));

        // One ERROR entry naming the line, column, and kind of the first problem.
        let errs = errors(&cmds);
        assert_eq!(errs.len(), 1, "{name}: {cmds:?}");
        assert_eq!(errs[0].0, OP_PARSE);
        let position = format!("line {}, column {}: {}", err.line, err.column, err.kind);
        assert!(errs[0].1.starts_with(&position), "{name}: {:?} vs {position:?}", errs[0].1);
        assert!(errs[0].1.contains("preserved"), "{name}: {errs:?}");

        // The single backup copy holds exactly the unreadable content.
        let b = backups(&cmds);
        assert_eq!(b.len(), 1, "{name}");
        assert_eq!(b[0].1, bytes, "{name}: backup is not byte-exact");
        assert_no_file_writes(&cmds);
        assert_eq!(cmds.len(), 2, "{name}: {cmds:?}");

        // No Pending_Snapshot, so no restoration at the next Trigger_Window.
        assert_eq!(s.durable(), &FileView::Malformed);
        assert!(s.pending_snapshot().is_none(), "{name}");
        assert!(s.begin_attempt().is_none(), "{name}");
        assert!(!s.is_protected());

        // A successful backup is silent.
        assert!(complete(&mut s, Millis(1), b[0].0, Ok(())).is_empty());
        assert_eq!(s.writes_in_flight(), 0);

        let (engine, ecmds) = engine_startup(SessionFileRead::Content(bytes));
        assert_eq!(ecmds, cmds, "{name}");
        assert!(engine.store().pending_snapshot().is_none());
    }
}

#[test]
fn malformed_file_is_replaced_by_the_next_save() {
    let (_, bytes, _) = malformed_inputs().remove(0);
    let (mut s, cmds) = StoreLogic::startup(&SessionFileRead::Content(bytes));
    let (backup_seq, _) = backups(&cmds)[0].clone();
    // Checkpointing works normally afterwards.
    let live = open(&[1, 3]);
    let (timer, at) = store_timer(&s.live_changed(Millis(100), &live)).unwrap();
    let (seq, text, urgent) = write(&fire(&mut s, at, timer, &live));
    assert!(!urgent);
    assert!(seq > backup_seq);
    let d = parse(text.as_bytes()).unwrap();
    assert_eq!((d.status, d.cause, d.failed_attempts), (Status::Pending, SaveCause::Checkpoint, 0));
    assert_eq!(d.snapshot, snapshot(&[1, 3]));
    assert!(complete(&mut s, at + 10, backup_seq, Ok(())).is_empty());
    assert!(complete(&mut s, at + 20, seq, Ok(())).is_empty());
    assert_eq!(s.durable(), &FileView::Doc(d));
}

#[test]
fn failed_backup_write_is_logged_and_not_retried() {
    let (_, bytes, _) = malformed_inputs().remove(2);
    for kind in ALL_IO_KINDS {
        let (mut s, cmds) = StoreLogic::startup(&SessionFileRead::Content(bytes.clone()));
        let (seq, _) = backups(&cmds)[0].clone();
        let cmds = complete(&mut s, Millis(10), seq, Err(kind));
        assert_eq!(errors(&cmds), vec![(OP_BACKUP.to_owned(), io_description(kind))], "{kind:?}");
        assert_eq!(cmds.len(), 1, "{cmds:?}");
        assert!(store_timer(&cmds).is_none());
        assert_eq!(s.retry_due(), None);
        assert!(!s.awaiting_change_to_retry());
        assert_eq!(s.durable(), &FileView::Malformed);
        assert!(s.pending_snapshot().is_none());
        // A repeated completion is ignored.
        assert!(complete(&mut s, Millis(11), seq, Err(kind)).is_empty());
        // The next live change does not write the backup again.
        assert_no_file_writes(&s.live_changed(Millis(20), &CLOSED));
    }
}

// ---------------------------------------------------------------------------
// Startup: newer format version (Req 11.7, 11.9)
// ---------------------------------------------------------------------------

#[test]
fn newer_version_is_preserved_and_logged_with_declared_and_newest_versions() {
    let current = serialize(&doc(Status::Pending, SaveCause::LastWindowClosed, 0, &[2, 1]));
    let header = format!("ESR-SESSION version={FORMAT_VERSION}");
    let inputs: Vec<(u32, Vec<u8>)> = vec![
        // Otherwise valid content.
        (FORMAT_VERSION + 1, edit(&current, &header, &format!("ESR-SESSION version={}", FORMAT_VERSION + 1)).into_bytes()),
        // A future grammar this parser cannot read.
        (7, b"ESR-SESSION version=7\nfuture-keyword a=b\n\xF0\x9F\x98\x80 more\n".to_vec()),
        (u32::MAX, format!("ESR-SESSION version={}\n", u32::MAX).into_bytes()),
    ];
    for (declared, bytes) in inputs {
        let err = parse(&bytes).expect_err("newer version");
        assert_eq!(err.kind, ParseErrorKind::UnsupportedVersion { declared, newest: FORMAT_VERSION });
        assert_eq!(err.line, 1);

        let (s, cmds) = StoreLogic::startup(&SessionFileRead::Content(bytes.clone()));
        let errs = errors(&cmds);
        assert_eq!(errs.len(), 1, "{cmds:?}");
        assert_eq!(errs[0].0, OP_PARSE);
        assert!(errs[0].1.starts_with(&err.to_string()), "{errs:?}");
        assert!(errs[0].1.contains(&format!("version {declared}")), "{errs:?}");
        assert!(errs[0].1.contains(&format!("newest supported version {FORMAT_VERSION}")), "{errs:?}");

        let b = backups(&cmds);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].1, bytes);
        assert_no_file_writes(&cmds);
        assert_eq!(s.durable(), &FileView::Malformed);
        assert!(s.pending_snapshot().is_none());
    }
}

// ---------------------------------------------------------------------------
// Startup: pending checkpoint and consumed snapshot
// ---------------------------------------------------------------------------

#[test]
fn pending_checkpoint_at_startup_is_logged_as_abrupt_end() {
    let d = doc(Status::Pending, SaveCause::Checkpoint, 0, &[3, 1, 2]);
    let expected = Command::AppendLog(LogEntry::Save { cause: SaveCause::Checkpoint, windows: 3, tabs: 6 });
    let (s, cmds) = StoreLogic::startup(&content(&d));
    assert_eq!(cmds, vec![expected.clone()]);
    assert_eq!(s.pending_snapshot(), Some(&d));
    assert_eq!(s.durable(), &FileView::Doc(d.clone()));
    assert!(!s.is_protected());

    let Command::AppendLog(entry) = &expected else { unreachable!() };
    let line = format_entry(&d.saved_at, entry);
    assert!(line.contains("SAVE") && line.contains("abrupt-end-detected-at-start"), "{line}");

    let (engine, ecmds) = engine_startup(content(&d));
    assert_eq!(ecmds, vec![expected]);
    assert_eq!(engine.store().pending_snapshot(), Some(&d));
}

#[test]
fn consumed_file_gives_no_pending_snapshot_and_no_log() {
    for cause in [SaveCause::Checkpoint, SaveCause::LastTabClosed, SaveCause::Shutdown] {
        for attempts in 0..=MAX_FAILED_ATTEMPTS {
            let d = doc(Status::Consumed, cause, attempts, &[2]);
            let (mut s, cmds) = StoreLogic::startup(&content(&d));
            assert!(cmds.is_empty(), "{cause:?}/{attempts}: {cmds:?}");
            assert_eq!(s.durable(), &FileView::Doc(d));
            assert!(s.pending_snapshot().is_none());
            assert!(!s.is_protected());
            assert!(s.begin_attempt().is_none());
            // Checkpointing is not suspended by a consumed snapshot.
            assert!(store_timer(&s.live_changed(Millis(0), &open(&[1]))).is_some());
        }
    }
}

// ---------------------------------------------------------------------------
// Retry schedule (Req 3.12)
// ---------------------------------------------------------------------------

/// Fails the write `seq` at `at` and checks the scheduled retry `n`.
/// Returns the retry timer and its due time.
fn fail_and_expect_retry(s: &mut StoreLogic, at: Millis, seq: u64, n: u8, kind: IoErrorKind) -> (u64, Millis) {
    let cmds = complete(s, at, seq, Err(kind));
    let errs = errors(&cmds);
    assert_eq!(errs.len(), 1, "{cmds:?}");
    assert_eq!(errs[0].0, OP_WRITE);
    assert!(errs[0].1.contains(&io_description(kind)), "{errs:?}");
    assert!(errs[0].1.contains(&format!("retry {n} of {MAX_RETRIES} in 2 s")), "{errs:?}");
    assert_no_file_writes(&cmds);
    assert_eq!(cmds.len(), 2, "{cmds:?}");
    let (timer, due) = store_timer(&cmds).expect("retry timer");
    assert_eq!(due, at + RETRY_INTERVAL_MS);
    assert_eq!(s.retry_due(), Some((due, n)));
    assert!(!s.awaiting_change_to_retry());
    (timer, due)
}

#[test]
fn retry_schedule_is_five_retries_two_seconds_apart_then_the_next_change() {
    assert_eq!((MAX_RETRIES, RETRY_INTERVAL_MS), (5, 2_000));

    let previous = doc(Status::Consumed, SaveCause::Restart, 0, &[1]);
    let (mut s, _) = StoreLogic::startup(&content(&previous));
    let (mut seq, text, _) = write(&s.session_end(Millis(1_000), None, SaveCause::Shutdown, snapshot(&[4, 2])));
    let saved = parse(text.as_bytes()).unwrap();

    // Time the write fails, time each retry is issued.
    let mut failed_at = Millis(1_030);
    let mut issued = Vec::new();
    let mut stale_timers = Vec::new();
    for n in 1..=MAX_RETRIES {
        let kind = ALL_IO_KINDS[n as usize % ALL_IO_KINDS.len()];
        let (timer, due) = fail_and_expect_retry(&mut s, failed_at, seq, n, kind);
        // Nothing happens before the retry is due: the previous content stays
        // on disk and the Live_Session stays in memory as the Pending_Snapshot.
        assert_eq!(s.durable(), &FileView::Doc(previous.clone()));
        assert_eq!(s.pending_snapshot(), Some(&saved));
        assert!(fire(&mut s, due, timer + 1_000, &CLOSED).is_empty());

        let cmds = fire(&mut s, due, timer, &CLOSED);
        let (next, retried, urgent) = write(&cmds);
        assert_eq!(retried, text, "retry {n} writes the same content");
        assert!(urgent, "a Session_End retry stays urgent");
        assert!(next > seq);
        assert_eq!(cmds.len(), 1);
        assert_eq!(s.retry_due(), None);
        issued.push(due);
        stale_timers.push(timer);
        seq = next;
        failed_at = due + 25;
    }
    // Exactly five retries, each issued 2 s after the failure before it.
    let intervals: Vec<u64> = std::iter::once(Millis(1_030))
        .chain(issued.iter().map(|d| *d + 25))
        .zip(&issued)
        .map(|(fail, due)| due.since(fail))
        .collect();
    assert_eq!(intervals, vec![RETRY_INTERVAL_MS; MAX_RETRIES as usize]);

    // The fifth retry fails too: logged, and no further retry is scheduled.
    let cmds = complete(&mut s, failed_at, seq, Err(IoErrorKind::DiskFull));
    let errs = errors(&cmds);
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0].0, OP_WRITE);
    assert!(errs[0].1.contains("all 5 retries failed"), "{errs:?}");
    assert!(store_timer(&cmds).is_none());
    assert_no_file_writes(&cmds);
    assert!(s.awaiting_change_to_retry());
    assert_eq!(s.retry_due(), None);
    assert_eq!(s.writes_in_flight(), 0);

    // No write while nothing changes, however much time passes or which old
    // timers fire.
    for (i, t) in stale_timers.iter().enumerate() {
        assert!(fire(&mut s, failed_at + 60_000 * (i as u64 + 1), *t, &CLOSED).is_empty());
    }
    assert!(s.handle(failed_at + 600_000, None, &Event::ExitRequested, &CLOSED).is_empty());
    assert!(s.awaiting_change_to_retry());
    assert_eq!(s.durable(), &FileView::Doc(previous));
    assert_eq!(s.pending_snapshot(), Some(&saved));

    // The next Live_Session change writes the same content again.
    let cmds = s.live_changed(failed_at + 700_000, &CLOSED);
    let (seq, retried, urgent) = write(&cmds);
    assert_eq!(retried, text);
    assert!(urgent);
    assert!(!s.awaiting_change_to_retry());
    assert!(complete(&mut s, failed_at + 700_050, seq, Ok(())).is_empty());
    assert_eq!(s.durable(), &FileView::Doc(saved));
    // Written once: a further change writes nothing (no windows open).
    assert!(s.live_changed(failed_at + 800_000, &CLOSED).is_empty());
}

#[test]
fn after_exhaustion_a_change_with_windows_open_rewrites_and_schedules_a_checkpoint() {
    let mut s = StoreLogic::startup(&SessionFileRead::Absent).0;
    let live = open(&[2]);
    let (timer, at) = store_timer(&s.live_changed(Millis(0), &live)).unwrap();
    let (mut seq, text, urgent) = write(&fire(&mut s, at, timer, &live));
    assert!(!urgent);
    let mut now = at + 10;
    for n in 1..=MAX_RETRIES {
        let (timer, due) = fail_and_expect_retry(&mut s, now, seq, n, IoErrorKind::SharingViolation);
        let (next, retried, urgent) = write(&fire(&mut s, due, timer, &live));
        assert_eq!(retried, text);
        assert!(!urgent, "a checkpoint retry stays non-urgent");
        seq = next;
        now = due + 10;
    }
    assert!(store_timer(&complete(&mut s, now, seq, Err(IoErrorKind::SharingViolation))).is_none());
    assert!(s.awaiting_change_to_retry());

    // The change writes the failed content at once and checkpoints the new state.
    let live = open(&[2, 5]);
    let cmds = s.live_changed(now + 30_000, &live);
    let (rewrite, retried, _) = write(&cmds);
    assert_eq!(retried, text);
    let (cp, cp_at) = store_timer(&cmds).expect("checkpoint");
    let (newest, cp_text, _) = write(&fire(&mut s, cp_at, cp, &live));
    assert!(newest > rewrite);
    assert_eq!(parse(cp_text.as_bytes()).unwrap().snapshot, snapshot(&[2, 5]));
    // The writer may skip the older queued write and report it Ok.
    assert!(complete(&mut s, cp_at + 10, newest, Ok(())).is_empty());
    assert!(complete(&mut s, cp_at + 11, rewrite, Ok(())).is_empty());
    assert_eq!(s.durable(), &FileView::Doc(parse(cp_text.as_bytes()).unwrap()));
}

#[test]
fn success_mid_way_stops_the_retries() {
    for succeed_on in 1..=MAX_RETRIES {
        let mut s = StoreLogic::startup(&SessionFileRead::Absent).0;
        let (mut seq, text, _) = write(&s.session_end(Millis(0), None, SaveCause::SignOut, snapshot(&[3])));
        let mut now = Millis(10);
        let mut timers = Vec::new();
        for n in 1..=succeed_on {
            let (timer, due) = fail_and_expect_retry(&mut s, now, seq, n, IoErrorKind::AccessDenied);
            timers.push(timer);
            seq = write(&fire(&mut s, due, timer, &CLOSED)).0;
            now = due + 10;
        }
        // Retry `succeed_on` lands: nothing logged, nothing scheduled.
        let cmds = complete(&mut s, now, seq, Ok(()));
        assert!(cmds.is_empty(), "{cmds:?}");
        assert_eq!(s.retry_due(), None);
        assert!(!s.awaiting_change_to_retry());
        assert_eq!(s.writes_in_flight(), 0);
        assert_eq!(s.durable(), &FileView::Doc(parse(text.as_bytes()).unwrap()));
        // No further write: neither from old timers nor from the next change.
        for t in timers {
            assert!(fire(&mut s, now + 2 * RETRY_INTERVAL_MS, t, &CLOSED).is_empty());
        }
        assert!(s.live_changed(now + 10_000, &CLOSED).is_empty());
    }
}

#[test]
fn a_newer_checkpoint_supersedes_a_pending_retry() {
    let mut s = StoreLogic::startup(&SessionFileRead::Absent).0;
    let live = open(&[1]);
    let (cp, at) = store_timer(&s.live_changed(Millis(0), &live)).unwrap();
    let (seq, _, _) = write(&fire(&mut s, at, cp, &live));
    let (retry_timer, retry_due) = fail_and_expect_retry(&mut s, at + 10, seq, 1, IoErrorKind::DiskFull);

    // A change while the retry is pending schedules a checkpoint that fires first.
    let live = open(&[1, 1]);
    let (cp, cp_at) = store_timer(&s.live_changed(at + 100, &live)).unwrap();
    assert!(cp_at < retry_due);
    let (newer, newer_text, _) = write(&fire(&mut s, cp_at, cp, &live));
    assert_eq!(s.retry_due(), None, "the newer write cancels the retry");
    assert!(fire(&mut s, retry_due, retry_timer, &live).is_empty());

    // A repeated completion of the failed write is ignored.
    let cmds = complete(&mut s, cp_at + 5, seq, Err(IoErrorKind::DiskFull));
    assert!(cmds.is_empty(), "already completed: {cmds:?}");
    assert_eq!(s.retry_due(), None);
    assert!(complete(&mut s, cp_at + 10, newer, Ok(())).is_empty());
    assert_eq!(s.durable(), &FileView::Doc(parse(newer_text.as_bytes()).unwrap()));
}

#[test]
fn a_session_end_supersedes_an_in_flight_retry() {
    let mut s = StoreLogic::startup(&SessionFileRead::Absent).0;
    let (seq, ..) = write(&s.session_end(Millis(0), None, SaveCause::LastWindowClosed, snapshot(&[1])));
    let (timer, due) = fail_and_expect_retry(&mut s, Millis(10), seq, 1, IoErrorKind::DiskFull);
    let (retry, ..) = write(&fire(&mut s, due, timer, &CLOSED));
    let (newer, text, _) = write(&s.session_end(due + 5, None, SaveCause::Restart, snapshot(&[2])));
    assert!(complete(&mut s, due + 10, newer, Ok(())).is_empty());

    // The retry fails after the newer write landed: logged, never retried.
    let cmds = complete(&mut s, due + 20, retry, Err(IoErrorKind::DiskFull));
    let errs = errors(&cmds);
    assert_eq!(errs.len(), 1, "{cmds:?}");
    assert_eq!(errs[0].0, OP_WRITE);
    assert!(errs[0].1.contains("superseded"), "{errs:?}");
    assert!(store_timer(&cmds).is_none());
    assert_eq!(s.retry_due(), None);
    assert!(!s.awaiting_change_to_retry());
    assert_eq!(s.durable(), &FileView::Doc(parse(text.as_bytes()).unwrap()));
}

#[test]
fn a_session_end_supersedes_an_exhausted_write() {
    let mut s = StoreLogic::startup(&SessionFileRead::Absent).0;
    let (mut seq, _, _) = write(&s.session_end(Millis(0), None, SaveCause::LastTabClosed, snapshot(&[1])));
    let mut now = Millis(10);
    for n in 1..=MAX_RETRIES {
        let (timer, due) = fail_and_expect_retry(&mut s, now, seq, n, IoErrorKind::Other(5));
        seq = write(&fire(&mut s, due, timer, &CLOSED)).0;
        now = due + 10;
    }
    complete(&mut s, now, seq, Err(IoErrorKind::Other(5)));
    assert!(s.awaiting_change_to_retry());

    let (seq, text, _) = write(&s.session_end(now + 50, None, SaveCause::LastWindowClosed, snapshot(&[2])));
    assert!(!s.awaiting_change_to_retry());
    // The next change does not resurrect the superseded content.
    assert!(s.live_changed(now + 60, &CLOSED).is_empty());
    assert!(complete(&mut s, now + 70, seq, Ok(())).is_empty());
    assert_eq!(s.durable(), &FileView::Doc(parse(text.as_bytes()).unwrap()));
}

// ---------------------------------------------------------------------------
// Through the Engine
// ---------------------------------------------------------------------------

#[test]
fn engine_routes_write_failures_and_retry_timers_to_the_store() {
    let (mut engine, cmds) = engine_startup(SessionFileRead::Absent);
    assert!(cmds.is_empty());
    let win = WinKey { hwnd: 0x10, seq: 1 };
    let tab = TabKey { hwnd: 0x20, seq: 2 };
    let scan = Event::WindowScanned(WindowObservation {
        win,
        pid: 42,
        tabs: vec![TabObservation { tab, location: ObservedLocation::Known(location(0)) }],
        active: Some(tab),
        strip: None,
        placement: placement(),
    });
    let cmds = engine.handle(Millis(100), scan);
    let (cp, at) = store_timer(&cmds).expect("checkpoint");
    let cmds = engine.handle(at, Event::Timer(TimerId { owner: TimerOwner::Store, seq: cp }));
    let (mut seq, text, _) = write(&cmds);

    let mut now = at + 20;
    for n in 1..=MAX_RETRIES {
        let cmds = engine.handle(now, Event::WriteCompleted { seq, result: Err(IoErrorKind::DiskFull) });
        let errs = errors(&cmds);
        assert_eq!(errs.len(), 1, "{cmds:?}");
        assert!(errs[0].1.contains(&format!("retry {n} of 5")), "{errs:?}");
        let (timer, due) = store_timer(&cmds).expect("retry timer");
        assert_eq!(due, now + RETRY_INTERVAL_MS);
        let cmds = engine.handle(due, Event::Timer(TimerId { owner: TimerOwner::Store, seq: timer }));
        let (next, retried, _) = write(&cmds);
        assert_eq!(retried, text);
        seq = next;
        now = due + 20;
    }
    let cmds = engine.handle(now, Event::WriteCompleted { seq, result: Err(IoErrorKind::DiskFull) });
    assert_eq!(errors(&cmds).len(), 1);
    assert!(store_timer(&cmds).is_none());
    assert!(engine.store().awaiting_change_to_retry());
    assert_eq!(engine.store().durable(), &FileView::Absent);
    assert!(engine.store().pending_snapshot().is_some());
}
