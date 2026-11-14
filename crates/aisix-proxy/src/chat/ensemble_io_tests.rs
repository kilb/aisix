use super::*;

use aisix_core::usage_io::UsageIoAudit;
use aisix_gateway::response_tap;
use aisix_gateway::upstream_io::{Protocol, UpstreamIoObserver};

fn state() -> (ProxyState, tokio::sync::mpsc::Receiver<UsageEvent>) {
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let state = ProxyState::new(
        aisix_core::snapshot::SnapshotHandle::new(AisixSnapshot::new()),
        Arc::new(aisix_gateway::Hub::new()),
        &aisix_core::ProxyConfig {
            addr: "127.0.0.1:0".into(),
            request_body_limit_bytes: 1_048_576,
            real_ip: Default::default(),
            request_id: Default::default(),
            url_rewrites: Vec::new(),
            expose_debug_headers: false,
            platform: Default::default(),
            tls: None,
            listeners: Vec::new(),
            thread_per_core: None,
            workers: None,
        },
    )
    .without_cache()
    .with_usage_sink(aisix_obs::UsageSink::new(tx));
    (state, rx)
}

fn rows() -> EnsembleRowCtx {
    EnsembleRowCtx {
        request_id: "ensemble-io".into(),
        client_model: "ensemble".into(),
        api_key_id: "key".into(),
        applied_guardrails: Vec::new(),
        audit: None,
        client: ClientContext::default(),
        started: Instant::now(),
    }
}

async fn observed_audit(text: &str, barrier: &tokio::sync::Barrier) -> UsageIoAudit {
    let tap = response_tap::detached();
    response_tap::scope(Arc::clone(&tap), async {
        let io = UpstreamIoObserver::observed(Protocol::Chat);
        io.observe_request_json(&serde_json::json!({
            "messages": [{"role": "user", "content": text}]
        }));
        barrier.wait().await;
        io.observe_output_text(text);
        io.finish_output_text();
        io.finish_response();
    })
    .await;
    tap.capture().io.unwrap().snapshot()
}

fn member(slot: u32, audit: UsageIoAudit) -> crate::ensemble::PanelOutcome {
    crate::ensemble::PanelOutcome {
        slot,
        model: format!("member-{slot}"),
        usage: aisix_gateway::UsageStats::new(7, 3),
        effective_usage: aisix_gateway::UsageStats::new(7, 3),
        usage_estimated: false,
        service_tier: None,
        io_audit: Some(audit),
    }
}

#[tokio::test]
async fn concurrent_member_audits_reach_distinct_rows_and_judge_extras_survive() {
    let barrier = tokio::sync::Barrier::new(2);
    let (a, b) = tokio::join!(
        observed_audit("A汉😀", &barrier),
        observed_audit("second-member", &barrier)
    );
    let judge = observed_audit("judge", &tokio::sync::Barrier::new(1)).await;
    let (state, mut rx) = state();
    let snap = state.snapshot.load();
    let ctx = rows();
    let members = [member(0, a.clone()), member(1, b.clone())];
    let answered = members
        .iter()
        .map(|outcome| (outcome.slot, EnsembleSubcall::of(&snap, outcome)))
        .collect::<Vec<_>>();
    emit_ensemble_rows(&state, &snap, &ctx, &answered, &[], None, false, "");
    let pk = crate::usage_attr::ResolvedPk::resolve(&snap, "");
    emit_usage_event(
        &state,
        &snap,
        &pk,
        &ctx.request_id,
        "judge",
        &ctx.client_model,
        &ctx.api_key_id,
        200,
        Duration::from_millis(1),
        11,
        5,
        UsageExtras {
            io_audit: Some(judge.clone()),
            attempt_index: 2,
            attempt_kind: "judge".into(),
            ..UsageExtras::default()
        },
        0.0,
        false,
        &ctx.client,
        None,
        true,
        true,
        &ctx.audit,
    );
    for (index, expected) in [a, b, judge].into_iter().enumerate() {
        let row = rx.try_recv().unwrap();
        assert_eq!(row.attempt_index, index as u32);
        assert_eq!(row.io_audit, Some(expected));
        assert_eq!(row.is_final_attempt, index == 2);
        assert_eq!(row.prompt_tokens, if index == 2 { 11 } else { 7 });
        assert_eq!(row.completion_tokens, if index == 2 { 5 } else { 3 });
    }
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn failed_subcall_uses_its_snapshot_and_undispatched_sibling_keeps_none() {
    let audit = observed_audit("failed-member", &tokio::sync::Barrier::new(1)).await;
    let (state, mut rx) = state();
    let snap = state.snapshot.load();
    let ctx = rows();
    let mut failed = crate::ensemble::SubcallFailure::new(
        0,
        "panel",
        "member",
        &BridgeError::Transport("reset".into()),
        false,
        Instant::now(),
    );
    failed.io_audit = Some(audit.clone());
    let refused = crate::ensemble::SubcallFailure::new(
        1,
        "panel",
        "refused",
        &BridgeError::Config("bad config".into()),
        false,
        Instant::now(),
    );
    let cell = Arc::new(crate::attribution::RequestAttribution::default());
    crate::attribution::scope(cell, async {
        let unrelated = UpstreamIoObserver::observed(Protocol::Chat);
        unrelated.observe_input_texts(["parent bytes must not replace a member"], true);
        emit_failed_subcall(&state, &snap, &ctx, &failed, 0);
        emit_failed_subcall(&state, &snap, &ctx, &refused, 1);
    })
    .await;
    let failed_row = rx.try_recv().unwrap();
    assert_eq!(failed_row.io_audit, Some(audit));
    assert_eq!(failed_row.prompt_tokens, 0);
    assert_eq!(failed_row.completion_tokens, 0);
    assert_eq!(failed_row.status_code, 502);
    let refused_row = rx.try_recv().unwrap();
    assert!(refused_row.io_audit.is_none());
    assert_eq!(refused_row.status_code, 500);
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn judge_stream_polls_inside_its_own_tap() {
    let parent = response_tap::detached();
    let judge = response_tap::detached();
    let stream = futures::stream::once(async {
        let io = UpstreamIoObserver::observed(Protocol::Chat);
        io.observe_input_texts(["judge input"], true);
        io.observe_output_text("judge output");
        io.finish_output_text();
        io.finish_response();
        Err(BridgeError::StreamAborted)
    })
    .boxed();
    let mut scoped = ensemble_stream_with_tap(stream, Arc::clone(&judge));
    response_tap::scope(Arc::clone(&parent), async {
        assert!(matches!(
            scoped.next().await,
            Some(Err(BridgeError::StreamAborted))
        ));
        assert!(scoped.next().await.is_none());
    })
    .await;
    assert!(parent.capture().io.is_none());
    let audit = judge.capture().io.unwrap().snapshot();
    assert_eq!(audit.exchange_count, Some(1));
    assert_eq!(audit.input_text.unwrap().utf8_bytes, 11);
    assert_eq!(audit.output_text.unwrap().utf8_bytes, 12);
}
