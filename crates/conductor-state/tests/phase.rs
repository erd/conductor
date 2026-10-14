use std::collections::BTreeSet;

use conductor_state::Phase::{
    self, Abandoned, AwaitingMerge, Briefed, Failed, Intake, Merged, NeedsDecision, Review,
    Spawning, TornDown, Working,
};

/// The edges drawn in the lifecycle diagram in `docs/design.md`.
const DIAGRAM: [(Phase, Phase); 14] = [
    (Intake, Briefed),
    (Briefed, Spawning),
    (Spawning, Working),
    (Working, NeedsDecision),
    (NeedsDecision, Working),
    (Working, Review),
    (Review, Working),
    (Review, AwaitingMerge),
    (AwaitingMerge, Working),
    (AwaitingMerge, Merged),
    (Review, TornDown),
    (Merged, TornDown),
    (Failed, TornDown),
    (Abandoned, TornDown),
];

/// Every legal transition: the diagram, plus any live phase to `failed` or `abandoned`, plus
/// `failed` to `abandoned`.
fn legal() -> BTreeSet<(usize, usize)> {
    let index = |phase: Phase| Phase::ALL.iter().position(|&p| p == phase).unwrap();
    let mut edges: BTreeSet<_> = DIAGRAM
        .iter()
        .map(|&(from, to)| (index(from), index(to)))
        .collect();
    for from in Phase::ALL {
        if !matches!(from, TornDown | Failed | Abandoned) {
            edges.insert((index(from), index(Failed)));
            edges.insert((index(from), index(Abandoned)));
        }
    }
    edges.insert((index(Failed), index(Abandoned)));
    edges
}

#[test]
fn diagram_edges_are_legal() {
    for (from, to) in DIAGRAM {
        assert!(Phase::can_transition(from, to), "{from:?} -> {to:?}");
    }
}

#[test]
fn failed_and_abandoned_are_reachable_from_every_live_phase() {
    for from in [
        Intake,
        Briefed,
        Spawning,
        Working,
        NeedsDecision,
        Review,
        AwaitingMerge,
        Merged,
    ] {
        assert!(Phase::can_transition(from, Failed), "{from:?} -> Failed");
        assert!(
            Phase::can_transition(from, Abandoned),
            "{from:?} -> Abandoned"
        );
    }
    assert!(Phase::can_transition(Failed, Abandoned));
}

#[test]
fn illegal_samples() {
    for (from, to) in [
        (Intake, Working),
        (Intake, Spawning),
        (Briefed, Working),
        (Spawning, Review),
        (Working, Merged),
        (Working, AwaitingMerge),
        (Working, TornDown),
        (NeedsDecision, Review),
        (Review, Merged),
        (Review, NeedsDecision),
        (AwaitingMerge, Review),
        (AwaitingMerge, TornDown),
        (Merged, Working),
        (TornDown, Failed),
        (TornDown, Abandoned),
        (TornDown, Intake),
        (Failed, Working),
        (Abandoned, Working),
        (Abandoned, Failed),
        (Working, Working),
        (Failed, Failed),
        (Abandoned, Abandoned),
    ] {
        assert!(!Phase::can_transition(from, to), "{from:?} -> {to:?}");
    }
}

#[test]
fn every_pair_matches_the_lifecycle() {
    let legal = legal();
    for (i, &from) in Phase::ALL.iter().enumerate() {
        for (j, &to) in Phase::ALL.iter().enumerate() {
            assert_eq!(
                Phase::can_transition(from, to),
                legal.contains(&(i, j)),
                "{from:?} -> {to:?}"
            );
        }
    }
}
