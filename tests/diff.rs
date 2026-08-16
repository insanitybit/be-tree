//! `diff`: exact sorted unique keys, overlay-aware equal-subtree skipping, and re-synchronization by key
//! when shapes disagree.
//!
//! The load-bearing case is the third test. An equal subtree id proves the subtree's *own* entries match,
//! but a newer message sitting in an ancestor buffer above it still changes the resolved winner. Copying
//! a prolly-tree cursor without overlay state would silently miss exactly that key.

mod support;

use std::sync::Arc;

use be_tree::format::Format;
use be_tree::store::MemStore;
use be_tree::{BeTree, Mutation, VersionStamp};
use bytes::Bytes;
use support::{self as harness, KeyShape, Model, Rng};

fn tree() -> (Arc<MemStore>, BeTree<MemStore>) {
    let store = Arc::new(MemStore::new());
    let tree = BeTree::with_format(store.clone(), Format::tiny()).record_metrics();
    (store, tree)
}

fn stamp(n: u64) -> VersionStamp {
    VersionStamp::from_counter(n)
}

#[tokio::test]
#[cfg_attr(miri, ignore = "native 2k-key equal-root fixture")]
async fn an_equal_root_diffs_to_nothing_with_zero_io() {
    let (_s, t) = tree();
    let keys = KeyShape::Ascending.keys(2_000, 1);
    let (root, _m) = harness::build(&t, &keys, 64, 8).await.unwrap();
    let before = t.metrics().waves;
    assert!(t.diff(root, root).await.unwrap().is_empty());
    assert_eq!(t.metrics().waves, before, "equal roots cost no I/O");
}

#[tokio::test]
#[cfg_attr(miri, ignore = "native multi-node diff fixture")]
async fn diff_reports_exactly_the_changed_keys_in_sorted_order() {
    let (_s, t) = tree();
    let keys = KeyShape::Ascending.keys(3_000, 1);
    let (a, mut model_a) = harness::build(&t, &keys, 64, 8).await.unwrap();
    let mut model_b = model_a.clone();

    let changed: Vec<Bytes> = vec![
        keys[7].clone(),
        keys[1_500].clone(),
        keys[2_999].clone(),
        Bytes::from_static(b"brand-new-key"),
    ];
    let muts: Vec<Mutation> = changed
        .iter()
        .enumerate()
        .map(|(i, k)| {
            if i == 2 {
                Mutation::tombstone(k.clone())
            } else {
                Mutation::upsert(k.clone(), Bytes::from_static(b"changed"))
            }
        })
        .collect();
    let s = stamp(1_000_000);
    model_b.apply(t.format(), s, &muts);
    let b = t.apply(a, s, muts).await.unwrap();

    let got = t.diff(a, b).await.unwrap();
    let want = model_b.diff(&model_a);
    assert_eq!(
        got.iter().map(|k| k.to_vec()).collect::<Vec<_>>(),
        want,
        "diff must equal the model's exact answer"
    );
    assert!(got.windows(2).all(|w| w[0] < w[1]), "sorted and unique");
    // ...and it is symmetric.
    assert_eq!(t.diff(b, a).await.unwrap(), got);
    let _ = &mut model_a;
}

/// THE overlay case. `b` differs from `a` only by messages that are still resident in `b`'s root buffer,
/// so every one of `b`'s children is byte-identical to `a`'s. A cursor that skipped equal subtrees
/// without consulting the ancestor overlays would report nothing.
#[tokio::test]
#[cfg_attr(miri, ignore = "native ancestor-overlay fixture")]
async fn a_newer_ancestor_buffer_message_over_an_equal_subtree_is_detected() {
    let (_s, t) = tree();
    let keys = KeyShape::Ascending.keys(4_000, 1);
    let (a, model_a) = harness::build(&t, &keys, 128, 8).await.unwrap();
    let mut model_b = model_a.clone();

    // Two mutations only: small enough that the root buffer absorbs them without flushing.
    let muts = vec![
        Mutation::upsert(keys[10].clone(), Bytes::from_static(b"buffered-newer")),
        Mutation::upsert(keys[3_500].clone(), Bytes::from_static(b"buffered-newer")),
    ];
    let s = stamp(9_000_000);
    model_b.apply(t.format(), s, &muts);
    let b = t.apply(a, s, muts).await.unwrap();

    // Confirm the fixture really is the interesting one: a and b share their child subtrees.
    let va = t.view(a).await.unwrap();
    let vb = t.view(b).await.unwrap();
    assert!(!va.is_leaf() && !vb.is_leaf());
    assert_eq!(
        va.children().collect::<Vec<_>>(),
        vb.children().collect::<Vec<_>>(),
        "the fixture must leave every child subtree byte-identical"
    );

    let got = t.diff(a, b).await.unwrap();
    assert_eq!(
        got,
        vec![keys[10].clone(), keys[3_500].clone()],
        "the buffered override must be detected through the equal-subtree skip"
    );
    assert_eq!(
        got.iter().map(|k| k.to_vec()).collect::<Vec<_>>(),
        model_b.diff(&model_a)
    );
    assert!(
        t.metrics().diff_equal_id_skips > 0,
        "the equal-subtree skip must actually have fired"
    );
}

/// The converse: an ancestor overlay that *loses* to the leaf must not be reported. Detecting a
/// difference requires resolution, not merely the presence of an overlay entry.
#[tokio::test]
#[cfg_attr(miri, ignore = "native ancestor-overlay fixture")]
async fn an_older_ancestor_buffer_message_is_not_a_difference() {
    let (_s, t) = tree();
    let keys = KeyShape::Ascending.keys(4_000, 1);
    // Build with high stamps, then inject a LOWER-stamped message for a key that already exists.
    let mut model = Model::new();
    let mut root = t.empty_root().await.unwrap();
    for chunk in keys.chunks(128) {
        let muts: Vec<Mutation> = chunk
            .iter()
            .map(|k| Mutation::upsert(k.clone(), Bytes::from_static(b"winner")))
            .collect();
        let s = stamp(1_000);
        model.apply(t.format(), s, &muts);
        root = t.apply(root, s, muts).await.unwrap();
    }
    let a = root;
    let model_a = model.clone();

    let muts = vec![Mutation::upsert(
        keys[10].clone(),
        Bytes::from_static(b"loser"),
    )];
    let s = stamp(5); // strictly lower order key
    model.apply(t.format(), s, &muts);
    let b = t.apply(a, s, muts).await.unwrap();

    assert_ne!(a, b, "the write still produced a new root");
    assert_eq!(
        t.diff(a, b).await.unwrap(),
        Vec::<Bytes>::new(),
        "a losing overlay changes no resolved value, so diff reports nothing"
    );
    assert!(model.diff(&model_a).is_empty());
    assert_eq!(
        t.get(b, &keys[10]).await.unwrap(),
        Some(Bytes::from_static(b"winner"))
    );
}

/// A localized edit must visit work proportional to the changed region plus bounded node-boundary work
/// — reported, not asserted as an unconditional asymptotic bound.
#[tokio::test]
#[cfg_attr(miri, ignore = "native 20k-key locality fixture")]
async fn a_localized_edit_visits_far_less_than_the_corpus() {
    let (store, t) = tree();
    let keys = KeyShape::Ascending.keys(20_000, 1);
    let (a, _m) = harness::build(&t, &keys, 256, 8).await.unwrap();
    let shape = harness::check_balanced(&t, a).await.unwrap();

    // Change one key, in a commit wide enough to reach a leaf.
    let muts: Vec<Mutation> = std::iter::once(Mutation::upsert(
        keys[9_000].clone(),
        Bytes::from_static(b"changed"),
    ))
    .collect();
    let b = t.apply(a, stamp(8_000_000), muts).await.unwrap();

    // Fresh handle so the metrics measure only the diff.
    let fresh = BeTree::with_format(store, Format::tiny()).record_metrics();
    let got = fresh.diff(a, b).await.unwrap();
    assert_eq!(got, vec![keys[9_000].clone()]);

    let visited = fresh.metrics().diff_visited_nodes;
    let skips = fresh.metrics().diff_equal_id_skips;
    assert!(
        visited < shape.nodes as u64 / 4,
        "visited {visited} nodes of {} — a localized edit must not walk the corpus",
        shape.nodes
    );
    assert!(skips > 0);
    println!(
        "localized diff: visited_nodes={visited} equal_id_skips={skips} visited_keys={} corpus_nodes={}",
        fresh.metrics().diff_visited_keys,
        shape.nodes
    );
}

/// Misaligned pivots must re-synchronize BY KEY rather than collecting whole subtrees. Two trees built
/// from different orders have unrelated shapes; the answer must still be exact.
#[tokio::test]
#[cfg_attr(miri, ignore = "native misaligned-shape fixture")]
async fn misaligned_shapes_resynchronize_by_key_and_stay_exact() {
    let keys = KeyShape::UniformRandom.keys(3_000, 12345);

    let (_s1, t1) = tree();
    let mut model_a = Model::new();
    let mut a = t1.empty_root().await.unwrap();
    for (i, chunk) in keys.chunks(1).enumerate() {
        let muts: Vec<Mutation> = chunk
            .iter()
            .map(|k| Mutation::upsert(k.clone(), Bytes::from_static(b"v")))
            .collect();
        let s = stamp(i as u64 + 1);
        model_a.apply(t1.format(), s, &muts);
        a = t1.apply(a, s, muts).await.unwrap();
    }

    // Same tree handle (same store) so both roots are readable by one tree, built all-at-once instead.
    let mut model_b = Model::new();
    let mut b = t1.empty_root().await.unwrap();
    let muts: Vec<Mutation> = keys
        .iter()
        .map(|k| Mutation::upsert(k.clone(), Bytes::from_static(b"v")))
        .collect();
    let s = stamp(10_000_000);
    model_b.apply(t1.format(), s, &muts);
    b = t1.apply(b, s, muts).await.unwrap();

    let sa = harness::check_balanced(&t1, a).await.unwrap();
    let sb = harness::check_balanced(&t1, b).await.unwrap();
    assert_ne!(a, b, "different build orders give different shapes");

    // Every key's resolved VALUE is the same; only the order keys differ, which is not observable.
    let got = t1.diff(a, b).await.unwrap();
    let want = model_b.diff(&model_a);
    assert_eq!(
        got.iter().map(|k| k.to_vec()).collect::<Vec<_>>(),
        want,
        "misaligned shapes must still produce the exact answer"
    );
    assert!(got.is_empty(), "the resolved maps are observably identical");
    println!(
        "misaligned diff exact: a(nodes={} depth={}) b(nodes={} depth={})",
        sa.nodes, sa.max_leaf_depth, sb.nodes, sb.max_leaf_depth
    );
}

#[tokio::test]
#[cfg_attr(miri, ignore = "native whole-tree diff fixture")]
async fn diff_against_an_empty_tree_lists_every_live_key() {
    let (_s, t) = tree();
    let empty = t.empty_root().await.unwrap();
    let keys = KeyShape::Ascending.keys(1_500, 1);
    let (root, model) = harness::build(&t, &keys, 32, 8).await.unwrap();

    let got = t.diff(empty, root).await.unwrap();
    assert_eq!(got.len(), model.live_len());
    assert_eq!(
        got.iter().map(|k| k.to_vec()).collect::<Vec<_>>(),
        model.diff(&Model::new())
    );

    // Tombstoning everything makes the tree observably empty again, so the diff back to `empty` is
    // nothing — a tombstone is indistinguishable from absent.
    let muts: Vec<Mutation> = keys
        .iter()
        .map(|k| Mutation::tombstone(k.clone()))
        .collect();
    let dead = t.apply(root, stamp(20_000_000), muts).await.unwrap();
    assert!(t.diff(empty, dead).await.unwrap().is_empty());
}

/// Randomized diff: many independent divergences, checked against the model.
#[tokio::test]
#[cfg_attr(miri, ignore = "native randomized diff matrix")]
async fn randomized_divergences_match_the_model() {
    for seed in [11u64, 22, 33, 44] {
        let (_s, t) = tree();
        let mut rng = Rng::new(seed);
        let keys = KeyShape::UniformRandom.keys(2_500, seed);
        let (a, model_a) = harness::build(&t, &keys, 64, 10).await.unwrap();

        let mut model_b = model_a.clone();
        let mut b = a;
        for round in 0..4u64 {
            let n = 1 + rng.below(30);
            let muts: Vec<Mutation> = (0..n)
                .map(|_| {
                    let k = keys[rng.below(keys.len())].clone();
                    if rng.below(3) == 0 {
                        Mutation::tombstone(k)
                    } else {
                        let vlen = rng.below(30);
                        Mutation::upsert(k, Bytes::from(rng.bytes(vlen)))
                    }
                })
                .collect();
            let s = stamp(50_000_000 + round);
            model_b.apply(t.format(), s, &muts);
            b = t.apply(b, s, muts).await.unwrap();
        }

        let got: Vec<Vec<u8>> = t
            .diff(a, b)
            .await
            .unwrap()
            .iter()
            .map(|k| k.to_vec())
            .collect();
        assert_eq!(got, model_b.diff(&model_a), "seed {seed}");
        assert_eq!(
            t.diff(b, a)
                .await
                .unwrap()
                .iter()
                .map(|k| k.to_vec())
                .collect::<Vec<_>>(),
            got,
            "seed {seed}: diff is symmetric"
        );
    }
}
