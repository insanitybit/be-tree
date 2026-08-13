//! Out-of-line values: the canonical `ValueObject` envelope.
//!
//! A value above `inline_value_bytes` is stored as its own content-addressed object, so a single large
//! value cannot dictate node size or split shape. Point lookup then costs one *additional* independent
//! fetch, and only for an out-of-line winner.
//!
//! The envelope begins with a distinct object-type domain tag, so node bytes and value bytes are
//! disjoint byte languages before hashing — the ordinary collision-resistance assumption for
//! [`BlockId`] then covers both without an outer wrapper.

use bytes::{BufMut, Bytes, BytesMut};

use crate::{BlockId, DecodeError, TreeError};

/// Value object-type domain tag. Disjoint from [`crate::format::NODE_MAGIC`].
pub const VALUE_MAGIC: [u8; 8] = *b"BeTreeV1";

/// `magic(8) + schema_id(16) + flags(1) + reserved(3) + logical_len(4)`. Unique and minimal: there is
/// exactly one envelope for one `(schema_id, payload)` pair.
///
/// The schema id is carried in full rather than truncated. Value objects only exist above the inline
/// threshold, so 16 bytes of exact identity costs nothing worth measuring.
pub const ENVELOPE_BYTES: usize = 32;

/// A staged out-of-line value: its id and its exact stored bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueObject {
    pub id: BlockId,
    pub bytes: Bytes,
    pub logical_len: u32,
}

/// Wrap `payload` in its canonical envelope and address it. Deterministic, so replaying the same write
/// produces the same id.
pub fn encode(schema_id: &crate::format::SchemaId, payload: &[u8]) -> ValueObject {
    let mut b = BytesMut::with_capacity(ENVELOPE_BYTES + payload.len());
    b.put_slice(&VALUE_MAGIC);
    b.put_slice(schema_id);
    b.put_u8(0); // flags
    b.put_slice(&[0u8; 3]); // reserved
    b.put_u32_le(payload.len() as u32);
    b.put_slice(payload);
    let bytes = b.freeze();
    ValueObject {
        id: BlockId::of(&bytes),
        bytes,
        logical_len: payload.len() as u32,
    }
}

/// Unwrap a fetched value envelope, checking it against the `external_value_len` authenticated by the
/// node that referenced it. Hash verification happens before this, in the loader.
pub fn decode(
    id: BlockId,
    bytes: &Bytes,
    schema_id: &crate::format::SchemaId,
    expected_len: u32,
) -> Result<Bytes, TreeError> {
    let payload = decode_payload(id, bytes, schema_id)?;
    validate_reference_len(id, &payload, expected_len)?;
    Ok(payload)
}

/// Validate one value object independently of any particular node reference. Caches are keyed by
/// `BlockId`, while each referencing node authenticates its own logical length; separating these two
/// checks lets one coalesced/cache fill serve many callers without allowing the first caller's length
/// to stand in for every later reference.
pub(crate) fn decode_payload(
    id: BlockId,
    bytes: &Bytes,
    schema_id: &crate::format::SchemaId,
) -> Result<Bytes, TreeError> {
    let err = |reason| TreeError::decode(Some(id), reason);
    if bytes.len() < ENVELOPE_BYTES {
        return Err(err(DecodeError::ValueEnvelope("shorter than the envelope")));
    }
    if bytes[..8] != VALUE_MAGIC {
        return Err(err(DecodeError::Magic));
    }
    if &bytes[8..24] != schema_id.as_slice() {
        return Err(err(DecodeError::SchemaId {
            found: bytes[8..24].iter().map(|b| format!("{b:02x}")).collect(),
            expected: schema_id.iter().map(|b| format!("{b:02x}")).collect(),
        }));
    }
    if bytes[24] != 0 {
        return Err(err(DecodeError::Flags(bytes[24])));
    }
    if bytes[25..28].iter().any(|&b| b != 0) {
        return Err(err(DecodeError::Reserved("value envelope")));
    }
    let declared = u32::from_le_bytes(bytes[28..32].try_into().expect("4 bytes"));
    // The envelope must be exactly its payload: no trailing bytes, no padding, one representation.
    let actual = bytes.len() - ENVELOPE_BYTES;
    if u64::from(declared) != actual as u64 {
        return Err(err(DecodeError::ValueEnvelope(
            "declared length disagrees with the payload length",
        )));
    }
    Ok(bytes.slice(ENVELOPE_BYTES..))
}

/// Check the logical length authenticated by one referencing node. This must run for every reference,
/// including duplicate ids and value-cache hits.
pub(crate) fn validate_reference_len(
    id: BlockId,
    payload: &Bytes,
    expected_len: u32,
) -> Result<(), TreeError> {
    let found = payload.len() as u32; // every value object is strictly below 2^32
    if found != expected_len {
        return Err(TreeError::decode(
            Some(id),
            DecodeError::ValueLength {
                found,
                expected: expected_len,
            },
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two distinct schema ids, so the envelope's identity check is exercised rather than assumed.
    const S1: crate::format::SchemaId = [1u8; 16];
    const S2: crate::format::SchemaId = [2u8; 16];

    #[test]
    fn round_trips_and_is_deterministic() {
        let payload = vec![7u8; 5000];
        let a = encode(&S1, &payload);
        let b = encode(&S1, &payload);
        assert_eq!(a, b, "canonical: same payload => same bytes and id");
        assert_eq!(
            decode(a.id, &a.bytes, &S1, a.logical_len).unwrap().as_ref(),
            payload.as_slice()
        );
    }

    #[test]
    fn empty_payload_round_trips() {
        let v = encode(&S1, b"");
        assert_eq!(v.bytes.len(), ENVELOPE_BYTES);
        assert!(decode(v.id, &v.bytes, &S1, 0).unwrap().is_empty());
    }

    #[test]
    fn a_substituted_value_object_is_rejected_by_its_authenticated_length() {
        let a = encode(&S1, b"short");
        let b = encode(&S1, b"a much longer value");
        // Pretend the store returned b's bytes for a reference that authenticated a's length.
        let e = decode(b.id, &b.bytes, &S1, a.logical_len).unwrap_err();
        assert!(matches!(
            e,
            TreeError::Decode {
                reason: DecodeError::ValueLength { .. },
                ..
            }
        ));
    }

    #[test]
    fn a_node_byte_string_is_not_a_value_object() {
        let mut fake = crate::format::NODE_MAGIC.to_vec();
        fake.extend_from_slice(&[0u8; 64]);
        let bytes = Bytes::from(fake);
        let id = BlockId::of(&bytes);
        assert!(matches!(
            decode(id, &bytes, &S1, 0).unwrap_err(),
            TreeError::Decode {
                reason: DecodeError::Magic,
                ..
            }
        ));
    }

    /// A value object built under another schema must be rejected, not silently reinterpreted.
    #[test]
    fn a_value_object_from_another_schema_is_rejected() {
        let v = encode(&S1, b"payload");
        assert!(matches!(
            decode(v.id, &v.bytes, &S2, 7).unwrap_err(),
            TreeError::Decode {
                reason: DecodeError::SchemaId { .. },
                ..
            }
        ));
    }

    #[test]
    fn trailing_bytes_are_not_a_second_representation() {
        let v = encode(&S1, b"abc");
        let mut tampered = v.bytes.to_vec();
        tampered.push(0);
        let bytes = Bytes::from(tampered);
        assert!(decode(BlockId::of(&bytes), &bytes, &S1, 3).is_err());
    }
}
