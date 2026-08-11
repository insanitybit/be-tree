//! The node-store PORT: the four operations a Bε-tree needs from whatever holds its nodes.
//!
//! The tree is content-addressed — a node's identity IS the hash of its bytes — so it needs no
//! allocation, no free list, and no mutation. That reduces its storage requirement to "give me bytes
//! for this hash" and "here are some bytes, tell me their hash", which is why this port is four methods
//! rather than a filesystem.
//!
//! Implement it over anything: an in-memory map (see [`MemStore`]), a local directory, or a packing
//! layer that coalesces `put_batch` into a few large writes. The tree does not care, and cannot tell.

use std::future::Future;

use bytes::Bytes;

use crate::{AccessHint, BlockId, TreeError};

/// One node staged for a batched write. Newtype rather than a bare `Bytes` so a store implementation
/// can attach its own per-node metadata (a retention horizon, a placement hint) without changing this
/// signature.
#[derive(Debug, Clone)]
pub struct StagedNode(pub Bytes);

/// Where a Bε-tree's nodes live. Content-addressed: `put` returns the hash, and `get(hash)` must return
/// the same bytes forever — so an implementation may cache without invalidation, and two trees that
/// share a subtree share its blocks.
pub trait NodeStore: Send + Sync + 'static {
    /// The host's write-class vocabulary, forwarded verbatim and never interpreted by the tree. A host
    /// with one storage tier uses `()`; a host with a durability ladder passes its own enum.
    type Class: Copy + Default + Send + Sync + 'static;

    /// Fetch one node by content hash.
    fn get(
        &self,
        id: BlockId,
        hint: AccessHint,
    ) -> impl Future<Output = Result<Bytes, TreeError>> + Send;

    /// Fetch many nodes as ONE dependent round trip. This is the method that decides read cost: a tree
    /// descent is O(depth) *waves* only if the frontier at each level is fetched together, so an
    /// implementation should issue these concurrently rather than serially.
    fn get_many(
        &self,
        ids: &[BlockId],
        hint: AccessHint,
    ) -> impl Future<Output = Vec<Result<Bytes, TreeError>>> + Send;

    /// Store one node; returns its content hash.
    fn put(
        &self,
        bytes: Bytes,
        class: Self::Class,
    ) -> impl Future<Output = Result<BlockId, TreeError>> + Send;

    /// Store a whole write walk's new nodes at once. A tree rewrite touches every node on one
    /// root→leaf path, so batching turns O(depth) round trips into one — the difference between a
    /// usable and an unusable tree whenever a request carries real latency.
    fn put_batch(
        &self,
        nodes: Vec<StagedNode>,
        class: Self::Class,
    ) -> impl Future<Output = Result<Vec<BlockId>, TreeError>> + Send;
}

/// An in-memory [`NodeStore`] — the reference implementation, and the one the tests run against.
/// Content-addressed storage is a map from hash to bytes, so this is genuinely all it takes; anything
/// more elaborate (packing, tiering, caching) is an optimisation over the same four methods.
#[derive(Debug, Default)]
pub struct MemStore {
    blocks: std::sync::Mutex<std::collections::HashMap<BlockId, Bytes>>,
}

impl MemStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// How many distinct nodes are stored — the deduplication a content-addressed tree buys you is
    /// visible here as "fewer blocks than writes".
    pub fn len(&self) -> usize {
        self.blocks.lock().expect("mem store").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl NodeStore for MemStore {
    type Class = ();

    async fn get(&self, id: BlockId, _hint: AccessHint) -> Result<Bytes, TreeError> {
        self.blocks
            .lock()
            .expect("mem store")
            .get(&id)
            .cloned()
            .ok_or_else(|| TreeError::Store(format!("absent node {id}")))
    }

    async fn get_many(&self, ids: &[BlockId], hint: AccessHint) -> Vec<Result<Bytes, TreeError>> {
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            out.push(self.get(*id, hint).await);
        }
        out
    }

    async fn put(&self, bytes: Bytes, _class: ()) -> Result<BlockId, TreeError> {
        let id = BlockId::of(&bytes);
        self.blocks.lock().expect("mem store").insert(id, bytes);
        Ok(id)
    }

    async fn put_batch(
        &self,
        nodes: Vec<StagedNode>,
        class: Self::Class,
    ) -> Result<Vec<BlockId>, TreeError> {
        let mut ids = Vec::with_capacity(nodes.len());
        for n in nodes {
            ids.push(self.put(n.0, class).await?);
        }
        Ok(ids)
    }
}
