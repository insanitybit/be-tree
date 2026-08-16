# Changelog

All notable changes to be-tree. The crate is pre-1.0: minor versions may break, but every break is
recorded here so a consumer's changelog is not the compiler.

## Unreleased

### Breaking

- **Crate renamed: `be-tree` → `cbe-tree`** (display name **cbε-tree**, content-addressed Bε-tree).
  Two reasons: the name now says what distinguishes this tree — content addressing — from every other
  Bε-tree, and `be-tree` was already taken on crates.io by an unrelated 2020 library, so the old name
  was unpublishable. The import path changes from `be_tree::` to `cbe_tree::`. A consumer can keep its
  existing paths with one dependency line:
  `be-tree = { package = "cbe-tree", path = "../cbe-tree" }`.

### Fixed

- Work-budget visit accounting is again charged once per external-value **reference** on every read
  path. The scratch-pooling optimization series had left scalar `get` double-charging (once in
  `load_values`, again in the deduplicated loader) while batched `get_many` charged only per unique
  value object. A workload sized exactly at `WorkBudget::max_objects` could fail one visit early via
  `get`, and duplicate-key batches sharing one value were undercharged. Scalar and batched reads now
  price identical work identically (pinned by `visit_budget_prices_scalar_and_batched_reads_identically`).
- Pooled read scratches no longer retain decoded nodes or value payloads between operations. Scratch
  vectors are cleared (capacity kept) before returning to their pools, so an idle tree does not pin a
  wave of `Arc<NodeView>`s outside cache accounting.

### Measurement harness (no library behavior change)

- The COW profile shapes (`cow-low`/`cow-high`) now apply commit-sized 256-mutation batches with keys
  spread across the keyspace, so low and high overlap are structurally different workloads; metrics
  collection moved outside the measured region and the Cachegrind setup twin is shape-matched.
- Cold point-get shapes read distinct strided keys instead of rereading one (cache-warmed) key.
- New shapes: `apply-WIDTH-mixed` (interleaved upserts and tombstones in one batch) and tombstone-dense
  scan rows in the Cachegrind matrix.

## 0.1.0 (2026-08-13)

Breaking changes relative to the API integrated by the downstream consumer at eval #2. Each was individually deliberate;
they are recorded here retroactively because they previously shipped undeclared and were discovered
downstream as a red build.

### Breaking

- **`be_tree::Tree` trait removed.** `BeTree` exposes inherent methods instead. A trait with one
  implementor was indirection with no seam; call `BeTree::get/apply/...` directly (or your own
  adapter trait) rather than importing `Tree`.
- **`be_tree::harness` module removed from the library.** The fixture/oracle code moved to
  `tests/support/mod.rs` and is no longer reachable downstream. Consumers that used
  `harness::check_balanced`/`check` as a degeneration oracle should use [`BeTree::references`] /
  [`BeTree::references_many`] — the supported graph-walk entry points — to derive node counts and
  leaf depths (a level-at-a-time walk filtered to `ObjectKind::Node`).
- **`with_metrics(Arc<Metrics>)` replaced by `record_metrics()`**, with readout via
  `metrics() -> MetricsSnapshot` (one owned snapshot type, no `Arc` plumbing).
- **`with_format` takes `Format` by value** instead of `Arc<Format>`.

### Added

- `benches/profile.rs`, `benches/allocations.rs`, and `tools/profile/` (containerized Cachegrind and
  allocation harnesses with a summarizer).
- Module split: `cache.rs`, `inflight.rs`, `metrics.rs` extracted from `tree.rs`; curated root
  re-export façade with internals behind `#[doc(hidden)]`.

### Removed

- The `explicit-simd` feature and `benches/hashing.rs` — added, measured, rejected, and deleted (see
  the recorded reversals section).
