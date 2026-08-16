# be-tree performance optimization plan

Status: implementation and integration validation complete for the measured plan: accepted Phase 1 read-path experiments, including the direct scalar `get` path, are implemented; Phase 3 encoder-buffer, Phase 4 flush, and leaf-capacity experiments were rejected by whole-operation evidence; the COW shape harness and downstream consumer gate are complete. Phase 2 representation changes and Phase 6 concurrency changes were not justified by the measured hotspots, and Phase 5's existing value-separation path was validated rather than replaced. A 2026-08-14 review pass repaired the measurement gates (commit-sized COW shapes, genuinely cold point-get rows, tombstone-scan and mixed-apply Cachegrind rows) and a budget-accounting parity defect, and re-captured the full matrix from committed revision `2fc2330`; see the profiling report's matching update. This closes the measured plan, not remote-store or exhaustive-concurrency validation.
Date: 2026-08-14 (measurement gates re-validated 2026-08-14)

The current implementation retains the existing canonical format and scan merge optimization while
measuring the next read/write experiments. A three-sample Cachegrind baseline is now captured through
the repository container; the expanded read, scan-cardinality, apply-shape, and point-get Cachegrind
matrices are now captured. The downstream consumer gate has also passed against the local path dependency;
remote production storage validation remains an environment limitation, not an unrun repository gate.

The remaining limits are explicit: capacity results report both objects touched and bytes under a
node-transfer model, but do not choose a universal node size or `F_MAX`; the public
`BeTree::references`/`references_many` methods are the supported graph-walk surface for downstream
shape and GC checks, while no separate shape-summary API is promised; and the concurrency tests are
native async stampede checks, not loom-style exhaustive interleaving validation. No remote-store
latency, billing-unit, or multi-worker throughput claim is made.

## Objective

Improve the primary consumer workloads without preserving the current internal node layout or
serialized format. The work will prioritize whole-operation latency, allocation pressure, cache
locality, and storage traffic rather than isolated microbenchmarks.

The target workload families are:

- Point reads and batched point reads (`get`, `get_many`).
- Prefix and range scans, including multi-prefix scans.
- Batched tree updates (`apply`) with realistic commit-sized mutation sets.
- Content-addressed COW rewrites and root publication.

The current profiling report identifies probe/merge work in `resolve_many` as the dominant warm-read
cost, and full-node encoder allocations plus rewrite/materialization work as the dominant write cost.
The current scan merge optimization in the multi-root scan path, marked by the merged-scan logic and
doc comment at `src/tree.rs:2092`, is the baseline and must be retained unless a replacement wins on
the same scan matrix.

## Compatibility and correctness boundary

The downstream consumer does not require the current serialized bytes, node shape, fanout, or format parameters. A
format-breaking change is acceptable while the system is pre-production and existing data can be
discarded or rebuilt.

The following properties are nevertheless hard contracts because the downstream consumer observes them through its
L1 adapter, L2 snapshots, and GC:

1. A finalized immutable object's ID is the hash of its canonical bytes, and the store receives the
   exact bytes associated with that ID.
2. Existing roots remain immutable. A new update produces a new root and may share unchanged objects.
3. Equal canonical content produces equal IDs, so subtree equality, snapshot sharing, and Merkle-style
   diff pruning remain meaningful.
4. Every reachable node and out-of-line value is discoverable by the reference walker and classified
   correctly for GC.
5. Point reads, misses, deletes/tombstones, range ordering, prefix boundaries, and batched result
   ordering remain unchanged.
6. Version/HLC winner resolution remains deterministic and preserves the current observable ordering
   rules, including same-batch behavior.
7. Store bounds, resource limits, corruption detection, hash validation, and error behavior are not
   weakened for speed.
8. One logical commit can still be submitted as one addressed batch to the `NodeStore` adapter.

Changing these properties is out of scope for performance work and requires an explicit consumer
architecture decision.

## Non-goals

- Do not preserve old on-disk bytes or provide migration unless separately requested.
- Do not optimize only a synthetic inner loop while worsening complete `get_many`, scan, or `apply`.
- Do not reapply the previous ordered-frontier sorting experiment without a new hypothesis and a
  matched regression guard.
- Do not add speculative SIMD, parallelism, or a new hash algorithm before the dominant work is
  removed and the end-to-end contribution is measurable.
- Do not change downstream consumer code during the first be-tree experiment series. If the adapter contract must
  change, stop and document the required coordinated change before proceeding.

## Measurement protocol

All comparisons use the same optimized toolchain, target, allocator, feature set, fixture generator,
storage model, and source metadata. Each result records the command, fixture dimensions, architecture,
profiler/cache model, checksum, and source revision or content digest.

### Required workload matrix

| Workload | Required shapes | Primary metrics |
| --- | --- | --- |
| `get_many` | 1, 16, 256, and 1024 keys; hits, misses, mixed; random and sorted keys | p50/p95 latency, allocations, bytes, instructions, branches |
| point `get` | hot cache and cold decoded cache; short and long common prefixes | latency, node loads, bytes fetched, cache behavior |
| scan | short and long ranges; 2, 32, and 256 returned rows | latency, allocations, instructions, mispredicts |
| scan-stream | same ranges as scan, streaming output | latency, peak live memory, allocations |
| `apply` | small, medium, and commit-sized batches; repeated and distinct keys; mixed and delete-heavy tombstone batches | latency, allocations, encoded bytes, peak live memory |
| COW rewrite | low and high overlap with an existing tree | new objects, bytes written, root-sharing rate |

The scan matrix must include a tombstone-dense range in addition to live-value ranges. The profile
fixtures now cover this surface: `scan-tombstone`/`scan-stream-tombstone` scan a persisted 2,000-key
tombstone interval, and `apply-WIDTH-delete`/`apply-WIDTH-mixed` exercise delete-heavy and interleaved
upsert+tombstone batches. Tombstone behavior remains an explicit regression surface — keep these
shapes in every matrix rerun rather than treating them as incidental.

Before Phase 0, confirm that the key/value sizes, duplicate rate, delete rate, common-prefix length,
batch widths, and update overlap match the intended consumer workloads. If the consumer does not yet have
representative traces, record the synthetic distributions and treat the result as provisional.

At least three deterministic allocation and Cachegrind samples are required. Native timing should use
enough repetitions to report a distribution rather than a single elapsed time. Warm, cold, and
capacity-pressure cache cases must remain distinct.

The existing `benches/profile.rs`, `benches/allocations.rs`, `tools/profile/` commands, and downstream
consumer workload should be reused where trustworthy. If a harness cannot consume results, fails to
chain returned roots, or excludes meaningful setup from the measured region, repair the harness before
using its numbers to select an optimization.

## Ranked experiment sequence

Each experiment is one coherent change. A change is retained only if it passes correctness checks and
improves the complete workload matrix without a material regression in another primary workload.

### Phase 0: Freeze the baseline

First commit the currently intended scratch-pooling work in `src/tree.rs` and `src/codec.rs` (the
in-flight work is roughly 600 uncommitted lines at the time this plan was written). The baseline must
be attributable to one committed revision before native timing, hotpath, allocation, and Cachegrind
measurements are compared. Do not mix baseline capture with additional optimization edits.

Capture the current scratch/codec baseline for all required workloads. Record:

- Native timing distributions.
- Allocation calls, reallocations, allocated bytes, peak live bytes, and retained live bytes.
- Cachegrind instructions, branches, and simulated cache misses.
- Node loads, bytes fetched, objects written, and root checksums.
- Current function-level profile for `resolve_many`, `wave`, `probe`, `step1`, encoders, and rewrite
  helpers.

This phase produces the comparison artifact used by every later experiment.

### Phase 1: Replace repeated batched lookup work

Hypothesis: warm `get_many` is dominated by repeated probe/merge work, not node loading. Group query
state by covering node and process each node's sorted key region once, carrying only compact query
indices into child groups. Preserve input-order result materialization at the boundary.

Candidate designs:

- A flat per-batch query buffer instead of one winner allocation per key.
- One node-local probe pass for all queries covered by that node.
- Specialized paths for already-sorted keys and for a single-node batch.
- A small-batch path that avoids general frontier machinery.

Acceptance gate: improve `get_many` latency or allocation totals on at least two batch widths, with no
regression in misses, long-prefix keys, scans, or result ordering. Explicitly run the tree-level
duplicate-key test represented by `tests/tree.rs:255` (`get_many_aligns_to_input_and_preserves_duplicates`),
or an equivalent focused test. Duplicate keys within one batch must remain in input order and resolve
to identical winners under the grouped traversal, including same-batch updates. Reject if the gain
exists only in the instrumented function and not in the complete operation.

### Phase 2: Compact finalized node representation

Hypothesis: `Surface::probe` and `step1` are expensive because hot routing metadata is spread across
objects or requires repeated decoding. Replace the internal representation with packed arrays or
offset tables, optionally using prefix compression, while keeping canonical serialization and hash
validation explicit.

Candidate designs:

- Contiguous routing keys and child/value references.
- Offset tables for variable-length keys and values.
- Prefix-compressed keys with validated shared-prefix bounds.
- Separate hot routing metadata from large values.

Acceptance gate: compare point reads, batched reads, scans, and decode costs. The new representation
must preserve deterministic bytes for equal logical nodes and must not increase fetched bytes or peak
memory beyond the agreed workload budget. Build the same logical tree through at least two insertion
histories and assert equal root IDs; this is the canonicalization guard for prefix compression and
other layout changes.

### Phase 3: Reduce write-path materialization

Hypothesis: `encode_leaf` and `encode_internal` allocate a full node-sized buffer on every rewrite,
while `apply_prepared`, `normalize`, and `pack_leaves` create overlapping temporary structures.

Candidate designs:

- Per-apply scratch output buffers with safe ownership transfer into addressed objects.
- Pooled or size-classed `BytesMut` for node encoding.
- Direct packing from normalized runs into final output buffers.
- Removal of intermediate vectors where the final node can be written in one pass.

Acceptance gate: reduce apply allocation calls/bytes and peak live memory without increasing encoded
bytes, write count, hash cost, or root construction latency. Verify that every transferred buffer is
immutable before it is addressed and stored.

### Phase 4: Revisit buffering and flush policy

Hypothesis: fewer, better-sized flushes can reduce COW rewrites and serialization work more than local
encoder tuning.

Candidate designs:

- Adaptive buffer thresholds based on bytes and subtree pressure.
- Cost-based or heaviest-child flush selection.
- Direct merging of staged updates into sorted leaf/ancestor runs.
- Delayed materialization until root publication.

Acceptance gate: measure both write latency and resulting read shape. Reject policies that improve
`apply` by creating taller trees, excessive read amplification, larger nodes, or poor long-lived
tombstone behavior.

### Phase 5: Value separation and cache policy

Hypothesis: large values pollute node caches and cause unnecessary copies during routing and scans.

Candidate designs:

- Out-of-line value objects with explicit node-to-value references.
- Metadata-only node caching for one-shot scans.
- Separate cache admission for values versus routing nodes.
- Read-through value loading only after the winning key is known.

Acceptance gate: compare small and large values, hot and cold caches, point reads and scans, including
GC reachability and bytes fetched. Any out-of-line representation must update reference enumeration,
object-kind handling, and corruption tests together. Confirm that the increased object count and
addressed-object batch size remain within `NodeStore` batch and resource bounds.

### Phase 6: Hashing and concurrency only if justified

Measure hash bytes and hash time before changing hashing. Measure one writer through oversubscription
before adding parallel flush or read work. Parallelism is accepted only with throughput, p95/p99,
CPU utilization, contention, and memory amplification evidence.

## Correctness and integration gates

For each retained experiment:

1. Run focused tree, codec, corruption, search-equivalence, read-path, and write-path tests relevant to
   the changed representation.
2. Run the proportional full crate gates.
3. Run the downstream consumer build/tests against the new dependency.
4. Exercise content-addressed storage with hash mismatch, missing object, bounded read, and GC walk
   cases.
5. Re-run the identical performance matrix and inspect the new hotspot profile.

No result is considered an improvement if correctness passes only because the relevant case was not
tested, or if the benchmark does not consume returned roots/results.

## Decision and rollback rules

- Keep a patch only when its causal hypothesis is supported by profile attribution and its complete
  workload result is non-degrading.
- A timing win smaller than measurement noise is rejected unless it also removes a deterministic,
  material allocation or storage cost.
- A regression in consumer-visible semantics is an immediate rejection regardless of speed.
- A format change that invalidates existing data is acceptable only when explicitly labeled as a reset
  boundary; do not silently claim it is compatible with existing stores.
- Preserve each accepted experiment as a narrow, independently reviewable commit or patch series.
- If two experiments interact, measure the combined result against the frozen baseline rather than
  comparing only adjacent microbenchmarks.

## Expected deliverables

- Baseline and per-experiment machine-readable measurements outside source control unless the repository
  already versions them.
- One narrow implementation patch per accepted hypothesis.
- Updated profiling report with matched before/after numbers and unavailable dimensions called out.
- Focused correctness coverage for any changed encoding, reference, cache, or batching behavior.
- A final note identifying the remaining dominant hotspot and the workload/environment limits of the
  claim. The result must be described as improved for measured workloads, not maximally performant.

## Review questions

The reviewing agent should specifically challenge:

1. Whether the content-addressed COW/Merkle invariants are stated completely.
2. Whether the proposed grouped traversal can preserve duplicate-key and input-order semantics.
3. Whether out-of-line values add GC or fetch costs that the matrix would miss.
4. Whether the workload sizes and value/key distributions represent the consumer's actual use.
5. Whether any proposed format reset could accidentally affect retained snapshots or existing L0 data.
6. Whether a phase has a sufficiently measurable hypothesis to justify implementation.
