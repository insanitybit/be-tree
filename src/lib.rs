//! A content-addressed, copy-on-write **buffered B-tree in the Bε-tree family**: updates enter an
//! internal node's message buffer, a full buffer flushes the heaviest child group one level down, and a
//! read combines the leaf with the messages found along the root-to-leaf path.
//!
//! Three properties, and they compound:
//!
//! 1. **A node's identity IS the hash of its canonical bytes.** So the tree is a Merkle tree for free:
//!    equal subtrees share a [`BlockId`], which lets [`BeTree::diff`] skip them and makes a snapshot just
//!    a root hash.
//! 2. **Writes are absorbed near the root.** A commit costs O(messages) at the root instead of
//!    O(log n) node rewrites. Buffers flush downward only when a *byte* budget is exceeded, and the
//!    victim is the child owning the most pending encoded bytes — never fewer than
//!    [`Format::min_flush_bytes`](format::Format::min_flush_bytes).
//! 3. **Nothing is ever mutated.** A write rewrites the changed paths as NEW nodes and returns a new
//!    root id. Readers holding an old root are unaffected, so MVCC needs no locks.
//!
//! Every regular node serializes to *exactly* [`Format::node_bytes`](format::Format::node_bytes) —
//! fixed columns, N+1 offset arrays, canonical zero padding — and decode is a *validated* zero-copy
//! view over shared [`bytes::Bytes`], never a struct cast. See [`codec`].
//!
//! Records are ordered by an opaque, fixed-width [`VersionStamp`] supplied per batch, resolved
//! last-writer-wins by the total order `(order_key, operation_tiebreak)`. The tree never interprets a
//! clock; [`hlc`] is one optional adapter that packs an HLC into an order key.
//!
//! Storage is a small port, [`NodeStore`]: the tree computes each [`BlockId`] itself and submits
//! *addressed* objects, so a store can neither rename nor reinterpret content.

/// The README's example is compiled as a doctest, so it cannot drift from the API.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

// These modules stay public so format auditors and benchmark authors can inspect the implementation,
// but they are not the application-facing API. The supported façade is re-exported below.
mod cache;
#[doc(hidden)]
pub mod codec;
#[doc(hidden)]
pub mod format;
pub mod hlc;
mod inflight;
mod metrics;
#[doc(hidden)]
pub mod search;
#[doc(hidden)]
pub mod store;
#[doc(hidden)]
pub mod tree;
#[doc(hidden)]
pub mod value;

pub use format::{Format, FormatParams, SchemaId};
pub use metrics::{HistogramSnapshot, MetricsSnapshot};
pub use store::{AddressedObject, MemStore, NodeStore};
pub use tree::{
    BeTree, CacheConfig, DEFAULT_PREFETCH_WIDTH, DiffCursor, MigrationReport, ScanCursor,
    VerifyPolicy,
};

use bytes::Bytes;

/// Width of the opaque order key, in bytes. A format constant: one exact width, one representation,
/// no negotiation. 28 is the smallest width that the [`hlc`] adapter can fill exactly
/// (`u64` wall_ms + `u32` logical + 16-byte writer id), so the adapter needs no padding scheme.
pub const VERSION_BYTES: usize = 28;

/// The opaque, fixed-width, totally ordered version key that stamps one batch. The tree compares it
/// lexicographically as unsigned bytes and never reinterprets its internal fields — a producer may
/// pack an HLC (see [`hlc`]), a Lamport counter, or anything else canonical into it.
///
/// Version keys are *ordering tokens, not authentication credentials*. The tree applies the total
/// order; it cannot validate that a producer followed its clock or commit protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VersionStamp {
    pub order_key: [u8; VERSION_BYTES],
}

impl VersionStamp {
    pub const ZERO: VersionStamp = VersionStamp {
        order_key: [0; VERSION_BYTES],
    };

    pub const fn new(order_key: [u8; VERSION_BYTES]) -> Self {
        VersionStamp { order_key }
    }

    /// A stamp from a big-endian `u64` in the most significant position — the simplest canonical
    /// total order (a Lamport counter), for tests and single-writer producers.
    pub fn from_counter(n: u64) -> Self {
        let mut order_key = [0u8; VERSION_BYTES];
        order_key[..8].copy_from_slice(&n.to_be_bytes());
        VersionStamp { order_key }
    }
}

/// A node's address: BLAKE3-256 of its exact stored bytes. Content-derived ⇒ a `BlockId` denotes the
/// same bytes forever ⇒ a cached decode stays coherent forever and nothing above the store needs
/// invalidating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(pub [u8; 32]);

impl BlockId {
    /// Address canonical object bytes with BLAKE3-256.
    pub fn of(bytes: &[u8]) -> Self {
        BlockId(*blake3::hash(bytes).as_bytes())
    }
    /// 2-byte fan-out prefix, so keys derived from a `BlockId` spread evenly across a store's
    /// partitions.
    pub fn fanout(&self) -> [u8; 2] {
        [self.0[0], self.0[1]]
    }
}

impl From<[u8; 32]> for BlockId {
    fn from(bytes: [u8; 32]) -> Self {
        BlockId(bytes)
    }
}

impl From<BlockId> for [u8; 32] {
    fn from(id: BlockId) -> Self {
        id.0
    }
}

impl AsRef<[u8; 32]> for BlockId {
    fn as_ref(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Display for BlockId {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}", blake3::Hash::from(self.0).to_hex())
    }
}

/// Intent flows DOWN: the caller hints the read pattern; the [`NodeStore`] owns the mechanism.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AccessHint {
    #[default]
    Random,
    /// next-hop / adjacent-range prefetch
    SequentialPrefetch,
    /// interior nodes over leaves — the highest-ROI cache lever
    MetadataOnly,
}

/// What the object being fetched or written is, so a store can tier nodes and payloads differently
/// without parsing bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    Node,
    Value,
}

/// One caller mutation. All mutations in a batch share the batch's [`VersionStamp`]; repeated keys
/// collapse to the **last** mutation in program order before the batch reaches the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mutation {
    pub key: Bytes,
    pub op: MutationOp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MutationOp {
    Upsert(Bytes),
    Tombstone,
}

impl Mutation {
    pub fn upsert(key: impl Into<Bytes>, value: impl Into<Bytes>) -> Self {
        Mutation {
            key: key.into(),
            op: MutationOp::Upsert(value.into()),
        }
    }
    pub fn tombstone(key: impl Into<Bytes>) -> Self {
        Mutation {
            key: key.into(),
            op: MutationOp::Tombstone,
        }
    }
}

/// The resolved winner for a key: its order key plus the operation that won. The `Ord` impl **is**
/// the implementation's total order — see [`WinnerOp`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Winner {
    pub(crate) order_key: [u8; VERSION_BYTES],
    pub(crate) op: WinnerOp,
}

/// The persisted operation of a winning candidate. Its ordering rank is the implementation's exact
/// `operation_tiebreak`: `(0, inline_value_bytes)`, `(1, ValueObject_ID, logical_length)`, `(2)`.
/// Delete therefore wins a reused order key, deterministically and without fetching a value object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WinnerOp {
    Inline(Bytes),
    External { id: BlockId, len: u32 },
    Tombstone,
}

impl WinnerOp {
    /// The leading discriminant of `operation_tiebreak`. Inline < External < Tombstone.
    fn rank(&self) -> u8 {
        match self {
            WinnerOp::Inline(_) => 0,
            WinnerOp::External { .. } => 1,
            WinnerOp::Tombstone => 2,
        }
    }
}

impl Ord for WinnerOp {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match self.rank().cmp(&other.rank()) {
            Ordering::Equal => match (self, other) {
                // Byte strings compare lexicographically as unsigned bytes...
                (WinnerOp::Inline(a), WinnerOp::Inline(b)) => a.as_ref().cmp(b.as_ref()),
                // ...and `logical_length` numerically.
                (
                    WinnerOp::External { id: ia, len: la },
                    WinnerOp::External { id: ib, len: lb },
                ) => ia.0.cmp(&ib.0).then(la.cmp(lb)),
                _ => Ordering::Equal,
            },
            ord => ord,
        }
    }
}
impl PartialOrd for WinnerOp {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Winner {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.order_key
            .cmp(&other.order_key)
            .then_with(|| self.op.cmp(&other.op))
    }
}
impl PartialOrd for Winner {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Caller/default limits on the work one public operation may perform: objects visited and bytes
/// fetched. A valid hash authenticates *bytes*, not a promise that an untrusted writer built a cheap
/// tree — so every walk is bounded before it runs.
///
/// Cache hits do not consume `max_fetched_bytes` but DO consume `max_objects`, so a malicious DAG
/// cannot turn sharing into unbounded CPU work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkBudget {
    pub max_objects: u64,
    pub max_fetched_bytes: u64,
}

impl Default for WorkBudget {
    fn default() -> Self {
        WorkBudget {
            max_objects: 1 << 20,
            max_fetched_bytes: 1 << 32,
        }
    }
}

impl WorkBudget {
    pub const UNLIMITED: WorkBudget = WorkBudget {
        max_objects: u64::MAX,
        max_fetched_bytes: u64::MAX,
    };
}

/// A live budget: what remains of a [`WorkBudget`] partway through one operation.
#[derive(Debug)]
pub(crate) struct BudgetState {
    objects: u64,
    bytes: u64,
}

impl BudgetState {
    pub(crate) fn new(b: WorkBudget) -> Self {
        BudgetState {
            objects: b.max_objects,
            bytes: b.max_fetched_bytes,
        }
    }

    /// Checked-subtract `n` object visits. Called *before* a wave, so an over-budget walk never issues
    /// the fetch.
    pub(crate) fn visit(&mut self, n: u64) -> Result<(), TreeError> {
        self.objects = self
            .objects
            .checked_sub(n)
            .ok_or(TreeError::ResourceLimit {
                what: "objects visited",
            })?;
        Ok(())
    }

    /// The remaining fetched-byte allowance, handed to the store so a batched read cannot return more.
    pub(crate) fn remaining_bytes(&self) -> u64 {
        self.bytes
    }

    pub(crate) fn spend_bytes(&mut self, n: u64) -> Result<(), TreeError> {
        self.bytes = self.bytes.checked_sub(n).ok_or(TreeError::ResourceLimit {
            what: "bytes fetched",
        })?;
        Ok(())
    }
}

/// Why a write was rejected before any object was stored. Every variant is a declared limit of the
/// configured [`Format`], never an accidental failure deep in the write path.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CapacityError {
    #[error("key of {len} bytes exceeds max_key_bytes {limit}")]
    KeyTooLarge { len: usize, limit: usize },
    #[error("value of {len} bytes exceeds max_value_bytes {limit}")]
    ValueTooLarge { len: usize, limit: usize },
    #[error("object of {len} bytes exceeds max_object_bytes {limit}")]
    ObjectTooLarge { len: usize, limit: usize },
    #[error("tree would grow to level {level}, above max_tree_level {limit}")]
    TreeTooTall { level: u32, limit: u16 },
    #[error("invalid format configuration: {0}")]
    Format(String),
}

/// What can go wrong. The classes are deliberately distinct because they imply different failures in
/// the store, the writer, or the caller — callers must not parse strings to tell them apart.
#[derive(Debug, Clone, thiserror::Error)]
pub enum TreeError {
    /// An invalid configuration, or a write above a declared key/value/object/depth limit. No root is
    /// published.
    #[error("capacity: {0}")]
    Capacity(#[from] CapacityError),
    /// One operation's [`WorkBudget`] would be exceeded. No partial logical result is returned.
    #[error("resource limit exceeded: {what}")]
    ResourceLimit { what: &'static str },
    /// Returned bytes do not hash to the requested id: bit rot, or the wrong object.
    #[error("hash mismatch: requested {requested}, bytes hash to {actual}")]
    HashMismatch { requested: BlockId, actual: BlockId },
    /// Bytes are not a canonical object of this format: a broken writer, or an attack.
    #[error("decode {}: {reason}", DisplayId(*id))]
    Decode {
        id: Option<BlockId>,
        reason: DecodeError,
    },
    /// A structurally valid node built by a different producer protocol.
    #[error(
        "version domain mismatch: expected {}, found {}",
        hex16(expected),
        hex16(found)
    )]
    VersionDomainMismatch { expected: [u8; 16], found: [u8; 16] },
    /// Transport or durability failure inside the [`NodeStore`].
    #[error("node store: {0}")]
    Store(String),
}

impl TreeError {
    pub fn decode(id: Option<BlockId>, reason: DecodeError) -> Self {
        TreeError::Decode { id, reason }
    }
    pub fn store(msg: impl std::fmt::Display) -> Self {
        TreeError::Store(msg.to_string())
    }
}

struct DisplayId(Option<BlockId>);
impl std::fmt::Display for DisplayId {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self.0 {
            Some(id) => write!(f, "of {id}"),
            None => write!(f, "of unaddressed bytes"),
        }
    }
}

fn hex16(b: &[u8; 16]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The specific canonicality rule that stored bytes broke. Enumerated rather than stringly-typed so
/// corruption tests can assert *which* invariant caught a mutation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("wrong length: {found} bytes, expected {expected}")]
    Length { found: usize, expected: usize },
    #[error("bad magic")]
    Magic,
    #[error("schema id {found} is not {expected}")]
    SchemaId { found: String, expected: String },
    #[error("unknown node kind {0}")]
    Kind(u8),
    #[error("non-canonical leaf layout: found kind {found}, expected {expected}")]
    LeafKind { found: u8, expected: u8 },
    #[error("flags must be zero, found {0:#x}")]
    Flags(u8),
    #[error("reserved bytes must be zero ({0})")]
    Reserved(&'static str),
    #[error("count out of range: {what} = {found}, capacity {capacity}")]
    Count {
        what: &'static str,
        found: u64,
        capacity: u64,
    },
    #[error("child_count {child} must equal pivot_count {pivot} + 1")]
    Cardinality { child: u16, pivot: u16 },
    #[error("tree_level {found} illegal for this kind (max {max})")]
    TreeLevel { found: u16, max: u16 },
    #[error("offsets not monotonic at index {0}")]
    OffsetMonotonicity(usize),
    #[error("final offset {found} must equal blob region length {expected}")]
    BlobLength { found: u64, expected: u64 },
    #[error("offset {found} exceeds blob capacity {capacity}")]
    OffsetRange { found: u64, capacity: u64 },
    #[error("padding or an unused slot is nonzero ({0})")]
    Padding(&'static str),
    #[error("{what} are not strictly ascending at index {index}")]
    Order { what: &'static str, index: usize },
    #[error("illegal op byte {op} at entry {index}")]
    Op { op: u8, index: usize },
    #[error("op/value-span mismatch at entry {index}: {why}")]
    Span { index: usize, why: &'static str },
    #[error("external_value_len illegal at entry {index}: {why}")]
    ExternalLen { index: usize, why: &'static str },
    #[error("stored key head at {index} disagrees with the key bytes")]
    Head { index: usize },
    #[error("stored head skip {found} disagrees with the recomputed common prefix {expected}")]
    HeadSkip { found: u32, expected: u32 },
    #[error("key of {len} bytes exceeds max_key_bytes {limit}")]
    KeyTooLarge { len: usize, limit: usize },
    #[error("child at index {0} is a zeroed slot")]
    ZeroChild(usize),
    #[error("child tree_level {child} is not one below parent {parent}")]
    ChildLevel { child: u16, parent: u16 },
    #[error("value envelope: {0}")]
    ValueEnvelope(&'static str),
    #[error("value length {found} disagrees with the authenticated length {expected}")]
    ValueLength { found: u32, expected: u32 },
    #[error("batched read returned {found} results for {expected} ids")]
    BatchCardinality { found: usize, expected: usize },
    #[error("object of {found} bytes exceeds the requested bound {limit}")]
    OversizeObject { found: usize, limit: usize },
}
