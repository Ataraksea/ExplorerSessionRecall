//! Property test for the `FOLD` Activity_Log entry.
//!
//! **Property 40: Fold log entries** — *for any* fold path, exactly one `FOLD`
//! entry is emitted when the fold completes or stops; its outcome
//! (`folded-primary`, `folded-fallback`, `folded-foreground`, `switched`,
//! `switch-failed-folded`, `not-folded`) and its `reason`, `matching`,
//! `switch-reason`, `into` and `passed` fields match the path taken (one value
//! each, passed-over windows in order), and `parse_line(format_entry(e))` gives
//! back every field.
//!
//! **Validates: Requirements 14.30, 13.5**

mod common;

use common::strategies::*;
use esr_core::fold::{
    FallbackReason, FoldInto, FoldOutcome, MatchingWindow, NotFoldedReason, PassReason, SwitchFailure,
};
use esr_core::logfmt::entry::{format_entry, parse_line, LogEntry};
use esr_core::model::*;
use proptest::collection::vec;
use proptest::prelude::*;
use proptest::sample::select;

// ---------------------------------------------------------------------------
// Test-side expectations (independent of the implementation's token tables)
// ---------------------------------------------------------------------------

fn pass_token(r: PassReason) -> &'static str {
    match r {
        PassReason::Closed => "closed",
        PassReason::Closing => "closing",
        PassReason::Unresponsive => "unresponsive",
        PassReason::TabNotCreated => "tab-not-created-1s",
    }
}

fn switch_token(r: SwitchFailure) -> &'static str {
    match r {
        SwitchFailure::NotActivated => "not-activated-1s",
        SwitchFailure::MatchingTabClosed => "matching-tab-closed",
        SwitchFailure::FoldTargetClosed => "fold-target-closed",
    }
}

fn fallback_token(r: FallbackReason) -> &'static str {
    match r {
        FallbackReason::PrimaryUnresponsive => "primary-unresponsive",
        FallbackReason::PrimaryClosing => "primary-closing",
        FallbackReason::TabNotCreated => "tab-not-created-1s",
    }
}

fn into_token(i: FoldInto) -> &'static str {
    match i {
        FoldInto::Foreground => "foreground",
        FoldInto::MatchingWindow => "matching-window",
        FoldInto::Primary => "primary",
        FoldInto::Fallback => "fallback",
    }
}

fn matching_token(m: MatchingWindow) -> &'static str {
    match m {
        MatchingWindow::Primary => "primary",
        MatchingWindow::Other => "other",
    }
}

fn not_folded_token(r: NotFoldedReason) -> &'static str {
    match r {
        NotFoldedReason::NoFoldTarget => "no-fold-target",
        NotFoldedReason::TabNotCreated => "tab-not-created-1s",
        NotFoldedReason::LocationNotShown => "location-not-shown-3s",
        NotFoldedReason::FoldTargetClosed => "fold-target-closed",
        NotFoldedReason::FirstNavTimeout => "first-nav-timeout-8s",
        NotFoldedReason::FoldingDisabled => "folding-disabled",
    }
}

fn passed_value(passed: &[PassReason]) -> String {
    passed.iter().map(|p| pass_token(*p)).collect::<Vec<_>>().join(",")
}

/// The outcome token and the fields that follow it, in the design's order
/// (`reason`, `matching`, `switch-reason`, `into`, `passed`).
fn expected_outcome(outcome: &FoldOutcome) -> (&'static str, Vec<(&'static str, String)>) {
    match outcome {
        FoldOutcome::FoldedPrimary => ("folded-primary", vec![]),
        FoldOutcome::FoldedFallback { reason } => {
            ("folded-fallback", vec![("reason", fallback_token(*reason).to_owned())])
        }
        FoldOutcome::FoldedForeground { into, passed } => (
            "folded-foreground",
            vec![("into", into_token(*into).to_owned()), ("passed", passed_value(passed))],
        ),
        FoldOutcome::Switched { matching } => {
            ("switched", vec![("matching", matching_token(*matching).to_owned())])
        }
        FoldOutcome::SwitchFailedFolded { switch_reason, into, passed } => (
            "switch-failed-folded",
            vec![
                ("switch-reason", switch_token(*switch_reason).to_owned()),
                ("into", into_token(*into).to_owned()),
                ("passed", passed_value(passed)),
            ],
        ),
        FoldOutcome::NotFolded { reason, switch_reason } => {
            let mut fields = vec![("reason", not_folded_token(*reason).to_owned())];
            if let Some(s) = switch_reason {
                fields.push(("switch-reason", switch_token(*s).to_owned()));
            }
            ("not-folded", fields)
        }
    }
}

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn pass_reason() -> impl Strategy<Value = PassReason> {
    select(vec![
        PassReason::Closed,
        PassReason::Closing,
        PassReason::Unresponsive,
        PassReason::TabNotCreated,
    ])
}

fn passed() -> impl Strategy<Value = Vec<PassReason>> {
    vec(pass_reason(), 0..=6)
}

fn switch_failure() -> impl Strategy<Value = SwitchFailure> {
    select(vec![SwitchFailure::NotActivated, SwitchFailure::MatchingTabClosed, SwitchFailure::FoldTargetClosed])
}

/// `folded-foreground` goes into the foreground, the primary or a fallback window.
fn foreground_into() -> impl Strategy<Value = FoldInto> {
    select(vec![FoldInto::Foreground, FoldInto::Primary, FoldInto::Fallback])
}

/// `switch-failed-folded` goes into the matching window, the primary or a fallback window.
fn failed_switch_into() -> impl Strategy<Value = FoldInto> {
    select(vec![FoldInto::MatchingWindow, FoldInto::Primary, FoldInto::Fallback])
}

fn fold_outcome() -> impl Strategy<Value = FoldOutcome> {
    prop_oneof![
        Just(FoldOutcome::FoldedPrimary),
        select(vec![
            FallbackReason::PrimaryUnresponsive,
            FallbackReason::PrimaryClosing,
            FallbackReason::TabNotCreated,
        ])
        .prop_map(|reason| FoldOutcome::FoldedFallback { reason }),
        (foreground_into(), passed()).prop_map(|(into, passed)| FoldOutcome::FoldedForeground { into, passed }),
        select(vec![MatchingWindow::Primary, MatchingWindow::Other])
            .prop_map(|matching| FoldOutcome::Switched { matching }),
        (switch_failure(), failed_switch_into(), passed()).prop_map(|(switch_reason, into, passed)| {
            FoldOutcome::SwitchFailedFolded { switch_reason, into, passed }
        }),
        (
            select(vec![
                NotFoldedReason::NoFoldTarget,
                NotFoldedReason::TabNotCreated,
                NotFoldedReason::LocationNotShown,
                NotFoldedReason::FoldTargetClosed,
                NotFoldedReason::FirstNavTimeout,
                NotFoldedReason::FoldingDisabled,
            ]),
            proptest::option::of(switch_failure()),
        )
            .prop_map(|(reason, switch_reason)| FoldOutcome::NotFolded { reason, switch_reason }),
    ]
}

// ---------------------------------------------------------------------------
// Property
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// **Property 40: Fold log entries**
    ///
    /// **Validates: Requirements 14.30, 13.5**
    #[test]
    fn fold_log_entries(ts in timestamp(), location in location_id(), outcome in fold_outcome()) {
        let entry = LogEntry::Fold { location: location.clone(), outcome: outcome.clone() };
        let line = format_entry(&ts, &entry);

        // Req 13.5: exactly one line, no BOM.
        prop_assert!(!line.contains(['\r', '\n']), "line break in {:?}", line);
        prop_assert!(!line.starts_with('\u{FEFF}'), "BOM in {:?}", line);

        // The right event token.
        let parsed = parse_line(&line);
        prop_assert!(parsed.is_some(), "unparsable line {:?}", line);
        let parsed = parsed.unwrap();
        prop_assert_eq!(&parsed.timestamp, &ts);
        prop_assert_eq!(parsed.event.as_str(), "FOLD");

        // `location`, `outcome`, then the outcome's fields in order; one value
        // per field, every value reading back unchanged.
        let (outcome_token, outcome_fields) = expected_outcome(&outcome);
        let mut expected: Vec<(String, WideString)> = vec![
            ("location".to_owned(), location.parsing_name.clone()),
            ("outcome".to_owned(), WideString::from(outcome_token)),
        ];
        expected.extend(outcome_fields.iter().map(|(k, v)| ((*k).to_owned(), WideString::from(v.as_str()))));
        prop_assert_eq!(&parsed.fields, &expected, "line {:?}", line);

        // Each key occurs once.
        for (key, _) in &parsed.fields {
            prop_assert_eq!(parsed.fields.iter().filter(|(k, _)| k == key).count(), 1, "key {:?}", key);
        }

        // The passed-over windows read back in order (an empty list is an empty value).
        if let FoldOutcome::FoldedForeground { passed, .. } | FoldOutcome::SwitchFailedFolded { passed, .. } = &outcome {
            let value = parsed.get_str("passed");
            prop_assert!(value.is_some(), "missing passed in {:?}", line);
            let value = value.unwrap();
            let tokens: Vec<&str> = if value.is_empty() { vec![] } else { value.split(',').collect() };
            let want: Vec<&str> = passed.iter().map(|p| pass_token(*p)).collect();
            prop_assert_eq!(tokens, want);
        } else {
            prop_assert!(parsed.get("passed").is_none(), "unexpected passed in {:?}", line);
        }

        // Fields the outcome does not have are absent.
        for key in ["reason", "matching", "switch-reason", "into", "passed"] {
            let wanted = outcome_fields.iter().any(|(k, _)| *k == key);
            prop_assert_eq!(parsed.get(key).is_some(), wanted, "field {} in {:?}", key, line);
        }
    }
}
