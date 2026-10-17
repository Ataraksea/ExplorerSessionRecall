//! Property test for `search-ms:` parsing and building (`esr_core::search`).

mod common;

use common::strategies::*;
use esr_core::model::*;
use esr_core::search::*;
use proptest::collection::vec;
use proptest::prelude::*;
use proptest::sample::select;

/// Pieces that stress the URL syntax: separators, escapes, crumb syntax,
/// controls, NFC/NFD forms, supplementary characters.
const PIECES: &[&str] = &[
    "&", "=", "%", ":", ",", "+", " ", "\\", "/", "?", "#", "~", "-", ".", "_", ";", "\"",
    "\t", "\n", "\r", "\0", "\u{7F}", "\u{85}", "\u{A0}", "\u{FEFF}", "\u{FFFD}",
    "a", "Z", "0", "é", "e\u{301}", "Å", "A\u{30A}", "ガ", "カ\u{3099}", "日本", "Ж",
    "😀", "👍🏽", "\u{10FFFF}", "%20", "%2", "%zz", "%C3%A9", "%%", "crumb=", "location:",
    "System.Generic.String:", "query=", ",include,recursive", ",,exclude", "search-ms:",
];

fn piece_string() -> impl Strategy<Value = String> {
    vec(select(PIECES), 0..24).prop_map(|p| p.concat())
}

/// Any valid-Unicode query, including empty.
fn query() -> impl Strategy<Value = String> {
    prop_oneof![
        4 => piece_string(),
        2 => any::<String>(),
        1 => "\\PC{0,60}",
        1 => Just(String::new()),
    ]
}

/// Realistic scope parsing names: paths and shell names from the shared
/// location strategy (lossily made valid Unicode), plus a few fixed ones.
fn realistic_scope() -> impl Strategy<Value = String> {
    prop_oneof![
        3 => plain_location_id().prop_map(|l| l.parsing_name.to_string_lossy()),
        1 => select(&[
            r"C:\Users\me\Downloads",
            r"\\server\share\dir",
            r"Z:\",
            "::{20D04FE0-3AEA-1069-A2D8-08002B30309D}",
            "::{031E4825-7B94-4DC3-B131-E946B44C8DD5}\\{7B0DB17D-9CD2-4A93-9733-46CC89022E7C}",
            "shell:Personal",
            "C:\\Jos\u{e9}\\日本\\a,b&c=d%20",
        ][..])
        .prop_map(str::to_owned),
    ]
}

/// Any non-empty valid-Unicode scope.
fn scope() -> impl Strategy<Value = String> {
    prop_oneof![
        3 => realistic_scope(),
        2 => piece_string(),
        2 => any::<String>(),
    ]
    .prop_filter("scope must be non-empty", |s| !s.is_empty())
}

fn w(s: &str) -> WideString {
    WideString::from(s)
}

/// Unit offsets of `s` at which a new unit cannot complete a surrogate pair
/// (char boundaries in UTF-16).
fn char_boundaries(s: &str) -> Vec<usize> {
    let mut out = vec![0];
    let mut at = 0;
    for c in s.chars() {
        at += c.len_utf16();
        out.push(at);
    }
    out
}

/// Inserts a lone surrogate at a char boundary chosen by `pick`; returns the
/// corrupted string and the index of the unpaired unit.
fn with_lone_surrogate(s: &str, pick: prop::sample::Index, surrogate: u16) -> (WideString, usize) {
    let bounds = char_boundaries(s);
    let at = bounds[pick.index(bounds.len())];
    let mut units = w(s).0;
    units.insert(at, surrogate);
    (WideString(units), at)
}

fn lone_surrogate() -> impl Strategy<Value = u16> {
    prop_oneof![0xD800u16..=0xDBFF, 0xDC00u16..=0xDFFF]
}

/// Encodes like Explorer: ASCII outside the unreserved set as `%XX` (upper or
/// lower hex), everything non-ASCII left raw.
fn explorer_encode(s: &str, lower_hex: bool) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if !c.is_ascii() || c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~') {
            out.push(c);
        } else if lower_hex {
            out.push_str(&format!("%{:02x}", c as u32));
        } else {
            out.push_str(&format!("%{:02X}", c as u32));
        }
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// **Property 25: Search location round trip.** For any query (any
    /// Unicode, including `&`, `%`, `:`, `=`, empty) and any non-empty scope,
    /// `parse_search_ms(build_search_ms(q, s)) == ParsedSearch { q, s }`.
    /// Building rejects unpaired surrogates and an empty scope, and a
    /// real-Explorer-shaped URL (extra `displayname=`, raw non-ASCII) parses
    /// to the same pair as its rebuilt form.
    ///
    /// **Validates: Requirements 8.5, 8.6**
    #[test]
    fn prop25_search_location_round_trip(
        q in query(),
        s in scope(),
        display in piece_string(),
        lower_hex in any::<bool>(),
        display_first in any::<bool>(),
        pick in any::<prop::sample::Index>(),
        sur in lone_surrogate(),
    ) {
        let (wq, ws) = (w(&q), w(&s));
        let expected = ParsedSearch { query: wq.clone(), scope: ws.clone() };

        // Round trip through our own builder.
        let url = build_search_ms(&wq, &ws).expect("valid query and non-empty scope must build");
        prop_assert!(is_search_ms(&url));
        prop_assert_eq!(parse_search_ms(&url), Ok(expected.clone()), "url = {}", url.to_string_lossy());

        // Explorer-shaped URL: displayname parameter, raw non-ASCII.
        let q_param = format!("crumb=System.Generic.String%3A{}", explorer_encode(&q, lower_hex));
        let s_param = format!("crumb=location:{}", explorer_encode(&s, lower_hex));
        let d_param = format!("displayname={}", explorer_encode(&display, lower_hex));
        let explorer_url = if display_first {
            format!("search-ms:{d_param}&{q_param}&{s_param}")
        } else {
            format!("search-ms:{q_param}&{s_param}&{d_param}")
        };
        let parsed = parse_search_ms(&w(&explorer_url));
        prop_assert_eq!(&parsed, &Ok(expected.clone()), "explorer url = {}", explorer_url);
        let rebuilt = build_search_ms(&wq, &ws).expect("rebuild");
        prop_assert_eq!(parse_search_ms(&rebuilt), parsed);

        // Unpaired surrogates cannot be encoded.
        let (bad_q, at) = with_lone_surrogate(&q, pick, sur);
        prop_assert_eq!(
            build_search_ms(&bad_q, &ws),
            Err(SearchBuildError::UnpairedSurrogate { field: SearchField::Query, index: at })
        );
        let (bad_s, at) = with_lone_surrogate(&s, pick, sur);
        prop_assert_eq!(
            build_search_ms(&wq, &bad_s),
            Err(SearchBuildError::UnpairedSurrogate { field: SearchField::Scope, index: at })
        );

        // An empty scope is rejected, whatever the query.
        prop_assert_eq!(build_search_ms(&wq, &WideString::default()), Err(SearchBuildError::EmptyScope));
        prop_assert_eq!(build_search_ms(&bad_q, &WideString::default()), Err(SearchBuildError::EmptyScope));
    }
}
