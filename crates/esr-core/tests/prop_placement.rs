//! Property test for placement resolution (`esr_core::plan::placement`).
//!
//! **Property 16: Placement resolution.** For any saved placement and any
//! display set, `resolve_placement`:
//! - keeps the restored bounds unchanged when at least 50% of them lie inside
//!   one display's work area;
//! - otherwise centres them in the primary work area with size
//!   `min(saved, work)` per axis (at least 1 px), fully inside it;
//! - targets the display with the largest overlap with the resolved bounds
//!   (ties: primary, then lowest index), which is the primary for bounds that
//!   did not fit;
//! - keeps `Normal`/`Maximized`, keeps `Minimized { rm }` with the same `rm`,
//!   and never minimizes the trigger of a Targeted_Launch.
//!
//! The screen ↔ workspace conversion round-trips exactly and fails only on
//! `i32` overflow.
//!
//! **Validates: Requirements 5.5, 5.6, 5.8, 5.12, 6.6**
//!
//! The oracle below computes everything in `i128` and does not use
//! `Rect::area` (which multiplies in `i64`).

mod common;

use common::strategies::*;
use esr_core::model::*;
use esr_core::plan::placement::*;
use proptest::collection::vec;
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

/// Coordinates weighted towards ordinary values and the `i32` limits.
fn coord() -> impl Strategy<Value = i32> {
    prop_oneof![
        4 => -10_000i32..10_000,
        1 => Just(i32::MIN),
        1 => Just(i32::MAX),
        1 => i32::MIN..i32::MIN + 16,
        1 => i32::MAX - 16..=i32::MAX,
        2 => any::<i32>(),
    ]
}

/// A positive rect with coordinates possibly at the `i32` limits.
fn extreme_positive_rect() -> impl Strategy<Value = Rect> {
    (coord(), coord(), coord(), coord()).prop_filter_map("needs positive area", |(a, b, c, d)| {
        let (left, right) = (a.min(b), a.max(b));
        let (top, bottom) = (c.min(d), c.max(d));
        let r = Rect { left, top, right, bottom };
        r.is_positive().then_some(r)
    })
}

/// Any rect, including empty and inverted ones.
fn raw_rect() -> impl Strategy<Value = Rect> {
    (coord(), coord(), coord(), coord()).prop_map(|(left, top, right, bottom)| Rect { left, top, right, bottom })
}

/// Saved restored bounds: realistic, extreme, or degenerate.
fn saved_rect() -> impl Strategy<Value = Rect> {
    prop_oneof![
        4 => any_rect(),
        2 => extreme_positive_rect(),
        1 => raw_rect(),
    ]
}

/// A display with arbitrary (possibly extreme or empty) monitor/work rects.
fn degenerate_display() -> impl Strategy<Value = Display> {
    let work_of = |m: Rect| {
        prop_oneof![
            Just(m),
            // Empty: zero width or zero height.
            Just(Rect { right: m.left, ..m }),
            Just(Rect { bottom: m.top, ..m }),
            raw_rect(),
            extreme_positive_rect(),
        ]
    };
    (prop_oneof![any_rect(), extreme_positive_rect(), raw_rect()], any::<bool>(), 0u8..10)
        .prop_flat_map(move |(monitor, primary, n)| (Just(monitor), work_of(monitor), Just(primary), Just(n)))
        .prop_map(|(monitor, work, primary, n)| Display {
            device: format!(r"\\.\DISPLAY{n}"),
            monitor,
            work,
            primary,
        })
}

/// Realistic display sets plus degenerate ones: empty, no primary, an empty
/// work area, and arbitrary (overlapping, extreme, multi-primary) sets.
fn display_set() -> impl Strategy<Value = Vec<Display>> {
    prop_oneof![
        6 => displays().boxed(),
        1 => Just(Vec::new()).boxed(),
        2 => displays()
            .prop_map(|mut ds| {
                for d in &mut ds {
                    d.primary = false;
                }
                ds
            })
            .boxed(),
        2 => (displays(), any::<prop::sample::Index>(), any::<bool>())
            .prop_map(|(mut ds, i, horizontal)| {
                let i = i.index(ds.len());
                let d = &mut ds[i];
                if horizontal {
                    d.work.right = d.work.left;
                } else {
                    d.work.bottom = d.work.top;
                }
                ds
            })
            .boxed(),
        3 => vec(degenerate_display(), 1..=4).boxed(),
    ]
}

/// A saved placement, on the given displays where possible.
fn saved_for(displays: Vec<Display>) -> BoxedStrategy<Placement> {
    let free = (show_state(), saved_rect(), prop_oneof![any_rect(), raw_rect()], 0u8..10)
        .prop_map(|(state, restored, monitor, n)| Placement {
            state,
            restored,
            display: DisplayRef { device: format!(r"\\.\DISPLAY{n}"), monitor },
        });
    if displays.is_empty() {
        free.boxed()
    } else {
        prop_oneof![3 => placement_on(displays).boxed(), 2 => free.boxed()].boxed()
    }
}

fn scenario() -> impl Strategy<Value = (Vec<Display>, Placement, bool)> {
    display_set().prop_flat_map(|ds| (Just(ds.clone()), saved_for(ds), any::<bool>()))
}

// ---------------------------------------------------------------------------
// Oracle (i128 arithmetic)
// ---------------------------------------------------------------------------

fn area(r: &Rect) -> i128 {
    let (w, h) = (i128::from(r.right) - i128::from(r.left), i128::from(r.bottom) - i128::from(r.top));
    if w > 0 && h > 0 {
        w * h
    } else {
        0
    }
}

fn overlap(a: &Rect, b: &Rect) -> i128 {
    let l = i128::from(a.left.max(b.left));
    let t = i128::from(a.top.max(b.top));
    let r = i128::from(a.right.min(b.right));
    let btm = i128::from(a.bottom.min(b.bottom));
    if r > l && btm > t {
        (r - l) * (btm - t)
    } else {
        0
    }
}

fn contains(outer: &Rect, inner: &Rect) -> bool {
    inner.left >= outer.left && inner.top >= outer.top && inner.right <= outer.right && inner.bottom <= outer.bottom
}

fn expected_state(saved: ShowState, targeted_trigger: bool) -> ShowState {
    match saved {
        ShowState::Minimized { .. } if targeted_trigger => ShowState::Normal,
        s => s,
    }
}

/// Expected `(start, end)` on one axis for a non-fitting rect: length
/// `min(saved, extent)` but at least 1, centred with floor division, and
/// kept inside `i32` (only matters for an empty axis at `i32::MAX`).
fn centred_axis(lo: i32, hi: i32, saved_len: i128) -> (i32, i32) {
    let extent = i128::from(hi) - i128::from(lo);
    let len = saved_len.min(extent).max(1);
    let start = (i128::from(lo) + (extent - len).max(0).div_euclid(2)).min(i128::from(i32::MAX) - len);
    (start as i32, (start + len) as i32)
}

/// Index of the display with maximal work-area overlap with `r`; among ties
/// the primary if it is one, else the lowest index.
fn expected_target(r: &Rect, ds: &[Display], primary: usize) -> usize {
    let max = ds.iter().map(|d| overlap(r, &d.work)).max().unwrap();
    let ties: Vec<usize> = (0..ds.len()).filter(|&i| overlap(r, &ds[i].work) == max).collect();
    if ties.contains(&primary) {
        primary
    } else {
        ties[0]
    }
}

fn shift(r: Rect, dx: i128, dy: i128) -> Option<Rect> {
    let f = |v: i32, d: i128| i32::try_from(i128::from(v) + d).ok();
    Some(Rect { left: f(r.left, dx)?, top: f(r.top, dy)?, right: f(r.right, dx)?, bottom: f(r.bottom, dy)? })
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    /// Property 16 (a)–(f), (h): fit, centring, target display, state.
    #[test]
    fn placement_resolution((ds, saved, targeted) in scenario()) {
        let p = resolve_placement(&saved, &ds, targeted);

        // (h) Deterministic.
        prop_assert_eq!(resolve_placement(&saved, &ds, targeted), p);

        // (e) State mapping.
        prop_assert_eq!(p.state, expected_state(saved.state, targeted));
        if targeted {
            let minimized = matches!(p.state, ShowState::Minimized { .. });
            prop_assert!(!minimized, "targeted trigger minimized");
        }

        // The workspace rect is the conversion with the target display.
        prop_assert_eq!(p.restored_workspace(), screen_to_workspace(p.restored, p.monitor, p.work));

        if ds.is_empty() {
            // No displays: saved values are kept, zero workspace offset.
            prop_assert_eq!(p.restored, saved.restored);
            prop_assert_eq!(p.monitor, saved.display.monitor);
            prop_assert_eq!(p.work, saved.display.monitor);
            prop_assert_eq!(p.restored_workspace(), Some(saved.restored));
            return Ok(());
        }

        // (f) Always positive with at least one display.
        prop_assert!(p.restored.is_positive(), "restored not positive: {:?}", p.restored);

        let primary = ds.iter().position(|d| d.primary).unwrap_or(0);
        let pw = ds[primary].work;
        let saved_area = area(&saved.restored);
        let best = ds.iter().map(|d| overlap(&saved.restored, &d.work)).max().unwrap();
        let fits = saved_area > 0 && 2 * best >= saved_area;

        if fits {
            // (a) Fit ⇒ unchanged.
            prop_assert_eq!(p.restored, saved.restored);
        } else {
            // (b) Clamped size, centred in the primary work area.
            let sw = i128::from(saved.restored.right) - i128::from(saved.restored.left);
            let sh = i128::from(saved.restored.bottom) - i128::from(saved.restored.top);
            let (left, right) = centred_axis(pw.left, pw.right, sw);
            let (top, bottom) = centred_axis(pw.top, pw.bottom, sh);
            prop_assert_eq!(p.restored, Rect { left, top, right, bottom });
            if pw.is_positive() {
                prop_assert!(contains(&pw, &p.restored), "{:?} not inside {:?}", p.restored, pw);
                prop_assert_eq!(i128::from(p.restored.width()), sw.min(i128::from(pw.width())).max(1));
                prop_assert_eq!(i128::from(p.restored.height()), sh.min(i128::from(pw.height())).max(1));
                // Centred: left slack ≤ right slack ≤ left slack + 1.
                let ls = i64::from(p.restored.left) - i64::from(pw.left);
                let rs = i64::from(pw.right) - i64::from(p.restored.right);
                let ts = i64::from(p.restored.top) - i64::from(pw.top);
                let bs = i64::from(pw.bottom) - i64::from(p.restored.bottom);
                prop_assert!(ls <= rs && rs <= ls + 1 && ts <= bs && bs <= ts + 1);

                // (d) A non-fitting window (maximized in particular) targets
                // the primary display.
                prop_assert_eq!((p.monitor, p.work), (ds[primary].monitor, ds[primary].work));
            }
        }

        // (c) Target is a real display with maximal overlap, tie rules applied.
        prop_assert!(ds.iter().any(|d| d.monitor == p.monitor && d.work == p.work));
        let t = expected_target(&p.restored, &ds, primary);
        prop_assert_eq!((p.monitor, p.work), (ds[t].monitor, ds[t].work));
        let max = ds.iter().map(|d| overlap(&p.restored, &d.work)).max().unwrap();
        prop_assert_eq!(overlap(&p.restored, &p.work), max);
    }

    /// Property 16 (g): screen ↔ workspace conversion round-trips exactly and
    /// returns `None` only on `i32` overflow.
    #[test]
    fn workspace_conversion_round_trips(
        r in prop_oneof![any_rect(), raw_rect()],
        monitor in prop_oneof![any_rect(), raw_rect()],
        work in prop_oneof![any_rect(), raw_rect()],
    ) {
        let dx = i128::from(work.left) - i128::from(monitor.left);
        let dy = i128::from(work.top) - i128::from(monitor.top);

        let w = screen_to_workspace(r, monitor, work);
        prop_assert_eq!(w, shift(r, -dx, -dy));
        if let Some(w) = w {
            prop_assert_eq!(workspace_to_screen(w, monitor, work), Some(r));
        }

        let s = workspace_to_screen(r, monitor, work);
        prop_assert_eq!(s, shift(r, dx, dy));
        if let Some(s) = s {
            prop_assert_eq!(screen_to_workspace(s, monitor, work), Some(r));
        }
    }
}

/// An empty primary work area at `i32::MAX` still yields a positive 1 px rect.
#[test]
fn empty_work_area_at_i32_max_is_positive() {
    let edge = Rect { left: i32::MAX, top: i32::MAX, right: i32::MAX, bottom: i32::MAX };
    let d = Display { device: r"\\.\DISPLAY1".into(), monitor: edge, work: edge, primary: true };
    let saved = Placement {
        state: ShowState::Normal,
        restored: Rect { left: 0, top: 0, right: 800, bottom: 600 },
        display: DisplayRef { device: r"\\.\DISPLAY1".into(), monitor: edge },
    };
    let p = resolve_placement(&saved, &[d], false);
    assert_eq!(p.restored, Rect { left: i32::MAX - 1, top: i32::MAX - 1, right: i32::MAX, bottom: i32::MAX });
}
