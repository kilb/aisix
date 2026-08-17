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
}

impl ExporterPipelines {
    /// Build an empty manager. Pipelines start on first
    /// [`get_or_create`](Self::get_or_create).
    pub fn new(cfg: PipelineConfig) -> Self {
        Self {
            running: Mutex::new(HashMap::new()),
            cfg,
        }
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
                old.worker.abort();
                tracing::info!(exporter = %key, "rebuilding reconfigured exporter pipeline");
            }
        }
        let sink = build();
        let (handle, pipeline) = SinkPipeline::new(sink, self.cfg.clone());
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let worker = tokio::spawn(pipeline.run(cancel_rx));
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
        running.retain(|key, pipeline| {
            if desired.get(key) == Some(&pipeline.fingerprint) {
                true
            } else {
                pipeline.worker.abort();
                tracing::info!(exporter = %key, "stopping revoked exporter pipeline");
                false
            }
        });
    }

    /// Per-exporter delivery stats, keyed by exporter name (health/dashboard).
    pub fn stats(&self) -> HashMap<String, SinkStatsSnapshot> {
        self.running
            .lock()
            .iter()
            .map(|(key, pipeline)| (key.clone(), pipeline.handle.stats()))
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
