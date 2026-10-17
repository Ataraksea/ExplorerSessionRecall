//! Property test for `esr_core::plan::active::active_after_omission`.
//!
//! **Property 15: Active tab after omission**
//!
//! *For any* saved tab count *n*, saved active index *a* < *n*, and omission
//! mask leaving at least one tab, `active_after_omission` returns the new
//! index of tab *a* if it is kept. Otherwise it returns the new index of the
//! nearest kept tab left of *a*, or, if none, the nearest kept tab right of
//! *a*.
//!
//! **Validates: Requirements 5.4, 9.4**

use esr_core::plan::active::active_after_omission;
use proptest::collection::vec;
use proptest::prelude::*;

/// Independent reference: map each kept saved index to its new index, then
/// pick the saved active tab, else nearest kept to the left, else nearest
/// kept to the right. Out-of-range `saved_active` is clamped to the last tab.
fn reference(kept: &[bool], saved_active: usize) -> Option<usize> {
    if kept.is_empty() {
        return None;
    }
    let a = saved_active.min(kept.len() - 1);
    // new_of[i] = Some(new index) for kept tabs, None for omitted ones.
    let mut next = 0;
    let new_of: Vec<Option<usize>> = kept
        .iter()
        .map(|&k| {
            if k {
                next += 1;
                Some(next - 1)
            } else {
                None
            }
        })
        .collect();
    if let Some(n) = new_of[a] {
        return Some(n);
    }
    let mut i = a;
    while i > 0 {
        i -= 1;
        if let Some(n) = new_of[i] {
            return Some(n);
        }
    }
    new_of[a + 1..].iter().find_map(|&n| n)
}

/// A mask of length 0..40 with a mix of densities (all kept, all omitted,
/// sparse, dense) and a saved active index that is usually in range but
/// sometimes out of range.
fn case() -> impl Strategy<Value = (Vec<bool>, usize)> {
    let mask = prop_oneof![
        2 => vec(any::<bool>(), 0..40),
        1 => vec(prop::bool::weighted(0.15), 0..40),
        1 => vec(prop::bool::weighted(0.85), 0..40),
        1 => (0usize..40).prop_map(|n| vec![false; n]),
        1 => (0usize..40).prop_map(|n| vec![true; n]),
    ];
    mask.prop_flat_map(|m| {
        let n = m.len();
        let active = prop_oneof![
            4 => 0..n.max(1),
            1 => n..n + 10,
            1 => Just(usize::MAX),
        ];
        (Just(m), active)
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Property 15: matches the independent reference, and the rule-by-rule
    /// checks hold directly.
    #[test]
    fn active_after_omission_property((kept, saved_active) in case()) {
        let got = active_after_omission(&kept, saved_active);
        prop_assert_eq!(got, reference(&kept, saved_active));

        let kept_count = kept.iter().filter(|&&k| k).count();
        // None iff no tab is kept.
        prop_assert_eq!(got.is_none(), kept_count == 0);
        let Some(r) = got else { return Ok(()); };
        // Result indexes into the kept subsequence.
        prop_assert!(r < kept_count);

        // Saved index of the kept tab the result refers to.
        let chosen = kept
            .iter()
            .enumerate()
            .filter(|&(_, &k)| k)
            .nth(r)
            .map(|(i, _)| i)
            .unwrap();

        let a = saved_active.min(kept.len() - 1);
        if kept[a] {
            // Saved active tab kept: it stays active.
            prop_assert_eq!(chosen, a);
        } else if let Some(left) = (0..a).rev().find(|&i| kept[i]) {
            // Omitted, kept tab to the left: nearest such tab.
            prop_assert_eq!(chosen, left);
        } else {
            // Omitted, none to the left: nearest kept tab to the right,
            // which is the first kept tab overall (new index 0).
            let right = (a + 1..kept.len()).find(|&i| kept[i]).unwrap();
            prop_assert_eq!(chosen, right);
            prop_assert_eq!(r, 0);
        }
    }
}
