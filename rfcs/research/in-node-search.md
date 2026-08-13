# In-node search layouts

Masstree, array-layout studies, ART/HOT, and Rust SIMD practicalities. Evidence on whether the RFC's
key-head design and its vectorization pay off.

## 1. The u64 big-endian head idea is well-validated

**Masstree** (Mao, Kohler, Morris, EuroSys'12,
<https://pdos.csail.mit.edu/papers/masstree:eurosys12.pdf>) is a trie of B+-trees with width 15, where
layer *h* is indexed by key bytes 8h..8h+7. From §4.2, verbatim:

> The keyslice variables store 8-byte key slices as 64-bit integers, byte-swapped if necessary so that
> native less-than comparisons provide the same results as lexicographic string comparison. **This was
> the most valuable of our coding tricks, improving performance by 13–19%.** Short key slices are
> padded with 0 bytes.

That is exactly the RFC's head construction, independently arrived at, with a measured number
attached. Keep it.

Two details Masstree found necessary that the RFC should mirror:

- **Store the key length beside the head.** Border nodes carry slice *plus length plus suffix*,
  because with binary keys `"ABCDEFG\0"` (8 bytes) and `"ABCDEFG"` (7 bytes) share a slice. This is
  the RFC's "equal heads are only a candidate range" problem, and it confirms length must be locally
  available rather than fetched from the blob.
- At most 10 keys can share a slice (lengths 0–8, plus one longer key or a deeper-layer link), and
  Masstree forces all same-slice keys into the same border node.

Concurrency aside, not directly applicable but worth knowing: a 64-bit `permutation` word packs
4-bit `nkeys` + 15×4-bit `keyindex[]`, so an insert writes an unused slot then publishes a new
permutation with one store — readers never see intermediate state and never retry. The RFC's
immutable nodes get this property structurally.

Masstree's cumulative ablation (Fig. 8): +IntCmp 15–24%, 4-tree +41–44%, +Prefetch, +Permuter +4% on
puts, Masstree itself +8%. Note the ordering — the biggest wins are comparison *shape* and *fanout*.
**Masstree does no SIMD in-node search at all** at fanout 15.

## 2. For an L1-resident array, branchless binary search is the thing to beat

**Khuong & Morin, *Array Layouts for Comparison-Based Searching***
(<https://arxiv.org/abs/1509.05053>; data at <http://cglab.ca/~morin/misc/arraylayout-v2/>). Executive
summary, verbatim: "For arrays small enough to be kept in L2 cache, the branch-free binary search code
… is the fastest algorithm." Findings 1–2: below L1, branch-free is "considerably faster than their
branchy counterparts, **sometimes by a factor of two**"; below L2, "a good branch-free implementation
of binary search is **unbeaten by any other strategy**," with Eytzinger branch-free a close second.
Only above L3 does Eytzinger + explicit prefetch win, because there branchy code acts as an implicit
prefetcher. B-tree and van Emde Boas layouts lose at every size.

A 64-entry × 8-byte head array is **512 bytes** — deep L1. The paper's answer for that regime is
branchless binary search on the plain sorted array, not a different layout. They tested one element
per comparison; SIMD appears only as an unbenchmarked footnote ("with AVX2 one can perform 8
comparisons in parallel").

**Algorithmica** (<https://en.algorithmica.org/hpc/data-structures/binary-search/> and `.../s-tree/`)
adds numbers and two critical caveats. Branchless binary search: up to **3× faster than
`std::lower_bound` on small arrays**, 4× overall; each branch mispredict costs 10–15 cycles. The
S-tree (B=16 int32 = one cache line, SIMD compare-all + `movemask` + `ffs`) reports up to 8×, S+ tree
15× — but (a) "the compilers are **not smart enough to auto-vectorize** this code yet, so we have to
optimize it manually," and (b) all headline numbers are **reciprocal throughput**, and "in terms of
real latency, the speedup is not that impressive." The gains came from removing branches and
overlapping *memory requests*. A single L1-resident node has no memory requests to overlap. Tellingly,
their 32-key block variant improved latency but **not** throughput.

## 3. Systems that win with SIMD in-node search narrow the keys first

- **ART** (<https://db.in.tum.de/~leis/papers/ART.pdf>) — Node4 loop, Node16 `_mm_set1_epi8` +
  `_mm_cmpeq_epi8` + `_mm_movemask_epi8 & mask`, Node48 two array lookups, Node256 direct index. SIMD
  is used only at n=16, on **1-byte** keys (16 lanes per register), and ART explicitly falls back to
  binary search where SIMD is unavailable. Also worth borrowing conceptually: *lazy expansion*
  (truncate paths to a single leaf, verify the full key at the leaf) and *path compression*.
- **HOT** (<https://dbis.uibk.ac.at/sites/default/files/2018-06/hot-height-optimized.pdf>) — max
  fanout 32; extracts only *discriminative bits* into sparse partial keys stored contiguously as
  8/16/32-bit arrays (9 physical layouts, smallest chosen per node) "so we can search all keys in
  parallel using SIMD instead of traversing the trie."
- **Bw-tree** — cost is dominated by delta-chain traversal and consolidation, not in-page search.

**Blunt read:** every system that wins with SIMD in-node search compressed keys to **1–4 bytes per
entry** first. Eight-byte heads are the worst case for lane count: AVX2 gives 4 lanes, so 64 heads is
16 registers, 16 compares, 16 mask/blend operations — against a branchless binary search that touches
about 6 elements total. Expect low single-digit percent, possibly a regression. The interesting
variant, if the SIMD step is to pay at all, is HOT's insight applied here: a **narrower head** (u16 or
u32 taken at `skip`) as a first-stage filter, with the u64 head as a second stage.

## 4. The real risk is the equal-head range, not kernel throughput

A single per-node `skip` is strictly weaker than front coding when prefixes are **non-uniform**. If
some keys in a node share 40 bytes and others share 3, `skip` collapses to 3, every head in the
40-byte cluster becomes near-identical, step 2 returns a huge equal-head range, and step 3 degenerates
into a linear full-key scan through the blob region.

Real systems solve this with **group-local prefixes**, not one per node: LevelDB/RocksDB restart
points every 16 keys, Pebble's power-of-two bundles (see `block-formats.md` §1), PlainTable indexing
every 16th row per prefix with the reasoning spelled out — "16 is the maximum number of rows that need
to be checked in the linear search following the binary search. By increasing the number, we would
save memory … but paying more costs for linear search." This maps naturally onto the RFC's descriptor
array: give each aligned group of 8 or 16 entries its own `skip`.

**The RFC should report the distribution of equal-head-range sizes on realistic keys.** If step 3
averages more than ~2 full-key comparisons, optimizing step 2 is optimizing the wrong thing.

## 5. Where the time actually goes

For a 512-byte L1-resident head array there is no memory-latency component to hide. Costs are branch
mispredicts (10–15 cycles × ~6 for branchy binary search) and instruction throughput. Rough budget:

- branchless binary search over 64 heads ≈ 6 dependent-but-predicted iterations ≈ 6 × ~4-cycle L1 load
  ≈ **25–30 cycles**;
- AVX2 scan of 64 u64 heads ≈ 16 loads + 16 compares + mask combining ≈ **20–40 uops**.

Same order of magnitude, with more code and a dispatch table. Meanwhile step 1 (prefix memcmp) and
step 3 (full-key comparisons chasing into the blob region) touch *different cache lines* and are the
plausible latency source — and for a content-addressed tree, the store lookup between levels likely
dwarfs everything on this list.

## 6. Recommended experiment order

By expected value over effort:

1. **Branchless binary search** over the heads (~2–3× vs branchy, no `unsafe`, no dispatch).
2. **Express the lower bound as a count**: `heads.iter().filter(|&&h| h < probe).count()` over a
   fixed-length `[u64; N]`. LLVM vectorizes this into compare + mask + popcount, and it yields the
   lower bound directly. It will *not* vectorize a `position()`-shaped early-exit loop — that
   asymmetry is the whole trick. **This is the highest-value experiment in the RFC**: plausibly most
   of the SIMD win with zero intrinsics and zero dispatch.
3. **Per-group `skip`** (restart-point style) to bound the equal-head range — likely a bigger win than
   any kernel, per §4.
4. **Hand-written AVX2/NEON kernels last**, gated on 1–3 being measured first.

The RFC's scalar reference + property-tested equivalence + runtime dispatch scaffolding is sound
engineering regardless. The change to make is treating the SIMD kernel as an **outcome of the
benchmark matrix** — which §"Format freeze" already gestures at — rather than a design commitment in
the title.

## 7. Rust specifics

- **`std::simd` is still nightly-only** (`portable_simd`, rust-lang/rust#86656). Docs warn operations
  "do not necessarily map to a single instruction" and that consistency is never traded for speed. A
  stable crate needs `core::arch` intrinsics behind `#[target_feature]`, or `wide`/`safe_arch`.
- **`multiversion` 0.8** (<https://docs.rs/multiversion>) generates cached dispatch via
  `#[multiversion(targets=...)]` and composes with `std::simd` in the same function body — the
  low-friction option.
- **`is_x86_feature_detected!`** is `cpuid`-backed but caches in a process-global atomic after first
  call, so steady state is a relaxed load + bit test (~1–2 ns). Negligible per *query*, not negligible
  per *node* in a hot descent loop: detect once at construction and store a function pointer or enum
  on the tree handle.
- **NEON is baseline** in the aarch64 target spec, so no detection at all is needed on Apple Silicon.
- **AVX-512 in 2026**: still not worth being the primary path — absent from every Intel consumer part
  since Alder Lake, present on Zen 4 (double-pumped) / Zen 5 and servers, with AVX10.1/10.2 as the
  forward path and only now appearing. Target AVX2 + NEON; treat AVX-512 as an optional third kernel.
  It is the one place that would genuinely shine here, since 64 u64 heads fit in 8 registers.
- **Autovectorization of "first index where u64 >= x": generally no.** It is a reduction with early
  exit, and LLVM will not vectorize an early-exit loop over `slice::iter().position()`. Hence the
  count-based reformulation in §6.2.
