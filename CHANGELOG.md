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

### Added

- **Retention floor restored: `with_retention_floor(VersionStamp)`.** The original
  `with_floor(store, Hlc)` was deleted undeclared in `e32f036` (see the retroactive Removed section
  under 0.1.0) and is reinstated in the tree's own vocabulary — an opaque stamp horizon, no clock
  interpretation. The job changed with the data model: merge now keeps exactly one winner per key, so
  per-key *history* no longer grows and needs no floor; what still grows without bound is
  **tombstones**, which must otherwise persist forever to shadow late-arriving lower-stamped writes.
  With a floor set, a tombstone whose stamp orders below it is physically purged at leaf rewrite.
  Live winners are never dropped regardless of stamp — a cold key whose only version predates the
  floor keeps resolving (this is the property the original implementation's comment guarded, now
  pinned by a test that was verified to fail against the naive `retain(stamp >= floor)`). Caller
  contract: no future batch may carry a stamp below the floor, or a purged tombstone can no longer
  defeat it (resurrection). `VersionStamp::ZERO` (default) keeps everything.

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

### Removed — undeclared at the time, recorded retroactively (2026-08-16 audit)

The following were removed in the `e32f036` performance refactor with no CHANGELOG entry and no test
to fail — the silent class: an undeclared removal is invisible in proportion to how untested the
feature was. Recorded now so the next audit has a baseline.

- **`with_floor(store, Hlc)` / `compact_below_floor`** — the per-key retention floor. Restored in
  Unreleased as `with_retention_floor(VersionStamp)`, with the test it never had.
- **Multi-version per-key history ("full time-travel headroom").** Leaves used to retain every
  version newest-first, resolved LWW-at-read; merge now collapses to exactly one winner per key at
  every rewrite. No as-of read ever existed publicly, so no observable read changed, but the
  documented headroom is gone by design. Deliberate and permanent.
- **`Hlc::join`** — the wall-clock-free confluent successor of two clocks ("a merge is a pure
  function of its parent commits"). No equivalent exists; `HlcClock::observe` + `next` requires a
  wall reading. If deterministic merge stamps are needed, that is a new design conversation, not a
  code restoration.
- **`NodeStore::put` and `StagedNode`** — the trait was redesigned around pre-addressed batches
  (`AddressedObject`, `put_batch` returning `()`, byte-bounded `get`/`get_many`). Every external
  `NodeStore` implementor breaks; the new contract is stronger (the store verifies the caller's
  hash and enforces read bounds).
- **Serde derives** on `BlockId`/`Hlc`/message types, and the serde dependency.
- **Per-message version stamps** — `tree_put` took per-message HLCs; `apply` stamps the whole batch.
  Re-injecting messages at heterogeneous historical stamps now costs one `apply` per stamp.
- **`TreeError::Schema` and `TreeError::Codec` variants** — subsumed by `Capacity`, structured
  `Decode { id, reason: DecodeError }`, and `VersionDomainMismatch`. Downstream exhaustive matches
  break.
