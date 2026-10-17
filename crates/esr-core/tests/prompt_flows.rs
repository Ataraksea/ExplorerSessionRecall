//! The Builtin_Restore_Option prompt flows and disabled restoration in the
//! `Engine` (task 11.6), end to end against the simulated Explorer and across
//! Session_Monitor starts. A "next start" is a new `Engine` built from the
//! Settings the previous run persisted (`PersistSettings`) and the
//! Session_File it last wrote. The simulator answers `SetBuiltinRestore` with
//! `BuiltinRestoreSet` (or an injected failure).
//!
//! The single-run details (exact command lists, decided-once logic) are unit
//! tests in `engine.rs`; trigger gating with restoration disabled at startup
//! is in `trigger_gating.rs`.
//!
//! Requirements 4.8, 4.9, 10.1, 10.2, 10.3, 10.5, 10.6, 10.7, 12.3.

use esr_core::engine::{
    Command, EndKind, Engine, Environment, Event, ExplorerOptionValues, IoErrorKind, MessageKind, Millis,
    PromptAnswer, PromptState, SessionFileRead, StartupInput, WinKey, OP_DISABLE_BUILTIN_RESTORE,
};
use esr_core::logfmt::entry::LogEntry;
use esr_core::model::{
    DisplayRef, LocationId, Placement, Rect, SaveCause, SessionDoc, ShowState, Snapshot, Status, TabRec, Timestamp,
    WindowId, WindowRec,
};
use esr_core::settings::Settings;
use esr_sim::explorer::{default_displays, default_locations, CloseOrder, NavOutcome, Op, Pick, SimConfig, SimExplorer};
use esr_sim::restore::{
    home_location, CommandKind, Driver, FailWhen, FailureRule, Launch, Origin, RecordedCommand, Stop, UserAction,
};

const DOCUMENTS: usize = 2;
const DOWNLOADS: usize = 3;
const PHOTOS: usize = 4;
const RECYCLE: usize = 11;

fn loc(i: usize) -> LocationId {
    default_locations()[i].clone()
}

fn locs(ids: &[usize]) -> Vec<LocationId> {
    ids.iter().map(|&i| loc(i)).collect()
}

fn rect(left: i32, top: i32) -> Rect {
    Rect { left, top, right: left + 800, bottom: top + 600 }
}

fn launch(location: LocationId) -> UserAction {
    UserAction::Launch(Launch { location, selection: vec![], rect: rect(300, 300), maximized: false })
}

fn open_tab(location: usize) -> UserAction {
    UserAction::Op(Op::OpenTab { win: Pick(0), location: Pick(location as u16), nav: NavOutcome::Complete })
}

fn close_window() -> UserAction {
    UserAction::Op(Op::CloseWindow {
        win: Pick(0),
        order: CloseOrder { revoke_before: 0, revoke_after: false, gap_ms: 0, rotate: Pick(0) },
    })
}

fn window(id: u32, tabs: &[usize], left: i32) -> WindowRec {
    let d = &default_displays()[usize::from(left >= 1920)];
    WindowRec {
        id: WindowId(id),
        tabs: tabs.iter().map(|&i| TabRec { location: loc(i) }).collect(),
        active: 0,
        placement: Placement {
            state: ShowState::Normal,
            restored: rect(left, 40),
            display: DisplayRef { device: d.device.clone(), monitor: d.monitor },
        },
    }
}

/// A Pending_Snapshot of two windows (Documents + Downloads, Photos).
fn pending_file() -> String {
    let windows = vec![window(1, &[DOCUMENTS, DOWNLOADS], 100), window(2, &[PHOTOS], 2000)];
    let activation = windows.iter().map(|w| w.id).collect();
    esr_core::format::serialize(&SessionDoc {
        version: esr_core::format::FORMAT_VERSION,
        status: Status::Pending,
        failed_attempts: 0,
        saved_at: Timestamp { year: 2025, month: 3, day: 4, hour: 5, minute: 6, second: 7, millis: 8, offset_minutes: 0 },
        cause: SaveCause::LastWindowClosed,
        snapshot: Snapshot { windows, activation },
    })
}

fn declined() -> Settings {
    Settings { builtin_prompt_declined: true, ..Settings::default() }
}

fn disabled() -> Settings {
    Settings { restoration_enabled: false, ..Settings::default() }
}

const WINDOW_COMMANDS: [CommandKind; 9] = [
    CommandKind::OpenWindow,
    CommandKind::ApplyPlacement,
    CommandKind::CreateTab,
    CommandKind::Navigate,
    CommandKind::CloseTab,
    CommandKind::ActivateTab,
    CommandKind::SelectItems,
    CommandKind::Reveal,
    CommandKind::Focus,
];

/// One Session_Monitor run: an Engine driving a fresh simulated Explorer.
struct Monitor {
    engine: Engine,
    driver: Driver,
    /// Settings the run started with.
    settings: Settings,
    /// Session_File content the run started with.
    file: Option<String>,
}

impl Monitor {
    /// Starts with `PersistBrowsers` read as `persist_browsers`.
    fn start(settings: Settings, persist_browsers: Option<bool>, file: Option<String>) -> Self {
        let startup = StartupInput {
            session_file: file.clone().map_or(SessionFileRead::Absent, |t| SessionFileRead::Content(t.into_bytes())),
            environment: Environment {
                displays: default_displays(),
                options: ExplorerOptionValues { persist_browsers, ..ExplorerOptionValues::default() },
                ..Environment::default()
            },
            // Not a logon start: the BuiltinRestore heuristic never applies.
            background_logon: false,
            logon_at: None,
            shell_ready_at: Some(Millis(0)),
        };
        let (engine, commands) = Engine::new(settings.clone(), startup);
        let mut driver = Driver::new(SimExplorer::new(SimConfig::for_restoration()));
        driver.submit(commands);
        let mut monitor = Monitor { engine, driver, settings, file };
        monitor.run();
        monitor
    }

    /// The next start: the Settings this run persisted and the Session_File
    /// it left, with `PersistBrowsers` read as `persist_browsers`.
    fn next_start(&self, persist_browsers: Option<bool>) -> Monitor {
        let file = self.driver.sim().session_file().map(str::to_owned).or_else(|| self.file.clone());
        Monitor::start(self.persisted(), persist_browsers, file)
    }

    fn run(&mut self) {
        let Monitor { engine, driver, .. } = self;
        assert_eq!(driver.run(|now, ev| engine.handle(now, ev)), Stop::Quiescent);
    }

    /// Plays `action` `ms` after the current time and runs to quiescence.
    fn act(&mut self, ms: u64, action: UserAction) {
        let at = self.driver.sim().now() + ms;
        self.driver.schedule(at, action);
        self.run();
    }

    /// Delivers `event` now and runs to quiescence.
    fn send(&mut self, event: Event) {
        self.driver.deliver_now(event);
        self.run();
    }

    fn answer(&mut self, answer: PromptAnswer) {
        self.send(Event::PromptAnswered(answer));
    }

    fn commands(&self, kind: CommandKind) -> Vec<&RecordedCommand> {
        self.driver.sim().commands_of(kind)
    }

    fn count(&self, kind: CommandKind) -> usize {
        self.commands(kind).len()
    }

    /// Settings as stored after this run.
    fn persisted(&self) -> Settings {
        self.commands(CommandKind::PersistSettings)
            .last()
            .map(|c| match &c.command {
                Command::PersistSettings(s) => s.clone(),
                _ => unreachable!(),
            })
            .unwrap_or_else(|| self.settings.clone())
    }

    fn writes(&self) -> Vec<SessionDoc> {
        self.commands(CommandKind::WriteSessionFile)
            .iter()
            .map(|c| match &c.command {
                Command::WriteSessionFile { text, .. } => esr_core::format::parse(text.as_bytes()).unwrap(),
                _ => unreachable!(),
            })
            .collect()
    }

    /// The Session_File as last written successfully.
    fn file_doc(&self) -> Option<SessionDoc> {
        self.driver.sim().session_file().map(|t| esr_core::format::parse(t.as_bytes()).unwrap())
    }

    fn logs(&self) -> Vec<&LogEntry> {
        self.driver.sim().log_entries().iter().map(|(_, e)| e).collect()
    }

    fn no_window_commands(&self) -> bool {
        WINDOW_COMMANDS.iter().all(|k| self.count(*k) == 0)
    }

    fn user_windows(&self) -> Vec<WinKey> {
        let records = self.driver.sim().window_records();
        records.iter().filter(|r| matches!(r.origin, Origin::User)).map(|r| r.key).collect()
    }

    /// Locations of `win`'s tabs, left to right.
    fn shown(&self, win: WinKey) -> Vec<LocationId> {
        let w = self.driver.sim().window(win).expect("open");
        w.tabs.iter().map(|t| t.last_known.clone().expect("navigated")).collect()
    }

    fn persist_browsers(&self) -> Option<bool> {
        self.engine.environment().options.persist_browsers
    }

    /// The Environment with `PersistBrowsers` read as `value`.
    fn environment_with(&self, value: Option<bool>) -> Environment {
        let mut env = self.engine.environment().clone();
        env.options.persist_browsers = value;
        env
    }
}

fn disable_failed_logged(entry: &LogEntry) -> bool {
    matches!(entry, LogEntry::Error { operation, .. } if operation == OP_DISABLE_BUILTIN_RESTORE)
}

// ---------------------------------------------------------------------------
// Answer paths (Req 10.1-10.3, 10.5, 10.6)
// ---------------------------------------------------------------------------

#[test]
fn accepting_disables_the_option_and_the_next_start_does_not_prompt() {
    let mut m = Monitor::start(Settings::default(), Some(true), None);
    assert_eq!(m.count(CommandKind::ShowPrompt), 1);
    assert_eq!(m.engine.prompt_state(), PromptState::Shown);

    m.answer(PromptAnswer::Accepted);
    let set = m.commands(CommandKind::SetBuiltinRestore);
    assert_eq!(set.len(), 1);
    assert_eq!(set[0].command, Command::SetBuiltinRestore(false));
    assert!(!set[0].failed);
    // The shell's success report updates the Engine's reading; nothing is
    // recorded in Settings and no message is shown.
    assert_eq!(m.persist_browsers(), Some(false));
    assert_eq!(m.engine.prompt_state(), PromptState::Done);
    assert_eq!(m.count(CommandKind::PersistSettings), 0);
    assert_eq!(m.count(CommandKind::ShowMessage), 0);
    assert!(m.logs().is_empty(), "{:?}", m.logs());

    // The option now reads as disabled: no prompt at the next start.
    let next = m.next_start(m.persist_browsers());
    assert_eq!(next.count(CommandKind::ShowPrompt), 0);
    assert_eq!(next.engine.prompt_state(), PromptState::Done);
}

#[test]
fn declining_keeps_the_option_and_no_later_start_prompts() {
    let mut m = Monitor::start(Settings::default(), Some(true), None);
    m.answer(PromptAnswer::Declined);
    assert_eq!(m.count(CommandKind::SetBuiltinRestore), 0, "the option is left enabled");
    assert_eq!(m.persist_browsers(), Some(true));
    assert_eq!(m.persisted(), declined());

    // No prompt at any later start while the option stays enabled.
    for _ in 0..3 {
        m = m.next_start(Some(true));
        assert_eq!(m.count(CommandKind::ShowPrompt), 0);
        assert_eq!(m.engine.prompt_state(), PromptState::Done);
        assert_eq!(m.persisted(), declined());
    }
}

#[test]
fn dismissing_records_nothing_and_the_next_start_prompts_again() {
    let mut m = Monitor::start(Settings::default(), Some(true), None);
    m.answer(PromptAnswer::Dismissed);
    assert_eq!(m.count(CommandKind::SetBuiltinRestore), 0);
    assert_eq!(m.count(CommandKind::PersistSettings), 0);
    assert_eq!(m.persist_browsers(), Some(true));
    assert_eq!(m.engine.prompt_state(), PromptState::Done);
    // Not again in this run, even when the environment is read again.
    m.send(Event::EnvironmentChanged(m.environment_with(Some(true))));
    assert_eq!(m.count(CommandKind::ShowPrompt), 1);

    let mut next = m.next_start(Some(true));
    assert_eq!(next.count(CommandKind::ShowPrompt), 1);
    assert_eq!(next.engine.prompt_state(), PromptState::Shown);
    // Dismissed again: still asked the time after.
    next.answer(PromptAnswer::Dismissed);
    assert_eq!(next.next_start(Some(true)).count(CommandKind::ShowPrompt), 1);
}

#[test]
fn a_failed_disable_is_reported_and_the_next_start_prompts_again() {
    let mut m = Monitor::start(Settings::default(), Some(true), None);
    let rule = FailureRule { io_error: IoErrorKind::AccessDenied, ..FailureRule::times(FailWhen::Kind(CommandKind::SetBuiltinRestore), 1) };
    m.driver.sim_mut().fail(rule);
    m.answer(PromptAnswer::Accepted);

    let set = m.commands(CommandKind::SetBuiltinRestore);
    assert_eq!(set.len(), 1, "no retry within the run");
    assert!(set[0].failed);
    // An error message and a timestamped ERROR entry (Req 10.5).
    let messages = m.commands(CommandKind::ShowMessage);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].command, Command::ShowMessage(MessageKind::BuiltinRestoreDisableFailed));
    let errors: Vec<_> = m.driver.sim().log_entries().iter().filter(|(_, e)| disable_failed_logged(e)).collect();
    assert_eq!(errors.len(), 1, "{:?}", m.logs());
    assert!(errors[0].0 >= set[0].at);
    // The option is unchanged and no decision is recorded.
    assert_eq!(m.persist_browsers(), Some(true));
    assert_eq!(m.count(CommandKind::PersistSettings), 0);
    assert_eq!(m.persisted(), Settings::default());
    // No second prompt in this run.
    m.send(Event::EnvironmentChanged(m.environment_with(Some(true))));
    assert_eq!(m.count(CommandKind::ShowPrompt), 1);

    // The next start (option still enabled) prompts again; accepting now
    // succeeds.
    let mut next = m.next_start(Some(true));
    assert_eq!(next.count(CommandKind::ShowPrompt), 1);
    next.answer(PromptAnswer::Accepted);
    assert!(next.commands(CommandKind::SetBuiltinRestore).iter().all(|c| !c.failed));
    assert_eq!(next.persist_browsers(), Some(false));
    assert_eq!(next.count(CommandKind::ShowMessage), 0);
    assert!(!next.logs().into_iter().any(disable_failed_logged));
    assert_eq!(next.next_start(next.persist_browsers()).count(CommandKind::ShowPrompt), 0);
}

// ---------------------------------------------------------------------------
// Re-enabling the prompt (Req 12.3)
// ---------------------------------------------------------------------------

#[test]
fn re_enabling_the_prompt_shows_it_at_the_next_start_with_the_option_enabled() {
    let mut m = Monitor::start(Settings::default(), Some(true), None);
    m.answer(PromptAnswer::Declined);
    let mut m = m.next_start(Some(true));
    assert_eq!(m.count(CommandKind::ShowPrompt), 0);

    // The user re-enables the prompt in Settings: persisted at once, shown
    // only at a later start.
    m.send(Event::SettingsChanged(Settings::default()));
    assert_eq!(m.persisted(), Settings::default());
    assert_eq!(m.count(CommandKind::ShowPrompt), 0);

    // A start with the option disabled does not prompt, and does not use
    // up the re-enabled prompt either.
    let m = m.next_start(Some(false));
    assert_eq!(m.count(CommandKind::ShowPrompt), 0);
    assert_eq!(m.persisted(), Settings::default());

    // The next start at which the option is enabled prompts.
    let mut m = m.next_start(Some(true));
    assert_eq!(m.count(CommandKind::ShowPrompt), 1);
    // Declining once more records the decline again.
    m.answer(PromptAnswer::Declined);
    assert_eq!(m.next_start(Some(true)).count(CommandKind::ShowPrompt), 0);
}

// ---------------------------------------------------------------------------
// Tracking while the prompt is shown (Req 10.7)
// ---------------------------------------------------------------------------

#[test]
fn tracking_checkpoints_and_session_end_continue_while_the_prompt_is_unanswered() {
    let mut m = Monitor::start(Settings::default(), Some(true), None);
    assert_eq!(m.engine.prompt_state(), PromptState::Shown);

    m.act(1_000, launch(loc(DOCUMENTS)));
    m.act(1_000, open_tab(DOWNLOADS));
    let win = m.user_windows()[0];
    assert_eq!(m.shown(win), locs(&[DOCUMENTS, DOWNLOADS]));
    // The Live_Session follows, and a checkpoint records it.
    let live = m.engine.tracker().capture().0;
    assert_eq!((live.windows.len(), live.tab_count()), (1, 2));
    assert!(
        m.writes().iter().any(|d| d.cause == SaveCause::Checkpoint && d.snapshot.tab_count() == 2),
        "{:?}",
        m.writes()
    );

    // Session_End: the last window closes while the prompt is still shown.
    m.act(1_000, close_window());
    let doc = m.file_doc().expect("saved");
    assert_eq!((doc.status, doc.cause), (Status::Pending, SaveCause::LastWindowClosed));
    assert_eq!(doc.snapshot.windows.len(), 1);
    assert_eq!(doc.snapshot.windows[0].tabs.iter().map(|t| t.location.clone()).collect::<Vec<_>>(), locs(&[DOCUMENTS, DOWNLOADS]));
    assert!(m.logs().iter().any(|e| matches!(e, LogEntry::Save { cause: SaveCause::LastWindowClosed, windows: 1, tabs: 2 })));
    assert_eq!(m.engine.prompt_state(), PromptState::Shown);
    assert_eq!(m.count(CommandKind::SetBuiltinRestore) + m.count(CommandKind::PersistSettings), 0);

    // A late answer still works.
    m.answer(PromptAnswer::Accepted);
    assert_eq!(m.persist_browsers(), Some(false));

    // The saved session is restored at the next start.
    let mut next = m.next_start(m.persist_browsers());
    next.act(1_000, launch(home_location()));
    let trigger = next.user_windows()[0];
    assert!(next.engine.restore_session().is_some_and(|s| s.is_finished() && s.trigger() == trigger));
    assert_eq!(next.shown(trigger), locs(&[DOCUMENTS, DOWNLOADS]));
}

#[test]
fn a_restoration_runs_while_the_prompt_is_unanswered() {
    let mut m = Monitor::start(Settings::default(), Some(true), Some(pending_file()));
    assert_eq!(m.count(CommandKind::ShowPrompt), 1);
    m.act(1_000, launch(home_location()));
    let trigger = m.user_windows()[0];
    assert!(m.engine.restore_session().is_some_and(|s| s.is_finished() && s.trigger() == trigger));
    assert_eq!(m.shown(trigger), locs(&[DOCUMENTS, DOWNLOADS]));
    assert_eq!(m.engine.tracker().window_count(), 2);
    // Consumed at completion; later checkpoints record the restored windows.
    assert!(m.writes().iter().any(|d| d.status == Status::Consumed));
    assert!(m.file_doc().is_some_and(|d| d.status == Status::Consumed || d.cause == SaveCause::Checkpoint));
    assert_eq!(m.engine.prompt_state(), PromptState::Shown);
}

// ---------------------------------------------------------------------------
// Restoration disabled (Req 4.8, 4.9)
// ---------------------------------------------------------------------------

#[test]
fn restoration_disabled_during_the_run_consumes_the_snapshot_at_the_trigger() {
    let mut m = Monitor::start(Settings::default(), Some(false), Some(pending_file()));
    // The user turns restoration off; it applies at once.
    m.send(Event::SettingsChanged(disabled()));
    assert_eq!(m.persisted(), disabled());

    // A Targeted_Launch: the window is left as it opened.
    m.act(1_000, launch(loc(RECYCLE)));
    let win = m.user_windows()[0];
    assert!(m.engine.restore_session().is_none());
    assert!(m.no_window_commands(), "{:?}", m.driver.sim().command_log());
    assert_eq!(m.shown(win), locs(&[RECYCLE]));
    let placement = m.driver.sim().window(win).unwrap().placement.restored;
    assert_eq!(placement, rect(300, 300));
    // The Pending_Snapshot is consumed, without an attempt being counted.
    let writes = m.writes();
    assert!(writes.iter().all(|d| d.failed_attempts == 0));
    let consumed = writes.iter().find(|d| d.status == Status::Consumed).expect("consumed");
    assert_eq!((consumed.snapshot.windows.len(), consumed.snapshot.tab_count()), (2, 3));
    // From then on only the Live_Session is checkpointed: the old snapshot
    // cannot come back, even with restoration turned on again.
    for doc in writes.iter().skip_while(|d| d.status != Status::Consumed).skip(1) {
        assert_eq!(doc.cause, SaveCause::Checkpoint);
        assert_eq!(doc.snapshot.windows.len(), 1);
        assert_eq!(doc.snapshot.windows[0].tabs[0].location, loc(RECYCLE));
    }
    assert!(m.engine.store().pending_snapshot().is_none_or(|d| d.cause == SaveCause::Checkpoint));
}

#[test]
fn session_ends_are_saved_while_restoration_is_disabled() {
    let mut m = Monitor::start(disabled(), Some(false), None);
    m.act(1_000, launch(loc(DOCUMENTS)));
    m.act(1_000, open_tab(PHOTOS));
    m.act(1_000, close_window());
    let doc = m.file_doc().expect("saved");
    assert_eq!((doc.status, doc.cause, doc.failed_attempts), (Status::Pending, SaveCause::LastWindowClosed, 0));
    assert_eq!(doc.snapshot.tab_count(), 2);
    assert!(m.engine.store().pending_snapshot().is_some());

    // After restoration is re-enabled, the next start restores that session.
    let mut next = m.next_start(Some(false));
    next.send(Event::SettingsChanged(Settings::default()));
    next.act(1_000, launch(home_location()));
    let trigger = next.user_windows()[0];
    assert!(next.engine.restore_session().is_some_and(|s| s.is_finished()));
    assert_eq!(next.shown(trigger), locs(&[DOCUMENTS, PHOTOS]));

    // A Windows shutdown while disabled is saved too.
    let mut m = Monitor::start(disabled(), Some(false), None);
    m.act(1_000, launch(loc(DOWNLOADS)));
    m.send(Event::EndSessionQuery(EndKind::Shutdown));
    let doc = m.file_doc().expect("saved");
    assert_eq!((doc.status, doc.cause), (Status::Pending, SaveCause::Shutdown));
    assert_eq!(doc.snapshot.tab_count(), 1);
    assert!(m.commands(CommandKind::WriteSessionFile).iter().any(|c| matches!(c.command, Command::WriteSessionFile { urgent: true, .. })));
}

// ---------------------------------------------------------------------------
// No prompt (Req 10.1)
// ---------------------------------------------------------------------------

#[test]
fn no_prompt_when_the_option_is_off_or_unknown() {
    // Off at startup.
    let mut m = Monitor::start(Settings::default(), Some(false), None);
    assert_eq!(m.engine.prompt_state(), PromptState::Done);
    m.act(1_000, launch(loc(DOCUMENTS)));
    m.act(1_000, close_window());
    assert_eq!(m.count(CommandKind::ShowPrompt), 0);
    assert!(m.file_doc().is_some(), "tracking and saving as usual");

    // Unknown at startup and still unknown when the environment is first
    // read: decided without a prompt.
    let mut m = Monitor::start(Settings::default(), None, None);
    assert_eq!(m.engine.prompt_state(), PromptState::Undecided);
    assert_eq!(m.count(CommandKind::ShowPrompt), 0);
    m.act(1_000, launch(loc(DOCUMENTS)));
    m.send(Event::EnvironmentChanged(m.environment_with(None)));
    assert_eq!(m.engine.prompt_state(), PromptState::Done);
    assert_eq!(m.count(CommandKind::ShowPrompt), 0);

    // Unknown at startup, off when first read: the decision is made then;
    // the option being switched on later in the run is not a start.
    let mut m = Monitor::start(Settings::default(), None, None);
    m.send(Event::EnvironmentChanged(m.environment_with(Some(false))));
    assert_eq!(m.engine.prompt_state(), PromptState::Done);
    m.send(Event::EnvironmentChanged(m.environment_with(Some(true))));
    assert_eq!(m.count(CommandKind::ShowPrompt), 0);
    // It is asked at the next start.
    assert_eq!(m.next_start(Some(true)).count(CommandKind::ShowPrompt), 1);

    // Unknown at startup, on when first read: shown once.
    let mut m = Monitor::start(Settings::default(), None, None);
    m.send(Event::EnvironmentChanged(m.environment_with(Some(true))));
    m.send(Event::EnvironmentChanged(m.environment_with(Some(true))));
    assert_eq!(m.count(CommandKind::ShowPrompt), 1);
    assert_eq!(m.engine.prompt_state(), PromptState::Shown);
}
