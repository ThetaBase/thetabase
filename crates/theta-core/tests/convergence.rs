//! Property-based CRDT convergence suite.
//!
//! This is the first half of the Consistency Verification gate
//! (`docs/specs/08-test-validation-plan.md` §2): CRDT-typed fields must converge
//! to the same state regardless of merge order, asserted over randomized
//! operation interleavings rather than a handful of hand-written cases.
//!
//! What this suite does NOT yet cover, and what the full gate still requires
//! before M1 can be called done: network partitions, node crashes mid-merge, and
//! read-your-writes under concurrent sessions. Those need the storage engine and
//! a fault-injection harness — see ROADMAP M1's gate.

use proptest::prelude::*;
use theta_core::crdt::{Crdt, LwwRegister, OrSet, PnCounter, ReplicaId, RgaSequence, WriteStamp};

#[derive(Debug, Clone)]
enum CounterOp {
    Inc(u8, u64),
    Dec(u8, u64),
}

fn counter_ops() -> impl Strategy<Value = Vec<CounterOp>> {
    prop::collection::vec(
        prop_oneof![
            (0u8..4, 0u64..1000).prop_map(|(r, n)| CounterOp::Inc(r, n)),
            (0u8..4, 0u64..1000).prop_map(|(r, n)| CounterOp::Dec(r, n)),
        ],
        0..40,
    )
}

fn apply_counter(ops: &[CounterOp]) -> PnCounter {
    let mut c = PnCounter::new();
    for op in ops {
        match op {
            CounterOp::Inc(r, n) => c.increment(ReplicaId(*r as u64), *n),
            CounterOp::Dec(r, n) => c.decrement(ReplicaId(*r as u64), *n),
        }
    }
    c
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    /// Commutativity: merge order does not change the result.
    #[test]
    fn counter_merge_is_commutative(a in counter_ops(), b in counter_ops()) {
        let (ca, cb) = (apply_counter(&a), apply_counter(&b));
        prop_assert_eq!(ca.clone().merged(&cb).value(), cb.clone().merged(&ca).value());
    }

    /// Associativity: grouping does not change the result.
    #[test]
    fn counter_merge_is_associative(a in counter_ops(), b in counter_ops(), c in counter_ops()) {
        let (ca, cb, cc) = (apply_counter(&a), apply_counter(&b), apply_counter(&c));
        let left = ca.clone().merged(&cb).merged(&cc);
        let right = ca.clone().merged(&cb.clone().merged(&cc));
        prop_assert_eq!(left.value(), right.value());
    }

    /// Idempotence: merging the same state repeatedly changes nothing.
    #[test]
    fn counter_merge_is_idempotent(a in counter_ops(), b in counter_ops()) {
        let (ca, cb) = (apply_counter(&a), apply_counter(&b));
        let once = ca.clone().merged(&cb);
        let twice = once.clone().merged(&cb);
        prop_assert_eq!(once.value(), twice.value());
    }

    /// The LWW tie-break is a total order, so a winner exists and is the same
    /// on every replica — no ambiguity on concurrent write.
    /// The LWW tie-break is a total order, so a winner exists and is the same
    /// on every replica — no ambiguity on concurrent write.
    ///
    /// Stamps are constructed the way the log constructs them: `replica` is the
    /// branch and `commit` is that branch's monotonic commit id, so
    /// `(replica, commit)` is unique across the whole run. Timestamps are drawn
    /// from a deliberately small range so collisions — the interesting case —
    /// happen constantly.
    #[test]
    fn register_converges_on_one_deterministic_winner(
        writes in prop::collection::vec((0i64..5, any::<bool>(), 0u32..100), 1..20)
    ) {
        let mut a = LwwRegister::new();
        let mut b = LwwRegister::new();
        let (mut commit_a, mut commit_b) = (0u64, 0u64);

        for (ts, to_a, value) in writes.iter() {
            // Two replicas that never saw each other, each with its own branch
            // id and its own commit sequence.
            if *to_a {
                a.set(*value, WriteStamp::new(*ts, ReplicaId(1), commit_a));
                commit_a += 1;
            } else {
                b.set(*value, WriteStamp::new(*ts, ReplicaId(2), commit_b));
                commit_b += 1;
            }
        }
        prop_assert_eq!(a.clone().merged(&b).value(), b.clone().merged(&a).value());
    }

    /// OR-Set converges under concurrent add/remove of the same elements.
    #[test]
    fn orset_converges_under_concurrent_add_and_remove(
        ops in prop::collection::vec((0u8..2, 0u8..5, any::<bool>()), 0..40)
    ) {
        let mut a: OrSet<u8> = OrSet::new();
        let mut b: OrSet<u8> = OrSet::new();
        for (side, element, is_add) in ops {
            let (set, replica) = if side == 0 { (&mut a, ReplicaId(1)) } else { (&mut b, ReplicaId(2)) };
            if is_add { set.add(element, replica) } else { set.remove(&element) }
        }
        prop_assert_eq!(a.clone().merged(&b).value(), b.clone().merged(&a).value());
    }

    /// Concurrent sequence inserts converge to one consistent order, and no
    /// insert is lost.
    #[test]
    fn sequence_converges_and_loses_nothing(
        a_inserts in prop::collection::vec(0u32..100, 0..10),
        b_inserts in prop::collection::vec(0u32..100, 0..10),
    ) {
        let mut base: RgaSequence<u32> = RgaSequence::new();
        let anchor = base.push(u32::MAX, ReplicaId(0));

        let mut a = base.clone();
        let mut b = base.clone();
        for v in &a_inserts { a.insert_after(Some(anchor), *v, ReplicaId(1)); }
        for v in &b_inserts { b.insert_after(Some(anchor), *v, ReplicaId(2)); }

        let ab = a.clone().merged(&b);
        let ba = b.clone().merged(&a);
        prop_assert_eq!(ab.value(), ba.value());
        prop_assert_eq!(ab.value().len(), 1 + a_inserts.len() + b_inserts.len());
    }
}
