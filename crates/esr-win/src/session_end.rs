//! `SessionEndListener`: end-kind detection for `WM_QUERYENDSESSION` /
//! `WM_ENDSESSION`.
//!
//! Requirements 3.5, 3.6, 13.1. See design.md, "Sign-out, shutdown,
//! restart".
//!
//! The hidden top-level window and its message loop belong to the Engine
//! thread (runtime, task 14.6). This module provides the pieces that thread
//! composes:
//!
//! * [`configure_shutdown_priority`]: call once at start so `esr.exe`
//!   receives `WM_QUERYENDSESSION` before Explorer closes its windows.
//! * [`EndSessionMessage::from_msg`] / [`SessionEndListener::on_message`]:
//!   decode the two messages in the window procedure.
//! * [`classify_end_session`]: derive the [`EndKind`] from `lParam` and the
//!   newest System-log event 1074, read by [`read_newest_1074`].
//!
//! The window must be a real top-level window (hidden, not `HWND_MESSAGE`),
//! because message-only windows do not receive `WM_QUERYENDSESSION`. On
//! `WM_QUERYENDSESSION` the runtime freezes and saves synchronously (blocking
//! ≤ 1.5 s) and then returns [`QUERY_END_SESSION_ALLOW`]. This module never
//! calls `ShutdownBlockReasonCreate`, and neither may the runtime, so Windows
//! shows no "app is preventing shutdown" message. `WM_ENDSESSION` with
//! `wParam == FALSE` means the session end was cancelled.
//!
//! Nothing here writes to an event log, simulates a shutdown, or calls
//! `ExitWindowsEx`.

use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use esr_core::engine::EndKind;
use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, LRESULT};
use windows::Win32::System::EventLog::{
    EVT_HANDLE, EvtClose, EvtNext, EvtQuery, EvtQueryChannelPath, EvtQueryReverseDirection,
    EvtRender, EvtRenderEventXml,
};
use windows::Win32::System::Threading::SetProcessShutdownParameters;
use windows::Win32::UI::WindowsAndMessaging::{
    ENDSESSION_LOGOFF, WM_ENDSESSION, WM_QUERYENDSESSION,
};
use windows::core::w;

use crate::strategy::{EVENT_1074_MAX_AGE_MS, EVENT_1074_QUERY_BUDGET_MS, EndKindSource};

/// Shutdown level passed to `SetProcessShutdownParameters`: the highest
/// level of the application range (0x100–0x3FF). Higher levels are asked
/// first, so this process is asked before Explorer closes its windows.
pub const SHUTDOWN_LEVEL: u32 = 0x3FF;

/// `SHUTDOWN_NORETRY` (winbase.h). Declared here to avoid pulling in the
/// `Win32_System_WindowsProgramming` feature for one constant.
pub const SHUTDOWN_NORETRY: u32 = 0x1;

/// Value the window procedure returns for `WM_QUERYENDSESSION` after the
/// synchronous save: `TRUE`, the session may end.
pub const QUERY_END_SESSION_ALLOW: LRESULT = LRESULT(1);

/// Default time budget for [`read_newest_1074`].
pub const EVENT_1074_QUERY_BUDGET: Duration = Duration::from_millis(EVENT_1074_QUERY_BUDGET_MS);

/// Makes this process receive `WM_QUERYENDSESSION` before Explorer closes
/// its windows (`SetProcessShutdownParameters(0x3FF, SHUTDOWN_NORETRY)`).
pub fn configure_shutdown_priority() -> windows::core::Result<()> {
    // SAFETY: plain Win32 call with value arguments.
    unsafe { SetProcessShutdownParameters(SHUTDOWN_LEVEL, SHUTDOWN_NORETRY) }
}

// ---------------------------------------------------------------------------
// Window-procedure helpers
// ---------------------------------------------------------------------------

/// A decoded session-end window message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndSessionMessage {
    /// `WM_QUERYENDSESSION`: freeze, save, then return
    /// [`QUERY_END_SESSION_ALLOW`].
    Query(EndKind),
    /// `WM_ENDSESSION`. `ending == false` means the session end was
    /// cancelled (unfreeze and rescan).
    Ended { ending: bool },
}

impl EndSessionMessage {
    /// Decodes `msg` with the default strategy ([`EndKindSource::DEFAULT`])
    /// and the default event-1074 budget. Returns `None` for other messages.
    pub fn from_msg(msg: u32, wparam: usize, lparam: isize) -> Option<Self> {
        SessionEndListener::default().on_message(msg, wparam, lparam)
    }

    /// Decodes `msg`, using `classify` to derive the kind from `lParam` of a
    /// `WM_QUERYENDSESSION`. `classify` is not called for other messages.
    pub fn from_msg_with(
        msg: u32,
        wparam: usize,
        lparam: isize,
        classify: impl FnOnce(isize) -> EndKind,
    ) -> Option<Self> {
        match msg {
            WM_QUERYENDSESSION => Some(Self::Query(classify(lparam))),
            WM_ENDSESSION => Some(Self::Ended {
                ending: wparam != 0,
            }),
            _ => None,
        }
    }
}

/// End-kind detection configured with an [`EndKindSource`] and a query
/// budget. The Engine thread's window procedure calls
/// [`on_message`](Self::on_message).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionEndListener {
    pub source: EndKindSource,
    pub budget: Duration,
}

impl Default for SessionEndListener {
    fn default() -> Self {
        Self::new(EndKindSource::DEFAULT)
    }
}

impl SessionEndListener {
    pub fn new(source: EndKindSource) -> Self {
        Self {
            source,
            budget: EVENT_1074_QUERY_BUDGET,
        }
    }

    /// Derives the kind of a `WM_QUERYENDSESSION` with this `lParam`, reading
    /// event 1074 live if needed.
    pub fn classify(&self, lparam: isize) -> EndKind {
        let budget = self.budget;
        classify_end_session(
            lparam,
            || read_newest_1074(budget),
            SystemTime::now(),
            self.source,
        )
    }

    /// Decodes a window message; `None` if it is not a session-end message.
    pub fn on_message(&self, msg: u32, wparam: usize, lparam: isize) -> Option<EndSessionMessage> {
        EndSessionMessage::from_msg_with(msg, wparam, lparam, |lp| self.classify(lp))
    }
}

// ---------------------------------------------------------------------------
// End-kind classification (pure apart from the injected reader)
// ---------------------------------------------------------------------------

/// Whether `lParam` of `WM_QUERYENDSESSION`/`WM_ENDSESSION` has
/// `ENDSESSION_LOGOFF` set.
pub fn is_logoff(lparam: isize) -> bool {
    (lparam as usize) & (ENDSESSION_LOGOFF as usize) != 0
}

/// Derives the [`EndKind`] of a `WM_QUERYENDSESSION`.
///
/// * `ENDSESSION_LOGOFF` in `lparam` → `SignOut` (the reader is not called).
/// * `EndKindSource::AssumeShutdown` → `Shutdown` (the reader is not called).
/// * Otherwise `read_1074` supplies the newest event 1074. A restart event
///   no older than [`EVENT_1074_MAX_AGE_MS`] relative to `now_utc` →
///   `Restart`. Anything else (no event, missing or unparsable timestamp,
///   stale, power off, unknown language) → `Shutdown`.
pub fn classify_end_session(
    lparam: isize,
    read_1074: impl FnOnce() -> Option<Event1074>,
    now_utc: SystemTime,
    strategy: EndKindSource,
) -> EndKind {
    if is_logoff(lparam) {
        return EndKind::SignOut;
    }
    match strategy {
        EndKindSource::AssumeShutdown => EndKind::Shutdown,
        EndKindSource::Event1074 => {
            let Some(event) = read_1074() else {
                return EndKind::Shutdown;
            };
            let fresh = event
                .time_created
                .as_deref()
                .and_then(parse_system_time_ms)
                .is_some_and(|created| is_fresh(created, unix_ms(now_utc), EVENT_1074_MAX_AGE_MS));
            match (fresh, event.kind) {
                (true, ShutdownKind::Restart) => EndKind::Restart,
                _ => EndKind::Shutdown,
            }
        }
    }
}

/// Whether an event created at `created_ms` is at most `max_age_ms` away
/// from `now_ms` (both milliseconds since the Unix epoch, UTC). A timestamp
/// further in the future than `max_age_ms` is rejected as well.
pub fn is_fresh(created_ms: i64, now_ms: i64, max_age_ms: u64) -> bool {
    now_ms.abs_diff(created_ms) <= max_age_ms
}

/// Milliseconds since the Unix epoch (negative before it).
fn unix_ms(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(e) => -(e.duration().as_millis() as i64),
    }
}

/// Parses an event-log `SystemTime` (`YYYY-MM-DDTHH:MM:SS[.fraction]Z`, UTC)
/// into milliseconds since the Unix epoch. Digits beyond milliseconds are
/// truncated. Returns `None` for anything else.
pub fn parse_system_time_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    let s = s.strip_suffix('Z').or_else(|| s.strip_suffix("+00:00"))?;
    let (date, time) = s.split_once('T')?;
    let (time, fraction) = match time.split_once('.') {
        Some((t, f)) => (t, Some(f)),
        None => (time, None),
    };

    let num = |part: &str, len: usize| -> Option<i64> {
        (part.len() == len && part.bytes().all(|b| b.is_ascii_digit()))
            .then(|| part.parse().ok())
            .flatten()
    };
    let mut d = date.split('-');
    let (year, month, day) = (num(d.next()?, 4)?, num(d.next()?, 2)?, num(d.next()?, 2)?);
    let mut t = time.split(':');
    let (hour, minute, second) = (num(t.next()?, 2)?, num(t.next()?, 2)?, num(t.next()?, 2)?);
    if d.next().is_some() || t.next().is_some() {
        return None;
    }
    if !(1..=12).contains(&month)
        || day < 1
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let millis = match fraction {
        None => 0,
        Some(f) if !f.is_empty() && f.bytes().all(|b| b.is_ascii_digit()) => {
            let mut ms = 0i64;
            for i in 0..3 {
                ms = ms * 10 + f.as_bytes().get(i).map_or(0, |b| i64::from(b - b'0'));
            }
            ms
        }
        Some(_) => return None,
    };
    let days = days_from_civil(year, month, day);
    Some(((days * 24 + hour) * 60 + minute) * 60_000 + second * 1000 + millis)
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (H. Hinnant's
/// `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// ---------------------------------------------------------------------------
// Event 1074 parsing (lifted from spike R7)
// ---------------------------------------------------------------------------

/// Kind of shutdown recorded by a User32 1074 event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownKind {
    Restart,
    PowerOff,
    /// The shutdown-type field was missing or not recognised (e.g. an unknown
    /// UI language). Callers fall back to "Windows shutdown".
    Unknown,
}

/// Fields of interest from a rendered 1074 event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event1074 {
    /// `System/TimeCreated/@SystemTime` (UTC, ISO 8601), used to reject a
    /// stale event left over from an earlier shutdown.
    pub time_created: Option<String>,
    /// Raw `param5` (shutdown type) text as logged; it is localized.
    pub shutdown_type: Option<String>,
    pub kind: ShutdownKind,
}

/// Maps the (localized) `param5` text of event 1074 to a kind. Matching is
/// case-insensitive; unknown languages yield `Unknown`.
pub fn classify_shutdown_type(text: &str) -> ShutdownKind {
    let t = text.trim().to_lowercase();
    const RESTART: &[&str] = &[
        "restart",
        "neu starten",
        "neustart",
        "redémarrer",
        "reiniciar",
        "riavvia",
    ];
    const POWER_OFF: &[&str] = &[
        "power off",
        "shutdown",
        "shut down",
        "ausschalten",
        "herunterfahren",
        "arrêter",
        "apagar",
        "arresta",
    ];
    if RESTART.iter().any(|k| t.contains(k)) {
        ShutdownKind::Restart
    } else if POWER_OFF.iter().any(|k| t.contains(k)) {
        ShutdownKind::PowerOff
    } else {
        ShutdownKind::Unknown
    }
}

/// Parses the XML produced by `EvtRender(EvtRenderEventXml)` for a 1074 event.
/// Returns `None` if the XML is not a 1074 event.
pub fn parse_event1074_xml(xml: &str) -> Option<Event1074> {
    let event_id = element_text(xml, "EventID")?;
    if event_id.trim() != "1074" {
        return None;
    }
    let time_created = find_tag(xml, "TimeCreated").and_then(|tag| attr_value(tag, "SystemTime"));
    // Prefer the named field; fall back to the fifth positional <Data>.
    let shutdown_type = data_named(xml, "param5").or_else(|| data_nth(xml, 4));
    let kind = shutdown_type
        .as_deref()
        .map_or(ShutdownKind::Unknown, classify_shutdown_type);
    Some(Event1074 {
        time_created,
        shutdown_type,
        kind,
    })
}

/// Returns the opening tag text (`<Name ...>` or `<Name .../>`) of the first `<Name` element.
fn find_tag<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let mut rest = xml;
    loop {
        let start = rest.find(&format!("<{name}"))?;
        let after = &rest[start + 1 + name.len()..];
        // Make sure the match is the whole element name (not a prefix).
        if after.starts_with([' ', '>', '/', '\t', '\r', '\n']) {
            let end = after.find('>')?;
            return Some(&rest[start..start + 1 + name.len() + end + 1]);
        }
        rest = after;
    }
}

fn attr_value(tag: &str, attr: &str) -> Option<String> {
    for quote in ['\'', '"'] {
        let needle = format!("{attr}={quote}");
        if let Some(i) = tag.find(&needle) {
            let v = &tag[i + needle.len()..];
            let end = v.find(quote)?;
            return Some(unescape(&v[..end]));
        }
    }
    None
}

fn element_text(xml: &str, name: &str) -> Option<String> {
    let tag = find_tag(xml, name)?;
    if tag.ends_with("/>") {
        return Some(String::new());
    }
    let start = xml.find(tag)? + tag.len();
    let end = xml[start..].find(&format!("</{name}>"))?;
    Some(unescape(&xml[start..start + end]))
}

/// Collects `(name attribute, text)` of every `<Data>` element.
fn data_elements(xml: &str) -> Vec<(Option<String>, String)> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(tag) = find_tag(rest, "Data") {
        let pos = rest.find(tag).unwrap_or(0) + tag.len();
        let name = attr_value(tag, "Name");
        if tag.ends_with("/>") {
            out.push((name, String::new()));
            rest = &rest[pos..];
            continue;
        }
        let Some(end) = rest[pos..].find("</Data>") else {
            break;
        };
        out.push((name, unescape(&rest[pos..pos + end])));
        rest = &rest[pos + end + "</Data>".len()..];
    }
    out
}

fn data_named(xml: &str, name: &str) -> Option<String> {
    data_elements(xml)
        .into_iter()
        .find(|(n, _)| n.as_deref() == Some(name))
        .map(|(_, v)| v)
}

fn data_nth(xml: &str, index: usize) -> Option<String> {
    data_elements(xml).into_iter().nth(index).map(|(_, v)| v)
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

// ---------------------------------------------------------------------------
// EvtQuery wrapper
// ---------------------------------------------------------------------------

/// Reads the newest User32 event 1074 from the System log. Returns `None`
/// if the log is unreadable, holds no such event, or the read does not
/// finish within `budget`.
///
/// The query runs on a short-lived worker thread so the caller never waits
/// longer than `budget`, even if `EvtQuery` itself stalls. A late worker
/// finishes on its own and its result is dropped.
pub fn read_newest_1074(budget: Duration) -> Option<Event1074> {
    let start = Instant::now();
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("esr-event1074".into())
        .spawn(move || {
            let xml = query_newest_1074_xml(budget.saturating_sub(start.elapsed()));
            let _ = tx.send(xml);
        })
        .ok()?;
    let xml = rx
        .recv_timeout(budget.saturating_sub(start.elapsed()))
        .ok()??;
    parse_event1074_xml(&xml)
}

/// Rendered XML of the newest User32 1074 event, or `None`.
fn query_newest_1074_xml(timeout: Duration) -> Option<String> {
    // SAFETY: every handle opened here is closed before returning; buffers
    // passed to EvtNext/EvtRender are sized as declared.
    unsafe {
        let query = EvtQuery(
            None,
            w!("System"),
            w!("*[System[Provider[@Name='User32'] and (EventID=1074)]]"),
            EvtQueryChannelPath.0 | EvtQueryReverseDirection.0,
        )
        .ok()?;
        let timeout_ms = u32::try_from(timeout.as_millis())
            .unwrap_or(u32::MAX)
            .max(1);
        let mut events = [0isize; 1];
        let mut returned = 0u32;
        let result = match EvtNext(query, &mut events, timeout_ms, 0, &mut returned) {
            Ok(()) if returned > 0 => {
                let event = EVT_HANDLE(events[0]);
                let xml = render_xml(event);
                let _ = EvtClose(event);
                xml
            }
            // No event (ERROR_NO_MORE_ITEMS), timeout, or access denied.
            _ => None,
        };
        let _ = EvtClose(query);
        result
    }
}

/// `EvtRender(EvtRenderEventXml)` of one event handle.
unsafe fn render_xml(event: EVT_HANDLE) -> Option<String> {
    let mut used = 0u32;
    let mut props = 0u32;
    // SAFETY (both calls): `event` is a live handle from EvtNext; the buffer
    // size passed matches the allocation.
    unsafe {
        // The first call sizes the buffer (fails with ERROR_INSUFFICIENT_BUFFER).
        if let Err(e) = EvtRender(
            None,
            event,
            EvtRenderEventXml.0,
            0,
            None,
            &mut used,
            &mut props,
        ) && e.code() != ERROR_INSUFFICIENT_BUFFER.to_hresult()
        {
            return None;
        }
        let mut buf = vec![0u16; (used as usize).div_ceil(2)];
        EvtRender(
            None,
            event,
            EvtRenderEventXml.0,
            (buf.len() * 2) as u32,
            Some(buf.as_mut_ptr().cast()),
            &mut used,
            &mut props,
        )
        .ok()?;
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..len]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    const SAMPLE_RESTART: &str = r#"<Event xmlns='http://schemas.microsoft.com/win/2004/08/events/event'><System><Provider Name='User32' Guid='{b0aa8734-56f7-41cc-b2f4-de228e98b946}' EventSourceName='User32'/><EventID Qualifiers='32768'>1074</EventID><Version>0</Version><Level>4</Level><Task>0</Task><Opcode>0</Opcode><Keywords>0x8080000000000000</Keywords><TimeCreated SystemTime='2025-01-02T03:04:05.6789012Z'/><EventRecordID>123</EventRecordID><Channel>System</Channel><Computer>HOST</Computer></System><EventData><Data Name='param1'>C:\Windows\System32\RuntimeBroker.exe (HOST)</Data><Data Name='param2'>HOST</Data><Data Name='param3'>Other (Unplanned)</Data><Data Name='param4'>0x0</Data><Data Name='param5'>restart</Data><Data Name='param6'></Data><Data Name='param7'>HOST\user</Data></EventData></Event>"#;

    /// `SAMPLE_RESTART`'s `TimeCreated` in ms since the Unix epoch.
    const SAMPLE_MS: i64 = 1_735_787_045_678;

    fn at_ms(ms: i64) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(ms as u64)
    }

    fn event(kind: ShutdownKind, time: Option<&str>) -> Event1074 {
        Event1074 {
            time_created: time.map(str::to_string),
            shutdown_type: None,
            kind,
        }
    }

    fn classify(
        lparam: isize,
        source: EndKindSource,
        ev: Option<Event1074>,
        now_ms: i64,
    ) -> EndKind {
        classify_end_session(lparam, || ev, at_ms(now_ms), source)
    }

    const TIME: &str = "2025-01-02T03:04:05.6789012Z";
    const LOGOFF: isize = ENDSESSION_LOGOFF as isize;

    // --- classify_end_session ------------------------------------------------

    #[test]
    fn logoff_flag_is_sign_out_without_reading_the_log() {
        let called = Cell::new(false);
        let read = || {
            called.set(true);
            Some(event(ShutdownKind::Restart, Some(TIME)))
        };
        for lparam in [LOGOFF, (ENDSESSION_LOGOFF as i32) as isize, LOGOFF | 0x1] {
            let kind =
                classify_end_session(lparam, read, at_ms(SAMPLE_MS), EndKindSource::Event1074);
            assert_eq!(kind, EndKind::SignOut, "lparam {lparam:#x}");
        }
        assert!(!called.get());
    }

    #[test]
    fn fresh_restart_event_is_restart() {
        let ev = Some(event(ShutdownKind::Restart, Some(TIME)));
        assert_eq!(
            classify(0, EndKindSource::Event1074, ev, SAMPLE_MS + 500),
            EndKind::Restart
        );
    }

    #[test]
    fn power_off_event_is_shutdown() {
        let ev = Some(event(ShutdownKind::PowerOff, Some(TIME)));
        assert_eq!(
            classify(0, EndKindSource::Event1074, ev, SAMPLE_MS),
            EndKind::Shutdown
        );
    }

    #[test]
    fn stale_restart_event_is_shutdown() {
        let max = EVENT_1074_MAX_AGE_MS as i64;
        let ev = || Some(event(ShutdownKind::Restart, Some(TIME)));
        assert_eq!(
            classify(0, EndKindSource::Event1074, ev(), SAMPLE_MS + max),
            EndKind::Restart
        );
        assert_eq!(
            classify(0, EndKindSource::Event1074, ev(), SAMPLE_MS + max + 1),
            EndKind::Shutdown
        );
        assert_eq!(
            classify(0, EndKindSource::Event1074, ev(), SAMPLE_MS - max - 1),
            EndKind::Shutdown
        );
    }

    #[test]
    fn unknown_language_is_shutdown() {
        let xml = SAMPLE_RESTART.replace(">restart<", ">再起動<");
        let ev = parse_event1074_xml(&xml);
        assert_eq!(ev.as_ref().unwrap().kind, ShutdownKind::Unknown);
        assert_eq!(
            classify(0, EndKindSource::Event1074, ev, SAMPLE_MS),
            EndKind::Shutdown
        );
    }

    #[test]
    fn no_event_or_no_timestamp_is_shutdown() {
        assert_eq!(
            classify(0, EndKindSource::Event1074, None, SAMPLE_MS),
            EndKind::Shutdown
        );
        let untimed = Some(event(ShutdownKind::Restart, None));
        assert_eq!(
            classify(0, EndKindSource::Event1074, untimed, SAMPLE_MS),
            EndKind::Shutdown
        );
        let garbled = Some(event(ShutdownKind::Restart, Some("yesterday")));
        assert_eq!(
            classify(0, EndKindSource::Event1074, garbled, SAMPLE_MS),
            EndKind::Shutdown
        );
    }

    #[test]
    fn assume_shutdown_strategy_never_reads_the_log() {
        let called = Cell::new(false);
        let kind = classify_end_session(
            0,
            || {
                called.set(true);
                Some(event(ShutdownKind::Restart, Some(TIME)))
            },
            at_ms(SAMPLE_MS),
            EndKindSource::AssumeShutdown,
        );
        assert_eq!(kind, EndKind::Shutdown);
        assert!(!called.get());
        // The logoff flag still wins over the strategy.
        assert_eq!(
            classify(LOGOFF, EndKindSource::AssumeShutdown, None, 0),
            EndKind::SignOut
        );
    }

    // --- XML parsing -----------------------------------------------------------

    #[test]
    fn parses_restart_and_time() {
        let e = parse_event1074_xml(SAMPLE_RESTART).expect("1074 event");
        assert_eq!(e.kind, ShutdownKind::Restart);
        assert_eq!(e.shutdown_type.as_deref(), Some("restart"));
        assert_eq!(e.time_created.as_deref(), Some(TIME));
    }

    #[test]
    fn parses_power_off_and_positional_data() {
        let named = SAMPLE_RESTART.replace(">restart<", ">power off<");
        assert_eq!(
            parse_event1074_xml(&named).unwrap().kind,
            ShutdownKind::PowerOff
        );
        // Unnamed <Data> elements: the fifth one is the shutdown type.
        let positional = "<Event><System><EventID>1074</EventID></System><EventData>\
            <Data>a</Data><Data>b</Data><Data>c</Data><Data>0x0</Data><Data>Ausschalten</Data></EventData></Event>";
        let e = parse_event1074_xml(positional).unwrap();
        assert_eq!(e.kind, ShutdownKind::PowerOff);
        assert_eq!(e.time_created, None);
    }

    #[test]
    fn unknown_and_non_1074() {
        assert_eq!(classify_shutdown_type("xyz"), ShutdownKind::Unknown);
        assert_eq!(
            classify_shutdown_type(" Neu starten "),
            ShutdownKind::Restart
        );
        assert_eq!(
            classify_shutdown_type("Herunterfahren"),
            ShutdownKind::PowerOff
        );
        let other = SAMPLE_RESTART.replace(">1074<", ">6006<");
        assert_eq!(parse_event1074_xml(&other), None);
        assert_eq!(parse_event1074_xml("not xml"), None);
        let empty = SAMPLE_RESTART.replace(
            "<Data Name='param5'>restart</Data>",
            "<Data Name='param5'/>",
        );
        assert_eq!(
            parse_event1074_xml(&empty).unwrap().kind,
            ShutdownKind::Unknown
        );
    }

    // --- timestamps and staleness ------------------------------------------------

    #[test]
    fn parses_system_time() {
        assert_eq!(parse_system_time_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_system_time_ms(TIME), Some(SAMPLE_MS));
        assert_eq!(
            parse_system_time_ms("2024-02-29T12:00:00.5Z"),
            Some(1_709_208_000_500)
        );
        assert_eq!(
            parse_system_time_ms("2025-01-01T00:00:00+00:00"),
            Some(1_735_689_600_000)
        );
        assert_eq!(parse_system_time_ms("1969-12-31T23:59:59.999Z"), Some(-1));
    }

    #[test]
    fn rejects_malformed_system_time() {
        for s in [
            "",
            "garbage",
            "2025-01-02T03:04:05",       // no zone
            "2025-01-02T03:04:05+01:00", // not UTC
            "2025-13-01T00:00:00Z",
            "2023-02-29T00:00:00Z",
            "2025-01-02T24:00:00Z",
            "2025-01-02T03:04:05.Z",
            "2025-1-02T03:04:05Z",
            "2025-01-02 03:04:05Z",
        ] {
            assert_eq!(parse_system_time_ms(s), None, "{s:?}");
        }
    }

    #[test]
    fn staleness_math() {
        let max = EVENT_1074_MAX_AGE_MS;
        assert!(is_fresh(1_000, 1_000, max));
        assert!(is_fresh(0, max as i64, max));
        assert!(!is_fresh(0, max as i64 + 1, max));
        assert!(
            is_fresh(max as i64, 0, max),
            "small future skew is accepted"
        );
        assert!(!is_fresh(max as i64 + 1, 0, max));
        assert_eq!(unix_ms(at_ms(SAMPLE_MS)), SAMPLE_MS);
        assert_eq!(unix_ms(UNIX_EPOCH - Duration::from_millis(5)), -5);
    }

    // --- window messages ---------------------------------------------------------

    #[test]
    fn decodes_window_messages() {
        let never = |_: isize| -> EndKind { panic!("classify called") };
        assert_eq!(
            EndSessionMessage::from_msg_with(WM_ENDSESSION, 0, 0, never),
            Some(EndSessionMessage::Ended { ending: false })
        );
        assert_eq!(
            EndSessionMessage::from_msg_with(WM_ENDSESSION, 1, LOGOFF, never),
            Some(EndSessionMessage::Ended { ending: true })
        );
        assert_eq!(
            EndSessionMessage::from_msg_with(0x0010 /* WM_CLOSE */, 0, 0, never),
            None
        );
        assert_eq!(
            EndSessionMessage::from_msg_with(WM_QUERYENDSESSION, 0, 7, |lp| {
                assert_eq!(lp, 7);
                EndKind::Restart
            }),
            Some(EndSessionMessage::Query(EndKind::Restart))
        );
        // Sign-out needs no log read, so the default path is deterministic here.
        assert_eq!(
            EndSessionMessage::from_msg(WM_QUERYENDSESSION, 0, LOGOFF),
            Some(EndSessionMessage::Query(EndKind::SignOut))
        );
        let assume = SessionEndListener::new(EndKindSource::AssumeShutdown);
        assert_eq!(
            assume.on_message(WM_QUERYENDSESSION, 0, 0),
            Some(EndSessionMessage::Query(EndKind::Shutdown))
        );
        assert_eq!(QUERY_END_SESSION_ALLOW, LRESULT(1));
    }

    #[test]
    fn default_listener_uses_spike_strategy_and_budget() {
        let l = SessionEndListener::default();
        assert_eq!(l.source, EndKindSource::DEFAULT);
        assert_eq!(l.budget, Duration::from_millis(300));
    }

    // --- live Win32 calls (read-only / process-local) ---------------------------

    #[test]
    fn live_read_of_event_1074_returns_within_budget() {
        let start = Instant::now();
        let event = read_newest_1074(EVENT_1074_QUERY_BUDGET);
        let elapsed = start.elapsed();
        // Small slack for thread scheduling on a loaded test machine.
        assert!(
            elapsed <= EVENT_1074_QUERY_BUDGET + Duration::from_millis(50),
            "took {elapsed:?}"
        );
        if let Some(e) = event {
            // Whatever was read must classify without panicking.
            let _ =
                classify_end_session(0, || Some(e), SystemTime::now(), EndKindSource::Event1074);
        }
    }

    #[test]
    fn configure_shutdown_priority_sets_level_and_flags() {
        use windows::Win32::System::Threading::GetProcessShutdownParameters;
        configure_shutdown_priority().expect("SetProcessShutdownParameters");
        let (mut level, mut flags) = (0u32, 0u32);
        // SAFETY: out-pointers to locals.
        unsafe { GetProcessShutdownParameters(&mut level, &mut flags) }.expect("get");
        assert_eq!((level, flags), (SHUTDOWN_LEVEL, SHUTDOWN_NORETRY));
    }
}
