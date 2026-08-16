//! The format constants and the byte layout they imply.
//!
//! A [`Format`] is *proved once* at construction: the section table, the pivot/leaf worst-case
//! reservations, and the two flush inequalities are checked there, so the write path contains explicit
//! capacity branches rather than accidental failures. A configuration that cannot satisfy them is
//! rejected — never patched up at runtime with an undersized flush.
//!
//! Every regular node serializes to *exactly* [`Format::node_bytes`]. Section offsets within the
//! serialized byte string are multiples of [`SECTION_ALIGN`] where that helps a vector load; this
//! asserts nothing about the alignment of the `Bytes` allocation the store hands back.

use crate::{CapacityError, VERSION_BYTES};

/// Fixed header size, and the alignment every following section starts on.
pub const HEADER_BYTES: usize = 64;
/// Section starts are multiples of this within the serialized byte string.
pub const SECTION_ALIGN: usize = 64;
/// Width of one key head, in bytes. The head is the eight bytes at `skip`, zero-padded right and read
/// as a big-endian `u64` — Masstree's "most valuable coding trick", and order-preserving.
pub const HEAD_BYTES: usize = 8;
/// An out-of-line value's span is exactly one 32-byte `ValueObject` id.
pub const EXTERNAL_ID_BYTES: usize = 32;

/// Node object-type domain tag. Disjoint from [`crate::value::VALUE_MAGIC`], so the two canonical byte
/// languages cannot be confused before hashing.
pub const NODE_MAGIC: [u8; 8] = *b"BeTreeN1";

/// `op` column byte values. Their numeric order **is** the leading discriminant of the format's
/// `operation_tiebreak`, so delete wins a reused order key.
pub const OP_INLINE: u8 = 0;
pub const OP_EXTERNAL: u8 = 1;
pub const OP_TOMBSTONE: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Leaf = 0,
    Internal = 1,
    /// Same 64 KiB object size with fewer fixed descriptor lanes and a larger blob. Selected
    /// canonically only when a long-key leaf cannot use the regular 640-lane layout.
    CompactLeaf = 2,
}

impl NodeKind {
    pub fn from_byte(b: u8) -> Option<NodeKind> {
        match b {
            0 => Some(NodeKind::Leaf),
            1 => Some(NodeKind::Internal),
            2 => Some(NodeKind::CompactLeaf),
            _ => None,
        }
    }
}

/// The knobs the capacity matrix sweeps. `Format::new` turns these into a validated layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatParams {
    /// Serialized size of a regular node. A format constant, not a property of current contents.
    pub node_bytes: usize,
    /// Maximum children in a regular internal node.
    pub f_max: usize,
    /// Descriptor capacity of a leaf.
    pub leaf_slots: usize,
    /// Descriptor capacity of an internal buffer.
    pub message_slots: usize,
    pub max_key_bytes: usize,
    /// Values at or below this width live in the node; larger ones become `ValueObject`s.
    pub inline_value_bytes: usize,
    pub max_value_bytes: usize,
    pub max_object_bytes: usize,
    /// Hard bound on authenticated tree depth, and therefore on dependent read waves.
    pub max_tree_level: u16,
    /// Producer-protocol domain. Two protocols cannot be mixed accidentally.
    pub version_domain: [u8; 16],
}

/// Where each column of one node kind begins, and how big the variable blob region is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sections {
    pub slots: usize,
    pub entry_heads: usize,
    pub pivot_heads: usize,
    pub child_ids: usize,
    pub pivot_offsets: usize,
    pub entry_offsets: usize,
    pub order_keys: usize,
    pub ops: usize,
    pub external_lens: usize,
    pub pivot_blob: usize,
    pub pivot_blob_cap: usize,
    pub entry_blob: usize,
    pub entry_blob_cap: usize,
}

impl Sections {
    /// Byte range of the `i`th 8-byte entry head.
    pub fn entry_head(&self, i: usize) -> std::ops::Range<usize> {
        let at = self.entry_heads + i * HEAD_BYTES;
        at..at + HEAD_BYTES
    }
    pub fn pivot_head(&self, i: usize) -> std::ops::Range<usize> {
        let at = self.pivot_heads + i * HEAD_BYTES;
        at..at + HEAD_BYTES
    }
    pub fn child_id(&self, i: usize) -> std::ops::Range<usize> {
        let at = self.child_ids + i * 32;
        at..at + 32
    }
    pub fn pivot_offset(&self, i: usize) -> std::ops::Range<usize> {
        let at = self.pivot_offsets + i * 4;
        at..at + 4
    }
    pub fn entry_offset(&self, i: usize) -> std::ops::Range<usize> {
        let at = self.entry_offsets + i * 4;
        at..at + 4
    }
    pub fn order_key(&self, i: usize) -> std::ops::Range<usize> {
        let at = self.order_keys + i * VERSION_BYTES;
        at..at + VERSION_BYTES
    }
    pub fn external_len(&self, i: usize) -> std::ops::Range<usize> {
        let at = self.external_lens + i * 4;
        at..at + 4
    }
}

/// Width of the exact schema identifier. 128 bits of BLAKE3 over the layout parameters: wide enough that
/// a collision between two *distinct* layouts is not a design concern.
///
/// The first revision of this format truncated the digest to 16 bits, leaving 32 768 effective values —
/// and a small sweep over valid configurations found a real collision (`400/525` against `400/596`
/// slots), which meant a node of one layout could be accepted as another. A truncated hash is not an
/// exact identifier, so it is not one here.
pub const SCHEMA_ID_BYTES: usize = 16;

/// The exact identifier of one encoding.
pub type SchemaId = [u8; SCHEMA_ID_BYTES];

/// Canonical node layout revision. `V2` is retained for decoding and migration so roots written by
/// the first released fixed-pivot format remain usable; the selected format uses `V3`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutVersion {
    V2FixedPivots,
    V3PrefixCompressedPivots,
}

/// A validated format: parameters, derived layout, derived flush floor, and the exact schema id that
/// names this one encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Format {
    params: FormatParams,
    layout: LayoutVersion,
    schema_id: SchemaId,
    leaf: Sections,
    compact_leaf: Sections,
    internal: Sections,
    /// Accounted bytes a message occupies before its key and value span: the fixed descriptor cost.
    desc_bytes: usize,
    /// Descriptors + blob reserved for buffered messages in a regular internal node.
    buffer_bytes: usize,
    /// Smallest ordinary child group worth rewriting under COW; derived so that *either* fullness
    /// trigger guarantees some child owns at least this many pending bytes.
    min_flush_bytes: usize,
}

fn align_up(x: usize, to: usize) -> Option<usize> {
    x.checked_next_multiple_of(to)
}

fn validate_params(p: &FormatParams) -> Result<(), CapacityError> {
    let bad = |message: String| CapacityError::Format(message);
    if !(3..=usize::from(u16::MAX)).contains(&p.f_max) {
        return Err(bad(format!("f_max {} must be in 3..=65535", p.f_max)));
    }
    if !p.node_bytes.is_multiple_of(SECTION_ALIGN) {
        return Err(bad(format!(
            "node_bytes {} must be a multiple of {SECTION_ALIGN}",
            p.node_bytes
        )));
    }
    if p.node_bytes > p.max_object_bytes {
        return Err(bad(format!(
            "node_bytes {} exceeds max_object_bytes {}",
            p.node_bytes, p.max_object_bytes
        )));
    }
    if p.max_object_bytes >= 1usize << 32 {
        return Err(bad(format!(
            "max_object_bytes {} must be below 2^32",
            p.max_object_bytes
        )));
    }
    let value_object_bytes = p
        .max_value_bytes
        .checked_add(crate::value::ENVELOPE_BYTES)
        .ok_or_else(|| bad(format!("max_value_bytes {} overflows", p.max_value_bytes)))?;
    if value_object_bytes > p.max_object_bytes {
        return Err(bad(format!(
            "a max-size value plus its {}-byte envelope ({value_object_bytes}) does not fit \
             max_object_bytes {}",
            crate::value::ENVELOPE_BYTES,
            p.max_object_bytes
        )));
    }
    if p.inline_value_bytes > p.max_value_bytes {
        return Err(bad("inline_value_bytes exceeds max_value_bytes".into()));
    }
    if p.max_key_bytes == 0 {
        return Err(bad("max_key_bytes must be nonzero".into()));
    }
    if p.leaf_slots == 0 || p.message_slots == 0 {
        return Err(bad("slot capacities must be nonzero".into()));
    }
    if p.max_tree_level == 0 {
        return Err(bad("max_tree_level must be nonzero".into()));
    }
    for (what, value) in [
        ("leaf_slots", p.leaf_slots as u64),
        ("message_slots", p.message_slots as u64),
        ("node_bytes", p.node_bytes as u64),
        ("max_key_bytes", p.max_key_bytes as u64),
        ("max_value_bytes", p.max_value_bytes as u64),
    ] {
        if value > u64::from(u32::MAX) {
            return Err(bad(format!("{what} does not fit a u32 field")));
        }
    }
    if VERSION_BYTES == 0 {
        return Err(bad("VERSION_BYTES must be nonzero".into()));
    }
    Ok(())
}

impl Format {
    /// The selected default, chosen from the capacity matrix in `benches/`.
    pub fn selected() -> Format {
        Format::new(FormatParams {
            node_bytes: 64 * 1024,
            f_max: 32,
            leaf_slots: 640,
            message_slots: 640,
            max_key_bytes: 4 * 1024,
            inline_value_bytes: 512,
            max_value_bytes: (4 << 20) - crate::value::ENVELOPE_BYTES,
            max_object_bytes: 4 << 20,
            max_tree_level: 32,
            version_domain: *b"be-tree/default\0",
        })
        .expect("the selected default format is valid by construction")
    }

    /// A deliberately tiny format, so tests exercise splits, multiway propagation, and root growth
    /// without writing megabytes. Same code paths, different constants.
    #[doc(hidden)]
    pub fn tiny() -> Format {
        Format::new(FormatParams {
            node_bytes: 4096,
            f_max: 4,
            leaf_slots: 16,
            message_slots: 16,
            max_key_bytes: 64,
            inline_value_bytes: 32,
            max_value_bytes: 1 << 16,
            max_object_bytes: 1 << 17,
            max_tree_level: 16,
            version_domain: *b"be-tree/tiny\0\0\0\0",
        })
        .expect("the tiny test format is valid by construction")
    }

    /// Validate a configuration and derive its layout. Every rejection here is a *format proof*: the
    /// write path may then assume a maximum-size key, a maximum inline value, and a legal flush all
    /// fit.
    pub fn new(p: FormatParams) -> Result<Format, CapacityError> {
        Self::new_for_layout(p, LayoutVersion::V3PrefixCompressedPivots)
    }

    /// The exact selected format shipped before prefix-compressed pivots. It is deliberately not a
    /// write default; it exists so immutable historical roots can be opened and migrated. Explicit
    /// `with_format` callers may still construct fixtures or continue writing it during a rollout.
    #[doc(hidden)]
    pub fn legacy_selected_v2() -> Format {
        Self::new_for_layout(
            FormatParams {
                node_bytes: 64 * 1024,
                f_max: 32,
                leaf_slots: 640,
                message_slots: 640,
                max_key_bytes: 256,
                inline_value_bytes: 512,
                max_value_bytes: (4 << 20) - crate::value::ENVELOPE_BYTES,
                max_object_bytes: 4 << 20,
                max_tree_level: 32,
                version_domain: *b"be-tree/default\0",
            },
            LayoutVersion::V2FixedPivots,
        )
        .expect("the legacy selected format remains valid")
    }

    fn new_for_layout(p: FormatParams, layout: LayoutVersion) -> Result<Format, CapacityError> {
        let bad = |m: String| CapacityError::Format(m);
        validate_params(&p)?;

        let leaf = Self::sections(&p, layout, NodeKind::Leaf, p.leaf_slots)?;
        let compact_slots = if layout == LayoutVersion::V3PrefixCompressedPivots {
            p.leaf_slots
                .min((p.node_bytes / p.max_key_bytes.max(1)).max(4))
        } else {
            p.leaf_slots
        };
        let compact_leaf = Self::sections(&p, layout, NodeKind::CompactLeaf, compact_slots)?;
        let internal = Self::sections(&p, layout, NodeKind::Internal, p.message_slots)?;

        // Worst-case reservations: with these, every legal key and every structural node is
        // representable as a regular node — there is no dynamically sized overflow shape.
        let leaf_worst = p.max_key_bytes + p.inline_value_bytes.max(EXTERNAL_ID_BYTES);
        if leaf.entry_blob_cap.max(compact_leaf.entry_blob_cap) < leaf_worst {
            return Err(bad(format!(
                "leaf blob region {} cannot hold one maximum entry ({leaf_worst} bytes)",
                leaf.entry_blob_cap
            )));
        }
        let required_pivots = match layout {
            LayoutVersion::V2FixedPivots => (p.f_max - 1)
                .checked_mul(p.max_key_bytes)
                .ok_or_else(|| bad("pivot reservation overflows".into()))?,
            // Three children are the minimum useful regular internal node. V3 may reduce realized
            // fanout for adversarial unrelated long separators, but two maximum-size pivots must
            // always fit so partitioning can make progress without an overflow-node shape.
            LayoutVersion::V3PrefixCompressedPivots => p
                .max_key_bytes
                .checked_mul(2)
                .ok_or_else(|| bad("minimum pivot reservation overflows".into()))?,
        };
        if internal.pivot_blob_cap < required_pivots {
            return Err(bad(format!(
                "pivot region {} cannot hold the required {required_pivots} bytes",
                internal.pivot_blob_cap
            )));
        }
        if internal.entry_blob_cap == 0 {
            return Err(bad("internal buffer blob region is empty".into()));
        }

        // Descriptor cost of one buffered message: head + its two u32 offsets + order key + op byte +
        // external length. This is `min_accounted_message_bytes` (an empty key and empty span).
        let desc_bytes = HEAD_BYTES + 8 + VERSION_BYTES + 1 + 4;
        let slot_bytes = p
            .message_slots
            .checked_mul(desc_bytes)
            .ok_or_else(|| bad("message_slots x descriptor bytes overflows".into()))?;
        let buffer_bytes = slot_bytes
            .checked_add(internal.entry_blob_cap)
            .ok_or_else(|| bad("buffer byte total overflows".into()))?;

        // Either ordinary fullness trigger must guarantee that some child owns at least
        // `min_flush_bytes`: the slot trigger gives `message_slots * desc_bytes` accounted bytes, the
        // blob trigger gives `entry_blob_cap`. Take the floor of the weaker one.
        let min_flush_bytes = slot_bytes.min(internal.entry_blob_cap) / p.f_max;
        if min_flush_bytes == 0 {
            return Err(bad(format!(
                "no positive min_flush_bytes: buffer_bytes {buffer_bytes} / f_max {} is degenerate",
                p.f_max
            )));
        }
        // The format's two inequalities, restated over the derived floor.
        debug_assert!(buffer_bytes >= p.f_max * min_flush_bytes);
        debug_assert!(p.message_slots * desc_bytes >= p.f_max * min_flush_bytes);

        // The schema id names exactly one encoding: derive it from every parameter that changes bytes.
        let schema_id = Self::derive_schema_id(&p, layout);

        Ok(Format {
            params: p,
            layout,
            schema_id,
            leaf,
            compact_leaf,
            internal,
            desc_bytes,
            buffer_bytes,
            min_flush_bytes,
        })
    }

    /// An exact schema identifier, not a compatibility version: two configurations that lay bytes out
    /// differently get different ids, and decode rejects any id but its own.
    ///
    /// `version_domain` is deliberately **excluded**. It does not change the layout, and a node from
    /// another producer protocol must surface as [`crate::TreeError::VersionDomainMismatch`] — a
    /// structurally valid node built by someone else — rather than being indistinguishable from
    /// garbage with the wrong `schema_id`.
    fn derive_schema_id(p: &FormatParams, layout: LayoutVersion) -> SchemaId {
        let mut h = blake3::Hasher::new();
        h.update(match layout {
            LayoutVersion::V2FixedPivots => b"be-tree/format/v2",
            LayoutVersion::V3PrefixCompressedPivots => b"be-tree/format/v3",
        });
        // Length-prefix nothing: every field is a fixed-width little-endian u64, so the input is
        // unambiguous by construction and two distinct parameter sets cannot produce the same preimage.
        for v in [
            p.node_bytes as u64,
            p.f_max as u64,
            p.leaf_slots as u64,
            p.message_slots as u64,
            p.max_key_bytes as u64,
            p.inline_value_bytes as u64,
            p.max_value_bytes as u64,
            p.max_object_bytes as u64,
            u64::from(p.max_tree_level),
            VERSION_BYTES as u64,
        ] {
            h.update(&v.to_le_bytes());
        }
        let d = h.finalize();
        d.as_bytes()[..SCHEMA_ID_BYTES]
            .try_into()
            .expect("SCHEMA_ID_BYTES <= 32")
    }

    fn sections(
        p: &FormatParams,
        layout: LayoutVersion,
        kind: NodeKind,
        slots: usize,
    ) -> Result<Sections, CapacityError> {
        let internal = kind == NodeKind::Internal;
        let mut at = HEADER_BYTES;
        // Every step is checked. Slot counts and key limits are caller-supplied, so a hostile or merely
        // careless configuration must produce a typed rejection, never a wrapped offset table.
        let overflow = || CapacityError::Format("format layout arithmetic overflows".into());
        let take = |n: usize, at: &mut usize| -> Result<usize, CapacityError> {
            let start = *at;
            *at = align_up(start.checked_add(n).ok_or_else(overflow)?, SECTION_ALIGN)
                .ok_or_else(overflow)?;
            Ok(start)
        };
        let mul = |a: usize, b: usize| a.checked_mul(b).ok_or_else(overflow);

        let entry_heads = take(mul(slots, HEAD_BYTES)?, &mut at)?;
        let (pivot_heads, child_ids, pivot_offsets) = if internal {
            let ph = take(mul(p.f_max - 1, HEAD_BYTES)?, &mut at)?;
            let ci = take(mul(p.f_max, 32)?, &mut at)?;
            let po = take(mul(p.f_max, 4)?, &mut at)?;
            (ph, ci, po)
        } else {
            (0, 0, 0)
        };
        let entry_offsets = take(
            mul(slots, 2)?.checked_add(1).ok_or_else(overflow)? * 4,
            &mut at,
        )?;
        let order_keys = take(mul(slots, VERSION_BYTES)?, &mut at)?;
        let ops = take(slots, &mut at)?;
        let external_lens = take(mul(slots, 4)?, &mut at)?;
        let (pivot_blob, pivot_blob_cap) = if internal {
            let raw_cap = match layout {
                LayoutVersion::V2FixedPivots => mul(p.f_max - 1, p.max_key_bytes)?,
                LayoutVersion::V3PrefixCompressedPivots => {
                    // Keep the selected format's old ~8 KiB pivot budget instead of multiplying the
                    // public 4 KiB key limit by every fanout lane. Prefix compression preserves full
                    // fanout for shared-prefix keys; byte-aware partitioning handles hostile tails.
                    mul(p.f_max - 1, p.max_key_bytes.min(256))?.max(mul(2, p.max_key_bytes)?)
                }
            };
            let cap = align_up(raw_cap, SECTION_ALIGN).ok_or_else(overflow)?;
            let start = take(cap, &mut at)?;
            (start, cap)
        } else {
            (0, 0)
        };
        let entry_blob = at;
        let entry_blob_cap = p.node_bytes.checked_sub(entry_blob).ok_or_else(|| {
            CapacityError::Format(format!(
                "node_bytes {} is smaller than the {:?} fixed sections ({entry_blob} bytes)",
                p.node_bytes, kind
            ))
        })?;

        Ok(Sections {
            slots,
            entry_heads,
            pivot_heads,
            child_ids,
            pivot_offsets,
            entry_offsets,
            order_keys,
            ops,
            external_lens,
            pivot_blob,
            pivot_blob_cap,
            entry_blob,
            entry_blob_cap,
        })
    }

    pub fn params(&self) -> &FormatParams {
        &self.params
    }
    pub fn schema_id(&self) -> &SchemaId {
        &self.schema_id
    }
    #[doc(hidden)]
    pub fn layout_version(&self) -> LayoutVersion {
        self.layout
    }
    #[doc(hidden)]
    pub fn compresses_pivots(&self) -> bool {
        self.layout == LayoutVersion::V3PrefixCompressedPivots
    }

    /// Resolve a schema emitted by a released be-tree format. This registry is the durable migration
    /// boundary: a released entry is not removed while immutable roots may still name it.
    #[doc(hidden)]
    pub fn known_schema(schema: &SchemaId) -> Option<Format> {
        Self::known_schemas().find(|format| format.schema_id() == schema)
    }

    /// The one released-schema registry used for both decoder selection and safe retrieval bounds.
    /// Keeping the enumeration here prevents `open_known` and `known_schema` from drifting apart.
    pub(crate) fn known_schemas() -> impl Iterator<Item = Format> {
        std::iter::once(Format::selected()).chain(std::iter::once(Format::legacy_selected_v2()))
    }
    /// Short hex form, for error messages and test vectors.
    pub fn schema_hex(&self) -> String {
        self.schema_id.iter().map(|b| format!("{b:02x}")).collect()
    }
    pub fn node_bytes(&self) -> usize {
        self.params.node_bytes
    }
    pub fn f_max(&self) -> usize {
        self.params.f_max
    }
    pub fn leaf_slots(&self) -> usize {
        self.params.leaf_slots
    }
    pub fn message_slots(&self) -> usize {
        self.params.message_slots
    }
    pub fn max_key_bytes(&self) -> usize {
        self.params.max_key_bytes
    }
    pub fn inline_value_bytes(&self) -> usize {
        self.params.inline_value_bytes
    }
    pub fn max_value_bytes(&self) -> usize {
        self.params.max_value_bytes
    }
    pub fn max_object_bytes(&self) -> usize {
        self.params.max_object_bytes
    }
    pub fn max_tree_level(&self) -> u16 {
        self.params.max_tree_level
    }
    pub fn version_domain(&self) -> &[u8; 16] {
        &self.params.version_domain
    }
    #[doc(hidden)]
    pub fn desc_bytes(&self) -> usize {
        self.desc_bytes
    }
    #[doc(hidden)]
    pub fn buffer_bytes(&self) -> usize {
        self.buffer_bytes
    }
    pub fn min_flush_bytes(&self) -> usize {
        self.min_flush_bytes
    }

    #[doc(hidden)]
    pub fn sections_for(&self, kind: NodeKind) -> &Sections {
        match kind {
            NodeKind::Leaf => &self.leaf,
            NodeKind::Internal => &self.internal,
            NodeKind::CompactLeaf => &self.compact_leaf,
        }
    }
    #[doc(hidden)]
    pub fn leaf_sections(&self) -> &Sections {
        &self.leaf
    }
    #[doc(hidden)]
    pub fn compact_leaf_sections(&self) -> &Sections {
        &self.compact_leaf
    }
    #[doc(hidden)]
    pub fn internal_sections(&self) -> &Sections {
        &self.internal
    }

    /// Accounted bytes one entry occupies in a leaf or a buffer: descriptor + key + value span. An
    /// out-of-line value contributes its 32-byte id and its length field, never its payload.
    #[doc(hidden)]
    pub fn accounted(&self, key_len: usize, span_len: usize) -> usize {
        self.desc_bytes + key_len + span_len
    }

    /// Blob bytes one entry occupies: just the key and the value span.
    #[doc(hidden)]
    pub fn blob_of(key_len: usize, span_len: usize) -> usize {
        key_len + span_len
    }

    /// Can a leaf hold `count` entries whose key+span bytes total `blob`?
    #[doc(hidden)]
    pub fn leaf_fits(&self, count: usize, blob: usize) -> bool {
        (count <= self.params.leaf_slots && blob <= self.leaf.entry_blob_cap)
            || (self.layout == LayoutVersion::V3PrefixCompressedPivots
                && count <= self.compact_leaf.slots
                && blob <= self.compact_leaf.entry_blob_cap)
    }

    #[doc(hidden)]
    pub fn leaf_kind(
        &self,
        count: usize,
        regular_blob: usize,
        compact_blob: usize,
    ) -> Option<NodeKind> {
        if count <= self.params.leaf_slots && regular_blob <= self.leaf.entry_blob_cap {
            Some(NodeKind::Leaf)
        } else if self.layout == LayoutVersion::V3PrefixCompressedPivots
            && count <= self.compact_leaf.slots
            && compact_blob <= self.compact_leaf.entry_blob_cap
        {
            Some(NodeKind::CompactLeaf)
        } else {
            None
        }
    }

    #[doc(hidden)]
    pub fn leaf_entries_fit(&self, count: usize, regular_blob: usize, compact_blob: usize) -> bool {
        self.leaf_kind(count, regular_blob, compact_blob).is_some()
    }

    /// Can a regular internal buffer hold `count` messages totalling `blob` blob bytes?
    #[doc(hidden)]
    pub fn buffer_fits(&self, count: usize, blob: usize) -> bool {
        count <= self.params.message_slots && blob <= self.internal.entry_blob_cap
    }

    /// Canonical bytes occupied by a pivot surface. V3 stores its common prefix once and then one
    /// suffix per pivot; V2 stores every pivot in full.
    #[doc(hidden)]
    pub fn pivot_blob_len(&self, pivots: &[bytes::Bytes]) -> usize {
        if !self.compresses_pivots() || pivots.len() < 2 {
            return pivots.iter().map(bytes::Bytes::len).sum();
        }
        let skip = crate::search::head_skip(
            pivots.first().map(bytes::Bytes::as_ref),
            pivots.last().map(bytes::Bytes::as_ref),
            pivots.len(),
        ) as usize;
        skip + pivots.iter().map(|pivot| pivot.len() - skip).sum::<usize>()
    }

    #[doc(hidden)]
    pub fn pivots_fit(&self, pivots: &[bytes::Bytes]) -> bool {
        pivots.len() < self.f_max() && self.pivot_blob_len(pivots) <= self.internal.pivot_blob_cap
    }

    /// Would this single message fail to fit an *empty* regular buffer? Such a mutation is routed
    /// directly toward its leaf, sacrificing buffering for it rather than creating a second node shape.
    #[doc(hidden)]
    pub fn message_is_oversized(&self, key_len: usize, span_len: usize) -> bool {
        !self.buffer_fits(1, Self::blob_of(key_len, span_len))
    }

    /// The largest span an inline value may occupy. Values above [`Self::inline_value_bytes`] become
    /// `ValueObject`s, so the inline threshold is deterministic and one accepted format has one
    /// representation for a given value.
    #[doc(hidden)]
    pub fn is_inline(&self, value_len: usize) -> bool {
        value_len <= self.params.inline_value_bytes
    }

    pub fn check_key(&self, key: &[u8]) -> Result<(), CapacityError> {
        if key.len() > self.params.max_key_bytes {
            return Err(CapacityError::KeyTooLarge {
                len: key.len(),
                limit: self.params.max_key_bytes,
            });
        }
        Ok(())
    }

    pub fn check_value(&self, value: &[u8]) -> Result<(), CapacityError> {
        if value.len() > self.params.max_value_bytes {
            return Err(CapacityError::ValueTooLarge {
                len: value.len(),
                limit: self.params.max_value_bytes,
            });
        }
        Ok(())
    }

    /// Allocation-free preflight using exactly the predicates [`crate::BeTree::apply`] runs before it
    /// stages an object. Facades can call this while constructing a commit instead of discovering a
    /// limit after the full draft has been assembled.
    pub fn check_mutation(&self, mutation: &crate::Mutation) -> Result<(), CapacityError> {
        self.check_key(&mutation.key)?;
        if let crate::MutationOp::Upsert(value) = &mutation.op {
            self.check_value(value)?;
        }
        Ok(())
    }
}

impl Default for Format {
    fn default() -> Self {
        Self::selected()
    }
}

#[cfg(test)]
#[path = "../tests/support/format_unit.rs"]
mod tests;
