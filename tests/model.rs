//! Randomized comparison against an independently implemented `BTreeMap` model, plus the property
//! tests the implementation requires: reads, scans, tombstones, batch duplicate collapse, replay, reused stamps,
//! and snapshots.
//!
//! Seeded so every failure is reproducible without a proptest dependency; the seed appears in every
//! assertion message.

mod support;

use std::sync::Arc;

use cbe_tree::format::Format;
use cbe_tree::store::MemStore;
use cbe_tree::{BeTree, Mutation, VersionStamp};
use bytes::Bytes;
use support::{self as harness, KeyShape, Model, Rng};

fn tree(fmt: Format) -> (Arc<MemStore>, BeTree<MemStore>) {
    let store = Arc::new(MemStore::new());
    let t = BeTree::with_format(store.clone(), fmt).record_metrics();
    (store, t)
}

/// One randomized operation stream, checked against the model after every commit.
async fn run_stream(seed: u64, ops: usize, key_space: usize) {
    let (_s, t) = tree(Format::tiny());
    let fmt = t.format().clone();
    let mut rng = Rng::new(seed);
    let mut model = Model::new();
    let mut root = t.empty_root().await.unwrap();
    let mut snapshots: Vec<(cbe_tree::BlockId, Model)> = Vec::new();
    let inline = fmt.inline_value_bytes();

    let key = |i: usize| -> Bytes {
        match i % 5 {
            0 => Bytes::from(format!("k{i:06}")),
            1 => Bytes::from(format!("shared/prefix/path/segment/{i:06}")),
            2 => Bytes::from(vec![0u8; 1 + i % 4]),
            3 if i % 25 == 3 => Bytes::new(),
            _ => Bytes::from(format!("{:04x}", i)),
        }
    };

    let mut stamp_n = 1u64;
    for step in 0..ops {
        // Commit width varies, including the single-mutation case that produced the old degeneration.
        let width = 1 + rng.below(40);
        let mut muts: Vec<Mutation> = Vec::with_capacity(width);
        for _ in 0..width {
            let k = key(rng.below(key_space));
            match rng.below(10) {
                0..=1 => muts.push(Mutation::tombstone(k)),
                2 => {
                    // Straddle the inline threshold, including out-of-line values.
                    let n = inline.saturating_sub(2) + rng.below(8);
                    muts.push(Mutation::upsert(k, Bytes::from(rng.bytes(n))))
                }
                3 if rng.below(4) == 0 => {
                    muts.push(Mutation::upsert(k, Bytes::from(rng.bytes(inline * 3))))
                }
                _ => {
                    let n = rng.below(20);
                    muts.push(Mutation::upsert(k, Bytes::from(rng.bytes(n))))
                }
            }
        }
        // Occasionally reuse the previous stamp, to exercise deterministic tie resolution.
        let stamp = if rng.below(12) == 0 && stamp_n > 1 {
            VersionStamp::from_counter(stamp_n - 1)
        } else {
            stamp_n += 1;
            VersionStamp::from_counter(stamp_n)
        };

        model.apply(&fmt, stamp, &muts);
        root = t.apply(root, stamp, muts).await.unwrap();

        // Snapshot occasionally: an old root must keep reading its own state forever.
        if rng.below(8) == 0 {
            snapshots.push((root, model.clone()));
        }

        // Full comparison every few commits, plus structural invariants.
        if step % 7 == 0 || step + 1 == ops {
            harness::check_balanced(&t, root)
                .await
                .unwrap_or_else(|e| panic!("seed {seed} step {step}: {e}"));

            let want = model.scan_range(None, None);
            let got = t.scan_range(root, None, None).await.unwrap();
            assert_eq!(
                got.len(),
                want.len(),
                "seed {seed} step {step}: scan cardinality"
            );
            for ((gk, gv), (wk, wv)) in got.iter().zip(&want) {
                assert_eq!(
                    gk.as_ref(),
                    wk.as_slice(),
                    "seed {seed} step {step}: key order"
                );
                assert_eq!(
                    gv.as_ref(),
                    wv.as_slice(),
                    "seed {seed} step {step}: value for {wk:?}"
                );
            }

            // Batched point reads over the whole key space, present and absent.
            let probe: Vec<Bytes> = (0..key_space.min(120)).map(key).collect();
            let refs: Vec<&[u8]> = probe.iter().map(|k| k.as_ref()).collect();
            let got = t.get_many(root, &refs).await.unwrap();
            for (k, g) in probe.iter().zip(got) {
                assert_eq!(
                    g.as_ref().map(|b| b.as_ref()),
                    model.get(k).as_deref(),
                    "seed {seed} step {step}: get({k:?})"
                );
            }
        }
    }

    // Every retained snapshot still reads its own state.
    for (i, (snap_root, snap_model)) in snapshots.iter().enumerate() {
        let got = t.scan_range(*snap_root, None, None).await.unwrap();
        let want = snap_model.scan_range(None, None);
        assert_eq!(
            got.len(),
            want.len(),
            "seed {seed}: snapshot {i} cardinality"
        );
        for ((gk, gv), (wk, wv)) in got.iter().zip(&want) {
            assert_eq!(gk.as_ref(), wk.as_slice());
            assert_eq!(gv.as_ref(), wv.as_slice());
        }
    }
}

#[tokio::test]
#[cfg_attr(miri, ignore = "native randomized model matrix")]
async fn randomized_streams_match_the_model() {
    for seed in [1u64, 2, 3, 5, 8, 13, 21, 34] {
        run_stream(seed, 60, 400).await;
    }
}

#[tokio::test]
#[cfg_attr(miri, ignore = "native randomized small-key-space matrix")]
async fn randomized_streams_over_a_small_key_space_match_the_model() {
    // A small key space makes overwrites, tombstones, and stamp reuse collide constantly.
    for seed in [101u64, 202, 303] {
        run_stream(seed, 80, 24).await;
    }
}

/// Range and prefix scans must agree with the model on every boundary, not just in the interior.
#[tokio::test]
#[cfg_attr(miri, ignore = "native scan boundary matrix")]
async fn scans_match_the_model_on_every_boundary() {
    let (_s, t) = tree(Format::tiny());
    let keys = KeyShape::LongSharedPrefix.keys(1_200, 5);
    let (root, model) = harness::build(&t, &keys, 32, 10).await.unwrap();

    for k in keys.iter().step_by(97) {
        // Half-open windows anchored exactly on a stored key, on both sides.
        for (lo, hi) in [
            (Some(k.as_ref()), None),
            (None, Some(k.as_ref())),
            (Some(k.as_ref()), Some(k.as_ref())),
        ] {
            let got = t.scan_range(root, lo, hi).await.unwrap();
            let want = model.scan_range(lo, hi);
            assert_eq!(got.len(), want.len(), "window {lo:?}..{hi:?}");
            for ((gk, _), (wk, _)) in got.iter().zip(&want) {
                assert_eq!(gk.as_ref(), wk.as_slice());
            }
        }
    }
    for plen in [1usize, 8, 20, 40] {
        let p = &keys[600][..plen.min(keys[600].len())];
        let got = t.scan_prefix(root, p).await.unwrap();
        let want = model.scan_prefix(p);
        assert_eq!(got.len(), want.len(), "prefix {p:?}");
    }
    // Batched multi-prefix must equal the per-prefix results, in input order.
    let prefixes: Vec<&[u8]> = vec![&keys[10][..30], &keys[900][..30], b"nope"];
    let many = t.scan_prefix_many(root, &prefixes).await.unwrap();
    for (p, got) in prefixes.iter().zip(many) {
        assert_eq!(got, t.scan_prefix(root, p).await.unwrap(), "prefix {p:?}");
    }
}

/// The batched-input edges of `scan_prefix_many`: an empty request, duplicate prefixes, and *nested*
/// prefixes of different lengths. Nesting is the one that can go wrong quietly — a key matching both a
/// short and a long prefix must appear in both buckets, which the per-length bucketing has to get right.
#[tokio::test]
#[cfg_attr(miri, ignore = "native prefix scan fixture")]
async fn scan_prefix_many_handles_empty_duplicate_and_nested_prefixes() {
    let (_s, t) = tree(Format::tiny());
    let mut model = Model::new();
    let mut root = t.empty_root().await.unwrap();
    let muts: Vec<Mutation> = ["ab", "abc", "abcd", "abd", "b", "bc", "zz"]
        .iter()
        .map(|k| Mutation::upsert(Bytes::from(*k), Bytes::from(format!("v-{k}"))))
        .collect();
    let stamp = VersionStamp::from_counter(1);
    model.apply(t.format(), stamp, &muts);
    root = t.apply(root, stamp, muts).await.unwrap();

    // An empty request performs no work and returns no lists.
    assert!(t.scan_prefix_many(root, &[]).await.unwrap().is_empty());

    // Nested prefixes of different lengths, with one duplicated and one matching nothing.
    let prefixes: Vec<&[u8]> = vec![b"a", b"ab", b"abc", b"a", b"b", b"nope", b""];
    let got = t.scan_prefix_many(root, &prefixes).await.unwrap();
    assert_eq!(
        got.len(),
        prefixes.len(),
        "one list per input, in input order"
    );
    for (p, rows) in prefixes.iter().zip(&got) {
        let want = model.scan_prefix(p);
        assert_eq!(rows.len(), want.len(), "prefix {p:?}");
        for ((gk, gv), (wk, wv)) in rows.iter().zip(&want) {
            assert_eq!(gk.as_ref(), wk.as_slice(), "prefix {p:?}");
            assert_eq!(gv.as_ref(), wv.as_slice(), "prefix {p:?}");
        }
        // ...and it must equal the single-prefix API exactly.
        assert_eq!(rows, &t.scan_prefix(root, p).await.unwrap(), "prefix {p:?}");
    }
    assert_eq!(got[0], got[3], "duplicate inputs get identical lists");
    assert_eq!(
        got[0].len(),
        4,
        "a, ab, abc, abcd, abd => 4 keys under \"a\""
    );
    assert!(got[5].is_empty());
    assert_eq!(got[6].len(), 7, "the empty prefix matches every key");
}

/// `scan_prefix_many` must walk the UNION of the prefix ranges, not the span between the lowest and the
/// highest. Two narrow prefixes at opposite ends of the key space must not drag in everything between
/// them — the regression this pins is a span walk, which is correct but reads the whole tree.
#[tokio::test]
#[cfg_attr(miri, ignore = "native union-pruning fixture")]
async fn scan_prefix_many_prunes_to_the_union_not_the_span() {
    let store = Arc::new(MemStore::new());
    let fmt = Format::tiny();
    let builder = BeTree::with_format(store.clone(), fmt.clone());
    let keys: Vec<Bytes> = (0..20_000u32)
        .map(|i| Bytes::from(format!("k{i:08}")))
        .collect();
    let (root, model) = harness::build(&builder, &keys, 256, 8).await.unwrap();

    // A full scan, to establish what "reads everything" costs.
    let full = BeTree::with_format(store.clone(), fmt.clone()).record_metrics();
    full.scan_range(root, None, None).await.unwrap();
    let full_objects = full.metrics().objects_read;
    assert!(
        full_objects > 50,
        "the fixture must be a real multi-node tree"
    );

    // Two narrow prefixes at opposite ends: `k0000000` (lowest 10) and `k0001999` (highest 10).
    let narrow: Vec<&[u8]> = vec![b"k0000000", b"k0001999"];
    let narrow_tree = BeTree::with_format(store.clone(), fmt.clone()).record_metrics();
    let got = narrow_tree.scan_prefix_many(root, &narrow).await.unwrap();

    // Exactness first: the answer must equal the model's, per prefix and in input order.
    for (p, rows) in narrow.iter().zip(&got) {
        let want = model.scan_prefix(p);
        assert_eq!(rows.len(), want.len(), "prefix {p:?}");
        for ((gk, gv), (wk, wv)) in rows.iter().zip(&want) {
            assert_eq!(gk.as_ref(), wk.as_slice());
            assert_eq!(gv.as_ref(), wv.as_slice());
        }
    }
    assert!(!got[0].is_empty() && !got[1].is_empty());

    let narrow_objects = narrow_tree.metrics().objects_read;
    assert!(
        narrow_objects * 4 < full_objects,
        "two narrow prefixes read {narrow_objects} objects against {full_objects} for a full scan — \
         that is a span walk, not a union walk"
    );
}

/// Multi-root scans: a sharded stream keeps one root per shard, so a logical scan spans several roots.
/// Disjoint roots must merge; a key present under two roots must resolve by the winner tuple rather than
/// by which root was folded first.
#[tokio::test]
#[cfg_attr(miri, ignore = "native multi-root fixture")]
async fn multi_root_scans_merge_disjoint_shards_and_arbitrate_overlaps() {
    let (_s, t) = tree(Format::tiny());
    let fmt = t.format().clone();
    let empty = t.empty_root().await.unwrap();

    // Two disjoint shards.
    let shard = |lo: u32, hi: u32, tag: &'static str| {
        let muts: Vec<Mutation> = (lo..hi)
            .map(|i| Mutation::upsert(Bytes::from(format!("k{i:04}")), Bytes::from(tag)))
            .collect();
        muts
    };
    let a = t
        .apply(empty, VersionStamp::from_counter(1), shard(0, 300, "a"))
        .await
        .unwrap();
    let b = t
        .apply(empty, VersionStamp::from_counter(1), shard(300, 600, "b"))
        .await
        .unwrap();

    let all = t.scan_range_roots(&[a, b], None, None).await.unwrap();
    assert_eq!(all.len(), 600, "disjoint roots merge");
    assert!(
        all.windows(2).all(|w| w[0].0 < w[1].0),
        "merged output is key ordered"
    );

    // Duplicate roots must not double-count.
    assert_eq!(
        t.scan_range_roots(&[a, b, a, b], None, None).await.unwrap(),
        all
    );

    // Prefix and prefix-many across roots agree with the single-root API.
    let pfx = t.scan_prefix_roots(&[a, b], b"k03").await.unwrap();
    assert_eq!(pfx.len(), 100);
    let many = t
        .scan_prefix_many_roots(&[a, b], &[b"k00", b"k05", b"zz"])
        .await
        .unwrap();
    assert_eq!(many.len(), 3);
    assert_eq!(many[0].len(), 100);
    assert_eq!(many[1].len(), 100);
    assert!(many[2].is_empty());
    assert!(t.scan_prefix_many_roots(&[], &[b"k00"]).await.unwrap()[0].is_empty());

    // Overlapping roots: the greater order key wins, whichever root is listed first.
    let lo = t
        .apply(
            empty,
            VersionStamp::from_counter(5),
            vec![Mutation::upsert(
                Bytes::from_static(b"dup"),
                Bytes::from_static(b"old"),
            )],
        )
        .await
        .unwrap();
    let hi = t
        .apply(
            empty,
            VersionStamp::from_counter(9),
            vec![Mutation::upsert(
                Bytes::from_static(b"dup"),
                Bytes::from_static(b"new"),
            )],
        )
        .await
        .unwrap();
    for order in [[lo, hi], [hi, lo]] {
        let got = t.scan_range_roots(&order, None, None).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(
            got[0].1.as_ref(),
            b"new",
            "the greater winner wins regardless of root order"
        );
    }
    let _ = fmt;
}

/// Out-of-line values must be indistinguishable from inline ones through the read API, at every size
/// around the threshold.
#[tokio::test]
#[cfg_attr(miri, ignore = "native 200-value inline/external boundary fixture")]
async fn out_of_line_values_are_transparent() {
    let (_s, t) = tree(Format::tiny());
    let fmt = t.format().clone();
    let inline = fmt.inline_value_bytes();
    let mut rng = Rng::new(77);
    let mut model = Model::new();
    let mut root = t.empty_root().await.unwrap();

    let mut muts = Vec::new();
    let mut expected: Vec<(Bytes, Bytes)> = Vec::new();
    for i in 0..200usize {
        let n = if i % 3 == 0 {
            inline + 1 + i * 7
        } else {
            i % (inline + 1)
        };
        let k = Bytes::from(format!("k{i:04}"));
        let v = Bytes::from(rng.bytes(n));
        expected.push((k.clone(), v.clone()));
        muts.push(Mutation::upsert(k, v));
    }
    let stamp = VersionStamp::from_counter(1);
    model.apply(&fmt, stamp, &muts);
    root = t.apply(root, stamp, muts).await.unwrap();

    for (k, v) in &expected {
        assert_eq!(t.get(root, k).await.unwrap().as_ref(), Some(v), "key {k:?}");
    }
    let refs: Vec<&[u8]> = expected.iter().map(|(k, _)| k.as_ref()).collect();
    let got = t.get_many(root, &refs).await.unwrap();
    for ((k, v), g) in expected.iter().zip(got) {
        assert_eq!(g.as_ref(), Some(v), "batched key {k:?}");
    }
    let scanned = t.scan_range(root, None, None).await.unwrap();
    assert_eq!(scanned.len(), expected.len());
    for ((gk, gv), (wk, wv)) in scanned.iter().zip(&expected) {
        assert_eq!(gk, wk);
        assert_eq!(gv, wv);
    }
    // The metrics must confirm the fixture really used both representations.
    let metrics = t.metrics();
    assert!(metrics.external_values > 0);
    assert!(metrics.inline_values > 0);
}

/// The same randomized stream applied at the *selected* production format, so the default constants get
/// the same correctness coverage as the tiny test format.
#[tokio::test]
#[cfg_attr(miri, ignore = "native selected-format corpus")]
async fn the_selected_format_matches_the_model_too() {
    let (_s, t) = tree(Format::selected());
    let fmt = t.format().clone();
    let mut rng = Rng::new(5150);
    let mut model = Model::new();
    let mut root = t.empty_root().await.unwrap();

    for round in 0..12u64 {
        let muts: Vec<Mutation> = (0..2_000)
            .map(|_| {
                let klen = 1 + rng.below(24);
                let k = Bytes::from(rng.bytes(klen));
                if rng.below(8) == 0 {
                    Mutation::tombstone(k)
                } else {
                    let vlen = rng.below(80);
                    Mutation::upsert(k, Bytes::from(rng.bytes(vlen)))
                }
            })
            .collect();
        let stamp = VersionStamp::from_counter(round + 1);
        model.apply(&fmt, stamp, &muts);
        root = t.apply(root, stamp, muts).await.unwrap();
    }
    harness::check_balanced(&t, root).await.unwrap();
    let got = t.scan_range(root, None, None).await.unwrap();
    let want = model.scan_range(None, None);
    assert_eq!(got.len(), want.len());
    for ((gk, gv), (wk, wv)) in got.iter().zip(&want) {
        assert_eq!(gk.as_ref(), wk.as_slice());
        assert_eq!(gv.as_ref(), wv.as_slice());
    }
}
