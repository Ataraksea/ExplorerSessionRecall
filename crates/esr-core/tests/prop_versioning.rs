// Feature: explorer-session-restore, Property 24: Format versioning
//
// For any valid `SessionDoc` and any supported format version v, parsing the
// test-only legacy writer output `write_v(v, doc)` yields a current-shape
// document that preserves status, windows, tabs, Location_Identifiers,
// active tabs, and Activation_Order (field-exact), with `version` = v. For
// any declared version greater than `FORMAT_VERSION`, `parse` returns
// `UnsupportedVersion { declared, newest: FORMAT_VERSION }` at the version
// value, without reading anything after it. Version 0 and values that do
// not fit a `u32` are `OutOfRange` at the same position.
//
// **Validates: Requirements 11.8, 11.9**

mod common;

use common::strategies::*;
use esr_core::format::parse::OLDEST_VERSION;
use esr_core::format::{parse, serialize, ParseError, ParseErrorKind, FORMAT_VERSION};
use esr_core::model::*;
use proptest::prelude::*;

/// Line and column of the version value in `ESR-SESSION version=N`
/// (`"ESR-SESSION version="` is 20 characters).
const VERSION_LINE: u32 = 1;
const VERSION_COLUMN: u32 = 21;

/// A writer of one format version.
type Writer = fn(&SessionDoc) -> String;

/// Test-only writers for every supported version older than
/// `FORMAT_VERSION`, oldest first. Version 1 is the only version today, so
/// the table is empty. When version N+1 is added, the current serializer's
/// grammar must be copied here as `(N, write_vN)`; `legacy_writers_cover_the_
/// migration_chain` fails until it is.
const LEGACY_WRITERS: &[(u32, Writer)] = &[];

/// Writes `doc` in Session_File_Format version `version`.
fn write_v(version: u32, doc: &SessionDoc) -> String {
    if version == FORMAT_VERSION {
        return serialize(doc);
    }
    match LEGACY_WRITERS.iter().find(|(v, _)| *v == version) {
        Some((_, write)) => write(doc),
        None => panic!("no legacy writer for supported version {version}"),
    }
}

/// Field-exact location comparison with a path for the failure message.
/// (`LocationId`'s `==` ignores PIDL bytes and display names.)
fn check_location(a: &LocationId, b: &LocationId, at: &str) -> Result<(), String> {
    if a.identical(b) {
        Ok(())
    } else {
        Err(format!("{at}: location differs\n  expected: {a:?}\n  parsed:   {b:?}"))
    }
}

/// Compares every field of two documents exactly.
fn check_doc_identical(expected: &SessionDoc, parsed: &SessionDoc) -> Result<(), String> {
    let field = |name: &str, same: bool| if same { Ok(()) } else { Err(format!("{name} differs")) };
    field("version", expected.version == parsed.version)?;
    field("status", expected.status == parsed.status)?;
    field("failed_attempts", expected.failed_attempts == parsed.failed_attempts)?;
    field("saved_at", expected.saved_at == parsed.saved_at)?;
    field("cause", expected.cause == parsed.cause)?;

    let (s, p) = (&expected.snapshot, &parsed.snapshot);
    field("activation order", s.activation == p.activation)?;
    field("window count", s.windows.len() == p.windows.len())?;
    for (wi, (w, q)) in s.windows.iter().zip(&p.windows).enumerate() {
        let at = format!("window #{wi} (id {})", w.id.0);
        field(&format!("{at} id"), w.id == q.id)?;
        field(&format!("{at} active"), w.active == q.active)?;
        field(&format!("{at} placement"), w.placement == q.placement)?;
        field(&format!("{at} tab count"), w.tabs.len() == q.tabs.len())?;
        for (ti, (t, u)) in w.tabs.iter().zip(&q.tabs).enumerate() {
            check_location(&t.location, &u.location, &format!("{at} tab {ti}"))?;
        }
    }
    field("document (==)", expected == parsed)
}

/// A valid document (validated by the generator's construction).
fn valid_doc() -> impl Strategy<Value = SessionDoc> {
    session_doc().prop_filter("valid document", |d| validate_doc(d).is_ok())
}

/// Every supported version, `OLDEST_VERSION..=FORMAT_VERSION`.
fn supported_version() -> impl Strategy<Value = u32> {
    OLDEST_VERSION..=FORMAT_VERSION
}

/// A declared version newer than `FORMAT_VERSION`, biased to the next one.
fn newer_version() -> impl Strategy<Value = u32> {
    prop_oneof![
        1 => Just(FORMAT_VERSION + 1),
        1 => (FORMAT_VERSION + 1)..=FORMAT_VERSION + 10,
        2 => (FORMAT_VERSION + 1)..=u32::MAX,
    ]
}

/// The text after the header's version value: what the parser must not
/// read once the version is rejected. It starts with a space, an LF, or is
/// empty, so it never joins the version token.
fn after_version() -> impl Strategy<Value = String> {
    prop_oneof![
        // A valid current-version body.
        3 => valid_doc().prop_map(|d| {
            let text = serialize(&d);
            let (_, body) = text.split_once('\n').expect("header line");
            format!("\n{body}")
        }),
        // Garbage lines (any valid UTF-8; UTF-8 is checked before parsing).
        2 => any::<String>().prop_map(|g| format!("\n{g}")),
        // Garbage on the header line itself, then anything.
        1 => (any::<String>(), any::<String>()).prop_map(|(a, b)| format!(" {a}\n{b}")),
        // Header only, without LF.
        1 => Just(String::new()),
        // Header only, with LF.
        1 => Just("\n".to_string()),
    ]
}

/// A canonical decimal that does not fit a `u32`.
fn overflowing_decimal() -> impl Strategy<Value = String> {
    prop_oneof![
        ((u64::from(u32::MAX) + 1)..=u64::MAX).prop_map(|n| n.to_string()),
        // Beyond u64 as well.
        "[1-9][0-9]{20,40}",
    ]
}

/// `text` with its header's version value replaced by `version`.
fn with_version(text: &str, version: &str) -> String {
    let (header, rest) = text.split_once('\n').expect("header line");
    assert_eq!(header, format!("ESR-SESSION version={FORMAT_VERSION}"));
    format!("ESR-SESSION version={version}\n{rest}")
}

fn version_error(kind: ParseErrorKind) -> ParseError {
    ParseError { line: VERSION_LINE, column: VERSION_COLUMN, kind }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// (a) Every supported version migrates to the current shape (Req 11.8).
    #[test]
    fn supported_versions_migrate_to_current(v in supported_version(), doc in valid_doc()) {
        let text = write_v(v, &doc);
        let parsed = match parse(text.as_bytes()) {
            Ok(p) => p,
            Err(e) => return Err(TestCaseError::fail(format!("version {v}: parse failed: {e:?}"))),
        };
        let expected = SessionDoc { version: v, ..doc.clone() };
        if let Err(msg) = check_doc_identical(&expected, &parsed) {
            return Err(TestCaseError::fail(format!("version {v}: {msg}")));
        }
        // The migrated document is current: it serializes exactly like the
        // original written in the current version.
        prop_assert!(serialize(&parsed) == serialize(&doc), "version {}: re-serialization differs", v);
    }

    /// (b) A newer declared version is rejected at the version value,
    /// whatever follows it (Req 11.9).
    #[test]
    fn newer_versions_are_unsupported(declared in newer_version(), rest in after_version()) {
        let text = format!("ESR-SESSION version={declared}{rest}");
        let expected = version_error(ParseErrorKind::UnsupportedVersion {
            declared,
            newest: FORMAT_VERSION,
        });
        prop_assert_eq!(parse(text.as_bytes()), Err(expected));
    }

    /// (c) Version 0 and versions beyond 32 bits are out of range.
    #[test]
    fn version_zero_and_overflow_are_out_of_range(
        version in prop_oneof![1 => Just("0".to_string()), 3 => overflowing_decimal()],
        doc in valid_doc(),
    ) {
        let text = with_version(&serialize(&doc), &version);
        prop_assert_eq!(parse(text.as_bytes()), Err(version_error(ParseErrorKind::OutOfRange)));
    }
}

/// The legacy writers cover exactly the supported versions older than the
/// current one, so every step of the migration chain is exercised.
#[test]
fn legacy_writers_cover_the_migration_chain() {
    let versions: Vec<u32> = LEGACY_WRITERS.iter().map(|(v, _)| *v).collect();
    let expected: Vec<u32> = (OLDEST_VERSION..FORMAT_VERSION).collect();
    assert_eq!(versions, expected);
    const { assert!(OLDEST_VERSION >= 1 && OLDEST_VERSION <= FORMAT_VERSION) };
}

/// The next version with an otherwise valid current body.
#[test]
fn next_version_with_valid_body_is_unsupported() {
    let doc = SessionDoc {
        version: FORMAT_VERSION,
        status: Status::Pending,
        failed_attempts: 0,
        saved_at: Timestamp {
            year: 2025,
            month: 6,
            day: 1,
            hour: 8,
            minute: 0,
            second: 0,
            millis: 0,
            offset_minutes: 120,
        },
        cause: SaveCause::Shutdown,
        snapshot: Snapshot { windows: vec![], activation: vec![] },
    };
    let text = with_version(&serialize(&doc), &(FORMAT_VERSION + 1).to_string());
    assert_eq!(
        parse(text.as_bytes()),
        Err(version_error(ParseErrorKind::UnsupportedVersion {
            declared: FORMAT_VERSION + 1,
            newest: FORMAT_VERSION,
        }))
    );
}
