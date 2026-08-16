//! The workload list: point reads (hit and miss), 256-key `get_many`, 64-prefix
//! `scan_prefix_many`, one-mutation and 256-mutation `apply`, `diff` at fixed logical divergence, and
//! read waves with verification on and off.
//!
//! Criterion runs against `MemStore`, so these measure **compute**. The dependent-wave and byte counts
//! come from [`CountingStore`] and are printed separately; `MemStore` timings are never presented as
//! storage-system I/O throughput, and no Bε asymptotic bound is claimed from them.

#[path = "../tests/support/mod.rs"]
mod support;

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::Ordering::Relaxed;

use be_tree::format::Format;
use be_tree::store::{AddressedObject, MemStore, NodeStore};
use be_tree::tree::{CacheConfig, VerifyPolicy};
use be_tree::{AccessHint, BeTree, BlockId, Mutation, TreeError, VersionStamp};
use bytes::Bytes;
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use support::{self as harness, CountingStore, KeyShape};

const KEYS: usize = 100_000;
const VALUE: usize = 24;

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime")
}

struct Fixture {
    store: Arc<MemStore>,
    tree: BeTree<MemStore>,
    root: BlockId,
    keys: Vec<Bytes>,
}

/// Per-sample writable overlay over one immutable corpus. Every write sample therefore starts at the
/// same tree and cache state, without copying the 100k-key fixture or letting earlier samples grow the
/// store underneath later ones.
struct OverlayStore {
    base: Arc<MemStore>,
    writes: Mutex<std::collections::HashMap<BlockId, Bytes>>,
}

impl OverlayStore {
    fn new(base: Arc<MemStore>) -> Self {
        Self {
            base,
            writes: Mutex::new(Default::default()),
        }
    }
}

impl NodeStore for OverlayStore {
    type Class = ();

    async fn get(&self, id: BlockId, hint: AccessHint, max: usize) -> Result<Bytes, TreeError> {
        if let Some(bytes) = self.writes.lock().expect("overlay").get(&id).cloned() {
            if bytes.len() > max {
                return Err(TreeError::Store(
                    "overlay object exceeds caller limit".into(),
                ));
            }
            return Ok(bytes);
        }
        self.base.get(id, hint, max).await
    }

    async fn get_many(
        &self,
        ids: &[BlockId],
        hint: AccessHint,
        max: usize,
        total: u64,
    ) -> Vec<Result<Bytes, TreeError>> {
        let mut used = 0u64;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let result = self.get(*id, hint, max).await.and_then(|bytes| {
                used = used.saturating_add(bytes.len() as u64);
                if used > total {
                    Err(TreeError::ResourceLimit {
                        what: "bytes fetched",
                    })
                } else {
                    Ok(bytes)
                }
            });
            out.push(result);
        }
        out
    }

    async fn put_batch(&self, objects: Vec<AddressedObject>, _class: ()) -> Result<(), TreeError> {
        let mut writes = self.writes.lock().expect("overlay");
        for object in objects {
            let actual = BlockId::of(&object.bytes);
            if actual != object.id {
                return Err(TreeError::HashMismatch {
                    requested: object.id,
                    actual,
                });
            }
            writes.insert(object.id, object.bytes);
        }
        Ok(())
    }
}

fn fixture(rt: &tokio::runtime::Runtime, shape: KeyShape) -> Fixture {
    let store = Arc::new(MemStore::new());
    let fmt = Format::selected();
    let tree = BeTree::with_format(store.clone(), fmt);
    let keys = shape.keys(KEYS, 2024);
    let (root, _model) = rt
        .block_on(harness::build(&tree, &keys, 256, VALUE))
        .expect("build");
    Fixture {
        store,
        tree,
        root,
        keys,
    }
}

fn reads(c: &mut Criterion) {
    let rt = rt();
    for shape in [KeyShape::UniformRandom, KeyShape::LongSharedPrefix] {
        let f = fixture(&rt, shape);
        let hit: Vec<&[u8]> = vec![f.keys[KEYS / 3].as_ref()];
        let miss_key = Bytes::from_static(b"\xff\xff\xff\xff-definitely-absent");
        let miss: Vec<&[u8]> = vec![miss_key.as_ref()];
        let batch: Vec<&[u8]> = f
            .keys
            .iter()
            .step_by(KEYS / 256)
            .map(|k| k.as_ref())
            .collect();

        let mut g = c.benchmark_group(format!("workload/{}", shape.name()));
        g.bench_function("point/hit", |b| {
            b.iter(|| rt.block_on(f.tree.get_many(f.root, &hit)).unwrap())
        });
        g.bench_function("point/miss", |b| {
            b.iter(|| rt.block_on(f.tree.get_many(f.root, &miss)).unwrap())
        });
        g.bench_function("get_many/256", |b| {
            b.iter(|| rt.block_on(f.tree.get_many(f.root, &batch)).unwrap())
        });

        // Prefix scans: 64 prefixes in one shared walk. The prefix must actually DISCRIMINATE, or the
        // benchmark measures materializing 64 copies of the whole corpus instead of the walk. For a
        // long-shared-prefix key set that means slicing past the shared part.
        let plen = match shape {
            KeyShape::LongSharedPrefix => 60,
            _ => 6,
        };
        let prefixes: Vec<Bytes> = f
            .keys
            .iter()
            .step_by(KEYS / 64)
            .map(|k| Bytes::copy_from_slice(&k[..k.len().min(plen)]))
            .collect();
        let prefix_refs: Vec<&[u8]> = prefixes.iter().map(|p| p.as_ref()).collect();
        g.bench_function("scan_prefix_many/64", |b| {
            b.iter(|| {
                rt.block_on(f.tree.scan_prefix_many(f.root, &prefix_refs))
                    .unwrap()
            })
        });

        // The degenerate case, measured on purpose: 64 prefixes that are all equal and match everything.
        // Its cost is the ANSWER (64 x corpus pairs), not the walk; a batched API cannot make that
        // cheaper, and pretending otherwise would hide a regression elsewhere.
        let broad = Bytes::copy_from_slice(&f.keys[0][..f.keys[0].len().min(1)]);
        let broad_refs: Vec<&[u8]> = vec![broad.as_ref(); 8];
        g.bench_function("scan_prefix_many/8_identical_broad", |b| {
            b.iter(|| {
                rt.block_on(f.tree.scan_prefix_many(f.root, &broad_refs))
                    .unwrap()
            })
        });
        g.bench_function("scan_range/window", |b| {
            b.iter(|| {
                rt.block_on(f.tree.scan_range(
                    f.root,
                    Some(f.keys[1000].as_ref()),
                    Some(f.keys[1256].as_ref()),
                ))
                .unwrap()
            })
        });
        g.finish();
    }
}

/// Writes, measured as a bounded **evolving chain**: each Criterion iteration starts from the same
/// fixture root and performs 32 commits, with each commit based on the preceding result.
///
/// The earlier version of this benchmark repeatedly mutated one fixed root and discarded the returned
/// one. That measures a warm, non-evolving snapshot: it never pays for the parent chain a real commit
/// walks, and it structurally cannot observe a commit refetching the node its predecessor just wrote.
/// Letting one root evolve across Criterion samples made the benchmark non-stationary: flush phases and
/// growing state produced confidence intervals too wide to support a median. Resetting at a fixed chain
/// boundary makes every sample comparable while retaining the parent-child behavior under test. A
/// fixed-root batch of the same 32 commits is reported beside it for contrast.
fn writes(c: &mut Criterion) {
    const CHAIN: usize = 32;
    let rt = rt();
    let f = fixture(&rt, KeyShape::UniformRandom);
    let mut g = c.benchmark_group("workload/apply");
    g.throughput(Throughput::Elements(CHAIN as u64));

    for width in [1usize, 256] {
        g.bench_function(format!("evolving_chain_{CHAIN}/{width}"), |b| {
            b.iter_custom(|iterations| {
                let mut measured = std::time::Duration::ZERO;
                for _ in 0..iterations {
                    let tree = BeTree::with_format(
                        Arc::new(OverlayStore::new(f.store.clone())),
                        Format::selected(),
                    );
                    // The chain begins warm, like a writer continuing from the root it already owns.
                    rt.block_on(tree.get(f.root, &f.keys[0])).unwrap();
                    let started = std::time::Instant::now();
                    let mut root = f.root;
                    for step in 0..CHAIN {
                        let n = 1_000_000 + step as u64;
                        let muts: Vec<Mutation> = (0..width)
                            .map(|i| {
                                Mutation::upsert(
                                    f.keys[(n as usize * 257 + i) % KEYS].clone(),
                                    Bytes::from_static(b"updated-value"),
                                )
                            })
                            .collect();
                        root = rt
                            .block_on(tree.apply(root, VersionStamp::from_counter(n), muts))
                            .unwrap();
                    }
                    std::hint::black_box(root);
                    measured += started.elapsed();
                }
                measured
            })
        });

        g.bench_function(format!("fixed_root_batch_{CHAIN}/{width}"), |b| {
            b.iter_custom(|iterations| {
                let mut measured = std::time::Duration::ZERO;
                for _ in 0..iterations {
                    let tree = BeTree::with_format(
                        Arc::new(OverlayStore::new(f.store.clone())),
                        Format::selected(),
                    );
                    rt.block_on(tree.get(f.root, &f.keys[0])).unwrap();
                    let started = std::time::Instant::now();
                    let mut last = f.root;
                    for step in 0..CHAIN {
                        let n = 2_000_000 + step as u64;
                        let muts: Vec<Mutation> = (0..width)
                            .map(|i| {
                                Mutation::upsert(
                                    f.keys[(n as usize * 257 + i) % KEYS].clone(),
                                    Bytes::from_static(b"updated-value"),
                                )
                            })
                            .collect();
                        last = rt
                            .block_on(tree.apply(f.root, VersionStamp::from_counter(n), muts))
                            .unwrap();
                    }
                    std::hint::black_box(last);
                    measured += started.elapsed();
                }
                measured
            })
        });
    }

    g.throughput(Throughput::Elements(1));
    g.bench_function("empty", |b| {
        b.iter(|| {
            rt.block_on(
                f.tree
                    .apply(f.root, VersionStamp::from_counter(1), Vec::new()),
            )
            .unwrap()
        })
    });
    g.finish();
}

/// Store interaction for an evolving chain on the tree that performed the build. Every committed node
/// should already be warm; the dedicated cold-handle tests separately cover fetch behavior.
fn report_evolving_writes(_c: &mut Criterion) {
    let rt = rt();
    println!("\n=== evolving commit chain after build (CountingStore, selected format) ===");
    let fmt = Format::selected();
    let mem = Arc::new(MemStore::new());
    let counting = Arc::new(CountingStore::new(mem));
    let t = BeTree::with_format(counting.clone(), fmt).record_metrics();
    let keys = KeyShape::UniformRandom.keys(KEYS, 2024);
    let (mut root, _m) = rt
        .block_on(harness::build(&t, &keys, 256, VALUE))
        .expect("build");

    for width in [1usize, 256] {
        let before_reads = counting.objects_fetched.load(Relaxed);
        let before_puts = counting.objects_put.load(Relaxed);
        let before_bytes = counting.bytes_put.load(Relaxed);
        const COMMITS: usize = 50;
        for i in 0..COMMITS {
            let muts: Vec<Mutation> = (0..width)
                .map(|j| {
                    Mutation::upsert(
                        keys[(i * 977 + j) % KEYS].clone(),
                        Bytes::from_static(b"updated"),
                    )
                })
                .collect();
            root = rt
                .block_on(t.apply(root, VersionStamp::from_counter(3_000_000 + i as u64), muts))
                .unwrap();
        }
        println!(
            "width {width:>3}: {COMMITS} commits -> objects_read={} objects_put={} bytes_put={} \
             (per commit: {:.1} read, {:.1} put)",
            counting.objects_fetched.load(Relaxed) - before_reads,
            counting.objects_put.load(Relaxed) - before_puts,
            counting.bytes_put.load(Relaxed) - before_bytes,
            (counting.objects_fetched.load(Relaxed) - before_reads) as f64 / COMMITS as f64,
            (counting.objects_put.load(Relaxed) - before_puts) as f64 / COMMITS as f64,
        );
    }
    println!(
        "  cache_warmed={} duplicates_elided={}",
        t.metrics().cache_warmed,
        t.metrics().duplicates_elided,
    );
}

fn diffs(c: &mut Criterion) {
    let rt = rt();
    let f = fixture(&rt, KeyShape::UniformRandom);
    let mut g = c.benchmark_group("workload/diff");
    for divergence in [1usize, 16, 256, 4096] {
        let muts: Vec<Mutation> = (0..divergence)
            .map(|i| {
                Mutation::upsert(
                    f.keys[(i * 7919) % KEYS].clone(),
                    Bytes::from_static(b"diverged"),
                )
            })
            .collect();
        let other = rt
            .block_on(
                f.tree
                    .apply(f.root, VersionStamp::from_counter(9_000_000), muts),
            )
            .unwrap();
        g.bench_function(format!("divergence/{divergence}"), |b| {
            b.iter(|| rt.block_on(f.tree.diff(f.root, other)).unwrap())
        });
    }
    g.finish();
}

fn verification(c: &mut Criterion) {
    let rt = rt();
    let f = fixture(&rt, KeyShape::UniformRandom);
    let fmt = Format::selected();
    let batch: Vec<&[u8]> = f
        .keys
        .iter()
        .step_by(KEYS / 256)
        .map(|k| k.as_ref())
        .collect();
    let mut g = c.benchmark_group("workload/verify");
    for policy in [VerifyPolicy::Always, VerifyPolicy::Never] {
        let name = if policy == VerifyPolicy::Always {
            "always"
        } else {
            "never"
        };
        // Cold every iteration: verification only costs anything on a cache miss.
        g.bench_function(format!("read_wave/{name}"), |b| {
            b.iter(|| {
                let t = BeTree::with_format(f.store.clone(), fmt.clone()).with_verify(policy);
                rt.block_on(t.get_many(f.root, &batch)).unwrap()
            })
        });
    }
    g.finish();
}

/// Contention characterization, not a single-writer proxy: independent branches apply concurrently
/// against one store/cache, and a mixed group runs point-read batches beside writes. The root dependency
/// of one branch is intentionally not parallelized; that is measured by `evolving_chain_32`.
async fn concurrent_sample(
    tree: Arc<BeTree<OverlayStore>>,
    root: BlockId,
    keys: Arc<Vec<Bytes>>,
    tasks: usize,
    mixed: bool,
    write_width: usize,
) {
    let mut set = tokio::task::JoinSet::new();
    for task in 0..tasks {
        let tree = tree.clone();
        let keys = keys.clone();
        set.spawn(async move {
            if mixed && task % 4 != 0 {
                let owned: Vec<Bytes> = (0..64usize)
                    .map(|i| keys[(task * 8191 + i * 257) % KEYS].clone())
                    .collect();
                let refs: Vec<&[u8]> = owned.iter().map(|key| key.as_ref()).collect();
                tree.get_many(root, &refs).await.unwrap();
            } else {
                let width = if mixed { 16 } else { write_width };
                let muts = (0..width)
                    .map(|i| {
                        Mutation::upsert(
                            keys[(task * 8191 + i * 257) % KEYS].clone(),
                            Bytes::from_static(if mixed {
                                &b"mixed"[..]
                            } else {
                                &b"concurrent"[..]
                            }),
                        )
                    })
                    .collect();
                tree.apply(
                    root,
                    VersionStamp::from_counter(
                        if mixed { 20_000_000 } else { 10_000_000 } + task as u64,
                    ),
                    muts,
                )
                .await
                .unwrap();
            }
        });
    }
    while let Some(result) = set.join_next().await {
        result.unwrap();
    }
}

fn concurrency(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .build()
        .expect("runtime");
    let f = fixture(&rt, KeyShape::UniformRandom);
    let root = f.root;
    let base = f.store;
    let keys = Arc::new(f.keys);
    let mut group = c.benchmark_group("workload/concurrency");
    group.sample_size(20);
    for tasks in [1usize, 8, 32] {
        group.throughput(Throughput::Elements(tasks as u64));
        group.bench_function(format!("independent_apply_32/{tasks}"), |b| {
            b.iter_custom(|iterations| {
                let mut measured = std::time::Duration::ZERO;
                for _ in 0..iterations {
                    let tree = Arc::new(BeTree::with_format(
                        Arc::new(OverlayStore::new(base.clone())),
                        Format::selected(),
                    ));
                    rt.block_on(tree.get(root, &keys[0])).unwrap();
                    let started = std::time::Instant::now();
                    rt.block_on(concurrent_sample(
                        tree,
                        root,
                        keys.clone(),
                        tasks,
                        false,
                        32,
                    ));
                    measured += started.elapsed();
                }
                measured
            })
        });
    }
    // Multiples of four keep the workload exactly 25% writes and 75% reads at every concurrency, so
    // throughput comparisons are not accidentally comparisons of different operation mixes.
    for tasks in [4usize, 8, 32] {
        group.throughput(Throughput::Elements(tasks as u64));
        group.bench_function(format!("mixed_read_write/{tasks}"), |b| {
            b.iter_custom(|iterations| {
                let mut measured = std::time::Duration::ZERO;
                for _ in 0..iterations {
                    let tree = Arc::new(BeTree::with_format(
                        Arc::new(OverlayStore::new(base.clone())),
                        Format::selected(),
                    ));
                    rt.block_on(tree.get(root, &keys[0])).unwrap();
                    let started = std::time::Instant::now();
                    rt.block_on(concurrent_sample(tree, root, keys.clone(), tasks, true, 16));
                    measured += started.elapsed();
                }
                measured
            })
        });
    }
    group.throughput(Throughput::Elements(32));
    group.bench_function("mixed_read_write_no_cache/32", |b| {
        b.iter_custom(|iterations| {
            let mut measured = std::time::Duration::ZERO;
            for _ in 0..iterations {
                let tree = Arc::new(
                    BeTree::with_format(
                        Arc::new(OverlayStore::new(base.clone())),
                        Format::selected(),
                    )
                    .with_caches(CacheConfig::NONE),
                );
                let started = std::time::Instant::now();
                rt.block_on(concurrent_sample(tree, root, keys.clone(), 32, true, 16));
                measured += started.elapsed();
            }
            measured
        })
    });
    group.finish();
}

/// Prefetch policy is an I/O-round-trip decision, so report it through `CountingStore` rather than
/// selecting it from MemStore latency. Each width gets a fresh decoded cache over the same immutable
/// corpus and consumes the full cursor in bounded result batches.
fn report_prefetch_widths(_c: &mut Criterion) {
    let rt = rt();
    let f = fixture(&rt, KeyShape::UniformRandom);
    println!("\n=== full-scan prefetch width (100k keys, selected format) ===");
    println!(
        "{:>8} {:>8} {:>8} {:>12}",
        "width", "waves", "objects", "bytes"
    );
    for width in [0usize, 1, 16, 64, 256] {
        let counting = Arc::new(CountingStore::new(f.store.clone()));
        let tree = BeTree::with_format(counting, Format::selected()).record_metrics();
        let mut cursor = tree
            .scan_cursor(f.root, None, None)
            .with_prefetch_width(width);
        let mut rows = 0usize;
        loop {
            let batch = rt.block_on(cursor.next_batch(256)).expect("scan");
            if batch.is_empty() {
                break;
            }
            rows += batch.len();
        }
        assert_eq!(rows, KEYS);
        println!(
            "{:>8} {:>8} {:>8} {:>12}",
            width,
            tree.metrics().waves,
            tree.metrics().objects_read,
            tree.metrics().bytes_read,
        );
    }
}

/// Dependent waves and bytes, from the counting store. This is the storage-interaction report; the
/// timings above are compute.
fn report_store_interaction(_c: &mut Criterion) {
    let rt = rt();
    println!("\n=== dependent waves and bytes per operation (CountingStore, selected format) ===");
    let fmt = Format::selected();
    let mem = Arc::new(MemStore::new());
    let counting = Arc::new(CountingStore::new(mem));
    let t = BeTree::with_format(counting.clone(), fmt.clone()).record_metrics();
    let keys = KeyShape::UniformRandom.keys(KEYS, 2024);
    let (root, _m) = rt
        .block_on(harness::build(&t, &keys, 256, VALUE))
        .expect("build");
    let shape = rt
        .block_on(harness::check_balanced(&t, root))
        .expect("balanced");

    println!("build ({KEYS} keys, width 256): {}", counting.report());
    println!("  {}", t.metrics());
    println!(
        "  shape: depth={} nodes={} mean_fanout={:.2} amp={:.2} eff_eps={:.3}",
        shape.max_leaf_depth,
        shape.nodes,
        shape.mean_fanout(),
        shape.space_amplification(),
        shape.effective_epsilon(fmt.leaf_slots() as f64),
    );

    for (label, n) in [("point", 1usize), ("get_many/256", 256)] {
        let cold = BeTree::with_format(counting.clone(), fmt.clone()).record_metrics();
        let probe: Vec<&[u8]> = keys
            .iter()
            .step_by(KEYS / n)
            .take(n)
            .map(|k| k.as_ref())
            .collect();
        rt.block_on(cold.get_many(root, &probe)).expect("read");
        println!(
            "{label:14} cold: waves={} objects={} bytes={} (depth was {})",
            cold.metrics().waves,
            cold.metrics().objects_read,
            cold.metrics().bytes_read,
            shape.max_leaf_depth,
        );
    }

    // Absent-key buffer probes: the number that prices a future routing summary.
    let cold = BeTree::with_format(counting.clone(), fmt.clone()).record_metrics();
    let absent: Vec<Bytes> = (0..256)
        .map(|i| Bytes::from(format!("\u{ff}absent-{i:04}")))
        .collect();
    let refs: Vec<&[u8]> = absent.iter().map(|k| k.as_ref()).collect();
    rt.block_on(cold.get_many(root, &refs)).expect("read");
    println!(
        "absent 256:    waves={} absent_key_buffer_probes={} probes={} (a routing summary could skip the buffer probes)",
        cold.metrics().waves,
        cold.metrics().absent_key_buffer_probes,
        cold.metrics().probes,
    );
}

criterion_group!(
    benches,
    report_store_interaction,
    report_evolving_writes,
    report_prefetch_widths,
    reads,
    writes,
    concurrency,
    diffs,
    verification
);
criterion_main!(benches);
