use super::*;
/// Every format the crate ships must satisfy the format invariants, including the two flush
/// inequalities that make an undersized flush impossible.
#[test]
fn shipped_formats_satisfy_the_flush_inequalities() {
    for f in [Format::selected(), Format::tiny()] {
        assert!(f.buffer_bytes() >= f.f_max() * f.min_flush_bytes());
        assert!(f.message_slots() * f.desc_bytes() >= f.f_max() * f.min_flush_bytes());
        assert!(f.min_flush_bytes() > 0);
        assert_eq!(f.node_bytes() % SECTION_ALIGN, 0);
        // The blob trigger must also clear the floor, which is why the derivation takes a min.
        assert!(f.internal_sections().entry_blob_cap >= f.f_max() * f.min_flush_bytes());
    }
}
#[test]
fn sections_stay_inside_the_node_and_are_aligned() {
    for f in [Format::selected(), Format::tiny()] {
        for kind in [NodeKind::Leaf, NodeKind::CompactLeaf, NodeKind::Internal] {
            let s = f.sections_for(kind);
            assert_eq!(s.entry_heads % SECTION_ALIGN, 0);
            assert_eq!(s.entry_offsets % SECTION_ALIGN, 0);
            assert_eq!(s.entry_blob + s.entry_blob_cap, f.node_bytes());
            if kind == NodeKind::Internal {
                assert_eq!(s.child_ids % SECTION_ALIGN, 0);
                assert_eq!(s.pivot_blob + s.pivot_blob_cap, s.entry_blob);
            }
        }
    }
}
#[test]
fn distinct_configurations_get_distinct_schema_ids() {
    let a = Format::selected();
    let b = Format::tiny();
    assert_ne!(a.schema_id(), b.schema_id());
    let mut p = *a.params();
    p.f_max += 1;
    assert_ne!(Format::new(p).unwrap().schema_id(), a.schema_id());
}
/// A 16-bit identifier collided on real, valid configurations — `400/525` slots against `400/596`,
/// which have *different layouts*. Sweep a wide space of valid formats and require every distinct
/// layout to get a distinct id.
#[test]
#[cfg_attr(
    miri,
    ignore = "thousands of format derivations are covered by native tests"
)]
fn no_two_distinct_layouts_share_a_schema_id() {
    use std::collections::HashMap;
    let base = *Format::selected().params();
    let mut seen: HashMap<Vec<u8>, FormatParams> = HashMap::new();
    let mut checked = 0usize;
    for leaf_slots in (64..=768).step_by(1) {
        for message_slots in [leaf_slots, 400, 525, 596] {
            let p = FormatParams {
                leaf_slots,
                message_slots,
                ..base
            };
            let Ok(f) = Format::new(p) else { continue };
            checked += 1;
            let id = f.schema_id().to_vec();
            if let Some(prev) = seen.insert(id, p) {
                assert_eq!(
                    (prev.leaf_slots, prev.message_slots),
                    (p.leaf_slots, p.message_slots),
                    "schema id collision between distinct layouts: {prev:?} vs {p:?}"
                );
            }
        }
    }
    assert!(
        checked > 1_000,
        "the sweep must cover a wide space, got {checked}"
    );
}
/// The domain is not part of the layout, so it must NOT change `schema_id` — otherwise a foreign
/// producer's node would be indistinguishable from a wrong-format node.
#[test]
fn version_domain_does_not_change_the_schema_id() {
    let a = Format::selected();
    let p = FormatParams {
        version_domain: *b"a-different-dom\0",
        ..*a.params()
    };
    assert_eq!(Format::new(p).unwrap().schema_id(), a.schema_id());
}
/// Format derivation must never panic or wrap on a hostile configuration. `usize::MAX` for
/// `max_value_bytes` used to overflow the envelope addition before the bound was checked: a debug
/// panic, and in release a *wrap* that accepted the configuration.
#[test]
fn extreme_configurations_are_rejected_not_wrapped() {
    let base = *Format::selected().params();
    for p in [
        FormatParams {
            max_value_bytes: usize::MAX,
            ..base
        },
        FormatParams {
            max_object_bytes: usize::MAX,
            ..base
        },
        FormatParams {
            leaf_slots: usize::MAX,
            ..base
        },
        FormatParams {
            message_slots: usize::MAX,
            ..base
        },
        FormatParams {
            max_key_bytes: usize::MAX,
            ..base
        },
        FormatParams {
            f_max: u16::MAX as usize,
            max_key_bytes: usize::MAX,
            ..base
        },
        FormatParams {
            node_bytes: usize::MAX & !63,
            ..base
        },
    ] {
        assert!(
            Format::new(p).is_err(),
            "expected a typed rejection, not a panic or a wrap, for {p:?}"
        );
    }
}
#[test]
fn invalid_configurations_are_rejected_not_patched() {
    let base = *Format::selected().params();
    let cases: Vec<FormatParams> = vec![
        FormatParams { f_max: 2, ..base },
        FormatParams {
            node_bytes: 100,
            ..base
        },
        FormatParams {
            node_bytes: 64,
            ..base
        },
        // A max key that cannot be reserved for F_MAX-1 pivots.
        FormatParams {
            max_key_bytes: 1 << 20,
            ..base
        },
        FormatParams {
            max_value_bytes: base.max_object_bytes,
            ..base
        },
        FormatParams {
            max_tree_level: 0,
            ..base
        },
        FormatParams {
            leaf_slots: 0,
            ..base
        },
    ];
    for p in cases {
        assert!(Format::new(p).is_err(), "expected rejection for {p:?}");
    }
}
