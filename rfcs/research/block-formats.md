# LSM block formats

Pebble, RocksDB, Arrow, and blob/value-separation designs. Format-level lessons for the canonical
node layout.

## 1. Pebble migrated away from exactly the RFC's row-wise descriptor layout

Pebble's columnar block format (`sstable/colblk/` in cockroachdb/pebble) is the most directly
applicable prior art available.

- **`raw_bytes.go` — N+1 offsets, no lengths.** Length is `off[j+1] - off[j]`. Same as Arrow's
  variable-binary layout (<https://arrow.apache.org/docs/format/Columnar.html>), which mandates
  monotonic offsets "even for null slots… this property ensures the location for all values is valid
  and well defined." This **structurally eliminates** the overlapping/inconsistent-length malformed
  cases the RFC's verifier would otherwise enumerate, and collapses validation to one monotonicity
  sweep. At 64 entries it takes descriptors from ~3136 B to ~1346 B. Duplicate keys encode as equal
  consecutive offsets (an empty slice).
- **`prefix_bytes.go` — bundled prefixes.** One block-wide prefix plus one prefix per power-of-two
  **bundle**, each row storing only its suffix relative to its *bundle* prefix, not the previous key.
  The package doc is explicit about the tradeoff: this "can result in less compression, but
  simplifies reverse iteration and allows iteration to be largely stateless." Preserves O(1) random
  access. Seek binary-searches only the first key of each bundle, since the bundle prefix is
  physically adjacent to it. Worked example: 119 bytes of keys → 61 bytes.
- **`uints.go` — data-dependent widths.** `UintEncoding` = bytes-per-value (0/1/2/4/8) plus a delta
  bit with the base stored once; width 0 means all values equal. `DetermineUintEncoding` picks the
  narrowest valid encoding, with a `UintEncodingRowThreshold = 8` heuristic so the 8-byte delta base
  pays for itself.
- **`block.go` — column directory.** Per-column `{type: 1B, page offset: 4B}`, so columns can be
  added without a version bump; rarely-read columns go last. Exactly one trailing padding byte,
  specified so that one-past-end of the last column is a valid pointer — padding that is
  load-bearing and documented, not merely "zeros."
- **`data_block.go` — `DataBlockValidator.Validate`** checks *derived* fields against recomputation
  (that the `prefixChanged` bitmap matches recomputed `Split.Prefix` results), not just bounds.
  Returns errors, not panics. Also: columns were deliberately *not* added to the cached decoder
  struct, because growing it would push every cached block across allocator size classes.

**Relevance:** four concrete changes. (i) Switch to N+1 offsets. (ii) Consider per-bundle skips —
this is also the fix for the non-uniform-prefix failure mode in `in-node-search.md` §4. (iii) Split
hot from cold: `hlc` (16 B) and `op` are never touched during search yet in a row layout they share
cache lines with the offsets being searched; at 64 entries HLCs+tags are 1088 B, and a per-node HLC
base plus a `u32` delta per entry would cut total descriptors ~81%. (iv) Validate derived fields —
`head_skip` and the key heads are recomputable from the blob, and a node that is memory-safe but
internally inconsistent returns *wrong search results* with no UB. That bug class is invisible to
bounds-checking and to fuzzing-for-panics.

Caveat if data-dependent widths are adopted: width selection must be a deterministic function of the
data (Pebble's is) **and the verifier must reject non-minimal encodings**, or two writers produce
different bytes for one logical node. See `encoding-and-decode.md` §1.

## 2. The row-wise baseline, and why not to copy it

**RocksDB BlockBasedTable** (<https://github.com/facebook/rocksdb/wiki/Rocksdb-BlockBasedTable-Format>)
encodes each entry as `varint(shared) varint(non_shared) varint(value_len) key_delta value`,
prefix-compressed against the *previous* key, reset every `block_restart_interval` (default 16) keys,
with a trailing u32 restart-offset array for binary search followed by a linear scan of ≤16 entries.

**Pebble `sstable/rowblk/rowblk_writer.go`** adds two hardening details worth noting:
`MaximumRestartOffset = 1<<31 - 1`, because the **top bit of each restart offset is stolen** for a
`setHasSameKeyPrefixSinceLastRestart` flag; and the writer returns `ErrBlockTooBig` rather than
silently emitting an inexpressible offset.

**Relevance:** varint + delta-against-previous-key means **no random access** — you can binary-search
only restarts, never rows. That is precisely why be-tree should not adopt classic prefix compression,
and why Pebble's bundle scheme is the better model for a random-access validated view. The
bit-stealing is a good example of a deliberate, documented, bounded trade; if the RFC's flags or
reserved bits ever do something similar, spell out the bound and error rather than truncating.

## 3. What an in-block search surface is worth

**RocksDB Data Block Hash Index** (<https://github.com/facebook/rocksdb/wiki/Data-Block-Hash-Index>)
appends a `uint8` bucket array mapping hash(key) → restart index, with `kNoEntry=255` and
`kCollision=254` sentinels; its presence flag is the **MSB of the existing num_restarts u32**, free
because blocks under 32 KB never use it. Measured: `DataBlockIter::Seek()` CPU **−21.8%**, overall
throughput **+10%** on fully cached workloads, at **+4.6% space**. Limits: ≤253 restart intervals,
point lookups only (range seeks fall back to binary search), and it requires that the comparator not
treat differing bytes as equal.

**Relevance:** quantifies what the RFC's key-head array is buying, and frames it honestly — this is a
*cache-miss* optimization, not a comparison-count one. The RFC's heads are order-preserving and so
serve range seeks too, which is a genuine advantage over a hash index and worth stating. The
MSB-of-num_restarts trick is also the canonical example of extending a format without a version bump,
and a reminder that reserved header bits must be **required zero and rejected if nonzero**, or that
option is lost.

**Index types** (`include/rocksdb/table.h`): `kBinarySearchWithFirstKey` stores the first key of each
block so iterators can defer reading it — "may significantly reduce read amplification of short range
scans… Makes the index significantly bigger (2x or more), especially when keys are long." Same bet as
the RFC's heads, and the honest downside is index size growing with key length; be-tree's fixed 8-byte
head bounds that growth, which is the design's real advantage. A newer `BlockSearchType::kInterpolation`
is read-time-only (byte-wise comparators only, and it warns that separator shortening skews end keys)
— a nice example of a search strategy needing no format change at all.

**Partitioned index/filters** (<https://github.com/facebook/rocksdb/wiki/Partitioned-Index-Filters>):
for a 256 MB SST, index/filter blocks run 0.5 MB/5 MB against 4–32 KB data blocks, so one filter
evicts thousands of data blocks. Fixed `NODE_BYTES` already bounds this, but it argues for sizing the
header + head array to stay within a couple of cache lines and one allocator size class.

**PlainTable** (<https://github.com/facebook/rocksdb/wiki/PlainTable-Format>) is the closest existing
thing to a zero-copy view over shared bytes: mmap-oriented, no block cache, no compression, no delta
encoding, no `Prev()`, files under 2^31. Its limitation list is a useful sanity check — but note its
index is built *in memory at load time*, not stored canonically. Storing the search surface in the
bytes is the harder and, for content addressing, the correct choice.

## 4. Large values and the overflow path

**Pebble blob files** (`sstable/blob/doc.go`, `handle.go`): value blocks plus a **columnar** index
block (virtual-block remapping column + `numBlocks+1` offsets) and a fixed footer. Values are
addressed by *logical* `(blockID, blockValueID)`, not byte offsets, so a blob file can be rewritten to
drop dead values **without rewriting the referencing sstables**. Handles split into an eagerly-decoded
`InlineHandlePreface` (blob-reference index + value length) and a lazily-decoded `HandleSuffix`.
Absent values inside a live block are empty slices (2–4 bytes); whole dead blocks are elided.

**RocksDB BlobDB** (<https://github.com/facebook/rocksdb/wiki/BlobDB>): WiscKey-style separation
above `min_blob_size`, with per-blob compression **and per-blob CRC32c**, GC integrated into
compaction.

**Relevance to `OverflowLeaf`:** (i) addressing payloads by logical ID rather than byte offset is what
makes independent compaction possible — content addressing already gives be-tree this for free, which
is worth calling out as an advantage; (ii) split the handle so the hot path (value length, presence)
decodes eagerly and the cold path (location) lazily, mirroring the RFC's need for `val_len` without
touching the value; (iii) verify each overflow payload independently, since reading one is a separate
I/O.

## 5. Checksums and corruption

**Pebble** (`sstable/block/block.go`, `physical.go`): `TrailerLen = 5` — 1 byte block type
(compression) + 4-byte LE checksum, **per block**. `ChecksumType`: None, CRC32c, XXHash, XXHash64
(truncated to 32 bits). Crucially the checksum covers `block ++ blockTypeByte`, so a flipped
compression byte is detected. On mismatch, `ValidateChecksum` goes on to re-checksum sub-ranges to
localize the corruption for diagnostics.

**RocksDB full-file checksum**
(<https://github.com/facebook/rocksdb/wiki/Full-File-Checksum-and-Checksum-Handoff>) exists because
per-block checksums verify *contents* but not *identity*: "If a wrong SST file is transferred to a
RocksDB SST file directory, all block checksums will match, but it doesn't contain the data we want."

**Relevance:** content addressing gives be-tree the identity property for free and stronger — the hash
*is* the identity, closing exactly the hole RocksDB needed a second mechanism for. So no per-node CRC
is warranted for identity; the only argument for a cheap checksum is fast rejection before the
expensive structural validation, and Pebble prices that at 4 truncated bytes. Two specific carries:
(a) include version/kind/flags inside whatever is hashed, mirroring Pebble putting the compression
byte inside the checksummed region; (b) return **typed errors distinguishing hash mismatch** (bit rot,
wrong node) **from structural malformation** (bad writer, attack) — the operational responses differ,
which is why Pebble has a localization pass at all.

## 6. Offset arrays elsewhere

- **Arrow** — `length+1` offsets, no lengths; `slot_length = offsets[j+1] - offsets[j]`; 32-bit by
  default, 64-bit only in `LargeBinary`/`LargeList`. Newer **BinaryView** (format 1.4) is the other
  design point: a 16-byte view of `length | prefix | buf_index | offset`, inlining strings ≤12 bytes —
  note the 4-byte inline *prefix*, which is the same idea as the RFC's heads.
- **Parquet** stores lengths (4-byte LE prefix per BYTE_ARRAY) because it is a sequential-decode
  format with no random-access requirement.
- **Postgres varlena/TOAST** — 1-byte header under 127 bytes, 4-byte otherwise; precedent for
  short-form headers, with the item pointer array providing O(1) indexing.
- **Dolt's physical node** (`go/serial/prolly.fbs`) — `key_items` blob + `key_offsets:[uint16]`,
  separate `value_items`/`value_offsets`, `address_array`, varint `subtree_counts`, `tree_count`,
  `tree_level`. Independent confirmation of splitting offset arrays from blobs. Two header fields the
  RFC lacks and should consider: **`tree_level`** (validate equal leaf depth *locally* — an RFC
  acceptance criterion with no local check today) and **`subtree_counts`/`tree_count`** (O(1)
  cardinality, rank queries, progress-bounded diffs). `uint16` offsets also imply ≤64 KiB nodes.

**Bottom line:** no major format stores both offsets and lengths. Given the RFC's stated invariant
(strictly increasing, gap-free), storing both is redundant and creates a second encoding of one
logical node. Since keys and values interleave in the blob, use a single unified `2N+1` offsets array
over alternating key/value spans rather than two arrays.
