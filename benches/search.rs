//! Throughput and cold-key-comparison cost of the selected occupancy-adaptive in-node search.
//!
//! It also reports equal-head-range percentiles and full-key comparisons per probe, because if step 3
//! averages more than about two full-key comparisons then optimizing step 2 is optimizing the wrong
//! thing.

#[path = "../tests/support/mod.rs"]
mod support;

use std::sync::Arc;

use cbe_tree::codec::{self, Entry, NodeView};
use cbe_tree::format::{Format, FormatParams};
use cbe_tree::{VERSION_BYTES, VersionStamp};
use bytes::Bytes;
use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use support::{Histogram, KeyShape, Rng};

fn ok(n: u64) -> [u8; VERSION_BYTES] {
    VersionStamp::from_counter(n).order_key
}

/// A leaf holding `n` keys of the given shape, at a format whose slot capacity fits them.
fn leaf(shape: KeyShape, n: usize) -> (Arc<Format>, Arc<NodeView>, Vec<Bytes>) {
    let keys = shape.keys(n, 12345);
    let n = keys.len();
    let fmt = Arc::new(
        Format::new(FormatParams {
            node_bytes: 256 * 1024,
            f_max: 32,
            leaf_slots: n.max(4),
            message_slots: n.max(4),
            max_key_bytes: 256,
            inline_value_bytes: 64,
            max_value_bytes: 1 << 20,
            max_object_bytes: 2 << 20,
            max_tree_level: 32,
            version_domain: *b"be-tree/bench\0\0\0",
        })
        .expect("bench format"),
    );
    let entries: Vec<Entry> = keys
        .iter()
        .enumerate()
        .map(|(i, k)| Entry::inline(k.clone(), ok(i as u64), Bytes::from_static(b"v")))
        .collect();
    let bytes = codec::encode_leaf(&fmt, &entries).expect("fits");
    let view = Arc::new(NodeView::decode(&fmt, None, bytes).expect("valid"));
    (fmt, view, keys)
}

/// Probes: half hits, half misses, in a fixed pseudo-random order.
fn probes(keys: &[Bytes], count: usize) -> Vec<Bytes> {
    let mut rng = Rng::new(0xF00D);
    (0..count)
        .map(|i| {
            let k = &keys[rng.below(keys.len())];
            if i % 2 == 0 {
                k.clone()
            } else {
                let mut m = k.to_vec();
                m.push(0x7f); // just past a stored key: a miss inside the same equal-head range
                Bytes::from(m)
            }
        })
        .collect()
}

fn search(c: &mut Criterion) {
    for shape in KeyShape::all().iter().copied() {
        for &n in &[16usize, 32, 64, 96, 128, 192, 256, 384, 640, 2048] {
            let (_fmt, view, keys) = leaf(shape, n);
            if keys.len() < n {
                continue;
            }
            let ps = probes(&keys, 512);
            let mut g = c.benchmark_group(format!("search/{}/{n}", shape.name()));
            g.throughput(criterion::Throughput::Elements(ps.len() as u64));
            g.bench_function("selected", |b| {
                b.iter(|| {
                    let s = view.entry_surface();
                    let mut acc = 0usize;
                    for p in &ps {
                        let f = s.probe(p);
                        acc += f.index + usize::from(f.exact);
                    }
                    acc
                })
            });
            g.finish();
        }
    }
}

/// The number that decides whether step 2 matters at all.
fn report_probe_costs(_c: &mut Criterion) {
    println!("\n=== equal-head-range and full-key comparisons per probe ===");
    println!(
        "{:20} {:>6} {:>10} {:>10} {:>10} {:>10} {:>10}",
        "key shape", "n", "eqhead_p50", "eqhead_p90", "eqhead_p99", "cmps_mean", "cmps_p99"
    );
    for shape in KeyShape::all().iter().copied() {
        for &n in &[64usize, 640, 2048] {
            let (_fmt, view, keys) = leaf(shape, n);
            if keys.len() < n {
                continue;
            }
            let hits = probes(&keys, 4096);
            let eq = Histogram::default();
            let cmps = Histogram::default();
            let s = view.entry_surface();
            for p in &hits {
                let f = s.probe(p);
                eq.record(u64::from(f.cost.equal_head_range));
                cmps.record(u64::from(f.cost.full_comparisons));
            }
            println!(
                "{:20} {:>6} {:>10} {:>10} {:>10} {:>10.2} {:>10}",
                shape.name(),
                n,
                eq.quantile(0.5),
                eq.quantile(0.9),
                eq.quantile(0.99),
                cmps.mean(),
                cmps.quantile(0.99),
            );
        }
    }
    let _ = BatchSize::SmallInput;
}

criterion_group!(benches, report_probe_costs, search);
criterion_main!(benches);
