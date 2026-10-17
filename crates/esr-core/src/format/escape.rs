//! Value escaping (`escape`/`unescape`) and RFC 4648 base64 for PIDLs.
//!
//! Requirements 8.4, 11.3 (see design.md, "Session_File_Format (version 1)").
//!
//! # Value escaping
//!
//! A [`WideString`] holds raw UTF-16, possibly with unpaired surrogates. It
//! is written as UTF-8 text in which:
//!
//! * `%` is written `%25`;
//! * space (`%20`), `=` (`%3D`), the C0 controls U+0000–U+001F, DEL U+007F,
//!   and the C1 controls U+0080–U+009F are written `%XX`, where `XX` is the
//!   code point (not its UTF-8 bytes) in two hex digits. DEL is included
//!   because Unicode classifies it as a control (`Cc`) alongside C0 and C1;
//! * an unpaired surrogate code unit is written `%uDXXX` (the code unit in
//!   four hex digits, always in `D800`–`DFFF`);
//! * every other Unicode scalar value is written raw, so normalization form
//!   (NFC/NFD) and case are preserved exactly. A valid surrogate pair is one
//!   scalar value and is therefore written raw.
//!
//! The escaped form never contains a space, `=`, LF, or any other control,
//! so it can be embedded as a `key=value` field of a line.
//!
//! # Canonical form and strictness
//!
//! Hex digits are always **uppercase** (`0-9`, `A-F`). [`unescape`] accepts
//! only the canonical form that [`escape`] produces and rejects everything
//! else, so `escape(unescape(s)?) == s` for every accepted `s` and
//! `unescape(escape(w)) == Ok(w)` for every `w`. In particular it rejects:
//!
//! * lowercase hex digits (`%3d`, `%ud800`);
//! * `%XX` for a character that is written raw (for example `%41`);
//! * `%u` for a value outside `D800`–`DFFF`;
//! * an escaped high surrogate immediately followed by an escaped low
//!   surrogate (that pair is a scalar value and must be written raw);
//! * a raw `%`-less occurrence of a character that must be escaped
//!   (space, `=`, controls);
//! * a `%` that is not followed by a complete escape.
//!
//! # Base64
//!
//! PIDLs use the RFC 4648 §4 standard alphabet (`A-Z a-z 0-9 + /`) with `=`
//! padding and no line breaks. [`base64_decode`] is strict: the length must be
//! a multiple of four, padding may appear only at the end of the final
//! quantum, and the unused trailing bits before the padding must be zero
//! (RFC 4648 §3.5), so every input decodes from exactly one encoding.
//!
//! # Error offsets
//!
//! Error offsets are 0-based counts of Unicode scalar values (`char`s) from
//! the start of the input passed to the decoding function, pointing at the
//! first character of the offending escape or symbol. The parser adds the
//! column at which the value starts to report 1-based line/column positions.

use std::fmt;

use crate::model::WideString;

const HEX_UPPER: &[u8; 16] = b"0123456789ABCDEF";

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

// ---------------------------------------------------------------------------
// Value escaping
// ---------------------------------------------------------------------------

/// True if the scalar value `c` must be written as `%XX`.
fn must_escape(c: char) -> bool {
    matches!(c, '%' | ' ' | '=' | '\u{0}'..='\u{1F}' | '\u{7F}'..='\u{9F}')
}

fn is_high_surrogate(u: u16) -> bool {
    (0xD800..=0xDBFF).contains(&u)
}

fn is_low_surrogate(u: u16) -> bool {
    (0xDC00..=0xDFFF).contains(&u)
}

fn push_hex(out: &mut String, value: u32, digits: u32) {
    for shift in (0..digits).rev() {
        let nibble = (value >> (shift * 4)) & 0xF;
        out.push(char::from(HEX_UPPER[nibble as usize]));
    }
}

/// Appends the escaped form of the UTF-16 code units `units` to `out`.
pub fn escape_into(out: &mut String, units: &[u16]) {
    for decoded in char::decode_utf16(units.iter().copied()) {
        match decoded {
            Ok(c) if must_escape(c) => {
                out.push('%');
                push_hex(out, u32::from(c), 2);
            }
            Ok(c) => out.push(c),
            Err(e) => {
                out.push_str("%u");
                push_hex(out, u32::from(e.unpaired_surrogate()), 4);
            }
        }
    }
}

/// Escapes a [`WideString`] into its Session_File_Format value text.
pub fn escape(value: &WideString) -> String {
    let mut out = String::with_capacity(value.len());
    escape_into(&mut out, value.units());
    out
}

/// Escapes a Rust string (which never contains unpaired surrogates) with the
/// same rules as [`escape`]; used for log fields.
pub fn escape_str(value: &str) -> String {
    let units: Vec<u16> = value.encode_utf16().collect();
    let mut out = String::with_capacity(value.len());
    escape_into(&mut out, &units);
    out
}

/// What is wrong with an escaped value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EscapeErrorKind {
    /// A `%` is not followed by a complete `%XX` or `%uXXXX` escape.
    TruncatedEscape,
    /// An escape contains a character that is not an uppercase hex digit,
    /// or a `%u` escape names a value outside `D800`–`DFFF`.
    MalformedEscape,
    /// A well-formed escape that [`escape`] never produces: `%XX` for a
    /// character written raw, or an escaped valid surrogate pair.
    NonCanonicalEscape,
    /// A character that must be escaped (space, `=`, a control) appears raw.
    UnescapedCharacter,
}

/// Error returned by [`unescape`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EscapeError {
    /// 0-based offset in Unicode scalar values of the offending escape or
    /// character within the input value.
    pub offset: usize,
    pub kind: EscapeErrorKind,
}

impl fmt::Display for EscapeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = match self.kind {
            EscapeErrorKind::TruncatedEscape => "truncated escape",
            EscapeErrorKind::MalformedEscape => "malformed escape",
            EscapeErrorKind::NonCanonicalEscape => "non-canonical escape",
            EscapeErrorKind::UnescapedCharacter => "character must be escaped",
        };
        write!(f, "{what} at offset {}", self.offset)
    }
}

impl std::error::Error for EscapeError {}

fn upper_hex_value(c: char) -> Option<u32> {
    match c {
        '0'..='9' => Some(c as u32 - '0' as u32),
        'A'..='F' => Some(c as u32 - 'A' as u32 + 10),
        _ => None,
    }
}

/// Reads `n` uppercase hex digits from `chars`.
fn read_hex(chars: &mut std::str::Chars<'_>, n: usize, start: usize) -> Result<u32, EscapeError> {
    let mut value = 0u32;
    for _ in 0..n {
        let c = chars.next().ok_or(EscapeError {
            offset: start,
            kind: EscapeErrorKind::TruncatedEscape,
        })?;
        let digit = upper_hex_value(c).ok_or(EscapeError {
            offset: start,
            kind: EscapeErrorKind::MalformedEscape,
        })?;
        value = (value << 4) | digit;
    }
    Ok(value)
}

/// Decodes a value written by [`escape`]; the exact inverse of [`escape`].
///
/// Only the canonical escaped form is accepted (see the module docs).
pub fn unescape(text: &str) -> Result<WideString, EscapeError> {
    let mut out: Vec<u16> = Vec::with_capacity(text.len());
    let mut chars = text.chars();
    let mut offset = 0usize;
    // True when the last unit pushed came from a `%u` high-surrogate escape.
    let mut after_escaped_high = false;

    while let Some(c) = chars.next() {
        let start = offset;
        if c != '%' {
            if must_escape(c) {
                return Err(EscapeError {
                    offset: start,
                    kind: EscapeErrorKind::UnescapedCharacter,
                });
            }
            let mut buf = [0u16; 2];
            out.extend_from_slice(c.encode_utf16(&mut buf));
            after_escaped_high = false;
            offset += 1;
            continue;
        }

        let mut rest = chars.clone();
        match rest.next() {
            None => {
                return Err(EscapeError {
                    offset: start,
                    kind: EscapeErrorKind::TruncatedEscape,
                })
            }
            Some('u') => {
                let unit = read_hex(&mut rest, 4, start)?;
                // Four hex digits always fit in a u16.
                let unit = unit as u16;
                if !(is_high_surrogate(unit) || is_low_surrogate(unit)) {
                    return Err(EscapeError {
                        offset: start,
                        kind: EscapeErrorKind::MalformedEscape,
                    });
                }
                if after_escaped_high && is_low_surrogate(unit) {
                    return Err(EscapeError {
                        offset: start,
                        kind: EscapeErrorKind::NonCanonicalEscape,
                    });
                }
                out.push(unit);
                after_escaped_high = is_high_surrogate(unit);
                offset += 6;
            }
            Some(_) => {
                // Re-read the first digit together with the second.
                let mut digits = chars.clone();
                let value = read_hex(&mut digits, 2, start)?;
                let ch = char::from_u32(value).filter(|&ch| must_escape(ch)).ok_or(
                    EscapeError {
                        offset: start,
                        kind: EscapeErrorKind::NonCanonicalEscape,
                    },
                )?;
                out.push(ch as u16);
                after_escaped_high = false;
                offset += 3;
                rest = digits;
            }
        }
        chars = rest;
    }
    Ok(WideString::new(out))
}

// ---------------------------------------------------------------------------
// Base64 (RFC 4648 §4, padded)
// ---------------------------------------------------------------------------

/// Encodes `bytes` as padded RFC 4648 base64 without line breaks.
pub fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let sym = |v: u32| char::from(BASE64_ALPHABET[(v & 0x3F) as usize]);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map_or(0, u32::from);
        let b2 = chunk.get(2).copied().map_or(0, u32::from);
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(sym(n >> 18));
        out.push(sym(n >> 12));
        out.push(if chunk.len() > 1 { sym(n >> 6) } else { '=' });
        out.push(if chunk.len() > 2 { sym(n) } else { '=' });
    }
    out
}

/// What is wrong with a base64 value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Base64ErrorKind {
    /// A character outside the base64 alphabet and `=`.
    InvalidCharacter,
    /// `=` in a position where padding is not allowed, or data after padding.
    InvalidPadding,
    /// The unused bits before the padding are not zero (non-canonical).
    NonZeroTrailingBits,
    /// The input ends inside a 4-character quantum.
    InvalidLength,
}

/// Error returned by [`base64_decode`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Base64Error {
    /// 0-based offset in Unicode scalar values of the offending character
    /// (the input length for [`Base64ErrorKind::InvalidLength`]).
    pub offset: usize,
    pub kind: Base64ErrorKind,
}

impl fmt::Display for Base64Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = match self.kind {
            Base64ErrorKind::InvalidCharacter => "invalid base64 character",
            Base64ErrorKind::InvalidPadding => "invalid base64 padding",
            Base64ErrorKind::NonZeroTrailingBits => "non-zero base64 trailing bits",
            Base64ErrorKind::InvalidLength => "base64 length is not a multiple of 4",
        };
        write!(f, "{what} at offset {}", self.offset)
    }
}

impl std::error::Error for Base64Error {}

fn base64_value(c: char) -> Option<u8> {
    let v = match c {
        'A'..='Z' => c as u32 - 'A' as u32,
        'a'..='z' => c as u32 - 'a' as u32 + 26,
        '0'..='9' => c as u32 - '0' as u32 + 52,
        '+' => 62,
        '/' => 63,
        _ => return None,
    };
    Some(v as u8)
}

/// Decodes strict, padded RFC 4648 base64 (the exact inverse of
/// [`base64_encode`]).
pub fn base64_decode(text: &str) -> Result<Vec<u8>, Base64Error> {
    let err = |offset, kind| Base64Error { offset, kind };
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut quad = [0u8; 4];
    let mut filled = 0usize; // characters in the current quantum
    let mut pad = 0usize; // `=` seen so far
    let mut len = 0usize;

    for (i, c) in text.chars().enumerate() {
        len = i + 1;
        if c == '=' {
            // Padding may fill only positions 2 and 3 of a quantum; data
            // after it is rejected below, so it can only end the input.
            if filled < 2 {
                return Err(err(i, Base64ErrorKind::InvalidPadding));
            }
            pad += 1;
            quad[filled] = 0;
        } else {
            if pad > 0 {
                return Err(err(i, Base64ErrorKind::InvalidPadding));
            }
            quad[filled] = base64_value(c).ok_or(err(i, Base64ErrorKind::InvalidCharacter))?;
        }
        filled += 1;

        if filled == 4 {
            let n = (u32::from(quad[0]) << 18)
                | (u32::from(quad[1]) << 12)
                | (u32::from(quad[2]) << 6)
                | u32::from(quad[3]);
            match pad {
                0 => out.extend_from_slice(&[(n >> 16) as u8, (n >> 8) as u8, n as u8]),
                1 => {
                    if quad[2] & 0x03 != 0 {
                        return Err(err(i - 1, Base64ErrorKind::NonZeroTrailingBits));
                    }
                    out.extend_from_slice(&[(n >> 16) as u8, (n >> 8) as u8]);
                }
                _ => {
                    if quad[1] & 0x0F != 0 {
                        return Err(err(i - 2, Base64ErrorKind::NonZeroTrailingBits));
                    }
                    out.push((n >> 16) as u8);
                }
            }
            filled = 0;
        }
    }

    if filled != 0 {
        return Err(err(len, Base64ErrorKind::InvalidLength));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn w(units: &[u16]) -> WideString {
        WideString::new(units.to_vec())
    }

    fn round_trip(value: &WideString) -> String {
        let text = escape(value);
        assert_eq!(unescape(&text).as_ref(), Ok(value), "round trip of {text:?}");
        text
    }

    fn unescape_err(text: &str) -> EscapeError {
        unescape(text).expect_err(text)
    }

    #[test]
    fn empty_string() {
        assert_eq!(round_trip(&w(&[])), "");
        assert_eq!(base64_encode(&[]), "");
        assert_eq!(base64_decode(""), Ok(vec![]));
    }

    #[test]
    fn reserved_ascii() {
        assert_eq!(round_trip(&WideString::from("%")), "%25");
        assert_eq!(round_trip(&WideString::from("=")), "%3D");
        assert_eq!(round_trip(&WideString::from(" ")), "%20");
        assert_eq!(
            round_trip(&WideString::from(r"C:\Program Files\a=b%c")),
            r"C:\Program%20Files\a%3Db%25c"
        );
        assert_eq!(round_trip(&WideString::from("%25")), "%2525");
    }

    #[test]
    fn every_c0_and_c1_control_and_del() {
        for cp in (0x00u16..=0x1F).chain(0x7F..=0x9F) {
            let text = round_trip(&w(&[cp]));
            assert_eq!(text, format!("%{cp:02X}"));
        }
        // Raw controls are rejected.
        for cp in (0x00u32..=0x1F).chain(0x7F..=0x9F) {
            let raw = char::from_u32(cp).unwrap().to_string();
            assert_eq!(
                unescape_err(&raw),
                EscapeError { offset: 0, kind: EscapeErrorKind::UnescapedCharacter }
            );
        }
    }

    #[test]
    fn printable_ascii_and_latin1_stay_raw() {
        for cp in (0x21u16..=0x7E).chain(0xA0..=0xFF) {
            if cp == u16::from(b'%') || cp == u16::from(b'=') {
                continue;
            }
            let c = char::from_u32(u32::from(cp)).unwrap();
            assert_eq!(round_trip(&w(&[cp])), c.to_string());
        }
    }

    #[test]
    fn nfc_and_nfd_are_preserved_distinctly() {
        let nfc = WideString::from("caf\u{E9}");
        let nfd = WideString::from("cafe\u{301}");
        assert_eq!(round_trip(&nfc), "caf\u{E9}");
        assert_eq!(round_trip(&nfd), "cafe\u{301}");
        assert_ne!(escape(&nfc), escape(&nfd));
        // Case is preserved too.
        assert_eq!(round_trip(&WideString::from("ÄbC")), "ÄbC");
    }

    #[test]
    fn lone_surrogates_are_escaped() {
        assert_eq!(round_trip(&w(&[0xD800])), "%uD800");
        assert_eq!(round_trip(&w(&[0xDBFF])), "%uDBFF");
        assert_eq!(round_trip(&w(&[0xDC00])), "%uDC00");
        assert_eq!(round_trip(&w(&[0xDFFF])), "%uDFFF");
        // High surrogate followed by a non-surrogate.
        assert_eq!(round_trip(&w(&[0xD83D, 0x41])), "%uD83DA");
        // Low surrogate followed by a high surrogate is two lone surrogates.
        assert_eq!(round_trip(&w(&[0xDE00, 0xD83D])), "%uDE00%uD83D");
        // Two highs then a low: the first is lone, the last two pair.
        assert_eq!(round_trip(&w(&[0xD83D, 0xD83D, 0xDE00])), "%uD83D\u{1F600}");
        // Lone high at the end after a pair.
        assert_eq!(round_trip(&w(&[0xD83D, 0xDE00, 0xD83D])), "\u{1F600}%uD83D");
    }

    #[test]
    fn valid_surrogate_pair_stays_raw() {
        let text = round_trip(&w(&[0xD83D, 0xDE00]));
        assert_eq!(text, "\u{1F600}");
        assert_eq!(text.chars().count(), 1);
    }

    #[test]
    fn escape_str_matches_escape() {
        let s = "a b=c%\u{1F600}\u{85}";
        assert_eq!(escape_str(s), escape(&WideString::from(s)));
        assert_eq!(escape_str(s), "a%20b%3Dc%25\u{1F600}%85");
    }

    #[test]
    fn unescape_rejects_lowercase_hex() {
        assert_eq!(
            unescape_err("%3d"),
            EscapeError { offset: 0, kind: EscapeErrorKind::MalformedEscape }
        );
        assert_eq!(
            unescape_err("x%ud800"),
            EscapeError { offset: 1, kind: EscapeErrorKind::MalformedEscape }
        );
        assert_eq!(
            unescape_err("%uDbFF"),
            EscapeError { offset: 0, kind: EscapeErrorKind::MalformedEscape }
        );
    }

    #[test]
    fn unescape_rejects_non_canonical_escapes() {
        // `A` is written raw, so `%41` is never produced.
        assert_eq!(
            unescape_err("ab%41"),
            EscapeError { offset: 2, kind: EscapeErrorKind::NonCanonicalEscape }
        );
        // An escaped valid pair must be written raw.
        assert_eq!(
            unescape_err("%uD83D%uDE00"),
            EscapeError { offset: 6, kind: EscapeErrorKind::NonCanonicalEscape }
        );
        // `%u` outside the surrogate range.
        assert_eq!(
            unescape_err("%u0041"),
            EscapeError { offset: 0, kind: EscapeErrorKind::MalformedEscape }
        );
    }

    #[test]
    fn unescape_rejects_truncated_and_raw_reserved() {
        for (text, offset) in [("%", 0), ("a%2", 1), ("%u", 0), ("é%uD80", 1)] {
            assert_eq!(
                unescape_err(text),
                EscapeError { offset, kind: EscapeErrorKind::TruncatedEscape },
                "{text:?}"
            );
        }
        assert_eq!(
            unescape_err("%G0"),
            EscapeError { offset: 0, kind: EscapeErrorKind::MalformedEscape }
        );
        // Offsets count scalar values, not bytes.
        assert_eq!(
            unescape_err("\u{1F600}é a"),
            EscapeError { offset: 2, kind: EscapeErrorKind::UnescapedCharacter }
        );
        assert_eq!(
            unescape_err("a=b"),
            EscapeError { offset: 1, kind: EscapeErrorKind::UnescapedCharacter }
        );
        assert_eq!(
            unescape_err("a\nb"),
            EscapeError { offset: 1, kind: EscapeErrorKind::UnescapedCharacter }
        );
    }

    #[test]
    fn escaped_text_never_contains_separators() {
        let all_bmp: Vec<u16> = (0..=0xFFFFu16).collect();
        let text = escape(&w(&all_bmp));
        assert!(!text.contains([' ', '=', '\n', '\r', '\t', '\u{85}']));
        assert_eq!(unescape(&text), Ok(w(&all_bmp)));
    }

    #[test]
    fn base64_rfc4648_test_vectors() {
        let vectors: [(&[u8], &str); 7] = [
            (b"", ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"fooba", "Zm9vYmE="),
            (b"foobar", "Zm9vYmFy"),
        ];
        for (bytes, text) in vectors {
            assert_eq!(base64_encode(bytes), text);
            assert_eq!(base64_decode(text).as_deref(), Ok(bytes));
        }
    }

    #[test]
    fn base64_all_byte_values_and_alphabet() {
        let bytes: Vec<u8> = (0..=255u8).collect();
        let text = base64_encode(&bytes);
        assert_eq!(base64_decode(&text), Ok(bytes));
        assert_eq!(base64_encode(&[0xFB, 0xFF, 0xBF]), "+/+/");
    }

    #[test]
    fn base64_rejects_malformed_input() {
        let cases: [(&str, usize, Base64ErrorKind); 11] = [
            ("Zg", 2, Base64ErrorKind::InvalidLength),
            ("Zm9vY", 5, Base64ErrorKind::InvalidLength),
            ("Zg=", 3, Base64ErrorKind::InvalidLength),
            ("Zm9v-g==", 4, Base64ErrorKind::InvalidCharacter),
            ("Zm 9", 2, Base64ErrorKind::InvalidCharacter),
            ("é===", 0, Base64ErrorKind::InvalidCharacter),
            ("=AAA", 0, Base64ErrorKind::InvalidPadding),
            ("A===", 1, Base64ErrorKind::InvalidPadding),
            ("Zg=A", 3, Base64ErrorKind::InvalidPadding),
            ("Zg==Zm8=", 4, Base64ErrorKind::InvalidPadding),
            ("Zh==", 1, Base64ErrorKind::NonZeroTrailingBits),
        ];
        for (text, offset, kind) in cases {
            assert_eq!(base64_decode(text), Err(Base64Error { offset, kind }), "{text:?}");
        }
        assert_eq!(
            base64_decode("Zm9="),
            Err(Base64Error { offset: 2, kind: Base64ErrorKind::NonZeroTrailingBits })
        );
    }
}
