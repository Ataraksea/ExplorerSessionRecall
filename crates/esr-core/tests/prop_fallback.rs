//! Property test for fallback selection (`esr_core::plan::fallback`).
//!
//! **Property 14: Fallback chooses the nearest qualifying ancestor.**
//! *For any* missing FileSystem or ZipFolder Location and *any* outcomes for
//! its ancestors (exists, missing, access denied, timeout), `choose_fallback`
//! returns `Redirected(a)` where *a* is the ancestor with the most path
//! components among those that exist, are accessible, and responded, or
//! `Omitted(NotExist)` if none qualify. *For any* Shell_Location other than
//! ZipFolder, a non-available outcome is always `Omitted`. `ancestors(p)`
//! lists only proper ancestors, ordered from deepest to shallowest, ending at
//! the drive root or UNC share root.
//!
//! **Validates: Requirements 9.1, 9.2, 8.9**
//!
//! Locations are generated in every recognised path form (drive, UNC,
//! `\\?\C:\`, `\\?\UNC\`), as ZIP folders, Shell and Search Locations, and in
//! unrecognised forms, with component names containing Unicode (NFC/NFD),
//! lone surrogates, spaces and dots at the ends, and `.zip` in odd case.
//! Recognised forms are checked against an independent oracle built while
//! generating the path; every form is checked against the structural
//! invariants (a), (b) and (g).

mod common;

use common::strategies::{all_location_kinds, location_id, location_of_kind, wide_string_of, ALL_SHELL_KINDS, ALL_VOLUMES};
use esr_core::model::*;
use esr_core::plan::fallback::*;
use proptest::collection::vec;
use proptest::prelude::*;
use proptest::sample::select;

const BS: u16 = 0x5C;
const SLASH: u16 = 0x2F;
const DOT: u16 = 0x2E;
const SPACE: u16 = 0x20;
const COLON: u16 = 0x3A;
/// `\\?\`
const VERBATIM: [u16; 4] = [BS, BS, 0x3F, BS];

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn is_fs_like(kind: LocationKind) -> bool {
    matches!(kind, LocationKind::FileSystem(_) | LocationKind::ZipFolder)
}

// ---------------------------------------------------------------------------
// Component names
// ---------------------------------------------------------------------------

const UNICODE_PIECES: &[&str] = &[
    "é", "e\u{301}", "Å", "A\u{30A}", "日本", "Ж", "İ", "ı", "ß", "Σ", "😀", "\u{FEFF}", "\u{A0}",
];
const ODD_PIECES: &[&str] = &["?", ":", "~", "$", "{", "}", "#", "%", "&", "'", ";", "[", "]", "UNC", "unc", "C:"];
/// Mostly none; `.zip` in odd case, near misses, and trailing dots/spaces.
const SUFFIXES: &[&str] = &[
    "", "", "", "", "", "", ".zip", ".ZIP", ".ZiP", ".zIp", ".zip.", ".zip ", ".zi", "zip", ".zipx",
    ".z\u{130}p", ".z\u{131}p", ".", " ", "..", ". ",
];

fn piece(verbatim: bool) -> BoxedStrategy<Vec<u16>> {
    let ascii = prop_oneof![b'a'..=b'z', b'A'..=b'Z', b'0'..=b'9'].prop_map(|b| vec![u16::from(b)]);
    let base = prop_oneof![
        8 => ascii,
        3 => select(UNICODE_PIECES).prop_map(w),
        1 => (0xD800u16..=0xDBFF).prop_map(|u| vec![u]),
        1 => (0xDC00u16..=0xDFFF).prop_map(|u| vec![u]),
        2 => Just(vec![SPACE]),
        2 => Just(vec![DOT]),
        1 => select(ODD_PIECES).prop_map(w),
    ];
    if verbatim {
        // Inside `\\?\` forms `/` is an ordinary character.
        prop_oneof![12 => base, 1 => Just(vec![SLASH])].boxed()
    } else {
        base.boxed()
    }
}

/// A non-empty component without separators that is not `.`, `..` or `?`.
fn component(verbatim: bool) -> BoxedStrategy<Vec<u16>> {
    (vec(piece(verbatim), 1..6), select(SUFFIXES))
        .prop_map(|(p, s)| {
            let mut c = p.concat();
            c.extend(w(s));
            c
        })
        .prop_filter("dot or device component", |c| c[..] != [DOT] && c[..] != [DOT, DOT] && c[..] != [0x3F])
        .boxed()
}

fn sep(verbatim: bool) -> BoxedStrategy<u16> {
    if verbatim {
        Just(BS).boxed()
    } else {
        prop_oneof![3 => Just(BS), 1 => Just(SLASH)].boxed()
    }
}

fn letter() -> impl Strategy<Value = u16> {
    prop_oneof![b'a'..=b'z', b'A'..=b'Z'].prop_map(u16::from)
}

// ---------------------------------------------------------------------------
// Recognised path forms, with the oracle recorded while building
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Root {
    units: Vec<u16>,
    /// Display name of the root ancestor (`C:` or the share name).
    name: Vec<u16>,
    unc: bool,
    verbatim: bool,
}

fn root() -> impl Strategy<Value = Root> {
    prop_oneof![
        3 => (letter(), sep(false)).prop_map(|(l, s)| Root {
            units: vec![l, COLON, s],
            name: vec![l, COLON],
            unc: false,
            verbatim: false,
        }),
        2 => letter().prop_map(|l| Root {
            units: [&VERBATIM[..], &[l, COLON, BS]].concat(),
            name: vec![l, COLON],
            unc: false,
            verbatim: true,
        }),
        2 => (sep(false), sep(false), component(false), sep(false), component(false)).prop_map(
            |(a, b, server, s, share)| Root {
                units: [&[a, b][..], &server, &[s], &share].concat(),
                name: share,
                unc: true,
                verbatim: false,
            }
        ),
        2 => (select(&["UNC", "unc", "Unc", "uNc"][..]), component(true), component(true)).prop_map(
            |(marker, server, share)| Root {
                units: [&VERBATIM[..], &w(marker), &[BS], &server, &[BS], &share].concat(),
                name: share,
                unc: true,
                verbatim: true,
            }
        ),
    ]
}

/// A path in a recognised form plus what `ancestors` must make of it.
#[derive(Clone, Debug)]
struct GenPath {
    units: Vec<u16>,
    /// `units[..root_end]` is the root ancestor's parsing name.
    root_end: usize,
    root_name: Vec<u16>,
    unc: bool,
    verbatim: bool,
    /// Spans of the components below the root.
    comps: Vec<(usize, usize)>,
}

impl GenPath {
    /// Expected `ancestor_paths`: deepest first, ending at the root.
    fn expected_paths(&self) -> Vec<&[u16]> {
        let n = self.comps.len();
        if n == 0 {
            return Vec::new();
        }
        let mut v: Vec<&[u16]> = (0..n - 1).rev().map(|k| &self.units[..self.comps[k].1]).collect();
        v.push(&self.units[..self.root_end]);
        v
    }

    /// Expected display names, parallel to [`Self::expected_paths`].
    fn expected_display(&self) -> Vec<&[u16]> {
        let n = self.comps.len();
        if n == 0 {
            return Vec::new();
        }
        let mut v: Vec<&[u16]> = (0..n - 1).rev().map(|k| &self.units[self.comps[k].0..self.comps[k].1]).collect();
        v.push(&self.root_name);
        v
    }

    /// Expected kinds, parallel to [`Self::expected_paths`] (module docs of
    /// `fallback`: ZIP ancestors inside the archive stay ZipFolder; the
    /// archive and everything above are FileSystem).
    fn expected_kinds(&self, kind: LocationKind) -> Vec<LocationKind> {
        let n = self.comps.len();
        if n == 0 {
            return Vec::new();
        }
        let fs = match kind {
            LocationKind::FileSystem(v) => LocationKind::FileSystem(v),
            _ => LocationKind::FileSystem(if self.unc { Volume::Unc } else { Volume::Fixed }),
        };
        let zip = kind == LocationKind::ZipFolder;
        let archive = if zip { self.comps.iter().rposition(|&(a, b)| is_zip(&self.units[a..b])) } else { None };
        let at = |k: usize| {
            if !zip || archive.is_some_and(|a| k <= a) {
                fs
            } else {
                LocationKind::ZipFolder
            }
        };
        (0..n - 1).rev().map(at).chain([fs]).collect()
    }

    /// Expected `probe_path`.
    fn expected_probe(&self) -> Vec<u16> {
        if !self.verbatim {
            return self.units.clone();
        }
        let normalised = if self.unc {
            [&[BS, BS][..], &self.units[8..]].concat()
        } else {
            self.units[4..].to_vec()
        };
        let safe = !normalised.contains(&SLASH)
            && normalised.split(|&u| u == BS).filter(|c| !c.is_empty()).all(|c| {
                c != [DOT] && c != [DOT, DOT] && !matches!(c.last(), Some(&DOT) | Some(&SPACE))
            });
        if safe { normalised } else { self.units.clone() }
    }
}

/// Ends in `.zip`, ASCII case-insensitive only.
fn is_zip(c: &[u16]) -> bool {
    c.len() >= 4
        && c[c.len() - 4..]
            .iter()
            .zip(b".zip")
            .all(|(&u, &b)| u8::try_from(u).is_ok_and(|x| x.to_ascii_lowercase() == b))
}

fn gen_path() -> impl Strategy<Value = GenPath> {
    root()
        .prop_flat_map(|r| {
            let v = r.verbatim;
            (Just(r), vec((vec(sep(v), 1..=2), component(v)), 0..7), vec(sep(v), 0..=2))
        })
        .prop_map(|(r, parts, trailing)| {
            let mut units = r.units.clone();
            let root_end = units.len();
            let mut comps = Vec::new();
            for (i, (seps, c)) in parts.into_iter().enumerate() {
                // A drive root already ends in a separator.
                let seps = if i == 0 && !r.unc { &seps[1..] } else { &seps[..] };
                units.extend_from_slice(seps);
                let start = units.len();
                units.extend(c);
                comps.push((start, units.len()));
            }
            units.extend(trailing);
            GenPath { units, root_end, root_name: r.name, unc: r.unc, verbatim: r.verbatim, comps }
        })
}

// ---------------------------------------------------------------------------
// Unrecognised forms
// ---------------------------------------------------------------------------

/// Prefixes that stay unrecognised whatever (separator-free) tail follows.
const BAD_WITH_TAIL: &[&str] = &[
    r"a\", "C:", r"C:x\", r"\x\", r"\\.\C:\", r"\\?\Volume{1234}\",
    r"::{20D04FE0-3AEA-1069-A2D8-08002B30309D}\", r"\\\srv\share\", r"\\srv\\share\",
    r"\\?\UNC\\share\", r"1:\", "search-ms:crumb=location:",
];
/// Unrecognised on their own.
const BAD_ALONE: &[&str] = &[
    "", "C", r"\\", r"\\srv", r"\\srv\", "//srv/", r"\\?\C:", r"\\?\UNC", r"\\?\UNC\srv", r"\\?\UNC\srv\", r"\\?\",
];

fn unrecognised() -> BoxedStrategy<Vec<u16>> {
    let tail = || vec(component(false), 0..4).prop_map(|cs| cs.join(&BS));
    prop_oneof![
        3 => (select(BAD_WITH_TAIL), tail()).prop_map(|(p, t)| [w(p), t].concat()),
        1 => select(BAD_ALONE).prop_map(w),
        // A recognised path with a `.` or `..` component.
        3 => (gen_path(), select(&[".", ".."][..]), tail()).prop_map(|(g, d, t)| {
            let mut u = g.units;
            u.push(BS);
            u.extend(w(d));
            if !t.is_empty() {
                u.push(BS);
                u.extend(t);
            }
            u
        }),
    ]
    .boxed()
}

// ---------------------------------------------------------------------------
// Locations and outcomes
// ---------------------------------------------------------------------------

fn make_loc(units: &[u16], kind: LocationKind, pidl: Vec<u8>) -> LocationId {
    let plain = |kind| LocationId {
        case_sensitive: false,
        kind,
        parsing_name: WideString::new(units.to_vec()),
        display_name: "shown".into(),
        pidl: pidl.clone(),
        search: None,
    };
    let mut loc = plain(kind);
    if kind == LocationKind::Search {
        loc.search = Some(SearchSpec { query: "q".into(), scope: Box::new(plain(LocationKind::FileSystem(Volume::Fixed))) });
    }
    loc
}

fn any_kind() -> impl Strategy<Value = LocationKind> {
    select(all_location_kinds())
}

fn fs_like_kind() -> impl Strategy<Value = LocationKind> {
    select(ALL_VOLUMES.iter().map(|&v| LocationKind::FileSystem(v)).chain([LocationKind::ZipFolder]).collect::<Vec<_>>())
}

fn shell_or_search_kind() -> impl Strategy<Value = LocationKind> {
    select(ALL_SHELL_KINDS.iter().map(|&k| LocationKind::Shell(k)).chain([LocationKind::Search]).collect::<Vec<_>>())
}

/// A recognised path with a location of any kind on it.
fn located() -> impl Strategy<Value = (GenPath, LocationId)> {
    (gen_path(), any_kind(), vec(any::<u8>(), 0..8)).prop_map(|(g, kind, pidl)| {
        let loc = make_loc(&g.units, kind, pidl);
        (g, loc)
    })
}

/// Arbitrary text, and arbitrary text behind the `\\?\` prefixes.
fn arbitrary_path() -> BoxedStrategy<Vec<u16>> {
    prop_oneof![
        2 => wide_string_of(0usize..=300).prop_map(|s| s.0),
        1 => wide_string_of(0usize..=60).prop_map(|s| [&VERBATIM[..], &s.0].concat()),
        1 => wide_string_of(0usize..=60).prop_map(|s| [&VERBATIM[..], &w(r"UNC\"), &s.0].concat()),
    ]
    .boxed()
}

/// Any path: recognised, unrecognised or arbitrary.
fn any_path() -> BoxedStrategy<Vec<u16>> {
    prop_oneof![
        4 => gen_path().prop_map(|g| g.units),
        2 => unrecognised(),
        2 => arbitrary_path(),
        1 => location_id().prop_map(|l| l.parsing_name.0),
    ]
    .boxed()
}

/// A location of any kind on any path, or from the shared strategy.
fn any_location() -> BoxedStrategy<LocationId> {
    prop_oneof![
        2 => location_id(),
        3 => (any_path(), any_kind()).prop_map(|(p, k)| make_loc(&p, k, vec![1, 2])),
    ]
    .boxed()
}

/// A missing FileSystem/ZipFolder Location with an arbitrary availability
/// mask over its ancestors, listed in arbitrary order.
fn missing_case() -> impl Strategy<Value = (LocationId, Vec<AncestorProbe>)> {
    let loc = prop_oneof![
        4 => (gen_path(), fs_like_kind()).prop_map(|(g, k)| make_loc(&g.units, k, vec![])),
        1 => fs_like_kind().prop_flat_map(location_of_kind),
    ];
    loc.prop_flat_map(|loc| {
        let anc = ancestors(&loc);
        let n = anc.len();
        let mask = vec(prop_oneof![3 => Just(false), 1 => Just(true)], n);
        let order = Just((0..n).collect::<Vec<_>>()).prop_shuffle();
        (Just(loc), Just(anc), mask, order)
    })
    .prop_map(|(loc, anc, mask, order)| {
        let probes = order.into_iter().map(|i| AncestorProbe { location: anc[i].clone(), available: mask[i] }).collect();
        (loc, probes)
    })
}

/// Any outcome other than `Available`; `Missing` carries ancestor probes of
/// some file-system path, often available.
fn non_available_outcome() -> impl Strategy<Value = ProbeOutcome> {
    let missing = (gen_path(), vec(any::<bool>(), 0..8)).prop_map(|(g, mask)| {
        let fs = make_loc(&g.units, LocationKind::FileSystem(Volume::Fixed), vec![]);
        let mut probes: Vec<AncestorProbe> = ancestors(&fs)
            .into_iter()
            .zip(mask.iter().copied().chain(std::iter::repeat(true)))
            .map(|(location, available)| AncestorProbe { location, available })
            .collect();
        // Also an available location that is not an ancestor at all.
        probes.push(AncestorProbe { location: fs, available: true });
        ProbeOutcome::Missing { ancestors: probes }
    });
    prop_oneof![
        3 => missing,
        1 => Just(ProbeOutcome::AccessDenied),
        1 => Just(ProbeOutcome::Timeout),
        1 => Just(ProbeOutcome::Unresolvable),
        1 => Just(ProbeOutcome::ShellUnavailable),
    ]
}

// ---------------------------------------------------------------------------
// Invariants
// ---------------------------------------------------------------------------

/// (a), (b), (g) for any location.
fn check_ancestor_invariants(loc: &LocationId) -> Result<(), TestCaseError> {
    let stored = loc.parsing_name.units();
    let got = ancestors(loc);
    if !is_fs_like(loc.kind) {
        prop_assert!(got.is_empty(), "Shell/Search has ancestors: {:?}", got);
        return Ok(());
    }
    let names: Vec<&[u16]> = got.iter().map(|a| a.parsing_name.units()).collect();
    prop_assert_eq!(&ancestor_paths(stored), &names);
    let Some(last) = got.last() else { return Ok(()) };

    let own = component_count(stored);
    prop_assert!(own.is_some(), "a path with ancestors must be recognised");
    let (mut prev_len, mut prev_count) = (stored.len(), own.unwrap());
    for a in &got {
        let u = a.parsing_name.units();
        // (a) strictly shallower, deepest first, a proper prefix; (g) exact units.
        prop_assert!(u.len() < prev_len, "not strictly shallower: {:?}", a);
        prop_assert_eq!(u, &stored[..u.len()]);
        // (b) one component fewer per step; a bare UNC server would be `None`.
        prop_assert_eq!(component_count(u), Some(prev_count - 1));
        let d = a.display_name.units();
        prop_assert!(!d.is_empty() && u.windows(d.len()).any(|x| x == d), "display name not from path: {:?}", a);
        prop_assert!(a.pidl.is_empty() && a.search.is_none());
        prop_assert!(is_fs_like(a.kind));
        prev_len = u.len();
        prev_count -= 1;
    }
    // Ends at the root, which has no ancestors of its own.
    prop_assert_eq!(component_count(last.parsing_name.units()), Some(1));
    prop_assert!(ancestor_paths(last.parsing_name.units()).is_empty());
    // The ancestors of the deepest ancestor are the rest of the list.
    prop_assert_eq!(&ancestor_paths(names[0])[..], &names[1..]);
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Property 14 (a), (b), (g): for every recognised form, `ancestors`
    /// matches the oracle (paths, display names, kinds), counts decrease by
    /// one per step down to the root, and Shell/Search have none.
    #[test]
    fn ancestors_match_oracle_for_recognised_forms((g, loc) in located()) {
        let n = g.comps.len();
        let expected = g.expected_paths();
        prop_assert_eq!(&ancestor_paths(&g.units), &expected);
        prop_assert_eq!(component_count(&g.units), Some(1 + n));
        for (i, p) in expected.iter().enumerate() {
            prop_assert_eq!(component_count(p), Some(n - i));
        }
        let got = ancestors(&loc);
        if is_fs_like(loc.kind) {
            let names: Vec<&[u16]> = got.iter().map(|a| a.parsing_name.units()).collect();
            prop_assert_eq!(&names, &expected);
            let display: Vec<&[u16]> = got.iter().map(|a| a.display_name.units()).collect();
            prop_assert_eq!(&display, &g.expected_display());
            let kinds: Vec<LocationKind> = got.iter().map(|a| a.kind).collect();
            prop_assert_eq!(kinds, g.expected_kinds(loc.kind));
        } else {
            prop_assert!(got.is_empty());
        }
        check_ancestor_invariants(&loc)?;
    }

    /// Property 14 (a), (b), (g) for every kind on any path, including
    /// arbitrary text and the shared location strategy.
    #[test]
    fn ancestor_invariants_hold_for_any_location(loc in any_location()) {
        check_ancestor_invariants(&loc)?;
    }

    /// Property 14: unrecognised forms have no ancestors, so a missing
    /// Location of such a form is omitted, never redirected.
    #[test]
    fn unrecognised_forms_have_no_ancestors(p in unrecognised(), kind in any_kind()) {
        prop_assert!(ancestor_paths(&p).is_empty());
        prop_assert_eq!(component_count(&p), None);
        let loc = make_loc(&p, kind, vec![]);
        prop_assert!(ancestors(&loc).is_empty());
        let got = choose_fallback(&loc, &ProbeOutcome::Missing { ancestors: vec![] });
        prop_assert!(matches!(got, Fallback::Omitted(OmitReason::NotExist | OmitReason::ShellUnavailable)), "{:?}", got);
    }

    /// Property 14 (c): a missing FileSystem/ZipFolder Location is redirected
    /// to the available ancestor with the most components, whatever order the
    /// probes are listed in, else omitted as NotExist (Req 9.1, 9.2).
    #[test]
    fn missing_redirects_to_nearest_available_ancestor((loc, probes) in missing_case()) {
        let depth = |p: &AncestorProbe| component_count(p.location.parsing_name.units()).unwrap();
        let expected = probes.iter().filter(|p| p.available).max_by_key(|p| depth(p));
        let got = choose_fallback(&loc, &ProbeOutcome::Missing { ancestors: probes.clone() });
        match (&got, expected) {
            (Fallback::Redirected(a), Some(e)) => {
                prop_assert!(a.identical(&e.location), "{:?} != {:?}", a, e.location);
                // The chosen ancestor is one of the ancestors, untouched.
                prop_assert!(ancestors(&loc).iter().any(|x| x.identical(a)));
            }
            (Fallback::Omitted(OmitReason::NotExist), None) => {}
            _ => prop_assert!(false, "got {:?}, expected {:?}", got, expected),
        }
    }

    /// Property 14 (d): a Shell_Location other than ZipFolder, and a search,
    /// is never redirected, even when available locations are reported
    /// (Req 8.9); `Missing` is `ShellUnavailable`.
    #[test]
    fn shell_and_search_are_never_redirected(
        loc in prop_oneof![
            shell_or_search_kind().prop_flat_map(location_of_kind),
            (any_path(), shell_or_search_kind()).prop_map(|(p, k)| make_loc(&p, k, vec![])),
        ],
        outcome in non_available_outcome(),
    ) {
        prop_assert!(ancestors(&loc).is_empty());
        let got = choose_fallback(&loc, &outcome);
        prop_assert!(matches!(got, Fallback::Omitted(_)), "{:?}", got);
        if matches!(outcome, ProbeOutcome::Missing { .. }) {
            prop_assert_eq!(got, Fallback::Omitted(OmitReason::ShellUnavailable));
        }
    }

    /// Property 14 (e), (f): for every kind, AccessDenied, Timeout,
    /// Unresolvable and ShellUnavailable map to their reasons, and Available
    /// is kept.
    #[test]
    fn other_outcomes_map_to_reasons_and_available_is_kept(loc in any_location()) {
        let cases = [
            (ProbeOutcome::AccessDenied, OmitReason::Inaccessible),
            (ProbeOutcome::Timeout, OmitReason::Timeout),
            (ProbeOutcome::Unresolvable, OmitReason::Unresolvable),
            (ProbeOutcome::ShellUnavailable, OmitReason::ShellUnavailable),
        ];
        for (outcome, reason) in cases {
            prop_assert_eq!(choose_fallback(&loc, &outcome), Fallback::Omitted(reason));
        }
        prop_assert_eq!(choose_fallback(&loc, &ProbeOutcome::Available), Fallback::Keep);
    }

    /// Property 14 (h): `probe_path` is idempotent, changes only `\\?\`
    /// forms, and when it does, only drops the prefix.
    #[test]
    fn probe_path_is_idempotent_and_only_touches_verbatim_forms(p in any_path()) {
        let once = probe_path(&p);
        prop_assert_eq!(&probe_path(&once), &once);
        if !p.starts_with(&VERBATIM) {
            prop_assert_eq!(&once, &p);
        } else if once != p {
            prop_assert!(!once.starts_with(&VERBATIM));
            let tail = if once.starts_with(&[BS, BS]) { &once[2..] } else { &once[..] };
            prop_assert!(tail.len() < p.len() && p.ends_with(tail), "{:?} -> {:?}", p, once);
            prop_assert_eq!(component_count(&once), component_count(&p));
        }
    }

    /// Property 14 (h): `probe_path` on recognised forms matches the oracle
    /// (prefix dropped exactly when Win32 would name the same item).
    #[test]
    fn probe_path_matches_oracle_for_recognised_forms(g in gen_path()) {
        prop_assert_eq!(probe_path(&g.units), g.expected_probe());
    }
}
