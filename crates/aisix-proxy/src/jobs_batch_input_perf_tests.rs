//! Batch JSONL 的真实准入组件测试；不启动网络、数据库或提供方替身。
use super::*;
use aisix_core::native_objects::{NativeObjectKind, OriginBinding};
use aisix_core::{
    snapshot::SnapshotHandle, AisixSnapshot, ApiKey, Model, ProviderKey, ProxyConfig,
};
use aisix_response_reference_test_alloc::{AllocationScope, Allocations};
use serde_json::json;
use uuid::Uuid;

const MODEL: &str = "11111111-1111-4111-8111-111111111111";
const PROVIDER: &str = "22222222-2222-4222-8222-222222222222";
const KEY: &str = "33333333-3333-4333-8333-333333333333";

struct Fixture {
    state: ProxyState,
    auth: AuthenticatedKey,
    origin: OriginBinding,
}

fn fixture(guardrail: Option<Value>) -> Fixture {
    let snapshot = AisixSnapshot::new();
    let provider: ProviderKey = serde_json::from_value(json!({
        "display_name":"batch-copy-account", "provider":"openai", "adapter":"openai",
        "api_key":"local-batch-copy-test-key", "api_base":"http://127.0.0.1:9/v1"
    }))
    .unwrap();
    snapshot
        .provider_keys
        .insert(ResourceEntry::new(PROVIDER, provider, 1));
    let model: Model = serde_json::from_value(json!({
        "display_name":"batch-public", "provider":"openai", "model_name":"batch-upstream",
        "provider_key_id":PROVIDER
    }))
    .unwrap();
    snapshot.models.insert(ResourceEntry::new(MODEL, model, 1));
    let key: ApiKey = serde_json::from_value(json!({
        "key_hash":ApiKey::hash_bearer("local-batch-copy-caller"), "allowed_models":["*"]
    }))
    .unwrap();
    let entry = Arc::new(ResourceEntry::new(KEY, key, 1));
    snapshot.apikeys.insert((*entry).clone());
    let auth = AuthenticatedKey::new(&snapshot, entry, None, false);
    if let Some(guardrail) = guardrail {
        crate::seed_env_scoped_guardrail(
            &snapshot,
            ResourceEntry::new(
                "batch-copy-rule",
                serde_json::from_value(guardrail).unwrap(),
                1,
            ),
        );
    }
    let config: ProxyConfig = serde_json::from_value(json!({"addr":"127.0.0.1:0"})).unwrap();
    let state = ProxyState::new(
        SnapshotHandle::new(snapshot),
        Arc::new(aisix_gateway::Hub::new()),
        &config,
    )
    .with_platform(crate::platform::PlatformSettings::from_secret(
        "batch-copy-test",
    ));
    let (_, origin) = crate::native_api::openai::resolve(
        &state,
        &state.snapshot.load(),
        &auth,
        &ClientContext::default(),
        "batch-public",
        NativeObjectKind::File,
    )
    .unwrap();
    Fixture {
        state,
        auth,
        origin,
    }
}

// 只依赖旧返回值也实现的 AsRef<[u8]>，旧产品可先编译运行出真实 RED。
macro_rules! admit {
    ($fixture:expr, $bytes:expr, $admission:expr) => {
        managed_batch_input(
            &$fixture.state,
            &$fixture.state.snapshot.load(),
            &$fixture.auth,
            &ClientContext::default(),
            &$fixture.origin,
            $bytes,
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut String::new(),
            &mut Vec::new(),
            &mut crate::redact::RedactionCounts::default(),
            $admission,
        )
        .await
    };
}

#[tokio::test(flavor = "current_thread")]
async fn unchanged_batch_input_shares_the_original_buffer_for_upload_and_submit() {
    let fixture = fixture(None);
    for admission in [
        BatchInputAdmission::Upload,
        BatchInputAdmission::Submit(&[]),
    ] {
        for ending in ["", "\n", "\r\n"] {
            let input = format!(
                " \n {{\"url\":\"/v1/chat/completions\",\"custom_id\":\"one\",\"body\":{{\"model\":\"batch-upstream\",\"messages\":[{{\"role\":\"user\",\"content\":\"hello\"}}]}}}}{ending}"
            ).into_bytes();
            let (output, entries) = admit!(fixture, &input, admission).unwrap();
            let output: &[u8] = output.as_ref();
            assert_eq!(output, input, "无改写须保留字段顺序、空行及末尾换行");
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].entry_id, "one");
            assert_eq!(entries[0].endpoint, "/v1/chat/completions");
            assert_eq!(entries[0].model_id, Uuid::parse_str(MODEL).unwrap());
            assert_eq!(
                entries[0].requested_model_id,
                Uuid::parse_str(MODEL).unwrap()
            );
            assert_eq!(entries[0].requested_model, "batch-public");
            assert_eq!(entries[0].upstream_model, "batch-upstream");
            // 恢复 copy_from_slice 即使字节仍相等，也必须在这里失败。
            assert_eq!(output.as_ptr(), input.as_ptr(), "无改写不能复制整份 JSONL");
        }
    }
}

async fn allocation_probe(fixture: &Fixture, input: &[u8]) -> Allocations {
    let scope = AllocationScope::start();
    let result = admit!(fixture, input, BatchInputAdmission::Upload);
    let allocations = scope.finish();
    let (output, entries) = result.unwrap();
    let output: &[u8] = output.as_ref();
    assert_eq!(output, input);
    assert_eq!(entries.len(), 1);
    allocations
}

#[tokio::test(flavor = "current_thread")]
async fn unchanged_batch_input_does_not_allocate_an_output_file_buffer() {
    let fixture = fixture(None);
    let line = br#"{"custom_id":"one","url":"/v1/chat/completions","body":{"model":"batch-upstream","messages":[{"role":"user","content":"hello"}]}}"#;
    let small = [b" ".as_slice(), line, b"\n"].concat();
    let large = [vec![b' '; 1024 * 1024].as_slice(), line, b"\n"].concat();
    // 仅增加 JSON 前空白，不增加解码值；模型/规则初始化不在计数窗口。
    allocation_probe(&fixture, &small).await;
    allocation_probe(&fixture, &large).await;
    let small_allocations = allocation_probe(&fixture, &small).await;
    let large_allocations = allocation_probe(&fixture, &large).await;
    assert!(small_allocations.calls > 0, "分配计数器必须实际启用");
    assert!(
        large_allocations.bytes <= small_allocations.bytes.saturating_add(4096),
        "只有原始空白增长，不能再分配文件大小的输出：small={small_allocations:?}, large={large_allocations:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn submit_rewrites_only_the_changed_line_and_preserves_surrounding_bytes() {
    let fixture = fixture(None);
    let before = b" \r\n{\"url\":\"/v1/chat/completions\",\"custom_id\":\"before\",\"body\":{\"messages\":[],\"model\":\"batch-upstream\"}}\r\n";
    let middle = b"{\"custom_id\":\"middle\",\"url\":\"/v1/chat/completions\",\"body\":{\"model\":\"batch-public\",\"messages\":[]}}\r\n";
    let after = b"\n {\"url\":\"/v1/chat/completions\",\"custom_id\":\"after\",\"body\":{\"messages\":[],\"model\":\"batch-upstream\"}}";
    let input = [before.as_slice(), middle, after].concat();
    let (output, entries) = admit!(fixture, &input, BatchInputAdmission::Submit(&[])).unwrap();
    let output: &[u8] = output.as_ref();
    assert!(output.starts_with(before));
    assert!(output.ends_with(after));
    assert_ne!(output.as_ptr(), input.as_ptr());
    let rewritten = &output[before.len()..output.len() - after.len()];
    assert!(rewritten.ends_with(b"\r\n"));
    let value: Value = serde_json::from_slice(rewritten).unwrap();
    assert_eq!(value["custom_id"], "middle");
    assert_eq!(value["body"]["model"], "batch-upstream");
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.entry_id.as_str())
            .collect::<Vec<_>>(),
        ["before", "middle", "after"]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn unchanged_lines_still_run_the_input_guardrail_and_report_the_verdict() {
    let fixture = fixture(Some(json!({
        "name":"block-last-line", "enabled":true, "hook_point":"input", "kind":"keyword",
        "patterns":[{"kind":"literal","value":"forbidden-last-line"}]
    })));
    let input = b"{\"custom_id\":\"first\",\"url\":\"/v1/chat/completions\",\"body\":{\"model\":\"batch-upstream\",\"messages\":[]}}\n{\"custom_id\":\"last\",\"url\":\"/v1/chat/completions\",\"body\":{\"model\":\"batch-upstream\",\"messages\":[{\"role\":\"user\",\"content\":\"forbidden-last-line\"}]}}\n";
    let mut enforced = Vec::new();
    let mut applied = Vec::new();
    let result = managed_batch_input(
        &fixture.state,
        &fixture.state.snapshot.load(),
        &fixture.auth,
        &ClientContext::default(),
        &fixture.origin,
        input,
        &mut Vec::new(),
        &mut enforced,
        &mut Vec::new(),
        &mut String::new(),
        &mut applied,
        &mut crate::redact::RedactionCounts::default(),
        BatchInputAdmission::Upload,
    )
    .await;
    assert!(matches!(result, Err(ProxyError::ContentFiltered { .. })));
    assert!(!enforced.is_empty(), "拒绝仍须保留原有逐项审计");
    assert!(!applied.is_empty(), "优化不能跳过已附着的规则");
}

#[tokio::test(flavor = "current_thread")]
async fn malformed_later_line_is_rejected_after_an_earlier_model_rewrite() {
    let fixture = fixture(None);
    let input = b"{\"custom_id\":\"first\",\"url\":\"/v1/chat/completions\",\"body\":{\"model\":\"batch-public\",\"messages\":[]}}\n{\"custom_id\":\"last\",\"url\":\"/v1/chat/completions\",\"body\":{\"messages\":[]}}\n";
    assert!(matches!(
        admit!(fixture, input, BatchInputAdmission::Submit(&[])),
        Err(ProxyError::InvalidRequest(_))
    ));
}
