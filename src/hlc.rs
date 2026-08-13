//! An **optional** producer adapter: pack a Hybrid Logical Clock into an order key.
//!
//! The tree treats order keys as opaque bytes and never reinterprets their clock fields. This module
//! exists so one common producer protocol is available and *tested*, not because the tree depends on
//! it. Other applications may use Lamport counters or any other canonical total order that fits
//! [`VERSION_BYTES`]; a different producer protocol uses a different `version_domain`.
//!
//! The order key is the big-endian triple `(wall_ms: u64, logical: u32, writer_id: [u8; 16])`, which is
//! exactly [`VERSION_BYTES`] (28 bytes).
//!
//! Milliseconds suffice because `wall_ms` is only the *coarse physical* component: `logical` orders
//! local and causally dependent events sharing a physical component, and `writer_id` orders independent
//! writers with equal HLC pairs. Nanosecond precision would neither eliminate cross-writer collisions
//! nor replace those fields. **That claim depends on this adapter's rules**, which are what the tests
//! below pin down:
//!
//! - it persists its last emitted HLC across restart ([`HlcClock::state`] / [`HlcClock::restore`]);
//! - it merges every observed remote HLC before issuing a causally dependent write ([`HlcClock::observe`]);
//! - clock rollback never decreases its state;
//! - the logical counter never wraps — on `u32` exhaustion it returns [`HlcError::LogicalExhausted`]
//!   and the caller must wait for a greater physical component.

use crate::{VERSION_BYTES, VersionStamp};

const _: () = assert!(
    VERSION_BYTES == 28,
    "the HLC adapter is the exact 28-byte order-key encoding"
);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Hlc {
    pub wall_ms: u64,
    pub logical: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HlcError {
    /// The logical counter is exhausted for this physical component. Wait for a greater `wall_ms`
    /// rather than wrapping, which would break the total order.
    #[error(
        "logical counter exhausted at wall_ms {wall_ms}; wait for a greater physical component"
    )]
    LogicalExhausted { wall_ms: u64 },
}

/// A single writer's clock. Not `Sync`-shared internally: a producer owns one and persists
/// [`HlcClock::state`] alongside its own durable writes.
#[derive(Debug, Clone)]
pub struct HlcClock {
    writer_id: [u8; 16],
    state: Hlc,
}

impl HlcClock {
    /// A fresh clock. Writer ids must be stable and unique within the producer domain.
    pub fn new(writer_id: [u8; 16]) -> Self {
        HlcClock {
            writer_id,
            state: Hlc::default(),
        }
    }

    /// Resume from persisted state after restart. Without this, a restarted writer could re-emit an
    /// order key it already used.
    pub fn restore(writer_id: [u8; 16], state: Hlc) -> Self {
        HlcClock { writer_id, state }
    }

    pub fn state(&self) -> Hlc {
        self.state
    }

    pub fn writer_id(&self) -> [u8; 16] {
        self.writer_id
    }

    /// Merge an observed remote clock. Must be called before issuing a causally dependent write, so the
    /// next stamp is strictly greater than the remote event it depends on.
    pub fn observe(&mut self, remote: Hlc) {
        self.state = self.state.max(remote);
    }

    /// The next stamp. `local_wall_ms` is the producer's physical reading; a *rollback* (a reading below
    /// the current state) never decreases state — it falls through to the logical counter.
    pub fn next(&mut self, local_wall_ms: u64) -> Result<VersionStamp, HlcError> {
        if local_wall_ms > self.state.wall_ms {
            self.state = Hlc {
                wall_ms: local_wall_ms,
                logical: 0,
            };
        } else {
            let logical = self
                .state
                .logical
                .checked_add(1)
                .ok_or(HlcError::LogicalExhausted {
                    wall_ms: self.state.wall_ms,
                })?;
            self.state.logical = logical;
        }
        Ok(encode(self.state, self.writer_id))
    }
}

/// Encode an HLC reading and writer id into a canonical order key.
pub fn encode(hlc: Hlc, writer_id: [u8; 16]) -> VersionStamp {
    let mut k = [0; VERSION_BYTES];
    k[0..8].copy_from_slice(&hlc.wall_ms.to_be_bytes());
    k[8..12].copy_from_slice(&hlc.logical.to_be_bytes());
    k[12..28].copy_from_slice(&writer_id);
    VersionStamp::new(k)
}

/// Interpret an order key as the adapter's HLC tuple. The encoding occupies all 28 bytes, so every
/// [`VersionStamp`] has one such interpretation; the tree itself never calls this.
pub fn decode(stamp: &VersionStamp) -> (Hlc, [u8; 16]) {
    let k = &stamp.order_key;
    let wall_ms = u64::from_be_bytes(k[0..8].try_into().expect("8"));
    let logical = u32::from_be_bytes(k[8..12].try_into().expect("4"));
    let writer_id: [u8; 16] = k[12..28].try_into().expect("16");
    (Hlc { wall_ms, logical }, writer_id)
}

#[cfg(test)]
mod tests {
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
}
