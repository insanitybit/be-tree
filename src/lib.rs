//! A content-addressed, copy-on-write **Bε-tree**: a write-optimised B-tree that absorbs updates into
//! per-node message buffers and flushes them downward lazily.
//!
//! Three properties, and they compound:
//!
//! 1. **A node's identity IS the hash of its bytes.** So the tree is a Merkle tree for free: equal
//!    subtrees share an id, which makes structural diff O(divergence) rather than O(size), and makes a
//!    snapshot just a root hash.
//! 2. **Writes are absorbed near the root.** An interior node carries a small message buffer, so a
//!    commit costs O(messages) at the root instead of O(log n) node rewrites. Buffers flush downward
//!    only when full.
//! 3. **Nothing is ever mutated.** A write rewrites the changed root→leaf path as NEW nodes and returns
//!    a new root id. Readers holding an old root are unaffected, so MVCC needs no locks and no
//!    versioning machinery.
//!
//! Records are versioned by [`Hlc`] and resolved **last-writer-wins at read time by descending HLC**,
//! which is what makes re-injecting a lower-HLC message next to a higher one harmless — an optimistic
//! writer can rebase and replay without needing idempotence.
//!
//! Storage is a four-method port, [`NodeStore`]: implement it over a map, a directory, or anything
//! else that can return bytes for a hash. `put_batch` is the one that matters when a request costs a
//! round trip — a tree rewrite batches its whole path into a single one.

use serde::{Deserialize, Serialize};

pub mod store;
pub mod tree;

pub use store::{NodeStore, StagedNode};
pub use tree::{MemTree, Tree};

/// What can go wrong in a tree operation. `Store` wraps whatever the [`NodeStore`] returned, so an
/// implementation's own error type stays visible through the boxed source.
#[derive(Debug, thiserror::Error)]
pub enum TreeError {
    #[error("node store: {0}")]
    Store(String),
    #[error("decode failure on node {0}: {1}")]
    Decode(BlockId, String),
    #[error("schema mismatch: {0}")]
    Schema(String),
    #[error("codec: {0}")]
    Codec(String),
}

/// A node's address: BLAKE3-256 of its bytes. Content-derived ⇒ a BlockId denotes the same bytes
/// forever ⇒ a cached decode stays coherent forever and nothing above the store needs invalidating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BlockId(pub [u8; 32]);

impl BlockId {
    pub fn of(bytes: &[u8]) -> Self {
        BlockId(*blake3::hash(bytes).as_bytes())
    }
    /// 2-byte fan-out prefix, so keys derived from a BlockId spread evenly across a store's partitions.
    pub fn fanout(&self) -> [u8; 2] {
        [self.0[0], self.0[1]]
    }
}
impl std::fmt::Display for BlockId {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        // blake3's hex, not a hand-rolled per-byte format! loop.
        write!(f, "{}", blake3::Hash::from(self.0).to_hex())
    }
}
/// Hybrid Logical Clock. Exact within a single lineage of writes; ε-bounded across independent
/// lineages (NTP SLA, default ε = 250ms).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Hlc {
    pub wall_ms: u64,
    pub logical: u32,
}
impl Hlc {
    /// The genesis / "no lower bound" clock. A retention floor of `ZERO` means "retain everything"
    /// (nothing is below it), and genesis commits start here.
    pub const ZERO: Hlc = Hlc {
        wall_ms: 0,
        logical: 0,
    };

    /// Advance strictly past `base`, installing a happens-before edge through the store.
    pub fn advance_past(base: Hlc, local_wall_ms: u64) -> Hlc {
        if local_wall_ms > base.wall_ms {
            Hlc {
                wall_ms: local_wall_ms,
                logical: 0,
            }
        } else {
            Hlc {
                wall_ms: base.wall_ms,
                logical: base.logical + 1,
            }
        }
    }

    /// The CONFLUENT successor of two clocks: strictly greater than both, derived from NOTHING but the
    /// inputs (no wall clock). This is what makes a merge a pure function of its parent commits — two
    /// nodes joining the same pair compute the same Hlc, hence (with deterministic encoding) the same
    /// commit bytes and the same CommitId. CALM in one line: the join of a semilattice needs no clock.
    pub fn join(a: Hlc, b: Hlc) -> Hlc {
        let m = a.max(b);
        Hlc {
            wall_ms: m.wall_ms,
            logical: m.logical + 1,
        }
    }
}

/// Intent flows DOWN: the caller hints the read pattern; the [`NodeStore`] owns the mechanism.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AccessHint {
    #[default]
    Random,
    SequentialPrefetch, // next-hop / adjacent-range prefetch
    MetadataOnly,       // interior nodes over leaves — the highest-ROI cache lever
}
/// A Bε buffer entry: absorbed near the root, flushed lazily. HLC-version-stamped and resolved by
/// LWW at read/compaction time — the READER picks the winner by HLC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BTreeMessage {
    pub key: Vec<u8>,
    pub op: MessageOp,
    pub hlc: Hlc,
}
impl BTreeMessage {
    pub fn upsert(key: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>, hlc: Hlc) -> Self {
        BTreeMessage {
            key: key.into(),
            op: MessageOp::Upsert(value.into()),
            hlc,
        }
    }
    pub fn tombstone(key: impl Into<Vec<u8>>, hlc: Hlc) -> Self {
        BTreeMessage {
            key: key.into(),
            op: MessageOp::Tombstone,
            hlc,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageOp {
    Upsert(Vec<u8>),
    Tombstone,
}
