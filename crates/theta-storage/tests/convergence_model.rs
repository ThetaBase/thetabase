//! A machine-checked model of convergence (ROADMAP-V3 M25, item 1).
//!
//! # What this replaces
//!
//! `claims.toml` says strong eventual consistency is "provable from the log and
//! CRDT design rather than asserted". Nobody had proved it. The evidence was two
//! example-based tests — that branches converge regardless of merge direction,
//! and that converged state survives a restart — which are true and are examples.
//!
//! This checks the property **exhaustively over a bounded space**: every
//! interleaving of a small set of operations across two branches, every merge
//! direction, every order. That is a model check rather than a proof about
//! unbounded histories, and the distinction is stated rather than blurred: what
//! it establishes is that no counterexample exists *below the bound*, which is a
//! different and weaker thing than TLA+ over an unbounded model.
//!
//! It is also the thing that finds real bugs. Almost every convergence bug that
//! has ever shipped is reachable in three or four operations.
//!
//! # The lesson from the classifier verification, applied here
//!
//! `exhaustive_classification.rs` recorded that its first three properties were
//! all of the form "never gets weaker", and that a family of properties pointing
//! one direction shares a blind spot: none of them could see a rule that had been
//! *deleted*.
//!
//! The equivalent trap here is checking only that merges **agree**. A merge
//! function that discarded everything would make both sides agree perfectly and
//! satisfy every convergence property. So agreement is checked alongside
//! **preservation** — that a converged state actually contains what was written —
//! and the two point in different directions on purpose.

use std::collections::{BTreeMap, BTreeSet};

use theta_core::crdt::{apply_op, CrdtState, OpContext, ReplicaId};
use theta_core::schema::CrdtKind;
use theta_core::{CrdtOp, Value};

/// One operation in the model.
///
/// Deliberately small. The space is exponential, so the value of each extra
/// operation kind has to beat the cost of every combination it multiplies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Increment(i64),
    Add(u8),
}

impl Op {
    fn to_crdt(self) -> CrdtOp {
        match self {
            Op::Increment(by) => CrdtOp::Increment { by },
            Op::Add(element) => CrdtOp::SetAdd {
                element: Value::Int(element as i64),
            },
        }
    }
}

/// Every sequence of `len` operations drawn from `alphabet`.
fn sequences(alphabet: &[Op], len: usize) -> Vec<Vec<Op>> {
    if len == 0 {
        return vec![Vec::new()];
    }
    let shorter = sequences(alphabet, len - 1);
    let mut out = Vec::with_capacity(shorter.len() * alphabet.len());
    for prefix in shorter {
        for op in alphabet {
            let mut next = prefix.clone();
            next.push(*op);
            out.push(next);
        }
    }
    out
}

/// Split a sequence between two branches in every possible way.
///
/// Order within each branch is preserved, which is what makes this a model of
/// two branches rather than of arbitrary shuffling: a branch has a total order
/// and the model must not quietly assume otherwise.
fn splits(ops: &[Op]) -> Vec<(Vec<Op>, Vec<Op>)> {
    let mut out = Vec::new();
    for mask in 0..(1u32 << ops.len()) {
        let mut left = Vec::new();
        let mut right = Vec::new();
        for (i, op) in ops.iter().enumerate() {
            if mask & (1 << i) == 0 {
                left.push(*op);
            } else {
                right.push(*op);
            }
        }
        out.push((left, right));
    }
    out
}

/// Replay a branch's operations into a fresh state.
///
/// `replica` distinguishes the two branches, which is what a CRDT needs to keep
/// concurrent operations apart. Two branches sharing a replica id would converge
/// trivially and the model would prove nothing.
fn run(kind: CrdtKind, ops: &[Op], replica: u64) -> CrdtState {
    let mut state = CrdtState::empty(kind);
    for (seq, op) in ops.iter().enumerate() {
        let applied = apply_op(
            &mut state,
            &op.to_crdt(),
            OpContext {
                replica: ReplicaId(replica),
                commit: seq as u64 + 1,
                timestamp_ms: seq as i64 + 1,
            },
        );
        assert!(applied, "the model must not generate ops of the wrong kind");
    }
    state
}

fn merged(mut a: CrdtState, b: &CrdtState) -> CrdtState {
    assert!(a.merge(b), "both sides are the same kind by construction");
    a
}

fn counter_alphabet() -> Vec<Op> {
    vec![Op::Increment(1), Op::Increment(-1), Op::Increment(5)]
}

#[test]
fn counters_converge_over_every_interleaving_up_to_five_operations() {
    // Exhaustive below the bound rather than sampled. 3^5 sequences times 2^5
    // splits is about 8,000 cases, which runs in well under a second and covers
    // every shape a convergence bug of this size can take.
    let alphabet = counter_alphabet();
    let mut checked = 0u64;

    for len in 0..=5 {
        for ops in sequences(&alphabet, len) {
            for (left_ops, right_ops) in splits(&ops) {
                let left = run(CrdtKind::Counter, &left_ops, 1);
                let right = run(CrdtKind::Counter, &right_ops, 2);

                let left_then_right = merged(left.clone(), &right);
                let right_then_left = merged(right.clone(), &left);

                assert_eq!(
                    left_then_right.value(),
                    right_then_left.value(),
                    "merge order changed the result for {left_ops:?} against {right_ops:?}"
                );
                checked += 1;
            }
        }
    }

    // Guard against the enumeration collapsing. A loop that stops covering the
    // space still passes every assertion inside it — the same failure the
    // exhaustive classifier check guards against.
    assert!(
        checked > 5_000,
        "only {checked} interleavings were checked; the enumeration has collapsed"
    );
}

#[test]
fn merging_is_idempotent_over_every_interleaving() {
    // Merging an already-merged branch must be a no-op. This is the property
    // that makes a retried merge safe, and it is the one a state-based CRDT is
    // supposed to give for free — which is exactly why it is worth checking
    // rather than assuming.
    let alphabet = counter_alphabet();
    for len in 0..=4 {
        for ops in sequences(&alphabet, len) {
            for (left_ops, right_ops) in splits(&ops) {
                let left = run(CrdtKind::Counter, &left_ops, 1);
                let right = run(CrdtKind::Counter, &right_ops, 2);

                let once = merged(left.clone(), &right);
                let twice = merged(once.clone(), &right);

                assert_eq!(
                    once.value(),
                    twice.value(),
                    "merging twice differed from merging once: {left_ops:?} / {right_ops:?}"
                );
            }
        }
    }
}

#[test]
fn merging_is_associative_over_three_branches() {
    // The property two branches cannot exercise. Three agents forking from one
    // head is the ordinary case in this product, and `(a ∪ b) ∪ c` differing
    // from `a ∪ (b ∪ c)` is a convergence bug that a two-branch test cannot see.
    let alphabet = counter_alphabet();
    for len in 0..=3 {
        for ops in sequences(&alphabet, len) {
            for (first, rest) in splits(&ops) {
                for (second, third) in splits(&rest) {
                    let a = run(CrdtKind::Counter, &first, 1);
                    let b = run(CrdtKind::Counter, &second, 2);
                    let c = run(CrdtKind::Counter, &third, 3);

                    let left = merged(merged(a.clone(), &b), &c);
                    let right_full = merged(a.clone(), &merged(b.clone(), &c));

                    assert_eq!(
                        left.value(),
                        right_full.value(),
                        "association changed the result: {first:?} / {second:?} / {third:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn convergence_preserves_what_was_written_and_does_not_merely_agree() {
    // **The property the others share a blind spot about.**
    //
    // `exhaustive_classification.rs` recorded that three properties all of the
    // form "never gets weaker" could not see a rule that had been deleted. The
    // equivalent here is that agreement, idempotence and associativity are all
    // satisfied perfectly by a merge function that discards everything.
    //
    // So this checks the other direction: a converged counter holds the sum of
    // what both sides did.
    let alphabet = counter_alphabet();
    for len in 1..=4 {
        for ops in sequences(&alphabet, len) {
            for (left_ops, right_ops) in splits(&ops) {
                let left = run(CrdtKind::Counter, &left_ops, 1);
                let right = run(CrdtKind::Counter, &right_ops, 2);

                let converged = merged(left.clone(), &right);

                let expected: i64 = ops
                    .iter()
                    .map(|op| match op {
                        Op::Increment(by) => *by,
                        _ => 0,
                    })
                    .sum();

                assert_eq!(
                    converged.value(),
                    Value::Int(expected),
                    "a merge that agreed but lost writes: {left_ops:?} / {right_ops:?}"
                );
            }
        }
    }
}

#[test]
fn sets_converge_and_keep_what_was_added() {
    // The same pair of directions for an OR-Set: both sides agree, *and* the
    // result contains what was added. An empty set satisfies the first alone.
    let mut checked = 0u64;
    for len in 0..=4 {
        for ops in sequences(&[Op::Add(1), Op::Add(2), Op::Add(3)], len) {
            for (left_ops, right_ops) in splits(&ops) {
                let left = run(CrdtKind::Set, &left_ops, 1);
                let right = run(CrdtKind::Set, &right_ops, 2);

                let a = merged(left.clone(), &right);
                let b = merged(right.clone(), &left);

                assert_eq!(a.value(), b.value(), "merge order changed a set");

                // Preservation, in the direction agreement cannot see: every
                // *distinct* element either side added is still there. Counted
                // as distinct values rather than as operations, because adding
                // the same element twice is one element and a model that
                // expected two would be asserting the wrong thing.
                if let Value::List(items) = a.value() {
                    let present: BTreeSet<String> =
                        items.iter().map(|v| format!("{v:?}")).collect();
                    let written: BTreeSet<String> = left_ops
                        .iter()
                        .chain(right_ops.iter())
                        .map(|op| match op {
                            Op::Add(element) => format!("{:?}", Value::Int(*element as i64)),
                            Op::Increment(_) => unreachable!("set alphabet"),
                        })
                        .collect();
                    assert_eq!(
                        present, written,
                        "a merge that agreed but lost elements: {left_ops:?} / {right_ops:?}"
                    );
                }
                checked += 1;
            }
        }
    }
    assert!(
        checked > 500,
        "only {checked} set interleavings were checked"
    );
}

#[test]
fn the_model_is_exhaustive_below_its_bound_and_says_so() {
    // What this establishes and what it does not, as an assertion rather than
    // only as a comment. A model check finds no counterexample below a bound;
    // it does not prove there is none above it, and the difference is the whole
    // reason `claims.toml` cannot say "proved".
    let alphabet = counter_alphabet();
    let mut space: BTreeMap<usize, usize> = BTreeMap::new();
    for len in 0..=5 {
        let count: usize = sequences(&alphabet, len)
            .iter()
            .map(|ops| splits(ops).len())
            .sum();
        space.insert(len, count);
    }

    assert_eq!(space[&0], 1, "the empty history is one case");
    assert!(
        space[&5] > space[&4],
        "the space must grow with the bound, or the enumeration is not enumerating"
    );
}
