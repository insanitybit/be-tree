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
/// The adversarial set: empty keys, embedded zeroes, all-0xff heads, long shared
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
