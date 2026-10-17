//! Deferred tabs: [`choose_deferred`] and [`placeholder_location`]
//! (Requirement 5.13; design.md, "Restore pipeline" → "Deferred tabs").
//!
//! File Explorer 22621 crashes (`SHELL32.dll` + `0x1ab1fa`,
//! `CSyncIntegrationManager::_CleanupCache`) once its sync-status cache
//! holds 128 entries, one per distinct folder a view showed in the last
//! 5 minutes (task 16.3; reproduced without this application by navigating
//! one window through 134 new folders). A restoration of more distinct
//! folders than that would bring Explorer down. So at most
//! [`IMMEDIATE_FOLDER_BUDGET`] distinct folders are opened immediately; the
//! remaining tabs are restored as **deferred tabs**: each shows a
//! placeholder (an empty library description file named after the saved
//! folder, see [`placeholder_location`]), so its tab header carries the saved
//! folder's name, and is navigated to its saved Location when the user
//! activates it (the Engine does that, see `Engine` "Deferred tabs").
//! Placeholders do not add sync-cache entries (task 16.3: 133 distinct
//! placeholders in one window left Explorer running, and the cache still
//! took 134 real folders afterwards before Explorer crashed).
//!
//! # Which tabs are deferred
//!
//! Decided once per restoration from the saved snapshot, so the choice never
//! changes while probes resolve. Locations are counted by canonical key
//! ([`LocationId::key`]); a tab whose Location is already counted costs
//! nothing and is never deferred. In this order:
//!
//! 1. the requested Location of a Targeted_Launch (always open already);
//! 2. the saved Active_Tab of every window, in saved Activation_Order. These
//!    are always immediate (they are what the user sees), even beyond the
//!    budget;
//! 3. every other tab, windows in saved Activation_Order, tabs left to right:
//!    immediate while fewer than `budget` distinct Locations are counted,
//!    deferred otherwise.
//!
//! The budget counts saved Locations. A tab redirected to an ancestor
//! (Req 9.1) may show one folder more; [`IMMEDIATE_FOLDER_BUDGET`] leaves
//! room for that, for the trigger's start location, and for folders the user
//! viewed shortly before.

use std::collections::{BTreeSet, HashSet};

use crate::model::{LocationId, LocationKind, ShellKind, Snapshot, WideString};

use super::{LaunchInfo, TabRef};

/// Most distinct saved Locations a restoration opens immediately; the other
/// tabs are deferred (see the module docs). Explorer 22621 crashes at 128.
pub const IMMEDIATE_FOLDER_BUDGET: usize = 100;

/// Longest placeholder name in UTF-16 units (the file name adds
/// `.library-ms`; the path stays well under `MAX_PATH`).
pub const MAX_PLACEHOLDER_NAME: usize = 120;

/// Extension of a placeholder file (an empty library description).
pub const PLACEHOLDER_EXTENSION: &str = ".library-ms";

/// Where deferred tabs get their placeholders, and how many distinct
/// Locations are opened immediately.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeferralConfig {
    pub budget: usize,
    /// Directory (an absolute path, no trailing separator) holding one
    /// numbered subdirectory per placeholder.
    pub placeholder_dir: WideString,
}

/// Result of [`choose_deferred`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Deferral {
    /// Saved tabs restored as deferred tabs.
    pub deferred: BTreeSet<TabRef>,
    /// Distinct saved Locations of the snapshot (with the requested one).
    pub distinct: usize,
    /// Distinct Locations opened immediately.
    pub immediate: usize,
}

/// Decides which saved tabs are deferred (see the module docs). Window ids in
/// `snapshot.activation` that name no window are skipped, as in
/// [`super::plan_restore`].
pub fn choose_deferred(snapshot: &Snapshot, launch: &LaunchInfo, budget: usize) -> Deferral {
    let windows: Vec<_> = snapshot.activation.iter().filter_map(|&id| snapshot.window(id)).collect();
    let mut all: HashSet<&LocationId> = windows.iter().flat_map(|w| w.tabs.iter().map(|t| &t.location)).collect();
    let mut counted: HashSet<&LocationId> = HashSet::new();
    if let LaunchInfo::Targeted { requested } = launch {
        all.insert(requested);
        counted.insert(requested);
    }
    for w in &windows {
        if let Some(t) = w.tabs.get(w.active) {
            counted.insert(&t.location);
        }
    }
    let mut deferred = BTreeSet::new();
    for w in &windows {
        for (i, t) in w.tabs.iter().enumerate() {
            if counted.contains(&t.location) {
                continue;
            }
            if counted.len() < budget {
                counted.insert(&t.location);
            } else {
                deferred.insert(TabRef { window: w.id, tab: i });
            }
        }
    }
    Deferral { deferred, distinct: all.len(), immediate: counted.len() }
}

/// The placeholder of the `index`-th deferred tab, which restores `saved`:
/// `<dir>\<index>\<name>.library-ms`, a library (`Shell(Library)`, how the
/// adapters capture such a file) whose display name is `name`, the saved
/// folder's name made safe for a file name ([`placeholder_name`]).
pub fn placeholder_location(dir: &WideString, index: usize, saved: &LocationId) -> LocationId {
    let name = placeholder_name(saved);
    let mut path = dir.units().to_vec();
    path.push(u16::from(b'\\'));
    path.extend(index.to_string().encode_utf16());
    path.push(u16::from(b'\\'));
    path.extend_from_slice(name.units());
    path.extend(PLACEHOLDER_EXTENSION.encode_utf16());
    LocationId {
        case_sensitive: false,
        kind: LocationKind::Shell(ShellKind::Library),
        parsing_name: WideString::new(path),
        display_name: name,
        pidl: Vec::new(),
        search: None,
    }
}

/// The name a placeholder shows: the saved display name (else the last
/// component of the parsing name) with characters a file name cannot hold
/// (`\ / : * ? " < > |`, control characters, unpaired surrogates) replaced by
/// `_`, trailing dots and spaces removed, at most [`MAX_PLACEHOLDER_NAME`]
/// units (never splitting a surrogate pair), `_` appended to a reserved
/// device name, and `Folder` if nothing is left.
pub fn placeholder_name(saved: &LocationId) -> WideString {
    let source = if saved.display_name.is_empty() {
        let p = saved.parsing_name.units();
        let trimmed = trim_end(p, &[u16::from(b'\\'), u16::from(b'/')]);
        let start = trimmed.iter().rposition(|&c| c == u16::from(b'\\') || c == u16::from(b'/')).map_or(0, |i| i + 1);
        trimmed[start..].to_vec()
    } else {
        saved.display_name.units().to_vec()
    };
    let mut out: Vec<u16> = Vec::with_capacity(source.len().min(MAX_PLACEHOLDER_NAME));
    let mut i = 0;
    while i < source.len() && out.len() < MAX_PLACEHOLDER_NAME {
        let c = source[i];
        let pair = (0xD800..0xDC00).contains(&c) && source.get(i + 1).is_some_and(|d| (0xDC00..0xE000).contains(d));
        if pair {
            if out.len() + 2 > MAX_PLACEHOLDER_NAME {
                break;
            }
            out.extend_from_slice(&source[i..i + 2]);
            i += 2;
            continue;
        }
        let bad = c < 0x20 || (0xD800..0xE000).contains(&c) || "\\/:*?\"<>|".encode_utf16().any(|b| b == c);
        out.push(if bad { u16::from(b'_') } else { c });
        i += 1;
    }
    let len = trim_end(&out, &[u16::from(b'.'), u16::from(b' ')]).len();
    out.truncate(len);
    if out.is_empty() {
        out = "Folder".encode_utf16().collect();
    }
    if is_reserved_device_name(&out) {
        out.push(u16::from(b'_'));
    }
    WideString::new(out)
}

/// Renames the UIA tab-strip items of tabs that show placeholders. Newer
/// builds (measured on 26300) name a tab's strip item by its parsing name,
/// older ones (22621) by its display name; either way the item of a deferred
/// tab carries the placeholder's name, which matches no tab of the saved
/// Location the tracker is given. `renames` holds (placeholder name, saved
/// name) pairs; names without a pair stay as they are.
pub fn rename_strip_names(names: &mut [WideString], renames: &[(WideString, WideString)]) {
    for name in names {
        if let Some((_, to)) = renames.iter().find(|(from, _)| from == name) {
            *name = to.clone();
        }
    }
}

fn trim_end<'a>(s: &'a [u16], chars: &[u16]) -> &'a [u16] {
    let end = s.iter().rposition(|c| !chars.contains(c)).map_or(0, |i| i + 1);
    &s[..end]
}

/// `CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9`, `LPT1`–`LPT9` (any case): file
/// names Windows maps to devices, whatever their extension.
fn is_reserved_device_name(name: &[u16]) -> bool {
    let s: String = String::from_utf16_lossy(name).to_ascii_uppercase();
    matches!(s.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (s.len() == 4
            && (s.starts_with("COM") || s.starts_with("LPT"))
            && s.as_bytes()[3].is_ascii_digit()
            && s.as_bytes()[3] != b'0')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DisplayRef, Placement, Rect, ShowState, TabRec, Volume, WindowId, WindowRec};

    fn fs(path: &str) -> LocationId {
        let name = path.rsplit('\\').next().unwrap_or(path);
        LocationId {
            case_sensitive: false,
            kind: LocationKind::FileSystem(Volume::Fixed),
            parsing_name: path.into(),
            display_name: name.into(),
            pidl: vec![],
            search: None,
        }
    }

    fn window(id: u32, paths: &[&str], active: usize) -> WindowRec {
        let r = Rect { left: 0, top: 0, right: 800, bottom: 600 };
        WindowRec {
            id: WindowId(id),
            tabs: paths.iter().map(|p| TabRec { location: fs(p) }).collect(),
            active,
            placement: Placement {
                state: ShowState::Normal,
                restored: r,
                display: DisplayRef { device: "P".into(), monitor: r },
            },
        }
    }

    fn snap(windows: Vec<WindowRec>, activation: &[u32]) -> Snapshot {
        Snapshot { windows, activation: activation.iter().map(|&i| WindowId(i)).collect() }
    }

    fn refs(d: &Deferral) -> Vec<(u32, usize)> {
        d.deferred.iter().map(|t| (t.window.0, t.tab)).collect()
    }

    #[test]
    fn within_budget_nothing_is_deferred() {
        let s = snap(vec![window(1, &[r"C:\a", r"C:\b"], 0), window(2, &[r"C:\c"], 0)], &[1, 2]);
        let d = choose_deferred(&s, &LaunchInfo::Default, 3);
        assert!(d.deferred.is_empty());
        assert_eq!((d.distinct, d.immediate), (3, 3));
    }

    #[test]
    fn actives_first_then_windows_in_activation_order_left_to_right() {
        // Window 2 ranks first. Budget 3: actives b (w2) and e (w1), then
        // w2's a; everything else is deferred except repeats of counted ones.
        let s = snap(
            vec![window(1, &[r"C:\d", r"C:\e", r"C:\a"], 1), window(2, &[r"C:\a", r"C:\b", r"C:\c", r"C:\e"], 1)],
            &[2, 1],
        );
        let d = choose_deferred(&s, &LaunchInfo::Default, 3);
        assert_eq!(refs(&d), [(1, 0), (2, 2)]);
        assert_eq!((d.distinct, d.immediate), (5, 3));
    }

    #[test]
    fn actives_are_immediate_beyond_the_budget_and_requested_counts_first() {
        let s = snap(vec![window(1, &[r"C:\a", r"C:\x"], 0), window(2, &[r"C:\b", r"C:\x"], 0)], &[1, 2]);
        let d = choose_deferred(&s, &LaunchInfo::Default, 1);
        assert_eq!(refs(&d), [(1, 1), (2, 1)]);
        assert_eq!(d.immediate, 2);
        let req = LaunchInfo::Targeted { requested: fs(r"C:\x") };
        let d = choose_deferred(&s, &req, 1);
        assert!(d.deferred.is_empty(), "C:\\x is open already");
        assert_eq!((d.distinct, d.immediate), (3, 3));
    }

    #[test]
    fn strip_items_named_after_a_placeholder_are_renamed_to_the_saved_folder() {
        let saved = fs(r"D:\Work\Proj");
        let placeholder = placeholder_location(&r"C:\data\deferred".into(), 3, &saved);
        let renames = vec![
            (placeholder.parsing_name.clone(), saved.parsing_name.clone()),
            (placeholder.display_name.clone(), saved.display_name.clone()),
        ];
        let mut names: Vec<WideString> = vec![r"D:\Other".into(), placeholder.parsing_name.clone(), "Proj".into()];
        rename_strip_names(&mut names, &renames);
        let want: Vec<WideString> = vec![r"D:\Other".into(), saved.parsing_name.clone(), saved.display_name.clone()];
        assert_eq!(names, want);
    }

    #[test]
    fn placeholder_is_a_numbered_library_named_after_the_folder() {
        let p = placeholder_location(&r"C:\data\deferred".into(), 7, &fs(r"D:\Work\Projekt Ü"));
        assert_eq!(p.kind, LocationKind::Shell(ShellKind::Library));
        assert_eq!(p.parsing_name.to_string_lossy(), r"C:\data\deferred\7\Projekt Ü.library-ms");
        assert_eq!(p.display_name.to_string_lossy(), "Projekt Ü");
    }

    #[test]
    fn names_are_made_safe_for_file_names() {
        let named = |display: &str, parsing: &str| {
            let mut l = fs(parsing);
            l.display_name = display.into();
            placeholder_name(&l).to_string_lossy()
        };
        assert_eq!(named("a:b*c?\"<>|/\\\u{1}", r"C:\x"), "a_b_c________");
        assert_eq!(named("trail. . ", r"C:\x"), "trail");
        assert_eq!(named("...", r"C:\x"), "Folder");
        assert_eq!(named("", r"C:\Users\me\Stuff\"), "Stuff");
        assert_eq!(named("", r"C:\"), "C_");
        assert_eq!(named("con", r"C:\x"), "con_");
        assert_eq!(named("COM1", r"C:\x"), "COM1_");
        assert_eq!(named("COM0", r"C:\x"), "COM0");
        let long = "x".repeat(300);
        assert_eq!(named(&long, r"C:\x").len(), MAX_PLACEHOLDER_NAME);
        // A surrogate pair at the limit is dropped whole; lone surrogates are replaced.
        let mut l = fs(r"C:\x");
        let mut units: Vec<u16> = "y".repeat(MAX_PLACEHOLDER_NAME - 1).encode_utf16().collect();
        units.extend("😀".encode_utf16());
        l.display_name = WideString::new(units);
        assert_eq!(placeholder_name(&l).len(), MAX_PLACEHOLDER_NAME - 1);
        l.display_name = WideString::new(vec![0xD800, u16::from(b'a'), 0xDC00]);
        assert_eq!(placeholder_name(&l).to_string_lossy(), "_a_");
    }
}
