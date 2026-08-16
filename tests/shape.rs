//! Shape and flush acceptance: multiway fanout, equal leaf depth across the whole order × commit-width
//! matrix, the byte-level flush floor, and buffer partition at every promoted pivot.
//!
//! This file contains the regression tests for the historical shape defect.
//! The two cases recorded on the old code are reproduced here as *regression* tests: one
//! 20,000-mutation apply, and 20,000 single-mutation applies. On the old code the first produced a
//! single two-child root over two 10,000-entry leaves, and the second produced only fanout-2 internal
//! nodes with leaf depths from 1 to 307.

mod support;

use std::sync::Arc;

use cbe_tree::format::{Format, FormatParams};
use cbe_tree::store::MemStore;
use cbe_tree::{BeTree, Mutation, VersionStamp};
use bytes::Bytes;
use support::{self as harness, COMMIT_WIDTHS, KEY_ORDERS, KeyShape, StoreModel};

#[test]
fn target_store_model_prices_waves_bytes_and_the_crossover() {
    let model = StoreModel {
        round_trip_ns: 2_000_000,
        bytes_per_second: 100 * 1024 * 1024,
    };
    assert!(model.cold_read_ns(3, 192 * 1024) < model.cold_read_ns(4, 64 * 1024));
    assert_eq!(
        StoreModel::crossover_round_trip_ns((3, 192 * 1024), (4, 64 * 1024), 100 * 1024 * 1024),
        Some(1_250_000)
    );
}

fn tiny_tree() -> (Arc<MemStore>, BeTree<MemStore>) {
    let store = Arc::new(MemStore::new());
    let tree = BeTree::with_format(store.clone(), Format::tiny()).record_metrics();
    (store, tree)
}

/// Case 1 of the recorded defect: one enormous apply. It used to make a two-child root over two
/// 10,000-entry leaves — nodes far past the nominal threshold, and a fanout of two.
#[tokio::test]
#[cfg_attr(miri, ignore = "native 20k-mutation shape fixture")]
async fn one_huge_apply_produces_a_wide_balanced_tree() {
    let (_s, t) = tiny_tree();
    let empty = t.empty_root().await.unwrap();
    let muts: Vec<Mutation> = (0..20_000u32)
        .map(|i| Mutation::upsert(Bytes::from(format!("k{i:08}")), Bytes::from_static(b"v")))
        .collect();
    let root = t
        .apply(empty, VersionStamp::from_counter(1), muts)
        .await
        .unwrap();

    let shape = harness::check_balanced(&t, root).await.unwrap();
    assert!(
        shape.max_fanout > 2,
        "internal nodes must widen beyond two children, got max fanout {}",
        shape.max_fanout
    );
    assert!(
        shape.internals > 1,
        "a 20k-key tree needs more than one internal node"
    );
    assert!(
        shape.leaves >= 20_000 / t.format().leaf_slots(),
        "leaves must respect the regular slot capacity: {} leaves for 20k keys at {} slots",
        shape.leaves,
        t.format().leaf_slots()
    );
    assert_eq!(t.metrics().undersized_flushes, 0);
    println!(
        "one-big-apply: {shape:?}\n  mean_fanout={:.2} eff_eps(B=leaf_slots)={:.3} space_amp={:.2}",
        shape.mean_fanout(),
        shape.effective_epsilon(t.format().leaf_slots() as f64),
        shape.space_amplification()
    );
}

/// The explicit long-key acceptance fixture. It is ignored in the ordinary gate because the
/// logical input alone is 400 MiB; CI/release qualification runs it explicitly.
#[tokio::test]
#[ignore = "100k x 4KiB capacity qualification; run explicitly in release"]
async fn one_hundred_thousand_four_kib_keys_keep_depth_and_amplification() {
    let store = Arc::new(MemStore::new());
    let tree = BeTree::new(store);
    assert!(tree.format().f_max() >= 16);
    assert!(tree.format().max_key_bytes() >= 4096);
    let empty = tree.empty_root().await.unwrap();
    let mutations: Vec<Mutation> = (0..100_000u64)
        .map(|i| {
            let mut key = vec![0u8; 4096];
            key[..8].copy_from_slice(&i.to_be_bytes());
            let digest = blake3::hash(&i.to_le_bytes());
            for (chunk, source) in key[8..]
                .chunks_mut(32)
                .zip(std::iter::repeat(digest.as_bytes()))
            {
                chunk.copy_from_slice(&source[..chunk.len()]);
            }
            Mutation::upsert(Bytes::from(key), Bytes::new())
        })
        .collect();
    let root = tree
        .apply(empty, VersionStamp::from_counter(1), mutations)
        .await
        .unwrap();
    let shape = harness::check_balanced(&tree, root).await.unwrap();
    assert!(shape.max_leaf_depth <= 3, "{shape:?}");
    assert!(shape.space_amplification() <= 1.6, "{shape:?}");
    assert!(shape.max_fanout >= 16, "{shape:?}");
    println!(
        "4KiB/100k: depth={} fanout={}..{} mean={:.2} nodes={} amp={:.3}",
        shape.max_leaf_depth,
        shape.min_fanout,
        shape.max_fanout,
        shape.mean_fanout(),
        shape.nodes,
        shape.space_amplification(),
    );
}

/// Case 2 of the recorded defect: 20,000 single-mutation applies. It used to produce 307 fanout-2
/// internal nodes and leaf depths from 1 to 307 — a spine, not a tree.
#[tokio::test]
#[cfg_attr(miri, ignore = "native 20k-commit shape fixture")]
async fn twenty_thousand_single_applies_stay_balanced_and_wide() {
    let (_s, t) = tiny_tree();
    let mut root = t.empty_root().await.unwrap();
    for i in 0..20_000u32 {
        root = t
            .apply(
                root,
                VersionStamp::from_counter(u64::from(i) + 1),
                vec![Mutation::upsert(
                    Bytes::from(format!("k{i:08}")),
                    Bytes::from_static(b"v"),
                )],
            )
            .await
            .unwrap();
    }
    let shape = harness::check_balanced(&t, root).await.unwrap();
    assert!(
        shape.max_fanout > 2,
        "max fanout {} must exceed 2",
        shape.max_fanout
    );
    assert!(
        shape.max_leaf_depth < 20,
        "depth {} must be logarithmic, not linear",
        shape.max_leaf_depth
    );
    assert_eq!(t.metrics().undersized_flushes, 0);
    // Every key must still be readable.
    for i in [0u32, 1, 9_999, 19_999] {
        assert_eq!(
            t.get(root, format!("k{i:08}").as_bytes()).await.unwrap(),
            Some(Bytes::from_static(b"v"))
        );
    }
    println!(
        "single-applies: depth={}..{} fanout={}..{} mean={:.2} nodes={}",
        shape.min_leaf_depth,
        shape.max_leaf_depth,
        shape.min_fanout,
        shape.max_fanout,
        shape.mean_fanout(),
        shape.nodes
    );
}

/// The full shape matrix: key order (ascending, descending, seeded random, and the hard key shapes)
/// crossed with commit widths 1, 16, 256, and all-at-once. Minimum and maximum leaf depth are reported
/// separately, because an average would hide an unbalanced spine.
#[tokio::test]
#[cfg_attr(miri, ignore = "native full shape matrix")]
async fn the_shape_matrix_is_balanced_and_wide_everywhere() {
    for shape_kind in KeyShape::all() {
        let base = shape_kind.keys(3_000, 0xC0FFEE);
        for &order in KEY_ORDERS {
            let mut keys = base.clone();
            order.apply(&mut keys, 0xD1CE);
            let order = order.name();
            for &width in COMMIT_WIDTHS {
                let (_s, t) = tiny_tree();
                let (root, model) = harness::build(&t, &keys, width, 12).await.unwrap();
                let s = harness::check_balanced(&t, root).await.unwrap_or_else(|e| {
                    panic!("{} / {order} / width {width}: {e}", shape_kind.name())
                });
                assert_eq!(
                    t.metrics().undersized_flushes,
                    0,
                    "{} / {order} / width {width}",
                    shape_kind.name()
                );
                // Every live key resolves, and nothing extra appears.
                let live = t.scan_range(root, None, None).await.unwrap();
                assert_eq!(
                    live.len(),
                    model.live_len(),
                    "{} / {order} / width {width}",
                    shape_kind.name()
                );
                if s.internals > 0 {
                    assert!(s.min_fanout >= 2);
                    assert!(s.max_fanout <= t.format().f_max());
                }
                println!(
                    "{:20} {:10} w={:>6}: depth={}..{} fanout={}..{} (mean {:.2}) nodes={} amp={:.2}",
                    shape_kind.name(),
                    order,
                    if width == usize::MAX {
                        "all".to_string()
                    } else {
                        width.to_string()
                    },
                    s.min_leaf_depth,
                    s.max_leaf_depth,
                    s.min_fanout,
                    s.max_fanout,
                    s.mean_fanout(),
                    s.nodes,
                    s.space_amplification(),
                );
            }
        }
    }
}

/// The flush floor: every ordinary flush's victim owns at least
/// `max(ceil(pending / child_count), MIN_FLUSH_BYTES)`. The tree records a counter when it does not,
/// and that counter must stay at zero across a workload that flushes thousands of times.
#[tokio::test]
#[cfg_attr(miri, ignore = "native flush-floor stress fixture")]
async fn every_ordinary_flush_meets_the_byte_floor() {
    let (_s, t) = tiny_tree();
    let keys = KeyShape::UniformRandom.keys(8_000, 99);
    let (root, _m) = harness::build(&t, &keys, 8, 16).await.unwrap();
    harness::check_balanced(&t, root).await.unwrap();

    let flushes = t.metrics().flushes;
    assert!(
        flushes > 50,
        "the fixture must actually flush, got {flushes}"
    );
    assert_eq!(t.metrics().undersized_flushes, 0);
    let mean_victim = t.metrics().victim_bytes.mean;
    assert!(
        mean_victim >= t.format().min_flush_bytes() as f64,
        "mean victim {mean_victim} below MIN_FLUSH_BYTES {}",
        t.format().min_flush_bytes()
    );
    println!(
        "flushes={flushes} mean_victim={mean_victim:.0} MIN_FLUSH_BYTES={} occupancy_p50={} victim_p50={}",
        t.format().min_flush_bytes(),
        t.metrics().buffer_occupancy.p50,
        t.metrics().victim_bytes.p50,
    );
}

/// A multi-node internal replacement must partition its buffer at every promoted pivot: a message for
/// key `k` must stay on `k`'s root-to-leaf path. `harness::check` verifies path ownership for every
/// entry in every node, so a mispartitioned buffer is caught structurally — and this fixture forces
/// internal partitions to actually happen.
#[tokio::test]
#[cfg_attr(miri, ignore = "native internal partition fixture")]
async fn internal_partitions_keep_every_message_on_its_own_path() {
    let (_s, t) = tiny_tree();
    // Wide random writes with buffered messages above every level force repeated internal partitions.
    let keys = KeyShape::UniformRandom.keys(20_000, 4242);
    let (root, mut model) = harness::build(&t, &keys, 512, 24).await.unwrap();
    // A wide commit drains the root buffer completely (each flush removes one whole child group, and
    // there are only `f_max` groups). A small trailing commit is what leaves messages resident above the
    // leaves, so the path-ownership check below has something to check.
    let tail: Vec<Mutation> = keys
        .iter()
        .take(3)
        .map(|k| Mutation::upsert(k.clone(), Bytes::from_static(b"tail-value")))
        .collect();
    let tail_stamp = VersionStamp::from_counter(1_000_000);
    model.apply(t.format(), tail_stamp, &tail);
    let root = t.apply(root, tail_stamp, tail).await.unwrap();
    let shape = harness::check_balanced(&t, root).await.unwrap();

    assert!(
        t.metrics().internal_partitions > 0,
        "the fixture must exercise internal partition"
    );
    assert!(
        shape.root_level >= 2,
        "root level {} should be deep",
        shape.root_level
    );
    assert!(
        shape.buffer_entries > 0,
        "messages must still be buffered above the leaves"
    );
    // And the resolved map still matches the model exactly.
    let live = t.scan_range(root, None, None).await.unwrap();
    let want = model.scan_range(None, None);
    assert_eq!(live.len(), want.len());
    for ((gk, gv), (wk, wv)) in live.iter().zip(&want) {
        assert_eq!(gk.as_ref(), wk.as_slice());
        assert_eq!(gv.as_ref(), wv.as_slice());
    }
    println!(
        "partitions={} root_level={} buffered={} nodes={}",
        t.metrics().internal_partitions,
        shape.root_level,
        shape.buffer_entries,
        shape.nodes
    );
}

/// A single commit large enough to grow the tree by more than one level must never encode an
/// over-capacity node.
#[tokio::test]
#[cfg_attr(miri, ignore = "native multi-level root-growth fixture")]
async fn one_commit_may_grow_the_tree_several_levels_at_once() {
    let (_s, t) = tiny_tree();
    let empty = t.empty_root().await.unwrap();
    // tiny format: f_max = 4, leaf_slots = 16 => 100k keys needs many levels in one go.
    let muts: Vec<Mutation> = (0..100_000u32)
        .map(|i| Mutation::upsert(Bytes::from(format!("k{i:08}")), Bytes::from_static(b"v")))
        .collect();
    let root = t
        .apply(empty, VersionStamp::from_counter(1), muts)
        .await
        .unwrap();
    let shape = harness::check_balanced(&t, root).await.unwrap();
    assert!(
        shape.root_level >= 3,
        "one commit should have grown several levels, got {}",
        shape.root_level
    );
    assert!(t.metrics().root_growths >= 3);
    println!(
        "grew to level {} with {} nodes in ONE apply",
        shape.root_level, shape.nodes
    );
}

/// A mutation whose accounted message cannot fit an empty regular buffer is routed directly toward its
/// leaf. It must not create a second node shape, and it must not participate in the flush floor.
#[tokio::test]
#[cfg_attr(miri, ignore = "native direct-routing fixture")]
async fn an_oversized_message_is_routed_directly_without_a_second_node_shape() {
    // A format whose buffer blob region is too small for a max-size key + max inline value.
    let params = FormatParams {
        node_bytes: 4096,
        f_max: 3,
        leaf_slots: 8,
        message_slots: 24,
        // A large max key makes the worst-case pivot reservation eat most of an internal node, leaving a
        // buffer blob region too small for one maximum-size message. That is exactly the case the format
        // routes directly toward the leaf.
        max_key_bytes: 1024,
        inline_value_bytes: 512,
        max_value_bytes: 1 << 16,
        max_object_bytes: 1 << 17,
        max_tree_level: 8,
        version_domain: *b"be-tree/direct\0\0",
    };
    let fmt = Format::new(params).expect("valid");
    assert!(
        fmt.message_is_oversized(fmt.max_key_bytes(), fmt.inline_value_bytes()),
        "the fixture format must actually make such a message oversized"
    );

    let store = Arc::new(MemStore::new());
    let t = BeTree::with_format(store, fmt.clone()).record_metrics();
    let mut root = t.empty_root().await.unwrap();

    // Enough small keys to build several levels, then the oversized messages.
    for round in 0..40u64 {
        let muts: Vec<Mutation> = (0..32u32)
            .map(|i| {
                Mutation::upsert(
                    Bytes::from(format!("k{:06}", round * 32 + u64::from(i))),
                    Bytes::from_static(b"v"),
                )
            })
            .collect();
        root = t
            .apply(root, VersionStamp::from_counter(round + 1), muts)
            .await
            .unwrap();
    }
    let big_key = Bytes::from(vec![b'z'; fmt.max_key_bytes()]);
    let big_val = Bytes::from(vec![b'V'; fmt.inline_value_bytes()]);
    root = t
        .apply(
            root,
            VersionStamp::from_counter(1000),
            vec![Mutation::upsert(big_key.clone(), big_val.clone())],
        )
        .await
        .unwrap();

    assert!(t.metrics().direct_routed > 0, "it must be directly routed");
    assert_eq!(t.metrics().undersized_flushes, 0);
    assert_eq!(t.get(root, &big_key).await.unwrap(), Some(big_val));
    let shape = harness::check_balanced(&t, root).await.unwrap();
    assert_eq!(
        shape.physical_bytes,
        shape.nodes * fmt.node_bytes(),
        "every node is exactly NODE_BYTES — no overflow shape exists"
    );
}

/// Equal resolved maps built in different orders are ALLOWED to have different roots. Asserting the
/// opposite would be an accidental confluence claim the implementation explicitly declines.
#[tokio::test]
#[cfg_attr(miri, ignore = "native order-dependent shape fixture")]
async fn equal_maps_built_in_different_orders_may_differ_structurally() {
    let keys = KeyShape::Ascending.keys(2_000, 1);
    let (_s1, t1) = tiny_tree();
    let (root_a, model_a) = harness::build(&t1, &keys, 1, 8).await.unwrap();

    let mut shuffled = keys.clone();
    let mut rng = harness::Rng::new(7);
    for i in (1..shuffled.len()).rev() {
        let j = rng.below(i + 1);
        shuffled.swap(i, j);
    }
    let (_s2, t2) = tiny_tree();
    let (root_b, model_b) = harness::build(&t2, &shuffled, 1, 8).await.unwrap();

    // The observable maps agree...
    assert_eq!(model_a.live_len(), model_b.live_len());
    let a = t1.scan_range(root_a, None, None).await.unwrap();
    let b = t2.scan_range(root_b, None, None).await.unwrap();
    assert_eq!(
        a.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
        b.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
        "the same key set is live under both roots"
    );
    // ...and the roots are permitted to differ. (Order keys differ per commit, so the values differ
    // too; the point is that no confluence is claimed or required.)
    assert_ne!(
        root_a, root_b,
        "median-driven splits are history dependent, by design"
    );
}
