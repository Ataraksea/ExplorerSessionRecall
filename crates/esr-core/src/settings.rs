//! `Settings` with INI-style `parse_or_default` / `serialize`.
//!
//! Task 1.3 defines the `Settings` value carried by `Event::SettingsChanged`
//! and `Command::PersistSettings`. Task 5.1 adds parsing and serialization
//! (Requirements 12.1-12.4).
//!
//! # File format
//!
//! The settings file (`%LOCALAPPDATA%\ExplorerSessionRecall\settings.ini`)
//! is UTF-8 with LF line endings. [`Settings::serialize`] writes the
//! canonical form:
//!
//! ```text
//! [settings]
//! restoration_enabled=true
//! start_at_sign_in=true
//! builtin_prompt_declined=false
//! tab_folding_enabled=true
//! ```
//!
//! [`Settings::parse_or_default`] is lenient:
//! - a leading BOM, CR before LF, surrounding whitespace and blank lines are
//!   ignored;
//! - lines starting with `;` or `#` are comments;
//! - keys are matched case-insensitively, and `-` is treated like `_`;
//! - keys before any section header or inside `[settings]` count; keys in
//!   other sections and unknown keys are ignored;
//! - accepted booleans (case-insensitive): `true`/`false`, `1`/`0`,
//!   `yes`/`no`, `on`/`off`;
//! - when a key appears more than once, the last readable value wins;
//! - a missing or unreadable value falls back to its own default without
//!   affecting the other options.

/// User-configurable options of the Session_Monitor.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Settings {
    /// "Restore my File Explorer session" (default on, Req 12.1).
    pub restoration_enabled: bool,
    /// "Start when I sign in" (default on, Req 12.2).
    pub start_at_sign_in: bool,
    /// The Builtin_Restore_Option prompt was declined (default off, Req 10.3).
    pub builtin_prompt_declined: bool,
    /// "Open folders as tabs in my File Explorer window" (default on, Req 12.10).
    pub tab_folding_enabled: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            restoration_enabled: true,
            start_at_sign_in: true,
            builtin_prompt_declined: false,
            tab_folding_enabled: true,
        }
    }
}

const SECTION: &str = "settings";
const KEY_RESTORATION_ENABLED: &str = "restoration_enabled";
const KEY_START_AT_SIGN_IN: &str = "start_at_sign_in";
const KEY_BUILTIN_PROMPT_DECLINED: &str = "builtin_prompt_declined";
const KEY_TAB_FOLDING_ENABLED: &str = "tab_folding_enabled";

impl Settings {
    /// Parses the settings file contents. Never fails: every option that is
    /// missing or unreadable keeps its default value independently of the
    /// others (Req 12.1, 12.2, 12.3).
    pub fn parse_or_default(text: &str) -> Settings {
        let mut settings = Settings::default();
        let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
        // Keys before any section header are accepted as well.
        let mut in_settings_section = true;

        for raw_line in text.split('\n') {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
                continue;
            }
            if let Some(rest) = line.strip_prefix('[') {
                in_settings_section = match rest.strip_suffix(']') {
                    Some(name) => name.trim().eq_ignore_ascii_case(SECTION),
                    // A malformed header: treat what follows as not ours.
                    None => false,
                };
                continue;
            }
            if !in_settings_section {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else { continue };
            let Some(value) = parse_bool(value.trim()) else { continue };
            let key = key.trim().to_ascii_lowercase().replace('-', "_");
            match key.as_str() {
                KEY_RESTORATION_ENABLED => settings.restoration_enabled = value,
                KEY_START_AT_SIGN_IN => settings.start_at_sign_in = value,
                KEY_BUILTIN_PROMPT_DECLINED => settings.builtin_prompt_declined = value,
                KEY_TAB_FOLDING_ENABLED => settings.tab_folding_enabled = value,
                _ => {}
            }
        }
        settings
    }

    /// Serializes to the canonical file form (UTF-8, LF, trailing newline).
    /// `parse_or_default(&s.serialize()) == s` for every `s` (Req 12.4).
    pub fn serialize(&self) -> String {
        format!(
            "[{SECTION}]\n{KEY_RESTORATION_ENABLED}={}\n{KEY_START_AT_SIGN_IN}={}\n{KEY_BUILTIN_PROMPT_DECLINED}={}\n{KEY_TAB_FOLDING_ENABLED}={}\n",
            self.restoration_enabled,
            self.start_at_sign_in,
            self.builtin_prompt_declined,
            self.tab_folding_enabled
        )
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    const TRUE: [&str; 4] = ["true", "1", "yes", "on"];
    const FALSE: [&str; 4] = ["false", "0", "no", "off"];
    if TRUE.iter().any(|t| value.eq_ignore_ascii_case(t)) {
        Some(true)
    } else if FALSE.iter().any(|f| value.eq_ignore_ascii_case(f)) {
        Some(false)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> impl Iterator<Item = Settings> {
        (0..16u8).map(|bits| Settings {
            restoration_enabled: bits & 1 != 0,
            start_at_sign_in: bits & 2 != 0,
            builtin_prompt_declined: bits & 4 != 0,
            tab_folding_enabled: bits & 8 != 0,
        })
    }

    #[test]
    fn serialize_is_canonical() {
        assert_eq!(
            Settings::default().serialize(),
            "[settings]\nrestoration_enabled=true\nstart_at_sign_in=true\nbuiltin_prompt_declined=false\ntab_folding_enabled=true\n"
        );
    }

    #[test]
    fn serialize_round_trips_every_combination() {
        for s in all() {
            assert_eq!(Settings::parse_or_default(&s.serialize()), s);
        }
    }

    #[test]
    fn empty_or_garbage_yields_defaults() {
        assert_eq!(Settings::parse_or_default(""), Settings::default());
        assert_eq!(Settings::parse_or_default("\u{0}\u{1}not ini at all\n==\n["), Settings::default());
    }

    #[test]
    fn unreadable_value_falls_back_individually() {
        let s = Settings::parse_or_default(
            "[settings]\nrestoration_enabled=maybe\nstart_at_sign_in=off\nbuiltin_prompt_declined=yes\ntab_folding_enabled=perhaps\n",
        );
        assert_eq!(
            s,
            Settings {
                restoration_enabled: true,
                start_at_sign_in: false,
                builtin_prompt_declined: true,
                tab_folding_enabled: true,
            }
        );
        let s = Settings::parse_or_default("tab_folding_enabled=off\n");
        assert_eq!(s, Settings { tab_folding_enabled: false, ..Settings::default() });
    }

    #[test]
    fn lenient_syntax_is_accepted() {
        let s = Settings::parse_or_default(
            "\u{FEFF}; comment\r\n# another\r\n  Restoration-Enabled =  NO \r\nSTART_AT_SIGN_IN=0\r\nunknown=1\r\n",
        );
        assert_eq!(
            s,
            Settings { restoration_enabled: false, start_at_sign_in: false, ..Settings::default() }
        );
    }

    #[test]
    fn all_boolean_spellings() {
        for (text, expected) in [
            ("true", true), ("1", true), ("yes", true), ("on", true), ("TRUE", true),
            ("false", false), ("0", false), ("no", false), ("off", false), ("Off", false),
        ] {
            let s = Settings::parse_or_default(&format!("builtin_prompt_declined={text}"));
            assert_eq!(s.builtin_prompt_declined, expected, "{text}");
        }
    }

    #[test]
    fn other_sections_are_ignored() {
        let s = Settings::parse_or_default(
            "[other]\nrestoration_enabled=false\n[Settings]\nstart_at_sign_in=false\n[x\nbuiltin_prompt_declined=true\n",
        );
        assert_eq!(
            s,
            Settings { start_at_sign_in: false, ..Settings::default() }
        );
    }

    #[test]
    fn last_readable_value_wins() {
        let s = Settings::parse_or_default("restoration_enabled=false\nrestoration_enabled=bogus\n");
        assert!(!s.restoration_enabled);
        let s = Settings::parse_or_default("restoration_enabled=false\nrestoration_enabled=on\n");
        assert!(s.restoration_enabled);
    }
}
