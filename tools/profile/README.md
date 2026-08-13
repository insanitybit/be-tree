# Reproducible performance profiles

Wall-clock benchmarks answer whether a change is faster on one machine. These profiles answer *why*:
how many instructions, simulated cache misses, branch mispredictions, allocations, and requested bytes
the same deterministic operation consumes.

Run the allocation profile natively:

```console
$ tools/profile/allocations.sh
```

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

Small negative last-level miss deltas can appear when two whole-process profiles are subtracted. They
mean the operation is beneath setup noise for that event; do not interpret them as a physical effect.
Instruction counts are much more stable. Always pair these profiles with the native Criterion timings:
neither a cache simulator nor an allocator counter models storage latency, scheduler contention, or the
actual host's instruction costs.
