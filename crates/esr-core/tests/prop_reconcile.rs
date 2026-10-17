//! Property test for `TabOrderReconciler::reconcile` (`esr_core::tracker::reconcile`).

use std::collections::{HashMap, HashSet};

use esr_core::engine::{TabKey, TabStripState};
use esr_core::model::WideString;
use esr_core::tracker::{Reconciled, ScannedTab, TabOrderReconciler};
use proptest::collection::vec;
use proptest::option;
use proptest::prelude::*;
use proptest::sample::{select, Index};

/// Which match names a tab carries.
#[derive(Clone, Copy, Debug)]
enum Kind {
    /// Display name `F<id>` and parsing name `C:\F<id>`.
    Both,
    /// Display name only (e.g. a tab whose Location is not known yet).
    DisplayOnly,
    /// No match names: never matched to a UIA item.
    NoNames,
}

/// Which items of the visual tab order the UIA strip exposes.
#[derive(Clone, Debug)]
enum Exposure {
    Full,
    /// Contiguous overflow window, bounds drawn from `0..=n`.
    Window(Index, Index),
    /// Arbitrary subsequence.
    Subset(Vec<bool>),
}

/// UIA names that match no tab (extra strip items).
const GHOSTS: &[&str] = &["Ghost", "Home", "", "Quick access"];

/// UIA names shown for a tab with no match names: ghosts or names of other tabs.
const NO_NAME_LABELS: &[&str] = &["Ghost", "Home", "", "F0", "F1", r"C:\F2", "F3"];

#[derive(Clone, Debug)]
struct Case {
    /// Ground truth: tabs in left-to-right visual order.
    visual: Vec<ScannedTab>,
    prev: Vec<TabKey>,
    /// Rescan result in enumeration order (may repeat an entry).
    scanned: Vec<ScannedTab>,
    strip: Option<TabStripState>,
    /// For each strip item, the visual index it was taken from (`None` for
    /// extra unknown items).
    strip_src: Vec<Option<usize>>,
    /// The strip exposes every visual tab.
    full: bool,
}

fn key(id: u64) -> TabKey {
    TabKey {
        hwnd: 0x1_0000 + id,
        seq: id,
    }
}

fn w(s: &str) -> WideString {
    s.into()
}

fn case() -> impl Strategy<Value = Case> {
    (0usize..=12, any::<bool>()).prop_flat_map(|(n, unique)| {
        // Unique mode draws distinct folder ids; otherwise a 4-name pool makes
        // duplicates frequent.
        let folders = if unique {
            Just((0..16u8).collect::<Vec<_>>())
                .prop_shuffle()
                .prop_map(move |v| v[..n].to_vec())
                .boxed()
        } else {
            vec(0u8..4, n).boxed()
        };
        let kinds = vec(
            prop_oneof![
                6 => Just(Kind::Both),
                2 => Just(Kind::DisplayOnly),
                1 => Just(Kind::NoNames),
            ],
            n,
        );
        let no_name_labels = vec(select(NO_NAME_LABELS), n);
        let tabs = (
            folders,
            kinds,
            no_name_labels,
            any::<bool>(), // UIA shows parsing names (26200) or display names (22621)
            Just((1..=n as u64).collect::<Vec<_>>()).prop_shuffle(),
        );
        let scan = (
            Just((0..n).collect::<Vec<_>>()).prop_shuffle(),
            vec(any::<Index>(), 0..2), // repeated enumeration entries
        );
        let prev = (
            Just((0..n).collect::<Vec<_>>()).prop_shuffle(),
            vec(prop::bool::weighted(0.75), n),
            vec((any::<Index>(), 100u64..104), 0..3), // stale keys
            vec(any::<Index>(), 0..2),                // repeated keys
        );
        let strip = (
            option::weighted(
                0.8,
                prop_oneof![
                    2 => Just(Exposure::Full),
                    1 => (any::<Index>(), any::<Index>()).prop_map(|(a, b)| Exposure::Window(a, b)),
                    1 => vec(any::<bool>(), n).prop_map(Exposure::Subset),
                ],
            ),
            vec((any::<Index>(), select(GHOSTS)), 0..3),
            option::of(any::<Index>()),
        );
        (tabs, scan, prev, strip).prop_map(move |t| build(n, t))
    })
}

type TabsPart = (Vec<u8>, Vec<Kind>, Vec<&'static str>, bool, Vec<u64>);
type ScanPart = (Vec<usize>, Vec<Index>);
type PrevPart = (Vec<usize>, Vec<bool>, Vec<(Index, u64)>, Vec<Index>);
type StripPart = (Option<Exposure>, Vec<(Index, &'static str)>, Option<Index>);

fn build(n: usize, (tabs, scan, prev, strip): (TabsPart, ScanPart, PrevPart, StripPart)) -> Case {
    let (folders, kinds, labels, parsing_mode, ids) = tabs;
    let mut visual = Vec::with_capacity(n);
    let mut uia_name = Vec::with_capacity(n);
    for i in 0..n {
        let display = format!("F{}", folders[i]);
        let parsing = format!(r"C:\F{}", folders[i]);
        let (names, shown) = match kinds[i] {
            Kind::Both => (
                vec![w(&display), w(&parsing)],
                if parsing_mode { parsing } else { display },
            ),
            Kind::DisplayOnly => (vec![w(&display)], display),
            Kind::NoNames => (vec![], labels[i].to_owned()),
        };
        visual.push(ScannedTab::new(key(ids[i]), names));
        uia_name.push(w(&shown));
    }

    let (enum_perm, repeats) = scan;
    let mut scanned: Vec<ScannedTab> = enum_perm.iter().map(|&i| visual[i].clone()).collect();
    if n > 0 {
        for r in repeats {
            let entry = scanned[r.index(n)].clone();
            scanned.push(entry);
        }
    }

    let (prev_perm, keep, stale, prev_repeats) = prev;
    let mut prev: Vec<TabKey> = prev_perm
        .iter()
        .filter(|&&i| keep[i])
        .map(|&i| visual[i].tab)
        .collect();
    for (at, id) in stale {
        let at = at.index(prev.len() + 1);
        prev.insert(at, key(id));
    }
    if !prev.is_empty() {
        for r in prev_repeats {
            let k = prev[r.index(prev.len())];
            prev.push(k);
        }
    }

    let (exposure, extras, selected) = strip;
    let (strip, strip_src, full) = match exposure {
        None => (None, Vec::new(), false),
        Some(exposure) => {
            let exposed: Vec<usize> = match exposure {
                Exposure::Full => (0..n).collect(),
                Exposure::Window(a, b) => {
                    let (a, b) = (a.index(n + 1), b.index(n + 1));
                    (a.min(b)..a.max(b)).collect()
                }
                Exposure::Subset(mask) => (0..n).filter(|&i| mask[i]).collect(),
            };
            let full = exposed.len() == n;
            let mut src: Vec<Option<usize>> = exposed.into_iter().map(Some).collect();
            let mut names: Vec<WideString> =
                src.iter().map(|s| uia_name[s.unwrap()].clone()).collect();
            for (at, ghost) in extras {
                let at = at.index(names.len() + 1);
                names.insert(at, w(ghost));
                src.insert(at, None);
            }
            let selected = selected
                .filter(|_| !names.is_empty())
                .map(|i| i.index(names.len()));
            (Some(TabStripState { names, selected }), src, full)
        }
    };

    Case {
        visual,
        prev,
        scanned,
        strip,
        strip_src,
        full,
    }
}

/// Previously known scanned tabs in previous order, then new tabs in
/// enumeration order, each key once.
fn base_order(prev: &[TabKey], scanned: &[ScannedTab]) -> Vec<TabKey> {
    let scanned_keys: HashSet<TabKey> = scanned.iter().map(|s| s.tab).collect();
    let mut seen = HashSet::new();
    let mut base = Vec::new();
    for k in prev {
        if scanned_keys.contains(k) && seen.insert(*k) {
            base.push(*k);
        }
    }
    for s in scanned {
        if seen.insert(s.tab) {
            base.push(s.tab);
        }
    }
    base
}

fn position_map(order: &[TabKey]) -> HashMap<TabKey, usize> {
    order.iter().enumerate().map(|(i, k)| (*k, i)).collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// **Property 2: Tab order reconciliation yields a permutation that honours UIA.**
    ///
    /// For any previous ordered tab list, scanned tab set, and UIA name
    /// sequence, `reconcile` returns a permutation of the scanned tab set.
    /// When the match names are pairwise distinct and the UIA names cover the
    /// scanned tabs, the result equals the UIA order. Also checked: matched
    /// tabs follow the strip order, indistinguishable tabs keep their previous
    /// relative order (known before new), without UIA the previous order is
    /// kept and new tabs are appended, and the item-to-tab mapping is a valid
    /// partial matching.
    ///
    /// **Validates: Requirements 2.1, 2.2**
    #[test]
    fn reconcile_is_a_permutation_that_honours_uia(c in case()) {
        let Reconciled { order, uia_tabs, selected } =
            TabOrderReconciler::reconcile(&c.prev, &c.scanned, c.strip.as_ref());

        let names_of: HashMap<TabKey, &[WideString]> = c
            .scanned
            .iter()
            .map(|s| (s.tab, s.match_names.as_slice()))
            .collect();
        let base = base_order(&c.prev, &c.scanned);
        let pos = position_map(&order);

        // (a) Permutation of the distinct scanned keys.
        let mut got = order.clone();
        got.sort();
        let mut want: Vec<TabKey> = names_of.keys().copied().collect();
        want.sort();
        prop_assert_eq!(got, want, "order {:?} is not a permutation of the scan", order);

        match &c.strip {
            None => {
                // (e) Without UIA: previous order, then new tabs in enumeration order.
                prop_assert_eq!(&order, &base);
                prop_assert!(uia_tabs.is_empty());
                prop_assert_eq!(selected, None);
            }
            Some(strip) => {
                // (f) One entry per item; each tab at most once; only by a match name.
                prop_assert_eq!(uia_tabs.len(), strip.names.len());
                let mut used = HashSet::new();
                for (i, t) in uia_tabs.iter().enumerate() {
                    if let Some(t) = t {
                        prop_assert!(used.insert(*t), "tab {:?} mapped twice", t);
                        let names = names_of.get(t);
                        prop_assert!(names.is_some(), "item {} mapped to unscanned {:?}", i, t);
                        prop_assert!(
                            names.unwrap().contains(&strip.names[i]),
                            "item {} ({:?}) mapped to {:?} without that match name", i, strip.names[i], t
                        );
                    }
                }

                // (c) Matched tabs appear in strip order.
                let matched: Vec<usize> = uia_tabs.iter().flatten().map(|t| pos[t]).collect();
                prop_assert!(
                    matched.windows(2).all(|p| p[0] < p[1]),
                    "matched tabs {:?} out of strip order in {:?}", uia_tabs, order
                );

                prop_assert_eq!(
                    selected,
                    strip.selected.and_then(|i| uia_tabs.get(i).copied().flatten())
                );

                // (b) Pairwise distinct non-empty match names, full strip: UIA order.
                let mut all_names = HashSet::new();
                let distinct = c.visual.iter().all(|s| {
                    !s.match_names.is_empty()
                        && s.match_names.iter().all(|n| all_names.insert(n.clone()))
                });
                if distinct && c.full {
                    let truth: Vec<TabKey> = c.visual.iter().map(|s| s.tab).collect();
                    prop_assert_eq!(&order, &truth);
                    let want_uia: Vec<Option<TabKey>> =
                        c.strip_src.iter().map(|s| s.map(|i| c.visual[i].tab)).collect();
                    prop_assert_eq!(&uia_tabs, &want_uia);
                    let want_sel = strip
                        .selected
                        .and_then(|i| c.strip_src[i])
                        .map(|i| c.visual[i].tab);
                    prop_assert_eq!(selected, want_sel);
                }
            }
        }

        // (d) Tabs with the same (non-empty) match names cannot be told apart:
        // they keep their base relative order, i.e. previous order for known
        // tabs, and newly seen ones after the known ones.
        let sorted_names = |k: &TabKey| {
            let mut v = names_of[k].to_vec();
            v.sort();
            v
        };
        for (i, a) in base.iter().enumerate() {
            let na = sorted_names(a);
            if na.is_empty() {
                continue;
            }
            for b in &base[i + 1..] {
                if sorted_names(b) == na {
                    prop_assert!(
                        pos[a] < pos[b],
                        "same-name tabs {:?} and {:?} swapped in {:?} (base {:?})", a, b, order, base
                    );
                }
            }
        }
    }
}
