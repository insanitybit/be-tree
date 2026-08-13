//! Deterministic workloads for instruction/cache/branch profilers.
//!
//! This is deliberately not Criterion: external profilers need a fixed amount of work, with no warmup,
//! sampling, or statistical loop hidden inside the executable.

#[path = "profile/support.rs"]
mod support;

fn usage() -> ! {
    eprintln!(
        "usage: profile <setup|setup-get-many|setup-hash|setup-decode|get-many|scan|scan-stream|apply|hash|decode> [iterations]"
    );
    std::process::exit(2);
}

#[cfg_attr(feature = "hotpath", hotpath::main)]
fn main() {
    let mut args = std::env::args().skip(1);
    let scenario = args.next().unwrap_or_else(|| usage());
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
        _ => usage(),
    };
    println!("scenario={scenario} iterations={iterations} checksum={checksum}");
}
