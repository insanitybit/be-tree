mod support;

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use cbe_tree::format::Format;
use cbe_tree::store::MemStore;
use cbe_tree::{BeTree, Mutation, VersionStamp};
use bytes::Bytes;
use support::{self as harness, CountingStore};

#[tokio::test]
async fn a_registered_historical_root_rewrites_every_winner_semantic() {
    let store = Arc::new(MemStore::new());
    let legacy = BeTree::with_format(store.clone(), Format::legacy_selected_v2());
    let mut old_root = legacy.empty_root().await.unwrap();
    old_root = legacy
        .apply(
            old_root,
            VersionStamp::from_counter(7),
            vec![
                Mutation::upsert("inline", "small"),
                Mutation::upsert("external", Bytes::from(vec![0x5a; 4096])),
                Mutation::upsert("deleted", "once-live"),
            ],
        )
        .await
        .unwrap();
    old_root = legacy
        .apply(
            old_root,
            VersionStamp::from_counter(9),
            vec![Mutation::tombstone("deleted")],
        )
        .await
        .unwrap();

    let counting = Arc::new(CountingStore::new(store.clone()));
    let detected = BeTree::open_known(counting.clone(), old_root)
        .await
        .unwrap();
    assert_eq!(
        counting.objects_fetched.load(Relaxed),
        1,
        "opening a known root must not refetch it after schema detection"
    );
    assert_eq!(detected.format().schema_id(), legacy.format().schema_id());

    let current = BeTree::new(store.clone());
    let report = current.migrate_from(&detected, old_root, 2).await.unwrap();
    assert_ne!(report.source_schema, report.target_schema);
    assert_eq!(report.rows, 3);
    assert_eq!(report.old_root, old_root);

    for key in [b"deleted".as_slice(), b"external", b"inline"] {
        assert_eq!(
            detected.get(old_root, key).await.unwrap(),
            current.get(report.new_root, key).await.unwrap(),
            "migration must preserve the observable winner for {key:?}"
        );
    }
    // The migrated tombstone retains its exact order key: an older write cannot resurrect it.
    let challenged = current
        .apply(
            report.new_root,
            VersionStamp::from_counter(8),
            vec![Mutation::upsert("deleted", "stale")],
        )
        .await
        .unwrap();
    assert_eq!(current.get(challenged, b"deleted").await.unwrap(), None);
    assert_eq!(
        current
            .get(report.new_root, b"external")
            .await
            .unwrap()
            .unwrap()
            .len(),
        4096
    );
    assert_eq!(
        detected.get(old_root, b"inline").await.unwrap(),
        Some(Bytes::from_static(b"small")),
        "migration never mutates historical roots"
    );
}

#[tokio::test]
#[cfg_attr(miri, ignore = "native multi-node historical-format rewrite")]
async fn migration_walks_a_multi_node_historical_tree_in_bounded_batches() {
    let store = Arc::new(MemStore::new());
    let legacy = BeTree::with_format(store.clone(), Format::legacy_selected_v2());
    let empty = legacy.empty_root().await.unwrap();
    let mutations: Vec<Mutation> = (0..1_000u32)
        .map(|i| {
            Mutation::upsert(
                Bytes::from(format!("historical/{i:04}")),
                Bytes::from(format!("value/{i:04}")),
            )
        })
        .collect();
    let old_root = legacy
        .apply(empty, VersionStamp::from_counter(1), mutations)
        .await
        .unwrap();
    assert!(
        harness::check_balanced(&legacy, old_root)
            .await
            .unwrap()
            .nodes
            > 1
    );

    let current = BeTree::new(store);
    let report = current.migrate_from(&legacy, old_root, 73).await.unwrap();
    assert_eq!(report.rows, 1_000);
    assert_eq!(report.apply_batches, 14);
    assert_eq!(
        legacy.scan_range(old_root, None, None).await.unwrap(),
        current
            .scan_range(report.new_root, None, None)
            .await
            .unwrap()
    );
    harness::check_balanced(&current, report.new_root)
        .await
        .unwrap();
}
