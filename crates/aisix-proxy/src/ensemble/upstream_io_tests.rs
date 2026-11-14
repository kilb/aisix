use super::*;

use aisix_core::usage_io::{TextMeasurement, UsageIoAudit};
use aisix_gateway::chat::FinishReason;
use aisix_gateway::response_tap::{self, ResponseHeaderTap};
use aisix_gateway::upstream_io::{Protocol, UpstreamIoObserver};

fn outcome(target: &str) -> ModelCallOutcome {
    ModelCallOutcome {
        response: ChatResponse {
            native_envelope: None,
            logprobs: None,
            more_choices: Vec::new(),
            id: format!("id-{target}"),
            model: target.to_owned(),
            message: ChatMessage::assistant(target),
            finish_reason: FinishReason::Stop,
            usage: UsageStats::new(10, 20),
            service_tier: None,
        },
        effective_usage: UsageStats::new(10, 20),
        usage_estimated: false,
        io_audit: None,
    }
}

fn request_body(text: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "messages": [{"role": "user", "content": text}]
    }))
    .unwrap()
}

fn response_body(text: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "choices": [{"message": {"role": "assistant", "content": text}}]
    }))
    .unwrap()
}

fn text_measurement(text: &str, complete: bool) -> TextMeasurement {
    TextMeasurement {
        utf8_bytes: text.len() as u64,
        unicode_scalars: text.chars().count() as u64,
        complete,
    }
}

fn assert_exchange(audit: &UsageIoAudit, input: &str, output: &str) {
    assert_eq!(audit.boundary, "upstream_attempt");
    assert_eq!(audit.exchange_count, Some(1));
    assert_eq!(
        audit.request_body_bytes,
        Some(request_body(input).len() as u64)
    );
    assert_eq!(
        audit.response_body_bytes,
        Some(response_body(output).len() as u64)
    );
    assert_eq!(audit.request_complete, Some(true));
    assert!(audit.response_complete);
    assert_eq!(audit.input_text, Some(text_measurement(input, true)));
    assert_eq!(audit.output_text, Some(text_measurement(output, true)));
}

struct InterleavedCaller {
    panel_started: tokio::sync::Barrier,
}

#[async_trait]
impl ModelCaller for InterleavedCaller {
    async fn call(
        &self,
        target: &str,
        _req: &RequestBuilder<'_>,
    ) -> Result<ModelCallOutcome, CallError> {
        let exchanges: &[(&str, &str)] = match target {
            "a" => &[("A汉😀", "first"), ("retry-A", "second")],
            "b" => &[("BBBB", "乙")],
            "judge" => &[("judge-input", "judge-output")],
            _ => panic!("unexpected target {target}"),
        };
        for (index, (input, output)) in exchanges.iter().enumerate() {
            let io = UpstreamIoObserver::observed(Protocol::Chat);
            io.observe_request_body(&request_body(input));
            io.finish_request();
            if index == 0 && target != "judge" {
                // 两个成员都已登记交换后再读响应，确保共享 tap 的变异会串计。
                self.panel_started.wait().await;
                tokio::task::yield_now().await;
            }
            io.begin_response(Some("application/json"));
            io.observe_response_body(&response_body(output));
            if target == "a" && index == 0 {
                // 首次读取在传输终态前失败，重试只改变交换累计，不增加计费用量。
                io.fail_response();
            } else {
                io.finish_response();
            }
        }
        Ok(outcome(target))
    }
}

#[tokio::test]
async fn concurrent_members_keep_retry_exchanges_and_judge_isolated() {
    let caller = InterleavedCaller {
        panel_started: tokio::sync::Barrier::new(2),
    };
    let config: EnsembleConfig = serde_json::from_value(serde_json::json!({
        "panel": [{"model": "a"}, {"model": "b"}],
        "judge": {"model": "judge"}
    }))
    .unwrap();
    let parent = Arc::new(ResponseHeaderTap::new(Vec::new(), false, "r".into()));
    let progress = EnsembleProgress::default();
    let request = ChatFormat::new("ensemble", vec![ChatMessage::user("caller input")]);
    let result = response_tap::scope(
        Arc::clone(&parent),
        run_ensemble(&request, &config, &caller, &progress),
    )
    .await
    .unwrap();

    assert!(parent.capture().io.is_none());
    assert_eq!(result.panel.len(), 2);
    assert!(result.failed.is_empty());
    let a = result.panel[0].io_audit.as_ref().unwrap();
    assert_eq!(a.exchange_count, Some(2));
    assert_eq!(
        a.request_body_bytes,
        Some((request_body("A汉😀").len() + request_body("retry-A").len()) as u64)
    );
    assert_eq!(
        a.response_body_bytes,
        Some((response_body("first").len() + response_body("second").len()) as u64)
    );
    assert_eq!(a.input_text, Some(text_measurement("A汉😀retry-A", true)));
    assert_eq!(a.output_text, Some(text_measurement("firstsecond", false)));
    assert_eq!(a.request_complete, Some(true));
    assert!(!a.response_complete);
    assert_eq!(result.panel[0].effective_usage.total_tokens, 30);
    assert_exchange(result.panel[1].io_audit.as_ref().unwrap(), "BBBB", "乙");
    assert_exchange(
        result.judge_io_audit.as_ref().unwrap(),
        "judge-input",
        "judge-output",
    );

    let (members, judge) = progress.take();
    assert_eq!(members.len(), 2);
    for (recorded, returned) in members.into_iter().zip(&result.panel) {
        assert_eq!(recorded.unwrap().io_audit, returned.io_audit);
    }
    assert_eq!(judge.unwrap().outcome.io_audit, result.judge_io_audit);
}

struct FailingCaller;

#[async_trait]
impl ModelCaller for FailingCaller {
    async fn call(
        &self,
        target: &str,
        _req: &RequestBuilder<'_>,
    ) -> Result<ModelCallOutcome, CallError> {
        if target == "undispatched" {
            return Err(BridgeError::Config("invalid config".into()).into());
        }
        let io = UpstreamIoObserver::observed(Protocol::Chat);
        io.observe_request_body(&request_body("input"));
        io.finish_request();
        io.begin_response(Some("text/event-stream"));
        io.observe_response_body(b"data: {\"choices\":[{\"delta\":{\"content\":\"prefix\"}}]}\n\n");
        if target == "timeout" {
            std::future::pending::<()>().await;
        }
        io.fail_response();
        Err(BridgeError::Transport("reset".into()).into())
    }
}

#[tokio::test(start_paused = true)]
async fn timeout_keeps_only_observed_prefix_and_undispatched_stays_unknown() {
    let request = || ChatFormat::new("test", vec![ChatMessage::user("input")]);
    let parent = Arc::new(ResponseHeaderTap::new(Vec::new(), false, "r".into()));
    let timeout = response_tap::scope(
        Arc::clone(&parent),
        call_with_optional_timeout(
            &FailingCaller,
            "timeout",
            &request,
            Some(Duration::from_secs(1)),
        ),
    )
    .await
    .unwrap_err();
    assert!(matches!(timeout.source, BridgeError::Timeout { .. }));
    let audit = timeout.io_audit.unwrap();
    assert_eq!(audit.exchange_count, Some(1));
    assert_eq!(audit.request_complete, Some(true));
    assert_eq!(audit.output_text, Some(text_measurement("prefix", false)));
    assert!(!audit.response_complete);
    assert!(audit.response_body_bytes.is_some_and(|bytes| bytes > 0));
    assert!(parent.capture().io.is_none());

    let refused = call_with_optional_timeout(&FailingCaller, "undispatched", &request, None)
        .await
        .unwrap_err();
    assert!(refused.io_audit.is_none());
}

#[tokio::test]
async fn failed_member_retains_its_audit_in_error_and_progress() {
    let config: EnsembleConfig = serde_json::from_value(serde_json::json!({
        "panel": [{"model": "failed"}, {"model": "undispatched"}],
        "judge": {"model": "judge"}
    }))
    .unwrap();
    let progress = EnsembleProgress::default();
    let request = ChatFormat::new("test", vec![ChatMessage::user("input")]);
    let error = run_ensemble(&request, &config, &FailingCaller, &progress)
        .await
        .unwrap_err();
    let EnsembleError::InsufficientPanel { panel, failed, .. } = error else {
        panic!("expected insufficient panel")
    };
    assert!(panel.is_empty());
    assert_eq!(failed.len(), 2);
    assert_eq!(failed[0].io_audit.as_ref().unwrap().exchange_count, Some(1));
    assert!(!failed[0].io_audit.as_ref().unwrap().response_complete);
    assert!(failed[1].io_audit.is_none());
    let (recorded, judge) = progress.take();
    assert!(judge.is_none());
    for (recorded, returned) in recorded.into_iter().zip(failed) {
        assert_eq!(recorded.unwrap_err().io_audit, returned.io_audit);
    }
}

struct CredentialCaller;

#[async_trait]
impl ModelCaller for CredentialCaller {
    async fn call(
        &self,
        target: &str,
        _req: &RequestBuilder<'_>,
    ) -> Result<ModelCallOutcome, CallError> {
        let scrubbed = aisix_gateway::upstream_error_scrub::scrub_upstream_error_text(
            "provider repeated opaque-ensemble-credential",
        );
        assert!(!scrubbed.contains("opaque-ensemble-credential"));
        Ok(outcome(target))
    }
}

#[tokio::test]
async fn isolated_subcall_inherits_credentials_for_error_scrubbing() {
    let parent = Arc::new(ResponseHeaderTap::new(Vec::new(), false, "r".into()));
    response_tap::scope(parent, async {
        response_tap::note_sent_credential("opaque-ensemble-credential");
        let request = || ChatFormat::new("test", vec![ChatMessage::user("input")]);
        let result = call_with_optional_timeout(&CredentialCaller, "model", &request, None)
            .await
            .unwrap();
        assert!(result.io_audit.is_none());
    })
    .await;
}
