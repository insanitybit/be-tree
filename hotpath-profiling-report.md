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

## Suggested next steps

1. Chase the 81 KB/batch in `resolve_many` — per-batch scratch reuse; verify with the
   same `get-many 512` run (the number should drop deterministically).
2. Give `encode_leaf`/`encode_internal` a reusable output buffer — ~28% of write-path bytes.
3. Look at `wave`'s 8.9 KB/call on all-hit waves — map/dedup scratch.
4. Optionally run `--features hotpath,hotpath-cpu` (after `cargo install samply` and
   granting profiling permissions) to attribute CPU samples inside `resolve_many`.
