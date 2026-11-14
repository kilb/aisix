//! 组件测试：真实注册器调用边界与 SSE 字节释放顺序；不宣称上游端到端覆盖。

use super::super::registry::{NativeObjectRegistry, NativeRegistryError};
use super::*;
use aisix_core::native_objects::*;
use futures::future::BoxFuture;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Registry {
    calls: AtomicUsize,
    fail: bool,
    origin: OriginBinding,
    requests: std::sync::Mutex<Vec<BindRequest>>,
    fail_children: std::sync::atomic::AtomicBool,
}

macro_rules! unused {
    ($name:ident, $request:ty) => {
        fn $name(&self, _: $request) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
            Box::pin(async { Err(NativeRegistryError::Unavailable) })
        }
    };
}

impl NativeObjectRegistry for Registry {
    unused!(begin, BeginRequest);
    unused!(lookup, LookupRequest);
    unused!(owned, OwnedRequest);
    unused!(tombstone, TombstoneRequest);
    unused!(claim, ClaimRequest);
    unused!(renew, LeaseRequest);
    unused!(reschedule, LeaseRequest);
    unused!(complete, CompleteRequest);

    fn bind(
        &self,
        request: BindRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.requests.lock().unwrap().push(request.clone());
        Box::pin(async move {
            if self.fail
                || (!request.objects.is_empty() && self.fail_children.load(Ordering::SeqCst))
            {
                return Err(NativeRegistryError::Unavailable);
            }
            let record: NativeRecord = serde_json::from_value(serde_json::json!({
                "record_id": request.operation_id, "operation_id": request.operation_id,
                "origin": self.origin, "protocol": "openai", "kind": "response",
                "raw_id": request.raw_id, "version": 1
            }))
            .unwrap();
            let mut reply = NativeReply::new("bound");
            reply.records = request.objects.iter().map(|object| serde_json::from_value(serde_json::json!({
                "record_id":Uuid::new_v4(), "parent_id":record.record_id,
                "origin":self.origin, "protocol":"openai", "kind":object.kind, "raw_id":object.raw_id
            })).unwrap()).collect();
            reply.record = Some(record);
            Ok(reply)
        })
    }
}

fn registration(fail: bool) -> (OriginRegistration, Arc<Registry>) {
    let origin = OriginBinding {
        model_id: Uuid::new_v4(),
        provider_key_id: Uuid::new_v4(),
        boundary: native_origin_digest(
            &[7; 32],
            Uuid::nil(),
            NativeProtocol::Openai,
            "openai",
            "https://api.example.test/v1",
            "test-key",
            &Default::default(),
        ),
    };
    let registry = Arc::new(Registry {
        calls: AtomicUsize::new(0),
        fail,
        origin: origin.clone(),
        requests: std::sync::Mutex::new(Vec::new()),
        fail_children: std::sync::atomic::AtomicBool::new(false),
    });
    (
        OriginRegistration {
            registry: registry.clone(),
            operation_id: Uuid::new_v4(),
            api_key_id: Uuid::new_v4(),
            existing_record_id: None,
            origin,
            bound_id: None,
            record: None,
            bound_objects: BTreeSet::new(),
        },
        registry,
    )
}

#[tokio::test]
async fn bind_precedes_first_response_id_and_repeated_frames_bind_once() {
    let (registration, registry) = registration(false);
    let frames = b"event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_owned\"}}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_owned\"}}\n\n";
    let upstream = futures::stream::iter(
        frames
            .chunks(7)
            .map(|b| Ok(bytes::Bytes::copy_from_slice(b)))
            .collect::<Vec<_>>(),
    );
    let mut stream = register_stream(Some(registration), Box::pin(upstream));
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap();
        assert_eq!(
            registry.calls.load(Ordering::SeqCst),
            1,
            "no ID bytes before binding"
        );
        bytes.extend_from_slice(&chunk);
    }
    assert_eq!(bytes, frames);
    assert_eq!(registry.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn failed_binding_releases_no_id_bytes() {
    let (registration, registry) = registration(true);
    let upstream = futures::stream::once(async {
        Ok(bytes::Bytes::from_static(
            b"data: {\"response\":{\"id\":\"resp_owned\"}}\n\n",
        ))
    });
    let mut stream = register_stream(Some(registration), Box::pin(upstream));
    assert!(matches!(stream.next().await, Some(Err(_))));
    assert!(stream.next().await.is_none());
    assert_eq!(registry.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn compat_native_origin_observes_terminal_usage_before_failed_bind_without_releasing_frame() {
    let (registration, registry) = registration(true);
    let terminal = serde_json::json!({
        "type":"response.completed",
        "response":{"id":"resp_owned","status":"completed","output":[],
            "usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10}}
    });
    let wire = format!("event: response.completed\ndata: {terminal}\n\n");
    let upstream = futures::stream::once(async move { Ok(Bytes::from(wire)) });
    let observed = Arc::new(std::sync::Mutex::new(None));
    let observed_c = Arc::clone(&observed);
    let registry_c = Arc::clone(&registry);
    let mut stream =
        register_stream_observed(Some(registration), Box::pin(upstream), move |event| {
            assert_eq!(
                registry_c.calls.load(Ordering::SeqCst),
                0,
                "observe before local await"
            );
            *observed_c.lock().unwrap() = crate::responses::responses_terminal_usage_stats(event);
        });
    assert!(matches!(
        stream.next().await,
        Some(Err(RawStreamError::DependencyUnavailable))
    ));
    assert!(stream.next().await.is_none());
    let usage = observed
        .lock()
        .unwrap()
        .clone()
        .expect("the terminal upstream usage survives a failed local bind");
    assert_eq!(
        (
            usage.prompt_tokens,
            usage.completion_tokens,
            usage.upstream_total_tokens
        ),
        (7, 3, 10)
    );
    assert_eq!(registry.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn compat_native_origin_observer_rejects_missing_or_changed_response_identity() {
    for raw_id in [None, Some("resp_foreign")] {
        let (mut registration, registry) = registration(false);
        registration
            .bind_response(&serde_json::json!({"id":"resp_owned"}))
            .await
            .unwrap();
        let mut response = serde_json::json!({
            "status":"completed","usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10}
        });
        if let Some(raw_id) = raw_id {
            response["id"] = raw_id.into();
        }
        let terminal = serde_json::json!({"type":"response.completed","response":response});
        let wire = format!("data: {terminal}\n\n");
        let upstream = futures::stream::once(async move { Ok(Bytes::from(wire)) });
        let observations = Arc::new(AtomicUsize::new(0));
        let observations_c = Arc::clone(&observations);
        let mut stream =
            register_stream_observed(Some(registration), Box::pin(upstream), move |_| {
                observations_c.fetch_add(1, Ordering::SeqCst);
            });
        assert!(matches!(
            stream.next().await,
            Some(Err(RawStreamError::UpstreamDecode))
        ));
        assert!(stream.next().await.is_none());
        assert_eq!(
            observations.load(Ordering::SeqCst),
            0,
            "invalid identity cannot contribute usage"
        );
        assert_eq!(registry.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn json_binding_failure_is_not_a_successful_response() {
    let (mut registration, registry) = registration(true);
    assert!(registration
        .bind_response(&serde_json::json!({"id":"resp_owned"}))
        .await
        .is_err());
    assert_eq!(registry.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn one_submission_never_binds_two_response_ids() {
    let (mut registration, registry) = registration(false);
    registration
        .bind_response(&serde_json::json!({"id":"resp_one"}))
        .await
        .unwrap();
    assert!(registration
        .bind_response(&serde_json::json!({"id":"resp_two"}))
        .await
        .is_err());
    assert_eq!(registry.calls.load(Ordering::SeqCst), 1);
}

// 官方完成事件可包含完整图像结果；此处只验证组件字节与计量，不调用模型。
fn large_image_response_frames() -> (Vec<u8>, Value) {
    use base64::Engine;

    let svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"1\" height=\"1\"><!--{}--><path d=\"M0 0h1v1H0z\"/></svg>",
        "x".repeat(1024 * 1024),
    );
    let image = base64::engine::general_purpose::STANDARD.encode(svg);
    let completed = serde_json::json!({
        "type": "response.completed", "sequence_number": 1,
        "response": {
            "id": "resp_large_image", "object": "response",
            "created_at": 1_700_000_000, "model": "gpt-6-astra",
            "status": "completed", "parallel_tool_calls": true,
            "tool_choice": "auto", "tools": [{"type": "image_generation"}],
            "output": [{
                "id": "ig_large_result", "type": "image_generation_call",
                "status": "completed", "result": image, "output_format": "svg"
            }],
            "usage": {
                "input_tokens": 31, "output_tokens": 17, "total_tokens": 48,
                "input_tokens_details": {"cached_tokens": 3, "cache_write_tokens": 0},
                "output_tokens_details": {"reasoning_tokens": 2}
            }
        }
    });
    let mut created_response = completed["response"].clone();
    created_response["status"] = serde_json::json!("in_progress");
    created_response["output"] = serde_json::json!([]);
    created_response["usage"] = Value::Null;
    let created = serde_json::json!({
        "type": "response.created", "sequence_number": 0,
        "response": created_response
    });
    let terminal = format!("event: response.completed\ndata: {completed}\n\n");
    assert!(terminal.len() > 1024 * 1024);
    assert!(terminal.len() < aisix_gateway::sse::MAX_SSE_FRAME_BYTES);
    (
        format!("event: response.created\ndata: {created}\n\n: keepalive\n\n{terminal}")
            .into_bytes(),
        completed,
    )
}

async fn assert_large_image_stream(chunk_size: usize) {
    let (registration, registry) = registration(false);
    let (expected, terminal) = large_image_response_frames();
    let chunks: Vec<_> = expected
        .chunks(chunk_size)
        .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
        .collect();
    let mut stream = register_stream(Some(registration), Box::pin(futures::stream::iter(chunks)));
    let mut actual = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("a documented image completion below the shared SSE limit");
        let expected_calls = if chunk
            .windows(b"ig_large_result".len())
            .any(|part| part == b"ig_large_result")
        {
            2
        } else {
            1
        };
        assert_eq!(
            registry.calls.load(Ordering::SeqCst),
            expected_calls,
            "each visible object must already be bound"
        );
        actual.extend_from_slice(&chunk);
    }
    assert_eq!(
        actual, expected,
        "image result, comments and terminal usage stay byte-identical"
    );
    assert_eq!(registry.calls.load(Ordering::SeqCst), 2);
    let mut decoder = aisix_gateway::sse::SseDecoder::default();
    let events = decoder.feed(actual.as_slice()).unwrap();
    let completed = events
        .into_iter()
        .filter_map(|event| match event {
            aisix_gateway::sse::SseEvent::Data(data) => serde_json::from_str::<Value>(&data).ok(),
            aisix_gateway::sse::SseEvent::Done => None,
        })
        .find(|event| event.get("type").and_then(Value::as_str) == Some("response.completed"))
        .expect("the original terminal event survives the ownership wrapper");
    assert_eq!(completed, terminal);
    let usage = crate::responses::responses_terminal_usage_stats(&completed)
        .expect("terminal provider usage must reach the existing observer");
    assert_eq!(
        (
            usage.prompt_tokens,
            usage.completion_tokens,
            usage.upstream_total_tokens
        ),
        (31, 17, 48)
    );
    assert_eq!(usage.cached_prompt_tokens, 3);
    assert_eq!(usage.reasoning_tokens, 2);
}

#[tokio::test]
async fn compat_native_origin_sse_large_image_coalesced_preserves_terminal_usage() {
    assert_large_image_stream(usize::MAX).await;
}

#[tokio::test]
async fn compat_native_origin_sse_large_image_fragmented_preserves_terminal_usage() {
    assert_large_image_stream(7 * 1024).await;
}

#[tokio::test]
async fn compat_native_origin_sse_conflicting_response_id_releases_no_foreign_frame() {
    let (registration, registry) = registration(false);
    let created = b"event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_owned\"}}\n\n";
    let foreign = b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_foreign\"}}\n\n";
    let chunks = vec![
        Ok(Bytes::copy_from_slice(created)),
        Ok(Bytes::copy_from_slice(foreign)),
    ];
    let mut stream = register_stream(Some(registration), Box::pin(futures::stream::iter(chunks)));
    assert_eq!(stream.next().await.unwrap().unwrap().as_ref(), created);
    assert!(matches!(
        stream.next().await,
        Some(Err(RawStreamError::UpstreamDecode))
    ));
    assert!(stream.next().await.is_none());
    assert_eq!(registry.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn compat_native_origin_sse_missing_response_id_releases_no_frame() {
    let (registration, registry) = registration(false);
    let upstream = futures::stream::once(async {
        Ok(Bytes::from_static(
            b"event: response.created\ndata: {\"type\":\"response.created\",\"response\":{}}\n\n",
        ))
    });
    let mut stream = register_stream(Some(registration), Box::pin(upstream));
    assert!(matches!(
        stream.next().await,
        Some(Err(RawStreamError::UpstreamDecode))
    ));
    assert!(stream.next().await.is_none());
    assert_eq!(registry.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn raw_ids_and_children_bind_before_json_and_each_stream_event() {
    let (mut json_registration, registry) = registration(false);
    let response = serde_json::json!({"id":"resp_raw_exact","output":[
        {"id":"fc_raw_exact","type":"function_call","call_id":"toolu_raw_exact","name":"f","arguments":"{}"},
        {"id":"ci_raw_exact","type":"code_interpreter_call","container_id":"cntr_raw_exact"}
    ]});
    json_registration.bind_response(&response).await.unwrap();
    assert_eq!(registry.calls.load(Ordering::SeqCst), 1);
    {
        let requests = registry.requests.lock().unwrap();
        assert_eq!(requests[0].raw_id, "resp_raw_exact");
        assert!(requests[0]
            .objects
            .iter()
            .any(|object| object.kind == NativeObjectKind::ResponseItem
                && object.raw_id == "fc_raw_exact"));
        assert!(requests[0]
            .objects
            .iter()
            .any(|object| object.kind == NativeObjectKind::ResponseContainer
                && object.raw_id == "cntr_raw_exact"));
        assert!(
            !requests[0]
                .objects
                .iter()
                .any(|object| object.raw_id == "toolu_raw_exact"),
            "inline call IDs require no global-object ownership"
        );
    }

    let (registration, registry) = registration(false);
    let frames = [
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_raw_exact\"}}\n\n",
        "data: {\"type\":\"response.output_item.added\",\"response_id\":\"resp_raw_exact\",\"item\":{\"id\":\"fc_raw_exact\",\"type\":\"function_call\",\"call_id\":\"toolu_raw_exact\"}}\n\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"response_id\":\"resp_raw_exact\",\"item_id\":\"fc_raw_exact\",\"delta\":\"{}\"}\n\n",
    ];
    let upstream = futures::stream::iter(
        frames
            .iter()
            .map(|frame| Ok(Bytes::copy_from_slice(frame.as_bytes())))
            .collect::<Vec<_>>(),
    );
    let mut stream = register_stream(Some(registration), Box::pin(upstream));
    for (index, frame) in frames.iter().enumerate() {
        assert_eq!(
            stream.next().await.unwrap().unwrap().as_ref(),
            frame.as_bytes()
        );
        assert_eq!(
            registry.calls.load(Ordering::SeqCst),
            if index == 0 { 1 } else { 2 }
        );
    }
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn failed_child_binding_releases_no_raw_item_frame() {
    let (registration, registry) = registration(false);
    registry.fail_children.store(true, Ordering::SeqCst);
    let frames = [
        Ok(Bytes::from_static(b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_owned\"}}\n\n")),
        Ok(Bytes::from_static(b"data: {\"type\":\"response.output_item.added\",\"item\":{\"id\":\"msg_owned\",\"type\":\"message\"}}\n\n")),
    ];
    let mut stream = register_stream(Some(registration), Box::pin(futures::stream::iter(frames)));
    let created = stream.next().await.unwrap().unwrap();
    assert!(!String::from_utf8_lossy(&created).contains("msg_owned"));
    assert!(matches!(
        stream.next().await,
        Some(Err(RawStreamError::DependencyUnavailable))
    ));
    assert!(stream.next().await.is_none());
    assert_eq!(registry.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn related_response_id_cannot_cross_a_bound_response() {
    let (mut registration, registry) = registration(false);
    registration
        .bind_response(&serde_json::json!({"id":"resp_owned"}))
        .await
        .unwrap();
    let upstream = futures::stream::once(async {
        Ok(Bytes::from_static(
        b"data: {\"type\":\"response.output_text.delta\",\"response_id\":\"resp_foreign\",\"item_id\":\"msg_foreign\",\"delta\":\"foreign\"}\n\n"
    ))
    });
    let mut stream = register_stream(Some(registration), Box::pin(upstream));
    assert!(matches!(
        stream.next().await,
        Some(Err(RawStreamError::UpstreamDecode))
    ));
    assert!(stream.next().await.is_none());
    assert_eq!(registry.calls.load(Ordering::SeqCst), 1);
}
