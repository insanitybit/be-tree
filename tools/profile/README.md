# Reproducible performance profiles

Wall-clock benchmarks answer whether a change is faster on one machine. These profiles answer *why*:
how many instructions, simulated cache misses, branch mispredictions, allocations, and requested bytes
the same deterministic operation consumes.

Run the allocation profile natively (three deterministic repeats by default; set `PROFILE_REPEATS` to change it):

```console
$ tools/profile/allocations.sh
```

The allocation binary also accepts deterministic batched-read shapes directly:

```console
$ target/release/deps/allocations-<hash> get-many-256-sorted-hits 100
$ target/release/deps/allocations-<hash> get-many-256-random-mixed 100
```

The shape is `get-many-WIDTH-(sorted|random)-(hits|misses|mixed)`, with widths 1, 16, 256, and 1024
used by the read matrix. Query construction is outside the counted region; the fixture is prepared and
the decoded-node cache is warmed before the operation is measured.

The same binary covers `scan-tombstone` and `scan-stream-tombstone`, which scan a range containing 2,000
persisted tombstones, plus `apply-WIDTH-(repeated|distinct|delete|mixed)` for small, medium, and
commit-sized mutation batches (`mixed` interleaves upserts and tombstones in one batch).
`tools/profile/allocations.sh` runs the complete deterministic set.
It also runs chained `cow-low` and `cow-high` rewrites — commit-sized 256-mutation batches whose keys
spread across the whole keyspace, so each commit flushes and rewrites leaf paths — and reports newly
stored object count, newly stored bytes, commit-to-commit node sharing, and the final root's node
sharing with the original tree, alongside allocator traffic. The mutation batches are prebuilt and the
sharing walks run after counting is disabled, so the counted region is rewrite work only.

Run Cachegrind in the repository's Linux container (Docker or a compatible runtime is required):

```console
$ tools/profile/cachegrind.sh
```

Both commands build dedicated non-Criterion benchmarks. The fixture contains 10,000 sorted keys with
24-byte inline values, is built in 256-mutation evolving commits, and is excluded from each measured
region. Point reads warm their fixture before measurement. `scan` collects the full result;
`scan-stream` consumes 256-row batches and drops each batch before requesting the next.

The allocation executable wraps the system allocator and reports region-scoped allocation calls,
reallocation calls, requested bytes, high-water live bytes above the prepared fixture, and final live
bytes above that fixture. It is a separate binary so the counter atomics cannot affect normal library
code or the Cachegrind workload.

Cachegrind runs three paired samples by default and reports the median after subtracting an identical
setup-only process. Set `PROFILE_REPEATS` to change that count. The reference cache is deliberately
declared as 32 KiB 8-way I1, 32 KiB 8-way D1, and 8 MiB 16-way last level, all with 64-byte lines. It is
a stable comparison model, not a claim about the host CPU's physical cache. Compare cache results only
when `target/profile/metadata.txt` has the same architecture, compiler, model, and source digest.

Generated evidence is under `target/profile/`:

- `cachegrind.tsv` contains per-operation medians;
- `cachegrind-variability.tsv` contains the min/median/max of the decision-driving counters;
- `*.annotate.txt` ranks functions after matched setup subtraction;
- `allocations.txt` contains the native allocator counts; and
- `metadata.txt` records Rust, Valgrind, architecture, cache model, repeats, and a source digest.

Small negative cache-miss deltas can appear when two whole-process profiles are subtracted. They mean
the operation is beneath setup noise for that event; do not interpret them as a physical effect.
Instruction counts are much more stable. Always pair these profiles with the native Criterion timings:
neither a cache simulator nor an allocator counter models storage latency, scheduler contention, or the
actual host's instruction costs.

The Cachegrind command runs the complete read matrix (widths 1, 16, 256, and 1024; sorted and random;
hits, misses, and mixed) with a shape-matched setup process for every row. It also runs bounded scans
returning 2, 32, and 256 rows, streaming equivalents, tombstone-dense `scan-tombstone` and
`scan-stream-tombstone` rows against their own tombstone-fixture setup twin, and apply shapes for
distinct, repeated, delete-heavy, and mixed upsert+tombstone batches, plus true point `get` rows for
cold/hot caches and short/long common prefixes. Cold point-get rows read 16 distinct strided keys (at
most the fixture's leaf count) so every measured leaf load is a genuine miss; hot rows reread one
warmed key. It also runs chained `cow-low` and `cow-high` rewrite shapes: commit-sized 256-mutation
batches, low overlap inserting fresh keys spread across every leaf versus high overlap rewriting
existing keys, with a setup twin that builds the same batches and applies none. They consume each
returned root and use `MemStore`, so adapter-level object-sharing and bytes-written claims still
require the Stratum consumer harness. `cachegrind.tsv` contains one median row per shape;
`cachegrind-variability.tsv` contains the min/median/max across the three deterministic repeats;
`commands.txt` records every profiled invocation and `metadata.txt` records the fixture dimensions.
