//! 当前轻支持源的路由分区组件；不作为提供商 E2E 或性能证据。

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use std::collections::BTreeSet;
use tower::ServiceExt;

const KEPT_PATHS: &[&str] = &[
    "/.well-known/oauth-authorization-server/mcp/:server",
    "/.well-known/oauth-protected-resource",
    "/.well-known/oauth-protected-resource/mcp",
    "/.well-known/oauth-protected-resource/mcp/:server",
    "/a2a/:agent",
    "/a2a/:agent/.well-known/agent-card.json",
    "/livez",
    "/mcp",
    "/mcp-oauth/callback",
    "/mcp-oauth/consent",
    "/mcp-oauth/login/callback",
    "/mcp/",
    "/mcp/:server",
    "/mcp/:server/.well-known/jwks.json",
    "/mcp/:server/.well-known/openid-configuration",
    "/mcp/:server/oauth/authorize",
    "/mcp/:server/oauth/credential",
    "/mcp/:server/oauth/register",
    "/mcp/:server/oauth/token",
    "/readyz",
    "/v1/audio/speech",
    "/v1/audio/transcriptions",
    "/v1/audio/voice_consents",
    "/v1/audio/voice_consents/:id",
    "/v1/audio/voices",
    "/v1/audio/voices/:id",
    "/v1/batches",
    "/v1/batches/:id",
    "/v1/batches/:id/cancel",
    "/v1/chat/completions",
    "/v1/chat/completions/:id",
    "/v1/chat/completions/:id/messages",
    "/v1/completions",
    "/v1/containers",
    "/v1/containers/:id",
    "/v1/containers/:id/files",
    "/v1/containers/:id/files/:file_id",
    "/v1/containers/:id/files/:file_id/content",
    "/v1/content_provenance_checks",
    "/v1/conversations",
    "/v1/conversations/:id",
    "/v1/conversations/:id/items",
    "/v1/conversations/:id/items/:item_id",
    "/v1/decisions",
    "/v1/embeddings",
    "/v1/files",
    "/v1/files/:id",
    "/v1/files/:id/content",
    "/v1/generation",
    "/v1/images/edits",
    "/v1/images/generations",
    "/v1/key",
    "/v1/messages",
    "/v1/messages/batches",
    "/v1/messages/batches/:id",
    "/v1/messages/batches/:id/cancel",
    "/v1/messages/batches/:id/results",
    "/v1/messages/count_tokens",
    "/v1/models",
    "/v1/models/:id",
    "/v1/moderations",
    "/v1/realtime",
    "/v1/rerank",
    "/v1/responses",
    "/v1/responses/:id",
    "/v1/responses/:id/cancel",
    "/v1/responses/:id/input_items",
    "/v1/responses/compact",
    "/v1/responses/input_tokens",
    "/v1/skills",
    "/v1/skills/:id",
    "/v1/skills/:id/content",
    "/v1/skills/:id/versions",
    "/v1/skills/:id/versions/:version",
    "/v1/skills/:id/versions/:version/content",
    "/v1/systemone",
    "/v1/uploads",
    "/v1/uploads/:id/cancel",
    "/v1/uploads/:id/complete",
    "/v1/uploads/:id/parts",
    "/v1/vector_stores",
    "/v1/vector_stores/:id",
    "/v1/vector_stores/:id/file_batches",
    "/v1/vector_stores/:id/file_batches/:batch_id",
    "/v1/vector_stores/:id/file_batches/:batch_id/cancel",
    "/v1/vector_stores/:id/file_batches/:batch_id/files",
    "/v1/vector_stores/:id/files",
    "/v1/vector_stores/:id/files/:file_id",
    "/v1/vector_stores/:id/files/:file_id/content",
    "/v1/vector_stores/:id/search",
    "/v1/videos",
    "/v1/videos/:id",
    "/v1/videos/:id/content",
];
const REQUIRED_GA_PATHS: &[&str] = &[
    "/v1/realtime/client_secrets",
    "/v1/realtime/translations",
    "/v1/realtime/translations/client_secrets",
];
const RETIRED_MOUNT_PATHS: &[&str] = &[
    "/v1/audio/translations",
    "/v1/evals",
    "/v1/evals/:eval_id",
    "/v1/evals/:eval_id/runs",
    "/v1/evals/:eval_id/runs/:run_id",
    "/v1/evals/:eval_id/runs/:run_id/cancel",
    "/v1/evals/:eval_id/runs/:run_id/output_items",
    "/v1/evals/:eval_id/runs/:run_id/output_items/:output_item_id",
    "/v1/fine_tuning/jobs",
    "/v1/fine_tuning/jobs/:id",
    "/v1/fine_tuning/jobs/:id/cancel",
    "/v1/images/variations",
];
const RETIRED_REQUESTS: &[(&str, &str)] = &[
    ("POST", "/v1/images/variations"),
    ("POST", "/v1/evals"),
    ("GET", "/v1/evals"),
    ("GET", "/v1/evals/owned_eval_id"),
    ("POST", "/v1/evals/owned_eval_id"),
    ("DELETE", "/v1/evals/owned_eval_id"),
    ("POST", "/v1/evals/owned_eval_id/runs"),
    ("GET", "/v1/evals/owned_eval_id/runs"),
    ("GET", "/v1/evals/owned_eval_id/runs/owned_run_id"),
    ("DELETE", "/v1/evals/owned_eval_id/runs/owned_run_id"),
    ("POST", "/v1/evals/owned_eval_id/runs/owned_run_id/cancel"),
    (
        "GET",
        "/v1/evals/owned_eval_id/runs/owned_run_id/output_items",
    ),
    (
        "GET",
        "/v1/evals/owned_eval_id/runs/owned_run_id/output_items/owned_output_item_id",
    ),
    ("POST", "/v1/audio/translations"),
    ("POST", "/v1/fine_tuning/jobs"),
    ("GET", "/v1/fine_tuning/jobs"),
    ("GET", "/v1/fine_tuning/jobs/owned_id"),
    ("POST", "/v1/fine_tuning/jobs/owned_id/cancel"),
    ("POST", "/v1/realtime/sessions"),
    ("POST", "/v1/realtime/transcription_sessions"),
    ("POST", "/v1/assistants"),
    ("GET", "/v1/assistants"),
    ("POST", "/v1/threads"),
    ("POST", "/v1/threads/runs"),
    ("POST", "/v1/realtime/calls"),
    ("POST", "/v1/realtime/calls/owned_call_id/accept"),
    ("POST", "/v1/realtime/calls/owned_call_id/reject"),
    ("POST", "/v1/realtime/calls/owned_call_id/refer"),
    ("POST", "/v1/realtime/calls/owned_call_id/hangup"),
];

#[test]
fn active_and_retired_paths_form_the_complete_supported_partition() {
    let kept: BTreeSet<_> = KEPT_PATHS.iter().copied().collect();
    let retired: BTreeSet<_> = RETIRED_MOUNT_PATHS.iter().copied().collect();
    assert_eq!(kept.len(), 93);
    assert_eq!(retired.len(), 12);
    assert!(kept.is_disjoint(&retired));
    let mounted = super::mounted_surfaces();
    for retired in &retired {
        assert!(
            !mounted.contains(*retired),
            "retired path remounted: {retired}"
        );
    }
    let expected: BTreeSet<String> = kept
        .into_iter()
        .chain(REQUIRED_GA_PATHS.iter().copied())
        .chain(std::iter::once(super::FALLBACK_SURFACE))
        .map(str::to_owned)
        .collect();
    assert_eq!(expected.len(), 97);
    assert_eq!(
        mounted, expected,
        "GA gaps remain active requirements; do not filter them from the expected set"
    );
}

#[tokio::test]
async fn retired_http_paths_return_not_found_with_a_valid_key() {
    let router = super::census_router();
    for (method, path) in RETIRED_REQUESTS {
        let request = Request::builder().method(*method).uri(*path)
            .header("authorization", format!("Bearer {}", super::CALLER))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"model":"census-openai","prompt":"route scope","training_file":"file-retired"}"#))
            .expect("固定路由fixture可构造");
        let response = router
            .clone()
            .oneshot(request)
            .await
            .expect("路由组件可返回");
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method} {path}");
    }
}
