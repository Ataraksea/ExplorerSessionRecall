// Feature: explorer-session-restore, Property 22: Session_File serialization round trip
//
// For any valid `SessionDoc` within the caps (up to 100 windows and 500 tabs)
// whose `WideString`s contain arbitrary UTF-16 code units up to 32,767 long,
// including unpaired surrogates, mixed normalization forms, `%`, `=`, spaces
// and control characters, `serialize(doc)` is valid UTF-8,
// `parse(serialize(doc)) == doc` (field-exact, including PIDL bytes and
// display names), and `serialize(parse(serialize(doc))) == serialize(doc)`.
//
// **Validates: Requirements 11.3, 11.5, 8.4**

mod common;

use common::strategies::*;
use esr_core::format::{parse, serialize, FORMAT_VERSION};
use esr_core::model::*;
use proptest::prelude::*;

/// Field-exact location comparison with a path for the failure message.
/// (`LocationId`'s `==` ignores PIDL bytes and display names.)
fn check_location(a: &LocationId, b: &LocationId, at: &str) -> Result<(), String> {
    if a.identical(b) {
        Ok(())
    } else {
        Err(format!("{at}: location differs\n  original: {a:?}\n  parsed:   {b:?}"))
    }
}

/// Compares every field of two documents exactly.
fn check_doc_identical(orig: &SessionDoc, parsed: &SessionDoc) -> Result<(), String> {
    let field = |name: &str, same: bool| if same { Ok(()) } else { Err(format!("{name} differs")) };
    field("version", orig.version == parsed.version)?;
    field("status", orig.status == parsed.status)?;
    field("failed_attempts", orig.failed_attempts == parsed.failed_attempts)?;
    field("saved_at", orig.saved_at == parsed.saved_at)?;
    field("cause", orig.cause == parsed.cause)?;

    let (s, p) = (&orig.snapshot, &parsed.snapshot);
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
    // Structural equality as a final safety net (uses the canonical keys).
    field("document (==)", orig == parsed)
}

/// A display device name with arbitrary (valid) Unicode, spaces, `%`, `=`
/// and controls. `DisplayRef::device` is a `String`, so unpaired surrogates
/// are replaced (they cannot occur in a device name).
fn device_name() -> impl Strategy<Value = String> {
    prop_oneof![
        3 => (1u32..=9).prop_map(|i| format!(r"\\.\DISPLAY{i}")),
        1 => wide_string_of(0usize..=40).prop_map(|w| w.to_string_lossy()),
    ]
}

/// Mostly small snapshots; occasionally snapshots up to the caps.
fn any_snapshot() -> impl Strategy<Value = Snapshot> {
    prop_oneof![
        9 => snapshot().boxed(),
        1 => snapshot_with(SnapshotParams::at_caps()).boxed(),
    ]
}

/// A valid document whose display device names are also fuzzed.
fn doc() -> impl Strategy<Value = SessionDoc> {
    session_doc_of(any_snapshot()).prop_flat_map(|doc| {
        let n = doc.snapshot.windows.len();
        (Just(doc), proptest::collection::vec(proptest::option::of(device_name()), n))
    })
    .prop_map(|(mut doc, devices)| {
        for (w, d) in doc.snapshot.windows.iter_mut().zip(devices) {
            if let Some(d) = d {
                w.placement.display.device = d;
            }
        }
        doc
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn session_file_round_trip(doc in doc()) {
        prop_assume!(validate_doc(&doc).is_ok());
        prop_assert_eq!(doc.version, FORMAT_VERSION);

        let text = serialize(&doc);
        // Valid UTF-8 (Req 11.3); LF only, no BOM.
        prop_assert!(std::str::from_utf8(text.as_bytes()).is_ok());
        let clean = !text.starts_with('\u{FEFF}') && !text.contains('\r');
        prop_assert!(clean, "BOM or CR in output");

        let parsed = match parse(text.as_bytes()) {
            Ok(p) => p,
            Err(e) => return Err(TestCaseError::fail(format!("parse failed: {e:?}"))),
        };
        if let Err(msg) = check_doc_identical(&doc, &parsed) {
            return Err(TestCaseError::fail(msg));
        }

        let again = serialize(&parsed);
        prop_assert!(again == text, "re-serialization differs from the original text");
    }
}

/// Deterministic example at the edges of Req 8.4: a 32,767-unit path beyond
/// MAX_PATH with unpaired surrogates, NFC/NFD forms, `%`, `=`, spaces and
/// controls, inside a search scope.
#[test]
fn max_length_unicode_path_round_trips() {
    let pattern: Vec<u16> = [
        &[u16::from(b'C'), u16::from(b':'), u16::from(b'\\')][..],
        &[0x00E9, 0x0065, 0x0301],      // é in NFC and NFD
        &[0xD800, u16::from(b'x')],     // lone high surrogate
        &[u16::from(b'y'), 0xDFFF],     // lone low surrogate
        &[0xD83D, 0xDE00],              // paired surrogate (😀)
        &[0x25, 0x3D, 0x20, 0x00, 0x0A, 0x7F, 0x5C], // % = space NUL LF DEL \
    ]
    .concat();
    let long = WideString(pattern.iter().copied().cycle().take(MAX_WIDE_LEN).collect());
    let scope = LocationId {
        case_sensitive: false,
        kind: LocationKind::FileSystem(Volume::Unc),
        parsing_name: long.clone(),
        display_name: long.clone(),
        pidl: (0..=255).collect(),
        search: None,
    };
    let tab = LocationId {
        case_sensitive: false,
        kind: LocationKind::Search,
        parsing_name: "search-ms:x".into(),
        display_name: WideString(vec![0xDC00]),
        pidl: vec![],
        search: Some(SearchSpec { query: long, scope: Box::new(scope) }),
    };
    let doc = SessionDoc {
        version: FORMAT_VERSION,
        status: Status::Consumed,
        failed_attempts: MAX_FAILED_ATTEMPTS,
        saved_at: Timestamp {
            year: 1,
            month: 2,
            day: 28,
            hour: 23,
            minute: 59,
            second: 59,
            millis: 999,
            offset_minutes: -1439,
        },
        cause: SaveCause::ExitThroughSettings,
        snapshot: Snapshot {
            windows: vec![WindowRec {
                id: WindowId(u32::MAX),
                tabs: vec![TabRec { location: tab }],
                active: 0,
                placement: Placement {
                    state: ShowState::Minimized { restore_maximized: true },
                    restored: Rect { left: i32::MIN, top: -1, right: i32::MAX, bottom: 0 },
                    display: DisplayRef {
                        device: "a b%c=d\u{1}".into(),
                        monitor: Rect { left: -3840, top: -2160, right: 0, bottom: 0 },
                    },
                },
            }],
            activation: vec![WindowId(u32::MAX)],
        },
    };
    assert_eq!(validate_doc(&doc), Ok(()));
    let text = serialize(&doc);
    let parsed = parse(text.as_bytes()).expect("parses");
    check_doc_identical(&doc, &parsed).unwrap();
    assert_eq!(serialize(&parsed), text);
}
