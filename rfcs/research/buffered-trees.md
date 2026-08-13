# Buffered-tree implementations

SplinterDB, TokuDB/PerconaFT, BetrFS, sled, Haura, Tucana. What real write-optimized stores do about
node size, flush policy, split propagation, and the read-side cost of buffering.

## 1. Routing filters: point reads should not probe every buffer

**SplinterDB** (ATC'20, <https://www.usenix.org/system/files/atc20-conway.pdf>; maplets follow-up
PACMMOD 2023, <https://dl.acm.org/doi/10.1145/3588726>) attaches a **quotient filter** to every
branch and stores, per child pointer, `a_c` = the oldest still-active branch index for that child.
A point query probes filters youngest-to-oldest and reads a branch only on a hit, so most point
queries do one I/O. Cost: 1–2 bytes/key, ~6% of DB size at 32-byte pairs, FPR <1%, and a probe
touches 1–2 adjacent cache lines (unlike Bloom's random probes). Quotient *maplets* can be merged
and resized without touching the underlying data, so a compaction merges filters as cheaply as runs.

**Relevance:** this is the single largest omission in the RFC. §"Conformance boundary" states as
load-bearing that "a point query examines every buffer on the correct root-to-leaf path" — true of
the current code, but SplinterDB shows it is not inherent to the Bε mechanism. A per-message-group
quotient/xor filter plus a per-child min-live-generation would cut point-read work from O(depth)
buffer scans to O(depth) cache-line probes. Worth an explicit "future work" section at minimum;
it also changes what the benchmark harness should measure.

## 2. NODE_BYTES: the buffered-tree literature says much larger than 4–16 KiB

- **PerconaFT** (`ft/ft-internal.h`): `FT_DEFAULT_NODE_SIZE = 4MB`,
  `FT_DEFAULT_BASEMENT_NODE_SIZE = 128KB`, `FT_DEFAULT_FANOUT = 16`. Leaves are partitioned into
  independently compressed **basement nodes** with per-partition on-disk offset/length, so a point
  query decodes only one basement, not the whole 4 MB node.
- **Bender et al.** (the RFC's own reference [2]) give the reason: in a Bε-tree the per-insert
  bandwidth cost is divided by ~√B, so cost grows very slowly with B — large nodes buy
  near-bandwidth range scans *and* faster inserts. With node ≈4 MB and fanout 4–16, "the Bε-tree can
  always flush in batches of at least 256KB."
- **BetrFS** (FAST'15) uses the same 4 MB nodes and, because messages are variable-length,
  deliberately **does not fix ε** — it bounds pivots per node between 4 and 16.
- Counterweight: **Dolt** clamps chunks to 512 B–16 KiB (target 4 KiB), and `bao-tree` defaults to
  16 KiB blocks. Both are content-addressed, and that is the operative difference — COW rewrites the
  whole node, so large nodes amplify write cost per commit in a way TokuDB's mutable pages did not.

**Relevance:** the RFC defers `NODE_BYTES` to step 7, which is right, but the harness matrix should
span 4 KiB → 1 MB, not just the small end, and should include the **basement/partition** idea: fixed
`NODE_BYTES` with internally partitioned leaf regions decouples "flush batch size" from "bytes
decoded per point read." That is the mechanism that makes large nodes tolerable.

## 3. Minimum flush size

**Haura/`betree`** (<https://github.com/parcio/haura>, `betree/src/tree/imp/flush.rs`) — the closest
existing Rust Bε-tree — uses `MIN_FLUSH_RATIO = 16`, i.e. a flush must move at least
`MAX_SIZE/16` or it is not worth doing; `MIN_FANOUT = 2`; `MAX_MESSAGE_SIZE = 512KB`. Its
`rebalance_tree` loop matches the RFC's proposal closely and is worth reading line by line: pick
largest child buffer → if no candidate, split → if child too large, descend first → merge low-fanout
children → flush → split leaf in a `while` loop → repeat if still too large. SplinterDB's equivalent
is "flush only when branches hold ≥ m bytes," m = memtable size.

**Relevance:** the RFC's flush rule ("greatest pending encoded byte count, lowest index tie-break")
has no floor. Under COW a flush mints new nodes for both parent and child, so flushing the heaviest
child when the heaviest child owns 200 bytes is pure amplification. Add a `MIN_FLUSH_BYTES` and make
the acceptance criterion "victim bytes ≥ max(M/c, MIN_FLUSH_BYTES)".

## 4. Flush accounting and promotion heuristics

**PerconaFT** `find_heaviest_child` weights a child by `nbytesinbuf(child) + BP_WORKDONE(child)`, and
`toku_ftnode_nonleaf_is_gorged` tests serialized node size + total workdone against the node size —
fullness is byte-accounted *including work already done*, not entry counts. Promotion rules: never
promote broadcast messages, never promote past a non-empty buffer, otherwise inject at most to
height 1 / depth 2 — except always to the leaves on the leftmost/rightmost edges, a sequential-insert
optimization gated on `FT_SEQINSERT_SCORE_THRESHOLD = 100`.

**Relevance:** the RFC counts only pending encoded bytes, which under-weights a child that has
repeatedly absorbed flushes. A `workdone`-style term is cheap to carry and is what PerconaFT
converged on. The edge-promotion heuristic is also the answer to sorted-key workloads, which
otherwise defeat buffering entirely (see §7).

## 5. Split propagation and COW constraints

- **Reserve `F_MAX + H` pivot slots** (SplinterDB, H = height bound ≈ 10) and preemptively split
  during a flush when fanout would exceed `F_MAX`. A flush to a leaf can cause O(log_F N) splits,
  which breaks the B-tree "one split per level" assumption. Budgeting for overshoot is simpler than
  bounding it — and it directly answers the RFC's multiway-split-propagation problem.
- **No leaf sibling pointers.** Rodeh, *B-trees, Shadowing, and Clones* (ACM TOS 2008, the btrfs
  basis) — sibling pointers force cascading shadowing under COW. The RFC's format has none; worth
  recording as a deliberate constraint so nobody adds them for scan performance. Rodeh also argues
  for proactive top-down split/merge so a write path shadows each node exactly once, which is what
  SplinterDB does and an alternative to the RFC's bottom-up pivot promotion.
- **Buffers as refcounted immutable shared runs** is the pattern in every system that made
  immutability × buffering work: flushes become pointer swings, splits become O(1) in buffer bytes,
  and filters stay mergeable. SplinterDB's **flush-then-compact** decouples the two — flush is a
  refcount/pointer copy under a brief write lock, compaction happens in background — which is why it
  reaches write amp ≈ 1 on skewed workloads. The tax is space amplification and GC (see §6).
- **Key lifting** (BetrFS FAST'18, <https://oscarlab.github.io/papers/fast18-betrfs.pdf>): all keys
  in a subtree begin with the longest common prefix of its enclosing pivots, so that prefix is
  stripped from every key *and message* in the subtree and reconstructed from the root path. Composes
  well with COW, since the prefix lives in the parent, which is being rewritten anyway. That paper
  also states that target node size and fanout are **performance, not correctness, invariants** and
  may be transiently violated — worth borrowing as framing for the RFC's overflow paths.

## 6. sled's two self-identified mistakes

<https://github.com/spacejam/sled#architecture>, <https://sled.rs/perf.html>. Both bear on this RFC:

- Log-scattered **page fragments** caused space amplification and GC pain; the stated priority was a
  storage rewrite into Marble to "dramatically lower both disk space usage and garbage collection
  overhead." A COW content-addressed store has the same hazard — budget for a compactor and a
  fill-ratio knob up front (`target_heap_file_fill_ratio: 0.9`).
- `Arc`-per-node-per-value graphs were a measured cost, so sled moved to **single-allocation**
  prefix-encoded nodes (CHANGELOG #1231) — which is what this RFC proposes, so this is confirmation
  rather than a correction. Current `Leaf<const LEAF_FANOUT: usize>` stores keys prefix-stripped
  against `lo`, recomputing the prefix as the common prefix of `lo`/`hi`.
- `max_inline_value_threshold: 4096` is a real precedent for the overflow threshold being a byte
  constant rather than a heuristic.
- Durable framing from perf.html: the **RUM conjecture** (read/update/memory — pick two; an immutable
  purely-functional tree buys contention-free R+U by never reclaiming space), and "avoid short-lived
  allocations, and make long-lived allocations spend the plentiful resource."

## 7. When buffering stops paying

- **The flush target is already cached.** **Tucana** (ATC'16,
  <https://www.usenix.org/system/files/conference/atc16/atc16_paper-papagiannis.pdf>) deliberately
  buffers only at the lowest part of the tree, arguing the index usually fits in memory so buffering
  at in-memory interior nodes only burns CPU cycles per operation. This is the clearest published
  statement of the boundary: a buffer whose flush target is resident converts an avoided I/O into
  pure CPU and memcpy.
- **The payload per key approaches node size.** BetrFS FAST'15 reports large sequential writes
  reaching only ~half disk bandwidth because every block percolated through interior buffers; writing
  straight to leaves matched other filesystems. FAST'16's **late-binding journal** is the principled
  version — log an unbound entry (op + key, no value), write the value once into the tree node, then
  append a binding entry with the node's physical address — used only for ≥1 MB runs, because an
  unbound insert prematurely forces a node to disk.
- **Keys arrive sorted** — use PerconaFT-style edge promotion (§4) rather than buffering.
- **CPU, not I/O, is the bottleneck.** KVell (SOSP'19) on NVMe: don't sort on disk, share nothing,
  batch I/O. Same force that drove SplinterDB's cache-line-local quotient filters and Tucana's
  stop-buffering-high rule.

## 8. Also surveyed

**Bw-tree** (Microsoft, ICDE'13) — delta records chained off a mapping table, i.e. per-node update
buffering without in-place writes; consolidation is the compaction step. Instructive mainly as the
cautionary case sled cites: delta chains make reads scan and push compaction cost onto readers
(RUM's U+M corner). **LMDB** — single-writer COW B+tree, page-granular shadowing, freelist keyed by
transaction id; the simplest correct GC design for a COW tree, and sled's README concedes it wins for
rarely-writing multi-process workloads.
