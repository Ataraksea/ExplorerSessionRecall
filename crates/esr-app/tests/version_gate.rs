//! Table-driven tests for the version gate (Requirements 1.2, 1.3).
//!
//! Only the pure decision (`is_supported`) and message text
//! (`unsupported_message`) are exercised; no message box is shown.

use esr_app::version::{
    MIN_BUILD, MIN_UBR, MINIMUM_VERSION_TEXT, OsVersion, is_supported, unsupported_message,
};

/// (label, build, ubr, expected supported)
const TABLE: &[(&str, u32, Option<u32>, bool)] = &[
    // Degenerate values.
    ("build 0, no UBR", 0, None, false),
    ("build 0, UBR max", 0, Some(u32::MAX), false),
    // Windows 10.
    ("Win10 1507 RTM", 10240, Some(16384), false),
    ("Win10 1507 no UBR", 10240, None, false),
    ("Win10 2004", 19041, Some(1415), false),
    ("Win10 21H2", 19044, Some(3086), false),
    ("Win10 22H2", 19045, Some(4170), false),
    ("Win10 22H2 huge UBR", 19045, Some(u32::MAX), false),
    ("Win10 22H2 no UBR", 19045, None, false),
    // Windows 11 21H2.
    ("Win11 21H2", 22000, Some(2538), false),
    ("Win11 21H2 UBR 675", 22000, Some(675), false),
    ("Win11 21H2 no UBR", 22000, None, false),
    // Windows 11 22H2: tabs arrived in 22621.675.
    ("22H2 no UBR", 22621, None, false),
    ("22H2 RTM .0", 22621, Some(0), false),
    ("22H2 .1", 22621, Some(1), false),
    ("22H2 .674", 22621, Some(674), false),
    ("22H2 .675 (first with tabs)", 22621, Some(675), true),
    ("22H2 .676", 22621, Some(676), true),
    ("22H2 .3880", 22621, Some(3880), true),
    ("22H2 UBR max", 22621, Some(u32::MAX), true),
    // Windows 11 23H2.
    ("23H2 .3880", 22631, Some(3880), true),
    ("23H2 .0", 22631, Some(0), true),
    ("23H2 no UBR", 22631, None, true),
    // Windows 11 24H2.
    ("24H2 .2033", 26100, Some(2033), true),
    ("24H2 no UBR", 26100, None, true),
    // 25H2 enablement package.
    ("25H2 eKB .4652", 26200, Some(4652), true),
    ("25H2 eKB no UBR", 26200, None, true),
    // Insider (Canary).
    ("Insider 27723", 27723, Some(1000), true),
    ("Insider 27723 no UBR", 27723, None, true),
    // Upper bound.
    ("build max, no UBR", u32::MAX, None, true),
    ("build max, UBR 0", u32::MAX, Some(0), true),
];

#[test]
fn constants_match_first_tabbed_build() {
    assert_eq!(MIN_BUILD, 22621);
    assert_eq!(MIN_UBR, 675);
}

#[test]
fn version_gate_table() {
    let failures: Vec<String> = TABLE
        .iter()
        .filter(|&&(_, build, ubr, expected)| is_supported(build, ubr) != expected)
        .map(|&(label, build, ubr, expected)| {
            format!("{label}: is_supported({build}, {ubr:?}) expected {expected}")
        })
        .collect();
    assert!(failures.is_empty(), "gate mismatches:\n{}", failures.join("\n"));
}

#[test]
fn os_version_method_agrees_with_free_function() {
    for &(label, build, ubr, expected) in TABLE {
        let v = OsVersion { major: 10, minor: 0, build, ubr };
        assert_eq!(v.is_supported(), expected, "{label}");
    }
}

fn os(build: u32, ubr: Option<u32>) -> OsVersion {
    OsVersion { major: 10, minor: 0, build, ubr }
}

#[test]
fn message_names_minimum_for_every_unsupported_row() {
    for &(label, build, ubr, expected) in TABLE {
        if expected {
            continue;
        }
        let text = unsupported_message(Some(os(build, ubr)));
        assert!(text.contains(MINIMUM_VERSION_TEXT), "{label}: {text}");
        assert!(text.contains("22621.675"), "{label}: {text}");
    }
    let text = unsupported_message(None);
    assert!(text.contains(MINIMUM_VERSION_TEXT), "{text}");
    assert!(text.contains("22621.675"), "{text}");
}

#[test]
fn message_includes_detected_build_and_ubr_when_known() {
    let cases = [
        (os(19045, Some(4170)), "build 19045.4170."),
        (os(22000, Some(2538)), "build 22000.2538."),
        (os(22621, Some(674)), "build 22621.674."),
        (os(22621, Some(0)), "build 22621.0."),
        (os(0, Some(u32::MAX)), "build 0.4294967295."),
    ];
    for (v, expected) in cases {
        let text = unsupported_message(Some(v));
        assert!(text.contains(expected), "expected {expected:?} in {text:?}");
    }
}

#[test]
fn message_omits_ubr_when_unknown() {
    for build in [0, 10240, 19045, 22000, 22621] {
        let text = unsupported_message(Some(os(build, None)));
        let detected = format!("This PC is running build {build}.");
        assert!(text.contains(&detected), "{text:?}");
        // The detected-build sentence ends right after the build number:
        // no ".<ubr>" follows it.
        assert!(text.ends_with(&detected), "{text:?}");
        assert!(!text.contains(&format!("{build}.0")), "{text:?}");
    }
}

#[test]
fn message_without_detection_has_no_detected_build() {
    let text = unsupported_message(None);
    assert!(!text.contains("This PC is running"), "{text:?}");
}
