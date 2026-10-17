//! `ancestors` and `choose_fallback` for Unavailable_Locations
//! (Requirements 8.9, 9.1, 9.2, 9.3; design.md, "Fallback" and "Ancestors").
//!
//! # Path forms
//!
//! Paths are raw UTF-16 code units ([`WideString`]) and are split on `\`
//! (0x5C). Component bytes are never case-folded, normalised or re-encoded
//! (Req 8.4). Recognised forms:
//!
//! | Form | Root (shallowest ancestor) |
//! |------|----------------------------|
//! | `C:\a\b` | `C:\` |
//! | `\\server\share\a` | `\\server\share` (the bare server is not an ancestor) |
//! | `\\?\C:\a\b` | `\\?\C:\` |
//! | `\\?\UNC\server\share\a` | `\\?\UNC\server\share` |
//!
//! Anything else (relative paths, `C:` without a separator, `\\.\` device
//! paths, `\\?\Volume{…}` paths, shell `::{GUID}` names, paths with `.` or
//! `..` components, a UNC root with an empty server or share) has no
//! ancestors, so a missing Location of that form is omitted rather than
//! redirected somewhere unexpected.
//!
//! Separators are handled conservatively: repeated separators and trailing
//! separators are skipped, and outside the `\\?\` forms `/` is accepted as a
//! separator too, as Win32 does. Inside a `\\?\` form only `\` separates.
//!
//! # Stored form vs probe form
//!
//! Ancestors are produced **in the same form as stored**: every ancestor's
//! parsing name is a prefix of the stored parsing name (trailing separators
//! removed; the drive root keeps its separator, `C:\`). A `\\?\` Location
//! therefore has `\\?\` ancestors, and the stored text is never rewritten.
//! [`probe_path`] is the separate normaliser the prober applies just before
//! probing; its result is never stored or logged as the Location.

use crate::model::{LocationId, LocationKind, Volume, WideString};

/// Why a tab was omitted (Req 9.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OmitReason {
    /// The Location does not exist and no ancestor qualifies.
    NotExist,
    /// The Location is inaccessible to the current user.
    Inaccessible,
    /// The Location did not respond within 5 seconds.
    Timeout,
    /// The Location_Identifier could not be resolved.
    Unresolvable,
    /// A non-ZIP Shell_Location (or a search whose scope) is unavailable.
    ShellUnavailable,
}

/// Probe result for one ancestor of a missing Location.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AncestorProbe {
    /// One of [`ancestors`] of the missing Location.
    pub location: LocationId,
    /// Exists, is accessible, and responded within the shared 5 s deadline
    /// (Req 9.1). Missing, access denied and timed out are all `false`.
    pub available: bool,
}

/// Result of probing one tab's Location (design.md, "Fallback").
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProbeOutcome {
    Available,
    /// FileSystem or ZipFolder Location that does not exist, with the probe
    /// results of its ancestors (deepest first, as listed by [`ancestors`]).
    Missing { ancestors: Vec<AncestorProbe> },
    AccessDenied,
    /// Timed out, or a network-unreachable class of error (Req 9.3).
    Timeout,
    /// Neither the parsing name nor the stored PIDL could be bound.
    Unresolvable,
    /// A non-ZIP Shell_Location is unavailable, including a search whose
    /// scope is unavailable (Req 8.9).
    ShellUnavailable,
}

/// What restoration does with one tab, decided by [`choose_fallback`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fallback {
    /// The Location is available; restore it as saved.
    Keep,
    /// Open this ancestor instead (Req 9.1). The logged reason is
    /// [`OmitReason::NotExist`].
    Redirected(LocationId),
    /// Omit the tab (Req 9.2).
    Omitted(OmitReason),
}

/// Decides the fate of a tab from its probe outcome (Req 8.9, 9.1, 9.2, 9.3).
///
/// - `Missing` FileSystem/ZipFolder → `Redirected` to the available ancestor
///   with the most path components, else `Omitted(NotExist)`.
/// - `AccessDenied` → `Inaccessible`, `Timeout` → `Timeout`,
///   `Unresolvable` → `Unresolvable`, `ShellUnavailable` → `ShellUnavailable`.
/// - A Shell_Location other than ZipFolder, and a search, is never redirected:
///   `Missing` for such a kind (e.g. a search whose scope no longer exists)
///   is `Omitted(ShellUnavailable)`.
pub fn choose_fallback(loc: &LocationId, outcome: &ProbeOutcome) -> Fallback {
    let fs_like = matches!(loc.kind, LocationKind::FileSystem(_) | LocationKind::ZipFolder);
    match outcome {
        ProbeOutcome::Available => Fallback::Keep,
        ProbeOutcome::Missing { ancestors } if fs_like => {
            // Max by component count; on a tie the earlier (deeper-listed) wins.
            let mut best: Option<(usize, &LocationId)> = None;
            for probe in ancestors.iter().filter(|p| p.available) {
                let depth = component_count(probe.location.parsing_name.units()).unwrap_or(0);
                if best.is_none_or(|(d, _)| depth > d) {
                    best = Some((depth, &probe.location));
                }
            }
            match best {
                Some((_, a)) => Fallback::Redirected(a.clone()),
                None => Fallback::Omitted(OmitReason::NotExist),
            }
        }
        ProbeOutcome::Missing { .. } | ProbeOutcome::ShellUnavailable => {
            Fallback::Omitted(OmitReason::ShellUnavailable)
        }
        ProbeOutcome::AccessDenied => Fallback::Omitted(OmitReason::Inaccessible),
        ProbeOutcome::Timeout => Fallback::Omitted(OmitReason::Timeout),
        ProbeOutcome::Unresolvable => Fallback::Omitted(OmitReason::Unresolvable),
    }
}

/// Proper ancestors of a FileSystem or ZipFolder Location, deepest first,
/// ending at the drive root or UNC share root (design.md, "Ancestors").
///
/// - Parsing names are in the stored form (see the module docs).
/// - `display_name` is the last component (`C:` for a drive root, the share
///   name for a UNC root); `pidl` is empty because it is unknown; `search` is
///   `None`.
/// - A FileSystem Location's ancestors keep its [`Volume`].
/// - For a ZipFolder, ancestors inside the archive stay ZipFolder; the `.zip`
///   folder itself and everything above it are FileSystem, with
///   [`Volume::Unc`] for UNC forms and [`Volume::Fixed`] otherwise (the real
///   volume type of the archive's drive is not known here). The archive is the
///   deepest component ending in `.zip` (ASCII case-insensitive). Without such
///   a component the boundary is unknown, and every ancestor except the root
///   stays ZipFolder.
/// - Other kinds (Shell, Search) have no ancestors.
pub fn ancestors(loc: &LocationId) -> Vec<LocationId> {
    let is_zip_folder = match loc.kind {
        LocationKind::FileSystem(_) => false,
        LocationKind::ZipFolder => true,
        LocationKind::Shell(_) | LocationKind::Search => return Vec::new(),
    };
    let path = loc.parsing_name.units();
    let Some(split) = split(path) else { return Vec::new() };
    let n = split.components.len();
    if n == 0 {
        return Vec::new();
    }
    let fs_kind = match loc.kind {
        LocationKind::FileSystem(v) => LocationKind::FileSystem(v),
        _ => LocationKind::FileSystem(match split.root {
            RootKind::Drive => Volume::Fixed,
            RootKind::Unc => Volume::Unc,
        }),
    };
    let archive = if is_zip_folder {
        split.components.iter().rposition(|&(a, b)| is_zip_name(&path[a..b]))
    } else {
        None
    };
    let kind_at = |k: usize| match (is_zip_folder, archive) {
        (false, _) => fs_kind,
        (true, Some(a)) if k <= a => fs_kind,
        (true, _) => LocationKind::ZipFolder,
    };
    let make = |kind, end: usize, (a, b): (usize, usize)| LocationId {
        case_sensitive: false,
        kind,
        parsing_name: WideString::new(path[..end].to_vec()),
        display_name: WideString::new(path[a..b].to_vec()),
        pidl: Vec::new(),
        search: None,
    };

    let mut out = Vec::with_capacity(n);
    for k in (0..n - 1).rev() {
        let span = split.components[k];
        out.push(make(kind_at(k), span.1, span));
    }
    out.push(make(fs_kind, split.root_end, split.root_name));
    out
}

/// Parsing names of the proper ancestors of `path`, deepest first, as
/// prefixes of `path` (the path-only part of [`ancestors`]).
pub fn ancestor_paths(path: &[u16]) -> Vec<&[u16]> {
    let Some(split) = split(path) else { return Vec::new() };
    let n = split.components.len();
    if n == 0 {
        return Vec::new();
    }
    let mut out: Vec<&[u16]> =
        (0..n - 1).rev().map(|k| &path[..split.components[k].1]).collect();
    out.push(&path[..split.root_end]);
    out
}

/// Number of path components, counting the root as one (`C:\` → 1,
/// `\\server\share\a` → 2), or `None` for an unrecognised form.
pub fn component_count(path: &[u16]) -> Option<usize> {
    split(path).map(|s| 1 + s.components.len())
}

/// The form the prober hands to Windows (design.md: long `\\?\` forms are
/// normalised for probing only, never in what is stored).
///
/// - `\\?\C:\rest` → `C:\rest`
/// - `\\?\UNC\server\share\rest` → `\\server\share\rest`
/// - anything else is returned unchanged.
///
/// A `\\?\` path is kept verbatim when dropping the prefix would change what
/// it names under Win32 rules: it contains `/`, or a component that is `.` or
/// `..` or ends in `.` or a space (Win32 strips those). Component text is
/// never altered.
pub fn probe_path(path: &[u16]) -> Vec<u16> {
    if !path.starts_with(&VERBATIM) {
        return path.to_vec();
    }
    let rest = &path[VERBATIM.len()..];
    let normalised: Vec<u16> = if is_drive_root(rest, 0, |u| u == BACKSLASH) {
        rest.to_vec()
    } else if starts_with_unc_marker(rest) && unc_root(path, VERBATIM.len() + 4, |u| u == BACKSLASH).is_some() {
        let mut v = vec![BACKSLASH, BACKSLASH];
        v.extend_from_slice(&rest[4..]);
        v
    } else {
        return path.to_vec();
    };
    let win32_safe = !normalised.contains(&SLASH)
        && normalised.split(|&u| u == BACKSLASH).filter(|c| !c.is_empty()).all(|c| {
            c != [DOT] && c != [DOT, DOT] && !matches!(c.last(), Some(&DOT) | Some(&SPACE))
        });
    if win32_safe { normalised } else { path.to_vec() }
}

// ---------------------------------------------------------------------------
// Splitting
// ---------------------------------------------------------------------------

const BACKSLASH: u16 = 0x5C;
const SLASH: u16 = 0x2F;
const COLON: u16 = 0x3A;
const QUESTION: u16 = 0x3F;
const DOT: u16 = 0x2E;
const SPACE: u16 = 0x20;
/// `\\?\`
const VERBATIM: [u16; 4] = [BACKSLASH, BACKSLASH, QUESTION, BACKSLASH];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RootKind {
    Drive,
    Unc,
}

#[derive(Debug)]
struct Split {
    root: RootKind,
    /// `path[..root_end]` is the root ancestor's parsing name.
    root_end: usize,
    /// Span of the root's display name (`C:` or the share name).
    root_name: (usize, usize),
    /// Non-empty components below the root, as `(start, end)` spans.
    components: Vec<(usize, usize)>,
}

fn split(path: &[u16]) -> Option<Split> {
    let verbatim = path.starts_with(&VERBATIM);
    let is_sep = move |u: u16| u == BACKSLASH || (!verbatim && u == SLASH);

    let (root, root_end, root_name) = if verbatim {
        let at = VERBATIM.len();
        if is_drive_root(path, at, is_sep) {
            (RootKind::Drive, at + 3, (at, at + 2))
        } else if starts_with_unc_marker(&path[at..]) {
            let (end, name) = unc_root(path, at + 4, is_sep)?;
            (RootKind::Unc, end, name)
        } else {
            return None;
        }
    } else if is_drive_root(path, 0, is_sep) {
        (RootKind::Drive, 3, (0, 2))
    } else if path.len() >= 2 && is_sep(path[0]) && is_sep(path[1]) {
        let (end, name) = unc_root(path, 2, is_sep)?;
        (RootKind::Unc, end, name)
    } else {
        return None;
    };

    let mut components = Vec::new();
    let mut start = root_end;
    for i in root_end..=path.len() {
        if i == path.len() || is_sep(path[i]) {
            if i > start {
                let c = &path[start..i];
                if c == [DOT] || c == [DOT, DOT] {
                    return None;
                }
                components.push((start, i));
            }
            start = i + 1;
        }
    }
    Some(Split { root, root_end, root_name, components })
}

/// `X:` followed by a separator at `path[at..]`, with `X` an ASCII letter.
fn is_drive_root(path: &[u16], at: usize, is_sep: impl Fn(u16) -> bool) -> bool {
    path.len() >= at + 3
        && u8::try_from(path[at]).is_ok_and(|b| b.is_ascii_alphabetic())
        && path[at + 1] == COLON
        && is_sep(path[at + 2])
}

/// `UNC\` (ASCII case-insensitive) at the start of `rest`.
fn starts_with_unc_marker(rest: &[u16]) -> bool {
    rest.len() >= 4 && ascii_eq_ignore_case(&rest[..3], b"UNC") && rest[3] == BACKSLASH
}

/// `server<sep>share` starting at `path[at]`; returns the end of the share
/// and the share's span. Rejects an empty server or share, a bare server,
/// and the `?` / `.` device prefixes.
fn unc_root(path: &[u16], at: usize, is_sep: impl Fn(u16) -> bool) -> Option<(usize, (usize, usize))> {
    let find_sep = |from: usize| (from..path.len()).find(|&i| is_sep(path[i])).unwrap_or(path.len());
    let server_end = find_sep(at);
    let server = &path[at..server_end];
    if server.is_empty() || server == [QUESTION] || server == [DOT] || server_end == path.len() {
        return None;
    }
    let share_start = server_end + 1;
    let share_end = find_sep(share_start);
    if share_end == share_start {
        return None;
    }
    Some((share_end, (share_start, share_end)))
}

/// Ends in `.zip`, ASCII case-insensitive.
fn is_zip_name(name: &[u16]) -> bool {
    name.len() >= 4 && ascii_eq_ignore_case(&name[name.len() - 4..], b".zip")
}

fn ascii_eq_ignore_case(units: &[u16], ascii: &[u8]) -> bool {
    units.len() == ascii.len()
        && units
            .iter()
            .zip(ascii)
            .all(|(&u, &b)| u8::try_from(u).is_ok_and(|c| c.eq_ignore_ascii_case(&b)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{SearchSpec, ShellKind};

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    fn loc(kind: LocationKind, path: &str) -> LocationId {
        LocationId {
            case_sensitive: false,
            kind,
            parsing_name: path.into(),
            display_name: path.into(),
            pidl: vec![7, 7],
            search: None,
        }
    }

    fn fs(path: &str) -> LocationId {
        loc(LocationKind::FileSystem(Volume::Fixed), path)
    }

    fn paths(p: &str) -> Vec<String> {
        let units = w(p);
        ancestor_paths(&units).into_iter().map(String::from_utf16_lossy).collect()
    }

    /// `(parsing name, display name, kind)` of each ancestor.
    fn summary(l: &LocationId) -> Vec<(String, String, LocationKind)> {
        ancestors(l)
            .into_iter()
            .map(|a| {
                assert!(a.pidl.is_empty());
                assert!(a.search.is_none());
                (a.parsing_name.to_string_lossy(), a.display_name.to_string_lossy(), a.kind)
            })
            .collect()
    }

    fn probe(location: LocationId, available: bool) -> AncestorProbe {
        AncestorProbe { location, available }
    }

    // ----- ancestors: path forms -----

    #[test]
    fn drive_path_goes_up_to_drive_root() {
        assert_eq!(paths(r"C:\a\b\c"), [r"C:\a\b", r"C:\a", r"C:\"]);
        assert_eq!(paths(r"C:\a"), [r"C:\"]);
        assert_eq!(component_count(&w(r"C:\")), Some(1));
        assert_eq!(component_count(&w(r"C:\a\b\c")), Some(4));
    }

    #[test]
    fn root_and_unrecognised_forms_have_no_ancestors() {
        for p in [
            r"C:\", "C:", r"C:a\b", r"a\b", "", r"\a\b", r"::{20D04FE0-3AEA-1069-A2D8-08002B30309D}",
            r"\\.\C:\x", r"\\?\Volume{1234}\x", r"\\srv", r"\\srv\", r"\\srv\share", r"\\srv\share\",
            r"\\srv\\share\x", r"\\\srv\share\x", r"C:\a\..\b", r"C:\a\.\b", r"\\?\C:", r"\\?\UNC\srv",
        ] {
            assert!(paths(p).is_empty(), "{p:?}");
        }
    }

    #[test]
    fn trailing_and_repeated_separators_are_skipped() {
        assert_eq!(paths(r"C:\a\\b\"), [r"C:\a", r"C:\"]);
        assert_eq!(paths(r"C:\\a\b\\"), [r"C:\\a", r"C:\"]);
        assert_eq!(paths(r"C:\a\"), [r"C:\"]);
        assert_eq!(paths(r"\\srv\share\\x\\"), [r"\\srv\share"]);
        assert_eq!(component_count(&w(r"C:\a\\b\")), Some(3));
    }

    #[test]
    fn forward_slashes_separate_outside_verbatim_form() {
        assert_eq!(paths("C:/a/b"), ["C:/a", "C:/"]);
        assert_eq!(paths(r"C:\a/b\c"), [r"C:\a/b", r"C:\a", r"C:\"]);
        assert_eq!(paths("//srv/share/x/y"), ["//srv/share/x", "//srv/share"]);
        // Inside `\\?\`, `/` is part of the component.
        assert_eq!(paths(r"\\?\C:\a/b\c"), [r"\\?\C:\a/b", r"\\?\C:\"]);
    }

    #[test]
    fn unc_path_stops_at_share_root() {
        assert_eq!(paths(r"\\srv\share\x\y"), [r"\\srv\share\x", r"\\srv\share"]);
        assert_eq!(paths(r"\\srv\share\x"), [r"\\srv\share"]);
        assert_eq!(component_count(&w(r"\\srv\share")), Some(1));
        assert_eq!(component_count(&w(r"\\srv")), None);
    }

    #[test]
    fn verbatim_forms_keep_stored_form() {
        assert_eq!(paths(r"\\?\C:\a\b"), [r"\\?\C:\a", r"\\?\C:\"]);
        assert_eq!(paths(r"\\?\UNC\srv\share\a\b"), [r"\\?\UNC\srv\share\a", r"\\?\UNC\srv\share"]);
        assert_eq!(paths(r"\\?\unc\srv\share\a"), [r"\\?\unc\srv\share"]);
        assert_eq!(component_count(&w(r"\\?\UNC\srv\share\a")), Some(2));
    }

    #[test]
    fn probe_path_normalises_verbatim_forms_only() {
        let p = |s: &str| String::from_utf16_lossy(&probe_path(&w(s)));
        assert_eq!(p(r"\\?\C:\a\b"), r"C:\a\b");
        assert_eq!(p(r"\\?\C:\"), r"C:\");
        assert_eq!(p(r"\\?\UNC\srv\share\a"), r"\\srv\share\a");
        assert_eq!(p(r"\\?\UNC\srv\share"), r"\\srv\share");
        // Already Win32 forms, and unknown forms, are untouched.
        for s in [r"C:\a\b", r"\\srv\share\a", "C:/a", r"\\?\Volume{1}\a", r"\\?\UNC\srv", "::{X}"] {
            assert_eq!(p(s), s);
        }
        // Dropping the prefix would change the meaning: keep verbatim.
        for s in [r"\\?\C:\a.", r"\\?\C:\a \b", r"\\?\C:\a/b", r"\\?\C:\a\..\b", r"\\?\UNC\srv\share\x."] {
            assert_eq!(p(s), s);
        }
    }

    #[test]
    fn case_and_unicode_of_components_are_preserved() {
        let mut path = w("C:\\Ä\\e\u{301}\\MiXeD\\");
        let lone_start = path.len();
        path.extend([0xD800, b'x' as u16]);
        let mut l = fs("");
        l.parsing_name = WideString::new(path.clone());
        let got = ancestors(&l);
        let names: Vec<&[u16]> = got.iter().map(|a| a.parsing_name.units()).collect();
        assert_eq!(names, [&path[..lone_start - 1], &w("C:\\Ä\\e\u{301}")[..], &w("C:\\Ä")[..], &w("C:\\")[..]]);
        assert_eq!(got[0].display_name.units(), w("MiXeD"));
        // A lone surrogate component is split and kept verbatim.
        let mut deeper = path.clone();
        deeper.extend(w(r"\y"));
        assert_eq!(ancestor_paths(&deeper)[0], &path[..]);
    }

    // ----- ancestors: kinds and fields -----

    #[test]
    fn file_system_ancestors_keep_volume_and_name_last_component() {
        let l = loc(LocationKind::FileSystem(Volume::Mapped), r"Z:\proj\src");
        let m = LocationKind::FileSystem(Volume::Mapped);
        assert_eq!(
            summary(&l),
            [(r"Z:\proj".into(), "proj".into(), m), (r"Z:\".into(), "Z:".into(), m)]
        );
        let u = LocationKind::FileSystem(Volume::Unc);
        let l = loc(u, r"\\srv\share\x\y");
        assert_eq!(
            summary(&l),
            [(r"\\srv\share\x".into(), "x".into(), u), (r"\\srv\share".into(), "share".into(), u)]
        );
        let l = fs(r"\\?\C:\a\b");
        assert_eq!(summary(&l)[1].1, "C:");
    }

    #[test]
    fn zip_ancestors_continue_through_the_archive() {
        let z = LocationKind::ZipFolder;
        let f = LocationKind::FileSystem(Volume::Fixed);
        let l = loc(z, r"C:\d\Arch.ZIP\in\deep");
        assert_eq!(
            summary(&l),
            [
                (r"C:\d\Arch.ZIP\in".into(), "in".into(), z),
                (r"C:\d\Arch.ZIP".into(), "Arch.ZIP".into(), f),
                (r"C:\d".into(), "d".into(), f),
                (r"C:\".into(), "C:".into(), f),
            ]
        );
        // The archive root itself: everything above is file system.
        let kinds: Vec<_> = summary(&loc(z, r"C:\d\a.zip")).into_iter().map(|s| s.2).collect();
        assert_eq!(kinds, [f, f]);
        // UNC archive.
        let u = LocationKind::FileSystem(Volume::Unc);
        let kinds: Vec<_> = summary(&loc(z, r"\\srv\share\a.zip\x\y")).into_iter().map(|s| s.2).collect();
        assert_eq!(kinds, [z, u, u]);
        // Unknown boundary: all but the root stay ZipFolder.
        let kinds: Vec<_> = summary(&loc(z, r"C:\d\archive\x")).into_iter().map(|s| s.2).collect();
        assert_eq!(kinds, [z, z, f]);
        // A FileSystem location with a `.zip`-named folder stays file system.
        let kinds: Vec<_> = summary(&fs(r"C:\a.zip\x\y")).into_iter().map(|s| s.2).collect();
        assert_eq!(kinds, [f, f, f]);
    }

    #[test]
    fn shell_and_search_locations_have_no_ancestors() {
        assert!(ancestors(&loc(LocationKind::Shell(ShellKind::Library), r"C:\a\b")).is_empty());
        let mut s = loc(LocationKind::Search, r"C:\a\b");
        s.search = Some(SearchSpec { query: "q".into(), scope: Box::new(fs(r"C:\a")) });
        assert!(ancestors(&s).is_empty());
    }

    // ----- choose_fallback -----

    #[test]
    fn available_is_kept() {
        assert_eq!(choose_fallback(&fs(r"C:\a"), &ProbeOutcome::Available), Fallback::Keep);
        let shell = loc(LocationKind::Shell(ShellKind::ThisPc), "::{X}");
        assert_eq!(choose_fallback(&shell, &ProbeOutcome::Available), Fallback::Keep);
    }

    #[test]
    fn missing_redirects_to_deepest_available_ancestor() {
        let l = fs(r"C:\a\b\c\d");
        let anc = ancestors(&l);
        let outcome = ProbeOutcome::Missing {
            ancestors: vec![
                probe(anc[0].clone(), false),
                probe(anc[1].clone(), true),
                probe(anc[2].clone(), false),
                probe(anc[3].clone(), true),
            ],
        };
        let Fallback::Redirected(got) = choose_fallback(&l, &outcome) else { panic!() };
        assert!(got.identical(&anc[1]), "{got:?}");

        // Independent of the order the prober lists them in.
        let outcome = ProbeOutcome::Missing {
            ancestors: vec![probe(anc[3].clone(), true), probe(anc[2].clone(), true)],
        };
        assert_eq!(choose_fallback(&l, &outcome), Fallback::Redirected(anc[2].clone()));
    }

    #[test]
    fn missing_without_qualifying_ancestor_is_not_exist() {
        let l = fs(r"\\srv\share\x\y");
        let anc = ancestors(&l);
        let none = ProbeOutcome::Missing { ancestors: anc.into_iter().map(|a| probe(a, false)).collect() };
        assert_eq!(choose_fallback(&l, &none), Fallback::Omitted(OmitReason::NotExist));
        let empty = ProbeOutcome::Missing { ancestors: vec![] };
        assert_eq!(choose_fallback(&fs(r"C:\"), &empty), Fallback::Omitted(OmitReason::NotExist));
    }

    #[test]
    fn missing_zip_folder_can_redirect_to_the_archive() {
        let l = loc(LocationKind::ZipFolder, r"C:\d\a.zip\x\y");
        let anc = ancestors(&l);
        let outcome = ProbeOutcome::Missing {
            ancestors: vec![probe(anc[0].clone(), false), probe(anc[1].clone(), true), probe(anc[2].clone(), true)],
        };
        let Fallback::Redirected(got) = choose_fallback(&l, &outcome) else { panic!() };
        assert_eq!(got.parsing_name.to_string_lossy(), r"C:\d\a.zip");
        assert_eq!(got.kind, LocationKind::FileSystem(Volume::Fixed));
    }

    #[test]
    fn other_outcomes_map_to_their_reason() {
        let cases = [
            (ProbeOutcome::AccessDenied, OmitReason::Inaccessible),
            (ProbeOutcome::Timeout, OmitReason::Timeout),
            (ProbeOutcome::Unresolvable, OmitReason::Unresolvable),
            (ProbeOutcome::ShellUnavailable, OmitReason::ShellUnavailable),
        ];
        for l in [fs(r"C:\a"), loc(LocationKind::ZipFolder, r"C:\a.zip\b"), loc(LocationKind::Shell(ShellKind::Network), r"\\srv")] {
            for (outcome, reason) in &cases {
                assert_eq!(choose_fallback(&l, outcome), Fallback::Omitted(*reason), "{l:?} {outcome:?}");
            }
        }
    }

    #[test]
    fn shell_and_search_locations_are_never_redirected() {
        let shell = loc(LocationKind::Shell(ShellKind::Library), r"C:\lib\x");
        let mut search = loc(LocationKind::Search, "search-ms:query=q");
        search.search = Some(SearchSpec { query: "q".into(), scope: Box::new(fs(r"D:\gone\deeper")) });
        // Even with an available ancestor reported, a non-ZIP Shell_Location
        // (and a search whose scope is missing, Req 8.9) is omitted.
        let outcome = ProbeOutcome::Missing { ancestors: vec![probe(fs(r"C:\lib"), true)] };
        let saved = loc(LocationKind::Shell(ShellKind::SavedSearch), r"C:\lib\gone.search-ms");
        assert!(ancestors(&saved).is_empty());
        for l in [shell, search, saved] {
            assert_eq!(choose_fallback(&l, &outcome), Fallback::Omitted(OmitReason::ShellUnavailable));
            assert_eq!(
                choose_fallback(&l, &ProbeOutcome::ShellUnavailable),
                Fallback::Omitted(OmitReason::ShellUnavailable)
            );
        }
    }
}
