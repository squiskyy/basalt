//! Prometheus metrics for Basalt.
//!
//! All metrics are behind the `metrics` feature flag. When disabled,
//! the module provides no-op stubs so the rest of the code compiles
//! without the prometheus dependency.

use std::sync::Arc;
use std::time::Instant;

/// Trait for metrics recording. Implemented by real Prometheus metrics
/// when the `metrics` feature is enabled, and by no-op stubs otherwise.
pub trait Metrics: Send + Sync + std::fmt::Debug {
    /// Record a read operation for the given namespace.
    fn record_read(&self, namespace: &str);
    /// Record a write operation for the given namespace.
    fn record_write(&self, namespace: &str);
    /// Update the entry count gauge for a namespace.
    fn set_namespace_entry_count(&self, namespace: &str, count: usize);
    /// Observe request duration for an operation and namespace.
    fn observe_request_duration(&self, operation: &str, namespace: &str, duration: std::time::Duration);
    /// Record shard entry count.
    fn set_shard_entries(&self, shard_id: usize, count: usize);
    /// Record snapshot duration.
    fn observe_snapshot_duration(&self, duration: std::time::Duration);
    /// Record snapshot success timestamp.
    fn set_snapshot_last_success(&self);
    /// Set replication lag for a replica.
    fn set_replication_lag(&self, replica: &str, lag_seconds: f64);
    /// Render all metrics in Prometheus text exposition format.
    fn render(&self) -> String;
}

// ---------------------------------------------------------------------------
// Feature-gated implementation
// ---------------------------------------------------------------------------

#[cfg(feature = "metrics")]
mod prometheus_impl {
    use super::Metrics;
    use prometheus::{
        self, HistogramOpts, HistogramVec, IntGaugeVec, Opts, Registry, TextEncoder,
    };
    use std::time::Duration;

    #[derive(Debug)]
    pub struct PrometheusMetrics {
        registry: Registry,
        namespace_reads: prometheus::IntCounterVec,
        namespace_writes: prometheus::IntCounterVec,
        namespace_entry_count: IntGaugeVec,
        request_duration: HistogramVec,
        shard_entries: IntGaugeVec,
        snapshot_duration: prometheus::Histogram,
        snapshot_last_success: IntGaugeVec,
        replication_lag: prometheus::GaugeVec,
    }

    impl Default for PrometheusMetrics {
        fn default() -> Self {
            Self::new()
        }
    }

    impl PrometheusMetrics {
        pub fn new() -> Self {
            let registry = Registry::new();

            let namespace_reads = prometheus::IntCounterVec::new(
                Opts::new("basalt_namespace_reads_total", "Cumulative read operations per namespace"),
                &["namespace"],
            ).expect("creating namespace_reads counter");
            registry.register(Box::new(namespace_reads.clone()))
                .expect("registering namespace_reads");

            let namespace_writes = prometheus::IntCounterVec::new(
                Opts::new("basalt_namespace_writes_total", "Cumulative write operations per namespace"),
                &["namespace"],
            ).expect("creating namespace_writes counter");
            registry.register(Box::new(namespace_writes.clone()))
                .expect("registering namespace_writes");

            let namespace_entry_count = IntGaugeVec::new(
                Opts::new("basalt_namespace_entry_count", "Current number of entries per namespace"),
                &["namespace"],
            ).expect("creating namespace_entry_count gauge");
            registry.register(Box::new(namespace_entry_count.clone()))
                .expect("registering namespace_entry_count");

            let request_duration = HistogramVec::new(
                HistogramOpts::new("basalt_request_duration_seconds", "Request latency by operation and namespace"),
                &["operation", "namespace"],
            ).expect("creating request_duration histogram");
            registry.register(Box::new(request_duration.clone()))
                .expect("registering request_duration");

            let shard_entries = IntGaugeVec::new(
                Opts::new("basalt_shard_entries", "Number of entries per shard"),
                &["shard"],
            ).expect("creating shard_entries gauge");
            registry.register(Box::new(shard_entries.clone()))
                .expect("registering shard_entries");

            let snapshot_duration = prometheus::Histogram::with_opts(
                HistogramOpts::new("basalt_snapshot_duration_seconds", "Time taken to complete snapshots"),
            ).expect("creating snapshot_duration histogram");
            registry.register(Box::new(snapshot_duration.clone()))
                .expect("registering snapshot_duration");

            let snapshot_last_success = IntGaugeVec::new(
                Opts::new("basalt_snapshot_last_success_timestamp", "Unix timestamp of most recent successful snapshot"),
                &[],  // no labels - single value
            ).expect("creating snapshot_last_success gauge");
            registry.register(Box::new(snapshot_last_success.clone()))
                .expect("registering snapshot_last_success");

            let replication_lag = prometheus::GaugeVec::new(
                Opts::new("basalt_replication_lag_seconds", "Replication lag between primary and replica"),
                &["replica"],
            ).expect("creating replication_lag gauge");
            registry.register(Box::new(replication_lag.clone()))
                .expect("registering replication_lag");

            PrometheusMetrics {
                registry,
                namespace_reads,
                namespace_writes,
                namespace_entry_count,
                request_duration,
                shard_entries,
                snapshot_duration,
                snapshot_last_success,
                replication_lag,
            }
        }
    }

    impl Metrics for PrometheusMetrics {
        fn record_read(&self, namespace: &str) {
            self.namespace_reads.with_label_values(&[namespace]).inc();
        }

        fn record_write(&self, namespace: &str) {
            self.namespace_writes.with_label_values(&[namespace]).inc();
        }

        fn set_namespace_entry_count(&self, namespace: &str, count: usize) {
            self.namespace_entry_count.with_label_values(&[namespace]).set(count as i64);
        }

        fn observe_request_duration(&self, operation: &str, namespace: &str, duration: Duration) {
            self.request_duration
                .with_label_values(&[operation, namespace])
                .observe(duration.as_secs_f64());
        }

        fn set_shard_entries(&self, shard_id: usize, count: usize) {
            self.shard_entries
                .with_label_values(&[&shard_id.to_string()])
                .set(count as i64);
        }

        fn observe_snapshot_duration(&self, duration: Duration) {
            self.snapshot_duration.observe(duration.as_secs_f64());
        }

        fn set_snapshot_last_success(&self) {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            self.snapshot_last_success.with_label_values(&[]).set(ts as i64);
        }

        fn set_replication_lag(&self, replica: &str, lag_seconds: f64) {
            self.replication_lag.with_label_values(&[replica]).set(lag_seconds);
        }

        fn render(&self) -> String {
            let encoder = TextEncoder::new();
            let metric_families = self.registry.gather();
            encoder.encode_to_string(&metric_families).unwrap_or_default()
        }
    }
}

#[cfg(feature = "metrics")]
pub use prometheus_impl::PrometheusMetrics;

#[cfg(feature = "metrics")]
/// Create a new metrics instance (real Prometheus-backed).
pub fn create_metrics() -> Arc<dyn Metrics> {
    Arc::new(PrometheusMetrics::new())
}

// ---------------------------------------------------------------------------
// No-op implementation when metrics feature is disabled
// ---------------------------------------------------------------------------

#[cfg(not(feature = "metrics"))]
mod noop_impl {
    use super::Metrics;
    use std::time::Duration;

    #[derive(Debug)]
    pub struct NoopMetrics;

    impl Metrics for NoopMetrics {
        fn record_read(&self, _namespace: &str) {}
        fn record_write(&self, _namespace: &str) {}
        fn set_namespace_entry_count(&self, _namespace: &str, _count: usize) {}
        fn observe_request_duration(&self, _operation: &str, _namespace: &str, _duration: Duration) {}
        fn set_shard_entries(&self, _shard_id: usize, _count: usize) {}
        fn observe_snapshot_duration(&self, _duration: Duration) {}
        fn set_snapshot_last_success(&self) {}
        fn set_replication_lag(&self, _replica: &str, _lag_seconds: f64) {}
        fn render(&self) -> String {
            String::new()
        }
    }
}

#[cfg(not(feature = "metrics"))]
pub use noop_impl::NoopMetrics;

#[cfg(not(feature = "metrics"))]
/// Create a new no-op metrics instance.
pub fn create_metrics() -> Arc<dyn Metrics> {
    Arc::new(NoopMetrics)
}

/// A guard that records the duration of a scope when dropped.
/// Used to measure request latency.
pub struct RequestTimer {
    metrics: Arc<dyn Metrics>,
    operation: String,
    namespace: String,
    start: Instant,
}

impl RequestTimer {
    pub fn new(metrics: Arc<dyn Metrics>, operation: &str, namespace: &str) -> Self {
        Self {
            metrics,
            operation: operation.to_string(),
            namespace: namespace.to_string(),
            start: Instant::now(),
        }
    }
}

impl Drop for RequestTimer {
    fn drop(&mut self) {
        let duration = self.start.elapsed();
        self.metrics.observe_request_duration(&self.operation, &self.namespace, duration);
    }
}
