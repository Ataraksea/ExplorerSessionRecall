//! Unit tests for `Settings` defaults (tasks 5.3 and 18.3, Requirements 12.1,
//! 12.2, 12.10).
//!
//! Complements the unit tests inside `settings.rs`: these cover empty,
//! garbage, and partially valid settings files, checking that every option
//! that is missing or unreadable keeps its default independently of the
//! others.

use std::time::{Duration, Instant};

use esr_core::settings::Settings;

/// Every option set to the opposite of its default.
fn non_default() -> Settings {
    Settings {
        restoration_enabled: false,
        start_at_sign_in: false,
        builtin_prompt_declined: true,
        tab_folding_enabled: false,
    }
}

const KEYS: [&str; 4] = [
    "restoration_enabled",
    "start_at_sign_in",
    "builtin_prompt_declined",
    "tab_folding_enabled",
];

/// The non-default value text for each key in `KEYS`.
const NON_DEFAULT_VALUES: [&str; 4] = ["false", "false", "true", "false"];

/// Returns `non_default()` with option `index` (into `KEYS`) reset to its default.
fn non_default_except(index: usize) -> Settings {
    let mut s = non_default();
    let d = Settings::default();
    match index {
        0 => s.restoration_enabled = d.restoration_enabled,
        1 => s.start_at_sign_in = d.start_at_sign_in,
        2 => s.builtin_prompt_declined = d.builtin_prompt_declined,
        3 => s.tab_folding_enabled = d.tab_folding_enabled,
        _ => unreachable!(),
    }
    s
}

/// Deterministic pseudo-random bytes (LCG) so the garbage test is stable.
fn garbage_bytes(len: usize, mut seed: u64) -> Vec<u8> {
    (0..len)
        .map(|_| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 56) as u8
        })
        .collect()
}

#[test]
fn default_matches_requirements() {
    let d = Settings::default();
    assert!(d.restoration_enabled, "Req 12.1: restoration enabled by default");
    assert!(d.start_at_sign_in, "Req 12.2: start-at-sign-in enabled by default");
    assert!(!d.builtin_prompt_declined, "prompt not declined by default");
    assert!(d.tab_folding_enabled, "Req 12.10: tab folding enabled by default");
}

#[test]
fn empty_and_whitespace_only_yield_defaults() {
    for text in ["", " ", "\n", "\r\n", "   \n\t\r\n  \n", "\t\t\t"] {
        assert_eq!(Settings::parse_or_default(text), Settings::default(), "{text:?}");
    }
}

#[test]
fn bom_only_yields_defaults() {
    assert_eq!(Settings::parse_or_default("\u{FEFF}"), Settings::default());
    assert_eq!(Settings::parse_or_default("\u{FEFF}\r\n"), Settings::default());
    // The same BOM arriving as raw bytes through a lossy UTF-8 decode.
    let lossy = String::from_utf8_lossy(&[0xEF, 0xBB, 0xBF]);
    assert_eq!(Settings::parse_or_default(&lossy), Settings::default());
}

#[test]
fn binary_garbage_yields_defaults() {
    for seed in 0..16u64 {
        let bytes = garbage_bytes(4096, seed);
        let text = String::from_utf8_lossy(&bytes);
        assert_eq!(Settings::parse_or_default(&text), Settings::default(), "seed {seed}");
    }
    // Invalid UTF-8 sequences decoded to U+FFFD.
    let text = String::from_utf8_lossy(&[0xFF, 0xFE, 0x00, 0xC3, 0x28, b'\n', 0x80, b'=', 0xFF]);
    assert_eq!(Settings::parse_or_default(&text), Settings::default());
}

#[test]
fn only_unknown_keys_yield_defaults() {
    let text = "[settings]\nfoo=false\nrestore=0\nstart_at_signin=off\nprompt_declined=yes\nrestoration_enabled_x=false\n";
    assert_eq!(Settings::parse_or_default(text), Settings::default());
    // Unknown keys outside any section as well.
    assert_eq!(Settings::parse_or_default("a=1\nb=0\n"), Settings::default());
}

#[test]
fn header_without_keys_yields_defaults() {
    for text in ["[settings]", "[settings]\n", "\u{FEFF}[settings]\r\n\r\n", "[Settings]\n; nothing here\n"] {
        assert_eq!(Settings::parse_or_default(text), Settings::default(), "{text:?}");
    }
}

#[test]
fn each_missing_option_defaults_alone() {
    for (missing, missing_key) in KEYS.iter().enumerate() {
        let mut text = String::from("[settings]\n");
        for (i, (key, value)) in KEYS.iter().zip(NON_DEFAULT_VALUES).enumerate() {
            if i != missing {
                text.push_str(&format!("{key}={value}\n"));
            }
        }
        assert_eq!(Settings::parse_or_default(&text), non_default_except(missing), "missing {missing_key}");
    }
}

#[test]
fn each_invalid_value_defaults_alone() {
    let invalid = ["", " ", "maybe", "2", "-1", "truee", "t", "f", "y", "n", "yes!", "\"true\"", "nope", "\u{FFFD}", "of"];
    for (bad_index, bad_key) in KEYS.iter().enumerate() {
        for bad in invalid {
            let mut text = String::from("[settings]\n");
            for (i, (key, value)) in KEYS.iter().zip(NON_DEFAULT_VALUES).enumerate() {
                let value = if i == bad_index { bad } else { value };
                text.push_str(&format!("{key}={value}\n"));
            }
            assert_eq!(
                Settings::parse_or_default(&text),
                non_default_except(bad_index),
                "{bad_key}={bad:?}"
            );
        }
    }
}

#[test]
fn truncated_file_keeps_only_complete_lines() {
    let full = non_default().serialize();
    for cut in 0..=full.len() {
        let prefix = &full[..cut];
        let mut expected = Settings::default();
        // An option takes its stored value only if its full `key=value` text
        // survived the cut; a partial key or partial value is unreadable.
        for (i, (key, value)) in KEYS.iter().zip(NON_DEFAULT_VALUES).enumerate() {
            if prefix.contains(&format!("{key}={value}")) {
                match i {
                    0 => expected.restoration_enabled = false,
                    1 => expected.start_at_sign_in = false,
                    2 => expected.builtin_prompt_declined = true,
                    _ => expected.tab_folding_enabled = false,
                }
            }
        }
        assert_eq!(Settings::parse_or_default(prefix), expected, "cut at {cut}: {prefix:?}");
    }
}

#[test]
fn truncated_header_ignores_following_keys() {
    // A header cut before `]` is malformed; keys after it are not ours.
    assert_eq!(Settings::parse_or_default("[sett\nrestoration_enabled=false\n"), Settings::default());
}

#[test]
fn trailing_garbage_after_value_is_unreadable() {
    for value in ["true x", "false x", "0 1", "off;comment", "no#", "yes yes"] {
        let text = format!("[settings]\nrestoration_enabled=false\nstart_at_sign_in={value}\nbuiltin_prompt_declined=true\ntab_folding_enabled=false\n");
        assert_eq!(Settings::parse_or_default(&text), non_default_except(1), "{value:?}");
    }
    // `true x` on a key whose default is `true` must not flip it to anything else.
    let s = Settings::parse_or_default("builtin_prompt_declined=true x\n");
    assert_eq!(s, Settings::default());
}

#[test]
fn duplicate_section_headers_merge() {
    let text = "[settings]\nrestoration_enabled=false\n[settings]\nstart_at_sign_in=false\n[SETTINGS]\nbuiltin_prompt_declined=true\ntab_folding_enabled=false\n";
    assert_eq!(Settings::parse_or_default(text), non_default());

    // Re-entering the section after another section still counts.
    let text = "[settings]\nrestoration_enabled=false\n[other]\nstart_at_sign_in=false\n[settings]\nbuiltin_prompt_declined=true\ntab_folding_enabled=false\n";
    assert_eq!(Settings::parse_or_default(text), non_default_except(1));

    // A later duplicate section overrides the earlier value.
    let text = "[settings]\nrestoration_enabled=false\n[settings]\nrestoration_enabled=true\n";
    assert_eq!(Settings::parse_or_default(text), Settings::default());
}

#[test]
fn only_tab_folding_false_leaves_other_options_at_defaults() {
    // Req 12.10: the folding option is read independently of the others.
    let expected = Settings { tab_folding_enabled: false, ..Settings::default() };
    for text in [
        "tab_folding_enabled=false",
        "tab_folding_enabled=false\n",
        "[settings]\ntab_folding_enabled=false\n",
        "\u{FEFF}[Settings]\r\nTab-Folding-Enabled = OFF\r\n",
        "[settings]\ntab_folding_enabled=0\nunknown=false\n",
    ] {
        let s = Settings::parse_or_default(text);
        assert_eq!(s, expected, "{text:?}");
        assert!(s.restoration_enabled, "Req 12.1: {text:?}");
        assert!(s.start_at_sign_in, "Req 12.2: {text:?}");
        assert!(!s.builtin_prompt_declined, "{text:?}");
    }
}

#[test]
fn tab_folding_defaults_to_true_when_missing_or_unreadable() {
    // A settings file written before the option existed (three keys, all
    // non-default) keeps folding enabled.
    let legacy = "[settings]\nrestoration_enabled=false\nstart_at_sign_in=false\nbuiltin_prompt_declined=true\n";
    let s = Settings::parse_or_default(legacy);
    assert_eq!(s, Settings { tab_folding_enabled: true, ..non_default() });

    // Unreadable values, empty value, and a key outside the settings section.
    for text in [
        "[settings]\ntab_folding_enabled=maybe\n",
        "[settings]\ntab_folding_enabled=\n",
        "[settings]\ntab_folding_enabled\n",
        "[other]\ntab_folding_enabled=false\n",
        "[settings]\ntab_folding_enabled=false x\n",
    ] {
        assert!(Settings::parse_or_default(text).tab_folding_enabled, "{text:?}");
    }
}

#[test]
fn partially_valid_file_keeps_valid_values_and_defaults_the_rest() {
    let s = Settings::parse_or_default(
        "[settings]\nrestoration_enabled=nonsense\nstart_at_sign_in=false\ntab_folding_enabled=false\n???\n",
    );
    assert_eq!(
        s,
        Settings {
            restoration_enabled: true,
            start_at_sign_in: false,
            builtin_prompt_declined: false,
            tab_folding_enabled: false,
        }
    );

    // An unreadable later value does not discard an earlier valid one.
    let s = Settings::parse_or_default("tab_folding_enabled=false\ntab_folding_enabled=bogus\n");
    assert_eq!(s, Settings { tab_folding_enabled: false, ..Settings::default() });
}

#[test]
fn very_long_file_parses_quickly() {
    let mut text = String::from("[settings]\n");
    for i in 0..10_000 {
        match i % 4 {
            0 => text.push_str(&format!("unknown_key_{i}=false\n")),
            1 => text.push_str("; a comment line that is reasonably long to add some bulk to the file\n"),
            2 => text.push_str("restoration_enabled=not-a-bool\n"),
            _ => text.push_str("garbage without an equals sign\n"),
        }
    }
    text.push_str("start_at_sign_in=false\n");

    let started = Instant::now();
    let s = Settings::parse_or_default(&text);
    let elapsed = started.elapsed();

    assert_eq!(s, Settings { start_at_sign_in: false, ..Settings::default() });
    assert!(elapsed < Duration::from_secs(1), "took {elapsed:?}");
}
