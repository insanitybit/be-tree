# be-tree

A content-addressed, copy-on-write **buffered B-tree in the Bε-tree family**: updates enter an internal
node's message buffer, a full buffer flushes the heaviest child group one level down, and a read combines
the leaf with the messages found along the root-to-leaf path.

```rust
use be_tree::{BeTree, MemStore, Mutation, VersionStamp};
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
   subtrees share an id, so `diff` skips them, and a snapshot is just a root hash.
2. **Writes are absorbed near the root.** A commit costs O(messages) at the root instead of O(log n) node
   rewrites. Buffers flush downward only when a *byte* budget is exceeded, and the victim is the child
   owning the most pending encoded bytes — never fewer than `MIN_FLUSH_BYTES`.
3. **Nothing is ever mutated.** A write rewrites the changed paths as new nodes and returns a new root.
   Readers holding an older root are unaffected — MVCC with no locks and no version table.

## Nodes are exact-size, canonical, and validated on decode

Every regular node serializes to *exactly* `NODE_BYTES` (64 KiB by default): a fixed header, fixed-capacity
search columns, N+1 offset arrays over one blob region, and canonical zero padding. There is one byte
string per logical node, so rebuilding a node reproduces its `BlockId`.

Decode is a **validated view** over shared `Bytes`, not a struct cast. Ordinary keys remain zero-copy;
the compact long-key leaf reconstructs its canonically prefix-compressed keys and charges those bytes to
the decoded cache. Before any descriptor is
exposed it checks the magic, the exact 128-bit `schema_id`, counts against capacities, offset monotonicity, strict
key order, legal op/span combinations, zeroed padding, and — because a memory-safe but internally
inconsistent node returns *wrong search results* with no UB — it recomputes every stored key head and head
skip from the blob. Malformed bytes are a typed `Decode` error; they never reach search.

In-node search is head-first: the eight bytes at the surface's common-prefix length, read as a big-endian
`u64`, so an integer compare replaces a `memcmp`. The measured implementation dispatches on live
occupancy — a pivot surface holds at most 31 keys, a leaf up to 640, and those want different algorithms.

## Reads are waves; writes are one batch

Multi-key navigation costs **O(tree depth) dependent `get_many` waves** plus at most one batched
value-object wave: a 256-key `get_many` over 100 000 keys takes the same 3 waves as a single key. Probes
are grouped by node so each validated head surface serves every probe assigned to it.

Overlapping cold waves coordinate per content id while preserving one batched fetch for newly owned
ids. Ordered reads and Merkle diffs expose `ScanCursor` and `DiffCursor` for bounded-memory backpressure;
the `Vec`-returning methods are convenience collectors. `ScanCursor::next_batch` batches external-value
fetches as well as node traversal.

Released schemas are explicit. `BeTree::open_known` detects a registered historical root, and
`migrate_from` rewrites its resolved keys, exact order stamps, tombstones, and values into the current
schema without mutating the old object graph. `Format::check_mutation` is the allocation-free predicate
used by `apply`, so callers can reject work before assembling a large batch.

`NodeStore` is `get` / `get_many` / `put_batch`. Two things make the boundary honest: every read carries
the caller's size bound and the operation's remaining byte budget, so no unverified header field ever sizes
a read; and the tree computes each `BlockId` itself and submits *addressed* objects, so a store can neither
assign nor reinterpret identity. `MemStore` is the reference implementation.

Every walk takes a `WorkBudget`. A valid hash authenticates *bytes*, not a promise that an untrusted
writer built a cheap tree.

## Versioning is an opaque total order

Records are ordered by a fixed-width `VersionStamp` supplied per batch, resolved last-writer-wins by
`(order_key, operation_tiebreak)`. The tree never interprets a clock. `hlc` is one optional producer
adapter that packs `(wall_ms, logical, writer_id)` into an order key; a Lamport counter works equally well.
A reused order key is a producer fault, but resolution stays deterministic — and delete wins — on every
replica.

## Public API

The crate root contains the application-facing surface: `BeTree`, `NodeStore`, `Format`, cache and
verification configuration, cursors, mutations, stamps, IDs, and typed errors. The codec and
benchmark machinery remains available for format auditing but is hidden from the normal generated API
index.

The common operations are inherent methods on `BeTree`; users do not need to import an extension trait
or learn a parallel API.

`BeTree::new` selects the supported default format. `BeTree::with_format` opts into an explicitly
validated format; a format's schema ID is part of its persisted-data contract. Builder methods configure
caches, verification, work budgets, opt-in metrics (`record_metrics`), and the store's write class. Scan
and diff cursors expose result batch size and prefetch width independently, so memory/backpressure policy
remains with the caller.

Implementing `NodeStore` requires three operations: bounded `get`, ordered bounded `get_many`, and
batch-result `put_batch`. The store may be local or remote and may compress or tier objects below the
content-address boundary, provided reads return the exact bytes named by each `BlockId`.

## Design

The implementation and benchmark sources are the authoritative design record, including the selected
constants, the benchmark matrix that chose them, and the constant-selection reversals plus the
adversarial-review corrections. Node size is target- and
store-dependent: the capacity benchmark reports realized depth, objects touched, bytes read, and
the assumed cache/transfer model. Its `bytes/lookup` number is a node-transfer model, not a universal
remote-storage prediction; use `objects/lookup` and the store's actual request and billing units
when choosing a format. The measured matrix gives the checked RTT/bandwidth crossover for the 16 KiB
and 64 KiB formats, but does not establish a universal `F_MAX=16` or 16 KiB choice after the current
key-reservation rules.

## Performance evidence

Criterion covers native latency and concurrent throughput. Deterministic external-profiler workloads
add instruction, simulated-cache, branch, and exact allocation evidence. See
[`tools/profile/README.md`](tools/profile/README.md) for the one-command native allocation and
containerized Cachegrind runs, their declared reference-cache model, and the generated hotspot reports.
The checked-in report records the current baseline and the conclusions it supports; none of these
measurements is treated as proof that a particular implementation is optimal.

## License

Licensed under either of [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your option.
