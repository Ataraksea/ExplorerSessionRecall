//! Table-driven unit tests for launch classification (task 9.2,
//! Requirements 6.1, 6.2, 10.4).
//!
//! Complements the tests inside `launch.rs`. The expected kind of every
//! combination is derived independently from the rules in the module docs of
//! `launch.rs` (see [`expected`]):
//!
//! - absent, 0, 2, 5 and unknown values accept Home only;
//! - value 1 accepts Home or This PC on every build;
//! - value 3 accepts Downloads, or Home only when the Downloads path is
//!   unresolved;
//! - value 4 accepts Home, or OneDrive when its path is resolved;
//! - Search locations never match;
//! - BuiltinRestore takes precedence over both.

use esr_core::engine::Millis;
use esr_core::launch::{
    classify, default_targets, is_builtin_restore, is_default_launch, launch_to_target, BuiltinContext,
    LaunchKind, LaunchTarget, LaunchToContext, HOME_PARSING_NAME, THIS_PC_PARSING_NAME,
};
use esr_core::model::{LocationId, LocationKind, SearchSpec, ShellKind, Volume};

const DOWNLOADS: &str = r"C:\Users\User\Downloads";
const ONEDRIVE: &str = r"C:\Users\User\OneDrive";
const RECYCLE_BIN: &str = "::{645FF040-5081-101B-9F08-00AA002F954E}";

/// Builds of interest. LaunchToContext carries no UBR, so 22621 stands for
/// every 22621.x; 26099/26100 straddle the value-1 change.
const BUILDS: [u32; 5] = [22_621, 22_631, 26_099, 26_100, 26_200];

/// Every measured value, plus absent and unknown values.
const VALUES: [Option<u32>; 10] =
    [None, Some(0), Some(1), Some(2), Some(3), Some(4), Some(5), Some(6), Some(255), Some(u32::MAX)];

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn loc(kind: LocationKind, name: &str) -> LocationId {
    LocationId { case_sensitive: false, kind, parsing_name: name.into(), display_name: name.into(), pidl: vec![], search: None }
}

fn fs(path: &str) -> LocationId {
    loc(LocationKind::FileSystem(Volume::Fixed), path)
}

fn home() -> LocationId {
    loc(LocationKind::Shell(ShellKind::Home), HOME_PARSING_NAME)
}

/// The initial Locations under test, with what they are for [`expected`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Initial {
    Home,
    ThisPc,
    DownloadsExact,
    DownloadsOtherCase,
    DownloadsTrailingSep,
    OneDrive,
    RandomFolder,
    DriveRoot,
    Unc,
    Search,
    Zip,
    RecycleBin,
}

const INITIALS: [Initial; 12] = [
    Initial::Home,
    Initial::ThisPc,
    Initial::DownloadsExact,
    Initial::DownloadsOtherCase,
    Initial::DownloadsTrailingSep,
    Initial::OneDrive,
    Initial::RandomFolder,
    Initial::DriveRoot,
    Initial::Unc,
    Initial::Search,
    Initial::Zip,
    Initial::RecycleBin,
];

impl Initial {
    fn location(self) -> LocationId {
        match self {
            Initial::Home => home(),
            Initial::ThisPc => loc(LocationKind::Shell(ShellKind::ThisPc), THIS_PC_PARSING_NAME),
            Initial::DownloadsExact => fs(DOWNLOADS),
            Initial::DownloadsOtherCase => fs(r"c:\USERS\user\dOWNLOADS"),
            Initial::DownloadsTrailingSep => fs(r"C:\Users\User\Downloads\"),
            Initial::OneDrive => fs(ONEDRIVE),
            Initial::RandomFolder => fs(r"D:\Projects"),
            Initial::DriveRoot => fs(r"C:\"),
            Initial::Unc => loc(LocationKind::FileSystem(Volume::Unc), r"\\server\share\folder"),
            // A search scoped to Home whose parsing name is Home's: still never
            // a Default_Launch.
            Initial::Search => LocationId {
                case_sensitive: false,
                kind: LocationKind::Search,
                parsing_name: HOME_PARSING_NAME.into(),
                display_name: "Search Results in Home".into(),
                pidl: vec![],
                search: Some(SearchSpec { query: "report".into(), scope: Box::new(home()) }),
            },
            Initial::Zip => loc(LocationKind::ZipFolder, r"C:\Users\User\Downloads\archive.zip\docs"),
            Initial::RecycleBin => loc(LocationKind::Shell(ShellKind::RecycleBin), RECYCLE_BIN),
        }
    }

    fn is_downloads(self) -> bool {
        matches!(self, Initial::DownloadsExact | Initial::DownloadsOtherCase | Initial::DownloadsTrailingSep)
    }
}

fn ctx(build: u32, downloads: bool, onedrive: bool) -> LaunchToContext {
    LaunchToContext {
        build,
        downloads: downloads.then(|| DOWNLOADS.into()),
        onedrive: onedrive.then(|| ONEDRIVE.into()),
    }
}

/// Expected classification without BuiltinRestore, from the module-doc rules.
/// Independent of the build by design (tolerant matching).
fn expected(initial: Initial, value: Option<u32>, downloads: bool, onedrive: bool) -> LaunchKind {
    use Initial as I;
    let default = match value {
        Some(1) => matches!(initial, I::Home | I::ThisPc),
        Some(3) => (downloads && initial.is_downloads()) || (!downloads && initial == I::Home),
        Some(4) => initial == I::Home || (onedrive && initial == I::OneDrive),
        // Absent, 0, 2, 5 and unknown values.
        _ => initial == I::Home,
    };
    if default {
        LaunchKind::Default
    } else {
        LaunchKind::Targeted
    }
}

fn no_builtin() -> BuiltinContext {
    BuiltinContext::default()
}

/// A context for which the BuiltinRestore heuristic holds at [`NOW`].
fn builtin() -> BuiltinContext {
    BuiltinContext {
        persist_browsers: Some(true),
        background_logon: true,
        logon_at: Some(Millis(10_000)),
        last_input_at: Some(Millis(11_500)),
        shell_ready_at: Some(Millis(25_000)),
    }
}

const NOW: Millis = Millis(40_000);

// ---------------------------------------------------------------------------
// LaunchTo table
// ---------------------------------------------------------------------------

#[test]
fn launch_to_target_for_every_value_and_build() {
    use LaunchTarget::*;
    for build in BUILDS {
        for value in VALUES {
            let want = match value {
                Some(1) if build < 26_100 => ThisPc,
                Some(3) => Downloads,
                _ => Home,
            };
            assert_eq!(launch_to_target(value, build), want, "value {value:?} on build {build}");
        }
    }
}

#[test]
fn default_targets_are_build_independent_and_cover_every_value() {
    use LaunchTarget::*;
    for value in VALUES {
        let want = match value {
            Some(1) => vec![ThisPc, Home],
            Some(3) => vec![Downloads],
            Some(4) => vec![Home, OneDrive],
            _ => vec![Home],
        };
        assert_eq!(default_targets(value), want, "value {value:?}");
        // The running build's target is always among the accepted ones.
        for build in BUILDS {
            assert!(default_targets(value).contains(&launch_to_target(value, build)), "{value:?} on {build}");
        }
    }
}

// ---------------------------------------------------------------------------
// Default / Targeted
// ---------------------------------------------------------------------------

#[test]
fn every_value_build_and_initial_location_with_resolved_folders() {
    for build in BUILDS {
        let c = ctx(build, true, true);
        for value in VALUES {
            for initial in INITIALS {
                let want = expected(initial, value, true, true);
                let got = classify(&initial.location(), value, &c, &no_builtin(), NOW);
                assert_eq!(got, want, "{initial:?}, value {value:?}, build {build}");
                assert_eq!(
                    is_default_launch(&initial.location(), value, &c),
                    want == LaunchKind::Default,
                    "is_default_launch: {initial:?}, value {value:?}, build {build}"
                );
            }
        }
    }
}

#[test]
fn every_value_build_and_initial_location_with_unresolved_folders() {
    for (downloads, onedrive) in [(false, false), (false, true), (true, false)] {
        for build in BUILDS {
            let c = ctx(build, downloads, onedrive);
            for value in VALUES {
                for initial in INITIALS {
                    let want = expected(initial, value, downloads, onedrive);
                    let got = classify(&initial.location(), value, &c, &no_builtin(), NOW);
                    assert_eq!(
                        got, want,
                        "{initial:?}, value {value:?}, build {build}, downloads {downloads}, onedrive {onedrive}"
                    );
                }
            }
        }
    }
}

#[test]
fn value_3_accepts_home_only_when_downloads_unresolved() {
    for build in BUILDS {
        assert_eq!(classify(&home(), Some(3), &ctx(build, true, true), &no_builtin(), NOW), LaunchKind::Targeted);
        assert_eq!(classify(&home(), Some(3), &ctx(build, false, true), &no_builtin(), NOW), LaunchKind::Default);
        assert_eq!(classify(&fs(DOWNLOADS), Some(3), &ctx(build, false, true), &no_builtin(), NOW), LaunchKind::Targeted);
    }
}

#[test]
fn drive_root_keeps_its_separator() {
    // `C:\` must not be trimmed to `C:` and match a Downloads path resolved to `C:`.
    let c = LaunchToContext { build: 22_621, downloads: Some("C:".into()), onedrive: None };
    assert_eq!(classify(&fs(r"C:\"), Some(3), &c, &no_builtin(), NOW), LaunchKind::Targeted);
    let c = LaunchToContext { downloads: Some(r"C:\".into()), ..c };
    assert_eq!(classify(&fs(r"C:\"), Some(3), &c, &no_builtin(), NOW), LaunchKind::Default);
    assert_eq!(classify(&fs(r"c:\"), Some(3), &c, &no_builtin(), NOW), LaunchKind::Default);
}

// ---------------------------------------------------------------------------
// Requirement 6.1 / 6.2 meaning
// ---------------------------------------------------------------------------

#[test]
fn req_6_1_plain_explorer_start_is_default_launch() {
    // Win+E or the taskbar icon with LaunchTo absent opens Home.
    for build in BUILDS {
        let c = ctx(build, true, true);
        assert_eq!(classify(&home(), None, &c, &no_builtin(), NOW), LaunchKind::Default, "build {build}");
    }
}

#[test]
fn req_6_2_show_in_folder_is_targeted_launch() {
    // "Show in folder" from a browser or app opens the file's parent folder.
    let requested = [
        fs(r"C:\Users\User\Documents\Reports"),
        fs(r"C:\Users\User\Downloads\installers"), // below Downloads, not Downloads itself
        loc(LocationKind::FileSystem(Volume::Mapped), r"Z:\Shared\Specs"),
        loc(LocationKind::FileSystem(Volume::Unc), r"\\nas\media\photos"),
    ];
    for build in BUILDS {
        let c = ctx(build, true, true);
        for value in VALUES {
            for r in &requested {
                assert_eq!(classify(r, value, &c, &no_builtin(), NOW), LaunchKind::Targeted, "{r:?}, {value:?}, {build}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// BuiltinRestore (Req 10.4)
// ---------------------------------------------------------------------------

#[test]
fn builtin_restore_baseline_holds() {
    assert!(is_builtin_restore(&builtin(), NOW));
    assert!(is_builtin_restore(&BuiltinContext { last_input_at: None, ..builtin() }, NOW));
    // Input before sign-in (e.g. on the lock screen) counts as no input since.
    assert!(is_builtin_restore(&BuiltinContext { last_input_at: Some(Millis(5_000)), ..builtin() }, NOW));
}

#[test]
fn builtin_restore_each_condition_failing_alone() {
    let cases: [(&str, BuiltinContext, Millis); 7] = [
        ("PersistBrowsers off", BuiltinContext { persist_browsers: Some(false), ..builtin() }, NOW),
        ("PersistBrowsers absent", BuiltinContext { persist_browsers: None, ..builtin() }, NOW),
        ("not a background logon start", BuiltinContext { background_logon: false, ..builtin() }, NOW),
        ("input after logon + 2 s", BuiltinContext { last_input_at: Some(Millis(30_000)), ..builtin() }, NOW),
        ("shell ready unknown", BuiltinContext { shell_ready_at: None, ..builtin() }, NOW),
        ("shell ready 60 s or more ago", builtin(), Millis(25_000 + 60_000)),
        ("shell ready long ago", builtin(), Millis(1_000_000)),
    ];
    for (why, c, now) in cases {
        assert!(!is_builtin_restore(&c, now), "{why}: {c:?} at {now:?}");
    }
}

#[test]
fn builtin_restore_input_boundary() {
    let at = |input: u64| BuiltinContext { last_input_at: Some(Millis(input)), ..builtin() };
    assert!(is_builtin_restore(&at(10_000 + 2_000), NOW), "exactly logon + 2000");
    assert!(!is_builtin_restore(&at(10_000 + 2_001), NOW), "logon + 2001");
}

#[test]
fn builtin_restore_shell_ready_boundary() {
    let ready = 25_000;
    assert!(is_builtin_restore(&builtin(), Millis(ready + 59_999)), "59 999 ms ago");
    assert!(!is_builtin_restore(&builtin(), Millis(ready + 60_000)), "60 000 ms ago");
    assert!(is_builtin_restore(&builtin(), Millis(ready)), "0 ms ago");
}

#[test]
fn builtin_restore_unknown_logon_fails() {
    for input in [None, Some(Millis(0)), Some(Millis(11_000))] {
        let c = BuiltinContext { logon_at: None, last_input_at: input, ..builtin() };
        assert!(!is_builtin_restore(&c, NOW), "input {input:?}");
    }
}

#[test]
fn builtin_restore_near_u64_max_does_not_overflow() {
    let max = u64::MAX;
    let c = BuiltinContext {
        logon_at: Some(Millis(max - 1_000)),
        last_input_at: Some(Millis(max)),
        shell_ready_at: Some(Millis(max - 59_999)),
        ..builtin()
    };
    // logon + 2000 saturates at u64::MAX, so input at u64::MAX is within grace.
    assert!(is_builtin_restore(&c, Millis(max)));
    let c = BuiltinContext { logon_at: Some(Millis(max)), ..c };
    assert!(is_builtin_restore(&c, Millis(max)));
    // Shell ready exactly 60 000 ms before u64::MAX.
    let c = BuiltinContext { shell_ready_at: Some(Millis(max - 60_000)), ..c };
    assert!(!is_builtin_restore(&c, Millis(max)));
    // Shell ready later than `now`: `Millis::since` saturates to 0 (recent).
    let c = BuiltinContext { shell_ready_at: Some(Millis(max)), ..c };
    assert!(is_builtin_restore(&c, Millis(0)));
    // Input far beyond a logon at 0.
    let c = BuiltinContext { logon_at: Some(Millis(0)), last_input_at: Some(Millis(max)), ..c };
    assert!(!is_builtin_restore(&c, Millis(0)));
}

#[test]
fn builtin_restore_takes_precedence_over_default_and_targeted() {
    for build in BUILDS {
        let c = ctx(build, true, true);
        for value in VALUES {
            for initial in INITIALS {
                let l = initial.location();
                assert_eq!(classify(&l, value, &c, &builtin(), NOW), LaunchKind::BuiltinRestore, "{initial:?}, {value:?}, {build}");
                // With the heuristic failing, the ordinary classification applies.
                let off = BuiltinContext { persist_browsers: None, ..builtin() };
                assert_eq!(classify(&l, value, &c, &off, NOW), expected(initial, value, true, true), "{initial:?}, {value:?}, {build}");
            }
        }
    }
}
