//! Shared correctness and measurement support for integration tests and benchmarks, including an
//! independently implemented reference model.
//!
//! This lands *before* the codec and tree changes it measures, because a single favorable fixture would
//! hide a structural defect. [`check`] is the shape oracle: it walks a whole tree and asserts every
//! structural commitment, including the ones local decode cannot see (child range
//! ownership, buffer path ownership, and equal leaf depth).
//!
//! `MemStore` timings measure *compute*. [`CountingStore`] is the separate instrument that records
//! calls, bytes, and dependent waves; no asymptotic bound is claimed from either.

#![allow(dead_code, unused_imports)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

use bytes::Bytes;

use be_tree::codec;
use be_tree::format::Format;
use be_tree::store::{AddressedObject, NodeStore};
use be_tree::tree::BeTree;
use be_tree::{AccessHint, BlockId, Mutation, MutationOp, TreeError, VERSION_BYTES, VersionStamp};

/// A deliberately small target-store model for comparing node shapes. It separates dependent-wave
/// latency from transferred bytes; callers supply measured target values rather than inheriting a
/// fictional universal disk or object-store profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreModel {
    pub round_trip_ns: u64,
    pub bytes_per_second: u64,
}

impl StoreModel {
    pub fn cold_read_ns(self, waves: u64, bytes: u64) -> Option<u64> {
        if self.bytes_per_second == 0 {
            return None;
        }
        let latency = u128::from(waves).checked_mul(u128::from(self.round_trip_ns))?;
        let transfer = u128::from(bytes)
            .checked_mul(1_000_000_000)?
            .div_ceil(u128::from(self.bytes_per_second));
        u64::try_from(latency.checked_add(transfer)?).ok()
    }

    /// RTT at which `fewer_waves` and `fewer_bytes` tie. The first shape must actually save waves and
    /// spend bytes; otherwise there is no positive crossover.
    pub fn crossover_round_trip_ns(
        fewer_waves: (u64, u64),
        fewer_bytes: (u64, u64),
        bytes_per_second: u64,
    ) -> Option<u64> {
        let saved_waves = fewer_bytes.0.checked_sub(fewer_waves.0)?;
        let extra_bytes = fewer_waves.1.checked_sub(fewer_bytes.1)?;
        if saved_waves == 0 || bytes_per_second == 0 {
            return None;
        }
        let ns = u128::from(extra_bytes)
            .checked_mul(1_000_000_000)?
            .div_ceil(u128::from(bytes_per_second).checked_mul(u128::from(saved_waves))?);
        u64::try_from(ns).ok()
    }
}
/// Exact histogram for single-process tests and benchmarks; production uses bounded counters.
#[derive(Debug, Default)]
pub struct Histogram(Mutex<Vec<u64>>);

impl Histogram {
    pub fn record(&self, value: u64) {
        self.0.lock().expect("histogram").push(value);
    }

    pub fn count(&self) -> u64 {
        self.0.lock().expect("histogram").len() as u64
    }

    pub fn mean(&self) -> f64 {
        let values = self.0.lock().expect("histogram");
        if values.is_empty() {
            0.0
        } else {
            values.iter().sum::<u64>() as f64 / values.len() as f64
        }
    }

    pub fn quantile(&self, q: f64) -> u64 {
        let mut values = self.0.lock().expect("histogram").clone();
        if values.is_empty() {
            return 0;
        }
        values.sort_unstable();
        let index = ((values.len() as f64 * q).ceil() as usize)
            .saturating_sub(1)
            .min(values.len() - 1);
        values[index]
    }
}
pub struct CountingStore<S: NodeStore> {
    inner: Arc<S>,
    pub gets: AtomicU64,
    pub get_many_calls: AtomicU64,
    pub objects_fetched: AtomicU64,
    pub bytes_fetched: AtomicU64,
    pub put_batches: AtomicU64,
    pub objects_put: AtomicU64,
    pub bytes_put: AtomicU64,
}

impl<S: NodeStore> CountingStore<S> {
    pub fn new(inner: Arc<S>) -> Self {
        CountingStore {
            inner,
            gets: AtomicU64::new(0),
            get_many_calls: AtomicU64::new(0),
            objects_fetched: AtomicU64::new(0),
            bytes_fetched: AtomicU64::new(0),
            put_batches: AtomicU64::new(0),
            objects_put: AtomicU64::new(0),
            bytes_put: AtomicU64::new(0),
        }
    }

    pub fn inner(&self) -> &Arc<S> {
        &self.inner
    }

    /// Dependent round trips: single gets plus batched calls.
    pub fn waves(&self) -> u64 {
        self.gets.load(Relaxed) + self.get_many_calls.load(Relaxed)
    }

    pub fn report(&self) -> String {
        format!(
            "get={} get_many={} objects_fetched={} bytes_fetched={} put_batch={} objects_put={} bytes_put={}",
            self.gets.load(Relaxed),
            self.get_many_calls.load(Relaxed),
            self.objects_fetched.load(Relaxed),
            self.bytes_fetched.load(Relaxed),
            self.put_batches.load(Relaxed),
            self.objects_put.load(Relaxed),
            self.bytes_put.load(Relaxed),
        )
    }
}

impl<S: NodeStore> NodeStore for CountingStore<S> {
    type Class = S::Class;

    async fn get(
        &self,
        id: BlockId,
        hint: AccessHint,
        max_object_bytes: usize,
    ) -> Result<Bytes, TreeError> {
        self.gets.fetch_add(1, Relaxed);
        let r = self.inner.get(id, hint, max_object_bytes).await;
        if let Ok(b) = &r {
            self.objects_fetched.fetch_add(1, Relaxed);
            self.bytes_fetched.fetch_add(b.len() as u64, Relaxed);
        }
        r
    }

    async fn get_many(
        &self,
        ids: &[BlockId],
        hint: AccessHint,
        max_object_bytes: usize,
        max_total_bytes: u64,
    ) -> Vec<Result<Bytes, TreeError>> {
        self.get_many_calls.fetch_add(1, Relaxed);
        let r = self
            .inner
            .get_many(ids, hint, max_object_bytes, max_total_bytes)
            .await;
        for b in r.iter().flatten() {
            self.objects_fetched.fetch_add(1, Relaxed);
            self.bytes_fetched.fetch_add(b.len() as u64, Relaxed);
        }
        r
    }

    async fn put_batch(
        &self,
        objects: Vec<AddressedObject>,
        class: Self::Class,
    ) -> Result<(), TreeError> {
        self.put_batches.fetch_add(1, Relaxed);
        self.objects_put.fetch_add(objects.len() as u64, Relaxed);
        self.bytes_put.fetch_add(
            objects.iter().map(|o| o.bytes.len() as u64).sum::<u64>(),
            Relaxed,
        );
        self.inner.put_batch(objects, class).await
    }
}

// ---------------------------------------------------------------- shape oracle

/// The structural facts a whole-tree walk establishes. Minimum and maximum leaf depth are reported
/// separately as a *correctness* signal: an average would hide an unbalanced spine, which is exactly the
/// defect this oracle exists to prevent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TreeShape {
    pub nodes: usize,
    pub leaves: usize,
    pub internals: usize,
    pub root_level: u16,
    pub min_leaf_depth: usize,
    pub max_leaf_depth: usize,
    pub min_fanout: usize,
    pub max_fanout: usize,
    pub fanout_sum: usize,
    pub leaf_entries: usize,
    pub buffer_entries: usize,
    pub external_values: usize,
    /// Physical serialized bytes of every distinct node.
    pub physical_bytes: usize,
    /// Live logical bytes (keys + value spans + order keys) those nodes carry.
    pub logical_bytes: usize,
}

impl TreeShape {
    pub fn mean_fanout(&self) -> f64 {
        if self.internals == 0 {
            0.0
        } else {
            self.fanout_sum as f64 / self.internals as f64
        }
    }

    /// Physical bytes per live logical byte — the space-amplification number exact-size regular nodes
    /// owe the reader.
    pub fn space_amplification(&self) -> f64 {
        if self.logical_bytes == 0 {
            f64::INFINITY
        } else {
            self.physical_bytes as f64 / self.logical_bytes as f64
        }
    }

    /// The **diagnostic** effective ε for one fixture: `ln(F_realized) / ln(B_fixture)`, where
    /// `B_fixture` is how many of that fixture's items fit a regular node. Not a format constant and
    /// not a universal complexity claim.
    pub fn effective_epsilon(&self, b_fixture: f64) -> f64 {
        let f = self.mean_fanout();
        if f <= 1.0 || b_fixture <= 1.0 {
            return f64::NAN;
        }
        f.ln() / b_fixture.ln()
    }
}

/// One pending node in the checker's walk: the node, its depth, the absolute key range the path to it
/// owns, and the level its parent implies.
#[derive(Debug, Clone)]
struct KeyRange {
    lo: Option<Bytes>,
    hi: Option<Bytes>,
}

impl KeyRange {
    fn new(lo: Option<Bytes>, hi: Option<Bytes>) -> Self {
        Self { lo, hi }
    }

    fn contains(&self, key: &[u8]) -> bool {
        self.lo.as_ref().is_none_or(|lo| key >= lo.as_ref())
            && self.hi.as_ref().is_none_or(|hi| key < hi.as_ref())
    }

    fn intersection(&self, other: &Self) -> Self {
        let lo = match (&self.lo, &other.lo) {
            (Some(a), Some(b)) => Some(a.max(b).clone()),
            (Some(bound), None) | (None, Some(bound)) => Some(bound.clone()),
            (None, None) => None,
        };
        let hi = match (&self.hi, &other.hi) {
            (Some(a), Some(b)) => Some(a.min(b).clone()),
            (Some(bound), None) | (None, Some(bound)) => Some(bound.clone()),
            (None, None) => None,
        };
        Self { lo, hi }
    }
}

struct Visit {
    id: BlockId,
    depth: usize,
    range: KeyRange,
    expect_level: Option<u16>,
}

/// Walk every node under `root` and check every structural commitment. Returns the shape, or the first
/// violation as a message.
pub async fn check<S: NodeStore>(tree: &BeTree<S>, root: BlockId) -> Result<TreeShape, TreeError> {
    let fmt = tree.format().clone();
    let mut shape = TreeShape {
        min_leaf_depth: usize::MAX,
        min_fanout: usize::MAX,
        ..Default::default()
    };
    let mut seen: foldhash::HashSet<BlockId> = Default::default();
    let mut stack: Vec<Visit> = vec![Visit {
        id: root,
        depth: 0,
        range: KeyRange::new(None, None),
        expect_level: None,
    }];
    let bad = |m: String| TreeError::Store(format!("invariant violated: {m}"));

    while let Some(Visit {
        id,
        depth,
        range,
        expect_level: expect,
    }) = stack.pop()
    {
        let v = tree.view(id).await?;
        if let Some(expect) = expect
            && v.tree_level() != expect
        {
            return Err(bad(format!(
                "node {id} has level {} but its parent implies {expect}",
                v.tree_level()
            )));
        }
        if v.bytes().len() != fmt.node_bytes() {
            return Err(bad(format!(
                "node {id} is {} bytes, not NODE_BYTES {}",
                v.bytes().len(),
                fmt.node_bytes()
            )));
        }
        // Canonical re-encoding: rebuilding the same logical node must give the same bytes and id.
        let rebuilt = if v.is_leaf() {
            codec::encode_leaf(&fmt, &v.entries().collect::<Vec<_>>())?
        } else {
            codec::encode_internal(
                &fmt,
                v.tree_level(),
                &(0..v.pivot_count())
                    .map(|i| v.pivot_bytes(i))
                    .collect::<Vec<_>>(),
                &v.children().collect::<Vec<_>>(),
                &v.entries().collect::<Vec<_>>(),
            )?
        };
        if rebuilt != *v.bytes() || BlockId::of(&rebuilt) != id {
            return Err(bad(format!(
                "node {id} does not re-encode to its own bytes"
            )));
        }

        // Every entry this node holds must lie in the key range the path to it owns — the Bε path
        // invariant, which local decode cannot check.
        for i in 0..v.entry_count() {
            let k = v.entry_key(i);
            if !range.contains(k) {
                return Err(bad(format!(
                    "node {id} holds key {k:?} outside its owned range {range:?}"
                )));
            }
            if v.entry_op(i) == be_tree::format::OP_EXTERNAL {
                shape.external_values += 1;
            }
        }

        let first_visit = seen.insert(id);
        if first_visit {
            shape.nodes += 1;
            shape.physical_bytes += v.bytes().len();
            shape.logical_bytes += v.logical_entry_blob_len() + v.entry_count() * VERSION_BYTES;
        }

        if v.is_leaf() {
            shape.leaves += 1;
            shape.leaf_entries += v.entry_count();
            shape.min_leaf_depth = shape.min_leaf_depth.min(depth);
            shape.max_leaf_depth = shape.max_leaf_depth.max(depth);
            if !fmt.leaf_fits(v.entry_count(), v.entry_blob_len()) {
                return Err(bad(format!("leaf {id} exceeds regular leaf capacity")));
            }
        } else {
            shape.internals += 1;
            shape.buffer_entries += v.entry_count();
            shape.root_level = shape.root_level.max(v.tree_level());
            let f = v.child_count();
            shape.min_fanout = shape.min_fanout.min(f);
            shape.max_fanout = shape.max_fanout.max(f);
            shape.fanout_sum += f;
            if f < 2 || f > fmt.f_max() {
                return Err(bad(format!("internal {id} has fanout {f}")));
            }
            if v.pivot_count() + 1 != f {
                return Err(bad(format!("internal {id} pivot/child cardinality")));
            }
            if !fmt.buffer_fits(v.entry_count(), v.entry_blob_len()) {
                return Err(bad(format!(
                    "internal {id} buffer exceeds regular capacity"
                )));
            }
            for i in 0..f {
                let (clo, chi) = v.child_range(i);
                // A child's range is the intersection of its pivot range with its parent's range;
                // pivots are nested, so the intersection must equal the pivot range.
                let child_range = range.intersection(&KeyRange::new(
                    clo.map(Bytes::copy_from_slice),
                    chi.map(Bytes::copy_from_slice),
                ));
                stack.push(Visit {
                    id: v.child(i),
                    depth: depth + 1,
                    range: child_range,
                    expect_level: Some(v.tree_level() - 1),
                });
            }
        }
    }
    if shape.min_leaf_depth == usize::MAX {
        shape.min_leaf_depth = 0;
    }
    if shape.min_fanout == usize::MAX {
        shape.min_fanout = 0;
    }
    Ok(shape)
}

/// `check`, plus the equal-leaf-depth assertion required by the tree invariants.
pub async fn check_balanced<S: NodeStore>(
    tree: &BeTree<S>,
    root: BlockId,
) -> Result<TreeShape, TreeError> {
    let shape = check(tree, root).await?;
    if shape.min_leaf_depth != shape.max_leaf_depth {
        return Err(TreeError::Store(format!(
            "invariant violated: leaf depths {}..{} are not equal",
            shape.min_leaf_depth, shape.max_leaf_depth
        )));
    }
    Ok(shape)
}

// ---------------------------------------------------------------- reference model

/// One candidate in the model's ordering. The comparator is implemented here **independently** of
/// [`be_tree::Winner`], so a bug in one does not silently validate the other.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cand {
    order_key: [u8; VERSION_BYTES],
    /// 0 = inline upsert, 1 = out-of-line upsert, 2 = tombstone.
    rank: u8,
    /// Inline value bytes, or the out-of-line object id.
    tail: Vec<u8>,
    /// Out-of-line logical length, compared numerically.
    len: u32,
    /// The observable value, or `None` for a tombstone.
    value: Option<Vec<u8>>,
}

impl Cand {
    fn key_tuple(&self) -> (&[u8], u8, &[u8], u32) {
        (&self.order_key, self.rank, &self.tail, self.len)
    }
    fn greater_than(&self, other: &Cand) -> bool {
        self.key_tuple() > other.key_tuple()
    }
}

/// A `BTreeMap`-backed reference implementation of the *logical* data model: latest-value reads,
/// immutable snapshots, batch duplicate collapse, and the total order. It knows nothing about nodes,
/// buffers, flushes, or bytes.
#[derive(Debug, Clone, Default)]
pub struct Model {
    map: BTreeMap<Vec<u8>, Cand>,
}

impl Model {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one stamped batch. Repeated keys collapse to the LAST mutation in program order; the
    /// surviving candidate for a key is the greater winner tuple.
    pub fn apply(&mut self, fmt: &Format, stamp: VersionStamp, mutations: &[Mutation]) {
        // Independent duplicate collapse: last write in program order wins, before any comparison.
        let mut last: BTreeMap<Vec<u8>, &MutationOp> = BTreeMap::new();
        for m in mutations {
            last.insert(m.key.to_vec(), &m.op);
        }
        for (key, op) in last {
            let cand = match op {
                MutationOp::Tombstone => Cand {
                    order_key: stamp.order_key,
                    rank: 2,
                    tail: Vec::new(),
                    len: 0,
                    value: None,
                },
                MutationOp::Upsert(v) if fmt.is_inline(v.len()) => Cand {
                    order_key: stamp.order_key,
                    rank: 0,
                    tail: v.to_vec(),
                    len: 0,
                    value: Some(v.to_vec()),
                },
                MutationOp::Upsert(v) => {
                    let obj = be_tree::value::encode(fmt.schema_id(), v);
                    Cand {
                        order_key: stamp.order_key,
                        rank: 1,
                        tail: obj.id.0.to_vec(),
                        len: obj.logical_len,
                        value: Some(v.to_vec()),
                    }
                }
            };
            match self.map.get(&key) {
                Some(cur) if !cand.greater_than(cur) => {}
                _ => {
                    self.map.insert(key, cand);
                }
            }
        }
    }

    pub fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.map.get(key).and_then(|c| c.value.clone())
    }

    pub fn scan_range(&self, lo: Option<&[u8]>, hi: Option<&[u8]>) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.map
            .iter()
            .filter(|(k, _)| {
                lo.is_none_or(|l| k.as_slice() >= l) && hi.is_none_or(|h| k.as_slice() < h)
            })
            .filter_map(|(k, c)| c.value.clone().map(|v| (k.clone(), v)))
            .collect()
    }

    pub fn scan_prefix(&self, prefix: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.map
            .iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .filter_map(|(k, c)| c.value.clone().map(|v| (k.clone(), v)))
            .collect()
    }

    /// Keys whose observable value differs. Tombstoned and absent are the same observation.
    pub fn diff(&self, other: &Model) -> Vec<Vec<u8>> {
        let mut keys: Vec<Vec<u8>> = self.map.keys().chain(other.map.keys()).cloned().collect();
        keys.sort();
        keys.dedup();
        keys.retain(|k| self.get(k) != other.get(k));
        keys
    }

    pub fn live_len(&self) -> usize {
        self.map.values().filter(|c| c.value.is_some()).count()
    }
}

// ---------------------------------------------------------------- fixtures

/// A small deterministic PRNG (SplitMix64). Seeded fixtures make every reported measurement and every
/// randomized property failure reproducible without a dependency.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_add(0x9E37_79B9_7F4A_7C15))
    }
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
    }
    pub fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| (self.next_u64() & 0xff) as u8).collect()
    }
}

/// The key-shape fixtures used by the workload matrix. Every workload runs against all of them, because a single
/// favorable distribution hides both the degeneration and the equal-head-range risk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyShape {
    /// Uniform random 16-byte keys.
    UniformRandom,
    /// A long shared prefix, which collapses a surface-wide head skip.
    LongSharedPrefix,
    /// Keys containing embedded zero bytes, plus the empty key.
    EmbeddedZeroes,
    /// Ascending dense keys — the order that produced the original degeneration.
    Ascending,
}

impl KeyShape {
    pub fn all() -> &'static [KeyShape] {
        &[
            KeyShape::UniformRandom,
            KeyShape::LongSharedPrefix,
            KeyShape::EmbeddedZeroes,
            KeyShape::Ascending,
        ]
    }

    pub fn name(self) -> &'static str {
        match self {
            KeyShape::UniformRandom => "uniform-random",
            KeyShape::LongSharedPrefix => "long-shared-prefix",
            KeyShape::EmbeddedZeroes => "embedded-zeroes",
            KeyShape::Ascending => "ascending",
        }
    }

    pub fn keys(self, n: usize, seed: u64) -> Vec<Bytes> {
        let mut rng = Rng::new(seed);
        let mut out: Vec<Bytes> = match self {
            KeyShape::UniformRandom => (0..n).map(|_| Bytes::from(rng.bytes(16))).collect(),
            KeyShape::LongSharedPrefix => (0..n)
                .map(|i| {
                    let mut k = b"organizations/acme/projects/atlas/documents/revisions/".to_vec();
                    k.extend_from_slice(format!("{i:08}").as_bytes());
                    Bytes::from(k)
                })
                .collect(),
            KeyShape::EmbeddedZeroes => (0..n)
                .map(|i| {
                    let mut k = Vec::new();
                    k.extend_from_slice(&(i as u32).to_be_bytes());
                    k.push(0);
                    k.extend_from_slice(&[0u8; 3]);
                    k.push((i % 251) as u8);
                    Bytes::from(k)
                })
                .collect(),
            KeyShape::Ascending => (0..n)
                .map(|i| Bytes::from(format!("k{i:08}").into_bytes()))
                .collect(),
        };
        if self == KeyShape::EmbeddedZeroes && n > 0 {
            out[0] = Bytes::new(); // the empty key must survive every path
        }
        out.sort();
        out.dedup();
        out
    }
}

/// Commit widths the shape matrix crosses with key order. `usize::MAX` means "all at once";
/// [`build`] clamps it to the key count.
pub const COMMIT_WIDTHS: &[usize] = &[1, 16, 256, usize::MAX];

/// Key orders the shape matrix crosses with [`COMMIT_WIDTHS`].
pub const KEY_ORDERS: &[KeyOrder] = &[KeyOrder::Ascending, KeyOrder::Descending, KeyOrder::Random];

/// How a fixture's sorted keys are reordered before they are applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyOrder {
    Ascending,
    Descending,
    /// Seeded Fisher-Yates, so a failure reproduces exactly.
    Random,
}

impl KeyOrder {
    pub fn name(self) -> &'static str {
        match self {
            KeyOrder::Ascending => "ascending",
            KeyOrder::Descending => "descending",
            KeyOrder::Random => "random",
        }
    }

    pub fn apply(self, keys: &mut [Bytes], seed: u64) {
        match self {
            KeyOrder::Ascending => {}
            KeyOrder::Descending => keys.reverse(),
            KeyOrder::Random => {
                let mut rng = Rng::new(seed);
                for i in (1..keys.len()).rev() {
                    let j = rng.below(i + 1);
                    keys.swap(i, j);
                }
            }
        }
    }
}

/// Build a tree by applying `keys` in `order` with commit width `width`, one ascending stamp per commit.
/// Returns the final root and the model that mirrors it.
pub async fn build<S: NodeStore>(
    tree: &BeTree<S>,
    keys: &[Bytes],
    width: usize,
    value_len: usize,
) -> Result<(BlockId, Model), TreeError> {
    let fmt = tree.format().clone();
    let mut model = Model::new();
    let mut root = tree.empty_root().await?;
    let width = width.min(keys.len()).max(1);
    for (stamp_n, chunk) in (1u64..).zip(keys.chunks(width)) {
        let stamp = VersionStamp::from_counter(stamp_n);
        let muts: Vec<Mutation> = chunk
            .iter()
            .map(|k| {
                let mut v = Vec::with_capacity(value_len);
                v.extend_from_slice(&k[..k.len().min(value_len)]);
                v.resize(value_len, b'v');
                Mutation::upsert(k.clone(), Bytes::from(v))
            })
            .collect();
        model.apply(&fmt, stamp, &muts);
        root = tree.apply(root, stamp, muts).await?;
    }
    Ok((root, model))
}
