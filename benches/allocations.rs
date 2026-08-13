//! Exact region-scoped allocation counts for the deterministic profile workloads.

#[path = "profile/support.rs"]
mod support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

struct CountingAllocator;

static ENABLED: AtomicBool = AtomicBool::new(false);
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);
static REALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static CURRENT_LIVE_BYTES: AtomicU64 = AtomicU64::new(0);
static BASELINE_LIVE_BYTES: AtomicU64 = AtomicU64::new(0);
static PEAK_LIVE_BYTES: AtomicU64 = AtomicU64::new(0);

fn add_live(bytes: u64) {
    let live = CURRENT_LIVE_BYTES.fetch_add(bytes, Relaxed) + bytes;
    if ENABLED.load(Relaxed) {
        PEAK_LIVE_BYTES.fetch_max(live, Relaxed);
    }
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            add_live(layout.size() as u64);
            if ENABLED.load(Relaxed) {
                ALLOCATIONS.fetch_add(1, Relaxed);
                ALLOCATED_BYTES.fetch_add(layout.size() as u64, Relaxed);
            }
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        CURRENT_LIVE_BYTES.fetch_sub(layout.size() as u64, Relaxed);
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, old: Layout, new_size: usize) -> *mut u8 {
        let replacement = unsafe { System.realloc(pointer, old, new_size) };
        if !replacement.is_null() {
            if new_size >= old.size() {
                add_live((new_size - old.size()) as u64);
            } else {
                CURRENT_LIVE_BYTES.fetch_sub((old.size() - new_size) as u64, Relaxed);
            }
            if ENABLED.load(Relaxed) {
                REALLOCATIONS.fetch_add(1, Relaxed);
                ALLOCATED_BYTES.fetch_add(new_size as u64, Relaxed);
            }
        }
        replacement
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

fn reset() {
    ALLOCATIONS.store(0, Relaxed);
    ALLOCATED_BYTES.store(0, Relaxed);
    REALLOCATIONS.store(0, Relaxed);
    let baseline = CURRENT_LIVE_BYTES.load(Relaxed);
    BASELINE_LIVE_BYTES.store(baseline, Relaxed);
    PEAK_LIVE_BYTES.store(baseline, Relaxed);
}

fn usage() -> ! {
    eprintln!(
        "usage: allocations <get-many|\
        get-many-WIDTH-(sorted|random)-(hits|misses|mixed)|scan|scan-stream|\
        scan-tombstone|scan-stream-tombstone|apply|apply-WIDTH-(repeated|distinct|delete)|\
        cow-low|cow-high|hash|decode> [iterations]"
    );
    std::process::exit(2);
}

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
    if args.next().is_some() || scenario == "setup" {
        usage();
    }

    let runtime = support::runtime();
    let is_get_many = scenario == "get-many" || scenario.starts_with("get-many-");
    let is_tombstone_scan = matches!(
        scenario.as_str(),
        "scan-tombstone" | "scan-stream-tombstone"
    );
    let is_apply = scenario == "apply" || scenario.starts_with("apply-");
    let is_cow = matches!(scenario.as_str(), "cow-low" | "cow-high");
    let fixture = (is_get_many
        || is_tombstone_scan
        || is_apply
        || is_cow
        || matches!(scenario.as_str(), "scan" | "scan-stream"))
    .then(|| {
        if is_tombstone_scan {
            runtime.block_on(support::tombstone_fixture())
        } else {
            runtime.block_on(support::fixture())
        }
    });
    let named_queries = scenario
        .strip_prefix("get-many-")
        .map(|name| support::get_many_queries(fixture.as_ref().unwrap(), name));
    let hash_bytes = (scenario == "hash").then(support::hash_bytes);
    let encoded_node = (scenario == "decode").then(support::encoded_node);
    if let Some(fixture) = &fixture
        && is_get_many
    {
        runtime.block_on(support::warm(fixture));
    }

    reset();
    ENABLED.store(true, Relaxed);
    let checksum = match scenario.as_str() {
        "get-many" => runtime.block_on(support::get_many(fixture.as_ref().unwrap(), iterations)),
        shape if shape.starts_with("get-many-") => runtime.block_on(support::get_many_prepared(
            fixture.as_ref().unwrap(),
            named_queries.as_ref().unwrap(),
            iterations,
        )),
        "scan" => runtime.block_on(support::scan(fixture.as_ref().unwrap(), iterations)),
        "scan-stream" => {
            runtime.block_on(support::scan_stream(fixture.as_ref().unwrap(), iterations))
        }
        "scan-tombstone" => runtime.block_on(support::scan_tombstones(
            fixture.as_ref().unwrap(),
            iterations,
        )),
        "scan-stream-tombstone" => runtime.block_on(support::scan_stream_tombstones(
            fixture.as_ref().unwrap(),
            iterations,
        )),
        "apply" => runtime.block_on(support::apply(fixture.as_ref().unwrap(), iterations)),
        shape if shape.starts_with("apply-") => runtime.block_on(support::apply_named(
            fixture.as_ref().unwrap(),
            iterations,
            &shape[6..],
        )),
        "cow-low" | "cow-high" => runtime.block_on(support::cow_fixture(
            fixture.as_ref().unwrap(),
            scenario == "cow-high",
            iterations,
        )),
        "hash" => support::hash(hash_bytes.as_deref().unwrap(), iterations),
        "decode" => {
            let (format, bytes) = encoded_node.as_ref().unwrap();
            support::decode(format, bytes, iterations)
        }
        _ => usage(),
    };
    ENABLED.store(false, Relaxed);
    let baseline = BASELINE_LIVE_BYTES.load(Relaxed);
    let peak = PEAK_LIVE_BYTES.load(Relaxed).saturating_sub(baseline);
    let live = CURRENT_LIVE_BYTES.load(Relaxed) as i128 - baseline as i128;

    println!(
        "scenario={scenario} iterations={iterations} checksum={checksum} allocations={} reallocations={} allocated_bytes={} peak_live_bytes={} live_bytes={}",
        ALLOCATIONS.load(Relaxed),
        REALLOCATIONS.load(Relaxed),
        ALLOCATED_BYTES.load(Relaxed),
        peak,
        live,
    );
}
