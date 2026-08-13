//! The capacity matrix: 4 KiB, 16 KiB, 64 KiB, 256 KiB, and 1 MiB regular nodes.
//!
//! This benchmark **selects** `NODE_BYTES`, `F_MAX`, and the slot capacities. It reports the numbers the
//! choice actually turns on: realized depth and fanout, physical bytes per live logical byte, bytes read
//! per point lookup, victim bytes per flush, and the diagnostic effective ε.
//!
//! It also compares whole leaves against independently content-addressed leaf *partitions*. An in-object
//! partition cannot reduce fetch or verification bytes under this store model, so only a separately
//! addressed partition is a meaningful point-read experiment — which is exactly what a smaller
//! `NODE_BYTES` at the same total corpus already measures.

#[path = "../tests/support/mod.rs"]
mod support;

use std::sync::Arc;

use be_tree::BeTree;
use be_tree::format::{Format, FormatParams};
use be_tree::store::MemStore;
use bytes::Bytes;
use criterion::{Criterion, criterion_group, criterion_main};
use support::{self as harness, CountingStore, KeyShape, StoreModel};

/// A candidate format at `node_bytes`, with slot capacities scaled so descriptors and blob stay balanced
/// for records of about `record_bytes`.
fn candidate(node_bytes: usize, f_max: usize, record_bytes: usize) -> Option<Format> {
    // Descriptor cost per slot is fixed by the format; aim for slots ~= node_bytes / (desc + record).
    let desc = 8 + 8 + be_tree::VERSION_BYTES + 1 + 4;
    let slots = (node_bytes / (desc + record_bytes)).max(4);
    // The worst-case pivot reservation is `(f_max - 1) * max_key_bytes`, so a small node with a large key
    // limit is *rejected by the format proofs* rather than silently made to work. Walk the key limit down
    // to the largest power of two this (node_bytes, f_max) pair can actually reserve for, and report it —
    // that trade is itself one of the matrix's findings.
    for max_key_bytes in [4096usize, 1024, 256, 128, 64, 32, 16] {
        let inline_value_bytes = 512.min(max_key_bytes * 2);
        if let Ok(f) = Format::new(FormatParams {
            node_bytes,
            f_max,
            leaf_slots: slots,
            message_slots: slots,
            max_key_bytes,
            inline_value_bytes,
            max_value_bytes: (4 << 20) - be_tree::value::ENVELOPE_BYTES,
            max_object_bytes: 8 << 20,
            max_tree_level: 32,
            version_domain: *b"be-tree/bench\0\0\0",
        }) {
            return Some(f);
        }
    }
    None
}

const SIZES: &[usize] = &[4 * 1024, 16 * 1024, 64 * 1024, 256 * 1024, 1024 * 1024];
const FANOUTS: &[usize] = &[8, 16, 32, 64, 128];

/// The table the constant selection is made from. Printed, not asserted: it is a measurement.
fn report_matrix(_c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    const KEYS: usize = 100_000;
    const RECORD: usize = 40;

    println!("\n=== capacity matrix ({KEYS} keys, ~{RECORD}B records, uniform random) ===");
    println!(
        "{:>8} {:>6} {:>7} {:>7} {:>6} {:>6} {:>7} {:>7} {:>8} {:>9} {:>10} {:>9} {:>8}",
        "node",
        "f_max",
        "slots",
        "max_key",
        "depth",
        "fanout",
        "nodes",
        "amp",
        "eff_eps",
        "min_flush",
        "victim",
        "rd_bytes",
        "waves"
    );
    for &node_bytes in SIZES {
        for &f_max in FANOUTS {
            let Some(fmt) = candidate(node_bytes, f_max, RECORD) else {
                continue;
            };
            let keys = KeyShape::UniformRandom.keys(KEYS, 7);
            let mem = Arc::new(MemStore::new());
            let counting = Arc::new(CountingStore::new(mem));
            let t = BeTree::with_format(counting.clone(), fmt.clone()).record_metrics();

            let (root, _model) = rt
                .block_on(harness::build(&t, &keys, 256, RECORD - 20))
                .expect("build");
            let shape = rt
                .block_on(harness::check_balanced(&t, root))
                .expect("balanced");

            // Cold point reads: a fresh handle so nothing is cached.
            let cold = BeTree::with_format(counting.clone(), fmt.clone()).record_metrics();
            let probe: Vec<&[u8]> = keys
                .iter()
                .step_by(keys.len() / 256)
                .map(|k| k.as_ref())
                .collect();
            let n_probes = probe.len() as f64;
            rt.block_on(cold.get_many(root, &probe)).expect("read");

            println!(
                "{:>8} {:>6} {:>7} {:>7} {:>6} {:>6.1} {:>7} {:>7.2} {:>8.3} {:>9} {:>10.0} {:>9.0} {:>8}",
                node_bytes,
                f_max,
                fmt.leaf_slots(),
                fmt.max_key_bytes(),
                shape.max_leaf_depth,
                shape.mean_fanout(),
                shape.nodes,
                shape.space_amplification(),
                shape.effective_epsilon(fmt.leaf_slots() as f64),
                fmt.min_flush_bytes(),
                t.metrics().victim_bytes.mean,
                cold.metrics().bytes_read as f64 / n_probes,
                cold.metrics().waves,
            );
        }
    }

    // Compare the measured depths of the 16 KiB/f_max=16 family and selected 64 KiB family. The former
    // supports keys only through 1 KiB while the latter supports 4 KiB, so this crossover applies only
    // to consumers whose key contract fits both. The target supplies bandwidth; this prints the RTT
    // crossover instead of pretending MemStore selects a universal node size.
    println!("\n=== 16 KiB vs 64 KiB cold-point crossover (100k-key measured depths) ===");
    println!("{:>12} {:>18}", "bandwidth", "64KiB wins above");
    for bandwidth_mib in [100u64, 500, 1024] {
        let bandwidth = bandwidth_mib * 1024 * 1024;
        let ns =
            StoreModel::crossover_round_trip_ns((3, 3 * 64 * 1024), (4, 4 * 16 * 1024), bandwidth)
                .expect("valid comparison");
        println!(
            "{:>9} MiB/s {:>14.3} ms",
            bandwidth_mib,
            ns as f64 / 1_000_000.0
        );
    }
}

/// Latency at the sizes that survived the table.
fn point_and_batch_reads(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    const KEYS: usize = 50_000;
    for &node_bytes in &[4 * 1024usize, 16 * 1024, 64 * 1024, 256 * 1024] {
        let Some(fmt) = candidate(node_bytes, 32, 40) else {
            continue;
        };
        let store = Arc::new(MemStore::new());
        let t = BeTree::with_format(store.clone(), fmt.clone());
        let keys = KeyShape::UniformRandom.keys(KEYS, 7);
        let (root, _m) = rt.block_on(harness::build(&t, &keys, 256, 20)).unwrap();

        let one: Vec<&[u8]> = vec![keys[KEYS / 3].as_ref()];
        let many: Vec<&[u8]> = keys
            .iter()
            .step_by(KEYS / 256)
            .map(|k| k.as_ref())
            .collect();

        let mut g = c.benchmark_group(format!("read/{}KiB", node_bytes / 1024));
        g.bench_function("point/warm", |b| {
            b.iter(|| rt.block_on(t.get_many(root, &one)).unwrap())
        });
        g.bench_function("get_many_256/warm", |b| {
            b.iter(|| rt.block_on(t.get_many(root, &many)).unwrap())
        });
        g.bench_function("point/cold", |b| {
            b.iter(|| {
                let cold = BeTree::with_format(store.clone(), fmt.clone());
                rt.block_on(cold.get_many(root, &one)).unwrap()
            })
        });
        g.finish();
    }
}

/// Whole leaves against separately addressed leaf partitions, at a fixed corpus. A smaller `NODE_BYTES`
/// with the same key set *is* the separately-addressed-partition experiment: it is the only variant that
/// reduces fetched and verified bytes per point read under this store model.
fn leaf_partitioning(_c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    const KEYS: usize = 50_000;
    println!("\n=== whole leaves vs separately addressed leaf partitions (point read) ===");
    println!(
        "{:>10} {:>8} {:>12} {:>14} {:>12}",
        "node", "depth", "objects", "bytes/lookup", "verified"
    );
    for &node_bytes in SIZES {
        let Some(fmt) = candidate(node_bytes, 32, 40) else {
            continue;
        };
        let store = Arc::new(MemStore::new());
        let t = BeTree::with_format(store.clone(), fmt.clone());
        let keys = KeyShape::UniformRandom.keys(KEYS, 7);
        let (root, _m) = rt.block_on(harness::build(&t, &keys, 256, 20)).unwrap();
        let shape = rt.block_on(harness::check_balanced(&t, root)).unwrap();

        let cold = BeTree::with_format(store, fmt).record_metrics();
        let probe: Vec<Bytes> = keys.iter().step_by(KEYS / 64).cloned().collect();
        for k in &probe {
            rt.block_on(cold.get(root, k)).unwrap();
        }
        println!(
            "{:>10} {:>8} {:>12} {:>14.0} {:>12.0}",
            node_bytes,
            shape.max_leaf_depth,
            shape.nodes,
            cold.metrics().bytes_read as f64 / probe.len() as f64,
            cold.metrics().bytes_verified as f64 / probe.len() as f64,
        );
    }
}

criterion_group!(
    benches,
    report_matrix,
    leaf_partitioning,
    point_and_batch_reads
);
criterion_main!(benches);
