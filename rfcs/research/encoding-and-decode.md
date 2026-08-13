# Canonical encoding, zero-copy decode, and compression

Canonicality bug classes, Rust validated-view practice, integer compression, and whether to compress
node bytes. Measurements marked *measured* were benchmarked during this survey (aarch64,
`opt-level=3`, synthetic 8 KiB node with 64 entries).

## 1. Canonicality must be verified on decode, not merely produced on encode

This is the strongest cross-cutting theme in the survey, and every item has a real CVE or consensus
incident behind it.

- **rkyv** — RUSTSEC-2021-0054 / CVE-2021-31919: archives leaked **uninitialized padding bytes**.
  Precisely the "same logical node, two hashes" failure, plus an info leak.
- **FlatBuffers** (<https://flatbuffers.dev/internals/>) is non-canonical **by design**: the spec says
  two implementations "may produce different binaries given the same input values, and this is
  perfectly valid," because field order and object order are undefined. Unusable as a hash preimage.
  Also RUSTSEC-2020-0009 / CVE-2020-35864 (`read_scalar` transmutes without `unsafe`) and
  RUSTSEC-2021-0122 (generated code reads/writes out of bounds from *safe* code).
- **Protobuf** ships an explicit warning against this exact use case
  (<https://protobuf.dev/programming-guides/serialization-not-canonical/>) — "taking a fingerprint or
  checksum of a serialized proto" — because output changes with library version, build flags, and
  schema edits, and unknown-field retention plus map ordering make it unfixable. Deterministic ≠
  canonical.
- **Cap'n Proto** (<https://capnproto.org/encoding.html>) has a **canonical form separate from its
  ordinary encoding** — preorder object tree, single segment, trailing zero words truncated, zero-size
  struct pointers at offset −1, unpacked — precisely because the wire format deliberately leaves
  padding unconstrained and truncates default-valued fields.
- **DAG-CBOR** (<https://ipld.io/specs/codecs/dag-cbor/spec/>) gets it right and states the rule:
  "requires that there exist a single, canonical way of encoding any given set of data, and that
  encoded forms contain no superfluous data." Only tag 42, string map keys only, RFC 8949 §4.2
  shortest-form integer headers, and decoders must **reject** non-canonical input, not merely tolerate
  it.
- **Bitcoin** — BIP 62 / BIP 66: non-canonical DER integer and length encodings in signatures caused
  consensus-splitting txid malleability and needed a soft fork. **git** avoids the class entirely by
  defining the object header as an exact printf-format byte string.

**Bug classes to name explicitly in the RFC:**

1. padding not zeroed → one logical node, two hashes;
2. multiple encodings of one value → varints, redundant fields that can disagree, unnormalized empty
   slots;
3. non-canonical length/integer forms;
4. undefined field or object ordering;
5. **redundant derivable fields** — storing both `val_off` and `val_len` when offsets are derivable
   means one node has two encodings. Either reject on mismatch (extra validation surface) or don't
   store it.

**Relevance:** the RFC's existing rules are the right set — sorted by `(key asc, HLC desc)`, at most
one entry per `(key, HLC)`, zero every reserved byte and unused slot, gap-free blob ordering, "zero is
padding, not a sentinel," checked-add bounds before exposing any descriptor. Two additions: state the
general principle once ("canonicality is verified on decode, not merely produced on encode" — the
DAG-CBOR rule, and what makes `hash(bytes)` a sound identity), and prefer **removing** redundancy to
validating it. If data-dependent integer widths are adopted (`block-formats.md` §1), width selection
must be a deterministic function of the data **and the verifier must reject non-minimal encodings**.

## 2. Zero-copy validated decode in Rust

- **`zerocopy`** (<https://docs.rs/zerocopy>) — `FromBytes` (all bit patterns valid), `TryFromBytes`
  (runtime-checked validity), `Unaligned`, `KnownLayout`, `Ref`. Validates size, alignment, and
  per-field bit-pattern validity. It does **not** validate semantic invariants: monotonic offsets,
  `off + len <= blob_len`, and zeroed padding are all yours. Advisory: RUSTSEC-2023-0074
  (`Ref::into_ref`/`into_mut`/`into_slice` unsound with some type params; patched ≥0.7.31).
- **`rkyv`** (<https://rkyv.org/format.html>) — archived types are `repr(C)` with stable-layout
  primitives; feature flags control endianness, `aligned`/`unaligned`, and pointer width. Validation
  via `bytecheck` is a full recursive pass with a subtree-range validator that tracks allocated
  ranges — real cost, and it moves access from "free" to "checked once." Two techniques worth
  borrowing: **explicit-endian integer newtypes** (`rend`) so no host-endianness assumption leaks in,
  which pairs with the RFC's big-endian heads; and a validator that tracks region coverage so
  overlapping or aliasing regions are caught.
- **`bytemuck`** — `Pod`-only, no fallible semantic validation, strictest about alignment.

**Alignment is a non-problem — read, don't reinterpret.** `bytes::Bytes` may be an unaligned subslice
of a larger allocation, so any `&[u64]` reinterpretation is UB. The idiomatic answer is
`u64::from_le_bytes(slice[i..i+8].try_into().unwrap())`, which compiles to a single unaligned load.
*Measured*: a `chunks_exact(8)` + `from_le_bytes` sum loop **autovectorizes to 4×-unrolled `ldp q/q` +
`add.2d`** — zero byte-shuffling, identical codegen to a `&[u64]` loop; the u32 version likewise
vectorizes to `add.4s`. **There is no performance argument for unsafe casts or for demanding an aligned
buffer.** The alternative with typed access and no `unsafe` is `#[repr(C, packed)]` descriptors +
`zerocopy::Unaligned` + `Ref`.

## 3. Field widths: u64 → u32, and a FOR trick for the HLC

**Offsets should be u32, arguably u16.** Offsets index within a ≤16 KiB node, so 14 bits suffice and
u16 covers 64 KiB; u64 is 4× wider than the domain. u32 is the conservative pick — matches Arrow's
default, leaves room to grow `NODE_BYTES`, keeps a 4-byte stride. Whatever the choice, **the format
must reject any offset ≥ `NODE_BYTES`**: narrowing the type shrinks the invalid-input space for free,
which is a *validation* win as much as a space one.

**The HLC is the real overhead.** At 64 entries, 16-byte HLCs plus op tags are 1088 B — 81% of the
descriptor budget after switching to N+1 offsets. Store one 16-byte HLC base per node plus a `u32`
delta per entry: no bit-packing, plain aligned loads, and descriptors land around 594 B, roughly 81%
below the current design. Combined with N+1 offsets (`block-formats.md` §1, §6), this is the
highest-value format change in the survey.

## 4. Verifier checklist

From `flatbuffers/verifier.h` and Cap'n Proto's security section — what production verifiers actually
check:

- **checked arithmetic on every `off + len`** — the overflow case is the classic hole; FlatBuffers has
  an explicit "protect against byte_size overflowing" comment;
- counts validated against the byte budget **before** being used as indices or iteration bounds;
- **reject a node claiming 64 entries with a 0-byte blob** — `entry_count` is attacker-controlled the
  same way FlatBuffers' vtables were. Cap'n Proto needed both a **traversal limit** (64 MiB default in
  C++) against pointer-cycle/overlap amplification *and* a separate **list-amplification** guard
  (huge `Void`/zero-size-struct element counts in constant space);
- **bound total verification work as a function of the header before performing it** — verification
  cost is itself an attack surface, which is why FlatBuffers has `max_depth` and `max_tables`;
- validate **derived** fields against recomputation, not just bounds — see `block-formats.md` §1 on
  `DataBlockValidator`. `head_skip` and the key heads are recomputable from the blob, and a node that
  is memory-safe but internally inconsistent returns *wrong search results* with no UB. Bounds
  checking and fuzzing-for-panics both miss this class entirely.
- verify `encoded_len` against the counts rather than trusting it, and per `content-addressing.md` §4,
  never let it size a read or allocation before the `BlockId` check passes.

## 5. Declined: bit-packing, and block compression

### FastLanes and friends — wrong granularity

**FastLanes** (VLDB 16(9):2132, <https://www.vldb.org/pvldb/vol16/p2132-afroozeh.pdf>) has two ideas:
bit-(un)packing interleaved against a *virtual* 1024-bit register (`FLMM1024`) defined only with ops
present in every SIMD dialect and in scalar code; and the **Unified Transposed Layout** — 1024 values
as eight 8×16 transposed tiles in "04261537" order, chosen so one physical order is optimal for
8/16/32/64-bit lanes. Because lanes are independent, plain scalar loops fully autovectorize; they
report >40 values/cycle. But Table 4 lists "random access" as the shortcoming of the competing delta
layouts, and the design assumes decoding the whole vector.

The Rust crate (<https://github.com/spiraldb/fastlanes>) makes this concrete: `pack::<WIDTH, PACKED>(&[T; 1024], ...)`
**hard-codes 1024 in the type signature**, and the README says that above ~10 values "it is typically
faster to unpack all values and then access the desired one" — `unpack_single` is the slow path.
**Vortex** (<https://docs.vortex.dev/>) is a good production consumer and a decent precedent for
validated-metadata-plus-compressed-payload separation. **BtrBlocks** (SIGMOD'23,
<https://www.cs.cit.tum.de/fileadmin/w00cfj/dis/papers/btrblocks.pdf>) cascades 8 encodings chosen by
compressing a ~1% sample, but that machinery only pays at 64K-row blocks.

Foundations, for completeness: **SIMD-BP128** (<https://arxiv.org/abs/1209.2137>) uses 128-integer
blocks, one bit width each; **Stream VByte** (<https://arxiv.org/abs/1709.08990>) separates control
from data stream (the transferable idea, not the varints); the **`bitpacking` crate**
(<https://docs.rs/bitpacking>) offers `BitPacker1x`/`4x` (128) / `8x` (**256 mandatory**), formats
mutually incompatible.

Random access into bit-packed data *is* O(1) — value *i* at bit `i*w` — but *measured*, a
`base + ((words[i*w/64] >> sh) | words[+1] << (64-sh)) & mask` probe is **~14 instructions with two
loads and two bounds checks**, versus **2 instructions and one load** for
`u32::from_le_bytes(b[i*4..][..4])`. Also note **delta + FOR + bitpack does not preserve random
access** — delta needs a prefix sum from the base, so probe *i* costs O(i). Since be-tree's offsets
*are* a prefix sum of lengths, storing offsets keeps O(1) while storing delta-coded lengths loses it.

**Verdict: decline, with reasons** — (i) every mature library's block granularity (128/256/1024)
mismatches ~64 entries, and padding 64→1024 is a 16× blowup on a 4–16 KiB node; (ii) a bit-packed
probe is ~7× the instruction count on the hot search path; (iii) narrowing u64→u32 plus the HLC FOR
trick (§3) captures most of the space win at zero decode cost and *better* validation; (iv) hand-rolled
bit-packers are exactly where non-canonical-encoding bugs breed, in the unspecified high padding bits
of the last word. Cite as considered and declined rather than as roadmap.

### Whole-node LZ4/zstd — not in v1

*Measured*, synthetic 8 KiB node:

| | zstd-1 | zstd-3 | lz4 |
| --- | --- | --- | --- |
| AoS node | 1.85× | 1.87× | 1.47× |
| SoA node | 1.87× | 1.75× | 1.52× |

Descriptors alone reach 4.8–5.0× at zstd-3 with u64 fields, since they are ~half zero bytes. Note the
**anti-synergy**: u32 descriptors compress to only 2.5×, so the two levers partially cancel — u64
3136 B→648 B versus u32 2112 B→841 B, i.e. the wasteful layout ends up *smaller* compressed. Optimize
one or the other, not both.

The structural problem is worse than the ratio. The hash must cover the stored bytes, so:

- hashing **compressed** bytes makes node identity depend on the compressor's exact version and level
  (zstd does not promise bit-stable output across versions) — the Protobuf canonicality failure of §1,
  one layer down;
- hashing **uncompressed** bytes keeps identity clean but means `NODE_BYTES` is no longer the physical
  size, breaking the RFC's byte-budget accounting, and forces a decompress-into-owned-`Bytes` on every
  fetch, which **destroys zero-copy**.

**Verdict:** not in v1. If ever, put it strictly below the content-address boundary — hash canonical
uncompressed bytes, treat compression as an opaque replaceable storage-layer detail, accept losing
zero-copy for those reads. **Never hash compressor output.**

## 6. SoA versus AoS: search surface only

SoA wins when a kernel touches one field across many records; AoS wins when a record's fields are
consumed together. When the whole array is in L1 — 1–3 KiB of descriptors here — the cache-line
argument evaporates, and *measured*, SoA-vs-AoS compression is within noise (4.84 vs 4.99 at zstd-3
for u64; 2.51 vs 2.63 for u32).

**So: keep `entry_heads` as its own contiguous `[u64; N]`** — that is a genuine SIMD-shape requirement
and the RFC already has it. Don't shred `key_off`/`val_off`/`hlc`/`op` into four more parallel arrays;
that adds four bounds-check sites and four canonicality invariants for no measurable gain. Note the
N+1-offsets change (`block-formats.md` §1) is already a partial SoA move, since the offsets array
becomes its own column, and the hot/cold split in §3 is the other half — which is where the real win
is, not in wholesale SoA.
