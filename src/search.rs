//! Head-first in-node search.
//!
//! For a byte string and a skip `s`, its **head** is the next eight bytes beginning at `s`, padded on
//! the right with zero and read as a big-endian `u64`. If two heads differ, their integer order *is*
//! their lexicographic order — so an integer compare replaces a `memcmp`. Equal heads are only a
//! *candidate range*: embedded zero bytes, short keys, and common prefixes require full-key comparison.
//!
//! The probe is three steps, and only step 2 is ever vectorized:
//!
//! 1. compare the probe with the surface's skipped common prefix;
//! 2. find the live range whose heads equal the probe head;
//! 3. resolve that range with the full byte comparator.
//!
//! Steps 1 and 3 are the correctness backstop. Step 2 selects between an autovectorized count reduction
//! and branchless binary search by live occupancy, at the measured crossover.

use crate::format::HEAD_BYTES;

/// Live-key count at or below which the linear count reduction beats a logarithmic search.
/// Located by `benches/search.rs`: the crossover sits between 64 and 256 live keys on both aarch64 and
/// x86-64, at every key shape measured. 128 is inside that band on every fixture.
pub const ADAPTIVE_THRESHOLD: usize = 128;

/// The head of `key` at `skip`: eight bytes, zero-padded right, big-endian.
///
/// Zero is *padding, not a sentinel*: an arbitrary key head occupies the whole `u64` domain, so no
/// value can mean "no key here". Unused lanes are masked by the live count instead.
#[inline]
pub fn head_of(key: &[u8], skip: usize) -> u64 {
    let rest = key.get(skip..).unwrap_or(&[]);
    let n = rest.len().min(HEAD_BYTES);
    let mut buf = [0u8; HEAD_BYTES];
    buf[..n].copy_from_slice(&rest[..n]);
    u64::from_be_bytes(buf)
}

/// Length of the longest common prefix of `a` and `b`.
#[inline]
pub fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    let n = a.len().min(b.len());
    let mut i = 0;
    while i < n && a[i] == b[i] {
        i += 1;
    }
    i
}

/// The head skip for a sorted surface: the common-prefix length of its live keys, which for a *sorted*
/// surface is exactly the common prefix of its first and last key. Empty and singleton surfaces use
/// zero, so the value is a deterministic function of the key set.
pub fn head_skip(first: Option<&[u8]>, last: Option<&[u8]>, count: usize) -> u32 {
    match (count, first, last) {
        (0 | 1, _, _) => 0,
        (_, Some(f), Some(l)) => common_prefix_len(f, l) as u32,
        _ => 0,
    }
}

/// Where a probe fell relative to the whole surface after step 1.
enum Step1 {
    /// The probe sorts before every key in the surface.
    BelowAll,
    /// The probe sorts after every key in the surface.
    AboveAll,
    /// The probe agrees on all `skip` prefix bytes; `head` is its suffix head.
    Inside { head: u64 },
}

/// Step 1: compare the probe with the skipped common prefix before looking at any suffix head.
fn step1(probe: &[u8], prefix: &[u8], skip: usize) -> Step1 {
    let n = probe.len().min(skip);
    for i in 0..n {
        if probe[i] != prefix[i] {
            return if probe[i] < prefix[i] {
                Step1::BelowAll
            } else {
                Step1::AboveAll
            };
        }
    }
    if probe.len() < skip {
        // The probe ends inside a matching common prefix, so it sorts before every longer key here.
        return Step1::BelowAll;
    }
    Step1::Inside {
        head: head_of(probe, skip),
    }
}

/// What one probe cost, for the harness's equal-head-range and comparison-per-probe percentiles.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProbeCost {
    /// Size of the equal-head range step 3 had to resolve.
    pub equal_head_range: u32,
    /// Full-key comparisons performed in step 3.
    pub full_comparisons: u32,
}

/// The outcome of one probe against a sorted surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Found {
    /// Index of the first key `>= probe` — the lower bound.
    pub index: usize,
    /// Whether `keys[index] == probe`.
    pub exact: bool,
    pub cost: ProbeCost,
}

impl Found {
    /// Count of keys `<= probe`. For a pivot surface this **is** the owning child index.
    pub fn upper_bound(&self) -> usize {
        self.index + usize::from(self.exact)
    }
}

/// A sorted key surface: a head column plus a way to read any full key. `heads` holds `count`
/// little-endian `u64`s; `prefix` is the surface's skipped common prefix (the first `skip` bytes of any
/// live key).
pub struct Surface<'a, F> {
    pub heads: &'a [u8],
    pub count: usize,
    pub skip: usize,
    pub prefix: &'a [u8],
    /// Full key by index. Reading a key is a *cold* blob access, so step 3 is what the format tries to
    /// keep rare.
    pub key: F,
}

impl<'a, F> Surface<'a, F>
where
    F: Fn(usize) -> &'a [u8],
{
    /// The full probe: steps 1, 2, and 3.
    pub fn probe(&self, needle: &[u8]) -> Found {
        if self.count == 0 {
            return Found {
                index: 0,
                exact: false,
                cost: ProbeCost::default(),
            };
        }
        let head = match step1(needle, self.prefix, self.skip) {
            Step1::BelowAll => {
                return Found {
                    index: 0,
                    exact: false,
                    cost: ProbeCost::default(),
                };
            }
            Step1::AboveAll => {
                return Found {
                    index: self.count,
                    exact: false,
                    cost: ProbeCost::default(),
                };
            }
            Step1::Inside { head } => head,
        };

        // Step 2: the equal-head range. Only this is ever vectorized.
        let (lo, hi) = self.equal_head_range(head);

        // Step 3: resolve the candidate range with the full byte comparator.
        let mut cost = ProbeCost {
            equal_head_range: (hi - lo) as u32,
            full_comparisons: 0,
        };
        let (mut a, mut b) = (lo, hi);
        while a < b {
            let mid = a + (b - a) / 2;
            cost.full_comparisons += 1;
            if (self.key)(mid) < needle {
                a = mid + 1;
            } else {
                b = mid;
            }
        }
        let exact = a < hi && {
            cost.full_comparisons += 1;
            (self.key)(a) == needle
        };
        Found {
            index: a,
            exact,
            cost,
        }
    }

    /// Step 2: `(first index with head >= probe, first index with head > probe)`. Small surfaces use
    /// the autovectorized count reduction; large surfaces use branchless binary search.
    #[inline]
    fn equal_head_range(&self, head: u64) -> (usize, usize) {
        let live = &self.heads[..self.count * HEAD_BYTES];
        if self.count <= ADAPTIVE_THRESHOLD {
            count_lt_le(live, head)
        } else {
            (
                partition_point_branchless(live, self.count, head, false),
                partition_point_branchless(live, self.count, head, true),
            )
        }
    }
}

#[inline]
fn head_at(heads: &[u8], i: usize) -> u64 {
    let at = i * HEAD_BYTES;
    u64::from_le_bytes(heads[at..at + HEAD_BYTES].try_into().expect("8 bytes"))
}

/// Branchless binary search: a fixed `ceil(log2(count+1))` steps, each a `cmov`-shaped select with no
/// data-dependent branch. Khuong & Morin's finding is that for an L1-resident array this beats the
/// branchy form, sometimes 2x, because a mispredict costs 10–15 cycles.
#[inline]
fn partition_point_branchless(heads: &[u8], count: usize, needle: u64, or_equal: bool) -> usize {
    let mut base = 0usize;
    let mut n = count;
    while n > 1 {
        let half = n / 2;
        let h = head_at(heads, base + half - 1);
        // `less` decides whether the answer is in the upper half; no branch on it.
        let less = if or_equal { h <= needle } else { h < needle };
        base += usize::from(less) * half;
        n -= half;
    }
    if n == 1 {
        let h = head_at(heads, base);
        let less = if or_equal { h <= needle } else { h < needle };
        base += usize::from(less);
    }
    base
}

/// The count reduction, in one pass: `(count of head < probe, count of head <= probe)`. No early exit,
/// so LLVM turns it into compare + mask + popcount. The heads are sorted, so a count *is* the
/// partition point — this was the highest-value strategy in the matrix precisely because it needs no
/// intrinsics.
#[inline]
fn count_lt_le(heads: &[u8], needle: u64) -> (usize, usize) {
    let mut lt = 0usize;
    let mut le = 0usize;
    for c in heads.chunks_exact(HEAD_BYTES) {
        let h = u64::from_le_bytes(c.try_into().expect("8 bytes"));
        lt += usize::from(h < needle);
        le += usize::from(h <= needle);
    }
    (lt, le)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The independent reference: a plain full-key binary search.
    fn reference(keys: &[Vec<u8>], needle: &[u8]) -> (usize, bool) {
        let i = keys.partition_point(|k| k.as_slice() < needle);
        (i, keys.get(i).map(|k| k.as_slice()) == Some(needle))
    }

    fn surface_probe(keys: &[Vec<u8>], needle: &[u8]) -> Found {
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
        Surface {
            heads: &heads,
            count: keys.len(),
            skip,
            prefix,
            key: |i: usize| keys[i].as_slice(),
        }
        .probe(needle)
    }

    /// The adversarial set the RFC names: empty keys, embedded zeroes, all-0xff heads, long shared
    /// prefixes, probes outside the prefix, and keys shorter than `skip`.
    fn hard_key_sets() -> Vec<Vec<Vec<u8>>> {
        let mut sets: Vec<Vec<Vec<u8>>> = Vec::new();
        sets.push(vec![]);
        sets.push(vec![b"".to_vec()]);
        sets.push(vec![b"".to_vec(), b"\x00".to_vec(), b"\x00\x00".to_vec()]);
        sets.push(vec![
            b"a".to_vec(),
            b"a\x00".to_vec(),
            b"a\x00\x00".to_vec(),
        ]);
        sets.push(vec![vec![0xff; 8], vec![0xff; 9], vec![0xff; 16]]);
        sets.push(
            (0..40u32)
                .map(|i| {
                    let mut k = b"a-very-long-shared-prefix-indeed/".to_vec();
                    k.extend_from_slice(format!("{i:04}").as_bytes());
                    k
                })
                .collect(),
        );
        // Non-uniform prefixes: one dense cluster plus scattered outliers.
        let mut mixed: Vec<Vec<u8>> = (0..32u32)
            .map(|i| format!("cluster/deep/path/{i:04}").into_bytes())
            .collect();
        mixed.push(b"z".to_vec());
        mixed.push(b"\x00".to_vec());
        mixed.sort();
        sets.push(mixed);
        sets.push(
            (0..64u32)
                .map(|i| format!("k{i:04}").into_bytes())
                .collect(),
        );
        sets
    }

    fn probes_for(keys: &[Vec<u8>]) -> Vec<Vec<u8>> {
        let mut ps: Vec<Vec<u8>> = vec![
            b"".to_vec(),
            b"\x00".to_vec(),
            vec![0xff; 12],
            b"a".to_vec(),
            b"zzzz".to_vec(),
        ];
        for k in keys {
            ps.push(k.clone());
            let mut before = k.clone();
            if let Some(l) = before.last_mut() {
                *l = l.wrapping_sub(1);
            }
            ps.push(before);
            let mut after = k.clone();
            after.push(0);
            ps.push(after);
            if k.len() > 1 {
                ps.push(k[..k.len() - 1].to_vec());
            }
        }
        ps
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "the exhaustive selected-search matrix is covered by native debug/release tests"
    )]
    fn selected_search_agrees_with_the_full_key_reference() {
        for keys in hard_key_sets() {
            let mut keys = keys;
            keys.sort();
            keys.dedup();
            for needle in probes_for(&keys) {
                let want = reference(&keys, &needle);
                let got = surface_probe(&keys, &needle);
                assert_eq!(
                    (got.index, got.exact),
                    want,
                    "needle {needle:?} in {keys:?}"
                );
            }
        }
    }

    #[test]
    fn upper_bound_is_the_owning_child_index() {
        // Pivot semantics: pivots[i] = min key of child i+1, so child index = count of pivots <= probe.
        let pivots: Vec<Vec<u8>> = vec![b"d".to_vec(), b"h".to_vec(), b"p".to_vec()];
        for (probe, want) in [
            (&b"a"[..], 0),
            (&b"d"[..], 1),
            (&b"e"[..], 1),
            (&b"h"[..], 2),
            (&b"p"[..], 3),
            (&b"zz"[..], 3),
        ] {
            assert_eq!(surface_probe(&pivots, probe).upper_bound(), want);
        }
    }

    #[test]
    fn heads_order_agrees_with_lexicographic_order_when_they_differ() {
        let samples: Vec<Vec<u8>> = vec![
            b"".to_vec(),
            b"\x00".to_vec(),
            b"a".to_vec(),
            b"a\x00".to_vec(),
            b"ab".to_vec(),
            vec![0xff; 8],
            vec![0xff; 9],
            b"abcdefgh".to_vec(),
            b"abcdefghi".to_vec(),
        ];
        for a in &samples {
            for b in &samples {
                let (ha, hb) = (head_of(a, 0), head_of(b, 0));
                if ha != hb {
                    assert_eq!(
                        ha.cmp(&hb),
                        a.cmp(b),
                        "head order must be lexicographic order for {a:?} vs {b:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn head_skip_is_zero_for_empty_and_singleton_surfaces() {
        assert_eq!(head_skip(None, None, 0), 0);
        assert_eq!(head_skip(Some(b"abcdef"), Some(b"abcdef"), 1), 0);
        assert_eq!(head_skip(Some(b"abcX"), Some(b"abcY"), 2), 3);
    }
}
