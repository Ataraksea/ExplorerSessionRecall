//! Smoke run of the `LiveSessionTracker` against the esr-sim reference model.
//!
//! Plays deterministic operation sequences (Property 1's operation mix) through
//! the simulated Explorer and checks that, once everything has settled,
//! `capture()` equals the reference snapshot, and that the tracker emitted the
//! reference's Session_Ends (cause and pre-close snapshot). The full
//! model-based property tests are tasks 7.8 and 7.9; this only guards the
//! tracker core (tasks 7.4, 7.5).

use std::collections::BTreeSet;

use esr_core::engine::{Command, Event, Millis, TimerId};
use esr_core::tracker::{LiveSessionTracker, SessionEnd};
use esr_sim::explorer::{ops_strategy, Op, OpsConfig, SimConfig, SimExplorer, TimedEvent, UiaNaming};
use esr_sim::reference::canonical;
use proptest::strategy::{Strategy, ValueTree};
use proptest::test_runner::TestRunner;

/// The tracker plus the tracker timers it set, delivered at their time.
#[derive(Default)]
struct Harness {
    tracker: LiveSessionTracker,
    timers: BTreeSet<(Millis, TimerId)>,
    ends: Vec<SessionEnd>,
    now: Millis,
}

impl Harness {
    fn deliver(&mut self, sim: &mut SimExplorer, at: Millis, event: &Event) {
        self.now = self.now.max(at);
        let out = self.tracker.handle(self.now, event);
        self.ends.extend(out.session_end);
        for c in out.commands {
            match c {
                Command::SetTimer { id, at } => {
                    self.timers.insert((at, id));
                }
                Command::AppendLog(_) => {}
                c => {
                    sim.handle_command(&c);
                }
            }
        }
    }

    /// Fires every timer due before `until`.
    fn fire_until(&mut self, sim: &mut SimExplorer, until: Millis) {
        while let Some(&(at, id)) = self.timers.first().filter(|(at, _)| *at < until) {
            self.timers.remove(&(at, id));
            self.deliver(sim, at, &Event::Timer(id));
        }
    }

    fn feed(&mut self, sim: &mut SimExplorer, events: Vec<TimedEvent>) {
        for e in events {
            self.fire_until(sim, e.at);
            self.deliver(sim, e.at, &e.event);
        }
    }
}

fn run(config: SimConfig, sequences: usize) {
    let mut runner = TestRunner::deterministic();
    let strategy = ops_strategy(&OpsConfig::tracking(), 1..80);
    let mut ends_compared = 0;
    for n in 0..sequences {
        let ops = strategy.new_tree(&mut runner).expect("strategy").current();
        let mut sim = SimExplorer::new(config.clone());
        let mut h = Harness::default();
        for op in &ops {
            let events = sim.apply(op);
            h.feed(&mut sim, events);
        }
        let events = sim.finish();
        h.feed(&mut sim, events);
        h.fire_until(&mut sim, Millis(u64::MAX));
        let (actual, _) = h.tracker.capture();
        let expected = sim.reference().snapshot();
        assert_eq!(canonical(&actual), canonical(&expected), "sequence {n}: {ops:#?}");

        // While the shell is lost the tracker cannot see windows or tabs the
        // reference knows, so Session_Ends are exact only without shell loss.
        if ops.iter().any(|op| matches!(op, Op::LoseShell)) {
            continue;
        }
        let actual_ends: Vec<_> = h.ends.iter().map(|e| (e.cause, canonical(&e.snapshot))).collect();
        let expected_ends: Vec<_> =
            sim.reference().session_ends().iter().map(|e| (e.cause, canonical(&e.snapshot))).collect();
        assert_eq!(actual_ends, expected_ends, "sequence {n}: {ops:#?}");
        ends_compared += expected_ends.len();
    }
    assert!(ends_compared >= 10, "only {ends_compared} Session_Ends compared");
}

#[test]
fn display_name_strips_match_the_reference() {
    run(SimConfig::default(), 150);
}

#[test]
fn full_path_strips_match_the_reference() {
    run(SimConfig { uia_naming: UiaNaming::FullPaths, ..SimConfig::default() }, 75);
}

#[test]
fn explicit_active_keys_match_the_reference() {
    run(SimConfig { report_active_key: true, reuse_hwnds: false, ..SimConfig::default() }, 75);
}
