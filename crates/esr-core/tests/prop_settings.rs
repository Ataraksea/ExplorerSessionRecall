//! Property 26: Settings persistence round trip.
//!
//! *For any* `Settings` value, `Settings::parse_or_default(serialize(s)) == s`.
//! *For any* string that does not parse, each option falls back to its
//! documented default.
//!
//! Checked here as three properties:
//! 1. round trip of `serialize` / `parse_or_default`;
//! 2. robustness: arbitrary text never panics, and for generated documents
//!    mixing valid lines, unreadable values, unknown keys, comments and other
//!    sections the result matches a simple independent model (last readable
//!    value in the settings section, or before any section, wins; otherwise
//!    the default);
//! 3. per-option independence: corrupting one option never changes the others.
//!
//! **Validates: Requirements 12.1, 12.2, 12.4, 12.10**

use esr_core::settings::Settings;
use proptest::prelude::*;

const KEYS: [&str; 4] = [
    "restoration_enabled",
    "start_at_sign_in",
    "builtin_prompt_declined",
    "tab_folding_enabled",
];
const TRUE_WORDS: [&str; 4] = ["true", "1", "yes", "on"];
const FALSE_WORDS: [&str; 4] = ["false", "0", "no", "off"];

fn get(s: &Settings, i: usize) -> bool {
    match i {
        0 => s.restoration_enabled,
        1 => s.start_at_sign_in,
        2 => s.builtin_prompt_declined,
        _ => s.tab_folding_enabled,
    }
}

fn set(s: &mut Settings, i: usize, v: bool) {
    match i {
        0 => s.restoration_enabled = v,
        1 => s.start_at_sign_in = v,
        2 => s.builtin_prompt_declined = v,
        _ => s.tab_folding_enabled = v,
    }
}

/// Independent check of whether a (trimmed) value is a documented boolean.
fn is_bool_word(v: &str) -> bool {
    let v = v.to_ascii_lowercase();
    TRUE_WORDS.contains(&v.as_str()) || FALSE_WORDS.contains(&v.as_str())
}

fn normalize_key(k: &str) -> String {
    k.trim().to_ascii_lowercase().replace('-', "_")
}

fn arb_settings() -> impl Strategy<Value = Settings> {
    (any::<bool>(), any::<bool>(), any::<bool>(), any::<bool>()).prop_map(|(a, b, c, d)| Settings {
        restoration_enabled: a,
        start_at_sign_in: b,
        builtin_prompt_declined: c,
        tab_folding_enabled: d,
    })
}

/// Randomly re-cases ASCII letters of `word`; optionally turns `_` into `-`.
fn respell(word: &'static str, dashes: bool) -> impl Strategy<Value = String> {
    prop::collection::vec((any::<bool>(), any::<bool>()), word.len()).prop_map(move |flags| {
        word.chars()
            .zip(flags)
            .map(|(c, (upper, dash))| match c {
                '_' if dashes && dash => '-',
                c if upper => c.to_ascii_uppercase(),
                c => c,
            })
            .collect()
    })
}

fn pad() -> impl Strategy<Value = String> {
    "[ \t]{0,2}"
}

fn key_spelling(i: usize) -> impl Strategy<Value = String> {
    respell(KEYS[i], true)
}

fn bool_spelling(v: bool) -> impl Strategy<Value = String> {
    let words = if v { TRUE_WORDS } else { FALSE_WORDS };
    prop::sample::select(words.to_vec()).prop_flat_map(|w| respell(w, false))
}

/// A value that is not a readable boolean once trimmed.
fn garbage_value() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9=_ .+-]{0,8}".prop_filter("must not be a boolean", |v| !is_bool_word(v.trim()))
}

/// What a generated line means to the model.
#[derive(Clone, Debug)]
enum Effect {
    Ignored,
    /// Section header; `true` when it is `[settings]`.
    Section(bool),
    /// A readable assignment of option `i`.
    Set(usize, bool),
}

fn assignment(key: impl Strategy<Value = String>, value: impl Strategy<Value = String>) -> impl Strategy<Value = String> {
    (pad(), key, pad(), pad(), value, pad()).prop_map(|(p1, k, p2, p3, v, p4)| format!("{p1}{k}{p2}={p3}{v}{p4}"))
}

fn valid_line() -> impl Strategy<Value = (String, Effect)> {
    (0..4usize, any::<bool>()).prop_flat_map(|(i, v)| {
        assignment(key_spelling(i), bool_spelling(v)).prop_map(move |t| (t, Effect::Set(i, v)))
    })
}

fn invalid_value_line() -> impl Strategy<Value = (String, Effect)> {
    (0..4usize).prop_flat_map(|i| {
        prop_oneof![
            assignment(key_spelling(i), garbage_value()),
            // No `=` at all.
            (pad(), key_spelling(i), pad()).prop_map(|(a, k, b)| format!("{a}{k}{b}")),
        ]
        .prop_map(|t| (t, Effect::Ignored))
    })
}

fn unknown_key_line() -> impl Strategy<Value = (String, Effect)> {
    let near_miss = (0..4usize, "[a-z0-9_]{1,3}", any::<bool>()).prop_map(|(i, extra, prefix)| {
        if prefix { format!("{extra}{}", KEYS[i]) } else { format!("{}{extra}", KEYS[i]) }
    });
    let key = prop_oneof!["[a-zA-Z][a-zA-Z0-9_-]{0,15}", near_miss]
        .prop_filter("must be unknown", |k| !KEYS.contains(&normalize_key(k).as_str()));
    assignment(key, any::<bool>().prop_flat_map(bool_spelling)).prop_map(|t| (t, Effect::Ignored))
}

fn comment_line() -> impl Strategy<Value = (String, Effect)> {
    let body = prop_oneof![valid_line().prop_map(|(t, _)| t), "[^\n]{0,20}"];
    (pad(), prop::sample::select(vec![';', '#']), body).prop_map(|(p, c, b)| (format!("{p}{c}{b}"), Effect::Ignored))
}

fn section_line() -> impl Strategy<Value = (String, Effect)> {
    let ours = (pad(), pad(), respell("settings", false), pad(), pad())
        .prop_map(|(a, b, n, c, d)| (format!("{a}[{b}{n}{c}]{d}"), Effect::Section(true)));
    let other = "[a-zA-Z0-9 _]{0,10}"
        .prop_filter("must not be [settings]", |n| !n.trim().eq_ignore_ascii_case("settings"))
        .prop_map(|n| (format!("[{n}]"), Effect::Section(false)));
    // No closing bracket: documented as "not ours".
    let malformed = "[a-zA-Z ]{0,10}".prop_map(|n| (format!("[{n}"), Effect::Section(false)));
    prop_oneof![ours, other, malformed]
}

fn noise_line() -> impl Strategy<Value = (String, Effect)> {
    prop_oneof![
        "[ \t]{0,3}",
        // No `=` and no `[`, so it can never be an assignment or header.
        "[^\n=\\[]{0,20}",
    ]
    .prop_map(|t| (t, Effect::Ignored))
}

fn any_line() -> impl Strategy<Value = (String, Effect)> {
    prop_oneof![
        4 => valid_line(),
        2 => invalid_value_line(),
        1 => unknown_key_line(),
        1 => comment_line(),
        2 => section_line(),
        1 => noise_line(),
    ]
}

/// A generated document: optional BOM, lines with mixed LF / CRLF endings,
/// optional trailing newline.
fn arb_document() -> impl Strategy<Value = (Vec<(String, Effect)>, String)> {
    (
        prop::collection::vec((any_line(), any::<bool>()), 0..16),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(|(lines, bom, trailing)| {
            let mut text = String::new();
            if bom {
                text.push('\u{FEFF}');
            }
            let n = lines.len();
            let mut effects = Vec::with_capacity(n);
            for (idx, ((line, effect), crlf)) in lines.into_iter().enumerate() {
                text.push_str(&line);
                if idx + 1 < n || trailing {
                    text.push_str(if crlf { "\r\n" } else { "\n" });
                }
                effects.push((line, effect));
            }
            (effects, text)
        })
}

/// Independent model: the last readable assignment made in the settings
/// section (or before any header) wins; otherwise the default.
fn model(lines: &[(String, Effect)]) -> Settings {
    let mut s = Settings::default();
    let mut in_section = true;
    for (_, effect) in lines {
        match *effect {
            Effect::Ignored => {}
            Effect::Section(ours) => in_section = ours,
            Effect::Set(i, v) if in_section => set(&mut s, i, v),
            Effect::Set(..) => {}
        }
    }
    s
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Property 26 (1): `parse_or_default(serialize(s)) == s`.
    #[test]
    fn round_trip(s in arb_settings()) {
        prop_assert_eq!(Settings::parse_or_default(&s.serialize()), s);
    }

    /// Property 26 (2a): parsing arbitrary bytes / text never panics.
    #[test]
    fn arbitrary_text_never_panics(
        bytes in prop::collection::vec(any::<u8>(), 0..256),
        text in "\\PC{0,64}",
        lines in prop::collection::vec(
            prop_oneof!["[a-z_=\\[\\]; \t\r-]{0,24}", "\\PC{0,16}", "[\\[a-zA-Z_=01]{0,12}"], 0..12),
    ) {
        let _ = Settings::parse_or_default(&String::from_utf8_lossy(&bytes));
        let _ = Settings::parse_or_default(&text);
        let _ = Settings::parse_or_default(&lines.join("\n"));
    }

    /// Property 26 (2b): a random mix of valid lines, unreadable values,
    /// unknown keys, comments and other sections parses exactly as the model
    /// predicts; options never set by a readable line keep their defaults.
    #[test]
    fn mixed_document_matches_model((lines, text) in arb_document()) {
        prop_assert_eq!(Settings::parse_or_default(&text), model(&lines), "text: {:?}", text);
    }

    /// Property 26 (3a): corrupting one option of a serialized file resets
    /// only that option to its default.
    #[test]
    fn corrupting_one_option_keeps_others(
        s in arb_settings(),
        i in 0..4usize,
        garbage in garbage_value(),
        kind in 0..4u8,
    ) {
        let text = s.serialize();
        let target = format!("{}={}", KEYS[i], get(&s, i));
        let replacement = match kind {
            0 => format!("{}={garbage}", KEYS[i]),
            1 => String::new(),
            2 => format!(";{target}"),
            _ => format!("[other]\n{target}\n[settings]"),
        };
        let corrupted = text.replace(&target, &replacement);
        let mut expected = s.clone();
        set(&mut expected, i, get(&Settings::default(), i));
        prop_assert_eq!(Settings::parse_or_default(&corrupted), expected, "text: {:?}", corrupted);
    }

    /// Property 26 (3b): in any generated document, making every assignment
    /// of one option unreadable leaves the other options unchanged.
    #[test]
    fn corrupting_one_option_in_any_document(
        (lines, _) in arb_document(),
        i in 0..4usize,
        garbage in garbage_value(),
    ) {
        let corrupted: Vec<String> = lines
            .iter()
            .map(|(t, e)| match e {
                Effect::Set(j, _) if *j == i => format!("{}={garbage}", KEYS[i]),
                _ => t.clone(),
            })
            .collect();
        let before = Settings::parse_or_default(&lines.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>().join("\n"));
        let after = Settings::parse_or_default(&corrupted.join("\n"));
        for j in (0..4).filter(|&j| j != i) {
            prop_assert_eq!(get(&after, j), get(&before, j), "option {}", KEYS[j]);
        }
        // The corrupted option itself is back to its default.
        prop_assert_eq!(get(&after, i), get(&Settings::default(), i));
    }
}
