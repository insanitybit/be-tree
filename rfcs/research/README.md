# RFC 0001 research notes

Prior-art survey supporting `../0001-simd-native-node-architecture.md`. Five areas, each in its own
file. Sources are linked inline; claims marked *measured* were benchmarked during the survey rather
than taken from a paper.

| File | Area | Headline finding |
| --- | --- | --- |
| [buffered-trees.md](buffered-trees.md) | SplinterDB, TokuDB, BetrFS, sled, Haura, Tucana | Routing filters remove the O(depth) buffer scan on point reads — the RFC's biggest gap |
| [block-formats.md](block-formats.md) | Pebble colblk, RocksDB, Arrow, blob files | Pebble migrated away from exactly the RFC's row-wise descriptor layout |
| [in-node-search.md](in-node-search.md) | Masstree, ART, HOT, array layouts | The u64 head idea is validated; vectorizing the head scan is not |
| [content-addressing.md](content-addressing.md) | Prolly trees, MSTs, IPLD, Bao | be-tree is not confluent, and deferred merge/steal is why |
| [encoding-and-decode.md](encoding-and-decode.md) | Canonicality, zerocopy/rkyv, FastLanes, compression | Canonicality must be *verified on decode*, not merely produced on encode |

## Ranked recommendations

Highest impact first, with the file that argues each one.

1. **Add routing filters** (per-message-group quotient/xor filter + per-child min-live-generation) so
   a point read probes cache lines instead of scanning every buffer on the path — `buffered-trees.md` §1.
2. **Replace `{off, len}` descriptor pairs with N+1 offsets**, and split hot search columns from cold
   `hlc`/`op` payload columns — `block-formats.md` §1.
3. **Demote the SIMD kernel to an outcome of the benchmark matrix**, and measure the equal-head-range
   distribution before optimizing the head scan at all — `in-node-search.md` §2–4.
4. **Widen the `NODE_BYTES` search space to 4 KiB–1 MB** and test partitioned leaf regions
   (TokuDB basements) — `buffered-trees.md` §2.
5. **Add a `MIN_FLUSH_BYTES` floor** to the flush rule and to the acceptance criteria —
   `buffered-trees.md` §3.
6. **Adopt a cursor-based `diff`** to eliminate the whole-subtree fallback — `content-addressing.md` §2.
7. **State the canonicality-is-verified-on-decode principle explicitly**, and the rule that no header
   field may size a read before the `BlockId` check passes — `encoding-and-decode.md` §1, §4.
8. **Narrow offsets to u32** and record FastLanes/bit-packing/block-compression as declined with
   reasons — `encoding-and-decode.md` §3, §5.
