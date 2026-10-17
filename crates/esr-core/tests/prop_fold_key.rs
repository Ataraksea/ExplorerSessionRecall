//! Property test for the canonical location key (`LocationId::fold_key`).
//!
//! **Property 36: Canonical location key**
//!
//! **Validates: Requirements 14.20**

use esr_core::model::*;
use proptest::collection::vec;
use proptest::prelude::*;
use proptest::sample::select;

/// Characters whose simple uppercase mapping is a single BMP character and
/// whose lower/upper forms fold to the same key.
const CHARS: &[char] = &[
    'a', 'b', 'k', 'z', 'A', 'B', 'K', 'Z', '0', '7', ' ', '.', '_', '-', 'é', 'É', 'ü', 'ñ', 'ж', 'Ж',
    'ß', 'ω', 'Ω', 'σ', 'ς', 'Σ',
];

/// A non-root path in one of the forms the restore engine records.
#[derive(Clone, Debug)]
enum Base {
    Drive { letter: char, segs: Vec<String> },
    Unc { server: String, share: String, segs: Vec<String> },
}

impl Base {
    fn render(&self) -> String {
        match self {
            Base::Drive { letter, segs } => format!(r"{letter}:\{}", segs.join(r"\")),
            Base::Unc { server, share, segs } => format!(r"\\{server}\{share}\{}", segs.join(r"\")),
        }
    }

    fn is_drive(&self) -> bool {
        matches!(self, Base::Drive { .. })
    }
}

fn segment() -> impl Strategy<Value = String> {
    vec(select(CHARS), 1..8).prop_map(|c| c.into_iter().collect())
}

fn base() -> impl Strategy<Value = Base> {
    prop_oneof![
        3 => (select(vec!['C', 'd', 'Z', 'e']), vec(segment(), 1..4))
            .prop_map(|(letter, segs)| Base::Drive { letter, segs }),
        1 => (segment(), segment(), vec(segment(), 1..3))
            .prop_map(|(server, share, segs)| Base::Unc { server, share, segs }),
    ]
}

/// Changes the letter case of each character by `mask` (cycled), keeping a
/// character when the other case is not a single character.
fn recase(s: &str, mask: &[bool]) -> String {
    s.chars()
        .enumerate()
        .map(|(i, c)| {
            let mut mapped: Vec<char> =
                if mask[i % mask.len()] { c.to_uppercase().collect() } else { c.to_lowercase().collect() };
            if mapped.len() == 1 { mapped.remove(0) } else { c }
        })
        .collect()
}

fn mask() -> impl Strategy<Value = Vec<bool>> {
    vec(any::<bool>(), 1..64)
}

fn separators() -> impl Strategy<Value = String> {
    vec(select(vec!['\\', '/']), 0..4).prop_map(|s| s.into_iter().collect())
}

fn loc(kind: LocationKind, name: &str, case_sensitive: bool) -> LocationId {
    LocationId {
        kind,
        parsing_name: name.into(),
        display_name: WideString::default(),
        pidl: Vec::new(),
        search: None,
        case_sensitive,
    }
}

fn fs(name: &str, case_sensitive: bool) -> LocationId {
    loc(LocationKind::FileSystem(Volume::Fixed), name, case_sensitive)
}

fn same_key(a: &LocationId, b: &LocationId) -> bool {
    a.fold_key() == b.fold_key()
}

fn search(query: WideString, scope: LocationId) -> LocationId {
    LocationId {
        search: Some(SearchSpec { query, scope: Box::new(scope) }),
        ..loc(LocationKind::Search, "search-ms:x", false)
    }
}

/// The same Location with its parsing name (and search scope) replaced by the
/// normalized name of its key.
fn renormalized(l: &LocationId) -> LocationId {
    match l.fold_key() {
        LocationKey::Plain { parsing_name, .. } => {
            LocationId { parsing_name: WideString(parsing_name.into_owned()), ..l.clone() }
        }
        LocationKey::Search { .. } => {
            let spec = l.search.as_ref().expect("search key has a spec");
            LocationId {
                search: Some(SearchSpec { query: spec.query.clone(), scope: Box::new(renormalized(&spec.scope)) }),
                ..l.clone()
            }
        }
    }
}

/// Reference fold for ASCII-only names: upper-case, nothing else.
fn ascii_fold(b: &Base) -> String {
    b.render().to_ascii_uppercase()
}

fn ascii_segment() -> impl Strategy<Value = String> {
    "[A-Za-z0-9 ._-]{1,6}"
}

fn ascii_drive() -> impl Strategy<Value = Base> {
    (select(vec!['C', 'c', 'D', 'd']), vec(ascii_segment(), 1..3))
        .prop_map(|(letter, segs)| Base::Drive { letter, segs })
}

proptest! {
    /// Letter case, a `\\?\` prefix and trailing separators do not change the key.
    #[test]
    fn equivalent_forms_share_a_key(b in base(), m in mask(), prefix in any::<bool>(), tail in separators()) {
        let name = b.render();
        let prefix = if prefix && b.is_drive() { r"\\?\" } else { "" };
        let variant = format!("{prefix}{}{tail}", recase(&name, &m));
        prop_assert!(same_key(&fs(&name, false), &fs(&variant, false)));
    }

    /// The same holds for the scope of a search, and the query stays exact.
    #[test]
    fn search_scope_is_folded_and_query_is_exact(
        b in base(), m in mask(), prefix in any::<bool>(), tail in separators(),
        q in vec(select(vec!['a', 'A', 'b', ' ', 'é']), 0..6),
    ) {
        let query: WideString = q.into_iter().collect::<String>().as_str().into();
        let name = b.render();
        let prefix = if prefix && b.is_drive() { r"\\?\" } else { "" };
        let variant = format!("{prefix}{}{tail}", recase(&name, &m));
        prop_assert!(same_key(&search(query.clone(), fs(&name, false)), &search(query, fs(&variant, false))));
    }

    /// Re-keying a Location whose name is already normalized gives the same key.
    #[test]
    fn key_is_idempotent_on_normalized_forms(
        b in base(), m in mask(), prefix in any::<bool>(), tail in separators(),
        case_sensitive in any::<bool>(), as_search in any::<bool>(),
    ) {
        let prefix = if prefix && b.is_drive() { r"\\?\" } else { "" };
        let name = format!("{prefix}{}{tail}", recase(&b.render(), &m));
        let plain = fs(&name, case_sensitive);
        let l = if as_search { search("Query".into(), plain) } else { plain };
        let once = renormalized(&l);
        prop_assert!(same_key(&once, &l));
        prop_assert!(same_key(&renormalized(&once), &once));
    }

    /// Extra separators after a root keep exactly the root's one.
    #[test]
    fn roots_keep_one_separator(letter in select(vec!['C', 'd']), n in 1usize..4, m in mask()) {
        let canonical = fs(&format!(r"{letter}:\"), false);
        let variant = format!("{}{}", recase(&format!("{letter}:"), &m), "\\".repeat(n));
        prop_assert!(same_key(&fs(&variant, false), &canonical));
        let prefixed = format!(r"\\?\{}", variant);
        prop_assert!(same_key(&fs(&prefixed, false), &canonical));
    }

    /// Locations of different kind have different keys, even with the same name.
    #[test]
    fn different_kinds_have_different_keys(b in base(), i in 0usize..3, j in 0usize..3) {
        let kinds = [LocationKind::FileSystem(Volume::Fixed), LocationKind::ZipFolder, LocationKind::Shell(ShellKind::Other)];
        let name = b.render();
        let (a, c) = (loc(kinds[i], &name, false), loc(kinds[j], &name, false));
        prop_assert_eq!(a.fold_key() == c.fold_key(), i == j);
        // A search is never keyed like a plain Location.
        let s = search("q".into(), fs(&name, false));
        prop_assert_ne!(s.fold_key(), a.fold_key());
    }

    /// Locations whose case-folded names differ have different keys.
    #[test]
    fn different_folded_names_have_different_keys(x in ascii_drive(), y in ascii_drive()) {
        let same = ascii_fold(&x) == ascii_fold(&y);
        prop_assert_eq!(same_key(&fs(&x.render(), false), &fs(&y.render(), false)), same);
        let (sx, sy) = (search("q".into(), fs(&x.render(), false)), search("q".into(), fs(&y.render(), false)));
        prop_assert_eq!(sx.fold_key() == sy.fold_key(), same);
    }

    /// Searches with different query text (case included) have different keys.
    #[test]
    fn different_query_text_has_different_keys(
        b in base(),
        q1 in vec(select(vec!['a', 'A', 'b', 'B']), 0..4),
        q2 in vec(select(vec!['a', 'A', 'b', 'B']), 0..4),
    ) {
        let name = b.render();
        let (w1, w2): (WideString, WideString) =
            (q1.iter().collect::<String>().as_str().into(), q2.iter().collect::<String>().as_str().into());
        let (s1, s2) = (search(w1, fs(&name, false)), search(w2, fs(&name, false)));
        prop_assert_eq!(s1.fold_key() == s2.fold_key(), q1 == q2);
    }

    /// On a case-sensitive folder, names that differ in letter case have
    /// different keys; elsewhere they do not.
    #[test]
    fn letter_case_matters_only_when_case_sensitive(b in base(), m in mask()) {
        let name = b.render();
        let other = recase(&name, &m);
        let same = name == other;
        prop_assert_eq!(same_key(&fs(&name, true), &fs(&other, true)), same);
        prop_assert!(same_key(&fs(&name, false), &fs(&other, false)));
        // A case-sensitive search scope is compared exactly as well.
        let (s1, s2) = (search("q".into(), fs(&name, true)), search("q".into(), fs(&other, true)));
        prop_assert_eq!(s1.fold_key() == s2.fold_key(), same);
    }
}
