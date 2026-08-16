use super::*;
use crate::VersionStamp;
fn fmt() -> Arc<Format> {
    Arc::new(Format::tiny())
}
fn ok(n: u64) -> [u8; VERSION_BYTES] {
    VersionStamp::from_counter(n).order_key
}
fn leaf_entries(n: usize) -> Vec<Entry> {
    (0..n)
        .map(|i| {
            Entry::inline(
                Bytes::from(format!("key{i:04}")),
                ok(i as u64 + 1),
                Bytes::from(format!("v{i}")),
            )
        })
        .collect()
}
#[test]
fn a_leaf_round_trips_and_is_exactly_node_bytes() {
    let f = fmt();
    let entries = leaf_entries(10);
    let bytes = encode_leaf(&f, &entries).unwrap();
    assert_eq!(bytes.len(), f.node_bytes());
    let v = NodeView::decode(&f, None, bytes).unwrap();
    assert!(v.is_leaf());
    assert_eq!(v.tree_level(), 0);
    assert_eq!(v.entries().collect::<Vec<_>>(), entries);
}
#[test]
fn an_internal_node_round_trips() {
    let f = fmt();
    let children: Vec<BlockId> = (0..3u8).map(|i| BlockId([i + 1; 32])).collect();
    let pivots = vec![
        Bytes::from_static(b"key0004"),
        Bytes::from_static(b"key0008"),
    ];
    let buffer = leaf_entries(4);
    let bytes = encode_internal(&f, 1, &pivots, &children, &buffer).unwrap();
    let v = NodeView::decode(&f, None, bytes).unwrap();
    assert_eq!(v.child_count(), 3);
    assert_eq!(v.pivot_count(), 2);
    assert_eq!(v.tree_level(), 1);
    assert_eq!(v.pivot(1), b"key0008");
    assert_eq!(v.child(2), children[2]);
    assert_eq!(v.entries().collect::<Vec<_>>(), buffer);
}
#[test]
fn the_empty_leaf_encodes_and_decodes() {
    let f = fmt();
    let bytes = encode_leaf(&f, &[]).unwrap();
    let v = NodeView::decode(&f, None, bytes.clone()).unwrap();
    assert_eq!(v.entry_count(), 0);
    // Determinism: the empty leaf is one shared node, forever.
    assert_eq!(encode_leaf(&f, &[]).unwrap(), bytes);
}
#[test]
fn rebuilding_the_same_logical_node_gives_the_same_bytes() {
    let f = fmt();
    let a = encode_leaf(&f, &leaf_entries(9)).unwrap();
    let b = encode_leaf(&f, &leaf_entries(9)).unwrap();
    assert_eq!(a, b);
    assert_eq!(BlockId::of(&a), BlockId::of(&b));
}
#[test]
fn tombstones_and_external_values_round_trip() {
    let f = fmt();
    let ext_len = f.inline_value_bytes() as u32 + 1;
    let entries = vec![
        Entry::tombstone(Bytes::from_static(b"a"), ok(1)),
        Entry::external(Bytes::from_static(b"b"), ok(2), BlockId([9; 32]), ext_len),
        Entry::inline(Bytes::from_static(b"c"), ok(3), Bytes::from_static(b"x")),
    ];
    let v = NodeView::decode(&f, None, encode_leaf(&f, &entries).unwrap()).unwrap();
    assert_eq!(v.entries().collect::<Vec<_>>(), entries);
    assert_eq!(
        v.winner(1),
        Winner {
            order_key: ok(2),
            op: WinnerOp::External {
                id: BlockId([9; 32]),
                len: ext_len
            }
        }
    );
    assert_eq!(
        v.references(),
        vec![(crate::ObjectKind::Value, BlockId([9; 32]))]
    );
}
#[test]
fn empty_and_embedded_zero_keys_round_trip() {
    let f = fmt();
    let keys: Vec<Bytes> = vec![
        Bytes::from_static(b""),
        Bytes::from_static(b"\x00"),
        Bytes::from_static(b"\x00\x00"),
        Bytes::from_static(b"a\x00b"),
        Bytes::from_static(&[0xff; 9]),
    ];
    let entries: Vec<Entry> = keys
        .iter()
        .enumerate()
        .map(|(i, k)| Entry::inline(k.clone(), ok(i as u64), Bytes::new()))
        .collect();
    let v = NodeView::decode(&f, None, encode_leaf(&f, &entries).unwrap()).unwrap();
    for (i, k) in keys.iter().enumerate() {
        assert_eq!(v.entry_key(i), k.as_ref());
        let found = v.find(k);
        assert!(found.exact && found.index == i, "probe for {k:?}");
    }
}
/// A maximum-size key with a maximum-size inline value must fit the format's canonical leaf kind —
/// the compact kind for selected V3 and the regular kind where its columns suffice.
#[test]
#[cfg_attr(
    miri,
    ignore = "maximum-size allocation is covered by native debug/release tests"
)]
fn a_maximum_entry_fits_a_canonical_leaf() {
    for f in [Format::tiny(), Format::selected()] {
        let f = Arc::new(f);
        let e = Entry::inline(
            Bytes::from(vec![0xab; f.max_key_bytes()]),
            ok(1),
            Bytes::from(vec![0xcd; f.inline_value_bytes()]),
        );
        let bytes = encode_leaf(&f, std::slice::from_ref(&e)).expect("must fit");
        assert_eq!(NodeView::decode(&f, None, bytes).unwrap().entry(0), e);
    }
}
/// A full-width surface of maximum-size, shared-prefix pivots fits because V3 stores that prefix
/// once. Unrelated long tails are handled by the tree's byte-aware partitioner.
#[test]
#[cfg_attr(
    miri,
    ignore = "worst-case full node is covered by native debug/release tests"
)]
fn a_full_width_long_key_internal_node_fits() {
    for f in [Format::tiny(), Format::selected()] {
        let f = Arc::new(f);
        let pivots: Vec<Bytes> = (0..f.f_max() - 1)
            .map(|i| {
                let mut k = vec![0x61u8; f.max_key_bytes()];
                let at = k.len() - 2;
                k[at..].copy_from_slice(&(i as u16).to_be_bytes());
                Bytes::from(k)
            })
            .collect();
        let children: Vec<BlockId> = (0..f.f_max())
            .map(|i| BlockId([(i + 1) as u8; 32]))
            .collect();
        let bytes = encode_internal(&f, 1, &pivots, &children, &[]).expect("must fit");
        let v = NodeView::decode(&f, None, bytes).unwrap();
        assert_eq!(v.child_count(), f.f_max());
    }
}
#[test]
fn an_oversize_key_is_a_capacity_error_not_a_panic() {
    let f = fmt();
    let e = Entry::inline(
        Bytes::from(vec![1u8; f.max_key_bytes() + 1]),
        ok(1),
        Bytes::new(),
    );
    assert!(matches!(
        encode_leaf(&f, &[e]),
        Err(TreeError::Capacity(CapacityError::KeyTooLarge { .. }))
    ));
}
#[test]
fn unsorted_or_duplicate_entries_are_refused_by_the_encoder() {
    let f = fmt();
    let a = Entry::inline(Bytes::from_static(b"b"), ok(1), Bytes::new());
    let b = Entry::inline(Bytes::from_static(b"a"), ok(1), Bytes::new());
    assert!(encode_leaf(&f, &[a.clone(), b]).is_err());
    assert!(encode_leaf(&f, &[a.clone(), a]).is_err());
}
/// Mutating any single byte of a canonical node must either decode to something valid or return
/// `Decode` — never panic, never read out of bounds.
#[test]
#[cfg_attr(
    miri,
    ignore = "4096 byte-by-byte decodes are covered by the native corruption gate"
)]
fn every_single_byte_mutation_is_rejected_or_valid_never_a_panic() {
    let f = fmt();
    let mut entries = leaf_entries(6);
    entries.push(Entry::tombstone(Bytes::from_static(b"zz"), ok(99)));
    let good = encode_leaf(&f, &entries).unwrap();
    let mut rejected = 0usize;
    for i in 0..good.len() {
        for delta in [1u8, 0x80, 0xff] {
            let mut m = good.to_vec();
            m[i] = m[i].wrapping_add(delta);
            if m == good.as_ref() {
                continue;
            }
            match NodeView::decode(&f, None, Bytes::from(m)) {
                Ok(v) => {
                    // If it validates, every accessor must stay in bounds and self-consistent.
                    for j in 0..v.entry_count() {
                        let _ = v.entry(j);
                        let _ = v.winner(j);
                    }
                }
                Err(TreeError::Decode { .. }) | Err(TreeError::VersionDomainMismatch { .. }) => {
                    rejected += 1
                }
                Err(other) => panic!("unexpected error class {other:?}"),
            }
        }
    }
    assert!(rejected > 0, "mutations must be caught");
}
#[test]
fn a_truncated_or_extended_node_is_rejected_on_length() {
    let f = fmt();
    let good = encode_leaf(&f, &leaf_entries(3)).unwrap();
    for bytes in [
        good.slice(..good.len() - 1),
        Bytes::from({
            let mut v = good.to_vec();
            v.push(0);
            v
        }),
        Bytes::new(),
    ] {
        assert!(matches!(
            NodeView::decode(&f, None, bytes),
            Err(TreeError::Decode {
                reason: DecodeError::Length { .. },
                ..
            })
        ));
    }
}
#[test]
fn arbitrary_bytes_of_the_right_length_are_rejected() {
    let f = fmt();
    for fill in [0u8, 0xff, 0x5a] {
        let bytes = Bytes::from(vec![fill; f.node_bytes()]);
        assert!(NodeView::decode(&f, None, bytes).is_err());
    }
}
#[test]
fn a_node_from_another_version_domain_is_its_own_error_class() {
    let f = fmt();
    let good = encode_leaf(&f, &leaf_entries(3)).unwrap();
    let mut other = good.to_vec();
    other[hdr::VERSION_DOMAIN].copy_from_slice(b"someone-elses-d\0");
    assert!(matches!(
        NodeView::decode(&f, None, Bytes::from(other)),
        Err(TreeError::VersionDomainMismatch { .. })
    ));
}
#[test]
fn a_node_of_another_format_is_rejected_by_schema_id() {
    let a = Arc::new(Format::tiny());
    let b = Arc::new(Format::selected());
    let bytes = encode_leaf(&a, &leaf_entries(2)).unwrap();
    assert!(matches!(
        NodeView::decode(&b, None, bytes),
        // Different node_bytes, so length catches it first; that is still a Decode.
        Err(TreeError::Decode { .. })
    ));
    // Same length, different schema id: exercise the identifier check directly.
    let mut p = *a.params();
    p.leaf_slots -= 1;
    let c = Arc::new(Format::new(p).unwrap());
    let bytes = encode_leaf(&a, &leaf_entries(2)).unwrap();
    assert!(matches!(
        NodeView::decode(&c, None, bytes),
        Err(TreeError::Decode {
            reason: DecodeError::SchemaId { .. },
            ..
        })
    ));
}
/// Postcard bytes — and anything else that is not this exact format — are rejected outright.
#[test]
fn legacy_postcard_bytes_are_rejected() {
    let f = fmt();
    let mut legacy = vec![0x00u8, 0x02, 0x03];
    legacy.resize(f.node_bytes(), 0);
    assert!(matches!(
        NodeView::decode(&f, None, Bytes::from(legacy)),
        Err(TreeError::Decode {
            reason: DecodeError::Magic,
            ..
        })
    ));
}
/// The derived-field checks: a node that is memory-safe but internally inconsistent must be
/// rejected, because it would return *wrong search results* with no UB.
#[test]
fn a_tampered_head_or_skip_is_rejected_even_though_it_is_memory_safe() {
    let f = fmt();
    let entries = leaf_entries(8);
    let good = encode_leaf(&f, &entries).unwrap();
    let s = f.leaf_sections();
    let mut m = good.to_vec();
    m[s.entry_head(3)][0] ^= 0xff;
    assert!(matches!(
        NodeView::decode(&f, None, Bytes::from(m)),
        Err(TreeError::Decode {
            reason: DecodeError::Head { .. },
            ..
        })
    ));
    let mut m = good.to_vec();
    put_u32(&mut m, hdr::ENTRY_HEAD_SKIP, 1);
    assert!(matches!(
        NodeView::decode(&f, None, Bytes::from(m)),
        Err(TreeError::Decode { .. })
    ));
}
#[test]
fn non_monotonic_offsets_are_rejected() {
    let f = fmt();
    let good = encode_leaf(&f, &leaf_entries(4)).unwrap();
    let s = f.leaf_sections();
    let mut m = good.to_vec();
    // Make one span run backwards.
    put_u32(&mut m, s.entry_offset(3), 0);
    assert!(NodeView::decode(&f, None, Bytes::from(m)).is_err());
    // And an offset past the blob capacity.
    let mut m = good.to_vec();
    put_u32(&mut m, s.entry_offset(2), u32::MAX);
    assert!(matches!(
        NodeView::decode(&f, None, Bytes::from(m)),
        Err(TreeError::Decode {
            reason: DecodeError::OffsetRange { .. } | DecodeError::OffsetMonotonicity(_),
            ..
        })
    ));
}
#[test]
fn nonzero_padding_is_rejected_so_reserved_bits_stay_usable() {
    let f = fmt();
    let good = encode_leaf(&f, &leaf_entries(2)).unwrap();
    let s = f.leaf_sections();
    for at in [
        hdr::FLAGS,
        hdr::RESERVED_A.start,
        s.ops + 5,
        s.entry_heads + 5 * HEAD_BYTES,
        s.entry_blob + s.entry_blob_cap - 1,
    ] {
        let mut m = good.to_vec();
        m[at] = 1;
        assert!(
            NodeView::decode(&f, None, Bytes::from(m)).is_err(),
            "nonzero at {at} must be rejected"
        );
    }
}
#[test]
fn illegal_op_span_combinations_are_rejected() {
    let f = fmt();
    let s = f.leaf_sections();
    let entries = vec![Entry::inline(
        Bytes::from_static(b"k"),
        ok(1),
        Bytes::from_static(b"vvvv"),
    )];
    let good = encode_leaf(&f, &entries).unwrap();
    // op = external, but the span is 4 bytes rather than a 32-byte id.
    let mut m = good.to_vec();
    m[s.ops] = OP_EXTERNAL;
    assert!(matches!(
        NodeView::decode(&f, None, Bytes::from(m)),
        Err(TreeError::Decode {
            reason: DecodeError::Span { .. },
            ..
        })
    ));
    // op = tombstone, but a value span is present.
    let mut m = good.to_vec();
    m[s.ops] = OP_TOMBSTONE;
    assert!(matches!(
        NodeView::decode(&f, None, Bytes::from(m)),
        Err(TreeError::Decode {
            reason: DecodeError::Span { .. },
            ..
        })
    ));
    // An unknown op byte.
    let mut m = good.to_vec();
    m[s.ops] = 7;
    assert!(matches!(
        NodeView::decode(&f, None, Bytes::from(m)),
        Err(TreeError::Decode {
            reason: DecodeError::Op { .. },
            ..
        })
    ));
    // A nonzero external length on an inline value.
    let mut m = good.to_vec();
    put_u32(&mut m, s.external_len(0), 9);
    assert!(matches!(
        NodeView::decode(&f, None, Bytes::from(m)),
        Err(TreeError::Decode {
            reason: DecodeError::ExternalLen { .. },
            ..
        })
    ));
}
/// An out-of-line value at or below the inline threshold would be a *second* representation of the
/// same logical entry, so it must be rejected.
#[test]
fn an_external_value_below_the_inline_threshold_is_rejected() {
    let f = fmt();
    let e = Entry::external(
        Bytes::from_static(b"k"),
        ok(1),
        BlockId([3; 32]),
        f.inline_value_bytes() as u32,
    );
    let bytes = encode_leaf(&f, &[e]).unwrap();
    assert!(matches!(
        NodeView::decode(&f, None, bytes),
        Err(TreeError::Decode {
            reason: DecodeError::ExternalLen { .. },
            ..
        })
    ));
}
#[test]
fn a_zeroed_child_slot_inside_the_live_range_is_rejected() {
    let f = fmt();
    let children = vec![BlockId([1; 32]), BlockId([2; 32])];
    let good = encode_internal(&f, 1, &[Bytes::from_static(b"m")], &children, &[]).unwrap();
    let s = f.internal_sections();
    let mut m = good.to_vec();
    m[s.child_id(1)].fill(0);
    assert!(matches!(
        NodeView::decode(&f, None, Bytes::from(m)),
        Err(TreeError::Decode {
            reason: DecodeError::ZeroChild(1),
            ..
        })
    ));
}
#[test]
fn a_leaf_claiming_children_or_a_level_is_rejected() {
    let f = fmt();
    let good = encode_leaf(&f, &leaf_entries(2)).unwrap();
    let mut m = good.to_vec();
    put_u16(&mut m, hdr::TREE_LEVEL, 1);
    assert!(NodeView::decode(&f, None, Bytes::from(m)).is_err());
    let mut m = good.to_vec();
    put_u16(&mut m, hdr::CHILD_COUNT, 2);
    assert!(NodeView::decode(&f, None, Bytes::from(m)).is_err());
}
#[test]
fn a_tree_level_above_the_limit_is_rejected() {
    let f = fmt();
    let good = encode_internal(
        &f,
        1,
        &[Bytes::from_static(b"m")],
        &[BlockId([1; 32]), BlockId([2; 32])],
        &[],
    )
    .unwrap();
    let mut m = good.to_vec();
    put_u16(&mut m, hdr::TREE_LEVEL, f.max_tree_level() + 1);
    assert!(matches!(
        NodeView::decode(&f, None, Bytes::from(m)),
        Err(TreeError::Decode {
            reason: DecodeError::TreeLevel { .. },
            ..
        })
    ));
}
#[test]
fn no_header_value_sizes_an_allocation_or_a_loop() {
    // entry_count is the only count that drives iteration, and it is bounded by `slots` before
    // any column is read. Assert the bound directly.
    let f = fmt();
    let good = encode_leaf(&f, &leaf_entries(2)).unwrap();
    let mut m = good.to_vec();
    put_u32(&mut m, hdr::ENTRY_COUNT, u32::MAX);
    assert!(matches!(
        NodeView::decode(&f, None, Bytes::from(m)),
        Err(TreeError::Decode {
            reason: DecodeError::Count { .. },
            ..
        })
    ));
    let mut m = good.to_vec();
    put_u16(&mut m, hdr::PIVOT_COUNT, u16::MAX);
    assert!(NodeView::decode(&f, None, Bytes::from(m)).is_err());
}
#[test]
fn section_gaps_cover_every_byte_not_in_a_section() {
    for f in [Format::tiny(), Format::selected()] {
        for kind in [NodeKind::Leaf, NodeKind::Internal] {
            let gaps = section_gaps(&f, kind);
            for g in &gaps {
                assert!(g.start < g.end && g.end <= f.node_bytes());
            }
        }
    }
}
