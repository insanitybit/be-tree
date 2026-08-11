# be-tree

A content-addressed, copy-on-write **Bε-tree**: a write-optimised B-tree that absorbs updates into
per-node message buffers and flushes them downward lazily.

```rust
use be_tree::{BTreeMessage, Hlc, MemTree, Tree, store::MemStore};
use std::sync::Arc;

async fn ex() -> Result<(), be_tree::TreeError> {
    let tree = MemTree::new(Arc::new(MemStore::new()));
    let empty = tree.empty_root().await?;
    
    let root = tree.tree_put(empty, vec![
        BTreeMessage::upsert(b"key".to_vec(), b"value".to_vec(), Hlc { wall_ms: 1, logical: 0 }),
    ]).await?;
    
    assert_eq!(tree.tree_get(root, b"key").await?, Some(b"value".to_vec()));
    // `empty` is still a valid snapshot — writes never mutate.
    assert_eq!(tree.tree_get(empty, b"key").await?, None);
    Ok(())
}
```

## Why these three properties compound

1. **A node's identity is the hash of its bytes.** The tree is therefore a Merkle tree for free: equal
   subtrees share an id, so structural diff is O(divergence) rather than O(size), and a snapshot is just
   a root hash.
2. **Writes are absorbed near the root.** An interior node carries a message buffer, so a commit costs
   O(messages) at the root instead of O(log n) node rewrites. Buffers flush down only when full.
3. **Nothing is ever mutated.** A write rewrites the changed root→leaf path as new nodes and returns a
   new root. Readers holding an older root are unaffected — MVCC with no locks and no version table.

Records are versioned by `Hlc` and resolved **last-writer-wins at read time by descending HLC**, which
makes an optimistic writer's rebase-and-replay safe: re-injecting a lower-HLC message next to a higher
one cannot resurrect it.

## Storage is a four-method port

`NodeStore` is `get` / `get_many` / `put` / `put_batch`. Implement it over a map, a directory, or
anything else that can return bytes for a hash. `get_many` is what keeps a descent O(depth) *round
trips* rather than O(nodes), and `put_batch` is what keeps a write to one round trip instead of one
per node — both matter enormously once a request carries real latency. `store::MemStore` is the
reference implementation and is genuinely all it takes.

## License

Licensed under either of [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your option.
