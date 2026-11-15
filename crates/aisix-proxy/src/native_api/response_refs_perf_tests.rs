//! 普通请求的局部分配与协议等价回归；计时不是供应商 E2E 或 TTFT 证据。
use super::*;
use crate::native_api::registry::{NativeObjectRegistry, NativeRegistryError};
use aisix_core::native_objects::*;
use aisix_core::{snapshot::SnapshotHandle, AisixSnapshot, ApiKey, ProxyConfig, ResourceEntry};
use futures::future::BoxFuture;
use serde_json::json;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Clone, Copy, Debug, Default)]
struct Allocations {
    calls: usize,
    bytes: usize,
}

thread_local! {
    static TRACKING: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<Allocations> = const { Cell::new(Allocations { calls: 0, bytes: 0 }) };
}

struct TrackedAllocator;

fn allocated(bytes: usize) {
    let _ = TRACKING.try_with(|tracking| {
        if tracking.get() {
            let _ = ALLOCATIONS.try_with(|counter| {
                let mut value = counter.get();
                value.calls = value.calls.saturating_add(1);
                value.bytes = value.bytes.saturating_add(bytes);
                counter.set(value);
            });
        }
    });
}

// 只在此 crate 的单元测试二进制中计数；关闭的其他线程仍使用原系统分配器。
unsafe impl GlobalAlloc for TrackedAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        allocated(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        allocated(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        allocated(size);
        unsafe { System.realloc(pointer, layout, size) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: TrackedAllocator = TrackedAllocator;

struct AllocationScope;

impl AllocationScope {
    fn start() -> Self {
        TRACKING.with(|enabled| assert!(!enabled.replace(true)));
        ALLOCATIONS.with(|counter| counter.set(Allocations::default()));
        Self
    }

    fn finish(self) -> Allocations {
        drop(self);
        ALLOCATIONS.with(Cell::get)
    }
}

impl Drop for AllocationScope {
    fn drop(&mut self) {
        TRACKING.with(|enabled| enabled.set(false));
    }
}

#[derive(Default)]
struct NoAuthorityCalls {
    calls: AtomicUsize,
    lookups: Mutex<Vec<ObjectLocator>>,
}

macro_rules! refuse_call {
    ($method:ident, $body:ty) => {
        fn $method(&self, _: $body) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err(NativeRegistryError::Unavailable) })
        }
    };
}

impl NativeObjectRegistry for NoAuthorityCalls {
    refuse_call!(begin, BeginRequest);
    refuse_call!(bind, BindRequest);
    refuse_call!(owned, OwnedRequest);
    refuse_call!(tombstone, TombstoneRequest);
    refuse_call!(claim, ClaimRequest);
    refuse_call!(renew, LeaseRequest);
    refuse_call!(reschedule, LeaseRequest);
    refuse_call!(complete, CompleteRequest);

    fn lookup(
        &self,
        request: LookupRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.lookups.lock().unwrap().push(request.locator);
        Box::pin(async { Err(NativeRegistryError::Unavailable) })
    }
}

fn fixture(native: bool, platform: bool) -> (ProxyState, AuthenticatedKey, Arc<NoAuthorityCalls>) {
    let snapshot = AisixSnapshot::new();
    let key: ApiKey = serde_json::from_value(json!({
        "key_hash":ApiKey::hash_bearer("local-response-reference-probe"),"allowed_models":["*"]
    }))
    .unwrap();
    let auth = AuthenticatedKey::new(
        &snapshot,
        Arc::new(ResourceEntry::new(
            "11111111-1111-4111-8111-111111111111",
            key,
            1,
        )),
        None,
        false,
    );
    let config: ProxyConfig = serde_json::from_value(json!({"addr":"127.0.0.1:0"})).unwrap();
    let mut state = ProxyState::new(
        SnapshotHandle::new(snapshot),
        Arc::new(aisix_gateway::Hub::new()),
        &config,
    );
    let registry = Arc::new(NoAuthorityCalls::default());
    if native {
        state = state.with_native_objects(registry.clone());
    }
    if platform {
        state = state.with_platform(crate::platform::PlatformSettings::from_secret(
            "response-reference-allocation-probe",
        ));
    }
    (state, auth, registry)
}

fn ordinary_body(kind: &str, size: usize) -> Value {
    let content = "x".repeat(size);
    match kind {
        "text" => json!({"model":"public-model","input":content,"store":false}),
        "schema" => json!({"model":"public-model","input":"hello","tools":[{
            "type":"function","name":"inspect","parameters":{"type":"object","properties":{
                "file_id":{"type":"string","description":content},
                "conversation":{"type":"object","default":{"type":"item_reference","id":"opaque-data"}}
            }}
        }],"metadata":{"file_id":"plain-user-data"}}),
        "inline_image" => json!({"model":"public-model","input":[{"role":"user","content":[
            {"type":"input_text","text":"describe"},
            {"type":"input_image","image_url":format!("data:image/png;base64,{content}")}
        ]}]}),
        _ => panic!("unknown fixed probe shape"),
    }
}

async fn probe(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    body: &Value,
    dispatch: bool,
) -> Allocations {
    let client = ClientContext::default();
    let mut affinity = crate::responses_affinity::ContextAffinity::default();
    let snapshot = state.snapshot.load();
    let scope = AllocationScope::start();
    let result = if dispatch {
        crate::native_api::openai_uploads::admit_responses_files(
            state, &snapshot, auth, &client, body, false, None,
        )
        .await
    } else {
        admit_affinity(state, auth, &client, body, &mut affinity).await
    };
    let allocations = scope.finish();
    assert!(
        result.is_ok(),
        "ordinary no-reference request must stay usable"
    );
    allocations
}

#[tokio::test(flavor = "current_thread")]
async fn ordinary_reference_admission_preserves_payload_and_never_uses_authority() {
    for native in [false, true] {
        for platform in [false, true] {
            let (state, auth, registry) = fixture(native, platform);
            for kind in ["text", "schema", "inline_image"] {
                for size in [128, 8 * 1024, 1024 * 1024] {
                    let body = ordinary_body(kind, size);
                    let before = serde_json::to_vec(&body).unwrap();
                    for dispatch in [false, true] {
                        probe(&state, &auth, &body, dispatch).await;
                    }
                    assert_eq!(serde_json::to_vec(&body).unwrap(), before);
                }
            }
            assert_eq!(registry.calls.load(Ordering::SeqCst), 0);
        }
    }
}

async fn assert_no_body_sized_allocation(dispatch: bool) {
    let (state, auth, registry) = fixture(true, true);
    for kind in ["text", "schema", "inline_image"] {
        let small = ordinary_body(kind, 128);
        let large = ordinary_body(kind, 1024 * 1024);
        let small_allocations = probe(&state, &auth, &small, dispatch).await;
        let large_allocations = probe(&state, &auth, &large, dispatch).await;
        // 结构完全相同，只加长一个无引用字符串；1KiB余量不允许复制1MiB正文。
        assert!(
            large_allocations.bytes <= small_allocations.bytes.saturating_add(1024),
            "{kind} dispatch={dispatch}: payload-sized copy remains: small={small_allocations:?}, large={large_allocations:?}"
        );
    }
    assert_eq!(registry.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn entry_reference_scan_does_not_copy_an_unreferenced_megabyte() {
    assert_no_body_sized_allocation(false).await;
}

#[tokio::test(flavor = "current_thread")]
async fn dispatch_reference_scan_does_not_copy_an_unreferenced_megabyte() {
    assert_no_body_sized_allocation(true).await;
}

#[test]
fn formal_reference_slots_keep_their_protocol_meaning_and_deduplicate() {
    let body = json!({
        "conversation":{"id":"conv_one"},
        "input":[{"role":"user","content":[
            {"type":"input_file","file_id":"file_input"},
            {"type":"input_image","file_id":"file_input"},
            {"type":"computer_screenshot","file_id":"file_screen"}
        ]},{"type":"item_reference","id":"item_one"},
        {"type":"function_call_output","output":[{"type":"input_file","file_id":"file_output"}]},
        {"type":"function_call","arguments":{"type":"input_file","file_id":"opaque_argument"}},
        {"type":"additional_tools","tools":[{"type":"file_search","vector_store_ids":["vs_one","vs_one"]}]}],
        "tools":[{"type":"code_interpreter","container":"cntr_one"},
          {"type":"image_generation","input_image_mask":{"file_id":"file_mask"}},
          {"type":"shell","environment":{"type":"container_auto","file_ids":["file_auto"],
            "skills":[{"type":"skill_reference","skill_id":"skill_one"}]}},
          {"type":"function","parameters":{"type":"object","properties":{"file_id":{"default":"opaque_schema"}}}}],
        "tool_choice":{"type":"allowed_tools","tools":[{"type":"container_reference","container_id":"cntr_one"}]},
        "metadata":{"type":"item_reference","id":"opaque_metadata"}
    });
    let before = serde_json::to_vec(&body).unwrap();
    let mut checked = body.clone();
    assert_eq!(
        take_references(&mut checked),
        vec![
            (NativeObjectKind::Conversation, "conv_one".into()),
            (NativeObjectKind::File, "file_auto".into()),
            (NativeObjectKind::File, "file_input".into()),
            (NativeObjectKind::File, "file_mask".into()),
            (NativeObjectKind::File, "file_output".into()),
            (NativeObjectKind::File, "file_screen".into()),
            (NativeObjectKind::ResponseContainer, "cntr_one".into()),
            (NativeObjectKind::ResponseItem, "item_one".into()),
            (NativeObjectKind::Skill, "skill_one".into()),
            (NativeObjectKind::VectorStore, "vs_one".into()),
        ]
    );
    assert_eq!(serde_json::to_vec(&body).unwrap(), before);
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_reference_slots_still_reach_the_existing_object_lookup() {
    let cases = [
        (
            NativeObjectKind::Conversation,
            "conv_one",
            json!({"conversation":"conv_one"}),
        ),
        (
            NativeObjectKind::Conversation,
            "conv_one",
            json!({"conversation":{"id":"conv_one"}}),
        ),
        (
            NativeObjectKind::ResponseItem,
            "item_one",
            json!({"input":[{"type":"item_reference","id":"item_one"}]}),
        ),
        (
            NativeObjectKind::File,
            "file_one",
            json!({"input":[{"type":"input_file","file_id":"file_one"}]}),
        ),
        (
            NativeObjectKind::File,
            "file_one",
            json!({"input":[{"role":"user","content":[{"type":"input_image","file_id":"file_one"}]}]}),
        ),
        (
            NativeObjectKind::File,
            "file_one",
            json!({"input":[{"type":"computer_call_output","output":[{"type":"computer_screenshot","file_id":"file_one"}]}]}),
        ),
        (
            NativeObjectKind::ResponseContainer,
            "cntr_one",
            json!({"tools":[{"type":"code_interpreter","container":"cntr_one"}]}),
        ),
        (
            NativeObjectKind::ResponseContainer,
            "cntr_one",
            json!({"input":[{"type":"code_interpreter_call","container_id":"cntr_one"}]}),
        ),
        (
            NativeObjectKind::File,
            "file_one",
            json!({"tools":[{"type":"image_generation","input_image_mask":{"file_id":"file_one"}}]}),
        ),
        (
            NativeObjectKind::File,
            "file_one",
            json!({"tools":[{"type":"code_interpreter","container":{"type":"auto","file_ids":["file_one"]}}]}),
        ),
        (
            NativeObjectKind::Skill,
            "skill_private_unit",
            json!({"tools":[{"type":"shell","environment":{"type":"container_auto","skills":[{"type":"skill_reference","skill_id":"skill_private_unit"}]}}]}),
        ),
        (
            NativeObjectKind::VectorStore,
            "vs_one",
            json!({"tool_choice":{"type":"allowed_tools","tools":[{"type":"file_search","vector_store_ids":["vs_one"]}]}}),
        ),
    ];
    for (kind, raw, body) in cases {
        let (state, auth, registry) = fixture(true, true);
        let before = serde_json::to_vec(&body).unwrap();
        assert!(
            resolve(&state, &auth, &ClientContext::default(), &body, None)
                .await
                .is_err()
        );
        let lookups = registry.lookups.lock().unwrap();
        assert_eq!(lookups.len(), 1, "{kind:?} {raw}");
        assert_eq!(lookups[0].kind, kind);
        assert_eq!(lookups[0].raw_id, raw);
        assert_eq!(serde_json::to_vec(&body).unwrap(), before);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn no_resolved_references_never_skips_the_existing_stored_field_refusal() {
    let (state, auth, registry) = fixture(true, true);
    let body = json!({"model":"public-model","input":"hello","prompt":{"id":"pmpt_unowned"}});
    let before = serde_json::to_vec(&body).unwrap();
    let result = crate::native_api::openai_uploads::admit_responses_files(
        &state,
        &state.snapshot.load(),
        &auth,
        &ClientContext::default(),
        &body,
        false,
        None,
    )
    .await;
    assert!(
        result.is_err(),
        "absence of resolved refs is not absence of the legacy refusal check"
    );
    assert_eq!(registry.calls.load(Ordering::SeqCst), 0);
    assert_eq!(serde_json::to_vec(&body).unwrap(), before);
}

#[tokio::test(flavor = "current_thread")]
async fn ordinary_reference_scan_component_timing_probe() {
    let iterations: usize = std::env::var("AISIX_RESPONSE_REFS_PROBE_ITERS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(16);
    assert!(iterations > 0);
    for native in [false, true] {
        for platform in [false, true] {
            let (state, auth, registry) = fixture(native, platform);
            for kind in ["text", "schema", "inline_image"] {
                for size in [128, 8 * 1024, 1024 * 1024] {
                    let body = ordinary_body(kind, size);
                    let before = serde_json::to_vec(&body).unwrap();
                    for dispatch in [false, true] {
                        let mut allocations = Allocations::default();
                        let start = Instant::now();
                        for _ in 0..iterations {
                            let sample = probe(&state, &auth, &body, dispatch).await;
                            allocations.calls += sample.calls;
                            allocations.bytes += sample.bytes;
                        }
                        println!(
                            "response_refs_probe {}",
                            json!({
                                "native":native,"platform":platform,"kind":kind,"field_bytes":size,
                                "serialized_bytes":before.len(),"dispatch":dispatch,"iterations":iterations,
                                "elapsed_ns":start.elapsed().as_nanos(),
                                "allocated_calls":allocations.calls,"allocated_bytes":allocations.bytes,
                            "authority_rpc_calls":registry.calls.load(Ordering::SeqCst)
                            })
                        );
                    }
                    assert_eq!(serde_json::to_vec(&body).unwrap(), before);
                }
            }
            assert_eq!(registry.calls.load(Ordering::SeqCst), 0);
        }
    }
}
