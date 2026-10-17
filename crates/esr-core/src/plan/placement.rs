//! `resolve_placement` and screen/workspace coordinate conversion
//! (Requirements 5.5, 5.6, 5.8, 5.9, 5.12, 6.6; design.md "Placement resolution").
//!
//! # Rules
//!
//! - **Overlap** of a rect with a display is `area(rect ∩ display.work)`, the
//!   intersection with the display's *work area* (not its monitor rect).
//! - **Fit (Req 5.5, 5.8, 5.12).** The saved restored bounds *fit* when a
//!   *single* connected display's work area holds at least 50% of their area:
//!   `2 · max_d overlap(d) ≥ area(saved.restored)`. The test is per display,
//!   not against the union of all work areas ("the work area of any currently
//!   connected display"). The comparison is exact integer arithmetic, so
//!   exactly 50% fits.
//! - **Resolved bounds.** A fitting rect is kept unchanged. Otherwise the size
//!   is the saved size clamped to the primary work area's size, centred in the
//!   primary work area. Centring uses floor division of the slack, so when the
//!   slack is odd the extra pixel goes to the right/bottom.
//! - **Target display.** The display with the largest overlap with the
//!   resolved bounds. Ties go to the primary display, then to the lowest index.
//!   A maximized window is maximized there (Req 5.5). For bounds that did not
//!   fit this is always the primary display, because the centred rect lies
//!   fully inside the primary work area and no display can overlap it more
//!   (Req 5.12). Normal and minimized windows use the target display for the
//!   workspace conversion.
//! - **State.** `Normal` and `Maximized` are kept. `Minimized { restore_maximized }`
//!   is kept with the same flag (Req 5.6). For the trigger window of a
//!   Targeted_Launch, a saved minimized state (either flag) becomes `Normal` at
//!   the resolved bounds (Req 6.6); the trigger is never minimized.
//!
//! # Degenerate display sets
//!
//! - **No displays:** the saved state (with the Targeted_Launch rule) and the
//!   saved restored bounds are kept, and the target monitor *and* work area are
//!   the saved `display.monitor`. That makes the workspace offset zero, so the
//!   saved screen coordinates are passed through unchanged.
//! - **No display flagged primary:** display 0 acts as primary. With several
//!   flagged, the first flagged one is primary.
//! - **Empty primary work area:** the clamped size is raised to at least 1 px so
//!   the resolved rect always has positive size (at `i32::MAX` it is moved back
//!   by 1 px to stay in range).
//!
//! # Screen ↔ workspace coordinates
//!
//! `WINDOWPLACEMENT.rcNormalPosition` is in *workspace* coordinates: relative
//! to the work area of the monitor the window is on, rather than to the
//! monitor's top-left. With `offset = (work.left − monitor.left,
//! work.top − monitor.top)` of that monitor:
//!
//! - `workspace = screen − offset` ([`screen_to_workspace`]), applied before
//!   `SetWindowPlacement` with the target display's `monitor`/`work` from
//!   [`FinalPlacement`] (see [`FinalPlacement::restored_workspace`]).
//! - `screen = workspace + offset` ([`workspace_to_screen`]), applied after
//!   `GetWindowPlacement` on capture, using the monitor the window is on
//!   (`MonitorFromWindow` + `GetMonitorInfoW`).
//!
//! Both are computed in `i64` and return `None` only when the result does not
//! fit in `i32`. Whenever one direction returns `Some(r)`, the other direction
//! maps `r` back exactly, so save → restore with the same displays is lossless
//! (Req 5.9).
//!
//! Areas are computed in `i128`: an `i32` rect can be almost 2³² px on each
//! side, whose area does not fit in `i64`.

use crate::model::{Display, Placement, Rect, ShowState};

/// The placement a restored window is given in one `SetWindowPlacement`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FinalPlacement {
    /// `Normal`, `Maximized`, or `Minimized { restore_maximized }`
    /// (`SW_SHOWMINNOACTIVE` + `WPF_RESTORETOMAXIMIZED`).
    pub state: ShowState,
    /// Restored bounds in screen coordinates, physical pixels.
    pub restored: Rect,
    /// Monitor rect of the target display (screen coordinates).
    pub monitor: Rect,
    /// Work area of the target display, used for the screen → workspace
    /// conversion of `restored` before `SetWindowPlacement`.
    pub work: Rect,
}

impl FinalPlacement {
    /// `restored` in workspace coordinates of the target display, ready for
    /// `WINDOWPLACEMENT.rcNormalPosition`. `None` only if out of `i32` range.
    pub fn restored_workspace(&self) -> Option<Rect> {
        screen_to_workspace(self.restored, self.monitor, self.work)
    }
}

/// Resolves the final placement of a restored window from its saved placement
/// and the currently connected displays. See the module docs for the rules.
///
/// `targeted_trigger` is `true` for the Trigger_Window of a Targeted_Launch,
/// which is never minimized (Req 6.6).
pub fn resolve_placement(
    saved: &Placement,
    displays: &[Display],
    targeted_trigger: bool,
) -> FinalPlacement {
    let state = match saved.state {
        ShowState::Minimized { .. } if targeted_trigger => ShowState::Normal,
        s => s,
    };

    if displays.is_empty() {
        return FinalPlacement {
            state,
            restored: saved.restored,
            monitor: saved.display.monitor,
            work: saved.display.monitor,
        };
    }

    let primary = primary_index(displays);
    let saved_area = area(&saved.restored);
    let best_overlap = displays.iter().map(|d| overlap(&saved.restored, &d.work)).max().unwrap_or(0);
    let fits = saved_area > 0 && 2 * best_overlap >= saved_area;

    let restored = if fits {
        saved.restored
    } else {
        let work = displays[primary].work;
        centred(&work, saved.restored.width(), saved.restored.height())
    };

    let target = &displays[target_index(&restored, displays, primary)];
    FinalPlacement { state, restored, monitor: target.monitor, work: target.work }
}

/// Converts a screen-coordinate rect to workspace coordinates of the monitor
/// with rect `monitor` and work area `work`: subtracts
/// `(work.left − monitor.left, work.top − monitor.top)`.
/// `None` if the result does not fit in `i32`.
pub fn screen_to_workspace(rect: Rect, monitor: Rect, work: Rect) -> Option<Rect> {
    let (dx, dy) = workspace_offset(monitor, work);
    translate(rect, -dx, -dy)
}

/// Converts a workspace-coordinate rect (as in `rcNormalPosition`) to screen
/// coordinates: adds `(work.left − monitor.left, work.top − monitor.top)`.
/// Exact inverse of [`screen_to_workspace`]. `None` if out of `i32` range.
pub fn workspace_to_screen(rect: Rect, monitor: Rect, work: Rect) -> Option<Rect> {
    let (dx, dy) = workspace_offset(monitor, work);
    translate(rect, dx, dy)
}

fn workspace_offset(monitor: Rect, work: Rect) -> (i64, i64) {
    (
        i64::from(work.left) - i64::from(monitor.left),
        i64::from(work.top) - i64::from(monitor.top),
    )
}

fn translate(r: Rect, dx: i64, dy: i64) -> Option<Rect> {
    let f = |v: i32, d: i64| i32::try_from(i64::from(v) + d).ok();
    Some(Rect {
        left: f(r.left, dx)?,
        top: f(r.top, dy)?,
        right: f(r.right, dx)?,
        bottom: f(r.bottom, dy)?,
    })
}

/// First display flagged primary, else display 0. `displays` is non-empty.
fn primary_index(displays: &[Display]) -> usize {
    displays.iter().position(|d| d.primary).unwrap_or(0)
}

/// Display with the largest work-area overlap with `rect`; ties go to
/// `primary`, then to the lowest index. `displays` is non-empty.
fn target_index(rect: &Rect, displays: &[Display], primary: usize) -> usize {
    let mut best = primary;
    let mut best_overlap = overlap(rect, &displays[primary].work);
    for (i, d) in displays.iter().enumerate() {
        let o = overlap(rect, &d.work);
        // Strictly greater keeps the primary on ties; ascending iteration keeps
        // the lowest index among non-primary ties.
        if o > best_overlap {
            best = i;
            best_overlap = o;
        }
    }
    best
}

fn area(r: &Rect) -> i128 {
    if r.is_positive() {
        i128::from(r.width()) * i128::from(r.height())
    } else {
        0
    }
}

fn overlap(a: &Rect, b: &Rect) -> i128 {
    a.intersect(b).map_or(0, |r| area(&r))
}

/// A `width × height` rect clamped to `work`'s size (at least 1 px) and centred
/// in `work`; odd slack puts the extra pixel on the right/bottom.
fn centred(work: &Rect, width: i64, height: i64) -> Rect {
    let axis = |lo: i32, extent: i64, len: i64| -> (i32, i32) {
        let len = len.min(extent).max(1);
        // `extent - len >= 0` unless the work area is empty, so `/ 2` floors.
        let start = i64::from(lo) + (extent - len).max(0) / 2;
        // Keep `start + len` within `i32` so the rect stays positive even for
        // an empty work area at `i32::MAX` (`len ≤ extent` otherwise, so this
        // only moves such an empty axis back by `len`).
        let start = start.min(i64::from(i32::MAX) - len);
        (start as i32, (start + len) as i32)
    };
    let (left, right) = axis(work.left, work.width(), width);
    let (top, bottom) = axis(work.top, work.height(), height);
    Rect { left, top, right, bottom }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DisplayRef;

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> Rect {
        Rect { left, top, right, bottom }
    }

    fn display(name: &str, monitor: Rect, work: Rect, primary: bool) -> Display {
        Display { device: name.into(), monitor, work, primary }
    }

    /// Primary 1920×1080 at the origin with a 40 px bottom taskbar.
    fn primary() -> Display {
        display("P", rect(0, 0, 1920, 1080), rect(0, 0, 1920, 1040), true)
    }

    /// Secondary 1920×1080 to the left (negative coordinates), taskbar on top.
    fn left() -> Display {
        display("L", rect(-1920, 0, 0, 1080), rect(-1920, 48, 0, 1080), false)
    }

    fn saved(state: ShowState, restored: Rect) -> Placement {
        Placement {
            state,
            restored,
            display: DisplayRef { device: "old".into(), monitor: rect(5000, 5000, 6000, 6000) },
        }
    }

    fn resolve(state: ShowState, restored: Rect, displays: &[Display]) -> FinalPlacement {
        resolve_placement(&saved(state, restored), displays, false)
    }

    const MIN_RM: ShowState = ShowState::Minimized { restore_maximized: true };
    const MIN_N: ShowState = ShowState::Minimized { restore_maximized: false };

    #[test]
    fn fitting_normal_keeps_bounds_and_state() {
        let r = rect(100, 100, 900, 700);
        let p = resolve(ShowState::Normal, r, &[primary(), left()]);
        assert_eq!(p.state, ShowState::Normal);
        assert_eq!(p.restored, r);
        assert_eq!((p.monitor, p.work), (primary().monitor, primary().work));
    }

    #[test]
    fn exactly_half_fits_and_just_under_does_not() {
        // 1000×400; 500 px of width inside the primary work area = exactly 50%.
        let half = rect(-500, 0, 500, 400);
        let p = resolve(ShowState::Normal, half, &[primary()]);
        assert_eq!(p.restored, half);

        // 499 px inside: below 50%, so centred (slack 920/2 = 460, 640/2 = 320).
        let under = rect(-501, 0, 499, 400);
        let p = resolve(ShowState::Normal, under, &[primary()]);
        assert_eq!(p.restored, rect(460, 320, 1460, 720));
        assert_eq!(p.state, ShowState::Normal);
    }

    #[test]
    fn half_on_the_taskbar_counts_only_work_area() {
        // 100 px tall, 50 px inside the work area and 50 px over the taskbar
        // (inside the monitor but outside the work area): exactly 50%.
        let r = rect(0, 990, 400, 1090);
        assert_eq!(resolve(ShowState::Normal, r, &[primary()]).restored, r);
        let r = rect(0, 991, 400, 1091);
        assert_ne!(resolve(ShowState::Normal, r, &[primary()]).restored, r);
    }

    #[test]
    fn fit_is_per_display_not_union() {
        // 1000 wide: 400 px in A's work area, 400 px in B's, 200 px beyond B.
        // 80% of the union, but no single display holds ≥ 50% → not fitting.
        let a = display("A", rect(0, 0, 1000, 1000), rect(0, 0, 1000, 1000), true);
        let b = display("B", rect(1000, 0, 2000, 1000), rect(1000, 0, 1400, 1000), false);
        let r = rect(600, 0, 1600, 100);
        let p = resolve(ShowState::Normal, r, &[a, b]);
        assert_eq!(p.restored, rect(0, 450, 1000, 550));
    }

    #[test]
    fn off_screen_window_is_centred_with_clamped_size() {
        // Larger than the work area in both dimensions and fully off-screen.
        let r = rect(10_000, 10_000, 13_000, 12_000);
        let p = resolve(ShowState::Normal, r, &[left(), primary()]);
        assert_eq!(p.restored, primary().work);
        assert_eq!(p.monitor, primary().monitor);

        // Only one dimension clamped.
        let r = rect(-9000, -9000, -8000, -6000);
        let p = resolve(ShowState::Normal, r, &[left(), primary()]);
        assert_eq!(p.restored, rect(460, 0, 1460, 1040));
    }

    #[test]
    fn centring_rounds_down_with_odd_slack() {
        // Work area 1921×1041 at negative coordinates: slack 921 and 641.
        let d = display("N", rect(-1921, -1041, 0, 0), rect(-1921, -1041, 0, 0), true);
        let p = resolve(ShowState::Normal, rect(0, 0, 1000, 400), &[d]);
        assert_eq!(p.restored, rect(-1921 + 460, -1041 + 320, -1921 + 1460, -1041 + 720));
    }

    #[test]
    fn negative_coordinate_display_fits() {
        let r = rect(-1800, 100, -800, 900);
        let p = resolve(ShowState::Normal, r, &[primary(), left()]);
        assert_eq!(p.restored, r);
        assert_eq!((p.monitor, p.work), (left().monitor, left().work));
    }

    #[test]
    fn maximized_goes_to_display_with_largest_overlap() {
        // 60% on the left display, 40% on the primary.
        let r = rect(-600, 100, 400, 500);
        let p = resolve(ShowState::Maximized, r, &[primary(), left()]);
        assert_eq!(p.state, ShowState::Maximized);
        assert_eq!(p.restored, r);
        assert_eq!(p.monitor, left().monitor);
    }

    #[test]
    fn maximized_tie_goes_to_primary_then_lowest_index() {
        // Exactly half on each side (rows 48..1040 are in both work areas).
        let r = rect(-500, 100, 500, 500);
        let p = resolve(ShowState::Maximized, r, &[left(), primary()]);
        assert_eq!(p.monitor, primary().monitor);

        // Two non-primary displays tie: lowest index wins.
        let a = display("A", rect(0, 0, 1000, 1000), rect(0, 0, 1000, 1000), false);
        let b = display("B", rect(1000, 0, 2000, 1000), rect(1000, 0, 2000, 1000), false);
        let p0 = display("P", rect(0, 2000, 100, 2100), rect(0, 2000, 100, 2100), true);
        let r = rect(500, 0, 1500, 100);
        let p = resolve(ShowState::Maximized, r, &[p0.clone(), b.clone(), a.clone()]);
        assert_eq!(p.monitor, b.monitor);
        let p = resolve(ShowState::Maximized, r, &[p0, a.clone(), b]);
        assert_eq!(p.monitor, a.monitor);
    }

    #[test]
    fn non_fitting_maximized_goes_to_primary_with_centred_bounds() {
        let r = rect(-20_000, 0, -19_000, 600);
        let p = resolve(ShowState::Maximized, r, &[left(), primary()]);
        assert_eq!(p.state, ShowState::Maximized);
        assert_eq!(p.monitor, primary().monitor);
        assert_eq!(p.work, primary().work);
        assert_eq!(p.restored, rect(460, 220, 1460, 820));
    }

    #[test]
    fn minimized_keeps_restore_to_maximized() {
        let r = rect(100, 100, 900, 700);
        for s in [MIN_RM, MIN_N] {
            let p = resolve(s, r, &[primary()]);
            assert_eq!(p.state, s);
            assert_eq!(p.restored, r);
        }
        // Non-fitting minimized windows are centred too (Req 5.8).
        let p = resolve(MIN_RM, rect(9000, 9000, 9800, 9600), &[primary()]);
        assert_eq!(p.state, MIN_RM);
        assert_eq!(p.restored, rect(560, 220, 1360, 820));
    }

    #[test]
    fn targeted_trigger_is_never_minimized() {
        let r = rect(100, 100, 900, 700);
        for s in [MIN_RM, MIN_N] {
            let p = resolve_placement(&saved(s, r), &[primary()], true);
            assert_eq!(p.state, ShowState::Normal);
            assert_eq!(p.restored, r);
        }
        // The 50% rule still applies.
        let p = resolve_placement(&saved(MIN_RM, rect(9000, 9000, 9800, 9600)), &[primary()], true);
        assert_eq!(p.state, ShowState::Normal);
        assert_eq!(p.restored, rect(560, 220, 1360, 820));
        // Non-minimized states are unaffected.
        for s in [ShowState::Normal, ShowState::Maximized] {
            assert_eq!(resolve_placement(&saved(s, r), &[primary()], true).state, s);
        }
    }

    #[test]
    fn no_displays_keeps_saved_values() {
        let r = rect(-100, -100, 500, 500);
        let s = saved(MIN_RM, r);
        let p = resolve_placement(&s, &[], false);
        assert_eq!(p.state, MIN_RM);
        assert_eq!(p.restored, r);
        assert_eq!((p.monitor, p.work), (s.display.monitor, s.display.monitor));
        assert_eq!(p.restored_workspace(), Some(r));
        assert_eq!(resolve_placement(&s, &[], true).state, ShowState::Normal);
    }

    #[test]
    fn no_flagged_primary_uses_display_zero() {
        let mut a = left();
        let mut b = primary();
        a.primary = false;
        b.primary = false;
        let p = resolve(ShowState::Maximized, rect(9000, 9000, 9100, 9100), &[a.clone(), b]);
        assert_eq!(p.monitor, a.monitor);
        assert_eq!(p.restored, rect(-1920 + 910, 48 + 466, -1920 + 1010, 48 + 566));
    }

    #[test]
    fn extreme_rects_do_not_overflow() {
        let huge = rect(i32::MIN, i32::MIN, i32::MAX, i32::MAX);
        let p = resolve(ShowState::Normal, huge, &[primary()]);
        assert_eq!(p.restored, primary().work);
        let d = display("H", huge, huge, true);
        let p = resolve(ShowState::Maximized, huge, &[d]);
        assert_eq!(p.restored, huge);
    }

    #[test]
    fn workspace_conversion_uses_work_offset() {
        let l = left(); // taskbar on top: offset (0, 48)
        let r = rect(-1800, 100, -800, 900);
        let w = screen_to_workspace(r, l.monitor, l.work).unwrap();
        assert_eq!(w, rect(-1800, 52, -800, 852));
        assert_eq!(workspace_to_screen(w, l.monitor, l.work), Some(r));

        // Taskbar on the left of a negative-coordinate monitor: offset (62, 0).
        let m = rect(-2560, -1440, 0, 0);
        let wk = rect(-2498, -1440, 0, 0);
        let r = rect(-2400, -1000, -1400, -200);
        let w = screen_to_workspace(r, m, wk).unwrap();
        assert_eq!(w, rect(-2462, -1000, -1462, -200));
        assert_eq!(workspace_to_screen(w, m, wk), Some(r));

        // Bottom taskbar: offset zero, identity.
        let p = primary();
        assert_eq!(screen_to_workspace(r, p.monitor, p.work), Some(r));
    }

    #[test]
    fn workspace_conversion_round_trips_or_reports_overflow() {
        let m = rect(i32::MIN, i32::MIN, 0, 0);
        let wk = rect(i32::MAX - 1, i32::MAX - 1, i32::MAX, i32::MAX);
        // Offset ≈ 2³² does not fit: overflow is reported, never wrapped.
        assert_eq!(screen_to_workspace(rect(1, 1, 2, 2), wk, m), None);
        assert_eq!(workspace_to_screen(rect(1, 1, 2, 2), m, wk), None);

        // Offset 2³² − 2 still fits for a rect at the very bottom of the range.
        let r = rect(i32::MIN, i32::MIN, i32::MIN + 1, i32::MIN + 1);
        let w = workspace_to_screen(r, m, wk).unwrap();
        assert_eq!(screen_to_workspace(w, m, wk), Some(r));
    }

    #[test]
    fn final_placement_workspace_uses_target_display() {
        let r = rect(-1800, 100, -800, 900);
        let p = resolve(ShowState::Normal, r, &[primary(), left()]);
        assert_eq!(p.restored_workspace(), Some(rect(-1800, 52, -800, 852)));
    }
}
