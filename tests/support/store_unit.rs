use super::*;
#[tokio::test]
async fn put_batch_rejects_an_id_bytes_mismatch() {
    let s = MemStore::new();
    let e = s
        .put_batch(
            vec![AddressedObject {
                id: BlockId([0; 32]),
                bytes: Bytes::from_static(b"not the preimage of zero"),
            }],
            (),
        )
        .await
        .unwrap_err();
    assert!(matches!(e, TreeError::HashMismatch { .. }));
    assert!(s.is_empty(), "nothing is stored when the batch is rejected");
}
#[tokio::test]
async fn get_many_preserves_input_order_and_duplicates() {
    let s = MemStore::new();
    let objs: Vec<AddressedObject> = [b"a".as_ref(), b"bb", b"ccc"]
        .iter()
        .map(|b| AddressedObject {
            id: BlockId::of(b),
            bytes: Bytes::from_static(b),
        })
        .collect();
    s.put_batch(objs.clone(), ()).await.unwrap();
    let ids = vec![objs[2].id, objs[0].id, objs[2].id];
    let got = s.get_many(&ids, AccessHint::Random, 64, 1 << 20).await;
    assert_eq!(got.len(), 3);
    assert_eq!(got[0].as_ref().unwrap(), &objs[2].bytes);
    assert_eq!(got[1].as_ref().unwrap(), &objs[0].bytes);
    assert_eq!(got[2].as_ref().unwrap(), &objs[2].bytes);
}
#[tokio::test]
async fn a_read_bound_is_enforced_by_the_store() {
    let s = MemStore::new();
    let bytes = Bytes::from_static(b"0123456789");
    let id = BlockId::of(&bytes);
    s.put_batch(vec![AddressedObject { id, bytes }], ())
        .await
        .unwrap();
    assert!(s.get(id, AccessHint::Random, 4).await.is_err());
    assert!(s.get(id, AccessHint::Random, 10).await.is_ok());
    // ...and the aggregate bound too.
    let got = s.get_many(&[id, id], AccessHint::Random, 10, 15).await;
    assert!(got[0].is_ok());
    assert!(matches!(got[1], Err(TreeError::ResourceLimit { .. })));
}
