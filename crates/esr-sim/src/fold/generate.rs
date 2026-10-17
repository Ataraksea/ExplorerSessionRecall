//! Generators for folding scenarios (task 21.4): candidate operations for
//! interleaving with the tracking operations of
//! [`op_strategy`](crate::explorer::op_strategy), and failure schedules.
//!
//! - [`fold_op_strategy`]: one operation of the folding part of the simulator
//!   (candidates, Ctrl+N, duplicate tab, drag-out, Virtual_Desktops, the
//!   foreground, unresponsive and closing windows, the first-navigation
//!   delay). [`OpsConfig::folding`](crate::explorer::OpsConfig) mixes these
//!   into [`op_strategy`](crate::explorer::op_strategy).
//! - [`fold_faults_strategy`]: a [`FoldFaults`] schedule for any mix of failed
//!   tab creations, unconfirmed activations, windows that close while they are
//!   a target, and candidates that ignore `CloseWindow` (Req 14.13, 14.14,
//!   14.16, 14.23, 14.24). [`failure_schedule`] is the same for the first
//!   `n` commands only, which keeps a scenario's faults where its commands
//!   are.
//! - [`selection_strategy`]: the items a candidate was opened to select.
//!
//! Requirements 14.13, 14.14, 14.16, 14.23, 14.24.

use std::collections::{BTreeMap, BTreeSet};

use esr_core::engine::ItemId;
use proptest::prelude::*;

use super::{
    ActivateFault, ActivateMode, CreateTabFault, DesktopPlace, DesktopSpec, FoldFaults, TabWindowFault,
};
use crate::explorer::{Op, Pick, ProcessChoice};

fn pick() -> impl Strategy<Value = Pick> {
    any::<u16>().prop_map(Pick)
}

/// 0–3 distinct items to select, each a few bytes (so that items collide
/// across candidates).
pub fn selection_strategy() -> impl Strategy<Value = Vec<ItemId>> {
    prop_oneof![
        3 => Just(Vec::new()),
        2 => proptest::collection::btree_set(0u8..6, 1..=3)
            .prop_map(|set| set.into_iter().map(|n| ItemId(vec![n, n.wrapping_mul(7)])).collect()),
    ]
}

/// Which windows a candidate's `desktop` lists: mostly the simulated desktop.
pub fn desktop_spec_strategy() -> impl Strategy<Value = DesktopSpec> {
    prop_oneof![
        6 => Just(DesktopSpec::Current),
        2 => Just(DesktopSpec::All),
        1 => Just(DesktopSpec::Unknown),
        2 => proptest::collection::vec(pick(), 0..4).prop_map(DesktopSpec::Windows),
    ]
}

fn desktop_place() -> impl Strategy<Value = DesktopPlace> {
    prop_oneof![
        4 => (0u8..4).prop_map(DesktopPlace::On),
        1 => Just(DesktopPlace::AllDesktops),
    ]
}

/// One folding operation (see the module docs).
pub fn fold_op_strategy() -> BoxedStrategy<Op> {
    let process = prop_oneof![
        4 => Just(ProcessChoice::Main),
        1 => pick().prop_map(ProcessChoice::Existing),
        1 => Just(ProcessChoice::New),
    ];
    // Changes of the environment a fold has to cope with.
    let environment = prop_oneof![
        1 => any::<bool>().prop_map(|readable| Op::SetDesktopMembership { readable }),
        1 => Just(Op::FocusApplication),
        2 => (pick(), any::<bool>()).prop_map(|(win, unresponsive)| Op::SetUnresponsive { win, unresponsive }),
        1 => pick().prop_map(|win| Op::BeginClosing { win }),
        1 => proptest::option::weighted(0.8, 0u64..=1_500).prop_map(|ms| Op::SetFirstNavigation { ms }),
    ];
    prop_oneof![
        6 => (pick(), selection_strategy(), prop::bool::weighted(0.1), desktop_spec_strategy(), proptest::option::of(pick()), process)
            .prop_map(|(location, selection, bypass_keys, desktop, foreground, process)| Op::OpenCandidate {
                location,
                selection,
                bypass_keys,
                desktop,
                foreground,
                process,
            }),
        2 => prop::bool::weighted(0.1).prop_map(|bypass_keys| Op::NewWindow { bypass_keys }),
        1 => pick().prop_map(|win| Op::DuplicateTab { win }),
        2 => (pick(), pick(), -200i32..=200, -200i32..=200)
            .prop_map(|(win, tab, dx, dy)| Op::DragOutCandidate { win, tab, dx, dy }),
        1 => (0u8..4).prop_map(|desktop| Op::SwitchDesktop { desktop }),
        1 => (pick(), desktop_place()).prop_map(|(win, to)| Op::MoveToDesktop { win, to }),
        1 => environment,
    ]
    .boxed()
}

fn create_tab_fault() -> impl Strategy<Value = CreateTabFault> {
    let tab_window = prop_oneof![
        4 => Just(TabWindowFault::Normal),
        2 => (0u64..3_000).prop_map(|extra_ms| TabWindowFault::Late { extra_ms }),
        1 => Just(TabWindowFault::Never),
    ];
    (tab_window, prop_oneof![3 => Just(0u64), 2 => 0u64..2_000], proptest::option::weighted(0.15, 0u64..400))
        .prop_map(|(tab_window, registration_delay_ms, target_closes_after_ms)| CreateTabFault {
            tab_window,
            registration_delay_ms,
            target_closes_after_ms,
        })
}

fn activate_fault() -> impl Strategy<Value = ActivateFault> {
    let mode = prop_oneof![
        3 => Just(ActivateMode::Normal),
        2 => (0u64..3_000).prop_map(|extra_ms| ActivateMode::Late { extra_ms }),
        2 => Just(ActivateMode::Unconfirmed),
        1 => Just(ActivateMode::Ignored),
    ];
    (mode, proptest::option::weighted(0.15, 0u64..400))
        .prop_map(|(mode, target_closes_after_ms)| ActivateFault { mode, target_closes_after_ms })
}

/// A failure schedule over the first `commands` commands of each kind: each
/// of them fails with a probability of about a third. `ignore_first_close`
/// is set for about one schedule in four.
pub fn failure_schedule(commands: u64) -> impl Strategy<Value = FoldFaults> {
    let ordinals = move || proptest::collection::btree_set(0..commands.max(1), 0..=commands.min(6) as usize);
    (
        ordinals().prop_flat_map(|set| {
            let set: Vec<u64> = set.into_iter().collect();
            proptest::collection::vec(create_tab_fault(), set.len())
                .prop_map(move |faults| set.iter().copied().zip(faults).collect::<BTreeMap<_, _>>())
        }),
        ordinals().prop_flat_map(|set| {
            let set: Vec<u64> = set.into_iter().collect();
            proptest::collection::vec(activate_fault(), set.len())
                .prop_map(move |faults| set.iter().copied().zip(faults).collect::<BTreeMap<_, _>>())
        }),
        ordinals(),
        prop::bool::weighted(0.25),
    )
        .prop_map(|(create_tab, activate_tab, ignore_close, ignore_first_close)| FoldFaults {
            create_tab,
            activate_tab,
            ignore_close: ignore_close.into_iter().collect::<BTreeSet<_>>(),
            ignore_first_close,
        })
}

/// A failure schedule over the first eight commands of each kind.
pub fn fold_faults_strategy() -> impl Strategy<Value = FoldFaults> {
    failure_schedule(8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explorer::{ops_strategy, OpsConfig, SimConfig, SimExplorer};
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;

    #[test]
    fn folding_operations_are_generated_only_when_asked_for() {
        let mut runner = TestRunner::deterministic();
        let mut count = |cfg: &OpsConfig| {
            let tree = ops_strategy(cfg, 200..=200).new_tree(&mut runner).unwrap();
            tree.current().iter().filter(|op| matches!(op, Op::OpenCandidate { .. } | Op::NewWindow { .. })).count()
        };
        assert_eq!(count(&OpsConfig::all()), 0);
        assert!(count(&OpsConfig::folding()) > 0);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]

        /// Whatever mix of tracking and folding operations runs, with any
        /// failure schedule, the simulator settles and has nothing left to do.
        ///
        /// **Validates: Requirements 14.13, 14.14, 14.23, 14.24**
        #[test]
        fn mixed_operations_with_any_failure_schedule_settle(
            ops in crate::explorer::ops_strategy(&OpsConfig::folding(), 0..60),
            faults in fold_faults_strategy(),
        ) {
            let mut sim = SimExplorer::new(SimConfig {
                fold: crate::fold::FoldConfig { report_tab_started: true, faults, ..Default::default() },
                ..SimConfig::default()
            });
            sim.apply_all(&ops);
            for w in sim.windows().iter().map(|w| w.key).collect::<Vec<_>>() {
                sim.set_unresponsive(w, false);
            }
            sim.finish();
            prop_assert!(sim.next_due().is_none());
        }
    }
}
