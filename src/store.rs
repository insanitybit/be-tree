//! The node-store PORT: what a buffered tree needs from whatever holds its objects.
//!
//! Two contract changes make the boundary honest:
//!
//! - **Retrieval is bounded before decode.** Every read carries the caller's `max_object_bytes`, and a
//!   batched read carries the aggregate `max_total_bytes` left in the operation's
//!   [`WorkBudget`](crate::WorkBudget). A
//!   store must not allocate or return more. No unverified header field ever sizes a read.
//! - **The tree addresses its own writes.** It computes each [`BlockId`] locally and submits
//!   *addressed* objects, so a store can neither assign nor reinterpret identity — and must reject an
//!   id/bytes mismatch.
//!
//! A batched read returns exactly one result per requested id, in input order. The tree rejects a
//! cardinality mismatch *before* associating any bytes with any id.

use std::future::Future;

use bytes::Bytes;

use crate::{AccessHint, BlockId, TreeError};

/// One addressed object staged for a batched write: the id the tree computed, and its exact bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressedObject {
    pub id: BlockId,
    pub bytes: Bytes,
}

/// Where a tree's objects live. Content-addressed: `get(id)` must return the same bytes forever, so an
/// implementation may cache without invalidation, and two trees that share a subtree share its objects.
pub trait NodeStore: Send + Sync + 'static {
    /// The host's write-class vocabulary, forwarded verbatim and never interpreted by the tree.
    type Class: Copy + Default + Send + Sync + 'static;

    /// Fetch one object. The store must not return more than `max_object_bytes`.
    fn get(
        &self,
        id: BlockId,
        hint: AccessHint,
        max_object_bytes: usize,
    ) -> impl Future<Output = Result<Bytes, TreeError>> + Send;

    /// Fetch many objects as ONE dependent round trip — the method that decides read cost, since a
    /// descent is O(depth) *waves* only if each level's frontier is fetched together.
    ///
    /// Returns exactly `ids.len()` results in input order. The total returned bytes must not exceed
    /// `max_total_bytes`, and no single object may exceed `max_object_bytes`.
    fn get_many(
        &self,
        ids: &[BlockId],
        hint: AccessHint,
        max_object_bytes: usize,
        max_total_bytes: u64,
    ) -> impl Future<Output = Vec<Result<Bytes, TreeError>>> + Send;

    /// Make every supplied object durable, or fail. The store must reject an id/bytes mismatch.
    ///
    /// It MAY leave a prefix durable on failure: unreferenced content-addressed objects are harmless,
    /// and the tree publishes no new root unless the whole batch succeeded. Duplicate ids are
    /// idempotent only when their bytes are identical.
    fn put_batch(
        &self,
        objects: Vec<AddressedObject>,
        class: Self::Class,
    ) -> impl Future<Output = Result<(), TreeError>> + Send;
}

/// An in-memory [`NodeStore`] — the reference implementation, and the one the tests run against.
#[derive(Debug, Default)]
pub struct MemStore {
    blocks: std::sync::Mutex<std::collections::HashMap<BlockId, Bytes>>,
}

impl MemStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// How many distinct objects are stored — the deduplication content addressing buys you is visible
    /// here as "fewer objects than writes".
    pub fn len(&self) -> usize {
        self.blocks.lock().expect("mem store").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Raw stored bytes, bypassing the tree. Golden-vector, corruption, and shape tests need to inspect
    /// or rewrite stored bytes directly.
    #[doc(hidden)]
    pub fn raw(&self, id: BlockId) -> Option<Bytes> {
        self.blocks.lock().expect("mem store").get(&id).cloned()
    }

    /// Overwrite the bytes stored *at* `id` without re-addressing them — the only way to simulate bit
    /// rot or a lying store, which is what verify-on-read exists to catch.
    #[doc(hidden)]
    pub fn corrupt(&self, id: BlockId, bytes: Bytes) {
        self.blocks.lock().expect("mem store").insert(id, bytes);
    }

    #[doc(hidden)]
    pub fn ids(&self) -> Vec<BlockId> {
        self.blocks
            .lock()
            .expect("mem store")
            .keys()
            .copied()
            .collect()
    }
}

impl NodeStore for MemStore {
    type Class = ();

    async fn get(
        &self,
        id: BlockId,
        _hint: AccessHint,
        max_object_bytes: usize,
    ) -> Result<Bytes, TreeError> {
        let bytes = self
            .blocks
            .lock()
            .expect("mem store")
            .get(&id)
            .cloned()
            .ok_or_else(|| TreeError::Store(format!("absent object {id}")))?;
        check_size(bytes, max_object_bytes)
    }

    /// Acquires the mutex ONCE and clones all requested byte strings in input order. Spawning
    /// concurrent lookups against one mutex would add overhead and would not model a remote store's
    /// concurrency anyway.
    async fn get_many(
        &self,
        ids: &[BlockId],
        _hint: AccessHint,
        max_object_bytes: usize,
        max_total_bytes: u64,
    ) -> Vec<Result<Bytes, TreeError>> {
        let guard = self.blocks.lock().expect("mem store");
        let mut total = 0u64;
        ids.iter()
            .map(|id| {
                let bytes = guard
                    .get(id)
                    .cloned()
                    .ok_or_else(|| TreeError::Store(format!("absent object {id}")))?;
                let bytes = check_size(bytes, max_object_bytes)?;
                total = total.saturating_add(bytes.len() as u64);
                if total > max_total_bytes {
                    return Err(TreeError::ResourceLimit {
                        what: "bytes fetched",
                    });
                }
                Ok(bytes)
            })
            .collect()
    }

    async fn put_batch(&self, objects: Vec<AddressedObject>, _class: ()) -> Result<(), TreeError> {
        // Reject an id/bytes mismatch: the store is the last place that can catch a broken writer.
        for o in &objects {
            let actual = BlockId::of(&o.bytes);
            if actual != o.id {
                return Err(TreeError::HashMismatch {
                    requested: o.id,
                    actual,
                });
            }
        }
        let mut guard = self.blocks.lock().expect("mem store");
        for o in objects {
            guard.insert(o.id, o.bytes);
        }
        Ok(())
    }
}

fn check_size(bytes: Bytes, limit: usize) -> Result<Bytes, TreeError> {
    if bytes.len() > limit {
        return Err(TreeError::decode(
            None,
            crate::DecodeError::OversizeObject {
                found: bytes.len(),
                limit,
            },
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
#[path = "../tests/support/store_unit.rs"]
mod tests;
