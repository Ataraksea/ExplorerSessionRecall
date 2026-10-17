// Unit tests for `ParseError` line and column positions (task 4.9).
//
// Positions are 1-based and counted in Unicode scalar values (not bytes and
// not UTF-16 code units). Invalid UTF-8 is reported at its first bad byte; the
// end of the input at column 1 of the line after the last one; a missing LF at
// the column after the last character of the unterminated line.
//
// Inputs are built by serializing small hand-made documents and editing the
// text, and most expected positions are computed from the edited text, so the
// tests follow small changes to the format.
//
// Requirements: 11.6

use esr_core::format::{parse, serialize, ParseError, ParseErrorKind, FORMAT_VERSION};
use esr_core::model::*;
use ParseErrorKind::{InvalidUtf8, MissingElement, Syntax};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn zip(parse: &str, display: &str) -> LocationId {
    LocationId {
        case_sensitive: false,
        kind: LocationKind::ZipFolder,
        parsing_name: parse.into(),
        display_name: display.into(),
        pidl: Vec::new(),
        search: None,
    }
}

fn doc(device: &str, tabs: Vec<LocationId>) -> SessionDoc {
    let windows = if tabs.is_empty() {
        Vec::new()
    } else {
        vec![WindowRec {
            id: WindowId(1),
            tabs: tabs.into_iter().map(|location| TabRec { location }).collect(),
            active: 0,
            placement: Placement {
                state: ShowState::Normal,
                restored: Rect { left: 0, top: 0, right: 800, bottom: 600 },
                display: DisplayRef {
                    device: device.into(),
                    monitor: Rect { left: 0, top: 0, right: 1920, bottom: 1080 },
                },
            },
        }]
    };
    let activation = windows.iter().map(|w| w.id).collect();
    SessionDoc {
        version: FORMAT_VERSION,
        status: Status::Pending,
        failed_attempts: 0,
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
        cause: SaveCause::Checkpoint,
        snapshot: Snapshot { windows, activation },
    }
}

/// One window on a device named with 2-, 3- and 4-byte UTF-8 characters,
/// holding one tab `parse=é display=€😀`.
///
/// Line 6 is the `window` line, 7 the `tab` line, 10 `activation 1`, and 11
/// `end-session`.
fn multibyte_text() -> String {
    let text = serialize(&doc("DISPLAYé€😀", vec![zip("é", "€😀")]));
    assert_eq!(text.lines().nth(6), Some("tab kind=zip parse=é display=€😀 pidl="));
    assert_eq!(text.lines().count(), 11);
    assert!(parse(text.as_bytes()).is_ok());
    text
}

fn e(line: u32, column: u32, kind: ParseErrorKind) -> ParseError {
    ParseError { line, column, kind }
}

fn err(bytes: &[u8]) -> ParseError {
    match parse(bytes) {
        Ok(doc) => panic!("parsed {:?} into {doc:?}", String::from_utf8_lossy(bytes)),
        Err(e) => e,
    }
}

/// 1-based line and column (in scalar values) of byte offset `i` of `text`.
fn pos(text: &str, i: usize) -> (u32, u32) {
    let before = &text[..i];
    let line = 1 + before.matches('\n').count();
    let start = before.rfind('\n').map_or(0, |n| n + 1);
    let column = 1 + before[start..].chars().count();
    (line as u32, column as u32)
}

/// Position of the first occurrence of `needle` in `text`.
fn pos_of(text: &str, needle: &str) -> (u32, u32) {
    let i = text.find(needle).unwrap_or_else(|| panic!("{needle:?} not in {text}"));
    pos(text, i)
}

/// `text` with `from` replaced by `to` once (`from` must occur).
fn edit(text: &str, from: &str, to: &str) -> String {
    assert!(text.contains(from), "{from:?} not in {text}");
    text.replacen(from, to, 1)
}

fn at((line, column): (u32, u32), kind: ParseErrorKind) -> ParseError {
    e(line, column, kind)
}

// ---------------------------------------------------------------------------
// Columns on lines with multi-byte characters
// ---------------------------------------------------------------------------

#[test]
fn column_counts_scalar_values_not_bytes_or_utf16_units() {
    let text = multibyte_text();
    // `tab kind=zip parse=é display=€😀 pidx=`: `pidx` starts at column 33.
    // Counting UTF-8 bytes would give 39, UTF-16 units 34.
    assert_eq!(err(edit(&text, " pidl=", " pidx=").as_bytes()), e(7, 33, Syntax));

    // `tab kind=zip parse=<s> displax=…`: `displax` is at column 21 + |s|.
    for s in ["é", "€", "😀", "𝄞", "é€😀", "aé€😀z", "ü\u{301}"] {
        let text = serialize(&doc("D", vec![zip(s, "x")]));
        let text = edit(&text, " display=x pidl=", " displax=x pidl=");
        let scalars = s.chars().count() as u32;
        assert_eq!(err(text.as_bytes()), e(7, 21 + scalars, Syntax), "parse={s}");
    }
}

#[test]
fn field_errors_after_multibyte_characters() {
    let text = multibyte_text();

    // A malformed number after the multi-byte device name on the window line.
    let bad = edit(&text, "monitor=0,0,1920,1080", "monitor=0,0,1920,1x80");
    assert_eq!(err(bad.as_bytes()), at(pos_of(&bad, "x80"), Syntax));

    // An unknown field name after the device name.
    let bad = edit(&text, " monitor=", " monitxr=");
    assert_eq!(err(bad.as_bytes()), at(pos_of(&bad, "monitxr"), Syntax));

    // Extra content is reported at the space that introduces it.
    let bad = edit(&text, "€😀 pidl=\n", "€😀 pidl= é\n");
    assert_eq!(err(bad.as_bytes()), e(7, 38, Syntax));
    assert_eq!(err(bad.as_bytes()), at(pos_of(&bad, " é\n"), Syntax));

    // A base64 symbol outside the alphabet, after multi-byte characters.
    let bad = edit(&text, " pidl=\n", " pidl=AQ€D\n");
    assert_eq!(err(bad.as_bytes()), at(pos_of(&bad, "€D"), Syntax));
}

#[test]
fn errors_inside_escaped_values() {
    // `é b=c%😀` is written `é%20b%3Dc%25😀`: each escape is three columns.
    let text = serialize(&doc("D", vec![zip("x", "é b=c%😀")]));
    assert!(text.contains("tab kind=zip parse=x display=é%20b%3Dc%25😀 pidl=\n"));

    // A lowercase (malformed) escape is reported at its `%`: the value starts
    // at column 30 and has 13 scalar values, so the `%` is at column 43.
    let bad = edit(&text, "%25😀 pidl=", "%25😀%3d pidl=");
    assert_eq!(err(bad.as_bytes()), e(7, 43, Syntax));
    assert_eq!(err(bad.as_bytes()), at(pos_of(&bad, "%3d"), Syntax));

    // A truncated escape at the end of the value.
    let bad = edit(&text, "%25😀 pidl=", "%25😀%2 pidl=");
    assert_eq!(err(bad.as_bytes()), at(pos_of(&bad, "%2 "), Syntax));

    // A non-canonical escape of a character written raw (`%41` = `A`).
    let bad = edit(&text, "é%20b", "é%41b");
    assert_eq!(err(bad.as_bytes()), at(pos_of(&bad, "%41"), Syntax));

    // A raw `=` that must be escaped, after an escape and a 4-byte character.
    let bad = edit(&text, "%25😀 pidl=", "%25😀= pidl=");
    let raw_eq = bad.find("😀= ").unwrap() + "😀".len();
    assert_eq!(err(bad.as_bytes()), at(pos(&bad, raw_eq), Syntax));
    assert_eq!(err(bad.as_bytes()), e(7, 43, Syntax));

    // An escape in the parsing name shifts later columns by its escaped
    // length, not by the decoded length.
    let text = serialize(&doc("D", vec![zip("a b", "x")]));
    let bad = edit(&text, " display=x pidl=", " displax=x pidl=");
    // `tab kind=zip parse=a%20b displax=`: `displax` is at column 26.
    assert_eq!(err(bad.as_bytes()), e(7, 26, Syntax));
}

#[test]
fn line_ending_early_after_multibyte_characters() {
    let text = multibyte_text();
    // The line ends where `pidl=` is required: the column after `😀`.
    let bad = edit(&text, "€😀 pidl=\n", "€😀\n");
    assert_eq!(err(bad.as_bytes()), e(7, 32, MissingElement));
    // The same when the input ends there without an LF.
    let cut = &text[..text.find(" pidl=").unwrap()];
    assert_eq!(err(cut.as_bytes()), e(7, 32, MissingElement));
}

// ---------------------------------------------------------------------------
// Invalid UTF-8
// ---------------------------------------------------------------------------

/// Byte sequences that are not valid UTF-8 whatever ASCII byte (or the end
/// of the input) follows them.
const BAD_SEQUENCES: &[(&str, &[u8])] = &[
    ("lone continuation byte", &[0x80]),
    ("two continuation bytes", &[0xBF, 0x80]),
    ("truncated 2-byte sequence", &[0xC3]),
    ("truncated 3-byte sequence", &[0xE2, 0x82]),
    ("truncated 4-byte sequence", &[0xF0, 0x9F, 0x98]),
    ("overlong 2-byte '/'", &[0xC0, 0xAF]),
    ("overlong 2-byte 'A'", &[0xC1, 0x81]),
    ("overlong 3-byte '/'", &[0xE0, 0x80, 0xAF]),
    ("overlong 4-byte NUL", &[0xF0, 0x80, 0x80, 0x80]),
    ("UTF-8-encoded high surrogate", &[0xED, 0xA0, 0x80]),
    ("UTF-8-encoded low surrogate", &[0xED, 0xBF, 0xBF]),
    ("code point above U+10FFFF", &[0xF4, 0x90, 0x80, 0x80]),
    ("byte 0xFF", &[0xFF]),
    ("byte 0xFE", &[0xFE]),
];

/// `text` with `bad` inserted at byte offset `i`.
fn inject(text: &str, i: usize, bad: &[u8]) -> Vec<u8> {
    let mut bytes = text.as_bytes()[..i].to_vec();
    bytes.extend_from_slice(bad);
    bytes.extend_from_slice(&text.as_bytes()[i..]);
    bytes
}

#[test]
fn invalid_utf8_is_reported_at_its_first_byte() {
    let text = multibyte_text();
    let after_multibyte = text.find(" pidl=").unwrap();
    let points = [
        ("start of input", 0),
        ("inside the header line", "ESR-SESSION".len()),
        ("after the device name", text.find(" monitor=").unwrap()),
        ("after `€😀` on the tab line", after_multibyte),
        ("between `€` and `😀`", text.find("😀 pidl=").unwrap()),
        ("end of the tab line", text.find("\nactive").unwrap()),
        ("start of the trailer line", text.find("end-session").unwrap()),
        ("end of input", text.len()),
    ];
    for (name, bad) in BAD_SEQUENCES {
        for (point, i) in points {
            let bytes = inject(&text, i, bad);
            assert_eq!(err(&bytes), at(pos(&text, i), InvalidUtf8), "{name} at {point}");
        }
    }

    // A few positions spelled out.
    assert_eq!(err(&inject(&text, 0, &[0xFF])), e(1, 1, InvalidUtf8));
    assert_eq!(err(&inject(&text, after_multibyte, &[0xFF])), e(7, 32, InvalidUtf8));
    let trailer = text.find("end-session").unwrap();
    assert_eq!(err(&inject(&text, trailer, &[0x80])), e(11, 1, InvalidUtf8));
    assert_eq!(err(&inject(&text, text.len(), &[0xC3])), e(12, 1, InvalidUtf8));
}

#[test]
fn invalid_utf8_replacing_part_of_a_multibyte_character() {
    let text = multibyte_text();
    let emoji = text.find("😀 pidl=").unwrap();
    // `😀` is F0 9F 98 80; dropping its last byte leaves a truncated sequence
    // starting at the column of `😀` (31).
    let mut bytes = text.as_bytes().to_vec();
    bytes.remove(emoji + 3);
    assert_eq!(err(&bytes), e(7, 31, InvalidUtf8));
    // Corrupting its lead byte into a lone continuation byte.
    let mut bytes = text.as_bytes().to_vec();
    bytes[emoji] = 0x80;
    assert_eq!(err(&bytes), e(7, 31, InvalidUtf8));
    // Replacing `é` (C3 A9) on the tab line with an overlong encoding.
    let e_acute = text.find("é display=").unwrap();
    let mut bytes = text.as_bytes().to_vec();
    bytes[e_acute] = 0xC0;
    assert_eq!(err(&bytes), e(7, 20, InvalidUtf8));
}

#[test]
fn invalid_utf8_is_checked_before_syntax() {
    let text = multibyte_text();
    // A syntax error on line 2 and invalid UTF-8 on line 7: UTF-8 wins.
    let bad = edit(&text, "status pending", "status bogus");
    let i = bad.find(" pidl=").unwrap();
    assert_eq!(err(&inject(&bad, i, &[0xED, 0xA0, 0x80])), e(7, 32, InvalidUtf8));
    // The same after a missing trailer.
    let bad = text.strip_suffix("end-session\n").unwrap();
    assert_eq!(err(&inject(bad, bad.len(), &[0xFF])), e(11, 1, InvalidUtf8));
}

// ---------------------------------------------------------------------------
// Missing trailer
// ---------------------------------------------------------------------------

#[test]
fn missing_trailer() {
    for text in [multibyte_text(), serialize(&doc("D", Vec::new()))] {
        let lines = text.lines().count() as u32;
        let activation = text.find("activation").unwrap();
        let (act_line, _) = pos(&text, activation);
        assert_eq!(act_line, lines - 1);
        let act_text = text.lines().nth(lines as usize - 2).unwrap();
        let act_end = act_text.chars().count() as u32 + 1;

        // Input ends after the activation line.
        let cut = text.strip_suffix("end-session\n").unwrap();
        assert_eq!(err(cut.as_bytes()), e(lines, 1, MissingElement));
        // The activation line itself has no LF.
        let cut = text.strip_suffix("\nend-session\n").unwrap();
        assert_eq!(err(cut.as_bytes()), e(lines - 1, act_end, MissingElement));
        // Input ends before the activation line.
        let cut = &text[..activation];
        assert_eq!(err(cut.as_bytes()), e(lines - 1, 1, MissingElement));
        // The trailer has no final LF: reported just after `end-session`.
        let cut = text.strip_suffix('\n').unwrap();
        assert_eq!(err(cut.as_bytes()), e(lines, 12, MissingElement));

        // Partial trailer words, with and without LF, are unknown keywords.
        let trailer = "end-session";
        let body = text.strip_suffix("end-session\n").unwrap();
        for k in 1..trailer.len() {
            let mut partial = format!("{body}{}", &trailer[..k]);
            assert_eq!(err(partial.as_bytes()), e(lines, 1, Syntax), "{partial:?}");
            partial.push('\n');
            assert_eq!(err(partial.as_bytes()), e(lines, 1, Syntax), "{partial:?}");
        }
        // A misspelt trailer with a multi-byte character.
        let bad = edit(&text, "end-session\n", "end-sessión\n");
        assert_eq!(err(bad.as_bytes()), e(lines, 1, Syntax));
        // Content after the trailer word: at the space that introduces it.
        let bad = edit(&text, "end-session\n", "end-session 😀\n");
        assert_eq!(err(bad.as_bytes()), e(lines, 12, Syntax));
    }
}

// ---------------------------------------------------------------------------
// CRLF line endings
// ---------------------------------------------------------------------------

#[test]
fn crlf_line_endings() {
    let text = multibyte_text();

    // Every line CRLF: the header's version value `1\r` is malformed at the CR.
    let crlf = text.replace('\n', "\r\n");
    assert_eq!(err(crlf.as_bytes()), e(1, 22, Syntax));

    // CRLF on single lines: reported at the CR, or at the field or keyword
    // that the CR makes unrecognisable.
    let cases = [
        ("status pending\n", 2, Some(8)),
        ("monitor=0,0,1920,1080\n", 6, None),
        ("€😀 pidl=\n", 7, None),
        ("active 0\n", 8, None),
        ("end-window\n", 9, Some(1)),
        ("activation 1\n", 10, None),
        ("end-session\n", 11, Some(1)),
    ];
    for (line_end, line, column) in cases {
        let bad = edit(&text, line_end, &line_end.replace('\n', "\r\n"));
        let cr = bad.find('\r').unwrap();
        let (cr_line, cr_column) = pos(&bad, cr);
        assert_eq!(cr_line, line);
        let expected = e(line, column.unwrap_or(cr_column), Syntax);
        assert_eq!(err(bad.as_bytes()), expected, "CRLF after {line_end:?}");
    }
    // The CR on the tab line follows multi-byte characters: column 38.
    let bad = edit(&text, "€😀 pidl=\n", "€😀 pidl=\r\n");
    assert_eq!(err(bad.as_bytes()), e(7, 38, Syntax));
    // A trailing CR with no LF at all.
    let bad = edit(&text, "end-session\n", "end-session\r");
    assert_eq!(err(bad.as_bytes()), e(11, 1, Syntax));
}
