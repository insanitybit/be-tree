# Content-addressed and Merkle trees

Prolly trees, Merkle Search Trees, IPLD, git, and BLAKE3 verified streaming. Bears on diff locality,
whether fixed `NODE_BYTES` is right, and verify-on-read.

## 1. Structure is order-dependent, so be-tree is not confluent

**Prolly trees** (<https://www.dolthub.com/blog/2022-06-27-prolly-chunker/>,
<https://docs.dolthub.com/architecture/storage-engine/prolly-tree>) are B+-trees whose node
boundaries are chosen by a chunking function over the sorted key/value stream, making them
**history-independent**: structure is a deterministic function of content, not insertion order. Built
bottom-up — level 0 chunked into leaves, boundary keys + chunk addresses become the level-1 stream,
recursively. Dolt states the cost model explicitly: read `log_k(n)`; **write `(1 + k/w) · log_k(n)`**
(k = avg block size, w = rolling-hash window — the write penalty is the price of content-defined
chunking); ordered scan `z/k`; **diff `d`**, the size of the difference rather than `n`.

**Merkle Search Trees** (Auvolat & Taïani, SRDS'19,
<https://inria.hal.science/hal-02303490/document>) reach the same property differently: item layer
`l(x)` = length of the longest all-zero prefix of `h(x)` in base `B`, so layer-`l` nodes are runs
bounded by higher-layer items. Averages `B−1` values and `B` children per node; the paper fixes
`B = 16`. Unique deterministic tree per item set; a put or delete causes at most one split or merge
per layer below `l(x)`.

Either way, equal content ⇒ equal root hash, so diff and three-way merge are O(divergence)
**unconditionally**, plus dedup across unrelated snapshots.

**Relevance:** the RFC's acceptance criterion "independently built trees receiving byte-identical
ordered operations produce identical roots" is *order*-determinism — strictly weaker. Two be-trees
with identical logical content can differ structurally and by root hash. Since HLC/LWW implies
convergence is a goal, the RFC should either:

- **(a)** say plainly that be-tree is not confluent, and name deferred merge/steal as the reason. The
  precedent is **IPLD HashMap/HAMT** (<https://ipld.io/specs/advanced-data-layouts/hamt/spec/>), whose
  CHAMP mutation rules give canonical form for any key set regardless of insertion *or deletion*
  order — enforced by an explicit invariant ("no non-root node holds fewer than `bucketSize + 1`
  entries", collapsing recursively on delete). Canonicalization *requires* a delete-side collapse
  rule, which is exactly what §4's "merge/steal out of scope" forecloses. The RFC lists these as two
  separate limitations; they are one.
- **(b)** consider a content-defined **split predicate** — salted key hash, clamped to the byte budget
  — which buys near-history-independence while keeping fixed `NODE_BYTES` and fixed search surfaces.

If (b), copy Dolt's current implementation rather than the classic rolling hash.
`go/store/prolly/tree/node_splitter.go` uses `keySplitter`: hashes the **key only**
(`xxh3.HashSeed(key, levelSalt[level])`) against a Weibull-shaped dynamic threshold
`(CDF(end) − CDF(start)) / (1 − CDF(start))` with `targetSize = 4096`, `K = 4`, hard clamps
`minChunkSize = 512` / `maxChunkSize = 16384`, and a per-level salt `saltFromLevel(l) = sha512(l)[:8]`
to decorrelate boundaries across levels. Three portable ideas there: target ~4 KiB not 64 KiB; salt
per level; hash the key only, so value rewrites don't move boundaries.

**A salted or keyed hash is mandatory.** Bluesky's MST spec (<https://atproto.com/specs/repository>)
requires implementations to cap entries-per-node and total depth in its Security Considerations,
precisely because users control record keys and can **mine** them for pathological depth — storage and
firehose amplification. Any content-defined variant inherits that hazard.

## 2. `diff`: adopt a cursor formulation

**Dolt** (<https://www.dolthub.com/blog/2020-06-16-efficient-diff-on-prolly-trees/>) implements diff as
a two-cursor ordered walk where each cursor exposes `Path() []ChunkAddress` and `NextAtLevel(h)`; on
equal chunk addresses at height h, both cursors skip that whole subtree. Because it re-syncs in **key
order** rather than requiring pivot-aligned children, it never falls back to whole-subtree collection.

**Relevance:** this replaces `candidate_keys`' structural-alignment requirement and its `collect_keys`
fallback with an unconditional O(divergence) walk. Exact results unchanged, O(depth) waves preserved,
and it is independent of every other change in this survey — arguably the cheapest large win
available. Note the current fanout-2 spine makes the fallback fire on essentially any split, so this
compounds with the multiway-fanout fix.

## 3. Fixed nodes versus content-defined chunking

What prolly trees **give up**: variable node size (so no fixed-offset SIMD surfaces, no fixed fanout,
unpredictable I/O), a `k/w` write-amplification factor, and vulnerability to adversarial key mining.
What they **gain**: confluence, plus cross-snapshot dedup.

Note the Noms chunk-size defect Dolt documents: a *static* rolling-hash pattern gives a **geometric**
chunk-size distribution (avg 4 KiB, many tiny chunks plus a heavy tail of huge ones), which hurts both
reads (binary search inside big chunks) and COW writes (copying big chunks). That is why Dolt moved to
the clamped Weibull threshold.

**Verdict for the RFC:** fixed `NODE_BYTES` is defensible, and no reviewed system uses truly unbounded
nodes — the industry-leading CDC implementation independently converged on **bounded** node size
(512 B–16 KiB, target 4 KiB, low variance). Fixed size is the limiting case of the same goal. The two
approaches are also not exclusive: keep the canonical fixed-`NODE_BYTES` *physical* format and fixed
search surfaces, but make the *split point* content-defined instead of median. That is the hybrid
worth a paragraph in the RFC even if declined.

## 4. Verified streaming, and verify-on-read granularity

**Bao spec** (<https://github.com/oconnor663/bao/blob/master/docs/spec.md>): BLAKE3's tree is 1 KiB
chunks; a Bao encoding is a length prefix plus parent nodes and chunks in pre-order. The decoder
verifies each node's chaining value as it reads, and the **slice format** omits every chunk and parent
not on the path to the requested byte range — so a subrange can be verified against the root hash
reading only `O(range + log n)` bytes. Two subtleties the spec calls out: the **length is only
validated when the final chunk is validated**, and the decoder must not expose length before that;
and parent nodes are malleable if a decoder reconstructs the tree from chunks without checking them.

**`bao-tree`** (<https://docs.rs/bao-tree>, n0-computer/iroh) generalizes this with a configurable
chunk group, recommending `BlockSize::from_chunk_log(4)` = **16 KiB blocks** — coarser groups shrink
outboard size at the cost of verification granularity — plus `ChunkRanges` queries and
`encode_ranges_validated`.

**Relevance:** the RFC's §5 hashes each raw byte string whole and compares against the requested
`BlockId` — all-or-nothing. Three consequences:

1. **Sub-node verification is achievable but needs a format change.** `BlockId` would have to become a
   BLAKE3 *tree* root (which `blake3::Hasher` already produces) with the store serving an outboard, or
   the node carrying per-region chaining values. Only then could a reader verify just the header +
   `pivot_heads` + one blob region without hashing all of `NODE_BYTES`. Pointless below ~16 KiB — so
   this is an argument for keeping `NODE_BYTES` small *if* verification granularity matters, and
   otherwise an explicit non-goal worth documenting with its escape hatch.
2. **The RFC's own hashing analysis and these defaults agree.** It notes `update_rayon` is typically a
   loss below 128 KiB and correctly declines to inflate node size for hashing. Bao's 16 KiB block and
   Dolt's 512 B–16 KiB clamp land in the same band. (Counterpoint from the buffered-tree side, where
   4 MB is standard — see `buffered-trees.md` §2. This is the genuine tension in the design.)
3. **Adopt Bao's length/malleability discipline.** The be-tree analogue of "don't expose length before
   the final chunk validates" is: **no header field — especially `encoded_len` — may size a read or an
   allocation before the `BlockId` check passes.** The RFC's verify-then-decode ordering is right but
   does not say this, and `encoded_len` on overflow nodes is exactly where it would bite.

## 5. Also surveyed

- **Noms** (<https://github.com/attic-labs/noms/blob/master/doc/intro.md>) — origin of "prolly tree =
  probabilistic B-tree"; core invariant "one logical value ⇔ exactly one hash." The RFC's canonical
  encoding rules already implement the *encoding* half of this; the *structural* half is what §1 is
  about.
- **git trees** (<https://git-scm.com/book/en/v2/Git-Internals-Git-Objects>) — one flat sorted node
  per directory, structure derived from user-visible paths, so no rebalancing at all. Fast diff via
  equal-OID pruning (the same trick be-tree's `diff` uses) but no size control: a 10k-entry directory
  rewrites entirely on one change. The failure mode overflow nodes must avoid.
- **Merklix** (deadalnix, 2016; site currently unreachable) — key-prefix radix trie over a Merkle DAG,
  deterministic and unordered, but no locality for range scans. Lower value here than MST or prolly.
- **Arrow BinaryView** and **ART lazy expansion** both independently store a short key prefix inline
  and defer the full comparison — see `in-node-search.md` §1, §3.
