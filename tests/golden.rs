//! Golden encoding and hash vectors.
//!
//! These pin the exact canonical bytes of each current schema and the permanent identifier of every
//! registered historical schema. A current byte vector is a tripwire, not a promise that future writers
//! emit it; a released schema id remains a read/migration contract. Regenerate current vectors with
//! `BE_TREE_REGENERATE_GOLDEN=1 cargo test --test golden -- --nocapture` after deliberately registering
//! the displaced schema.

mod support;

use std::sync::Arc;

use be_tree::codec::{self, Entry, NodeView};
use be_tree::format::Format;
use be_tree::search::head_of;
use be_tree::store::MemStore;
use be_tree::{BeTree, BlockId, Mutation, VERSION_BYTES, VersionStamp};
use bytes::Bytes;

fn hex(id: BlockId) -> String {
    id.0.iter().map(|b| format!("{b:02x}")).collect()
}

fn regenerating() -> bool {
    std::env::var_os("BE_TREE_REGENERATE_GOLDEN").is_some()
}

/// Compare against a pinned value, or print the actual value when regenerating.
fn pin(name: &str, actual: &str, expected: &str) {
    if regenerating() {
        println!("{name} = \"{actual}\"");
        return;
    }
    assert_eq!(actual, expected, "golden vector `{name}` changed");
}

fn ok(n: u64) -> [u8; VERSION_BYTES] {
    VersionStamp::from_counter(n).order_key
}

/// A fixed, deliberately awkward entry set: an empty key, an embedded zero, an all-0xff key, a
/// tombstone, an inline value, and an out-of-line reference.
fn fixture_entries(fmt: &Format) -> Vec<Entry> {
    let mut es = vec![
        Entry::inline(
            Bytes::from_static(b""),
            ok(1),
            Bytes::from_static(b"empty-key"),
        ),
        Entry::inline(Bytes::from_static(b"\x00a\x00"), ok(2), Bytes::new()),
        Entry::tombstone(Bytes::from_static(b"deleted"), ok(3)),
        Entry::external(
            Bytes::from_static(b"external"),
            ok(4),
            BlockId([0x5a; 32]),
            fmt.inline_value_bytes() as u32 + 1,
        ),
        Entry::inline(
            Bytes::from_static(b"shared/prefix/one"),
            ok(5),
            Bytes::from_static(b"v1"),
        ),
        Entry::inline(
            Bytes::from_static(b"shared/prefix/two"),
            ok(6),
            Bytes::from_static(b"v2"),
        ),
        Entry::inline(
            Bytes::from_static(&[0xff, 0xff, 0xff]),
            ok(7),
            Bytes::from_static(b"high"),
        ),
    ];
    es.sort_by(|a, b| a.key.cmp(&b.key));
    es
}

#[test]
fn the_shipped_schema_ids_are_pinned() {
    pin(
        "tiny.schema_id",
        &Format::tiny().schema_hex(),
        "718b5338e1b709eeb01079cf58967ed6",
    );
    pin(
        "selected.schema_id",
        &Format::selected().schema_hex(),
        "b91588e3375d113c275d2976c650698f",
    );
    pin(
        "legacy_selected_v2.schema_id",
        &Format::legacy_selected_v2().schema_hex(),
        "d14377a94570af8b3f22520538a93d3f",
    );
    // VERSION_BYTES is part of every schema id, so pin it explicitly too.
    pin("VERSION_BYTES", &VERSION_BYTES.to_string(), "28");
    pin(
        "SCHEMA_ID_BYTES",
        &be_tree::format::SCHEMA_ID_BYTES.to_string(),
        "16",
    );
}

#[test]
fn the_empty_leaf_hash_is_pinned() {
    for (name, fmt) in [("tiny", Format::tiny()), ("selected", Format::selected())] {
        let fmt = Arc::new(fmt);
        let bytes = codec::encode_leaf(&fmt, &[]).unwrap();
        assert_eq!(bytes.len(), fmt.node_bytes());
        pin(
            &format!("{name}.empty_leaf"),
            &hex(BlockId::of(&bytes)),
            match name {
                "tiny" => "aef3dccfaa754338f4ab1ca5a33ebd7dd878822b1360373f56921dd3f9f9516b",
                _ => "b7ad92ea6bf415de1c44de1699c28dc810555f0937930a74d3659148a813c94e",
            },
        );
        // ...and it round-trips.
        let v = NodeView::decode(&fmt, None, bytes).unwrap();
        assert_eq!(v.entry_count(), 0);
        assert!(v.is_leaf());
    }
}

#[test]
fn a_fixture_leaf_encodes_to_pinned_bytes() {
    let fmt = Arc::new(Format::tiny());
    let entries = fixture_entries(&fmt);
    let bytes = codec::encode_leaf(&fmt, &entries).unwrap();
    pin(
        "tiny.fixture_leaf",
        &hex(BlockId::of(&bytes)),
        "3183ff05f510a0cf81f591f57144973270ccc1c347b8be9109fb9fe0230899cf",
    );
    // Determinism, and exact round-trip of every field.
    assert_eq!(codec::encode_leaf(&fmt, &entries).unwrap(), bytes);
    let v = NodeView::decode(&fmt, None, bytes).unwrap();
    assert_eq!(v.entries().collect::<Vec<_>>(), entries);
    // The head skip must be zero here: the first and last keys share no prefix.
    pin(
        "tiny.fixture_leaf.entry_0_key_len",
        &v.entry_key(0).len().to_string(),
        "0",
    );
}

#[test]
fn a_fixture_internal_node_encodes_to_pinned_bytes() {
    let fmt = Arc::new(Format::tiny());
    let pivots = vec![
        Bytes::from_static(b"deleted"),
        Bytes::from_static(b"shared/prefix/two"),
    ];
    let children = vec![BlockId([1; 32]), BlockId([2; 32]), BlockId([3; 32])];
    let buffer: Vec<Entry> = fixture_entries(&fmt).into_iter().take(4).collect();
    let bytes = codec::encode_internal(&fmt, 3, &pivots, &children, &buffer).unwrap();
    pin(
        "tiny.fixture_internal",
        &hex(BlockId::of(&bytes)),
        "f856b397e775778056f4913a191029da8fb785b6682bf0caae6e1b154afed24e",
    );
    let v = NodeView::decode(&fmt, None, bytes).unwrap();
    assert_eq!(v.tree_level(), 3);
    assert_eq!(v.children().collect::<Vec<_>>(), children);
    assert_eq!(v.pivot(0), b"deleted");
    assert_eq!(v.entries().collect::<Vec<_>>(), buffer);
}

#[test]
fn a_value_object_encodes_to_pinned_bytes() {
    let v = be_tree::value::encode(Format::tiny().schema_id(), b"the quick brown fox");
    pin(
        "value.fox",
        &hex(v.id),
        "168dc63ffcc319c537a05fe11fcbfc5e800ede232a7771620adf0d41cfd305fa",
    );
    assert_eq!(v.bytes.len(), be_tree::value::ENVELOPE_BYTES + 19);
    pin(
        "ENVELOPE_BYTES",
        &be_tree::value::ENVELOPE_BYTES.to_string(),
        "32",
    );
    let empty = be_tree::value::encode(Format::tiny().schema_id(), b"");
    pin(
        "value.empty",
        &hex(empty.id),
        "18d23117a549c22629f94b2fc36ddb8f73819948e9c4930b4462f51e1e8ebd06",
    );
}

/// The head construction is the arithmetic every head-first search depends on. Pin it against
/// hand-computed values, including the padding and embedded-zero cases.
#[test]
fn head_construction_is_pinned() {
    let cases: &[(&[u8], usize, u64)] = &[
        (b"", 0, 0x0000_0000_0000_0000),
        (b"a", 0, 0x6100_0000_0000_0000),
        (b"a\x00", 0, 0x6100_0000_0000_0000), // padding is indistinguishable from a trailing zero
        (b"abcdefgh", 0, 0x6162_6364_6566_6768),
        (b"abcdefghi", 0, 0x6162_6364_6566_6768), // only the first eight bytes
        (b"abcdefghi", 1, 0x6263_6465_6667_6869),
        (b"abc", 8, 0x0000_0000_0000_0000), // skip past the end
        (&[0xff; 8], 0, u64::MAX),
        (&[0xff; 9], 0, u64::MAX),
    ];
    for (key, skip, want) in cases {
        assert_eq!(
            head_of(key, *skip),
            *want,
            "head_of({key:?}, {skip}) must be {want:#018x}"
        );
    }
}

/// A whole tree built by a fixed script must produce a pinned root. This is the end-to-end tripwire:
/// it covers normalization, flush order, split shape, and root growth all at once.
#[tokio::test]
#[cfg_attr(miri, ignore = "native scripted multi-level tree fixture")]
async fn a_scripted_tree_produces_a_pinned_root() {
    let store = Arc::new(MemStore::new());
    let t = BeTree::with_format(store.clone(), Format::tiny());
    let mut root = t.empty_root().await.unwrap();
    for round in 0..40u64 {
        let muts: Vec<Mutation> = (0..37u32)
            .map(|i| {
                let n = round * 37 + u64::from(i);
                if n % 11 == 0 {
                    Mutation::tombstone(Bytes::from(format!("k{n:06}")))
                } else {
                    Mutation::upsert(
                        Bytes::from(format!("k{n:06}")),
                        Bytes::from(format!("value-{n}")),
                    )
                }
            })
            .collect();
        root = t
            .apply(root, VersionStamp::from_counter(round + 1), muts)
            .await
            .unwrap();
    }
    pin(
        "scripted_tree.root",
        &hex(root),
        "e7ba366e5d5ec3b81332ebadb2fa02511f7908f165ed5f86752fe583972445ca",
    );
    // The tripwire is only meaningful if the tree is also correct.
    let shape = support::check_balanced(&t, root).await.unwrap();
    pin("scripted_tree.nodes", &shape.nodes.to_string(), "133");
    pin(
        "scripted_tree.root_level",
        &shape.root_level.to_string(),
        "4",
    );
    pin(
        "scripted_tree.live_keys",
        &t.scan_range(root, None, None)
            .await
            .unwrap()
            .len()
            .to_string(),
        "1345",
    );
}
