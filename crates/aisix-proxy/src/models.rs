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
//! to use, and the same key→model ACL that authorizes the request
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
//!
//! ## Prices, in platform mode
//!
//! In platform mode, with a control plane to ask, each model the caller's
//! tenant has a price for carries `pricing`: `currency`, `input`, `output`
//! and, when the price has them, `input_cache_read`, `input_cache_write` and
//! `input_cache_write_1h`, per token as the control plane's exact decimal
//! strings in the currency it names ([`crate::account::prices`]), in both
//! shapes and on both routes. The field names follow the hosted gateways'
//! listings, which carry the same per-token prices as strings. A price is
//! found by the listed row's resource id: it keys on the entry the caller
//! addresses, whatever its kind (a direct model, an embedding model, or a
//! routing, semantic or ensemble alias, each priced as itself; a wildcard
//! alias is never listed). A model without a price, or a listing whose prices
//! the control plane did not give in time, is listed exactly as it is without
//! platform mode's prices; with platform mode off nothing is asked and
//! nothing changes.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use crate::account::prices::{ModelPricing, TenantPricesAnswer};
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
    /// The caller's tenant's price, in platform mode only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pricing: Option<ModelPricing>,
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
    /// The caller's tenant's price, in platform mode only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pricing: Option<ModelPricing>,
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
/// ids, so they are excluded. The same `allowed_models` ACL that authorizes
/// a request decides what appears here, so the list never advertises a name
/// the caller cannot then use; and so does [`listable`].
fn permitted_model_names(
    snapshot: &aisix_core::AisixSnapshot,
    auth: &AuthenticatedKey,
) -> Vec<String> {
    let all: Vec<String> = snapshot
        .models
        .entries()
        .into_iter()
        .filter(|e| listable(&e.value, auth))
        .map(|e| e.value.display_name.clone())
        .collect();
    let mut names: Vec<String> = auth
        .key()
        .accessible_models(snapshot, all.iter().map(|s: &String| s.as_str()))
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

/// Whether a model is a name this caller may be shown, before the
/// `allowed_models` ACL: not a wildcard alias, and not an ensemble when the
/// key is held to provider groups, since every request naming one is
/// answered as a model that does not exist
/// ([`crate::model_resolve::refuse_ensemble_to_grouped_key`]). Keys on the
/// requested entry, the only identity a listing has.
fn listable(model: &aisix_core::Model, auth: &AuthenticatedKey) -> bool {
    !model.display_name.contains('*')
        && crate::model_resolve::refuse_ensemble_to_grouped_key(
            &model.display_name,
            model,
            &auth.key().allowed_provider_groups,
        )
        .is_ok()
}

/// `owned_by` for the OpenAI shape: the model's vendor when it has one.
/// A routing / ensemble / semantic alias has no single vendor — it is the
/// gateway's own construct — so it is owned by the gateway.
/// In platform mode the vendor is never named: every model is owned by the
/// gateway, so a listing does not reveal which upstream channel serves it.
fn owned_by(snapshot: &aisix_core::AisixSnapshot, name: &str, platform: bool) -> String {
    if platform {
        return "aisix".to_string();
    }
    snapshot
        .models
        .get_by_name(name)
        .and_then(|e| e.value.provider.clone())
        .unwrap_or_else(|| "aisix".to_string())
}

fn openai_model(
    snapshot: &aisix_core::AisixSnapshot,
    name: &str,
    platform: bool,
    prices: Option<&TenantPricesAnswer>,
) -> ModelObject {
    ModelObject {
        id: name.to_string(),
        object: "model",
        created: UNKNOWN_CREATED_UNIX,
        owned_by: owned_by(snapshot, name, platform),
        pricing: pricing(snapshot, name, prices),
    }
}

fn anthropic_model(
    snapshot: &aisix_core::AisixSnapshot,
    name: &str,
    prices: Option<&TenantPricesAnswer>,
) -> AnthropicModelObject {
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
        pricing: pricing(snapshot, name, prices),
    }
}

/// The caller's tenant's prices, in platform mode with a control plane to
/// ask; `None` otherwise, without asking. Cached per tenant and provider
/// group ([`crate::account::prices::prices_cache_key`]), since a price is the
/// key's group's rate. Never fails the listing, and never holds it past
/// [`crate::account::prices::PRICES_TIMEOUT`].
async fn tenant_prices(
    state: &ProxyState,
    auth: &AuthenticatedKey,
) -> Option<Arc<TenantPricesAnswer>> {
    state.platform()?;
    let account = state.account.as_deref()?;
    account
        .tenant_prices(
            &crate::account::prices::prices_cache_key(auth),
            &auth.entry.id,
        )
        .await
}

/// The listed model `name`'s price, by the resource id of the row that name
/// lists.
fn pricing(
    snapshot: &aisix_core::AisixSnapshot,
    name: &str,
    prices: Option<&TenantPricesAnswer>,
) -> Option<ModelPricing> {
    let prices = prices?;
    let row = snapshot.models.get_by_name(name)?;
    prices.price_of(&row.id).cloned()
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

/// The access-log line and request metrics a discovery request owes: these
/// routes have no dispatch path to write them.
pub(crate) fn record_discovery(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    method: &axum::http::Method,
    path: &str,
    request_id: Option<&axum::Extension<crate::request_id::RequestId>>,
    response: &Response,
    started: std::time::Instant,
) {
    crate::reject::emit_unrouted_access_log(
        method.as_str(),
        path,
        request_id.map(|r| r.0 .0.as_str()).unwrap_or_default(),
        Some(&auth.entry.id),
        response.status().as_u16(),
        started,
    );
    crate::reject::record_undispatched(
        state,
        path,
        crate::request_metrics::Caller::new(auth),
        response.status().as_u16(),
        crate::attribution::rendered_error_source(response.status().as_u16()),
        started,
    );
}

pub async fn list_models(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    headers: HeaderMap,
    method: axum::http::Method,
    request_id: Option<axum::Extension<crate::request_id::RequestId>>,
    Query(raw): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let started = std::time::Instant::now();
    let response = model_list(&state, &auth, &headers, &raw).await;
    record_discovery(
        &state,
        &auth,
        &method,
        "/v1/models",
        request_id.as_ref(),
        &response,
        started,
    );
    response
}

async fn model_list(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    headers: &HeaderMap,
    raw: &std::collections::HashMap<String, String>,
) -> Response {
    let params = match ListParams::parse(raw) {
        Ok(p) => p,
        Err(e) => return dialect_error(headers, e),
    };
    let prices = tenant_prices(state, auth).await;
    let prices = prices.as_deref();
    let snapshot = state.snapshot.load();
    let names = permitted_model_names(&snapshot, auth);

    if !wants_anthropic_shape(headers) {
        // OpenAI's list endpoint defines no pagination — every permitted
        // model, in one page.
        return Json(ModelList {
            object: "list",
            data: names
                .iter()
                .map(|n| openai_model(&snapshot, n, state.platform().is_some(), prices))
                .collect(),
        })
        .into_response();
    }

    let (page, has_more) = paginate(&names, &params);
    Json(AnthropicModelList {
        first_id: page.first().cloned(),
        last_id: page.last().cloned(),
        data: page
            .iter()
            .map(|n| anthropic_model(&snapshot, n, prices))
            .collect(),
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
    method: axum::http::Method,
    uri: axum::http::Uri,
    request_id: Option<axum::Extension<crate::request_id::RequestId>>,
    crate::reject::AisixPath(id): crate::reject::AisixPath<String>,
) -> Response {
    let started = std::time::Instant::now();
    let response = model_retrieve(&state, &auth, &headers, id).await;
    record_discovery(
        &state,
        &auth,
        &method,
        uri.path(),
        request_id.as_ref(),
        &response,
        started,
    );
    response
}

async fn model_retrieve(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    headers: &HeaderMap,
    id: String,
) -> Response {
    let snapshot = state.snapshot.load();
    let known = snapshot
        .models
        .get_by_name(&id)
        .is_some_and(|e| listable(&e.value, auth));
    if !known || !auth.key().can_access(&snapshot, &id) {
        return dialect_error(headers, ProxyError::ModelNotFound(id));
    }

    // Asked only for a model the caller may see, so a missing and a
    // forbidden model still answer alike, in the same time.
    let prices = tenant_prices(state, auth).await;
    let prices = prices.as_deref();
    if wants_anthropic_shape(headers) {
        Json(anthropic_model(&snapshot, &id, prices)).into_response()
    } else {
        Json(openai_model(
            &snapshot,
            &id,
            state.platform().is_some(),
            prices,
        ))
        .into_response()
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
    use axum::http::{HeaderMap, Request, StatusCode};
    use axum::Router;
    use serde_json::{json, Value};
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use crate::account::prices::{PRICES_FAILURE_TTL, PRICES_TIMEOUT};
    use crate::account::stand_in::StandIn;
    use crate::account::AccountClient;

    fn cfg() -> ProxyConfig {
        ProxyConfig {
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
        }
    }

    fn build_app(snapshot: AisixSnapshot) -> Router {
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let handle = SnapshotHandle::new(snapshot);
        crate::build_router(crate::ProxyState::new(handle, hub, &cfg()).without_cache())
    }

    fn build_platform_app(snapshot: AisixSnapshot) -> Router {
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let handle = SnapshotHandle::new(snapshot);
        let state = crate::ProxyState::new(handle, hub, &cfg())
            .without_cache()
            .with_platform(crate::platform::PlatformSettings::from_secret("s"));
        crate::build_router(state)
    }

    /// Lists `/v1/models` (OpenAI shape) as `sk-caller` and returns the body.
    async fn list_as_caller(app: Router) -> serde_json::Value {
        let req = Request::builder()
            .method("GET")
            .uri("/v1/models")
            .header("authorization", "Bearer sk-caller")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
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

    #[tokio::test]
    async fn platform_mode_lists_every_model_as_owned_by_the_gateway() {
        let snap = new_snap();
        snap.models.insert(model_entry("m1", "gpt4"));
        snap.models.insert(model_entry("m2", "gpt4-mini"));
        snap.apikeys.insert(apikey_entry("sk-caller", &["*"]));

        let body = list_as_caller(build_platform_app(snap)).await;
        let data = body["data"].as_array().unwrap();
        assert_eq!(data.len(), 2);
        assert!(data.iter().all(|m| m["owned_by"] == "aisix"));
    }

    #[tokio::test]
    async fn platform_mode_retrieve_names_no_upstream_channel() {
        let snap = new_snap();
        snap.models.insert(model_entry("m1", "gpt4"));
        snap.apikeys.insert(apikey_entry("sk-caller", &["*"]));

        let req = Request::builder()
            .method("GET")
            .uri("/v1/models/gpt4")
            .header("authorization", "Bearer sk-caller")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = tower::ServiceExt::oneshot(build_platform_app(snap), req)
            .await
            .unwrap();
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["owned_by"], "aisix");
    }

    #[tokio::test]
    async fn without_platform_mode_owned_by_is_the_models_provider() {
        let snap = new_snap();
        snap.models.insert(model_entry("m1", "gpt4"));
        snap.apikeys.insert(apikey_entry("sk-caller", &["*"]));

        let body = list_as_caller(build_app(snap)).await;
        assert_eq!(body["data"][0]["owned_by"], "openai");
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

    /// A key held to provider groups is answered as if no ensemble existed,
    /// so neither the list nor a retrieve names one to it; a key without
    /// groups still sees it.
    #[tokio::test]
    async fn a_key_with_provider_groups_is_shown_no_ensemble() {
        let snap = new_snap();
        snap.models.insert(model_entry("m1", "gpt4"));
        let ensemble: Model = serde_json::from_value(serde_json::json!({
            "display_name": "panel",
            "ensemble": {"panel": [{"model": "gpt4"}], "judge": {"model": "gpt4"}}
        }))
        .unwrap();
        snap.models.insert(ResourceEntry::new("e1", ensemble, 1));
        let grouped: ApiKey = serde_json::from_value(serde_json::json!({
            "key_hash": ApiKey::hash_bearer("sk-grouped"),
            "allowed_models": ["*"],
            "allowed_provider_groups": ["grp-alpha"],
        }))
        .unwrap();
        snap.apikeys.insert(ResourceEntry::new("key-g", grouped, 1));
        snap.apikeys.insert(apikey_entry("sk-caller", &["*"]));
        let app = build_app(snap);
        let get = |key: &'static str, uri: String| {
            let app = app.clone();
            async move {
                let req = Request::builder()
                    .method("GET")
                    .uri(uri)
                    .header("authorization", format!("Bearer {key}"))
                    .body(axum::body::Body::empty())
                    .unwrap();
                let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
                let status = resp.status();
                let body = to_bytes(resp.into_body(), 65536).await.unwrap();
                (status, String::from_utf8(body.to_vec()).unwrap())
            }
        };
        let ids = |body: &str| -> Vec<String> {
            serde_json::from_str::<serde_json::Value>(body).unwrap()["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["id"].as_str().unwrap().to_string())
                .collect()
        };

        let (status, body) = get("sk-grouped", "/v1/models".into()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(ids(&body), ["gpt4"]);
        let ensemble = get("sk-grouped", "/v1/models/panel".into()).await;
        let missing = get("sk-grouped", "/v1/models/nothing-here".into()).await;
        assert_eq!(ensemble.0, StatusCode::NOT_FOUND);
        assert_eq!(
            ensemble.1.replace("panel", "X"),
            missing.1.replace("nothing-here", "X"),
            "an ensemble reads as a model that does not exist"
        );
        assert_eq!(
            get("sk-grouped", "/v1/models/gpt4".into()).await.0,
            StatusCode::OK
        );

        // Without groups the ensemble is listed and retrieved, as before.
        let (_, body) = get("sk-caller", "/v1/models".into()).await;
        assert_eq!(ids(&body), ["gpt4", "panel"]);
        assert_eq!(
            get("sk-caller", "/v1/models/panel".into()).await.0,
            StatusCode::OK
        );
    }

    // ── prices, in platform mode ────────────────────────────────────────────

    /// Two models' resource ids, as the control plane names them.
    const M1: &str = "0b3e8a2c-5d41-4f6e-9a7b-1c2d3e4f5a61";
    const M2: &str = "0b3e8a2c-5d41-4f6e-9a7b-1c2d3e4f5a62";

    /// A key of `team` (none for a key of no team) in provider `groups`,
    /// seeing every model.
    fn tenant_key(
        id: &str,
        plaintext: &str,
        team: Option<&str>,
        groups: &[&str],
    ) -> ResourceEntry<ApiKey> {
        let key: ApiKey = serde_json::from_value(json!({
            "key_hash": ApiKey::hash_bearer(plaintext),
            "allowed_models": ["*"],
            "team_id": team,
            "allowed_provider_groups": groups,
        }))
        .unwrap();
        ResourceEntry::new(id, key, 1)
    }

    /// `chat-1` (M1) and `chat-2` (M2); two keys of tenant A in `grp-a`, a
    /// third in `grp-b`, one of tenant B, and two keys in no team.
    fn priced_snap() -> AisixSnapshot {
        let snap = new_snap();
        snap.models.insert(model_entry(M1, "chat-1"));
        snap.models.insert(model_entry(M2, "chat-2"));
        for (id, plaintext, team, groups) in [
            ("key-a1", "sk-a1", Some("tenant-a"), ["grp-a"]),
            ("key-a2", "sk-a2", Some("tenant-a"), ["grp-a"]),
            ("key-a3", "sk-a3", Some("tenant-a"), ["grp-b"]),
            ("key-b1", "sk-b1", Some("tenant-b"), ["grp-a"]),
            ("key-solo1", "sk-solo1", None, ["grp-a"]),
            ("key-solo2", "sk-solo2", None, ["grp-a"]),
        ] {
            snap.apikeys
                .insert(tenant_key(id, plaintext, team, &groups));
        }
        snap
    }

    fn app_with(platform: bool, account: Option<AccountClient>) -> Router {
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let handle = SnapshotHandle::new(priced_snap());
        let mut state = crate::ProxyState::new(handle, hub, &cfg()).without_cache();
        if platform {
            state = state.with_platform(crate::platform::PlatformSettings::from_secret("s"));
        }
        if let Some(account) = account {
            state = state.with_account(Arc::new(account));
        }
        crate::build_router(state)
    }

    struct Answer {
        status: StatusCode,
        /// Every header but the request id, which is new on every request.
        headers: HeaderMap,
        body: Vec<u8>,
    }

    impl Answer {
        fn json(&self) -> Value {
            serde_json::from_slice(&self.body).unwrap()
        }
    }

    async fn get(app: &Router, uri: &str, bearer: &str, anthropic: bool) -> Answer {
        let mut req = Request::builder()
            .method("GET")
            .uri(uri)
            .header("authorization", format!("Bearer {bearer}"));
        if anthropic {
            req = req.header("anthropic-version", "2023-06-01");
        }
        let req = req.body(axum::body::Body::empty()).unwrap();
        let resp = tower::ServiceExt::oneshot(app.clone(), req).await.unwrap();
        let status = resp.status();
        let mut headers = resp.headers().clone();
        headers.remove("x-aisix-request-id");
        let body = to_bytes(resp.into_body(), 65536).await.unwrap().to_vec();
        Answer {
            status,
            headers,
            body,
        }
    }

    fn ok(body: Value) -> (StatusCode, String) {
        (StatusCode::OK, body.to_string())
    }

    /// Tenant A's prices: M1 only.
    fn m1_priced() -> Value {
        json!({
            "tenant_id": "tenant-a",
            "pricing_version": 7,
            "currency": "CNY",
            "valid_until": null,
            "items": [{"model_id": M1, "input": "0.000006", "output": "0.00002"}],
        })
    }

    /// The listed model `id` of a listing in either shape.
    fn listed<'a>(listing: &'a Value, id: &str) -> &'a Value {
        listing["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == id)
            .unwrap_or_else(|| panic!("{id} is not listed: {listing}"))
    }

    #[tokio::test]
    async fn the_listing_without_platform_mode_is_unchanged() {
        let stand_in = StandIn::start(Arc::new(|_| ok(m1_priced()))).await;
        let with = app_with(false, Some(stand_in.client()));
        let without = app_with(false, None);
        for (uri, anthropic) in [
            ("/v1/models", false),
            ("/v1/models", true),
            ("/v1/models/chat-1", false),
            ("/v1/models/chat-1", true),
        ] {
            let a = get(&with, uri, "sk-a1", anthropic).await;
            let b = get(&without, uri, "sk-a1", anthropic).await;
            assert_eq!(a.status, StatusCode::OK, "{uri}");
            assert_eq!(a.status, b.status, "{uri}");
            assert_eq!(a.headers, b.headers, "{uri}");
            assert_eq!(a.body, b.body, "{uri}");
            let text = String::from_utf8(a.body).unwrap();
            assert!(!text.contains("pricing"), "{uri}: {text}");
        }
        assert_eq!(stand_in.hits(), 0, "nothing is asked off platform mode");
    }

    #[tokio::test]
    async fn a_platform_listing_carries_each_models_price() {
        let stand_in = StandIn::start(Arc::new(|_| ok(m1_priced()))).await;
        let app = app_with(true, Some(stand_in.client()));
        let answer = get(&app, "/v1/models", "sk-a1", false).await;
        assert_eq!(answer.status, StatusCode::OK);
        let mut listing = answer.json();
        assert_eq!(
            listed(&listing, "chat-1")["pricing"],
            json!({"currency": "CNY", "input": "0.000006", "output": "0.00002"})
        );
        assert!(
            listed(&listing, "chat-2").get("pricing").is_none(),
            "{listing}"
        );

        // Everything else is what a platform listing without prices says.
        let plain = get(&app_with(true, None), "/v1/models", "sk-a1", false).await;
        assert_eq!(answer.headers, plain.headers);
        for model in listing["data"].as_array_mut().unwrap() {
            model.as_object_mut().unwrap().remove("pricing");
        }
        assert_eq!(listing, plain.json());

        // The Anthropic shape carries the same price.
        let anthropic = get(&app, "/v1/models", "sk-a1", true).await.json();
        assert_eq!(
            listed(&anthropic, "chat-1")["pricing"],
            json!({"currency": "CNY", "input": "0.000006", "output": "0.00002"})
        );
        assert!(listed(&anthropic, "chat-2").get("pricing").is_none());

        // Asked once, with the caller's own key.
        let paths = stand_in.paths.lock().unwrap().clone();
        assert_eq!(
            paths,
            HashMap::from([("/dp/v1/keys/key-a1/prices".to_string(), 1)])
        );
    }

    /// Each tenant is asked with its own key and shown only its own
    /// prices, a failure included, and a key in no team is a tenant of its
    /// own.
    #[tokio::test]
    async fn each_tenant_is_shown_only_its_own_prices() {
        let stand_in = StandIn::start(Arc::new(|key| {
            let input = match key {
                "key-a1" | "key-a2" => "0.000006",
                "key-b1" => "0.000003",
                "key-solo1" => "0.000009",
                // The control plane fails for key-solo2.
                _ => return (StatusCode::SERVICE_UNAVAILABLE, "{}".to_string()),
            };
            ok(json!({
                "currency": "CNY",
                "items": [{"model_id": M1, "input": input, "output": "0.00002"}],
            }))
        }))
        .await;
        let app = app_with(true, Some(stand_in.client()));
        for (key, expected) in [
            ("sk-a1", Some("0.000006")),
            ("sk-solo2", None),
            ("sk-solo1", Some("0.000009")),
            ("sk-b1", Some("0.000003")),
            ("sk-a2", Some("0.000006")),
        ] {
            let listing = get(&app, "/v1/models", key, false).await.json();
            let chat = listed(&listing, "chat-1");
            match expected {
                Some(input) => assert_eq!(chat["pricing"]["input"], input, "{key}: {listing}"),
                None => assert!(chat.get("pricing").is_none(), "{key}: {listing}"),
            }
        }
        // Each tenant was asked once, with its own key; A's second key was
        // answered from A's entry.
        let paths = stand_in.paths.lock().unwrap().clone();
        assert_eq!(
            paths,
            HashMap::from([
                ("/dp/v1/keys/key-a1/prices".to_string(), 1),
                ("/dp/v1/keys/key-solo2/prices".to_string(), 1),
                ("/dp/v1/keys/key-solo1/prices".to_string(), 1),
                ("/dp/v1/keys/key-b1/prices".to_string(), 1),
            ])
        );
    }

    /// The prices cache is keyed by the tenant **and** the key document's
    /// provider groups, not by the tenant alone: two keys of one tenant in
    /// two groups are shown their own groups' prices, and two keys in one
    /// group share one fetch.
    #[tokio::test]
    async fn a_second_group_of_one_tenant_is_shown_its_own_prices() {
        // Each group's price, by the key that asked.
        let stand_in = StandIn::start(Arc::new(|key| {
            let (input, output) = match key {
                // Tenant A's key in `grp-a`, and the same tenant's in `grp-b`.
                "key-a1" | "key-a2" => ("0.000006", "0.00002"),
                "key-a3" => ("0.000003", "0.00001"),
                _ => ("0.000009", "0.00003"),
            };
            ok(json!({
                "currency": "CNY",
                "items": [{"model_id": M1, "input": input, "output": output}],
            }))
        }))
        .await;
        let app = app_with(true, Some(stand_in.client()));

        for (key, input, output) in [
            ("sk-a1", "0.000006", "0.00002"),
            ("sk-a3", "0.000003", "0.00001"),
            // A's other key of the first group is served from that group's
            // entry, never the second group's.
            ("sk-a2", "0.000006", "0.00002"),
        ] {
            let listing = get(&app, "/v1/models", key, false).await.json();
            let price = &listed(&listing, "chat-1")["pricing"];
            assert_eq!(price["currency"], "CNY", "{key}: {listing}");
            assert_eq!(price["input"], input, "{key}: {listing}");
            assert_eq!(price["output"], output, "{key}: {listing}");
        }
        // Two groups of one tenant, two fetches; the second key of the first
        // group asked nothing.
        let paths = stand_in.paths.lock().unwrap().clone();
        assert_eq!(
            paths,
            HashMap::from([
                ("/dp/v1/keys/key-a1/prices".to_string(), 1),
                ("/dp/v1/keys/key-a3/prices".to_string(), 1),
            ]),
            "one fetch per group, one per tenant-and-group entry"
        );
    }

    #[tokio::test]
    async fn a_slow_control_plane_never_holds_the_listing() {
        let stand_in =
            StandIn::start_with_delay(Arc::new(|_| ok(m1_priced())), Duration::from_secs(3)).await;
        let app = app_with(true, Some(stand_in.client()));

        let started = Instant::now();
        let answer = get(&app, "/v1/models", "sk-a1", false).await;
        let waited = started.elapsed();
        assert_eq!(answer.status, StatusCode::OK);
        assert!(
            waited < PRICES_TIMEOUT + Duration::from_millis(200),
            "the listing waited {waited:?}"
        );
        let listing = answer.json();
        assert_eq!(listing["data"].as_array().unwrap().len(), 2);
        assert!(
            !String::from_utf8(answer.body).unwrap().contains("pricing"),
            "{listing}"
        );
        assert_eq!(stand_in.hits(), 1);

        // Within PRICES_FAILURE_TTL the tenant is listed without asking.
        let started = Instant::now();
        let again = get(&app, "/v1/models", "sk-a2", false).await;
        assert_eq!(again.status, StatusCode::OK);
        assert!(started.elapsed() < PRICES_FAILURE_TTL);
        assert!(!String::from_utf8(again.body).unwrap().contains("pricing"));
        assert_eq!(
            stand_in.hits(),
            1,
            "a second listing within 2 s asks nothing"
        );
    }

    #[tokio::test]
    async fn prices_are_cached_per_tenant_until_valid_until() {
        // Every answer says the prices change one second from now.
        let stand_in = StandIn::start(Arc::new(|_| {
            let mut body = m1_priced();
            body["valid_until"] = json!(chrono::Utc::now() + chrono::Duration::seconds(1));
            ok(body)
        }))
        .await;
        let app = app_with(true, Some(stand_in.client()));
        for key in ["sk-a1", "sk-a2"] {
            let listing = get(&app, "/v1/models", key, false).await.json();
            assert_eq!(listed(&listing, "chat-1")["pricing"]["input"], "0.000006");
        }
        assert_eq!(stand_in.hits(), 1, "two keys of one tenant share its entry");

        tokio::time::sleep(Duration::from_millis(1_200)).await;
        let listing = get(&app, "/v1/models", "sk-a2", false).await.json();
        assert_eq!(listed(&listing, "chat-1")["pricing"]["input"], "0.000006");
        assert_eq!(
            stand_in.hits(),
            2,
            "past valid_until the tenant is asked again"
        );
    }

    #[tokio::test]
    async fn the_single_model_route_carries_the_same_pricing() {
        let stand_in = StandIn::start(Arc::new(|_| {
            ok(json!({
                "currency": "CNY",
                "items": [{
                    "model_id": M1,
                    "input": "0.000006",
                    "output": "0.00002",
                    "input_cache_read": "0.0000006",
                    "input_cache_write": "0.0000075",
                    "input_cache_write_1h": "0.000012",
                }],
            }))
        }))
        .await;
        let app = app_with(true, Some(stand_in.client()));
        let listing = get(&app, "/v1/models", "sk-a1", false).await.json();
        let expected = json!({
            "currency": "CNY",
            "input": "0.000006",
            "output": "0.00002",
            "input_cache_read": "0.0000006",
            "input_cache_write": "0.0000075",
            "input_cache_write_1h": "0.000012",
        });
        assert_eq!(listed(&listing, "chat-1")["pricing"], expected);

        for anthropic in [false, true] {
            let one = get(&app, "/v1/models/chat-1", "sk-a1", anthropic).await;
            assert_eq!(one.status, StatusCode::OK);
            assert_eq!(one.json()["pricing"], expected, "anthropic: {anthropic}");
            let other = get(&app, "/v1/models/chat-2", "sk-a1", anthropic).await;
            assert_eq!(other.status, StatusCode::OK);
            assert!(
                other.json().get("pricing").is_none(),
                "anthropic: {anthropic}"
            );
        }
    }
}
