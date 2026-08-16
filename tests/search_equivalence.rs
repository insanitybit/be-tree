//! The selected head-first search must be property-equivalent to a safe full-key reference — on indexes
//! and matches, not merely "close enough".
//!
//! The reference here is deliberately *not* the head-first algorithm: it is a plain full-key binary
//! search over the same sorted keys. That keeps the oracle independent of the thing under test, so a bug
//! in the head construction cannot validate itself.

mod support;

use std::sync::Arc;

use cbe_tree::codec::{self, Entry, NodeView};
use cbe_tree::format::{Format, FormatParams};
use cbe_tree::search::{Surface, head_of, head_skip};
use cbe_tree::{VERSION_BYTES, VersionStamp};
use bytes::Bytes;
use support::Rng;

fn ok(n: u64) -> [u8; VERSION_BYTES] {
    VersionStamp::from_counter(n).order_key
}

/// The independent oracle.
fn reference(keys: &[Vec<u8>], needle: &[u8]) -> (usize, bool) {
    let i = keys.partition_point(|k| k.as_slice() < needle);
    (i, keys.get(i).map(|k| k.as_slice()) == Some(needle))
}

/// Build a bare surface (no node) so key sets that no format could hold are still covered.
fn bare_probe(keys: &[Vec<u8>], needle: &[u8]) -> (usize, bool) {
    let skip = head_skip(
        keys.first().map(|k| k.as_slice()),
        keys.last().map(|k| k.as_slice()),
        keys.len(),
    ) as usize;
    let heads: Vec<u8> = keys
        .iter()
        .flat_map(|k| head_of(k, skip).to_le_bytes())
        .collect();
    let prefix: &[u8] = keys.first().map(|k| &k[..skip.min(k.len())]).unwrap_or(&[]);
    let f = Surface {
        heads: &heads,
        count: keys.len(),
        skip,
        prefix,
        key: |i: usize| keys[i].as_slice(),
    }
    .probe(needle);
    (f.index, f.exact)
}

/// Randomized key sets that deliberately land on the hard cases: short keys, embedded zeroes, keys that
/// are prefixes of each other, all-`0xff` heads, and non-uniform shared prefixes.
fn random_keys(rng: &mut Rng, n: usize, max_len: usize) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::with_capacity(n);
    for _ in 0..n {
        let len = rng.below(max_len + 1);
        let mut k = match rng.below(6) {
            0 => rng.bytes(len),
            1 => vec![0u8; len],
            2 => vec![0xff; len],
            3 => {
                let mut k = b"common/prefix/that/is/long/".to_vec();
                k.extend_from_slice(&rng.bytes(len));
                k
            }
            4 => {
                // A key that is a strict prefix of another, plus a trailing zero variant.
                let mut k = rng.bytes(len);
                if rng.below(2) == 0 {
                    k.push(0);
                }
                k
            }
            _ => {
                let mut k = rng.bytes(len.min(3));
                k.extend_from_slice(&[0u8; 5]);
                k.extend_from_slice(&rng.bytes(len.min(2)));
                k
            }
        };
        k.truncate(64);
        out.push(k);
    }
    out.sort();
    out.dedup();
    out
}

/// Probes chosen to hit every boundary: exact keys, keys minus/plus one byte, keys with a byte
/// decremented or incremented, and probes entirely outside the surface.
fn boundary_probes(keys: &[Vec<u8>], rng: &mut Rng) -> Vec<Vec<u8>> {
    let mut ps: Vec<Vec<u8>> = vec![
        Vec::new(),
        vec![0],
        vec![0, 0],
        vec![0xff; 1],
        vec![0xff; 9],
        b"common/prefix".to_vec(),
        b"common/prefix/that/is/long/".to_vec(),
    ];
    for k in keys {
        ps.push(k.clone());
        if !k.is_empty() {
            ps.push(k[..k.len() - 1].to_vec());
            let mut dec = k.clone();
            let last = dec.len() - 1;
            dec[last] = dec[last].wrapping_sub(1);
            ps.push(dec);
            let mut inc = k.clone();
            inc[last] = inc[last].wrapping_add(1);
            ps.push(inc);
        }
        let mut longer = k.clone();
        longer.push(0);
        ps.push(longer);
        let mut longer = k.clone();
        longer.push(0xff);
        ps.push(longer);
    }
    for _ in 0..32 {
        let n = rng.below(20);
        ps.push(rng.bytes(n));
    }
    ps
}

#[test]
#[cfg_attr(miri, ignore = "native randomized surface matrix")]
fn selected_search_matches_the_full_key_reference_on_random_surfaces() {
    for seed in 0..64u64 {
        let mut rng = Rng::new(seed);
        let n = 1 + rng.below(300);
        let max_len = 1 + rng.below(40);
        let keys = random_keys(&mut rng, n, max_len);
        if keys.is_empty() {
            continue;
        }
        for needle in boundary_probes(&keys, &mut rng) {
            let want = reference(&keys, &needle);
            assert_eq!(
                bare_probe(&keys, &needle),
                want,
                "seed {seed}: search disagreed on {needle:?} (n = {})",
                keys.len()
            );
        }
    }
}

/// The same equivalence, but through a *real validated node* — so the head column and `head_skip` that
/// decode recomputed are the ones being searched.
#[test]
#[cfg_attr(miri, ignore = "native validated-node search matrix")]
fn selected_search_matches_the_reference_through_a_validated_node() {
    for seed in 100..140u64 {
        let mut rng = Rng::new(seed);
        let n = 1 + rng.below(200);
        let keys = random_keys(&mut rng, n, 32);
        if keys.is_empty() {
            continue;
        }
        let fmt = Arc::new(
            Format::new(FormatParams {
                node_bytes: 64 * 1024,
                f_max: 8,
                leaf_slots: keys.len().max(4),
                message_slots: keys.len().max(4),
                max_key_bytes: 64,
                inline_value_bytes: 32,
                max_value_bytes: 1 << 16,
                max_object_bytes: 1 << 17,
                max_tree_level: 8,
                version_domain: *b"be-tree/eqtest\0\0",
            })
            .expect("format"),
        );
        let entries: Vec<Entry> = keys
            .iter()
            .enumerate()
            .map(|(i, k)| {
                Entry::inline(
                    Bytes::copy_from_slice(k),
                    ok(i as u64),
                    Bytes::from_static(b"v"),
                )
            })
            .collect();
        let bytes = codec::encode_leaf(&fmt, &entries).expect("fits");
        let view = NodeView::decode(&fmt, None, bytes).expect("valid");

        for needle in boundary_probes(&keys, &mut rng) {
            let want = reference(&keys, &needle);
            let f = view.find(&needle);
            assert_eq!(
                (f.index, f.exact),
                want,
                "seed {seed}: search disagreed through a node on {needle:?}"
            );
        }
    }
}

/// Pivot semantics: `child_of` must be the count of pivots `<= probe`.
#[test]
#[cfg_attr(miri, ignore = "native child-routing search matrix")]
fn child_routing_agrees_with_the_reference() {
    for seed in 200..230u64 {
        let mut rng = Rng::new(seed);
        let f_max = 3 + rng.below(60);
        let pivots = random_keys(&mut rng, f_max - 1, 24);
        if pivots.len() < 2 {
            continue;
        }
        let fmt = Arc::new(
            Format::new(FormatParams {
                node_bytes: 64 * 1024,
                f_max: pivots.len() + 1,
                leaf_slots: 8,
                message_slots: 8,
                max_key_bytes: 64,
                inline_value_bytes: 32,
                max_value_bytes: 1 << 16,
                max_object_bytes: 1 << 17,
                max_tree_level: 8,
                version_domain: *b"be-tree/eqtest\0\0",
            })
            .expect("format"),
        );
        let pivot_bytes: Vec<Bytes> = pivots.iter().map(|p| Bytes::copy_from_slice(p)).collect();
        let children: Vec<cbe_tree::BlockId> = (0..pivots.len() + 1)
            .map(|i| cbe_tree::BlockId([(i as u8) | 0x80; 32]))
            .collect();
        let bytes = codec::encode_internal(&fmt, 1, &pivot_bytes, &children, &[]).expect("fits");
        let view = NodeView::decode(&fmt, None, bytes).expect("valid");

        for needle in boundary_probes(&pivots, &mut rng) {
            // Independent oracle for routing.
            let want = pivots
                .iter()
                .filter(|p| p.as_slice() <= needle.as_slice())
                .count();
            assert_eq!(
                view.child_of(&needle),
                want,
                "seed {seed}: routed {needle:?} wrongly"
            );
        }
    }
}

/// Exercise both sides of the occupancy dispatch boundary, where an off-by-one would hide.
#[test]
#[cfg_attr(miri, ignore = "native adaptive-threshold matrix")]
fn the_occupancy_dispatch_agrees_with_the_reference_at_its_threshold() {
    let t = cbe_tree::search::ADAPTIVE_THRESHOLD;
    for n in [t - 1, t, t + 1] {
        let mut rng = Rng::new(n as u64);
        let keys: Vec<Vec<u8>> = {
            let mut k: Vec<Vec<u8>> = (0..n).map(|i| format!("k{i:06}").into_bytes()).collect();
            k.sort();
            k
        };
        for needle in boundary_probes(&keys, &mut rng) {
            let want = reference(&keys, &needle);
            assert_eq!(
                bare_probe(&keys, &needle),
                want,
                "n = {n}: search disagreed at the dispatch boundary on {needle:?}"
            );
        }
    }
}
