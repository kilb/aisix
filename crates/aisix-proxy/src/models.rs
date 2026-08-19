//! `GET /v1/models` and `GET /v1/models/{id}` — model discovery.
//!
//! Served in TWO wire shapes from the same routes, because the gateway
//! fronts both SDK families and each defines these endpoints at the same
//! paths. The request's `anthropic-version` header decides: the Anthropic
//! SDK sets it on every call and an OpenAI-shaped client never does.
//! (`x-api-key` is NOT the discriminator — this gateway accepts it as a
//! convenience credential on every endpoint, so it says nothing about which
//! SDK is calling.)
//!
//! Without this, an Anthropic SDK doing `client.models.list()` against the
//! gateway received OpenAI's envelope and failed to deserialize — the same
//! silent, client-side-only failure shape as any other endpoint-family gap.
//!
//! Returns the subset of Models in the current snapshot that the
//! authenticated ApiKey is permitted to use. The response shape
//! matches the OpenAI `/v1/models` contract so any client that uses
//! `client.models.list()` sees the models available to it.
//!
//! The listing is defined as *the names a caller may put in a request's
//! `model` field*, so every dispatch shape qualifies: direct models and
//! the virtual aliases (routing / semantic / ensemble) alike. A Model
//! Group is the stable public entry point its operator intends callers
//! to use, and the same `allowed_models` ACL that authorizes the request
//! decides whether it appears here.
//!
//! OpenAI shape (<https://platform.openai.com/docs/api-reference/models>):
//! `{"id", "object": "model", "created", "owned_by"}` inside
//! `{"object": "list", "data": [...]}`.
//!
//! Anthropic shape (<https://platform.claude.com/docs/en/api/models/list>):
//! `{"id", "type": "model", "display_name", "created_at"}` inside
//! `{"data": [...], "first_id", "last_id", "has_more"}`, with `limit` /
//! `after_id` / `before_id` cursor pagination.
//!
//! ## Two divergences from upstream, both forced
//!
//! **Creation time is the epoch.** No Model carries one: the resource has no
//! such field and the control plane writes none, so there is nothing
//! truthful to report. It was previously stamped with `now`, which was both
//! false and unstable — the same model reported a different `created` on
//! every call, breaking any client that diffs or caches the list. The epoch
//! is what Anthropic's own spec prescribes for this case ("May be set to an
//! epoch value if the release date is unknown"), and the OpenAI shape uses
//! the same value for the same reason.
//!
//! **Ordering is alphabetical by id.** Anthropic orders by release date,
//! most recent first; with no release date to sort on, a stable
//! alphabetical order is the only ordering the gateway can promise, and a
//! stable one matters more here than a plausible one — cursor pagination is
//! only correct over a total order that does not shift between pages. A
//! total order also needs distinct ids, which is why the name list is
//! deduplicated: two model rows may carry the same `display_name`.

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use crate::auth::AuthenticatedKey;
use crate::error::ProxyError;
use crate::state::ProxyState;

/// Creation time reported for every Model, in both shapes.
///
/// Nothing records when a Model was created — the resource has no such
/// field and the control plane writes none — so the epoch is the honest
/// answer, and the one Anthropic's spec names for exactly this case. It is
/// also STABLE, which the previous `now` was not: a value that changes on
/// every call breaks any client that diffs or caches the list.
const UNKNOWN_CREATED_UNIX: i64 = 0;
const UNKNOWN_CREATED_RFC3339: &str = "1970-01-01T00:00:00Z";

/// Anthropic's documented page size: default 20, clamped to 1..=1000.
const ANTHROPIC_DEFAULT_LIMIT: usize = 20;
const ANTHROPIC_MAX_LIMIT: usize = 1000;

/// Whether the caller is speaking Anthropic's dialect.
///
/// `anthropic-version` is required on every Anthropic API request and is
/// never sent by an OpenAI-shaped client, which makes it the one header
/// that identifies the dialect rather than merely the credential.
fn wants_anthropic_shape(headers: &HeaderMap) -> bool {
    headers.contains_key("anthropic-version")
}

/// Render an error in whichever dialect the caller asked in, so an SDK sees
/// the envelope its own error type deserializes.
fn dialect_error(headers: &HeaderMap, err: ProxyError) -> Response {
    if wants_anthropic_shape(headers) {
        err.into_anthropic_response()
    } else {
        err.into_response()
    }
}

/// A single model entry in the `/v1/models` response.
#[derive(Debug, Serialize)]
pub struct ModelObject {
    pub id: String,
    pub object: &'static str,
    pub created: i64,
    pub owned_by: String,
}

/// OpenAI-style list envelope.
#[derive(Debug, Serialize)]
pub struct ModelList {
    pub object: &'static str,
    pub data: Vec<ModelObject>,
}

/// A single model in Anthropic's shape.
///
/// The three nullable fields are emitted explicitly rather than omitted:
/// the gateway does not track an upstream model's capabilities or context
/// limits, and a present `null` says that, where an absent key says nothing
/// and deserializes to `undefined` in the SDKs that type them as
/// `T | null`.
#[derive(Debug, Serialize)]
pub struct AnthropicModelObject {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub display_name: String,
    pub created_at: &'static str,
    pub capabilities: Option<()>,
    pub max_input_tokens: Option<u32>,
    pub max_tokens: Option<u32>,
}

/// Anthropic's paginated list envelope.
#[derive(Debug, Serialize)]
pub struct AnthropicModelList {
    pub data: Vec<AnthropicModelObject>,
    pub first_id: Option<String>,
    pub last_id: Option<String>,
    pub has_more: bool,
}

/// Cursor pagination, per Anthropic's list contract. Ignored by the OpenAI
/// shape, whose own list endpoint defines no pagination.
///
/// Parsed by hand from a string map rather than through a typed extractor:
/// a typed one rejects `?limit=abc` before the handler runs, and axum's own
/// rejection is plain text that neither SDK's error type can read — the same
/// client-side-only failure this module exists to remove.
#[derive(Debug, Default)]
pub struct ListParams {
    limit: Option<usize>,
    after_id: Option<String>,
    before_id: Option<String>,
}

impl ListParams {
    fn parse(raw: &std::collections::HashMap<String, String>) -> Result<Self, ProxyError> {
        let limit = match raw.get("limit") {
            None => None,
            Some(v) => Some(v.trim().parse::<usize>().map_err(|_| {
                ProxyError::InvalidRequest(format!(
                    "`limit` must be a whole number between 1 and {ANTHROPIC_MAX_LIMIT}, got {v:?}"
                ))
            })?),
        };
        if let Some(n) = limit {
            if n == 0 || n > ANTHROPIC_MAX_LIMIT {
                return Err(ProxyError::InvalidRequest(format!(
                    "`limit` must be between 1 and {ANTHROPIC_MAX_LIMIT}, got {n}"
                )));
            }
        }
        Ok(Self {
            limit,
            after_id: raw.get("after_id").cloned(),
            before_id: raw.get("before_id").cloned(),
        })
    }
}

/// Names the caller may put in a request's `model` field, sorted.
///
/// Wildcard aliases (`provider/*`) are patterns rather than requestable
/// ids, so they are the one exclusion. The same `allowed_models` ACL that
/// authorizes a request decides what appears here, so the list never
/// advertises a name the caller cannot then use.
fn permitted_model_names(
    snapshot: &aisix_core::AisixSnapshot,
    auth: &AuthenticatedKey,
) -> Vec<String> {
    let all: Vec<String> = snapshot
        .models
        .entries()
        .into_iter()
        .filter(|e| !e.value.display_name.contains('*'))
        .map(|e| e.value.display_name.clone())
        .collect();
    let mut names: Vec<String> = auth
        .key()
        .accessible_models(all.iter().map(|s: &String| s.as_str()))
        .into_iter()
        .map(str::to_string)
        .collect();
    names.sort();
    // Two rows may carry the same `display_name`: the name index keeps only
    // the last, but `entries()` walks the id index, which holds both. A
    // duplicate would advertise the same id twice AND wedge cursor
    // pagination — `position()` on a repeated name always returns the first
    // occurrence, so a client that pages past it is handed the same page
    // forever.
    names.dedup();
    names
}

/// `owned_by` for the OpenAI shape: the model's vendor when it has one.
/// A routing / ensemble / semantic alias has no single vendor — it is the
/// gateway's own construct — so it is owned by the gateway.
fn owned_by(snapshot: &aisix_core::AisixSnapshot, name: &str) -> String {
    snapshot
        .models
        .get_by_name(name)
        .and_then(|e| e.value.provider.clone())
        .unwrap_or_else(|| "aisix".to_string())
}

fn openai_model(snapshot: &aisix_core::AisixSnapshot, name: &str) -> ModelObject {
    ModelObject {
        id: name.to_string(),
        object: "model",
        created: UNKNOWN_CREATED_UNIX,
        owned_by: owned_by(snapshot, name),
    }
}

fn anthropic_model(name: &str) -> AnthropicModelObject {
    AnthropicModelObject {
        id: name.to_string(),
        kind: "model",
        // The gateway's model name IS the human-readable name an operator
        // chose; there is no second, prettier one to report.
        display_name: name.to_string(),
        created_at: UNKNOWN_CREATED_RFC3339,
        capabilities: None,
        max_input_tokens: None,
        max_tokens: None,
    }
}

/// Apply Anthropic's cursor pagination to the sorted name list.
///
/// `after_id` and `before_id` are positions in that order, not filters: a
/// cursor naming something the caller cannot see (or that no longer
/// exists) yields an empty page rather than silently restarting from the
/// top, which would make a paginating client loop forever.
fn paginate(names: &[String], params: &ListParams) -> (Vec<String>, bool) {
    let limit = params.limit.unwrap_or(ANTHROPIC_DEFAULT_LIMIT);

    if let Some(before) = params.before_id.as_deref() {
        let Some(at) = names.iter().position(|n| n == before) else {
            return (Vec::new(), false);
        };
        let start = at.saturating_sub(limit);
        return (names[start..at].to_vec(), start > 0);
    }

    let start = match params.after_id.as_deref() {
        Some(after) => match names.iter().position(|n| n == after) {
            Some(at) => at + 1,
            None => return (Vec::new(), false),
        },
        None => 0,
    };
    let end = (start + limit).min(names.len());
    let page = names.get(start..end).unwrap_or_default().to_vec();
    (page, end < names.len())
}

pub async fn list_models(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    headers: HeaderMap,
    Query(raw): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let params = match ListParams::parse(&raw) {
        Ok(p) => p,
        Err(e) => return dialect_error(&headers, e),
    };
    let snapshot = state.snapshot.load();
    let names = permitted_model_names(&snapshot, &auth);

    if !wants_anthropic_shape(&headers) {
        // OpenAI's list endpoint defines no pagination — every permitted
        // model, in one page.
        return Json(ModelList {
            object: "list",
            data: names.iter().map(|n| openai_model(&snapshot, n)).collect(),
        })
        .into_response();
    }

    let (page, has_more) = paginate(&names, &params);
    Json(AnthropicModelList {
        first_id: page.first().cloned(),
        last_id: page.last().cloned(),
        data: page.iter().map(|n| anthropic_model(n)).collect(),
        has_more,
    })
    .into_response()
}

/// `GET /v1/models/{id}` — both SDK families define a retrieve-one endpoint
/// at this path, so one route serves both and the dialect decides the shape.
///
/// A model the caller may not reach answers 404, not 403: the list omits it
/// entirely, and an endpoint that distinguished "no such model" from "not
/// yours" would let a caller enumerate an environment's models through the
/// error code alone. Same existence fold `/v1/videos` and `/mcp` apply.
pub async fn get_model(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    headers: HeaderMap,
    crate::reject::AisixPath(id): crate::reject::AisixPath<String>,
) -> Response {
    let snapshot = state.snapshot.load();
    let known = snapshot
        .models
        .get_by_name(&id)
        .is_some_and(|e| !e.value.display_name.contains('*'));
    if !known || !auth.key().can_access(&id) {
        return dialect_error(&headers, ProxyError::ModelNotFound(id));
    }

    if wants_anthropic_shape(&headers) {
        Json(anthropic_model(&id)).into_response()
    } else {
        Json(openai_model(&snapshot, &id)).into_response()
    }
}

#[cfg(test)]
mod tests {

    use aisix_core::resource::ResourceEntry;
    use aisix_core::snapshot::SnapshotHandle;
    use aisix_core::{AisixSnapshot, ApiKey, Model, ProxyConfig};
    use aisix_gateway::Hub;
    use aisix_provider_openai::OpenAiBridge;
    use axum::body::to_bytes;
    use axum::http::{Request, StatusCode};
    use axum::Router;
    use std::sync::Arc;

    fn cfg() -> ProxyConfig {
        ProxyConfig {
            addr: "127.0.0.1:0".into(),
            request_body_limit_bytes: Some(1_048_576),
            real_ip: Default::default(),
            request_id: Default::default(),
            url_rewrites: Vec::new(),
            tls: None,
            thread_per_core: None,
            workers: None,
        }
    }

    fn build_app(snapshot: AisixSnapshot) -> Router {
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let handle = SnapshotHandle::new(snapshot);
        crate::build_router(crate::ProxyState::new(handle, hub, &cfg()).without_cache())
    }

    const PK_ID: &str = "11111111-1111-1111-1111-111111111111";

    fn model_entry(id: &str, name: &str) -> ResourceEntry<Model> {
        let cfg = format!(
            r#"{{
                "display_name": "{name}",
                "provider": "openai",
                "model_name": "gpt-4o",
                "provider_key_id": "{PK_ID}"
            }}"#
        );
        let m: Model = serde_json::from_str(&cfg).unwrap();
        ResourceEntry::new(id, m, 1)
    }

    fn provider_key_entry() -> ResourceEntry<aisix_core::ProviderKey> {
        let pk: aisix_core::ProviderKey =
            serde_json::from_str(r#"{"display_name":"openai-up","secret":"sk-up","provider":"openai","adapter":"openai"}"#).unwrap();
        ResourceEntry::new(PK_ID, pk, 1)
    }

    fn new_snap() -> AisixSnapshot {
        let snap = AisixSnapshot::new();
        snap.provider_keys.insert(provider_key_entry());
        snap
    }

    fn apikey_entry(key: &str, allowed: &[&str]) -> ResourceEntry<ApiKey> {
        // Plaintext in, hash on the wire (§9A.7B.4).
        let key_hash = ApiKey::hash_bearer(key);
        let json = format!(
            r#"{{"key_hash": "{key_hash}", "allowed_models": {}}}"#,
            serde_json::to_string(&allowed).unwrap()
        );
        let k: ApiKey = serde_json::from_str(&json).unwrap();
        ResourceEntry::new("key-1", k, 1)
    }

    #[tokio::test]
    async fn unauthenticated_request_is_401() {
        let snap = new_snap();
        snap.models.insert(model_entry("m1", "gpt4"));
        snap.apikeys.insert(apikey_entry("sk-caller", &["*"]));

        let app = build_app(snap);
        let req = Request::builder()
            .method("GET")
            .uri("/v1/models")
            .body(axum::body::Body::empty())
            .unwrap();

        let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn wildcard_key_sees_all_concrete_models() {
        let snap = new_snap();
        snap.models.insert(model_entry("m1", "gpt4"));
        snap.models.insert(model_entry("m2", "claude"));
        snap.apikeys.insert(apikey_entry("sk-caller", &["*"]));

        let app = build_app(snap);
        let req = Request::builder()
            .method("GET")
            .uri("/v1/models")
            .header("authorization", "Bearer sk-caller")
            .body(axum::body::Body::empty())
            .unwrap();

        let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["object"], "list");
        let data = v["data"].as_array().unwrap();
        assert_eq!(data.len(), 2);
        // sorted by id
        assert_eq!(data[0]["id"], "claude");
        assert_eq!(data[1]["id"], "gpt4");
    }

    #[tokio::test]
    async fn restricted_key_sees_only_allowed_models() {
        let snap = new_snap();
        snap.models.insert(model_entry("m1", "gpt4"));
        snap.models.insert(model_entry("m2", "claude"));
        snap.apikeys.insert(apikey_entry("sk-caller", &["gpt4"]));

        let app = build_app(snap);
        let req = Request::builder()
            .method("GET")
            .uri("/v1/models")
            .header("authorization", "Bearer sk-caller")
            .body(axum::body::Body::empty())
            .unwrap();

        let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let data = v["data"].as_array().unwrap();
        assert_eq!(data.len(), 1);
        assert_eq!(data[0]["id"], "gpt4");
    }

    #[tokio::test]
    async fn empty_allowed_models_returns_empty_list() {
        let snap = new_snap();
        snap.models.insert(model_entry("m1", "gpt4"));
        snap.apikeys.insert(apikey_entry("sk-caller", &[]));

        let app = build_app(snap);
        let req = Request::builder()
            .method("GET")
            .uri("/v1/models")
            .header("authorization", "Bearer sk-caller")
            .body(axum::body::Body::empty())
            .unwrap();

        let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let data = v["data"].as_array().unwrap();
        assert_eq!(data.len(), 0);
    }

    /// Every virtual dispatch shape — routing, semantic, ensemble — is a name
    /// a caller may request, so all three list alongside direct models.
    #[tokio::test]
    async fn virtual_aliases_are_listed_alongside_direct_models() {
        let snap = new_snap();
        snap.models.insert(model_entry("m1", "gpt4"));
        snap.models.insert(model_entry("m2", "embed"));

        let routing: Model = serde_json::from_value(serde_json::json!({
            "display_name": "smart-router",
            "routing": {
                "strategy": "failover",
                "targets": [{"model": "gpt4"}]
            }
        }))
        .unwrap();
        snap.models.insert(ResourceEntry::new("r1", routing, 1));

        let semantic: Model = serde_json::from_value(serde_json::json!({
            "display_name": "topic-router",
            "semantic": {
                "embedding_model": "embed",
                "routes": [{"name": "legal", "target": "gpt4", "examples": ["contract review"]}],
                "default": "gpt4",
                "match": {"threshold": 0.8}
            }
        }))
        .unwrap();
        snap.models.insert(ResourceEntry::new("s1", semantic, 1));

        let ensemble: Model = serde_json::from_value(serde_json::json!({
            "display_name": "panel",
            "ensemble": {
                "panel": [{"model": "gpt4"}],
                "judge": {"model": "gpt4"}
            }
        }))
        .unwrap();
        snap.models.insert(ResourceEntry::new("e1", ensemble, 1));

        snap.apikeys.insert(apikey_entry("sk-caller", &["*"]));

        let app = build_app(snap);
        let req = Request::builder()
            .method("GET")
            .uri("/v1/models")
            .header("authorization", "Bearer sk-caller")
            .body(axum::body::Body::empty())
            .unwrap();

        let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let ids: Vec<&str> = v["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            ["embed", "gpt4", "panel", "smart-router", "topic-router"]
        );
    }

    /// A Model Group carries no `provider` of its own, so it falls back to the
    /// gateway as owner rather than leaking a target's provider.
    #[tokio::test]
    async fn routing_model_is_owned_by_the_gateway() {
        let snap = new_snap();
        snap.models.insert(model_entry("m1", "gpt4"));
        let routing: Model = serde_json::from_value(serde_json::json!({
            "display_name": "smart-router",
            "routing": {
                "strategy": "failover",
                "targets": [{"model": "gpt4"}]
            }
        }))
        .unwrap();
        snap.models.insert(ResourceEntry::new("r1", routing, 1));
        snap.apikeys
            .insert(apikey_entry("sk-caller", &["smart-router"]));

        let app = build_app(snap);
        let req = Request::builder()
            .method("GET")
            .uri("/v1/models")
            .header("authorization", "Bearer sk-caller")
            .body(axum::body::Body::empty())
            .unwrap();

        let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let item = &v["data"][0];
        assert_eq!(item["id"], "smart-router");
        assert_eq!(item["object"], "model");
        assert_eq!(item["owned_by"], "aisix");
    }

    /// The Model-Group-only deployment: the key is authorized for the group
    /// alone, so the group is the whole listing and its targets stay hidden.
    #[tokio::test]
    async fn key_scoped_to_a_group_sees_the_group_and_not_its_targets() {
        let snap = new_snap();
        snap.models.insert(model_entry("m1", "gpt4"));
        snap.models.insert(model_entry("m2", "claude"));
        let routing: Model = serde_json::from_value(serde_json::json!({
            "display_name": "my-gpt-group",
            "routing": {
                "strategy": "failover",
                "targets": [{"model": "gpt4"}, {"model": "claude"}]
            }
        }))
        .unwrap();
        snap.models.insert(ResourceEntry::new("r1", routing, 1));
        snap.apikeys
            .insert(apikey_entry("sk-caller", &["my-gpt-group"]));

        let app = build_app(snap);
        let req = Request::builder()
            .method("GET")
            .uri("/v1/models")
            .header("authorization", "Bearer sk-caller")
            .body(axum::body::Body::empty())
            .unwrap();

        let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let data = v["data"].as_array().unwrap();
        assert_eq!(data.len(), 1);
        assert_eq!(data[0]["id"], "my-gpt-group");
    }

    #[tokio::test]
    async fn wildcard_models_are_excluded_from_list() {
        let snap = new_snap();
        snap.models.insert(model_entry("m1", "gpt4"));
        // A `provider/*` wildcard alias is a pattern, not a concrete id.
        snap.models.insert(model_entry("w1", "openai/*"));
        snap.apikeys.insert(apikey_entry("sk-caller", &["*"]));

        let app = build_app(snap);
        let req = Request::builder()
            .method("GET")
            .uri("/v1/models")
            .header("authorization", "Bearer sk-caller")
            .body(axum::body::Body::empty())
            .unwrap();

        let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let data = v["data"].as_array().unwrap();
        // Only the concrete gpt4, not the `openai/*` pattern.
        assert_eq!(data.len(), 1);
        assert_eq!(data[0]["id"], "gpt4");
    }

    #[tokio::test]
    async fn response_shape_matches_openai_contract() {
        let snap = new_snap();
        snap.models.insert(model_entry("m1", "gpt4"));
        snap.apikeys.insert(apikey_entry("sk-caller", &["*"]));

        let app = build_app(snap);
        let req = Request::builder()
            .method("GET")
            .uri("/v1/models")
            .header("authorization", "Bearer sk-caller")
            .body(axum::body::Body::empty())
            .unwrap();

        let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let item = &v["data"][0];
        assert_eq!(item["object"], "model");
        assert_eq!(item["owned_by"], "openai");
        // `created` is the epoch, not `now`. Nothing records when a Model
        // was created, and the previous per-request timestamp was both
        // false and — the part that actually broke clients — different on
        // every call. This asserts the property that matters, which the
        // old `> 0` check could not have caught: it is STABLE.
        assert_eq!(item["created"].as_i64().unwrap(), 0);
    }

    /// The list must not change between identical calls. A `created` stamped
    /// per request made every response differ, so any client diffing or
    /// caching the catalogue saw churn that was not there.
    #[tokio::test]
    async fn the_list_is_byte_identical_across_calls() {
        let snap = AisixSnapshot::new();
        snap.models.insert(model_entry("m-1", "gpt-4o"));
        snap.apikeys.insert(apikey_entry("sk-caller", &["*"]));
        let app = build_app(snap);

        let call = || {
            let app = app.clone();
            async move {
                let req = Request::builder()
                    .method("GET")
                    .uri("/v1/models")
                    .header("authorization", "Bearer sk-caller")
                    .body(axum::body::Body::empty())
                    .unwrap();
                let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
                to_bytes(resp.into_body(), 65536).await.unwrap()
            }
        };
        assert_eq!(call().await, call().await);
    }

    /// An Anthropic SDK reaches the same route and must get its own
    /// envelope: `data` + cursor fields, entries typed `model` with a
    /// `display_name` and an RFC 3339 `created_at`. Served OpenAI's shape,
    /// its `client.models.list()` fails to deserialize — a break that only
    /// ever surfaces on the client.
    #[tokio::test]
    async fn an_anthropic_client_gets_the_anthropic_envelope() {
        let snap = AisixSnapshot::new();
        snap.models.insert(model_entry("m-1", "gpt-4o"));
        snap.apikeys.insert(apikey_entry("sk-caller", &["*"]));
        let app = build_app(snap);

        let req = Request::builder()
            .method("GET")
            .uri("/v1/models")
            .header("x-api-key", "sk-caller")
            .header("anthropic-version", "2023-06-01")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        assert!(
            v.get("object").is_none(),
            "OpenAI's envelope key must be absent: {v}"
        );
        assert_eq!(v["has_more"], false);
        assert_eq!(v["first_id"], "gpt-4o");
        assert_eq!(v["last_id"], "gpt-4o");
        let item = &v["data"][0];
        assert_eq!(item["type"], "model");
        assert_eq!(item["id"], "gpt-4o");
        assert_eq!(item["display_name"], "gpt-4o");
        assert_eq!(item["created_at"], "1970-01-01T00:00:00Z");
    }

    /// `x-api-key` alone must NOT select the Anthropic shape: this gateway
    /// accepts it as a convenience credential on every endpoint, so it says
    /// nothing about which SDK is calling. Only `anthropic-version` does.
    #[tokio::test]
    async fn x_api_key_alone_still_gets_the_openai_envelope() {
        let snap = AisixSnapshot::new();
        snap.models.insert(model_entry("m-1", "gpt-4o"));
        snap.apikeys.insert(apikey_entry("sk-caller", &["*"]));
        let app = build_app(snap);

        let req = Request::builder()
            .method("GET")
            .uri("/v1/models")
            .header("x-api-key", "sk-caller")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["object"], "list");
    }

    /// A paginating client walks the whole catalogue and terminates. With
    /// `limit=1` it must see each model exactly once and stop — the loop a
    /// mis-implemented cursor turns infinite.
    #[tokio::test]
    async fn cursor_pagination_walks_every_model_once_and_stops() {
        let snap = AisixSnapshot::new();
        for (i, name) in ["alpha", "bravo", "charlie"].iter().enumerate() {
            snap.models.insert(model_entry(&format!("m-{i}"), name));
        }
        snap.apikeys.insert(apikey_entry("sk-caller", &["*"]));
        let app = build_app(snap);

        let mut seen: Vec<String> = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..10 {
            let uri = match &cursor {
                Some(c) => format!("/v1/models?limit=1&after_id={c}"),
                None => "/v1/models?limit=1".to_string(),
            };
            let req = Request::builder()
                .method("GET")
                .uri(uri)
                .header("x-api-key", "sk-caller")
                .header("anthropic-version", "2023-06-01")
                .body(axum::body::Body::empty())
                .unwrap();
            let resp = tower::ServiceExt::oneshot(app.clone(), req).await.unwrap();
            let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
            let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            let page = v["data"].as_array().unwrap();
            assert_eq!(page.len(), 1, "limit=1 must yield one entry: {v}");
            seen.push(page[0]["id"].as_str().unwrap().to_string());
            if !v["has_more"].as_bool().unwrap() {
                break;
            }
            cursor = Some(v["last_id"].as_str().unwrap().to_string());
        }
        assert_eq!(seen, vec!["alpha", "bravo", "charlie"]);
    }

    /// Two rows may carry the same `display_name` — the id index holds both
    /// while the name index keeps only the last. Listing them twice would
    /// advertise a duplicate id AND wedge the cursor: `after_id` resolves to
    /// the FIRST occurrence, so the page after it repeats forever.
    #[tokio::test]
    async fn two_models_sharing_a_display_name_are_listed_once() {
        let snap = AisixSnapshot::new();
        snap.models.insert(model_entry("m-1", "shared"));
        snap.models.insert(model_entry("m-2", "shared"));
        snap.models.insert(model_entry("m-3", "zulu"));
        snap.apikeys.insert(apikey_entry("sk-caller", &["*"]));
        let app = build_app(snap);

        let req = Request::builder()
            .method("GET")
            .uri("/v1/models")
            .header("authorization", "Bearer sk-caller")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = tower::ServiceExt::oneshot(app.clone(), req).await.unwrap();
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let ids: Vec<&str> = v["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["shared", "zulu"], "duplicate names collapse: {v}");
    }

    /// A cursor walk over a catalogue containing a duplicated name still
    /// terminates. This is the failure the dedup above prevents: without it
    /// `after_id=shared` lands on the first `shared` every time and the
    /// client re-reads the same page until it gives up.
    #[tokio::test]
    async fn a_duplicated_name_does_not_wedge_the_cursor() {
        let snap = AisixSnapshot::new();
        snap.models.insert(model_entry("m-1", "shared"));
        snap.models.insert(model_entry("m-2", "shared"));
        snap.models.insert(model_entry("m-3", "zulu"));
        snap.apikeys.insert(apikey_entry("sk-caller", &["*"]));
        let app = build_app(snap);

        let mut seen: Vec<String> = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..6 {
            let uri = match &cursor {
                Some(c) => format!("/v1/models?limit=1&after_id={c}"),
                None => "/v1/models?limit=1".to_string(),
            };
            let req = Request::builder()
                .method("GET")
                .uri(uri)
                .header("x-api-key", "sk-caller")
                .header("anthropic-version", "2023-06-01")
                .body(axum::body::Body::empty())
                .unwrap();
            let resp = tower::ServiceExt::oneshot(app.clone(), req).await.unwrap();
            let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
            let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            let page = v["data"].as_array().unwrap();
            if page.is_empty() {
                break;
            }
            seen.push(page[0]["id"].as_str().unwrap().to_string());
            if !v["has_more"].as_bool().unwrap() {
                break;
            }
            cursor = Some(v["last_id"].as_str().unwrap().to_string());
        }
        assert_eq!(seen, vec!["shared", "zulu"]);
    }

    /// A malformed `limit` must come back in the caller's own error shape.
    /// A typed extractor rejects it before the handler runs, and axum's
    /// rejection is plain text — which neither SDK's error type can parse,
    /// so the client raises a decode error instead of the 400 it was sent.
    #[tokio::test]
    async fn a_bad_limit_is_rejected_in_the_caller_dialect() {
        let snap = AisixSnapshot::new();
        snap.models.insert(model_entry("m-1", "gpt-4o"));
        snap.apikeys.insert(apikey_entry("sk-caller", &["*"]));
        let app = build_app(snap);

        for (headers, probe) in [
            (vec![("authorization", "Bearer sk-caller")], "openai"),
            (
                vec![
                    ("x-api-key", "sk-caller"),
                    ("anthropic-version", "2023-06-01"),
                ],
                "anthropic",
            ),
        ] {
            for uri in ["/v1/models?limit=abc", "/v1/models?limit=0"] {
                let mut b = Request::builder().method("GET").uri(uri);
                for (k, v) in &headers {
                    b = b.header(*k, *v);
                }
                let resp = tower::ServiceExt::oneshot(
                    app.clone(),
                    b.body(axum::body::Body::empty()).unwrap(),
                )
                .await
                .unwrap();
                assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{probe} {uri}");
                let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
                let v: serde_json::Value = serde_json::from_slice(&bytes)
                    .unwrap_or_else(|e| panic!("{probe} {uri} must be JSON: {e}"));
                // Both dialects nest the machine-readable code under `error`.
                assert!(
                    v.get("error").and_then(|e| e.get("type")).is_some(),
                    "{probe} {uri} lost its error envelope: {v}"
                );
            }
        }
    }

    /// A cursor naming something the caller cannot see yields an empty page
    /// rather than restarting from the top — restarting is what makes a
    /// paginating client loop forever.
    #[tokio::test]
    async fn an_unknown_cursor_ends_the_walk_instead_of_restarting() {
        let snap = AisixSnapshot::new();
        snap.models.insert(model_entry("m-1", "gpt-4o"));
        snap.apikeys.insert(apikey_entry("sk-caller", &["*"]));
        let app = build_app(snap);

        let req = Request::builder()
            .method("GET")
            .uri("/v1/models?after_id=nothing-by-this-name")
            .header("x-api-key", "sk-caller")
            .header("anthropic-version", "2023-06-01")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["data"].as_array().unwrap().len(), 0);
        assert_eq!(v["has_more"], false);
    }

    /// `GET /v1/models/{id}` serves both dialects from one route.
    #[tokio::test]
    async fn retrieve_serves_both_dialects() {
        let snap = AisixSnapshot::new();
        snap.models.insert(model_entry("m-1", "gpt-4o"));
        snap.apikeys.insert(apikey_entry("sk-caller", &["*"]));
        let app = build_app(snap);

        let openai = Request::builder()
            .method("GET")
            .uri("/v1/models/gpt-4o")
            .header("authorization", "Bearer sk-caller")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = tower::ServiceExt::oneshot(app.clone(), openai)
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["object"], "model");
        assert_eq!(v["id"], "gpt-4o");

        let anthropic = Request::builder()
            .method("GET")
            .uri("/v1/models/gpt-4o")
            .header("x-api-key", "sk-caller")
            .header("anthropic-version", "2023-06-01")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = tower::ServiceExt::oneshot(app, anthropic).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["type"], "model");
        assert_eq!(v["display_name"], "gpt-4o");
    }

    /// A model the caller may not reach answers 404, not 403 — the same
    /// existence fold the list applies by omitting it. A 403 would let a
    /// caller enumerate an environment's models through the error code.
    #[tokio::test]
    async fn retrieving_a_forbidden_model_is_indistinguishable_from_a_missing_one() {
        let snap = AisixSnapshot::new();
        snap.models.insert(model_entry("m-1", "gpt-4o"));
        snap.models.insert(model_entry("m-2", "secret"));
        snap.apikeys.insert(apikey_entry("sk-caller", &["gpt-4o"]));
        let app = build_app(snap);

        let mut statuses = Vec::new();
        for name in ["secret", "does-not-exist"] {
            let req = Request::builder()
                .method("GET")
                .uri(format!("/v1/models/{name}"))
                .header("authorization", "Bearer sk-caller")
                .body(axum::body::Body::empty())
                .unwrap();
            let resp = tower::ServiceExt::oneshot(app.clone(), req).await.unwrap();
            statuses.push(resp.status());
        }
        assert_eq!(
            statuses,
            vec![StatusCode::NOT_FOUND, StatusCode::NOT_FOUND],
            "a forbidden model and a missing one must be indistinguishable",
        );
    }
}
