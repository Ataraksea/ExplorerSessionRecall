//! Property test for `esr_core::plan::defer::choose_deferred`.
//!
//! **Property 30: Deferral keeps the distinct-folder budget**
//!
//! *For any* snapshot, launch, and budget *b*: every saved Active_Tab, and
//! every tab showing the requested Location of a Targeted_Launch, is opened
//! immediately; the immediately opened tabs show at most
//! max(*b*, number of distinct Active_Tab and requested Locations) distinct
//! Locations, which is the reported `immediate` count; a deferred tab's
//! Location is shown by no immediate tab; tabs are deferred only once the
//! budget is used up; and, in saved Activation_Order and left-to-right
//! order, the Locations opened immediately beyond the Active_Tabs are a
//! prefix of the Locations met for the first time.
//!
//! **Validates: Requirements 5.13**

use std::collections::{BTreeSet, HashSet};

use esr_core::model::{
    DisplayRef, LocationId, LocationKind, Placement, Rect, ShowState, Snapshot, TabRec, Volume, WindowId, WindowRec,
};
use esr_core::plan::defer::choose_deferred;
use esr_core::plan::{LaunchInfo, TabRef};
use proptest::collection::vec;
use proptest::prelude::*;

fn loc(n: u8) -> LocationId {
    LocationId {
        case_sensitive: false,
        kind: LocationKind::FileSystem(Volume::Fixed),
        parsing_name: format!(r"C:\f{n}").as_str().into(),
        display_name: format!("f{n}").as_str().into(),
        pidl: vec![],
        search: None,
    }
}

fn window(id: u32, tabs: Vec<u8>, active: usize) -> WindowRec {
    let r = Rect { left: 0, top: 0, right: 800, bottom: 600 };
    let active = active % tabs.len();
    WindowRec {
        id: WindowId(id),
        tabs: tabs.into_iter().map(|n| TabRec { location: loc(n) }).collect(),
        active,
        placement: Placement { state: ShowState::Normal, restored: r, display: DisplayRef { device: "P".into(), monitor: r } },
    }
}

/// Up to 6 windows of up to 12 tabs over a pool of `pool` Locations (small
/// pools give many repeats), a random activation order, a launch, and a
/// budget around the distinct count.
fn case() -> impl Strategy<Value = (Snapshot, LaunchInfo, usize)> {
    (2u8..40).prop_flat_map(|pool| {
        let windows = vec((vec(0..pool, 1..12), any::<usize>()), 1..6);
        let launch = prop_oneof![Just(None), (0..pool + 3).prop_map(Some)];
        (windows, launch, 0usize..30, any::<u64>())
    })
    .prop_map(|(ws, requested, budget, seed)| {
        let windows: Vec<WindowRec> =
            ws.into_iter().enumerate().map(|(i, (tabs, a))| window(i as u32 + 1, tabs, a)).collect();
        let mut activation: Vec<WindowId> = windows.iter().map(|w| w.id).collect();
        // A deterministic shuffle from `seed`.
        let mut s = seed;
        for i in (1..activation.len()).rev() {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            activation.swap(i, (s >> 33) as usize % (i + 1));
        }
        let launch = match requested {
            Some(n) => LaunchInfo::Targeted { requested: loc(n) },
            None => LaunchInfo::Default,
        };
        (Snapshot { windows, activation }, launch, budget)
    })
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(512),
        ..ProptestConfig::default()
    })]

    #[test]
    fn deferral_keeps_the_budget((snap, launch, budget) in case()) {
        let d = choose_deferred(&snap, &launch, budget);
        let ordered: Vec<&WindowRec> = snap.activation.iter().filter_map(|&id| snap.window(id)).collect();
        let requested = match &launch { LaunchInfo::Targeted { requested } => Some(requested), _ => None };

        let mut priority: HashSet<&LocationId> = ordered.iter().map(|w| &w.tabs[w.active].location).collect();
        priority.extend(requested);
        let all: HashSet<&LocationId> =
            ordered.iter().flat_map(|w| w.tabs.iter().map(|t| &t.location)).chain(requested).collect();
        prop_assert_eq!(d.distinct, all.len());

        let mut immediate: HashSet<&LocationId> = requested.into_iter().collect();
        let mut deferred_locs: HashSet<&LocationId> = HashSet::new();
        let mut first_seen: Vec<(&LocationId, bool)> = Vec::new();
        let mut seen: HashSet<&LocationId> = HashSet::new();
        for w in &ordered {
            for (i, t) in w.tabs.iter().enumerate() {
                let is_deferred = d.deferred.contains(&TabRef { window: w.id, tab: i });
                if i == w.active || requested == Some(&t.location) {
                    prop_assert!(!is_deferred, "Active_Tab or requested Location deferred: {:?}", (w.id, i));
                }
                if is_deferred { deferred_locs.insert(&t.location); } else { immediate.insert(&t.location); }
                if !priority.contains(&t.location) && seen.insert(&t.location) {
                    first_seen.push((&t.location, is_deferred));
                }
            }
        }
        // Only tabs of the snapshot are deferred.
        let refs: BTreeSet<TabRef> = ordered
            .iter()
            .flat_map(|w| (0..w.tabs.len()).map(|tab| TabRef { window: w.id, tab }))
            .collect();
        prop_assert!(d.deferred.is_subset(&refs));

        prop_assert_eq!(d.immediate, immediate.len());
        prop_assert!(immediate.len() <= budget.max(priority.len()));
        prop_assert!(deferred_locs.is_disjoint(&immediate));
        if !d.deferred.is_empty() {
            prop_assert!(immediate.len() >= budget);
        }
        // Immediate first occurrences form a prefix.
        if let Some(cut) = first_seen.iter().position(|(_, deferred)| *deferred) {
            prop_assert!(first_seen[cut..].iter().all(|(_, deferred)| *deferred));
        }
    }
}
