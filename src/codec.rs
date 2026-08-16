//! The canonical node codec: one byte string for one logical node, and a **validated** view over it.
//!
//! Encoding is deterministic — sorted entries, one entry per key, N+1 offset columns written in
//! logical order with no gaps, every reserved byte and unused slot zeroed, exactly
//! [`Format::node_bytes`] emitted. Zero is *padding, not a sentinel*.
//!
//! Decoding is not a struct cast. [`NodeView::decode`] checks every canonicality rule *before* any
//! descriptor is exposed, so malformed bytes return [`DecodeError`] and never reach search. All
//! reads are `from_le_bytes` over unaligned slices; nothing assumes the store returned an aligned
//! `Bytes`.

use std::sync::Arc;

use bytes::Bytes;

use crate::format::{
    EXTERNAL_ID_BYTES, Format, HEAD_BYTES, HEADER_BYTES, NODE_MAGIC, NodeKind, OP_EXTERNAL,
    OP_INLINE, OP_TOMBSTONE, Sections,
};
use crate::search::{self, Found, Surface};
use crate::{BlockId, CapacityError, DecodeError, TreeError, VERSION_BYTES, Winner, WinnerOp};

mod layout;
use layout::{
    OffsetColumn, all_zero, check_offsets, get_u16, get_u32, hex, put_u16, put_u32, section_gaps,
};

use layout::hdr;

/// One logical entry, as the encoder takes it and the view hands it back. `span` is the value
/// *representation*: the value bytes for an inline upsert, exactly one 32-byte `ValueObject` id for an
/// out-of-line upsert, and empty for a tombstone. The explicit `op` distinguishes all three.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub key: Bytes,
    pub order_key: [u8; VERSION_BYTES],
    pub op: u8,
    pub span: Bytes,
    pub external_len: u32,
}

impl Entry {
    pub fn inline(key: Bytes, order_key: [u8; VERSION_BYTES], value: Bytes) -> Entry {
        Entry {
            key,
            order_key,
            op: OP_INLINE,
            span: value,
            external_len: 0,
        }
    }

    pub fn external(key: Bytes, order_key: [u8; VERSION_BYTES], id: BlockId, len: u32) -> Entry {
        Entry {
            key,
            order_key,
            op: OP_EXTERNAL,
            span: Bytes::copy_from_slice(&id.0),
            external_len: len,
        }
    }

    pub fn tombstone(key: Bytes, order_key: [u8; VERSION_BYTES]) -> Entry {
        Entry {
            key,
            order_key,
            op: OP_TOMBSTONE,
            span: Bytes::new(),
            external_len: 0,
        }
    }

    /// Blob bytes this entry occupies: key + value span. Never the out-of-line payload.
    pub fn blob_len(&self) -> usize {
        self.key.len() + self.span.len()
    }

    pub(crate) fn is_tombstone(&self) -> bool {
        self.op == OP_TOMBSTONE
    }

    /// The comparable winner tuple. This is the *only* place a persisted candidate becomes ordered.
    pub(crate) fn winner(&self) -> Winner {
        Winner {
            order_key: self.order_key,
            op: match self.op {
                OP_INLINE => WinnerOp::Inline(self.span.clone()),
                OP_EXTERNAL => WinnerOp::External {
                    id: BlockId(
                        self.span
                            .as_ref()
                            .try_into()
                            .expect("validated: an external span is exactly 32 bytes"),
                    ),
                    len: self.external_len,
                },
                _ => WinnerOp::Tombstone,
            },
        }
    }
}

/// Read only the self-declared schema from a node envelope. This does not validate the node; it is
/// used solely to select a decoder from [`Format::known_schema`], after which normal hash and
/// structural verification still run in full.
pub fn declared_schema(bytes: &[u8]) -> Result<crate::format::SchemaId, TreeError> {
    if bytes.len() < HEADER_BYTES {
        return Err(TreeError::decode(
            None,
            DecodeError::Length {
                found: bytes.len(),
                expected: HEADER_BYTES,
            },
        ));
    }
    if bytes[hdr::MAGIC] != NODE_MAGIC {
        return Err(TreeError::decode(None, DecodeError::Magic));
    }
    Ok(bytes[hdr::SCHEMA_ID].try_into().expect("SCHEMA_ID_BYTES"))
}

fn builder_bug(msg: impl std::fmt::Display) -> TreeError {
    TreeError::Capacity(CapacityError::Format(msg.to_string()))
}

/// Encode a leaf. Entries must be sorted by key ascending with exactly one entry per key.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub fn encode_leaf(fmt: &Format, entries: &[Entry]) -> Result<Bytes, TreeError> {
    let regular_blob: usize = entries.iter().map(Entry::blob_len).sum();
    let compact_blob = compressed_entry_blob_len(fmt, entries);
    let kind = fmt.leaf_kind(entries.len(), regular_blob, compact_blob).ok_or_else(|| {
        builder_bug(format!(
            "{} leaf entries using {regular_blob}/{compact_blob} blob bytes fit neither leaf layout",
            entries.len()
        ))
    })?;
    encode(fmt, kind, 0, &[], &[], entries)
}

/// Encode an internal node. `children.len() == pivots.len() + 1`, `pivots[i] = min(children[i+1])`,
/// and `buffer` is sorted with one entry per key.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub fn encode_internal(
    fmt: &Format,
    tree_level: u16,
    pivots: &[Bytes],
    children: &[BlockId],
    buffer: &[Entry],
) -> Result<Bytes, TreeError> {
    encode(
        fmt,
        NodeKind::Internal,
        tree_level,
        pivots,
        children,
        buffer,
    )
}

pub(crate) fn compressed_entry_blob_len(fmt: &Format, entries: &[Entry]) -> usize {
    let skip = if fmt.compresses_pivots() {
        search::head_skip(
            entries.first().map(|entry| entry.key.as_ref()),
            entries.last().map(|entry| entry.key.as_ref()),
            entries.len(),
        ) as usize
    } else {
        0
    };
    skip + entries
        .iter()
        .map(|entry| entry.key.len() - skip + entry.span.len())
        .sum::<usize>()
}

fn encode(
    fmt: &Format,
    kind: NodeKind,
    tree_level: u16,
    pivots: &[Bytes],
    children: &[BlockId],
    entries: &[Entry],
) -> Result<Bytes, TreeError> {
    let input = EncodeInput {
        fmt,
        kind,
        tree_level,
        pivots,
        children,
        entries,
    };
    let plan = EncodePlan::new(&input)?;
    let mut bytes = vec![0u8; fmt.node_bytes()];
    plan.write_header_and_columns(&mut bytes, &input);
    plan.write_blobs(&mut bytes, &input);
    Ok(Bytes::from(bytes))
}

#[derive(Clone, Copy)]
struct EncodeInput<'a> {
    fmt: &'a Format,
    kind: NodeKind,
    tree_level: u16,
    pivots: &'a [Bytes],
    children: &'a [BlockId],
    entries: &'a [Entry],
}

#[derive(Clone, Copy)]
struct EncodePlan {
    sections: Sections,
    internal: bool,
    entry_skip: u32,
    pivot_skip: u32,
}

impl EncodePlan {
    fn new(input: &EncodeInput<'_>) -> Result<Self, TreeError> {
        let EncodeInput {
            fmt,
            kind,
            tree_level,
            pivots,
            children,
            entries,
        } = *input;
        let s = *fmt.sections_for(kind);
        let internal = kind == NodeKind::Internal;

        // Capacity is rejected explicitly rather than discovered by an out-of-bounds write.
        if entries.len() > s.slots {
            return Err(builder_bug(format!(
                "{} entries exceed {} slots",
                entries.len(),
                s.slots
            )));
        }
        if internal {
            if children.len() != pivots.len() + 1 {
                return Err(builder_bug(format!(
                    "child_count {} != pivot_count {} + 1",
                    children.len(),
                    pivots.len()
                )));
            }
            if children.len() < 2 || children.len() > fmt.f_max() {
                return Err(builder_bug(format!(
                    "child_count {} outside 2..={}",
                    children.len(),
                    fmt.f_max()
                )));
            }
            if tree_level == 0 || tree_level > fmt.max_tree_level() {
                return Err(TreeError::Capacity(CapacityError::TreeTooTall {
                    level: u32::from(tree_level),
                    limit: fmt.max_tree_level(),
                }));
            }
        }

        let entry_blob = if kind == NodeKind::CompactLeaf {
            compressed_entry_blob_len(fmt, entries)
        } else {
            entries.iter().map(Entry::blob_len).sum()
        };
        if entry_blob > s.entry_blob_cap {
            return Err(builder_bug(format!(
                "entry blob {entry_blob} exceeds capacity {}",
                s.entry_blob_cap
            )));
        }
        let pivot_blob = fmt.pivot_blob_len(pivots);
        if pivot_blob > s.pivot_blob_cap {
            return Err(builder_bug(format!(
                "pivot blob {pivot_blob} exceeds capacity {}",
                s.pivot_blob_cap
            )));
        }
        for entry in entries {
            fmt.check_key(&entry.key)?;
        }
        for pivot in pivots {
            fmt.check_key(pivot)?;
        }
        if entries.windows(2).any(|w| w[0].key >= w[1].key) {
            return Err(builder_bug("entries are not strictly ascending by key"));
        }
        if pivots.windows(2).any(|w| w[0] >= w[1]) {
            return Err(builder_bug("pivots are not strictly ascending"));
        }

        Ok(Self {
            sections: s,
            internal,
            entry_skip: search::head_skip(
                entries.first().map(|entry| entry.key.as_ref()),
                entries.last().map(|entry| entry.key.as_ref()),
                entries.len(),
            ),
            pivot_skip: search::head_skip(
                pivots.first().map(|pivot| pivot.as_ref()),
                pivots.last().map(|pivot| pivot.as_ref()),
                pivots.len(),
            ),
        })
    }

    fn write_header_and_columns(self, buf: &mut [u8], input: &EncodeInput<'_>) {
        let EncodeInput {
            fmt,
            kind,
            tree_level,
            pivots,
            children,
            entries,
        } = *input;
        let s = self.sections;
        buf[hdr::MAGIC].copy_from_slice(&NODE_MAGIC);
        buf[hdr::SCHEMA_ID].copy_from_slice(fmt.schema_id());
        buf[hdr::KIND] = kind as u8;
        put_u16(buf, hdr::TREE_LEVEL, tree_level);
        put_u16(buf, hdr::PIVOT_COUNT, pivots.len() as u16);
        put_u16(
            buf,
            hdr::CHILD_COUNT,
            if self.internal {
                children.len() as u16
            } else {
                0
            },
        );
        put_u32(buf, hdr::ENTRY_COUNT, entries.len() as u32);
        put_u32(buf, hdr::ENTRY_HEAD_SKIP, self.entry_skip);
        put_u32(buf, hdr::PIVOT_HEAD_SKIP, self.pivot_skip);
        buf[hdr::VERSION_DOMAIN].copy_from_slice(fmt.version_domain());

        for (i, entry) in entries.iter().enumerate() {
            let head = search::head_of(&entry.key, self.entry_skip as usize).to_le_bytes();
            buf[s.entry_head(i)].copy_from_slice(&head);
        }
        if self.internal {
            for (i, pivot) in pivots.iter().enumerate() {
                let head = search::head_of(pivot, self.pivot_skip as usize).to_le_bytes();
                buf[s.pivot_head(i)].copy_from_slice(&head);
            }
            for (i, child) in children.iter().enumerate() {
                buf[s.child_id(i)].copy_from_slice(&child.0);
            }
        }
    }

    fn write_blobs(self, buf: &mut [u8], input: &EncodeInput<'_>) {
        let EncodeInput {
            fmt,
            kind,
            pivots,
            entries,
            ..
        } = *input;
        let s = self.sections;
        if self.internal {
            let skip = self.pivot_skip as usize;
            if fmt.compresses_pivots() && skip > 0 {
                buf[s.pivot_blob..s.pivot_blob + skip].copy_from_slice(&pivots[0][..skip]);
            }
            let base = if fmt.compresses_pivots() { skip } else { 0 };
            let mut at = 0u32;
            put_u32(buf, s.pivot_offset(0), 0);
            for (i, pivot) in pivots.iter().enumerate() {
                let suffix = if fmt.compresses_pivots() {
                    &pivot[skip..]
                } else {
                    pivot.as_ref()
                };
                let start = s.pivot_blob + base + at as usize;
                buf[start..start + suffix.len()].copy_from_slice(suffix);
                at += suffix.len() as u32;
                put_u32(buf, s.pivot_offset(i + 1), at);
            }
        }

        let entry_prefix = if kind == NodeKind::CompactLeaf {
            self.entry_skip as usize
        } else {
            0
        };
        if entry_prefix > 0 {
            buf[s.entry_blob..s.entry_blob + entry_prefix]
                .copy_from_slice(&entries[0].key[..entry_prefix]);
        }
        let mut at = 0u32;
        put_u32(buf, s.entry_offset(0), 0);
        for (i, entry) in entries.iter().enumerate() {
            let key = &entry.key[entry_prefix..];
            let start = s.entry_blob + entry_prefix + at as usize;
            buf[start..start + key.len()].copy_from_slice(key);
            at += key.len() as u32;
            put_u32(buf, s.entry_offset(2 * i + 1), at);
            let start = s.entry_blob + entry_prefix + at as usize;
            buf[start..start + entry.span.len()].copy_from_slice(&entry.span);
            at += entry.span.len() as u32;
            put_u32(buf, s.entry_offset(2 * i + 2), at);

            buf[s.order_key(i)].copy_from_slice(&entry.order_key);
            buf[s.ops + i] = entry.op;
            put_u32(buf, s.external_len(i), entry.external_len);
        }
    }
}

/// A decoded node: a validated *view* over shared bytes. Every accessor below is in-bounds by
/// construction because [`NodeView::decode`] already proved it.
#[derive(Debug)]
pub struct NodeView {
    bytes: Bytes,
    fmt: Arc<Format>,
    s: Sections,
    kind: NodeKind,
    tree_level: u16,
    entry_count: usize,
    pivot_count: usize,
    child_count: usize,
    entry_head_skip: usize,
    pivot_head_skip: usize,
    entry_blob_len: usize,
    pivot_blob_len: usize,
    pivots: Vec<Bytes>,
    entry_keys: Vec<Bytes>,
    entry_prefix_len: usize,
}

struct DecodedHeader {
    sections: Sections,
    kind: NodeKind,
    tree_level: u16,
    entry_count: usize,
    pivot_count: usize,
    child_count: usize,
    entry_head_skip: usize,
    pivot_head_skip: usize,
}

struct DecodedBlobs {
    entry_blob_len: usize,
    pivot_blob_len: usize,
    pivots: Vec<Bytes>,
    entry_keys: Vec<Bytes>,
    entry_prefix_len: usize,
}

fn decode_envelope(fmt: &Format, id: Option<BlockId>, bytes: &[u8]) -> Result<NodeKind, TreeError> {
    let err = |reason| TreeError::decode(id, reason);
    if bytes.len() != fmt.node_bytes() {
        return Err(err(DecodeError::Length {
            found: bytes.len(),
            expected: fmt.node_bytes(),
        }));
    }
    if bytes[hdr::MAGIC] != NODE_MAGIC {
        return Err(err(DecodeError::Magic));
    }
    let found: crate::format::SchemaId = bytes[hdr::SCHEMA_ID].try_into().expect("SCHEMA_ID_BYTES");
    if &found != fmt.schema_id() {
        return Err(err(DecodeError::SchemaId {
            found: hex(&found),
            expected: fmt.schema_hex(),
        }));
    }
    let kind = NodeKind::from_byte(bytes[hdr::KIND])
        .ok_or_else(|| err(DecodeError::Kind(bytes[hdr::KIND])))?;
    if kind == NodeKind::CompactLeaf && !fmt.compresses_pivots() {
        return Err(err(DecodeError::Kind(bytes[hdr::KIND])));
    }
    if bytes[hdr::FLAGS] != 0 {
        return Err(err(DecodeError::Flags(bytes[hdr::FLAGS])));
    }
    if !all_zero(&bytes[hdr::RESERVED_A]) {
        return Err(err(DecodeError::Reserved("header")));
    }
    let domain: [u8; 16] = bytes[hdr::VERSION_DOMAIN].try_into().expect("16 bytes");
    if &domain != fmt.version_domain() {
        return Err(TreeError::VersionDomainMismatch {
            expected: *fmt.version_domain(),
            found: domain,
        });
    }
    Ok(kind)
}

impl DecodedHeader {
    fn read(fmt: &Format, id: Option<BlockId>, bytes: &[u8]) -> Result<Self, TreeError> {
        let err = |reason| TreeError::decode(id, reason);
        let kind = decode_envelope(fmt, id, bytes)?;

        let sections = *fmt.sections_for(kind);
        let entry_count = get_u32(bytes, hdr::ENTRY_COUNT) as usize;
        let pivot_count = usize::from(get_u16(bytes, hdr::PIVOT_COUNT));
        let child_count = usize::from(get_u16(bytes, hdr::CHILD_COUNT));
        let tree_level = get_u16(bytes, hdr::TREE_LEVEL);
        if entry_count > sections.slots {
            return Err(err(DecodeError::Count {
                what: "entry_count",
                found: entry_count as u64,
                capacity: sections.slots as u64,
            }));
        }
        if kind == NodeKind::Internal {
            if pivot_count + 1 != child_count {
                return Err(err(DecodeError::Cardinality {
                    child: child_count as u16,
                    pivot: pivot_count as u16,
                }));
            }
            if child_count < 2 || child_count > fmt.f_max() {
                return Err(err(DecodeError::Count {
                    what: "child_count",
                    found: child_count as u64,
                    capacity: fmt.f_max() as u64,
                }));
            }
            if tree_level == 0 || tree_level > fmt.max_tree_level() {
                return Err(err(DecodeError::TreeLevel {
                    found: tree_level,
                    max: fmt.max_tree_level(),
                }));
            }
        } else {
            if pivot_count != 0 || child_count != 0 {
                return Err(err(DecodeError::Cardinality {
                    child: child_count as u16,
                    pivot: pivot_count as u16,
                }));
            }
            if tree_level != 0 {
                return Err(err(DecodeError::TreeLevel {
                    found: tree_level,
                    max: 0,
                }));
            }
        }

        let entry_head_skip = get_u32(bytes, hdr::ENTRY_HEAD_SKIP) as usize;
        let pivot_head_skip = get_u32(bytes, hdr::PIVOT_HEAD_SKIP) as usize;
        if entry_head_skip > fmt.max_key_bytes() || pivot_head_skip > fmt.max_key_bytes() {
            return Err(err(DecodeError::HeadSkip {
                found: entry_head_skip.max(pivot_head_skip) as u32,
                expected: fmt.max_key_bytes() as u32,
            }));
        }
        if kind != NodeKind::Internal && pivot_head_skip != 0 {
            return Err(err(DecodeError::Padding("pivot_head_skip on a leaf")));
        }

        Ok(Self {
            sections,
            kind,
            tree_level,
            entry_count,
            pivot_count,
            child_count,
            entry_head_skip,
            pivot_head_skip,
        })
    }

    fn decode_blobs(
        &self,
        fmt: &Format,
        id: Option<BlockId>,
        bytes: &[u8],
    ) -> Result<DecodedBlobs, TreeError> {
        let err = |reason| TreeError::decode(id, reason);
        let s = self.sections;
        let entry_prefix_len = if self.kind == NodeKind::CompactLeaf {
            self.entry_head_skip
        } else {
            0
        };
        let entry_suffix_len = check_offsets(
            bytes,
            id,
            OffsetColumn {
                base: s.entry_offsets,
                live: 2 * self.entry_count + 1,
                slots: 2 * s.slots + 1,
                cap: s.entry_blob_cap.saturating_sub(entry_prefix_len),
            },
        )?;
        let entry_blob_len = entry_prefix_len + entry_suffix_len;
        if entry_blob_len > s.entry_blob_cap {
            return Err(err(DecodeError::OffsetRange {
                found: entry_blob_len as u64,
                capacity: s.entry_blob_cap as u64,
            }));
        }
        let entry_keys = reconstruct_prefixed(
            bytes,
            s.entry_blob,
            entry_prefix_len,
            self.entry_count,
            |i| (s.entry_offset(2 * i), s.entry_offset(2 * i + 1)),
        );

        let (pivot_blob_len, pivots) = if self.kind == NodeKind::Internal {
            let prefix_len = if fmt.compresses_pivots() {
                self.pivot_head_skip
            } else {
                0
            };
            let suffix_len = check_offsets(
                bytes,
                id,
                OffsetColumn {
                    base: s.pivot_offsets,
                    live: self.pivot_count + 1,
                    slots: fmt.f_max(),
                    cap: s.pivot_blob_cap.saturating_sub(prefix_len),
                },
            )?;
            let physical_len = prefix_len + suffix_len;
            if physical_len > s.pivot_blob_cap {
                return Err(err(DecodeError::OffsetRange {
                    found: physical_len as u64,
                    capacity: s.pivot_blob_cap as u64,
                }));
            }
            let pivots =
                reconstruct_prefixed(bytes, s.pivot_blob, prefix_len, self.pivot_count, |i| {
                    (s.pivot_offset(i), s.pivot_offset(i + 1))
                });
            (physical_len, pivots)
        } else {
            (0, Vec::new())
        };

        Ok(DecodedBlobs {
            entry_blob_len,
            pivot_blob_len,
            pivots,
            entry_keys,
            entry_prefix_len,
        })
    }

    fn check_padding(
        &self,
        fmt: &Format,
        id: Option<BlockId>,
        bytes: &[u8],
        blobs: &DecodedBlobs,
    ) -> Result<(), TreeError> {
        let err = |reason| TreeError::decode(id, reason);
        let s = self.sections;
        let zero = |range: std::ops::Range<usize>, why| {
            all_zero(&bytes[range])
                .then_some(())
                .ok_or_else(|| err(DecodeError::Padding(why)))
        };
        zero(
            s.entry_heads + self.entry_count * HEAD_BYTES..s.entry_heads + s.slots * HEAD_BYTES,
            "unused entry head lanes",
        )?;
        zero(
            s.order_keys + self.entry_count * VERSION_BYTES..s.order_keys + s.slots * VERSION_BYTES,
            "unused order-key slots",
        )?;
        zero(s.ops + self.entry_count..s.ops + s.slots, "unused op slots")?;
        zero(
            s.external_lens + self.entry_count * 4..s.external_lens + s.slots * 4,
            "unused external_value_len slots",
        )?;
        zero(
            s.entry_blob + blobs.entry_blob_len..s.entry_blob + s.entry_blob_cap,
            "entry blob tail",
        )?;
        for gap in section_gaps(fmt, self.kind) {
            zero(gap, "section alignment")?;
        }
        if self.kind == NodeKind::Internal {
            zero(
                s.pivot_heads + self.pivot_count * HEAD_BYTES
                    ..s.pivot_heads + (fmt.f_max() - 1) * HEAD_BYTES,
                "unused pivot head lanes",
            )?;
            zero(
                s.child_ids + self.child_count * 32..s.child_ids + fmt.f_max() * 32,
                "unused child slots",
            )?;
            zero(
                s.pivot_blob + blobs.pivot_blob_len..s.pivot_blob + s.pivot_blob_cap,
                "pivot blob tail",
            )?;
            for i in 0..self.child_count {
                if all_zero(&bytes[s.child_id(i)]) {
                    return Err(err(DecodeError::ZeroChild(i)));
                }
            }
        }
        Ok(())
    }
}

fn reconstruct_prefixed(
    bytes: &[u8],
    blob_start: usize,
    prefix_len: usize,
    count: usize,
    offsets: impl Fn(usize) -> (std::ops::Range<usize>, std::ops::Range<usize>),
) -> Vec<Bytes> {
    if prefix_len == 0 {
        return Vec::new();
    }
    let prefix = &bytes[blob_start..blob_start + prefix_len];
    (0..count)
        .map(|i| {
            let (a, z) = offsets(i);
            let a = get_u32(bytes, a) as usize;
            let z = get_u32(bytes, z) as usize;
            let mut value = Vec::with_capacity(prefix_len + z - a);
            value.extend_from_slice(prefix);
            value.extend_from_slice(
                &bytes[blob_start + prefix_len + a..blob_start + prefix_len + z],
            );
            Bytes::from(value)
        })
        .collect()
}

impl NodeView {
    /// Validate `bytes` as a canonical node of `fmt`. `id` is only for error reporting; hash
    /// verification happens in the loader, *before* this.
    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    pub fn decode(
        fmt: &Arc<Format>,
        id: Option<BlockId>,
        bytes: Bytes,
    ) -> Result<NodeView, TreeError> {
        let err = |reason| TreeError::decode(id, reason);
        let header = DecodedHeader::read(fmt, id, &bytes)?;
        let blobs = header.decode_blobs(fmt, id, &bytes)?;
        header.check_padding(fmt, id, &bytes, &blobs)?;
        let view = NodeView {
            bytes,
            fmt: fmt.clone(),
            s: header.sections,
            kind: header.kind,
            tree_level: header.tree_level,
            entry_count: header.entry_count,
            pivot_count: header.pivot_count,
            child_count: header.child_count,
            entry_head_skip: header.entry_head_skip,
            pivot_head_skip: header.pivot_head_skip,
            entry_blob_len: blobs.entry_blob_len,
            pivot_blob_len: blobs.pivot_blob_len,
            pivots: blobs.pivots,
            entry_keys: blobs.entry_keys,
            entry_prefix_len: blobs.entry_prefix_len,
        };

        if view.is_leaf() {
            let regular_blob = (0..view.entry_count)
                .map(|i| view.entry_key(i).len() + view.entry_span(i).len())
                .sum();
            let expected = fmt
                .leaf_kind(view.entry_count, regular_blob, view.entry_blob_len)
                .ok_or_else(|| {
                    err(DecodeError::OffsetRange {
                        found: view.entry_blob_len as u64,
                        capacity: view.s.entry_blob_cap as u64,
                    })
                })?;
            if expected != view.kind {
                return Err(err(DecodeError::LeafKind {
                    found: view.kind as u8,
                    expected: expected as u8,
                }));
            }
        }

        // --- Per-entry semantics, key order, key size, and recomputed derived fields. ---
        view.check_entries(id)?;
        view.check_keys_and_heads(id)?;
        Ok(view)
    }

    fn check_entries(&self, id: Option<BlockId>) -> Result<(), TreeError> {
        let err = |reason| TreeError::decode(id, reason);
        for i in 0..self.entry_count {
            let op = self.bytes[self.s.ops + i];
            let span = self.entry_span(i);
            let ext = self.entry_external_len(i);
            match op {
                OP_TOMBSTONE => {
                    if !span.is_empty() {
                        return Err(err(DecodeError::Span {
                            index: i,
                            why: "a tombstone must have an empty value span",
                        }));
                    }
                    if ext != 0 {
                        return Err(err(DecodeError::ExternalLen {
                            index: i,
                            why: "must be zero for a tombstone",
                        }));
                    }
                }
                OP_INLINE => {
                    if span.len() > self.fmt.inline_value_bytes() {
                        return Err(err(DecodeError::Span {
                            index: i,
                            why: "an inline value exceeds inline_value_bytes",
                        }));
                    }
                    if ext != 0 {
                        return Err(err(DecodeError::ExternalLen {
                            index: i,
                            why: "must be zero for an inline value",
                        }));
                    }
                }
                OP_EXTERNAL => {
                    if span.len() != EXTERNAL_ID_BYTES {
                        return Err(err(DecodeError::Span {
                            index: i,
                            why: "an out-of-line value span must be exactly 32 bytes",
                        }));
                    }
                    // The inline threshold is deterministic, so a value that *could* have been inline
                    // must not be out-of-line: that would be a second representation.
                    if (ext as usize) <= self.fmt.inline_value_bytes() {
                        return Err(err(DecodeError::ExternalLen {
                            index: i,
                            why: "at or below the inline threshold, so it must be stored inline",
                        }));
                    }
                    if ext as usize > self.fmt.max_value_bytes() {
                        return Err(err(DecodeError::ExternalLen {
                            index: i,
                            why: "above max_value_bytes",
                        }));
                    }
                }
                other => {
                    return Err(err(DecodeError::Op {
                        op: other,
                        index: i,
                    }));
                }
            }
        }
        Ok(())
    }

    /// Strict key order, key-size limits, and the *derived* fields: `head_skip` and every stored head
    /// are recomputed from the blob. A node that is memory-safe but internally inconsistent would
    /// otherwise return wrong search results with no UB — the bug class bounds-checking cannot see.
    fn check_keys_and_heads(&self, id: Option<BlockId>) -> Result<(), TreeError> {
        self.check_surface(
            id,
            "entry keys",
            self.entry_count,
            self.entry_head_skip,
            &|i| self.entry_key(i),
            &|i| self.raw_entry_head(i),
        )?;
        self.check_surface(
            id,
            "pivots",
            self.pivot_count,
            self.pivot_head_skip,
            &|i| self.pivot(i),
            &|i| self.raw_pivot_head(i),
        )
    }

    fn check_surface<'a>(
        &'a self,
        id: Option<BlockId>,
        what: &'static str,
        count: usize,
        skip: usize,
        key_at: &dyn Fn(usize) -> &'a [u8],
        head_at: &dyn Fn(usize) -> u64,
    ) -> Result<(), TreeError> {
        let err = |reason| TreeError::decode(id, reason);
        for i in 0..count {
            let k = key_at(i);
            if k.len() > self.fmt.max_key_bytes() {
                return Err(err(DecodeError::KeyTooLarge {
                    len: k.len(),
                    limit: self.fmt.max_key_bytes(),
                }));
            }
            if i + 1 < count && k >= key_at(i + 1) {
                return Err(err(DecodeError::Order { what, index: i }));
            }
            if head_at(i) != search::head_of(k, skip) {
                return Err(err(DecodeError::Head { index: i }));
            }
        }
        let expected = search::head_skip(
            (count > 0).then(|| key_at(0)),
            (count > 0).then(|| key_at(count - 1)),
            count,
        );
        if expected as usize != skip {
            return Err(err(DecodeError::HeadSkip {
                found: skip as u32,
                expected,
            }));
        }
        Ok(())
    }

    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }
    pub fn format(&self) -> &Arc<Format> {
        &self.fmt
    }
    pub fn kind(&self) -> NodeKind {
        self.kind
    }
    pub fn is_leaf(&self) -> bool {
        self.kind != NodeKind::Internal
    }
    pub fn tree_level(&self) -> u16 {
        self.tree_level
    }
    pub fn entry_count(&self) -> usize {
        self.entry_count
    }
    pub fn pivot_count(&self) -> usize {
        self.pivot_count
    }
    pub fn child_count(&self) -> usize {
        self.child_count
    }
    pub fn entry_blob_len(&self) -> usize {
        self.entry_blob_len
    }
    pub fn pivot_blob_len(&self) -> usize {
        self.pivot_blob_len
    }

    /// Heap bytes used to reconstruct prefix-compressed keys. Included in cache weighting so a 64 KiB
    /// encoded node with many 4 KiB logical keys cannot masquerade as a 64 KiB cache entry.
    pub fn materialized_key_bytes(&self) -> usize {
        let entries = if self.entry_prefix_len == 0 {
            0
        } else {
            self.entry_keys.iter().map(Bytes::len).sum()
        };
        let pivots = if self.pivot_head_skip == 0 {
            0
        } else {
            self.pivots.iter().map(Bytes::len).sum()
        };
        entries + pivots
    }

    pub fn logical_entry_blob_len(&self) -> usize {
        (0..self.entry_count)
            .map(|i| self.entry_key(i).len() + self.entry_span(i).len())
            .sum()
    }

    /// Accounted bytes this node's entries occupy — what flush accounting and occupancy metrics use.
    pub fn accounted_bytes(&self) -> usize {
        self.entry_count * self.fmt.desc_bytes() + self.entry_blob_len
    }

    fn offset(&self, base: usize, i: usize) -> usize {
        get_u32(&self.bytes, base + i * 4..base + i * 4 + 4) as usize
    }

    pub fn entry_key(&self, i: usize) -> &[u8] {
        if self.entry_prefix_len == 0 {
            let (a, b) = (
                self.offset(self.s.entry_offsets, 2 * i),
                self.offset(self.s.entry_offsets, 2 * i + 1),
            );
            &self.bytes[self.s.entry_blob + a..self.s.entry_blob + b]
        } else {
            &self.entry_keys[i]
        }
    }

    pub fn entry_span(&self, i: usize) -> &[u8] {
        let (a, b) = (
            self.offset(self.s.entry_offsets, 2 * i + 1),
            self.offset(self.s.entry_offsets, 2 * i + 2),
        );
        &self.bytes[self.s.entry_blob + self.entry_prefix_len + a
            ..self.s.entry_blob + self.entry_prefix_len + b]
    }

    /// Zero-copy: the returned `Bytes` shares this node's allocation.
    pub fn entry_key_bytes(&self, i: usize) -> Bytes {
        if self.entry_prefix_len == 0 {
            let (a, b) = (
                self.offset(self.s.entry_offsets, 2 * i),
                self.offset(self.s.entry_offsets, 2 * i + 1),
            );
            self.bytes
                .slice(self.s.entry_blob + a..self.s.entry_blob + b)
        } else {
            self.entry_keys[i].clone()
        }
    }

    pub fn entry_span_bytes(&self, i: usize) -> Bytes {
        let (a, b) = (
            self.offset(self.s.entry_offsets, 2 * i + 1),
            self.offset(self.s.entry_offsets, 2 * i + 2),
        );
        self.bytes.slice(
            self.s.entry_blob + self.entry_prefix_len + a
                ..self.s.entry_blob + self.entry_prefix_len + b,
        )
    }

    pub fn entry_order_key(&self, i: usize) -> [u8; VERSION_BYTES] {
        self.bytes[self.s.order_key(i)]
            .try_into()
            .expect("VERSION_BYTES")
    }

    pub fn entry_op(&self, i: usize) -> u8 {
        self.bytes[self.s.ops + i]
    }

    pub fn entry_external_len(&self, i: usize) -> u32 {
        get_u32(&self.bytes, self.s.external_len(i))
    }

    pub fn entry(&self, i: usize) -> Entry {
        Entry {
            key: self.entry_key_bytes(i),
            order_key: self.entry_order_key(i),
            op: self.entry_op(i),
            span: self.entry_span_bytes(i),
            external_len: self.entry_external_len(i),
        }
    }

    pub(crate) fn winner(&self, i: usize) -> Winner {
        Winner {
            order_key: self.entry_order_key(i),
            op: match self.entry_op(i) {
                OP_INLINE => WinnerOp::Inline(self.entry_span_bytes(i)),
                OP_EXTERNAL => WinnerOp::External {
                    id: BlockId(self.entry_span(i).try_into().expect("validated 32 bytes")),
                    len: self.entry_external_len(i),
                },
                _ => WinnerOp::Tombstone,
            },
        }
    }

    pub fn entries(&self) -> impl Iterator<Item = Entry> + '_ {
        (0..self.entry_count).map(|i| self.entry(i))
    }

    pub fn pivot(&self, i: usize) -> &[u8] {
        if self.pivot_head_skip == 0 {
            let (a, b) = (
                self.offset(self.s.pivot_offsets, i),
                self.offset(self.s.pivot_offsets, i + 1),
            );
            &self.bytes[self.s.pivot_blob + a..self.s.pivot_blob + b]
        } else {
            &self.pivots[i]
        }
    }

    pub fn pivot_bytes(&self, i: usize) -> Bytes {
        if self.pivot_head_skip == 0 {
            let (a, b) = (
                self.offset(self.s.pivot_offsets, i),
                self.offset(self.s.pivot_offsets, i + 1),
            );
            self.bytes
                .slice(self.s.pivot_blob + a..self.s.pivot_blob + b)
        } else {
            self.pivots[i].clone()
        }
    }

    pub fn child(&self, i: usize) -> BlockId {
        BlockId(self.bytes[self.s.child_id(i)].try_into().expect("32 bytes"))
    }

    pub fn children(&self) -> impl Iterator<Item = BlockId> + '_ {
        (0..self.child_count).map(|i| self.child(i))
    }

    /// Every `BlockId` this node references: child nodes **and** out-of-line value objects. A GC
    /// cannot infer reachability by parsing only child ids, so both are returned together.
    pub fn references(&self) -> Vec<(crate::ObjectKind, BlockId)> {
        let mut out = Vec::with_capacity(self.child_count + self.entry_count);
        for i in 0..self.child_count {
            out.push((crate::ObjectKind::Node, self.child(i)));
        }
        for i in 0..self.entry_count {
            if self.entry_op(i) == OP_EXTERNAL {
                out.push((
                    crate::ObjectKind::Value,
                    BlockId(self.entry_span(i).try_into().expect("validated 32 bytes")),
                ));
            }
        }
        out
    }

    fn raw_entry_head(&self, i: usize) -> u64 {
        u64::from_le_bytes(
            self.bytes[self.s.entry_head(i)]
                .try_into()
                .expect("8 bytes"),
        )
    }
    fn raw_pivot_head(&self, i: usize) -> u64 {
        u64::from_le_bytes(
            self.bytes[self.s.pivot_head(i)]
                .try_into()
                .expect("8 bytes"),
        )
    }

    /// The entry search surface, reused across every probe assigned to this node in a wave.
    pub fn entry_surface<'a>(&'a self) -> Surface<'a, impl Fn(usize) -> &'a [u8] + 'a> {
        let skip = self.entry_head_skip;
        Surface {
            heads: &self.bytes
                [self.s.entry_heads..self.s.entry_heads + self.entry_count * HEAD_BYTES],
            count: self.entry_count,
            skip,
            prefix: if self.entry_count == 0 {
                &[]
            } else {
                let k = self.entry_key(0);
                &k[..skip.min(k.len())]
            },
            key: move |i: usize| self.entry_key(i),
        }
    }

    /// The pivot search surface. A buffer and its pivots are independent surfaces with independent
    /// skips, because they are independently distributed key sets.
    pub fn pivot_surface<'a>(&'a self) -> Surface<'a, impl Fn(usize) -> &'a [u8] + 'a> {
        let skip = self.pivot_head_skip;
        Surface {
            heads: &self.bytes
                [self.s.pivot_heads..self.s.pivot_heads + self.pivot_count * HEAD_BYTES],
            count: self.pivot_count,
            skip,
            prefix: if self.pivot_count == 0 {
                &[]
            } else {
                let p = self.pivot(0);
                &p[..skip.min(p.len())]
            },
            key: move |i: usize| self.pivot(i),
        }
    }

    /// Which child owns `key`: the count of pivots `<= key`.
    pub fn child_of(&self, key: &[u8]) -> usize {
        self.pivot_surface().probe(key).upper_bound()
    }

    /// Find `key` in this node's entries.
    pub fn find(&self, key: &[u8]) -> Found {
        self.entry_surface().probe(key)
    }

    /// Half-open key range owned by child `i`, derived from the pivots.
    pub fn child_range(&self, i: usize) -> (Option<&[u8]>, Option<&[u8]>) {
        let lo = i.checked_sub(1).map(|j| self.pivot(j));
        let hi = (i < self.pivot_count).then(|| self.pivot(i));
        (lo, hi)
    }
}

#[cfg(test)]
#[path = "../tests/support/codec_unit.rs"]
mod tests;
