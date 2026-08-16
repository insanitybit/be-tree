# hotpath profiling report for be-tree

_2026-08-13 · branch `hotpath-profiling` · hotpath 0.23.2 · approach from
[Profiling Rust: The Complete Guide](https://hotpath.rs/blog/profiling-rust-guide)_

## Setup

The guide's feature-gated pattern is wired in with zero cost when disabled:

- `Cargo.toml` adds an optional `hotpath = "0.23"` dependency and three features:
  `hotpath` (timing), `hotpath-alloc` (allocation tracking), `hotpath-cpu` (samply-based
  CPU sampling, requires the `samply` binary; not exercised in this report).
- 14 hot functions carry `#[cfg_attr(feature = "hotpath", hotpath::measure)]`:
  - `src/tree.rs`: `apply`, `get_many`, `apply_prepared`, `load`, `load_coalesced`,
    `decode_verified`, `wave`, `normalize`, `pack_leaves`, `resolve_many`, `materialize`
  - `src/codec.rs`: `encode_leaf`, `encode_internal`, `NodeView::decode`
  - `partition` was deliberately skipped: it returns a boxed future, so `measure`
    would time only future construction, not execution.
- `benches/profile.rs` `main` carries `#[cfg_attr(feature = "hotpath", hotpath::main)]`.
  The deterministic profile binary was already designed for external profilers
  (fixed work, no warmup or sampling loop), which makes its runs directly comparable
  across builds — and makes the alloc numbers below exactly reproducible.

### How to run

```bash
# Build (cargo bench appends a `--bench` arg the binary rejects, so run the
# binary directly; --no-run prints its path)
cargo bench --bench profile --features hotpath,hotpath-alloc --no-run

./target/release/deps/profile-<hash> get-many 512
./target/release/deps/profile-<hash> apply 64
```

Both `cargo check --benches` and `cargo check --benches --features hotpath` pass;
with the features off every macro is a no-op and hotpath is not compiled.

## Findings

## Update (scratch-buffer pass, 2026-08-13)

I replaced the get-many descent allocation path with a reusable scratch-path and in-place winner materialization.

### Read path (`get-many 512`, 256 keys per batch)

| Function | Calls | Avg | Total | % of run | Alloc Total |
|---|---|---|---|---|---|
| `tree::get_many` | 513 | 59.64 µs | 30.60 ms | 63.27% | 6.1 MB |
| `tree::wave` | 1026 | 3.41 µs | 3.50 ms | 7.24% | 9.0 MB |

### Exclusive allocations (`get-many 512`)

| Function | Calls | Avg | P95 | Total | % |
|---|---|---|---|---|---|
| `tree::get_many` | 513 | 12.2 KB | 12.1 KB | 6.1 MB | 15.77% |
| `tree::wave` | 1026 | 8.9 KB | 11.0 KB | 9.0 MB | 23.14% |
| `tree::apply_prepared` | 40 | 255.6 KB | 634.5 KB | 10.0 MB | 25.77% |

The old `resolve_many` allocation spike is no longer exposed as its own hotpath bucket; the
dominant read allocation moved to `tree::get_many`, now down to 6.1 MB total for the same
512-iteration read workload (down from 40.6 MB in the earlier measurement).

## Update (frontier-sorted fast-path pass, 2026-08-13)

I kept wave pooling and switched wave lookups to binary search on sorted wave entries, while avoiding
frontier sorting work on already-sorted traversals.

### Read path (`get-many 512`, 256 keys per batch)

| Function | Calls | Avg | Total | % of run |
|---|---|---|---|---|
| `tree::get_many` | 513 | 32.30 µs | 16.57 ms | 62.60% |
| `tree::resolve_many` | 513 | 29.64 µs | 15.21 ms | 57.44% |
| `tree::wave` | 1026 | 2.35 µs | 2.42 ms | 9.13% |

### Native allocation (`get-many 100`, `scan`, `scan-stream`, `apply`, `hash`, `decode`)

| Scenario | Allocations | Reallocations | Allocated bytes | Peak live bytes | Live bytes |
|---|---|---|---|---|---|
| get-many | 560 | 200 | 1,405,064 | 16,456 | 72 |
| scan | 816 | 492 | 16,106,482 | 1,276,373 | 1000 |
| scan-stream | 814 | 480 | 11,944,946 | 227,798 | 1000 |
| apply | 6,319 | 400 | 9,052,394 | 6,759,154 | 6,744,520 |
| hash | 0 | 0 | 0 | 0 | 0 |
| decode | 2,001 | 0 | 176,024 | 200 | 24 |

Compared with the prior scratch pass, this pass does not change allocation totals materially on this deterministic
read workload, but it reduced read-side `resolve_many` timing and made it more stable when frontier IDs are
already sorted.

## Update (materialize scratch pass, 2026-08-13)

I pooled the per-read `materialize` scratch allocations (`refs`, `slots`) and routed `get_many` through it.

### Read path (`get-many 512`, 256 keys per batch)

| Function | Calls | Avg | Total | % of run |
|---|---|---|---|---|
| `tree::get_many` | 513 | 32.16 µs | 16.50 ms | 66.38% |
| `tree::resolve_many` | 513 | 29.49 µs | 15.13 ms | 60.87% |
| `tree::wave` | 1026 | 2.35 µs | 2.42 ms | 9.77% |

### Native allocation (`get-many 100`, with profiling allocator)

| Scenario | Allocations | Reallocations | Allocated bytes | Peak live bytes | Live bytes |
|---|---|---|---|---|---|
| get-many | 460 | 200 | 995,464 | 12,360 | 72 |
| scan | 816 | 492 | 16,106,482 | 1,276,373 | 1000 |
| scan-stream | 814 | 480 | 11,944,946 | 227,798 | 1000 |
| apply | 6,319 | 400 | 9,052,398 | 6,759,154 | 6,744,520 |
| hash | 0 | 0 | 0 | 0 | 0 |
| decode | 2,001 | 0 | 176,024 | 200 | 24 |

This pass materially reduced `get-many` allocations (`560` -> `460` calls, `1,405,064` -> `995,464` bytes) while
keeping the same deterministic workload shape. The measured timing remained near prior baselines in this run.

## Update (wave-entry capacity recycling, 2026-08-13)

`wave_cached` returns its sorted entries to the caller for binary-search lookup. Previously the entries
vector was moved into `Wave` and an empty `WaveScratch` was immediately pooled, so every subsequent
wave lost the vector capacity. The sorted-read caller now returns that capacity to the wave scratch pool
after processing each dependent level.

The deterministic 10,000-key, 24-byte inline-value fixture was run for 100 warm iterations per shape:

| Shape | Allocations before | Allocations after | Reallocations before | Reallocations after | Bytes before | Bytes after |
|---|---:|---:|---:|---:|---:|---:|
| 256 sorted hits | 322 | 122 | 0 | 0 | 860,456 | 828,456 |
| 256 random hits | 460 | 260 | 200 | 0 | 995,464 | 867,464 |
| 256 sorted misses | 322 | 122 | 0 | 0 | 860,456 | 828,456 |
| 256 random mixed | 460 | 260 | 200 | 0 | 995,464 | 867,464 |

The expanded harness also covers 1, 16, 256, and 1024 keys, both orderings, and hit/miss/mixed outcomes.
Checksums are consumed for every operation. These are allocator and compute-fixture measurements, not
storage-throughput claims; Cachegrind is unavailable natively on this macOS host — the Docker
container harness introduced below now provides it.

## Update (write-matrix refresh and rejected flush experiment, 2026-08-13)

The expanded native allocation harness was rerun after reverting the contiguous-range flush
experiment. Current results for ten iterations are:

| Shape | Allocations | Reallocations | Allocated bytes | Peak live bytes |
|---|---:|---:|---:|---:|
| `apply-1-distinct` | 632 | 40 | 797,451 | 681,390 |
| `apply-256-distinct` | 1,634 | 1,957 | 25,827,089 | 2,551,286 |
| `apply-256-delete` | 1,561 | 1,853 | 24,854,146 | 2,427,990 |

The contiguous-range flush candidate reduced allocation counts and bytes for medium and large
updates, but it was rejected by matched Criterion timing: the 256-key evolving-chain benchmark
was about 8.87 ms at the baseline and 14.78 ms with the candidate. The candidate is not retained;
allocation reduction alone does not satisfy the whole-operation gate.

A `pack_leaves` capacity-estimation candidate likewise reduced `apply-256-distinct` allocation
traffic from 25,827,089 to 22,128,385 bytes and reallocations from 1,957 to 1,750, but increased
the matched `evolving_chain_32/256` median from 6.586 ms to 6.940 ms (+5.4%). It is reverted.

## Update (container Cachegrind baseline and rejected `step1` experiment, 2026-08-13)

Docker is available on this arm64 host, so the repository Cachegrind harness was run in its Linux
container with three deterministic samples and the declared reference model (`I1=32 KiB`, `D1=32
KiB`, `LL=8 MiB`, all 8-way/16-way as configured). The current retained implementation measured:

| Workload | Instructions/op | D1 misses/op | Branch mispredicts/op |
|---|---:|---:|---:|
| `get-many` (256 keys, 100 iterations) | 538,472 | 3,723 | 4,225 |
| `scan` (2 iterations) | 8,204,507 | 107,877 | 5,948 |
| `scan-stream` (2 iterations) | 8,113,536 | 88,996 | 6,649 |
| `apply` (25 iterations) | 950,980 | 3,647 | 499 |

Cachegrind attributes warm batched-read work primarily to `Surface::probe` (26.8% of instructions,
70.2% of branch mispredicts) and `step1` (23.9% of instructions). A narrow `step1` slice-comparison
candidate passed the search correctness matrix, but regressed the selected-search matrix for uniform
random keys by 36.4% at 256 entries and 23.9% at 2,048 entries; it is reverted. A branchless full-key
lower-bound candidate improved uniform-random search, but regressed long-shared-prefix search by 92.2%
at 256 entries and 65.0% at 2,048 entries; it is also reverted. Neither candidate satisfies the
required mixed-shape gate.

The measurements are simulator counters from the arm64 Linux container, not physical Apple cache
counters. Raw outputs and metadata are under `target/profile/` and remain outside source control.

The completed shape matrix produced these representative medians (all rows have three repeats and
shape-matched setup subtraction). *Superseded: these rows were captured from an intermediate
uncommitted working tree; the committed-revision re-measurement in the 2026-08-14 update below is
authoritative.*

| Shape | Instructions/op | D1 misses/op | Branch mispredicts/op |
|---|---:|---:|---:|
| `get-many-1-sorted-hits` | 5,867 | 26 | 15 |
| `get-many-16-sorted-hits` | 33,869 | 24 | 214 |
| `get-many-256-sorted-hits` | 459,270 | 1,380 | 3,012 |
| `get-many-1024-sorted-hits` | 1,949,562 | 13,483 | 11,996 |
| `get-many-256-random-mixed` | 381,980 | 2,749 | 3,184 |
| `scan-2` | 54,888 | 538 | 305 |
| `scan-32` | 86,192 | 729 | 515 |
| `scan-256` | 256,934 | 2,442 | 217 |
| `scan-stream-256` | 265,650 | 2,432 | 312 |
| `apply-256-distinct` | 5,961,167 | 76,035 | 8,865 |
| `apply-1024-distinct` | 17,405,279 | 251,518 | 30,175 |
| `apply-256-delete` | 5,733,371 | 72,238 | 9,045 |

True point `get` Cachegrind rows, using the same fixture size and 100 measured calls, were
(*superseded — see the 2026-08-14 update: these "cold" rows reread one key, so calls 2–100 were
cache hits and cold ≈ hot below*):

| Shape | Instructions/op | D1 misses/op | Branch mispredicts/op |
|---|---:|---:|---:|
| `get-cold-short` | 5,452 | 10 | 14 |
| `get-hot-short` | 5,522 | 10 | 19 |
| `get-cold-long` | 6,438 | 4 | 14 |
| `get-hot-long` | 6,672 | 43 | 13 |

These rows are after the retained direct scalar `get` path. Compared with the prior `get_many`-based
point path, native Criterion improved uniform hit/miss mean estimates by 2.2%/15.1% and long-prefix
hit/miss mean estimates by 2.6%/16.4% (median estimates: 1.6%/14.3% and 2.2%/15.4%); all four 95%
confidence intervals exclude zero on the matched workload fixture.

The full 24-row read matrix and all scan/apply rows are in `target/profile/cachegrind.tsv`; negative
last-level deltas are below setup-subtraction noise and are not interpreted as physical cache effects.

## Update (COW matrix and rejected flush candidates, 2026-08-13)

The external profiling harness now includes chained content-addressed rewrite shapes for low overlap
(`cow-low`, new keys) and high overlap (`cow-high`, existing keys). Both shapes consume and chain every
returned root; shape-matched setup subtraction is registered in `summarize.py`. The current profile
fixture uses `MemStore`, so these rows validate rewrite work and root chaining, but do not claim Stratum
store throughput or report adapter-level object sharing. Those consumer measurements remain open.

Two ordinary-buffer flush candidates were measured and rejected. Sorted contiguous-range extraction
reduced `apply-256-distinct` from 1,634 to 1,216 allocations, reallocations from 1,957 to 313, and
allocated bytes from 25.83 MiB to 15.37 MiB; `apply-256-delete` showed the same direction (1,561 to
1,175 allocations and 24.85 MiB to 14.59 MiB). The complete-operation gate failed: evolving-chain
width-1 latency regressed about 14%, while width-256 showed no statistically significant change.
The buffer-reuse grouping variant likewise retained substantially more allocation traffic than the
range candidate and was not retained. The production path remains the original deterministic grouping
and heaviest-child policy.

## Update (consumer integration gate, 2026-08-13)

The real Stratum consumer was validated against the current path dependency with:

```text
CARGO_TARGET_DIR=/tmp/stratum-be-tree-integration \
  env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= \
  cargo test -p stratum-l1 -p stratum-l2
```

The command passed compilation, the L1 adapter tests, and the L2 tests. The exercised consumer gate
includes `tree_put_is_cow_new_root` (old-root immutability), `one_tree_put_stages_many_nodes_into_few_puts`
(one addressed batch for a multi-node rewrite), split/read-back and tombstone cases, deterministic
encoding, faulted confluence, and GC reachability. The L1 node-size study also records be-tree metrics
and cold point-read pack misses against the simulated block fabric. This closes the previously open
consumer correctness gate; the profile's `MemStore` COW rows remain compute/allocation evidence, not
remote-store latency evidence.

The native COW allocation rows now report storage and sharing counters as well as allocator traffic.
*Superseded: these single-key rewrites were absorbed by the root buffer, so cow-low and cow-high were
structurally identical one-node rewrites and could not measure the overlap axis. The 2026-08-14
update below replaces them with commit-sized batches.* Across three repeats, 25 chained single-key
rewrites on the 10,000-key fixture had the following medians:

| Shape | New objects | New bytes | Root-node sharing | Allocations | Allocated bytes |
|---|---:|---:|---:|---:|---:|
| `cow-low` | 25 | 1,638,400 | 0.941176 | 2,286 | 10.74 MiB |
| `cow-high` | 25 | 1,638,400 | 0.941176 | 2,245 | 10.75 MiB |

The current three-repeat Cachegrind rows are `1,094,553` instructions / `4,005` D1 misses /
`748` branch mispredicts for low overlap and `1,094,389` / `4,044` / `736` for high overlap.
These are intentionally small single-key rewrites; the Stratum tests cover the larger one-commit
staged batch and simulated block fabric.

## Update (single-key descent fast path, 2026-08-13)

The grouped frontier is now bypassed for a one-key `get_many`. Scalar descent still uses the same
validated `load`, head-surface probe, child-level check, work budget, and winner resolution rules, but
does not construct frontier or wave-result state that cannot be shared by another query.

With the same prepared 10,000-key fixture and 100 iterations, hotpath reported:

| Shape | `get_many` average | `resolve_many_cached` average |
|---|---:|---:|
| 1 sorted hit | 4.16 µs | 2.04 µs |
| 256 sorted hits | 24.35 µs | 20.84 µs |

These are instrumented native timings including the warmed fixture process; they are not a cross-build
before/after claim. The allocation matrix remains the decision evidence for the wave-capacity change.

### Read path (`get-many 512`, 256 keys per batch)

Timing (fixture setup — the `apply` rows — excluded from interpretation):

| Function | Calls | Avg | Total | % of run |
|---|---|---|---|---|
| `tree::get_many` | 513 | 34.97 µs | 17.94 ms | 65.1% |
| `tree::resolve_many` | 513 | 32.72 µs | 16.78 ms | 60.9% |
| `tree::wave` | 1026 | 1.74 µs | 1.78 ms | 6.5% |
| `tree::materialize` | 513 | 833 ns | 427 µs | 1.6% |
| `tree::load` | 52 | 777 ns | 40 µs | 0.2% |

**Read cost is compute, not loading.** Of `resolve_many`'s ~33 µs per 256-key batch,
node loading (`wave`, two calls per batch on this warm two-level fixture) accounts for
under 4 µs. The remaining ~29 µs is probe/merge work over already-cached nodes.
Optimizing fetch or cache lookup further would not move read latency; the probe loop would.

Exclusive allocation bytes (total 75.3 MB for the run):

| Function | Calls | Avg | P95 | Total | % |
|---|---|---|---|---|---|
| `tree::resolve_many` | 513 | **81.0 KB** | 81.0 KB | 40.6 MB | 53.9% |
| `tree::apply_prepared` | 40 | 255.5 KB | 634.5 KB | 10.0 MB | 13.3% |
| `tree::wave` | 1026 | 8.9 KB | 11.0 KB | 9.0 MB | 11.9% |
| `tree::materialize` | 513 | 4.0 KB | 4.0 KB | 2.0 MB | 2.7% |
| `tree::load` | 52 | 0 B | 0 B | 0 B | 0.0% |

**Headline: `resolve_many` allocates exactly 81.0 KB per 256-key batch (~324 B/key),
54% of all bytes allocated in the read workload.** P95 equals the average — the
allocation pattern is perfectly deterministic, so a fix is verifiable to the byte.
Likely sources worth checking: the per-key `best: Vec<Option<Winner>>`, per-node probe
grouping maps, and any per-key `Bytes`/key materialization in the descent. A reusable
scratch arena or flattened per-batch buffers are the obvious shapes.

Second tier: `wave` at 8.9 KB per call even when everything is cache-hit suggests the
wave result map (`foldhash::HashMap<BlockId, Arc<NodeView>>`) and id-dedup scratch are
allocated per wave regardless of misses.

### Write path (`apply 64`, total 29.3 MB allocated)

| Function | Calls | Avg | Total | % |
|---|---|---|---|---|
| `tree::apply_prepared` | 104 | 112.9 KB | 11.5 MB | 39.2% |
| `codec::encode_internal` | 102 | **64.0 KB** | 6.4 MB | 21.8% |
| `tree::pack_leaves` | 15 | 295.8 KB | 4.3 MB | 14.8% |
| `tree::normalize` | 104 | 35.0 KB | 3.6 MB | 12.1% |
| `codec::encode_leaf` | 31 | **64.0 KB** | 1.9 MB | 6.6% |

**Both encoders allocate a uniform 64 KB per call** — exactly `node_bytes` for this
fixture. Each encode materializes a full node-sized buffer. Since encodes happen on
every rewrite along the flush path, an encoder that reuses a scratch buffer (or writes
into a pooled `BytesMut`) would cut write-path allocation by ~28% in this workload.

Timing on the write side: `apply` ≈ 183–188 µs per batch, with `apply_prepared`
(rewrite + flush) at ~94% of it and `normalize` ~6%.

## Update (review-fix pass and committed-revision re-measurement, 2026-08-14)

A review of the measured plan found three harness defects and one library defect; all are fixed in
commit `2fc2330` and the full matrix below was re-captured from that committed revision. The
container digest `5a2be23b…` reproduces from the clean checkout with
`LC_ALL=C find src benches Cargo.toml Cargo.lock -type f -print0 | sort -z | xargs -0 sha256sum | sha256sum`,
so every row in `target/profile/` is now attributable to one commit. `metadata.txt` additionally
records the fixture dimensions and `commands.txt` records every profiled invocation.

The fixes that change what the numbers mean:

- **Budget parity (library).** Work-budget visits are charged once per external-value reference on
  every read path. Scalar `get` had been double-charging and batched `get_many` undercharging
  (per unique value object). Pinned by a scalar/batched parity test; the Stratum consumer gate
  (stratum-l1 + stratum-l2, 135 tests) passes against the fixed revision.
- **COW shapes are commit-sized.** `cow-low`/`cow-high` now apply 25 chained 256-mutation prebuilt
  batches with keys spread across the whole keyspace. The previous single-key commits were absorbed
  by the root buffer, making both shapes identical one-node rewrites. Metrics walks now run outside
  the measured region and the setup twin builds the same batches and applies none.
- **Cold point-get is cold.** Cold rows read 16 distinct strided keys (at most the fixture's leaf
  count) instead of rereading one warmed key 100 times. Cold and hot rows are now distinct, as the
  measurement protocol requires; cold rows are not comparable to the superseded table above.
- **New required shapes.** Tombstone-dense scans and the mixed upsert+tombstone apply batch joined
  the Cachegrind matrix.

Representative medians (three repeats, shape-matched setup subtraction; full 24-row read matrix in
`target/profile/cachegrind.tsv`):

| Shape | Instructions/op | D1 misses/op | Branch mispredicts/op |
|---|---:|---:|---:|
| `get-many-1-sorted-hits` | 5,963 | 20 | 9 |
| `get-many-16-sorted-hits` | 33,406 | 29 | 214 |
| `get-many-256-sorted-hits` | 457,584 | 1,388 | 3,259 |
| `get-many-1024-sorted-hits` | 1,942,924 | 13,515 | 13,007 |
| `get-many-256-random-mixed` | 380,637 | 2,743 | 3,326 |
| `get-many-256-sorted-misses` | 172,664 | 981 | 803 |
| `scan-2` | 62,166 | -543 | 346 |
| `scan-32` | 78,072 | -220 | 589 |
| `scan-256` | 270,239 | 1,995 | 401 |
| `scan-stream-256` | 274,792 | 2,899 | 623 |
| `scan-tombstone` | 2,693,439 | 21,809 | 4,045 |
| `scan-stream-tombstone` | 2,684,805 | 21,971 | 3,911 |
| `apply-256-distinct` | 5,985,172 | 76,436 | 8,775 |
| `apply-256-delete` | 5,742,465 | 72,457 | 8,906 |
| `apply-256-mixed` | 5,727,342 | 73,089 | 10,665 |
| `apply-1024-distinct` | 17,475,814 | 251,715 | 30,212 |

Point `get`, with genuinely cold leaf loads (16 distinct strided keys) versus a warmed reread:

| Shape | Instructions/op | D1 misses/op | Branch mispredicts/op |
|---|---:|---:|---:|
| `get-cold-short` | 4,853 | 7 | 31 |
| `get-cold-long` | 6,653 | 194 | 59 |
| `get-hot-short` | 5,410 | 10 | 12 |
| `get-hot-long` | 6,258 | 3 | 11 |

COW rewrite shapes, per 256-mutation commit: `cow-low` 7,972,614 instructions / 79,925 D1 misses /
11,139 branch mispredicts; `cow-high` 6,167,056 / 78,324 / 9,221. The overlap axis now
discriminates. Native storage counters across 25 commits (byte-identical over three repeats):

| Shape | New objects | New bytes | Successive sharing | Final sharing with original | Allocations | Allocated bytes |
|---|---:|---:|---:|---:|---:|---:|
| `cow-low` | 148 | 9,699,328 | 0.876974 | 0.000000 | 6,208 | 66,089,950 |
| `cow-high` | 91 | 5,963,776 | 0.785882 | 0.000000 | 4,151 | 67,518,688 |

At 25 commit-sized batches over this 17-node fixture every original node is eventually rewritten, so
final sharing with the original root saturates at zero for both shapes; the discriminating metrics at
this scale are new objects, new bytes, and commit-to-commit sharing. These remain `MemStore`
compute/allocation rows, not adapter-level storage claims.

Read-path continuity check against the superseded intermediate capture: `get-many-256-sorted-hits`
moved 457,707 → 457,169 instructions/op and `apply-256-distinct` 5,975,792 → 5,982,400 (allocation
totals unchanged: 122 allocations / 828,456 bytes per 100 iterations, 25.8 MB per 10 applies) — the
budget-parity and scratch-clearing fixes are visit-accounting and retention changes, not hot-loop
changes, and the matrix confirms no read or write regression beyond run-to-run noise.

## What transfers from the guide, what doesn't

- **Applies:** function timing, allocation attribution (especially valuable here
  because the profile workloads are deterministic — the guide's point about
  byte-exact reproducibility holds exactly), and CPU sampling as a next step.
- **Doesn't apply:** SQL, HTTP, and I/O stream tracing (no such layers in be-tree);
  lock instrumentation (no contended `Mutex`/`RwLock` on the hot path — concurrency
  goes through `moka` and `event-listener`).
- **Complements existing tooling:** `benches/allocations.rs` gives exact region-scoped
  totals; hotpath attributes bytes and time *per function* with call counts.
  Criterion remains the statistical harness; hotpath answers "where inside".

## Historical follow-ups (superseded by the 2026-08-14 capture)

The earlier `get-many 512` and single-key COW captures above are retained as provenance, not as
current decision evidence. Their suggested scratch, encoder, and wave changes were not accepted
without a matched whole-operation result. The current committed-revision matrix and allocation
rows are the decision evidence; any further optimization must start with a new frozen baseline.

CPU sampling remains optional and environment-dependent (`samply`/permissions were not part of this
closeout). The profiles do not validate remote-store latency, request pricing, or concurrency scaling.
