use std::collections::HashSet;
use std::hint::black_box;
use std::sync::Arc;

use cbe_tree::codec::{self, NodeView};
use cbe_tree::{BeTree, BlockId, Format, MemStore, Mutation, ObjectKind, VersionStamp};
use bytes::Bytes;

pub const KEYS: usize = 10_000;
pub const WIDTH: usize = 256;

pub struct Fixture {
    pub tree: BeTree<MemStore>,
    pub store: Arc<MemStore>,
    pub root: BlockId,
    pub keys: Vec<Bytes>,
}

pub fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("profiling runtime")
}

pub async fn fixture() -> Fixture {
    fixture_with_prefix("profile/key/").await
}

pub async fn fixture_with_prefix(prefix: &str) -> Fixture {
    let store = Arc::new(MemStore::new());
    let tree = BeTree::new(store.clone());
    let keys: Vec<Bytes> = (0..KEYS)
        .map(|i| Bytes::from(format!("{prefix}{i:08}")))
        .collect();
    let mut root = tree.empty_root().await.expect("empty root");
    for (stamp, chunk) in (1u64..).zip(keys.chunks(WIDTH)) {
        let mutations = chunk
            .iter()
            .map(|key| Mutation::upsert(key.clone(), Bytes::from_static(b"profile-value-24-bytes")))
            .collect();
        root = tree
            .apply(root, VersionStamp::from_counter(stamp), mutations)
            .await
            .expect("build profiling fixture");
    }
    Fixture {
        tree,
        store,
        root,
        keys,
    }
}

#[allow(dead_code)]
pub async fn point_get(fixture: &Fixture, iterations: usize) -> u64 {
    let key = fixture.keys[fixture.keys.len() / 2].clone();
    let mut checksum = 0u64;
    for _ in 0..iterations {
        let value = fixture
            .tree
            .get(fixture.root, &key)
            .await
            .expect("profile get");
        checksum = checksum.wrapping_add(value.as_ref().map_or(0, Bytes::len) as u64);
        black_box(&value);
    }
    checksum
}

/// Point reads over DISTINCT keys strided across the whole keyspace, for the cold-cache shapes.
/// Rereading one key would warm the decoded cache after the first call and report a 99%-hot number
/// as "cold"; the stride puts successive probes on different leaves, so keep `iterations` at or
/// below the fixture's leaf count for every measured leaf load to be a genuine miss.
#[allow(dead_code)]
pub async fn point_get_distinct(fixture: &Fixture, iterations: usize) -> u64 {
    let stride = (fixture.keys.len() / iterations.max(1)).max(1);
    let mut checksum = 0u64;
    for i in 0..iterations {
        let key = &fixture.keys[(i * stride) % fixture.keys.len()];
        let value = fixture
            .tree
            .get(fixture.root, key)
            .await
            .expect("profile distinct get");
        checksum = checksum.wrapping_add(value.as_ref().map_or(0, Bytes::len) as u64);
        black_box(&value);
    }
    checksum
}

#[allow(dead_code)]
pub async fn point_fixture(long_prefix: bool, warm_cache: bool) -> u64 {
    let fixture = if long_prefix {
        fixture_with_prefix("profile/long-common-prefix/with-many-shared-bytes/").await
    } else {
        fixture().await
    };
    if warm_cache {
        warm(&fixture).await;
    }
    black_box(fixture.root.0[0] as u64)
}

/// A live corpus with a contiguous tombstone-dense interval. The interval is deliberately retained in
/// the tree rather than removed from the fixture so scans exercise winner resolution and tombstone
/// filtering over persisted delete records.
pub async fn tombstone_fixture() -> Fixture {
    let fixture = fixture().await;
    let start = KEYS / 2 - 1_000;
    let mutations = fixture.keys[start..start + 2_000]
        .iter()
        .cloned()
        .map(Mutation::tombstone)
        .collect();
    let root = fixture
        .tree
        .apply(
            fixture.root,
            VersionStamp::from_counter(2_000_000),
            mutations,
        )
        .await
        .expect("build tombstone profiling fixture");
    Fixture { root, ..fixture }
}

pub fn probes(keys: &[Bytes]) -> Vec<&[u8]> {
    keys.iter()
        .step_by((keys.len() / WIDTH).max(1))
        .take(WIDTH)
        .map(Bytes::as_ref)
        .collect()
}

/// Build one deterministic batched-read workload. Query construction happens before the measured
/// operation loop, so the profile describes tree traversal and result materialization rather than
/// repeatedly formatting synthetic keys. The returned `Bytes` owns misses and cloned hits alike,
/// which keeps every shape's lifetime and ordering rules identical.
pub fn get_many_queries(fixture: &Fixture, name: &str) -> Vec<Bytes> {
    let mut parts = name.split('-');
    let width: usize = parts
        .next()
        .and_then(|value| value.parse().ok())
        .expect("get-many shape width");
    let order = parts.next().expect("get-many shape order");
    let outcome = parts.next().expect("get-many shape outcome");
    assert!(parts.next().is_none(), "unexpected get-many shape suffix");
    assert!(matches!(order, "sorted" | "random"));
    assert!(matches!(outcome, "hits" | "misses" | "mixed"));
    let width = width.min(fixture.keys.len());
    let mut queries = Vec::with_capacity(width);
    for i in 0..width {
        let source = match order {
            "sorted" => i,
            "random" => (i * 7_919 + 1_013) % fixture.keys.len(),
            _ => unreachable!(),
        };
        let hit = Bytes::copy_from_slice(&fixture.keys[source]);
        let query = match outcome {
            "hits" => hit,
            "misses" => Bytes::from(format!("profile/miss/{order}/{i:08}")),
            "mixed" if i % 2 == 0 => hit,
            "mixed" => Bytes::from(format!("profile/miss/{order}/{i:08}")),
            _ => unreachable!(),
        };
        queries.push(query);
    }
    queries
}

pub async fn get_many_prepared(fixture: &Fixture, queries: &[Bytes], iterations: usize) -> u64 {
    let refs: Vec<&[u8]> = queries.iter().map(Bytes::as_ref).collect();
    let mut checksum = 0u64;
    for _ in 0..iterations {
        let values = fixture
            .tree
            .get_many(fixture.root, &refs)
            .await
            .expect("profile get_many");
        checksum =
            checksum.wrapping_add(values.iter().flatten().map(Bytes::len).sum::<usize>() as u64);
        black_box(&values);
    }
    checksum
}

pub async fn warm(fixture: &Fixture) {
    let keys = probes(&fixture.keys);
    black_box(
        fixture
            .tree
            .get_many(fixture.root, &keys)
            .await
            .expect("warm probes"),
    );
}

pub async fn get_many(fixture: &Fixture, iterations: usize) -> u64 {
    let keys = probes(&fixture.keys);
    let mut checksum = 0u64;
    for _ in 0..iterations {
        let values = fixture
            .tree
            .get_many(fixture.root, &keys)
            .await
            .expect("profile get_many");
        checksum =
            checksum.wrapping_add(values.iter().flatten().map(Bytes::len).sum::<usize>() as u64);
        black_box(&values);
    }
    checksum
}

pub async fn scan(fixture: &Fixture, iterations: usize) -> u64 {
    let mut checksum = 0u64;
    for _ in 0..iterations {
        let rows = fixture
            .tree
            .scan_range(fixture.root, None, None)
            .await
            .expect("profile scan");
        checksum = checksum.wrapping_add(rows.len() as u64);
        black_box(&rows);
    }
    checksum
}

pub async fn scan_stream(fixture: &Fixture, iterations: usize) -> u64 {
    let mut checksum = 0u64;
    for _ in 0..iterations {
        let mut cursor = fixture.tree.scan_cursor(fixture.root, None, None);
        loop {
            let rows = cursor.next_batch(WIDTH).await.expect("profile scan cursor");
            if rows.is_empty() {
                break;
            }
            checksum = checksum.wrapping_add(rows.len() as u64);
            black_box(&rows);
        }
    }
    checksum
}

pub async fn scan_tombstones(fixture: &Fixture, iterations: usize) -> u64 {
    let lo = fixture.keys[KEYS / 2 - 1_500].as_ref();
    let hi = fixture.keys[KEYS / 2 + 1_500].as_ref();
    let mut checksum = 0u64;
    for _ in 0..iterations {
        let rows = fixture
            .tree
            .scan_range(fixture.root, Some(lo), Some(hi))
            .await
            .expect("profile tombstone scan");
        checksum = checksum.wrapping_add(rows.len() as u64);
        black_box(&rows);
    }
    checksum
}

pub async fn scan_stream_tombstones(fixture: &Fixture, iterations: usize) -> u64 {
    let lo = fixture.keys[KEYS / 2 - 1_500].as_ref();
    let hi = fixture.keys[KEYS / 2 + 1_500].as_ref();
    let mut checksum = 0u64;
    for _ in 0..iterations {
        let mut cursor = fixture.tree.scan_cursor(fixture.root, Some(lo), Some(hi));
        loop {
            let rows = cursor
                .next_batch(WIDTH)
                .await
                .expect("profile tombstone scan cursor");
            if rows.is_empty() {
                break;
            }
            checksum = checksum.wrapping_add(rows.len() as u64);
            black_box(&rows);
        }
    }
    checksum
}

#[allow(dead_code)]
pub async fn scan_rows(fixture: &Fixture, rows: usize, iterations: usize) -> u64 {
    assert!(rows > 0 && rows < fixture.keys.len());
    let lo = fixture.keys[0].as_ref();
    let hi = fixture.keys[rows].as_ref();
    let mut checksum = 0u64;
    for _ in 0..iterations {
        let result = fixture
            .tree
            .scan_range(fixture.root, Some(lo), Some(hi))
            .await
            .expect("profile bounded scan");
        checksum = checksum.wrapping_add(result.len() as u64);
        black_box(&result);
    }
    checksum
}

#[allow(dead_code)]
pub async fn scan_stream_rows(fixture: &Fixture, rows: usize, iterations: usize) -> u64 {
    assert!(rows > 0 && rows < fixture.keys.len());
    let lo = fixture.keys[0].as_ref();
    let hi = fixture.keys[rows].as_ref();
    let mut checksum = 0u64;
    for _ in 0..iterations {
        let mut cursor = fixture.tree.scan_cursor(fixture.root, Some(lo), Some(hi));
        loop {
            let result = cursor
                .next_batch(WIDTH)
                .await
                .expect("profile bounded cursor");
            if result.is_empty() {
                break;
            }
            checksum = checksum.wrapping_add(result.len() as u64);
            black_box(&result);
        }
    }
    checksum
}

pub async fn apply(fixture: &Fixture, iterations: usize) -> u64 {
    let mut root = fixture.root;
    for i in 0..iterations {
        let key = fixture.keys[(i * 977) % fixture.keys.len()].clone();
        root = fixture
            .tree
            .apply(
                root,
                VersionStamp::from_counter(1_000_000 + i as u64),
                vec![Mutation::upsert(
                    key,
                    Bytes::from_static(b"updated-profile-value"),
                )],
            )
            .await
            .expect("profile apply");
    }
    black_box(root.0[0] as u64)
}

pub async fn apply_named(fixture: &Fixture, iterations: usize, name: &str) -> u64 {
    let mut parts = name.split('-');
    let width: usize = parts
        .next()
        .and_then(|value| value.parse().ok())
        .expect("apply shape width");
    let mode = parts.next().expect("apply shape mode");
    assert!(parts.next().is_none(), "unexpected apply shape suffix");
    assert!(matches!(mode, "repeated" | "distinct" | "delete" | "mixed"));
    let width = width.min(fixture.keys.len());
    let mut root = fixture.root;
    for i in 0..iterations {
        let mutations = (0..width)
            .map(|j| {
                let index = if mode == "repeated" {
                    (i * 977) % fixture.keys.len()
                } else {
                    (i * 977 + j * 1_009) % fixture.keys.len()
                };
                let key = fixture.keys[index].clone();
                // "mixed" interleaves upserts and tombstones in ONE batch: winner resolution over a
                // batch that both writes and deletes is its own regression surface.
                if mode == "delete" || (mode == "mixed" && j % 2 == 1) {
                    Mutation::tombstone(key)
                } else {
                    Mutation::upsert(key, Bytes::from_static(b"updated-profile-value"))
                }
            })
            .collect();
        root = fixture
            .tree
            .apply(
                root,
                VersionStamp::from_counter(3_000_000 + i as u64),
                mutations,
            )
            .await
            .expect("profile named apply");
    }
    black_box(root.0[0] as u64)
}

/// Build the chained COW mutation batches OUTSIDE the measured region. Each commit is `WIDTH`
/// mutations — commit-sized, so every apply flushes buffered messages and rewrites leaf paths;
/// a single buffered mutation would be absorbed by the root and make low and high overlap
/// indistinguishable one-node rewrites.
///
/// `overlap == true` rewrites existing keys spread across the whole keyspace (high structural
/// overlap with the original tree). `overlap == false` inserts fresh keys that each sort
/// immediately after a different existing key, so new entries spread across every leaf instead of
/// piling into one new prefix subtree.
#[allow(dead_code)]
pub fn cow_batches(fixture: &Fixture, overlap: bool, commits: usize) -> Vec<Vec<Mutation>> {
    (0..commits)
        .map(|i| {
            (0..WIDTH)
                .map(|j| {
                    // 1,009 is coprime to the fixture size, so one commit's slots are distinct.
                    let slot = (i * WIDTH + j * 1_009) % fixture.keys.len();
                    let key = if overlap {
                        fixture.keys[slot].clone()
                    } else {
                        let anchor = String::from_utf8_lossy(&fixture.keys[slot]);
                        Bytes::from(format!("{anchor}x{i:04}"))
                    };
                    Mutation::upsert(key, Bytes::from_static(b"cow-profile-value"))
                })
                .collect()
        })
        .collect()
}

/// Apply prebuilt COW batches, chaining every returned root and recording each in `roots` (both
/// preallocated by the caller). The measured region is therefore rewrite work only: no key
/// formatting, no reachability walks, no metric collection.
#[allow(dead_code)]
pub async fn cow_apply(
    fixture: &Fixture,
    batches: &mut Vec<Vec<Mutation>>,
    roots: &mut Vec<BlockId>,
) -> u64 {
    let mut root = fixture.root;
    for (i, batch) in batches.drain(..).enumerate() {
        root = fixture
            .tree
            .apply(
                root,
                VersionStamp::from_counter(4_000_000 + i as u64),
                batch,
            )
            .await
            .expect("profile COW apply");
        roots.push(root);
    }
    black_box(root.0[0] as u64)
}

/// Storage and sharing counters for a completed COW run. Called OUTSIDE any measured region: the
/// reachability walks below are metrics collection, not rewrite work, and must not be attributed
/// to the operation under measurement.
#[allow(dead_code)]
pub async fn cow_metrics_line(
    fixture: &Fixture,
    overlap: bool,
    initial_ids: &HashSet<BlockId>,
    roots: &[BlockId],
) {
    let final_ids: HashSet<_> = fixture.store.ids().into_iter().collect();
    let new_objects: Vec<_> = final_ids.difference(initial_ids).copied().collect();
    let bytes_written: usize = new_objects
        .iter()
        .filter_map(|id| fixture.store.raw(*id))
        .map(|bytes| bytes.len())
        .sum();
    let original = reachable_nodes(fixture, fixture.root).await;
    let mut previous = original.clone();
    let mut shared_nodes = 0usize;
    let mut resulting_nodes = 0usize;
    let mut last = HashSet::new();
    for root in roots {
        let nodes = reachable_nodes(fixture, *root).await;
        shared_nodes += previous.intersection(&nodes).count();
        resulting_nodes += nodes.len();
        previous = nodes.clone();
        last = nodes;
    }
    // Two distinct rates: commit-to-commit sharing (how much one rewrite preserves of its immediate
    // predecessor) and final sharing with the ORIGINAL tree (the cumulative overlap axis that
    // separates the low and high shapes).
    let successive = if resulting_nodes == 0 {
        0.0
    } else {
        shared_nodes as f64 / resulting_nodes as f64
    };
    let with_original = if last.is_empty() {
        0.0
    } else {
        original.intersection(&last).count() as f64 / last.len() as f64
    };
    println!(
        "cow_metrics overlap={} commits={} new_objects={} bytes_written={} successive_sharing_rate={successive:.6} final_sharing_with_original={with_original:.6}",
        if overlap { "high" } else { "low" },
        roots.len(),
        new_objects.len(),
        bytes_written,
    );
}

async fn reachable_nodes(fixture: &Fixture, root: BlockId) -> HashSet<BlockId> {
    let mut seen = HashSet::from([root]);
    let mut frontier = vec![root];
    while !frontier.is_empty() {
        let mut next = Vec::new();
        for id in frontier {
            for (kind, child) in fixture
                .tree
                .references(id)
                .await
                .expect("profile references")
            {
                if matches!(kind, ObjectKind::Node) && seen.insert(child) {
                    next.push(child);
                }
            }
        }
        frontier = next;
    }
    seen
}

pub fn hash_bytes() -> Vec<u8> {
    vec![0x5a; Format::selected().node_bytes()]
}

pub fn hash(bytes: &[u8], iterations: usize) -> u64 {
    let mut checksum = 0u64;
    for _ in 0..iterations {
        checksum = checksum.wrapping_add(black_box(BlockId::of(black_box(bytes))).0[0] as u64);
    }
    checksum
}

pub fn encoded_node() -> (Arc<Format>, Bytes) {
    let format = Arc::new(Format::selected());
    let bytes = codec::encode_leaf(&format, &[]).expect("encode profiling node");
    (format, bytes)
}

pub fn decode(format: &Arc<Format>, bytes: &Bytes, iterations: usize) -> u64 {
    let mut checksum = 0u64;
    for _ in 0..iterations {
        let view =
            NodeView::decode(format, None, black_box(bytes.clone())).expect("profile decode");
        checksum = checksum.wrapping_add(black_box(view.bytes().len()) as u64);
    }
    checksum
}
