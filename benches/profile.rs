//! Deterministic workloads for instruction/cache/branch profilers.
//!
//! This is deliberately not Criterion: external profilers need a fixed amount of work, with no warmup,
//! sampling, or statistical loop hidden inside the executable.

#[path = "profile/support.rs"]
mod support;

fn usage() -> ! {
    eprintln!(
        "usage: profile <setup|setup-get-many|setup-hash|setup-decode|get-many|\
         get-many-WIDTH-(sorted|random)-(hits|misses|mixed)|scan|scan-stream|\
         scan-2|scan-32|scan-256|scan-stream-2|scan-stream-32|scan-stream-256|\
         get-cold-short|get-cold-long|get-hot-short|get-hot-long|\
         scan-tombstone|scan-stream-tombstone|apply|apply-WIDTH-(repeated|distinct|delete|mixed)|\
         cow-low|cow-high|setup-cow-low|setup-cow-high|hash|decode> [iterations]"
    );
    std::process::exit(2);
}

#[cfg_attr(feature = "hotpath", hotpath::main)]
fn main() {
    let mut args = std::env::args().skip(1);
    let Some(scenario) = args.next() else {
        // `cargo test --all-targets` invokes harness-free benches without CLI arguments.
        return;
    };
    let iterations: usize = args
        .next()
        .map(|arg| arg.parse().unwrap_or_else(|_| usage()))
        .unwrap_or(1);
    if args.next().is_some() {
        usage();
    }

    let checksum = match scenario.as_str() {
        "setup-hash" => support::hash_bytes().len() as u64,
        "hash" => {
            let bytes = support::hash_bytes();
            support::hash(&bytes, iterations)
        }
        "setup-decode" => support::encoded_node().1.len() as u64,
        "decode" => {
            let (format, bytes) = support::encoded_node();
            support::decode(&format, &bytes, iterations)
        }
        shape
            if matches!(
                shape,
                "setup-get-cold-short"
                    | "setup-get-cold-long"
                    | "setup-get-hot-short"
                    | "setup-get-hot-long"
            ) =>
        {
            let long_prefix = shape.ends_with("long");
            let warm_cache = shape.contains("hot");
            let runtime = support::runtime();
            runtime.block_on(support::point_fixture(long_prefix, warm_cache))
        }
        shape
            if matches!(
                shape,
                "get-cold-short" | "get-cold-long" | "get-hot-short" | "get-hot-long"
            ) =>
        {
            let long_prefix = shape.ends_with("long");
            let warm_cache = shape.contains("hot");
            let runtime = support::runtime();
            runtime.block_on(async {
                let fixture = if long_prefix {
                    support::fixture_with_prefix(
                        "profile/long-common-prefix/with-many-shared-bytes/",
                    )
                    .await
                } else {
                    support::fixture().await
                };
                if warm_cache {
                    support::warm(&fixture).await;
                    // Hot: reread one warmed key, so every measured call is a cache hit.
                    support::point_get(&fixture, iterations).await
                } else {
                    // Cold: distinct strided keys, so measured leaf loads stay genuine misses.
                    support::point_get_distinct(&fixture, iterations).await
                }
            })
        }
        "setup" | "setup-get-many" | "get-many" | "scan" | "scan-stream" | "apply" => {
            let runtime = support::runtime();
            runtime.block_on(async {
                let fixture = support::fixture().await;
                match scenario.as_str() {
                    "setup" => fixture.root.0[0] as u64,
                    "setup-get-many" => {
                        support::warm(&fixture).await;
                        fixture.root.0[0] as u64
                    }
                    "get-many" => {
                        support::warm(&fixture).await;
                        support::get_many(&fixture, iterations).await
                    }
                    "scan" => support::scan(&fixture, iterations).await,
                    "scan-stream" => support::scan_stream(&fixture, iterations).await,
                    "apply" => support::apply(&fixture, iterations).await,
                    _ => unreachable!(),
                }
            })
        }
        shape if shape.starts_with("setup-get-many-") => {
            let runtime = support::runtime();
            runtime.block_on(async {
                let fixture = support::fixture().await;
                support::warm(&fixture).await;
                let queries = support::get_many_queries(&fixture, &shape[15..]);
                std::hint::black_box(queries.len() as u64)
            })
        }
        shape if shape.starts_with("get-many-") => {
            let runtime = support::runtime();
            runtime.block_on(async {
                let fixture = support::fixture().await;
                support::warm(&fixture).await;
                let queries = support::get_many_queries(&fixture, &shape[9..]);
                support::get_many_prepared(&fixture, &queries, iterations).await
            })
        }
        "scan-tombstone"
        | "scan-stream-tombstone"
        | "setup-scan-tombstone"
        | "setup-scan-stream-tombstone" => {
            let runtime = support::runtime();
            runtime.block_on(async {
                // The tombstone fixture includes an extra delete batch, so these shapes need their
                // own setup twin: subtracting the plain fixture would attribute that apply to the scan.
                let fixture = support::tombstone_fixture().await;
                match scenario.as_str() {
                    "setup-scan-tombstone" | "setup-scan-stream-tombstone" => {
                        fixture.root.0[0] as u64
                    }
                    "scan-tombstone" => support::scan_tombstones(&fixture, iterations).await,
                    _ => support::scan_stream_tombstones(&fixture, iterations).await,
                }
            })
        }
        "setup-cow-low" | "setup-cow-high" | "cow-low" | "cow-high" => {
            let runtime = support::runtime();
            runtime.block_on(async {
                let fixture = support::fixture().await;
                let overlap = scenario.ends_with("high");
                // Both variants build the same batches; only the measured variant applies them, so
                // the setup twin is shape-matched and the delta is exactly `iterations` commits of
                // rewrite work. Run the setup twin with the SAME iteration count.
                let mut batches = support::cow_batches(&fixture, overlap, iterations);
                let mut roots = Vec::with_capacity(batches.len());
                if scenario.starts_with("setup-") {
                    std::hint::black_box(batches.len() as u64)
                } else {
                    support::cow_apply(&fixture, &mut batches, &mut roots).await
                }
            })
        }
        shape if shape == "scan-2" || shape == "scan-32" || shape == "scan-256" => {
            let rows = shape[5..].parse().expect("scan row count");
            let runtime = support::runtime();
            runtime.block_on(async {
                let fixture = support::fixture().await;
                support::scan_rows(&fixture, rows, iterations).await
            })
        }
        shape
            if shape == "scan-stream-2"
                || shape == "scan-stream-32"
                || shape == "scan-stream-256" =>
        {
            let rows = shape[12..].parse().expect("scan row count");
            let runtime = support::runtime();
            runtime.block_on(async {
                let fixture = support::fixture().await;
                support::scan_stream_rows(&fixture, rows, iterations).await
            })
        }
        shape if shape.starts_with("apply-") => {
            let runtime = support::runtime();
            runtime.block_on(async {
                let fixture = support::fixture().await;
                support::apply_named(&fixture, iterations, &shape[6..]).await
            })
        }
        _ => usage(),
    };
    println!("scenario={scenario} iterations={iterations} checksum={checksum}");
}
