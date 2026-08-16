//! Write-path cost, as an *evolving* sequence of commits rather than repeated writes against one warm
//! root. Each test here corresponds to a measured defect: the numbers in the assertions are the ones a
//! fixed-root microbenchmark structurally cannot see.

mod support;

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use cbe_tree::format::Format;
use cbe_tree::store::MemStore;
use cbe_tree::{BeTree, Mutation, VersionStamp};
use bytes::Bytes;
use support::{self as harness, CountingStore, KeyShape};

fn stamp(n: u64) -> VersionStamp {
    VersionStamp::from_counter(n)
}

/// A chain of single-mutation commits must not refetch the root each commit wrote. Before staged nodes
/// were cached, 10 commits caused 10 root refetches — the parent of every commit is the node the
/// previous commit just produced, so this is the *common* case, not an edge case.
#[tokio::test]
#[cfg_attr(miri, ignore = "native evolving 100k-key corpus")]
async fn an_evolving_commit_chain_does_not_refetch_what_it_just_wrote() {
    let mem = Arc::new(MemStore::new());
    let counting = Arc::new(CountingStore::new(mem));
    let t = BeTree::with_format(counting.clone(), Format::tiny()).record_metrics();

    // Seed a real multi-level tree first.
    let keys = KeyShape::Ascending.keys(4_000, 1);
    let (mut root, _m) = harness::build(&t, &keys, 256, 8).await.unwrap();
    let shape = harness::check_balanced(&t, root).await.unwrap();
    assert!(shape.max_leaf_depth >= 2, "need a real spine to refetch");

    let reads_before = counting.objects_fetched.load(Relaxed);
    const COMMITS: u64 = 10;
    for i in 0..COMMITS {
        root = t
            .apply(
                root,
                stamp(1_000_000 + i),
                vec![Mutation::upsert(
                    keys[i as usize].clone(),
                    Bytes::from_static(b"x"),
                )],
            )
            .await
            .unwrap();
    }
    let fetched = counting.objects_fetched.load(Relaxed) - reads_before;

    assert!(
        t.metrics().cache_warmed >= COMMITS,
        "every commit must warm the cache with the nodes it wrote"
    );
    assert!(
        fetched < COMMITS,
        "{COMMITS} single-mutation commits refetched {fetched} objects; a commit must not refetch the \
         root its predecessor just wrote"
    );
    // The result is still correct and still balanced.
    harness::check_balanced(&t, root).await.unwrap();
    for k in keys.iter().take(COMMITS as usize) {
        assert_eq!(
            t.get(root, k).await.unwrap(),
            Some(Bytes::from_static(b"x"))
        );
    }
}

/// Total bytes of every stored VALUE object. Node bytes dominate any aggregate, so value amplification
/// has to be measured on its own or it is invisible.
fn stored_value_bytes(mem: &MemStore) -> usize {
    mem.ids()
        .into_iter()
        .filter_map(|id| mem.raw(id))
        .filter(|b| b.starts_with(&cbe_tree::value::VALUE_MAGIC))
        .map(|b| b.len())
        .sum()
}

/// One logical value applied to many keys must be staged **once**. This is the reviewer's exact probe:
/// 256 keys sharing one 513-byte value. It used to submit 257 objects and 200 960 bytes when only 2
/// objects were new; at the 4 MiB value limit that shape is roughly a gigabyte of write traffic for one
/// logical value.
#[tokio::test]
#[cfg_attr(miri, ignore = "native shared-value fanout fixture")]
async fn one_shared_value_is_staged_once_not_once_per_key() {
    let mem = Arc::new(MemStore::new());
    let counting = Arc::new(CountingStore::new(mem.clone()));
    // The selected format, so 513 bytes really is just over its 512-byte inline threshold.
    let t = BeTree::with_format(counting.clone(), Format::selected()).record_metrics();
    let empty = t.empty_root().await.unwrap();
    assert_eq!(t.format().inline_value_bytes(), 512);

    let big = Bytes::from(vec![b'V'; 513]);
    let muts: Vec<Mutation> = (0..256u32)
        .map(|i| Mutation::upsert(Bytes::from(format!("k{i:04}")), big.clone()))
        .collect();

    let objects_before = counting.objects_put.load(Relaxed);
    let root = t.apply(empty, stamp(1), muts).await.unwrap();
    let objects = counting.objects_put.load(Relaxed) - objects_before;

    assert_eq!(
        t.metrics().duplicates_elided,
        255,
        "exactly 255 redundant stagings of one value must be elided"
    );
    // The precise claim: one logical value is one stored object, of one envelope plus one payload.
    assert_eq!(
        stored_value_bytes(&mem),
        cbe_tree::value::ENVELOPE_BYTES + 513,
        "256 keys sharing one value must store that value exactly once"
    );
    assert!(
        objects <= 4,
        "submitted {objects} objects for 256 keys sharing one value; expected one value plus a leaf"
    );

    // ...and every key still reads it back.
    for i in [0u32, 1, 128, 255] {
        assert_eq!(
            t.get(root, format!("k{i:04}").as_bytes()).await.unwrap(),
            Some(big.clone())
        );
    }
}

/// The value object is hashed exactly once. `value::encode` must hash to address the object, so staging
/// must reuse that id rather than hash the same bytes again.
///
/// The invariant that isolates this: `bytes_hashed` counts staging hashes, and since a value's id is now
/// reused rather than recomputed, the figure must be **independent of the value's size** — the same
/// key/node shape hashes the same number of bytes whether the value is 600 bytes or 600 KiB.
#[tokio::test]
#[cfg_attr(miri, ignore = "native 600 KiB value-hashing fixture")]
async fn a_value_object_is_hashed_once_not_twice() {
    async fn staging_bytes_for(value_len: usize) -> u64 {
        let t = BeTree::with_format(Arc::new(MemStore::new()), Format::selected()).record_metrics();
        let empty = t.empty_root().await.unwrap();
        t.apply(
            empty,
            stamp(1),
            vec![Mutation::upsert(
                Bytes::from_static(b"k"),
                Bytes::from(vec![b'V'; value_len]),
            )],
        )
        .await
        .unwrap();
        t.metrics().bytes_hashed
    }

    let small = staging_bytes_for(600).await;
    let large = staging_bytes_for(600 * 1024).await;
    assert_eq!(
        small, large,
        "staging hashed {small} bytes for a 600-byte value and {large} for a 600 KiB one; the value is \
         being hashed a second time at staging"
    );
    // Sanity: both really did go out of line, so the comparison is meaningful.
    assert!(small > 0);
}

/// Reusing one immutable `Bytes` payload across many keys must reuse its envelope and BLAKE3 result,
/// not merely deduplicate after doing the expensive work once per key.
#[tokio::test]
#[cfg_attr(miri, ignore = "native maximum-value hash fixture")]
async fn one_shared_large_bytes_is_encoded_and_hashed_once() {
    let t = BeTree::new(Arc::new(MemStore::new())).record_metrics();
    let empty = t.empty_root().await.unwrap();
    let value = Bytes::from(vec![b'V'; t.format().max_value_bytes()]);
    let muts = (0..256u32)
        .map(|i| Mutation::upsert(Bytes::from(format!("k{i:04}")), value.clone()))
        .collect();
    t.apply(empty, stamp(1), muts).await.unwrap();
    assert_eq!(t.metrics().value_objects_encoded, 1);
    assert_eq!(
        t.metrics().value_bytes_hashed,
        (value.len() + cbe_tree::value::ENVELOPE_BYTES) as u64
    );
}

/// Successful publication makes both newly written nodes and caller-supplied value payloads warm.
#[tokio::test]
async fn an_external_value_is_warm_immediately_after_write() {
    let counting = Arc::new(CountingStore::new(Arc::new(MemStore::new())));
    let t = BeTree::new(counting.clone());
    let empty = t.empty_root().await.unwrap();
    let root = t
        .apply(
            empty,
            stamp(1),
            vec![Mutation::upsert(
                Bytes::from_static(b"k"),
                Bytes::from(vec![b'V'; 4096]),
            )],
        )
        .await
        .unwrap();
    let before = counting.objects_fetched.load(Relaxed);
    assert_eq!(t.get(root, b"k").await.unwrap().unwrap().len(), 4096);
    assert_eq!(counting.objects_fetched.load(Relaxed), before);
}

/// Values normalized for a replay or a stale losing mutation must not be submitted when the resolved
/// root is unchanged. Content-addressed store deduplication is not permission to send duplicate bytes.
#[tokio::test]
async fn losing_external_values_are_not_submitted() {
    let counting = Arc::new(CountingStore::new(Arc::new(MemStore::new())));
    let t = BeTree::new(counting.clone());
    let empty = t.empty_root().await.unwrap();
    let original = Mutation::upsert(Bytes::from_static(b"k"), Bytes::from(vec![b'A'; 4096]));
    let root = t
        .apply(empty, stamp(2), vec![original.clone()])
        .await
        .unwrap();

    for (candidate_stamp, candidate) in [
        (stamp(2), original),
        (
            stamp(1),
            Mutation::upsert(Bytes::from_static(b"k"), Bytes::from(vec![b'B'; 4096])),
        ),
    ] {
        let puts = counting.put_batches.load(Relaxed);
        let objects = counting.objects_put.load(Relaxed);
        let bytes = counting.bytes_put.load(Relaxed);
        assert_eq!(
            t.apply(root, candidate_stamp, vec![candidate])
                .await
                .unwrap(),
            root
        );
        assert_eq!(counting.put_batches.load(Relaxed), puts);
        assert_eq!(counting.objects_put.load(Relaxed), objects);
        assert_eq!(counting.bytes_put.load(Relaxed), bytes);
    }

    // The root-changing case exercises reachability pruning rather than the unchanged-root fast path:
    // `k` loses, while the independent inline insertion wins. Only the replacement node is reachable.
    let puts = counting.put_batches.load(Relaxed);
    let objects = counting.objects_put.load(Relaxed);
    let bytes = counting.bytes_put.load(Relaxed);
    let changed = t
        .apply(
            root,
            stamp(1),
            vec![
                Mutation::upsert(Bytes::from_static(b"k"), Bytes::from(vec![b'B'; 4096])),
                Mutation::upsert(Bytes::from_static(b"z"), Bytes::from_static(b"inline")),
            ],
        )
        .await
        .unwrap();
    assert_ne!(changed, root);
    assert_eq!(counting.put_batches.load(Relaxed) - puts, 1);
    assert_eq!(counting.objects_put.load(Relaxed) - objects, 1);
    assert_eq!(
        counting.bytes_put.load(Relaxed) - bytes,
        t.format().node_bytes() as u64,
        "a losing external candidate must not be submitted alongside an independent winner"
    );
    assert_eq!(
        t.get(changed, b"k").await.unwrap().unwrap(),
        Bytes::from(vec![b'A'; 4096])
    );
    assert_eq!(
        t.get(changed, b"z").await.unwrap().unwrap(),
        Bytes::from_static(b"inline")
    );
}

/// Repeated identical inline commits produce no store call and no refetches.
#[tokio::test]
async fn replaying_a_commit_writes_nothing_and_reads_nothing() {
    let mem = Arc::new(MemStore::new());
    let counting = Arc::new(CountingStore::new(mem.clone()));
    let t = BeTree::with_format(counting.clone(), Format::tiny());
    let empty = t.empty_root().await.unwrap();
    let muts: Vec<Mutation> = (0..4u32)
        .map(|i| Mutation::upsert(Bytes::from(format!("k{i}")), Bytes::from_static(b"v")))
        .collect();

    let once = t.apply(empty, stamp(1), muts.clone()).await.unwrap();
    let objects = mem.len();
    let fetched = counting.objects_fetched.load(Relaxed);
    let puts = counting.put_batches.load(Relaxed);

    let twice = t.apply(once, stamp(1), muts).await.unwrap();
    assert_eq!(once, twice);
    assert_eq!(
        mem.len(),
        objects,
        "content addressing makes the write a no-op"
    );
    assert_eq!(
        counting.put_batches.load(Relaxed),
        puts,
        "no-op replay must not call the store"
    );
    assert_eq!(
        counting.objects_fetched.load(Relaxed),
        fetched,
        "the root it needs is the one it just wrote, and it is cached"
    );
}
