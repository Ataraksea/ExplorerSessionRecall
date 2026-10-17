//! Launch classification (`Default_Launch`, `Targeted_Launch`, `BuiltinRestore`)
//! and the LaunchTo value-to-location data table.
//!
//! Requirements 4.1, 6.1, 6.2, 10.4 (see design.md, "Trigger detection and
//! launch classification").
//!
//! # LaunchTo table
//!
//! The location Explorer opens for a given `Advanced\LaunchTo` value depends on
//! the OS build, so the mapping is data ([`LAUNCH_TO_TABLE`]) keyed by
//! `(value, build range)`. Spike R6 measured:
//!
//! | value              | 22621     | 26200 |
//! |--------------------|-----------|-------|
//! | absent, 0, 2, 5, other | Home  | Home  |
//! | 1                  | This PC   | Home  |
//! | 3                  | Downloads | Downloads |
//! | 4                  | Home      | Home  |
//!
//! Build 26100 was not measured. 26200 is an enablement package on the 26100
//! code base, so the This PC → Home change for value 1 is placed at 26100.
//!
//! Downloads is reported by Explorer as a file-system path (for example
//! `C:\Users\User\Downloads`), not as a shell GUID, so the shell resolves
//! `FOLDERID_Downloads` and passes it in [`LaunchToContext`]. OneDrive is
//! passed the same way in case a future build honours value 4.
//!
//! # Tolerant Default_Launch matching
//!
//! Classifying a Default_Launch as targeted adds a spurious extra tab to the
//! Trigger_Window. Classifying a targeted launch as default only loses the
//! requested folder as an extra tab, and the user can open it again. The
//! first error is worse, so [`is_default_launch`] accepts the initial Location
//! when it matches *any* location the value can open on *any* build
//! ([`default_targets`]), not just the one for the running build:
//!
//! - value 1: Home or This PC;
//! - value 4: Home or OneDrive ([`LAUNCH_TO_ALTERNATES`]);
//! - value 3 (Downloads) or an alternate (OneDrive) whose path the shell could
//!   not resolve: Home, which is where Explorer falls back.
//!
//! # Location comparison
//!
//! The comparison uses the canonical key of the Location_Identifier
//! (`kind tag + parsing_name`), with two relaxations that apply to launch
//! classification only. Session data comparison stays exact (Req 8.4).
//!
//! - **Kind tag.** The kind tag is not compared, except that `Search`
//!   locations never match. The target parsing names cannot be confused across
//!   kinds: a `::{GUID}` name is never a file-system path, and a Downloads or
//!   OneDrive folder is never a ZIP folder. Ignoring the tag keeps a
//!   misclassified kind in an adapter from turning a default launch into a
//!   targeted one.
//! - **Case.** Both sides are compared after simple per-code-unit uppercase
//!   folding (one-to-one BMP mappings only; surrogates are kept as they are).
//!   Windows file-system paths are case-insensitive, and the hexadecimal
//!   digits of a shell GUID name may be written in either case. A single
//!   trailing `\` is ignored, except on a drive root such as `C:\`.
//!
//! # BuiltinRestore
//!
//! [`is_builtin_restore`] is design.md trigger check step 3. It comes before
//! classification (step 7), so [`classify`] returns
//! [`LaunchKind::BuiltinRestore`] whenever the heuristic holds, whatever the
//! initial Location is.

use crate::engine::Millis;
use crate::model::{LocationId, LocationKind, WideString};

/// Parsing name of Home (`shell:::{F874310E-...}`).
pub const HOME_PARSING_NAME: &str = "::{F874310E-B6B7-47DC-BC84-B9E6B38F5903}";
/// Parsing name of This PC.
pub const THIS_PC_PARSING_NAME: &str = "::{20D04FE0-3AEA-1069-A2D8-08002B30309D}";

/// No user input may come later than this long after sign-in (Req 10.4).
pub const INPUT_GRACE_MS: u64 = 2_000;
/// The Trigger_Window must appear less than this long after the shell became ready.
pub const SHELL_READY_WINDOW_MS: u64 = 60_000;

// ---------------------------------------------------------------------------
// LaunchTo table
// ---------------------------------------------------------------------------

/// A location that a LaunchTo value can make Explorer open.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LaunchTarget {
    Home,
    ThisPc,
    /// `FOLDERID_Downloads`, reported as a file-system path.
    Downloads,
    /// `FOLDERID_SkyDrive`, reported as a file-system path.
    OneDrive,
}

/// One row of the LaunchTo table: `value` opens `opens` on builds
/// `min_build..=max_build`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LaunchToRow {
    pub value: u32,
    pub min_build: u32,
    pub max_build: u32,
    pub opens: LaunchTarget,
}

const fn row(value: u32, min_build: u32, max_build: u32, opens: LaunchTarget) -> LaunchToRow {
    LaunchToRow { value, min_build, max_build, opens }
}

/// First build on which LaunchTo = 1 opens Home instead of This PC.
pub const THIS_PC_TO_HOME_BUILD: u32 = 26_100;

/// Measured LaunchTo mapping (spike R6). Values without a row for the running
/// build, and an absent value, open Home.
pub const LAUNCH_TO_TABLE: &[LaunchToRow] = &[
    row(0, 0, u32::MAX, LaunchTarget::Home),
    row(1, 0, THIS_PC_TO_HOME_BUILD - 1, LaunchTarget::ThisPc),
    row(1, THIS_PC_TO_HOME_BUILD, u32::MAX, LaunchTarget::Home),
    row(2, 0, u32::MAX, LaunchTarget::Home),
    row(3, 0, u32::MAX, LaunchTarget::Downloads),
    row(4, 0, u32::MAX, LaunchTarget::Home),
    row(5, 0, u32::MAX, LaunchTarget::Home),
];

/// Locations a value may open on a build that was not measured. Accepted as a
/// Default_Launch but never the primary mapping.
pub const LAUNCH_TO_ALTERNATES: &[(u32, LaunchTarget)] = &[(4, LaunchTarget::OneDrive)];

/// What the shell knows about the running system for LaunchTo resolution.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LaunchToContext {
    /// OS build number (e.g. 22621, 26200).
    pub build: u32,
    /// Resolved `FOLDERID_Downloads` path, if available.
    pub downloads: Option<WideString>,
    /// Resolved OneDrive (`FOLDERID_SkyDrive`) path, if available.
    pub onedrive: Option<WideString>,
}

/// The location `value` opens on `build`, per [`LAUNCH_TO_TABLE`].
pub fn launch_to_target(value: Option<u32>, build: u32) -> LaunchTarget {
    value
        .and_then(|v| {
            LAUNCH_TO_TABLE
                .iter()
                .find(|r| r.value == v && (r.min_build..=r.max_build).contains(&build))
        })
        .map_or(LaunchTarget::Home, |r| r.opens)
}

/// Every location `value` can open on any build: all table rows for the value,
/// plus its alternates. Home for an absent or unknown value. No duplicates;
/// table order first.
pub fn default_targets(value: Option<u32>) -> Vec<LaunchTarget> {
    let mut out = Vec::new();
    if let Some(v) = value {
        let rows = LAUNCH_TO_TABLE.iter().filter(|r| r.value == v).map(|r| r.opens);
        let alts = LAUNCH_TO_ALTERNATES.iter().filter(|(a, _)| *a == v).map(|(_, t)| *t);
        for t in rows.chain(alts) {
            if !out.contains(&t) {
                out.push(t);
            }
        }
    }
    if out.is_empty() {
        out.push(LaunchTarget::Home);
    }
    out
}

impl LaunchTarget {
    /// The parsing name of this target, or `None` for a folder whose path the
    /// shell could not resolve.
    pub fn parsing_name(self, ctx: &LaunchToContext) -> Option<WideString> {
        match self {
            LaunchTarget::Home => Some(HOME_PARSING_NAME.into()),
            LaunchTarget::ThisPc => Some(THIS_PC_PARSING_NAME.into()),
            LaunchTarget::Downloads => ctx.downloads.clone(),
            LaunchTarget::OneDrive => ctx.onedrive.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

/// How the Trigger_Window was opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LaunchKind {
    /// Default_Launch: the initial Location is the LaunchTo location.
    Default,
    /// Targeted_Launch: the initial Location differs from it.
    Targeted,
    /// Explorer's own logon restore (Builtin_Restore_Option) opened the window.
    BuiltinRestore,
}

/// Inputs of the BuiltinRestore heuristic (Req 10.4, design trigger step 3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct BuiltinContext {
    /// `Advanced\PersistBrowsers`; `None` if absent or unreadable.
    pub persist_browsers: Option<bool>,
    /// The process was started with `--background` by the logon task.
    pub background_logon: bool,
    /// Sign-in time, if known.
    pub logon_at: Option<Millis>,
    /// Last user input (`GetLastInputInfo`); `None` means no input known.
    pub last_input_at: Option<Millis>,
    /// When the shell became ready, if known.
    pub shell_ready_at: Option<Millis>,
}

/// `true` when the window was most likely opened by Explorer's own logon
/// restore. All of these must hold:
///
/// - `PersistBrowsers` is on (absent or unreadable counts as off);
/// - the process was started with `--background` at logon;
/// - no user input since sign-in + 2 s: `last_input_at` is `None` or at most
///   `logon_at + 2000`. An unknown `logon_at` fails this check;
/// - the shell became ready (known) less than 60 s before `now`.
pub fn is_builtin_restore(ctx: &BuiltinContext, now: Millis) -> bool {
    let no_input = match (ctx.logon_at, ctx.last_input_at) {
        (None, _) => false,
        (Some(_), None) => true,
        (Some(logon), Some(input)) => input.0 <= logon.0.saturating_add(INPUT_GRACE_MS),
    };
    let shell_recent = ctx.shell_ready_at.is_some_and(|r| now.since(r) < SHELL_READY_WINDOW_MS);
    ctx.persist_browsers == Some(true) && ctx.background_logon && no_input && shell_recent
}

/// `true` when `initial` equals a location that `launch_to` can open (see the
/// module docs for the tolerant comparison).
pub fn is_default_launch(initial: &LocationId, launch_to: Option<u32>, ctx: &LaunchToContext) -> bool {
    if initial.kind == LocationKind::Search {
        return false;
    }
    default_targets(launch_to).into_iter().any(|t| {
        // An unresolved folder falls back to Home, as Explorer does.
        let name = t.parsing_name(ctx).unwrap_or_else(|| HOME_PARSING_NAME.into());
        names_match(initial.parsing_name.units(), name.units())
    })
}

/// Classifies a Trigger_Window. BuiltinRestore takes precedence (design trigger
/// step 3 runs before classification in step 7).
pub fn classify(
    initial: &LocationId,
    launch_to: Option<u32>,
    launch_ctx: &LaunchToContext,
    builtin: &BuiltinContext,
    now: Millis,
) -> LaunchKind {
    if is_builtin_restore(builtin, now) {
        LaunchKind::BuiltinRestore
    } else if is_default_launch(initial, launch_to, launch_ctx) {
        LaunchKind::Default
    } else {
        LaunchKind::Targeted
    }
}

/// Case-insensitive parsing-name equality, ignoring one trailing `\` (not on a
/// drive root).
fn names_match(a: &[u16], b: &[u16]) -> bool {
    let (a, b) = (trim_separator(a), trim_separator(b));
    a.len() == b.len() && a.iter().zip(b).all(|(&x, &y)| fold_unit(x) == fold_unit(y))
}

fn trim_separator(s: &[u16]) -> &[u16] {
    match s.split_last() {
        Some((&last, rest)) if last == u16::from(b'\\') && s.len() > 3 => rest,
        _ => s,
    }
}

/// Simple uppercase of one UTF-16 code unit: one-to-one BMP mappings only.
fn fold_unit(u: u16) -> u16 {
    let Some(c) = char::from_u32(u32::from(u)) else {
        return u; // surrogate
    };
    let mut up = c.to_uppercase();
    match (up.next(), up.next()) {
        (Some(x), None) => u16::try_from(u32::from(x)).unwrap_or(u),
        _ => u,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{SearchSpec, ShellKind, Volume};

    const DOWNLOADS: &str = r"C:\Users\User\Downloads";
    const ONEDRIVE: &str = r"C:\Users\User\OneDrive";

    fn shell(kind: ShellKind, name: &str) -> LocationId {
        LocationId {
            case_sensitive: false,
            kind: LocationKind::Shell(kind),
            parsing_name: name.into(),
            display_name: name.into(),
            pidl: vec![],
            search: None,
        }
    }

    fn fs(path: &str) -> LocationId {
        LocationId {
            case_sensitive: false,
            kind: LocationKind::FileSystem(Volume::Fixed),
            parsing_name: path.into(),
            display_name: path.into(),
            pidl: vec![],
            search: None,
        }
    }

    fn home() -> LocationId {
        shell(ShellKind::Home, HOME_PARSING_NAME)
    }

    fn this_pc() -> LocationId {
        shell(ShellKind::ThisPc, THIS_PC_PARSING_NAME)
    }

    fn ctx(build: u32) -> LaunchToContext {
        LaunchToContext {
            build,
            downloads: Some(DOWNLOADS.into()),
            onedrive: Some(ONEDRIVE.into()),
        }
    }

    fn kind(initial: &LocationId, value: Option<u32>, c: &LaunchToContext) -> LaunchKind {
        classify(initial, value, c, &BuiltinContext::default(), Millis(0))
    }

    #[test]
    fn table_per_build() {
        use LaunchTarget::*;
        for build in [22621, 26200] {
            for v in [None, Some(0), Some(2), Some(5), Some(6), Some(99), Some(u32::MAX)] {
                assert_eq!(launch_to_target(v, build), Home, "{v:?} on {build}");
            }
            assert_eq!(launch_to_target(Some(3), build), Downloads);
            assert_eq!(launch_to_target(Some(4), build), Home);
        }
        assert_eq!(launch_to_target(Some(1), 22621), ThisPc);
        assert_eq!(launch_to_target(Some(1), 26099), ThisPc);
        assert_eq!(launch_to_target(Some(1), 26100), Home);
        assert_eq!(launch_to_target(Some(1), 26200), Home);
    }

    #[test]
    fn candidates_span_builds() {
        use LaunchTarget::*;
        assert_eq!(default_targets(None), vec![Home]);
        assert_eq!(default_targets(Some(7)), vec![Home]);
        assert_eq!(default_targets(Some(1)), vec![ThisPc, Home]);
        assert_eq!(default_targets(Some(3)), vec![Downloads]);
        assert_eq!(default_targets(Some(4)), vec![Home, OneDrive]);
    }

    #[test]
    fn home_values_are_default() {
        for build in [22621, 26200] {
            for v in [None, Some(0), Some(2), Some(5), Some(42)] {
                assert_eq!(kind(&home(), v, &ctx(build)), LaunchKind::Default, "{v:?}");
                assert_eq!(kind(&this_pc(), v, &ctx(build)), LaunchKind::Targeted, "{v:?}");
            }
        }
    }

    #[test]
    fn value_1_accepts_home_or_this_pc_on_every_build() {
        for build in [22621, 26200] {
            assert_eq!(kind(&this_pc(), Some(1), &ctx(build)), LaunchKind::Default);
            assert_eq!(kind(&home(), Some(1), &ctx(build)), LaunchKind::Default);
            assert_eq!(kind(&fs(DOWNLOADS), Some(1), &ctx(build)), LaunchKind::Targeted);
        }
    }

    #[test]
    fn value_4_accepts_home_or_onedrive() {
        let c = ctx(22621);
        assert_eq!(kind(&home(), Some(4), &c), LaunchKind::Default);
        assert_eq!(kind(&fs(ONEDRIVE), Some(4), &c), LaunchKind::Default);
        assert_eq!(kind(&this_pc(), Some(4), &c), LaunchKind::Targeted);
    }

    #[test]
    fn downloads_matches_resolved_path_case_insensitively() {
        let c = ctx(26200);
        for p in [DOWNLOADS, r"c:\users\user\downloads", r"C:\USERS\USER\DOWNLOADS\"] {
            assert_eq!(kind(&fs(p), Some(3), &c), LaunchKind::Default, "{p}");
        }
        assert_eq!(kind(&home(), Some(3), &c), LaunchKind::Targeted);
        assert_eq!(kind(&fs(r"C:\Users\User\Downloads\sub"), Some(3), &c), LaunchKind::Targeted);
        assert_eq!(kind(&fs(r"C:\Users\User\Download"), Some(3), &c), LaunchKind::Targeted);

        // Non-ASCII user name, different case.
        let c = LaunchToContext { downloads: Some(r"C:\Users\Émile\Downloads".into()), ..ctx(22621) };
        assert_eq!(kind(&fs(r"c:\users\émile\downloads"), Some(3), &c), LaunchKind::Default);
    }

    #[test]
    fn unresolved_folder_falls_back_to_home() {
        let c = LaunchToContext { build: 22621, downloads: None, onedrive: None };
        assert_eq!(kind(&home(), Some(3), &c), LaunchKind::Default);
        assert_eq!(kind(&fs(DOWNLOADS), Some(3), &c), LaunchKind::Targeted);
        assert_eq!(kind(&home(), Some(4), &c), LaunchKind::Default);
    }

    #[test]
    fn guid_case_and_kind_tag_are_tolerated() {
        let lower = shell(ShellKind::Other, &HOME_PARSING_NAME.to_lowercase());
        assert_eq!(kind(&lower, None, &ctx(22621)), LaunchKind::Default);
        // Downloads reported with a mapped-volume tag still matches.
        let mut d = fs(DOWNLOADS);
        d.kind = LocationKind::FileSystem(Volume::Mapped);
        assert_eq!(kind(&d, Some(3), &ctx(22621)), LaunchKind::Default);
        // A search location never matches.
        let s = LocationId {
            case_sensitive: false,
            kind: LocationKind::Search,
            parsing_name: HOME_PARSING_NAME.into(),
            display_name: "Search".into(),
            pidl: vec![],
            search: Some(SearchSpec { query: "q".into(), scope: Box::new(home()) }),
        };
        assert_eq!(kind(&s, None, &ctx(22621)), LaunchKind::Targeted);
    }

    #[test]
    fn targeted_folder() {
        for v in [None, Some(1), Some(3), Some(4)] {
            assert_eq!(kind(&fs(r"D:\Projects"), v, &ctx(22621)), LaunchKind::Targeted);
        }
        // Drive root keeps its separator.
        assert_eq!(kind(&fs(r"C:\"), None, &ctx(22621)), LaunchKind::Targeted);
    }

    fn builtin() -> BuiltinContext {
        BuiltinContext {
            persist_browsers: Some(true),
            background_logon: true,
            logon_at: Some(Millis(10_000)),
            last_input_at: Some(Millis(11_000)),
            shell_ready_at: Some(Millis(20_000)),
        }
    }

    #[test]
    fn builtin_restore_positive() {
        assert!(is_builtin_restore(&builtin(), Millis(30_000)));
        assert!(is_builtin_restore(&BuiltinContext { last_input_at: None, ..builtin() }, Millis(30_000)));
        // Boundaries: input exactly at logon + 2 s, 59,999 ms after shell ready.
        let b = BuiltinContext { last_input_at: Some(Millis(12_000)), ..builtin() };
        assert!(is_builtin_restore(&b, Millis(79_999)));
    }

    #[test]
    fn builtin_restore_negative_for_each_condition() {
        let now = Millis(30_000);
        let cases = [
            BuiltinContext { persist_browsers: Some(false), ..builtin() },
            BuiltinContext { persist_browsers: None, ..builtin() },
            BuiltinContext { background_logon: false, ..builtin() },
            BuiltinContext { last_input_at: Some(Millis(12_001)), ..builtin() },
            BuiltinContext { logon_at: None, ..builtin() },
            BuiltinContext { shell_ready_at: None, ..builtin() },
        ];
        for c in cases {
            assert!(!is_builtin_restore(&c, now), "{c:?}");
        }
        assert!(!is_builtin_restore(&builtin(), Millis(80_000)));
    }

    #[test]
    fn builtin_restore_takes_precedence() {
        let c = ctx(22621);
        let now = Millis(30_000);
        assert_eq!(classify(&fs(r"D:\x"), None, &c, &builtin(), now), LaunchKind::BuiltinRestore);
        assert_eq!(classify(&home(), None, &c, &builtin(), now), LaunchKind::BuiltinRestore);
        let off = BuiltinContext { background_logon: false, ..builtin() };
        assert_eq!(classify(&home(), None, &c, &off, now), LaunchKind::Default);
    }
}
