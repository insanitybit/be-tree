//! The logical contract: reads, snapshots, tombstones, the total order, batch normalization, and the
//! explicit capacity/resource/verification error classes.

mod support;

use std::sync::Arc;

use cbe_tree::format::Format;
use cbe_tree::store::MemStore;
use cbe_tree::tree::{CacheConfig, VerifyPolicy};
use cbe_tree::{BeTree, BlockId, CapacityError, Mutation, TreeError, VersionStamp, WorkBudget};
use bytes::Bytes;
use support as harness;

fn stamp(n: u64) -> VersionStamp {
    VersionStamp::from_counter(n)
}

/// A tiny-format tree, so splits, multiway propagation, and root growth all happen at test scale.
async fn tree() -> (Arc<MemStore>, BeTree<MemStore>, BlockId) {
    let store = Arc::new(MemStore::new());
    let t = BeTree::with_format(store.clone(), Format::tiny()).record_metrics();
    let empty = t.empty_root().await.expect("empty root");
    (store, t, empty)
}

fn up(k: &str, v: &str) -> Mutation {
    Mutation::upsert(Bytes::from(k.to_owned()), Bytes::from(v.to_owned()))
}

#[tokio::test]
#[cfg_attr(miri, ignore = "native 500-key readback fixture")]
async fn writes_read_back_and_the_root_changes_with_content() {
    let (_s, t, empty) = tree().await;
    let muts: Vec<Mutation> = (0..500u32)
        .map(|i| up(&format!("k{i:04}"), &format!("v{i}")))
        .collect();
    let root = t.apply(empty, stamp(1), muts).await.expect("write");
    for i in 0..500u32 {
        assert_eq!(
            t.get(root, format!("k{i:04}").as_bytes())
                .await
                .expect("read"),
            Some(Bytes::from(format!("v{i}"))),
            "every written key reads back"
        );
    }
    assert_eq!(t.get(root, b"absent").await.expect("read"), None);
    harness::check_balanced(&t, root).await.expect("balanced");
}

#[tokio::test]
async fn an_empty_batch_performs_no_store_operation_and_returns_its_input_root() {
    let (store, t, empty) = tree().await;
    let before = store.len();
    let root = t.apply(empty, stamp(1), Vec::new()).await.expect("empty");
    assert_eq!(root, empty);
    assert_eq!(store.len(), before, "no object was written");
}

#[tokio::test]
#[cfg_attr(miri, ignore = "native historical-root rewrite fixture")]
async fn a_write_rewrites_only_new_nodes_so_old_roots_still_read() {
    let (store, t, empty) = tree().await;
    let v1 = t
        .apply(empty, stamp(1), vec![up("key", "first")])
        .await
        .unwrap();
    let after_v1 = store.len();
    let v2 = t
        .apply(v1, stamp(2), vec![up("key", "second")])
        .await
        .unwrap();
    assert_ne!(v1, v2, "a write produces a NEW root");
    assert_eq!(t.get(v1, b"key").await.unwrap(), Some(Bytes::from("first")));
    assert_eq!(
        t.get(v2, b"key").await.unwrap(),
        Some(Bytes::from("second"))
    );
    assert!(
        store.len() > after_v1,
        "COW adds objects rather than mutating"
    );
}

#[tokio::test]
async fn the_greater_order_key_wins_regardless_of_write_order() {
    let (_s, t, empty) = tree().await;
    let root = t
        .apply(empty, stamp(10), vec![up("k", "new")])
        .await
        .unwrap();
    let root = t.apply(root, stamp(5), vec![up("k", "old")]).await.unwrap();
    assert_eq!(
        t.get(root, b"k").await.unwrap(),
        Some(Bytes::from("new")),
        "the greater order key wins even though the lesser was written LAST"
    );
}

#[tokio::test]
async fn a_tombstone_hides_a_key_and_a_later_upsert_revives_it() {
    let (_s, t, empty) = tree().await;
    let root = t.apply(empty, stamp(1), vec![up("k", "v")]).await.unwrap();
    let root = t
        .apply(root, stamp(2), vec![Mutation::tombstone(Bytes::from("k"))])
        .await
        .unwrap();
    assert_eq!(t.get(root, b"k").await.unwrap(), None);
    let root = t
        .apply(root, stamp(3), vec![up("k", "again")])
        .await
        .unwrap();
    assert_eq!(t.get(root, b"k").await.unwrap(), Some(Bytes::from("again")));
}

/// Repeated keys in one batch collapse to the LAST mutation in program order — caller intent, applied
/// before any comparison, so upsert-then-tombstone in one batch deletes.
#[tokio::test]
async fn repeated_batch_keys_collapse_in_program_order() {
    let (_s, t, empty) = tree().await;
    let root = t
        .apply(
            empty,
            stamp(1),
            vec![
                up("k", "first"),
                Mutation::tombstone(Bytes::from("k")),
                up("k", "last"),
            ],
        )
        .await
        .unwrap();
    assert_eq!(t.get(root, b"k").await.unwrap(), Some(Bytes::from("last")));

    let root = t
        .apply(
            root,
            stamp(2),
            vec![up("k", "resurrect"), Mutation::tombstone(Bytes::from("k"))],
        )
        .await
        .unwrap();
    assert_eq!(t.get(root, b"k").await.unwrap(), None);
}

/// A reused order key is a producer fault, but the canonical operation comparison still makes the tree
/// deterministic — and delete wins, on every replica, in either arrival order.
#[tokio::test]
async fn a_reused_order_key_resolves_identically_on_every_replica() {
    let (_s, a, empty_a) = tree().await;
    let (_s2, b, empty_b) = tree().await;
    let s = stamp(7);

    let ra = a.apply(empty_a, s, vec![up("k", "value")]).await.unwrap();
    let ra = a
        .apply(ra, s, vec![Mutation::tombstone(Bytes::from("k"))])
        .await
        .unwrap();

    let rb = b
        .apply(empty_b, s, vec![Mutation::tombstone(Bytes::from("k"))])
        .await
        .unwrap();
    let rb = b.apply(rb, s, vec![up("k", "value")]).await.unwrap();

    assert_eq!(a.get(ra, b"k").await.unwrap(), None, "delete wins");
    assert_eq!(b.get(rb, b"k").await.unwrap(), None, "on both replicas");
}

/// Two inline upserts sharing an order key resolve by their value bytes lexicographically — no digest
/// tie-break, no value fetch.
#[tokio::test]
async fn a_reused_order_key_between_two_upserts_resolves_by_value_bytes() {
    let (_s, t, empty) = tree().await;
    let s = stamp(7);
    let r1 = t.apply(empty, s, vec![up("k", "aaa")]).await.unwrap();
    let r1 = t.apply(r1, s, vec![up("k", "bbb")]).await.unwrap();
    let r2 = t.apply(empty, s, vec![up("k", "bbb")]).await.unwrap();
    let r2 = t.apply(r2, s, vec![up("k", "aaa")]).await.unwrap();
    assert_eq!(t.get(r1, b"k").await.unwrap(), Some(Bytes::from("bbb")));
    assert_eq!(t.get(r2, b"k").await.unwrap(), Some(Bytes::from("bbb")));
}

/// Replay is idempotent **at the key level**: re-injecting byte-identical operations cannot change any
/// resolved value. It is deliberately NOT a claim about the root hash — a replayed message re-enters the
/// root buffer even when an identical copy already sits in a leaf, and every apply that changes tree
/// bytes writes at least a root object. Confluence is explicitly out of scope.
#[tokio::test]
#[cfg_attr(miri, ignore = "native 200-key replay fixture")]
async fn byte_identical_replay_is_idempotent_at_the_key_level() {
    let (_s, t, empty) = tree().await;
    let muts: Vec<Mutation> = (0..200u32).map(|i| up(&format!("k{i:04}"), "v")).collect();
    let once = t.apply(empty, stamp(1), muts.clone()).await.unwrap();
    let twice = t.apply(once, stamp(1), muts.clone()).await.unwrap();
    let thrice = t.apply(twice, stamp(1), muts).await.unwrap();

    for root in [once, twice, thrice] {
        let got = t.scan_range(root, None, None).await.unwrap();
        assert_eq!(got.len(), 200);
        assert!(got.iter().all(|(_, v)| v.as_ref() == b"v"));
        harness::check_balanced(&t, root).await.unwrap();
    }
}

/// When the replayed messages land in a node that already holds them identically, the merge collapses
/// them and the bytes really are unchanged — so replay produces no new object at all.
#[tokio::test]
async fn replay_into_a_single_leaf_produces_no_new_bytes() {
    let (store, t, empty) = tree().await;
    let muts: Vec<Mutation> = (0..4u32).map(|i| up(&format!("k{i}"), "v")).collect();
    let once = t.apply(empty, stamp(1), muts.clone()).await.unwrap();
    let objects = store.len();
    let twice = t.apply(once, stamp(1), muts).await.unwrap();
    assert_eq!(
        once, twice,
        "an idempotent merge re-encodes to the same bytes"
    );
    assert_eq!(
        store.len(),
        objects,
        "content addressing makes the write a no-op"
    );
}

#[tokio::test]
#[cfg_attr(miri, ignore = "native replica construction fixture")]
async fn independently_built_trees_agree_on_the_root() {
    let build = || async {
        let (_s, t, empty) = tree().await;
        let mut root = empty;
        for chunk in (0..400u32).collect::<Vec<_>>().chunks(16) {
            let muts: Vec<Mutation> = chunk
                .iter()
                .map(|i| up(&format!("k{i:04}"), &format!("v{i}")))
                .collect();
            root = t
                .apply(root, stamp(1 + u64::from(chunk[0])), muts)
                .await
                .unwrap();
        }
        root
    };
    assert_eq!(
        build().await,
        build().await,
        "identical ordered operations => identical root"
    );
}

// ------------------------------------------------------------------ batched reads

#[tokio::test]
#[cfg_attr(miri, ignore = "native 300-key batched-read fixture")]
async fn get_many_aligns_to_input_and_preserves_duplicates() {
    let (_s, t, empty) = tree().await;
    let muts: Vec<Mutation> = (0..300u32)
        .map(|i| up(&format!("k{i:04}"), &format!("v{i}")))
        .collect();
    let root = t.apply(empty, stamp(1), muts).await.unwrap();

    let keys: Vec<Vec<u8>> = vec![
        b"k0005".to_vec(),
        b"absent".to_vec(),
        b"k0299".to_vec(),
        b"k0005".to_vec(),
    ];
    let refs: Vec<&[u8]> = keys.iter().map(|k| k.as_slice()).collect();
    let got = t.get_many(root, &refs).await.unwrap();
    assert_eq!(
        got,
        vec![
            Some(Bytes::from("v5")),
            None,
            Some(Bytes::from("v299")),
            Some(Bytes::from("v5")),
        ]
    );
    assert!(t.get_many(root, &[]).await.unwrap().is_empty());
}

/// The wave claim: a 256-key batch costs O(depth) dependent waves, not O(keys).
#[tokio::test]
#[cfg_attr(miri, ignore = "native batched-wave corpus")]
async fn a_batched_read_costs_depth_waves_not_key_waves() {
    let (store, _t, _e) = tree().await;
    let t = BeTree::with_format(store.clone(), Format::tiny()).record_metrics();
    let keys: Vec<Bytes> = (0..4000u32)
        .map(|i| Bytes::from(format!("k{i:06}")))
        .collect();
    let (root, _model) = harness::build(&t, &keys, 256, 8).await.unwrap();
    let shape = harness::check_balanced(&t, root).await.unwrap();
    assert!(
        shape.max_leaf_depth >= 2,
        "the fixture must build a real multi-level tree"
    );

    // A cold tree, so every wave is a real fetch.
    let cold = BeTree::with_format(store, Format::tiny()).record_metrics();
    let probe: Vec<&[u8]> = keys.iter().take(256).map(|k| k.as_ref()).collect();
    let got = cold.get_many(root, &probe).await.unwrap();
    assert!(got.iter().all(|v| v.is_some()));
    let waves = cold.metrics().waves;
    assert!(
        waves <= (shape.max_leaf_depth as u64 + 2),
        "expected <= depth+2 waves for 256 keys, got {waves} at depth {}",
        shape.max_leaf_depth
    );
}

// ------------------------------------------------------------------ scans

#[tokio::test]
#[cfg_attr(miri, ignore = "native range-scan corpus")]
async fn scans_return_live_pairs_in_key_order() {
    let (_s, t, empty) = tree().await;
    let mut muts: Vec<Mutation> = Vec::new();
    for i in 0..300u32 {
        muts.push(up(&format!("a/{i:04}"), &format!("v{i}")));
        muts.push(up(&format!("b/{i:04}"), &format!("w{i}")));
    }
    let root = t.apply(empty, stamp(1), muts).await.unwrap();
    let root = t
        .apply(
            root,
            stamp(2),
            vec![Mutation::tombstone(Bytes::from("a/0007"))],
        )
        .await
        .unwrap();

    let a = t.scan_prefix(root, b"a/").await.unwrap();
    assert_eq!(a.len(), 299, "the tombstoned key is excluded");
    assert!(a.windows(2).all(|w| w[0].0 < w[1].0), "key order");
    assert!(a.iter().all(|(k, _)| k.starts_with(b"a/")));

    let window = t
        .scan_range(root, Some(b"a/0100"), Some(b"a/0105"))
        .await
        .unwrap();
    assert_eq!(
        window.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
        (100..105)
            .map(|i| Bytes::from(format!("a/{i:04}")))
            .collect::<Vec<_>>()
    );

    let all = t.scan_range(root, None, None).await.unwrap();
    assert_eq!(all.len(), 599);

    let many = t
        .scan_prefix_many(root, &[b"a/00", b"b/01", b"zz"])
        .await
        .unwrap();
    assert_eq!(many.len(), 3);
    assert_eq!(many[0].len(), 99, "a/0000..a/0099 minus the tombstone");
    assert_eq!(many[1].len(), 100);
    assert!(many[2].is_empty());
}

#[tokio::test]
#[cfg_attr(miri, ignore = "native streaming scan/diff corpus")]
async fn streaming_scan_and_diff_match_the_collecting_convenience_apis() {
    let (_store, t, empty) = tree().await;
    let keys: Vec<Bytes> = (0..300u32)
        .map(|i| Bytes::from(format!("stream/{i:04}")))
        .collect();
    let (root, _) = harness::build(&t, &keys, 37, 700).await.unwrap();
    let changed = t
        .apply(
            root,
            stamp(99),
            (0..300usize)
                .step_by(11)
                .map(|i| Mutation::upsert(keys[i].clone(), Bytes::from_static(b"changed")))
                .collect(),
        )
        .await
        .unwrap();

    let expected_scan = t
        .scan_range(root, Some(b"stream/0040"), Some(b"stream/0260"))
        .await
        .unwrap();
    for width in [0, 1, 64, 256] {
        let mut scan = t
            .scan_cursor(root, Some(b"stream/0040"), Some(b"stream/0260"))
            .with_prefetch_width(width);
        let mut streamed_scan = Vec::new();
        loop {
            let batch = scan.next_batch(17).await.unwrap();
            if batch.is_empty() {
                break;
            }
            assert!(batch.len() <= 17);
            streamed_scan.extend(batch);
        }
        assert_eq!(streamed_scan, expected_scan, "prefetch width {width}");
    }

    let expected_diff = t.diff(root, changed).await.unwrap();
    let mut diff = t.diff_cursor(root, changed);
    let mut streamed_diff = Vec::new();
    loop {
        let batch = diff.next_batch(7).await.unwrap();
        if batch.is_empty() {
            break;
        }
        assert!(batch.len() <= 7);
        streamed_diff.extend(batch);
    }
    assert_eq!(streamed_diff, expected_diff);
    assert!(t.diff(empty, empty).await.unwrap().is_empty());
}

#[tokio::test]
#[cfg_attr(miri, ignore = "native cache-overflow streaming corpus")]
async fn streaming_a_corpus_larger_than_the_cache_never_materializes_the_result() {
    let (store, writer, _empty) = tree().await;
    let keys: Vec<Bytes> = (0..5_000u32)
        .map(|i| Bytes::from(format!("bounded/{i:05}")))
        .collect();
    let (root, _) = harness::build(&writer, &keys, 256, 700).await.unwrap();
    let reader = BeTree::with_format(store, Format::tiny()).with_caches(CacheConfig {
        node_bytes: 4096,
        value_bytes: 4096,
    });
    let mut cursor = reader.scan_prefix_cursor(root, b"bounded/");
    let mut count = 0usize;
    loop {
        let batch = cursor.next_batch(13).await.unwrap();
        if batch.is_empty() {
            break;
        }
        assert!(batch.len() <= 13);
        count += batch.len();
    }
    assert_eq!(count, 5_000);
}

/// An empty prefix scans everything, and an all-0xFF prefix is unbounded above.
#[tokio::test]
async fn boundary_prefixes_behave() {
    let (_s, t, empty) = tree().await;
    let root = t
        .apply(
            empty,
            stamp(1),
            vec![
                Mutation::upsert(Bytes::from_static(b""), Bytes::from("empty-key")),
                Mutation::upsert(Bytes::from_static(&[0xff, 0xff]), Bytes::from("high")),
                up("m", "mid"),
            ],
        )
        .await
        .unwrap();
    assert_eq!(t.scan_prefix(root, b"").await.unwrap().len(), 3);
    assert_eq!(t.scan_prefix(root, &[0xff]).await.unwrap().len(), 1);
    assert_eq!(
        t.get(root, b"").await.unwrap(),
        Some(Bytes::from("empty-key"))
    );
}

// ------------------------------------------------------------------ values

#[tokio::test]
async fn values_around_the_inline_threshold_all_round_trip() {
    let (_s, t, empty) = tree().await;
    let inline = t.format().inline_value_bytes();
    let sizes = [
        0usize,
        1,
        inline - 1,
        inline,
        inline + 1,
        inline * 4,
        60_000,
    ];
    let muts: Vec<Mutation> = sizes
        .iter()
        .enumerate()
        .map(|(i, n)| Mutation::upsert(Bytes::from(format!("k{i}")), Bytes::from(vec![b'x'; *n])))
        .collect();
    let root = t.apply(empty, stamp(1), muts).await.unwrap();
    for (i, n) in sizes.iter().enumerate() {
        assert_eq!(
            t.get(root, format!("k{i}").as_bytes())
                .await
                .unwrap()
                .map(|v| v.len()),
            Some(*n),
            "value of {n} bytes"
        );
    }
    let m = t.metrics();
    assert!(m.inline_values > 0 && m.external_values > 0);
    // An out-of-line winner costs exactly one extra batched wave, not one per key.
    harness::check(&t, root).await.unwrap();
}

#[tokio::test]
async fn an_oversize_key_or_value_fails_without_publishing_a_root() {
    let (store, t, empty) = tree().await;
    let before = store.len();

    let e = t
        .apply(
            empty,
            stamp(1),
            vec![Mutation::upsert(
                Bytes::from(vec![b'k'; t.format().max_key_bytes() + 1]),
                Bytes::from("v"),
            )],
        )
        .await
        .unwrap_err();
    assert!(matches!(
        e,
        TreeError::Capacity(CapacityError::KeyTooLarge { .. })
    ));

    let e = t
        .apply(
            empty,
            stamp(1),
            vec![Mutation::upsert(
                Bytes::from("k"),
                Bytes::from(vec![b'v'; t.format().max_value_bytes() + 1]),
            )],
        )
        .await
        .unwrap_err();
    assert!(matches!(
        e,
        TreeError::Capacity(CapacityError::ValueTooLarge { .. })
    ));
    assert_eq!(store.len(), before, "no root, and no object, was published");
}

#[tokio::test]
#[cfg_attr(miri, ignore = "native maximum-key/value preflight fixture")]
async fn mutation_preflight_and_apply_agree_at_every_size_boundary() {
    let (_store, tree, empty) = tree().await;
    let f = tree.format();
    let cases = [
        Mutation::upsert(Bytes::new(), Bytes::new()),
        Mutation::tombstone(Bytes::from(vec![b'k'; f.max_key_bytes()])),
        Mutation::tombstone(Bytes::from(vec![b'k'; f.max_key_bytes() + 1])),
        Mutation::upsert("v", Bytes::from(vec![b'v'; f.max_value_bytes()])),
        Mutation::upsert("v", Bytes::from(vec![b'v'; f.max_value_bytes() + 1])),
    ];
    for (i, mutation) in cases.into_iter().enumerate() {
        let preflight = f.check_mutation(&mutation);
        let applied = tree.apply(empty, stamp(i as u64 + 1), vec![mutation]).await;
        assert_eq!(
            preflight.is_ok(),
            applied.is_ok(),
            "preflight and apply diverged for boundary case {i}: {preflight:?} / {applied:?}"
        );
    }
}

// ------------------------------------------------------------------ verification

#[tokio::test]
async fn verify_on_read_catches_a_lying_store_and_is_a_distinct_error_class() {
    let (store, t, empty) = tree().await;
    let root = t.apply(empty, stamp(1), vec![up("k", "v")]).await.unwrap();

    // Bit rot: the bytes at `root` are replaced with a *valid node of a different content*.
    let other = t
        .apply(empty, stamp(1), vec![up("k", "different")])
        .await
        .unwrap();
    let other_bytes = store.raw(other).expect("stored");
    store.corrupt(root, other_bytes);

    let fresh = BeTree::with_format(store.clone(), Format::tiny());
    assert!(matches!(
        fresh.get(root, b"k").await.unwrap_err(),
        TreeError::HashMismatch { .. }
    ));

    // ...and structural malformation is a *different* class, because the operational response differs.
    let mut junk = vec![0u8; t.format().node_bytes()];
    junk[0] = 0xff;
    store.corrupt(root, Bytes::from(junk));
    let fresh = BeTree::with_format(store.clone(), Format::tiny()).with_verify(VerifyPolicy::Never);
    assert!(matches!(
        fresh.get(root, b"k").await.unwrap_err(),
        TreeError::Decode { .. }
    ));
}

#[tokio::test]
#[cfg_attr(miri, ignore = "native work-budget corpus")]
async fn a_work_budget_bounds_a_walk_and_returns_no_partial_result() {
    let (store, t, _e) = tree().await;
    let keys: Vec<Bytes> = (0..3000u32)
        .map(|i| Bytes::from(format!("k{i:06}")))
        .collect();
    let (root, _m) = harness::build(&t, &keys, 64, 8).await.unwrap();

    let stingy = BeTree::with_format(store.clone(), Format::tiny()).with_budget(WorkBudget {
        max_objects: 2,
        max_fetched_bytes: 1 << 30,
    });
    let probe: Vec<&[u8]> = keys.iter().map(|k| k.as_ref()).collect();
    assert!(matches!(
        stingy.get_many(root, &probe).await.unwrap_err(),
        TreeError::ResourceLimit { .. }
    ));

    let byte_starved = BeTree::with_format(store, Format::tiny()).with_budget(WorkBudget {
        max_objects: u64::MAX,
        max_fetched_bytes: 100,
    });
    assert!(byte_starved.get_many(root, &probe).await.is_err());
}

/// Scalar `get` and a one-key `get_many` must price identical work identically, and a batch's visit
/// cost must grow per REFERENCE for a shared external value: N keys resolving to one value object are
/// still N resolutions, so value sharing cannot buy unbounded free CPU work under one budget.
#[tokio::test]
async fn visit_budget_prices_scalar_and_batched_reads_identically() {
    let (store, t, empty) = tree().await;
    // 64 bytes is above `Format::tiny()`'s inline threshold, so the winner is an external reference.
    let root = t
        .apply(empty, stamp(1), vec![up("k", &"x".repeat(64))])
        .await
        .unwrap();
    assert!(
        t.references(root)
            .await
            .unwrap()
            .iter()
            .any(|(kind, _)| *kind == cbe_tree::ObjectKind::Value),
        "fixture must produce an out-of-line value"
    );

    // Find the smallest visit budget at which each operation succeeds. Fresh tree per probe: budgets
    // are per-operation, and visits are charged for cache hits too, so caching must not change the
    // count.
    let procbe_tree = |budget: u64| {
        BeTree::with_format(store.clone(), Format::tiny()).with_budget(WorkBudget {
            max_objects: budget,
            max_fetched_bytes: u64::MAX,
        })
    };
    let mut min_get = None;
    let mut min_one = None;
    let mut min_dup = None;
    for budget in 1..64u64 {
        if min_get.is_none() && procbe_tree(budget).get(root, b"k").await.is_ok() {
            min_get = Some(budget);
        }
        if min_one.is_none()
            && procbe_tree(budget)
                .get_many(root, &[b"k".as_ref()])
                .await
                .is_ok()
        {
            min_one = Some(budget);
        }
        if min_dup.is_none()
            && procbe_tree(budget)
                .get_many(root, &[b"k".as_ref(), b"k".as_ref()])
                .await
                .is_ok()
        {
            min_dup = Some(budget);
        }
    }
    let min_get = min_get.expect("get succeeds within 64 visits");
    let min_one = min_one.expect("get_many succeeds within 64 visits");
    let min_dup = min_dup.expect("duplicate get_many succeeds within 64 visits");
    assert_eq!(
        min_get, min_one,
        "scalar get and one-key get_many charge equal visits"
    );
    assert_eq!(
        min_dup,
        min_one + 1,
        "a duplicated key costs exactly its extra external-value reference"
    );
}

/// A tree taller than `max_tree_level` cannot be built: the attempt fails and publishes nothing.
#[tokio::test]
#[cfg_attr(miri, ignore = "native maximum-depth root-growth fixture")]
async fn root_growth_beyond_max_tree_level_fails_without_publication() {
    use cbe_tree::format::FormatParams;
    // f_max = 3, max_tree_level = 1: a tree can hold at most 3 leaves.
    let params = FormatParams {
        max_tree_level: 1,
        ..*Format::tiny().params()
    };
    let fmt = Format::new(params).unwrap();
    let store = Arc::new(MemStore::new());
    let t = BeTree::with_format(store.clone(), fmt);
    let mut root = t.empty_root().await.unwrap();

    // Push until it must grow past level 1.
    let mut err = None;
    for round in 0..40u64 {
        let muts: Vec<Mutation> = (0..64u32)
            .map(|i| up(&format!("k{:04}", round * 64 + u64::from(i)), "v"))
            .collect();
        match t.apply(root, stamp(round + 1), muts).await {
            Ok(next) => root = next,
            Err(e) => {
                err = Some(e);
                break;
            }
        }
    }
    let err = err.expect("the depth limit must eventually be hit");
    assert!(matches!(
        err,
        TreeError::Capacity(CapacityError::TreeTooTall { .. })
    ));
    // The last successful root is still fully readable: the failed apply published nothing.
    harness::check_balanced(&t, root)
        .await
        .expect("the surviving root is intact");
}

/// Intent flows DOWN: the tree tells the store what kind of read this is, and the store owns the
/// mechanism. A store that tiers interior nodes differently from leaves can only do so if the tree
/// actually distinguishes them, so this asserts the hints arrive rather than trusting the wiring.
#[tokio::test]
#[cfg_attr(miri, ignore = "native access-hint tree fixture")]
async fn the_tree_tells_the_store_what_kind_of_read_each_wave_is() {
    use cbe_tree::AccessHint;
    use cbe_tree::store::{AddressedObject, NodeStore};
    use std::sync::Mutex;

    /// Records the hint of every read it serves.
    struct HintStore {
        inner: Arc<MemStore>,
        seen: Mutex<Vec<AccessHint>>,
    }
    impl HintStore {
        fn drain(&self) -> Vec<AccessHint> {
            std::mem::take(&mut *self.seen.lock().unwrap())
        }
    }
    impl NodeStore for HintStore {
        type Class = ();
        async fn get(&self, id: BlockId, h: AccessHint, m: usize) -> Result<Bytes, TreeError> {
            self.seen.lock().unwrap().push(h);
            self.inner.get(id, h, m).await
        }
        async fn get_many(
            &self,
            ids: &[BlockId],
            h: AccessHint,
            m: usize,
            t: u64,
        ) -> Vec<Result<Bytes, TreeError>> {
            self.seen.lock().unwrap().push(h);
            self.inner.get_many(ids, h, m, t).await
        }
        async fn put_batch(&self, o: Vec<AddressedObject>, c: ()) -> Result<(), TreeError> {
            self.inner.put_batch(o, c).await
        }
    }

    let mem = Arc::new(MemStore::new());
    let build = BeTree::with_format(mem.clone(), Format::tiny());
    let keys: Vec<Bytes> = (0..8000u32)
        .map(|i| Bytes::from(format!("k{i:06}")))
        .collect();
    let (root, _m) = harness::build(&build, &keys, 256, 8).await.unwrap();
    let shape = harness::check_balanced(&build, root).await.unwrap();
    assert!(
        shape.max_leaf_depth >= 2,
        "need internal levels above the leaves"
    );

    let fresh = || {
        let store = Arc::new(HintStore {
            inner: mem.clone(),
            seen: Mutex::new(Vec::new()),
        });
        let tree = BeTree::with_format(store.clone(), Format::tiny());
        (tree, store)
    };

    // A point read: interior levels are metadata, the leaf level is a random fetch.
    let (t, store) = fresh();
    let probe: Vec<&[u8]> = keys.iter().step_by(500).map(|k| k.as_ref()).collect();
    t.get_many(root, &probe).await.unwrap();
    let hints = store.drain();
    assert!(
        hints.contains(&AccessHint::MetadataOnly),
        "a descent through interior levels must hint MetadataOnly, got {hints:?}"
    );
    assert!(
        hints.contains(&AccessHint::Random),
        "the leaf level of a point read must hint Random, got {hints:?}"
    );
    assert!(
        !hints.contains(&AccessHint::SequentialPrefetch),
        "a point read is not a sequential sweep, got {hints:?}"
    );

    // An ordered scan: the leaf level becomes sequential, the spine stays metadata.
    let (t, store) = fresh();
    t.scan_range(root, None, None).await.unwrap();
    let hints = store.drain();
    assert!(
        hints.contains(&AccessHint::SequentialPrefetch),
        "an ordered scan must hint SequentialPrefetch at the leaf level, got {hints:?}"
    );
    assert!(
        hints.contains(&AccessHint::MetadataOnly),
        "a scan's spine is still metadata, got {hints:?}"
    );

    // A GC mark walk wants the object graph, not payloads.
    let (t, store) = fresh();
    t.references_many(&[root]).await.unwrap();
    assert_eq!(store.drain(), vec![AccessHint::MetadataOnly]);
}

#[tokio::test]
#[cfg_attr(miri, ignore = "native malformed-batch multi-node fixture")]
async fn a_malformed_batched_read_cardinality_is_rejected_before_bytes_meet_ids() {
    use cbe_tree::AccessHint;
    use cbe_tree::store::{AddressedObject, NodeStore};

    /// A store that returns one fewer result than requested.
    struct ShortStore(Arc<MemStore>);
    impl NodeStore for ShortStore {
        type Class = ();
        async fn get(&self, id: BlockId, h: AccessHint, m: usize) -> Result<Bytes, TreeError> {
            self.0.get(id, h, m).await
        }
        async fn get_many(
            &self,
            ids: &[BlockId],
            h: AccessHint,
            m: usize,
            t: u64,
        ) -> Vec<Result<Bytes, TreeError>> {
            let mut r = self.0.get_many(ids, h, m, t).await;
            r.pop();
            r
        }
        async fn put_batch(&self, o: Vec<AddressedObject>, c: ()) -> Result<(), TreeError> {
            self.0.put_batch(o, c).await
        }
    }

    let mem = Arc::new(MemStore::new());
    let t = BeTree::with_format(mem.clone(), Format::tiny());
    let empty = t.empty_root().await.unwrap();
    let root = t
        .apply(
            empty,
            stamp(1),
            (0..200u32).map(|i| up(&format!("k{i:04}"), "v")).collect(),
        )
        .await
        .unwrap();

    let short = BeTree::with_format(Arc::new(ShortStore(mem)), Format::tiny());
    let keys: Vec<Vec<u8>> = (0..200u32)
        .map(|i| format!("k{i:04}").into_bytes())
        .collect();
    let refs: Vec<&[u8]> = keys.iter().map(|k| k.as_slice()).collect();
    let e = short.get_many(root, &refs).await.unwrap_err();
    assert!(
        matches!(
            e,
            TreeError::Decode {
                reason: cbe_tree::DecodeError::BatchCardinality { .. },
                ..
            }
        ),
        "got {e:?}"
    );
}
