//! `LogEntry` and `format_entry` (single-line, timestamped entries).
//!
//! Requirements 9.7, 13.1, 13.2, 13.3, 13.5 (design.md, "Activity_Log entry").
//!
//! A line looks like
//!
//! ```text
//! 2025-01-31 09:15:02.123 +01:00 TAB-OMITTED location=C:\Old%20Stuff reason=does-not-exist
//! ```
//!
//! i.e. `YYYY-MM-DD HH:MM:SS.mmm ±HH:MM <EVENT>` followed by zero or more
//! ` key=value` fields. Values are escaped with the session-format rules
//! ([`crate::format::escape`]), so a
//! value never contains a space, `=`, CR or LF, and the line splits back on
//! single spaces ([`parse_line`]). The timestamp is not part of the entry: the
//! shell supplies local time when it formats the line, keeping the core
//! clock-free. `format_entry` returns the line without a terminator.

use std::fmt::Write as _;

use crate::fold::{
    FallbackReason, FoldInto, FoldOutcome, MatchingWindow, NotFoldedReason, PassReason, SwitchFailure,
};
use crate::format::escape::{escape_into, unescape};
use crate::model::{LocationId, SaveCause, Timestamp, WideString, MAX_FAILED_ATTEMPTS};
use crate::plan::fallback::OmitReason;

/// One Activity_Log entry (event names from design.md, "Activity_Log entry").
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogEntry {
    /// `SAVE` (Req 13.1): a Pending_Snapshot was saved.
    ///
    /// A [`SaveCause::Checkpoint`] is only logged when a pending checkpoint is
    /// found at the next start, so it is reported as "abrupt Session_Monitor
    /// end or power loss detected at the next start"; every other cause maps to
    /// the matching Req 13.1 cause.
    Save { cause: SaveCause, windows: u32, tabs: u32 },
    /// `RESTORE` (Req 13.2). `targeted` selects Targeted_Launch over
    /// Default_Launch; `duration_ms` runs from the Trigger_Window first becoming
    /// visible to restoration completion.
    Restore { targeted: bool, windows: u32, tabs: u32, omitted: u32, redirected: u32, duration_ms: u64 },
    /// `TAB-OMITTED` (Req 9.7): the saved Location and why it was omitted.
    TabOmitted { location: LocationId, reason: OmitReason },
    /// `TAB-REDIRECTED` (Req 9.7): the saved Location, why it was unavailable,
    /// and the ancestor that was opened instead.
    TabRedirected { location: LocationId, reason: OmitReason, ancestor: LocationId },
    /// `ERROR` (Req 13.3): the failed operation and a description.
    Error { operation: String, description: String },
    /// `CAP-EXCLUDED` (Req 2.10): numbers of windows and tabs excluded by the caps.
    CapExcluded { windows: u32, tabs: u32 },
    /// `UNIDENTIFIABLE-LOCATION` (Req 8.8). `kept_previous` is true when the
    /// tab's last identifier was kept, false when the tab was excluded.
    UnidentifiableLocation { kept_previous: bool },
    /// `ABANDONED` (Req 4.11): the Pending_Snapshot was abandoned after
    /// [`MAX_FAILED_ATTEMPTS`] failed attempts.
    Abandoned,
    /// `BUILTIN-RESTORE-DETECTED` (Req 10.4).
    BuiltinRestoreDetected,
    /// `TABS-DEFERRED` (Req 5.13): the restoration opens `immediate` of the
    /// snapshot's `distinct` saved Locations at once (at most `budget`,
    /// beyond it only Active_Tabs) and restores `deferred` tabs as deferred
    /// tabs that navigate when activated.
    TabsDeferred { distinct: u32, immediate: u32, deferred: u32, budget: u32 },
    /// `FOLD` (Req 14.30): how a Folding_Candidate's fold ended. `location` is
    /// the candidate's Location. Written as
    /// `FOLD location=<parsing name> outcome=<o>` followed by the fields of
    /// [`fold_outcome_fields`].
    Fold { location: LocationId, outcome: FoldOutcome },
}

impl LogEntry {
    /// The `<EVENT>` token of this entry.
    pub fn event_name(&self) -> &'static str {
        match self {
            LogEntry::Save { .. } => "SAVE",
            LogEntry::Restore { .. } => "RESTORE",
            LogEntry::TabOmitted { .. } => "TAB-OMITTED",
            LogEntry::TabRedirected { .. } => "TAB-REDIRECTED",
            LogEntry::Error { .. } => "ERROR",
            LogEntry::CapExcluded { .. } => "CAP-EXCLUDED",
            LogEntry::UnidentifiableLocation { .. } => "UNIDENTIFIABLE-LOCATION",
            LogEntry::Abandoned => "ABANDONED",
            LogEntry::BuiltinRestoreDetected => "BUILTIN-RESTORE-DETECTED",
            LogEntry::TabsDeferred { .. } => "TABS-DEFERRED",
            LogEntry::Fold { .. } => "FOLD",
        }
    }
}

/// Log token of a [`PassReason`] (an element of the `passed` list, Req 14.30).
pub fn pass_reason_token(reason: PassReason) -> &'static str {
    match reason {
        PassReason::Closed => "closed",
        PassReason::Closing => "closing",
        PassReason::Unresponsive => "unresponsive",
        PassReason::TabNotCreated => "tab-not-created-1s",
    }
}

/// Log token of a [`SwitchFailure`] (the `switch-reason` field, Req 14.30).
pub fn switch_failure_token(reason: SwitchFailure) -> &'static str {
    match reason {
        SwitchFailure::NotActivated => "not-activated-1s",
        SwitchFailure::MatchingTabClosed => "matching-tab-closed",
        SwitchFailure::FoldTargetClosed => "fold-target-closed",
    }
}

/// Log token of a [`FallbackReason`] (the `reason` field of `folded-fallback`).
pub fn fallback_reason_token(reason: FallbackReason) -> &'static str {
    match reason {
        FallbackReason::PrimaryUnresponsive => "primary-unresponsive",
        FallbackReason::PrimaryClosing => "primary-closing",
        FallbackReason::TabNotCreated => "tab-not-created-1s",
    }
}

/// Log token of a [`FoldInto`] (the `into` field, Req 14.30).
pub fn fold_into_token(into: FoldInto) -> &'static str {
    match into {
        FoldInto::Foreground => "foreground",
        FoldInto::MatchingWindow => "matching-window",
        FoldInto::Primary => "primary",
        FoldInto::Fallback => "fallback",
    }
}

/// Log token of a [`MatchingWindow`] (the `matching` field of `switched`).
pub fn matching_window_token(matching: MatchingWindow) -> &'static str {
    match matching {
        MatchingWindow::Primary => "primary",
        MatchingWindow::Other => "other",
    }
}

/// Log token of a [`NotFoldedReason`] (the `reason` field of `not-folded`).
pub fn not_folded_reason_token(reason: NotFoldedReason) -> &'static str {
    match reason {
        NotFoldedReason::NoFoldTarget => "no-fold-target",
        NotFoldedReason::TabNotCreated => "tab-not-created-1s",
        NotFoldedReason::LocationNotShown => "location-not-shown-3s",
        NotFoldedReason::FoldTargetClosed => "fold-target-closed",
        NotFoldedReason::FirstNavTimeout => "first-nav-timeout-8s",
        NotFoldedReason::FoldingDisabled => "folding-disabled",
    }
}

/// The `outcome=` token of a [`FoldOutcome`] (Req 14.30).
pub fn fold_outcome_token(outcome: &FoldOutcome) -> &'static str {
    match outcome {
        FoldOutcome::FoldedPrimary => "folded-primary",
        FoldOutcome::FoldedFallback { .. } => "folded-fallback",
        FoldOutcome::FoldedForeground { .. } => "folded-foreground",
        FoldOutcome::Switched { .. } => "switched",
        FoldOutcome::SwitchFailedFolded { .. } => "switch-failed-folded",
        FoldOutcome::NotFolded { .. } => "not-folded",
    }
}

/// The `passed` list: tokens joined by `,` in order (empty when none).
fn passed_list(passed: &[PassReason]) -> String {
    passed.iter().map(|r| pass_reason_token(*r)).collect::<Vec<_>>().join(",")
}

/// The fields that follow `outcome=` for `outcome`, in line order, as
/// `(key, value)` with `value` already a safe (unescaped) token:
/// `reason`, `matching`, `switch-reason`, `into`, `passed` (design.md,
/// "Activity_Log"). Only the fields the outcome has are returned.
pub fn fold_outcome_fields(outcome: &FoldOutcome) -> Vec<(&'static str, String)> {
    match outcome {
        FoldOutcome::FoldedPrimary => vec![],
        FoldOutcome::FoldedFallback { reason } => vec![("reason", fallback_reason_token(*reason).into())],
        FoldOutcome::FoldedForeground { into, passed } => {
            vec![("into", fold_into_token(*into).into()), ("passed", passed_list(passed))]
        }
        FoldOutcome::Switched { matching } => vec![("matching", matching_window_token(*matching).into())],
        FoldOutcome::SwitchFailedFolded { switch_reason, into, passed } => vec![
            ("switch-reason", switch_failure_token(*switch_reason).into()),
            ("into", fold_into_token(*into).into()),
            ("passed", passed_list(passed)),
        ],
        FoldOutcome::NotFolded { reason, switch_reason } => {
            let mut fields = vec![("reason", not_folded_reason_token(*reason).into())];
            if let Some(s) = switch_reason {
                fields.push(("switch-reason", switch_failure_token(*s).into()));
            }
            fields
        }
    }
}

/// Log token for a save cause (exactly one of the Req 13.1 causes).
pub fn save_cause_token(cause: SaveCause) -> &'static str {
    match cause {
        SaveCause::LastWindowClosed => "last-window-closed",
        SaveCause::LastTabClosed => "last-tab-closed",
        SaveCause::ExplorerTerminated => "explorer-terminated",
        SaveCause::SignOut => "sign-out",
        SaveCause::Shutdown => "shutdown",
        SaveCause::Restart => "restart",
        SaveCause::ExitThroughSettings => "exit-through-settings",
        SaveCause::Checkpoint => "abrupt-end-detected-at-start",
    }
}

/// Log token for an omission reason (exactly one of the Req 9.7 reasons).
pub fn omit_reason_token(reason: OmitReason) -> &'static str {
    match reason {
        OmitReason::NotExist => "does-not-exist",
        OmitReason::Inaccessible => "inaccessible",
        OmitReason::Timeout => "no-response-within-5s",
        OmitReason::Unresolvable => "identifier-unresolvable",
        OmitReason::ShellUnavailable => "shell-location-unavailable",
    }
}

/// Formats `entry` at local time `ts` as one Activity_Log line (no terminator).
pub fn format_entry(ts: &Timestamp, entry: &LogEntry) -> String {
    let mut line = format_timestamp(ts);
    line.push(' ');
    line.push_str(entry.event_name());

    // Values that are already safe tokens (numbers, fixed tokens) go in raw.
    let raw = |line: &mut String, key: &str, value: &dyn std::fmt::Display| {
        let _ = write!(line, " {key}={value}");
    };
    let esc = |line: &mut String, key: &str, value: &[u16]| {
        let _ = write!(line, " {key}=");
        escape_into(line, value);
    };

    match entry {
        LogEntry::Save { cause, windows, tabs } => {
            raw(&mut line, "cause", &save_cause_token(*cause));
            raw(&mut line, "windows", windows);
            raw(&mut line, "tabs", tabs);
        }
        LogEntry::Restore { targeted, windows, tabs, omitted, redirected, duration_ms } => {
            raw(&mut line, "launch", &if *targeted { "targeted" } else { "default" });
            raw(&mut line, "windows", windows);
            raw(&mut line, "tabs", tabs);
            raw(&mut line, "omitted", omitted);
            raw(&mut line, "redirected", redirected);
            raw(&mut line, "duration_ms", duration_ms);
        }
        LogEntry::TabOmitted { location, reason } => {
            esc(&mut line, "location", location.parsing_name.units());
            raw(&mut line, "reason", &omit_reason_token(*reason));
        }
        LogEntry::TabRedirected { location, reason, ancestor } => {
            esc(&mut line, "location", location.parsing_name.units());
            raw(&mut line, "reason", &omit_reason_token(*reason));
            esc(&mut line, "ancestor", ancestor.parsing_name.units());
        }
        LogEntry::Error { operation, description } => {
            let op: Vec<u16> = operation.encode_utf16().collect();
            let desc: Vec<u16> = description.encode_utf16().collect();
            esc(&mut line, "operation", &op);
            esc(&mut line, "description", &desc);
        }
        LogEntry::CapExcluded { windows, tabs } => {
            raw(&mut line, "windows", windows);
            raw(&mut line, "tabs", tabs);
        }
        LogEntry::UnidentifiableLocation { kept_previous } => {
            raw(&mut line, "action", &if *kept_previous { "kept-previous" } else { "tab-excluded" });
        }
        LogEntry::Abandoned => raw(&mut line, "failed_attempts", &MAX_FAILED_ATTEMPTS),
        LogEntry::BuiltinRestoreDetected => {}
        LogEntry::TabsDeferred { distinct, immediate, deferred, budget } => {
            raw(&mut line, "distinct", distinct);
            raw(&mut line, "immediate", immediate);
            raw(&mut line, "deferred", deferred);
            raw(&mut line, "budget", budget);
        }
        LogEntry::Fold { location, outcome } => {
            esc(&mut line, "location", location.parsing_name.units());
            raw(&mut line, "outcome", &fold_outcome_token(outcome));
            for (key, value) in fold_outcome_fields(outcome) {
                raw(&mut line, key, &value);
            }
        }
    }
    line
}

/// `YYYY-MM-DD HH:MM:SS.mmm ±HH:MM`; a zero offset is `+00:00`.
fn format_timestamp(ts: &Timestamp) -> String {
    let sign = if ts.offset_minutes < 0 { '-' } else { '+' };
    let off = ts.offset_minutes.unsigned_abs();
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03} {sign}{:02}:{:02}",
        ts.year,
        ts.month,
        ts.day,
        ts.hour,
        ts.minute,
        ts.second,
        ts.millis,
        off / 60,
        off % 60
    )
}

// ---------------------------------------------------------------------------
// Parsing back (tests and diagnostics)
// ---------------------------------------------------------------------------

/// An Activity_Log line split back into its parts by [`parse_line`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedLine {
    pub timestamp: Timestamp,
    pub event: String,
    /// `key=value` fields in line order, values unescaped.
    pub fields: Vec<(String, WideString)>,
}

impl ParsedLine {
    /// The unescaped value of the first field named `key`.
    pub fn get(&self, key: &str) -> Option<&WideString> {
        self.fields.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// [`get`](Self::get) as a lossy `String`.
    pub fn get_str(&self, key: &str) -> Option<String> {
        self.get(key).map(WideString::to_string_lossy)
    }
}

/// Parses a line produced by [`format_entry`]. Returns `None` unless the line
/// has the exact shape `YYYY-MM-DD HH:MM:SS.mmm ±HH:MM EVENT[ key=value]*`
/// with single-space separators and validly escaped values.
pub fn parse_line(line: &str) -> Option<ParsedLine> {
    let mut parts = line.split(' ');
    let timestamp = parse_timestamp(parts.next()?, parts.next()?, parts.next()?)?;
    let event = parts.next()?;
    if event.is_empty() || !event.bytes().all(|b| b.is_ascii_uppercase() || b == b'-') {
        return None;
    }
    let fields = parts
        .map(|field| {
            let (key, value) = field.split_once('=')?;
            if key.is_empty() || value.contains('=') {
                return None;
            }
            Some((key.to_owned(), unescape(value).ok()?))
        })
        .collect::<Option<Vec<_>>>()?;
    Some(ParsedLine { timestamp, event: event.to_owned(), fields })
}

fn parse_timestamp(date: &str, time: &str, offset: &str) -> Option<Timestamp> {
    fn num<T: std::str::FromStr>(s: &str, len: usize) -> Option<T> {
        (s.len() == len && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok())?
    }
    let d: Vec<&str> = date.split('-').collect();
    let (hms, millis) = time.split_once('.')?;
    let t: Vec<&str> = hms.split(':').collect();
    let (sign, off) = match offset.as_bytes().first()? {
        b'+' => (1i16, &offset[1..]),
        b'-' => (-1i16, &offset[1..]),
        _ => return None,
    };
    let (oh, om) = off.split_once(':')?;
    if d.len() != 3 || t.len() != 3 {
        return None;
    }
    let ts = Timestamp {
        year: num(d[0], 4)?,
        month: num(d[1], 2)?,
        day: num(d[2], 2)?,
        hour: num(t[0], 2)?,
        minute: num(t[1], 2)?,
        second: num(t[2], 2)?,
        millis: num(millis, 3)?,
        offset_minutes: sign * (num::<i16>(oh, 2)? * 60 + num::<i16>(om, 2)?),
    };
    ts.is_valid().then_some(ts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{LocationKind, Volume};

    fn ts(offset_minutes: i16) -> Timestamp {
        Timestamp {
            year: 2025,
            month: 1,
            day: 31,
            hour: 9,
            minute: 15,
            second: 2,
            millis: 123,
            offset_minutes,
        }
    }

    fn loc(units: Vec<u16>) -> LocationId {
        LocationId {
            case_sensitive: false,
            kind: LocationKind::FileSystem(Volume::Fixed),
            parsing_name: WideString(units),
            display_name: "display name is not logged".into(),
            pidl: vec![1, 2],
            search: None,
        }
    }

    fn fs(path: &str) -> LocationId {
        loc(path.encode_utf16().collect())
    }

    /// Formats, checks the single-line shape, and parses back.
    fn roundtrip(entry: &LogEntry) -> (String, ParsedLine) {
        let line = format_entry(&ts(60), entry);
        assert!(!line.contains('\n') && !line.contains('\r'), "{line:?}");
        let parsed = parse_line(&line).unwrap_or_else(|| panic!("unparsable: {line:?}"));
        assert_eq!(parsed.timestamp, ts(60));
        assert_eq!(parsed.event, entry.event_name());
        (line, parsed)
    }

    #[test]
    fn timestamp_format_and_offsets() {
        let e = LogEntry::BuiltinRestoreDetected;
        assert_eq!(format_entry(&ts(60), &e), "2025-01-31 09:15:02.123 +01:00 BUILTIN-RESTORE-DETECTED");
        assert!(format_entry(&ts(0), &e).starts_with("2025-01-31 09:15:02.123 +00:00 "));
        assert!(format_entry(&ts(-330), &e).starts_with("2025-01-31 09:15:02.123 -05:30 "));
        assert!(format_entry(&ts(23 * 60 + 59), &e).contains(" +23:59 "));
        let early = Timestamp { year: 7, month: 2, day: 3, hour: 0, minute: 4, second: 5, millis: 6, offset_minutes: -1 };
        assert!(format_entry(&early, &e).starts_with("0007-02-03 00:04:05.006 -00:01 "));
        assert_eq!(parse_line(&format_entry(&early, &e)).unwrap().timestamp, early);
    }

    #[test]
    fn save_every_cause() {
        let causes = [
            (SaveCause::LastWindowClosed, "last-window-closed"),
            (SaveCause::LastTabClosed, "last-tab-closed"),
            (SaveCause::ExplorerTerminated, "explorer-terminated"),
            (SaveCause::SignOut, "sign-out"),
            (SaveCause::Shutdown, "shutdown"),
            (SaveCause::Restart, "restart"),
            (SaveCause::ExitThroughSettings, "exit-through-settings"),
            (SaveCause::Checkpoint, "abrupt-end-detected-at-start"),
        ];
        for (cause, token) in causes {
            let (line, p) = roundtrip(&LogEntry::Save { cause, windows: 3, tabs: 11 });
            assert_eq!(line, format!("2025-01-31 09:15:02.123 +01:00 SAVE cause={token} windows=3 tabs=11"));
            assert_eq!(p.get_str("cause").as_deref(), Some(token));
        }
    }

    #[test]
    fn restore() {
        let entry = LogEntry::Restore {
            targeted: false,
            windows: 2,
            tabs: 7,
            omitted: 1,
            redirected: 2,
            duration_ms: 1234,
        };
        let (line, _) = roundtrip(&entry);
        assert!(line.ends_with(
            " RESTORE launch=default windows=2 tabs=7 omitted=1 redirected=2 duration_ms=1234"
        ));
        let entry = LogEntry::Restore {
            targeted: true,
            windows: 0,
            tabs: 0,
            omitted: 0,
            redirected: 0,
            duration_ms: u64::MAX,
        };
        let (_, p) = roundtrip(&entry);
        assert_eq!(p.get_str("launch").as_deref(), Some("targeted"));
        assert_eq!(p.get_str("duration_ms"), Some(u64::MAX.to_string()));
    }

    #[test]
    fn tab_omitted_every_reason() {
        let reasons = [
            (OmitReason::NotExist, "does-not-exist"),
            (OmitReason::Inaccessible, "inaccessible"),
            (OmitReason::Timeout, "no-response-within-5s"),
            (OmitReason::Unresolvable, "identifier-unresolvable"),
            (OmitReason::ShellUnavailable, "shell-location-unavailable"),
        ];
        for (reason, token) in reasons {
            let entry = LogEntry::TabOmitted { location: fs(r"\\server\share\a b"), reason };
            let (line, p) = roundtrip(&entry);
            assert!(line.ends_with(&format!(r" TAB-OMITTED location=\\server\share\a%20b reason={token}")));
            assert_eq!(p.get("location"), Some(&fs(r"\\server\share\a b").parsing_name));
        }
    }

    #[test]
    fn tab_redirected() {
        let entry = LogEntry::TabRedirected {
            location: fs(r"C:\Users\me\Old=Stuff\deep"),
            reason: OmitReason::NotExist,
            ancestor: fs(r"C:\Users\me"),
        };
        let (line, p) = roundtrip(&entry);
        assert!(line.ends_with(
            r" TAB-REDIRECTED location=C:\Users\me\Old%3DStuff\deep reason=does-not-exist ancestor=C:\Users\me"
        ));
        assert_eq!(p.get_str("location").as_deref(), Some(r"C:\Users\me\Old=Stuff\deep"));
        assert_eq!(p.get_str("ancestor").as_deref(), Some(r"C:\Users\me"));
    }

    #[test]
    fn error_fields_are_escaped() {
        let entry = LogEntry::Error {
            operation: "session-file write".into(),
            description: "Access is denied.\r\n(os error 5) 100% \u{85}é😀".into(),
        };
        let (line, p) = roundtrip(&entry);
        assert!(line.ends_with(
            " ERROR operation=session-file%20write \
             description=Access%20is%20denied.%0D%0A(os%20error%205)%20100%25%20%85é😀"
        ));
        assert_eq!(p.get_str("operation").as_deref(), Some("session-file write"));
        assert_eq!(p.get_str("description").as_deref(), Some("Access is denied.\r\n(os error 5) 100% \u{85}é😀"));

        let (line, p) = roundtrip(&LogEntry::Error { operation: String::new(), description: String::new() });
        assert!(line.ends_with(" ERROR operation= description="));
        assert_eq!(p.get_str("operation").as_deref(), Some(""));
    }

    #[test]
    fn unpaired_surrogates_are_escaped_and_preserved() {
        let units = vec![b'C' as u16, b':' as u16, 0xD800, b'x' as u16, 0xDFFF, 0xD83D, 0xDE00];
        let entry = LogEntry::TabOmitted { location: loc(units.clone()), reason: OmitReason::Unresolvable };
        let (line, p) = roundtrip(&entry);
        assert!(line.contains(" location=C:%uD800x%uDFFF😀 "), "{line}");
        assert_eq!(p.get("location"), Some(&WideString(units)));
    }

    #[test]
    fn cap_excluded() {
        let (line, _) = roundtrip(&LogEntry::CapExcluded { windows: 5, tabs: 40 });
        assert!(line.ends_with(" CAP-EXCLUDED windows=5 tabs=40"));
    }

    #[test]
    fn unidentifiable_location() {
        let (line, _) = roundtrip(&LogEntry::UnidentifiableLocation { kept_previous: true });
        assert!(line.ends_with(" UNIDENTIFIABLE-LOCATION action=kept-previous"));
        let (line, _) = roundtrip(&LogEntry::UnidentifiableLocation { kept_previous: false });
        assert!(line.ends_with(" UNIDENTIFIABLE-LOCATION action=tab-excluded"));
    }

    #[test]
    fn abandoned() {
        let (line, _) = roundtrip(&LogEntry::Abandoned);
        assert!(line.ends_with(" ABANDONED failed_attempts=3"));
    }

    #[test]
    fn builtin_restore_detected() {
        let (_, p) = roundtrip(&LogEntry::BuiltinRestoreDetected);
        assert!(p.fields.is_empty());
    }

    #[test]
    fn tabs_deferred() {
        let entry = LogEntry::TabsDeferred { distinct: 140, immediate: 100, deferred: 45, budget: 100 };
        let (line, _) = roundtrip(&entry);
        assert!(line.ends_with(" TABS-DEFERRED distinct=140 immediate=100 deferred=45 budget=100"));
    }

    fn fold(outcome: FoldOutcome) -> (String, ParsedLine) {
        roundtrip(&LogEntry::Fold { location: fs(r"C:\My Docs"), outcome })
    }

    #[test]
    fn fold_simple_outcomes() {
        let (line, p) = fold(FoldOutcome::FoldedPrimary);
        assert!(line.ends_with(r" FOLD location=C:\My%20Docs outcome=folded-primary"), "{line}");
        assert_eq!(p.get_str("location").as_deref(), Some(r"C:\My Docs"));

        let (line, _) = fold(FoldOutcome::FoldedFallback { reason: FallbackReason::TabNotCreated });
        assert!(line.ends_with(" outcome=folded-fallback reason=tab-not-created-1s"), "{line}");
        let (line, _) = fold(FoldOutcome::FoldedFallback { reason: FallbackReason::PrimaryUnresponsive });
        assert!(line.ends_with(" reason=primary-unresponsive"));
        let (line, _) = fold(FoldOutcome::FoldedFallback { reason: FallbackReason::PrimaryClosing });
        assert!(line.ends_with(" reason=primary-closing"));

        let (line, _) = fold(FoldOutcome::Switched { matching: MatchingWindow::Other });
        assert!(line.ends_with(" outcome=switched matching=other"), "{line}");
        let (line, _) = fold(FoldOutcome::Switched { matching: MatchingWindow::Primary });
        assert!(line.ends_with(" outcome=switched matching=primary"));
    }

    #[test]
    fn fold_foreground_passed_list_in_order() {
        let passed = vec![
            PassReason::Closed,
            PassReason::Closing,
            PassReason::Unresponsive,
            PassReason::TabNotCreated,
            PassReason::Closed,
        ];
        let (line, p) = fold(FoldOutcome::FoldedForeground { into: FoldInto::Fallback, passed });
        assert!(
            line.ends_with(
                " outcome=folded-foreground into=fallback passed=closed,closing,unresponsive,tab-not-created-1s,closed"
            ),
            "{line}"
        );
        assert_eq!(p.get_str("into").as_deref(), Some("fallback"));

        let (line, p) = fold(FoldOutcome::FoldedForeground { into: FoldInto::Foreground, passed: vec![] });
        assert!(line.ends_with(" outcome=folded-foreground into=foreground passed="), "{line}");
        assert_eq!(p.get_str("passed").as_deref(), Some(""));
    }

    #[test]
    fn fold_switch_failed_and_not_folded() {
        let (line, _) = fold(FoldOutcome::SwitchFailedFolded {
            switch_reason: SwitchFailure::FoldTargetClosed,
            into: FoldInto::MatchingWindow,
            passed: vec![PassReason::Unresponsive],
        });
        assert!(
            line.ends_with(
                " outcome=switch-failed-folded switch-reason=fold-target-closed into=matching-window passed=unresponsive"
            ),
            "{line}"
        );
        let (line, _) = fold(FoldOutcome::SwitchFailedFolded {
            switch_reason: SwitchFailure::NotActivated,
            into: FoldInto::Primary,
            passed: vec![],
        });
        assert!(line.ends_with(" switch-reason=not-activated-1s into=primary passed="), "{line}");
        let (line, _) = fold(FoldOutcome::SwitchFailedFolded {
            switch_reason: SwitchFailure::MatchingTabClosed,
            into: FoldInto::Fallback,
            passed: vec![],
        });
        assert!(line.contains(" switch-reason=matching-tab-closed into=fallback "), "{line}");

        let (line, p) = fold(FoldOutcome::NotFolded { reason: NotFoldedReason::FirstNavTimeout, switch_reason: None });
        assert!(line.ends_with(" outcome=not-folded reason=first-nav-timeout-8s"), "{line}");
        assert_eq!(p.get("switch-reason"), None);
        let (line, p) = fold(FoldOutcome::NotFolded {
            reason: NotFoldedReason::LocationNotShown,
            switch_reason: Some(SwitchFailure::NotActivated),
        });
        assert!(
            line.ends_with(" outcome=not-folded reason=location-not-shown-3s switch-reason=not-activated-1s"),
            "{line}"
        );
        assert_eq!(p.get_str("switch-reason").as_deref(), Some("not-activated-1s"));
        for (reason, token) in [
            (NotFoldedReason::NoFoldTarget, "no-fold-target"),
            (NotFoldedReason::TabNotCreated, "tab-not-created-1s"),
            (NotFoldedReason::FoldTargetClosed, "fold-target-closed"),
            (NotFoldedReason::FoldingDisabled, "folding-disabled"),
        ] {
            let (line, _) = fold(FoldOutcome::NotFolded { reason, switch_reason: None });
            assert!(line.ends_with(&format!(" reason={token}")), "{line}");
        }
    }

    #[test]
    fn parse_line_rejects_malformed() {
        for bad in [
            "",
            "2025-01-31 09:15:02.123 +01:00",
            "2025-01-31 09:15:02.123 +01:00 SAVE  cause=x",
            "2025-01-31 09:15:02.123 01:00 SAVE",
            "2025-01-31 09:15:02.12 +01:00 SAVE",
            "2025-02-30 09:15:02.123 +01:00 SAVE",
            "2025-01-31 09:15:02.123 +01:00 save",
            "2025-01-31 09:15:02.123 +01:00 SAVE cause",
            "2025-01-31 09:15:02.123 +01:00 SAVE =x",
            "2025-01-31 09:15:02.123 +01:00 SAVE a=b=c",
            "2025-01-31 09:15:02.123 +01:00 SAVE a=%2",
            "2025-01-31 09:15:02.123 +01:00 SAVE a=%uD8",
            "2025-01-31 09:15:02.123 +01:00 SAVE a=%u0041",
        ] {
            assert_eq!(parse_line(bad), None, "{bad:?}");
        }
    }
}
