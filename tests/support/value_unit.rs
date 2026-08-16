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
