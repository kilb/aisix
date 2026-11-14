use aisix_core::native_objects::BeginRelatedRequest;
use serde_json::{json, Value};

fn submission() -> Value {
    json!({
        "operation_id":"00000000-0000-4000-8000-000000000001",
        "api_key_id":"00000000-0000-4000-8000-000000000002",
        "protocol":"openai", "kind":"message_batch",
        "origin":{
            "model_id":"00000000-0000-4000-8000-000000000003",
            "provider_key_id":"00000000-0000-4000-8000-000000000004",
            "boundary":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        },
        "requested_model_id":"00000000-0000-4000-8000-000000000003",
        "requested_model":"visible-model", "upstream_model":"upstream-model",
        "operation":"batches", "request_id":"entry-reference-contract",
        "asynchronous":true,
        "entries":[
            {"entry_id":"one", "endpoint":"/v1/responses", "requested_model":"visible-model", "requested_model_id":"00000000-0000-4000-8000-000000000003", "model_id":"00000000-0000-4000-8000-000000000003", "upstream_model":"upstream-model"},
            {"entry_id":"two", "endpoint":"/v1/responses", "requested_model":"visible-model", "requested_model_id":"00000000-0000-4000-8000-000000000003", "model_id":"00000000-0000-4000-8000-000000000003", "upstream_model":"upstream-model"}
        ]
    })
}

// 去掉逐条 manifest 字段，两个不同 entry 的引用都会丢失，此断言必须变红。
#[test]
fn related_operation_preserves_entry_specific_reference_manifests() {
    let references = json!({
        "one":[{"record_id":"00000000-0000-4000-8000-000000000005", "consumer_family":"responses"}],
        "two":[{"record_id":"00000000-0000-4000-8000-000000000006", "consumer_family":"responses"}]
    });
    let request: BeginRelatedRequest = serde_json::from_value(json!({
        "submission":submission(), "references":[], "entry_references":references
    }))
    .expect("the accepted per-entry JSON request decodes");
    let encoded = serde_json::to_value(request).expect("the request serializes");
    assert_eq!(encoded.get("entry_references"), Some(&references));
}

#[test]
fn older_related_requests_default_to_an_empty_entry_reference_map() {
    let request: BeginRelatedRequest = serde_json::from_value(json!({
        "submission":submission(), "references":[]
    }))
    .expect("older requests remain readable");
    let encoded = serde_json::to_value(request).expect("the request serializes");
    assert_eq!(encoded.get("entry_references"), Some(&json!({})));
}

// 不能把 manifest 当未知 JSON 静默丢弃；其中的对象身份仍使用已有 UUID 合同。
#[test]
fn an_entry_reference_with_an_invalid_object_identity_is_rejected() {
    let request = serde_json::from_value::<BeginRelatedRequest>(json!({
        "submission":submission(), "references":[],
        "entry_references":{"one":[{"record_id":"not-an-object-id", "consumer_family":"responses"}]}
    }));
    assert!(
        request.is_err(),
        "an invalid reference UUID must not be ignored"
    );
}
