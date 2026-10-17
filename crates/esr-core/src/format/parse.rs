//! Snapshot_Parser: `parse(&[u8]) -> Result<SessionDoc, ParseError>`, the
//! exact inverse of [`super::serialize()`], with version dispatch and
//! migrations.
//!
//! Requirements 11.4, 11.6, 11.8, 11.9.
//!
//! # Strictness
//!
//! The parser accepts exactly the text the serializer writes for a valid
//! document, so `serialize(&parse(s)?) == s` for every accepted `s` whose
//! declared version is [`FORMAT_VERSION`]. In particular:
//!
//! * the input is UTF-8 without a BOM; every line, including the
//!   `end-session` trailer, ends with LF; nothing follows the trailer;
//! * fields are separated by single spaces and appear in the fixed order of
//!   the grammar, with the fixed `key=` names (`scope-` prefixes for nested
//!   search scopes, see [`super::serialize()`]);
//! * integers are canonical decimal: no sign on unsigned values, no leading
//!   zeros, no `-0`;
//! * values use the canonical escaping of [`super::escape::unescape`] and
//!   PIDLs strict padded base64;
//! * `saved-at` is `YYYY-MM-DDTHH:MM:SS.mmm±HH:MM` with a valid calendar
//!   date, minutes of the offset below 60, and `+00:00` (not `-00:00`) for UTC.
//!
//! # Errors
//!
//! A [`ParseError`] names the 1-based line and column (counted in Unicode
//! scalar values) of the first problem met while reading the file from the
//! start, and its [`ParseErrorKind`]:
//!
//! * [`InvalidUtf8`]: the position of the first
//!   byte that is not part of valid UTF-8. UTF-8 is checked before anything
//!   else.
//! * [`Syntax`]: a line, field, or value that does
//!   not match the grammar (unknown keyword or token, wrong field name,
//!   doubled space, malformed number, escape or base64, a CR, a BOM, content
//!   after the trailer). The column is that of the offending character or
//!   field.
//! * [`MissingElement`]: the input ends, or a
//!   line ends, where a line or field is required; also a line that starts
//!   with a keyword of the format other than the one required at that point
//!   (the required line is missing). End of input is reported at column 1 of
//!   the line after the last one; a missing LF at the column after the last
//!   character of the unterminated line.
//! * [`OutOfRange`]: a well-formed number that
//!   does not fit its field (`failed-attempts` above 3, an id or coordinate
//!   beyond 32 bits, version 0, an impossible date or offset).
//! * [`InvalidSnapshot`]: the snapshot
//!   breaks a validity rule of [`crate::model::validate`]: more than
//!   [`MAX_WINDOWS`] windows or [`MAX_TABS_TOTAL`] tabs (reported on the line
//!   that exceeds the cap), a duplicate window id, a non-positive restored
//!   rect, a string longer than [`MAX_WIDE_LEN`] units, `kind=search` without
//!   a `query=` group or a `query=` group on another kind, a window without
//!   tabs or with an active index out of range, an `activation` entry that is
//!   unknown or repeated, or an `activation` line missing some window (column
//!   after the line's end).
//! * [`UnsupportedVersion`](ParseErrorKind::UnsupportedVersion): the header
//!   declares a version newer than [`FORMAT_VERSION`] (Req 11.9), reported at
//!   the version value; nothing after it is read.
//!
//! On any error no document is returned (Req 11.6). After a successful read,
//! [`crate::model::validate_doc`] re-checks the document as a safety net; if
//! it disagrees, the error is `InvalidSnapshot` at the trailer line.
//!
//! # Versions and migrations
//!
//! The header line `ESR-SESSION version=N` is shared by every version. Its
//! version selects the body parser `parse_vN`, which yields the document in
//! that version's shape (`Versioned`). `migrate` then applies
//! `migrate_vN_to_vN+1` steps until the document is current (Req 11.8).
//! Version 1 is the only and newest version, so the chain is empty today.
//! The returned document's `version` is the version declared in the file; its
//! content is always in the current shape.

use std::collections::BTreeSet;
use std::fmt;

use super::escape::{base64_decode, unescape};
use super::serialize::FORMAT_VERSION;
use super::tokens::{cause_from_token, kind_from_token, show_state_from_token, status_from_token};
use crate::model::{
    validate_doc, DisplayRef, LocationId, LocationKind, Placement, Rect, SearchSpec, SessionDoc,
    Snapshot, TabRec, Timestamp, WideString, WindowId, WindowRec, MAX_FAILED_ATTEMPTS,
    MAX_TABS_PER_WINDOW, MAX_TABS_TOTAL, MAX_WIDE_LEN, MAX_WINDOWS,
};

/// The oldest Session_File_Format version the parser reads.
pub const OLDEST_VERSION: u32 = 1;

/// Why the Session_File could not be parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseErrorKind {
    InvalidUtf8,
    Syntax,
    MissingElement,
    OutOfRange,
    InvalidSnapshot,
    /// The file declares a version newer than the parser supports (Req 11.9).
    UnsupportedVersion { declared: u32, newest: u32 },
}

impl fmt::Display for ParseErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseErrorKind::InvalidUtf8 => f.write_str("invalid UTF-8"),
            ParseErrorKind::Syntax => f.write_str("syntax error"),
            ParseErrorKind::MissingElement => f.write_str("missing element"),
            ParseErrorKind::OutOfRange => f.write_str("value out of range"),
            ParseErrorKind::InvalidSnapshot => f.write_str("invalid snapshot"),
            ParseErrorKind::UnsupportedVersion { declared, newest } => write!(
                f,
                "format version {declared} is newer than the newest supported version {newest}"
            ),
        }
    }
}

/// A parse failure at a 1-based line and column counted in Unicode scalar
/// values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub line: u32,
    pub column: u32,
    pub kind: ParseErrorKind,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}, column {}: {}", self.line, self.column, self.kind)
    }
}

impl std::error::Error for ParseError {}

use ParseErrorKind::{InvalidSnapshot, InvalidUtf8, MissingElement, OutOfRange, Syntax};

/// Every line keyword of the format, in any version.
const KEYWORDS: &[&str] = &[
    "ESR-SESSION",
    "status",
    "failed-attempts",
    "saved-at",
    "cause",
    "window",
    "tab",
    "active",
    "end-window",
    "activation",
    "end-session",
];

/// Parses Session_File content (Req 11.4, 11.6, 11.8, 11.9).
///
/// Returns a valid current-shape document, or the first problem; never a
/// partial document.
pub fn parse(bytes: &[u8]) -> Result<SessionDoc, ParseError> {
    let text = decode_utf8(bytes)?;
    let mut lines = Lines::new(text);
    let version = parse_header(&mut lines)?;
    let doc = migrate(parse_body(version, &mut lines)?);
    if validate_doc(&doc).is_err() {
        return Err(ParseError { line: lines.last_line, column: 1, kind: InvalidSnapshot });
    }
    Ok(doc)
}

// ---------------------------------------------------------------------------
// Version dispatch and migrations
// ---------------------------------------------------------------------------

/// Document shape of Session_File_Format version 1 (the current one).
type DocV1 = SessionDoc;

/// A document in the shape of the version it was read from.
enum Versioned {
    V1(DocV1),
}

/// Parses the body after the header with the parser of `version`.
fn parse_body(version: u32, lines: &mut Lines<'_>) -> Result<Versioned, ParseError> {
    match version {
        1 => parse_v1(lines, version).map(Versioned::V1),
        // `parse_header` admits only OLDEST_VERSION..=FORMAT_VERSION.
        declared => Err(ParseError {
            line: 1,
            column: 1,
            kind: ParseErrorKind::UnsupportedVersion { declared, newest: FORMAT_VERSION },
        }),
    }
}

/// Upgrades `doc` one version at a time until it has the current shape.
///
/// Each step preserves status, windows, tabs, Location_Identifiers, active
/// tabs, and activation order (Req 11.8). When version 2 is added, the `V1`
/// arm becomes `Versioned::V1(d) => migrate(Versioned::V2(migrate_v1_to_v2(d)))`
/// and a `V2` arm returns the document.
fn migrate(doc: Versioned) -> SessionDoc {
    match doc {
        Versioned::V1(doc) => doc,
    }
}

/// Reads the version-independent header `ESR-SESSION version=N`.
fn parse_header(lines: &mut Lines<'_>) -> Result<u32, ParseError> {
    let mut line = lines.expect_keyword("ESR-SESSION")?;
    let tok = line.field("version")?;
    let version: u32 = line.uint(tok)?;
    if version < OLDEST_VERSION {
        return Err(line.err(tok.col, OutOfRange));
    }
    if version > FORMAT_VERSION {
        return Err(line.err(
            tok.col,
            ParseErrorKind::UnsupportedVersion { declared: version, newest: FORMAT_VERSION },
        ));
    }
    line.finish()?;
    Ok(version)
}

// ---------------------------------------------------------------------------
// Version 1
// ---------------------------------------------------------------------------

/// Window bookkeeping while reading a version 1 snapshot.
#[derive(Default)]
struct WindowsSoFar {
    windows: Vec<WindowRec>,
    ids: BTreeSet<WindowId>,
    tabs: usize,
}

fn parse_v1(lines: &mut Lines<'_>, version: u32) -> Result<DocV1, ParseError> {
    let mut line = lines.expect_keyword("status")?;
    let tok = line.arg()?;
    let status = status_from_token(tok.text).ok_or_else(|| line.err(tok.col, Syntax))?;
    line.finish()?;

    let mut line = lines.expect_keyword("failed-attempts")?;
    let tok = line.arg()?;
    let failed_attempts: u8 = line.uint(tok)?;
    if failed_attempts > MAX_FAILED_ATTEMPTS {
        return Err(line.err(tok.col, OutOfRange));
    }
    line.finish()?;

    let mut line = lines.expect_keyword("saved-at")?;
    let tok = line.arg()?;
    let saved_at = line.timestamp(tok)?;
    line.finish()?;

    let mut line = lines.expect_keyword("cause")?;
    let tok = line.arg()?;
    let cause = cause_from_token(tok.text).ok_or_else(|| line.err(tok.col, Syntax))?;
    line.finish()?;

    let mut so_far = WindowsSoFar::default();
    let mut line = lines.expect()?;
    while line.keyword() == "window" {
        let window = parse_window(lines, line, &so_far)?;
        so_far.ids.insert(window.id);
        so_far.tabs += window.tabs.len();
        so_far.windows.push(window);
        line = lines.expect()?;
    }

    if line.keyword() != "activation" {
        return Err(line.unexpected());
    }
    let mut activation = Vec::with_capacity(so_far.windows.len());
    let mut seen = BTreeSet::new();
    while let Some(tok) = line.next_tok() {
        let id = WindowId(line.uint(tok)?);
        if !so_far.ids.contains(&id) || !seen.insert(id) {
            return Err(line.err(tok.col, InvalidSnapshot));
        }
        activation.push(id);
    }
    if activation.len() != so_far.windows.len() {
        return Err(line.err(line.end_col, InvalidSnapshot));
    }

    lines.expect_keyword("end-session")?.finish()?;
    if let Some(extra) = lines.next()? {
        return Err(extra.err(1, Syntax));
    }

    Ok(SessionDoc {
        version,
        status,
        failed_attempts,
        saved_at,
        cause,
        snapshot: Snapshot { windows: so_far.windows, activation },
    })
}

/// Reads a `window` block whose first line is `line`, through `end-window`.
fn parse_window<'a>(
    lines: &mut Lines<'a>,
    mut line: Line<'a>,
    so_far: &WindowsSoFar,
) -> Result<WindowRec, ParseError> {
    if so_far.windows.len() >= MAX_WINDOWS {
        return Err(line.err(1, InvalidSnapshot));
    }
    let tok = line.field("id")?;
    let id = WindowId(line.uint(tok)?);
    if so_far.ids.contains(&id) {
        return Err(line.err(tok.col, InvalidSnapshot));
    }
    let tok = line.field("state")?;
    let state = show_state_from_token(tok.text).ok_or_else(|| line.err(tok.col, Syntax))?;
    let tok = line.field("restored")?;
    let restored = line.rect(tok)?;
    if !restored.is_positive() {
        return Err(line.err(tok.col, InvalidSnapshot));
    }
    let tok = line.field("display")?;
    let device = line.wide(tok)?;
    let device = String::from_utf16(device.units()).map_err(|_| {
        // Canonical escaping writes `%u` only for unpaired surrogates.
        let offset = tok.text.find("%u").map_or(0, |b| tok.text[..b].chars().count());
        line.err(at(tok.col, offset), Syntax)
    })?;
    let tok = line.field("monitor")?;
    let monitor = line.rect(tok)?;
    line.finish()?;

    let mut tabs = Vec::new();
    let active = loop {
        let mut line = lines.expect()?;
        match line.keyword() {
            "tab" => {
                if so_far.tabs + tabs.len() >= MAX_TABS_TOTAL || tabs.len() >= MAX_TABS_PER_WINDOW {
                    return Err(line.err(1, InvalidSnapshot));
                }
                let location = parse_location(&mut line)?;
                line.finish()?;
                tabs.push(TabRec { location });
            }
            "active" => {
                let tok = line.arg()?;
                let active: usize = line.uint(tok)?;
                if tabs.is_empty() {
                    return Err(line.err(1, InvalidSnapshot));
                }
                if active >= tabs.len() {
                    return Err(line.err(tok.col, InvalidSnapshot));
                }
                line.finish()?;
                break active;
            }
            _ => return Err(line.unexpected()),
        }
    };
    lines.expect_keyword("end-window")?.finish()?;

    Ok(WindowRec {
        id,
        tabs,
        active,
        placement: Placement { state, restored, display: DisplayRef { device, monitor } },
    })
}

/// Reads the location fields of a `tab` line, including nested search
/// scopes (each level's keys prefixed by one more `scope-`). Iterative, so
/// deep nesting cannot exhaust the stack while parsing.
fn parse_location(line: &mut Line<'_>) -> Result<LocationId, ParseError> {
    // Search levels from the outermost inwards: the location and its query.
    let mut searches: Vec<(LocationId, WideString)> = Vec::new();
    let mut prefix = String::new();
    let innermost = loop {
        let kind_tok = line.field(&format!("{prefix}kind"))?;
        let kind = kind_from_token(kind_tok.text).ok_or_else(|| line.err(kind_tok.col, Syntax))?;
        let tok = line.field(&format!("{prefix}parse"))?;
        let parsing_name = line.wide(tok)?;
        let tok = line.field(&format!("{prefix}display"))?;
        let display_name = line.wide(tok)?;
        let tok = line.field(&format!("{prefix}pidl"))?;
        let pidl = base64_decode(tok.text).map_err(|e| line.err(at(tok.col, e.offset), Syntax))?;
        let location = LocationId { case_sensitive: false, kind, parsing_name, display_name, pidl, search: None };

        match line.opt_field(&format!("{prefix}query")) {
            Some(tok) => {
                if kind != LocationKind::Search {
                    return Err(line.err(tok.col, InvalidSnapshot));
                }
                let query = line.wide(tok)?;
                searches.push((location, query));
                prefix.push_str("scope-");
            }
            None => {
                if kind == LocationKind::Search {
                    return Err(line.err(kind_tok.col, InvalidSnapshot));
                }
                break location;
            }
        }
    };
    let location = searches.into_iter().rev().fold(innermost, |scope, (mut loc, query)| {
        loc.search = Some(SearchSpec { query, scope: Box::new(scope) });
        loc
    });
    Ok(location)
}

// ---------------------------------------------------------------------------
// Lines and tokens
// ---------------------------------------------------------------------------

fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// `col` advanced by `offset` scalar values.
fn at(col: u32, offset: usize) -> u32 {
    col.saturating_add(to_u32(offset))
}

fn decode_utf8(bytes: &[u8]) -> Result<&str, ParseError> {
    std::str::from_utf8(bytes).map_err(|e| {
        // The prefix up to `valid_up_to` is valid UTF-8 by definition.
        let valid = std::str::from_utf8(&bytes[..e.valid_up_to()]).unwrap_or("");
        let line = 1 + valid.matches('\n').count();
        let last = valid.rfind('\n').map_or(valid, |i| &valid[i + 1..]);
        ParseError { line: to_u32(line), column: at(1, last.chars().count()), kind: InvalidUtf8 }
    })
}

/// The input split into LF-terminated lines.
struct Lines<'a> {
    rest: &'a str,
    /// Number of the most recently returned line (0 before the first).
    last_line: u32,
    /// Position of the missing LF of the last line, if it has none.
    unterminated: Option<(u32, u32)>,
}

impl<'a> Lines<'a> {
    fn new(text: &'a str) -> Self {
        Self { rest: text, last_line: 0, unterminated: None }
    }

    /// The next line, or `None` at the end of the input. An unterminated
    /// last line is returned once; asking for more reports its missing LF.
    fn next(&mut self) -> Result<Option<Line<'a>>, ParseError> {
        if let Some((line, column)) = self.unterminated.take() {
            return Err(ParseError { line, column, kind: MissingElement });
        }
        if self.rest.is_empty() {
            return Ok(None);
        }
        let (text, rest, terminated) = match self.rest.find('\n') {
            Some(i) => (&self.rest[..i], &self.rest[i + 1..], true),
            None => (self.rest, "", false),
        };
        self.rest = rest;
        self.last_line = self.last_line.saturating_add(1);
        let line = Line::new(self.last_line, text);
        if !terminated {
            self.unterminated = Some((line.no, line.end_col));
        }
        Ok(Some(line))
    }

    /// The next line; the end of the input is a missing element.
    fn expect(&mut self) -> Result<Line<'a>, ParseError> {
        let after = self.last_line.saturating_add(1);
        self.next()?.ok_or(ParseError { line: after, column: 1, kind: MissingElement })
    }

    /// The next line, which must start with `keyword`.
    fn expect_keyword(&mut self, keyword: &str) -> Result<Line<'a>, ParseError> {
        let line = self.expect()?;
        if line.keyword() == keyword {
            Ok(line)
        } else {
            Err(line.unexpected())
        }
    }
}

/// A piece of a line and the column of its first character.
#[derive(Clone, Copy, Debug)]
struct Tok<'a> {
    text: &'a str,
    col: u32,
}

/// One line split on single spaces; the first token is the keyword.
struct Line<'a> {
    no: u32,
    tokens: Vec<Tok<'a>>,
    /// Index of the next unread token.
    next: usize,
    /// Column just after the last character.
    end_col: u32,
}

impl<'a> Line<'a> {
    fn new(no: u32, text: &'a str) -> Self {
        let mut tokens = Vec::new();
        let mut col = 1u32;
        for piece in text.split(' ') {
            tokens.push(Tok { text: piece, col });
            col = at(col, piece.chars().count() + 1);
        }
        // `split` yields at least one piece, so `col` is one past the space
        // that would follow the last piece.
        Self { no, tokens, next: 1, end_col: col.saturating_sub(1) }
    }

    fn keyword(&self) -> &'a str {
        self.tokens[0].text
    }

    fn err(&self, column: u32, kind: ParseErrorKind) -> ParseError {
        ParseError { line: self.no, column, kind }
    }

    /// Error for a line whose keyword is not the one required here: a known
    /// keyword means the required line is missing.
    fn unexpected(&self) -> ParseError {
        let kind = if KEYWORDS.contains(&self.keyword()) { MissingElement } else { Syntax };
        self.err(1, kind)
    }

    fn next_tok(&mut self) -> Option<Tok<'a>> {
        let tok = self.tokens.get(self.next).copied();
        if tok.is_some() {
            self.next += 1;
        }
        tok
    }

    /// The next token as a plain argument (`status pending`).
    fn arg(&mut self) -> Result<Tok<'a>, ParseError> {
        self.next_tok().ok_or(self.err(self.end_col, MissingElement))
    }

    /// The value of the next token, which must be `key=value`.
    fn field(&mut self, key: &str) -> Result<Tok<'a>, ParseError> {
        let tok = self.arg()?;
        split_field(tok, key).ok_or(self.err(tok.col, Syntax))
    }

    /// The value of the next token if it is `key=value`; otherwise reads
    /// nothing.
    fn opt_field(&mut self, key: &str) -> Option<Tok<'a>> {
        let value = split_field(*self.tokens.get(self.next)?, key)?;
        self.next += 1;
        Some(value)
    }

    /// Every token has been read. Extra content is reported at the space
    /// that introduces it, its first unexpected character.
    fn finish(&self) -> Result<(), ParseError> {
        match self.tokens.get(self.next) {
            Some(tok) => Err(self.err(tok.col.saturating_sub(1), Syntax)),
            None => Ok(()),
        }
    }

    fn uint<T: TryFrom<u64>>(&self, tok: Tok<'_>) -> Result<T, ParseError> {
        let value = digits(tok.text).map_err(|e| self.num_err(tok.col, e))?;
        T::try_from(value).map_err(|_| self.err(tok.col, OutOfRange))
    }

    fn int(&self, tok: Tok<'_>) -> Result<i32, ParseError> {
        let (negative, body, skip) = match tok.text.strip_prefix('-') {
            Some(body) => (true, body, 1),
            None => (false, tok.text, 0),
        };
        let magnitude = digits(body).map_err(|e| self.num_err(at(tok.col, skip), e))?;
        if negative && magnitude == 0 {
            return Err(self.err(tok.col, Syntax));
        }
        let value = if negative { -i128::from(magnitude) } else { i128::from(magnitude) };
        i32::try_from(value).map_err(|_| self.err(tok.col, OutOfRange))
    }

    fn num_err(&self, col: u32, e: NumError) -> ParseError {
        match e {
            NumError::Syntax(offset) => self.err(at(col, offset), Syntax),
            NumError::Overflow => self.err(col, OutOfRange),
        }
    }

    /// `left,top,right,bottom`.
    fn rect(&self, tok: Tok<'_>) -> Result<Rect, ParseError> {
        let mut parts = [0i32; 4];
        let mut col = tok.col;
        let mut pieces = tok.text.split(',');
        for (i, part) in parts.iter_mut().enumerate() {
            let Some(piece) = pieces.next() else {
                // Fewer than four numbers: the value ends too early.
                return Err(self.err(col.saturating_sub(1), Syntax));
            };
            *part = self.int(Tok { text: piece, col })?;
            col = at(col, piece.chars().count() + 1);
            if i == 3 && pieces.next().is_some() {
                // A fifth number: report the comma that starts it.
                return Err(self.err(col.saturating_sub(1), Syntax));
            }
        }
        let [left, top, right, bottom] = parts;
        Ok(Rect { left, top, right, bottom })
    }

    /// An escaped `WideString` of at most [`MAX_WIDE_LEN`] units.
    fn wide(&self, tok: Tok<'_>) -> Result<WideString, ParseError> {
        let value = unescape(tok.text).map_err(|e| self.err(at(tok.col, e.offset), Syntax))?;
        if value.len() > MAX_WIDE_LEN {
            return Err(self.err(tok.col, InvalidSnapshot));
        }
        Ok(value)
    }

    /// `YYYY-MM-DDTHH:MM:SS.mmm±HH:MM`.
    fn timestamp(&self, tok: Tok<'_>) -> Result<Timestamp, ParseError> {
        const PATTERN: &[u8] = b"DDDD-DD-DDTDD:DD:DD.DDDSDD:DD";
        let chars: Vec<char> = tok.text.chars().collect();
        for (i, &p) in PATTERN.iter().enumerate() {
            let ok = chars.get(i).is_some_and(|&c| match p {
                b'D' => c.is_ascii_digit(),
                b'S' => c == '+' || c == '-',
                _ => c == char::from(p),
            });
            if !ok {
                return Err(self.err(at(tok.col, i), Syntax));
            }
        }
        if chars.len() > PATTERN.len() {
            return Err(self.err(at(tok.col, PATTERN.len()), Syntax));
        }
        let num = |from: usize, to: usize| {
            chars[from..to].iter().fold(0u16, |v, c| v * 10 + c.to_digit(10).unwrap_or(0) as u16)
        };
        let (off_hours, off_minutes) = (num(24, 26), num(27, 29));
        let negative = chars[23] == '-';
        if negative && off_hours == 0 && off_minutes == 0 {
            return Err(self.err(at(tok.col, 23), Syntax));
        }
        if off_minutes >= 60 {
            return Err(self.err(at(tok.col, 27), OutOfRange));
        }
        // At most 99 * 60 + 59, which fits an i16.
        let offset = (off_hours * 60 + off_minutes) as i16;
        // Every field below fits its type: at most 4, 2, or 3 digits.
        let t = Timestamp {
            year: num(0, 4),
            month: num(5, 7) as u8,
            day: num(8, 10) as u8,
            hour: num(11, 13) as u8,
            minute: num(14, 16) as u8,
            second: num(17, 19) as u8,
            millis: num(20, 23),
            offset_minutes: if negative { -offset } else { offset },
        };
        if !t.is_valid() {
            return Err(self.err(tok.col, OutOfRange));
        }
        Ok(t)
    }
}

/// The value of `tok` if it is `key=value`, with its column.
fn split_field<'a>(tok: Tok<'a>, key: &str) -> Option<Tok<'a>> {
    let value = tok.text.strip_prefix(key)?.strip_prefix('=')?;
    // Keys are ASCII, so their byte length is their length in scalar values.
    Some(Tok { text: value, col: at(tok.col, key.len() + 1) })
}

enum NumError {
    /// 0-based offset of the offending character.
    Syntax(usize),
    Overflow,
}

/// Canonical unsigned decimal: `0` or a non-zero digit followed by digits.
fn digits(text: &str) -> Result<u64, NumError> {
    if text.is_empty() {
        return Err(NumError::Syntax(0));
    }
    if let Some(i) = text.chars().position(|c| !c.is_ascii_digit()) {
        return Err(NumError::Syntax(i));
    }
    if text.len() > 1 && text.starts_with('0') {
        return Err(NumError::Syntax(0));
    }
    text.bytes().try_fold(0u64, |v, b| {
        v.checked_mul(10).and_then(|v| v.checked_add(u64::from(b - b'0'))).ok_or(NumError::Overflow)
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::serialize::serialize;
    use crate::model::*;

    fn loc(kind: LocationKind, parse: &str, display: &str, pidl: &[u8]) -> LocationId {
        LocationId {
            case_sensitive: false,
            kind,
            parsing_name: parse.into(),
            display_name: display.into(),
            pidl: pidl.to_vec(),
            search: None,
        }
    }

    fn search(query: &str, scope: LocationId) -> LocationId {
        let mut l = loc(LocationKind::Search, "search-ms:x", "Search Results", &[]);
        l.search = Some(SearchSpec { query: query.into(), scope: Box::new(scope) });
        l
    }

    fn window(id: u32, tabs: Vec<LocationId>, active: usize) -> WindowRec {
        WindowRec {
            id: WindowId(id),
            tabs: tabs.into_iter().map(|location| TabRec { location }).collect(),
            active,
            placement: Placement {
                state: ShowState::Minimized { restore_maximized: true },
                restored: Rect { left: -100, top: 20, right: 700, bottom: 520 },
                display: DisplayRef {
                    device: r"\\.\DISPLAY2 ü".into(),
                    monitor: Rect { left: -1920, top: 0, right: 0, bottom: 1080 },
                },
            },
        }
    }

    fn sample() -> SessionDoc {
        let fs = loc(LocationKind::FileSystem(Volume::Fixed), r"C:\Program Files", "Prog=ram %", &[1, 2, 3]);
        let unc = loc(LocationKind::FileSystem(Volume::Unc), r"\\server\share", "share", &[0xFF]);
        let mut lone = loc(LocationKind::ZipFolder, "", "", &[]);
        lone.parsing_name = WideString(vec![u16::from(b'a'), 0xD800, u16::from(b'\n')]);
        let nested = search("outer", search("inner", lone.clone()));
        SessionDoc {
            version: FORMAT_VERSION,
            status: Status::Pending,
            failed_attempts: 3,
            saved_at: Timestamp {
                year: 2025,
                month: 1,
                day: 31,
                hour: 9,
                minute: 15,
                second: 2,
                millis: 123,
                offset_minutes: -570,
            },
            cause: SaveCause::Shutdown,
            snapshot: Snapshot {
                windows: vec![
                    window(7, vec![fs, search("a=b", unc)], 1),
                    window(0, vec![nested, lone], 0),
                ],
                activation: vec![WindowId(0), WindowId(7)],
            },
        }
    }

    fn assert_identical(a: &SessionDoc, b: &SessionDoc) {
        assert_eq!(a, b);
        for (wa, wb) in a.snapshot.windows.iter().zip(&b.snapshot.windows) {
            for (ta, tb) in wa.tabs.iter().zip(&wb.tabs) {
                assert!(ta.location.identical(&tb.location), "{:?} vs {:?}", ta, tb);
            }
        }
    }

    fn err(text: &str) -> ParseError {
        parse(text.as_bytes()).expect_err(text)
    }

    fn e(line: u32, column: u32, kind: ParseErrorKind) -> ParseError {
        ParseError { line, column, kind }
    }

    /// `sample()` serialized, with `from` replaced by `to` once.
    fn edited(from: &str, to: &str) -> String {
        let text = serialize(&sample());
        assert!(text.contains(from), "{from:?} not in {text}");
        text.replacen(from, to, 1)
    }

    #[test]
    fn round_trip_sample_and_empty() {
        let doc = sample();
        let text = serialize(&doc);
        let parsed = parse(text.as_bytes()).unwrap();
        assert_identical(&parsed, &doc);
        assert_eq!(serialize(&parsed), text);

        let empty = SessionDoc {
            status: Status::Consumed,
            failed_attempts: 0,
            cause: SaveCause::Checkpoint,
            snapshot: Snapshot::default(),
            ..doc
        };
        let text = serialize(&empty);
        assert_eq!(parse(text.as_bytes()), Ok(empty));
    }

    #[test]
    fn newer_version_is_unsupported() {
        let kind = ParseErrorKind::UnsupportedVersion { declared: 2, newest: FORMAT_VERSION };
        // Nothing after the version is read.
        assert_eq!(err("ESR-SESSION version=2 future=x\ngarbage"), e(1, 21, kind));
        assert_eq!(err("ESR-SESSION version=0\n"), e(1, 21, OutOfRange));
        assert_eq!(err("ESR-SESSION version=4294967296\n"), e(1, 21, OutOfRange));
        assert_eq!(
            e(1, 21, kind).to_string(),
            "line 1, column 21: format version 2 is newer than the newest supported version 1"
        );
    }

    #[test]
    fn header_problems() {
        assert_eq!(err(""), e(1, 1, MissingElement));
        assert_eq!(err("\u{FEFF}ESR-SESSION version=1\n"), e(1, 1, Syntax));
        assert_eq!(err("status pending\n"), e(1, 1, MissingElement));
        assert_eq!(err("ESR-SESSION version=01\n"), e(1, 21, Syntax));
        assert_eq!(err("ESR-SESSION version=1"), e(1, 22, MissingElement));
        assert_eq!(err("ESR-SESSION version=1\n"), e(2, 1, MissingElement));
        assert_eq!(err("ESR-SESSION version=1 \n"), e(1, 22, Syntax));
    }

    #[test]
    fn invalid_utf8_position() {
        let mut bytes = b"ESR-SESSION version=1\nstatus p\xC3\xA9".to_vec();
        bytes.push(0xFF);
        assert_eq!(parse(&bytes), Err(e(2, 10, InvalidUtf8)));
    }

    #[test]
    fn trailer_and_line_endings() {
        let text = serialize(&sample());
        let lines = to_u32(text.lines().count());
        assert_eq!(err(text.strip_suffix('\n').unwrap()), e(lines, 12, MissingElement));
        assert_eq!(err(text.strip_suffix("end-session\n").unwrap()), e(lines, 1, MissingElement));
        assert_eq!(err(&format!("{text}\n")), e(lines + 1, 1, Syntax));
        assert_eq!(err(&text.replacen('\n', "\r\n", 1)), e(1, 22, Syntax));
    }

    #[test]
    fn field_level_errors() {
        assert_eq!(err(&edited("failed-attempts 3", "failed-attempts 4")), e(3, 17, OutOfRange));
        assert_eq!(err(&edited("status pending", "status  pending")), e(2, 8, Syntax));
        assert_eq!(err(&edited("cause shutdown", "cause reboot")), e(5, 7, Syntax));
        assert_eq!(err(&edited("cause shutdown\n", "")), e(5, 1, MissingElement));
        // Offset minutes, impossible dates, and `-00:00`.
        assert_eq!(err(&edited("-09:30", "+09:60")), e(4, 37, OutOfRange));
        assert_eq!(err(&edited("2025-01-31", "2025-02-30")), e(4, 10, OutOfRange));
        assert_eq!(err(&edited("123-09:30", "123-00:00")), e(4, 33, Syntax));
        assert_eq!(err(&edited("123-09:30", "123-09:3")), e(4, 38, Syntax));
        // Window line: `window id=7 state=minimized-max restored=-100,...`.
        assert_eq!(err(&edited("restored=-100,", "restored=-0,")), e(6, 42, Syntax));
        assert_eq!(err(&edited("restored=-100,20,700,520", "restored=-100,20,700")), e(6, 53, Syntax));
        assert_eq!(err(&edited("restored=-100,20,700,520", "restored=-100,20,700,520,1")), e(6, 57, Syntax));
        assert_eq!(err(&edited("restored=-100", "restored=-2147483649")), e(6, 42, OutOfRange));
        assert_eq!(err(&edited(" state=", " stat=")), e(6, 13, Syntax));
        // Display device with a lone surrogate cannot be a `String`.
        assert_eq!(err(&edited("display=\\\\.\\DISPLAY2", "display=x%uD800")), e(6, 67, Syntax));
        // Tab line `tab kind=fs-fixed parse=C:\Program%20Files display=Prog%3Dram%20%25 pidl=AQID`.
        assert_eq!(err(&edited("kind=fs-fixed", "kind=fs-ssd")), e(7, 10, Syntax));
        assert_eq!(err(&edited("pidl=AQID", "pidl=AQI")), e(7, 77, Syntax));
        assert_eq!(err(&edited("pidl=AQID", "pidl=AQID x=y")), e(7, 78, Syntax));
        assert_eq!(err(&edited("Prog%3Dram", "Prog=ram")), e(7, 56, Syntax));
    }

    #[test]
    fn snapshot_validity_errors() {
        // Active index out of range (window 7 has 2 tabs).
        assert_eq!(err(&edited("active 1", "active 2")), e(9, 8, InvalidSnapshot));
        // Duplicate window id.
        assert_eq!(err(&edited("window id=0", "window id=7")), e(11, 11, InvalidSnapshot));
        // Activation: repeated, unknown, and incomplete.
        assert_eq!(err(&edited("activation 0 7", "activation 0 0")), e(16, 14, InvalidSnapshot));
        assert_eq!(err(&edited("activation 0 7", "activation 0 9")), e(16, 14, InvalidSnapshot));
        assert_eq!(err(&edited("activation 0 7", "activation 0")), e(16, 13, InvalidSnapshot));
        // Non-positive restored rect.
        assert_eq!(err(&edited("restored=-100,20,700,520", "restored=0,0,0,1")), e(6, 42, InvalidSnapshot));
        // Search group mismatch: query on a plain kind, search without query.
        assert_eq!(
            err(&edited("pidl=AQID\n", "pidl=AQID query=q scope-kind=zip scope-parse= scope-display= scope-pidl=\n")),
            e(7, 85, InvalidSnapshot)
        );
        let text = edited("kind=fs-fixed", "kind=search");
        assert_eq!(err(&text), e(7, 10, InvalidSnapshot));
        // A window without tabs.
        let empty = "ESR-SESSION version=1\nstatus pending\nfailed-attempts 0\n\
                     saved-at 2025-01-31T09:15:02.123+00:00\ncause checkpoint\n\
                     window id=1 state=normal restored=0,0,1,1 display= monitor=0,0,1,1\n\
                     active 0\nend-window\nactivation 1\nend-session\n";
        assert_eq!(err(empty), e(7, 1, InvalidSnapshot));
    }

    #[test]
    fn caps_are_enforced_at_the_exceeding_line() {
        let w = |id: u32, tabs: usize| window(id, vec![loc(LocationKind::ZipFolder, "z", "z", &[]); tabs], 0);
        let mut doc = sample();
        doc.snapshot.windows = (0..MAX_WINDOWS as u32).map(|i| w(i, 1)).collect();
        doc.snapshot.activation = (0..MAX_WINDOWS as u32).map(WindowId).collect();
        let text = serialize(&doc);
        assert!(parse(text.as_bytes()).is_ok());
        doc.snapshot.windows.push(w(1000, 1));
        doc.snapshot.activation.push(WindowId(1000));
        // 5 header lines, then 4 lines per window.
        let line = to_u32(5 + 4 * MAX_WINDOWS + 1);
        assert_eq!(parse(serialize(&doc).as_bytes()), Err(e(line, 1, InvalidSnapshot)));

        doc.snapshot.windows = vec![w(0, 300), w(1, 201)];
        doc.snapshot.activation = vec![WindowId(0), WindowId(1)];
        // Window 1's line is 5 + 300 + 3 + 1; its 201st tab is 201 lines later.
        let line = to_u32(5 + 300 + 3 + 1 + 201);
        assert_eq!(parse(serialize(&doc).as_bytes()), Err(e(line, 1, InvalidSnapshot)));

        let mut long = sample();
        long.snapshot.windows[0].tabs[0].location.display_name = WideString(vec![0x41; MAX_WIDE_LEN + 1]);
        let text = serialize(&long);
        assert_eq!(parse(text.as_bytes()), Err(e(7, 52, InvalidSnapshot)));
    }

    #[test]
    fn digits_are_canonical() {
        assert!(matches!(digits("0"), Ok(0)));
        assert!(matches!(digits("18446744073709551615"), Ok(u64::MAX)));
        assert!(matches!(digits("18446744073709551616"), Err(NumError::Overflow)));
        assert!(matches!(digits("00"), Err(NumError::Syntax(0))));
        assert!(matches!(digits("+1"), Err(NumError::Syntax(0))));
        assert!(matches!(digits("1٣"), Err(NumError::Syntax(1))));
        assert!(matches!(digits(""), Err(NumError::Syntax(0))));
    }
}
