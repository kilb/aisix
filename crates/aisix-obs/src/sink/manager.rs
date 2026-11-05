//! Per-exporter pipeline manager.
//!
//! Owns one [`SinkPipeline`] per configured exporter. The request hot path
//! resolves an exporter's pipeline with [`ExporterPipelines::get_or_create`]
//! — lazily starting it on first sighting, and rebuilding it when the
//! exporter's config changes — then enqueues into the returned handle. A
//! [`ExporterPipelines::reconcile`] immediately aborts pipelines for exporters
//! that left the live snapshot or whose delivery configuration changed.
//!
//! Lazy-on-first-sighting mirrors the previous `OtlpHttpFanOut` permit map:
//! it is immediately consistent with the snapshot (a just-added exporter
//! receives the very next request), avoiding the reconcile-loop race where a
//! newly-added exporter would miss requests until the next poll.
//!
//! Sink-agnostic — the caller supplies a `build` closure mapping an exporter
//! to an [`ObservabilitySink`], so the manager is shared by every
//! pipeline-backed sink family (`otlp`, `http_batch`, …).

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::{ObservabilitySink, PipelineConfig, SinkHandle, SinkPipeline, SinkStatsSnapshot};
use crate::metrics::Metrics;

/// One running pipeline plus the bookkeeping needed to stop/rebuild it.
struct Running {
    /// Hash of the exporter's delivery-relevant config. A change rebuilds the
    /// pipeline (old stops, new starts).
    fingerprint: u64,
    handle: SinkHandle,
    cancel: watch::Sender<bool>,
    /// Detached worker; awaited only on graceful [`ExporterPipelines::shutdown`].
    worker: JoinHandle<()>,
}

/// Manages the live set of per-exporter pipelines.
pub struct ExporterPipelines {
    running: Mutex<HashMap<String, Running>>,
    cfg: PipelineConfig,
    /// Handed to every pipeline this manager starts, so telemetry drops and
    /// hard delivery failures reach the Prometheus scrape and not only the
    /// heartbeat. `None` in tests and standalone construction.
    metrics: Option<Metrics>,
    /// Where pipelines run: the runtime this manager was built on.
    ///
    /// A pipeline starts on first sighting, which happens on the request
    /// path. Spawned from there, under thread-per-core serving, it would live
    /// on one request worker's runtime: its batch encoding would stall that
    /// worker's requests, and the worker dropping its runtime at the end of
    /// the drain would kill it before [`Self::shutdown`] could drain it.
    runtime: Option<tokio::runtime::Handle>,
}

impl ExporterPipelines {
    /// Build an empty manager. Pipelines start on first
    /// [`get_or_create`](Self::get_or_create).
    pub fn new(cfg: PipelineConfig) -> Self {
        Self {
            running: Mutex::new(HashMap::new()),
            cfg,
            metrics: None,
            runtime: tokio::runtime::Handle::try_current().ok(),
        }
    }

    /// Emit the fan-out drop/failure counters on `metrics`. Every
    /// pipeline this manager starts inherits the handle.
    pub fn with_metrics(mut self, metrics: Metrics) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Number of running pipelines.
    pub fn len(&self) -> usize {
        self.running.lock().len()
    }

    /// True when no pipeline is running.
    pub fn is_empty(&self) -> bool {
        self.running.lock().is_empty()
    }

    /// Resolve the pipeline handle for exporter `key`, lazily starting it via
    /// `build` on first sighting — or rebuilding it (stop old, start new) when
    /// `fingerprint` changed (the exporter's config was updated). `build` runs
    /// only when a (re)start is needed; an unchanged exporter just returns its
    /// existing handle. The returned handle is cheap to clone and enqueue into.
    pub fn get_or_create(
        &self,
        key: &str,
        fingerprint: u64,
        build: impl FnOnce() -> Arc<dyn ObservabilitySink>,
    ) -> SinkHandle {
        let mut running = self.running.lock();
        if let Some(existing) = running.get(key) {
            if existing.fingerprint == fingerprint {
                return existing.handle.clone();
            }
            // Config changed — drop queued records for the old destination
            // immediately, then rebuild below. Draining here could deliver
            // captured content after the operator revoked that config.
            if let Some(old) = running.remove(key) {
                old.handle.count_queued_as_dropped();
                old.worker.abort();
                tracing::info!(exporter = %key, "rebuilding reconfigured exporter pipeline");
            }
        }
        let sink = build();
        let (handle, pipeline) =
            SinkPipeline::with_metrics(sink, self.cfg.clone(), self.metrics.clone());
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let worker = match &self.runtime {
            Some(runtime) => runtime.spawn(pipeline.run(cancel_rx)),
            None => tokio::spawn(pipeline.run(cancel_rx)),
        };
        running.insert(
            key.to_string(),
            Running {
                fingerprint,
                handle: handle.clone(),
                cancel: cancel_tx,
                worker,
            },
        );
        handle
    }

    /// Abort pipelines absent from `desired` or whose desired fingerprint no
    /// longer matches the running configuration, discarding queued records.
    /// Deletion, disablement, and reconfiguration are authorization
    /// revocations; a graceful drain could send captured content to a stale
    /// destination afterwards. Replacement pipelines remain lazy and start on
    /// the next [`Self::get_or_create`] call.
    pub fn reconcile(&self, desired: &HashMap<String, u64>) {
        let mut running = self.running.lock();
        running.retain(|key, pipeline| match desired.get(key) {
            Some(fingerprint) if *fingerprint == pipeline.fingerprint => true,
            Some(_) => {
                pipeline.handle.count_queued_as_dropped();
                pipeline.worker.abort();
                tracing::info!(exporter = %key, "stopping reconfigured exporter pipeline");
                false
            }
            None => {
                pipeline.handle.count_queued_as_dropped();
                pipeline.worker.abort();
                tracing::info!(exporter = %key, "stopping removed exporter pipeline");
                false
            }
        });
    }

    /// Whether a pipeline for `key` is running this exact configuration.
    pub fn is_running(&self, key: &str, fingerprint: u64) -> bool {
        self.running
            .lock()
            .get(key)
            .is_some_and(|pipeline| pipeline.fingerprint == fingerprint)
    }

    /// Per-exporter delivery stats, keyed by exporter name (health/dashboard).
    pub fn stats(&self) -> HashMap<String, SinkStatsSnapshot> {
        self.running
            .lock()
            .iter()
            .map(|(key, pipeline)| (key.clone(), pipeline.handle.stats()))
            .collect()
    }

    /// Records waiting in each running pipeline's queue, keyed by exporter
    /// name. A pipeline stopped by a config change has discarded its queue.
    pub fn queue_depths(&self) -> Vec<(String, u64)> {
        self.running
            .lock()
            .iter()
            .map(|(key, pipeline)| (key.clone(), pipeline.handle.queued()))
            .collect()
    }

    /// Stop every pipeline and await its worker (graceful shutdown). Each
    /// pipeline performs a final drain before exiting.
    pub async fn shutdown(&self) {
        let pipelines: Vec<Running> = self.running.lock().drain().map(|(_, p)| p).collect();
        for pipeline in &pipelines {
            let _ = pipeline.cancel.send(true);
        }
        for pipeline in pipelines {
            let _ = pipeline.worker.await;
        }
    }

    /// [`Self::shutdown`], giving up at `deadline`, for the process exit.
    ///
    /// A pipeline bounds its own drain, but only between deliveries: a sink
    /// that never returns from one holds it for as long as that sink's own
    /// timeout, which is the sink's to decide. Returns whether every pipeline
    /// finished; what a pipeline given up on still holds is counted as lost.
    pub async fn shutdown_within(&self, deadline: std::time::Duration) -> bool {
        let pipelines: Vec<Running> = self.running.lock().drain().map(|(_, p)| p).collect();
        for pipeline in &pipelines {
            let _ = pipeline.cancel.send(true);
        }
        let until = tokio::time::Instant::now() + deadline;
        let mut finished = true;
        for pipeline in pipelines {
            let abort = pipeline.worker.abort_handle();
            if tokio::time::timeout_at(until, pipeline.worker)
                .await
                .is_err()
            {
                pipeline.handle.count_queued_as_dropped();
                abort.abort();
                finished = false;
            }
        }
        finished
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sink::{
        BatchUnit, EventBatch, IdempotencyMarker, IdempotencyScheme, ObservabilitySink,
        OrderingScope, SinkAck, SinkCapabilities, SinkHealth, SinkRecord, SinkResult,
    };
    use crate::usage::UsageEvent;
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

    /// A sink that adds each delivered batch's size to a shared counter.
    struct CountingSink {
        delivered: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ObservabilitySink for CountingSink {
        fn name(&self) -> &str {
            "counting"
        }
        fn capabilities(&self) -> SinkCapabilities {
            SinkCapabilities {
                idempotency: IdempotencyScheme::None,
                ordering: OrderingScope::None,
                batch_unit: BatchUnit::Records,
                max_batch_bytes: None,
                supports_partial_batch: false,
                supports_streaming_ingest: false,
            }
        }
        async fn append_batch(
            &self,
            batch: &EventBatch,
            _marker: &IdempotencyMarker,
        ) -> SinkResult {
            self.delivered.fetch_add(batch.len(), Ordering::Relaxed);
            Ok(SinkAck {
                accepted: batch.len(),
                ..SinkAck::default()
            })
        }
        async fn healthcheck(&self) -> SinkHealth {
            SinkHealth::healthy()
        }
    }

    fn rec() -> Arc<SinkRecord> {
        Arc::new(SinkRecord::metadata_only(UsageEvent::default()))
    }

    fn manager() -> ExporterPipelines {
        // Long flush interval so the only flush is the shutdown drain —
        // keeps delivery assertions deterministic.
        let cfg = PipelineConfig {
            flush_interval: std::time::Duration::from_secs(60),
            ..PipelineConfig::default()
        };
        ExporterPipelines::new(cfg)
    }

    /// A `build` closure that counts invocations and shares a delivery counter.
    fn counting(
        builds: &Arc<AtomicU32>,
        delivered: &Arc<AtomicUsize>,
    ) -> impl FnOnce() -> Arc<dyn ObservabilitySink> {
        let builds = Arc::clone(builds);
        let delivered = Arc::clone(delivered);
        move || {
            builds.fetch_add(1, Ordering::Relaxed);
            Arc::new(CountingSink { delivered }) as Arc<dyn ObservabilitySink>
        }
    }

    /// A sink that never finishes a delivery, so records stay queued.
    struct StuckSink;

    #[async_trait::async_trait]
    impl ObservabilitySink for StuckSink {
        fn name(&self) -> &str {
            "stuck"
        }
        fn capabilities(&self) -> SinkCapabilities {
            SinkCapabilities {
                idempotency: IdempotencyScheme::None,
                ordering: OrderingScope::None,
                batch_unit: BatchUnit::Records,
                max_batch_bytes: None,
                supports_partial_batch: false,
                supports_streaming_ingest: false,
            }
        }
        async fn append_batch(
            &self,
            _batch: &EventBatch,
            _marker: &IdempotencyMarker,
        ) -> SinkResult {
            std::future::pending().await
        }
        async fn healthcheck(&self) -> SinkHealth {
            SinkHealth::healthy()
        }
    }

    /// A sink that never comes back from a delivery must not hold the exit:
    /// nothing after the drain runs until it returns.
    #[tokio::test(start_paused = true)]
    async fn an_exit_gives_up_on_a_sink_that_never_returns() {
        let metrics = Metrics::new(false);
        let mgr = ExporterPipelines::new(PipelineConfig {
            flush_interval: std::time::Duration::from_secs(60),
            max_batch: 1,
            ..PipelineConfig::default()
        })
        .with_metrics(metrics.clone());
        let handle = mgr.get_or_create("stuck", 1, || {
            Arc::new(StuckSink) as Arc<dyn ObservabilitySink>
        });
        for _ in 0..3 {
            assert!(handle.try_enqueue(rec()));
        }
        tokio::task::yield_now().await;

        let started = tokio::time::Instant::now();
        let finished = mgr
            .shutdown_within(std::time::Duration::from_secs(10))
            .await;
        assert!(!finished, "the stuck pipeline did not finish");
        assert_eq!(started.elapsed(), std::time::Duration::from_secs(10));
        assert!(mgr.is_empty());
        assert!(
            handle.stats().dropped > 0,
            "what it still held has to be accounted for"
        );
    }

    /// Discarding a revoked exporter's queue is deliberate, but it is still
    /// lost telemetry, and the drop counter is what an operator reads to learn
    /// how much of it there was.
    #[tokio::test]
    async fn discarding_a_stopped_pipelines_queue_is_counted() {
        for reconfigured in [false, true] {
            let metrics = Metrics::new(false);
            let mgr = manager().with_metrics(metrics.clone());
            let handle = mgr.get_or_create("gone", 1, || {
                Arc::new(StuckSink) as Arc<dyn ObservabilitySink>
            });
            for _ in 0..3 {
                assert!(handle.try_enqueue(rec()));
            }
            if reconfigured {
                mgr.get_or_create("gone", 2, || {
                    Arc::new(StuckSink) as Arc<dyn ObservabilitySink>
                });
            } else {
                mgr.reconcile(&HashMap::new());
            }
            assert_eq!(handle.stats().dropped, 3);
            let rendered = metrics.render();
            assert!(
                rendered.lines().any(|line| line
                    .starts_with(crate::metrics::M_OTLP_FANOUT_DROPS_TOTAL)
                    && line.contains("reason=\"worker_stopped\"")
                    && line.ends_with(" 3")),
                "reconfigured={reconfigured}: {rendered}"
            );
        }
    }

    #[tokio::test]
    async fn get_or_create_starts_once_and_reuses() {
        let mgr = manager();
        let builds = Arc::new(AtomicU32::new(0));
        let delivered = Arc::new(AtomicUsize::new(0));
        let h1 = mgr.get_or_create("a", 1, counting(&builds, &delivered));
        let h2 = mgr.get_or_create("a", 1, counting(&builds, &delivered));
        assert_eq!(
            builds.load(Ordering::Relaxed),
            1,
            "second call reuses the pipeline"
        );
        assert_eq!(mgr.len(), 1);

        // Both handles point at the same pipeline.
        h1.try_enqueue(rec());
        h2.try_enqueue(rec());
        mgr.shutdown().await;
        assert_eq!(delivered.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn get_or_create_rebuilds_on_fingerprint_change() {
        let mgr = manager();
        let builds = Arc::new(AtomicU32::new(0));
        let delivered = Arc::new(AtomicUsize::new(0));
        mgr.get_or_create("a", 1, counting(&builds, &delivered));
        mgr.get_or_create("a", 2, counting(&builds, &delivered));
        assert_eq!(
            builds.load(Ordering::Relaxed),
            2,
            "config change rebuilds the pipeline"
        );
        assert_eq!(mgr.len(), 1, "still exactly one pipeline for exporter a");
    }

    #[tokio::test]
    async fn reconcile_stops_absent_exporters() {
        let mgr = manager();
        let builds = Arc::new(AtomicU32::new(0));
        let delivered = Arc::new(AtomicUsize::new(0));
        mgr.get_or_create("a", 1, counting(&builds, &delivered));
        mgr.get_or_create("b", 1, counting(&builds, &delivered));
        assert_eq!(mgr.len(), 2);

        let desired: HashMap<String, u64> = [("a".to_string(), 1)].into_iter().collect();
        mgr.reconcile(&desired);
        assert_eq!(mgr.len(), 1, "exporter b's pipeline was stopped");
    }

    #[tokio::test]
    async fn reconcile_discards_queued_records_for_removed_exporter() {
        let mgr = manager();
        let builds = Arc::new(AtomicU32::new(0));
        let delivered = Arc::new(AtomicUsize::new(0));
        let handle = mgr.get_or_create("revoked", 1, counting(&builds, &delivered));
        // Let the interval's immediate empty tick complete, then place one
        // record in the long-lived batch and revoke the exporter.
        tokio::task::yield_now().await;
        assert!(handle.try_enqueue(rec()));
        tokio::task::yield_now().await;

        mgr.reconcile(&HashMap::new());
        tokio::task::yield_now().await;
        assert_eq!(delivered.load(Ordering::Relaxed), 0);
        assert!(mgr.is_empty());
    }

    #[tokio::test]
    async fn reconcile_discards_queued_records_for_reconfigured_exporter() {
        let mgr = manager();
        let builds = Arc::new(AtomicU32::new(0));
        let delivered = Arc::new(AtomicUsize::new(0));
        let handle = mgr.get_or_create("changed", 1, counting(&builds, &delivered));
        tokio::task::yield_now().await;
        assert!(handle.try_enqueue(rec()));
        tokio::task::yield_now().await;

        let desired: HashMap<String, u64> = [("changed".to_string(), 2)].into_iter().collect();
        mgr.reconcile(&desired);
        tokio::task::yield_now().await;
        assert_eq!(delivered.load(Ordering::Relaxed), 0);
        assert!(mgr.is_empty(), "replacement remains lazy");

        mgr.get_or_create("changed", 2, counting(&builds, &delivered));
        assert_eq!(builds.load(Ordering::Relaxed), 2);
        mgr.shutdown().await;
    }
}
