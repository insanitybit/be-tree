//! The three properties the crate's documentation claims, asserted against the reference `MemStore`.
//! Deliberately behavioural: they hold for any correct [`NodeStore`], not just this one.

use std::sync::Arc;

use be_tree::store::MemStore;
use be_tree::{BTreeMessage, Hlc, MemTree, Tree};

fn hlc(ms: u64) -> Hlc {
    Hlc {
        wall_ms: ms,
        logical: 0,
    }
}

/// A fresh tree plus its empty root — every write starts from `empty` (or a prior root).
async fn tree() -> (Arc<MemStore>, MemTree<MemStore>, be_tree::BlockId) {
    let store = Arc::new(MemStore::new());
    let t = MemTree::new(store.clone());
    let empty = t.empty_root().await.expect("empty root");
    (store, t, empty)
}

#[tokio::test]
async fn writes_read_back_and_the_root_changes_with_content() {
    let (_store, t, empty) = tree().await;
    let msgs: Vec<BTreeMessage> = (0..64u32)
        .map(|i| {
            BTreeMessage::upsert(
                format!("k{i:04}").into_bytes(),
                format!("v{i}").into_bytes(),
                hlc(1),
            )
        })
        .collect();
    let root = t.tree_put(empty, msgs).await.expect("write");
    for i in 0..64u32 {
        assert_eq!(
            t.tree_get(root, format!("k{i:04}").as_bytes())
                .await
                .expect("read"),
            Some(format!("v{i}").into_bytes()),
            "every written key reads back"
        );
    }
    assert_eq!(
        t.tree_get(root, b"absent").await.expect("read"),
        None,
        "an unwritten key is absent, not an error"
    );
}

#[tokio::test]
async fn equal_content_yields_the_same_root_hash() {
    // Property 1: the tree is a Merkle tree by construction. Two independently-built trees holding the
    // same logical content must agree on the root id — that is what makes structural diff O(divergence)
    // and what lets two replicas recognise convergence without comparing contents.
    let build = |seed: u64| async move {
        let (_s, t, empty) = tree().await;
        let msgs: Vec<BTreeMessage> = (0..32u32)
            .map(|i| {
                BTreeMessage::upsert(
                    format!("k{i:04}").into_bytes(),
                    format!("v{i}").into_bytes(),
                    hlc(seed),
                )
            })
            .collect();
        t.tree_put(empty, msgs).await.expect("write")
    };
    assert_eq!(
        build(7).await,
        build(7).await,
        "same content + same HLCs => same root hash, on independently built trees"
    );
    assert_ne!(
        build(7).await,
        build(8).await,
        "different HLCs are part of the content, so the root must differ"
    );
}

#[tokio::test]
async fn a_write_rewrites_only_the_touched_path_so_old_roots_still_read() {
    // Properties 2 and 3: copy-on-write means an old root is still a valid, complete snapshot after a
    // subsequent write. This is MVCC with no locks and no version table — the reader just keeps its id.
    let (store, t, empty) = tree().await;
    let v1 = t
        .tree_put(
            empty,
            vec![BTreeMessage::upsert(
                b"key".to_vec(),
                b"first".to_vec(),
                hlc(1),
            )],
        )
        .await
        .expect("v1");
    let blocks_after_v1 = store.len();

    let v2 = t
        .tree_put(
            v1,
            vec![BTreeMessage::upsert(
                b"key".to_vec(),
                b"second".to_vec(),
                hlc(2),
            )],
        )
        .await
        .expect("v2");

    assert_ne!(v1, v2, "a write produces a NEW root");
    assert_eq!(
        t.tree_get(v1, b"key").await.expect("read v1"),
        Some(b"first".to_vec()),
        "the old root is untouched — it still reads its own version"
    );
    assert_eq!(
        t.tree_get(v2, b"key").await.expect("read v2"),
        Some(b"second".to_vec()),
        "the new root sees the update"
    );
    assert!(
        store.len() > blocks_after_v1,
        "COW adds nodes rather than mutating them"
    );
}

#[tokio::test]
async fn higher_hlc_wins_regardless_of_write_order() {
    // The LWW-at-read-by-descending-HLC rule, which is what makes an optimistic writer's rebase-and-
    // replay safe: re-injecting a LOWER-HLC message next to a higher one must not resurrect it.
    let (_s, t, empty) = tree().await;
    let root = t
        .tree_put(
            empty,
            vec![BTreeMessage::upsert(
                b"k".to_vec(),
                b"new".to_vec(),
                hlc(10),
            )],
        )
        .await
        .expect("high first");
    let root = t
        .tree_put(
            root,
            vec![BTreeMessage::upsert(b"k".to_vec(), b"old".to_vec(), hlc(5))],
        )
        .await
        .expect("low second");
    assert_eq!(
        t.tree_get(root, b"k").await.expect("read"),
        Some(b"new".to_vec()),
        "the higher HLC wins even though the lower was written LAST"
    );
}

#[tokio::test]
async fn a_tombstone_hides_a_key() {
    let (_s, t, empty) = tree().await;
    let root = t
        .tree_put(
            empty,
            vec![BTreeMessage::upsert(b"k".to_vec(), b"v".to_vec(), hlc(1))],
        )
        .await
        .expect("put");
    let root = t
        .tree_put(root, vec![BTreeMessage::tombstone(b"k".to_vec(), hlc(2))])
        .await
        .expect("delete");
    assert_eq!(t.tree_get(root, b"k").await.expect("read"), None);
}
