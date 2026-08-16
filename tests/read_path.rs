//! Read-path cost: batched cursor traversal, external-value deduplication and caching, and cold-miss
//! coalescing. Every assertion here counts *store interactions*, because these are the costs a warm
//! `MemStore` latency measurement cannot see.

mod support;

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use be_tree::format::Format;
use be_tree::store::MemStore;
use be_tree::tree::CacheConfig;
use be_tree::{BeTree, Mutation, VersionStamp};
use bytes::Bytes;
use support::{self as harness, CountingStore, KeyShape};

fn stamp(n: u64) -> VersionStamp {
    VersionStamp::from_counter(n)
}

/// A cold full scan must batch its reads. It used to issue one scalar `get` per visited node — 337 gets
/// and zero `get_many` calls for a 4 000-key tree.
#[tokio::test]
#[cfg_attr(miri, ignore = "native 4k-row scan fixture")]
async fn a_cold_scan_batches_its_reads_instead_of_one_get_per_node() {
    let mem = Arc::new(MemStore::new());
    let build = BeTree::with_format(mem.clone(), Format::tiny());
    let keys = KeyShape::Ascending.keys(4_000, 1);
    let (root, _m) = harness::build(&build, &keys, 256, 8).await.unwrap();
    let shape = harness::check_balanced(&build, root).await.unwrap();
    assert!(
        shape.nodes > 100,
        "need a real corpus, got {} nodes",
        shape.nodes
    );

    // A cold handle: nothing cached, so every node must come from the store.
    let counting = Arc::new(CountingStore::new(mem.clone()));
    let cold = BeTree::with_format(counting.clone(), Format::tiny());
    let rows = cold.scan_range(root, None, None).await.unwrap();
    assert_eq!(rows.len(), keys.len());

    let gets = counting.gets.load(Relaxed);
    let batches = counting.get_many_calls.load(Relaxed);
    let objects = counting.objects_fetched.load(Relaxed);
    assert!(
        batches > 0,
        "a cold scan issued {batches} batched reads and {gets} scalar gets for {objects} objects"
    );
    // The whole point: dependent round trips must be far fewer than nodes visited.
    let round_trips = gets + batches;
    assert!(
        round_trips * 4 < objects,
        "{round_trips} round trips for {objects} objects is not batching"
    );
    println!(
        "cold full scan: {objects} objects in {round_trips} round trips ({batches} batched, {gets} scalar)"
    );
}

/// The same for a prefix scan over a narrow window, and for `scan_prefix_many`.
#[tokio::test]
#[cfg_attr(miri, ignore = "native cold prefix scan fixture")]
async fn cold_prefix_scans_batch_too() {
    let mem = Arc::new(MemStore::new());
    let build = BeTree::with_format(mem.clone(), Format::tiny());
    let keys: Vec<Bytes> = (0..8_000u32)
        .map(|i| Bytes::from(format!("p{:02}/k{i:06}", i % 40)))
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    let (root, _m) = harness::build(&build, &sorted, 256, 8).await.unwrap();

    for label in ["scan_prefix", "scan_prefix_many"] {
        let counting = Arc::new(CountingStore::new(mem.clone()));
        let cold = BeTree::with_format(counting.clone(), Format::tiny());
        let rows = if label == "scan_prefix" {
            cold.scan_prefix(root, b"p07/").await.unwrap().len()
        } else {
            cold.scan_prefix_many(root, &[b"p07/", b"p23/"])
                .await
                .unwrap()
                .iter()
                .map(|r| r.len())
                .sum()
        };
        assert!(rows > 0);
        let objects = counting.objects_fetched.load(Relaxed);
        let round_trips = counting.gets.load(Relaxed) + counting.get_many_calls.load(Relaxed);
        assert!(
            counting.get_many_calls.load(Relaxed) > 0,
            "{label} did not batch: {round_trips} round trips for {objects} objects"
        );
        println!("{label}: {objects} objects in {round_trips} round trips");
    }
}

/// A wave may contain both decoded-cache hits and store misses. The returned entries must remain
/// sorted by content id so binary lookup cannot silently skip a node when the hit precedes a miss.
#[tokio::test]
async fn mixed_cached_and_fetched_nodes_preserve_batch_results() {
    let mem = Arc::new(MemStore::new());
    let build = BeTree::with_format(mem.clone(), Format::tiny());
    let keys = KeyShape::Ascending.keys(4_000, 1);
    let (root, _model) = harness::build(&build, &keys, 256, 8).await.unwrap();
    let refs: Vec<&[u8]> = keys.iter().take(512).map(Bytes::as_ref).collect();
    let expected = build.get_many(root, &refs).await.unwrap();

    let cold = BeTree::with_format(mem, Format::tiny());
    // Warm different paths one at a time, then issue a broad batch so each dependent wave has a
    // deterministic mixture of cache hits and misses.
    for key in keys.iter().step_by(17).take(30) {
        cold.get(root, key).await.unwrap();
        assert_eq!(cold.get_many(root, &refs).await.unwrap(), expected);
    }
}

/// `diff` must NOT prefetch: its equal-subtree skip exists to avoid reading those subtrees, so batching
/// them would fetch exactly what the skip saves. This pins the guard that keeps the two apart.
#[tokio::test]
#[cfg_attr(miri, ignore = "native localized-diff I/O fixture")]
async fn diff_still_skips_rather_than_prefetching_what_it_will_not_read() {
    let mem = Arc::new(MemStore::new());
    let build = BeTree::with_format(mem.clone(), Format::tiny());
    let keys = KeyShape::Ascending.keys(20_000, 1);
    let (a, _m) = harness::build(&build, &keys, 256, 8).await.unwrap();
    let shape = harness::check_balanced(&build, a).await.unwrap();
    let b = build
        .apply(
            a,
            stamp(9_000_000),
            vec![Mutation::upsert(
                keys[9_000].clone(),
                Bytes::from_static(b"changed"),
            )],
        )
        .await
        .unwrap();

    let counting = Arc::new(CountingStore::new(mem));
    let cold = BeTree::with_format(counting.clone(), Format::tiny()).record_metrics();
    assert_eq!(cold.diff(a, b).await.unwrap(), vec![keys[9_000].clone()]);

    let objects = counting.objects_fetched.load(Relaxed);
    assert!(
        objects * 4 < shape.nodes as u64,
        "a localized diff fetched {objects} of {} nodes — prefetch is defeating the skip",
        shape.nodes
    );
    assert!(cold.metrics().diff_equal_id_skips > 0);
    println!(
        "localized diff: fetched {objects} objects of {} nodes, {} equal-id skips",
        shape.nodes,
        cold.metrics().diff_equal_id_skips
    );
}

/// One external value shared by many keys must be fetched ONCE per read, not once per reference.
#[tokio::test]
async fn a_shared_external_value_is_fetched_once_not_once_per_reference() {
    let mem = Arc::new(MemStore::new());
    let t = BeTree::with_format(mem.clone(), Format::selected());
    let empty = t.empty_root().await.unwrap();
    let big = Bytes::from(vec![b'V'; 4096]);
    let muts: Vec<Mutation> = (0..8u32)
        .map(|i| Mutation::upsert(Bytes::from(format!("k{i}")), big.clone()))
        .collect();
    let root = t.apply(empty, stamp(1), muts).await.unwrap();

    // Cold handle, all eight keys in one batch: eight references, one distinct value object.
    let counting = Arc::new(CountingStore::new(mem.clone()));
    let cold = BeTree::with_format(counting.clone(), Format::selected());
    let keys: Vec<Vec<u8>> = (0..8u32).map(|i| format!("k{i}").into_bytes()).collect();
    let refs: Vec<&[u8]> = keys.iter().map(|k| k.as_slice()).collect();
    let got = cold.get_many(root, &refs).await.unwrap();
    assert!(got.iter().all(|v| v.as_deref() == Some(big.as_ref())));

    // Value objects fetched: the node(s) plus exactly one value.
    let value_fetches = counting
        .objects_fetched
        .load(Relaxed)
        .saturating_sub(1 /* the root leaf */);
    assert!(
        value_fetches <= 2,
        "8 references to one value caused {value_fetches} value fetches"
    );

    // And a second read of the same value hits the value cache, with no further fetch at all.
    let before = counting.objects_fetched.load(Relaxed);
    for k in &refs {
        assert_eq!(
            cold.get(root, k).await.unwrap().as_deref(),
            Some(big.as_ref())
        );
    }
    assert_eq!(
        counting.objects_fetched.load(Relaxed),
        before,
        "repeat reads of a cached external value must not refetch it"
    );
}

/// Deduplication and cache hits must not let one reference's correct length bless another reference to
/// the same id with an inconsistent authenticated length.
#[tokio::test]
async fn every_external_reference_validates_its_own_length() {
    use be_tree::BlockId;
    use be_tree::codec::{self, Entry};
    use be_tree::store::{AddressedObject, NodeStore};

    let fmt = Arc::new(Format::selected());
    let value = be_tree::value::encode(fmt.schema_id(), &vec![b'v'; 4096]);
    let entries = vec![
        Entry::external(Bytes::from_static(b"a"), stamp(1).order_key, value.id, 4096),
        Entry::external(Bytes::from_static(b"b"), stamp(1).order_key, value.id, 4097),
    ];
    let node = codec::encode_leaf(&fmt, &entries).unwrap();
    let root = BlockId::of(&node);
    let mem = Arc::new(MemStore::new());
    mem.put_batch(
        vec![
            AddressedObject {
                id: value.id,
                bytes: value.bytes,
            },
            AddressedObject {
                id: root,
                bytes: node,
            },
        ],
        (),
    )
    .await
    .unwrap();
    let tree = BeTree::with_format(mem, fmt.as_ref().clone());

    // Populate the value cache through the correct reference first.
    assert_eq!(tree.get(root, b"a").await.unwrap().unwrap().len(), 4096);
    assert!(
        tree.get(root, b"b").await.is_err(),
        "a cache hit skipped the bad length"
    );
    assert!(
        tree.get_many(root, &[b"a", b"b"]).await.is_err(),
        "deduplicating the id skipped the bad second length"
    );
}

/// Value objects need the same single-id miss coalescing as nodes. Otherwise a hot large value behind
/// one newly cold cache entry can stampede even though every node on its path coalesces correctly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[cfg_attr(miri, ignore = "native concurrent stampede fixture")]
async fn concurrent_cold_reads_of_one_external_value_do_not_stampede() {
    use be_tree::store::{AddressedObject, NodeStore};
    use be_tree::{AccessHint, BlockId, TreeError};

    struct SlowStore(Arc<MemStore>);
    impl NodeStore for SlowStore {
        type Class = ();
        async fn get(&self, id: BlockId, h: AccessHint, m: usize) -> Result<Bytes, TreeError> {
            for _ in 0..100 {
                tokio::task::yield_now().await;
            }
            self.0.get(id, h, m).await
        }
        async fn get_many(
            &self,
            ids: &[BlockId],
            h: AccessHint,
            m: usize,
            t: u64,
        ) -> Vec<Result<Bytes, TreeError>> {
            for _ in 0..100 {
                tokio::task::yield_now().await;
            }
            self.0.get_many(ids, h, m, t).await
        }
        async fn put_batch(&self, o: Vec<AddressedObject>, c: ()) -> Result<(), TreeError> {
            self.0.put_batch(o, c).await
        }
    }

    let mem = Arc::new(MemStore::new());
    let build = BeTree::new(mem.clone());
    let empty = build.empty_root().await.unwrap();
    let root = build
        .apply(
            empty,
            stamp(1),
            vec![Mutation::upsert(
                Bytes::from_static(b"k"),
                Bytes::from(vec![b'v'; 4096]),
            )],
        )
        .await
        .unwrap();

    let counting = Arc::new(CountingStore::new(Arc::new(SlowStore(mem))));
    let cold = Arc::new(BeTree::new(counting.clone()));
    let mut tasks = Vec::new();
    for _ in 0..64 {
        let tree = cold.clone();
        tasks.push(tokio::spawn(
            async move { tree.get(root, b"k").await.unwrap() },
        ));
    }
    for task in tasks {
        assert_eq!(task.await.unwrap().unwrap().len(), 4096);
    }
    let round_trips = counting.gets.load(Relaxed) + counting.get_many_calls.load(Relaxed);
    assert_eq!(
        round_trips, 2,
        "one root plus one value should be fetched once each"
    );
}

/// Overlapping multi-id waves must coordinate per id while preserving batching. This is the case a
/// single-key cache loader cannot cover: many readers concurrently request the same set of distinct
/// external values.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[cfg_attr(miri, ignore = "native 32-reader multi-value fixture")]
async fn concurrent_multi_value_waves_fetch_each_id_once() {
    use be_tree::store::{AddressedObject, NodeStore};
    use be_tree::{AccessHint, BlockId, TreeError};

    struct SlowStore(Arc<MemStore>);
    impl NodeStore for SlowStore {
        type Class = ();
        async fn get(&self, id: BlockId, h: AccessHint, m: usize) -> Result<Bytes, TreeError> {
            tokio::task::yield_now().await;
            self.0.get(id, h, m).await
        }
        async fn get_many(
            &self,
            ids: &[BlockId],
            h: AccessHint,
            m: usize,
            t: u64,
        ) -> Vec<Result<Bytes, TreeError>> {
            for _ in 0..20 {
                tokio::task::yield_now().await;
            }
            self.0.get_many(ids, h, m, t).await
        }
        async fn put_batch(&self, o: Vec<AddressedObject>, c: ()) -> Result<(), TreeError> {
            self.0.put_batch(o, c).await
        }
    }

    let mem = Arc::new(MemStore::new());
    let build = BeTree::new(mem.clone());
    let empty = build.empty_root().await.unwrap();
    let keys: Vec<Bytes> = (0..64u32)
        .map(|i| Bytes::from(format!("v/{i:03}")))
        .collect();
    let root = build
        .apply(
            empty,
            stamp(1),
            keys.iter()
                .enumerate()
                .map(|(i, key)| Mutation::upsert(key.clone(), Bytes::from(vec![i as u8; 4096])))
                .collect(),
        )
        .await
        .unwrap();
    let refs: Vec<Bytes> = keys;
    let counting = Arc::new(CountingStore::new(Arc::new(SlowStore(mem))));
    let cold = Arc::new(BeTree::new(counting.clone()));
    let mut tasks = Vec::new();
    for _ in 0..32 {
        let tree = cold.clone();
        let keys = refs.clone();
        tasks.push(tokio::spawn(async move {
            let borrowed: Vec<&[u8]> = keys.iter().map(|key| key.as_ref()).collect();
            tree.get_many(root, &borrowed).await.unwrap()
        }));
    }
    for task in tasks {
        assert_eq!(task.await.unwrap().len(), 64);
    }
    assert_eq!(
        counting.objects_fetched.load(Relaxed),
        65,
        "one leaf plus 64 distinct values should each be fetched once"
    );
    assert_eq!(counting.waves(), 2, "one node wave plus one value wave");
}

/// Concurrent cold reads of one root must collapse into a single fetch, not one per reader.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[cfg_attr(miri, ignore = "native concurrent root fixture")]
async fn concurrent_cold_reads_of_one_root_do_not_stampede() {
    use be_tree::store::{AddressedObject, NodeStore};
    use be_tree::{AccessHint, BlockId, TreeError};

    /// A store that yields before answering, widening the window in which a stampede can form.
    struct SlowStore(Arc<MemStore>);
    impl NodeStore for SlowStore {
        type Class = ();
        async fn get(&self, id: BlockId, h: AccessHint, m: usize) -> Result<Bytes, TreeError> {
            tokio::task::yield_now().await;
            self.0.get(id, h, m).await
        }
        async fn get_many(
            &self,
            ids: &[BlockId],
            h: AccessHint,
            m: usize,
            t: u64,
        ) -> Vec<Result<Bytes, TreeError>> {
            tokio::task::yield_now().await;
            self.0.get_many(ids, h, m, t).await
        }
        async fn put_batch(&self, o: Vec<AddressedObject>, c: ()) -> Result<(), TreeError> {
            self.0.put_batch(o, c).await
        }
    }

    let mem = Arc::new(MemStore::new());
    let build = BeTree::with_format(mem.clone(), Format::tiny());
    let keys = KeyShape::Ascending.keys(2_000, 1);
    let (root, _m) = harness::build(&build, &keys, 256, 8).await.unwrap();

    let counting = Arc::new(CountingStore::new(Arc::new(SlowStore(mem))));
    let cold = Arc::new(BeTree::with_format(counting.clone(), Format::tiny()));

    // 64 readers of the same cold key, therefore the same cold root.
    let mut tasks = Vec::new();
    for _ in 0..64 {
        let t = cold.clone();
        let k = keys[0].clone();
        tasks.push(tokio::spawn(async move { t.get(root, &k).await }));
    }
    for h in tasks {
        h.await.unwrap().unwrap();
    }

    let round_trips = counting.gets.load(Relaxed) + counting.get_many_calls.load(Relaxed);
    assert!(
        round_trips < 16,
        "64 concurrent cold reads of one root caused {round_trips} store round trips; \
         misses must coalesce"
    );
    println!("64 concurrent cold readers: {round_trips} store round trips");
}

/// Overlapping MULTI-ID waves must share their fetches too.
///
/// Per-key cache coalescing only helps when the miss set is a single id; two concurrent descents over the
/// same 256 cold keys each want the same *set* of nodes, and without an in-flight batch coordinator both
/// would issue the whole batch. The coordinator claims ids atomically: the first wave owns them, the
/// overlapping wave waits for those exact results, and disjoint ids still travel in one `get_many`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[cfg_attr(miri, ignore = "native overlapping-wave concurrency fixture")]
async fn overlapping_multi_id_waves_share_their_fetches() {
    use be_tree::store::{AddressedObject, NodeStore};
    use be_tree::{AccessHint, BlockId, TreeError};

    /// Yields repeatedly before answering, so every reader is in flight at once.
    struct SlowStore(Arc<MemStore>);
    impl NodeStore for SlowStore {
        type Class = ();
        async fn get(&self, id: BlockId, h: AccessHint, m: usize) -> Result<Bytes, TreeError> {
            for _ in 0..50 {
                tokio::task::yield_now().await;
            }
            self.0.get(id, h, m).await
        }
        async fn get_many(
            &self,
            ids: &[BlockId],
            h: AccessHint,
            m: usize,
            t: u64,
        ) -> Vec<Result<Bytes, TreeError>> {
            for _ in 0..50 {
                tokio::task::yield_now().await;
            }
            self.0.get_many(ids, h, m, t).await
        }
        async fn put_batch(&self, o: Vec<AddressedObject>, c: ()) -> Result<(), TreeError> {
            self.0.put_batch(o, c).await
        }
    }

    let mem = Arc::new(MemStore::new());
    let build = BeTree::with_format(mem.clone(), Format::tiny());
    let keys = KeyShape::Ascending.keys(4_000, 1);
    let (root, _m) = harness::build(&build, &keys, 256, 8).await.unwrap();

    // One cold reader, to establish what a single descent costs.
    let solo_counter = Arc::new(CountingStore::new(Arc::new(SlowStore(mem.clone()))));
    let solo = BeTree::with_format(solo_counter.clone(), Format::tiny());
    let probe: Vec<&[u8]> = keys.iter().step_by(16).map(|k| k.as_ref()).collect();
    solo.get_many(root, &probe).await.unwrap();
    let solo_objects = solo_counter.objects_fetched.load(Relaxed);
    assert!(
        solo_objects > 8,
        "the probe must miss on many nodes, got {solo_objects}"
    );

    // Now eight concurrent readers of the SAME key set against a cold tree.
    let counting = Arc::new(CountingStore::new(Arc::new(SlowStore(mem))));
    let cold = Arc::new(BeTree::with_format(counting.clone(), Format::tiny()));
    let owned: Vec<Bytes> = keys.iter().step_by(16).cloned().collect();
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let t = cold.clone();
        let ks = owned.clone();
        tasks.push(tokio::spawn(async move {
            let refs: Vec<&[u8]> = ks.iter().map(|k| k.as_ref()).collect();
            t.get_many(root, &refs).await.map(|v| v.len())
        }));
    }
    for h in tasks {
        assert_eq!(h.await.unwrap().unwrap(), owned.len());
    }

    // Eight readers must not fetch eight times what one reader fetched.
    let objects = counting.objects_fetched.load(Relaxed);
    assert!(
        objects < solo_objects * 2,
        "8 concurrent readers fetched {objects} objects where one fetched {solo_objects}; \
         overlapping waves are not sharing"
    );
    println!("8 overlapping multi-id waves: {objects} objects vs {solo_objects} for one reader");
}

/// Cancelling the task that owns an in-flight batch must remove its claims and wake waiters. Without
/// the owner guard, every later reader of those ids waits forever on flights nobody can complete.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg_attr(miri, ignore = "native cancellation scheduling fixture")]
async fn cancelling_a_wave_owner_does_not_poison_later_reads() {
    use be_tree::store::{AddressedObject, NodeStore};
    use be_tree::{AccessHint, BlockId, TreeError};
    use std::sync::atomic::AtomicBool;

    struct CancelOnceStore {
        inner: Arc<MemStore>,
        block_once: AtomicBool,
        started: AtomicBool,
    }
    impl NodeStore for CancelOnceStore {
        type Class = ();
        async fn get(&self, id: BlockId, h: AccessHint, m: usize) -> Result<Bytes, TreeError> {
            self.inner.get(id, h, m).await
        }
        async fn get_many(
            &self,
            ids: &[BlockId],
            h: AccessHint,
            m: usize,
            t: u64,
        ) -> Vec<Result<Bytes, TreeError>> {
            if self.block_once.swap(false, Relaxed) {
                self.started.store(true, Relaxed);
                std::future::pending::<()>().await;
            }
            self.inner.get_many(ids, h, m, t).await
        }
        async fn put_batch(&self, o: Vec<AddressedObject>, c: ()) -> Result<(), TreeError> {
            self.inner.put_batch(o, c).await
        }
    }

    let mem = Arc::new(MemStore::new());
    let build = BeTree::with_format(mem.clone(), Format::tiny());
    let keys = KeyShape::Ascending.keys(4_000, 1);
    let (root, _) = harness::build(&build, &keys, 256, 8).await.unwrap();
    let store = Arc::new(CancelOnceStore {
        inner: mem,
        block_once: AtomicBool::new(true),
        started: AtomicBool::new(false),
    });
    let tree = Arc::new(BeTree::with_format(store.clone(), Format::tiny()));
    let owned: Vec<Bytes> = keys.iter().step_by(16).cloned().collect();
    let first_tree = tree.clone();
    let first_keys = owned.clone();
    let first = tokio::spawn(async move {
        let refs: Vec<&[u8]> = first_keys.iter().map(|key| key.as_ref()).collect();
        first_tree.get_many(root, &refs).await
    });
    while !store.started.load(Relaxed) {
        tokio::task::yield_now().await;
    }
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());

    let refs: Vec<&[u8]> = owned.iter().map(|key| key.as_ref()).collect();
    assert_eq!(tree.get_many(root, &refs).await.unwrap().len(), owned.len());
}

/// Cache sizing is a policy the host owns, so a zero budget must still be correct.
#[tokio::test]
#[cfg_attr(miri, ignore = "native 2k-key disabled-cache fixture")]
async fn a_disabled_cache_is_still_correct() {
    let mem = Arc::new(MemStore::new());
    let build = BeTree::with_format(mem.clone(), Format::tiny());
    let keys = KeyShape::Ascending.keys(2_000, 1);
    let (root, model) = harness::build(&build, &keys, 64, 8).await.unwrap();

    let t = BeTree::with_format(mem, Format::tiny()).with_caches(CacheConfig::NONE);
    assert_eq!(
        t.scan_range(root, None, None).await.unwrap().len(),
        model.live_len()
    );
    assert!(t.get(root, &keys[7]).await.unwrap().is_some());
}
