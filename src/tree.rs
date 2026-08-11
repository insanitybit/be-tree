//! The content-addressed COW Bε-tree. A node's identity **is** its BLAKE3 content hash, so the tree
//! is a Merkle tree by construction: equal subtrees share a BlockId, which is what makes
//! `subtree_hash` diff O(divergence) and snapshots free.
//!
//! Interior nodes carry a small **message buffer**: a write is absorbed near the root and flushed
//! downward lazily. Consequences: commit work is O(messages) at the root, not O(log n)
//! full rewrites; `tree_put` never mutates — it rewrites only the changed root→leaf path as new
//! blocks and returns a NEW root BlockId.
//!
//! Records are versioned: a key may have many entries at different HLCs. Resolution is **LWW-at-read
//! by descending HLC** — the reader picks the highest-HLC entry, so re-injecting a
//! lower-HLC message next to a higher one still reads back the higher. This is why rebase re-injection
//! is correct without idempotence.

use std::sync::Arc;

use crate::store::{NodeStore, StagedNode};
use crate::{AccessHint, BTreeMessage, BlockId, Hlc, MessageOp, TreeError};
use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

/// Hot-path node maps are keyed by `BlockId` (already a blake3 hash) — foldhash beats SipHash on
/// every descent probe with zero DoS exposure (keys are content hashes, not attacker-chosen).
type NodeMap = foldhash::HashMap<BlockId, Arc<Node>>;

/// Accumulates a write walk's new nodes so ONE commit's tree rewrite flushes as a single packed
/// `put_batch` instead of one write per node. Staging is pure: `BlockId::of` is blake3 (no I/O), so
/// a parent references a freshly-staged child by id before that child is durable — the whole batch
/// lands together at the `tree_put` boundary, before anything names the new root — a root is only
/// ever published once every node it reaches is durable. Tree nodes are reachability-governed:
/// retained until nothing references them.
#[derive(Default)]
struct Staged {
    nodes: Vec<StagedNode>,
}

impl Staged {
    /// Serialize a node, compute its content id, and stage its bytes (no I/O). Returns the id so a
    /// parent can reference it immediately.
    fn stage(&mut self, node: &Node) -> Result<BlockId, TreeError> {
        let bytes = postcard::to_stdvec(node).map_err(|e| TreeError::Codec(e.to_string()))?;
        let id = BlockId::of(&bytes);
        self.nodes.push(StagedNode(Bytes::from(bytes))); // reachability-governed, never expiring
        Ok(id)
    }
}

/// One versioned entry for a key. Entries for a key are kept newest-first (descending HLC) so a point
/// read returns the current state from the first slot without scanning history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    key: Vec<u8>,
    hlc: Hlc,
    op: MessageOp,
}

/// A tree node: either a leaf (sorted versioned entries) or an interior node (pivots + child BlockIds
/// + a message buffer). Serialized with postcard deterministically ⇒ its BlockId is content-stable.
#[derive(Debug, Clone, Serialize, Deserialize)]
enum Node {
    Leaf {
        entries: Vec<Entry>, // sorted by (key asc, hlc desc)
    },
    Internal {
        pivots: Vec<Vec<u8>>, // pivots[i] = min key of children[i+1]; len = children.len()-1
        children: Vec<BlockId>, // child roots
        buffer: Vec<Entry>,   // messages absorbed here, not yet flushed; sorted like a leaf
    },
}

/// A leaf splits when it exceeds this many entries; an interior buffer flushes when it exceeds it.
/// Small so tests exercise splits/flushes; a real deployment tunes it (impl §11 flagged knob).
const FANOUT: usize = 64;

/// The Bε-tree over any [`NodeStore`]. `&self` + the store's internal locks — `Arc<MemTree>` shared.
pub struct MemTree<S: NodeStore> {
    blocks: Arc<S>,
    /// Retention floor: on a write that rewrites a leaf, per-key versions strictly below this HLC are
    /// dropped (keeping each key's winner). `ZERO` = keep-all (default). Set it to the oldest HLC any
    /// live branch or snapshot can still read, so compaction never drops a reachable version.
    floor: Hlc,
    /// The store's write class for this tree's nodes — the host's own vocabulary, never interpreted
    /// here. Defaults to `Class::default()`; derive a class-scoped view with `for_class` so a commit's
    /// tree nodes are written at the same class as the data they index.
    class: S::Class,
    /// Decoded-node cache: `BlockId → Arc<Node>`. A node is immutable content-addressed bytes, so a
    /// cached decode is coherent FOREVER — zero invalidation, eviction is pure capacity. Shared across
    /// the class/floor views derived by `for_class`, so a walk decodes each node ONCE rather than once
    /// per view. Weight-bounded by decoded bytes.
    cache: moka::future::Cache<BlockId, Arc<Node>>,
}

/// Approximate decoded footprint of a node, for the cache weigher (bounds memory: data leaves are large,
/// a count-bounded cache would not). Cheap: sums key + value lengths, no re-encode.
fn node_weight(n: &Node) -> u32 {
    let entry = |e: &Entry| -> usize {
        e.key.len()
            + match &e.op {
                MessageOp::Upsert(v) => v.len(),
                MessageOp::Tombstone => 0,
            }
            + 24 // hlc + per-entry overhead
    };
    let bytes = match n {
        Node::Leaf { entries } => entries.iter().map(entry).sum::<usize>(),
        Node::Internal {
            pivots,
            children,
            buffer,
        } => {
            pivots.iter().map(|p| p.len()).sum::<usize>()
                + children.len() * 32
                + buffer.iter().map(entry).sum::<usize>()
        }
    };
    bytes.min(u32::MAX as usize) as u32
}

/// A fresh decoded-node cache. Weight-bounded (decoded bytes) so data-heavy leaves can't blow memory.
fn new_node_cache() -> moka::future::Cache<BlockId, Arc<Node>> {
    moka::future::Cache::builder()
        .max_capacity(512 << 20) // 512 MiB of decoded nodes
        .weigher(|_id: &BlockId, node: &Arc<Node>| node_weight(node))
        .build()
}

/// The content-addressed COW Bε-tree contract. `tree_put` NEVER mutates — it returns a NEW root,
/// which is what makes a snapshot free. Merkle by construction: a node's identity IS its hash.
#[async_trait]
pub trait Tree: Send + Sync {
    /// Create an empty tree; returns the root BlockId of an empty leaf.
    async fn empty_root(&self) -> Result<BlockId, TreeError>;
    /// Inject messages at the root, flushing lazily; returns the NEW root BlockId.
    async fn tree_put(&self, root: BlockId, msgs: Vec<BTreeMessage>) -> Result<BlockId, TreeError>;
    /// Point read with LWW-at-read (descending-HLC). None if absent or the winner is a tombstone.
    async fn tree_get(&self, root: BlockId, key: &[u8]) -> Result<Option<Vec<u8>>, TreeError>;
    /// BATCHED point read: resolve MANY keys under `root` in O(tree-depth) `get_many` waves, decoding
    /// each covering node ONCE — vs one full re-decoding root→leaf walk per key. Results align to
    /// `keys`. This is what makes reassembling a chunked file (hundreds of hash-scattered chunk keys)
    /// cost O(depth waves + nodes-touched), not O(keys × node-decode) — the fix for whole-file reads.
    /// Default: sequential `tree_get` (correct but O(keys × walk)); `MemTree` overrides with the wave.
    async fn tree_get_many(
        &self,
        root: BlockId,
        keys: &[&[u8]],
    ) -> Result<Vec<Option<Vec<u8>>>, TreeError> {
        let mut out = Vec::with_capacity(keys.len());
        for k in keys {
            out.push(self.tree_get(root, k).await?);
        }
        Ok(out)
    }
    /// O(divergence) diff: keys whose resolved value differs between two roots. Identical subtrees
    /// (equal BlockId) are skipped with zero I/O.
    async fn diff(&self, a: BlockId, b: BlockId) -> Result<Vec<Vec<u8>>, TreeError>;
    /// Range scan: all live `(key, value)` pairs whose key starts with `prefix`, in key order, with
    /// LWW-at-read resolution (tombstoned keys excluded). This is the primitive adjacency lookups and
    /// directory listings ride on — a neighbour lookup is a prefix scan. Empty `prefix` scans the
    /// whole tree.
    async fn scan_prefix(
        &self,
        root: BlockId,
        prefix: &[u8],
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, TreeError>;

    /// **Batched multi-prefix scan.** Resolve MANY
    /// prefixes in ONE tree walk that SHARES covering internal nodes — one `load_wave` per LEVEL across the
    /// whole set, not one descent per prefix. This is what a multi-hop traversal's frontier expansion needs:
    /// N sources expand in a single batched descent (covering nodes deduped + cache-checked once), instead
    /// of N independent throttled scans. Returns one live `(key,value)` list per input prefix, in input
    /// order. Default: sequential `scan_prefix` (correct, O(prefixes × walk)); `MemTree` overrides with the
    /// single shared walk.
    async fn scan_prefix_many(
        &self,
        root: BlockId,
        prefixes: &[&[u8]],
    ) -> Result<Vec<Vec<(Vec<u8>, Vec<u8>)>>, TreeError> {
        let mut out = Vec::with_capacity(prefixes.len());
        for p in prefixes {
            out.push(self.scan_prefix(root, p).await?);
        }
        Ok(out)
    }

    /// Half-open range scan `[lo, hi)` (either bound `None` = unbounded), in key order, LWW-resolved,
    /// tombstones excluded. Prunes any subtree whose key range is disjoint from `[lo, hi)`, so a narrow
    /// window touches O(window + path) — an open lower bound a prefix scan cannot express, giving
    /// O(window) tailing rather than O(corpus).
    async fn scan_range(
        &self,
        root: BlockId,
        lo: Option<&[u8]>,
        hi: Option<&[u8]>,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, TreeError>;
}

impl<S: NodeStore> MemTree<S> {
    pub fn new(blocks: Arc<S>) -> Self {
        MemTree {
            blocks,
            floor: Hlc::ZERO,
            class: S::Class::default(),
            cache: new_node_cache(),
        }
    }

    /// Construct with a retention floor: leaf rewrites drop per-key versions below it (keeping each
    /// key's winner). Pass the oldest HLC any live branch or snapshot can still read, so compaction
    /// is bounded by what remains reachable.
    pub fn with_floor(blocks: Arc<S>, floor: Hlc) -> Self {
        MemTree {
            blocks,
            floor,
            class: S::Class::default(),
            cache: new_node_cache(),
        }
    }

    /// A cheap class-scoped view of this tree (shares the same store + floor + decoded-node cache):
    /// node writes land at `class` instead of the default. Derive one per commit so a commit's tree
    /// nodes are written as durably as the data they index — the class must reach the block writes.
    /// The cache is SHARED (immutable nodes ⇒ coherent across views).
    pub fn for_class(&self, class: S::Class) -> Self {
        MemTree {
            blocks: self.blocks.clone(),
            floor: self.floor,
            class,
            cache: self.cache.clone(),
        }
    }

    /// Load + decode one node, returning a SHARED `Arc<Node>`. Served from the decoded-node cache on
    /// hit (no fetch, no re-decode); on miss the bytes fault through to the store and are cached. Shared
    /// (not cloned) so a data-heavy leaf is decoded at most once regardless of how many keys touch it.
    async fn load(&self, id: BlockId) -> Result<Arc<Node>, TreeError> {
        if let Some(node) = self.cache.get(&id).await {
            return Ok(node);
        }
        let bytes = self.blocks.get(id, AccessHint::Random).await?;
        let node: Node =
            postcard::from_bytes(&bytes).map_err(|e| TreeError::Decode(id, e.to_string()))?;
        let node = Arc::new(node);
        self.cache.insert(id, node.clone()).await;
        Ok(node)
    }

    /// Load a whole frontier in ONE `get_many` wave — query cost is the DEPTH of dependent fetch
    /// waves, not the fetch count. Cache hits are skipped from the wave entirely; only true misses fetch.
    /// Input ids are deduped (content-addressing can share a subtree — e.g. the empty leaf — under one
    /// root); returns a shared decoded node for each id.
    async fn load_wave(&self, ids: &[BlockId]) -> Result<NodeMap, TreeError> {
        let mut out = NodeMap::with_capacity_and_hasher(ids.len(), Default::default());
        let mut misses: Vec<BlockId> = Vec::new();
        {
            let mut seen: foldhash::HashSet<BlockId> = Default::default();
            for id in ids.iter().copied().filter(|id| seen.insert(*id)) {
                match self.cache.get(&id).await {
                    Some(node) => {
                        out.insert(id, node);
                    }
                    None => misses.push(id),
                }
            }
        }
        if !misses.is_empty() {
            let fetched = self.blocks.get_many(&misses, AccessHint::Random).await;
            for (id, res) in misses.into_iter().zip(fetched) {
                let bytes = res?;
                let node: Node = postcard::from_bytes(&bytes)
                    .map_err(|e| TreeError::Decode(id, e.to_string()))?;
                let node = Arc::new(node);
                self.cache.insert(id, node.clone()).await;
                out.insert(id, node);
            }
        }
        Ok(out)
    }

    /// Fetch the (pruned) subtree under `root` into memory as BFS `get_many` waves — one wave per
    /// tree level, so round-trip depth is O(tree depth) instead of O(nodes touched). `descend`
    /// decides which children of an internal node to enqueue (the range/prefix pruning hook).
    /// The caller then walks the returned map SYNCHRONOUSLY in the original recursion order, so
    /// order-sensitive folds (`merge_into`'s children-then-buffer tie-break) are preserved exactly.
    async fn fetch_pruned(
        &self,
        root: BlockId,
        descend: impl Fn(&[Vec<u8>], usize) -> bool,
    ) -> Result<NodeMap, TreeError> {
        self.fetch_pruned_multi(std::slice::from_ref(&root), descend)
            .await
    }

    /// `fetch_pruned` seeded from MANY roots at once: every tree's frontier advances in the SAME BFS
    /// wave, so the whole fan-out costs O(max tree depth) waves, not O(roots × depth).
    /// Content-addressing dedups nodes shared across roots.
    async fn fetch_pruned_multi(
        &self,
        roots: &[BlockId],
        descend: impl Fn(&[Vec<u8>], usize) -> bool,
    ) -> Result<NodeMap, TreeError> {
        let mut nodes = NodeMap::default();
        let mut frontier = roots.to_vec();
        while !frontier.is_empty() {
            let wave = self.load_wave(&frontier).await?;
            frontier.clear();
            for (id, node) in wave {
                if let Node::Internal {
                    pivots, children, ..
                } = node.as_ref()
                {
                    for (i, child) in children.iter().enumerate() {
                        if descend(pivots, i) && !nodes.contains_key(child) {
                            frontier.push(*child);
                        }
                    }
                }
                nodes.insert(id, node);
            }
        }
        Ok(nodes)
    }

    async fn store(&self, node: &Node) -> Result<BlockId, TreeError> {
        let bytes = postcard::to_stdvec(node).map_err(|e| TreeError::Codec(e.to_string()))?;
        self.blocks.put(bytes.into(), self.class).await
    }

    /// Land a write walk's staged nodes as ONE packed batch. Durable on return. Empty staging
    /// (no nodes produced) is a no-op. The batch's block ids match what `stage` computed, so the parent
    /// references stay valid; content-addressing makes the pack PUT idempotent on a re-driven commit.
    async fn flush(&self, staged: Staged) -> Result<(), TreeError> {
        if staged.nodes.is_empty() {
            return Ok(());
        }
        self.blocks.put_batch(staged.nodes, self.class).await?;
        Ok(())
    }

    /// The child BlockIds a tree node directly references (empty for a leaf). Lets a garbage collector
    /// walk the tree's Merkle DAG for reachability marking without knowing the node layout.
    pub async fn child_blocks(&self, node: BlockId) -> Result<Vec<BlockId>, TreeError> {
        match self.load(node).await?.as_ref() {
            Node::Leaf { .. } => Ok(Vec::new()),
            Node::Internal { children, .. } => Ok(children.clone()),
        }
    }

    /// `child_blocks` over a whole frontier in ONE `get_many` wave — the GC mark walks a level per
    /// round-trip instead of a node per round-trip. Duplicates in `nodes` are fetched
    /// once; the returned child list is the concatenation over DISTINCT input nodes.
    pub async fn child_blocks_many(&self, nodes: &[BlockId]) -> Result<Vec<BlockId>, TreeError> {
        let wave = self.load_wave(nodes).await?;
        Ok(wave
            .into_values()
            .flat_map(|n| match n.as_ref() {
                Node::Leaf { .. } => Vec::new(),
                Node::Internal { children, .. } => children.clone(),
            })
            .collect())
    }
}

/// Merge `msgs` into `dst`, keeping the (key asc, hlc desc) invariant and collapsing duplicate
/// `(key, hlc)` pairs to ONE. Which duplicate survives is load-bearing: a caller stamps *every* message
/// in a single commit with the SAME HLC (`Hlc::advance_past` advances only BETWEEN commits), so two
/// writes to one key in one commit collide on `(key, hlc)`. The LAST-queued message must win —
/// program order is caller intent, so an upsert-then-tombstone in one commit deletes, not resurrects
/// program order. `msgs` are appended after `dst` (they're the newer write), and the
/// explicit position tiebreak (later index first) makes `dedup_by` — which keeps the first of each run
/// — keep the last-queued entry. A re-injected (rebase) duplicate is byte-identical, so either wins.
fn merge_entries(dst: &mut Vec<Entry>, msgs: Vec<Entry>) {
    dst.extend(msgs);
    let mut tagged: Vec<(usize, Entry)> = std::mem::take(dst).into_iter().enumerate().collect();
    // key asc, hlc desc, then LATER original position first — so the surviving (first-of-run) entry is
    // the last-queued one.
    tagged.sort_by(|(ia, a), (ib, b)| {
        a.key
            .cmp(&b.key)
            .then_with(|| b.hlc.cmp(&a.hlc))
            .then_with(|| ib.cmp(ia))
    });
    let mut out: Vec<Entry> = tagged.into_iter().map(|(_, e)| e).collect();
    out.dedup_by(|a, b| a.key == b.key && a.hlc == b.hlc);
    *dst = out;
}

/// Drop per-key versions below the retention `floor`, bounding unbounded history growth for hot keys
/// (see the `split_leaf` note). `entries` is sorted (key asc, hlc desc), so per key the FIRST entry
/// is the current (highest-HLC) winner.
///
/// Read-preserving by construction — safe because reads are LWW-at-read (the winner) and there is no
/// AS-OF read below the floor (a floor is exactly the horizon below which no snapshot can branch):
/// for each key keep every version with `hlc >= floor`, AND always keep the winner even if it
/// is itself below the floor (a rarely-updated key whose only version predates the floor must survive,
/// never resolve to absent). `floor == Hlc::ZERO` keeps everything (the default — current behavior,
/// full time-travel headroom).
fn compact_below_floor(entries: &mut Vec<Entry>, floor: Hlc) {
    if floor == Hlc::ZERO {
        return; // keep-all fast path
    }
    let mut prev_key: Option<Vec<u8>> = None;
    entries.retain(|e| {
        let is_winner = prev_key.as_deref() != Some(e.key.as_slice());
        prev_key = Some(e.key.clone());
        is_winner || e.hlc >= floor // winner (first per key) always kept; others only at/above floor
    });
}

/// Resolve a key from a set of entries (already newest-first): the first matching entry wins (LWW).
fn resolve<'a>(entries: &'a [Entry], key: &[u8]) -> Option<&'a Entry> {
    entries.iter().find(|e| e.key == key)
}

impl<S: NodeStore> MemTree<S> {
    /// Which child index owns `key` given `pivots` (pivots[i] = min key of child i+1).
    fn child_of(pivots: &[Vec<u8>], key: &[u8]) -> usize {
        // First pivot strictly greater than key bounds the child; else the last child.
        pivots.partition_point(|p| p.as_slice() <= key)
    }

    async fn put_node(
        &self,
        mut buffer: Vec<Entry>,
        root: BlockId,
        staged: &mut Staged,
    ) -> Result<BlockId, TreeError> {
        let node = self.load(root).await?;
        // Cached node is shared+immutable; the write path mutates, so clone the fields it rewrites.
        match node.as_ref() {
            Node::Leaf { entries } => {
                let mut entries = entries.clone();
                merge_entries(&mut entries, buffer);
                // Compaction: drop per-key versions below the retention floor (keeping each winner).
                // Bounds a hot key's history; a no-op when floor == ZERO.
                compact_below_floor(&mut entries, self.floor);
                if entries.len() <= FANOUT {
                    return staged.stage(&Node::Leaf { entries });
                }
                // Split into two leaves + a parent. Split at a key boundary (never mid-key-version).
                self.split_leaf(entries, staged)
            }
            Node::Internal {
                pivots,
                children,
                buffer: node_buf,
            } => {
                let pivots = pivots.clone();
                let mut children = children.clone();
                let mut node_buf = node_buf.clone();
                merge_entries(&mut node_buf, std::mem::take(&mut buffer));
                if node_buf.len() <= FANOUT {
                    return staged.stage(&Node::Internal {
                        pivots,
                        children,
                        buffer: node_buf,
                    });
                }
                // Buffer full: flush the heaviest child (Bε lazy flush, impl §11 victim = heaviest).
                let mut by_child: Vec<Vec<Entry>> = vec![Vec::new(); children.len()];
                for e in node_buf {
                    let idx = Self::child_of(&pivots, &e.key);
                    by_child[idx].push(e);
                }
                let heavy = by_child
                    .iter()
                    .enumerate()
                    .max_by_key(|(_, v)| v.len())
                    .map(|(i, _)| i)
                    .unwrap();
                let flushed = std::mem::take(&mut by_child[heavy]);
                children[heavy] = Box::pin(self.put_node(flushed, children[heavy], staged)).await?;
                // Non-heavy groups stay in this node's buffer.
                let remaining: Vec<Entry> = by_child.into_iter().flatten().collect();
                let mut buf = remaining;
                buf.sort_by(|a, b| a.key.cmp(&b.key).then(b.hlc.cmp(&a.hlc)));
                staged.stage(&Node::Internal {
                    pivots,
                    children,
                    buffer: buf,
                })
            }
        }
    }

    /// Split an oversized leaf at a KEY boundary into two child leaves + an internal parent. A key's
    /// full version history must never straddle the split (LWW resolution and `AS OF` read one leaf
    /// per key), so the pivot is always an existing distinct key and `key < pivot` keeps every version
    /// of a key together.
    ///
    /// A leaf of a SINGLE distinct key with > FANOUT versions cannot be key-split (there is no
    /// interior key boundary). We keep it as one leaf rather than manufacture an empty-left child —
    /// which is what produced the unbounded empty-spine growth (review A8). Per-key version count is
    /// bounded by COMPACTION (`compact_below_floor`, run on every leaf rewrite before this split when
    /// the tree has a retention floor), a separate concern from tree fan-out; a hot single key is a
    /// tall-but-single leaf, never a degenerate spine, and with a floor its history stays bounded.
    fn split_leaf(&self, entries: Vec<Entry>, staged: &mut Staged) -> Result<BlockId, TreeError> {
        let mut distinct: Vec<&Vec<u8>> = entries.iter().map(|e| &e.key).collect();
        distinct.dedup();
        if distinct.len() < 2 {
            return staged.stage(&Node::Leaf { entries }); // single key ⇒ can't key-split
        }
        // Pivot at the median distinct key; `< pivot` guarantees a non-empty left (distinct[0] sorts
        // below it) and a non-empty right (pivot itself is present).
        let pivot = distinct[distinct.len() / 2].clone();
        let (left, right): (Vec<Entry>, Vec<Entry>) =
            entries.into_iter().partition(|e| e.key < pivot);
        let lchild = staged.stage(&Node::Leaf { entries: left })?;
        let rchild = staged.stage(&Node::Leaf { entries: right })?;
        staged.stage(&Node::Internal {
            pivots: vec![pivot],
            children: vec![lchild, rchild],
            buffer: Vec::new(),
        })
    }

    /// Walk to `key`, collecting the winning entry along the path: an interior buffer may hold a newer
    /// version than the leaf, so the highest-HLC hit across the whole path wins (LWW-at-read).
    async fn get_entry(&self, root: BlockId, key: &[u8]) -> Result<Option<Entry>, TreeError> {
        let mut cur = root;
        let mut best: Option<Entry> = None;
        loop {
            let node = self.load(cur).await?;
            match node.as_ref() {
                Node::Leaf { entries } => {
                    if let Some(e) = resolve(entries, key) {
                        pick_newer(&mut best, e);
                    }
                    return Ok(best);
                }
                Node::Internal {
                    pivots,
                    children,
                    buffer,
                } => {
                    if let Some(e) = resolve(buffer, key) {
                        pick_newer(&mut best, e);
                    }
                    cur = children[Self::child_of(pivots, key)];
                }
            }
        }
    }

    /// Batched [`get_entry`]: load every node on ANY wanted key's resolution path in O(depth)
    /// `get_many` waves (each node decoded ONCE), then resolve each key by walking that in-memory node
    /// map with the SAME buffer-override/LWW logic as `get_entry`. Results align to `keys`.
    async fn get_entry_many(
        &self,
        root: BlockId,
        keys: &[&[u8]],
    ) -> Result<Vec<Option<Entry>>, TreeError> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        // Keep child `i` iff some wanted key descends into it — so every key's root→leaf path loads.
        let nodes = self
            .fetch_pruned(root, |pivots, i| {
                keys.iter().any(|k| Self::child_of(pivots, k) == i)
            })
            .await?;

        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            let mut cur = root;
            let mut best: Option<Entry> = None;
            while let Some(node) = nodes.get(&cur) {
                match node.as_ref() {
                    Node::Leaf { entries } => {
                        if let Some(e) = resolve(entries, key) {
                            pick_newer(&mut best, e);
                        }
                        break;
                    }
                    Node::Internal {
                        pivots,
                        children,
                        buffer,
                    } => {
                        if let Some(e) = resolve(buffer, key) {
                            pick_newer(&mut best, e);
                        }
                        cur = children[Self::child_of(pivots, key)];
                    }
                }
            }
            out.push(best);
        }
        Ok(out)
    }

    /// Gather the CANDIDATE keys where trees `a` and `b` might diverge, by aligned lockstep descent
    /// — the O(divergence) diff. Equal BlockId ⇒ identical subtree ⇒ prune. Two internal nodes with
    /// equal pivots recurse children pairwise (so shared children prune at their id); a structural
    /// mismatch (pivots differ, or one side is a leaf) collects both subtrees' keys wholesale (a
    /// correctness-preserving over-approximation — the caller resolves each candidate exactly). Buffer
    /// keys are always candidates when the nodes differ.
    fn candidate_keys<'a>(
        &'a self,
        a: BlockId,
        b: BlockId,
        out: &'a mut std::collections::BTreeSet<Vec<u8>>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), TreeError>> + Send + 'a>>
    {
        Box::pin(async move {
            // BFS over ALIGNED PAIRS, one `get_many` wave per level: round-trip depth is
            // O(tree depth), not O(divergent nodes). Structural mismatches defer to a wholesale
            // collect (itself wave-based) after the lockstep walk.
            let mut pairs: Vec<(BlockId, BlockId)> = vec![(a, b)];
            let mut wholesale: Vec<BlockId> = Vec::new();
            while !pairs.is_empty() {
                let ids: Vec<BlockId> = pairs.iter().flat_map(|(x, y)| [*x, *y]).collect();
                let wave = self.load_wave(&ids).await?;
                let mut next: Vec<(BlockId, BlockId)> = Vec::new();
                for (pa_id, pb_id) in pairs {
                    match (wave[&pa_id].as_ref(), wave[&pb_id].as_ref()) {
                        (
                            Node::Internal {
                                pivots: pa,
                                children: ca,
                                buffer: ba,
                            },
                            Node::Internal {
                                pivots: pb,
                                children: cb,
                                buffer: bb,
                            },
                        ) if pa == pb => {
                            // Aligned: same pivots ⇒ children line up 1:1. Equal ids prune.
                            out.extend(ba.iter().map(|e| e.key.clone()));
                            out.extend(bb.iter().map(|e| e.key.clone()));
                            next.extend(
                                ca.iter()
                                    .zip(cb)
                                    .filter(|(ac, bc)| ac != bc)
                                    .map(|(ac, bc)| (*ac, *bc)),
                            );
                        }
                        _ => {
                            // Structural change (or leaf vs internal): both subtrees wholesale.
                            wholesale.push(pa_id);
                            wholesale.push(pb_id);
                        }
                    }
                }
                pairs = next;
            }
            for root in wholesale {
                self.collect_keys(root, out).await?;
            }
            Ok(())
        })
    }

    /// Collect every key reachable from `root` (leaves + interior buffers) — the wholesale arm of
    /// `candidate_keys` when structure diverges. BFS in `get_many` waves: one round-trip per level
    /// not one per node. Key ORDER doesn't matter here (the accumulator is a set).
    fn collect_keys<'a>(
        &'a self,
        root: BlockId,
        out: &'a mut std::collections::BTreeSet<Vec<u8>>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), TreeError>> + Send + 'a>>
    {
        Box::pin(async move {
            let mut frontier = vec![root];
            while !frontier.is_empty() {
                let wave = self.load_wave(&frontier).await?;
                frontier.clear();
                for node in wave.into_values() {
                    match node.as_ref() {
                        Node::Leaf { entries } => out.extend(entries.iter().map(|e| e.key.clone())),
                        Node::Internal {
                            children, buffer, ..
                        } => {
                            out.extend(buffer.iter().map(|e| e.key.clone()));
                            frontier.extend(children.iter().copied());
                        }
                    }
                }
            }
            Ok(())
        })
    }

    /// Like `collect`, but only entries whose key starts with `prefix`. Prunes an interior child when
    /// its key range cannot contain the prefix (child `i` covers `[pivots[i-1], pivots[i])`), so a
    /// narrow prefix touches O(matching subtree + path), not the whole tree.
    async fn collect_prefix(
        &self,
        root: BlockId,
        prefix: &[u8],
        acc: &mut std::collections::BTreeMap<Vec<u8>, Entry>,
    ) -> Result<(), TreeError> {
        // I/O first: prefetch the pruned subtree in one wave per LEVEL, then fold
        // synchronously in the original recursion order (children before buffer — the merge_into
        // tie-break contract).
        let nodes = self
            .fetch_pruned(root, |pivots, i| {
                let lo = i.checked_sub(1).map(|j| pivots[j].as_slice());
                let hi = pivots.get(i).map(|p| p.as_slice());
                child_range_may_contain_prefix(lo, hi, prefix)
            })
            .await?;
        fold_prefix(&nodes, root, prefix, acc);
        Ok(())
    }
}

/// Synchronous post-prefetch fold for `collect_prefix` — identical traversal order to the old
/// recursive form (children in index order, then the interior buffer).
fn fold_prefix(
    nodes: &NodeMap,
    at: BlockId,
    prefix: &[u8],
    acc: &mut std::collections::BTreeMap<Vec<u8>, Entry>,
) {
    match nodes[&at].as_ref() {
        Node::Leaf { entries } => {
            for e in entries {
                if e.key.starts_with(prefix) {
                    merge_into(acc, e);
                }
            }
        }
        Node::Internal {
            pivots,
            children,
            buffer,
        } => {
            for (i, child) in children.iter().enumerate() {
                let lo = i.checked_sub(1).map(|j| pivots[j].as_slice());
                let hi = pivots.get(i).map(|p| p.as_slice());
                if child_range_may_contain_prefix(lo, hi, prefix) {
                    fold_prefix(nodes, *child, prefix, acc);
                }
            }
            for e in buffer {
                if e.key.starts_with(prefix) {
                    merge_into(acc, e);
                }
            }
        }
    }
}

/// The exclusive successor of a prefix (increment with carry, truncating trailing 0xFF). `None` = the
/// prefix is all-0xFF ⇒ unbounded above. The upper bound of "every key starting with `prefix`".
fn prefix_succ(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut hi = prefix.to_vec();
    for i in (0..hi.len()).rev() {
        if hi[i] != 0xFF {
            hi[i] += 1;
            hi.truncate(i + 1);
            return Some(hi);
        }
    }
    None
}

/// Does the child key-range `[lo, hi)` overlap the span `[a, b)` (None = unbounded)? Coarse frontier
/// descend filter for `scan_prefix_many`: over-including is safe because the per-entry bucketing filters
/// exactly. O(1) per child (byte-slice compares), so the descend never scales with frontier size.
fn range_overlaps_span(lo: Option<&[u8]>, hi: Option<&[u8]>, a: &[u8], b: Option<&[u8]>) -> bool {
    let hi_gt_a = hi.is_none_or(|hi| hi > a);
    let lo_lt_b = match (lo, b) {
        (Some(lo), Some(b)) => lo < b,
        _ => true,
    };
    hi_gt_a && lo_lt_b
}

/// SINGLE-PASS bucketing fold: walk the prefetched subtree ONCE and route
/// each entry to its input prefix's bucket by an O(1) hash lookup on its `plen`-byte prefix — the
/// set-membership frontier sweep that makes batched expansion O(entries), not O(frontier × tree). Same
/// traversal order as `fold_prefix` (children in index order, then the interior buffer) so the
/// `merge_into` LWW tie-break is unchanged.
fn fold_into_buckets(
    nodes: &NodeMap,
    at: BlockId,
    plen: usize,
    index: &foldhash::HashMap<&[u8], usize>,
    out: &mut [std::collections::BTreeMap<Vec<u8>, Entry>],
) {
    let route = |e: &Entry, out: &mut [std::collections::BTreeMap<Vec<u8>, Entry>]| {
        if e.key.len() >= plen
            && let Some(&i) = index.get(&e.key[..plen])
        {
            merge_into(&mut out[i], e);
        }
    };
    match nodes[&at].as_ref() {
        Node::Leaf { entries } => {
            for e in entries {
                route(e, out);
            }
        }
        Node::Internal {
            children, buffer, ..
        } => {
            for child in children {
                if nodes.contains_key(child) {
                    fold_into_buckets(nodes, *child, plen, index, out);
                }
            }
            for e in buffer {
                route(e, out);
            }
        }
    }
}

impl<S: NodeStore> MemTree<S> {
    /// Like `collect`, but only entries with key in `[lo, hi)`. Prunes an interior child whose key
    /// range `[clo, chi)` is disjoint from the query window, so a narrow window touches O(window +
    /// path). Buffer/leaf entries are membership-filtered.
    async fn collect_range(
        &self,
        root: BlockId,
        lo: Option<&[u8]>,
        hi: Option<&[u8]>,
        acc: &mut std::collections::BTreeMap<Vec<u8>, Entry>,
    ) -> Result<(), TreeError> {
        // I/O first (one wave per level), then a synchronous fold in the original
        // recursion order — see `collect_prefix`.
        let nodes = self
            .fetch_pruned(root, |pivots, i| {
                let clo = i.checked_sub(1).map(|j| pivots[j].as_slice());
                let chi = pivots.get(i).map(|p| p.as_slice());
                child_range_overlaps(clo, chi, lo, hi)
            })
            .await?;
        fold_range(&nodes, root, lo, hi, acc);
        Ok(())
    }
}

/// Synchronous post-prefetch fold for `collect_range` — children in index order, then the buffer.
fn fold_range(
    nodes: &NodeMap,
    at: BlockId,
    lo: Option<&[u8]>,
    hi: Option<&[u8]>,
    acc: &mut std::collections::BTreeMap<Vec<u8>, Entry>,
) {
    let in_window =
        |k: &[u8]| lo.map(|l| k >= l).unwrap_or(true) && hi.map(|h| k < h).unwrap_or(true);
    match nodes[&at].as_ref() {
        Node::Leaf { entries } => {
            for e in entries {
                if in_window(&e.key) {
                    merge_into(acc, e);
                }
            }
        }
        Node::Internal {
            pivots,
            children,
            buffer,
        } => {
            for (i, child) in children.iter().enumerate() {
                // Child i covers [clo, chi): clo = pivots[i-1] (or unbounded), chi = pivots[i].
                let clo = i.checked_sub(1).map(|j| pivots[j].as_slice());
                let chi = pivots.get(i).map(|p| p.as_slice());
                if child_range_overlaps(clo, chi, lo, hi) {
                    fold_range(nodes, *child, lo, hi, acc);
                }
            }
            for e in buffer {
                if in_window(&e.key) {
                    merge_into(acc, e);
                }
            }
        }
    }
}

/// Do two half-open ranges `[clo, chi)` and `[qlo, qhi)` (open bounds = None) overlap? Disjoint iff
/// the child ends at/before the query start (`chi <= qlo`) or starts at/after the query end
/// (`clo >= qhi`).
fn child_range_overlaps(
    clo: Option<&[u8]>,
    chi: Option<&[u8]>,
    qlo: Option<&[u8]>,
    qhi: Option<&[u8]>,
) -> bool {
    if let (Some(chi), Some(qlo)) = (chi, qlo)
        && chi <= qlo
    {
        return false;
    }
    if let (Some(clo), Some(qhi)) = (clo, qhi)
        && clo >= qhi
    {
        return false;
    }
    true
}

/// Could a child covering `[lo, hi)` (open bounds = None) contain a key starting with `prefix`?
/// Conservative — returns true unless the child range is provably disjoint from every prefix-match.
fn child_range_may_contain_prefix(lo: Option<&[u8]>, hi: Option<&[u8]>, prefix: &[u8]) -> bool {
    // No key starting with `prefix` can be < `prefix`, so if the child ends at or before `prefix`
    // (and hi itself isn't a prefix-extension), the child is entirely below the matches.
    if let Some(hi) = hi
        && hi <= prefix
        && !hi.starts_with(prefix)
    {
        return false;
    }
    // If the child starts strictly past the prefix range and doesn't share the prefix, it's above.
    if let Some(lo) = lo
        && lo > prefix
        && !lo.starts_with(prefix)
    {
        return false;
    }
    true
}

/// LWW keep-or-replace over a BORROWED candidate: the clone (key + full value) happens only when the
/// candidate actually wins — a losing candidate on the walk costs a comparison, not an allocation.
fn pick_newer(best: &mut Option<Entry>, cand: &Entry) {
    if best.as_ref().is_none_or(|b| cand.hlc > b.hlc) {
        *best = Some(cand.clone());
    }
}

/// Fold entry `e` into the accumulator. `collect`/`collect_*` apply CHILDREN first, then the interior
/// BUFFER, so on an exact `(key, hlc)` tie the later-applied buffer entry must win — matching the
/// point-read `pick_newer`, where the buffer (seen first on the root→leaf walk) also wins a tie. Hence
/// `>=`, not `>`: the two read paths agree on a tie. (Same-`(key,hlc)` buffer-vs-leaf pairs cannot arise
/// today — `merge_entries` collapses intra-commit duplicates at root injection before they descend —
/// but keeping the tie-break consistent forecloses a latent point-read-vs-scan divergence.)
fn merge_into(acc: &mut std::collections::BTreeMap<Vec<u8>, Entry>, e: &Entry) {
    // Borrowed candidate: probe first, clone (key + value) ONLY on insert-or-win. The consuming shape
    // cloned every scanned entry into the call, then the key again — 3 allocations per losing row.
    match acc.get_mut(&e.key) {
        Some(cur) => {
            if e.hlc >= cur.hlc {
                *cur = e.clone();
            }
        }
        None => {
            acc.insert(e.key.clone(), e.clone());
        }
    }
}

#[async_trait]
impl<S: NodeStore> Tree for MemTree<S> {
    async fn empty_root(&self) -> Result<BlockId, TreeError> {
        // Standalone `put`: the empty leaf is a single shared node (deduped across every empty stream),
        // not worth a pack — and it's created outside a tree-write walk.
        self.store(&Node::Leaf {
            entries: Vec::new(),
        })
        .await
    }

    async fn tree_put(&self, root: BlockId, msgs: Vec<BTreeMessage>) -> Result<BlockId, TreeError> {
        let mut entries: Vec<Entry> = msgs
            .into_iter()
            .map(|m| Entry {
                key: m.key,
                hlc: m.hlc,
                op: m.op,
            })
            .collect();
        entries.sort_by(|a, b| a.key.cmp(&b.key).then(b.hlc.cmp(&a.hlc)));
        // Walk the COW rewrite staging all new nodes, then land them as ONE packed batch before
        // returning the new root: the batch is durable on return, so a caller can only publish the
        // root after every node it reaches is durable.
        let mut staged = Staged::default();
        let new_root = self.put_node(entries, root, &mut staged).await?;
        self.flush(staged).await?;
        Ok(new_root)
    }

    async fn tree_get(&self, root: BlockId, key: &[u8]) -> Result<Option<Vec<u8>>, TreeError> {
        Ok(self.get_entry(root, key).await?.and_then(|e| match e.op {
            MessageOp::Upsert(v) => Some(v),
            MessageOp::Tombstone => None,
        }))
    }

    async fn tree_get_many(
        &self,
        root: BlockId,
        keys: &[&[u8]],
    ) -> Result<Vec<Option<Vec<u8>>>, TreeError> {
        Ok(self
            .get_entry_many(root, keys)
            .await?
            .into_iter()
            .map(|e| {
                e.and_then(|e| match e.op {
                    MessageOp::Upsert(v) => Some(v),
                    MessageOp::Tombstone => None,
                })
            })
            .collect())
    }

    async fn diff(&self, a: BlockId, b: BlockId) -> Result<Vec<Vec<u8>>, TreeError> {
        if a == b {
            return Ok(Vec::new()); // equal subtree ⇒ zero divergence, zero I/O
        }
        // **O(divergence), not O(corpus)**. Invariant: a key whose resolved value differs has
        // its winning entry in a node UNSHARED by BlockId (if every node on both of k's resolution
        // paths shared an id, resolution would be byte-identical). `candidate_keys` descends the two
        // trees in ALIGNED LOCKSTEP: two internal nodes with the same pivots recurse child-position-
        // wise, so an equal-BlockId child short-circuits (the COW-preserved off-path siblings of a
        // localized edit); only a structural change (a split shifted pivots, or leaf-vs-internal) falls
        // back to collecting that one subtree's keys. Visited work is O(divergence) for localized
        // edits, O(subtree) at a structural change, O(corpus) only under a total reshape — never wrong
        // (candidates are a superset; the per-key resolve below is exact and handles buffer overrides).
        let mut candidates: std::collections::BTreeSet<Vec<u8>> = std::collections::BTreeSet::new();
        self.candidate_keys(a, b, &mut candidates).await?;

        let resolved = |e: Option<Entry>| -> Option<Vec<u8>> {
            match e {
                Some(Entry {
                    op: MessageOp::Upsert(v),
                    ..
                }) => Some(v),
                _ => None, // absent OR tombstone ⇒ observably None
            }
        };
        let mut keys: Vec<Vec<u8>> = Vec::new();
        for k in candidates {
            let va = resolved(self.get_entry(a, &k).await?);
            let vb = resolved(self.get_entry(b, &k).await?);
            if va != vb {
                keys.push(k);
            }
        }
        Ok(keys) // already sorted + unique (BTreeSet iteration)
    }

    async fn scan_prefix(
        &self,
        root: BlockId,
        prefix: &[u8],
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, TreeError> {
        let mut acc = std::collections::BTreeMap::new();
        self.collect_prefix(root, prefix, &mut acc).await?;
        Ok(live_in_order(acc))
    }

    async fn scan_prefix_many(
        &self,
        root: BlockId,
        prefixes: &[&[u8]],
    ) -> Result<Vec<Vec<(Vec<u8>, Vec<u8>)>>, TreeError> {
        if prefixes.is_empty() {
            return Ok(Vec::new());
        }
        // ONE shared descent prefetching the frontier's SPAN — `[min prefix, succ(max prefix))` — with an
        // O(1)-per-child overlap test (never O(frontier), the quadratic trap). One `load_wave` per level
        // across the whole span; covering internal nodes deduped + cache-checked once.
        let min_p = prefixes.iter().min().copied().unwrap();
        let max_p = prefixes.iter().max().copied().unwrap();
        let max_succ = prefix_succ(max_p);
        let nodes = self
            .fetch_pruned(root, |pivots, i| {
                let lo = i.checked_sub(1).map(|j| pivots[j].as_slice());
                let hi = pivots.get(i).map(|p| p.as_slice());
                range_overlaps_span(lo, hi, min_p, max_succ.as_deref())
            })
            .await?;
        let mut out: Vec<std::collections::BTreeMap<Vec<u8>, Entry>> =
            vec![Default::default(); prefixes.len()];
        // Fast path (the traversal case): all prefixes the SAME length ⇒ one bucketing pass keyed by the
        // `plen`-byte prefix (O(entries), not O(frontier × tree)). Otherwise fall back to a per-prefix
        // fold over the same prefetched nodes (correct, no extra I/O — only used for mixed-length inputs).
        let plen = prefixes[0].len();
        if prefixes.iter().all(|p| p.len() == plen) {
            let mut index: foldhash::HashMap<&[u8], usize> =
                foldhash::HashMap::with_capacity_and_hasher(prefixes.len(), Default::default());
            for (i, p) in prefixes.iter().enumerate() {
                index.entry(*p).or_insert(i);
            }
            fold_into_buckets(&nodes, root, plen, &index, &mut out);
        } else {
            for (i, p) in prefixes.iter().enumerate() {
                fold_prefix(&nodes, root, p, &mut out[i]);
            }
        }
        Ok(out.into_iter().map(live_in_order).collect())
    }

    async fn scan_range(
        &self,
        root: BlockId,
        lo: Option<&[u8]>,
        hi: Option<&[u8]>,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, TreeError> {
        let mut acc = std::collections::BTreeMap::new();
        self.collect_range(root, lo, hi, &mut acc).await?;
        Ok(live_in_order(acc))
    }
}

/// MULTI-ROOT scans: every root's pruned subtree prefetches in ONE shared BFS (`fetch_pruned_multi`
/// — one `get_many` wave per LEVEL across the whole root set), then folds per root into one
/// accumulator. Callers are expected to supply roots with DISJOINT key spaces, so the shared BTreeMap
/// is a merge, not an arbitration. Wave count stays O(max tree depth), not O(roots × depth).
impl<S: NodeStore> MemTree<S> {
    pub async fn scan_prefix_roots(
        &self,
        roots: &[BlockId],
        prefix: &[u8],
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, TreeError> {
        let nodes = self
            .fetch_pruned_multi(roots, |pivots, i| {
                let lo = i.checked_sub(1).map(|j| pivots[j].as_slice());
                let hi = pivots.get(i).map(|p| p.as_slice());
                child_range_may_contain_prefix(lo, hi, prefix)
            })
            .await?;
        let mut acc = std::collections::BTreeMap::new();
        for root in dedup_roots(roots) {
            fold_prefix(&nodes, root, prefix, &mut acc);
        }
        Ok(live_in_order(acc))
    }

    pub async fn scan_range_roots(
        &self,
        roots: &[BlockId],
        lo: Option<&[u8]>,
        hi: Option<&[u8]>,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, TreeError> {
        let nodes = self
            .fetch_pruned_multi(roots, |pivots, i| {
                let clo = i.checked_sub(1).map(|j| pivots[j].as_slice());
                let chi = pivots.get(i).map(|p| p.as_slice());
                child_range_overlaps(clo, chi, lo, hi)
            })
            .await?;
        let mut acc = std::collections::BTreeMap::new();
        for root in dedup_roots(roots) {
            fold_range(&nodes, root, lo, hi, &mut acc);
        }
        Ok(live_in_order(acc))
    }

    /// `scan_prefix_many` over many roots — a partitioned frontier expansion. Same span-pruned shared
    /// descent and single-pass bucketing as the single-root form, seeded with every root.
    pub async fn scan_prefix_many_roots(
        &self,
        roots: &[BlockId],
        prefixes: &[&[u8]],
    ) -> Result<Vec<Vec<(Vec<u8>, Vec<u8>)>>, TreeError> {
        if prefixes.is_empty() || roots.is_empty() {
            return Ok(vec![Vec::new(); prefixes.len()]);
        }
        let min_p = prefixes.iter().min().copied().unwrap();
        let max_p = prefixes.iter().max().copied().unwrap();
        let max_succ = prefix_succ(max_p);
        let nodes = self
            .fetch_pruned_multi(roots, |pivots, i| {
                let lo = i.checked_sub(1).map(|j| pivots[j].as_slice());
                let hi = pivots.get(i).map(|p| p.as_slice());
                range_overlaps_span(lo, hi, min_p, max_succ.as_deref())
            })
            .await?;
        let mut out: Vec<std::collections::BTreeMap<Vec<u8>, Entry>> =
            vec![Default::default(); prefixes.len()];
        let plen = prefixes[0].len();
        let roots = dedup_roots(roots);
        if prefixes.iter().all(|p| p.len() == plen) {
            let mut index: foldhash::HashMap<&[u8], usize> =
                foldhash::HashMap::with_capacity_and_hasher(prefixes.len(), Default::default());
            for (i, p) in prefixes.iter().enumerate() {
                index.entry(*p).or_insert(i);
            }
            for root in roots {
                fold_into_buckets(&nodes, root, plen, &index, &mut out);
            }
        } else {
            for root in roots {
                for (i, p) in prefixes.iter().enumerate() {
                    fold_prefix(&nodes, root, p, &mut out[i]);
                }
            }
        }
        Ok(out.into_iter().map(live_in_order).collect())
    }
}

/// Distinct roots in input order (folding one root twice would double-visit its entries).
fn dedup_roots(roots: &[BlockId]) -> Vec<BlockId> {
    let mut seen: foldhash::HashSet<BlockId> = Default::default();
    roots.iter().copied().filter(|r| seen.insert(*r)).collect()
}

/// A resolved-entry accumulator → the live `(key, value)` pairs in key order (BTreeMap iteration),
/// tombstones dropped. Shared by `scan_prefix` and `scan_range`.
fn live_in_order(acc: std::collections::BTreeMap<Vec<u8>, Entry>) -> Vec<(Vec<u8>, Vec<u8>)> {
    acc.into_iter()
        .filter_map(|(k, e)| match e.op {
            MessageOp::Upsert(v) => Some((k, v)),
            MessageOp::Tombstone => None,
        })
        .collect()
}
