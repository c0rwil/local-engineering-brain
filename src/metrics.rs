//! Lock-light in-process observability metrics (spec §25.1 subset).
//!
//! The registry is owned by the daemon (`DaemonState`) and updated from
//! request handlers with plain relaxed atomics: no locks are taken on any
//! request path, and recording a sample is a single `fetch_add` per bucket
//! plus one for the running sum.
//!
//! # Persistence
//!
//! Metrics are process-lifetime only and reset when the daemon restarts.
//! This is spec-compliant for V1: canonical, durable per-request history
//! (compile latency, candidate counts, selected items/tokens, stable-prefix
//! hashes) already lives in the `context_runs`/`context_items` tables and the
//! immutable event log, and can be reconstructed from there.
//!
//! # Stale-injection tracking
//!
//! `stale_injections` counts memories that were compiled into context at
//! least once (`injected_count > 0`) and were later invalidated by symbol
//! drift ([`crate::memory::MemoryStore::mark_symbol_drift_suspect`]). That is
//! the only invalidation signal the daemon observes cheaply today, so the
//! counter is a lower bound: it does not attribute an invalidation to the
//! specific runs that injected the memory, and invalidations through explicit
//! contradiction review are not yet wired. The counter is real data, never
//! synthesized.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Upper bucket bounds (inclusive, milliseconds) for every latency histogram.
/// One additional unbounded overflow bucket follows the last bound.
pub const LATENCY_BUCKET_UPPER_MS: [u64; 15] = [
    1, 2, 5, 10, 25, 50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000, 30_000, 60_000,
];

const BUCKET_COUNT: usize = LATENCY_BUCKET_UPPER_MS.len() + 1;

/// A fixed-bucket exponential latency histogram updated with relaxed atomics.
#[derive(Debug)]
pub struct Histogram {
    buckets: [AtomicU64; BUCKET_COUNT],
    sum_ms: AtomicU64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            sum_ms: AtomicU64::new(0),
        }
    }
}

impl Histogram {
    /// Record one sample in milliseconds.
    pub fn record_ms(&self, ms: u64) {
        let index = LATENCY_BUCKET_UPPER_MS
            .iter()
            .position(|bound| ms <= *bound)
            .unwrap_or(BUCKET_COUNT - 1);
        self.buckets[index].fetch_add(1, Ordering::Relaxed);
        self.sum_ms.fetch_add(ms, Ordering::Relaxed);
    }

    /// Record one elapsed duration, saturating to whole milliseconds.
    pub fn observe(&self, elapsed: Duration) {
        self.record_ms(u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX));
    }

    /// A JSON-friendly consistent view with percentiles derived from buckets.
    #[must_use]
    pub fn snapshot(&self) -> HistogramSnapshot {
        let counts: Vec<u64> = self
            .buckets
            .iter()
            .map(|bucket| bucket.load(Ordering::Relaxed))
            .collect();
        let count: u64 = counts.iter().sum();
        let buckets = counts
            .iter()
            .enumerate()
            .map(|(index, bucket_count)| HistogramBucket {
                le_ms: LATENCY_BUCKET_UPPER_MS.get(index).copied(),
                count: *bucket_count,
            })
            .collect();
        HistogramSnapshot {
            count,
            sum_ms: self.sum_ms.load(Ordering::Relaxed),
            p50_ms: percentile(&counts, count, 0.50),
            p90_ms: percentile(&counts, count, 0.90),
            p95_ms: percentile(&counts, count, 0.95),
            p99_ms: percentile(&counts, count, 0.99),
            buckets,
        }
    }
}

/// Estimate a percentile by linear interpolation inside the matched bucket.
/// The overflow bucket saturates to the largest finite bound.
#[allow(clippy::cast_precision_loss)]
fn percentile(counts: &[u64], total: u64, quantile: f64) -> f64 {
    if total == 0 {
        return 0.0;
    }
    let last_bound = LATENCY_BUCKET_UPPER_MS[LATENCY_BUCKET_UPPER_MS.len() - 1];
    let rank = quantile * total as f64;
    let mut cumulative = 0_u64;
    for (index, bucket_count) in counts.iter().enumerate() {
        let next = cumulative + bucket_count;
        if *bucket_count > 0 && next as f64 >= rank {
            let lower = if index == 0 {
                0
            } else {
                LATENCY_BUCKET_UPPER_MS[index - 1]
            };
            let upper = LATENCY_BUCKET_UPPER_MS
                .get(index)
                .copied()
                .unwrap_or(last_bound);
            let fraction = (rank - cumulative as f64) / *bucket_count as f64;
            return lower as f64 + fraction.clamp(0.0, 1.0) * (upper.saturating_sub(lower)) as f64;
        }
        cumulative = next;
    }
    last_bound as f64
}

/// Serialized view of one histogram.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct HistogramSnapshot {
    pub count: u64,
    pub sum_ms: u64,
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub buckets: Vec<HistogramBucket>,
}

/// One cumulative-style bucket; `le_ms: None` is the unbounded overflow bucket.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct HistogramBucket {
    pub le_ms: Option<u64>,
    pub count: u64,
}

/// Coarse endpoint grouping for per-request daemon latency histograms.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EndpointGroup {
    Observability,
    Admin,
    Projects,
    World,
    Context,
    Memory,
    Experience,
    Learning,
    Semantic,
    Other,
}

impl EndpointGroup {
    const ALL: [Self; 10] = [
        Self::Observability,
        Self::Admin,
        Self::Projects,
        Self::World,
        Self::Context,
        Self::Memory,
        Self::Experience,
        Self::Learning,
        Self::Semantic,
        Self::Other,
    ];

    const fn index(self) -> usize {
        match self {
            Self::Observability => 0,
            Self::Admin => 1,
            Self::Projects => 2,
            Self::World => 3,
            Self::Context => 4,
            Self::Memory => 5,
            Self::Experience => 6,
            Self::Learning => 7,
            Self::Semantic => 8,
            Self::Other => 9,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Observability => "observability",
            Self::Admin => "admin",
            Self::Projects => "projects",
            Self::World => "world",
            Self::Context => "context",
            Self::Memory => "memory",
            Self::Experience => "experience",
            Self::Learning => "learning",
            Self::Semantic => "semantic",
            Self::Other => "other",
        }
    }

    /// Map a request path onto its endpoint group.
    #[must_use]
    pub fn classify(path: &str) -> Self {
        let Some(suffix) = path.strip_prefix("/api/v1/") else {
            return Self::Other;
        };
        if suffix == "health" || suffix.starts_with("metrics") {
            Self::Observability
        } else if suffix.starts_with("admin") {
            Self::Admin
        } else if suffix.starts_with("projects") {
            Self::Projects
        } else if suffix.starts_with("world") {
            Self::World
        } else if suffix.starts_with("context") {
            Self::Context
        } else if suffix.starts_with("memories") {
            Self::Memory
        } else if suffix.starts_with("tasks")
            || suffix.starts_with("evidence")
            || suffix.starts_with("experience")
        {
            Self::Experience
        } else if suffix.starts_with("evaluation") || suffix.starts_with("learning") {
            Self::Learning
        } else if suffix.starts_with("semantic") {
            Self::Semantic
        } else {
            Self::Other
        }
    }
}

/// In-daemon metrics registry. All members are updated with relaxed atomics.
#[derive(Debug, Default)]
pub struct MetricsRegistry {
    context_compile_ms: Histogram,
    retrieval_ms: Histogram,
    index_scan_ms: Histogram,
    request_ms: [Histogram; 10],
    context_compiles_brain: AtomicU64,
    context_compiles_off: AtomicU64,
    injected_items: AtomicU64,
    injected_tokens: AtomicU64,
    retrieval_candidates_considered: AtomicU64,
    retrieval_candidates_selected: AtomicU64,
    memory_promotions: AtomicU64,
    memory_rejections: AtomicU64,
    memory_purges: AtomicU64,
    memories_invalidated: AtomicU64,
    stale_injections: AtomicU64,
    evidence_records: AtomicU64,
}

impl MetricsRegistry {
    /// Record one served daemon request for its endpoint group.
    pub fn observe_request(&self, group: EndpointGroup, elapsed: Duration) {
        self.request_ms[group.index()].observe(elapsed);
    }

    /// Record the read-only retrieval/prepare stage of one brain compile.
    pub fn observe_retrieval(&self, elapsed: Duration) {
        self.retrieval_ms.observe(elapsed);
    }

    /// Record one structural index scan (prepare stage).
    pub fn observe_index_scan(&self, elapsed: Duration) {
        self.index_scan_ms.observe(elapsed);
    }

    /// Record one completed context compile with its per-run metrics.
    pub fn observe_context_compile(
        &self,
        brain: bool,
        compile_ms: u64,
        injected_items: u64,
        injected_tokens: u64,
        candidates_considered: u64,
    ) {
        self.context_compile_ms.record_ms(compile_ms);
        if brain {
            self.context_compiles_brain.fetch_add(1, Ordering::Relaxed);
        } else {
            self.context_compiles_off.fetch_add(1, Ordering::Relaxed);
        }
        self.injected_items
            .fetch_add(injected_items, Ordering::Relaxed);
        self.injected_tokens
            .fetch_add(injected_tokens, Ordering::Relaxed);
        self.retrieval_candidates_considered
            .fetch_add(candidates_considered, Ordering::Relaxed);
        self.retrieval_candidates_selected
            .fetch_add(injected_items, Ordering::Relaxed);
    }

    /// Count one accepted epistemic promotion.
    pub fn count_memory_promotion(&self) {
        self.memory_promotions.fetch_add(1, Ordering::Relaxed);
    }

    /// Count one explicit rejection transition.
    pub fn count_memory_rejection(&self) {
        self.memory_rejections.fetch_add(1, Ordering::Relaxed);
    }

    /// Count one irreversible purge.
    pub fn count_memory_purge(&self) {
        self.memory_purges.fetch_add(1, Ordering::Relaxed);
    }

    /// Count one recorded evidence artifact.
    pub fn count_evidence_record(&self) {
        self.evidence_records.fetch_add(1, Ordering::Relaxed);
    }

    /// Count symbol-drift invalidations; `previously_injected` feeds the
    /// stale-injection counter (see module docs for its exact meaning).
    pub fn count_drift_invalidation(&self, invalidated: u64, previously_injected: u64) {
        self.memories_invalidated
            .fetch_add(invalidated, Ordering::Relaxed);
        self.stale_injections
            .fetch_add(previously_injected, Ordering::Relaxed);
    }

    /// A stable JSON-friendly snapshot of every counter and histogram.
    #[must_use]
    pub fn snapshot(&self) -> MetricsSnapshot {
        let requests_ms = EndpointGroup::ALL
            .iter()
            .map(|group| {
                (
                    group.as_str().to_owned(),
                    self.request_ms[group.index()].snapshot(),
                )
            })
            .collect();
        MetricsSnapshot {
            counters: CounterSnapshot {
                context_compiles_brain: self.context_compiles_brain.load(Ordering::Relaxed),
                context_compiles_off: self.context_compiles_off.load(Ordering::Relaxed),
                injected_items: self.injected_items.load(Ordering::Relaxed),
                injected_tokens: self.injected_tokens.load(Ordering::Relaxed),
                retrieval_candidates_considered: self
                    .retrieval_candidates_considered
                    .load(Ordering::Relaxed),
                retrieval_candidates_selected: self
                    .retrieval_candidates_selected
                    .load(Ordering::Relaxed),
                memory_promotions: self.memory_promotions.load(Ordering::Relaxed),
                memory_rejections: self.memory_rejections.load(Ordering::Relaxed),
                memory_purges: self.memory_purges.load(Ordering::Relaxed),
                memories_invalidated: self.memories_invalidated.load(Ordering::Relaxed),
                stale_injections: self.stale_injections.load(Ordering::Relaxed),
                evidence_records: self.evidence_records.load(Ordering::Relaxed),
            },
            context_compile_ms: self.context_compile_ms.snapshot(),
            retrieval_ms: self.retrieval_ms.snapshot(),
            index_scan_ms: self.index_scan_ms.snapshot(),
            requests_ms,
        }
    }
}

/// Monotonic process-lifetime counters.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CounterSnapshot {
    pub context_compiles_brain: u64,
    pub context_compiles_off: u64,
    pub injected_items: u64,
    pub injected_tokens: u64,
    pub retrieval_candidates_considered: u64,
    pub retrieval_candidates_selected: u64,
    pub memory_promotions: u64,
    pub memory_rejections: u64,
    pub memory_purges: u64,
    pub memories_invalidated: u64,
    /// Lower-bound count of previously injected memories later invalidated.
    /// See module docs; zero until drift invalidation observes injected items.
    pub stale_injections: u64,
    pub evidence_records: u64,
}

/// Full registry snapshot returned by `GET /api/v1/metrics`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MetricsSnapshot {
    pub counters: CounterSnapshot,
    pub context_compile_ms: HistogramSnapshot,
    pub retrieval_ms: HistogramSnapshot,
    pub index_scan_ms: HistogramSnapshot,
    /// Per-endpoint-group daemon request latency, keyed by group name. All
    /// groups are always present so the shape is stable for consumers.
    pub requests_ms: BTreeMap<String, HistogramSnapshot>,
}

/// Request-outcome counters for the separate `leb-proxy` process. The proxy
/// does not share the daemon's address space, so it owns this tiny registry
/// and serves it from `GET /leb/metrics` on its loopback listener.
#[derive(Debug, Default)]
pub struct ProxyMetrics {
    forwarded: AtomicU64,
    injected: AtomicU64,
    degraded: AtomicU64,
    refused: AtomicU64,
}

impl ProxyMetrics {
    /// Count a request forwarded without context injection.
    pub fn count_forwarded(&self) {
        self.forwarded.fetch_add(1, Ordering::Relaxed);
    }

    /// Count a request forwarded with compiled context injected.
    pub fn count_injected(&self) {
        self.injected.fetch_add(1, Ordering::Relaxed);
    }

    /// Count a fail-open request forwarded without available context.
    pub fn count_degraded(&self) {
        self.degraded.fetch_add(1, Ordering::Relaxed);
    }

    /// Count a request refused with an error response.
    pub fn count_refused(&self) {
        self.refused.fetch_add(1, Ordering::Relaxed);
    }

    /// A stable JSON-friendly snapshot of the proxy outcome counters.
    #[must_use]
    pub fn snapshot(&self) -> ProxyMetricsSnapshot {
        ProxyMetricsSnapshot {
            forwarded: self.forwarded.load(Ordering::Relaxed),
            injected: self.injected.load(Ordering::Relaxed),
            degraded: self.degraded.load(Ordering::Relaxed),
            refused: self.refused.load(Ordering::Relaxed),
        }
    }
}

/// Serialized proxy outcome counters. Outcomes are mutually exclusive per
/// request: `forwarded` excludes injected and degraded requests.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ProxyMetricsSnapshot {
    pub forwarded: u64,
    pub injected: u64,
    pub degraded: u64,
    pub refused: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_boundaries_are_inclusive_upper_bounds() {
        let histogram = Histogram::default();
        histogram.record_ms(0);
        histogram.record_ms(1);
        histogram.record_ms(2);
        histogram.record_ms(3);
        histogram.record_ms(60_000);
        histogram.record_ms(60_001);
        let snapshot = histogram.snapshot();
        assert_eq!(snapshot.count, 6);
        let count_for = |le_ms: Option<u64>| {
            snapshot
                .buckets
                .iter()
                .find(|bucket| bucket.le_ms == le_ms)
                .expect("bucket exists")
                .count
        };
        assert_eq!(count_for(Some(1)), 2, "0 and 1 share the first bucket");
        assert_eq!(count_for(Some(2)), 1);
        assert_eq!(count_for(Some(5)), 1, "3 rounds up into the 5 ms bucket");
        assert_eq!(count_for(Some(60_000)), 1);
        assert_eq!(count_for(None), 1, "60_001 lands in the overflow bucket");
    }

    #[test]
    fn percentiles_interpolate_within_known_distributions() {
        let empty = Histogram::default().snapshot();
        assert_eq!(empty.count, 0);
        assert!((empty.p50_ms - 0.0).abs() < f64::EPSILON);

        // 100 samples of 10 ms all land in the (5, 10] bucket, so quantile q
        // interpolates to 5 + q * 5.
        let histogram = Histogram::default();
        for _ in 0..100 {
            histogram.record_ms(10);
        }
        let snapshot = histogram.snapshot();
        assert!((snapshot.p50_ms - 7.5).abs() < 1e-9, "{}", snapshot.p50_ms);
        assert!((snapshot.p90_ms - 9.5).abs() < 1e-9, "{}", snapshot.p90_ms);
        assert!((snapshot.p99_ms - 9.95).abs() < 1e-9, "{}", snapshot.p99_ms);
        assert_eq!(snapshot.sum_ms, 1_000);

        // A bimodal distribution: 90 fast samples and 10 slow ones. p50 falls
        // in the fast bucket, p99 in the slow bucket.
        let bimodal = Histogram::default();
        for _ in 0..90 {
            bimodal.record_ms(1);
        }
        for _ in 0..10 {
            bimodal.record_ms(400);
        }
        let snapshot = bimodal.snapshot();
        assert!(snapshot.p50_ms <= 1.0, "{}", snapshot.p50_ms);
        assert!(
            snapshot.p99_ms > 250.0 && snapshot.p99_ms <= 500.0,
            "{}",
            snapshot.p99_ms
        );

        // Overflow samples saturate to the largest finite bound.
        let overflow = Histogram::default();
        overflow.record_ms(1_000_000);
        assert!((overflow.snapshot().p50_ms - 60_000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn concurrent_recording_loses_no_samples() {
        let registry = MetricsRegistry::default();
        std::thread::scope(|scope| {
            for worker in 0..8_u64 {
                let registry = &registry;
                scope.spawn(move || {
                    for iteration in 0..1_000_u64 {
                        registry.observe_request(
                            EndpointGroup::Context,
                            Duration::from_millis(worker * iteration % 50),
                        );
                        registry.observe_context_compile(true, iteration % 20, 2, 40, 5);
                    }
                });
            }
        });
        let snapshot = registry.snapshot();
        assert_eq!(snapshot.requests_ms["context"].count, 8_000);
        assert_eq!(snapshot.context_compile_ms.count, 8_000);
        assert_eq!(snapshot.counters.context_compiles_brain, 8_000);
        assert_eq!(snapshot.counters.injected_items, 16_000);
        assert_eq!(snapshot.counters.injected_tokens, 320_000);
        assert_eq!(snapshot.counters.retrieval_candidates_considered, 40_000);
    }

    #[test]
    fn snapshot_shape_is_stable_and_serializable() {
        let registry = MetricsRegistry::default();
        let snapshot = registry.snapshot();
        assert_eq!(snapshot.requests_ms.len(), EndpointGroup::ALL.len());
        let encoded = serde_json::to_string(&snapshot).expect("serialize");
        let decoded: MetricsSnapshot = serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(decoded, snapshot);
    }

    #[test]
    fn endpoint_classification_covers_the_api_surface() {
        let cases = [
            ("/api/v1/health", EndpointGroup::Observability),
            ("/api/v1/metrics", EndpointGroup::Observability),
            ("/api/v1/metrics/memories", EndpointGroup::Observability),
            ("/api/v1/admin/doctor", EndpointGroup::Admin),
            ("/api/v1/projects/index", EndpointGroup::Projects),
            ("/api/v1/world/search", EndpointGroup::World),
            ("/api/v1/context/compile", EndpointGroup::Context),
            ("/api/v1/memories/some-id/promote", EndpointGroup::Memory),
            ("/api/v1/tasks/id/finish", EndpointGroup::Experience),
            ("/api/v1/evidence", EndpointGroup::Experience),
            ("/api/v1/experience/failures", EndpointGroup::Experience),
            ("/api/v1/evaluation/runs", EndpointGroup::Learning),
            ("/api/v1/learning/distill", EndpointGroup::Learning),
            ("/api/v1/semantic/rebuild", EndpointGroup::Semantic),
            ("/api/v1/unknown", EndpointGroup::Other),
            ("/outside", EndpointGroup::Other),
        ];
        for (path, expected) in cases {
            assert_eq!(EndpointGroup::classify(path), expected, "{path}");
        }
    }
}
