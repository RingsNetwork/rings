//! The first-edge law of a registration (#855): a heartbeat that began with no admitted edge
//! arms an early publish, the first admitted edge fires it, and it fires at most once between two
//! heartbeats, whatever the edges do.

use super::early_publish_armed;
use super::republish_due;

/// One step of a registration daemon's world.
#[derive(Clone, Copy, Debug)]
enum Step {
    /// The heartbeat interval elapses: a heartbeat publishes.
    Heartbeat,
    /// An edge is admitted.
    Admit,
    /// An edge is retired, if any is current.
    Retire,
}

/// The truth tables of both predicates.
#[test]
fn test_only_a_heartbeat_begun_alone_arms_and_an_edge_fires_it() {
    assert!(early_publish_armed(true, 0));
    assert!(!early_publish_armed(true, 1));
    assert!(!early_publish_armed(false, 0));
    assert!(!early_publish_armed(false, 3));
    assert!(republish_due(true, 1));
    assert!(republish_due(true, 4));
    assert!(!republish_due(true, 0));
    assert!(!republish_due(false, 1));
}

/// What the trigger did over one trace, by heartbeat interval.
#[derive(Debug, Default)]
struct Outcome {
    /// The most early publishes between two heartbeats.
    most_early_between_heartbeats: usize,
    /// Whether an interval that began alone saw an admission but no early publish.
    missed_a_first_edge: bool,
}

/// Run the daemon's trigger over `trace`, from a first heartbeat at listen with no edge: after
/// every step, a due early publish fires, as the daemon's wait on the edge changes does.
fn run(trace: &[Step]) -> Outcome {
    let mut outcome = Outcome::default();
    let mut edges = 0_usize;
    let mut armed = early_publish_armed(true, edges);
    let mut began_alone = edges == 0;
    let mut admitted = false;
    let mut early = 0_usize;
    let mut close_interval = |early: usize, began_alone: bool, admitted: bool| {
        outcome.most_early_between_heartbeats = outcome.most_early_between_heartbeats.max(early);
        outcome.missed_a_first_edge |= began_alone && admitted && early == 0;
    };
    for step in trace {
        match step {
            Step::Heartbeat => {
                close_interval(early, began_alone, admitted);
                armed = early_publish_armed(true, edges);
                began_alone = edges == 0;
                admitted = false;
                early = 0;
            }
            Step::Admit => {
                edges += 1;
                admitted = true;
            }
            Step::Retire => edges = edges.saturating_sub(1),
        }
        if republish_due(armed, edges) {
            armed = early_publish_armed(false, edges);
            early += 1;
        }
    }
    close_interval(early, began_alone, admitted);
    outcome
}

/// Every trace of up to 8 steps: an early publish fires at most once between two heartbeats,
/// and an interval that began alone and saw an edge admitted had one.
#[test]
fn test_at_most_one_early_publish_per_heartbeat_and_none_missed() {
    let steps = [Step::Heartbeat, Step::Admit, Step::Retire];
    let mut traces = vec![Vec::new()];
    for _ in 0..8 {
        let longer = traces
            .iter()
            .flat_map(|trace: &Vec<Step>| {
                steps.iter().map(move |step| {
                    let mut next = trace.clone();
                    next.push(*step);
                    next
                })
            })
            .collect::<Vec<_>>();
        for trace in &longer {
            let outcome = run(trace);
            assert!(
                outcome.most_early_between_heartbeats <= 1,
                "more than one early publish in an interval of {trace:?}"
            );
            assert!(
                !outcome.missed_a_first_edge,
                "a first edge without an early publish in {trace:?}"
            );
        }
        traces = longer;
    }
}
