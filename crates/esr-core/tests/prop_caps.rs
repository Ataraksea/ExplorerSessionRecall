//! Property test for the window and tab caps (`esr_core::tracker::caps`).
//!
//! **Property 6: Caps keep an activation-order prefix and a valid snapshot**
//!
//! *For any* list of windows in Activation_Order (up to 150 windows and 800
//! tabs), `apply_caps` keeps at most 100 windows and 500 tabs, the kept
//! windows are a prefix of the Activation_Order, kept tabs of each window are
//! a left-to-right prefix, the capture passes `validate`, and a
//! `CAP-EXCLUDED` log entry is emitted exactly when something was excluded.
//!
//! Checked both on `apply_caps` directly and on
//! `LiveSessionTracker::capture`, fed with scans and activations of the same
//! windows. Also checked: a cut Active_Tab becomes the rightmost kept tab,
//! only the last kept window may be cut (and only at the tab cap), the caps
//! keep as much as they may, and an input within the caps is unchanged.
//!
//! **Validates: Requirements 2.10**

// Feature: explorer-session-restore, Property 6: Caps keep an activation-order prefix and a valid snapshot

mod common;

use std::collections::HashSet;

use common::strategies::placement_on;
use esr_core::engine::{Event, Millis, ObservedLocation, TabKey, TabObservation, WinKey, WindowObservation};
use esr_core::logfmt::LogEntry;
use esr_core::model::*;
use esr_core::tracker::{apply_caps, Excluded, LiveSessionTracker};
use proptest::collection::{btree_set, vec};
use proptest::prelude::*;
use proptest::sample::Index;

/// Largest generated input (design.md, Property 6).
const MAX_INPUT_WINDOWS: usize = 150;
const MAX_INPUT_TABS: usize = 800;

/// Clamps per-window tab counts so the total fits `max_total` while every
/// window keeps at least one tab (local copy of the shared helper).
fn clamp_counts(mut counts: Vec<usize>, max_total: usize) -> Vec<usize> {
    let mut budget = max_total;
    let n = counts.len();
    for (i, c) in counts.iter_mut().enumerate() {
        let allowed = budget - (n - i - 1);
        *c = (*c).min(allowed).max(1);
        budget -= *c;
    }
    counts
}

/// Per-window tab counts, weighted towards inputs that exceed a cap.
fn tab_counts() -> impl Strategy<Value = Vec<usize>> {
    prop_oneof![
        // Well within the caps.
        1 => vec(1usize..=10, 0..=10),
        // Around and over the window cap.
        3 => vec(1usize..=6, 90..=MAX_INPUT_WINDOWS),
        // Few windows, over the tab cap (some very large windows).
        3 => vec(prop_oneof![3 => 1usize..=20, 1 => 50usize..=400], 1..=30),
        // Total tab count right around 500, spread evenly.
        2 => (1usize..=MAX_WINDOWS, 490usize..=510)
            .prop_map(|(n, t)| (0..n).map(|i| t / n + usize::from(i < t % n)).collect()),
    ]
    .prop_map(|c| clamp_counts(c, MAX_INPUT_TABS))
}

fn display() -> Display {
    let monitor = Rect { left: 0, top: 0, right: 1920, bottom: 1080 };
    Display {
        device: r"\\.\DISPLAY1".into(),
        monitor,
        work: Rect { bottom: 1040, ..monitor },
        primary: true,
    }
}

/// A unique location per (window index, tab index), so prefixes are exact.
fn tab(window: usize, index: usize) -> TabRec {
    let path = format!(r"C:\w{window}\t{index}");
    TabRec {
        location: LocationId {
            case_sensitive: false,
            kind: LocationKind::FileSystem(Volume::Fixed),
            parsing_name: path.as_str().into(),
            display_name: format!("t{index}").as_str().into(),
            pidl: vec![1, index as u8],
            search: None,
        },
    }
}

/// A snapshot that is valid except that it may exceed the caps.
fn uncapped_snapshot() -> impl Strategy<Value = Snapshot> {
    tab_counts()
        .prop_flat_map(|counts| {
            let n = counts.len();
            (
                Just(counts),
                vec(any::<Index>(), n),
                btree_set(any::<u32>(), n),
                Just((0..n).collect::<Vec<usize>>()).prop_shuffle(),
                vec(placement_on(vec![display()]), n),
            )
        })
        .prop_map(|(counts, actives, ids, order, placements)| {
            let ids: Vec<WindowId> = ids.into_iter().map(WindowId).collect();
            let windows = counts
                .iter()
                .zip(actives)
                .zip(placements)
                .enumerate()
                .map(|(i, ((&c, active), placement))| WindowRec {
                    id: ids[i],
                    tabs: (0..c).map(|j| tab(i, j)).collect(),
                    active: active.index(c),
                    placement,
                })
                .collect();
            Snapshot { windows, activation: order.into_iter().map(|i| ids[i]).collect() }
        })
}

/// Checks Property 6 for `input` capped to `out`, with the reported
/// exclusion counts and log entry.
fn check_caps(
    input: &Snapshot,
    out: &Snapshot,
    excluded: Excluded,
    log: Option<LogEntry>,
) -> Result<(), TestCaseError> {
    let n = input.windows.len();
    let total = input.tab_count();
    let k = out.windows.len();
    let kept_tabs = out.tab_count();

    // Within the caps, and valid.
    prop_assert!(k <= MAX_WINDOWS, "{} windows kept", k);
    prop_assert!(kept_tabs <= MAX_TABS_TOTAL, "{} tabs kept", kept_tabs);
    prop_assert_eq!(validate(out), Ok(()));

    // Kept windows: an Activation_Order prefix; `windows` keeps input order.
    prop_assert_eq!(&out.activation[..], &input.activation[..k]);
    let kept: HashSet<WindowId> = out.activation.iter().copied().collect();
    let want_ids: Vec<WindowId> =
        input.windows.iter().map(|w| w.id).filter(|id| kept.contains(id)).collect();
    let got_ids: Vec<WindowId> = out.windows.iter().map(|w| w.id).collect();
    prop_assert_eq!(got_ids, want_ids);

    // Each kept window keeps its leftmost tabs; a cut Active_Tab becomes the
    // rightmost kept tab.
    let mut cut = Vec::new();
    for w in &out.windows {
        let orig = input.window(w.id).expect("kept window is from the input");
        let len = w.tabs.len();
        prop_assert!(len >= 1 && len <= orig.tabs.len());
        prop_assert_eq!(&w.tabs[..], &orig.tabs[..len], "window {:?} tabs not a prefix", w.id);
        prop_assert_eq!(&w.placement, &orig.placement);
        let want_active = if orig.active < len { orig.active } else { len - 1 };
        prop_assert_eq!(w.active, want_active, "window {:?} active", w.id);
        if len < orig.tabs.len() {
            cut.push(w.id);
        }
    }

    // Only the least recently activated kept window may be cut, and only
    // because the tab cap was reached.
    prop_assert!(cut.len() <= 1, "several windows cut: {:?}", cut);
    if let Some(id) = cut.first() {
        prop_assert_eq!(Some(id), out.activation.last());
        prop_assert_eq!(kept_tabs, MAX_TABS_TOTAL);
    }
    // A window is excluded only when a cap is reached.
    if k < n {
        prop_assert!(
            k == MAX_WINDOWS || kept_tabs == MAX_TABS_TOTAL,
            "excluded windows with {} windows and {} tabs kept", k, kept_tabs
        );
    }

    // Exclusion counts and the CAP-EXCLUDED entry.
    let over = n > MAX_WINDOWS || total > MAX_TABS_TOTAL;
    if !over {
        prop_assert_eq!(out, input);
    }
    prop_assert_eq!(excluded.windows as usize, n - k);
    prop_assert_eq!(excluded.tabs as usize, total - kept_tabs);
    prop_assert_eq!(excluded.is_empty(), !over);
    let want_log = over.then_some(LogEntry::CapExcluded { windows: excluded.windows, tabs: excluded.tabs });
    prop_assert_eq!(log, want_log);
    Ok(())
}

fn win_key(i: usize) -> WinKey {
    WinKey { hwnd: 0x1_0000 + i as u64, seq: 1 }
}

fn tab_key(window: usize, index: usize) -> TabKey {
    TabKey { hwnd: 0x100_0000 + (window * 1000 + index) as u64, seq: 1 }
}

/// Feeds every window of `s` to a tracker (one scan each, then activations
/// so the tracker's Activation_Order equals `s.activation`), and returns the
/// tracker with the uncapped snapshot `capture` should cap: windows in
/// Activation_Order with ids `0, 1, …`.
fn track(s: &Snapshot) -> (LiveSessionTracker, Snapshot) {
    let mut tracker = LiveSessionTracker::new();
    let mut now = 0;
    for (i, w) in s.windows.iter().enumerate() {
        let tabs = w
            .tabs
            .iter()
            .enumerate()
            .map(|(j, t)| TabObservation {
                tab: tab_key(i, j),
                location: ObservedLocation::Known(t.location.clone()),
            })
            .collect();
        let obs = WindowObservation {
            win: win_key(i),
            pid: 10,
            tabs,
            active: Some(tab_key(i, w.active)),
            strip: None,
            placement: w.placement.clone(),
        };
        tracker.handle(Millis(now), &Event::WindowScanned(obs));
        now += 1;
    }
    let index_of = |id: &WindowId| s.windows.iter().position(|w| w.id == *id).expect("listed");
    for id in s.activation.iter().rev() {
        tracker.handle(Millis(now), &Event::WindowActivated { win: win_key(index_of(id)) });
        now += 1;
    }
    let windows: Vec<WindowRec> = s
        .activation
        .iter()
        .enumerate()
        .map(|(rank, id)| WindowRec { id: WindowId(rank as u32), ..s.windows[index_of(id)].clone() })
        .collect();
    let activation = windows.iter().map(|w| w.id).collect();
    (tracker, Snapshot { windows, activation })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// **Property 6: Caps keep an activation-order prefix and a valid snapshot**
    ///
    /// **Validates: Requirements 2.10**
    #[test]
    fn apply_caps_keeps_an_activation_order_prefix(input in uncapped_snapshot()) {
        prop_assert!(input.windows.len() <= MAX_INPUT_WINDOWS);
        prop_assert!(input.tab_count() <= MAX_INPUT_TABS);
        let (out, excluded) = apply_caps(input.clone());
        check_caps(&input, &out, excluded, excluded.log_entry())?;
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// **Property 6: Caps keep an activation-order prefix and a valid snapshot**
    /// (through `LiveSessionTracker::capture`).
    ///
    /// **Validates: Requirements 2.10**
    #[test]
    fn capture_applies_the_caps(input in uncapped_snapshot()) {
        let (tracker, uncapped) = track(&input);
        let (out, log) = tracker.capture();
        let excluded = match log {
            Some(LogEntry::CapExcluded { windows, tabs }) => Excluded { windows, tabs },
            _ => Excluded::default(),
        };
        check_caps(&uncapped, &out, excluded, log)?;
    }
}
