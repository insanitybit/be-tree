//! Optional low-overhead instrumentation for tuning a tree against its workload.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use crate::search::ProbeCost;

/// A power-of-two bucketed histogram that retains no samples.
#[derive(Debug)]
struct Histogram {
    buckets: [AtomicU64; 40],
    total: AtomicU64,
    sum: AtomicU64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            total: AtomicU64::new(0),
            sum: AtomicU64::new(0),
        }
    }
}

impl Histogram {
    fn record(&self, value: u64) {
        let bucket = (64 - value.leading_zeros()) as usize;
        self.buckets[bucket.min(39)].fetch_add(1, Relaxed);
        self.total.fetch_add(1, Relaxed);
        self.sum.fetch_add(value, Relaxed);
    }

    fn count(&self) -> u64 {
        self.total.load(Relaxed)
    }

    fn mean(&self) -> f64 {
        match self.count() {
            0 => 0.0,
            count => self.sum.load(Relaxed) as f64 / count as f64,
        }
    }

    /// Upper bound of the bucket containing quantile `q`.
    fn quantile(&self, q: f64) -> u64 {
        let count = self.count();
        if count == 0 {
            return 0;
        }
        let target = (count as f64 * q).ceil() as u64;
        let mut seen = 0;
        for (index, bucket) in self.buckets.iter().enumerate() {
            seen += bucket.load(Relaxed);
            if seen >= target {
                return if index == 0 { 0 } else { 1 << (index - 1) };
            }
        }
        u64::MAX
    }

    fn snapshot(&self) -> HistogramSnapshot {
        HistogramSnapshot {
            count: self.count(),
            mean: self.mean(),
            p50: self.quantile(0.5),
            p99: self.quantile(0.99),
        }
    }
}

/// Lock-free workload counters shared by cloned and class-scoped tree handles.
///
/// Recording is off by default. Every update then returns after one immutable boolean load.
#[derive(Debug, Default)]
pub(crate) struct Metrics {
    enabled: bool,
    objects_read: AtomicU64,
    bytes_read: AtomicU64,
    bytes_verified: AtomicU64,
    objects_written: AtomicU64,
    bytes_hashed: AtomicU64,
    duplicates_elided: AtomicU64,
    cache_warmed: AtomicU64,
    waves: AtomicU64,
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
    flushes: AtomicU64,
    flush_victim_bytes: AtomicU64,
    flush_pending_bytes: AtomicU64,
    undersized_flushes: AtomicU64,
    buffer_occupancy: Histogram,
    victim_bytes: Histogram,
    direct_routed: AtomicU64,
    leaves_written: AtomicU64,
    internals_written: AtomicU64,
    internal_partitions: AtomicU64,
    root_growths: AtomicU64,
    fanout_written: Histogram,
    inline_values: AtomicU64,
    inline_value_bytes: AtomicU64,
    external_values: AtomicU64,
    external_value_bytes: AtomicU64,
    value_objects_encoded: AtomicU64,
    value_bytes_hashed: AtomicU64,
    probes: AtomicU64,
    equal_head_range: Histogram,
    full_comparisons: Histogram,
    absent_key_buffer_probes: AtomicU64,
    diff_equal_id_skips: AtomicU64,
    diff_visited_keys: AtomicU64,
    diff_visited_nodes: AtomicU64,
}

/// One immutable histogram reading.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct HistogramSnapshot {
    pub count: u64,
    pub mean: f64,
    pub p50: u64,
    pub p99: u64,
}

/// One coherent-enough, immutable reading of the optional workload counters.
///
/// Counters are read independently; concurrent operations may finish between fields.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MetricsSnapshot {
    pub objects_read: u64,
    pub bytes_read: u64,
    pub bytes_verified: u64,
    pub objects_written: u64,
    pub bytes_hashed: u64,
    pub duplicates_elided: u64,
    pub cache_warmed: u64,
    pub waves: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub flushes: u64,
    pub flush_victim_bytes: u64,
    pub flush_pending_bytes: u64,
    pub undersized_flushes: u64,
    pub buffer_occupancy: HistogramSnapshot,
    pub victim_bytes: HistogramSnapshot,
    pub direct_routed: u64,
    pub leaves_written: u64,
    pub internals_written: u64,
    pub internal_partitions: u64,
    pub root_growths: u64,
    pub fanout_written: HistogramSnapshot,
    pub inline_values: u64,
    pub inline_value_bytes: u64,
    pub external_values: u64,
    pub external_value_bytes: u64,
    pub value_objects_encoded: u64,
    pub value_bytes_hashed: u64,
    pub probes: u64,
    pub equal_head_range: HistogramSnapshot,
    pub full_comparisons: HistogramSnapshot,
    pub absent_key_buffer_probes: u64,
    pub diff_equal_id_skips: u64,
    pub diff_visited_keys: u64,
    pub diff_visited_nodes: u64,
}

impl Metrics {
    pub(crate) fn recording() -> Self {
        Self {
            enabled: true,
            ..Self::default()
        }
    }

    /// Read the counters without exposing their synchronization machinery.
    pub(crate) fn snapshot(&self) -> MetricsSnapshot {
        let load = |counter: &AtomicU64| counter.load(Relaxed);
        MetricsSnapshot {
            objects_read: load(&self.objects_read),
            bytes_read: load(&self.bytes_read),
            bytes_verified: load(&self.bytes_verified),
            objects_written: load(&self.objects_written),
            bytes_hashed: load(&self.bytes_hashed),
            duplicates_elided: load(&self.duplicates_elided),
            cache_warmed: load(&self.cache_warmed),
            waves: load(&self.waves),
            cache_hits: load(&self.cache_hits),
            cache_misses: load(&self.cache_misses),
            flushes: load(&self.flushes),
            flush_victim_bytes: load(&self.flush_victim_bytes),
            flush_pending_bytes: load(&self.flush_pending_bytes),
            undersized_flushes: load(&self.undersized_flushes),
            buffer_occupancy: self.buffer_occupancy.snapshot(),
            victim_bytes: self.victim_bytes.snapshot(),
            direct_routed: load(&self.direct_routed),
            leaves_written: load(&self.leaves_written),
            internals_written: load(&self.internals_written),
            internal_partitions: load(&self.internal_partitions),
            root_growths: load(&self.root_growths),
            fanout_written: self.fanout_written.snapshot(),
            inline_values: load(&self.inline_values),
            inline_value_bytes: load(&self.inline_value_bytes),
            external_values: load(&self.external_values),
            external_value_bytes: load(&self.external_value_bytes),
            value_objects_encoded: load(&self.value_objects_encoded),
            value_bytes_hashed: load(&self.value_bytes_hashed),
            probes: load(&self.probes),
            equal_head_range: self.equal_head_range.snapshot(),
            full_comparisons: self.full_comparisons.snapshot(),
            absent_key_buffer_probes: load(&self.absent_key_buffer_probes),
            diff_equal_id_skips: load(&self.diff_equal_id_skips),
            diff_visited_keys: load(&self.diff_visited_keys),
            diff_visited_nodes: load(&self.diff_visited_nodes),
        }
    }

    #[inline]
    fn add(&self, counter: &AtomicU64, value: u64) {
        if self.enabled {
            counter.fetch_add(value, Relaxed);
        }
    }

    pub(crate) fn cache_hit(&self) {
        self.add(&self.cache_hits, 1);
    }
    pub(crate) fn cache_miss(&self) {
        self.add(&self.cache_misses, 1);
    }
    pub(crate) fn object_fetched(&self) {
        self.add(&self.objects_read, 1);
    }
    pub(crate) fn wave(&self, count: u64) {
        self.add(&self.waves, count);
    }
    pub(crate) fn bytes_read(&self, bytes: u64) {
        self.add(&self.bytes_read, bytes);
    }
    pub(crate) fn bytes_verified(&self, bytes: u64) {
        self.add(&self.bytes_verified, bytes);
    }
    pub(crate) fn written(&self, objects: u64, bytes: u64, duplicates: u64) {
        self.add(&self.objects_written, objects);
        self.add(&self.bytes_hashed, bytes);
        self.add(&self.duplicates_elided, duplicates);
    }
    pub(crate) fn cache_warmed(&self) {
        self.add(&self.cache_warmed, 1);
    }
    pub(crate) fn flush(&self, pending: u64, victim: u64, _children: u64) {
        self.add(&self.flushes, 1);
        self.add(&self.flush_pending_bytes, pending);
        self.add(&self.flush_victim_bytes, victim);
        if self.enabled {
            self.buffer_occupancy.record(pending);
            self.victim_bytes.record(victim);
        }
    }
    pub(crate) fn undersized_flush(&self) {
        self.add(&self.undersized_flushes, 1);
    }
    pub(crate) fn direct_routed(&self, count: u64) {
        self.add(&self.direct_routed, count);
    }
    pub(crate) fn leaves_written(&self, count: u64) {
        self.add(&self.leaves_written, count);
    }
    pub(crate) fn internal_written(&self, fanout: u64, _buffer: u64) {
        self.add(&self.internals_written, 1);
        if self.enabled {
            self.fanout_written.record(fanout);
        }
    }
    pub(crate) fn internal_partition(&self, groups: u64) {
        self.add(&self.internal_partitions, groups);
    }
    pub(crate) fn root_grown(&self) {
        self.add(&self.root_growths, 1);
    }
    pub(crate) fn inline_value(&self, bytes: u64) {
        self.add(&self.inline_values, 1);
        self.add(&self.inline_value_bytes, bytes);
    }
    pub(crate) fn external_value(&self, bytes: u64) {
        self.add(&self.external_values, 1);
        self.add(&self.external_value_bytes, bytes);
    }
    pub(crate) fn value_object_encoded(&self, bytes: u64) {
        self.add(&self.value_objects_encoded, 1);
        self.add(&self.value_bytes_hashed, bytes);
    }
    pub(crate) fn probe(&self, cost: ProbeCost) {
        self.add(&self.probes, 1);
        if self.enabled {
            self.equal_head_range.record(cost.equal_head_range.into());
            self.full_comparisons.record(cost.full_comparisons.into());
        }
    }
    pub(crate) fn absent_key_buffer_probe(&self) {
        self.add(&self.absent_key_buffer_probes, 1);
    }
    pub(crate) fn diff_equal_id_skip(&self) {
        self.add(&self.diff_equal_id_skips, 1);
    }
    pub(crate) fn diff_visited_keys(&self, count: u64) {
        self.add(&self.diff_visited_keys, count);
    }
    pub(crate) fn diff_visited_nodes(&self, count: u64) {
        self.add(&self.diff_visited_nodes, count);
    }
}

impl std::fmt::Display for MetricsSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "waves={} objects_read={} bytes_read={} objects_written={} bytes_hashed={} cache={}/{} \
             flushes={} victim_bytes(mean)={:.0} undersized={} direct_routed={} leaves={} internals={} \
             partitions={} root_growths={} values(inline/ext/encoded)={}/{}/{} value_bytes_hashed={} \
             probes={} eq_head_p50={} eq_head_p99={} cmps_mean={:.2} absent_buffer_probes={} \
             diff(skips/keys/nodes)={}/{}/{}",
            self.waves,
            self.objects_read,
            self.bytes_read,
            self.objects_written,
            self.bytes_hashed,
            self.cache_hits,
            self.cache_misses,
            self.flushes,
            self.victim_bytes.mean,
            self.undersized_flushes,
            self.direct_routed,
            self.leaves_written,
            self.internals_written,
            self.internal_partitions,
            self.root_growths,
            self.inline_values,
            self.external_values,
            self.value_objects_encoded,
            self.value_bytes_hashed,
            self.probes,
            self.equal_head_range.p50,
            self.equal_head_range.p99,
            self.full_comparisons.mean,
            self.absent_key_buffer_probes,
            self.diff_equal_id_skips,
            self.diff_visited_keys,
            self.diff_visited_nodes,
        )
    }
}
