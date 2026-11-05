use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use aisix_core::models::BackgroundModelCheck;
use aisix_core::resource::ResourceEntry;
use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{AisixSnapshot, Model};
use aisix_gateway::{BridgeError, ChatFormat, ChatMessage, EmbeddingRequest, Hub};
use tokio::sync::{watch, Semaphore};
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::dispatch;
use crate::health::ModelRuntimeStatusTracker;

/// Cap on the number of background model checks that may run
/// concurrently across all configured direct models. Each check
/// issues a real chat completion against the upstream provider —
/// burning the operator's quota and dollars — so we serialize them
/// to keep the cost bounded regardless of how many direct models
/// the operator has registered.
///
/// Rationale: a deployment with 100 direct models all configured
/// with the same `interval_seconds` would otherwise fan out 100
/// concurrent requests to upstream providers every interval. The
/// semaphore turns that into a slow trickle of ≤4 in-flight checks
/// at any time. The total cost-per-interval is unchanged; the
/// burstiness (and the chance of self-induced 429 on small
/// accounts) is dampened.
const MAX_CONCURRENT_BACKGROUND_CHECKS: usize = 4;

/// Whether a direct model's upstream id is a concrete string the prober
/// can send. A wildcard alias row whose `model_name` template is itself a
/// glob (`openai/*` → `"*"`) or absent has no concrete id — probing would
/// send the `*` verbatim, burn real quota every interval, and pin the row
/// permanently Unhealthy. A wildcard alias pinned to a concrete template
/// (`gpt-*` → `gpt-4o`) probes that template safely and keeps its check.
fn upstream_template_is_probeable(model: &aisix_core::Model) -> bool {
    model
        .model_name
        .as_deref()
        .is_some_and(|m| !m.contains('*'))
}

/// The check a model runs, if it runs one.
fn enabled_check(model: &Model) -> Option<&BackgroundModelCheck> {
    if model.is_routing() || !upstream_template_is_probeable(model) {
        return None;
    }
    model.background_model_check.as_ref().filter(|c| c.enabled)
}

/// When each model's check last started. `interval_seconds` is a statement
/// about one model, so each is timed on its own: a check is a paid upstream
/// call, and one model's short interval is no reason to spend another's budget
/// at that rate.
#[derive(Default)]
struct CheckSchedule {
    last_started: HashMap<String, Instant>,
    /// Checks still running. One never overlaps itself, however short its
    /// interval against its timeout.
    in_flight: HashSet<String>,
}

impl CheckSchedule {
    /// The rows due at `now`, marked as started, and when the next one not
    /// running falls due. Reads each row's interval as it stands, so a
    /// shortened one applies from the last start rather than from the next.
    fn take_due(
        &mut self,
        snapshot: &AisixSnapshot,
        now: Instant,
    ) -> (Vec<Arc<ResourceEntry<Model>>>, Option<Instant>) {
        let checked = snapshot
            .models
            .matching_entries(|e| enabled_check(&e.value).is_some());
        // A row that left and came back is checked as soon as it returns.
        let present: HashSet<&str> = checked.iter().map(|e| e.id.as_str()).collect();
        self.last_started
            .retain(|id, _| present.contains(id.as_str()));
        let mut due = Vec::new();
        let mut next = None::<Instant>;
        for entry in &checked {
            let Some(cfg) = enabled_check(&entry.value) else {
                continue;
            };
            if self.in_flight.contains(&entry.id) {
                continue;
            }
            let interval = Duration::from_secs(cfg.interval_seconds.max(1));
            let mut at = self.last_started.get(&entry.id).map(|t| *t + interval);
            if at.is_none_or(|at| at <= now) {
                self.last_started.insert(entry.id.clone(), now);
                self.in_flight.insert(entry.id.clone());
                at = Some(now + interval);
                due.push(Arc::clone(entry));
            }
            next = at.map(|at| next.map_or(at, |n: Instant| n.min(at)));
        }
        (due, next)
    }

    fn finished(&mut self, id: &str) {
        self.in_flight.remove(id);
    }
}

/// Run every enabled background check on its own interval until `cancel`.
///
/// A slow check holds nothing but its own permit, so a model whose upstream
/// hangs for its whole `timeout_seconds` does not delay anyone else's. Follows
/// configuration changes as they publish, and abandons running checks at
/// shutdown: a probe has nothing to report once the process is going away, and
/// its timeout is the operator's to set.
pub async fn run_background_model_checks(
    snapshot: SnapshotHandle<AisixSnapshot>,
    hub: Arc<Hub>,
    tracker: Arc<ModelRuntimeStatusTracker>,
    mut cancel: watch::Receiver<bool>,
) {
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_BACKGROUND_CHECKS));
    let mut published = snapshot.subscribe();
    let mut schedule = CheckSchedule::default();
    let mut running: JoinSet<String> = JoinSet::new();
    loop {
        if *cancel.borrow() {
            return;
        }
        let snap = snapshot.load();
        let (due, next) = schedule.take_due(&snap, Instant::now());
        for entry in due {
            let snapshot = Arc::clone(&snap);
            let hub = Arc::clone(&hub);
            let tracker = Arc::clone(&tracker);
            let permits = Arc::clone(&semaphore);
            running.spawn(async move {
                // Acquire concurrency permit. If the semaphore is closed
                // (only happens during shutdown), the check is skipped
                // and the tracker stays at last-known state — fine.
                if let Ok(_permit) = permits.acquire_owned().await {
                    if let Some(cfg) = enabled_check(&entry.value) {
                        run_one(
                            Arc::clone(&snapshot),
                            hub,
                            tracker,
                            &entry.id,
                            &entry.value,
                            cfg,
                            "background-model-check",
                        )
                        .await;
                    }
                }
                entry.id.clone()
            });
        }
        drop(snap);
        tokio::select! {
            changed = cancel.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            changed = published.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            Some(done) = running.join_next(), if !running.is_empty() => {
                match done {
                    Ok(id) => schedule.finished(&id),
                    // A panicked check names no model. Its row must not stay
                    // marked as running for good, so nothing here is; at
                    // worst a check overlaps itself once.
                    Err(_) => schedule.in_flight.clear(),
                }
            }
            _ = sleep_until_due(next) => {}
        }
    }
}

async fn sleep_until_due(next: Option<Instant>) {
    match next {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

async fn run_one(
    snapshot: Arc<AisixSnapshot>,
    hub: Arc<Hub>,
    tracker: Arc<ModelRuntimeStatusTracker>,
    id: &str,
    model: &Arc<Model>,
    cfg: &BackgroundModelCheck,
    request_id: &str,
) {
    let outcome = check_direct_model(&snapshot, &hub, id, model, cfg, request_id).await;
    match outcome {
        Ok(()) => tracker.clear_unhealthy(id),
        Err(BridgeError::UpstreamStatus { status, .. })
            if cfg.ignore_statuses.contains(&status) =>
        {
            tracker.record_ignored_check(id, status, "ignored_transient_error")
        }
        Err(BridgeError::Timeout { .. }) if cfg.ignore_statuses.contains(&408) => {
            tracker.record_ignored_check(id, 408, "ignored_transient_error")
        }
        Err(err) => {
            tracker.mark_unhealthy(id, background_status_code(&err), "background_check_failed")
        }
    }
}

async fn check_direct_model(
    snapshot: &AisixSnapshot,
    hub: &Hub,
    model_id: &str,
    model: &Arc<Model>,
    cfg: &BackgroundModelCheck,
    request_id: &str,
) -> Result<(), BridgeError> {
    let _provider =
        dispatch::require_provider(model).map_err(|e| BridgeError::Config(e.to_string()))?;
    let pk_entry = dispatch::resolve_provider_key(snapshot, model)
        .map_err(|e| BridgeError::Config(e.to_string()))?;
    let bridge = dispatch::resolve_bridge(hub, &pk_entry.value).ok_or_else(|| {
        BridgeError::Config(format!(
            "no bridge registered for provider_key provider={:?} adapter={:?}",
            pk_entry.value.provider, pk_entry.value.adapter
        ))
    })?;

    let req = ChatFormat {
        model: model.display_name.clone(),
        messages: vec![ChatMessage::user(cfg.prompt.clone())],
        max_tokens: Some(cfg.max_tokens),
        ..ChatFormat::new(model.display_name.clone(), vec![])
    };
    let ctx = dispatch::bridge_ctx(
        request_id,
        model_id,
        Arc::clone(model),
        &pk_entry.id,
        Arc::clone(&pk_entry.value),
        None,
    )
    .with_deadline(timeout(cfg));

    // A model is probed the way it is called. An embedding model answers a
    // chat completion with an error, which would read as the model being down
    // and take it out of its group while it works fine.
    if model.is_embedding() {
        let req = EmbeddingRequest {
            model: model.display_name.clone(),
            input: vec![cfg.prompt.clone()],
            input_was_single: true,
            encoding_format: None,
            dimensions: None,
            input_type: None,
            embedding_types: None,
            token_input: None,
            extra: Default::default(),
        };
        let _ = bridge.embed(&req, &ctx).await?;
        return Ok(());
    }

    let _ = bridge.chat(&req, &ctx).await?;
    let _ = model_id;
    Ok(())
}

fn timeout(cfg: &BackgroundModelCheck) -> Duration {
    Duration::from_secs(cfg.timeout_seconds)
}

fn background_status_code(err: &BridgeError) -> Option<u16> {
    match err {
        BridgeError::UpstreamStatus { status, .. } => Some(*status),
        BridgeError::Timeout { .. } => Some(408),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use aisix_provider_openai::OpenAiBridge;
    use reqwest::Client;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn upstream_template_probeability() {
        let model = |model_name: Option<&str>| -> aisix_core::Model {
            let mut v = serde_json::json!({
                "display_name": "d",
                "provider": "openai",
                "provider_key_id": "pk",
            });
            if let Some(m) = model_name {
                v["model_name"] = serde_json::json!(m);
            }
            serde_json::from_value(v).unwrap()
        };
        // Concrete template probes safely — including a wildcard ALIAS
        // pinned to a concrete upstream (`gpt-*` → `gpt-4o`).
        assert!(upstream_template_is_probeable(&model(Some("gpt-4o"))));
        // A glob template would send `*` verbatim — skip.
        assert!(!upstream_template_is_probeable(&model(Some("*"))));
        assert!(!upstream_template_is_probeable(&model(Some("gpt-*"))));
        // No upstream id at all — skip.
        assert!(!upstream_template_is_probeable(&model(None)));
    }

    /// Every eligible row's check, once, in sequence.
    async fn run_background_model_check_once(
        snapshot: Arc<AisixSnapshot>,
        hub: Arc<Hub>,
        tracker: Arc<ModelRuntimeStatusTracker>,
        request_id: &str,
    ) {
        for entry in snapshot
            .models
            .matching_entries(|e| enabled_check(&e.value).is_some())
        {
            let cfg = enabled_check(&entry.value).unwrap();
            run_one(
                Arc::clone(&snapshot),
                Arc::clone(&hub),
                Arc::clone(&tracker),
                &entry.id,
                &entry.value,
                cfg,
                request_id,
            )
            .await;
        }
    }

    fn checked_model(id: &str, interval_seconds: u64) -> ResourceEntry<Model> {
        let model: Model = serde_json::from_value(serde_json::json!({
            "display_name": id,
            "provider": "openai",
            "model_name": "gpt-4o-mini",
            "provider_key_id": "pk-1",
            "background_model_check": {
                "enabled": true,
                "interval_seconds": interval_seconds,
                "timeout_seconds": 10,
                "prompt": "ping",
                "max_tokens": 1,
                "stale_after_seconds": 90
            }
        }))
        .unwrap();
        ResourceEntry::new(id, model, 1)
    }

    fn ids(due: &[Arc<ResourceEntry<Model>>]) -> Vec<String> {
        let mut ids: Vec<String> = due.iter().map(|e| e.id.clone()).collect();
        ids.sort();
        ids
    }

    /// `interval_seconds` is per model. Probing everything at the shortest
    /// one spends a long-interval model's upstream budget many times over.
    #[test]
    fn each_model_is_due_on_its_own_interval() {
        let snap = AisixSnapshot::new();
        snap.models.insert(checked_model("fast", 30));
        snap.models.insert(checked_model("slow", 3600));
        let mut schedule = CheckSchedule::default();
        let t0 = Instant::now();

        let (due, next) = schedule.take_due(&snap, t0);
        assert_eq!(ids(&due), ["fast", "slow"], "both start at boot");
        assert_eq!(next, Some(t0 + Duration::from_secs(30)));
        schedule.finished("fast");
        schedule.finished("slow");

        let (due, next) = schedule.take_due(&snap, t0 + Duration::from_secs(29));
        assert!(due.is_empty());
        assert_eq!(next, Some(t0 + Duration::from_secs(30)));

        let t30 = t0 + Duration::from_secs(30);
        let (due, next) = schedule.take_due(&snap, t30);
        assert_eq!(ids(&due), ["fast"]);
        assert_eq!(next, Some(t30 + Duration::from_secs(30)));
        schedule.finished("fast");

        let t3600 = t0 + Duration::from_secs(3600);
        let (due, _) = schedule.take_due(&snap, t3600);
        assert_eq!(ids(&due), ["fast", "slow"]);
    }

    #[test]
    fn a_running_check_does_not_overlap_itself_or_hold_up_another() {
        let snap = AisixSnapshot::new();
        snap.models.insert(checked_model("hung", 5));
        snap.models.insert(checked_model("fine", 5));
        let mut schedule = CheckSchedule::default();
        let t0 = Instant::now();
        let (due, _) = schedule.take_due(&snap, t0);
        assert_eq!(due.len(), 2);
        schedule.finished("fine");

        let t5 = t0 + Duration::from_secs(5);
        let (due, next) = schedule.take_due(&snap, t5);
        assert_eq!(ids(&due), ["fine"], "`hung` is still running");
        assert_eq!(next, Some(t5 + Duration::from_secs(5)));

        schedule.finished("hung");
        schedule.finished("fine");
        let (due, _) = schedule.take_due(&snap, t5);
        assert_eq!(ids(&due), ["hung"], "due as soon as it finishes");
    }

    #[test]
    fn the_schedule_follows_configuration_changes() {
        let snap = AisixSnapshot::new();
        snap.models.insert(checked_model("m", 3600));
        let mut schedule = CheckSchedule::default();
        let t0 = Instant::now();
        let (due, _) = schedule.take_due(&snap, t0);
        assert_eq!(due.len(), 1);
        schedule.finished("m");

        // Shortened: measured from the last start, not from the next one.
        snap.models.insert(checked_model("m", 5));
        let (due, next) = schedule.take_due(&snap, t0 + Duration::from_secs(1));
        assert!(due.is_empty());
        assert_eq!(next, Some(t0 + Duration::from_secs(5)));

        // Newly added: its first check does not wait for anything.
        snap.models.insert(checked_model("new", 3600));
        let (due, _) = schedule.take_due(&snap, t0 + Duration::from_secs(1));
        assert_eq!(ids(&due), ["new"]);
    }

    #[tokio::test]
    async fn a_hung_check_holds_up_neither_another_model_nor_the_exit() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({"messages": [{"role": "user", "content": "hang"}]}),
            ))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(600)))
            .mount(&upstream)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(503).set_body_string("down"))
            .mount(&upstream)
            .await;

        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(openai_test_bridge()));
        let snap = AisixSnapshot::new();
        snap.provider_keys
            .insert(provider_key_entry("pk-1", &upstream.uri()));
        let mut hung = checked_model("hung", 3600);
        let mut hung_model = (*hung.value).clone();
        if let Some(cfg) = hung_model.background_model_check.as_mut() {
            cfg.prompt = "hang".into();
            cfg.timeout_seconds = 600;
        }
        hung.value = Arc::new(hung_model);
        snap.models.insert(hung);
        snap.models.insert(checked_model("other", 3600));
        let tracker = Arc::new(ModelRuntimeStatusTracker::new());
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let task = tokio::spawn(run_background_model_checks(
            SnapshotHandle::new(snap),
            hub,
            tracker.clone(),
            cancel_rx,
        ));

        let deadline = Instant::now() + Duration::from_secs(10);
        while tracker.status("other").status != crate::RuntimeStatus::Unhealthy {
            assert!(Instant::now() < deadline, "the hung check held up `other`");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        cancel_tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("cancel must not wait out the hung check's timeout")
            .unwrap();
    }

    #[tokio::test]
    async fn an_embedding_model_is_probed_with_an_embeddings_call() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "model": "text-embedding-3-small",
                "data": [{"index": 0, "object": "embedding", "embedding": [0.1, 0.2]}],
                "usage": {"prompt_tokens": 1, "total_tokens": 1}
            })))
            .expect(1)
            .mount(&upstream)
            .await;
        // The answer such a model gives a chat completion.
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(404).set_body_string("not a chat model"))
            .expect(0)
            .mount(&upstream)
            .await;

        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(openai_test_bridge()));
        let snapshot = Arc::new(AisixSnapshot::new());
        snapshot
            .provider_keys
            .insert(provider_key_entry("pk-1", &upstream.uri()));
        let model: Model = serde_json::from_value(serde_json::json!({
            "display_name": "embed",
            "provider": "openai",
            "model_name": "text-embedding-3-small",
            "provider_key_id": "pk-1",
            "embedding": {"dimensions": 2},
            "background_model_check": {
                "enabled": true,
                "interval_seconds": 30,
                "timeout_seconds": 10,
                "prompt": "ping",
                "max_tokens": 1,
                "stale_after_seconds": 90
            }
        }))
        .unwrap();
        snapshot.models.insert(ResourceEntry::new("m-1", model, 1));
        let tracker = Arc::new(ModelRuntimeStatusTracker::new());

        run_background_model_check_once(snapshot, hub, tracker.clone(), "bg-check-1").await;

        assert_eq!(tracker.status("m-1").status, crate::RuntimeStatus::Healthy);
    }

    fn openai_test_bridge() -> OpenAiBridge {
        let client = Client::builder()
            .user_agent("aisix-test/0.1")
            .no_proxy()
            .build()
            .unwrap();
        OpenAiBridge::with_client(client)
    }

    fn provider_key_entry(id: &str, api_base: &str) -> ResourceEntry<aisix_core::ProviderKey> {
        let cfg = format!(
            r#"{{"display_name":"pk-{id}","secret":"sk-upstream","api_base":"{api_base}","provider":"openai","adapter":"openai"}}"#
        );
        let pk: aisix_core::ProviderKey = serde_json::from_str(&cfg).unwrap();
        ResourceEntry::new(id, pk, 1)
    }

    fn direct_model_entry(
        id: &str,
        name: &str,
        pk_id: &str,
        enabled: bool,
        ignore: &[u16],
    ) -> ResourceEntry<Model> {
        let cfg = serde_json::json!({
            "display_name": name,
            "provider": "openai",
            "model_name": "gpt-4o-mini",
            "provider_key_id": pk_id,
            "background_model_check": {
                "enabled": enabled,
                "interval_seconds": 30,
                "timeout_seconds": 10,
                "prompt": "Respond with OK",
                "max_tokens": 8,
                "ignore_statuses": ignore,
                "stale_after_seconds": 90
            }
        });
        let model: Model = serde_json::from_value(cfg).unwrap();
        ResourceEntry::new(id, model, 1)
    }

    #[tokio::test]
    async fn background_check_marks_unhealthy_on_failure() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(503).set_body_string("down"))
            .mount(&upstream)
            .await;

        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(openai_test_bridge()));
        let snapshot = Arc::new(AisixSnapshot::new());
        snapshot
            .provider_keys
            .insert(provider_key_entry("pk-1", &upstream.uri()));
        snapshot.models.insert(direct_model_entry(
            "m-1",
            "bg-model",
            "pk-1",
            true,
            &[408, 429],
        ));
        let tracker = Arc::new(ModelRuntimeStatusTracker::new());

        run_background_model_check_once(snapshot, hub, tracker.clone(), "bg-check-1").await;

        let status = tracker.status("m-1");
        assert_eq!(status.status, crate::RuntimeStatus::Unhealthy);
        assert_eq!(status.last_check_status, Some(503));
    }

    #[tokio::test]
    async fn background_check_ignores_configured_transient_status() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(429).set_body_string("slow down"))
            .mount(&upstream)
            .await;

        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(openai_test_bridge()));
        let snapshot = Arc::new(AisixSnapshot::new());
        snapshot
            .provider_keys
            .insert(provider_key_entry("pk-1", &upstream.uri()));
        snapshot.models.insert(direct_model_entry(
            "m-1",
            "bg-model",
            "pk-1",
            true,
            &[408, 429],
        ));
        let tracker = Arc::new(ModelRuntimeStatusTracker::new());

        run_background_model_check_once(snapshot, hub, tracker.clone(), "bg-check-1").await;

        let status = tracker.status("m-1");
        assert_eq!(status.status, crate::RuntimeStatus::Healthy);
        assert_eq!(status.last_check_status, Some(429));
        assert_eq!(
            status.status_reason.as_deref(),
            Some("ignored_transient_error")
        );
    }
}
