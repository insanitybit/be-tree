//! The content-addressed COW buffered tree.
//!
//! **Descent** is wave-synchronous: for a loaded BFS wave, probes are grouped by node and each
//! validated head surface is reused for every probe assigned to that node. Multi-key navigation costs
//! O(tree depth) dependent `get_many` waves plus at most one batched value-object wave for out-of-line
//! winners.
//!
//! **Ascent** is byte-accounted. A transient builder that exceeds either regular-buffer capacity routes
//! its messages by the current pivots, flushes the child owning the most pending *encoded bytes*, and
//! integrates whatever ordered replacement run the child returns — splicing the parent *wider* rather
//! than nesting temporary two-child internal nodes. An overfull internal builder partitions into
//! contiguous groups and partitions its buffer at every promoted pivot, because every message for key
//! `k` must remain on `k`'s root-to-leaf path.
//!
//! Nothing here is confluent: canonical encoding gives one byte string per logical *node*, not one tree
//! shape per resolved key/value set. Different operation orders may produce different roots for
//! observably equal maps, and `tests/model.rs` asserts exactly that distinction.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use bytes::Bytes;

use crate::MetricsSnapshot;
use crate::cache::ObjectCache;
use crate::codec::{self, Entry, NodeView};
use crate::format::Format;
use crate::inflight::Inflight;
use crate::metrics::Metrics;
use crate::store::{AddressedObject, NodeStore};
use crate::value;
use crate::{
    AccessHint, BlockId, BudgetState, CapacityError, DecodeError, Mutation, MutationOp, TreeError,
    VersionStamp, Winner, WinnerOp, WorkBudget,
};

/// Whether a cache miss rehashes fetched bytes before decoding them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VerifyPolicy {
    /// Hash every fetched byte string and compare it with the requested `BlockId` before decoding.
    /// Hashing arbitrary bytes is safe; decode validation still establishes well-formedness.
    #[default]
    Always,
    /// Rely on the [`NodeStore`]'s own content-addressing contract.
    Never,
}

type Fut<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, TreeError>> + Send + 'a>>;

/// Objects one cursor lookahead may fetch. At the selected 64 KiB node size this bounds speculative
/// prefetch to about 4 MiB, which is the memory a scan trades for batched reads. On the selected
/// 100k-key fixture, width 64 cuts a full scan from 162 to 11 waves; width 256 spends 4x the lookahead
/// memory to reach 6, so 64 is the measured default knee rather than a hidden universal constant.
pub const DEFAULT_PREFETCH_WIDTH: usize = 64;

/// The ordered replacement run one recursive write produced. `following` is empty for a stable
/// one-node rewrite; a binary `Split` would be insufficient because a large flushed batch can split one
/// child into more than two regular nodes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Rewrite {
    first: BlockId,
    /// `(minimum key of that node, its id)`, in key order.
    following: Vec<(Bytes, BlockId)>,
    /// The tree level every node in this run sits at.
    level: u16,
}

impl Rewrite {
    fn single(id: BlockId, level: u16) -> Rewrite {
        Rewrite {
            first: id,
            following: Vec::new(),
            level,
        }
    }
    fn ids(&self) -> Vec<BlockId> {
        std::iter::once(self.first)
            .chain(self.following.iter().map(|(_, id)| *id))
            .collect()
    }
}

/// One transient internal node. Keeping its coupled columns together makes the cardinality invariant
/// (`children = pivots + 1`) visible at every flush, splice, and partition boundary.
struct InternalBuilder {
    level: u16,
    children: Vec<BlockId>,
    pivots: Vec<Bytes>,
    buffer: Vec<Entry>,
    direct: Vec<Entry>,
}

impl InternalBuilder {
    fn needs_partition(&self, fmt: &Format) -> bool {
        self.children.len() > fmt.f_max() || !fmt.pivots_fit(&self.pivots)
    }

    /// Replace one child with an ordered rewrite run, widening this node when the child split.
    fn splice(&mut self, at: usize, rewrite: Rewrite) {
        self.children[at] = rewrite.first;
        for (offset, (min_key, id)) in rewrite.following.into_iter().enumerate() {
            self.pivots.insert(at + offset, min_key);
            self.children.insert(at + 1 + offset, id);
        }
        debug_assert_eq!(self.children.len(), self.pivots.len() + 1);
    }
}

/// Exactly the staged objects reachable from the proposed root.
struct Reachable {
    nodes: foldhash::HashSet<BlockId>,
    values: foldhash::HashSet<BlockId>,
    decoded_nodes: Vec<(BlockId, Arc<NodeView>)>,
}

/// Accumulates one apply's new objects so the whole rewrite lands as a single addressed `put_batch`.
/// Staging is pure — `BlockId::of` is BLAKE3, no I/O — so a parent can reference a freshly staged child
/// by id before that child is durable. The batch lands before anything names the new root.
#[derive(Default)]
struct Staged {
    objects: Vec<AddressedObject>,
    /// Ids already staged. Content addressing makes a duplicate submission redundant *and* the tree can
    /// produce many: one 513-byte value applied to 256 keys is one distinct object, and staging it 256
    /// times submitted 257 objects and 200 KiB where 2 objects and ~1.5 KiB were new. At the 4 MiB value
    /// limit the same shape turns one logical value into ~1 GiB of write traffic.
    seen: foldhash::HashSet<BlockId>,
    bytes_hashed: u64,
    /// Nodes staged this apply, so they can warm the decoded cache once the batch is durable.
    node_bytes: Vec<(BlockId, Bytes)>,
    /// Distinct value payloads staged this apply, so read-after-write does not fetch bytes the caller
    /// just supplied. Inserted only after the addressed batch is durable.
    value_payloads: Vec<(BlockId, Bytes)>,
    duplicates_elided: u64,
}

impl Staged {
    /// Stage one node whose id a parent needs immediately.
    fn one(&mut self, bytes: Bytes) -> BlockId {
        let id = BlockId::of(&bytes);
        self.bytes_hashed += bytes.len() as u64;
        if self.insert(id, bytes.clone()) {
            self.node_bytes.push((id, bytes));
        }
        id
    }

    /// Stage an object whose id is **already known**, without hashing it again. `value::encode` has to
    /// hash to address the object; re-hashing the same bytes here was pure duplicated work.
    fn value(&mut self, obj: &value::ValueObject) {
        if self.insert(obj.id, obj.bytes.clone()) {
            // Share the canonical object's allocation with the value cache. Retaining the caller's
            // original `Bytes` here would keep a second full allocation alive through publication.
            self.value_payloads
                .push((obj.id, obj.bytes.slice(value::ENVELOPE_BYTES..)));
        }
    }

    /// `true` when this id was newly staged.
    fn insert(&mut self, id: BlockId, bytes: Bytes) -> bool {
        if !self.seen.insert(id) {
            self.duplicates_elided += 1;
            return false;
        }
        self.objects.push(AddressedObject { id, bytes });
        true
    }

    /// Stage a whole topological layer of independent nodes.
    fn layer(&mut self, layer: Vec<Bytes>) -> Vec<BlockId> {
        let ids: Vec<BlockId> = layer.iter().map(|bytes| BlockId::of(bytes)).collect();
        for (id, bytes) in ids.iter().zip(layer) {
            self.bytes_hashed += bytes.len() as u64;
            if self.insert(*id, bytes.clone()) {
                self.node_bytes.push((*id, bytes));
            }
        }
        ids
    }

    /// Reachability inside this apply's staged node DAG. A normalized external value may lose to an
    /// existing winner, and a transient node may be replaced before publication; neither belongs in
    /// the durable batch. Old child ids are absent from `node_bytes` and are already durable by the
    /// input-root contract.
    fn reachable(&self, fmt: &Arc<Format>, root: BlockId) -> Result<Reachable, TreeError> {
        let nodes: foldhash::HashMap<BlockId, &Bytes> = self
            .node_bytes
            .iter()
            .map(|(id, bytes)| (*id, bytes))
            .collect();
        let mut reachable_nodes = foldhash::HashSet::default();
        let mut reachable_values = foldhash::HashSet::default();
        let mut decoded = Vec::new();
        let mut pending = vec![root];
        while let Some(id) = pending.pop() {
            let Some(bytes) = nodes.get(&id) else {
                continue;
            };
            if !reachable_nodes.insert(id) {
                continue;
            }
            let view = NodeView::decode(fmt, Some(id), (*bytes).clone())?;
            for (kind, referenced) in view.references() {
                match kind {
                    crate::ObjectKind::Node => pending.push(referenced),
                    crate::ObjectKind::Value => {
                        reachable_values.insert(referenced);
                    }
                }
            }
            decoded.push((id, Arc::new(view)));
        }
        Ok(Reachable {
            nodes: reachable_nodes,
            values: reachable_values,
            decoded_nodes: decoded,
        })
    }
}

/// The buffered tree over any [`NodeStore`]. `&self` throughout: shared as `Arc<BeTree<_>>`.
pub struct BeTree<S: NodeStore> {
    store: Arc<S>,
    fmt: Arc<Format>,
    class: S::Class,
    verify: VerifyPolicy,
    budget: WorkBudget,
    /// Decoded-node cache. A node is immutable content-addressed bytes, so a cached decode is coherent
    /// forever — zero invalidation, eviction is pure capacity. Weighted by the retained byte string.
    cache: ObjectCache<Arc<NodeView>>,
    /// Out-of-line value cache, keyed by the value object's id. Values were excluded from caching
    /// entirely, so three reads of one external value fetched it three times.
    values: ObjectCache<Bytes>,
    node_flights: Arc<Inflight<Arc<NodeView>>>,
    value_flights: Arc<Inflight<Bytes>>,
    metrics: Arc<Metrics>,
    resolve_scratch: Arc<Mutex<Vec<ResolveScratch>>>,
    wave_scratch: Arc<Mutex<Vec<WaveScratch>>>,
    materialize_scratch: Arc<Mutex<Vec<MaterializeScratch>>>,
}

#[derive(Default)]
struct ResolveScratch {
    best: Vec<Option<Winner>>,
    frontier: Vec<(usize, BlockId, u16)>,
    ids: Vec<BlockId>,
    next: Vec<(usize, BlockId, u16)>,
}

#[derive(Default)]
struct MaterializeScratch {
    refs: Vec<(BlockId, u32)>,
    slots: Vec<Option<usize>>,
    index: foldhash::HashMap<BlockId, usize>,
    out: Vec<Option<Bytes>>,
    cached: Vec<Option<Bytes>>,
    missing: Vec<(BlockId, u32)>,
}

#[derive(Default)]
struct Wave {
    entries: Vec<(BlockId, Arc<NodeView>)>,
}

impl Wave {
    fn get(&self, id: &BlockId) -> Option<&Arc<NodeView>> {
        match self
            .entries
            .binary_search_by_key(id, |(entry_id, _)| *entry_id)
        {
            Ok(idx) => Some(&self.entries[idx].1),
            Err(_) => None,
        }
    }

    fn into_values(self) -> impl Iterator<Item = Arc<NodeView>> {
        self.entries.into_iter().map(|(_, view)| view)
    }
}

#[derive(Default)]
struct WaveScratch {
    ids: Vec<BlockId>,
    entries: Vec<(BlockId, Arc<NodeView>)>,
    misses: Vec<BlockId>,
    fetch: Vec<BlockId>,
}

impl WaveScratch {
    fn reset(&mut self) {
        self.ids.clear();
        self.entries.clear();
        self.misses.clear();
        self.fetch.clear();
    }
}

impl MaterializeScratch {
    fn reset(&mut self, keys_len: usize) {
        self.refs.clear();
        self.slots.clear();
        self.index.clear();
        self.out.clear();
        self.cached.clear();
        self.missing.clear();
        self.slots.resize(keys_len, None);
        self.out.reserve(keys_len);
        self.cached.reserve(keys_len);
        self.missing.reserve(keys_len);
    }
}

impl ResolveScratch {
    fn reset(&mut self, keys_len: usize) {
        self.best.clear();
        self.best.resize(keys_len, None);
        self.frontier.clear();
        self.ids.clear();
        self.next.clear();
    }
}

/// Result of an offline canonical rewrite. The old root remains valid and untouched; the new root is
/// published only through the returned id after every target batch is durable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    pub source_schema: crate::format::SchemaId,
    pub target_schema: crate::format::SchemaId,
    pub old_root: BlockId,
    pub new_root: BlockId,
    pub rows: u64,
    pub apply_batches: u64,
}

/// Cache sizing. Hard-coding 512 MiB of nodes and no value cache at all is not a defensible universal
/// policy — an embedded host may have a total budget smaller than that default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheConfig {
    /// Weight budget for decoded nodes, in bytes.
    pub node_bytes: u64,
    /// Weight budget for out-of-line value payloads, in bytes.
    pub value_bytes: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        CacheConfig {
            node_bytes: 64 << 20,
            value_bytes: 16 << 20,
        }
    }
}

impl CacheConfig {
    /// Caching disabled entirely, for a host that does its own.
    pub const NONE: CacheConfig = CacheConfig {
        node_bytes: 0,
        value_bytes: 0,
    };
}

/// Weight of a cached view: the actual retained byte-string length plus fixed view metadata, with a
/// saturating conversion into moka's weight type.
fn view_weight(v: &Arc<NodeView>) -> u32 {
    const VIEW_OVERHEAD: usize = std::mem::size_of::<NodeView>();
    v.bytes()
        .len()
        .saturating_add(v.materialized_key_bytes())
        .saturating_add(VIEW_OVERHEAD)
        .min(u32::MAX as usize) as u32
}

impl<S: NodeStore> BeTree<S> {
    pub fn new(store: Arc<S>) -> Self {
        Self::with_format(store, Format::selected())
    }

    /// Open a root whose schema is in the released-format registry. The envelope is used only to pick
    /// a decoder; the root hash and every canonical invariant are then verified normally.
    pub async fn open_known(store: Arc<S>, root: BlockId) -> Result<Self, TreeError> {
        let max = Format::known_schemas()
            .map(|format| format.max_object_bytes())
            .max()
            .expect("registry is nonempty");
        let bytes = store.get(root, AccessHint::Random, max).await?;
        let actual = BlockId::of(&bytes);
        if actual != root {
            return Err(TreeError::HashMismatch {
                requested: root,
                actual,
            });
        }
        let schema = codec::declared_schema(&bytes)?;
        let format = Format::known_schema(&schema).ok_or_else(|| {
            TreeError::decode(
                Some(root),
                DecodeError::SchemaId {
                    found: schema.iter().map(|byte| format!("{byte:02x}")).collect(),
                    expected: "a schema in Format::known_schema".into(),
                },
            )
        })?;
        let tree = BeTree::with_format(store, format);
        let view = Arc::new(NodeView::decode(&tree.fmt, Some(root), bytes)?);
        tree.cache.insert(root, view).await;
        Ok(tree)
    }

    pub fn with_format(store: Arc<S>, fmt: Format) -> Self {
        let fmt = Arc::new(fmt);
        BeTree {
            store,
            fmt,
            class: S::Class::default(),
            verify: VerifyPolicy::default(),
            budget: WorkBudget::default(),
            cache: ObjectCache::new(CacheConfig::default().node_bytes, |_id, view| {
                view_weight(view)
            }),
            values: ObjectCache::new(CacheConfig::default().value_bytes, |_id, value: &Bytes| {
                value.len().min(u32::MAX as usize) as u32
            }),
            node_flights: Arc::new(Inflight::default()),
            value_flights: Arc::new(Inflight::default()),
            metrics: Arc::new(Metrics::default()),
            resolve_scratch: Arc::new(Mutex::new(Vec::new())),
            wave_scratch: Arc::new(Mutex::new(Vec::new())),
            materialize_scratch: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Size both caches. Zero disables one.
    pub fn with_caches(mut self, cfg: CacheConfig) -> Self {
        self.cache = ObjectCache::new(cfg.node_bytes, |_id, view| view_weight(view));
        self.values = ObjectCache::new(cfg.value_bytes, |_id, value: &Bytes| {
            value.len().min(u32::MAX as usize) as u32
        });
        self
    }

    pub fn with_verify(mut self, verify: VerifyPolicy) -> Self {
        self.verify = verify;
        self
    }
    pub fn with_budget(mut self, budget: WorkBudget) -> Self {
        self.budget = budget;
        self
    }
    /// Enable lock-free workload counters. Recording is off by default.
    pub fn record_metrics(mut self) -> Self {
        self.metrics = Arc::new(Metrics::recording());
        self
    }

    /// A cheap class-scoped view sharing the store, format, cache, and metrics.
    pub fn for_class(&self, class: S::Class) -> Self {
        BeTree {
            store: self.store.clone(),
            fmt: self.fmt.clone(),
            class,
            verify: self.verify,
            budget: self.budget,
            cache: self.cache.clone(),
            values: self.values.clone(),
            node_flights: self.node_flights.clone(),
            value_flights: self.value_flights.clone(),
            metrics: self.metrics.clone(),
            resolve_scratch: self.resolve_scratch.clone(),
            wave_scratch: self.wave_scratch.clone(),
            materialize_scratch: self.materialize_scratch.clone(),
        }
    }

    pub fn format(&self) -> &Format {
        &self.fmt
    }
    /// Read the optional workload counters without exposing their synchronization machinery.
    pub fn metrics(&self) -> MetricsSnapshot {
        self.metrics.snapshot()
    }

    /// Return the canonical root of an empty tree.
    pub async fn empty_root(&self) -> Result<BlockId, TreeError> {
        let bytes = codec::encode_leaf(&self.fmt, &[])?;
        let id = BlockId::of(&bytes);
        let view = Arc::new(NodeView::decode(&self.fmt, Some(id), bytes.clone())?);
        self.store
            .put_batch(vec![AddressedObject { id, bytes }], self.class)
            .await?;
        self.warm_cache(vec![(id, view)]).await;
        Ok(id)
    }

    /// Apply one stamped batch and return a new root without mutating the old snapshot.
    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    pub async fn apply(
        &self,
        root: BlockId,
        stamp: VersionStamp,
        mutations: Vec<Mutation>,
    ) -> Result<BlockId, TreeError> {
        if mutations.is_empty() {
            return Ok(root);
        }
        let mut staged = Staged::default();
        let entries = self.normalize(stamp, mutations, &mut staged)?;
        self.apply_prepared(root, entries, staged).await
    }

    /// Read one key. Returns `None` when it is absent or tombstoned.
    pub async fn get(&self, root: BlockId, key: &[u8]) -> Result<Option<Bytes>, TreeError> {
        let mut budget = BudgetState::new(self.budget);
        let mut scratch = self
            .resolve_scratch
            .lock()
            .expect("resolve scratch pool lock")
            .pop()
            .unwrap_or_default();
        if let Err(error) = self
            .resolve_many_cached(root, &[key], &mut budget, &mut scratch)
            .await
        {
            self.resolve_scratch
                .lock()
                .expect("resolve scratch pool lock")
                .push(scratch);
            return Err(error);
        }
        let winner = scratch.best.pop().flatten();
        self.resolve_scratch
            .lock()
            .expect("resolve scratch pool lock")
            .push(scratch);
        match winner.map(|winner| winner.op) {
            Some(WinnerOp::Inline(value)) => Ok(Some(value)),
            Some(WinnerOp::External { id, len }) => Ok(Some(
                self.load_values(&[(id, len)], &mut budget)
                    .await?
                    .pop()
                    .expect("one external value reference"),
            )),
            Some(WinnerOp::Tombstone) | None => Ok(None),
        }
    }

    /// Read many keys in O(tree depth) dependent node waves, preserving input order.
    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    pub async fn get_many(
        &self,
        root: BlockId,
        keys: &[&[u8]],
    ) -> Result<Vec<Option<Bytes>>, TreeError> {
        let mut budget = BudgetState::new(self.budget);
        let mut scratch = self
            .resolve_scratch
            .lock()
            .expect("resolve scratch pool lock")
            .pop()
            .unwrap_or_default();
        let mut materialize = self
            .materialize_scratch
            .lock()
            .expect("materialize scratch pool lock")
            .pop()
            .unwrap_or_default();
        if let Err(err) = self
            .resolve_many_cached(root, keys, &mut budget, &mut scratch)
            .await
        {
            self.resolve_scratch
                .lock()
                .expect("resolve scratch pool lock")
                .push(scratch);
            self.materialize_scratch
                .lock()
                .expect("materialize scratch pool lock")
                .push(materialize);
            return Err(err);
        };
        let values = match self
            .materialize_cached(&scratch.best, &mut budget, &mut materialize)
            .await
        {
            Ok(values) => values,
            Err(err) => {
                self.materialize_scratch
                    .lock()
                    .expect("materialize scratch pool lock")
                    .push(materialize);
                self.resolve_scratch
                    .lock()
                    .expect("resolve scratch pool lock")
                    .push(scratch);
                return Err(err);
            }
        };
        scratch.best.clear();
        self.resolve_scratch
            .lock()
            .expect("resolve scratch pool lock")
            .push(scratch);
        self.materialize_scratch
            .lock()
            .expect("materialize scratch pool lock")
            .push(materialize);
        Ok(values)
    }

    /// Collect the sorted unique keys whose resolved values differ between two roots.
    /// Use [`Self::diff_cursor`] for bounded-memory backpressure.
    pub async fn diff(&self, a: BlockId, b: BlockId) -> Result<Vec<Bytes>, TreeError> {
        let mut cursor = self.diff_cursor(a, b);
        let mut differing = Vec::new();
        while let Some(key) = cursor.next().await? {
            differing.push(key);
        }
        Ok(differing)
    }

    /// Collect live entries in the half-open range `[lo, hi)`.
    /// Use [`Self::scan_cursor`] for bounded-memory backpressure.
    pub async fn scan_range(
        &self,
        root: BlockId,
        lo: Option<&[u8]>,
        hi: Option<&[u8]>,
    ) -> Result<Vec<(Bytes, Bytes)>, TreeError> {
        let mut cursor = self.scan_cursor(root, lo, hi);
        let mut rows = Vec::new();
        loop {
            let batch = cursor.next_batch(256).await?;
            if batch.is_empty() {
                break;
            }
            rows.extend(batch);
        }
        Ok(rows)
    }

    /// Collect live entries whose keys begin with `prefix`.
    pub async fn scan_prefix(
        &self,
        root: BlockId,
        prefix: &[u8],
    ) -> Result<Vec<(Bytes, Bytes)>, TreeError> {
        let hi = prefix_succ(prefix);
        self.scan_range(root, Some(prefix), hi.as_deref()).await
    }

    /// Scan many prefixes through one shared traversal, preserving prefix input order.
    pub async fn scan_prefix_many(
        &self,
        root: BlockId,
        prefixes: &[&[u8]],
    ) -> Result<Vec<Vec<(Bytes, Bytes)>>, TreeError> {
        if prefixes.is_empty() {
            return Ok(Vec::new());
        }
        let ranges = prefixes
            .iter()
            .map(|prefix| KeyRange::new(Some(Bytes::copy_from_slice(prefix)), prefix_succ(prefix)))
            .collect();
        let mut budget = BudgetState::new(self.budget);
        let mut cursor = self.cursor_over(root, ranges);
        let mut rows = Vec::new();
        while let Some(row) = cursor.next_kv(&mut budget).await? {
            rows.push(row);
        }
        let live = self.live_pairs(rows, &mut budget).await?;
        Ok(bucket_by_prefix(prefixes, live))
    }

    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    async fn apply_prepared(
        &self,
        root: BlockId,
        entries: Vec<Entry>,
        mut staged: Staged,
    ) -> Result<BlockId, TreeError> {
        if entries.is_empty() {
            return Ok(root);
        }
        let mut budget = BudgetState::new(self.budget);
        let rw = self
            .write_node(root, entries, &mut staged, &mut budget)
            .await?;
        let new_root = self.grow_root(rw, &mut staged)?;
        if new_root == root {
            return Ok(root);
        }

        let reachable = staged.reachable(&self.fmt, new_root)?;
        let values_to_warm: Vec<(BlockId, Bytes)> = staged
            .value_payloads
            .iter()
            .filter(|(id, _)| reachable.values.contains(id))
            .cloned()
            .collect();
        staged.objects.retain(|object| {
            if reachable.nodes.contains(&object.id) {
                !self.cache.contains_key(&object.id)
            } else if reachable.values.contains(&object.id) {
                !self.values.contains_key(&object.id)
            } else {
                false
            }
        });
        self.metrics.written(
            staged.objects.len() as u64,
            staged.bytes_hashed,
            staged.duplicates_elided,
        );
        if !staged.objects.is_empty() {
            self.store.put_batch(staged.objects, self.class).await?;
        }
        self.warm_cache(reachable.decoded_nodes).await;
        self.warm_values(values_to_warm).await;
        Ok(new_root)
    }

    // ---------------------------------------------------------------- loading

    /// Load and validate one node. On a cache miss the raw bytes are hashed and compared with the
    /// requested id *before* decoding, under [`VerifyPolicy::Always`].
    #[doc(hidden)]
    pub async fn view(&self, id: BlockId) -> Result<Arc<NodeView>, TreeError> {
        let mut b = BudgetState::new(self.budget);
        self.load(id, AccessHint::Random, &mut b).await
    }

    /// The hint a wave of nodes at `expect_level` deserves. Intent flows down: an internal level is
    /// metadata a store should prefer to keep resident, while the leaf level of an ordered walk is
    /// sequential. `None` is the root, whose kind is not yet known.
    fn descent_hint(expect_level: Option<u16>, ordered: bool) -> AccessHint {
        match expect_level {
            Some(0) if ordered => AccessHint::SequentialPrefetch,
            Some(0) => AccessHint::Random,
            Some(_) => AccessHint::MetadataOnly,
            None => AccessHint::Random,
        }
    }

    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    async fn load(
        &self,
        id: BlockId,
        hint: AccessHint,
        budget: &mut BudgetState,
    ) -> Result<Arc<NodeView>, TreeError> {
        budget.visit(1)?;
        if let Some(v) = self.cache.get(&id).await {
            self.metrics.cache_hit();
            return Ok(v);
        }
        self.metrics.cache_miss();
        self.load_coalesced(id, hint, budget).await
    }

    /// Fetch one object, sharing the work with any concurrent caller that wants the same id.
    ///
    /// The byte budget is charged from the *observed* size afterwards rather than inside the loader, so a
    /// caller that piggybacks on someone else's fetch is still charged for the bytes it consumed.
    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    async fn load_coalesced(
        &self,
        id: BlockId,
        hint: AccessHint,
        budget: &mut BudgetState,
    ) -> Result<Arc<NodeView>, TreeError> {
        if self.fmt.node_bytes() as u64 > budget.remaining_bytes() {
            return Err(TreeError::ResourceLimit {
                what: "bytes fetched",
            });
        }
        let claim = self.node_flights.claim(&[id]);
        if claim.owned_ids().next().is_some() {
            let result = if let Some(view) = self.cache.get(&id).await {
                Ok(view)
            } else {
                self.metrics.wave(1);
                match self.store.get(id, hint, self.fmt.max_object_bytes()).await {
                    Ok(bytes) => {
                        self.metrics.object_fetched();
                        self.metrics.bytes_read(bytes.len() as u64);
                        match self.decode_verified(id, bytes) {
                            Ok(view) => {
                                let view = Arc::new(view);
                                self.cache.insert(id, view.clone()).await;
                                Ok(view)
                            }
                            Err(error) => Err(error),
                        }
                    }
                    Err(error) => Err(error),
                }
            };
            claim.complete(id, result);
        }
        let v = claim.wait(id).await?;
        budget.spend_bytes(v.bytes().len() as u64)?;
        Ok(v)
    }

    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    fn decode_verified(&self, id: BlockId, bytes: Bytes) -> Result<NodeView, TreeError> {
        // Bound before decode: the store is asked for a limit, and the tree checks the answer too.
        if bytes.len() > self.fmt.max_object_bytes() {
            return Err(TreeError::decode(
                Some(id),
                DecodeError::OversizeObject {
                    found: bytes.len(),
                    limit: self.fmt.max_object_bytes(),
                },
            ));
        }
        if self.verify == VerifyPolicy::Always {
            let actual = BlockId::of(&bytes);
            self.metrics.bytes_verified(bytes.len() as u64);
            if actual != id {
                return Err(TreeError::HashMismatch {
                    requested: id,
                    actual,
                });
            }
        }
        NodeView::decode(&self.fmt, Some(id), bytes)
    }

    /// Load a whole frontier in ONE `get_many` wave. Cache hits skip the fetch entirely; ids are
    /// deduplicated because content addressing lets one subtree appear under several parents.
    async fn load_wave(
        &self,
        ids: &[BlockId],
        hint: AccessHint,
        budget: &mut BudgetState,
    ) -> Result<Wave, TreeError> {
        self.wave(ids, hint, budget, true).await
    }

    async fn wave_sorted_ids(
        &self,
        ids: &[BlockId],
        hint: AccessHint,
        budget: &mut BudgetState,
        charge_visits: bool,
    ) -> Result<(Wave, WaveScratch), TreeError> {
        let mut scratch = self
            .wave_scratch
            .lock()
            .expect("wave scratch pool lock")
            .pop()
            .unwrap_or_default();
        let wave = self
            .wave_cached(ids, true, hint, budget, charge_visits, &mut scratch)
            .await;
        match wave {
            Ok(wave) => Ok((wave, scratch)),
            Err(error) => {
                self.wave_scratch
                    .lock()
                    .expect("wave scratch pool lock")
                    .push(scratch);
                Err(error)
            }
        }
    }

    /// Breadth-first prefetch from `roots`, one batched read per level, stopping after `width` objects.
    ///
    /// This is what makes an ordered cursor's I/O batched rather than serial. A cursor is depth-first by
    /// nature — that is what bounds its memory — so batching only one parent's children still costs about
    /// one round trip per internal node. Expanding breadth-first under an explicit object budget gets the
    /// round trips down to roughly `nodes / width` while keeping the memory bound explicit rather than
    /// "the whole subtree".
    async fn prefetch_ahead(
        &self,
        roots: Vec<BlockId>,
        ranges: &RangeSet,
        width: usize,
        budget: &mut BudgetState,
    ) {
        let mut frontier = roots;
        let mut fetched = 0usize;
        while !frontier.is_empty() && fetched < width {
            frontier.truncate(width - fetched);
            fetched += frontier.len();
            let Ok(wave) = self
                .wave(&frontier, AccessHint::SequentialPrefetch, budget, false)
                .await
            else {
                return;
            };
            // Expand only internal nodes, and only into children still in scope.
            let mut next = Vec::new();
            for id in &frontier {
                let Some(v) = wave.get(id) else { continue };
                if v.is_leaf() {
                    continue;
                }
                for i in 0..v.child_count() {
                    let (clo, chi) = v.child_range(i);
                    let child = KeyRange::new(
                        clo.map(Bytes::copy_from_slice),
                        chi.map(Bytes::copy_from_slice),
                    );
                    if overlaps_scope(ranges, &child) {
                        next.push(v.child(i));
                    }
                }
            }
            frontier = next;
        }
    }

    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    async fn wave(
        &self,
        ids: &[BlockId],
        hint: AccessHint,
        budget: &mut BudgetState,
        charge_visits: bool,
    ) -> Result<Wave, TreeError> {
        let mut scratch = self
            .wave_scratch
            .lock()
            .expect("wave scratch pool lock")
            .pop()
            .unwrap_or_default();
        let wave = self
            .wave_cached(ids, false, hint, budget, charge_visits, &mut scratch)
            .await;
        self.wave_scratch
            .lock()
            .expect("wave scratch pool lock")
            .push(scratch);
        wave
    }

    async fn wave_cached(
        &self,
        ids: &[BlockId],
        ids_are_sorted: bool,
        hint: AccessHint,
        budget: &mut BudgetState,
        charge_visits: bool,
        scratch: &mut WaveScratch,
    ) -> Result<Wave, TreeError> {
        scratch.reset();
        let out = &mut scratch.entries;
        if ids.is_empty() {
            return Ok(Wave {
                entries: std::mem::take(out),
            });
        }
        if !ids_are_sorted && ids.len() > 1 {
            scratch.ids.extend_from_slice(ids);
            scratch.ids.sort_unstable();
            scratch.ids.dedup();
        } else {
            scratch.ids.extend_from_slice(ids);
        }
        for id in &scratch.ids {
            // Object-visit budget is charged for hits too, so sharing cannot buy unbounded CPU work.
            if charge_visits {
                budget.visit(1)?;
            }
            match self.cache.get(id).await {
                Some(v) => {
                    if charge_visits {
                        self.metrics.cache_hit();
                    }
                    out.push((*id, v));
                }
                None => {
                    if charge_visits {
                        self.metrics.cache_miss();
                    }
                    scratch.misses.push(*id);
                }
            }
        }
        if scratch.misses.is_empty() {
            return Ok(Wave {
                entries: std::mem::take(out),
            });
        }
        if scratch.misses.len() == 1 {
            let id = scratch.misses[0];
            let v = self.load_coalesced(id, hint, budget).await?;
            out.push((id, v));
            out.sort_unstable_by_key(|(id, _)| *id);
            return Ok(Wave {
                entries: std::mem::take(out),
            });
        }
        // Early rejection, as for values: every node is exactly `node_bytes`, so the wave's cost is known
        // before the read.
        let declared = (self.fmt.node_bytes() as u64)
            .checked_mul(scratch.misses.len() as u64)
            .ok_or(TreeError::ResourceLimit {
                what: "bytes fetched",
            })?;
        if declared > budget.remaining_bytes() {
            return Err(TreeError::ResourceLimit {
                what: "bytes fetched",
            });
        }
        let allowance = budget.remaining_bytes();
        let claim = self.node_flights.claim(&scratch.misses);
        // A flight can finish and leave the map between the initial cache miss and `claim`. Recheck
        // every newly owned id so that narrow race cannot turn a completed fill into a duplicate fetch.
        let fetch = &mut scratch.fetch;
        for id in claim.owned_ids() {
            if let Some(view) = self.cache.get(&id).await {
                claim.complete(id, Ok(view));
            } else {
                fetch.push(id);
            }
        }
        if !fetch.is_empty() {
            self.metrics.wave(1);
            let fetched = self
                .store
                .get_many(fetch, hint, self.fmt.max_object_bytes(), allowance)
                .await;
            if fetched.len() != fetch.len() {
                let error = TreeError::decode(
                    None,
                    DecodeError::BatchCardinality {
                        found: fetched.len(),
                        expected: fetch.len(),
                    },
                );
                for id in fetch.iter() {
                    claim.complete(*id, Err(error.clone()));
                }
            } else {
                for (id, result) in fetch.iter().zip(fetched) {
                    let result = match result {
                        Ok(bytes) => {
                            self.metrics.object_fetched();
                            self.metrics.bytes_read(bytes.len() as u64);
                            match self.decode_verified(*id, bytes) {
                                Ok(view) => {
                                    let view = Arc::new(view);
                                    self.cache.insert(*id, view.clone()).await;
                                    Ok(view)
                                }
                                Err(error) => Err(error),
                            }
                        }
                        Err(error) => Err(error),
                    };
                    claim.complete(*id, result);
                }
            }
        }
        for id in scratch.misses.drain(..) {
            let v = claim.wait(id).await?;
            budget.spend_bytes(v.bytes().len() as u64)?;
            out.push((id, v));
        }
        // Cache hits and newly fetched misses are appended by different paths; restore the ordering
        // required by `Wave::get` before exposing the wave.
        out.sort_unstable_by_key(|(id, _)| *id);
        Ok(Wave {
            entries: std::mem::take(out),
        })
    }

    /// One batched wave for out-of-line winners. Each envelope must agree with the
    /// `external_value_len` its referencing node authenticated.
    async fn load_values(
        &self,
        refs: &[(BlockId, u32)],
        budget: &mut BudgetState,
    ) -> Result<Vec<Bytes>, TreeError> {
        if refs.is_empty() {
            return Ok(Vec::new());
        }
        // Deduplicate and consult the value cache first. Passing duplicate ids straight to `get_many`
        // fetched one shared value once per reference — a value shared by N keys cost N fetches.
        let mut unique: Vec<(BlockId, u32)> = Vec::new();
        let mut slot: Vec<usize> = Vec::with_capacity(refs.len());
        let mut index: foldhash::HashMap<BlockId, usize> = Default::default();
        for (id, len) in refs {
            budget.visit(1)?;
            match index.get(id) {
                Some(&i) => slot.push(i),
                None => {
                    let i = unique.len();
                    index.insert(*id, i);
                    slot.push(i);
                    unique.push((*id, *len));
                }
            }
        }
        let resolved = self.load_values_deduplicated(&unique, budget).await?;
        // The cache is keyed by content id, but the logical length is authenticated independently by
        // every referencing node. Validate *each* reference after deduplication, including cache hits;
        // otherwise a correct first length can accidentally bless an inconsistent second reference.
        refs.iter()
            .zip(slot)
            .map(|((id, len), i)| {
                value::validate_reference_len(*id, &resolved[i], *len)?;
                Ok(resolved[i].clone())
            })
            .collect()
    }

    async fn load_values_deduplicated(
        &self,
        refs: &[(BlockId, u32)],
        budget: &mut BudgetState,
    ) -> Result<Vec<Bytes>, TreeError> {
        if refs.is_empty() {
            return Ok(Vec::new());
        }
        let mut cached: Vec<Option<Bytes>> = Vec::with_capacity(refs.len());
        let mut missing: Vec<(BlockId, u32)> = Vec::new();
        for (id, len) in refs {
            budget.visit(1)?;
            match self.values.get(id).await {
                Some(payload) => {
                    self.metrics.cache_hit();
                    cached.push(Some(payload));
                }
                None => {
                    missing.push((*id, *len));
                    cached.push(None);
                }
            }
        }
        let fetched_unique = self.fetch_values_coalesced(&missing, budget).await?;
        let mut fetched = fetched_unique.into_iter();
        Ok(cached
            .into_iter()
            .map(|payload| match payload {
                Some(payload) => payload,
                None => fetched.next().expect("one fetch per miss"),
            })
            .collect())
    }

    async fn load_values_deduplicated_cached(
        &self,
        budget: &mut BudgetState,
        scratch: &mut MaterializeScratch,
    ) -> Result<(), TreeError> {
        if scratch.refs.is_empty() {
            return Ok(());
        }
        scratch.cached.clear();
        scratch.cached.reserve(scratch.refs.len());
        scratch.missing.clear();
        scratch.missing.reserve(scratch.refs.len());
        for (id, len) in scratch.refs.iter() {
            budget.visit(1)?;
            match self.values.get(id).await {
                Some(payload) => {
                    self.metrics.cache_hit();
                    scratch.cached.push(Some(payload));
                }
                None => {
                    scratch.missing.push((*id, *len));
                    scratch.cached.push(None);
                }
            }
        }
        if scratch.missing.is_empty() {
            return Ok(());
        }
        let mut missing = self
            .fetch_values_coalesced(&scratch.missing, budget)
            .await?
            .into_iter();
        for entry in scratch.cached.iter_mut() {
            if entry.is_none() {
                *entry =
                    Some(missing.next().ok_or_else(|| {
                        TreeError::store("fetched fewer values than cache misses")
                    })?);
            }
        }
        if missing.next().is_none() {
            Ok(())
        } else {
            Err(TreeError::store("fetched more values than cache misses"))
        }
    }

    async fn fetch_values_coalesced(
        &self,
        refs: &[(BlockId, u32)],
        budget: &mut BudgetState,
    ) -> Result<Vec<Bytes>, TreeError> {
        if refs.is_empty() {
            return Ok(Vec::new());
        }
        // Early rejection: if the objects these references *declare* already exceed the operation's
        // budget, fail before issuing the read.
        let declared = refs
            .iter()
            .try_fold(0u64, |sum, (_, len)| {
                sum.checked_add(u64::from(*len) + value::ENVELOPE_BYTES as u64)
            })
            .ok_or(TreeError::ResourceLimit {
                what: "bytes fetched",
            })?;
        if declared > budget.remaining_bytes() {
            return Err(TreeError::ResourceLimit {
                what: "bytes fetched",
            });
        }
        // The aggregate allowance handed to the store is the operation's REMAINING budget, per the
        // contract — not `declared`. Capping the store at the declared total would turn a *substituted*
        // value object (a larger valid object served for a smaller reference) into a resource-limit
        // error, hiding the length mismatch that actually explains it. Substitution is caught by each
        // reference's authenticated length instead, which is the diagnostic that tells an operator this
        // is bit rot or a lying store rather than a budget that was set too low.
        let allowance = budget.remaining_bytes();

        let ids: Vec<BlockId> = refs.iter().map(|(id, _)| *id).collect();
        let claim = self.value_flights.claim(&ids);
        let mut fetch = Vec::new();
        for id in claim.owned_ids().collect::<Vec<_>>() {
            if let Some(payload) = self.values.get(&id).await {
                claim.complete(id, Ok(payload));
            } else {
                fetch.push(id);
            }
        }
        if !fetch.is_empty() {
            self.metrics.wave(1);
            let fetched = self
                .store
                .get_many(
                    &fetch,
                    AccessHint::Random,
                    self.fmt.max_object_bytes(),
                    allowance,
                )
                .await;
            if fetched.len() != fetch.len() {
                let error = TreeError::decode(
                    None,
                    DecodeError::BatchCardinality {
                        found: fetched.len(),
                        expected: fetch.len(),
                    },
                );
                for id in &fetch {
                    claim.complete(*id, Err(error.clone()));
                }
            } else {
                for (id, result) in fetch.iter().zip(fetched) {
                    let result = match result {
                        Ok(bytes) => {
                            self.metrics.object_fetched();
                            self.metrics.bytes_read(bytes.len() as u64);
                            let decoded = if bytes.len() > self.fmt.max_object_bytes() {
                                Err(TreeError::decode(
                                    Some(*id),
                                    DecodeError::OversizeObject {
                                        found: bytes.len(),
                                        limit: self.fmt.max_object_bytes(),
                                    },
                                ))
                            } else if self.verify == VerifyPolicy::Always {
                                let actual = BlockId::of(&bytes);
                                self.metrics.bytes_verified(bytes.len() as u64);
                                if actual != *id {
                                    Err(TreeError::HashMismatch {
                                        requested: *id,
                                        actual,
                                    })
                                } else {
                                    value::decode_payload(*id, &bytes, self.fmt.schema_id())
                                }
                            } else {
                                value::decode_payload(*id, &bytes, self.fmt.schema_id())
                            };
                            match decoded {
                                Ok(payload) => {
                                    self.values.insert(*id, payload.clone()).await;
                                    Ok(payload)
                                }
                                Err(error) => Err(error),
                            }
                        }
                        Err(error) => Err(error),
                    };
                    claim.complete(*id, result);
                }
            }
        }
        let mut out = Vec::with_capacity(refs.len());
        for (id, _) in refs {
            let payload = claim.wait(*id).await?;
            budget.spend_bytes((payload.len() + value::ENVELOPE_BYTES) as u64)?;
            out.push(payload);
        }
        Ok(out)
    }

    // ---------------------------------------------------------------- writing

    /// Normalize one caller batch: collapse repeated keys to the **last** mutation in program order,
    /// attach the batch stamp, externalize large values, then sort. Every limit is checked here, before
    /// any object is stored.
    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    fn normalize(
        &self,
        stamp: VersionStamp,
        mutations: Vec<Mutation>,
        staged: &mut Staged,
    ) -> Result<Vec<Entry>, TreeError> {
        let mut order: Vec<Bytes> = Vec::with_capacity(mutations.len());
        let mut last: foldhash::HashMap<Bytes, MutationOp> =
            foldhash::HashMap::with_capacity_and_hasher(mutations.len(), Default::default());
        for m in mutations {
            self.fmt.check_key(&m.key)?;
            if let MutationOp::Upsert(v) = &m.op {
                self.fmt.check_value(v)?;
            }
            if last.insert(m.key.clone(), m.op).is_none() {
                order.push(m.key);
            }
        }
        let mut entries = Vec::with_capacity(order.len());
        // `Bytes::clone` preserves the same immutable slice. Memoize by that live slice identity before
        // allocating an envelope or hashing it: one 4 MiB `Bytes` shared by 256 keys must be read and
        // hashed once, not 256 times. Separately allocated equal payloads still require hashing to prove
        // equality; the addressed staging set deduplicates their eventual store submission.
        let mut encoded_values: foldhash::HashMap<(usize, usize), value::ValueObject> =
            Default::default();
        for key in order {
            let op = last.remove(&key).expect("inserted above");
            entries.push(match op {
                MutationOp::Tombstone => Entry::tombstone(key, stamp.order_key),
                MutationOp::Upsert(v) if self.fmt.is_inline(v.len()) => {
                    self.metrics.inline_value(v.len() as u64);
                    Entry::inline(key, stamp.order_key, v)
                }
                MutationOp::Upsert(v) => {
                    let identity = (v.as_ptr() as usize, v.len());
                    let obj = match encoded_values.get(&identity) {
                        Some(obj) => obj.clone(),
                        None => {
                            let obj = value::encode(self.fmt.schema_id(), &v);
                            self.metrics.value_object_encoded(obj.bytes.len() as u64);
                            encoded_values.insert(identity, obj.clone());
                            obj
                        }
                    };
                    if obj.bytes.len() > self.fmt.max_object_bytes() {
                        return Err(TreeError::Capacity(CapacityError::ObjectTooLarge {
                            len: obj.bytes.len(),
                            limit: self.fmt.max_object_bytes(),
                        }));
                    }
                    self.metrics.external_value(v.len() as u64);
                    // The value object is in the same durable batch as the nodes referencing it.
                    // `value::encode` already hashed these bytes to address them, so reuse that id
                    // rather than hashing the whole payload a second time.
                    staged.value(&obj);
                    Entry::external(key, stamp.order_key, obj.id, obj.logical_len)
                }
            });
        }
        entries.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(entries)
    }

    /// Merge two sorted runs, retaining the entry with the greater winner tuple. Equal stamps compare
    /// their exact `operation_tiebreak`; byte-identical operations are idempotent. No persisted tie
    /// depends on queue position, tree level, traversal order, or which replica merged.
    fn merge_runs(mut a: Vec<Entry>, b: Vec<Entry>) -> Vec<Entry> {
        if a.is_empty() {
            return b;
        }
        if b.is_empty() {
            return a;
        }
        let mut out = Vec::with_capacity(a.len() + b.len());
        let mut ia = a.drain(..).peekable();
        let mut ib = b.into_iter().peekable();
        loop {
            match (ia.peek(), ib.peek()) {
                (Some(x), Some(y)) => match x.key.cmp(&y.key) {
                    std::cmp::Ordering::Less => out.push(ia.next().expect("peeked")),
                    std::cmp::Ordering::Greater => out.push(ib.next().expect("peeked")),
                    std::cmp::Ordering::Equal => {
                        let (x, y) = (ia.next().expect("peeked"), ib.next().expect("peeked"));
                        out.push(if y.winner() > x.winner() { y } else { x });
                    }
                },
                (Some(_), None) => out.push(ia.next().expect("peeked")),
                (None, Some(_)) => out.push(ib.next().expect("peeked")),
                (None, None) => break,
            }
        }
        out
    }

    fn blob_of(&self, entries: &[Entry]) -> usize {
        entries.iter().map(Entry::blob_len).sum()
    }

    fn accounted_of(&self, entries: &[Entry]) -> usize {
        entries
            .len()
            .saturating_mul(self.fmt.desc_bytes())
            .saturating_add(self.blob_of(entries))
    }

    /// Which child owns `key`, given the *current* pivots. Recomputed after every splice, because a
    /// split shifts indices.
    fn child_of(&self, pivots: &[Bytes], key: &[u8]) -> usize {
        pivots.partition_point(|p| p.as_ref() <= key)
    }

    /// The shortest prefix of `right` that remains strictly above `left`. A separator need not repeat
    /// the complete minimum key of its right child; this is what keeps unrelated 4 KiB keys from
    /// consuming 4 KiB in every pivot lane.
    fn shortest_separator(left: &[u8], right: &[u8]) -> Bytes {
        debug_assert!(left < right);
        let shared = left.iter().zip(right).take_while(|(a, b)| a == b).count();
        Bytes::copy_from_slice(&right[..(shared + 1).min(right.len())])
    }

    /// Split a sorted run into leaves: take the longest non-empty prefix that fits both the slot and
    /// the blob limit, then repeat. Deterministic; a final sparse leaf is permitted.
    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    fn pack_leaves(&self, entries: Vec<Entry>) -> Vec<(Option<Bytes>, Vec<Entry>)> {
        if entries.is_empty() {
            return vec![(None, Vec::new())];
        }
        let mut out: Vec<(Option<Bytes>, Vec<Entry>)> = Vec::new();
        let mut cur: Vec<Entry> = Vec::new();
        let mut cur_lo: Option<Bytes> = None;
        let mut raw_blob = 0usize;
        for e in entries {
            let next_raw = raw_blob + e.blob_len();
            let next_count = cur.len() + 1;
            let skip = if self.fmt.compresses_pivots() && next_count > 1 {
                crate::search::head_skip(
                    cur.first().map(|entry| entry.key.as_ref()),
                    Some(&e.key),
                    next_count,
                ) as usize
            } else {
                0
            };
            let compressed_blob = next_raw - next_count * skip + skip;
            if !cur.is_empty()
                && !self
                    .fmt
                    .leaf_entries_fit(next_count, next_raw, compressed_blob)
            {
                let separator =
                    Self::shortest_separator(&cur.last().expect("nonempty").key, &e.key);
                out.push((cur_lo.take(), std::mem::take(&mut cur)));
                cur_lo = Some(separator);
                raw_blob = 0;
            }
            raw_blob += e.blob_len();
            cur.push(e);
        }
        if !cur.is_empty() {
            out.push((cur_lo, cur));
        }
        out
    }

    /// Contiguous groups bounded by both child lanes and compressed pivot bytes. The selected format
    /// therefore retains fanout 32 for ordinary/shared-prefix keys while pathological unrelated long
    /// separators reduce only the affected node's realized fanout.
    fn group_sizes_for_pivots(&self, pivots: &[Bytes]) -> Vec<usize> {
        let n = pivots.len() + 1;
        if n <= self.fmt.f_max() && self.fmt.pivots_fit(pivots) {
            return vec![n];
        }
        let mut out = Vec::new();
        let mut at = 0usize;
        while at < n {
            let remaining = n - at;
            if remaining <= 3 {
                // Format construction proves two maximum-size pivots fit, so a final 2- or 3-child
                // node is always representable.
                out.push(remaining);
                break;
            }
            let mut size = 2usize;
            while size < remaining && size < self.fmt.f_max() {
                let candidate = size + 1;
                if !self.fmt.pivots_fit(&pivots[at..at + candidate - 1]) {
                    break;
                }
                size = candidate;
            }
            if remaining - size == 1 {
                size -= 1;
            }
            out.push(size);
            at += size;
        }
        debug_assert_eq!(out.iter().sum::<usize>(), n);
        debug_assert!(out.iter().all(|&size| size >= 2));
        out
    }

    fn write_node<'a>(
        &'a self,
        id: BlockId,
        msgs: Vec<Entry>,
        staged: &'a mut Staged,
        budget: &'a mut BudgetState,
    ) -> Fut<'a, Rewrite> {
        Box::pin(async move {
            // A COW rewrite is a random walk down one path, not an ordered sweep.
            let view = self.load(id, AccessHint::Random, budget).await?;
            if view.is_leaf() {
                let existing: Vec<Entry> = view.entries().collect();
                let merged = Self::merge_runs(existing, msgs);
                self.emit_leaves(merged, staged)
            } else {
                self.write_internal(&view, msgs, staged, budget).await
            }
        })
    }

    fn emit_leaves(&self, entries: Vec<Entry>, staged: &mut Staged) -> Result<Rewrite, TreeError> {
        let packed = self.pack_leaves(entries);
        self.metrics.leaves_written(packed.len() as u64);
        let mut layer = Vec::with_capacity(packed.len());
        for (_, es) in &packed {
            layer.push(codec::encode_leaf(&self.fmt, es)?);
        }
        let ids = staged.layer(layer);
        Ok(Rewrite {
            first: ids[0],
            following: packed
                .into_iter()
                .zip(&ids)
                .skip(1)
                .map(|((min, _), id)| (min.expect("only the first run has no min key"), *id))
                .collect(),
            level: 0,
        })
    }

    fn write_internal<'a>(
        &'a self,
        view: &'a NodeView,
        msgs: Vec<Entry>,
        staged: &'a mut Staged,
        budget: &'a mut BudgetState,
    ) -> Fut<'a, Rewrite> {
        Box::pin(async move {
            let children: Vec<BlockId> = view.children().collect();
            let pivots: Vec<Bytes> = (0..view.pivot_count())
                .map(|i| view.pivot_bytes(i))
                .collect();
            let existing: Vec<Entry> = view.entries().collect();

            // A mutation whose accounted message cannot fit an EMPTY regular buffer is routed directly
            // toward its leaf: buffering is sacrificed for it rather than creating a second node shape.
            let (direct, ordinary): (Vec<Entry>, Vec<Entry>) = msgs
                .into_iter()
                .partition(|e| self.fmt.message_is_oversized(e.key.len(), e.span.len()));
            self.metrics.direct_routed(direct.len() as u64);

            let node = InternalBuilder {
                level: view.tree_level(),
                children,
                pivots,
                buffer: Self::merge_runs(existing, ordinary),
                direct,
            };
            self.finish_internal(node, staged, budget).await
        })
    }

    /// Flush until the transient builder is within regular capacity, then serialize — or partition if
    /// integration pushed it past `F_MAX`. The builder may hold more than `F_MAX` children transiently;
    /// an overfull node is never serialized.
    fn finish_internal<'a>(
        &'a self,
        mut node: InternalBuilder,
        staged: &'a mut Staged,
        budget: &'a mut BudgetState,
    ) -> Fut<'a, Rewrite> {
        Box::pin(async move {
            loop {
                // 1. Directly routed oversized messages first. They do NOT participate in the flush
                //    lower bound, so they are handled separately from buffer accounting.
                if !node.direct.is_empty() {
                    let victim = self.child_of(&node.pivots, &node.direct[0].key);
                    let mut group = Vec::new();
                    let mut rest = Vec::with_capacity(node.direct.len());
                    for e in node.direct {
                        if self.child_of(&node.pivots, &e.key) == victim {
                            group.push(e);
                        } else {
                            rest.push(e);
                        }
                    }
                    node.direct = rest;
                    let rw = self
                        .write_node(node.children[victim], group, staged, budget)
                        .await?;
                    self.check_child_level(node.level, rw.level)?;
                    node.splice(victim, rw);
                    if node.needs_partition(&self.fmt) {
                        return self.partition(node, staged, budget).await;
                    }
                    continue;
                }

                // 2. Ordinary fullness: either the slot capacity or the encoded blob region.
                if self
                    .fmt
                    .buffer_fits(node.buffer.len(), self.blob_of(&node.buffer))
                {
                    break;
                }

                let mut by_child: Vec<Vec<Entry>> = vec![Vec::new(); node.children.len()];
                for e in node.buffer.drain(..) {
                    let i = self.child_of(&node.pivots, &e.key);
                    by_child[i].push(e);
                }
                let weights: Vec<usize> = by_child.iter().map(|g| self.accounted_of(g)).collect();
                // Greatest pending encoded byte count; lowest child index is the deterministic
                // tie-break.
                let victim = (0..weights.len())
                    .max_by_key(|&i| (weights[i], std::cmp::Reverse(i)))
                    .expect("an internal node has children");
                let total: usize = weights.iter().sum();
                self.metrics.flush(
                    total as u64,
                    weights[victim] as u64,
                    node.children.len() as u64,
                );
                let floor = (total.div_ceil(node.children.len())).max(self.fmt.min_flush_bytes());
                if weights[victim] < floor {
                    self.metrics.undersized_flush();
                    debug_assert!(
                        false,
                        "flush victim {} bytes is below the guaranteed floor {floor}",
                        weights[victim]
                    );
                }

                let group = std::mem::take(&mut by_child[victim]);
                node.buffer = by_child.into_iter().flatten().collect();
                debug_assert!(node.buffer.windows(2).all(|w| w[0].key < w[1].key));

                let rw = self
                    .write_node(node.children[victim], group, staged, budget)
                    .await?;
                self.check_child_level(node.level, rw.level)?;
                node.splice(victim, rw);
                if node.needs_partition(&self.fmt) {
                    return self.partition(node, staged, budget).await;
                }
            }

            self.metrics
                .internal_written(node.children.len() as u64, node.buffer.len() as u64);
            let bytes = codec::encode_internal(
                &self.fmt,
                node.level,
                &node.pivots,
                &node.children,
                &node.buffer,
            )?;
            Ok(Rewrite::single(staged.one(bytes), node.level))
        })
    }

    /// A parent may only accept a replacement run whose nodes sit exactly one level below it.
    fn check_child_level(&self, parent: u16, child: u16) -> Result<(), TreeError> {
        if parent != child + 1 {
            return Err(TreeError::decode(
                None,
                DecodeError::ChildLevel { child, parent },
            ));
        }
        Ok(())
    }

    /// Partition an overfull internal builder into contiguous groups, partitioning the buffer at every
    /// promoted pivot. This is required by the Bε path invariant: every message for key `k` must remain
    /// on `k`'s root-to-leaf path.
    fn partition<'a>(
        &'a self,
        node: InternalBuilder,
        staged: &'a mut Staged,
        budget: &'a mut BudgetState,
    ) -> Fut<'a, Rewrite> {
        Box::pin(async move {
            let sizes = self.group_sizes_for_pivots(&node.pivots);
            self.metrics.internal_partition(sizes.len() as u64);
            let mut runs: Vec<(Option<Bytes>, Rewrite)> = Vec::with_capacity(sizes.len());
            let mut at = 0usize;
            // Buffers are partitioned by the promoted pivots, so each output receives exactly the
            // messages in its child-key range.
            let mut buffer = VecDeque::from(node.buffer);
            let mut direct = VecDeque::from(node.direct);
            for (g, size) in sizes.iter().copied().enumerate() {
                let end = at + size;
                let group_lo: Option<Bytes> = at.checked_sub(1).map(|j| node.pivots[j].clone());
                let group_hi: Option<Bytes> =
                    (end - 1 < node.pivots.len()).then(|| node.pivots[end - 1].clone());
                let take = |q: &mut VecDeque<Entry>, hi: &Option<Bytes>| -> Vec<Entry> {
                    let mut out = Vec::new();
                    while let Some(front) = q.front() {
                        let inside = match hi {
                            Some(h) => front.key < *h,
                            None => true,
                        };
                        if !inside {
                            break;
                        }
                        out.push(q.pop_front().expect("peeked"));
                    }
                    out
                };
                let local_buffer = take(&mut buffer, &group_hi);
                let local_direct = take(&mut direct, &group_hi);
                let local_children = node.children[at..end].to_vec();
                let local_pivots = node.pivots[at..end - 1].to_vec();
                let rw = self
                    .finish_internal(
                        InternalBuilder {
                            level: node.level,
                            children: local_children,
                            pivots: local_pivots,
                            buffer: local_buffer,
                            direct: local_direct,
                        },
                        staged,
                        budget,
                    )
                    .await?;
                runs.push((if g == 0 { None } else { group_lo }, rw));
                at = end;
            }
            debug_assert!(buffer.is_empty() && direct.is_empty());

            // Concatenate the group runs into one ordered replacement run.
            let mut it = runs.into_iter();
            let (_, head) = it.next().expect("at least one group");
            let mut following = head.following;
            for (min, rw) in it {
                following.push((
                    min.expect("only the first group has no promoted pivot"),
                    rw.first,
                ));
                following.extend(rw.following);
            }
            Ok(Rewrite {
                first: head.first,
                following,
                level: node.level,
            })
        })
    }

    /// If a root rewrite produced more than one node, build new internal levels from the replacement run
    /// until one root remains. A sufficiently large commit may therefore grow the tree by more than one
    /// level without ever encoding an over-capacity node.
    fn grow_root(&self, mut rw: Rewrite, staged: &mut Staged) -> Result<BlockId, TreeError> {
        while !rw.following.is_empty() {
            let level = u32::from(rw.level) + 1;
            if level > u32::from(self.fmt.max_tree_level()) {
                return Err(TreeError::Capacity(CapacityError::TreeTooTall {
                    level,
                    limit: self.fmt.max_tree_level(),
                }));
            }
            let level = level as u16;
            let ids = rw.ids();
            let mins: Vec<Option<Bytes>> = std::iter::once(None)
                .chain(rw.following.iter().map(|(k, _)| Some(k.clone())))
                .collect();
            let root_pivots: Vec<Bytes> = mins.iter().skip(1).flatten().cloned().collect();
            let sizes = self.group_sizes_for_pivots(&root_pivots);
            let mut layer = Vec::with_capacity(sizes.len());
            let mut group_mins: Vec<Option<Bytes>> = Vec::with_capacity(sizes.len());
            let mut at = 0usize;
            for size in sizes {
                let end = at + size;
                let group_children = &ids[at..end];
                let group_pivots: Vec<Bytes> = mins[at + 1..end]
                    .iter()
                    .map(|m| m.clone().expect("only index 0 lacks a min key"))
                    .collect();
                layer.push(codec::encode_internal(
                    &self.fmt,
                    level,
                    &group_pivots,
                    group_children,
                    &[],
                )?);
                group_mins.push(mins[at].clone());
                at = end;
            }
            self.metrics.root_grown();
            let new_ids = staged.layer(layer);
            rw = Rewrite {
                first: new_ids[0],
                following: new_ids
                    .iter()
                    .zip(group_mins)
                    .skip(1)
                    .map(|(id, min)| (min.expect("non-first groups have a min key"), *id))
                    .collect(),
                level,
            };
        }
        Ok(rw.first)
    }

    /// Insert freshly written nodes into the decoded cache. They are already known-canonical, but they
    /// are re-validated here rather than trusted: the cost is one decode, and it makes an encoder bug a
    /// loud local failure instead of a corrupt cached view. A node that somehow fails is simply not
    /// cached, never fatal — the store has the authoritative bytes.
    async fn warm_cache(&self, nodes: Vec<(BlockId, Arc<NodeView>)>) {
        for (id, view) in nodes {
            if self.cache.contains_key(&id) {
                continue;
            }
            self.metrics.cache_warmed();
            self.cache.insert(id, view).await;
        }
    }

    async fn warm_values(&self, values: Vec<(BlockId, Bytes)>) {
        for (id, payload) in values {
            if !self.values.contains_key(&id) {
                self.values.insert(id, payload).await;
            }
        }
    }

    // ---------------------------------------------------------------- reading

    /// Resolve many keys to their winners in O(depth) dependent waves, grouping probes by node so each
    /// validated head surface is reused for every probe assigned to it.
    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    async fn resolve_many_cached(
        &self,
        root: BlockId,
        keys: &[&[u8]],
        budget: &mut BudgetState,
        scratch: &mut ResolveScratch,
    ) -> Result<(), TreeError> {
        scratch.reset(keys.len());
        if keys.is_empty() {
            return Ok(());
        }
        if keys.len() == 1 {
            return self
                .resolve_one_cached(root, keys[0], budget, scratch)
                .await;
        }
        // (probe index, node id, the level its parent claimed)
        scratch
            .frontier
            .extend((0..keys.len()).map(|i| (i, root, u16::MAX)));

        let frontier = &mut scratch.frontier;
        let mut frontier_is_sorted = true;

        for _ in 0..=u32::from(self.fmt.max_tree_level()) {
            let mut next_sorted = true;
            if frontier.is_empty() {
                break;
            }
            if !frontier_is_sorted {
                frontier.sort_unstable_by_key(|(_, id, _)| *id);
            }
            // Every probe descends in lockstep, so one frontier is one level and one hint.
            let hint =
                Self::descent_hint((frontier[0].2 != u16::MAX).then_some(frontier[0].2), false);

            scratch.ids.clear();
            for &(_, id, _) in frontier.iter() {
                if scratch.ids.last() != Some(&id) {
                    scratch.ids.push(id);
                }
            }

            let (wave, mut wave_scratch) = self
                .wave_sorted_ids(&scratch.ids, hint, budget, true)
                .await?;

            scratch.next.clear();
            scratch.next.reserve(frontier.len());
            let mut i = 0usize;
            while i < frontier.len() {
                let id = frontier[i].1;
                let expect = frontier[i].2;
                // Traversal rejects an edge unless the child's level is exactly one below its parent.
                let tree_level = wave.get(&id).expect("loaded in this wave").tree_level();
                if expect != u16::MAX && tree_level != expect {
                    let parent = expect + 1;
                    wave_scratch.entries = wave.entries;
                    self.wave_scratch
                        .lock()
                        .expect("wave scratch pool lock")
                        .push(wave_scratch);
                    return Err(TreeError::decode(
                        Some(id),
                        DecodeError::ChildLevel {
                            child: tree_level,
                            parent,
                        },
                    ));
                }

                let v = wave.get(&id).expect("loaded in this wave");
                let surface = v.entry_surface();
                let is_leaf = v.is_leaf();
                let pivot_surface = (!is_leaf).then(|| v.pivot_surface());
                let child_level = if is_leaf {
                    None
                } else {
                    let Some(level) = tree_level.checked_sub(1) else {
                        self.wave_scratch
                            .lock()
                            .expect("wave scratch pool lock")
                            .push(wave_scratch);
                        return Err(TreeError::decode(
                            Some(id),
                            DecodeError::ChildLevel {
                                child: tree_level,
                                parent: tree_level,
                            },
                        ));
                    };
                    Some(level)
                };
                let mut end = i + 1;
                while end < frontier.len() && frontier[end].1 == id {
                    end += 1;
                }

                for &(p, _, _) in &frontier[i..end] {
                    let key = keys[p];
                    let f = surface.probe(key);
                    self.metrics.probe(f.cost);
                    if f.exact {
                        let w = v.winner(f.index);
                        if scratch.best[p].as_ref().is_none_or(|b| w > *b) {
                            scratch.best[p] = Some(w);
                        }
                    } else if !is_leaf {
                        // Priced for a future routing summary: a probe that touched a buffer and found
                        // nothing is exactly the work a summary could have skipped.
                        self.metrics.absent_key_buffer_probe();
                    }
                    if let Some(ps) = &pivot_surface {
                        let child = ps.probe(key).upper_bound();
                        let child_id = v.child(child);
                        let child_level = child_level.unwrap();
                        if let Some(last_id) = scratch.next.last().map(|(_, child_id, _)| *child_id)
                            && child_id < last_id
                        {
                            next_sorted = false;
                        }
                        scratch.next.push((p, child_id, child_level));
                    }
                }

                i = end;
            }
            // `wave_cached` moves the entries vector into `Wave` so it can be searched while this
            // level is processed. Recycle that vector's capacity along with the remaining wave
            // scratch before the next dependent level.
            wave_scratch.entries = wave.entries;
            self.wave_scratch
                .lock()
                .expect("wave scratch pool lock")
                .push(wave_scratch);
            std::mem::swap(frontier, &mut scratch.next);
            frontier_is_sorted = next_sorted;
        }
        if !frontier.is_empty() {
            return Err(TreeError::ResourceLimit {
                what: "tree depth above max_tree_level",
            });
        }
        Ok(())
    }

    /// Scalar descent for a point read. The general grouped frontier is valuable once several keys
    /// share a wave, but it adds frontier construction and wave-result plumbing to the one-key case.
    /// This path keeps the same level, budget, hash, and structural validation contracts as grouped
    /// traversal while using the single-object cache/load fast path at each depth.
    async fn resolve_one_cached(
        &self,
        root: BlockId,
        key: &[u8],
        budget: &mut BudgetState,
        scratch: &mut ResolveScratch,
    ) -> Result<(), TreeError> {
        scratch.best.clear();
        scratch.best.resize(1, None);
        let mut id = root;
        let mut expect = u16::MAX;
        for _ in 0..=u32::from(self.fmt.max_tree_level()) {
            let hint = Self::descent_hint((expect != u16::MAX).then_some(expect), false);
            let view = self.load(id, hint, budget).await?;
            let tree_level = view.tree_level();
            if expect != u16::MAX && tree_level != expect {
                return Err(TreeError::decode(
                    Some(id),
                    DecodeError::ChildLevel {
                        child: tree_level,
                        parent: expect + 1,
                    },
                ));
            }
            let found = view.entry_surface().probe(key);
            self.metrics.probe(found.cost);
            if found.exact {
                let winner = view.winner(found.index);
                if scratch.best[0].as_ref().is_none_or(|best| winner > *best) {
                    scratch.best[0] = Some(winner);
                }
            } else if !view.is_leaf() {
                self.metrics.absent_key_buffer_probe();
            }
            if view.is_leaf() {
                return Ok(());
            }
            let Some(child_level) = tree_level.checked_sub(1) else {
                return Err(TreeError::decode(
                    Some(id),
                    DecodeError::ChildLevel {
                        child: tree_level,
                        parent: tree_level,
                    },
                ));
            };
            let child = view.child(view.pivot_surface().probe(key).upper_bound());
            id = child;
            expect = child_level;
        }
        Err(TreeError::ResourceLimit {
            what: "tree depth above max_tree_level",
        })
    }

    async fn resolve_many(
        &self,
        root: BlockId,
        keys: &[&[u8]],
        budget: &mut BudgetState,
    ) -> Result<Vec<Option<Winner>>, TreeError> {
        let mut scratch = ResolveScratch::default();
        self.resolve_many_cached(root, keys, budget, &mut scratch)
            .await?;
        Ok(scratch.best)
    }

    /// Turn resolved winners into values, fetching every out-of-line winner in ONE batched wave.
    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    async fn materialize_cached(
        &self,
        winners: &[Option<Winner>],
        budget: &mut BudgetState,
        scratch: &mut MaterializeScratch,
    ) -> Result<Vec<Option<Bytes>>, TreeError> {
        scratch.reset(winners.len());
        for (slot, w) in winners.iter().enumerate() {
            match w.as_ref().map(|w| &w.op) {
                Some(WinnerOp::External { id, len }) => {
                    let ref_slot = *scratch.index.entry(*id).or_insert_with(|| {
                        let index = scratch.refs.len();
                        scratch.refs.push((*id, *len));
                        index
                    });
                    scratch.slots[slot] = Some(ref_slot);
                }
                Some(_) => {}
                None => {}
            }
        }
        if scratch.refs.is_empty() {
            return Ok(winners
                .iter()
                .map(|w| {
                    w.as_ref().and_then(|w| match &w.op {
                        WinnerOp::Inline(v) => Some(v.clone()),
                        _ => None,
                    })
                })
                .collect());
        }
        self.load_values_deduplicated_cached(budget, scratch)
            .await?;
        scratch.out.clear();
        scratch.out.reserve(winners.len());
        for (slot, winner) in scratch.slots.iter().copied().zip(winners.iter()) {
            scratch.out.push(match winner {
                Some(w) => match &w.op {
                    WinnerOp::Inline(v) => Some(v.clone()),
                    WinnerOp::External { .. } => {
                        let WinnerOp::External { id, len } = &w.op else {
                            unreachable!()
                        };
                        let payload = scratch.cached[slot.expect("recorded")]
                            .as_ref()
                            .expect("recorded");
                        value::validate_reference_len(*id, payload, *len)?;
                        Some(payload.clone())
                    }
                    WinnerOp::Tombstone => None,
                },
                None => None,
            });
        }
        Ok(std::mem::take(&mut scratch.out))
    }

    async fn materialize(
        &self,
        winners: Vec<Option<Winner>>,
        budget: &mut BudgetState,
    ) -> Result<Vec<Option<Bytes>>, TreeError> {
        self.materialize_cached(&winners, budget, &mut MaterializeScratch::default())
            .await
    }

    /// Every `BlockId` reachable from `node`: child nodes AND out-of-line values, so a GC cannot miss a
    /// value edge by parsing only child ids.
    pub async fn references(
        &self,
        node: BlockId,
    ) -> Result<Vec<(crate::ObjectKind, BlockId)>, TreeError> {
        Ok(self.view(node).await?.references())
    }

    /// `references` over a whole frontier in ONE wave, so a GC mark walks a level per round trip.
    pub async fn references_many(
        &self,
        nodes: &[BlockId],
    ) -> Result<Vec<(crate::ObjectKind, BlockId)>, TreeError> {
        let mut budget = BudgetState::new(self.budget);
        // A GC mark walk wants the object graph, not the payloads.
        let wave = self
            .load_wave(nodes, AccessHint::MetadataOnly, &mut budget)
            .await?;
        Ok(wave.into_values().flat_map(|v| v.references()).collect())
    }

    // ---------------------------------------------------------------- cursors

    /// An ordered cursor over one root, restricted to `[lo, hi)`.
    fn cursor(&self, root: BlockId, lo: Option<Bytes>, hi: Option<Bytes>) -> Cursor<'_, S> {
        self.cursor_over(root, vec![KeyRange::new(lo, hi)])
    }

    /// A cursor restricted to a *set* of ranges. Children disjoint from every range are pruned, so many
    /// scattered windows still cost O(depth) waves over their union rather than one walk per window —
    /// and, crucially, not a walk of everything between the lowest and highest window.
    ///
    /// `ranges` need not be sorted or disjoint; it is normalized here.
    fn cursor_over(&self, root: BlockId, ranges: Vec<KeyRange>) -> Cursor<'_, S> {
        let ranges = normalize_ranges(ranges);
        let span = KeyRange::new(
            ranges.first().and_then(|range| range.lo.clone()),
            ranges.last().and_then(|range| range.hi.clone()),
        );
        Cursor {
            tree: self,
            frames: Vec::new(),
            pending: (!ranges.is_empty()).then_some(Pending {
                id: root,
                range: span.clone(),
                expect_level: None,
            }),
            batch: Vec::new().into_iter(),
            span,
            ranges,
            prefetch_width: DEFAULT_PREFETCH_WIDTH,
            visited_nodes: 0,
        }
    }

    async fn live_pairs(
        &self,
        rows: Vec<(Bytes, Winner)>,
        budget: &mut BudgetState,
    ) -> Result<Vec<(Bytes, Bytes)>, TreeError> {
        let (keys, winners): (Vec<Bytes>, Vec<Option<Winner>>) =
            rows.into_iter().map(|(k, w)| (k, Some(w))).unzip();
        let values = self.materialize(winners, budget).await?;
        Ok(keys
            .into_iter()
            .zip(values)
            .filter_map(|(k, v)| v.map(|v| (k, v)))
            .collect())
    }
}

/// Multi-root scans support sharded or independently partitioned datasets through one merged scan.
///
/// Roots are expected to own disjoint key spaces, so this is a *merge* rather than an arbitration — but
/// when two roots do hold the same key, the greater winner tuple wins, exactly as it would inside one
/// tree. That keeps the answer well defined instead of order dependent.
impl<S: NodeStore> BeTree<S> {
    async fn collect_roots(
        &self,
        roots: &[BlockId],
        ranges: Vec<KeyRange>,
        budget: &mut BudgetState,
    ) -> Result<Vec<(Bytes, Winner)>, TreeError> {
        let mut acc: BTreeMap<Bytes, Winner> = BTreeMap::new();
        // Distinct roots only: folding one root twice would double-visit its entries.
        let mut seen: foldhash::HashSet<BlockId> = Default::default();
        for root in roots.iter().copied().filter(|r| seen.insert(*r)) {
            let mut c = self.cursor_over(root, ranges.clone());
            while let Some((k, w)) = c.next_kv(budget).await? {
                match acc.get_mut(&k) {
                    Some(cur) if *cur >= w => {}
                    Some(cur) => *cur = w,
                    None => {
                        acc.insert(k, w);
                    }
                }
            }
        }
        Ok(acc.into_iter().collect())
    }

    /// Half-open range scan across many roots.
    pub async fn scan_range_roots(
        &self,
        roots: &[BlockId],
        lo: Option<&[u8]>,
        hi: Option<&[u8]>,
    ) -> Result<Vec<(Bytes, Bytes)>, TreeError> {
        let mut budget = BudgetState::new(self.budget);
        let ranges = vec![KeyRange::new(
            lo.map(Bytes::copy_from_slice),
            hi.map(Bytes::copy_from_slice),
        )];
        let rows = self.collect_roots(roots, ranges, &mut budget).await?;
        self.live_pairs(rows, &mut budget).await
    }

    /// Prefix scan across many roots.
    pub async fn scan_prefix_roots(
        &self,
        roots: &[BlockId],
        prefix: &[u8],
    ) -> Result<Vec<(Bytes, Bytes)>, TreeError> {
        let hi = prefix_succ(prefix);
        self.scan_range_roots(roots, Some(prefix), hi.as_deref())
            .await
    }

    /// Many prefixes across many roots, in one pruned walk per root. One live list per input prefix, in
    /// input order.
    pub async fn scan_prefix_many_roots(
        &self,
        roots: &[BlockId],
        prefixes: &[&[u8]],
    ) -> Result<Vec<Vec<(Bytes, Bytes)>>, TreeError> {
        if prefixes.is_empty() || roots.is_empty() {
            return Ok(vec![Vec::new(); prefixes.len()]);
        }
        let mut budget = BudgetState::new(self.budget);
        let ranges: Vec<KeyRange> = prefixes
            .iter()
            .map(|p| KeyRange::new(Some(Bytes::copy_from_slice(p)), prefix_succ(p)))
            .collect();
        let rows = self.collect_roots(roots, ranges, &mut budget).await?;
        let live = self.live_pairs(rows, &mut budget).await?;
        Ok(bucket_by_prefix(prefixes, live))
    }
}

/// Route each live row into every input prefix it matches, by one hash lookup per distinct prefix
/// LENGTH. Shared by the single- and multi-root prefix-many paths so they cannot diverge.
fn bucket_by_prefix(prefixes: &[&[u8]], live: Vec<(Bytes, Bytes)>) -> Vec<Vec<(Bytes, Bytes)>> {
    let mut by_len: foldhash::HashMap<usize, foldhash::HashMap<&[u8], Vec<usize>>> =
        Default::default();
    for (i, p) in prefixes.iter().enumerate() {
        by_len
            .entry(p.len())
            .or_default()
            .entry(*p)
            .or_default()
            .push(i);
    }
    let mut out: Vec<Vec<(Bytes, Bytes)>> = vec![Vec::new(); prefixes.len()];
    for (k, v) in live {
        for (len, index) in &by_len {
            if k.len() < *len {
                continue;
            }
            if let Some(slots) = index.get(&k[..*len]) {
                for &i in slots {
                    out[i].push((k.clone(), v.clone()));
                }
            }
        }
    }
    out
}

/// The exclusive successor of a prefix (increment with carry, truncating trailing `0xFF`). `None` means
/// the prefix is all-`0xFF`, so it is unbounded above.
fn prefix_succ(prefix: &[u8]) -> Option<Bytes> {
    let mut hi = prefix.to_vec();
    for i in (0..hi.len()).rev() {
        if hi[i] != 0xFF {
            hi[i] += 1;
            hi.truncate(i + 1);
            return Some(Bytes::from(hi));
        }
    }
    None
}

/// One half-open key interval. Bounds travel together so callers cannot accidentally intersect a
/// lower bound from one subtree with an upper bound from another.
#[derive(Debug, Clone, PartialEq, Eq)]
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

    fn is_empty(&self) -> bool {
        matches!((&self.lo, &self.hi), (Some(lo), Some(hi)) if lo >= hi)
    }

    fn intersects(&self, other: &Self) -> bool {
        !matches!((&self.hi, &other.lo), (Some(hi), Some(lo)) if hi <= lo)
            && !matches!((&self.lo, &other.hi), (Some(lo), Some(hi)) if lo >= hi)
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

    /// Extend this range upward through an overlapping or touching successor.
    fn merge(&mut self, other: Self) {
        self.hi = match (&self.hi, other.hi) {
            (Some(a), Some(b)) => Some(a.max(&b).clone()),
            _ => None,
        };
    }
}

/// Sort ranges by lower bound and coalesce every overlapping or touching pair, dropping empty ones. The
/// result is sorted and pairwise disjoint, which is what makes the cursor's overlap test a binary search
/// instead of a scan.
fn normalize_ranges(mut ranges: Vec<KeyRange>) -> Vec<KeyRange> {
    // `None` as a lower bound sorts first; `None` as an upper bound sorts last.
    ranges.retain(|range| !range.is_empty());
    ranges.sort_by(|a, b| match (&a.lo, &b.lo) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(x), Some(y)) => x.cmp(y),
    });
    let mut out: Vec<KeyRange> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match out.last_mut() {
            // The next range starts at or before the current one ends, so they merge.
            Some(previous)
                if previous.hi.is_none()
                    || range
                        .lo
                        .as_ref()
                        .is_none_or(|lo| previous.hi.as_ref().is_some_and(|hi| lo <= hi)) =>
            {
                previous.merge(range);
            }
            _ => out.push(range),
        }
    }
    out
}

/// A subtree the cursor is positioned at but has not entered.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pending {
    id: BlockId,
    range: KeyRange,
    expect_level: Option<u16>,
}

struct Frame {
    view: Arc<NodeView>,
    /// Absolute key range this node owns, already intersected with the scan range.
    range: KeyRange,
    next_child: usize,
    /// Children of this frame actually entered, and children skipped without being read.
    entered: usize,
    skipped: usize,
    /// Whether this frame's remaining in-scope children have already been prefetched.
    prefetched: bool,
}

/// An ordered cursor: it yields resolved `(key, winner)` pairs in key order, and it exposes the
/// *ancestor buffer overlays* covering the range it is about to enter.
///
/// That overlay state is what makes it correct for a buffered tree. Copying a prolly-tree cursor
/// without it would be wrong: an equal subtree id proves the subtree's own entries match, but a
/// newer message sitting in an ancestor buffer above it can still change the resolved winner.
struct Cursor<'t, S: NodeStore> {
    tree: &'t BeTree<S>,
    frames: Vec<Frame>,
    pending: Option<Pending>,
    batch: std::vec::IntoIter<(Bytes, Winner)>,
    /// The union's outer bounds, used to bound one leaf's entry sweep.
    span: KeyRange,
    /// Sorted, pairwise disjoint ranges. A key is in scope iff it falls in one of them.
    ranges: Vec<KeyRange>,
    /// Objects one lookahead may fetch. Bounds the speculative memory a scan pulls into the cache.
    prefetch_width: usize,
    visited_nodes: u64,
}

type RangeSet = [KeyRange];

/// Does `[lo, hi)` overlap any range in the normalized set? A binary search, so pruning never scales
/// with the number of ranges.
fn overlaps_scope(ranges: &RangeSet, candidate: &KeyRange) -> bool {
    if ranges.len() == 1 {
        return candidate.intersects(&ranges[0]);
    }
    // First range whose lower bound is not below `hi`; the candidate is that one or its predecessor.
    let at = ranges.partition_point(|range| match (&range.lo, &candidate.hi) {
        (_, None) => true,
        (None, Some(_)) => true,
        (Some(rlo), Some(hi)) => rlo < hi,
    });
    (at.saturating_sub(1)..=at)
        .filter_map(|i| ranges.get(i))
        .any(|range| candidate.intersects(range))
}

/// Is `key` inside one of the ranges?
fn in_scope(ranges: &RangeSet, key: &[u8]) -> bool {
    if ranges.len() == 1 {
        return ranges[0].contains(key);
    }
    let at = ranges.partition_point(|range| range.lo.as_ref().is_none_or(|lo| lo.as_ref() <= key));
    at.checked_sub(1)
        .and_then(|i| ranges.get(i))
        .is_some_and(|range| range.contains(key))
}

/// Merge two sorted, unique winner runs. A leaf and its collapsed ancestor overlays already have the
/// order a cursor needs; rebuilding that order through a tree map adds O(n log n) comparisons and one
/// allocation per key.
fn merge_winner_runs(leaf: Vec<(Bytes, Winner)>, overlays: Vec<Entry>) -> Vec<(Bytes, Winner)> {
    let capacity = leaf.len().saturating_add(overlays.len());
    let mut leaf = leaf.into_iter().peekable();
    let mut overlays = overlays
        .into_iter()
        .map(|entry| {
            let winner = entry.winner();
            (entry.key, winner)
        })
        .peekable();
    let mut merged = Vec::with_capacity(capacity);
    loop {
        match (leaf.peek(), overlays.peek()) {
            (Some((leaf_key, _)), Some((overlay_key, _))) => {
                use std::cmp::Ordering;
                match leaf_key.cmp(overlay_key) {
                    Ordering::Less => merged.push(leaf.next().expect("peeked")),
                    Ordering::Greater => merged.push(overlays.next().expect("peeked")),
                    Ordering::Equal => {
                        let (key, leaf_winner) = leaf.next().expect("peeked");
                        let (_, overlay_winner) = overlays.next().expect("peeked");
                        merged.push((key, leaf_winner.max(overlay_winner)));
                    }
                }
            }
            (Some(_), None) => {
                merged.extend(leaf);
                break;
            }
            (None, Some(_)) => {
                merged.extend(overlays);
                break;
            }
            (None, None) => break,
        }
    }
    merged
}

impl<'t, S: NodeStore> Cursor<'t, S> {
    /// The subtree about to be entered, if the cursor is at a subtree boundary with nothing buffered.
    fn peek_pending(&self) -> Option<&Pending> {
        self.batch
            .as_slice()
            .is_empty()
            .then_some(self.pending.as_ref())
            .flatten()
    }

    fn visited_nodes(&self) -> u64 {
        self.visited_nodes
    }

    /// Merge the ancestor buffer runs overlapping `[lo, hi)` into one entry per key, keeping the greater
    /// winner. Depth is not authority: the winner tuple decides.
    fn overlays_in(&self, range: &KeyRange) -> Vec<Entry> {
        let mut acc: BTreeMap<Bytes, Entry> = BTreeMap::new();
        for f in &self.frames {
            let v = &f.view;
            let start = match &range.lo {
                Some(l) => v.entry_surface().probe(l).index,
                None => 0,
            };
            for i in start..v.entry_count() {
                let key = v.entry_key(i);
                if range.hi.as_ref().is_some_and(|hi| key >= hi.as_ref()) {
                    break;
                }
                let e = v.entry(i);
                match acc.get_mut(&e.key) {
                    Some(cur) if cur.winner() >= e.winner() => {}
                    slot => {
                        let key = e.key.clone();
                        match slot {
                            Some(cur) => *cur = e,
                            None => {
                                acc.insert(key, e);
                            }
                        }
                    }
                }
            }
        }
        acc.into_values().collect()
    }

    /// Discard the pending subtree without loading it. Used only when the caller has already accounted
    /// for that range some other way.
    fn skip_pending(&mut self) {
        self.pending = None;
        if let Some(f) = self.frames.last_mut() {
            f.skipped += 1;
        }
        self.advance();
    }

    /// The remaining in-scope children of the deepest frame, for one batched prefetch.
    fn sibling_run(&self) -> Vec<BlockId> {
        let Some(f) = self.frames.last() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        // `next_child` already points past the pending child, so include the pending explicitly.
        if let Some(p) = &self.pending {
            out.push(p.id);
        }
        for i in f.next_child..f.view.child_count() {
            let (clo, chi) = f.view.child_range(i);
            let child = KeyRange::new(
                clo.map(Bytes::copy_from_slice),
                chi.map(Bytes::copy_from_slice),
            );
            let range = f.range.intersection(&child);
            if overlaps_scope(&self.ranges, &range) {
                out.push(f.view.child(i));
            }
        }
        out
    }

    /// Set `pending` to the next child to visit, popping exhausted frames. Children whose range is
    /// disjoint from the scan range are pruned here.
    fn advance(&mut self) {
        while let Some(f) = self.frames.last_mut() {
            if f.next_child >= f.view.child_count() {
                self.frames.pop();
                continue;
            }
            let i = f.next_child;
            f.next_child += 1;
            let (clo, chi) = f.view.child_range(i);
            let child = KeyRange::new(
                clo.map(Bytes::copy_from_slice),
                chi.map(Bytes::copy_from_slice),
            );
            let range = f.range.intersection(&child);
            if !overlaps_scope(&self.ranges, &range) {
                continue;
            }
            self.pending = Some(Pending {
                id: f.view.child(i),
                range,
                // A malformed level-zero internal node must remain a typed error path rather than
                // underflowing while constructing the next cursor frame. The subsequent load check
                // still validates the edge; saturating here only protects the verifier-off path.
                expect_level: Some(f.view.tree_level().saturating_sub(1)),
            });
            return;
        }
        self.pending = None;
    }

    /// Descend until a leaf is reached, then fill `batch` with that leaf's range, resolved against the
    /// ancestor overlays. A batch is bounded by one leaf, so memory is bounded by `NODE_BYTES`.
    async fn fill(&mut self, budget: &mut BudgetState) -> Result<(), TreeError> {
        while self.batch.as_slice().is_empty() {
            let Some(p) = self.pending.take() else {
                return Ok(());
            };
            // Cursors are ordered walks: their leaf level is sequential, their spine is metadata.
            let hint = BeTree::<S>::descent_hint(p.expect_level, true);
            // A prefetch charges bytes but not object visits; the visit is charged by the load below.

            // Batch the whole remaining sibling run in ONE read rather than one `get` per node. A serial
            // cursor turned a cold 4 000-key scan into 337 scalar gets and zero batched reads.
            //
            // The guard is what keeps this from hurting `diff`: a frame that has *skipped* a child is one
            // whose subtrees the caller intends not to read (an equal-subtree skip), so prefetching them
            // would fetch exactly what the skip exists to avoid. A scan never skips, so it always
            // batches; an aligned diff skips before its first load, so it never does.
            let should_prefetch = self
                .frames
                .last()
                .is_some_and(|f| !f.prefetched && f.skipped == 0);
            if should_prefetch {
                let run = self.sibling_run();
                if run.len() > 1 {
                    let ranges = self.ranges.clone();
                    self.tree
                        .prefetch_ahead(run, &ranges, self.prefetch_width, budget)
                        .await;
                }
                if let Some(f) = self.frames.last_mut() {
                    f.prefetched = true;
                }
            }

            let view = self.tree.load(p.id, hint, budget).await?;
            self.visited_nodes += 1;
            if let Some(f) = self.frames.last_mut() {
                f.entered += 1;
            }
            if let Some(expect) = p.expect_level
                && view.tree_level() != expect
            {
                return Err(TreeError::decode(
                    Some(p.id),
                    DecodeError::ChildLevel {
                        child: view.tree_level(),
                        parent: expect + 1,
                    },
                ));
            }
            if view.is_leaf() {
                let range = p.range.intersection(&self.span);
                // k-way merge of this leaf's run with every overlapping ancestor buffer run. The
                // greatest winner tuple wins, so traversal-source priority has no semantic role.
                let mut leaf = Vec::with_capacity(view.entry_count());
                let start = match &range.lo {
                    Some(l) => view.entry_surface().probe(l).index,
                    None => 0,
                };
                for i in start..view.entry_count() {
                    let key = view.entry_key(i);
                    if range.hi.as_ref().is_some_and(|hi| key >= hi.as_ref()) {
                        break;
                    }
                    if !in_scope(&self.ranges, key) {
                        continue;
                    }
                    leaf.push((view.entry_key_bytes(i), view.winner(i)));
                }
                let overlays = self
                    .overlays_in(&range)
                    .into_iter()
                    .filter(|entry| in_scope(&self.ranges, &entry.key))
                    .collect();
                self.batch = merge_winner_runs(leaf, overlays).into_iter();
                self.advance();
            } else {
                let range = p.range.intersection(&self.span);
                self.frames.push(Frame {
                    view,
                    range,
                    next_child: 0,
                    entered: 0,
                    skipped: 0,
                    prefetched: false,
                });
                // Fresh frame: its first in-range child becomes the new pending.
                self.advance();
            }
        }
        Ok(())
    }

    async fn peek_kv(
        &mut self,
        budget: &mut BudgetState,
    ) -> Result<Option<&(Bytes, Winner)>, TreeError> {
        self.fill(budget).await?;
        Ok(self.batch.as_slice().first())
    }

    async fn next_kv(
        &mut self,
        budget: &mut BudgetState,
    ) -> Result<Option<(Bytes, Winner)>, TreeError> {
        self.fill(budget).await?;
        Ok(self.batch.next())
    }
}

/// A backpressured live range scan. One call advances at most to the next live row; memory is bounded
/// by one decoded leaf, its ancestor overlays, and the cursor's bounded prefetch window.
pub struct ScanCursor<'t, S: NodeStore> {
    tree: &'t BeTree<S>,
    inner: Cursor<'t, S>,
    budget: BudgetState,
}

impl<'t, S: NodeStore> ScanCursor<'t, S> {
    /// Set the maximum number of node objects fetched by one breadth-first lookahead. A width of zero
    /// disables lookahead; result order and exactness are independent of this throughput knob.
    pub fn with_prefetch_width(mut self, width: usize) -> Self {
        self.inner.prefetch_width = width;
        self
    }

    pub async fn next(&mut self) -> Result<Option<(Bytes, Bytes)>, TreeError> {
        Ok(self.next_batch(1).await?.pop())
    }

    /// Pull at most `max_rows` raw winners and materialize all external values in one coalesced wave.
    /// This is the throughput form of the cursor: bounded memory and backpressure without turning an
    /// external-value scan into one round trip per row.
    pub async fn next_batch(&mut self, max_rows: usize) -> Result<Vec<(Bytes, Bytes)>, TreeError> {
        if max_rows == 0 {
            return Ok(Vec::new());
        }
        let mut rows = Vec::with_capacity(max_rows);
        while rows.len() < max_rows {
            let Some((key, winner)) = self.inner.next_kv(&mut self.budget).await? else {
                break;
            };
            if !matches!(winner.op, WinnerOp::Tombstone) {
                rows.push((key, winner));
            }
        }
        self.tree.live_pairs(rows, &mut self.budget).await
    }
}

/// A backpressured Merkle diff. Equal pending subtrees are skipped before any bytes below them are
/// loaded; ancestor-overlay candidates for that exact range are resolved as one small batch.
pub struct DiffCursor<'t, S: NodeStore> {
    tree: &'t BeTree<S>,
    a_root: BlockId,
    b_root: BlockId,
    a: Cursor<'t, S>,
    b: Cursor<'t, S>,
    budget: BudgetState,
    ready: VecDeque<Bytes>,
    done: bool,
    reported: bool,
}

impl<'t, S: NodeStore> DiffCursor<'t, S> {
    /// Set lookahead independently of result batch size. Equal subtrees are still skipped before
    /// prefetch, so a wider value cannot defeat Merkle pruning.
    pub fn with_prefetch_width(mut self, width: usize) -> Self {
        self.a.prefetch_width = width;
        self.b.prefetch_width = width;
        self
    }

    pub async fn next(&mut self) -> Result<Option<Bytes>, TreeError> {
        if let Some(key) = self.ready.pop_front() {
            return Ok(Some(key));
        }
        while !self.done {
            let equal = match (self.a.peek_pending(), self.b.peek_pending()) {
                (Some(a), Some(b)) if a.id == b.id && a.range == b.range => Some(a.range.clone()),
                _ => None,
            };
            if let Some(range) = equal {
                self.tree.metrics.diff_equal_id_skip();
                let oa = self.a.overlays_in(&range);
                let ob = self.b.overlays_in(&range);
                if oa != ob {
                    let keys: Vec<Bytes> = oa
                        .iter()
                        .chain(&ob)
                        .map(|entry| entry.key.clone())
                        .collect::<std::collections::BTreeSet<_>>()
                        .into_iter()
                        .collect();
                    if !keys.is_empty() {
                        let refs: Vec<&[u8]> = keys.iter().map(|key| key.as_ref()).collect();
                        let wa = self
                            .tree
                            .resolve_many(self.a_root, &refs, &mut self.budget)
                            .await?;
                        let wb = self
                            .tree
                            .resolve_many(self.b_root, &refs, &mut self.budget)
                            .await?;
                        self.ready
                            .extend(keys.into_iter().zip(wa).zip(wb).filter_map(
                                |((key, a), b)| (live_value(&a) != live_value(&b)).then_some(key),
                            ));
                    }
                }
                self.a.skip_pending();
                self.b.skip_pending();
                if let Some(key) = self.ready.pop_front() {
                    return Ok(Some(key));
                }
                continue;
            }

            let ka = self
                .a
                .peek_kv(&mut self.budget)
                .await?
                .map(|(key, _)| key.clone());
            let kb = self
                .b
                .peek_kv(&mut self.budget)
                .await?
                .map(|(key, _)| key.clone());
            let next = match (ka, kb) {
                (None, None) => {
                    if self.a.peek_pending().is_none() && self.b.peek_pending().is_none() {
                        self.done = true;
                    }
                    None
                }
                (Some(a), Some(b)) if a == b => {
                    let (_, wa) = self.a.next_kv(&mut self.budget).await?.expect("peeked");
                    let (_, wb) = self.b.next_kv(&mut self.budget).await?.expect("peeked");
                    self.tree.metrics.diff_visited_keys(2);
                    (observable(&wa) != observable(&wb)).then_some(a)
                }
                (Some(a), b) if b.as_ref().is_none_or(|b| a < *b) => {
                    let (key, winner) = self.a.next_kv(&mut self.budget).await?.expect("peeked");
                    self.tree.metrics.diff_visited_keys(1);
                    observable(&winner).is_some().then_some(key)
                }
                _ => {
                    let (key, winner) = self.b.next_kv(&mut self.budget).await?.expect("peeked");
                    self.tree.metrics.diff_visited_keys(1);
                    observable(&winner).is_some().then_some(key)
                }
            };
            if let Some(key) = next {
                return Ok(Some(key));
            }
        }
        if !self.reported {
            self.tree
                .metrics
                .diff_visited_nodes(self.a.visited_nodes() + self.b.visited_nodes());
            self.reported = true;
        }
        Ok(None)
    }

    /// Pull at most `max_keys` differences. Zero is a non-advancing request.
    pub async fn next_batch(&mut self, max_keys: usize) -> Result<Vec<Bytes>, TreeError> {
        let mut out = Vec::with_capacity(max_keys);
        while out.len() < max_keys {
            let Some(key) = self.next().await? else {
                break;
            };
            out.push(key);
        }
        Ok(out)
    }
}

impl<S: NodeStore> BeTree<S> {
    pub fn scan_cursor(
        &self,
        root: BlockId,
        lo: Option<&[u8]>,
        hi: Option<&[u8]>,
    ) -> ScanCursor<'_, S> {
        ScanCursor {
            tree: self,
            inner: self.cursor(
                root,
                lo.map(Bytes::copy_from_slice),
                hi.map(Bytes::copy_from_slice),
            ),
            budget: BudgetState::new(self.budget),
        }
    }

    /// Prefix form of [`Self::scan_cursor`], including the all-`0xff` unbounded upper edge.
    pub fn scan_prefix_cursor(&self, root: BlockId, prefix: &[u8]) -> ScanCursor<'_, S> {
        let hi = prefix_succ(prefix);
        ScanCursor {
            tree: self,
            inner: self.cursor(root, Some(Bytes::copy_from_slice(prefix)), hi),
            budget: BudgetState::new(self.budget),
        }
    }

    pub fn diff_cursor(&self, a: BlockId, b: BlockId) -> DiffCursor<'_, S> {
        DiffCursor {
            tree: self,
            a_root: a,
            b_root: b,
            a: self.cursor(a, None, None),
            b: self.cursor(b, None, None),
            budget: BudgetState::new(self.budget),
            ready: VecDeque::new(),
            done: a == b,
            reported: false,
        }
    }

    /// Rewrite every resolved winner from `source_root` into this tree's format. This is deliberately
    /// an offline streaming operation: it preserves tombstones and exact order keys, materializes old
    /// external values through the source decoder, bounds live memory by `batch_rows + depth`, and
    /// never mutates or deletes the historical object graph.
    pub async fn migrate_from<T: NodeStore>(
        &self,
        source: &BeTree<T>,
        source_root: BlockId,
        batch_rows: usize,
    ) -> Result<MigrationReport, TreeError> {
        if batch_rows == 0 {
            return Err(TreeError::Capacity(CapacityError::Format(
                "migration batch_rows must be nonzero".into(),
            )));
        }
        let mut target_root = self.empty_root().await?;
        let mut cursor = source.cursor(source_root, None, None);
        let mut source_budget = BudgetState::new(source.budget);
        let mut rows = 0u64;
        let mut apply_batches = 0u64;

        loop {
            let mut raw = Vec::with_capacity(batch_rows);
            while raw.len() < batch_rows {
                let Some(row) = cursor.next_kv(&mut source_budget).await? else {
                    break;
                };
                raw.push(row);
            }
            if raw.is_empty() {
                break;
            }

            let refs: Vec<(BlockId, u32)> = raw
                .iter()
                .filter_map(|(_, winner)| match winner.op {
                    WinnerOp::External { id, len } => Some((id, len)),
                    _ => None,
                })
                .collect();
            let values = source.load_values(&refs, &mut source_budget).await?;
            let mut staged = Staged::default();
            let mut entries = Vec::with_capacity(raw.len());
            let mut value_at = 0usize;
            for (key, winner) in raw {
                self.fmt.check_key(&key)?;
                let entry = match winner.op {
                    WinnerOp::Inline(value) => {
                        self.fmt.check_value(&value)?;
                        Entry::inline(key, winner.order_key, value)
                    }
                    WinnerOp::External { .. } => {
                        let payload = &values[value_at];
                        value_at += 1;
                        self.fmt.check_value(payload)?;
                        let object = value::encode(self.fmt.schema_id(), payload);
                        staged.value(&object);
                        Entry::external(key, winner.order_key, object.id, object.logical_len)
                    }
                    WinnerOp::Tombstone => Entry::tombstone(key, winner.order_key),
                };
                entries.push(entry);
                rows += 1;
            }
            target_root = self.apply_prepared(target_root, entries, staged).await?;
            apply_batches += 1;
        }

        Ok(MigrationReport {
            source_schema: *source.format().schema_id(),
            target_schema: *self.format().schema_id(),
            old_root: source_root,
            new_root: target_root,
            rows,
            apply_batches,
        })
    }
}

/// The observable value of a winner: `None` for a tombstone, which is indistinguishable from absent.
///
/// Comparing [`WinnerOp`] equality is exactly comparing observable *values*: an out-of-line winner is
/// equal iff its `(id, length)` is equal, and content addressing makes that equivalent to equal bytes.
/// The inline threshold is deterministic, so the same bytes never appear both inline and out-of-line.
fn observable(w: &Winner) -> Option<&WinnerOp> {
    match &w.op {
        WinnerOp::Tombstone => None,
        op => Some(op),
    }
}

/// [`observable`] over an optional winner: absent and tombstoned are the same observation.
fn live_value(w: &Option<Winner>) -> Option<&WinnerOp> {
    w.as_ref().and_then(observable)
}
