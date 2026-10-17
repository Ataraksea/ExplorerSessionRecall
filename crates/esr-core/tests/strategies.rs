//! Self-checks for the shared strategies in `tests/common/strategies.rs`:
//! generated values are valid by construction and cover the intended space.
//!
//! **Validates: Requirements 8.4, 11.5**

mod common;

use std::collections::HashSet;

use common::strategies::*;
use esr_core::engine::DesktopView;
use esr_core::model::*;
use esr_sim::fold::FoldFaults;
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::TestRunner;

/// Draws `n` values from `s` with a deterministic runner.
fn sample<S: Strategy>(s: S, n: usize) -> Vec<S::Value> {
    let mut runner = TestRunner::deterministic();
    (0..n).map(|_| s.new_tree(&mut runner).unwrap().current()).collect()
}

fn len_allowed(len: usize) -> bool {
    matches!(len, 0..=40 | 259..=261 | MAX_WIDE_LEN)
}

fn is_high(u: u16) -> bool {
    (0xD800..=0xDBFF).contains(&u)
}

fn is_low(u: u16) -> bool {
    (0xDC00..=0xDFFF).contains(&u)
}

fn has_unpaired_surrogate(s: &[u16]) -> bool {
    String::from_utf16(s).is_err()
}

fn assert_displays_ok(ds: &[Display]) {
    assert!((1..=4).contains(&ds.len()));
    assert_eq!(ds.iter().filter(|d| d.primary).count(), 1, "exactly one primary");
    let primary = ds.iter().find(|d| d.primary).unwrap();
    assert_eq!((primary.monitor.left, primary.monitor.top), (0, 0));
    for (i, d) in ds.iter().enumerate() {
        assert!(d.monitor.is_positive() && d.work.is_positive());
        assert_eq!(d.work.intersect(&d.monitor), Some(d.work), "work inside monitor");
        for e in &ds[i + 1..] {
            assert_ne!(d.device, e.device);
            assert_eq!(d.monitor.intersect(&e.monitor), None, "monitors overlap");
        }
    }
}

proptest! {
    #[test]
    fn wide_strings_have_allowed_lengths(s in wide_string()) {
        prop_assert!(len_allowed(s.len()), "len {}", s.len());
    }

    #[test]
    fn locations_are_well_formed(loc in location_id()) {
        prop_assert_eq!(loc.kind == LocationKind::Search, loc.search.is_some());
        if let Some(spec) = &loc.search {
            prop_assert!(spec.scope.kind != LocationKind::Search && spec.scope.search.is_none());
        }
        for s in [&loc.parsing_name, &loc.display_name] {
            prop_assert!(s.len() <= MAX_WIDE_LEN);
        }
    }

    #[test]
    fn displays_are_consistent(ds in displays()) {
        assert_displays_ok(&ds);
    }

    #[test]
    fn snapshots_are_valid((ds, snap) in displays_and_snapshot()) {
        assert_displays_ok(&ds);
        prop_assert_eq!(validate(&snap), Ok(()));
        let devices: HashSet<_> = ds.iter().map(|d| d.device.clone()).collect();
        for w in &snap.windows {
            let r = &w.placement.display;
            // Either a current display (with its monitor) or a vanished one.
            if devices.contains(&r.device) {
                prop_assert!(ds.iter().any(|d| d.device == r.device && d.monitor == r.monitor));
            }
        }
    }

    #[test]
    fn docs_are_valid(doc in session_doc()) {
        prop_assert_eq!(doc.version, FORMAT_VERSION);
        prop_assert!(doc.saved_at.is_valid());
        prop_assert_eq!(validate_doc(&doc), Ok(()));
    }

    #[test]
    fn timestamps_are_valid(t in timestamp()) {
        prop_assert!(t.is_valid(), "{:?}", t);
    }

    #[test]
    fn availability_covers_all_targets((_, snap, avail) in restore_scenario()) {
        let targets = probe_targets(&snap);
        prop_assert_eq!(avail.len(), targets.len());
        for loc in &targets {
            let r = avail.get(loc).expect("every target probed");
            match r.outcome {
                TestProbeOutcome::Timeout => prop_assert_eq!(r.latency_ms, PROBE_DEADLINE_MS),
                _ => prop_assert!(r.latency_ms < PROBE_DEADLINE_MS),
            }
            let fs_like = matches!(loc.kind, LocationKind::FileSystem(_) | LocationKind::ZipFolder);
            match r.outcome {
                TestProbeOutcome::Missing | TestProbeOutcome::AccessDenied => prop_assert!(fs_like),
                TestProbeOutcome::ShellUnavailable => prop_assert!(!fs_like),
                _ => {}
            }
        }
        for w in &snap.windows {
            for t in &w.tabs {
                if let Some(spec) = &t.location.search {
                    prop_assert!(avail.contains_key(&spec.scope));
                }
            }
        }
    }
}

#[test]
fn wide_strings_cover_the_weighted_classes() {
    let all = sample(wide_string(), 2000);
    let lens: HashSet<usize> = all.iter().map(|s| s.len()).collect();
    for l in [0, 1, 259, 260, 261, MAX_WIDE_LEN] {
        assert!(lens.contains(&l), "length {l} never generated");
    }
    let huge = all.iter().filter(|s| s.len() == MAX_WIDE_LEN).count();
    assert!(huge < all.len() / 10, "32,767-unit strings should be rare ({huge})");

    let units: Vec<u16> = all.iter().flat_map(|s| s.units().iter().copied()).collect();
    let has = |u: u16| units.contains(&u);
    assert!(units.iter().any(|&u| u8::try_from(u).is_ok_and(|b| b.is_ascii_alphabetic())), "letters");
    for c in *b"%= " {
        assert!(has(u16::from(c)), "{:?}", c as char);
    }
    assert!(units.iter().any(|&u| u < 0x20 || u == 0x7F), "controls");
    assert!(units.contains(&0x0301), "NFD combining mark");
    assert!(units.contains(&0x00E9) || units.contains(&0x00C5), "NFC precomposed");
    assert!(units.iter().any(|&u| is_high(u)) && units.iter().any(|&u| is_low(u)));
    assert!(all.iter().any(|s| has_unpaired_surrogate(s.units())), "unpaired surrogates");
}

#[test]
fn locations_cover_every_kind() {
    let kinds: HashSet<LocationKind> = sample(location_id(), 3000).into_iter().map(|l| l.kind).collect();
    for k in all_location_kinds() {
        assert!(kinds.contains(&k), "{k:?} never generated");
    }
}

#[test]
fn displays_cover_counts_and_negative_coordinates() {
    let all = sample(displays(), 500);
    let counts: HashSet<usize> = all.iter().map(Vec::len).collect();
    assert_eq!(counts, (1..=4).collect());
    let monitors = || all.iter().flatten().map(|d| d.monitor);
    assert!(monitors().any(|m| m.left < 0), "negative x");
    assert!(monitors().any(|m| m.top < 0), "negative y");
}

#[test]
fn snapshots_at_caps_are_valid() {
    let all = sample(snapshot_with(SnapshotParams::at_caps()), 8);
    for s in &all {
        assert_eq!(validate(s), Ok(()));
    }
    assert!(all.iter().any(|s| s.tab_count() > 100), "large snapshots generated");
    assert!(sample(snapshot(), 200).iter().any(|s| s.windows.is_empty()), "empty snapshot");
}

#[test]
fn availability_covers_every_outcome() {
    let outcomes: HashSet<TestProbeOutcome> = sample(restore_scenario(), 300)
        .into_iter()
        .flat_map(|(_, _, a)| a.into_values().map(|r| r.outcome))
        .collect();
    use TestProbeOutcome::*;
    for o in [Available, Missing, AccessDenied, Timeout, Unresolvable, ShellUnavailable] {
        assert!(outcomes.contains(&o), "{o:?} never generated");
    }
}

// ---------------------------------------------------------------------------
// Fold generators (Req 14.20)
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn fold_scenarios_are_consistent(sc in fold_scenario()) {
        let keys = sc.keys();
        let distinct: HashSet<_> = keys.iter().collect();
        prop_assert_eq!(distinct.len(), keys.len(), "tracked keys are distinct");
        prop_assert!(keys.len() <= 6);

        for w in &sc.windows {
            prop_assert!((1..=5).contains(&w.tabs.len()));
            if let Some(a) = w.active {
                prop_assert!(a < w.tabs.len());
            }
            for t in &w.tabs {
                prop_assert!(!t.deferred || t.location.is_some(), "a Deferred_Tab has a Location");
            }
        }
        // Opening_Order holds every tracked window once, plus at most two others.
        let opening: HashSet<_> = sc.opening.iter().collect();
        prop_assert_eq!(opening.len(), sc.opening.len());
        prop_assert!(keys.iter().all(|k| opening.contains(k)));
        prop_assert!(sc.opening.len() <= keys.len() + 2);
        // Activation_Order is a permutation of the tracked windows.
        let (mut a, mut b) = (sc.activation.clone(), keys.clone());
        a.sort();
        b.sort();
        prop_assert_eq!(a, b);
        // The references name tracked windows.
        prop_assert!(sc.foreground.is_none_or(|f| keys.contains(&f)));
        prop_assert!(sc.exclude.is_none_or(|f| keys.contains(&f)));
        prop_assert!(sc.unresponsive.iter().all(|k| keys.contains(k)));
        prop_assert!(sc.pending_folds.iter().all(|(k, _)| keys.contains(k)));
        // The lent context mirrors the owned data.
        sc.with_ctx(|ctx| {
            assert_eq!(ctx.windows.len(), sc.windows.len());
            assert_eq!(ctx.candidate_key, sc.candidate.fold_key());
            for (fw, w) in ctx.windows.iter().zip(&sc.windows) {
                assert_eq!((fw.win, fw.active, fw.closing, fw.tabs.len()), (w.win, w.active, w.closing, w.tabs.len()));
            }
            assert_eq!(ctx.pending_folds.len(), sc.pending_folds.len());
        });
    }

    #[test]
    fn desktop_views_are_valid(d in desktop_view()) {
        if let DesktopView::Known(set) = d {
            prop_assert!(set.len() <= 8);
        }
    }
}

#[test]
fn pool_spellings_share_a_key_and_entries_do_not() {
    for i in 0..FOLD_POOL_SIZE {
        for j in 0..FOLD_POOL_SIZE {
            for s in 0..FOLD_SPELLINGS {
                let (a, b) = (pool_location(i, 0), pool_location(j, s));
                assert_eq!(a.fold_key() == b.fold_key(), i == j, "entries {i}/{j}, spelling {s}");
            }
        }
    }
}

#[test]
fn fold_scenarios_cover_the_interesting_cases() {
    let all = sample(fold_scenario(), 1000);
    let any = |p: &dyn Fn(&FoldScenario) -> bool| all.iter().any(p);
    let tabs = |sc: &FoldScenario| sc.windows.iter().flat_map(|w| w.tabs.clone()).collect::<Vec<_>>();

    assert!(any(&|s| s.windows.is_empty()), "no tracked windows");
    assert!(any(&|s| s.windows.len() == 6), "six tracked windows");
    assert!(any(&|s| matches!(s.desktop, DesktopView::Unknown)), "Unknown desktop");
    assert!(any(&|s| matches!(&s.desktop, DesktopView::Known(k) if k.len() < s.windows.len())), "off-desktop window");
    assert!(any(&|s| s.windows.iter().any(|w| w.closing)), "closing window");
    assert!(any(&|s| s.windows.iter().any(|w| w.active.is_none())), "unknown Active_Tab");
    assert!(any(&|s| tabs(s).iter().any(|t| t.deferred)), "Deferred_Tab");
    assert!(any(&|s| tabs(s).iter().any(|t| t.location.is_none())), "tab without a Location");
    assert!(any(&|s| s.foreground.is_some()) && any(&|s| s.foreground.is_none()), "foreground");
    assert!(any(&|s| s.exclude.is_some()) && any(&|s| s.exclude.is_none()), "exclude");
    assert!(any(&|s| !s.unresponsive.is_empty()), "unresponsive windows");
    assert!(any(&|s| !s.pending_folds.is_empty()), "pending Folded_Tabs");
    assert!(any(&|s| s.opening.len() > s.windows.len()), "untracked open window");
    // Equal keys: the candidate's key occurs on a tab, and two tabs share a key.
    assert!(
        any(&|s| tabs(s).iter().any(|t| t.location.as_ref().is_some_and(|l| l.fold_key() == s.candidate.fold_key()))),
        "a Matching_Tab"
    );
    assert!(
        any(&|s| {
            let all_tabs = tabs(s);
            let keys: Vec<_> = all_tabs.iter().filter_map(|t| t.location.as_ref().map(LocationId::fold_key)).collect();
            keys.iter().enumerate().any(|(i, k)| keys[i + 1..].contains(k))
        }),
        "two tabs with an equal key"
    );
    assert!(
        any(&|s| s.pending_folds.iter().any(|(_, l)| l.fold_key() == s.candidate.fold_key())),
        "a pending Folded_Tab with the candidate's key"
    );
    // Not every context has a match.
    assert!(
        any(&|s| !tabs(s).iter().any(|t| t.location.as_ref().is_some_and(|l| l.fold_key() == s.candidate.fold_key()))),
        "no Matching_Tab"
    );
}

// ---------------------------------------------------------------------------
// Burst and failure-schedule generators (Req 14.19, 14.20)
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn bursts_are_consistent(b in fold_burst()) {
        prop_assert!((1..=6).contains(&b.len()));
        // The arrival order is a permutation of the creation order.
        let mut arrival = b.arrival_order();
        arrival.sort();
        prop_assert_eq!(arrival, (0..b.len()).collect::<Vec<_>>());
        // Equality groups hold candidates with equal keys, and only those.
        for g in b.equalities() {
            prop_assert!(g.len() >= 2 && g.windows(2).all(|w| w[0] < w[1]));
            prop_assert!(g.iter().all(|&i| b.location(i).fold_key() == b.location(g[0]).fold_key()));
        }
        for i in 0..b.len() {
            for j in 0..b.len() {
                let equal = b.location(i).fold_key() == b.location(j).fold_key();
                prop_assert_eq!(equal, b.candidates[i].slot == b.candidates[j].slot);
            }
        }
        // Three simulator operations per candidate.
        prop_assert_eq!(b.sim_ops().len(), 3 * b.len());
    }

    #[test]
    fn failure_schedules_stay_within_their_commands((b, f) in fold_burst_with_faults()) {
        let limit = 3 * b.len() as u64;
        prop_assert!(f.create_tab.keys().chain(f.activate_tab.keys()).chain(f.ignore_close.iter()).all(|&n| n < limit));
    }
}

#[test]
fn bursts_cover_the_interesting_cases() {
    let all = sample(fold_burst(), 500);
    let any = |p: &dyn Fn(&FoldBurst) -> bool| all.iter().any(p);

    assert!(any(&|b| b.len() == 1) && any(&|b| b.len() == 6), "burst sizes");
    assert!(any(&|b| !b.equalities().is_empty()), "equal Locations");
    assert!(any(&|b| b.equalities().is_empty() && b.len() > 1), "all Locations different");
    assert!(
        any(&|b| b.equalities().iter().flatten().any(|&i| b.candidates[i].spelling != 0)),
        "equal Locations in different spellings"
    );
    assert!(
        any(&|b| b.arrival_order() != (0..b.len()).collect::<Vec<_>>()),
        "Locations arriving in another order than the creation order"
    );
    assert!(any(&|b| b.arrival_order() == (0..b.len()).collect::<Vec<_>>() && b.len() > 1), "arrival in creation order");
    assert!(any(&|b| b.candidates.iter().any(|c| !c.selection.is_empty())), "a selection");
    assert!(any(&|b| b.candidates.iter().any(|c| c.foreground.is_some())), "a foreground window");
    assert!(any(&|b| b.candidates.iter().any(|c| c.gap_ms == 0)), "candidates at the same moment");
    assert!(any(&|b| b.candidates.iter().any(|c| c.gap_ms > 60)), "a slow burst");
}

#[test]
fn failure_schedules_cover_every_kind_of_fault() {
    use esr_sim::fold::{ActivateMode, TabWindowFault};
    let all = sample(fold_failure_schedule(), 500);
    let any = |p: &dyn Fn(&FoldFaults) -> bool| all.iter().any(p);
    let creates = |f: &FoldFaults, p: &dyn Fn(&esr_sim::fold::CreateTabFault) -> bool| f.create_tab.values().any(p);
    let activates = |f: &FoldFaults, p: &dyn Fn(&esr_sim::fold::ActivateFault) -> bool| f.activate_tab.values().any(p);

    assert!(any(&|f| *f == FoldFaults::none()), "no fault at all");
    assert!(any(&|f| creates(f, &|c| c.tab_window == TabWindowFault::Never)), "tab window never");
    assert!(any(&|f| creates(f, &|c| matches!(c.tab_window, TabWindowFault::Late { .. }))), "tab window late");
    assert!(any(&|f| creates(f, &|c| c.registration_delay_ms > 0)), "delayed registration");
    assert!(any(&|f| creates(f, &|c| c.target_closes_after_ms.is_some())), "target closes during a create");
    assert!(any(&|f| activates(f, &|a| a.mode == ActivateMode::Unconfirmed)), "unconfirmed activation");
    assert!(any(&|f| activates(f, &|a| a.mode == ActivateMode::Ignored)), "ignored activation");
    assert!(any(&|f| activates(f, &|a| matches!(a.mode, ActivateMode::Late { .. }))), "late confirmation");
    assert!(any(&|f| activates(f, &|a| a.target_closes_after_ms.is_some())), "target closes during an activation");
    assert!(any(&|f| !f.ignore_close.is_empty()), "ignored CloseWindow by ordinal");
    assert!(any(&|f| f.ignore_first_close), "candidates ignoring their first CloseWindow");
}
