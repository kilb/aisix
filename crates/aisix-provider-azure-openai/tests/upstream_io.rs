use std::sync::Arc;

use aisix_core::usage_io::{TextMeasurement, UsageIoAudit};
use aisix_gateway::{
    response_tap, Bridge, BridgeContext, ChatFormat, ChatMessage, EmbeddingRequest,
    ResponseHeaderTap,
};
use aisix_provider_azure_openai::AzureOpenAiBridge;
use futures::TryStreamExt;
use serde_json::{json, Value};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn context(base: &str, completion_defaults: bool) -> BridgeContext {
    let model = serde_json::from_value(json!({
        "display_name":"io-audit-azure", "provider":"openai",
        "model_name":"azure-deployment", "provider_key_id":"pk-io-audit"
    }))
    .unwrap();
    let mut key = json!({
        "display_name":"io-audit-key", "api_key":"synthetic-key", "api_base":base
    });
    if completion_defaults {
        key["request"] = json!({"default_body_fields":{"temperature":0.25}});
    }
    BridgeContext::new(
        "req-io-audit-azure",
        Arc::new(model),
        Arc::new(serde_json::from_value(key).unwrap()),
    )
}

fn tap() -> Arc<ResponseHeaderTap> {
    Arc::new(ResponseHeaderTap::new(
        vec!["x-request-id".into()],
        false,
        "req-io-audit-azure".into(),
    ))
}

async fn assert_exchange(
    tap: &ResponseHeaderTap,
    server: &MockServer,
    response: &[u8],
    input_bytes: u64,
    input_scalars: u64,
) -> UsageIoAudit {
    let received = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let capture = tap.capture();
    assert_eq!(capture.request_id, "azure-upstream-id");
    let audit = capture
        .io
        .expect("actual Azure send must be observed")
        .snapshot();
    assert_eq!(audit.version, 2);
    assert_eq!(audit.boundary, "upstream_attempt");
    assert_eq!(audit.exchange_count, Some(1));
    assert_eq!(audit.request_complete, Some(true));
    assert_eq!(
        audit.request_body_bytes,
        Some(received[0].body.len() as u64)
    );
    assert_eq!(audit.response_body_bytes, Some(response.len() as u64));
    assert!(audit.response_complete);
    assert_eq!(
        audit.input_text,
        Some(TextMeasurement {
            utf8_bytes: input_bytes,
            unicode_scalars: input_scalars,
            complete: true,
        })
    );
    audit
}

async fn upstream(body: &[u8], stream: bool) -> MockServer {
    let server = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-request-id", "azure-upstream-id")
                .insert_header(
                    "content-type",
                    if stream {
                        "text/event-stream"
                    } else {
                        "application/json"
                    },
                )
                .set_body_bytes(body),
        )
        .mount(&server)
        .await;
    server
}

fn assert_output(audit: &UsageIoAudit) {
    assert_eq!(
        audit.output_text,
        Some(TextMeasurement {
            utf8_bytes: 8,
            unicode_scalars: 3,
            complete: true,
        })
    );
}

#[tokio::test]
async fn upstream_io_azure_chat_observes_both_wire_modes() {
    for stream in [false, true] {
        let body = if stream {
            format!(
                "data: {}\n\ndata: [DONE]\n\n",
                json!({"id":"chat-azure","object":"chat.completion.chunk","model":"azure-deployment",
                    "choices":[{"index":0,"delta":{"content":"A汉😀"},"finish_reason":"stop"}],
                    "usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}})
            )
            .into_bytes()
        } else {
            serde_json::to_vec(&json!({"id":"chat-azure","object":"chat.completion","model":"azure-deployment",
                "choices":[{"index":0,"message":{"role":"assistant","content":"A汉😀"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}})).unwrap()
        };
        let server = upstream(&body, stream).await;
        let ctx = context(&server.uri(), false);
        let req = ChatFormat::new("caller-alias", vec![ChatMessage::user("A汉😀")]);
        let bridge = AzureOpenAiBridge::new();
        let tap = tap();
        response_tap::scope(tap.clone(), async {
            if stream {
                let chunks: Vec<_> = bridge
                    .chat_stream(&req, &ctx)
                    .await
                    .unwrap()
                    .try_collect()
                    .await
                    .unwrap();
                assert!(!chunks.is_empty());
            } else {
                let answer = bridge.chat(&req, &ctx).await.unwrap();
                assert_eq!(answer.message.content_str(), "A汉😀");
                assert_eq!(answer.usage.upstream_total_tokens, 5);
            }
        })
        .await;
        assert_output(&assert_exchange(&tap, &server, &body, 8, 3).await);
    }
}

#[tokio::test]
async fn upstream_io_azure_completion_counts_final_override_in_both_wire_modes() {
    for stream in [false, true] {
        let wire: Value = json!({"id":"completion-azure","object":"text_completion","model":"azure-deployment",
            "choices":[{"index":0,"text":"A汉😀","finish_reason":"stop"}],
            "usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}});
        let body = if stream {
            format!("data: {wire}\n\ndata: [DONE]\n\n").into_bytes()
        } else {
            serde_json::to_vec(&wire).unwrap()
        };
        let server = upstream(&body, stream).await;
        let ctx = context(&server.uri(), true);
        let bridge = AzureOpenAiBridge::new();
        let tap = tap();
        response_tap::scope(tap.clone(), async {
            let request =
                json!({"model":"caller-alias","prompt":"x","suffix":"A汉😀","stream":stream});
            if stream {
                let chunks: Vec<_> = bridge
                    .complete_stream(&request, &ctx)
                    .await
                    .unwrap()
                    .try_collect()
                    .await
                    .unwrap();
                assert!(!chunks.is_empty());
            } else {
                assert_eq!(
                    bridge.complete(&request, &ctx).await.unwrap()["choices"][0]["text"],
                    "A汉😀"
                );
            }
        })
        .await;
        let audit = assert_exchange(&tap, &server, &body, 9, 4).await;
        assert_output(&audit);
        let received = server.received_requests().await.unwrap();
        let sent: Value = serde_json::from_slice(&received[0].body).unwrap();
        assert_eq!(sent["suffix"], "A汉😀");
        assert_eq!(sent["temperature"], json!(0.25));
        assert_eq!(sent["model"], "azure-deployment");
    }
}

#[tokio::test]
async fn upstream_io_azure_embeddings_and_images_keep_binary_outputs_out_of_text() {
    for image in [false, true] {
        let wire = if image {
            json!({"created":0,"data":[{"url":"https://example.invalid/generated.png"}]})
        } else {
            json!({"object":"list","model":"azure-deployment","data":[{"object":"embedding","index":0,"embedding":[0.1,0.2]}],"usage":{"prompt_tokens":3,"total_tokens":3}})
        };
        let body = serde_json::to_vec(&wire).unwrap();
        let server = upstream(&body, false).await;
        let ctx = context(&server.uri(), false);
        let bridge = AzureOpenAiBridge::new();
        let tap = tap();
        response_tap::scope(tap.clone(), async {
            if image {
                assert_eq!(
                    bridge
                        .generate_image(&json!({"prompt":"A汉😀"}), &ctx)
                        .await
                        .unwrap(),
                    wire
                );
            } else {
                let request = EmbeddingRequest {
                    model: "caller-alias".into(),
                    input: vec!["A汉😀".into()],
                    token_input: None,
                    input_was_single: true,
                    encoding_format: None,
                    dimensions: None,
                    input_type: None,
                    embedding_types: None,
                    extra: Default::default(),
                };
                assert_eq!(
                    bridge
                        .embed(&request, &ctx)
                        .await
                        .unwrap()
                        .usage
                        .prompt_tokens,
                    3
                );
            }
        })
        .await;
        let audit = assert_exchange(&tap, &server, &body, 8, 3).await;
        if image {
            assert!(audit.output_text.is_none());
        } else {
            assert_eq!(
                audit.output_text,
                Some(TextMeasurement {
                    utf8_bytes: 0,
                    unicode_scalars: 0,
                    complete: true,
                })
            );
        }
    }
}
