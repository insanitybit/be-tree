use std::hint::black_box;
use std::sync::Arc;

use be_tree::codec::{self, NodeView};
use be_tree::{BeTree, BlockId, Format, MemStore, Mutation, VersionStamp};
use bytes::Bytes;

pub const KEYS: usize = 10_000;
pub const WIDTH: usize = 256;

pub struct Fixture {
    pub tree: BeTree<MemStore>,
    pub root: BlockId,
    pub keys: Vec<Bytes>,
}

pub fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("profiling runtime")
}

pub async fn fixture() -> Fixture {
    let store = Arc::new(MemStore::new());
    let tree = BeTree::new(store.clone());
    let keys: Vec<Bytes> = (0..KEYS)
        .map(|i| Bytes::from(format!("profile/key/{i:08}")))
        .collect();
    let mut root = tree.empty_root().await.expect("empty root");
    for (stamp, chunk) in (1u64..).zip(keys.chunks(WIDTH)) {
        let mutations = chunk
            .iter()
            .map(|key| Mutation::upsert(key.clone(), Bytes::from_static(b"profile-value-24-bytes")))
            .collect();
        root = tree
            .apply(root, VersionStamp::from_counter(stamp), mutations)
            .await
            .expect("build profiling fixture");
    }
    Fixture { tree, root, keys }
}

pub fn probes(keys: &[Bytes]) -> Vec<&[u8]> {
    keys.iter()
        .step_by((keys.len() / WIDTH).max(1))
        .take(WIDTH)
        .map(Bytes::as_ref)
        .collect()
}

pub async fn warm(fixture: &Fixture) {
    let keys = probes(&fixture.keys);
    black_box(
        fixture
            .tree
            .get_many(fixture.root, &keys)
            .await
            .expect("warm probes"),
    );
}

pub async fn get_many(fixture: &Fixture, iterations: usize) -> u64 {
    let keys = probes(&fixture.keys);
    let mut checksum = 0u64;
    for _ in 0..iterations {
        let values = fixture
            .tree
            .get_many(fixture.root, &keys)
            .await
            .expect("profile get_many");
        checksum =
            checksum.wrapping_add(values.iter().flatten().map(Bytes::len).sum::<usize>() as u64);
        black_box(&values);
    }
    checksum
}

pub async fn scan(fixture: &Fixture, iterations: usize) -> u64 {
    let mut checksum = 0u64;
    for _ in 0..iterations {
        let rows = fixture
            .tree
            .scan_range(fixture.root, None, None)
            .await
            .expect("profile scan");
        checksum = checksum.wrapping_add(rows.len() as u64);
        black_box(&rows);
    }
    checksum
}

pub async fn scan_stream(fixture: &Fixture, iterations: usize) -> u64 {
    let mut checksum = 0u64;
    for _ in 0..iterations {
        let mut cursor = fixture.tree.scan_cursor(fixture.root, None, None);
        loop {
            let rows = cursor.next_batch(WIDTH).await.expect("profile scan cursor");
            if rows.is_empty() {
                break;
            }
            checksum = checksum.wrapping_add(rows.len() as u64);
            black_box(&rows);
        }
    }
    checksum
}

pub async fn apply(fixture: &Fixture, iterations: usize) -> u64 {
    let mut root = fixture.root;
    for i in 0..iterations {
        let key = fixture.keys[(i * 977) % fixture.keys.len()].clone();
        root = fixture
            .tree
            .apply(
                root,
                VersionStamp::from_counter(1_000_000 + i as u64),
                vec![Mutation::upsert(
                    key,
                    Bytes::from_static(b"updated-profile-value"),
                )],
            )
            .await
            .expect("profile apply");
    }
    black_box(root.0[0] as u64)
}

pub fn hash_bytes() -> Vec<u8> {
    vec![0x5a; Format::selected().node_bytes()]
}

pub fn hash(bytes: &[u8], iterations: usize) -> u64 {
    let mut checksum = 0u64;
    for _ in 0..iterations {
        checksum = checksum.wrapping_add(black_box(BlockId::of(black_box(bytes))).0[0] as u64);
    }
    checksum
}

pub fn encoded_node() -> (Arc<Format>, Bytes) {
    let format = Arc::new(Format::selected());
    let bytes = codec::encode_leaf(&format, &[]).expect("encode profiling node");
    (format, bytes)
}

pub fn decode(format: &Arc<Format>, bytes: &Bytes, iterations: usize) -> u64 {
    let mut checksum = 0u64;
    for _ in 0..iterations {
        let view =
            NodeView::decode(format, None, black_box(bytes.clone())).expect("profile decode");
        checksum = checksum.wrapping_add(black_box(view.bytes().len()) as u64);
    }
    checksum
}
