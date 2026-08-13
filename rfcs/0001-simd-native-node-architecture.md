# RFC 0001: Flat, wave-oriented buffered-tree nodes

**Status:** Implemented and internally validated; **deployment validation remains target-specific.** Schema id
`b91588e3375d113c275d2976c650698f` (selected V3) / `718b5338e1b709eeb01079cf58967ed6`
(tiny V3). Historical selected V2 is registered as `d14377a94570af8b3f22520538a93d3f`.  
**Scope:** Node representation, traversal, and the supporting `BeTree`/`NodeStore` contracts. Public
APIs may make breaking changes, but a released selected schema is never silently reinterpreted. This
RFC defines the read/migration policy below.

Every undecided constant and implementation variant below has been selected from the measurements
recorded in [Selected constants and measured decisions](#selected-constants-and-measured-decisions).
Four constant-selection decisions went *against* the expectation this RFC and its research notes started from;
each is called out there rather than quietly reversed in the prose.

The structural design is implemented and tested. An independent review then found that several real
storage, caching, and concurrency costs were missing from both the implementation and the benchmarks;
those are itemized, fixed, and pinned by tests in
[Storage, caching, and concurrency corrections](#storage-caching-and-concurrency-corrections), and what
the follow-up review gaps are closed in
[Storage, caching, and concurrency corrections](#storage-caching-and-concurrency-corrections). The one
remaining boundary is external: supply the target store's measured RTT and bandwidth before freezing
`NODE_BYTES`. Do not treat local compute timing as a storage-system commitment.

A downstream integration exposed six requirements that belong to the crate rather than to that
application: durable roots must survive schema changes, 4 KiB identity keys must not collapse fanout or
amplification, callers need exact preflight, scans and diffs need bounded-memory backpressure, hash
policy needs a contended workload, and Miri needs one unfiltered command. V3 and the associated API/test
work close all six; each deployment still needs the target-store measurement described above.

## Summary

Replace postcard-encoded, allocation-heavy nodes with a canonical flat byte format; repair the
current binary-tree degeneration with multiway B-tree split propagation; and make a BFS wave the
unit of decode, search, hash, and verification.

The result is a practical, content-addressed **buffered B-tree in the Bε-tree family**. It preserves
the defining Bε mechanism: updates enter an internal-node buffer, full buffers flush a large group
toward one child, and reads combine the leaf with messages found along its root-to-leaf path. It is
not a literal implementation of the external-memory model from the original analysis:

- records have variable-sized byte keys and values;
- every changed snapshot is named by a Merkle root, so any apply that changes tree bytes writes at
  least a root block;
- winners are ordered by an application-supplied total version key rather than update arrival; and
- `NodeStore` exposes batched content-addressed objects rather than mutable disk pages.

Accordingly, this RFC does not claim the classical sub-one-I/O amortized update bound for each
independently published commit. It claims and measures three narrower properties:

1. regular nodes have a fixed physical byte budget and multiway fanout;
2. a downward flush chooses the child owning the most pending encoded bytes; and
3. multi-key navigation performs O(tree depth) dependent `get_many` waves, followed by at most one
   batched value-object wave for out-of-line winners.

Vector search is an experiment, not an architectural commitment. The format exposes a contiguous
head column that permits branchless, autovectorized, or hand-written SIMD search; the benchmark
matrix chooses among them. This execution work is not part of the Bε-tree literature.

The supporting survey is in [`research/`](research/README.md). Its measurements guide the experiment
order, but acceptance still requires reproduction in this crate.

## Source model and terminology

Brodal and Fagerberg introduced the Bε-tree trade-off in the external-memory model. The practical
description used by this RFC is Bender et al., *An Introduction to Bε-trees and
Write-Optimization* [2]. In the unit-item model, a size-`B` internal node has approximately
`B^ε` children and `B - B^ε` buffered messages, for `0 < ε ≤ 1`. A full buffer therefore contains
enough messages that its heaviest child owns at least

```text
(B - B^ε) / B^ε = Θ(B^(1-ε))
```

messages. This produces the classical query/update trade-off. The analysis assumes unit-size
items, mutable pages, and amortization across operations.

Real implementations use variable-sized records, a fixed physical node size, and a fixed fanout.
In that setting `ε` is not known exactly a priori [2]. This RFC therefore uses the following terms:

- `NODE_BYTES`: serialized size of a regular node.
- `F_MAX`: maximum children in a regular internal node.
- `BUFFER_BYTES`: bytes reserved for buffered messages, including descriptors.
- `MIN_FLUSH_BYTES`: smallest ordinary child group worth rewriting under COW.
- `LEAF_SLOTS` and `MESSAGE_SLOTS`: descriptor capacities, guarding workloads made of tiny items.
- `VERSION_BYTES`: fixed width of the opaque order key in one exact format.
- `MAX_OBJECT_BYTES`, `MAX_KEY_BYTES`, and `MAX_VALUE_BYTES`: hard limits enforced at the API/store
  boundary. Their errors are part of the new contract.
- `MAX_TREE_LEVEL`: hard bound on authenticated tree depth and therefore on dependent read waves.
- `WorkBudget`: caller/default limits on objects visited and bytes fetched by one operation.
- **effective ε**: a diagnostic reported for a fixed-size benchmark fixture as
  `ln(F_realized) / ln(B_fixture)`, where `B_fixture` is the number of that fixture's items that fit
  in a regular node. It is not a format constant or a universal complexity claim.

For a bounded-item model, choosing `F_MAX = Θ(B^ε)` and `BUFFER_BYTES = Θ(B - B^ε)` recovers the
usual Bε parameterization below the Merkle root. The implementation reports realized fanout, flush
bytes, and oversize frequency instead of pretending that an entry count proves the external-memory
bound.

### Conformance boundary

The following behavior is source-derived and load-bearing:

- insert and tombstone messages enter at the root;
- internal buffers are persisted as part of their nodes;
- a point query logically consults every buffer on the correct root-to-leaf path; an exact or
  no-false-negative routing summary may prove that a physical buffer probe is unnecessary;
- a full buffer flushes messages toward a child, normally the heaviest child;
- leaves split at key boundaries;
- child splits widen their parent, and overfull internal builders split; and
- an internal split partitions its pending messages by key range across all replacement nodes.

The following behavior is specific to this crate:

- immutable content-addressed nodes and copy-on-write snapshots;
- opaque total-version ordering and LWW-register resolution;
- `BlockId` verification;
- batched network/storage waves; and
- flat search surfaces and occupancy-adaptive scalar search.

### Logical data model

The tree exposes latest-value reads and immutable root snapshots. It does **not** expose reads as of an
arbitrary timestamp. Therefore, whenever candidates for a key meet during a node merge, only their
greatest version needs to survive. An old snapshot retains its old winner because its old root remains
reachable; copying older co-located versions into a new leaf is not what preserves MVCC.

The write API changes from caller-stamped individual messages to a stamped batch:

```text
apply(root, stamp, mutations_in_program_order) -> new_root

VersionStamp {
    order_key: [u8; VERSION_BYTES],
}
```

All mutations in one batch share `stamp`. Before tree insertion, repeated keys in the batch collapse
to the last mutation in program order. Each persisted leaf or buffer contains at most one entry per
key, not one entry per historical version. A stale child candidate may coexist with a newer buffered
candidate above it until flush; reads compare the bounded one-per-level candidates on the path.
An empty batch returns the input root without calling the store.

The tree does not need to understand physical time. It needs a fixed-width, canonical, totally ordered
version key. Winner order is lexicographic by `(order_key, operation_tiebreak)`. The exact
`operation_tiebreak` is `(0, inline_value_bytes)` for an inline upsert,
`(1, ValueObject_ID, logical_length)` for an out-of-line upsert, and `(2)` for a tombstone. Byte strings
are compared lexicographically as unsigned bytes and `logical_length` numerically. The inline
threshold is deterministic, so one accepted format has one representation. Thus a reused order key
is deterministic and delete-wins, without a probabilistic digest tie-break or a value-object fetch.
Batch duplicate collapse happens first, so an upsert after a tombstone in the same batch still follows
program order; delete-wins applies only when distinct persisted candidates reuse an order key.

The application is responsible for making causally later writes receive greater order keys. Reusing
an order key is a producer fault, but the canonical operation comparison makes the tree deterministic
even then. Version keys are ordering tokens, not authentication credentials; provenance belongs
outside this tree.

This total order makes merge and replay commutative at the key level: the greater winner tuple wins,
and byte-identical replay is idempotent. Arrival order never resolves a persisted tie.

“Last writer” means greatest winner tuple, not omniscient real-time order. The tree validates byte
representation and applies the total order; it cannot validate that a producer followed its clock or
commit protocol.

The crate may provide an HLC adapter for a format with `VERSION_BYTES >= 28`. It constructs the
significant prefix as big-endian `(wall_ms: u64, logical: u32, writer_id: [u8; 16])`; any remaining
suffix bytes have one scheme-defined canonical value. A tree configuration has a separate fixed
16-byte `version_domain`, so different producer protocols cannot be mixed accidentally.

Milliseconds are sufficient for this adapter because `wall_ms` is only the coarse physical
component. The logical counter orders local events and causally dependent events that share the same
physical component; the writer ID orders independent writers with equal HLC pairs. Nanosecond
precision would neither eliminate cross-writer collisions nor replace those fields. This statement
depends on a correct adapter: it persists its last emitted HLC across restart, merges every observed
remote HLC before issuing a causally dependent write, never lets clock rollback decrease its state,
and never wraps the logical counter. On `u32` exhaustion it waits for a greater physical component or
returns an error. Writer IDs must be stable and unique for the producer domain. Other applications
may use Lamport counters or another canonical total order that fits `VERSION_BYTES`, without changing
tree algorithms; a different producer protocol uses a different `version_domain`.

Physical-clock precision affects only how closely concurrent writes are ranked by observed wall
time. It does not establish causality, and finer precision does not repair clock skew. Tree
correctness depends on the total-order and HLC transition rules above, not on millisecond accuracy.

Removing stored history also removes the retention floor and the unsplittable hot-key-history case.
An as-of query API would require a separate history index or version-chain design; it must not be
smuggled into ordinary buffered-tree nodes.

## Motivation

### The current structure is the ε≈0 endpoint

`split_leaf` is the only constructor of internal nodes. It always creates one pivot and two
children, and a parent replaces a split child with the new two-child internal node instead of
absorbing the promoted pivot. Thus every internal node has fanout two. The existing `FANOUT = 64`
controls the leaf entry threshold and internal buffer entry threshold, but not fanout or physical
node size.

The update-buffering mechanism remains useful, but the structure is not a balanced B-tree and its
shape is highly sensitive to batch boundaries. A local probe on the current code with 20,000
ascending keys found:

- one 20,000-mutation apply produced one two-child root and two 10,000-entry leaves, despite the
  nominal 64-entry threshold; and
- 20,000 single-mutation applies produced only fanout-2 internal nodes and leaf depths from 1 through
  307.

The probe used 9-byte keys, 1-byte values, and strictly increasing logical clocks. It was removed
after measurement. The harness must reproduce both cases and add randomized order/commit-width
combinations before implementation begins; a single favorable fixture would hide the degeneration.

### The current representation has no vector search surface

`Node` contains `Vec<Entry>`, and each entry contains separately allocated `Vec<u8>` keys and
values. Postcard decoding is a serial varint parse followed by per-field allocation. `resolve()`
linear-scans entries; a COW rewrite clones the entry vector and `merge_entries` sorts it again.

SIMD cannot repair that representation after decoding. A vector path needs fixed-offset head
arrays, validated bounds, and canonical padding. Full keys and values can remain variable-sized in
an in-block blob.

### Existing APIs already expose useful batches

`tree_get_many`, `scan_prefix_many`, `fetch_pruned_multi`, and `Staged`/`flush` already batch work
to reduce dependent storage round trips. The same boundaries can batch decode, search, and hashing.
The traits may change to make object-size limits and verified retrieval explicit.

## Design

### 1. Capacity and value model

A **regular node** serializes to exactly `NODE_BYTES`. `NODE_BYTES` is a format constant, not a
property inferred from its current contents. Every fixed section and unused byte is included in the
content hash.

An internal node has separately budgeted pivot/child and message regions. V3 stores the pivot surface's
common prefix once and stores suffixes in its offset region; builders use the shortest separator that
still routes strictly between adjacent child ranges. It is full when adding a
message would exceed either `MESSAGE_SLOTS` or the encoded buffer-byte region. A leaf is full when
adding an entry would exceed either `LEAF_SLOTS` or its blob region. Fullness is never based only on
the number of variable-sized values.

The configured buffer trigger must satisfy both of these inequalities, where the descriptor size is
included in `min_accounted_message_bytes`:

```text
BUFFER_BYTES >= F_MAX * MIN_FLUSH_BYTES
MESSAGE_SLOTS * min_accounted_message_bytes >= F_MAX * MIN_FLUSH_BYTES
```

Thus either ordinary fullness trigger guarantees that some child owns at least
`MIN_FLUSH_BYTES`. If a chosen format cannot satisfy both inequalities, it is rejected during format
selection rather than relying on undersized flushes at runtime.

This RFC deliberately replaces the effectively unbounded `Vec<u8>` contract with explicit limits.
`MAX_KEY_BYTES`, `MAX_VALUE_BYTES`, and `MAX_OBJECT_BYTES` are format/configuration constants. A write
that exceeds them returns a typed capacity error before storage and publishes no root.

`Format::check_key`, `check_value`, and `check_mutation` expose those same allocation-free predicates
to callers assembling work incrementally. `apply` calls the same methods during normalization; the boundary test checks
accepted and rejected key/value sizes at `limit` and `limit + 1`, preventing preflight drift.

The V2 format reserved `F_MAX - 1` maximum-size pivots, which coupled the public key limit directly to
fanout. V3 instead reserves an 8 KiB compressed-pivot region, proves that at least two maximum-size
pivots fit, and partitions a transient parent by both child lanes and actual compressed pivot bytes.
Ordinary surfaces retain fanout up to `F_MAX`; adversarial unrelated long separators reduce only that
node's realized fanout and never create an overflow object.

Leaves have two canonical column layouts, both exactly `NODE_BYTES`: the regular 640-lane leaf for
ordinary keys and a compact 16-lane leaf with a much larger blob for long keys. Only the compact layout
stores its common key prefix once. The encoder selects regular whenever it fits and compact otherwise,
so one logical leaf has one representation. This avoids the roughly 2x amplification that a 640-lane
descriptor reservation imposed on incompressible 4 KiB keys without slowing ordinary decode. Every
legal key plus its maximum inline/reference span fits at least one leaf layout.

Construction rejects configurations unless `NODE_BYTES ≤ MAX_OBJECT_BYTES`, a maximum-size value plus
its envelope fits `MAX_OBJECT_BYTES < 2^32`, every length and slot count fits its `u32` field,
`VERSION_BYTES` is nonzero, `3 <= F_MAX <= u16::MAX`, and the pivot/leaf/buffer inequalities in this
section all hold. `MAX_TREE_LEVEL` must be representable as `u16`; an insertion that would create a
taller root returns a typed capacity error and publishes no root. These are format proofs checked once
or explicit capacity branches, not accidental failures in the write path.

Values at or below `INLINE_VALUE_BYTES` are stored in the node. Larger legal values are stored in a
separate canonical `ValueObject` whose `BlockId` and logical length are stored in the entry. A value
object is at most `MAX_OBJECT_BYTES` and is written in the same durable batch as the nodes that first
reference it. Point lookup therefore costs one additional independent fetch only for an out-of-line
winner. This separation prevents a single value from dictating node size or split shape.

The node header magic and the value envelope begin with distinct object-type domain tags, making the
two canonical byte languages disjoint before hashing. `NODE_BYTES` includes the complete node header
and padding; the domain tag is not an extra outer wrapper. The ordinary collision-resistance
assumption for `BlockId` still applies. Traversal/GC APIs return both child-node references and value
references; callers cannot infer reachability by parsing only internal-node child IDs. A future
chunked-value manifest can raise `MAX_VALUE_BYTES`, but is not part of this RFC.

A mutation whose accounted message cannot fit an empty regular internal buffer is routed directly
toward its leaf. This sacrifices buffering for that mutation but does not create a second node shape.

### 2. Canonical node format

All structural integers and lengths in the node codec are little-endian. Opaque byte strings such as
`BlockId`s and order keys retain their scheme-defined byte order; the HLC adapter's big-endian prefix
is therefore not re-encoded by the node codec. Section **offsets within the serialized byte string**
are multiples of 64 where useful to SIMD loads. This does not assert that a `Bytes` allocation is
64-byte aligned: `NodeStore` may return a sliced or otherwise unaligned `Bytes` value.

The following is the scalar baseline candidate. Bundled-prefix or narrower-head experiments may
change the head directory before constant selection; any accepted alternative receives a distinct
exact 128-bit `schema_id` rather than being decoded heuristically.

```text
Header (64 bytes)
  magic              [u8; 8]
  schema_id          [u8; 16] BLAKE3-128 parameter digest, not a compatibility version
  kind               u8       regular leaf | internal | compact long-key leaf
  flags              u8       must be zero in this format
  tree_level         u16      zero for leaves; parent = child + 1
  pivot_count        u16
  child_count        u16
  entry_count        u32
  entry_head_skip    u32
  pivot_head_skip    u32
  reserved           [u8; 4]  all zero
  version_domain     [u8; 16] must match tree configuration

Regular search surfaces (fixed capacity)
  entry_heads        [u64; LEAF_SLOTS or MESSAGE_SLOTS]
  pivot_heads        [u64; F_MAX - 1]                 internal only
  child_ids          [[u8; 32]; F_MAX]                internal only

Offset columns
  pivot_offsets      [u32; F_MAX]                     N+1 live offsets
  entry_offsets      [u32; 2 * ENTRY_SLOTS + 1]      key/value spans alternate

Cold entry columns
  order_key          [[u8; VERSION_BYTES]; ENTRY_SLOTS]
  op                 [u8; ENTRY_SLOTS]
  external_value_len [u32; ENTRY_SLOTS]   zero unless op is out-of-line upsert

Blob regions
  V3 pivots: common prefix once, then suffixes in pivot order
  regular/internal entries: alternating full key/value spans in entry order
  compact-leaf entries: common key prefix once, then alternating key suffix/value spans
```

In the layout above, `ENTRY_SLOTS` means `LEAF_SLOTS` for a leaf and `MESSAGE_SLOTS` for an internal
buffer.

Lengths are differences between consecutive offsets. For entry `i`, offsets `2i..2i+2` delimit its
key and value representation. A tombstone has an empty value span; an inline upsert's span is the
value bytes; an out-of-line upsert's span is exactly one 32-byte `ValueObject` ID. The explicit `op`
distinguishes all three. Monotonic N+1 offsets remove redundant `{off, len}` encodings and reduce
validation to a bounded monotonicity pass. `u32` is ample because every object is smaller than
`MAX_OBJECT_BYTES < 2^32`; an offset greater than its blob-region length is rejected, while an offset
equal to the region length is the valid one-past-end boundary. V3 pivot and compact-leaf key offsets
address suffix bytes after the stored common prefix; other entry offsets address full keys. The live
final offset plus its applicable prefix must equal the used blob length.

Key length is derived from the two offsets only after a head equality. Storing it beside the head
would save those cold loads but create a redundant canonical field; the benchmark must demonstrate a
need before the format accepts that extra verifier invariant.

Order keys are cold, opaque bytes. The tree compares them lexicographically and does not reinterpret
their clock fields. Keeping the baseline fixed-width avoids multiple encodings and makes arbitrary
producer schemes safe. Prefix/dictionary compression of order keys is a benchmark candidate only if
it defines one deterministic minimal representation and repays its verifier complexity.

#### Canonical encoding

Content addressing requires one byte representation for one logical node. Builders must:

- sort entries by key ascending;
- contain exactly zero or one entry for each key;
- set `child_count = pivot_count + 1` for every internal node;
- choose every data-dependent encoding width by one deterministic minimal-width rule;
- write offset columns and blob fields in logical order with no gaps;
- zero every reserved byte and unused column/child slot;
- zero unused head lanes; and
- emit exactly `NODE_BYTES` for every node. A `ValueObject` uses its separately specified unique
  minimal envelope around the value bytes.

Zero is padding, not a sentinel. Arbitrary key heads occupy the entire `u64` domain, so no `u64`
value can represent an out-of-band lane. Search receives the live count and masks unused lanes before
calculating an index.

#### Validated zero-copy decode

Canonicality is verified on decode, not merely produced on encode. Decode is a validated view over
shared `Bytes`, not a Rust struct cast. Accessors use `from_le_bytes` over checked slices. The safe view
constructor checks before
any descriptor is exposed:

- magic, exact `schema_id`, kind, zero flags, and canonical total length;
- count/capacity relationships and `child_count = pivot_count + 1`;
- `tree_level == 0` for leaves and `0 < tree_level <= MAX_TREE_LEVEL` for internal nodes;
- a `version_domain` matching the configured producer protocol and a correctly sized order-key
  column;
- monotonic N+1 offsets whose final value is exactly the blob length;
- checked arithmetic before every offset/index calculation;
- canonical, gap-free blob ordering;
- zero reserved bytes, unused slots, and padding;
- strictly sorted pivots and entry keys;
- legal `op`/value-span combinations, including exactly 32 bytes for an out-of-line value ID;
- zero `external_value_len` for tombstones/inline values and a legal nonzero length for out-of-line
  values;
- stored heads and head-skip values against the referenced full keys; and
- local pivot/child cardinality invariants.

Malformed bytes return `TreeError::Decode`; they never reach search. Byte fuzzing and mutation tests
supplement these runtime checks but do not replace them. Production search contains no unsafe code or
architecture-specific intrinsics.

Local decode cannot prove an edge invariant without its child. Traversal therefore rejects an
internal-node edge unless the fetched child's `tree_level` is exactly one less than the parent's.
Every public walk accepts a `WorkBudget { max_objects, max_fetched_bytes }` or uses configured
defaults. Before each wave it checked-adds object visits and passes the remaining byte budget to the
store. A batched store read must not return more bytes than that aggregate allowance. The tree returns
`ResourceLimit` without a partial logical result if either bound would be exceeded. Cache hits do not
consume fetched-byte budget but do consume object-visit budget, so a malicious DAG cannot turn sharing
into unbounded CPU work. A valid hash authenticates bytes, not a promise that an untrusted writer built
a cheap tree.

The decoded-node cache stores `Arc<NodeView>` retaining the original `Bytes`. Ordinary keys remain
zero-copy slices. Compact leaves and a pivot surface with a nonzero stored prefix reconstruct full keys;
their heap bytes are included in the moka weight along with the byte string and fixed view metadata.

Retrieval is bounded before decode. `NodeStore` must accept the caller's `MAX_OBJECT_BYTES` and must
not allocate or return a larger object. A batched read returns exactly one result per requested ID,
in input order; the tree rejects a cardinality mismatch before associating bytes with IDs. The tree
also checks every returned `Bytes::len()` against its configured limit, hashes the complete returned
bytes, compares the requested `BlockId`, and only then reads the header. No unverified header field
may size a read, allocation, loop, or recursive walk.

```text
get(id, hint, max_object_bytes) -> Result<Bytes>
get_many(ids, hint, max_object_bytes, max_total_bytes) -> Vec<Result<Bytes>>
```

The write contract also changes. The tree computes each `BlockId` locally and submits addressed
objects rather than asking the store to assign IDs:

```text
put_batch([(BlockId, Bytes), ...], class) -> Result<()>
```

The store must make every supplied object durable before success and must reject an ID/bytes mismatch.
It may leave a prefix durable on failure because unreferenced content-addressed objects are harmless;
the tree returns no new root unless the entire batch succeeds. The batch includes out-of-line values
before or alongside every node that references them. Duplicate IDs are idempotent only when their
bytes are identical.

The breaking API also separates failure classes: `Capacity` for an invalid configuration or write
above a declared key/value/depth limit, `ResourceLimit` for an operation work budget, `HashMismatch`
for bytes that do not match the requested ID, `Decode` for malformed canonical bytes,
`VersionDomainMismatch` for a valid node from another producer domain, and `Store` for transport or
durability failure. Callers must not parse strings to distinguish them.

#### Stored-format policy and migration

Postcard remains unsupported: it predates the canonical schema registry and is rejected. Starting with
selected V2, every released selected schema has an explicit `Format::known_schema` registry entry and
golden schema id. Unknown ids are rejected; no decoder guesses a layout. A registered schema remains
readable and migratable within this major line. Removing one requires a separately announced major
change and is safe only after callers have rewritten or intentionally discarded every root using it.

`BeTree::open_known(store, root)` reads the authenticated root envelope, selects a registered decoder,
then performs the normal hash and complete structural validation. `target.migrate_from(source, root,
batch_rows)` is the offline rewrite mechanism. It streams resolved winners in bounded batches into a
fresh target root, preserving every key, exact order key, tombstone, and logical value. External values
are re-enveloped under the target schema, so their ids intentionally change; replay in the target format
then uses the target's canonical tiebreak. The old root and its objects are never mutated or deleted.
The returned report identifies both schemas and roots and counts rows/batches. Root replacement inside
an application's persisted root references is application-level work because only that application can
decide which immutable history to retain.

Every future byte-layout change must add its old selected schema to the registry, a golden id, and a
migration test before changing `Format::selected`. `BlockId`s still change whenever canonical bytes
change; the mechanism makes that transition explicit rather than pretending ids are stable.

### 3. Correct head-first search

For a byte string and a skip `s`, its head is the next eight bytes beginning at `s`, padded on the
right with zero, and interpreted as a big-endian `u64`. If two heads differ, their integer order is
their lexicographic order. Equal heads are only a candidate range: embedded zero bytes, short keys,
and common prefixes require full-key comparison.

`entry_head_skip` and `pivot_head_skip` are separate because a buffer and its pivots are independent
search surfaces. Each is the common-prefix length of the live keys in that surface, computed from
the first and last sorted keys. Empty and singleton surfaces use zero.

A probe follows this scalar reference algorithm:

1. Compare the probe with the skipped common prefix before looking at suffix heads. A mismatch
   before `skip` places the probe before or after the entire surface. A probe that ends inside a
   matching common prefix sorts before every longer key in the surface.
2. Compute the probe head and find the live range whose heads equal it. Unused lanes are masked;
   they are never compared as keys.
3. Search that equal-head range with the full byte comparator. Entry keys are unique within a node;
   for pivots, return the count of full pivot keys `<= probe`.

This algorithm handles long shared prefixes without changing routing order. LLVM may vectorize step 2;
steps 1 and 3 remain the correctness backstop. Property tests compare every index and match result with
an independent full-key implementation, including empty keys, embedded zeroes,
all-`0xff` heads, probes outside the common prefix, and keys shorter than `skip`.

A single surface-wide skip is the selected representation. The format study also tested aligned bundles
of 8 and 16 entries, each with its own recomputed prefix length, while preserving O(1) random access.
Those alternatives lost and were removed after their measurements were recorded below.

Search implementations are evaluated in this order:

1. ordinary and branchless binary search over the live head range;
2. a fixed-capacity count reduction (`head < probe`) that LLVM may autovectorize;
3. bundled prefix skips; and
4. hand-written AVX2/NEON kernels.

The first implementation with the best reproducible latency/throughput result wins. There is no
requirement that the accepted implementation contain explicit SIMD.

**Result (`benches/search.rs`).** No single kernel wins. The count reduction is vectorized but still
`O(n)`, so it wins below ~128 live keys and loses badly above (9x slower at 2048); a logarithmic
search wins above. The two surfaces in a node have systematically different sizes — a pivot surface
holds at most `F_MAX - 1 = 31` keys, an entry surface up to `LEAF_SLOTS = 640` — so the accepted
implementation dispatches on **live occupancy** at `ADAPTIVE_THRESHOLD = 128`, one integer test that
picks correctly for both surfaces. Bundled prefixes and explicit SIMD were both measured and both
rejected; see the selected-constants section for the numbers.

#### Routing summaries

SplinterDB-style quotient/xor summaries can prove that a message group does not contain a point key.
They are not in the baseline format. This tree already fetches the internal node to obtain its child
pointer and has one sorted buffer per node, so a summary can avoid buffer-key/blob comparisons but
cannot eliminate that node I/O. The harness records absent-key buffer probes; a later format may add
a canonical, verifier-recomputed no-false-negative summary if the saved comparisons exceed its
space, hash, and verification cost. Range seeks never depend on the summary.

### 4. Canonical merge, flush, and split propagation

#### Merge semantics

Batch ingestion first collapses repeated keys to their last program-order mutation and attaches the
batch's `VersionStamp`. It then sorts the resulting unique keys. Merging two sorted runs compares keys
once and retains the entry with the greater winner tuple. Equal stamps compare their exact
`operation_tiebreak`; byte-identical operations are idempotent. No persisted tie depends on queue
position, tree level, traversal order, or which replica performed the merge. The operation comparison
occurs only on the exceptional equal-stamp path.

#### Flush

An ordinary incoming message is first included in a transient builder. If that builder exceeds either
regular-buffer capacity, it is flushed before any node is serialized:

1. Route buffered entries by the current full pivots.
2. Account for the exact descriptor, key, and operation-representation bytes that the entry would
   occupy in a child buffer or leaf. An out-of-line value contributes its ID and length, not its
   separately stored payload.
3. Choose the child with the greatest pending encoded byte count; use the lowest child index as the
   deterministic tie-break. The configuration invariants above guarantee that the winner owns at
   least `MIN_FLUSH_BYTES`.
4. Remove that complete group from the parent and recursively merge it into the child.
5. Integrate any split returned by the child.
6. If integration makes the parent exceed `F_MAX`, partition the parent and its buffer immediately;
   do not apply the `c ≤ F_MAX` flush bound to a transient overfull builder.
7. Otherwise repeat while the parent buffer is over capacity.

Immediately-routed oversized messages do not participate in the flush lower bound. For ordinary
messages, the over-capacity transient builder has `M` accounted bytes and `c ≤ F_MAX` children, so a
victim owns at least `ceil(M/c) ≥ MIN_FLUSH_BYTES` by the pigeonhole principle and the format
inequalities. This is the byte-level batching guarantee the implementation records. The classical
`Θ(B^(1-ε))` message bound additionally requires the bounded-unit model and the parameter relationship
from the source-model section.

A `workdone`-weighted victim score and edge promotion for sorted inserts are useful PerconaFT
heuristics, but they are not canonical baseline state: persisting heuristic history would make node
identity depend on work scheduling. The benchmark may evaluate deterministic, rebuild-derived
variants after the byte-only rule is established.

#### Multi-replacement propagation

A large flushed batch can split one child into more than two regular nodes. A binary `Split` result is
therefore insufficient. Recursive writes return an ordered replacement run:

```text
Rewrite {
    first_id,
    following: [(min_key, id), ...]
}
```

`following` is empty for a stable one-node rewrite. A leaf contains one entry per key. To split a
sorted run, take the longest non-empty prefix that fits both the slot and blob limits, then repeat.
This greedy rule is deterministic; a final sparse leaf is permitted and reported. The key/value
constraints guarantee that a single entry is representable, so there is no unsplittable-leaf case.

The parent removes the old child, splices `first_id` into its position, then inserts every
`(min_key, id)` from `following`. This widens the parent rather than nesting temporary two-child
internal nodes. The builder may hold more than `F_MAX` children transiently, but an overfull node is
never serialized.

For an overfull internal builder with `n` ordered children, let `q = ceil(n / F_MAX)`. Partition the
children into `q` contiguous groups whose sizes differ by at most one, assigning larger groups to the
left. Because `F_MAX >= 3` and `n > F_MAX`, every group has at least two children. The format's
worst-case pivot reservation guarantees that every group fits regardless of separator lengths. With
`pivots[i] = min(children[i + 1])`, each output receives its local children and intervening pivots;
the minimum key of every output after the first becomes the corresponding promoted pivot in the
returned replacement run. Buffered entries are partitioned by those promoted pivots, so each output
receives exactly the messages in its child-key range, and each output buffer is independently flushed
if necessary. These rules define the split bytes independently of task scheduling.

This partition is required by the Bε path invariant: every message for key `k` must remain on `k`'s
root-to-leaf path. If a root rewrite produces more than one node, new internal levels are built from
the replacement run until one root remains. A sufficiently large commit may therefore grow the tree
by more than one level without ever encoding an over-capacity node.

Tombstones do not immediately remove keys, so merge/steal on delete-side underflow remains out of
scope. A later garbage-collection policy may drop stable tombstone winners and then must define
underflow repair explicitly.

Leaves do not contain sibling pointers. Under COW, rewriting one leaf would otherwise require
rewriting its neighbor and potentially cascading through unrelated paths. Ordered scans use the
parent path/cursor instead.

#### Canonical bytes are not a confluent tree

Canonical encoding guarantees one byte string for one logical **node**. Median/size-driven splits
and deferred underflow do not guarantee one tree shape for one resolved key/value set. Different
operation orders can therefore produce different roots for observably equal maps. This is expected;
tests must distinguish operation-order determinism from structural confluence.

Content-defined split predicates can improve history independence, but they introduce variable
occupancy, extra COW rewrite amplification, and adversarial key-mining risk unless keyed. They also
work against fixed search surfaces. This RFC declines them. If confluence becomes a requirement, it
needs a separate design covering keyed boundary selection and delete-side collapse together; adding
only deterministic splits is insufficient.

### 5. Wave-synchronous execution

The selected search has an independent safe full-key reference. Architecture-specific paths were
considered only after the scalar format and verifier landed and were rejected when measurement showed no
reproducible crossover.

#### Descent: grouped multi-key probes

For a loaded BFS wave, group in-flight probes by node. Reuse each validated head surface for all
probes assigned to that node. `tree_get_many` and the multi-prefix APIs are the intended fast path;
single-key access is a one-element batch.

The expected gain is fewer repeated loads and comparisons, not an assumption that an entire head
surface stays resident in registers. Benchmarks determine head width, capacity, and whether explicit
vector code exists at all.

#### Ascent: canonical encode and hash

`Staged` forms a dependency DAG: a parent can be encoded only after the IDs of its newly staged
children are known. Encode and hash nodes in topological layers, children before parents.

The pinned `blake3` 1.8.6 one-shot APIs are single-threaded; its internal multi-input kernels are not
a stable public batching API. Its Rayon API is documented as commonly slower below 128 KiB on
x86-64. Therefore node size is chosen for tree/storage behavior, not inflated merely to cross a hash
threshold. The implementation uses one ordinary `blake3::hash` per node.

No unstable BLAKE3 internals enter the implementation unless a later RFC supplies measurements,
an encapsulation boundary, and a maintenance plan.

**Result (development experiment and `benches/workloads.rs`).** The isolated crossover depends on object
size: at width 4 task parallelism loses for 16 KiB nodes but wins for 64 KiB and 256 KiB; width 16 is the
conservative crossover across every measured size. That isolated result does not survive as a clear
system-level win. With four executor workers and 4,096-mutation applies—wide enough to exercise the
parallel ascent—task parallelism is 8% slower at one task, within 1% at eight tasks, and 2% slower at
32 tasks. The task-parallel implementation and its configuration surface were therefore removed;
hashing is unconditionally sequential until a future RFC demonstrates a system-level win.

The same benchmark prices the halves of a read: hashing a 64 KiB node costs ~27.7 µs while decoding and
fully validating it costs ~11.3 µs. Verification, not structural validation, is what `VerifyPolicy`
trades away; it is 3.7x on a cold wave that fetches an entire tree.

#### Verify on read

On a cache miss, `load` and `load_wave` hash each raw byte string and compare it with the requested
`BlockId` before decoding. Hashing arbitrary bytes is safe; decode validation still establishes
well-formedness. The default policy is `Always`; `Never` relies on the existing `NodeStore`
content-addressing contract. A verified immutable `NodeView` or value object need not be rehashed on
cache hits. Hash mismatch and structurally malformed canonical bytes are distinct typed errors because
they imply different failures in the store/writer. A fetched value envelope must also agree with the
authenticated `external_value_len` in its referencing node before its payload is returned.

### 6. Adjacent scalar improvements

These changes use the same sorted, allocation-free representation but remain separately measurable:

- `resolve()` becomes the head-first lower-bound/exact-match algorithm.
- `merge_entries` becomes the run-aware two-pointer merge specified above.
- `diff` becomes a two-cursor ordered walk. Each cursor exposes its current key and path of
  `BlockId`s **plus the sorted ancestor-buffer runs that overlay that key range**. Equal subtree IDs
  skip the subtree's own entries only after candidate keys from unequal overlays in that range have
  been emitted. Unequal shapes re-synchronize by key instead of collecting an entire subtree.
  Candidate winners are resolved in batches. This removes the current pivot-alignment fallback while
  preserving exact results; copying a prolly-tree cursor without overlay state would be incorrect for
  a buffered tree.
- scans use a k-way merge of sorted leaf and buffer runs. The greatest winner tuple wins, so
  traversal-source priority has no semantic role.
- `MemStore::get_many` acquires its mutex once and clones all requested byte strings in input order.
  Spawning concurrent lookups against one mutex would add overhead and would not model remote-store
  concurrency.

## Semantic commitments

API signatures and stored bytes may change. The replacement must preserve these logical properties,
apart from the newly explicit size limits:

- `BlockId` is the BLAKE3 hash of the exact stored bytes.
- lexicographic `(order_key, operation_tiebreak)` LWW resolution, with last-program-order mutation
  during batch normalization only.
- COW publication occurs only after every reachable staged object is durable.
- old-root snapshot behavior; current roots retain only the winner per key at each buffered level.
- batched results preserve input order and duplicate-input behavior.
- `diff` returns exact sorted unique keys. Its cursor implementation is expected to scale with changed
  key regions plus bounded node-boundary work; the benchmark reports visited nodes and keys rather
  than asserting confluence or an unconditional mathematical O(divergence) bound.

## Benchmark and test harness

The harness lands before the codec or tree changes.

### Workloads

- point read and 256-key `tree_get_many`;
- successful and unsuccessful point reads with buffer hit/miss accounting;
- 64-prefix `scan_prefix_many`;
- one-mutation and 256-mutation `apply`;
- `diff` at fixed logical divergence;
- canonical encode plus hash by staged layer; and
- read waves with verification on and off.

Fixtures include uniform random keys, long shared prefixes, empty and embedded-zero keys, repeated
updates to one key, arbitrary opaque order keys, equal-HLC writes from different HLC-adapter writers,
reused order keys with different operations, variable values around the inline threshold and every
capacity boundary, and rejected keys/values immediately above their limits.

Shape fixtures cross ascending, descending, and seeded-random key order with commit widths 1, 16,
256, and all-at-once. They report minimum/maximum leaf depth as a correctness signal as well as
fanout, rather than averaging away an unbalanced spine.

The capacity matrix spans at least 4 KiB, 16 KiB, 64 KiB, 256 KiB, and 1 MiB regular nodes. It also
compares whole leaves with independently content-addressed leaf partitions: an in-object partition
cannot reduce fetch or verification bytes under this store model, so only a separately addressed
partition is a meaningful point-read experiment.

### Reported measurements

- latency and throughput;
- serialized bytes read, written, hashed, and flushed;
- realized depth and fanout distribution;
- buffer occupancy before flush and victim bytes per flush;
- minimum-flush eligibility and any attempted undersized flush;
- inline/out-of-line value count and bytes;
- equal-head-range percentiles and full-key comparisons per probe;
- visited nodes/keys and equal-ID skips during `diff`;
- absent-key buffer probes, to price a future routing summary;
- decoded-cache weight and hit rate; and
- ordinary binary, branchless binary, autovectorized reduction, bundled-prefix, and explicit-SIMD
  crossover by wave width and node occupancy.

Criterion runs against `MemStore` measure compute. A separate counting/latency store records calls,
bytes, and dependent waves; `MemStore` results are not presented as storage-system I/O throughput.

### Correctness oracles

- Existing tests for point/range reads, tombstones, snapshots, and content addressing retain their
  logical results. Tests that encoded historical-version retention or arrival-order tie resolution
  are replaced by the explicit semantics in this RFC.
- A simple `BTreeMap<Vec<u8>, Winner>` model checks randomized puts, reads, scans, tombstones, batch
  duplicate collapse, replay, reused-stamp deterministic resolution, and snapshots. Its winner
  comparator is implemented independently from the tree.
- HLC-adapter tests cover same-millisecond events, remote merge, clock rollback, restart recovery,
  distinct writers, and logical-counter exhaustion.
- Every generated tree checks sortedness, pivot/child counts, child range ownership, buffer path
  ownership, regular-node byte limits, parent/child `tree_level`, and canonical re-encoding.
- Decoding arbitrary and mutated bytes either returns a validated view or `TreeError::Decode`, never
  panics or reaches unchecked memory.
- No unverified header value controls an allocation, iteration count, read size, or recursion depth.
- The selected search agrees with an independent full-key oracle on indexes and matches; canonical
  encoding and hashing reproduce every pinned byte string and content id.
- Independently built trees receiving identical ordered operations produce identical roots. A
  separate test demonstrates that equal resolved maps built in different orders are allowed to have
  different roots, preventing accidental confluence claims.

## Sequencing

1. **Harness and invariant checker.** Reproduce the binary-fanout defect and establish baselines.
2. **Logical and storage contracts.** Replace caller-stamped messages with normalized stamped batches,
   add opaque-version/operation total ordering, remove retention-floor/history storage, make
   key/value/object/depth limits and caller-bounded retrieval explicit, and add addressed batch
   writes plus typed errors.
3. **Canonical scalar codec.** Add N+1-offset node/value encoders, validated views, round-trip tests,
   corruption tests, and deterministic-byte tests. Keep existing tree shape temporarily.
4. **Multiway replacement and byte-accounted flushing.** Add replacement runs, parent splicing,
   internal buffer partition, `MIN_FLUSH_BYTES`, direct routing, and shape/property tests.
5. **Scalar search and merge improvements.** Benchmark binary, branchless, count-reduction, and
   bundled-prefix variants before adding architecture-specific code.
6. **Cursor diff and k-way scans.** Remove the structural-alignment fallback and measure visited work.
7. **Wave hashing and verify-on-read.** Establish the policy and task-parallel crossover.
8. **Optional explicit SIMD.** Add dispatch only if it beats every scalar/autovectorized baseline.
9. **Constant selection.** Select `NODE_BYTES`, `F_MAX`, byte/slot regions, `VERSION_BYTES`, key/value/
   object/depth limits, inline threshold, minimum flush size, head width, and prefix grouping from the
   full matrix; record workload assumptions and effective-ε diagnostics.

The selected `schema_id` describes one encoding under the BLAKE3 collision-resistance assumption. It
does not make `BlockId`s stable across format changes. Released schemas instead remain explicit
registry entries that can be opened and migrated under the policy above; adding or removing that
support is a deliberate compatibility decision, never an inferred layout guess.

### Status

All nine steps are complete, in order.

| step | landed as | note |
| --- | --- | --- |
| 1. Harness and invariant checker | `tests/support`, `tests/shape.rs` | The pre-RFC defect was reproduced first on the old code — fanout 2 everywhere, leaf depths 1..307 from 20 000 single applies, and one two-child root over two 10 000-entry leaves from one big apply — exactly as this RFC recorded. That probe was ephemeral; `tests/shape.rs` is its permanent replacement and now asserts the repaired shape. |
| 2. Logical and storage contracts | `src/lib.rs`, `src/store.rs`, `src/hlc.rs` | Postcard, the HLC-facing API, the retention floor, and historical-version storage are gone, not deprecated. |
| 3. Canonical scalar codec | `src/format.rs`, `src/codec.rs`, `src/value.rs` | 60 unit tests including exhaustive single-byte mutation. V3 adds compact leaves and prefix-compressed pivots. |
| 4. Multiway replacement and byte-accounted flushing | `src/tree.rs` | `Rewrite` runs, parent splicing, balanced partition with buffer partition at every promoted pivot, derived `MIN_FLUSH_BYTES`, direct routing, multi-level root growth. |
| 5. Scalar search and merge improvements | `src/search.rs`, `benches/search.rs` | Alternatives were benchmarked, then deleted; production retains only the occupancy-adaptive choice. |
| 6. Cursor diff and k-way scans | `src/tree.rs` (`Cursor`), `tests/diff.rs` | No structural-alignment fallback; overlay state carried per frame. |
| 7. Layer hashing and verify-on-read | `src/tree.rs` | Contended full-tree measurement rejected task-parallel hashing; production has one sequential path. |
| 8. Explicit SIMD experiment | development history | Added, measured, rejected, and deleted: it lost. |
| 9. Constant selection | this document | See [Selected constants and measured decisions](#selected-constants-and-measured-decisions). |

Test inventory: **144 tests across 11 targets, including 1 doctest**: 143 pass in the ordinary gate and
the 100k x 4 KiB qualification is explicitly ignored there and run separately in release. Counts are
53 unit, 25 `tree`, 11 `read_path`, 10 `shape`, 8 `corruption`, 8 `diff`, 8 `model`, 7 `golden`,
7 `write_path`, 4 `search_equivalence`, 2 `migration`, and 1 doctest. Verified with debug and release
tests, `cargo clippy --all-targets`, rustdoc, MSRV, package, and Miri gates. The debug runs matter
separately: the flush-floor and buffer-sortedness `debug_assert`s only fire there.

**Fuzz soak.** `BE_TREE_FUZZ=60 cargo test --release --test corruption` scales every sweep 60x:
24 000 single-bit flips in reachable nodes, 36 000 multi-byte mutations, and 120 000 arbitrary or
plausible-but-random byte strings. Zero panics, zero out-of-bounds accesses, zero unsafe acceptances.

The measurement worth reporting is the **structural** rejection rate with verification *off*, counted
against a full scan (which visits every reachable node — a point read only fetches its own key paths, so
counting against one would measure coverage rather than the verifier): **94.7% of mutations rejected by
the structural verifier alone.** The 5.3% that survive land in live value-payload or order-key bytes,
which are opaque data no invariant can constrain. Those change the *answer*, which is precisely what
`VerifyPolicy::Always` catches — and is why it is the default.

**Miri:** the entire crate has a reproducible, unfiltered single-command gate:

```
MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test
    => 75 passed, 69 ignored, 0 filtered out across 11 targets
```

Every `cfg_attr(miri, ignore = ...)` gives its native-scale reason: large randomized/corpus matrices,
maximum-size objects, concurrency scheduling, exhaustive corruption/search sweeps, and other fixtures
whose setup is dominated by interpreted BLAKE3. The remaining 81 tests include public tree, migration,
value, corruption, and streaming-adjacent APIs. Native debug/release runs all annotations; the command
above uses no name or target filters. Under Miri only, Moka is replaced by a mutex-backed semantic cache
because current Miri rejects Moka's crossbeam-epoch implementation; native builds always use Moka.

## Selected constants and measured decisions

All numbers below are medians on one aarch64 (Apple Silicon) host, `--release`, `MemStore` unless a
`CountingStore` column says otherwise. `MemStore` timings are **compute**, not storage throughput, and
no Bε asymptotic bound is claimed from them. Reproduce with `cargo bench`.

### The selected format

| constant | value | why this value |
| --- | --- | --- |
| `NODE_BYTES` | 65536 | Depth 2 at 100 k keys (3 waves) where 16 KiB needs depth 3–4; 256 KiB and 1 MiB buy no further depth at this corpus but double the bytes per point read. |
| `F_MAX` | 32 | Highest fanout that keeps `MIN_FLUSH_BYTES` in the high hundreds. In the refreshed 4 KiB-key matrix, `F_MAX = 64` improves ordinary amplification 1.45 → 1.43 but collapses the guaranteed flush floor 616 → 162 bytes. The floor bounds COW write amplification, so 32 wins. |
| regular / compact leaf slots; message slots | 640 / 16; 640 | Ordinary keys keep the high-occupancy layout. A compact long-key leaf removes the 640-descriptor tax while remaining exactly 64 KiB. |
| `VERSION_BYTES` | 28 | Exactly `u64 + u32 + 16`, so the HLC adapter fills it with no suffix scheme. Every extra byte costs one byte per slot in the cold order-key column. |
| `MAX_KEY_BYTES` | 4096 | Supports names, composite identities, and other nontrivial byte keys without forcing application-level hashing. Prefix-compressed shortest pivots and byte-aware internal partitioning decouple it from `F_MAX`; compact leaves remove the ordinary descriptor tax. |
| `INLINE_VALUE_BYTES` | 512 | One maximum key plus one maximum inline value fits a compact leaf; ordinary keys retain the regular leaf. |
| `MAX_VALUE_BYTES` | 4 MiB − 32 | A maximum value plus its 32-byte envelope is exactly `MAX_OBJECT_BYTES`. |
| `MAX_OBJECT_BYTES` | 4 MiB | Well under 2^32, so every offset fits `u32`. |
| `MAX_TREE_LEVEL` | 32 | Bounds dependent read waves. At `F_MAX = 32` this is far beyond any reachable corpus. |
| `BUFFER_BYTES` | 55808 | Derived: `MESSAGE_SLOTS x 49 + 24448`; the 8 KiB pivot proof costs 256 bytes versus selected V2. |
| `MIN_FLUSH_BYTES` | 764 | **Derived, not configured** — `min(31360, 24448) / 32`. |
| head width | 8 bytes (`u64`) | Masstree's construction, order-preserving, and the equal-head range it produces is already ~1 (below). A narrower head would need a second stage for no measured gain. |
| prefix encoding | one surface-wide prefix for V3 pivots and compact leaves | The prefix is already authenticated by `head_skip`; ordinary leaf/buffer keys remain uncompressed and zero-copy. Multi-bundle directories remain rejected — see reversal 2. |
| search | occupancy-adaptive at 128 | See reversal 1. |
| hashing | sequential | A wide-apply concurrency workload found no stable system-level win for task parallelism; the alternative and its configuration were removed. |
| explicit SIMD | absent | Measured, rejected, and removed — see reversal 4. |

### Capacity matrix (100 000 keys, ~40 B records, uniform random)

`amp` is physical bytes per live logical byte; `eff_eps` is the diagnostic `ln(F_realized)/ln(B_fixture)`;
`rd_bytes` is bytes fetched per lookup amortized over a cold 256-key batch. This matrix scales slot
capacity mechanically with node size to isolate size/fanout; the bold row identifies the selected
**size/fanout family**, not the exact selected layout. V3 candidates select the largest proved key limit,
including compact leaves. The separate slot sweep chose 640 rather than 736 slots and derives
`MIN_FLUSH_BYTES = 764`; the exact selected tree is the 162-node shape reported below.

| node | f_max | slots | max_key | depth | fanout | nodes | amp | eff_eps | min_flush | rd_bytes | waves |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 4096 | 8 | 46 | 128 | 4 | 7.7 | 2500 | 1.60 | 0.532 | 48 | 8941 | 5 |
| 4096 | 16 | 46 | 32 | 3 | 15.6 | 2323 | 1.49 | 0.717 | 28 | 6455 | 4 |
| 16384 | 8 | 184 | 1024 | 4 | 7.0 | 635 | 1.63 | 0.372 | 592 | 22185 | 5 |
| 16384 | 16 | 184 | 1024 | 3 | 14.6 | 584 | 1.50 | 0.514 | 164 | 18934 | 4 |
| 16384 | 32 | 184 | 128 | 2 | 31.2 | 562 | 1.44 | 0.660 | 56 | 17532 | 3 |
| **65536** | **32** | **736** | **4096** | **2** | **23.5** | **142** | **1.45** | **0.478** | **616** | **36211** | **3** |
| 65536 | 64 | 736 | 4096 | 2 | 34.8 | 140 | 1.43 | 0.538 | 162 | 35701 | 3 |
| 262144 | 32 | 2945 | 4096 | 2 | 12.0 | 37 | 1.52 | 0.311 | 3372 | 37741 | 3 |
| 262144 | 64 | 2945 | 4096 | 1 | 34.0 | 35 | 1.43 | 0.441 | 1540 | 35701 | 2 |
| 1048576 | 16 | 11781 | 4096 | 1 | 9.0 | 10 | 1.64 | 0.234 | 28884 | 40801 | 2 |
| 1048576 | 128 | 11781 | 4096 | 1 | 9.0 | 10 | 1.64 | 0.234 | 3382 | 40801 | 2 |

**4 KiB nodes are not viable at a useful key limit**, even with V3: their fixed columns leave only a
128-byte key limit at `F_MAX = 8` or 32 bytes at `F_MAX = 16`, and `MIN_FLUSH_BYTES` collapses to 48/28.
At 64 KiB, 4 KiB keys coexist with `F_MAX = 32` and a useful flush floor.

The public 4 KiB-key qualification is deliberately less compressible than a name-prefix best case:
100,000 sorted 4096-byte keys contain an 8-byte ordinal followed by deterministic pseudorandom tails.
The selected format produces equal leaf depth **3**, realized fanout **7..32** (mean **31.72**), **6884**
nodes, and physical/logical amplification **1.094**. This passes the requested depth <= 3,
`F_MAX >= 16`, and amplification <= 1.6 without hashing or shortening key identity.

Separately addressed leaf partitions were compared by holding the corpus fixed and shrinking
`NODE_BYTES`, which is the only variant that can reduce fetched *or verified* bytes under this store
model. Bytes per single point read: 16 KiB -> 18905, 64 KiB -> 69569, 256 KiB -> 72594, 1 MiB -> 96792.
Smaller nodes reduce per-lookup bytes 3.7x, at the cost of a level of depth. Neither becomes the
default on compute timing alone: the choice is workload-dependent. The 16 KiB / `F_MAX = 16`
alternative supports 1 KiB keys, not the selected format's 4 KiB contract; a deployment needing both 4 KiB keys
and the current encoding therefore selects at least 64 KiB.

The target-store model is `waves × RTT + bytes / bandwidth`, with checked arithmetic in
`harness::StoreModel`. For keys within the 16 KiB format's 1 KiB limit, a cold point read is four 16 KiB
objects for that alternative versus three 64 KiB objects for the selected format. Therefore 64 KiB wins only when RTT exceeds the
transfer time of its extra 128 KiB: **1.250 ms at 100 MiB/s, 0.250 ms at 500 MiB/s, and 0.122 ms at
1 GiB/s**. `benches/capacity.rs` prints these crossovers from the measured depths. A deployment must
insert its own measured RTT/bandwidth; if it falls below the crossover, select 16 KiB instead of treating
64 KiB as universally optimal.

### In-node search

Equal-head-range and full-key-comparison percentiles, over 4096 half-hit/half-miss probes:

| key shape | n | eqhead p50 | p90 | p99 | cmps mean | cmps p99 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| uniform-random | 64 / 640 / 2048 | 1 | 1 | 1 | 1.50 | 2 |
| long-shared-prefix | 64 / 640 / 2048 | 0 | 1 | 1 | 1.00 | 2 |
| embedded-zeroes | 64 / 640 / 2048 | 1 | 1 | 1 | 1.50 | 2 |
| ascending | 64 / 640 / 2048 | 0 | 1 | 1 | 1.00 | 2 |

Step 3 costs one or two full-key comparisons at every size and shape measured, including the
long-shared-prefix set the research notes flagged as the real risk. The mandatory common-prefix check in
step 1 is what makes this hold: it removes the shared prefix *before* the head is taken, so the head is
already discriminating.

Kernel latency per probe (uniform-random; the other shapes have the same shape of curve):

| n | binary | branchless | count-reduction | adaptive | bundled-8 | bundled-16 |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 16 | 12.5 | 12.1 | **9.2** | 9.5 | 34.1 | 27.0 |
| 64 | 15.8 | 16.9 | **12.8** | 12.9 | 46.0 | 36.8 |
| 128 | 18.1 | 19.6 | 18.0 | 18.2 | 54.3 | 46.0 |
| 256 | **20.4** | 22.2 | 34.7 | 22.2 | 64.3 | 55.2 |
| 640 | 35.3 | **28.3** | 83.3 | 28.8 | 83.3 | 73.1 |
| 2048 | **29.2** | 31.8 | 267.4 | 31.7 | 95.1 | 87.0 |

`Adaptive` is within noise of the best kernel at every size except 2048 (8% behind branchy binary),
which the selected format cannot reach.

### The four reversals

1. **The count reduction is not the winner it was predicted to be.** `research/in-node-search.md` §6.2
   called it "the highest-value experiment in the RFC". It is genuinely vectorized and it does win —
   but only below ~128 live keys, because it is still linear. At the selected `LEAF_SLOTS = 640` it is
   2.9x *slower* than a branchless binary search. The accepted kernel is occupancy-adaptive rather than
   either extreme.
2. **Bundled prefixes are rejected, and for a different reason than expected.** They were proposed to
   bound a large equal-head range. There is no large equal-head range to bound (table above: p99 = 1),
   so they have nothing to fix, and they cost 2–4x in latency. This RFC's paragraph "A single
   surface-wide skip is the baseline, not a frozen choice" is resolved: the surface-wide skip stays, and
   no second `schema_id` is introduced.
3. **`MIN_FLUSH_BYTES` is derived, not configured.** The RFC presented it as a constant constrained by
   two inequalities. Making it an input means a configuration can *state* a floor it does not meet.
   `Format::new` instead computes it as `min(MESSAGE_SLOTS x desc_bytes, buffer_blob_cap) / F_MAX`,
   which is the largest value both inequalities admit, so the inequalities hold by construction and
   either fullness trigger provably clears the floor. The `min` with the blob capacity is stricter than
   the RFC's `BUFFER_BYTES` form, because the blob trigger alone can fire.
4. **Explicit SIMD is rejected on measurement.** The hand-written NEON count kernel is 1.2x–2.3x
   *slower* than the autovectorized scalar reduction at every size (16 → 1.23x, 128 → 2.02x, 640 →
   2.12x). Per-iteration horizontal adds serialize where LLVM keeps vector accumulators live. The
   experiment was removed after its results were recorded here; production contains no explicit SIMD
   path or `unsafe` search code.

### Three further code/RFC corrections

- **`schema_id` excludes `version_domain`.** Deriving the identifier from every parameter *including*
  the domain would make a foreign producer's node indistinguishable from wrong-format garbage,
  destroying the `VersionDomainMismatch` class this RFC requires. The domain does not change the layout,
  so it is checked separately and reported as its own error. Regression test:
  `format::tests::version_domain_does_not_change_the_schema_id`.
- **Replay idempotence is a key-level property, not a byte-level one.** "byte-identical replay is
  idempotent" holds for resolved values; it does not imply an unchanged root, because a replayed message
  re-enters the root buffer even when an identical copy already sits in a leaf, and every apply that
  changes tree bytes writes at least a root object. Both readings are now pinned:
  `byte_identical_replay_is_idempotent_at_the_key_level` and
  `replay_into_a_single_leaf_produces_no_new_bytes`.
- **`AccessHint` is now actually delivered.** The port documents "intent flows DOWN", and describes
  `MetadataOnly` as "interior nodes over leaves — the highest-ROI cache lever", but the tree sent
  `Random` for every read, so a store could not act on any of it. Each wave now carries the hint its
  level and access pattern imply: `MetadataOnly` for interior levels and for a GC mark walk, `Random`
  for the leaf level of a point read or a COW rewrite, and `SequentialPrefetch` for the leaf level of an
  ordered cursor walk. The tree knows which without an extra fetch, because a child's expected
  `tree_level` is already carried for the edge check. Regression test:
  `tree.rs::the_tree_tells_the_store_what_kind_of_read_each_wave_is`.
- **`scan_prefix_many` walks the union of the prefix ranges, not their span.** Walking the span between
  the lowest and highest prefix visits everything in the gaps, and testing every row against every
  prefix is quadratic. The cursor now prunes against a normalized disjoint range *set* (binary-searched,
  so pruning never scales with the prefix count) and buckets rows by one hash lookup per distinct prefix
  length. Measured: 64 scattered prefixes over 100 k keys, 23.2 ms → 1.23 ms.

### Storage, caching, and concurrency corrections

An independent review found that several real costs were absent from both the implementation and the
benchmarks. Every item below is fixed, with the test that pins it. **These are corrections to earlier
claims in this document, not new features.**

| defect | was | now | pinned by |
| --- | --- | --- | --- |
| Write benchmark mutated one fixed root; the first evolving replacement then let the store/cache grow across Criterion samples | 65 µs / 82 µs, then non-stationary 307 µs / 1013 µs | Every sample gets a fresh writable overlay over one immutable corpus and runs a fixed 32-commit chain: 78.4 µs / 210.7 µs per evolving commit | `benches/workloads.rs::writes`, `report_evolving_writes` |
| Staged nodes were never inserted into the decoded cache, so each commit refetched the root its predecessor wrote | 10 refetches per 10 commits | **0 objects read per commit** | `write_path.rs::an_evolving_commit_chain_does_not_refetch_what_it_just_wrote` |
| Cursor did one scalar `get` per visited node — scans and both sides of `diff` were serially I/O bound | 337 gets, 0 batched reads for a cold 4 000-key scan | 337 objects in **42 round trips** (26 batched) | `read_path.rs::a_cold_scan_batches_its_reads_instead_of_one_get_per_node` |
| Duplicate external-value ids were passed straight to `get_many`, values were never cached, and deduplication accidentally let the first reference's length bless later references | one shared value fetched once per reference; inconsistent duplicate lengths accepted | each id fetched once and cached, but **every reference** validates its authenticated logical length | `read_path.rs::a_shared_external_value_is_fetched_once_not_once_per_reference`, `every_external_reference_validates_its_own_length` |
| Staged objects were not deduplicated | 256 keys sharing one 513-byte value submitted 257 objects / 200 960 bytes | 255 stagings elided; the value is stored **exactly once** | `write_path.rs::one_shared_value_is_staged_once_not_once_per_key` |
| `value::encode` hashed the object, then staging hashed the same bytes again; shared 4 MiB `Bytes` were still enveloped and hashed 256 times before staged-id deduplication | duplicate hash and O(keys × value bytes) normalization | one envelope/hash per shared immutable slice; staging reuses the id and reachable payload slice | `write_path.rs::a_value_object_is_hashed_once_not_twice`, `one_shared_large_bytes_is_encoded_and_hashed_once` |
| Cache lookup/fill were separate and only single-id misses could coalesce | concurrent readers stampeded; overlapping multi-id waves could refetch every id; a cancelled owner could strand waiters | one cancellation-safe in-flight coordinator claims ids independently while owners retain one batched fetch: 32 concurrent 64-value reads fetch 65 total objects in **2 waves** | `read_path.rs::concurrent_cold_reads_of_one_root_do_not_stampede`, `concurrent_multi_value_waves_fetch_each_id_once`, `overlapping_multi_id_waves_share_their_fetches`, `cancelling_a_wave_owner_does_not_poison_later_reads` |
| External values were not warmed after a successful apply; replay/stale losers and transient nodes were still submitted | immediate read refetched; no-op replay submitted 2 objects / 69 664 bytes | durable publication warms node/value caches; only objects reachable from the new root are submitted; unchanged roots perform no store call | `write_path.rs::an_external_value_is_warm_immediately_after_write`, `losing_external_values_are_not_submitted`, `replaying_a_commit_writes_nothing_and_reads_nothing` |
| `schema_id` truncated BLAKE3 to 16 bits, leaving 32 768 values; a sweep found `400/525` colliding with `400/596`, two *different* layouts | 16-bit id | 128-bit parameter digest (`SCHEMA_ID_BYTES = 16`) in both node header and value envelope; collision resistance is the explicit assumption | `format::tests::no_two_distinct_layouts_share_a_schema_id` |
| `max_value_bytes + ENVELOPE_BYTES` was evaluated before `max_value_bytes` was bounded: debug panic, release **wrap** | unchecked | checked arithmetic throughout format derivation | `format::tests::extreme_configurations_are_rejected_not_wrapped` |
| The rewrite silently **dropped** `scan_prefix_roots`, `scan_range_roots`, and `scan_prefix_many_roots`, required for sharded datasets | three public methods gone, unremarked | restored over the cursor, merging by winner tuple so an overlapping key resolves the same regardless of root order | `model.rs::multi_root_scans_merge_disjoint_shards_and_arbitrate_overlaps` |
| Metrics performed several atomic RMWs on every probe with no way to turn them off | always on | recording is off by default; opt in with `BeTree::record_metrics()` and read immutable snapshots with `BeTree::metrics()` | — |
| Node cache hard-coded at 512 MiB, values not cached at all | not configurable | `CacheConfig` for both, defaults 64 MiB / 16 MiB, `CacheConfig::NONE` supported; disabling cache drops the 32-task mixed workload from 14.47k to 4.21k ops/s | `read_path.rs::a_disabled_cache_is_still_correct`, `benches/workloads.rs::concurrency` |
| Task-parallel hashing was made the default from a single-writer benchmark | default | **removed**; wide 4,096-mutation applies make it 8% slower at one task, within 1% at eight, and 2% slower at 32, so production exposes one sequential path | recorded concurrency experiment |
| `scan_range` and `diff` had only whole-result `Vec` APIs | O(result) retained memory and no backpressure | `ScanCursor::next`/`next_batch` and `DiffCursor::next`/`next_batch`; collectors remain convenience wrappers, and scan batches external values | `tree.rs::streaming_scan_and_diff_match_the_collecting_convenience_apis` |
| Node-size selection had no target-store model and concurrency had no workload | MemStore compute was easy to overgeneralize | checked RTT/bandwidth crossover model plus 1/8/32-task apply and mixed read/write benchmarks | `shape.rs::target_store_model_prices_waves_bytes_and_the_crossover`, `benches/workloads.rs::concurrency` |
| Released roots had no compatibility mechanism | a layout change made every historical root undecodable | explicit released-schema registry, authenticated `open_known`, and bounded offline `migrate_from`; selected V2 has a permanent golden id and migration fixture | `migration.rs::a_registered_historical_root_rewrites_every_winner_semantic`, `golden.rs::the_shipped_schema_ids_are_pinned` |
| `open_known` authenticated the root to select its schema, then fetched the same root again on first use | two store reads to open one root | the authenticated bytes are decoded once and inserted into the selected tree's cache; opening costs one object read | `migration.rs::a_registered_historical_root_rewrites_every_winner_semantic` |
| `MAX_KEY_BYTES = 256` was coupled to `(F_MAX - 1) x MAX_KEY_BYTES` pivot reservation | a valid 257-byte identity failed before a caller could preflight it | V3 shortest/prefix-compressed pivots, byte-aware internal grouping, and exact-size compact leaves; 100k incompressible 4 KiB keys reach depth 3, fanout 7..32, amplification 1.094 | `shape.rs::one_hundred_thousand_four_kib_keys_keep_depth_and_amplification` |
| Capacity was discoverable only during `apply` | a late key in a large draft wasted all draft-building work | public allocation-free `check_key`, `check_value`, and `check_mutation`, with `apply` using the same predicates | `tree.rs::mutation_preflight_and_apply_agree_at_every_size_boundary` |
| Streaming acceptance covered equality with collectors but not a corpus larger than cache; diff lacked batch pulling | boundedness was documented but not exercised at the cache boundary | prefix/range scan cursor, `DiffCursor::next_batch`, and a 5000-row scan through a one-node/one-value cache with batches of 13 | `tree.rs::streaming_a_corpus_larger_than_the_cache_never_materializes_the_result` |
| The documented Miri gate covered only `--lib` | integration APIs were outside the single command; Moka/crossbeam is rejected by current Miri's experimental alias model | expensive native matrices carry explicit `cfg_attr(miri, ignore)` reasons; `cfg(miri)` uses a semantic cache substitute, so unfiltered `cargo +nightly miri test` runs the remaining full APIs | exact command in the status section |
| Ordinary callers had to import a non-object-safe `Tree` trait, and the raw overlay cursor/rewrite types were public | duplicate and implementation APIs appeared beside the application API | common operations are inherent on `BeTree`; the duplicate trait is removed; only the backpressured `ScanCursor` and `DiffCursor` are public | README doctest and rustdoc with warnings denied |

The implementation now carries recurring invariants in small internal types instead of parallel local
state: `KeyRange` owns half-open range algebra, `InternalBuilder` keeps child/pivot/buffer state together,
`ObjectCache` hides the native/Miri cache backend, and an in-flight `Claim` owns cancellation-safe fetch
coordination. Encoding first derives one checked `EncodePlan`; decoding proceeds through named envelope,
header, blob, padding, and semantic validation stages. These are implementation boundaries, not new
persisted concepts: all seven golden byte strings and content ids remain unchanged. Rejected search and
hashing experiments, plus the reference model and shape harness, no longer compile into the library or
appear as public configuration; they remain only as recorded rationale and test support where useful.
The shape checker has its own half-open range implementation rather than reusing `KeyRange`, so a defect
in production range algebra cannot validate itself. The application API also keeps ownership machinery
private: `with_format` accepts a `Format` value, metrics are enabled on the tree and read as immutable
snapshots, and raw winner/budget/cache/store internals are not façade types. Because the HLC encoding
occupies exactly all 28 order-key bytes, its decoder returns the tuple directly rather than a meaningless
`Option` and exposes no nonexistent suffix policy.

A third correction found while testing the fixes: the batched read's **aggregate allowance must be the
operation's remaining byte budget**, not the sum of the objects' declared lengths. Capping the store at the
declared total is tempting — it is tighter — but it converts a *substituted* value object (a larger valid
object served for a smaller reference) into `ResourceLimit`, which tells an operator their budget was too
small when the truth is bit rot or a lying store. Substitution is caught by each reference's authenticated
length instead. `corruption.rs::a_substituted_value_object_is_rejected` and
`the_two_object_domains_cannot_be_confused` both regressed to `ResourceLimit` before this was fixed.

Two design notes on the fixes:

- **Prefetch must not defeat `diff`.** The cursor batches a bounded breadth-first lookahead
  (`DEFAULT_PREFETCH_WIDTH = 64` objects, ~4 MiB at 64 KiB nodes), but only for a frame that has not
  *skipped* a child. An equal-subtree skip exists precisely to avoid reading that subtree, so prefetching
  it would spend exactly what the skip saves. A scan never skips and always batches; an aligned `diff`
  skips before its first load and never does. `read_path.rs::diff_still_skips_rather_than_prefetching_what_it_will_not_read`
  pins that a localized diff still touches under a quarter of the corpus.
- **Coalescing preserves batching.** The in-flight coordinator claims each content id independently.
  One wave fetches every id it newly owns in one `get_many`; an overlapping wave waits only on ids
  already owned and batches its disjoint remainder. Cache insertion happens before a flight completes,
  so completion/removal cannot open a refetch race.

### Remaining deployment boundary

The review's implementation and evidence gaps are closed. One choice cannot be closed generically:
`NODE_BYTES` depends on the target store. The checked model now makes the crossover explicit, but this
repository has no measured production RTT/bandwidth. Before deployment, feed those measurements into
the model. A 16 KiB format is available below the crossover only when an application can cap keys at
1 KiB; applications that need the selected format's 4 KiB identity contract require 64 KiB as the
smallest validated choice in this matrix. This is an external acceptance input, not evidence that
64 KiB is universally ideal.

### Workload results (selected format, 100 000 keys, 24 B values)

| operation | uniform-random | long-shared-prefix |
| --- | ---: | ---: |
| point read, hit | 1.04 µs | 1.13 µs |
| point read, miss | 1.11 µs | 0.96 µs |
| `get_many`, 256 keys | 74.8 µs | 119.7 µs |
| `scan_prefix_many`, 64 discriminating prefixes | 1.35 ms | 2.16 ms |
| `scan_prefix_many`, 8 identical maximally broad prefixes | 64.7 µs | 23.41 ms |
| `scan_range`, 256-key window | 17.73 µs | 19.61 µs |

| operation | time |
| --- | ---: |
| `apply`, 1 mutation, **evolving chain** | 78.4 µs/commit (32-commit stationary sample) |
| `apply`, 256 mutations, **evolving chain** | 210.7 µs/commit (32-commit stationary sample) |
| `apply`, 1 mutation, fixed warm root (*contrast only*) | 78.8 µs/commit |
| `apply`, 256 mutations, fixed warm root (*contrast only*) | 105.7 µs/commit |
| `apply`, empty batch | 118 ns (no store call) |
| `diff` at divergence 1 / 16 / 256 / 4096 | 234 µs / 307 µs / 534 µs / 7.69 ms |
| cold 256-key read wave, verify Always / Never | 6.28 ms / 1.69 ms |
| encode 64 KiB node / hash it / decode+validate it | 10.0 µs / 27.7 µs / 11.3 µs |

### Instruction, cache, branch, and allocation profile

Wall time alone did not establish where the implementation spent work. The repository therefore has
two deterministic external-profiler binaries. `tools/profile/allocations.sh` counts native system
allocator traffic in an explicitly delimited region. `tools/profile/cachegrind.sh` runs three paired
ARM64 Linux samples under Valgrind 3.19, subtracts the identical fixture-only process, and reports the
median under a declared reference cache: 32 KiB 8-way I1, 32 KiB 8-way D1, and 8 MiB 16-way last level,
all with 64-byte lines. The fixed geometry makes changes comparable; it does **not** model Apple M4
latency or claim these are physical hardware counters. The profile metadata includes the compiler,
architecture, model, repeat count, and source digest.

The deterministic fixture has 10,000 keys, 24-byte inline values, and 256-mutation construction batches.
Counts below are per operation. Allocator high-water is for the whole measured region (100 point-read or
apply iterations, two scans, or 1,000 hash/decode iterations), because retained cache entries are a real
property of that evolving region.

| operation | instructions | D1 read misses | D1 write misses | conditional mispredicts | allocations / reallocations | requested bytes | high-water live bytes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| warm `get_many`, 256 keys | 708,693 | 3,839 | 1,422 | 4,513 | 36.6 / 51.0 | 130.2 KiB | 76.8 KiB |
| collect 10,000-row scan | 7.63 M | 103,557 | 96,410 | 4,216 | 370.5 / 244 | 7.38 MiB | 1.22 MiB |
| stream 10,000-row scan, batches of 256 | 7.50 M | 84,441 | 83,813 | 4,035 | 369.5 / 238 | 5.39 MiB | 222 KiB |
| evolving one-key `apply` | 951,932 | 3,626 | 1,251 | 467 | 63.2 / 4.0 | 88.4 KiB | 6.45 MiB |
| hash one 64 KiB node | 290,438 | 1,035 | 28 | 42 | 0 / 0 | 0 | 0 |
| decode+validate one empty 64 KiB node | 318,378 | 1,059 | 24 | 9 | 2.0 / 0 | 176 B | 200 B |

These counters reject several vague optimization claims and expose concrete work:

- The first profile assigned 21% of streaming-scan instructions to reinserting an already-sorted leaf
  into a `BTreeMap`, with key comparison taking another 24%. Replacing that O(n log n), per-key-allocating
  step with one linear merge of the sorted leaf and sorted ancestor-overlay run cut full-scan instructions
  from 20.75 M to 7.63 M (**63%**), conditional mispredictions from 52,081 to 4,216 (**92%**), and
  allocation calls from 2,025.5 to 370.5 (**82%**). Using the resulting `Vec`'s consuming iterator as
  cursor state removed another 3.5% of instructions without adding an allocation. The
  native 256-row range benchmark independently improved 44% on uniform keys and 41% on long-prefix keys.
- Streaming remains primarily a memory/backpressure choice: against the optimized collector it reduces
  requested allocation bytes by 27% and high-water live bytes by 82%, while instructions differ by 1.7%.
- The two head-search functions consume 39% of warm batched-read instructions and 87% of its conditional
  mispredictions. Any replacement must beat that whole operation under the same profile, not only a
  synthetic compare loop.
- Hashing one canonical node allocates nothing. On this ARM64 run, 98% of its instructions are in BLAKE3's
  NEON hash-many path, so a hand-written search SIMD loop says nothing about hashing efficiency.
- In an evolving one-key apply, BLAKE3 accounts for 60% and validated decode for 25% of instructions.
  Parallel hashing remains unjustified by this serial profile; the concurrent Criterion matrix remains
  the authority for whether parallelism improves throughput.
- Full validation of an empty canonical 64 KiB node costs 318,000 simulated instructions, chiefly the
  zero-padding and offset sweep. The decoded cache is therefore performance-critical, but weakening
  validation is not an acceptable optimization.

The generated TSV, variability table, matched-delta function reports, and methodology live under
`target/profile/` and `tools/profile/README.md`. Last-level deltas smaller than fixture noise can be
negative after subtraction and are deliberately not used above.

Four-worker stationary concurrent throughput; every Criterion sample uses a fresh writable overlay and
cache over the immutable corpus, so repeated samples cannot turn writes into already-durable no-ops:

| workload | concurrency | throughput |
| --- | ---: | ---: |
| independent 32-mutation apply | 1 / 8 / 32 | 9.55k / 22.02k / 24.33k ops/s |
| mixed exactly 25% 16-mutation apply / 75% 64-key read | 4 / 8 / 32 | 3.41k / 5.65k / 14.47k ops/s |
| sequential 4,096-mutation apply | 1 / 8 / 32 | 132 / 351 / 479 ops/s |
| task-parallel 4,096-mutation apply | 1 / 8 / 32 | 122 / 355 / 468 ops/s |

These are MemStore compute/contention results, not production-store throughput. They show scaling and
the onset of contention; the target-store model separately prices dependent latency and bandwidth.
At 32 tasks, disabling both caches reduces the same mixed workload from **14.47k to 4.21k ops/s**.
That establishes that caching is load-bearing; the 64 MiB / 16 MiB split remains configurable because
the inline-value fixture can select the node budget but cannot select a universal value working set.

Cursor lookahead is priced as store interaction, not MemStore compute. A full selected-format scan reads
the same 162 objects / 10.6 MB at every width:

| prefetch width | dependent waves | maximum speculative node bytes |
| ---: | ---: | ---: |
| 0 | 162 | 0 |
| 1 | 162 | 64 KiB |
| 16 | 84 | 1 MiB |
| **64 (default)** | **11** | **4 MiB** |
| 256 | 6 | 16 MiB |

Width 64 is the knee: 15x fewer waves than no lookahead, while width 256 buys only another 1.8x for 4x
the per-cursor lookahead memory. `ScanCursor::with_prefetch_width` and
`DiffCursor::with_prefetch_width` expose the trade-off; exactness is tested at widths 0, 1, 64, and 256,
and diff still performs equal-subtree pruning before lookahead.

The identical-broad-prefix row is a deliberate negative result: its cost is the *answer* (one corpus
copy per prefix), which no batching can reduce. It is measured so a future regression elsewhere cannot
hide behind it.

Storage interaction, from `CountingStore` (build of 100 000 keys at commit width 256):

- build: 392 `put_batch` calls, 760 objects submitted, 49.8 MB written, **0 dependent reads**.
- resulting shape: depth 2, 162 nodes, mean fanout 26.83, amplification 1.56, effective ε 0.509.
- cold point read: **3 waves**, 3 objects, 196 608 bytes.
- cold 256-key `get_many`: **3 waves**, 162 objects, 10.6 MB — the same wave count as one key.
- 232 flushes, mean victim 68 352 bytes, **0 undersized flushes**, 0 directly routed messages.
- 256 absent keys: 512 absent-key buffer probes out of 768 total probes. That is the work a
  no-false-negative routing summary could skip: two thirds of all probes on an all-miss workload, and
  none of the node I/O, since the internal node is fetched for its child pointer regardless.

## Acceptance criteria

The RFC is implemented only when all of the following hold. Each is followed by the test that
demonstrates it; all of them pass.

| criterion | demonstrated by |
| --- | --- |
| internal nodes widen beyond two children; split propagation preserves equal leaf depth | `shape.rs`: `one_huge_apply_produces_a_wide_balanced_tree`, `twenty_thousand_single_applies_stay_balanced_and_wide`, `the_shape_matrix_is_balanced_and_wide_everywhere` (4 key shapes x 3 orders x 4 commit widths, all via `harness::check_balanced`) |
| every multi-node internal replacement partitions its buffer at every promoted pivot | `shape.rs::internal_partitions_keep_every_message_on_its_own_path`; `harness::check` verifies path ownership for every entry of every node |
| every node is exactly `NODE_BYTES`; every legal key is representable without an overflow shape | `codec.rs::a_maximum_entry_fits_a_canonical_leaf`, `a_full_width_long_key_internal_node_fits`; `shape.rs` asserts `physical_bytes == nodes x NODE_BYTES` |
| no object exceeds `MAX_OBJECT_BYTES`; oversize writes fail without publishing a root | `tree.rs::an_oversize_key_or_value_fails_without_publishing_a_root` |
| at most one candidate per key per node; batch keys collapse in program order; reused stamps resolve identically on every replica | `tree.rs::repeated_batch_keys_collapse_in_program_order`, `a_reused_order_key_resolves_identically_on_every_replica`, `a_reused_order_key_between_two_upserts_resolves_by_value_bytes`; encoder rejects duplicates |
| an empty batch performs no store operation and returns its input root | `tree.rs::an_empty_batch_performs_no_store_operation_and_returns_its_input_root` |
| every ordinary flush records victim bytes at least `max(ceil_div(pending, children), MIN_FLUSH_BYTES)` | `shape.rs::every_ordinary_flush_meets_the_byte_floor`; `BeTree::metrics().undersized_flushes == 0` across every shape fixture |
| root growth beyond `MAX_TREE_LEVEL` fails without publication; malformed batched-read cardinality is rejected before bytes meet ids | `tree.rs::root_growth_beyond_max_tree_level_fails_without_publication`, `a_malformed_batched_read_cardinality_is_rejected_before_bytes_meet_ids` |
| malformed bytes cannot cause an out-of-bounds access, panic, UB, or unbounded recursion | `corruption.rs` (8 tests, ~3000 mutated and arbitrary byte strings, both verify policies, plus a self-referential node); `codec.rs::every_single_byte_mutation_is_rejected_or_valid_never_a_panic` |
| hash mismatch, malformed bytes, wrong version domain, capacity, resource limits, and store failure are distinguishable typed errors | `tree.rs::verify_on_read_catches_a_lying_store_and_is_a_distinct_error_class`; `codec.rs::a_node_from_another_version_domain_is_its_own_error_class`; `corruption.rs::assert_typed` |
| rebuilding the same logical node produces the same canonical bytes and `BlockId` | `codec.rs::rebuilding_the_same_logical_node_gives_the_same_bytes`; `harness::check` re-encodes **every** node of every generated tree |
| selected search is property-equivalent to an independent full-key reference | `search_equivalence.rs` (4 tests spanning random surfaces, validated nodes, child routing, and the occupancy threshold) |
| `diff` has no whole-subtree collection fallback caused only by pivot misalignment | `diff.rs::misaligned_shapes_resynchronize_by_key_and_stay_exact`, `a_localized_edit_visits_far_less_than_the_corpus`; the overlay cases `a_newer_ancestor_buffer_message_over_an_equal_subtree_is_detected` and `an_older_ancestor_buffer_message_is_not_a_difference` |
| point, range, snapshot, tombstone, replay, reused-stamp, batch-order, streaming scan, and streaming diff tests pass | `model.rs` (10 tests, 11 seeds, per-commit model comparison and snapshot replay), `tree.rs` (25 tests) |
| every released root is either selected by the authenticated schema registry or rejected, and selected V2 rewrites to V3 without changing resolved history | `migration.rs::a_registered_historical_root_rewrites_every_winner_semantic`, `golden.rs::the_shipped_schema_ids_are_pinned` |
| 4 KiB identity keys retain useful fanout and meet the crate's published depth/amplification budget | explicit release-only `shape.rs::one_hundred_thousand_four_kib_keys_keep_depth_and_amplification`: depth 3, fanout 7..32, amplification 1.094 |
| allocation-free mutation preflight is exactly the predicate used by `apply` | `tree.rs::mutation_preflight_and_apply_agree_at_every_size_boundary` |
| range/prefix scan and diff expose bounded batches, including when the corpus exceeds cache | `tree.rs::streaming_scan_and_diff_match_the_collecting_convenience_apis`, `streaming_a_corpus_larger_than_the_cache_never_materializes_the_result` |
| cursor lookahead is configurable without changing results, and its default is a measured round-trip/memory knee | the same streaming-equivalence test at widths 0/1/64/256; `benches/workloads.rs::report_prefetch_widths` |
| one unfiltered Miri command covers all targets not explicitly annotated as native-scale | `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test`: 75 passed, 69 ignored, 0 filtered |
| benchmarks report regressions as well as improvements; no explicit-SIMD, routing-summary, or separately addressed leaf-partition path is default without a crossover | the selected-constants section: SIMD 1.2–2.3x **slower**, bundled prefixes 2–4x **slower**, count reduction 2.9x slower at 640, separately addressed leaf partitioning recorded as workload-dependent and not defaulted, routing summaries priced but not built |

Verbatim criteria:


- internal nodes widen beyond two children and split propagation preserves equal leaf depth;
- every multi-node internal replacement partitions its buffer at every promoted pivot;
- every node is exactly `NODE_BYTES`, and every legal key is representable without an overflow shape;
- no object exceeds `MAX_OBJECT_BYTES`, and oversize writes fail without publishing a root;
- each node contains at most one candidate per key, repeated batch keys collapse in program order,
  and reused stamps resolve identically on every replica;
- an empty batch performs no store operation and returns its input root;
- every ordinary flush records victim bytes at least
  `max(ceil_div(total_pending_bytes, child_count), MIN_FLUSH_BYTES)`, excluding immediately routed
  oversized messages;
- a root-growth attempt beyond `MAX_TREE_LEVEL` fails without publication, and malformed batched-read
  cardinality is rejected before bytes are associated with IDs;
- malformed bytes cannot cause an out-of-bounds access, panic, or UB;
- hash mismatch, malformed bytes, a wrong version domain, capacity exhaustion, resource limits, and
  store failure remain distinguishable typed errors;
- rebuilding the same logical node produces the same canonical bytes and `BlockId`;
- selected search is property-equivalent to its independent full-key reference;
- `diff` has no whole-subtree collection fallback caused only by pivot misalignment;
- point, range, snapshot, tombstone, replay, reused-stamp, and batch-order model tests pass; and
- benchmark results report regressions as well as improvements. No explicit SIMD, routing-summary,
  or separately addressed leaf-partition path becomes the default without a demonstrated crossover, and no Bε asymptotic
  bound is claimed from `MemStore` timings.

## Explicitly declined or deferred

- **Bit-packed offset/metadata columns:** declined. Blocks of 128–1024 values do not fit the expected
  node occupancy, random access costs more instructions, and nonzero padding creates another
  canonicality surface. Plain `u32` offsets retain O(1) access.
- **Whole-node compression inside the content boundary:** declined. Hashing compressor output couples
  identity to codec/version choices; hashing canonical uncompressed bytes requires decompression and
  defeats zero-copy. A `NodeStore` may compress below the content-address boundary if it returns and
  verifies the canonical bytes identified by `BlockId`.
- **Clock-specific order-key compression in the baseline:** declined. The tree core treats order keys
  as opaque. Prefix or dictionary compression may be benchmarked only as a scheme-independent
  canonical encoding; the node codec must not reinterpret milliseconds, logical counters, or writer
  IDs.
- **Bundled prefix skips:** declined **on measurement**. Proposed to bound a large equal-head range;
  the equal-head range is already 1 at p99 on every fixture, so there is nothing to bound, and they cost
  2–4x per probe. The rejected implementation was deleted; the measurements remain in this RFC, and no
  second `schema_id` exists.
- **Explicit AVX2/NEON kernels:** declined **on measurement**. The hand-written NEON count kernel is
  1.2x–2.3x slower than LLVM's autovectorization of the same reduction. The feature and implementation
  were deleted; production search contains no `unsafe`.
- **Content-defined split points:** deferred with confluence and delete-side collapse to a separate
  RFC.
- **Persisted routing filters and shared immutable buffer runs:** deferred until baseline measurements
  price buffer probes and COW buffer copying. Both add object-graph, GC, and canonical-verifier state.
- **Persisted logical subtree counts:** deferred. Buffered upserts and tombstones make resolved
  cardinality depend on messages above the child, so a locally maintained count is not a simple
  derivable field. `tree_level` is retained because equal-depth validation has no such ambiguity.

## Risks and limits

- **Large-object workloads:** values above `INLINE_VALUE_BYTES` add a value-object fetch and GC edge;
  exceeding the configured key/value hard limit is a visible write error.
- **Shared-prefix keys:** suffix heads help only after a mandatory common-prefix check; equal-head
  ranges still require full comparisons.
- **Space amplification:** exact-size regular nodes contain canonical zero padding. The benchmark
  reports physical bytes per live logical byte.
- **COW root amplification:** every apply that changes tree bytes creates at least a root object.
  Large commit batches amortize that cost; single-message commits do not inherit the classical
  mutable-root bound.
- **Unsafe SIMD:** if explicit intrinsics survive the benchmark matrix, validation establishes bounds,
  but search still requires focused fuzzing, Miri coverage of scalar/view code, sanitizers where
  supported, and code review.
- **Underflow:** tombstone winners avoid immediate physical deletion. A future design that proves a
  tombstone globally stable and removes the key must specify merge/steal, snapshot interaction, and
  whether confluence is required.

## References

Detailed prior-art notes and measured survey results:

- [buffered-tree implementations](research/buffered-trees.md)
- [block formats](research/block-formats.md)
- [content-addressed and Merkle trees](research/content-addressing.md)
- [canonical encoding and decode](research/encoding-and-decode.md)
- [in-node search](research/in-node-search.md)

1. Gerth Stølting Brodal and Rolf Fagerberg, “Lower Bounds for External Memory Dictionaries,”
   SODA 2003, pp. 546–554. <https://dblp.org/rec/conf/soda/BrodalF03>
2. Michael A. Bender, Martin Farach-Colton, William Jannen, Rob Johnson, Bradley C. Kuszmaul,
   Donald E. Porter, Jun Yuan, and Yang Zhan, “An Introduction to Bε-trees and
   Write-Optimization,” *;login:* 40(5), 2015, pp. 22–28.
   <https://www.usenix.org/publications/login/oct15/bender>
3. BLAKE3 Rust crate 1.8.6 API documentation, especially `Hasher::update_rayon`.
   <https://docs.rs/blake3/1.8.6/blake3/struct.Hasher.html#method.update_rayon>
4. Alex Conway et al., “SplinterDB: Closing the Bandwidth Gap for NVMe Key-Value Stores,” USENIX
   ATC 2020. <https://www.usenix.org/system/files/atc20-conway.pdf>
5. Yandong Mao, Eddie Kohler, and Robert Morris, “Cache Craftiness for Fast Multicore Key-Value
   Storage,” EuroSys 2012. <https://pdos.csail.mit.edu/papers/masstree:eurosys12.pdf>
6. Paul-Virak Khuong and Pat Morin, “Array Layouts for Comparison-Based Searching,” 2015.
   <https://arxiv.org/abs/1509.05053>
