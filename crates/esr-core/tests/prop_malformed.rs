// Feature: explorer-session-restore, Property 23: Malformed input is rejected with a position and no partial result
//
// For any valid serialization mutated in one of these ways, `parse` returns
// `Err(ParseError)` with the matching kind and a line/column that lies within
// the input, and never returns a partial snapshot:
// - truncated at a random byte;
// - with invalid UTF-8 bytes injected;
// - with a keyword or field corrupted;
// - with a required line removed;
// - with a numeric value pushed outside its range;
// - made semantically invalid (active index out of bounds, activation not a
//   permutation, empty window).
// For any input that `parse` accepts, `validate` succeeds on the result.
//
// Where the first problem of a mutation is known, the exact line, column and
// kind are checked against the position rules in `format/parse.rs`. Random
// byte edits may happen to yield another valid text; such a text must then be
// canonical (`serialize(parse(t)) == t`) and its document valid.
//
// **Validates: Requirements 11.4, 11.6**

mod common;

use common::strategies::*;
use esr_core::format::{parse, serialize, ParseError, ParseErrorKind as K};
use esr_core::model::*;
use proptest::prelude::*;
use proptest::sample::{select, Index};

// ---------------------------------------------------------------------------
// Text helpers
// ---------------------------------------------------------------------------

fn n32(n: usize) -> u32 {
    u32::try_from(n).expect("fits u32")
}

/// Length in Unicode scalar values (the parser's column unit).
fn chars(s: &str) -> u32 {
    n32(s.chars().count())
}

/// 1-based line and column of the position just after `prefix`.
fn end_of(prefix: &str) -> (u32, u32) {
    let line = 1 + prefix.matches('\n').count();
    let last = prefix.rfind('\n').map_or(prefix, |i| &prefix[i + 1..]);
    (n32(line), 1 + chars(last))
}

/// An error on the line with 0-based index `line`.
fn err_at(line: usize, column: u32, kind: K) -> ParseError {
    ParseError { line: n32(line + 1), column, kind }
}

/// The lines of a serialized document, without their LFs.
fn split_lines(text: &str) -> Vec<String> {
    let body = text.strip_suffix('\n').expect("serializer ends every line with LF");
    body.split('\n').map(str::to_owned).collect()
}

fn join_lines(lines: &[String]) -> String {
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

fn keyword(line: &str) -> &str {
    line.split(' ').next().unwrap_or("")
}

/// Every space-separated token of `line` with its 1-based column.
fn tokens(line: &str) -> Vec<(u32, &str)> {
    let mut col = 1;
    line.split(' ')
        .map(|t| {
            let at = col;
            col += chars(t) + 1;
            (at, t)
        })
        .collect()
}

/// The key of a `key=value` token (the whole token if it has no `=`).
fn key(tok: &str) -> &str {
    tok.split('=').next().unwrap_or(tok)
}

/// Column of the value of the `key=value` token `tok` at column `col`.
fn value_col(col: u32, tok: &str) -> u32 {
    col + chars(key(tok)) + 1
}

fn replace_token(line: &str, idx: usize, new: &str) -> String {
    let mut toks: Vec<&str> = line.split(' ').collect();
    toks[idx] = new;
    toks.join(" ")
}

fn remove_token(line: &str, idx: usize) -> String {
    let mut toks: Vec<&str> = line.split(' ').collect();
    toks.remove(idx);
    toks.join(" ")
}

/// Replaces component `comp` (0..4) of the rect field at token `idx`;
/// returns the new line and the column of that component.
fn edit_rect(line: &str, idx: usize, comp: usize, value: &str) -> (String, u32) {
    let (col, tok) = tokens(line)[idx];
    let (k, v) = tok.split_once('=').expect("rect field");
    let mut parts: Vec<String> = v.split(',').map(str::to_owned).collect();
    let comp_col = value_col(col, tok) + parts[..comp].iter().map(|p| chars(p) + 1).sum::<u32>();
    parts[comp] = value.to_owned();
    (replace_token(line, idx, &format!("{k}={}", parts.join(","))), comp_col)
}

/// The first 160 characters of `s`, for failure messages.
fn clip(s: &str) -> String {
    s.chars().take(160).collect()
}

// ---------------------------------------------------------------------------
// Document layout
// ---------------------------------------------------------------------------

/// Fixed line indices of the version 1 header.
const HEADER: usize = 0;
const FAILED_ATTEMPTS: usize = 2;
const SAVED_AT: usize = 3;

/// Token indices on a `window` line.
const RESTORED: usize = 3;
const MONITOR: usize = 5;

#[derive(Clone, Copy, Debug)]
struct WindowAt {
    /// The `window` line.
    header: usize,
    /// The `active` line; the tab lines lie strictly between the two.
    active: usize,
}

/// A serialized document split into lines, with the line of every block.
struct Layout {
    lines: Vec<String>,
    windows: Vec<WindowAt>,
}

impl Layout {
    fn of(text: &str) -> Self {
        let lines = split_lines(text);
        let windows = (0..lines.len())
            .filter(|&i| keyword(&lines[i]) == "window")
            .map(|header| {
                let active = (header..lines.len())
                    .find(|&a| keyword(&lines[a]) == "active")
                    .expect("every window has an active line");
                WindowAt { header, active }
            })
            .collect();
        Self { lines, windows }
    }

    fn activation(&self) -> usize {
        self.lines.len() - 2
    }

    fn text(&self) -> String {
        join_lines(&self.lines)
    }

    fn window(&self, pick: Index) -> Option<WindowAt> {
        (!self.windows.is_empty()).then(|| self.windows[pick.index(self.windows.len())])
    }
}

// ---------------------------------------------------------------------------
// Checks
// ---------------------------------------------------------------------------

/// Parses `bytes`. An error must lie within the input: a line that exists
/// (or the line after the last LF, for the end of input) and a column from 1
/// to one past the line's last character. A success is never partial: the
/// document is valid and re-serializes to exactly `bytes`.
fn parse_checked(bytes: &[u8]) -> Result<Result<SessionDoc, ParseError>, TestCaseError> {
    let result = parse(bytes);
    match &result {
        Err(e) => {
            // Lossy decoding agrees with the strict decoder up to the first
            // invalid byte, so it bounds InvalidUtf8 positions too.
            let text = String::from_utf8_lossy(bytes);
            let lines: Vec<&str> = text.split('\n').collect();
            let line_ok = e.line >= 1 && (e.line as usize) <= lines.len();
            let col_ok = line_ok && e.column >= 1 && e.column <= chars(lines[e.line as usize - 1]) + 1;
            prop_assert!(col_ok, "error {e:?} outside the input ({} lines)", lines.len());
        }
        Ok(doc) => {
            prop_assert!(validate_doc(doc).is_ok(), "accepted an invalid document");
            prop_assert!(serialize(doc).as_bytes() == bytes, "accepted non-canonical text");
        }
    }
    Ok(result)
}

/// The input must be rejected; returns the error.
fn rejected(bytes: &[u8]) -> Result<ParseError, TestCaseError> {
    match parse_checked(bytes)? {
        Ok(_) => Err(TestCaseError::fail("malformed input was accepted")),
        Err(e) => Ok(e),
    }
}

/// The input must be rejected with exactly `expected`.
fn rejected_with(bytes: &[u8], expected: ParseError, context: &str) -> Result<(), TestCaseError> {
    let e = rejected(bytes)?;
    prop_assert_eq!(e, expected, "{}", clip(context));
    Ok(())
}

fn valid_doc() -> impl Strategy<Value = SessionDoc> {
    session_doc().prop_filter("valid document", |d| validate_doc(d).is_ok())
}

// ---------------------------------------------------------------------------
// Mutations with parameters
// ---------------------------------------------------------------------------

/// Byte sequences that are invalid UTF-8 from their first byte when
/// inserted at a character boundary (the following byte is never a
/// continuation byte).
const BAD_UTF8: &[&[u8]] = &[
    &[0x80],
    &[0xBF],
    &[0xC0, 0x80],             // overlong NUL
    &[0xC1, 0xBF],             // overlong
    &[0xC3],                   // lead byte without continuation
    &[0xE2, 0x82],             // truncated 3-byte sequence
    &[0xED, 0xA0, 0x80],       // encoded surrogate D800
    &[0xF0, 0x9F, 0x98],       // truncated 4-byte sequence
    &[0xF4, 0x90, 0x80, 0x80], // above U+10FFFF
    &[0xF8, 0x88, 0x80, 0x80, 0x80],
    &[0xFE],
    &[0xFF],
];

#[derive(Clone, Debug)]
enum OutOfRange {
    Version0,
    FailedAttempts(u64),
    WindowId(Index, u64),
    Rect { window: Index, monitor: bool, comp: usize, value: i64 },
    /// A decimal beyond `u64::MAX`.
    Active(Index, String),
    Activation(Index, u64),
    /// Timestamp field 0..8 (year, month, day, hour, minute, second, offset
    /// hours, offset minutes) and a seed for its value.
    Time(usize, u8),
}

fn out_of_range() -> impl Strategy<Value = OutOfRange> {
    let beyond_u32 = || (u64::from(u32::MAX) + 1)..=u64::MAX;
    let outside_i32 = prop_oneof![(i64::from(i32::MAX) + 1)..=i64::MAX, i64::MIN..i64::from(i32::MIN)];
    prop_oneof![
        Just(OutOfRange::Version0),
        (u64::from(MAX_FAILED_ATTEMPTS) + 1..=u64::MAX).prop_map(OutOfRange::FailedAttempts),
        (any::<Index>(), beyond_u32()).prop_map(|(w, n)| OutOfRange::WindowId(w, n)),
        (any::<Index>(), any::<bool>(), 0usize..4, outside_i32)
            .prop_map(|(window, monitor, comp, value)| OutOfRange::Rect { window, monitor, comp, value }),
        (any::<Index>(), "[1-9][0-9]{20,24}").prop_map(|(w, n)| OutOfRange::Active(w, n)),
        (any::<Index>(), beyond_u32()).prop_map(|(e, n)| OutOfRange::Activation(e, n)),
        (0usize..8, any::<u8>()).prop_map(|(f, s)| OutOfRange::Time(f, s)),
    ]
}

#[derive(Clone, Debug)]
enum Semantic {
    ActiveOutOfBounds { window: Index, excess: u32 },
    ActivationUnknown { entry: Index, id: u32 },
    ActivationRepeated { first: Index, second: Index },
    ActivationIncomplete { entry: Index },
    DuplicateWindowId { first: Index, second: Index },
    NonPositiveRestored { window: Index, left: i32, top: i32, width: i32, height: i32 },
    EmptyWindow { window: Index },
}

fn semantic() -> impl Strategy<Value = Semantic> {
    let degenerate_size = prop_oneof![(-1000i32..=0, -1000i32..=1000), (-1000i32..=1000, -1000i32..=0)];
    prop_oneof![
        (any::<Index>(), 0u32..1_000_000)
            .prop_map(|(window, excess)| Semantic::ActiveOutOfBounds { window, excess }),
        (any::<Index>(), any::<u32>()).prop_map(|(entry, id)| Semantic::ActivationUnknown { entry, id }),
        (any::<Index>(), any::<Index>())
            .prop_map(|(first, second)| Semantic::ActivationRepeated { first, second }),
        any::<Index>().prop_map(|entry| Semantic::ActivationIncomplete { entry }),
        (any::<Index>(), any::<Index>())
            .prop_map(|(first, second)| Semantic::DuplicateWindowId { first, second }),
        (any::<Index>(), -10_000i32..=10_000, -10_000i32..=10_000, degenerate_size).prop_map(
            |(window, left, top, (width, height))| Semantic::NonPositiveRestored {
                window,
                left,
                top,
                width,
                height
            }
        ),
        any::<Index>().prop_map(|window| Semantic::EmptyWindow { window }),
    ]
}

#[derive(Clone, Debug)]
enum SyntaxEdit {
    Bom,
    Cr(Index),
    DoubledSpace(Index),
    AfterTrailer(String),
    LeadingZero(Index),
    NegativeZero { window: Index, monitor: bool, comp: usize },
}

fn syntax_edit() -> impl Strategy<Value = SyntaxEdit> {
    prop_oneof![
        Just(SyntaxEdit::Bom),
        any::<Index>().prop_map(SyntaxEdit::Cr),
        any::<Index>().prop_map(SyntaxEdit::DoubledSpace),
        "(.|\n){1,20}".prop_map(SyntaxEdit::AfterTrailer),
        any::<Index>().prop_map(SyntaxEdit::LeadingZero),
        (any::<Index>(), any::<bool>(), 0usize..4)
            .prop_map(|(window, monitor, comp)| SyntaxEdit::NegativeZero { window, monitor, comp }),
    ]
}

#[derive(Clone, Debug)]
enum ByteEdit {
    Flip(Index, u8),
    Insert(Index, u8),
    Delete(Index),
    Replace(Index, u8),
}

fn byte_edit() -> impl Strategy<Value = ByteEdit> {
    prop_oneof![
        (any::<Index>(), 1u8..=255).prop_map(|(i, m)| ByteEdit::Flip(i, m)),
        (any::<Index>(), any::<u8>()).prop_map(|(i, b)| ByteEdit::Insert(i, b)),
        any::<Index>().prop_map(ByteEdit::Delete),
        (any::<Index>(), any::<u8>()).prop_map(|(i, b)| ByteEdit::Replace(i, b)),
    ]
}

/// Tokens that `parse` reads with a plain `field`/`arg` (not the optional
/// `query=` of a search tab, whose absence means "no search group").
fn required_token(tok: &str) -> bool {
    !key(tok).ends_with("query")
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 200, ..ProptestConfig::default() })]

    /// Truncation at any byte is rejected. At a line boundary it is the end
    /// of input (column 1 of the next line), without the final LF it is the
    /// missing LF (after the last character), and inside a multi-byte
    /// character it is invalid UTF-8 at that character; otherwise the error
    /// is on the cut line.
    #[test]
    fn truncated_input_is_rejected(doc in valid_doc(), at_line_end in any::<bool>(), cut in any::<Index>()) {
        let text = serialize(&doc);
        let k = if at_line_end {
            let ends: Vec<usize> = std::iter::once(0)
                .chain(text.match_indices('\n').map(|(i, _)| i + 1).filter(|&i| i < text.len()))
                .chain(std::iter::once(text.len() - 1))
                .collect();
            ends[cut.index(ends.len())]
        } else {
            cut.index(text.len())
        };
        let bytes = &text.as_bytes()[..k];
        let e = rejected(bytes)?;
        if !text.is_char_boundary(k) {
            let start = (0..k).rev().find(|&b| text.is_char_boundary(b)).unwrap_or(0);
            let (line, column) = end_of(&text[..start]);
            prop_assert_eq!(e, ParseError { line, column, kind: K::InvalidUtf8 });
        } else {
            let (line, column) = end_of(&text[..k]);
            if k == 0 || bytes[k - 1] == b'\n' || k == text.len() - 1 {
                prop_assert_eq!(e, ParseError { line, column, kind: K::MissingElement });
            } else {
                prop_assert_ne!(e.kind, K::InvalidUtf8);
                prop_assert_eq!(e.line, line, "error {:?} not on the cut line", e);
            }
        }
    }

    /// Invalid UTF-8 inserted at a character boundary, or the high bit set
    /// on an ASCII byte, is reported at that byte's line and column.
    #[test]
    fn invalid_utf8_is_reported_at_the_first_bad_byte(
        doc in valid_doc(),
        at in any::<Index>(),
        bad in select(BAD_UTF8.to_vec()),
        flip in any::<bool>(),
    ) {
        let text = serialize(&doc);
        let mut bytes = text.as_bytes().to_vec();
        let p = if flip {
            let ascii: Vec<usize> = (0..bytes.len()).filter(|&i| bytes[i].is_ascii()).collect();
            let p = ascii[at.index(ascii.len())];
            bytes[p] |= 0x80;
            p
        } else {
            let bounds: Vec<usize> = text.char_indices().map(|(i, _)| i).chain([text.len()]).collect();
            let p = bounds[at.index(bounds.len())];
            let tail = bytes.split_off(p);
            bytes.extend_from_slice(bad);
            bytes.extend(tail);
            p
        };
        let (line, column) = end_of(&text[..p]);
        rejected_with(&bytes, ParseError { line, column, kind: K::InvalidUtf8 }, "")?;
    }

    /// Removing any line other than a `tab` line makes the line now in its
    /// place (or the end of input) a missing element; removing every tab of
    /// a window is an empty window, reported at its `active` line.
    #[test]
    fn removed_line_is_a_missing_element(doc in valid_doc(), pick in any::<Index>(), all_tabs in any::<bool>()) {
        let mut layout = Layout::of(&serialize(&doc));
        if all_tabs && !layout.windows.is_empty() {
            let w = layout.window(pick).unwrap();
            layout.lines.drain(w.header + 1..w.active);
            let expected = err_at(w.header + 1, 1, K::InvalidSnapshot);
            rejected_with(layout.text().as_bytes(), expected, "window without tabs")?;
        } else {
            let eligible: Vec<usize> =
                (0..layout.lines.len()).filter(|&i| keyword(&layout.lines[i]) != "tab").collect();
            let i = eligible[pick.index(eligible.len())];
            let removed = layout.lines.remove(i);
            rejected_with(layout.text().as_bytes(), err_at(i, 1, K::MissingElement), &removed)?;
        }
    }

    /// A repeated non-`tab` line is a missing element at the copy (the next
    /// required line is missing), except a second trailer, which is content
    /// after the trailer. A repeated `tab` line is just another tab.
    #[test]
    fn duplicated_line_is_rejected_at_the_copy(doc in valid_doc(), pick in any::<Index>()) {
        let mut layout = Layout::of(&serialize(&doc));
        let i = pick.index(layout.lines.len());
        let copy = layout.lines[i].clone();
        layout.lines.insert(i + 1, copy.clone());
        let bytes = layout.text();
        match keyword(&copy) {
            "tab" => {
                let _ = parse_checked(bytes.as_bytes())?;
            }
            "end-session" => rejected_with(bytes.as_bytes(), err_at(i + 1, 1, K::Syntax), &copy)?,
            _ => rejected_with(bytes.as_bytes(), err_at(i + 1, 1, K::MissingElement), &copy)?,
        }
    }

    /// A corrupted keyword is a syntax error at column 1 of its line.
    #[test]
    fn corrupted_keyword_is_a_syntax_error(
        doc in valid_doc(),
        pick in any::<Index>(),
        suffix in select(vec!['X', '_', '0', '-', 'é', '\u{1F600}']),
        uppercase in any::<bool>(),
    ) {
        let mut layout = Layout::of(&serialize(&doc));
        let i = pick.index(layout.lines.len());
        let line = layout.lines[i].clone();
        let kw = keyword(&line);
        let bad = if uppercase && kw != "ESR-SESSION" { kw.to_uppercase() } else { format!("{kw}{suffix}") };
        layout.lines[i] = replace_token(&line, 0, &bad);
        rejected_with(layout.text().as_bytes(), err_at(i, 1, K::Syntax), &layout.lines[i])?;
    }

    /// A required field or argument removed from a line: the token now in
    /// its place has the wrong name (syntax error at it), or, if it was the
    /// last one, the line ends too early (missing element after its end).
    #[test]
    fn removed_field_is_rejected_at_its_place(doc in valid_doc(), pick in any::<Index>()) {
        let mut layout = Layout::of(&serialize(&doc));
        let candidates: Vec<(usize, usize)> = layout
            .lines
            .iter()
            .enumerate()
            .filter(|(_, l)| keyword(l) != "activation" && keyword(l) != "end-window" && keyword(l) != "end-session")
            .flat_map(|(i, l)| {
                tokens(l)
                    .into_iter()
                    .enumerate()
                    .skip(1)
                    .filter(|(_, (_, t))| required_token(t))
                    .map(move |(j, _)| (i, j))
            })
            .collect();
        let (i, j) = candidates[pick.index(candidates.len())];
        let toks = tokens(&layout.lines[i]);
        let (col, _) = toks[j];
        let last = j + 1 == toks.len();
        let line = remove_token(&layout.lines[i], j);
        let expected =
            if last { err_at(i, chars(&line) + 1, K::MissingElement) } else { err_at(i, col, K::Syntax) };
        layout.lines[i] = line;
        rejected_with(layout.text().as_bytes(), expected, &layout.lines[i])?;
    }

    /// A field whose name is corrupted is a syntax error at the field.
    #[test]
    fn corrupted_field_name_is_a_syntax_error(doc in valid_doc(), pick in any::<Index>()) {
        let mut layout = Layout::of(&serialize(&doc));
        let candidates: Vec<(usize, usize, u32)> = layout
            .lines
            .iter()
            .enumerate()
            .filter(|(_, l)| matches!(keyword(l), "ESR-SESSION" | "window" | "tab"))
            .flat_map(|(i, l)| {
                tokens(l)
                    .into_iter()
                    .enumerate()
                    .skip(1)
                    .filter(|(_, (_, t))| required_token(t))
                    .map(move |(j, (col, _))| (i, j, col))
            })
            .collect();
        let (i, j, col) = candidates[pick.index(candidates.len())];
        let tok = layout.lines[i].split(' ').nth(j).unwrap().to_owned();
        layout.lines[i] = replace_token(&layout.lines[i], j, &format!("X{tok}"));
        rejected_with(layout.text().as_bytes(), err_at(i, col, K::Syntax), &layout.lines[i])?;
    }

    /// A well-formed number outside its field's range is OutOfRange at the
    /// number (at the timestamp for an impossible date or time).
    #[test]
    fn number_out_of_range_is_rejected(doc in valid_doc(), edit in out_of_range()) {
        let mut layout = Layout::of(&serialize(&doc));
        let (i, column) = match &edit {
            OutOfRange::Version0 => {
                layout.lines[HEADER] = "ESR-SESSION version=0".to_owned();
                (HEADER, 21)
            }
            OutOfRange::FailedAttempts(n) => {
                layout.lines[FAILED_ATTEMPTS] = format!("failed-attempts {n}");
                (FAILED_ATTEMPTS, 17)
            }
            OutOfRange::WindowId(w, n) => {
                let Some(w) = layout.window(*w) else { return Err(TestCaseError::reject("no window")) };
                layout.lines[w.header] = replace_token(&layout.lines[w.header], 1, &format!("id={n}"));
                (w.header, 11)
            }
            OutOfRange::Rect { window, monitor, comp, value } => {
                let Some(w) = layout.window(*window) else { return Err(TestCaseError::reject("no window")) };
                let idx = if *monitor { MONITOR } else { RESTORED };
                let (line, col) = edit_rect(&layout.lines[w.header], idx, *comp, &value.to_string());
                layout.lines[w.header] = line;
                (w.header, col)
            }
            OutOfRange::Active(w, n) => {
                let Some(w) = layout.window(*w) else { return Err(TestCaseError::reject("no window")) };
                layout.lines[w.active] = format!("active {n}");
                (w.active, 8)
            }
            OutOfRange::Activation(e, n) => {
                let a = layout.activation();
                let entries = tokens(&layout.lines[a]).len() - 1;
                prop_assume!(entries > 0);
                let j = 1 + e.index(entries);
                let col = tokens(&layout.lines[a])[j].0;
                layout.lines[a] = replace_token(&layout.lines[a], j, &n.to_string());
                (a, col)
            }
            OutOfRange::Time(field, seed) => {
                let s = u32::from(*seed);
                let (range, value) = match *field {
                    0 => (0..4, "0000".to_owned()),
                    1 => (5..7, if s % 2 == 0 { "00".to_owned() } else { (13 + s % 87).to_string() }),
                    2 => (8..10, if s % 2 == 0 { "00".to_owned() } else { (32 + s % 68).to_string() }),
                    3 => (11..13, (24 + s % 76).to_string()),
                    4 => (14..16, (60 + s % 40).to_string()),
                    5 => (17..19, (60 + s % 40).to_string()),
                    6 => (24..26, (24 + s % 76).to_string()),
                    _ => (27..29, (60 + s % 40).to_string()),
                };
                // `saved-at ` is 9 characters; the line is ASCII.
                let start = 9;
                layout.lines[SAVED_AT].replace_range(start + range.start..start + range.end, &value);
                let column = if range.start == 27 { 10 + 27 } else { 10 };
                (SAVED_AT, column)
            }
        };
        let expected = err_at(i, column, K::OutOfRange);
        rejected_with(layout.text().as_bytes(), expected, &layout.lines[i])?;
    }

    /// Semantically invalid snapshots are InvalidSnapshot at the offending
    /// value or line.
    #[test]
    fn semantically_invalid_snapshot_is_rejected(doc in valid_doc(), edit in semantic()) {
        let mut layout = Layout::of(&serialize(&doc));
        let n = layout.windows.len();
        let a = layout.activation();
        let (i, column) = match &edit {
            Semantic::ActiveOutOfBounds { window, excess } => {
                prop_assume!(n > 0);
                let w = layout.window(*window).unwrap();
                let tabs = w.active - w.header - 1;
                layout.lines[w.active] = format!("active {}", tabs as u64 + u64::from(*excess));
                (w.active, 8)
            }
            Semantic::ActivationUnknown { entry, id } => {
                prop_assume!(doc.snapshot.windows.iter().all(|w| w.id.0 != *id));
                if n == 0 {
                    layout.lines[a] = format!("activation {id}");
                    (a, 12)
                } else {
                    let j = 1 + entry.index(n);
                    let col = tokens(&layout.lines[a])[j].0;
                    layout.lines[a] = replace_token(&layout.lines[a], j, &id.to_string());
                    (a, col)
                }
            }
            Semantic::ActivationRepeated { first, second } => {
                prop_assume!(n >= 2);
                let j = 2 + second.index(n - 1); // token index 2..=n
                let i = 1 + first.index(j - 1); // token index 1..j
                let toks: Vec<String> = layout.lines[a].split(' ').map(str::to_owned).collect();
                let col = tokens(&layout.lines[a])[j].0;
                layout.lines[a] = replace_token(&layout.lines[a], j, &toks[i]);
                (a, col)
            }
            Semantic::ActivationIncomplete { entry } => {
                prop_assume!(n > 0);
                layout.lines[a] = remove_token(&layout.lines[a], 1 + entry.index(n));
                (a, chars(&layout.lines[a]) + 1)
            }
            Semantic::DuplicateWindowId { first, second } => {
                prop_assume!(n >= 2);
                let j = 1 + second.index(n - 1);
                let i = first.index(j);
                let (wi, wj) = (layout.windows[i], layout.windows[j]);
                let id = layout.lines[wi.header].split(' ').nth(1).unwrap().to_owned();
                layout.lines[wj.header] = replace_token(&layout.lines[wj.header], 1, &id);
                (wj.header, 11)
            }
            Semantic::NonPositiveRestored { window, left, top, width, height } => {
                prop_assume!(n > 0);
                let w = layout.window(*window).unwrap();
                let rect = format!("restored={left},{top},{},{}", left + width, top + height);
                let col = value_col(tokens(&layout.lines[w.header])[RESTORED].0, "restored=");
                layout.lines[w.header] = replace_token(&layout.lines[w.header], RESTORED, &rect);
                (w.header, col)
            }
            Semantic::EmptyWindow { window } => {
                prop_assume!(n > 0);
                let w = layout.window(*window).unwrap();
                layout.lines.drain(w.header + 1..w.active);
                (w.header + 1, 1)
            }
        };
        let expected = err_at(i, column, K::InvalidSnapshot);
        rejected_with(layout.text().as_bytes(), expected, &layout.lines[i])?;
    }

    /// Byte-level deviations from the canonical text are syntax errors at
    /// the first deviating character: a BOM, a CR, a doubled space, content
    /// after the trailer, a leading zero, `-0`.
    #[test]
    fn non_canonical_text_is_a_syntax_error(doc in valid_doc(), edit in syntax_edit()) {
        let mut layout = Layout::of(&serialize(&doc));
        let expected = match &edit {
            SyntaxEdit::Bom => {
                layout.lines[HEADER].insert(0, '\u{FEFF}');
                err_at(HEADER, 1, K::Syntax)
            }
            SyntaxEdit::Cr(pick) => {
                // The CR becomes part of the line's last token; where the
                // error lands within that token depends on the token.
                let i = pick.index(layout.lines.len());
                let cr_col = chars(&layout.lines[i]) + 1;
                layout.lines[i].push('\r');
                let e = rejected(layout.text().as_bytes())?;
                prop_assert_eq!((e.line, e.kind), (n32(i + 1), K::Syntax), "{:?}", clip(&layout.lines[i]));
                let last_tok_col = tokens(&layout.lines[i]).last().unwrap().0;
                prop_assert!(
                    (last_tok_col..=cr_col).contains(&e.column),
                    "{e:?} not within the last token {:?}", clip(&layout.lines[i])
                );
                return Ok(());
            }
            SyntaxEdit::DoubledSpace(pick) => {
                let candidates: Vec<(usize, usize, u32)> = layout
                    .lines
                    .iter()
                    .enumerate()
                    .flat_map(|(i, l)| {
                        tokens(l)
                            .into_iter()
                            .enumerate()
                            .skip(1)
                            .filter(|(_, (_, t))| required_token(t))
                            .map(move |(j, (col, _))| (i, j, col))
                    })
                    .collect();
                prop_assume!(!candidates.is_empty());
                let (i, j, col) = candidates[pick.index(candidates.len())];
                let tok = layout.lines[i].split(' ').nth(j).unwrap().to_owned();
                layout.lines[i] = replace_token(&layout.lines[i], j, &format!(" {tok}"));
                // The empty token between the two spaces starts at `col`.
                err_at(i, col, K::Syntax)
            }
            SyntaxEdit::AfterTrailer(extra) => {
                let mut bytes = layout.text();
                bytes.push_str(extra);
                return rejected_with(bytes.as_bytes(), err_at(layout.lines.len(), 1, K::Syntax), extra);
            }
            SyntaxEdit::LeadingZero(pick) => {
                // (line, token, length of `key=` before the number)
                let mut candidates = vec![(HEADER, 1, "version=".len()), (FAILED_ATTEMPTS, 1, 0)];
                for w in &layout.windows {
                    candidates.push((w.header, 1, "id=".len()));
                    candidates.push((w.active, 1, 0));
                }
                let a = layout.activation();
                candidates.extend((1..tokens(&layout.lines[a]).len()).map(|j| (a, j, 0)));
                let (i, j, skip) = candidates[pick.index(candidates.len())];
                let (col, tok) = tokens(&layout.lines[i])[j];
                let bad = format!("{}0{}", &tok[..skip], &tok[skip..]);
                let col = col + n32(skip);
                layout.lines[i] = replace_token(&layout.lines[i], j, &bad);
                err_at(i, col, K::Syntax)
            }
            SyntaxEdit::NegativeZero { window, monitor, comp } => {
                let Some(w) = layout.window(*window) else { return Err(TestCaseError::reject("no window")) };
                let idx = if *monitor { MONITOR } else { RESTORED };
                let (line, col) = edit_rect(&layout.lines[w.header], idx, *comp, "-0");
                layout.lines[w.header] = line;
                err_at(w.header, col, K::Syntax)
            }
        };
        let line = layout.lines[expected.line as usize - 1].clone();
        rejected_with(layout.text().as_bytes(), expected, &line)?;
    }

    /// Random byte edits: rejected with a position within the input, or, if
    /// they happen to produce another valid text, accepted only as a valid
    /// document that re-serializes to exactly that text.
    #[test]
    fn random_byte_edits_are_rejected_or_canonical(
        doc in valid_doc(),
        edits in proptest::collection::vec(byte_edit(), 1..=4),
    ) {
        let mut bytes = serialize(&doc).into_bytes();
        for edit in &edits {
            match *edit {
                ByteEdit::Flip(i, mask) if !bytes.is_empty() => {
                    let i = i.index(bytes.len());
                    bytes[i] ^= mask;
                }
                ByteEdit::Insert(i, b) => {
                    let i = i.index(bytes.len() + 1);
                    bytes.insert(i, b);
                }
                ByteEdit::Delete(i) if !bytes.is_empty() => {
                    let i = i.index(bytes.len());
                    bytes.remove(i);
                }
                ByteEdit::Replace(i, b) if !bytes.is_empty() => {
                    let i = i.index(bytes.len());
                    bytes[i] = b;
                }
                _ => {}
            }
        }
        let _ = parse_checked(&bytes)?;
    }
}
