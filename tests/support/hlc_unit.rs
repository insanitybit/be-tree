use super::*;
const A: [u8; 16] = [0xa; 16];
const B: [u8; 16] = [0xb; 16];
#[test]
fn same_millisecond_events_are_still_strictly_ordered() {
    let mut c = HlcClock::new(A);
    let s1 = c.next(1000).unwrap();
    let s2 = c.next(1000).unwrap();
    let s3 = c.next(1000).unwrap();
    assert!(
        s1 < s2 && s2 < s3,
        "the logical counter orders local events"
    );
    assert_eq!(decode(&s3).0.logical, 2);
}
#[test]
fn a_remote_merge_makes_the_next_write_strictly_later() {
    let mut a = HlcClock::new(A);
    let mut b = HlcClock::new(B);
    let remote = b.next(5_000).unwrap();
    let (remote_hlc, _) = decode(&remote);
    // a's physical clock is far behind, but it observed the remote event.
    a.observe(remote_hlc);
    let s = a.next(10).unwrap();
    assert!(
        s > remote,
        "a causally dependent write must sort after the event it saw"
    );
}
#[test]
fn clock_rollback_never_decreases_state() {
    let mut c = HlcClock::new(A);
    let high = c.next(9_000).unwrap();
    // The wall clock jumps backwards by an hour.
    let after = c.next(5_400_000_u64.wrapping_sub(5_400_000)).unwrap();
    assert!(after > high, "a rollback must not produce a smaller stamp");
    assert_eq!(decode(&after).0.wall_ms, 9_000);
}
#[test]
fn restart_recovery_never_reissues_a_stamp() {
    let mut c = HlcClock::new(A);
    let last = c.next(7_000).unwrap();
    let persisted = c.state();
    // Restart with a wall clock that has not advanced.
    let mut c2 = HlcClock::restore(A, persisted);
    let after = c2.next(7_000).unwrap();
    assert!(after > last);
}
#[test]
fn distinct_writers_with_equal_hlc_pairs_are_ordered_by_writer_id() {
    let mut a = HlcClock::new(A);
    let mut b = HlcClock::new(B);
    let sa = a.next(1234).unwrap();
    let sb = b.next(1234).unwrap();
    assert_eq!(decode(&sa).0, decode(&sb).0);
    assert!(
        sa < sb,
        "the writer id breaks an equal-HLC tie deterministically"
    );
}
#[test]
fn logical_exhaustion_errors_instead_of_wrapping() {
    let mut c = HlcClock::restore(
        A,
        Hlc {
            wall_ms: 42,
            logical: u32::MAX,
        },
    );
    assert_eq!(c.next(42), Err(HlcError::LogicalExhausted { wall_ms: 42 }));
    // A greater physical component clears it.
    assert!(c.next(43).is_ok());
}
#[test]
fn encoding_is_canonical_and_order_preserving() {
    let readings = [
        Hlc {
            wall_ms: 0,
            logical: 0,
        },
        Hlc {
            wall_ms: 0,
            logical: 1,
        },
        Hlc {
            wall_ms: 1,
            logical: 0,
        },
        Hlc {
            wall_ms: u64::MAX,
            logical: u32::MAX,
        },
    ];
    for (i, x) in readings.iter().enumerate() {
        for (j, y) in readings.iter().enumerate() {
            let (sx, sy) = (encode(*x, A), encode(*y, A));
            assert_eq!(
                sx.cmp(&sy),
                i.cmp(&j),
                "big-endian encoding must be order preserving"
            );
        }
        assert_eq!(decode(&encode(*x, A)), (*x, A));
    }
}
