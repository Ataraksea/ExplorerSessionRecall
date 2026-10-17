//! `search-ms:` parsing and building (`parse_search_ms`, `build_search_ms`).
//!
//! Requirements 8.5, 8.6 (design.md, "Parsing-name rules").
//!
//! A search tab's `SHGDN_FORPARSING` name is a `search-ms:` URL such as
//!
//! ```text
//! search-ms:displayname=Search%20Results%20in%20Downloads&crumb=System.Generic.String%3Afoo&crumb=location:C%3A%5CUsers%5Cme%5CDownloads
//! ```
//!
//! This module only deals in strings: [`parse_search_ms`] returns the query text
//! and the scope's parsing name. Turning the scope into a full `LocationId`
//! (kind detection, PIDL) is the Windows adapter's job.
//!
//! # Parsing rules
//!
//! - The scheme `search-ms:` is matched ASCII case-insensitively. Parameters are
//!   separated by a raw `&`; the key ends at the first raw `=`. Keys (`crumb`,
//!   `query`) and the crumb columns (`System.Generic.String:`, `location:`) are
//!   matched ASCII case-insensitively. Other parameters (`displayname`, …) and
//!   other crumbs (e.g. `kind:`) are ignored: only the query text and the scope
//!   are part of the Location_Identifier (Req 8.5).
//! - Values are percent-decoded as UTF-8. Runs of consecutive `%XX` escapes must
//!   form valid UTF-8; `%` not followed by two hex digits is an error. Characters
//!   that Explorer leaves raw (including non-ASCII) are kept verbatim. `+` is a
//!   literal plus, not a space.
//! - Query: the value after `crumb=System.Generic.String:` (Explorer encodes the
//!   colon as `%3A`, so the column is matched after decoding the whole crumb) or
//!   the value of `query=`. Raw commas in the query are kept literally. An empty
//!   query is allowed. Several sources must agree, otherwise the URL is rejected
//!   with [`SearchParseError::ConflictingQueries`].
//! - Scope: the value after `crumb=location:`. Per the CRUMB syntax
//!   (`location:<value>[,<label>][,include|exclude][,recursive|nonrecursive]`) a
//!   raw comma ends the value; the label is ignored. `exclude`, `nonrecursive`,
//!   or any further argument cannot be represented by a single scope and is
//!   rejected with [`SearchParseError::UnsupportedScopeOption`].
//! - Multiple scopes: Explorer may emit several `crumb=location:` parameters
//!   (multi-folder scopes). Identical duplicates are collapsed; distinct scopes are
//!   rejected with [`SearchParseError::MultipleScopes`] rather than keeping the
//!   first, because restoring a narrower search would not be the "identical search
//!   scope" of Req 8.6. The adapter treats any parse error as an unidentifiable
//!   Location (Req 8.8): the tab keeps its last recorded Location_Identifier and
//!   `UNIDENTIFIABLE-LOCATION` is logged, so restore never sees such a search.
//! - A missing (or empty) scope is [`SearchParseError::MissingScope`]; a missing
//!   query is [`SearchParseError::MissingQuery`].
//! - The input must be well-formed UTF-16, so every parsed query and scope is
//!   valid Unicode and can always be rebuilt.
//!
//! # Building
//!
//! [`build_search_ms`] emits Explorer's own form,
//! `search-ms:crumb=System.Generic.String%3A<query>&crumb=location:<scope>`.
//! Every character except the RFC 3986 unreserved set (`A–Z a–z 0–9 - . _ ~`) is
//! percent-encoded as UTF-8 with upper-case hex, so `&`, `=`, `%`, `:`, `,`,
//! spaces and all non-ASCII text survive. No `displayname` is emitted (it would
//! need a localized string); Explorer then shows its default title.
//!
//! An unpaired surrogate cannot be encoded as UTF-8, so building fails with
//! [`SearchBuildError::UnpairedSurrogate`]. Such values cannot come from
//! [`parse_search_ms`]; they could only come from a hand-edited Session_File.
//! An empty scope is [`SearchBuildError::EmptyScope`].
//!
//! For every valid query `q` and non-empty valid scope `s`,
//! `parse_search_ms(&build_search_ms(&q, &s)?) == Ok(ParsedSearch { query: q, scope: s })`
//! (Property 25).

use std::fmt;

use crate::model::WideString;

/// The URL scheme of search-results locations (matched case-insensitively).
pub const SCHEME: &str = "search-ms:";

const QUERY_COLUMN: &str = "System.Generic.String:";
const LOCATION_COLUMN: &str = "location:";

/// Query text and scope parsing name extracted from a `search-ms:` URL.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ParsedSearch {
    /// Exactly as entered (Req 8.5).
    pub query: WideString,
    /// `SHGDN_FORPARSING` name of the search scope, e.g. `C:\Users\me\Downloads`
    /// or `::{20D04FE0-3AEA-1069-A2D8-08002B30309D}`.
    pub scope: WideString,
}

/// Why a parsing name is not a usable `search-ms:` location. Indices are
/// positions in UTF-16 code units within the whole parsing name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchParseError {
    /// The string does not start with `search-ms:`.
    NotSearchMs,
    /// Unpaired surrogate in the input.
    UnpairedSurrogate { index: usize },
    /// `%` not followed by two hex digits.
    InvalidEscape { index: usize },
    /// A run of `%XX` escapes starting here is not valid UTF-8.
    InvalidUtf8 { index: usize },
    /// No `crumb=System.Generic.String:` and no `query=`.
    MissingQuery,
    /// Query sources with different values.
    ConflictingQueries,
    /// No non-empty `crumb=location:`.
    MissingScope,
    /// More than one distinct `crumb=location:` scope.
    MultipleScopes,
    /// A location crumb with `exclude`, `nonrecursive`, or extra arguments.
    UnsupportedScopeOption,
}

impl fmt::Display for SearchParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use SearchParseError::*;
        match self {
            NotSearchMs => write!(f, "not a search-ms: URL"),
            UnpairedSurrogate { index } => write!(f, "unpaired surrogate at unit {index}"),
            InvalidEscape { index } => write!(f, "invalid percent escape at unit {index}"),
            InvalidUtf8 { index } => write!(f, "percent escapes at unit {index} are not valid UTF-8"),
            MissingQuery => write!(f, "search has no query"),
            ConflictingQueries => write!(f, "search has conflicting queries"),
            MissingScope => write!(f, "search has no location scope"),
            MultipleScopes => write!(f, "search has more than one location scope"),
            UnsupportedScopeOption => write!(f, "search scope uses an unsupported option"),
        }
    }
}

impl std::error::Error for SearchParseError {}

/// Which input of [`build_search_ms`] is invalid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchField {
    Query,
    Scope,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchBuildError {
    /// An unpaired surrogate at `index` (UTF-16 units) cannot be UTF-8 encoded.
    UnpairedSurrogate { field: SearchField, index: usize },
    /// The scope parsing name is empty.
    EmptyScope,
}

impl fmt::Display for SearchBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SearchBuildError::UnpairedSurrogate { field, index } => {
                write!(f, "search {field:?} has an unpaired surrogate at unit {index}")
            }
            SearchBuildError::EmptyScope => write!(f, "search scope is empty"),
        }
    }
}

impl std::error::Error for SearchBuildError {}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const PERCENT: u16 = b'%' as u16;
const AMP: u16 = b'&' as u16;
const EQ: u16 = b'=' as u16;
const COMMA: u16 = b',' as u16;

/// ASCII case-insensitive prefix test of UTF-16 units against an ASCII pattern.
fn starts_with_ci(units: &[u16], pattern: &str) -> bool {
    units.len() >= pattern.len()
        && units.iter().zip(pattern.bytes()).all(|(&u, b)| {
            u < 0x80 && (u as u8).eq_ignore_ascii_case(&b)
        })
}

fn eq_ci(units: &[u16], pattern: &str) -> bool {
    units.len() == pattern.len() && starts_with_ci(units, pattern)
}

/// `true` if the parsing name uses the `search-ms:` scheme.
pub fn is_search_ms(parsing_name: &WideString) -> bool {
    starts_with_ci(parsing_name.units(), SCHEME)
}

/// Index of the first unpaired surrogate, if any.
fn first_unpaired_surrogate(units: &[u16]) -> Option<usize> {
    let mut i = 0;
    while i < units.len() {
        match units[i] {
            0xD800..=0xDBFF if matches!(units.get(i + 1), Some(0xDC00..=0xDFFF)) => i += 2,
            0xD800..=0xDFFF => return Some(i),
            _ => i += 1,
        }
    }
    None
}

fn hex_value(u: u16) -> Option<u8> {
    match u {
        0x30..=0x39 => Some((u - 0x30) as u8),
        0x41..=0x46 => Some((u - 0x41 + 10) as u8),
        0x61..=0x66 => Some((u - 0x61 + 10) as u8),
        _ => None,
    }
}

/// Percent-decodes `units` (which start at `base` in the whole parsing name).
/// Runs of escapes are decoded as UTF-8; raw units are copied verbatim.
fn percent_decode(units: &[u16], base: usize) -> Result<Vec<u16>, SearchParseError> {
    fn flush(out: &mut Vec<u16>, bytes: &mut Vec<u8>, start: usize) -> Result<(), SearchParseError> {
        if !bytes.is_empty() {
            let s = std::str::from_utf8(bytes)
                .map_err(|_| SearchParseError::InvalidUtf8 { index: start })?;
            out.extend(s.encode_utf16());
            bytes.clear();
        }
        Ok(())
    }

    let mut out = Vec::with_capacity(units.len());
    let mut bytes = Vec::new();
    let mut run_start = 0;
    let mut i = 0;
    while i < units.len() {
        if units[i] == PERCENT {
            let hi = units.get(i + 1).copied().and_then(hex_value);
            let lo = units.get(i + 2).copied().and_then(hex_value);
            let (Some(hi), Some(lo)) = (hi, lo) else {
                return Err(SearchParseError::InvalidEscape { index: base + i });
            };
            if bytes.is_empty() {
                run_start = base + i;
            }
            bytes.push((hi << 4) | lo);
            i += 3;
        } else {
            flush(&mut out, &mut bytes, run_start)?;
            out.push(units[i]);
            i += 1;
        }
    }
    flush(&mut out, &mut bytes, run_start)?;
    Ok(out)
}

/// Splits `units` (starting at `base`) at every raw `sep`, yielding
/// `(offset, segment)` pairs.
fn split_raw(units: &[u16], base: usize, sep: u16) -> impl Iterator<Item = (usize, &[u16])> {
    let mut offset = base;
    units.split(move |&u| u == sep).map(move |seg| {
        let start = offset;
        offset += seg.len() + 1;
        (start, seg)
    })
}

/// Pushes `value` unless an equal value is already present.
fn push_distinct(values: &mut Vec<Vec<u16>>, value: Vec<u16>) {
    if !values.contains(&value) {
        values.push(value);
    }
}

/// Handles one `crumb=` value, collecting query and scope candidates.
fn parse_crumb(
    value: &[u16],
    base: usize,
    queries: &mut Vec<Vec<u16>>,
    scopes: &mut Vec<Vec<u16>>,
) -> Result<(), SearchParseError> {
    // Explorer writes `System.Generic.String%3A<query>`: the column separator is
    // escaped, so match the column after decoding the whole crumb.
    let decoded = percent_decode(value, base)?;
    if starts_with_ci(&decoded, QUERY_COLUMN) {
        push_distinct(queries, decoded[QUERY_COLUMN.len()..].to_vec());
        return Ok(());
    }

    // `location:<value>[,<label>][,<inclusion>][,<recursion>]`; raw commas are
    // structural, literal commas in the path are `%2C`.
    let mut segments = split_raw(value, base, COMMA);
    let (off, first) = segments.next().unwrap_or((base, &[]));
    let first = percent_decode(first, off)?;
    if !starts_with_ci(&first, LOCATION_COLUMN) {
        return Ok(()); // other crumbs (kind:, store:, AQS) are not part of the identifier
    }
    let _label = segments.next();
    for (index, (off, arg)) in segments.enumerate() {
        let arg = percent_decode(arg, off)?;
        let ok = arg.is_empty()
            || match index {
                0 => eq_ci(&arg, "include"),
                1 => eq_ci(&arg, "recursive"),
                _ => false,
            };
        if !ok {
            return Err(SearchParseError::UnsupportedScopeOption);
        }
    }
    let scope = first[LOCATION_COLUMN.len()..].to_vec();
    if !scope.is_empty() {
        push_distinct(scopes, scope);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Extracts the query text and scope parsing name from a `search-ms:` parsing
/// name (Req 8.5). See the module docs for the exact rules.
pub fn parse_search_ms(parsing_name: &WideString) -> Result<ParsedSearch, SearchParseError> {
    let units = parsing_name.units();
    if !starts_with_ci(units, SCHEME) {
        return Err(SearchParseError::NotSearchMs);
    }
    if let Some(index) = first_unpaired_surrogate(units) {
        return Err(SearchParseError::UnpairedSurrogate { index });
    }

    let mut queries = Vec::new();
    let mut scopes = Vec::new();
    for (off, param) in split_raw(&units[SCHEME.len()..], SCHEME.len(), AMP) {
        let Some(eq) = param.iter().position(|&u| u == EQ) else {
            continue;
        };
        let (key, value, value_off) = (&param[..eq], &param[eq + 1..], off + eq + 1);
        if eq_ci(key, "crumb") {
            parse_crumb(value, value_off, &mut queries, &mut scopes)?;
        } else if eq_ci(key, "query") {
            push_distinct(&mut queries, percent_decode(value, value_off)?);
        }
    }

    let query = match queries.len() {
        0 => return Err(SearchParseError::MissingQuery),
        1 => queries.pop().unwrap_or_default(),
        _ => return Err(SearchParseError::ConflictingQueries),
    };
    let scope = match scopes.len() {
        0 => return Err(SearchParseError::MissingScope),
        1 => scopes.pop().unwrap_or_default(),
        _ => return Err(SearchParseError::MultipleScopes),
    };
    Ok(ParsedSearch { query: WideString(query), scope: WideString(scope) })
}

/// Appends `units` percent-encoded as UTF-8; only RFC 3986 unreserved
/// characters stay raw. `units` must be valid UTF-16.
fn percent_encode_into(out: &mut Vec<u16>, units: &[u16]) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut buf = [0u8; 4];
    for c in char::decode_utf16(units.iter().copied()).map(|r| r.unwrap_or('\u{FFFD}')) {
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~') {
            out.push(c as u16);
        } else {
            for &b in c.encode_utf8(&mut buf).as_bytes() {
                out.extend([b'%' as u16, HEX[(b >> 4) as usize] as u16, HEX[(b & 0xF) as usize] as u16]);
            }
        }
    }
}

/// Builds a fresh `search-ms:` URL that reruns the search for `query` within
/// the scope whose parsing name is `scope` (Req 8.6).
pub fn build_search_ms(query: &WideString, scope: &WideString) -> Result<WideString, SearchBuildError> {
    if scope.is_empty() {
        return Err(SearchBuildError::EmptyScope);
    }
    for (field, value) in [(SearchField::Query, query), (SearchField::Scope, scope)] {
        if let Some(index) = first_unpaired_surrogate(value.units()) {
            return Err(SearchBuildError::UnpairedSurrogate { field, index });
        }
    }

    let mut out: Vec<u16> = "search-ms:crumb=System.Generic.String%3A".encode_utf16().collect();
    percent_encode_into(&mut out, query.units());
    out.extend("&crumb=location:".encode_utf16());
    percent_encode_into(&mut out, scope.units());
    Ok(WideString(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> WideString {
        s.into()
    }

    fn parse(s: &str) -> Result<ParsedSearch, SearchParseError> {
        parse_search_ms(&w(s))
    }

    fn parsed(query: &str, scope: &str) -> Result<ParsedSearch, SearchParseError> {
        Ok(ParsedSearch { query: w(query), scope: w(scope) })
    }

    fn build(query: &str, scope: &str) -> String {
        build_search_ms(&w(query), &w(scope)).unwrap().to_string_lossy()
    }

    #[test]
    fn parses_real_explorer_url() {
        let url = "search-ms:displayname=Search%20Results%20in%20Downloads\
                   &crumb=System.Generic.String%3Afoo\
                   &crumb=location:C%3A%5CUsers%5Cme%5CDownloads";
        assert_eq!(parse(url), parsed("foo", r"C:\Users\me\Downloads"));
        assert!(is_search_ms(&w(url)));
    }

    #[test]
    fn scheme_and_names_are_case_insensitive() {
        assert_eq!(
            parse("SEARCH-MS:CRUMB=system.generic.string%3aBar&Crumb=LOCATION:D%3a%5c"),
            parsed("Bar", r"D:\")
        );
        assert!(is_search_ms(&w("Search-Ms:x")));
        assert_eq!(parse("search:query=a&crumb=location:C%3A%5C"), Err(SearchParseError::NotSearchMs));
        assert_eq!(parse(r"C:\Users"), Err(SearchParseError::NotSearchMs));
        assert_eq!(parse(""), Err(SearchParseError::NotSearchMs));
    }

    #[test]
    fn query_parameter_and_agreeing_sources() {
        assert_eq!(parse("search-ms:query=a%20b&crumb=location:C%3A%5Cx"), parsed("a b", r"C:\x"));
        // Both forms present with the same text.
        assert_eq!(
            parse("search-ms:query=q&crumb=System.Generic.String%3Aq&crumb=location:C%3A%5C"),
            parsed("q", r"C:\")
        );
        assert_eq!(
            parse("search-ms:query=a&crumb=System.Generic.String%3Ab&crumb=location:C%3A%5C"),
            Err(SearchParseError::ConflictingQueries)
        );
    }

    #[test]
    fn decodes_special_characters_and_unicode() {
        // `&`, `=`, `%`, `:`, `,`, `+`, space, and UTF-8 (é, 😀).
        let url = "search-ms:crumb=System.Generic.String%3Aa%26b%3Dc%25d%3Ae%2Cf+g%20%C3%A9%F0%9F%98%80\
                   &crumb=location:C%3A%5CJos%C3%A9";
        assert_eq!(parse(url), parsed("a&b=c%d:e,f+g é😀", "C:\\Jos\u{e9}"));
        // Raw non-ASCII and raw colons/backslashes left unencoded by the producer.
        assert_eq!(
            parse("search-ms:crumb=System.Generic.String:日本 語&crumb=location:C:\\Users\\José"),
            parsed("日本 語", "C:\\Users\\José")
        );
    }

    #[test]
    fn empty_query_is_allowed() {
        assert_eq!(parse("search-ms:crumb=System.Generic.String%3A&crumb=location:C%3A%5C"), parsed("", r"C:\"));
    }

    #[test]
    fn missing_parts() {
        assert_eq!(parse("search-ms:crumb=location:C%3A%5C"), Err(SearchParseError::MissingQuery));
        assert_eq!(parse("search-ms:"), Err(SearchParseError::MissingQuery));
        assert_eq!(parse("search-ms:query=foo"), Err(SearchParseError::MissingScope));
        assert_eq!(parse("search-ms:query=foo&crumb=location:"), Err(SearchParseError::MissingScope));
        // Unknown parameters, valueless parameters and other crumbs are ignored.
        assert_eq!(
            parse("search-ms:displayname=X&flag&crumb=kind:pics&&query=foo&crumb=location:C%3A%5C"),
            parsed("foo", r"C:\")
        );
    }

    #[test]
    fn multiple_scopes() {
        assert_eq!(
            parse("search-ms:query=a&crumb=location:C%3A%5C&crumb=location:D%3A%5C"),
            Err(SearchParseError::MultipleScopes)
        );
        // Identical duplicates collapse (raw and encoded spellings decode equal).
        assert_eq!(
            parse("search-ms:query=a&crumb=location:C%3A%5C&crumb=location:C:%5C"),
            parsed("a", r"C:\")
        );
    }

    #[test]
    fn location_crumb_arguments() {
        assert_eq!(
            parse("search-ms:query=a&crumb=location:shell%3APersonal,Docs,include,recursive"),
            parsed("a", "shell:Personal")
        );
        assert_eq!(parse("search-ms:query=a&crumb=location:C%3A%5Ca%2Cb,"), parsed("a", r"C:\a,b"));
        for url in [
            "search-ms:query=a&crumb=location:C%3A%5C&crumb=location:C%3A%5Cx,,exclude",
            "search-ms:query=a&crumb=location:C%3A%5C,,,nonrecursive",
            "search-ms:query=a&crumb=location:C%3A%5C,,include,recursive,kind:pics",
        ] {
            assert_eq!(parse(url), Err(SearchParseError::UnsupportedScopeOption), "{url}");
        }
    }

    #[test]
    fn invalid_escapes() {
        // Index points at the offending `%` (units from the start of the URL).
        let base = "search-ms:query=".len();
        assert_eq!(parse("search-ms:query=%"), Err(SearchParseError::InvalidEscape { index: base }));
        assert_eq!(parse("search-ms:query=a%2"), Err(SearchParseError::InvalidEscape { index: base + 1 }));
        assert_eq!(parse("search-ms:query=%zz"), Err(SearchParseError::InvalidEscape { index: base }));
        // Lone continuation byte, truncated sequence, encoded surrogate (CESU/WTF-8).
        assert_eq!(parse("search-ms:query=a%80"), Err(SearchParseError::InvalidUtf8 { index: base + 1 }));
        assert_eq!(parse("search-ms:query=%C3x"), Err(SearchParseError::InvalidUtf8 { index: base }));
        assert_eq!(parse("search-ms:query=%ED%A0%80"), Err(SearchParseError::InvalidUtf8 { index: base }));
        // Errors inside crumbs are reported too.
        assert!(matches!(
            parse("search-ms:query=a&crumb=location:C%3"),
            Err(SearchParseError::InvalidEscape { .. })
        ));
    }

    #[test]
    fn rejects_unpaired_surrogates_in_input() {
        let mut url = w("search-ms:query=a");
        url.0.push(0xD800);
        url.0.extend(w("&crumb=location:C%3A%5C").0);
        assert_eq!(parse_search_ms(&url), Err(SearchParseError::UnpairedSurrogate { index: 17 }));
    }

    #[test]
    fn builds_explorer_form() {
        assert_eq!(
            build("foo", r"C:\Users\me\Downloads"),
            "search-ms:crumb=System.Generic.String%3Afoo&crumb=location:C%3A%5CUsers%5Cme%5CDownloads"
        );
        assert_eq!(
            build("a&b=c%d:e,f+g ~-._é😀", "::{20D04FE0-3AEA-1069-A2D8-08002B30309D}"),
            "search-ms:crumb=System.Generic.String%3Aa%26b%3Dc%25d%3Ae%2Cf%2Bg%20~-._%C3%A9%F0%9F%98%80\
             &crumb=location:%3A%3A%7B20D04FE0-3AEA-1069-A2D8-08002B30309D%7D"
        );
    }

    #[test]
    fn build_round_trips_examples() {
        for (q, s) in [
            ("foo", r"C:\Users\me\Downloads"),
            ("", r"\\server\share\dir"),
            ("a&b=c%d:e,f+g", "C:\\Jos\u{e9}\\日本"),
            ("crumb=location:X&query=y", "shell:Personal"),
            ("%20%zz", r"C:\a,b,,exclude"),
            ("😀\u{301}e\u{301}", "::{645FF040-5081-101B-9F08-00AA002F954E}"),
        ] {
            let url = build_search_ms(&w(q), &w(s)).unwrap();
            assert_eq!(parse_search_ms(&url), parsed(q, s), "{q:?} in {s:?}");
        }
    }

    #[test]
    fn build_rejects_unencodable_input() {
        let mut q = w("ab");
        q.0.push(0xDC00);
        assert_eq!(
            build_search_ms(&q, &w(r"C:\")),
            Err(SearchBuildError::UnpairedSurrogate { field: SearchField::Query, index: 2 })
        );
        let s = WideString(vec![0xD800, b'x' as u16]);
        assert_eq!(
            build_search_ms(&w("q"), &s),
            Err(SearchBuildError::UnpairedSurrogate { field: SearchField::Scope, index: 0 })
        );
        assert_eq!(build_search_ms(&w("q"), &w("")), Err(SearchBuildError::EmptyScope));
    }
}
