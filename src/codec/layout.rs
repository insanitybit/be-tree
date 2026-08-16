use crate::format::{Format, HEAD_BYTES, HEADER_BYTES, NodeKind, Sections};
use crate::{BlockId, DecodeError, TreeError};

/// Byte locations for the fixed node envelope.
pub(super) mod hdr {
    pub const MAGIC: std::ops::Range<usize> = 0..8;
    pub const SCHEMA_ID: std::ops::Range<usize> = 8..24;
    pub const KIND: usize = 24;
    pub const FLAGS: usize = 25;
    pub const TREE_LEVEL: std::ops::Range<usize> = 26..28;
    pub const PIVOT_COUNT: std::ops::Range<usize> = 28..30;
    pub const CHILD_COUNT: std::ops::Range<usize> = 30..32;
    pub const ENTRY_COUNT: std::ops::Range<usize> = 32..36;
    pub const ENTRY_HEAD_SKIP: std::ops::Range<usize> = 36..40;
    pub const PIVOT_HEAD_SKIP: std::ops::Range<usize> = 40..44;
    pub const RESERVED_A: std::ops::Range<usize> = 44..48;
    pub const VERSION_DOMAIN: std::ops::Range<usize> = 48..64;
}

pub(super) fn put_u16(buf: &mut [u8], at: std::ops::Range<usize>, v: u16) {
    buf[at].copy_from_slice(&v.to_le_bytes());
}

pub(super) fn put_u32(buf: &mut [u8], at: std::ops::Range<usize>, v: u32) {
    buf[at].copy_from_slice(&v.to_le_bytes());
}

pub(super) fn get_u16(buf: &[u8], at: std::ops::Range<usize>) -> u16 {
    u16::from_le_bytes(buf[at].try_into().expect("2 bytes"))
}

pub(super) fn get_u32(buf: &[u8], at: std::ops::Range<usize>) -> u32 {
    u32::from_le_bytes(buf[at].try_into().expect("4 bytes"))
}

pub(super) fn all_zero(b: &[u8]) -> bool {
    b.iter().all(|&x| x == 0)
}

pub(super) fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub(super) struct OffsetColumn {
    pub(super) base: usize,
    pub(super) live: usize,
    pub(super) slots: usize,
    pub(super) cap: usize,
}

/// Validate one monotonic offset column and return its used byte length.
pub(super) fn check_offsets(
    b: &[u8],
    id: Option<BlockId>,
    c: OffsetColumn,
) -> Result<usize, TreeError> {
    let err = |reason| TreeError::decode(id, reason);
    let get = |i: usize| get_u32(b, c.base + i * 4..c.base + i * 4 + 4) as u64;
    if get(0) != 0 {
        return Err(err(DecodeError::OffsetMonotonicity(0)));
    }
    let mut prev = 0u64;
    for i in 1..c.live {
        let v = get(i);
        if v < prev {
            return Err(err(DecodeError::OffsetMonotonicity(i)));
        }
        if v > c.cap as u64 {
            return Err(err(DecodeError::OffsetRange {
                found: v,
                capacity: c.cap as u64,
            }));
        }
        prev = v;
    }
    if !all_zero(&b[c.base + c.live * 4..c.base + c.slots * 4]) {
        return Err(err(DecodeError::Padding("unused offset slots")));
    }
    Ok(prev as usize)
}

/// Return the alignment gaps that the canonical encoder must leave zero.
pub(super) fn section_gaps(fmt: &Format, kind: NodeKind) -> Vec<std::ops::Range<usize>> {
    let s: &Sections = fmt.sections_for(kind);
    let internal = kind == NodeKind::Internal;
    let mut spans: Vec<(usize, usize)> = vec![
        (0, HEADER_BYTES),
        (s.entry_heads, s.slots * HEAD_BYTES),
        (s.entry_offsets, (2 * s.slots + 1) * 4),
        (s.order_keys, s.slots * crate::VERSION_BYTES),
        (s.ops, s.slots),
        (s.external_lens, s.slots * 4),
        (s.entry_blob, s.entry_blob_cap),
    ];
    if internal {
        spans.push((s.pivot_heads, (fmt.f_max() - 1) * HEAD_BYTES));
        spans.push((s.child_ids, fmt.f_max() * 32));
        spans.push((s.pivot_offsets, fmt.f_max() * 4));
        spans.push((s.pivot_blob, s.pivot_blob_cap));
    }
    spans.sort();
    let mut gaps = Vec::new();
    let mut at = 0usize;
    for (start, len) in spans {
        if start > at {
            gaps.push(at..start);
        }
        at = start + len;
    }
    if at < fmt.node_bytes() {
        gaps.push(at..fmt.node_bytes());
    }
    gaps
}
