//! Corruption and byte-fuzzing: malformed bytes must produce a typed error, never a panic, an
//! out-of-bounds access, or UB.
//!
//! Two independent guards are exercised separately. With [`VerifyPolicy::Always`] the hash catches any
//! mutation before decode; with [`VerifyPolicy::Never`] the *structural verifier alone* has to hold the
//! line, which is the interesting case — a store that content-addresses correctly can still hand back
//! bytes written by a broken or hostile writer.
//!
//! Fuzzing supplements the runtime checks in `codec`; it does not replace them.

mod support;

use std::sync::Arc;

use cbe_tree::codec::NodeView;
use cbe_tree::format::{Format, NodeKind};
use cbe_tree::store::MemStore;
use cbe_tree::tree::VerifyPolicy;
use cbe_tree::{BeTree, BlockId, DecodeError, Mutation, TreeError, VersionStamp};
use bytes::Bytes;
use support::{self as harness, KeyShape, Rng};

fn fmt() -> Format {
    Format::tiny()
}

/// Fuzz iteration count, scaled by `BE_TREE_FUZZ` so CI stays fast while a soak run can go deep.
/// `BE_TREE_FUZZ=50 cargo test --release --test corruption` multiplies every sweep by 50.
fn rounds(base: usize) -> usize {
    let scale: usize = std::env::var("BE_TREE_FUZZ")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    base * scale.max(1)
}

/// Every NODE reachable from `root`. Mutating an unreachable object (an intermediate root from an
/// earlier commit) proves nothing: no read would ever fetch it.
async fn reachable_nodes(t: &BeTree<MemStore>, root: BlockId) -> Vec<BlockId> {
    let mut seen: Vec<BlockId> = Vec::new();
    let mut frontier = vec![root];
    while let Some(id) = frontier.pop() {
        if seen.contains(&id) {
            continue;
        }
        seen.push(id);
        for (kind, child) in t.references(id).await.unwrap() {
            if kind == cbe_tree::ObjectKind::Node {
                frontier.push(child);
            }
        }
    }
    seen
}

/// A real multi-level tree, plus every object id it stored.
async fn corpus() -> (Arc<MemStore>, BeTree<MemStore>, BlockId, Vec<Bytes>) {
    let store = Arc::new(MemStore::new());
    let t = BeTree::with_format(store.clone(), fmt());
    let keys = KeyShape::EmbeddedZeroes.keys(3_000, 9);
    let (root, _m) = harness::build(&t, &keys, 32, 20).await.unwrap();
    // Add an out-of-line value so value envelopes are in the corpus too.
    let root = t
        .apply(
            root,
            VersionStamp::from_counter(9_999),
            vec![Mutation::upsert(
                Bytes::from_static(b"big"),
                Bytes::from(vec![b'B'; t.format().inline_value_bytes() * 4]),
            )],
        )
        .await
        .unwrap();
    let ids = store.ids();
    let raw: Vec<Bytes> = ids.iter().filter_map(|id| store.raw(*id)).collect();
    (store, t, root, raw)
}

/// Every legal error class, and nothing else. A panic or an `Ok` from mutated bytes is a failure.
fn assert_typed(e: TreeError, what: &str) {
    match e {
        TreeError::Decode { .. }
        | TreeError::HashMismatch { .. }
        | TreeError::VersionDomainMismatch { .. }
        | TreeError::Capacity(_)
        | TreeError::ResourceLimit { .. }
        | TreeError::Store(_) => {}
    }
    let _ = what;
}

#[tokio::test]
#[cfg_attr(miri, ignore = "native corruption sweep")]
async fn mutating_any_reachable_node_yields_a_typed_error_through_the_read_api() {
    let (store, t, root, _raw) = corpus().await;
    let ids = reachable_nodes(&t, root).await;
    assert!(
        ids.len() > 20,
        "the fixture must have a real tree, got {} nodes",
        ids.len()
    );
    let mut rng = Rng::new(0xFEED);

    for round in 0..rounds(400) {
        let id = ids[rng.below(ids.len())];
        let original = store.raw(id).expect("stored");
        let mut mutated = original.to_vec();
        let at = rng.below(mutated.len());
        mutated[at] ^= 1 << rng.below(8);
        store.corrupt(id, Bytes::from(mutated));

        // Fresh handle each round: the decoded-node cache must not serve a stale good view. A full scan
        // visits every reachable node, so the mutated one is always fetched.
        let fresh = BeTree::with_format(store.clone(), fmt());
        match fresh.scan_range(root, None, None).await {
            Ok(_) => panic!("round {round}: mutated bytes at {id} were accepted by a full scan"),
            Err(e) => assert_typed(e, "scan"),
        }
        store.corrupt(id, original);
    }
}

/// With verification off, the structural verifier alone must hold. Some mutations are genuinely
/// undetectable this way (a flipped bit inside a value payload changes the value but breaks no
/// invariant), so this asserts *no panic and a typed error when rejected* rather than always-rejected.
#[tokio::test]
#[cfg_attr(miri, ignore = "native structural mutation sweep")]
async fn with_verification_off_the_structural_verifier_never_panics() {
    let (store, t, root, _raw) = corpus().await;
    let ids = reachable_nodes(&t, root).await;
    let mut rng = Rng::new(0xBEEF);
    let keys: Vec<Bytes> = KeyShape::EmbeddedZeroes.keys(3_000, 9);
    let probe: Vec<&[u8]> = keys.iter().take(32).map(|k| k.as_ref()).collect();

    // Counted against the FULL SCAN, which visits every reachable node. A point read only fetches the
    // nodes on its own 32 key paths, so most mutations would never be fetched at all and an
    // "accepted" count from it would measure coverage, not the verifier.
    let mut scan_rejected = 0usize;
    let mut scan_accepted = 0usize;
    for _ in 0..rounds(600) {
        let id = ids[rng.below(ids.len())];
        let original = store.raw(id).expect("stored");
        let mut mutated = original.to_vec();
        // Mutate a whole byte, sometimes several, to reach header and column bytes.
        for _ in 0..1 + rng.below(3) {
            let at = rng.below(mutated.len());
            mutated[at] = (rng.next_u64() & 0xff) as u8;
        }
        store.corrupt(id, Bytes::from(mutated));

        let fresh = BeTree::with_format(store.clone(), fmt()).with_verify(VerifyPolicy::Never);
        // A point read may legitimately not touch the mutated node; it must still never panic.
        if let Err(e) = fresh.get_many(root, &probe).await {
            assert_typed(e, "unverified point read");
        }
        match fresh.scan_range(root, None, None).await {
            Ok(_) => scan_accepted += 1,
            Err(e) => {
                scan_rejected += 1;
                assert_typed(e, "unverified scan");
            }
        }
        store.corrupt(id, original);
    }
    // The surviving mutations are exactly the ones no invariant can see: bytes inside a live value
    // payload or a live order key, which are opaque data. Those change the ANSWER, which is what
    // `VerifyPolicy::Always` exists to catch — hence the default.
    let total = scan_rejected + scan_accepted;
    let rate = scan_rejected as f64 / total as f64;
    println!(
        "verify=Never, full scan: {scan_rejected}/{total} rejected ({:.1}%); \
         {scan_accepted} landed in live payload/order-key bytes, which no structural invariant can see",
        rate * 100.0
    );
    assert!(
        rate > 0.80,
        "the structural verifier caught only {:.1}% of mutations under a full scan; \
         padding, column, and derived-field checks should catch far more",
        rate * 100.0
    );
}

/// Arbitrary bytes of the right length: never a panic, always a typed error or a valid view whose every
/// accessor stays in bounds.
#[test]
#[cfg_attr(miri, ignore = "native arbitrary-byte sweep")]
fn arbitrary_byte_strings_are_never_accepted_unsafely() {
    let f = Arc::new(fmt());
    let mut rng = Rng::new(0xD00D);
    for round in 0..rounds(2_000) {
        let bytes = match round % 4 {
            0 => Bytes::from(rng.bytes(f.node_bytes())),
            1 => Bytes::from(vec![0u8; f.node_bytes()]),
            2 => Bytes::from(vec![0xffu8; f.node_bytes()]),
            _ => {
                // A *plausible* node: correct magic and format id, random everything else. This is the
                // shape that actually reaches the deep validators.
                let mut b = vec![0u8; f.node_bytes()];
                b[..8].copy_from_slice(&cbe_tree::format::NODE_MAGIC);
                b[8..24].copy_from_slice(f.schema_id());
                b[24] = (rng.next_u64() & 1) as u8;
                b[48..64].copy_from_slice(f.version_domain());
                let n = 8 + rng.below(64);
                for _ in 0..n {
                    let at = 64 + rng.below(f.node_bytes() - 64);
                    b[at] = (rng.next_u64() & 0xff) as u8;
                }
                Bytes::from(b)
            }
        };
        match NodeView::decode(&f, None, bytes) {
            Ok(v) => {
                // Exercise every accessor: if validation said yes, nothing may read out of bounds.
                for i in 0..v.entry_count() {
                    let _ = v.entry(i);
                    let _ = v.entry_key(i);
                    let _ = v.entry_span(i);
                }
                for i in 0..v.pivot_count() {
                    let _ = v.pivot(i);
                }
                for i in 0..v.child_count() {
                    let _ = v.child(i);
                }
                let _ = v.references();
                let _ = v.find(b"probe");
                if v.kind() == NodeKind::Internal {
                    let _ = v.child_of(b"probe");
                }
            }
            Err(TreeError::Decode { .. }) | Err(TreeError::VersionDomainMismatch { .. }) => {}
            Err(other) => panic!("round {round}: unexpected error class {other:?}"),
        }
    }
}

/// Truncation and extension at every length, including zero and one byte off.
#[test]
#[cfg_attr(miri, ignore = "native exhaustive length sweep")]
fn every_wrong_length_is_rejected_on_length_alone() {
    let f = Arc::new(fmt());
    let good = cbe_tree::codec::encode_leaf(
        &f,
        &[cbe_tree::codec::Entry::inline(
            Bytes::from_static(b"k"),
            VersionStamp::from_counter(1).order_key,
            Bytes::from_static(b"v"),
        )],
    )
    .unwrap();
    for len in [0usize, 1, 63, 64, f.node_bytes() - 1] {
        assert!(matches!(
            NodeView::decode(&f, None, good.slice(..len)),
            Err(TreeError::Decode {
                reason: DecodeError::Length { .. },
                ..
            })
        ));
    }
    let mut longer = good.to_vec();
    longer.extend_from_slice(&[0u8; 64]);
    assert!(NodeView::decode(&f, None, Bytes::from(longer)).is_err());
}

/// A value object substituted for another valid one must be rejected by the length its referencing node
/// authenticated — content addressing plus that check closes the substitution hole.
#[tokio::test]
async fn a_substituted_value_object_is_rejected() {
    let store = Arc::new(MemStore::new());
    let t = BeTree::with_format(store.clone(), fmt());
    let empty = t.empty_root().await.unwrap();
    let inline = t.format().inline_value_bytes();
    let root = t
        .apply(
            empty,
            VersionStamp::from_counter(1),
            vec![
                Mutation::upsert(
                    Bytes::from_static(b"a"),
                    Bytes::from(vec![b'a'; inline * 2]),
                ),
                Mutation::upsert(
                    Bytes::from_static(b"b"),
                    Bytes::from(vec![b'b'; inline * 3]),
                ),
            ],
        )
        .await
        .unwrap();

    // Find the two value objects by size and swap their bytes.
    let mut value_objects: Vec<(BlockId, Bytes)> = store
        .ids()
        .into_iter()
        .filter_map(|id| store.raw(id).map(|b| (id, b)))
        .filter(|(_, b)| b.starts_with(&cbe_tree::value::VALUE_MAGIC))
        .collect();
    assert_eq!(
        value_objects.len(),
        2,
        "the fixture must have two value objects"
    );
    value_objects.sort_by_key(|(_, b)| b.len());
    let (small_id, _small) = value_objects[0].clone();
    let (large_id, large) = value_objects[1].clone();

    store.corrupt(small_id, large);
    let fresh = BeTree::with_format(store.clone(), fmt()).with_verify(VerifyPolicy::Never);
    let e = fresh.get(root, b"a").await.unwrap_err();
    assert!(
        matches!(
            e,
            TreeError::Decode {
                reason: DecodeError::ValueLength { .. },
                ..
            }
        ),
        "got {e:?}"
    );
    // And with verification on, the hash catches it first.
    store.corrupt(small_id, store.raw(large_id).unwrap());
    let verified = BeTree::with_format(store, fmt());
    assert!(matches!(
        verified.get(root, b"a").await.unwrap_err(),
        TreeError::HashMismatch { .. }
    ));
}

/// A node byte string offered where a value object is expected, and vice versa: the two object-type
/// domain tags make the byte languages disjoint.
#[tokio::test]
async fn the_two_object_domains_cannot_be_confused() {
    let store = Arc::new(MemStore::new());
    let t = BeTree::with_format(store.clone(), fmt());
    let empty = t.empty_root().await.unwrap();
    let inline = t.format().inline_value_bytes();
    let root = t
        .apply(
            empty,
            VersionStamp::from_counter(1),
            vec![Mutation::upsert(
                Bytes::from_static(b"a"),
                Bytes::from(vec![b'a'; inline * 2]),
            )],
        )
        .await
        .unwrap();

    let value_id = store
        .ids()
        .into_iter()
        .find(|id| {
            store
                .raw(*id)
                .is_some_and(|b| b.starts_with(&cbe_tree::value::VALUE_MAGIC))
        })
        .expect("a value object");
    // Replace the value object with a perfectly valid NODE.
    store.corrupt(value_id, store.raw(root).unwrap());
    let fresh = BeTree::with_format(store.clone(), fmt()).with_verify(VerifyPolicy::Never);
    assert!(matches!(
        fresh.get(root, b"a").await.unwrap_err(),
        TreeError::Decode {
            reason: DecodeError::Magic
                | DecodeError::ValueEnvelope(_)
                | DecodeError::ValueLength { .. },
            ..
        }
    ));
}

/// A cycle is the sharpest form of "no unverified header value controls recursion depth": a node that
/// points at itself would make any naive walk run forever. The `tree_level` edge check makes it
/// impossible — a child must sit exactly one level below its parent, and a self-edge cannot.
#[tokio::test]
#[cfg_attr(miri, ignore = "native recursive-DAG fixture")]
async fn a_self_referential_node_cannot_drive_unbounded_recursion() {
    let store = Arc::new(MemStore::new());
    let t = BeTree::with_format(store.clone(), fmt());
    let keys = KeyShape::Ascending.keys(3_000, 1);
    let (root, _m) = harness::build(&t, &keys, 64, 8).await.unwrap();
    let rv = t.view(root).await.unwrap();

    // Rewrite the root so its first child points at the root itself.
    let mut bytes = rv.bytes().to_vec();
    let s = t.format().internal_sections();
    let cycle_id = {
        // The id depends on the bytes, which depend on the id — so aim the edge at the ORIGINAL root and
        // store the result under a fresh address. Either way a walk that ignored levels would loop.
        bytes[s.child_id(0)].copy_from_slice(&root.0);
        BlockId::of(&bytes)
    };
    store.corrupt(cycle_id, Bytes::from(bytes));

    let fresh = BeTree::with_format(store.clone(), fmt());
    for e in [
        fresh.get(cycle_id, &keys[0]).await.unwrap_err(),
        fresh.scan_range(cycle_id, None, None).await.unwrap_err(),
    ] {
        assert!(
            matches!(
                e,
                TreeError::Decode {
                    reason: DecodeError::ChildLevel { .. },
                    ..
                }
            ),
            "got {e:?}"
        );
    }
}

/// Rewiring a child pointer to a node at the wrong level must be rejected, because local decode cannot
/// prove an edge invariant without the child.
#[tokio::test]
#[cfg_attr(miri, ignore = "native large edge-level fixture")]
async fn a_child_edge_to_the_wrong_level_is_rejected() {
    let store = Arc::new(MemStore::new());
    let t = BeTree::with_format(store.clone(), fmt());
    let keys = KeyShape::Ascending.keys(5_000, 1);
    let (root, _m) = harness::build(&t, &keys, 64, 8).await.unwrap();
    let rv = t.view(root).await.unwrap();
    assert!(rv.tree_level() >= 2, "need a root at least two levels up");

    // Point the root's first child at a grandchild (level - 2 instead of level - 1).
    let child = t.view(rv.child(0)).await.unwrap();
    let grandchild = child.child(0);
    let mut bytes = rv.bytes().to_vec();
    let s = t.format().internal_sections();
    bytes[s.child_id(0)].copy_from_slice(&grandchild.0);
    let bad_root = BlockId::of(&bytes);
    store.corrupt(bad_root, Bytes::from(bytes));

    let fresh = BeTree::with_format(store, fmt());
    let e = fresh.get(bad_root, &keys[0]).await.unwrap_err();
    assert!(
        matches!(
            e,
            TreeError::Decode {
                reason: DecodeError::ChildLevel { .. },
                ..
            }
        ),
        "got {e:?}"
    );
}
