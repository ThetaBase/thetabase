//! Proving a result omitted nothing (ROADMAP-V3 M25).
//!
//! # The gap this closes
//!
//! [`crate::inclusion`] proves an entry is in the history. It cannot prove the
//! server told you about every entry it should have, and it says so: a server
//! answering "the customers whose plan is `free`" can leave one out, and every
//! row it *did* return still verifies perfectly. Presence is cheap to prove;
//! absence of anything else is not, because the client would need to know the
//! full key set already — which is the thing it asked for.
//!
//! That asymmetry is a property of the chain, not of proofs in general. A chain
//! orders entries by when they were written, and "what keys exist right now" is
//! a fact about the fold over it, not about any one link. So this builds the
//! structure `inclusion.rs` names and does not have: an **authenticated ordered
//! map** — a Merkle tree over the live key set, in sorted order.
//!
//! # Why sorted order is the whole trick
//!
//! Once leaves are ordered by key and each leaf knows its index, adjacency is
//! provable. To show that a range query returned everything, the server returns
//! the matches *and the two keys immediately outside the range*, with a path for
//! each. If the client verifies that those flanking leaves are at indices
//! `first - 1` and `last + 1`, there is no room left: a hidden key would have to
//! sit at an index that is already occupied by a leaf the client has verified.
//!
//! The same structure proves a key is absent — show the two leaves it would sit
//! between, and that they are adjacent.
//!
//! # What it still does not establish
//!
//! The root has to come from somewhere the operator does not control, exactly as
//! for inclusion proofs; see [`crate::anchor`]. Verified against a root the
//! server just supplied, a completeness proof shows the server is internally
//! consistent, which is not the property anybody wanted.
//!
//! It also proves completeness *of the map at that root*. A server that builds
//! the map from a doctored fold has produced an honest proof of a dishonest
//! state — which is what the hash chain is for, and why the two compose.

use serde::{Deserialize, Serialize};
use theta_core::hash::ContentHash;

/// Domain separation for the two node kinds.
///
/// Without it, a leaf whose bytes happen to look like a concatenation of two
/// child hashes could be presented as an internal node, or the reverse — the
/// standard second-preimage attack on Merkle trees. One byte makes the two
/// preimage spaces disjoint.
const LEAF_TAG: &[u8] = b"thetabase.completeness.leaf.v1";
const NODE_TAG: &[u8] = b"thetabase.completeness.node.v1";

fn leaf_hash(key: &str, value: &ContentHash) -> ContentHash {
    // The key's length is hashed in, not just its bytes. Otherwise ("ab", "c")
    // and ("a", "bc") produce the same leaf, and a server could rename a row by
    // moving the boundary.
    ContentHash::of_fields(&[
        LEAF_TAG,
        &(key.len() as u64).to_be_bytes(),
        key.as_bytes(),
        &value.0,
    ])
}

fn node_hash(left: &ContentHash, right: &ContentHash) -> ContentHash {
    ContentHash::of_fields(&[NODE_TAG, &left.0, &right.0])
}

/// One key and the hash of its value, as the map sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MapEntry {
    pub key: String,
    pub value: ContentHash,
}

/// An authenticated ordered map over the live key set.
///
/// Built from a snapshot rather than maintained incrementally. That is the
/// slower choice and it is the one that cannot drift: an incremental tree that
/// missed an update would produce proofs that verify against a root describing
/// a state the database was never in, which is a worse failure than being slow.
#[derive(Debug, Clone)]
pub struct OrderedMap {
    /// Leaves, sorted by key, deduplicated.
    entries: Vec<MapEntry>,
    /// `levels[0]` is the leaf hashes; the last level is the single root.
    levels: Vec<Vec<ContentHash>>,
}

impl OrderedMap {
    /// Build the map from whatever the fold currently holds.
    ///
    /// Duplicate keys are a caller error and the last one wins, which matches
    /// what a fold over the log does. Sorting happens here rather than being
    /// required of the caller: a map built from unsorted input would produce
    /// proofs whose adjacency claims are meaningless, and that is not a mistake
    /// worth leaving available.
    pub fn build(mut entries: Vec<MapEntry>) -> Self {
        entries.sort_by(|a, b| a.key.cmp(&b.key));
        entries.dedup_by(|a, b| a.key == b.key);

        let leaves: Vec<ContentHash> = entries
            .iter()
            .map(|e| leaf_hash(&e.key, &e.value))
            .collect();

        let mut levels = vec![leaves];
        while levels.last().map(Vec::len).unwrap_or(0) > 1 {
            let below = levels.last().expect("a level exists");
            let mut up = Vec::with_capacity(below.len().div_ceil(2));
            for pair in below.chunks(2) {
                up.push(match pair {
                    [l, r] => node_hash(l, r),
                    // An odd node is carried up rather than duplicated.
                    // Duplicating it is the classic CVE-2012-2459 shape: two
                    // different leaf sets produce the same root, which would
                    // let a server prove a result it never had.
                    [l] => *l,
                    _ => unreachable!("chunks(2) yields one or two"),
                });
            }
            levels.push(up);
        }

        Self { entries, levels }
    }

    /// The root a client checks proofs against.
    ///
    /// An empty map has a root of zero rather than an arbitrary constant, so
    /// "there is nothing here" is a statement a client can recognise instead of
    /// a hash it has to be told the meaning of.
    pub fn root(&self) -> ContentHash {
        match self.levels.last().and_then(|top| top.first()) {
            Some(root) if !self.entries.is_empty() => *root,
            _ => ContentHash::ZERO,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The sibling hashes from `index` up to the root.
    fn path(&self, index: usize) -> Vec<ContentHash> {
        let mut siblings = Vec::new();
        let mut i = index;
        for level in &self.levels {
            if level.len() <= 1 {
                break;
            }
            let sibling = if i.is_multiple_of(2) { i + 1 } else { i - 1 };
            // No sibling means this node was carried up unchanged, so there is
            // nothing to combine with at this level.
            if let Some(hash) = level.get(sibling) {
                siblings.push(*hash);
            }
            i /= 2;
        }
        siblings
    }

    fn at(&self, index: usize) -> Option<ProvenEntry> {
        let entry = self.entries.get(index)?.clone();
        Some(ProvenEntry {
            index,
            entry,
            path: self.path(index),
        })
    }

    /// Prove that `matches` is *every* key in `[start, end)`.
    ///
    /// The proof carries the matched entries plus the neighbours on either side
    /// of the range. Those neighbours are what make it a completeness proof
    /// rather than a bundle of inclusion proofs.
    pub fn prove_range(&self, start: &str, end: &str) -> RangeProof {
        let first = self.entries.partition_point(|e| e.key.as_str() < start);
        let last = self.entries.partition_point(|e| e.key.as_str() < end);

        let matched: Vec<ProvenEntry> = (first..last).filter_map(|i| self.at(i)).collect();

        RangeProof {
            start: start.to_string(),
            end: end.to_string(),
            // `first` is where the range begins in sort order, so `first - 1` is
            // the last key before it. Absent when the range starts at the very
            // beginning of the map, which is itself the boundary evidence.
            left: first.checked_sub(1).and_then(|i| self.at(i)),
            right: self.at(last),
            total: self.entries.len(),
            matched,
            absent: None,
        }
    }

    /// Prove that `key` is not in the map.
    pub fn prove_absent(&self, key: &str) -> RangeProof {
        // An empty half-open range at the key: nothing can match it, and the
        // flanking entries show there is no room for the key between them.
        RangeProof {
            absent: Some(key.to_string()),
            ..self.prove_range(key, key)
        }
    }
}

/// One entry, with the path that ties it to the root and its position.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProvenEntry {
    /// Position in sorted order. This is what makes adjacency checkable.
    pub index: usize,
    pub entry: MapEntry,
    pub path: Vec<ContentHash>,
}

/// A claim that a set of entries is everything in a range.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RangeProof {
    pub start: String,
    pub end: String,
    pub matched: Vec<ProvenEntry>,
    /// The key immediately before the range, if there is one.
    pub left: Option<ProvenEntry>,
    /// The key immediately after the range, if there is one.
    pub right: Option<ProvenEntry>,
    /// How many keys the map holds in total, so a client can recognise the
    /// boundaries as boundaries.
    pub total: usize,
    /// The key this proof claims is absent, when that is what it claims.
    ///
    /// Carried explicitly rather than inferred from an empty range. An absence
    /// proof is an empty range `[k, k)`, and a key equal to `end` is legitimately
    /// *outside* a half-open range -- so the boundary check accepts the right
    /// neighbour being exactly `k`, and a proof of "k is absent" verified while
    /// carrying k in it. The engine test caught it; the range checks alone
    /// cannot, because for a range that answer is correct.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub absent: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CompletenessError {
    #[error("an entry's path does not reconstruct the root")]
    BadPath { key: String },

    #[error("the entries are not in sorted order: {earlier:?} is listed before {later:?}")]
    OutOfOrder { earlier: String, later: String },

    #[error("the proven entries are not adjacent: index {expected} is missing between them")]
    Gap { expected: usize },

    #[error("{key:?} was returned but is outside the range being proved")]
    OutsideRange { key: String },

    #[error("{key:?} is claimed absent, and the proof carries it")]
    ClaimedAbsentButPresent { key: String },

    #[error("the left neighbour {key:?} is inside the range, so it proves no boundary")]
    LeftNeighbourInside { key: String },

    #[error("the right neighbour {key:?} is inside the range, so it proves no boundary")]
    RightNeighbourInside { key: String },

    #[error(
        "the range is not bounded below: no left neighbour, and the first proven \
         entry is at index {index} rather than 0, so keys before it are unaccounted for"
    )]
    UnboundedBelow { index: usize },

    #[error(
        "the range is not bounded above: no right neighbour, and the last proven \
         entry is at index {index} of {total}, so keys after it are unaccounted for"
    )]
    UnboundedAbove { index: usize, total: usize },
}

/// Recompute the root from one entry and its path.
///
/// `total` is not decoration. A level with an odd node carries it up unchanged
/// rather than duplicating it, so at that level the path contains no sibling --
/// and a verifier stepping through the siblings alone cannot tell a skipped
/// level from a consumed one. Its index then diverges from the real position
/// and every proof over a tree with an odd level fails to reconstruct.
///
/// Knowing the leaf count lets the verifier recompute each level's width and
/// ask the same question the builder asked: does this node have a sibling here?
fn reconstruct(proven: &ProvenEntry, total: usize) -> ContentHash {
    let mut hash = leaf_hash(&proven.entry.key, &proven.entry.value);
    let mut index = proven.index;
    let mut width = total;
    let mut siblings = proven.path.iter();

    while width > 1 {
        let sibling = if index.is_multiple_of(2) {
            index + 1
        } else {
            index - 1
        };
        if sibling < width {
            let Some(hash_of_sibling) = siblings.next() else {
                // The path is shorter than the tree's shape requires. A proof
                // that cannot be walked is not a proof.
                return ContentHash::ZERO;
            };
            hash = if index.is_multiple_of(2) {
                node_hash(&hash, hash_of_sibling)
            } else {
                node_hash(hash_of_sibling, &hash)
            };
        }
        index /= 2;
        width = width.div_ceil(2);
    }

    // Trailing siblings mean the path claims a taller tree than `total`
    // describes, which is another way of saying the two disagree.
    match siblings.next() {
        Some(_) => ContentHash::ZERO,
        None => hash,
    }
}

/// Check that a range proof really proves completeness.
///
/// The order of these checks is the argument. Verifying the paths first means
/// everything after it is reasoning about entries the root vouches for; doing
/// adjacency first would be reasoning about numbers the server made up.
pub fn verify_range(
    proof: &RangeProof,
    trusted_root: &ContentHash,
) -> Result<(), CompletenessError> {
    // 1. Every entry in the proof, matched or flanking, must tie to the root.
    let all: Vec<&ProvenEntry> = proof
        .left
        .iter()
        .chain(proof.matched.iter())
        .chain(proof.right.iter())
        .collect();

    for proven in &all {
        if reconstruct(proven, proof.total) != *trusted_root {
            return Err(CompletenessError::BadPath {
                key: proven.entry.key.clone(),
            });
        }
    }

    // 2. Sorted, and consecutive. A gap is where an omitted key would hide.
    for pair in all.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if a.entry.key >= b.entry.key {
            return Err(CompletenessError::OutOfOrder {
                earlier: a.entry.key.clone(),
                later: b.entry.key.clone(),
            });
        }
        if b.index != a.index + 1 {
            return Err(CompletenessError::Gap {
                expected: a.index + 1,
            });
        }
    }

    // 3. An absence claim is refused if the proof itself holds the key.
    //
    //    Checked before the range rules rather than after, because those rules
    //    will happily accept it: `[k, k)` is empty, and a neighbour equal to `k`
    //    is outside a half-open range ending at `k`. Correct for a range,
    //    catastrophic for an absence claim.
    if let Some(key) = &proof.absent {
        let carried = all.iter().any(|p| &p.entry.key == key);
        if carried {
            return Err(CompletenessError::ClaimedAbsentButPresent { key: key.clone() });
        }
    }

    // 4. Everything claimed as a match is actually in the range.
    for proven in &proof.matched {
        let key = proven.entry.key.as_str();
        if key < proof.start.as_str() || key >= proof.end.as_str() {
            return Err(CompletenessError::OutsideRange {
                key: proven.entry.key.clone(),
            });
        }
    }

    // 5. The boundaries. Either a neighbour outside the range pins the edge, or
    //    the edge is the edge of the map.
    match &proof.left {
        Some(left) if left.entry.key.as_str() >= proof.start.as_str() => {
            return Err(CompletenessError::LeftNeighbourInside {
                key: left.entry.key.clone(),
            })
        }
        Some(_) => {}
        None => {
            let first = proof.matched.first().map(|p| p.index).unwrap_or(0);
            if first != 0 {
                return Err(CompletenessError::UnboundedBelow { index: first });
            }
        }
    }

    match &proof.right {
        Some(right) if right.entry.key.as_str() < proof.end.as_str() => {
            return Err(CompletenessError::RightNeighbourInside {
                key: right.entry.key.clone(),
            })
        }
        Some(_) => {}
        None => {
            // The last index the proof accounts for. With no matches and no
            // left neighbour the map is empty, which `total` confirms.
            let last = proof
                .matched
                .last()
                .map(|p| p.index)
                .or_else(|| proof.left.as_ref().map(|p| p.index));
            if let Some(index) = last {
                if index + 1 != proof.total {
                    return Err(CompletenessError::UnboundedAbove {
                        index,
                        total: proof.total,
                    });
                }
            }
        }
    }

    Ok(())
}

/// What a verified range proof lets a client conclude, in one sentence.
///
/// Returned as text for the same reason [`crate::inclusion::establishes`] is:
/// the sentence belongs next to the code that makes it true, so that changing
/// what is proved and forgetting to change what is claimed requires editing
/// this file.
pub fn establishes() -> &'static str {
    "Every key in the range is present in this result, and no key has been \
     omitted, as of the map root this was checked against. It does not \
     establish that the root describes an honest fold over the log — that is \
     what the hash chain and an external anchor are for."
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(keys: &[&str]) -> OrderedMap {
        OrderedMap::build(
            keys.iter()
                .map(|k| MapEntry {
                    key: (*k).to_string(),
                    value: ContentHash::of(k.as_bytes()),
                })
                .collect(),
        )
    }

    #[test]
    fn a_range_proof_verifies_against_the_root() {
        let m = map(&["a", "b", "c", "d", "e"]);
        let proof = m.prove_range("b", "d");

        assert_eq!(proof.matched.len(), 2, "b and c are in [b, d)");
        verify_range(&proof, &m.root()).expect("verifies");
    }

    #[test]
    fn a_proof_from_another_map_does_not_verify() {
        let m = map(&["a", "b", "c"]);
        let other = map(&["a", "b", "x"]);

        assert!(verify_range(&m.prove_range("a", "c"), &other.root()).is_err());
    }

    /// The point of the whole structure.
    #[test]
    fn dropping_a_matched_entry_is_caught() {
        let m = map(&["a", "b", "c", "d"]);
        let mut proof = m.prove_range("a", "d");
        assert_eq!(proof.matched.len(), 3);

        // The server answers with two of the three and hopes nobody notices.
        proof.matched.remove(1);

        let err = verify_range(&proof, &m.root()).expect_err("an omission must be caught");
        assert!(
            matches!(err, CompletenessError::Gap { .. }),
            "expected a gap, got {err}"
        );
    }

    #[test]
    fn an_entry_the_map_does_not_hold_is_caught() {
        let m = map(&["a", "b", "c"]);
        let mut proof = m.prove_range("a", "c");
        proof.matched[0].entry.value = ContentHash::of(b"tampered");

        assert!(matches!(
            verify_range(&proof, &m.root()),
            Err(CompletenessError::BadPath { .. })
        ));
    }

    #[test]
    fn a_match_outside_the_range_is_caught() {
        let m = map(&["a", "b", "c", "d"]);
        let mut proof = m.prove_range("b", "c");
        // Smuggle in a real, provable entry that does not belong to the range.
        proof.matched.push(m.at(3).expect("d exists"));

        assert!(matches!(
            verify_range(&proof, &m.root()),
            Err(CompletenessError::OutsideRange { .. } | CompletenessError::Gap { .. })
        ));
    }

    /// Without the flank, a truncated result would verify.
    #[test]
    fn a_result_truncated_at_the_end_is_caught_by_the_right_neighbour() {
        let m = map(&["a", "b", "c", "d", "e"]);
        let mut proof = m.prove_range("a", "e");

        // Drop the last match and the neighbour that would expose it.
        proof.matched.pop();
        proof.right = None;

        let err = verify_range(&proof, &m.root()).expect_err("truncation must be caught");
        assert!(
            matches!(err, CompletenessError::UnboundedAbove { .. }),
            "expected an unbounded range, got {err}"
        );
    }

    #[test]
    fn a_range_covering_the_whole_map_needs_no_neighbours() {
        let m = map(&["a", "b", "c"]);
        let proof = m.prove_range("a", "z");

        assert!(proof.left.is_none() && proof.right.is_none());
        verify_range(&proof, &m.root()).expect("the map's own edges are the boundary");
    }

    #[test]
    fn absence_is_provable() {
        let m = map(&["a", "c", "e"]);
        let proof = m.prove_absent("d");

        assert!(proof.matched.is_empty(), "d is not in the map");
        verify_range(&proof, &m.root()).expect("verifies");
        assert_eq!(proof.left.as_ref().map(|p| p.entry.key.as_str()), Some("c"));
        assert_eq!(
            proof.right.as_ref().map(|p| p.entry.key.as_str()),
            Some("e")
        );
    }

    /// A key that exists cannot be proved absent.
    ///
    /// This is the bug the engine-level suite found. `prove_absent(k)` builds
    /// the empty range `[k, k)`, and a neighbour equal to `k` is legitimately
    /// *outside* a half-open range ending at `k` -- so every range rule passed
    /// and the verifier accepted a proof of absence that carried the key in it.
    /// The absence claim had to become explicit before it could be checked.
    #[test]
    fn a_key_the_map_holds_cannot_be_proved_absent() {
        let m = map(&["a", "b", "c"]);
        let proof = m.prove_absent("b");

        let err =
            verify_range(&proof, &m.root()).expect_err("a key the map holds was proved absent");
        assert!(
            matches!(err, CompletenessError::ClaimedAbsentButPresent { .. }),
            "expected the absence claim to be refused, got {err}"
        );
    }

    /// And the control: absence of a key that really is absent still proves.
    #[test]
    fn the_absence_check_does_not_refuse_a_genuine_absence() {
        let m = map(&["a", "c", "e"]);
        verify_range(&m.prove_absent("b"), &m.root()).expect("b really is absent");
        verify_range(&m.prove_absent("z"), &m.root()).expect("z is past the end");
        verify_range(&m.prove_absent("0"), &m.root()).expect("0 is before the start");
    }

    /// A server claiming a key is absent when it holds it.
    #[test]
    fn a_false_absence_cannot_be_proved() {
        let m = map(&["a", "c", "e"]);
        let mut proof = m.prove_absent("c");

        // `c` exists, so an honest proof of its absence is impossible: the
        // flanking entries would have to be adjacent, and `c` sits between
        // them. The server's best attempt is to point at the pair around it.
        proof.left = m.at(0); // a
        proof.right = m.at(2); // e

        let err = verify_range(&proof, &m.root()).expect_err("a false absence must be caught");
        assert!(
            matches!(err, CompletenessError::Gap { .. }),
            "expected a gap where c sits, got {err}"
        );
    }

    #[test]
    fn an_odd_number_of_leaves_still_proves() {
        for n in 1..=17usize {
            let keys: Vec<String> = (0..n).map(|i| format!("k{i:03}")).collect();
            let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
            let m = map(&refs);

            let proof = m.prove_range("k000", "k999");
            verify_range(&proof, &m.root())
                .unwrap_or_else(|e| panic!("a map of {n} leaves failed to prove: {e}"));
            assert_eq!(proof.matched.len(), n);
        }
    }

    /// An odd leaf is carried up, never duplicated.
    ///
    /// Duplicating it to pad a level is CVE-2012-2459: `[a, b, c]` padded to
    /// `[a, b, c, c]` gives a four-leaf tree with the same root as the
    /// three-leaf one, so a server could prove a result set it never held.
    ///
    /// The obvious test -- build both key sets and compare roots -- cannot be
    /// written against this API, because `build` deduplicates by key and the two
    /// inputs collapse to the same map. It passes for a reason that has nothing
    /// to do with the property. So the duplicated tree is computed by hand here
    /// and the real root is checked to differ from it.
    #[test]
    fn an_odd_leaf_is_carried_up_rather_than_duplicated() {
        let m = map(&["a", "b", "c"]);

        let leaves: Vec<ContentHash> = ["a", "b", "c"]
            .iter()
            .map(|k| leaf_hash(k, &ContentHash::of(k.as_bytes())))
            .collect();

        // What duplication would have produced: [H(a,b), H(c,c)] -> root.
        let duplicated = node_hash(
            &node_hash(&leaves[0], &leaves[1]),
            &node_hash(&leaves[2], &leaves[2]),
        );

        // What carrying up produces: [H(a,b), c] -> root.
        let carried = node_hash(&node_hash(&leaves[0], &leaves[1]), &leaves[2]);

        assert_eq!(m.root(), carried, "the odd leaf was not carried up");
        assert_ne!(
            m.root(),
            duplicated,
            "the root matches a duplicated-leaf tree, which is the collision              that lets a server prove a result set it never held"
        );
    }

    /// A leaf must not be presentable as an internal node.
    #[test]
    fn leaves_and_internal_nodes_live_in_different_preimage_spaces() {
        let left = ContentHash::of(b"left");
        let right = ContentHash::of(b"right");
        let internal = node_hash(&left, &right);

        // A leaf whose key and value are exactly the two child hashes.
        let forged = leaf_hash(String::from_utf8_lossy(&left.0).as_ref(), &right);
        assert_ne!(internal, forged);
    }

    #[test]
    fn an_empty_map_has_a_recognisable_root() {
        let m = OrderedMap::build(Vec::new());
        assert!(m.is_empty());
        assert_eq!(m.root(), ContentHash::ZERO);
        verify_range(&m.prove_range("a", "z"), &m.root()).expect("nothing to omit");
    }

    #[test]
    fn the_key_length_is_bound_into_the_leaf() {
        // ("ab", v) and ("a", v) must differ by more than the boundary, or a
        // server could move the split and call it a different row.
        let v = ContentHash::of(b"v");
        assert_ne!(leaf_hash("ab", &v), leaf_hash("a", &v));
        assert_ne!(
            map(&["ab", "c"]).root(),
            map(&["a", "bc"]).root(),
            "two key sets differing only in where the boundary falls share a root"
        );
    }
}
